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
    /// GitHub surface that produced the observation.
    pub source: StewardCheckSource,
    /// GitHub App database ID that produced this check run. Legacy commit
    /// statuses and observations whose producer is unavailable have no ID.
    pub app_id: Option<u64>,
    /// GitHub state (`QUEUED`, `IN_PROGRESS`, or `COMPLETED`).
    pub status: String,
    /// Terminal conclusion, when any.
    pub conclusion: Option<String>,
    /// Workflow run ID parsed from the details URL, when available.
    pub run_id: Option<u64>,
    /// GitHub observation timestamp used to disambiguate duplicate contexts.
    pub observed_at: Option<String>,
}

/// GitHub check-rollup surface that produced a check observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StewardCheckSource {
    /// A GitHub Actions/App check run.
    CheckRun,
    /// A commit status context.
    StatusContext,
}

/// One required status-check rule from repository governance.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RequiredCheck {
    /// Required check/status context.
    pub context: String,
    /// GitHub App database ID required to produce the check, when pinned.
    pub app_id: Option<u64>,
}

impl RequiredCheck {
    pub(crate) fn label(&self) -> String {
        self.app_id.map_or_else(
            || self.context.clone(),
            |app_id| format!("{} (app_id={app_id})", self.context),
        )
    }
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
    /// Authoritative required check rules. Empty means mutation is refused
    /// because the observed check set cannot prove complete materialization.
    pub required_checks: Vec<RequiredCheck>,
    /// Label that opts a PR out of stewardship.
    pub opt_out_label: String,
    /// Labels that denote unresolved or otherwise non-authoritative PR
    /// provenance. A matching label denies all steward queue, rerun, and
    /// cancellation authority until a later live observation proves it was
    /// removed.
    pub provenance_blocking_labels: Vec<String>,
    /// Label that explicitly hands a PR to the steward. `None` preserves the
    /// legacy classify-all behavior for library callers; the CLI always sets
    /// this to a concrete label.
    pub managed_label: Option<String>,
    /// Successful commit-status context required on the current immutable
    /// head when `managed_label` is configured.
    pub handoff_context: String,
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
        !self.preamble_labels.is_empty() && !self.advisory_workflows.is_empty()
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
    /// PR has not been explicitly handed to the steward.
    Unmanaged,
    /// PR carries the management label but the current immutable head has no
    /// successful handoff receipt.
    HandoffMissing,
    /// PR is already in the merge queue. Queue order is preserved.
    Queued {
        /// Current zero-based queue position.
        position: u64,
    },
    /// PR explicitly opted out.
    OptedOut,
    /// Current PR metadata says its provenance is unresolved. The steward may
    /// observe the PR, but it receives no mutation authority.
    ProvenanceBlocked {
        /// Configured blocker labels observed on the current PR, normalized to
        /// the configured spelling for deterministic reports.
        labels: Vec<String>,
    },
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
    /// Refuse client-side direct merge because GitHub cannot atomically enforce
    /// all admission facts used by the steward.
    DirectMergeRefused {
        /// Server guarantees missing from the direct-merge path.
        reasons: Vec<DirectMergeRefusal>,
    },
}

/// Classify the producer-provenanced summary emitted by the daemon's exact
/// shadow observer. This intentionally covers only lifecycle routing: normal
/// merge mutations still use [`classify_pr`] with the complete PR policy.
#[must_use]
pub(crate) fn classify_shadow_summary(
    exact_head: bool,
    pending_checks: u64,
    passed_checks: u64,
    failed_checks: u64,
) -> StewardDecision {
    if !exact_head {
        return StewardDecision::NeedsUpdate {
            merge_state: "STALE_HEAD".to_owned(),
        };
    }
    if pending_checks > 0 || (passed_checks == 0 && failed_checks == 0) {
        return StewardDecision::WaitingRequired {
            contexts: vec!["producer-provenanced-required-checks".to_owned()],
        };
    }
    if failed_checks > 0 {
        return StewardDecision::RequiredFailed {
            contexts: vec!["producer-provenanced-required-failure".to_owned()],
        };
    }
    StewardDecision::ArmMergeQueue
}

/// Atomic server guarantees required before the steward may merge directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectMergeRefusal {
    /// The observed check set does not prove every intended check materialized.
    RequiredCheckMaterializationNotAuthoritative,
    /// GitHub's REST merge guard binds the head SHA but not the validated base.
    ValidatedBaseRevisionNotAtomic,
}

/// Classify one PR without performing a mutation.
#[must_use]
pub fn classify_pr(
    pr: &StewardPullRequest,
    policy: &StewardPolicy,
    transient_attempts: &BTreeMap<u64, u32>,
) -> StewardDecision {
    if let Some(decision) = classify_provenance(pr, policy) {
        return decision;
    }
    if has_pr_label(pr, &policy.opt_out_label) {
        return StewardDecision::OptedOut;
    }
    if let Some(decision) = classify_management(pr, policy) {
        return decision;
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
    if policy.required_checks.is_empty() {
        return if policy.merge_queue {
            StewardDecision::WaitingRequired {
                contexts: vec!["authoritative-required-check-policy".to_owned()],
            }
        } else {
            StewardDecision::DirectMergeRefused {
                reasons: vec![
                    DirectMergeRefusal::RequiredCheckMaterializationNotAuthoritative,
                    DirectMergeRefusal::ValidatedBaseRevisionNotAtomic,
                ],
            }
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
        StewardDecision::DirectMergeRefused {
            reasons: vec![DirectMergeRefusal::ValidatedBaseRevisionNotAtomic],
        }
    }
}

fn has_pr_label(pr: &StewardPullRequest, expected: &str) -> bool {
    pr.labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(expected))
}

fn classify_provenance(pr: &StewardPullRequest, policy: &StewardPolicy) -> Option<StewardDecision> {
    let labels = matching_provenance_blockers(pr, policy);
    (!labels.is_empty()).then_some(StewardDecision::ProvenanceBlocked { labels })
}

/// Configured provenance blockers currently attached to a PR.
///
/// GitHub label names are case-insensitive. Returning configured spellings
/// keeps JSON decisions stable even if a hostile or legacy client varies case.
#[must_use]
pub fn matching_provenance_blockers(
    pr: &StewardPullRequest,
    policy: &StewardPolicy,
) -> Vec<String> {
    policy
        .provenance_blocking_labels
        .iter()
        .filter(|blocker| {
            pr.labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case(blocker))
        })
        .cloned()
        .collect()
}

fn classify_management(pr: &StewardPullRequest, policy: &StewardPolicy) -> Option<StewardDecision> {
    let managed_label = policy.managed_label.as_deref()?;
    let labelled = pr
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(managed_label));
    if !labelled {
        return Some(StewardDecision::Unmanaged);
    }
    (!has_successful_status(pr, &policy.handoff_context)).then_some(StewardDecision::HandoffMissing)
}

/// Whether the current immutable head carries a successful status context.
#[must_use]
pub fn has_successful_status(pr: &StewardPullRequest, context: &str) -> bool {
    pr.checks.iter().any(|check| {
        check.name.eq_ignore_ascii_case(context)
            && check.source == StewardCheckSource::StatusContext
            && check.status.eq_ignore_ascii_case("COMPLETED")
            && check
                .conclusion
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("SUCCESS"))
    })
}

fn selected_checks<'a>(
    pr: &'a StewardPullRequest,
    policy: &'a StewardPolicy,
) -> Vec<(String, Option<&'a StewardCheck>)> {
    policy
        .required_checks
        .iter()
        .map(|required| {
            (
                required.label(),
                selected_required_check(&pr.checks, required),
            )
        })
        .collect()
}

pub(crate) fn selected_required_check<'a>(
    checks: &'a [StewardCheck],
    required: &RequiredCheck,
) -> Option<&'a StewardCheck> {
    checks
        .iter()
        .filter(|check| {
            check.name.eq_ignore_ascii_case(&required.context)
                && required
                    .app_id
                    .is_none_or(|app_id| check.app_id == Some(app_id))
        })
        .max_by_key(|check| check_recency(check))
}

fn check_recency(check: &StewardCheck) -> (bool, &str, bool) {
    (
        check.observed_at.is_none() && !check.status.eq_ignore_ascii_case("COMPLETED"),
        check.observed_at.as_deref().unwrap_or_default(),
        check.status.eq_ignore_ascii_case("COMPLETED"),
    )
}

pub(crate) fn is_transient_conclusion(conclusion: &str) -> bool {
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
    /// Legacy non-authorizing value retained for backwards-compatible reports.
    ///
    /// Same-head runs can differ in inputs that GitHub's run observation does
    /// not expose, so this reason must never authorize cancellation.
    DuplicateImmutableHead,
    /// Pull-request branch has advanced to a different immutable head.
    SupersededPullRequestHead,
    /// Merge queue has materialized a newer speculative head for the same PR.
    SupersededMergeGroupHead,
    /// An advisory PR workflow owns shared preamble capacity.
    AdvisoryPreambleCapacityTheft,
    /// Legacy ledger value retained so interrupted pre-release state can be
    /// read and safely rejected; this reason never authorizes a cancellation.
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
/// Only explicitly advisory PR workflows with a running shared-preamble job
/// are eligible. Required workflows, pushes, merge groups, unknown
/// workflows/jobs, and any run whose expensive `pulp-build` leg was already
/// observed as started are never selected.
#[must_use]
pub fn plan_capacity_preemptions(
    runs: &[StewardRun],
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
            let reason = preemption_reason(run, opted_out_pull_requests, policy)?;
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
    policy.workflow_is_advisory(workflow)
}

/// Revalidate that a live run still satisfies the exact planned preemption.
#[must_use]
pub fn is_safe_capacity_preemption(
    run: &StewardRun,
    opted_out_pull_requests: &BTreeSet<u64>,
    policy: &CapacityPreemptionPolicy,
    expected_reason: RunCancellationReason,
) -> bool {
    preemption_reason(run, opted_out_pull_requests, policy) == Some(expected_reason)
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

/// Plan queued-run supersedence cleanup.
///
/// Runs observed in progress are never planned; a queued run that advances
/// before cancellation is re-read and left alone. GitHub may still reject a
/// cancellation when its run state changes after that final observation.
/// Push/schedule runs are never touched.
/// The planner only acts when a PR or merge-group run's immutable full SHA is
/// different from the corresponding current authoritative head. Same-head
/// duplicates are deliberately left to GitHub.
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

    reasons
        .into_iter()
        .map(|(run_id, reason)| RunCancellation { run_id, reason })
        .collect()
}

/// Whether a coalescing reason proves immutable-head supersedence strongly
/// enough to authorize GitHub cancellation.
#[must_use]
pub fn coalescing_reason_authorizes(reason: RunCancellationReason) -> bool {
    matches!(
        reason,
        RunCancellationReason::SupersededPullRequestHead
            | RunCancellationReason::SupersededMergeGroupHead
    )
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
    crate::merge_queue_liveness::merge_group_pr(branch)
}

#[cfg(test)]
mod tests;
