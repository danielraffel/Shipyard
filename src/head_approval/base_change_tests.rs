use serde_json::{Value, json};

use super::{BASE_CHANGE_QUERY, base_changes, without_approvals_through};
use crate::head_approval::{ApprovalBlocker, HeadGate};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CASES: &str = include_str!("base_change_cases.json");

/// A fixture record as the Rust gate reads it.
fn record(case: &Value) -> Value {
    let approve = case["verdict"] == "APPROVE";
    if case["kind"] == "review" {
        json!({"id": case["id"], "at": case["at"], "commit_id": HEAD,
               "state": if approve { "APPROVED" } else { "CHANGES_REQUESTED" }})
    } else {
        json!({"id": case["id"], "at": case["at"],
               "body": format!("verdict: {} head:{HEAD}", if approve { "APPROVE" } else { "HOLD" })})
    }
}

/// Every shared case (`base_change_cases.json`, judged the same by the
/// Python guard's tests): which records survive, or an error.
#[test]
fn the_shared_cases_judge_as_written() {
    let cases: Value = serde_json::from_str(CASES).expect("cases");
    for case in cases["cases"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("name");
        let records: Vec<Value> = case["records"]
            .as_array()
            .expect("records")
            .iter()
            .map(record)
            .collect();
        let changes = base_changes(&case["answer"]);
        if case.get("error").is_some() {
            assert!(changes.is_err(), "{name}: {changes:?}");
            continue;
        }
        let changes = changes.unwrap_or_else(|error| panic!("{name}: {error}"));
        let kept = match changes.last() {
            Some(change) => without_approvals_through(records, change).0,
            None => records,
        };
        let kept: Vec<u64> = kept.iter().filter_map(|r| r["id"].as_u64()).collect();
        let want: Vec<u64> = case["kept"]
            .as_array()
            .expect("kept")
            .iter()
            .filter_map(Value::as_u64)
            .collect();
        assert_eq!(kept, want, "{name}");
    }
}

#[test]
fn the_query_reads_both_kinds_of_base_change_and_proves_completeness() {
    for part in [
        "BASE_REF_CHANGED_EVENT",
        "AUTOMATIC_BASE_CHANGE_SUCCEEDED_EVENT",
        "filteredCount",
        "hasNextPage",
        "baseRefName",
    ] {
        assert!(BASE_CHANGE_QUERY.contains(part), "{part}");
    }
}

fn timeline(nodes: &Value) -> Value {
    json!({"data": {"repository": {"pullRequest": {"baseRefName": "main", "timelineItems": {
        "filteredCount": nodes.as_array().map_or(0, Vec::len),
        "pageInfo": {"hasNextPage": false}, "nodes": nodes}}}}})
}

fn retarget(at: &str) -> Value {
    json!([{"__typename": "BaseRefChangedEvent", "createdAt": at,
            "previousRefName": "parent", "currentRefName": "main"}])
}

/// An App APPROVE of `HEAD` posted at `at`, stamped with `agent`.
fn approve(id: u64, at: &str, agent: &str) -> Value {
    json!({"id": id, "node_id": format!("IC_node{id}"), "login": "shipyard-local[bot]",
           "type": "Bot", "association": "CONTRIBUTOR", "at": at, "updated_at": at,
           "body": format!("reviewer: pulp-review\n<!-- session: Daniels-Mac-Studio-m3/r1 \
                            agent: {agent} -->\nverdict: APPROVE head:{HEAD}")})
}

/// A `gh` for `Generous-Corp/pulp#7` answering `comments` and the timeline
/// read with `answer`; `reads` counts timeline reads.
fn gh<'a>(
    comments: &'a [Value],
    answer: Result<Value, String>,
    reads: &'a std::cell::Cell<u32>,
) -> impl Fn(&[String]) -> Result<String, String> + 'a {
    move |args: &[String]| {
        let joined = args.join(" ");
        if joined.contains("contents/.shipyard/config.toml") {
            Ok("[auto_merge]\nreviewer_signers = [\"pulp-review\"]\n".to_owned())
        } else if joined.contains("BASE_REF_CHANGED_EVENT") {
            assert!(joined.contains("owner=Generous-Corp") && joined.contains("number=7"));
            reads.set(reads.get() + 1);
            answer.clone().map(|answer| answer.to_string())
        } else if joined.contains("lastEditedAt") {
            Ok(json!({"data": {"node": {"lastEditedAt": null}}}).to_string())
        } else if joined.contains("/reviews") {
            Ok(String::new())
        } else if joined.contains("/comments") {
            Ok(comments
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"))
        } else if joined.contains(".commit.committer.date") {
            Ok("2026-11-20T09:00:00Z\n".to_owned())
        } else {
            Err(format!("unexpected gh call: {joined}"))
        }
    }
}

type Judged = Result<(HeadGate, Option<ApprovalBlocker>), String>;

fn judge(
    comments: &[Value],
    answer: Result<Value, String>,
    reads: &std::cell::Cell<u32>,
) -> Judged {
    judge_as(comments, answer, reads, None)
}

fn judge_as(
    comments: &[Value],
    answer: Result<Value, String>,
    reads: &std::cell::Cell<u32>,
    agent: Option<&str>,
) -> Judged {
    let gh = gh(comments, answer, reads);
    crate::head_approval::with_host_sessions(None, || {
        crate::head_approval::with_arming_agent(agent, || {
            let policy = crate::head_approval::read_policy_repository_side(
                &gh,
                "Generous-Corp/pulp",
                "main",
            )?;
            crate::head_approval::evaluate_with_policy_for_author_with_diagnosis(
                &gh,
                "Generous-Corp/pulp",
                7,
                HEAD,
                &policy,
                &crate::head_approval::tests::empty_queue(HEAD),
                Some("someone"),
            )
            .map(|(gate, _, blocker)| (gate, blocker))
        })
    })
}

/// The stacked child: approved on its parent's branch, then retargeted to
/// `main` with the same head. The old APPROVE does not land it.
#[test]
fn a_retarget_after_the_approval_voids_it() {
    let reads = std::cell::Cell::new(0);
    let comments = [approve(7, "2026-11-20T10:00:00Z", "reviewer")];
    let (gate, blocker) = judge(
        &comments,
        Ok(timeline(&retarget("2026-11-20T11:00:00Z"))),
        &reads,
    )
    .expect("readable");
    assert!(!gate.allows(), "{gate:?}");
    assert!(matches!(gate, HeadGate::BaseChanged { .. }), "{gate:?}");
    assert!(gate.explain().contains("Re-review against the new base"));
    let blocker = blocker.expect("blocker");
    assert_eq!(blocker.token(), "approval_void:base_changed");
    assert_eq!(blocker.since(), Some("2026-11-20T11:00:00Z"));
    assert_eq!(reads.get(), 1);
}

/// An APPROVE posted after the retarget was given against the new base.
#[test]
fn a_retarget_before_the_approval_leaves_it_counting() {
    let reads = std::cell::Cell::new(0);
    let comments = [approve(7, "2026-11-20T12:00:00Z", "reviewer")];
    let (gate, blocker) = judge(
        &comments,
        Ok(timeline(&retarget("2026-11-20T11:00:00Z"))),
        &reads,
    )
    .expect("readable");
    assert!(gate.allows(), "{gate:?}");
    assert_eq!(reads.get(), 1, "the decision read the timeline");
    assert_eq!(blocker, None);
    // A re-review after the change clears the void.
    let comments = [
        approve(7, "2026-11-20T10:00:00Z", "reviewer"),
        approve(8, "2026-11-20T12:00:00Z", "reviewer"),
    ];
    let (gate, _) = judge(
        &comments,
        Ok(timeline(&retarget("2026-11-20T11:00:00Z"))),
        &reads,
    )
    .expect("readable");
    assert!(gate.allows(), "{gate:?}");
    assert_eq!(reads.get(), 2, "each decision read the timeline");
}

/// No base change: the approval counts exactly as before, and a pull request
/// with no APPROVE at all costs no timeline read.
#[test]
fn no_retarget_leaves_approval_counting_unchanged() {
    let reads = std::cell::Cell::new(0);
    let comments = [approve(7, "2026-11-20T10:00:00Z", "reviewer")];
    let (gate, blocker) = judge(&comments, Ok(timeline(&json!([]))), &reads).expect("readable");
    assert!(gate.allows(), "{gate:?}");
    assert_eq!(blocker, None);
    assert_eq!(reads.get(), 1);
    let (gate, blocker) = judge(&[], Err("must not be read".to_owned()), &reads).expect("readable");
    assert!(matches!(gate, HeadGate::NotApproved { .. }), "{gate:?}");
    assert_eq!(blocker.map(|b| b.token()), Some("no_verdict".to_owned()));
    assert_eq!(reads.get(), 1, "no APPROVE, no timeline read");
}

/// An incomplete or unreadable timeline is an error, never "no change".
#[test]
fn an_incomplete_timeline_fails_closed() {
    let reads = std::cell::Cell::new(0);
    let comments = [approve(7, "2026-11-20T10:00:00Z", "reviewer")];
    let mut further = timeline(&json!([]));
    further["data"]["repository"]["pullRequest"]["timelineItems"]["pageInfo"]["hasNextPage"] =
        json!(true);
    let mut miscounted = timeline(&json!([]));
    miscounted["data"]["repository"]["pullRequest"]["timelineItems"]["filteredCount"] = json!(1);
    let mut errors = timeline(&json!([]));
    errors["errors"] = json!([{"message": "rate limited"}]);
    for answer in [
        Ok(further),
        Ok(miscounted),
        Ok(errors),
        Ok(json!({"data": {"repository": {"pullRequest": null}}})),
        Err("gh: HTTP 502".to_owned()),
    ] {
        let judged = judge(&comments, answer.clone(), &reads);
        assert!(judged.is_err(), "{answer:?}: {judged:?}");
    }
}

/// Planted negative: after a retarget voids the reviewer's APPROVE, an
/// APPROVE the arming agent posted itself still never lands the head.
#[test]
fn a_self_approval_after_the_retarget_does_not_revive_it() {
    let reads = std::cell::Cell::new(0);
    let comments = [
        approve(7, "2026-11-20T10:00:00Z", "reviewer"),
        approve(8, "2026-11-20T12:00:00Z", "author-agent"),
    ];
    let (gate, _) = judge_as(
        &comments,
        Ok(timeline(&retarget("2026-11-20T11:00:00Z"))),
        &reads,
        Some("author-agent"),
    )
    .expect("readable");
    assert!(!gate.allows(), "{gate:?}");
}
