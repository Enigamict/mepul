mod docker_engine;
mod image_ref;
mod oci_archive;
mod registry;
mod store;
mod types;
mod download;

use clap::Parser;

use anyhow::{Context, Result};
use image_ref::ImageReference;
use oci_archive::write_oci_archive;
use registry::{PlatformSpec, RegistryClient};
use store::resolve_blobs;


#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
   image:String,
    #[arg(default_value_t = String::from("/var/run/docker.sock"))]
   sock:String,
}


#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    
    let image = ImageReference::parse(&args.image)?;
    let client = RegistryClient::new()?;
    let platform = PlatformSpec::host_default();

    println!(
        "pulling {}/{}:{} ({}/{})",
        image.registry, image.repository, image.reference, platform.os, platform.arch
    );

    let pull = client.pull(&image, &platform).await?;
    println!("manifest: {}", pull.manifest.digest);

    let (config, layers) = resolve_blobs(&client, &image, &pull).await?;

    println!("loading into Docker image store...");
    docker_engine::load_archive(&args.sock, |archive| {
        write_oci_archive(archive, &image, &pull, &config, &layers)
    })
    .with_context(|| "failed to load archive into Docker image store")?;

    println!("done. loaded into Docker image store");
    Ok(())
}

