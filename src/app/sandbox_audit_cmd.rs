use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitCode};

use crate::daemon_worker_capacity::DaemonWorkerCapacity;
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
    let Some(_exclusive_sandbox) = capacity
        .claim_exclusive_sandbox_if_queue_idle(state_dir, work_id, authority_sha)
        .map_err(|error| {
            CliFailure::new(
                WRITER_DOMAIN_OVERLAP_EXIT_CODE,
                format!(
                    "{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: could not prove exclusive sandbox admission: {error}"
                ),
            )
        })?
    else {
        return Err(CliFailure::new(
            WRITER_DOMAIN_OVERLAP_EXIT_CODE,
            format!(
                "{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: exclusive sandbox admission requires an idle production queue and free shared worker capacity"
            ),
        ));
    };
    let (program, args) = command
        .split_first()
        .ok_or_else(|| CliFailure::new(2, "sandbox audit command cannot be empty"))?;
    let status = Command::new(program).args(args).status().map_err(|error| {
        CliFailure::new(1, format!("could not run sandbox audit child: {error}"))
    })?;
    let code = if status.success() {
        0
    } else {
        status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .filter(|code| *code != 0)
            .unwrap_or(1)
    };
    Ok(ExitCode::from(code))
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::daemon_worker_capacity::{DaemonWorkerCapacity, DaemonWorkerClaim};
    use crate::job::{Job, Priority, ValidationMode};
    use crate::queue::Queue;

    use super::*;

    #[test]
    fn holds_capacity_for_full_child_lifetime() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ready = temp.path().join("ready");
        let release = temp.path().join("release");
        let script = temp.path().join("audit-child");
        std::fs::write(
            &script,
            "#!/bin/sh\nset -eu\ntouch \"$1\"\nwhile [ ! -e \"$2\" ]; do sleep 0.01; done\n",
        )
        .expect("script");
        let mut permissions = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod script");
        let state_dir = temp.path().to_path_buf();
        let command = vec![
            script.into_os_string(),
            ready.clone().into_os_string(),
            release.clone().into_os_string(),
        ];
        let worker = thread::spawn(move || {
            sandbox_audit_exec_command(&state_dir, "audit-1", "a1b2c3", &command)
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "sandbox child did not start");

        let capacity = DaemonWorkerCapacity::new(temp.path());
        let queue_claim = DaemonWorkerClaim::queue("queue-1", "d4e5f6");
        assert!(!capacity.claim_or_heartbeat(&queue_claim).expect("blocked"));
        std::fs::write(&release, b"release\n").expect("release child");
        assert_eq!(
            worker.join().expect("audit wrapper").expect("audit child"),
            ExitCode::SUCCESS
        );
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
        assert!(!invoked.exists(), "refused audit must not start its child");
    }
}
