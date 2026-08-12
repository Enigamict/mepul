//! Component (4): the file-unit, content-addressed cache.
//!
//! Layer blobs are what the registry hands out, but a CI job only ever touches
//! a fraction of the files inside them, and the ones it touches are scattered
//! across layers. So the cache stores individual file contents keyed by their
//! own hash. Identical files coming from different images or different layers
//! collapse onto one entry for free.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Mode every cache entry is stored with. Materializing a file with this mode
/// can hard-link the entry directly.
const CANONICAL_MODE: u32 = 0o644;

pub struct FileCache {
    root: PathBuf,
}

impl FileCache {
    /// Opens the cache under `$MEPUL_CACHE_DIR` (or `~/.cache/mepul`), creating
    /// it if this is the first run.
    pub fn open() -> Result<Self> {
        Self::open_at(&cache_home()?.join("files/sha256"))
    }

    pub fn open_at(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("failed to create file cache at {}", root.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub fn contains(&self, digest: &str) -> bool {
        self.path_for(digest).map(|p| p.exists()).unwrap_or(false)
    }

    /// Every digest currently held.
    ///
    /// Taken before and after a run, the difference is exactly what that run
    /// contributed — which is not the same as what it recorded, because a file
    /// whose contents are already cached costs nothing to observe again.
    /// Mode views (`<hash>.m<octal>`) and in-flight temporaries are not
    /// entries of their own, so anything with a suffix is skipped.
    pub fn digests(&self) -> Result<HashSet<String>> {
        let mut digests = HashSet::new();
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("failed to list {}", self.root.display()))?
        {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.contains('.') {
                continue;
            }
            digests.insert(format!("sha256:{name}"));
        }
        Ok(digests)
    }

    /// Size of a stored entry, for reporting what a run cached.
    pub fn size_of(&self, digest: &str) -> Option<u64> {
        fs::metadata(self.path_for(digest)?).ok().map(|m| m.len())
    }

    /// Stores `bytes` and returns the digest they are addressed by. Writing an
    /// entry that already exists is a no-op, which is what makes repeated runs
    /// cheap.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let digest = format!("sha256:{:x}", Sha256::digest(bytes));
        let path = self
            .path_for(&digest)
            .expect("digest we just produced is well formed");
        if path.exists() {
            return Ok(digest);
        }
        write_atomic(&path, bytes)?;
        Ok(digest)
    }

    /// Places a cached file at `dest` with mode `mode`.
    ///
    /// Materializing is a hard link, not a copy: the cache and the assembled
    /// lower layer are on the same filesystem, and the lower layer is only ever
    /// mounted read-only, so sharing the inode is safe and reduces "place a
    /// file" to a directory-entry write.
    ///
    /// A hard link shares the *inode*, which means it shares the mode too — so
    /// linking a 0644 cache entry into place as `/bin/sh` produces a file that
    /// cannot be executed. Content alone is therefore not a fine enough key:
    /// each distinct mode gets its own view, `<hash>.m<octal>`, paid for by one
    /// copy the first time that combination is seen and hard-linked forever
    /// after.
    pub fn materialize(&self, digest: &str, dest: &Path, mode: u32) -> Result<()> {
        let base = self
            .path_for(digest)
            .with_context(|| format!("invalid digest {digest}"))?;
        if !base.exists() {
            bail!("{digest} is not in the file cache");
        }
        let src = if mode == CANONICAL_MODE {
            base
        } else {
            self.ensure_view(&base, mode)?
        };

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if fs::symlink_metadata(dest).is_ok() {
            fs::remove_file(dest).ok();
        }

        if fs::hard_link(&src, dest).is_err() {
            fs::copy(&src, dest)
                .with_context(|| format!("failed to copy {digest} to {}", dest.display()))?;
            fs::set_permissions(dest, fs::Permissions::from_mode(mode))
                .with_context(|| format!("failed to chmod {}", dest.display()))?;
        }
        Ok(())
    }

    /// Returns a cache entry holding the same contents as `base` but with
    /// `mode`, creating it if this is the first time that mode is needed.
    fn ensure_view(&self, base: &Path, mode: u32) -> Result<PathBuf> {
        let view = base.with_extension(format!("m{mode:o}"));
        if view.exists() {
            return Ok(view);
        }
        let tmp = base.with_extension(format!("m{mode:o}.tmp{}", std::process::id()));
        fs::copy(base, &tmp)
            .with_context(|| format!("failed to copy {} for mode {mode:o}", base.display()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))
            .with_context(|| format!("failed to chmod {}", tmp.display()))?;
        fs::rename(&tmp, &view)
            .with_context(|| format!("failed to rename into {}", view.display()))?;
        Ok(view)
    }

    fn path_for(&self, digest: &str) -> Option<PathBuf> {
        let encoded = digest.strip_prefix("sha256:")?;
        if encoded.is_empty() || !encoded.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(self.root.join(encoded))
    }
}

/// Root of everything that survives a run: the file cache, the layer blobs and
/// the profiles. Deliberately outside the disposable filesystem.
pub fn cache_home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MEPUL_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cache/mepul"))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
    // Pin the mode rather than inheriting the umask, so `materialize` can tell
    // which entries are safe to hard-link without a view.
    fs::set_permissions(&tmp, fs::Permissions::from_mode(CANONICAL_MODE))
        .with_context(|| format!("failed to chmod {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("failed to rename into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::FileCache;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mepul_fc_{}_{}", std::process::id(), name));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn put_returns_sha256_of_contents() {
        let dir = scratch("put_digest");
        let cache = FileCache::open_at(&dir).unwrap();

        let digest = cache.put(b"hello world").unwrap();

        assert_eq!(
            digest,
            "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn identical_contents_collapse_onto_one_entry() {
        let dir = scratch("dedup");
        let cache = FileCache::open_at(&dir).unwrap();

        let first = cache.put(b"same bytes").unwrap();
        let second = cache.put(b"same bytes").unwrap();

        assert_eq!(first, second);
        let entries = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(entries, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn contains_reports_presence() {
        let dir = scratch("contains");
        let cache = FileCache::open_at(&dir).unwrap();
        let digest = cache.put(b"present").unwrap();

        assert!(cache.contains(&digest));
        assert!(!cache.contains(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn materialize_writes_contents_to_destination() {
        let dir = scratch("materialize");
        let cache = FileCache::open_at(&dir).unwrap();
        let digest = cache.put(b"payload").unwrap();
        let dest = dir.join("out/nested/file");

        cache.materialize(&digest, &dest, 0o644).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn materialize_preserves_an_executable_mode() {
        // Regression: hard links share the inode, so linking a 0644 entry into
        // place as /bin/sh yielded a rootfs whose shell could not be executed.
        let dir = scratch("exec_mode");
        let cache = FileCache::open_at(&dir).unwrap();
        let digest = cache.put(b"#!/bin/sh\n").unwrap();
        let dest = dir.join("out/bin/sh");

        cache.materialize(&digest, &dest, 0o755).unwrap();

        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn materializing_two_modes_of_one_content_keeps_both() {
        let dir = scratch("two_modes");
        let cache = FileCache::open_at(&dir).unwrap();
        let digest = cache.put(b"shared bytes").unwrap();
        let executable = dir.join("out/exec");
        let readable = dir.join("out/read");

        cache.materialize(&digest, &executable, 0o755).unwrap();
        cache.materialize(&digest, &readable, 0o644).unwrap();

        let exec_mode = std::fs::metadata(&executable).unwrap().permissions().mode() & 0o7777;
        let read_mode = std::fs::metadata(&readable).unwrap().permissions().mode() & 0o7777;
        assert_eq!(exec_mode, 0o755);
        assert_eq!(read_mode, 0o644);
        assert_eq!(std::fs::read(&executable).unwrap(), b"shared bytes");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn materialize_replaces_an_existing_destination() {
        let dir = scratch("replace");
        let cache = FileCache::open_at(&dir).unwrap();
        let digest = cache.put(b"new").unwrap();
        let dest = dir.join("out/file");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"stale").unwrap();

        cache.materialize(&digest, &dest, 0o644).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn materialize_fails_for_missing_entry() {
        let dir = scratch("missing");
        let cache = FileCache::open_at(&dir).unwrap();

        let result = cache.materialize(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            &dir.join("out"),
            0o644,
        );

        assert!(result.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_for_rejects_malformed_digests() {
        let dir = scratch("malformed");
        let cache = FileCache::open_at(&dir).unwrap();

        assert!(!cache.contains("md5:abcdef"));
        assert!(!cache.contains("sha256:../escape"));
        assert!(!cache.contains("sha256:"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
