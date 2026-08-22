use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::evidence::canonical_repository;

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

/// Terminal marker recorded when the daemon's opt-in resume sweep abandons an
/// orphaned in-flight ship-state — its owning ship worker died and never wrote a
/// verdict, so the state would otherwise block the wait/auto-merge path forever.
/// Setting it makes the state terminally failed (never merged); the PR must be
/// re-shipped by hand. Surfaced by `ship-state list` and a daemon IPC event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AbandonRecord {
    /// Human-readable reason the state was abandoned.
    pub reason: String,
    /// The orphan-evidence class that justified abandonment (e.g. `queue_stale`).
    pub evidence: String,
    /// Minutes the state had been idle (`updated_at` age) when abandoned.
    pub stalled_minutes: i64,
    /// The dead owning queue job's id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// When the abandonment was recorded.
    pub abandoned_at: DateTime<Utc>,
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
    /// Queue job that owns the current ship attempt.
    ///
    /// Queue-absence recovery requires this exact identity; older state files
    /// without it fail closed instead of adopting a same-head envelope from a
    /// prior attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_job_id: Option<String>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// When Shipyard most recently observed this PR in GitHub's merge queue.
    ///
    /// Persisting this across process restarts is the authority required to
    /// distinguish a recoverable eviction from a PR that was never queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue_observed_at: Option<DateTime<Utc>>,
    /// Start of the current native queue admission attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue_attempt_started_at: Option<DateTime<Utc>>,
    /// When GitHub accepted the current exact-head enqueue mutation.
    ///
    /// An absent PR after this point but before observed membership is
    /// terminal: Shipyard must not undo a manual dequeue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue_enqueue_succeeded_at: Option<DateTime<Utc>>,
    /// Durable marker written before issuing an enqueue mutation.
    ///
    /// A process exit after GitHub accepts the mutation but before the success
    /// marker is saved leaves this set, preventing a later invocation from
    /// treating queue absence as proof that no admission was attempted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue_enqueue_started_at: Option<DateTime<Utc>>,
    /// Terminal abandonment marker set by the daemon's opt-in orphan resume
    /// sweep. `None` for a normal in-flight or evidence-terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned: Option<AbandonRecord>,
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
            source_job_id: None,
            created_at: now,
            updated_at: now,
            merge_queue_observed_at: None,
            merge_queue_attempt_started_at: None,
            merge_queue_enqueue_succeeded_at: None,
            merge_queue_enqueue_started_at: None,
            abandoned: None,
        }
    }

    /// Whether this state has been terminally abandoned by the resume sweep.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        self.abandoned.is_some()
    }

    /// Record a terminal abandonment marker and bump `updated_at`.
    pub fn mark_abandoned(&mut self, record: AbandonRecord) {
        self.abandoned = Some(record);
        self.touch();
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
        crate::writer_domain_lease::ensure_protected_dir_all(&path)?;
        crate::writer_domain_lease::ensure_protected_dir_all(&path.join("archive"))?;
        crate::writer_domain_lease::ensure_protected_dir_all(&path.join("scoped"))?;
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

    /// Return the repository-scoped active state path for a PR.
    #[must_use]
    pub fn state_path_scoped(&self, repository: &str, pr: u64) -> PathBuf {
        self.path
            .join("scoped")
            .join(repository_key(repository))
            .join(format!("{pr}.json"))
    }

    /// Return the archive directory path.
    #[must_use]
    pub fn archive_dir(&self) -> PathBuf {
        self.path.join("archive")
    }

    /// Load an active state for a PR.
    #[must_use]
    pub fn get(&self, pr: u64) -> Option<ShipState> {
        let mut states = self.states_for_pr(pr);
        (states.len() == 1).then(|| states.pop()).flatten()
    }

    /// Load one repository-scoped state, migrating a matching legacy state on
    /// the next scoped save.
    #[must_use]
    pub fn get_scoped(&self, repository: &str, pr: u64) -> Option<ShipState> {
        let lock = self.lock_pr_scoped(repository, pr).ok()?;
        self.get_locked_scoped(repository, pr, &lock)
    }

    /// Acquire the per-PR ship-state lock.
    pub fn lock_pr(&self, pr: u64) -> io::Result<ShipStatePrLock> {
        ShipStatePrLock::acquire(self.lock_path(pr))
    }

    /// Acquire the legacy fence followed by the repository-scoped PR lock.
    ///
    /// Holding the legacy fence while taking the scoped lock makes migration
    /// safe against an older Shipyard binary that still writes `<pr>.json`.
    pub fn lock_pr_scoped(&self, repository: &str, pr: u64) -> io::Result<ShipStatePrLock> {
        // Preserve an authoritative legacy record before a different
        // repository with the same PR number can create scoped state. The
        // optimistic check keeps already-migrated repositories concurrent;
        // only a real migration takes the exclusive legacy fence.
        self.migrate_unrepresented_legacy(pr)?;
        // Older binaries take this fence exclusively. New binaries share it,
        // which blocks legacy writers for the full operation while allowing
        // different repository-scoped locks for Pulp #42 and Forge #42 to run
        // concurrently.
        let legacy_lock = ShipStatePrLock::acquire_shared(self.lock_path(pr))?;
        let scoped_lock = ShipStatePrLock::acquire(self.lock_path_scoped(repository, pr))?;
        self.migrate_matching_legacy_locked(repository, pr)?;
        Ok(legacy_lock.combine(scoped_lock))
    }

    /// Load an active state while the caller holds the per-PR lock.
    #[must_use]
    pub fn get_locked(&self, pr: u64, _lock: &ShipStatePrLock) -> Option<ShipState> {
        self.get(pr)
    }

    /// Load a scoped state while holding its migration-safe lock.
    #[must_use]
    pub fn get_locked_scoped(
        &self,
        repository: &str,
        pr: u64,
        _lock: &ShipStatePrLock,
    ) -> Option<ShipState> {
        if let Some(state) = Self::get_unlocked_path(&self.state_path_scoped(repository, pr)) {
            return (same_repository(&state.repo, repository) && state.pr == pr).then_some(state);
        }
        if self.collision_marker_path(pr).exists() {
            return None;
        }
        let legacy = self.get_unlocked(pr)?;
        (same_repository(&legacy.repo, repository) && legacy.pr == pr).then_some(legacy)
    }

    /// Mutate one repository-scoped PR state under its migration-safe lock.
    pub fn with_pr_state_scoped_locked<T>(
        &self,
        repository: &str,
        pr: u64,
        f: impl FnOnce(&mut Option<ShipState>) -> Result<T, Box<dyn std::error::Error>>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let lock = self.lock_pr_scoped(repository, pr)?;
        let mut state = self.get_locked_scoped(repository, pr, &lock);
        let output = f(&mut state)?;
        if let Some(state) = state {
            self.save_scoped_locked(&state, &lock)?;
        } else {
            self.delete_scoped_locked(repository, pr)?;
        }
        Ok(output)
    }

    /// Mutate one PR's state while holding that PR's lock.
    pub fn with_pr_state_locked<T>(
        &self,
        pr: u64,
        f: impl FnOnce(&mut Option<ShipState>) -> Result<T, Box<dyn std::error::Error>>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let states = self.states_for_pr(pr);
        if let [state] = states.as_slice() {
            return self.with_pr_state_scoped_locked(&state.repo, pr, f);
        }
        if states.len() > 1 {
            return Err(format!(
                "PR #{pr} is ambiguous across repositories; repository is required"
            )
            .into());
        }
        let lock = self.lock_pr(pr)?;
        let mut state = self.get_unlocked(pr);
        let output = f(&mut state)?;
        if let Some(state) = state {
            self.save_locked(&state, &lock)?;
        } else {
            let path = self.state_path(pr);
            if path.exists() {
                let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&path)?;
                fs::remove_file(path)?;
            }
        }
        Ok(output)
    }

    fn get_unlocked(&self, pr: u64) -> Option<ShipState> {
        Self::get_unlocked_path(&self.state_path(pr))
    }

    fn get_unlocked_path(path: &Path) -> Option<ShipState> {
        let contents = fs::read_to_string(path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    /// Save a state atomically.
    pub fn save(&self, state: &ShipState) -> Result<(), Box<dyn std::error::Error>> {
        let lock = self.lock_pr_scoped(&state.repo, state.pr)?;
        self.save_scoped_locked(state, &lock)
    }

    /// Save a state atomically while the caller holds the per-PR lock.
    pub fn save_locked(
        &self,
        state: &ShipState,
        _lock: &ShipStatePrLock,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
        let payload = serde_json::to_string_pretty(state)?;
        let temp = tempfile::NamedTempFile::new_in(&self.path)?;
        fs::write(temp.path(), format!("{payload}\n"))?;
        temp.persist(self.state_path(state.pr))?;
        Ok(())
    }

    /// Save a state under its repository namespace while holding the matching
    /// migration-safe scoped lock.
    pub fn save_scoped_locked(
        &self,
        state: &ShipState,
        _lock: &ShipStatePrLock,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.ensure_legacy_is_preserved(state.pr)?;
        let path = self.state_path_scoped(&state.repo, state.pr);
        Self::persist_state_at(state, &path)?;
        self.sync_legacy_mirror_for_pr(state.pr)?;
        Ok(())
    }

    /// Delete an active state file.
    pub fn delete(&self, pr: u64) -> Result<(), std::io::Error> {
        let states = self.states_for_pr(pr);
        match states.as_slice() {
            [] => Ok(()),
            [state] => self.delete_scoped(&state.repo, pr),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("PR #{pr} is ambiguous across repositories; repository is required"),
            )),
        }
    }

    /// Delete one repository-scoped active state file.
    pub fn delete_scoped(&self, repository: &str, pr: u64) -> Result<(), std::io::Error> {
        let _lock = self.lock_pr_scoped(repository, pr)?;
        self.delete_scoped_locked(repository, pr)
    }

    fn delete_scoped_locked(&self, repository: &str, pr: u64) -> Result<(), std::io::Error> {
        {
            let _writer_domain =
                crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
            let path = self.state_path_scoped(repository, pr);
            if path.exists() {
                fs::remove_file(path)?;
            }
            self.remove_matching_legacy(repository, pr)?;
        }
        self.sync_legacy_mirror_for_pr(pr)?;
        Ok(())
    }

    /// Move an active state into the archive directory.
    pub fn archive(&self, pr: u64) -> Result<Option<PathBuf>, std::io::Error> {
        let states = self.states_for_pr(pr);
        match states.as_slice() {
            [] => Ok(None),
            [state] => self.archive_scoped(&state.repo, pr),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("PR #{pr} is ambiguous across repositories; repository is required"),
            )),
        }
    }

    /// Archive one repository-scoped active state.
    pub fn archive_scoped(
        &self,
        repository: &str,
        pr: u64,
    ) -> Result<Option<PathBuf>, std::io::Error> {
        let lock = self.lock_pr_scoped(repository, pr)?;
        self.archive_scoped_locked(repository, pr, &lock)
    }

    /// Move an active state into the archive directory while holding the lock.
    pub fn archive_locked(
        &self,
        pr: u64,
        _lock: &ShipStatePrLock,
    ) -> Result<Option<PathBuf>, std::io::Error> {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
        let source = self.state_path(pr);
        if !source.exists() {
            return Ok(None);
        }
        let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
        let dest = self.archive_dir().join(format!("{pr}-{stamp}.json"));
        fs::rename(source, &dest)?;
        Ok(Some(dest))
    }

    /// Archive a repository-scoped state while holding its scoped lock.
    pub fn archive_scoped_locked(
        &self,
        repository: &str,
        pr: u64,
        _lock: &ShipStatePrLock,
    ) -> Result<Option<PathBuf>, std::io::Error> {
        let scoped = self.state_path_scoped(repository, pr);
        let source = if scoped.exists() {
            scoped
        } else {
            let legacy = self.state_path(pr);
            if !Self::get_unlocked_path(&legacy)
                .is_some_and(|state| same_repository(&state.repo, repository))
            {
                return Ok(None);
            }
            legacy
        };
        let dest = {
            let _writer_domain =
                crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
            let archive_dir = self.archive_dir().join(repository_key(repository));
            fs::create_dir_all(&archive_dir)?;
            let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
            let dest = archive_dir.join(format!("{pr}-{stamp}.json"));
            fs::rename(source, &dest)?;
            self.remove_matching_legacy(repository, pr)?;
            dest
        };
        self.sync_legacy_mirror_for_pr(pr)?;
        Ok(Some(dest))
    }

    /// Return active states sorted by PR number.
    pub fn list_active(&self) -> Vec<ShipState> {
        let mut states = BTreeMap::<(String, u64), ShipState>::new();
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
                    && !self.collision_marker_path(state.pr).exists()
                {
                    insert_newest_state(&mut states, state);
                }
            }
        }
        let scoped_root = self.path.join("scoped");
        if let Ok(repositories) = fs::read_dir(scoped_root) {
            for repository in repositories.flatten() {
                if !repository.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                if let Ok(entries) = fs::read_dir(repository.path()) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                            continue;
                        }
                        if let Some(state) = Self::get_unlocked_path(&path) {
                            insert_newest_state(&mut states, state);
                        }
                    }
                }
            }
        }
        let mut states = states.into_values().collect::<Vec<_>>();
        states.sort_by(|left, right| {
            left.pr.cmp(&right.pr).then_with(|| {
                canonical_repository(&left.repo).cmp(&canonical_repository(&right.repo))
            })
        });
        states
    }

    /// Return archived state file paths sorted by filename.
    #[must_use]
    pub fn list_archived(&self) -> Vec<PathBuf> {
        let mut archived = Vec::new();
        collect_json_files(&self.archive_dir(), &mut archived);
        archived.sort();
        archived
    }

    /// Archive the current state for a PR and create a fresh attempt record.
    pub fn archive_and_replace(
        &self,
        state: &ShipState,
        new_attempt: Option<u32>,
    ) -> Result<ShipState, Box<dyn std::error::Error>> {
        let lock = self.lock_pr_scoped(&state.repo, state.pr)?;
        self.archive_and_replace_locked(state, new_attempt, &lock)
    }

    /// Archive the current state and create a fresh attempt while holding the lock.
    pub fn archive_and_replace_locked(
        &self,
        state: &ShipState,
        new_attempt: Option<u32>,
        lock: &ShipStatePrLock,
    ) -> Result<ShipState, Box<dyn std::error::Error>> {
        let _ = self.archive_scoped_locked(&state.repo, state.pr, lock)?;
        let now = Utc::now();
        Ok(ShipState {
            attempt: new_attempt.unwrap_or(state.attempt + 1),
            source_job_id: None,
            dispatched_runs: Vec::new(),
            evidence_snapshot: BTreeMap::new(),
            created_at: now,
            updated_at: now,
            merge_queue_observed_at: None,
            merge_queue_attempt_started_at: None,
            merge_queue_enqueue_succeeded_at: None,
            merge_queue_enqueue_started_at: None,
            // A fresh attempt must not inherit the prior attempt's terminal
            // abandonment marker, or the re-ship would be dead on arrival.
            abandoned: None,
            ..state.clone()
        })
    }

    fn lock_path(&self, pr: u64) -> PathBuf {
        self.path.join(format!("{pr}.lock"))
    }

    fn lock_path_scoped(&self, repository: &str, pr: u64) -> PathBuf {
        self.path
            .join("scoped")
            .join(repository_key(repository))
            .join(format!("{pr}.lock"))
    }

    fn compatibility_lock_path(&self, pr: u64) -> PathBuf {
        self.path.join(format!("{pr}.compat.lock"))
    }

    fn collision_marker_path(&self, pr: u64) -> PathBuf {
        self.path.join(format!("{pr}.scoped-collision"))
    }

    fn remove_matching_legacy(&self, repository: &str, pr: u64) -> io::Result<()> {
        let legacy = self.state_path(pr);
        if Self::get_unlocked_path(&legacy)
            .is_some_and(|state| same_repository(&state.repo, repository))
        {
            fs::remove_file(legacy)?;
        }
        Ok(())
    }

    fn states_for_pr(&self, pr: u64) -> Vec<ShipState> {
        self.list_active()
            .into_iter()
            .filter(|state| state.pr == pr)
            .collect()
    }

    fn scoped_states_for_pr(&self, pr: u64) -> Vec<ShipState> {
        let mut states = Vec::new();
        let scoped_root = self.path.join("scoped");
        if let Ok(repositories) = fs::read_dir(scoped_root) {
            for repository in repositories.flatten() {
                if !repository.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let path = repository.path().join(format!("{pr}.json"));
                if let Some(state) = Self::get_unlocked_path(&path) {
                    states.push(state);
                }
            }
        }
        states
    }

    fn persist_state_at(state: &ShipState, path: &Path) -> io::Result<()> {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
        let parent = path.parent().expect("ship-state path has parent");
        fs::create_dir_all(parent)?;
        let payload = serde_json::to_string_pretty(state)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let temp = tempfile::NamedTempFile::new_in(parent)?;
        fs::write(temp.path(), format!("{payload}\n"))?;
        temp.persist(path).map_err(|error| error.error)?;
        Ok(())
    }

    fn legacy_is_preserved(&self, legacy: &ShipState) -> bool {
        if self.collision_marker_path(legacy.pr).exists() {
            return true;
        }
        Self::get_unlocked_path(&self.state_path_scoped(&legacy.repo, legacy.pr))
            .is_some_and(|scoped| scoped.updated_at >= legacy.updated_at)
    }

    fn ensure_legacy_is_preserved(&self, pr: u64) -> io::Result<()> {
        let Some(legacy) = self.get_unlocked(pr) else {
            return Ok(());
        };
        if self.legacy_is_preserved(&legacy) {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "legacy ship-state for {} PR #{} is not yet preserved in its repository namespace",
                legacy.repo, legacy.pr
            ),
        ))
    }

    fn migrate_unrepresented_legacy(&self, pr: u64) -> io::Result<()> {
        if self.collision_marker_path(pr).exists() {
            return Ok(());
        }
        let Some(legacy) = self.get_unlocked(pr) else {
            return Ok(());
        };
        if self.legacy_is_preserved(&legacy) {
            return Ok(());
        }

        let _legacy_lock = self.lock_pr(pr)?;
        let Some(legacy) = self.get_unlocked(pr) else {
            return Ok(());
        };
        if self.legacy_is_preserved(&legacy) {
            return Ok(());
        }
        let _scoped_lock = ShipStatePrLock::acquire(self.lock_path_scoped(&legacy.repo, pr))?;
        let scoped_path = self.state_path_scoped(&legacy.repo, pr);
        if Self::get_unlocked_path(&scoped_path)
            .as_ref()
            .is_none_or(|scoped| legacy.updated_at > scoped.updated_at)
        {
            Self::persist_state_at(&legacy, &scoped_path)?;
        }
        Ok(())
    }

    /// Keep old binaries functional while one repository owns this PR number.
    /// A legacy file cannot represent a cross-repository collision, so remove
    /// it in that case and make old binaries fail closed instead of selecting
    /// an arbitrary repository. The short compatibility lock serializes this
    /// mirror update without serializing the repository-scoped operations.
    fn sync_legacy_mirror_for_pr(&self, pr: u64) -> io::Result<()> {
        let _compatibility_lock = ShipStatePrLock::acquire(self.compatibility_lock_path(pr))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
        self.ensure_legacy_is_preserved(pr)?;
        let scoped = self.scoped_states_for_pr(pr);
        let legacy_path = self.state_path(pr);
        let collision_marker = self.collision_marker_path(pr);
        match scoped.as_slice() {
            [state] => {
                let payload = serde_json::to_string_pretty(state)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                let temp = tempfile::NamedTempFile::new_in(&self.path)?;
                fs::write(temp.path(), format!("{payload}\n"))?;
                temp.persist(&legacy_path).map_err(|error| error.error)?;
                if collision_marker.exists() {
                    fs::remove_file(collision_marker)?;
                }
            }
            [] => {
                if legacy_path.exists() {
                    fs::remove_file(legacy_path)?;
                }
                if collision_marker.exists() {
                    fs::remove_file(collision_marker)?;
                }
            }
            _ => {
                // Persist the fence before removing the ambiguous mirror. An
                // older binary can recreate `<pr>.json` after its lock is
                // released, but new binaries must never import that record
                // over repository-scoped runs and evidence.
                fs::write(&collision_marker, b"repository-scoped\n")?;
                if legacy_path.exists() {
                    fs::remove_file(legacy_path)?;
                }
            }
        }
        Ok(())
    }

    fn migrate_matching_legacy_locked(&self, repository: &str, pr: u64) -> io::Result<()> {
        if self.collision_marker_path(pr).exists() {
            return Ok(());
        }
        let legacy_path = self.state_path(pr);
        let Some(legacy) = Self::get_unlocked_path(&legacy_path) else {
            return Ok(());
        };
        if !same_repository(&legacy.repo, repository) {
            return Ok(());
        }
        let scoped_path = self.state_path_scoped(repository, pr);
        let scoped = Self::get_unlocked_path(&scoped_path);
        if scoped
            .as_ref()
            .is_none_or(|scoped| legacy.updated_at > scoped.updated_at)
        {
            Self::persist_state_at(&legacy, &scoped_path)?;
        }
        self.sync_legacy_mirror_for_pr(pr)?;
        Ok(())
    }
}

/// Held per-PR ship-state lock.
#[derive(Debug)]
pub struct ShipStatePrLock {
    files: Vec<File>,
}

impl ShipStatePrLock {
    fn acquire(path: PathBuf) -> io::Result<Self> {
        let writer_domain = crate::writer_domain_lease::acquire_for_protected_creation(&path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        drop(writer_domain);
        FileExt::lock_exclusive(&file)?;
        Ok(Self { files: vec![file] })
    }

    fn acquire_shared(path: PathBuf) -> io::Result<Self> {
        let writer_domain = crate::writer_domain_lease::acquire_for_protected_creation(&path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        drop(writer_domain);
        FileExt::lock_shared(&file)?;
        Ok(Self { files: vec![file] })
    }

    fn combine(mut self, mut other: Self) -> Self {
        self.files.append(&mut other.files);
        self
    }
}

impl Drop for ShipStatePrLock {
    fn drop(&mut self) {
        for file in self.files.iter().rev() {
            let _ = FileExt::unlock(file);
        }
    }
}

fn same_repository(left: &str, right: &str) -> bool {
    canonical_repository(left) == canonical_repository(right)
}

fn insert_newest_state(states: &mut BTreeMap<(String, u64), ShipState>, state: ShipState) {
    let key = (canonical_repository(&state.repo), state.pr);
    if states
        .get(&key)
        .is_none_or(|existing| state.updated_at > existing.updated_at)
    {
        states.insert(key, state);
    }
}

fn repository_key(repository: &str) -> String {
    let canonical = canonical_repository(repository);
    let readable = canonical
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .take(48)
        .collect::<String>();
    let digest = Sha256::digest(canonical.as_bytes());
    format!("{readable}--{}", hex::encode(digest))
}

fn collect_json_files(directory: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            collect_json_files(&path, output);
        } else if path.extension().and_then(std::ffi::OsStr::to_str) == Some("json") {
            output.push(path);
        }
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
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration as StdDuration;

    use chrono::{Duration, TimeZone, Utc};

    use super::{
        AbandonRecord, DispatchedRun, ShipState, ShipStateStore, compute_policy_signature,
        same_repository,
    };

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
        state.merge_queue_observed_at = Some(Utc::now());
        state.merge_queue_attempt_started_at = Some(Utc::now());
        state.merge_queue_enqueue_succeeded_at = Some(Utc::now());
        state.merge_queue_enqueue_started_at = Some(Utc::now());
        store.save(&state).expect("save");

        let fresh = store
            .archive_and_replace(&state, None)
            .expect("archive and replace");
        assert_eq!(fresh.attempt, 2);
        assert!(fresh.dispatched_runs.is_empty());
        assert!(fresh.evidence_snapshot.is_empty());
        assert!(fresh.merge_queue_observed_at.is_none());
        assert!(fresh.merge_queue_attempt_started_at.is_none());
        assert!(fresh.merge_queue_enqueue_succeeded_at.is_none());
        assert!(fresh.merge_queue_enqueue_started_at.is_none());
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
    fn same_pr_number_is_isolated_across_pulp_forge_and_vellum() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let identities = [
            ("Generous-Corp/pulp", "pulp"),
            ("Generous-Corp/forge", "forge"),
            ("Generous-Corp/vellum", "vellum"),
        ];

        for (repository, sha) in identities {
            store
                .save(&ShipState::new(
                    42,
                    repository,
                    "feature/shared",
                    "main",
                    sha,
                    "policy",
                ))
                .expect("save scoped state");
        }

        for (repository, sha) in identities {
            assert_eq!(
                store
                    .get_scoped(repository, 42)
                    .expect("scoped state")
                    .head_sha,
                sha
            );
        }
        assert!(
            store.get(42).is_none(),
            "number-only lookup must fail closed when repositories collide"
        );
        assert!(
            !store.state_path(42).exists(),
            "legacy state must fail closed when one PR number spans repositories"
        );
        assert_eq!(store.list_active().len(), identities.len());
    }

    #[test]
    fn same_pr_number_scoped_locks_do_not_block_other_repositories() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(ShipStateStore::new(temp.path().join("ship")).expect("store"));
        let pulp_lock = store
            .lock_pr_scoped("Generous-Corp/pulp", 42)
            .expect("pulp lock");
        let forge_store = Arc::clone(&store);
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let _forge_lock = forge_store
                .lock_pr_scoped("Generous-Corp/forge", 42)
                .expect("forge lock");
            acquired_tx.send(()).expect("acquired");
        });

        acquired_rx
            .recv_timeout(StdDuration::from_secs(1))
            .expect("Forge lock must not wait for Pulp's same-number PR");
        drop(pulp_lock);
        handle.join().expect("lock thread");
    }

    #[test]
    fn concurrent_cross_repository_saves_remove_ambiguous_legacy_mirror() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(ShipStateStore::new(temp.path().join("ship")).expect("store"));
        let start = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for (repository, sha) in [
            ("Generous-Corp/pulp", "pulp"),
            ("Generous-Corp/forge", "forge"),
        ] {
            let worker_store = Arc::clone(&store);
            let worker_start = Arc::clone(&start);
            workers.push(thread::spawn(move || {
                let state = ShipState::new(42, repository, "feature/shared", "main", sha, "policy");
                worker_start.wait();
                worker_store.save(&state).expect("scoped save");
            }));
        }
        start.wait();
        for worker in workers {
            worker.join().expect("save worker");
        }

        assert!(store.get_scoped("Generous-Corp/pulp", 42).is_some());
        assert!(store.get_scoped("Generous-Corp/forge", 42).is_some());
        assert!(
            !store.state_path(42).exists(),
            "legacy mirror must not select one repository after a concurrent collision"
        );
        assert!(store.collision_marker_path(42).exists());
    }

    #[test]
    fn collision_fence_rejects_a_recreated_legacy_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut pulp = ShipState::new(
            42,
            "Generous-Corp/pulp",
            "feature/shared",
            "main",
            "pulp-scoped",
            "policy",
        );
        pulp.update_evidence("mac", "pass");
        store.save(&pulp).expect("save Pulp state");
        store
            .save(&ShipState::new(
                42,
                "Generous-Corp/forge",
                "feature/shared",
                "main",
                "forge-scoped",
                "policy",
            ))
            .expect("save Forge state");

        let mut recreated = ShipState::new(
            42,
            "Generous-Corp/pulp",
            "feature/shared",
            "main",
            "pulp-legacy-recreated",
            "policy",
        );
        recreated.updated_at = Utc::now() + chrono::Duration::minutes(1);
        let legacy_lock = store.lock_pr(42).expect("legacy writer lock");
        store
            .save_locked(&recreated, &legacy_lock)
            .expect("old binary recreates legacy state");
        drop(legacy_lock);

        let active = store
            .get_scoped("Generous-Corp/pulp", 42)
            .expect("scoped Pulp state");
        assert_eq!(active.head_sha, "pulp-scoped");
        assert_eq!(
            active.evidence_snapshot.get("mac").map(String::as_str),
            Some("pass")
        );
        assert_eq!(
            store
                .list_active()
                .into_iter()
                .find(|state| same_repository(&state.repo, "Generous-Corp/pulp"))
                .expect("listed Pulp state")
                .head_sha,
            "pulp-scoped"
        );

        fs::remove_file(store.state_path_scoped("Generous-Corp/pulp", 42))
            .expect("simulate missing scoped state");
        assert!(
            store.get_scoped("Generous-Corp/pulp", 42).is_none(),
            "collision fence must reject a recreated legacy fallback"
        );

        store.save(&active).expect("resave scoped state");
        assert!(!store.state_path(42).exists());
        assert!(store.collision_marker_path(42).exists());

        store
            .archive_scoped("Generous-Corp/forge", 42)
            .expect("archive Forge state");
        assert!(!store.collision_marker_path(42).exists());
        assert_eq!(
            store
                .get(42)
                .expect("unambiguous compatibility state")
                .head_sha,
            "pulp-scoped"
        );
    }

    #[test]
    fn scoped_lock_fences_legacy_writer_for_full_operation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(ShipStateStore::new(temp.path().join("ship")).expect("store"));
        let scoped_lock = store
            .lock_pr_scoped("Generous-Corp/pulp", 42)
            .expect("scoped lock");
        let legacy_store = Arc::clone(&store);
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let _legacy_lock = legacy_store.lock_pr(42).expect("legacy lock");
            acquired_tx.send(()).expect("acquired");
        });

        assert!(
            acquired_rx
                .recv_timeout(StdDuration::from_millis(50))
                .is_err(),
            "legacy writer bypassed the scoped operation's compatibility fence"
        );
        drop(scoped_lock);
        acquired_rx
            .recv_timeout(StdDuration::from_secs(1))
            .expect("legacy writer proceeds after scoped operation");
        handle.join().expect("lock thread");
    }

    #[test]
    fn scoped_lock_migrates_only_matching_legacy_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let legacy = sample_state(91, "legacy");
        let legacy_lock = store.lock_pr(91).expect("legacy lock");
        store
            .save_locked(&legacy, &legacy_lock)
            .expect("legacy save");
        drop(legacy_lock);

        assert!(store.get_scoped("Generous-Corp/forge", 91).is_none());
        assert!(store.state_path(91).exists());

        let migrated = store
            .get_scoped("DANIELRAFFEL/PULP", 91)
            .expect("matching legacy state");
        assert_eq!(migrated.head_sha, "legacy");
        assert_eq!(
            ShipStateStore::get_unlocked_path(&store.state_path(91))
                .expect("legacy compatibility mirror")
                .head_sha,
            "legacy"
        );
        assert!(store.state_path_scoped("danielraffel/pulp", 91).exists());
    }

    #[test]
    fn first_colliding_scoped_write_preserves_mismatched_legacy_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let legacy = sample_state(93, "pulp-legacy");
        let legacy_lock = store.lock_pr(93).expect("legacy lock");
        store
            .save_locked(&legacy, &legacy_lock)
            .expect("legacy save");
        drop(legacy_lock);

        let forge = ShipState::new(
            93,
            "Generous-Corp/forge",
            "feature/shared",
            "main",
            "forge-new",
            "policy",
        );
        store.save(&forge).expect("colliding scoped save");

        assert_eq!(
            store
                .get_scoped("danielraffel/pulp", 93)
                .expect("preserved Pulp state")
                .head_sha,
            "pulp-legacy"
        );
        assert_eq!(
            store
                .get_scoped("Generous-Corp/forge", 93)
                .expect("new Forge state")
                .head_sha,
            "forge-new"
        );
        assert!(
            !store.state_path(93).exists(),
            "legacy mirror must be removed after preserving both repositories"
        );
    }

    #[test]
    fn scoped_lock_reconciles_newer_legacy_update_under_compatibility_fence() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut scoped = sample_state(92, "scoped-old");
        scoped.updated_at = Utc::now() - chrono::Duration::minutes(2);
        store.save(&scoped).expect("scoped save");

        let mut legacy = scoped.clone();
        legacy.head_sha = "legacy-new".to_owned();
        legacy.updated_at = Utc::now();
        let legacy_lock = store.lock_pr(92).expect("legacy lock");
        store
            .save_locked(&legacy, &legacy_lock)
            .expect("mixed-version legacy save");
        drop(legacy_lock);

        assert_eq!(
            store.get(92).expect("newest unscoped state").head_sha,
            "legacy-new",
            "compatibility reads must not let an older scoped mirror hide a newer legacy write"
        );

        let active = store
            .get_scoped("danielraffel/pulp", 92)
            .expect("newest state remains active");
        assert_eq!(active.head_sha, "legacy-new");
        assert_eq!(
            ShipStateStore::get_unlocked_path(&store.state_path(92))
                .expect("legacy compatibility mirror")
                .head_sha,
            "legacy-new"
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
    fn archive_and_replace_clears_prior_abandonment() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = sample_state(7, "abc");
        state.mark_abandoned(AbandonRecord {
            reason: "orphaned".to_owned(),
            evidence: "queue_stale".to_owned(),
            stalled_minutes: 90,
            job_id: Some("job-1".to_owned()),
            abandoned_at: Utc::now(),
        });
        store.save(&state).expect("save");

        let fresh = store
            .archive_and_replace(&state, None)
            .expect("archive + fresh attempt");
        assert!(
            !fresh.is_abandoned(),
            "a fresh attempt must not inherit the abandonment marker"
        );
    }
}
