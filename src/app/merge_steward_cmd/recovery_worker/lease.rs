use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde_json::Value;

use super::CliFailure;

const RECOVERY_LEASE_TIMEOUT_SECONDS: u64 = 5;
const RECOVERY_LEASE_POLL_MILLIS: u64 = 10;

/// Exclusive ordering proof shared by request/witness publication and clear.
#[derive(Debug)]
pub(in crate::app::merge_steward_cmd) struct RecoveryEnqueueLease {
    _file: File,
    store_root: PathBuf,
}

impl RecoveryEnqueueLease {
    pub(super) fn covers(&self, store_root: &Path) -> bool {
        self.store_root == store_root
    }
}

/// Machine-global model capacity.
///
/// This guard intentionally relies on last-handle close rather than an
/// explicit unlock. The model inherits a duplicate as stdin, so an abrupt
/// parent exit cannot release capacity while that child is still alive.
pub(super) struct GlobalModelLease(File);

impl GlobalModelLease {
    pub(super) fn worker_stdin(&self, request: &Value) -> Result<Stdio, CliFailure> {
        let mut input = self.0.try_clone().map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to duplicate global model lease: {error}"),
            )
        })?;
        input.set_len(0).map_err(|error| {
            CliFailure::new(1, format!("failed to reset recovery-worker stdin: {error}"))
        })?;
        input.seek(SeekFrom::Start(0)).map_err(|error| {
            CliFailure::new(1, format!("failed to seek recovery-worker stdin: {error}"))
        })?;
        serde_json::to_writer(&mut input, request).map_err(|error| {
            CliFailure::new(1, format!("failed to serialize recovery request: {error}"))
        })?;
        input.flush().map_err(|error| {
            CliFailure::new(1, format!("failed to flush recovery-worker stdin: {error}"))
        })?;
        input.seek(SeekFrom::Start(0)).map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to rewind recovery-worker stdin: {error}"),
            )
        })?;
        Ok(Stdio::from(input))
    }
}

pub(super) fn acquire_global_model_lease(
    path: &Path,
) -> Result<Option<GlobalModelLease>, CliFailure> {
    let parent = path
        .parent()
        .ok_or_else(|| CliFailure::new(1, "global recovery-model lease path has no parent"))?;
    fs::create_dir_all(parent).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "failed to create global recovery-model lease directory {}: {error}",
                parent.display()
            ),
        )
    })?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "failed to open global recovery-model lease {}: {error}",
                    path.display()
                ),
            )
        })?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(GlobalModelLease(file))),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(CliFailure::new(
            1,
            format!("failed to acquire global recovery-model lease: {error}"),
        )),
    }
}

pub(super) fn recovery_lease_deadline() -> Instant {
    Instant::now() + Duration::from_secs(RECOVERY_LEASE_TIMEOUT_SECONDS)
}

pub(in crate::app::merge_steward_cmd) fn acquire_recovery_enqueue_lease(
    store_root: &Path,
    deadline: Instant,
) -> Result<RecoveryEnqueueLease, CliFailure> {
    let path = store_root.join("enqueue-witness.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "failed to open recovery enqueue lease {}: {error}",
                    path.display()
                ),
            )
        })?;
    Ok(RecoveryEnqueueLease {
        _file: wait_for_lease(file, deadline, true)?,
        store_root: store_root.to_path_buf(),
    })
}

pub(super) fn acquire_recovery_enqueue_read_lease(
    store_root: &Path,
    deadline: Instant,
) -> Result<File, CliFailure> {
    let path = store_root.join("enqueue-witness.lock");
    let file = OpenOptions::new().read(true).open(&path).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "failed to open existing recovery enqueue lease {}: {error}",
                path.display()
            ),
        )
    })?;
    wait_for_lease(file, deadline, false)
}

fn wait_for_lease(file: File, deadline: Instant, exclusive: bool) -> Result<File, CliFailure> {
    loop {
        let result = if exclusive {
            FileExt::try_lock_exclusive(&file)
        } else {
            FileExt::try_lock_shared(&file)
        };
        match result {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    let kind = if exclusive { "exclusive" } else { "shared" };
                    return Err(CliFailure::new(
                        1,
                        format!("timed out acquiring {kind} recovery enqueue lease"),
                    ));
                }
                thread::sleep(remaining.min(Duration::from_millis(RECOVERY_LEASE_POLL_MILLIS)));
            }
            Err(error) => {
                let action = if exclusive { "lock" } else { "share-lock" };
                return Err(CliFailure::new(
                    1,
                    format!("failed to {action} recovery enqueue lease: {error}"),
                ));
            }
        }
    }
}
