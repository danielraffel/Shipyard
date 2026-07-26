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
    /// Immutable run head SHA.
    pub head_sha: String,
    /// Head branch, including merge-group branches.
    pub head_branch: String,
    /// GitHub run state.
    pub status: String,
    /// GitHub event (`pull_request`, `merge_group`, etc.).
    pub event: String,
    /// Creation timestamp, used only for deterministic retention.
    pub created_at: String,
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
}

/// One conservative queued-run cancellation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunCancellation {
    /// Workflow run ID.
    pub run_id: u64,
    /// Cancellation reason.
    pub reason: RunCancellationReason,
}

/// Plan queued-run coalescing.
///
/// In-progress work is never cancelled. Push/schedule runs are never touched.
/// The planner only acts on immutable, full-SHA PR/merge-group observations.
#[must_use]
pub fn plan_run_coalescing(
    runs: &[StewardRun],
    current_pr_heads: &BTreeMap<String, String>,
    current_merge_group_heads: &BTreeMap<u64, String>,
) -> Vec<RunCancellation> {
    let mut reasons = BTreeMap::<u64, RunCancellationReason>::new();
    for run in runs {
        if !run.status.eq_ignore_ascii_case("queued") || !is_full_sha(&run.head_sha) {
            continue;
        }
        if run.event == "pull_request"
            && let Some(current) = current_pr_heads.get(&run.head_branch)
            && is_full_sha(current)
            && !current.eq_ignore_ascii_case(&run.head_sha)
        {
            reasons.insert(run.id, RunCancellationReason::SupersededPullRequestHead);
        } else if run.event == "merge_group"
            && let Some(pr) = merge_group_pr(&run.head_branch)
            && let Some(current) = current_merge_group_heads.get(&pr)
            && is_full_sha(current)
            && !current.eq_ignore_ascii_case(&run.head_sha)
        {
            reasons.insert(run.id, RunCancellationReason::SupersededMergeGroupHead);
        }
    }

    let mut groups = BTreeMap::<(u64, String), Vec<&StewardRun>>::new();
    for run in runs {
        if matches!(run.event.as_str(), "pull_request" | "merge_group")
            && is_full_sha(&run.head_sha)
        {
            groups
                .entry((run.workflow_id, run.head_sha.to_ascii_lowercase()))
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
        for duplicate in group.iter().skip(1) {
            if duplicate.status.eq_ignore_ascii_case("queued") {
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
    fn coalesces_only_queued_duplicate_and_superseded_pr_runs() {
        let runs = vec![
            StewardRun {
                id: 1,
                workflow_id: 8,
                head_sha: sha('a'),
                head_branch: "feature".to_owned(),
                status: "in_progress".to_owned(),
                event: "pull_request".to_owned(),
                created_at: "2026-01-01T00:00:00Z".to_owned(),
            },
            StewardRun {
                id: 2,
                workflow_id: 8,
                head_sha: sha('a'),
                head_branch: "feature".to_owned(),
                status: "queued".to_owned(),
                event: "pull_request".to_owned(),
                created_at: "2026-01-01T00:01:00Z".to_owned(),
            },
            StewardRun {
                id: 3,
                workflow_id: 9,
                head_sha: sha('b'),
                head_branch: "feature".to_owned(),
                status: "queued".to_owned(),
                event: "pull_request".to_owned(),
                created_at: "2026-01-01T00:02:00Z".to_owned(),
            },
        ];
        let plan = plan_run_coalescing(
            &runs,
            &BTreeMap::from([("feature".to_owned(), sha('a'))]),
            &BTreeMap::new(),
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
    fn never_cancels_in_progress_or_non_pr_runs() {
        let runs = vec![
            StewardRun {
                id: 1,
                workflow_id: 1,
                head_sha: sha('a'),
                head_branch: "feature".to_owned(),
                status: "in_progress".to_owned(),
                event: "pull_request".to_owned(),
                created_at: String::new(),
            },
            StewardRun {
                id: 2,
                workflow_id: 2,
                head_sha: sha('b'),
                head_branch: "main".to_owned(),
                status: "queued".to_owned(),
                event: "push".to_owned(),
                created_at: String::new(),
            },
        ];
        assert!(
            plan_run_coalescing(
                &runs,
                &BTreeMap::from([("feature".to_owned(), sha('c'))]),
                &BTreeMap::new()
            )
            .is_empty()
        );
    }
}
