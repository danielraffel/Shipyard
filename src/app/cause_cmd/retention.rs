//! Bounded, fail-closed retention for the private cause archive.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::CliFailure;
use super::super::cli::CausePruneArgs;
use crate::diagnose::cause::{self, CiCause};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct RedKey {
    repo: String,
    run_id: u64,
    attempt: u64,
}

#[derive(Clone, Debug)]
struct Row {
    path: PathBuf,
    line: String,
    at: DateTime<Utc>,
    key: Option<RedKey>,
    id: Option<String>,
    supersedes: Option<String>,
    line_number: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    schema: &'static str,
    dir: String,
    retention_days: i64,
    max_bytes: u64,
    cutoff: String,
    files_scanned: usize,
    cause_rows: usize,
    fixlink_rows: usize,
    rows_pruned: usize,
    bytes_before: u64,
    bytes_after: u64,
    dry_run: bool,
    unresolved_fixlinks: usize,
    unkeyed_causes: usize,
    size_cap_overrode_age: bool,
    unresolved: Vec<UnresolvedRow>,
}

#[derive(Debug, Serialize)]
struct UnresolvedRow {
    file: String,
    line: usize,
    reason: &'static str,
}

const ARCHIVE_MARKER: &str = ".shipyard-cause-archive.json";
const MARKER_SCHEMA: &str = "shipyard.cause-archive/v1";
const MAX_RETENTION_DAYS: i64 = 36_500;
const MAX_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SCAN_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SCAN_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SCAN_LINES: usize = 2_000_000;
const MAX_SCAN_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_SCAN_FILES: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ArchiveMarker {
    schema: String,
    frozen: bool,
}

mod transaction;

pub(super) use transaction::{
    apply_transaction, has_pending_transaction, recover_pending_transaction,
};

fn marker_path(dir: &Path) -> PathBuf {
    dir.join(ARCHIVE_MARKER)
}

fn read_marker(dir: &Path) -> Result<Option<ArchiveMarker>, String> {
    let path = marker_path(dir);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_file() || metadata.len() > 4096 {
        return Err(format!("invalid archive marker {}", path.display()));
    }
    let marker: ArchiveMarker =
        serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("invalid archive marker: {error}"))?;
    if marker.schema != MARKER_SCHEMA {
        return Err("archive marker has an unknown schema".to_owned());
    }
    Ok(Some(marker))
}

pub(super) fn ensure_not_frozen(dir: &Path) -> Result<(), String> {
    if read_marker(dir)?.is_some_and(|marker| marker.frozen) {
        return Err(
            "refusing to modify frozen cause archive; create a mutable copy first".to_owned(),
        );
    }
    Ok(())
}

pub(super) fn is_frozen(dir: &Path) -> Result<bool, String> {
    Ok(read_marker(dir)?.is_some_and(|marker| marker.frozen))
}

/// Ensure a managed, mutable archive marker exists. Prune requires the marker
/// so an arbitrary evaluation fixture cannot be mutated by `--apply`.
pub(super) fn ensure_archive_marker(dir: &Path, require_existing: bool) -> Result<(), String> {
    match read_marker(dir)? {
        Some(marker) if marker.frozen => Err(format!(
            "refusing to modify frozen cause archive {}; create a mutable copy first",
            dir.display()
        )),
        Some(_) => Ok(()),
        None if require_existing => Err(format!(
            "refusing to apply retention to unmarked archive {}; explicitly enroll only a mutable operational copy using the documented archive marker",
            dir.display()
        )),
        // Continue legacy append-only collection without enrolling a historical
        // archive for deletion. Explicit prune --initialize enrolls it later.
        None if fs::symlink_metadata(dir.join("causes")).is_ok()
            || fs::symlink_metadata(dir.join("fixlinks.jsonl")).is_ok() =>
        {
            Ok(())
        }
        None => initialize_archive_marker(dir),
    }
}

/// Explicitly enroll an existing archive for mutable retention. This is a
/// separate operator action so a historical/evaluation corpus is never made
/// mutable as a side effect of recording a new row.
pub(super) fn initialize_archive_marker(dir: &Path) -> Result<(), String> {
    match read_marker(dir)? {
        Some(marker) if marker.frozen => Err("refusing to enroll a frozen archive".to_owned()),
        Some(_) => Ok(()),
        None => {
            let path = marker_path(dir);
            let temp = dir.join(format!(".{ARCHIVE_MARKER}.tmp-{}", std::process::id()));
            let marker = ArchiveMarker {
                schema: MARKER_SCHEMA.to_owned(),
                frozen: false,
            };
            let bytes = serde_json::to_vec(&marker).map_err(|error| error.to_string())?;
            let result = (|| {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp)
                    .map_err(|error| error.to_string())?;
                file.write_all(&bytes).map_err(|error| error.to_string())?;
                file.write_all(b"\n").map_err(|error| error.to_string())?;
                file.sync_all().map_err(|error| error.to_string())?;
                fs::rename(&temp, &path).map_err(|error| error.to_string())?;
                #[cfg(not(windows))]
                File::open(dir)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|error| format!("{}: {error}", dir.display()))?;
                Ok(())
            })();
            if result.is_err() {
                let _ = fs::remove_file(&temp);
            }
            result
        }
    }
}

fn parse_time(value: &str, label: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| format!("{label} is not RFC3339: {error}"))
}

fn cause_row(line: &str) -> Result<CiCause, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|error| format!("invalid JSON: {error}"))?;
    let candidate = if value.get("schema").and_then(Value::as_str) == Some(cause::SCHEMA) {
        value
    } else {
        value
            .get("cause")
            .filter(|value| !value.is_null())
            .cloned()
            .ok_or_else(|| "row is not a ci.cause/v1 record".to_owned())?
    };
    let record: CiCause = serde_json::from_value(candidate)
        .map_err(|error| format!("row is not a ci.cause/v1 record: {error}"))?;
    (record.schema == cause::SCHEMA)
        .then_some(record)
        .ok_or_else(|| "row has the wrong cause schema".to_owned())
}

fn red_key(value: &Value) -> Result<RedKey, String> {
    Ok(RedKey {
        repo: value["repo"]
            .as_str()
            .ok_or_else(|| "fix-link lacks repo".to_owned())?
            .to_owned(),
        run_id: value["red_run_id"]
            .as_u64()
            .filter(|value| *value != 0)
            .ok_or_else(|| "fix-link lacks red_run_id".to_owned())?,
        attempt: match value.get("red_attempt") {
            None | Some(Value::Null) => 1,
            Some(value) => value
                .as_u64()
                .filter(|value| *value != 0)
                .ok_or_else(|| "fix-link red_attempt must be a non-zero integer".to_owned())?,
        },
    })
}

struct ScanBudget {
    bytes: u64,
    lines: usize,
    files: usize,
}

fn scan_file<F>(path: &Path, budget: &mut ScanBudget, mut visit: F) -> Result<(), String>
where
    F: FnMut(usize, &str) -> Result<(), String>,
{
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular archive file", path.display()));
    }
    if metadata.len() > MAX_SCAN_FILE_BYTES {
        return Err(format!(
            "{} exceeds the {} byte scan limit",
            path.display(),
            MAX_SCAN_FILE_BYTES
        ));
    }
    budget.files = budget
        .files
        .checked_add(1)
        .ok_or_else(|| "archive file count overflow".to_owned())?;
    if budget.files > MAX_SCAN_FILES {
        return Err(format!(
            "archive exceeds the {MAX_SCAN_FILES} file scan limit"
        ));
    }
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut line_no = 0;
    let mut file_bytes = 0u64;
    loop {
        line.clear();
        let read_limit = (MAX_SCAN_LINE_BYTES as u64)
            .min(MAX_SCAN_TOTAL_BYTES.saturating_sub(budget.bytes))
            .min(MAX_SCAN_FILE_BYTES.saturating_sub(file_bytes))
            + 1;
        let count = Read::by_ref(&mut reader)
            .take(read_limit)
            .read_line(&mut line)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        file_bytes += count as u64;
        if file_bytes > MAX_SCAN_FILE_BYTES {
            return Err(format!(
                "{} exceeds the per-file scan limit",
                path.display()
            ));
        }
        if count > MAX_SCAN_LINE_BYTES {
            return Err(format!(
                "{}:{} exceeds the {} byte line scan limit",
                path.display(),
                line_no + 1,
                MAX_SCAN_LINE_BYTES
            ));
        }
        budget.bytes = budget
            .bytes
            .checked_add(count as u64)
            .ok_or_else(|| "archive byte count overflow".to_owned())?;
        if budget.bytes > MAX_SCAN_TOTAL_BYTES {
            return Err(format!(
                "archive exceeds the {MAX_SCAN_TOTAL_BYTES} byte scan limit"
            ));
        }
        budget.lines = budget
            .lines
            .checked_add(1)
            .ok_or_else(|| "archive line count overflow".to_owned())?;
        if budget.lines > MAX_SCAN_LINES {
            return Err(format!(
                "archive exceeds the {MAX_SCAN_LINES} line scan limit"
            ));
        }
        line_no += 1;
        let line = line.strip_suffix('\n').unwrap_or(&line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        visit(line_no, line)?;
    }
    Ok(())
}

fn archive_files(dir: &Path) -> Result<(Vec<PathBuf>, Option<PathBuf>), String> {
    let causes_dir = dir.join("causes");
    let mut causes = Vec::new();
    let causes_meta = match fs::symlink_metadata(&causes_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "{} is not a regular archive directory",
                    causes_dir.display()
                ));
            }
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    if causes_meta.is_some() {
        if let Ok(entries) = fs::read_dir(&causes_dir) {
            for (index, entry) in entries.enumerate() {
                if index >= MAX_SCAN_FILES {
                    return Err(format!(
                        "archive exceeds the {MAX_SCAN_FILES} directory entry limit"
                    ));
                }
                let path = entry.map_err(|error| error.to_string())?.path();
                let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
                if metadata.file_type().is_symlink() {
                    return Err(format!("refusing symlink archive entry {}", path.display()));
                }
                if !metadata.is_file() {
                    return Err(format!("{} is not a regular archive entry", path.display()));
                }
                if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                    causes.push(path);
                }
            }
        } else {
            return Err(format!(
                "cannot read causes archive {}",
                causes_dir.display()
            ));
        }
    }
    causes.sort();
    let fix_path = dir.join("fixlinks.jsonl");
    let fix = match fs::symlink_metadata(&fix_path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(format!(
                    "{} is not a regular archive file",
                    fix_path.display()
                ));
            }
            Some(fix_path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    Ok((causes, fix))
}

fn read_rows(dir: &Path) -> Result<(Vec<Row>, Vec<Row>, usize), String> {
    let mut causes = Vec::new();
    let mut fixlinks = Vec::new();
    let mut budget = ScanBudget {
        bytes: 0,
        lines: 0,
        files: 0,
    };
    let (cause_paths, fix_path) = archive_files(dir)?;
    for path in cause_paths {
        scan_file(&path, &mut budget, |line_no, raw| {
            if raw.trim().is_empty() {
                return Ok(());
            }
            let record = cause_row(raw)
                .map_err(|error| format!("{}:{}: {error}", path.display(), line_no))?;
            let named_at = record
                .named_at
                .as_deref()
                .ok_or_else(|| format!("{}:{}: cause has no named_at", path.display(), line_no))?;
            let key = match (record.run_id, record.attempt) {
                (Some(run_id), Some(attempt)) if run_id != 0 && attempt != 0 => Some(RedKey {
                    repo: record.repo.clone(),
                    run_id,
                    attempt,
                }),
                _ => None,
            };
            causes.push(Row {
                path: path.clone(),
                line: format!("{raw}\n"),
                at: parse_time(named_at, "named_at")?,
                key,
                id: Some(record.id()),
                supersedes: record.supersedes.clone(),
                line_number: line_no,
            });
            Ok(())
        })?;
    }
    if let Some(fix_path) = fix_path {
        scan_file(&fix_path, &mut budget, |line_no, raw| {
            if raw.trim().is_empty() {
                return Ok(());
            }
            let value: Value = serde_json::from_str(raw).map_err(|error| {
                format!("{}:{}: invalid JSON: {error}", fix_path.display(), line_no)
            })?;
            if value["schema"] != cause::SCHEMA || value["source"] != "fix-link" {
                return Err(format!(
                    "{}:{}: row is not a ci.cause/v1 fix-link",
                    fix_path.display(),
                    line_no
                ));
            }
            let red_time = value["red_time"].as_str().ok_or_else(|| {
                format!(
                    "{}:{}: fix-link lacks red_time",
                    fix_path.display(),
                    line_no
                )
            })?;
            fixlinks.push(Row {
                path: fix_path.clone(),
                line: format!("{raw}\n"),
                at: parse_time(red_time, "red_time")?,
                key: Some(
                    red_key(&value)
                        .map_err(|error| format!("{}:{line_no}: {error}", fix_path.display()))?,
                ),
                id: None,
                supersedes: None,
                line_number: line_no,
            });
            Ok(())
        })?;
    }
    Ok((causes, fixlinks, budget.files))
}

fn retained_bytes(causes: &[Row], keep_causes: &[bool], links: &[Row], keep_links: &[bool]) -> u64 {
    causes
        .iter()
        .zip(keep_causes)
        .filter(|(_, keep)| **keep)
        .map(|(row, _)| row.line.len() as u64)
        .chain(
            links
                .iter()
                .zip(keep_links)
                .filter(|(_, keep)| **keep)
                .map(|(row, _)| row.line.len() as u64),
        )
        .sum()
}

// Retention operates on whole red-attempt groups. A retained fix must never
// outlive its supporting cause merely because their timestamps differ.
#[derive(Default)]
struct EvidenceGroup {
    causes: Vec<usize>,
    links: Vec<usize>,
}

type RetentionPlan = (Vec<bool>, Vec<bool>, usize, usize, bool);

fn plan_rows(
    causes: &[Row],
    links: &[Row],
    cutoff: DateTime<Utc>,
    max_bytes: u64,
) -> Result<RetentionPlan, String> {
    let mut groups = BTreeMap::<RedKey, EvidenceGroup>::new();
    let mut keep_causes = vec![false; causes.len()];
    let mut keep_links = vec![false; links.len()];
    let mut unkeyed_causes = 0;
    let mut unresolved_fixlinks = 0;
    for (index, row) in causes.iter().enumerate() {
        if let Some(key) = &row.key {
            groups.entry(key.clone()).or_default().causes.push(index);
        } else {
            // There is insufficient identity to prove which relationships
            // would be lost. Keep it and make the unresolved evidence visible.
            keep_causes[index] = true;
            unkeyed_causes += 1;
        }
    }
    for (index, row) in links.iter().enumerate() {
        if let Some(group) = row.key.as_ref().and_then(|key| groups.get_mut(key)) {
            group.links.push(index);
        } else {
            keep_links[index] = true;
            unresolved_fixlinks += 1;
        }
    }
    let superseded: BTreeSet<&str> = causes
        .iter()
        .filter_map(|row| row.supersedes.as_deref())
        .collect();
    let pinned_keys: BTreeSet<&RedKey> = causes
        .iter()
        .filter(|row| {
            row.supersedes.is_some() || row.id.as_deref().is_some_and(|id| superseded.contains(id))
        })
        .filter_map(|row| row.key.as_ref())
        .collect();
    let mut eligible = Vec::new();
    for (key, group) in &groups {
        let newest = group
            .causes
            .iter()
            .map(|index| causes[*index].at)
            .chain(group.links.iter().map(|index| links[*index].at))
            .max()
            .expect("groups contain at least one cause");
        let pinned = pinned_keys.contains(key);
        if newest >= cutoff || pinned {
            for index in &group.causes {
                keep_causes[*index] = true;
            }
            for index in &group.links {
                keep_links[*index] = true;
            }
            let bytes = group
                .causes
                .iter()
                .map(|index| causes[*index].line.len() as u64)
                .chain(
                    group
                        .links
                        .iter()
                        .map(|index| links[*index].line.len() as u64),
                )
                .sum::<u64>();
            if !pinned {
                eligible.push((newest, key, bytes));
            }
        }
    }
    // Stable tie-breaking by red identity keeps previews independent of file
    // enumeration order. Sort once rather than repeatedly rescanning the archive.
    eligible.sort();
    let mut bytes = retained_bytes(causes, &keep_causes, links, &keep_links);
    let mut size_cap_overrode_age = false;
    for (_, key, group_bytes) in eligible {
        if bytes <= max_bytes {
            break;
        }
        size_cap_overrode_age = true;
        let group = &groups[key];
        for index in &group.causes {
            keep_causes[*index] = false;
        }
        for index in &group.links {
            keep_links[*index] = false;
        }
        bytes -= group_bytes;
    }
    if bytes > max_bytes {
        return Err(format!(
            "archive cannot fit max-bytes={max_bytes} without dropping unresolved or supersedes-linked evidence ({unkeyed_causes} unkeyed causes, {unresolved_fixlinks} unmatched links); increase --max-bytes or repair their identities"
        ));
    }
    Ok((
        keep_causes,
        keep_links,
        unresolved_fixlinks,
        unkeyed_causes,
        size_cap_overrode_age,
    ))
}

fn unresolved_rows(dir: &Path, causes: &[Row], links: &[Row]) -> Vec<UnresolvedRow> {
    let keys: BTreeSet<_> = causes.iter().filter_map(|row| row.key.as_ref()).collect();
    let ids: BTreeSet<_> = causes.iter().filter_map(|row| row.id.as_deref()).collect();
    let mut unresolved = Vec::new();
    for row in causes {
        let reason = if row.key.is_none() {
            Some("unkeyed_cause")
        } else if row
            .supersedes
            .as_deref()
            .is_some_and(|id| !ids.contains(id))
        {
            Some("unresolved_supersedes")
        } else {
            None
        };
        if let Some(reason) = reason {
            unresolved.push(UnresolvedRow {
                file: row
                    .path
                    .strip_prefix(dir)
                    .unwrap_or(&row.path)
                    .display()
                    .to_string(),
                line: row.line_number,
                reason,
            });
        }
    }
    for row in links {
        if row.key.as_ref().is_none_or(|key| !keys.contains(key)) {
            unresolved.push(UnresolvedRow {
                file: "fixlinks.jsonl".to_owned(),
                line: row.line_number,
                reason: "unmatched_fixlink",
            });
        }
    }
    unresolved
}

fn apply_prune_rows(
    dir: &Path,
    args: &CausePruneArgs,
    causes: &[Row],
    links: &[Row],
    keep_causes: &[bool],
    keep_links: &[bool],
) -> Result<(), CliFailure> {
    if args.initialize {
        initialize_archive_marker(dir).map_err(|error| CliFailure::new(2, error))?;
    }
    let mut grouped: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for (row, keep) in causes.iter().zip(keep_causes) {
        if *keep {
            grouped
                .entry(row.path.clone())
                .or_default()
                .push(row.line.clone());
        }
    }
    let mut target_paths = causes
        .iter()
        .map(|row| row.path.clone())
        .collect::<BTreeSet<_>>();
    let fix_path = dir.join("fixlinks.jsonl");
    let retained_link_lines = links
        .iter()
        .zip(keep_links)
        .filter_map(|(row, keep)| keep.then_some(row.line.clone()))
        .collect::<Vec<_>>();
    if fix_path.exists() || !retained_link_lines.is_empty() {
        target_paths.insert(fix_path.clone());
        grouped.insert(fix_path, retained_link_lines);
    }
    apply_transaction(dir, &grouped, &target_paths).map_err(|error| CliFailure::new(1, error))
}

fn prune(args: &CausePruneArgs) -> Result<Report, CliFailure> {
    if args.retention_days <= 0
        || args.retention_days > MAX_RETENTION_DAYS
        || args.max_bytes == 0
        || args.max_bytes > MAX_BYTES
    {
        return Err(CliFailure::new(
            2,
            format!(
                "cause prune requires retention-days in 1..={MAX_RETENTION_DAYS} and max-bytes in 1..={MAX_BYTES}"
            ),
        ));
    }
    super::require_archive_parent(&args.dir).map_err(|error| CliFailure::new(2, error))?;
    if !args.dir.is_dir() {
        return Err(CliFailure::new(
            2,
            format!(
                "cause archive directory does not exist: {}",
                args.dir.display()
            ),
        ));
    }
    if args.initialize && !args.apply {
        return Err(CliFailure::new(2, "--initialize requires --apply"));
    }
    if args.apply && !args.initialize {
        ensure_archive_marker(&args.dir, true).map_err(|error| CliFailure::new(2, error))?;
    }
    let _lock =
        super::lock_archive(&args.dir, args.apply).map_err(|error| CliFailure::new(1, error))?;
    if args.apply && !args.initialize {
        ensure_archive_marker(&args.dir, true).map_err(|error| CliFailure::new(2, error))?;
    }
    let cutoff = Utc::now() - Duration::days(args.retention_days);
    let (causes, links, files_scanned) =
        read_rows(&args.dir).map_err(|error| CliFailure::new(2, error))?;
    let bytes_before = causes
        .iter()
        .chain(&links)
        .map(|row| row.line.len() as u64)
        .sum::<u64>();
    let (keep_causes, keep_links, unresolved_fixlinks, unkeyed_causes, size_cap_overrode_age) =
        plan_rows(&causes, &links, cutoff, args.max_bytes)
            .map_err(|error| CliFailure::new(2, error))?;
    let unresolved = unresolved_rows(&args.dir, &causes, &links);
    let after = retained_bytes(&causes, &keep_causes, &links, &keep_links);
    if args.apply {
        apply_prune_rows(&args.dir, args, &causes, &links, &keep_causes, &keep_links)?;
    }
    Ok(Report {
        schema: "shipyard.cause-prune/v1",
        dir: args.dir.display().to_string(),
        retention_days: args.retention_days,
        max_bytes: args.max_bytes,
        cutoff: cutoff.to_rfc3339(),
        files_scanned,
        cause_rows: causes.len(),
        fixlink_rows: links.len(),
        rows_pruned: causes
            .iter()
            .zip(&keep_causes)
            .filter(|(_, keep)| !**keep)
            .count()
            + links
                .iter()
                .zip(&keep_links)
                .filter(|(_, keep)| !**keep)
                .count(),
        bytes_before,
        bytes_after: after,
        dry_run: !args.apply,
        unresolved_fixlinks,
        unkeyed_causes,
        size_cap_overrode_age,
        unresolved,
    })
}

pub(super) fn command<W: Write>(
    args: &CausePruneArgs,
    json: bool,
    out: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let report = prune(args)?;
    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&report).unwrap_or_default()
        )
    } else {
        writeln!(
            out,
            "cause prune: {} row(s) pruned, {} -> {} bytes ({}; {} unresolved row(s), {} unresolved link(s), {} unkeyed cause(s), size cap overrode age: {})",
            report.rows_pruned,
            report.bytes_before,
            report.bytes_after,
            if report.dry_run {
                "dry run; use --apply to write"
            } else {
                "applied"
            },
            report.unresolved.len(),
            report.unresolved_fixlinks,
            report.unkeyed_causes,
            report.size_cap_overrode_age
        )
    }
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cause(run_id: u64, named_at: &str) -> String {
        serde_json::json!({
            "schema": "ci.cause/v1", "repo": "o/r", "run_id": run_id, "attempt": 1,
            "job_id": run_id + 10, "job": "test", "head_sha": null, "event": "pull_request", "pr": 1,
            "failed_step": "test", "failure_class": "test", "format": "ctest", "signature": "x",
            "detail": null, "evidence": {"log_line": 1, "excerpt": ["x"]}, "fix_hint": null,
            "confidence": "high", "source": "extractor", "extractor": "c2", "named_at": named_at,
            "supersedes": null, "kind": "test", "location": "x", "step_failed": true, "retrospective": true
        }).to_string()
    }

    #[test]
    fn dry_run_is_bounded_and_apply_removes_old_rows() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        fs::write(
            root.path().join("causes/2025-01.jsonl"),
            format!("{}\n", cause(1, "2025-01-01T00:00:00Z")),
        )
        .unwrap();
        fs::write(
            root.path().join("causes/2026-10.jsonl"),
            format!("{}\n", cause(2, "2026-10-01T00:00:00Z")),
        )
        .unwrap();
        let args = CausePruneArgs {
            dir: root.path().to_owned(),
            retention_days: 30_000,
            max_bytes: 1,
            apply: false,
            initialize: false,
        };
        let preview = prune(&args).unwrap();
        assert!(preview.dry_run && preview.bytes_after <= 1);
        assert!(!root.path().join(".lock").exists());
        assert!(root.path().join("causes/2025-01.jsonl").exists());
        let args = CausePruneArgs {
            apply: true,
            ..args
        };
        fs::write(
            marker_path(root.path()),
            r#"{"schema":"shipyard.cause-archive/v1","frozen":false}"#,
        )
        .unwrap();
        prune(&args).unwrap();
        assert!(!root.path().join("causes/2025-01.jsonl").exists());
        assert!(!root.path().join("causes/2026-10.jsonl").exists());
    }

    #[test]
    fn malformed_rows_fail_closed() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        fs::write(root.path().join("causes/2026-10.jsonl"), "not json\n").unwrap();
        let args = CausePruneArgs {
            dir: root.path().to_owned(),
            retention_days: 365,
            max_bytes: 10,
            apply: true,
            initialize: false,
        };
        fs::write(
            marker_path(root.path()),
            r#"{"schema":"shipyard.cause-archive/v1","frozen":false}"#,
        )
        .unwrap();
        let error = prune(&args).unwrap_err();
        assert_eq!(error.code, 2);
        assert!(error.message.contains("invalid JSON"));
        assert_eq!(
            fs::read_to_string(root.path().join("causes/2026-10.jsonl")).unwrap(),
            "not json\n"
        );
    }

    #[test]
    fn apply_requires_a_mutable_archive_marker() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        fs::write(
            root.path().join("causes/2026-10.jsonl"),
            format!("{}\n", cause(1, "2026-10-01T00:00:00Z")),
        )
        .unwrap();
        let error = prune(&CausePruneArgs {
            dir: root.path().to_owned(),
            retention_days: 1,
            max_bytes: MAX_BYTES,
            apply: true,
            initialize: false,
        })
        .unwrap_err();
        assert_eq!(error.code, 2);
        assert!(error.message.contains("unmarked archive"));
    }

    #[test]
    fn frozen_archives_are_never_mutated() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        fs::write(
            root.path().join("causes/2026-10.jsonl"),
            format!("{}\n", cause(1, "2026-10-01T00:00:00Z")),
        )
        .unwrap();
        fs::write(
            marker_path(root.path()),
            r#"{"schema":"shipyard.cause-archive/v1","frozen":true}
"#,
        )
        .unwrap();
        let error = prune(&CausePruneArgs {
            dir: root.path().to_owned(),
            retention_days: 1,
            max_bytes: MAX_BYTES,
            apply: true,
            initialize: false,
        })
        .unwrap_err();
        assert_eq!(error.code, 2);
        assert!(error.message.contains("frozen"));
    }

    #[test]
    fn retention_inputs_have_safe_upper_bounds() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        for (retention_days, max_bytes) in [(MAX_RETENTION_DAYS + 1, MAX_BYTES), (1, MAX_BYTES + 1)]
        {
            let error = prune(&CausePruneArgs {
                dir: root.path().to_owned(),
                retention_days,
                max_bytes,
                apply: false,
                initialize: false,
            })
            .unwrap_err();
            assert_eq!(error.code, 2);
        }
    }

    #[test]
    fn recent_fix_link_keeps_its_older_cause_group() {
        let now = Utc::now();
        let key = RedKey {
            repo: "o/r".to_owned(),
            run_id: 7,
            attempt: 1,
        };
        let causes = vec![Row {
            path: PathBuf::from("causes/2026-01.jsonl"),
            line: "cause\n".to_owned(),
            at: now - Duration::days(10),
            key: Some(key.clone()),
            id: Some("old".to_owned()),
            supersedes: None,
            line_number: 1,
        }];
        let links = vec![Row {
            path: PathBuf::from("fixlinks.jsonl"),
            line: "link\n".to_owned(),
            at: now,
            key: Some(key),
            id: None,
            supersedes: None,
            line_number: 1,
        }];
        let (keep_causes, keep_links, unresolved, _, _) =
            plan_rows(&causes, &links, now - Duration::days(1), MAX_BYTES).unwrap();
        assert_eq!(
            (keep_causes, keep_links, unresolved),
            (vec![true], vec![true], 0)
        );
    }

    #[test]
    fn supersedes_groups_are_pinned_even_when_the_size_cap_would_drop_them() {
        let now = Utc::now();
        let old_key = RedKey {
            repo: "o/r".to_owned(),
            run_id: 7,
            attempt: 1,
        };
        let replacement_key = RedKey {
            repo: "o/r".to_owned(),
            run_id: 8,
            attempt: 1,
        };
        let causes = vec![
            Row {
                path: PathBuf::from("causes/2025-01.jsonl"),
                line: "old\n".to_owned(),
                at: now - Duration::days(400),
                key: Some(old_key),
                id: Some("old".to_owned()),
                supersedes: None,
                line_number: 1,
            },
            Row {
                path: PathBuf::from("causes/2025-02.jsonl"),
                line: "replacement\n".to_owned(),
                at: now - Duration::days(300),
                key: Some(replacement_key),
                id: Some("replacement".to_owned()),
                supersedes: Some("old".to_owned()),
                line_number: 1,
            },
        ];
        let error = plan_rows(&causes, &[], now - Duration::days(1), 1).unwrap_err();

        assert!(error.contains("supersedes-linked evidence"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_archive_entries_fail_closed() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        let outside = root.path().join("outside.jsonl");
        fs::write(&outside, "not used\n").unwrap();
        symlink(&outside, root.path().join("causes/2026-10.jsonl")).unwrap();
        let error = prune(&CausePruneArgs {
            dir: root.path().to_owned(),
            retention_days: 1,
            max_bytes: MAX_BYTES,
            apply: false,
            initialize: false,
        })
        .unwrap_err();
        assert_eq!(error.code, 1);
        assert!(error.message.contains("regular file"));
    }
}
