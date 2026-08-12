mod docker_engine;
mod filecache;
mod image_ref;
mod oci_archive;
mod profile;
mod registry;
mod rootfs;
mod run;
mod store;
mod trace;
mod types;

use clap::{Parser, Subcommand};

use anyhow::{Context, Result};
use image_ref::ImageReference;
use oci_archive::write_oci_archive;
use profile::{profile_path, Profile};
use registry::{PlatformSpec, RegistryClient};
use run::{RunOptions, RunReport};
use store::resolve_blobs;

#[derive(Parser, Debug)]
#[command(version, about = "Profile-driven image delivery for ephemeral CI", long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Pull an image into the Docker image store.
    Pull {
        image: String,
        #[arg(long, default_value = "/var/run/docker.sock")]
        sock: String,
    },
    /// Run a command on a disposable root filesystem, observing what it opens.
    ///
    /// The first run for a pipeline pulls the image and learns; later runs
    /// assemble only the observed files out of the cache.
    Run {
        /// Image the pipeline runs on, e.g. alpine:latest.
        image: String,
        /// Identity the profile is stored under. The same pipeline run again is
        /// what makes the cache pay off.
        #[arg(long)]
        pipeline: String,
        /// Ignore the profile and pull the image. Use this to measure a cold
        /// start without discarding what has been learned.
        #[arg(long)]
        cold: bool,
        /// Skip eBPF observation. The profile stops improving.
        #[arg(long)]
        no_trace: bool,
        /// Report every observed path, what became of it, and what the run
        /// added to the profile and the cache.
        #[arg(long, short)]
        verbose: bool,
        /// Command to run inside the image, after `--`.
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// Show what a pipeline's profile has learned.
    Profile {
        pipeline: String,
        /// List every recorded path.
        #[arg(long)]
        paths: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.command {
        Command::Pull { image, sock } => pull(&image, &sock).await,
        Command::Run {
            image,
            pipeline,
            cold,
            no_trace,
            verbose,
            argv,
        } => {
            let options = RunOptions {
                image,
                pipeline,
                command: argv,
                force_cold: cold,
                no_trace,
                verbose,
            };
            let report = run::run(&options).await?;
            print_report(&report);
            if report.exit_code != 0 {
                std::process::exit(report.exit_code);
            }
            Ok(())
        }
        Command::Profile { pipeline, paths } => show_profile(&pipeline, paths),
    }
}

async fn pull(image: &str, sock: &str) -> Result<()> {
    let image = ImageReference::parse(image)?;
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
    docker_engine::load_archive(sock, |archive| {
        write_oci_archive(archive, &image, &pull, &config, &layers)
    })
    .with_context(|| "failed to load archive into Docker image store")?;

    println!("done. loaded into Docker image store");
    Ok(())
}

fn print_report(report: &RunReport) {
    println!("---");
    println!("mode:     {}", report.mode.label());
    if report.fell_back {
        println!("fallback: warm run failed, redone cold");
    }
    println!(
        "prepare:  {:.3}s (rootfs ready)",
        report.prepare.as_secs_f64()
    );
    println!("execute:  {:.3}s (job)", report.execute.as_secs_f64());
    println!(
        "total:    {:.3}s",
        (report.prepare + report.execute).as_secs_f64()
    );
    println!(
        "observed: {} path(s), {} recorded",
        report.observed, report.recorded
    );
    println!("entries:  {}", report.profile_entries);
    println!("exit:     {}", report.exit_code);
}

fn show_profile(pipeline: &str, list_paths: bool) -> Result<()> {
    let path = profile_path(pipeline)?;
    let Some(profile) = Profile::load(&path)? else {
        println!("no profile for {pipeline} yet ({})", path.display());
        return Ok(());
    };

    println!("pipeline: {}", profile.pipeline);
    println!("image:    {}", profile.image);
    println!("runs:     {}", profile.runs);
    println!(
        "entries:  {} ({} files)",
        profile.len(),
        profile.file_count()
    );
    println!("path:     {}", path.display());

    if list_paths {
        for (entry_path, entry) in &profile.entries {
            let kind = match entry {
                profile::Entry::Dir { .. } => "dir",
                profile::Entry::File { .. } => "file",
                profile::Entry::Symlink { .. } => "link",
            };
            println!("  {kind:<5} {entry_path}");
        }
    }
    Ok(())
}
