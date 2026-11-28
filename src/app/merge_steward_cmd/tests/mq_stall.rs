//! Jobs GitHub never gave a runner, on two recorded Generous-Corp/vellum
//! shapes (read back with `ghread`, trimmed to the fields the stall reads):
//!
//! * `mq_stall_vellum_825.json`, 2026-11-28T04:55Z: the queue head #825's
//!   run 46104476461 had job 140854026289 queued with runner id 0 since
//!   03:46:57 while job 140854029336 of #769's run 46104477453 (created the
//!   same second) started on `studio-vellum-gate-slot2-02-59395-93` at
//!   03:51:50;
//! * `mq_stall_vellum_pr_jobs.json`, 09:29Z: seven approved pull requests'
//!   `gpu-macos-arm64` jobs queued from 05:12Z while job 140898811001 (run
//!   46118904930, created 06:56:33) started at 09:05:16 and completed.
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::*;
use crate::app::merge_steward_cmd::mq_stall::{
    self, Acts, Detection, Entry, Job, Kind, Receipt, RunState,
};
use crate::merge_steward::mq_stall::{MqStallMode, MqStallPolicy};

const REPO: &str = "Generous-Corp/vellum";
const G825: &str = "430728e9c997d53d71f6255dc26a3c76ea076131";
const G769: &str = "14817233736013cf383caf8480bac78d6cb5e7a0";
const G791: &str = "a616c7b8a01fbee43478d69b51ceea89cad603b1";
const RUN_825: u64 = 46_104_476_461;
const JOB_825: u64 = 140_854_026_289;
const RUN_769: u64 = 46_104_477_453;
const JOB_769: u64 = 140_854_029_336;
const WITNESS_PR_RUN: u64 = 46_118_904_930;
const WITNESS_PR_JOB: u64 = 140_898_811_001;

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("time")
        .with_timezone(&Utc)
}

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/app/merge_steward_cmd/tests/fixtures")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

/// A recorded shape: the census, the workflow listing and each run's jobs,
/// editable before the fake GitHub answers from it.
struct Shape {
    now: DateTime<Utc>,
    census: Vec<StewardRun>,
    workflow_runs: Vec<Value>,
    jobs: BTreeMap<u64, Vec<Value>>,
    /// Paths that answer with an error.
    failing: Vec<String>,
    /// Paths read, in order.
    reads: RefCell<Vec<String>>,
}

impl Shape {
    fn load(name: &str) -> Self {
        let value = fixture(name);
        Self {
            now: at(value["now"].as_str().expect("now")),
            census: value["census"]
                .as_array()
                .expect("census")
                .iter()
                .map(|row| crate::app::merge_steward_cmd::observation::parse_run(row).expect("run"))
                .collect(),
            workflow_runs: value["workflow_runs"].as_array().expect("runs").clone(),
            jobs: value["jobs"]
                .as_object()
                .expect("jobs")
                .iter()
                .map(|(id, jobs)| {
                    (
                        id.parse().expect("run id"),
                        jobs.as_array().expect("jobs").clone(),
                    )
                })
                .collect(),
            failing: Vec::new(),
            reads: RefCell::default(),
        }
    }

    fn job_mut(&mut self, job_id: u64) -> &mut Value {
        self.jobs
            .values_mut()
            .flatten()
            .find(|job| job["id"].as_u64() == Some(job_id))
            .expect("job")
    }

    fn gh(&self, argv: &[String]) -> Result<String, String> {
        let path = argv.last().cloned().unwrap_or_default();
        self.reads.borrow_mut().push(path.clone());
        if self.failing.iter().any(|failing| path.contains(failing)) {
            return Err("HTTP 502".to_owned());
        }
        if path.contains("/actions/workflows/") {
            return Ok(serde_json::json!({
                "total_count": self.workflow_runs.len(),
                "workflow_runs": self.workflow_runs,
            })
            .to_string());
        }
        let run_id = path
            .split("/actions/runs/")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .and_then(|id| id.parse::<u64>().ok())
            .ok_or_else(|| format!("unexpected read {path}"))?;
        let jobs = self.jobs.get(&run_id).cloned().unwrap_or_default();
        Ok(serde_json::json!({"total_count": jobs.len(), "jobs": jobs}).to_string())
    }

    fn read_jobs_of(&self, run_id: u64) -> bool {
        self.reads
            .borrow()
            .iter()
            .any(|path| path.contains(&format!("/actions/runs/{run_id}/jobs")))
    }
}

/// An open pull request; `approved` puts a green `review-verdict` on its
/// head, `gpu_run` a queued required `gpu-macos-arm64` check from that run.
fn pr(
    number: u64,
    head: &str,
    position: Option<u64>,
    approved: bool,
    gpu_run: Option<u64>,
) -> ObservedPr {
    let mut rollup = Vec::new();
    if approved {
        rollup.push(serde_json::json!({
            "__typename": "CheckRun", "name": "review-verdict", "status": "COMPLETED",
            "conclusion": "SUCCESS", "completedAt": "2026-11-28T05:00:00Z",
        }));
    }
    if let Some(run) = gpu_run {
        rollup.push(serde_json::json!({
            "__typename": "CheckRun", "name": "gpu-macos-arm64", "status": "QUEUED",
            "conclusion": null,
            "detailsUrl": format!("https://github.com/{REPO}/actions/runs/{run}/job/1"),
        }));
    }
    let positions = position
        .map(|position| BTreeMap::from([(number, position)]))
        .unwrap_or_default();
    parse_pr(
        &serde_json::json!({
            "id": format!("PR_{number}"),
            "number": number,
            "state": "OPEN",
            "isDraft": false,
            "baseRefName": "main",
            "headRefOid": head,
            "headRefName": format!("branch-{number}"),
            "mergeStateStatus": "BLOCKED",
            "autoMergeRequest": null,
            "labels": [],
            "statusCheckRollup": rollup,
        }),
        &positions,
    )
    .expect("pr")
}

fn required() -> Vec<RequiredCheck> {
    ["gpu-macos-arm64", "review-verdict"]
        .iter()
        .map(|context| RequiredCheck {
            context: (*context).to_owned(),
            app_id: None,
        })
        .collect()
}

fn observation(prs: Vec<ObservedPr>, runs: Vec<StewardRun>) -> RepoObservation {
    let mut observation = observation_for(prs[0].clone(), true);
    observation.repo = REPO.to_owned();
    observation.required_checks = required();
    observation.prs = prs;
    observation.runs = runs;
    observation
}

/// The 04:55Z queue: #825 head, #769, #791.
fn queue_825(shape: &Shape) -> RepoObservation {
    let mut observation = observation(
        vec![
            pr(825, &"8".repeat(40), Some(1), true, None),
            pr(769, &"7".repeat(40), Some(2), true, None),
            pr(791, &"9".repeat(40), Some(3), true, None),
        ],
        shape.census.clone(),
    );
    observation.merge_group_heads = BTreeMap::from([
        (825, G825.to_owned()),
        (769, G769.to_owned()),
        (791, G791.to_owned()),
    ]);
    observation
}

fn detect(shape: &Shape, observation: &RepoObservation, now: DateTime<Utc>) -> Detection {
    mq_stall::detect(
        &|argv: &[String]| shape.gh(argv),
        observation,
        crate::merge_steward::DEFAULT_APPROVAL_CONTEXT,
        20,
        now,
    )
}

#[test]
fn the_825_shape_is_a_queue_head_stall_with_the_769_witness() {
    let shape = Shape::load("mq_stall_vellum_825.json");
    let detection = detect(&shape, &queue_825(&shape), shape.now);
    assert!(detection.unread.is_none(), "{detection:?}");
    assert_eq!(detection.stalls.len(), 1, "{detection:?}");
    let stall = &detection.stalls[0];
    assert_eq!(stall.kind, Kind::MergeQueue);
    assert_eq!(
        (stall.pr, stall.run_id, stall.job_id),
        (825, RUN_825, JOB_825)
    );
    assert_eq!(
        (stall.witness_run_id, stall.witness_job_id),
        (RUN_769, JOB_769)
    );
    assert_eq!(
        stall.witness_runner.as_deref(),
        Some("studio-vellum-gate-slot2-02-59395-93")
    );
    assert_eq!(stall.token(), "#825:run:46104476461:job:140854026289:68m");
    // Jobs are read once per pass: the stalled run, the listing, the witness.
    assert_eq!(detection.reads, 3, "{:?}", shape.reads.borrow());
}

#[test]
fn a_full_fleet_with_no_newer_started_job_is_not_a_stall() {
    let mut shape = Shape::load("mq_stall_vellum_825.json");
    // Nothing newer with these labels got a runner.
    for job in [JOB_769, 140_859_201_898] {
        let job = shape.job_mut(job);
        job["status"] = "queued".into();
        job["conclusion"] = Value::Null;
        job["runner_id"] = 0.into();
        job["runner_name"] = "".into();
    }
    let detection = detect(&shape, &queue_825(&shape), shape.now);
    assert!(detection.stalls.is_empty(), "{detection:?}");
    assert!(
        detection
            .notes
            .iter()
            .any(|note| note.contains("no newer job with its labels")),
        "{detection:?}"
    );
}

#[test]
fn a_witness_with_fewer_labels_does_not_count() {
    let mut shape = Shape::load("mq_stall_vellum_825.json");
    for job in [JOB_769, 140_859_201_898] {
        shape.job_mut(job)["labels"] = serde_json::json!(["self-hosted", "macOS", "ARM64"]);
    }
    assert!(
        detect(&shape, &queue_825(&shape), shape.now)
            .stalls
            .is_empty()
    );
}

#[test]
fn a_non_head_entry_is_never_judged() {
    let shape = Shape::load("mq_stall_vellum_825.json");
    let mut observation = queue_825(&shape);
    // #769 is the head now; #825's stalled run belongs to a non-head entry.
    for pr in &mut observation.prs {
        pr.fact.queue_position = match pr.fact.number {
            769 => Some(1),
            791 => Some(2),
            _ => Some(3),
        };
    }
    let detection = detect(&shape, &observation, shape.now);
    assert!(detection.stalls.is_empty(), "{detection:?}");
    assert!(!shape.read_jobs_of(RUN_825), "{:?}", shape.reads.borrow());
}

#[test]
fn a_job_with_a_runner_is_never_a_stall() {
    let mut shape = Shape::load("mq_stall_vellum_825.json");
    shape.job_mut(JOB_825)["runner_id"] = 35_831.into();
    assert!(
        detect(&shape, &queue_825(&shape), shape.now)
            .stalls
            .is_empty()
    );
    let mut named = Shape::load("mq_stall_vellum_825.json");
    named.job_mut(JOB_825)["runner_name"] = "studio-vellum-gate-01".into();
    assert!(
        detect(&named, &queue_825(&named), named.now)
            .stalls
            .is_empty()
    );
}

#[test]
fn under_the_threshold_nothing_is_read() {
    let shape = Shape::load("mq_stall_vellum_825.json");
    let detection = detect(&shape, &queue_825(&shape), at("2026-11-28T04:06:00Z"));
    assert!(detection.stalls.is_empty(), "{detection:?}");
    assert!(
        shape.reads.borrow().is_empty(),
        "{:?}",
        shape.reads.borrow()
    );
    // The run is old enough but the job is not: read, not a stall.
    let mut young = Shape::load("mq_stall_vellum_825.json");
    young.job_mut(JOB_825)["created_at"] = "2026-11-28T04:40:00Z".into();
    assert!(
        detect(&young, &queue_825(&young), young.now)
            .stalls
            .is_empty()
    );
}

#[test]
fn another_job_of_the_run_in_progress_holds_the_verdict() {
    let mut shape = Shape::load("mq_stall_vellum_825.json");
    shape.job_mut(140_854_026_055)["status"] = "in_progress".into();
    let detection = detect(&shape, &queue_825(&shape), shape.now);
    assert!(detection.stalls.is_empty(), "{detection:?}");
}

#[test]
fn an_incomplete_read_decides_nothing() {
    for failing in [
        format!("/actions/runs/{RUN_825}/jobs"),
        "/actions/workflows/".to_owned(),
        format!("/actions/runs/{RUN_769}/jobs"),
    ] {
        let mut shape = Shape::load("mq_stall_vellum_825.json");
        shape.failing.push(failing.clone());
        let detection = detect(&shape, &queue_825(&shape), shape.now);
        assert!(detection.stalls.is_empty(), "{failing}: {detection:?}");
        assert!(detection.unread.is_some(), "{failing}: {detection:?}");
    }
    // A page that may not hold every job is refused like a failed read.
    let partial = |argv: &[String]| -> Result<String, String> {
        let path = argv.last().cloned().unwrap_or_default();
        if path.contains(&format!("/actions/runs/{RUN_825}/jobs")) {
            return Ok(r#"{"total_count":120,"jobs":[]}"#.to_owned());
        }
        Ok(r#"{"total_count":0,"jobs":[],"workflow_runs":[]}"#.to_owned())
    };
    let shape = Shape::load("mq_stall_vellum_825.json");
    let detection = mq_stall::detect(
        &partial,
        &queue_825(&shape),
        "review-verdict",
        20,
        shape.now,
    );
    assert!(detection.unread.is_some(), "{detection:?}");
}

/// Approved PR heads of the 09:29Z shape: each with its queued GPU run.
fn pr_jobs(shape: &Shape) -> RepoObservation {
    let prs = shape
        .census
        .iter()
        .map(|run| {
            pr(
                run.pull_request_number.expect("pr"),
                &run.head_sha,
                None,
                true,
                Some(run.id),
            )
        })
        .collect::<Vec<_>>();
    observation(prs, shape.census.clone())
}

#[test]
fn approved_pr_heads_with_old_queued_gpu_jobs_are_pr_stalls() {
    let shape = Shape::load("mq_stall_vellum_pr_jobs.json");
    let detection = detect(&shape, &pr_jobs(&shape), shape.now);
    assert!(detection.unread.is_none(), "{detection:?}");
    let stalled = detection
        .stalls
        .iter()
        .map(|stall| stall.pr)
        .collect::<Vec<_>>();
    // Created before the 06:56:33 witness and over 20 minutes old.
    assert_eq!(stalled, vec![842, 843, 844, 833], "{detection:?}");
    assert!(
        detection
            .stalls
            .iter()
            .all(|stall| stall.kind == Kind::PullRequest
                && stall.witness_run_id == WITNESS_PR_RUN
                && stall.witness_job_id == WITNESS_PR_JOB)
    );
    // The witness run had completed: only the workflow listing names it.
    assert!(!shape.census.iter().any(|run| run.id == WITNESS_PR_RUN));
    assert_eq!(
        detection.stalls[0].token(),
        "#842:run:46111057074:job:140874457840:257m"
    );
}

#[test]
fn an_unapproved_or_draft_pr_is_not_a_target() {
    let shape = Shape::load("mq_stall_vellum_pr_jobs.json");
    let mut observation = pr_jobs(&shape);
    observation.prs = observation
        .prs
        .iter()
        .map(|row| pr(row.fact.number, &row.fact.head_sha, None, false, Some(0)))
        .collect();
    let detection = detect(&shape, &observation, shape.now);
    assert!(detection.stalls.is_empty() && shape.reads.borrow().is_empty());
    let mut drafts = pr_jobs(&shape);
    for row in &mut drafts.prs {
        row.fact.draft = true;
    }
    assert!(detect(&shape, &drafts, shape.now).stalls.is_empty());
}

#[test]
fn a_pr_job_that_produces_no_required_check_is_not_a_stall() {
    let shape = Shape::load("mq_stall_vellum_pr_jobs.json");
    let mut observation = pr_jobs(&shape);
    observation.required_checks = vec![RequiredCheck {
        context: "review-verdict".to_owned(),
        app_id: None,
    }];
    observation.prs = observation
        .prs
        .iter()
        .map(|row| pr(row.fact.number, &row.fact.head_sha, None, true, None))
        .collect();
    let detection = detect(&shape, &observation, shape.now);
    assert!(detection.stalls.is_empty(), "{detection:?}");
}

struct FakeActs {
    entry: RefCell<Result<Entry, String>>,
    job: RefCell<Option<Result<Job, String>>>,
    cancel: Result<(), String>,
    run: RefCell<Result<RunState, String>>,
    cancels: RefCell<Vec<u64>>,
    reruns: RefCell<Vec<u64>>,
}

impl FakeActs {
    fn ok(entry: Entry) -> Self {
        Self {
            entry: RefCell::new(Ok(entry)),
            job: RefCell::new(None),
            cancel: Ok(()),
            run: RefCell::new(Ok(cancelled_run(1))),
            cancels: RefCell::default(),
            reruns: RefCell::default(),
        }
    }

    fn head_825() -> Self {
        Self::ok(Entry {
            head: Some("8".repeat(40)),
            position: Some(1),
            group: Some(G825.to_owned()),
        })
    }
}

fn cancelled_run(attempt: u64) -> RunState {
    RunState {
        status: "completed".to_owned(),
        conclusion: Some("cancelled".to_owned()),
        run_attempt: attempt,
    }
}

fn waiting_job(id: u64) -> Job {
    Job {
        id,
        run_id: RUN_825,
        name: "gpu-macos-arm64".to_owned(),
        status: "queued".to_owned(),
        created_at: Some(at("2026-11-28T03:46:57Z")),
        started_at: Some(at("2026-11-28T03:46:57Z")),
        runner_id: Some(0),
        runner_name: Some(String::new()),
        labels: Vec::new(),
    }
}

impl Acts for FakeActs {
    fn reread_entry(&self, _pr: u64) -> Result<Entry, String> {
        self.entry.borrow().clone()
    }
    fn reread_job(&self, job_id: u64) -> Result<Job, String> {
        self.job
            .borrow()
            .clone()
            .unwrap_or_else(|| Ok(waiting_job(job_id)))
    }
    fn cancel(&self, run: &StewardRun) -> Result<(), String> {
        self.cancels.borrow_mut().push(run.id);
        self.cancel.clone()
    }
    fn read_run(&self, _run_id: u64) -> Result<RunState, String> {
        self.run.borrow().clone()
    }
    fn rerun(&self, run: &StewardRun) -> Result<(), String> {
        self.reruns.borrow_mut().push(run.id);
        Ok(())
    }
}

fn policy(mode: MqStallMode, pr_mode: MqStallMode) -> MqStallPolicy {
    MqStallPolicy {
        mode,
        pr_mode,
        ..MqStallPolicy::default()
    }
}

fn act(
    detection: &Detection,
    policy: &MqStallPolicy,
    applying: bool,
    observation: &RepoObservation,
    ledger: &mut StewardLedger,
    acts: &FakeActs,
    now: DateTime<Utc>,
) -> Vec<mq_stall::Action> {
    mq_stall::act(
        detection,
        policy,
        applying,
        &BTreeSet::new(),
        observation,
        ledger,
        acts,
        now,
    )
}

fn stalled_825() -> (Shape, RepoObservation, Detection) {
    let shape = Shape::load("mq_stall_vellum_825.json");
    let observation = queue_825(&shape);
    let detection = detect(&shape, &observation, shape.now);
    assert_eq!(detection.stalls.len(), 1);
    (shape, observation, detection)
}

#[test]
fn off_does_nothing_and_shadow_reports_would_cancel_only() {
    let (shape, observation, detection) = stalled_825();
    let acts = FakeActs::head_825();
    let mut ledger = StewardLedger::default();
    let off = act(
        &detection,
        &MqStallPolicy::default(),
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert!(off.is_empty());
    for (mode, applying) in [(MqStallMode::Shadow, true), (MqStallMode::Apply, false)] {
        let actions = act(
            &detection,
            &policy(mode, MqStallMode::Off),
            applying,
            &observation,
            &mut ledger,
            &acts,
            shape.now,
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].outcome, "would_cancel", "{mode:?} {applying}");
    }
    assert!(acts.cancels.borrow().is_empty());
    assert!(ledger.mq_stall_cancels.is_empty());
}

#[test]
fn apply_cancels_the_run_once_and_records_it() {
    let (shape, observation, detection) = stalled_825();
    let acts = FakeActs::head_825();
    let mut ledger = StewardLedger::default();
    let apply = policy(MqStallMode::Apply, MqStallMode::Off);
    let actions = act(
        &detection,
        &apply,
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert_eq!(actions[0].outcome, "cancelled", "{actions:?}");
    assert_eq!(*acts.cancels.borrow(), vec![RUN_825]);
    let receipt = ledger
        .mq_stall_cancels
        .get(&mq_stall::receipt_key(REPO, RUN_825))
        .expect("receipt");
    assert_eq!(
        (receipt.pr, receipt.job_id, receipt.outcome.as_str()),
        (825, JOB_825, "cancelled")
    );
    assert!(receipt.progress.is_none());
    // Once per run id, even an hour later.
    let again = act(
        &detection,
        &apply,
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now + chrono::Duration::hours(2),
    );
    assert_eq!(again[0].outcome, "already_attempted");
    assert_eq!(acts.cancels.borrow().len(), 1);
}

#[test]
fn a_moved_entry_or_a_job_that_got_a_runner_refuses_the_cancel() {
    let (shape, observation, detection) = stalled_825();
    let apply = policy(MqStallMode::Apply, MqStallMode::Off);
    let moved = FakeActs::ok(Entry {
        head: None,
        position: Some(2),
        group: Some(G825.to_owned()),
    });
    let mut ledger = StewardLedger::default();
    let actions = act(
        &detection,
        &apply,
        true,
        &observation,
        &mut ledger,
        &moved,
        shape.now,
    );
    assert_eq!(actions[0].outcome, "refused", "{actions:?}");
    let regrouped = FakeActs::ok(Entry {
        head: None,
        position: Some(1),
        group: Some(G769.to_owned()),
    });
    assert_eq!(
        act(
            &detection,
            &apply,
            true,
            &observation,
            &mut ledger,
            &regrouped,
            shape.now
        )[0]
        .outcome,
        "refused"
    );
    let served = FakeActs::head_825();
    let mut job = waiting_job(JOB_825);
    job.runner_id = Some(35_831);
    *served.job.borrow_mut() = Some(Ok(job));
    assert_eq!(
        act(
            &detection,
            &apply,
            true,
            &observation,
            &mut ledger,
            &served,
            shape.now
        )[0]
        .outcome,
        "refused"
    );
    let unread = FakeActs::head_825();
    *unread.entry.borrow_mut() = Err("HTTP 502".to_owned());
    assert_eq!(
        act(
            &detection,
            &apply,
            true,
            &observation,
            &mut ledger,
            &unread,
            shape.now
        )[0]
        .outcome,
        "refused"
    );
    assert!(moved.cancels.borrow().is_empty() && regrouped.cancels.borrow().is_empty());
    assert!(served.cancels.borrow().is_empty() && unread.cancels.borrow().is_empty());
    assert!(ledger.mq_stall_cancels.is_empty());
}

#[test]
fn an_unread_detection_never_acts() {
    let (shape, observation, mut detection) = stalled_825();
    detection.unread = Some("witness run jobs: HTTP 502".to_owned());
    let acts = FakeActs::head_825();
    let mut ledger = StewardLedger::default();
    let apply = policy(MqStallMode::Apply, MqStallMode::Apply);
    assert!(
        act(
            &detection,
            &apply,
            true,
            &observation,
            &mut ledger,
            &acts,
            shape.now
        )
        .is_empty()
    );
    assert!(acts.cancels.borrow().is_empty());
}

#[test]
fn the_hourly_cap_and_one_cancel_per_pass_hold() {
    let shape = Shape::load("mq_stall_vellum_pr_jobs.json");
    let observation = pr_jobs(&shape);
    let detection = detect(&shape, &observation, shape.now);
    assert_eq!(detection.stalls.len(), 4);
    let acts = FakeActs::ok(Entry::default());
    let apply = policy(MqStallMode::Off, MqStallMode::Apply);
    let mut ledger = StewardLedger::default();
    let first = mq_stall::act(
        &detection,
        &apply,
        true,
        &BTreeSet::new(),
        &observation,
        &mut ledger,
        &HeadActs(&acts, &observation),
        shape.now,
    );
    let outcomes = first.iter().map(|a| a.outcome.as_str()).collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        vec!["cancelled", "deferred", "deferred", "deferred"]
    );
    assert_eq!(acts.cancels.borrow().len(), 1);
    // Two more passes spend the hour's cap of three; the fourth is refused.
    for minutes in [5, 10] {
        let next = mq_stall::act(
            &detection,
            &apply,
            true,
            &BTreeSet::new(),
            &observation,
            &mut ledger,
            &HeadActs(&acts, &observation),
            shape.now + chrono::Duration::minutes(minutes),
        );
        assert!(
            next.iter().filter(|a| a.outcome == "cancelled").count() == 1,
            "{next:?}"
        );
    }
    let capped = mq_stall::act(
        &detection,
        &apply,
        true,
        &BTreeSet::new(),
        &observation,
        &mut ledger,
        &HeadActs(&acts, &observation),
        shape.now + chrono::Duration::minutes(15),
    );
    let refused = capped
        .iter()
        .find(|a| a.outcome == "refused")
        .expect("refused");
    assert!(
        refused
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("cap 3"),
        "{capped:?}"
    );
    assert_eq!(acts.cancels.borrow().len(), 3);
    // An hour after the first, the cap has room again.
    let later = mq_stall::act(
        &detection,
        &apply,
        true,
        &BTreeSet::new(),
        &observation,
        &mut ledger,
        &HeadActs(&acts, &observation),
        shape.now + chrono::Duration::minutes(61),
    );
    assert!(later.iter().any(|a| a.outcome == "cancelled"), "{later:?}");
}

/// The fake acts, re-reading each pull request at its observed head.
struct HeadActs<'a>(&'a FakeActs, &'a RepoObservation);

impl Acts for HeadActs<'_> {
    fn reread_entry(&self, pr: u64) -> Result<Entry, String> {
        Ok(Entry {
            head: self
                .1
                .prs
                .iter()
                .find(|row| row.fact.number == pr)
                .map(|row| row.fact.head_sha.clone()),
            position: None,
            group: None,
        })
    }
    fn reread_job(&self, job_id: u64) -> Result<Job, String> {
        self.0.reread_job(job_id)
    }
    fn cancel(&self, run: &StewardRun) -> Result<(), String> {
        self.0.cancel(run)
    }
    fn read_run(&self, run_id: u64) -> Result<RunState, String> {
        self.0.read_run(run_id)
    }
    fn rerun(&self, run: &StewardRun) -> Result<(), String> {
        self.0.rerun(run)
    }
}

#[test]
fn an_opted_out_pr_is_refused() {
    let (shape, observation, detection) = stalled_825();
    let acts = FakeActs::head_825();
    let mut ledger = StewardLedger::default();
    let actions = mq_stall::act(
        &detection,
        &policy(MqStallMode::Apply, MqStallMode::Off),
        true,
        &BTreeSet::from([825]),
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert_eq!(actions[0].outcome, "refused");
    assert!(acts.cancels.borrow().is_empty());
}

/// Cancel #825's run, then return the ledger and the cancel time.
fn cancelled_825() -> (RepoObservation, StewardLedger, DateTime<Utc>) {
    let (shape, observation, detection) = stalled_825();
    let mut ledger = StewardLedger::default();
    let actions = act(
        &detection,
        &policy(MqStallMode::Apply, MqStallMode::Off),
        true,
        &observation,
        &mut ledger,
        &FakeActs::head_825(),
        shape.now,
    );
    assert_eq!(actions[0].outcome, "cancelled");
    (observation, ledger, shape.now)
}

fn receipt(ledger: &StewardLedger, run_id: u64) -> Receipt {
    ledger
        .mq_stall_cancels
        .get(&mq_stall::receipt_key(REPO, run_id))
        .expect("receipt")
        .clone()
}

#[test]
fn the_progress_read_waits_a_pass_then_proves_a_re_enqueue() {
    let (mut observation, mut ledger, cancelled) = cancelled_825();
    let acts = FakeActs::head_825();
    // Not on the cancelling pass.
    let (none, lines) = mq_stall::progress(&acts, &observation, &[], &mut ledger, cancelled);
    assert!(none.is_empty() && lines.is_empty());
    assert!(receipt(&ledger, RUN_825).progress.is_none());
    // Next pass: the run is cancelled and #825 is back on a new group.
    observation.merge_group_heads.insert(825, "b".repeat(40));
    let next = cancelled + chrono::Duration::minutes(5);
    let (unrecovered, lines) = mq_stall::progress(&acts, &observation, &[], &mut ledger, next);
    assert!(unrecovered.is_empty(), "{lines:?}");
    let proved = receipt(&ledger, RUN_825);
    assert!(
        proved
            .progress
            .as_deref()
            .unwrap_or_default()
            .starts_with("recovered: re-enqueued on group"),
        "{proved:?}"
    );
}

#[test]
fn an_enqueue_this_pass_or_a_fresh_group_run_is_progress() {
    let (mut observation, mut ledger, cancelled) = cancelled_825();
    let acts = FakeActs::head_825();
    observation.prs.retain(|row| row.fact.number != 825);
    observation
        .prs
        .push(pr(825, &"8".repeat(40), None, true, None));
    let report = PrReport {
        number: 825,
        head_sha: "8".repeat(40),
        decision: StewardDecision::ArmMergeQueue,
        mutation: Some("enqueued".to_owned()),
        error: None,
        blocked: crate::app::merge_steward_cmd::blocked_on::Stall::default(),
        required_checks: Vec::new(),
    };
    let next = cancelled + chrono::Duration::minutes(5);
    mq_stall::progress(&acts, &observation, &[report], &mut ledger, next);
    assert_eq!(
        receipt(&ledger, RUN_825).progress.as_deref(),
        Some("recovered: re-enqueued this pass")
    );
    let (mut observation, mut ledger, cancelled) = cancelled_825();
    let mut fresh = observation.runs[0].clone();
    fresh.id = RUN_825 + 10;
    fresh.created_at = (cancelled + chrono::Duration::minutes(1)).to_rfc3339();
    observation.runs.push(fresh);
    mq_stall::progress(
        &acts,
        &observation,
        &[],
        &mut ledger,
        cancelled + chrono::Duration::minutes(5),
    );
    assert!(
        receipt(&ledger, RUN_825)
            .progress
            .as_deref()
            .unwrap_or_default()
            .starts_with("recovered: fresh merge_group run"),
    );
}

#[test]
fn no_movement_is_unrecovered_and_the_run_is_never_retried() {
    let (shape, observation, detection) = stalled_825();
    let (_, mut ledger, cancelled) = cancelled_825();
    let acts = FakeActs::head_825();
    // Still queued on the same group, no fresh run, nothing enqueued: pending
    // for the window (#825 took 37 minutes to be re-enqueued), then not.
    let early = cancelled + chrono::Duration::minutes(40);
    let (none, lines) = mq_stall::progress(&acts, &observation, &[], &mut ledger, early);
    assert!(none.is_empty() && lines[0].contains("pending"), "{lines:?}");
    assert!(receipt(&ledger, RUN_825).progress.is_none());
    let next = cancelled + chrono::Duration::minutes(61);
    let (unrecovered, _) = mq_stall::progress(&acts, &observation, &[], &mut ledger, next);
    assert_eq!(
        unrecovered,
        vec![(Kind::MergeQueue, "#825:run:46104476461".to_owned())]
    );
    assert!(
        receipt(&ledger, RUN_825)
            .progress
            .as_deref()
            .unwrap_or_default()
            .starts_with("unrecovered")
    );
    let again = act(
        &detection,
        &policy(MqStallMode::Apply, MqStallMode::Off),
        true,
        &observation,
        &mut ledger,
        &acts,
        next,
    );
    assert_eq!(again[0].outcome, "already_attempted");
    assert!(acts.cancels.borrow().is_empty());
    let status = mq_stall::status(
        &policy(MqStallMode::Apply, MqStallMode::Off),
        detection,
        again,
        Vec::new(),
        unrecovered,
        &ledger,
        REPO,
    );
    assert_eq!(
        status.summary.mq_stall,
        vec!["#825:run:46104476461:job:140854026289:68m"]
    );
    assert_eq!(
        status.summary.mq_stall_unrecovered,
        vec!["#825:run:46104476461"]
    );
    assert_eq!(
        status.summary.act,
        vec!["already_attempted:#825:run:46104476461"]
    );
    let _ = shape;
}

#[test]
fn a_run_that_was_not_cancelled_is_unrecovered() {
    let (observation, mut ledger, cancelled) = cancelled_825();
    let acts = FakeActs::head_825();
    *acts.run.borrow_mut() = Ok(RunState {
        status: "in_progress".to_owned(),
        conclusion: None,
        run_attempt: 1,
    });
    let (unrecovered, _) = mq_stall::progress(
        &acts,
        &observation,
        &[],
        &mut ledger,
        cancelled + chrono::Duration::minutes(61),
    );
    assert_eq!(unrecovered.len(), 1);
}

#[test]
fn an_unread_run_stays_pending_for_an_hour_then_is_unrecovered() {
    let (observation, mut ledger, cancelled) = cancelled_825();
    let acts = FakeActs::head_825();
    *acts.run.borrow_mut() = Err("HTTP 502".to_owned());
    let (unrecovered, lines) = mq_stall::progress(
        &acts,
        &observation,
        &[],
        &mut ledger,
        cancelled + chrono::Duration::minutes(5),
    );
    assert!(
        unrecovered.is_empty() && lines[0].contains("pending"),
        "{lines:?}"
    );
    assert!(receipt(&ledger, RUN_825).progress.is_none());
    let (unrecovered, _) = mq_stall::progress(
        &acts,
        &observation,
        &[],
        &mut ledger,
        cancelled + chrono::Duration::minutes(61),
    );
    assert_eq!(unrecovered.len(), 1);
}

/// Cancel #842's PR run.
fn cancelled_842() -> (RepoObservation, StewardLedger, DateTime<Utc>, FakeActs) {
    let shape = Shape::load("mq_stall_vellum_pr_jobs.json");
    let observation = pr_jobs(&shape);
    let detection = detect(&shape, &observation, shape.now);
    let acts = FakeActs::ok(Entry::default());
    let mut ledger = StewardLedger::default();
    let actions = mq_stall::act(
        &detection,
        &policy(MqStallMode::Off, MqStallMode::Apply),
        true,
        &BTreeSet::new(),
        &observation,
        &mut ledger,
        &HeadActs(&acts, &observation),
        shape.now,
    );
    assert_eq!(
        (actions[0].pr, actions[0].outcome.as_str()),
        (842, "cancelled")
    );
    acts.cancels.borrow_mut().clear();
    (observation, ledger, shape.now, acts)
}

#[test]
fn a_cancelled_pr_run_is_re_run_once_and_proved_by_its_new_attempt() {
    let (observation, mut ledger, cancelled, acts) = cancelled_842();
    let run = 46_111_057_074;
    let next = cancelled + chrono::Duration::minutes(5);
    let (unrecovered, lines) = mq_stall::progress(&acts, &observation, &[], &mut ledger, next);
    assert!(unrecovered.is_empty(), "{lines:?}");
    assert_eq!(*acts.reruns.borrow(), vec![run]);
    assert_eq!(receipt(&ledger, run).rerun.as_deref(), Some("requested"));
    assert!(receipt(&ledger, run).progress.is_none());
    // The next pass sees attempt 2.
    *acts.run.borrow_mut() = Ok(RunState {
        status: "queued".to_owned(),
        conclusion: None,
        run_attempt: 2,
    });
    mq_stall::progress(
        &acts,
        &observation,
        &[],
        &mut ledger,
        next + chrono::Duration::minutes(5),
    );
    assert_eq!(
        receipt(&ledger, run).progress.as_deref(),
        Some("recovered: re-run as attempt 2 (queued)")
    );
    assert_eq!(acts.reruns.borrow().len(), 1);
}

#[test]
fn a_re_run_that_never_shows_is_unrecovered() {
    let (observation, mut ledger, cancelled, acts) = cancelled_842();
    let next = cancelled + chrono::Duration::minutes(5);
    mq_stall::progress(&acts, &observation, &[], &mut ledger, next);
    let (unrecovered, _) = mq_stall::progress(
        &acts,
        &observation,
        &[],
        &mut ledger,
        next + chrono::Duration::minutes(61),
    );
    assert_eq!(
        unrecovered,
        vec![(Kind::PullRequest, "#842:run:46111057074".to_owned())]
    );
    assert_eq!(acts.reruns.borrow().len(), 1, "re-run once");
}

#[test]
fn the_stewards_own_re_run_counts_without_a_second_one() {
    let (observation, mut ledger, cancelled, acts) = cancelled_842();
    *acts.run.borrow_mut() = Ok(RunState {
        status: "in_progress".to_owned(),
        conclusion: None,
        run_attempt: 2,
    });
    mq_stall::progress(
        &acts,
        &observation,
        &[],
        &mut ledger,
        cancelled + chrono::Duration::minutes(5),
    );
    assert!(acts.reruns.borrow().is_empty());
    assert!(
        receipt(&ledger, 46_111_057_074)
            .progress
            .as_deref()
            .unwrap_or_default()
            .starts_with("recovered")
    );
}

#[test]
fn a_moved_pr_head_refuses_the_cancel() {
    let shape = Shape::load("mq_stall_vellum_pr_jobs.json");
    let observation = pr_jobs(&shape);
    let detection = detect(&shape, &observation, shape.now);
    let acts = FakeActs::ok(Entry {
        head: Some("f".repeat(40)),
        position: None,
        group: None,
    });
    let mut ledger = StewardLedger::default();
    let actions = mq_stall::act(
        &detection,
        &policy(MqStallMode::Off, MqStallMode::Apply),
        true,
        &BTreeSet::new(),
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert!(
        actions.iter().all(|a| a.outcome == "refused"),
        "{actions:?}"
    );
    assert!(acts.cancels.borrow().is_empty());
}

#[test]
fn the_queue_and_pr_knobs_are_separate() {
    let shape = Shape::load("mq_stall_vellum_pr_jobs.json");
    let observation = pr_jobs(&shape);
    let detection = detect(&shape, &observation, shape.now);
    let acts = FakeActs::ok(Entry::default());
    let mut ledger = StewardLedger::default();
    // The queue knob alone does nothing for pull request stalls.
    let actions = act(
        &detection,
        &policy(MqStallMode::Apply, MqStallMode::Off),
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert!(actions.is_empty(), "{actions:?}");
    let status = mq_stall::status(
        &MqStallPolicy::default(),
        detection,
        actions,
        Vec::new(),
        Vec::new(),
        &ledger,
        REPO,
    );
    assert_eq!(status.summary.pr_stall.len(), 4);
    assert!(status.summary.mq_stall.is_empty());
}

#[test]
fn the_policy_reads_from_the_base_config_on_a_merge_queue_base_only() {
    use crate::merge_steward::EjectionGuards;
    let table: toml::Table = toml::from_str(
        "[auto_merge]\nmq_stall_recover = \"apply\"\npr_stall_recover = \"shadow\"\n",
    )
    .expect("toml");
    let queued = EjectionGuards::from_table(&table, true).mq_stall;
    assert_eq!(
        (queued.mode, queued.pr_mode),
        (MqStallMode::Apply, MqStallMode::Shadow)
    );
    let direct = EjectionGuards::from_table(&table, false).mq_stall;
    assert_eq!(direct, MqStallPolicy::default());
    assert_eq!(
        EjectionGuards::for_base(true).mq_stall.mode,
        MqStallMode::Off
    );
}
