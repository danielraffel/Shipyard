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
    let mut known = BTreeSet::new();
    let mut stable = 0_u8;
    while Instant::now() < deadline {
        let Some(observed) = sentinel_processes(path, deadline) else {
            return SentinelCleanup {
                proven: false,
                residual_detected: !known.is_empty(),
            };
        };
        let observed = observed
            .into_iter()
            .filter(|pid| *pid != std::process::id())
            .collect::<BTreeSet<_>>();
        if observed.is_empty() {
            return SentinelCleanup {
                proven: true,
                residual_detected: !known.is_empty(),
            };
        }
        let before = known.len();
        known.extend(observed);
        for pid in &known {
            let _ = signal(*pid, "-STOP");
        }
        if known.len() == before {
            stable += 1;
            if stable >= 2 {
                break;
            }
        } else {
            stable = 0;
        }
        std::thread::sleep(poll_interval.min(deadline.saturating_duration_since(Instant::now())));
    }
    for pid in known.iter().rev() {
        let _ = signal(*pid, "-KILL");
    }
    while Instant::now() < deadline {
        let Some(remaining) = sentinel_processes(path, deadline) else {
            return SentinelCleanup {
                proven: false,
                residual_detected: true,
            };
        };
        if remaining.into_iter().all(|pid| pid == std::process::id()) {
            return SentinelCleanup {
                proven: true,
                residual_detected: true,
            };
        }
        std::thread::sleep(poll_interval.min(deadline.saturating_duration_since(Instant::now())));
    }
    SentinelCleanup {
        proven: false,
        residual_detected: true,
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
        let descriptors = match std::fs::read_dir(entry.path().join("fd")) {
            Ok(descriptors) => descriptors,
            Err(_) => continue,
        };
        if descriptors
            .filter_map(Result::ok)
            .filter_map(|descriptor| std::fs::read_link(descriptor.path()).ok())
            .any(|target| target == expected)
        {
            pids.insert(pid);
        }
    }
    Some(pids)
}

#[cfg(target_os = "macos")]
fn sentinel_processes(path: &Path, deadline: Instant) -> Option<BTreeSet<u32>> {
    let mut command = Command::new("/usr/sbin/lsof");
    command.args(["-t", "--"]).arg(path);
    let output =
        crate::process::run_output_until(&mut command, deadline, "provider sentinel scan").ok()?;
    // lsof exits 1 when the file has no holders; its empty output is proof.
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::parse)
        .collect::<Result<BTreeSet<u32>, _>>()
        .ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sentinel_processes(_path: &Path, _deadline: Instant) -> Option<BTreeSet<u32>> {
    None
}
