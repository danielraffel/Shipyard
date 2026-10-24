//! Transport-free planning for draining a pull-request CI backlog on a small
//! self-hosted runner pool.
//!
//! Three plays keep a backlog moving without spending runner or agent time on
//! work that cannot land:
//!
//! * **triage** marks a pull-request workflow run *doomed* when its result can
//!   never let the pull request merge at that head: the branch advanced past
//!   the run, the pull request conflicts with its base, or a required check has
//!   already failed on the current head.
//! * **prioritize** yields queued, not-yet-started runs that sit ahead of one
//!   pull request's waiting jobs on the same runner labels. Runs with any job
//!   already in progress are never selected.
//! * **resume** re-runs what an earlier pass cancelled once its pull request is
//!   still open, still at the same head, and no longer doomed.
//!
//! The CLI adapter owns GitHub reads, mutations, and the restore ledger file;
//! every decision lives here so it can be tested without credentials.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::merge_steward::{
    RequiredCheck, StewardCheck, StewardPullRequest, StewardRun, is_full_sha,
    selected_required_check,
};

/// Label that explicitly marks a pull request as the fix that unblocks the
/// base branch. Text heuristics apply only while a required check is red on
/// the base; the label is honoured on its own.
pub const UNBLOCK_LABEL: &str = "shipyard:unblock";

/// Words that, next to a red required context name, mark a pull request as an
/// intended fix. Matched as whole words so `fixture` or `prefix` never count.
const FIX_WORDS: [&str; 17] = [
    "fix",
    "fixes",
    "fixed",
    "fixing",
    "unblock",
    "unblocks",
    "unblocked",
    "unblocking",
    "repair",
    "repairs",
    "repaired",
    "restore",
    "restores",
    "restored",
    "revert",
    "reverts",
    "reverted",
];

/// One open pull request plus the free text used for unblocking detection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrainPullRequest {
    /// Current-head facts. `pr.checks` must belong to `pr.head_sha`.
    pub pr: StewardPullRequest,
    /// Pull-request title.
    pub title: String,
    /// Pull-request description.
    pub body: String,
}

/// One workflow job with the queue timestamp GitHub assigned to it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DrainJob {
    /// Workflow job database ID.
    pub id: u64,
    /// Job display name.
    pub name: String,
    /// GitHub job status (`queued`, `in_progress`, `completed`, ...).
    pub status: String,
    /// Runner labels the job requests.
    pub labels: Vec<String>,
    /// When the job entered the runner queue.
    pub created_at: String,
}

/// Why a run cannot help its pull request merge at the run's head.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DoomReason {
    /// The pull request's branch advanced; this run validates an old head.
    SupersededHead {
        /// Current pull-request head.
        current_head: String,
    },
    /// The pull request conflicts with its base and needs a new head.
    Conflicting {
        /// GitHub merge-state status.
        merge_state: String,
    },
    /// A required check already failed on this head.
    RequiredCheckFailed {
        /// Failed required contexts.
        contexts: Vec<String>,
    },
}

/// Triage verdict for one active workflow run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum RunVerdict {
    /// The run can still contribute to a merge.
    Viable,
    /// The run's result cannot let its pull request merge at this head.
    Doomed {
        /// Proof of doom.
        reason: DoomReason,
    },
    /// The run is not a pull-request run (merge group, push, schedule, ...).
    /// Those are never touched: merge-queue landing always beats PR-head CI.
    NotPullRequest,
    /// The owning pull request is in the merge queue. Its runs are never
    /// cancelled, even when they look doomed.
    InMergeQueue,
    /// No open pull request on the observed base owns this run.
    Unattributed,
}

/// One triaged workflow run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunTriage {
    /// Workflow run ID.
    pub run_id: u64,
    /// Workflow display name.
    pub workflow: String,
    /// Run status at observation.
    pub status: String,
    /// Run event.
    pub event: String,
    /// Owning pull request, when attributed.
    pub pr_number: Option<u64>,
    /// Immutable run head.
    pub head_sha: String,
    /// Run creation time.
    pub created_at: String,
    /// Decision.
    #[serde(flatten)]
    pub verdict: RunVerdict,
    /// Whether `--apply` may cancel this run (see [`may_cancel_doomed`]).
    pub cancel: bool,
}

/// Whether a doomed run in `status` may be cancelled.
///
/// A superseded or conflicting head is proven by the pull request itself, so
/// those runs are cancelled whether queued or running. A failed required check
/// is weaker evidence -- a lost runner also reports `failure` -- so on that
/// signal only runs that have not started are cancelled; a running gate is left
/// to finish.
#[must_use]
pub fn may_cancel_doomed(reason: &DoomReason, status: &str) -> bool {
    match reason {
        DoomReason::SupersededHead { .. } | DoomReason::Conflicting { .. } => run_is_active(status),
        DoomReason::RequiredCheckFailed { .. } => job_is_waiting(status),
    }
}

/// Find the open pull request a run belongs to.
///
/// GitHub omits `pull_requests[]` for some runs (forks, re-targeted bases), so
/// a pull-request run without one is matched by head branch, and only when
/// exactly one open pull request uses that branch.
#[must_use]
pub fn pull_request_for_run<'a>(
    run: &StewardRun,
    prs: &'a [DrainPullRequest],
) -> Option<&'a DrainPullRequest> {
    if let Some(number) = run.pull_request_number {
        return prs.iter().find(|pr| pr.pr.number == number);
    }
    let mut matches = prs
        .iter()
        .filter(|pr| !run.head_branch.is_empty() && pr.pr.head_branch == run.head_branch);
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// Whether a run is still active (queued, waiting, or running).
#[must_use]
pub fn run_is_active(status: &str) -> bool {
    job_is_waiting(status) || status.eq_ignore_ascii_case("in_progress")
}

/// Whether a run or job status means it has not started on a runner.
#[must_use]
pub fn job_is_waiting(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "queued" | "waiting" | "pending" | "requested"
    )
}

/// Doom reason for a run at `run_head` that belongs to `pr`, if any.
///
/// A failed required check produced by the run itself is ignored: a queued
/// re-run of that exact run is the repair, not a doomed duplicate.
#[must_use]
pub fn doom_reason(
    run_id: Option<u64>,
    run_head: &str,
    pr: &DrainPullRequest,
    required: &[RequiredCheck],
) -> Option<DoomReason> {
    if is_full_sha(&pr.pr.head_sha) && !pr.pr.head_sha.eq_ignore_ascii_case(run_head) {
        return Some(DoomReason::SupersededHead {
            current_head: pr.pr.head_sha.clone(),
        });
    }
    let merge_state = pr.pr.merge_state.to_ascii_uppercase();
    if matches!(merge_state.as_str(), "DIRTY" | "CONFLICTING") {
        return Some(DoomReason::Conflicting {
            merge_state: pr.pr.merge_state.clone(),
        });
    }
    let contexts = failed_required_contexts(&pr.pr.checks, required, run_id);
    (!contexts.is_empty()).then_some(DoomReason::RequiredCheckFailed { contexts })
}

/// Required contexts whose latest check on this head failed for good.
///
/// Transient conclusions (`cancelled`, `timed_out`, `startup_failure`,
/// `stale`) are re-runnable and therefore not failures here.
#[must_use]
pub fn failed_required_contexts(
    checks: &[StewardCheck],
    required: &[RequiredCheck],
    ignore_run_id: Option<u64>,
) -> Vec<String> {
    let mut failed = required
        .iter()
        .filter_map(|rule| {
            let check = selected_required_check(checks, rule)?;
            let terminal_failure = check.status.eq_ignore_ascii_case("completed")
                && check.conclusion.as_deref().is_some_and(|conclusion| {
                    matches!(
                        conclusion.to_ascii_uppercase().as_str(),
                        "FAILURE" | "ACTION_REQUIRED"
                    )
                });
            let produced_by_ignored_run = ignore_run_id.is_some() && check.run_id == ignore_run_id;
            (terminal_failure && !produced_by_ignored_run).then(|| rule.label())
        })
        .collect::<Vec<_>>();
    failed.sort();
    failed.dedup();
    failed
}

/// Whether a run is ordinary pull-request CI that queue draining may act on.
///
/// Merge-group runs (by event or by GitHub's `gh-readonly-queue/` branch),
/// pushes, schedules, and dispatches are excluded everywhere, so draining the
/// backlog can never cancel, re-run, or jump ahead of merge-queue landing.
#[must_use]
pub fn is_pull_request_run(run: &StewardRun) -> bool {
    run.event == "pull_request" && !run.head_branch.starts_with("gh-readonly-queue/")
}

/// Whether the pull request currently holds a merge-queue entry.
#[must_use]
pub fn in_merge_queue(pr: &DrainPullRequest) -> bool {
    pr.pr.queue_position.is_some()
}

/// Triage every active run. Only pull-request runs can be doomed.
#[must_use]
pub fn plan_triage(
    runs: &[StewardRun],
    prs: &[DrainPullRequest],
    required: &[RequiredCheck],
) -> Vec<RunTriage> {
    let mut out = runs
        .iter()
        .filter(|run| run_is_active(&run.status))
        .map(|run| {
            let pr = is_pull_request_run(run)
                .then(|| pull_request_for_run(run, prs))
                .flatten();
            let verdict = if !is_pull_request_run(run) {
                RunVerdict::NotPullRequest
            } else if !is_full_sha(&run.head_sha) {
                RunVerdict::Unattributed
            } else if pr.is_some_and(in_merge_queue) {
                RunVerdict::InMergeQueue
            } else if let Some(pr) = pr {
                doom_reason(Some(run.id), &run.head_sha, pr, required)
                    .map_or(RunVerdict::Viable, |reason| RunVerdict::Doomed { reason })
            } else {
                RunVerdict::Unattributed
            };
            RunTriage {
                run_id: run.id,
                workflow: run.workflow.clone(),
                status: run.status.clone(),
                event: run.event.clone(),
                pr_number: pr.map(|pr| pr.pr.number),
                head_sha: run.head_sha.clone(),
                created_at: run.created_at.clone(),
                cancel: match &verdict {
                    RunVerdict::Doomed { reason } => may_cancel_doomed(reason, &run.status),
                    _ => false,
                },
                verdict,
            }
        })
        .collect::<Vec<_>>();
    out.sort_by_key(|triage| triage.run_id);
    out
}

/// A run plus its jobs, for label-aware priority planning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrainRun {
    /// Run observation.
    pub run: StewardRun,
    /// Every job of the run's latest attempt.
    pub jobs: Vec<DrainJob>,
}

/// Why a priority jump was refused.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum PrioritizeRefusal {
    /// The pull request is not open on the observed base.
    NotOpen,
    /// Draft pull requests are not worth a jump.
    Draft,
    /// The pull request itself cannot merge at its head.
    Doomed {
        /// Proof of doom.
        reason: DoomReason,
    },
    /// The pull request has no waiting job at its current head.
    NothingWaiting,
    /// The pull request is already in the merge queue; its merge-group run,
    /// not PR-head CI, decides whether it lands.
    InMergeQueue,
}

/// One run that must yield so the prioritized pull request runs sooner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Yield {
    /// Workflow run to cancel.
    pub run_id: u64,
    /// Workflow display name.
    pub workflow: String,
    /// Pull request that owns the yielding run.
    pub pr_number: Option<u64>,
    /// Immutable run head.
    pub head_sha: String,
    /// Waiting jobs in this run that queue ahead on shared labels.
    pub competing_jobs: Vec<String>,
}

/// A run that competes ahead of the target but is left alone.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct KeptAhead {
    /// Workflow run ID.
    pub run_id: u64,
    /// Pull request that owns the run, when known.
    pub pr_number: Option<u64>,
    /// Why it was kept.
    pub reason: String,
}

/// Priority-jump plan for one pull request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PrioritizePlan {
    /// Prioritized pull request.
    pub pr_number: u64,
    /// Current head being prioritized.
    pub head_sha: Option<String>,
    /// Refusal, when the jump is not allowed.
    pub refusal: Option<PrioritizeRefusal>,
    /// Waiting job IDs of the prioritized pull request.
    pub target_jobs: Vec<u64>,
    /// Runs to cancel and record for resume.
    pub yields: Vec<Yield>,
    /// Competing runs that were not selected.
    pub kept: Vec<KeptAhead>,
}

/// Plan a priority jump for `target`.
///
/// A run yields only when it is a pull-request run for another pull request,
/// none of its jobs has started (`in_progress`), and at least one of its waiting
/// jobs entered the queue before one of the target's waiting jobs while
/// requesting a compatible runner label set. Merge-group and push runs are
/// never selected; they already hold queue priority.
#[must_use]
pub fn plan_prioritize(
    target: u64,
    runs: &[DrainRun],
    prs: &[DrainPullRequest],
    required: &[RequiredCheck],
) -> PrioritizePlan {
    let mut plan = PrioritizePlan {
        pr_number: target,
        head_sha: None,
        refusal: None,
        target_jobs: Vec::new(),
        yields: Vec::new(),
        kept: Vec::new(),
    };
    let Some(pr) = prs.iter().find(|pr| pr.pr.number == target) else {
        plan.refusal = Some(PrioritizeRefusal::NotOpen);
        return plan;
    };
    plan.head_sha = Some(pr.pr.head_sha.clone());
    if pr.pr.draft {
        plan.refusal = Some(PrioritizeRefusal::Draft);
        return plan;
    }
    if in_merge_queue(pr) {
        plan.refusal = Some(PrioritizeRefusal::InMergeQueue);
        return plan;
    }
    if let Some(reason) = doom_reason(None, &pr.pr.head_sha, pr, required) {
        plan.refusal = Some(PrioritizeRefusal::Doomed { reason });
        return plan;
    }
    let target_jobs = runs
        .iter()
        .filter(|drain| {
            is_pull_request_run(&drain.run)
                && drain.run.head_sha.eq_ignore_ascii_case(&pr.pr.head_sha)
                && pull_request_for_run(&drain.run, prs)
                    .is_some_and(|owner| owner.pr.number == target)
        })
        .flat_map(|drain| drain.jobs.iter())
        .filter(|job| job_is_waiting(&job.status))
        .collect::<Vec<_>>();
    if target_jobs.is_empty() {
        plan.refusal = Some(PrioritizeRefusal::NothingWaiting);
        return plan;
    }
    plan.target_jobs = target_jobs.iter().map(|job| job.id).collect();

    for drain in runs {
        let run = &drain.run;
        if !is_pull_request_run(run) || !run_is_active(&run.status) {
            continue;
        }
        let owner_pr = pull_request_for_run(run, prs);
        let owner = owner_pr.map(|owner| owner.pr.number);
        if owner == Some(target) {
            continue;
        }
        let competing = drain
            .jobs
            .iter()
            .filter(|job| job_is_waiting(&job.status))
            .filter(|job| {
                target_jobs.iter().any(|target_job| {
                    queues_ahead(job, target_job) && labels_compete(job, target_job)
                })
            })
            .map(|job| job.name.clone())
            .collect::<Vec<_>>();
        if competing.is_empty() {
            continue;
        }
        if owner_pr.is_some_and(in_merge_queue) {
            plan.kept.push(KeptAhead {
                run_id: run.id,
                pr_number: owner,
                reason: "its pull request is in the merge queue".to_owned(),
            });
            continue;
        }
        if drain
            .jobs
            .iter()
            .any(|job| job.status.eq_ignore_ascii_case("in_progress"))
        {
            plan.kept.push(KeptAhead {
                run_id: run.id,
                pr_number: owner,
                reason: "a job of this run is already in progress".to_owned(),
            });
            continue;
        }
        plan.yields.push(Yield {
            run_id: run.id,
            workflow: run.workflow.clone(),
            pr_number: owner,
            head_sha: run.head_sha.clone(),
            competing_jobs: competing,
        });
    }
    plan.yields.sort_by_key(|entry| entry.run_id);
    plan.kept.sort_by_key(|entry| entry.run_id);
    plan
}

/// Whether `job` entered the queue strictly before `target`.
fn queues_ahead(job: &DrainJob, target: &DrainJob) -> bool {
    match (parse_time(&job.created_at), parse_time(&target.created_at)) {
        (Some(job_at), Some(target_at)) => job_at < target_at,
        // Unknown queue time: cannot prove the job is ahead.
        _ => false,
    }
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// Whether one runner could serve either job: GitHub assigns a job to a runner
/// that carries every requested label, so two label sets compete when one
/// contains the other.
#[must_use]
pub fn labels_compete(left: &DrainJob, right: &DrainJob) -> bool {
    let normalize = |job: &DrainJob| {
        job.labels
            .iter()
            .map(|label| label.to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
    };
    let left = normalize(left);
    let right = normalize(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    left.is_subset(&right) || right.is_subset(&left)
}

/// Required contexts that are red on the base branch head.
#[must_use]
pub fn red_required_contexts(
    base_checks: &[StewardCheck],
    required: &[RequiredCheck],
) -> Vec<String> {
    let mut red = required
        .iter()
        .filter(|rule| {
            selected_required_check(base_checks, rule).is_some_and(|check| {
                check.status.eq_ignore_ascii_case("completed")
                    && check.conclusion.as_deref().is_some_and(|conclusion| {
                        matches!(
                            conclusion.to_ascii_uppercase().as_str(),
                            "FAILURE" | "ACTION_REQUIRED" | "TIMED_OUT"
                        )
                    })
            })
        })
        .map(|rule| rule.context.clone())
        .collect::<Vec<_>>();
    red.sort();
    red.dedup();
    red
}

/// A pull request that looks like the fix for a red base.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UnblockCandidate {
    /// Pull-request number.
    pub pr_number: u64,
    /// Human-readable evidence.
    pub evidence: Vec<String>,
}

/// Find pull requests that say they fix a check that is red on the base.
///
/// A pull request carrying [`UNBLOCK_LABEL`] always qualifies. Otherwise its
/// title or description must name a red required context *and* use a fix word
/// (`fix`, `unblock`, `repair`, `restore`, `revert`). Drafts, doomed heads, and
/// pull requests whose own run of the red context already failed are excluded.
#[must_use]
pub fn unblocking_candidates(
    prs: &[DrainPullRequest],
    red_contexts: &[String],
    required: &[RequiredCheck],
) -> Vec<UnblockCandidate> {
    let mut out = Vec::new();
    for pr in prs {
        if pr.pr.draft || doom_reason(None, &pr.pr.head_sha, pr, required).is_some() {
            continue;
        }
        let mut evidence = Vec::new();
        if pr
            .pr
            .labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case(UNBLOCK_LABEL))
        {
            evidence.push(format!("label `{UNBLOCK_LABEL}`"));
        }
        let text = format!("{}\n{}", pr.title, pr.body).to_ascii_lowercase();
        if has_fix_word(&text) {
            for context in red_contexts {
                if contains_bounded(&text, &context.to_ascii_lowercase()) {
                    evidence.push(format!("mentions red required check `{context}`"));
                }
            }
        }
        if !evidence.is_empty() {
            out.push(UnblockCandidate {
                pr_number: pr.pr.number,
                evidence,
            });
        }
    }
    out
}

fn has_fix_word(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| FIX_WORDS.contains(&word))
}

/// Substring match bounded by non-identifier characters on both sides.
fn contains_bounded(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    haystack.match_indices(needle).any(|(start, _)| {
        let before = haystack[..start].chars().next_back();
        let after = haystack[start + needle.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

/// Why a ledger entry's run was cancelled.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CancelCause {
    /// Triage proved the run doomed.
    Doomed {
        /// Proof of doom at cancellation.
        reason: DoomReason,
    },
    /// The run yielded its queue position to another pull request.
    YieldedTo {
        /// Prioritized pull request.
        pr_number: u64,
    },
}

/// One cancelled run that may be re-run later.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RestoreEntry {
    /// Canonical owner/repository slug.
    pub repo: String,
    /// Base branch observed when the run was cancelled.
    pub base: String,
    /// Workflow run ID.
    pub run_id: u64,
    /// Workflow display name.
    pub workflow: String,
    /// Owning pull request.
    pub pr_number: u64,
    /// Immutable run head.
    pub head_sha: String,
    /// Run status when cancelled.
    pub status_at_cancel: String,
    /// Cancellation cause.
    pub cause: CancelCause,
    /// When the entry was recorded.
    pub recorded_at: String,
}

/// Durable restore ledger for cancelled runs.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RestoreLedger {
    /// Schema version.
    #[serde(default = "ledger_version")]
    pub version: u32,
    /// Entries keyed by [`RestoreLedger::key`].
    #[serde(default)]
    pub entries: BTreeMap<String, RestoreEntry>,
}

fn ledger_version() -> u32 {
    1
}

impl RestoreLedger {
    /// Stable entry key.
    #[must_use]
    pub fn key(repo: &str, run_id: u64) -> String {
        format!("{}#{run_id}", repo.to_ascii_lowercase())
    }

    /// Record or replace an entry.
    pub fn record(&mut self, entry: RestoreEntry) {
        self.version = ledger_version();
        self.entries
            .insert(Self::key(&entry.repo, entry.run_id), entry);
    }

    /// Remove an entry.
    pub fn remove(&mut self, repo: &str, run_id: u64) -> Option<RestoreEntry> {
        self.entries.remove(&Self::key(repo, run_id))
    }

    /// Entries for one repository, ordered by run ID.
    #[must_use]
    pub fn for_repo(&self, repo: &str) -> Vec<&RestoreEntry> {
        let mut entries = self
            .entries
            .values()
            .filter(|entry| entry.repo.eq_ignore_ascii_case(repo))
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.run_id);
        entries
    }
}

/// What resume should do with one ledger entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ResumeDecision {
    /// Re-run the cancelled jobs, then clear the entry.
    Rerun,
    /// Clear the entry without re-running it.
    Drop {
        /// Why the run is no longer worth re-running.
        reason: String,
    },
    /// Keep the entry for a later resume.
    Keep {
        /// Why it is not ready.
        reason: DoomReason,
    },
    /// The pull request is in the merge queue. A PR-head re-run would compete
    /// with merge-queue landing, so the entry waits until it leaves the queue.
    HoldForMergeQueue,
}

/// Decide whether a ledger entry should be re-run now.
#[must_use]
pub fn resume_decision(
    entry: &RestoreEntry,
    prs: &[DrainPullRequest],
    required: &[RequiredCheck],
) -> ResumeDecision {
    let Some(pr) = prs.iter().find(|pr| pr.pr.number == entry.pr_number) else {
        return ResumeDecision::Drop {
            reason: "pull request is no longer open on the base".to_owned(),
        };
    };
    match doom_reason(Some(entry.run_id), &entry.head_sha, pr, required) {
        None if in_merge_queue(pr) => ResumeDecision::HoldForMergeQueue,
        None => ResumeDecision::Rerun,
        Some(DoomReason::SupersededHead { current_head }) => ResumeDecision::Drop {
            reason: format!(
                "pull request advanced to {current_head}; its new head has its own runs"
            ),
        },
        Some(reason) => ResumeDecision::Keep { reason },
    }
}

/// Prove that the live run a ledger entry names is the pull-request run that
/// was cancelled, before anything re-runs it.
///
/// The ledger is a host-local file any local process can write, so its run ID
/// is a claim, not authority. The live run must be a `pull_request` run off a
/// non-merge-queue branch, at the entry's exact head, owned by the entry's
/// pull request (by GitHub's association, or by branch when GitHub omits it).
pub fn verify_live_run(
    entry: &RestoreEntry,
    live: &StewardRun,
    pr_head_branch: Option<&str>,
) -> Result<(), String> {
    if live.id != entry.run_id {
        return Err(format!(
            "live run is {} but the entry names {}",
            live.id, entry.run_id
        ));
    }
    if !is_pull_request_run(live) {
        return Err(format!(
            "run {} is a `{}` run on `{}`, not pull-request CI",
            live.id, live.event, live.head_branch
        ));
    }
    if !is_full_sha(&live.head_sha) || !live.head_sha.eq_ignore_ascii_case(&entry.head_sha) {
        return Err(format!(
            "run {} is at {}, not the recorded head {}",
            live.id, live.head_sha, entry.head_sha
        ));
    }
    let owned = match live.pull_request_number {
        Some(number) => number == entry.pr_number,
        None => pr_head_branch.is_some_and(|branch| branch == live.head_branch),
    };
    if !owned {
        return Err(format!(
            "run {} does not belong to pull request #{}",
            live.id, entry.pr_number
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
