//! The landing verdict: one line an agent can quote about one pull request.
//!
//! Agents that summarise a pull request's state have repeatedly called a red
//! required check "a flake", "infrastructure" or "still in progress" while an
//! identical failure sat on an earlier head of the same pull request. Every
//! such claim was checkable, and none was checked, because the facts that
//! settle it (which checks branch protection requires, what the current head's
//! required checks concluded, which tests they failed, and whether the same
//! test failed before) live on four different GitHub surfaces.
//!
//! This module folds those facts into one [`LandingVerdict`]:
//!
//! ```text
//! VERDICT #8933 head fc399ea6: RED — macos failed cmake-forge-catalog-install (REPEAT on 2 heads: cc6302b9, fc399ea6); other required: 4 green; queue: ejected failed_checks at 2026-09-29T03:40:00Z, same head
//! ```
//!
//! * `RED`: at least one required check's latest attempt on the current head
//!   concluded red. Advisory checks never make a verdict red.
//! * `PENDING`: nothing required is red, and at least one required check is
//!   queued, running, or not created on the current head yet.
//! * `GREEN`: every required check passed on the current head.
//! * `UNKNOWN`: the required checks or the head's check runs could not be
//!   read. Never collapsed into green or pending.
//!
//! `REPEAT` reuses PR watch's flag-1 rule ([`crate::pr_watch::repeat_findings`])
//! so the two surfaces cannot disagree about what a repeat is: the same
//! normalised failing test on at least two runs (heads, re-runs, or merge
//! groups named for the pull request) of the same required check. When the
//! same test is also failing on at least two *other* pull requests within the
//! window, the line says `also failing on #a, #b — likely main/shared` instead
//! of pinning it on this pull request.
//!
//! Everything here is pure; [`super::verdict_gather`] reads the facts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::pr_queue_state::PrQueueState;
use crate::pr_watch::{
    CheckFact, GroupRun, RepeatFinding, RepoHistory, Thresholds, failures_by_pr, repeat_findings,
    short,
};

/// Tests named per failing check before "and N more".
const TESTS_SHOWN: usize = 3;

/// The headline state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictState {
    /// A required check is red on the current head.
    Red,
    /// Nothing required is red; something required has not finished.
    Pending,
    /// Every required check passed on the current head.
    Green,
    /// The facts could not be read.
    Unknown,
}

impl VerdictState {
    /// Upper-case label used in the line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Red => "RED",
            Self::Pending => "PENDING",
            Self::Green => "GREEN",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// One required context's state on the current head.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredState {
    /// Concluded `success`, `neutral`, or `skipped` (a commit status: `success`).
    Green,
    /// Concluded red (`failure`, `timed_out`, `startup_failure`, `cancelled`,
    /// `action_required`; a commit status: `failure`/`error`).
    Red,
    /// Created and not finished.
    Running,
    /// No check run or commit status with this name on the head.
    NotCreated,
}

/// Where a required context's state was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredSource {
    /// A check run (for Actions, the job).
    CheckRun,
    /// A commit status.
    Status,
    /// Neither exists.
    None,
}

/// A repeated failing test on one required check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepeatEvidence {
    /// Failing tests on the current head that also failed on earlier runs.
    pub tests: Vec<String>,
    /// Where they failed: short head SHAs or `merge group <run id>`.
    pub lanes: Vec<String>,
    /// Distinct failing runs, summed over the tests' largest count.
    pub runs: usize,
}

/// A failing test that is also failing on other pull requests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SharedEvidence {
    /// Failing tests on the current head that other pull requests also fail.
    pub tests: Vec<String>,
    /// Those other pull requests.
    pub other_prs: Vec<u64>,
}

/// One required context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RequiredCheckVerdict {
    /// Context name as branch protection spells it.
    pub name: String,
    /// State on the current head.
    pub state: RequiredState,
    /// Where it was read.
    pub source: RequiredSource,
    /// Raw `status` (check run) or `state` (commit status).
    pub status: Option<String>,
    /// Raw `conclusion`, when completed.
    pub conclusion: Option<String>,
    /// Check run (Actions job) id of the latest attempt.
    pub check_run_id: Option<u64>,
    /// Normalised failing tests (or an `error:` signature) from the job log.
    pub failing_tests: Vec<String>,
    /// Whether the failing job's log was read. `false` with an empty
    /// `failing_tests` means "not known", not "no test failed".
    pub log_read: bool,
    /// Set when a failing test repeated on this pull request.
    pub repeat: Option<RepeatEvidence>,
    /// Set when a failing test is also failing on other pull requests.
    pub shared: Option<SharedEvidence>,
}

/// The latest merge group named for the pull request on its current head,
/// when it failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GroupVerdict {
    /// Gate-workflow run id of the group.
    pub run_id: u64,
    /// When the group's run was created.
    pub created_at: DateTime<Utc>,
    /// Its failed required jobs. Empty when the jobs could not be read.
    pub failed: Vec<RequiredCheckVerdict>,
}

/// The verdict for one pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LandingVerdict {
    /// Headline.
    pub state: VerdictState,
    /// The one line to quote.
    pub line: String,
    /// Pull request.
    pub pr: u64,
    /// Current head, when read.
    pub head_sha: Option<String>,
    /// Base branch whose protection supplied the required contexts.
    pub base: Option<String>,
    /// Every required context, in branch protection's order.
    pub required: Vec<RequiredCheckVerdict>,
    /// Set when the latest merge group named for this pull request on its
    /// current head failed: the head can be green on a fast tier while the
    /// group that ran the full suite is red, and that pull request cannot land
    /// as it stands.
    pub merge_group: Option<GroupVerdict>,
    /// Queue state suffix, as printed.
    pub queue: Option<String>,
    /// Why the verdict is unknown.
    pub unknown_detail: Option<String>,
    /// Reads that failed or were cut short. Never treated as absence.
    pub gaps: Vec<String>,
    /// GitHub API calls spent on the verdict (cache hits excluded).
    pub api_calls: u32,
}

/// Everything the verdict is computed from.
#[derive(Clone, Debug, Default)]
pub struct VerdictFacts {
    /// Pull request.
    pub pr: u64,
    /// Current head SHA.
    pub head_sha: String,
    /// Base branch.
    pub base: String,
    /// Required contexts from branch protection.
    pub required: Vec<String>,
    /// This pull request (its heads, with every required check attempt), the
    /// merge groups named for it, and other pull requests' failures read for
    /// the shared-failure test. `required_checks` must equal `required`.
    pub history: RepoHistory,
    /// Commit statuses on the current head: context to state.
    pub statuses: BTreeMap<String, String>,
    /// Failed check-run ids whose log could not be read.
    pub unreadable_logs: BTreeSet<u64>,
    /// Failed merge-group runs whose jobs could not be read. Such a run
    /// counts as a failed group: its failure is known, only its cause is not.
    pub unread_group_runs: BTreeSet<u64>,
    /// When the verdict is taken.
    pub now: DateTime<Utc>,
    /// Gaps from the read.
    pub gaps: Vec<String>,
    /// API calls spent.
    pub api_calls: u32,
}

/// The queue-state suffix for a verdict line.
#[must_use]
pub fn queue_suffix(state: &PrQueueState) -> String {
    match state {
        PrQueueState::Merged => "merged".to_owned(),
        PrQueueState::Closed => "closed".to_owned(),
        PrQueueState::Queued { position, .. } => position.map_or_else(
            || "queued".to_owned(),
            |position| format!("queued pos {position}"),
        ),
        PrQueueState::ArmedNotQueued { .. } => "armed".to_owned(),
        PrQueueState::NeverArmed => "not armed".to_owned(),
        PrQueueState::Ejected {
            reason,
            at,
            new_head_since_removal,
            ..
        } => format!(
            "ejected {reason} at {}, {}",
            at.as_deref().unwrap_or("UNKNOWN"),
            if *new_head_since_removal {
                "new head since"
            } else {
                "same head"
            }
        ),
        PrQueueState::Unknown { .. } => "UNKNOWN".to_owned(),
    }
}

/// A verdict that could not be computed.
#[must_use]
pub fn unknown(
    pr: u64,
    head_sha: Option<String>,
    detail: &str,
    queue: Option<String>,
    gaps: Vec<String>,
    api_calls: u32,
) -> LandingVerdict {
    let mut line = format!(
        "VERDICT #{pr}{}: UNKNOWN — {detail}; do not report this PR as green, red, or flaky",
        head_sha
            .as_deref()
            .map_or_else(String::new, |sha| format!(" head {}", short(sha)))
    );
    if let Some(queue) = &queue {
        let _ = write!(line, "; queue: {queue}");
    }
    LandingVerdict {
        state: VerdictState::Unknown,
        line,
        pr,
        head_sha,
        base: None,
        required: Vec::new(),
        merge_group: None,
        queue,
        unknown_detail: Some(detail.to_owned()),
        gaps,
        api_calls,
    }
}

fn check_state(check: &CheckFact) -> RequiredState {
    if check.status != "completed" {
        return RequiredState::Running;
    }
    match check.conclusion.as_deref() {
        Some("success" | "neutral" | "skipped") => RequiredState::Green,
        Some("failure" | "timed_out" | "startup_failure" | "cancelled" | "action_required") => {
            RequiredState::Red
        }
        _ => RequiredState::Running,
    }
}

fn status_state(state: &str) -> RequiredState {
    match state {
        "success" => RequiredState::Green,
        "failure" | "error" => RequiredState::Red,
        _ => RequiredState::Running,
    }
}

/// The latest attempt of `name` among `checks`.
fn latest<'a>(checks: &'a [CheckFact], name: &str) -> Option<&'a CheckFact> {
    checks
        .iter()
        .filter(|check| check.name == name)
        .max_by_key(|check| (check.started_at.or(check.completed_at), check.id))
}

/// Compute the verdict.
#[must_use]
pub fn compute(
    facts: &VerdictFacts,
    queue: Option<&PrQueueState>,
    thresholds: &Thresholds,
) -> LandingVerdict {
    let queue_text = queue.map(queue_suffix);
    let head_checks: &[CheckFact] = facts
        .history
        .prs
        .get(&facts.pr)
        .and_then(|pr| pr.heads.iter().find(|head| head.sha == facts.head_sha))
        .map_or(&[], |head| head.checks.as_slice());

    let failures = failures_by_pr(&facts.history);
    let findings = repeat_findings(&facts.history, &failures, facts.pr, facts.now, thresholds);
    let shared_for = |name: &str, signature: &str| -> BTreeSet<u64> {
        crate::pr_watch::flags::prs_failing_signature(
            &failures, facts.pr, name, signature, facts.now, thresholds,
        )
    };

    let required: Vec<RequiredCheckVerdict> = facts
        .required
        .iter()
        .map(|name| required_verdict(name, head_checks, facts, &findings, thresholds, &shared_for))
        .collect();

    let failing_runs = latest_group_failure(facts);
    let merge_group = failing_runs.first().map(|group| {
        let failed = failing_runs
            .iter()
            .flat_map(|run| run.required_jobs.iter())
            .filter(|job| job.failed() && facts.required.contains(&job.name))
            .map(|job| {
                let mut verdict = RequiredCheckVerdict {
                    name: job.name.clone(),
                    state: RequiredState::Red,
                    source: RequiredSource::CheckRun,
                    status: Some(job.status.clone()),
                    conclusion: job.conclusion.clone(),
                    check_run_id: Some(job.id),
                    failing_tests: job.signatures.clone(),
                    log_read: !facts.unreadable_logs.contains(&job.id),
                    repeat: None,
                    shared: None,
                };
                attach_history(&mut verdict, &findings, thresholds, &shared_for);
                verdict
            })
            .collect();
        GroupVerdict {
            run_id: group.id,
            created_at: group.created_at,
            failed,
        }
    });

    let any = |state: RequiredState| required.iter().any(|check| check.state == state);
    let state = if any(RequiredState::Red) || merge_group.is_some() {
        VerdictState::Red
    } else if any(RequiredState::Running) || any(RequiredState::NotCreated) {
        VerdictState::Pending
    } else {
        VerdictState::Green
    };
    let line = render_line(
        facts,
        state,
        &required,
        merge_group.as_ref(),
        queue_text.as_deref(),
    );
    LandingVerdict {
        state,
        line,
        pr: facts.pr,
        head_sha: Some(facts.head_sha.clone()),
        base: Some(facts.base.clone()),
        required,
        merge_group,
        queue: queue_text,
        unknown_detail: None,
        gaps: facts.gaps.clone(),
        api_calls: facts.api_calls,
    }
}

/// One required context's verdict on the current head.
fn required_verdict(
    name: &str,
    head_checks: &[CheckFact],
    facts: &VerdictFacts,
    findings: &[RepeatFinding],
    thresholds: &Thresholds,
    shared_for: &dyn Fn(&str, &str) -> BTreeSet<u64>,
) -> RequiredCheckVerdict {
    let blank = |state: RequiredState, source: RequiredSource| RequiredCheckVerdict {
        name: name.to_owned(),
        state,
        source,
        status: None,
        conclusion: None,
        check_run_id: None,
        failing_tests: Vec::new(),
        log_read: false,
        repeat: None,
        shared: None,
    };
    if let Some(check) = latest(head_checks, name) {
        let state = check_state(check);
        let mut verdict = RequiredCheckVerdict {
            status: Some(check.status.clone()),
            conclusion: check.conclusion.clone(),
            check_run_id: Some(check.id),
            ..blank(state, RequiredSource::CheckRun)
        };
        if state == RequiredState::Red {
            verdict.failing_tests.clone_from(&check.signatures);
            verdict.log_read = !facts.unreadable_logs.contains(&check.id);
            attach_history(&mut verdict, findings, thresholds, shared_for);
        }
        verdict
    } else if let Some(state) = facts.statuses.get(name) {
        RequiredCheckVerdict {
            status: Some(state.clone()),
            ..blank(status_state(state), RequiredSource::Status)
        }
    } else {
        blank(RequiredState::NotCreated, RequiredSource::None)
    }
}

/// The failed runs of the latest merge group named for the pull request that
/// was created after its current head was first seen. A group is one merge
/// commit, and every workflow of it is its own run; it failed when any run
/// failed a required job (or failed with jobs unread). A later group that did
/// not fail clears an earlier red one, and a group from before the current
/// head says nothing about it.
fn latest_group_failure(facts: &VerdictFacts) -> Vec<&GroupRun> {
    let Some(since) = facts
        .history
        .prs
        .get(&facts.pr)
        .and_then(|pr| pr.heads.iter().find(|head| head.sha == facts.head_sha))
        .map(|head| head.first_seen_at)
    else {
        return Vec::new();
    };
    let own: Vec<&GroupRun> = facts
        .history
        .group_runs
        .iter()
        .filter(|run| run.pr == Some(facts.pr) && run.created_at >= since)
        .collect();
    let Some(latest) = own.iter().max_by_key(|run| (run.created_at, run.id)) else {
        return Vec::new();
    };
    own.iter()
        .filter(|run| run.head_sha == latest.head_sha)
        .filter(|run| {
            run.required_jobs
                .iter()
                .any(|job| job.failed() && facts.required.contains(&job.name))
                || (run.conclusion.as_deref() == Some("failure")
                    && facts.unread_group_runs.contains(&run.id))
        })
        .copied()
        .collect()
}

/// Attach repeat and shared evidence for a red check's current failing tests.
fn attach_history(
    verdict: &mut RequiredCheckVerdict,
    findings: &[RepeatFinding],
    thresholds: &Thresholds,
    shared_for: &dyn Fn(&str, &str) -> BTreeSet<u64>,
) {
    let mut shared_tests = Vec::new();
    let mut shared_prs: BTreeSet<u64> = BTreeSet::new();
    let mut repeat_tests = Vec::new();
    let mut lanes: Vec<String> = Vec::new();
    let mut runs = 0usize;
    for test in &verdict.failing_tests {
        let others = shared_for(&verdict.name, test);
        if others.len() >= thresholds.pre_existing_other_prs {
            shared_tests.push(test.clone());
            shared_prs.extend(others);
            continue;
        }
        if let Some(finding) = findings
            .iter()
            .find(|finding| finding.check == verdict.name && finding.signature == *test)
        {
            repeat_tests.push(test.clone());
            runs = runs.max(finding.runs);
            for lane in &finding.lanes {
                if !lanes.contains(lane) {
                    lanes.push(lane.clone());
                }
            }
        }
    }
    if !repeat_tests.is_empty() {
        verdict.repeat = Some(RepeatEvidence {
            tests: repeat_tests,
            lanes,
            runs,
        });
    }
    if !shared_tests.is_empty() {
        verdict.shared = Some(SharedEvidence {
            tests: shared_tests,
            other_prs: shared_prs.into_iter().collect(),
        });
    }
}

fn listed(items: &[String]) -> String {
    let more = items.len().saturating_sub(TESTS_SHOWN);
    let mut text = items
        .iter()
        .take(TESTS_SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if more > 0 {
        let _ = write!(text, " and {more} more");
    }
    text
}

fn describe_red(check: &RequiredCheckVerdict) -> String {
    let verb = match check.conclusion.as_deref() {
        Some("cancelled") => "was cancelled",
        Some("timed_out") => "timed out",
        Some("action_required") => "needs action",
        _ => "failed",
    };
    let mut text = format!("{} {verb}", check.name);
    if check.failing_tests.is_empty() {
        if check.source == RequiredSource::CheckRun
            && check.conclusion.as_deref() != Some("cancelled")
        {
            text.push_str(if check.log_read {
                " (no failing test parsed from its log)"
            } else {
                " (log unread; failing tests unknown)"
            });
        }
        return text;
    }
    let _ = write!(text, " {}", listed(&check.failing_tests));
    if let Some(repeat) = &check.repeat {
        let partial = repeat.tests.len() < check.failing_tests.len();
        let of = if partial {
            format!(" of {}", listed(&repeat.tests))
        } else {
            String::new()
        };
        if repeat.lanes.len() >= 2 {
            let noun = if repeat
                .lanes
                .iter()
                .any(|lane| lane.starts_with("merge group"))
            {
                "runs"
            } else {
                "heads"
            };
            let _ = write!(
                text,
                " (REPEAT{of} on {} {noun}: {})",
                repeat.lanes.len(),
                repeat.lanes.join(", ")
            );
        } else {
            let _ = write!(
                text,
                " (REPEAT{of} on {} runs of {})",
                repeat.runs,
                repeat.lanes.join(", ")
            );
        }
    }
    if let Some(shared) = &check.shared {
        let prs = shared
            .other_prs
            .iter()
            .map(|pr| format!("#{pr}"))
            .collect::<Vec<_>>()
            .join(",");
        let which = if shared.tests.len() < check.failing_tests.len() {
            format!("{} ", listed(&shared.tests))
        } else {
            String::new()
        };
        let _ = write!(text, " ({which}also failing on {prs} — likely main/shared)");
    }
    text
}

fn describe_pending(check: &RequiredCheckVerdict) -> String {
    match check.state {
        RequiredState::NotCreated => format!("{} not created", check.name),
        _ => format!(
            "{} {}",
            check.name,
            check.status.as_deref().unwrap_or("pending")
        ),
    }
}

fn others(required: &[RequiredCheckVerdict], shown: RequiredState) -> String {
    let count = |state: RequiredState| {
        required
            .iter()
            .filter(|check| check.state == state && state != shown)
            .count()
    };
    let mut parts = Vec::new();
    let green = count(RequiredState::Green);
    if green > 0 {
        parts.push(format!("{green} green"));
    }
    let pending = count(RequiredState::Running) + count(RequiredState::NotCreated);
    if pending > 0 && shown == RequiredState::Red {
        let names: Vec<String> = required
            .iter()
            .filter(|check| {
                matches!(
                    check.state,
                    RequiredState::Running | RequiredState::NotCreated
                )
            })
            .map(describe_pending)
            .collect();
        parts.push(format!("{pending} pending ({})", names.join(", ")));
    }
    if parts.is_empty() {
        "none".to_owned()
    } else {
        parts.join(", ")
    }
}

fn render_line(
    facts: &VerdictFacts,
    state: VerdictState,
    required: &[RequiredCheckVerdict],
    merge_group: Option<&GroupVerdict>,
    queue: Option<&str>,
) -> String {
    let mut line = format!(
        "VERDICT #{} head {}: {} — ",
        facts.pr,
        short(&facts.head_sha),
        state.label()
    );
    let head_red = required
        .iter()
        .any(|check| check.state == RequiredState::Red);
    match state {
        VerdictState::Red if head_red => {
            let red: Vec<String> = required
                .iter()
                .filter(|check| check.state == RequiredState::Red)
                .map(describe_red)
                .collect();
            let _ = write!(
                line,
                "{}; other required: {}",
                red.join("; "),
                others(required, RequiredState::Red)
            );
        }
        VerdictState::Red => {
            // Red only through the merge group: say what the head shows too,
            // so a green head is not mistaken for a landable one.
            let _ = write!(
                line,
                "head required: {}",
                others(required, RequiredState::Red)
            );
        }
        VerdictState::Pending => {
            let pending: Vec<String> = required
                .iter()
                .filter(|check| {
                    matches!(
                        check.state,
                        RequiredState::Running | RequiredState::NotCreated
                    )
                })
                .map(describe_pending)
                .collect();
            let _ = write!(
                line,
                "{}; other required: {}",
                pending.join(", "),
                others(required, RequiredState::Running)
            );
        }
        VerdictState::Green => {
            if required.is_empty() {
                let _ = write!(line, "no required checks on {}", facts.base);
            } else {
                let _ = write!(line, "all {} required green", required.len());
            }
        }
        VerdictState::Unknown => {}
    }
    if let Some(group) = merge_group {
        let jobs = if group.failed.is_empty() {
            "failed (jobs unread)".to_owned()
        } else {
            format!(
                "failed {}",
                group
                    .failed
                    .iter()
                    .map(describe_red)
                    .map(|text| text.replacen(" failed", "", 1))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        };
        let text = format!(
            "merge group run {} (named for #{}, on this head) {jobs}",
            group.run_id, facts.pr
        );
        if head_red {
            let _ = write!(line, "; {text}");
        } else {
            // Lead with the failure: it is why the verdict is red.
            let prefix = format!(
                "VERDICT #{} head {}: RED — ",
                facts.pr,
                short(&facts.head_sha)
            );
            line = format!("{prefix}{text}; {}", &line[prefix.len()..]);
        }
    }
    if let Some(queue) = queue {
        let _ = write!(line, "; queue: {queue}");
    }
    line
}
