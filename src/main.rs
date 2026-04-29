mod docker_engine;
mod image_ref;
mod oci_archive;
mod registry;
mod store;
mod types;

use anyhow::{Context, Result};
use image_ref::ImageReference;
use oci_archive::write_oci_archive;
use registry::{PlatformSpec, RegistryClient};
use store::BlobStore;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: mepul <image>");
        std::process::exit(1);
    }

    let image = ImageReference::parse(&args[1])?;
    let store = BlobStore::temporary().await?;
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

    download_and_store(&client, &store, &image, &pull.manifest.config, "config").await?;
    for (i, layer) in pull.manifest.layers.iter().enumerate() {
        download_and_store(&client, &store, &image, layer, &format!("layer {}", i + 1)).await?;
    }

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

async fn download_and_store(
    client: &RegistryClient,
    store: &BlobStore,
    image: &ImageReference,
    descriptor: &types::Descriptor,
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
