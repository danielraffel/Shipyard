#![forbid(unsafe_code)]

//! Core library for Shipyard.

/// CLI entrypoint and command dispatch.
pub mod app;
/// Classify a "Shipyard validated green but GitHub refused the merge" wedge and
/// decide whether a red required check is a flaky leg the operator can recover.
pub mod auto_rescue;
/// Remote branch creation and branch-protection application.
pub mod branch;
/// Bundle transfer command construction and path normalization.
pub mod bundle;
/// VM-slot-aware macOS capacity accounting across host-class members.
pub mod capacity;
/// Changelog tag graph extraction and markdown rendering.
pub mod changelog;
/// Coarse failure classification shared by executors.
pub mod classify;
/// GitHub Actions workflow discovery, dispatch planning, and shell helpers.
pub mod cloud;
/// Durable cloud workflow dispatch records.
pub mod cloud_records;
/// Layered configuration loading and worktree fallback behavior.
pub mod config;
/// Unix socket IPC primitives for daemon subscribers and status reads.
pub mod daemon_ipc;
/// Minimal daemon runtime and lifecycle helpers.
pub mod daemon_runtime;
/// Shared daemon/CLI version comparison helpers.
pub mod daemon_version;
/// Phase 1 failure diagnostics for cloud (GitHub Actions) targets.
/// Fetches failing-job metadata + parses a bounded log tail so
/// `Validation failed.` becomes an actionable, structured block.
pub mod diagnostics;
/// Doctor report generation for machine and environment checks.
pub mod doctor;
/// Durable evidence records and cross-branch lookup helpers.
pub mod evidence;
/// Local and remote executor support modules.
pub mod executor;
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
/// Merge-queue enqueue / poll / eviction supervision engine.
pub mod merge_queue;
/// Fleet authority, serialization, hold, and audit controls for queue writes.
pub mod merge_queue_control;
/// Read-only merge-queue front/check/fleet liveness correlation.
pub mod merge_queue_liveness;
/// Conservative cross-repository merge and queued-run stewardship.
pub mod merge_steward;
/// Runner and CI timing metrics store and analysis helpers.
pub mod metrics;
/// Structured JSON output helpers.
pub mod output;
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
/// Durable queue write helpers and retry policy.
pub mod queue;
/// Durable queued execution request and outcome stores.
pub mod queue_request;
/// Cooperative queue scheduler planning primitives.
pub mod queue_scheduler;
/// Best-effort reconciliation of durable ship-state against GitHub truth.
pub mod reconcile;
/// GitHub webhook registration through the user's existing `gh` auth.
pub mod registrar;
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
