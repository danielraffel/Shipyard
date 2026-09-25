use std::cell::RefCell;

use serde_json::{Value, json};

use super::*;

fn annotation(title: &str, message: &str) -> Annotation {
    Annotation {
        title: title.to_owned(),
        message: message.to_owned(),
    }
}

const FAST: &str = r#"{"schema":"shipyard-test-tier/v1","tier":"fast","selector":"pr-fast","full_suite_runs_in":"merge_group"}"#;
const FULL: &str = r#"{"schema":"shipyard-test-tier/v1","tier":"full"}"#;
const REUSE: &str = r#"{"schema":"shipyard-receipt-decision/v1","target":"macos","verdict":"reuse","reason":"inputs unchanged","source_run_id":"36092791212","selected":812,"passed":800,"skipped":12,"inventory_count":2400}"#;
const REFUSE: &str = r#"{"schema":"shipyard-receipt-decision/v1","target":"macos","verdict":"refuse","reason":"narrowed tier","source_run_id":null,"selected":null,"passed":null,"skipped":null,"inventory_count":null}"#;

#[test]
fn fast_tier_parses_with_selector_and_destination() {
    assert_eq!(
        parse_test_tier(FAST),
        TierReading::Reported {
            tier: "fast".into(),
            selector: Some("pr-fast".into()),
            full_suite_runs_in: Some("merge_group".into()),
        }
    );
    assert_eq!(parse_test_tier(FULL).tier(), Some("full"));
}

#[test]
fn unknown_future_tier_is_kept_verbatim() {
    let reading =
        parse_test_tier(r#"{"schema":"shipyard-test-tier/v1","tier":"smoke-plus","extra":1}"#);
    assert_eq!(reading.tier(), Some("smoke-plus"));
    assert_eq!(reading.render(), "tier smoke-plus");
}

#[test]
fn malformed_and_wrong_schema_tiers_are_unparseable_not_dropped() {
    for (message, needle) in [
        ("not json", "not JSON"),
        ("[1,2]", "not a JSON object"),
        (r#"{"tier":"full"}"#, "missing `schema`"),
        (
            r#"{"schema":"shipyard-test-tier/v2","tier":"full"}"#,
            "shipyard-test-tier/v2",
        ),
        (
            r#"{"schema":"shipyard-test-tier/v1"}"#,
            "missing string field `tier`",
        ),
    ] {
        match parse_test_tier(message) {
            TierReading::Unparseable { raw, error } => {
                assert_eq!(raw, message);
                assert!(error.contains(needle), "{message}: {error}");
            }
            other => panic!("{message} parsed as {other:?}"),
        }
    }
    let reading = tier_from_annotations(&[annotation(TEST_TIER_TITLE, "{oops")]);
    assert!(
        reading.render().contains("UNPARSEABLE"),
        "{}",
        reading.render()
    );
}

#[test]
fn missing_tier_annotation_is_unknown_never_full() {
    let reading = tier_from_annotations(&[
        annotation("", "Process completed with exit code 0."),
        annotation("macos gate did not run the suite", "free text"),
    ]);
    assert!(matches!(reading, TierReading::Unknown { .. }));
    let verdict = summarize_tiers(&[CheckTier {
        check: "macos".into(),
        check_run_id: Some(1),
        status: None,
        conclusion: Some("success".into()),
        reading,
    }]);
    assert_eq!(verdict.tier, UNKNOWN_TIER);
    assert_eq!(verdict.unreported, 1);
    assert!(verdict.summary.contains("do not assume the full suite ran"));
}

#[test]
fn last_tier_annotation_wins() {
    let reading = tier_from_annotations(&[
        annotation(TEST_TIER_TITLE, FAST),
        annotation(TEST_TIER_TITLE, FULL),
    ]);
    assert_eq!(reading.tier(), Some("full"));
}

fn check(name: &str, reading: TierReading) -> CheckTier {
    CheckTier {
        check: name.into(),
        check_run_id: None,
        status: None,
        conclusion: None,
        reading,
    }
}

#[test]
fn narrowed_tier_wins_over_full_in_the_verdict() {
    let verdict = summarize_tiers(&[
        check("linux", parse_test_tier(FULL)),
        check("macos", parse_test_tier(FAST)),
        check("lint", TierReading::Unknown { reason: "x".into() }),
    ]);
    assert_eq!(verdict.tier, "fast");
    assert_eq!(verdict.selector.as_deref(), Some("pr-fast"));
    assert_eq!(verdict.full_suite_runs_in.as_deref(), Some("merge_group"));
    assert_eq!((verdict.reported, verdict.unreported), (2, 1));
    assert_eq!(
        verdict.summary,
        "validated on the fast tier (pr-fast) by macos; the full suite runs in merge_group, not \
         on this head"
    );
}

#[test]
fn all_full_is_fully_tested_and_names_unreported_checks() {
    let verdict = summarize_tiers(&[
        check("macos", parse_test_tier(FULL)),
        check("lint", TierReading::Unknown { reason: "x".into() }),
    ]);
    assert_eq!(verdict.tier, FULL_TIER);
    assert!(
        verdict
            .summary
            .starts_with("fully tested (tier full, reported by macos)")
    );
    assert!(
        verdict
            .summary
            .contains("1 other required check(s) reported no tier")
    );
}

#[test]
fn receipt_decisions_render_per_contract() {
    assert_eq!(
        parse_receipt_decision(REUSE, Some("protected-receipt-reuse")).render(),
        "macos: reused receipt from run 36092791212: 812 selected / 800 passed (12 skipped)"
    );
    assert_eq!(
        parse_receipt_decision(REFUSE, None).render(),
        "macos: validated in full: receipt refused because narrowed tier"
    );
    let numeric = parse_receipt_decision(
        r#"{"schema":"shipyard-receipt-decision/v1","target":"linux","verdict":"reuse","source_run_id":42,"selected":3,"passed":3,"skipped":0}"#,
        None,
    );
    assert!(
        numeric
            .render()
            .contains("run 42: 3 selected / 3 passed (0 skipped)")
    );
    let future = parse_receipt_decision(
        r#"{"schema":"shipyard-receipt-decision/v1","target":"linux","verdict":"defer","reason":"later"}"#,
        None,
    );
    assert_eq!(future.render(), "linux: verdict defer: later");
}

#[test]
fn malformed_receipt_decisions_are_reported_with_their_check() {
    for (message, needle) in [
        ("{", "not JSON"),
        (
            r#"{"schema":"shipyard-test-tier/v1","target":"macos","verdict":"reuse"}"#,
            "is not `shipyard-receipt-decision/v1`",
        ),
        (
            r#"{"schema":"shipyard-receipt-decision/v1","verdict":"reuse"}"#,
            "`target`",
        ),
        (
            r#"{"schema":"shipyard-receipt-decision/v1","target":"macos","verdict":"reuse","selected":-1}"#,
            "`selected`",
        ),
        (
            r#"{"schema":"shipyard-receipt-decision/v1","target":"macos","verdict":"reuse","source_run_id":[1]}"#,
            "`source_run_id`",
        ),
    ] {
        let decision = parse_receipt_decision(message, Some("gate"));
        match &decision {
            ReceiptDecision::Unparseable { error, check, raw } => {
                assert!(error.contains(needle), "{message}: {error}");
                assert_eq!(check.as_deref(), Some("gate"));
                assert_eq!(raw, message);
            }
            other @ ReceiptDecision::Parsed { .. } => panic!("{message} parsed as {other:?}"),
        }
        assert!(
            decision
                .render()
                .starts_with("receipt decision UNPARSEABLE on gate")
        );
    }
}

#[test]
fn json_field_names_are_stable() {
    let value = serde_json::to_value(parse_receipt_decision(REUSE, Some("gate"))).expect("json");
    assert_eq!(value["state"], "parsed");
    assert_eq!(value["verdict"], "reuse");
    assert_eq!(value["source_run_id"], "36092791212");
    assert_eq!(value["inventory_count"], 2400);
    let tier = serde_json::to_value(parse_test_tier(FAST)).expect("json");
    assert_eq!(tier["state"], "reported");
    assert_eq!(tier["full_suite_runs_in"], "merge_group");
}

/// A scripted `gh`: each call is matched by substring against the joined
/// argument vector, in order of the routes given.
struct FakeGh {
    routes: Vec<(&'static str, Result<Value, String>)>,
    calls: RefCell<Vec<String>>,
}

impl FakeGh {
    fn new(routes: Vec<(&'static str, Result<Value, String>)>) -> Self {
        Self {
            routes,
            calls: RefCell::new(Vec::new()),
        }
    }

    fn call(&self, args: &[String]) -> Result<String, String> {
        let joined = args.join(" ");
        self.calls.borrow_mut().push(joined.clone());
        for (needle, response) in &self.routes {
            if joined.contains(needle) {
                return response
                    .as_ref()
                    .map(Value::to_string)
                    .map_err(Clone::clone);
            }
        }
        Err(format!("HTTP 404: no route for {joined}"))
    }
}

fn pr_body(merge_commit: Option<&str>, queue: Option<&str>) -> Value {
    json!({"data": {"repository": {"pullRequest": {
        "headRefOid": "aaaa",
        "mergeCommit": merge_commit.map(|oid| json!({"oid": oid})),
        "mergeQueueEntry": queue.map(|oid| json!({"headCommit": {"oid": oid}})),
        "commits": {"nodes": [{"commit": {"oid": "aaaa", "statusCheckRollup": {"contexts": {
            "pageInfo": {"hasNextPage": false},
            "nodes": [
                {"__typename": "CheckRun", "databaseId": 10, "name": "macos", "status": "COMPLETED", "conclusion": "FAILURE", "isRequired": true},
                {"__typename": "CheckRun", "databaseId": 11, "name": "macos", "status": "COMPLETED", "conclusion": "SUCCESS", "isRequired": true},
                {"__typename": "CheckRun", "databaseId": 12, "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS", "isRequired": true},
                {"__typename": "CheckRun", "databaseId": 13, "name": "advisory", "status": "COMPLETED", "conclusion": "FAILURE", "isRequired": false},
                {"__typename": "StatusContext", "context": "ext/ci", "state": "SUCCESS", "isRequired": true}
            ]}}}}]}
    }}}})
}

#[test]
fn pr_head_on_fast_tier_is_green_but_not_full_validation() {
    let gh = FakeGh::new(vec![
        ("graphql", Ok(pr_body(None, None))),
        (
            "check-runs/11/annotations",
            Ok(json!([
                {"title": "", "message": "noise", "annotation_level": "warning"},
                {"title": TEST_TIER_TITLE, "message": FAST, "annotation_level": "notice"}
            ])),
        ),
        (
            "check-runs/12/annotations",
            Err("HTTP 403: forbidden".into()),
        ),
    ]);
    let signals = gather_pr(&|args| gh.call(args), "o/r", 5);
    assert_eq!(signals.required_state, "green", "{signals:?}");
    assert_eq!(signals.test_tier.len(), 3);
    assert_eq!(signals.test_tier_verdict.tier, "fast");
    let headline = signals.headline();
    assert!(
        headline.starts_with("PR head aaaa GREEN on the fast tier, NOT full validation"),
        "{headline}"
    );
    assert!(
        headline.contains("full suite runs in merge_group"),
        "{headline}"
    );
    let lint = signals
        .test_tier
        .iter()
        .find(|check| check.check == "lint")
        .expect("lint");
    assert!(lint.reading.render().contains("annotations unreadable"));
    let status = signals
        .test_tier
        .iter()
        .find(|check| check.check == "ext/ci")
        .expect("status context");
    assert!(status.reading.render().contains("commit status context"));
    // One GraphQL read plus one annotation read per required check run; the
    // superseded macos run (10) and the non-required run (13) are never read.
    let calls = gh.calls.borrow();
    assert_eq!(signals.api_calls, 3);
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(!calls.iter().any(|call| call.contains("check-runs/10/")));
    assert!(!calls.iter().any(|call| call.contains("check-runs/13/")));
}

#[test]
fn pr_head_without_any_tier_annotation_says_unknown() {
    let gh = FakeGh::new(vec![
        ("graphql", Ok(pr_body(None, None))),
        ("annotations", Ok(json!([]))),
    ]);
    let signals = gather_pr(&|args| gh.call(args), "o/r", 5);
    assert_eq!(signals.test_tier_verdict.tier, UNKNOWN_TIER);
    assert!(
        signals
            .headline()
            .contains("GREEN; test tier unknown: no required check published"),
        "{}",
        signals.headline()
    );
}

#[test]
fn unreadable_pr_leaves_everything_unknown() {
    let gh = FakeGh::new(vec![("graphql", Err("HTTP 502".into()))]);
    let signals = gather_pr(&|args| gh.call(args), "o/r", 5);
    assert_eq!(signals.required_state, "unknown");
    assert_eq!(signals.test_tier_verdict.tier, UNKNOWN_TIER);
    assert!(signals.warnings[0].contains("HTTP 502"));
}

fn merge_group_routes() -> Vec<(&'static str, Result<Value, String>)> {
    vec![
        ("graphql", Ok(pr_body(Some("mmmm"), None))),
        (
            "actions/runs?head_sha=mmmm&event=merge_group",
            Ok(json!({"workflow_runs": [{"id": 700, "check_suite_id": 70}]})),
        ),
        (
            "commits/mmmm/check-runs",
            Ok(json!({"total_count": 3, "check_runs": [
                {"id": 900, "name": "receipt-job", "check_suite": {"id": 70}, "output": {"annotations_count": 3}},
                {"id": 901, "name": "macos", "status": "completed", "conclusion": "success", "check_suite": {"id": 70}, "output": {"annotations_count": 1}},
                {"id": 902, "name": "push-only", "check_suite": {"id": 99}, "output": {"annotations_count": 5}},
                {"id": 903, "name": "quiet", "check_suite": {"id": 70}, "output": {"annotations_count": 0}}
            ]})),
        ),
        (
            "check-runs/900/annotations",
            Ok(json!([
                {"title": RECEIPT_DECISION_TITLE, "message": REUSE},
                {"title": RECEIPT_DECISION_TITLE, "message": REFUSE.replace("macos", "linux")},
                {"title": RECEIPT_DECISION_TITLE, "message": "{broken"}
            ])),
        ),
        (
            "check-runs/901/annotations",
            Ok(json!([{"title": TEST_TIER_TITLE, "message": FULL}])),
        ),
        ("annotations", Ok(json!([]))),
    ]
}

#[test]
fn merged_pr_reports_receipt_decisions_from_merge_group_runs_only() {
    let gh = FakeGh::new(merge_group_routes());
    let signals = gather_pr(&|args| gh.call(args), "o/r", 5);
    assert_eq!(signals.merge_groups.len(), 1);
    let group = &signals.merge_groups[0];
    assert_eq!(group.source, MergeGroupSource::MergeCommit);
    assert_eq!(group.status, "read");
    let rendered: Vec<String> = group
        .receipt_decisions
        .iter()
        .map(ReceiptDecision::render)
        .collect();
    assert_eq!(
        rendered,
        vec![
            "macos: reused receipt from run 36092791212: 812 selected / 800 passed (12 skipped)"
                .to_owned(),
            "linux: validated in full: receipt refused because narrowed tier".to_owned(),
            "receipt decision UNPARSEABLE on receipt-job (message is not JSON: key must be a \
             string at line 1 column 2): {broken"
                .to_owned(),
        ]
    );
    assert_eq!(group.test_tier.len(), 1);
    assert_eq!(group.test_tier[0].reading.tier(), Some("full"));
    assert!(
        group.headline().starts_with("mixed"),
        "{}",
        group.headline()
    );
    let calls = gh.calls.borrow();
    assert!(
        !calls.iter().any(|call| call.contains("check-runs/902/")),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("check-runs/903/")),
        "{calls:?}"
    );
}

#[test]
fn merge_commit_without_merge_group_runs_says_so() {
    let gh = FakeGh::new(vec![
        ("graphql", Ok(pr_body(Some("mmmm"), None))),
        ("actions/runs", Ok(json!({"workflow_runs": []}))),
        ("annotations", Ok(json!([]))),
    ]);
    let signals = gather_pr(&|args| gh.call(args), "o/r", 5);
    let group = &signals.merge_groups[0];
    assert_eq!(group.status, "no_merge_group_runs");
    assert!(group.headline().contains("no merge_group workflow runs"));
    assert!(
        !gh.calls
            .borrow()
            .iter()
            .any(|call| call.contains("commits/mmmm/check-runs"))
    );
}

#[test]
fn queued_pr_reads_its_live_merge_group() {
    let gh = FakeGh::new(vec![
        ("graphql", Ok(pr_body(None, Some("qqqq")))),
        ("actions/runs", Err("HTTP 404: Not Found".into())),
        ("annotations", Ok(json!([]))),
    ]);
    let signals = gather_pr(&|args| gh.call(args), "o/r", 5);
    let group = &signals.merge_groups[0];
    assert_eq!(group.source, MergeGroupSource::MergeQueueEntry);
    assert_eq!(group.sha, "qqqq");
    assert_eq!(group.status, "unreadable");
    assert!(
        group.headline().contains("HTTP 404"),
        "{}",
        group.headline()
    );
}

#[test]
fn graphql_contexts_yield_decisions_and_tiers() {
    let contexts = json!({"nodes": [
        {"__typename": "CheckRun", "name": "receipt-job", "databaseId": 1,
         "annotations": {"nodes": [{"title": RECEIPT_DECISION_TITLE, "message": REFUSE}]}},
        {"__typename": "CheckRun", "name": "macos", "databaseId": 2, "status": "COMPLETED", "conclusion": "SUCCESS",
         "annotations": {"nodes": [{"title": TEST_TIER_TITLE, "message": FULL}, {"title": "", "message": "x"}]}},
        {"__typename": "StatusContext", "context": "ext"}
    ]});
    let (decisions, tiers) = signals_from_graphql_contexts(Some(&contexts));
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        decisions[0].render(),
        "macos: validated in full: receipt refused because narrowed tier"
    );
    assert_eq!(tiers.len(), 1);
    assert_eq!(tiers[0].check, "macos");
    assert_eq!(tiers[0].conclusion.as_deref(), Some("success"));
}
