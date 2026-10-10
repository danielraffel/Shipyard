//! Plan the unattended carrier's actions for one pull request from GitHub
//! facts alone.
//!
//! ## What the carrier is
//!
//! An approved, armed pull request can stall with nobody watching: a required
//! run is cancelled before it reaches a runner, or the merge queue removes the
//! pull request because its batch starved. Each of those is an interruption,
//! not a verdict on the code, and the fix is mechanical. The carrier is the
//! one controller that performs those mechanical steps, one action class at a
//! time:
//!
//! * [`CarrierClass::Redispatch`] reruns a cancelled required run on the
//!   current head of an armed pull request.
//! * [`CarrierClass::Rearm`] re-arms native auto-merge on the exact head the
//!   queue removed, when every required merge-group job that did not pass
//!   starved.
//! * [`CarrierClass::UpdateBranch`] proposes a merge-only branch update for an
//!   armed pull request that is `BEHIND`. It is planned, never applied here:
//!   applying it needs the own-lines invariant, which extends the approval to
//!   the merge commit it creates.
//!
//! ## Why only GitHub facts
//!
//! The planner must give the same answer on any host, so a controller can move
//! between hosts and a replay can reproduce a decision. Every input is a
//! GitHub fact: the pull request, its required checks, the workflow runs and
//! jobs those checks name, the merge-queue timeline, and the approval record.
//! Retry budgets are read from GitHub's own `run_attempt`, not from a local
//! ledger, so a restarted controller cannot repeat an action.
//!
//! ## The approval record
//!
//! A pull request is carried only on a head someone approved. The approval is
//! the commit status [`APPROVED_HEAD_CONTEXT`] in state `success` on that exact
//! SHA, posted by the arming actor and linking the review comment that names
//! the head. A status is bound to its SHA by GitHub, so a pushed head carries
//! no approval until someone approves it, and "the head moved since approval"
//! needs no comparison: the current head simply lacks the status. Every agent
//! posts through the same App identity, so the status proves which head was
//! approved, not who approved it. A reviewer's comment carrying a line
//! `reviewed:<full sha>` for the current head is read as the same record, so
//! a verdict comment needs no separate status write.
//!
//! ## What the planner never does
//!
//! It never acts on a pull request that is a draft, conflicting, unarmed,
//! queued, or unapproved at its current head; never reruns a run that failed;
//! never re-arms after a removal whose required jobs failed, conflicted, or
//! were removed by a person; and never acts while a run it would rerun is
//! still live. An unreadable fact holds the pull request.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::gate_cost::GateJobSample;
use crate::gate_cost::proxy::ejection_cause;

/// Commit-status context that records the approved head.
pub const APPROVED_HEAD_CONTEXT: &str = "shipyard/approved-head";
/// Reruns the carrier may cause on one workflow run, read from `run_attempt`.
pub const MAX_REDISPATCHES_PER_RUN: u64 = 2;
/// Reruns the carrier may cause on one pull request within
/// [`REDISPATCH_WINDOW_MINUTES`].
pub const MAX_REDISPATCHES_PER_PR_WINDOW: usize = 2;
/// Window for [`MAX_REDISPATCHES_PER_PR_WINDOW`].
pub const REDISPATCH_WINDOW_MINUTES: i64 = 60;
/// Minutes a job must have waited, from creation to cancellation, before a
/// no-runner cancellation counts as starvation. Starved merge-group jobs on
/// Pulp waited 15 to 20 minutes; a superseding push or a concurrency-group
/// cancel ends a job within seconds, also with no runner name.
pub const STARVATION_MIN_WAIT_MINUTES: i64 = 10;

/// One mechanical action class, graduated to live one at a time.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CarrierClass {
    /// Rerun a cancelled required run on the current head.
    Redispatch,
    /// Re-arm the exact head after an interruption removal.
    Rearm,
    /// Merge the base into a `BEHIND` head (planned only).
    UpdateBranch,
}

impl CarrierClass {
    /// Parse the CLI spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "redispatch" => Some(Self::Redispatch),
            "rearm" => Some(Self::Rearm),
            "update_branch" | "update-branch" => Some(Self::UpdateBranch),
            _ => None,
        }
    }
}

/// The pull request's merge-queue state, as `pr_queue_state` classified it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CarrierQueueFact {
    /// In the queue now.
    Queued,
    /// Auto-merge armed, not yet admitted.
    ArmedNotQueued,
    /// Never armed in the visible timeline.
    NeverArmed,
    /// The last removal was not a merge and the pull request is not armed.
    Ejected {
        /// GitHub's removal reason.
        reason: String,
        /// Whether a new head followed the removal.
        new_head_since: bool,
        /// The merge-group commit the queue built, when the removal names one.
        merge_group_commit: Option<String>,
    },
    /// Merged or closed.
    NotOpen,
    /// The state could not be read.
    Unknown {
        /// What was missing.
        detail: String,
    },
}

/// One required context on the current head.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequiredFact {
    /// Required context name.
    pub context: String,
    /// `QUEUED`, `IN_PROGRESS`, `COMPLETED`, or `MISSING`.
    pub status: String,
    /// Upper-case conclusion, when completed.
    pub conclusion: Option<String>,
    /// Workflow run the check belongs to, when it is an Actions check.
    pub run_id: Option<u64>,
}

/// One workflow job.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobFact {
    /// Job name.
    pub name: String,
    /// Lower-case status.
    pub status: String,
    /// Lower-case conclusion.
    pub conclusion: Option<String>,
    /// Runner that took the job; `None` or empty when none did.
    pub runner_name: Option<String>,
    /// When the job was created (queued).
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// When the job completed.
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

/// One workflow run the plan depends on.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunFact {
    /// Run id.
    pub id: u64,
    /// Workflow name.
    pub workflow: String,
    /// `pull_request`, `merge_group`, ...
    pub event: String,
    /// Head the run built.
    pub head_sha: String,
    /// Lower-case status.
    pub status: String,
    /// Lower-case conclusion.
    pub conclusion: Option<String>,
    /// GitHub's attempt number for this run id.
    pub run_attempt: u64,
    /// When the latest attempt started.
    pub run_started_at: Option<DateTime<Utc>>,
    /// Jobs of the latest attempt, when fetched.
    #[serde(default)]
    pub jobs: Vec<JobFact>,
}

/// Every fact the planner reads for one pull request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CarrierFacts {
    /// `OWNER/REPO`.
    pub repo: String,
    /// Pull-request number.
    pub number: u64,
    /// Current head.
    pub head_sha: String,
    /// Draft flag.
    pub draft: bool,
    /// `mergeStateStatus`, upper case.
    pub merge_state: String,
    /// Queue state.
    pub queue: CarrierQueueFact,
    /// Whether the current head carries an approval record: a successful
    /// [`APPROVED_HEAD_CONTEXT`] status, or a `reviewed:<sha>` comment line
    /// naming the full current head.
    pub approved_head: bool,
    /// Which record proved the approval (`status:<context>` or
    /// `comment:<id>`), for the audit trail.
    #[serde(default)]
    pub approval_evidence: Option<String>,
    /// Required contexts on the current head, in policy order.
    pub required: Vec<RequiredFact>,
    /// Runs on the current head (any event), plus the merge-group runs of the
    /// last removal.
    pub runs: Vec<RunFact>,
    /// When the facts were read.
    pub observed_at: DateTime<Utc>,
}

/// What the carrier would do.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CarrierAction {
    /// Rerun these runs' failed jobs on `head`.
    Redispatch {
        /// Head the runs built.
        head: String,
        /// Runs to rerun.
        run_ids: Vec<u64>,
    },
    /// Arm native auto-merge with `expectedHeadOid = head`.
    Rearm {
        /// The exact approved head.
        head: String,
    },
    /// Merge the base into the branch without rebasing.
    UpdateBranch {
        /// Head the update is expected to start from.
        head: String,
    },
}

impl CarrierAction {
    /// The action's class.
    #[must_use]
    pub const fn class(&self) -> CarrierClass {
        match self {
            Self::Redispatch { .. } => CarrierClass::Redispatch,
            Self::Rearm { .. } => CarrierClass::Rearm,
            Self::UpdateBranch { .. } => CarrierClass::UpdateBranch,
        }
    }
}

/// Why the carrier leaves a pull request alone.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "hold", rename_all = "snake_case")]
pub enum CarrierHold {
    /// Queue state unreadable.
    QueueStateUnknown {
        /// What was missing.
        detail: String,
    },
    /// Merged or closed.
    NotOpen,
    /// Already queued; the queue owns it.
    Queued,
    /// Draft.
    Draft,
    /// Conflicting; a steward resolves it.
    Conflicting {
        /// `mergeStateStatus`.
        merge_state: String,
    },
    /// Never armed; arming is a person's decision.
    Unarmed,
    /// The current head carries no approval record: it moved since approval,
    /// or nobody recorded one.
    HeadNotApproved,
    /// A new head followed the last removal.
    HeadMovedSinceRemoval,
    /// A required check failed on the current head.
    RequiredFailed {
        /// Failed contexts.
        contexts: Vec<String>,
    },
    /// A cancelled required run is still being retried or its run is live.
    RunStillLive {
        /// Runs still live.
        run_ids: Vec<u64>,
    },
    /// A cancelled required context has no readable run on the current head,
    /// or is a commit status that no rerun can reach.
    RunFactMissing {
        /// Contexts whose run could not be checked.
        contexts: Vec<String>,
    },
    /// Retry budget spent.
    RedispatchBudgetSpent {
        /// Runs whose budget is spent.
        run_ids: Vec<u64>,
    },
    /// Required checks still running or missing.
    WaitingRequired {
        /// Contexts not yet terminal.
        contexts: Vec<String>,
    },
    /// Green and armed; GitHub enqueues it.
    AwaitingQueue,
    /// The removal conflicted; a steward resolves it.
    RemovedForConflict,
    /// A person or tool removed it; their decision stands.
    RemovedByPerson {
        /// Removal reason.
        reason: String,
    },
    /// The removal's required jobs did not all starve.
    RemovalNotInterruption {
        /// Non-starved causes.
        causes: Vec<String>,
    },
    /// The removal could not be attributed to any job.
    RemovalUnclassified {
        /// Why.
        detail: String,
    },
}

/// The plan for one pull request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CarrierPlan {
    /// Pull-request number.
    pub number: u64,
    /// Head the plan was made against.
    pub head_sha: String,
    /// The proposed action, or why there is none.
    #[serde(flatten)]
    pub decision: CarrierDecision,
}

/// An action or a hold.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum CarrierDecision {
    /// Act.
    Propose {
        /// The action.
        #[serde(flatten)]
        action: CarrierAction,
    },
    /// Leave alone.
    Hold {
        /// Why.
        #[serde(flatten)]
        hold: CarrierHold,
    },
}

impl CarrierPlan {
    /// The proposed action, if any.
    #[must_use]
    pub const fn action(&self) -> Option<&CarrierAction> {
        match &self.decision {
            CarrierDecision::Propose { action } => Some(action),
            CarrierDecision::Hold { .. } => None,
        }
    }
}

fn hold(facts: &CarrierFacts, hold: CarrierHold) -> CarrierPlan {
    CarrierPlan {
        number: facts.number,
        head_sha: facts.head_sha.clone(),
        decision: CarrierDecision::Hold { hold },
    }
}

fn propose(facts: &CarrierFacts, action: CarrierAction) -> CarrierPlan {
    CarrierPlan {
        number: facts.number,
        head_sha: facts.head_sha.clone(),
        decision: CarrierDecision::Propose { action },
    }
}

fn is_interruption_conclusion(conclusion: &str) -> bool {
    matches!(conclusion, "CANCELLED" | "STARTUP_FAILURE" | "STALE")
}

fn is_pass_conclusion(conclusion: &str) -> bool {
    matches!(conclusion, "SUCCESS" | "NEUTRAL" | "SKIPPED")
}

fn is_live(status: &str) -> bool {
    !status.eq_ignore_ascii_case("completed")
}

/// Plan one pull request.
#[must_use]
pub fn plan(facts: &CarrierFacts) -> CarrierPlan {
    let ejection = match &facts.queue {
        CarrierQueueFact::ArmedNotQueued => None,
        CarrierQueueFact::Ejected {
            reason,
            new_head_since,
            merge_group_commit,
        } => Some((
            reason.as_str(),
            *new_head_since,
            merge_group_commit.as_deref(),
        )),
        CarrierQueueFact::NeverArmed => return hold(facts, CarrierHold::Unarmed),
        CarrierQueueFact::Queued => return hold(facts, CarrierHold::Queued),
        CarrierQueueFact::NotOpen => return hold(facts, CarrierHold::NotOpen),
        CarrierQueueFact::Unknown { detail } => {
            return hold(
                facts,
                CarrierHold::QueueStateUnknown {
                    detail: detail.clone(),
                },
            );
        }
    };
    if facts.draft {
        return hold(facts, CarrierHold::Draft);
    }
    let merge_state = facts.merge_state.to_ascii_uppercase();
    if matches!(merge_state.as_str(), "DIRTY" | "CONFLICTING") {
        return hold(
            facts,
            CarrierHold::Conflicting {
                merge_state: facts.merge_state.clone(),
            },
        );
    }
    if !facts.approved_head {
        return hold(facts, CarrierHold::HeadNotApproved);
    }
    match ejection {
        None => plan_armed(facts, &merge_state),
        Some((reason, new_head_since, commit)) => {
            plan_ejected(facts, reason, new_head_since, commit)
        }
    }
}

/// Required contexts that failed outright, waited, or were interrupted.
struct RequiredSplit {
    failed: Vec<String>,
    waiting: Vec<String>,
    interrupted: Vec<(String, Option<u64>)>,
}

fn split_required(facts: &CarrierFacts) -> RequiredSplit {
    let mut split = RequiredSplit {
        failed: Vec::new(),
        waiting: Vec::new(),
        interrupted: Vec::new(),
    };
    for required in &facts.required {
        if !required.status.eq_ignore_ascii_case("COMPLETED") {
            split.waiting.push(required.context.clone());
            continue;
        }
        let conclusion = required
            .conclusion
            .as_deref()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if is_pass_conclusion(&conclusion) {
            continue;
        }
        if is_interruption_conclusion(&conclusion) {
            split
                .interrupted
                .push((required.context.clone(), required.run_id));
        } else {
            split.failed.push(required.context.clone());
        }
    }
    split
}

fn plan_armed(facts: &CarrierFacts, merge_state: &str) -> CarrierPlan {
    let split = split_required(facts);
    if !split.failed.is_empty() {
        return hold(
            facts,
            CarrierHold::RequiredFailed {
                contexts: split.failed,
            },
        );
    }
    if !split.interrupted.is_empty() {
        return plan_redispatch(facts, &split.interrupted);
    }
    if !split.waiting.is_empty() {
        return hold(
            facts,
            CarrierHold::WaitingRequired {
                contexts: split.waiting,
            },
        );
    }
    if merge_state == "BEHIND" {
        return propose(
            facts,
            CarrierAction::UpdateBranch {
                head: facts.head_sha.clone(),
            },
        );
    }
    hold(facts, CarrierHold::AwaitingQueue)
}

fn plan_redispatch(facts: &CarrierFacts, interrupted: &[(String, Option<u64>)]) -> CarrierPlan {
    let mut missing = Vec::new();
    let mut live = Vec::new();
    let mut spent = Vec::new();
    let mut eligible = Vec::new();
    for (context, run_id) in interrupted {
        // A required status that is not an Actions run cannot be rerun.
        let Some(run) = run_id.and_then(|id| facts.runs.iter().find(|run| run.id == id)) else {
            missing.push(context.clone());
            continue;
        };
        let run_id = &run.id;
        if !run.head_sha.eq_ignore_ascii_case(&facts.head_sha) {
            missing.push(context.clone());
            continue;
        }
        if is_live(&run.status) {
            live.push(*run_id);
            continue;
        }
        if run.run_attempt.saturating_sub(1) >= MAX_REDISPATCHES_PER_RUN {
            spent.push(*run_id);
            continue;
        }
        if !eligible.contains(run_id) {
            eligible.push(*run_id);
        }
    }
    if !missing.is_empty() {
        return hold(facts, CarrierHold::RunFactMissing { contexts: missing });
    }
    if !live.is_empty() {
        return hold(facts, CarrierHold::RunStillLive { run_ids: live });
    }
    if !spent.is_empty() {
        return hold(facts, CarrierHold::RedispatchBudgetSpent { run_ids: spent });
    }
    // Another live run of the same workflow on this head means a retry is
    // already under way, from a person or a re-push.
    let live_same_workflow: Vec<u64> = eligible
        .iter()
        .filter_map(|run_id| facts.runs.iter().find(|run| run.id == *run_id))
        .flat_map(|cancelled| {
            facts.runs.iter().filter(move |other| {
                other.id != cancelled.id
                    && other.workflow == cancelled.workflow
                    && other.head_sha.eq_ignore_ascii_case(&facts.head_sha)
                    && is_live(&other.status)
            })
        })
        .map(|run| run.id)
        .collect();
    if !live_same_workflow.is_empty() {
        return hold(
            facts,
            CarrierHold::RunStillLive {
                run_ids: live_same_workflow,
            },
        );
    }
    let window_start = facts.observed_at - Duration::minutes(REDISPATCH_WINDOW_MINUTES);
    let recent = facts
        .runs
        .iter()
        .filter(|run| {
            run.head_sha.eq_ignore_ascii_case(&facts.head_sha)
                && run.run_attempt > 1
                && run.run_started_at.is_some_and(|at| at > window_start)
        })
        .count();
    let room = MAX_REDISPATCHES_PER_PR_WINDOW.saturating_sub(recent);
    if room == 0 {
        return hold(
            facts,
            CarrierHold::RedispatchBudgetSpent { run_ids: eligible },
        );
    }
    eligible.sort_unstable();
    eligible.truncate(room);
    propose(
        facts,
        CarrierAction::Redispatch {
            head: facts.head_sha.clone(),
            run_ids: eligible,
        },
    )
}

fn plan_ejected(
    facts: &CarrierFacts,
    reason: &str,
    new_head_since: bool,
    merge_group_commit: Option<&str>,
) -> CarrierPlan {
    if new_head_since {
        return hold(facts, CarrierHold::HeadMovedSinceRemoval);
    }
    match reason.to_ascii_lowercase().as_str() {
        "failed_checks" => {}
        "merge_conflict" => return hold(facts, CarrierHold::RemovedForConflict),
        _ => {
            return hold(
                facts,
                CarrierHold::RemovedByPerson {
                    reason: reason.to_owned(),
                },
            );
        }
    }
    let Some(commit) = merge_group_commit else {
        return hold(
            facts,
            CarrierHold::RemovalUnclassified {
                detail: "the removal names no merge-group commit".to_owned(),
            },
        );
    };
    let causes = match removal_causes(facts, commit) {
        Ok(causes) => causes,
        Err(detail) => return hold(facts, CarrierHold::RemovalUnclassified { detail }),
    };
    let not_starved: Vec<String> = causes
        .iter()
        .filter(|cause| cause.as_str() != "starved")
        .cloned()
        .collect();
    if !not_starved.is_empty() {
        return hold(
            facts,
            CarrierHold::RemovalNotInterruption {
                causes: not_starved,
            },
        );
    }
    let split = split_required(facts);
    if !split.failed.is_empty() {
        return hold(
            facts,
            CarrierHold::RequiredFailed {
                contexts: split.failed,
            },
        );
    }
    if !split.waiting.is_empty() || !split.interrupted.is_empty() {
        let mut contexts = split.waiting;
        contexts.extend(split.interrupted.into_iter().map(|(context, _)| context));
        return hold(facts, CarrierHold::WaitingRequired { contexts });
    }
    propose(
        facts,
        CarrierAction::Rearm {
            head: facts.head_sha.clone(),
        },
    )
}

fn waited_for_a_runner(job: &JobFact) -> bool {
    match (job.created_at, job.completed_at) {
        (Some(created), Some(completed)) => {
            completed - created >= Duration::minutes(STARVATION_MIN_WAIT_MINUTES)
        }
        _ => false,
    }
}

/// Non-passing causes across the removal's merge-group runs.
///
/// A job starves when it is cancelled with no runner after waiting at least
/// [`STARVATION_MIN_WAIT_MINUTES`]; one with unreadable times does not.
/// A job counts when its name is a required context, or when it starved: a
/// preamble job that never reached a runner leaves its dependants skipped,
/// so the starvation shows only on the preamble. A failing advisory job does
/// not count; it did not remove the pull request.
fn removal_causes(facts: &CarrierFacts, commit: &str) -> Result<Vec<String>, String> {
    let runs: Vec<&RunFact> = facts
        .runs
        .iter()
        .filter(|run| run.event == "merge_group" && run.head_sha.eq_ignore_ascii_case(commit))
        .collect();
    if runs.is_empty() {
        return Err(format!("no merge-group run built {commit}"));
    }
    let required: Vec<String> = facts
        .required
        .iter()
        .map(|required| required.context.to_ascii_lowercase())
        .collect();
    let mut causes = Vec::new();
    for run in runs {
        if is_live(&run.status) {
            return Err(format!("merge-group run {} is still live", run.id));
        }
        if run.jobs.is_empty() {
            return Err(format!("merge-group run {} has no job facts", run.id));
        }
        for job in &run.jobs {
            let sample = GateJobSample {
                run_id: run.id,
                event: run.event.clone(),
                attempt: run.run_attempt,
                status: job.status.to_ascii_lowercase(),
                conclusion: job.conclusion.as_deref().map(str::to_ascii_lowercase),
                started_at: None,
                completed_at: job.completed_at,
                created_at: job.created_at,
                runner_name: job.runner_name.clone(),
                labels: Vec::new(),
            };
            let Some(cause) = ejection_cause(&sample) else {
                continue;
            };
            // No runner name, but cancelled too soon to be starvation: a
            // superseding push or a concurrency-group cancel.
            let cause = if cause == "starved" && !waited_for_a_runner(job) {
                "cancelled_before_wait".to_owned()
            } else {
                cause
            };
            let is_required = required.contains(&job.name.to_ascii_lowercase());
            if is_required || cause == "starved" {
                causes.push(cause);
            }
        }
    }
    if causes.is_empty() {
        return Err("no required or starved job explains the removal".to_owned());
    }
    causes.sort();
    causes.dedup();
    Ok(causes)
}

#[cfg(test)]
mod tests;
