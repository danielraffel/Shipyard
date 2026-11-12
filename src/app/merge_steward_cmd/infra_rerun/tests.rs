use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use super::*;
use crate::app::diagnose_cmd::diagnose_gathered;
use crate::diagnose::Step;
use crate::merge_steward::{CapacityPreemptionPolicy, StewardPullRequest};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const T0: i64 = 1_794_400_000;

fn head(pr: u64) -> String {
    format!("{:040x}", 0xa000 + pr)
}

fn check(name: &str, conclusion: &str, run_id: u64, check_id: u64) -> StewardCheck {
    StewardCheck {
        name: name.to_owned(),
        source: StewardCheckSource::CheckRun,
        app_id: Some(15_368),
        status: "COMPLETED".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        run_id: Some(run_id),
        observed_at: None,
        check_id: Some(check_id),
        description: None,
    }
}

fn pr(number: u64, checks: Vec<StewardCheck>, labels: &[&str]) -> ObservedPr {
    ObservedPr {
        node_id: format!("PR_{number}"),
        owner: None,
        fact: StewardPullRequest {
            number,
            head_sha: head(number),
            head_branch: format!("feature-{number}"),
            author: Some("someone".to_owned()),
            draft: false,
            merge_state: "BLOCKED".to_owned(),
            auto_merge_active: false,
            queue_position: None,
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            checks,
        },
        check_rollup_maybe_truncated: false,
    }
}

fn observation_of(prs: impl Iterator<Item = ObservedPr>) -> RepoObservation {
    observation(prs.collect())
}

fn observation(prs: Vec<ObservedPr>) -> RepoObservation {
    RepoObservation {
        repo: "Owner/Repo".to_owned(),
        base: "main".to_owned(),
        allow_auto_merge: true,
        merge_queue: true,
        required_checks: ["drift-fast", "macos", "review-verdict"]
            .into_iter()
            .map(|context| RequiredCheck {
                context: context.to_owned(),
                app_id: Some(15_368),
            })
            .collect(),
        prs,
        runs: Vec::new(),
        merge_group_heads: BTreeMap::new(),
        merge_group_enqueued_at: BTreeMap::new(),
        capacity_preemption_policy: CapacityPreemptionPolicy::for_repository("owner/repo"),
        preemption_error: None,
    }
}

/// Each pull request's checks as the census read them.
fn census(observation: &RepoObservation) -> BTreeMap<u64, Vec<StewardCheck>> {
    observation
        .prs
        .iter()
        .map(|pr| (pr.fact.number, pr.fact.checks.clone()))
        .collect()
}

fn queued(name: &str, run_id: u64, check_id: u64) -> StewardCheck {
    StewardCheck {
        status: "QUEUED".to_owned(),
        conclusion: None,
        ..check(name, "FAILURE", run_id, check_id)
    }
}

fn step(name: &str, conclusion: &str) -> Step {
    Step {
        number: None,
        name: name.to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: None,
        completed_at: None,
    }
}

/// A completed job of `run_id` on `sha` that failed at `failing_step`.
fn job(id: i64, name: &str, run_id: u64, attempt: u64, sha: &str, failing_step: &str) -> Job {
    Job {
        id,
        name: name.to_owned(),
        conclusion: Some("failure".to_owned()),
        status: Some("completed".to_owned()),
        runner_name: Some("GitHub Actions 7".to_owned()),
        run_id: Some(run_id),
        run_attempt: Some(attempt),
        head_sha: Some(sha.to_owned()),
        head_branch: Some("feature".to_owned()),
        steps: Some(vec![
            step("Set up job", "success"),
            step(failing_step, "failure"),
        ]),
        ..Job::default()
    }
}

fn required() -> Vec<String> {
    ["drift-fast", "macos", "review-verdict"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// The real diagnosis of `jobs`, through the same function the steward uses.
fn diagnosed(jobs: &[Job], logs: &[(i64, &str)]) -> (Diagnosis, Vec<Job>) {
    let logs: HashMap<i64, String> = logs
        .iter()
        .map(|(id, text)| (*id, (*text).to_owned()))
        .collect();
    (
        diagnose_gathered(jobs, &logs, HashMap::new(), &required()),
        jobs.to_vec(),
    )
}

/// Offline [`Acts`]: each run answers with the jobs the test put there; every
/// diagnosis and re-run is counted.
#[derive(Default)]
struct FakeActs {
    runs: RefCell<BTreeMap<u64, Vec<Job>>>,
    logs: RefCell<Vec<(i64, String)>>,
    diagnosed: RefCell<Vec<u64>>,
    reran: RefCell<Vec<u64>>,
    rollups: RefCell<BTreeMap<String, Vec<StewardCheck>>>,
    rollup_reads: RefCell<usize>,
    refuse: bool,
}

impl FakeActs {
    fn set(&self, run_id: u64, jobs: Vec<Job>) {
        self.runs.borrow_mut().insert(run_id, jobs);
    }
}

impl Acts for FakeActs {
    fn rollups(&self, heads: &[String]) -> Result<BTreeMap<String, Vec<StewardCheck>>, String> {
        *self.rollup_reads.borrow_mut() += 1;
        let known = self.rollups.borrow();
        Ok(heads
            .iter()
            .filter_map(|head| known.get(head).map(|checks| (head.clone(), checks.clone())))
            .collect())
    }

    fn diagnose(&self, run_id: u64) -> Result<(Diagnosis, Vec<Job>), String> {
        self.diagnosed.borrow_mut().push(run_id);
        let jobs = self.runs.borrow().get(&run_id).cloned().unwrap_or_default();
        let logs = self.logs.borrow();
        let logs: Vec<(i64, &str)> = logs.iter().map(|(id, text)| (*id, text.as_str())).collect();
        Ok(diagnosed(&jobs, &logs))
    }

    fn rerun_failed_jobs(&self, run_id: u64) -> Result<(), String> {
        if self.refuse {
            return Err("HTTP 403".to_owned());
        }
        self.reran.borrow_mut().push(run_id);
        Ok(())
    }
}

fn run_sweep(
    observation: &RepoObservation,
    mode: Mode,
    now: i64,
    ledger: &mut StewardLedger,
    acts: &FakeActs,
) -> RepoStatus {
    sweep(
        &Sweep {
            observation,
            mode,
            approval_context: "review-verdict",
            excluded_labels: &["shipyard:no-auto-merge".to_owned()],
            now_epoch: now,
            deadline: None,
            ledger_path: None,
        },
        ledger,
        acts,
    )
    .0
}

fn actions(status: &RepoStatus) -> Vec<&str> {
    status
        .outcomes
        .iter()
        .map(|outcome| outcome.action.as_str())
        .collect()
}

// ---- next(): the per-check budget ----

#[test]
fn next_budget_is_one_rerun_then_one_after_backoff_then_capped() {
    assert_eq!(next(None, 1, T0, Mode::Enforce), Next::Diagnose);
    let mut record = Record {
        reruns: 1,
        last_rerun_epoch: Some(T0),
        judged_check_id: Some(1),
        verdict: "rerun".to_owned(),
        ..Record::default()
    };
    // The re-run's new attempt has not shown: the old red is still observed.
    assert_eq!(
        next(Some(&record), 1, T0 + 60, Mode::Enforce),
        Next::Pending
    );
    // The re-run failed again inside the backoff: wait.
    assert_eq!(
        next(Some(&record), 2, T0 + 5 * 60, Mode::Enforce),
        Next::Wait {
            remaining_secs: BACKOFF_SECS - 5 * 60
        }
    );
    assert_eq!(
        next(Some(&record), 2, T0 + BACKOFF_SECS, Mode::Enforce),
        Next::Diagnose
    );
    record.reruns = 2;
    record.judged_check_id = Some(2);
    // A third red after two re-runs is classified once (so a real failure is
    // declined, not reported as an outage) ...
    assert_eq!(
        next(Some(&record), 3, T0 + 10 * BACKOFF_SECS, Mode::Enforce),
        Next::Diagnose
    );
    // ... and once capped, never read again.
    record.verdict = "capped".to_owned();
    record.judged_check_id = Some(3);
    assert_eq!(
        next(Some(&record), 3, T0 + 11 * BACKOFF_SECS, Mode::Enforce),
        Next::Settled("capped".to_owned())
    );
    assert_eq!(
        next(Some(&record), 4, T0 + 12 * BACKOFF_SECS, Mode::Enforce),
        Next::Settled("capped".to_owned())
    );
}

#[test]
fn next_lets_an_enforcing_pass_act_on_a_shadow_call() {
    let record = Record {
        judged_check_id: Some(7),
        verdict: "would_rerun".to_owned(),
        ..Record::default()
    };
    assert_eq!(next(Some(&record), 7, T0, Mode::Shadow), Next::Pending);
    assert_eq!(next(Some(&record), 7, T0, Mode::Enforce), Next::Diagnose);
}

#[test]
fn next_settles_a_declined_red_without_rereading_it() {
    let record = Record {
        judged_check_id: Some(7),
        verdict: "declined".to_owned(),
        ..Record::default()
    };
    assert_eq!(
        next(Some(&record), 7, T0, Mode::Enforce),
        Next::Settled("declined".to_owned())
    );
    // A new red on the same head (someone re-ran it) is judged afresh.
    assert_eq!(next(Some(&record), 8, T0, Mode::Enforce), Next::Diagnose);
}

#[test]
fn next_retries_a_refused_rerun_only_after_the_backoff() {
    let record = Record {
        judged_check_id: Some(7),
        verdict: "error".to_owned(),
        last_error_epoch: Some(T0),
        ..Record::default()
    };
    assert!(matches!(
        next(Some(&record), 7, T0 + 60, Mode::Enforce),
        Next::Wait { .. }
    ));
    assert_eq!(
        next(Some(&record), 7, T0 + BACKOFF_SECS, Mode::Enforce),
        Next::Diagnose
    );
    let settled = Record {
        errors: MAX_ERRORS,
        ..record
    };
    assert_eq!(
        next(Some(&settled), 7, T0 + 10 * BACKOFF_SECS, Mode::Enforce),
        Next::Settled("refused".to_owned())
    );
}

// ---- candidates(): what is even considered, before any read ----

#[test]
fn review_verdict_is_never_a_candidate() {
    let observation = observation(vec![pr(
        1,
        vec![
            check("review-verdict", "FAILURE", 10, 100),
            check("drift-fast", "FAILURE", 11, 110),
        ],
        &[],
    )]);
    let checks = census(&observation);
    let found = candidates(&observation, &checks, "review-verdict", &[]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].check, "drift-fast");
    // Also when the steward is configured with another approval context.
    let found = candidates(&observation, &checks, "approval", &[]);
    assert!(
        found
            .iter()
            .all(|candidate| candidate.check != "review-verdict")
    );
}

#[test]
fn candidates_skip_unrequired_cancelled_opted_out_and_superseded() {
    let mut cancelled = check("macos", "CANCELLED", 12, 120);
    cancelled.conclusion = Some("CANCELLED".to_owned());
    let observation = observation(vec![
        pr(1, vec![check("lint", "FAILURE", 10, 100)], &[]),
        pr(2, vec![cancelled], &[]),
        pr(
            3,
            vec![check("macos", "FAILURE", 13, 130)],
            &["shipyard:no-auto-merge"],
        ),
        pr(4, vec![check("macos", "FAILURE", 14, 140)], &["superseded"]),
        pr(5, vec![check("macos", "FAILURE", 15, 150)], &[]),
        pr(
            6,
            vec![check("macos", "FAILURE", 16, 160)],
            &["owner-decision"],
        ),
        pr(
            7,
            vec![check("macos", "FAILURE", 17, 170)],
            &["PARKED/linux"],
        ),
    ]);
    let found = candidates(
        &observation,
        &census(&observation),
        "review-verdict",
        &["shipyard:no-auto-merge".to_owned()],
    );
    let prs: Vec<u64> = found.iter().map(|candidate| candidate.pr).collect();
    assert_eq!(prs, vec![5]);
}

#[test]
fn candidates_use_the_newest_attempt_of_a_check() {
    // The newer attempt passed: the older red is history, not a candidate.
    let observation = observation(vec![pr(
        1,
        vec![
            check("macos", "FAILURE", 10, 100),
            check("macos", "SUCCESS", 10, 101),
        ],
        &[],
    )]);
    assert!(candidates(&observation, &census(&observation), "review-verdict", &[]).is_empty());
}

// ---- judge_run(): the classifier's verdict on a run ----

#[test]
fn judge_reruns_a_failed_install_dependencies_step() {
    let jobs = [job(
        100,
        "drift-fast",
        10,
        1,
        HEAD,
        "Install Linux dependencies",
    )];
    let (doc, jobs) = diagnosed(&jobs, &[]);
    assert_eq!(
        judge_run(10, HEAD, &doc, &jobs, "review-verdict", &["drift-fast"]),
        RunVerdict::Rerun
    );
}

#[test]
fn judge_never_reruns_a_real_test_failure() {
    // Negative control: the same check, failing in its test step with a
    // named failing test, is a code failure and must not be re-run.
    let jobs = [job(100, "drift-fast", 10, 1, HEAD, "Run drift tests")];
    let log = "2026-11-11T14:40:00.0000000Z ##[group]Run drift tests\n\
               2026-11-11T14:40:01.0000000Z The following tests FAILED:\n\
               2026-11-11T14:40:01.0000000Z \t 12 - census-drift (Failed)\n\
               2026-11-11T14:40:02.0000000Z ##[error]Process completed with exit code 8.\n";
    let (doc, jobs) = diagnosed(&jobs, &[(100, log)]);
    let verdict = judge_run(10, HEAD, &doc, &jobs, "review-verdict", &["drift-fast"]);
    assert!(
        matches!(&verdict, RunVerdict::Decline(why) if why.contains("real")),
        "{verdict:?}"
    );
}

#[test]
fn judge_declines_a_run_with_one_real_failure_beside_an_infra_one() {
    let jobs = [
        job(100, "drift-fast", 10, 1, HEAD, "Install Linux dependencies"),
        job(101, "macos", 10, 1, HEAD, "Run tests"),
    ];
    let (doc, jobs) = diagnosed(&jobs, &[]);
    assert!(matches!(
        judge_run(10, HEAD, &doc, &jobs, "review-verdict", &["drift-fast"]),
        RunVerdict::Decline(_)
    ));
}

#[test]
fn judge_declines_a_run_in_which_review_verdict_failed() {
    // review-verdict's failing step is a non-content one, so the classifier
    // alone calls it infra; rerun-failed-jobs would re-run it.
    let jobs = [
        job(100, "drift-fast", 10, 1, HEAD, "Install Linux dependencies"),
        job(101, "review-verdict", 10, 1, HEAD, "Post review verdict"),
    ];
    let (doc, jobs) = diagnosed(&jobs, &[]);
    let verdict = judge_run(10, HEAD, &doc, &jobs, "review-verdict", &["drift-fast"]);
    assert!(
        matches!(&verdict, RunVerdict::Decline(why) if why.contains("review-verdict")),
        "{verdict:?}"
    );
}

#[test]
fn judge_caps_on_githubs_own_attempt_count() {
    let jobs = [job(
        100,
        "drift-fast",
        10,
        3,
        HEAD,
        "Install Linux dependencies",
    )];
    let (doc, jobs) = diagnosed(&jobs, &[]);
    assert_eq!(
        judge_run(10, HEAD, &doc, &jobs, "review-verdict", &["drift-fast"]),
        RunVerdict::Capped
    );
}

#[test]
fn judge_waits_for_a_run_still_in_flight_and_refuses_another_head() {
    let mut running = job(101, "lint", 10, 1, HEAD, "Lint");
    running.status = Some("in_progress".to_owned());
    running.conclusion = None;
    let jobs = [
        job(100, "drift-fast", 10, 1, HEAD, "Install Linux dependencies"),
        running,
    ];
    let (doc, jobs) = diagnosed(&jobs, &[]);
    assert_eq!(
        judge_run(10, HEAD, &doc, &jobs, "review-verdict", &["drift-fast"]),
        RunVerdict::InFlight
    );

    let jobs = [job(
        100,
        "drift-fast",
        10,
        1,
        &head(9),
        "Install Linux dependencies",
    )];
    let (doc, jobs) = diagnosed(&jobs, &[]);
    assert!(matches!(
        judge_run(10, HEAD, &doc, &jobs, "review-verdict", &["drift-fast"]),
        RunVerdict::Decline(_)
    ));
}

// ---- sweep(): the pass end to end, offline ----

/// The 2026-11-11 incident: the apt mirror is down, `drift-fast` fails at
/// "Install Linux dependencies" on every PR. One pass re-runs each once and
/// reports one outage finding.
#[test]
fn incident_reruns_each_pr_once_and_reports_one_outage() {
    let acts = FakeActs::default();
    let mut prs = Vec::new();
    for number in 1..=4 {
        let run = 1000 + number;
        prs.push(pr(
            number,
            vec![check("drift-fast", "FAILURE", run, run * 10)],
            &[],
        ));
        acts.set(
            run,
            vec![job(
                i64::try_from(run * 10).unwrap(),
                "drift-fast",
                run,
                1,
                &head(number),
                "Install Linux dependencies",
            )],
        );
    }
    let observation = observation(prs);
    let mut ledger = StewardLedger::default();
    let status = run_sweep(&observation, Mode::Enforce, T0, &mut ledger, &acts);
    assert_eq!(actions(&status), vec!["rerun"; 4]);
    assert_eq!(acts.reran.borrow().len(), 4);
    assert_eq!(status.findings.len(), 1);
    let finding = &status.findings[0];
    assert_eq!(
        finding.message,
        "infra outage suspected: drift-fast failing on 4 PRs at step 'Install Linux dependencies'"
    );
    assert!(finding.new);

    // Next pass: the re-runs are queued. Nothing is read or re-run again, and
    // the outage is still the same one finding.
    let rerunning = observation_of((1..=4).map(|number| {
        let run = 1000 + number;
        pr(number, vec![queued("drift-fast", run, run * 10 + 1)], &[])
    }));
    let status = run_sweep(&rerunning, Mode::Enforce, T0 + 120, &mut ledger, &acts);
    assert!(status.outcomes.is_empty());
    assert_eq!(acts.diagnosed.borrow().len(), 4);
    assert_eq!(acts.reran.borrow().len(), 4);
    assert_eq!(status.findings.len(), 1);
    assert!(!status.findings[0].new);

    // The re-runs passed: the finding ends.
    let healed = observation_of((1..=4).map(|number| {
        let run = 1000 + number;
        pr(
            number,
            vec![check("drift-fast", "SUCCESS", run, run * 10 + 1)],
            &[],
        )
    }));
    let status = run_sweep(&healed, Mode::Enforce, T0 + 900, &mut ledger, &acts);
    assert!(status.findings.is_empty());
}

/// The cap: two re-runs per (head, check) at most, the second only after the
/// backoff, then a finding and nothing more, however many passes follow.
#[test]
fn the_cap_holds_across_passes() {
    let acts = FakeActs::default();
    let mut ledger = StewardLedger::default();
    let observe = |check_id: u64| {
        observation(vec![pr(
            1,
            vec![check("drift-fast", "FAILURE", 10, check_id)],
            &[],
        )])
    };
    let attempt = |check_id: u64, attempt: u64| {
        acts.set(
            10,
            vec![job(
                i64::try_from(check_id).unwrap(),
                "drift-fast",
                10,
                attempt,
                &head(1),
                "Install Linux dependencies",
            )],
        );
    };
    attempt(100, 1);
    let status = run_sweep(&observe(100), Mode::Enforce, T0, &mut ledger, &acts);
    assert_eq!(actions(&status), vec!["rerun"]);

    // The re-run failed the same way five minutes later: wait, no read.
    attempt(101, 2);
    let status = run_sweep(&observe(101), Mode::Enforce, T0 + 300, &mut ledger, &acts);
    assert_eq!(actions(&status), vec!["waiting"]);
    assert_eq!(acts.diagnosed.borrow().len(), 1);

    // After the backoff: the second and last re-run.
    let status = run_sweep(
        &observe(101),
        Mode::Enforce,
        T0 + BACKOFF_SECS,
        &mut ledger,
        &acts,
    );
    assert_eq!(actions(&status), vec!["rerun"]);

    // It failed again: capped, reported, and never re-run again.
    attempt(102, 3);
    for pass in 0..5 {
        let now = T0 + BACKOFF_SECS * (3 + pass);
        let status = run_sweep(&observe(102), Mode::Enforce, now, &mut ledger, &acts);
        assert_eq!(actions(&status), vec!["capped"]);
        assert_eq!(status.findings.len(), 1);
        assert!(status.findings[0].capped);
        assert_eq!(
            status.findings[0].message,
            "infra outage suspected: drift-fast failing on 1 PRs at step 'Install Linux dependencies'"
        );
    }
    assert_eq!(acts.reran.borrow().as_slice(), &[10, 10]);
}

/// Control for the cap test: losing the ledger does not buy a third re-run,
/// because GitHub's attempt counter fences it.
#[test]
fn the_cap_survives_a_lost_ledger() {
    let acts = FakeActs::default();
    acts.set(
        10,
        vec![job(
            102,
            "drift-fast",
            10,
            3,
            &head(1),
            "Install Linux dependencies",
        )],
    );
    let observation = observation(vec![pr(
        1,
        vec![check("drift-fast", "FAILURE", 10, 102)],
        &[],
    )]);
    let mut ledger = StewardLedger::default();
    let status = run_sweep(&observation, Mode::Enforce, T0, &mut ledger, &acts);
    assert_eq!(actions(&status), vec!["capped"]);
    assert!(acts.reran.borrow().is_empty());
}

#[test]
fn a_real_failure_is_declined_once_and_never_reread() {
    let acts = FakeActs::default();
    acts.set(
        10,
        vec![job(100, "drift-fast", 10, 1, &head(1), "Run drift tests")],
    );
    acts.logs.borrow_mut().push((
        100,
        "2026-11-11T14:40:01.0000000Z The following tests FAILED:\n\
         2026-11-11T14:40:01.0000000Z \t 12 - census-drift (Failed)\n"
            .to_owned(),
    ));
    let observation = observation(vec![pr(
        1,
        vec![check("drift-fast", "FAILURE", 10, 100)],
        &[],
    )]);
    let mut ledger = StewardLedger::default();
    for pass in 0..3 {
        let status = run_sweep(
            &observation,
            Mode::Enforce,
            T0 + pass * 120,
            &mut ledger,
            &acts,
        );
        assert_eq!(actions(&status), vec!["declined"]);
        assert!(status.findings.is_empty());
    }
    assert!(acts.reran.borrow().is_empty());
    assert_eq!(acts.diagnosed.borrow().len(), 1);
}

#[test]
fn review_verdict_red_is_never_read_or_rerun() {
    let acts = FakeActs::default();
    acts.set(
        10,
        vec![job(
            100,
            "review-verdict",
            10,
            1,
            &head(1),
            "Post review verdict",
        )],
    );
    let observation = observation(vec![pr(
        1,
        vec![check("review-verdict", "FAILURE", 10, 100)],
        &[],
    )]);
    let mut ledger = StewardLedger::default();
    let status = run_sweep(&observation, Mode::Enforce, T0, &mut ledger, &acts);
    assert!(status.outcomes.is_empty());
    assert!(acts.diagnosed.borrow().is_empty());
    assert!(acts.reran.borrow().is_empty());
}

#[test]
fn shadow_reports_and_reruns_nothing() {
    let acts = FakeActs::default();
    acts.set(
        10,
        vec![job(
            100,
            "drift-fast",
            10,
            1,
            &head(1),
            "Install Linux dependencies",
        )],
    );
    let observation = observation(vec![pr(
        1,
        vec![check("drift-fast", "FAILURE", 10, 100)],
        &[],
    )]);
    let mut ledger = StewardLedger::default();
    let status = run_sweep(&observation, Mode::Shadow, T0, &mut ledger, &acts);
    assert_eq!(actions(&status), vec!["would_rerun"]);
    assert!(acts.reran.borrow().is_empty());
    let status = run_sweep(&observation, Mode::Off, T0, &mut ledger, &acts);
    assert!(status.outcomes.is_empty());
    assert_eq!(acts.diagnosed.borrow().len(), 1);
}

#[test]
fn a_refused_rerun_spends_no_budget_and_fails_the_pass() {
    let acts = FakeActs {
        refuse: true,
        ..FakeActs::default()
    };
    acts.set(
        10,
        vec![job(
            100,
            "drift-fast",
            10,
            1,
            &head(1),
            "Install Linux dependencies",
        )],
    );
    let observation = observation(vec![pr(
        1,
        vec![check("drift-fast", "FAILURE", 10, 100)],
        &[],
    )]);
    let mut ledger = StewardLedger::default();
    let (status, failed) = sweep(
        &Sweep {
            observation: &observation,
            mode: Mode::Enforce,
            approval_context: "review-verdict",
            excluded_labels: &[],
            now_epoch: T0,
            deadline: None,
            ledger_path: None,
        },
        &mut ledger,
        &acts,
    );
    assert!(failed);
    assert_eq!(actions(&status), vec!["error"]);
    let record = ledger.infra_reruns.values().next().expect("record");
    assert_eq!(record.reruns, 0);
}

#[test]
fn records_of_closed_or_moved_heads_are_pruned() {
    let mut ledger = StewardLedger::default();
    ledger.infra_reruns.insert(
        record_key("Owner/Repo", 1, &head(1), "drift-fast"),
        Record::default(),
    );
    ledger.infra_reruns.insert(
        record_key("Owner/Repo", 2, &head(2), "drift-fast"),
        Record::default(),
    );
    ledger.infra_reruns.insert(
        record_key("Other/Repo", 3, &head(3), "macos"),
        Record::default(),
    );
    let observation = observation(vec![pr(1, Vec::new(), &[])]);
    let acts = FakeActs::default();
    run_sweep(&observation, Mode::Shadow, T0, &mut ledger, &acts);
    let keys: Vec<&String> = ledger.infra_reruns.keys().collect();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().any(|key| key.starts_with("other/repo#3")));
    assert!(keys.iter().any(|key| key.starts_with("owner/repo#1")));
}

#[test]
fn the_diagnosis_budget_defers_the_rest_of_a_burst() {
    let acts = FakeActs::default();
    let mut prs = Vec::new();
    let count = u64::try_from(MAX_RUN_DIAGNOSES).unwrap() + 2;
    for number in 1..=count {
        let run = 1000 + number;
        prs.push(pr(
            number,
            vec![check("drift-fast", "FAILURE", run, run * 10)],
            &[],
        ));
        acts.set(
            run,
            vec![job(
                i64::try_from(run * 10).unwrap(),
                "drift-fast",
                run,
                1,
                &head(number),
                "Install Linux dependencies",
            )],
        );
    }
    let observation = observation(prs);
    let mut ledger = StewardLedger::default();
    let status = run_sweep(&observation, Mode::Enforce, T0, &mut ledger, &acts);
    let deferred = actions(&status)
        .iter()
        .filter(|action| **action == "deferred")
        .count();
    assert_eq!(deferred, 2);
    assert_eq!(acts.diagnosed.borrow().len(), MAX_RUN_DIAGNOSES);
}

#[test]
fn judge_declines_when_a_non_required_job_failed_for_real_in_the_run() {
    // rerun-failed-jobs would re-run the advisory job's real failure too.
    let jobs = [
        job(100, "drift-fast", 10, 1, HEAD, "Install Linux dependencies"),
        job(101, "linux-advisory", 10, 1, HEAD, "Run tests"),
    ];
    let (doc, jobs) = diagnosed(&jobs, &[]);
    let verdict = judge_run(10, HEAD, &doc, &jobs, "review-verdict", &["drift-fast"]);
    assert!(
        matches!(&verdict, RunVerdict::Decline(why) if why.contains("linux-advisory")),
        "{verdict:?}"
    );
}

#[test]
fn judge_tolerates_cancelled_fail_fast_siblings() {
    let mut sibling = job(101, "linux-advisory", 10, 1, HEAD, "Run tests");
    sibling.conclusion = Some("cancelled".to_owned());
    let jobs = [
        job(100, "drift-fast", 10, 1, HEAD, "Install Linux dependencies"),
        sibling,
    ];
    let (doc, jobs) = diagnosed(&jobs, &[]);
    assert_eq!(
        judge_run(10, HEAD, &doc, &jobs, "review-verdict", &["drift-fast"]),
        RunVerdict::Rerun
    );
}

#[test]
fn judge_declines_a_run_that_contains_an_approval_job_in_any_state() {
    let mut verdict_job = job(101, "review-verdict", 10, 1, HEAD, "Post review verdict");
    verdict_job.conclusion = Some("skipped".to_owned());
    let jobs = [
        job(100, "drift-fast", 10, 1, HEAD, "Install Linux dependencies"),
        verdict_job,
    ];
    let (doc, jobs) = diagnosed(&jobs, &[]);
    assert!(matches!(
        judge_run(10, HEAD, &doc, &jobs, "review-verdict", &["drift-fast"]),
        RunVerdict::Decline(_)
    ));
}

#[test]
fn judge_declines_a_candidate_it_did_not_diagnose() {
    let jobs = [job(
        100,
        "drift-fast",
        10,
        1,
        HEAD,
        "Install Linux dependencies",
    )];
    let (doc, jobs) = diagnosed(&jobs, &[]);
    assert!(matches!(
        judge_run(
            10,
            HEAD,
            &doc,
            &jobs,
            "review-verdict",
            &["drift-fast", "macos"]
        ),
        RunVerdict::Decline(_)
    ));
}

/// An unmanaged pull request: the census read no checks for it, so the sweep
/// reads its rollup (one batched read) and acts on it.
#[test]
fn unmanaged_prs_are_read_and_rerun() {
    let acts = FakeActs::default();
    acts.rollups
        .borrow_mut()
        .insert(head(1), vec![check("drift-fast", "failure", 10, 100)]);
    acts.set(
        10,
        vec![job(
            100,
            "drift-fast",
            10,
            1,
            &head(1),
            "Install Linux dependencies",
        )],
    );
    let observation = observation(vec![pr(1, Vec::new(), &[]), {
        let mut draft = pr(2, Vec::new(), &[]);
        draft.fact.draft = true;
        draft
    }]);
    let mut ledger = StewardLedger::default();
    let status = run_sweep(&observation, Mode::Enforce, T0, &mut ledger, &acts);
    assert_eq!(actions(&status), vec!["rerun"]);
    assert_eq!(*acts.rollup_reads.borrow(), 1);
    assert_eq!(acts.reran.borrow().as_slice(), &[10]);
}

#[test]
fn a_run_still_in_flight_in_the_census_costs_no_read() {
    let acts = FakeActs::default();
    let observation = observation(vec![pr(
        1,
        vec![
            check("drift-fast", "FAILURE", 10, 100),
            queued("macos", 10, 101),
        ],
        &[],
    )]);
    let mut ledger = StewardLedger::default();
    let status = run_sweep(&observation, Mode::Enforce, T0, &mut ledger, &acts);
    assert_eq!(actions(&status), vec!["in_flight"]);
    assert!(acts.diagnosed.borrow().is_empty());
}

#[test]
fn shadow_then_enforce_still_reruns() {
    let acts = FakeActs::default();
    acts.set(
        10,
        vec![job(
            100,
            "drift-fast",
            10,
            1,
            &head(1),
            "Install Linux dependencies",
        )],
    );
    let observation = observation(vec![pr(
        1,
        vec![check("drift-fast", "FAILURE", 10, 100)],
        &[],
    )]);
    let mut ledger = StewardLedger::default();
    let status = run_sweep(&observation, Mode::Shadow, T0, &mut ledger, &acts);
    assert_eq!(actions(&status), vec!["would_rerun"]);
    let status = run_sweep(&observation, Mode::Shadow, T0 + 120, &mut ledger, &acts);
    assert_eq!(actions(&status), vec!["pending"]);
    let status = run_sweep(&observation, Mode::Enforce, T0 + 240, &mut ledger, &acts);
    assert_eq!(actions(&status), vec!["rerun"]);
    assert_eq!(acts.reran.borrow().as_slice(), &[10]);
}

/// After the cap, a red that is a real failure is declined, not reported as
/// an infra outage.
#[test]
fn a_real_failure_after_the_cap_is_not_an_outage() {
    let acts = FakeActs::default();
    acts.set(
        10,
        vec![job(102, "drift-fast", 10, 3, &head(1), "Run drift tests")],
    );
    acts.logs.borrow_mut().push((
        102,
        "2026-11-11T14:40:01.0000000Z The following tests FAILED:\n\
         2026-11-11T14:40:01.0000000Z \t 12 - census-drift (Failed)\n"
            .to_owned(),
    ));
    let mut ledger = StewardLedger::default();
    ledger.infra_reruns.insert(
        record_key("Owner/Repo", 1, &head(1), "drift-fast"),
        Record {
            pr: 1,
            check: "drift-fast".to_owned(),
            reruns: 2,
            last_rerun_epoch: Some(T0),
            judged_check_id: Some(101),
            verdict: "rerun".to_owned(),
            step: "Install Linux dependencies".to_owned(),
            ..Record::default()
        },
    );
    let observation = observation(vec![pr(
        1,
        vec![check("drift-fast", "FAILURE", 10, 102)],
        &[],
    )]);
    let status = run_sweep(
        &observation,
        Mode::Enforce,
        T0 + BACKOFF_SECS,
        &mut ledger,
        &acts,
    );
    assert_eq!(actions(&status), vec!["declined"]);
    assert!(status.findings.is_empty());
    assert!(acts.reran.borrow().is_empty());
}

#[test]
fn a_rate_limited_diagnosis_stops_the_sweeps_reads() {
    struct Limited(RefCell<usize>);
    impl Acts for Limited {
        fn rollups(&self, _: &[String]) -> Result<BTreeMap<String, Vec<StewardCheck>>, String> {
            Ok(BTreeMap::new())
        }
        fn diagnose(&self, _: u64) -> Result<(Diagnosis, Vec<Job>), String> {
            *self.0.borrow_mut() += 1;
            Err("API rate limit exceeded for installation".to_owned())
        }
        fn rerun_failed_jobs(&self, _: u64) -> Result<(), String> {
            panic!("no re-run without a diagnosis")
        }
    }
    let observation = observation(vec![
        pr(1, vec![check("drift-fast", "FAILURE", 10, 100)], &[]),
        pr(2, vec![check("drift-fast", "FAILURE", 11, 110)], &[]),
    ]);
    let acts = Limited(RefCell::new(0));
    let mut ledger = StewardLedger::default();
    let (status, _) = sweep(
        &Sweep {
            observation: &observation,
            mode: Mode::Enforce,
            approval_context: "review-verdict",
            excluded_labels: &[],
            now_epoch: T0,
            deadline: None,
            ledger_path: None,
        },
        &mut ledger,
        &acts,
    );
    assert_eq!(*acts.0.borrow(), 1);
    assert_eq!(actions(&status), vec!["error", "deferred"]);
}

// ---- Back-test: replay recorded failures through the same classifier ----

/// Replays every sample under `$INFRA_RERUN_BACKTEST_DIR/samples` (written by
/// the fleet back-test collector) through `diagnose_gathered` and
/// [`judge_run`], printing one JSON line per failing required job to
/// `$INFRA_RERUN_BACKTEST_OUT`. Ignored by default: it needs recorded data.
#[test]
#[ignore = "needs a recorded back-test corpus"]
fn backtest_replay() {
    use std::io::Write as _;
    let Ok(dir) = std::env::var("INFRA_RERUN_BACKTEST_DIR") else {
        return;
    };
    let out = std::env::var("INFRA_RERUN_BACKTEST_OUT").expect("INFRA_RERUN_BACKTEST_OUT");
    let mut sink = std::fs::File::create(out).expect("output");
    let mut entries: Vec<_> = std::fs::read_dir(std::path::Path::new(&dir).join("samples"))
        .expect("samples")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        let text = std::fs::read_to_string(&path).expect("sample");
        let sample: serde_json::Value = serde_json::from_str(&text).expect("json");
        let jobs: Vec<Job> = serde_json::from_value(sample["jobs"].clone()).expect("jobs");
        let required: Vec<String> =
            serde_json::from_value(sample["required"].clone()).expect("required");
        let mut with_approval = required.clone();
        with_approval.push("review-verdict".to_owned());
        let logs: HashMap<i64, String> = sample["logs"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(id, text)| Some((id.parse().ok()?, text.as_str()?.to_owned())))
            .collect();
        let annotations: HashMap<i64, Vec<String>> = sample["annotations"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(id, notes)| {
                Some((
                    id.parse().ok()?,
                    serde_json::from_value(notes.clone()).ok()?,
                ))
            })
            .collect();
        let doc = diagnose_gathered(&jobs, &logs, annotations, &with_approval);
        let run_id = sample["run_id"].as_u64().unwrap_or_default();
        let head = sample["head_sha"].as_str().unwrap_or_default();
        let failing: Vec<&str> = doc
            .checks
            .iter()
            .filter(|check| check.conclusion.as_deref() == Some("failure"))
            .map(|check| check.context.as_str())
            .collect();
        let verdict = judge_run(run_id, head, &doc, &jobs, "review-verdict", &failing);
        for check in &doc.checks {
            let line = serde_json::json!({
                "repo": sample["repo"],
                "run_id": run_id,
                "attempt": sample["attempt"],
                "job": check.context,
                "job_id": check.job_id,
                "conclusion": check.conclusion,
                "class": check.classification.class,
                "rule": check.classification.rule,
                "why": check.classification.why,
                "step": check.failing_step.as_ref().map(|step| step.name.clone()),
                "run_verdict": format!("{verdict:?}"),
            });
            writeln!(sink, "{line}").expect("write");
        }
    }
}
