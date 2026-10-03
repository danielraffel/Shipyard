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
//! whose platform and toolchain are stated and equal the plan's, that passes
//! the record format's own usability rules, and whose commit is an ancestor of
//! the plan's protected base. A record from a commit that is not yet merged is
//! never chosen: the code that wrote it is unreviewed. A pending directory a
//! cancelled run left behind is swept after [`PENDING_MAX_AGE`].

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
/// A pending directory older than this belongs to a run that never reached
/// filing (cancelled, or its process killed) and is removed.
pub const PENDING_MAX_AGE: std::time::Duration = std::time::Duration::from_hours(24);

/// Remove pending directories older than `max_age`.
fn sweep_pending(pending: &Path, max_age: std::time::Duration) {
    let Ok(entries) = fs::read_dir(pending) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

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
    sweep_pending(&pending, PENDING_MAX_AGE);
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
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Refusals {
    /// The record does not state its platform (arch and OS family).
    pub unknown_platform: usize,
    /// Its platform differs from the plan's.
    pub other_platform: usize,
    /// It does not state its toolchain, or states it as unknown.
    pub unknown_toolchain: usize,
    /// Its toolchain differs from the plan's.
    pub other_toolchain: usize,
    /// It fails the record format's own usability rules.
    pub unusable: usize,
    /// Its commit is not an ancestor of the protected base.
    pub not_merged: usize,
}

/// Why no record was chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NoBase {
    /// The store holds no record at all.
    Empty,
    /// Records exist and none qualifies.
    NoneQualify(Refusals),
}

/// What a plan requires of its base record. The record format belongs to the
/// project, so it says how to read a record; the store decides the order.
pub trait BaseCriteria {
    /// The record's platform (architecture and OS family), `None` when unstated.
    fn platform(&self, job: &Value) -> Option<String>;
    /// The record's toolchain identity, `None` when any part of it is unstated
    /// or unknown.
    fn toolchain(&self, job: &Value) -> Option<String>;
    /// Why the record cannot be keyed against (for example, its link-member or
    /// object-dependency record is marked unusable), `None` when it can.
    fn unusable(&self, record: &StoredRecord) -> Option<String>;
    /// Whether `commit` is an ancestor of the plan's protected base.
    fn merged(&self, commit: &str) -> bool;
}

/// A stated value: neither empty nor the literal `unknown`.
fn stated(value: Option<String>) -> Option<String> {
    value.filter(|v| {
        let v = v.trim();
        !v.is_empty() && !v.eq_ignore_ascii_case("unknown")
    })
}

/// Choose the plan's base record: the newest one whose platform, then
/// toolchain, are stated and equal `want_platform` and `want_toolchain`, that
/// passes the format's usability rules, and whose commit is merged. Platform
/// is checked first, since a record from another OS or architecture can carry
/// a plausible-looking toolchain.
///
/// # Errors
///
/// [`NoBase`] saying why nothing qualified.
pub fn select_base<C: BaseCriteria>(
    store: &Path,
    want_platform: &str,
    want_toolchain: &str,
    criteria: &C,
) -> Result<StoredRecord, NoBase> {
    let records = list(store);
    if records.is_empty() {
        return Err(NoBase::Empty);
    }
    let mut refused = Refusals::default();
    for record in records {
        match stated(criteria.platform(&record.job)) {
            None => refused.unknown_platform += 1,
            Some(platform) if platform != want_platform => refused.other_platform += 1,
            Some(_) => match stated(criteria.toolchain(&record.job)) {
                None => refused.unknown_toolchain += 1,
                Some(toolchain) if toolchain != want_toolchain => refused.other_toolchain += 1,
                Some(_) if criteria.unusable(&record).is_some() => refused.unusable += 1,
                Some(_) if !criteria.merged(&record.commit) => refused.not_merged += 1,
                Some(_) => return Ok(record),
            },
        }
    }
    Err(NoBase::NoneQualify(refused))
}

#[cfg(test)]
mod tests;
