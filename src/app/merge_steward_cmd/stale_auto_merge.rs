//! Disarm a native auto-merge request whose current head nobody approved.
//!
//! GitHub drops an armed auto-merge request on a push only when the pusher
//! lacks write access. Agents and the steward's own app have write access, so
//! a request armed for one head survives every later push, and once the new
//! head's checks go green GitHub enqueues (or merges) it itself: past the
//! steward, past the `ghapp` arm guard, and past exact-head approval.
//!
//! The steward is the single admission writer for its base, so on every
//! `--apply` pass it sweeps every open pull request on that base, owned or
//! not, and disarms any armed request whose current head the exact-head
//! approval gate does not admit. A head the gate admits is left armed: the
//! steward would enqueue that head itself, and the `--arm-unqueued` backstop
//! arms exactly such heads, so disarming them would only make the two flap.
//! An approval that cannot be read is treated as missing: an unreadable
//! approval must never leave a live arm. A dry run reports what it would
//! disarm and changes nothing.
//!
//! Owned pull requests on a server-enforced (no-queue) base are disarmed
//! unconditionally by [`super::exact_head_merge::disarm_owned_auto_merge`];
//! this sweep covers everything else on both admission paths.

use super::{
    GitHubActions, MutationApplyContext, ObservedPr, RepoObservation, StewardDecision,
    StewardLedger, StewardPolicy, Value, acquire_pr_mutation_guard,
    exact_head_merge::{disarm_under_guard, owns_for_disarm},
    pr_mutations::read_head_gate_for,
};

/// The ledger action recorded for each disarm.
const AUDIT_ACTION: &str = "disarm_stale_auto_merge";

/// Whether the sweep judges `pr` on this pass.
pub(super) fn applies(policy: &StewardPolicy, pr: &ObservedPr, decision: &StewardDecision) -> bool {
    let server_enforced = policy.server_enforced_exact_head_merge();
    pr.fact.auto_merge_active
        && (policy.merge_queue || server_enforced)
        // Disarmed unconditionally, approved or not, by the no-queue path.
        && !(server_enforced && owns_for_disarm(decision))
}

/// What the live read says about `pr`'s armed request.
#[derive(Debug, Eq, PartialEq)]
enum Judgement {
    /// The current head is admitted; leave the request armed.
    Keep,
    /// The live read shows no request armed any more.
    NotArmed,
    /// Disarm, for this reason.
    Stale(String),
}

fn judge(actions: &GitHubActions, observation: &RepoObservation, pr: &ObservedPr) -> Judgement {
    let (gate, _events, queue) = match read_head_gate_for(actions, observation, pr) {
        Ok(read) => read,
        Err(error) => return Judgement::Stale(format!("approval unreadable: {error}")),
    };
    let live = queue.pointer("/data/repository/pullRequest");
    if live
        .and_then(|pull| pull.get("autoMergeRequest"))
        .is_some_and(Value::is_null)
    {
        return Judgement::NotArmed;
    }
    let head = &pr.fact.head_sha;
    // The gate judged the census head; an armed request follows the live one.
    if !live
        .and_then(|pull| pull.get("headRefOid"))
        .and_then(Value::as_str)
        .is_some_and(|live| live.eq_ignore_ascii_case(head))
    {
        return Judgement::Stale("head moved since the census".to_owned());
    }
    if gate.allows() {
        Judgement::Keep
    } else {
        Judgement::Stale(gate.explain())
    }
}

/// Judge `pr`'s armed request and, under `--apply`, disarm it when stale.
///
/// Mutations, as the per-PR mutation string:
/// * `disarmed_stale_auto_merge:<head>`: disarmed under the mutation guard.
/// * `would_disarm_stale_auto_merge:<head>`: a dry run found it stale.
pub(super) fn sweep(
    actions: &GitHubActions,
    observation: &RepoObservation,
    context: Option<&MutationApplyContext<'_>>,
    apply: bool,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let head = &pr.fact.head_sha;
    let reason = match judge(actions, observation, pr) {
        Judgement::Keep | Judgement::NotArmed => return (None, None),
        Judgement::Stale(reason) => reason,
    };
    if !apply {
        return (Some(format!("would_disarm_stale_auto_merge:{head}")), None);
    }
    let Some(context) = context else {
        return (
            None,
            Some("stale auto-merge disarm requires mutation control".to_owned()),
        );
    };
    let guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        pr,
        "runner steward disarm stale auto-merge on an unapproved head",
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };
    let (outcome, mutation, error) = match disarm_under_guard(context, pr, ledger, AUDIT_ACTION) {
        Ok(()) => (
            "stale_auto_merge_disarmed",
            Some(format!("disarmed_stale_auto_merge:{head}")),
            None,
        ),
        Err(error) => (
            "disarm_failed",
            None,
            Some(format!("{error} (disarming because {reason})")),
        ),
    };
    match (guard.finish(outcome), error) {
        (Ok(()), error) => (mutation, error),
        (Err(audit), None) => (mutation, Some(format!("mutation audit failed: {audit}"))),
        (Err(audit), Some(error)) => (
            mutation,
            Some(format!("{error}; mutation audit also failed: {audit}")),
        ),
    }
}
