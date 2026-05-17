use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::fs;

use crate::image_ref::ImageReference;
use crate::registry::{PullPlan, RegistryClient};
use crate::types::Descriptor;

pub struct BlobStore {
    root: PathBuf,
    cleanup_on_drop: bool,
}

impl BlobStore {
    pub async fn temporary() -> Result<Self> {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before UNIX epoch")?
            .as_nanos();

        for attempt in 0..100 {
            let root = base.join(format!("mepul-{pid}-{now}-{attempt}"));
            match fs::create_dir(&root).await {
                Ok(()) => {
                    fs::create_dir_all(root.join("blobs")).await?;
                    fs::create_dir_all(root.join("images")).await?;
                    fs::create_dir_all(root.join("manifests")).await?;
                    return Ok(Self {
                        root,
                        cleanup_on_drop: true,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create temporary store {}", root.display())
                    });
                }
            }
        }

        bail!("failed to create a unique temporary store directory");
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blob_path(&self, digest: &str) -> Result<PathBuf> {
        let (algorithm, encoded) = split_digest(digest)?;
        Ok(self.root.join("blobs").join(algorithm).join(encoded))
    }

    pub async fn contains_blob(&self, digest: &str) -> Result<bool> {
        Ok(fs::try_exists(self.blob_path(digest)?).await?)
    }

    pub async fn write_blob_verified(&self, digest: &str, bytes: &[u8]) -> Result<PathBuf> {
        verify_digest(digest, bytes)?;
        self.write_content(digest, bytes).await
    }

    pub async fn write_content_verified(&self, digest: &str, bytes: &[u8]) -> Result<PathBuf> {
        verify_digest(digest, bytes)?;
        self.write_content(digest, bytes).await
    }

    pub async fn write_image_record(
        &self,
        image_ref: &str,
        target: &Descriptor,
        manifest_digest: &str,
        os: &str,
        arch: &str,
    ) -> Result<PathBuf> {
        let image_path = self
            .root
            .join("images")
            .join(format!("{}.json", sanitize(image_ref)));
        let record = ImageRecord {
            name: image_ref.to_string(),
            target: target.clone(),
            manifest_digest: manifest_digest.to_string(),
            platform: ImagePlatform {
                os: os.to_string(),
                architecture: arch.to_string(),
            },
        };
        let bytes = serde_json::to_vec_pretty(&record)?;
        fs::write(&image_path, bytes)
            .await
            .with_context(|| format!("failed to write image record {}", image_path.display()))?;
        Ok(image_path)
    }

    async fn write_content(&self, digest: &str, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.blob_path(digest)?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        fs::write(&path, bytes)
            .await
            .with_context(|| format!("failed to write blob to {}", path.display()))?;
        Ok(path)
    }

    pub async fn write_manifest_reference(
        &self,
        image_ref: &str,
        manifest_digest: &str,
        bytes: &[u8],
    ) -> Result<PathBuf> {
        let manifest_path = self.root.join("manifests").join(sanitize(image_ref));
        fs::write(&manifest_path, bytes).await?;

        let digest_path = self
            .root
            .join("manifests")
            .join(format!("{}.digest", sanitize(image_ref)));
        fs::write(&digest_path, manifest_digest.as_bytes()).await?;
        Ok(manifest_path)
    }
}

impl Drop for BlobStore {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn split_digest(digest: &str) -> Result<(&str, &str)> {
    let mut parts = digest.splitn(2, ':');
    let algorithm = parts.next().unwrap_or_default();
    let encoded = parts.next().unwrap_or_default();

    if algorithm.is_empty() || encoded.is_empty() {
        bail!("invalid digest: {digest}");
    }

    Ok((algorithm, encoded))
}

fn sanitize(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

fn verify_digest(expected: &str, bytes: &[u8]) -> Result<()> {
    let (algorithm, encoded) = split_digest(expected)?;
    if algorithm != "sha256" {
        bail!("unsupported digest algorithm: {algorithm}");
    }

    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != encoded {
        bail!("digest mismatch: expected {expected}, got sha256:{actual}");
    }

    Ok(())
}

#[derive(Serialize)]
struct ImageRecord {
    name: String,
    target: Descriptor,
    manifest_digest: String,
    platform: ImagePlatform,
}

#[derive(Serialize)]
struct ImagePlatform {
    os: String,
    architecture: String,
}

pub async fn download_blobs(
    client: &RegistryClient,
    store: &Arc<BlobStore>,
    image: &ImageReference,
    pull: &PullPlan,
) -> Result<()> {
    let mut set = tokio::task::JoinSet::new();

    let tasks = std::iter::once((&pull.manifest.config, "config".to_string())).chain(
        pull.manifest
            .layers
            .iter()
            .enumerate()
            .map(|(i, l)| (l, format!("layer {}", i + 1))),
    );

    for (descriptor, label) in tasks {
        let client = client.clone();
        let store = Arc::clone(store);
        let image = image.clone();
        let descriptor = descriptor.clone();
        set.spawn(async move {
            download_and_store(&client, &store, &image, &descriptor, &label).await
        });
    }

    while let Some(result) = set.join_next().await {
        result.context("download task panicked")??;
    }

    Ok(())
}

async fn download_and_store(
    client: &RegistryClient,
    store: &BlobStore,
    image: &ImageReference,
    descriptor: &Descriptor,
    label: &str,
) -> Result<()> {
    if store.contains_blob(&descriptor.digest).await? {
        println!("{label}: cached {}", descriptor.digest);
        return Ok(());
    }

    println!("{label}: downloading {}", descriptor.digest);
    let bytes = client
        .fetch_blob(image, &descriptor.digest)
        .await
        .with_context(|| format!("failed to download {label}"))?;

    if bytes.len() as u64 != descriptor.size {
        anyhow::bail!(
            "size mismatch for {}: expected {}, got {}",
            descriptor.digest,
            descriptor.size,
            bytes.len()
        );
    }

    let path = store
        .write_blob_verified(&descriptor.digest, &bytes)
        .await
        .with_context(|| format!("failed to store {label}"))?;
    println!("{label}: saved {}", path.display());
    Ok(())
}




#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_digest_success() {
        let data = b"hello world";
        let digest = "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert!(verify_digest(digest, data).is_ok());
    }

    #[test]
    fn test_verify_digest_failure() {
        let data = b"hello world!";
        let digest = "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert!(verify_digest(digest, data).is_err());
    }

    #[test]
    fn test_verify_split_digest_success() {
        let digest = "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert_eq!(
            split_digest(digest).unwrap(),
            (
                "sha256",
                "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
            )
        )
    }

    #[test]
    fn test_verify_split_digest_failure() {
        let digest = "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert_ne!(
            split_digest(digest).unwrap(),
            (
                "sha256",
                "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
            )
        )
    }
    
}
