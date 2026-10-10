use std::collections::HashMap;

use serde_json::{Value, json};

use super::*;
use crate::pr_queue_state::explain_pr_queue_state;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/github");
const REPO: &str = "Generous-Corp/pulp";

fn fixture(name: &str) -> Value {
    let raw = std::fs::read_to_string(format!("{FIXTURES}/{name}")).expect("fixture");
    serde_json::from_str(&raw).expect("fixture JSON")
}

fn job_fixture(name: &str) -> Value {
    fixture(&format!("job_logs/{name}"))
}

fn failing_steps(job: &Value) -> Vec<(String, String)> {
    job["job"]["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter(|step| step["conclusion"] == "failure")
        .map(|step| {
            (
                step["name"].as_str().expect("name").to_owned(),
                step["started_at"].as_str().expect("started").to_owned(),
            )
        })
        .collect()
}

#[test]
fn every_job_log_matches_the_shared_expectations() {
    let expected = job_fixture("expected_environment_signatures.json");
    let mut checked = 0;
    for (name, steps) in expected.as_object().expect("object") {
        if name.starts_with('_') {
            continue;
        }
        let job = job_fixture(name);
        let log = job["log"].as_str().expect("log");
        let failing = failing_steps(&job);
        assert_eq!(
            failing.len(),
            steps.as_object().expect("steps").len(),
            "{name}"
        );
        for (step, started) in failing {
            let want = &steps[&step];
            let reading = read_failing_step(log, &started);
            match &reading {
                StepReading::Environment(hit) => {
                    assert_eq!(want["reading"], "environment", "{name}/{step}");
                    assert_eq!(want["signature"], hit.signature.as_str(), "{name}/{step}");
                }
                StepReading::Interruption(hit) => {
                    assert_eq!(want["reading"], "interruption", "{name}/{step}");
                    assert_eq!(want["signature"], hit.signature.as_str(), "{name}/{step}");
                }
                StepReading::NoSignature => {
                    assert_eq!(want["reading"], "no_signature", "{name}/{step}");
                }
                StepReading::NotLocated => panic!("{name}/{step}: output not located"),
            }
            checked += 1;
        }
    }
    // Control: every failing step of every excerpt was read.
    assert_eq!(checked, 6);
}

#[test]
fn a_later_continue_on_error_step_is_not_the_failing_step() {
    // The pip excerpt ends with an `if: always()` step that errors with exit 2
    // and carries no signature. Reading "the first ##[error] of the log" would
    // still work here, so pin the stronger property: the output read for the
    // failing step stops at its own error line.
    let job = job_fixture("job_real_pip_relay_403.json");
    let (_, started) = failing_steps(&job).remove(0);
    let output = failing_step_output(job["log"].as_str().expect("log"), &started).expect("output");
    let last = output.last().expect("line");
    assert_eq!(*last, "##[error]Process completed with exit code 1.");
    assert!(!output.iter().any(|line| line.contains("exit code 2")));
}

/// The trap the task names: a step's own script can quote the signature.
/// Real lines: the `Install visual-analysis Python dependencies` step of
/// #8933's job, whose script comment quotes the pip relay 403, followed by
/// its real (passing) output. Only the failure line is appended.
#[test]
fn a_signature_in_the_script_echo_is_not_output() {
    let job = job_fixture("job_real_8933_test_failure.json");
    let log = job["log"].as_str().expect("log");
    let mut lines = log.lines();
    let mut segment = vec![lines.next().expect("first line")];
    segment.extend(lines.take_while(|line| !line.contains("##[group]Run ")));
    assert!(
        segment[0].contains("##[group]Run set -euo pipefail"),
        "excerpt starts at the pip step"
    );
    // Control: the signature IS in the text handed over.
    assert!(
        segment
            .iter()
            .any(|line| line.contains("Tunnel connection failed"))
    );
    let started = &segment[0][..20];
    let mut synthetic = segment.join("\n");
    synthetic
        .push_str("\n2026-09-29T06:33:30.0000000Z ##[error]Process completed with exit code 1.\n");
    assert_eq!(
        read_failing_step(&synthetic, started),
        StepReading::NoSignature
    );
    // The property itself, independent of the proximity window: no echoed
    // script line is read as output.
    let output = failing_step_output(&synthetic, started).expect("output");
    assert!(
        !output
            .iter()
            .any(|line| line.contains("Tunnel connection failed"))
    );
    assert!(
        !output
            .iter()
            .any(|line| line.starts_with("set -euo pipefail"))
    );
}

#[test]
fn a_signature_far_from_the_failure_explains_nothing() {
    let header = "2026-09-29T00:00:00.0000000Z ##[group]Run make\n2026-09-29T00:00:00.1000000Z make\n2026-09-29T00:00:00.2000000Z ##[endgroup]\n";
    let line = |text: &str| format!("2026-09-29T00:00:01.0000000Z {text}\n");
    let build = |gap: usize| {
        let mut log = header.to_owned();
        log.push_str(&line(
            "warning: spurious network error: Could not resolve host: index.crates.io",
        ));
        for _ in 0..gap {
            log.push_str(&line("[12/40] Compiling foo.cpp"));
        }
        log.push_str(&line("foo.cpp:1:1: error: expected ';'"));
        log.push_str(&line("##[error]Process completed with exit code 1."));
        log
    };
    // Signature, gap lines, the compile error, the ##[error]: the window is
    // FAILURE_PROXIMITY_LINES lines ending at the ##[error].
    let edge = FAILURE_PROXIMITY_LINES - 3;
    assert!(matches!(
        read_failing_step(&build(edge), "2026-09-29T00:00:00Z"),
        StepReading::Environment(_)
    ));
    assert_eq!(
        read_failing_step(&build(edge + 1), "2026-09-29T00:00:00Z"),
        StepReading::NoSignature
    );
}

#[test]
fn a_step_is_read_from_its_own_start_not_an_earlier_error() {
    // Real log: Chrome fails with curl (56) and a later continue-on-error step
    // errors with exit 2. Read at the later step's start, the earlier step's
    // signature must not be the answer.
    let job = job_fixture("job_real_chrome_proxy_connect.json");
    let later = job["job"]["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .find(|step| {
            step["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("Observe ctest non-runs"))
        })
        .expect("later step");
    assert_eq!(
        read_failing_step(
            job["log"].as_str().expect("log"),
            later["started_at"].as_str().expect("started")
        ),
        StepReading::NoSignature
    );
}

#[test]
fn a_step_with_no_run_segment_is_not_located() {
    let job = job_fixture("job_real_chrome_proxy_connect.json");
    // A start after every segment in the excerpt.
    assert_eq!(
        read_failing_step(job["log"].as_str().expect("log"), "2026-09-30T00:00:00Z"),
        StepReading::NotLocated
    );
}

#[test]
fn transport_markers_are_shared_with_the_infra_classifier() {
    for marker in NETWORK_TRANSPORT_MARKERS {
        assert!(ENVIRONMENT_SIGNATURES.contains(&marker));
        assert_eq!(
            crate::classify::classify_failure("", marker, 1, false, false),
            crate::classify::FailureClass::Infra,
            "{marker}"
        );
    }
}

/// A `gh` stand-in answering from real captures.
struct FakeGh {
    answers: HashMap<String, Result<String, String>>,
}

impl FakeGh {
    fn new() -> Self {
        let checks = fixture("merge_group_required_checks_real.json");
        let mut answers = HashMap::new();
        answers.insert(
            format!("repos/{REPO}/rules/branches/main"),
            Ok(checks["rules_branches_main"].to_string()),
        );
        answers.insert(
            format!("repos/{REPO}/branches/main/protection/required_status_checks"),
            Ok(checks["required_status_checks"].to_string()),
        );
        for (sha, commit) in checks["commits"].as_object().expect("commits") {
            answers.insert(
                format!("repos/{REPO}/commits/{sha}/check-runs?per_page=100"),
                Ok(commit["check_runs"].to_string()),
            );
            answers.insert(
                format!("repos/{REPO}/commits/{sha}/status"),
                Ok(commit["status"].to_string()),
            );
        }
        for pr in [8678, 8911, 8933] {
            answers.insert(
                format!("repos/{REPO}/pulls/{pr}"),
                Ok(json!({"base": {"ref": "main"}}).to_string()),
            );
        }
        for name in [
            "job_real_pip_relay_403.json",
            "job_real_cargo_dns_in_build.json",
            "job_real_8933_test_failure.json",
        ] {
            let job = job_fixture(name);
            let id = job["job"]["id"].as_u64().expect("id");
            answers.insert(
                format!("repos/{REPO}/actions/jobs/{id}"),
                Ok(job["job"].to_string()),
            );
            answers.insert(
                format!("repos/{REPO}/actions/jobs/{id}/logs"),
                Ok(job["log"].as_str().expect("log").to_owned()),
            );
        }
        Self { answers }
    }

    fn call(&self, args: &[String]) -> Result<String, String> {
        assert_eq!(args[0], "api");
        self.answers
            .get(&args[1])
            .cloned()
            .unwrap_or_else(|| Err(format!("unexpected call {}", args[1])))
    }
}

fn assess_fixture(gh: &FakeGh, pr_fixture: &str, opted_in: bool) -> Option<EnvironmentRequeue> {
    let report = explain_pr_queue_state(&fixture(pr_fixture));
    assess(&|args: &[String]| gh.call(args), REPO, &report, opted_in)
}

#[test]
fn pip_relay_ejection_of_8678_allows_one_re_enqueue() {
    let verdict = assess_fixture(
        &FakeGh::new(),
        "pr_real_8678_first_environment_ejection.json",
        true,
    )
    .expect("applies");
    assert!(verdict.allowed, "{}", verdict.reason);
    assert_eq!(verdict.evidence.len(), 1);
    assert_eq!(verdict.evidence[0].check, "macos");
    assert_eq!(
        verdict.evidence[0].step,
        "Install visual-analysis Python dependencies"
    );
    // The advisory Linux job also failed on that commit and is not read.
    assert!(
        verdict
            .evidence
            .iter()
            .all(|step| !step.check.starts_with("Linux"))
    );
}

#[test]
fn cargo_dns_ejection_of_8911_allows_one_re_enqueue() {
    let verdict = assess_fixture(
        &FakeGh::new(),
        "pr_real_8911_environment_ejection.json",
        true,
    )
    .expect("applies");
    assert!(verdict.allowed, "{}", verdict.reason);
    assert!(matches!(
        &verdict.evidence[0].reading,
        StepReading::Environment(hit) if hit.signature == "Could not resolve host"
    ));
}

#[test]
fn test_failure_ejection_of_8933_stays_refused() {
    let verdict = assess_fixture(
        &FakeGh::new(),
        "pr_real_8933_test_failure_ejection.json",
        true,
    )
    .expect("applies");
    assert!(!verdict.allowed);
    // Refused for the test failure itself: this was the head's first
    // failed_checks ejection, so the bound is not what refuses it.
    assert!(
        verdict.reason.contains("macos / Test (non-Windows)"),
        "{}",
        verdict.reason
    );
    assert_eq!(verdict.evidence[0].reading, StepReading::NoSignature);
}

#[test]
fn a_second_ejection_of_the_same_head_is_refused() {
    // The real capture of 8678's second ejection: its merge-group commit has
    // no answers here, so it refuses whether or not the logs are read.
    let verdict = assess_fixture(
        &FakeGh::new(),
        "pr_real_8678_second_ejection_same_head.json",
        true,
    )
    .expect("applies");
    assert!(!verdict.allowed, "{}", verdict.reason);
}

#[test]
fn a_network_failure_allows_only_the_first_ejection() {
    let gh = FakeGh::new();
    let mut report =
        explain_pr_queue_state(&fixture("pr_real_8678_first_environment_ejection.json"));
    report.ejections_of_current_head = 2;
    let verdict = assess(&|args: &[String]| gh.call(args), REPO, &report, true).expect("applies");
    assert!(!verdict.allowed);
    assert!(verdict.reason.contains("2 times"), "{}", verdict.reason);
    assert!(
        matches!(&verdict.evidence[0].reading, StepReading::Environment(_)),
        "the evidence was network, and network allows one"
    );
}

const GROUP: &str = "c0ffee0000000000000000000000000000000001";

/// A merge group whose required `macos` check ended `conclusion` on Actions
/// job 7, with `job` as that job's API record and `log` as its log.
fn interrupted_group(
    conclusion: &str,
    job: &Value,
    log: &str,
) -> HashMap<String, Result<String, String>> {
    let checks = fixture("merge_group_required_checks_real.json");
    let mut answers = HashMap::new();
    answers.insert(
        format!("repos/{REPO}/rules/branches/main"),
        Ok(checks["rules_branches_main"].to_string()),
    );
    answers.insert(
        format!("repos/{REPO}/branches/main/protection/required_status_checks"),
        Ok(json!({"contexts": ["macos"]}).to_string()),
    );
    answers.insert(
        format!("repos/{REPO}/pulls/8678"),
        Ok(json!({"base": {"ref": "main"}}).to_string()),
    );
    answers.insert(
        format!("repos/{REPO}/commits/{GROUP}/check-runs?per_page=100"),
        Ok(json!({"total_count": 1, "check_runs": [{
            "id": 7, "name": "macos", "status": "completed", "conclusion": conclusion,
            "app": {"slug": "github-actions"}
        }]})
        .to_string()),
    );
    answers.insert(
        format!("repos/{REPO}/commits/{GROUP}/status"),
        Ok(json!({"statuses": []}).to_string()),
    );
    answers.insert(format!("repos/{REPO}/actions/jobs/7"), Ok(job.to_string()));
    answers.insert(
        format!("repos/{REPO}/check-runs/7/annotations"),
        Ok("[]".to_owned()),
    );
    answers.insert(
        format!("repos/{REPO}/actions/jobs/7/logs"),
        Ok(log.to_owned()),
    );
    answers
}

fn assess_group(
    answers: &HashMap<String, Result<String, String>>,
    ejections: u32,
) -> EnvironmentRequeue {
    let mut report =
        explain_pr_queue_state(&fixture("pr_real_8678_first_environment_ejection.json"));
    report.ejections_of_current_head = ejections;
    if let Some(ejection) = report.last_ejection.as_mut() {
        ejection.merge_group_commit = Some(GROUP.to_owned());
    }
    let gh = |args: &[String]| {
        answers
            .get(&args[1])
            .cloned()
            .unwrap_or_else(|| Err(format!("unexpected call {}", args[1])))
    };
    assess(&gh, REPO, &report, true).expect("applies")
}

fn queued_job(runner: &str, created: &str, completed: &str) -> Value {
    json!({
        "id": 7, "status": "completed", "conclusion": "cancelled",
        "created_at": created, "started_at": null, "completed_at": completed,
        "runner_name": runner, "labels": ["pulp-gate"], "steps": []
    })
}

#[test]
fn a_required_job_starved_of_a_runner_is_an_interruption() {
    // #9650/#9657/#9658's shape: cancelled 16 min after creation, no runner.
    let job = queued_job("", "2026-10-08T10:00:00Z", "2026-10-08T10:16:00Z");
    let verdict = assess_group(&interrupted_group("cancelled", &job, ""), 1);
    assert!(verdict.allowed, "{}", verdict.reason);
    assert!(verdict.is_allowed_interruption());
    assert!(matches!(
        &verdict.evidence[0].reading,
        StepReading::Interruption(hit) if hit.signature == "starved"
    ));
    // The bound: a second interruption is allowed, a third is not.
    assert!(assess_group(&interrupted_group("cancelled", &job, ""), 2).allowed);
    let third = assess_group(&interrupted_group("cancelled", &job, ""), 3);
    assert!(
        !third.allowed && third.reason.contains("3 times"),
        "{}",
        third.reason
    );
}

#[test]
fn a_concurrency_supersede_is_not_starvation() {
    // Run 37891885987's macos job: no runner, 16.5 min queued, then GitHub
    // cancelled it for a higher-priority run in its concurrency group.
    let job = queued_job("", "2026-10-08T10:00:00Z", "2026-10-08T10:16:30Z");
    let mut answers = interrupted_group("cancelled", &job, "");
    answers.insert(
        format!("repos/{REPO}/check-runs/7/annotations"),
        Ok(json!([{"annotation_level": "failure", "message":
            "Canceling since a higher priority waiting request for build-refs/pull/9957/merge exists"}])
        .to_string()),
    );
    let verdict = assess_group(&answers, 1);
    assert!(!verdict.allowed, "{}", verdict.reason);
    assert_eq!(verdict.evidence[0].reading, StepReading::NoSignature);
    // Control: the same job with no annotation is starved.
    assert!(assess_group(&interrupted_group("cancelled", &job, ""), 1).allowed);
    // Unreadable annotations cannot rule a supersede out.
    let mut unreadable = interrupted_group("cancelled", &job, "");
    unreadable.insert(
        format!("repos/{REPO}/check-runs/7/annotations"),
        Err("HTTP 502".to_owned()),
    );
    assert!(!assess_group(&unreadable, 1).allowed);
}

#[test]
fn a_cancel_that_had_a_runner_or_never_waited_is_not_starvation() {
    for (runner, completed, why) in [
        (
            "studio-pulp-gate-01",
            "2026-10-08T10:16:00Z",
            "a runner took it",
        ),
        (
            "",
            "2026-10-08T10:02:00Z",
            "a 2-minute cancel is a superseding push",
        ),
        ("", "", "no completion time fails closed"),
    ] {
        let mut job = queued_job(runner, "2026-10-08T10:00:00Z", completed);
        if completed.is_empty() {
            job["completed_at"] = Value::Null;
        }
        let verdict = assess_group(&interrupted_group("cancelled", &job, ""), 1);
        assert!(!verdict.allowed, "{why}: {}", verdict.reason);
        assert_eq!(
            verdict.evidence[0].reading,
            StepReading::NoSignature,
            "{why}"
        );
    }
}

const UPLOAD_STALL_LOG: &str = "\
2026-10-09T23:10:00.0000000Z ##[group]Run cargo test
2026-10-09T23:10:00.0000000Z cargo test
2026-10-09T23:10:00.0000000Z ##[endgroup]
2026-10-09T23:30:00.0000000Z test result: ok. 22592 passed; 0 failed
2026-10-09T23:30:01.0000000Z ##[group]Run actions/upload-artifact@v4
2026-10-09T23:30:01.0000000Z with:
2026-10-09T23:30:01.0000000Z ##[endgroup]
2026-10-09T23:30:02.0000000Z Uploading 812 files
2026-10-09T23:37:00.0000000Z ##[error]Upload progress stalled.
";

fn upload_job(test_conclusion: &str) -> Value {
    json!({
        "id": 7, "status": "completed", "conclusion": "failure",
        "created_at": "2026-10-09T23:00:00Z", "completed_at": "2026-10-09T23:37:00Z",
        "runner_name": "studio-pulp-gate-01",
        "steps": [
            {"name": "Test", "conclusion": test_conclusion, "started_at": "2026-10-09T23:10:00Z"},
            {"name": "Upload test artifacts", "conclusion": "failure", "started_at": "2026-10-09T23:30:01Z"}
        ]
    })
}

#[test]
fn an_upload_that_stalls_after_a_green_test_step_is_an_interruption() {
    // #9976, job 114059088753: 22592/22592 passed, then the upload stalled.
    let verdict = assess_group(
        &interrupted_group("failure", &upload_job("success"), UPLOAD_STALL_LOG),
        1,
    );
    assert!(verdict.allowed, "{}", verdict.reason);
    assert!(verdict.is_allowed_interruption());
    assert_eq!(verdict.evidence.len(), 1);
    assert_eq!(verdict.evidence[0].step, "Upload test artifacts");
}

#[test]
fn a_recovered_stall_earlier_in_the_step_does_not_explain_its_failure() {
    // The upload stalled, recovered, and later failed for another reason.
    let log = UPLOAD_STALL_LOG.replace(
        "2026-10-09T23:37:00.0000000Z ##[error]Upload progress stalled.\n",
        "2026-10-09T23:31:00.0000000Z Upload progress stalled.\n\
         2026-10-09T23:32:00.0000000Z Retrying upload\n\
         2026-10-09T23:36:00.0000000Z Finalizing artifact\n\
         2026-10-09T23:37:00.0000000Z ##[error]Artifact storage quota has been hit.\n",
    );
    let verdict = assess_group(
        &interrupted_group("failure", &upload_job("success"), &log),
        1,
    );
    assert!(!verdict.allowed, "{}", verdict.reason);
    assert_eq!(verdict.evidence[0].reading, StepReading::NoSignature);
}

#[test]
fn a_real_red_before_a_stalled_upload_stays_refused() {
    let log = UPLOAD_STALL_LOG.replace(
        "test result: ok. 22592 passed; 0 failed",
        "test result: FAILED. 22591 passed; 1 failed\n2026-10-09T23:30:00.5000000Z ##[error]Process completed with exit code 101.",
    );
    let verdict = assess_group(
        &interrupted_group("failure", &upload_job("failure"), &log),
        1,
    );
    assert!(!verdict.allowed, "{}", verdict.reason);
    assert!(verdict.reason.contains("Test"), "{}", verdict.reason);
}

#[test]
fn a_repository_that_has_not_opted_in_is_refused() {
    let verdict = assess_fixture(
        &FakeGh::new(),
        "pr_real_8678_first_environment_ejection.json",
        false,
    )
    .expect("applies");
    assert!(!verdict.allowed);
    assert!(verdict.reason.contains(CONFIG_KEY));
}

#[test]
fn only_same_head_failed_checks_ejections_are_assessed() {
    let gh = FakeGh::new();
    for name in [
        "pr_queued.json",
        "pr_never_armed.json",
        "pr_ejected_new_head.json",
        "pr_merged.json",
    ] {
        assert!(assess_fixture(&gh, name, true).is_none(), "{name}");
    }
}

#[test]
fn a_failed_job_with_no_failing_step_refuses() {
    let mut gh = FakeGh::new();
    let path = format!("repos/{REPO}/actions/jobs/107036733221");
    let mut job: Value =
        serde_json::from_str(gh.answers[&path].as_ref().expect("job")).expect("json");
    for step in job["steps"].as_array_mut().expect("steps") {
        step["conclusion"] = Value::from("success");
    }
    gh.answers.insert(path, Ok(job.to_string()));
    let verdict =
        assess_fixture(&gh, "pr_real_8678_first_environment_ejection.json", true).expect("applies");
    assert!(!verdict.allowed);
    assert!(
        verdict.reason.contains("no failing step recorded"),
        "{}",
        verdict.reason
    );
}

#[test]
fn an_unreadable_log_refuses() {
    let mut gh = FakeGh::new();
    gh.answers.insert(
        format!("repos/{REPO}/actions/jobs/107036733221/logs"),
        Err("HTTP 404: Not Found".to_owned()),
    );
    let verdict =
        assess_fixture(&gh, "pr_real_8678_first_environment_ejection.json", true).expect("applies");
    assert!(!verdict.allowed);
    assert!(
        verdict.reason.contains("could not be read"),
        "{}",
        verdict.reason
    );
}

#[test]
fn a_failing_required_commit_status_refuses() {
    let mut gh = FakeGh::new();
    gh.answers.insert(
        format!("repos/{REPO}/commits/2410ca497342cfc0264bf5b72713de0b56099a0f/status"),
        Ok(json!({"statuses": [{"context": "Vellum freeze", "state": "failure"}]}).to_string()),
    );
    let verdict =
        assess_fixture(&gh, "pr_real_8678_first_environment_ejection.json", true).expect("applies");
    assert!(!verdict.allowed);
    assert!(
        verdict.reason.contains("not a GitHub Actions job"),
        "{}",
        verdict.reason
    );
}
