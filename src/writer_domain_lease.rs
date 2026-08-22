//! Host-global coordination between production persistence and sandbox E2E audits.
//!
//! A resident daemon or long-running validation is not itself a writer. Callers
//! acquire a shared lease only around the filesystem mutation they are about to
//! perform. The sandbox harness holds the matching exclusive lease from before
//! its protected-path snapshot through its final contamination assertion.
//!
//! A separate turnstile is taken exclusively while either side enters the data
//! domain. The sandbox keeps the turnstile while auditing, so a steady stream of
//! short production writes cannot starve the exclusive audit.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::marker::PhantomData;
use std::path::Path;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;

use crate::identity::RuntimeMode;
use crate::paths::{RuntimePaths, home_dir};

/// Stable machine-readable prefix for bounded writer-domain contention.
pub const WRITER_DOMAIN_OVERLAP_CLASSIFICATION: &str = "sandbox_writer_domain_overlap";
/// Temporary-failure exit used when a proven overlap outlives the bounded wait.
pub const WRITER_DOMAIN_OVERLAP_EXIT_CODE: u8 = 75;

const WRITER_DOMAIN_LOCK_NAME: &str = ".sandbox-writer-domain.lock";
const WRITER_DOMAIN_TURNSTILE_NAME: &str = ".sandbox-writer-domain.turnstile.lock";
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Internal marker set only when Shipyard redirects a detached process's
/// stdout/stderr to a protected log.
pub(crate) const PROTECTED_STDIO_PATH_ENV: &str = "SHIPYARD_PROTECTED_STDIO_PATH";

struct ThreadWriterDomainLease {
    domain: File,
    depth: usize,
}

thread_local! {
    static THREAD_WRITER_DOMAIN: RefCell<Option<ThreadWriterDomainLease>> = const {
        RefCell::new(None)
    };
}

/// A mutation-scoped shared production-writer lease.
///
/// The token is deliberately thread-bound. Nested persistence helpers on the
/// same thread increment one lease depth instead of reacquiring the turnstile;
/// otherwise an arriving exclusive audit could own the turnstile while
/// waiting for the outer shared domain, deadlocking the nested writer.
#[derive(Debug)]
pub(crate) struct ProductionWriterDomainLease {
    _thread_bound: PhantomData<Rc<()>>,
}

impl Drop for ProductionWriterDomainLease {
    fn drop(&mut self) {
        THREAD_WRITER_DOMAIN.with(|slot| {
            let mut slot = slot.borrow_mut();
            let held = slot
                .as_mut()
                .expect("writer-domain token dropped without a thread lease");
            held.depth = held
                .depth
                .checked_sub(1)
                .expect("writer-domain lease depth underflow");
            if held.depth == 0 {
                let held = slot.take().expect("writer-domain lease disappeared");
                let _ = FileExt::unlock(&held.domain);
            }
        });
    }
}

/// Acquire the production writer domain when `path` is in the real Shipyard
/// state or configuration tree.
///
/// Unit tests and isolated/overridden state roots deliberately remain outside
/// this machine-global domain. Their files cannot contaminate the real-home
/// sandbox audit.
pub(crate) fn acquire_for_protected_path(
    path: &Path,
) -> io::Result<Option<ProductionWriterDomainLease>> {
    let runtime_paths = RuntimePaths::current(RuntimeMode::Shipyard);
    if !is_protected_path(path, &home_dir(), &runtime_paths) {
        return Ok(None);
    }
    acquire_thread_lease_at(&runtime_paths.state_dir, DEFAULT_ACQUIRE_TIMEOUT).map(Some)
}

fn acquire_thread_lease_at(
    state_dir: &Path,
    timeout: Duration,
) -> io::Result<ProductionWriterDomainLease> {
    let nested = THREAD_WRITER_DOMAIN.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(held) = slot.as_mut() else {
            return false;
        };
        held.depth += 1;
        true
    });
    if !nested {
        let domain = acquire_at(state_dir, timeout)?;
        THREAD_WRITER_DOMAIN.with(|slot| {
            let previous = slot.replace(Some(ThreadWriterDomainLease { domain, depth: 1 }));
            debug_assert!(previous.is_none());
        });
    }
    Ok(ProductionWriterDomainLease {
        _thread_bound: PhantomData,
    })
}

/// Fence a write to stdout/stderr when the spawning Shipyard process routed
/// that descriptor to a protected log. Ordinary interactive invocations do
/// not set the marker and remain lock-free.
pub(crate) fn acquire_for_protected_stdio() -> io::Result<Option<ProductionWriterDomainLease>> {
    let Some(path) = std::env::var_os(PROTECTED_STDIO_PATH_ENV) else {
        return Ok(None);
    };
    acquire_for_protected_path(Path::new(&path))
}

/// Create a protected directory only when it is absent. Existing stores are a
/// read surface and must not briefly enter the writer domain merely because a
/// constructor was used to inspect them.
pub(crate) fn ensure_protected_dir_all(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let _writer_domain = acquire_for_protected_path(path)?;
    fs::create_dir_all(path)
}

/// Fence an `OpenOptions::create(true)` call only when it can create a new
/// protected directory entry. Locking an already-existing coordination file
/// is read-only with respect to the sandbox contamination surface.
pub(crate) fn acquire_for_protected_creation(
    path: &Path,
) -> io::Result<Option<ProductionWriterDomainLease>> {
    if path.exists() {
        return Ok(None);
    }
    acquire_for_protected_path(path)
}

fn is_protected_path(path: &Path, home: &Path, runtime_paths: &RuntimePaths) -> bool {
    if path.starts_with(&runtime_paths.state_dir) || path.starts_with(&runtime_paths.global_dir) {
        return true;
    }
    [
        home.join("Library/Application Support/shipyard"),
        home.join("Library/Application Support/shipyard-dev"),
        home.join(".config/shipyard"),
        home.join(".config/shipyard-dev"),
        home.join(".local/state/shipyard"),
        home.join(".local/state/shipyard-dev"),
        home.join("AppData/Local/shipyard"),
        home.join("AppData/Local/shipyard-dev"),
        home.join(".local/bin"),
        home.join(".shipyard"),
        home.join(".shipyard-dev"),
        home.join(".cache/shipyard"),
        home.join(".cache/shipyard-dev"),
    ]
    .iter()
    .any(|root| path.starts_with(root))
}

fn acquire_at(state_dir: &Path, timeout: Duration) -> io::Result<File> {
    fs::create_dir_all(state_dir)?;
    let deadline = Instant::now() + timeout;
    let turnstile_path = state_dir.join(WRITER_DOMAIN_TURNSTILE_NAME);
    let turnstile = open_lock_file(&turnstile_path)?;
    acquire_lock(&turnstile, LockKind::Exclusive, deadline, &turnstile_path)?;

    let domain_path = state_dir.join(WRITER_DOMAIN_LOCK_NAME);
    let domain = open_lock_file(&domain_path)?;
    let result = acquire_lock(&domain, LockKind::Shared, deadline, &domain_path);
    let _ = FileExt::unlock(&turnstile);
    result?;
    Ok(domain)
}

#[derive(Clone, Copy)]
enum LockKind {
    Shared,
    Exclusive,
}

fn acquire_lock(file: &File, kind: LockKind, deadline: Instant, path: &Path) -> io::Result<()> {
    loop {
        let result = match kind {
            LockKind::Shared => FileExt::try_lock_shared(file),
            LockKind::Exclusive => FileExt::try_lock_exclusive(file),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) if lock_is_contended(&error) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(overlap_error(path));
                }
                thread::sleep(remaining.min(POLL_INTERVAL));
            }
            Err(error) => return Err(error),
        }
    }
}

fn overlap_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: exclusive sandbox audit owns {}",
            path.display()
        ),
    )
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

pub(crate) fn is_writer_domain_overlap(message: &str) -> bool {
    message.contains(WRITER_DOMAIN_OVERLAP_CLASSIFICATION)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn unrelated_test_path_never_opens_production_writer_domain() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lease = acquire_for_protected_path(&temp.path().join("state.json"))
            .expect("unrelated acquisition");
        assert!(lease.is_none());
        assert!(!temp.path().join(WRITER_DOMAIN_LOCK_NAME).exists());
    }

    #[test]
    fn every_sandbox_audited_home_root_joins_the_writer_domain() {
        let home = Path::new("/host-home");
        let runtime_paths = RuntimePaths::for_platform(
            crate::platform::Platform::MacOs,
            home,
            RuntimeMode::Shipyard,
        );
        for relative in [
            "Library/Application Support/shipyard/state",
            "Library/Application Support/shipyard-dev/state",
            ".config/shipyard/config.toml",
            ".config/shipyard-dev/config.toml",
            ".local/state/shipyard/queue.json",
            ".local/state/shipyard-dev/queue.json",
            "AppData/Local/shipyard/queue.json",
            "AppData/Local/shipyard-dev/queue.json",
            ".local/bin/shipyard",
            ".shipyard/state.json",
            ".shipyard-dev/state.json",
            ".cache/shipyard/object",
            ".cache/shipyard-dev/object",
        ] {
            assert!(
                is_protected_path(&home.join(relative), home, &runtime_paths),
                "missing protected root {relative}"
            );
        }
        assert!(!is_protected_path(
            &home.join("Code/Shipyard"),
            home,
            &runtime_paths
        ));
    }

    #[test]
    fn production_mutation_times_out_behind_exclusive_audit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let domain_path = temp.path().join(WRITER_DOMAIN_LOCK_NAME);
        let exclusive = open_lock_file(&domain_path).expect("exclusive handle");
        FileExt::lock_exclusive(&exclusive).expect("exclusive lock");

        let error = acquire_at(temp.path(), Duration::from_millis(30))
            .expect_err("production mutation must not bypass audit");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(is_writer_domain_overlap(&error.to_string()));
        FileExt::unlock(&exclusive).expect("unlock exclusive");
    }

    #[test]
    fn mutation_lease_is_released_at_critical_section_end() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = acquire_at(temp.path(), Duration::from_millis(50)).expect("writer");
        let audit = open_lock_file(&temp.path().join(WRITER_DOMAIN_LOCK_NAME)).expect("audit");
        assert!(FileExt::try_lock_exclusive(&audit).is_err());
        drop(first);
        FileExt::lock_exclusive(&audit).expect("audit after mutation");
        FileExt::unlock(&audit).expect("unlock audit");
    }

    #[test]
    fn nested_mutation_reuses_domain_after_audit_owns_turnstile() {
        let temp = tempfile::tempdir().expect("tempdir");
        let outer =
            acquire_thread_lease_at(temp.path(), Duration::from_millis(50)).expect("outer writer");
        let turnstile =
            open_lock_file(&temp.path().join(WRITER_DOMAIN_TURNSTILE_NAME)).expect("turnstile");
        FileExt::lock_exclusive(&turnstile).expect("audit turnstile");

        let nested = acquire_thread_lease_at(temp.path(), Duration::ZERO)
            .expect("nested writer must not reacquire turnstile");
        let audit = open_lock_file(&temp.path().join(WRITER_DOMAIN_LOCK_NAME)).expect("audit");
        assert!(FileExt::try_lock_exclusive(&audit).is_err());
        drop(nested);
        assert!(FileExt::try_lock_exclusive(&audit).is_err());
        drop(outer);
        FileExt::lock_exclusive(&audit).expect("audit after outer critical section");

        FileExt::unlock(&audit).expect("unlock audit domain");
        FileExt::unlock(&turnstile).expect("unlock audit turnstile");
    }

    #[test]
    fn audit_turnstile_is_not_starved_by_continuous_writers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = Arc::new(temp.path().to_path_buf());
        let running = Arc::new(AtomicBool::new(true));
        let mut writers = Vec::new();
        for _ in 0..4 {
            let root = Arc::clone(&root);
            let running = Arc::clone(&running);
            writers.push(thread::spawn(move || {
                while running.load(Ordering::Acquire) {
                    let lease = acquire_at(&root, Duration::from_secs(1)).expect("writer lease");
                    thread::sleep(Duration::from_millis(1));
                    drop(lease);
                }
            }));
        }

        thread::sleep(Duration::from_millis(20));
        let turnstile = open_lock_file(&root.join(WRITER_DOMAIN_TURNSTILE_NAME)).expect("gate");
        acquire_lock(
            &turnstile,
            LockKind::Exclusive,
            Instant::now() + Duration::from_secs(2),
            &root.join(WRITER_DOMAIN_TURNSTILE_NAME),
        )
        .expect("audit turnstile");
        let domain = open_lock_file(&root.join(WRITER_DOMAIN_LOCK_NAME)).expect("domain");
        acquire_lock(
            &domain,
            LockKind::Exclusive,
            Instant::now() + Duration::from_secs(2),
            &root.join(WRITER_DOMAIN_LOCK_NAME),
        )
        .expect("audit domain");

        running.store(false, Ordering::Release);
        FileExt::unlock(&domain).expect("unlock domain");
        FileExt::unlock(&turnstile).expect("unlock turnstile");
        for writer in writers {
            writer.join().expect("writer join");
        }
    }
}
