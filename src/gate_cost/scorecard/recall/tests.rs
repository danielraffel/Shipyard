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
//!   before, so the walk stops there. No head selected the test, but each
//!   ran it in its non-gating full suite (`Test (non-Windows)`): #10115's
//!   head failed it there, twice, before it was queued; #10114's and
//!   #10133's passed it. #10134's head needed no native build: its `macos`
//!   job was the stand-in that runs no test step.
//!
//! - window 2026-10-11 13:01..13:03: #10071's group, then #10053's built on
//!   it, both failing `vellum-inheritance-ledger` and its negative contract;
//!   #10071's head ran both in its affected-test step, and they passed
//!   there.
//!
//! Answers are `mg-runs-<from>.json` (a window's merge-group runs),
//! `mg-runs-on-<sha>.json` and `runs-on-<sha>.json` (runs on one commit),
//! `jobs-run-<id>.json`, `log-job-<id>.log` (reduced keeping both windows'
//! failing tests), `pull-<n>.json`, `commit-<sha>.json` and
//! `compare-<sha>.json` (`<sha>...main`, keeping `status` and the counts).
//! Some tests change one captured answer to show what a different one would
//! do; each says which.
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
    /// Requests containing this string are answered `HTTP 502` this many
    /// times before the capture.
    gateway_errors: Option<(String, Mutex<u32>)>,
}

impl FakeGh {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            refuse: Vec::new(),
            replace: Vec::new(),
            gateway_errors: None,
        }
    }

    fn answer(&self, args: &[String]) -> Result<String, String> {
        let joined = args.join(" ");
        self.calls.lock().expect("calls").push(joined.clone());
        if self.refuse.iter().any(|refused| joined.contains(refused)) {
            return Err(format!("refused: {joined}"));
        }
        if let Some((pattern, left)) = &self.gateway_errors
            && joined.contains(pattern.as_str())
        {
            let mut left = left.lock().expect("count");
            if *left > 0 {
                *left -= 1;
                return Err(format!("gh: Server Error (HTTP 502) {joined}"));
            }
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
        if let Some((sha, _)) = rest
            .strip_prefix("compare/")
            .and_then(|tail| tail.split_once("..."))
        {
            return fixture(&format!("compare-{sha}.json"));
        }
        Err(format!("unexpected call: {joined}"))
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }
}

/// The window's merge-group runs, their gates, and the recall reads.
fn observe(fake: &FakeGh, query: &ScorecardQuery) -> Vec<GroupFailure> {
    observe_with(fake, query, |_| {})
}

/// [`observe`], with `edit` applied to the gates read before recall runs.
fn observe_with(
    fake: &FakeGh,
    query: &ScorecardQuery,
    edit: impl Fn(&mut Vec<MergeGroupRun>),
) -> Vec<GroupFailure> {
    let reader = |args: &[String]| fake.answer(args);
    let cache = ReadCache::disabled();
    let listing = format!("mg-runs-{}.json", query.from.format("%Y%m%dT%H%M"));
    let pages: Vec<Value> =
        serde_json::from_str(&fixture(&listing).expect("listing")).expect("listing JSON");
    let listed = collect_counted(&pages, "workflow_runs", "merge-group runs").expect("runs");
    let mut runs = read_gates(&reader, &cache, query, &listed).expect("gates");
    edit(&mut runs);
    gather(&reader, &cache, query, &listed, &runs)
}

fn chain() -> ScorecardQuery {
    query("2026-10-12T05:40:00Z", "2026-10-12T05:50:00Z")
}

fn ledger() -> ScorecardQuery {
    query("2026-10-11T13:01:00Z", "2026-10-11T13:03:00Z")
}

fn recall_of(failures: &[GroupFailure]) -> SelectionRecall {
    let refs: Vec<&GroupFailure> = failures.iter().collect();
    compute(PULP, &refs)
}

fn log_reads(fake: &FakeGh) -> Vec<String> {
    fake.calls()
        .into_iter()
        .filter(|call| call.ends_with("/logs"))
        .collect()
}

const HEAD_10115: &str = "319a9140c538546e8e06d7f3c8dd66e1c9d2639e";
const HEAD_10071: &str = "d97045ef35a10b146c65caac9364a4eb23517fa0";

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
    let failures = observe(&fake, &chain());
    let ids: Vec<u64> = failures.iter().map(|failure| failure.run_id).collect();
    assert_eq!(ids, vec![38_208_936_044, 38_209_188_003, 38_209_631_270]);
    for failure in &failures {
        let failed = failure.failed.as_ref().expect("failed tests");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].name, MIGRATION);
    }
    assert_eq!(
        members(&failures[0]),
        vec![(Some(10115), "ran_outside"), (Some(10114), "ran_outside")]
    );
    assert_eq!(
        members(&failures[2]),
        vec![
            (Some(10134), "no_selection"),
            (Some(10133), "ran_outside"),
            (Some(10115), "ran_outside"),
            (Some(10114), "ran_outside"),
        ]
    );
    // #10114 merged at 05:52 as the base of #10115's branch, after these
    // runs were created, so it was ahead; #10100, its own base, had merged
    // before as that commit, so the walk stops without a compare.
    let calls = fake.calls();
    assert!(calls.iter().any(|call| call.ends_with("/pulls/10114")));
    assert!(calls.iter().any(|call| call.ends_with("/pulls/10100")));
    assert!(!calls.iter().any(|call| call.contains("/compare/")));
}

#[test]
fn a_test_a_head_failed_outside_its_selection_is_an_ignored_failure() {
    let fake = FakeGh::new();
    let recall = recall_of(&observe(&fake, &chain()));
    assert_eq!(
        (
            recall.failed_gate_runs,
            recall.runs_with_test_failures,
            recall.test_failures
        ),
        (3, 3, 3)
    );
    assert_eq!(
        (
            recall.selected,
            recall.ran_outside,
            recall.failed_on_head_ignored,
            recall.missed,
            recall.undetermined
        ),
        (0, 3, 3, 0, 0)
    );
    assert_eq!(recall.recall, Some(0.0));
    assert_eq!(recall.recall_anywhere, Some(1.0));
    assert!(recall.cases.iter().all(|case| case.failed_on_head));
    let own = &recall.cases[0].members[0];
    assert_eq!(
        (own.pr, own.status, own.outcome.as_deref()),
        (Some(10115), "ran_outside", Some("Failed"))
    );
    let mate = &recall.cases[0].members[1];
    assert_eq!(
        (mate.pr, mate.status, mate.outcome.as_deref()),
        (Some(10114), "ran_outside", Some("Passed"))
    );
    assert!(
        recall
            .cases
            .iter()
            .all(|case| case.failed_gates_with_test == 3)
    );
    assert_eq!(
        recall.cases[2].run_url,
        "https://github.com/Generous-Corp/pulp/actions/runs/38209631270"
    );
}

#[test]
fn a_stand_in_head_selected_nothing_and_makes_a_case_undetermined() {
    // Had #10115 not run the test anywhere, the third group would be left
    // with #10134's stand-in, which ran no selection: not a miss.
    let mut fake = FakeGh::new();
    fake.refuse = vec![format!("head_sha={HEAD_10115}")];
    let recall = recall_of(&observe(&fake, &chain()));
    let stand_in = &recall.cases[2].members[0];
    assert_eq!(
        (stand_in.pr, stand_in.status, stand_in.selection_ran),
        (Some(10134), "no_selection", Some(false))
    );
    assert_eq!(stand_in.selection_size, Some(0));
    // The stand-in job's log is never read: three gate logs, and the heads
    // of #10114 and #10133.
    assert_eq!(log_reads(&fake).len(), 3 + 2);
    // With only heads that selected and passed it, the group would be a
    // miss; the stand-in makes it undetermined.
    let mut fake = FakeGh::new();
    fake.refuse = vec![format!("head_sha={HEAD_10115}")];
    let only_stand_in = observe(&fake, &chain());
    let members: Vec<GroupMember> = only_stand_in[2]
        .members
        .as_ref()
        .expect("members")
        .iter()
        .filter(|member| member.pr == Some(10134))
        .cloned()
        .collect();
    let alone = GroupFailure {
        members: Ok(members),
        ..only_stand_in[2].clone()
    };
    let recall = recall_of(&[alone]);
    assert_eq!((recall.missed, recall.undetermined), (0, 1));
    assert_eq!(recall.undetermined_no_selection, 1);
}

#[test]
fn a_selected_head_that_never_ran_the_test_is_a_miss() {
    // The ledger window's second group without #10071: #10053's head
    // selected tests and none was the failing one.
    let fake = FakeGh::new();
    let failures = observe(&fake, &ledger());
    let second = &failures[1];
    let members: Vec<GroupMember> = second
        .members
        .as_ref()
        .expect("members")
        .iter()
        .filter(|member| member.pr == Some(10053))
        .cloned()
        .collect();
    let alone = GroupFailure {
        members: Ok(members),
        ..second.clone()
    };
    let recall = recall_of(&[alone]);
    assert_eq!((recall.selected, recall.missed), (0, 2));
    assert_eq!(recall.recall, Some(0.0));
    assert_eq!(recall.recall_anywhere, Some(0.0));
}

#[test]
fn an_unreadable_head_makes_the_case_undetermined() {
    let mut fake = FakeGh::new();
    // Every listing of a head's runs fails: no head can be read.
    fake.refuse = vec!["/runs -f head_sha=".to_owned()];
    let recall = recall_of(&observe(&fake, &chain()));
    assert_eq!(
        (recall.selected, recall.missed, recall.undetermined),
        (0, 0, 3)
    );
    assert_eq!(recall.recall, None);
    assert_eq!(recall.cases[0].members.len(), 2);
    // A refusal is not a gateway error: each head's listing is asked once.
    let asked = fake
        .calls()
        .iter()
        .filter(|call| call.contains(&format!("/runs -f head_sha={HEAD_10115}")))
        .count();
    assert_eq!(asked, 1);
    assert!(recall.cases[0].members.iter().all(|member| {
        member.status == "unread"
            && member
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("refused"))
    }));
}

#[test]
fn an_unreadable_group_makes_the_case_undetermined() {
    let mut fake = FakeGh::new();
    // The walk cannot list the run built on #10114's base.
    fake.refuse = vec!["-f event=merge_group -f head_sha=".to_owned()];
    let recall = recall_of(&observe(&fake, &chain()));
    assert_eq!(
        (
            recall.selected,
            recall.ran_outside,
            recall.missed,
            recall.undetermined
        ),
        (0, 0, 0, 3)
    );
    let case = &recall.cases[0];
    assert!(case.members.is_empty());
    assert!(
        case.reason
            .as_deref()
            .is_some_and(|r| r.contains("refused")),
        "{case:?}"
    );
}

#[test]
fn a_base_no_merge_group_was_built_for_must_be_on_the_branch() {
    let empty = r#"[{"total_count":0,"workflow_runs":[]}]"#.to_owned();
    let base = "9f6192e69cd766601b625ab671030819baf42e1d";
    // Had no merge group been built on #10114's base, GitHub's compare
    // (captured: main is 118 commits ahead of it) confirms it is on main.
    let mut fake = FakeGh::new();
    fake.replace = vec![(format!("head_sha={base} -f per_page=100"), empty.clone())];
    let failures = observe(&fake, &chain());
    assert_eq!(
        members(&failures[0]),
        vec![(Some(10115), "ran_outside"), (Some(10114), "ran_outside")]
    );
    assert!(
        fake.calls()
            .iter()
            .any(|call| call.contains(&format!("compare/{base}...main")))
    );
    // Off main, it is not a base: the group is unread, never a guess.
    let mut fake = FakeGh::new();
    fake.replace = vec![
        (format!("head_sha={base} -f per_page=100"), empty),
        (
            "per_page=1".to_owned(),
            r#"{"status":"diverged","ahead_by":3,"behind_by":2}"#.to_owned(),
        ),
    ];
    let failures = observe(&fake, &chain());
    let recall = recall_of(&failures);
    assert_eq!(recall.undetermined, 3);
    assert!(
        recall.cases[0]
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("has no merge-group run and is not on main")),
        "{:?}",
        recall.cases[0]
    );
}

#[test]
fn a_cancelled_gate_is_not_a_failure() {
    let fake = FakeGh::new();
    let failures = observe_with(&fake, &chain(), |runs| {
        for run in runs.iter_mut().filter(|run| run.run_id == 38_209_188_003) {
            if let Some(gate) = run.gate.as_mut() {
                gate.conclusion = Some("cancelled".to_owned());
            }
        }
    });
    let ids: Vec<u64> = failures.iter().map(|failure| failure.run_id).collect();
    assert_eq!(ids, vec![38_208_936_044, 38_209_631_270]);
}

#[test]
fn a_head_whose_checks_were_dispatched_is_read() {
    // #10115's head run, as an App-opened pull request's dispatched run.
    let mut fake = FakeGh::new();
    let dispatched = fixture(&format!("runs-on-{HEAD_10115}.json"))
        .expect("runs")
        .replace(
            r#""event":"pull_request""#,
            r#""event":"workflow_dispatch""#,
        );
    assert!(dispatched.contains("workflow_dispatch"));
    fake.replace = vec![(format!("head_sha={HEAD_10115} -f per_page=100"), dispatched)];
    let recall = recall_of(&observe(&fake, &chain()));
    let own = &recall.cases[0].members[0];
    assert_eq!(
        (own.pr, own.status, own.runs.clone()),
        (Some(10115), "ran_outside", vec![38_205_573_503])
    );
}

#[test]
fn a_skipped_head_job_is_not_read() {
    // #10115's head `macos` job, as if it had been skipped.
    let mut fake = FakeGh::new();
    let mut jobs: Value =
        serde_json::from_str(&fixture("jobs-run-38205573503.json").expect("jobs")).expect("JSON");
    for page in jobs.as_array_mut().expect("pages") {
        for job in page["jobs"].as_array_mut().expect("jobs") {
            if job["name"] == "macos" {
                job["conclusion"] = Value::String("skipped".to_owned());
            }
        }
    }
    fake.replace = vec![(
        "actions/runs/38205573503/jobs -f filter=all -f per_page=100".to_owned(),
        jobs.to_string(),
    )];
    let recall = recall_of(&observe(&fake, &chain()));
    let own = &recall.cases[0].members[0];
    assert_eq!((own.pr, own.status), (Some(10115), "no_selection"));
    assert!(
        !log_reads(&fake)
            .iter()
            .any(|call| call.contains("114668299889"))
    );
}

#[test]
fn a_head_run_started_after_the_group_does_not_count() {
    // #10071's head run, as if started after its groups failed (a run to
    // diagnose them): it must not make either case selected.
    let mut fake = FakeGh::new();
    let late = fixture(&format!("runs-on-{HEAD_10071}.json"))
        .expect("runs")
        .replace(
            r#""created_at":"2026-10-11T06:05:18Z""#,
            r#""created_at":"2026-10-11T13:30:00Z""#,
        );
    assert!(late.contains("13:30:00Z"));
    fake.replace = vec![(format!("head_sha={HEAD_10071} -f per_page=100"), late)];
    let recall = recall_of(&observe(&fake, &ledger()));
    assert_eq!((recall.selected, recall.undetermined), (0, 4));
    let late_head = &recall.cases[0].members[0];
    assert_eq!((late_head.pr, late_head.status), (Some(10071), "unread"));
    assert!(late_head.runs.is_empty());
    assert!(
        late_head
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("before the merge-group run"))
    );
}

#[test]
fn an_unreadable_gate_log_is_counted_apart() {
    let mut fake = FakeGh::new();
    fake.refuse = vec!["actions/jobs/114678648909/logs".to_owned()];
    let recall = recall_of(&observe(&fake, &chain()));
    assert_eq!(recall.runs_unread, 1);
    assert_eq!(recall.test_failures, 2);
}

/// A head's pull request, status and outcome in one case.
type Seen<'a> = (Option<u64>, &'a str, Option<&'a str>);

#[test]
fn a_test_a_head_ran_is_selected_whatever_it_did_there() {
    let fake = FakeGh::new();
    let recall = recall_of(&observe(&fake, &ledger()));
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
    let asked = BTreeSet::from([
        "pulp.vellum-closure-external-input-negative".to_owned(),
        "timeline agent decodes local file URI media locators".to_owned(),
        "not a test".to_owned(),
    ]);
    let selection = selection_from_log(&text, &job, &steps, &asked);
    assert_eq!(selection.ran.len(), 99 + 28);
    assert!(selection.selection_ran);
    assert!(selection.missing_sections.is_empty());
    assert_eq!(selection.declared_selected, Some(28));
    assert_eq!(selection.declared_budget_skipped, Some(0));
    assert_eq!(selection.ran_outside, 165);
    // Only the tests asked about are kept from the other steps, and a test
    // that failed before passing on its retry keeps the failure.
    assert_eq!(
        selection.outside,
        BTreeMap::from([
            (
                "pulp.vellum-closure-external-input-negative".to_owned(),
                "Passed".to_owned()
            ),
            (
                "timeline agent decodes local file URI media locators".to_owned(),
                "Failed".to_owned()
            ),
        ])
    );
    // A step the job ran whose section is absent is named, not read as
    // having run nothing.
    let mut renamed = job.clone();
    for step in renamed["steps"].as_array_mut().expect("steps") {
        if step["name"] == AFFECTED {
            step["started_at"] = Value::String("2026-10-12T22:00:00Z".to_owned());
        }
    }
    let selection = selection_from_log(&text, &renamed, &steps, &asked);
    assert_eq!(selection.missing_sections, vec![AFFECTED.to_owned()]);
}

#[test]
fn the_report_lists_each_case_with_its_heads() {
    let fake = FakeGh::new();
    let query = chain();
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
            "  selector recall: 0 (0 of 3 merge-group test failures ran in a pull-request head's \
             selection; 3 ran outside it, 3 of them failing on the head unheeded; 0 missed); \
             recall anywhere 1; 0 undetermined"
        ),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "    ran_outside (failed on the head): `{MIGRATION}` (Failed, failed 3 gate(s) this \
             week) in https://github.com/Generous-Corp/pulp/actions/runs/38209631270: #10134 no \
             selection step ran; #10133 head selection"
        )),
        "{text}"
    );
    assert!(
        text.contains("ran it outside the selection: Failed"),
        "{text}"
    );
    let json = serde_json::to_value(&report).expect("JSON");
    let week = &json["weeks"][0]["selection_recall"];
    assert_eq!(week["failed_on_head_ignored"], 3);
    assert_eq!(week["recall_anywhere"], 1.0);
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
    let failures = observe(&fake, &chain());
    assert_eq!(
        members(&failures[0]),
        vec![(Some(10115), "ran_outside"), (Some(10114), "ran_outside")]
    );
    // Merged before the runs as exactly that commit, it was the base.
    fake.replace = vec![(
        "/pulls/10114".to_owned(),
        r#"{"number":10114,"merged_at":"2026-10-12T05:00:00Z","merge_commit_sha":"8d708a76561c2ed6f3631ade693fa832af86ec9c"}"#
            .to_owned(),
    )];
    let failures = observe(&fake, &chain());
    assert_eq!(members(&failures[0]), vec![(Some(10115), "ran_outside")]);
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
    let failures = observe(&fake, &chain());
    let recall = compute(PULP, &[&failures[0]]);
    let own = &recall.cases[0].members[0];
    assert_eq!((own.pr, own.status), (Some(10115), "unread"));
    assert!(
        own.reason
            .as_deref()
            .is_some_and(|reason| reason.contains("are not the queue base")),
        "{own:?}"
    );
    // #10114 passed the test outside its selection, so the case still reads
    // as run outside, not failed on a head.
    assert_eq!(recall.cases[0].verdict, "ran_outside");
    assert!(!recall.cases[0].failed_on_head);
}

#[test]
fn a_gateway_error_is_read_again_and_a_lasting_one_leaves_the_read_unread() {
    // Two `HTTP 502` answers on #10115's head log: the third read lands.
    let mut fake = FakeGh::new();
    fake.gateway_errors = Some(("jobs/114668299889/logs".to_owned(), Mutex::new(2)));
    let recall = recall_of(&observe(&fake, &chain()));
    assert_eq!(recall.cases[0].members[0].status, "ran_outside");
    let reads = log_reads(&fake)
        .iter()
        .filter(|call| call.contains("114668299889"))
        .count();
    assert_eq!(reads, 3);
    // Three in a row leave that head unread, naming the status.
    let mut fake = FakeGh::new();
    fake.gateway_errors = Some(("jobs/114668299889/logs".to_owned(), Mutex::new(3)));
    let recall = recall_of(&observe(&fake, &chain()));
    let own = &recall.cases[0].members[0];
    assert_eq!(own.status, "unread");
    assert!(
        own.reason
            .as_deref()
            .is_some_and(|r| r.contains("HTTP 502")),
        "{own:?}"
    );
    // A gateway error on a merge-group listing in the walk is read again too.
    let mut fake = FakeGh::new();
    fake.gateway_errors = Some(("event=merge_group -f head_sha=".to_owned(), Mutex::new(1)));
    let recall = recall_of(&observe(&fake, &chain()));
    assert_eq!(recall.undetermined, 0);
}
