//! A host-local store of the reuse records local validation runs write.
//!
//! A plan that keys executables against an earlier build needs that build's
//! record (link members, object dependencies, codemodel, verdicts) from a run
//! on the same toolchain. The local lane has no GitHub credentials, so it
//! cannot publish or fetch artifacts; its records stay on the host.
//!
//! Each run gets a fresh pending directory (exported to the stages as
//! [`RECORD_DIR_ENV`]). After the stages finish, whatever their outcome, the
//! directory is filed under `records/<commit>/<run>` only when it holds a
//! non-empty `job.json` that parses; otherwise it is removed and the reason
//! returned for the run log. The store keeps the newest [`KEEP`] records.
//!
//! [`select_base`] picks the record a plan compares against: the newest one
//! whose toolchain and platform are known and equal the plan's, and whose commit
//! is an ancestor of the plan's protected base. A record from a commit that is
//! not yet merged is never chosen: the code that wrote it is unreviewed.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

/// Environment variable naming the run's pending record directory.
pub const RECORD_DIR_ENV: &str = "SHIPYARD_REUSE_RECORD_DIR";
/// Records kept per store.
pub const KEEP: usize = 40;
/// The file a record must hold to be filed.
pub const JOB_FILE: &str = "job.json";

/// Where one repository's records live under Shipyard's state directory.
#[must_use]
pub fn store_dir(state_dir: &Path, repository: &str) -> PathBuf {
    state_dir
        .join("reuse-records")
        .join(repository.replace('/', "__"))
}

/// Create a fresh pending directory for a run of `commit` (mode 0700).
///
/// # Errors
///
/// The I/O error when the directory cannot be created.
pub fn create_pending(store: &Path, commit: &str, now: DateTime<Utc>) -> std::io::Result<PathBuf> {
    let pending = store.join("pending");
    fs::create_dir_all(&pending)?;
    let dir = pending.join(format!(
        "{commit}-{}-{}",
        now.timestamp_nanos_opt().unwrap_or_default(),
        std::process::id()
    ));
    fs::create_dir(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// What filing a pending directory did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Filed {
    /// Kept at this path.
    Kept(PathBuf),
    /// Removed, with the reason.
    Discarded(String),
}

/// File `pending` (a run of `commit`) into the store, then prune.
///
/// # Errors
///
/// The I/O error when a kept record cannot be moved or the store pruned.
pub fn file(store: &Path, pending: &Path, commit: &str) -> std::io::Result<Filed> {
    let job = pending.join(JOB_FILE);
    let verdict = match fs::read(&job) {
        Err(_) => Some(format!("no {JOB_FILE} was written")),
        Ok(bytes) if bytes.is_empty() => Some(format!("{JOB_FILE} is empty")),
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .err()
            .map(|error| format!("{JOB_FILE} does not parse: {error}")),
    };
    if let Some(reason) = verdict {
        let _ = fs::remove_dir_all(pending);
        return Ok(Filed::Discarded(reason));
    }
    let run = pending
        .file_name()
        .map_or_else(|| "run".into(), |name| name.to_string_lossy().into_owned());
    let dest = store.join("records").join(commit).join(run);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(pending, &dest)?;
    prune(store, KEEP)?;
    Ok(Filed::Kept(dest))
}

/// One filed record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredRecord {
    /// The commit the run validated.
    pub commit: String,
    /// The record directory.
    pub path: PathBuf,
    /// The parsed `job.json`.
    pub job: Value,
    /// When it was filed (directory modification time), for ordering.
    pub filed_at: std::time::SystemTime,
}

/// Every filed record, newest first. Unreadable entries are skipped.
#[must_use]
pub fn list(store: &Path) -> Vec<StoredRecord> {
    let mut records = Vec::new();
    let Ok(commits) = fs::read_dir(store.join("records")) else {
        return records;
    };
    for commit in commits.flatten() {
        let commit_name = commit.file_name().to_string_lossy().into_owned();
        let Ok(runs) = fs::read_dir(commit.path()) else {
            continue;
        };
        for run in runs.flatten() {
            let path = run.path();
            let Ok(bytes) = fs::read(path.join(JOB_FILE)) else {
                continue;
            };
            let Ok(job) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            let Ok(filed_at) = run.metadata().and_then(|meta| meta.modified()) else {
                continue;
            };
            records.push(StoredRecord {
                commit: commit_name.clone(),
                path,
                job,
                filed_at,
            });
        }
    }
    records.sort_by(|a, b| {
        b.filed_at
            .cmp(&a.filed_at)
            .then_with(|| b.path.cmp(&a.path))
    });
    records
}

/// Keep the newest `keep` records; remove the rest and any emptied commit dir.
///
/// # Errors
///
/// The I/O error of a removal that fails.
pub fn prune(store: &Path, keep: usize) -> std::io::Result<()> {
    for stale in list(store).into_iter().skip(keep) {
        fs::remove_dir_all(&stale.path)?;
        if let Some(commit_dir) = stale.path.parent() {
            let _ = fs::remove_dir(commit_dir);
        }
    }
    Ok(())
}

/// Why no record was chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NoBase {
    /// The store holds no record at all.
    Empty,
    /// Records exist, none qualifies; the count of each refusal.
    NoneQualify {
        /// Toolchain or platform unknown on the record.
        unknown_identity: usize,
        /// Known, but differs from the plan's.
        other_identity: usize,
        /// Matching, but its commit is not an ancestor of the protected base.
        not_merged: usize,
    },
}

/// Choose the plan's base record.
///
/// `identity` reads a record's `(toolchain, platform)` from its `job.json`,
/// `None` for either when the record does not state it; `want` is the plan's
/// own. `merged(commit)` says whether `commit` is an ancestor of the protected
/// base. The newest record passing all three wins.
///
/// # Errors
///
/// [`NoBase`] saying why nothing qualified.
pub fn select_base<I, M>(
    store: &Path,
    want: (&str, &str),
    identity: I,
    merged: M,
) -> Result<StoredRecord, NoBase>
where
    I: Fn(&Value) -> (Option<String>, Option<String>),
    M: Fn(&str) -> bool,
{
    let records = list(store);
    if records.is_empty() {
        return Err(NoBase::Empty);
    }
    let (mut unknown_identity, mut other_identity, mut not_merged) = (0, 0, 0);
    for record in records {
        match identity(&record.job) {
            (Some(toolchain), Some(platform)) => {
                if (toolchain.as_str(), platform.as_str()) != want {
                    other_identity += 1;
                } else if !merged(&record.commit) {
                    not_merged += 1;
                } else {
                    return Ok(record);
                }
            }
            _ => unknown_identity += 1,
        }
    }
    Err(NoBase::NoneQualify {
        unknown_identity,
        other_identity,
        not_merged,
    })
}

#[cfg(test)]
mod tests;
