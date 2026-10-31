//! Exact-head native auto-merge admission for a base without a merge queue.
//!
//! GitHub offers merge queues only to organization-owned repositories, so a
//! user-owned repository can never take the steward's `enqueuePullRequest`
//! path. When such a base still has GitHub-enforced required checks that
//! include an exact-head approval context (`review-verdict` by default) and
//! the repository allows auto-merge, GitHub itself refuses to merge any head
//! whose approval check has not passed. Arming `enablePullRequestAutoMerge`
//! with `expectedHeadOid` = the approved head then grants nothing branch
//! protection does not already hold. See
//! [`crate::merge_steward::StewardPolicy::native_auto_merge_exact_head`].
//!
//! The steward still runs its own exact-head approval gate first, and the
//! arm goes through the ordinary `gh` path, so the `ghapp` queue-arm guard
//! judges it too. A head that is already mergeable cannot be armed, so on
//! this path only it is merged with `mergePullRequest` bound by
//! `expectedHeadOid`; the ruleset still makes GitHub refuse any head whose
//! approval context or CI has not passed. A base without the required
//! approval context, and the org-queue path, never take this merge.

use super::{
    MutationApplyContext, ObservedPr, StewardLedger, StewardPolicy, Value,
    acquire_pr_mutation_guard, pr_mutations::final_enqueue_revalidation,
    pr_mutations::read_head_gate_with_queue_state, record_audit,
};
use crate::auto_arm::{arm_mutation_args_at_head, arm_response_accepted, first_graphql_error};

/// GraphQL document that disarms native auto-merge. Variable: `id`.
const DISABLE_AUTO_MERGE_MUTATION: &str = "mutation($id:ID!){disablePullRequestAutoMerge(input:\
     {pullRequestId:$id}){pullRequest{number}}}";

/// GitHub's answer when the pull request is already mergeable: auto-merge
/// cannot be armed on it, and the steward does not merge directly.
const CLEAN_STATUS_REFUSAL: &str = "clean status";

/// Where an existing native auto-merge request stands relative to `head`.
#[derive(Debug, Eq, PartialEq)]
enum ArmedState {
    /// Nothing is armed.
    Unarmed,
    /// Armed at or after `head` arrived.
    ArmedForHead,
    /// Armed before `head` arrived, so it was armed for an earlier head.
    ArmedForEarlierHead,
}

fn armed_state(queue: &Value, head: &str) -> Result<ArmedState, String> {
    let armed = queue
        .pointer("/data/repository/pullRequest/autoMergeRequest")
        .is_some_and(|request| !request.is_null());
    if !armed {
        return Ok(ArmedState::Unarmed);
    }
    match crate::merge_carrier::head_arm_time(queue, head) {
        Ok(Some(_)) => Ok(ArmedState::ArmedForHead),
        Ok(None) => Ok(ArmedState::ArmedForEarlierHead),
        Err(detail) => Err(format!(
            "which head native auto-merge was armed for cannot be read: {detail}"
        )),
    }
}

fn short(head: &str) -> &str {
    head.get(..12).unwrap_or(head)
}

/// Finish the mutation guard with `outcome`, folding an audit failure into the
/// returned error.
fn finish(
    guard: crate::merge_queue_control::MergeQueueMutationGuard,
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

/// Arm GitHub-native auto-merge on `pr`'s exact, approved head.
///
/// Outcomes, as the per-PR mutation string:
/// * `waiting_head_approval` — the exact-head gate or the queue-arm guard
///   refused; nothing was armed.
/// * `auto_merge_already_enabled:<sha>` — armed for this head already.
/// * `auto_merge_disabled_head_moved:<sha>` — an arm left over from an
///   earlier head was disarmed; the next pass re-arms after approval.
/// * `auto_merge_enabled:<sha>` — armed with `expectedHeadOid` = `<sha>`.
/// * `merged_exact_head:<sha>` — the head was already mergeable (`CLEAN`,
///   every required check green), so it was merged with `mergePullRequest`
///   bound by `expectedHeadOid`; GitHub will not arm auto-merge on it.
/// * `auto_merge_refused_clean_status:<sha>` — GitHub reported clean status
///   even though the steward did not see the head as mergeable; nothing is
///   merged on that answer.
#[allow(clippy::too_many_lines)] // One guarded transaction keeps the gate, the read and the arm adjacent.
pub(super) fn enable_auto_merge_exact_head(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let head = pr.fact.head_sha.clone();
    let mut approval_comment_id = None;
    let mut approval_app_id = None;
    let (verdict_audit_events, queue) = match read_head_gate_with_queue_state(context, pr) {
        Ok((crate::head_approval::HeadGate::Approved { approval }, events, queue)) => {
            approval_comment_id = approval.comment_id();
            approval_app_id = approval.app_id();
            (events, queue)
        }
        Ok((gate, events, queue)) if gate.allows() => (events, queue),
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
    let armed = match armed_state(&queue, &head) {
        Ok(armed) => armed,
        Err(error) => return (None, Some(error)),
    };
    if armed == ArmedState::ArmedForHead {
        return (Some(format!("auto_merge_already_enabled:{head}")), None);
    }
    let mut guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        pr,
        if armed == ArmedState::ArmedForEarlierHead {
            "runner steward disable stale native auto-merge"
        } else {
            "runner steward enable exact-head native auto-merge"
        },
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };
    if armed == ArmedState::ArmedForEarlierHead {
        // An arm placed before this head arrived was not placed on evidence
        // about this head. Disarm it; re-arming waits for a later pass that
        // proves approval of this head afresh.
        let args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={DISABLE_AUTO_MERGE_MUTATION}"),
            "-F".to_owned(),
            format!("id={}", pr.node_id),
        ];
        return match context.actions.run_gh(&args) {
            Ok(raw)
                if serde_json::from_str::<Value>(&raw).is_ok_and(|value| {
                    value.get("errors").is_none()
                        && value
                            .pointer("/data/disablePullRequestAutoMerge/pullRequest")
                            .is_some_and(|pull_request| !pull_request.is_null())
                }) =>
            {
                record_audit(
                    ledger,
                    &context.observation.repo,
                    &format!("pr:{}:{head}", pr.fact.number),
                    "disable_auto_merge_head_moved",
                );
                finish(
                    guard,
                    "auto_merge_disabled_head_moved",
                    Some(format!("auto_merge_disabled_head_moved:{head}")),
                    None,
                )
            }
            Ok(raw) => finish(
                guard,
                "rejected",
                None,
                Some(format!(
                    "PR #{} stale auto-merge disarm was not accepted: {}",
                    pr.fact.number,
                    first_graphql_error(&raw).unwrap_or_else(|| "no errors reported".to_owned())
                )),
            ),
            Err(error) => finish(
                guard,
                "disarm_failed",
                None,
                Some(format!(
                    "PR #{} stale auto-merge could not be disarmed: {error}",
                    pr.fact.number
                )),
            ),
        };
    }
    if let Err(error) = guard.record_verdict_audit_events(verdict_audit_events) {
        return finish(
            guard,
            "verdict_evidence_persistence_failed",
            None,
            Some(error),
        );
    }
    if let Err(error) = guard.record_approval_evidence(approval_comment_id, approval_app_id) {
        return finish(
            guard,
            "approval_evidence_persistence_failed",
            None,
            Some(error),
        );
    }
    // Final live read under the guard: same head, same queue capability, and
    // still an arm candidate on GitHub's current facts.
    let live = match final_enqueue_revalidation(context, pr, policy, ledger) {
        Ok(Some(live)) => live,
        Ok(None) => {
            return finish(
                guard,
                "skipped_after_final_live_revalidation",
                Some("skipped_after_final_live_revalidation".to_owned()),
                None,
            );
        }
        Err(error) => return finish(guard, "final_live_revalidation_failed", None, Some(error)),
    };
    if live.fact.merge_state.eq_ignore_ascii_case("CLEAN")
        && crate::merge_steward::required_checks_all_succeeded(&live.fact, policy)
    {
        // GitHub will not arm auto-merge on a pull request it could merge
        // now. On this path only, merge the approved head directly: the
        // mutation is bound to it by `expectedHeadOid`, and the base's
        // ruleset still requires the approval context and CI, so GitHub
        // refuses the merge of any head those have not passed.
        return merge_exact_head(context, pr, &head, ledger, guard);
    }
    // Deliberately the ordinary `run_gh`: the `ghapp` queue-arm guard judges
    // this arm, and its refusals are honoured, never overridden.
    match context
        .actions
        .run_gh(&arm_mutation_args_at_head(&pr.node_id, &head))
    {
        Ok(raw) if arm_response_accepted(&raw) => {
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("pr:{}:{head}", pr.fact.number),
                "enable_auto_merge_exact_head",
            );
            finish(
                guard,
                "auto_merge_enabled",
                Some(format!("auto_merge_enabled:{head}")),
                None,
            )
        }
        Ok(raw) => {
            let detail =
                first_graphql_error(&raw).unwrap_or_else(|| "no errors reported".to_owned());
            if detail.to_ascii_lowercase().contains(CLEAN_STATUS_REFUSAL) {
                finish(
                    guard,
                    "refused_clean_status",
                    Some(format!("auto_merge_refused_clean_status:{head}")),
                    Some(format!(
                        "PR #{} head {} was not seen as mergeable but GitHub reports clean \
                         status, so nothing was armed or merged; the next pass re-reads it \
                         ({detail})",
                        pr.fact.number,
                        short(&head)
                    )),
                )
            } else {
                finish(
                    guard,
                    "rejected",
                    None,
                    Some(format!(
                        "PR #{} auto-merge arm was not accepted: {detail}",
                        pr.fact.number
                    )),
                )
            }
        }
        Err(error) => {
            let message = error.to_string();
            if crate::queue_arm_gate::is_refusal(&message) {
                finish(
                    guard,
                    "refused_by_queue_arm_guard",
                    Some("waiting_head_approval".to_owned()),
                    Some(message),
                )
            } else {
                finish(guard, "arm_failed", None, Some(message))
            }
        }
    }
}

/// GraphQL document that merges one exact head. Variables: `id`, `head`.
const EXACT_HEAD_MERGE_MUTATION: &str = "mutation($id:ID!,$head:GitObjectID!)\
     {mergePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head,mergeMethod:MERGE})\
     {pullRequest{number merged}}}";

/// Merge an approved, already-mergeable head on an exact-head auto-merge
/// base, through the ordinary `gh` path so the `ghapp` merge guard judges it.
fn merge_exact_head(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    head: &str,
    ledger: &mut StewardLedger,
    guard: crate::merge_queue_control::MergeQueueMutationGuard,
) -> (Option<String>, Option<String>) {
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
                "merge_exact_head_clean",
            );
            finish(
                guard,
                "merged_exact_head",
                Some(format!("merged_exact_head:{head}")),
                None,
            )
        }
        Ok(raw) => finish(
            guard,
            "rejected",
            None,
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
                    Some("waiting_head_approval".to_owned()),
                    Some(message),
                )
            } else {
                finish(guard, "merge_failed", None, Some(message))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ArmedState, armed_state};

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn queue(armed: bool, nodes: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({"data":{"repository":{"pullRequest":{
            "headRefOid": HEAD,
            "autoMergeRequest": if armed { serde_json::json!({"enabledAt":"2026-10-30T00:00:00Z"}) } else { serde_json::Value::Null },
            "timelineItems": {"pageInfo":{"hasPreviousPage":false},"nodes": nodes}
        }}}})
    }

    #[test]
    fn an_arm_before_the_head_arrived_belongs_to_an_earlier_head() {
        let arrived = serde_json::json!([
            {"__typename":"AutoMergeEnabledEvent","createdAt":"2026-10-30T00:00:00Z"},
            {"__typename":"PullRequestCommit","commit":{"oid":HEAD,"checkSuites":{"nodes":[{"createdAt":"2026-10-30T01:00:00Z"}]}}}
        ]);
        assert_eq!(
            armed_state(&queue(true, &arrived), HEAD),
            Ok(ArmedState::ArmedForEarlierHead)
        );
        let rearmed = serde_json::json!([
            {"__typename":"PullRequestCommit","commit":{"oid":HEAD,"checkSuites":{"nodes":[{"createdAt":"2026-10-30T01:00:00Z"}]}}},
            {"__typename":"AutoMergeEnabledEvent","createdAt":"2026-10-30T02:00:00Z"}
        ]);
        assert_eq!(
            armed_state(&queue(true, &rearmed), HEAD),
            Ok(ArmedState::ArmedForHead)
        );
        assert_eq!(
            armed_state(&queue(false, &rearmed), HEAD),
            Ok(ArmedState::Unarmed)
        );
    }
}
