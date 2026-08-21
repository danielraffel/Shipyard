//! Trusted CLI adapter for durable semantic-recovery requests.
//!
//! This module deliberately owns only machine-policy parsing, exact-head
//! revalidation, and supervised argv execution. Durable request semantics and
//! model-output validation live in `crate::recovery_worker`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::CliFailure;
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::paths::RuntimePaths;
use crate::platform::Platform;
use crate::process::ProcessTree;
use crate::recovery_worker::{
    EnqueueOutcome, RecoveryFailureFact, RecoveryRecord, RecoveryRequest, RecoveryRequiredCheck,
    RecoveryStatus, RecoveryStore,
};

const POLICY_KEY: &str = "merge_steward.recovery_worker";
const DEFAULT_FIRST_LINE_MODEL: &str = "gpt-5.3-codex-spark";
const DEFAULT_TIMEOUT_SECONDS: u64 = 120;
const DEFAULT_MAX_LOG_TAIL_BYTES: usize = 16 * 1024;
const MAX_TIMEOUT_SECONDS: u64 = 300;
const MAX_LOG_TAIL_BYTES: usize = 64 * 1024;
const MAX_DRAIN_REQUESTS: usize = 32;
const MAX_OUTPUT_WORDS: usize = 800;
const MAX_RECEIPT_DETAIL_BYTES: usize = 1_200;
const GITHUB_CALL_TIMEOUT_SECONDS: u64 = 20;
const GITHUB_STDOUT_LIMIT_BYTES: usize = 4 * 1024 * 1024;
const GITHUB_STDERR_LIMIT_BYTES: usize = 64 * 1024;
const PREFLIGHT_BUDGET_SECONDS: u64 = 60;
const TERMINAL_PERSIST_TIMEOUT_SECONDS: u64 = 5;
const FORCED_REASONING_CONFIG: &str = "model_reasoning_effort=\"low\"";
const DISABLED_CODEX_FEATURES: &[&str] = &[
    "auth_elicitation",
    "shell_tool",
    "shell_snapshot",
    "unified_exec",
    "code_mode_host",
    "hooks",
    "memories",
    "multi_agent",
    "multi_agent_v2",
    "goals",
    "apps",
    "enable_mcp_apps",
    "mcp_2026_07_28",
    "plugins",
    "plugin_sharing",
    "remote_plugin",
    "skill_mcp_dependency_install",
    "tool_call_mcp_elicitation",
    "request_permissions_tool",
    "browser_use",
    "browser_use_external",
    "browser_use_full_cdp_access",
    "in_app_browser",
    "computer_use",
    "image_generation",
    "view_image",
    "tool_suggest",
    "skill_search",
    "standalone_web_search",
    "unbounded_connection_retries",
    "workspace_dependencies",
];

#[derive(Clone, Copy, Debug)]
pub(in crate::app) struct RecoveryWorkerCommandArgs {
    pub(in crate::app) once: bool,
    pub(in crate::app) drain: bool,
    pub(in crate::app) apply: bool,
}

#[derive(Debug)]
struct WorkerProcessOutput {
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: Vec<u8>,
    stdout_truncated: bool,
    stderr: Vec<u8>,
}

#[derive(Debug)]
struct BoundedStream {
    tail: Vec<u8>,
    total_bytes: usize,
}

#[derive(Debug, Serialize)]
struct RecoveryWorkerReport {
    request_id: String,
    repo: String,
    pr: u64,
    head_sha: String,
    action: String,
    detail: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct RecoveryWitness {
    request_id: String,
    head_sha: String,
    policy_signature: String,
    failure_fingerprint: String,
    updated_at: chrono::DateTime<chrono::Utc>,
}

enum RequestDisposition {
    Current,
    Superseded(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RecoveryEnqueueDisposition {
    Disabled,
    Created(String),
    Existing(String),
}

fn recovery_store_root(state_dir: &Path) -> PathBuf {
    state_dir.join("merge-steward").join("recovery")
}

fn stable_account_home() -> Result<PathBuf, CliFailure> {
    #[cfg(unix)]
    let home = {
        let uid = nix::unistd::Uid::effective();
        nix::unistd::User::from_uid(uid)
            .map_err(|error| {
                CliFailure::new(
                    1,
                    format!("failed to resolve recovery account for uid {uid}: {error}"),
                )
            })?
            .ok_or_else(|| CliFailure::new(1, format!("no recovery account exists for uid {uid}")))?
            .dir
    };

    #[cfg(windows)]
    let home = known_folders::get_known_folder_path(known_folders::KnownFolder::Profile)
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "failed to resolve the recovery account's Windows profile directory",
            )
        })?;

    #[cfg(not(any(unix, windows)))]
    return Err(CliFailure::new(
        1,
        "recovery-worker has no stable account-home resolver on this platform",
    ));

    if !home.is_absolute() {
        return Err(CliFailure::new(
            1,
            format!(
                "recovery account home must be absolute, got {}",
                home.display()
            ),
        ));
    }
    Ok(home)
}

fn canonical_recovery_paths() -> Result<RuntimePaths, CliFailure> {
    let account_home = stable_account_home()?;
    Ok(RuntimePaths::for_platform(
        Platform::current(),
        &account_home,
        RuntimeMode::Shipyard,
    ))
}

fn ensure_canonical_recovery_paths(global_dir: &Path, state_dir: &Path) -> Result<(), CliFailure> {
    let canonical = canonical_recovery_paths()?;
    if global_dir != canonical.global_dir || state_dir != canonical.state_dir {
        return Err(CliFailure::new(
            1,
            format!(
                "recovery-worker requires canonical machine-global paths (global={}, state={}); runtime mode and path overrides cannot fork policy or attempt accounting",
                canonical.global_dir.display(),
                canonical.state_dir.display()
            ),
        ));
    }
    Ok(())
}

pub(super) fn recovery_publication_is_enabled(
    global_dir: &Path,
    state_dir: &Path,
    repo: &str,
) -> Result<bool, CliFailure> {
    ensure_canonical_recovery_paths(global_dir, state_dir)?;
    let trusted_config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to load trusted recovery-worker policy: {error}"),
            )
        })?;
    let Some(policy) = enqueue_policy(&trusted_config)? else {
        return Ok(false);
    };
    policy.signature()?;
    policy.repo_path(repo)?;
    Ok(true)
}

/// Persist one deterministic exact-head recovery request after the steward has
/// already written and revalidated its needs-agent signal.
///
/// `required_checks`, `failure_summary`, and `failure_facts` must be
/// Shipyard-normalized policy/facts; callers must never pass PR bodies,
/// comments, or contributor-controlled log prose through this trusted prompt
/// boundary.
#[allow(clippy::too_many_arguments)]
pub(super) fn enqueue_recovery_request(
    global_dir: &Path,
    state_dir: &Path,
    publication_lease: RecoveryEnqueueLease,
    repo: &str,
    pr: u64,
    base_ref: &str,
    head_sha: &str,
    merge_queue: bool,
    opt_out_label: &str,
    failure_fingerprint: &str,
    failure_summary: &str,
    required_checks: Vec<RecoveryRequiredCheck>,
    failure_facts: Vec<RecoveryFailureFact>,
    policy_signature: &str,
) -> Result<RecoveryEnqueueDisposition, CliFailure> {
    ensure_canonical_recovery_paths(global_dir, state_dir)?;
    let trusted_config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to load trusted recovery-worker policy: {error}"),
            )
        })?;
    let Some(policy) = enqueue_policy(&trusted_config)? else {
        return Ok(RecoveryEnqueueDisposition::Disabled);
    };
    let config_signature = policy.signature()?;
    let _ = policy.repo_path(repo)?;
    let request = RecoveryRequest::new_with_steward_policy(
        repo,
        pr,
        base_ref,
        head_sha,
        merge_queue,
        opt_out_label,
        failure_fingerprint,
        failure_summary,
        required_checks,
        failure_facts,
        policy_signature,
        &config_signature,
    )
    .map_err(|error| CliFailure::new(1, format!("invalid recovery request: {error}")))?;
    let id = request.id.clone();
    let store = RecoveryStore::with_max_attempts(
        recovery_store_root(state_dir),
        policy.max_attempts_per_head,
    )
    .map_err(|error| CliFailure::new(1, format!("failed to open recovery store: {error}")))?;
    if !publication_lease.covers(store.root()) {
        return Err(CliFailure::new(
            1,
            "recovery publication lease does not cover the canonical store",
        ));
    }
    let outcome = store.enqueue(request).map_err(|error| {
        CliFailure::new(1, format!("failed to enqueue recovery request: {error}"))
    })?;
    let witness_id = match &outcome {
        EnqueueOutcome::Created => Some(id.as_str()),
        EnqueueOutcome::Existing => store
            .get(&id)
            .map_err(|error| {
                CliFailure::new(1, format!("failed to reload recovery request: {error}"))
            })?
            .filter(|record| {
                matches!(
                    record.receipt.status,
                    crate::recovery_worker::RecoveryStatus::Pending
                        | crate::recovery_worker::RecoveryStatus::Running
                )
            })
            .map(|_| id.as_str()),
        EnqueueOutcome::HeadAlreadyTracked { .. } => None,
    };
    if let Some(witness_id) = witness_id {
        write_recovery_witness(
            state_dir,
            repo,
            pr,
            witness_id,
            head_sha,
            policy_signature,
            failure_fingerprint,
        )?;
    }
    // Make the transaction boundary explicit: neither clear nor a worker's
    // shared evidence read may interleave before both durable surfaces exist.
    drop(publication_lease);
    Ok(match outcome {
        EnqueueOutcome::Created => RecoveryEnqueueDisposition::Created(id),
        EnqueueOutcome::Existing => RecoveryEnqueueDisposition::Existing(id),
        EnqueueOutcome::HeadAlreadyTracked { existing_id } => {
            RecoveryEnqueueDisposition::Existing(existing_id)
        }
    })
}

mod witness;
#[cfg(test)]
pub(super) use witness::has_recovery_witness;
pub(super) use witness::with_recovery_clear_fence;
use witness::{verify_recovery_witness, write_recovery_witness};
mod lease;
use lease::{
    GlobalModelLease, acquire_global_model_lease, acquire_recovery_enqueue_read_lease,
    recovery_lease_deadline,
};
pub(super) use lease::{RecoveryEnqueueLease, acquire_recovery_enqueue_lease};

pub(super) fn acquire_recovery_publication_lease(
    state_dir: &Path,
) -> Result<RecoveryEnqueueLease, CliFailure> {
    let store = RecoveryStore::new(recovery_store_root(state_dir)).map_err(|error| {
        CliFailure::new(
            1,
            format!("failed to open recovery store for publication: {error}"),
        )
    })?;
    acquire_recovery_enqueue_lease(store.root(), recovery_lease_deadline())
}

/// Process a bounded durable-request snapshot.
pub(in crate::app) fn recovery_worker_command<W: Write>(
    args: RecoveryWorkerCommandArgs,
    _cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    ensure_canonical_recovery_paths(&runtime_paths.global_dir, &runtime_paths.state_dir)?;
    let (policy, policy_signature, trusted_config) =
        RecoveryWorkerPolicy::load(&runtime_paths.global_dir)?;
    if args.once && args.drain {
        return Err(CliFailure::new(
            2,
            "--once and --drain are mutually exclusive",
        ));
    }
    if !policy.enabled {
        return Err(CliFailure::new(
            1,
            format!("[{POLICY_KEY}] is not enabled in machine-global config"),
        ));
    }

    // The durable-store bridge is intentionally kept in one helper so the
    // CLI/config/process surface cannot accidentally acquire queue mutation
    // authority. `--drain` operates on at most one bounded initial snapshot.
    process_durable_requests(
        args.apply,
        &policy,
        &policy_signature,
        &trusted_config,
        runtime_paths,
        json,
        stdout,
        if args.drain { MAX_DRAIN_REQUESTS } else { 1 },
    )
}

#[allow(clippy::too_many_arguments)]
fn process_durable_requests<W: Write>(
    apply: bool,
    policy: &RecoveryWorkerPolicy,
    policy_signature: &str,
    trusted_config: &LoadedConfig,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
    limit: usize,
) -> Result<ExitCode, CliFailure> {
    let store_root = recovery_store_root(&runtime_paths.state_dir);
    if !apply && !store_root.exists() {
        render_reports(stdout, json, false, policy, policy_signature, &[])?;
        return Ok(ExitCode::SUCCESS);
    }
    let store = RecoveryStore::new(&store_root)
        .map_err(|error| CliFailure::new(1, format!("failed to open recovery store: {error}")))?;
    // Reconciliation is a terminal state transition. It must share the same
    // global lease as model execution so an active worker cannot be expired
    // while it is finishing bounded post-model validation or persistence.
    let model_lease = if apply {
        if let Some(lease) = acquire_global_model_lease(&canonical_global_model_lease_path()?)? {
            Some(lease)
        } else {
            let records = store.pending_read_only(limit).map_err(|error| {
                CliFailure::new(
                    1,
                    format!("failed to list deferred recovery requests: {error}"),
                )
            })?;
            let reports = records
                .iter()
                .map(|record| {
                    report(
                        &record.request,
                        "deferred_global_capacity",
                        "another recovery model invocation owns the global lease",
                    )
                })
                .collect::<Vec<_>>();
            render_reports(stdout, json, apply, policy, policy_signature, &reports)?;
            return Ok(ExitCode::SUCCESS);
        }
    } else {
        None
    };
    let stale = reconcile_orphaned_requests(&store, model_lease.as_ref())?;
    let records = if apply {
        store.pending(limit)
    } else {
        store.pending_read_only(limit)
    }
    .map_err(|error| {
        CliFailure::new(
            1,
            format!("failed to list pending recovery requests: {error}"),
        )
    })?;
    let mut reports = stale
        .into_iter()
        .map(|record| {
            report(
                &record.request,
                "failed_orphaned_running",
                "prior worker lost the machine-global model lease before terminalization",
            )
        })
        .collect::<Vec<_>>();
    reports.reserve(records.len());
    let mut had_error = false;
    for record in records {
        match process_record(ProcessRecordInputs {
            store: &store,
            record: &record,
            apply,
            policy,
            policy_signature,
            trusted_config,
            model_lease: model_lease.as_ref(),
            state_dir: &runtime_paths.state_dir,
            scratch_dir: &store_root.join("scratch"),
        }) {
            Ok(report) => reports.push(report),
            Err(error) => {
                had_error = true;
                reports.push(report_error(&record, error.message()));
            }
        }
    }
    render_reports(stdout, json, apply, policy, policy_signature, &reports)?;
    Ok(if had_error {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn reconcile_orphaned_requests(
    store: &RecoveryStore,
    model_lease: Option<&GlobalModelLease>,
) -> Result<Vec<RecoveryRecord>, CliFailure> {
    let Some(_model_lease) = model_lease else {
        return Ok(Vec::new());
    };
    store
        .reconcile_orphaned_running(
            "prior worker lost the machine-global model lease before terminalization",
        )
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to reconcile orphaned recovery workers: {error}"),
            )
        })
}

fn canonical_global_model_lease_path() -> Result<PathBuf, CliFailure> {
    Ok(canonical_recovery_paths()?
        .state_dir
        .join("merge-steward")
        .join("recovery")
        .join("global-model.lock"))
}

mod record;
use record::{ProcessRecordInputs, process_record};

mod github;
use github::inspect_request;
mod terminal;
#[cfg(test)]
use terminal::fail_after_claim;
use terminal::{bounded_detail, process_failure_detail, worker_generation};

mod report;
use report::{render_reports, report, report_error};
mod prompt;
use prompt::recovery_prompt;
mod process;
use process::run_worker_process;
mod config;
use config::{ClaimPolicyRefresh, RecoveryWorkerPolicy, enqueue_policy};
#[cfg(test)]
#[path = "recovery_worker/config_tests.rs"]
mod config_tests;
#[cfg(test)]
#[path = "recovery_worker/freshness_tests.rs"]
mod freshness_tests;
#[cfg(test)]
#[path = "recovery_worker/lease_tests.rs"]
mod lease_tests;
#[cfg(test)]
#[path = "recovery_worker/tests.rs"]
mod tests;
