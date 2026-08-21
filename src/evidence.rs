use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

/// Proof that a specific SHA was validated on a specific target.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EvidenceRecord {
    /// Validated git SHA.
    pub sha: String,
    /// Git branch associated with the run.
    pub branch: String,
    /// Repository or workload namespace that owns this evidence.
    ///
    /// Legacy records omit this field and remain readable through the
    /// unscoped compatibility API. New queue-backed validation must use the
    /// scoped APIs so repositories with the same branch and target names do
    /// not serialize on, or overwrite, one another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_scope: Option<String>,
    /// Logical target name.
    #[serde(rename = "target")]
    pub target_name: String,
    /// Typed validation build configuration declared by the executed target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_build_type: Option<String>,
    /// Concrete platform label.
    pub platform: String,
    /// Validation result status.
    pub status: String,
    /// Backend that produced this evidence.
    pub backend: String,
    /// Git HEAD observed in the execution checkout before validation began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_head_sha: Option<String>,
    /// Git tree observed in the execution checkout before validation began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tree_sha: Option<String>,
    /// Whether the execution checkout was clean before validation began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_checkout_clean: Option<bool>,
    /// Whether validation began without resume or prepared-state stage reuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_execution: Option<bool>,
    /// Completion timestamp.
    pub completed_at: DateTime<Utc>,
    /// Optional duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    /// Optional host identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Primary backend when failover occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_backend: Option<String>,
    /// Failover reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failover_reason: Option<String>,
    /// Cloud provider label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Runner profile label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_profile: Option<String>,
    /// Coarse failure class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    /// Ancestor SHA this record reused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_from: Option<String>,
    /// Digest of the contract in effect when the record was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_digest: Option<String>,
    /// Stable signature of the stage pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stages_signature: Option<String>,
}

/// Artifact captured for a workload-agnostic command-evidence bundle.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommandEvidenceArtifact {
    /// Glob that selected this artifact.
    pub pattern: String,
    /// Path on the target, relative to the command working directory.
    pub source: String,
    /// Local path where Shipyard stored the artifact.
    pub path: String,
    /// Captured file size in bytes.
    pub size_bytes: u64,
}

/// Typed evidence for one arbitrary command run on a local or POSIX SSH target.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommandEvidenceRecord {
    /// Record schema version.
    pub schema_version: u8,
    /// Stable command-evidence id.
    pub id: String,
    /// User-facing workload name.
    pub name: String,
    /// Repository/workload namespace that owns the bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_scope: Option<String>,
    /// Git branch associated with the command, when run inside a checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Git SHA associated with the command, when run inside a checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// Logical target name.
    #[serde(rename = "target")]
    pub target_name: String,
    /// Concrete platform label.
    pub platform: String,
    /// Backend that ran the command.
    pub backend: String,
    /// Optional host identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Working directory on the target.
    pub workdir: String,
    /// Command argv.
    pub command: Vec<String>,
    /// Expected process exit code.
    pub expected_exit_code: i32,
    /// Observed process exit code.
    pub exit_code: i32,
    /// `pass` when the observed exit code matches the expected code, otherwise `fail`.
    pub status: String,
    /// Process start timestamp.
    pub started_at: DateTime<Utc>,
    /// Process completion timestamp.
    pub completed_at: DateTime<Utc>,
    /// Wall-clock duration in seconds.
    pub duration_secs: f64,
    /// Local log path.
    pub log_path: String,
    /// Bounded log excerpt captured from command output.
    pub log_excerpt: String,
    /// Environment variable fingerprints keyed by variable name.
    pub env_fingerprint: BTreeMap<String, String>,
    /// Captured artifacts.
    pub artifacts: Vec<CommandEvidenceArtifact>,
    /// Artifact collection errors. A non-empty list makes the evidence fail.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_errors: Vec<String>,
    /// Bundle directory containing `evidence.json` and captured artifacts.
    pub bundle_path: String,
}

impl CommandEvidenceRecord {
    /// Whether the command evidence passed its exit-code assertion.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == "pass"
    }
}

/// Persistent store for command-evidence bundles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandEvidenceStore {
    path: PathBuf,
}

impl CommandEvidenceStore {
    /// Open a command-evidence store at the given path.
    pub fn new(path: PathBuf) -> Result<Self, std::io::Error> {
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Backing path of the command-evidence store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the bundle directory for an evidence id.
    #[must_use]
    pub fn bundle_dir(&self, id: &str) -> PathBuf {
        self.path.join(sanitize_component(id))
    }

    /// Return the artifact directory for an evidence id.
    #[must_use]
    pub fn artifact_dir(&self, id: &str) -> PathBuf {
        self.bundle_dir(id).join("artifacts")
    }

    /// Atomically reserve a unique bundle id derived from `preferred_id`.
    ///
    /// The old timestamp-only id could collide across concurrent Shipyard
    /// processes and let one workload replace another workload's evidence and
    /// artifacts. Directory creation is the cross-process uniqueness fence;
    /// a numeric suffix is used only when the preferred id is already owned.
    pub fn reserve_bundle_id(&self, preferred_id: &str) -> Result<String, std::io::Error> {
        let preferred_id = sanitize_component(preferred_id);
        for suffix in 0_u64.. {
            let candidate = if suffix == 0 {
                preferred_id.clone()
            } else {
                format!("{preferred_id}-{suffix}")
            };
            match fs::create_dir(self.bundle_dir(&candidate)) {
                Ok(()) => return Ok(candidate),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        unreachable!("the bundle suffix space is unbounded")
    }

    /// Store or replace a command-evidence record.
    pub fn record(
        &self,
        evidence: &CommandEvidenceRecord,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bundle_dir = self.bundle_dir(&evidence.id);
        fs::create_dir_all(&bundle_dir)?;
        let payload = serde_json::to_string_pretty(evidence)?;
        let temp = tempfile::NamedTempFile::new_in(&bundle_dir)?;
        fs::write(temp.path(), format!("{payload}\n"))?;
        temp.persist(bundle_dir.join("evidence.json"))?;
        Ok(())
    }

    /// Return a command-evidence record by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<CommandEvidenceRecord> {
        Self::load_record(&self.bundle_dir(id).join("evidence.json")).ok()
    }

    /// Return all readable command-evidence records sorted newest first.
    #[must_use]
    pub fn list(&self) -> Vec<CommandEvidenceRecord> {
        let Ok(entries) = fs::read_dir(&self.path) else {
            return Vec::new();
        };
        let mut records = entries
            .flatten()
            .filter_map(|entry| Self::load_record(&entry.path().join("evidence.json")).ok())
            .collect::<Vec<_>>();
        records.sort_by_key(|record| std::cmp::Reverse(record.completed_at));
        records
    }

    /// Return the newest readable command-evidence record.
    #[must_use]
    pub fn latest(&self) -> Option<CommandEvidenceRecord> {
        self.list().into_iter().next()
    }

    fn load_record(path: &Path) -> Result<CommandEvidenceRecord, Box<dyn std::error::Error>> {
        let contents = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&contents)?)
    }
}

impl EvidenceRecord {
    /// Whether this record is a passing validation.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == "pass"
    }

    /// Whether this record was synthesized from an ancestor pass.
    #[must_use]
    pub fn reused(&self) -> bool {
        self.reused_from.is_some()
    }
}

/// Persistent store for per-branch evidence records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceStore {
    path: PathBuf,
}

impl EvidenceStore {
    /// Open an evidence store at the given path.
    pub fn new(path: PathBuf) -> Result<Self, std::io::Error> {
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Backing path of the store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Store or replace a record for the same branch and target.
    pub fn record(&self, evidence: &EvidenceRecord) -> Result<(), Box<dyn std::error::Error>> {
        self.with_branch_records_locked(&evidence.branch, |records| {
            records.insert(evidence.target_name.clone(), evidence.clone());
            Ok(())
        })
    }

    /// Store or replace a record inside one repository/workload namespace.
    pub fn record_scoped(
        &self,
        workload_scope: &str,
        evidence: &EvidenceRecord,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.with_scoped_branch_records_locked(workload_scope, &evidence.branch, |records| {
            let mut evidence = evidence.clone();
            evidence.workload_scope = Some(workload_scope.to_owned());
            records.insert(evidence.target_name.clone(), evidence);
            Ok(())
        })
    }

    /// Mutate one branch's records while holding that branch's evidence lock.
    pub fn with_branch_records_locked<T>(
        &self,
        branch: &str,
        f: impl FnOnce(&mut BTreeMap<String, EvidenceRecord>) -> Result<T, Box<dyn std::error::Error>>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let branch_key = sanitize_branch(branch);
        let _lock = StoreLock::acquire(self.branch_lock_file(&branch_key))?;
        let mut records = self.load_branch(&branch_key)?;
        let output = f(&mut records)?;
        self.save_branch(&branch_key, &records)?;
        Ok(output)
    }

    /// Mutate one scoped branch while holding only that namespace's lock.
    pub fn with_scoped_branch_records_locked<T>(
        &self,
        workload_scope: &str,
        branch: &str,
        f: impl FnOnce(&mut BTreeMap<String, EvidenceRecord>) -> Result<T, Box<dyn std::error::Error>>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let scope_key = collision_safe_component(workload_scope);
        let branch_key = collision_safe_component(branch);
        let directory = self.path.join("scoped").join(scope_key);
        let legacy_scope_key = legacy_collision_safe_component(workload_scope);
        let legacy_branch_key = legacy_collision_safe_component(branch);
        let legacy_directory = self.path.join("scoped").join(legacy_scope_key);
        let legacy_file = legacy_directory.join(format!("{legacy_branch_key}.json"));
        // Do not create the long legacy namespace for new writes: that would
        // reintroduce the Windows MAX_PATH failure this encoding fixes.
        let _legacy_lock = legacy_file
            .try_exists()
            .unwrap_or(false)
            .then(|| StoreLock::acquire(legacy_directory.join(format!("{legacy_branch_key}.lock"))))
            .transpose()?;
        let _lock = StoreLock::acquire(directory.join(format!("{branch_key}.lock")))?;
        let mut records = self.load_scoped_records(workload_scope, branch)?;
        let output = f(&mut records)?;
        Self::save_records(&directory, &branch_key, &records)?;
        Ok(output)
    }

    /// Return all evidence for a branch keyed by target name.
    #[must_use]
    pub fn get_branch(&self, branch: &str) -> BTreeMap<String, EvidenceRecord> {
        self.load_branch(&sanitize_branch(branch))
            .unwrap_or_default()
    }

    /// Return evidence for a branch in one repository/workload namespace.
    #[must_use]
    pub fn get_branch_scoped(
        &self,
        workload_scope: &str,
        branch: &str,
    ) -> BTreeMap<String, EvidenceRecord> {
        self.load_scoped_records(workload_scope, branch)
            .unwrap_or_default()
    }

    /// Return the newest record per target for scoped workloads whose stored
    /// scope starts with `workload_scope_prefix`.
    #[must_use]
    pub fn get_branch_scoped_prefix(
        &self,
        workload_scope_prefix: &str,
        branch: &str,
    ) -> BTreeMap<String, EvidenceRecord> {
        let mut merged = BTreeMap::<String, EvidenceRecord>::new();
        let Ok(scopes) = fs::read_dir(self.path.join("scoped")) else {
            return merged;
        };
        let branch_keys = [
            collision_safe_component(branch),
            legacy_collision_safe_component(branch),
        ];
        for scope in scopes.flatten() {
            for records in branch_keys.iter().filter_map(|branch_key| {
                Self::load_records(&scope.path().join(format!("{branch_key}.json"))).ok()
            }) {
                for (target, record) in records {
                    if !record
                        .workload_scope
                        .as_deref()
                        .is_some_and(|scope| scope.starts_with(workload_scope_prefix))
                    {
                        continue;
                    }
                    if merged
                        .get(&target)
                        .is_none_or(|existing| record.completed_at > existing.completed_at)
                    {
                        merged.insert(target, record);
                    }
                }
            }
        }
        merged
    }

    /// Return evidence for a specific branch and target, if present.
    #[must_use]
    pub fn get_target(&self, branch: &str, target_name: &str) -> Option<EvidenceRecord> {
        self.get_branch(branch).remove(target_name)
    }

    /// Return evidence for one scoped branch and target, if present.
    #[must_use]
    pub fn get_target_scoped(
        &self,
        workload_scope: &str,
        branch: &str,
        target_name: &str,
    ) -> Option<EvidenceRecord> {
        self.get_branch_scoped(workload_scope, branch)
            .remove(target_name)
    }

    fn load_scoped_records(
        &self,
        workload_scope: &str,
        branch: &str,
    ) -> Result<BTreeMap<String, EvidenceRecord>, Box<dyn std::error::Error>> {
        let scoped = self.path.join("scoped");
        let current = scoped
            .join(collision_safe_component(workload_scope))
            .join(format!("{}.json", collision_safe_component(branch)));
        let legacy = scoped
            .join(legacy_collision_safe_component(workload_scope))
            .join(format!("{}.json", legacy_collision_safe_component(branch)));
        let mut merged = Self::load_records(&legacy)?;
        for (target, record) in Self::load_records(&current)? {
            if merged
                .get(&target)
                .is_none_or(|existing| record.completed_at >= existing.completed_at)
            {
                merged.insert(target, record);
            }
        }
        Ok(merged)
    }

    /// Return whether every required platform has passing evidence for the SHA.
    #[must_use]
    pub fn is_merge_ready(
        &self,
        branch: &str,
        sha: &str,
        required_platforms: &[String],
    ) -> (bool, BTreeMap<String, Option<EvidenceRecord>>) {
        let records = self.get_branch(branch);
        let mut evidence_map = BTreeMap::new();
        let mut all_green = true;

        for platform in required_platforms {
            let record = records
                .values()
                .find(|record| record.platform == *platform && record.sha == sha && record.passed())
                .cloned();
            if record.is_none() {
                all_green = false;
            }
            evidence_map.insert(platform.clone(), record);
        }

        (all_green, evidence_map)
    }

    /// Find the highest-ranked passing record for a target across all branches.
    #[must_use]
    pub fn query_passing_for_target(
        &self,
        target_name: &str,
        sha_candidates: &[String],
    ) -> Option<EvidenceRecord> {
        Self::query_passing_in_directory(&self.path, target_name, sha_candidates)
    }

    /// Find the highest-ranked passing record inside one workload namespace.
    #[must_use]
    pub fn query_passing_for_target_scoped(
        &self,
        workload_scope: &str,
        target_name: &str,
        sha_candidates: &[String],
    ) -> Option<EvidenceRecord> {
        let directories = self.scoped_directories(workload_scope);
        Self::query_passing_in_directories(
            directories.iter().map(PathBuf::as_path),
            target_name,
            sha_candidates,
        )
    }

    /// Return every non-reused passing record for one target and exact SHA,
    /// newest first, so callers can apply their complete contract predicate.
    #[must_use]
    pub fn passing_records_for_target_sha(
        &self,
        target_name: &str,
        sha: &str,
    ) -> Vec<EvidenceRecord> {
        Self::passing_records_in_directory(&self.path, target_name, sha)
    }

    /// Return exact-SHA passing records inside one workload namespace.
    #[must_use]
    pub fn passing_records_for_target_sha_scoped(
        &self,
        workload_scope: &str,
        target_name: &str,
        sha: &str,
    ) -> Vec<EvidenceRecord> {
        let directories = self.scoped_directories(workload_scope);
        Self::passing_records_in_directories(
            directories.iter().map(PathBuf::as_path),
            target_name,
            sha,
        )
    }

    /// Return exact-SHA passing records across scoped workloads whose stored
    /// scope starts with `workload_scope_prefix`.
    #[must_use]
    pub fn passing_records_for_target_sha_scoped_prefix(
        &self,
        workload_scope_prefix: &str,
        target_name: &str,
        sha: &str,
    ) -> Vec<EvidenceRecord> {
        let Ok(scopes) = fs::read_dir(self.path.join("scoped")) else {
            return Vec::new();
        };
        let mut records = scopes
            .flatten()
            .filter_map(|scope| fs::read_dir(scope.path()).ok())
            .flat_map(Iterator::flatten)
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(std::ffi::OsStr::to_str) == Some("json"))
                    .then(|| Self::load_records(&path).ok())
                    .flatten()
            })
            .flat_map(BTreeMap::into_values)
            .filter(|record| {
                record
                    .workload_scope
                    .as_deref()
                    .is_some_and(|scope| scope.starts_with(workload_scope_prefix))
                    && record.target_name == target_name
                    && record.sha == sha
                    && record.passed()
                    && !record.reused()
            })
            .collect::<Vec<_>>();
        records.sort_by_key(|record| std::cmp::Reverse(record.completed_at));
        records
    }

    fn query_passing_in_directory(
        directory: &Path,
        target_name: &str,
        sha_candidates: &[String],
    ) -> Option<EvidenceRecord> {
        Self::query_passing_in_directories(std::iter::once(directory), target_name, sha_candidates)
    }

    fn query_passing_in_directories<'a>(
        directories: impl IntoIterator<Item = &'a Path>,
        target_name: &str,
        sha_candidates: &[String],
    ) -> Option<EvidenceRecord> {
        let candidate_ranks = sha_candidates
            .iter()
            .enumerate()
            .map(|(rank, sha)| (sha.as_str(), rank))
            .collect::<BTreeMap<_, _>>();
        let mut best: Option<(usize, EvidenceRecord)> = None;
        for directory in directories {
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            for records in entries.flatten().filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(std::ffi::OsStr::to_str) == Some("json"))
                    .then(|| Self::load_records(&path).ok())
                    .flatten()
            }) {
                for record in records.values() {
                    if record.target_name != target_name || !record.passed() || record.reused() {
                        continue;
                    }
                    let Some(rank) = candidate_ranks.get(record.sha.as_str()).copied() else {
                        continue;
                    };
                    if best.as_ref().is_none_or(|(best_rank, current)| {
                        rank < *best_rank
                            || (rank == *best_rank && record.completed_at > current.completed_at)
                    }) {
                        best = Some((rank, record.clone()));
                    }
                }
            }
        }
        best.map(|(_, record)| record)
    }

    fn passing_records_in_directory(
        directory: &Path,
        target_name: &str,
        sha: &str,
    ) -> Vec<EvidenceRecord> {
        Self::passing_records_in_directories(std::iter::once(directory), target_name, sha)
    }

    fn passing_records_in_directories<'a>(
        directories: impl IntoIterator<Item = &'a Path>,
        target_name: &str,
        sha: &str,
    ) -> Vec<EvidenceRecord> {
        let mut records = directories
            .into_iter()
            .filter_map(|directory| fs::read_dir(directory).ok())
            .flat_map(Iterator::flatten)
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(std::ffi::OsStr::to_str) == Some("json"))
                    .then(|| Self::load_records(&path).ok())
                    .flatten()
            })
            .flat_map(BTreeMap::into_values)
            .filter(|record| {
                record.target_name == target_name
                    && record.sha == sha
                    && record.passed()
                    && !record.reused()
            })
            .collect::<Vec<_>>();
        records.sort_by_key(|record| std::cmp::Reverse(record.completed_at));
        records
    }

    fn scoped_directories(&self, workload_scope: &str) -> [PathBuf; 2] {
        let scoped = self.path.join("scoped");
        [
            scoped.join(collision_safe_component(workload_scope)),
            scoped.join(legacy_collision_safe_component(workload_scope)),
        ]
    }

    fn branch_file(&self, branch_key: &str) -> PathBuf {
        self.path.join(format!("{branch_key}.json"))
    }

    fn branch_lock_file(&self, branch_key: &str) -> PathBuf {
        self.path.join(format!("{branch_key}.lock"))
    }

    fn load_branch(
        &self,
        branch_key: &str,
    ) -> Result<BTreeMap<String, EvidenceRecord>, Box<dyn std::error::Error>> {
        let path = self.branch_file(branch_key);
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let contents = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&contents)?)
    }

    fn load_records(
        path: &Path,
    ) -> Result<BTreeMap<String, EvidenceRecord>, Box<dyn std::error::Error>> {
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let contents = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&contents)?)
    }

    fn save_branch(
        &self,
        branch_key: &str,
        records: &BTreeMap<String, EvidenceRecord>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let payload = serde_json::to_string_pretty(records)?;
        let temp = tempfile::NamedTempFile::new_in(&self.path)?;
        fs::write(temp.path(), format!("{payload}\n"))?;
        temp.persist(self.branch_file(branch_key))?;
        Ok(())
    }

    fn save_records(
        directory: &Path,
        branch_key: &str,
        records: &BTreeMap<String, EvidenceRecord>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        fs::create_dir_all(directory)?;
        let payload = serde_json::to_string_pretty(records)?;
        let temp = tempfile::NamedTempFile::new_in(directory)?;
        fs::write(temp.path(), format!("{payload}\n"))?;
        temp.persist(directory.join(format!("{branch_key}.json")))?;
        Ok(())
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

fn sanitize_branch(branch: &str) -> String {
    branch.replace(['/', '\\'], "--")
}

fn sanitize_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "command-evidence".to_owned()
    } else {
        sanitized
    }
}

fn collision_safe_component(value: &str) -> String {
    use sha2::{Digest, Sha256};

    let readable = sanitize_component(value);
    let digest = Sha256::digest(value.as_bytes());
    // Scoped evidence nests one encoded scope and one encoded branch. Keep
    // both components compact enough for Windows runners that still enforce
    // MAX_PATH while retaining a 128-bit digest (about 64 bits of birthday
    // collision security, which is sufficient for this local file namespace).
    format!(
        "{}--{}",
        readable.chars().take(16).collect::<String>(),
        &hex::encode(digest)[..32]
    )
}

fn legacy_collision_safe_component(value: &str) -> String {
    use sha2::{Digest, Sha256};

    let readable = sanitize_component(value);
    let digest = Sha256::digest(value.as_bytes());
    format!(
        "{}--{}",
        readable.chars().take(48).collect::<String>(),
        hex::encode(digest)
    )
}

/// Stable evidence namespace for repository-level validations that are not
/// owned by a single pull request.
#[must_use]
pub fn repository_evidence_scope(repository: &str) -> String {
    format!("repo:{}", canonical_repository(repository))
}

/// Stable evidence namespace for one repository-scoped ship workload.
#[must_use]
pub fn repository_ship_evidence_scope(repository: &str, pr: u64) -> String {
    format!("ship:{}:pr-{pr}", canonical_repository(repository))
}

/// Prefix shared by every PR-scoped ship evidence namespace in a repository.
#[must_use]
pub fn repository_ship_evidence_scope_prefix(repository: &str) -> String {
    format!("ship:{}:pr-", canonical_repository(repository))
}

/// Canonical GitHub repository slug used by all durable ownership keys.
#[must_use]
pub fn canonical_repository(repository: &str) -> String {
    repository.trim().to_ascii_lowercase()
}

/// Evidence namespace for a ship workload, with a checkout identity fallback
/// for legacy/offline requests that lack a repository slug.
#[must_use]
pub fn ship_evidence_scope(repository: &str, pr: u64, cwd: &Path) -> String {
    if repository.trim().is_empty() {
        format!("{}:ship-pr-{pr}", run_evidence_scope(cwd))
    } else {
        repository_ship_evidence_scope(repository, pr)
    }
}

/// Stable evidence namespace for one checkout-backed arbitrary run workload.
#[must_use]
pub fn run_evidence_scope(cwd: &Path) -> String {
    let identity = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    format!("run:{}", identity.to_string_lossy())
}

/// Exclusive scheduler claim for one scoped branch/target evidence writer.
///
/// Length-prefixing makes the tuple unambiguous before hashing, while the
/// readable target suffix keeps queue diagnostics useful.
#[must_use]
pub fn evidence_resource_claim(workload_scope: &str, branch: &str, target: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    for component in [workload_scope, branch, target] {
        hasher.update(component.len().to_be_bytes());
        hasher.update(component.as_bytes());
    }
    let digest = hasher.finalize();
    format!(
        "evidence:{}:{}",
        hex::encode(digest),
        sanitize_component(target)
    )
}

/// Stable scope for a named command workload in one checkout.
#[must_use]
pub fn command_evidence_scope(cwd: &Path, name: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(name.as_bytes());
    format!(
        "{}:command:{}--{}",
        run_evidence_scope(cwd),
        sanitize_component(name),
        hex::encode(digest)
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use chrono::Utc;

    use super::{
        CommandEvidenceStore, EvidenceRecord, EvidenceStore, collision_safe_component,
        legacy_collision_safe_component, repository_ship_evidence_scope,
        repository_ship_evidence_scope_prefix,
    };

    fn record(branch: &str, target: &str, sha: &str) -> EvidenceRecord {
        EvidenceRecord {
            sha: sha.to_owned(),
            branch: branch.to_owned(),
            workload_scope: None,
            target_name: target.to_owned(),
            validation_build_type: None,
            platform: format!("{target}-platform"),
            status: "pass".to_owned(),
            backend: "local".to_owned(),
            source_head_sha: None,
            source_tree_sha: None,
            source_checkout_clean: None,
            full_execution: None,
            completed_at: Utc::now(),
            duration_secs: None,
            host: None,
            primary_backend: None,
            failover_reason: None,
            provider: None,
            runner_profile: None,
            failure_class: None,
            reused_from: None,
            contract_digest: None,
            stages_signature: None,
        }
    }

    #[test]
    fn evidence_record_round_trips_reuse_fields() {
        let record = EvidenceRecord {
            sha: "new".to_owned(),
            branch: "feat/x".to_owned(),
            workload_scope: None,
            target_name: "mac".to_owned(),
            validation_build_type: Some("release".to_owned()),
            platform: "macos-arm64".to_owned(),
            status: "pass".to_owned(),
            backend: "reused".to_owned(),
            source_head_sha: Some("new".to_owned()),
            source_tree_sha: Some("tree".to_owned()),
            source_checkout_clean: Some(true),
            full_execution: Some(true),
            completed_at: Utc::now(),
            duration_secs: None,
            host: None,
            primary_backend: None,
            failover_reason: None,
            provider: None,
            runner_profile: None,
            failure_class: None,
            reused_from: Some("old".to_owned()),
            contract_digest: Some("abc123".to_owned()),
            stages_signature: Some("build|test".to_owned()),
        };

        assert!(record.passed());
        assert!(record.reused());

        let value = serde_json::to_value(&record).expect("serialize");
        assert_eq!(value["target"], "mac");
        assert_eq!(value["validation_build_type"], "release");
        assert_eq!(value["reused_from"], "old");
        assert_eq!(value["contract_digest"], "abc123");
        assert_eq!(value["stages_signature"], "build|test");

        let restored: EvidenceRecord = serde_json::from_value(value).expect("deserialize");
        assert_eq!(restored, record);
    }

    #[test]
    fn evidence_record_omits_reuse_fields_when_absent() {
        let record = record("main", "mac", "abc");
        let value = serde_json::to_value(&record).expect("serialize");
        assert!(value.get("reused_from").is_none());
        assert!(value.get("target_name").is_none());
    }

    #[test]
    fn record_and_retrieve_branch_data() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(temp.path().join("evidence")).expect("store");
        let record = record("feat/x", "mac", "abc");

        store.record(&record).expect("record");

        assert_eq!(
            store.get_target("feat/x", "mac").expect("record").sha,
            "abc"
        );
        assert_eq!(store.get_branch("feat/x").len(), 1);
    }

    #[test]
    fn latest_record_overwrites_by_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(temp.path().join("evidence")).expect("store");

        store.record(&record("main", "mac", "old")).expect("record");
        store.record(&record("main", "mac", "new")).expect("record");

        assert_eq!(store.get_target("main", "mac").expect("record").sha, "new");
    }

    #[test]
    fn branch_names_are_safely_sanitized() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(temp.path().join("evidence")).expect("store");
        store
            .record(&record("feat/x\\nested", "mac", "abc"))
            .expect("record");

        assert!(store.path().join("feat--x--nested.json").exists());
        assert!(store.get_target("feat/x\\nested", "mac").is_some());
    }

    #[test]
    fn merge_ready_requires_all_required_platforms() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(temp.path().join("evidence")).expect("store");
        let mut mac = record("main", "mac", "abc");
        mac.platform = "macos-arm64".to_owned();
        let mut linux = record("main", "linux", "abc");
        linux.platform = "linux-x64".to_owned();
        store.record(&mac).expect("record");
        store.record(&linux).expect("record");

        let (ready, evidence) = store.is_merge_ready(
            "main",
            "abc",
            &["macos-arm64".to_owned(), "linux-x64".to_owned()],
        );

        assert!(ready);
        assert!(evidence.values().all(Option::is_some));
    }

    #[test]
    fn merge_ready_rejects_missing_wrong_or_failed_records() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(temp.path().join("evidence")).expect("store");

        let mut wrong_sha = record("main", "mac", "old");
        wrong_sha.platform = "macos-arm64".to_owned();
        let mut failed = record("main", "linux", "abc");
        failed.platform = "linux-x64".to_owned();
        failed.status = "fail".to_owned();
        store.record(&wrong_sha).expect("record");
        store.record(&failed).expect("record");

        let (ready, evidence) = store.is_merge_ready(
            "main",
            "abc",
            &["macos-arm64".to_owned(), "linux-x64".to_owned()],
        );

        assert!(!ready);
        assert!(evidence["macos-arm64"].is_none());
        assert!(evidence["linux-x64"].is_none());
    }

    #[test]
    fn store_persists_across_instances() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("evidence");
        let store = EvidenceStore::new(path.clone()).expect("store");
        store.record(&record("main", "mac", "abc")).expect("record");

        let reopened = EvidenceStore::new(path).expect("store");
        assert_eq!(
            reopened.get_target("main", "mac").expect("record").sha,
            "abc"
        );
    }

    #[test]
    fn scoped_store_isolates_pulp_forge_products_and_vellum() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(temp.path().join("evidence")).expect("store");
        let identities = [
            ("repo:generous-corp/pulp", "pulp-sha"),
            ("repo:generous-corp/forge:workload:modular", "modular-sha"),
            (
                "repo:generous-corp/forge:workload:sequencer",
                "sequencer-sha",
            ),
            ("repo:generous-corp/vellum", "vellum-sha"),
        ];

        for (scope, sha) in identities {
            store
                .record_scoped(scope, &record("feature/shared", "macos", sha))
                .expect("record scoped evidence");
        }

        for (scope, sha) in identities {
            let evidence = store
                .get_target_scoped(scope, "feature/shared", "macos")
                .expect("scoped record");
            assert_eq!(evidence.sha, sha);
            assert_eq!(evidence.workload_scope.as_deref(), Some(scope));
        }
        assert!(store.get_branch("feature/shared").is_empty());
    }

    #[test]
    fn scoped_evidence_keys_fit_windows_path_budget() {
        let scope = collision_safe_component(&"repository-workload-scope-".repeat(12));
        let branch = collision_safe_component(&"generated-agent-feature-branch-".repeat(12));
        let other_scope = collision_safe_component(&format!(
            "{}different",
            "repository-workload-scope-".repeat(12)
        ));

        assert!(scope.len() <= 50);
        assert!(branch.len() <= 50);
        assert_ne!(scope, other_scope);

        // GitHub-hosted Windows temp roots can already consume substantial
        // path space. Preserve headroom below legacy MAX_PATH for the final
        // atomic-persist destination.
        let relative = Path::new("scoped")
            .join(scope)
            .join(format!("{branch}.json"));
        let representative_temp_root_len = 96;
        assert!(representative_temp_root_len + 1 + relative.as_os_str().len() <= 240);
    }

    #[test]
    fn scoped_store_reads_and_migrates_legacy_long_keys() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("evidence");
        let store = EvidenceStore::new(root.clone()).expect("store");
        let scope = "repo:generous-corp/forge:workload:modular";
        let branch = "feature/legacy-scoped-evidence";
        let legacy_scope = legacy_collision_safe_component(scope);
        let legacy_branch = legacy_collision_safe_component(branch);
        let legacy_directory = root.join("scoped").join(legacy_scope);
        let mut records = BTreeMap::new();
        let mut legacy = record(branch, "macos", "legacy-sha");
        legacy.workload_scope = Some(scope.to_owned());
        records.insert("macos".to_owned(), legacy);
        EvidenceStore::save_records(&legacy_directory, &legacy_branch, &records)
            .expect("legacy records");

        assert_eq!(
            store
                .get_target_scoped(scope, branch, "macos")
                .expect("legacy lookup")
                .sha,
            "legacy-sha"
        );
        assert_eq!(
            store.get_branch_scoped_prefix("repo:generous-corp/forge", branch)["macos"].sha,
            "legacy-sha"
        );
        assert_eq!(
            store
                .query_passing_for_target_scoped(scope, "macos", &["legacy-sha".to_owned()],)
                .expect("legacy ranked lookup")
                .sha,
            "legacy-sha"
        );
        assert_eq!(
            store
                .passing_records_for_target_sha_scoped(scope, "macos", "legacy-sha")
                .len(),
            1
        );

        store
            .record_scoped(scope, &record(branch, "windows", "current-sha"))
            .expect("migrate and record");
        assert_eq!(
            store
                .get_target_scoped(scope, branch, "macos")
                .expect("migrated legacy target")
                .sha,
            "legacy-sha"
        );
        assert_eq!(
            store
                .get_target_scoped(scope, branch, "windows")
                .expect("current target")
                .sha,
            "current-sha"
        );
    }

    #[test]
    fn prefix_read_aggregates_pr_scopes_without_collapsing_storage() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(temp.path().join("evidence")).expect("store");
        let modular_scope = repository_ship_evidence_scope("Generous-Corp/forge", 127);
        let sequencer_scope = repository_ship_evidence_scope("Generous-Corp/forge", 128);
        let mut modular = record("main", "macos", "modular");
        modular.completed_at = Utc::now() - chrono::Duration::minutes(1);
        let mut sequencer = record("main", "macos", "sequencer");
        sequencer.completed_at = Utc::now();
        store
            .record_scoped(&modular_scope, &modular)
            .expect("modular evidence");
        store
            .record_scoped(&sequencer_scope, &sequencer)
            .expect("sequencer evidence");

        assert_eq!(
            store
                .get_target_scoped(&modular_scope, "main", "macos")
                .expect("modular scoped")
                .sha,
            "modular"
        );
        assert_eq!(
            store
                .get_target_scoped(&sequencer_scope, "main", "macos")
                .expect("sequencer scoped")
                .sha,
            "sequencer"
        );
        assert_eq!(
            store.get_branch_scoped_prefix(
                &repository_ship_evidence_scope_prefix("Generous-Corp/forge"),
                "main",
            )["macos"]
                .sha,
            "sequencer"
        );
    }

    #[test]
    fn scoped_store_does_not_alias_sanitized_branch_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(temp.path().join("evidence")).expect("store");
        let scope = "repo:generous-corp/forge";
        store
            .record_scoped(scope, &record("feature/x", "macos", "slash"))
            .expect("slash branch");
        store
            .record_scoped(scope, &record("feature--x", "macos", "dashes"))
            .expect("dash branch");

        assert_eq!(
            store
                .get_target_scoped(scope, "feature/x", "macos")
                .expect("slash")
                .sha,
            "slash"
        );
        assert_eq!(
            store
                .get_target_scoped(scope, "feature--x", "macos")
                .expect("dashes")
                .sha,
            "dashes"
        );
    }

    #[test]
    fn command_bundle_reservation_never_reuses_an_owned_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = CommandEvidenceStore::new(temp.path().join("command")).expect("store");

        let first = store.reserve_bundle_id("same-id").expect("first");
        let second = store.reserve_bundle_id("same-id").expect("second");
        let third = store.reserve_bundle_id("same-id").expect("third");

        assert_eq!(first, "same-id");
        assert_eq!(second, "same-id-1");
        assert_eq!(third, "same-id-2");
        assert!(store.bundle_dir(&first).is_dir());
        assert!(store.bundle_dir(&second).is_dir());
        assert!(store.bundle_dir(&third).is_dir());
    }

    #[test]
    fn branch_lock_preserves_two_handle_mutations() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(EvidenceStore::new(temp.path().join("evidence")).expect("store"));
        let writer = Arc::clone(&store);
        let (started_tx, started_rx) = mpsc::channel();

        let handle = store
            .with_branch_records_locked("main", |records| {
                let handle = thread::spawn(move || {
                    started_tx.send(()).expect("started");
                    writer
                        .record(&record("main", "linux", "def"))
                        .expect("record linux");
                });
                started_rx.recv().expect("started received");
                thread::sleep(Duration::from_millis(50));
                records.insert("mac".to_owned(), record("main", "mac", "abc"));
                Ok(handle)
            })
            .expect("locked mutation");
        handle.join().expect("writer thread");

        let records = store.get_branch("main");
        assert_eq!(records["mac"].sha, "abc");
        assert_eq!(records["linux"].sha, "def");
    }

    #[test]
    fn query_passing_for_target_uses_candidate_rank_and_filters_invalid_records() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(temp.path().join("evidence")).expect("store");

        store
            .record(&record("ba", "mac", &"a".repeat(40)))
            .expect("record");
        store
            .record(&record("bb", "mac", &"b".repeat(40)))
            .expect("record");
        store
            .record(&record("bc", "mac", &"c".repeat(40)))
            .expect("record");

        let mut reused = record("main", "mac", "abc");
        reused.backend = "reused".to_owned();
        reused.reused_from = Some("parent".to_owned());
        store.record(&reused).expect("record");

        let mut failed = record("main", "linux", "abc");
        failed.status = "fail".to_owned();
        store.record(&failed).expect("record");

        let candidates = vec!["b".repeat(40), "c".repeat(40), "a".repeat(40)];
        let match_record = store
            .query_passing_for_target("mac", &candidates)
            .expect("record");
        assert_eq!(match_record.sha, "b".repeat(40));

        let same_sha = "d".repeat(40);
        let mut older = record("old", "mac", &same_sha);
        older.completed_at = Utc::now() - chrono::Duration::hours(2);
        store.record(&older).expect("older record");
        let mut newer = record("new", "mac", &same_sha);
        newer.completed_at = Utc::now() - chrono::Duration::hours(1);
        store.record(&newer).expect("newer record");
        let newest = store
            .query_passing_for_target("mac", std::slice::from_ref(&same_sha))
            .expect("newest exact-sha record");
        assert_eq!(newest.branch, "new");
        let all_exact = store.passing_records_for_target_sha("mac", &same_sha);
        assert_eq!(
            all_exact
                .iter()
                .map(|record| record.branch.as_str())
                .collect::<Vec<_>>(),
            ["new", "old"]
        );
        assert!(
            store
                .query_passing_for_target("linux", &["abc".to_owned()])
                .is_none()
        );
        assert!(
            store
                .query_passing_for_target("unknown", &["abc".to_owned()])
                .is_none()
        );
    }
}
