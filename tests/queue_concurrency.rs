//! End-to-end cooperative queue concurrency coverage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use chrono::Utc;
use shipyard::config::{LoadedConfig, LocalOverlaySource};
use shipyard::evidence::EvidenceStore;
use shipyard::executor::dispatch::{
    DispatchValidationRequest, ResolvedBackend, ResolvedTarget, ResolvedValidation,
};
use shipyard::executor::local::{LocalTargetConfig, LocalValidationConfig};
use shipyard::job::{JobStatus, Priority, TargetResult, TargetStatus, ValidationMode};
use shipyard::queue::Queue;
use shipyard::queue_request::QueueOutcomeStore;
use shipyard::ship::{
    RunExecutionRequest, RunStores, ShipExecutionRequest, ShipStores, ShipTargetDispatcher,
    drain_or_wait_run, drain_or_wait_ship, submit_run, submit_ship,
};
use shipyard::ship_state::ShipStateStore;
use shipyard::warm_pool::WarmPool;

fn local_target(name: &str, cwd: impl Into<PathBuf>) -> ResolvedTarget {
    let mut stages = BTreeMap::new();
    stages.insert("test".to_owned(), "true".to_owned());
    ResolvedTarget {
        name: name.to_owned(),
        platform: "macos".to_owned(),
        backend_name: "local".to_owned(),
        warm_keepalive_seconds: 0,
        host: None,
        backend: ResolvedBackend::Local(LocalTargetConfig {
            name: name.to_owned(),
            platform: "macos".to_owned(),
            cwd: Some(cwd.into()),
            timeout_secs: 300,
        }),
        validation: ResolvedValidation::Local(LocalValidationConfig {
            command: None,
            stages,
            contract: None,
            prepared_state_enabled: true,
            allow_tree_drift: false,
        }),
        failure_parser: None,
    }
}

fn run_request(branch: &str, sha: &str, target: ResolvedTarget) -> RunExecutionRequest {
    RunExecutionRequest {
        branch: branch.to_owned(),
        sha: sha.to_owned(),
        mode: ValidationMode::Full,
        priority: Priority::Normal,
        warm_disabled: true,
        fail_fast: false,
        resume_from: None,
        targets: vec![target],
    }
}

fn ship_request(branch: &str, sha: &str, pr: u64, target: ResolvedTarget) -> ShipExecutionRequest {
    ShipExecutionRequest {
        pr,
        repo: "danielraffel/shipyard".to_owned(),
        branch: branch.to_owned(),
        base_branch: "main".to_owned(),
        sha: sha.to_owned(),
        commit_subject: "queue concurrency integration".to_owned(),
        pr_url: Some(format!(
            "https://github.com/danielraffel/shipyard/pull/{pr}"
        )),
        pr_title: Some("Queue concurrency integration".to_owned()),
        mode: ValidationMode::Full,
        priority: Priority::Normal,
        warm_disabled: true,
        fail_fast: false,
        resume_from: None,
        advisory_targets: BTreeSet::new(),
        adopt_head: false,
        targets: vec![target],
    }
}

fn run_stores<'a>(
    queue: &'a mut Queue,
    evidence: &'a EvidenceStore,
    warm_pool: &'a WarmPool,
    cwd: &'a Path,
    state_dir: &'a Path,
    config: &'a LoadedConfig,
) -> RunStores<'a> {
    RunStores {
        queue,
        evidence,
        warm_pool,
        cwd,
        state_dir,
        config,
    }
}

fn ship_stores<'a>(
    queue: &'a mut Queue,
    evidence: &'a EvidenceStore,
    ship_state: &'a ShipStateStore,
    warm_pool: &'a WarmPool,
    cwd: &'a Path,
    state_dir: &'a Path,
    config: &'a LoadedConfig,
) -> ShipStores<'a> {
    ShipStores {
        queue,
        evidence,
        ship_state,
        warm_pool,
        cwd,
        state_dir,
        config,
    }
}

fn empty_config(root: &Path) -> LoadedConfig {
    LoadedConfig {
        data: toml::Table::new(),
        global_dir: root.join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: LocalOverlaySource::None,
    }
}

#[derive(Default)]
struct ProbeState {
    active: usize,
    max_active: usize,
    calls: usize,
    threads: Vec<ThreadId>,
}

struct ProbeDispatcher {
    state: Mutex<ProbeState>,
    changed: Condvar,
    wait_for_overlap: bool,
    hold_for: Duration,
}

impl ProbeDispatcher {
    fn new(wait_for_overlap: bool, hold_for: Duration) -> Self {
        Self {
            state: Mutex::new(ProbeState::default()),
            changed: Condvar::new(),
            wait_for_overlap,
            hold_for,
        }
    }

    fn calls(&self) -> usize {
        self.state.lock().expect("state").calls
    }

    fn max_active(&self) -> usize {
        self.state.lock().expect("state").max_active
    }

    fn threads(&self) -> Vec<ThreadId> {
        self.state.lock().expect("state").threads.clone()
    }
}

impl ShipTargetDispatcher for ProbeDispatcher {
    fn validate(&self, request: DispatchValidationRequest<'_, '_>) -> TargetResult {
        {
            let mut state = self.state.lock().expect("state");
            state.active += 1;
            state.calls += 1;
            state.max_active = state.max_active.max(state.active);
            state.threads.push(thread::current().id());
            self.changed.notify_all();
            if self.wait_for_overlap {
                let deadline = Instant::now() + Duration::from_secs(5);
                while state.active < 2 {
                    let now = Instant::now();
                    assert!(now < deadline, "timed out waiting for overlapping workers");
                    let timeout = deadline.saturating_duration_since(now);
                    let (next, _) = self
                        .changed
                        .wait_timeout(state, timeout)
                        .expect("wait for overlap");
                    state = next;
                }
            }
        }

        if !self.hold_for.is_zero() {
            thread::sleep(self.hold_for);
        }

        let mut state = self.state.lock().expect("state");
        state.active -= 1;
        self.changed.notify_all();
        drop(state);

        let now = Utc::now();
        let mut result = TargetResult::new(
            request.target.name.clone(),
            request.target.platform.clone(),
            TargetStatus::Pass,
            request.target.backend_name.clone(),
        );
        result.started_at = Some(now);
        result.completed_at = Some(now);
        result.log_path = Some(request.log_path.to_string_lossy().into_owned());
        result
    }
}

#[test]
fn drain_owner_runs_non_conflicting_jobs_concurrently() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path().join("state");
    let cwd_a = temp.path().join("repo-a");
    let cwd_b = temp.path().join("repo-b");
    std::fs::create_dir_all(&cwd_a).expect("repo a");
    std::fs::create_dir_all(&cwd_b).expect("repo b");
    let mut queue = Queue::new(&state_dir).expect("queue");
    let evidence = EvidenceStore::new(state_dir.join("evidence")).expect("evidence");
    let warm_pool = WarmPool::new(state_dir.join("warm_pool.json"));
    let config = empty_config(temp.path());
    let dispatcher = ProbeDispatcher::new(true, Duration::from_millis(20));
    let request_a = run_request("feature/a", "sha-a", local_target("mac-a", &cwd_a));
    let request_b = run_request("feature/b", "sha-b", local_target("mac-b", &cwd_b));
    let job_a = submit_run(&request_a, &mut queue, temp.path(), &state_dir).expect("submit a");
    let job_b = submit_run(&request_b, &mut queue, temp.path(), &state_dir).expect("submit b");

    let outcome = drain_or_wait_run(
        &request_a,
        job_a.clone(),
        run_stores(
            &mut queue,
            &evidence,
            &warm_pool,
            temp.path(),
            &state_dir,
            &config,
        ),
        &dispatcher,
    )
    .expect("drain");

    assert_eq!(outcome.job.status, JobStatus::Completed);
    assert_eq!(dispatcher.calls(), 2);
    assert_eq!(dispatcher.max_active(), 2);
    assert_eq!(
        queue.get(&job_b.id).expect("queue").expect("job b").status,
        JobStatus::Completed
    );
}

#[test]
#[allow(clippy::similar_names)]
fn conflicting_jobs_serialize_across_submitters() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path().join("state");
    let shared_cwd = temp.path().join("repo");
    std::fs::create_dir_all(&shared_cwd).expect("repo");
    let mut queue = Queue::new(&state_dir).expect("queue");
    let request_a = run_request("feature/a", "sha-a", local_target("mac-a", &shared_cwd));
    let request_b = run_request("feature/b", "sha-b", local_target("mac-b", &shared_cwd));
    let job_a = submit_run(&request_a, &mut queue, temp.path(), &state_dir).expect("submit a");
    let job_b = submit_run(&request_b, &mut queue, temp.path(), &state_dir).expect("submit b");
    drop(queue);

    let dispatcher = Arc::new(ProbeDispatcher::new(false, Duration::from_millis(100)));
    let state_a = state_dir.clone();
    let state_b = state_dir.clone();
    let cwd_a = temp.path().to_path_buf();
    let cwd_b = temp.path().to_path_buf();
    let dispatcher_a = Arc::clone(&dispatcher);
    let dispatcher_b = Arc::clone(&dispatcher);
    let request_a_thread = request_a.clone();
    let request_b_thread = request_b.clone();
    let handle_a = thread::spawn(move || {
        let mut queue = Queue::new(&state_a).expect("queue a");
        let evidence = EvidenceStore::new(state_a.join("evidence")).expect("evidence a");
        let warm_pool = WarmPool::new(state_a.join("warm_pool.json"));
        let config = empty_config(&cwd_a);
        drain_or_wait_run(
            &request_a_thread,
            job_a,
            run_stores(&mut queue, &evidence, &warm_pool, &cwd_a, &state_a, &config),
            dispatcher_a.as_ref(),
        )
        .expect("drain a")
    });
    let handle_b = thread::spawn(move || {
        let mut queue = Queue::new(&state_b).expect("queue b");
        let evidence = EvidenceStore::new(state_b.join("evidence")).expect("evidence b");
        let warm_pool = WarmPool::new(state_b.join("warm_pool.json"));
        let config = empty_config(&cwd_b);
        drain_or_wait_run(
            &request_b_thread,
            job_b,
            run_stores(&mut queue, &evidence, &warm_pool, &cwd_b, &state_b, &config),
            dispatcher_b.as_ref(),
        )
        .expect("drain b")
    });

    assert_eq!(
        handle_a.join().expect("join a").job.status,
        JobStatus::Completed
    );
    assert_eq!(
        handle_b.join().expect("join b").job.status,
        JobStatus::Completed
    );
    assert_eq!(dispatcher.calls(), 2);
    assert_eq!(dispatcher.max_active(), 1);
}

#[test]
fn losing_submitter_waits_without_dispatching_targets() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path().join("state");
    let cwd_a = temp.path().join("repo-a");
    let cwd_b = temp.path().join("repo-b");
    std::fs::create_dir_all(&cwd_a).expect("repo a");
    std::fs::create_dir_all(&cwd_b).expect("repo b");
    let mut queue = Queue::new(&state_dir).expect("queue");
    let request_a = run_request("feature/a", "sha-a", local_target("mac-a", &cwd_a));
    let request_b = run_request("feature/b", "sha-b", local_target("mac-b", &cwd_b));
    let job_a = submit_run(&request_a, &mut queue, temp.path(), &state_dir).expect("submit a");
    let job_b = submit_run(&request_b, &mut queue, temp.path(), &state_dir).expect("submit b");
    drop(queue);

    let dispatcher = Arc::new(ProbeDispatcher::new(true, Duration::from_millis(50)));
    let owner_state = state_dir.clone();
    let owner_cwd = temp.path().to_path_buf();
    let owner_dispatcher = Arc::clone(&dispatcher);
    let owner_request = request_a.clone();
    let owner = thread::spawn(move || {
        let mut queue = Queue::new(&owner_state).expect("owner queue");
        let evidence = EvidenceStore::new(owner_state.join("evidence")).expect("owner evidence");
        let warm_pool = WarmPool::new(owner_state.join("warm_pool.json"));
        let config = empty_config(&owner_cwd);
        drain_or_wait_run(
            &owner_request,
            job_a,
            run_stores(
                &mut queue,
                &evidence,
                &warm_pool,
                &owner_cwd,
                &owner_state,
                &config,
            ),
            owner_dispatcher.as_ref(),
        )
        .expect("owner drain")
    });

    let loser_state = state_dir.clone();
    let loser_cwd = temp.path().to_path_buf();
    let loser_dispatcher = Arc::clone(&dispatcher);
    let loser_request = request_b.clone();
    let loser = thread::spawn(move || {
        let loser_thread_id = thread::current().id();
        let mut queue = Queue::new(&loser_state).expect("loser queue");
        let evidence = EvidenceStore::new(loser_state.join("evidence")).expect("loser evidence");
        let warm_pool = WarmPool::new(loser_state.join("warm_pool.json"));
        let config = empty_config(&loser_cwd);
        let outcome = drain_or_wait_run(
            &loser_request,
            job_b,
            run_stores(
                &mut queue,
                &evidence,
                &warm_pool,
                &loser_cwd,
                &loser_state,
                &config,
            ),
            loser_dispatcher.as_ref(),
        )
        .expect("loser wait");
        (loser_thread_id, outcome)
    });

    assert_eq!(
        owner.join().expect("join owner").job.status,
        JobStatus::Completed
    );
    let (loser_thread_id, loser_outcome) = loser.join().expect("join loser");
    assert_eq!(loser_outcome.job.status, JobStatus::Completed);
    assert_eq!(dispatcher.calls(), 2);
    assert!(
        dispatcher
            .threads()
            .into_iter()
            .all(|thread_id| thread_id != loser_thread_id),
        "losing submitter thread must not dispatch targets"
    );
}

#[test]
fn same_pr_pending_ship_is_superseded_by_newer_ship() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path().join("state");
    let cwd = temp.path().join("repo");
    std::fs::create_dir_all(&cwd).expect("repo");
    let mut queue = Queue::new(&state_dir).expect("queue");
    let evidence = EvidenceStore::new(state_dir.join("evidence")).expect("evidence");
    let ship_state = ShipStateStore::new(state_dir.join("ship")).expect("ship state");
    let warm_pool = WarmPool::new(state_dir.join("warm_pool.json"));
    let config = empty_config(temp.path());
    let dispatcher = ProbeDispatcher::new(false, Duration::ZERO);
    let old_request = ship_request("feature/old", "sha-old", 42, local_target("mac-old", &cwd));
    let new_request = ship_request("feature/new", "sha-new", 42, local_target("mac-new", &cwd));
    let old_job = submit_ship(&old_request, &mut queue, temp.path(), &state_dir).expect("old");
    let new_job = submit_ship(&new_request, &mut queue, temp.path(), &state_dir).expect("new");

    let outcome = drain_or_wait_ship(
        &new_request,
        new_job,
        ship_stores(
            &mut queue,
            &evidence,
            &ship_state,
            &warm_pool,
            temp.path(),
            &state_dir,
            &config,
        ),
        &dispatcher,
    )
    .expect("drain ship");

    assert_eq!(outcome.job.status, JobStatus::Completed);
    let old = queue.get(&old_job.id).expect("queue").expect("old job");
    assert_eq!(old.status, JobStatus::Cancelled);
    assert!(
        old.cancellation_reason
            .as_deref()
            .unwrap_or_default()
            .contains("Superseded by newer queued ship")
    );
}

#[test]
fn abandoned_drain_after_start_is_recovered_with_outcome() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state_dir = temp.path().join("state");
    let cwd = temp.path().join("repo");
    std::fs::create_dir_all(&cwd).expect("repo");
    let mut queue = Queue::new(&state_dir).expect("queue");
    let evidence = EvidenceStore::new(state_dir.join("evidence")).expect("evidence");
    let warm_pool = WarmPool::new(state_dir.join("warm_pool.json"));
    let config = empty_config(temp.path());
    let dispatcher = ProbeDispatcher::new(false, Duration::ZERO);
    let request = run_request("feature/recover", "sha-recover", local_target("mac", &cwd));
    let job = submit_run(&request, &mut queue, temp.path(), &state_dir).expect("submit");
    {
        let drain_lock = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("acquired");
        let started = queue
            .start_pending_jobs_for_drain(&drain_lock, std::slice::from_ref(&job.id))
            .expect("start");
        assert_eq!(started.len(), 1);
    }

    let outcome = drain_or_wait_run(
        &request,
        job.clone(),
        run_stores(
            &mut queue,
            &evidence,
            &warm_pool,
            temp.path(),
            &state_dir,
            &config,
        ),
        &dispatcher,
    )
    .expect("recover");

    assert_eq!(outcome.job.status, JobStatus::Completed);
    assert_eq!(dispatcher.calls(), 0);
    assert!(
        QueueOutcomeStore::new(&state_dir)
            .expect("outcome store")
            .load(&job.id)
            .expect("load outcome")
            .is_some()
    );
}
