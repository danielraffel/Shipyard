//! The apply-derived advice: named only when derived-file drift is the only
//! failure and CI's expected files exist at the head.

use std::cell::RefCell;
use std::fmt::Write as _;

use crate::app::merge_steward_cmd::derived_drift::advice;
use crate::app::merge_steward_cmd::next_action::Actor;
use crate::merge_steward::{
    DEFAULT_APPROVAL_CONTEXT, EjectionGuards, RequiredCheck, StewardCheck, StewardCheckSource,
    StewardPolicy, StewardPullRequest,
};

const REPO: &str = "acme/widgets";
const CONFIG: &str = r#"
[derived_files]
paths = ["test/ctest_script_inputs/"]
drift_tests = ["script-test-inputs-drift", "changed-surface-script-families-drift"]

[[derived_files.artifacts]]
name = "script-inputs-expected"
root = "test/ctest_script_inputs"
"#;

fn head() -> String {
    "a".repeat(40)
}

fn policy(config: &str) -> StewardPolicy {
    let table: toml::Table = config.parse().unwrap();
    StewardPolicy {
        merge_queue: true,
        native_auto_merge: true,
        approval_context: DEFAULT_APPROVAL_CONTEXT.to_owned(),
        required_checks: ["macos", "drift-fast", "review-verdict"]
            .iter()
            .map(|context| RequiredCheck {
                context: (*context).to_owned(),
                app_id: None,
            })
            .collect(),
        opt_out_label: String::new(),
        provenance_blocking_labels: Vec::new(),
        managed_label: None,
        handoff_context: String::new(),
        max_transient_reruns: 0,
        ejection_guards: EjectionGuards::from_table(&table, true),
    }
}

fn check(name: &str, conclusion: &str, job: u64) -> StewardCheck {
    StewardCheck {
        name: name.to_owned(),
        source: StewardCheckSource::CheckRun,
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        run_id: Some(500),
        observed_at: None,
        check_id: Some(job),
        description: None,
    }
}

fn pr(checks: Vec<StewardCheck>) -> StewardPullRequest {
    StewardPullRequest {
        number: 12,
        changed_files: None,
        commit_count: None,
        head_sha: head(),
        head_branch: "feature".to_owned(),
        author: None,
        draft: false,
        merge_state: "BLOCKED".to_owned(),
        auto_merge_active: false,
        queue_position: None,
        labels: Vec::new(),
        checks,
    }
}

fn failed_block(tests: &[&str]) -> String {
    let mut log = String::from(
        "2026-11-27T10:00:00.0000000Z Errors while running CTest\n\
         2026-11-27T10:00:00.0000000Z The following tests FAILED:\n",
    );
    for (number, test) in tests.iter().enumerate() {
        let _ = writeln!(
            log,
            "2026-11-27T10:00:00.0000000Z \t{} - {test} (Failed)     pr-fast",
            2200 + number
        );
    }
    log
}

/// A GitHub reader answering the runs, artifact and job-log reads.
struct Reads {
    artifact: bool,
    logs: Vec<(u64, String)>,
    calls: RefCell<Vec<String>>,
}

impl Reads {
    fn new(artifact: bool, logs: &[(u64, &[&str])]) -> Self {
        Self {
            artifact,
            logs: logs
                .iter()
                .map(|(job, tests)| (*job, failed_block(tests)))
                .collect(),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn read(&self, args: &[String]) -> Result<String, String> {
        let path = args.last().cloned().unwrap_or_default();
        self.calls.borrow_mut().push(path.clone());
        if path.contains("/actions/runs?head_sha=") {
            return Ok(format!(
                r#"{{"workflow_runs": [
                    {{"id": 500, "conclusion": "failure", "head_sha": "{}"}},
                    {{"id": 400, "conclusion": "success", "head_sha": "{}"}}
                ]}}"#,
                head(),
                head()
            ));
        }
        if path.ends_with("/actions/runs/500/artifacts?per_page=100") {
            let name = if self.artifact {
                "script-inputs-expected"
            } else {
                "ctest-logs-macos"
            };
            return Ok(format!(
                r#"{{"artifacts": [{{"id": 1, "name": "{name}", "expired": false}}]}}"#
            ));
        }
        for (job, log) in &self.logs {
            if path.ends_with(&format!("/actions/jobs/{job}/logs")) {
                return Ok(log.clone());
            }
        }
        Err(format!("unexpected read {path}"))
    }
}

fn advise(reads: &Reads, checks: Vec<StewardCheck>, token: &str, config: &str) -> Option<String> {
    let read = |args: &[String]| reads.read(args);
    advice(&read, REPO, &pr(checks), &policy(config), token).map(|next| {
        assert_eq!(next.actor, Actor::Author);
        next.action
    })
}

#[test]
fn names_the_command_when_drift_is_the_only_failure() {
    let reads = Reads::new(true, &[(11, &["script-test-inputs-drift"])]);
    let action = advise(
        &reads,
        vec![
            check("macos", "FAILURE", 11),
            check("drift-fast", "SUCCESS", 12),
            check("review-verdict", "FAILURE", 13),
        ],
        "required_failed:macos",
        CONFIG,
    )
    .expect("drift-only advice");
    assert!(
        action.starts_with("Derived-file drift only: run `shipyard apply-derived --pr 12 --push`"),
        "{action}"
    );
    assert!(action.contains("run 500"), "{action}");
}

#[test]
fn two_failing_checks_both_on_drift_still_advise() {
    let reads = Reads::new(
        true,
        &[
            (11, &["script-test-inputs-drift"]),
            (12, &["changed-surface-script-families-drift"]),
        ],
    );
    let checks = vec![
        check("macos", "FAILURE", 11),
        check("drift-fast", "FAILURE", 12),
    ];
    assert!(advise(&reads, checks, "required_failed:drift-fast,macos", CONFIG).is_some());
}

#[test]
fn another_failing_test_is_not_drift_only() {
    let reads = Reads::new(
        true,
        &[(
            11,
            &["script-test-inputs-drift", "format-header-view-boundary"],
        )],
    );
    let checks = vec![check("macos", "FAILURE", 11)];
    assert_eq!(
        advise(&reads, checks, "required_failed:macos", CONFIG),
        None
    );
}

#[test]
fn another_failing_check_is_not_drift_only() {
    let reads = Reads::new(
        true,
        &[
            (11, &["script-test-inputs-drift"]),
            (12, &["wide-non-native-selftest"]),
        ],
    );
    let checks = vec![
        check("macos", "FAILURE", 11),
        check("drift-fast", "FAILURE", 12),
    ];
    assert_eq!(
        advise(&reads, checks, "required_failed:drift-fast,macos", CONFIG),
        None
    );
}

#[test]
fn no_artifact_means_no_advice_and_no_log_reads() {
    let reads = Reads::new(false, &[(11, &["script-test-inputs-drift"])]);
    let checks = vec![check("macos", "FAILURE", 11)];
    assert_eq!(
        advise(&reads, checks, "required_failed:macos", CONFIG),
        None
    );
    assert!(
        !reads
            .calls
            .borrow()
            .iter()
            .any(|call| call.contains("/logs")),
        "{:?}",
        reads.calls.borrow()
    );
}

#[test]
fn an_unreadable_log_or_a_failure_naming_no_test_is_not_drift_only() {
    let reads = Reads::new(true, &[]);
    let checks = vec![check("macos", "FAILURE", 11)];
    assert_eq!(
        advise(&reads, checks, "required_failed:macos", CONFIG),
        None
    );
    let reads = Reads::new(true, &[(11, &[])]);
    let checks = vec![check("macos", "FAILURE", 11)];
    assert_eq!(
        advise(&reads, checks, "required_failed:macos", CONFIG),
        None
    );
}

#[test]
fn other_tokens_and_unconfigured_bases_read_nothing() {
    let reads = Reads::new(true, &[(11, &["script-test-inputs-drift"])]);
    let checks = || vec![check("macos", "FAILURE", 11)];
    assert_eq!(advise(&reads, checks(), "required_pending", CONFIG), None);
    assert_eq!(advise(&reads, checks(), "no_verdict", CONFIG), None);
    assert_eq!(
        advise(&reads, checks(), "required_failed:macos", "[auto_merge]\n"),
        None
    );
    assert!(
        reads.calls.borrow().is_empty(),
        "{:?}",
        reads.calls.borrow()
    );
}
