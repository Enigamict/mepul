//! Component (1): the eBPF observer.
//!
//! Watches which files the job reaches for and nothing else — it never carries
//! the data itself. The output is one list of paths, which becomes the profile.
//!
//! "Reaches for" is deliberately wider than "opens". A job fails just as hard on
//! a missing file it only ever `stat`ed: `test -f`, `ls`, and every configure
//! script alive decide what to do from metadata alone, and a profile built from
//! `openat` would leave those files out of the rootfs. So the stat, access and
//! readlink families are observed too. They cost nothing extra — a path that
//! turns out not to matter is one the profile records and the job never uses.
//!
//! One blind spot cannot be closed from here: the ELF interpreter. The kernel
//! opens it inside `execve` without a syscall of its own, so no probe sees it.
//! `rootfs::record_path` reads it out of the binary instead.
//!
//! Three details make the observation trustworthy:
//!
//! * Nothing is reported until the traced process has called `chroot`. Getting
//!   the job into its rootfs takes `unshare`, `env` and `chroot` — host
//!   binaries that open their own libraries and locales first, in the same pid
//!   the job will run under. Their opens are indistinguishable from the job's
//!   by path alone, and when the host and the image are both Ubuntu the paths
//!   exist on both sides, so they would be recorded as if the job needed them.
//!   The `chroot` syscall is the moment the process stops looking at the host,
//!   which makes it the honest boundary. A mount namespace is not: `unshare -m`
//!   creates the new namespace *before* `env` and `chroot` run, so they are
//!   already inside it.
//!
//! * The tracer follows a process *and its descendants*. A CI job is a tree of
//!   processes, and `unshare -f` forks before the real command ever runs, so
//!   tracking a single pid would see almost nothing.
//! * The traced process is started blocked on a gate. Probes attach while it
//!   cannot run, then the gate opens. Without that ordering the first execve —
//!   the most important event of the run — races the attach.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{bail, Context, Result};
use aya::maps::{HashMap as BpfHashMap, MapData, RingBuf};
use aya::programs::TracePoint;
use aya::{Ebpf, EbpfLoader};

/// The compiled observer, embedded so an ephemeral runner needs nothing
/// installed to be traced.
const OBJECT: &[u8] = include_bytes!(env!("MEPUL_BPF_OBJECT"));

/// Every program in the object, as `(category, name)` for `TracePoint::attach`.
///
/// Listed explicitly rather than discovered: a probe silently failing to attach
/// would show up as a thinner profile, not as an error, and that is exactly the
/// kind of failure this design cannot afford.
const PROBES: [(&str, &str, &str); 10] = [
    ("on_fork", "sched", "sched_process_fork"),
    ("on_exit", "sched", "sched_process_exit"),
    ("on_chroot", "syscalls", "sys_enter_chroot"),
    ("on_execve", "syscalls", "sys_enter_execve"),
    ("on_openat", "syscalls", "sys_enter_openat"),
    ("on_newfstatat", "syscalls", "sys_enter_newfstatat"),
    ("on_statx", "syscalls", "sys_enter_statx"),
    ("on_faccessat", "syscalls", "sys_enter_faccessat"),
    ("on_faccessat2", "syscalls", "sys_enter_faccessat2"),
    ("on_readlinkat", "syscalls", "sys_enter_readlinkat"),
];

/// Mirrors `struct event` in the observer.
const PATH_MAX: usize = 4096;
const EVENT_SIZE: usize = 4 + PATH_MAX;

pub struct Tracer {
    /// Held so the programs stay attached; dropping it detaches everything.
    _bpf: Ebpf,
    events: Receiver<String>,
    reader: Option<JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}

impl Tracer {
    /// Attaches to `target_pid` and its future descendants, returning once the
    /// probes are live.
    ///
    /// Loading happens before the seed pid is written and before anything is
    /// attached, so by the time this returns there is no window in which the
    /// job could run unobserved. The gated job is blocked on that guarantee.
    pub fn start(target_pid: u32) -> Result<Self> {
        let mut bpf = EbpfLoader::new()
            .load(OBJECT)
            .context("failed to load the eBPF observer (needs root and a BPF-enabled kernel)")?;

        // Seed the process tree before attaching. A pid written afterwards
        // could miss the job's very first syscall.
        {
            let mut tracked: BpfHashMap<_, u32, u8> = bpf
                .map_mut("tracked")
                .context("the observer has no `tracked` map")?
                .try_into()?;
            tracked
                .insert(target_pid, 1, 0)
                .context("failed to seed the traced pid")?;
        }

        for (program, category, name) in PROBES {
            let probe: &mut TracePoint = bpf
                .program_mut(program)
                .with_context(|| format!("the observer has no program `{program}`"))?
                .try_into()?;
            probe
                .load()
                .with_context(|| format!("the verifier rejected `{program}`"))?;
            probe
                .attach(category, name)
                .with_context(|| format!("failed to attach `{program}` to {category}:{name}"))?;
        }

        let ring: RingBuf<MapData> = bpf
            .take_map("events")
            .context("the observer has no `events` ring buffer")?
            .try_into()?;

        let (event_tx, event_rx) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let reader = std::thread::spawn({
            let stopping = Arc::clone(&stopping);
            move || drain(ring, event_tx, stopping)
        });

        Ok(Self {
            _bpf: bpf,
            events: event_rx,
            reader: Some(reader),
            stopping,
        })
    }

    /// Stops tracing and returns the observed paths, deduplicated in first-seen
    /// order.
    pub fn stop(mut self) -> Result<Vec<String>> {
        // The reader drains what is already in the ring before it exits, so the
        // last events of the run are not lost.
        self.stopping.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }

        let mut seen = std::collections::HashSet::new();
        let mut paths = Vec::new();
        while let Ok(path) = self.events.try_recv() {
            if seen.insert(path.clone()) {
                paths.push(path);
            }
        }
        Ok(paths)
    }
}

/// Moves records off the ring buffer until the run ends.
///
/// The ring has no blocking read here, so the thread polls; the interval is
/// short enough that a 16 MiB ring cannot fill during a job, and long enough
/// that idling costs nothing measurable.
fn drain(mut ring: RingBuf<MapData>, tx: mpsc::Sender<String>, stopping: Arc<AtomicBool>) {
    loop {
        let mut drained = false;
        while let Some(record) = ring.next() {
            drained = true;
            if let Some(path) = decode(&record) {
                if tx.send(path).is_err() {
                    return;
                }
            }
        }
        if stopping.load(Ordering::Acquire) && !drained {
            return;
        }
        if !drained {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// Reads one `struct event`. A record whose length does not fit its own buffer
/// is dropped rather than trusted.
fn decode(record: &[u8]) -> Option<String> {
    if record.len() < EVENT_SIZE {
        return None;
    }
    let len = u32::from_ne_bytes(record[..4].try_into().ok()?) as usize;
    if len == 0 || len > PATH_MAX {
        return None;
    }
    std::str::from_utf8(&record[4..4 + len])
        .ok()
        .map(str::to_string)
}

/// A process parked on a FIFO so probes can attach before it does anything.
///
/// `sh` opens the gate for reading, which blocks until a writer shows up. The
/// pid is stable across the wait and the subsequent `exec`, so it is safe to
/// hand to the tracer.
pub struct GatedJob {
    child: Child,
    gate: PathBuf,
}

impl GatedJob {
    pub fn spawn(gate_dir: &Path, command: &str) -> Result<Self> {
        let gate = gate_dir.join("gate");
        std::fs::remove_file(&gate).ok();
        make_fifo(&gate)?;

        let script = format!("read _ < {}; exec {}", shell_quote(&gate), command);
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .spawn()
            .context("failed to spawn the gated job")?;

        Ok(Self { child, gate })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Opens the gate and waits for the job to finish, yielding its exit code.
    pub fn release_and_wait(mut self) -> Result<i32> {
        let mut gate = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.gate)
            .with_context(|| format!("failed to open gate {}", self.gate.display()))?;
        gate.write_all(b"go\n")?;
        drop(gate);

        let status = self.child.wait().context("failed to wait for the job")?;
        std::fs::remove_file(&self.gate).ok();
        Ok(status.code().unwrap_or(-1))
    }
}

fn make_fifo(path: &Path) -> Result<()> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        bail!("failed to create gate {}: {err}", path.display());
    }
    Ok(())
}

/// Single-quotes a path for `/bin/sh`.
pub fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', r#"'\''"#))
}

/// Whether this kernel will accept the observer.
///
/// The programs are compiled into the binary, so there is nothing to install;
/// what can still be missing is the privilege to load them.
pub fn is_available() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::{decode, shell_quote, EVENT_SIZE, OBJECT, PATH_MAX, PROBES};
    use std::path::Path;

    fn record(path: &[u8]) -> Vec<u8> {
        let mut buffer = vec![0u8; EVENT_SIZE];
        buffer[..4].copy_from_slice(&(path.len() as u32).to_ne_bytes());
        buffer[4..4 + path.len()].copy_from_slice(path);
        buffer
    }

    #[test]
    fn the_compiled_observer_is_embedded() {
        // A runner is wiped between jobs; anything that had to be installed
        // there would have to be installed every time.
        assert!(OBJECT.len() > 1024);
        assert_eq!(&OBJECT[..4], b"\x7fELF");
    }

    #[test]
    fn every_probe_names_a_real_tracepoint_category() {
        for (_, category, _) in PROBES {
            assert!(
                category == "sched" || category == "syscalls",
                "unexpected tracepoint category {category}"
            );
        }
    }

    #[test]
    fn descendants_are_tracked_not_just_the_target() {
        // Without the fork probe the tracer would miss everything the job's own
        // subprocesses do.
        assert!(PROBES.iter().any(|(_, _, n)| *n == "sched_process_fork"));
    }

    #[test]
    fn both_opens_and_execs_are_observed() {
        assert!(PROBES.iter().any(|(_, _, n)| *n == "sys_enter_openat"));
        assert!(PROBES.iter().any(|(_, _, n)| *n == "sys_enter_execve"));
    }

    #[test]
    fn metadata_only_access_is_observed_too() {
        // A job fails just as hard on a file it only ever stat'ed.
        for name in [
            "sys_enter_newfstatat",
            "sys_enter_statx",
            "sys_enter_faccessat",
            "sys_enter_faccessat2",
            "sys_enter_readlinkat",
        ] {
            assert!(
                PROBES.iter().any(|(_, _, n)| *n == name),
                "{name} should be observed"
            );
        }
    }

    #[test]
    fn the_rootfs_boundary_is_observed() {
        // Without this probe nothing would ever be reported: `inside` is only
        // ever set from chroot.
        assert!(PROBES.iter().any(|(_, _, n)| *n == "sys_enter_chroot"));
    }

    #[test]
    fn decode_reads_a_path_out_of_a_record() {
        assert_eq!(decode(&record(b"/bin/sh")).as_deref(), Some("/bin/sh"));
    }

    #[test]
    fn decode_rejects_a_truncated_record() {
        assert_eq!(decode(&record(b"/bin/sh")[..64]), None);
    }

    #[test]
    fn decode_rejects_a_length_that_overruns_the_buffer() {
        // The length comes from the kernel program; treating it as trustworthy
        // would be a read past the record.
        let mut bad = record(b"/bin/sh");
        bad[..4].copy_from_slice(&((PATH_MAX + 1) as u32).to_ne_bytes());

        assert_eq!(decode(&bad), None);
    }

    #[test]
    fn decode_rejects_an_empty_path() {
        assert_eq!(decode(&record(b"")), None);
    }

    #[test]
    fn shell_quote_wraps_plain_paths() {
        assert_eq!(shell_quote(Path::new("/tmp/gate")), "'/tmp/gate'");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote(Path::new("/tmp/it's")), r#"'/tmp/it'\''s'"#);
    }
}
