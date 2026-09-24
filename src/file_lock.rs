//! A locked file that releases its `flock` explicitly when dropped.
//!
//! `flock` belongs to the open file description, not to the descriptor. A
//! child forked by *any* thread in this process shares that description until
//! its `exec` closes the (close-on-exec) descriptor. A lock released only by
//! closing its own descriptor therefore stays held for a moment by such a
//! child, and the next acquisition in this process reads a free lock as
//! contended. Shipyard spawns `gh`, `git` and `ssh` children constantly, so
//! every lock whose release matters calls `unlock` instead of relying on close.

use std::fs::File;
use std::ops::{Deref, DerefMut};

use fs2::FileExt;

/// A file holding an `flock`, released explicitly on drop.
#[derive(Debug)]
pub struct LockedFile(File);

impl LockedFile {
    /// Wrap a file the caller has already locked.
    #[must_use]
    pub const fn new(file: File) -> Self {
        Self(file)
    }
}

impl Deref for LockedFile {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

    use super::*;

    fn open(path: &std::path::Path) -> File {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .expect("open lock")
    }

    /// A duplicate of the locked descriptor, which is what a concurrently
    /// forked child holds until its exec, must not keep the lock held after
    /// the owner drops it.
    #[test]
    fn dropping_releases_the_lock_while_a_duplicate_descriptor_lives() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("lock");
        let file = open(&path);
        file.try_lock_exclusive().expect("lock");
        let duplicate = file.try_clone().expect("dup");
        drop(LockedFile::new(file));
        open(&path)
            .try_lock_exclusive()
            .expect("a lingering duplicate descriptor kept the lock held");
        drop(duplicate);
    }

    /// Control: closing alone (the old behaviour) leaves the lock held by the
    /// duplicate, which is the failure the explicit unlock prevents.
    #[cfg(unix)]
    #[test]
    fn closing_alone_leaves_the_lock_held_by_a_duplicate() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("lock");
        let file = open(&path);
        file.try_lock_exclusive().expect("lock");
        let duplicate = file.try_clone().expect("dup");
        drop(file);
        assert!(open(&path).try_lock_exclusive().is_err());
        drop(duplicate);
    }
}
