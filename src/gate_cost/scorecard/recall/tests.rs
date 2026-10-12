//! Selector recall over answers captured from `Generous-Corp/pulp` on
//! 2026-10-12 UTC (`tests/fixtures/gate-cost-scorecard/recall/`), with the
//! same requests [`gather`] sends. Job logs are reduced by
//! `tests/fixtures/ctest-log/reduce_log.py`; a pull request and a commit keep
//! only the fields read
//! (`number`, `merged_at`, `merge_commit_sha`; `sha`, `parents`).
//!
//! - window 2026-10-12 05:40..05:50: three merge-group runs, each built on
//!   the one before (#10115, #10133, #10134), all failing
//!   `build_migration_index.py - frontmatter -> embedded C++ round-trip`.
//!   #10115's group also holds #10114, which merged at 05:52 as the commit
//!   #10115's branch was built on; #10114's own branch base had merged
//!   before, so the walk stops there. #10134's head needed no native build:
//!   its `macos` job ran no selection step.
//!
//! - window 2026-10-11 13:01..13:03: #10071's group, then #10053's built on
//!   it, both failing `vellum-inheritance-ledger` and its negative contract;
//!   #10071's head ran both in its affected-test step, and they passed
//!   there.
//!
//! Answers are `mg-runs-<from>.json` (a window's merge-group runs),
//! `mg-runs-on-<sha>.json` and `runs-on-<sha>.json` (runs on one commit),
//! `jobs-run-<id>.json`, `log-job-<id>.log`, `pull-<n>.json` and
//! `commit-<sha>.json`.
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use super::super::{ScorecardObservation, read_gates};
use super::*;

const PULP: &str = "Generous-Corp/pulp";
const FAST: &str = "Test fast deterministic tier (pull request head)";
const AFFECTED: &str = "Test what this pull request reaches (pull request head)";
const MIGRATION: &str = "build_migration_index.py - frontmatter -> embedded C++ round-trip";

fn fixture(name: &str) -> Result<String, String> {
    let path = format!(
        "{}/tests/fixtures/gate-cost-scorecard/recall/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))
}

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("timestamp")
        .with_timezone(&Utc)
}

fn query(from: &str, to: &str) -> ScorecardQuery {
    ScorecardQuery {
        repo: PULP.to_owned(),
        workflow: "build.yml".to_owned(),
        gate_job: "macos".to_owned(),
        base_branch: "main".to_owned(),
        required_contexts: Vec::new(),
        selection_steps: vec![FAST.to_owned(), AFFECTED.to_owned()],
        from: at(from),
        to: at(to),
    }
}

/// A fake `gh` answering from the captures; any other request fails the
/// read it belongs to.
struct FakeGh {
    calls: Mutex<Vec<String>>,
    /// Requests answered with an error instead of the capture.
    refuse: Vec<String>,
    /// Requests ending in the first string, answered with the second.
    replace: Vec<(String, String)>,
}

impl FakeGh {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            refuse: Vec::new(),
            replace: Vec::new(),
        }
    }

    fn answer(&self, args: &[String]) -> Result<String, String> {
        let joined = args.join(" ");
        self.calls.lock().expect("calls").push(joined.clone());
        if self.refuse.iter().any(|refused| joined.contains(refused)) {
            return Err(format!("refused: {joined}"));
        }
        if let Some((_, answer)) = self.replace.iter().find(|(end, _)| joined.ends_with(end)) {
            return Ok(answer.clone());
        }
        let field = |prefix: &str| {
            args.iter()
                .find_map(|arg| arg.strip_prefix(prefix))
                .map(str::to_owned)
        };
        let path = args
            .iter()
            .find(|arg| arg.starts_with("repos/"))
            .cloned()
            .unwrap_or_default();
        let rest = path
            .strip_prefix(&format!("repos/{PULP}/"))
            .unwrap_or_default();
        if rest == "actions/workflows/build.yml/runs" {
            return match (field("event=").as_deref(), field("head_sha=")) {
                (Some("merge_group"), Some(sha)) => fixture(&format!("mg-runs-on-{sha}.json")),
                (None, Some(sha)) => fixture(&format!("runs-on-{sha}.json")),
                _ => Err(format!("unexpected listing: {joined}")),
            };
        }
        if let Some(id) = rest
            .strip_prefix("actions/runs/")
            .and_then(|tail| tail.strip_suffix("/jobs"))
        {
            return fixture(&format!("jobs-run-{id}.json"));
        }
        if let Some(id) = rest
            .strip_prefix("actions/jobs/")
            .and_then(|tail| tail.strip_suffix("/logs"))
        {
            return fixture(&format!("log-job-{id}.log"));
        }
        if let Some(number) = rest.strip_prefix("pulls/") {
            return fixture(&format!("pull-{number}.json"));
        }
        if let Some(sha) = rest.strip_prefix("git/commits/") {
            return fixture(&format!("commit-{sha}.json"));
        }
        Err(format!("unexpected call: {joined}"))
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }
}

/// The window's merge-group runs, their gates, and the recall reads.
fn observe(fake: &FakeGh, query: &ScorecardQuery) -> Vec<GroupFailure> {
    let reader = |args: &[String]| fake.answer(args);
    let cache = ReadCache::disabled();
    let listing = format!("mg-runs-{}.json", query.from.format("%Y%m%dT%H%M"));
    let pages: Vec<Value> =
        serde_json::from_str(&fixture(&listing).expect("listing")).expect("listing JSON");
    let listed = collect_counted(&pages, "workflow_runs", "merge-group runs").expect("runs");
    let runs = read_gates(&reader, &cache, query, &listed).expect("gates");
    gather(&reader, &cache, query, &listed, &runs)
}

fn members(failure: &GroupFailure) -> Vec<(Option<u64>, &'static str)> {
    let recall = compute(PULP, &[failure]);
    recall.cases[0]
        .members
        .iter()
        .map(|member| (member.pr, member.status))
        .collect()
}

#[test]
fn a_chain_of_failed_groups_names_every_entry_ahead() {
    let fake = FakeGh::new();
    let query = query("2026-10-12T05:40:00Z", "2026-10-12T05:50:00Z");
    let failures = observe(&fake, &query);
    let ids: Vec<u64> = failures.iter().map(|failure| failure.run_id).collect();
    assert_eq!(ids, vec![38_208_936_044, 38_209_188_003, 38_209_631_270]);
    for failure in &failures {
        let failed = failure.failed.as_ref().expect("failed tests");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].name, MIGRATION);
    }
    assert_eq!(
        members(&failures[0]),
        vec![(Some(10115), "not_selected"), (Some(10114), "not_selected")]
    );
    assert_eq!(
        members(&failures[2]),
        vec![
            (Some(10134), "not_selected"),
            (Some(10133), "not_selected"),
            (Some(10115), "not_selected"),
            (Some(10114), "not_selected"),
        ]
    );
    // #10114 merged at 05:52 as the base of #10115's branch, after these
    // runs were created, so it was ahead; its own base had merged before.
    let walked: Vec<String> = fake
        .calls()
        .into_iter()
        .filter(|call| call.contains("/pulls/"))
        .collect();
    assert!(walked.iter().any(|call| call.ends_with("/pulls/10114")));
}

#[test]
fn every_failure_is_a_miss_and_a_head_without_a_selection_step_selected_nothing() {
    let fake = FakeGh::new();
    let query = query("2026-10-12T05:40:00Z", "2026-10-12T05:50:00Z");
    let failures = observe(&fake, &query);
    let refs: Vec<&GroupFailure> = failures.iter().collect();
    let recall = compute(PULP, &refs);
    assert_eq!(
        (
            recall.failed_gate_runs,
            recall.runs_with_test_failures,
            recall.test_failures
        ),
        (3, 3, 3)
    );
    assert_eq!(
        (recall.selected, recall.missed, recall.undetermined),
        (0, 3, 0)
    );
    assert_eq!(recall.recall, Some(0.0));
    assert!(
        recall
            .cases
            .iter()
            .all(|case| case.failed_gates_with_test == 3)
    );
    let last = &recall.cases[2];
    assert_eq!(
        last.run_url,
        "https://github.com/Generous-Corp/pulp/actions/runs/38209631270"
    );
    let head = &last.members[0];
    assert_eq!(head.pr, Some(10134));
    assert_eq!(head.selection_ran, Some(false));
    assert_eq!(head.selection_size, Some(0));
    // The stand-in job's log is never read.
    let logs = fake
        .calls()
        .iter()
        .filter(|call| call.ends_with("/logs"))
        .count();
    assert_eq!(logs, 3 + 3, "three gate logs and three heads' logs");
    let read = &last.members[1];
    assert_eq!(read.pr, Some(10133));
    assert_eq!(read.selection_ran, Some(true));
}

#[test]
fn an_unreadable_head_makes_the_case_undetermined() {
    let mut fake = FakeGh::new();
    let query = query("2026-10-12T05:40:00Z", "2026-10-12T05:50:00Z");
    // Every listing of a head's runs fails: no head can be read.
    fake.refuse = vec!["/runs -f head_sha=".to_owned()];
    let failures = observe(&fake, &query);
    let refs: Vec<&GroupFailure> = failures.iter().collect();
    let recall = compute(PULP, &refs);
    assert_eq!(
        (recall.selected, recall.missed, recall.undetermined),
        (0, 0, 3)
    );
    assert_eq!(recall.recall, None);
    assert_eq!(recall.cases[0].members.len(), 2);
    assert!(recall.cases[0].members.iter().all(|member| {
        member.status == "unread"
            && member
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("refused"))
    }));
}

#[test]
fn an_unreadable_gate_log_is_counted_apart() {
    let mut fake = FakeGh::new();
    let query = query("2026-10-12T05:40:00Z", "2026-10-12T05:50:00Z");
    fake.refuse = vec!["actions/jobs/114678648909/logs".to_owned()];
    let failures = observe(&fake, &query);
    let refs: Vec<&GroupFailure> = failures.iter().collect();
    let recall = compute(PULP, &refs);
    assert_eq!(recall.runs_unread, 1);
    assert_eq!(recall.test_failures, 2);
}

/// A head's pull request, status and outcome in one case.
type Seen<'a> = (Option<u64>, &'a str, Option<&'a str>);

#[test]
fn a_test_a_head_ran_is_selected_whatever_it_did_there() {
    let fake = FakeGh::new();
    let query = query("2026-10-11T13:01:00Z", "2026-10-11T13:03:00Z");
    let failures = observe(&fake, &query);
    let refs: Vec<&GroupFailure> = failures.iter().collect();
    let recall = compute(PULP, &refs);
    assert_eq!(
        (recall.selected, recall.missed, recall.undetermined),
        (4, 0, 0)
    );
    assert_eq!(recall.recall, Some(1.0));
    let cases: Vec<(u64, &str, Vec<Seen<'_>>)> = recall
        .cases
        .iter()
        .map(|case| {
            (
                case.run_id,
                case.test.as_str(),
                case.members
                    .iter()
                    .map(|member| (member.pr, member.status, member.outcome.as_deref()))
                    .collect(),
            )
        })
        .collect();
    let ledger = "vellum-inheritance-ledger";
    let negative = "vellum-inheritance-ledger-negative-contract";
    let ran = (Some(10071), "ran", Some("Passed"));
    let other = (Some(10053), "not_selected", None);
    assert_eq!(
        cases,
        vec![
            (38_141_548_287, ledger, vec![ran]),
            (38_141_548_287, negative, vec![ran]),
            (38_141_621_537, ledger, vec![other, ran]),
            (38_141_621_537, negative, vec![other, ran]),
        ]
    );
}

#[test]
fn the_selection_is_the_union_of_its_steps_and_announced_counts_are_kept() {
    let jobs: Value = serde_json::from_str(
        &fixture("../../ctest-log/jobs-pr-run-38305299000.json").expect("jobs"),
    )
    .expect("JSON");
    let job = jobs["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .find(|job| job["id"].as_u64() == Some(114_997_948_499))
        .cloned()
        .expect("job");
    let text = fixture("../../ctest-log/log-pr-job-114997948499.log").expect("log");
    let steps = [FAST.to_owned(), AFFECTED.to_owned()];
    let selection = selection_from_log(&text, &job, &steps);
    assert_eq!(selection.ran.len(), 99 + 28);
    assert!(selection.selection_ran);
    assert!(selection.missing_sections.is_empty());
    assert_eq!(selection.declared_selected, Some(28));
    assert_eq!(selection.declared_budget_skipped, Some(0));
    assert_eq!(selection.ran_outside, 165);
    // A step the job ran whose section is absent is named, not read as
    // having run nothing.
    let mut renamed = job.clone();
    for step in renamed["steps"].as_array_mut().expect("steps") {
        if step["name"] == AFFECTED {
            step["started_at"] = Value::String("2026-10-12T22:00:00Z".to_owned());
        }
    }
    let selection = selection_from_log(&text, &renamed, &steps);
    assert_eq!(selection.missing_sections, vec![AFFECTED.to_owned()]);
}

#[test]
fn the_report_lists_each_miss_with_its_heads() {
    let fake = FakeGh::new();
    let query = query("2026-10-12T05:40:00Z", "2026-10-12T05:50:00Z");
    let observation = ScorecardObservation {
        query: query.clone(),
        required: Ok((Vec::new(), "--required-context".to_owned())),
        prs: Vec::new(),
        merge_group_runs: Vec::new(),
        queue_events: Vec::new(),
        selection: Some(observe(&fake, &query)),
        gaps: Vec::new(),
    };
    let report = super::super::compute(&observation);
    let text = super::super::render(&report);
    assert!(
        text.contains(
            "  selector recall: 0 (0 of 3 merge-group test failures ran on a pull-request head"
        ),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "    missed: `{MIGRATION}` (Failed, failed 3 gate(s) this week) in \
             https://github.com/Generous-Corp/pulp/actions/runs/38209631270: #10134 head \
             selection 0 test(s), no selection step ran; #10133 head selection"
        )),
        "{text}"
    );
    let json = serde_json::to_value(&report).expect("JSON");
    assert_eq!(json["weeks"][0]["selection_recall"]["missed"], 3);
    assert_eq!(json["selection_steps"][1], AFFECTED);
}

#[test]
fn an_entry_merged_before_the_run_under_another_commit_was_still_ahead() {
    // #10114 merged at 05:52 as the commit #10115's branch was built on. Had
    // it merged before the runs under a different commit (re-queued after
    // an ejection, say), its old queue head could not have been the base:
    // it was still an entry ahead.
    let mut fake = FakeGh::new();
    fake.replace = vec![(
        "/pulls/10114".to_owned(),
        r#"{"number":10114,"merged_at":"2026-10-12T05:00:00Z","merge_commit_sha":"0000000000000000000000000000000000000000"}"#
            .to_owned(),
    )];
    let query = query("2026-10-12T05:40:00Z", "2026-10-12T05:50:00Z");
    let failures = observe(&fake, &query);
    assert_eq!(
        members(&failures[0]),
        vec![(Some(10115), "not_selected"), (Some(10114), "not_selected")]
    );
    // Merged before the runs as exactly that commit, it was the base.
    fake.replace = vec![(
        "/pulls/10114".to_owned(),
        r#"{"number":10114,"merged_at":"2026-10-12T05:00:00Z","merge_commit_sha":"8d708a76561c2ed6f3631ade693fa832af86ec9c"}"#
            .to_owned(),
    )];
    let failures = observe(&fake, &query);
    assert_eq!(members(&failures[0]), vec![(Some(10115), "not_selected")]);
}

#[test]
fn a_queue_merge_whose_first_parent_is_not_the_base_has_no_head() {
    // #10115's queue merge with its parents swapped: the first is no longer
    // the commit its branch was built on, so its second is not taken as the
    // pull request's head.
    let mut fake = FakeGh::new();
    fake.replace = vec![(
        "/git/commits/c5d524e52338a628d567e906a49fdc8e92d228f3".to_owned(),
        r#"{"sha":"c5d524e52338a628d567e906a49fdc8e92d228f3","parents":[{"sha":"319a9140c538546e8e06d7f3c8dd66e1c9d2639e"},{"sha":"8d708a76561c2ed6f3631ade693fa832af86ec9c"}]}"#
            .to_owned(),
    )];
    let query = query("2026-10-12T05:40:00Z", "2026-10-12T05:50:00Z");
    let failures = observe(&fake, &query);
    let recall = compute(PULP, &[&failures[0]]);
    let own = &recall.cases[0].members[0];
    assert_eq!((own.pr, own.status), (Some(10115), "unread"));
    assert!(
        own.reason
            .as_deref()
            .is_some_and(|reason| reason.contains("are not the queue base")),
        "{own:?}"
    );
    assert_eq!(recall.cases[0].verdict, "undetermined");
}
