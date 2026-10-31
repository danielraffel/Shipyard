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

use super::{
    GitHubActions, MutationControl, ObservedPr, PrReport, RepoObservation, StewardDecision,
};
use crate::auto_arm::{
    ArmSkip, ArmVerdict, arm_mutation_args, arm_mutation_args_at_head, arm_response_accepted,
    decide_from_queue_state_with_environment, first_graphql_error, is_arm_guard_refusal,
    preselect_backstop_candidate,
};
use crate::merge_queue_control::{MergeQueueMutationPreflight, preflight_mutation_authority};
use crate::pr_queue_state::{PR_QUEUE_STATE_QUERY, explain_pr_queue_state};

/// What the backstop did for this repository on this pass.
#[derive(Clone, Debug, Serialize)]
pub(super) struct NativeArmRepoStatus {
    /// Why the pass acted, or why it did nothing.
    pub(super) policy: String,
    /// One entry per pull request the pass considered a candidate.
    pub(super) results: Vec<NativeArmResult>,
}

impl Default for NativeArmRepoStatus {
    /// The state for a repository the pass never reached, said as such.
    ///
    /// An empty policy string would read as "nothing to report", which is the
    /// one thing it must not mean: a repository whose settings could not be
    /// read was not evaluated, and that is not the same as having no
    /// candidates.
    fn default() -> Self {
        Self {
            policy: "not evaluated: the repository was not readable on this pass".to_owned(),
            results: Vec::new(),
        }
    }
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

/// Why arming may not proceed on this pass.
#[derive(Debug)]
pub(super) struct ArmAuthorityRefusal {
    /// Reported as the repository's backstop policy.
    pub(super) reason: String,
    /// A real failure, as opposed to a deliberate HOLD.
    pub(super) failed: bool,
}

/// Take the same central HOLD, machine authority and control lock every other
/// merge-queue mutation takes, and keep it for the arming that follows.
///
/// Arming changes what the queue will admit, so an operator's HOLD, a host
/// other than `[merge_queue].mutation_machine`, or an uncertain earlier
/// mutation on this repository and base all stop it.
pub(super) fn arming_authority(
    control: &MutationControl,
    repo: &str,
    base: &str,
) -> Result<MergeQueueMutationPreflight, ArmAuthorityRefusal> {
    match crate::merge_queue_control::hold_status(&control.state_dir) {
        Ok(Some(_)) => {
            return Err(ArmAuthorityRefusal {
                reason: format!(
                    "held: merge-queue mutations are centrally held by {}, so nothing was armed",
                    control
                        .state_dir
                        .join(crate::merge_queue_control::HOLD_FILE)
                        .display()
                ),
                failed: false,
            });
        }
        Ok(None) => {}
        Err(reason) => {
            return Err(ArmAuthorityRefusal {
                reason,
                failed: true,
            });
        }
    }
    preflight_mutation_authority(
        &control.state_dir,
        &control.cwd,
        control.mode,
        &control.global_dir,
        repo,
        base,
    )
    .map_err(|reason| ArmAuthorityRefusal {
        reason: format!("declined: no merge-queue mutation authority: {reason}"),
        failed: true,
    })
}

/// Run the backstop over one repository's observation.
///
/// Returns the status plus whether anything genuinely failed. A refusal is not
/// a failure: the pass reports it and moves on. With `apply`, `authorize` runs
/// once there is a candidate, and what it returns is held until the pass ends.
pub(super) fn apply_native_arm_backstop<A>(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr_reports: &[PrReport],
    opt_out_label: &str,
    apply: bool,
    authorize: impl FnOnce() -> Result<A, ArmAuthorityRefusal>,
) -> (NativeArmRepoStatus, bool) {
    if !observation.merge_queue {
        // Without a merge queue an armed request is not consumed on enqueue:
        // it survives a later push by anyone with write access and then
        // merges a head nobody judged. A no-queue base is landed only by the
        // steward's own exact-head merge, if at all.
        return (
            NativeArmRepoStatus {
                policy: format!(
                    "not applicable: {} has no merge queue on {}, and native auto-merge is \
                     never armed on a no-queue base",
                    observation.repo, observation.base
                ),
                results: Vec::new(),
            },
            false,
        );
    }
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

    let _authority = if apply {
        match authorize() {
            Ok(authority) => Some(authority),
            Err(refusal) => {
                return (
                    NativeArmRepoStatus {
                        policy: refusal.reason,
                        results: Vec::new(),
                    },
                    refusal.failed,
                );
            }
        }
    } else {
        None
    };
    let mut unhealthy = false;
    let mut results = Vec::new();
    for pr in candidates {
        let result = consider_candidate(actions, &observation.repo, &observation.base, pr, apply);
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
    base: &str,
    pr: &ObservedPr,
    apply: bool,
) -> NativeArmResult {
    let number = pr.fact.number;
    let head_sha = pr.fact.head_sha.clone();
    let queue = match read_queue_state(actions, repo, number) {
        Ok(value) => value,
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

    let report = explain_pr_queue_state(&queue);
    let run_gh = |args: &[String]| actions.run_gh(args).map_err(|error| error.to_string());
    // A same-head `failed_checks` ejection is re-armed only on a classified
    // interruption or environment failure, never a real red, and only at the
    // exact head the classification read.
    let environment =
        match same_head_verdict(&run_gh, repo, base, &report, &queue, number, &head_sha) {
            Ok(verdict) => verdict,
            Err(result) => return *result,
        };
    let same_head = environment.as_ref().is_some_and(|verdict| verdict.allowed);
    let excuse = environment
        .as_ref()
        .is_some_and(crate::environment_requeue::EnvironmentRequeue::is_allowed_interruption);
    let verdict = match decide_from_queue_state_with_environment(
        &report.state,
        pr.fact.draft,
        environment.as_ref(),
    ) {
        ArmVerdict::Arm => {
            // The same strict read the steward's enqueue and stale-arm sweep
            // use: an absent or unreadable base policy refuses the arm rather
            // than defaulting to "approval not required".
            match crate::head_approval::read_policy_required(&run_gh, repo, base).and_then(
                |policy| {
                    crate::head_approval::evaluate_excusing_with_policy(
                        &run_gh, repo, number, &head_sha, &policy, &queue, excuse,
                    )
                },
            ) {
                Ok(gate) if gate.allows() => {
                    match tier_verdict(&run_gh, repo, base, number, &head_sha) {
                        Ok(verdict) => verdict,
                        Err(result) => return *result,
                    }
                }
                Ok(gate) => ArmVerdict::Skip(ArmSkip::HeadNotApproved {
                    detail: gate.explain(),
                }),
                Err(detail) => {
                    return NativeArmResult {
                        number,
                        head_sha,
                        outcome: "skipped".to_owned(),
                        skip: Some(ArmSkip::Unknown {
                            detail: detail.clone(),
                        }),
                        error: Some(format!(
                            "PR #{number} head approval unreadable; refusing to arm blind: {detail}"
                        )),
                    };
                }
            }
        }
        skip @ ArmVerdict::Skip(_) => skip,
    };
    match verdict {
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
            outcome: if same_head {
                "would_rearm_same_head"
            } else {
                "would_arm"
            }
            .to_owned(),
            skip: None,
            error: None,
        },
        ArmVerdict::Arm => arm(actions, number, head_sha, &pr.node_id, same_head),
    }
}

/// [`ArmVerdict::Arm`] unless a required check ran only its deferred test
/// tier on `head_sha` ([`crate::deferred_tier`]); an unreadable tier ends the
/// candidate with its result.
fn tier_verdict(
    run_gh: crate::deferred_tier::RunGh<'_>,
    repo: &str,
    base: &str,
    number: u64,
    head_sha: &str,
) -> Result<ArmVerdict, Box<NativeArmResult>> {
    match crate::deferred_tier::deferred_tier_checks(run_gh, repo, number, base, head_sha) {
        Ok(checks) => Ok(crate::deferred_tier::explain_all(&checks)
            .map_or(ArmVerdict::Arm, |detail| {
                ArmVerdict::Skip(ArmSkip::DeferredTier { detail })
            })),
        Err(detail) => Err(Box::new(NativeArmResult {
            number,
            head_sha: head_sha.to_owned(),
            outcome: "skipped".to_owned(),
            skip: Some(ArmSkip::Unknown {
                detail: detail.clone(),
            }),
            error: Some(format!(
                "PR #{number} test tier unreadable; refusing to arm blind: {detail}"
            )),
        })),
    }
}

/// Issue the arm mutation for one confirmed candidate.
/// For a same-head `failed_checks` ejection, the classifier's verdict on
/// whether this exact head may re-enter the queue
/// ([`crate::environment_requeue::assess`]); `None` for any other state. A
/// moved head or an unreadable policy ends the candidate with its result.
fn same_head_verdict(
    run_gh: crate::environment_requeue::RunGh<'_>,
    repo: &str,
    base: &str,
    report: &crate::pr_queue_state::PrQueueReport,
    queue: &serde_json::Value,
    number: u64,
    head_sha: &str,
) -> Result<Option<crate::environment_requeue::EnvironmentRequeue>, Box<NativeArmResult>> {
    if !crate::environment_requeue::applies(report) {
        return Ok(None);
    }
    let skipped = |detail: String, error: Option<String>| {
        Box::new(NativeArmResult {
            number,
            head_sha: head_sha.to_owned(),
            outcome: "skipped".to_owned(),
            skip: Some(ArmSkip::Unknown { detail }),
            error,
        })
    };
    let queue_head = queue
        .pointer("/data/repository/pullRequest/headRefOid")
        .and_then(serde_json::Value::as_str);
    if queue_head != Some(head_sha) {
        return Err(skipped(
            "the head moved between observation and the queue read".to_owned(),
            None,
        ));
    }
    match crate::environment_requeue::read_opt_in(run_gh, repo, base) {
        Ok(opted_in) => Ok(
            crate::environment_requeue::assess(run_gh, repo, report, opted_in)
                .map(|verdict| armed_since_arrival(verdict, queue, head_sha)),
        ),
        Err(detail) => Err(skipped(
            detail.clone(),
            Some(format!(
                "PR #{number} re-enqueue policy unreadable; refusing to arm blind: {detail}"
            )),
        )),
    }
}

/// An allowed same-head verdict stands only when someone armed *this* head:
/// an `AutoMergeEnabledEvent` at or after the head's own arrival (its own
/// force-push, or its first check suite), read by the steward carrier's
/// [`crate::merge_carrier::head_arm_time`]. Otherwise a head pushed after an
/// earlier head was armed, auto-queued and then starved would be re-armed with
/// nobody having armed it. A window that cannot place the arrival refuses.
fn armed_since_arrival(
    mut verdict: crate::environment_requeue::EnvironmentRequeue,
    queue: &serde_json::Value,
    head_sha: &str,
) -> crate::environment_requeue::EnvironmentRequeue {
    if !verdict.allowed {
        return verdict;
    }
    let refusal = match crate::merge_carrier::head_arm_time(queue, head_sha) {
        Ok(Some(_)) => return verdict,
        Ok(None) => format!(
            "nobody armed head {} after it arrived, so the backstop will not re-arm it",
            head_sha.get(..12).unwrap_or(head_sha)
        ),
        Err(detail) => format!("whether this head was ever armed cannot be read: {detail}"),
    };
    verdict.allowed = false;
    verdict.class = None;
    verdict.reason = format!("{refusal} (the ejection itself was: {})", verdict.reason);
    verdict
}

/// Arm one pull request. `same_head` is a classified same-head re-enqueue:
/// the mutation is bound to `head_sha` with `expectedHeadOid`, so GitHub
/// refuses it if the head moved since the classification.
fn arm(
    actions: &GitHubActions,
    number: u64,
    head_sha: String,
    node_id: &str,
    same_head: bool,
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
    let args = if same_head {
        arm_mutation_args_at_head(node_id, &head_sha)
    } else {
        arm_mutation_args(node_id)
    };
    match actions.run_gh(&args) {
        Ok(raw) if arm_response_accepted(&raw) => NativeArmResult {
            number,
            outcome: if same_head {
                "rearmed_same_head"
            } else {
                "armed"
            }
            .to_owned(),
            head_sha,
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

pub(super) fn read_queue_state(
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
