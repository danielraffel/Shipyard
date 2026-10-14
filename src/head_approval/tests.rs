use serde_json::{Value, json};

use super::{
    Approval, ApprovalPolicy, EJECTION_CAP, HeadGate, decide, decide_excusing, decide_for_author,
    ejection_cause, find_approval, head_ejections, read_policy,
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
        reviewer_logins: if reviewers.is_empty() {
            vec!["someone".to_owned()]
        } else {
            reviewers.iter().map(|login| (*login).to_owned()).collect()
        },
        any_reviewer_but_author: false,
        human_reviewer_logins: Vec::new(),
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
        &format!("Read it.\nreviewed:{}\n", PR_9438_HEADS[6]),
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
            login: "someone".to_owned(),
            performed_via_github_app_id: None,
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
        "reviewed:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )];
    assert_eq!(
        find_approval(HEAD, &[], &comments, &required(&["Reviewer"]), None),
        Some(Approval::Marker {
            login: "reviewer".to_owned(),
            comment_id: None,
            performed_via_github_app_id: None,
        })
    );
}

#[test]
fn real_reviewer_comment_accepts_spaced_final_marker() {
    let body = r"## fable-graph review of 57802dbb66a6c963300cd0dd0177a9143ac4c63d: APPROVED

- Required checks on the exact head: `macos` success, `linux`, `windows`, `Enforce version & skill sync` all success.

reviewed: 57802dbb66a6c963300cd0dd0177a9143ac4c63d";
    assert!(find_approval(
        "57802dbb66a6c963300cd0dd0177a9143ac4c63d",
        &[],
        &[serde_json::json!({"login": "fable-graph", "type": "User", "at": "2026-10-11T12:00:00Z", "body": body})],
        &required(&["fable-graph"]),
        None,
    ).is_some());
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
fn structured_current_head_verdicts_and_latest_hold_are_scoped() {
    let policy = required(&["shipyard-review[bot]"]);
    let comments = [
        json!({"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-04T01:00:00Z","body":format!("verdict: APPROVE head:{HEAD}")}),
        json!({"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-04T02:00:00Z","body":format!("verdict: HOLD head:{HEAD}")}),
    ];
    assert!(!decide(&policy, HEAD, &[], &comments, &empty_queue(HEAD)).allows());
    let prose = [human(
        "shipyard-review[bot]",
        "2026-10-04T03:00:00Z",
        "HOLD is mentioned in prose",
    )];
    assert_eq!(find_approval(HEAD, &[], &prose, &policy, None), None);
}

#[test]
fn review_app_and_self_review_rules_are_enforced() {
    let policy = required(&["shipyard-review[bot]"]);
    let review = json!({
        "login":"shipyard-review[bot]", "type":"Bot", "at":"2026-10-04T01:00:00Z",
        "state":"APPROVED", "commit_id":HEAD, "performed_via_github_app_id": 42
    });
    assert_eq!(
        find_approval(HEAD, &[review], &[], &policy, None),
        Some(Approval::Review {
            login: "shipyard-review[bot]".to_owned(),
            performed_via_github_app_id: Some(42)
        })
    );
    assert!(!decide_for_author(
        &policy, HEAD, &[json!({"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-04T01:00:00Z","state":"APPROVED","commit_id":HEAD})], &[], &empty_queue(HEAD), false, Some("shipyard-review[bot]")
    ).allows());
}

#[test]
fn fleet_author_review_and_earlier_review_head_are_negative_controls() {
    let policy = required(&["shipyard-review[bot]"]);
    let fleet_review = json!({
        "login":"app/shipyard-local", "type":"Bot", "at":"2026-10-04T01:00:00Z",
        "state":"APPROVED", "commit_id":HEAD, "performed_via_github_app_id": 7
    });
    assert_eq!(
        find_approval(HEAD, &[fleet_review], &[], &policy, None),
        None
    );
    let old_review = json!({
        "login":"shipyard-review[bot]", "type":"Bot", "at":"2026-10-04T01:00:00Z",
        "state":"APPROVED", "commit_id":OTHER
    });
    assert_eq!(find_approval(HEAD, &[old_review], &[], &policy, None), None);
}

#[test]
fn ignored_verdicts_are_auditable_without_affecting_the_gate() {
    let policy = required(&["shipyard-review[bot]"]);
    let comments = [
        json!({"login":"shipyard-review[bot]", "at":"2026-10-04T01:00:00Z", "body":format!("verdict: HOLD head:{OTHER}")}),
        json!({"login":"shipyard-review[bot]", "at":"2026-10-04T02:00:00Z", "body":"The hold is only prose."}),
    ];
    let events =
        super::verdict_audit_events(HEAD, &[], &comments, &policy, Some("app/shipyard-local"));
    assert_eq!(events, vec!["stale_hold", "unstructured_hold_mention"]);
}

#[test]
fn short_markers_and_older_head_holds_never_gate() {
    let policy = required(&["shipyard-review[bot]"]);
    let short = [
        json!({"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-04T01:00:00Z","body":"reviewed:aaaaaaa"}),
    ];
    assert!(!decide(&policy, HEAD, &[], &short, &empty_queue(HEAD)).allows());
    let older_hold = [
        json!({"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-04T01:00:00Z","body":format!("verdict: HOLD head:{OTHER}")}),
    ];
    let approve = [
        json!({"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-04T02:00:00Z","body":format!("reviewed: {HEAD}")}),
    ];
    assert!(!decide(&policy, HEAD, &[], &older_hold, &empty_queue(HEAD)).allows());
    assert!(decide(&policy, HEAD, &[], &approve, &empty_queue(HEAD)).allows());
}

fn reviewer_comment(at: &str, body: &str) -> Value {
    json!({"login": "shipyard-review[bot]", "type": "Bot", "at": at, "body": body})
}

#[test]
fn a_hold_on_another_head_does_not_block_an_approval_of_this_head() {
    let policy = required(&["shipyard-review[bot]"]);
    let approve = reviewer_comment(
        "2026-10-04T01:00:00Z",
        &format!("verdict: APPROVE head:{HEAD}"),
    );
    // The addendum's order: HOLD on head A, then APPROVE on head B.
    let hold_first = [
        reviewer_comment(
            "2026-10-04T00:30:00Z",
            &format!("verdict: HOLD head:{OTHER}"),
        ),
        approve.clone(),
    ];
    assert!(decide(&policy, HEAD, &[], &hold_first, &empty_queue(HEAD)).allows());
    // A HOLD on another head posted after the approval is still not a
    // verdict on this head.
    let hold_after = [
        approve,
        reviewer_comment(
            "2026-10-04T02:00:00Z",
            &format!("verdict: HOLD head:{OTHER}"),
        ),
    ];
    assert!(decide(&policy, HEAD, &[], &hold_after, &empty_queue(HEAD)).allows());
}

#[test]
fn only_the_last_non_empty_line_is_a_verdict() {
    let policy = required(&["shipyard-review[bot]"]);
    let decide_on = |comments: &[Value]| decide(&policy, HEAD, &[], comments, &empty_queue(HEAD));

    // An APPROVE line above trailing prose is not a verdict.
    let approve_then_prose = [reviewer_comment(
        "2026-10-04T01:00:00Z",
        &format!("verdict: APPROVE head:{HEAD}\nThanks, nice work."),
    )];
    assert!(!decide_on(&approve_then_prose).allows());

    // A quoted HOLD on the last line is not a verdict, so the earlier
    // APPROVE stands.
    let quoted_hold = [
        reviewer_comment(
            "2026-10-04T01:00:00Z",
            &format!("verdict: APPROVE head:{HEAD}"),
        ),
        reviewer_comment(
            "2026-10-04T02:00:00Z",
            &format!("Earlier I wrote:\n> verdict: HOLD head:{HEAD}"),
        ),
    ];
    assert!(decide_on(&quoted_hold).allows());

    // A HOLD above an APPROVE last line does not block.
    let hold_above_approve = [reviewer_comment(
        "2026-10-04T01:00:00Z",
        &format!("verdict: HOLD head:{HEAD}\n\nverdict: APPROVE head:{HEAD}\n"),
    )];
    assert!(decide_on(&hold_above_approve).allows());
}

#[test]
fn an_undated_hold_on_this_head_blocks_an_approval() {
    let policy = required(&["shipyard-review[bot]"]);
    let comments = [
        reviewer_comment(
            "2026-10-04T01:00:00Z",
            &format!("verdict: APPROVE head:{HEAD}"),
        ),
        json!({"login": "shipyard-review[bot]", "type": "Bot", "at": "not a time",
               "body": format!("verdict: HOLD head:{HEAD}")}),
    ];
    assert!(!decide(&policy, HEAD, &[], &comments, &empty_queue(HEAD)).allows());
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
    let policy = required(&["someone"]);
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
            reviewer_logins: vec!["danielraffel".to_owned()],
            any_reviewer_but_author: false,
            human_reviewer_logins: Vec::new(),
        }
    );
    let empty: toml::Table = "[project]\nname = \"x\"\n".parse().expect("toml");
    assert_eq!(
        ApprovalPolicy::from_table(&empty),
        ApprovalPolicy::default()
    );
}

#[test]
fn policy_rejects_the_fleet_author_as_a_reviewer_identity() {
    let table = toml::toml! {
        auto_merge = {
            arm_requires_head_approval = true,
            reviewer_identities = ["shipyard-review[bot]", "shipyard-local[bot]"]
        }
    };
    let policy = ApprovalPolicy::from_table(&table);
    assert!(policy.validate().is_err());
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

// ---------------------------------------------------------------------------
// Repositories that require review by default
// ---------------------------------------------------------------------------

fn by_default(repo: &str) -> ApprovalPolicy {
    ApprovalPolicy::without_config(repo, true)
}

/// [`by_default`] with `humans` listed in `human_reviewers`.
fn by_default_with_humans(repo: &str, humans: &[&str]) -> ApprovalPolicy {
    let mut policy = by_default(repo);
    policy.human_reviewer_logins = humans
        .iter()
        .map(|login| login.to_ascii_lowercase())
        .collect();
    policy
}

/// A comment as GitHub reports the fleet App's: `CONTRIBUTOR`.
fn fleet(at: &str, body: &str) -> Value {
    json!({"login": "shipyard-local[bot]", "type": "Bot", "association": "CONTRIBUTOR",
           "at": at, "body": body})
}

fn person(login: &str, association: &str, body: &str) -> Value {
    json!({"login": login, "type": "User", "association": association,
           "at": "2026-10-12T01:00:00Z", "body": body})
}

fn allows(policy: &ApprovalPolicy, comments: &[Value], author: Option<&str>) -> bool {
    decide_for_author(
        policy,
        HEAD,
        &[],
        comments,
        &empty_queue(HEAD),
        false,
        author,
    )
    .allows()
}

#[test]
fn a_listed_repository_requires_review_only_on_a_host_that_turns_it_on() {
    for repo in super::REVIEW_REQUIRED_REPOS {
        let policy = by_default(repo);
        assert!(policy.required, "{repo}");
        assert!(policy.any_reviewer_but_author, "{repo}");
        assert!(policy.validate().is_ok(), "{repo}");
        assert_eq!(
            ApprovalPolicy::without_config(repo, false),
            ApprovalPolicy::default(),
            "a new binary alone changes nothing: {repo}"
        );
    }
    assert!(
        by_default("danielraffel/shipyard").required,
        "slugs are case-blind"
    );
    assert_eq!(by_default("someone/else"), ApprovalPolicy::default());
    let silent: toml::Table = "[auto_merge]\nqueue = true\n".parse().expect("toml");
    assert!(ApprovalPolicy::from_table_for_repo(&silent, "danielraffel/tartci", true).required);
    assert!(!ApprovalPolicy::from_table_for_repo(&silent, "danielraffel/tartci", false).required);
}

#[test]
fn a_config_that_sets_the_key_decides_over_the_default() {
    let off: toml::Table = "[auto_merge]\narm_requires_head_approval = false\n"
        .parse()
        .expect("toml");
    assert_eq!(
        ApprovalPolicy::from_table_for_repo(&off, "Generous-Corp/pulp", true),
        ApprovalPolicy::default()
    );
    let listed: toml::Table = "[auto_merge]\narm_requires_head_approval = true\n\
         reviewer_identities = [\"fable-graph\"]\n"
        .parse()
        .expect("toml");
    for host in [true, false] {
        let policy = ApprovalPolicy::from_table_for_repo(&listed, "Generous-Corp/pulp", host);
        assert!(
            policy.required,
            "the repository's own key needs no host switch"
        );
        assert!(!policy.any_reviewer_but_author);
        // The strict rule holds: the fleet App's marker does not count.
        let marker = [fleet("2026-10-12T01:00:00Z", &format!("reviewed: {HEAD}"))];
        assert!(!allows(&policy, &marker, None));
    }
}

#[test]
fn a_missing_config_on_a_listed_repository_follows_the_host_switch() {
    let missing =
        |_: &[String]| -> Result<String, String> { Err("gh: Not Found (HTTP 404)".to_owned()) };
    super::with_host_review_default(true, || {
        assert_eq!(
            read_policy(&missing, "danielraffel/tartci", "main"),
            Ok(by_default("danielraffel/tartci"))
        );
        assert!(
            super::read_policy_required(&missing, "Generous-Corp/vellum", "main")
                .expect("a listed repository needs no file")
                .required
        );
    });
    super::with_host_review_default(false, || {
        assert_eq!(
            read_policy(&missing, "danielraffel/tartci", "main"),
            Ok(ApprovalPolicy::default())
        );
        assert!(super::read_policy_required(&missing, "Generous-Corp/vellum", "main").is_err());
    });
}

#[test]
fn a_config_that_does_not_parse_refuses_instead_of_loosening() {
    let garbled = |_: &[String]| -> Result<String, String> {
        Ok("[auto_merge\nreviewer_identities = [\"fable-graph\"]\n".to_owned())
    };
    super::with_host_review_default(true, || {
        assert!(read_policy(&garbled, "Generous-Corp/pulp", "main").is_err());
    });
}

#[test]
fn the_fleet_apps_verdict_counts_on_the_fleet_apps_pull_request() {
    let policy = by_default("danielraffel/Shipyard");
    let approve = [fleet(
        "2026-10-12T01:00:00Z",
        &format!("Delta review.\n\nverdict: APPROVE head:{HEAD}"),
    )];
    assert!(allows(&policy, &approve, Some("app/shipyard-local")));
    // No verdict, or one naming an older head, is refused and says what is needed.
    let gate = decide_for_author(
        &policy,
        HEAD,
        &[],
        &[],
        &empty_queue(HEAD),
        false,
        Some("app/shipyard-local"),
    );
    assert!(!gate.allows());
    assert!(
        gate.explain().contains(&format!(
            "`verdict: APPROVE head:{HEAD}` or `reviewed: {HEAD}`"
        )),
        "{}",
        gate.explain()
    );
    let older = [fleet("2026-10-12T01:00:00Z", &format!("reviewed: {OTHER}"))];
    assert!(!allows(&policy, &older, Some("app/shipyard-local")));
}

#[test]
fn a_drive_by_account_cannot_approve_on_a_public_repository() {
    let policy = by_default("danielraffel/tartci");
    for association in [
        "NONE",
        "CONTRIBUTOR",
        "FIRST_TIME_CONTRIBUTOR",
        "FIRST_TIMER",
    ] {
        let drive_by = [person(
            "some-random-github-user",
            association,
            &format!("verdict: APPROVE head:{HEAD}"),
        )];
        assert!(
            !allows(&policy, &drive_by, Some("shipyard-local[bot]")),
            "{association}"
        );
    }
    let unread = [json!({"login": "some-random-github-user", "type": "User",
                         "at": "2026-10-12T01:00:00Z", "body": format!("reviewed: {HEAD}")})];
    assert!(!allows(&policy, &unread, Some("shipyard-local[bot]")));
    let drive_by_review = [json!({"login": "some-random-github-user", "type": "User",
        "association": "NONE", "at": "2026-10-12T01:00:00Z", "state": "APPROVED",
        "commit_id": HEAD})];
    assert!(
        !decide_for_author(
            &policy,
            HEAD,
            &drive_by_review,
            &[],
            &empty_queue(HEAD),
            false,
            Some("shipyard-local[bot]")
        )
        .allows()
    );
    let listed = by_default_with_humans("danielraffel/tartci", &["fable-graph"]);
    for association in super::TRUSTED_ASSOCIATIONS {
        let trusted = [person(
            "fable-graph",
            association,
            &format!("reviewed: {HEAD}"),
        )];
        assert!(
            !allows(&policy, &trusted, Some("shipyard-local[bot]")),
            "an unlisted human does not approve: {association}"
        );
        assert!(
            allows(&listed, &trusted, Some("shipyard-local[bot]")),
            "{association}"
        );
    }
    // A drive-by HOLD does not block either.
    let comments = [
        fleet(
            "2026-10-12T01:00:00Z",
            &format!("verdict: APPROVE head:{HEAD}"),
        ),
        json!({"login": "some-random-github-user", "type": "User", "association": "NONE",
               "at": "2026-10-12T02:00:00Z", "body": format!("verdict: HOLD head:{HEAD}")}),
    ];
    assert!(allows(&policy, &comments, Some("shipyard-local[bot]")));
}

#[test]
fn a_distinguishable_author_cannot_approve_their_own_head() {
    let policy = by_default("Generous-Corp/pulp");
    let own = [person(
        "DanielRaffel",
        "OWNER",
        &format!("reviewed: {HEAD}"),
    )];
    assert!(!allows(&policy, &own, Some("danielraffel")));
    let other = [person(
        "fable-graph",
        "COLLABORATOR",
        &format!("reviewed: {HEAD}"),
    )];
    let listed = by_default_with_humans("Generous-Corp/pulp", &["fable-graph", "danielraffel"]);
    assert!(allows(&listed, &other, Some("danielraffel")));
    assert!(
        !allows(&listed, &own, Some("danielraffel")),
        "listing a human never lets them approve their own pull request"
    );
    let own_review = [
        json!({"login": "danielraffel", "type": "User", "association": "OWNER",
        "at": "2026-10-12T01:00:00Z", "state": "APPROVED", "commit_id": HEAD}),
    ];
    assert!(
        !decide_for_author(
            &policy,
            HEAD,
            &own_review,
            &[],
            &empty_queue(HEAD),
            false,
            Some("danielraffel")
        )
        .allows()
    );
}

/// pulp#10293: an APPROVED review by the owner's own account, posted a minute
/// before an agent armed the App's pull request. Only the fleet App's verdict,
/// or a human listed in `human_reviewers`, approves.
#[test]
fn a_human_review_approves_only_when_the_base_lists_that_human() {
    let review = |login: &str, state: &str| {
        json!({"login": login, "type": "User", "association": "OWNER",
               "at": "2026-10-14T17:50:05Z", "state": state, "commit_id": HEAD})
    };
    let gate = |policy: &ApprovalPolicy, reviews: &[Value], comments: &[Value]| {
        decide_for_author(
            policy,
            HEAD,
            reviews,
            comments,
            &empty_queue(HEAD),
            false,
            Some("shipyard-local"),
        )
    };
    let unlisted = by_default("Generous-Corp/pulp");
    let owner_review = [review("danielraffel", "APPROVED")];
    assert!(
        !gate(&unlisted, &owner_review, &[]).allows(),
        "an unlisted human's APPROVED review does not approve"
    );
    assert_eq!(
        super::verdict_audit_events(HEAD, &owner_review, &[], &unlisted, Some("shipyard-local")),
        vec!["excluded_human_approval:danielraffel".to_owned()],
        "the exclusion is reported"
    );
    let owner_marker = [person(
        "danielraffel",
        "OWNER",
        &format!("verdict: APPROVE head:{HEAD}"),
    )];
    assert!(!gate(&unlisted, &[], &owner_marker).allows());
    let listed = by_default_with_humans("Generous-Corp/pulp", &["DanielRaffel"]);
    assert!(
        gate(&listed, &owner_review, &[]).allows(),
        "a listed human approves"
    );
    assert!(gate(&listed, &[], &owner_marker).allows());
    assert!(super::verdict_audit_events(HEAD, &owner_review, &[], &listed, None).is_empty());
    let app = [fleet(
        "2026-10-14T17:00:00Z",
        &format!("verdict: APPROVE head:{HEAD}"),
    )];
    assert!(
        gate(&unlisted, &[], &app).allows(),
        "the fleet App approves"
    );
    // An unlisted human's HOLD still stops the landing: it cannot approve.
    let held = [review("danielraffel", "CHANGES_REQUESTED")];
    assert!(!gate(&unlisted, &held, &app).allows());
    // A repository that lists reviewer_identities decides by that list alone.
    let strict = required(&["danielraffel"]);
    assert!(gate(&strict, &owner_review, &[]).allows());
}

#[test]
fn human_reviewers_is_read_from_the_base_config() {
    let table: toml::Table = "[auto_merge]\nhuman_reviewers = [\"DanielRaffel\", \"app/x[bot]\"]\n"
        .parse()
        .expect("toml");
    assert_eq!(
        ApprovalPolicy::from_table(&table).human_reviewer_logins,
        vec!["danielraffel".to_owned(), "x".to_owned()]
    );
    assert!(
        ApprovalPolicy::without_config("Generous-Corp/pulp", true)
            .human_reviewer_logins
            .is_empty(),
        "empty by default"
    );
}

#[test]
fn a_hold_after_the_approve_refuses_by_default() {
    let policy = by_default("danielraffel/tartci");
    let comments = [
        fleet(
            "2026-10-12T01:00:00Z",
            &format!("verdict: APPROVE head:{HEAD}"),
        ),
        fleet(
            "2026-10-12T02:00:00Z",
            &format!("verdict: HOLD head:{HEAD}"),
        ),
    ];
    assert!(!allows(&policy, &comments, None));
    // Control: the same pair the other way round approves.
    let reversed = [
        fleet(
            "2026-10-12T01:00:00Z",
            &format!("verdict: HOLD head:{HEAD}"),
        ),
        fleet(
            "2026-10-12T02:00:00Z",
            &format!("verdict: APPROVE head:{HEAD}"),
        ),
    ];
    assert!(allows(&policy, &reversed, None));
}

#[test]
fn the_arm_path_reads_the_author_before_judging() {
    let calls = std::cell::RefCell::new(Vec::<String>::new());
    let gh = |args: &[String]| -> Result<String, String> {
        let joined = args.join(" ");
        calls.borrow_mut().push(joined.clone());
        if joined.contains("contents/.shipyard/config.toml") {
            Err("gh: Not Found (HTTP 404)".to_owned())
        } else if joined.contains("/reviews") {
            Ok(String::new())
        } else if joined.contains("/comments") {
            Ok(json!({"id": 1, "login": "danielraffel", "type": "User",
                      "association": "OWNER", "at": "2026-10-12T01:00:00Z",
                      "body": format!("reviewed: {HEAD}")})
            .to_string())
        } else if joined.contains("repos/Generous-Corp/pulp/pulls/7 --jq .user.login") {
            Ok("danielraffel\n".to_owned())
        } else if joined.contains("repos/Generous-Corp/pulp/pulls/7 ") {
            Ok("main\n".to_owned())
        } else {
            Err(format!("unexpected gh call: {joined}"))
        }
    };
    let gate = super::with_host_review_default(true, || {
        super::evaluate(
            &gh,
            "Generous-Corp/pulp",
            7,
            HEAD,
            "main",
            &empty_queue(HEAD),
        )
    })
    .expect("readable");
    assert!(
        !gate.allows(),
        "the author's own marker must not count: {gate:?}"
    );
    assert!(
        calls
            .borrow()
            .iter()
            .any(|call| call.contains("repos/Generous-Corp/pulp/pulls/7 --jq .user.login")),
        "{:?}",
        calls.borrow()
    );
}

// ---------------------------------------------------------------------------
// Merge-only updates carry an approval: real crafted git histories (the cases
// of the reviewer's carry_probe), answered the way GitHub's compare does.
// ---------------------------------------------------------------------------

/// A scratch git repository with a pull request that adds a guard line (and
/// optionally changes a binary), its approved head, and a base commit since.
/// Past this size the fake GitHub, like GitHub, omits a text file's patch.
const LARGE_TEXT: u64 = 100_000;

struct History {
    dir: tempfile::TempDir,
    approved: String,
    blobs_unreadable: bool,
}

impl History {
    fn new(binary: bool) -> Self {
        Self::with(binary, false)
    }

    /// A history whose pull request also adds a one-line JSON text file
    /// larger than GitHub shows a patch for, when `large_text`.
    fn with(binary: bool, large_text: bool) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let history = Self {
            dir,
            approved: String::new(),
            blobs_unreadable: false,
        };
        history.git(&["init", "-q", "-b", "main"]);
        history.git(&["config", "user.email", "r@r"]);
        history.git(&["config", "user.name", "r"]);
        let lines: String = (1..=60)
            .map(|n| ["line ", &n.to_string(), "\n"].concat())
            .collect();
        history.write("guard.c", lines.as_bytes());
        if binary {
            history.write("x.bin", b"\x00\x01base-binary");
        }
        history.git(&["add", "-A"]);
        history.git(&["commit", "-qm", "base0"]);
        history.git(&["checkout", "-qb", "pr"]);
        let mut lines: Vec<String> = history.read("guard.c").lines().map(str::to_owned).collect();
        lines.insert(9, "  if (!authorized) return;".to_owned());
        history.write("guard.c", (lines.join("\n") + "\n").as_bytes());
        if binary {
            history.write("x.bin", b"\x00\x01pr-binary-v1");
        }
        if large_text {
            let rows = vec![r#"{"k": "v"}"#; 40_000].join(",");
            history.write(
                "fixture.json",
                format!(r#"{{"rows": [{rows}]}}"#).as_bytes(),
            );
            history.git(&["add", "fixture.json"]);
        }
        history.git(&["commit", "-qam", "pr: add guard"]);
        let approved = history.git(&["rev-parse", "HEAD"]);
        history.git(&["checkout", "-q", "main"]);
        history.write("other.txt", b"unrelated\n");
        history.git(&["add", "other.txt"]);
        history.git(&["commit", "-qm", "base1"]);
        Self {
            approved,
            ..history
        }
    }

    fn git(&self, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(self.dir.path())
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn write(&self, name: &str, data: &[u8]) {
        std::fs::write(self.dir.path().join(name), data).expect("write");
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(name)).expect("read")
    }

    /// Merge the base into the approved head, letting `edit` change the
    /// merge before it is committed.
    fn merge(&self, edit: impl FnOnce(&Self)) -> String {
        self.git(&["checkout", "-q", "-B", "head", &self.approved]);
        self.git(&["merge", "-q", "--no-commit", "main"]);
        edit(self);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", "Merge branch 'main' into pr"]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// GitHub's answer to a tree query: each `dN:object(expression:"<sha>:<dir>")`
    /// alias's entries, from `git ls-tree`.
    fn trees(&self, query: &str) -> String {
        const MARK: &str = ":object(expression:";
        let mut repository = serde_json::Map::new();
        for (at, _) in query.match_indices(MARK) {
            let alias: String = query[..at]
                .chars()
                .rev()
                .take_while(char::is_ascii_alphanumeric)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let rest = &query[at + MARK.len()..];
            let literal = &rest[..rest.find(')').expect("literal")];
            let expression: String = serde_json::from_str(literal).expect("string");
            let (commit, directory) = expression.split_once(':').expect("sha:dir");
            let spec = if directory.is_empty() {
                commit.to_owned()
            } else {
                expression.clone()
            };
            let entries: Vec<Value> = self
                .git(&["ls-tree", &spec])
                .lines()
                .map(|line| {
                    let (meta, name) = line.split_once('\t').expect("ls-tree line");
                    let mode = meta.split_whitespace().next().expect("mode");
                    json!({"name": name, "mode": u64::from_str_radix(mode, 8).expect("octal")})
                })
                .collect();
            repository.insert(alias, json!({"entries": entries}));
        }
        json!({"data": {"repository": repository}}).to_string()
    }

    /// GitHub's answer to the carry's reads of this repository.
    fn gh(&self, args: &[String]) -> Result<String, String> {
        let joined = args.join(" ");
        if joined.contains(":object(expression:") {
            return Ok(self.trees(&joined));
        }
        if joined.contains("contents/.shipyard/config.toml") {
            return Err("gh: Not Found (HTTP 404)".to_owned());
        }
        if joined.contains("/reviews") {
            return Ok(String::new());
        }
        if joined.contains("/comments") {
            return Ok(fleet(
                "2026-10-12T01:00:00Z",
                &format!("verdict: APPROVE head:{}", self.approved),
            )
            .to_string());
        }
        if joined.contains("pulls/7 --jq .user.login") {
            return Ok("shipyard-local[bot]\n".to_owned());
        }
        if joined.contains("pulls/7 --jq .base.ref") {
            return Ok("main\n".to_owned());
        }
        if let Some(sha) = args
            .iter()
            .find_map(|arg| arg.split_once("/git/blobs/").map(|(_, sha)| sha.to_owned()))
        {
            return self.blob(&sha);
        }
        let range = args
            .iter()
            .find_map(|arg| {
                arg.split_once("/compare/")
                    .map(|(_, range)| range.to_owned())
            })
            .ok_or_else(|| format!("unexpected gh call: {joined}"))?;
        let (x, y) = range.split_once("...").expect("a three-dot range");
        let base = self.git(&["merge-base", x, y]);
        let count = |range: &str| {
            self.git(&["rev-list", "--count", range])
                .parse::<u64>()
                .expect("count")
        };
        let (ahead, behind) = (
            count(&format!("{base}..{y}")),
            count(&format!("{base}..{x}")),
        );
        let status = match (ahead, behind) {
            (0, 0) => "identical",
            (_, 0) => "ahead",
            (0, _) => "behind",
            _ => "diverged",
        };
        if joined.ends_with("--jq .status") {
            return Ok(format!("{status}\n"));
        }
        if joined.contains("merge_base") {
            return Ok(self.compare_files_json(&base, y));
        }
        let commits: Vec<Value> = self
            .git(&[
                "rev-list",
                "--reverse",
                "--parents",
                &format!("{base}..{y}"),
            ])
            .lines()
            .map(|line| {
                let mut parts = line.split_whitespace();
                let sha = parts.next().expect("sha");
                json!({"sha": sha, "parents": parts.collect::<Vec<_>>()})
            })
            .collect();
        Ok(
            json!({"status": status, "behind_by": behind, "total_commits": ahead,
                  "commits": commits})
            .to_string(),
        )
    }

    /// `compare/<x>...<y>` file listing, as `compare_files` asks for it.
    fn compare_files_json(&self, base: &str, y: &str) -> String {
        let binary: Vec<String> = self
            .git(&["diff", "--numstat", base, y])
            .lines()
            .filter(|line| line.starts_with("-\t-\t"))
            .map(|line| line[4..].to_owned())
            .collect();
        let files: Vec<Value> = self
            .git(&["diff", "--raw", "--no-abbrev", "--no-renames", base, y])
            .lines()
            .map(|line| {
                let (meta, name) = line.split_once('\t').expect("raw line");
                let fields: Vec<&str> = meta.split_whitespace().collect();
                let status = match fields[4] {
                    "A" => "added",
                    "D" => "removed",
                    _ => "modified",
                };
                let sha = if fields[4] == "D" {
                    fields[2]
                } else {
                    fields[3]
                };
                json!({"filename": name, "status": status, "sha": sha,
                       "patch": !binary.iter().any(|b| b == name)
                           && self.shows_patch(sha, fields[4] == "D")})
            })
            .collect();
        json!({"merge_base": base, "files": files}).to_string()
    }

    /// GitHub sends no patch for a text file too large to show.
    fn shows_patch(&self, sha: &str, deleted: bool) -> bool {
        deleted
            || self
                .git(&["cat-file", "-s", sha])
                .parse::<u64>()
                .expect("size")
                <= LARGE_TEXT
    }

    /// `git/blobs/<sha>` answered from the repository.
    fn blob(&self, sha: &str) -> Result<String, String> {
        use base64::Engine as _;
        if self.blobs_unreadable {
            return Err("HTTP 502".to_owned());
        }
        let data = std::process::Command::new("git")
            .args(["cat-file", "blob", sha])
            .current_dir(self.dir.path())
            .output()
            .expect("git runs")
            .stdout;
        Ok(json!({"encoding": "base64",
                  "content": base64::engine::general_purpose::STANDARD.encode(&data),
                  "size": data.len()})
        .to_string())
    }

    fn gate(&self, head: &str) -> HeadGate {
        super::with_host_review_default(true, || {
            super::evaluate(
                &|args: &[String]| self.gh(args),
                "danielraffel/tartci",
                7,
                head,
                "main",
                &empty_queue(head),
            )
        })
        .expect("readable")
    }

    fn refusal(&self, head: &str) -> String {
        let pr = 7;
        super::merge_only_carry(
            &|args: &[String]| self.gh(args),
            "danielraffel/tartci",
            pr,
            &self.approved,
            head,
        )
        .expect_err("refused")
    }
}

#[test]
fn an_honest_merge_of_the_base_carries_the_approval() {
    let history = History::new(false);
    let head = history.merge(|_| {});
    match history.gate(&head) {
        HeadGate::Approved {
            approval: Approval::Carried { from_head, .. },
        } => assert_eq!(from_head, history.approved),
        gate => panic!("expected a carried approval, got {gate:?}"),
    }
}

#[test]
fn a_merge_that_moves_the_prs_guard_line_needs_a_new_verdict() {
    let history = History::new(false);
    let head = history.merge(|history| {
        let mut lines: Vec<String> = history
            .read("guard.c")
            .lines()
            .filter(|line| !line.contains("authorized"))
            .map(str::to_owned)
            .collect();
        lines.insert(48, "  if (!authorized) return;".to_owned());
        history.write("guard.c", (lines.join("\n") + "\n").as_bytes());
    });
    assert!(matches!(history.gate(&head), HeadGate::NotApproved { .. }));
    assert!(
        history.refusal(&head).contains("guard.c at"),
        "{}",
        history.refusal(&head)
    );
}

#[test]
fn a_merge_that_swaps_a_binary_the_pr_touches_needs_a_new_verdict() {
    let history = History::new(true);
    let head = history.merge(|history| history.write("x.bin", b"\x00\x01EVIL-binary"));
    assert!(matches!(history.gate(&head), HeadGate::NotApproved { .. }));
    assert_eq!(
        history.refusal(&head),
        "the pull request changes a binary file (x.bin)"
    );
}

/// Each file's mode is its own entry's, even when a sibling with another
/// mode is listed first, and a file its directory does not list has none.
#[test]
fn tree_modes_reads_each_files_own_entry() {
    let gh = |args: &[String]| -> Result<String, String> {
        let query = args.join(" ");
        assert!(query.contains("d0:object(expression:\"abc:\")"), "{query}");
        assert!(
            query.contains("d1:object(expression:\"abc:bin/sub\")"),
            "{query}"
        );
        Ok(json!({"data": {"repository": {
            "d0": {"entries": [{"name": "a.sh", "mode": 33261}, {"name": "guard.c", "mode": 33188}]},
            "d1": {"entries": [{"name": "run", "mode": 33261}, {"name": "tool.py", "mode": 33188}]}}}})
        .to_string())
    };
    let modes = super::tree_modes(
        &gh,
        "o/r",
        "abc",
        &["guard.c", "bin/sub/tool.py", "bin/sub/gone.txt"],
    )
    .expect("read");
    assert_eq!(modes.get("guard.c"), Some(&0o100_644));
    assert_eq!(modes.get("bin/sub/tool.py"), Some(&0o100_644));
    assert_eq!(modes.get("bin/sub/gone.txt"), None);
}

#[cfg(unix)]
#[test]
fn a_merge_that_only_flips_a_prs_file_mode_needs_a_new_verdict() {
    use std::os::unix::fs::PermissionsExt;
    let history = History::new(false);
    let head = history.merge(|history| {
        let path = history.dir.path().join("guard.c");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    });
    // The blob is unchanged; only the mode moved, which compare cannot show.
    assert_eq!(
        history.git(&["rev-parse", &format!("{head}:guard.c")]),
        history.git(&["rev-parse", &format!("{}:guard.c", history.approved)])
    );
    assert!(
        history
            .git(&["ls-tree", &head, "guard.c"])
            .starts_with("100755")
    );
    assert!(matches!(history.gate(&head), HeadGate::NotApproved { .. }));
    assert_eq!(
        history.refusal(&head),
        format!(
            "guard.c at {} does not keep the mode approved at {}",
            &head[..12],
            &history.approved[..12]
        )
    );
    // Control: the honest merge reads the same trees and carries.
    let honest = history.merge(|_| {});
    assert!(matches!(
        history.gate(&honest),
        HeadGate::Approved {
            approval: Approval::Carried { .. }
        }
    ));
}

#[test]
fn a_merge_that_adds_a_line_needs_a_new_verdict() {
    let history = History::new(false);
    let head = history.merge(|history| {
        let text = history.read("guard.c") + "  backdoor();\n";
        history.write("guard.c", text.as_bytes());
    });
    assert!(matches!(history.gate(&head), HeadGate::NotApproved { .. }));
}

#[test]
fn a_file_both_the_pr_and_the_base_changed_needs_a_new_verdict() {
    let history = History::new(false);
    history.git(&["checkout", "-q", "main"]);
    let text = format!("base head\n{}", history.read("guard.c"));
    history.write("guard.c", text.as_bytes());
    history.git(&["commit", "-qam", "base edit"]);
    // The merge keeps the PR's blob, quietly dropping the base's change.
    let approved = history.approved.clone();
    let head = history.merge(|history| {
        history.git(&["checkout", &approved, "--", "guard.c"]);
    });
    assert!(matches!(history.gate(&head), HeadGate::NotApproved { .. }));
    assert_eq!(
        history.refusal(&head),
        "guard.c was changed by both the pull request and the base"
    );
}

#[test]
fn a_commit_that_is_not_a_merge_needs_a_new_verdict() {
    let history = History::new(false);
    history.git(&["checkout", "-q", "-B", "single", &history.approved]);
    history.write("notes.txt", b"note\n");
    history.git(&["add", "-A"]);
    history.git(&["commit", "-qm", "single parent"]);
    let head = history.git(&["rev-parse", "HEAD"]);
    assert!(history.refusal(&head).contains("is not a merge"));
}

#[test]
fn a_head_that_does_not_descend_from_the_approved_one_needs_a_new_verdict() {
    let history = History::new(false);
    history.git(&["checkout", "-q", "-B", "rewritten", "main"]);
    history.git(&["checkout", &history.approved, "--", "guard.c"]);
    history.git(&["commit", "-qam", "pr: add guard (rebased)"]);
    let head = history.git(&["rev-parse", "HEAD"]);
    assert!(history.refusal(&head).contains("does not descend from"));
}

#[test]
fn a_hold_on_the_updated_head_is_not_overridden_by_a_carry() {
    let history = History::new(false);
    let head = history.merge(|_| {});
    let comments = [
        fleet(
            "2026-10-12T01:00:00Z",
            &format!("verdict: APPROVE head:{}", history.approved),
        ),
        fleet(
            "2026-10-12T02:00:00Z",
            &format!("verdict: HOLD head:{head}"),
        ),
    ];
    let gh = |args: &[String]| {
        if args.join(" ").contains("/comments") {
            Ok(comments
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"))
        } else {
            history.gh(args)
        }
    };
    let gate = super::with_host_review_default(true, || {
        super::evaluate(
            &gh,
            "danielraffel/tartci",
            7,
            &head,
            "main",
            &empty_queue(&head),
        )
    })
    .expect("readable");
    assert!(matches!(gate, HeadGate::NotApproved { .. }), "{gate:?}");
}

// ---------------------------------------------------------------------------
// The host switch itself, read from a child process so no test override
// stands in for it.
// ---------------------------------------------------------------------------

const PROBE: &str = "SHIPYARD_HOST_SWITCH_PROBE";

#[test]
fn host_switch_probe() {
    if std::env::var_os(PROBE).is_some() {
        println!("HOST_SWITCH={}", super::host_review_default());
    }
}

fn host_switch(env: &[(&str, &std::path::Path)], switch: Option<&str>) -> bool {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command
        .args([
            "--exact",
            "head_approval::tests::host_switch_probe",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE, "1")
        .env_remove(super::HOST_ENABLE_ENV)
        .env_remove("SHIPYARD_GHAPP_GUARDS_DIR");
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(switch) = switch {
        command.env(super::HOST_ENABLE_ENV, switch);
    }
    let output = command.output().expect("probe runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // libtest prints the probe's line after the test's name, on one line.
    match stdout
        .split_whitespace()
        .find_map(|word| word.strip_prefix("HOST_SWITCH="))
    {
        Some("true") => true,
        Some("false") => false,
        _ => panic!("probe printed no switch: {stdout}"),
    }
}

const OPT_OUT_RECORD: &str =
    r#"{"reason":"lab host","at":"2026-10-14T00:00:00Z","by":"shipyard test"}"#;

#[test]
fn the_host_switch_is_on_unless_the_opt_out_file_exists() {
    let empty = tempfile::tempdir().expect("tempdir");
    let opted = tempfile::tempdir().expect("tempdir");
    std::fs::write(opted.path().join(super::HOST_OPT_OUT_FILE), OPT_OUT_RECORD).expect("opt-out");
    let guards = "SHIPYARD_GHAPP_GUARDS_DIR";
    assert!(
        host_switch(&[(guards, empty.path())], None),
        "on by default"
    );
    assert!(
        !host_switch(&[(guards, opted.path())], None),
        "the opt-out file turns it off"
    );
    assert!(
        host_switch(&[(guards, empty.path())], Some("0")),
        "nothing in the environment turns it off"
    );
    assert!(
        host_switch(&[(guards, opted.path())], Some("1")),
        "1 forces it on for one process"
    );
    // Without the guards-dir variable, the opt-out is read from HOME.
    let home = tempfile::tempdir().expect("tempdir");
    let dir = home.path().join(".config/shipyard/guards");
    std::fs::create_dir_all(&dir).expect("dirs");
    assert!(host_switch(&[("HOME", home.path())], None));
    std::fs::write(dir.join(super::HOST_OPT_OUT_FILE), "").expect("opt-out");
    assert!(
        host_switch(&[("HOME", home.path())], None),
        "an empty file is not an opt-out"
    );
    std::fs::write(dir.join(super::HOST_OPT_OUT_FILE), OPT_OUT_RECORD).expect("opt-out");
    assert!(!host_switch(&[("HOME", home.path())], None));
}

// ---------------------------------------------------------------------------
// A session cannot approve its own landing.
// ---------------------------------------------------------------------------

fn stamped_approval(session: Option<&str>, agent: Option<&str>) -> Value {
    let stamp = session.map_or_else(String::new, |session| {
        let agent = agent.map_or_else(String::new, |agent| format!(" agent: {agent}"));
        format!("<!-- session: m5s/{session}{agent} -->\n")
    });
    json!({"id": 77, "login": "shipyard-local[bot]", "type": "Bot", "association": "CONTRIBUTOR",
           "at": "2026-10-14T01:00:00Z",
           "body": format!("Reviewed.\n{stamp}verdict: APPROVE head:{HEAD}")})
}

fn gate_for(comment: &Value, arming_agent: Option<&str>) -> HeadGate {
    super::with_arming_agent(arming_agent, || {
        decide_for_author(
            &by_default("Generous-Corp/pulp"),
            HEAD,
            &[],
            std::slice::from_ref(comment),
            &empty_queue(HEAD),
            false,
            Some("someone-else"),
        )
    })
}

#[test]
fn the_agent_that_approved_a_head_cannot_arm_it() {
    let gate = gate_for(
        &stamped_approval(Some("team-1"), Some("impl")),
        Some("impl"),
    );
    assert_eq!(
        gate,
        HeadGate::SelfApproved {
            head: HEAD.to_owned(),
            agent: "impl".to_owned()
        }
    );
    assert!(!gate.allows());
    assert!(gate.explain().contains("cannot approve its own landing"));
}

#[test]
fn a_shared_session_never_refuses_only_a_shared_agent_does() {
    let approved = |gate: HeadGate| matches!(gate, HeadGate::Approved { .. });
    // One Claude Code team: reviewer and lead share the session, not the agent.
    assert!(approved(gate_for(
        &stamped_approval(Some("team-1"), Some("reviewer")),
        Some("lead")
    )));
    // The same session with no agent id on either side, or on one side.
    assert!(approved(gate_for(
        &stamped_approval(Some("team-1"), None),
        None
    )));
    assert!(approved(gate_for(
        &stamped_approval(Some("team-1"), None),
        Some("lead")
    )));
    assert!(approved(gate_for(
        &stamped_approval(Some("team-1"), Some("reviewer")),
        None
    )));
    // Unstamped verdicts count as before.
    assert!(approved(gate_for(
        &stamped_approval(None, None),
        Some("lead")
    )));
    // Only the stamp's agent is compared: an arming agent id equal to the
    // approval's session string is still another agent.
    assert!(approved(gate_for(
        &stamped_approval(Some("team-1"), Some("reviewer")),
        Some("team-1")
    )));
}

#[test]
fn the_stamp_is_read_from_the_comment_body() {
    let body = "x\n<!-- session: m3/abc-123 agent: reviewer -->\nverdict: APPROVE head:a";
    assert_eq!(super::stamped_session(body), Some("abc-123".to_owned()));
    assert_eq!(super::stamped_agent(body), Some("reviewer".to_owned()));
    let plain = "x\n<!-- session: m3/abc-123 -->\nverdict: APPROVE head:a";
    assert_eq!(super::stamped_session(plain), Some("abc-123".to_owned()));
    assert_eq!(super::stamped_agent(plain), None);
    assert_eq!(super::stamped_session("verdict: APPROVE head:a"), None);
}

fn files(names: &[&str]) -> std::collections::BTreeMap<String, (String, String, bool)> {
    names
        .iter()
        .map(|name| {
            (
                (*name).to_owned(),
                ("modified".to_owned(), "1".repeat(40), true),
            )
        })
        .collect()
}

fn sources(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, previous)| ((*name).to_owned(), (*previous).to_owned()))
        .collect()
}

/// A rename counts under both names, on the pull request's side and on the
/// base's: a base edit to the file a pull request renames from would be
/// dropped by a merge that keeps the approved destination.
#[test]
fn a_base_change_to_a_rename_source_is_a_change_to_the_prs_file() {
    let pr = files(&["b.py"]);
    let pr_sources = sources(&[("b.py", "a.py")]);
    let none = sources(&[]);
    assert_eq!(
        super::touched_by_base(&pr, &[&pr_sources, &none], &files(&["a.py"]), &none),
        Some("a.py".to_owned())
    );
    // The base renamed the source away.
    assert_eq!(
        super::touched_by_base(
            &pr,
            &[&pr_sources, &pr_sources],
            &files(&["moved.py"]),
            &sources(&[("moved.py", "a.py")])
        ),
        Some("a.py".to_owned())
    );
    // Control: a base edit elsewhere touches nothing of the pull request's.
    assert_eq!(
        super::touched_by_base(
            &pr,
            &[&pr_sources, &pr_sources],
            &files(&["elsewhere.py"]),
            &none
        ),
        None
    );
    assert_eq!(
        super::touched_by_base(&pr, &[&none, &none], &files(&["b.py"]), &none),
        Some("b.py".to_owned())
    );
}

/// GitHub omits the patch of a one-line JSON fixture too large to show; its
/// blob has no NUL byte, so it is text and the approval carries.
#[test]
fn a_large_one_line_text_file_is_not_binary() {
    let history = History::with(false, true);
    let head = history.merge(|_| {});
    assert!(
        matches!(
            history.gate(&head),
            HeadGate::Approved {
                approval: Approval::Carried { .. }
            }
        ),
        "{:?}",
        history.gate(&head)
    );
    // Control: a real binary beside it still refuses.
    let history = History::with(true, true);
    let head = history.merge(|_| {});
    assert_eq!(
        history.refusal(&head),
        "the pull request changes a binary file (x.bin)"
    );
}

#[test]
fn a_rename_whose_source_changed_does_not_carry() {
    assert_eq!(
        super::rename_source_moved(&sources(&[("b.py", "a.py")]), &sources(&[("b.py", "c.py")])),
        Some("b.py".to_owned())
    );
    assert_eq!(
        super::rename_source_moved(&sources(&[("b.py", "a.py")]), &sources(&[])),
        Some("b.py".to_owned())
    );
    assert_eq!(
        super::rename_source_moved(&sources(&[("b.py", "a.py")]), &sources(&[("b.py", "a.py")])),
        None
    );
}

#[test]
fn an_unreadable_blob_refuses_without_calling_it_binary() {
    let history = History {
        blobs_unreadable: true,
        ..History::with(false, true)
    };
    let head = history.merge(|_| {});
    let refusal = history.refusal(&head);
    assert!(refusal.starts_with("cannot tell whether blob"), "{refusal}");
    assert!(!refusal.contains("binary file"), "{refusal}");
}

#[test]
fn a_blob_whose_content_is_short_of_its_size_is_unknown() {
    use base64::Engine as _;
    let blob = |data: &[u8], size: Option<u64>| {
        let mut value = json!({"encoding": "base64",
            "content": base64::engine::general_purpose::STANDARD.encode(data)});
        if let Some(size) = size {
            value["size"] = json!(size);
        }
        value
    };
    assert_eq!(super::binary_blob(&blob(b"{}{}", Some(4))), Ok(false));
    assert_eq!(super::binary_blob(&blob(b"\x00\x01", Some(2))), Ok(true));
    for (data, size) in [
        (&b"{}{}"[..], Some(5)),
        (&b""[..], Some(406_888)),
        (&b"text"[..], None),
    ] {
        assert!(
            super::binary_blob(&blob(data, size)).is_err(),
            "{data:?} of {size:?} must be unknown"
        );
    }
}
