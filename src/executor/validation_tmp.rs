//! Ownership and reclamation of validation TMPDIRs.
//!
//! A local validation run gives its children a private `shipyard-validation-*`
//! TMPDIR under the system temp root and removes it when the run ends. A run
//! that is killed (SIGKILL, a supervisor timeout, a reboot) never reaches that
//! removal, and test suites fill these directories with build trees, so a few
//! killed runs leave tens of GiB on the boot volume.
//!
//! Each directory therefore gets a sibling owner file naming the process that
//! created it. A directory is reclaimable only when it is older than the
//! policy's minimum age and nothing can still be using it: for an owned
//! directory, its owner process is gone; for a directory from a build that
//! wrote no owner file, no live process has it as its `TMPDIR`. Anything that
//! cannot be established (an unreadable owner file, a failed process listing)
//! keeps the directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde::Serialize;

/// Directory-name prefix every validation TMPDIR carries.
pub const PREFIX: &str = "shipyard-validation-";
/// Suffix of the owner file written next to a validation TMPDIR.
pub const OWNER_SUFFIX: &str = ".owner";
/// Minimum age before a run sweeps a dead owner's directory on its own.
pub const AUTOMATIC_MIN_AGE: Duration = Duration::from_hours(6);

/// The owner file for `dir`: a sibling, so the TMPDIR itself stays exactly
/// what the validation run's children see.
#[must_use]
pub fn owner_path(dir: &Path) -> PathBuf {
    let mut name = dir.as_os_str().to_owned();
    name.push(OWNER_SUFFIX);
    PathBuf::from(name)
}

/// Record the current process as the owner of `dir`.
///
/// # Errors
/// Returns the write error; callers treat a missing owner file as "owner
/// unknown", which only ever keeps the directory longer.
pub fn write_owner(dir: &Path) -> std::io::Result<()> {
    fs::write(owner_path(dir), format!("{}\n", std::process::id()))
}

/// Remove `dir`'s owner file. Best effort.
pub fn remove_owner(dir: &Path) {
    let _ = fs::remove_file(owner_path(dir));
}

/// What a reclaim may delete.
#[derive(Clone, Copy, Debug)]
pub struct ReclaimPolicy {
    /// Directories younger than this are always kept.
    pub min_age: Duration,
    /// Also consider directories with no owner file (written by builds before
    /// owner files existed). Those need a process-environment scan.
    pub include_unowned: bool,
}

/// How a directory's users are checked.
pub trait Liveness {
    /// Whether process `pid` still exists.
    fn pid_alive(&self, pid: u32) -> bool;
    /// TMPDIR values of every live process this user can see, or `None` when
    /// the listing failed (every unowned directory is then kept).
    fn live_tmpdirs(&self) -> Option<Vec<String>>;
}

/// Liveness from the running system.
pub struct SystemLiveness;

impl Liveness for SystemLiveness {
    fn pid_alive(&self, pid: u32) -> bool {
        pid_alive(pid)
    }

    fn live_tmpdirs(&self) -> Option<Vec<String>> {
        // `ps eww` appends each same-user process's environment to its command.
        let output = Command::new("ps")
            .args(["eww", "-ax", "-o", "command="])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Some(
            text.split_whitespace()
                .filter_map(|word| word.strip_prefix("TMPDIR="))
                .map(|value| value.trim_end_matches('/').to_owned())
                .collect(),
        )
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return true;
    };
    !matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

/// One directory a reclaim would delete.
#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    /// The validation TMPDIR.
    pub path: PathBuf,
    /// Bytes it holds.
    pub bytes: u64,
    /// Hours since it last changed.
    pub age_hours: u64,
    /// Why nothing can still be using it.
    pub reason: String,
}

/// The validation TMPDIRs under `base` that `policy` allows deleting.
#[must_use]
pub fn plan(
    base: &Path,
    now: SystemTime,
    policy: ReclaimPolicy,
    liveness: &dyn Liveness,
) -> Vec<Candidate> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    let mut live_tmpdirs: Option<Option<Vec<String>>> = None;
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_validation_dir = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(PREFIX) && !name.ends_with(OWNER_SUFFIX));
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !is_validation_dir || !metadata.is_dir() || !owned_by_current_user(&metadata) {
            continue;
        }
        let Some(age) = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
        else {
            continue;
        };
        if age < policy.min_age {
            continue;
        }
        let reason = match fs::read_to_string(owner_path(&path)) {
            Ok(text) => {
                let Ok(pid) = text.trim().parse::<u32>() else {
                    continue;
                };
                if liveness.pid_alive(pid) {
                    continue;
                }
                format!("owner process {pid} is gone")
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !policy.include_unowned {
                    continue;
                }
                let in_use = live_tmpdirs.get_or_insert_with(|| liveness.live_tmpdirs());
                let Some(in_use) = in_use else {
                    continue;
                };
                let text = path.to_string_lossy();
                if in_use.iter().any(|value| value == text.as_ref()) {
                    continue;
                }
                "no owner file and no live process uses it as TMPDIR".to_owned()
            }
            Err(_) => continue,
        };
        candidates.push(Candidate {
            bytes: tree_bytes(&path),
            age_hours: age.as_secs() / 3600,
            path,
            reason,
        });
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.bytes));
    candidates
}

/// Delete planned directories, returning the bytes actually freed.
#[must_use]
pub fn apply(candidates: &[Candidate]) -> u64 {
    let mut freed = 0;
    for candidate in candidates {
        super::local::make_tree_owner_writable(&candidate.path);
        if fs::remove_dir_all(&candidate.path).is_ok() {
            freed += candidate.bytes;
            remove_owner(&candidate.path);
        }
    }
    freed
}

/// Delete dead owners' directories older than [`AUTOMATIC_MIN_AGE`]. A run
/// calls this when it creates its own TMPDIR, so a killed run's tree does not
/// outlive the next run. Directories without an owner file are left to the
/// explicit `shipyard cleanup --validation-tmp`.
pub fn sweep_dead_owners(base: &Path) {
    let policy = ReclaimPolicy {
        min_age: AUTOMATIC_MIN_AGE,
        include_unowned: false,
    };
    let _ = apply(&plan(base, SystemTime::now(), policy, &SystemLiveness));
}

/// The directory validation TMPDIRs are created under.
#[must_use]
pub fn default_base() -> PathBuf {
    if let Some(base) =
        std::env::var_os(super::local::VALIDATION_TMP_BASE_ENV).filter(|value| !value.is_empty())
    {
        return PathBuf::from(base);
    }
    if cfg!(target_os = "macos") {
        PathBuf::from("/private/tmp")
    } else {
        std::env::temp_dir()
    }
}

#[cfg(unix)]
fn owned_by_current_user(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == nix::unistd::getuid().as_raw()
}

#[cfg(not(unix))]
fn owned_by_current_user(_metadata: &fs::Metadata) -> bool {
    true
}

fn tree_bytes(root: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                total += metadata.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests;
