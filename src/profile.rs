//! Component (2): the access profile that survives a run.
//!
//! The runner is wiped between runs; this file is not. It records what the
//! pipeline actually opened last time, so the next run can assemble a root
//! filesystem out of just those files instead of pulling the whole image.
//!
//! Entries are keyed by path in a `BTreeMap`, which does two useful things:
//! observations from repeated runs merge instead of accumulating duplicates,
//! and iteration yields parents before their children (`/bin` sorts before
//! `/bin/sh`), so replaying the profile never has to create a file inside a
//! directory that does not exist yet.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::filecache::cache_home;

/// One thing the profile knows how to recreate.
///
/// Directories and symlinks are here because observing an open of `/bin/sh`
/// implies `/bin` must exist and `/bin/sh` may be a symlink to `busybox`;
/// replaying only the regular files would produce a rootfs that cannot resolve
/// the path it was built for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Entry {
    Dir { mode: u32 },
    File { digest: String, mode: u32 },
    Symlink { target: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub pipeline: String,
    pub image: String,
    /// How many runs have contributed observations. Useful when reading a
    /// profile by hand to judge how settled it is.
    pub runs: u32,
    #[serde(default)]
    pub entries: BTreeMap<String, Entry>,
}

impl Profile {
    pub fn new(pipeline: &str, image: &str) -> Self {
        Self {
            pipeline: pipeline.to_string(),
            image: image.to_string(),
            runs: 0,
            entries: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Number of entries that carry file contents, i.e. what the cache has to
    /// hold for a warm run to succeed.
    pub fn file_count(&self) -> usize {
        self.entries
            .values()
            .filter(|e| matches!(e, Entry::File { .. }))
            .count()
    }

    pub fn insert(&mut self, path: String, entry: Entry) {
        self.entries.insert(path, entry);
    }

    /// Digests the profile depends on. The safety net checks these against the
    /// cache before trusting a warm build.
    pub fn required_digests(&self) -> Vec<&str> {
        self.entries
            .values()
            .filter_map(|e| match e {
                Entry::File { digest, .. } => Some(digest.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        match fs::read(path) {
            Ok(bytes) => {
                let profile = serde_json::from_slice(&bytes)
                    .with_context(|| format!("failed to parse profile {}", path.display()))?;
                Ok(Some(profile))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => {
                Err(e).with_context(|| format!("failed to read profile {}", path.display()))
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("failed to rename into {}", path.display()))?;
        Ok(())
    }
}

pub fn profile_path(pipeline: &str) -> Result<PathBuf> {
    Ok(cache_home()?
        .join("profiles")
        .join(format!("{}.json", sanitize(pipeline))))
}

/// Keeps a pipeline name from reaching outside the profile directory.
fn sanitize(pipeline: &str) -> String {
    pipeline
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{sanitize, Entry, Profile};
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mepul_pr_{}_{}.json", std::process::id(), name))
    }

    fn sample() -> Profile {
        let mut profile = Profile::new("build", "docker.io/library/alpine:latest");
        profile.insert("/bin".into(), Entry::Dir { mode: 0o755 });
        profile.insert(
            "/bin/busybox".into(),
            Entry::File {
                digest: "sha256:aa".into(),
                mode: 0o755,
            },
        );
        profile.insert(
            "/bin/sh".into(),
            Entry::Symlink {
                target: "busybox".into(),
            },
        );
        profile
    }

    #[test]
    fn new_profile_is_empty() {
        let profile = Profile::new("build", "alpine");

        assert!(profile.is_empty());
        assert_eq!(profile.runs, 0);
    }

    #[test]
    fn entries_iterate_parents_before_children() {
        let mut profile = Profile::new("build", "alpine");
        profile.insert(
            "/bin/sh".into(),
            Entry::File {
                digest: "sha256:aa".into(),
                mode: 0o755,
            },
        );
        profile.insert("/bin".into(), Entry::Dir { mode: 0o755 });

        let paths: Vec<_> = profile.entries.keys().cloned().collect();

        assert_eq!(paths, vec!["/bin".to_string(), "/bin/sh".to_string()]);
    }

    #[test]
    fn repeated_observations_merge_instead_of_duplicating() {
        let mut profile = Profile::new("build", "alpine");
        profile.insert("/bin".into(), Entry::Dir { mode: 0o755 });
        profile.insert("/bin".into(), Entry::Dir { mode: 0o755 });

        assert_eq!(profile.len(), 1);
    }

    #[test]
    fn file_count_ignores_dirs_and_symlinks() {
        let profile = sample();

        assert_eq!(profile.len(), 3);
        assert_eq!(profile.file_count(), 1);
    }

    #[test]
    fn required_digests_lists_only_file_contents() {
        let profile = sample();

        assert_eq!(profile.required_digests(), vec!["sha256:aa"]);
    }

    #[test]
    fn save_then_load_round_trips() {
        let path = scratch("roundtrip");
        let profile = sample();

        profile.save(&path).unwrap();
        let loaded = Profile::load(&path).unwrap().unwrap();

        std::fs::remove_file(&path).ok();
        assert_eq!(loaded.entries, profile.entries);
        assert_eq!(loaded.image, profile.image);
    }

    #[test]
    fn load_returns_none_when_profile_absent() {
        let path = scratch("absent");

        let loaded = Profile::load(&path).unwrap();

        assert!(loaded.is_none());
    }

    #[test]
    fn sanitize_strips_path_separators() {
        assert_eq!(sanitize("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize("build-job_1.2"), "build-job_1.2");
    }
}
