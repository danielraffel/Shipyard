//! Prepared-state cache for warm validation reruns.
//!
//! Prepared state is per-stage progress for one exact
//! `(sha, target, mode)` tuple. It is separate from merge evidence:
//! evidence gates PRs, while prepared state avoids rerunning setup or
//! build stages that already passed for the same commit and config.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A single stage outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StageOutcome {
    /// Stage name.
    pub stage: String,
    /// Outcome string, currently `pass` or `fail`.
    pub status: String,
}

impl StageOutcome {
    /// Return whether the recorded stage passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == "pass"
    }
}

/// Per-stage outcome cache for one `(sha, target, mode)` tuple.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedStateRecord {
    /// Commit SHA this record applies to.
    pub sha: String,
    /// Target name.
    pub target: String,
    /// Validation mode.
    pub mode: String,
    /// Stage outcomes by stage name.
    #[serde(default)]
    pub stages: BTreeMap<String, String>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Digest of stage command strings.
    #[serde(default)]
    pub config_hash: String,
}

impl PreparedStateRecord {
    /// Create an empty prepared-state record.
    #[must_use]
    pub fn new(
        sha: impl Into<String>,
        target: impl Into<String>,
        mode: impl Into<String>,
        config_hash: impl Into<String>,
    ) -> Self {
        Self {
            sha: sha.into(),
            target: target.into(),
            mode: mode.into(),
            stages: BTreeMap::new(),
            updated_at: Utc::now(),
            config_hash: config_hash.into(),
        }
    }

    /// Return whether a stage is recorded as passed.
    #[must_use]
    pub fn is_passed(&self, stage: &str) -> bool {
        self.stages
            .get(stage)
            .is_some_and(|status| status == "pass")
    }

    /// Record a stage outcome and refresh `updated_at`.
    pub fn mark(&mut self, stage: impl Into<String>, status: impl Into<String>) {
        self.stages.insert(stage.into(), status.into());
        self.updated_at = Utc::now();
    }
}

/// Persistent prepared-state store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedStateStore {
    path: PathBuf,
}

impl PreparedStateStore {
    /// Open a prepared-state store at `path`.
    pub fn new(path: PathBuf) -> Result<Self, std::io::Error> {
        crate::writer_domain_lease::ensure_protected_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Store root path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load a record, treating corrupt JSON as missing.
    #[must_use]
    pub fn get(&self, sha: &str, target: &str, mode: &str) -> Option<PreparedStateRecord> {
        let path = self.record_path(sha, target, mode);
        let contents = fs::read_to_string(path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    /// Save a record atomically.
    pub fn save(&self, record: &PreparedStateRecord) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.record_path(&record.sha, &record.target, &record.mode);
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&path)?;
        let parent = path.parent().expect("record path has parent");
        fs::create_dir_all(parent)?;
        let payload = serde_json::to_string_pretty(record)?;
        let temp = tempfile::NamedTempFile::new_in(parent)?;
        fs::write(temp.path(), format!("{payload}\n"))?;
        temp.persist(path).map_err(|error| error.error)?;
        Ok(())
    }

    /// Delete one record. Missing files are ignored.
    pub fn delete(&self, sha: &str, target: &str, mode: &str) -> Result<(), std::io::Error> {
        let path = self.record_path(sha, target, mode);
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&path)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Delete every record for a SHA. Returns the number of files removed.
    pub fn delete_sha(&self, sha: &str) -> Result<usize, std::io::Error> {
        let sha_dir = self.path.join(sanitize(sha));
        if !sha_dir.exists() {
            return Ok(0);
        }
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&sha_dir)?;
        let deleted = remove_json_files(&sha_dir)?;
        let _ = fs::remove_dir(&sha_dir);
        Ok(deleted)
    }

    /// Delete every record except those for `keep_sha`.
    pub fn cleanup_other_shas(&self, keep_sha: &str) -> Result<usize, std::io::Error> {
        if !self.path.exists() {
            return Ok(0);
        }
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
        let keep_dir = sanitize(keep_sha);
        let mut deleted = 0;
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_name() == keep_dir.as_str() {
                continue;
            }
            deleted += remove_json_files(&entry.path())?;
            let _ = fs::remove_dir(entry.path());
        }
        Ok(deleted)
    }

    fn record_path(&self, sha: &str, target: &str, mode: &str) -> PathBuf {
        self.path
            .join(sanitize(sha))
            .join(format!("{}--{}.json", sanitize(target), sanitize(mode)))
    }
}

/// Compute a stable digest of stage name and command pairs.
#[must_use]
pub fn hash_stage_commands(stages: &[(String, String)]) -> String {
    let mut ordered = stages.to_vec();
    ordered.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));

    let mut hasher = Sha256::new();
    for (stage, command) in ordered {
        hasher.update(stage.as_bytes());
        hasher.update([0]);
        hasher.update(command.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    hex::encode(&digest[..8])
}

/// Filter stages using a prepared-state record.
///
/// A stage is skipped only while the record exists, the command hash
/// matches, and every prior stage in this requested run is also marked
/// `pass`. Once one stage needs to run, every following stage runs too.
#[must_use]
pub fn filter_stages_by_prepared_state(
    stages: &[(String, String)],
    record: Option<&PreparedStateRecord>,
    current_config_hash: &str,
) -> (Vec<(String, String)>, Vec<String>) {
    let Some(record) = record else {
        return (stages.to_vec(), Vec::new());
    };
    if record.config_hash != current_config_hash {
        return (stages.to_vec(), Vec::new());
    }

    let mut skipped = Vec::new();
    let mut to_run = Vec::new();
    let mut skipping_phase = true;

    for (stage, command) in stages {
        if skipping_phase && record.is_passed(stage) {
            skipped.push(stage.clone());
            continue;
        }
        skipping_phase = false;
        to_run.push((stage.clone(), command.clone()));
    }

    (to_run, skipped)
}

fn remove_json_files(directory: &Path) -> Result<usize, std::io::Error> {
    let mut deleted = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.path().extension().is_some_and(|ext| ext == "json")
        {
            fs::remove_file(entry.path())?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

fn sanitize(value: &str) -> String {
    value.replace(['/', '\\'], "--").replace([':', ' '], "_")
}

#[cfg(test)]
mod tests {
    use super::{
        PreparedStateRecord, PreparedStateStore, StageOutcome, filter_stages_by_prepared_state,
        hash_stage_commands,
    };

    fn stages(values: &[(&str, &str)]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|(stage, command)| ((*stage).to_owned(), (*command).to_owned()))
            .collect()
    }

    #[test]
    fn stage_outcome_passed_checks_exact_status() {
        assert!(
            StageOutcome {
                stage: "setup".to_owned(),
                status: "pass".to_owned()
            }
            .passed()
        );
        assert!(
            !StageOutcome {
                stage: "setup".to_owned(),
                status: "fail".to_owned()
            }
            .passed()
        );
    }

    #[test]
    fn hash_stage_commands_is_stable_and_order_independent() {
        let a = hash_stage_commands(&stages(&[("setup", "./setup.sh"), ("build", "make")]));
        let b = hash_stage_commands(&stages(&[("build", "make"), ("setup", "./setup.sh")]));
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn hash_stage_commands_changes_when_command_or_stage_changes() {
        let base = hash_stage_commands(&stages(&[("build", "make")]));
        let changed_command = hash_stage_commands(&stages(&[("build", "ninja")]));
        let added_stage = hash_stage_commands(&stages(&[("build", "make"), ("test", "ctest")]));
        assert_ne!(base, changed_command);
        assert_ne!(base, added_stage);
    }

    #[test]
    fn record_marks_stages_and_round_trips_json() {
        let mut record = PreparedStateRecord::new("abc123", "ubuntu", "default", "hash");
        record.mark("setup", "pass");
        record.mark("build", "fail");

        assert!(record.is_passed("setup"));
        assert!(!record.is_passed("build"));
        assert!(!record.is_passed("test"));

        let payload = serde_json::to_string(&record).expect("serialize");
        let restored: PreparedStateRecord = serde_json::from_str(&payload).expect("deserialize");
        assert_eq!(restored, record);
    }

    #[test]
    fn store_saves_loads_overwrites_and_deletes_records() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = PreparedStateStore::new(temp.path().to_path_buf()).expect("store");
        assert!(store.get("abc", "ubuntu", "default").is_none());

        let mut record = PreparedStateRecord::new("abc", "ubuntu", "default", "hash");
        record.mark("setup", "fail");
        store.save(&record).expect("save");
        assert!(
            !store
                .get("abc", "ubuntu", "default")
                .expect("record")
                .is_passed("setup")
        );

        record.mark("setup", "pass");
        store.save(&record).expect("save");
        assert!(
            store
                .get("abc", "ubuntu", "default")
                .expect("record")
                .is_passed("setup")
        );

        store.delete("abc", "ubuntu", "default").expect("delete");
        assert!(store.get("abc", "ubuntu", "default").is_none());
    }

    #[test]
    fn store_corrupt_record_is_treated_as_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = PreparedStateStore::new(temp.path().to_path_buf()).expect("store");
        let path = temp.path().join("abc").join("ubuntu--default.json");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, "not valid json {{{").expect("write");
        assert!(store.get("abc", "ubuntu", "default").is_none());
    }

    #[test]
    fn store_delete_sha_and_cleanup_other_shas_count_deleted_records() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = PreparedStateStore::new(temp.path().to_path_buf()).expect("store");
        for sha in ["old1", "old2", "current"] {
            let mut record = PreparedStateRecord::new(sha, "ubuntu", "default", "hash");
            record.mark("setup", "pass");
            store.save(&record).expect("save");
        }

        assert_eq!(store.cleanup_other_shas("current").expect("cleanup"), 2);
        assert!(store.get("current", "ubuntu", "default").is_some());
        assert!(store.get("old1", "ubuntu", "default").is_none());
        assert!(store.get("old2", "ubuntu", "default").is_none());
        assert_eq!(store.delete_sha("current").expect("delete sha"), 1);
        assert!(store.get("current", "ubuntu", "default").is_none());
    }

    #[test]
    fn store_sanitizes_record_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = PreparedStateStore::new(temp.path().to_path_buf()).expect("store");
        let mut record = PreparedStateRecord::new("feature/x:y", "win host", "smoke/full", "hash");
        record.mark("setup", "pass");
        store.save(&record).expect("save");

        assert!(temp.path().join("feature--x_y").exists());
        assert!(store.get("feature/x:y", "win host", "smoke/full").is_some());
    }

    #[test]
    fn filter_no_record_or_hash_mismatch_runs_everything() {
        let stages = stages(&[("setup", "x"), ("build", "y")]);
        assert_eq!(
            filter_stages_by_prepared_state(&stages, None, "hash"),
            (stages.clone(), Vec::new())
        );

        let mut record = PreparedStateRecord::new("s", "t", "m", "old");
        record.mark("setup", "pass");
        assert_eq!(
            filter_stages_by_prepared_state(&stages, Some(&record), "new"),
            (stages, Vec::new())
        );
    }

    #[test]
    fn filter_skips_only_contiguous_passed_prefix() {
        let stages = stages(&[("setup", "x"), ("build", "y"), ("test", "z")]);
        let mut record = PreparedStateRecord::new("s", "t", "m", "hash");
        record.mark("setup", "pass");
        record.mark("test", "pass");

        let (to_run, skipped) = filter_stages_by_prepared_state(&stages, Some(&record), "hash");
        assert_eq!(skipped, vec!["setup"]);
        assert_eq!(
            to_run
                .iter()
                .map(|(stage, _)| stage.as_str())
                .collect::<Vec<_>>(),
            vec!["build", "test"]
        );
    }

    #[test]
    fn filter_all_passed_can_make_run_noop() {
        let stages = stages(&[("setup", "x"), ("build", "y")]);
        let mut record = PreparedStateRecord::new("s", "t", "m", "hash");
        record.mark("setup", "pass");
        record.mark("build", "pass");

        let (to_run, skipped) = filter_stages_by_prepared_state(&stages, Some(&record), "hash");
        assert!(to_run.is_empty());
        assert_eq!(skipped, vec!["setup", "build"]);
    }
}
