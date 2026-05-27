//! Warm-pool runner reuse state.
//!
//! The warm pool is an opt-in cache of runner workdirs keyed by
//! `(target, host)`. It only reuses entries for the exact same SHA and
//! only for backends whose workdir survives a Shipyard invocation.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

/// Canonical warm-pool JSON filename.
pub const DEFAULT_POOL_FILENAME: &str = "warm_pool.json";

const ELIGIBLE_BACKENDS: [&str; 3] = ["ssh", "ssh-windows", "local"];

/// A single warm-pool record.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PoolEntry {
    /// Target name.
    pub target: String,
    /// Host key, normally SSH host or `local`.
    pub host: String,
    /// Backend that warmed the workdir.
    pub backend: String,
    /// Runner workdir path.
    pub workdir: String,
    /// Exact SHA this entry is valid for.
    pub sha: String,
    /// UNIX epoch seconds when the entry expires.
    pub expires_at: f64,
    /// UNIX epoch seconds when the entry was created.
    pub created_at: f64,
}

impl PoolEntry {
    /// Construct a warm-pool entry.
    #[must_use]
    pub fn new(
        target: impl Into<String>,
        host: impl Into<String>,
        backend: impl Into<String>,
        workdir: impl Into<String>,
        sha: impl Into<String>,
        expires_at: f64,
        created_at: f64,
    ) -> Self {
        Self {
            target: target.into(),
            host: host.into(),
            backend: backend.into(),
            workdir: workdir.into(),
            sha: sha.into(),
            expires_at,
            created_at,
        }
    }

    /// Return true if `now` is at or past expiry.
    #[must_use]
    pub fn is_expired(&self, now: f64) -> bool {
        now >= self.expires_at
    }

    /// Return seconds until expiry, clamped at zero.
    #[must_use]
    pub fn ttl_remaining_secs(&self, now: f64) -> f64 {
        (self.expires_at - now).max(0.0)
    }
}

/// JSON-backed persistent warm-pool store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WarmPool {
    path: PathBuf,
}

impl WarmPool {
    /// Construct a store at an explicit path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Return the backing JSON path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return every valid entry currently in the pool, including expired entries.
    #[must_use]
    pub fn all_entries(&self) -> Vec<PoolEntry> {
        self.load_entries_unlocked()
    }

    /// Mutate the pool entries while holding the warm-pool file lock.
    pub fn with_entries_locked<T>(
        &self,
        f: impl FnOnce(&mut Vec<PoolEntry>) -> Result<T, io::Error>,
    ) -> Result<T, io::Error> {
        let _lock = StoreLock::acquire(self.lock_path())?;
        let mut entries = self.load_entries_unlocked();
        let output = f(&mut entries)?;
        self.save_entries_unlocked(&entries)?;
        Ok(output)
    }

    fn load_entries_unlocked(&self) -> Vec<PoolEntry> {
        self.load_raw()
            .into_iter()
            .filter_map(|value| serde_json::from_value(value).ok())
            .collect()
    }

    /// Atomically rewrite the pool file with the given entries.
    pub fn save_entries(&self, entries: &[PoolEntry]) -> Result<(), std::io::Error> {
        let _lock = StoreLock::acquire(self.lock_path())?;
        self.save_entries_unlocked(entries)
    }

    fn save_entries_unlocked(&self, entries: &[PoolEntry]) -> Result<(), std::io::Error> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = serde_json::json!({ "entries": entries });
        let tmp = self.path.with_extension(format!(
            "{}tmp",
            self.path
                .extension()
                .and_then(|extension| extension.to_str())
                .map_or_else(String::new, |extension| format!("{extension}."))
        ));
        fs::write(
            &tmp,
            serde_json::to_string_pretty(&payload).expect("pool serializes") + "\n",
        )?;
        fs::rename(tmp, &self.path)
    }

    /// Return the unexpired entry for `(target, host)`, if any.
    #[must_use]
    pub fn get(&self, target: &str, host: &str, now: f64) -> Option<PoolEntry> {
        self.all_entries()
            .into_iter()
            .find(|entry| entry.target == target && entry.host == host)
            .filter(|entry| !entry.is_expired(now))
    }

    /// Insert or replace the record for `(target, host)`.
    pub fn upsert(&self, entry: PoolEntry) -> Result<(), std::io::Error> {
        self.with_entries_locked(|entries| {
            entries
                .retain(|existing| existing.target != entry.target || existing.host != entry.host);
            entries.push(entry);
            Ok(())
        })
    }

    /// Remove the entry for `(target, host)`. Returns true if removed.
    pub fn evict(&self, target: &str, host: &str) -> Result<bool, std::io::Error> {
        self.with_entries_locked(|entries| {
            let original_len = entries.len();
            entries.retain(|entry| entry.target != target || entry.host != host);
            Ok(entries.len() != original_len)
        })
    }

    /// Remove every entry. Returns count drained.
    pub fn drain(&self) -> Result<usize, std::io::Error> {
        self.with_entries_locked(|entries| {
            let count = entries.len();
            entries.clear();
            Ok(count)
        })
    }

    /// Remove expired entries. Returns count pruned.
    pub fn prune_expired(&self, now: f64) -> Result<usize, std::io::Error> {
        self.with_entries_locked(|entries| {
            let mut pruned = 0;
            entries.retain(|entry| {
                if entry.is_expired(now) {
                    pruned += 1;
                    false
                } else {
                    true
                }
            });
            Ok(pruned)
        })
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.path.clone();
        path.set_extension("lock");
        path
    }

    fn load_raw(&self) -> Vec<serde_json::Value> {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Vec::new();
        };
        payload
            .get("entries")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(serde_json::Value::is_object)
            .collect()
    }
}

#[derive(Debug)]
struct StoreLock {
    file: File,
}

impl StoreLock {
    fn acquire(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.lock_exclusive()?;
        Ok(Self { file })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Canonical location for the warm-pool JSON file.
#[must_use]
pub fn default_pool_path(state_dir: &Path) -> PathBuf {
    state_dir.join(DEFAULT_POOL_FILENAME)
}

/// Return true when a warm-pool env value disables reuse.
#[must_use]
pub fn warm_reuse_disabled_by_env_value(value: Option<&str>) -> bool {
    value.unwrap_or_default().trim().eq_ignore_ascii_case("1")
        || matches!(
            value
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "true" | "yes" | "on"
        )
}

/// Extract `warm_keepalive_seconds`; invalid or non-positive values map to zero.
#[must_use]
pub fn extract_warm_keepalive_seconds(value: Option<&toml::Value>) -> u32 {
    let Some(value) = value else {
        return 0;
    };
    let parsed = value.as_integer().or_else(|| {
        value
            .as_str()
            .and_then(|raw| raw.trim().parse::<i64>().ok())
    });
    parsed
        .and_then(|seconds| u32::try_from(seconds).ok())
        .unwrap_or(0)
}

/// Whether `backend` is a type where reuse makes sense.
#[must_use]
pub fn is_backend_eligible(backend: &str) -> bool {
    let normalized = backend.trim().to_ascii_lowercase().replace('_', "-");
    ELIGIBLE_BACKENDS.contains(&normalized.as_str())
}

/// Pick a stable host key for pool indexing.
#[must_use]
pub fn warm_host_key(host: Option<&str>) -> String {
    host.map(str::trim)
        .filter(|host| !host.is_empty())
        .map_or_else(|| "local".to_owned(), ToOwned::to_owned)
}

/// Return the absolute expiry time for a new pool entry.
#[must_use]
pub fn compute_expires_at(keepalive_seconds: u32, now: f64) -> f64 {
    now + f64::from(keepalive_seconds)
}

/// Return current UNIX epoch seconds.
#[must_use]
pub fn now_epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

/// Render entries keyed by `(target, host)` for status surfaces.
#[must_use]
pub fn entries_by_key(entries: &[PoolEntry]) -> BTreeMap<(String, String), PoolEntry> {
    entries
        .iter()
        .cloned()
        .map(|entry| ((entry.target.clone(), entry.host.clone()), entry))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::{
        PoolEntry, WarmPool, compute_expires_at, default_pool_path, entries_by_key,
        extract_warm_keepalive_seconds, is_backend_eligible, warm_host_key,
        warm_reuse_disabled_by_env_value,
    };

    fn entry(target: &str, host: &str, sha: &str, expires_at: f64) -> PoolEntry {
        PoolEntry::new(target, host, "ssh", "/repo", sha, expires_at, 10.0)
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < f64::EPSILON,
            "{actual} != {expected}"
        );
    }

    #[test]
    fn pool_entry_expiry_and_ttl_match_python_contract() {
        let record = entry("ubuntu", "vm", "abc", 20.0);
        assert!(!record.is_expired(19.9));
        assert!(record.is_expired(20.0));
        assert_close(record.ttl_remaining_secs(15.0), 5.0);
        assert_close(record.ttl_remaining_secs(25.0), 0.0);
    }

    #[test]
    fn warm_pool_missing_or_corrupt_file_reads_empty() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = WarmPool::new(temp.path().join("warm_pool.json"));
        assert!(pool.all_entries().is_empty());

        std::fs::write(pool.path(), "{not-json").expect("write");
        assert!(pool.all_entries().is_empty());
    }

    #[test]
    fn warm_pool_roundtrips_and_filters_malformed_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = WarmPool::new(temp.path().join("state").join("warm_pool.json"));
        pool.save_entries(&[entry("ubuntu", "vm", "abc", 100.0)])
            .expect("save");
        assert_eq!(pool.all_entries().len(), 1);
        assert_eq!(pool.get("ubuntu", "vm", 50.0).expect("entry").sha, "abc");

        std::fs::write(
            pool.path(),
            r#"{"entries":[{"target":"bad"},42,{"target":"mac","host":"local","backend":"local","workdir":"/repo","sha":"def","expires_at":100,"created_at":10}]}"#,
        )
        .expect("write malformed");
        let entries = pool.all_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target, "mac");
    }

    #[test]
    fn upsert_replaces_by_target_and_host() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = WarmPool::new(temp.path().join("warm_pool.json"));
        pool.upsert(entry("ubuntu", "vm", "old", 100.0))
            .expect("upsert old");
        pool.upsert(entry("ubuntu", "vm", "new", 100.0))
            .expect("upsert new");
        pool.upsert(entry("ubuntu", "other", "other", 100.0))
            .expect("upsert other");

        let entries = pool.all_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(pool.get("ubuntu", "vm", 1.0).expect("entry").sha, "new");
    }

    #[test]
    fn get_ignores_expired_without_pruning() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = WarmPool::new(temp.path().join("warm_pool.json"));
        pool.save_entries(&[entry("ubuntu", "vm", "abc", 5.0)])
            .expect("save");

        assert!(pool.get("ubuntu", "vm", 5.0).is_none());
        assert_eq!(pool.all_entries().len(), 1);
    }

    #[test]
    fn evict_drain_and_prune_mutate_store() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = WarmPool::new(temp.path().join("warm_pool.json"));
        pool.save_entries(&[
            entry("ubuntu", "vm", "abc", 5.0),
            entry("mac", "local", "def", 50.0),
        ])
        .expect("save");

        assert_eq!(pool.prune_expired(10.0).expect("prune"), 1);
        assert_eq!(pool.all_entries()[0].target, "mac");
        assert!(pool.evict("mac", "local").expect("evict"));
        assert!(!pool.evict("mac", "local").expect("evict missing"));
        pool.upsert(entry("ubuntu", "vm", "abc", 100.0))
            .expect("upsert");
        assert_eq!(pool.drain().expect("drain"), 1);
        assert!(pool.all_entries().is_empty());
    }

    #[test]
    fn warm_pool_lock_preserves_two_handle_mutations() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = Arc::new(WarmPool::new(temp.path().join("warm_pool.json")));
        let writer = Arc::clone(&pool);
        let (started_tx, started_rx) = mpsc::channel();

        let handle = pool
            .with_entries_locked(|entries| {
                let handle = thread::spawn(move || {
                    started_tx.send(()).expect("started");
                    writer
                        .upsert(entry("linux", "vm", "def", 100.0))
                        .expect("upsert linux");
                });
                started_rx.recv().expect("started received");
                thread::sleep(Duration::from_millis(50));
                entries.push(entry("mac", "local", "abc", 100.0));
                Ok(handle)
            })
            .expect("locked mutation");
        handle.join().expect("writer thread");

        assert_eq!(pool.get("mac", "local", 1.0).expect("mac").sha, "abc");
        assert_eq!(pool.get("linux", "vm", 1.0).expect("linux").sha, "def");
    }

    #[test]
    fn helper_contracts_match_python_defaults() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            default_pool_path(temp.path()),
            temp.path().join("warm_pool.json")
        );
        for value in ["1", "true", "TRUE", "yes", "on"] {
            assert!(warm_reuse_disabled_by_env_value(Some(value)));
        }
        assert!(!warm_reuse_disabled_by_env_value(Some("0")));

        assert_eq!(
            extract_warm_keepalive_seconds(Some(&toml::Value::Integer(30))),
            30
        );
        assert_eq!(
            extract_warm_keepalive_seconds(Some(&toml::Value::String("45".to_owned()))),
            45
        );
        assert_eq!(
            extract_warm_keepalive_seconds(Some(&toml::Value::Integer(-1))),
            0
        );
        assert_eq!(
            extract_warm_keepalive_seconds(Some(&toml::Value::String("bad".to_owned()))),
            0
        );

        assert!(is_backend_eligible("ssh"));
        assert!(is_backend_eligible("ssh_windows"));
        assert!(is_backend_eligible("local"));
        assert!(!is_backend_eligible("cloud"));
        assert_eq!(warm_host_key(Some(" vm ")), "vm");
        assert_eq!(warm_host_key(None), "local");
        assert_close(compute_expires_at(30, 10.0), 40.0);
    }

    #[test]
    fn entries_by_key_keeps_latest_for_key() {
        let map = entries_by_key(&[
            entry("ubuntu", "vm", "old", 10.0),
            entry("ubuntu", "vm", "new", 20.0),
        ]);
        assert_eq!(map.len(), 1);
        assert_eq!(map[&("ubuntu".to_owned(), "vm".to_owned())].sha, "new");
    }
}
