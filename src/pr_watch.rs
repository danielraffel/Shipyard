//! PR watch: read-only flags for open pull requests that are stuck in a way a
//! person should look at.
//!
//! One pure engine, two drivers. [`gather`] reads a window of GitHub history
//! (pull requests and their queue timelines, gate-workflow runs, required
//! check runs, failing-test signatures, merge bases) into a [`RepoHistory`].
//! [`evaluate`] turns that history into the [`Flag`]s that hold at one
//! instant. A live scan is `evaluate(history, now)`; [`replay`] calls the same
//! function at every tick of a past window, so replay and live cannot disagree.
//!
//! The flags, each with the evidence line it would post:
//!
//! 1. [`FlagKind::RepeatTestFailure`]: the same required check failed with the
//!    same failing test (or error signature) on at least two runs of the pull
//!    request. The verdict is "code failure, not flake" unless the same
//!    signature is also failing on at least two *other* pull requests within
//!    24 hours, in which case it is "failing on main/pre-existing".
//! 2. [`FlagKind::RedWhileArmed`]: auto-merge is armed (or the pull request was
//!    ejected for `failed_checks` and not re-armed), it is not in the queue,
//!    and a required check on the current head has been red for longer than
//!    the threshold with no push since.
//! 3. [`FlagKind::RepeatedEjection`]: at least two merge groups named for the
//!    pull request failed a required job. GitHub names a group after its last
//!    entry, so this is "named for", not "proved culprit"; the parent group's
//!    status is shown as evidence.
//! 4. [`FlagKind::RebaseTreadmill`]: the head was replaced at least three
//!    times within 24 hours, each time cancelling the previous head's gate run
//!    while the merge base with the base branch advanced. "Main moved" is an
//!    inference from the merge base and the evidence line says so.
//! 5. [`FlagKind::SplitCandidate`]: open longer than three days, or more than
//!    60 files or 30 commits. Advisory only: it is raised only alongside
//!    another flag on the same pull request, never alone.
//!
//! Everything here is read-only on GitHub except two opt-in writes: the
//! sticky pull-request comment in [`comment`] (comment endpoints only) and the
//! [`handback`]'s `shipyard:needs-agent` label add/remove (label endpoints
//! only), which also notifies a live owning session without typing into it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

pub mod comment;
pub mod digest;
pub mod fixtures;
pub mod flags;
pub mod gather;
pub mod handback;
pub mod ledger;
pub mod replay;
pub mod scan;
pub mod wakes;

pub use flags::{
    DigestRoute, Flag, FlagKind, RepeatFinding, Thresholds, evaluate, repeat_findings,
};
pub use gather::{WatchQuery, gather};
pub use ledger::{Ledger, LedgerEntry};
pub use replay::{Expectation, ReplayReport, replay};

/// Marker that identifies the sticky comment this tool owns.
pub const COMMENT_MARKER: &str = "<!-- shipyard-pr-watch v1 -->";
/// Label a person adds to acknowledge a pull request's flags. Read only; the
/// tool never creates or removes it.
pub const ACK_LABEL: &str = "shipyard:ack/pr-watch";

/// Everything [`evaluate`] reads, for one repository and one window.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoHistory {
    /// `OWNER/REPO`.
    pub repo: String,
    /// Protected branch the merge queue merges into.
    pub base: String,
    /// Window start, inclusive.
    pub from: DateTime<Utc>,
    /// Window end, exclusive.
    pub to: DateTime<Utc>,
    /// Required check names (branch protection or config). Only these ever
    /// raise a flag; advisory jobs are invisible to every rule.
    pub required_checks: Vec<String>,
    /// Pull requests updated in the window, by number.
    pub prs: BTreeMap<u64, PrHistory>,
    /// Merge-group runs of the gate workflow in the window.
    pub group_runs: Vec<GroupRun>,
    /// Signals that could not be read completely. Never treated as absence.
    pub gaps: Vec<String>,
}

/// One pull request's facts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrHistory {
    /// Pull request number.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Web URL.
    pub url: String,
    /// Creation time.
    pub created_at: Option<DateTime<Utc>>,
    /// Merge time.
    pub merged_at: Option<DateTime<Utc>>,
    /// Close time (also set for merged pull requests).
    pub closed_at: Option<DateTime<Utc>>,
    /// Changed files (current).
    pub changed_files: u64,
    /// Commit count (current).
    pub commits: u64,
    /// Head branch name.
    pub head_ref: String,
    /// Current head SHA.
    pub head_sha: String,
    /// Labels (current).
    pub labels: Vec<String>,
    /// Heads the pull request had, ordered by when they were first seen.
    pub heads: Vec<HeadFact>,
    /// Queue timeline, ordered by time.
    pub events: Vec<QueueEvent>,
    /// `false` when GitHub reported earlier timeline items than were read.
    pub timeline_complete: bool,
}

/// One head the pull request had.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadFact {
    /// Commit SHA.
    pub sha: String,
    /// When it was pushed: the earliest of its first gate-workflow run and any
    /// force-push event naming it. Commit dates are author-controlled and are
    /// never used.
    pub first_seen_at: DateTime<Utc>,
    /// Gate-workflow `pull_request` runs on this head.
    pub gate_runs: Vec<RunFact>,
    /// Required check runs on this head (every attempt).
    pub checks: Vec<CheckFact>,
    /// Merge base with the base branch, when it was read.
    pub merge_base: Option<String>,
}

/// One workflow run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFact {
    /// Run id.
    pub id: u64,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// `status`.
    pub status: String,
    /// `conclusion`, when completed.
    pub conclusion: Option<String>,
}

/// One required check run (or required job of a merge-group run).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckFact {
    /// Check (job) name.
    pub name: String,
    /// Check run id, which is the Actions job id.
    pub id: u64,
    /// `status`.
    pub status: String,
    /// `conclusion`, when completed.
    pub conclusion: Option<String>,
    /// Start time.
    pub started_at: Option<DateTime<Utc>>,
    /// Completion time.
    pub completed_at: Option<DateTime<Utc>>,
    /// Failing tests (or the first error line) read from the job log, for a
    /// failed check. Empty when the log was unreadable or held no signature.
    pub signatures: Vec<String>,
}

impl CheckFact {
    /// Whether this attempt concluded red.
    #[must_use]
    pub fn failed(&self) -> bool {
        matches!(
            self.conclusion.as_deref(),
            Some("failure" | "timed_out" | "startup_failure")
        )
    }

    /// Whether this attempt concluded green.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.conclusion.as_deref() == Some("success")
    }
}

/// One merge-group run of the gate workflow.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRun {
    /// Run id.
    pub id: u64,
    /// Pull request the group is named for (its last entry).
    pub pr: Option<u64>,
    /// Merge-group commit.
    pub head_sha: String,
    /// The commit the group was built on (the ref suffix): the previous
    /// group's commit, or a base-branch commit.
    pub parent_sha: Option<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Run conclusion.
    pub conclusion: Option<String>,
    /// Required jobs of the run. Read only for runs that did not succeed.
    pub required_jobs: Vec<CheckFact>,
    /// The repository's batch attributor's ruling on this failed group, when
    /// one was configured and asked (live scans only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<Attribution>,
}

/// A `[queue.attribution] command` verdict for one failed merge group, as
/// Shipyard's queue-arm guard reads it: `implicates_head` exactly `false`
/// with a verdict that positively names why clears the named pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    /// `implicates_head`, `other_pull_request`, `infrastructure`, `unexplained`.
    pub verdict: String,
    /// `Some(false)` only on positive evidence the head is not the cause.
    pub implicates_head: Option<bool>,
    /// The pull request the attributor blamed instead, if any.
    pub implicated_pr: Option<u64>,
}

impl Attribution {
    /// Whether this clears the named pull request (`pr`).
    #[must_use]
    pub fn clears(&self, pr: u64) -> bool {
        self.implicates_head == Some(false)
            && matches!(
                self.verdict.as_str(),
                "other_pull_request" | "infrastructure"
            )
            && self.implicated_pr != Some(pr)
    }

    /// Parse the attributor's stdout.
    #[must_use]
    pub fn parse(stdout: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
        Some(Self {
            verdict: value.get("verdict")?.as_str()?.to_owned(),
            implicates_head: value
                .get("implicates_head")
                .and_then(serde_json::Value::as_bool),
            implicated_pr: value
                .get("implicated_pr")
                .and_then(serde_json::Value::as_u64),
        })
    }
}

impl GroupRun {
    /// Whether a required job failed.
    #[must_use]
    pub fn gate_failed(&self) -> bool {
        self.required_jobs.iter().any(CheckFact::failed)
    }

    /// Whether the run succeeded (every required job passed).
    #[must_use]
    pub fn gate_passed(&self) -> bool {
        self.conclusion.as_deref() == Some("success")
    }

    /// When the failing required job finished, else the run's creation.
    #[must_use]
    pub fn settled_at(&self) -> DateTime<Utc> {
        self.required_jobs
            .iter()
            .filter_map(|job| job.completed_at)
            .max()
            .unwrap_or(self.created_at)
    }
}

/// One queue-timeline event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEvent {
    /// When.
    pub at: DateTime<Utc>,
    /// What.
    pub kind: QueueEventKind,
}

/// Queue-timeline event kinds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueueEventKind {
    /// `AutoMergeEnabledEvent`.
    Armed,
    /// `AutoMergeDisabledEvent`.
    Disarmed {
        /// Reason GitHub gave.
        reason: Option<String>,
    },
    /// `AddedToMergeQueueEvent`.
    Enqueued,
    /// `RemovedFromMergeQueueEvent`.
    Removed {
        /// Reason GitHub gave (`failed_checks`, `manual`, `merged`, ...).
        reason: String,
    },
    /// `HeadRefForcePushedEvent`.
    ForcePushed {
        /// The new head.
        after: Option<String>,
    },
    /// `MergedEvent`.
    Merged,
    /// `ClosedEvent`.
    Closed,
    /// `ReopenedEvent`.
    Reopened,
}

/// Whether a pull request is open at `at`.
#[must_use]
pub fn open_at(pr: &PrHistory, at: DateTime<Utc>) -> bool {
    pr.created_at.is_none_or(|created| created <= at)
        && pr.merged_at.is_none_or(|merged| merged > at)
        && pr.closed_at.is_none_or(|closed| closed > at)
}

/// The head current at `at`.
#[must_use]
pub fn head_at(pr: &PrHistory, at: DateTime<Utc>) -> Option<&HeadFact> {
    pr.heads.iter().rev().find(|head| head.first_seen_at <= at)
}

/// A failed required job attributed to one pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureRecord<'a> {
    /// Pull request.
    pub pr: u64,
    /// Where it ran: a head SHA, or `merge group <run id>`.
    pub lane: String,
    /// The failing job.
    pub check: &'a CheckFact,
}

/// Every required-job outcome (pass or fail) attributed to a pull request:
/// only checks named in [`RepoHistory::required_checks`], so an advisory job
/// can never contribute,
/// its heads' required checks and the required jobs of merge groups named
/// for it. Settled outcomes only.
#[must_use]
pub fn outcomes_for(history: &RepoHistory, pr: u64) -> Vec<FailureRecord<'_>> {
    let required = |check: &CheckFact| history.required_checks.contains(&check.name);
    let mut out = Vec::new();
    if let Some(entry) = history.prs.get(&pr) {
        for head in &entry.heads {
            for check in head.checks.iter().filter(|check| required(check)) {
                out.push(FailureRecord {
                    pr,
                    lane: short(&head.sha).to_owned(),
                    check,
                });
            }
        }
    }
    for run in history.group_runs.iter().filter(|run| run.pr == Some(pr)) {
        for check in run.required_jobs.iter().filter(|check| required(check)) {
            out.push(FailureRecord {
                pr,
                lane: format!("merge group {}", run.id),
                check,
            });
        }
    }
    out
}

/// Failed required jobs of every pull request in the history (heads and
/// named merge groups), keyed by pull request.
#[must_use]
pub fn failures_by_pr(history: &RepoHistory) -> BTreeMap<u64, Vec<FailureRecord<'_>>> {
    let mut numbers: BTreeSet<u64> = history.prs.keys().copied().collect();
    numbers.extend(history.group_runs.iter().filter_map(|run| run.pr));
    numbers
        .into_iter()
        .map(|pr| {
            let failures: Vec<FailureRecord<'_>> = outcomes_for(history, pr)
                .into_iter()
                .filter(|record| record.check.failed())
                .collect();
            (pr, failures)
        })
        .filter(|(_, failures)| !failures.is_empty())
        .collect()
}

/// First eight characters of a SHA.
#[must_use]
pub fn short(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

/// `<id> - <name> (Failed)  <labels>` -> `<name>`. `CTest` renumbers tests
/// between runs, so only the bare name identifies a test across runs.
#[must_use]
pub fn normalize_ctest_name(line: &str) -> String {
    let trimmed = line.trim();
    let without_id = match trimmed.split_once(" - ") {
        Some((id, rest)) if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) => rest,
        _ => trimmed,
    };
    let name = without_id
        .find(" (")
        .map_or(without_id, |index| &without_id[..index]);
    name.trim().to_owned()
}

/// Failure signatures from a job log: the normalised names of the failing
/// `CTest` tests when the log has a `CTest` summary, else the first meaningful
/// `##[error]` line (a bare "Process completed with exit code N" is not
/// meaningful: it names every failure alike).
#[must_use]
pub fn failure_signatures(log: &str) -> Vec<String> {
    let tail = crate::diagnostics::log_tail_clean(log, LOG_TAIL_BYTES);
    let parser = crate::diagnostics::select_parser(Some("ctest"));
    let tests: BTreeSet<String> = parser
        .parse(&tail)
        .iter()
        .map(|line| normalize_ctest_name(line))
        .filter(|name| !name.is_empty())
        .collect();
    if !tests.is_empty() {
        return tests.into_iter().collect();
    }
    for line in tail.lines() {
        let payload = strip_log_timestamp(line);
        let Some(message) = payload.trim().strip_prefix("##[error]") else {
            continue;
        };
        let message = message.trim();
        if message.is_empty() || message.starts_with("Process completed with exit code") {
            continue;
        }
        let mut signature: String = message.chars().take(160).collect();
        if signature.len() < message.len() {
            signature.push('…');
        }
        return vec![format!("error: {signature}")];
    }
    Vec::new()
}

/// Bytes of a job log's tail read for signatures. Pulp's `macos` logs run to
/// about 2 MB with the `CTest` summary a few hundred KB from the end.
pub const LOG_TAIL_BYTES: usize = 1_048_576;

fn strip_log_timestamp(line: &str) -> &str {
    // "2026-09-29T03:26:54.2414940Z payload"
    match line.split_once(' ') {
        Some((stamp, rest)) if stamp.len() >= 20 && stamp.ends_with('Z') && stamp.contains('T') => {
            rest
        }
        _ => line,
    }
}

/// Parse `48h`, `7d`, `15m`.
///
/// # Errors
/// When the text is not a positive count with a `d`, `h`, or `m` unit.
pub fn parse_duration(value: &str) -> Result<Duration, String> {
    crate::gate_cost::parse_window(value)
}

#[cfg(test)]
mod tests;
