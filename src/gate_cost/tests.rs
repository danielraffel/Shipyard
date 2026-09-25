use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::*;

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("fixture timestamp")
        .with_timezone(&Utc)
}

fn query() -> GateCostQuery {
    GateCostQuery {
        repo: "o/r".to_owned(),
        workflow: "build.yml".to_owned(),
        gate_job: "macos".to_owned(),
        base_branch: "main".to_owned(),
        events: vec!["pull_request".to_owned(), "merge_group".to_owned()],
        receipt_job: Some("protected-receipt-reuse".to_owned()),
        receipt_target: "macos".to_owned(),
        from: at("2026-09-24T00:00:00Z"),
        to: at("2026-09-26T00:00:00Z"),
    }
}

/// A job whose wall time is exactly `minutes` minutes.
fn job(id: u64, name: &str, attempt: u64, conclusion: &str, minutes: i64) -> Value {
    let start = at("2026-09-24T10:00:00Z");
    json!({
        "id": id,
        "name": name,
        "run_attempt": attempt,
        "status": "completed",
        "conclusion": conclusion,
        "started_at": start.to_rfc3339(),
        "completed_at": (start + chrono::Duration::minutes(minutes)).to_rfc3339(),
    })
}

fn decision(target: &str, verdict: &str) -> Value {
    json!({
        "title": "shipyard-receipt-decision",
        "message": json!({
            "schema": "shipyard-receipt-decision/v1",
            "target": target,
            "verdict": verdict,
        }).to_string(),
    })
}

fn runs_page(ids: &[u64]) -> Value {
    json!([{
        "total_count": ids.len(),
        "workflow_runs": ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>(),
    }])
}

fn jobs_page(jobs: &[Value]) -> Value {
    json!([{"total_count": jobs.len(), "jobs": jobs}])
}

fn commit(parent: Option<&str>) -> Value {
    json!({"parents": parent.map_or_else(Vec::new, |sha| vec![json!({"sha": sha})])})
}

/// Known fixture:
/// - PR heads: run 101 macos success 20m; run 102 macos failure 10m then
///   success 30m on a re-run; run 103 macos skipped.
/// - Merge groups: run 201 macos success 25m, reused for macos; run 202 macos
///   cancelled 5m, refused for macos (a linux reuse must not count).
/// - Merge-queue pushes: a2 (2 PRs), a3 (1 PR), one outside the window, and
///   one whose first-parent walk never reaches its pre-push commit.
fn fixture() -> BTreeMap<String, Value> {
    let mut responses = BTreeMap::new();
    let mut put = |key: &str, value: Value| {
        responses.insert(key.to_owned(), value);
    };
    put("runs:pull_request", runs_page(&[101, 102, 103]));
    put("runs:merge_group", runs_page(&[201, 202]));
    put(
        "repos/o/r/actions/runs/101/jobs",
        jobs_page(&[
            job(1, "macos", 1, "success", 20),
            job(2, "linux", 1, "success", 40),
        ]),
    );
    put(
        "repos/o/r/actions/runs/102/jobs",
        jobs_page(&[
            job(3, "macos", 1, "failure", 10),
            job(4, "macos", 2, "success", 30),
        ]),
    );
    put(
        "repos/o/r/actions/runs/103/jobs",
        jobs_page(&[job(5, "macos", 1, "skipped", 0)]),
    );
    put(
        "repos/o/r/actions/runs/201/jobs",
        jobs_page(&[
            job(6, "macos", 1, "success", 25),
            job(7, "protected-receipt-reuse", 1, "success", 1),
        ]),
    );
    put(
        "repos/o/r/actions/runs/202/jobs",
        jobs_page(&[
            job(8, "macos", 1, "cancelled", 5),
            job(9, "protected-receipt-reuse", 1, "success", 1),
        ]),
    );
    put(
        "repos/o/r/check-runs/7/annotations",
        json!([[decision("macos", "reuse")]]),
    );
    put(
        "repos/o/r/check-runs/9/annotations",
        json!([[decision("linux", "reuse"), decision("macos", "refuse")]]),
    );
    put(
        "repos/o/r/rules/branches/main",
        json!([{"type": "merge_queue", "parameters": {
            "max_entries_to_merge": 5, "max_entries_to_build": 3, "merge_method": "MERGE"}}]),
    );
    put(
        "repos/o/r/activity",
        json!([[
            {"before": "b0", "after": "a2", "timestamp": "2026-09-24T12:00:00Z"},
            {"before": "a2", "after": "a3", "timestamp": "2026-09-25T12:00:00Z"},
            {"before": "z0", "after": "z1", "timestamp": "2026-09-23T12:00:00Z"},
            {"before": "nowhere", "after": "d1", "timestamp": "2026-09-25T13:00:00Z"}
        ]]),
    );
    put("repos/o/r/git/commits/a2", commit(Some("a1")));
    put("repos/o/r/git/commits/a1", commit(Some("b0")));
    put("repos/o/r/git/commits/a3", commit(Some("a2")));
    put("repos/o/r/git/commits/d1", commit(Some("d0")));
    put("repos/o/r/git/commits/d0", commit(None));
    put(
        "search/issues",
        json!({"total_count": 3, "incomplete_results": false}),
    );
    put(
        "graphql",
        json!({"data": {"repository": {"mergeQueue": {"entries": {"totalCount": 2}}}}}),
    );
    responses
}

fn reader(responses: BTreeMap<String, Value>) -> impl Fn(&[String]) -> Result<String, String> {
    move |args: &[String]| {
        let path = args
            .iter()
            .find(|arg| arg.starts_with("repos/") || *arg == "search/issues" || *arg == "graphql")
            .ok_or_else(|| format!("no path in {args:?}"))?;
        let key = if path.ends_with("/runs") && path.contains("/workflows/") {
            let event = args
                .iter()
                .find_map(|arg| arg.strip_prefix("event="))
                .ok_or("no event")?;
            format!("runs:{event}")
        } else {
            path.clone()
        };
        responses
            .get(&key)
            .map(Value::to_string)
            .ok_or_else(|| format!("HTTP 404 for {key}"))
    }
}

fn run_fixture(responses: BTreeMap<String, Value>) -> Result<GateCostReport, String> {
    let gh = reader(responses);
    let observation = gather(&gh, &query(), at("2026-09-26T01:00:00Z"))?;
    Ok(compute(&observation))
}

#[test]
#[allow(clippy::float_cmp)] // exact reporting is the property under test
fn synthetic_fixture_is_reported_exactly() {
    let report = run_fixture(fixture()).expect("fixture gathers");

    assert_eq!(report.pr_head.runs, 3);
    assert_eq!(report.pr_head.jobs, 4);
    assert_eq!(report.pr_head.jobs_ran, 3);
    assert_eq!(report.pr_head.gate_minutes, 60.0);
    assert_eq!(report.pr_head.wasted_minutes, 10.0);
    assert_eq!(report.pr_head.median_minutes, Some(20.0));
    assert_eq!(report.pr_head.p25_minutes, Some(15.0));
    assert_eq!(report.pr_head.p75_minutes, Some(25.0));

    assert_eq!(report.merge_group.runs, 2);
    assert_eq!(report.merge_group.jobs_ran, 2);
    assert_eq!(report.merge_group.gate_minutes, 30.0);
    assert_eq!(report.merge_group.wasted_minutes, 5.0);
    assert_eq!(report.merge_group.median_minutes, Some(15.0));

    assert_eq!(report.gate_minutes, 90.0);
    assert_eq!(report.wasted_gate_minutes, 15.0);
    assert_eq!(report.merged_prs, Some(3));
    assert_eq!(report.gate_minutes_per_merged_pr, Some(30.0));
    assert_eq!(report.pr_head_runs_per_merged_pr, Some(1.0));
    assert_eq!(report.merge_group_runs_per_merged_pr, Some(0.67));

    assert_eq!(report.batches.batches, 3);
    assert_eq!(report.batches.unresolved, 1);
    assert_eq!(report.batches.prs_in_batches, 3);
    assert_eq!(report.batches.mean_prs_per_batch, Some(1.5));
    assert_eq!(report.batches.max_entries_to_merge, Some(5));
    assert_eq!(report.batches.max_entries_to_build, Some(3));
    assert_eq!(report.batches.mean_fullness, Some(0.3));
    assert_eq!(report.batches.at_capacity, 0);
    assert_eq!(
        report.batches.distribution,
        BTreeMap::from([(1, 1), (2, 1)])
    );

    assert_eq!(report.reuse.merge_group_runs, 2);
    assert_eq!(report.reuse.reused, 1);
    assert_eq!(report.reuse.refused, 1);
    assert_eq!(report.reuse.no_decision, 0);
    assert_eq!(report.reuse.rate, Some(0.5));

    assert_eq!(report.current_queue_depth, Some(2));
    let gap_signals: Vec<_> = report
        .telemetry_gaps
        .iter()
        .map(|gap| gap.signal.as_str())
        .collect();
    assert_eq!(gap_signals, ["batch_fullness", "queue_depth_history"]);
}

#[test]
fn a_short_page_is_refused_not_undercounted() {
    let mut responses = fixture();
    responses.insert(
        "runs:merge_group".to_owned(),
        json!([{"total_count": 3, "workflow_runs": [{"id": 201}, {"id": 202}]}]),
    );
    let error = run_fixture(responses).expect_err("partial listing must fail");
    assert!(error.contains("refusing a partial read"), "{error}");
}

#[test]
fn missing_decisions_count_as_not_reused_and_are_named_as_a_gap() {
    let mut responses = fixture();
    responses.insert("repos/o/r/check-runs/7/annotations".to_owned(), json!([[]]));
    let report = run_fixture(responses).expect("fixture gathers");
    assert_eq!(report.reuse.reused, 0);
    assert_eq!(report.reuse.no_decision, 1);
    assert_eq!(report.reuse.rate, Some(0.0));
    assert!(
        report
            .telemetry_gaps
            .iter()
            .any(|gap| gap.signal == "receipt_reuse")
    );
}

#[test]
fn unreadable_side_signals_degrade_to_gaps() {
    let mut responses = fixture();
    responses.remove("search/issues");
    responses.remove("graphql");
    responses.remove("repos/o/r/rules/branches/main");
    let report = run_fixture(responses).expect("gate runs still readable");
    assert_eq!(report.merged_prs, None);
    assert_eq!(report.gate_minutes_per_merged_pr, None);
    assert_eq!(report.batches.mean_fullness, None);
    assert_eq!(report.current_queue_depth, None);
    for signal in ["merged_prs", "current_queue_depth", "max_entries_to_merge"] {
        assert!(
            report.telemetry_gaps.iter().any(|gap| gap.signal == signal),
            "missing gap {signal}"
        );
    }
}

#[test]
fn window_lengths_parse() {
    assert_eq!(parse_window("48h"), Ok(chrono::Duration::hours(48)));
    assert_eq!(parse_window("2d"), Ok(chrono::Duration::days(2)));
    assert_eq!(parse_window("90m"), Ok(chrono::Duration::minutes(90)));
    assert!(parse_window("0h").is_err());
    assert!(parse_window("2w").is_err());
    assert!(parse_window("").is_err());
}

#[test]
fn activity_period_covers_the_window_start() {
    let now = at("2026-09-26T00:00:00Z");
    assert_eq!(activity_period(now, at("2026-09-25T12:00:00Z")), "day");
    assert_eq!(activity_period(now, at("2026-09-24T00:00:00Z")), "week");
    assert_eq!(activity_period(now, at("2026-09-01T00:00:00Z")), "month");
}
