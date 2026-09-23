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
    file: File,
}

impl Drop for ControllerLock {
    /// Release explicitly rather than by closing. `flock` belongs to the open
    /// file description, which a child forked by any thread in this process
    /// shares until its `exec` closes the descriptor. Closing our copy then
    /// leaves the lock held by that child for a moment, and the next
    /// acquisition in this process reads a free controller as busy. Unlocking
    /// releases the description itself, whoever else still holds a copy.
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
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
        Ok(()) => Ok(Some(ControllerLock { file })),
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

    /// A duplicate of the locked descriptor (what a forked child holds until
    /// its exec) must not keep the controller locked after the holder drops.
    #[test]
    fn releasing_frees_the_lock_even_while_a_duplicate_descriptor_lives() {
        let temp = tempfile::tempdir().expect("temp");
        let held = try_acquire(temp.path()).expect("lock").expect("free");
        let duplicate = held.file.try_clone().expect("dup");
        drop(held);
        assert!(
            try_acquire(temp.path()).expect("lock").is_some(),
            "a lingering duplicate descriptor kept the controller locked"
        );
        drop(duplicate);
    }
}
