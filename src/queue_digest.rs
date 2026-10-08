//! Read-only census of durable GitHub queue-observer state.
//!
//! `queue-observe` intentionally owns one repository/base pair at a time. This
//! module is the bounded aggregation layer: it reads those durable snapshots,
//! preserves their exact identity, and refuses to call an incomplete census
//! healthy. It never contacts GitHub and never mutates observer state.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use crate::queue_observer::{CheckSnapshot, ObserverState, PullRequestSnapshot, load_state};

/// Default observer freshness window. A digest should not silently report an
/// old observer as the current GitHub queue.
pub const DEFAULT_STALE_AFTER_SECONDS: u64 = 900;

/// Stable output schema for queue digests.
pub const QUEUE_DIGEST_SCHEMA_VERSION: u32 = 1;

/// One observer's durable freshness and identity receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ObserverDigest {
    /// State file used for this observer.
    pub state_file: String,
    /// `owner/name` repository slug.
    pub repo: String,
    /// Observed base branch.
    pub base: String,
    /// Exact base-branch SHA in the observer snapshot.
    pub main_sha: String,
    /// SHA-256 of the canonical observer snapshot.
    pub state_hash: String,
    /// File modification time in RFC3339 UTC.
    pub modified_at: String,
    /// File age at digest time.
    pub age_seconds: u64,
    /// Whether the observer exceeded the configured freshness window.
    pub stale: bool,
    /// Whether the observer was conservative because a bounded source was
    /// unavailable.
    pub truncated: bool,
    /// Number of open pull requests represented by the snapshot.
    pub pull_request_count: usize,
    /// Number of merge-queue entries represented by the snapshot.
    pub queue_count: usize,
}

/// A pull request retained in an actionable digest bucket.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DigestPullRequest {
    /// Repository containing the pull request.
    pub repo: String,
    /// Base branch targeted by the pull request.
    pub base: String,
    /// Pull request number.
    pub number: u64,
    /// Full GitHub pull request URL.
    pub url: String,
    /// Exact pull request head SHA.
    pub head_sha: String,
    /// Human owners as observed by queue-observe.
    pub owners: Vec<String>,
    /// Explicit Shipyard blockers.
    pub blockers: Vec<String>,
    /// GitHub merge-state classification.
    pub merge_state: String,
    /// Whether native GitHub auto-merge is armed.
    pub auto_merge: bool,
    /// Actionable digest bucket.
    pub bucket: String,
}

/// Deterministically ordered actionable buckets.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DigestBuckets {
    /// Pull requests currently in GitHub's merge queue.
    pub queued: Vec<DigestPullRequest>,
    /// Auto-merge armed but not currently in the merge queue.
    pub armed: Vec<DigestPullRequest>,
    /// Clean, unarmed pull requests with no observed failed check.
    pub green_unarmed: Vec<DigestPullRequest>,
    /// Unarmed pull requests with a failed required or observed check.
    pub red: Vec<DigestPullRequest>,
    /// Pull requests with a dirty merge state.
    pub dirty: Vec<DigestPullRequest>,
    /// Pull requests behind the observed base branch.
    pub behind: Vec<DigestPullRequest>,
    /// Pull requests explicitly blocked by merge state or owner labels.
    pub blocked: Vec<DigestPullRequest>,
    /// Known but unsettled merge states (for example `unstable`).
    pub unstable: Vec<DigestPullRequest>,
    /// Unrecognized merge states that require operator interpretation.
    pub unknown: Vec<DigestPullRequest>,
}

impl DigestBuckets {
    fn push(&mut self, item: DigestPullRequest) {
        match item.bucket.as_str() {
            "queued" => self.queued.push(item),
            "armed" => self.armed.push(item),
            "green_unarmed" => self.green_unarmed.push(item),
            "red" => self.red.push(item),
            "dirty" => self.dirty.push(item),
            "behind" => self.behind.push(item),
            "blocked" => self.blocked.push(item),
            "unstable" => self.unstable.push(item),
            "unknown" => self.unknown.push(item),
            _ => unreachable!("unknown queue digest bucket"),
        }
    }

    fn sort(&mut self) {
        for bucket in [
            &mut self.queued,
            &mut self.armed,
            &mut self.green_unarmed,
            &mut self.red,
            &mut self.dirty,
            &mut self.behind,
            &mut self.blocked,
            &mut self.unstable,
            &mut self.unknown,
        ] {
            bucket.sort_by(|left, right| {
                (&left.repo, &left.base, left.number, &left.head_sha).cmp(&(
                    &right.repo,
                    &right.base,
                    right.number,
                    &right.head_sha,
                ))
            });
        }
    }
}

/// Complete queue census. `complete == false` is a deliberate fail-closed
/// result; callers must inspect `errors` instead of treating it as all-clear.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QueueDigest {
    /// Stable digest schema version.
    pub schema_version: u32,
    /// False whenever any observer or census safety condition is unresolved.
    pub complete: bool,
    /// Per-observer identity and freshness receipts.
    pub observers: Vec<ObserverDigest>,
    /// Pull requests grouped by actionable state.
    pub buckets: DigestBuckets,
    /// Fail-closed diagnostics. Sorted for deterministic output.
    pub errors: Vec<String>,
}

impl QueueDigest {
    fn empty(error: String) -> Self {
        Self {
            schema_version: QUEUE_DIGEST_SCHEMA_VERSION,
            complete: false,
            observers: Vec::new(),
            buckets: DigestBuckets::default(),
            errors: vec![error],
        }
    }
}

/// Read the observer directory using the current wall clock.
#[must_use]
pub fn read_digest(state_root: &Path, stale_after_seconds: u64) -> QueueDigest {
    read_digest_at(state_root, stale_after_seconds, SystemTime::now())
}

/// Deterministic form of [`read_digest`] for tests and offline audits.
#[must_use]
pub fn read_digest_at(state_root: &Path, stale_after_seconds: u64, now: SystemTime) -> QueueDigest {
    let observer_root = state_root.join("queue-observer");
    let mut paths = match fs::read_dir(&observer_root) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>(),
        Err(error) => {
            return QueueDigest::empty(format!(
                "cannot read queue-observer state directory {}: {error}",
                observer_root.display()
            ));
        }
    };
    paths.sort();
    if paths.is_empty() {
        return QueueDigest::empty(format!(
            "no queue-observer state files found under {}",
            observer_root.display()
        ));
    }
    let mut digest = complete_digest();
    let mut observer_keys = BTreeMap::<(String, String), PathBuf>::new();
    let mut pull_request_keys = BTreeMap::<(String, String, u64), PathBuf>::new();
    let mut queued_keys = BTreeSet::<(String, String, u64)>::new();
    for path in paths {
        let (observer_state, observer, mut observer_errors) =
            match read_observer(&path, stale_after_seconds, now) {
                Ok(value) => value,
                Err(error) => {
                    digest.errors.push(error);
                    continue;
                }
            };
        let snapshot = &observer_state.snapshot;
        let observer_key = (snapshot.repo.clone(), snapshot.base.clone());
        if let Some(previous) = observer_keys.insert(observer_key.clone(), path.clone()) {
            digest.errors.push(format!(
                "duplicate queue-observer census for {}/{}: {} and {}",
                observer_key.0,
                observer_key.1,
                previous.display(),
                path.display()
            ));
        }
        digest.errors.append(&mut observer_errors);
        digest.observers.push(observer);
        for entry in &snapshot.queue {
            let key = (snapshot.repo.clone(), snapshot.base.clone(), entry.pr);
            if !queued_keys.insert(key.clone()) {
                digest.errors.push(format!(
                    "duplicate merge-queue entry for {}/{} PR #{}",
                    key.0, key.1, key.2
                ));
            }
        }
        for pr in &snapshot.pull_requests {
            let key = (snapshot.repo.clone(), snapshot.base.clone(), pr.number);
            if let Some(previous) = pull_request_keys.insert(key.clone(), path.clone()) {
                digest.errors.push(format!(
                    "duplicate pull-request census for {}/{} PR #{}: {} and {}",
                    key.0,
                    key.1,
                    key.2,
                    previous.display(),
                    path.display()
                ));
            }
            let queued = queued_keys.contains(&key);
            let bucket = classify_pull_request(pr, queued);
            digest.buckets.push(DigestPullRequest {
                repo: snapshot.repo.clone(),
                base: snapshot.base.clone(),
                number: pr.number,
                url: pr.url.clone(),
                head_sha: pr.head_sha.clone(),
                owners: pr.owners.clone(),
                blockers: pr.blockers.clone(),
                merge_state: pr.merge_state.clone(),
                auto_merge: pr.auto_merge,
                bucket: bucket.to_owned(),
            });
        }
    }
    for (repo, base, number) in &queued_keys {
        if !pull_request_keys.contains_key(&(repo.clone(), base.clone(), *number)) {
            digest.errors.push(format!(
                "merge-queue entry for {repo}/{base} PR #{number} has no matching pull-request census row"
            ));
        }
    }
    sort_observers(&mut digest.observers);
    digest.buckets.sort();
    digest.errors.sort();
    digest.errors.dedup();
    digest.complete = digest.errors.is_empty() && !digest.observers.is_empty();
    digest
}

fn sort_observers(observers: &mut [ObserverDigest]) {
    observers.sort_unstable_by(|left, right| {
        left.repo
            .cmp(&right.repo)
            .then_with(|| left.base.cmp(&right.base))
            .then_with(|| left.state_file.cmp(&right.state_file))
    });
}

fn read_observer(
    path: &Path,
    stale_after_seconds: u64,
    now: SystemTime,
) -> Result<(ObserverState, ObserverDigest, Vec<String>), String> {
    let metadata = fs::metadata(path).map_err(|error| {
        format!(
            "cannot stat queue-observer state {}: {error}",
            path.display()
        )
    })?;
    let modified = metadata.modified().map_err(|error| {
        format!(
            "cannot read modification time for queue-observer state {}: {error}",
            path.display()
        )
    })?;
    let mut errors = Vec::new();
    let (age_seconds, is_stale) = if let Ok(age) = now.duration_since(modified) {
        let seconds = age.as_secs();
        (seconds, seconds > stale_after_seconds)
    } else {
        errors.push(format!(
            "queue-observer state {} has a future modification time",
            path.display()
        ));
        (0, true)
    };
    let Some(observer_state) = load_state(path)? else {
        return Err(format!(
            "queue-observer state {} disappeared during digest",
            path.display()
        ));
    };
    let snapshot = &observer_state.snapshot;
    if is_stale {
        errors.push(format!(
            "queue-observer state {} is stale: age={}s threshold={}s",
            path.display(),
            age_seconds,
            stale_after_seconds
        ));
    }
    if snapshot.truncated {
        errors.push(format!(
            "queue-observer state {} is truncated and cannot prove a complete census",
            path.display()
        ));
    }
    if let Some(blocker) = snapshot.ownership.blocker.as_deref() {
        errors.push(format!(
            "queue-observer state {} has an ownership blocker: {blocker}",
            path.display()
        ));
    }
    let observer = ObserverDigest {
        state_file: path.display().to_string(),
        repo: snapshot.repo.clone(),
        base: snapshot.base.clone(),
        main_sha: snapshot.main_sha.clone(),
        state_hash: observer_state.state_hash.clone(),
        modified_at: DateTime::<Utc>::from(modified).to_rfc3339_opts(SecondsFormat::Secs, true),
        age_seconds,
        stale: is_stale,
        truncated: snapshot.truncated,
        pull_request_count: snapshot.pull_requests.len(),
        queue_count: snapshot.queue.len(),
    };
    Ok((observer_state, observer, errors))
}

fn classify_pull_request(pr: &PullRequestSnapshot, queued: bool) -> &'static str {
    if queued {
        return "queued";
    }
    if pr.auto_merge {
        return "armed";
    }
    if has_failed_check(&pr.checks) {
        return "red";
    }
    if !pr.blockers.is_empty() || pr.merge_state == "blocked" {
        return "blocked";
    }
    match pr.merge_state.as_str() {
        "clean" => "green_unarmed",
        "dirty" => "dirty",
        "behind" => "behind",
        "unstable" => "unstable",
        _ => "unknown",
    }
}

fn complete_digest() -> QueueDigest {
    QueueDigest {
        schema_version: QUEUE_DIGEST_SCHEMA_VERSION,
        complete: true,
        observers: Vec::new(),
        buckets: DigestBuckets::default(),
        errors: Vec::new(),
    }
}

fn has_failed_check(checks: &[CheckSnapshot]) -> bool {
    checks.iter().any(|check| {
        check
            .conclusion
            .as_deref()
            .or(Some(check.status.as_str()))
            .is_some_and(|value| {
                matches!(
                    value,
                    "failure"
                        | "failed"
                        | "error"
                        | "cancelled"
                        | "timed_out"
                        | "startup_failure"
                        | "action_required"
                        | "stale"
                )
            })
    })
}

/// Render a digest as deterministic operator-facing Markdown.
#[must_use]
pub fn render_markdown(digest: &QueueDigest) -> String {
    let mut lines = vec![
        format!(
            "# Queue digest ({})",
            if digest.complete {
                "complete"
            } else {
                "INCOMPLETE"
            }
        ),
        format!("- observers: {}", digest.observers.len()),
    ];
    for observer in &digest.observers {
        lines.push(format!(
            "- `{}` `{}` base `{}`; state `{}`; age={}s; stale={}; truncated={}; file `{}`",
            observer.repo,
            observer.base,
            observer.main_sha,
            observer.state_hash,
            observer.age_seconds,
            observer.stale,
            observer.truncated,
            observer.state_file
        ));
    }
    let buckets = [
        ("queued", &digest.buckets.queued),
        ("armed", &digest.buckets.armed),
        ("green_unarmed", &digest.buckets.green_unarmed),
        ("red", &digest.buckets.red),
        ("dirty", &digest.buckets.dirty),
        ("behind", &digest.buckets.behind),
        ("blocked", &digest.buckets.blocked),
        ("unstable", &digest.buckets.unstable),
        ("unknown", &digest.buckets.unknown),
    ];
    for (name, pull_requests) in buckets {
        lines.push(format!("## {name} ({})", pull_requests.len()));
        for pr in pull_requests {
            let owners = if pr.owners.is_empty() {
                "unowned".to_owned()
            } else {
                pr.owners.join(",")
            };
            lines.push(format!(
                "- [{}/{} PR #{}]({}): `{}` `{}`; head `{}`; owners={}; auto_merge={}",
                pr.repo,
                pr.base,
                pr.number,
                pr.url,
                pr.merge_state,
                pr.bucket,
                pr.head_sha,
                owners,
                pr.auto_merge
            ));
        }
    }
    if digest.errors.is_empty() {
        lines.push("- errors: none".to_owned());
    } else {
        lines.push("## Errors".to_owned());
        lines.extend(
            digest
                .errors
                .iter()
                .map(|error| format!("- **FAIL-CLOSED:** {error}")),
        );
    }
    let mut output = lines.join("\n");
    output.push('\n');
    output
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::queue_observer::{
        ObserverState, OwnershipSnapshot, QueueEntrySnapshot, QueueStateSnapshot, observe,
        save_state,
    };

    fn state(repo: &str, base: &str, number: u64, bucket: &str) -> ObserverState {
        let (merge_state, auto_merge, checks, blockers) = match bucket {
            "green_unarmed" => ("clean", false, vec![], vec![]),
            "red" => (
                "clean",
                false,
                vec![CheckSnapshot {
                    name: "macos".to_owned(),
                    status: "completed".to_owned(),
                    conclusion: Some("failure".to_owned()),
                    required: true,
                    app_id: None,
                    required_app_id: None,
                    url: Some("https://github.test/check/1".to_owned()),
                }],
                vec![],
            ),
            "blocked" => ("blocked", false, vec![], vec!["review".to_owned()]),
            _ => (bucket, false, vec![], vec![]),
        };
        observe(
            None,
            QueueStateSnapshot {
                schema_version: 1,
                repo: repo.to_owned(),
                base: base.to_owned(),
                main_sha: "b".repeat(40),
                main_url: format!("https://github.test/{repo}/commit/base"),
                truncated: false,
                required_contexts: vec![],
                required_checks: vec![],
                ownership: OwnershipSnapshot::default(),
                queue: vec![],
                pull_requests: vec![PullRequestSnapshot {
                    number,
                    url: format!("https://github.test/{repo}/pull/{number}"),
                    head_sha: "h".repeat(40),
                    merge_state: merge_state.to_owned(),
                    auto_merge,
                    owners: vec!["owner".to_owned()],
                    blockers,
                    checks,
                }],
            },
        )
        .expect("state")
        .state
    }

    fn write_state(root: &Path, name: &str, state: &ObserverState) {
        let path = root.join("queue-observer").join(name);
        save_state(&path, state).expect("save state");
    }

    #[test]
    fn complete_digest_preserves_identity_and_buckets() {
        let temp = tempfile::tempdir().expect("temp");
        write_state(
            temp.path(),
            "one.json",
            &state("o/r", "main", 1, "green_unarmed"),
        );
        write_state(temp.path(), "two.json", &state("o/r2", "release", 2, "red"));
        let digest = read_digest_at(temp.path(), 900, SystemTime::now() + Duration::from_secs(1));
        assert!(digest.complete, "{digest:?}");
        assert_eq!(digest.observers.len(), 2);
        assert_eq!(digest.buckets.green_unarmed[0].number, 1);
        assert_eq!(digest.buckets.red[0].url, "https://github.test/o/r2/pull/2");
        assert_eq!(digest.buckets.red[0].head_sha, "h".repeat(40));
        let markdown = render_markdown(&digest);
        assert!(markdown.contains("https://github.test/o/r2/pull/2"));
        assert!(markdown.contains(&"h".repeat(40)));
    }

    #[test]
    fn checked_in_fixture_is_a_valid_complete_observer_state() {
        let temp = tempfile::tempdir().expect("temp");
        let observer_root = temp.path().join("queue-observer");
        fs::create_dir_all(&observer_root).expect("observer root");
        fs::write(
            observer_root.join("fixture.json"),
            include_str!("../tests/fixtures/queue-digest/complete.json"),
        )
        .expect("fixture");
        let digest = read_digest_at(
            temp.path(),
            u64::MAX,
            SystemTime::now() + Duration::from_secs(1),
        );
        assert!(digest.complete, "{digest:?}");
        assert_eq!(digest.observers[0].repo, "fixture/repo");
        assert_eq!(digest.buckets.green_unarmed[0].number, 7);
    }

    #[test]
    fn missing_directory_fails_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let digest = read_digest(temp.path(), DEFAULT_STALE_AFTER_SECONDS);
        assert!(!digest.complete);
        assert!(
            digest
                .errors
                .iter()
                .any(|error| error.contains("queue-observer"))
        );
    }

    #[test]
    fn duplicate_observer_and_pull_request_census_fails_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let value = state("o/r", "main", 1, "green_unarmed");
        write_state(temp.path(), "one.json", &value);
        write_state(temp.path(), "two.json", &value);
        let digest = read_digest_at(temp.path(), 900, SystemTime::now() + Duration::from_secs(1));
        assert!(!digest.complete);
        assert!(
            digest
                .errors
                .iter()
                .any(|error| error.contains("duplicate queue-observer"))
        );
        assert!(
            digest
                .errors
                .iter()
                .any(|error| error.contains("duplicate pull-request"))
        );
    }

    #[test]
    fn orphaned_queue_entry_fails_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let mut value = state("o/r", "main", 1, "green_unarmed");
        value.snapshot.queue.push(QueueEntrySnapshot {
            pr: 99,
            position: 1,
            url: "https://github.test/o/r/pull/99".to_owned(),
            pr_head_sha: "q".repeat(40),
            merge_group_sha: None,
            enqueued_at: "2026-10-08T00:00:00Z".to_owned(),
            checks: vec![],
            receipt_decisions: vec![],
            test_tier: vec![],
        });
        value.state_hash = crate::queue_observer::snapshot_hash(&value.snapshot).expect("hash");
        write_state(temp.path(), "one.json", &value);
        let digest = read_digest_at(temp.path(), 900, SystemTime::now() + Duration::from_secs(1));
        assert!(!digest.complete);
        assert!(
            digest
                .errors
                .iter()
                .any(|error| error.contains("no matching pull-request"))
        );
    }

    #[test]
    fn corrupt_state_fails_closed_without_all_clear() {
        let temp = tempfile::tempdir().expect("temp");
        let observer_root = temp.path().join("queue-observer");
        fs::create_dir_all(&observer_root).expect("observer root");
        fs::write(
            observer_root.join("corrupt.json"),
            b"{\"schema_version\":1,",
        )
        .expect("state");
        let digest = read_digest_at(temp.path(), 900, SystemTime::now());
        assert!(!digest.complete);
        assert!(
            digest
                .errors
                .iter()
                .any(|error| error.contains("parse observer state"))
        );
    }

    #[test]
    fn stale_and_truncated_observer_fails_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let mut value = state("o/r", "main", 1, "green_unarmed");
        value.snapshot.truncated = true;
        value.state_hash = crate::queue_observer::snapshot_hash(&value.snapshot).expect("hash");
        write_state(temp.path(), "one.json", &value);
        let digest = read_digest_at(temp.path(), 0, UNIX_EPOCH + Duration::from_secs(1));
        assert!(!digest.complete);
        assert!(digest.observers[0].stale);
        assert!(
            digest
                .errors
                .iter()
                .any(|error| error.contains("truncated"))
        );
        assert!(digest.errors.iter().any(|error| error.contains("stale")));
    }
}
