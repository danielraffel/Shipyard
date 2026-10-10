use serde_json::{Value, json};

use super::{
    Approval, ApprovalPolicy, EJECTION_CAP, HeadGate, decide, decide_excusing, ejection_cause,
    find_approval, head_ejections, read_policy,
};

/// The heads Generous-Corp/pulp#9438 was force-pushed to on 2026-10-04, in
/// order. Each push was followed within seconds by Shipyard re-arming it.
pub(crate) const PR_9438_HEADS: &[&str] = &[
    "d89dfa7b4ee1b11b797d9d03edfdeddf640b8d3c",
    "92f3fceadd215065fe8f5e817b698b13f7e32199",
    "5c64192d3ed885cfb9ef9eec4b034987cad0434e",
    "7f7bf0bcab7305af2892e82e497ff0cb0456f820",
    "df36a4160da0e35101f04cc9417129f849a93fc0",
    "d9385a887b82ebaedfc985030c6ae6d664d62742",
    "db2c94acbfddf9583fed3c6142288a57870ca4d5",
];

/// #9438's merge-queue timeline as `PR_QUEUE_STATE_QUERY` returned it just
/// after the force-push to `head`: every event up to and including that push.
pub(crate) fn pr_9438_queue(head: &str) -> Value {
    let removal = |at: &str, reason: &str, oid: &str| {
        json!({"__typename": "RemovedFromMergeQueueEvent", "createdAt": at, "reason": reason,
               "actor": {"login": "shipyard-local"}, "beforeCommit": {"oid": oid}})
    };
    let push = |at: &str, oid: &str| {
        json!({"__typename": "HeadRefForcePushedEvent", "createdAt": at,
               "afterCommit": {"oid": oid}})
    };
    let events = vec![
        removal(
            "2026-10-04T00:48:52Z",
            "manual",
            "0394ac44cd909550325cf0095ef4e68e9ca2654e",
        ),
        push("2026-10-04T00:50:06Z", PR_9438_HEADS[0]),
        push("2026-10-04T01:04:25Z", PR_9438_HEADS[1]),
        push("2026-10-04T01:40:30Z", PR_9438_HEADS[2]),
        removal(
            "2026-10-04T02:20:39Z",
            "manual",
            "cca4ee6c25a8b4fda6b97e6e9993994c34a69eb2",
        ),
        push("2026-10-04T02:29:36Z", PR_9438_HEADS[3]),
        push("2026-10-04T03:44:17Z", PR_9438_HEADS[4]),
        push("2026-10-04T04:04:26Z", PR_9438_HEADS[5]),
        removal(
            "2026-10-04T04:41:55Z",
            "manual",
            "a436b80422de2da55e96c5e05ec8d8077940eb8e",
        ),
        push("2026-10-04T04:43:06Z", PR_9438_HEADS[6]),
    ];
    let pushed = events
        .iter()
        .position(|event| event.pointer("/afterCommit/oid").and_then(Value::as_str) == Some(head))
        .expect("head is one of #9438's pushes");
    json!({"data": {"repository": {"pullRequest": {
        "number": 9438, "state": "OPEN", "headRefOid": head, "isInMergeQueue": false,
        "mergeQueueEntry": null, "autoMergeRequest": null,
        "timelineItems": {"pageInfo": {"hasPreviousPage": false},
                          "nodes": events[..=pushed].to_vec()}}}}})
}

/// The reviewer comments #9438 carried, all posted through the shared
/// Shipyard App identity. The last one is the approval of the final head; a
/// marker naming each head is added to prove a bot marker never counts.
pub(crate) fn pr_9438_bot_comments() -> Vec<Value> {
    let mut comments = vec![
        json!({"login": "shipyard-local[bot]", "type": "Bot", "at": "2026-10-04T03:57:36Z",
               "body": "Review of df36a416 (fable-graph, design owner). Verdict: one must-fix, then approve."}),
        json!({"login": "shipyard-local[bot]", "type": "Bot", "at": "2026-10-04T04:49:15Z",
               "body": "Re-review of 3ac483be (fable-graph). Approve this exact head."}),
    ];
    for head in PR_9438_HEADS {
        comments.push(json!({"login": "shipyard-local[bot]", "type": "Bot",
                             "at": "2026-10-04T05:00:00Z", "body": format!("reviewed:{head}")}));
    }
    comments
}

fn required(reviewers: &[&str]) -> ApprovalPolicy {
    ApprovalPolicy {
        required: true,
        reviewer_logins: reviewers.iter().map(|login| (*login).to_owned()).collect(),
    }
}

fn human(login: &str, at: &str, body: &str) -> Value {
    json!({"login": login, "type": "User", "at": at, "body": body})
}

fn review(login: &str, kind: &str, state: &str, commit: &str, at: &str) -> Value {
    json!({"login": login, "type": kind, "at": at, "state": state, "commit_id": commit})
}

fn empty_queue(head: &str) -> Value {
    json!({"data": {"repository": {"pullRequest": {"headRefOid": head,
        "timelineItems": {"nodes": []}}}}})
}

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

// ---------------------------------------------------------------------------
// #9438 replayed
// ---------------------------------------------------------------------------

/// Negative control: every head #9438 was rebased to stays disarmed, even
/// with a reviewer listed, because nothing a non-bot account wrote approved
/// any of them.
#[test]
fn replaying_9438_leaves_every_rebased_head_unapproved() {
    let policy = required(&["danielraffel"]);
    let comments = pr_9438_bot_comments();
    for head in PR_9438_HEADS {
        let gate = decide(&policy, head, &[], &comments, &pr_9438_queue(head));
        assert_eq!(
            gate,
            HeadGate::NotApproved {
                head: (*head).to_owned()
            },
            "{head}"
        );
    }
}

/// Positive control: the same sequence with a listed reviewer's marker for one
/// rebased head approves exactly that head.
#[test]
fn replaying_9438_with_a_marker_approves_exactly_the_marked_head() {
    let policy = required(&["danielraffel"]);
    let mut comments = pr_9438_bot_comments();
    comments.push(human(
        "danielraffel",
        "2026-10-04T04:45:00Z",
        "Read it.\nreviewed:db2c94acbfdd\n",
    ));
    let approved: Vec<&str> = PR_9438_HEADS
        .iter()
        .copied()
        .filter(|head| decide(&policy, head, &[], &comments, &pr_9438_queue(head)).allows())
        .collect();
    assert_eq!(approved, vec![PR_9438_HEADS[6]]);
}

// ---------------------------------------------------------------------------
// What counts as approval
// ---------------------------------------------------------------------------

#[test]
fn an_approving_review_on_the_head_counts() {
    let reviews = [review(
        "someone",
        "User",
        "APPROVED",
        HEAD,
        "2026-10-04T01:00:00Z",
    )];
    assert_eq!(
        find_approval(HEAD, &reviews, &[], &required(&[]), None),
        Some(Approval::Review {
            login: "someone".to_owned()
        })
    );
}

#[test]
fn an_approving_review_of_an_earlier_head_does_not_count() {
    let reviews = [review(
        "someone",
        "User",
        "APPROVED",
        OTHER,
        "2026-10-04T01:00:00Z",
    )];
    assert_eq!(
        find_approval(HEAD, &reviews, &[], &required(&[]), None),
        None
    );
}

#[test]
fn a_review_that_is_not_an_approval_does_not_count() {
    let reviews = [review(
        "someone",
        "User",
        "COMMENTED",
        HEAD,
        "2026-10-04T01:00:00Z",
    )];
    assert_eq!(
        find_approval(HEAD, &reviews, &[], &required(&[]), None),
        None
    );
}

#[test]
fn an_approving_review_from_a_bot_does_not_count() {
    let reviews = [
        review(
            "shipyard-local[bot]",
            "Bot",
            "APPROVED",
            HEAD,
            "2026-10-04T01:00:00Z",
        ),
        review("helper", "Bot", "APPROVED", HEAD, "2026-10-04T01:00:00Z"),
    ];
    assert_eq!(
        find_approval(HEAD, &reviews, &[], &required(&[]), None),
        None
    );
}

#[test]
fn a_marker_from_a_listed_reviewer_counts() {
    let comments = [human(
        "reviewer",
        "2026-10-04T01:00:00Z",
        "reviewed:aaaaaaa",
    )];
    assert_eq!(
        find_approval(HEAD, &[], &comments, &required(&["Reviewer"]), None),
        Some(Approval::Marker {
            login: "reviewer".to_owned()
        })
    );
}

#[test]
fn a_marker_counts_only_once_reviewers_are_listed() {
    let comments = [human(
        "reviewer",
        "2026-10-04T01:00:00Z",
        &format!("reviewed:{HEAD}"),
    )];
    assert_eq!(
        find_approval(HEAD, &[], &comments, &required(&[]), None),
        None
    );
    assert_eq!(
        find_approval(HEAD, &[], &comments, &required(&["someone-else"]), None),
        None
    );
}

#[test]
fn a_marker_from_a_listed_bot_does_not_count() {
    let comments = [json!({"login": "shipyard-local[bot]", "type": "Bot",
                           "at": "2026-10-04T01:00:00Z", "body": format!("reviewed:{HEAD}")})];
    assert_eq!(
        find_approval(
            HEAD,
            &[],
            &comments,
            &required(&["shipyard-local[bot]"]),
            None
        ),
        None
    );
}

#[test]
fn a_marker_must_name_this_head_on_its_own_line() {
    let policy = required(&["reviewer"]);
    for body in [
        format!("reviewed:{OTHER}"),
        "reviewed:aaaaaa".to_owned(),
        format!("not reviewed:{HEAD}"),
        format!("reviewed: {HEAD} looks fine"),
    ] {
        let comments = [human("reviewer", "2026-10-04T01:00:00Z", &body)];
        assert_eq!(
            find_approval(HEAD, &[], &comments, &policy, None),
            None,
            "{body}"
        );
    }
}

#[test]
fn a_repository_that_does_not_require_approval_arms_without_one() {
    assert_eq!(
        decide(
            &ApprovalPolicy::default(),
            HEAD,
            &[],
            &[],
            &empty_queue(HEAD)
        ),
        HeadGate::NotRequired
    );
}

// ---------------------------------------------------------------------------
// Ejections
// ---------------------------------------------------------------------------

fn ejected_twice(head: &str) -> Value {
    json!({"data": {"repository": {"pullRequest": {"headRefOid": head,
    "timelineItems": {"nodes": [
        {"__typename": "RemovedFromMergeQueueEvent", "createdAt": "2026-10-04T01:00:00Z",
         "reason": "FAILED_CHECKS", "beforeCommit": {"oid": head}},
        {"__typename": "RemovedFromMergeQueueEvent", "createdAt": "2026-10-04T02:00:00Z",
         "reason": "INVALID_MERGE_COMMIT", "beforeCommit": {"oid": head}},
        {"__typename": "RemovedFromMergeQueueEvent", "createdAt": "2026-10-04T03:00:00Z",
         "reason": "MANUAL", "beforeCommit": {"oid": OTHER}},
        {"__typename": "RemovedFromMergeQueueEvent", "createdAt": "2026-10-04T04:00:00Z",
         "reason": "MERGED", "beforeCommit": {"oid": head}},
    ]}}}}})
}

#[test]
fn ejections_are_counted_per_head_with_their_cause_and_merges_excluded() {
    let ejections = head_ejections(&ejected_twice(HEAD), HEAD);
    assert_eq!(ejections.len(), 2);
    assert_eq!(ejections[0].reason, "FAILED_CHECKS");
    assert_eq!(
        ejections[0].cause,
        "checks failed, its own or a batch neighbour's"
    );
    assert_eq!(ejections[1].cause, "main moved under it");
    assert_eq!(head_ejections(&ejected_twice(HEAD), OTHER).len(), 1);
}

#[test]
fn ejection_causes_name_manual_and_unknown_reasons() {
    assert_eq!(ejection_cause("manual"), "removed by hand");
    assert_eq!(ejection_cause("merge_conflict"), "main moved under it");
    assert_eq!(ejection_cause("something_new"), "another reason");
}

#[test]
fn a_head_ejected_twice_needs_an_approval_newer_than_the_last_ejection() {
    assert_eq!(EJECTION_CAP, 2);
    let policy = required(&["reviewer"]);
    let old = [review(
        "someone",
        "User",
        "APPROVED",
        HEAD,
        "2026-10-04T01:30:00Z",
    )];
    let gate = decide(&policy, HEAD, &old, &[], &ejected_twice(HEAD));
    assert!(matches!(gate, HeadGate::EjectionCap { ref ejections, .. } if ejections.len() == 2));
    assert!(
        gate.explain().contains("FAILED_CHECKS"),
        "{}",
        gate.explain()
    );

    let fresh = [human(
        "reviewer",
        "2026-10-04T02:30:00Z",
        &format!("reviewed:{HEAD}"),
    )];
    assert!(decide(&policy, HEAD, &old, &fresh, &ejected_twice(HEAD)).allows());
}

#[test]
fn an_interruption_ejection_does_not_count_toward_the_cap() {
    let policy = required(&["reviewer"]);
    let old = [review(
        "someone",
        "User",
        "APPROVED",
        HEAD,
        "2026-10-04T01:30:00Z",
    )];
    // The second removal was classified an interruption: one counted
    // ejection remains, under the cap, so the earlier approval still holds.
    assert!(decide_excusing(&policy, HEAD, &old, &[], &ejected_twice(HEAD), true).allows());
    // Control: unexcused, the same facts hit the cap.
    assert!(matches!(
        decide_excusing(&policy, HEAD, &old, &[], &ejected_twice(HEAD), false),
        HeadGate::EjectionCap { .. }
    ));
}

#[test]
fn a_head_ejected_once_keeps_its_earlier_approval() {
    let queue = json!({"data": {"repository": {"pullRequest": {"timelineItems": {"nodes": [
        {"__typename": "RemovedFromMergeQueueEvent", "createdAt": "2026-10-04T02:00:00Z",
         "reason": "MANUAL", "beforeCommit": {"oid": HEAD}}]}}}}});
    let old = [review(
        "someone",
        "User",
        "APPROVED",
        HEAD,
        "2026-10-04T01:30:00Z",
    )];
    assert!(decide(&required(&[]), HEAD, &old, &[], &queue).allows());
}

// ---------------------------------------------------------------------------
// The policy
// ---------------------------------------------------------------------------

#[test]
fn the_policy_is_read_from_the_auto_merge_table() {
    let table: toml::Table = "[auto_merge]\narm_requires_head_approval = true\n\
         reviewer_logins = [\"DanielRaffel\"]\n"
        .parse()
        .expect("toml");
    assert_eq!(
        ApprovalPolicy::from_table(&table),
        ApprovalPolicy {
            required: true,
            reviewer_logins: vec!["danielraffel".to_owned()]
        }
    );
    let empty: toml::Table = "[project]\nname = \"x\"\n".parse().expect("toml");
    assert_eq!(
        ApprovalPolicy::from_table(&empty),
        ApprovalPolicy::default()
    );
}

#[test]
fn the_policy_comes_from_the_base_branch_and_a_missing_file_is_the_default() {
    let asked = std::cell::RefCell::new(String::new());
    let present = |args: &[String]| -> Result<String, String> {
        *asked.borrow_mut() = args.join(" ");
        Ok("[auto_merge]\narm_requires_head_approval = true\n".to_owned())
    };
    assert!(read_policy(&present, "o/r", "main").expect("read").required);
    assert!(
        asked
            .borrow()
            .contains("repos/o/r/contents/.shipyard/config.toml?ref=main"),
        "{}",
        asked.borrow()
    );
    let missing =
        |_: &[String]| -> Result<String, String> { Err("gh: Not Found (HTTP 404)".to_owned()) };
    assert_eq!(
        read_policy(&missing, "o/r", "main"),
        Ok(ApprovalPolicy::default())
    );
}

#[test]
fn an_unreadable_policy_is_an_error_not_the_default() {
    let failing =
        |_: &[String]| -> Result<String, String> { Err("HTTP 502: Bad Gateway".to_owned()) };
    assert!(read_policy(&failing, "o/r", "main").is_err());
    let garbled = |_: &[String]| -> Result<String, String> { Ok("[auto_merge\n".to_owned()) };
    assert!(read_policy(&garbled, "o/r", "main").is_err());
}
