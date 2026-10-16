use serde_json::json;

use super::*;
use crate::diagnose::cause::Place;
use crate::diagnose::{Job, Step};

fn failed_job(step: &str) -> Job {
    Job {
        id: 7,
        name: "macos".to_owned(),
        status: Some("completed".to_owned()),
        conclusion: Some("failure".to_owned()),
        runner_name: Some("runner".to_owned()),
        run_id: Some(1),
        run_attempt: Some(1),
        head_sha: Some("abc".to_owned()),
        steps: Some(vec![Step {
            name: step.to_owned(),
            conclusion: Some("failure".to_owned()),
            number: Some(4),
            ..Step::default()
        }]),
        ..Job::default()
    }
}

fn cause_of(log: &str, step: &str) -> CiCause {
    let place = Place {
        repo: "o/r".to_owned(),
        ..Place::default()
    };
    cause::build(&failed_job(step), Some(log), &place, None, true).expect("red")
}

/// The step's script is echoed between `##[group]Run` and `##[endgroup]`;
/// its comments mention "not run" and "error", and must never be the label.
const NOTRUN_LOG: &str = "\
2026-10-15T00:38:29.2515570Z ##[group]Run set -euo pipefail
2026-10-15T00:38:29.2515571Z # otherwise record a named \"not run\" instead of an unavailable error.
2026-10-15T00:38:29.2515572Z # A suite that DID run and left no report stays an error.
2026-10-15T00:38:29.2515573Z absent=()
2026-10-15T00:38:29.2536200Z ##[endgroup]
2026-10-15T00:50:06.5660480Z selected 412 tests for this pull request head
2026-10-15T00:50:06.5660490Z pr-head-affected-tests: NOTRUN (self-skipped by ctest; not a budget omission): agent-panel-native-invariants
2026-10-15T00:50:06.5660500Z wrote summary.json
2026-10-15T00:50:06.5978220Z ##[error]Process completed with exit code 1.
";

#[test]
fn a_bare_exit_is_labelled_by_its_step_and_its_last_error_line_not_its_script() {
    let mut cause = cause_of(NOTRUN_LOG, "Test what this pull request reaches");
    assert_eq!(cause.format, "shell-exit");
    assert_eq!(label_bare_exit(&mut cause, Some(NOTRUN_LOG)), Some("step"));
    assert_eq!(
        cause.failed_step.as_deref(),
        Some("Test what this pull request reaches")
    );
    assert!(
        cause
            .signature
            .starts_with("pr-head-affected-tests: NOTRUN"),
        "{}",
        cause.signature
    );
    let detail = cause.detail.as_deref().expect("detail");
    assert!(
        detail.starts_with("pr-head-affected-tests: NOTRUN"),
        "{detail}"
    );
    assert_eq!(cause.evidence.log_line, 7);
    assert!(
        cause
            .evidence
            .excerpt
            .iter()
            .all(|line| !line.starts_with('#') && !line.contains("absent=")),
        "{:?}",
        cause.evidence.excerpt
    );
    assert_eq!(cause.confidence, "low");
    assert!(!cause.named());
}

#[test]
fn a_step_that_printed_nothing_after_its_script_has_no_label_line() {
    let log = "\
2026-10-15T00:38:29.2515570Z ##[group]Run exit 3 # a failed error check
2026-10-15T00:38:29.2536200Z ##[endgroup]
2026-10-15T00:38:29.5978220Z ##[error]Process completed with exit code 3.
";
    let mut cause = cause_of(log, "Guard");
    let before = cause.detail.clone();
    assert_eq!(
        label_bare_exit(&mut cause, Some(log)),
        Some("step_no_output")
    );
    // Nothing printed: C2's own name for it (the step's script line) stays,
    // and the step is the failed step.
    assert_eq!(cause.signature, "exit 3 # a failed error check");
    assert_eq!(cause.failed_step.as_deref(), Some("Guard"));
    assert_eq!(cause.detail, before);
    assert!(cause.evidence.excerpt.is_empty());
    assert_eq!(cause.confidence, "low");
}

#[test]
fn a_named_cause_is_not_relabelled() {
    let log = "\
2026-10-15T00:50:06.5660480Z The following tests FAILED:
2026-10-15T00:50:06.5660490Z \t12 - knob_drag (Failed)
2026-10-15T00:50:06.5978220Z ##[error]Process completed with exit code 8.
";
    let mut cause = cause_of(log, "Run ctest");
    assert_eq!(cause.format, "ctest");
    assert_eq!(label_bare_exit(&mut cause, Some(log)), None);
    assert_eq!(cause.signature, "knob_drag");
}

fn row(job_id: u64, job: &str, format: &str, class: &str, confidence: &str) -> Value {
    json!({
        "run_id": 1, "attempt": 1, "job_id": job_id, "job": job,
        "format": format, "failure_class": class, "confidence": confidence,
        "signature": format!("sig-{job_id}"), "detail": format!("detail-{job_id}"),
        "fix_hint": null, "kind": "k", "location": "",
        "evidence": {"log_line": job_id, "excerpt": [format!("line of job {job_id}")]},
        "step_failed": true,
        "failed_step": format!("step of job {job_id}")
    })
}

#[test]
fn a_summary_job_takes_the_cause_of_the_job_it_summarises() {
    // The summary's own values differ from the source's, so a copy that
    // skips a field shows.
    let mut summary_row = row(10, "linux", "shell-exit", "unknown", "low");
    summary_row["step_failed"] = json!(false);
    let mut rows = vec![
        summary_row,
        row(11, "Linux (x64) [github-hosted]", "ctest", "test", "high"),
        row(12, "macos-merge-group", "cargo-test", "test", "high"),
    ];
    assert_eq!(inherit_from_summarised(&mut rows, &[]), 1);
    let summary = &rows[0];
    assert_eq!(summary["format"], "ctest");
    assert_eq!(summary["failure_class"], "test");
    assert_eq!(summary["signature"], "sig-11");
    assert_eq!(summary["inherited_from_job_id"], 11);
    assert_eq!(summary["inherited_from_job"], "Linux (x64) [github-hosted]");
    assert_eq!(summary["labelled_by"], "summarised_job");
    // Its evidence is the source job's, never its own log beside another
    // job's detail.
    assert_eq!(
        summary["evidence"],
        json!({"log_line": 11, "excerpt": ["line of job 11"]})
    );
    assert_eq!(summary["evidence_job_id"], 11);
    assert_eq!(summary["step_failed"], true);
    assert_eq!(summary["failed_step"], "step of job 11");
}

#[test]
fn a_summary_job_inherits_from_an_archived_row_and_only_from_its_own_run_attempt() {
    let mut rows = vec![row(10, "macos", "shell-exit", "unknown", "low")];
    let mut other_attempt = row(20, "macOS (ARM64) [local]", "ctest", "test", "high");
    other_attempt["attempt"] = json!(2);
    let unnamed = row(
        21,
        "macOS (ARM64) [github-hosted]",
        "shell-exit",
        "unknown",
        "low",
    );
    let archived = [
        other_attempt,
        unnamed,
        row(22, "macOS (ARM64) [local]", "compiler", "build", "high"),
    ];
    assert_eq!(inherit_from_summarised(&mut rows, &archived), 1);
    assert_eq!(rows[0]["inherited_from_job_id"], 22);
    assert_eq!(rows[0]["format"], "compiler");
}

#[test]
fn a_job_whose_name_only_shares_a_prefix_is_not_summarised() {
    assert!(summarises("macos", "macOS (ARM64) [local]"));
    assert!(summarises("linux", "Linux [x64]"));
    assert!(!summarises("macos", "macos-merge-group"));
    assert!(!summarises("linux", "linux"));
    let mut rows = vec![
        row(10, "macos", "shell-exit", "unknown", "low"),
        row(12, "macos-merge-group", "cargo-test", "test", "high"),
    ];
    assert_eq!(inherit_from_summarised(&mut rows, &[]), 0);
    assert_eq!(rows[0]["format"], "shell-exit");
}
