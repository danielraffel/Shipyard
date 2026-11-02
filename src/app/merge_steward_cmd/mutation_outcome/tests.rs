use serde_json::json;

use super::{CODE_OWNER_REVIEW, definitive_refusal, is_refusal_token, judge, refusal_token};
use crate::merge_queue_control::{ARM_ACTION, UncertainVerdict};

const HEAD: &str = "4b6a8712010692227ea47c4040d7178d4823f6ce";
const ENQUEUE: &str = "runner steward enqueue pull request";

/// The exact failure `gh` reported for vellum #136 on 2026-11-01.
const VELLUM_136: &str = r#"gh api graphql -f query=mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head}){mergeQueueEntry{position}}} -F id=PR_kwDOTgkcHM8AAAABMIzpxg -F head=4b6a8712010692227ea47c4040d7178d4823f6ce failed with status 1: {"data":{"enqueuePullRequest":null},"errors":[{"type":"UNPROCESSABLE","path":["enqueuePullRequest"],"locations":[{"line":1,"column":38}],"message":"Pull request Waiting on code owner review from danielraffel."}]}gh: Pull request Waiting on code owner review from danielraffel."#;

#[test]
fn a_typed_graphql_refusal_is_definitive() {
    let refusal = definitive_refusal(VELLUM_136).expect("definitive");
    assert_eq!(
        refusal,
        "Pull request Waiting on code owner review from danielraffel."
    );
    assert_eq!(refusal_token(&refusal), CODE_OWNER_REVIEW);
    // A bare response body, as the success path receives it, reads the same.
    let body = r#"{"data":{"enqueuePullRequest":null},"errors":[{"type":"UNPROCESSABLE","message":"Base branch was modified, review and try again."}]}"#;
    let refusal = definitive_refusal(body).expect("definitive");
    let token = refusal_token(&refusal);
    assert_eq!(
        token,
        "enqueue_refused:Base branch was modified; review and try again"
    );
    assert!(is_refusal_token(&token));
    assert!(
        !token[1..].contains(','),
        "tokens are comma-joined: {token}"
    );
}

#[test]
fn anything_short_of_a_typed_refusal_is_not_definitive() {
    for text in [
        "connection reset after request body",
        "HTTP 502: transient upstream failure",
        "operation timed out",
        // Untyped error: GitHub did not say it declined.
        r#"{"data":{"enqueuePullRequest":null},"errors":[{"message":"Something went wrong"}]}"#,
        // A payload beside the error is a partial success.
        r#"{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"position":1}}},"errors":[{"type":"UNPROCESSABLE","message":"x"}]}"#,
        // Success.
        r#"{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"position":1}}}}"#,
    ] {
        assert_eq!(definitive_refusal(text), None, "{text}");
    }
    assert!(!is_refusal_token("waiting_enqueue_requirements"));
    assert!(!is_refusal_token("enqueued"));
}

fn queue(pr: &serde_json::Value) -> serde_json::Value {
    json!({"data": {"repository": {"pullRequest": pr}}})
}

#[test]
fn a_merged_or_queued_head_settles_accepted() {
    let merged = queue(&json!({"state": "MERGED", "headRefOid": HEAD,
        "mergeQueueEntry": null, "autoMergeRequest": null}));
    assert!(matches!(
        judge(&merged, HEAD, ENQUEUE),
        UncertainVerdict::Accepted(_)
    ));
    let queued = queue(
        &json!({"state": "OPEN", "headRefOid": HEAD, "isInMergeQueue": true,
        "mergeQueueEntry": {"state": "QUEUED", "position": 2}, "autoMergeRequest": null}),
    );
    assert!(matches!(
        judge(&queued, HEAD, ENQUEUE),
        UncertainVerdict::Accepted(_)
    ));
    let armed = queue(&json!({"state": "OPEN", "headRefOid": HEAD,
        "mergeQueueEntry": null, "autoMergeRequest": {"enabledAt": "2026-11-01T00:00:00Z"}}));
    assert!(matches!(
        judge(&armed, HEAD, ARM_ACTION),
        UncertainVerdict::Accepted(_)
    ));
}

#[test]
fn nothing_on_github_settles_rejected() {
    let idle = queue(
        &json!({"state": "OPEN", "headRefOid": HEAD, "isInMergeQueue": false,
        "mergeQueueEntry": null, "autoMergeRequest": null}),
    );
    assert!(matches!(
        judge(&idle, HEAD, ENQUEUE),
        UncertainVerdict::Rejected(_)
    ));
    let closed = queue(&json!({"state": "CLOSED", "headRefOid": HEAD,
        "mergeQueueEntry": null, "autoMergeRequest": null}));
    assert!(matches!(
        judge(&closed, HEAD, ENQUEUE),
        UncertainVerdict::Rejected(_)
    ));
}

#[test]
fn an_undecided_or_unreadable_state_stays_uncertain() {
    let other_head = queue(&json!({"state": "OPEN", "headRefOid": "c".repeat(40),
        "mergeQueueEntry": {"state": "QUEUED", "position": 0}, "autoMergeRequest": null}));
    let armed_enqueue = queue(&json!({"state": "OPEN", "headRefOid": HEAD,
        "mergeQueueEntry": null, "autoMergeRequest": {"enabledAt": "x"}}));
    let errors = json!({"errors": [{"message": "boom"}]});
    let missing = json!({"data": {"repository": {"pullRequest": null}}});
    for (name, value) in [
        ("queued at another head", other_head),
        ("armed but an enqueue was recorded", armed_enqueue),
        ("errors", errors),
        ("no pull request", missing),
    ] {
        assert!(
            matches!(
                judge(&value, HEAD, ENQUEUE),
                UncertainVerdict::Unresolved(_)
            ),
            "{name}"
        );
    }
}
