use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::fs;

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
