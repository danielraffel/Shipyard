//! Disk cache for the GitHub reads a gate-cost window repeats on every run.
//!
//! A 66-hour window over a busy repository is well over a thousand GitHub
//! reads, and nearly all of them are answers that can no longer change: the
//! jobs of a completed run attempt, the annotations of a completed check run,
//! and the parents of a commit. Only those are cached. Anything still in
//! flight is read live every time, so a cached report never disagrees with a
//! live one about a finished fact.
//!
//! Keys name exactly the immutable identity of the answer (run id *and* run
//! attempt for jobs, because a re-run adds jobs to the same run id). Entries
//! are one JSON file each, written atomically, and pruned by age so the
//! directory cannot grow without bound.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Entries unused for longer than this are removed when the cache opens.
pub const MAX_ENTRY_AGE: Duration = Duration::from_hours(35 * 24);

/// Where the reads behind one report came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ReadStats {
    /// Requests sent to GitHub.
    pub github: u64,
    /// Answers served from the disk cache instead.
    pub cached: u64,
}

/// Cache of immutable GitHub answers. A disabled cache stores nothing and
/// still counts reads, so every report states how many requests it cost.
#[derive(Debug, Default)]
pub struct ReadCache {
    dir: Option<PathBuf>,
    github: AtomicU64,
    cached: AtomicU64,
}

impl ReadCache {
    /// A cache that never stores or serves anything.
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Open (creating if needed) a cache rooted at `dir`, pruning entries
    /// older than [`MAX_ENTRY_AGE`] relative to `now`. A directory that cannot
    /// be created yields a disabled cache: caching is an optimisation and must
    /// never fail the report.
    #[must_use]
    pub fn open(dir: &Path, now: SystemTime) -> Self {
        if fs::create_dir_all(dir).is_err() {
            return Self::disabled();
        }
        prune(dir, now);
        Self {
            dir: Some(dir.to_path_buf()),
            ..Self::default()
        }
    }

    /// Record one request sent to GitHub.
    pub fn note_github_read(&self) {
        self.github.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts so far.
    #[must_use]
    pub fn stats(&self) -> ReadStats {
        ReadStats {
            github: self.github.load(Ordering::Relaxed),
            cached: self.cached.load(Ordering::Relaxed),
        }
    }

    fn entry_path(&self, key: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        let digest = hex::encode(Sha256::digest(key.as_bytes()));
        Some(dir.join(format!("{digest}.json")))
    }

    /// The cached answer for `key`, if one was stored under exactly that key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Value> {
        let path = self.entry_path(key)?;
        let text = fs::read_to_string(&path).ok()?;
        let entry: Value = serde_json::from_str(&text).ok()?;
        // The key is stored beside the value so a digest collision, or a file
        // written by an older layout, is a miss rather than a wrong answer.
        if entry.get("key").and_then(Value::as_str) != Some(key) {
            return None;
        }
        let value = entry.get("value")?.clone();
        self.cached.fetch_add(1, Ordering::Relaxed);
        Some(value)
    }

    /// Store an answer the caller has established can no longer change.
    pub fn put(&self, key: &str, value: &Value) {
        let Some(path) = self.entry_path(key) else {
            return;
        };
        let Some(dir) = path.parent() else {
            return;
        };
        let body = serde_json::json!({"key": key, "value": value}).to_string();
        let Ok(mut file) = tempfile::NamedTempFile::new_in(dir) else {
            return;
        };
        if file.write_all(body.as_bytes()).is_ok() {
            let _ = file.persist(&path);
        }
    }
}

fn prune(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > MAX_ENTRY_AGE);
        if stale {
            let _ = fs::remove_file(path);
        }
    }
}
