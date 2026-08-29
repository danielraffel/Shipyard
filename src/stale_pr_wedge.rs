//! Transport-free Pulp stale PR concurrency-wedge classification.

use serde::{Deserialize, Serialize};

use crate::merge_steward::{
    RequiredCheck, StewardJob, StewardPullRequest, StewardRun, is_full_sha,
};

/// Immutable evidence for one stale PR workflow concurrency wedge.
///
/// Cancelling an in-progress run requires binding both workflow runs, the live
/// PR ref/head, and the exact required local macOS job that owns the stale
/// concurrency slot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StalePrRunWedgeCandidate {
    /// Canonical owner/repository slug.
    pub repo: String,
    /// Pull request whose head advanced.
    pub pr_number: u64,
    /// Stale run that may be cancelled.
    pub old_run_id: u64,
    /// Immutable stale run head.
    pub old_head_sha: String,
    /// Exact stale workflow attempt.
    pub old_run_attempt: u64,
    /// New exact-current-head run proving the concurrency wedge.
    pub new_run_id: u64,
    /// Immutable current PR/run head.
    pub new_head_sha: String,
    /// Exact current-head workflow attempt.
    pub new_run_attempt: u64,
    /// Stable workflow database ID shared by both runs.
    pub workflow_id: u64,
    /// Exact workflow display name shared by both runs.
    pub workflow: String,
    /// Exact live PR ref shared by both runs.
    pub head_ref: String,
    /// Required local macOS job currently owned by the stale run.
    pub local_required_job: StewardJob,
}

/// Plan at most one high-confidence stale PR concurrency-wedge cancellation.
///
/// This rule is intentionally Pulp-only and macOS-first. It does not inspect
/// or cancel Linux/Windows work, never selects push or merge-group runs, and
/// preserves every exact-current-head run. Merge-group cleanup remains a
/// separate rule requiring proven absence from a complete live merge queue.
#[must_use]
pub fn plan_stale_pr_run_wedges(
    repo: &str,
    runs: &[StewardRun],
    pull_requests: &[StewardPullRequest],
    required_checks: &[RequiredCheck],
) -> Vec<StalePrRunWedgeCandidate> {
    if !repo.eq_ignore_ascii_case("Generous-Corp/pulp")
        || !required_checks.iter().any(|check| check.context == "macos")
    {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for old in runs {
        if old.event != "pull_request"
            || old.workflow != "Build and Test"
            || !matches!(
                old.status.to_ascii_lowercase().as_str(),
                "queued" | "in_progress"
            )
            || !is_full_sha(&old.head_sha)
        {
            continue;
        }
        let Some(pr_number) = old.pull_request_number else {
            continue;
        };
        let Some(pr) = pull_requests.iter().find(|pr| pr.number == pr_number) else {
            continue;
        };
        if !is_full_sha(&pr.head_sha)
            || old.head_sha.eq_ignore_ascii_case(&pr.head_sha)
            || old.head_branch != pr.head_branch
        {
            continue;
        }
        let mut local_jobs = old.jobs.iter().filter(|job| required_local_macos_job(job));
        let Some(local_required_job) = local_jobs.next() else {
            continue;
        };
        if local_jobs.next().is_some() {
            continue;
        }
        let newer = runs
            .iter()
            .filter(|run| {
                run.id > old.id
                    && run.created_at > old.created_at
                    && run.event == "pull_request"
                    && run.pull_request_number == Some(pr_number)
                    && run.workflow_id == old.workflow_id
                    && run.workflow == old.workflow
                    && run.head_branch == pr.head_branch
                    && run.head_sha.eq_ignore_ascii_case(&pr.head_sha)
                    && run.status.eq_ignore_ascii_case("pending")
                    && run.jobs.is_empty()
            })
            .min_by_key(|run| run.id);
        let Some(newer) = newer else {
            continue;
        };
        candidates.push(StalePrRunWedgeCandidate {
            repo: repo.to_owned(),
            pr_number,
            old_run_id: old.id,
            old_head_sha: old.head_sha.clone(),
            old_run_attempt: old.run_attempt,
            new_run_id: newer.id,
            new_head_sha: newer.head_sha.clone(),
            new_run_attempt: newer.run_attempt,
            workflow_id: old.workflow_id,
            workflow: old.workflow.clone(),
            head_ref: pr.head_branch.clone(),
            local_required_job: local_required_job.clone(),
        });
    }
    candidates.sort_by_key(|candidate| candidate.old_run_id);
    candidates.truncate(1);
    candidates
}

fn required_local_macos_job(job: &StewardJob) -> bool {
    job.id != 0
        && job.name == "macos"
        && matches!(
            job.status.to_ascii_lowercase().as_str(),
            "queued" | "waiting" | "pending" | "requested" | "in_progress"
        )
        && job.conclusion.is_none()
        && [
            "self-hosted",
            "macos",
            "arm64",
            "pulp-build",
            "pulp-build-vm",
            "pulp-build-pr-head",
        ]
        .iter()
        .all(|expected| {
            job.labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case(expected))
        })
}
