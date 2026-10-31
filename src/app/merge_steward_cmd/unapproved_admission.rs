//! Withdraw admission GitHub granted a head nobody approved.
//!
//! GitHub drops an armed auto-merge request on a push only when the pusher
//! lacks write access. Agents and the steward's own app have write access, so
//! a request armed for one head survives every later push. Once the new head's
//! checks go green GitHub enqueues it (on a queue base) or merges it (on a base
//! without one) itself: past the steward, past the `ghapp` arm guard, and past
//! exact-head approval. On enqueue GitHub consumes the request, so a queued
//! entry carries no arm left to disarm.
//!
//! The steward is the single admission writer for its base, so on every pass
//! it sweeps every open pull request on that base, owned or not, whatever the
//! base's merge path, and withdraws what the exact-head approval gate does not
//! admit:
//!
//! * an armed request on an unadmitted head is disabled
//!   (`disarmed_stale_auto_merge:<head>`);
//! * a merge-queue entry whose head is unadmitted is removed
//!   (`dequeued_unapproved:<head>`).
//!
//! A head the gate admits (`Approved`, or `NotRequired` where the base does not
//! require approval) keeps its arm and its queue entry: the steward would
//! enqueue that head itself, and the `--arm-unqueued` backstop arms exactly
//! such heads. An approval that cannot be read, or a live head that moved
//! since the census, counts as unadmitted: an unreadable approval must never
//! leave a live arm or entry. A dry run reports `would_disarm_stale_auto_merge`
//! and `would_dequeue_unapproved` and changes nothing.
//!
//! Both mutations run under the PR mutation guard on Shipyard's internal
//! queue-mutation path, whose one-shot `internal-removal` nonce the `ghapp`
//! `queue-removal-guard` accepts as an audited Shipyard removal.
//!
//! An armed request on an owned pull request is disarmed unconditionally,
//! approved or not, by [`super::exact_head_merge::disarm_owned_auto_merge`]:
//! the steward admits owned heads itself.

use super::{
    GitHubActions, MutationApplyContext, ObservedPr, RepoObservation, StewardDecision,
    StewardLedger, Value, acquire_pr_mutation_guard,
    exact_head_merge::{disarm_under_guard, owns_for_disarm},
    pr_mutations::{evaluate_head_gate, read_approval_policy, read_pr_queue_state},
    record_audit,
};
use crate::auto_arm::first_graphql_error;
use crate::head_approval::ApprovalPolicy;

/// The ledger action recorded for each disarm.
const DISARM_AUDIT: &str = "disarm_stale_auto_merge";
/// The ledger action recorded for each dequeue.
const DEQUEUE_AUDIT: &str = "dequeue_unapproved_queue_entry";

/// GraphQL document that removes a pull request from its merge queue.
const DEQUEUE_MUTATION: &str =
    "mutation($id:ID!){dequeuePullRequest(input:{id:$id}){mergeQueueEntry{id}}}";

/// The reviewer policy, read at most once per pass.
#[derive(Default)]
pub(super) struct PolicyCache(Option<Result<ApprovalPolicy, String>>);

impl PolicyCache {
    fn get(
        &mut self,
        actions: &GitHubActions,
        observation: &RepoObservation,
    ) -> &Result<ApprovalPolicy, String> {
        self.0
            .get_or_insert_with(|| read_approval_policy(actions, observation))
    }
}

/// Whether the sweep judges `pr` on this pass, from census facts.
pub(super) fn applies(pr: &ObservedPr, decision: &StewardDecision) -> bool {
    pr.fact.queue_position.is_some() || (pr.fact.auto_merge_active && !owns_for_disarm(decision))
}

/// What the sweep withdraws from `pr`.
#[derive(Debug, Default, Eq, PartialEq)]
struct Withdrawal {
    disarm: bool,
    dequeue: bool,
    /// Why the head is not admitted.
    reason: String,
}

fn judge(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
    owned: bool,
    policy: &mut PolicyCache,
) -> Withdrawal {
    let census_armed = pr.fact.auto_merge_active && !owned;
    let census_queued = pr.fact.queue_position.is_some();
    let queue = match read_pr_queue_state(actions, observation, pr.fact.number) {
        Ok(queue) => queue,
        // Nothing live to go on: withdraw what the census saw.
        Err(error) => {
            return Withdrawal {
                disarm: census_armed,
                dequeue: census_queued,
                reason: error,
            };
        }
    };
    let live = queue.pointer("/data/repository/pullRequest");
    let armed = !owned
        && live
            .and_then(|pull| pull.get("autoMergeRequest"))
            .is_none_or(|request| !request.is_null());
    let queued = live
        .and_then(|pull| pull.get("isInMergeQueue"))
        .and_then(Value::as_bool)
        .unwrap_or(census_queued);
    if !(armed || queued) {
        return Withdrawal::default();
    }
    let head = &pr.fact.head_sha;
    // The gate judges the census head; the arm and the entry follow the live one.
    let reason = if live
        .and_then(|pull| pull.get("headRefOid"))
        .and_then(Value::as_str)
        .is_some_and(|live| live.eq_ignore_ascii_case(head))
    {
        match policy.get(actions, observation) {
            Err(error) => Some(format!("approval unreadable: {error}")),
            Ok(approval_policy) => {
                match evaluate_head_gate(actions, observation, pr, approval_policy, &queue) {
                    Ok((gate, _events)) if gate.allows() => None,
                    Ok((gate, _events)) => Some(gate.explain()),
                    Err(error) => Some(format!("approval unreadable: {error}")),
                }
            }
        }
    } else {
        Some("head moved since the census".to_owned())
    };
    reason.map_or_else(Withdrawal::default, |reason| Withdrawal {
        disarm: armed,
        dequeue: queued,
        reason,
    })
}

/// Remove `pr` from its merge queue under an already-held guard.
fn dequeue_under_guard(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> Result<(), String> {
    if pr.node_id.is_empty() {
        return Err(format!("PR #{} node ID unavailable", pr.fact.number));
    }
    let args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={DEQUEUE_MUTATION}"),
        "-F".to_owned(),
        format!("id={}", pr.node_id),
    ];
    let raw = context
        .actions
        .run_gh_internal_queue_mutation(&args)
        .map_err(|error| format!("PR #{} could not be dequeued: {error}", pr.fact.number))?;
    let accepted = serde_json::from_str::<Value>(&raw).is_ok_and(|value| {
        value.get("errors").is_none()
            && value
                .pointer("/data/dequeuePullRequest")
                .is_some_and(|result| !result.is_null())
    });
    if !accepted {
        return Err(format!(
            "PR #{} dequeue was not accepted: {}",
            pr.fact.number,
            first_graphql_error(&raw).unwrap_or_else(|| "no errors reported".to_owned())
        ));
    }
    record_audit(
        ledger,
        &context.observation.repo,
        &format!("pr:{}:{}", pr.fact.number, pr.fact.head_sha),
        DEQUEUE_AUDIT,
    );
    Ok(())
}

/// Judge `pr` and, under `--apply`, withdraw its arm and queue entry when its
/// head is not admitted.
///
/// Mutations, comma-joined as the per-PR mutation string:
/// * `disarmed_stale_auto_merge:<head>` / `would_disarm_stale_auto_merge:<head>`
/// * `dequeued_unapproved:<head>` / `would_dequeue_unapproved:<head>`
#[allow(clippy::too_many_arguments)] // The pass's read inputs, its write context and its caches.
pub(super) fn sweep(
    actions: &GitHubActions,
    observation: &RepoObservation,
    context: Option<&MutationApplyContext<'_>>,
    apply: bool,
    pr: &ObservedPr,
    decision: &StewardDecision,
    policy: &mut PolicyCache,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let head = &pr.fact.head_sha;
    let withdrawal = judge(actions, observation, pr, owns_for_disarm(decision), policy);
    if !(withdrawal.disarm || withdrawal.dequeue) {
        return (None, None);
    }
    if !apply {
        let planned = [
            withdrawal
                .disarm
                .then(|| format!("would_disarm_stale_auto_merge:{head}")),
            withdrawal
                .dequeue
                .then(|| format!("would_dequeue_unapproved:{head}")),
        ];
        return (
            Some(planned.into_iter().flatten().collect::<Vec<_>>().join(",")),
            None,
        );
    }
    let Some(context) = context else {
        return (
            None,
            Some("unapproved admission withdrawal requires mutation control".to_owned()),
        );
    };
    let guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        pr,
        "runner steward withdraw admission of an unapproved head",
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };
    let mut done = Vec::new();
    let mut error = None;
    // Disarm first, so a request left armed cannot re-enqueue the head the
    // dequeue removes.
    if withdrawal.disarm {
        match disarm_under_guard(context, pr, ledger, DISARM_AUDIT) {
            Ok(()) => done.push(format!("disarmed_stale_auto_merge:{head}")),
            Err(failure) => error = Some(failure),
        }
    }
    if withdrawal.dequeue && error.is_none() {
        match dequeue_under_guard(context, pr, ledger) {
            Ok(()) => done.push(format!("dequeued_unapproved:{head}")),
            Err(failure) => error = Some(failure),
        }
    }
    let error =
        error.map(|failure| format!("{failure} (withdrawing because {})", withdrawal.reason));
    let outcome = if error.is_some() {
        "withdrawal_failed"
    } else {
        "unapproved_admission_withdrawn"
    };
    let mutation = (!done.is_empty()).then(|| done.join(","));
    match (guard.finish(outcome), error) {
        (Ok(()), error) => (mutation, error),
        (Err(audit), None) => (mutation, Some(format!("mutation audit failed: {audit}"))),
        (Err(audit), Some(error)) => (
            mutation,
            Some(format!("{error}; mutation audit also failed: {audit}")),
        ),
    }
}
