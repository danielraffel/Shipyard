//! Causes over a captured Pulp merge-group gate: run 38308868279's `macos`
//! job 115009634572 (`tests/fixtures/ctest-log/`), in which
//! `pulp.vellum-closure-external-input-negative` failed, plus small
//! synthetic jobs for the classes the capture does not hold.
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

fn gate_job() -> Job {
    serde_json::from_value(gate_job_payload()).expect("job")
}

fn gate_log() -> String {
    fixture("ctest-log/log-mg-job-115009634572.log")
}

/// A red job with one failed step that ran 00:00:01..00:00:09.
fn synthetic_job(id: i64, conclusion: &str) -> Job {
    serde_json::from_value(json!({
        "id": id,
        "name": "lint",
        "status": "completed",
        "conclusion": conclusion,
        "runner_name": "runner-1",
        "started_at": "2026-10-01T00:00:00Z",
        "completed_at": "2026-10-01T00:00:10Z",
        "run_id": 7,
        "run_attempt": 1,
        "steps": [{
            "number": 3,
            "name": "Check",
            "conclusion": conclusion,
            "started_at": "2026-10-01T00:00:01Z",
            "completed_at": "2026-10-01T00:00:09Z"
        }]
    }))
    .expect("job")
}

fn stamped(lines: &[&str]) -> String {
    let mut text = String::new();
    for line in lines {
        text.push_str("2026-10-01T00:00:05.0000000Z ");
        text.push_str(line);
        text.push('\n');
    }
    text
}

#[test]
fn a_ctest_failure_is_named_by_its_test() {
    let cause = cause_of(&gate_job(), Some(&gate_log())).expect("red");
    assert_eq!(cause.class, "test");
    assert_eq!(cause.confidence, "high");
    assert!(cause.named());
    assert_eq!(cause.signature.as_deref(), Some(VELLUM));
    assert_eq!(cause.failing_tests, vec![VELLUM.to_owned()]);
    assert_eq!(cause.failing_step.as_deref(), Some("Test (non-Windows)"));
    assert_eq!(cause.source, SOURCE_EXTRACTOR);
    assert_eq!(cause.named_at.as_deref(), Some("2026-10-12T21:25:47Z"));
    assert!(cause.retrospective);
    assert!(cause.evidence_line.is_some());
    assert!(!cause.excerpt.is_empty() && cause.excerpt.len() <= MAX_EXCERPT);
}

#[test]
fn without_its_ctest_block_the_same_job_names_no_test() {
    // Negative control: the identical job with the failure lines removed
    // must not still read as a named test failure.
    let stripped: String = gate_log()
        .lines()
        .filter(|line| !line.contains(VELLUM) && !line.contains("The following tests FAILED"))
        .flat_map(|line| [line, "\n"])
        .collect();
    let cause = cause_of(&gate_job(), Some(&stripped)).expect("red");
    assert!(cause.failing_tests.is_empty());
    assert_ne!(cause.class, "test");
    assert_ne!(cause.signature.as_deref(), Some(VELLUM));
}

#[test]
fn a_job_without_a_log_is_not_named() {
    let cause = cause_of(&gate_job(), None).expect("red");
    assert_eq!(cause.confidence, "none");
    assert_eq!(cause.class, "unknown");
    assert!(!cause.named());
    assert!(cause.excerpt.is_empty());
}

#[test]
fn a_scored_error_line_is_named_at_medium_confidence() {
    let log = stamped(&[
        "##[group]Run make",
        "src/widget.cpp:12:3: error: use of undeclared identifier 'Approx'",
        "##[error]Process completed with exit code 2.",
    ]);
    let cause = cause_of(&synthetic_job(11, "failure"), Some(&log)).expect("red");
    assert_eq!(cause.confidence, "medium");
    assert!(cause.named());
    assert!(
        cause
            .signature
            .as_deref()
            .is_some_and(|line| line.contains("use of undeclared identifier"))
    );
}

#[test]
fn only_a_tail_is_low_and_not_named() {
    let log = stamped(&[
        "##[group]Run ./check.sh",
        "sandbox-e2e must run on the sandbox host",
        "##[error]Process completed with exit code 1.",
    ]);
    let cause = cause_of(&synthetic_job(12, "failure"), Some(&log)).expect("red");
    assert_eq!(cause.confidence, "low");
    assert!(!cause.named());
    assert_eq!(
        cause.signature.as_deref(),
        Some("sandbox-e2e must run on the sandbox host")
    );
}

#[test]
fn a_cancel_without_a_reason_is_low() {
    let cause = cause_of(&synthetic_job(13, "cancelled"), Some("")).expect("red");
    assert_eq!(cause.class, "cancelled");
    assert_eq!(cause.confidence, "low");
    assert!(!cause.named());
}

#[test]
fn a_green_job_has_no_cause() {
    let mut job = gate_job();
    job.conclusion = Some("success".to_owned());
    assert_eq!(cause_of(&job, Some(&gate_log())), None);
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
        let path = args.get(1).cloned().unwrap_or_default();
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
    assert_eq!(read.by_job[&GATE_JOB].signature.as_deref(), Some(VELLUM));
    assert_eq!(read.by_job[&42].confidence, "none");
    assert_eq!(read.unreadable.len(), 1);
    assert_eq!(read.unreadable[0].job_id, 7);
    assert!(read.unreadable[0].reason.contains("unexpected request"));
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
    assert!(gate.evidence.is_some());
    assert!(!gate.unread.contains(&"evidence"));
    assert_eq!(
        gate.cause
            .as_ref()
            .and_then(|cause| cause.signature.as_deref()),
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
        BTreeMap::from([
            ("high".to_owned(), 1),
            ("none".to_owned(), 1),
            ("unread".to_owned(), 1),
        ])
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
    assert!(
        rows.iter()
            .all(|row| row.cause.is_none() && row.unread.contains(&"evidence"))
    );
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
