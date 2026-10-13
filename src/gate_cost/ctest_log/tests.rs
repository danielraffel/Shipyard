//! Over two `Generous-Corp/pulp` `macos` gate job logs captured on 2026-10-12
//! (`tests/fixtures/ctest-log/`) and reduced by `reduce_log.py` there
//! (step headers, FAILED blocks, notices, and every result line except a
//! section's passes past its first 120; the second log also keeps
//! `--keep-test pulp.vellum-closure-external-input-negative`), with each
//! job's `actions/runs/{id}/jobs` answer:
//!
//! - job 115009634572, merge-group run 38308868279: fourteen tests, one of
//!   which failed and failed again on its `--repeat until-pass:2` retry;
//! - job 114997948499, pull-request run 38305299000: the fast tier (99 tests),
//!   the affected tests (28), and the full suite as receipt evidence (22,655
//!   results cut to 164: 120 passes, 41 skips, a test that failed and passed
//!   on its retry, and the kept test passing).
use super::*;

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/ctest-log/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn job(jobs: &str, id: u64) -> Value {
    let jobs: Value = serde_json::from_str(&fixture(jobs)).expect("jobs JSON");
    jobs["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .find(|job| job["id"].as_u64() == Some(id))
        .cloned()
        .expect("job")
}

const FAST: &str = "Test fast deterministic tier (pull request head)";
const AFFECTED: &str = "Test what this pull request reaches (pull request head)";

#[test]
fn a_retry_line_has_no_progress_prefix() {
    let text = fixture("log-mg-job-115009634572.log");
    let log = JobLog::new(&text);
    let results = results(log.lines());
    assert_eq!(results.len(), 15, "{results:#?}");
    let retried: Vec<&CtestResult> = results
        .iter()
        .filter(|result| result.name == "pulp.vellum-closure-external-input-negative")
        .collect();
    assert_eq!(retried.len(), 2);
    assert!(!retried[0].retry && retried[1].retry);
    assert!(retried.iter().all(|result| result.outcome == "Failed"));
    assert!(retried.iter().all(|result| result.failed()));
    let passed = results.iter().filter(|result| result.passed()).count();
    assert_eq!(passed, 13);
}

#[test]
fn the_failed_block_names_the_test_without_its_labels() {
    let text = fixture("log-mg-job-115009634572.log");
    let log = JobLog::new(&text);
    let failed = failed_tests(log.lines());
    assert_eq!(failed.len(), 1);
    assert_eq!(
        failed[0].name,
        "pulp.vellum-closure-external-input-negative"
    );
    assert_eq!(failed[0].result, "Failed");
}

#[test]
fn a_skipped_test_neither_passed_nor_failed() {
    let skipped = CtestResult {
        name: "t".to_owned(),
        outcome: "Skipped".to_owned(),
        retry: false,
    };
    assert!(!skipped.passed() && !skipped.failed());
    let disabled = CtestResult {
        outcome: "Not Run (Disabled)".to_owned(),
        ..skipped.clone()
    };
    assert!(!disabled.passed() && !disabled.failed());
    let timeout = CtestResult {
        outcome: "Timeout".to_owned(),
        ..skipped
    };
    assert!(timeout.failed());
}

#[test]
fn headers_map_to_steps_that_start_in_the_same_second() {
    let text = fixture("log-mg-job-115009634572.log");
    let raw = evidence::split_lines(&text);
    let steps = job_steps(&job("jobs-mg-run-38308868279.json", 115_009_634_572));
    let names: Vec<Option<String>> = step_sections(&raw, &steps)
        .into_iter()
        .map(|(_, _, name)| name)
        .collect();
    // Steps 2, 3 and 4 all start at 21:14:13, one header each.
    assert_eq!(
        names[..3],
        [
            Some("Mark job start for the reuse replay record".to_owned()),
            Some("Seed the checkout from the host git mirror (tartci macOS)".to_owned()),
            Some("Run actions/checkout@v7".to_owned()),
        ]
    );
    assert!(names.contains(&Some("Test (non-Windows)".to_owned())));
}

#[test]
fn selection_steps_hold_only_their_own_results() {
    let text = fixture("log-pr-job-114997948499.log");
    let log = JobLog::new(&text);
    let steps = job_steps(&job("jobs-pr-run-38305299000.json", 114_997_948_499));
    let fast = [FAST.to_owned()];
    let (lines, found) = log.lines_of(&steps, &fast);
    assert_eq!(found.into_iter().collect::<Vec<_>>(), vec![FAST.to_owned()]);
    assert_eq!(results(&lines).len(), 99);
    let affected = [AFFECTED.to_owned()];
    let (lines, _) = log.lines_of(&steps, &affected);
    let ran = results(&lines);
    assert_eq!(ran.len(), 28);
    assert_eq!(ran[0].name, "reuse-policy-replay-selftest");
    let both = [FAST.to_owned(), AFFECTED.to_owned()];
    // The full suite (cut to 164) and one GPU-adapter test a later step ran.
    let outside = results(&log.lines_outside(&steps, &both));
    assert_eq!(outside.len(), 165);
    assert_eq!(results(log.lines()).len(), 99 + 28 + 165);
}

#[test]
fn a_failure_that_passed_on_its_retry_reads_as_both() {
    let text = fixture("log-pr-job-114997948499.log");
    let log = JobLog::new(&text);
    let flip: Vec<CtestResult> = results(log.lines())
        .into_iter()
        .filter(|result| result.name == "timeline agent decodes local file URI media locators")
        .collect();
    assert_eq!(flip.len(), 2, "{flip:#?}");
    assert!(!flip[0].retry && flip[0].failed());
    assert!(flip[1].retry && flip[1].passed());
}
