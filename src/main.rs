mod docker_engine;
mod image_ref;
mod oci_archive;
mod registry;
mod store;
mod types;

use clap::Parser;
use mimalloc::MiMalloc;
use anyhow::{Context, Result};
use image_ref::ImageReference;
use oci_archive::write_oci_archive;
use registry::{PlatformSpec, RegistryClient};
use store::resolve_blobs;

use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;
use tracing_opentelemetry::OpenTelemetryLayer;



#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;


#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
   image:String,
    #[arg(default_value_t = String::from("/var/run/docker.sock"))]
   sock:String,
}

fn init_tracing() -> SdkTracerProvider {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .expect("failed to create OTLP span exporter");

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();

    let tracer = provider.tracer("mepul");
    let otel_layer = OpenTelemetryLayer::new(tracer);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("mepul=trace"));

    tracing_subscriber::registry()
        .with(filter)
        .with(otel_layer)
        .with(tracing_subscriber::fmt::layer().compact())
        .init();

    provider
}

#[tokio::main]
async fn main() -> Result<()> {
    let provider = init_tracing();

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

    provider.shutdown().ok();
    Ok(())
}

