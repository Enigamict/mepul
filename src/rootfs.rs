//! Component (3): the disposable root filesystem.
//!
//! The job gets an overlay whose upper layer is a tmpfs. Everything the job
//! writes lands in memory and dies with the mount; the lower layer is assembled
//! read-only, either by unpacking the image (cold) or by replaying a profile out
//! of the file cache (warm).

use std::collections::VecDeque;
use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;

use crate::filecache::FileCache;
use crate::profile::{Entry, Profile};
use crate::store::{BlobSource, ResolvedBlob};

/// Symlink hops allowed while resolving one observed path, matching the
/// kernel's own ceiling.
const MAX_SYMLINK_HOPS: usize = 40;

/// How many times recording a binary may pull in the loader it names. A loader
/// names no loader of its own, so one hop is the realistic case.
const MAX_INTERP_DEPTH: usize = 4;

/// Roots the disposable filesystem mounts fresh on every run.
///
/// Recording under them is worse than useless: `/proc/<pid>/maps` is noise that
/// names a pid which will never exist again, and any contents replayed there
/// would sit underneath a mount that hides them anyway.
const PSEUDO_ROOTS: [&str; 3] = ["/proc", "/sys", "/dev"];

/// An overlay mount plus the tmpfs backing its upper layer, unmounted on drop.
pub struct Disposable {
    base: PathBuf,
    merged: PathBuf,
    mounted_overlay: bool,
    mounted_tmpfs: bool,
    mounted_proc: bool,
}

impl Disposable {
    /// Mounts `lower` read-only under a tmpfs-backed overlay.
    pub fn mount(base: &Path, lower: &Path) -> Result<Self> {
        let merged = base.join("merged");
        let rw = base.join("rw");
        fs::create_dir_all(&merged)?;
        fs::create_dir_all(&rw)?;

        let mut disposable = Self {
            base: base.to_path_buf(),
            merged: merged.clone(),
            mounted_overlay: false,
            mounted_tmpfs: false,
            mounted_proc: false,
        };

        mount_syscall("tmpfs", &rw, "tmpfs", 0, Some("mode=0755"))
            .context("failed to mount tmpfs for the disposable upper layer")?;
        disposable.mounted_tmpfs = true;

        let upper = rw.join("upper");
        let work = rw.join("work");
        fs::create_dir_all(&upper)?;
        fs::create_dir_all(&work)?;

        let options = format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        );
        mount_syscall("overlay", &merged, "overlay", 0, Some(&options))
            .context("failed to mount the overlay")?;
        disposable.mounted_overlay = true;

        // /proc has to exist at the *chrooted* path, so it is mounted inside
        // the merged tree rather than left to `unshare --mount-proc`.
        let proc = merged.join("proc");
        fs::create_dir_all(&proc)?;
        mount_syscall("proc", &proc, "proc", 0, None).context("failed to mount /proc")?;
        disposable.mounted_proc = true;

        populate_dev(&merged)?;

        Ok(disposable)
    }

    pub fn merged(&self) -> &Path {
        &self.merged
    }
}

impl Drop for Disposable {
    fn drop(&mut self) {
        // The whole point is that this goes away; report failures but never
        // panic out of a drop. Inner mounts come off first.
        if self.mounted_proc {
            if let Err(e) = umount(&self.merged.join("proc")) {
                eprintln!("warning: failed to unmount /proc: {e}");
            }
        }
        if self.mounted_overlay {
            if let Err(e) = umount(&self.merged) {
                eprintln!("warning: failed to unmount overlay: {e}");
                return;
            }
        }
        if self.mounted_tmpfs {
            if let Err(e) = umount(&self.base.join("rw")) {
                eprintln!("warning: failed to unmount tmpfs: {e}");
                return;
            }
        }
        if let Err(e) = fs::remove_dir_all(&self.base) {
            eprintln!("warning: failed to remove {}: {e}", self.base.display());
        }
    }
}

/// Creates the handful of device nodes anything nontrivial expects. They land
/// in the tmpfs upper layer, so they are disposable too.
fn populate_dev(merged: &Path) -> Result<()> {
    let dev = merged.join("dev");
    fs::create_dir_all(&dev)?;
    for (name, major, minor) in [
        ("null", 1, 3),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
    ] {
        let path = dev.join(name);
        if path.exists() {
            continue;
        }
        let c_path = CString::new(path.as_os_str().as_encoded_bytes())?;
        let dev_id = libc::makedev(major, minor);
        // Best effort: a sandbox may forbid mknod, and most jobs still run.
        unsafe { libc::mknod(c_path.as_ptr(), libc::S_IFCHR | 0o666, dev_id) };
    }
    Ok(())
}

fn mount_syscall(
    source: &str,
    target: &Path,
    fstype: &str,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<()> {
    let c_source = CString::new(source)?;
    let c_target = CString::new(target.as_os_str().as_encoded_bytes())?;
    let c_fstype = CString::new(fstype)?;
    let c_data = data.map(CString::new).transpose()?;
    let data_ptr = c_data
        .as_ref()
        .map(|d| d.as_ptr() as *const libc::c_void)
        .unwrap_or(std::ptr::null());

    let rc = unsafe {
        libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            c_fstype.as_ptr(),
            flags,
            data_ptr,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        bail!("mount({fstype}) on {} failed: {err}", target.display());
    }
    Ok(())
}

fn umount(target: &Path) -> Result<()> {
    let c_target = CString::new(target.as_os_str().as_encoded_bytes())?;
    // MNT_DETACH so a lingering process in the mount does not wedge cleanup.
    let rc = unsafe { libc::umount2(c_target.as_ptr(), libc::MNT_DETACH) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        bail!("umount {} failed: {err}", target.display());
    }
    Ok(())
}

/// Cold path: unpacks every layer in order into `dest`.
pub fn extract_layers(layers: &[ResolvedBlob], dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for (i, layer) in layers.iter().enumerate() {
        extract_layer(layer, dest)
            .with_context(|| format!("failed to extract layer {}", i + 1))?;
    }
    Ok(())
}

fn extract_layer(layer: &ResolvedBlob, dest: &Path) -> Result<()> {
    let reader: Box<dyn Read> = match &layer.source {
        BlobSource::CachedFile(path) => Box::new(
            fs::File::open(path)
                .with_context(|| format!("failed to open {}", path.display()))?,
        ),
        BlobSource::Downloaded(bytes) => Box::new(std::io::Cursor::new(bytes.clone())),
    };

    let mut archive = tar::Archive::new(GzDecoder::new(reader));
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();

        if let Some(target) = whiteout_target(&path) {
            apply_whiteout(dest, &target)?;
            continue;
        }
        // Layers routinely ship entries whose parent was created by an earlier
        // layer with restrictive permissions; unpack_in reports those rather
        // than aborting the whole image.
        if let Err(e) = entry.unpack_in(dest) {
            eprintln!("warning: skipped {}: {e}", path.display());
        }
    }
    Ok(())
}

/// Maps an OCI whiteout entry to the path it deletes.
///
/// `.wh..wh..opq` clears a directory's inherited contents; `.wh.<name>` deletes
/// one sibling.
fn whiteout_target(path: &Path) -> Option<WhiteoutTarget> {
    let name = path.file_name()?.to_str()?;
    let parent = path.parent().unwrap_or(Path::new(""));
    if name == ".wh..wh..opq" {
        return Some(WhiteoutTarget::Opaque(parent.to_path_buf()));
    }
    let stripped = name.strip_prefix(".wh.")?;
    Some(WhiteoutTarget::Single(parent.join(stripped)))
}

enum WhiteoutTarget {
    Single(PathBuf),
    Opaque(PathBuf),
}

fn apply_whiteout(dest: &Path, target: &WhiteoutTarget) -> Result<()> {
    match target {
        WhiteoutTarget::Single(relative) => {
            let path = dest.join(relative);
            if path.is_dir() {
                fs::remove_dir_all(&path).ok();
            } else {
                fs::remove_file(&path).ok();
            }
        }
        WhiteoutTarget::Opaque(relative) => {
            let path = dest.join(relative);
            if let Ok(entries) = fs::read_dir(&path) {
                for entry in entries.flatten() {
                    let child = entry.path();
                    if child.is_dir() {
                        fs::remove_dir_all(&child).ok();
                    } else {
                        fs::remove_file(&child).ok();
                    }
                }
            }
        }
    }
    Ok(())
}

/// Warm path: builds a lower layer out of the profile alone.
///
/// This is the (a) "copy first" strategy from the design: everything the
/// profile knows about is placed before the job starts. Entries arrive parents
/// first, so directories always exist by the time their contents are written.
pub fn build_from_profile(profile: &Profile, cache: &FileCache, dest: &Path) -> Result<usize> {
    fs::create_dir_all(dest)?;
    let mut placed = 0usize;

    for (path, entry) in &profile.entries {
        let relative = path.trim_start_matches('/');
        if relative.is_empty() {
            continue;
        }
        let target = dest.join(relative);

        match entry {
            Entry::Dir { mode } => {
                fs::create_dir_all(&target)
                    .with_context(|| format!("failed to create {}", target.display()))?;
                fs::set_permissions(&target, fs::Permissions::from_mode(*mode)).ok();
            }
            Entry::Symlink { target: link } => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                if target.exists() || fs::symlink_metadata(&target).is_ok() {
                    fs::remove_file(&target).ok();
                }
                symlink(link, &target)
                    .with_context(|| format!("failed to link {}", target.display()))?;
            }
            Entry::File { digest, mode } => {
                cache
                    .materialize(digest, &target, *mode)
                    .with_context(|| format!("failed to place {path}"))?;
            }
        }
        placed += 1;
    }

    Ok(placed)
}

/// Folds one observed path into the profile, caching the contents it resolves
/// to.
///
/// Observing an open of `/bin/sh` is not enough on its own to rebuild a working
/// rootfs: `/bin` has to be a directory and `/bin/sh` may be a symlink to
/// `busybox`. So every component along the way is recorded, symlinks are
/// followed, and each link's target is recorded too.
///
/// What became of one observed path. Most of a run's observations are not
/// worth recording, and the reason is the interesting part: it explains why a
/// job that opened fifty paths contributes three entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    /// Recorded, along with every component and symlink on the way to it.
    Added,
    /// On a filesystem the disposable rootfs mounts fresh every run.
    Pseudo,
    /// Not absolute, so it depends on a working directory the next run will
    /// not have.
    Relative,
    /// Does not resolve under the rootfs — host activity the tracer picked up
    /// because it watches the whole process tree.
    Outside,
    /// A socket, fifo or device node: nothing whose contents mean anything.
    NotRegular,
}

pub fn record_path(
    root: &Path,
    logical: &str,
    cache: &FileCache,
    profile: &mut Profile,
) -> Result<Record> {
    record_within(root, logical, cache, profile, 0)
}

fn record_within(
    root: &Path,
    logical: &str,
    cache: &FileCache,
    profile: &mut Profile,
    depth: usize,
) -> Result<Record> {
    if !logical.starts_with('/') {
        return Ok(Record::Relative);
    }
    if is_pseudo(logical) {
        return Ok(Record::Pseudo);
    }
    // Set when the recorded file names a dynamic loader, which has to be
    // recorded too — but only once the walk below has let go of `profile`.
    let mut interpreter: Option<String> = None;

    let mut remaining: VecDeque<String> = logical
        .split('/')
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect();
    let mut current = String::new();
    let mut hops = 0usize;

    while let Some(component) = remaining.pop_front() {
        if component == "." {
            continue;
        }
        if component == ".." {
            if let Some(index) = current.rfind('/') {
                current.truncate(index);
            }
            continue;
        }

        let candidate = format!("{current}/{component}");
        let host = root.join(candidate.trim_start_matches('/'));
        let metadata = match fs::symlink_metadata(&host) {
            Ok(metadata) => metadata,
            Err(_) => return Ok(Record::Outside),
        };

        if metadata.is_symlink() {
            hops += 1;
            if hops > MAX_SYMLINK_HOPS {
                bail!("too many symlink hops resolving {logical}");
            }
            let link = fs::read_link(&host)?;
            let link = link.to_string_lossy().into_owned();
            profile.insert(candidate.clone(), Entry::Symlink { target: link.clone() });

            // Splice the link target in front of what is left to resolve. An
            // absolute target restarts from the container root; a relative one
            // continues from the directory holding the link.
            let mut spliced: VecDeque<String> = link
                .split('/')
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect();
            if link.starts_with('/') {
                current.clear();
            }
            while let Some(part) = spliced.pop_back() {
                remaining.push_front(part);
            }
        } else if metadata.is_dir() {
            profile.insert(
                candidate.clone(),
                Entry::Dir {
                    mode: metadata.mode() & 0o7777,
                },
            );
            current = candidate;
        } else if metadata.is_file() {
            if !remaining.is_empty() {
                // Something in the middle of the path is a regular file, so the
                // path cannot resolve.
                return Ok(Record::Outside);
            }
            // Read once and hand the same bytes to both the cache and the ELF
            // check, since every observed file goes through here.
            let bytes = fs::read(&host)
                .with_context(|| format!("failed to read {}", host.display()))?;
            let digest = cache.put(&bytes)?;
            profile.insert(
                candidate.clone(),
                Entry::File {
                    digest,
                    mode: metadata.mode() & 0o7777,
                },
            );
            interpreter = elf_interpreter(&bytes);
            current = candidate;
        } else {
            // Sockets, fifos and device nodes have no contents worth caching.
            return Ok(Record::NotRegular);
        }
    }

    if let Some(interpreter) = interpreter {
        if depth < MAX_INTERP_DEPTH {
            record_within(root, &interpreter, cache, profile, depth + 1)?;
        }
    }

    Ok(Record::Added)
}

fn is_pseudo(path: &str) -> bool {
    PSEUDO_ROOTS.iter().any(|root| {
        matches!(path.strip_prefix(root), Some(rest) if rest.is_empty() || rest.starts_with('/'))
    })
}

/// The dynamic loader an ELF names in its `PT_INTERP` header, if it has one.
///
/// This exists to cover the observer's one structural blind spot. When the
/// kernel executes a dynamically linked binary it opens the interpreter itself,
/// inside `execve`, without ever issuing an `openat` — so the file without which
/// the binary cannot start is precisely the file the tracepoint never reports.
/// On musl the loader is also libc and is never reopened, so nothing else in the
/// run names it either. Reading it out of the binary is the only way to learn
/// it.
fn elf_interpreter(bytes: &[u8]) -> Option<String> {
    const PT_INTERP: u32 = 3;

    if bytes.get(..4)? != b"\x7fELF" {
        return None;
    }
    // Little-endian only: every architecture this runs on is little-endian, and
    // guessing wrong should miss rather than invent a path.
    if *bytes.get(5)? != 1 {
        return None;
    }
    let wide = match *bytes.get(4)? {
        2 => true,
        1 => false,
        _ => return None,
    };

    let (table, entry_size, count) = if wide {
        (read_u64(bytes, 0x20)?, read_u16(bytes, 0x36)?, read_u16(bytes, 0x38)?)
    } else {
        (read_u32(bytes, 0x1c)?, read_u16(bytes, 0x2a)?, read_u16(bytes, 0x2c)?)
    };

    for index in 0..count {
        let header = table.checked_add(index.checked_mul(entry_size)?)?;
        if read_u32(bytes, header)? as u32 != PT_INTERP {
            continue;
        }
        let (offset, size) = if wide {
            (read_u64(bytes, header.checked_add(8)?)?, read_u64(bytes, header.checked_add(0x20)?)?)
        } else {
            (read_u32(bytes, header.checked_add(4)?)?, read_u32(bytes, header.checked_add(0x10)?)?)
        };
        let segment = bytes.get(offset..offset.checked_add(size)?)?;
        let text = std::str::from_utf8(segment.split(|b| *b == 0).next()?).ok()?;
        // A relative interpreter is not something the profile can replay.
        return text.starts_with('/').then(|| text.to_string());
    }
    None
}

fn read_u16(bytes: &[u8], at: usize) -> Option<usize> {
    let end = at.checked_add(2)?;
    Some(u16::from_le_bytes(bytes.get(at..end)?.try_into().ok()?) as usize)
}

fn read_u32(bytes: &[u8], at: usize) -> Option<usize> {
    let end = at.checked_add(4)?;
    Some(u32::from_le_bytes(bytes.get(at..end)?.try_into().ok()?) as usize)
}

fn read_u64(bytes: &[u8], at: usize) -> Option<usize> {
    let end = at.checked_add(8)?;
    Some(u64::from_le_bytes(bytes.get(at..end)?.try_into().ok()?) as usize)
}

#[cfg(test)]
mod tests {
    use super::{
        build_from_profile, elf_interpreter, is_pseudo, record_path, whiteout_target, Record,
        WhiteoutTarget,
    };
    use crate::filecache::FileCache;
    use crate::profile::{Entry, Profile};
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};

    /// A minimal little-endian ELF64 carrying one `PT_INTERP` header naming
    /// `interp`. Enough structure for the parser, nothing that could run.
    fn elf_named(interp: &str) -> Vec<u8> {
        const PHOFF: usize = 0x40;
        const STROFF: usize = 0x80;

        let mut elf = vec![0u8; STROFF + interp.len() + 1];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2; // 64-bit
        elf[5] = 1; // little-endian
        elf[0x20..0x28].copy_from_slice(&(PHOFF as u64).to_le_bytes());
        elf[0x36..0x38].copy_from_slice(&0x38u16.to_le_bytes()); // e_phentsize
        elf[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

        elf[PHOFF..PHOFF + 4].copy_from_slice(&3u32.to_le_bytes()); // PT_INTERP
        elf[PHOFF + 8..PHOFF + 16].copy_from_slice(&(STROFF as u64).to_le_bytes());
        let size = interp.len() as u64 + 1;
        elf[PHOFF + 0x20..PHOFF + 0x28].copy_from_slice(&size.to_le_bytes());

        elf[STROFF..STROFF + interp.len()].copy_from_slice(interp.as_bytes());
        elf
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mepul_rf_{}_{}", std::process::id(), name));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A miniature rootfs: /bin/busybox with /bin/sh pointing at it.
    fn fake_root(dir: &Path) -> PathBuf {
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/busybox"), b"ELF").unwrap();
        symlink("busybox", root.join("bin/sh")).unwrap();
        root
    }

    #[test]
    fn whiteout_target_recognises_single_deletion() {
        let target = whiteout_target(Path::new("usr/lib/.wh.libfoo.so")).unwrap();

        match target {
            WhiteoutTarget::Single(path) => assert_eq!(path, Path::new("usr/lib/libfoo.so")),
            _ => panic!("expected a single-file whiteout"),
        }
    }

    #[test]
    fn whiteout_target_recognises_opaque_directory() {
        let target = whiteout_target(Path::new("var/cache/.wh..wh..opq")).unwrap();

        match target {
            WhiteoutTarget::Opaque(path) => assert_eq!(path, Path::new("var/cache")),
            _ => panic!("expected an opaque whiteout"),
        }
    }

    #[test]
    fn whiteout_target_ignores_ordinary_entries() {
        assert!(whiteout_target(Path::new("bin/sh")).is_none());
        assert!(whiteout_target(Path::new("bin/whatever")).is_none());
    }

    #[test]
    fn record_path_captures_every_component_and_follows_symlinks() {
        let dir = scratch("record_symlink");
        let root = fake_root(&dir);
        let cache = FileCache::open_at(&dir.join("cache")).unwrap();
        let mut profile = Profile::new("t", "img");

        let outcome = record_path(&root, "/bin/sh", &cache, &mut profile).unwrap();

        assert_eq!(outcome, Record::Added);
        assert!(matches!(
            profile.entries.get("/bin"),
            Some(Entry::Dir { .. })
        ));
        assert!(matches!(
            profile.entries.get("/bin/sh"),
            Some(Entry::Symlink { target }) if target == "busybox"
        ));
        assert!(matches!(
            profile.entries.get("/bin/busybox"),
            Some(Entry::File { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_path_rejects_paths_outside_the_rootfs() {
        let dir = scratch("record_absent");
        let root = fake_root(&dir);
        let cache = FileCache::open_at(&dir.join("cache")).unwrap();
        let mut profile = Profile::new("t", "img");

        let outcome = record_path(&root, "/usr/lib/host-only.so", &cache, &mut profile).unwrap();

        assert_eq!(outcome, Record::Outside);
        assert!(profile.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_path_ignores_relative_paths() {
        let dir = scratch("record_relative");
        let root = fake_root(&dir);
        let cache = FileCache::open_at(&dir.join("cache")).unwrap();
        let mut profile = Profile::new("t", "img");

        let outcome = record_path(&root, "lib/relative.so", &cache, &mut profile).unwrap();

        assert_eq!(outcome, Record::Relative);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_path_normalises_dot_dot() {
        let dir = scratch("record_dotdot");
        let root = fake_root(&dir);
        let cache = FileCache::open_at(&dir.join("cache")).unwrap();
        let mut profile = Profile::new("t", "img");

        let outcome = record_path(&root, "/bin/../bin/busybox", &cache, &mut profile).unwrap();

        assert_eq!(outcome, Record::Added);
        assert!(profile.entries.contains_key("/bin/busybox"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn elf_interpreter_reads_the_loader_out_of_a_binary() {
        let elf = elf_named("/lib/ld-musl-aarch64.so.1");

        assert_eq!(
            elf_interpreter(&elf).as_deref(),
            Some("/lib/ld-musl-aarch64.so.1")
        );
    }

    #[test]
    fn elf_interpreter_ignores_files_that_are_not_elf() {
        assert_eq!(elf_interpreter(b"#!/bin/sh\necho hi\n"), None);
        assert_eq!(elf_interpreter(b""), None);
    }

    #[test]
    fn elf_interpreter_survives_a_truncated_binary() {
        // A header promising a program table past the end of the file must miss,
        // not panic: cached contents are attacker-adjacent input.
        let elf = elf_named("/lib/ld.so");

        assert_eq!(elf_interpreter(&elf[..0x50]), None);
    }

    #[test]
    fn record_path_pulls_in_the_loader_the_tracer_cannot_see() {
        // The kernel opens the interpreter inside execve, so no probe reports
        // it. Recording the binary has to be enough to learn it anyway.
        let dir = scratch("record_interp");
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(root.join("bin/prog"), elf_named("/lib/ld-fake.so.1")).unwrap();
        std::fs::write(root.join("lib/ld-fake.so.1"), b"loader").unwrap();
        let cache = FileCache::open_at(&dir.join("cache")).unwrap();
        let mut profile = Profile::new("t", "img");

        record_path(&root, "/bin/prog", &cache, &mut profile).unwrap();

        assert!(matches!(
            profile.entries.get("/lib/ld-fake.so.1"),
            Some(Entry::File { .. })
        ));
        assert!(matches!(
            profile.entries.get("/lib"),
            Some(Entry::Dir { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_path_skips_filesystems_that_are_mounted_fresh() {
        // /proc/<pid>/maps names a pid that will never exist again, and the
        // mount would hide any replayed contents regardless.
        let dir = scratch("record_pseudo");
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("proc/42")).unwrap();
        std::fs::write(root.join("proc/42/maps"), b"...").unwrap();
        let cache = FileCache::open_at(&dir.join("cache")).unwrap();
        let mut profile = Profile::new("t", "img");

        let outcome = record_path(&root, "/proc/42/maps", &cache, &mut profile).unwrap();

        assert_eq!(outcome, Record::Pseudo);
        assert!(profile.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_pseudo_matches_whole_components_only() {
        assert!(is_pseudo("/proc"));
        assert!(is_pseudo("/proc/self/maps"));
        assert!(is_pseudo("/sys/kernel"));
        assert!(is_pseudo("/dev/null"));
        // A real path that merely starts with the same letters is not pseudo.
        assert!(!is_pseudo("/procession"));
        assert!(!is_pseudo("/etc/hostname"));
    }

    #[test]
    fn record_then_build_reproduces_a_working_path() {
        let dir = scratch("roundtrip");
        let root = fake_root(&dir);
        let cache = FileCache::open_at(&dir.join("cache")).unwrap();
        let mut profile = Profile::new("t", "img");
        record_path(&root, "/bin/sh", &cache, &mut profile).unwrap();

        let rebuilt = dir.join("rebuilt");
        let placed = build_from_profile(&profile, &cache, &rebuilt).unwrap();

        assert_eq!(placed, profile.len());
        assert_eq!(std::fs::read(rebuilt.join("bin/busybox")).unwrap(), b"ELF");
        // Resolving through the replayed symlink is the property that matters.
        assert_eq!(std::fs::read(rebuilt.join("bin/sh")).unwrap(), b"ELF");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_from_profile_fails_when_cache_is_missing_contents() {
        let dir = scratch("build_missing");
        let cache = FileCache::open_at(&dir.join("cache")).unwrap();
        let mut profile = Profile::new("t", "img");
        profile.insert("/bin".into(), Entry::Dir { mode: 0o755 });
        profile.insert(
            "/bin/sh".into(),
            Entry::File {
                digest: "sha256:00".into(),
                mode: 0o755,
            },
        );

        let result = build_from_profile(&profile, &cache, &dir.join("rebuilt"));

        assert!(result.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
