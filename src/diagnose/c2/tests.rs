//! The fixture set in `tests/fixtures/c2/` is C2's contract, shared verbatim
//! with EXP-A3's `c2_extract.py`; these tests mirror its own.
use serde_json::Value;

use super::*;

fn dir() -> String {
    format!("{}/tests/fixtures/c2", env!("CARGO_MANIFEST_DIR"))
}

fn read(name: &str) -> String {
    let path = format!("{}/{name}", dir());
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn expected(name: &str) -> Vec<Record> {
    serde_json::from_str(&read(&format!("{name}.expected.json"))).expect("expected records")
}

fn log_fixtures() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir())
        .expect("fixtures")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.strip_suffix(".log").map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

#[test]
fn every_format_has_a_fixture() {
    for format in FORMATS {
        let input = match format {
            "merge_queue" => "json",
            "log_missing" => "txt",
            _ => "log",
        };
        assert!(
            std::path::Path::new(&format!("{}/{format}.{input}", dir())).exists(),
            "{format}"
        );
        assert!(
            std::path::Path::new(&format!("{}/{format}.expected.json", dir())).exists(),
            "{format}"
        );
    }
}

#[test]
fn every_log_fixture_yields_its_expected_records() {
    let names = log_fixtures();
    for name in &names {
        let got = extract(&read(&format!("{name}.log")));
        assert_eq!(got, expected(name), "{name}");
        assert!(
            got.iter().any(|record| record.format == *name),
            "{name}: its own rule must fire on its fixture"
        );
    }
    assert_eq!(names.len(), FORMATS.len() - 2);
    assert_eq!(RULES.len(), FORMATS.len() - 2);
}

#[test]
fn a_merge_queue_removal_is_an_ejection() {
    let event: Value = serde_json::from_str(&read("merge_queue.json")).expect("event");
    assert_eq!(extract_merge_queue_event(&event), expected("merge_queue"));
    assert!(
        extract_merge_queue_event(&serde_json::json!({"event": "added_to_merge_queue"})).is_empty()
    );
}

#[test]
fn a_missing_log_is_its_own_record() {
    assert_eq!(
        extract_log_fetch_error(&read("log_missing.txt")),
        expected("log_missing")
    );
    assert!(extract_log_fetch_error("rate limit exceeded").is_empty());
}

#[test]
fn removing_a_fixtures_marker_removes_its_records() {
    // Negative control per fixture: delete the lines its own rule fired on.
    for name in log_fixtures() {
        let lines: Vec<String> = read(&format!("{name}.log"))
            .lines()
            .map(str::to_owned)
            .collect();
        let hit: std::collections::BTreeSet<usize> = extract(&lines.join("\n"))
            .iter()
            .filter(|record| record.format == name)
            .map(|record| record.line)
            .collect();
        assert!(!hit.is_empty(), "{name}");
        let kept: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(i, _)| !hit.contains(&(i + 1)))
            .map(|(_, line)| line.as_str())
            .collect();
        assert!(
            extract(&kept.join("\n"))
                .iter()
                .all(|record| record.format != name),
            "{name}"
        );
    }
}

#[test]
fn the_regressions_yield_their_expected_records() {
    let mut names: Vec<String> = std::fs::read_dir(format!("{}/regressions", dir()))
        .expect("regressions")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.strip_suffix(".log").map(str::to_owned)
        })
        .collect();
    names.sort();
    for name in &names {
        assert_eq!(
            extract(&read(&format!("regressions/{name}.log"))),
            expected(&format!("regressions/{name}")),
            "{name}"
        );
    }
    assert_eq!(names.len(), 9);
}

fn gate_records(name: &str) -> Vec<Record> {
    extract(&read(&format!("regressions/{name}.log")))
        .into_iter()
        .filter(|record| record.format == "gate_policy")
        .collect()
}

#[test]
fn gate_policy_skips_the_echoed_script() {
    let records = gate_records("gate_policy_echo");
    assert_eq!(
        records
            .iter()
            .map(|record| (record.subject.as_str(), record.line))
            .collect::<Vec<_>>(),
        vec![("provider resolution", 5)]
    );
}

#[test]
fn doc_sync_names_any_path() {
    let subjects: Vec<String> = gate_records("doc_sync_path")
        .into_iter()
        .map(|record| record.subject)
        .collect();
    assert_eq!(subjects, ["docs/ship-state-machine.md", "config-doc"]);
}

#[test]
fn skill_sync_detail_drops_the_period() {
    let records = gate_records("skill_sync_period");
    assert_eq!(
        records
            .iter()
            .map(|record| (record.subject.as_str(), record.message.as_str()))
            .collect::<Vec<_>>(),
        vec![("Skill-sync", "")]
    );
}

#[test]
fn an_exits_tail_walks_past_step_settings() {
    let records = extract(&read("regressions/exit_tail_env.log"));
    assert_eq!(records[0].message, "exit code 1: ./guard.sh");
}

#[test]
fn ctest_names_may_contain_spaces() {
    let names: Vec<String> = extract(&read("ctest_inline.log"))
        .into_iter()
        .map(|record| record.subject)
        .collect();
    assert!(
        names.contains(&"WorkerPool cold-idles workers without blocking later batches".to_owned())
    );
    assert!(names.contains(&"Knob sprite sheet decodes".to_owned()));
    assert!(!names.contains(&"A lane that requires a GPU adapter has one".to_owned()));
}

#[test]
fn catch2_reads_only_the_failing_tests_block() {
    let locations: Vec<String> = extract(&read("regressions/catch2_anchor.log"))
        .into_iter()
        .filter(|record| record.format == "catch2")
        .map(|record| record.location)
        .collect();
    assert_eq!(locations, vec!["test_eq_curve.cpp:57".to_owned()]);
}

#[test]
fn the_step_that_ended_in_error_wins() {
    let records = extract(&read("regressions/step_priority_37215232810.log"));
    assert_eq!(records[0].format, "infra");
    assert_eq!(records[0].subject, "exec format error");
    let stalls: Vec<Option<bool>> = records
        .iter()
        .filter(|record| record.subject == "upload stalled")
        .map(|record| record.step_failed)
        .collect();
    // Retried inside a step that succeeded.
    assert_eq!(stalls, vec![Some(false)]);
    let demoted: Vec<&Record> = records
        .iter()
        .filter(|record| record.format == "catch2")
        .collect();
    assert_eq!(demoted.len(), 1);
    assert_eq!(demoted[0].step_failed, Some(false));
    assert!(signature(&records).starts_with("infra exec format error"));
}

#[test]
fn a_clean_log_yields_no_records() {
    let clean = [
        "2026-10-01T00:00:00.0000000Z ##[group]Run ctest --test-dir build",
        "100% tests passed, 0 tests failed out of 97",
        "test result: ok. 41 passed; 0 failed",
        r#"grep -aE "FAILED:|with expansion:|REQUIRE\(" "$tdir/LastTest.log""#,
        "echo '##[warning]not an error'",
        "Run 14 tests in 0.6s",
    ]
    .join("\n");
    assert_eq!(extract(&clean), Vec::new());
}

#[test]
fn normalization_makes_two_runs_comparable() {
    let mac = "/Users/admin/actions-runner/_work/pulp/pulp/core/x.cpp:12:3: error: boom at 0x7ffee1234567 in 1.5 sec";
    let linux =
        "/home/runner/work/pulp/pulp/core/x.cpp:12:3: error: boom at 0x10000ab00 in 9.25 sec";
    assert_eq!(extract(mac), extract(linux));
    let a = "##[error]Skia prebuilt not found under /Users/admin/actions-runner/_work/pulp/pulp/external/skia-build";
    let b =
        "##[error]Skia prebuilt not found under /home/runner/work/pulp/pulp/external/skia-build";
    assert_eq!(extract(a), extract(b));
    assert_eq!(
        extract(a)[0].message,
        "Skia prebuilt not found under external/skia-build"
    );
}

#[test]
fn the_signature_dedups_and_keeps_order() {
    let records = extract(&read("cargo_test.log"));
    let text = signature(&records);
    assert!(
        text.lines().next().is_some_and(
            |line| line.starts_with("test_failure merge::tests::queue_order_is_stable")
        )
    );
    assert_eq!(text.lines().count(), 3);
    let doubled: Vec<Record> = records.iter().chain(&records).cloned().collect();
    assert_eq!(signature(&doubled), text);
}

#[test]
fn extraction_is_deterministic() {
    let text = read("unittest.log");
    assert_eq!(extract(&text), extract(&text));
}

#[test]
fn an_infra_subject_has_its_class() {
    assert_eq!(infra_class("operation canceled"), "cancelled");
    assert_eq!(infra_class("exec format error"), "infra");
    assert_eq!(infra_class("ENOTFOUND"), "infra");
}

#[test]
fn an_exits_tail_skips_step_settings() {
    let log = [
        "##[group]Run ./guard.sh",
        "refusing: the head is stale",
        "shell: /usr/bin/bash -e {0}",
        "env:",
        "  CARGO_HOME: /opt/cargo",
        "CCACHE_DIR: /tmp/cache",
        "##[endgroup]",
        "##[error]Process completed with exit code 1.",
    ]
    .join("\n");
    let records = extract(&log);
    assert_eq!(
        records[0].message,
        "exit code 1: refusing: the head is stale"
    );
}
