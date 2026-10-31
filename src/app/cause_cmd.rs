//! `shipyard cause`: name a failed job's cause from its log, and keep the
//! records in an append-only archive.
//!
//! `extract` is what a workflow runs in an `if: failure()` step over the
//! output it captured: `--notice` prints the record as a workflow command, so
//! the cause lands in the job's own log, where `diagnose` and the scorecard
//! read it back. It is also EXP-A3's entry point to C2 (`--format c2`), so the
//! corpus and Shipyard use one extractor.
//!
//! `archive` appends records to `<dir>/causes/YYYY-MM.jsonl` by the month of
//! `named_at`. A record whose id is already in the archive is skipped; no
//! line is ever rewritten. The directory passed to `archive` is the parent
//! containing `causes/` and `fixlinks.jsonl`, the same layout used by the
//! recorder, advice, and audit commands.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use fs2::FileExt;
use serde::Serialize;
use serde_json::Value;

mod advice;
mod audit;
mod record;
mod retention;

use super::CliFailure;
use super::cli::{CauseArchiveArgs, CauseCommand, CauseExtractArgs, CauseRecordArgs};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::diagnose::c2;
use crate::diagnose::cause::{self, CiCause, Place};
use crate::diagnose::{Job, Step};

/// The step a job assembled from the environment failed in.
const CAPTURED_STEP: &str = "captured output";

pub(super) fn cause_command<W: Write>(
    command: CauseCommand,
    json: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    match command {
        CauseCommand::Extract(args) => {
            if args.job.is_none() && args.format == "record" {
                eprintln!(
                    "cause extract: no --job, so the record names the job by its key \
                     (GITHUB_JOB); for a job with `name:` pass --job with that name, or \
                     the scorecard will not take the record's time"
                );
            }
            let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
            let text = extract(
                &args,
                &env,
                &Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            )?;
            writeln!(stdout, "{text}").map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        CauseCommand::Record(args) => {
            return Err(CliFailure::new(
                2,
                format!(
                    "cause record for {} needs the configured GitHub reader",
                    args.repo
                ),
            ));
        }
        CauseCommand::Advise(args) => return advice::command(&args, json, stdout),
        CauseCommand::Archive(args) => {
            let report = archive_inputs(&args)?;
            let written = if json {
                writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string(&report).unwrap_or_default()
                )
            } else {
                writeln!(
                    stdout,
                    "archive: {} appended, {} already archived, {} line(s) without a record",
                    report.appended, report.already_archived, report.without_record
                )
            };
            written.map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        CauseCommand::Prune(args) => return retention::command(&args, json, stdout),
        CauseCommand::Audit(args) => return audit::command(&args, json, stdout),
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Hold an archive lock for the complete writer operation, or share an
/// existing lock for a reader. Readers never create a lock or recover a
/// transaction: an archive without a lock is read without one when no
/// transaction is pending, while an interrupted transaction is refused until
/// a writer recovers it.
pub(crate) struct ArchiveLock {
    _file: Option<std::fs::File>,
}

fn validate_archive_root(dir: &Path) -> Result<(), String> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err("archive root must be a real directory".to_owned())
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
        _ => Ok(()),
    }
}

fn validate_archive_layout(dir: &Path) -> Result<(), String> {
    let causes = dir.join("causes");
    if let Ok(metadata) = fs::symlink_metadata(&causes) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "cause archive causes path must be a real directory: {}",
                causes.display()
            ));
        }
        for entry in fs::read_dir(&causes).map_err(|error| error.to_string())? {
            let path = entry.map_err(|error| error.to_string())?.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!(
                    "cause archive entry must be a regular file: {}",
                    path.display()
                ));
            }
        }
    }
    let fixlinks = dir.join("fixlinks.jsonl");
    if let Ok(metadata) = fs::symlink_metadata(&fixlinks)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(format!(
            "cause archive fixlinks must be a regular file: {}",
            fixlinks.display()
        ));
    }
    Ok(())
}

pub(super) fn lock_archive(dir: &Path, exclusive: bool) -> Result<ArchiveLock, String> {
    lock_archive_with_options(dir, exclusive, true)
}

/// Read an archive without creating a lock file. Used by dry-run commands so
/// previewing retention remains metadata-free.
pub(super) fn lock_archive_without_creation(dir: &Path) -> Result<ArchiveLock, String> {
    lock_archive_with_options(dir, false, false)
}

fn lock_archive_with_options(
    dir: &Path,
    exclusive: bool,
    create_mutable_read_lock: bool,
) -> Result<ArchiveLock, String> {
    validate_archive_root(dir)?;
    // Check before creating a lock so a frozen archive remains byte-for-byte
    // untouched when it has never been opened by this process.
    if exclusive {
        retention::ensure_not_frozen(dir)?;
    } else if retention::is_frozen(dir)? {
        validate_archive_layout(dir)?;
        if retention::has_pending_transaction(dir) {
            return Err(
                "archive has an interrupted prune; run cause prune --apply to recover before reading"
                    .to_owned(),
            );
        }
        return Ok(ArchiveLock { _file: None });
    }
    if exclusive {
        fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err("archive root must be a real directory".to_owned());
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.to_string()),
        _ => {}
    }
    let lock_path = dir.join(".lock");
    if let Ok(metadata) = fs::symlink_metadata(&lock_path)
        && !metadata.is_file()
    {
        return Err("archive lock must be a regular file".to_owned());
    }
    let mutable_archive = create_mutable_read_lock
        && retention::is_managed(dir)?
        && (fs::symlink_metadata(dir.join("causes")).is_ok()
            || fs::symlink_metadata(dir.join("fixlinks.jsonl")).is_ok());
    if mutable_archive {
        // Validate before creating a missing reader lock so malformed or
        // symlinked archives remain metadata-free and fail closed.
        validate_archive_layout(dir)?;
    }
    let lock = match OpenOptions::new()
        // Mutable archives join the writer lock domain even when no lock file
        // exists yet. Frozen archives return above and remain read-only.
        .create(exclusive || mutable_archive)
        .truncate(false)
        .read(true)
        .write(exclusive || mutable_archive)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        Err(error) if !exclusive && error.kind() == std::io::ErrorKind::NotFound => {
            validate_archive_layout(dir)?;
            if retention::has_pending_transaction(dir) {
                return Err(
                    "archive has an interrupted prune; run cause prune --apply to recover before reading"
                        .to_owned(),
                );
            }
            return Ok(ArchiveLock { _file: None });
        }
        Err(error) if !exclusive && error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(error.to_string());
        }
        Err(error) => return Err(error.to_string()),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let result = if exclusive {
            lock.try_lock_exclusive()
        } else {
            fs2::FileExt::try_lock_shared(&lock)
        };
        match result {
            Ok(()) => break,
            Err(error) if lock_contention(&error) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(error) => {
                return Err(format!(
                    "archive lock unavailable after bounded wait: {error}"
                ));
            }
        }
    }
    validate_archive_layout(dir)?;
    if exclusive {
        retention::recover_pending_transaction(dir)?;
    } else if retention::has_pending_transaction(dir) {
        return Err(
            "archive has an interrupted prune; run cause prune --apply to recover before reading"
                .to_owned(),
        );
    }
    Ok(ArchiveLock { _file: Some(lock) })
}

fn lock_contention(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        // Windows reports a byte-range lock held by another process as
        // ERROR_LOCK_VIOLATION (33), which fs2 surfaces as ErrorKind::Other.
        || error.raw_os_error() == Some(33)
}

/// `cause record`: one observer pass over the live GitHub reader.
pub(super) fn record_command<W: Write>(
    args: &CauseRecordArgs,
    config: &LoadedConfig,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let actions = GitHubActions::from_loaded_config(cwd, config);
    let gh = |gh_args: &[String]| {
        actions
            .run_gh_with_timeout(gh_args, std::time::Duration::from_mins(1))
            .map_err(|error| error.to_string())
    };
    let now = Utc::now();
    let query = record::RecordQuery {
        repo: args.repo.clone(),
        workflow: args.workflow.clone(),
        dir: args.dir.clone(),
        since: args.since.clone().unwrap_or_else(|| {
            (now - chrono::Duration::days(3))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        }),
        base: args.base.clone(),
        now: now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };
    let report = record::record(&gh, &query).map_err(|error| CliFailure::new(1, error))?;
    let written = if json {
        writeln!(
            stdout,
            "{}",
            serde_json::to_string(&report).unwrap_or_default()
        )
    } else {
        writeln!(
            stdout,
            "record: {} run(s) listed, {} read; {} cause(s) written ({} labelled by step, \
             {} inherited from the job they summarise), {} already recorded, {} cancelled \
             job(s) left to outages; {} fix-link(s) written, {} red(s) pending ({} under 24h, \
             {} 24-72h, {} over 72h); {} unreadable",
            report.runs_listed,
            report.runs_read,
            report.causes_written,
            report.labelled_by_step,
            report.inherited,
            report.already_recorded,
            report.cancelled_skipped,
            report.fixlinks_written,
            report.reds_pending,
            report.pending_under_24h,
            report.pending_24h_to_72h,
            report.pending_over_72h,
            report.unreadable.len()
        )
    };
    written.map_err(|error| CliFailure::new(1, error.to_string()))?;
    for reason in &report.unreadable {
        eprintln!("cause record: {reason}");
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// The job and where it ran: the `--job-json` payload when given, else a
/// failed job assembled from the flags and the runner's environment.
fn job_and_place(
    args: &CauseExtractArgs,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Job, Place), CliFailure> {
    let number = |flag: Option<u64>, key: &str| flag.or_else(|| env(key)?.parse().ok());
    let mut job = match &args.job_json {
        Some(path) => {
            let text = fs::read_to_string(path).map_err(|error| {
                CliFailure::new(2, format!("cannot read {}: {error}", path.display()))
            })?;
            serde_json::from_str::<Job>(&text).map_err(|error| {
                CliFailure::new(
                    2,
                    format!("{} is not a job payload: {error}", path.display()),
                )
            })?
        }
        // A job with no recorded steps reads to `diagnose` as a lost runner,
        // so the captured output stands in as the one failed step.
        None => Job {
            status: Some("completed".to_owned()),
            conclusion: Some("failure".to_owned()),
            runner_name: env("RUNNER_NAME"),
            steps: Some(vec![Step {
                name: CAPTURED_STEP.to_owned(),
                conclusion: Some("failure".to_owned()),
                ..Step::default()
            }]),
            ..Job::default()
        },
    };
    if let Some(name) = args.job.clone().or_else(|| env("GITHUB_JOB")) {
        job.name = name;
    }
    job.run_id = number(args.run_id, "GITHUB_RUN_ID").or(job.run_id);
    job.run_attempt = number(args.attempt, "GITHUB_RUN_ATTEMPT").or(job.run_attempt);
    job.head_sha = args
        .head_sha
        .clone()
        .or_else(|| pull_request_head(env))
        .or_else(|| env("GITHUB_SHA"))
        .or(job.head_sha);
    let place = Place {
        repo: args
            .repo
            .clone()
            .or_else(|| env("GITHUB_REPOSITORY"))
            .unwrap_or_default(),
        event: args.event.clone().or_else(|| env("GITHUB_EVENT_NAME")),
        pr: args.pr,
        ..Place::default()
    };
    Ok((job, place))
}

/// The pull request's head commit from the event payload: on a
/// `pull_request` run `GITHUB_SHA` is the test merge commit, while the jobs
/// API (and so the record's binding) names the head.
fn pull_request_head(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let event: Value =
        serde_json::from_str(&fs::read_to_string(env("GITHUB_EVENT_PATH")?).ok()?).ok()?;
    event
        .pointer("/pull_request/head/sha")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// What `extract` prints.
fn extract(
    args: &CauseExtractArgs,
    env: &dyn Fn(&str) -> Option<String>,
    now: &str,
) -> Result<String, CliFailure> {
    let text = fs::read_to_string(&args.log).map_err(|error| {
        CliFailure::new(2, format!("cannot read {}: {error}", args.log.display()))
    })?;
    match args.format.as_str() {
        "c2" => Ok(serde_json::to_string(&c2::extract(&text)).unwrap_or_default()),
        "record" => {
            let (job, place) = job_and_place(args, env)?;
            // A job that did not fail has no cause; its record is `null`.
            let record = cause::build(&job, Some(&text), &place, Some(now.to_owned()), false);
            Ok(match (record, args.notice) {
                (Some(record), true) => cause::notice_line(&record),
                (None, true) => String::new(),
                (record, false) => serde_json::to_string(&record).unwrap_or_default(),
            })
        }
        other => Err(CliFailure::new(
            2,
            format!("--format must be record or c2, not `{other}`"),
        )),
    }
}

/// What one archive call did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct ArchiveReport {
    /// Records appended.
    pub appended: usize,
    /// Records whose id was already archived (or repeated in the input).
    pub already_archived: usize,
    /// Input lines that held no record.
    pub without_record: usize,
}

/// A `ci.cause/v1` record from one JSONL line: the line itself, or a
/// scorecard case row's `cause`.
fn record_of(line: &str) -> Option<CiCause> {
    let value: Value = serde_json::from_str(line).ok()?;
    let candidate = if value.get("schema").and_then(Value::as_str) == Some(cause::SCHEMA) {
        value
    } else {
        value.get("cause").filter(|cause| !cause.is_null())?.clone()
    };
    let record: CiCause = serde_json::from_value(candidate).ok()?;
    (record.schema == cause::SCHEMA).then_some(record)
}

fn month_file(record: &CiCause) -> String {
    let month = record
        .named_at
        .as_deref()
        .and_then(|at| at.get(..7))
        .filter(|month| {
            month.len() == 7
                && month.as_bytes()[4] == b'-'
                && month
                    .bytes()
                    .enumerate()
                    .all(|(i, byte)| i == 4 || byte.is_ascii_digit())
        })
        .unwrap_or("undated");
    format!("{month}.jsonl")
}

fn causes_dir(dir: &Path) -> PathBuf {
    dir.join("causes")
}

fn legacy_root_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(error) => return Err(error.to_string()),
    };
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            && path
                .file_stem()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name == "undated"
                        || (name.len() == 7
                            && name.as_bytes()[4] == b'-'
                            && name
                                .bytes()
                                .enumerate()
                                .all(|(i, b)| i == 4 || b.is_ascii_digit()))
                })
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn require_archive_parent(dir: &Path) -> Result<(), String> {
    let legacy = legacy_root_files(dir)?;
    if !legacy.is_empty() {
        let files = legacy
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "legacy cause archive files found directly under {} ({files}); use the parent directory if these files are already in causes/; otherwise reconcile them into {}/causes without overwriting existing files before retrying",
            dir.display(),
            dir.display()
        ));
    }
    Ok(())
}

fn archived_ids(dir: &Path) -> Result<BTreeSet<String>, String> {
    let mut ids = BTreeSet::new();
    let entries = match fs::read_dir(causes_dir(dir)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
        Err(error) => return Err(error.to_string()),
    };
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        ids.extend(text.lines().filter_map(record_of).map(|record| record.id()));
    }
    Ok(ids)
}

/// Append `records` to the archive under `dir`, holding its lock.
pub(super) fn archive(dir: &Path, lines: &[String]) -> Result<ArchiveReport, String> {
    let _lock = lock_archive(dir, true)?;
    require_archive_parent(dir)?;
    retention::ensure_archive_marker(dir, false)?;
    fs::create_dir_all(causes_dir(dir)).map_err(|error| error.to_string())?;
    let mut seen = archived_ids(dir)?;
    let mut report = ArchiveReport::default();
    for line in lines.iter().filter(|line| !line.trim().is_empty()) {
        let Some(record) = record_of(line) else {
            report.without_record += 1;
            continue;
        };
        if !seen.insert(record.id()) {
            report.already_archived += 1;
            continue;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(causes_dir(dir).join(month_file(&record)))
            .map_err(|error| error.to_string())?;
        let mut bytes = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        report.appended += 1;
    }
    Ok(report)
}

fn archive_inputs(args: &CauseArchiveArgs) -> Result<ArchiveReport, CliFailure> {
    let mut lines = Vec::new();
    for path in &args.inputs {
        let text = fs::read_to_string(path).map_err(|error| {
            CliFailure::new(2, format!("cannot read {}: {error}", path.display()))
        })?;
        lines.extend(text.lines().map(str::to_owned));
    }
    archive(&args.dir, &lines)
        .map_err(|error| CliFailure::new(1, format!("archive {}: {error}", args.dir.display())))
}

#[cfg(test)]
mod tests;
