use std::cmp::Reverse;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Persistent record for one `shipyard cloud run` dispatch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CloudRunRecord {
    /// Stable local dispatch identifier.
    pub dispatch_id: String,
    /// Workflow key selected by Shipyard.
    pub workflow_key: String,
    /// GitHub workflow filename.
    pub workflow_file: String,
    /// Human-readable workflow name.
    pub workflow_name: String,
    /// Optional owner/repo override for dispatch and refresh calls.
    pub repository: Option<String>,
    /// Git ref requested at dispatch time.
    pub requested_ref: String,
    /// Runner provider requested for the workflow.
    pub provider: String,
    /// `workflow_dispatch` input fields passed to GitHub.
    pub dispatch_fields: std::collections::BTreeMap<String, String>,
    /// Last observed GitHub Actions status.
    pub status: String,
    /// Last observed terminal conclusion, when any.
    pub conclusion: Option<String>,
    /// GitHub Actions workflow run ID, once discovered.
    pub run_id: Option<String>,
    /// GitHub Actions run URL, once discovered.
    pub url: Option<String>,
    /// Time the local dispatch record was created.
    pub dispatched_at: Option<DateTime<Utc>>,
    /// Time the run was observed as started.
    pub started_at: Option<DateTime<Utc>>,
    /// Time the run was observed as completed.
    pub completed_at: Option<DateTime<Utc>>,
    /// Time the record was last updated.
    pub updated_at: Option<DateTime<Utc>>,
}

impl CloudRunRecord {
    /// Create a dispatch record with Python-compatible defaults.
    #[must_use]
    pub fn new(
        dispatch_id: impl Into<String>,
        workflow_key: impl Into<String>,
        workflow_file: impl Into<String>,
        workflow_name: impl Into<String>,
        requested_ref: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            dispatch_id: dispatch_id.into(),
            workflow_key: workflow_key.into(),
            workflow_file: workflow_file.into(),
            workflow_name: workflow_name.into(),
            repository: None,
            requested_ref: requested_ref.into(),
            provider: provider.into(),
            dispatch_fields: std::collections::BTreeMap::new(),
            status: "dispatched".to_owned(),
            conclusion: None,
            run_id: None,
            url: None,
            dispatched_at: Some(now),
            started_at: None,
            completed_at: None,
            updated_at: Some(now),
        }
    }
}

/// JSON-backed store for cloud workflow dispatch records.
#[derive(Clone, Debug)]
pub struct CloudRecordStore {
    path: PathBuf,
}

impl CloudRecordStore {
    /// Open or create a cloud record store at `path`.
    pub fn new(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        crate::writer_domain_lease::ensure_protected_dir_all(&path)?;
        Ok(Self { path })
    }

    /// Create a Python-shaped local dispatch ID.
    #[must_use]
    pub fn new_dispatch_id(&self) -> String {
        let now = Utc::now();
        let time_bits = now
            .timestamp_nanos_opt()
            .and_then(|nanos| u64::try_from(nanos).ok())
            .unwrap_or_else(|| u64::try_from(now.timestamp_micros()).unwrap_or_default());
        let suffix = time_bits ^ u64::from(std::process::id());
        format!(
            "cloud-{}-{:08x}",
            now.format("%Y%m%d"),
            suffix & 0xffff_ffff
        )
    }

    /// Save a record and return the path written.
    pub fn save(&self, record: &CloudRunRecord) -> io::Result<PathBuf> {
        let target = self.path.join(format!("{}.json", record.dispatch_id));
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&target)?;
        let tmp = self.path.join(format!(
            "{}.json.tmp.{}",
            record.dispatch_id,
            std::process::id()
        ));
        let payload = serde_json::to_string_pretty(record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(&tmp, format!("{payload}\n"))?;
        fs::rename(&tmp, &target)?;
        Ok(target)
    }

    /// Load one record by dispatch ID.
    #[must_use]
    pub fn get(&self, dispatch_id: &str) -> Option<CloudRunRecord> {
        let path = self.path.join(format!("{dispatch_id}.json"));
        read_record(&path).ok()
    }

    /// List records newest-first, skipping corrupt files.
    #[must_use]
    pub fn list(&self, limit: usize) -> Vec<CloudRunRecord> {
        let Ok(entries) = fs::read_dir(&self.path) else {
            return Vec::new();
        };
        let mut records = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .filter_map(|path| read_record(&path).ok())
            .collect::<Vec<_>>();
        records.sort_by_key(|record| Reverse(record_sort_key(record)));
        records.truncate(limit);
        records
    }
}

fn read_record(path: &Path) -> io::Result<CloudRunRecord> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn record_sort_key(record: &CloudRunRecord) -> (Option<DateTime<Utc>>, String) {
    (
        record.updated_at.or(record.dispatched_at),
        record.dispatch_id.clone(),
    )
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn store_saves_gets_and_lists_newest_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = CloudRecordStore::new(temp.path()).expect("store");
        let mut older = CloudRunRecord::new("cloud-1", "ci", "ci.yml", "CI", "main", "namespace");
        older.updated_at = Some(Utc.with_ymd_and_hms(2026, 4, 24, 12, 0, 0).unwrap());
        let mut newer = CloudRunRecord::new("cloud-2", "ci", "ci.yml", "CI", "main", "namespace");
        newer.updated_at = Some(Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).unwrap());

        store.save(&older).expect("save older");
        store.save(&newer).expect("save newer");

        assert_eq!(store.get("cloud-1"), Some(older));
        assert_eq!(
            store
                .list(10)
                .into_iter()
                .map(|record| record.dispatch_id)
                .collect::<Vec<_>>(),
            vec!["cloud-2", "cloud-1"]
        );
    }

    #[test]
    fn list_skips_corrupt_records_and_honors_limit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = CloudRecordStore::new(temp.path()).expect("store");
        store
            .save(&CloudRunRecord::new(
                "cloud-1",
                "ci",
                "ci.yml",
                "CI",
                "main",
                "namespace",
            ))
            .expect("save");
        fs::write(temp.path().join("broken.json"), "{").expect("broken");

        assert_eq!(store.list(1).len(), 1);
    }

    #[test]
    fn dispatch_id_uses_python_shape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = CloudRecordStore::new(temp.path()).expect("store");
        let id = store.new_dispatch_id();
        assert!(id.starts_with("cloud-"));
        assert_eq!(id.len(), "cloud-20260425-12345678".len());
    }
}
