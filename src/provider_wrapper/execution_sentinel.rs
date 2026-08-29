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
            if Instant::now() >= deadline {
                return SentinelCleanup {
                    proven: false,
                    residual_detected,
                };
            }
            let _ = signal(*pid, "-KILL", deadline);
        }
        std::thread::sleep(poll_interval.min(deadline.saturating_duration_since(Instant::now())));
    }
    SentinelCleanup {
        proven: false,
        residual_detected,
    }
}

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

#[cfg(target_os = "linux")]
fn sentinel_processes(path: &Path, deadline: Instant) -> Option<BTreeSet<u32>> {
    use std::os::unix::fs::MetadataExt;

    if Instant::now() >= deadline {
        return None;
    }
    let expected = path.canonicalize().ok()?;
    let current_uid = std::fs::metadata("/proc/self").ok()?.uid();
    let mut pids = BTreeSet::new();
    for entry in std::fs::read_dir("/proc").ok()? {
        if Instant::now() >= deadline {
            return None;
        }
        let entry = entry.ok()?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let process_uid = match entry.metadata() {
            Ok(metadata) => metadata.uid(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let same_uid = process_uid == current_uid;
        let descriptors = match std::fs::read_dir(entry.path().join("fd")) {
            Ok(descriptors) => descriptors,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound || !same_uid => continue,
            Err(_) => return None,
        };
        for descriptor in descriptors {
            if Instant::now() >= deadline {
                return None;
            }
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
    (Instant::now() < deadline).then_some(pids)
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
