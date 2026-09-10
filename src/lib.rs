#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::large_stack_arrays,
        reason = "Rust 1.92's generated libtest registry exceeds the limit; keep this waiver test-only while production and integration targets remain enforced"
    )
)]

//! Core library for Shipyard.

// Windows release builds retain selected Unix controller modules so their
// explicit fail-closed stubs remain type-checked. Keep lint accommodations on
// those modules instead of suppressing Windows diagnostics crate-wide.
/// CLI entrypoint and command dispatch.
pub mod app;
/// Immutable artifact manifests, resumable receiver-pull planning, and verified publication.
pub mod artifact_transport;
/// Classify a "Shipyard validated green but GitHub refused the merge" wedge and
/// decide whether a red required check is a flaky leg the operator can recover.
pub mod auto_rescue;
/// Remote branch creation and branch-protection application.
pub mod branch;
/// Bundle transfer command construction and path normalization.
pub mod bundle;
/// VM-slot-aware macOS capacity accounting across host-class members.
pub mod capacity;
/// Exact-head changed-surface test planning and shadow receipts.
pub mod changed_surface;
/// Changelog tag graph extraction and markdown rendering.
pub mod changelog;
/// Typed CI routing profile schema.
pub mod ci_profile;
/// Coarse failure classification shared by executors.
pub mod classify;
/// GitHub Actions workflow discovery, dispatch planning, and shell helpers.
pub mod cloud;
/// Durable cloud workflow dispatch records.
pub mod cloud_records;
/// Layered configuration loading and worktree fallback behavior.
pub mod config;
/// Pure, fail-closed translation from CTest JSON-v1 metadata into a canonical test inventory.
pub mod ctest_inventory;
/// Unix socket IPC primitives for daemon subscribers and status reads.
pub mod daemon_ipc;
/// Minimal daemon runtime and lifecycle helpers.
pub mod daemon_runtime;
/// Shared daemon/CLI version comparison helpers.
pub mod daemon_version;
#[cfg_attr(windows, allow(dead_code, unused_imports))]
pub(crate) mod daemon_worker_capacity;
/// Typed dependency-channel policy and immutable consumer locks.
pub mod dependency;
/// Phase 1 failure diagnostics for cloud (GitHub Actions) targets.
/// Fetches failing-job metadata + parses a bounded log tail so
/// `Validation failed.` becomes an actionable, structured block.
pub mod diagnostics;
/// Doctor report generation for machine and environment checks.
pub mod doctor;
/// Durable evidence records and cross-branch lookup helpers.
pub mod evidence;
pub mod execution_supervisor;
pub(crate) mod execution_termination;
/// Local and remote executor support modules.
pub mod executor;
/// Fail-closed check that a host has converged to the declared fleet epoch.
pub mod fleet_epoch;
/// Decide when a fleet verdict should leave the host and reach a human.
pub mod fleet_escalation;
/// Typed guard assertions: is a declared guard actually armed on the host, and
/// is the installed copy still the one the repo believes it deployed?
pub mod fleet_guards;
/// What a process owes when it correctly refuses to act: raise it, dispose of
/// what it held, and bound the retry.
pub mod fleet_handback;
/// Typed service assertions: is a declared lane actually being served?
/// Reconcile a host-health claim against evidence that the host is serving,
/// so a confidently wrong verdict cannot remove a working host from rotation.
pub mod fleet_health_reconciliation;
/// The leak assertion: a live object whose subject already ended is a defect.
pub mod fleet_lifecycle;
/// Typed relay assertions: does every declared hop connect inside its budget?
pub mod fleet_relay;
/// Bounded self-heal gate: is a corrective action provably safe to take?
pub mod fleet_selfheal;
pub mod fleet_service;
/// Typed slot assertions: is a free macOS VM slot being withheld, and why?
pub mod fleet_slot;
/// Assert that a lane's service survives an ordinary exit, not merely that it
/// is serving right now.
pub mod fleet_supervision;
/// Typed supervisor assertions: can the thing that boots VMs see the work?
pub mod fleet_supervisor;
/// Repo-local gate script resolution for `shipyard pr`.
pub mod gate_scripts;
/// Shared GitHub CLI command boundary and auth resolution.
pub mod gh;
/// Branch governance profiles and GitHub branch-protection helpers.
pub mod governance;
/// Optional host-health pre-dispatch gate (reads the `host_vitals` signal).
pub mod host_health;
/// Local host-pool configuration and lease state.
pub mod host_pool;
/// Product naming and runtime-mode identity.
pub mod identity;
#[cfg_attr(
    windows,
    allow(
        dead_code,
        unused_variables,
        clippy::unnecessary_wraps,
        clippy::unused_self
    )
)]
pub(crate) mod immutable_store;
/// Project initialization and ecosystem detection.
pub mod init_config;
/// Job and target-result domain types used by executors and queues.
pub mod job;
/// Advisory-vs-required lane policy resolution.
pub mod lane_policy;
/// Bounded log rotation, terminal classification, and retention primitives.
pub mod log_retention;
/// Merge-queue enqueue / poll / eviction supervision engine.
pub mod merge_queue;
/// Fleet authority, serialization, hold, and audit controls for queue writes.
pub mod merge_queue_control;
/// Read-only merge-queue front/check/fleet liveness correlation.
pub mod merge_queue_liveness;
/// Conservative cross-repository merge and queued-run stewardship.
pub mod merge_steward;
pub mod metadata_authority;
/// Runner and CI timing metrics store and analysis helpers.
pub mod metrics;
mod native_executable;
/// Structured JSON output helpers.
pub mod output;
/// Shadow-only build-once and sharded-test proof invariants.
pub mod parallel_proof;
/// Default-off admission policy for the first Pulp macOS sharding canary.
pub mod parallel_proof_canary;
#[cfg_attr(windows, allow(dead_code, unused_imports))]
pub(crate) mod parallel_proof_canary_adapter;
/// Default-off immutable cache-generation observation and evidence foundation.
pub mod parallel_proof_canary_cache;
/// Read-only physical-readiness observations for the Pulp macOS canary.
#[cfg_attr(
    windows,
    allow(dead_code, unused_imports, unused_variables, clippy::unused_self)
)]
pub mod parallel_proof_canary_controller;
/// Default-off controller driver for measured Pulp macOS shadow receipts.
pub mod parallel_proof_canary_driver;
/// Restart-reconcilable, session-independent execution custody for protected canary jobs.
pub mod parallel_proof_canary_job;
/// Typed daemon-supervisor adapter for restartable parallel-proof canary jobs.
pub mod parallel_proof_canary_job_adapter;
/// Immutable measurement receipts for the default-off Pulp macOS sharding canary.
pub mod parallel_proof_canary_receipt;
/// Authenticated companion protocol for read-only remote M1 cache observation.
pub mod parallel_proof_canary_remote_cache;
/// Default-off one-host build-once consumption proof for the Pulp M3 shadow canary.
pub mod parallel_proof_one_host;
/// Filesystem path resolution for isolated and compatible modes.
pub mod paths;
/// Consumer repository Shipyard pin helpers.
pub mod pin;
/// Platform detection used by pure path-resolution logic.
pub mod platform;
/// Pull request shell boundary used by `ship`.
pub mod pr;
/// Pull request title/body composition.
pub mod pr_text;
/// Submission preflight checks for `ship --pr`.
pub mod preflight;
/// Prepared-state cache for warm stage reruns.
pub mod prepared_state;
mod process;
/// Proof gates for applying a routing profile to GitHub variables.
pub mod profile_apply;
/// Durable queue write helpers and retry policy.
pub mod queue;
/// Crash-safe, opt-in recovery of exact ship work missing from the queue.
pub mod queue_absent_recovery;
/// Stable read-only GitHub queue snapshots, state hashing, and delta tracking.
pub mod queue_observer;
/// Durable queued execution request and outcome stores.
pub mod queue_request;
/// Cooperative queue scheduler planning primitives.
pub mod queue_scheduler;
/// Best-effort reconciliation of durable ship-state against GitHub truth.
pub mod reconcile;
mod record_identity;
/// Durable, fail-closed requests for bounded model-assisted recovery.
pub mod recovery_worker;
/// GitHub webhook registration through the user's existing `gh` auth.
pub mod registrar;
/// Shared parsing for classic and ruleset required-check policies.
pub mod required_check_policy;
/// Cloud→local macOS reroute decision logic (#316 Part C).
pub mod reroute;
/// Self-hosted runner provisioning (register/list/remove) pure logic.
pub mod runner_provision;
/// Self-hosted runner watchdog detection logic.
pub mod runner_watchdog;
/// Ship execution orchestration helpers.
pub mod ship;
/// Read-only orphan/liveness classification for in-flight ship states.
pub mod ship_liveness;
/// Opt-in, default-off daemon sweep that abandons orphaned in-flight states.
pub mod ship_resume;
/// Opt-in, default-off same-backend retry policy for transient local legs.
pub mod ship_retry;
/// Durable in-flight ship-state model and store.
pub mod ship_state;
/// GitHub stacked pull request discovery and initial fail-closed policy.
pub mod stacked_pr;
/// Pulp-specific stale PR workflow concurrency-wedge classification.
pub mod stale_pr_wedge;
/// Subprocess helpers that mark supervised child processes with
/// `SHIPYARD_PR_RUNNING=1` (issue #266). Used by every `git` / `gh`
/// spawn site that participates in the supervised PR / ship / merge
/// pipeline; diagnostic subcommands deliberately skip this.
pub mod supervised;
mod terminal_delivery_authority;
/// Working-tree drift detection shared by future `shipyard run` wiring.
pub mod tree_drift;
/// Tunnel readiness, Tailscale probe decoding, and supervisor retry policy.
pub mod tunnel;
/// Fail-closed policy primitives for contributor-controlled review requests.
pub mod untrusted;
/// Pure truth evaluators for `shipyard wait`.
pub mod wait;
/// Transport orchestration and snapshot fetching for `shipyard wait`.
pub mod wait_transport;
/// Warm-pool runner reuse state and helper contracts.
pub mod warm_pool;
/// Watch-mode rendering and terminal-verdict logic.
pub mod watch;
/// GitHub webhook signature validation and event decoding.
pub mod webhook;
pub(crate) mod worker_process_custody;
/// Fail-closed policy for automated workflow-run cancellation.
pub mod workflow_cancellation;
/// Host-global production-writer coordination for sandbox E2E isolation.
mod writer_domain_lease;

#[cfg(all(test, unix))]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{LazyLock, Mutex};

    /// Serializes tests that create, adopt, signal, or reap Unix process trees.
    ///
    /// A module-local lock is insufficient: the daemon lifecycle and execution
    /// supervisor suites run in the same test binary and can otherwise signal
    /// or observe each other's short-lived fixture processes.
    pub(crate) static PROCESS_TREE_TEST_LOCK: LazyLock<Mutex<()>> =
        LazyLock::new(|| Mutex::new(()));

    /// Take the process-tree lock, ignoring poisoning.
    ///
    /// This mutex guards `()`. It carries no state, so a panicking holder
    /// cannot have left an invariant broken — poisoning here says only "some
    /// earlier test failed", which the harness already reports.
    ///
    /// Honouring it is actively harmful: `lock().expect(..)` turns one failing
    /// test into a cascade, because every later test panics on the poison
    /// rather than on anything of its own. A single real assertion failure was
    /// observed producing sixteen such cascades in one CI run, which buries the
    /// one line that mattered under sixteen that did not.
    pub(crate) fn lock_process_tree_for_test() -> std::sync::MutexGuard<'static, ()> {
        PROCESS_TREE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Force the poisoning cascade deterministically, then show the fix.
    ///
    /// No timing, no race, no retry: a holder panics, and from that instant the
    /// raw `lock()` returns `Err` **forever** for every later caller in the
    /// binary. That is the whole mechanism. One real assertion failure in
    /// `merge_steward_cmd` was observed turning into sixteen further reds this
    /// way, none of which had anything wrong with them.
    ///
    /// The first assertion is the planted control. Without it this test would
    /// still pass against a mutex that was never poisoned at all, and would be
    /// proving nothing.
    ///
    /// This test poisons the lock that the other suites in this binary share.
    /// That is deliberate and is the integration half of the proof: every one
    /// of them goes on to acquire it through the helper regardless of the order
    /// the harness happens to run them in.
    ///
    /// A panic message is printed while this runs. It is expected.
    #[test]
    fn forcing_a_panic_under_the_tree_lock_poisons_it_and_the_helper_serves_it_anyway() {
        let panicked = std::panic::catch_unwind(|| {
            let _guard = lock_process_tree_for_test();
            panic!("deliberate poison, expected by this test: a holder failed");
        });
        assert!(panicked.is_err(), "the forcing panic did not happen");

        // Control: prove the lock really is poisoned, so the assertion below is
        // not passing vacuously against a healthy mutex.
        assert!(
            PROCESS_TREE_TEST_LOCK.lock().is_err(),
            "control failed: the lock was not poisoned, so this test proves nothing"
        );

        // The fix: a later caller is served rather than cascaded into.
        let _guard = lock_process_tree_for_test();
    }

    /// Write an executable shell fixture without ever holding a writable
    /// descriptor on it in this process.
    ///
    /// A test that writes a script and then runs it races the rest of the
    /// suite. Writing the file here leaves a writable descriptor open on the
    /// inode for the duration of the write, and any other test thread that
    /// spawns a process in that window forks first and execs second. The forked
    /// child inherits a duplicate of that descriptor until its own exec runs,
    /// because `O_CLOEXEC` closes descriptors at exec and not at fork. While
    /// the duplicate is open the kernel refuses to exec the inode and returns
    /// `ETXTBSY` (`Text file busy`, os error 26).
    ///
    /// Giving each test its own directory does not help: the descriptor refers
    /// to the inode, not the path. Staging the script and renaming it into
    /// place does not help either, for the same reason. Only Linux enforces the
    /// rule, so the failure is invisible on macOS and surfaces on the Linux
    /// leg, most often under coverage instrumentation where every process lives
    /// longer and the fork window widens.
    ///
    /// So a child process opens the file, writes it, and marks it executable,
    /// and this call waits for that child to exit. No thread of this process
    /// ever owns a writable descriptor on the script, no sibling fork can
    /// inherit one, and the file is exec-ready the moment this returns.
    pub(crate) fn write_executable_script(path: &Path, contents: &str) {
        write_executable_script_with_mode(path, contents, 0o755);
    }

    /// `write_executable_script` with an explicit permission mode.
    pub(crate) fn write_executable_script_with_mode(path: &Path, contents: &str, mode: u32) {
        use std::io::Write as _;

        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"cat > "$1" && chmod "$2" "$1""#)
            .arg("shipyard-write-executable-script")
            .arg(path)
            .arg(format!("{mode:o}"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn writer for {}: {error}", path.display()));

        let mut stdin = child
            .stdin
            .take()
            .unwrap_or_else(|| panic!("writer stdin for {}", path.display()));
        // A writer that died early, on a missing parent directory say, closes
        // the pipe and this write then fails with EPIPE. The child's own exit
        // status and stderr name the real cause, so report those first and keep
        // the write error as a fallback.
        let written = stdin.write_all(contents.as_bytes());
        drop(stdin);

        let output = child
            .wait_with_output()
            .unwrap_or_else(|error| panic!("await writer for {}: {error}", path.display()));
        assert!(
            output.status.success(),
            "writer failed for {}: {} {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        written.unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    }

    #[test]
    fn a_script_it_writes_is_executable_immediately() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("fixture");
        write_executable_script(&path, "#!/bin/sh\nexit 7\n");
        let status = Command::new(&path).status().expect("run fixture");
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn it_honours_an_explicit_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("fixture");
        write_executable_script_with_mode(&path, "#!/bin/sh\nexit 7\n", 0o700);
        let mode = std::fs::metadata(&path)
            .expect("fixture metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// Force the race the helper exists to remove, then show the helper is
    /// clear of it.
    ///
    /// The subject runs first and proves this body and mode execute at all. The
    /// control then proves that the identical body, written in this process with
    /// the handle still open, is refused with errno 26. Without that pairing the
    /// subject would pass on any kernel that never enforces `ETXTBSY` and would
    /// be proving nothing. Only Linux enforces it, so this runs only there.
    ///
    /// The control deliberately never re-execs after closing its handle. That
    /// exec is the very shape this helper exists to remove: a sibling thread
    /// that forked during the write still holds an inherited duplicate, so the
    /// exec can fail for a reason that has nothing to do with this test. The
    /// subject already establishes that the body and mode are runnable.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_retained_write_handle_is_the_race_the_helper_removes() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let body = "#!/bin/sh\nexit 7\n";

        let fixed = temp.path().join("fixed");
        write_executable_script(&fixed, body);
        assert_eq!(
            Command::new(&fixed).status().expect("run fixture").code(),
            Some(7),
            "the helper must write a body that execs immediately"
        );

        let busy = temp.path().join("busy");
        let mut handle = std::fs::File::create(&busy).expect("create control");
        handle.write_all(body.as_bytes()).expect("write control");
        handle.flush().expect("flush control");
        let mut permissions = std::fs::metadata(&busy)
            .expect("control metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&busy, permissions).expect("chmod control");
        let refused = Command::new(&busy)
            .status()
            .expect_err("exec must be refused while the write handle is open");
        assert_eq!(
            refused.raw_os_error(),
            Some(26),
            "control did not reproduce ETXTBSY, so this test proves nothing"
        );
        drop(handle);
    }

    /// Compile a tiny native fixture when a security boundary deliberately
    /// rejects script wrappers. The fixture is scoped to the caller's tempdir.
    pub(crate) fn compile_native_test_program(
        directory: &Path,
        output_name: &str,
        source: &str,
    ) -> PathBuf {
        let source_path = directory.join(format!("{output_name}_fixture.rs"));
        let output_path = directory.join(output_name);
        std::fs::write(&source_path, source).expect("write native fixture source");
        let output = Command::new("rustc")
            .args(["--edition=2024", "--crate-name", "shipyard_native_fixture"])
            .arg(&source_path)
            .args(["-C", "debuginfo=0", "-o"])
            .arg(&output_path)
            .output()
            .expect("compile native fixture");
        assert!(
            output.status.success(),
            "native fixture compilation failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output_path
    }
}
