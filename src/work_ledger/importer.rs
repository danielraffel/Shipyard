//! Legacy lifecycle scanning and canonical projection.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::queue_absent_recovery::{
    QueueAbsentRecoveryRecord, RECOVERY_SCHEMA_VERSION as QUEUE_ABSENT_RECOVERY_SCHEMA_VERSION,
    recovery_record_path, validate_recovery_record,
};
use crate::queue_request::{
    QUEUED_EXECUTION_SCHEMA_VERSION, QueuedExecutionEnvelope, QueuedExecutionOutcome,
    validate_queued_execution_envelope, validate_queued_execution_outcome,
};
use crate::recovery_worker::{RECOVERY_SCHEMA_VERSION, RecoveryRecord, validate_record};
use crate::ship_state::{SHIP_STATE_SCHEMA_VERSION, ShipState, repository_key};

mod projection;
mod validation;

use projection::legacy_is_newer;
pub(super) use projection::{candidate, import_report};
pub(super) use validation::validate_legacy_record;

use super::{
    ImportCandidate, ImportReport, WorkLedgerError, WorkLedgerResult, digest, opaque_path_ref,
    opaque_ref, validate_candidate,
};

/// Scan known legacy stores without writing anything.
#[cfg(unix)]
pub(super) fn scan_legacy(state_dir: &Path) -> WorkLedgerResult<Vec<ImportCandidate>> {
    let mut candidates = scan_ship_records(state_dir)?;
    scan_queue_records(state_dir, &mut candidates)?;
    scan_recovery(state_dir, &mut candidates)?;
    scan_steward(state_dir, &mut candidates)?;
    deduplicate_and_validate(candidates)
}

#[cfg(unix)]
fn scan_ship_records(state_dir: &Path) -> WorkLedgerResult<Vec<ImportCandidate>> {
    let ship_root = state_dir.join("ship");
    let mut candidates = Vec::new();
    scan_tree(
        state_dir,
        &ship_root.join("scoped"),
        "ship_state",
        true,
        &mut candidates,
    )?;
    let mut active_by_id = candidates
        .drain(..)
        .map(|candidate| (candidate.work_id.clone(), candidate))
        .collect::<BTreeMap<_, _>>();
    let mut legacy_active = Vec::new();
    scan_tree(
        state_dir,
        &ship_root,
        "ship_state",
        false,
        &mut legacy_active,
    )?;
    for candidate in legacy_active {
        if let Some(pr) = candidate.pr
            && ship_collision_marker_present(state_dir, pr)?
        {
            continue;
        }
        match active_by_id.get(&candidate.work_id) {
            Some(scoped) if legacy_is_newer(&candidate, scoped)? => {
                active_by_id.insert(candidate.work_id.clone(), candidate);
            }
            None => {
                active_by_id.insert(candidate.work_id.clone(), candidate);
            }
            Some(_) => {}
        }
    }
    let active_ship_ids = active_by_id
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    candidates.extend(active_by_id.into_values());
    let mut archived = Vec::new();
    scan_tree(
        state_dir,
        &ship_root.join("archive"),
        "ship_state",
        true,
        &mut archived,
    )?;
    let mut latest_archived = BTreeMap::new();
    for candidate in archived {
        match latest_archived.get(&candidate.work_id) {
            Some(existing) if legacy_is_newer(&candidate, existing)? => {
                latest_archived.insert(candidate.work_id.clone(), candidate);
            }
            None => {
                latest_archived.insert(candidate.work_id.clone(), candidate);
            }
            Some(_) => {}
        }
    }
    candidates.extend(
        latest_archived
            .into_values()
            .filter(|candidate| !active_ship_ids.contains(&candidate.work_id)),
    );
    Ok(candidates)
}

#[cfg(unix)]
fn scan_queue_records(
    state_dir: &Path,
    candidates: &mut Vec<ImportCandidate>,
) -> WorkLedgerResult<()> {
    scan_tree(
        state_dir,
        &state_dir.join("queue").join("requests"),
        "queue_request",
        false,
        candidates,
    )?;
    scan_tree(
        state_dir,
        &state_dir.join("queue").join("outcomes"),
        "queue_outcome",
        false,
        candidates,
    )?;
    scan_tree(
        state_dir,
        &state_dir.join("queue").join("recovery"),
        "recovery",
        false,
        candidates,
    )?;
    Ok(())
}

fn deduplicate_and_validate(
    mut candidates: Vec<ImportCandidate>,
) -> WorkLedgerResult<Vec<ImportCandidate>> {
    candidates.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.work_id.cmp(&right.work_id))
            .then_with(|| left.content_digest.cmp(&right.content_digest))
    });
    let mut deduplicated: Vec<ImportCandidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if let Some(previous) = deduplicated.last()
            && previous.work_id == candidate.work_id
        {
            if previous.content_digest != candidate.content_digest {
                return Err(WorkLedgerError::Refused(
                    "mirrored legacy records disagree for one logical work item".to_owned(),
                ));
            }
            continue;
        }
        deduplicated.push(candidate);
    }
    for candidate in &deduplicated {
        validate_candidate(candidate)?;
    }
    Ok(deduplicated)
}

/// Legacy import is gated until an equivalent no-follow traversal exists.
#[cfg(not(unix))]
pub(super) fn scan_legacy(_state_dir: &Path) -> WorkLedgerResult<Vec<ImportCandidate>> {
    Err(WorkLedgerError::Refused(
        "work-ledger legacy import is currently supported only on Unix hosts".to_owned(),
    ))
}

/// Build the dry-run report without opening or creating the database.
#[must_use]
pub(super) fn dry_run_report(candidates: &[ImportCandidate]) -> ImportReport {
    import_report(candidates, false, candidates.len(), 0)
}

#[cfg(unix)]
fn scan_tree(
    state_dir: &Path,
    root: &Path,
    kind: &str,
    recurse: bool,
    candidates: &mut Vec<ImportCandidate>,
) -> WorkLedgerResult<()> {
    let Some(directory) = open_pinned_directory(state_dir, root)? else {
        return Ok(());
    };
    scan_pinned_tree(state_dir, root, &directory, kind, recurse, candidates)
}

#[cfg(unix)]
fn open_pinned_directory(state_dir: &Path, root: &Path) -> WorkLedgerResult<Option<std::fs::File>> {
    use rustix::fs::{Mode, OFlags, open, openat};
    use std::path::Component;

    let relative = root.strip_prefix(state_dir).map_err(|_| {
        WorkLedgerError::Refused("legacy root is outside the configured state directory".to_owned())
    })?;
    let mut directory = std::fs::File::from(
        match open(
            state_dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(directory) => directory,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(std::io::Error::from(error).into()),
        },
    );
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(WorkLedgerError::Refused(
                "legacy root path is not normalized".to_owned(),
            ));
        };
        directory = match openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(file) => std::fs::File::from(file),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(std::io::Error::from(error).into()),
        };
    }
    Ok(Some(directory))
}

#[cfg(unix)]
fn scan_pinned_tree(
    state_dir: &Path,
    root: &Path,
    directory: &std::fs::File,
    kind: &str,
    recurse: bool,
    candidates: &mut Vec<ImportCandidate>,
) -> WorkLedgerResult<()> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, statat};
    use std::ffi::OsStr;
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;

    let mut names = rustix::fs::Dir::read_from(directory)
        .map_err(std::io::Error::from)?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_bytes().to_vec())
                .map_err(std::io::Error::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.retain(|name| name.as_slice() != b"." && name.as_slice() != b"..");
    names.sort();
    for name in names {
        let name = OsStr::from_bytes(&name);
        let path = root.join(name);
        let stat =
            statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Symlink => {
                return Err(WorkLedgerError::Refused(format!(
                    "legacy source {} is a symlink",
                    opaque_path_ref(state_dir, &path, None)
                )));
            }
            FileType::Directory if recurse => {
                let child = std::fs::File::from(
                    openat(
                        directory,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(std::io::Error::from)?,
                );
                scan_pinned_tree(state_dir, &path, &child, kind, true, candidates)?;
            }
            FileType::RegularFile
                if path.extension().and_then(|extension| extension.to_str()) == Some("json") =>
            {
                let mut child = std::fs::File::from(
                    openat(
                        directory,
                        name,
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(std::io::Error::from)?,
                );
                if !child.metadata()?.is_file() {
                    return Err(WorkLedgerError::Refused(
                        "legacy source changed type while opening".to_owned(),
                    ));
                }
                let mut bytes = Vec::new();
                child.read_to_end(&mut bytes)?;
                let source = opaque_path_ref(state_dir, &path, None);
                let value: Value =
                    serde_json::from_slice(&bytes).map_err(|error| WorkLedgerError::Json {
                        source: source.clone(),
                        error,
                    })?;
                validate_legacy_record(kind, &source, &value)?;
                validate_authoritative_filename(state_dir, kind, &path, &value)?;
                if kind == "recovery" {
                    validate_recovery_storage_path(state_dir, &path, &value)?;
                }
                candidates.push(candidate(kind, source, digest(&bytes), &value));
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_authoritative_filename(
    state_dir: &Path,
    kind: &str,
    path: &Path,
    value: &Value,
) -> WorkLedgerResult<()> {
    if kind == "ship_state" {
        let record = serde_json::from_value::<ShipState>(value.clone()).map_err(|_| {
            WorkLedgerError::Refused("legacy ship-state path lacks valid identity".to_owned())
        })?;
        return validate_ship_state_path(state_dir, path, &record);
    }
    let expected = match kind {
        "queue_request" => serde_json::from_value::<QueuedExecutionEnvelope>(value.clone())
            .ok()
            .map(|record| format!("{}.json", record.job_id)),
        "queue_outcome" => serde_json::from_value::<QueuedExecutionOutcome>(value.clone())
            .ok()
            .map(|record| format!("{}.json", record.job_id())),
        "recovery" => {
            if let Ok(record) = serde_json::from_value::<QueueAbsentRecoveryRecord>(value.clone()) {
                recovery_record_path(Path::new(""), &record.repo, record.pr)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            } else {
                serde_json::from_value::<RecoveryRecord>(value.clone())
                    .ok()
                    .map(|record| format!("{}.json", record.request.id))
            }
        }
        _ => None,
    };
    if let Some(expected) = expected
        && path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str())
    {
        return Err(WorkLedgerError::Refused(format!(
            "legacy {kind} source filename disagrees with its embedded identity"
        )));
    }
    Ok(())
}

fn validate_ship_state_path(
    state_dir: &Path,
    path: &Path,
    record: &ShipState,
) -> WorkLedgerResult<()> {
    let relative = path
        .strip_prefix(state_dir.join("ship"))
        .map_err(|_| WorkLedgerError::Refused("ship state is outside its store".to_owned()))?;
    let components = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let active = format!("{}.json", record.pr);
    let repo_key = repository_key(&record.repo);
    let valid = match components.as_slice() {
        [filename] => filename == &active,
        [scoped, key, filename] if scoped == "scoped" => key == &repo_key && filename == &active,
        [archive, filename] if archive == "archive" => {
            valid_ship_archive_filename(filename, record.pr)
        }
        [archive, key, filename] if archive == "archive" => {
            key == &repo_key && valid_ship_archive_filename(filename, record.pr)
        }
        _ => false,
    };
    if !valid {
        return Err(WorkLedgerError::Refused(
            "legacy ship-state path disagrees with its embedded authority".to_owned(),
        ));
    }
    Ok(())
}

fn valid_ship_archive_filename(filename: &str, pr: u64) -> bool {
    let Some(stamp) = filename
        .strip_prefix(&format!("{pr}-"))
        .and_then(|value| value.strip_suffix(".json"))
    else {
        return false;
    };
    stamp.len() == 16
        && stamp.as_bytes()[8] == b'T'
        && stamp.as_bytes()[15] == b'Z'
        && stamp
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 8 | 15) || byte.is_ascii_digit())
}

#[cfg(unix)]
fn read_pinned_regular_file(state_dir: &Path, path: &Path) -> WorkLedgerResult<Option<Vec<u8>>> {
    use rustix::fs::{Mode, OFlags, openat};
    use std::io::Read;

    let parent = path.parent().ok_or_else(|| {
        WorkLedgerError::Refused("legacy source has no parent directory".to_owned())
    })?;
    let Some(directory) = open_pinned_directory(state_dir, parent)? else {
        return Ok(None);
    };
    let name = path
        .file_name()
        .ok_or_else(|| WorkLedgerError::Refused("legacy source has no file name".to_owned()))?;
    let mut file = match openat(
        &directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => std::fs::File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    if !file.metadata()?.is_file() {
        return Err(WorkLedgerError::Refused(
            "legacy source is not a regular file".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

#[cfg(unix)]
fn scan_recovery(state_dir: &Path, candidates: &mut Vec<ImportCandidate>) -> WorkLedgerResult<()> {
    let root = state_dir.join("merge-steward").join("recovery");
    let mut active = Vec::new();
    // Only root records and cold archive records are authoritative work.
    // `head-index` and `witnesses` are derived lookup/evidence structures and
    // must not become duplicate canonical work items.
    scan_tree(state_dir, &root, "recovery", false, &mut active)?;
    let mut by_id = active
        .into_iter()
        .map(|candidate| (candidate.work_id.clone(), candidate))
        .collect::<BTreeMap<_, _>>();
    let archived = scan_canonical_recovery_archive(state_dir, &root.join("archive"))?;
    for candidate in archived {
        by_id.insert(candidate.work_id.clone(), candidate);
    }
    candidates.extend(
        by_id
            .into_values()
            .filter(|entry| entry.repo.is_some() && entry.pr.is_some()),
    );
    Ok(())
}

#[cfg(unix)]
fn scan_canonical_recovery_archive(
    state_dir: &Path,
    archive_root: &Path,
) -> WorkLedgerResult<Vec<ImportCandidate>> {
    let mut archived = Vec::new();
    scan_tree(state_dir, archive_root, "recovery", true, &mut archived)?;
    Ok(archived)
}

fn validate_recovery_storage_path(
    state_dir: &Path,
    path: &Path,
    value: &Value,
) -> WorkLedgerResult<()> {
    let relative = path.strip_prefix(state_dir).map_err(|_| {
        WorkLedgerError::Refused("recovery source is outside the state directory".to_owned())
    })?;
    let components = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if components.first().map(String::as_str) != Some("merge-steward")
        || components.get(1).map(String::as_str) != Some("recovery")
        || components.get(2).map(String::as_str) != Some("archive")
    {
        return Ok(());
    }
    let id = value
        .pointer("/request/id")
        .and_then(Value::as_str)
        .ok_or_else(|| WorkLedgerError::Refused("archived recovery lacks an ID".to_owned()))?;
    let expected_file = format!("{id}.json");
    if id.len() < 2
        || components.len() != 5
        || components[3] != id[..2]
        || components[4] != expected_file
    {
        return Err(WorkLedgerError::Refused(
            "recovery archive record is outside its canonical shard".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn scan_steward(state_dir: &Path, candidates: &mut Vec<ImportCandidate>) -> WorkLedgerResult<()> {
    let path = state_dir.join("merge-steward.json");
    let Some(bytes) = read_pinned_regular_file(state_dir, &path)? else {
        return Ok(());
    };
    let path_ref = opaque_path_ref(state_dir, &path, None);
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| WorkLedgerError::Json {
        source: path_ref,
        error,
    })?;
    if !value.is_object() {
        return Err(WorkLedgerError::Refused(
            "legacy steward ledger root is not an object".to_owned(),
        ));
    }
    for (field, kind) in [
        ("terminal_handoffs", "terminal_handoff"),
        ("resume_records", "resume_record"),
    ] {
        let Some(field_value) = value.get(field) else {
            continue;
        };
        let records = field_value.as_object().ok_or_else(|| {
            WorkLedgerError::Refused(format!(
                "legacy source {} has a malformed {field} map",
                opaque_path_ref(state_dir, &path, None)
            ))
        })?;
        let mut keys = records.keys().collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            let record = &records[key];
            validate_legacy_record(kind, &opaque_path_ref(state_dir, &path, Some(key)), record)?;
            let identity_field = if kind == "terminal_handoff" {
                "dedupe_key"
            } else {
                "terminal_handoff_key"
            };
            if record.get(identity_field).and_then(Value::as_str) != Some(key) {
                return Err(WorkLedgerError::Refused(format!(
                    "legacy {kind} map key disagrees with its embedded identity"
                )));
            }
            let encoded = serde_json::to_vec(record).expect("JSON value serializes");
            candidates.push(candidate(
                kind,
                opaque_path_ref(state_dir, &path, Some(key)),
                digest(&encoded),
                record,
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn ship_collision_marker_present(state_dir: &Path, pr: u64) -> WorkLedgerResult<bool> {
    use rustix::fs::{AtFlags, FileType, statat};

    let root = state_dir.join("ship");
    let Some(directory) = open_pinned_directory(state_dir, &root)? else {
        return Ok(false);
    };
    let name = format!("{pr}.scoped-collision");
    match statat(&directory, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => Ok(true),
        Ok(_) => Err(WorkLedgerError::Refused(
            "ship collision marker is not a regular file".to_owned(),
        )),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(error) => Err(std::io::Error::from(error).into()),
    }
}
