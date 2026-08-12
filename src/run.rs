//! Orchestration: one CI job, observed, on a disposable root filesystem.
//!
//! The first run has no profile, so it pays the full pull and spends the run
//! learning. Every run after that assembles a root filesystem out of the file
//! cache and never talks to the registry. Both paths are observed, so the
//! profile keeps improving.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::filecache::{cache_home, FileCache};
use crate::image_ref::ImageReference;
use crate::profile::{profile_path, Entry, Profile};
use crate::registry::{PlatformSpec, RegistryClient};
use crate::rootfs::{build_from_profile, extract_layers, record_path, Disposable, Record};
use crate::store::resolve_blobs;
use crate::trace::{shell_quote, GatedJob, Tracer};

const CONTAINER_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub struct RunOptions {
    pub image: String,
    pub pipeline: String,
    pub command: Vec<String>,
    /// Ignore any existing profile and pull the image. What the second half of
    /// a two-run measurement must *not* do.
    pub force_cold: bool,
    /// Run without the tracer, for timing a build that is not paying for
    /// observation.
    pub no_trace: bool,
    /// Report what the observation actually produced, path by path.
    pub verbose: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No usable profile: pull the image and learn.
    Cold,
    /// Profile hit: assemble from the cache only.
    Warm,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Cold => "cold",
            Mode::Warm => "warm",
        }
    }
}

pub struct RunReport {
    pub mode: Mode,
    pub prepare: Duration,
    pub execute: Duration,
    pub exit_code: i32,
    pub observed: usize,
    pub recorded: usize,
    pub profile_entries: usize,
    pub fell_back: bool,
}

pub async fn run(options: &RunOptions) -> Result<RunReport> {
    require_root()?;
    if !options.no_trace && !crate::trace::is_available() {
        bail!("loading the eBPF observer needs root; run under sudo or pass --no-trace");
    }

    let image = ImageReference::parse(&options.image)?;
    let canonical = image.display_reference();
    let path = profile_path(&options.pipeline)?;
    let cache = FileCache::open()?;

    let existing = Profile::load(&path)?;
    let mode = choose_mode(existing.as_ref(), &canonical, &cache, options.force_cold);

    let mut profile = match existing {
        Some(profile) if profile.image == canonical => profile,
        // A profile for a different image describes a different filesystem, so
        // its entries cannot be reused.
        _ => Profile::new(&options.pipeline, &canonical),
    };

    println!(
        "pipeline {} | image {} | mode {}",
        options.pipeline,
        canonical,
        mode.label()
    );

    let mut report = execute(options, &image, mode, &mut profile, &cache).await?;

    // Safety net for the (a) "copy first" strategy: it cannot fault a missing
    // file in mid-run, so a warm run that fails is treated as a profile miss
    // and redone the slow, always-correct way.
    if mode == Mode::Warm && report.exit_code != 0 {
        eprintln!(
            "warm run exited {}; falling back to a full pull",
            report.exit_code
        );
        // Start the profile over — the one that failed described a rootfs that
        // does not work — but keep the run count, which is history, not a
        // prediction.
        let mut fresh = Profile::new(&options.pipeline, &canonical);
        fresh.runs = profile.runs;
        let mut retry = execute(options, &image, Mode::Cold, &mut fresh, &cache).await?;
        retry.fell_back = true;
        profile = fresh;
        report = retry;
    }

    profile.runs += 1;
    profile.save(&path)?;
    report.profile_entries = profile.len();

    println!(
        "profile: {} entries ({} files) after {} run(s) -> {}",
        profile.len(),
        profile.file_count(),
        profile.runs,
        path.display()
    );

    Ok(report)
}

/// Decides whether the cache can serve this run on its own.
///
/// A profile is only trusted when it is for the same image and every file it
/// names is actually present; a partially evicted cache would otherwise produce
/// a rootfs with holes in it.
fn choose_mode(
    profile: Option<&Profile>,
    image: &str,
    cache: &FileCache,
    force_cold: bool,
) -> Mode {
    if force_cold {
        return Mode::Cold;
    }
    let Some(profile) = profile else {
        return Mode::Cold;
    };
    if profile.is_empty() || profile.image != image {
        return Mode::Cold;
    }
    let missing = profile
        .required_digests()
        .into_iter()
        .filter(|digest| !cache.contains(digest))
        .count();
    if missing > 0 {
        eprintln!("profile references {missing} file(s) missing from the cache; going cold");
        return Mode::Cold;
    }
    Mode::Warm
}

async fn execute(
    options: &RunOptions,
    image: &ImageReference,
    mode: Mode,
    profile: &mut Profile,
    cache: &FileCache,
) -> Result<RunReport> {
    let workspace = Workspace::create(&options.pipeline)?;
    let lower = workspace.path().join("lower");

    let prepare_start = Instant::now();
    match mode {
        Mode::Cold => {
            let client = RegistryClient::new()?;
            let platform = PlatformSpec::host_default();
            let plan = client.pull(image, &platform).await?;
            let (_config, layers) = resolve_blobs(&client, image, &plan).await?;
            extract_layers(&layers, &lower)?;
            println!("cold: unpacked {} layer(s)", layers.len());
        }
        Mode::Warm => {
            let placed = build_from_profile(profile, cache, &lower)?;
            println!("warm: placed {placed} entr(ies) from the cache, no registry access");
        }
    }

    let disposable = Disposable::mount(workspace.path(), &lower)?;
    let prepare = prepare_start.elapsed();

    let merged = disposable.merged().to_path_buf();
    let command = container_command(&merged, &options.command);

    let execute_start = Instant::now();
    let job = GatedJob::spawn(workspace.path(), &command)?;
    let tracer = if options.no_trace {
        None
    } else {
        Some(Tracer::start(job.pid())?)
    };
    let exit_code = job.release_and_wait()?;
    let observed_paths = match tracer {
        Some(tracer) => tracer.stop()?,
        None => Vec::new(),
    };
    let execute = execute_start.elapsed();

    // Fold observations into the profile while the rootfs still exists — the
    // contents have to be read before the mount goes away.
    let before_entries: BTreeSet<String> = if options.verbose {
        profile.entries.keys().cloned().collect()
    } else {
        BTreeSet::new()
    };
    let before_digests = if options.verbose {
        cache.digests()?
    } else {
        HashSet::new()
    };

    let mut recorded = 0usize;
    for observed in &observed_paths {
        match record_path(&merged, observed, cache, profile) {
            Ok(outcome) => {
                if outcome == Record::Added {
                    recorded += 1;
                }
                if options.verbose {
                    println!("  {:<10} {observed}", label_for(outcome));
                }
            }
            Err(e) => eprintln!("warning: could not record {observed}: {e}"),
        }
    }

    if options.verbose {
        report_gains(profile, cache, &before_entries, &before_digests)?;
    }

    drop(disposable);

    Ok(RunReport {
        mode,
        prepare,
        execute,
        exit_code,
        observed: observed_paths.len(),
        recorded,
        profile_entries: profile.len(),
        fell_back: false,
    })
}

fn label_for(outcome: Record) -> &'static str {
    match outcome {
        Record::Added => "added",
        Record::Pseudo => "pseudo",
        Record::Relative => "relative",
        Record::Outside => "outside",
        Record::NotRegular => "special",
    }
}

/// Prints what this run contributed, separating the two things that "caching"
/// means here: entries gained by the profile, and contents new to the cache.
///
/// They differ, and the difference is the point. A file observed for the first
/// time by this pipeline still costs nothing to store if some other pipeline —
/// or another path in this one — already cached the same bytes.
fn report_gains(
    profile: &Profile,
    cache: &FileCache,
    before_entries: &BTreeSet<String>,
    before_digests: &HashSet<String>,
) -> Result<()> {
    let gained: Vec<_> = profile
        .entries
        .iter()
        .filter(|(path, _)| !before_entries.contains(*path))
        .collect();

    println!("--- profile gained {} entr(ies)", gained.len());
    let mut fresh_bytes = 0u64;
    let mut fresh_count = 0usize;
    for (path, entry) in gained {
        match entry {
            Entry::Dir { mode } => println!("  dir   {mode:04o}  {path}"),
            Entry::Symlink { target } => println!("  link        {path} -> {target}"),
            Entry::File { digest, mode } => {
                let size = cache.size_of(digest).unwrap_or(0);
                let novelty = if before_digests.contains(digest) {
                    "deduplicated"
                } else {
                    fresh_bytes += size;
                    fresh_count += 1;
                    "new"
                };
                let short: String = digest.chars().take(19).collect();
                println!(
                    "  file  {mode:04o}  {path}  {short}… {:.1} KiB ({novelty})",
                    size as f64 / 1024.0
                );
            }
        }
    }
    println!(
        "--- cache gained {fresh_count} entr(ies), {:.1} KiB",
        fresh_bytes as f64 / 1024.0
    );
    Ok(())
}

/// Builds the shell command that puts the job inside the disposable rootfs.
///
/// `unshare -m` keeps any mounts the job makes out of the host namespace;
/// `chroot` moves it into the merged tree. PATH is set explicitly because the
/// host's is meaningless inside the image.
fn container_command(merged: &Path, command: &[String]) -> String {
    let mut parts = vec![
        "/usr/bin/unshare".to_string(),
        "-m".to_string(),
        "/usr/bin/env".to_string(),
        "-i".to_string(),
        format!("PATH={CONTAINER_PATH}"),
        "HOME=/root".to_string(),
        "/usr/bin/chroot".to_string(),
        shell_quote(merged),
    ];
    parts.extend(command.iter().map(|arg| shell_quote(Path::new(arg))));
    parts.join(" ")
}

/// The disposable side of the design: a directory that holds the lower layer
/// and the mounts, removed when `Disposable` drops.
///
/// It lives under the cache root so that materializing files can hard-link
/// instead of copy — the two have to be on the same filesystem.
struct Workspace {
    path: PathBuf,
}

impl Workspace {
    fn create(pipeline: &str) -> Result<Self> {
        let slug: String = pipeline
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let path = cache_home()?
            .join("work")
            .join(format!("{slug}-{}", std::process::id()));
        std::fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("mepul run needs root for mount and eBPF; try `sudo -E mepul run ...`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{choose_mode, container_command, Mode, CONTAINER_PATH};
    use crate::filecache::FileCache;
    use crate::profile::{Entry, Profile};
    use std::path::{Path, PathBuf};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mepul_run_{}_{}", std::process::id(), name));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    fn profile_with(digest: &str) -> Profile {
        let mut profile = Profile::new("build", "docker.io/library/alpine:latest");
        profile.insert(
            "/bin/sh".into(),
            Entry::File {
                digest: digest.into(),
                mode: 0o755,
            },
        );
        profile
    }

    #[test]
    fn first_run_without_a_profile_is_cold() {
        let dir = scratch("no_profile");
        let cache = FileCache::open_at(&dir).unwrap();

        let mode = choose_mode(None, "docker.io/library/alpine:latest", &cache, false);

        assert_eq!(mode, Mode::Cold);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_complete_profile_goes_warm() {
        let dir = scratch("complete");
        let cache = FileCache::open_at(&dir).unwrap();
        let digest = cache.put(b"busybox").unwrap();
        let profile = profile_with(&digest);

        let mode = choose_mode(
            Some(&profile),
            "docker.io/library/alpine:latest",
            &cache,
            false,
        );

        assert_eq!(mode, Mode::Warm);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_evicted_cache_entry_forces_a_cold_run() {
        let dir = scratch("evicted");
        let cache = FileCache::open_at(&dir).unwrap();
        let profile = profile_with(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        );

        let mode = choose_mode(
            Some(&profile),
            "docker.io/library/alpine:latest",
            &cache,
            false,
        );

        assert_eq!(mode, Mode::Cold);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_profile_for_another_image_is_not_reused() {
        let dir = scratch("other_image");
        let cache = FileCache::open_at(&dir).unwrap();
        let digest = cache.put(b"busybox").unwrap();
        let profile = profile_with(&digest);

        let mode = choose_mode(
            Some(&profile),
            "docker.io/library/ubuntu:24.04",
            &cache,
            false,
        );

        assert_eq!(mode, Mode::Cold);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn force_cold_overrides_a_usable_profile() {
        let dir = scratch("force_cold");
        let cache = FileCache::open_at(&dir).unwrap();
        let digest = cache.put(b"busybox").unwrap();
        let profile = profile_with(&digest);

        let mode = choose_mode(
            Some(&profile),
            "docker.io/library/alpine:latest",
            &cache,
            true,
        );

        assert_eq!(mode, Mode::Cold);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn container_command_chroots_into_the_merged_tree() {
        let command = container_command(
            Path::new("/var/work/merged"),
            &["/bin/sh".to_string(), "-c".to_string(), "echo hi".to_string()],
        );

        assert!(command.contains("/usr/bin/unshare -m"));
        assert!(command.contains("/usr/bin/chroot '/var/work/merged'"));
        assert!(command.ends_with("'/bin/sh' '-c' 'echo hi'"));
        assert!(command.contains(CONTAINER_PATH));
    }

    #[test]
    fn container_command_quotes_arguments_with_spaces() {
        let command = container_command(
            Path::new("/var/work/merged"),
            &["/bin/sh".to_string(), "-c".to_string(), "a b; rm -rf /".to_string()],
        );

        assert!(command.ends_with(r#"'/bin/sh' '-c' 'a b; rm -rf /'"#));
    }
}
