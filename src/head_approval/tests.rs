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
    ApprovalPolicy::without_config(repo)
}

fn fleet(at: &str, body: &str) -> Value {
    json!({"login": "shipyard-local[bot]", "type": "Bot", "at": at, "body": body})
}

#[test]
fn a_listed_repository_requires_review_when_its_config_is_silent() {
    for repo in super::REVIEW_REQUIRED_REPOS {
        let policy = by_default(repo);
        assert!(policy.required, "{repo}");
        assert!(policy.any_reviewer_but_author, "{repo}");
        assert!(policy.validate().is_ok(), "{repo}");
    }
    assert!(
        by_default("danielraffel/shipyard").required,
        "slugs are case-blind"
    );
    assert_eq!(by_default("someone/else"), ApprovalPolicy::default());
    let silent: toml::Table = "[auto_merge]\nqueue = true\n".parse().expect("toml");
    assert!(ApprovalPolicy::from_table_for_repo(&silent, "danielraffel/tartci").required);
}

#[test]
fn a_config_that_sets_the_key_decides_over_the_default() {
    let off: toml::Table = "[auto_merge]\narm_requires_head_approval = false\n"
        .parse()
        .expect("toml");
    assert_eq!(
        ApprovalPolicy::from_table_for_repo(&off, "Generous-Corp/pulp"),
        ApprovalPolicy::default()
    );
    let listed: toml::Table = "[auto_merge]\narm_requires_head_approval = true\n\
         reviewer_identities = [\"fable-graph\"]\n"
        .parse()
        .expect("toml");
    let policy = ApprovalPolicy::from_table_for_repo(&listed, "Generous-Corp/pulp");
    assert!(policy.required);
    assert!(!policy.any_reviewer_but_author);
    // The strict rule holds: the fleet App's marker does not count.
    let marker = [fleet("2026-10-12T01:00:00Z", &format!("reviewed: {HEAD}"))];
    assert!(
        !decide_for_author(&policy, HEAD, &[], &marker, &empty_queue(HEAD), false, None).allows()
    );
}

#[test]
fn a_missing_config_on_a_listed_repository_requires_review() {
    let missing =
        |_: &[String]| -> Result<String, String> { Err("gh: Not Found (HTTP 404)".to_owned()) };
    assert_eq!(
        read_policy(&missing, "danielraffel/tartci", "main"),
        Ok(by_default("danielraffel/tartci"))
    );
    assert!(
        super::read_policy_required(&missing, "Generous-Corp/vellum", "main")
            .expect("a listed repository needs no file")
            .required
    );
}

#[test]
fn the_fleet_apps_verdict_counts_on_the_fleet_apps_pull_request() {
    let policy = by_default("danielraffel/Shipyard");
    let queue = empty_queue(HEAD);
    let approve = [fleet(
        "2026-10-12T01:00:00Z",
        &format!("Delta review.\n\nverdict: APPROVE head:{HEAD}"),
    )];
    let gate = decide_for_author(
        &policy,
        HEAD,
        &[],
        &approve,
        &queue,
        false,
        Some("app/shipyard-local"),
    );
    assert!(gate.allows(), "{gate:?}");
    // No verdict, or one naming an older head, is refused and says what is needed.
    let gate = decide_for_author(
        &policy,
        HEAD,
        &[],
        &[],
        &queue,
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
    assert!(
        !decide_for_author(
            &policy,
            HEAD,
            &[],
            &older,
            &queue,
            false,
            Some("app/shipyard-local")
        )
        .allows()
    );
}

#[test]
fn a_distinguishable_author_cannot_approve_their_own_head() {
    let policy = by_default("Generous-Corp/pulp");
    let queue = empty_queue(HEAD);
    let own = [human(
        "DanielRaffel",
        "2026-10-12T01:00:00Z",
        &format!("reviewed: {HEAD}"),
    )];
    assert!(
        !decide_for_author(
            &policy,
            HEAD,
            &[],
            &own,
            &queue,
            false,
            Some("danielraffel")
        )
        .allows()
    );
    let other = [human(
        "fable-graph",
        "2026-10-12T01:00:00Z",
        &format!("reviewed: {HEAD}"),
    )];
    assert!(
        decide_for_author(
            &policy,
            HEAD,
            &[],
            &other,
            &queue,
            false,
            Some("danielraffel")
        )
        .allows()
    );
    let own_review = [review(
        "danielraffel",
        "User",
        "APPROVED",
        HEAD,
        "2026-10-12T01:00:00Z",
    )];
    assert!(
        !decide_for_author(
            &policy,
            HEAD,
            &own_review,
            &[],
            &queue,
            false,
            Some("danielraffel")
        )
        .allows()
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
    assert!(
        !decide_for_author(
            &policy,
            HEAD,
            &[],
            &comments,
            &empty_queue(HEAD),
            false,
            None
        )
        .allows()
    );
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
    assert!(
        decide_for_author(
            &policy,
            HEAD,
            &[],
            &reversed,
            &empty_queue(HEAD),
            false,
            None
        )
        .allows()
    );
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
                      "at": "2026-10-12T01:00:00Z", "body": format!("reviewed: {HEAD}")})
            .to_string())
        } else if joined.contains("repos/Generous-Corp/pulp/pulls/7 ") {
            Ok("danielraffel\n".to_owned())
        } else {
            Err(format!("unexpected gh call: {joined}"))
        }
    };
    let gate = super::evaluate(
        &gh,
        "Generous-Corp/pulp",
        7,
        HEAD,
        "main",
        &empty_queue(HEAD),
    )
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
