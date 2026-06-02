use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

/// Schema version for durable ship-state files.
pub const SHIP_STATE_SCHEMA_VERSION: u32 = 1;

/// A single dispatched run tracked as part of an in-flight ship.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DispatchedRun {
    /// Target name associated with the run.
    pub target: String,
    /// Provider or backend label.
    pub provider: String,
    /// Provider-specific run identifier.
    #[serde(deserialize_with = "deserialize_run_id")]
    pub run_id: String,
    /// Latest observed status.
    pub status: String,
    /// Timestamp when the run started.
    pub started_at: DateTime<Utc>,
    /// Timestamp when the run was last updated.
    pub updated_at: DateTime<Utc>,
    /// Attempt number for reruns.
    #[serde(default = "default_attempt")]
    pub attempt: u32,
    /// Optional last heartbeat timestamp for stale-run detection.
    #[serde(default)]
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Optional current phase name.
    #[serde(default)]
    pub phase: Option<String>,
    /// Whether this lane is merge-blocking.
    #[serde(default = "default_true")]
    pub required: bool,
}

impl DispatchedRun {
    /// Convert this run into the JSON shape emitted by the Python CLI.
    #[must_use]
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("DispatchedRun must serialize")
    }
}

/// Durable state for a single in-flight PR ship.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ShipState {
    /// Schema version for this state file.
    #[serde(default = "default_ship_state_schema_version")]
    pub schema_version: u32,
    /// Pull request number.
    pub pr: u64,
    /// Repository slug.
    pub repo: String,
    /// Absolute path to the repo checkout root (the dir that contains
    /// `.shipyard/`). Lets a GUI run repo-scoped config commands in the right
    /// directory. Empty (`""`) for legacy records written before this field.
    #[serde(default)]
    pub repo_root: String,
    /// Head branch name.
    pub branch: String,
    /// Base branch name.
    pub base_branch: String,
    /// Recorded head SHA.
    pub head_sha: String,
    /// Merge-policy signature captured at dispatch time.
    #[serde(default)]
    pub policy_signature: String,
    /// Optional PR URL for self-describing state output.
    #[serde(default)]
    pub pr_url: String,
    /// Optional PR title for self-describing state output.
    #[serde(default)]
    pub pr_title: String,
    /// Optional commit subject for self-describing state output.
    #[serde(default)]
    pub commit_subject: String,
    /// Recorded remote runs.
    #[serde(default)]
    pub dispatched_runs: Vec<DispatchedRun>,
    /// Snapshot of evidence statuses by target.
    #[serde(default)]
    pub evidence_snapshot: BTreeMap<String, String>,
    /// Attempt number for this PR ship.
    #[serde(default = "default_attempt")]
    pub attempt: u32,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
}

impl ShipState {
    /// Construct a new in-flight ship state.
    #[must_use]
    pub fn new(
        pr: u64,
        repo: impl Into<String>,
        branch: impl Into<String>,
        base_branch: impl Into<String>,
        head_sha: impl Into<String>,
        policy_signature: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            schema_version: SHIP_STATE_SCHEMA_VERSION,
            pr,
            repo: repo.into(),
            repo_root: String::new(),
            branch: branch.into(),
            base_branch: base_branch.into(),
            head_sha: head_sha.into(),
            policy_signature: policy_signature.into(),
            pr_url: String::new(),
            pr_title: String::new(),
            commit_subject: String::new(),
            dispatched_runs: Vec::new(),
            evidence_snapshot: BTreeMap::new(),
            attempt: default_attempt(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Update the `updated_at` timestamp.
    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    /// Insert or replace a run by `(target, run_id)`.
    pub fn upsert_run(&mut self, run: DispatchedRun) {
        if let Some(existing) = self
            .dispatched_runs
            .iter_mut()
            .find(|existing| existing.target == run.target && existing.run_id == run.run_id)
        {
            *existing = run;
        } else {
            self.dispatched_runs.push(run);
        }
        self.touch();
    }

    /// Return the most recently updated run for a target.
    #[must_use]
    pub fn get_run(&self, target: &str) -> Option<&DispatchedRun> {
        self.dispatched_runs
            .iter()
            .filter(|run| run.target == target)
            .max_by_key(|run| run.updated_at)
    }

    /// Return whether any run already exists for a target.
    #[must_use]
    pub fn has_target(&self, target: &str) -> bool {
        self.dispatched_runs.iter().any(|run| run.target == target)
    }

    /// Append a new run without deduplication.
    pub fn append_run(&mut self, run: DispatchedRun) {
        self.dispatched_runs.push(run);
        self.touch();
    }

    /// Update the saved evidence status for a target.
    pub fn update_evidence(&mut self, target: impl Into<String>, status: impl Into<String>) {
        self.evidence_snapshot.insert(target.into(), status.into());
        self.touch();
    }

    /// Return whether the recorded head SHA differs from the current SHA.
    #[must_use]
    pub fn is_sha_drift(&self, current_sha: &str) -> bool {
        current_sha != self.head_sha
    }
}

/// Report describing what a prune operation removed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PruneReport {
    /// Deleted active PR numbers.
    pub deleted_active: Vec<u64>,
    /// Deleted archived filenames.
    pub deleted_archived: Vec<String>,
}

impl PruneReport {
    /// Total number of deleted entries.
    #[must_use]
    pub fn total(&self) -> usize {
        self.deleted_active.len() + self.deleted_archived.len()
    }
}

/// Persistent store for active and archived ship-state files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShipStateStore {
    path: PathBuf,
}

impl ShipStateStore {
    /// Open a state store at the given path.
    pub fn new(path: PathBuf) -> Result<Self, std::io::Error> {
        fs::create_dir_all(&path)?;
        fs::create_dir_all(path.join("archive"))?;
        Ok(Self { path })
    }

    /// Backing path of the store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the active state path for a PR.
    #[must_use]
    pub fn state_path(&self, pr: u64) -> PathBuf {
        self.path.join(format!("{pr}.json"))
    }

    /// Return the archive directory path.
    #[must_use]
    pub fn archive_dir(&self) -> PathBuf {
        self.path.join("archive")
    }

    /// Load an active state for a PR.
    #[must_use]
    pub fn get(&self, pr: u64) -> Option<ShipState> {
        let lock = self.lock_pr(pr).ok()?;
        self.get_locked(pr, &lock)
    }

    /// Acquire the per-PR ship-state lock.
    pub fn lock_pr(&self, pr: u64) -> io::Result<ShipStatePrLock> {
        ShipStatePrLock::acquire(self.lock_path(pr))
    }

    /// Load an active state while the caller holds the per-PR lock.
    #[must_use]
    pub fn get_locked(&self, pr: u64, _lock: &ShipStatePrLock) -> Option<ShipState> {
        self.get_unlocked(pr)
    }

    /// Mutate one PR's state while holding that PR's lock.
    pub fn with_pr_state_locked<T>(
        &self,
        pr: u64,
        f: impl FnOnce(&mut Option<ShipState>) -> Result<T, Box<dyn std::error::Error>>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let lock = self.lock_pr(pr)?;
        let mut state = self.get_unlocked(pr);
        let output = f(&mut state)?;
        if let Some(state) = state {
            self.save_locked(&state, &lock)?;
        } else {
            let path = self.state_path(pr);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        Ok(output)
    }

    fn get_unlocked(&self, pr: u64) -> Option<ShipState> {
        let path = self.state_path(pr);
        let contents = fs::read_to_string(path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    /// Save a state atomically.
    pub fn save(&self, state: &ShipState) -> Result<(), Box<dyn std::error::Error>> {
        let lock = self.lock_pr(state.pr)?;
        self.save_locked(state, &lock)
    }

    /// Save a state atomically while the caller holds the per-PR lock.
    pub fn save_locked(
        &self,
        state: &ShipState,
        _lock: &ShipStatePrLock,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let payload = serde_json::to_string_pretty(state)?;
        let temp = tempfile::NamedTempFile::new_in(&self.path)?;
        fs::write(temp.path(), format!("{payload}\n"))?;
        temp.persist(self.state_path(state.pr))?;
        Ok(())
    }

    /// Delete an active state file.
    pub fn delete(&self, pr: u64) -> Result<(), std::io::Error> {
        let _lock = self.lock_pr(pr)?;
        let path = self.state_path(pr);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Move an active state into the archive directory.
    pub fn archive(&self, pr: u64) -> Result<Option<PathBuf>, std::io::Error> {
        let lock = self.lock_pr(pr)?;
        self.archive_locked(pr, &lock)
    }

    /// Move an active state into the archive directory while holding the lock.
    pub fn archive_locked(
        &self,
        pr: u64,
        _lock: &ShipStatePrLock,
    ) -> Result<Option<PathBuf>, std::io::Error> {
        let source = self.state_path(pr);
        if !source.exists() {
            return Ok(None);
        }
        let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
        let dest = self.archive_dir().join(format!("{pr}-{stamp}.json"));
        fs::rename(source, &dest)?;
        Ok(Some(dest))
    }

    /// Return active states sorted by PR number.
    pub fn list_active(&self) -> Vec<ShipState> {
        let mut states = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.parent() == Some(self.archive_dir().as_path()) {
                    continue;
                }
                if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                    continue;
                }
                if !path
                    .file_stem()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|stem| stem.chars().all(|ch| ch.is_ascii_digit()))
                {
                    continue;
                }
                if let Ok(contents) = fs::read_to_string(&path)
                    && let Ok(state) = serde_json::from_str::<ShipState>(&contents)
                {
                    states.push(state);
                }
            }
        }
        states.sort_by_key(|state| state.pr);
        states
    }

    /// Return archived state file paths sorted by filename.
    pub fn list_archived(&self) -> Vec<PathBuf> {
        let mut archived = Vec::new();
        if let Ok(entries) = fs::read_dir(self.archive_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(std::ffi::OsStr::to_str) == Some("json") {
                    archived.push(path);
                }
            }
        }
        archived.sort();
        archived
    }

    /// Archive the current state for a PR and create a fresh attempt record.
    pub fn archive_and_replace(
        &self,
        state: &ShipState,
        new_attempt: Option<u32>,
    ) -> Result<ShipState, Box<dyn std::error::Error>> {
        let lock = self.lock_pr(state.pr)?;
        self.archive_and_replace_locked(state, new_attempt, &lock)
    }

    /// Archive the current state and create a fresh attempt while holding the lock.
    pub fn archive_and_replace_locked(
        &self,
        state: &ShipState,
        new_attempt: Option<u32>,
        lock: &ShipStatePrLock,
    ) -> Result<ShipState, Box<dyn std::error::Error>> {
        let _ = self.archive_locked(state.pr, lock)?;
        let now = Utc::now();
        Ok(ShipState {
            attempt: new_attempt.unwrap_or(state.attempt + 1),
            dispatched_runs: Vec::new(),
            evidence_snapshot: BTreeMap::new(),
            created_at: now,
            updated_at: now,
            ..state.clone()
        })
    }

    fn lock_path(&self, pr: u64) -> PathBuf {
        self.path.join(format!("{pr}.lock"))
    }
}

/// Held per-PR ship-state lock.
#[derive(Debug)]
pub struct ShipStatePrLock {
    file: File,
}

impl ShipStatePrLock {
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

impl Drop for ShipStatePrLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Compute a stable digest of merge-policy inputs.
#[must_use]
pub fn compute_policy_signature(
    required_platforms: &[String],
    target_names: &[String],
    mode: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"platforms:");
    let mut platforms = required_platforms.to_vec();
    platforms.sort();
    for platform in platforms {
        hasher.update(platform.as_bytes());
        hasher.update([0]);
    }
    hasher.update(b"targets:");
    let mut targets = target_names.to_vec();
    targets.sort();
    for target in targets {
        hasher.update(target.as_bytes());
        hasher.update([0]);
    }
    hasher.update(b"mode:");
    hasher.update(mode.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..8])
}

fn default_attempt() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

fn default_ship_state_schema_version() -> u32 {
    SHIP_STATE_SCHEMA_VERSION
}

fn deserialize_run_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(text) => Ok(text),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "run_id must be a string or number, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration as StdDuration;

    use chrono::{Duration, TimeZone, Utc};

    use super::{DispatchedRun, ShipState, ShipStateStore, compute_policy_signature};

    fn sample_state(pr: u64, sha: &str) -> ShipState {
        ShipState::new(
            pr,
            "danielraffel/pulp",
            "feature/test",
            "main",
            sha,
            "policy0001",
        )
    }

    fn sample_run(target: &str, run_id: &str) -> DispatchedRun {
        let now = Utc::now();
        DispatchedRun {
            target: target.to_owned(),
            provider: "namespace".to_owned(),
            run_id: run_id.to_owned(),
            status: "in_progress".to_owned(),
            started_at: now,
            updated_at: now,
            attempt: 1,
            last_heartbeat_at: None,
            phase: None,
            required: true,
        }
    }

    #[test]
    fn dispatched_run_roundtrip_accepts_numeric_run_id() {
        let value = serde_json::json!({
            "target": "cloud",
            "provider": "namespace",
            "run_id": 24_446_948_064_u64,
            "status": "in_progress",
            "started_at": "2026-04-15T10:00:00+00:00",
            "updated_at": "2026-04-15T10:00:00+00:00"
        });
        let run: DispatchedRun = serde_json::from_value(value).expect("run should deserialize");
        assert_eq!(run.run_id, "24446948064");
        assert_eq!(run.attempt, 1);
        assert!(run.required);
    }

    #[test]
    fn ship_state_roundtrip_preserves_optional_fields() {
        let mut state = sample_state(224, "abc1234");
        state.pr_url = "https://github.com/danielraffel/pulp/pull/224".to_owned();
        state.pr_title = "Fix ARA controller".to_owned();
        state.commit_subject = "ara: out-of-line destructor".to_owned();
        state.upsert_run(sample_run("cloud", "99999"));
        state.update_evidence("macos", "pass");

        let restored: ShipState =
            serde_json::from_value(serde_json::to_value(&state).expect("serialize"))
                .expect("deserialize");
        assert_eq!(restored, state);
    }

    #[test]
    fn get_run_returns_most_recent_match() {
        let mut state = sample_state(1, "abc");
        let older = Utc::now() - Duration::minutes(10);
        let newer = Utc::now();
        state.dispatched_runs.push(DispatchedRun {
            updated_at: older,
            started_at: older,
            ..sample_run("cloud", "111")
        });
        state.dispatched_runs.push(DispatchedRun {
            updated_at: newer,
            started_at: newer,
            ..sample_run("cloud", "222")
        });
        assert_eq!(
            state.get_run("cloud").map(|run| run.run_id.as_str()),
            Some("222")
        );
    }

    #[test]
    fn store_save_get_list_and_archive_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = sample_state(42, "abc1234");
        state.upsert_run(sample_run("cloud", "999"));
        store.save(&state).expect("save");

        let restored = store.get(42).expect("state should exist");
        assert_eq!(restored.pr, 42);
        assert_eq!(
            store
                .list_active()
                .iter()
                .map(|item| item.pr)
                .collect::<Vec<_>>(),
            vec![42]
        );

        let archived = store
            .archive(42)
            .expect("archive call")
            .expect("archive path");
        assert!(archived.exists());
        assert!(store.get(42).is_none());
        assert_eq!(store.list_archived().len(), 1);
    }

    #[test]
    fn list_active_ignores_corrupt_and_non_integer_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store.save(&sample_state(7, "abc")).expect("save");
        fs::write(store.path().join("notapr.json"), "{}").expect("write stray");
        fs::write(store.state_path(21), "{broken").expect("write corrupt");
        let prs = store
            .list_active()
            .iter()
            .map(|state| state.pr)
            .collect::<Vec<_>>();
        assert_eq!(prs, vec![7]);
    }

    #[test]
    fn archive_and_replace_increments_attempt_and_clears_live_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = sample_state(30, "abc");
        state.attempt = 1;
        state.upsert_run(sample_run("cloud", "999"));
        state.update_evidence("macos", "pass");
        store.save(&state).expect("save");

        let fresh = store
            .archive_and_replace(&state, None)
            .expect("archive and replace");
        assert_eq!(fresh.attempt, 2);
        assert!(fresh.dispatched_runs.is_empty());
        assert!(fresh.evidence_snapshot.is_empty());
        assert_eq!(store.list_archived().len(), 1);
    }

    #[test]
    fn compute_policy_signature_is_stable_and_changes_with_inputs() {
        let a = compute_policy_signature(
            &["macos".to_owned(), "linux".to_owned(), "windows".to_owned()],
            &["mac".to_owned(), "ubuntu".to_owned()],
            "default",
        );
        let b = compute_policy_signature(
            &["windows".to_owned(), "macos".to_owned(), "linux".to_owned()],
            &["ubuntu".to_owned(), "mac".to_owned()],
            "default",
        );
        let c = compute_policy_signature(&["macos".to_owned()], &["mac".to_owned()], "strict");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn legacy_state_without_human_context_fields_still_loads() {
        let value = serde_json::json!({
            "schema_version": 1,
            "pr": 224,
            "repo": "danielraffel/pulp",
            "branch": "feature/test",
            "base_branch": "main",
            "head_sha": "abc1234",
            "policy_signature": "policy0001",
            "dispatched_runs": [],
            "evidence_snapshot": {},
            "attempt": 1,
            "created_at": "2026-04-15T10:00:00+00:00",
            "updated_at": "2026-04-15T10:00:00+00:00"
        });
        let state: ShipState = serde_json::from_value(value).expect("legacy state");
        assert_eq!(state.pr_url, "");
        assert_eq!(state.pr_title, "");
        assert_eq!(state.commit_subject, "");
    }

    #[test]
    fn save_is_atomic_and_leaves_no_named_tempfiles() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store.save(&sample_state(55, "abc")).expect("save");
        let strays = fs::read_dir(store.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect::<Vec<_>>();
        assert!(strays.is_empty(), "unexpected temp files: {strays:?}");
    }

    #[test]
    fn pr_lock_preserves_two_handle_mutations() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(ShipStateStore::new(temp.path().join("ship")).expect("store"));
        store.save(&sample_state(77, "abc")).expect("save");
        let writer = Arc::clone(&store);
        let (started_tx, started_rx) = mpsc::channel();

        let handle = store
            .with_pr_state_locked(77, |state| {
                let handle = thread::spawn(move || {
                    started_tx.send(()).expect("started");
                    writer
                        .with_pr_state_locked(77, |state| {
                            let state = state.as_mut().expect("state");
                            state.upsert_run(sample_run("linux", "222"));
                            Ok(())
                        })
                        .expect("update linux");
                });
                started_rx.recv().expect("started received");
                thread::sleep(StdDuration::from_millis(50));
                let state = state.as_mut().expect("state");
                state.upsert_run(sample_run("mac", "111"));
                Ok(handle)
            })
            .expect("locked mutation");
        handle.join().expect("writer thread");

        let state = store.get(77).expect("state");
        assert_eq!(
            state.get_run("mac").map(|run| run.run_id.as_str()),
            Some("111")
        );
        assert_eq!(
            state.get_run("linux").map(|run| run.run_id.as_str()),
            Some("222")
        );
    }

    #[test]
    fn touch_and_sha_drift_behave_as_expected() {
        let mut state = sample_state(1, "abc");
        let original = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("valid date");
        state.updated_at = original;
        state.touch();
        assert!(state.updated_at >= original);
        assert!(!state.is_sha_drift("abc"));
        assert!(state.is_sha_drift("def"));
    }

    #[test]
    fn deserializes_legacy_state_without_repo_root() {
        // A record written before the `repo_root` field must decode with
        // `repo_root == ""` (serde default), not fail.
        let json = r#"{
            "pr": 7,
            "repo": "danielraffel/pulp",
            "branch": "feature/x",
            "base_branch": "main",
            "head_sha": "abc123",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }"#;
        let state: ShipState = serde_json::from_str(json).expect("decode legacy state");
        assert_eq!(state.repo_root, "");
        assert_eq!(state.repo, "danielraffel/pulp");
    }
}
