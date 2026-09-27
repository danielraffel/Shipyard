use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::*;

const REPO: &str = "o/r";
const WORKFLOW: &str = "main-health-detector.yml";

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("fixture time")
        .with_timezone(&Utc)
}

fn signal_annotation(status: &str, fix: Option<u64>) -> Value {
    json!({
        "title": SIGNAL_TITLE,
        "message": json!({
            "schema": SIGNAL_SCHEMA,
            "status": status,
            "safe_to_pause_queue": status == STATUS_POISONED,
            "tests": ["pulp-test-widgets"],
            "candidate_fix_pr": fix,
            "main_run_id": "900",
            "reason": "main failed",
        }).to_string(),
    })
}

fn runs(entries: &[(u64, &str)]) -> Value {
    json!({"workflow_runs": entries.iter().map(|(id, conclusion)| json!({
        "id": id,
        "conclusion": conclusion,
        "updated_at": "2026-09-27T10:00:00Z",
        "html_url": format!("https://example.test/runs/{id}"),
    })).collect::<Vec<_>>()})
}

/// A reader over canned responses keyed by API path (query string dropped).
fn reader(responses: BTreeMap<String, Value>) -> impl Fn(&[String]) -> Result<String, String> {
    move |args: &[String]| {
        let path = args.get(1).ok_or("no path")?;
        let key = path.split('?').next().unwrap_or(path);
        responses
            .get(key)
            .map(Value::to_string)
            .ok_or_else(|| format!("gh: Not Found (HTTP 404) {key}"))
    }
}

fn detector(run_list: Value, annotations: Value) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            format!("repos/{REPO}/actions/workflows/{WORKFLOW}/runs"),
            run_list,
        ),
        (
            format!("repos/{REPO}/actions/runs/7/jobs"),
            json!({"jobs": [{"id": 70}]}),
        ),
        (
            format!("repos/{REPO}/check-runs/70/annotations"),
            annotations,
        ),
    ])
}

fn finding(responses: BTreeMap<String, Value>) -> BaseHealthFinding {
    read_latest(&reader(responses), REPO, WORKFLOW)
}

#[test]
fn a_poisoned_base_with_a_named_fix_yields_jump_advice_and_commands() {
    let found = finding(detector(
        runs(&[(7, "failure")]),
        json!([{"title": "", "message": "noise"}, signal_annotation("poisoned", Some(8933))]),
    ));
    let advice = jump_advice(REPO, &found, at("2026-09-27T10:30:00Z")).expect("advice");
    assert_eq!(advice.pr, 8933);
    assert_eq!(
        advice.message,
        "main red: pulp-test-widgets, fix PR #8933, jump it"
    );
    assert_eq!(advice.main_run_id.as_deref(), Some("900"));
    assert_eq!(advice.signal_run_id, 7);
    assert!(advice.commands[2].contains("dequeuePullRequest"));
    assert!(advice.commands[3].contains("jump:true"));
    assert!(advice.commands[3].contains("expectedHeadOid"));
    assert!(advice.commands[0].contains("repos/o/r/pulls/8933"));
}

#[test]
fn no_advice_unless_poisoned_named_and_fresh() {
    let now = at("2026-09-27T10:30:00Z");
    for (status, fix) in [
        ("suspected", Some(1)),
        ("healthy", Some(1)),
        ("poisoned", None),
    ] {
        let found = finding(detector(
            runs(&[(7, "success")]),
            json!([signal_annotation(status, fix)]),
        ));
        assert!(matches!(found, BaseHealthFinding::Signal(_)), "{status}");
        assert!(jump_advice(REPO, &found, now).is_none(), "{status} {fix:?}");
    }
    let stale = finding(detector(
        runs(&[(7, "failure")]),
        json!([signal_annotation("poisoned", Some(1))]),
    ));
    assert!(jump_advice(REPO, &stale, at("2026-09-27T12:30:00Z")).is_none());
    assert!(
        jump_advice(REPO, &stale, now).is_some(),
        "control: the same signal fresh"
    );
}

#[test]
fn cancelled_runs_are_skipped_and_the_newest_evidence_run_decides() {
    let found = finding(detector(
        runs(&[(6, "cancelled"), (7, "success"), (5, "success")]),
        json!([signal_annotation("poisoned", Some(2))]),
    ));
    let BaseHealthFinding::Signal(observation) = found else {
        panic!("signal expected");
    };
    assert_eq!(observation.run_id, 7);

    // The newest evidence run published nothing: an older run is not consulted.
    let silent = finding(detector(runs(&[(7, "success"), (5, "success")]), json!([])));
    assert!(
        matches!(silent, BaseHealthFinding::NoSignal { .. }),
        "{silent:?}"
    );
}

#[test]
fn a_missing_detector_is_no_signal_and_a_failed_read_is_unknown() {
    let absent = finding(BTreeMap::new());
    assert!(
        matches!(absent, BaseHealthFinding::NoSignal { .. }),
        "{absent:?}"
    );

    let mut broken = detector(runs(&[(7, "success")]), json!([]));
    broken.remove(&format!("repos/{REPO}/actions/runs/7/jobs"));
    let failing = |args: &[String]| {
        if args[1].contains("/jobs") {
            Err("HTTP 502".to_owned())
        } else {
            reader(broken.clone())(args)
        }
    };
    assert!(matches!(
        read_latest(&failing, REPO, WORKFLOW),
        BaseHealthFinding::Unreadable { .. }
    ));
}

#[test]
fn a_signal_with_another_schema_is_refused() {
    let wrong = json!([{"title": SIGNAL_TITLE, "message": json!({"schema": "base-poison-signal/v2", "status": "poisoned"}).to_string()}]);
    let found = finding(detector(runs(&[(7, "failure")]), wrong));
    assert!(
        matches!(found, BaseHealthFinding::Unreadable { .. }),
        "{found:?}"
    );
}

#[test]
fn auto_jump_defaults_off_and_rejects_typos() {
    assert_eq!(AutoJump::parse(None), Ok(AutoJump::Off));
    assert_eq!(AutoJump::parse(Some("off")), Ok(AutoJump::Off));
    assert_eq!(AutoJump::parse(Some("dry-run")), Ok(AutoJump::DryRun));
    assert_eq!(AutoJump::parse(Some("on")), Ok(AutoJump::On));
    assert!(AutoJump::parse(Some("yes")).is_err());
    assert!(AutoJump::parse(Some("true")).is_err());
}

fn decision(mode: &str, main: &str, pr: u64, outcome: &str) -> JumpDecision {
    JumpDecision {
        decided_at: "2026-09-27T10:00:00Z".to_owned(),
        repo: REPO.to_owned(),
        mode: mode.to_owned(),
        pr,
        tests: vec![],
        signal_run_id: 7,
        main_run_id: Some(main.to_owned()),
        outcome: outcome.to_owned(),
    }
}

#[test]
fn one_episode_is_recorded_once_but_a_failed_live_jump_may_retry() {
    let previous = vec![decision("dry_run", "900", 8933, "would_jump")];
    assert!(already_recorded(
        &previous,
        &decision("dry_run", "900", 8933, "would_jump")
    ));
    assert!(!already_recorded(
        &previous,
        &decision("dry_run", "901", 8933, "would_jump")
    ));
    assert!(!already_recorded(
        &previous,
        &decision("dry_run", "900", 8934, "would_jump")
    ));
    assert!(!already_recorded(
        &previous,
        &decision("on", "900", 8933, "jumped")
    ));

    let failed = vec![decision("on", "900", 8933, "failed: enqueue: HTTP 422")];
    assert!(!already_recorded(
        &failed,
        &decision("on", "900", 8933, "jumped")
    ));
}
