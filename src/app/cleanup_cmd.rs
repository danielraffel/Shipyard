use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use super::CliFailure;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::queue::Queue;
use crate::ship_state::ShipStateStore;

const ACTIVE_SHIP_STATE_DAYS: i64 = 14;
const ARCHIVED_SHIP_STATE_DAYS: i64 = 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CleanupCommandOptions {
    pub(super) mode: CleanupMode,
    pub(super) scope: CleanupScope,
    pub(super) output: CleanupOutput,
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
    options: CleanupCommandOptions,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let dry_run = options.mode.is_dry_run();
    let result = cleanup_retention(state_dir, dry_run)?;
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct CleanupItem {
    path: String,
    kind: String,
    size_bytes: u64,
    reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CleanupResult {
    items: Vec<CleanupItem>,
    total_bytes: u64,
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

fn cleanup_retention(state_dir: &Path, dry_run: bool) -> Result<CleanupResult, CliFailure> {
    let mut items = Vec::new();
    let active_ids = load_active_job_ids(&Queue::queue_file_at(state_dir));
    scan_orphaned_logs(state_dir, &active_ids, dry_run, &mut items)?;
    scan_bundles(state_dir, dry_run, &mut items)?;
    scan_evidence(state_dir, dry_run, &mut items)?;
    let total_bytes = items.iter().map(|item| item.size_bytes).sum();
    Ok(CleanupResult {
        items,
        total_bytes,
        dry_run,
    })
}

fn scan_orphaned_logs(
    state_dir: &Path,
    active_ids: &[String],
    dry_run: bool,
    items: &mut Vec<CleanupItem>,
) -> Result<(), CliFailure> {
    let logs_dir = state_dir.join("logs");
    let Ok(entries) = fs::read_dir(&logs_dir) else {
        return Ok(());
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if !path.is_dir() {
            continue;
        }
        let Some(job_id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if active_ids.iter().any(|active| active == job_id) {
            continue;
        }
        let size = dir_size(&path);
        items.push(CleanupItem {
            path: path.display().to_string(),
            kind: "log".to_owned(),
            size_bytes: size,
            reason: format!("Job {job_id} not in queue"),
        });
        if !dry_run {
            fs::remove_dir_all(&path).map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
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
    let repository = super::branch_cmd::detect_repo_from_remote(cwd, None);
    let mut closed = Vec::new();
    for state in store.list_active() {
        if repository
            .as_ref()
            .is_none_or(|repository| !repository.eq_ignore_ascii_case(&state.repo))
        {
            continue;
        }
        if pr_is_closed(state.pr, cwd, gh_client)? {
            closed.push((state.repo, state.pr));
        }
    }
    Ok(closed)
}

fn pr_is_closed(pr: u64, cwd: &Path, gh_client: &GhClient) -> Result<bool, CliFailure> {
    let output = gh_client
        .prepare_command(
            cwd,
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .args(["pr", "view", &pr.to_string(), "--json", "state"])
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
    if result.items.is_empty() && ship_state_report.is_none() {
        writeln!(stdout, "Nothing to clean up.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    let action = if result.dry_run {
        "would delete"
    } else {
        "deleted"
    };
    for item in &result.items {
        writeln!(
            stdout,
            "  {action}: {} ({} bytes)",
            item.path, item.size_bytes
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    if let Some(report) = ship_state_report {
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

fn load_active_job_ids(queue_file: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(queue_file) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    value
        .get("jobs")
        .and_then(Value::as_array)
        .map(|jobs| {
            jobs.iter()
                .filter_map(|job| job.get("id").and_then(Value::as_str).map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
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
