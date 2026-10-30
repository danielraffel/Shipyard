//! Reading, caching and counting causes over a captured Pulp merge-group
//! gate: run 38308868279's `macos` job 115009634572
//! (`tests/fixtures/ctest-log/`), in which
//! `pulp.vellum-closure-external-input-negative` failed. How one job's cause
//! is chosen is tested in `diagnose::cause`.
use std::collections::BTreeMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::*;
use crate::gate_cost::GateJobSample;
use crate::gate_cost::scorecard::cases::cases;
use crate::gate_cost::scorecard::{
    CheckOutcome, MergeGroupRun, MergeVerdict, MergedPr, PrCommit, ScorecardQuery,
};

const PULP: &str = "Generous-Corp/pulp";
const GATE_JOB: u64 = 115_009_634_572;
const VELLUM: &str = "pulp.vellum-closure-external-input-negative";

fn fixture(path: &str) -> String {
    let path = format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn gate_job_payload() -> Value {
    let jobs: Value =
        serde_json::from_str(&fixture("ctest-log/jobs-mg-run-38308868279.json")).expect("jobs");
    jobs["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .find(|job| job["id"].as_u64() == Some(GATE_JOB))
        .cloned()
        .expect("gate job")
}

fn gate_log() -> String {
    fixture("ctest-log/log-mg-job-115009634572.log")
}

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("timestamp")
        .with_timezone(&Utc)
}

fn check(name: &str, outcome: &str, job_id: Option<u64>) -> CheckOutcome {
    CheckOutcome {
        name: name.to_owned(),
        outcome: Some(outcome.to_owned()),
        at: Some(at("2026-10-12T21:25:47Z")),
        job_id,
        run_id: job_id.map(|_| 38_308_868_279),
    }
}

fn observation() -> ScorecardObservation {
    ScorecardObservation {
        query: ScorecardQuery {
            repo: PULP.to_owned(),
            workflow: "build.yml".to_owned(),
            gate_job: "macos".to_owned(),
            base_branch: "main".to_owned(),
            required_contexts: vec!["macos".to_owned(), "status/ci".to_owned()],
            selection_steps: Vec::new(),
            from: at("2026-10-12T00:00:00Z"),
            to: at("2026-10-13T00:00:00Z"),
        },
        required: Ok((
            vec!["macos".to_owned(), "status/ci".to_owned()],
            "--required-context".to_owned(),
        )),
        prs: vec![MergedPr {
            number: 10_154,
            created_at: at("2026-10-12T20:00:00Z"),
            merged_at: at("2026-10-12T22:00:00Z"),
            commits_total: 2,
            verdict: MergeVerdict::None,
            commits: vec![
                PrCommit {
                    oid: "a".repeat(40),
                    checks: vec![
                        check("macos", "FAILURE", Some(GATE_JOB)),
                        // Red but not required: never read.
                        check("lint", "FAILURE", Some(1)),
                        // Required, red, a status context: no job to read.
                        check("status/ci", "ERROR", None),
                    ],
                },
                PrCommit {
                    oid: "b".repeat(40),
                    // Required and green: not a case.
                    checks: vec![check("macos", "SUCCESS", Some(2))],
                },
            ],
        }],
        merge_group_runs: vec![MergeGroupRun {
            run_id: 99,
            created_at: at("2026-10-12T21:00:00Z"),
            conclusion: Some("failure".to_owned()),
            gate: Some(GateJobSample::from_job(
                99,
                "merge_group",
                &json!({"status": "completed", "conclusion": "failure", "runner_name": "runner-1"}),
            )),
            gate_job_id: Some(42),
        }],
        queue_events: Vec::new(),
        selection: None,
        gaps: Vec::new(),
    }
}

#[test]
fn red_jobs_are_the_required_reds_and_the_failed_gates() {
    let jobs: Vec<u64> = red_jobs(&observation()).into_iter().collect();
    assert_eq!(jobs, vec![42, GATE_JOB]);
}

struct FakeGh {
    calls: Mutex<Vec<String>>,
}

impl FakeGh {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }

    fn answer(&self, args: &[String]) -> Result<String, String> {
        let path = args.last().cloned().unwrap_or_default();
        self.calls.lock().expect("calls").push(path.clone());
        match path.as_str() {
            p if p == format!("repos/{PULP}/actions/jobs/{GATE_JOB}") => {
                Ok(gate_job_payload().to_string())
            }
            p if p == format!("repos/{PULP}/actions/jobs/{GATE_JOB}/logs") => Ok(gate_log()),
            p if p == format!("repos/{PULP}/actions/jobs/42") => {
                let mut job = gate_job_payload();
                job["id"] = json!(42);
                Ok(job.to_string())
            }
            p if p == format!("repos/{PULP}/actions/jobs/42/logs") => {
                Err("gh: Not Found (HTTP 404)".to_owned())
            }
            p if (43..=50).any(|id| p == format!("repos/{PULP}/actions/jobs/{id}")) => {
                let mut job = gate_job_payload();
                job["id"] = json!(p.rsplit('/').next().and_then(|id| id.parse::<u64>().ok()));
                Ok(job.to_string())
            }
            p if (43..=50).any(|id| p == format!("repos/{PULP}/actions/jobs/{id}/logs")) => {
                let id: u64 = p
                    .split('/')
                    .nth(5)
                    .and_then(|id| id.parse().ok())
                    .expect("id");
                Ok(self_named_log(id))
            }
            _ => Err(format!("unexpected request {path}")),
        }
    }
}

#[test]
fn gather_reads_each_job_and_a_missing_log_still_yields_a_cause() {
    let gh = FakeGh::new();
    let reader = |args: &[String]| gh.answer(args);
    let jobs = [GATE_JOB, 42, 7].into_iter().collect();
    let read = gather(
        &reader,
        &ReadCache::disabled(),
        PULP,
        &jobs,
        DEFAULT_MAX_JOBS,
    );
    assert_eq!(read.by_job[&GATE_JOB].signature, VELLUM);
    assert_eq!(read.by_job[&42].format, "log-missing");
    assert!(!read.by_job[&42].named());
    assert_eq!(read.unreadable.len(), 1);
    assert_eq!(read.unreadable[0].job_id, 7);
    assert!(read.unreadable[0].reason.contains("unexpected request"));
}

/// A log in which the job printed its own record at 21:25:00, inside step 52
/// (21:24:00 to 21:25:37). Jobs 44 to 46 and 48 print a forged one: wrong
/// head, wrong run, wrong attempt, or a cause the log does not hold. Job 47's
/// record claims an earlier `named_at` than it was printed at; job 49 prints a
/// genuine record at a time no step covers, and job 50 one printed in a step
/// before the failing step ran.
fn self_named_log(id: u64) -> String {
    let job: crate::diagnose::Job = serde_json::from_value(gate_job_payload()).expect("job");
    let place = Place {
        repo: PULP.to_owned(),
        ..Place::default()
    };
    let mut record = cause::build(
        &job,
        Some(&gate_log()),
        &place,
        Some("2026-10-12T21:25:00Z".to_owned()),
        false,
    )
    .expect("red");
    match id {
        44 => record.head_sha = Some("f".repeat(40)),
        45 => record.run_id = Some(1),
        46 => record.attempt = Some(2),
        47 => record.named_at = Some("2026-10-12T21:14:09Z".to_owned()),
        48 => record.signature = "a test that never failed".to_owned(),
        _ => {}
    }
    let printed = if id == 49 {
        "2026-10-12T21:30:00.0000000Z"
    } else if id == 50 {
        // In checkout (step 4), before the failing step ran.
        "2026-10-12T21:14:15.0000000Z"
    } else {
        "2026-10-12T21:25:00.0000000Z"
    };
    let mut log = gate_log();
    log.push_str(printed);
    log.push_str(" ##[notice]");
    log.push_str(&serde_json::to_string(&record).expect("json"));
    log.push('\n');
    log
}

#[test]
fn a_cause_the_job_recorded_keeps_its_time() {
    let gh = FakeGh::new();
    let reader = |args: &[String]| gh.answer(args);
    let jobs = std::iter::once(43).collect();
    let read = gather(
        &reader,
        &ReadCache::disabled(),
        PULP,
        &jobs,
        DEFAULT_MAX_JOBS,
    );
    let cause = &read.by_job[&43];
    assert!(!cause.retrospective);
    // GitHub's time for the step that printed it, not the record's.
    assert_eq!(cause.named_at.as_deref(), Some("2026-10-12T21:25:37Z"));
    // The content is the extractor's, bound to the payload.
    assert_eq!(cause.job_id, Some(43));
    assert_eq!(cause.signature, VELLUM);
}

#[test]
fn a_forged_record_is_ignored_and_the_extractor_answers() {
    let gh = FakeGh::new();
    let reader = |args: &[String]| gh.answer(args);
    let jobs = (44..=50).collect();
    let read = gather(
        &reader,
        &ReadCache::disabled(),
        PULP,
        &jobs,
        DEFAULT_MAX_JOBS,
    );
    for id in [44, 45, 46, 48, 49, 50] {
        let cause = &read.by_job[&id];
        assert!(cause.retrospective, "job {id}: a forged record was trusted");
        assert_eq!(
            cause.named_at.as_deref(),
            Some("2026-10-12T21:25:47Z"),
            "job {id}"
        );
        assert_eq!(cause.signature, VELLUM, "job {id}");
    }
    // A record that only moves its own time gains nothing: the time is the
    // API's for the step that printed it.
    let moved = &read.by_job[&47];
    assert!(!moved.retrospective);
    assert_eq!(moved.named_at.as_deref(), Some("2026-10-12T21:25:37Z"));
}

#[test]
fn a_read_cause_is_cached_and_not_read_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = ReadCache::open(dir.path(), std::time::SystemTime::now());
    let gh = FakeGh::new();
    let reader = |args: &[String]| gh.answer(args);
    let jobs = std::iter::once(GATE_JOB).collect();
    let first = gather(&reader, &cache, PULP, &jobs, DEFAULT_MAX_JOBS);
    let reads = gh.calls.lock().expect("calls").len();
    assert_eq!(reads, 2);
    let second = gather(&reader, &cache, PULP, &jobs, DEFAULT_MAX_JOBS);
    assert_eq!(gh.calls.lock().expect("calls").len(), reads);
    assert_eq!(first, second);
}

#[test]
fn attached_causes_fill_evidence_and_are_counted_as_named() {
    let observation = observation();
    let mut rows = cases(&observation);
    let gh = FakeGh::new();
    let reader = |args: &[String]| gh.answer(args);
    let read = gather(
        &reader,
        &ReadCache::disabled(),
        PULP,
        &red_jobs(&observation),
        DEFAULT_MAX_JOBS,
    );
    attach(&mut rows, &read);
    let gate = rows
        .iter()
        .find(|row| row.job_id == Some(GATE_JOB))
        .expect("red macos row");
    assert_eq!(gate.run_id, Some(38_308_868_279));
    // The record takes the pull request from its case.
    assert_eq!(gate.cause.as_ref().and_then(|cause| cause.pr), Some(10_154));
    assert!(gate.evidence.is_some());
    assert!(!gate.unread.contains(&"cause"));
    assert!(!gate.unread.contains(&"evidence"));
    assert_eq!(
        gate.cause.as_ref().map(|cause| cause.signature.as_str()),
        Some(VELLUM)
    );
    let status = rows
        .iter()
        .find(|row| row.check.as_deref() == Some("status/ci"))
        .expect("status row");
    assert!(status.cause.is_none() && status.unread.contains(&"evidence"));

    let naming = compute(&rows, &read);
    // macos (named, high), status/ci (no job), merge-group gate 42 (no log).
    assert_eq!(naming.reds, 3);
    assert_eq!(naming.without_job, 1);
    assert_eq!(naming.unreadable, 0);
    assert_eq!(naming.named, 1);
    assert_eq!(naming.named_share, Some(1.0 / 3.0));
    assert_eq!(
        naming.by_confidence,
        // The missing log is a certain record that names nothing.
        BTreeMap::from([("high".to_owned(), 2), ("unread".to_owned(), 1)])
    );
    assert_eq!(naming.by_class, BTreeMap::from([("test".to_owned(), 1)]));
    // The check completed when its job did, so the cause was nameable then.
    assert_eq!(naming.time_to_named_n, 1);
    assert_eq!(naming.time_to_named_median_minutes, Some(0.0));
    assert!(naming.retrospective);
    let text = render(&naming);
    assert!(text.contains("named causes: 1/3 red cases (33%)"), "{text}");
    assert!(text.contains("retrospective"), "{text}");
    assert_eq!(
        (naming.budget, naming.skipped_for_budget),
        (DEFAULT_MAX_JOBS, 0)
    );
}

#[test]
fn rows_without_causes_keep_evidence_unread() {
    let rows = cases(&observation());
    assert!(rows.iter().all(|row| {
        row.cause.is_none() && row.unread.contains(&"cause") && row.unread.contains(&"evidence")
    }));
    let naming = compute(&rows, &CausesRead::default());
    assert_eq!(naming.named, 0);
    assert_eq!(naming.time_to_named_median_minutes, None);
}

#[test]
fn the_budget_reads_newest_first_and_skipped_jobs_count_as_unread() {
    let observation = observation();
    let gh = FakeGh::new();
    let reader = |args: &[String]| gh.answer(args);
    // Two uncached red jobs, budget one: the newer (larger id) is read.
    let read = gather(
        &reader,
        &ReadCache::disabled(),
        PULP,
        &red_jobs(&observation),
        1,
    );
    assert!(read.by_job.contains_key(&GATE_JOB));
    assert!(!read.by_job.contains_key(&42));
    assert_eq!(read.skipped_for_budget, 1);
    assert_eq!(
        read.unreadable,
        vec![Unreadable {
            job_id: 42,
            reason: OVER_BUDGET.to_owned(),
        }]
    );
    assert!(
        gh.calls
            .lock()
            .expect("calls")
            .iter()
            .all(|path| !path.contains("/jobs/42")),
        "a job over the budget was fetched"
    );
    let mut rows = cases(&observation);
    attach(&mut rows, &read);
    let naming = compute(&rows, &read);
    assert_eq!((naming.budget, naming.skipped_for_budget), (1, 1));
    // The skipped gate is unread, never a cause with no confidence.
    assert_eq!(naming.unreadable, 1);
    assert_eq!(naming.by_confidence.get("unread"), Some(&2));
    assert_eq!(naming.by_confidence.get("none"), None);
    let text = render(&naming);
    assert!(
        text.contains("1 unreadable (1 skipped over the --causes-max-jobs budget of 1)"),
        "{text}"
    );
}

#[test]
fn a_cached_cause_does_not_count_against_the_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = ReadCache::open(dir.path(), std::time::SystemTime::now());
    let gh = FakeGh::new();
    let reader = |args: &[String]| gh.answer(args);
    let _ = gather(
        &reader,
        &cache,
        PULP,
        &std::iter::once(GATE_JOB).collect(),
        1,
    );
    let reads = gh.calls.lock().expect("calls").len();
    // Budget zero: the cached job is still read, the uncached one is skipped.
    let jobs = [GATE_JOB, 42].into_iter().collect();
    let read = gather(&reader, &cache, PULP, &jobs, 0);
    assert!(read.by_job.contains_key(&GATE_JOB));
    assert_eq!(read.skipped_for_budget, 1);
    assert_eq!(read.unreadable[0].job_id, 42);
    assert_eq!(gh.calls.lock().expect("calls").len(), reads);
}

fn archive_cause(job_id: u64, run_id: u64, repo: &str) -> Value {
    json!({
        "schema": "ci.cause/v1",
        "repo": repo,
        "run_id": run_id,
        "attempt": 1,
        "job_id": job_id,
        "job": "macos",
        "head_sha": "a".repeat(40),
        "event": "pull_request",
        "pr": 10_154,
        "failed_step": "test",
        "failure_class": "test",
        "format": "ctest",
        "signature": VELLUM,
        "detail": "failed",
        "evidence": {"log_line": 10, "excerpt": ["failed"]},
        "fix_hint": null,
        "confidence": "high",
        "source": "extractor",
        "extractor": "test",
        "named_at": "2026-10-12T21:25:47Z",
        "supersedes": null,
        "kind": "test",
        "location": "test.cpp:1",
        "step_failed": true,
        "retrospective": true
    })
}

#[test]
fn archive_joins_exact_identity_and_reports_unjoined_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive_dir = dir.path().join("causes");
    std::fs::create_dir(&archive_dir).expect("archive dir");
    let path = archive_dir.join("2026-10.jsonl");
    let rows = [
        archive_cause(GATE_JOB, 38_308_868_279, PULP),
        archive_cause(999, 38_308_868_279, PULP),
    ];
    std::fs::write(
        &path,
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .expect("archive");
    let read = archive(dir.path(), PULP);
    assert_eq!(read.source, CauseSource::Archive);
    assert_eq!(read.archive_rows, 2);
    let mut cases = cases(&observation());
    attach(&mut cases, &read);
    let naming = compute(&cases, &read);
    assert_eq!(naming.source, "archive");
    assert_eq!(naming.named, 1);
    assert_eq!(naming.archive_unjoined, 1);
    assert_eq!(naming.archive_malformed, 0);
}

#[test]
fn archive_duplicates_and_malformed_rows_stay_unreadable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive_dir = dir.path().join("causes");
    std::fs::create_dir(&archive_dir).expect("archive dir");
    let path = archive_dir.join("2026-10.jsonl");
    let duplicate = archive_cause(GATE_JOB, 38_308_868_279, PULP);
    let mut wrong_repo = archive_cause(998, 38_308_868_279, "other/repo");
    let mut missing_job = archive_cause(997, 38_308_868_279, PULP);
    missing_job["job_id"] = Value::Null;
    wrong_repo["repo"] = json!("other/repo");
    std::fs::write(
        &path,
        format!("{duplicate}\n{duplicate}\n{wrong_repo}\n{missing_job}\n{{bad json}}\n"),
    )
    .expect("archive");
    let read = archive(dir.path(), PULP);
    assert_eq!(read.archive_rows, 4);
    assert_eq!(read.archive_duplicates, 1);
    assert_eq!(read.archive_malformed, 3);
    let mut cases = cases(&observation());
    attach(&mut cases, &read);
    let naming = compute(&cases, &read);
    assert_eq!(naming.named, 0, "ambiguous causes must not be joined");
    assert_eq!(naming.unreadable, 2, "the duplicate job is unreadable");
}

#[test]
fn archive_rejects_non_v1_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive_dir = dir.path().join("causes");
    std::fs::create_dir(&archive_dir).expect("archive dir");
    let path = archive_dir.join("2026-10.jsonl");
    let mut row = archive_cause(GATE_JOB, 38_308_868_279, PULP);
    row["schema"] = json!("other/v1");
    std::fs::write(&path, format!("{row}\n")).expect("archive");

    let read = archive(dir.path(), PULP);
    assert_eq!(read.archive_rows, 1);
    assert_eq!(read.archive_malformed, 1);
    assert!(read.by_job.is_empty());
}

#[test]
fn archive_rejects_oversized_files_before_reading() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive_dir = dir.path().join("causes");
    std::fs::create_dir(&archive_dir).expect("archive dir");
    let path = archive_dir.join("2026-10.jsonl");
    let file = std::fs::File::create(&path).expect("archive");
    file.set_len(MAX_ARCHIVE_BYTES + 1).expect("sparse archive");

    let read = archive(dir.path(), PULP);
    assert!(read.archive_truncated);
    assert_eq!(read.archive_rows, 0);
}
