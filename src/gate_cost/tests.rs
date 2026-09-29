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

/// A job whose wall time is exactly `minutes` minutes, queued two minutes
/// before it started on runner `r1`.
fn job(id: u64, name: &str, attempt: u64, conclusion: &str, minutes: i64) -> Value {
    job_at(
        id,
        name,
        attempt,
        conclusion,
        "2026-09-24T10:00:00Z",
        minutes,
    )
}

fn job_at(id: u64, name: &str, attempt: u64, conclusion: &str, start: &str, minutes: i64) -> Value {
    let start = at(start);
    json!({
        "id": id,
        "name": name,
        "run_attempt": attempt,
        "status": "completed",
        "conclusion": conclusion,
        "created_at": (start - chrono::Duration::minutes(2)).to_rfc3339(),
        "started_at": start.to_rfc3339(),
        "completed_at": (start + chrono::Duration::minutes(minutes)).to_rfc3339(),
        "runner_name": "r1",
        "labels": ["self-hosted", "macos"],
    })
}

/// A job no runner ever picked up.
fn unassigned(mut job: Value, labels: &[&str]) -> Value {
    job["runner_name"] = Value::Null;
    job["labels"] = json!(labels);
    job
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
            // The unfiltered event walk reads `walk:<event>:<page>` when a
            // fixture provides it, and otherwise the same listing as the
            // `created` filter (so the two agree).
            let walk_page = args.iter().find_map(|arg| arg.strip_prefix("page="));
            match walk_page {
                Some(page)
                    if !args.iter().any(|arg| arg.starts_with("created="))
                        && responses.contains_key(&format!("walk:{event}:{page}")) =>
                {
                    format!("walk:{event}:{page}")
                }
                Some(page) if page != "1" => return Ok(json!({"workflow_runs": []}).to_string()),
                _ => format!("runs:{event}"),
            }
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
    assert_eq!(
        gap_signals,
        [
            "batch_fullness",
            "runner_census",
            "push_type",
            "queue_depth_history"
        ]
    );
}

fn run(id: u64, branch: &str, sha: &str, created: &str) -> Value {
    json!({"id": id, "head_branch": branch, "head_sha": sha, "created_at": created})
}

/// The base fixture plus: run 104 on branch `c` whose gate was cancelled at
/// 09:30 after run 105 (same branch, new commit) was created at 09:20; a
/// merge-group gate (run 202) cancelled before any runner took it; a
/// `gpu-test` job requesting labels no runner advertises; and a repository
/// runner census whose organisation scope is unreadable.
fn proxy_fixture() -> BTreeMap<String, Value> {
    let mut responses = fixture();
    let mut put = |key: &str, value: Value| {
        responses.insert(key.to_owned(), value);
    };
    put(
        "runs:pull_request",
        json!([{"total_count": 5, "workflow_runs": [
            run(101, "a", "a1", "2026-09-24T09:50:00Z"),
            run(102, "b", "b1", "2026-09-24T09:50:00Z"),
            run(103, "a", "a2", "2026-09-24T09:55:00Z"),
            run(104, "c", "c1", "2026-09-24T09:00:00Z"),
            run(105, "c", "c2", "2026-09-24T09:20:00Z"),
        ]}]),
    );
    put(
        "repos/o/r/actions/runs/101/jobs",
        jobs_page(&[
            job(1, "macos", 1, "success", 20),
            job(2, "linux", 1, "success", 40),
            unassigned(
                job(10, "gpu-test", 1, "cancelled", 0),
                &["self-hosted", "gpu"],
            ),
        ]),
    );
    put(
        "repos/o/r/actions/runs/104/jobs",
        jobs_page(&[job_at(
            11,
            "macos",
            1,
            "cancelled",
            "2026-09-24T09:05:00Z",
            25,
        )]),
    );
    put(
        "repos/o/r/actions/runs/105/jobs",
        jobs_page(&[job(12, "macos", 1, "success", 20)]),
    );
    put(
        "repos/o/r/actions/runs/202/jobs",
        jobs_page(&[
            unassigned(
                job(8, "macos", 1, "cancelled", 5),
                &["self-hosted", "macos"],
            ),
            job(9, "protected-receipt-reuse", 1, "success", 1),
        ]),
    );
    put(
        "repos/o/r/actions/runners",
        json!([{"total_count": 1, "runners": [
            {"name": "r1", "status": "offline", "labels": [{"name": "self-hosted"}, {"name": "macOS"}]}
        ]}]),
    );
    responses
}

#[test]
#[allow(clippy::float_cmp)] // exact reporting is the property under test
fn count_based_proxies_are_reported_exactly() {
    let report = run_fixture(proxy_fixture()).expect("fixture gathers");
    let proxies = &report.proxies;
    assert_eq!(proxies.basis, "count-based, load-independent");

    let runs = &proxies.runs_per_merged_pr;
    assert_eq!((runs.pr_head_runs, runs.merge_group_runs), (5, 2));
    assert_eq!(
        runs.wasted_attempts, 3,
        "102 failure, 104 and 202 cancelled"
    );
    assert_eq!(runs.pr_head_per_merged_pr, Some(1.67));
    assert_eq!(runs.merge_group_per_merged_pr, Some(0.67));
    assert_eq!(runs.wasted_per_merged_pr, Some(1.0));
    assert!(!runs.evidence.sufficient, "3 merged PRs is under the floor");

    assert_eq!(proxies.starvation.gate_attempts, 7);
    assert_eq!(proxies.starvation.cancelled_before_runner, 1);
    assert_eq!(
        proxies.starvation.superseded_by_push, 0,
        "run 202 is a merge group"
    );
    assert_eq!(proxies.starvation.share, Some(0.143));
    assert!(!proxies.starvation.evidence.sufficient);

    let placement = &proxies.placement;
    assert_eq!(placement.census, "partial");
    assert_eq!(placement.jobs, 11);
    assert_eq!(placement.unserved_label_jobs, 1);
    assert_eq!(
        placement.unserved_label_sets,
        vec![vec!["gpu".to_owned(), "self-hosted".to_owned()]]
    );
    assert_eq!(placement.placement_correct_share, Some(0.9));
    assert_eq!(placement.by_job_class["gpu-test"].unserved, 1);
    assert_eq!(placement.by_job_class["macos"].waiting_or_starved, 1);
    assert_eq!(
        placement.by_job_class["macos"].placement_correct_share,
        Some(1.0)
    );

    let queue = &proxies.merge_queue;
    assert_eq!(queue.attempts, 2);
    assert_eq!(queue.attempts_per_merged_pr, Some(0.67));
    assert_eq!(queue.ejections, 1);
    assert_eq!(queue.ejections_by_cause.get("starved"), Some(&1));

    let pushes = &proxies.push_cancellations;
    assert_eq!(pushes.cancelled_pr_head_runs, 1);
    assert_eq!(pushes.superseded_by_push, 1);
    assert_eq!(pushes.share_of_pr_head_runs, Some(0.2));
    assert_eq!(pushes.by_push_type, None);

    let wait = &proxies.queue_wait_per_job_ahead;
    assert_eq!(
        wait.evidence.sample, 6,
        "the starved job never waited to a start"
    );
    assert_eq!(wait.raw_median_wait_seconds, Some(120.0));
}

#[test]
fn a_cancellation_without_a_newer_push_is_not_counted_as_superseded() {
    let mut responses = proxy_fixture();
    responses.insert(
        "runs:pull_request".to_owned(),
        json!([{"total_count": 5, "workflow_runs": [
            run(101, "a", "a1", "2026-09-24T09:50:00Z"),
            run(102, "b", "b1", "2026-09-24T09:50:00Z"),
            run(103, "a", "a2", "2026-09-24T09:55:00Z"),
            run(104, "c", "c1", "2026-09-24T09:00:00Z"),
            // Same commit: a re-run, not a push.
            run(105, "c", "c1", "2026-09-24T09:20:00Z"),
        ]}]),
    );
    let report = run_fixture(responses).expect("fixture gathers");
    assert_eq!(report.proxies.push_cancellations.cancelled_pr_head_runs, 1);
    assert_eq!(report.proxies.push_cancellations.superseded_by_push, 0);
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

#[test]
fn a_queued_gate_withdrawn_by_a_push_is_separated_from_capacity_starvation() {
    let mut responses = proxy_fixture();
    responses.insert(
        "repos/o/r/actions/runs/104/jobs".to_owned(),
        jobs_page(&[unassigned(
            job_at(11, "macos", 1, "cancelled", "2026-09-24T09:05:00Z", 25),
            &["self-hosted", "macos"],
        )]),
    );
    let report = run_fixture(responses).expect("fixture gathers");
    let starvation = &report.proxies.starvation;
    assert_eq!(starvation.cancelled_before_runner, 2, "jobs 8 and 11");
    assert_eq!(
        starvation.superseded_by_push, 1,
        "job 11's run was superseded"
    );
    assert_eq!(report.proxies.push_cancellations.superseded_by_push, 1);
}

/// The fixture with every run listed as a completed first attempt, so its
/// jobs are settled answers the cache may keep.
fn settled_fixture() -> BTreeMap<String, Value> {
    let mut responses = fixture();
    for (event, ids) in [
        ("pull_request", &[101_u64, 102, 103][..]),
        ("merge_group", &[201, 202][..]),
    ] {
        responses.insert(
            format!("runs:{event}"),
            json!([{
                "total_count": ids.len(),
                "workflow_runs": ids
                    .iter()
                    .map(|id| json!({"id": id, "run_attempt": 1, "status": "completed"}))
                    .collect::<Vec<_>>(),
            }]),
        );
    }
    responses
}

fn gather_with(
    responses: BTreeMap<String, Value>,
    cache: &ReadCache,
) -> (GateCostReport, Vec<String>) {
    let seen = std::sync::Mutex::new(Vec::new());
    let inner = reader(responses);
    let gh = |args: &[String]| {
        if let Some(path) = args
            .iter()
            .find(|arg| arg.starts_with("repos/") || arg.starts_with("orgs/"))
        {
            seen.lock().expect("seen").push(path.clone());
        }
        inner(args)
    };
    let observation =
        gather_cached(&gh, &query(), at("2026-09-26T01:00:00Z"), cache).expect("gathers");
    (compute(&observation), seen.into_inner().expect("seen"))
}

fn settled_reads(paths: &[String]) -> usize {
    paths
        .iter()
        .filter(|path| {
            path.ends_with("/jobs")
                || path.ends_with("/annotations")
                || path.contains("/git/commits/")
        })
        .count()
}

#[test]
fn a_second_run_serves_settled_answers_from_the_cache() {
    let dir = tempfile::tempdir().expect("tempdir");
    let now = std::time::SystemTime::now();

    let cold = ReadCache::open(dir.path(), now);
    let (first, first_paths) = gather_with(settled_fixture(), &cold);
    // Control: the first run really read every settled answer from GitHub.
    // 5 run job lists, 2 receipt-job annotation lists, 5 commits.
    assert_eq!(settled_reads(&first_paths), 12);
    assert_eq!(first.reads.cached, 0);

    let warm = ReadCache::open(dir.path(), now);
    let (second, second_paths) = gather_with(settled_fixture(), &warm);
    assert_eq!(settled_reads(&second_paths), 0, "{second_paths:?}");
    assert_eq!(second.reads.cached, 12);
    assert_eq!(
        second.reads.github + 12,
        first.reads.github,
        "only the settled reads moved to the cache"
    );

    let strip = |report: &GateCostReport| {
        let mut report = report.clone();
        report.reads = ReadStats::default();
        report
    };
    assert_eq!(
        strip(&first),
        strip(&second),
        "cached answers change nothing"
    );
}

#[test]
fn unsettled_runs_and_jobs_are_read_live_every_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let now = std::time::SystemTime::now();
    let mut responses = settled_fixture();
    // Run 101 is still in progress; run 102 is listed completed but one of
    // its jobs is not, so neither answer may be kept.
    responses.insert(
        "runs:pull_request".to_owned(),
        json!([{"total_count": 3, "workflow_runs": [
            {"id": 101, "run_attempt": 1, "status": "in_progress"},
            {"id": 102, "run_attempt": 1, "status": "completed"},
            {"id": 103, "run_attempt": 1, "status": "completed"},
        ]}]),
    );
    let mut in_flight = job(4, "macos", 2, "success", 30);
    in_flight["status"] = json!("in_progress");
    responses.insert(
        "repos/o/r/actions/runs/102/jobs".to_owned(),
        jobs_page(&[job(3, "macos", 1, "failure", 10), in_flight]),
    );

    gather_with(responses.clone(), &ReadCache::open(dir.path(), now));
    let (_, paths) = gather_with(responses, &ReadCache::open(dir.path(), now));
    // Reads run on several workers, so their order is not part of the contract.
    let mut live: Vec<&String> = paths
        .iter()
        .filter(|path| path.ends_with("/jobs"))
        .collect();
    live.sort();
    assert_eq!(
        live,
        [
            "repos/o/r/actions/runs/101/jobs",
            "repos/o/r/actions/runs/102/jobs"
        ]
    );
}

#[test]
fn a_rerun_attempt_is_not_answered_from_the_earlier_attempt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let now = std::time::SystemTime::now();
    gather_with(settled_fixture(), &ReadCache::open(dir.path(), now));

    let mut rerun = settled_fixture();
    rerun.insert(
        "runs:pull_request".to_owned(),
        json!([{"total_count": 3, "workflow_runs": [
            {"id": 101, "run_attempt": 2, "status": "completed"},
            {"id": 102, "run_attempt": 1, "status": "completed"},
            {"id": 103, "run_attempt": 1, "status": "completed"},
        ]}]),
    );
    rerun.insert(
        "repos/o/r/actions/runs/101/jobs".to_owned(),
        jobs_page(&[
            job(1, "macos", 1, "success", 20),
            job(2, "linux", 1, "success", 40),
            job(12, "macos", 2, "success", 20),
        ]),
    );
    let (report, _) = gather_with(rerun, &ReadCache::open(dir.path(), now));
    assert_eq!(report.pr_head.jobs, 5, "attempt 2's job is counted");
}

#[test]
fn reuse_stops_at_the_job_that_published_the_decision() {
    let mut responses = fixture();
    // Every job that ran is read when no receipt job is named. Job 20 comes
    // after the decider and has no annotations to read: reaching it would
    // make the run unreadable.
    responses.insert(
        "repos/o/r/actions/runs/202/jobs".to_owned(),
        jobs_page(&[
            job(8, "macos", 1, "cancelled", 5),
            job(9, "protected-receipt-reuse", 1, "success", 1),
            job(20, "linux", 1, "success", 1),
        ]),
    );
    responses.insert("repos/o/r/check-runs/8/annotations".to_owned(), json!([[]]));
    responses.insert("repos/o/r/check-runs/6/annotations".to_owned(), json!([[]]));
    let gh = reader(responses);
    let mut unnamed = query();
    unnamed.receipt_job = None;
    let observation = gather(&gh, &unnamed, at("2026-09-26T01:00:00Z")).expect("gathers");
    let report = compute(&observation);
    assert_eq!(report.reuse.refused, 1);
    assert_eq!(report.reuse.unreadable, 0);
}

#[test]
fn a_cache_entry_under_another_key_is_a_miss() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = ReadCache::open(dir.path(), std::time::SystemTime::now());
    cache.put("commit-parents:o/r:a1", &json!([{"sha": "b0"}]));
    assert_eq!(
        cache.get("commit-parents:o/r:a1"),
        Some(json!([{"sha": "b0"}]))
    );
    assert_eq!(cache.get("commit-parents:o/r:a2"), None);
    // Tamper with the stored key: the digest still matches, the key does not.
    let entry = std::fs::read_dir(dir.path())
        .expect("dir")
        .flatten()
        .next()
        .expect("one entry")
        .path();
    std::fs::write(&entry, json!({"key": "other", "value": 1}).to_string()).expect("write");
    assert_eq!(cache.get("commit-parents:o/r:a1"), None);
    assert_eq!(cache.stats().cached, 1, "only the real hit counts");
}

#[test]
fn entries_older_than_the_age_limit_are_pruned_on_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let now = std::time::SystemTime::now();
    ReadCache::open(dir.path(), now).put("k", &json!(1));
    assert_eq!(ReadCache::open(dir.path(), now).get("k"), Some(json!(1)));
    let later = now + cache::MAX_ENTRY_AGE + std::time::Duration::from_secs(60);
    assert_eq!(ReadCache::open(dir.path(), later).get("k"), None);
}

/// A short `created` listing whose `total_count` agrees with itself: the
/// failure `collect_counted` cannot see. The event walk finds the runs it
/// missed, they are counted, and the disagreement is named.
#[test]
fn a_short_created_listing_is_completed_by_the_event_walk() {
    let mut responses = fixture();
    responses.insert(
        "runs:pull_request".to_owned(),
        json!([{"total_count": 1, "workflow_runs": [
            {"id": 101, "created_at": "2026-09-24T09:00:00Z"}
        ]}]),
    );
    responses.insert(
        "walk:pull_request:1".to_owned(),
        json!({"workflow_runs": [
            {"id": 103, "created_at": "2026-09-25T09:00:00Z"},
            {"id": 102, "created_at": "2026-09-24T12:00:00Z"},
            {"id": 101, "created_at": "2026-09-24T09:00:00Z"},
            {"id": 99, "created_at": "2026-09-23T09:00:00Z"}
        ]}),
    );
    let report = run_fixture(responses).expect("fixture gathers");
    assert_eq!(report.pr_head.runs, 3, "the walk restores runs 102 and 103");
    assert_eq!(report.pr_head.jobs, 4);
    let gap = report
        .telemetry_gaps
        .iter()
        .find(|gap| gap.signal == "run_listing")
        .expect("the disagreement is a gap");
    assert!(
        gap.reason.contains("returned 1 `pull_request` run(s)")
            && gap.reason.contains("found 2 more"),
        "{}",
        gap.reason
    );
}

/// A gate whose `name:` is an expression GitHub reported unevaluated is still
/// counted, a run with no gate job at all adds nothing, and both are named.
#[test]
fn a_gate_reported_under_an_unevaluated_name_is_counted_and_named() {
    let mut responses = fixture();
    let expression = "github.event_name == 'pull_request' && 'macos' || 'macos-unused'";
    responses.insert(
        "repos/o/r/actions/runs/101/jobs".to_owned(),
        jobs_page(&[
            job(1, expression, 1, "success", 20),
            job(
                11,
                "github.event_name == 'merge_group' && 'macos' || 'x'",
                1,
                "skipped",
                0,
            ),
            job(2, "linux", 1, "success", 40),
        ]),
    );
    responses.insert(
        "repos/o/r/actions/runs/103/jobs".to_owned(),
        jobs_page(&[job(5, "linux", 1, "success", 3)]),
    );
    let report = run_fixture(responses).expect("fixture gathers");
    // Run 101's 20 minutes still count; the skipped alternate does not; run
    // 103 has no gate job.
    assert_eq!(report.pr_head.jobs, 3);
    assert_eq!(report.pr_head.jobs_ran, 3);
    let gap = report
        .telemetry_gaps
        .iter()
        .find(|gap| gap.signal == "gate_job_name")
        .expect("named as a gap");
    assert!(
        gap.reason
            .starts_with("2 of 3 `pull_request` run(s) have no job named exactly `macos`"),
        "{}",
        gap.reason
    );
    assert!(
        gap.reason
            .contains("1 of them were matched through an unevaluated job name")
    );
    assert!(gap.reason.contains(expression));
    assert!(gap.reason.contains("1 run(s) carry no gate job at all"));
}
