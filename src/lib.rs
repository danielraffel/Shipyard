#![forbid(unsafe_code)]

//! Core library for Shipyard.

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
pub(crate) mod provider_wrapper;
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
/// Subscriber-independent, read-only canonical-ledger shadow observation.
pub mod shadow_scheduler;
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
/// Subprocess helpers that mark supervised child processes with
/// `SHIPYARD_PR_RUNNING=1` (issue #266). Used by every `git` / `gh`
/// spawn site that participates in the supervised PR / ship / merge
/// pipeline; diagnostic subcommands deliberately skip this.
pub mod supervised;
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
/// Shadow-only canonical work-item ledger and legacy-state importer.
pub mod work_ledger;
/// Fail-closed policy for automated workflow-run cancellation.
pub mod workflow_cancellation;
pub(crate) mod workstream_activation_loader;
/// Trusted machine-global policy for future workstream continuation dispatch.
pub mod workstream_continuation_config;
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
