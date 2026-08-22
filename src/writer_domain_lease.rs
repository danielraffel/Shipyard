//! Host-global coordination between production writers and the sandbox E2E audit.
//!
//! Production Shipyard invocations hold a shared lease for their lifetime. The
//! sandbox E2E harness takes the matching exclusive lease before snapshotting
//! protected paths and retains it through the contamination assertion. This
//! makes lock ownership, rather than filename heuristics, the proof that no
//! legitimate production writer overlapped an audit.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;

use crate::identity::RuntimeMode;
use crate::paths::RuntimePaths;

/// Stable machine-readable prefix for bounded writer-domain contention.
pub const WRITER_DOMAIN_OVERLAP_CLASSIFICATION: &str = "sandbox_writer_domain_overlap";
/// Temporary-failure exit used when a proven overlap outlives the bounded wait.
pub const WRITER_DOMAIN_OVERLAP_EXIT_CODE: u8 = 75;

const WRITER_DOMAIN_LOCK_NAME: &str = ".sandbox-writer-domain.lock";
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// A lifetime-scoped shared production-writer lease.
pub(crate) struct ProductionWriterDomainLease(File);

impl Drop for ProductionWriterDomainLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

/// Acquire the shared host-global writer lease for a production invocation.
///
/// Isolated invocations intentionally do not join the production writer domain:
/// the E2E child is rooted in its temporary HOME while its parent owns the
/// exclusive lease for the real host. Explicit production mode is the boundary
/// for all commands because a read-looking command may still refresh durable
/// state, diagnostics, logs, or evidence in a deeper layer.
pub(crate) fn acquire_production_writer_domain_lease(
    mode: RuntimeMode,
) -> io::Result<Option<ProductionWriterDomainLease>> {
    acquire_production_writer_domain_lease_with_timeout(mode, DEFAULT_ACQUIRE_TIMEOUT)
}

fn acquire_production_writer_domain_lease_with_timeout(
    mode: RuntimeMode,
    timeout: Duration,
) -> io::Result<Option<ProductionWriterDomainLease>> {
    if mode != RuntimeMode::Shipyard {
        return Ok(None);
    }

    let state_dir = RuntimePaths::current(RuntimeMode::Shipyard).state_dir;
    fs::create_dir_all(&state_dir)?;
    let path = writer_domain_lock_path(&state_dir);
    let file = open_lock_file(&path)?;
    let deadline = Instant::now() + timeout;

    loop {
        match FileExt::try_lock_shared(&file) {
            Ok(()) => return Ok(Some(ProductionWriterDomainLease(file))),
            Err(error) if lock_is_contended(&error) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: exclusive sandbox audit owns {}",
                            path.display()
                        ),
                    ));
                }
                thread::sleep(remaining.min(POLL_INTERVAL));
            }
            Err(error) => return Err(error),
        }
    }
}

fn writer_domain_lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join(WRITER_DOMAIN_LOCK_NAME)
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn lock_is_contended(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        matches!(error.raw_os_error(), Some(32 | 33))
    }
    #[cfg(not(windows))]
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_mode_never_opens_production_writer_domain() {
        let lease = acquire_production_writer_domain_lease_with_timeout(
            RuntimeMode::Isolated,
            Duration::ZERO,
        )
        .expect("isolated acquisition");
        assert!(lease.is_none());
    }

    #[test]
    fn shared_production_lease_times_out_behind_exclusive_audit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = writer_domain_lock_path(temp.path());
        let exclusive = open_lock_file(&path).expect("exclusive handle");
        FileExt::lock_exclusive(&exclusive).expect("exclusive lock");

        let contender = open_lock_file(&path).expect("shared contender");
        let deadline = Instant::now() + Duration::from_millis(30);
        let error = loop {
            match FileExt::try_lock_shared(&contender) {
                Ok(()) => panic!("shared lease bypassed exclusive audit"),
                Err(error) if lock_is_contended(&error) => {
                    if Instant::now() >= deadline {
                        break io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: proven overlap"),
                        );
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("unexpected lock error: {error}"),
            }
        };

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            error
                .to_string()
                .starts_with(WRITER_DOMAIN_OVERLAP_CLASSIFICATION)
        );
        FileExt::unlock(&exclusive).expect("unlock exclusive");
    }

    #[test]
    fn exclusive_audit_cannot_overlap_multiple_shared_writers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = writer_domain_lock_path(temp.path());
        let first = open_lock_file(&path).expect("first writer");
        let second = open_lock_file(&path).expect("second writer");
        FileExt::lock_shared(&first).expect("first shared");
        FileExt::lock_shared(&second).expect("second shared");

        let audit = open_lock_file(&path).expect("audit contender");
        let error = FileExt::try_lock_exclusive(&audit).expect_err("must overlap");
        assert!(lock_is_contended(&error));

        FileExt::unlock(&first).expect("unlock first");
        FileExt::unlock(&second).expect("unlock second");
        FileExt::lock_exclusive(&audit).expect("audit after writers");
        FileExt::unlock(&audit).expect("unlock audit");
    }
}
