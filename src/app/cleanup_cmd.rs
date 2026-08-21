use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::Serialize;
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use super::CliFailure;
use crate::config::LoadedConfig;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::log_retention::{
    AUDIT_PIN_FILE, LogRetentionPolicy, gzip_source_will_retire, prepare_gzip_derivative,
    publish_gzip_derivative, read_terminal_manifest, retire_verified_gzip_source,
    sync_parent_directory, verify_gzip_for_retirement,
};
use crate::output::write_json_envelope;
use crate::queue::Queue;
use crate::ship_state::ShipStateStore;

const ACTIVE_SHIP_STATE_DAYS: i64 = 14;
const ARCHIVED_SHIP_STATE_DAYS: i64 = 30;
const RETIREMENT_QUARANTINE_PREFIX: &str = ".shipyard-retire-";

fn audit_pin_exists(job_dir: &Path) -> bool {
    !matches!(
        fs::symlink_metadata(job_dir.join(AUDIT_PIN_FILE)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CleanupCommandOptions {
    pub(super) mode: CleanupMode,
    pub(super) scope: CleanupScope,
    pub(super) output: CleanupOutput,
    pub(super) pin: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CleanupMode {
    DryRun,
    Apply,
}

impl CleanupMode {
    pub(super) fn from_flags(dry_run: bool, apply: bool) -> Self {
        if apply || !dry_run {
            Self::Apply
        } else {
            Self::DryRun
        }
    }

    fn is_dry_run(self) -> bool {
        self == Self::DryRun
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CleanupScope {
    RetentionOnly,
    IncludeShipState,
}

impl CleanupScope {
    pub(super) fn from_flag(enabled: bool) -> Self {
        if enabled {
            Self::IncludeShipState
        } else {
            Self::RetentionOnly
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CleanupOutput {
    Human,
    Json,
}

impl CleanupOutput {
    pub(super) fn from_json(enabled: bool) -> Self {
        if enabled { Self::Json } else { Self::Human }
    }
}

pub(super) fn cleanup_command<W: Write>(
    state_dir: &Path,
    mode: RuntimeMode,
    cwd: &Path,
    options: &CleanupCommandOptions,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if let Some(job_id) = options.pin.as_deref() {
        return pin_log_directory(state_dir, job_id, options.output, stdout);
    }
    let dry_run = options.mode.is_dry_run();
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let result = cleanup_retention(state_dir, dry_run, LogRetentionPolicy::from_config(&config))?;
    let ship_state_report = if options.scope == CleanupScope::IncludeShipState {
        Some(cleanup_ship_state(state_dir, mode, cwd, dry_run)?)
    } else {
        None
    };

    if options.output == CleanupOutput::Json {
        write_cleanup_json(stdout, &result, ship_state_report.as_ref())?;
    } else {
        write_cleanup_human(stdout, &result, ship_state_report.as_ref())?;
    }
    Ok(ExitCode::SUCCESS)
}

fn pin_log_directory<W: Write>(
    state_dir: &Path,
    job_id: &str,
    output: CleanupOutput,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if Path::new(job_id).file_name().and_then(|name| name.to_str()) != Some(job_id) {
        return Err(CliFailure::new(2, "cleanup --pin requires a plain job id"));
    }
    fs::create_dir_all(state_dir).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state_dir.join("cleanup.lock"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    lock.lock_exclusive()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let logs = state_dir.join("logs");
    reject_log_symlinks(&logs)?;
    let job = logs.join(job_id);
    let metadata = fs::symlink_metadata(&job).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "cannot pin missing log directory {}: {error}",
                job.display()
            ),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CliFailure::new(
            1,
            "refusing to pin a non-directory log path",
        ));
    }
    let marker = job.join(AUDIT_PIN_FILE);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(mut file) => {
            file.write_all(b"pinned by shipyard cleanup --pin\n")
                .and_then(|()| file.sync_all())
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(&marker).map_or(true, |metadata| {
                metadata.file_type().is_symlink() || !metadata.is_file()
            }) {
                return Err(CliFailure::new(1, "invalid audit pin marker"));
            }
        }
        Err(error) => return Err(CliFailure::new(1, error.to_string())),
    }
    sync_parent_directory(&marker).map_err(|error| CliFailure::new(1, error.to_string()))?;
    if output == CleanupOutput::Json {
        write_json_envelope(
            stdout,
            "cleanup",
            BTreeMap::from([
                ("action".to_owned(), json!("pin")),
                ("job_id".to_owned(), json!(job_id)),
                ("path".to_owned(), json!(marker.display().to_string())),
            ]),
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "Pinned: {}", marker.display())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct CleanupItem {
    path: String,
    kind: String,
    action: String,
    size_bytes: u64,
    reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ProtectedItem {
    path: String,
    size_bytes: u64,
    category: String,
    reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CleanupResult {
    items: Vec<CleanupItem>,
    protected_items: Vec<ProtectedItem>,
    total_bytes: u64,
    deleted_bytes: u64,
    protected_bytes: u64,
    pinned_bytes: u64,
    skipped_bytes: u64,
    log_bytes_before: u64,
    projected_log_bytes_after: u64,
    high_watermark_bytes: u64,
    low_watermark_bytes: u64,
    dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ShipStateCleanupReport {
    deleted_active: Vec<u64>,
    deleted_archived: Vec<String>,
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

fn cleanup_retention(
    state_dir: &Path,
    dry_run: bool,
    policy: LogRetentionPolicy,
) -> Result<CleanupResult, CliFailure> {
    let _cleanup_guard = if dry_run {
        None
    } else {
        fs::create_dir_all(state_dir).map_err(|error| CliFailure::new(1, error.to_string()))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state_dir.join("cleanup.lock"))
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        lock.lock_exclusive()
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        Some(lock)
    };
    let mut items = Vec::new();
    let mut protected_items = Vec::new();
    reject_log_symlinks(&state_dir.join("logs"))?;
    let recovered =
        recover_retirement_quarantines(state_dir, dry_run, &mut items, &mut protected_items)?;
    reject_log_symlinks(&state_dir.join("logs"))?;
    if recovered {
        let protected_bytes = protected_items.iter().map(|item| item.size_bytes).sum();
        let log_bytes =
            dir_size(&state_dir.join("logs")) + if dry_run { protected_bytes } else { 0 };
        return Ok(CleanupResult {
            total_bytes: items.iter().map(|item| item.size_bytes).sum(),
            deleted_bytes: 0,
            protected_bytes,
            pinned_bytes: protected_items
                .iter()
                .filter(|item| item.category == "audit_pin")
                .map(|item| item.size_bytes)
                .sum(),
            skipped_bytes: protected_bytes,
            log_bytes_before: log_bytes,
            projected_log_bytes_after: log_bytes,
            high_watermark_bytes: policy.high_watermark_bytes,
            low_watermark_bytes: policy.low_watermark_bytes,
            dry_run,
            items,
            protected_items,
        });
    }
    let staging_candidates = plan_cleanup_staging(state_dir, &mut items)?;
    let queue_jobs = if has_job_log_directories(state_dir)? {
        load_queue_job_retention(&Queue::queue_file_at(state_dir))?
    } else {
        BTreeMap::new()
    };
    let (log_bytes_before, projected_log_bytes_after) = scan_job_logs_with_lock(
        state_dir,
        &queue_jobs,
        policy,
        dry_run,
        false,
        &mut items,
        &mut protected_items,
    )?;
    scan_bundles(state_dir, dry_run, &mut items)?;
    scan_evidence(state_dir, dry_run, &mut items)?;
    if !dry_run {
        apply_cleanup_staging(&staging_candidates)?;
    }
    let total_bytes = items.iter().map(|item| item.size_bytes).sum();
    let deleted_bytes = items
        .iter()
        .filter(|item| item.action == "delete")
        .map(|item| item.size_bytes)
        .sum();
    let protected_bytes = protected_items.iter().map(|item| item.size_bytes).sum();
    let pinned_bytes = protected_items
        .iter()
        .filter(|item| item.category == "audit_pin")
        .map(|item| item.size_bytes)
        .sum();
    let skipped_bytes = protected_items
        .iter()
        .filter(|item| matches!(item.category.as_str(), "active" | "audit_pin"))
        .map(|item| item.size_bytes)
        .sum();
    Ok(CleanupResult {
        items,
        protected_items,
        total_bytes,
        deleted_bytes,
        protected_bytes,
        pinned_bytes,
        skipped_bytes,
        log_bytes_before,
        projected_log_bytes_after,
        high_watermark_bytes: policy.high_watermark_bytes,
        low_watermark_bytes: policy.low_watermark_bytes,
        dry_run,
    })
}

#[derive(Debug)]
struct StagingCandidate {
    path: PathBuf,
    identity: PathIdentity,
}

fn recover_retirement_quarantines(
    state_dir: &Path,
    dry_run: bool,
    items: &mut Vec<CleanupItem>,
    protected_items: &mut Vec<ProtectedItem>,
) -> Result<bool, CliFailure> {
    let staging = state_dir.join("cleanup-staging");
    let metadata = match fs::symlink_metadata(&staging) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(CliFailure::new(1, error.to_string())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CliFailure::new(1, "invalid cleanup staging directory"));
    }
    let mut recovered = false;
    for entry in fs::read_dir(&staging).map_err(|error| CliFailure::new(1, error.to_string()))? {
        let entry = entry.map_err(|error| CliFailure::new(1, error.to_string()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(CliFailure::new(1, "non-UTF-8 cleanup staging entry"));
        };
        if !name.starts_with(RETIREMENT_QUARANTINE_PREFIX) {
            continue;
        }
        let kind = entry
            .file_type()
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if !kind.is_dir() || kind.is_symlink() {
            return Err(CliFailure::new(1, "invalid retirement quarantine"));
        }
        let mut children = fs::read_dir(entry.path())
            .map_err(|error| CliFailure::new(1, error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if children.is_empty() {
            remove_empty_retirement_quarantine(&entry.path(), dry_run, items)?;
            continue;
        }
        if children.len() != 1 {
            return Err(CliFailure::new(
                1,
                "retirement quarantine must contain exactly one job directory",
            ));
        }
        let child = children.pop().expect("length checked");
        let child_kind = child
            .file_type()
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if !child_kind.is_dir() || child_kind.is_symlink() {
            return Err(CliFailure::new(1, "invalid quarantined log evidence"));
        }
        let child_identity = path_identity(&child.path())
            .ok_or_else(|| CliFailure::new(1, "quarantined evidence has no stable identity"))?;
        let job_name = child.file_name();
        let destination = state_dir.join("logs").join(&job_name);
        if destination.exists() {
            return Err(CliFailure::new(
                1,
                format!(
                    "cannot restore quarantined evidence because {} already exists",
                    destination.display()
                ),
            ));
        }
        let size = dir_size(&child.path());
        recovered = true;
        items.push(CleanupItem {
            path: destination.display().to_string(),
            kind: "log_recovery".to_owned(),
            action: "restore".to_owned(),
            size_bytes: size,
            reason: "restore interrupted log retirement before retention scan".to_owned(),
        });
        protected_items.push(ProtectedItem {
            path: destination.display().to_string(),
            size_bytes: size,
            category: "restart_recovery".to_owned(),
            reason: "restored evidence is protected until a separately previewed cleanup"
                .to_owned(),
        });
        if !dry_run {
            if path_identity(&child.path()) != Some(child_identity) {
                return Err(CliFailure::new(
                    1,
                    "quarantined evidence changed during restart recovery",
                ));
            }
            fs::create_dir_all(state_dir.join("logs"))
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            fs::rename(child.path(), &destination)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            sync_parent_directory(&child.path())
                .and_then(|()| sync_parent_directory(&destination))
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            fs::remove_dir(entry.path()).map_err(|error| CliFailure::new(1, error.to_string()))?;
            sync_parent_directory(&entry.path())
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(recovered)
}

fn remove_empty_retirement_quarantine(
    path: &Path,
    dry_run: bool,
    items: &mut Vec<CleanupItem>,
) -> Result<(), CliFailure> {
    items.push(CleanupItem {
        path: path.display().to_string(),
        kind: "log_recovery".to_owned(),
        action: "delete".to_owned(),
        size_bytes: 0,
        reason: "empty completed retirement quarantine".to_owned(),
    });
    if !dry_run {
        fs::remove_dir(path).map_err(|error| CliFailure::new(1, error.to_string()))?;
        sync_parent_directory(path).map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn plan_cleanup_staging(
    state_dir: &Path,
    items: &mut Vec<CleanupItem>,
) -> Result<Vec<StagingCandidate>, CliFailure> {
    let staging = state_dir.join("cleanup-staging");
    let metadata = match fs::symlink_metadata(&staging) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(CliFailure::new(1, error.to_string())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CliFailure::new(
            1,
            format!(
                "refusing cleanup because staging path {} is not a real directory",
                staging.display()
            ),
        ));
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&staging).map_err(|error| CliFailure::new(1, error.to_string()))? {
        let entry = entry.map_err(|error| CliFailure::new(1, error.to_string()))?;
        let kind = entry
            .file_type()
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if kind.is_dir()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(RETIREMENT_QUARANTINE_PREFIX))
        {
            continue;
        }
        if !kind.is_file() {
            return Err(CliFailure::new(
                1,
                format!(
                    "refusing cleanup because staging entry {} is not a regular file",
                    entry.path().display()
                ),
            ));
        }
        let size = entry.metadata().map_or(0, |value| value.len());
        items.push(CleanupItem {
            path: entry.path().display().to_string(),
            kind: "log_staging".to_owned(),
            action: "delete".to_owned(),
            size_bytes: size,
            reason: "abandoned gzip staging file".to_owned(),
        });
        let path = entry.path();
        let identity = path_identity(&path).ok_or_else(|| {
            CliFailure::new(
                1,
                format!(
                    "refusing cleanup because {} has no stable identity",
                    path.display()
                ),
            )
        })?;
        candidates.push(StagingCandidate { path, identity });
    }
    Ok(candidates)
}

fn apply_cleanup_staging(candidates: &[StagingCandidate]) -> Result<(), CliFailure> {
    for candidate in candidates {
        if path_identity(&candidate.path) != Some(candidate.identity.clone()) {
            return Err(CliFailure::new(
                1,
                format!(
                    "cleanup staging entry {} changed after planning",
                    candidate.path.display()
                ),
            ));
        }
        fs::remove_file(&candidate.path).map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn has_job_log_directories(state_dir: &Path) -> Result<bool, CliFailure> {
    match fs::read_dir(state_dir.join("logs")) {
        Ok(entries) => Ok(entries
            .flatten()
            .any(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(CliFailure::new(
            1,
            format!("failed to inspect log directory: {error}"),
        )),
    }
}

#[derive(Clone, Copy, Debug)]
struct QueueLogState {
    active: bool,
    failed: Option<bool>,
    terminal_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct LogCandidate {
    path: PathBuf,
    size: u64,
    age_hours: i64,
    terminal_at: Option<DateTime<Utc>>,
    manifest_disposition: Option<(bool, DateTime<Utc>)>,
    failed: Option<bool>,
    queue_disposition: Option<bool>,
    active: bool,
    audit_pinned: bool,
    reason: String,
    logs_root: PathBuf,
    logs_root_identity: PathIdentity,
    job_identity: PathIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PathIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial: u64,
    #[cfg(windows)]
    file_index: u64,
}

#[cfg(test)]
fn scan_job_logs(
    state_dir: &Path,
    queue_jobs: &BTreeMap<String, QueueLogState>,
    policy: LogRetentionPolicy,
    dry_run: bool,
    items: &mut Vec<CleanupItem>,
    protected_items: &mut Vec<ProtectedItem>,
) -> Result<(u64, u64), CliFailure> {
    scan_job_logs_with_lock(
        state_dir,
        queue_jobs,
        policy,
        dry_run,
        false,
        items,
        protected_items,
    )
}

fn scan_job_logs_with_lock(
    state_dir: &Path,
    queue_jobs: &BTreeMap<String, QueueLogState>,
    policy: LogRetentionPolicy,
    dry_run: bool,
    queue_lock_held: bool,
    items: &mut Vec<CleanupItem>,
    protected_items: &mut Vec<ProtectedItem>,
) -> Result<(u64, u64), CliFailure> {
    let logs_dir = state_dir.join("logs");
    reject_log_symlinks(&logs_dir)?;
    let candidates = collect_log_candidates(&logs_dir, queue_jobs);
    let log_bytes_before = candidates
        .iter()
        .map(|candidate| candidate.size)
        .sum::<u64>();
    let (deletions, projected) = select_log_deletions(&candidates, policy, log_bytes_before);
    let mut output = LogApplyContext {
        deletions: &deletions,
        policy,
        log_bytes_before,
        dry_run,
        queue_lock_held,
        items,
        protected_items,
    };
    for candidate in &candidates {
        apply_log_candidate(candidate, &mut output)?;
    }
    let retired_sources = output
        .items
        .iter()
        .filter(|item| item.kind == "log_source" && item.action == "delete")
        .map(|item| item.size_bytes)
        .sum::<u64>();
    let projected = if dry_run {
        projected.saturating_sub(retired_sources)
    } else {
        dir_size(&logs_dir)
    };
    Ok((log_bytes_before, projected))
}

fn collect_log_candidates(
    logs_dir: &Path,
    queue_jobs: &BTreeMap<String, QueueLogState>,
) -> Vec<LogCandidate> {
    let Ok(entries) = fs::read_dir(logs_dir) else {
        return Vec::new();
    };
    let Some(logs_root_identity) = path_identity(logs_dir) else {
        return Vec::new();
    };
    let now = Utc::now();
    let mut candidates = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let job_id = path.file_name()?.to_str()?;
            if !path.is_dir() {
                return None;
            }
            let queue_state = queue_jobs.get(job_id).copied();
            let manifest = read_terminal_manifest(&path);
            let manifest_disposition = manifest
                .as_ref()
                .map(|value| (value.failed, value.terminal_at));
            // A terminal manifest is the durable classification boundary.
            // Queue-only outcomes remain unclassified so a failed manifest
            // transition can never shorten evidence retention.
            let failed = manifest.as_ref().map(|value| value.failed);
            let terminal_at = manifest
                .as_ref()
                .map(|value| value.terminal_at)
                .or_else(|| queue_state.and_then(|value| value.terminal_at))
                .or_else(|| newest_mtime(&path));
            let reason = manifest.map_or_else(
                || "legacy/unclassified evidence (fail-safe failure retention)".to_owned(),
                |value| format!("terminal {}", value.reason),
            );
            let job_identity = path_identity(&path)?;
            Some(LogCandidate {
                size: dir_size(&path),
                age_hours: terminal_at.map_or(0, |time| (now - time).num_hours().max(0)),
                terminal_at,
                manifest_disposition,
                active: queue_state.is_some_and(|value| value.active),
                audit_pinned: audit_pin_exists(&path),
                path,
                failed,
                queue_disposition: queue_state.and_then(|value| value.failed),
                reason,
                logs_root: logs_dir.to_path_buf(),
                logs_root_identity: logs_root_identity.clone(),
                job_identity,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    candidates
}

fn select_log_deletions(
    candidates: &[LogCandidate],
    policy: LogRetentionPolicy,
    log_bytes_before: u64,
) -> (BTreeSet<PathBuf>, u64) {
    let mut deletions = BTreeSet::new();
    for candidate in candidates {
        let retention_hours = if candidate.failed.unwrap_or(true) {
            policy.failure_days * 24
        } else {
            policy.success_days * 24
        };
        if !candidate.active && !candidate.audit_pinned && candidate.age_hours >= retention_hours {
            deletions.insert(candidate.path.clone());
        }
    }

    let mut projected = log_bytes_before.saturating_sub(
        candidates
            .iter()
            .filter(|candidate| deletions.contains(&candidate.path))
            .map(|candidate| candidate.size)
            .sum::<u64>(),
    );
    let planned_retirement_bytes = candidates
        .iter()
        .filter(|candidate| {
            !candidate.active
                && !candidate.audit_pinned
                && !deletions.contains(&candidate.path)
                && candidate.age_hours >= policy.compress_after_hours
        })
        .flat_map(|candidate| closed_log_files(&candidate.path))
        .filter(|path| gzip_source_will_retire(path).unwrap_or(false))
        .map(|path| path.metadata().map_or(0, |metadata| metadata.len()))
        .sum::<u64>();
    let mut pressure_projected = projected.saturating_sub(planned_retirement_bytes);
    if pressure_projected > policy.high_watermark_bytes {
        let mut pressure_candidates = candidates
            .iter()
            .filter(|candidate| {
                !candidate.active
                    && !candidate.audit_pinned
                    && candidate.failed == Some(false)
                    && !deletions.contains(&candidate.path)
                    // Compact first, then make a pressure-deletion decision
                    // from the next scan's real on-disk gzip size. Estimating
                    // compression could delete evidence unnecessarily.
                    && closed_log_files(&candidate.path).is_empty()
            })
            .collect::<Vec<_>>();
        pressure_candidates.sort_by(|left, right| {
            left.terminal_at
                .cmp(&right.terminal_at)
                .then_with(|| left.path.cmp(&right.path))
        });
        for candidate in pressure_candidates {
            if pressure_projected <= policy.low_watermark_bytes {
                break;
            }
            deletions.insert(candidate.path.clone());
            projected = projected.saturating_sub(candidate.size);
            pressure_projected = pressure_projected.saturating_sub(candidate.size);
        }
    }
    (deletions, projected)
}

struct LogApplyContext<'a> {
    deletions: &'a BTreeSet<PathBuf>,
    policy: LogRetentionPolicy,
    log_bytes_before: u64,
    dry_run: bool,
    queue_lock_held: bool,
    items: &'a mut Vec<CleanupItem>,
    protected_items: &'a mut Vec<ProtectedItem>,
}

fn apply_log_candidate(
    candidate: &LogCandidate,
    output: &mut LogApplyContext<'_>,
) -> Result<(), CliFailure> {
    if output.deletions.contains(&candidate.path) {
        let retention_hours = if candidate.failed.unwrap_or(true) {
            output.policy.failure_days * 24
        } else {
            output.policy.success_days * 24
        };
        output.items.push(CleanupItem {
            path: candidate.path.display().to_string(),
            kind: "log".to_owned(),
            action: "delete".to_owned(),
            size_bytes: candidate.size,
            reason: if candidate.age_hours < retention_hours {
                format!(
                    "log watermark pressure ({} > {})",
                    output.log_bytes_before, output.policy.high_watermark_bytes
                )
            } else {
                format!(
                    "{}; retention expired after {} hours",
                    candidate.reason, candidate.age_hours
                )
            },
        });
        if !output.dry_run {
            retire_log_directory(candidate, output.queue_lock_held)?;
        }
        return Ok(());
    }

    let (protected_category, protected_reason) = if candidate.active {
        ("active", "active queue writer".to_owned())
    } else if candidate.audit_pinned {
        (
            "audit_pin",
            format!("explicit audit pin ({AUDIT_PIN_FILE})"),
        )
    } else if candidate.failed.unwrap_or(true) {
        (
            "failure_retention",
            format!(
                "failure/unclassified evidence retained for {} days",
                output.policy.failure_days
            ),
        )
    } else {
        (
            "success_retention",
            format!(
                "successful evidence retained for {} days",
                output.policy.success_days
            ),
        )
    };
    output.protected_items.push(ProtectedItem {
        path: candidate.path.display().to_string(),
        size_bytes: candidate.size,
        category: protected_category.to_owned(),
        reason: protected_reason,
    });

    if !candidate.active
        && !candidate.audit_pinned
        && candidate.age_hours >= output.policy.compress_after_hours
    {
        for log_path in closed_log_files(&candidate.path) {
            let size = log_path.metadata().map_or(0, |metadata| metadata.len());
            let retires_source = if output.dry_run {
                gzip_source_will_retire(&log_path).unwrap_or(false)
            } else {
                apply_closed_log(candidate, output.queue_lock_held, &log_path)?
            };
            output.items.push(CleanupItem {
                path: log_path.display().to_string(),
                kind: if retires_source { "log_source" } else { "log" }.to_owned(),
                action: if retires_source { "delete" } else { "compress" }.to_owned(),
                size_bytes: size,
                reason: if retires_source {
                    "verified durable gzip; retire source".to_owned()
                } else {
                    format!(
                        "terminal log older than {} hours",
                        output.policy.compress_after_hours
                    )
                },
            });
        }
    }
    if !output.dry_run
        && let Some(item) = output.protected_items.last_mut()
    {
        item.size_bytes = dir_size(&candidate.path);
    }
    Ok(())
}

fn reject_log_symlinks(logs_dir: &Path) -> Result<(), CliFailure> {
    match fs::symlink_metadata(logs_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(CliFailure::new(
                1,
                format!(
                    "refusing log cleanup because {} is a symbolic link",
                    logs_dir.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CliFailure::new(1, error.to_string())),
    }
    let mut stack = vec![logs_dir.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(CliFailure::new(1, error.to_string())),
        };
        for entry in entries.flatten() {
            let kind = entry
                .file_type()
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            if kind.is_symlink() {
                return Err(CliFailure::new(
                    1,
                    format!(
                        "refusing log cleanup because {} is a symbolic link",
                        entry.path().display()
                    ),
                ));
            }
            if kind.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

fn apply_closed_log(
    candidate: &LogCandidate,
    queue_lock_held: bool,
    log_path: &Path,
) -> Result<bool, CliFailure> {
    if let Ok(Some(verification)) = verify_gzip_for_retirement(log_path) {
        let retired = with_log_mutation_guard(candidate, queue_lock_held, || {
            retire_verified_gzip_source(log_path, &verification)
        })?;
        if retired {
            return Ok(true);
        }
    }

    let state_dir = candidate
        .logs_root
        .parent()
        .ok_or_else(|| CliFailure::new(1, "invalid log staging boundary"))?;
    let prepared = prepare_gzip_derivative(log_path, &state_dir.join("cleanup-staging"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let published = with_log_mutation_guard(candidate, queue_lock_held, || {
        publish_gzip_derivative(log_path, prepared).map(drop)
    });
    published.map(|()| false)
}

fn with_log_mutation_guard<T>(
    candidate: &LogCandidate,
    queue_lock_held: bool,
    mutation: impl FnOnce() -> std::io::Result<T>,
) -> Result<T, CliFailure> {
    if queue_lock_held {
        recheck_mutation_boundary(candidate)?;
        return mutation().map_err(|error| CliFailure::new(1, error.to_string()));
    }
    let Some(state_dir) = candidate.path.parent().and_then(Path::parent) else {
        return Err(CliFailure::new(1, "invalid log directory boundary"));
    };
    let queue = Queue::new(state_dir).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let _guard = queue
        .lock_for_log_cleanup()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    recheck_mutation_boundary(candidate)?;
    mutation().map_err(|error| CliFailure::new(1, error.to_string()))
}

fn retire_log_directory(candidate: &LogCandidate, queue_lock_held: bool) -> Result<(), CliFailure> {
    retire_log_directory_with_hook(candidate, queue_lock_held, |_| Ok(()))
}

fn retire_log_directory_with_hook(
    candidate: &LogCandidate,
    queue_lock_held: bool,
    after_rename: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), CliFailure> {
    with_log_mutation_guard(candidate, queue_lock_held, || {
        let state_dir = candidate
            .logs_root
            .parent()
            .ok_or_else(|| std::io::Error::other("invalid log staging boundary"))?;
        let staging_dir = state_dir.join("cleanup-staging");
        fs::create_dir_all(&staging_dir)?;
        sync_parent_directory(&staging_dir)?;
        let tombstone = tempfile::Builder::new()
            .prefix(RETIREMENT_QUARANTINE_PREFIX)
            .tempdir_in(&staging_dir)?;
        // Once evidence moves here, restart recovery—not Drop—owns it.
        let tombstone = tombstone.keep();
        sync_parent_directory(&tombstone)?;
        let job_name = candidate
            .path
            .file_name()
            .ok_or_else(|| std::io::Error::other("invalid log job directory"))?;
        let quarantined = tombstone.join(job_name);
        fs::rename(&candidate.path, &quarantined)?;
        sync_parent_directory(&candidate.path)?;
        sync_parent_directory(&quarantined)?;
        if let Err(error) = after_rename(&quarantined) {
            fs::rename(&quarantined, &candidate.path)?;
            sync_parent_directory(&quarantined)?;
            sync_parent_directory(&candidate.path)?;
            return Err(error);
        }

        match fs::symlink_metadata(quarantined.join(AUDIT_PIN_FILE)) {
            Ok(_) => {
                fs::rename(&quarantined, &candidate.path)?;
                sync_parent_directory(&quarantined)?;
                sync_parent_directory(&candidate.path)?;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!(
                        "log cleanup stopped because {} was audit-pinned during retirement",
                        candidate.path.display()
                    ),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                fs::rename(&quarantined, &candidate.path)?;
                sync_parent_directory(&quarantined)?;
                sync_parent_directory(&candidate.path)?;
                return Err(error);
            }
        }
        fs::remove_dir_all(&quarantined)?;
        fs::remove_dir(&tombstone)?;
        sync_parent_directory(&tombstone)
    })
}

fn scan_bundles(
    state_dir: &Path,
    dry_run: bool,
    items: &mut Vec<CleanupItem>,
) -> Result<(), CliFailure> {
    let bundles_dir = state_dir.join("bundles");
    let Ok(entries) = fs::read_dir(&bundles_dir) else {
        return Ok(());
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if !path.is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("bundle")
        {
            continue;
        }
        let size = path.metadata().map_or(0, |metadata| metadata.len());
        items.push(CleanupItem {
            path: path.display().to_string(),
            kind: "bundle".to_owned(),
            action: "delete".to_owned(),
            size_bytes: size,
            reason: "Orphaned git bundle".to_owned(),
        });
        if !dry_run {
            fs::remove_file(&path).map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}

fn scan_evidence(
    state_dir: &Path,
    dry_run: bool,
    items: &mut Vec<CleanupItem>,
) -> Result<(), CliFailure> {
    let evidence_dir = state_dir.join("evidence");
    let Ok(entries) = fs::read_dir(&evidence_dir) else {
        return Ok(());
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if !path.is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("json")
        {
            continue;
        }
        let size = path.metadata().map_or(0, |metadata| metadata.len());
        let reason = match fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        {
            Some(Value::Object(object)) if object.is_empty() => Some("Empty evidence file"),
            Some(_) => None,
            None => Some("Corrupt evidence file"),
        };
        let Some(reason) = reason else {
            continue;
        };
        items.push(CleanupItem {
            path: path.display().to_string(),
            kind: "evidence".to_owned(),
            action: "delete".to_owned(),
            size_bytes: size,
            reason: reason.to_owned(),
        });
        if !dry_run {
            fs::remove_file(&path).map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}

fn cleanup_ship_state(
    state_dir: &Path,
    mode: RuntimeMode,
    cwd: &Path,
    dry_run: bool,
) -> Result<ShipStateCleanupReport, CliFailure> {
    let store = ShipStateStore::new(state_dir.join("ship"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if dry_run {
        let deleted_archived = preview_archived_ship_state(&store);
        return Ok(ShipStateCleanupReport {
            total: deleted_archived.len(),
            deleted_active: Vec::new(),
            deleted_archived,
            note: Some("Active-file pruning is only computed during --apply.".to_owned()),
        });
    }

    let now = Utc::now();
    let active_cutoff = now - Duration::days(ACTIVE_SHIP_STATE_DAYS);
    let gh_client =
        GhClient::from_cwd(mode, cwd).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let closed_prs = gather_closed_prs(&store, cwd, &gh_client)?;
    let mut deleted_active = Vec::new();
    for state in store.list_active() {
        if closed_prs
            .iter()
            .any(|(repo, pr)| repo.eq_ignore_ascii_case(&state.repo) && *pr == state.pr)
            && state.updated_at <= active_cutoff
        {
            store
                .delete_scoped(&state.repo, state.pr)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            deleted_active.push(state.pr);
        }
    }

    let deleted_archived = prune_archived_ship_state(&store)?;
    Ok(ShipStateCleanupReport {
        total: deleted_active.len() + deleted_archived.len(),
        deleted_active,
        deleted_archived,
        note: None,
    })
}

fn preview_archived_ship_state(store: &ShipStateStore) -> Vec<String> {
    let cutoff = Utc::now() - Duration::days(ARCHIVED_SHIP_STATE_DAYS);
    store
        .list_archived()
        .into_iter()
        .filter(|path| file_mtime(path).is_some_and(|mtime| mtime <= cutoff))
        .filter_map(|path| path.file_name()?.to_str().map(ToOwned::to_owned))
        .collect()
}

fn prune_archived_ship_state(store: &ShipStateStore) -> Result<Vec<String>, CliFailure> {
    let cutoff = Utc::now() - Duration::days(ARCHIVED_SHIP_STATE_DAYS);
    let mut deleted = Vec::new();
    for path in store.list_archived() {
        if file_mtime(&path).is_none_or(|mtime| mtime > cutoff) {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            deleted.push(name.to_owned());
        }
        fs::remove_file(path).map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(deleted)
}

fn gather_closed_prs(
    store: &ShipStateStore,
    cwd: &Path,
    gh_client: &GhClient,
) -> Result<Vec<(String, u64)>, CliFailure> {
    gather_closed_prs_with(store, |repo, pr| pr_is_closed(repo, pr, cwd, gh_client))
}

fn gather_closed_prs_with(
    store: &ShipStateStore,
    mut is_closed: impl FnMut(&str, u64) -> Result<bool, CliFailure>,
) -> Result<Vec<(String, u64)>, CliFailure> {
    let mut closed = Vec::new();
    for state in store.list_active() {
        if is_closed(&state.repo, state.pr)? {
            closed.push((state.repo, state.pr));
        }
    }
    Ok(closed)
}

fn pr_is_closed(repo: &str, pr: u64, cwd: &Path, gh_client: &GhClient) -> Result<bool, CliFailure> {
    let output = gh_client
        .clone()
        .with_repo_override(repo)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .prepare_command(
            cwd,
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .args([
            "pr",
            "view",
            &pr.to_string(),
            "--repo",
            repo,
            "--json",
            "state",
        ])
        .output()
        .map_err(|error| CliFailure::new(1, format!("failed to run gh pr view: {error}")))?;
    if !output.status.success() {
        return Ok(false);
    }
    let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Ok(false);
    };
    Ok(matches!(
        value.get("state").and_then(Value::as_str),
        Some("MERGED" | "CLOSED")
    ))
}

fn write_cleanup_json<W: Write>(
    stdout: &mut W,
    result: &CleanupResult,
    ship_state_report: Option<&ShipStateCleanupReport>,
) -> Result<(), CliFailure> {
    let mut data = BTreeMap::new();
    data.insert(
        "items".to_owned(),
        serde_json::to_value(&result.items)
            .map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    data.insert("total_bytes".to_owned(), json!(result.total_bytes));
    data.insert("deleted_bytes".to_owned(), json!(result.deleted_bytes));
    data.insert("protected_bytes".to_owned(), json!(result.protected_bytes));
    data.insert("pinned_bytes".to_owned(), json!(result.pinned_bytes));
    data.insert("skipped_bytes".to_owned(), json!(result.skipped_bytes));
    data.insert(
        "protected_items".to_owned(),
        serde_json::to_value(&result.protected_items)
            .map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    data.insert(
        "log_bytes_before".to_owned(),
        json!(result.log_bytes_before),
    );
    data.insert(
        "projected_log_bytes_after".to_owned(),
        json!(result.projected_log_bytes_after),
    );
    data.insert(
        "high_watermark_bytes".to_owned(),
        json!(result.high_watermark_bytes),
    );
    data.insert(
        "low_watermark_bytes".to_owned(),
        json!(result.low_watermark_bytes),
    );
    data.insert("dry_run".to_owned(), json!(result.dry_run));
    data.insert("count".to_owned(), json!(result.items.len()));
    if let Some(report) = ship_state_report {
        data.insert(
            "ship_state".to_owned(),
            serde_json::to_value(report).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
    }
    write_json_envelope(stdout, "cleanup", data)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn write_cleanup_human<W: Write>(
    stdout: &mut W,
    result: &CleanupResult,
    ship_state_report: Option<&ShipStateCleanupReport>,
) -> Result<(), CliFailure> {
    if result.items.is_empty() && result.protected_items.is_empty() && ship_state_report.is_none() {
        writeln!(stdout, "Nothing to clean up.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    for item in &result.items {
        let action = if result.dry_run {
            format!("would {}", item.action)
        } else {
            match item.action.as_str() {
                "compress" => "compressed".to_owned(),
                "delete" => "deleted".to_owned(),
                "restore" => "restored".to_owned(),
                action => action.to_owned(),
            }
        };
        writeln!(
            stdout,
            "  {action}: {} ({} bytes; {})",
            item.path, item.size_bytes, item.reason
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    for item in &result.protected_items {
        writeln!(
            stdout,
            "  retained: {} ({} bytes; {})",
            item.path, item.size_bytes, item.reason
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    writeln!(
        stdout,
        "  bytes: deleted={}, protected={}, pinned={}, skipped={}",
        result.deleted_bytes, result.protected_bytes, result.pinned_bytes, result.skipped_bytes
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if let Some(report) = ship_state_report {
        let action = if result.dry_run {
            "would delete"
        } else {
            "deleted"
        };
        for pr in &report.deleted_active {
            writeln!(stdout, "  {action}: ship state for PR #{pr}")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        for name in &report.deleted_archived {
            writeln!(stdout, "  {action}: archived ship state {name}")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    if result.dry_run {
        writeln!(stdout, "\nRun with --apply to delete.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn load_queue_job_retention(
    queue_file: &Path,
) -> Result<BTreeMap<String, QueueLogState>, CliFailure> {
    let text = fs::read_to_string(queue_file).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "refusing log cleanup because queue state {} is unreadable: {error}",
                queue_file.display()
            ),
        )
    })?;
    let value = serde_json::from_str::<Value>(&text).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "refusing log cleanup because queue state {} is corrupt: {error}",
                queue_file.display()
            ),
        )
    })?;
    let jobs = value.get("jobs").and_then(Value::as_array).ok_or_else(|| {
        CliFailure::new(
            1,
            format!(
                "refusing log cleanup because queue state {} has no jobs array",
                queue_file.display()
            ),
        )
    })?;
    let mut parsed = BTreeMap::new();
    for job in jobs {
        let (id, state) = parse_queue_log_state(job, queue_file)?;
        if parsed.insert(id, state).is_some() {
            return Err(CliFailure::new(
                1,
                format!(
                    "refusing log cleanup because queue state {} contains duplicate job ids",
                    queue_file.display()
                ),
            ));
        }
    }
    Ok(parsed)
}

fn parse_queue_log_state(
    job: &Value,
    queue_file: &Path,
) -> Result<(String, QueueLogState), CliFailure> {
    let id = job
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            CliFailure::new(
                1,
                format!(
                    "refusing log cleanup because queue state {} contains a job without a valid id",
                    queue_file.display()
                ),
            )
        })?
        .to_owned();
    let status = job
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| matches!(*status, "pending" | "running" | "completed" | "cancelled"))
        .ok_or_else(|| {
            CliFailure::new(
                1,
                format!(
                    "refusing log cleanup because queue state {} contains an unknown or missing job status",
                    queue_file.display()
                ),
            )
        })?;
    let terminal_at = job
        .get("completed_at")
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));
    let expected_targets = job
        .get("targets")
        .and_then(Value::as_array)
        .and_then(|targets| {
            targets
                .iter()
                .map(Value::as_str)
                .collect::<Option<BTreeSet<_>>>()
                .filter(|names| !names.is_empty() && names.len() == targets.len())
        });
    let result_statuses = job
        .get("results")
        .and_then(Value::as_object)
        .and_then(|results| {
            let statuses = results
                .iter()
                .map(|(target, result)| {
                    (result.get("target")?.as_str()? == target).then_some(())?;
                    let status = result.get("status")?.as_str()?;
                    matches!(
                        status,
                        "pass" | "fail" | "error" | "unreachable" | "cancelled"
                    )
                    .then_some((target.as_str(), status))
                })
                .collect::<Option<Vec<_>>>()?;
            let result_targets = statuses
                .iter()
                .map(|(target, _)| *target)
                .collect::<BTreeSet<_>>();
            (expected_targets.as_ref() == Some(&result_targets)).then_some(statuses)
        });
    let completed_contract = terminal_at.is_some() && result_statuses.is_some();
    if (status == "completed" && !completed_contract)
        || (status == "cancelled" && terminal_at.is_none())
    {
        return Err(CliFailure::new(
            1,
            format!(
                "refusing log cleanup because queue state {} contains a malformed terminal job",
                queue_file.display()
            ),
        ));
    }
    let active = matches!(status, "pending" | "running");
    let failed = match status {
        "cancelled" if terminal_at.is_some() => Some(true),
        "completed" if completed_contract => result_statuses
            .as_ref()
            .map(|statuses| statuses.iter().any(|(_, value)| *value != "pass")),
        _ => None,
    };
    Ok((
        id,
        QueueLogState {
            active,
            failed,
            terminal_at,
        },
    ))
}

fn recheck_mutation_boundary(candidate: &LogCandidate) -> Result<(), CliFailure> {
    validate_candidate_containment(candidate)?;
    if audit_pin_exists(&candidate.path) {
        return Err(CliFailure::new(
            1,
            format!(
                "log cleanup stopped because {} was audit-pinned after planning",
                candidate.path.display()
            ),
        ));
    }
    let Some(state_dir) = candidate.path.parent().and_then(Path::parent) else {
        return Err(CliFailure::new(1, "invalid log directory boundary"));
    };
    let job_id = candidate
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CliFailure::new(1, "invalid log job id"))?;
    let jobs = load_queue_job_retention(&Queue::queue_file_at(state_dir))?;
    if let Some(planned) = candidate.manifest_disposition {
        let current = read_terminal_manifest(&candidate.path)
            .map(|manifest| (manifest.failed, manifest.terminal_at));
        if current != Some(planned) {
            return Err(CliFailure::new(
                1,
                format!(
                    "log cleanup stopped because job {job_id} retention manifest changed after planning"
                ),
            ));
        }
    }
    if let Some(current) = jobs.get(job_id) {
        if current.active
            || current.failed != candidate.queue_disposition
            || current.terminal_at != candidate.terminal_at
        {
            return Err(CliFailure::new(
                1,
                format!(
                    "log cleanup stopped because job {job_id} disposition changed after planning"
                ),
            ));
        }
    } else {
        let manifest = read_terminal_manifest(&candidate.path);
        let changed = manifest.as_ref().is_some_and(|manifest| {
            Some(manifest.failed) != candidate.failed
                || Some(manifest.terminal_at) != candidate.terminal_at
        });
        let classified_evidence_disappeared = candidate.failed.is_some() && manifest.is_none();
        if changed || classified_evidence_disappeared {
            return Err(CliFailure::new(
                1,
                format!(
                    "log cleanup stopped because job {job_id} retention manifest changed after planning"
                ),
            ));
        }
    }
    Ok(())
}

fn path_identity(path: &Path) -> Option<PathIdentity> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        return None;
    }
    #[cfg(windows)]
    let windows_info = {
        let handle = winapi_util::Handle::from_path_any(path).ok()?;
        winapi_util::file::information(&handle).ok()?
    };
    Some(PathIdentity {
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(windows)]
        volume_serial: windows_info.volume_serial_number(),
        #[cfg(windows)]
        file_index: windows_info.file_index(),
    })
}

fn validate_candidate_containment(candidate: &LogCandidate) -> Result<(), CliFailure> {
    reject_log_symlinks(&candidate.logs_root)?;
    if path_identity(&candidate.logs_root) != Some(candidate.logs_root_identity.clone())
        || path_identity(&candidate.path) != Some(candidate.job_identity.clone())
    {
        return Err(CliFailure::new(
            1,
            "log cleanup stopped because the planned filesystem identity changed",
        ));
    }
    let root = fs::canonicalize(&candidate.logs_root)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let job =
        fs::canonicalize(&candidate.path).map_err(|error| CliFailure::new(1, error.to_string()))?;
    if job.parent() != Some(root.as_path()) {
        return Err(CliFailure::new(
            1,
            "log cleanup stopped because the planned job escaped the log root",
        ));
    }
    Ok(())
}

fn newest_mtime(path: &Path) -> Option<DateTime<Utc>> {
    let mut newest = path.metadata().ok()?.modified().ok();
    let mut stack = vec![path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            }
            if let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified())
                && newest.is_none_or(|value| modified > value)
            {
                newest = Some(modified);
            }
        }
    }
    newest.map(DateTime::<Utc>::from)
}

fn closed_log_files(job_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(job_dir) else {
        return Vec::new();
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.contains(".log")
                            && !Path::new(name)
                                .extension()
                                .is_some_and(|extension| extension.eq_ignore_ascii_case("gz"))
                    })
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![PathBuf::from(path)];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                total += path.metadata().map_or(0, |metadata| metadata.len());
            }
        }
    }
    total
}

fn file_mtime(path: &Path) -> Option<DateTime<Utc>> {
    path.metadata()
        .ok()?
        .modified()
        .ok()
        .map(DateTime::<Utc>::from)
}

#[cfg(test)]
mod tests {
    use super::gather_closed_prs_with;
    use crate::ship_state::{ShipState, ShipStateStore};

    #[test]
    fn closed_pr_scan_uses_each_states_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("ship-state store");
        for (repo, head) in [
            ("Generous-Corp/pulp", "pulp-head"),
            ("Generous-Corp/forge", "forge-head"),
            ("Generous-Corp/vellum", "vellum-head"),
        ] {
            store
                .save(&ShipState::new(
                    42,
                    repo,
                    "feature/x",
                    "main",
                    head,
                    "policy",
                ))
                .expect("save state");
        }
        let mut observed = Vec::new();

        let closed = gather_closed_prs_with(&store, |repo, pr| {
            observed.push((repo.to_owned(), pr));
            Ok(repo.eq_ignore_ascii_case("Generous-Corp/forge"))
        })
        .expect("scan closed PRs");

        observed.sort();
        assert_eq!(
            observed,
            vec![
                ("Generous-Corp/forge".to_owned(), 42),
                ("Generous-Corp/pulp".to_owned(), 42),
                ("Generous-Corp/vellum".to_owned(), 42),
            ]
        );
        assert_eq!(closed, vec![("Generous-Corp/forge".to_owned(), 42)]);
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    use crate::log_retention::gzip_closed_log;
    use crate::log_retention::{AUDIT_PIN_FILE, TERMINAL_MANIFEST_FILE, TerminalLogManifest};

    fn write_terminal(dir: &Path, job_id: &str, failed: bool, age_hours: i64) {
        fs::create_dir_all(dir).expect("job dir");
        let state_dir = dir.parent().and_then(Path::parent).expect("state dir");
        if !state_dir.join("queue.json").exists() {
            fs::write(state_dir.join("queue.json"), r#"{"jobs":[]}"#).expect("queue");
        }
        fs::write(dir.join("target.log"), "0123456789abcdef").expect("log");
        let manifest = TerminalLogManifest {
            schema_version: 1,
            job_id: job_id.to_owned(),
            terminal_at: Utc::now() - Duration::hours(age_hours),
            failed,
            reason: if failed { "target_failure" } else { "passed" }.to_owned(),
        };
        fs::write(
            dir.join(TERMINAL_MANIFEST_FILE),
            serde_json::to_vec(&manifest).expect("manifest json"),
        )
        .expect("manifest");
    }

    #[test]
    fn watermark_reclaims_success_but_preserves_failure() {
        let temp = tempfile::tempdir().expect("temp");
        let logs = temp.path().join("logs");
        write_terminal(&logs.join("success"), "success", false, 2);
        write_terminal(&logs.join("failure"), "failure", true, 2);
        fs::rename(
            logs.join("success/target.log"),
            logs.join("success/target.log.gz"),
        )
        .expect("mark success compacted");
        let policy = LogRetentionPolicy {
            high_watermark_bytes: 200,
            low_watermark_bytes: 180,
            compress_after_hours: 24,
            ..LogRetentionPolicy::default()
        };
        let mut items = Vec::new();
        let mut protected = Vec::new();
        let (before, projected_bytes) = scan_job_logs(
            temp.path(),
            &BTreeMap::new(),
            policy,
            true,
            &mut items,
            &mut protected,
        )
        .expect("scan");
        assert!(before > policy.high_watermark_bytes);
        assert!(projected_bytes <= policy.low_watermark_bytes);
        assert!(
            items
                .iter()
                .any(|item| item.path.ends_with("success") && item.action == "delete")
        );
        assert!(
            protected
                .iter()
                .any(|item| item.path.ends_with("failure") && item.reason.contains("failure"))
        );
    }

    #[test]
    fn queue_success_without_manifest_remains_failure_retained() {
        let temp = tempfile::tempdir().expect("temp");
        let job = temp.path().join("logs/job");
        fs::create_dir_all(&job).expect("job dir");
        fs::write(job.join("target.log"), "diagnostic evidence").expect("log");
        let queue_jobs = BTreeMap::from([(
            "job".to_owned(),
            QueueLogState {
                active: false,
                failed: Some(false),
                terminal_at: Some(Utc::now() - Duration::days(2)),
            },
        )]);
        let policy = LogRetentionPolicy {
            success_days: 1,
            failure_days: 30,
            ..LogRetentionPolicy::default()
        };
        let mut items = Vec::new();
        let mut protected = Vec::new();
        scan_job_logs(
            temp.path(),
            &queue_jobs,
            policy,
            true,
            &mut items,
            &mut protected,
        )
        .expect("scan");
        assert!(!items.iter().any(|item| item.action == "delete"));
        assert!(protected.iter().any(|item| {
            item.path.ends_with("job") && item.reason.contains("failure/unclassified")
        }));
    }

    #[test]
    fn explicit_audit_pin_survives_expiry_and_pressure() {
        let temp = tempfile::tempdir().expect("temp");
        let job = temp.path().join("logs/audit");
        write_terminal(&job, "audit", false, 24 * 365);
        fs::write(job.join(AUDIT_PIN_FILE), "incident-42\n").expect("pin");
        let policy = LogRetentionPolicy {
            high_watermark_bytes: 16,
            low_watermark_bytes: 8,
            ..LogRetentionPolicy::default()
        };
        let mut items = Vec::new();
        let mut protected = Vec::new();
        scan_job_logs(
            temp.path(),
            &BTreeMap::new(),
            policy,
            true,
            &mut items,
            &mut protected,
        )
        .expect("scan");
        assert!(items.iter().all(|item| item.action != "delete"));
        assert!(
            protected
                .iter()
                .any(|item| item.reason.contains("audit pin"))
        );
    }

    #[test]
    fn supported_pin_operation_serializes_and_protects_evidence() {
        let temp = tempfile::tempdir().expect("temp");
        let job = temp.path().join("logs/success");
        write_terminal(&job, "success", false, 24 * 365);
        fs::write(temp.path().join("queue.json"), r#"{"jobs":[]}"#).expect("queue");
        let mut output = Vec::new();
        pin_log_directory(temp.path(), "success", CleanupOutput::Human, &mut output).expect("pin");
        assert!(String::from_utf8(output).expect("utf8").contains("Pinned:"));

        cleanup_retention(temp.path(), false, LogRetentionPolicy::default()).expect("cleanup");
        assert!(job.join(AUDIT_PIN_FILE).is_file());
        assert!(job.join(TERMINAL_MANIFEST_FILE).is_file());
    }

    #[test]
    fn terminal_log_compression_is_mutating_only_on_apply() {
        let temp = tempfile::tempdir().expect("temp");
        let job = temp.path().join("logs/success");
        write_terminal(&job, "success", false, 2);
        let policy = LogRetentionPolicy::default();
        let mut items = Vec::new();
        let mut protected = Vec::new();
        scan_job_logs(
            temp.path(),
            &BTreeMap::new(),
            policy,
            true,
            &mut items,
            &mut protected,
        )
        .expect("dry run");
        assert!(job.join("target.log").exists());
        assert!(items.iter().any(|item| item.action == "compress"));

        items.clear();
        protected.clear();
        scan_job_logs(
            temp.path(),
            &BTreeMap::new(),
            policy,
            false,
            &mut items,
            &mut protected,
        )
        .expect("apply");
        assert!(job.join("target.log").exists());
        assert!(job.join("target.log.gz").exists());
        assert_eq!(protected[0].size_bytes, dir_size(&job));
        items.clear();
        protected.clear();
        let source_bytes = job.join("target.log").metadata().expect("source").len();
        let (_, after_retirement_bytes) = scan_job_logs(
            temp.path(),
            &BTreeMap::new(),
            policy,
            true,
            &mut items,
            &mut protected,
        )
        .expect("second dry run");
        assert!(items.iter().any(|item| {
            item.kind == "log_source" && item.action == "delete" && item.size_bytes == source_bytes
        }));
        assert_eq!(after_retirement_bytes + source_bytes, dir_size(&job));

        items.clear();
        protected.clear();
        scan_job_logs(
            temp.path(),
            &BTreeMap::new(),
            policy,
            false,
            &mut items,
            &mut protected,
        )
        .expect("second apply");
        assert!(!job.join("target.log").exists());
    }

    #[test]
    fn planned_source_retirement_avoids_unnecessary_pressure_deletion() {
        let temp = tempfile::tempdir().expect("temp");
        let job = temp.path().join("logs/success");
        write_terminal(&job, "success", false, 2);
        gzip_closed_log(&job.join("target.log")).expect("first compression pass");
        let compacted_bytes =
            dir_size(&job) - job.join("target.log").metadata().expect("source").len();
        let policy = LogRetentionPolicy {
            high_watermark_bytes: compacted_bytes,
            low_watermark_bytes: compacted_bytes,
            ..LogRetentionPolicy::default()
        };
        let mut items = Vec::new();
        let mut protected = Vec::new();
        scan_job_logs(
            temp.path(),
            &BTreeMap::new(),
            policy,
            true,
            &mut items,
            &mut protected,
        )
        .expect("dry run");
        assert!(items.iter().any(|item| item.kind == "log_source"));
        assert!(
            !items
                .iter()
                .any(|item| item.path == job.display().to_string())
        );
    }

    #[test]
    fn corrupt_gzip_is_repaired_without_retiring_source() {
        let temp = tempfile::tempdir().expect("temp");
        let job = temp.path().join("logs/success");
        write_terminal(&job, "success", false, 2);
        gzip_closed_log(&job.join("target.log")).expect("first compression pass");
        fs::write(job.join("target.log.gz"), "truncated").expect("corrupt derivative");
        let mut items = Vec::new();
        let mut protected = Vec::new();
        scan_job_logs(
            temp.path(),
            &BTreeMap::new(),
            LogRetentionPolicy::default(),
            false,
            &mut items,
            &mut protected,
        )
        .expect("repair pass");
        assert!(job.join("target.log").exists());
        assert!(gzip_source_will_retire(&job.join("target.log")).expect("repaired derivative"));
        assert!(items.iter().any(|item| item.action == "compress"));
    }

    #[test]
    fn apply_sweeps_abandoned_gzip_staging_with_receipt() {
        let temp = tempfile::tempdir().expect("temp");
        let staging = temp.path().join("cleanup-staging");
        fs::create_dir(&staging).expect("staging");
        let abandoned = staging.join("abandoned.tmp");
        fs::write(&abandoned, "partial gzip").expect("abandoned file");
        let result =
            cleanup_retention(temp.path(), false, LogRetentionPolicy::default()).expect("cleanup");
        assert!(!abandoned.exists());
        assert!(result.items.iter().any(|item| {
            item.kind == "log_staging"
                && item.action == "delete"
                && item.reason == "abandoned gzip staging file"
        }));
    }

    #[test]
    fn dry_run_reports_staging_and_failed_validation_preserves_it() {
        let temp = tempfile::tempdir().expect("temp");
        let staging = temp.path().join("cleanup-staging");
        fs::create_dir(&staging).expect("staging");
        let abandoned = staging.join("abandoned.tmp");
        fs::write(&abandoned, "partial gzip").expect("abandoned file");

        let dry_run =
            cleanup_retention(temp.path(), true, LogRetentionPolicy::default()).expect("dry run");
        assert!(abandoned.exists());
        assert!(dry_run.items.iter().any(|item| {
            item.path == abandoned.display().to_string()
                && item.kind == "log_staging"
                && item.action == "delete"
        }));

        fs::create_dir_all(temp.path().join("logs/job")).expect("job logs");
        fs::write(temp.path().join("queue.json"), "{}").expect("invalid queue");
        cleanup_retention(temp.path(), false, LogRetentionPolicy::default())
            .expect_err("queue validation must fail");
        assert!(abandoned.exists());
    }

    #[test]
    fn pressure_uses_exact_terminal_time_before_path_order() {
        let now = Utc::now();
        let temp = tempfile::tempdir().expect("temp");
        let identity = path_identity(temp.path()).expect("identity");
        let candidate = |path: &str, terminal_at| LogCandidate {
            path: PathBuf::from(path),
            size: 1,
            age_hours: 0,
            terminal_at: Some(terminal_at),
            manifest_disposition: Some((false, terminal_at)),
            failed: Some(false),
            queue_disposition: Some(false),
            active: false,
            audit_pinned: false,
            reason: "passed".to_owned(),
            logs_root: PathBuf::new(),
            logs_root_identity: identity.clone(),
            job_identity: identity.clone(),
        };
        let newer = candidate("a-newer", now);
        let older = candidate("z-older", now - Duration::minutes(1));
        let policy = LogRetentionPolicy {
            high_watermark_bytes: 1,
            low_watermark_bytes: 1,
            ..LogRetentionPolicy::default()
        };
        let (deletions, _) = select_log_deletions(&[newer, older], policy, 2);
        assert_eq!(deletions, BTreeSet::from([PathBuf::from("z-older")]));
    }

    #[test]
    fn corrupt_queue_state_fails_closed() {
        let temp = tempfile::tempdir().expect("temp");
        fs::write(temp.path().join("queue.json"), "{}").expect("queue");
        let error = load_queue_job_retention(&temp.path().join("queue.json"))
            .expect_err("missing jobs must fail closed");
        assert!(error.message.contains("no jobs array"));

        fs::write(
            temp.path().join("queue.json"),
            r#"{"jobs":[{"status":"running"}]}"#,
        )
        .expect("invalid job");
        let error = load_queue_job_retention(&temp.path().join("queue.json"))
            .expect_err("missing id must fail closed");
        assert!(error.message.contains("valid id"));

        fs::write(
            temp.path().join("queue.json"),
            r#"{"jobs":[{"id":"live","status":"runing"}]}"#,
        )
        .expect("unknown status");
        let error = load_queue_job_retention(&temp.path().join("queue.json"))
            .expect_err("unknown state must fail the cleanup closed");
        assert!(error.message.contains("unknown or missing job status"));

        fs::write(
            temp.path().join("queue.json"),
            r#"{"jobs":[{"id":"live"}]}"#,
        )
        .expect("missing status");
        let error = load_queue_job_retention(&temp.path().join("queue.json"))
            .expect_err("missing state must fail the cleanup closed");
        assert!(error.message.contains("unknown or missing job status"));

        fs::write(
            temp.path().join("queue.json"),
            r#"{"jobs":[{"id":"live","status":"completed"}]}"#,
        )
        .expect("incomplete terminal");
        load_queue_job_retention(&temp.path().join("queue.json"))
            .expect_err("incomplete terminal must fail cleanup closed");

        fs::write(
            temp.path().join("queue.json"),
            r#"{"jobs":[{"id":"live","status":"completed","completed_at":"2026-01-01T00:00:00Z","targets":["macos","linux"],"results":{"macos":{"status":"pass"}}}]}"#,
        )
        .expect("missing target result");
        load_queue_job_retention(&temp.path().join("queue.json"))
            .expect_err("partial result must fail cleanup closed");

        fs::write(
            temp.path().join("queue.json"),
            r#"{"jobs":[{"id":"live","status":"completed","completed_at":"2026-01-01T00:00:00Z","targets":["macos"],"results":{"macos":{"target":"linux","status":"pass"}}}]}"#,
        )
        .expect("mismatched inner target");
        load_queue_job_retention(&temp.path().join("queue.json"))
            .expect_err("mismatched result must fail cleanup closed");

        fs::write(
            temp.path().join("queue.json"),
            r#"{"jobs":[{"id":"live","status":"running"},{"id":"live","status":"running"}]}"#,
        )
        .expect("duplicate ids");
        let error = load_queue_job_retention(&temp.path().join("queue.json"))
            .expect_err("duplicate ids must fail closed");
        assert!(error.message.contains("duplicate"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_log_content_is_rejected_before_traversal() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("temp");
        let external = tempfile::tempdir().expect("external");
        fs::create_dir_all(temp.path().join("logs")).expect("logs");
        fs::write(external.path().join("target.log"), "external evidence").expect("external log");
        symlink(external.path(), temp.path().join("logs/escape")).expect("symlink");
        let mut items = Vec::new();
        let mut protected = Vec::new();
        let error = scan_job_logs(
            temp.path(),
            &BTreeMap::new(),
            LogRetentionPolicy::default(),
            false,
            &mut items,
            &mut protected,
        )
        .expect_err("symlink must fail closed");
        assert!(error.message.contains("symbolic link"));
        assert_eq!(
            fs::read_to_string(external.path().join("target.log")).expect("external remains"),
            "external evidence"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_log_tree_root_is_rejected() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("temp");
        let external = tempfile::tempdir().expect("external");
        let external_job = external.path().join("job");
        fs::create_dir(&external_job).expect("external job");
        fs::write(external_job.join("target.log"), "external evidence").expect("external log");
        symlink(external.path(), temp.path().join("logs")).expect("root symlink");
        let mut items = Vec::new();
        let mut protected = Vec::new();
        let error = scan_job_logs(
            temp.path(),
            &BTreeMap::new(),
            LogRetentionPolicy::default(),
            false,
            &mut items,
            &mut protected,
        )
        .expect_err("root symlink must fail closed");
        assert!(error.message.contains("symbolic link"));
        assert!(external_job.join("target.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn mutation_boundary_rejects_replaced_log_root() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("temp");
        let job = temp.path().join("logs/success");
        write_terminal(&job, "success", false, 24 * 365);
        let candidates = collect_log_candidates(&temp.path().join("logs"), &BTreeMap::new());
        let candidate = candidates.first().expect("candidate");
        fs::rename(temp.path().join("logs"), temp.path().join("original-logs"))
            .expect("move original tree");
        let external = tempfile::tempdir().expect("external");
        let external_job = external.path().join("success");
        write_terminal(&external_job, "success", false, 24 * 365);
        symlink(external.path(), temp.path().join("logs")).expect("replacement root");
        let error = recheck_mutation_boundary(candidate).expect_err("replacement must block");
        assert!(error.message.contains("symbolic link") || error.message.contains("identity"));
        assert!(external_job.exists());
    }

    #[test]
    fn mutation_boundary_rechecks_audit_pin() {
        let temp = tempfile::tempdir().expect("temp");
        let job = temp.path().join("logs/success");
        write_terminal(&job, "success", false, 24 * 365);
        let candidates = collect_log_candidates(&temp.path().join("logs"), &BTreeMap::new());
        let candidate = candidates.first().expect("candidate");
        fs::write(job.join(AUDIT_PIN_FILE), "incident\n").expect("late pin");
        let mut deletions = BTreeSet::new();
        deletions.insert(job.clone());
        let mut items = Vec::new();
        let mut protected = Vec::new();
        let mut output = LogApplyContext {
            deletions: &deletions,
            policy: LogRetentionPolicy::default(),
            log_bytes_before: candidate.size,
            dry_run: false,
            queue_lock_held: false,
            items: &mut items,
            protected_items: &mut protected,
        };
        let error = apply_log_candidate(candidate, &mut output).expect_err("late pin blocks");
        assert!(error.message.contains("audit-pinned"));
        assert!(job.exists());
    }

    #[test]
    fn retirement_quarantine_catches_pin_created_at_final_boundary() {
        let temp = tempfile::tempdir().expect("temp");
        let job = temp.path().join("logs/success");
        write_terminal(&job, "success", false, 24 * 365);
        let candidates = collect_log_candidates(&temp.path().join("logs"), &BTreeMap::new());
        let candidate = candidates.first().expect("candidate");

        let error = retire_log_directory_with_hook(candidate, false, |quarantined| {
            fs::write(quarantined.join(AUDIT_PIN_FILE), "incident\n")
        })
        .expect_err("pin created at retirement boundary must block");
        assert!(error.message.contains("audit-pinned during retirement"));
        assert!(job.join(AUDIT_PIN_FILE).is_file());
        assert!(job.join(TERMINAL_MANIFEST_FILE).is_file());
    }

    #[test]
    fn restart_restores_pinned_retirement_quarantine_before_scanning() {
        let temp = tempfile::tempdir().expect("temp");
        let quarantine = temp
            .path()
            .join("cleanup-staging/.shipyard-retire-interrupted");
        let quarantined_job = quarantine.join("success");
        write_terminal(&quarantined_job, "success", false, 24 * 365);
        fs::write(quarantined_job.join(AUDIT_PIN_FILE), "incident\n").expect("pin");
        fs::write(temp.path().join("queue.json"), r#"{"jobs":[]}"#).expect("queue");

        let preview = cleanup_retention(temp.path(), true, LogRetentionPolicy::default())
            .expect("dry-run recovery receipt");
        assert!(quarantined_job.exists());
        assert!(preview.items.iter().any(|item| {
            item.kind == "log_recovery"
                && item.action == "restore"
                && Path::new(&item.path).ends_with(Path::new("logs").join("success"))
        }));

        let applied = cleanup_retention(temp.path(), false, LogRetentionPolicy::default())
            .expect("apply recovery");
        let restored = temp.path().join("logs/success");
        assert!(restored.join(AUDIT_PIN_FILE).is_file());
        assert!(restored.join(TERMINAL_MANIFEST_FILE).is_file());
        assert!(!quarantine.exists());
        assert!(
            applied
                .protected_items
                .iter()
                .any(|item| item.category == "restart_recovery")
        );
    }

    #[test]
    fn mutation_boundary_rechecks_failure_disposition() {
        let temp = tempfile::tempdir().expect("temp");
        let job = temp.path().join("logs/success");
        write_terminal(&job, "success", false, 24 * 365);
        let candidates = collect_log_candidates(&temp.path().join("logs"), &BTreeMap::new());
        let candidate = candidates.first().expect("candidate");
        fs::write(
            temp.path().join("queue.json"),
            r#"{"jobs":[{"id":"success","status":"completed","completed_at":"2026-01-01T00:00:00Z","targets":["macos"],"results":{"macos":{"target":"macos","status":"error"}}}]}"#,
        )
        .expect("reclassified queue");
        let error = recheck_mutation_boundary(candidate).expect_err("failure change blocks");
        assert!(error.message.contains("disposition changed"));
        assert!(job.exists());
    }

    #[test]
    fn mutation_boundary_rejects_disappearing_manifest() {
        let temp = tempfile::tempdir().expect("temp");
        let job = temp.path().join("logs/success");
        write_terminal(&job, "success", false, 24 * 365);
        let candidates = collect_log_candidates(&temp.path().join("logs"), &BTreeMap::new());
        let candidate = candidates.first().expect("candidate");
        fs::remove_file(job.join(TERMINAL_MANIFEST_FILE)).expect("remove manifest");
        let error = recheck_mutation_boundary(candidate).expect_err("missing manifest blocks");
        assert!(error.message.contains("manifest changed"));
        assert!(job.exists());
    }
}
