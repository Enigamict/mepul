mod docker_engine;
mod image_ref;
mod oci_archive;
mod registry;
mod store;
mod types;

use std::sync::Arc;

use anyhow::{Context, Result};
use image_ref::ImageReference;
use oci_archive::write_oci_archive;
use registry::{PlatformSpec, RegistryClient};
use store::{download_blobs, BlobStore};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: mepul <image>");
        std::process::exit(1);
    }

    let image = ImageReference::parse(&args[1])?;
    let store = Arc::new(BlobStore::temporary().await?);
    let client = RegistryClient::new()?;
    let platform = PlatformSpec::host_default();

    println!(
        "pulling {}/{}:{} ({}/{})",
        image.registry, image.repository, image.reference, platform.os, platform.arch
    );

    let pull = client.pull(&image, &platform).await?;
    println!("manifest: {}", pull.manifest.digest);

    let manifest_path = store
        .write_content_verified(&pull.manifest_descriptor.digest, &pull.manifest_bytes)
        .await
        .context("failed to store manifest")?;
    println!("manifest: saved {}", manifest_path.display());

    download_blobs(&client, &store, &image, &pull).await?;

    let reference_string = image.display_reference();
    store
        .write_manifest_reference(&reference_string, &pull.manifest.digest, &pull.manifest_bytes)
        .await?;
    let image_record_path = store
        .write_image_record(
            &reference_string,
            &pull.manifest_descriptor,
            &pull.manifest.digest,
            &platform.os,
            &platform.arch,
        )
        .await
        .context("failed to register image")?;
    println!("image: registered {}", image_record_path.display());

    println!("loading into Docker image store...");
    docker_engine::load_archive(|archive| write_oci_archive(archive, &image, &pull, &store))
        .with_context(|| format!("failed to load archive into Docker image store"))?;

    println!(
        "done. loaded into Docker image store; temporary store will be removed from {}",
        store.root().display()
    );
    Ok(())
}

