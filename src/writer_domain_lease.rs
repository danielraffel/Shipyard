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
use std::io::{self, Write};
use std::marker::PhantomData;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus};
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
pub(crate) const CHILD_WRITER_PATH_ENV: &str = "SHIPYARD_CHILD_WRITER_PATH";

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

/// Type-level proof that the current thread owns the exclusive production
/// snapshot domain rather than an ordinary shared writer lease.
#[derive(Debug)]
pub(crate) struct ProductionSnapshotLease {
    _lease: ProductionWriterDomainLease,
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
    if !is_current_protected_path(path)? {
        return Ok(None);
    }
    let runtime_paths = RuntimePaths::current(RuntimeMode::Shipyard);
    acquire_thread_lease_at(&runtime_paths.state_dir, DEFAULT_ACQUIRE_TIMEOUT).map(Some)
}

/// Acquire an exclusive production snapshot barrier for a bounded migration.
/// Nested writers on this thread reuse the exclusive lease; all other
/// production writers wait at the normal shared-domain acquisition point.
pub(crate) fn acquire_exclusive_for_protected_path(
    path: &Path,
) -> io::Result<Option<ProductionSnapshotLease>> {
    if !is_current_protected_path(path)? {
        return Ok(None);
    }
    let already_held = THREAD_WRITER_DOMAIN.with(|slot| slot.borrow().is_some());
    if already_held {
        return Err(io::Error::other(
            "cannot upgrade an active writer-domain lease to an exclusive snapshot",
        ));
    }
    let runtime_paths = RuntimePaths::current(RuntimeMode::Shipyard);
    let domain = acquire_exclusive_at(&runtime_paths.state_dir, DEFAULT_ACQUIRE_TIMEOUT)?;
    THREAD_WRITER_DOMAIN.with(|slot| {
        let previous = slot.replace(Some(ThreadWriterDomainLease { domain, depth: 1 }));
        debug_assert!(previous.is_none());
    });
    Ok(Some(ProductionSnapshotLease {
        _lease: ProductionWriterDomainLease {
            _thread_bound: PhantomData,
        },
    }))
}

/// Acquire an exclusive production snapshot barrier without creating either
/// writer-domain lock file.
///
/// Read-only authority paths use this form so a dry-run can never materialize
/// coordination state. A production tree whose lock generation has not yet
/// been established is refused; the next real writer establishes it through
/// the ordinary creating acquisition path.
pub(crate) fn acquire_existing_exclusive_for_protected_path(
    path: &Path,
) -> io::Result<Option<ProductionSnapshotLease>> {
    if !is_current_protected_path(path)? {
        return Ok(None);
    }
    let already_held = THREAD_WRITER_DOMAIN.with(|slot| slot.borrow().is_some());
    if already_held {
        return Err(io::Error::other(
            "cannot upgrade an active writer-domain lease to an exclusive snapshot",
        ));
    }
    let runtime_paths = RuntimePaths::current(RuntimeMode::Shipyard);
    let domain = acquire_existing_exclusive_at(&runtime_paths.state_dir, DEFAULT_ACQUIRE_TIMEOUT)?;
    THREAD_WRITER_DOMAIN.with(|slot| {
        let previous = slot.replace(Some(ThreadWriterDomainLease { domain, depth: 1 }));
        debug_assert!(previous.is_none());
    });
    Ok(Some(ProductionSnapshotLease {
        _lease: ProductionWriterDomainLease {
            _thread_bound: PhantomData,
        },
    }))
}

/// Report whether a path belongs to the current machine's production writer
/// domain without acquiring its lease.
pub(crate) fn is_current_protected_path(path: &Path) -> io::Result<bool> {
    let runtime_paths = RuntimePaths::current(RuntimeMode::Shipyard);
    is_protected_path(path, &home_dir(), &runtime_paths)
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

/// Append one complete diagnostic to stderr while respecting a detached
/// protected-log marker. Callers intentionally ignore failures: writing after
/// an exclusive audit wins would contaminate evidence, so silence is safer.
pub(crate) fn write_stderr(arguments: std::fmt::Arguments<'_>) -> io::Result<()> {
    let lease = acquire_for_protected_stdio();
    let mut stderr = io::stderr().lock();
    write_diagnostic_with_lease(arguments, lease, &mut stderr)
}

fn write_diagnostic_with_lease<T>(
    arguments: std::fmt::Arguments<'_>,
    lease: io::Result<T>,
    writer: &mut impl Write,
) -> io::Result<()> {
    let _lease = lease?;
    writer.write_fmt(arguments)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Wrap an external command in a Shipyard process that acquires its own lease
/// before spawning the writer. Environment and cwd overrides are preserved.
pub(crate) fn guarded_child_command(command: &Command, path: &Path) -> io::Result<Command> {
    let mut guarded = Command::new(std::env::current_exe()?);
    guarded
        .arg("writer-domain-exec")
        .arg("--path")
        .arg(path)
        .arg("--")
        .arg(command.get_program())
        .args(command.get_args());
    if let Some(cwd) = command.get_current_dir() {
        guarded.current_dir(cwd);
    }
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            guarded.env(key, value);
        } else {
            guarded.env_remove(key);
        }
    }
    Ok(guarded)
}

pub(crate) fn run_guarded_child(
    path: &Path,
    argv: &[std::ffi::OsString],
) -> io::Result<ExitStatus> {
    let (program, command_args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing guarded command"))?;
    let _writer_domain = acquire_for_protected_path(path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "guarded child path is outside protected roots: {}",
                path.display()
            ),
        )
    })?;
    Command::new(program).args(command_args).status()
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

fn is_protected_path(path: &Path, home: &Path, runtime_paths: &RuntimePaths) -> io::Result<bool> {
    let candidate = canonicalize_with_missing_suffix(path)?;
    let roots = [
        runtime_paths.state_dir.clone(),
        runtime_paths.global_dir.clone(),
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
    ];
    for root in roots {
        let root = canonicalize_with_missing_suffix(&root)?;
        if protected_prefix(&candidate, &root) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Resolve aliases one component at a time while retaining a normalized suffix
/// for paths that a writer is about to create. Resolving each existing
/// component before interpreting `..` preserves filesystem semantics when a
/// parent component is itself a symlink.
fn canonicalize_with_missing_suffix(path: &Path) -> io::Result<PathBuf> {
    canonicalize_with_missing_suffix_with(path, std::env::current_dir)
}

fn canonicalize_with_missing_suffix_with<C>(path: &Path, current_dir: C) -> io::Result<PathBuf>
where
    C: FnOnce() -> io::Result<PathBuf>,
{
    if path.is_absolute() {
        return canonicalize_with_missing_suffix_from(path, Path::new(""));
    }
    canonicalize_with_missing_suffix_from(path, &current_dir()?)
}

fn canonicalize_with_missing_suffix_from(path: &Path, current_dir: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::Normal(name) => {
                let candidate = resolved.join(name);
                match canonicalize_component_with(
                    &candidate,
                    |path| fs::canonicalize(path),
                    |path| fs::symlink_metadata(path),
                )? {
                    Some(canonical) => resolved = canonical,
                    None => resolved.push(name),
                }
            }
        }
    }
    Ok(resolved)
}

fn canonicalize_component_with<C, M>(
    candidate: &Path,
    mut canonicalize: C,
    symlink_metadata: M,
) -> io::Result<Option<PathBuf>>
where
    C: FnMut(&Path) -> io::Result<PathBuf>,
    M: FnOnce(&Path) -> io::Result<fs::Metadata>,
{
    match canonicalize(candidate) {
        Ok(canonical) => Ok(Some(canonical)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match symlink_metadata(candidate) {
                // A dangling symlink/reparse point is ambiguous: treating it as an
                // ordinary missing path could let a later target swap cross the
                // protected-root boundary.
                Ok(metadata) if metadata.file_type().is_symlink() => Err(error),
                // A regular entry can appear between canonicalize and metadata
                // when two processes create the same coordination file. Retry the
                // authoritative resolution instead of returning the stale ENOENT.
                Ok(_) => canonicalize(candidate).map(Some),
                Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(metadata_error) => Err(metadata_error),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn protected_prefix(path: &Path, root: &Path) -> bool {
    let path_components: Vec<_> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    let root_components: Vec<_> = root
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
        .collect();
    path_components.starts_with(&root_components)
}

#[cfg(not(windows))]
fn protected_prefix(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
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

fn acquire_exclusive_at(state_dir: &Path, timeout: Duration) -> io::Result<File> {
    fs::create_dir_all(state_dir)?;
    let deadline = Instant::now() + timeout;
    let turnstile_path = state_dir.join(WRITER_DOMAIN_TURNSTILE_NAME);
    let turnstile = open_lock_file(&turnstile_path)?;
    acquire_lock(&turnstile, LockKind::Exclusive, deadline, &turnstile_path)?;

    let domain_path = state_dir.join(WRITER_DOMAIN_LOCK_NAME);
    let domain = open_lock_file(&domain_path)?;
    let result = acquire_lock(&domain, LockKind::Exclusive, deadline, &domain_path);
    let _ = FileExt::unlock(&turnstile);
    result?;
    Ok(domain)
}

fn acquire_existing_exclusive_at(state_dir: &Path, timeout: Duration) -> io::Result<File> {
    let deadline = Instant::now() + timeout;
    let turnstile_path = state_dir.join(WRITER_DOMAIN_TURNSTILE_NAME);
    let turnstile = open_existing_lock_file(&turnstile_path)?;
    acquire_lock(&turnstile, LockKind::Exclusive, deadline, &turnstile_path)?;

    let domain_path = state_dir.join(WRITER_DOMAIN_LOCK_NAME);
    let domain = open_existing_lock_file(&domain_path)?;
    let result = acquire_lock(&domain, LockKind::Exclusive, deadline, &domain_path);
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

fn open_existing_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
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
    fn absolute_protected_path_never_resolves_process_cwd() {
        let root = tempfile::tempdir().expect("tempdir");
        let absolute = root.path().join("shipyard-absolute-protected-path");
        let resolved = canonicalize_with_missing_suffix_with(&absolute, || {
            panic!("absolute path classification must not inspect the process cwd")
        })
        .expect("absolute path");

        assert_eq!(
            resolved,
            fs::canonicalize(root.path())
                .expect("canonical tempdir")
                .join("shipyard-absolute-protected-path")
        );
    }

    #[test]
    fn unrelated_test_path_never_opens_production_writer_domain() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lease = acquire_for_protected_path(&temp.path().join("state.json"))
            .expect("unrelated acquisition");
        assert!(lease.is_none());
        assert!(!temp.path().join(WRITER_DOMAIN_LOCK_NAME).exists());
    }

    #[test]
    fn concurrent_regular_creation_retries_canonicalization() {
        let temp = tempfile::tempdir().expect("tempdir");
        let candidate = temp.path().join("queue.lock");
        let mut attempts = 0;

        let resolved = canonicalize_component_with(
            &candidate,
            |path| {
                attempts += 1;
                if attempts == 1 {
                    File::create(path).expect("concurrent regular creation");
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "synthetic first lookup race",
                    ));
                }
                fs::canonicalize(path)
            },
            |path| fs::symlink_metadata(path),
        )
        .expect("retry concurrent regular creation")
        .expect("regular entry must resolve");

        assert_eq!(attempts, 2);
        assert_eq!(
            resolved,
            fs::canonicalize(&candidate).expect("canonical lock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_remains_fail_closed_during_canonicalization() {
        let temp = tempfile::tempdir().expect("tempdir");
        let candidate = temp.path().join("queue.lock");
        std::os::unix::fs::symlink(temp.path().join("missing"), &candidate)
            .expect("dangling symlink");

        let error = canonicalize_component_with(
            &candidate,
            |path| fs::canonicalize(path),
            |path| fs::symlink_metadata(path),
        )
        .expect_err("dangling symlink must refuse");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
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
                is_protected_path(&home.join(relative), home, &runtime_paths)
                    .expect("classify protected root"),
                "missing protected root {relative}"
            );
        }
        assert!(
            !is_protected_path(&home.join("Code/Shipyard"), home, &runtime_paths)
                .expect("classify unrelated root")
        );
    }

    #[test]
    fn detached_daemon_sources_never_bypass_protected_stdio_fence() {
        for (name, source) in [
            ("daemon_runtime.rs", include_str!("daemon_runtime.rs")),
            ("shadow_scheduler.rs", include_str!("shadow_scheduler.rs")),
        ] {
            for raw_macro in ["eprintln!", "eprint!", "println!", "print!"] {
                assert!(
                    !source.contains(raw_macro),
                    "{name} must route {raw_macro} diagnostics through writer_domain_lease::write_stderr"
                );
            }
        }
    }

    #[test]
    fn protected_diagnostic_writes_only_after_its_writer_lease_is_acquired() {
        let temp = tempfile::tempdir().expect("tempdir");
        let domain_path = temp.path().join(WRITER_DOMAIN_LOCK_NAME);
        let exclusive = open_lock_file(&domain_path).expect("exclusive handle");
        FileExt::lock_exclusive(&exclusive).expect("exclusive lock");

        let mut blocked = Vec::new();
        let error = write_diagnostic_with_lease(
            format_args!("must remain absent"),
            acquire_at(temp.path(), Duration::from_millis(30)),
            &mut blocked,
        )
        .expect_err("diagnostic must not bypass the exclusive audit");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(blocked.is_empty());

        FileExt::unlock(&exclusive).expect("unlock exclusive");
        let mut written = Vec::new();
        write_diagnostic_with_lease(
            format_args!("written after release"),
            acquire_at(temp.path(), Duration::from_millis(30)),
            &mut written,
        )
        .expect("diagnostic after release");
        assert_eq!(written, b"written after release\n");
    }

    #[test]
    fn relative_dotdot_and_symlink_aliases_resolve_into_protected_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let protected = home.join(".local/state/shipyard");
        fs::create_dir_all(&protected).expect("protected root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).expect("outside");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&protected, outside.join("state-link")).expect("state symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&protected, outside.join("state-link"))
            .expect("state symlink");
        let runtime_paths = RuntimePaths::for_platform(
            crate::platform::Platform::current(),
            &home,
            RuntimeMode::Shipyard,
        );

        assert!(
            is_protected_path(
                &outside.join("../home/.local/state/shipyard/new.json"),
                &home,
                &runtime_paths,
            )
            .expect("dotdot alias")
        );
        assert!(
            is_protected_path(&outside.join("state-link/new.json"), &home, &runtime_paths,)
                .expect("symlink alias")
        );
        assert_eq!(
            canonicalize_with_missing_suffix_from(Path::new("new.json"), &protected)
                .expect("relative protected path"),
            fs::canonicalize(&protected)
                .expect("canonical protected root")
                .join("new.json")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_case_alias_of_missing_protected_suffix_is_protected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        fs::create_dir_all(&home).expect("home");
        let runtime_paths = RuntimePaths::for_platform(
            crate::platform::Platform::Windows,
            &home,
            RuntimeMode::Shipyard,
        );
        assert!(
            is_protected_path(
                &home.join(".LOCAL/STATE/SHIPYARD/new.json"),
                &home,
                &runtime_paths,
            )
            .expect("case alias")
        );
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
    fn exclusive_migration_snapshot_blocks_production_writers_until_release() {
        let temp = tempfile::tempdir().expect("tempdir");
        let exclusive =
            acquire_exclusive_at(temp.path(), Duration::from_millis(50)).expect("exclusive");
        let error = acquire_at(temp.path(), Duration::from_millis(30))
            .expect_err("writer must wait behind migration snapshot");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(exclusive);
        drop(acquire_at(temp.path(), Duration::from_millis(50)).expect("writer after snapshot"));
    }

    #[test]
    fn read_only_snapshot_barrier_never_creates_missing_lock_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let before = fs::read_dir(temp.path()).expect("empty directory").count();

        let error = acquire_existing_exclusive_at(temp.path(), Duration::from_millis(50))
            .expect_err("missing read barrier must refuse without creating it");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            fs::read_dir(temp.path())
                .expect("unchanged directory")
                .count(),
            before
        );
        assert!(!temp.path().join(WRITER_DOMAIN_LOCK_NAME).exists());
        assert!(!temp.path().join(WRITER_DOMAIN_TURNSTILE_NAME).exists());
    }

    #[test]
    fn read_only_snapshot_barrier_excludes_an_existing_writer_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        drop(acquire_at(temp.path(), Duration::from_millis(50)).expect("establish generation"));
        let snapshot = acquire_existing_exclusive_at(temp.path(), Duration::from_millis(50))
            .expect("snapshot");
        let error = acquire_at(temp.path(), Duration::from_millis(30))
            .expect_err("writer must wait behind read-only snapshot");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(snapshot);
        drop(acquire_at(temp.path(), Duration::from_millis(50)).expect("writer after snapshot"));
    }

    #[test]
    fn read_only_snapshot_never_passes_a_live_production_writer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let writer = acquire_at(temp.path(), Duration::from_millis(50)).expect("writer");

        let error = acquire_existing_exclusive_at(temp.path(), Duration::from_millis(30))
            .expect_err("snapshot must wait or refuse while a writer can change authority");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(writer);
        drop(
            acquire_existing_exclusive_at(temp.path(), Duration::from_millis(50))
                .expect("snapshot after writer"),
        );
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
