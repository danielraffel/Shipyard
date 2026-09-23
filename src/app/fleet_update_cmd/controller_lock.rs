//! One fleet mutation at a time per controller.
//!
//! A release-stage rollout, an operator's `fleet-update --apply` and the
//! reconcile agent can all start a rollout. Two at once would interleave
//! updates, verifications and rollbacks on the same hosts. The lock spans the
//! whole apply + verify + rollback sequence; a contender does not wait.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;

pub(super) fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("fleet-update.lock")
}

/// Held for as long as the value lives.
#[derive(Debug)]
pub(super) struct ControllerLock {
    _file: File,
}

/// Take the controller lock without waiting. `Ok(None)` means another rollout
/// holds it.
pub(super) fn try_acquire(state_dir: &Path) -> Result<Option<ControllerLock>, String> {
    std::fs::create_dir_all(state_dir)
        .map_err(|error| format!("create fleet controller state dir: {error}"))?;
    let path = lock_path(state_dir);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| format!("open fleet controller lock {}: {error}", path.display()))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(ControllerLock { _file: file })),
        Err(error)
            if error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                || error.kind() == std::io::ErrorKind::WouldBlock =>
        {
            Ok(None)
        }
        Err(error) => Err(format!("lock fleet controller {}: {error}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_until_the_first_releases() {
        let temp = tempfile::tempdir().expect("temp");
        let first = try_acquire(temp.path()).expect("lock").expect("free");
        assert!(try_acquire(temp.path()).expect("lock").is_none());
        drop(first);
        assert!(try_acquire(temp.path()).expect("lock").is_some());
    }
}
