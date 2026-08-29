#[cfg(unix)]
use std::process::{Child, Command};

use super::*;
#[cfg(unix)]
use crate::parallel_proof_canary_job::launch_canary_job;
use crate::parallel_proof_canary_job::{
    ApprovedCanaryOperation, CanaryCancellationPolicy, CanaryJobOwner, CanaryLogPolicy,
    CanarySuccessPredicate, CanaryWakePredicate,
};

fn digest(value: &str) -> Sha256Digest {
    Sha256Digest::of_bytes(value.as_bytes())
}

fn job(executable_sha256: Sha256Digest) -> ApprovedCanaryJob {
    ApprovedCanaryJob {
        schema_version: 1,
        job_id: "adapter-real-child".to_owned(),
        correlation_id: "adapter-real-child".to_owned(),
        owner: CanaryJobOwner {
            controller_id: "controller".to_owned(),
            controller_incarnation: "incarnation".to_owned(),
            approval_sha256: digest("approval"),
        },
        operation: ApprovedCanaryOperation::ParallelProofDistributedShadow {
            repository_id: 42,
            repository: "Generous-Corp/pulp".to_owned(),
            target: "macos".to_owned(),
            target_triple: "aarch64-apple-darwin".to_owned(),
            builder_host_id: "m3".to_owned(),
            worker_host_id: "m1".to_owned(),
            manifest_sha256: digest("manifest"),
            request_sha256: digest("request"),
            release_sha256: digest("release"),
            builder_session_generation: 3,
            worker_session_generation: 5,
            cache_authority_sha256: digest("cache"),
            storage_authority_sha256: digest("storage"),
            artifact_bytes_total: 1024,
            invocation_authority_sha256: digest("invocation"),
            adapter_executable_sha256: digest("pinned-adapter"),
            worker_executable_sha256: executable_sha256,
        },
        approved_at_ms: 1,
        deadline_at_ms: 60_000,
        heartbeat_interval_ms: 100,
        heartbeat_timeout_ms: 1_000,
        max_heartbeat_receipts: 4,
        success: CanarySuccessPredicate {
            required_exit_code: 0,
            artifact_schema_version: 1,
            max_artifact_bytes: 4096,
        },
        cancellation: CanaryCancellationPolicy {
            grace_ms: 1_000,
            cancel_at_deadline: true,
        },
        wake: CanaryWakePredicate {
            on_success: true,
            on_actionable_failure: true,
        },
        native_continuation: None,
        logs: CanaryLogPolicy {
            segment_bytes: 1024,
            max_segments: 2,
        },
    }
}

#[cfg(unix)]
struct RealChildSupervisor {
    child: Option<Child>,
    executable_sha256: Sha256Digest,
}

#[cfg(unix)]
impl CanaryProcessSupervisor for RealChildSupervisor {
    fn launch_typed_worker(
        &mut self,
        request: &CanarySupervisedLaunch,
    ) -> Result<CanaryProcessTreeIdentity, String> {
        use std::os::unix::process::CommandExt as _;
        let mut command = Command::new("/bin/sleep");
        command.arg("30").process_group(0);
        let child = command.spawn().map_err(|error| error.to_string())?;
        let pid = child.id();
        self.child = Some(child);
        Ok(CanaryProcessTreeIdentity {
            pid,
            tree_id: format!("pgrp-{pid}"),
            birth_token: format!("test-{pid}"),
            os_start_identity_sha256: digest("test-start"),
            launch_nonce_sha256: request.launch_nonce_sha256.clone(),
            executable_sha256: self.executable_sha256.clone(),
            launched_at_ms: request.claimed_at_ms,
        })
    }

    fn discover_typed_worker(
        &mut self,
        _job: &ApprovedCanaryJob,
        _launch_nonce_sha256: &Sha256Digest,
    ) -> Result<CanaryProcessObservation, String> {
        Ok(CanaryProcessObservation::Missing)
    }

    fn observe_typed_worker(
        &mut self,
        _job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
    ) -> Result<CanaryProcessObservation, String> {
        match self.child.as_mut().ok_or("missing child")?.try_wait() {
            Ok(None) => Ok(CanaryProcessObservation::Alive(process.clone())),
            Ok(Some(status)) => Ok(CanaryProcessObservation::Exited {
                process: process.clone(),
                exit_code: status.code(),
                exited_at_ms: process.launched_at_ms + 1,
                artifact: None,
            }),
            Err(error) => Err(error.to_string()),
        }
    }

    fn cancel_typed_worker(
        &mut self,
        _job: &ApprovedCanaryJob,
        _process: &CanaryProcessTreeIdentity,
        _grace_ms: u64,
    ) -> Result<CanaryCancellationObservation, String> {
        let child = self.child.as_mut().ok_or("missing child")?;
        child.kill().map_err(|error| error.to_string())?;
        child.wait().map_err(|error| error.to_string())?;
        Ok(CanaryCancellationObservation::Terminated)
    }
}

#[cfg(unix)]
#[test]
fn typed_boundary_launches_a_real_child_without_command_authority() {
    let _guard = crate::test_support::PROCESS_TREE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let executable_sha256 = digest("pinned-shipyard-worker");
    let job = job(executable_sha256.clone());
    let temp = tempfile::tempdir().unwrap();
    let store = crate::parallel_proof_canary_job::CanaryJobStore::open(temp.path()).unwrap();
    let supervisor = RealChildSupervisor {
        child: None,
        executable_sha256,
    };
    let mut backend = DaemonCanaryJobBackend::new(supervisor);
    let transition = launch_canary_job(&store, &job, 2, &mut backend).unwrap();
    assert!(transition.launched);
    let process = match &transition.snapshot.latest().receipt {
        crate::parallel_proof_canary_job::CanaryJobReceiptState::Running { process } => process,
        other => panic!("expected running receipt, got {other:?}"),
    };
    assert_eq!(
        process.launch_nonce_sha256,
        match &transition.snapshot.receipts[0].receipt {
            crate::parallel_proof_canary_job::CanaryJobReceiptState::Prepared {
                launch_nonce_sha256,
            } => launch_nonce_sha256.clone(),
            _ => unreachable!(),
        }
    );
    assert_eq!(
        backend.cancel(&job, process, 1_000).unwrap(),
        CanaryCancellationObservation::Terminated
    );
}

#[test]
fn capability_is_exact_and_default_off() {
    assert!(!daemon_supports_canary_jobs(&serde_json::json!({})));
    assert!(!daemon_supports_canary_jobs(&serde_json::json!({
        "capabilities": ["parallel_proof_canary_job_v2"]
    })));
    assert!(daemon_supports_canary_jobs(&serde_json::json!({
        "capabilities": [DAEMON_CANARY_JOB_CAPABILITY]
    })));
}

#[cfg(windows)]
#[test]
fn production_supervisor_refuses_launch_without_unix_process_custody() {
    let temp = tempfile::tempdir().unwrap();
    let binary = std::env::current_exe().unwrap();
    let binary_sha256 = executable_digest(&binary).unwrap();
    let job = job(binary_sha256.clone());
    let request = CanarySupervisedLaunch {
        job: job.clone(),
        job_sha256: job.digest().unwrap(),
        launch_nonce_sha256: digest("windows-refusal"),
        claimed_at_ms: 2,
    };
    let mut supervisor = ShipyardCanaryProcessSupervisor::new(
        binary,
        RuntimeMode::Isolated,
        temp.path().join("global"),
        temp.path().join("state"),
    )
    .unwrap();

    assert_eq!(
        supervisor.launch_typed_worker(&request),
        Err("canary worker custody requires Unix process birth and group identity".to_owned())
    );
}

#[test]
fn bounded_batches_rotate_fairly_across_backlog() {
    let jobs = (0..5)
        .map(|index| format!("job-{index}"))
        .collect::<Vec<_>>();
    assert_eq!(bounded_job_batch(&jobs, None, 1), vec!["job-0"]);
    assert_eq!(bounded_job_batch(&jobs, Some("job-0"), 1), vec!["job-1"]);
    assert_eq!(bounded_job_batch(&jobs, Some("job-4"), 1), vec!["job-0"]);
    assert!(bounded_job_batch(&jobs, None, 0).is_empty());
}

#[cfg(unix)]
#[test]
fn mismatched_canary_identity_cannot_kill_queue_worker() {
    use std::os::unix::process::CommandExt as _;

    let _guard = crate::test_support::PROCESS_TREE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let binary = PathBuf::from("/bin/sleep");
    let binary_sha256 = executable_digest(&binary).unwrap();
    let job = job(binary_sha256.clone());
    let nonce = digest("pid-reuse-negative-control");
    let mut child = Command::new(&binary)
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    let process = CanaryProcessTreeIdentity {
        pid,
        tree_id: format!("pgrp-{pid}"),
        birth_token: nonce.as_str().to_owned(),
        os_start_identity_sha256: digest("different-process-start"),
        launch_nonce_sha256: nonce.clone(),
        executable_sha256: binary_sha256,
        launched_at_ms: 2,
    };
    let mut supervisor = ShipyardCanaryProcessSupervisor::new(
        binary,
        RuntimeMode::Isolated,
        temp.path().join("global"),
        temp.path().join("state"),
    )
    .unwrap();
    supervisor
        .store
        .put_receipt(&CanarySupervisorReceipt {
            job_id: job.job_id.clone(),
            generation: nonce.as_str().to_owned(),
            process: process.clone(),
        })
        .unwrap();

    assert_eq!(
        supervisor.cancel_typed_worker(&job, &process, 100).unwrap(),
        CanaryCancellationObservation::Missing
    );
    assert!(child.try_wait().unwrap().is_none());
    child.kill().unwrap();
    child.wait().unwrap();
}

#[cfg(unix)]
#[test]
fn canary_reaping_is_disjoint_from_queue_worker_custody() {
    use std::os::unix::process::CommandExt as _;

    let _guard = crate::test_support::PROCESS_TREE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut queue_worker = Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let mut canary_worker = Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();

    assert!(
        crate::worker_process_custody::terminate_detached_worker_tree(canary_worker.id()).unwrap()
    );
    canary_worker.wait().unwrap();
    assert!(queue_worker.try_wait().unwrap().is_none());

    queue_worker.kill().unwrap();
    queue_worker.wait().unwrap();
}

#[cfg(unix)]
#[test]
fn failed_spawn_identity_capture_leaves_no_orphan() {
    use std::os::unix::process::CommandExt as _;

    let _guard = crate::test_support::PROCESS_TREE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();

    let error = capture_worker_start_identity(&mut child, |_| {
        Err("injected start identity failure".to_owned())
    })
    .unwrap_err();

    assert_eq!(error, "injected start identity failure");
    assert!(child.try_wait().unwrap().is_some());
    assert_eq!(
        crate::worker_process_custody::process_id_liveness(pid),
        crate::worker_process_custody::ProcessLiveness::Dead
    );
}

#[cfg(unix)]
#[test]
fn production_supervisor_restarts_without_redispatch_and_cancels_tree() {
    let _guard = crate::test_support::PROCESS_TREE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let temp = tempfile::tempdir().unwrap();
    let descendant_pid_path = temp.path().join("detached-descendant.pid");
    let descendant_pid_path_literal = format!("{descendant_pid_path:?}");
    let source = format!(
        r#"use std::os::unix::process::CommandExt as _;
fn main() {{
    let mut command = std::process::Command::new("/bin/sleep");
    command.arg("30").process_group(0);
    let child = command.spawn().unwrap();
    let pid_path = std::path::PathBuf::from({descendant_pid_path_literal});
    let staged_pid_path = pid_path.with_extension("pid.staged");
    std::fs::write(&staged_pid_path, child.id().to_string()).unwrap();
    std::fs::rename(staged_pid_path, pid_path).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(30));
}}"#,
    );
    let binary = crate::test_support::compile_native_test_program(
        temp.path(),
        "typed_canary_worker_fixture",
        &source,
    );
    let binary_sha256 = executable_digest(&binary).unwrap();
    let job = job(binary_sha256);
    let nonce = digest("restart-nonce");
    let request = CanarySupervisedLaunch {
        job: job.clone(),
        job_sha256: job.digest().unwrap(),
        launch_nonce_sha256: nonce.clone(),
        claimed_at_ms: 2,
    };
    let state_dir = temp.path().join("state");
    let global_dir = temp.path().join("global");
    std::fs::create_dir_all(&global_dir).unwrap();
    let mut first = ShipyardCanaryProcessSupervisor::new(
        binary.clone(),
        RuntimeMode::Isolated,
        global_dir.clone(),
        state_dir.clone(),
    )
    .unwrap();
    let process = first.launch_typed_worker(&request).unwrap();
    // Full-suite scheduler load can delay the compiled fixture well beyond
    // the old three-second wall-clock guess. Bound the wait generously,
    // but fail early if the exact root exits before publishing readiness.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !descendant_pid_path.exists() && Instant::now() < deadline {
        if !matches!(
            first.observe_typed_worker(&job, &process).unwrap(),
            CanaryProcessObservation::Alive(_)
        ) {
            panic!("canary fixture exited before publishing descendant identity");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let descendant_pid = std::fs::read_to_string(&descendant_pid_path)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    drop(first);

    let mut restarted =
        ShipyardCanaryProcessSupervisor::new(binary, RuntimeMode::Isolated, global_dir, state_dir)
            .unwrap();
    assert!(matches!(
        restarted.discover_typed_worker(&job, &nonce).unwrap(),
        CanaryProcessObservation::Alive(observed) if observed == process
    ));
    assert_eq!(
        restarted.cancel_typed_worker(&job, &process, 500).unwrap(),
        CanaryCancellationObservation::Terminated
    );
    let descendant_status = Command::new("/bin/ps")
        .args(["-p", &descendant_pid.to_string(), "-o", "stat="])
        .output()
        .unwrap();
    assert!(
        !descendant_status.status.success()
            || String::from_utf8_lossy(&descendant_status.stdout)
                .trim_start()
                .starts_with('Z')
    );
    assert!(matches!(
        restarted.observe_typed_worker(&job, &process).unwrap(),
        CanaryProcessObservation::Missing
    ));
}

#[cfg(unix)]
#[test]
fn production_supervisor_missing_receipt_never_launches_during_discovery() {
    let temp = tempfile::tempdir().unwrap();
    let binary = crate::test_support::compile_native_test_program(
        temp.path(),
        "typed_canary_missing_fixture",
        "fn main() {}",
    );
    let binary_sha256 = executable_digest(&binary).unwrap();
    let job = job(binary_sha256);
    let mut supervisor = ShipyardCanaryProcessSupervisor::new(
        binary,
        RuntimeMode::Isolated,
        temp.path().join("global"),
        temp.path().join("state"),
    )
    .unwrap();
    assert!(matches!(
        supervisor
            .discover_typed_worker(&job, &digest("never-launched"))
            .unwrap(),
        CanaryProcessObservation::Missing
    ));
}
