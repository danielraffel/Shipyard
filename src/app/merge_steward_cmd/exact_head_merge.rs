//! Server-enforced exact-head merge for a base without a merge queue.
//!
//! GitHub offers merge queues only to organization-owned repositories, so a
//! user-owned repository can never take the steward's `enqueuePullRequest`
//! path. When such a base still has a GitHub-enforced required-check set that
//! includes an app-bound exact-head approval context (`review-verdict` by
//! default), the steward merges an approved head itself: only when GitHub
//! reports it `CLEAN`, every required check is green, and the steward's own
//! exact-head approval gate returns `Approved` (never `NotRequired`). The
//! mutation is `mergePullRequest` bound by `expectedHeadOid`, through the
//! ordinary `gh` path so the `ghapp` merge guard judges it, and GitHub still
//! refuses the merge of any head whose approval context or CI has not passed.
//! See [`crate::merge_steward::StewardPolicy::server_enforced_exact_head_merge`].
//!
//! It never arms native auto-merge: an armed request survives a later push by
//! anyone with write access and would then merge a head nobody judged. For the
//! same reason any auto-merge request found armed on an owned pull request on
//! such a base is disarmed, whatever the pull request's decision.

use super::{
    MutationApplyContext, ObservedPr, StewardDecision, StewardLedger, StewardPolicy, Value,
    acquire_pr_mutation_guard, pr_mutations::final_enqueue_revalidation,
    pr_mutations::read_head_gate_with_queue_state, record_audit,
};
use crate::auto_arm::first_graphql_error;
use crate::merge_queue_control::MergeQueueMutationGuard;

/// GraphQL document that disarms native auto-merge. Variable: `id`.
const DISABLE_AUTO_MERGE_MUTATION: &str = "mutation($id:ID!){disablePullRequestAutoMerge(input:\
     {pullRequestId:$id}){pullRequest{number}}}";

/// GraphQL document that merges one exact head. Variables: `id`, `head`.
const EXACT_HEAD_MERGE_MUTATION: &str = "mutation($id:ID!,$head:GitObjectID!)\
     {mergePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head,mergeMethod:MERGE})\
     {pullRequest{number merged}}}";

/// Finish the mutation guard with `outcome`, folding an audit failure into the
/// returned error.
fn finish(
    guard: MergeQueueMutationGuard,
    outcome: &str,
    mutation: Option<String>,
    error: Option<String>,
) -> (Option<String>, Option<String>) {
    match (guard.finish(outcome), error) {
        (Ok(()), error) => (mutation, error),
        (Err(audit), None) => (mutation, Some(format!("mutation audit failed: {audit}"))),
        (Err(audit), Some(error)) => (
            mutation,
            Some(format!("{error}; mutation audit also failed: {audit}")),
        ),
    }
}

fn join(prior: Option<String>, next: &str) -> String {
    prior.map_or_else(|| next.to_owned(), |prior| format!("{prior},{next}"))
}

/// Whether the steward owns `decision` on a server-enforced base, and so must
/// not leave an auto-merge request armed on it.
pub(super) fn owns_for_disarm(decision: &StewardDecision) -> bool {
    !matches!(
        decision,
        StewardDecision::Unmanaged
            | StewardDecision::HandoffMissing
            | StewardDecision::OptedOut
            | StewardDecision::ProvenanceBlocked { .. }
    )
}

/// Disarm native auto-merge on `pr` under an already-held guard.
fn disarm(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> Result<String, String> {
    let args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={DISABLE_AUTO_MERGE_MUTATION}"),
        "-F".to_owned(),
        format!("id={}", pr.node_id),
    ];
    let raw = context.actions.run_gh(&args).map_err(|error| {
        format!(
            "PR #{} auto-merge could not be disarmed: {error}",
            pr.fact.number
        )
    })?;
    let accepted = serde_json::from_str::<Value>(&raw).is_ok_and(|value| {
        value.get("errors").is_none()
            && value
                .pointer("/data/disablePullRequestAutoMerge/pullRequest")
                .is_some_and(|pull_request| !pull_request.is_null())
    });
    if !accepted {
        return Err(format!(
            "PR #{} auto-merge disarm was not accepted: {}",
            pr.fact.number,
            first_graphql_error(&raw).unwrap_or_else(|| "no errors reported".to_owned())
        ));
    }
    record_audit(
        ledger,
        &context.observation.repo,
        &format!("pr:{}:{}", pr.fact.number, pr.fact.head_sha),
        "disable_auto_merge_server_enforced_base",
    );
    Ok(format!("auto_merge_disabled:{}", pr.fact.head_sha))
}

/// Disarm an auto-merge request the census saw armed on an owned pull
/// request. Disarming only removes authority, so it needs the mutation guard
/// but no approval.
pub(super) fn disarm_owned_auto_merge(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        pr,
        "runner steward disable auto-merge on a server-enforced base",
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };
    match disarm(context, pr, ledger) {
        Ok(mutation) => finish(guard, "auto_merge_disabled", Some(mutation), None),
        Err(error) => finish(guard, "disarm_failed", None, Some(error)),
    }
}

/// Merge `pr`'s exact, approved, already-mergeable head.
///
/// Outcomes, as the per-PR mutation string:
/// * `waiting_head_approval` — the exact-head gate did not return
///   `Approved`, or the `ghapp` merge guard refused; nothing was merged.
/// * `waiting_required` — the live head is not `CLEAN` with every required
///   check green.
/// * `auto_merge_disabled:<sha>` — an armed auto-merge request was disarmed
///   (prefixes any other outcome).
/// * `merged_exact_head:<sha>` — merged with `expectedHeadOid` = `<sha>`.
#[allow(clippy::too_many_lines)] // One guarded transaction keeps the gate, the read and the merge adjacent.
pub(super) fn merge_exact_head(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let head = pr.fact.head_sha.clone();
    let (approval, verdict_audit_events, queue) = match read_head_gate_with_queue_state(context, pr)
    {
        Ok((crate::head_approval::HeadGate::Approved { approval }, events, queue)) => {
            (approval, events, queue)
        }
        Ok((crate::head_approval::HeadGate::NotRequired, _events, _queue)) => {
            return (
                Some("waiting_head_approval".to_owned()),
                Some(format!(
                    "head {} needs an exact-head approval: this base merges only approved \
                     heads, and head approval is not required here, so nothing was merged",
                    head.get(..12).unwrap_or(&head)
                )),
            );
        }
        Ok((gate, _events, _queue)) => {
            return (
                Some("waiting_head_approval".to_owned()),
                Some(gate.explain()),
            );
        }
        Err(error) => return (None, Some(error)),
    };
    let queue_head = queue
        .pointer("/data/repository/pullRequest/headRefOid")
        .and_then(Value::as_str);
    if !queue_head.is_some_and(|live| live.eq_ignore_ascii_case(&head)) {
        return (Some("skipped_after_live_revalidation".to_owned()), None);
    }
    let run_gh = |argv: &[String]| {
        context
            .actions
            .run_gh(argv)
            .map_err(|error| error.to_string())
    };
    match crate::deferred_tier::deferred_tier_checks(
        &run_gh,
        &context.observation.repo,
        pr.fact.number,
        &context.observation.base,
        &head,
    ) {
        Ok(checks) => {
            if let Some(detail) = crate::deferred_tier::explain_all(&checks) {
                return (Some("waiting_full_test_tier".to_owned()), Some(detail));
            }
        }
        Err(error) => return (None, Some(format!("test tier unreadable: {error}"))),
    }
    let mut guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        pr,
        "runner steward exact-head merge on a server-enforced base",
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };
    let mut mutation = None;
    if queue
        .pointer("/data/repository/pullRequest/autoMergeRequest")
        .is_some_and(|request| !request.is_null())
    {
        match disarm(context, pr, ledger) {
            Ok(disarmed) => mutation = Some(disarmed),
            Err(error) => return finish(guard, "disarm_failed", None, Some(error)),
        }
    }
    if let Err(error) = guard.record_verdict_audit_events(verdict_audit_events) {
        return finish(
            guard,
            "verdict_evidence_persistence_failed",
            mutation,
            Some(error),
        );
    }
    if let Err(error) = guard.record_approval_evidence(approval.comment_id(), approval.app_id()) {
        return finish(
            guard,
            "approval_evidence_persistence_failed",
            mutation,
            Some(error),
        );
    }
    // Final live read under the guard: same head, same queue capability, and
    // still a merge candidate on GitHub's current facts.
    let live = match final_enqueue_revalidation(context, pr, policy, ledger) {
        Ok(Some(live)) => live,
        Ok(None) => {
            return finish(
                guard,
                "skipped_after_final_live_revalidation",
                Some(join(mutation, "skipped_after_final_live_revalidation")),
                None,
            );
        }
        Err(error) => {
            return finish(
                guard,
                "final_live_revalidation_failed",
                mutation,
                Some(error),
            );
        }
    };
    if !(live.fact.merge_state.eq_ignore_ascii_case("CLEAN")
        && crate::merge_steward::required_checks_all_succeeded(&live.fact, policy)
        && crate::merge_steward::approval_status_agrees(&live.fact, policy))
    {
        return finish(
            guard,
            "waiting_required",
            Some(join(mutation, "waiting_required")),
            None,
        );
    }
    let args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={EXACT_HEAD_MERGE_MUTATION}"),
        "-F".to_owned(),
        format!("id={}", pr.node_id),
        "-f".to_owned(),
        format!("head={head}"),
    ];
    // Deliberately the ordinary `run_gh`: the `ghapp` merge guard judges this
    // merge, and its refusals are honoured, never overridden.
    match context.actions.run_gh(&args) {
        Ok(raw)
            if serde_json::from_str::<Value>(&raw).is_ok_and(|value| {
                value.get("errors").is_none()
                    && value
                        .pointer("/data/mergePullRequest/pullRequest/merged")
                        .and_then(Value::as_bool)
                        == Some(true)
            }) =>
        {
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("pr:{}:{head}", pr.fact.number),
                "merge_exact_head_server_enforced",
            );
            finish(
                guard,
                "merged_exact_head",
                Some(join(mutation, &format!("merged_exact_head:{head}"))),
                None,
            )
        }
        Ok(raw) => finish(
            guard,
            "rejected",
            mutation,
            Some(format!(
                "PR #{} exact-head merge was not accepted: {}",
                pr.fact.number,
                first_graphql_error(&raw).unwrap_or_else(|| "no errors reported".to_owned())
            )),
        ),
        Err(error) => {
            let message = error.to_string();
            if crate::queue_arm_gate::is_refusal(&message) {
                finish(
                    guard,
                    "refused_by_merge_guard",
                    Some(join(mutation, "waiting_head_approval")),
                    Some(message),
                )
            } else {
                finish(guard, "merge_failed", mutation, Some(message))
            }
        }
    }
}
