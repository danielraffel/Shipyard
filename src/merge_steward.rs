//! Conservative cross-repository merge stewardship.
//!
//! This module is transport-free. It classifies current-head pull-request
//! observations and plans safe queued-run coalescing; the CLI adapter owns
//! GitHub reads and mutations. The separation keeps every safety boundary
//! unit-testable without credentials.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

/// One current-head check observed on a pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StewardCheck {
    /// Check/status context.
    pub name: String,
    /// GitHub state (`QUEUED`, `IN_PROGRESS`, or `COMPLETED`).
    pub status: String,
    /// Terminal conclusion, when any.
    pub conclusion: Option<String>,
    /// Workflow run ID parsed from the details URL, when available.
    pub run_id: Option<u64>,
    /// GitHub observation timestamp used to disambiguate duplicate contexts.
    pub observed_at: Option<String>,
}

/// One open pull request observed at an immutable head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StewardPullRequest {
    /// Pull-request number.
    pub number: u64,
    /// Full current head SHA.
    pub head_sha: String,
    /// Current head branch.
    pub head_branch: String,
    /// Whether the PR is a draft.
    pub draft: bool,
    /// GitHub merge-state status.
    pub merge_state: String,
    /// Whether native auto-merge is currently armed.
    pub auto_merge_active: bool,
    /// Current merge-queue position, when already enqueued.
    pub queue_position: Option<u64>,
    /// Labels attached to the PR.
    pub labels: Vec<String>,
    /// Current-head checks.
    pub checks: Vec<StewardCheck>,
}

/// Repository merge-governance facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StewardPolicy {
    /// Repository has a native merge queue for the target branch.
    pub merge_queue: bool,
    /// Repository allows native auto-merge.
    pub native_auto_merge: bool,
    /// Required check contexts. Empty means every observed check gates.
    pub required_contexts: Vec<String>,
    /// Label that opts a PR out of stewardship.
    pub opt_out_label: String,
    /// Maximum transient reruns allowed per immutable head and run.
    pub max_transient_reruns: u32,
}

/// Repository-specific routing vocabulary for bounded capacity preemption.
///
/// Merge-on-green and queued-run coalescing are repository-neutral. Capacity
/// preemption is not: it must know which workflows and runner labels represent
/// cheap shared preamble work versus an expensive build. Unknown repositories
/// therefore receive [`Self::disabled`] rather than inheriting Pulp names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapacityPreemptionPolicy {
    required_workflows: Vec<String>,
    advisory_workflows: Vec<String>,
    preamble_labels: Vec<String>,
    expensive_label_prefixes: Vec<String>,
    known_hosted_labels: Vec<String>,
}

impl CapacityPreemptionPolicy {
    /// No capacity-preemption authority.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            required_workflows: Vec::new(),
            advisory_workflows: Vec::new(),
            preamble_labels: Vec::new(),
            expensive_label_prefixes: Vec::new(),
            known_hosted_labels: Vec::new(),
        }
    }

    /// Exact workflow and routing vocabulary used by the Pulp CI fleet.
    #[must_use]
    pub fn pulp() -> Self {
        Self {
            required_workflows: vec!["Build and Test".to_owned()],
            advisory_workflows: vec![
                "Example validation".to_owned(),
                "GPU Web Plugins".to_owned(),
                "Intel portability (advisory)".to_owned(),
                "IWYU advisory".to_owned(),
                "macOS Cross (advisory)".to_owned(),
            ],
            preamble_labels: vec!["pulp-preamble".to_owned()],
            expensive_label_prefixes: vec!["pulp-build".to_owned()],
            known_hosted_labels: vec![
                "ubuntu-latest".to_owned(),
                "ubuntu-24.04".to_owned(),
                "ubuntu-22.04".to_owned(),
                "windows-latest".to_owned(),
                "windows-2025".to_owned(),
                "windows-2022".to_owned(),
                "macos-latest".to_owned(),
                "macos-15".to_owned(),
                "macos-14".to_owned(),
                "macos-13".to_owned(),
            ],
        }
    }

    /// Select a built-in policy by canonical repository name.
    ///
    /// Only the canonical Pulp repository receives the Pulp preset. Repositories
    /// with the same short name under another owner remain fail-closed.
    #[must_use]
    pub fn for_repository(repo: &str) -> Self {
        if repo.eq_ignore_ascii_case("Generous-Corp/pulp") {
            Self::pulp()
        } else {
            Self::disabled()
        }
    }

    /// Whether this policy authorizes any capacity-preemption observation or mutation.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.preamble_labels.is_empty()
            && (!self.required_workflows.is_empty() || !self.advisory_workflows.is_empty())
    }

    fn workflow_is_required(&self, workflow: &str) -> bool {
        self.required_workflows
            .iter()
            .any(|candidate| candidate == workflow)
    }

    fn workflow_is_advisory(&self, workflow: &str) -> bool {
        self.advisory_workflows
            .iter()
            .any(|candidate| candidate == workflow)
    }

    fn job_uses_preamble(&self, job: &StewardJob) -> bool {
        self.preamble_labels
            .iter()
            .any(|label| has_label(job, label))
    }

    fn job_is_expensive(&self, job: &StewardJob) -> bool {
        job.labels.iter().any(|label| {
            self.expensive_label_prefixes.iter().any(|prefix| {
                label.eq_ignore_ascii_case(prefix)
                    || label
                        .to_ascii_lowercase()
                        .starts_with(&format!("{}-", prefix.to_ascii_lowercase()))
            })
        })
    }

    fn job_is_known_hosted(&self, job: &StewardJob) -> bool {
        job.labels.len() == 1
            && job
                .labels
                .iter()
                .any(|label| self.known_hosted_labels.iter().any(|known| known == label))
    }
}

/// Safe action selected for a pull request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum StewardDecision {
    /// PR is already in the merge queue. Queue order is preserved.
    Queued {
        /// Current zero-based queue position.
        position: u64,
    },
    /// PR explicitly opted out.
    OptedOut,
    /// Draft PRs are never armed.
    Draft,
    /// Head SHA was malformed; mutation is forbidden.
    InvalidHead,
    /// Conflict or unsafe behind state blocks admission.
    NeedsUpdate {
        /// GitHub merge-state status that requires a new validated head.
        merge_state: String,
    },
    /// Required checks have not all materialized or completed.
    WaitingRequired {
        /// Required contexts that are missing or non-terminal.
        contexts: Vec<String>,
    },
    /// A genuine required failure blocks admission and is not rerun.
    RequiredFailed {
        /// Required contexts with genuine or exhausted failures.
        contexts: Vec<String>,
    },
    /// A bounded set of transient workflow runs may be rerun.
    RerunTransient {
        /// Unique workflow run IDs eligible for one bounded rerun.
        run_ids: Vec<u64>,
    },
    /// Arm native merge-queue admission for this exact head.
    ArmMergeQueue,
    /// Merge a private/free-plan repository via REST with an exact-head guard.
    ExactHeadMerge,
}

/// Classify one PR without performing a mutation.
#[must_use]
pub fn classify_pr(
    pr: &StewardPullRequest,
    policy: &StewardPolicy,
    transient_attempts: &BTreeMap<u64, u32>,
) -> StewardDecision {
    if pr
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(&policy.opt_out_label))
    {
        return StewardDecision::OptedOut;
    }
    if let Some(position) = pr.queue_position {
        return StewardDecision::Queued { position };
    }
    if pr.draft {
        return StewardDecision::Draft;
    }
    if !is_full_sha(&pr.head_sha) {
        return StewardDecision::InvalidHead;
    }
    let merge_state = pr.merge_state.to_ascii_uppercase();
    if matches!(merge_state.as_str(), "DIRTY" | "CONFLICTING")
        || (!policy.merge_queue && merge_state == "BEHIND")
    {
        return StewardDecision::NeedsUpdate {
            merge_state: pr.merge_state.clone(),
        };
    }

    let selected = selected_checks(pr, policy);
    let mut waiting = Vec::new();
    let mut failed = Vec::new();
    let mut transient_runs = BTreeSet::new();
    for (context, check) in selected {
        let Some(check) = check else {
            waiting.push(context);
            continue;
        };
        if !check.status.eq_ignore_ascii_case("COMPLETED") {
            waiting.push(context);
            continue;
        }
        let conclusion = check
            .conclusion
            .as_deref()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if matches!(conclusion.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED") {
            continue;
        }
        if is_transient_conclusion(&conclusion)
            && let Some(run_id) = check.run_id
            && transient_attempts.get(&run_id).copied().unwrap_or(0) < policy.max_transient_reruns
        {
            transient_runs.insert(run_id);
        } else {
            failed.push(context);
        }
    }
    if !waiting.is_empty() {
        waiting.sort();
        waiting.dedup();
        return StewardDecision::WaitingRequired { contexts: waiting };
    }
    if !failed.is_empty() {
        failed.sort();
        failed.dedup();
        return StewardDecision::RequiredFailed { contexts: failed };
    }
    if !transient_runs.is_empty() {
        return StewardDecision::RerunTransient {
            run_ids: transient_runs.into_iter().collect(),
        };
    }
    if !policy.merge_queue && merge_state != "CLEAN" {
        return StewardDecision::WaitingRequired {
            contexts: vec![format!("github-merge-state:CLEAN (current={merge_state})")],
        };
    }
    if policy.merge_queue {
        // The exact-head enqueue mutation is idempotent from the steward's
        // perspective and is stronger evidence than autoMergeRequest: GitHub
        // clears that field once a PR is in the queue, while a stranded native
        // request can remain armed without ever materializing a queue entry.
        StewardDecision::ArmMergeQueue
    } else {
        StewardDecision::ExactHeadMerge
    }
}

fn selected_checks<'a>(
    pr: &'a StewardPullRequest,
    policy: &'a StewardPolicy,
) -> Vec<(String, Option<&'a StewardCheck>)> {
    if policy.required_contexts.is_empty() {
        if pr.checks.is_empty() {
            return vec![("at-least-one-current-head-check".to_owned(), None)];
        }
        let mut newest_by_context = BTreeMap::<String, &StewardCheck>::new();
        for check in &pr.checks {
            newest_by_context
                .entry(check.name.to_ascii_lowercase())
                .and_modify(|current| {
                    if check_recency(check) > check_recency(current) {
                        *current = check;
                    }
                })
                .or_insert(check);
        }
        return newest_by_context
            .into_values()
            .map(|check| (check.name.clone(), Some(check)))
            .collect();
    }
    policy
        .required_contexts
        .iter()
        .map(|required| {
            (
                required.clone(),
                pr.checks
                    .iter()
                    .filter(|check| check.name.eq_ignore_ascii_case(required))
                    .max_by_key(|check| check_recency(check)),
            )
        })
        .collect()
}

fn check_recency(check: &StewardCheck) -> (&str, bool) {
    (
        check.observed_at.as_deref().unwrap_or_default(),
        check.status.eq_ignore_ascii_case("COMPLETED"),
    )
}

fn is_transient_conclusion(conclusion: &str) -> bool {
    matches!(
        conclusion,
        "CANCELLED" | "TIMED_OUT" | "STARTUP_FAILURE" | "STALE"
    )
}

/// True only for a complete hexadecimal SHA-1.
#[must_use]
pub fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// One queued/in-progress workflow-run observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StewardRun {
    /// Workflow run ID.
    pub id: u64,
    /// Stable workflow ID.
    pub workflow_id: u64,
    /// GitHub workflow attempt number for this run ID.
    pub run_attempt: u64,
    /// Workflow display name.
    pub workflow: String,
    /// Immutable run head SHA.
    pub head_sha: String,
    /// Head branch, including merge-group branches.
    pub head_branch: String,
    /// GitHub run state.
    pub status: String,
    /// GitHub event (`pull_request`, `merge_group`, etc.).
    pub event: String,
    /// Pull request identity from the workflow-run payload, when present.
    pub pull_request_number: Option<u64>,
    /// Creation timestamp, used only for deterministic retention.
    pub created_at: String,
    /// Jobs fetched for queue-front pressure or preemption candidates.
    pub jobs: Vec<StewardJob>,
}

/// One workflow job used by conservative capacity-preemption policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StewardJob {
    /// Job display name.
    pub name: String,
    /// GitHub job status.
    pub status: String,
    /// Terminal conclusion, when GitHub has completed the job.
    pub conclusion: Option<String>,
    /// Runner labels requested by the job.
    pub labels: Vec<String>,
    /// Assigned runner name, when any.
    pub runner_name: Option<String>,
}

/// Why a queued workflow run is safe to cancel.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunCancellationReason {
    /// Same workflow and immutable SHA already has a preferred run.
    DuplicateImmutableHead,
    /// Pull-request branch has advanced to a different immutable head.
    SupersededPullRequestHead,
    /// Merge queue has materialized a newer speculative head for the same PR.
    SupersededMergeGroupHead,
    /// An advisory PR workflow owns shared preamble capacity.
    AdvisoryPreambleCapacityTheft,
    /// A superseded branch validation head holds capacity ahead of the queue front.
    LowerPriorityBranchPreamble,
}

/// One conservative queued-run cancellation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunCancellation {
    /// Workflow run ID.
    pub run_id: u64,
    /// Cancellation reason.
    pub reason: RunCancellationReason,
}

/// Inputs that prove a queue front is old enough to justify bounded preemption.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueFrontPressure {
    /// Exact speculative merge-group SHA at the queue front.
    pub head_sha: String,
    /// Whether the front has exceeded the configured wait threshold.
    pub old_enough: bool,
}

/// Plan at most `max_preemptions` incident-safe in-progress cancellations.
///
/// Only PR workflows with a running shared-preamble job are eligible. Pushes,
/// merge groups, unknown workflows/jobs, and any run whose expensive
/// `pulp-build` leg started are never selected.
#[must_use]
pub fn plan_capacity_preemptions(
    runs: &[StewardRun],
    current_pull_request_heads: &BTreeMap<u64, String>,
    opted_out_pull_requests: &BTreeSet<u64>,
    policy: &CapacityPreemptionPolicy,
    pressure: &QueueFrontPressure,
    attempted_heads: &BTreeSet<String>,
    max_preemptions: usize,
) -> Vec<RunCancellation> {
    if !policy.is_enabled()
        || max_preemptions == 0
        || !pressure.old_enough
        || !is_full_sha(&pressure.head_sha)
    {
        return Vec::new();
    }
    if !queue_front_waits_for_pool(runs, &pressure.head_sha, policy) {
        return Vec::new();
    }

    let mut candidates = runs
        .iter()
        .filter_map(|run| {
            let reason = preemption_reason(
                run,
                current_pull_request_heads,
                opted_out_pull_requests,
                policy,
            )?;
            if attempted_heads.contains(&preemption_key(run)) {
                return None;
            }
            Some((run, reason))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|(left, left_reason), (right, right_reason)| {
        preemption_rank(*left_reason)
            .cmp(&preemption_rank(*right_reason))
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates
        .into_iter()
        .take(max_preemptions.min(1))
        .map(|(run, reason)| RunCancellation {
            run_id: run.id,
            reason,
        })
        .collect()
}

/// Prove that the exact merge-group front still has recognized pool work waiting.
#[must_use]
pub fn queue_front_waits_for_pool(
    runs: &[StewardRun],
    head_sha: &str,
    policy: &CapacityPreemptionPolicy,
) -> bool {
    is_full_sha(head_sha)
        && runs.iter().any(|run| {
            run.event == "merge_group"
                && run.head_sha.eq_ignore_ascii_case(head_sha)
                && run.jobs.iter().any(|job| {
                    is_waiting(&job.status)
                        && (policy.job_uses_preamble(job) || policy.job_is_expensive(job))
                })
        })
}

fn preemption_reason(
    run: &StewardRun,
    current_pull_request_heads: &BTreeMap<u64, String>,
    opted_out_pull_requests: &BTreeSet<u64>,
    policy: &CapacityPreemptionPolicy,
) -> Option<RunCancellationReason> {
    if run.event != "pull_request"
        || !run.status.eq_ignore_ascii_case("in_progress")
        || !is_full_sha(&run.head_sha)
    {
        return None;
    }
    let pull_request_number = run.pull_request_number?;
    if opted_out_pull_requests.contains(&pull_request_number) {
        return None;
    }
    let running_preamble = run
        .jobs
        .iter()
        .any(|job| job.status.eq_ignore_ascii_case("in_progress") && policy.job_uses_preamble(job));
    if !running_preamble
        || run
            .jobs
            .iter()
            .any(|job| expensive_leg_started(job, policy) || unknown_active_job(job, policy))
    {
        return None;
    }
    if policy.workflow_is_advisory(&run.workflow) {
        Some(RunCancellationReason::AdvisoryPreambleCapacityTheft)
    } else if policy.workflow_is_required(&run.workflow)
        && current_pull_request_heads
            .get(&run.pull_request_number?)
            .is_some_and(|head| !head.eq_ignore_ascii_case(&run.head_sha))
    {
        Some(RunCancellationReason::LowerPriorityBranchPreamble)
    } else {
        None
    }
}

/// Return whether a workflow name is explicitly allowed for capacity preemption.
///
/// This list is intentionally exact and fail-closed. A workflow merely containing
/// the word "advisory" must not gain cancellation authority.
#[must_use]
pub fn is_capacity_preemption_workflow(workflow: &str, policy: &CapacityPreemptionPolicy) -> bool {
    policy.workflow_is_required(workflow) || policy.workflow_is_advisory(workflow)
}

/// Revalidate that a live run still satisfies the exact planned preemption.
#[must_use]
pub fn is_safe_capacity_preemption(
    run: &StewardRun,
    current_pull_request_heads: &BTreeMap<u64, String>,
    opted_out_pull_requests: &BTreeSet<u64>,
    policy: &CapacityPreemptionPolicy,
    expected_reason: RunCancellationReason,
) -> bool {
    preemption_reason(
        run,
        current_pull_request_heads,
        opted_out_pull_requests,
        policy,
    ) == Some(expected_reason)
}

fn expensive_leg_started(job: &StewardJob, policy: &CapacityPreemptionPolicy) -> bool {
    policy.job_is_expensive(job)
        && !is_waiting(&job.status)
        && !(job.status == "completed" && job.conclusion.as_deref() == Some("skipped"))
}

fn is_waiting(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "queued" | "waiting" | "pending" | "requested"
    )
}

fn unknown_active_job(job: &StewardJob, policy: &CapacityPreemptionPolicy) -> bool {
    if !is_nonterminal(&job.status)
        || policy.job_uses_preamble(job)
        || policy.job_is_expensive(job)
        || policy.job_is_known_hosted(job)
    {
        return false;
    }
    true
}

fn is_nonterminal(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "queued" | "waiting" | "pending" | "requested" | "in_progress"
    )
}

fn has_label(job: &StewardJob, expected: &str) -> bool {
    job.labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(expected))
}

/// Stable attempt key shared by the planner and durable adapter ledger.
#[must_use]
pub fn preemption_key(run: &StewardRun) -> String {
    format!(
        "{}:{}",
        run.workflow.to_ascii_lowercase(),
        run.head_sha.to_ascii_lowercase()
    )
}

fn preemption_rank(reason: RunCancellationReason) -> u8 {
    match reason {
        RunCancellationReason::AdvisoryPreambleCapacityTheft => 0,
        RunCancellationReason::LowerPriorityBranchPreamble => 1,
        _ => 2,
    }
}

/// Plan queued-run coalescing.
///
/// In-progress work is never cancelled. Push/schedule runs are never touched.
/// The planner only acts on immutable, full-SHA PR/merge-group observations.
#[must_use]
pub fn plan_run_coalescing(
    runs: &[StewardRun],
    current_pr_heads: &BTreeMap<u64, String>,
    current_merge_group_heads: &BTreeMap<u64, String>,
    opted_out_pull_requests: &BTreeSet<u64>,
) -> Vec<RunCancellation> {
    let mut reasons = BTreeMap::<u64, RunCancellationReason>::new();
    for run in runs {
        if !run.status.eq_ignore_ascii_case("queued") || !is_full_sha(&run.head_sha) {
            continue;
        }
        let Some(pr_number) = run_pull_request_number(run) else {
            continue;
        };
        if opted_out_pull_requests.contains(&pr_number) {
            continue;
        }
        if run.event == "pull_request"
            && let Some(current) = current_pr_heads.get(&pr_number)
            && is_full_sha(current)
            && !current.eq_ignore_ascii_case(&run.head_sha)
        {
            reasons.insert(run.id, RunCancellationReason::SupersededPullRequestHead);
        } else if run.event == "merge_group"
            && let Some(current) = current_merge_group_heads.get(&pr_number)
            && is_full_sha(current)
            && !current.eq_ignore_ascii_case(&run.head_sha)
        {
            reasons.insert(run.id, RunCancellationReason::SupersededMergeGroupHead);
        }
    }

    let mut groups = BTreeMap::<(String, u64, String, u64), Vec<&StewardRun>>::new();
    for run in runs {
        let Some(pr_number) = run_pull_request_number(run) else {
            continue;
        };
        if matches!(run.event.as_str(), "pull_request" | "merge_group")
            && is_full_sha(&run.head_sha)
            && !opted_out_pull_requests.contains(&pr_number)
        {
            groups
                .entry((
                    run.event.clone(),
                    run.workflow_id,
                    run.head_sha.to_ascii_lowercase(),
                    pr_number,
                ))
                .or_default()
                .push(run);
        }
    }
    for group in groups.values_mut() {
        if group.len() < 2 {
            continue;
        }
        group.sort_by(|left, right| {
            let left_running = left.status.eq_ignore_ascii_case("in_progress");
            let right_running = right.status.eq_ignore_ascii_case("in_progress");
            right_running
                .cmp(&left_running)
                .then_with(|| right.created_at.cmp(&left.created_at))
                .then_with(|| right.id.cmp(&left.id))
        });
        let retained_id = group[0].id;
        for duplicate in group.iter().skip(1) {
            if duplicate.id != retained_id && duplicate.status.eq_ignore_ascii_case("queued") {
                reasons
                    .entry(duplicate.id)
                    .or_insert(RunCancellationReason::DuplicateImmutableHead);
            }
        }
    }
    reasons
        .into_iter()
        .map(|(run_id, reason)| RunCancellation { run_id, reason })
        .collect()
}

fn run_pull_request_number(run: &StewardRun) -> Option<u64> {
    run.pull_request_number.or_else(|| {
        (run.event == "merge_group")
            .then(|| merge_group_pr(&run.head_branch))
            .flatten()
    })
}

/// Extract a PR number from GitHub's merge-group branch convention.
#[must_use]
pub fn merge_group_pr(branch: &str) -> Option<u64> {
    let marker = branch.strip_prefix("gh-readonly-queue/")?.find("/pr-")?;
    branch
        .strip_prefix("gh-readonly-queue/")?
        .get(marker + 4..)?
        .split('-')
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests;
