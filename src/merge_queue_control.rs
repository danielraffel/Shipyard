//! Machine authority, serialization, and audit controls for merge-queue writes.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use fs2::FileExt;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::ship_state::{ShipState, ShipStateStore};

/// Name of the state-root sentinel that blocks every merge-queue mutation.
pub const HOLD_FILE: &str = "merge_queue/HOLD";
const CONTROL_LOCK_FILE: &str = "merge_queue/control.lock";
const AUDIT_FILE: &str = "merge_queue/mutations.jsonl";
static CORRELATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct ControlLock(Option<File>);

impl ControlLock {
    fn new(file: File) -> Self {
        Self(Some(file))
    }

    fn unlock(&mut self) -> io::Result<()> {
        let Some(file) = self.0.take() else {
            return Ok(());
        };
        file.unlock()
    }
}

impl Drop for ControlLock {
    fn drop(&mut self) {
        let _ = self.unlock();
    }
}

/// Create or replace the local authority hold with a durable reason record.
pub fn hold(state_root: &Path, reason: &str) -> Result<PathBuf, String> {
    hold_with_lock_boundary_signal(state_root, reason, || {})
}

fn hold_with_lock_boundary_signal(
    state_root: &Path,
    reason: &str,
    at_lock_boundary: impl FnOnce(),
) -> Result<PathBuf, String> {
    let mut control_lock =
        acquire_control_lock_with_boundary_signal(state_root, false, at_lock_boundary)?;
    let path = state_root.join(HOLD_FILE);
    let parent = path
        .parent()
        .ok_or_else(|| "merge-queue hold path has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create merge-queue control directory: {error}"))?;
    let payload = json!({
        "held_at": Utc::now(),
        "reason": reason,
        "pid": process::id(),
        "machine": read_machine_tag(state_root).as_deref().unwrap_or("unconfigured"),
    });
    let temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("failed to create merge-queue hold record: {error}"))?;
    fs::write(temp.path(), format!("{payload}\n"))
        .map_err(|error| format!("failed to write merge-queue hold record: {error}"))?;
    temp.persist(&path)
        .map_err(|error| format!("failed to persist merge-queue hold: {error}"))?;
    control_lock
        .unlock()
        .map_err(|error| format!("failed to release merge-queue control lock: {error}"))?;
    Ok(path)
}

/// Remove the local authority hold. Returns false when no hold existed.
pub fn resume(state_root: &Path) -> Result<bool, String> {
    let mut control_lock = acquire_control_lock(state_root, false)?;
    let path = state_root.join(HOLD_FILE);
    if !path.exists() {
        control_lock
            .unlock()
            .map_err(|error| format!("failed to release merge-queue control lock: {error}"))?;
        return Ok(false);
    }
    fs::remove_file(&path).map_err(|error| {
        format!(
            "failed to remove merge-queue hold {}: {error}",
            path.display()
        )
    })?;
    control_lock
        .unlock()
        .map_err(|error| format!("failed to release merge-queue control lock: {error}"))?;
    Ok(true)
}

/// Read the durable hold payload when queue mutations are paused.
pub fn hold_status(state_root: &Path) -> Result<Option<serde_json::Value>, String> {
    let path = state_root.join(HOLD_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path).map_err(|error| {
        format!(
            "failed to read merge-queue hold {}: {error}",
            path.display()
        )
    })?;
    let status = serde_json::from_str(&contents).map_err(|error| {
        format!(
            "merge-queue hold {} is malformed; mutations remain blocked: {error}",
            path.display()
        )
    })?;
    Ok(Some(status))
}

/// Report whether this machine matches the trusted machine-global mutation
/// authority. Repository and checkout-local configuration are never consulted.
pub fn authority_status(
    state_root: &Path,
    _cwd: &Path,
    _mode: RuntimeMode,
    global_dir: &Path,
) -> Result<serde_json::Value, String> {
    let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
        .map_err(|error| format!("failed to load merge-queue mutation policy: {error}"))?;
    let authority = match config.get("merge_queue.mutation_machine") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    "merge_queue.mutation_machine must be a non-empty string".to_owned()
                })?,
        ),
    };
    if let Some(authority) = authority {
        crate::runner_provision::validate_machine_tag(authority)?;
    }
    let machine = read_machine_tag(state_root);
    Ok(json!({
        "machine": machine,
        "mutation_machine": authority,
        "authority_configured": authority.is_some(),
        "authority_matches": authority.is_some() && machine.as_deref() == authority,
    }))
}

/// Fail before any remote side effect when a workflow intends to perform a
/// later merge-queue mutation but does not yet know the PR identity.
#[derive(Debug)]
pub struct MergeQueueMutationPreflight {
    control_lock: ControlLock,
    global_dir: PathBuf,
}

/// Validate authority and retain process-wide serialization until the caller
/// converts this preflight into a PR-specific audited mutation guard.
pub fn preflight_mutation_authority(
    state_root: &Path,
    _cwd: &Path,
    _mode: RuntimeMode,
    global_dir: &Path,
    repo: &str,
    base: &str,
) -> Result<MergeQueueMutationPreflight, String> {
    fs::create_dir_all(state_root.join("merge_queue"))
        .map_err(|error| format!("failed to create merge-queue control directory: {error}"))?;
    let control_lock = acquire_control_lock(state_root, true)?;
    let hold_path = state_root.join(HOLD_FILE);
    if hold_path.exists() {
        return Err(format!(
            "merge-queue mutations are centrally held by {}",
            hold_path.display()
        ));
    }
    // A workflow that does not know its PR identity yet must not change a
    // remote branch while any prior mutation remains ambiguous: an existing
    // auto-merge could otherwise consume the newly pushed head.
    if let Some(uncertain) = uncertain_mutations(state_root)?.iter().find(|entry| {
        entry.get("repo").and_then(serde_json::Value::as_str) == Some(repo)
            && entry.get("base").and_then(serde_json::Value::as_str) == Some(base)
    }) {
        let correlation_id = uncertain
            .get("correlation_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<correlation-id>");
        return Err(format!(
            "merge-queue mutation {correlation_id} for {repo}/{base} is uncertain; reconcile it before remote branch changes"
        ));
    }
    let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
        .map_err(|error| format!("failed to load merge-queue mutation policy: {error}"))?;
    validate_machine_authority(&config, state_root)?;
    Ok(MergeQueueMutationPreflight {
        control_lock,
        global_dir: global_dir.to_path_buf(),
    })
}

/// Return mutation starts that have no durable finished record. Hard process
/// termination cannot run `Drop`, so readers classify these as uncertain.
pub fn uncertain_mutations(state_root: &Path) -> Result<Vec<serde_json::Value>, String> {
    let audit_path = state_root.join(AUDIT_FILE);
    let Some((_audit_lock, ordered_paths)) = locked_audit_paths(&audit_path)? else {
        return Ok(Vec::new());
    };
    read_uncertain_from_paths(&ordered_paths)
}

fn read_uncertain_from_paths(ordered_paths: &[PathBuf]) -> Result<Vec<serde_json::Value>, String> {
    let mut started = BTreeMap::new();
    for path in ordered_paths {
        let contents = fs::read_to_string(path).map_err(|error| {
            format!(
                "failed to read merge-queue mutation audit {}: {error}; mutations remain blocked",
                path.display()
            )
        })?;
        for (index, line) in contents.lines().enumerate() {
            apply_audit_line(path, index, line, &mut started)?;
        }
    }
    Ok(started.into_values().collect())
}

fn locked_audit_paths(audit_path: &Path) -> Result<Option<(File, Vec<PathBuf>)>, String> {
    let Some(audit_dir) = audit_path.parent() else {
        return Ok(None);
    };
    if !audit_dir.is_dir() {
        return Ok(None);
    }
    let audit_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(audit_path.with_extension("lock"))
        .map_err(|error| format!("failed to lock merge-queue mutation audit: {error}"))?;
    audit_lock
        .lock_exclusive()
        .map_err(|error| format!("failed to lock merge-queue mutation audit: {error}"))?;
    Ok(ordered_audit_paths(audit_path)?.map(|paths| (audit_lock, paths)))
}

fn ordered_audit_paths(audit_path: &Path) -> Result<Option<Vec<PathBuf>>, String> {
    let Some(audit_dir) = audit_path.parent() else {
        return Ok(None);
    };
    if !audit_dir.is_dir() {
        return Ok(None);
    }
    let mut audit_paths = fs::read_dir(audit_dir)
        .map_err(|error| format!("failed to list merge-queue mutation audit: {error}"))?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            name.strip_prefix("mutations.jsonl.")?
                .parse::<usize>()
                .ok()
                .map(|index| (index, path))
        })
        .collect::<Vec<_>>();
    audit_paths.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
    let mut ordered_paths = audit_paths
        .into_iter()
        .map(|(_, path)| path)
        .collect::<Vec<_>>();
    if audit_path.exists() && !audit_path.is_file() {
        return Err(format!(
            "failed to read merge-queue mutation audit {}: not a regular file; mutations remain blocked",
            audit_path.display()
        ));
    }
    if audit_path.is_file() {
        ordered_paths.push(audit_path.to_path_buf());
    }
    if ordered_paths.is_empty() {
        return Ok(None);
    }
    Ok(Some(ordered_paths))
}

fn apply_audit_line(
    path: &Path,
    index: usize,
    line: &str,
    started: &mut BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    let mut value = serde_json::from_str::<serde_json::Value>(line).map_err(|error| {
        format!(
            "malformed merge-queue mutation audit {} at line {}: {error}; mutations remain blocked",
            path.display(),
            index + 1
        )
    })?;
    let Some(correlation_id) = value
        .get("correlation_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
    else {
        return Err(format!(
            "malformed merge-queue mutation audit {} at line {}: missing correlation_id; mutations remain blocked",
            path.display(),
            index + 1
        ));
    };
    match value.get("phase").and_then(serde_json::Value::as_str) {
        Some("started") => {
            if let Some(object) = value.as_object_mut() {
                object.insert("outcome".to_owned(), json!("uncertain"));
            }
            started.insert(correlation_id, value);
        }
        Some("finished") => {
            if value.get("outcome").and_then(serde_json::Value::as_str) == Some("uncertain") {
                if let Some(existing) = started.get_mut(&correlation_id) {
                    if let Some(object) = existing.as_object_mut() {
                        object.insert("outcome".to_owned(), json!("uncertain"));
                        object.insert(
                            "finished_at".to_owned(),
                            value
                                .get("timestamp")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        );
                    }
                } else {
                    started.insert(correlation_id, value);
                }
            } else {
                started.remove(&correlation_id);
            }
        }
        _ => {
            return Err(format!(
                "malformed merge-queue mutation audit {} at line {}: invalid phase; mutations remain blocked",
                path.display(),
                index + 1
            ));
        }
    }
    Ok(())
}

/// Resolve one previously uncertain mutation after authoritative GitHub
/// reconciliation. The resolution itself is serialized and audited.
pub fn resolve_uncertainty(
    state_root: &Path,
    global_dir: &Path,
    correlation_id: &str,
    outcome: &str,
    reason: &str,
) -> Result<(), String> {
    if !matches!(outcome, "accepted" | "rejected") {
        return Err("uncertain mutation outcome must be `accepted` or `rejected`".to_owned());
    }
    let mut control_lock = acquire_control_lock(state_root, false)?;
    let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
        .map_err(|error| format!("failed to load merge-queue log policy: {error}"))?;
    let retention_policy = crate::log_retention::LogRetentionPolicy::from_config(&config);
    let unresolved = uncertain_mutations(state_root)?;
    let Some(entry) = unresolved.iter().find(|entry| {
        entry
            .get("correlation_id")
            .and_then(serde_json::Value::as_str)
            == Some(correlation_id)
    }) else {
        return Err(format!(
            "no uncertain merge-queue mutation has correlation id `{correlation_id}`"
        ));
    };
    if let Some(pr) = entry.get("pr").and_then(serde_json::Value::as_u64) {
        let repo = entry
            .get("repo")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "uncertain mutation is missing repository identity".to_owned())?;
        let store = ShipStateStore::new(state_root.join("ship"))
            .map_err(|error| format!("failed to open ship-state store: {error}"))?;
        let lock = store
            .lock_pr_scoped(repo, pr)
            .map_err(|error| format!("failed to lock ship-state for PR #{pr}: {error}"))?;
        if let Some(mut state) = store.get_locked_scoped(repo, pr, &lock) {
            let action = entry
                .get("action")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let enqueue = action == "enqueue pull request";
            let revocation = matches!(action, "disable native auto-merge" | "dequeue drifted PR");
            let identity_matches = entry.get("repo").and_then(serde_json::Value::as_str)
                == Some(state.repo.as_str())
                && entry.get("head").and_then(serde_json::Value::as_str)
                    == Some(state.head_sha.as_str())
                && (revocation
                    || entry.get("base").and_then(serde_json::Value::as_str)
                        == Some(state.base_branch.as_str()));
            if identity_matches && enqueue {
                state.merge_queue_enqueue_started_at = None;
                if outcome == "accepted" {
                    let started_at = entry
                        .get("timestamp")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                        .map_or_else(Utc::now, |value| value.with_timezone(&Utc));
                    state.merge_queue_attempt_started_at = Some(started_at);
                    state
                        .merge_queue_enqueue_succeeded_at
                        .get_or_insert_with(Utc::now);
                } else {
                    state.merge_queue_enqueue_succeeded_at = None;
                }
            }
            if identity_matches {
                state.touch();
                store.save_scoped_locked(&state, &lock).map_err(|error| {
                    format!("failed to reconcile ship-state for PR #{pr}: {error}")
                })?;
            }
        }
    }
    append_audit(
        &state_root.join(AUDIT_FILE),
        retention_policy,
        &json!({
            "timestamp": Utc::now(),
            "correlation_id": correlation_id,
            "phase": "finished",
            "outcome": format!("resolved_{outcome}"),
            "reason": reason,
            "resolver_pid": process::id(),
            "resolver_machine": read_machine_tag(state_root)
                .as_deref()
                .unwrap_or("unconfigured"),
        }),
    )
    .map_err(|error| format!("failed to write merge-queue mutation resolution: {error}"))?;
    control_lock
        .unlock()
        .map_err(|error| format!("failed to release merge-queue control lock: {error}"))
}

/// Close an uncertain mutation without claiming whether the remote request was
/// accepted. Used when an exact terminal observation or a freshly revalidated
/// idempotent retry makes the ambiguous request irrelevant.
pub fn supersede_uncertainty(
    state_root: &Path,
    global_dir: &Path,
    correlation_id: &str,
    reason: &str,
) -> Result<bool, String> {
    let mut control_lock = acquire_control_lock(state_root, false)?;
    if !uncertain_mutations(state_root)?.iter().any(|entry| {
        entry
            .get("correlation_id")
            .and_then(serde_json::Value::as_str)
            == Some(correlation_id)
    }) {
        return Ok(false);
    }
    let hold_path = state_root.join(HOLD_FILE);
    if hold_path.exists() {
        return Err(format!(
            "merge-queue mutations are centrally held by {}",
            hold_path.display()
        ));
    }
    let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
        .map_err(|error| format!("failed to load merge-queue mutation policy: {error}"))?;
    validate_machine_authority(&config, state_root)?;
    append_audit(
        &state_root.join(AUDIT_FILE),
        crate::log_retention::LogRetentionPolicy::from_config(&config),
        &json!({
            "timestamp": Utc::now(),
            "correlation_id": correlation_id,
            "phase": "finished",
            "outcome": "superseded",
            "reason": reason,
            "resolver_pid": process::id(),
        }),
    )
    .map_err(|error| format!("failed to supersede merge-queue mutation: {error}"))?;
    control_lock
        .unlock()
        .map_err(|error| format!("failed to release merge-queue control lock: {error}"))?;
    Ok(true)
}

/// Exclusive authority for one repository/base merge queue mutation.
#[derive(Debug)]
pub struct MergeQueueMutationGuard {
    control_lock: ControlLock,
    lock: File,
    audit_path: PathBuf,
    correlation_id: String,
    action: String,
    finished: bool,
    retention_policy: crate::log_retention::LogRetentionPolicy,
    repo: String,
    base: String,
    pr: u64,
    head: String,
}

/// A mutation correlation that may be persisted before authority is acquired.
///
/// Crash-resumable callers write this identifier into their own durable state,
/// then acquire the normal mutation guard with the same identifier. On restart,
/// [`Self::resume`] provides typed access to uncertainty reconciliation without
/// exposing the mutation audit's JSON representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableMutationIntent {
    correlation_id: String,
}

impl DurableMutationIntent {
    /// Allocate a fresh correlation for a write-ahead mutation intent.
    #[must_use]
    pub fn new() -> Self {
        Self {
            correlation_id: MergeQueueMutationGuard::new_correlation_id(),
        }
    }

    /// Rehydrate a correlation previously persisted by the caller.
    pub fn resume(correlation_id: &str) -> Result<Self, String> {
        if correlation_id.trim().is_empty() {
            return Err("invalid durable merge-queue mutation correlation".to_owned());
        }
        Ok(Self {
            correlation_id: correlation_id.to_owned(),
        })
    }

    /// Identifier the caller must persist before acquiring mutation authority.
    #[must_use]
    pub fn correlation_id(&self) -> &str {
        &self.correlation_id
    }

    /// Validate authority before the caller persists its write-ahead record.
    pub fn validate(
        &self,
        store: &ShipStateStore,
        global_dir: &Path,
        state: &ShipState,
    ) -> Result<(), String> {
        if self.correlation_id.is_empty() {
            return Err("durable merge-queue mutation correlation is empty".to_owned());
        }
        MergeQueueMutationGuard::validate_in_mode(store, global_dir, state)
    }

    /// Acquire mutation authority using this already-persisted correlation.
    pub fn acquire(
        &self,
        store: &ShipStateStore,
        mode: RuntimeMode,
        global_dir: &Path,
        state: &ShipState,
        action: &str,
    ) -> Result<MergeQueueMutationGuard, String> {
        MergeQueueMutationGuard::acquire_in_mode_with_correlation(
            store,
            store.path(),
            mode,
            global_dir,
            state,
            action,
            &self.correlation_id,
        )
    }

    /// Whether this exact persisted mutation remains unresolved.
    pub fn is_uncertain(&self, state_root: &Path) -> Result<bool, String> {
        Ok(uncertain_mutations(state_root)?.iter().any(|entry| {
            entry
                .get("correlation_id")
                .and_then(serde_json::Value::as_str)
                == Some(self.correlation_id.as_str())
        }))
    }

    /// Close this exact uncertainty when fresh evidence makes it irrelevant.
    ///
    /// Returns `false` when the correlation is already terminal.
    pub fn supersede_if_uncertain(
        &self,
        state_root: &Path,
        global_dir: &Path,
        reason: &str,
    ) -> Result<bool, String> {
        supersede_uncertainty(state_root, global_dir, &self.correlation_id, reason)
    }
}

impl Default for DurableMutationIntent {
    fn default() -> Self {
        Self::new()
    }
}

impl MergeQueueMutationGuard {
    /// Validate authority, HOLD, and exact-PR uncertainty before a caller
    /// persists a correlation that will be handed to a later guard acquire.
    pub fn validate_in_mode(
        store: &ShipStateStore,
        global_dir: &Path,
        state: &ShipState,
    ) -> Result<(), String> {
        let state_root = store.path().parent().unwrap_or_else(|| store.path());
        let _control_lock = acquire_control_lock(state_root, true)?;
        let hold_path = state_root.join(HOLD_FILE);
        if hold_path.exists() {
            return Err(format!(
                "merge-queue mutations are centrally held by {}",
                hold_path.display()
            ));
        }
        if let Some(uncertain) = uncertain_mutations(state_root)?.into_iter().find(|entry| {
            entry.get("repo").and_then(serde_json::Value::as_str) == Some(state.repo.as_str())
                && entry.get("base").and_then(serde_json::Value::as_str)
                    == Some(state.base_branch.as_str())
                && entry.get("pr").and_then(serde_json::Value::as_u64) == Some(state.pr)
        }) {
            let correlation_id = uncertain["correlation_id"]
                .as_str()
                .unwrap_or("<correlation-id>");
            return Err(format!(
                "merge-queue mutation {correlation_id} for {}/{} PR #{} is uncertain",
                state.repo, state.base_branch, state.pr
            ));
        }
        let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
            .map_err(|error| format!("failed to load merge-queue mutation policy: {error}"))?;
        validate_machine_authority(&config, state_root).map(|_| ())
    }

    /// Stable audit identity for durable state that brackets this mutation.
    #[must_use]
    pub fn correlation_id(&self) -> &str {
        &self.correlation_id
    }

    /// Verify machine authority and the central hold, then serialize mutations
    /// for this repository/base pair.
    pub fn acquire(
        store: &ShipStateStore,
        cwd: &Path,
        state: &ShipState,
        action: &str,
    ) -> Result<Self, String> {
        let mode = RuntimeMode::Shipyard;
        let global_dir = crate::paths::RuntimePaths::current(mode).global_dir;
        Self::acquire_in_mode(store, cwd, mode, &global_dir, state, action)
    }

    /// Acquire mutation authority from the caller's trusted machine-global
    /// configuration. Repository and checkout-local policy is ignored.
    pub fn acquire_in_mode(
        store: &ShipStateStore,
        _cwd: &Path,
        _mode: RuntimeMode,
        global_dir: &Path,
        state: &ShipState,
        action: &str,
    ) -> Result<Self, String> {
        let state_root = store.path().parent().unwrap_or_else(|| store.path());
        let control_dir = state_root.join("merge_queue");
        fs::create_dir_all(control_dir.join("locks"))
            .map_err(|error| format!("failed to create merge-queue control directory: {error}"))?;
        let control_lock = acquire_control_lock(state_root, true)?;
        let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
            .map_err(|error| format!("failed to load merge-queue mutation policy: {error}"))?;
        Self::acquire_with_control_lock(store, &config, state, action, control_lock, None)
    }

    /// Acquire using a correlation ID already persisted by a crash-resumable
    /// caller, closing the gap between guard creation and caller state.
    pub fn acquire_in_mode_with_correlation(
        store: &ShipStateStore,
        _cwd: &Path,
        _mode: RuntimeMode,
        global_dir: &Path,
        state: &ShipState,
        action: &str,
        correlation_id: &str,
    ) -> Result<Self, String> {
        let state_root = store.path().parent().unwrap_or_else(|| store.path());
        let control_lock = acquire_control_lock(state_root, true)?;
        let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
            .map_err(|error| format!("failed to load merge-queue mutation policy: {error}"))?;
        Self::acquire_with_control_lock(
            store,
            &config,
            state,
            action,
            control_lock,
            Some(correlation_id),
        )
    }

    /// Generate the same collision-resistant audit identity used by guards.
    #[must_use]
    pub fn new_correlation_id() -> String {
        format!(
            "mq-{}-{}-{}",
            Utc::now().format("%Y%m%dT%H%M%S%.6fZ"),
            process::id(),
            CORRELATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Convert a preflight covering earlier remote branch work into the
    /// audited, PR-specific mutation guard without releasing serialization.
    pub fn acquire_after_preflight(
        preflight: MergeQueueMutationPreflight,
        store: &ShipStateStore,
        _cwd: &Path,
        _mode: RuntimeMode,
        state: &ShipState,
        action: &str,
    ) -> Result<Self, String> {
        let config = LoadedConfig::load_machine_global_from_dir(preflight.global_dir)
            .map_err(|error| format!("failed to load merge-queue mutation policy: {error}"))?;
        Self::acquire_with_control_lock(store, &config, state, action, preflight.control_lock, None)
    }

    fn acquire_with_control_lock(
        store: &ShipStateStore,
        config: &LoadedConfig,
        state: &ShipState,
        action: &str,
        control_lock: ControlLock,
        correlation_id: Option<&str>,
    ) -> Result<Self, String> {
        let state_root = store.path().parent().unwrap_or_else(|| store.path());
        let control_dir = state_root.join("merge_queue");
        fs::create_dir_all(control_dir.join("locks"))
            .map_err(|error| format!("failed to create merge-queue control directory: {error}"))?;
        let hold_path = state_root.join(HOLD_FILE);
        if hold_path.exists() {
            return Err(format!(
                "merge-queue mutations are centrally held by {}",
                hold_path.display()
            ));
        }
        if let Some(uncertain) = uncertain_mutations(state_root)?.into_iter().find(|entry| {
            entry.get("repo").and_then(serde_json::Value::as_str) == Some(state.repo.as_str())
                && entry.get("base").and_then(serde_json::Value::as_str)
                    == Some(state.base_branch.as_str())
                && entry.get("pr").and_then(serde_json::Value::as_u64) == Some(state.pr)
        }) {
            let correlation_id = uncertain["correlation_id"]
                .as_str()
                .unwrap_or("<correlation-id>");
            return Err(format!(
                "merge-queue mutation {correlation_id} for {}/{} PR #{} is uncertain; reconcile GitHub, then run `shipyard merge-queue resolve {correlation_id} --outcome accepted|rejected --reason <reason>`",
                state.repo, state.base_branch, state.pr
            ));
        }
        let machine_tag = validate_machine_authority(config, state_root)?;

        let lock_path = control_dir.join("locks").join(format!(
            "{}.lock",
            queue_key(&state.repo, &state.base_branch)
        ));
        let mut lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| format!("failed to open merge-queue mutation lock: {error}"))?;
        match lock.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if lock_is_contended(&error) => {
                return Err(format!(
                    "another Shipyard process owns merge-queue mutation authority for {}/{}",
                    state.repo, state.base_branch
                ));
            }
            Err(error) => {
                return Err(format!(
                    "failed to acquire merge-queue mutation authority: {error}"
                ));
            }
        }

        let correlation_id = correlation_id.map_or_else(Self::new_correlation_id, str::to_owned);
        lock.set_len(0)
            .and_then(|()| {
                writeln!(
                    lock,
                    "correlation_id={correlation_id}\npid={}\nmachine={}\nrepo={}\nbase={}\npr={}\nhead={}\naction={action}",
                    process::id(),
                    machine_tag.as_deref().unwrap_or("unconfigured"),
                    state.repo,
                    state.base_branch,
                    state.pr,
                    state.head_sha
                )
            })
            .and_then(|()| lock.sync_all())
            .map_err(|error| format!("failed to persist merge-queue lock owner: {error}"))?;

        let audit_path = state_root.join(AUDIT_FILE);
        append_audit(
            &audit_path,
            crate::log_retention::LogRetentionPolicy::from_config(config),
            &json!({
                "timestamp": Utc::now(),
                "correlation_id": correlation_id,
                "phase": "started",
                "action": action,
                "machine": machine_tag.as_deref().unwrap_or("unconfigured"),
                "pid": process::id(),
                "repo": state.repo,
                "base": state.base_branch,
                "pr": state.pr,
                "head": state.head_sha,
            }),
        )
        .map_err(|error| format!("failed to write merge-queue mutation audit: {error}"))?;

        Ok(Self {
            control_lock,
            lock,
            audit_path,
            correlation_id,
            action: action.to_owned(),
            finished: false,
            retention_policy: crate::log_retention::LogRetentionPolicy::from_config(config),
            repo: state.repo.clone(),
            base: state.base_branch.clone(),
            pr: state.pr,
            head: state.head_sha.clone(),
        })
    }

    /// Record the externally observed mutation result before releasing authority.
    pub fn finish(mut self, outcome: &str) -> Result<(), String> {
        append_audit(
            &self.audit_path,
            self.retention_policy,
            &json!({
                "timestamp": Utc::now(),
                "correlation_id": self.correlation_id,
                "phase": "finished",
                "action": self.action,
                "outcome": outcome,
                "repo": self.repo,
                "base": self.base,
                "pr": self.pr,
                "head": self.head,
            }),
        )
        .map_err(|error| format!("failed to write merge-queue mutation result: {error}"))?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for MergeQueueMutationGuard {
    fn drop(&mut self) {
        if !self.finished {
            let _ = append_audit(
                &self.audit_path,
                self.retention_policy,
                &json!({
                    "timestamp": Utc::now(),
                    "correlation_id": self.correlation_id,
                    "phase": "finished",
                    "action": self.action,
                    "outcome": "uncertain",
                    "repo": self.repo,
                    "base": self.base,
                    "pr": self.pr,
                    "head": self.head,
                }),
            );
        }
        let _ = self.lock.unlock();
        let _ = self.control_lock.unlock();
    }
}

fn validate_machine_authority(
    config: &LoadedConfig,
    state_root: &Path,
) -> Result<Option<String>, String> {
    let machine_tag = read_machine_tag(state_root);
    let authority = match config.get("merge_queue.mutation_machine") {
        None => {
            return Err(
                "merge_queue.mutation_machine is not configured in trusted machine-global config"
                    .to_owned(),
            );
        }
        Some(value) => value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "merge_queue.mutation_machine must be a non-empty string".to_owned())?,
    };
    crate::runner_provision::validate_machine_tag(authority)?;
    let Some(tag) = machine_tag.as_deref() else {
        return Err(format!(
            "merge-queue mutation authority is `{authority}`, but this machine has no Shipyard runner tag"
        ));
    };
    if tag != authority {
        return Err(format!(
            "merge-queue mutation authority is `{authority}`; this machine is `{tag}`"
        ));
    }
    Ok(machine_tag)
}

fn acquire_control_lock(state_root: &Path, nonblocking: bool) -> Result<ControlLock, String> {
    acquire_control_lock_with_boundary_signal(state_root, nonblocking, || {})
}

fn acquire_control_lock_with_boundary_signal(
    state_root: &Path,
    nonblocking: bool,
    at_lock_boundary: impl FnOnce(),
) -> Result<ControlLock, String> {
    let path = state_root.join(CONTROL_LOCK_FILE);
    let parent = path
        .parent()
        .ok_or_else(|| "merge-queue control lock path has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create merge-queue control directory: {error}"))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("failed to open merge-queue control lock: {error}"))?;
    at_lock_boundary();
    let result = if nonblocking {
        lock.try_lock_exclusive()
    } else {
        lock.lock_exclusive()
    };
    match result {
        Ok(()) => Ok(ControlLock::new(lock)),
        Err(error) if nonblocking && lock_is_contended(&error) => {
            Err("another Shipyard process is performing a merge-queue mutation".to_owned())
        }
        Err(error) => Err(format!(
            "failed to acquire merge-queue control lock: {error}"
        )),
    }
}

fn read_machine_tag(state_root: &Path) -> Option<String> {
    let tag = fs::read_to_string(state_root.join("machine-tag")).ok()?;
    let tag = tag.trim();
    (!tag.is_empty()).then(|| tag.to_owned())
}

fn queue_key(repo: &str, base: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(repo.as_bytes());
    digest.update([0]);
    digest.update(base.as_bytes());
    hex::encode(&digest.finalize()[..12])
}

fn append_audit(
    path: &Path,
    policy: crate::log_retention::LogRetentionPolicy,
    value: &serde_json::Value,
) -> io::Result<()> {
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.lock_exclusive()?;
    let unresolved = ordered_audit_paths(path)
        .map_err(io::Error::other)?
        .map_or_else(|| Ok(Vec::new()), |paths| read_uncertain_from_paths(&paths))
        .map_err(io::Error::other)?;
    if unresolved.is_empty() {
        crate::log_retention::rotate_if_oversize(path, policy)?;
    }
    let result = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut audit| {
            writeln!(audit, "{value}")?;
            audit.sync_all()?;
            crate::log_retention::sync_parent_directory(path)
        });
    let unlock_result = lock.unlock();
    result.and(unlock_result)
}

pub(crate) fn lock_is_contended(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }

    #[cfg(windows)]
    {
        error.raw_os_error() == Some(33)
    }

    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
mod tests;
