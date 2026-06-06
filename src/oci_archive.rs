use std::fs::File;
use std::io::{Cursor, Write};

use anyhow::{Context, Result};
use serde::Serialize;
use tar::{Builder, Header};

use crate::image_ref::ImageReference;
use crate::registry::PullPlan;
use crate::store::{BlobSource, ResolvedBlob};

pub fn write_oci_archive<W: Write>(
    writer: W,
    image: &ImageReference,
    plan: &PullPlan,
    config: &ResolvedBlob,
    layers: &[ResolvedBlob],
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

    append_blob(&mut builder, config)?;
    for layer in layers {
        append_blob(&mut builder, layer)?;
    }

    builder.finish().context("failed to finish OCI archive")?;
    Ok(())
}

fn append_blob<W: Write>(builder: &mut Builder<W>, blob: &ResolvedBlob) -> Result<()> {
    let archive_path = blob_archive_path(&blob.digest)?;
    match &blob.source {
        BlobSource::CachedFile(path) => {
            let mut file = File::open(path)
                .with_context(|| format!("failed to open cached blob {}", path.display()))?;
            builder
                .append_file(&archive_path, &mut file)
                .with_context(|| format!("failed to append cached blob {}", path.display()))?;
        }
        BlobSource::Downloaded(bytes) => {
            append_bytes(builder, &archive_path, bytes)?;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::write_oci_archive;
    use crate::image_ref::ImageReference;
    use crate::registry::{PullPlan, ResolvedManifest};
    use crate::store::{BlobSource, ResolvedBlob};
    use crate::types::Descriptor;

    fn descriptor(media_type: &str, digest: &str, size: u64) -> Descriptor {
        Descriptor {
            media_type: media_type.to_string(),
            digest: digest.to_string(),
            size,
        }
    }

    fn test_make_plan(manifest_bytes: &[u8]) -> PullPlan {
        PullPlan {
            manifest_descriptor: descriptor(
                "application/vnd.oci.image.manifest.v1+json",
                "sha256:aaaa",
                manifest_bytes.len() as u64,
            ),
            manifest_bytes: manifest_bytes.to_vec(),
            manifest: ResolvedManifest {
                digest: "sha256:aaaa".to_string(),
                raw_bytes: manifest_bytes.to_vec(),
                config: descriptor("application/vnd.oci.image.config.v1+json", "sha256:bbbb", 2),
                layers: vec![descriptor(
                    "application/vnd.oci.image.layer.v1.tar+gzip",
                    "sha256:cccc",
                    5,
                )],
            },
        }
    }

    fn tar_entry_content(tar_bytes: &[u8], name: &str) -> Option<Vec<u8>> {
        let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().to_string_lossy() == name {
                let mut content = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut content).unwrap();
                return Some(content);
            }
        }
        None
    }

    fn tar_entry_paths(tar_bytes: &[u8]) -> Vec<String> {
        let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
        archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn write_oci_archive_produces_valid_tar_with_required_entries() {
        let image = ImageReference::parse("ubuntu:24.04").unwrap();
        let plan = test_make_plan(b"{}");
        let config = ResolvedBlob {
            digest: "sha256:bbbb".to_string(),
            source: BlobSource::Downloaded(b"{}".to_vec()),
        };
        let layer = ResolvedBlob {
            digest: "sha256:cccc".to_string(),
            source: BlobSource::Downloaded(b"layer".to_vec()),
        };

        let mut output = Vec::new();
        write_oci_archive(&mut output, &image, &plan, &config, &[layer]).unwrap();

        let paths = tar_entry_paths(&output);
        assert!(paths.contains(&"oci-layout".to_string()));
        assert!(paths.contains(&"index.json".to_string()));
        assert!(paths.contains(&"blobs/sha256/aaaa".to_string()));
        assert!(paths.contains(&"blobs/sha256/bbbb".to_string()));
        assert!(paths.contains(&"blobs/sha256/cccc".to_string()));
    }

    #[test]
    fn write_oci_archive_oci_layout_has_correct_content() {
        let image = ImageReference::parse("ubuntu:24.04").unwrap();
        let plan = test_make_plan(b"{}");
        let config = ResolvedBlob {
            digest: "sha256:bbbb".to_string(),
            source: BlobSource::Downloaded(b"{}".to_vec()),
        };

        let mut output = Vec::new();
        write_oci_archive(&mut output, &image, &plan, &config, &[]).unwrap();

        let content = tar_entry_content(&output, "oci-layout").expect("oci-layout not found");
        let v: serde_json::Value = serde_json::from_slice(&content).unwrap();
        assert_eq!(v["imageLayoutVersion"], "1.0.0");
    }

    #[test]
    fn write_oci_archive_index_contains_image_ref_annotation() {
        let image = ImageReference::parse("ghcr.io/example/app:v1").unwrap();
        let plan = test_make_plan(b"{}");
        let config = ResolvedBlob {
            digest: "sha256:bbbb".to_string(),
            source: BlobSource::Downloaded(b"{}".to_vec()),
        };

        let mut output = Vec::new();
        write_oci_archive(&mut output, &image, &plan, &config, &[]).unwrap();

        let content = tar_entry_content(&output, "index.json").expect("index.json not found");
        let v: serde_json::Value = serde_json::from_slice(&content).unwrap();
        let ref_name = &v["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"];
        assert_eq!(ref_name, "ghcr.io/example/app:v1");
    }

    #[test]
    fn blob_archive_path_errors_on_invalid_digest() {
        let image = ImageReference::parse("ubuntu:24.04").unwrap();
        let plan = PullPlan {
            manifest_descriptor: descriptor(
                "application/vnd.oci.image.manifest.v1+json",
                "invalid-no-colon",
                0,
            ),
            manifest_bytes: b"{}".to_vec(),
            manifest: ResolvedManifest {
                digest: "invalid-no-colon".to_string(),
                raw_bytes: b"{}".to_vec(),
                config: descriptor("", "sha256:bbbb", 0),
                layers: vec![],
            },
        };
        let config = ResolvedBlob {
            digest: "sha256:bbbb".to_string(),
            source: BlobSource::Downloaded(vec![]),
        };

        let mut output = Vec::new();
        let result = write_oci_archive(&mut output, &image, &plan, &config, &[]);

        assert!(result.is_err());
    }
}
