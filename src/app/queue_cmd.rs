use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::{Value, json};

use super::{CliFailure, cli::QueuePriority};
use crate::config::LoadedConfig;
use crate::evidence::EvidenceStore;
use crate::executor::dispatch::{ExecutorDispatcher, resolve_targets};
use crate::identity::RuntimeMode;
use crate::job::{Job, JobStatus, Priority, ValidationMode};
use crate::output::write_json_envelope;
use crate::queue::{Queue, QueueError};

impl From<QueueError> for CliFailure {
    fn from(error: QueueError) -> Self {
        Self::new(1, error.to_string())
    }
}

impl From<serde_json::Error> for CliFailure {
    fn from(error: serde_json::Error) -> Self {
        Self::new(1, error.to_string())
    }
}

pub(super) fn status_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut queue = open_queue(state_dir)?;
    let active_runs = queue.get_running()?;
    let active = active_runs.first();
    let pending = queue.pending_count()?;
    let recent = queue.get_recent(5)?;
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let targets = target_statuses(&config)?;
    let orphaned = collect_orphaned_ship_states(state_dir, &config);

    if json_mode {
        let mut data = BTreeMap::new();
        data.insert(
            "queue".to_owned(),
            json!({
                "pending": pending,
                "running": active_runs.len(),
                "completed_recent": recent.len(),
            }),
        );
        if let Some(active) = active.as_ref() {
            data.insert("active_run".to_owned(), active.to_json_value());
        }
        data.insert("active_runs".to_owned(), jobs_value(&active_runs)?);
        data.insert("targets".to_owned(), serde_json::to_value(targets)?);
        data.insert(
            "orphaned_ship_states".to_owned(),
            Value::Array(
                orphaned
                    .iter()
                    .map(|(repo, pr, report)| {
                        json!({
                            "repo": repo,
                            "pr": pr,
                            "stalled_minutes": report.stalled_minutes,
                            "evidence": report.evidence.as_str(),
                        })
                    })
                    .collect(),
            ),
        );
        write_json_envelope(stdout, "status", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        write_status_human(
            stdout,
            active_runs.len(),
            pending,
            &recent,
            &targets,
            &orphaned,
        )?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Classify every active ship-state against the queue for orphan reporting.
/// Best-effort and strictly read-only: only reads the ship-state store when it
/// already exists (so `status` never materializes state in a fresh directory),
/// and yields nothing on any read failure rather than failing `status`.
fn collect_orphaned_ship_states(
    state_dir: &Path,
    config: &LoadedConfig,
) -> Vec<(String, u64, crate::ship_liveness::OrphanReport)> {
    let ship_dir = state_dir.join("ship");
    if !ship_dir.is_dir() {
        return Vec::new();
    }
    let Ok(store) = crate::ship_state::ShipStateStore::new(ship_dir) else {
        return Vec::new();
    };
    let stale_after = crate::ship_liveness::orphan_stale_after(config);
    let now = chrono::Utc::now();
    crate::ship_liveness::with_liveness_context(state_dir, stale_after, |liveness| {
        crate::ship_liveness::collect_orphans(&store, liveness, now)
    })
}

pub(super) fn evidence_command<W: Write>(
    branch: Option<String>,
    cwd: &Path,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let branch = branch
        .or_else(|| current_git_branch(cwd))
        .unwrap_or_else(|| "main".to_owned());
    let store = EvidenceStore::new(state_dir.join("evidence"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut records = BTreeMap::new();
    if let Some(repository) = super::branch_cmd::detect_repo_from_remote(cwd, None) {
        records.extend(store.get_branch_scoped(
            &crate::evidence::repository_evidence_scope(&repository),
            &branch,
        ));
        for (target, record) in store.get_branch_scoped_prefix(
            &crate::evidence::repository_ship_evidence_scope_prefix(&repository),
            &branch,
        ) {
            if records
                .get(&target)
                .is_none_or(|existing| record.completed_at > existing.completed_at)
            {
                records.insert(target, record);
            }
        }
    }
    for (target, record) in
        store.get_branch_scoped(&crate::evidence::run_evidence_scope(cwd), &branch)
    {
        if records
            .get(&target)
            .is_none_or(|existing| record.completed_at > existing.completed_at)
        {
            records.insert(target, record);
        }
    }
    // Legacy branch-only records remain visible until every machine has
    // emitted scoped evidence at least once. They are display-only here and
    // never satisfy scoped reuse or exact-head gates.
    for (target, record) in store.get_branch(&branch) {
        records.entry(target).or_insert(record);
    }
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("branch".to_owned(), Value::String(branch.clone()));
        data.insert("evidence".to_owned(), serde_json::to_value(&records)?);
        write_json_envelope(stdout, "evidence", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if records.is_empty() {
        writeln!(stdout, "No evidence for {branch}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "Evidence for {branch}:")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for (target, record) in records {
            writeln!(
                stdout,
                "  {target}: {} {} {}",
                record.status, record.sha, record.completed_at
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn logs_command<W: Write>(
    job_id: &str,
    target: Option<String>,
    state_dir: &Path,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut queue = open_queue(state_dir)?;
    let Some(job) = queue.get(job_id)? else {
        let target = target.ok_or_else(|| {
            CliFailure::new(
                1,
                format!("Job {job_id} is retained; specify --target to read its log"),
            )
        })?;
        if !is_plain_component(job_id) || !is_plain_component(&target) {
            return Err(CliFailure::new(2, "target must be one path component"));
        }
        let job_dir = state_dir.join("logs").join(job_id);
        let job_kind = fs::symlink_metadata(&job_dir)
            .ok()
            .map(|value| value.file_type());
        if !job_kind.is_some_and(|kind| kind.is_dir() && !kind.is_symlink()) {
            return Err(CliFailure::new(1, format!("Job {job_id} not found")));
        }
        write_retained_target_logs(stdout, &job_dir, &target)?;
        return Ok(ExitCode::SUCCESS);
    };
    if let Some(target) = target {
        let result = job
            .results
            .get(&target)
            .ok_or_else(|| CliFailure::new(1, format!("No log for target {target}")))?;
        let log_path = result
            .log_path
            .as_ref()
            .ok_or_else(|| CliFailure::new(1, format!("No log for target {target}")))?;
        write_log(stdout, log_path)?;
        return Ok(ExitCode::SUCCESS);
    }

    for name in &job.target_names {
        if let Some(result) = job.results.get(name)
            && let Some(log_path) = result.log_path.as_ref()
        {
            writeln!(stdout, "\n--- {name} ---")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            write_log(stdout, log_path)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn is_plain_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && Path::new(value).file_name().and_then(|name| name.to_str()) == Some(value)
}

fn write_retained_target_logs<W: Write>(
    stdout: &mut W,
    job_dir: &Path,
    target: &str,
) -> Result<(), CliFailure> {
    let base_name = format!("{target}.log");
    let mut logical_logs = BTreeMap::new();
    for entry in fs::read_dir(job_dir).map_err(|error| CliFailure::new(1, error.to_string()))? {
        let entry = entry.map_err(|error| CliFailure::new(1, error.to_string()))?;
        let kind = entry
            .file_type()
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if !kind.is_file() || kind.is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some((order, root_name)) = retained_log_root(&name, &base_name) {
            logical_logs.insert(order, job_dir.join(root_name));
        }
    }
    if logical_logs.is_empty() {
        return Err(CliFailure::new(
            1,
            format!("No retained log for target {target}"),
        ));
    }
    for path in logical_logs.into_values() {
        write_log(stdout, &path.to_string_lossy())?;
    }
    Ok(())
}

fn retained_log_root(name: &str, base_name: &str) -> Option<([u32; 2], String)> {
    let without_gzip = name.strip_suffix(".gz").unwrap_or(name);
    let (root, _) = without_gzip
        .rsplit_once('.')
        .filter(|(_, suffix)| suffix.parse::<usize>().is_ok())
        .unwrap_or((without_gzip, ""));
    let mut suffix = root.strip_prefix(base_name)?;
    let mut order = [0, 0];
    while !suffix.is_empty() {
        let (slot, offset, rest) = if let Some(rest) = suffix.strip_prefix(".retry") {
            (0, 0, rest)
        } else {
            (1, 1, suffix.strip_prefix(".attempt-")?)
        };
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let index = rest[..digits].parse::<u32>().ok()?;
        if order[slot] != 0 || (slot == 0 && order[1] != 0) {
            return None;
        }
        order[slot] = index.checked_add(offset)?;
        suffix = &rest[digits..];
    }
    Some((order, root.to_owned()))
}

pub(super) fn cancel_command<W: Write>(
    job_id: &str,
    reason: Option<&str>,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut queue = open_queue(state_dir)?;
    let host = std::env::var("WHENCE_HOST")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_owned());
    let agent = std::env::var("WHENCE_AGENT").unwrap_or_else(|_| "unknown-agent".to_owned());
    let reason = reason.map_or_else(
        || {
            format!(
                "Manual cancellation via shipyard cancel (host={host}, agent={agent}, pid={})",
                std::process::id()
            )
        },
        str::to_owned,
    );
    let cancelled = queue
        .request_cancel(job_id, Some(reason))?
        .ok_or_else(|| CliFailure::new(1, format!("Job {job_id} not found")))?;
    if json_mode {
        write_job_envelope(stdout, "cancel", &cancelled)?;
    } else {
        writeln!(stdout, "Cancelled {job_id}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn bump_command<W: Write>(
    job_id: &str,
    priority: QueuePriority,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut queue = open_queue(state_dir)?;
    let job = queue
        .get(job_id)?
        .ok_or_else(|| CliFailure::new(1, format!("Job {job_id} not found")))?;
    if job.status != JobStatus::Pending {
        return Err(CliFailure::new(
            1,
            format!("Can only bump pending jobs (current: {:?})", job.status),
        ));
    }
    let updated = job.with_priority(priority.into());
    queue.update(&updated)?;
    if json_mode {
        write_job_envelope(stdout, "bump", &updated)?;
    } else {
        writeln!(stdout, "Bumped {job_id} to {}", priority.as_str())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn queue_command<W: Write>(
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut queue = open_queue(state_dir)?;
    let active_runs = queue.get_running()?;
    let active = active_runs.first().cloned();
    let pending = queue.get_pending()?;
    let recent = queue.get_recent(5)?;
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("active".to_owned(), queue_value(active)?);
        data.insert("active_runs".to_owned(), jobs_value(&active_runs)?);
        data.insert("pending".to_owned(), jobs_value(&pending)?);
        data.insert("recent".to_owned(), jobs_value(&recent)?);
        write_json_envelope(stdout, "queue", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        write_queue_human(stdout, &active_runs, &pending, &recent)?;
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(serde::Serialize)]
struct TargetStatusRow {
    backend: String,
    reachable: bool,
}

fn target_statuses(config: &LoadedConfig) -> Result<BTreeMap<String, TargetStatusRow>, CliFailure> {
    if config.data.get("targets").is_none() {
        return Ok(BTreeMap::new());
    }
    let targets = resolve_targets(config, ValidationMode::Full)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let dispatcher = ExecutorDispatcher::new(None);
    Ok(targets
        .into_iter()
        .map(|target| {
            (
                target.name.clone(),
                TargetStatusRow {
                    backend: target.backend_name.clone(),
                    reachable: dispatcher.probe(&target),
                },
            )
        })
        .collect())
}

fn write_status_human<W: Write>(
    stdout: &mut W,
    running: usize,
    pending: usize,
    recent: &[Job],
    targets: &BTreeMap<String, TargetStatusRow>,
    orphaned: &[(String, u64, crate::ship_liveness::OrphanReport)],
) -> Result<(), CliFailure> {
    writeln!(stdout, "Status").map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "  running: {running}")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "  pending: {pending}")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "  completed_recent: {}", recent.len())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if !targets.is_empty() {
        writeln!(stdout, "Targets").map_err(|error| CliFailure::new(1, error.to_string()))?;
        for (name, info) in targets {
            writeln!(
                stdout,
                "  {name}: {} reachable={}",
                info.backend, info.reachable
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    if !orphaned.is_empty() {
        writeln!(
            stdout,
            "Orphaned ship states (in flight, worker likely gone)"
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for (repo, pr, report) in orphaned {
            writeln!(
                stdout,
                "  {repo} PR #{pr}: {} ({}m stalled) — re-run `shipyard ship {pr}` or `ship-state discard {pr}`",
                report.evidence.cause(),
                report.stalled_minutes,
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}

fn write_queue_human<W: Write>(
    stdout: &mut W,
    active_runs: &[Job],
    pending: &[Job],
    recent: &[Job],
) -> Result<(), CliFailure> {
    writeln!(stdout, "Queue").map_err(|error| CliFailure::new(1, error.to_string()))?;
    if !active_runs.is_empty() {
        writeln!(stdout, "  Running ({})", active_runs.len())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for active in active_runs {
            writeln!(
                stdout,
                "    {} {} @ {} [{}]",
                active.id,
                active.branch,
                short_sha(&active.sha),
                priority_name(active.priority)
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    if pending.is_empty() {
        writeln!(stdout, "  No pending jobs")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "  Pending ({})", pending.len())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for job in pending {
            writeln!(
                stdout,
                "    {} {} @ {} [{}]",
                job.id,
                job.branch,
                short_sha(&job.sha),
                priority_name(job.priority)
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    if !recent.is_empty() {
        writeln!(stdout, "  Recent ({})", recent.len())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for job in recent {
            writeln!(
                stdout,
                "    {} {} @ {} {}",
                job.id,
                job.branch,
                short_sha(&job.sha),
                if job.passed() { "pass" } else { "fail" }
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}

fn open_queue(state_dir: &Path) -> Result<Queue, CliFailure> {
    Queue::new(state_dir).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn queue_value(job: Option<Job>) -> Result<Value, CliFailure> {
    job.map_or(Ok(Value::Null), |job| {
        serde_json::to_value(job.to_json_value())
            .map_err(|error| CliFailure::new(1, error.to_string()))
    })
}

fn jobs_value(jobs: &[Job]) -> Result<Value, CliFailure> {
    serde_json::to_value(jobs.iter().map(Job::to_json_value).collect::<Vec<_>>())
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn write_job_envelope<W: Write>(
    stdout: &mut W,
    command: &str,
    job: &Job,
) -> Result<(), CliFailure> {
    let mut data = BTreeMap::new();
    data.insert("job".to_owned(), job.to_json_value());
    write_json_envelope(stdout, command, data)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn write_log<W: Write>(stdout: &mut W, log_path: &str) -> Result<(), CliFailure> {
    let mut retained = Vec::new();
    for index in (1..=32).rev() {
        let rotated = PathBuf::from(format!("{log_path}.{index}"));
        let compressed = PathBuf::from(format!("{log_path}.{index}.gz"));
        if rotated.exists() {
            retained.push((rotated, false));
        } else if compressed.exists() {
            retained.push((compressed, true));
        }
    }
    let base = PathBuf::from(log_path);
    if base.exists() {
        retained.push((base, false));
    } else {
        let compressed = PathBuf::from(format!("{log_path}.gz"));
        if compressed.exists() {
            retained.push((compressed, true));
        }
    }
    if retained.is_empty() {
        return Err(CliFailure::new(
            1,
            format!("Log file not found: {log_path}"),
        ));
    }
    for (path, compressed) in retained {
        let text = if compressed {
            let file = fs::File::open(&path).map_err(|error| {
                CliFailure::new(1, format!("failed to read {}: {error}", path.display()))
            })?;
            let mut decoder = flate2::read::GzDecoder::new(file);
            let mut text = String::new();
            std::io::Read::read_to_string(&mut decoder, &mut text).map_err(|error| {
                CliFailure::new(1, format!("failed to read {}: {error}", path.display()))
            })?;
            text
        } else {
            fs::read_to_string(&path).map_err(|error| {
                CliFailure::new(1, format!("failed to read {}: {error}", path.display()))
            })?
        };
        write!(stdout, "{text}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn current_git_branch(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

fn priority_name(priority: Priority) -> &'static str {
    match priority {
        Priority::Low => "low",
        Priority::Normal => "normal",
        Priority::High => "high",
    }
}

impl QueuePriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }
}

impl From<QueuePriority> for Priority {
    fn from(value: QueuePriority) -> Self {
        match value {
            QueuePriority::Low => Self::Low,
            QueuePriority::Normal => Self::Normal,
            QueuePriority::High => Self::High,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{logs_command, write_log};
    use crate::log_retention::{TERMINAL_MANIFEST_FILE, TerminalLogManifest};
    use chrono::Utc;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    #[test]
    fn write_log_falls_back_to_retained_gzip() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("target.log");
        let output = std::fs::File::create(format!("{}.gz", path.display())).expect("gzip");
        let mut encoder = GzEncoder::new(output, Compression::fast());
        encoder.write_all(b"retained evidence\n").expect("write");
        encoder.finish().expect("finish");
        let mut stdout = Vec::new();
        write_log(&mut stdout, path.to_str().expect("path")).expect("read gzip");
        assert_eq!(stdout, b"retained evidence\n");
    }

    #[test]
    fn write_log_includes_rotated_segments_oldest_first() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("target.log");
        std::fs::write(format!("{}.2", path.display()), "oldest\n").expect("oldest");
        std::fs::write(format!("{}.1", path.display()), "older\n").expect("older");
        std::fs::write(&path, "active\n").expect("active");
        let mut stdout = Vec::new();
        write_log(&mut stdout, path.to_str().expect("path")).expect("read history");
        assert_eq!(stdout, b"oldest\nolder\nactive\n");
    }

    #[test]
    fn trimmed_terminal_job_log_is_readable_by_target() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join("queue.json"), r#"{"jobs":[]}"#).expect("queue");
        let job_dir = temp.path().join("logs/job");
        std::fs::create_dir_all(&job_dir).expect("job dir");
        std::fs::write(job_dir.join("macos.log"), "first attempt\n").expect("log");
        std::fs::write(job_dir.join("macos.log.attempt-1"), "first failover\n").expect("failover");
        std::fs::write(job_dir.join("macos.log.retry1"), "terminal retry\n").expect("retry");
        let nested = std::fs::File::create(job_dir.join("macos.log.retry1.attempt-2.gz"))
            .expect("nested gzip");
        let mut encoder = GzEncoder::new(nested, Compression::fast());
        encoder
            .write_all(b"terminal failover\n")
            .expect("nested log");
        encoder.finish().expect("finish nested gzip");
        let manifest = TerminalLogManifest {
            schema_version: 1,
            job_id: "job".to_owned(),
            terminal_at: Utc::now(),
            failed: false,
            reason: "passed".to_owned(),
        };
        std::fs::write(
            job_dir.join(TERMINAL_MANIFEST_FILE),
            serde_json::to_vec(&manifest).expect("manifest"),
        )
        .expect("manifest");
        let mut stdout = Vec::new();
        logs_command("job", Some("macos".to_owned()), temp.path(), &mut stdout)
            .expect("retained log");
        assert_eq!(
            stdout,
            b"first attempt\nfirst failover\nterminal retry\nterminal failover\n"
        );

        std::fs::remove_file(job_dir.join(TERMINAL_MANIFEST_FILE)).expect("remove manifest");
        let mut unclassified = Vec::new();
        logs_command(
            "job",
            Some("macos".to_owned()),
            temp.path(),
            &mut unclassified,
        )
        .expect("unclassified retained log");
        assert_eq!(unclassified, stdout);

        let error = logs_command(
            "../job",
            Some("macos".to_owned()),
            temp.path(),
            &mut Vec::new(),
        )
        .expect_err("job traversal rejected");
        assert_eq!(error.code, 2);
    }
}
