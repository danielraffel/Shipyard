//! Cross-session cleanup proof for one provider-wrapper invocation.

#[cfg(target_os = "macos")]
use std::collections::BTreeSet;
#[cfg(target_os = "macos")]
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::{
    LinuxSentinelSupervisorSpecV3, LinuxSupervisorCleanupV3, LinuxSupervisorProviderV3,
    LinuxSupervisorResultV3, MAX_SPEC_BYTES, READY_FRAME, RESULT_FRAME_PREFIX,
    SPEC_ADMISSION_BUDGET, run_linux_sentinel_supervisor,
};

/// Result of proving that no process still holds the invocation sentinel.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SentinelCleanup {
    pub(super) proven: bool,
    pub(super) residual_detected: bool,
}

/// Kill every process that inherited this invocation's open sentinel.
///
/// Unlike a process group, the sentinel survives `setsid`, `setpgid`, fork,
/// and exec. The exact trusted wrapper contract forbids closing it or clearing
/// it in descendants. This lets Shipyard find a child even after the wrapper
/// parent exited and it was reparented by the OS.
#[cfg(target_os = "macos")]
pub(super) fn terminate_sentinel_processes(
    path: &Path,
    deadline: Instant,
    poll_interval: Duration,
) -> SentinelCleanup {
    terminate_sentinel_processes_with(
        deadline,
        poll_interval,
        |scan_deadline| sentinel_processes(path, scan_deadline),
        signal,
        Instant::now,
        std::thread::sleep,
    )
}

#[cfg(target_os = "macos")]
fn terminate_sentinel_processes_with<Scan, Kill, Now, Sleep>(
    deadline: Instant,
    poll_interval: Duration,
    mut scan: Scan,
    mut kill: Kill,
    mut now: Now,
    mut sleep: Sleep,
) -> SentinelCleanup
where
    Scan: FnMut(Instant) -> Option<BTreeSet<u32>>,
    Kill: FnMut(u32, &str, Instant) -> std::io::Result<()>,
    Now: FnMut() -> Instant,
    Sleep: FnMut(Duration),
{
    let started = now();
    let remaining = deadline.saturating_duration_since(started);
    let discovery_deadline = started + remaining / 2;
    let kill_deadline = started + (remaining / 4) * 3;
    let mut residual_detected = false;
    let mut discovery_succeeded = false;
    while now() < deadline {
        let current = now();
        let scan_deadline = if !discovery_succeeded {
            discovery_deadline
        } else if current < kill_deadline {
            kill_deadline
        } else {
            deadline
        };
        let Some(observed) = scan(scan_deadline) else {
            if (!discovery_succeeded && now() >= discovery_deadline) || now() >= deadline {
                return SentinelCleanup {
                    proven: false,
                    residual_detected,
                };
            }
            sleep(poll_interval.min(deadline.saturating_duration_since(now())));
            continue;
        };
        discovery_succeeded = true;
        let observed = observed
            .into_iter()
            .filter(|pid| *pid != std::process::id())
            .collect::<BTreeSet<_>>();
        if observed.is_empty() {
            return SentinelCleanup {
                proven: true,
                residual_detected,
            };
        }
        residual_detected = true;
        if now() >= kill_deadline {
            return SentinelCleanup {
                proven: false,
                residual_detected,
            };
        }
        // Never leave a durable stopped process between discovery and
        // termination. A supervisor abort in the former STOP-then-KILL gap
        // orphaned wrappers under launchd indefinitely. Killing every exact
        // sentinel holder immediately and rescanning still closes the fork
        // race: any child created before its parent dies inherited this same
        // private descriptor and appears on the next pass.
        for pid in observed.iter().rev() {
            if now() >= kill_deadline {
                return SentinelCleanup {
                    proven: false,
                    residual_detected,
                };
            }
            let _ = kill(*pid, "-KILL", kill_deadline);
        }
        sleep(poll_interval.min(deadline.saturating_duration_since(now())));
    }
    SentinelCleanup {
        proven: false,
        residual_detected,
    }
}

#[cfg(target_os = "macos")]
fn signal(pid: u32, signal: &str, deadline: Instant) -> std::io::Result<()> {
    let mut command = Command::new("/bin/kill");
    command
        .args([signal, "--", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let output = crate::process::run_output_until(&mut command, deadline, "provider sentinel kill")
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    output
        .status
        .success()
        .then_some(())
        .ok_or_else(|| std::io::Error::other(format!("could not signal process {pid}")))
}

#[cfg(target_os = "macos")]
fn sentinel_processes(path: &Path, deadline: Instant) -> Option<BTreeSet<u32>> {
    let mut command = Command::new("/usr/sbin/lsof");
    // The trusted launcher opens the private sentinel as descriptor 9 and its
    // descendants must retain that descriptor. Intersecting the pathname and
    // descriptor selections reduces work on build hosts with large FD tables.
    command.args(["-a", "-d", "9", "-t", "--"]).arg(path);
    let output =
        crate::process::run_output_until(&mut command, deadline, "provider sentinel scan").ok()?;
    parse_lsof_output(output.status.code(), &output.stdout, &output.stderr)
}

#[cfg(target_os = "macos")]
fn parse_lsof_output(
    status_code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> Option<BTreeSet<u32>> {
    if status_code == Some(1) && stdout.is_empty() && stderr.is_empty() {
        return Some(BTreeSet::new());
    }
    if status_code != Some(0) || stdout.is_empty() || !stderr.is_empty() {
        return None;
    }
    let body = stdout.strip_suffix(b"\n")?;
    if body.is_empty() {
        return None;
    }
    let mut pids = BTreeSet::new();
    for line in body.split(|byte| *byte == b'\n') {
        if line.is_empty() || line[0] == b'0' || !line.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let pid = std::str::from_utf8(line).ok()?.parse::<u32>().ok()?;
        if pid == 0 || pid.to_string().as_bytes() != line || !pids.insert(pid) {
            return None;
        }
    }
    Some(pids)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::cell::Cell;
    use std::collections::{BTreeSet, VecDeque};
    use std::time::{Duration, Instant};

    use super::{parse_lsof_output, terminate_sentinel_processes_with};

    #[test]
    fn transient_scan_failures_retry_across_kill_and_final_verification() {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(100);
        let discovery_deadline = started + Duration::from_millis(50);
        let kill_deadline = started + Duration::from_millis(75);
        let clock = Cell::new(started);
        let mut scans = VecDeque::from([None, Some(BTreeSet::from([42])), None, None]);
        let mut scan_deadlines = Vec::new();
        let mut killed = Vec::new();
        let cleanup = terminate_sentinel_processes_with(
            deadline,
            Duration::from_millis(10),
            |scan_deadline| {
                scan_deadlines.push(scan_deadline);
                let result = scans.pop_front().unwrap_or_else(|| Some(BTreeSet::new()));
                if scan_deadlines.len() == 2 {
                    clock.set(started + Duration::from_millis(49));
                } else if scan_deadlines.len() == 4 {
                    clock.set(started + Duration::from_millis(76));
                }
                result
            },
            |pid, signal, signal_deadline| {
                killed.push((pid, signal.to_owned(), signal_deadline));
                Ok(())
            },
            || clock.get(),
            |duration| clock.set(clock.get() + duration),
        );

        assert!(cleanup.proven);
        assert!(cleanup.residual_detected);
        assert_eq!(
            scan_deadlines,
            [
                discovery_deadline,
                discovery_deadline,
                kill_deadline,
                kill_deadline,
                deadline,
            ]
        );
        assert_eq!(killed, [(42, "-KILL".to_owned(), kill_deadline)]);
    }

    #[test]
    fn discovery_exhaustion_refuses_without_kill() {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(100);
        let discovery_deadline = started + Duration::from_millis(50);
        let clock = Cell::new(started);
        let mut scan_deadlines = Vec::new();
        let mut kills = 0;
        let cleanup = terminate_sentinel_processes_with(
            deadline,
            Duration::from_millis(10),
            |scan_deadline| {
                scan_deadlines.push(scan_deadline);
                clock.set(started + Duration::from_millis(51));
                None
            },
            |_, _, _| {
                kills += 1;
                Ok(())
            },
            || clock.get(),
            |duration| clock.set(clock.get() + duration),
        );

        assert!(!cleanup.proven);
        assert!(!cleanup.residual_detected);
        assert_eq!(scan_deadlines, [discovery_deadline]);
        assert_eq!(kills, 0);
    }

    #[test]
    fn lsof_output_accepts_only_exact_holder_or_no_match_shapes() {
        assert_eq!(
            parse_lsof_output(Some(0), b"42\n7\n", b"")
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            [7, 42]
        );
        assert!(parse_lsof_output(Some(1), b"", b"").unwrap().is_empty());

        for (status, stdout, stderr) in [
            (Some(0), b"".as_slice(), b"".as_slice()),
            (Some(0), b"\n".as_slice(), b"".as_slice()),
            (Some(0), b"0\n".as_slice(), b"".as_slice()),
            (Some(0), b"042\n".as_slice(), b"".as_slice()),
            (Some(0), b" 42\n".as_slice(), b"".as_slice()),
            (Some(0), b"42 \n".as_slice(), b"".as_slice()),
            (Some(0), b"42\n\n".as_slice(), b"".as_slice()),
            (Some(0), b"42".as_slice(), b"".as_slice()),
            (Some(0), b"42\n42\n".as_slice(), b"".as_slice()),
            (Some(0), b"invalid\n".as_slice(), b"".as_slice()),
            (Some(0), b"42\n".as_slice(), b"warning".as_slice()),
            (Some(1), b"42\n".as_slice(), b"".as_slice()),
            (Some(1), b"".as_slice(), b"error".as_slice()),
            (Some(2), b"".as_slice(), b"".as_slice()),
            (None, b"".as_slice(), b"".as_slice()),
        ] {
            assert!(
                parse_lsof_output(status, stdout, stderr).is_none(),
                "accepted status={status:?}, stdout={stdout:?}, stderr={stderr:?}"
            );
        }
    }
}
