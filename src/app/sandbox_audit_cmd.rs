use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::daemon_worker_capacity::{DaemonWorkerCapacity, ExclusiveSandboxAdmission};
use crate::parallel_proof::Sha256Digest;
use crate::writer_domain_lease::{
    WRITER_DOMAIN_OVERLAP_CLASSIFICATION, WRITER_DOMAIN_OVERLAP_EXIT_CODE,
};

use super::CliFailure;

pub(super) fn sandbox_audit_exec_command(
    state_dir: &Path,
    work_id: &str,
    authority_sha: &str,
    command: &[OsString],
) -> Result<ExitCode, CliFailure> {
    let capacity = DaemonWorkerCapacity::new(state_dir);
    let admission = capacity
        .claim_exclusive_sandbox_if_queue_idle(state_dir, work_id, authority_sha)
        .map_err(|error| {
            CliFailure::new(
                WRITER_DOMAIN_OVERLAP_EXIT_CODE,
                format!(
                    "{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: could not prove exclusive sandbox admission: {error}"
                ),
            )
        })?;
    let _exclusive_sandbox = match admission {
        ExclusiveSandboxAdmission::Acquired(lease) => lease,
        ExclusiveSandboxAdmission::Refused(refusal) => {
            return Err(CliFailure::new(
                WRITER_DOMAIN_OVERLAP_EXIT_CODE,
                format!(
                    "{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: exclusive sandbox admission refused: {}",
                    refusal.classification()
                ),
            ));
        }
    };
    let (program, args) = command
        .split_first()
        .ok_or_else(|| CliFailure::new(2, "sandbox audit command cannot be empty"))?;
    run_admitted_audit(&capacity, state_dir, work_id, authority_sha, program, args)
}

fn run_admitted_audit(
    capacity: &DaemonWorkerCapacity,
    state_dir: &Path,
    work_id: &str,
    authority_sha: &str,
    program: &OsString,
    args: &[OsString],
) -> Result<ExitCode, CliFailure> {
    let generation = process_generation(work_id, authority_sha)?;
    let mut worker_command = Command::new(std::env::current_exe().map_err(|error| {
        CliFailure::new(
            1,
            format!("could not resolve sandbox audit worker: {error}"),
        )
    })?);
    worker_command
        .arg("--state-dir")
        .arg(state_dir)
        .arg("sandbox-audit-exec")
        .arg("--work-id")
        .arg(work_id)
        .arg("--authority-sha")
        .arg(authority_sha)
        .arg("--worker-generation")
        .arg(&generation)
        .arg("--")
        .arg(program)
        .args(args)
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        worker_command.process_group(0);
    }
    let mut worker = worker_command.spawn().map_err(|error| {
        CliFailure::new(1, format!("could not launch sandbox audit worker: {error}"))
    })?;
    let pid = worker.id();
    let start_identity = capture_process_start_identity(pid).map_err(|error| {
        let _ = crate::worker_process_custody::terminate_child_tree(&mut worker);
        let _ = worker.wait();
        CliFailure::new(
            WRITER_DOMAIN_OVERLAP_EXIT_CODE,
            format!("{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: {error}"),
        )
    })?;
    if let Err(error) =
        capacity.bind_exclusive_process(work_id, authority_sha, &generation, pid, start_identity)
    {
        let _ = crate::worker_process_custody::terminate_child_tree(&mut worker);
        let _ = worker.wait();
        return Err(CliFailure::new(
            WRITER_DOMAIN_OVERLAP_EXIT_CODE,
            format!(
                "{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: could not bind sandbox process custody: {error}"
            ),
        ));
    }
    let status = loop {
        match worker.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if let Err(error) =
                    capacity.verify_exclusive_process(work_id, authority_sha, &generation, pid)
                {
                    let _ = crate::worker_process_custody::terminate_child_tree(&mut worker);
                    let _ = worker.wait();
                    let _ = capacity.clear_exclusive_process(work_id, authority_sha, &generation);
                    return Err(CliFailure::new(
                        WRITER_DOMAIN_OVERLAP_EXIT_CODE,
                        format!(
                            "{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: sandbox process custody was lost: {error}"
                        ),
                    ));
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                let _ = crate::worker_process_custody::terminate_child_tree(&mut worker);
                let _ = worker.wait();
                return Err(CliFailure::new(
                    WRITER_DOMAIN_OVERLAP_EXIT_CODE,
                    format!(
                        "{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: could not observe sandbox audit worker: {error}"
                    ),
                ));
            }
        }
    };
    capacity
        .clear_exclusive_process(work_id, authority_sha, &generation)
        .map_err(|error| {
            CliFailure::new(
                WRITER_DOMAIN_OVERLAP_EXIT_CODE,
                format!(
                    "{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: could not reconcile sandbox process custody: {error}"
                ),
            )
        })?;
    Ok(exit_code(status.code(), status.success()))
}

pub(super) fn sandbox_audit_worker_command(
    state_dir: &Path,
    work_id: &str,
    authority_sha: &str,
    generation: &str,
    command: &[OsString],
) -> Result<ExitCode, CliFailure> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match DaemonWorkerCapacity::new(state_dir).verify_exclusive_process(
            work_id,
            authority_sha,
            generation,
            std::process::id(),
        ) {
            Ok(()) => break,
            Err(error) if error.contains("receipt is missing") && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(CliFailure::new(
                    WRITER_DOMAIN_OVERLAP_EXIT_CODE,
                    format!(
                        "{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: sandbox worker authority refused: {error}"
                    ),
                ));
            }
        }
    }
    let (program, args) = command
        .split_first()
        .ok_or_else(|| CliFailure::new(2, "sandbox audit worker command cannot be empty"))?;
    let status = Command::new(program).args(args).status().map_err(|error| {
        CliFailure::new(1, format!("could not run sandbox audit child: {error}"))
    })?;
    Ok(exit_code(status.code(), status.success()))
}

fn exit_code(code: Option<i32>, success: bool) -> ExitCode {
    let code = if success {
        0
    } else {
        code.and_then(|code| u8::try_from(code).ok())
            .filter(|code| *code != 0)
            .unwrap_or(1)
    };
    ExitCode::from(code)
}

fn process_generation(work_id: &str, authority_sha: &str) -> Result<String, CliFailure> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CliFailure::new(1, "system clock predates UNIX epoch"))?;
    Ok(Sha256Digest::of_bytes(
        format!(
            "shipyard.sandbox-audit-process.v1\0{work_id}\0{authority_sha}\0{}\0{}",
            std::process::id(),
            elapsed.as_nanos()
        )
        .as_bytes(),
    )
    .as_str()
    .to_owned())
}

#[cfg(unix)]
fn capture_process_start_identity(pid: u32) -> Result<Sha256Digest, String> {
    crate::worker_process_custody::process_start_identity(pid)
        .map_err(|error| error.to_string())?
        .map(|bytes| Sha256Digest::of_bytes(&bytes))
        .ok_or_else(|| "sandbox audit worker exited before birth identity capture".to_owned())
}

#[cfg(not(unix))]
fn capture_process_start_identity(_pid: u32) -> Result<Sha256Digest, String> {
    Err("durable sandbox process custody is unsupported on this platform".to_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::CommandExt as _;

    use crate::daemon_worker_capacity::{
        DaemonWorkerCapacity, DaemonWorkerClaim, ExclusiveSandboxAdmission,
    };
    use crate::job::{Job, Priority, ValidationMode};
    use crate::queue::Queue;

    use super::*;

    #[test]
    fn durable_process_receipt_holds_capacity_for_full_child_lifetime() {
        let temp = tempfile::tempdir().expect("tempdir");
        let capacity = DaemonWorkerCapacity::new(temp.path());
        let exclusive = capacity
            .claim_exclusive_sandbox_if_queue_idle(temp.path(), "audit-1", "a1b2c3")
            .expect("admission");
        let ExclusiveSandboxAdmission::Acquired(exclusive) = exclusive else {
            panic!("exclusive capacity refused");
        };
        let mut command = Command::new("/bin/sleep");
        command.arg("30").process_group(0);
        let mut worker = command.spawn().expect("worker");
        let identity = capture_process_start_identity(worker.id()).expect("birth identity");
        capacity
            .bind_exclusive_process("audit-1", "a1b2c3", "generation-1", worker.id(), identity)
            .expect("bind process");
        let queue_claim = DaemonWorkerClaim::queue("queue-1", "d4e5f6");
        assert!(!capacity.claim_or_heartbeat(&queue_claim).expect("blocked"));
        crate::worker_process_custody::terminate_child_tree(&mut worker).expect("terminate");
        let _ = worker.wait();
        capacity
            .clear_exclusive_process("audit-1", "a1b2c3", "generation-1")
            .expect("clear process");
        drop(exclusive);
        assert!(capacity.claim_or_heartbeat(&queue_claim).expect("released"));
    }

    #[test]
    fn refuses_before_child_when_queue_is_pending() {
        let temp = tempfile::tempdir().expect("tempdir");
        Queue::new(temp.path())
            .expect("queue")
            .enqueue(Job::create(
                "a".repeat(40),
                "main",
                vec!["macos".to_owned()],
                ValidationMode::Full,
                Priority::Normal,
            ))
            .expect("pending job");
        let invoked = temp.path().join("invoked");
        let command = vec![
            OsString::from("/usr/bin/touch"),
            invoked.clone().into_os_string(),
        ];

        let error = sandbox_audit_exec_command(temp.path(), "audit-2", "a1b2c3", &command)
            .expect_err("pending queue must refuse audit");

        assert_eq!(error.code, WRITER_DOMAIN_OVERLAP_EXIT_CODE);
        assert!(error.message.contains(WRITER_DOMAIN_OVERLAP_CLASSIFICATION));
        assert!(error.message.contains("sandbox_queue_not_idle"));
        assert!(!invoked.exists(), "refused audit must not start its child");
    }
}
