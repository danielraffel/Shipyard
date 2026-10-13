//! Same-SHA flakes over answers captured from `Generous-Corp/pulp` on
//! 2026-10-12 UTC with the requests [`gather`] sends:
//!
//! - `tests/fixtures/gate-cost-scorecard/flakes/w1012/runs-<event>.json`:
//!   the window 2026-10-12 20:36:00..20:36:30 holds one run, pull-request run
//!   38305299000 on #10154's head `a3d47158`;
//! - `tests/fixtures/ctest-log/`: that run's jobs and its `macos` job's log,
//!   in which `timeline agent decodes local file URI media locators` failed
//!   and passed on its `--repeat` retry; and merge-group run 38308868279's
//!   `macos` job, in which `pulp.vellum-closure-external-input-negative`
//!   failed twice (it passed in the pull-request job's full suite);
//! - `.../flakes/w1005/`: the window 2026-10-10 06:05:30..06:05:45 holds
//!   pull-request run 38029738235 on `8ae6e0c8`, run twice. In attempt 1
//!   `pulp-browser-capture-node-integration` failed and passed on its retry,
//!   and in attempt 2 it passed; `changed-surface-script-families-drift`
//!   failed twice in both. Its logs were reduced keeping both tests;
//! - `.../flakes/w1012b/`: the window 2026-10-12 14:32:45..14:32:55 holds
//!   pull-request run 38263712561, whose first `macos` attempt was cancelled
//!   before any runner took it and whose second passed.
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use super::*;

const PULP: &str = "Generous-Corp/pulp";
const HEAD: &str = "a3d471584ceae8cc0d0eb9c82d86cd11bbaef56f";
const TIMELINE: &str = "timeline agent decodes local file URI media locators";
const VELLUM: &str = "pulp.vellum-closure-external-input-negative";

fn fixture(path: &str) -> Result<String, String> {
    let path = format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))
}

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("timestamp")
        .with_timezone(&Utc)
}

fn query() -> ScorecardQuery {
    query_for("2026-10-12T20:36:00Z", "2026-10-12T20:36:30Z")
}

fn query_for(from: &str, to: &str) -> ScorecardQuery {
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

struct FakeGh {
    /// The window's directory under `flakes/`.
    window: &'static str,
    calls: Mutex<Vec<String>>,
    refuse: Vec<String>,
    /// Requests containing this string are answered `HTTP 502` this many
    /// times before the capture.
    gateway_errors: Option<(String, Mutex<u32>)>,
}

impl FakeGh {
    fn new() -> Self {
        Self::window("w1012")
    }

    fn window(window: &'static str) -> Self {
        Self {
            window,
            calls: Mutex::new(Vec::new()),
            refuse: Vec::new(),
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
        let path = args
            .iter()
            .find(|arg| arg.starts_with("repos/"))
            .cloned()
            .unwrap_or_default();
        let rest = path
            .strip_prefix(&format!("repos/{PULP}/"))
            .unwrap_or_default();
        if rest == "actions/workflows/build.yml/runs" {
            let event = args
                .iter()
                .find_map(|arg| arg.strip_prefix("event="))
                .expect("event");
            assert!(joined.contains("created="), "{joined}");
            return fixture(&format!(
                "gate-cost-scorecard/flakes/{}/runs-{event}.json",
                self.window
            ));
        }
        let own = |name: String| {
            fixture(&format!(
                "gate-cost-scorecard/flakes/{}/{name}",
                self.window
            ))
        };
        if let Some(id) = rest
            .strip_prefix("actions/runs/")
            .and_then(|tail| tail.strip_suffix("/jobs"))
        {
            return own(format!("jobs-run-{id}.json"))
                .or_else(|_| fixture(&format!("ctest-log/jobs-pr-run-{id}.json")));
        }
        if let Some(id) = rest
            .strip_prefix("actions/jobs/")
            .and_then(|tail| tail.strip_suffix("/logs"))
        {
            return own(format!("log-job-{id}.log"))
                .or_else(|_| fixture(&format!("ctest-log/log-pr-job-{id}.log")));
        }
        Err(format!("unexpected call: {joined}"))
    }
}

fn observe(fake: &FakeGh) -> FlakeObservation {
    observe_in(fake, &query())
}

fn observe_in(fake: &FakeGh, query: &ScorecardQuery) -> FlakeObservation {
    let reader = |args: &[String]| fake.answer(args);
    gather(&reader, &ReadCache::disabled(), query).expect("gather")
}

#[test]
fn a_test_that_passed_on_its_retry_is_a_flake_on_that_commit() {
    let fake = FakeGh::new();
    let observation = observe(&fake);
    // Of the run's fourteen jobs, only `macos` is read: one log.
    assert_eq!(observation.jobs.len(), 1);
    let job = &observation.jobs[0];
    assert_eq!(
        (
            job.run_id,
            job.job_id,
            job.event.as_str(),
            job.head_sha.as_str()
        ),
        (38_305_299_000, 114_997_948_499, "pull_request", HEAD)
    );
    let outcomes = job.outcomes.as_ref().expect("outcomes");
    assert_eq!(outcomes.results, 99 + 28 + 165);
    assert_eq!(
        outcomes.failed.get(TIMELINE),
        Some(&Tally {
            passed: 1,
            failed: 1
        })
    );
    // Only tests with a failure are kept.
    assert_eq!(outcomes.failed.len(), 1);
    let flakes = compute(PULP, &observation);
    assert_eq!(
        (
            flakes.jobs_read,
            flakes.head_shas,
            flakes.shas_with_several_jobs,
            flakes.flaky_tests
        ),
        (1, 1, 0, 1)
    );
    let top = &flakes.top[0];
    assert_eq!(top.name, TIMELINE);
    assert_eq!((top.flake_count, top.retry, top.rerun), (1, 1, 0));
    assert_eq!(
        top.occurrences[0].jobs,
        vec!["https://github.com/Generous-Corp/pulp/actions/runs/38305299000/job/114997948499"]
    );
    let logs = fake
        .calls
        .lock()
        .expect("calls")
        .iter()
        .filter(|call| call.ends_with("/logs"))
        .count();
    assert_eq!(logs, 1);
}

#[test]
fn jobs_that_disagree_on_one_commit_are_a_rerun_flake() {
    // The merge-group job failed `VELLUM` twice; the pull-request job passed
    // it. Their real outcomes, placed on one commit as two attempts would be.
    let failed = outcomes_from_log(&fixture("ctest-log/log-mg-job-115009634572.log").expect("log"));
    assert_eq!(
        failed.failed.get(VELLUM),
        Some(&Tally {
            passed: 0,
            failed: 2
        })
    );
    let log = fixture("ctest-log/log-pr-job-114997948499.log").expect("log");
    let job = |run_id, job_id, outcomes| GateJob {
        run_id,
        job_id,
        event: "pull_request".to_owned(),
        head_sha: HEAD.to_owned(),
        outcomes: Ok(outcomes),
        cross: Ok(BTreeMap::new()),
    };
    let mut passed = job(2, 20, outcomes_from_log(&log));
    // What [`gather`] asks of the passing job: its results for the tests
    // that failed in the other job on the commit.
    let text = JobLog::new(&log);
    let mut tally = Tally::default();
    for result in ctest_log::results(text.lines()) {
        if result.name == VELLUM {
            tally.passed += u32::from(result.passed());
            tally.failed += u32::from(result.failed());
        }
    }
    assert_eq!(
        tally,
        Tally {
            passed: 1,
            failed: 0
        }
    );
    passed.cross = Ok(BTreeMap::from([(VELLUM.to_owned(), tally)]));
    let observation = FlakeObservation {
        jobs: vec![job(1, 10, failed), passed],
        unlisted_runs: Vec::new(),
    };
    let flakes = compute(PULP, &observation);
    assert_eq!(flakes.shas_with_several_jobs, 1);
    let names: Vec<(&str, usize, usize, usize)> = flakes
        .top
        .iter()
        .map(|test| (test.name.as_str(), test.flake_count, test.retry, test.rerun))
        .collect();
    // Tied on one commit each, by name.
    assert_eq!(names, vec![(VELLUM, 1, 0, 1), (TIMELINE, 1, 1, 0)]);
    let vellum = &flakes.top[0].occurrences[0];
    assert_eq!((vellum.failed, vellum.passed), (2, 1));
    assert_eq!(vellum.jobs.len(), 2);
    // Without the passing job's answer there is no flake.
    let alone = FlakeObservation {
        jobs: vec![observation.jobs[0].clone()],
        unlisted_runs: Vec::new(),
    };
    assert_eq!(compute(PULP, &alone).flaky_tests, 0);
}

#[test]
fn a_test_failing_twice_on_one_commit_is_not_a_flake() {
    let failed = outcomes_from_log(&fixture("ctest-log/log-mg-job-115009634572.log").expect("log"));
    let observation = FlakeObservation {
        jobs: vec![GateJob {
            run_id: 38_308_868_279,
            job_id: 115_009_634_572,
            event: "merge_group".to_owned(),
            head_sha: "6cb6bad799ddccaa04e2b9116d2075e1ba7823e9".to_owned(),
            outcomes: Ok(failed),
            cross: Ok(BTreeMap::new()),
        }],
        unlisted_runs: Vec::new(),
    };
    let flakes = compute(PULP, &observation);
    assert_eq!((flakes.jobs_read, flakes.flaky_tests), (1, 0));
}

#[test]
fn an_unreadable_log_is_counted_and_named() {
    let mut fake = FakeGh::new();
    fake.refuse = vec!["/logs".to_owned()];
    let observation = observe(&fake);
    let flakes = compute(PULP, &observation);
    assert_eq!((flakes.jobs_read, flakes.jobs_unread), (0, 1));
    // A refused log is not asked for again; only a gateway error is.
    let logs = fake
        .calls
        .lock()
        .expect("calls")
        .iter()
        .filter(|call| call.ends_with("/logs"))
        .count();
    assert_eq!(logs, 1);
    assert!(
        flakes.unread_examples[0].starts_with("job 114997948499: log of job 114997948499"),
        "{:?}",
        flakes.unread_examples
    );
    assert!(render(&flakes).contains("  unread: job 114997948499"));
}

#[test]
fn the_report_ranks_by_commits_and_links_jobs() {
    let fake = FakeGh::new();
    let flakes = compute(PULP, &observe(&fake));
    let text = render(&flakes);
    assert!(
        text.starts_with(
            "same-SHA flakes (pull_request, merge_group, workflow_dispatch runs): 1 test(s) \
             passed and failed on one commit; 1 gate jobs read on 1 commits (0 with several \
             jobs), 0 unread, 0 runs unlisted\n"
        ),
        "{text}"
    );
    assert!(text.contains(&format!(
        "  {TIMELINE} on 1 commit(s) (1 retry, 0 rerun): \
         https://github.com/Generous-Corp/pulp/actions/runs/38305299000/job/114997948499"
    )));
    let json = serde_json::to_value(&flakes).expect("JSON");
    assert_eq!(json["top"][0]["occurrences"][0]["kind"], "retry");
    assert_eq!(json["top"][0]["occurrences"][0]["head_sha"], HEAD);
}

fn on(head: &str, job_id: u64, failed: &[(&str, Tally)]) -> GateJob {
    GateJob {
        run_id: job_id,
        job_id,
        event: "merge_group".to_owned(),
        head_sha: head.to_owned(),
        outcomes: Ok(JobOutcomes {
            failed: failed
                .iter()
                .map(|(name, tally)| ((*name).to_owned(), *tally))
                .collect(),
            results: failed.len(),
        }),
        cross: Ok(BTreeMap::new()),
    }
}

#[test]
fn more_commits_rank_first_and_only_the_top_ten_are_listed() {
    let flip = Tally {
        passed: 1,
        failed: 1,
    };
    // `zz-most` flipped on three commits; eleven others on one each.
    let mut jobs: Vec<GateJob> = (0..3)
        .map(|index| on(&format!("sha-most-{index}"), index, &[("zz-most", flip)]))
        .collect();
    for index in 0..11 {
        let name = format!("t{index:02}");
        jobs.push(on(&format!("sha-{index}"), 100 + index, &[(&name, flip)]));
    }
    let flakes = compute(
        PULP,
        &FlakeObservation {
            jobs,
            unlisted_runs: Vec::new(),
        },
    );
    assert_eq!(flakes.flaky_tests, 12);
    assert_eq!(flakes.top.len(), TOP);
    assert_eq!(flakes.top[0].name, "zz-most");
    assert_eq!(flakes.top[0].flake_count, 3);
    assert_eq!(flakes.top[1].name, "t00");
    assert_eq!(flakes.top[9].name, "t08");
}

#[test]
fn two_attempts_on_one_commit_are_read_for_each_other_s_failures() {
    let fake = FakeGh::window("w1005");
    let query = query_for("2026-10-10T06:05:30Z", "2026-10-10T06:05:45Z");
    let observation = observe_in(&fake, &query);
    let jobs: Vec<(u64, &str)> = observation
        .jobs
        .iter()
        .map(|job| (job.job_id, job.head_sha.as_str()))
        .collect();
    let sha = "8ae6e0c8e3853ec65bdb56824e3f1885ff0865ce";
    assert_eq!(jobs, vec![(114_148_117_573, sha), (114_151_219_829, sha)]);
    let browser = "pulp-browser-capture-node-integration";
    let drift = "changed-surface-script-families-drift";
    // Attempt 1 failed both; attempt 2 failed only the drift check, so only
    // attempt 2 is asked about the other test, and attempt 1 about nothing.
    assert_eq!(observation.jobs[0].cross, Ok(BTreeMap::new()));
    assert_eq!(
        observation.jobs[1].cross,
        Ok(BTreeMap::from([(
            browser.to_owned(),
            Tally {
                passed: 1,
                failed: 0
            }
        )]))
    );
    let flakes = compute(PULP, &observation);
    assert_eq!((flakes.head_shas, flakes.shas_with_several_jobs), (1, 1));
    assert_eq!(flakes.flaky_tests, 1);
    let top = &flakes.top[0];
    assert_eq!(top.name, browser);
    assert_eq!((top.flake_count, top.retry, top.rerun), (1, 1, 0));
    let occurrence = &top.occurrences[0];
    assert_eq!((occurrence.failed, occurrence.passed), (1, 2));
    assert_eq!(occurrence.jobs.len(), 2);
    assert!(!flakes.top.iter().any(|test| test.name == drift));
    // Each log is read once for its own failures and attempt 2's once more.
    let logs = fake
        .calls
        .lock()
        .expect("calls")
        .iter()
        .filter(|call| call.ends_with("/logs"))
        .count();
    assert_eq!(logs, 3);
}

#[test]
fn a_gateway_error_is_read_again_and_a_lasting_one_fails_the_listing() {
    // Two `HTTP 502` answers on the run listing, as a 14-day read of Pulp
    // met on a page of pull-request runs: the third attempt is read.
    let mut fake = FakeGh::new();
    fake.gateway_errors = Some(("event=pull_request".to_owned(), Mutex::new(2)));
    let observation = observe(&fake);
    assert_eq!(observation.jobs.len(), 1);
    let listings = fake
        .calls
        .lock()
        .expect("calls")
        .iter()
        .filter(|call| call.contains("event=pull_request"))
        .count();
    assert_eq!(listings, 3);
    // Three in a row fail the read, naming the status.
    let mut fake = FakeGh::new();
    fake.gateway_errors = Some(("event=pull_request".to_owned(), Mutex::new(3)));
    let reader = |args: &[String]| fake.answer(args);
    let error = gather(&reader, &ReadCache::disabled(), &query()).expect_err("lasting 502");
    assert!(error.contains("HTTP 502"), "{error}");
    // A log's gateway error is read again too.
    let mut fake = FakeGh::new();
    fake.gateway_errors = Some(("/logs".to_owned(), Mutex::new(1)));
    let observation = observe(&fake);
    assert!(observation.jobs[0].outcomes.is_ok());
}

#[test]
fn an_attempt_no_runner_took_has_no_log_to_read() {
    let fake = FakeGh::window("w1012b");
    let query = query_for("2026-10-12T14:32:45Z", "2026-10-12T14:32:55Z");
    let observation = observe_in(&fake, &query);
    let jobs: Vec<u64> = observation.jobs.iter().map(|job| job.job_id).collect();
    assert_eq!(jobs, vec![114_957_933_055]);
    let flakes = compute(PULP, &observation);
    assert_eq!((flakes.jobs_read, flakes.jobs_unread), (1, 0));
}
