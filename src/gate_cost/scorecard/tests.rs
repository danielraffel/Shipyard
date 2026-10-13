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
        "d" => ("2026-10-06T14:05:00Z", "2026-10-06T14:15:00Z"),
        "e" => ("2026-10-06T09:30:00Z", "2026-10-06T09:40:00Z"),
        _ => ("2026-10-09T04:50:00Z", "2026-10-09T05:00:00Z"),
    };
    ScorecardQuery {
        repo: PULP.to_owned(),
        workflow: "build.yml".to_owned(),
        gate_job: "macos".to_owned(),
        base_branch: "main".to_owned(),
        required_contexts: Vec::new(),
        selection_steps: Vec::new(),
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
                if joined.contains("reviews(last:100)") {
                    return Ok(fixture(&format!("pr-{number}-verdicts.json")));
                }
                if joined.contains("timelineItems") {
                    return Ok(fixture(&format!("timeline-{number}.json")));
                }
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
    // Two merge-group gates failed, yet no pull request was ejected in the
    // window: #9606's group failed and it stayed queued until it merged.
    let gate = &week.merge_group_gate;
    assert_eq!((gate.runs, gate.gated_runs, gate.failures), (3, 3, 2));
    assert_eq!(gate.by_cause.get("gate_failed"), Some(&2));
    assert_eq!(gate.failure_rate, Some(0.67));
    assert_eq!(gate.gate_failed_rate, Some(0.67));
    assert_eq!(gate.run_failed_gate_passed, 0);
    let queue = &week.merge_queue;
    assert_eq!(
        (queue.entries, queue.ejections, queue.ejection_rate),
        (0, 0, None)
    );
    assert_eq!(removals(queue), vec![("merged", "github-merge-queue", 4)]);
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
    // None of the four carried a verdict on its merge head.
    assert_eq!(
        week.merge_verdicts,
        MergeVerdicts {
            none: 4,
            ..MergeVerdicts::default()
        }
    );
    assert!(
        render(&report).contains("merge-head verdict: 0 approved, 0 HOLD, 4 none, 0 unread"),
        "{}",
        render(&report)
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
    let gate = &week.merge_group_gate;
    assert_eq!((gate.runs, gate.gated_runs, gate.failures), (2, 2, 0));
    assert_eq!(gate.failure_rate, Some(0.0));
    assert_eq!(gate.run_failed_gate_passed, 1);
    let queue = &week.merge_queue;
    assert_eq!((queue.entries, queue.ejections), (2, 0));
    assert_eq!(queue.ejection_rate, Some(0.0));
}

fn removals(queue: &MergeQueue) -> Vec<(&str, &str, usize)> {
    queue
        .removals
        .iter()
        .flat_map(|(reason, actors)| {
            actors
                .iter()
                .map(move |(actor, count)| (reason.as_str(), actor.as_str(), *count))
        })
        .collect()
}

#[test]
fn an_ejection_while_the_gate_passed_is_counted() {
    // #9741's merge-group run failed with `macos` green, and the queue removed
    // it for `failed_checks` five minutes later: another required check
    // ejected it.
    let fake = FakeGh::new("d");
    let report = compute(&observe(&fake, &query("d")).expect("gather"));
    let week = only_week(&report);
    let gate = &week.merge_group_gate;
    assert_eq!(
        (gate.gated_runs, gate.failures, gate.run_failed_gate_passed),
        (1, 0, 1)
    );
    let queue = &week.merge_queue;
    assert_eq!((queue.entries, queue.ejections), (1, 1));
    assert_eq!(queue.ejection_rate, Some(1.0));
    assert_eq!(
        removals(queue),
        vec![("failed_checks", "github-merge-queue", 1)]
    );
    assert_eq!(
        report.scopes.merge_queue,
        "every required check, as GitHub's removal reason reports it"
    );
    assert_eq!(
        report.scopes.merge_group_gate,
        "job `macos` of build.yml in merge-group runs only"
    );
}

#[test]
fn a_cancelled_gate_and_a_manual_removal_are_not_ejections() {
    // #9706's `macos` was cancelled after a runner took it, and shipyard-local
    // removed the pull request by hand two minutes later.
    let fake = FakeGh::new("e");
    let report = compute(&observe(&fake, &query("e")).expect("gather"));
    let week = only_week(&report);
    let gate = &week.merge_group_gate;
    assert_eq!((gate.gated_runs, gate.failures), (2, 1));
    assert_eq!(gate.by_cause.get("cancelled_after_start"), Some(&1));
    assert_eq!(gate.run_failed_gate_passed, 1);
    let queue = &week.merge_queue;
    assert_eq!((queue.entries, queue.ejections), (2, 0));
    assert_eq!(removals(queue), vec![("manual", "shipyard-local", 1)]);
}

#[test]
fn a_queue_branch_names_its_pull_request() {
    assert_eq!(
        queue_branch_pr("gh-readonly-queue/main/pr-9741-1f2e3d4c5b6a79881726354453627180918273645"),
        Some(9741)
    );
    assert_eq!(
        queue_branch_pr("gh-readonly-queue/release/v2/pr-12-abc"),
        Some(12)
    );
    for branch in [
        "feature/pr-12-abc",
        "gh-readonly-queue/main/pr-x-abc",
        "main",
    ] {
        assert_eq!(queue_branch_pr(branch), None, "{branch}");
    }
}

#[test]
fn a_short_or_unreadable_timeline_is_refused() {
    let fake = FakeGh::new("d");
    let captured: Value = serde_json::from_str(&fixture("timeline-9741.json")).expect("timeline");
    let mut short = captured.clone();
    short[0]["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"]
        .as_array_mut()
        .expect("nodes")
        .pop();
    let mut no_reason = captured;
    no_reason[0]["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"][1]
        .as_object_mut()
        .expect("removal")
        .remove("reason");
    for (answer, expected) in [
        (short, "collected 3 of filteredCount Some(4)"),
        (no_reason, "a removal has no reason"),
    ] {
        let answer = answer.to_string();
        let reader = |args: &[String]| {
            if args.join(" ").contains("timelineItems") {
                Ok(answer.clone())
            } else {
                fake.answer(args)
            }
        };
        let error = gather(&reader, &query("d"), &ReadCache::disabled()).expect_err("refused");
        assert!(error.contains(expected), "{error}");
    }
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
        verdict: MergeVerdict::None,
        commits: vec![PrCommit {
            oid: format!("{number:040}"),
            checks: checks
                .iter()
                .map(|(name, outcome)| CheckOutcome {
                    name: (*name).to_owned(),
                    outcome: Some((*outcome).to_owned()),
                    at: None,
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
        queue_events: Vec::new(),
        selection: None,
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
                at: Some(at("2026-10-05T11:30:00Z")),
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
        queue_events: Vec::new(),
        selection: None,
        gaps: Vec::new(),
    });
    assert_eq!(only_week(&report).capped_prs, vec![9755]);
    assert!(render(&report).contains("first 250 commits counted"));
}

#[test]
fn only_the_queue_s_own_check_removals_are_ejections() {
    let event = |pr: u64, kind: QueueEventKind| QueueEvent {
        pr,
        at: at("2026-10-06T12:00:00Z"),
        kind,
    };
    let removed = |reason: &str, actor: Option<&str>| QueueEventKind::Removed {
        reason: reason.to_owned(),
        actor: actor.map(str::to_owned),
    };
    let events = [
        event(1, QueueEventKind::Added),
        event(1, removed("failed_checks", Some("github-merge-queue"))),
        event(2, QueueEventKind::Added),
        event(2, removed("checks_timed_out", Some("github-merge-queue"))),
        event(3, QueueEventKind::Added),
        event(3, removed("failed_checks", Some("someone"))),
        event(4, QueueEventKind::Added),
        event(4, removed("merge_conflict", Some("github-merge-queue"))),
        event(5, QueueEventKind::Added),
        event(5, removed("manual", None)),
        // The queue records some manual removals as its own.
        event(6, QueueEventKind::Added),
        event(6, removed("manual", Some("github-merge-queue"))),
    ];
    let queue = merge_queue(&events.iter().collect::<Vec<_>>());
    assert_eq!((queue.entries, queue.ejections), (6, 2));
    assert_eq!(queue.ejection_rate, Some(0.33));
    assert_eq!(
        removals(&queue),
        vec![
            ("checks_timed_out", "github-merge-queue", 1),
            ("failed_checks", "github-merge-queue", 1),
            ("failed_checks", "someone", 1),
            ("manual", "github-merge-queue", 1),
            ("manual", "unknown", 1),
            ("merge_conflict", "github-merge-queue", 1),
        ]
    );
}

#[test]
fn pulp_week_41_has_thirty_eight_ejections() {
    // The merge-queue timeline of every pull request Pulp's 2026-W41 reads:
    // those a merge-group run in the week names and those merged in it (470).
    // A recompute from GitHub's removal records found 38 check-based
    // ejections, against 59 merge-group runs whose `macos` gate did not pass.
    let captured: Value = serde_json::from_str(&fixture("w41-timelines.json")).expect("timelines");
    let timelines = captured["timelines"].as_object().expect("timelines");
    assert_eq!(timelines.len(), 470);
    let mut queue_events = Vec::new();
    for (number, pages) in timelines {
        queue_events.extend(
            parse_queue_events(
                number.parse().expect("number"),
                pages.as_array().expect("pages"),
            )
            .expect("events"),
        );
    }
    let mut query = query("a");
    query.from = at("2026-10-05T00:00:00Z");
    query.to = at("2026-10-12T00:00:00Z");
    let report = compute(&ScorecardObservation {
        query,
        required: Ok((Vec::new(), "the base branch is not protected".to_owned())),
        prs: Vec::new(),
        merge_group_runs: Vec::new(),
        queue_events,
        selection: None,
        gaps: Vec::new(),
    });
    let week = only_week(&report);
    assert_eq!(week.week, "2026-W41");
    let queue = &week.merge_queue;
    assert_eq!((queue.entries, queue.ejections), (543, 38));
    assert_eq!(queue.ejection_rate, Some(0.07));
    assert_eq!(
        removals(queue),
        vec![
            ("checks_timed_out", "github-merge-queue", 2),
            ("failed_checks", "github-merge-queue", 36),
            ("manual", "danielraffel", 1),
            ("manual", "github-merge-queue", 5),
            ("manual", "shipyard-local", 27),
            ("merge_conflict", "github-merge-queue", 17),
            ("merged", "github-merge-queue", 455),
        ]
    );
}

// ---- merge-head verdict ------------------------------------------------------

fn verdicts_of(number: u64) -> Value {
    let answer: Value =
        serde_json::from_str(&fixture(&format!("pr-{number}-verdicts.json"))).expect("json");
    answer["data"]["repository"]["pullRequest"].clone()
}

/// #10042's merge head carries `reviewed: <head>` from the fleet App, posted
/// before it merged; the gather reads it through the fake `gh`.
#[test]
fn an_approval_on_the_merge_head_reads_approve() {
    let fake = FakeGh::new("b");
    let report = compute(&observe(&fake, &query("b")).expect("gather"));
    assert_eq!(
        only_week(&report).merge_verdicts,
        MergeVerdicts {
            approve: 1,
            ..MergeVerdicts::default()
        }
    );
    assert!(
        fake.calls()
            .iter()
            .any(|call| call.contains("reviews(last:100)") && call.contains("number=10042"))
    );
}

/// #10114 merged at 05:52:01Z, 2.5 minutes after a fleet `verdict: HOLD`
/// naming its merge head (captured 2026-10-13).
#[test]
fn a_hold_on_the_merge_head_reads_hold() {
    let pr = verdicts_of(10_114);
    assert_eq!(
        merge_verdict(&pr, at("2026-10-12T05:52:01Z")),
        Ok(MergeVerdict::Hold)
    );
    // Control: judged before the HOLD was posted, it was not held.
    assert_ne!(
        merge_verdict(&pr, at("2026-10-12T05:49:00Z")),
        Ok(MergeVerdict::Hold)
    );
}

fn synthetic(comments: &[Value], reviews: &[Value], totals: (u64, u64)) -> Value {
    serde_json::json!({
        "headRefOid": HEAD,
        "author": {"login": "alice"},
        "comments": {"totalCount": totals.0, "nodes": comments},
        "reviews": {"totalCount": totals.1, "nodes": reviews},
    })
}

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn comment(login: &str, association: &str, at: &str, body: &str) -> Value {
    serde_json::json!({"author": {"login": login}, "authorAssociation": association,
                       "createdAt": at, "body": body})
}

#[test]
fn only_a_trusted_verdict_before_the_merge_decides() {
    let merged = at("2026-10-12T12:00:00Z");
    let hold = format!("verdict: HOLD head:{HEAD}");
    let approve = format!("verdict: APPROVE head:{HEAD}");
    let read = |comments: Vec<Value>, reviews: Vec<Value>| {
        let totals = (comments.len() as u64, reviews.len() as u64);
        merge_verdict(&synthetic(&comments, &reviews, totals), merged).expect("read")
    };
    // The latest trusted verdict decides.
    assert_eq!(
        read(
            vec![
                comment(
                    "shipyard-local",
                    "CONTRIBUTOR",
                    "2026-10-12T10:00:00Z",
                    &approve
                ),
                comment("daniel", "OWNER", "2026-10-12T11:00:00Z", &hold),
            ],
            vec![]
        ),
        MergeVerdict::Hold
    );
    // A HOLD posted after the merge does not count.
    assert_eq!(
        read(
            vec![
                comment(
                    "shipyard-local",
                    "CONTRIBUTOR",
                    "2026-10-12T10:00:00Z",
                    &approve
                ),
                comment("daniel", "OWNER", "2026-10-12T13:00:00Z", &hold),
            ],
            vec![]
        ),
        MergeVerdict::Approve
    );
    // A drive-by account and the author never decide.
    assert_eq!(
        read(
            vec![
                comment("drive-by", "NONE", "2026-10-12T10:00:00Z", &hold),
                comment("alice", "OWNER", "2026-10-12T10:30:00Z", &approve),
            ],
            vec![]
        ),
        MergeVerdict::None
    );
    // A verdict naming another head is not about the merge head.
    let other = format!("verdict: HOLD head:{}", "b".repeat(40));
    assert_eq!(
        read(
            vec![comment("daniel", "OWNER", "2026-10-12T10:00:00Z", &other)],
            vec![]
        ),
        MergeVerdict::None
    );
    // A review on the merge head decides like a comment.
    let review = serde_json::json!({"author": {"login": "daniel"}, "authorAssociation": "OWNER",
        "state": "CHANGES_REQUESTED", "submittedAt": "2026-10-12T11:00:00Z",
        "commit": {"oid": HEAD}});
    assert_eq!(read(vec![], vec![review]), MergeVerdict::Hold);
    // A tie goes to the HOLD.
    assert_eq!(
        read(
            vec![
                comment("daniel", "OWNER", "2026-10-12T10:00:00Z", &approve),
                comment(
                    "shipyard-local",
                    "CONTRIBUTOR",
                    "2026-10-12T10:00:00Z",
                    &hold
                ),
            ],
            vec![]
        ),
        MergeVerdict::Hold
    );
}

fn edited(login: &str, association: &str, at: &str, edited: &str, body: &str) -> Value {
    let mut node = comment(login, association, at, body);
    node["lastEditedAt"] = Value::from(edited);
    node
}

/// A comment is read as it is now, so one posted before the merge and edited
/// after it cannot say what it said at the merge.
#[test]
fn a_verdict_edited_after_the_merge_is_unknown() {
    let merged = at("2026-10-12T12:00:00Z");
    let hold = format!("verdict: HOLD head:{HEAD}");
    let approve = format!("verdict: APPROVE head:{HEAD}");
    let read = |comments: Vec<Value>, reviews: Vec<Value>| {
        let totals = (comments.len() as u64, reviews.len() as u64);
        merge_verdict(&synthetic(&comments, &reviews, totals), merged).expect("read")
    };
    // An APPROVE edited to HOLD after the merge is not a held merge.
    assert_eq!(
        read(
            vec![edited(
                "shipyard-local",
                "CONTRIBUTOR",
                "2026-10-12T10:00:00Z",
                "2026-10-12T13:00:00Z",
                &hold
            )],
            vec![]
        ),
        MergeVerdict::Unread
    );
    // A verdict line added after the merge is not a pre-merge approval.
    assert_eq!(
        read(
            vec![
                comment("daniel", "OWNER", "2026-10-12T09:00:00Z", &hold),
                edited(
                    "daniel",
                    "OWNER",
                    "2026-10-12T10:00:00Z",
                    "2026-10-12T14:00:00Z",
                    &approve
                ),
            ],
            vec![]
        ),
        MergeVerdict::Unread
    );
    // A newer verdict that was not edited still decides.
    assert_eq!(
        read(
            vec![
                edited(
                    "daniel",
                    "OWNER",
                    "2026-10-12T09:00:00Z",
                    "2026-10-12T14:00:00Z",
                    &approve
                ),
                comment("daniel", "OWNER", "2026-10-12T10:00:00Z", &hold),
            ],
            vec![]
        ),
        MergeVerdict::Hold
    );
    // An edit made before the merge is the body at the merge.
    assert_eq!(
        read(
            vec![edited(
                "daniel",
                "OWNER",
                "2026-10-12T09:00:00Z",
                "2026-10-12T11:00:00Z",
                &hold
            )],
            vec![]
        ),
        MergeVerdict::Hold
    );
    // An untrusted account's edit is not a verdict either way.
    assert_eq!(
        read(
            vec![
                edited(
                    "drive-by",
                    "NONE",
                    "2026-10-12T11:00:00Z",
                    "2026-10-12T14:00:00Z",
                    &hold
                ),
                comment("daniel", "OWNER", "2026-10-12T10:00:00Z", &approve),
            ],
            vec![]
        ),
        MergeVerdict::Approve
    );
}

/// A review on the merge head dismissed since no longer says what it said.
#[test]
fn a_review_dismissed_since_is_unknown() {
    let merged = at("2026-10-12T12:00:00Z");
    let review = |state: &str, at: &str| {
        serde_json::json!({"author": {"login": "daniel"}, "authorAssociation": "OWNER",
            "state": state, "submittedAt": at, "commit": {"oid": HEAD}})
    };
    let read = |reviews: Vec<Value>| {
        let total = reviews.len() as u64;
        merge_verdict(&synthetic(&[], &reviews, (0, total)), merged).expect("read")
    };
    assert_eq!(
        read(vec![
            review("APPROVED", "2026-10-12T09:00:00Z"),
            review("DISMISSED", "2026-10-12T10:00:00Z"),
        ]),
        MergeVerdict::Unread
    );
    // An older dismissal does not hide a newer decision.
    assert_eq!(
        read(vec![
            review("DISMISSED", "2026-10-12T09:00:00Z"),
            review("CHANGES_REQUESTED", "2026-10-12T10:00:00Z"),
        ]),
        MergeVerdict::Hold
    );
}

#[test]
fn a_partial_listing_without_a_decision_is_unread() {
    let merged = at("2026-10-12T12:00:00Z");
    assert_eq!(
        merge_verdict(&synthetic(&[], &[], (150, 0)), merged),
        Ok(MergeVerdict::Unread)
    );
    assert_eq!(
        merge_verdict(&synthetic(&[], &[], (0, 0)), merged),
        Ok(MergeVerdict::None)
    );
}

#[test]
fn a_held_merge_is_named_in_the_week() {
    let mut held = pr(7, "2026-10-12T01:00:00Z", "2026-10-12T05:00:00Z", &[]);
    held.verdict = MergeVerdict::Hold;
    let mut approved = pr(8, "2026-10-12T01:00:00Z", "2026-10-12T05:00:00Z", &[]);
    approved.verdict = MergeVerdict::Approve;
    let mut query = query("a");
    query.from = at("2026-10-12T00:00:00Z");
    query.to = at("2026-10-13T00:00:00Z");
    let report = compute(&ScorecardObservation {
        query,
        required: Ok((Vec::new(), "--required-context".to_owned())),
        prs: vec![held, approved],
        merge_group_runs: Vec::new(),
        queue_events: Vec::new(),
        selection: None,
        gaps: Vec::new(),
    });
    let verdicts = &only_week(&report).merge_verdicts;
    assert_eq!(
        (
            verdicts.approve,
            verdicts.hold,
            verdicts.held_merged.clone()
        ),
        (1, 1, vec![7])
    );
    assert!(
        render(&report).contains("1 HOLD (merged anyway: #7)"),
        "{}",
        render(&report)
    );
}

// ---- export cases --------------------------------------------------------------

/// Window `a`'s two failed merge-group gates are cases; its four merged pull
/// requests had no red required check and no removal but `merged`.
#[test]
fn the_captured_window_exports_its_failed_gates() {
    let fake = FakeGh::new("a");
    let observation = observe(&fake, &query("a")).expect("gather");
    let rows = cases::cases(&observation);
    assert_eq!(rows.len(), 2, "{rows:#?}");
    for row in &rows {
        assert_eq!(row.kind, "merge_group_gate_failed");
        assert_eq!(row.repo, PULP);
        assert_eq!(row.outcome.as_deref(), Some("gate_failed"));
        assert_eq!(row.check.as_deref(), Some("macos"));
        assert!(row.run_id.is_some() && row.job_id.is_some(), "{row:?}");
        assert_eq!(row.label, None);
        let created = at(row.created_at.as_deref().expect("created"));
        assert_eq!(
            at(row.label_not_before.as_deref().expect("label time")),
            created + Duration::hours(24)
        );
        assert!(row.unread.contains(&"rule_verdict"));
    }
    assert!(rows[0].created_at <= rows[1].created_at, "oldest first");
}

#[test]
fn red_required_checks_and_removals_are_cases() {
    let mut red = pr(
        7,
        "2026-10-12T01:00:00Z",
        "2026-10-12T05:00:00Z",
        &[
            ("macos", "FAILURE"),
            ("advisory", "FAILURE"),
            ("drift-fast", "CANCELLED"),
        ],
    );
    red.commits[0].checks[0].at = Some(at("2026-10-12T02:00:00Z"));
    red.verdict = MergeVerdict::Hold;
    let mut query = query("a");
    query.from = at("2026-10-12T00:00:00Z");
    query.to = at("2026-10-13T00:00:00Z");
    let removed = |pr: u64, when: &str, reason: &str| QueueEvent {
        pr,
        at: at(when),
        kind: QueueEventKind::Removed {
            reason: reason.to_owned(),
            actor: Some("github-merge-queue".to_owned()),
        },
    };
    let observation = ScorecardObservation {
        query,
        required: Ok((
            vec!["macos".to_owned(), "drift-fast".to_owned()],
            "--required-context".to_owned(),
        )),
        prs: vec![red],
        merge_group_runs: Vec::new(),
        queue_events: vec![
            removed(7, "2026-10-12T03:00:00Z", "failed_checks"),
            removed(7, "2026-10-12T05:00:00Z", "merged"),
            removed(9, "2026-10-11T23:00:00Z", "failed_checks"),
        ],
        selection: None,
        gaps: Vec::new(),
    };
    let rows = cases::cases(&observation);
    let kinds: Vec<(&str, Option<&str>)> = rows
        .iter()
        .map(|row| (row.kind, row.outcome.as_deref()))
        .collect();
    // The advisory check is not required, a cancelled check is not red, a
    // `merged` removal and one before the window are not cases.
    assert_eq!(
        kinds,
        vec![
            ("required_check_red", Some("FAILURE")),
            ("queue_removal", Some("failed_checks")),
        ]
    );
    assert_eq!(rows[0].check.as_deref(), Some("macos"));
    assert_eq!(
        rows[0].head_sha.as_deref(),
        Some(format!("{:040}", 7).as_str())
    );
    assert_eq!(rows[0].created_at.as_deref(), Some("2026-10-12T02:00:00Z"));
    assert_eq!(rows[1].actor.as_deref(), Some("github-merge-queue"));
    for row in &rows {
        assert_eq!(row.merge_head_verdict, Some(MergeVerdict::Hold));
    }
}

#[test]
fn the_receipt_counts_the_rows_and_hashes_the_bytes() {
    let fake = FakeGh::new("a");
    let observation = observe(&fake, &query("a")).expect("gather");
    let rows = cases::cases(&observation);
    let (bytes, receipt) = cases::export(&observation, &rows).expect("export");
    let text = String::from_utf8(bytes.clone()).expect("utf-8");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON row per line"))
        .collect();
    assert_eq!(lines.len(), receipt.rows);
    assert_eq!(lines[0]["schema"], cases::SCHEMA);
    // Every field v1 does not read is present, null, and named as unread.
    for line in &lines {
        let unread: Vec<&str> = line["unread"]
            .as_array()
            .expect("unread")
            .iter()
            .map(|name| name.as_str().expect("name"))
            .collect();
        assert_eq!(
            unread,
            [
                "evidence",
                "rule_verdict",
                "cancel_cause",
                "receipt_same_tree",
                "outage_window"
            ]
        );
        for name in unread {
            assert!(line.get(name).is_some_and(Value::is_null), "{name}: {line}");
        }
        assert!(line["label"].is_null(), "{line}");
    }
    assert_eq!(receipt.by_kind.get("merge_group_gate_failed"), Some(&2));
    {
        use sha2::Digest;
        assert_eq!(receipt.sha256, hex::encode(sha2::Sha256::digest(&bytes)));
    }
    assert_eq!(receipt.from, "2026-10-05T15:00:00Z");
}

/// An in-progress merge-group gate has no conclusion yet and is not a case;
/// only completed gates that did not pass are.
#[test]
fn an_in_progress_merge_group_gate_is_not_a_case() {
    let fake = FakeGh::new("a");
    let mut observation = observe(&fake, &query("a")).expect("gather");
    let completed = cases::cases(&observation).len();
    assert_eq!(completed, 2, "control: the window's failed gates are cases");
    let mut running = observation.merge_group_runs[0].clone();
    running.run_id += 1_000_000;
    let gate = running.gate.as_mut().expect("the captured run has a gate");
    gate.status = "in_progress".to_owned();
    gate.conclusion = None;
    gate.completed_at = None;
    observation.merge_group_runs.push(running);
    let rows = cases::cases(&observation);
    assert_eq!(rows.len(), completed, "{rows:#?}");
    assert!(
        rows.iter()
            .all(|row| row.outcome.as_deref() != Some("no_conclusion")),
        "{rows:#?}"
    );
}

/// Unread required contexts make `required_check_red` unknown in the
/// receipt, never a silent zero.
#[test]
fn the_receipt_names_where_required_contexts_came_from() {
    let fake = FakeGh::new("a");
    let mut observation = observe(&fake, &query("a")).expect("gather");
    let (_, read) = cases::export(&observation, &cases::cases(&observation)).expect("export");
    assert!(read.unread_kinds.is_empty(), "{read:?}");
    assert!(!read.required_source.starts_with("unread"), "{read:?}");
    observation.required = Err("HTTP 403: branch protection".to_owned());
    let rows = cases::cases(&observation);
    assert!(rows.iter().all(|row| row.kind != "required_check_red"));
    let (_, unread) = cases::export(&observation, &rows).expect("export");
    assert_eq!(unread.unread_kinds, ["required_check_red"]);
    assert_eq!(
        unread.required_source,
        "unread: HTTP 403: branch protection"
    );
    assert_eq!(
        unread.required_source,
        compute(&observation).required_source,
        "the receipt and the report name the same source"
    );
}

/// `merge_head_verdict` is null by kind: always on a merge-group gate, and on
/// a removal of a pull request that did not merge in the window.
#[test]
fn merge_head_verdict_is_null_only_where_documented() {
    let fake = FakeGh::new("a");
    let mut observation = observe(&fake, &query("a")).expect("gather");
    observation.queue_events.push(QueueEvent {
        pr: 999_999,
        at: observation.query.from + Duration::minutes(1),
        kind: QueueEventKind::Removed {
            reason: "failed_checks".to_owned(),
            actor: Some("github-merge-queue".to_owned()),
        },
    });
    let rows = cases::cases(&observation);
    let merged: BTreeSet<u64> = observation.prs.iter().map(|pr| pr.number).collect();
    assert!(
        rows.iter().any(|row| row.kind == "queue_removal"),
        "control"
    );
    for row in &rows {
        let expect_null = match row.kind {
            "merge_group_gate_failed" => true,
            "queue_removal" => !row.pr.is_some_and(|pr| merged.contains(&pr)),
            _ => false,
        };
        assert_eq!(row.merge_head_verdict.is_none(), expect_null, "{row:?}");
    }
}
