use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;

use super::annotate::{self, Tree};
use super::classify::{cancel_cause, default_fail_closed};
use super::evidence::{self, split_lines};
use super::*;

/// Real CI logs, trimmed. Each fixture's `expected.json` and
/// `expected_check_run.json` were produced by the reference implementation the
/// design was validated with (37 failing required jobs behind a week of merge
/// queue ejections, labelled from how each failure was actually resolved), and
/// the trim was accepted only when it left that implementation's verdict
/// unchanged.
fn fixtures() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/diagnose");
    let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
        .expect("diagnose fixtures exist")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    assert!(
        dirs.len() >= 16,
        "expected the 16 diagnose fixtures, found {}",
        dirs.len()
    );
    dirs
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("fixture file")).expect("fixture json")
}

struct Fixture {
    name: String,
    job: String,
    class: String,
    rule: String,
    jobs: Vec<Job>,
    logs: HashMap<i64, String>,
    required: Vec<String>,
    context: Context,
    tree: Tree,
    head_sha: String,
    expected: Value,
    expected_check_run: Value,
}

fn load(dir: &Path) -> Fixture {
    let case = read_json(&dir.join("case.json"));
    let job = case["job"].as_str().expect("case job").to_owned();
    let jobs: Vec<Job> =
        serde_json::from_value(read_json(&dir.join("jobs.json"))["jobs"].clone()).expect("jobs");
    let target = jobs.iter().find(|j| j.name == job).expect("target job");
    let (target_id, head_sha) = (target.id, target.head_sha.clone().unwrap_or_default());
    let mut logs = HashMap::new();
    if let Ok(text) = fs::read_to_string(dir.join("log.txt")) {
        logs.insert(target_id, text);
    }
    let mut context = Context::new();
    context.stale_markers = vec![Regex::new("not current protected main").expect("marker")];
    let annotations = read_json(&dir.join("annotations.json"));
    for (id, messages) in annotations.as_object().expect("annotations object") {
        context.annotations.insert(
            id.parse().expect("job id"),
            messages
                .as_array()
                .expect("messages")
                .iter()
                .map(|m| m.as_str().expect("message").to_owned())
                .collect(),
        );
    }
    let tree_text = fs::read_to_string(dir.join("tree.txt")).unwrap_or_default();
    Fixture {
        name: dir
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned(),
        head_sha,
        job,
        class: case["class"].as_str().expect("class").to_owned(),
        rule: case["rule"].as_str().expect("rule").to_owned(),
        required: serde_json::from_value(read_json(&dir.join("required.json"))).expect("required"),
        logs,
        jobs,
        context,
        tree: Tree::new(tree_text.lines().filter(|l| !l.is_empty())),
        expected: read_json(&dir.join("expected.json")),
        expected_check_run: read_json(&dir.join("expected_check_run.json")),
    }
}

fn without_bytes(mut value: Value) -> Value {
    if let Some(bounds) = value.get_mut("bounds").and_then(Value::as_object_mut) {
        bounds.remove("bytes");
    }
    value
}

#[test]
fn every_fixture_matches_the_reference_implementation() {
    let mut failures = Vec::new();
    for dir in fixtures() {
        let f = load(&dir);
        let doc = build(&f.jobs, &f.logs, &f.required, &f.context, DEFAULT_MAX_BYTES);
        let got = without_bytes(serde_json::to_value(&doc).expect("serialize"));
        if got != without_bytes(f.expected.clone()) {
            failures.push(format!(
                "{}: document differs\n got: {}\nwant: {}",
                f.name,
                serde_json::to_string(&got).unwrap_or_default(),
                serde_json::to_string(&f.expected).unwrap_or_default()
            ));
        }
        let payload = annotate::check_run_payload(&doc, &f.head_sha, &f.tree);
        let got = serde_json::to_value(&payload.output.annotations).expect("serialize");
        if got != f.expected_check_run["output"]["annotations"] {
            failures.push(format!(
                "{}: annotations differ\n got: {got}\nwant: {}",
                f.name, f.expected_check_run["output"]["annotations"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn every_fixture_has_its_labelled_class_and_rule() {
    for dir in fixtures() {
        let f = load(&dir);
        let doc = build(&f.jobs, &f.logs, &f.required, &f.context, DEFAULT_MAX_BYTES);
        let check = doc
            .checks
            .iter()
            .find(|c| c.context == f.job)
            .unwrap_or_else(|| panic!("{}: no diagnosis for {}", f.name, f.job));
        assert_eq!(
            (
                check.classification.class.as_str(),
                check.classification.rule.as_str()
            ),
            (f.class.as_str(), f.rule.as_str()),
            "{}: {}",
            f.name,
            check.classification.why
        );
    }
}

#[test]
fn bytes_is_exact_and_never_over_the_cap() {
    for dir in fixtures() {
        let f = load(&dir);
        for limit in [900, 1_000, 1_500, 4_000, DEFAULT_MAX_BYTES] {
            let doc = build(&f.jobs, &f.logs, &f.required, &f.context, limit);
            let size = doc.to_compact().len();
            assert_eq!(doc.bounds.bytes, size, "{} at {limit}", f.name);
            assert!(size <= limit, "{} at {limit}: {size} bytes", f.name);
        }
    }
}

fn job(id: i64, name: &str, conclusion: &str) -> Job {
    Job {
        id,
        name: name.to_owned(),
        conclusion: Some(conclusion.to_owned()),
        status: Some("completed".to_owned()),
        runner_name: Some("runner-1".to_owned()),
        run_id: Some(1),
        steps: Some(vec![Step {
            number: Some(1),
            name: "Test".to_owned(),
            conclusion: Some(conclusion.to_owned()),
            started_at: Some("2026-10-01T00:00:00Z".to_owned()),
            completed_at: Some("2026-10-01T00:00:09Z".to_owned()),
        }]),
        ..Job::default()
    }
}

#[test]
fn an_adversarial_run_still_fits_the_cap() {
    let mut log = format!("2026-10-01T00:00:00.0000000Z {}\n", "x".repeat(5_000_000));
    log.push_str("2026-10-01T00:00:01.0000000Z The following tests FAILED:\n");
    let padding = "y".repeat(300);
    for i in 0..3_000 {
        let _ = writeln!(
            log,
            "2026-10-01T00:00:01.0000000Z \t{i} - test_{i}_{padding} (Failed)"
        );
    }
    log.push_str("2026-10-01T00:00:01.0000000Z Errors while running CTest\n");
    let jobs: Vec<Job> = (0..12)
        .map(|k| job(k, &format!("req{k}"), "failure"))
        .collect();
    let logs: HashMap<i64, String> = (0..12).map(|k| (k, log.clone())).collect();
    let required: Vec<String> = (0..12).map(|k| format!("req{k}")).collect();
    let doc = build(&jobs, &logs, &required, &Context::new(), DEFAULT_MAX_BYTES);
    assert!(doc.bounds.bytes <= DEFAULT_MAX_BYTES);
    assert_eq!(doc.bounds.bytes, doc.to_compact().len());
    assert!(doc.bounds.truncated);
    assert_eq!(doc.checks.len() + doc.omitted_checks.len(), 12);
}

#[test]
fn a_green_required_check_yields_no_findings_whatever_else_failed() {
    let jobs = vec![job(1, "macos", "success"), job(2, "linux", "failure")];
    let doc = build(
        &jobs,
        &HashMap::new(),
        &["macos".to_owned()],
        &Context::new(),
        DEFAULT_MAX_BYTES,
    );
    assert_eq!(doc.verdict, "green");
    assert!(doc.checks.is_empty());
    assert_eq!(doc.advisory_failures.count, 1);
}

#[test]
fn a_runnerless_cancel_without_a_recorded_cause_is_never_infra_or_real() {
    let mut never_ran = job(1, "macos", "cancelled");
    never_ran.runner_name = Some(String::new());
    never_ran.steps = Some(Vec::new());
    let doc = build(
        &[never_ran],
        &HashMap::new(),
        &["macos".to_owned()],
        &Context::new(),
        DEFAULT_MAX_BYTES,
    );
    let class = &doc.checks[0].classification;
    assert_eq!(
        (class.class.as_str(), class.rule.as_str()),
        ("interrupted", "unknown")
    );
}

#[test]
fn a_step_window_excludes_errors_printed_by_earlier_steps() {
    let log = "\
2026-10-01T00:00:00.0000000Z error: unrelated failure in an earlier step
2026-10-01T00:00:05.0000000Z running the lint
2026-10-01T00:00:06.0000000Z lint: tools/x.py has a problem
2026-10-01T00:00:07.0000000Z ##[error]Process completed with exit code 1.
";
    let mut lint = job(1, "lint", "failure");
    if let Some(steps) = lint.steps.as_mut() {
        steps[0].started_at = Some("2026-10-01T00:00:05Z".to_owned());
        steps[0].completed_at = Some("2026-10-01T00:00:07Z".to_owned());
    }
    let logs = HashMap::from([(1, log.to_owned())]);
    let doc = build(
        &[lint],
        &logs,
        &["lint".to_owned()],
        &Context::new(),
        DEFAULT_MAX_BYTES,
    );
    let text: Vec<&String> = doc.checks[0]
        .evidence
        .iter()
        .flat_map(|g| &g.lines)
        .collect();
    assert!(
        text.iter()
            .any(|l| l.contains("lint: tools/x.py has a problem"))
    );
    assert!(
        !text.iter().any(|l| l.contains("unrelated failure")),
        "{text:?}"
    );
    assert_eq!(doc.checks[0].more.step_lines, Some([2, 4]));
}

#[test]
fn an_interrupted_or_runner_cause_never_gets_a_position() {
    let tree = Tree::new(["test/foo.cpp"]);
    for (class, rule) in [
        ("interrupted", "timeout"),
        ("interrupted", "superseded"),
        ("infra", "lost_runner"),
        ("infra", "no_runner"),
        ("infra", "needs_starved"),
        ("infra", "non_content_step"),
        ("real", "default"),
    ] {
        let mut doc = build(
            &[],
            &HashMap::new(),
            &[],
            &Context::new(),
            DEFAULT_MAX_BYTES,
        );
        doc.checks.push(CheckDiagnosis {
            context: "macos".to_owned(),
            conclusion: Some("failure".to_owned()),
            run_id: Some(1),
            run_attempt: Some(1),
            job_id: 1,
            runner: Runner {
                name: None,
                labels: Vec::new(),
            },
            timing_s: Timing {
                queued: None,
                ran: None,
            },
            failing_step: None,
            classification: Classification {
                class: class.to_owned(),
                rule: rule.to_owned(),
                why: String::new(),
            },
            failing_tests: FailingTests {
                names: Vec::new(),
                total: 0,
                truncated: false,
            },
            passed_on_retry: None,
            evidence: vec![EvidenceGroup {
                kind: "signal".to_owned(),
                line: 1,
                test: None,
                repeats: None,
                lines: vec!["test/foo.cpp:10: FAILED:".to_owned()],
            }],
            evidence_truncated: false,
            more: More {
                job_url: None,
                log_lines: None,
                log_bytes: None,
                step_lines: None,
                fetch: String::new(),
                note: None,
            },
        });
        let positions = annotate::annotations(&doc, &tree);
        let want = usize::from(class == "real");
        assert_eq!(positions.len(), want, "{class}/{rule}: {positions:?}");
    }
}

#[test]
fn paths_resolve_only_to_one_tree_file() {
    let tree = Tree::new([
        "test/test_ipc.cpp",
        "experimental/pulp-rs/tests/help_parity_test.rs",
        "a/util.py",
        "b/util.py",
    ]);
    assert_eq!(
        tree.resolve("/Users/admin/actions-runner/_work/pulp/pulp/test/test_ipc.cpp")
            .as_deref(),
        Some("test/test_ipc.cpp")
    );
    assert_eq!(
        tree.resolve("/home/runner/work/pulp/pulp/test/test_ipc.cpp")
            .as_deref(),
        Some("test/test_ipc.cpp")
    );
    assert_eq!(
        tree.resolve("tests/help_parity_test.rs").as_deref(),
        Some("experimental/pulp-rs/tests/help_parity_test.rs")
    );
    assert_eq!(tree.resolve("util.py"), None, "ambiguous suffix");
    assert_eq!(
        tree.resolve("/opt/hostedtoolcache/Python/3.12/lib/util.py"),
        None
    );
    assert_eq!(tree.resolve("missing.cpp"), None);
}

#[test]
fn positions_read_the_forms_ci_tools_print() {
    let got = |line: &str| annotate::positions(line);
    assert_eq!(
        got("  /x/test/test_js_engine.cpp:1004: FAILED:"),
        vec![("/x/test/test_js_engine.cpp".to_owned(), 1004)]
    );
    assert_eq!(
        got("thread 'a' panicked at tests/help_parity_test.rs:67:5:")[0],
        ("tests/help_parity_test.rs".to_owned(), 67)
    );
    assert_eq!(
        got("CMake Error at tools/cmake/PulpDependencies.cmake:303 (message):")[0],
        ("tools/cmake/PulpDependencies.cmake".to_owned(), 303)
    );
    assert_eq!(
        got(r#"  File "/usr/lib/python3.12/urllib/request.py", line 639, in x"#)[0],
        ("/usr/lib/python3.12/urllib/request.py".to_owned(), 639)
    );
    assert!(
        got(" see foo.py:12abc").is_empty(),
        "a position must end at a delimiter"
    );
    assert_eq!(
        got(" see foo.py:12:5x")[0],
        ("foo.py".to_owned(), 12),
        "column dropped, line kept"
    );
}

#[test]
fn split_lines_matches_python_splitlines() {
    assert_eq!(split_lines("a\r\nb\rc\nd"), vec!["a", "b", "c", "d"]);
    assert_eq!(split_lines("a\n"), vec!["a"]);
    assert_eq!(split_lines("a\n\nb"), vec!["a", "", "b"]);
    assert!(split_lines("").is_empty());
}

#[test]
fn clip_caps_characters_not_bytes() {
    let long = "é".repeat(300);
    let clipped = evidence::clip(&long);
    assert_eq!(clipped.chars().count(), evidence::MAX_LINE_CHARS);
    assert!(clipped.ends_with('…'));
}

fn two_step_job(test: &str, publish: &str) -> Job {
    let step = |number, name: &str, conclusion: &str, from: &str, to: &str| Step {
        number: Some(number),
        name: name.to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: Some(format!("2026-10-01T00:00:{from}Z")),
        completed_at: Some(format!("2026-10-01T00:00:{to}Z")),
    };
    Job {
        steps: Some(vec![
            step(1, "Test (non-Windows)", test, "00", "04"),
            step(2, "Publish results", publish, "05", "09"),
        ]),
        ..job(1, "macos", "failure")
    }
}

const STALL_LOG: &str = "\
2026-10-01T00:00:01.0000000Z The following tests FAILED:
2026-10-01T00:00:01.0000000Z \t7 - parser_rejects_bad_input (Failed)
2026-10-01T00:00:01.0000000Z Errors while running CTest
2026-10-01T00:00:06.0000000Z Upload progress stalled.
2026-10-01T00:00:07.0000000Z ##[error]Process completed with exit code 1.
";

#[test]
fn an_upload_stall_after_a_green_test_step_is_infra() {
    let green_log = STALL_LOG
        .replace("FAILED", "passed")
        .replace("(Failed)", "");
    let logs = HashMap::from([(1, green_log)]);
    let jobs = [two_step_job("success", "failure")];
    let doc = build(
        &jobs,
        &logs,
        &["macos".to_owned()],
        &Context::new(),
        DEFAULT_MAX_BYTES,
    );
    let class = &doc.checks[0].classification;
    assert_eq!(
        (class.class.as_str(), class.rule.as_str()),
        ("infra", "infra_marker")
    );
}

#[test]
fn a_red_test_step_before_an_upload_stall_stays_real() {
    let logs = HashMap::from([(1, STALL_LOG.to_owned())]);
    let jobs = [two_step_job("failure", "failure")];
    let doc = build(
        &jobs,
        &logs,
        &["macos".to_owned()],
        &Context::new(),
        DEFAULT_MAX_BYTES,
    );
    let check = &doc.checks[0];
    assert_eq!(
        check.classification.class, "real",
        "{}",
        check.classification.why
    );
    assert_eq!(check.failing_tests.names, vec!["parser_rejects_bad_input"]);
}

#[test]
fn every_failing_test_corroborated_elsewhere_reads_flake_candidate() {
    let log = "\
2026-10-01T00:00:01.0000000Z The following tests FAILED:
2026-10-01T00:00:01.0000000Z \t7 - racy_counter (Failed)
2026-10-01T00:00:01.0000000Z \t8 - other_test (Failed)
2026-10-01T00:00:01.0000000Z Errors while running CTest
";
    let logs = HashMap::from([(1, log.to_owned())]);
    let jobs = [job(1, "macos", "failure")];
    let required = ["macos".to_owned()];
    let mut context = Context::new();
    context
        .history
        .insert("racy_counter".to_owned(), vec![9547]);
    let doc = build(&jobs, &logs, &required, &context, DEFAULT_MAX_BYTES);
    assert_eq!(
        doc.checks[0].classification.class, "real",
        "one uncorroborated test keeps it real"
    );
    context.history.insert("other_test".to_owned(), vec![9540]);
    let doc = build(&jobs, &logs, &required, &context, DEFAULT_MAX_BYTES);
    let class = &doc.checks[0].classification;
    assert_eq!(
        (class.class.as_str(), class.rule.as_str()),
        ("flake_candidate", "failed_on_other_heads")
    );
    assert!(
        class.why.contains("9540") && class.why.contains("9547"),
        "{}",
        class.why
    );
}

#[test]
fn a_starved_sibling_from_another_run_does_not_exonerate_the_failure() {
    let mut target = job(1, "macos", "failure");
    target.run_id = Some(1);
    target.steps = Some(vec![Step {
        number: Some(1),
        name: "macOS merge-group bootstrap (required when native leg is absent)".to_owned(),
        conclusion: Some("failure".to_owned()),
        started_at: Some("2026-10-01T00:00:00Z".to_owned()),
        completed_at: Some("2026-10-01T00:00:09Z".to_owned()),
    }]);
    let mut starved = job(2, "classify", "cancelled");
    starved.run_id = Some(2);
    starved.runner_name = Some(String::new());
    starved.steps = Some(Vec::new());
    let mut context = Context::new();
    context.annotations.insert(
        starved.id,
        vec!["The job was not acquired by Runner".to_owned()],
    );
    context.fail_closed = default_fail_closed();
    let mut logs = HashMap::new();
    logs.insert(
        target.id,
        "2026-10-01T00:00:01.0000000Z provider resolution did not succeed — failing macos gate closed\n"
            .to_owned(),
    );
    let doc = build(
        &[target, starved],
        &logs,
        &["macos".to_owned()],
        &context,
        DEFAULT_MAX_BYTES,
    );
    let class = &doc.checks[0].classification;
    assert_eq!(
        (class.class.as_str(), class.rule.as_str()),
        ("real", "default")
    );
}

#[test]
fn a_runnerless_same_run_sibling_is_starved_without_annotation() {
    let mut target = job(1, "macos", "failure");
    target.steps = Some(vec![Step {
        number: Some(1),
        name: "macOS merge-group bootstrap (required when native leg is absent)".to_owned(),
        conclusion: Some("failure".to_owned()),
        started_at: Some("2026-10-01T00:00:00Z".to_owned()),
        completed_at: Some("2026-10-01T00:00:09Z".to_owned()),
    }]);
    let mut starved = job(2, "classify", "cancelled");
    starved.runner_name = Some(String::new());
    starved.steps = Some(Vec::new());
    let mut context = Context::new();
    context.fail_closed = default_fail_closed();
    let logs = HashMap::from([(
        target.id,
        "2026-10-01T00:00:01.0000000Z provider resolution did not succeed — failing macos gate closed\n"
            .to_owned(),
    )]);
    let doc = build(
        &[target, starved],
        &logs,
        &["macos".to_owned()],
        &context,
        DEFAULT_MAX_BYTES,
    );
    let class = &doc.checks[0].classification;
    assert_eq!(
        (class.class.as_str(), class.rule.as_str()),
        ("infra", "needs_starved")
    );
}

#[test]
fn superseded_annotation_uses_environment_requeue_phrase() {
    let message = format!(
        "Canceling since a {} for build-refs/pull/9957/merge exists",
        crate::environment_requeue::SUPERSEDED_ANNOTATIONS[0]
    );
    assert_eq!(cancel_cause(&[message]).map(|(rule, _)| rule), Some("superseded"));
}
