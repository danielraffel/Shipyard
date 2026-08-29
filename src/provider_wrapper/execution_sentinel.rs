//! Cross-session cleanup proof for one provider-wrapper invocation.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Result of proving that no process still holds the invocation sentinel.
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
pub(super) fn terminate_sentinel_processes(
    path: &Path,
    deadline: Instant,
    poll_interval: Duration,
) -> SentinelCleanup {
    let mut residual_detected = false;
    while Instant::now() < deadline {
        let Some(observed) = sentinel_processes(path, deadline) else {
            return SentinelCleanup {
                proven: false,
                residual_detected,
            };
        };
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
        // Never leave a durable stopped process between discovery and
        // termination. A supervisor abort in the former STOP-then-KILL gap
        // orphaned wrappers under launchd indefinitely. Killing every exact
        // sentinel holder immediately and rescanning still closes the fork
        // race: any child created before its parent dies inherited this same
        // private descriptor and appears on the next pass.
        for pid in observed.iter().rev() {
            let _ = signal(*pid, "-KILL");
        }
        std::thread::sleep(poll_interval.min(deadline.saturating_duration_since(Instant::now())));
    }
    SentinelCleanup {
        proven: false,
        residual_detected,
    }
}

fn signal(pid: u32, signal: &str) -> std::io::Result<()> {
    let status = Command::new("/bin/kill")
        .args([signal, "--", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| std::io::Error::other(format!("could not signal process {pid}")))
}

#[cfg(target_os = "linux")]
fn sentinel_processes(path: &Path, _deadline: Instant) -> Option<BTreeSet<u32>> {
    let expected = path.canonicalize().ok()?;
    let current_uid = linux_effective_uid(&std::fs::read_to_string("/proc/self/status").ok()?)?;
    let mut pids = BTreeSet::new();
    for entry in std::fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let status = match std::fs::read_to_string(entry.path().join("status")) {
            Ok(status) => status,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let process_uid = linux_effective_uid(&status)?;
        let same_uid = process_uid == current_uid;
        let descriptors = match std::fs::read_dir(entry.path().join("fd")) {
            Ok(descriptors) => descriptors,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound || !same_uid => continue,
            Err(_) => return None,
        };
        for descriptor in descriptors {
            let descriptor = match descriptor {
                Ok(descriptor) => descriptor,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound || !same_uid => continue,
                Err(_) => return None,
            };
            match std::fs::read_link(descriptor.path()) {
                Ok(target) if target == expected => {
                    pids.insert(pid);
                    break;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound || !same_uid => {}
                Err(_) => return None,
            }
        }
    }
    Some(pids)
}

#[cfg(target_os = "linux")]
fn linux_effective_uid(status: &str) -> Option<u32> {
    let mut values = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace();
    let _real = values.next()?.parse::<u32>().ok()?;
    let effective = values.next()?.parse::<u32>().ok()?;
    let _saved = values.next()?.parse::<u32>().ok()?;
    let _filesystem = values.next()?.parse::<u32>().ok()?;
    values.next().is_none().then_some(effective)
}

#[cfg(target_os = "macos")]
fn sentinel_processes(path: &Path, deadline: Instant) -> Option<BTreeSet<u32>> {
    let mut command = Command::new("/usr/sbin/lsof");
    command.args(["-t", "--"]).arg(path);
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
    let pids = std::str::from_utf8(stdout)
        .ok()?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::parse)
        .collect::<Result<BTreeSet<u32>, _>>()
        .ok()?;
    (!pids.is_empty() && pids.iter().all(|pid| *pid != 0)).then_some(pids)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sentinel_processes(_path: &Path, _deadline: Instant) -> Option<BTreeSet<u32>> {
    None
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::parse_lsof_output;

    #[test]
    fn lsof_output_accepts_only_exact_holder_or_no_match_shapes() {
        assert_eq!(
            parse_lsof_output(Some(0), b"42\n7\n42\n", b"")
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::linux_effective_uid;

    #[test]
    fn proc_status_requires_one_complete_uid_row() {
        assert_eq!(
            linux_effective_uid("Name:\twrapper\nUid:\t501\t502\t503\t504\n"),
            Some(502)
        );
        for status in [
            "Name:\twrapper\n",
            "Uid:\t501\t502\t503\n",
            "Uid:\t501\t502\t503\t504\t505\n",
            "Uid:\t501\tinvalid\t503\t504\n",
        ] {
            assert_eq!(linux_effective_uid(status), None);
        }
    }
}
