//! Durable, fail-closed reviewer-dispatch receipts.
//!
//! A request check says that a workflow asked for a review. It does not prove
//! that a reviewer session launched, stayed alive, or posted a verdict. This
//! module records the authenticated launcher/session identity and reconciles
//! that receipt with an independently observed exact head and verdict.
//!
//! The module never launches a reviewer, posts a verdict, or makes an approval
//! decision. It only produces evidence and bounded retry eligibility for the
//! caller that owns those actions.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

/// Current receipt schema.
pub const SCHEMA: &str = "shipyard.reviewer-dispatch/v1";

/// Result of the launcher attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchResult {
    /// The upstream launcher accepted the request but has not reported a session yet.
    Requested,
    /// The launcher created an authenticated reviewer session.
    Started,
    /// The launcher failed before creating a reviewer session.
    Failed,
}

impl LaunchResult {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Started => "started",
            Self::Failed => "failed",
        }
    }
}

/// One append-only dispatch attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchReceipt {
    /// [`SCHEMA`].
    pub schema: String,
    /// Canonical GitHub repository identity.
    pub repo: String,
    /// Pull request number.
    pub pr: u64,
    /// Full exact pull-request head SHA.
    pub head_sha: String,
    /// Reviewer identity the caller asked to launch.
    pub reviewer: String,
    /// Stable identity of this attempt, distinct across retries.
    pub attempt_id: String,
    /// One-based attempt number. The default policy permits one retry.
    pub attempt: u32,
    /// Authenticated launcher identity.
    pub launcher_identity: String,
    /// Authenticated reviewer-session identifier, when the launcher created one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Stable dispatch/request identifier from the upstream launcher.
    pub dispatch_id: String,
    /// Launcher attempt start time.
    pub started_at: DateTime<Utc>,
    /// Deadline after which the attempt is expired.
    pub deadline_at: DateTime<Utc>,
    /// Launcher outcome.
    pub result: LaunchResult,
    /// Required when `result` is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A verdict observed independently of the launcher receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedVerdict {
    /// Exact pull-request head named by the verdict.
    head_sha: String,
    /// Time at which the verdict was observed.
    posted_at: DateTime<Utc>,
}

impl ObservedVerdict {
    /// Construct evidence only after the caller has completed an authenticated
    /// exact-head review-gate read.
    pub(crate) fn trusted_exact_head(head_sha: String, posted_at: DateTime<Utc>) -> Self {
        Self {
            head_sha,
            posted_at,
        }
    }
}

/// Why a dispatch attempt is in its current state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationStatus {
    /// The launcher reported a failure before creating a session.
    LaunchFailed,
    /// A dispatch exists but its verdict has not been observed before expiry.
    Unobserved,
    /// The dispatch deadline elapsed without a trusted verdict.
    Expired,
    /// A trusted verdict names the dispatched exact head.
    VerdictPosted,
    /// The pull-request head changed after dispatch.
    HeadChanged,
}

impl ReconciliationStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::LaunchFailed => "launch_failed",
            Self::Unobserved => "unobserved",
            Self::Expired => "expired",
            Self::VerdictPosted => "verdict_posted",
            Self::HeadChanged => "head_changed",
        }
    }

    /// Whether a caller may make one bounded retry for this status.
    #[must_use]
    pub fn retryable(&self) -> bool {
        matches!(self, Self::LaunchFailed | Self::Unobserved | Self::Expired)
    }
}

/// Read-only reconciliation result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconciliation {
    /// [`SCHEMA`].
    pub schema: String,
    /// Canonical GitHub repository identity.
    pub repo: String,
    /// Pull request number.
    pub pr: u64,
    /// Exact head associated with the dispatch.
    pub head_sha: String,
    /// Dispatch attempt identity.
    pub attempt_id: String,
    /// One-based dispatch attempt number.
    pub attempt: u32,
    /// Reconciled state.
    pub status: ReconciliationStatus,
    /// Whether the caller may make its one bounded retry.
    pub retry_allowed: bool,
    /// Evidence explaining the state.
    pub detail: String,
    /// Time of reconciliation.
    pub observed_at: DateTime<Utc>,
    /// Time of the matching trusted verdict, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict_posted_at: Option<DateTime<Utc>>,
}

/// Validate one receipt before it can be persisted or observed.
pub fn validate(receipt: &DispatchReceipt) -> Result<(), String> {
    if receipt.schema != SCHEMA {
        return Err(format!(
            "unsupported reviewer dispatch schema {:?}",
            receipt.schema
        ));
    }
    if !valid_repo(&receipt.repo) {
        return Err(format!("invalid repository {:?}", receipt.repo));
    }
    if receipt.pr == 0 {
        return Err("pull request number must be positive".to_owned());
    }
    if !valid_sha(&receipt.head_sha) {
        return Err("head_sha must be a full 40-character hexadecimal SHA".to_owned());
    }
    for (name, value) in [
        ("reviewer", receipt.reviewer.as_str()),
        ("attempt_id", receipt.attempt_id.as_str()),
        ("launcher_identity", receipt.launcher_identity.as_str()),
        ("dispatch_id", receipt.dispatch_id.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!("{name} must not be empty"));
        }
    }
    if receipt.attempt == 0 {
        return Err("attempt must be positive".to_owned());
    }
    if receipt.deadline_at <= receipt.started_at {
        return Err("deadline_at must be after started_at".to_owned());
    }
    if matches!(receipt.result, LaunchResult::Started)
        && receipt
            .session_id
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err("started dispatches require a session_id".to_owned());
    }
    if matches!(receipt.result, LaunchResult::Failed)
        && receipt
            .error
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err("failed dispatches require an error".to_owned());
    }
    if receipt
        .session_id
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err("session_id must not be empty".to_owned());
    }
    Ok(())
}

fn valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !name.is_empty()
        && owner != "."
        && owner != ".."
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !repo.contains('\\')
        && !repo.chars().any(char::is_whitespace)
}

fn valid_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Append one validated receipt and sync the file before returning.
pub fn append(path: &Path, receipt: &DispatchReceipt) -> Result<(), String> {
    validate(receipt)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| format!("create receipt directory: {error}"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("open receipt store {}: {error}", path.display()))?;
    file.lock_exclusive()
        .map_err(|error| format!("lock receipt store {}: {error}", path.display()))?;
    let existing = read_locked(&file, path)?;
    if existing
        .iter()
        .any(|row| row.attempt_id == receipt.attempt_id)
    {
        let _ = file.unlock();
        return Err(format!(
            "receipt attempt_id {:?} is already recorded",
            receipt.attempt_id
        ));
    }
    if existing.iter().any(|row| {
        row.repo == receipt.repo
            && row.pr == receipt.pr
            && row.reviewer == receipt.reviewer
            && row.head_sha.eq_ignore_ascii_case(&receipt.head_sha)
            && row.attempt >= receipt.attempt
    }) {
        let _ = file.unlock();
        return Err(format!(
            "receipt attempt {} is not newer for {} PR #{} reviewer {} head {}",
            receipt.attempt, receipt.repo, receipt.pr, receipt.reviewer, receipt.head_sha
        ));
    }
    let line = serde_json::to_vec(receipt).map_err(|error| format!("encode receipt: {error}"))?;
    let write_result = file
        .write_all(&line)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_data())
        .map_err(|error| format!("write receipt store {}: {error}", path.display()));
    let unlock_result = file
        .unlock()
        .map_err(|error| format!("unlock receipt store {}: {error}", path.display()));
    write_result?;
    unlock_result?;
    sync_parent(parent).map_err(|error| format!("sync receipt directory: {error}"))
}

fn sync_parent(parent: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

/// Read every receipt, refusing malformed or invalid rows.
pub fn read(path: &Path) -> Result<Vec<DispatchReceipt>, String> {
    let file = File::open(path)
        .map_err(|error| format!("open receipt store {}: {error}", path.display()))?;
    file.lock_shared()
        .map_err(|error| format!("lock receipt store {}: {error}", path.display()))?;
    let out = read_locked(&file, path)?;
    file.unlock()
        .map_err(|error| format!("unlock receipt store {}: {error}", path.display()))?;
    Ok(out)
}

/// Append one reconciliation observation and sync it before returning.
pub fn append_observation(path: &Path, observation: &Reconciliation) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| format!("create observation directory: {error}"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("open observation store {}: {error}", path.display()))?;
    file.lock_exclusive()
        .map_err(|error| format!("lock observation store {}: {error}", path.display()))?;
    let line =
        serde_json::to_vec(observation).map_err(|error| format!("encode observation: {error}"))?;
    let write_result = file
        .write_all(&line)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_data())
        .map_err(|error| format!("write observation store {}: {error}", path.display()));
    let unlock_result = file
        .unlock()
        .map_err(|error| format!("unlock observation store {}: {error}", path.display()));
    write_result?;
    unlock_result?;
    sync_parent(parent).map_err(|error| format!("sync observation directory: {error}"))
}

fn read_locked(file: &File, path: &Path) -> Result<Vec<DispatchReceipt>, String> {
    let mut clone = file
        .try_clone()
        .map_err(|error| format!("clone receipt store {}: {error}", path.display()))?;
    clone
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek receipt store {}: {error}", path.display()))?;
    let mut out = Vec::new();
    for (line, raw) in BufReader::new(clone).lines().enumerate() {
        let raw = raw.map_err(|error| format!("read receipt line {}: {error}", line + 1))?;
        if raw.trim().is_empty() {
            continue;
        }
        let receipt: DispatchReceipt = serde_json::from_str(&raw)
            .map_err(|error| format!("receipt line {} is invalid JSON: {error}", line + 1))?;
        validate(&receipt).map_err(|error| format!("receipt line {}: {error}", line + 1))?;
        out.push(receipt);
    }
    Ok(out)
}

/// Reconcile one receipt against independently authenticated observations.
#[must_use]
pub fn reconcile(
    receipt: &DispatchReceipt,
    now: DateTime<Utc>,
    current_head: &str,
    verdict: Option<&ObservedVerdict>,
    retry_allowed: bool,
) -> Reconciliation {
    let (status, detail, verdict_posted_at) =
        if !current_head.eq_ignore_ascii_case(&receipt.head_sha) {
            (
                ReconciliationStatus::HeadChanged,
                format!(
                    "current head {current_head} differs from dispatched head {}",
                    receipt.head_sha
                ),
                None,
            )
        } else if matches!(receipt.result, LaunchResult::Failed) {
            (
                ReconciliationStatus::LaunchFailed,
                receipt
                    .error
                    .clone()
                    .unwrap_or_else(|| "launcher failed".to_owned()),
                None,
            )
        } else if let Some(verdict) = verdict.filter(|verdict| {
            verdict.head_sha.eq_ignore_ascii_case(&receipt.head_sha)
                && verdict.posted_at >= receipt.started_at
                && verdict.posted_at <= receipt.deadline_at
                && verdict.posted_at <= now
        }) {
            (
                ReconciliationStatus::VerdictPosted,
                "trusted verdict names the dispatched exact head".to_owned(),
                Some(verdict.posted_at),
            )
        } else if now >= receipt.deadline_at {
            (
                ReconciliationStatus::Expired,
                format!(
                    "no trusted exact-head verdict by {}",
                    receipt.deadline_at.to_rfc3339()
                ),
                None,
            )
        } else {
            (
                ReconciliationStatus::Unobserved,
                "dispatch exists, but no trusted exact-head verdict is observed yet".to_owned(),
                None,
            )
        };
    let retry_allowed = retry_allowed && !matches!(status, ReconciliationStatus::Unobserved);
    Reconciliation {
        schema: SCHEMA.to_owned(),
        repo: receipt.repo.clone(),
        pr: receipt.pr,
        head_sha: receipt.head_sha.clone(),
        attempt_id: receipt.attempt_id.clone(),
        attempt: receipt.attempt,
        retry_allowed: status.retryable() && retry_allowed,
        status,
        detail,
        observed_at: now,
        verdict_posted_at,
    }
}

/// Whether this receipt is still eligible for the one bounded retry.
#[must_use]
pub fn retry_budget_available(
    receipts: &[DispatchReceipt],
    current: &DispatchReceipt,
    max_retries: u32,
) -> bool {
    if !(1..=1).contains(&max_retries) {
        return false;
    }
    let attempts = receipts
        .iter()
        .filter(|receipt| {
            receipt.repo == current.repo
                && receipt.pr == current.pr
                && receipt.reviewer == current.reviewer
                && receipt.head_sha.eq_ignore_ascii_case(&current.head_sha)
        })
        .map(|receipt| receipt.attempt)
        .max()
        .unwrap_or(current.attempt);
    attempts <= max_retries
}

/// Select the latest attempt for one exact repository/PR/reviewer/head tuple.
#[must_use]
pub fn latest<'a>(
    receipts: &'a [DispatchReceipt],
    repo: &str,
    pr: u64,
    reviewer: &str,
    head_sha: &str,
) -> Option<&'a DispatchReceipt> {
    receipts
        .iter()
        .filter(|receipt| {
            receipt.repo == repo
                && receipt.pr == pr
                && receipt.reviewer == reviewer
                && receipt.head_sha.eq_ignore_ascii_case(head_sha)
        })
        .max_by(|left, right| {
            left.attempt
                .cmp(&right.attempt)
                .then_with(|| left.started_at.cmp(&right.started_at))
                .then_with(|| left.attempt_id.cmp(&right.attempt_id))
        })
}

/// Select the latest attempt for one repository/PR/reviewer, regardless of
/// which exact head it named. This lets an observer report `head_changed`
/// instead of hiding the stale receipt behind a lookup miss.
#[must_use]
pub fn latest_for_subject<'a>(
    receipts: &'a [DispatchReceipt],
    repo: &str,
    pr: u64,
    reviewer: &str,
) -> Option<&'a DispatchReceipt> {
    receipts
        .iter()
        .filter(|receipt| receipt.repo == repo && receipt.pr == pr && receipt.reviewer == reviewer)
        .max_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.attempt.cmp(&right.attempt))
                .then_with(|| left.attempt_id.cmp(&right.attempt_id))
        })
}

/// Build a receipt path under a state directory without accepting a path-like
/// repository name as a filename.
#[must_use]
pub fn default_path(state_dir: &Path, repo: &str) -> PathBuf {
    let safe = repo.replace(['/', '\\'], "__");
    state_dir
        .join("reviewer-dispatch")
        .join(format!("{safe}.jsonl"))
}

/// Build the default durable observation path for one repository.
#[must_use]
pub fn default_observation_path(state_dir: &Path, repo: &str) -> PathBuf {
    let safe = repo.replace(['/', '\\'], "__");
    state_dir
        .join("reviewer-dispatch")
        .join(format!("{safe}.observations.jsonl"))
}

/// Derive an observation path beside an explicitly selected receipt store.
#[must_use]
pub fn observation_path_for_store(store: &Path) -> PathBuf {
    let mut path = store.to_owned();
    path.set_extension("observations.jsonl");
    path
}

impl std::fmt::Display for ReconciliationStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::fmt::Display for LaunchResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn time(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-10-29T00:{minutes:02}:00Z"))
            .unwrap()
            .with_timezone(&Utc)
    }

    fn receipt(result: LaunchResult) -> DispatchReceipt {
        DispatchReceipt {
            schema: SCHEMA.to_owned(),
            repo: "owner/repo".to_owned(),
            pr: 7,
            head_sha: "a".repeat(40),
            reviewer: "infra-review2".to_owned(),
            attempt_id: "attempt-1".to_owned(),
            attempt: 1,
            launcher_identity: "m3/reviewer-launcher".to_owned(),
            session_id: matches!(result, LaunchResult::Started).then(|| "session-1".to_owned()),
            dispatch_id: "dispatch-1".to_owned(),
            started_at: time(0),
            deadline_at: time(10),
            result,
            error: None,
        }
    }

    #[test]
    fn append_and_read_are_durable_and_validate() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested/receipts.jsonl");
        let receipt = receipt(LaunchResult::Started);
        append(&path, &receipt).unwrap();
        assert_eq!(read(&path).unwrap(), vec![receipt.clone()]);
        assert!(append(&path, &receipt).unwrap_err().contains("attempt_id"));
    }

    #[test]
    fn malformed_or_unauthenticated_receipts_refuse() {
        let mut row = receipt(LaunchResult::Requested);
        row.launcher_identity.clear();
        assert!(validate(&row).unwrap_err().contains("launcher_identity"));
        row = receipt(LaunchResult::Started);
        row.session_id = None;
        assert!(validate(&row).unwrap_err().contains("session_id"));
        row = receipt(LaunchResult::Failed);
        assert!(validate(&row).unwrap_err().contains("error"));
        row = receipt(LaunchResult::Started);
        row.session_id = Some("   ".to_owned());
        assert!(validate(&row).unwrap_err().contains("session_id"));
        row = receipt(LaunchResult::Failed);
        row.error = Some("   ".to_owned());
        assert!(validate(&row).unwrap_err().contains("error"));
        row.repo = r"owner\..\repo".to_owned();
        assert!(validate(&row).unwrap_err().contains("repository"));
    }

    #[test]
    fn reconciliation_is_fail_closed_and_exact_head_bound() {
        let failed = receipt(LaunchResult::Failed);
        let mut failed = failed;
        failed.error = Some("temporary launch failure".to_owned());
        let result = reconcile(&failed, time(1), &failed.head_sha, None, true);
        assert_eq!(result.status, ReconciliationStatus::LaunchFailed);
        assert!(result.retry_allowed);

        let started = receipt(LaunchResult::Started);
        let result = reconcile(&started, time(1), &started.head_sha, None, true);
        assert_eq!(result.status, ReconciliationStatus::Unobserved);
        assert!(!result.retry_allowed);

        let result = reconcile(&started, time(11), &started.head_sha, None, true);
        assert_eq!(result.status, ReconciliationStatus::Expired);
        assert!(result.retry_allowed);

        let verdict = ObservedVerdict::trusted_exact_head(started.head_sha.clone(), time(5));
        let result = reconcile(&started, time(6), &started.head_sha, Some(&verdict), true);
        assert_eq!(result.status, ReconciliationStatus::VerdictPosted);
        assert!(!result.retry_allowed);

        let early = ObservedVerdict::trusted_exact_head(
            started.head_sha.clone(),
            DateTime::parse_from_rfc3339("2026-10-28T23:59:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let result = reconcile(&started, time(1), &started.head_sha, Some(&early), true);
        assert_eq!(result.status, ReconciliationStatus::Unobserved);

        let mut failed_with_verdict = receipt(LaunchResult::Failed);
        failed_with_verdict.error = Some("launch failed".to_owned());
        let result = reconcile(
            &failed_with_verdict,
            time(6),
            &failed_with_verdict.head_sha,
            Some(&verdict),
            true,
        );
        assert_eq!(result.status, ReconciliationStatus::LaunchFailed);

        let result = reconcile(&started, time(6), &"b".repeat(40), Some(&verdict), true);
        assert_eq!(result.status, ReconciliationStatus::HeadChanged);
        assert!(!result.retry_allowed);
    }

    #[test]
    fn retry_budget_allows_one_retry_and_no_more() {
        let first = receipt(LaunchResult::Requested);
        assert!(retry_budget_available(
            std::slice::from_ref(&first),
            &first,
            1
        ));
        let mut second = first.clone();
        second.attempt = 2;
        second.attempt_id = "attempt-2".to_owned();
        assert!(!retry_budget_available(
            &[first, second.clone()],
            &second,
            1
        ));
    }

    #[test]
    fn latest_is_exact_tuple_and_attempt_ordered() {
        let first = receipt(LaunchResult::Requested);
        let mut second = first.clone();
        second.attempt = 2;
        second.attempt_id = "attempt-2".to_owned();
        assert_eq!(
            latest(
                &[first, second.clone()],
                "owner/repo",
                7,
                "infra-review2",
                &second.head_sha
            )
            .unwrap()
            .attempt,
            2
        );
        assert!(latest(&[second], "owner/repo", 7, "other", &"a".repeat(40)).is_none());
    }

    #[test]
    fn latest_for_subject_exposes_head_changes() {
        let receipt = receipt(LaunchResult::Started);
        let receipts = [receipt];
        let latest = latest_for_subject(&receipts, "owner/repo", 7, "infra-review2").unwrap();
        let result = reconcile(latest, time(1), &"b".repeat(40), None, true);
        assert_eq!(result.status, ReconciliationStatus::HeadChanged);
        assert!(!result.retry_allowed);
    }
}
