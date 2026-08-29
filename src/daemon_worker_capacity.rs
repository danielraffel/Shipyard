//! Shared singleton native-worker admission for daemon execution runtimes.
//!
//! Durable queue and canary custody remain independent. This module shares
//! only the host-capacity fence that must be held before either runtime may
//! launch a native worker.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::path::PathBuf;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::host_pool::{HostPoolLeaseError, HostPoolLeaseRequest, HostPoolLeaseStore};
use crate::parallel_proof::Sha256Digest;
use crate::queue::Queue;
use crate::worker_process_custody::{ProcessLiveness, process_id_liveness};
#[cfg(unix)]
use crate::worker_process_custody::{
    process_group_liveness, process_start_identity, terminate_process_group,
};

const POOL: &str = "shipyard-daemon-native-worker";
const MEMBER: &str = "local-daemon";
const LEASE_STALE_SECONDS: u64 = 30;
const EXCLUSIVE_PROCESS_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ExclusiveSandboxProcessReceipt {
    schema_version: u32,
    work_id: String,
    authority_sha: String,
    generation: String,
    pid: u32,
    process_group: u32,
    os_start_identity_sha256: Sha256Digest,
}

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
    root: PathBuf,
    lease_stale_seconds: u64,
}

impl DaemonWorkerCapacity {
    pub(crate) fn new(state_dir: &Path) -> Self {
        let root = state_dir.join("daemon-worker-capacity");
        Self {
            leases: HostPoolLeaseStore::new(root.join("leases.json")),
            root,
            lease_stale_seconds: LEASE_STALE_SECONDS,
        }
    }

    #[cfg(test)]
    fn with_stale_seconds(state_dir: &Path, lease_stale_seconds: u64) -> Self {
        let root = state_dir.join("daemon-worker-capacity");
        Self {
            leases: HostPoolLeaseStore::new(root.join("leases.json")),
            root,
            lease_stale_seconds,
        }
    }

    pub(crate) fn claim_or_heartbeat(
        &self,
        claim: &DaemonWorkerClaim,
    ) -> Result<bool, HostPoolLeaseError> {
        self.with_control_lock(|| {
            if self.reconcile_exclusive_process()? {
                return Ok(false);
            }
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
        })
    }

    pub(crate) fn heartbeat_existing(
        &self,
        claim: &DaemonWorkerClaim,
    ) -> Result<bool, HostPoolLeaseError> {
        self.with_control_lock(|| {
            if self.reconcile_exclusive_process()? {
                return Ok(false);
            }
            self.leases.heartbeat_existing_job(
                POOL,
                MEMBER,
                &claim.lease_job_id(),
                self.lease_stale_seconds,
            )
        })
    }

    pub(crate) fn release_inactive_queue_claims(
        &self,
        active_work_ids: &BTreeSet<String>,
    ) -> Result<(), HostPoolLeaseError> {
        self.with_control_lock(|| {
            if self.reconcile_exclusive_process()? {
                return Ok(());
            }
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
        })
    }

    pub(crate) fn release(&self, claim: &DaemonWorkerClaim) -> Result<bool, HostPoolLeaseError> {
        self.with_control_lock(|| {
            if self.reconcile_exclusive_process()? {
                return Ok(false);
            }
            let logical_job_id = claim.lease_job_id();
            let exact = self.leases.leases()?.into_iter().find(|lease| {
                lease.pool_name == POOL
                    && lease.member_id == MEMBER
                    && lease.job_id.as_deref() == Some(logical_job_id.as_str())
                    && lease.branch == claim.kind_label()
                    && lease.sha == claim.authority_sha
            });
            exact.map_or(Ok(false), |lease| self.leases.release(&lease.lease_id))
        })
    }

    pub(crate) fn release_queue_work(&self, work_id: &str) -> Result<bool, HostPoolLeaseError> {
        self.with_control_lock(|| {
            if self.reconcile_exclusive_process()? {
                return Ok(false);
            }
            self.leases
                .release_for_job(&DaemonWorkerClaim::queue(work_id, "").lease_job_id())
                .map(|removed| removed != 0)
        })
    }

    pub(crate) fn bind_exclusive_process(
        &self,
        work_id: &str,
        authority_sha: &str,
        generation: &str,
        pid: u32,
        os_start_identity_sha256: Sha256Digest,
    ) -> Result<(), String> {
        self.with_control_lock(|| {
            if self.reconcile_exclusive_process()? {
                return Err(HostPoolLeaseError::Io(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another exclusive sandbox process retains custody",
                )));
            }
            let claim = DaemonWorkerClaim::exclusive_sandbox(work_id, authority_sha);
            let exact_lease = self.leases.leases()?.into_iter().any(|lease| {
                lease.pool_name == POOL
                    && lease.member_id == MEMBER
                    && lease.job_id.as_deref() == Some(claim.lease_job_id().as_str())
                    && lease.branch == claim.kind_label()
                    && lease.sha == claim.authority_sha
            });
            if !exact_lease {
                return Err(HostPoolLeaseError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "exclusive sandbox process has no exact capacity lease",
                )));
            }
            self.write_exclusive_process(&ExclusiveSandboxProcessReceipt {
                schema_version: EXCLUSIVE_PROCESS_SCHEMA_VERSION,
                work_id: work_id.to_owned(),
                authority_sha: authority_sha.to_owned(),
                generation: generation.to_owned(),
                pid,
                process_group: pid,
                os_start_identity_sha256,
            })?;
            Ok(())
        })
        .map_err(|error| error.to_string())
    }

    pub(crate) fn verify_exclusive_process(
        &self,
        work_id: &str,
        authority_sha: &str,
        generation: &str,
        pid: u32,
    ) -> Result<(), String> {
        let receipt = self
            .read_exclusive_process()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "exclusive sandbox process receipt is missing".to_owned())?;
        if receipt.schema_version != EXCLUSIVE_PROCESS_SCHEMA_VERSION
            || receipt.work_id != work_id
            || receipt.authority_sha != authority_sha
            || receipt.generation != generation
            || receipt.pid != pid
            || receipt.process_group != pid
        {
            return Err("exclusive sandbox process receipt authority mismatch".to_owned());
        }
        Self::verify_process_identity(&receipt).map_err(|error| error.to_string())
    }

    pub(crate) fn clear_exclusive_process(
        &self,
        work_id: &str,
        authority_sha: &str,
        generation: &str,
    ) -> Result<(), String> {
        self.with_control_lock(|| {
            let Some(receipt) = self.read_exclusive_process()? else {
                return Ok(());
            };
            if receipt.work_id != work_id
                || receipt.authority_sha != authority_sha
                || receipt.generation != generation
            {
                return Err(HostPoolLeaseError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "exclusive sandbox process clear authority mismatch",
                )));
            }
            Self::reap_exclusive_process(&receipt)?;
            self.remove_exclusive_process()
        })
        .map_err(|error| error.to_string())
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

    fn control_lock_path(&self) -> PathBuf {
        self.root.join("control.lock")
    }

    fn exclusive_process_path(&self) -> PathBuf {
        self.root.join("exclusive-sandbox-process.json")
    }

    fn with_control_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, HostPoolLeaseError>,
    ) -> Result<T, HostPoolLeaseError> {
        crate::writer_domain_lease::ensure_protected_dir_all(&self.root)?;
        let path = self.control_lock_path();
        let creation_lease = crate::writer_domain_lease::acquire_for_protected_creation(&path)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        drop(creation_lease);
        FileExt::lock_exclusive(&lock)?;
        let result = operation();
        let unlock = FileExt::unlock(&lock);
        match (result, unlock) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
        }
    }

    fn read_exclusive_process(
        &self,
    ) -> Result<Option<ExclusiveSandboxProcessReceipt>, HostPoolLeaseError> {
        match fs::read(self.exclusive_process_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(Into::into),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn write_exclusive_process(
        &self,
        receipt: &ExclusiveSandboxProcessReceipt,
    ) -> Result<(), HostPoolLeaseError> {
        let path = self.exclusive_process_path();
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&path)?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer_pretty(&mut temp, receipt)?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    fn remove_exclusive_process(&self) -> Result<(), HostPoolLeaseError> {
        let path = self.exclusive_process_path();
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&path)?;
        match fs::remove_file(path) {
            Ok(()) => {
                #[cfg(unix)]
                File::open(&self.root)?.sync_all()?;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn reconcile_exclusive_process(&self) -> Result<bool, HostPoolLeaseError> {
        let Some(receipt) = self.read_exclusive_process()? else {
            return Ok(false);
        };
        if receipt.schema_version != EXCLUSIVE_PROCESS_SCHEMA_VERSION
            || receipt.work_id.is_empty()
            || receipt.authority_sha.is_empty()
            || receipt.generation.is_empty()
            || receipt.pid == 0
            || receipt.process_group != receipt.pid
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exclusive sandbox process receipt is invalid",
            )
            .into());
        }
        match process_id_liveness(receipt.pid) {
            ProcessLiveness::Alive => {
                Self::verify_process_identity(&receipt)?;
                Ok(true)
            }
            ProcessLiveness::Dead => {
                Self::reap_exclusive_process(&receipt)?;
                self.leases.release_for_job(
                    &DaemonWorkerClaim::exclusive_sandbox(&receipt.work_id, &receipt.authority_sha)
                        .lease_job_id(),
                )?;
                self.remove_exclusive_process()?;
                Ok(false)
            }
            ProcessLiveness::Unknown => {
                Err(io::Error::other("exclusive sandbox process liveness is unknown").into())
            }
        }
    }

    #[cfg(unix)]
    fn verify_process_identity(
        receipt: &ExclusiveSandboxProcessReceipt,
    ) -> Result<(), HostPoolLeaseError> {
        let identity = process_start_identity(receipt.pid)?
            .ok_or_else(|| io::Error::other("exclusive sandbox process disappeared"))?;
        if Sha256Digest::of_bytes(&identity) != receipt.os_start_identity_sha256 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "exclusive sandbox process birth identity mismatch",
            )
            .into());
        }
        if process_group_liveness(receipt.process_group)? != ProcessLiveness::Alive {
            return Err(io::Error::other("exclusive sandbox process group is not alive").into());
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn verify_process_identity(
        _receipt: &ExclusiveSandboxProcessReceipt,
    ) -> Result<(), HostPoolLeaseError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable exclusive sandbox process identity is unsupported on this platform",
        )
        .into())
    }

    #[cfg(unix)]
    fn reap_exclusive_process(
        receipt: &ExclusiveSandboxProcessReceipt,
    ) -> Result<(), HostPoolLeaseError> {
        match process_id_liveness(receipt.pid) {
            ProcessLiveness::Alive => Self::verify_process_identity(receipt)?,
            ProcessLiveness::Dead => {}
            ProcessLiveness::Unknown => {
                return Err(io::Error::other(
                    "exclusive sandbox process liveness is unknown during reap",
                )
                .into());
            }
        }
        match process_group_liveness(receipt.process_group)? {
            ProcessLiveness::Dead => Ok(()),
            ProcessLiveness::Alive => {
                if terminate_process_group(receipt.process_group)? {
                    Ok(())
                } else {
                    Err(io::Error::other("exclusive sandbox process group survived reap").into())
                }
            }
            ProcessLiveness::Unknown => Err(io::Error::other(
                "exclusive sandbox process group liveness is unknown during reap",
            )
            .into()),
        }
    }

    #[cfg(not(unix))]
    fn reap_exclusive_process(
        _receipt: &ExclusiveSandboxProcessReceipt,
    ) -> Result<(), HostPoolLeaseError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable exclusive sandbox process reaping is unsupported on this platform",
        )
        .into())
    }
}

#[allow(
    dead_code,
    reason = "consumed by the exclusive sandbox audit integration lane"
)]
pub(crate) struct ExclusiveSandboxLease {
    capacity: DaemonWorkerCapacity,
    claim: DaemonWorkerClaim,
}

impl ExclusiveSandboxLease {
    fn start(capacity: DaemonWorkerCapacity, claim: DaemonWorkerClaim) -> Self {
        Self { capacity, claim }
    }
}

impl Drop for ExclusiveSandboxLease {
    fn drop(&mut self) {
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
    use std::time::Duration;

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
    #[cfg(unix)]
    fn exclusive_sandbox_process_custody_outlives_capacity_lease_window() {
        use std::os::unix::process::CommandExt as _;

        let temp = tempfile::tempdir().unwrap();
        let capacity = DaemonWorkerCapacity::with_stale_seconds(temp.path(), 1);
        let exclusive = capacity
            .claim_exclusive_sandbox_if_queue_idle(temp.path(), "long-audit", "authority")
            .unwrap()
            .unwrap();
        let mut command = std::process::Command::new("/bin/sleep");
        command.arg("30").process_group(0);
        let mut child = command.spawn().unwrap();
        let identity = process_start_identity(child.id()).unwrap().unwrap();
        capacity
            .bind_exclusive_process(
                "long-audit",
                "authority",
                "generation",
                child.id(),
                Sha256Digest::of_bytes(&identity),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(1_300));
        assert!(
            !DaemonWorkerCapacity::new(temp.path())
                .claim_or_heartbeat(&DaemonWorkerClaim::queue("queued", "sha"))
                .unwrap()
        );
        crate::worker_process_custody::terminate_child_tree(&mut child).unwrap();
        let _ = child.wait();
        capacity
            .clear_exclusive_process("long-audit", "authority", "generation")
            .unwrap();
        drop(exclusive);
        assert!(
            DaemonWorkerCapacity::new(temp.path())
                .claim_or_heartbeat(&DaemonWorkerClaim::queue("queued", "sha"))
                .unwrap()
        );
    }

    #[test]
    #[cfg(unix)]
    fn crashed_audit_wrapper_reaps_its_surviving_process_group_before_admission() {
        use std::os::unix::process::CommandExt as _;

        let temp = tempfile::tempdir().unwrap();
        let capacity = DaemonWorkerCapacity::with_stale_seconds(temp.path(), 1);
        let exclusive = capacity
            .claim_exclusive_sandbox_if_queue_idle(temp.path(), "crash-audit", "authority")
            .unwrap()
            .unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "sleep 0.2; sleep 30 & exit 0"])
            .process_group(0);
        let mut wrapper = command.spawn().unwrap();
        let wrapper_pid = wrapper.id();
        let identity = process_start_identity(wrapper_pid).unwrap().unwrap();
        capacity
            .bind_exclusive_process(
                "crash-audit",
                "authority",
                "generation",
                wrapper_pid,
                Sha256Digest::of_bytes(&identity),
            )
            .unwrap();
        assert!(wrapper.wait().unwrap().success());
        assert_eq!(
            process_group_liveness(wrapper_pid).unwrap(),
            ProcessLiveness::Alive,
            "the crash fixture must leave a surviving child group"
        );
        std::mem::forget(exclusive);

        assert!(
            capacity
                .claim_or_heartbeat(&DaemonWorkerClaim::queue("queued-after-crash", "sha"))
                .unwrap(),
            "queue admission must occur only after exact orphan group reaping"
        );
        assert_eq!(
            process_group_liveness(wrapper_pid).unwrap(),
            ProcessLiveness::Dead
        );
    }

    #[test]
    fn malformed_audit_process_receipt_fails_closed_without_capacity_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let capacity = DaemonWorkerCapacity::new(temp.path());
        crate::writer_domain_lease::ensure_protected_dir_all(&capacity.root).unwrap();
        std::fs::write(capacity.exclusive_process_path(), b"{}\n").unwrap();

        assert!(
            capacity
                .claim_or_heartbeat(&DaemonWorkerClaim::queue("queue", "sha"))
                .is_err()
        );
        assert!(capacity.leases.leases().unwrap().is_empty());
    }

    #[test]
    fn expired_guard_authority_cannot_release_a_replacement_generation() {
        let temp = tempfile::tempdir().unwrap();
        let capacity = DaemonWorkerCapacity::with_stale_seconds(temp.path(), 1);
        let old = DaemonWorkerClaim::exclusive_sandbox("audit", "authority-old");
        assert!(capacity.claim_or_heartbeat(&old).unwrap());
        std::thread::sleep(Duration::from_millis(1_100));
        let replacement = DaemonWorkerClaim::exclusive_sandbox("audit", "authority-new");
        assert!(capacity.claim_or_heartbeat(&replacement).unwrap());
        assert!(!capacity.release(&old).unwrap());
        assert!(capacity.heartbeat_existing(&replacement).unwrap());
    }
}
