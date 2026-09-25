//! Periodic backstop: arm GitHub-native auto-merge on green, unqueued,
//! unarmed pull requests the steward will never enqueue itself.
//!
//! ## Why this lives in the steward
//!
//! The steward is already the repository's periodic merge-on-green reconciler:
//! it iterates every configured repo, is audit-only unless `--apply`, holds a
//! durable ledger, obeys the machine-global mutation HOLD, and already reads
//! every open pull request's draft flag, queue position, armed flag, labels and
//! `mergeStateStatus` in the pass it already makes. `shipyard landing` and
//! `shipyard queue-observe` both assert read-only invariants in code, so
//! neither may mutate; the daemon's GitHub-reading work is subscriber-gated,
//! which is precisely wrong for a backstop that must run when nobody is
//! watching. Extending the one existing mutating reconciler adds a pass, not a
//! surface.
//!
//! ## Why it does not overlap the steward's own enqueue
//!
//! The steward enqueues only pull requests explicitly handed to it (management
//! label plus a current-head handoff receipt). This pass acts *only* on the
//! complement — pull requests whose steward decision is `Unmanaged` or
//! `HandoffMissing`, which the steward will never touch — so the two can never
//! both act on one pull request.
//!
//! That split is also what makes the weaker mutation appropriate. The
//! steward's `enqueuePullRequest` asserts "this exact head is admissible now",
//! which is why it demands an ownership receipt. `enablePullRequestAutoMerge`
//! asserts only "merge this when GitHub says it is ready": GitHub re-checks
//! required checks itself, refuses drafts itself, and drops the request if the
//! head changes. Extending that to unmanaged pull requests grants no authority
//! the repository's own branch protection does not already hold.
//!
//! ## Cost
//!
//! Candidate selection is free: it reads facts the observation already
//! fetched. Only candidates cost a round trip, because a backlog row carries no
//! timeline and so cannot tell a never-armed pull request from one the queue
//! ejected on this exact head — and re-arming the latter re-enqueues it, which
//! under `ALLGREEN` grouping fails every batch-mate with it.

use serde::Serialize;

use super::{GitHubActions, ObservedPr, PrReport, RepoObservation, StewardDecision};
use crate::auto_arm::{
    ArmSkip, ArmVerdict, arm_mutation_args, arm_response_accepted, decide_from_queue_state,
    first_graphql_error, is_arm_guard_refusal, preselect_backstop_candidate,
};
use crate::pr_queue_state::{PR_QUEUE_STATE_QUERY, explain_pr_queue_state};

/// What the backstop did for this repository on this pass.
#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct NativeArmRepoStatus {
    /// Why the pass acted, or why it did nothing.
    pub(super) policy: String,
    /// One entry per pull request the pass considered a candidate.
    pub(super) results: Vec<NativeArmResult>,
}

/// One considered pull request.
#[derive(Clone, Debug, Serialize)]
pub(super) struct NativeArmResult {
    /// Pull-request number.
    pub(super) number: u64,
    /// Immutable head the decision was made against.
    pub(super) head_sha: String,
    /// `armed`, `would_arm` (audit mode), or `skipped`.
    pub(super) outcome: String,
    /// Why it was skipped, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) skip: Option<ArmSkip>,
    /// A real failure, as opposed to a refusal that is agreement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

/// Whether the steward itself will never act on this pull request, making it
/// the backstop's business rather than the enqueue path's.
///
/// Every other decision means the managed path owns the pull request — either
/// it is acting now or it is deliberately waiting — so the backstop stays out.
fn steward_declines_ownership(decision: &StewardDecision) -> bool {
    matches!(
        decision,
        StewardDecision::Unmanaged | StewardDecision::HandoffMissing
    )
}

/// Run the backstop over one repository's observation.
///
/// Returns the status plus whether anything genuinely failed. A refusal is not
/// a failure: the pass reports it and moves on.
pub(super) fn apply_native_arm_backstop(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr_reports: &[PrReport],
    opt_out_label: &str,
    apply: bool,
) -> (NativeArmRepoStatus, bool) {
    if !observation.allow_auto_merge {
        return (
            NativeArmRepoStatus {
                policy: format!(
                    "declined: {} does not allow native auto-merge, so nothing can be armed",
                    observation.repo
                ),
                results: Vec::new(),
            },
            false,
        );
    }

    let candidates = select_candidates(observation, pr_reports, opt_out_label);
    if candidates.is_empty() {
        return (
            NativeArmRepoStatus {
                policy: format!(
                    "no candidates: of {} open pull request(s) on {}, none is a green, unarmed, \
                     unqueued pull request the steward declines to own",
                    observation.prs.len(),
                    observation.base
                ),
                results: Vec::new(),
            },
            false,
        );
    }

    let mut unhealthy = false;
    let mut results = Vec::new();
    for pr in candidates {
        let result = consider_candidate(actions, &observation.repo, pr, apply);
        unhealthy |= result.error.is_some();
        results.push(result);
    }
    (
        NativeArmRepoStatus {
            policy: format!(
                "{} candidate(s) confirmed against their merge-queue state before {}",
                results.len(),
                if apply {
                    "arming"
                } else {
                    "reporting (audit mode; pass --apply to arm)"
                }
            ),
            results,
        },
        unhealthy,
    )
}

/// Pull requests worth a confirming read, from facts already in hand.
fn select_candidates<'a>(
    observation: &'a RepoObservation,
    pr_reports: &[PrReport],
    opt_out_label: &str,
) -> Vec<&'a ObservedPr> {
    observation
        .prs
        .iter()
        .filter(|pr| {
            pr_reports
                .iter()
                .find(|report| report.number == pr.fact.number)
                .is_some_and(|report| steward_declines_ownership(&report.decision))
        })
        .filter(|pr| {
            preselect_backstop_candidate(&pr.fact, observation.allow_auto_merge, opt_out_label)
                .arms()
        })
        .collect()
}

/// Confirm one candidate authoritatively, then arm it when `apply`.
fn consider_candidate(
    actions: &GitHubActions,
    repo: &str,
    pr: &ObservedPr,
    apply: bool,
) -> NativeArmResult {
    let number = pr.fact.number;
    let head_sha = pr.fact.head_sha.clone();
    let state = match read_queue_state(actions, repo, number) {
        Ok(value) => explain_pr_queue_state(&value).state,
        Err(detail) => {
            // An unreadable state is not an unarmed state. This is reported as
            // an error rather than a skip: the pass could not do its job.
            return NativeArmResult {
                number,
                head_sha,
                outcome: "skipped".to_owned(),
                skip: Some(ArmSkip::Unknown {
                    detail: detail.clone(),
                }),
                error: Some(format!(
                    "PR #{number} merge-queue state unreadable; refusing to arm blind: {detail}"
                )),
            };
        }
    };

    match decide_from_queue_state(&state, pr.fact.draft) {
        ArmVerdict::Skip(skip) => NativeArmResult {
            number,
            head_sha,
            outcome: "skipped".to_owned(),
            skip: Some(skip),
            error: None,
        },
        ArmVerdict::Arm if !apply => NativeArmResult {
            number,
            head_sha,
            outcome: "would_arm".to_owned(),
            skip: None,
            error: None,
        },
        ArmVerdict::Arm => arm(actions, number, head_sha, &pr.node_id),
    }
}

/// Issue the arm mutation for one confirmed candidate.
fn arm(
    actions: &GitHubActions,
    number: u64,
    head_sha: String,
    node_id: &str,
) -> NativeArmResult {
    if node_id.is_empty() {
        return NativeArmResult {
            number,
            head_sha,
            outcome: "skipped".to_owned(),
            skip: Some(ArmSkip::Unknown {
                detail: "pull request node ID unavailable".to_owned(),
            }),
            error: None,
        };
    }
    // Deliberately the ordinary `run_gh`, not the internal-queue-mutation
    // path: the internal marker makes the `ghapp` arm guard step aside, and
    // this request is not bound to a validated head, so the guard's second
    // opinion is wanted. Its refusals are honoured, never overridden, and
    // Shipyard never sets `GHAPP_ALLOW_QUEUE_REARM`.
    match actions.run_gh(&arm_mutation_args(node_id)) {
        Ok(raw) if arm_response_accepted(&raw) => NativeArmResult {
            number,
            head_sha,
            outcome: "armed".to_owned(),
            skip: None,
            error: None,
        },
        Ok(raw) => NativeArmResult {
            number,
            head_sha,
            outcome: "skipped".to_owned(),
            skip: None,
            error: Some(format!(
                "PR #{number} arm request returned no armed pull request: {}",
                first_graphql_error(&raw).unwrap_or_else(|| "no errors reported".to_owned())
            )),
        },
        Err(error) => {
            let detail = error.to_string();
            if is_arm_guard_refusal(&detail) {
                // The guard refuses exactly the states this pass refuses, so a
                // refusal is agreement. Report it; never retry, never override.
                NativeArmResult {
                    number,
                    head_sha,
                    outcome: "skipped".to_owned(),
                    skip: Some(ArmSkip::Unknown {
                        detail: format!("queue-arm guard declined it: {detail}"),
                    }),
                    error: None,
                }
            } else {
                NativeArmResult {
                    number,
                    head_sha,
                    outcome: "skipped".to_owned(),
                    skip: None,
                    error: Some(format!("PR #{number} could not be armed: {detail}")),
                }
            }
        }
    }
}

fn read_queue_state(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
) -> Result<serde_json::Value, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("repo `{repo}` is not OWNER/REPO"))?;
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={PR_QUEUE_STATE_QUERY}"),
            "-F".to_owned(),
            format!("owner={owner}"),
            "-F".to_owned(),
            format!("name={name}"),
            "-F".to_owned(),
            format!("number={pr}"),
        ])
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&raw).map_err(|error| format!("malformed GraphQL JSON: {error}"))
}

#[cfg(test)]
mod tests;
