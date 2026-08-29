//! Shared singleton native-worker admission for daemon execution runtimes.
//!
//! Durable queue and canary custody remain independent. This module shares
//! only the host-capacity fence that must be held before either runtime may
//! launch a native worker.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::host_pool::{HostPoolLeaseError, HostPoolLeaseRequest, HostPoolLeaseStore};
use crate::queue::Queue;

const POOL: &str = "shipyard-daemon-native-worker";
const MEMBER: &str = "local-daemon";
const LEASE_STALE_SECONDS: u64 = 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DaemonWorkerKind {
    Queue,
    Canary,
    ExclusiveSandbox,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DaemonWorkerClaim {
    kind: DaemonWorkerKind,
    work_id: String,
    authority_sha: String,
}

impl DaemonWorkerClaim {
    pub(crate) fn queue(work_id: &str, authority_sha: &str) -> Self {
        Self {
            kind: DaemonWorkerKind::Queue,
            work_id: work_id.to_owned(),
            authority_sha: authority_sha.to_owned(),
        }
    }

    pub(crate) fn canary(work_id: &str, authority_sha: &str) -> Self {
        Self {
            kind: DaemonWorkerKind::Canary,
            work_id: work_id.to_owned(),
            authority_sha: authority_sha.to_owned(),
        }
    }

    pub(crate) fn exclusive_sandbox(work_id: &str, authority_sha: &str) -> Self {
        Self {
            kind: DaemonWorkerKind::ExclusiveSandbox,
            work_id: work_id.to_owned(),
            authority_sha: authority_sha.to_owned(),
        }
    }

    fn lease_job_id(&self) -> String {
        let kind = match self.kind {
            DaemonWorkerKind::Queue => "queue",
            DaemonWorkerKind::Canary => "canary",
            DaemonWorkerKind::ExclusiveSandbox => "exclusive-sandbox",
        };
        format!("daemon-capacity:{kind}:{}", self.work_id)
    }

    pub(crate) fn work_id(&self) -> &str {
        &self.work_id
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DaemonWorkerCapacity {
    leases: HostPoolLeaseStore,
    lease_stale_seconds: u64,
}

impl DaemonWorkerCapacity {
    pub(crate) fn new(state_dir: &Path) -> Self {
        Self {
            leases: HostPoolLeaseStore::new(
                state_dir.join("daemon-worker-capacity").join("leases.json"),
            ),
            lease_stale_seconds: LEASE_STALE_SECONDS,
        }
    }

    #[cfg(test)]
    fn with_stale_seconds(state_dir: &Path, lease_stale_seconds: u64) -> Self {
        Self {
            leases: HostPoolLeaseStore::new(
                state_dir.join("daemon-worker-capacity").join("leases.json"),
            ),
            lease_stale_seconds,
        }
    }

    pub(crate) fn claim_or_heartbeat(
        &self,
        claim: &DaemonWorkerClaim,
    ) -> Result<bool, HostPoolLeaseError> {
        self.leases
            .acquire_or_heartbeat_exact(&HostPoolLeaseRequest {
                pool_name: POOL.to_owned(),
                member_id: MEMBER.to_owned(),
                target_name: "native-worker".to_owned(),
                backend: "daemon".to_owned(),
                host: None,
                job_id: Some(claim.lease_job_id()),
                branch: claim.kind_label().to_owned(),
                sha: claim.authority_sha.clone(),
                max_concurrency: 1,
                lease_stale_seconds: self.lease_stale_seconds,
            })
            .map(|lease| lease.is_some())
    }

    pub(crate) fn heartbeat_existing(
        &self,
        claim: &DaemonWorkerClaim,
    ) -> Result<bool, HostPoolLeaseError> {
        self.leases.heartbeat_existing_job(
            POOL,
            MEMBER,
            &claim.lease_job_id(),
            self.lease_stale_seconds,
        )
    }

    pub(crate) fn release_inactive_queue_claims(
        &self,
        active_work_ids: &BTreeSet<String>,
    ) -> Result<(), HostPoolLeaseError> {
        for lease in self.leases.leases()? {
            if lease.pool_name != POOL || lease.member_id != MEMBER || lease.branch != "queue" {
                continue;
            }
            let Some(job_id) = lease.job_id.as_deref() else {
                continue;
            };
            let Some(work_id) = job_id.strip_prefix("daemon-capacity:queue:") else {
                continue;
            };
            if !active_work_ids.contains(work_id) {
                self.leases.release(&lease.lease_id)?;
            }
        }
        Ok(())
    }

    pub(crate) fn release(&self, claim: &DaemonWorkerClaim) -> Result<bool, HostPoolLeaseError> {
        self.leases
            .release_for_job(&claim.lease_job_id())
            .map(|removed| removed != 0)
    }

    /// Acquire the exclusive sandbox barrier only from an exactly idle queue.
    /// The drain lock serializes the emptiness check with daemon admission;
    /// the shared singleton lease then blocks queue and canary launch.
    #[allow(
        dead_code,
        reason = "consumed by the exclusive sandbox audit integration lane"
    )]
    pub(crate) fn claim_exclusive_sandbox_if_queue_idle(
        &self,
        state_dir: &Path,
        work_id: &str,
        authority_sha: &str,
    ) -> Result<Option<ExclusiveSandboxLease>, String> {
        let mut queue = Queue::new(state_dir).map_err(|error| error.to_string())?;
        let Some(_drain_lock) = queue
            .acquire_drain_lock()
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        if !queue
            .get_running()
            .map_err(|error| error.to_string())?
            .is_empty()
            || !queue
                .get_pending()
                .map_err(|error| error.to_string())?
                .is_empty()
        {
            return Ok(None);
        }
        let claim = DaemonWorkerClaim::exclusive_sandbox(work_id, authority_sha);
        if self
            .claim_or_heartbeat(&claim)
            .map_err(|error| error.to_string())?
        {
            Ok(Some(ExclusiveSandboxLease::start(self.clone(), claim)))
        } else {
            Ok(None)
        }
    }
}

#[allow(
    dead_code,
    reason = "consumed by the exclusive sandbox audit integration lane"
)]
pub(crate) struct ExclusiveSandboxLease {
    capacity: DaemonWorkerCapacity,
    claim: DaemonWorkerClaim,
    running: Arc<AtomicBool>,
    heartbeat: Option<JoinHandle<()>>,
}

impl ExclusiveSandboxLease {
    fn start(capacity: DaemonWorkerCapacity, claim: DaemonWorkerClaim) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let heartbeat_running = Arc::clone(&running);
        let heartbeat_capacity = capacity.clone();
        let heartbeat_claim = claim.clone();
        let interval =
            Duration::from_millis(capacity.lease_stale_seconds.saturating_mul(1_000).max(3) / 3);
        let heartbeat = thread::spawn(move || {
            while heartbeat_running.load(Ordering::Acquire) {
                thread::park_timeout(interval);
                if !heartbeat_running.load(Ordering::Acquire) {
                    break;
                }
                if heartbeat_capacity.heartbeat_existing(&heartbeat_claim).ok() != Some(true) {
                    break;
                }
            }
        });
        Self {
            capacity,
            claim,
            running,
            heartbeat: Some(heartbeat),
        }
    }
}

impl Drop for ExclusiveSandboxLease {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.thread().unpark();
            let _ = heartbeat.join();
        }
        let _ = self.capacity.release(&self.claim);
    }
}

impl DaemonWorkerClaim {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            DaemonWorkerKind::Queue => "queue",
            DaemonWorkerKind::Canary => "canary",
            DaemonWorkerKind::ExclusiveSandbox => "exclusive-sandbox",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, Priority, ValidationMode};

    #[test]
    fn queue_canary_and_exclusive_sandbox_contend_without_cross_release() {
        let temp = tempfile::tempdir().unwrap();
        let capacity = DaemonWorkerCapacity::new(temp.path());
        let queue = DaemonWorkerClaim::queue("same", "queue-authority");
        let canary = DaemonWorkerClaim::canary("same", "canary-authority");

        assert!(capacity.claim_or_heartbeat(&queue).unwrap());
        assert!(!capacity.claim_or_heartbeat(&canary).unwrap());
        assert!(!capacity.release(&canary).unwrap());
        assert!(capacity.claim_or_heartbeat(&queue).unwrap());
        assert!(capacity.release(&queue).unwrap());
        assert!(capacity.claim_or_heartbeat(&canary).unwrap());
        assert!(capacity.release(&canary).unwrap());
        let exclusive = capacity
            .claim_exclusive_sandbox_if_queue_idle(temp.path(), "audit", "audit-authority")
            .unwrap()
            .unwrap();
        assert!(!capacity.claim_or_heartbeat(&queue).unwrap());
        assert!(!capacity.claim_or_heartbeat(&canary).unwrap());
        drop(exclusive);
        assert!(capacity.claim_or_heartbeat(&queue).unwrap());
    }

    #[test]
    fn contradictory_restart_authority_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let capacity = DaemonWorkerCapacity::new(temp.path());
        assert!(
            capacity
                .claim_or_heartbeat(&DaemonWorkerClaim::queue("job", "sha-a"))
                .unwrap()
        );
        assert!(
            capacity
                .claim_or_heartbeat(&DaemonWorkerClaim::queue("job", "sha-b"))
                .is_err()
        );
    }

    #[test]
    fn exclusive_sandbox_refuses_a_pending_production_queue() {
        let temp = tempfile::tempdir().unwrap();
        Queue::new(temp.path())
            .unwrap()
            .enqueue(Job::create(
                "a".repeat(40),
                "main",
                vec!["macos".to_owned()],
                ValidationMode::Full,
                Priority::Normal,
            ))
            .unwrap();

        assert!(
            DaemonWorkerCapacity::new(temp.path())
                .claim_exclusive_sandbox_if_queue_idle(temp.path(), "audit", "audit-authority",)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn exclusive_sandbox_heartbeats_past_stale_window_and_then_releases() {
        let temp = tempfile::tempdir().unwrap();
        let capacity = DaemonWorkerCapacity::with_stale_seconds(temp.path(), 1);
        let exclusive = capacity
            .claim_exclusive_sandbox_if_queue_idle(temp.path(), "long-audit", "authority")
            .unwrap()
            .unwrap();
        std::thread::sleep(Duration::from_millis(1_300));
        assert!(
            !DaemonWorkerCapacity::new(temp.path())
                .claim_or_heartbeat(&DaemonWorkerClaim::queue("queued", "sha"))
                .unwrap()
        );
        drop(exclusive);
        assert!(
            DaemonWorkerCapacity::new(temp.path())
                .claim_or_heartbeat(&DaemonWorkerClaim::queue("queued", "sha"))
                .unwrap()
        );
    }
}
