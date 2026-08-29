//! Low-level OS process custody shared by daemon execution runtimes.
//!
//! This module deliberately owns no scheduler state, receipts, namespaces, or
//! lifecycle policy. Callers must prove their own typed authority before using
//! these primitives.

use std::io;
use std::process::{Child, Command, Stdio};
#[cfg(unix)]
use std::thread;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use wait_timeout::ChildExt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessLiveness {
    #[cfg_attr(
        windows,
        expect(
            dead_code,
            reason = "Windows process probing never proves a process alive"
        )
    )]
    Alive,
    #[cfg_attr(
        windows,
        expect(
            dead_code,
            reason = "Windows process probing never proves a process dead"
        )
    )]
    Dead,
    Unknown,
}

/// Capture the platform start identity for an exact live process.
#[cfg(unix)]
pub(crate) fn process_start_identity(pid: u32) -> io::Result<Option<Vec<u8>>> {
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .stdin(Stdio::null())
        .output()?;
    let value = String::from_utf8_lossy(&output.stdout)
        .trim()
        .as_bytes()
        .to_vec();
    if output.status.success() && !value.is_empty() {
        Ok(Some(value))
    } else if !output.status.success() && output.stderr.is_empty() {
        Ok(None)
    } else {
        Err(io::Error::other("process OS start identity is unavailable"))
    }
}

pub(crate) fn terminate_child_tree(child: &mut Child) -> io::Result<bool> {
    if child.try_wait()?.is_some() {
        #[cfg(unix)]
        return verify_exited_worker_group_dead(child.id());
        #[cfg(windows)]
        return Ok(false);
    }
    #[cfg_attr(
        windows,
        expect(
            unused_variables,
            reason = "Windows taskkill confirms the tree without exposing descendant PIDs"
        )
    )]
    let Ok(descendants) = signal_process_tree(child.id()) else {
        return Ok(false);
    };
    if child.wait_timeout(Duration::from_secs(5))?.is_some() {
        #[cfg(unix)]
        return Ok(descendants
            .iter()
            .all(|pid| process_id_liveness(*pid) == ProcessLiveness::Dead));
        #[cfg(windows)]
        return Ok(true);
    }
    let _ = child.kill();
    let root_dead = child.wait_timeout(Duration::from_secs(1))?.is_some();
    #[cfg(unix)]
    return Ok(root_dead
        && descendants
            .iter()
            .all(|pid| process_id_liveness(*pid) == ProcessLiveness::Dead));
    #[cfg(windows)]
    Ok(root_dead)
}

/// Terminate a detached daemon-owned worker tree by exact root identity.
#[cfg(unix)]
pub(crate) fn terminate_detached_worker_tree(pid: u32) -> io::Result<bool> {
    let descendants = match signal_process_tree(pid) {
        Ok(descendants) => descendants,
        Err(_error) if process_id_liveness(pid) == ProcessLiveness::Dead => Vec::new(),
        Err(error) => return Err(error),
    };
    let group_dead = terminate_process_group(pid)?;
    Ok(group_dead
        && descendants
            .iter()
            .all(|descendant| process_id_liveness(*descendant) == ProcessLiveness::Dead))
}

/// Terminate a daemon-owned process group even when its original leader has
/// already exited. Callers must authenticate the group identity before using
/// this primitive; a live leader with a mismatched birth identity is not safe
/// to signal.
#[cfg(unix)]
pub(crate) fn terminate_process_group(process_group: u32) -> io::Result<bool> {
    let status = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{process_group}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() && process_group_liveness(process_group)? == ProcessLiveness::Alive {
        return Err(io::Error::other("worker process group could not be killed"));
    }
    wait_for_process_group_death(process_group)
}

/// Observe whether a process group still has any non-zombie members.
#[cfg(unix)]
pub(crate) fn process_group_liveness(process_group: u32) -> io::Result<ProcessLiveness> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pgid=,stat="])
        .output()?;
    if !output.status.success() {
        return Ok(ProcessLiveness::Unknown);
    }
    let live = String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next().and_then(|value| value.parse::<u32>().ok()) == Some(process_group)
            && !fields.next().is_some_and(|state| state.starts_with('Z'))
    });
    Ok(if live {
        ProcessLiveness::Alive
    } else {
        ProcessLiveness::Dead
    })
}

fn signal_process_tree(pid: u32) -> io::Result<Vec<u32>> {
    #[cfg(unix)]
    {
        let stopped = Command::new("/bin/kill")
            .args(["-STOP", "--", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !stopped.success() {
            return Err(io::Error::other("worker root could not be stopped"));
        }
        let descendants = descendant_processes(pid)?;
        for descendant in descendants.iter().rev() {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &descendant.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let status = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !status?.success() {
            return Err(io::Error::other("worker process group could not be killed"));
        }
        Ok(descendants)
    }
    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(io::Error::other("worker process tree could not be killed"));
        }
        Ok(Vec::new())
    }
}

#[cfg(unix)]
fn verify_exited_worker_group_dead(process_group: u32) -> io::Result<bool> {
    terminate_process_group(process_group)
}

#[cfg(unix)]
fn wait_for_process_group_death(process_group: u32) -> io::Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if process_group_liveness(process_group)? == ProcessLiveness::Dead {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn descendant_processes(root: u32) -> io::Result<Vec<u32>> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid="])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("process-tree observation failed"));
    }
    let relations = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((
                fields.next()?.parse::<u32>().ok()?,
                fields.next()?.parse::<u32>().ok()?,
            ))
        })
        .collect::<Vec<_>>();
    let mut descendants = Vec::new();
    let mut parents = vec![root];
    while let Some(parent) = parents.pop() {
        for &(pid, observed_parent) in &relations {
            if observed_parent == parent && !descendants.contains(&pid) {
                descendants.push(pid);
                parents.push(pid);
            }
        }
    }
    Ok(descendants)
}

#[cfg(unix)]
pub(crate) fn process_id_liveness(pid: u32) -> ProcessLiveness {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
    else {
        return ProcessLiveness::Unknown;
    };
    if output.status.success() {
        let state = String::from_utf8_lossy(&output.stdout);
        if state.trim_start().starts_with('Z') || state.trim().is_empty() {
            ProcessLiveness::Dead
        } else {
            ProcessLiveness::Alive
        }
    } else if output.stderr.is_empty() {
        ProcessLiveness::Dead
    } else {
        ProcessLiveness::Unknown
    }
}
