//! Jobs GitHub never gave a runner, on three recorded Generous-Corp/vellum
//! shapes (read back with `ghread`, trimmed to the fields the stall reads):
//!
//! * `mq_stall_vellum_825.json`, 2026-11-28T04:55Z: the queue head #825's
//!   run 46104476461 had job 140854026289 queued with runner id 0 since
//!   03:46:57 while job 140859201898 of #791's run 46106139411 (created
//!   04:08:34) started on `m5studio-vellum-gate-01-19987-181` at 04:11:24.
//!   The same-second sibling 140854029336 (#769, started 03:51:50) is a batch
//!   sibling and never a witness;
//! * `mq_stall_vellum_825_batch_sibling.json`, 08:00Z, a negative: head job
//!   140908713155 (created 07:38:19) waited while only its batch sibling
//!   140908717862 (created 07:38:21) had started; the head got a runner at
//!   08:10:19 and #825 merged at 09:03:46 unaided;
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
const RUN_791: u64 = 46_106_139_411;
const JOB_791: u64 = 140_859_201_898;
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
        Some(crate::merge_steward::DEFAULT_APPROVAL_CONTEXT),
        20,
        now,
    )
}

#[test]
fn the_825_shape_is_a_queue_head_stall_with_the_791_witness() {
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
    // Not #769's same-second batch sibling: #791's job, 11.6 minutes later.
    assert_eq!(
        (stall.witness_run_id, stall.witness_job_id),
        (RUN_791, JOB_791)
    );
    assert_eq!(
        stall.witness_runner.as_deref(),
        Some("m5studio-vellum-gate-01-19987-181")
    );
    assert_eq!(stall.token(), "#825:run:46104476461:job:140854026289:68m");
    // Jobs are read once per pass: the stalled run, the listing, #769's run,
    // the witness run.
    assert_eq!(detection.reads, 4, "{:?}", shape.reads.borrow());
}

/// The 07:38Z shape: only a batch sibling had a runner, and the head was
/// served 32 minutes in and merged by itself.
#[test]
fn a_batch_sibling_taking_the_free_runner_is_not_a_witness() {
    let shape = Shape::load("mq_stall_vellum_825_batch_sibling.json");
    let mut observation = observation(
        vec![
            pr(825, &"8".repeat(40), Some(1), true, None),
            pr(837, &"3".repeat(40), Some(2), true, None),
            pr(816, &"6".repeat(40), Some(3), true, None),
        ],
        shape.census.clone(),
    );
    observation.merge_group_heads = BTreeMap::from([
        (825, "a563a7a70457181faf57d0b170a9e80344a7e257".to_owned()),
        (816, "3d4f2d17399052076984888ee8c959d9f2a1c1ed".to_owned()),
    ]);
    let detection = detect(&shape, &observation, shape.now);
    assert!(detection.unread.is_none(), "{detection:?}");
    assert!(detection.stalls.is_empty(), "{detection:?}");
    // The same shape with a later witness would count: the margin is the guard.
    let mut later = Shape::load("mq_stall_vellum_825_batch_sibling.json");
    later.job_mut(140_908_717_862)["created_at"] = "2026-11-28T07:43:30Z".into();
    assert_eq!(detect(&later, &observation, later.now).stalls.len(), 1);
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
fn a_stalled_job_with_no_labels_is_never_a_stall() {
    let mut shape = Shape::load("mq_stall_vellum_825.json");
    shape.job_mut(JOB_825)["labels"] = serde_json::json!([]);
    assert!(
        detect(&shape, &queue_825(&shape), shape.now)
            .stalls
            .is_empty()
    );
}

#[test]
fn a_run_off_the_heads_current_group_is_never_judged() {
    let shape = Shape::load("mq_stall_vellum_825.json");
    let mut observation = queue_825(&shape);
    // #825's queue entry moved to a new group; the old group's run lingers.
    observation.merge_group_heads.insert(825, "c".repeat(40));
    let detection = detect(&shape, &observation, shape.now);
    assert!(detection.stalls.is_empty(), "{detection:?}");
    assert!(!shape.read_jobs_of(RUN_825), "{:?}", shape.reads.borrow());
}

#[test]
fn pull_request_targets_are_read_only_when_their_knob_is_on() {
    let shape = Shape::load("mq_stall_vellum_pr_jobs.json");
    let detection = mq_stall::detect(
        &|argv: &[String]| shape.gh(argv),
        &pr_jobs(&shape),
        None,
        20,
        shape.now,
    );
    assert!(detection.stalls.is_empty(), "{detection:?}");
    assert!(
        shape.reads.borrow().is_empty(),
        "{:?}",
        shape.reads.borrow()
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
        Some("review-verdict"),
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
    // Over 20 minutes old and created at least 5 minutes before the 06:56:33
    // witness; #833's job (06:54:10) is too close to it.
    assert_eq!(stalled, vec![842, 843, 844], "{detection:?}");
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
    /// The run's jobs on the final re-read; `None` is the stalled job alone.
    jobs: RefCell<Option<Result<Vec<Job>, String>>>,
    cancel: Result<(), String>,
    run: RefCell<Result<RunState, String>>,
    cancels: RefCell<Vec<u64>>,
    /// Save results, in order; empty means every save works.
    saves: RefCell<Vec<Result<(), String>>>,
    /// Each save: the receipt outcomes and how many cancels had run.
    saved: RefCell<Vec<(Vec<String>, usize)>>,
}

impl FakeActs {
    fn ok(entry: Entry) -> Self {
        Self {
            entry: RefCell::new(Ok(entry)),
            jobs: RefCell::new(None),
            cancel: Ok(()),
            run: RefCell::new(Ok(cancelled_run(1))),
            cancels: RefCell::default(),
            saves: RefCell::default(),
            saved: RefCell::default(),
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
    fn reread_jobs(&self, _run_id: u64) -> Result<Vec<Job>, String> {
        self.jobs
            .borrow()
            .clone()
            .unwrap_or_else(|| Ok(vec![waiting_job(JOB_825)]))
    }
    fn cancel(&self, run: &StewardRun) -> Result<(), String> {
        self.cancels.borrow_mut().push(run.id);
        self.cancel.clone()
    }
    fn persist(&self, ledger: &StewardLedger) -> Result<(), String> {
        self.saved.borrow_mut().push((
            ledger
                .mq_stall_cancels
                .values()
                .map(|receipt| receipt.outcome.clone())
                .collect(),
            self.cancels.borrow().len(),
        ));
        let mut saves = self.saves.borrow_mut();
        if saves.is_empty() {
            Ok(())
        } else {
            saves.remove(0)
        }
    }
    fn read_run(&self, _run_id: u64) -> Result<RunState, String> {
        self.run.borrow().clone()
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
    *served.jobs.borrow_mut() = Some(Ok(vec![job]));
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

/// The 825 stall, as if on another pull request and run.
fn other_stall(detection: &Detection, pr: u64, run_id: u64) -> mq_stall::Stall {
    let mut stall = detection.stalls[0].clone();
    stall.pr = pr;
    stall.run_id = run_id;
    stall
}

fn prior_receipt(ledger: &mut StewardLedger, pr: u64, run_id: u64, at: DateTime<Utc>) {
    ledger.mq_stall_cancels.insert(
        mq_stall::receipt_key(REPO, run_id),
        Receipt {
            repo: REPO.to_owned(),
            kind: Kind::MergeQueue,
            pr,
            run_id,
            job_id: 1,
            head_sha: G825.to_owned(),
            at: at.to_rfc3339(),
            outcome: "cancelled".to_owned(),
            progress: None,
            proved_at: None,
        },
    );
}

#[test]
fn one_cancel_per_pass() {
    let (shape, mut observation, mut detection) = stalled_825();
    let mut second = observation.runs[0].clone();
    second.id = RUN_825 + 1;
    observation.runs.push(second);
    let extra = other_stall(&detection, 825, RUN_825 + 1);
    detection.stalls.push(extra);
    let acts = FakeActs::head_825();
    let mut ledger = StewardLedger::default();
    let actions = act(
        &detection,
        &policy(MqStallMode::Apply, MqStallMode::Off),
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    let outcomes = actions
        .iter()
        .map(|a| a.outcome.as_str())
        .collect::<Vec<_>>();
    assert_eq!(outcomes, vec!["cancelled", "deferred"]);
    assert_eq!(acts.cancels.borrow().len(), 1);
}

#[test]
fn the_hourly_cap_holds() {
    let (shape, observation, detection) = stalled_825();
    let acts = FakeActs::head_825();
    let apply = policy(MqStallMode::Apply, MqStallMode::Off);
    let mut ledger = StewardLedger::default();
    for (index, pr) in [901, 902, 903].into_iter().enumerate() {
        let minutes = 10 * (i64::try_from(index).expect("small") + 1);
        prior_receipt(
            &mut ledger,
            pr,
            1_000 + pr,
            shape.now - chrono::Duration::minutes(minutes),
        );
    }
    let capped = act(
        &detection,
        &apply,
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert_eq!(capped[0].outcome, "refused", "{capped:?}");
    assert!(
        capped[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("cap 3"),
        "{capped:?}"
    );
    assert!(acts.cancels.borrow().is_empty());
    // Once the oldest is over an hour old there is room again.
    let later = shape.now + chrono::Duration::minutes(31);
    let next = act(
        &detection,
        &apply,
        true,
        &observation,
        &mut ledger,
        &acts,
        later,
    );
    assert_eq!(next[0].outcome, "cancelled", "{next:?}");
}

/// A re-enqueued head gets a new group and a new run id: one cancel per
/// pull request in six hours, not only per run.
#[test]
fn one_cancel_per_pull_request_in_six_hours() {
    let (shape, observation, detection) = stalled_825();
    let acts = FakeActs::head_825();
    let apply = policy(MqStallMode::Apply, MqStallMode::Off);
    let mut ledger = StewardLedger::default();
    prior_receipt(
        &mut ledger,
        825,
        RUN_825 - 5,
        shape.now - chrono::Duration::hours(2),
    );
    let refused = act(
        &detection,
        &apply,
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert_eq!(refused[0].outcome, "refused", "{refused:?}");
    assert!(
        refused[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("one per pull request"),
        "{refused:?}"
    );
    assert!(acts.cancels.borrow().is_empty());
    let later = shape.now + chrono::Duration::hours(5);
    assert_eq!(
        act(
            &detection,
            &apply,
            true,
            &observation,
            &mut ledger,
            &acts,
            later
        )[0]
        .outcome,
        "cancelled"
    );
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

/// The queue entry moved, but the run was never cancelled (it finished on
/// its own): not a recovery by the cancel.
#[test]
fn a_moved_entry_without_a_cancelled_run_is_not_recovered() {
    let (mut observation, mut ledger, cancelled) = cancelled_825();
    observation.merge_group_heads.insert(825, "b".repeat(40));
    let acts = FakeActs::head_825();
    *acts.run.borrow_mut() = Ok(RunState {
        status: "completed".to_owned(),
        conclusion: Some("success".to_owned()),
        run_attempt: 1,
    });
    let (none, _) = mq_stall::progress(
        &acts,
        &observation,
        &[],
        &mut ledger,
        cancelled + chrono::Duration::minutes(5),
    );
    assert!(none.is_empty());
    assert!(receipt(&ledger, RUN_825).progress.is_none());
    let (unrecovered, _) = mq_stall::progress(
        &acts,
        &observation,
        &[],
        &mut ledger,
        cancelled + chrono::Duration::minutes(61),
    );
    assert_eq!(unrecovered.len(), 1);
    assert!(
        receipt(&ledger, RUN_825)
            .progress
            .as_deref()
            .unwrap_or_default()
            .starts_with("unrecovered: run is completed/success")
    );
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

/// Pull request stalls are report only: even `apply` (read as shadow, but
/// also if a policy said apply) never cancels one.
#[test]
fn a_pull_request_stall_is_never_cancelled() {
    let shape = Shape::load("mq_stall_vellum_pr_jobs.json");
    let observation = pr_jobs(&shape);
    let detection = detect(&shape, &observation, shape.now);
    assert_eq!(detection.stalls.len(), 3);
    let acts = FakeActs::ok(Entry::default());
    let mut ledger = StewardLedger::default();
    let actions = act(
        &detection,
        &policy(MqStallMode::Apply, MqStallMode::Apply),
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert!(
        actions.iter().all(|a| a.outcome == "would_cancel"),
        "{actions:?}"
    );
    assert!(acts.cancels.borrow().is_empty() && ledger.mq_stall_cancels.is_empty());
}

#[test]
fn a_pull_request_receipt_from_an_older_binary_is_closed() {
    let acts = FakeActs::head_825();
    let shape = Shape::load("mq_stall_vellum_pr_jobs.json");
    let observation = pr_jobs(&shape);
    let mut ledger = StewardLedger::default();
    prior_receipt(
        &mut ledger,
        842,
        46_111_057_074,
        shape.now - chrono::Duration::minutes(5),
    );
    if let Some(receipt) = ledger
        .mq_stall_cancels
        .get_mut(&mq_stall::receipt_key(REPO, 46_111_057_074))
    {
        receipt.kind = Kind::PullRequest;
    }
    let (unrecovered, _) = mq_stall::progress(&acts, &observation, &[], &mut ledger, shape.now);
    assert_eq!(
        unrecovered,
        vec![(Kind::PullRequest, "#842:run:46111057074".to_owned())]
    );
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
    assert_eq!(status.summary.pr_stall.len(), 3);
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

#[test]
fn the_intent_is_saved_before_the_cancel_and_the_outcome_after() {
    let (shape, observation, detection) = stalled_825();
    let acts = FakeActs::head_825();
    let mut ledger = StewardLedger::default();
    let actions = act(
        &detection,
        &policy(MqStallMode::Apply, MqStallMode::Off),
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert_eq!(actions[0].outcome, "cancelled");
    assert_eq!(
        *acts.saved.borrow(),
        vec![
            (vec![mq_stall::INTENT.to_owned()], 0),
            (vec!["cancelled".to_owned()], 1),
        ]
    );
}

#[test]
fn an_intent_that_cannot_be_saved_refuses_the_cancel_and_is_an_error() {
    let (shape, observation, detection) = stalled_825();
    let acts = FakeActs::head_825();
    acts.saves.borrow_mut().push(Err("disk full".to_owned()));
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
    assert_eq!(actions[0].outcome, "intent_not_saved", "{actions:?}");
    assert!(acts.cancels.borrow().is_empty());
    assert!(ledger.mq_stall_cancels.is_empty());
    let status = mq_stall::status(
        &apply,
        detection,
        actions,
        Vec::new(),
        Vec::new(),
        &ledger,
        REPO,
    );
    assert_eq!(
        status.summary.errors,
        vec!["intent_not_saved:#825:run:46104476461"]
    );
    assert_eq!(status.errors().len(), 1);
}

/// A pass that saved its intent and died: the next pass reads the attempt
/// as `cancel_unknown`, and it counts against the per-PR limit.
#[test]
fn an_intent_with_no_outcome_is_cancel_unknown_and_counts() {
    let (shape, observation, detection) = stalled_825();
    let acts = FakeActs::head_825();
    let mut ledger = StewardLedger::default();
    prior_receipt(
        &mut ledger,
        825,
        RUN_825 - 5,
        shape.now - chrono::Duration::minutes(10),
    );
    if let Some(receipt) = ledger
        .mq_stall_cancels
        .get_mut(&mq_stall::receipt_key(REPO, RUN_825 - 5))
    {
        receipt.outcome = mq_stall::INTENT.to_owned();
    }
    let (_, lines) = mq_stall::progress(&acts, &observation, &[], &mut ledger, shape.now);
    assert!(
        lines.iter().any(|line| line.contains("cancel_unknown")),
        "{lines:?}"
    );
    assert_eq!(
        receipt(&ledger, RUN_825 - 5).outcome,
        mq_stall::CANCEL_UNKNOWN
    );
    let actions = act(
        &detection,
        &policy(MqStallMode::Apply, MqStallMode::Off),
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert_eq!(actions[0].outcome, "refused", "{actions:?}");
    assert!(acts.cancels.borrow().is_empty());
}

#[test]
fn a_sibling_that_started_since_detection_refuses_the_cancel() {
    let (shape, observation, detection) = stalled_825();
    let acts = FakeActs::head_825();
    let mut sibling = waiting_job(JOB_825 + 1);
    sibling.status = "in_progress".to_owned();
    sibling.runner_id = Some(35_831);
    *acts.jobs.borrow_mut() = Some(Ok(vec![waiting_job(JOB_825), sibling]));
    let mut ledger = StewardLedger::default();
    let actions = act(
        &detection,
        &policy(MqStallMode::Apply, MqStallMode::Off),
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert_eq!(actions[0].outcome, "refused", "{actions:?}");
    assert!(
        actions[0]
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("started"),
        "{actions:?}"
    );
    assert!(acts.cancels.borrow().is_empty() && ledger.mq_stall_cancels.is_empty());
}

#[test]
fn malformed_runner_fields_or_labels_are_unreadable_not_no_runner() {
    let row = |patch: Value| {
        let mut row = serde_json::json!({
            "id": JOB_825, "run_id": RUN_825, "name": "gpu-macos-arm64", "status": "queued",
            "created_at": "2026-11-28T03:46:57Z", "started_at": "2026-11-28T03:46:57Z",
            "runner_id": 0, "runner_name": "", "labels": ["self-hosted", "macOS"],
        });
        if let (Some(row), Some(patch)) = (row.as_object_mut(), patch.as_object()) {
            for (key, value) in patch {
                row.insert(key.clone(), value.clone());
            }
        }
        row
    };
    assert!(
        mq_stall::parse_job(&row(serde_json::json!({})))
            .expect("clean")
            .waiting()
    );
    assert!(
        mq_stall::parse_job(&row(
            serde_json::json!({"runner_id": null, "runner_name": null})
        ))
        .is_ok()
    );
    for bad in [
        serde_json::json!({"runner_id": "35831"}),
        serde_json::json!({"runner_id": -1}),
        serde_json::json!({"runner_name": 7}),
        serde_json::json!({"labels": ["self-hosted", 1]}),
        serde_json::json!({"labels": "self-hosted"}),
    ] {
        assert!(mq_stall::parse_job(&row(bad.clone())).is_err(), "{bad}");
    }
    // In a pass, the malformed stalled job decides nothing.
    let mut shape = Shape::load("mq_stall_vellum_825.json");
    shape.job_mut(JOB_825)["runner_id"] = "35831".into();
    let detection = detect(&shape, &queue_825(&shape), shape.now);
    assert!(
        detection.stalls.is_empty() && detection.unread.is_some(),
        "{detection:?}"
    );
}

#[test]
fn an_unread_detection_or_a_failed_cancel_is_a_repository_error() {
    let (shape, observation, mut detection) = stalled_825();
    let apply = policy(MqStallMode::Apply, MqStallMode::Off);
    let mut acts = FakeActs::head_825();
    acts.cancel = Err("HTTP 500".to_owned());
    let mut ledger = StewardLedger::default();
    let actions = act(
        &detection,
        &apply,
        true,
        &observation,
        &mut ledger,
        &acts,
        shape.now,
    );
    assert_eq!(actions[0].outcome, "cancel_failed");
    let status = mq_stall::status(
        &apply,
        detection.clone(),
        actions,
        Vec::new(),
        Vec::new(),
        &ledger,
        REPO,
    );
    assert_eq!(
        status.summary.errors,
        vec!["cancel_failed:#825:run:46104476461"]
    );
    assert!(
        status.errors()[0].contains("cancel_failed"),
        "{:?}",
        status.errors()
    );
    detection.unread = Some("run 1 jobs: HTTP 502".to_owned());
    let unread = mq_stall::status(
        &apply,
        detection,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        &ledger,
        REPO,
    );
    assert_eq!(unread.summary.errors, vec!["unread"]);
    assert!(
        unread.errors()[0].contains("HTTP 502"),
        "{:?}",
        unread.errors()
    );
}
