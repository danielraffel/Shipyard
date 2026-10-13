use serde_json::{Value, json};

use super::*;

const VELLUM: &str = "pulp.vellum-closure-external-input-negative";
const GATE_JOB: u64 = 115_009_634_572;

fn fixture(path: &str) -> String {
    let path = format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn gate_job() -> Job {
    let jobs: Value =
        serde_json::from_str(&fixture("ctest-log/jobs-mg-run-38308868279.json")).expect("jobs");
    let job = jobs["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .find(|job| job["id"].as_u64() == Some(GATE_JOB))
        .cloned()
        .expect("gate job");
    serde_json::from_value(job).expect("job")
}

fn place() -> Place {
    Place {
        repo: "Generous-Corp/pulp".to_owned(),
        pr: Some(10_154),
        ..Place::default()
    }
}

/// A red job with one failed step named `step`.
fn job(conclusion: &str, step: &str) -> Job {
    serde_json::from_value(json!({
        "id": 7,
        "name": "macos",
        "status": "completed",
        "conclusion": conclusion,
        "runner_name": "runner-1",
        "run_id": 70,
        "run_attempt": 2,
        "head_branch": "feature",
        "steps": [{"number": 1, "name": step, "conclusion": conclusion}]
    }))
    .expect("job")
}

#[test]
fn a_ctest_failure_is_named_by_its_test() {
    let named_at = Some("2026-10-12T21:25:47Z".to_owned());
    let log = fixture("ctest-log/log-mg-job-115009634572.log");
    let cause = build(&gate_job(), Some(&log), &place(), named_at.clone(), true).expect("red");
    assert_eq!(cause.schema, SCHEMA);
    assert_eq!(cause.format, "ctest");
    assert_eq!(cause.signature, VELLUM);
    assert_eq!(
        (cause.failure_class.as_str(), cause.confidence.as_str()),
        ("test", "high")
    );
    assert!(cause.named());
    assert_eq!(cause.job_id, Some(GATE_JOB));
    assert_eq!(cause.run_id, Some(38_308_868_279));
    assert_eq!(cause.event.as_deref(), Some("merge_group"));
    assert_eq!(cause.failed_step.as_deref(), Some("Test (non-Windows)"));
    assert_eq!(cause.named_at, named_at);
    assert!(cause.retrospective);
    assert!(cause.evidence.log_line > 0);
    assert!(!cause.evidence.excerpt.is_empty() && cause.evidence.excerpt.len() <= MAX_EXCERPT);
    assert_eq!(
        (cause.source.as_str(), cause.extractor.as_str()),
        (SOURCE_EXTRACTOR, EXTRACTOR)
    );
}

#[test]
fn without_its_ctest_block_the_job_is_not_named_by_that_test() {
    // Negative control for the test above.
    let log: String = fixture("ctest-log/log-mg-job-115009634572.log")
        .lines()
        .filter(|line| !line.contains(VELLUM) && !line.contains("The following tests FAILED"))
        .flat_map(|line| [line, "\n"])
        .collect();
    let cause = build(&gate_job(), Some(&log), &place(), None, true).expect("red");
    assert_ne!(cause.signature, VELLUM);
    // Nothing else in the reduced log is a failing line: the miss is a
    // record that names nothing.
    assert_eq!(cause.format, "shell-exit");
    assert!(!cause.named());
}

#[test]
fn the_step_that_ended_in_error_names_the_cause() {
    let log = fixture("c2/regressions/step_priority_37215232810.log");
    let cause = build(
        &job("failure", "Post Run actions/checkout@v5"),
        Some(&log),
        &place(),
        None,
        true,
    )
    .expect("red");
    assert_eq!(
        (cause.format.as_str(), cause.signature.as_str()),
        ("infra-runner", "exec format error")
    );
    assert_eq!(
        (cause.failure_class.as_str(), cause.confidence.as_str()),
        ("infra", "high")
    );
}

#[test]
fn an_assertion_outside_the_failing_step_is_low() {
    let log = fixture("c2/regressions/step_priority_37215232810.log");
    let lines = split_lines(&log);
    let demoted = c2::extract(&log)
        .into_iter()
        .find(|record| record.format == "catch2")
        .expect("catch2");
    let cause = to_ci_cause_v1(&demoted, &place(), &lines, None, true);
    assert_eq!(cause.confidence, "low");
    assert!(!cause.named());
}

#[test]
fn a_cancel_with_no_reason_is_githubs_low_record() {
    let cause = build(&job("cancelled", "Test"), Some(""), &place(), None, true).expect("red");
    assert_eq!(cause.format, "infra-cancel");
    assert_eq!(
        (cause.failure_class.as_str(), cause.confidence.as_str()),
        ("cancelled", "low")
    );
    assert!(!cause.named());
}

#[test]
fn githubs_cancel_replaces_an_exit_code_that_names_nothing() {
    let log = "2026-10-05T03:00:00.0000000Z ##[error]Process completed with exit code 143.\n";
    let cause = build(&job("cancelled", "Test"), Some(log), &place(), None, true).expect("red");
    assert_eq!(cause.format, "infra-cancel");
    assert_eq!(cause.failure_class, "cancelled");
}

#[test]
fn a_job_without_a_log_is_log_missing_and_not_named() {
    let cause = build(&gate_job(), None, &place(), None, true).expect("red");
    assert_eq!(cause.format, "log-missing");
    assert_eq!(cause.failure_class, "unknown");
    assert!(!cause.named());
    assert!(cause.evidence.excerpt.is_empty());
}

#[test]
fn a_green_job_has_no_cause() {
    let mut green = gate_job();
    green.conclusion = Some("success".to_owned());
    assert_eq!(build(&green, Some("x"), &place(), None, true), None);
}

#[test]
fn the_mapping_covers_every_fixture_record() {
    let allowed = [
        "ctest",
        "pulp-affected",
        "catch2",
        "unittest",
        "pytest",
        "cargo-test",
        "rust-panic",
        "rustc",
        "cargo-build",
        "compiler",
        "ninja",
        "cmake",
        "node-test",
        "gate-sync",
        "pulp-gate-closed",
        "net-infra",
        "infra-runner",
        "infra-cancel",
        "git-push",
        "github-api",
        "py-exception",
        "shell-exit",
        "log-missing",
        "merge-queue",
    ];
    let mut seen = std::collections::BTreeSet::new();
    for format in c2::FORMATS {
        if matches!(format, "merge_queue" | "log_missing") {
            continue;
        }
        let text = fixture(&format!("c2/{format}.log"));
        let lines = split_lines(&text);
        for record in c2::extract(&text) {
            let cause = to_ci_cause_v1(&record, &place(), &lines, None, true);
            assert!(CLASSES.contains(&cause.failure_class.as_str()), "{format}");
            assert!(["high", "medium", "low"].contains(&cause.confidence.as_str()));
            assert!(allowed.contains(&cause.format.as_str()), "{}", cause.format);
            assert!(cause.evidence.excerpt.len() <= MAX_EXCERPT);
            assert!(
                cause
                    .evidence
                    .excerpt
                    .iter()
                    .all(|line| line.chars().count() <= MAX_EXCERPT_CHARS)
            );
            assert_eq!(cause.evidence.log_line, record.line);
            let own = c2::norm(&c2::strip_line(lines[record.line - 1]), MAX_EXCERPT_CHARS);
            assert!(cause.evidence.excerpt.contains(&own), "{format}");
            assert_eq!(cause.signature, record.subject);
            seen.insert(cause.format);
        }
    }
    let infra = fixture("c2/infra.log");
    let formats: std::collections::BTreeSet<String> = c2::extract(&infra)
        .iter()
        .filter(|record| record.format == "infra")
        .map(|record| to_ci_cause_v1(record, &place(), &[], None, true).format)
        .collect();
    assert_eq!(
        formats,
        std::collections::BTreeSet::from(
            ["infra-cancel", "infra-runner", "net-infra"].map(str::to_owned)
        )
    );
    assert!(seen.contains("pulp-gate-closed"));
}

#[test]
fn the_record_keeps_every_c2_field() {
    // A superset of c2.cause.v0: the C2 record is recoverable from it.
    let text = fixture("c2/catch2.log");
    let record = c2::extract(&text).remove(0);
    let cause = to_ci_cause_v1(&record, &place(), &split_lines(&text), None, true);
    assert_eq!(cause.kind, record.kind);
    assert_eq!(cause.signature, record.subject);
    assert_eq!(cause.detail.as_deref(), Some(record.message.as_str()));
    assert_eq!(cause.location, record.location);
    assert_eq!(cause.evidence.log_line, record.line);
    assert_eq!(cause.step_failed, record.step_failed);
    let value = serde_json::to_value(&cause).expect("json");
    for key in [
        "repo",
        "run_id",
        "attempt",
        "job_id",
        "job",
        "head_sha",
        "event",
        "pr",
        "failed_step",
        "failure_class",
        "signature",
        "detail",
        "evidence",
        "fix_hint",
        "confidence",
        "source",
        "extractor",
        "named_at",
        "supersedes",
    ] {
        assert!(value.get(key).is_some(), "{key}");
    }
}

#[test]
fn named_needs_a_class_and_confidence() {
    let text = fixture("c2/infra.log");
    let cancel = c2::extract(&text)
        .into_iter()
        .find(|record| record.subject == "operation canceled")
        .expect("cancel");
    let bare = to_ci_cause_v1(&cancel, &place(), &[], None, true);
    assert_eq!(
        (bare.failure_class.as_str(), bare.confidence.as_str()),
        ("cancelled", "low")
    );
    assert!(!bare.named());
    let missing = c2::extract_log_fetch_error("Not Found").remove(0);
    let cause = to_ci_cause_v1(&missing, &place(), &[], None, true);
    assert_eq!(cause.confidence, "high");
    assert!(!cause.named());
}

#[test]
fn the_id_names_the_job_attempt_and_source() {
    let log = fixture("ctest-log/log-mg-job-115009634572.log");
    let cause = build(&gate_job(), Some(&log), &place(), None, true).expect("red");
    assert_eq!(
        cause.id(),
        format!("Generous-Corp/pulp/38308868279/1/{GATE_JOB}/extractor")
    );
}

#[test]
fn an_inline_ctest_timeout_is_a_timeout() {
    let text = fixture("c2/ctest_inline.log");
    let timeouts: Vec<CiCause> = c2::extract(&text)
        .iter()
        .filter(|record| record.format == "ctest_inline" && record.message.starts_with("Timeout"))
        .map(|record| to_ci_cause_v1(record, &place(), &[], None, true))
        .collect();
    assert!(!timeouts.is_empty());
    assert!(
        timeouts
            .iter()
            .all(|cause| cause.failure_class == "timeout")
    );
}

#[test]
fn a_bare_cancel_is_low_and_not_named() {
    let text = fixture("c2/regressions/bare_cancel.log");
    let record = c2::extract(&text).remove(0);
    let cause = to_ci_cause_v1(&record, &place(), &split_lines(&text), None, true);
    assert_eq!(
        (
            cause.format.as_str(),
            cause.failure_class.as_str(),
            cause.confidence.as_str()
        ),
        ("infra-cancel", "cancelled", "low")
    );
    assert!(!cause.named());
}

#[test]
fn only_ci_cause_notices_are_read_back_from_a_log() {
    let log = fixture("ctest-log/log-mg-job-115009634572.log");
    let cause = build(&gate_job(), Some(&log), &place(), None, false).expect("red");
    let line = notice_line(&cause);
    let data = line
        .strip_prefix("::notice title=ci-cause::")
        .expect("notice");
    let text = format!(
        "2026-10-12T21:20:07.2208660Z ##[notice]{{\"schema\":\"shipyard-test-tier/v1\",\"tier\":\"full\"}}\n\
         2026-10-12T21:20:07.3000000Z ##[notice]{}\n\
         2026-10-12T21:20:07.4000000Z ##[notice]not json\n\
         2026-10-12T21:20:07.5000000Z ##[notice]{}\n",
        data.replace("%25", "%"),
        data.replace("%25", "%")
            .replace("ci.cause/v1", "ci.cause/v0")
    );
    assert_eq!(records_in_log(&text), vec![cause]);
    assert!(records_in_log(&log).is_empty());
}

#[test]
fn check_run_text_lists_records_and_counts_what_did_not_fit() {
    assert_eq!(check_run_text(&[], 65_535), None);
    let log = fixture("ctest-log/log-mg-job-115009634572.log");
    let cause = build(&gate_job(), Some(&log), &place(), None, true).expect("red");
    let text = check_run_text(&[cause.clone(), cause.clone()], 65_535).expect("text");
    assert_eq!(text.lines().count(), 3);
    let line = text.lines().nth(1).expect("record");
    assert_eq!(
        serde_json::from_str::<CiCause>(line).expect("record"),
        cause
    );
    let one = serde_json::to_string(&cause).expect("json").chars().count();
    let tight = check_run_text(&[cause.clone(), cause], one + 120).expect("text");
    assert!(tight.ends_with("1 record(s) did not fit\n"), "{tight}");
    assert!(tight.chars().count() <= one + 120 + 40);
}

/// One way to forge a record.
type Forge = fn(&mut CiCause);

fn recorded_for(job: &Job) -> (CiCause, CiCause) {
    let log = fixture("ctest-log/log-mg-job-115009634572.log");
    let place = Place {
        repo: "Generous-Corp/pulp".to_owned(),
        ..Place::default()
    };
    let extracted = build(job, Some(&log), &place, None, true).expect("red");
    let mut recorded = extracted.clone();
    recorded.named_at = Some("2026-10-12T21:25:00Z".to_owned());
    recorded.retrospective = false;
    (recorded, extracted)
}

#[test]
fn a_record_corroborates_only_when_bound_to_the_job_and_its_cause() {
    let job = gate_job();
    let (recorded, extracted) = recorded_for(&job);
    let repo = "Generous-Corp/pulp";
    assert!(corroborates(&recorded, &extracted, &job, repo));
    let changed: [(&str, Forge); 7] = [
        ("repo", |r| r.repo = "o/other".to_owned()),
        ("head", |r| r.head_sha = Some("f".repeat(40))),
        ("no head", |r| r.head_sha = None),
        ("run", |r| r.run_id = Some(1)),
        ("attempt", |r| r.attempt = Some(2)),
        ("job", |r| r.job = "linux".to_owned()),
        ("signature", |r| r.signature = "another test".to_owned()),
    ];
    for (what, change) in changed {
        let mut forged = recorded.clone();
        change(&mut forged);
        assert!(!corroborates(&forged, &extracted, &job, repo), "{what}");
    }
    // The record's own time is not evidence of anything.
    let mut early = recorded.clone();
    early.named_at = Some("2000-01-01T00:00:00Z".to_owned());
    assert!(corroborates(&early, &extracted, &job, repo));
}

#[test]
fn a_record_is_timed_by_the_step_that_printed_it() {
    let job = gate_job();
    // Step 52 ran 21:24:00 to 21:25:37.
    assert_eq!(
        printed_at(&job, Some("2026-10-12T21:25:00.1234567Z")).as_deref(),
        Some("2026-10-12T21:25:37Z")
    );
    // Within the API's whole second of the step's end.
    assert_eq!(
        printed_at(&job, Some("2026-10-12T21:25:37.9000000Z")).as_deref(),
        Some("2026-10-12T21:25:37Z")
    );
    assert_eq!(printed_at(&job, Some("2026-10-12T21:30:00Z")), None);
    assert_eq!(printed_at(&job, None), None);
    assert_eq!(printed_at(&job, Some("not a time")), None);
    let log = format!(
        "2026-10-12T21:25:00.5000000Z ##[notice]{}\n##[notice]{}\n",
        serde_json::to_string(&recorded_for(&job).0).expect("json"),
        serde_json::to_string(&recorded_for(&job).0).expect("json"),
    );
    let times: Vec<Option<String>> = records_with_times(&log)
        .into_iter()
        .map(|(_, at)| at)
        .collect();
    assert_eq!(
        times,
        vec![Some("2026-10-12T21:25:00.5000000Z".to_owned()), None]
    );
}

#[test]
fn a_printed_record_is_not_read_as_evidence() {
    // The record quotes an API refusal; C2 must not read it from the log.
    let log = fixture("ctest-log/log-mg-job-115009634572.log");
    let mut quoted = build(&gate_job(), Some(&log), &place(), None, false).expect("red");
    quoted.detail = Some("GitHub API PUT merge failed: HTTP 405".to_owned());
    let line = notice_line(&quoted);
    let data = line
        .strip_prefix("::notice title=ci-cause::")
        .expect("notice")
        .replace("%25", "%");
    let text = format!("{log}2026-10-12T21:25:00.0000000Z ##[notice]{data}\n");
    let cleaned = without_records(&text);
    assert_eq!(cleaned.lines().count(), text.lines().count());
    assert!(
        c2::extract(&cleaned)
            .iter()
            .all(|record| record.format != "github_api")
    );
    // Control: uncleaned, the quoted line would be read.
    assert!(
        c2::extract(&text)
            .iter()
            .any(|record| record.format == "github_api")
    );
    assert_eq!(
        build(&gate_job(), Some(&text), &place(), None, true),
        build(&gate_job(), Some(&log), &place(), None, true)
    );
    // With nothing else in the log, a quoted refusal must not become the
    // job's cause.
    let bare: String = log
        .lines()
        .filter(|line| !line.contains(VELLUM) && !line.contains("The following tests FAILED"))
        .flat_map(|line| [line, "\n"])
        .collect();
    let printed = format!("{bare}2026-10-12T21:25:00.0000000Z ##[notice]{data}\n");
    let cause = build(&gate_job(), Some(&printed), &place(), None, true).expect("red");
    assert_eq!(cause.format, "shell-exit");
    assert!(!cause.named());
}
