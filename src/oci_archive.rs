use std::fs::File;
use std::io::{Cursor, Write};

use anyhow::{Context, Result};
use serde::Serialize;
use tar::{Builder, Header};

use crate::image_ref::ImageReference;
use crate::registry::PullPlan;
use crate::store::BlobStore;

pub fn write_oci_archive<W: Write>(
    writer: W,
    image: &ImageReference,
    plan: &PullPlan,
    store: &BlobStore,
) -> Result<()> {
    let mut builder = Builder::new(writer);

    append_bytes(
        &mut builder,
        "oci-layout",
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )?;

    let index = OciIndex {
        schema_version: 2,
        manifests: vec![IndexEntry {
            media_type: plan.manifest_descriptor.media_type.clone(),
            digest: plan.manifest_descriptor.digest.clone(),
            size: plan.manifest_descriptor.size,
            annotations: ImageAnnotations {
                ref_name: image.display_reference(),
            },
        }],
    };
    let index_bytes = serde_json::to_vec_pretty(&index)?;
    append_bytes(&mut builder, "index.json", &index_bytes)?;

    append_bytes(
        &mut builder,
        &blob_archive_path(&plan.manifest_descriptor.digest)?,
        &plan.manifest_bytes,
    )?;

    append_blob_from_store(&mut builder, store, &plan.manifest.config.digest)?;
    for layer in &plan.manifest.layers {
        append_blob_from_store(&mut builder, store, &layer.digest)?;
    }

    builder
        .finish()
        .context("failed to finish OCI archive")?;
    Ok(())
}

fn append_blob_from_store<W: Write>(
    builder: &mut Builder<W>,
    store: &BlobStore,
    digest: &str,
) -> Result<()> {
    let path = store.blob_path(digest)?;
    let archive_path = blob_archive_path(digest)?;
    let mut file =
        File::open(&path).with_context(|| format!("failed to open blob {}", path.display()))?;
    builder
        .append_file(&archive_path, &mut file)
        .with_context(|| format!("failed to append blob {}", path.display()))?;
    Ok(())
}

fn append_bytes<W: Write>(builder: &mut Builder<W>, path: &str, bytes: &[u8]) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();

    builder
        .append_data(&mut header, path, Cursor::new(bytes))
        .with_context(|| format!("failed to append archive entry {path}"))?;
    Ok(())
}

fn blob_archive_path(digest: &str) -> Result<String> {
    let (algorithm, encoded) = split_digest(digest)?;
    Ok(format!("blobs/{algorithm}/{encoded}"))
}

fn split_digest(digest: &str) -> Result<(&str, &str)> {
    let mut parts = digest.splitn(2, ':');
    let algorithm = parts.next().unwrap_or_default();
    let encoded = parts.next().unwrap_or_default();

    anyhow::ensure!(
        !algorithm.is_empty() && !encoded.is_empty(),
        "invalid digest: {digest}"
    );

    Ok((algorithm, encoded))
}

#[derive(Serialize)]
struct OciIndex {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    manifests: Vec<IndexEntry>,
}

#[derive(Serialize)]
struct IndexEntry {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    annotations: ImageAnnotations,
}

#[derive(Serialize)]
struct ImageAnnotations {
    #[serde(rename = "org.opencontainers.image.ref.name")]
    ref_name: String,
}
