//! The scorecard over answers captured from GitHub for `Generous-Corp/pulp`
//! (`tests/fixtures/gate-cost-scorecard/`, read on 2026-10-12 UTC with the same
//! queries [`gather`] sends):
//!
//! - window `a`, 2026-10-05 15:00..15:20: four merged pull requests and
//!   three merge-group runs, two of whose `macos` gate failed;
//! - window `b`, 2026-10-11 15:50..15:55: #10042, eleven commits, two of
//!   which carry more than 100 checks (103 and 101);
//! - window `c`, 2026-10-09 04:50..05:00: no merged pull request and two
//!   merge-group runs, one concluding `failure` with its gate green because
//!   an advisory Linux job failed;
//! - #9529, 27 commits, read in two pages of commits.
//!
//! Search answers are `<window>-search.json`, a pull request's commits
//! `pr-<number>-commits.json`, and a commit's checks past the first hundred
//! `contexts-<oid>.json`.
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use super::*;

const PULP: &str = "Generous-Corp/pulp";
const NOT_PROTECTED: &str = "gh: Branch not protected (HTTP 404)";

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/gate-cost-scorecard/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("timestamp")
        .with_timezone(&Utc)
}

fn query(window: &str) -> ScorecardQuery {
    let (from, to) = match window {
        "a" => ("2026-10-05T15:00:00Z", "2026-10-05T15:20:00Z"),
        "b" => ("2026-10-11T15:50:00Z", "2026-10-11T15:55:00Z"),
        _ => ("2026-10-09T04:50:00Z", "2026-10-09T05:00:00Z"),
    };
    ScorecardQuery {
        repo: PULP.to_owned(),
        workflow: "build.yml".to_owned(),
        gate_job: "macos".to_owned(),
        base_branch: "main".to_owned(),
        required_contexts: Vec::new(),
        from: at(from),
        to: at(to),
    }
}

/// A fake `gh` answering from the captures of `window`. The unfiltered run
/// walk is not captured (a page is 100 full runs), so it answers an error,
/// which the report must carry as a gap. `protection` overrides the branch
/// protection answer.
struct FakeGh {
    window: &'static str,
    protection: Result<String, String>,
    calls: Mutex<Vec<String>>,
}

impl FakeGh {
    fn new(window: &'static str) -> Self {
        Self {
            window,
            protection: Ok(fixture("protection-pulp.json")),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn answer(&self, args: &[String]) -> Result<String, String> {
        let joined = args.join(" ");
        self.calls.lock().expect("calls").push(joined.clone());
        let window = self.window;
        if args.iter().any(|arg| arg == "graphql") {
            if let Some(oid) = args.iter().find_map(|arg| arg.strip_prefix("oid=")) {
                return Ok(fixture(&format!("contexts-{oid}.json")));
            }
            if let Some(number) = args.iter().find_map(|arg| arg.strip_prefix("number=")) {
                return Ok(fixture(&format!("pr-{number}-commits.json")));
            }
            let q = args
                .iter()
                .find_map(|arg| arg.strip_prefix("q="))
                .expect("search q");
            assert!(
                q.contains(&format!("merged:{}..", rfc3339(query(window).from))),
                "{q}"
            );
            return Ok(fixture(&format!("{window}-search.json")));
        }
        if joined.contains("/rules/branches/main") {
            return Ok(fixture("rules-pulp.json"));
        }
        if joined.contains("/protection/required_status_checks") {
            return self.protection.clone();
        }
        if joined.contains("/workflows/build.yml/runs") {
            if joined.contains("created=") {
                return Ok(fixture(&format!("{window}-mg-runs.json")));
            }
            return Err("the event walk was not captured".to_owned());
        }
        if let Some(id) = args.iter().find_map(|arg| {
            arg.strip_prefix(&format!("repos/{PULP}/actions/runs/"))
                .and_then(|rest| rest.strip_suffix("/jobs"))
        }) {
            return Ok(fixture(&format!("{window}-jobs-{id}.json")));
        }
        Err(format!("unexpected call: {joined}"))
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }
}

fn observe(fake: &FakeGh, query: &ScorecardQuery) -> Result<ScorecardObservation, String> {
    let reader = |args: &[String]| fake.answer(args);
    gather(&reader, query, &ReadCache::disabled())
}

fn only_week(report: &ScorecardReport) -> &WeekScore {
    assert_eq!(report.weeks.len(), 1, "{report:#?}");
    &report.weeks[0]
}

#[test]
fn a_window_scores_open_to_merge_ejections_and_repeat_failures() {
    let fake = FakeGh::new("a");
    let report = compute(&observe(&fake, &query("a")).expect("gather"));
    assert_eq!(report.schema, SCHEMA);
    assert_eq!(report.required_source, "branch protection");
    assert_eq!(report.required_contexts.len(), 6);
    let week = only_week(&report);
    assert_eq!(week.week, "2026-W41");
    assert_eq!(week.from, "2026-10-05T15:00:00Z");
    assert_eq!(week.to, "2026-10-05T15:20:00Z");
    assert_eq!(week.merged_prs, 4);
    // 1.48, 1.94, 4.43 and 5.24 hours.
    assert_eq!(week.open_to_merge.median_hours, Some(3.18));
    assert_eq!(week.open_to_merge.p90_hours, Some(4.99));
    let queue = &week.merge_queue;
    assert_eq!((queue.runs, queue.gated_runs, queue.ejections), (3, 3, 2));
    assert_eq!(queue.by_cause.get("gate_failed"), Some(&2));
    assert_eq!(queue.rate, Some(0.67));
    assert_eq!(queue.gate_failed_rate, Some(0.67));
    assert_eq!(queue.run_failed_gate_passed, 0);
    let red = week.red_after_push.as_ref().expect("required contexts");
    assert_eq!((red.pushes, red.red, red.cancelled_only), (4, 0, 0));
    let repeats: Vec<(&str, usize, bool)> = week
        .repeat_failures
        .iter()
        .map(|repeat| (repeat.name.as_str(), repeat.prs.len(), repeat.required))
        .collect();
    assert_eq!(
        repeats,
        vec![
            ("Diff coverage advisory", 4, false),
            ("Request Codex review on an App-authored PR", 3, false),
            ("AddressSanitizer (macOS ARM64)", 2, false),
        ]
    );
    // `Analyze c-cpp` was cancelled on all four: cancelled is not red.
    assert!(
        !week
            .repeat_failures
            .iter()
            .any(|repeat| repeat.name == "Analyze c-cpp")
    );
    assert!(
        report
            .gaps
            .iter()
            .any(|gap| gap.signal == "run_listing" && gap.reason.contains("not captured")),
        "{:?}",
        report.gaps
    );
}

#[test]
fn a_run_that_failed_with_its_gate_green_is_not_an_ejection() {
    let fake = FakeGh::new("c");
    let observation = observe(&fake, &query("c")).expect("gather");
    let failed = observation
        .merge_group_runs
        .iter()
        .find(|run| run.run_id == 37_886_097_258)
        .expect("run");
    assert_eq!(failed.conclusion.as_deref(), Some("failure"));
    assert_eq!(
        failed
            .gate
            .as_ref()
            .and_then(|gate| gate.conclusion.as_deref()),
        Some("success")
    );
    let report = compute(&observation);
    let week = only_week(&report);
    assert_eq!(week.merged_prs, 0);
    assert_eq!(week.open_to_merge.median_hours, None);
    let queue = &week.merge_queue;
    assert_eq!((queue.runs, queue.gated_runs, queue.ejections), (2, 2, 0));
    assert_eq!(queue.rate, Some(0.0));
    assert_eq!(queue.run_failed_gate_passed, 1);
}

#[test]
fn checks_past_the_first_hundred_are_read_and_the_latest_run_decides() {
    let fake = FakeGh::new("b");
    let observation = observe(&fake, &query("b")).expect("gather");
    let calls = fake.calls();
    let follow_ups: Vec<&String> = calls.iter().filter(|call| call.contains("oid=")).collect();
    assert_eq!(follow_ups.len(), 2, "{calls:#?}");
    let pr = &observation.prs[0];
    assert_eq!(pr.number, 10042);
    assert_eq!(pr.commits.len(), 11);
    let head = pr
        .commits
        .iter()
        .find(|commit| commit.oid == "96942c39701d6a360bfac1634f788016551826ac")
        .expect("commit with 103 checks");
    // Only on the second page of this commit's checks.
    let trusted = head
        .checks
        .iter()
        .find(|check| check.name == "Vellum trusted freeze")
        .expect("a required context past the first hundred");
    assert_eq!(trusted.outcome.as_deref(), Some("SUCCESS"));
    let report = compute(&observation);
    let red = only_week(&report)
        .red_after_push
        .as_ref()
        .expect("required contexts");
    // Of eleven commits seven carry a required context: four red, two only
    // cancelled by the next push.
    assert_eq!((red.pushes, red.red, red.cancelled_only), (7, 4, 2));
    assert_eq!(red.rate, Some(0.57));
}

#[test]
fn an_unprotected_base_has_no_red_after_push_rate() {
    let mut fake = FakeGh::new("c");
    fake.protection = Err(NOT_PROTECTED.to_owned());
    let report = compute(&observe(&fake, &query("c")).expect("gather"));
    assert_eq!(report.required_source, "the base branch is not protected");
    assert!(report.required_contexts.is_empty());
    assert_eq!(only_week(&report).red_after_push, None);

    // Any other failure is reported, not read as "no required checks".
    let mut fake = FakeGh::new("c");
    fake.protection = Err("gh: Not Found (HTTP 404)".to_owned());
    let report = compute(&observe(&fake, &query("c")).expect("gather"));
    assert!(
        report.required_source.starts_with("unread:"),
        "{}",
        report.required_source
    );

    // A flag names the required contexts of a repository without protection.
    let mut fake = FakeGh::new("b");
    fake.protection = Err(NOT_PROTECTED.to_owned());
    let mut flagged = query("b");
    flagged.required_contexts = vec!["macos".to_owned(), "macOS".to_owned()];
    let report = compute(&observe(&fake, &flagged).expect("gather"));
    assert_eq!(report.required_source, "--required-context");
    // A misspelled context is named, not silently counted as never red.
    let unseen: Vec<&str> = report
        .gaps
        .iter()
        .filter(|gap| gap.signal == "required_contexts")
        .map(|gap| gap.reason.as_str())
        .collect();
    assert_eq!(unseen.len(), 1, "{unseen:?}");
    assert!(unseen[0].contains("`macOS`"), "{unseen:?}");
    let red = only_week(&report).red_after_push.clone().expect("flagged");
    assert!(red.pushes > 0 && red.red > 0, "{red:?}");
    assert!(!fake.calls().iter().any(|call| call.contains("protection")));
}

#[test]
fn a_ruleset_that_requires_checks_is_refused_rather_than_ignored() {
    let fake = FakeGh::new("c");
    // The captured rules answer, with a required-checks rule added.
    let mut rules: Value = serde_json::from_str(&fixture("rules-pulp.json")).expect("rules");
    rules
        .as_array_mut()
        .expect("rules array")
        .push(serde_json::json!({"type": "required_status_checks", "parameters": {}}));
    let rules = rules.to_string();
    let reader = |args: &[String]| {
        if args.join(" ").contains("/rules/branches/") {
            Ok(rules.clone())
        } else {
            fake.answer(args)
        }
    };
    let observation = gather(&reader, &query("c"), &ReadCache::disabled()).expect("gather");
    let error = observation.required.expect_err("refused");
    assert!(error.contains("--required-context"), "{error}");
}

#[test]
fn a_short_search_read_is_refused() {
    let fake = FakeGh::new("a");
    let mut pages: Value = serde_json::from_str(&fixture("a-search.json")).expect("search");
    pages[0]["data"]["search"]["nodes"]
        .as_array_mut()
        .expect("nodes")
        .pop();
    let pages = pages.to_string();
    let reader = |args: &[String]| {
        if args.iter().any(|arg| arg.starts_with("q=")) {
            Ok(pages.clone())
        } else {
            fake.answer(args)
        }
    };
    let error = gather(&reader, &query("a"), &ReadCache::disabled()).expect_err("partial");
    assert!(error.contains("collected 3 of issueCount 4"), "{error}");
}

#[test]
fn an_answer_missing_a_collection_is_an_error_not_an_empty_list() {
    // The captured search, with one commit's `statusCheckRollup` key removed
    // (as opposed to `null`, which is a commit without checks), and then
    // with a search page's `nodes` removed.
    let fake = FakeGh::new("a");
    let mut no_rollup: Value =
        serde_json::from_str(&fixture("pr-9637-commits.json")).expect("commits");
    no_rollup[0]["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]
        .as_object_mut()
        .expect("commit")
        .remove("statusCheckRollup");
    let mut no_nodes: Value = serde_json::from_str(&fixture("a-search.json")).expect("search");
    no_nodes[0]["data"]["search"]
        .as_object_mut()
        .expect("search")
        .remove("nodes");
    for (answer, arg, expected) in [
        (no_rollup, "number=9637", "no statusCheckRollup"),
        (no_nodes, "q=", "merged pull requests: no nodes"),
    ] {
        let answer = answer.to_string();
        let reader = |args: &[String]| {
            if args.iter().any(|given| given.starts_with(arg)) {
                Ok(answer.clone())
            } else {
                fake.answer(args)
            }
        };
        let error = gather(&reader, &query("a"), &ReadCache::disabled()).expect_err("unread");
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn a_gateway_error_is_read_again_and_a_lasting_one_is_reported() {
    // The tail GitHub gave a live read of 2026-09-28 (`gh` prints the pages it
    // read, then the status).
    const GATEWAY: &str = "</html>\n]gh: HTTP 504";
    for (failures, expect_ok) in [(1, true), (2, true), (3, false)] {
        let fake = FakeGh::new("a");
        let left = Mutex::new(failures);
        let reader = |args: &[String]| {
            if args.iter().any(|arg| arg.starts_with("q=")) {
                let mut left = left.lock().expect("left");
                if *left > 0 {
                    *left -= 1;
                    return Err(format!("{}{GATEWAY}", "x".repeat(2000)));
                }
            }
            fake.answer(args)
        };
        let result = gather(&reader, &query("a"), &ReadCache::disabled());
        if expect_ok {
            assert_eq!(result.expect("retried").prs.len(), 4);
        } else {
            let error = result.expect_err("lasting");
            assert!(error.contains("after 3 attempt(s)"), "{error}");
            assert!(error.ends_with("gh: HTTP 504"), "{error}");
            assert!(error.len() < 600, "{}", error.len());
        }
    }
    // Anything else is not retried.
    let fake = FakeGh::new("a");
    let calls = Mutex::new(0);
    let reader = |args: &[String]| {
        if args.iter().any(|arg| arg.starts_with("q=")) {
            *calls.lock().expect("calls") += 1;
            return Err("gh: HTTP 401: Bad credentials".to_owned());
        }
        fake.answer(args)
    };
    let error = gather(&reader, &query("a"), &ReadCache::disabled()).expect_err("401");
    assert!(error.contains("after 1 attempt(s)"), "{error}");
    assert_eq!(*calls.lock().expect("calls"), 1);
}

fn pr(number: u64, created: &str, merged: &str, checks: &[(&str, &str)]) -> MergedPr {
    MergedPr {
        number,
        created_at: at(created),
        merged_at: at(merged),
        commits_total: 1,
        commits: vec![PrCommit {
            oid: format!("{number:040}"),
            checks: checks
                .iter()
                .map(|(name, outcome)| CheckOutcome {
                    name: (*name).to_owned(),
                    outcome: Some((*outcome).to_owned()),
                })
                .collect(),
        }],
    }
}

#[test]
fn weeks_are_iso_weeks_clipped_to_the_window() {
    let mut query = query("a");
    query.from = at("2026-10-03T12:00:00Z"); // Saturday of 2026-W40
    query.to = at("2026-10-14T00:00:00Z"); // Wednesday of 2026-W42
    let observation = ScorecardObservation {
        query,
        required: Ok((vec!["macos".to_owned()], "--required-context".to_owned())),
        prs: vec![
            pr(
                1,
                "2026-10-04T10:00:00Z",
                "2026-10-04T23:59:59Z",
                &[("macos", "FAILURE")],
            ),
            pr(
                2,
                "2026-10-05T00:00:00Z",
                "2026-10-05T02:00:00Z",
                &[("lint", "FAILURE")],
            ),
            pr(
                3,
                "2026-10-06T00:00:00Z",
                "2026-10-06T04:00:00Z",
                &[("lint", "TIMED_OUT")],
            ),
            pr(
                4,
                "2026-10-12T00:00:00Z",
                "2026-10-12T01:00:00Z",
                &[("macos", "SUCCESS")],
            ),
        ],
        merge_group_runs: Vec::new(),
        gaps: Vec::new(),
    };
    let report = compute(&observation);
    let weeks: Vec<(&str, &str, &str, usize)> = report
        .weeks
        .iter()
        .map(|week| {
            (
                week.week.as_str(),
                week.from.as_str(),
                week.to.as_str(),
                week.merged_prs,
            )
        })
        .collect();
    assert_eq!(
        weeks,
        vec![
            (
                "2026-W40",
                "2026-10-03T12:00:00Z",
                "2026-10-05T00:00:00Z",
                1
            ),
            (
                "2026-W41",
                "2026-10-05T00:00:00Z",
                "2026-10-12T00:00:00Z",
                2
            ),
            (
                "2026-W42",
                "2026-10-12T00:00:00Z",
                "2026-10-14T00:00:00Z",
                1
            ),
        ]
    );
    let w41 = &report.weeks[1];
    assert_eq!(w41.open_to_merge.median_hours, Some(3.0));
    assert_eq!(
        w41.repeat_failures,
        vec![RepeatFailure {
            name: "lint".to_owned(),
            required: false,
            prs: vec![2, 3],
        }]
    );
    // `lint` is not required, so neither push is red.
    let red = w41.red_after_push.as_ref().expect("required");
    assert_eq!((red.pushes, red.red), (0, 0));
    let w40 = report.weeks[0].red_after_push.as_ref().expect("required");
    assert_eq!((w40.pushes, w40.red), (1, 1));
}

#[test]
fn a_rerun_supersedes_the_run_it_repeats_in_either_order() {
    let failed = serde_json::json!({"__typename": "CheckRun", "name": "macos", "conclusion": "FAILURE", "startedAt": "2026-10-05T10:00:00Z", "completedAt": "2026-10-05T10:30:00Z"});
    let passed = serde_json::json!({"__typename": "CheckRun", "name": "macos", "conclusion": "SUCCESS", "startedAt": "2026-10-05T11:00:00Z", "completedAt": "2026-10-05T11:30:00Z"});
    for contexts in [
        vec![failed.clone(), passed.clone()],
        vec![passed.clone(), failed.clone()],
    ] {
        assert_eq!(
            latest_by_name(&contexts),
            vec![CheckOutcome {
                name: "macos".to_owned(),
                outcome: Some("SUCCESS".to_owned()),
            }]
        );
    }
}

#[test]
fn every_page_of_a_pull_request_s_commits_is_read() {
    let fake = FakeGh::new("a");
    let reader = |args: &[String]| fake.answer(args);
    let (commits, total) = read_commits(&reader, "Generous-Corp", "pulp", 9529).expect("commits");
    assert_eq!((commits.len(), total), (27, 27));

    // The second page answered short is refused, not counted as 25 commits.
    let mut pages: Value = serde_json::from_str(&fixture("pr-9529-commits.json")).expect("commits");
    pages[1]["data"]["repository"]["pullRequest"]["commits"]["nodes"]
        .as_array_mut()
        .expect("nodes")
        .pop();
    let pages = pages.to_string();
    let reader = |_: &[String]| Ok(pages.clone());
    let error = read_commits(&reader, "Generous-Corp", "pulp", 9529).expect_err("short");
    assert!(
        error.contains("collected 26 of totalCount Some(27)"),
        "{error}"
    );
}

#[test]
fn a_short_read_of_a_commit_s_checks_is_refused() {
    const OID: &str = "96942c39701d6a360bfac1634f788016551826ac";
    let fake = FakeGh::new("b");
    let reader = |args: &[String]| fake.answer(args);
    assert_eq!(
        read_contexts(&reader, "Generous-Corp", "pulp", OID)
            .expect("checks")
            .len(),
        103
    );
    let mut pages: Value =
        serde_json::from_str(&fixture(&format!("contexts-{OID}.json"))).expect("checks");
    pages[1]["data"]["repository"]["object"]["statusCheckRollup"]["contexts"]["nodes"]
        .as_array_mut()
        .expect("nodes")
        .pop();
    let pages = pages.to_string();
    let reader = |_: &[String]| Ok(pages.clone());
    let error = read_contexts(&reader, "Generous-Corp", "pulp", OID).expect_err("short");
    assert!(
        error.contains("collected 102 of totalCount Some(103)"),
        "{error}"
    );
}

#[test]
fn github_s_commit_cap_is_counted_not_refused() {
    // #9755 has 300 commits; GitHub listed 250 of `totalCount` 300.
    assert_eq!(commits_read(9755, Some(300), 250), Ok(300));
    // Short of the cap, or short of a total under it, is a partial read.
    for (total, collected) in [(Some(300), 249), (Some(240), 239), (None, 0)] {
        assert!(
            commits_read(9755, total, collected).is_err(),
            "{total:?} {collected}"
        );
    }
    let mut query = query("a");
    query.from = at("2026-10-06T00:00:00Z");
    query.to = at("2026-10-07T00:00:00Z");
    let mut capped = pr(9755, "2026-10-05T00:00:00Z", "2026-10-06T10:00:00Z", &[]);
    capped.commits_total = 300;
    let report = compute(&ScorecardObservation {
        query,
        required: Ok((Vec::new(), "the base branch is not protected".to_owned())),
        prs: vec![capped],
        merge_group_runs: Vec::new(),
        gaps: Vec::new(),
    });
    assert_eq!(only_week(&report).capped_prs, vec![9755]);
    assert!(render(&report).contains("first 250 commits counted"));
}
