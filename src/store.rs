use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;

use crate::image_ref::ImageReference;
use crate::registry::{PullPlan, RegistryClient};
use crate::types::Descriptor;

pub enum BlobSource {
    CachedFile(PathBuf),
    Downloaded(Vec<u8>),
}

pub struct ResolvedBlob {
    pub digest: String,
    pub source: BlobSource,
}

#[tracing::instrument(skip_all, fields(total_blobs = 1 + pull.manifest.layers.len()))]
pub async fn resolve_blobs(
    client: &RegistryClient,
    image: &ImageReference,
    pull: &PullPlan,
) -> Result<(ResolvedBlob, Vec<ResolvedBlob>)> {
    let total = 1 + pull.manifest.layers.len();
    let mut set: JoinSet<Result<(usize, ResolvedBlob)>> = JoinSet::new();

    {
        let client = client.clone();
        let image = image.clone();
        let descriptor = pull.manifest.config.clone();
        set.spawn(async move {
            let blob = resolve_blob(&client, &image, &descriptor, "config").await?;
            Ok((0, blob))
        });
    }

    for (i, descriptor) in pull.manifest.layers.iter().enumerate() {
        let client = client.clone();
        let image = image.clone();
        let descriptor = descriptor.clone();
        let label = format!("layer {}", i + 1);
        set.spawn(async move {
            let blob = resolve_blob(&client, &image, &descriptor, &label).await?;
            Ok((i + 1, blob))
        });
    }

    let mut results: Vec<Option<ResolvedBlob>> = (0..total).map(|_| None).collect();
    while let Some(result) = set.join_next().await {
        let (i, blob) = result.context("resolve task panicked")??;
        results[i] = Some(blob);
    }

    let mut iter = results.into_iter().map(Option::unwrap);
    let config = iter.next().unwrap();
    let layers = iter.collect();

    Ok((config, layers))
}

#[tracing::instrument(skip(client, image), fields(digest = %descriptor.digest, label = %label))]
async fn resolve_blob(
    client: &RegistryClient,
    image: &ImageReference,
    descriptor: &Descriptor,
    label: &str,
) -> Result<ResolvedBlob> {
    if let Some(path) = cache_blob_path(&descriptor.digest) {
        if path.exists() {
            println!("{label}: cached {}", descriptor.digest);
            return Ok(ResolvedBlob {
                digest: descriptor.digest.clone(),
                source: BlobSource::CachedFile(path),
            });
        }
    }

    println!("{label}: downloading {}", descriptor.digest);
    let bytes = client
        .fetch_blob(image, &descriptor.digest)
        .await
        .with_context(|| format!("failed to download {label}"))?;

    if bytes.len() as u64 != descriptor.size {
        bail!(
            "size mismatch for {}: expected {}, got {}",
            descriptor.digest,
            descriptor.size,
            bytes.len()
        );
    }

    verify_digest(&descriptor.digest, &bytes)?;

    if let Some(path) = cache_blob_path(&descriptor.digest) {
        if let Err(e) = save_to_cache(&path, &bytes).await {
            eprintln!("warning: failed to save cache for {}: {e}", descriptor.digest);
        }
    }

    Ok(ResolvedBlob {
        digest: descriptor.digest.clone(),
        source: BlobSource::Downloaded(bytes),
    })
}

fn cache_blob_path(digest: &str) -> Option<PathBuf> {
    let hash = digest.strip_prefix("sha256:")?;
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cache/mepul/blobs/sha256").join(hash))
}

#[tracing::instrument(skip(bytes), fields(path = %path.display()))]
async fn save_to_cache(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
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

fn verify_digest(expected: &str, bytes: &[u8]) -> Result<()> {
    let start = std::time::Instant::now();
    let (algorithm, encoded) = split_digest(expected)?;
    if algorithm != "sha256" {
        bail!("unsupported digest algorithm: {algorithm}");
    }
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != encoded {
        bail!("digest mismatch: expected {expected}, got sha256:{actual}");
    }
    let elapsed = start.elapsed();
    tracing::info!(digest = %expected, cpu_time_ms = elapsed.as_millis() as u64, bytes = bytes.len(), "digest verification complete");
    Ok(())
}
