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
        return pr
            .checks
            .iter()
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
                    .max_by_key(|check| {
                        (
                            check.observed_at.as_deref().unwrap_or_default(),
                            check.status.eq_ignore_ascii_case("COMPLETED"),
                        )
                    }),
            )
        })
        .collect()
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
    pressure: &QueueFrontPressure,
    attempted_heads: &BTreeSet<String>,
    max_preemptions: usize,
) -> Vec<RunCancellation> {
    if max_preemptions == 0 || !pressure.old_enough || !is_full_sha(&pressure.head_sha) {
        return Vec::new();
    }
    if !queue_front_waits_for_pool(runs, &pressure.head_sha) {
        return Vec::new();
    }

    let mut candidates = runs
        .iter()
        .filter_map(|run| {
            let reason =
                preemption_reason(run, current_pull_request_heads, opted_out_pull_requests)?;
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
pub fn queue_front_waits_for_pool(runs: &[StewardRun], head_sha: &str) -> bool {
    is_full_sha(head_sha)
        && runs.iter().any(|run| {
            run.event == "merge_group"
                && run.head_sha.eq_ignore_ascii_case(head_sha)
                && run.jobs.iter().any(|job| {
                    is_waiting(&job.status)
                        && (has_label(job, "pulp-preamble") || is_expensive_job(job))
                })
        })
}

fn preemption_reason(
    run: &StewardRun,
    current_pull_request_heads: &BTreeMap<u64, String>,
    opted_out_pull_requests: &BTreeSet<u64>,
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
    let running_preamble = run.jobs.iter().any(|job| {
        job.status.eq_ignore_ascii_case("in_progress") && has_label(job, "pulp-preamble")
    });
    if !running_preamble
        || run
            .jobs
            .iter()
            .any(|job| expensive_leg_started(job) || unknown_active_job(job))
    {
        return None;
    }
    if is_advisory_capacity_workflow(&run.workflow) {
        Some(RunCancellationReason::AdvisoryPreambleCapacityTheft)
    } else if run.workflow == "Build and Test"
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
pub fn is_capacity_preemption_workflow(workflow: &str) -> bool {
    workflow == "Build and Test" || is_advisory_capacity_workflow(workflow)
}

fn is_advisory_capacity_workflow(workflow: &str) -> bool {
    [
        "Example validation",
        "GPU Web Plugins",
        "Intel portability (advisory)",
        "IWYU advisory",
        "macOS Cross (advisory)",
    ]
    .contains(&workflow)
}

/// Revalidate that a live run still satisfies the exact planned preemption.
#[must_use]
pub fn is_safe_capacity_preemption(
    run: &StewardRun,
    current_pull_request_heads: &BTreeMap<u64, String>,
    opted_out_pull_requests: &BTreeSet<u64>,
    expected_reason: RunCancellationReason,
) -> bool {
    preemption_reason(run, current_pull_request_heads, opted_out_pull_requests)
        == Some(expected_reason)
}

fn expensive_leg_started(job: &StewardJob) -> bool {
    is_expensive_job(job)
        && !is_waiting(&job.status)
        && !(job.status == "completed" && job.conclusion.as_deref() == Some("skipped"))
}

fn is_expensive_job(job: &StewardJob) -> bool {
    job.labels.iter().any(|label| {
        let label = label.to_ascii_lowercase();
        label == "pulp-build" || label.starts_with("pulp-build-")
    })
}

fn is_waiting(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "queued" | "waiting" | "pending" | "requested"
    )
}

fn unknown_active_job(job: &StewardJob) -> bool {
    if !is_nonterminal(&job.status)
        || has_label(job, "pulp-preamble")
        || is_expensive_job(job)
        || is_known_hosted_job(job)
    {
        return false;
    }
    true
}

fn is_known_hosted_job(job: &StewardJob) -> bool {
    job.labels.len() == 1
        && job.labels.iter().any(|label| {
            matches!(
                label.as_str(),
                "ubuntu-latest"
                    | "ubuntu-24.04"
                    | "ubuntu-22.04"
                    | "windows-latest"
                    | "windows-2025"
                    | "windows-2022"
                    | "macos-latest"
                    | "macos-15"
                    | "macos-14"
                    | "macos-13"
            )
        })
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
mod tests {
    use super::*;

    fn sha(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    fn green_pr() -> StewardPullRequest {
        StewardPullRequest {
            number: 7,
            head_sha: sha('a'),
            head_branch: "feature".to_owned(),
            draft: false,
            merge_state: "CLEAN".to_owned(),
            auto_merge_active: false,
            queue_position: None,
            labels: Vec::new(),
            checks: vec![StewardCheck {
                name: "required".to_owned(),
                status: "COMPLETED".to_owned(),
                conclusion: Some("SUCCESS".to_owned()),
                run_id: Some(10),
                observed_at: Some("2026-07-26T00:00:00Z".to_owned()),
            }],
        }
    }

    fn queue_policy() -> StewardPolicy {
        StewardPolicy {
            merge_queue: true,
            native_auto_merge: true,
            required_contexts: vec!["required".to_owned()],
            opt_out_label: "shipyard:no-auto-merge".to_owned(),
            max_transient_reruns: 1,
        }
    }

    #[test]
    fn queued_entry_is_authority_even_when_auto_merge_request_is_null() {
        let mut pr = green_pr();
        pr.queue_position = Some(11);
        assert_eq!(
            classify_pr(&pr, &queue_policy(), &BTreeMap::new()),
            StewardDecision::Queued { position: 11 }
        );
    }

    #[test]
    fn ignores_advisory_failure_when_required_context_is_green() {
        let mut pr = green_pr();
        pr.checks.push(StewardCheck {
            name: "advisory".to_owned(),
            status: "COMPLETED".to_owned(),
            conclusion: Some("FAILURE".to_owned()),
            run_id: Some(11),
            observed_at: Some("2026-07-26T00:00:00Z".to_owned()),
        });
        assert_eq!(
            classify_pr(&pr, &queue_policy(), &BTreeMap::new()),
            StewardDecision::ArmMergeQueue
        );
    }

    #[test]
    fn newest_duplicate_required_context_is_authoritative() {
        let mut pr = green_pr();
        pr.checks[0].conclusion = Some("FAILURE".to_owned());
        pr.checks[0].observed_at = Some("2026-07-25T00:00:00Z".to_owned());
        pr.checks.push(StewardCheck {
            name: "required".to_owned(),
            status: "COMPLETED".to_owned(),
            conclusion: Some("SUCCESS".to_owned()),
            run_id: Some(12),
            observed_at: Some("2026-07-26T00:00:00Z".to_owned()),
        });
        assert_eq!(
            classify_pr(&pr, &queue_policy(), &BTreeMap::new()),
            StewardDecision::ArmMergeQueue
        );
    }

    #[test]
    fn private_free_repo_requires_all_observed_checks_and_exact_head_merge() {
        let mut policy = queue_policy();
        policy.merge_queue = false;
        policy.native_auto_merge = false;
        policy.required_contexts.clear();
        assert_eq!(
            classify_pr(&green_pr(), &policy, &BTreeMap::new()),
            StewardDecision::ExactHeadMerge
        );
        let mut red = green_pr();
        red.checks[0].conclusion = Some("FAILURE".to_owned());
        assert!(matches!(
            classify_pr(&red, &policy, &BTreeMap::new()),
            StewardDecision::RequiredFailed { .. }
        ));
    }

    #[test]
    fn genuine_failure_is_not_rerun_but_transient_is_bounded() {
        let mut pr = green_pr();
        pr.checks[0].conclusion = Some("TIMED_OUT".to_owned());
        assert_eq!(
            classify_pr(&pr, &queue_policy(), &BTreeMap::new()),
            StewardDecision::RerunTransient { run_ids: vec![10] }
        );
        let attempts = BTreeMap::from([(10, 1)]);
        assert!(matches!(
            classify_pr(&pr, &queue_policy(), &attempts),
            StewardDecision::RequiredFailed { .. }
        ));
        pr.checks[0].conclusion = Some("FAILURE".to_owned());
        assert!(matches!(
            classify_pr(&pr, &queue_policy(), &BTreeMap::new()),
            StewardDecision::RequiredFailed { .. }
        ));
    }

    #[test]
    fn never_direct_merges_a_behind_private_pr() {
        let mut pr = green_pr();
        pr.merge_state = "BEHIND".to_owned();
        let mut policy = queue_policy();
        policy.merge_queue = false;
        policy.required_contexts.clear();
        assert!(matches!(
            classify_pr(&pr, &policy, &BTreeMap::new()),
            StewardDecision::NeedsUpdate { .. }
        ));
    }

    #[test]
    fn private_exact_head_merge_never_bypasses_blocked_ruleset_state() {
        let mut pr = green_pr();
        pr.merge_state = "BLOCKED".to_owned();
        let mut policy = queue_policy();
        policy.merge_queue = false;
        assert!(matches!(
            classify_pr(&pr, &policy, &BTreeMap::new()),
            StewardDecision::WaitingRequired { contexts }
                if contexts == vec!["github-merge-state:CLEAN (current=BLOCKED)"]
        ));
    }

    #[test]
    fn coalesces_only_queued_duplicate_and_superseded_pr_runs() {
        let runs = vec![
            StewardRun {
                id: 1,
                workflow_id: 8,
                workflow: "Build and Test".to_owned(),
                head_sha: sha('a'),
                head_branch: "feature".to_owned(),
                status: "in_progress".to_owned(),
                event: "pull_request".to_owned(),
                pull_request_number: Some(1),
                created_at: "2026-01-01T00:00:00Z".to_owned(),
                jobs: Vec::new(),
            },
            StewardRun {
                id: 2,
                workflow_id: 8,
                workflow: "Build and Test".to_owned(),
                head_sha: sha('a'),
                head_branch: "feature".to_owned(),
                status: "queued".to_owned(),
                event: "pull_request".to_owned(),
                pull_request_number: Some(1),
                created_at: "2026-01-01T00:01:00Z".to_owned(),
                jobs: Vec::new(),
            },
            StewardRun {
                id: 3,
                workflow_id: 9,
                workflow: "Build and Test".to_owned(),
                head_sha: sha('b'),
                head_branch: "feature".to_owned(),
                status: "queued".to_owned(),
                event: "pull_request".to_owned(),
                pull_request_number: Some(1),
                created_at: "2026-01-01T00:02:00Z".to_owned(),
                jobs: Vec::new(),
            },
        ];
        let plan = plan_run_coalescing(
            &runs,
            &BTreeMap::from([(1, sha('a'))]),
            &BTreeMap::new(),
            &BTreeSet::new(),
        );
        assert_eq!(
            plan,
            vec![
                RunCancellation {
                    run_id: 2,
                    reason: RunCancellationReason::DuplicateImmutableHead,
                },
                RunCancellation {
                    run_id: 3,
                    reason: RunCancellationReason::SupersededPullRequestHead,
                },
            ]
        );
    }

    #[test]
    fn repeated_observation_of_same_run_id_is_not_a_duplicate_run() {
        let run = StewardRun {
            id: 1,
            workflow_id: 8,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('a'),
            head_branch: "feature".to_owned(),
            status: "queued".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(1),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            jobs: Vec::new(),
        };
        assert!(
            plan_run_coalescing(
                &[run.clone(), run],
                &BTreeMap::from([(1, sha('a'))]),
                &BTreeMap::new(),
                &BTreeSet::new(),
            )
            .is_empty()
        );
    }

    #[test]
    fn never_cancels_in_progress_or_non_pr_runs() {
        let runs = vec![
            StewardRun {
                id: 1,
                workflow_id: 1,
                workflow: "Build and Test".to_owned(),
                head_sha: sha('a'),
                head_branch: "feature".to_owned(),
                status: "in_progress".to_owned(),
                event: "pull_request".to_owned(),
                pull_request_number: Some(1),
                created_at: String::new(),
                jobs: Vec::new(),
            },
            StewardRun {
                id: 2,
                workflow_id: 2,
                workflow: "Build and Test".to_owned(),
                head_sha: sha('b'),
                head_branch: "main".to_owned(),
                status: "queued".to_owned(),
                event: "push".to_owned(),
                pull_request_number: None,
                created_at: String::new(),
                jobs: Vec::new(),
            },
        ];
        assert!(
            plan_run_coalescing(
                &runs,
                &BTreeMap::from([(1, sha('c'))]),
                &BTreeMap::new(),
                &BTreeSet::new(),
            )
            .is_empty()
        );
    }

    #[test]
    fn coalescing_uses_pr_identity_and_honors_opt_out() {
        let runs = vec![
            StewardRun {
                id: 10,
                workflow_id: 8,
                workflow: "Build and Test".to_owned(),
                head_sha: sha('a'),
                head_branch: "same-name".to_owned(),
                status: "queued".to_owned(),
                event: "pull_request".to_owned(),
                pull_request_number: Some(1),
                created_at: String::new(),
                jobs: Vec::new(),
            },
            StewardRun {
                id: 11,
                workflow_id: 8,
                workflow: "Build and Test".to_owned(),
                head_sha: sha('b'),
                head_branch: "same-name".to_owned(),
                status: "queued".to_owned(),
                event: "pull_request".to_owned(),
                pull_request_number: Some(2),
                created_at: String::new(),
                jobs: Vec::new(),
            },
        ];
        assert!(
            plan_run_coalescing(
                &runs,
                &BTreeMap::from([(1, sha('a')), (2, sha('c'))]),
                &BTreeMap::new(),
                &BTreeSet::from([2]),
            )
            .is_empty()
        );
    }

    #[test]
    fn head_move_a_to_b_to_a_does_not_leave_cached_superseded_proof() {
        let run = StewardRun {
            id: 12,
            workflow_id: 8,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('a'),
            head_branch: "feature".to_owned(),
            status: "queued".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(1),
            created_at: String::new(),
            jobs: Vec::new(),
        };
        assert!(
            plan_run_coalescing(
                &[run],
                &BTreeMap::from([(1, sha('a'))]),
                &BTreeMap::new(),
                &BTreeSet::new(),
            )
            .is_empty()
        );
    }

    fn job(name: &str, status: &str, labels: &[&str]) -> StewardJob {
        StewardJob {
            name: name.to_owned(),
            status: status.to_owned(),
            conclusion: (status == "skipped").then(|| "skipped".to_owned()),
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            runner_name: None,
        }
    }

    fn pressure_runs() -> Vec<StewardRun> {
        vec![
            StewardRun {
                id: 100,
                workflow_id: 1,
                workflow: "Build and Test".to_owned(),
                head_sha: sha('f'),
                head_branch: "gh-readonly-queue/main/pr-7-deadbeef".to_owned(),
                status: "queued".to_owned(),
                event: "merge_group".to_owned(),
                pull_request_number: None,
                created_at: "2026-01-01T00:00:00Z".to_owned(),
                jobs: vec![job(
                    "macOS (ARM64) [local]",
                    "queued",
                    &["self-hosted", "pulp-build", "pulp-build-vm"],
                )],
            },
            StewardRun {
                id: 200,
                workflow_id: 2,
                workflow: "Example validation".to_owned(),
                head_sha: sha('a'),
                head_branch: "feature-a".to_owned(),
                status: "in_progress".to_owned(),
                event: "pull_request".to_owned(),
                pull_request_number: Some(8),
                created_at: "2026-01-01T00:01:00Z".to_owned(),
                jobs: vec![
                    job(
                        "Detect example changes",
                        "in_progress",
                        &["self-hosted", "pulp-preamble"],
                    ),
                    job(
                        "Validate examples (macOS)",
                        "queued",
                        &["self-hosted", "pulp-build", "pulp-build-vm"],
                    ),
                ],
            },
            StewardRun {
                id: 300,
                workflow_id: 3,
                workflow: "Build and Test".to_owned(),
                head_sha: sha('b'),
                head_branch: "feature-b".to_owned(),
                status: "in_progress".to_owned(),
                event: "pull_request".to_owned(),
                pull_request_number: Some(9),
                created_at: "2026-01-01T00:02:00Z".to_owned(),
                jobs: vec![
                    job("macos", "in_progress", &["self-hosted", "pulp-preamble"]),
                    job(
                        "macOS (ARM64) [local]",
                        "queued",
                        &["self-hosted", "pulp-build", "pulp-build-vm"],
                    ),
                    job("Windows", "in_progress", &["windows-latest"]),
                ],
            },
        ]
    }

    fn current_heads() -> BTreeMap<u64, String> {
        BTreeMap::from([(8, sha('a')), (9, sha('c'))])
    }

    #[test]
    fn preempts_one_advisory_before_lower_priority_branch() {
        let plan = plan_capacity_preemptions(
            &pressure_runs(),
            &current_heads(),
            &BTreeSet::new(),
            &QueueFrontPressure {
                head_sha: sha('f'),
                old_enough: true,
            },
            &BTreeSet::new(),
            usize::MAX,
        );
        assert_eq!(
            plan,
            vec![RunCancellation {
                run_id: 200,
                reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
            }]
        );
    }

    #[test]
    fn gpu_web_plugins_is_exact_advisory_capacity_work_not_cached_supersedence() {
        let mut runs = pressure_runs();
        runs[1].workflow = "GPU Web Plugins".to_owned();
        let plan = plan_capacity_preemptions(
            &runs,
            &current_heads(),
            &BTreeSet::new(),
            &QueueFrontPressure {
                head_sha: sha('f'),
                old_enough: true,
            },
            &BTreeSet::new(),
            1,
        );
        assert_eq!(
            plan,
            vec![RunCancellation {
                run_id: 200,
                reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
            }]
        );
    }

    #[test]
    fn never_preempts_started_expensive_push_or_unknown_work() {
        let mut cases = Vec::new();
        let mut expensive = pressure_runs();
        expensive[1].jobs[1].status = "in_progress".to_owned();
        cases.push(expensive);
        let mut completed_linux = pressure_runs();
        completed_linux[1].jobs.push(job(
            "Linux (x64) [local]",
            "completed",
            &["self-hosted", "pulp-build-linux"],
        ));
        cases.push(completed_linux);
        let mut push = pressure_runs();
        push[1].event = "push".to_owned();
        cases.push(push);
        let mut unknown = pressure_runs();
        unknown[1].jobs.push(job(
            "mystery",
            "in_progress",
            &["self-hosted", "custom-pool"],
        ));
        cases.push(unknown);
        let mut unknown_workflow = pressure_runs();
        unknown_workflow[1].workflow = "Unclassified advisory validation".to_owned();
        cases.push(unknown_workflow);
        let mut wrong_case_workflow = pressure_runs();
        wrong_case_workflow[1].workflow = "EXAMPLE VALIDATION".to_owned();
        cases.push(wrong_case_workflow);
        let mut fake_hosted = pressure_runs();
        fake_hosted[1].jobs.push(job(
            "custom",
            "in_progress",
            &["self-hosted", "custom-latest"],
        ));
        cases.push(fake_hosted);
        let mut requested_unknown = pressure_runs();
        requested_unknown[1]
            .jobs
            .push(job("requested custom", "requested", &["custom-pool"]));
        cases.push(requested_unknown);
        let mut missing_pr_identity = pressure_runs();
        missing_pr_identity[1].pull_request_number = None;
        cases.push(missing_pr_identity);
        for runs in cases {
            let plan = plan_capacity_preemptions(
                &runs,
                &current_heads(),
                &BTreeSet::new(),
                &QueueFrontPressure {
                    head_sha: sha('f'),
                    old_enough: true,
                },
                &BTreeSet::new(),
                1,
            );
            assert!(
                plan.iter().all(|cancellation| cancellation.run_id != 200),
                "unsafe run was selected: {plan:?}"
            );
        }
    }

    #[test]
    fn requires_aged_exact_front_and_durable_attempt_budget() {
        let runs = pressure_runs();
        let attempted = BTreeSet::from([preemption_key(&runs[1])]);
        let plan = plan_capacity_preemptions(
            &runs,
            &current_heads(),
            &BTreeSet::new(),
            &QueueFrontPressure {
                head_sha: sha('f'),
                old_enough: true,
            },
            &attempted,
            1,
        );
        assert_eq!(plan[0].run_id, 300);
        let wrong_pr_identity = BTreeMap::from([(10, sha('c'))]);
        assert!(
            plan_capacity_preemptions(
                &runs,
                &wrong_pr_identity,
                &BTreeSet::new(),
                &QueueFrontPressure {
                    head_sha: sha('f'),
                    old_enough: true,
                },
                &attempted,
                1,
            )
            .is_empty(),
            "a same-name branch from another PR must not prove stale identity"
        );
        assert!(
            plan_capacity_preemptions(
                &runs,
                &current_heads(),
                &BTreeSet::new(),
                &QueueFrontPressure {
                    head_sha: sha('f'),
                    old_enough: false,
                },
                &BTreeSet::new(),
                1
            )
            .is_empty()
        );
        assert!(
            plan_capacity_preemptions(
                &runs,
                &current_heads(),
                &BTreeSet::new(),
                &QueueFrontPressure {
                    head_sha: sha('e'),
                    old_enough: true,
                },
                &BTreeSet::new(),
                1
            )
            .is_empty()
        );

        let mut running_front = runs;
        running_front[0].jobs[0].status = "in_progress".to_owned();
        assert!(
            plan_capacity_preemptions(
                &running_front,
                &current_heads(),
                &BTreeSet::new(),
                &QueueFrontPressure {
                    head_sha: sha('f'),
                    old_enough: true,
                },
                &BTreeSet::new(),
                1
            )
            .is_empty(),
            "an already-running queue front is not waiting for pool capacity"
        );
    }

    #[test]
    fn queued_front_preamble_models_global_scheduler_cap_pressure() {
        let mut runs = pressure_runs();
        runs[0].jobs = vec![job(
            "resolve-provider",
            "queued",
            &["self-hosted", "pulp-preamble"],
        )];
        assert_eq!(
            plan_capacity_preemptions(
                &runs,
                &current_heads(),
                &BTreeSet::new(),
                &QueueFrontPressure {
                    head_sha: sha('f'),
                    old_enough: true,
                },
                &BTreeSet::new(),
                1,
            ),
            vec![RunCancellation {
                run_id: 200,
                reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
            }]
        );
        runs[0].jobs[0].status = "requested".to_owned();
        assert_eq!(
            plan_capacity_preemptions(
                &runs,
                &current_heads(),
                &BTreeSet::new(),
                &QueueFrontPressure {
                    head_sha: sha('f'),
                    old_enough: true,
                },
                &BTreeSet::new(),
                1,
            )[0]
            .run_id,
            200
        );
    }

    #[test]
    fn completed_skipped_expensive_leg_remains_unstarted() {
        let mut runs = pressure_runs();
        runs[1].jobs[1].status = "completed".to_owned();
        runs[1].jobs[1].conclusion = Some("skipped".to_owned());
        assert_eq!(
            plan_capacity_preemptions(
                &runs,
                &current_heads(),
                &BTreeSet::new(),
                &QueueFrontPressure {
                    head_sha: sha('f'),
                    old_enough: true,
                },
                &BTreeSet::new(),
                1,
            )[0]
            .run_id,
            200
        );
    }

    #[test]
    fn opted_out_pull_request_is_never_preempted() {
        let runs = pressure_runs();
        let pressure = QueueFrontPressure {
            head_sha: sha('f'),
            old_enough: true,
        };
        let plan = plan_capacity_preemptions(
            &runs,
            &current_heads(),
            &BTreeSet::from([8]),
            &pressure,
            &BTreeSet::new(),
            1,
        );
        assert_eq!(plan[0].run_id, 300);
        assert!(
            plan_capacity_preemptions(
                &runs,
                &current_heads(),
                &BTreeSet::from([8, 9]),
                &pressure,
                &BTreeSet::new(),
                1,
            )
            .is_empty()
        );
    }
}
