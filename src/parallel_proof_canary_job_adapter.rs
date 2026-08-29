//! Closed protocol between durable canary custody and Shipyard's daemon-owned
//! process supervisor.
//!
//! The CLI may submit immutable work, but only a daemon advertising the exact
//! capability below may instantiate this backend. There is deliberately no
//! foreground, shell, `nohup`, or ambient-cwd fallback.

use serde::{Deserialize, Serialize};

use crate::parallel_proof::Sha256Digest;
use crate::parallel_proof_canary_job::{
    ApprovedCanaryJob, CanaryCancellationObservation, CanaryJobBackend, CanaryProcessObservation,
    CanaryProcessTreeIdentity,
};

/// Exact daemon status capability required before production submission.
pub const DAEMON_CANARY_JOB_CAPABILITY: &str = "parallel_proof_canary_job_v1";

/// Return true only when the live daemon explicitly advertises the typed lane.
#[must_use]
pub fn daemon_supports_canary_jobs(status: &serde_json::Value) -> bool {
    status
        .get("capabilities")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|capabilities| {
            capabilities
                .iter()
                .any(|capability| capability.as_str() == Some(DAEMON_CANARY_JOB_CAPABILITY))
        })
}

/// Closed launch request passed from custody to the daemon-owned supervisor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanarySupervisedLaunch {
    /// Exact immutable job envelope.
    pub job: ApprovedCanaryJob,
    /// Digest of the exact envelope, repeated for protocol framing.
    pub job_sha256: Sha256Digest,
    /// Prepared-receipt nonce; the worker must publish it in its identity.
    pub launch_nonce_sha256: Sha256Digest,
    /// Controller time at the exclusive immutable launch claim.
    pub claimed_at_ms: u64,
}

impl CanarySupervisedLaunch {
    fn validate(&self) -> Result<(), String> {
        self.job.validate().map_err(|error| error.to_string())?;
        if self.job.digest().map_err(|error| error.to_string())? != self.job_sha256 {
            return Err("supervised canary launch job digest mismatch".to_owned());
        }
        Ok(())
    }
}

/// Daemon-owned process mechanics. Implementations choose the already-pinned
/// Shipyard worker binary; the request contains no executable path or arguments.
pub trait CanaryProcessSupervisor {
    /// Spawn the hidden typed worker in the daemon's supervised process group.
    fn launch_typed_worker(
        &mut self,
        request: &CanarySupervisedLaunch,
    ) -> Result<CanaryProcessTreeIdentity, String>;

    /// Discover an original launch by immutable nonce after daemon restart.
    fn discover_typed_worker(
        &mut self,
        job: &ApprovedCanaryJob,
        launch_nonce_sha256: &Sha256Digest,
    ) -> Result<CanaryProcessObservation, String>;

    /// Observe the exact process generation without redispatch.
    fn observe_typed_worker(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
    ) -> Result<CanaryProcessObservation, String>;

    /// Terminate and prove the complete supervised tree within the grace bound.
    fn cancel_typed_worker(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
        grace_ms: u64,
    ) -> Result<CanaryCancellationObservation, String>;
}

/// Production adapter used by the daemon lane to drive durable custody.
pub struct DaemonCanaryJobBackend<S> {
    supervisor: S,
}

impl<S> DaemonCanaryJobBackend<S> {
    /// Bind one daemon-owned supervisor. Constructing this does not launch work.
    pub const fn new(supervisor: S) -> Self {
        Self { supervisor }
    }
}

impl<S: CanaryProcessSupervisor> CanaryJobBackend for DaemonCanaryJobBackend<S> {
    fn launch(
        &mut self,
        job: &ApprovedCanaryJob,
        launch_nonce_sha256: &Sha256Digest,
        claimed_at_ms: u64,
    ) -> Result<CanaryProcessTreeIdentity, String> {
        let request = CanarySupervisedLaunch {
            job: job.clone(),
            job_sha256: job.digest().map_err(|error| error.to_string())?,
            launch_nonce_sha256: launch_nonce_sha256.clone(),
            claimed_at_ms,
        };
        request.validate()?;
        self.supervisor.launch_typed_worker(&request)
    }

    fn discover(
        &mut self,
        job: &ApprovedCanaryJob,
        launch_nonce_sha256: &Sha256Digest,
    ) -> Result<CanaryProcessObservation, String> {
        self.supervisor
            .discover_typed_worker(job, launch_nonce_sha256)
    }

    fn observe(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
    ) -> Result<CanaryProcessObservation, String> {
        self.supervisor.observe_typed_worker(job, process)
    }

    fn cancel(
        &mut self,
        job: &ApprovedCanaryJob,
        process: &CanaryProcessTreeIdentity,
        grace_ms: u64,
    ) -> Result<CanaryCancellationObservation, String> {
        self.supervisor.cancel_typed_worker(job, process, grace_ms)
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command};

    use super::*;
    use crate::parallel_proof_canary_job::{
        ApprovedCanaryOperation, CanaryCancellationPolicy, CanaryJobOwner, CanaryLogPolicy,
        CanarySuccessPredicate, CanaryWakePredicate, launch_canary_job,
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
                adapter_executable_sha256: executable_sha256,
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
            .expect("process tree test lock");
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
}
