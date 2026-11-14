use std::cell::RefCell;

use serde_json::{Value, json};

use super::*;
use crate::head_approval::{
    ApprovalPolicy, HeadGate, PolicyReadError, evaluate_with_policy_for_author_with_events,
    read_policy_required, with_guards_dir,
};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const POLICY_SHA: &str = "cccccccccccccccccccccccccccccccccccccccc";
const GUARD_APP: u64 = 4242;
const SHIPYARD_LOCAL: u64 = 3_878_000;

fn run(id: u64) -> Value {
    json!({
        "id": id,
        "name": DEFAULT_GUARD_CHECK,
        "head_sha": HEAD,
        "status": "completed",
        "conclusion": "success",
        "external_id": json!({
            "base_sha": OTHER, "head_sha": HEAD, "policy_sha": POLICY_SHA,
            "classification": "routine",
        }).to_string(),
        "app": {"id": GUARD_APP, "slug": "fleet-guard"},
    })
}

fn with(mut run: Value, key: &str, value: Value) -> Value {
    if key.contains('/') {
        *run.pointer_mut(key).expect("pointer") = value;
    } else {
        run[key] = value;
    }
    run
}

fn judged(runs: &[Value]) -> (Judgement, Vec<String>) {
    let mut events = Vec::new();
    let judgement = judge(runs, HEAD, DEFAULT_GUARD_CHECK, GUARD_APP, &mut events);
    (judgement, events)
}

fn section(text: &str) -> toml::Table {
    text.parse::<toml::Table>().expect("toml")["auto_merge"]
        .as_table()
        .expect("table")
        .clone()
}

#[test]
fn comments_stay_the_default_and_guard_needs_a_usable_app_id() {
    assert_eq!(source_from_section(None), Ok(ApprovalSource::Comments));
    assert_eq!(
        source_from_section(Some(&section(
            "[auto_merge]\napproval_source = \"comments\""
        ))),
        Ok(ApprovalSource::Comments)
    );
    assert_eq!(
        source_from_section(Some(&section(
            "[auto_merge]\napproval_source = \"guard\"\nguard_app_id = 4242"
        ))),
        Ok(ApprovalSource::Guard {
            check: DEFAULT_GUARD_CHECK.to_owned(),
            app_id: GUARD_APP
        })
    );
    for bad in [
        "approval_source = \"guard\"",
        "approval_source = \"guard\"\nguard_app_id = 0",
        "approval_source = \"guard\"\nguard_app_id = \"4242\"",
        "approval_source = \"guard\"\nguard_app_id = 15368",
        "approval_source = \"guard\"\nguard_app_id = 4242\nguard_admission_check = \" \"",
        "approval_source = \"either\"",
    ] {
        assert!(
            source_from_section(Some(&section(&format!("[auto_merge]\n{bad}")))).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn a_malformed_guard_policy_refuses_rather_than_falling_back_to_comments() {
    let table: toml::Table = "[auto_merge]\napproval_source = \"guard\""
        .parse()
        .expect("toml");
    let policy = ApprovalPolicy::from_table(&table);
    assert!(matches!(
        policy.source,
        ApprovalSource::Misconfigured { .. }
    ));
    assert!(policy.required, "guard mode cannot be switched off");
}

#[test]
fn guard_mode_requires_authorization_whatever_arm_requires_head_approval_says() {
    let table: toml::Table = "[auto_merge]\narm_requires_head_approval = false\napproval_source = \"guard\"\nguard_app_id = 4242"
        .parse()
        .expect("toml");
    assert!(ApprovalPolicy::from_table(&table).required);
}

#[test]
fn a_bound_successful_run_from_the_guard_app_admits() {
    let (judgement, events) = judged(&[run(10)]);
    assert_eq!(
        judgement,
        Judgement::Admitted {
            check_run_id: 10,
            policy_sha: POLICY_SHA.to_owned(),
            classification: "routine".to_owned(),
        }
    );
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn runs_from_any_other_app_never_count() {
    for app in [
        json!({"id": SHARED_ACTIONS_APP_ID, "slug": "github-actions"}),
        json!({"id": SHIPYARD_LOCAL, "slug": "shipyard-local"}),
        // Misconfigured with the shared App's id: its slug still refuses it.
        json!({"id": GUARD_APP, "slug": "shipyard-local"}),
        json!(null),
    ] {
        let (judgement, events) = judged(&[with(run(10), "app", app.clone())]);
        assert!(matches!(judgement, Judgement::Waiting { .. }), "{app}");
        assert!(
            events
                .first()
                .is_some_and(|event| event.starts_with("excluded_admission:")),
            "{events:?}"
        );
    }
}

#[test]
fn the_newest_counting_run_decides() {
    let failed = with(run(11), "conclusion", json!("failure"));
    assert!(matches!(
        judged(&[run(10), failed.clone()]).0,
        Judgement::Refused {
            check_run_id: 11,
            ..
        }
    ));
    assert!(matches!(
        judged(&[failed, run(12)]).0,
        Judgement::Admitted {
            check_run_id: 12,
            ..
        }
    ));
    let running = with(
        with(run(13), "status", json!("in_progress")),
        "conclusion",
        Value::Null,
    );
    assert!(matches!(
        judged(&[run(12), running]).0,
        Judgement::Waiting { .. }
    ));
}

#[test]
fn an_unbound_record_counts_as_absent() {
    for external in [
        Value::Null,
        json!(""),
        json!("not json"),
        json!(
            json!({"head_sha": OTHER, "policy_sha": POLICY_SHA, "classification": "routine"})
                .to_string()
        ),
        json!(
            json!({"head_sha": HEAD, "policy_sha": "abc", "classification": "routine"}).to_string()
        ),
        json!(json!({"head_sha": HEAD, "policy_sha": POLICY_SHA}).to_string()),
    ] {
        let (judgement, _) = judged(&[with(run(10), "external_id", external.clone())]);
        assert!(matches!(judgement, Judgement::Waiting { .. }), "{external}");
    }
}

#[test]
fn a_run_on_another_head_or_of_another_name_is_ignored() {
    assert!(matches!(
        judged(&[with(run(10), "head_sha", json!(OTHER))]).0,
        Judgement::Waiting { .. }
    ));
    assert!(matches!(
        judged(&[with(run(10), "name", json!("review-verdict"))]).0,
        Judgement::Waiting { .. }
    ));
    assert!(matches!(judged(&[]).0, Judgement::Waiting { .. }));
}

/// A fake GitHub: the base config, an approving reviewer comment, and the
/// head's admission check runs. Every call is recorded.
fn github<'a>(
    config: &'a str,
    runs: &'a [Value],
    calls: &'a RefCell<Vec<String>>,
) -> impl Fn(&[String]) -> Result<String, String> + 'a {
    move |args: &[String]| {
        let joined = args.join(" ");
        calls.borrow_mut().push(joined.clone());
        if joined.contains("contents/.shipyard/config.toml") {
            return Ok(config.to_owned());
        }
        if joined.contains("/check-runs?check_name=fleet-guard%2Fadmission") {
            return Ok(runs
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"));
        }
        if joined.contains("/issues/7/comments") {
            return Ok(
                json!({"id": 1, "login": "shipyard-local[bot]", "type": "Bot",
                "association": "NONE", "at": "2026-11-04T00:00:00Z",
                "body": format!("verdict: APPROVE head:{HEAD}"),
                "performed_via_github_app_id": SHIPYARD_LOCAL})
                .to_string(),
            );
        }
        if joined.contains("/pulls/7/reviews") {
            return Ok(String::new());
        }
        Err(format!("unexpected: {joined}"))
    }
}

const GUARD_CONFIG: &str = "[auto_merge]\napproval_source = \"guard\"\nguard_app_id = 4242\n";
const COMMENTS_CONFIG: &str = "[auto_merge]\narm_requires_head_approval = true\n";

fn gate_for(config: &str, runs: &[Value]) -> (HeadGate, Vec<String>) {
    let calls = RefCell::new(Vec::new());
    let run_gh = github(config, runs, &calls);
    let policy = read_policy_required(&run_gh, "owner/repo", "main").expect("policy");
    let (gate, _events) = evaluate_with_policy_for_author_with_events(
        &run_gh,
        "owner/repo",
        7,
        HEAD,
        &policy,
        &json!({}),
        Some("author"),
    )
    .expect("gate");
    drop(run_gh);
    (gate, calls.into_inner())
}

#[test]
fn guard_mode_ignores_a_verdict_comment_and_admits_only_on_the_guard_run() {
    let (gate, calls) = gate_for(GUARD_CONFIG, &[]);
    assert!(matches!(gate, HeadGate::NotAdmitted { .. }), "{gate:?}");
    assert!(
        !calls.iter().any(|call| call.contains("/comments")),
        "guard mode reads no comment: {calls:?}"
    );
    let (gate, _) = gate_for(GUARD_CONFIG, &[run(10)]);
    assert!(
        matches!(
            &gate,
            HeadGate::Approved {
                approval: crate::head_approval::Approval::Admission {
                    check_run_id: 10,
                    ..
                }
            }
        ),
        "{gate:?}"
    );
    let forged = with(
        run(11),
        "app",
        json!({"id": SHIPYARD_LOCAL, "slug": "shipyard-local"}),
    );
    let (gate, _) = gate_for(GUARD_CONFIG, &[forged]);
    assert!(!gate.allows(), "{gate:?}");
}

#[test]
fn a_host_that_requires_guard_refuses_a_comment_mode_base() {
    let dir = tempfile::tempdir().expect("temp");
    std::fs::write(dir.path().join(HOST_REQUIRE_FILE), "1\n").expect("write");
    let (gate, _) = with_guards_dir(dir.path(), || gate_for(COMMENTS_CONFIG, &[run(10)]));
    assert!(matches!(gate, HeadGate::NotAdmitted { .. }), "{gate:?}");
}

#[test]
fn a_guard_config_without_an_app_id_is_an_unreadable_policy() {
    let calls = RefCell::new(Vec::new());
    let run_gh = github("[auto_merge]\napproval_source = \"guard\"\n", &[], &calls);
    assert!(matches!(
        read_policy_required(&run_gh, "owner/repo", "main"),
        Err(PolicyReadError::Unreadable(_))
    ));
}

/// A queue-state response in which the merge queue removed `head` `count`
/// times for a failure (never `merged`), in GitHub's shape: `beforeCommit` is
/// the merge-group commit, with `head` as its second parent.
fn ejected(head: &str, count: usize) -> Value {
    let nodes = (0..count)
        .map(|index| {
            json!({
                "__typename": "RemovedFromMergeQueueEvent",
                "createdAt": format!("2026-11-04T0{index}:30:00Z"),
                "reason": "failed_checks",
                "beforeCommit": {
                    "oid": format!("{index:040x}"),
                    "parents": {"nodes": [
                        {"oid": "cccccccccccccccccccccccccccccccccccccccc"},
                        {"oid": head},
                    ]},
                },
            })
        })
        .collect::<Vec<_>>();
    json!({"data": {"repository": {"pullRequest": {"timelineItems": {"nodes": nodes}}}}})
}

fn gate_with_queue(queue: &Value, latest_was_interruption: bool) -> HeadGate {
    let calls = RefCell::new(Vec::new());
    let runs = [run(10)];
    let run_gh = github(GUARD_CONFIG, &runs, &calls);
    let policy = read_policy_required(&run_gh, "owner/repo", "main").expect("policy");
    if latest_was_interruption {
        return crate::head_approval::evaluate_excusing_with_policy(
            &run_gh,
            "owner/repo",
            7,
            HEAD,
            &policy,
            queue,
            true,
        )
        .expect("gate");
    }
    evaluate_with_policy_for_author_with_events(
        &run_gh,
        "owner/repo",
        7,
        HEAD,
        &policy,
        queue,
        Some("author"),
    )
    .expect("gate")
    .0
}

/// The ejection cap is the steward's loop-breaker, not an approval rule, so
/// it holds in guard mode: an admitted head the queue removed the cap's
/// number of times is not admitted for another enqueue, and a fresh
/// admission run does not lift it (only a new head does).
#[test]
fn an_admitted_head_ejected_to_the_cap_is_not_re_enqueued() {
    let cap = crate::head_approval::EJECTION_CAP;
    let gate = gate_with_queue(&ejected(HEAD, cap), false);
    assert!(
        matches!(&gate, HeadGate::EjectionCap { ejections, .. } if ejections.len() == cap),
        "{gate:?}"
    );
    assert!(!gate.allows());
    // Control: one removal short of the cap, the same admission admits.
    assert!(gate_with_queue(&ejected(HEAD, cap - 1), false).allows());
    // Removals of another head do not count against this one.
    assert!(gate_with_queue(&ejected(OTHER, cap), false).allows());
}

#[test]
fn an_interruption_excuses_the_newest_removal_in_guard_mode_too() {
    let cap = crate::head_approval::EJECTION_CAP;
    assert!(gate_with_queue(&ejected(HEAD, cap), true).allows());
    assert!(!gate_with_queue(&ejected(HEAD, cap + 1), true).allows());
}

/// A fake GitHub for a comment-mode base: `config` and `comments` answer as
/// given, and any admission check run on the head is a green one from the
/// guard App, so a gate that consulted it would be visibly different.
fn comment_mode_github<'a>(
    config: &'a str,
    comments: &'a [Value],
    calls: &'a RefCell<Vec<String>>,
) -> impl Fn(&[String]) -> Result<String, String> + 'a {
    move |args: &[String]| {
        let joined = args.join(" ");
        calls.borrow_mut().push(joined.clone());
        if joined.contains("contents/.shipyard/config.toml") {
            return Ok(config.to_owned());
        }
        if joined.contains("/check-runs") {
            return Ok(run(10).to_string());
        }
        if joined.contains("/issues/7/comments") {
            return Ok(comments
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"));
        }
        if joined.contains("/pulls/7/reviews") {
            return Ok(String::new());
        }
        if joined.contains(&format!("commits/{HEAD}")) {
            return Ok("2026-11-01T00:00:00Z".to_owned());
        }
        Err(format!("unexpected: {joined}"))
    }
}

/// Landing this module changes nothing for a base that has not opted in: a
/// base with no `approval_source` (or `approval_source = "comments"`) gets
/// exactly the comment rule's decision, never reads a check run, and is not
/// admitted by a green admission run alone.
#[test]
fn an_unconfigured_base_behaves_exactly_as_the_comment_rule() {
    let reviewer = "shipyard-review[bot]";
    let comment = |verdict: &str| {
        json!({"id": 5, "login": reviewer, "type": "Bot", "association": "NONE",
            "at": "2026-11-02T00:00:00Z", "body": format!("verdict: {verdict} head:{HEAD}"),
            "performed_via_github_app_id": 99})
    };
    let base = "[auto_merge]\narm_requires_head_approval = true\nreviewer_identities = [\"shipyard-review[bot]\"]\n";
    let explicit = format!("{base}approval_source = \"comments\"\n");
    let silent = "[project]\nname = \"x\"\n";
    for config in [base, explicit.as_str(), silent] {
        for comments in [vec![], vec![comment("APPROVE")], vec![comment("HOLD")]] {
            let calls = RefCell::new(Vec::new());
            let run_gh = comment_mode_github(config, &comments, &calls);
            let policy = read_policy_required(&run_gh, "owner/repo", "main").expect("policy");
            assert_eq!(policy.source, ApprovalSource::Comments, "{config}");
            let (gate, _) = evaluate_with_policy_for_author_with_events(
                &run_gh,
                "owner/repo",
                7,
                HEAD,
                &policy,
                &json!({}),
                Some("author"),
            )
            .expect("gate");
            let expected = crate::head_approval::decide_for_author(
                &policy,
                HEAD,
                &[],
                &comments,
                &json!({}),
                false,
                Some("author"),
            );
            assert_eq!(gate, expected, "{config} {comments:?}");
            assert!(
                !matches!(
                    &gate,
                    HeadGate::Approved {
                        approval: crate::head_approval::Approval::Admission { .. }
                    }
                ),
                "{gate:?}"
            );
            drop(run_gh);
            let calls = calls.into_inner();
            assert!(
                !calls.iter().any(|call| call.contains("/check-runs")),
                "a comment-mode base never reads a check run: {calls:?}"
            );
        }
    }
    // Control: the fixture distinguishes the outcomes it claims to compare.
    let calls = RefCell::new(Vec::new());
    let approve = [comment("APPROVE")];
    let run_gh = comment_mode_github(base, &approve, &calls);
    let policy = read_policy_required(&run_gh, "owner/repo", "main").expect("policy");
    let (gate, _) = evaluate_with_policy_for_author_with_events(
        &run_gh,
        "owner/repo",
        7,
        HEAD,
        &policy,
        &json!({}),
        Some("author"),
    )
    .expect("gate");
    assert!(gate.allows(), "{gate:?}");
}
