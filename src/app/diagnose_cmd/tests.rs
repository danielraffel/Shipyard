use chrono::{Duration, TimeZone, Utc};
use serde_json::json;

use super::*;
use crate::pr_watch::ledger::{Ledger, LedgerEntry};

fn rollup(nodes: &Value, more: bool) -> Value {
    json!({"data": {"repository": {"pullRequest": {
        "headRefOid": "abc",
        "commits": {"nodes": [{"commit": {"oid": "abc", "statusCheckRollup": {"contexts": {
            "pageInfo": {"hasNextPage": more},
            "nodes": nodes
        }}}}]}
    }}}})
}

#[test]
fn the_rollup_names_red_required_actions_jobs_and_nothing_else() {
    let nodes = json!([
        {"__typename": "CheckRun", "databaseId": 11, "name": "macos", "status": "COMPLETED",
         "conclusion": "FAILURE", "isRequired": true,
         "detailsUrl": "https://github.com/o/r/actions/runs/500/job/11"},
        {"__typename": "CheckRun", "databaseId": 12, "name": "Linux", "status": "COMPLETED",
         "conclusion": "FAILURE", "isRequired": false,
         "detailsUrl": "https://github.com/o/r/actions/runs/500/job/12"},
        {"__typename": "CheckRun", "databaseId": 13, "name": "drift-fast", "status": "COMPLETED",
         "conclusion": "SUCCESS", "isRequired": true,
         "detailsUrl": "https://github.com/o/r/actions/runs/501/job/13"},
        {"__typename": "CheckRun", "databaseId": 14, "name": "external", "status": "COMPLETED",
         "conclusion": "FAILURE", "isRequired": true, "detailsUrl": "https://ci.example/1"},
        {"__typename": "StatusContext", "context": "legacy", "state": "ERROR", "isRequired": true}
    ]);
    let parsed = parse_pr_rollup(&rollup(&nodes, false)).expect("parse");
    assert_eq!(parsed.head, "abc");
    assert_eq!(
        parsed.required,
        vec!["drift-fast", "external", "legacy", "macos"]
    );
    assert_eq!(
        parsed.failing,
        vec![RedContext {
            name: "macos".to_owned(),
            run_id: Some(500),
            state: "failure".to_owned()
        }]
    );
    let names: Vec<&str> = parsed
        .unreadable
        .iter()
        .map(|u| u.context.as_str())
        .collect();
    assert_eq!(names, vec!["external", "legacy"]);
}

#[test]
fn a_rollup_past_one_page_says_so() {
    let parsed = parse_pr_rollup(&rollup(&json!([]), true)).expect("parse");
    assert_eq!(parsed.unreadable.len(), 1);
    assert!(parsed.unreadable[0].reason.contains("more than 100"));
}

fn job(id: i64, name: &str, conclusion: &str, runnerless: bool) -> Job {
    Job {
        id,
        name: name.to_owned(),
        conclusion: Some(conclusion.to_owned()),
        status: Some("completed".to_owned()),
        runner_name: Some(if runnerless { "" } else { "r" }.to_owned()),
        run_id: Some(7),
        steps: Some(if runnerless {
            Vec::new()
        } else {
            vec![diagnose::Step::default()]
        }),
        ..Job::default()
    }
}

#[test]
fn annotations_are_read_for_cancelled_required_jobs_and_starved_siblings_only() {
    let target = Target {
        head_sha: "abc".to_owned(),
        jobs: vec![
            job(1, "macos", "failure", false),
            job(2, "classify", "cancelled", true),
            job(3, "drift-fast", "cancelled", false),
            job(4, "Linux", "cancelled", false),
            job(5, "version", "success", false),
        ],
        required: vec![
            "macos".to_owned(),
            "drift-fast".to_owned(),
            "version".to_owned(),
        ],
        unreadable: Vec::new(),
    };
    assert_eq!(annotation_targets(&target), vec![2, 3]);
}

fn entry(pr: u64, kind: FlagKind, route: DigestRoute, seen: DateTime<Utc>) -> LedgerEntry {
    LedgerEntry {
        pr,
        kind,
        key: "t".to_owned(),
        title: String::new(),
        url: String::new(),
        verdict: String::new(),
        evidence: String::new(),
        head_sha: String::new(),
        first_seen_at: seen,
        last_seen_at: seen,
        addressed_at: None,
        addressed_reason: None,
        digested_at: None,
        route,
        shared_tests: vec!["QuickJS waits".to_owned()],
        related_prs: vec![9547, 9540],
    }
}

#[test]
fn history_counts_recent_shared_failures_on_other_pull_requests() {
    let now = Utc
        .with_ymd_and_hms(2026, 10, 5, 4, 0, 0)
        .single()
        .expect("time");
    let mut ledger = Ledger::new("o/r", "main");
    ledger.entries.insert(
        "a".to_owned(),
        entry(
            9525,
            FlagKind::RepeatTestFailure,
            DigestRoute::Shared,
            now - Duration::hours(1),
        ),
    );
    let history = history_from_ledger(&ledger, Some(9540), now);
    assert_eq!(
        history.get("QuickJS waits").map(Vec::as_slice),
        Some(&[9525, 9547][..])
    );

    // stale, per-PR, or another rule: no corroboration
    for stale in [
        entry(
            9525,
            FlagKind::RepeatTestFailure,
            DigestRoute::Shared,
            now - Duration::hours(30),
        ),
        entry(9525, FlagKind::RepeatTestFailure, DigestRoute::PerPr, now),
        entry(9525, FlagKind::RepeatedEjection, DigestRoute::Shared, now),
    ] {
        let mut ledger = Ledger::new("o/r", "main");
        ledger.entries.insert("a".to_owned(), stale);
        assert!(history_from_ledger(&ledger, Some(9540), now).is_empty());
    }
}

#[test]
fn annotate_mode_is_off_plan_or_post() {
    assert_eq!(AnnotateMode::parse("off"), Ok(AnnotateMode::Off));
    assert_eq!(AnnotateMode::parse("plan"), Ok(AnnotateMode::Plan));
    assert_eq!(AnnotateMode::parse("post"), Ok(AnnotateMode::Post));
    assert!(AnnotateMode::parse("yes").is_err());
}

#[test]
fn a_merge_group_job_names_its_pull_request() {
    let mut queued = job(1, "macos", "failure", false);
    queued.head_branch = Some(format!("gh-readonly-queue/main/pr-9650-{}", "a".repeat(40)));
    assert_eq!(queue_pr(&queued), Some(9650));
    assert_eq!(queue_pr(&job(2, "macos", "failure", false)), None);
}
