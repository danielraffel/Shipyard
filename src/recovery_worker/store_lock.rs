use super::{RecoveryResult, RecoveryStore, is_file_lock_contended};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io;
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_STORE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const STORE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

impl RecoveryStore {
    pub(super) fn lock(&self) -> RecoveryResult<RecoveryStoreLock> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.root.join("store.lock"))?;
        wait_for_store_lock(file, self.lock_deadline(), true)
    }

    pub(super) fn read_lock_if_present(&self) -> RecoveryResult<Option<RecoveryStoreLock>> {
        let file = match OpenOptions::new()
            .read(true)
            .open(self.root.join("store.lock"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        wait_for_store_lock(file, self.lock_deadline(), false).map(Some)
    }

    fn lock_deadline(&self) -> Instant {
        self.lock_deadline
            .unwrap_or_else(|| Instant::now() + DEFAULT_STORE_LOCK_TIMEOUT)
    }
}

fn wait_for_store_lock(
    file: File,
    deadline: Instant,
    exclusive: bool,
) -> RecoveryResult<RecoveryStoreLock> {
    loop {
        let result = if exclusive {
            FileExt::try_lock_exclusive(&file)
        } else {
            FileExt::try_lock_shared(&file)
        };
        match result {
            Ok(()) => return Ok(RecoveryStoreLock(file)),
            Err(error) if is_file_lock_contended(&error) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    let kind = if exclusive { "exclusive" } else { "shared" };
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("timed out acquiring {kind} recovery store lock"),
                    )
                    .into());
                }
                thread::sleep(remaining.min(STORE_LOCK_POLL_INTERVAL));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub(super) struct RecoveryStoreLock(File);

impl Drop for RecoveryStoreLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
