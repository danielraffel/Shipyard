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
//!
//! The cache lives in Shipyard's protected state tree, so every write (create
//! the directory, store an entry, prune) holds the production writer-domain
//! lease the sandbox audit waits on. When the lease cannot be had, the write is
//! skipped and the cache stops writing for the rest of the run: caching is an
//! optimisation, and a cold read is always correct.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::writer_domain_lease::{ProductionWriterDomainLease, acquire_for_protected_path};

/// Takes the writer-domain lease for a write under `path`.
type LeaseFn = fn(&Path) -> io::Result<Option<ProductionWriterDomainLease>>;

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
#[derive(Debug)]
pub struct ReadCache {
    dir: Option<PathBuf>,
    github: AtomicU64,
    cached: AtomicU64,
    lease: LeaseFn,
    /// Set once a write could not take the lease; later writes are skipped
    /// rather than each waiting out the lease timeout again.
    writes_refused: AtomicBool,
}

impl Default for ReadCache {
    fn default() -> Self {
        Self {
            dir: None,
            github: AtomicU64::new(0),
            cached: AtomicU64::new(0),
            lease: acquire_for_protected_path,
            writes_refused: AtomicBool::new(false),
        }
    }
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
        Self::open_with_lease(dir, now, acquire_for_protected_path)
    }

    fn open_with_lease(dir: &Path, now: SystemTime, lease: LeaseFn) -> Self {
        let cache = Self {
            lease,
            ..Self::default()
        };
        if !dir.is_dir() {
            let Ok(_lease) = cache.lease(dir) else {
                return cache;
            };
            if fs::create_dir_all(dir).is_err() {
                return cache;
            }
        }
        cache.prune(dir, now);
        Self {
            dir: Some(dir.to_path_buf()),
            ..cache
        }
    }

    /// The writer-domain lease for a write under `path`. `Err` means the
    /// write must be skipped; `Ok(None)` means the path needs no lease.
    fn lease(&self, path: &Path) -> Result<Option<ProductionWriterDomainLease>, ()> {
        if self.writes_refused.load(Ordering::Relaxed) {
            return Err(());
        }
        (self.lease)(path).map_err(|_| {
            self.writes_refused.store(true, Ordering::Relaxed);
        })
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
        let Ok(_lease) = self.lease(dir) else {
            return;
        };
        let Ok(mut file) = tempfile::NamedTempFile::new_in(dir) else {
            return;
        };
        if file.write_all(body.as_bytes()).is_ok() {
            let _ = file.persist(&path);
        }
    }
}

impl ReadCache {
    fn prune(&self, dir: &Path, now: SystemTime) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let stale: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| {
                entry.path().extension().and_then(|ext| ext.to_str()) == Some("json")
                    && entry
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| now.duration_since(modified).ok())
                        .is_some_and(|age| age > MAX_ENTRY_AGE)
            })
            .map(|entry| entry.path())
            .collect();
        // Reading the directory needs no lease; only a removal is a write.
        if stale.is_empty() {
            return;
        }
        let Ok(_lease) = self.lease(dir) else {
            return;
        };
        for path in stale {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod lease_tests {
    use std::io;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use serde_json::json;

    use super::{MAX_ENTRY_AGE, ProductionWriterDomainLease, ReadCache};

    fn refused(_: &Path) -> io::Result<Option<ProductionWriterDomainLease>> {
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "sandbox audit holds the domain",
        ))
    }

    #[allow(clippy::unnecessary_wraps)] // must match the lease function type
    fn granted(_: &Path) -> io::Result<Option<ProductionWriterDomainLease>> {
        Ok(None)
    }

    #[test]
    fn without_the_lease_the_cache_writes_nothing() {
        let root = tempfile::tempdir().expect("temp");
        let now = SystemTime::now();

        // Absent directory: not created.
        let missing = root.path().join("gate-cost-cache");
        let cache = ReadCache::open_with_lease(&missing, now, refused);
        cache.put("k", &json!(1));
        assert!(
            !missing.exists(),
            "created the cache directory without the lease"
        );

        // Existing directory: no entry stored, and a stale entry is not pruned.
        let dir = root.path().join("existing");
        std::fs::create_dir(&dir).expect("dir");
        let stale = dir.join("old.json");
        std::fs::write(&stale, "{}").expect("stale");
        let old = now + MAX_ENTRY_AGE + Duration::from_secs(60);
        let cache = ReadCache::open_with_lease(&dir, old, refused);
        cache.put("k", &json!(1));
        assert!(stale.exists(), "pruned without the lease");
        let entries: Vec<_> = std::fs::read_dir(&dir).expect("read").flatten().collect();
        assert_eq!(
            entries.len(),
            1,
            "stored an entry without the lease: {entries:?}"
        );
        assert_eq!(cache.get("k"), None);
    }

    #[test]
    fn with_the_lease_the_same_calls_write() {
        // Control for the test above: the identical sequence does create,
        // store and prune when the lease is granted.
        let root = tempfile::tempdir().expect("temp");
        let now = SystemTime::now();
        let missing = root.path().join("gate-cost-cache");
        let cache = ReadCache::open_with_lease(&missing, now, granted);
        cache.put("k", &json!(1));
        assert_eq!(cache.get("k"), Some(json!(1)));

        let dir = root.path().join("existing");
        std::fs::create_dir(&dir).expect("dir");
        let stale = dir.join("old.json");
        std::fs::write(&stale, "{}").expect("stale");
        let old = now + MAX_ENTRY_AGE + Duration::from_secs(60);
        let _ = ReadCache::open_with_lease(&dir, old, granted);
        assert!(!stale.exists());
    }
}
