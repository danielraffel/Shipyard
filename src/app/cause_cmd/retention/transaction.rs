//! Crash-recoverable transactions for cause-archive retention.
//!
//! A prune transaction is prepared under the archive's exclusive lock.  The
//! prepared manifest is immutable.  Live files are changed only after every
//! backup and replacement has been staged and checksummed.  A separate
//! committed receipt is published atomically after all replacements have been
//! synced.  If the process stops before that receipt exists, the next writer
//! restores the old bytes from copies and keeps those copies until cleanup
//! succeeds.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TRANSACTION_DIR: &str = ".prune-txn";
const GC_DIR: &str = ".prune-gc";
const MANIFEST_NAME: &str = "manifest.json";
const COMMITTED_NAME: &str = "committed";
const TX_SCHEMA: &str = "shipyard.cause-prune-txn/v2";
const COMMIT_SCHEMA: &str = "shipyard.cause-prune-commit/v1";
// Both bounds are intentional: MAX_ENTRIES bounds the number of archive files
// touched in one pass, while MAX_MANIFEST_BYTES independently bounds the
// serialized path/checksum metadata. Either limit can refuse a pathological
// transaction before live bytes are changed.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_ENTRIES: usize = 1024;
const MAX_TRANSACTION_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TRANSACTION_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TRANSACTION_FILES: usize = MAX_ENTRIES * 2 + 16;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TransactionEntry {
    rel: String,
    old: String,
    old_sha256: Option<String>,
    new: Option<String>,
    new_sha256: Option<String>,
    existed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TransactionManifest {
    schema: String,
    state: String,
    entries: Vec<TransactionEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CommitReceipt {
    schema: String,
    manifest_sha256: String,
}

fn transaction_dir(dir: &Path) -> PathBuf {
    dir.join(TRANSACTION_DIR)
}

fn gc_dir(dir: &Path) -> PathBuf {
    dir.join(GC_DIR)
}

#[cfg(not(windows))]
fn sync_dir(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))
}

// Windows does not expose a portable directory fsync. File contents are still
// synced, and rename is the platform's atomic publication primitive.
#[cfg(windows)]
fn sync_dir(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn safe_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn valid_archive_relative(path: &str) -> bool {
    if path == "fixlinks.jsonl" {
        return true;
    }
    let path = Path::new(path);
    let mut components = path.components();
    matches!(components.next(), Some(std::path::Component::Normal(name)) if name == "causes")
        && matches!(components.next(), Some(std::path::Component::Normal(name)) if Path::new(name).extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        && components.next().is_none()
}

fn validate_archive_target(dir: &Path, rel: &str) -> Result<PathBuf, String> {
    if !safe_relative(rel) || !valid_archive_relative(rel) {
        return Err(format!(
            "prune transaction targets an unsafe archive path {rel}"
        ));
    }
    let target = dir.join(rel);
    let mut current = dir.to_owned();
    for component in Path::new(rel).components() {
        current.push(component.as_os_str());
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "refusing symlink archive path {}",
                current.display()
            ));
        }
        if current != target && !metadata.is_dir() {
            return Err(format!("{} is not an archive directory", current.display()));
        }
    }
    if let Ok(metadata) = fs::symlink_metadata(&target)
        && !metadata.is_file()
    {
        return Err(format!(
            "{} is not a regular archive file",
            target.display()
        ));
    }
    Ok(target)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{} is not a regular transaction file",
            path.display()
        ));
    }
    if metadata.len() > MAX_TRANSACTION_FILE_BYTES {
        return Err(format!("{} exceeds transaction file limit", path.display()));
    }
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn add_transaction_bytes(total: &mut u64, path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{} is not a regular transaction file",
            path.display()
        ));
    }
    if metadata.len() > MAX_TRANSACTION_FILE_BYTES
        || *total > MAX_TRANSACTION_TOTAL_BYTES.saturating_sub(metadata.len())
    {
        return Err(format!(
            "transaction bytes exceed safety limit at {}",
            path.display()
        ));
    }
    *total += metadata.len();
    Ok(())
}

fn sync_file(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn generated_temp(target: &Path, kind: &str) -> Result<PathBuf, String> {
    let parent = target
        .parent()
        .ok_or_else(|| "archive target has no parent".to_owned())?;
    let name = target
        .file_name()
        .ok_or_else(|| "archive target has no file name".to_owned())?
        .to_string_lossy();
    Ok(parent.join(format!(".{name}.{kind}")))
}

fn remove_generated_temps(target: &Path) -> Result<(), String> {
    for kind in ["prune", "restore"] {
        let temp = generated_temp(target, kind)?;
        let Ok(metadata) = fs::symlink_metadata(&temp) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!("unsafe generated temporary {}", temp.display()));
        }
        fs::remove_file(&temp).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    if fs::symlink_metadata(&temp).is_ok() {
        return Err(format!(
            "refusing to overwrite stale temporary file {}",
            temp.display()
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|error| format!("{}: {error}", temp.display()))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temp);
        return Err(format!("{}: {error}", temp.display()));
    }
    fs::rename(&temp, path).map_err(|error| format!("{}: {error}", path.display()))?;
    sync_dir(parent)
}

fn stage_file(path: &Path, lines: &[String]) -> Result<String, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    let metadata = fs::symlink_metadata(parent).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{} is not a transaction directory",
            parent.display()
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    for line in lines {
        file.write_all(line.as_bytes())
            .map_err(|error| error.to_string())?;
    }
    file.sync_all().map_err(|error| error.to_string())?;
    sync_dir(parent)?;
    sha256_file(path)
}

fn validate_tx_dir(tx: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(tx).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("{} is not a transaction directory", tx.display()));
    }
    let mut file_count = 0usize;
    for entry in fs::read_dir(tx).map_err(|error| error.to_string())? {
        file_count += 1;
        if file_count > MAX_TRANSACTION_FILES {
            return Err(format!(
                "transaction directory {} is too large",
                tx.display()
            ));
        }
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "refusing symlink in transaction {}",
                path.display()
            ));
        }
        match name.as_str() {
            "old" | "new" => {
                if !metadata.is_dir() {
                    return Err(format!("{} is not a transaction directory", path.display()));
                }
                for child in fs::read_dir(&path).map_err(|error| error.to_string())? {
                    file_count += 1;
                    if file_count > MAX_TRANSACTION_FILES {
                        return Err(format!(
                            "transaction directory {} is too large",
                            tx.display()
                        ));
                    }
                    let child = child.map_err(|error| error.to_string())?;
                    let child_meta =
                        fs::symlink_metadata(child.path()).map_err(|error| error.to_string())?;
                    if child_meta.file_type().is_symlink() || !child_meta.is_file() {
                        return Err(format!(
                            "invalid transaction staged entry {}",
                            child.path().display()
                        ));
                    }
                    let child_name = child.file_name().to_string_lossy().into_owned();
                    if child_name.is_empty()
                        || !child_name.bytes().all(|byte| byte.is_ascii_digit())
                    {
                        return Err(format!(
                            "invalid transaction staged entry {}",
                            child.path().display()
                        ));
                    }
                }
            }
            MANIFEST_NAME | COMMITTED_NAME => {
                if !metadata.is_file() {
                    return Err(format!("{} is not a transaction file", path.display()));
                }
            }
            name if name.starts_with(".manifest.json.tmp-")
                || name.starts_with(".committed.tmp-") =>
            {
                if !metadata.is_file() {
                    return Err(format!("{} is not a transaction file", path.display()));
                }
            }
            _ => return Err(format!("unexpected transaction entry {}", path.display())),
        }
    }
    Ok(())
}

fn cleanup_tx(tx: &Path, dir: &Path) -> Result<(), String> {
    validate_tx_dir(tx)?;
    let gc = gc_dir(dir);
    if fs::symlink_metadata(&gc).is_ok() {
        cleanup_gc(dir)?;
    }
    fs::rename(tx, &gc).map_err(|error| error.to_string())?;
    sync_dir(dir)?;
    cleanup_gc(dir)
}

fn cleanup_gc(dir: &Path) -> Result<(), String> {
    let gc = gc_dir(dir);
    let Ok(metadata) = fs::symlink_metadata(&gc) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("unsafe prune cleanup directory {}", gc.display()));
    }
    // A GC directory is never rolled back. Validate only that anything left
    // in it is generated transaction material, then remove it. If removal is
    // interrupted, the next recovery repeats this same one-way cleanup.
    validate_tx_dir(&gc)?;
    fs::remove_dir_all(&gc).map_err(|error| error.to_string())?;
    sync_dir(dir)
}

fn read_manifest(tx: &Path) -> Result<(TransactionManifest, Vec<u8>), String> {
    let path = tx.join(MANIFEST_NAME);
    let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_MANIFEST_BYTES
    {
        return Err(format!(
            "invalid prune transaction manifest {}",
            path.display()
        ));
    }
    let bytes = fs::read(&path).map_err(|error| error.to_string())?;
    let manifest: TransactionManifest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid prune transaction manifest: {error}"))?;
    if manifest.schema != TX_SCHEMA || manifest.state != "prepared" {
        return Err("invalid prune transaction state or schema".to_owned());
    }
    if manifest.entries.is_empty() || manifest.entries.len() > MAX_ENTRIES {
        return Err("invalid prune transaction entry count".to_owned());
    }
    Ok((manifest, bytes))
}

fn validate_manifest(
    dir: &Path,
    tx: &Path,
    manifest: &TransactionManifest,
    committed: bool,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    let mut total_bytes = 0u64;
    for (index, entry) in manifest.entries.iter().enumerate() {
        if !seen.insert(entry.rel.clone()) {
            return Err(format!("duplicate transaction target {}", entry.rel));
        }
        let target = validate_archive_target(dir, &entry.rel)?;
        let expected_old = format!("old/{index}");
        if entry.old != expected_old || !entry.old.starts_with("old/") {
            return Err("transaction backup path does not match entry index".to_owned());
        }
        let old = tx.join(&entry.old);
        if entry.existed {
            let Some(expected) = entry.old_sha256.as_deref() else {
                return Err("transaction entry lacks backup checksum".to_owned());
            };
            add_transaction_bytes(&mut total_bytes, &old)?;
            if !valid_digest(expected) || sha256_file(&old)? != expected {
                return Err(format!(
                    "transaction backup checksum mismatch {}",
                    old.display()
                ));
            }
        } else if entry.old_sha256.is_some() || fs::symlink_metadata(&old).is_ok() {
            return Err("nonexistent target has a backup".to_owned());
        }
        if let Some(new_name) = &entry.new {
            let expected_new = format!("new/{index}");
            if new_name != &expected_new || !new_name.starts_with("new/") {
                return Err("transaction staged path does not match entry index".to_owned());
            }
            let Some(expected) = entry.new_sha256.as_deref() else {
                return Err("transaction entry lacks staged checksum".to_owned());
            };
            if !valid_digest(expected) {
                return Err("transaction staged checksum is invalid".to_owned());
            }
            if committed {
                if fs::symlink_metadata(&target).is_err() {
                    return Err(format!(
                        "committed archive target is missing {}",
                        target.display()
                    ));
                }
                if sha256_file(&target)? != expected {
                    return Err(format!(
                        "committed archive checksum mismatch {}",
                        target.display()
                    ));
                }
            } else {
                let new = tx.join(new_name);
                match fs::symlink_metadata(&new) {
                    Ok(_) => {
                        add_transaction_bytes(&mut total_bytes, &new)?;
                        if sha256_file(&new)? != expected {
                            return Err(format!(
                                "transaction staged checksum mismatch {}",
                                new.display()
                            ));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        // A crash after the staged rename can leave the new
                        // bytes at the live target while the prepared
                        // manifest still names the staging path. Verify that
                        // target before accepting recovery.
                        if sha256_file(&target)? != expected {
                            return Err(format!(
                                "missing staged and live replacement bytes {}",
                                target.display()
                            ));
                        }
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
        } else if entry.new_sha256.is_some() {
            return Err("transaction entry has checksum without staged file".to_owned());
        } else if committed && fs::symlink_metadata(&target).is_ok() {
            return Err(format!(
                "committed archive target should be absent {}",
                target.display()
            ));
        }
        if entry.existed
            && let Ok(meta) = fs::symlink_metadata(&target)
            && (meta.file_type().is_symlink() || !meta.is_file())
        {
            return Err(format!(
                "{} is not a regular archive file",
                target.display()
            ));
        }
    }
    Ok(())
}

fn read_commit_receipt(tx: &Path, manifest_bytes: &[u8]) -> Result<bool, String> {
    let path = tx.join(COMMITTED_NAME);
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return Ok(false);
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 4096 {
        return Err(format!("invalid prune commit marker {}", path.display()));
    }
    let receipt: CommitReceipt =
        serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("invalid prune commit marker: {error}"))?;
    if receipt.schema != COMMIT_SCHEMA || !valid_digest(&receipt.manifest_sha256) {
        return Err("invalid prune commit marker schema".to_owned());
    }
    let mut hasher = Sha256::new();
    hasher.update(manifest_bytes);
    if hex::encode(hasher.finalize()) != receipt.manifest_sha256 {
        return Err("prune commit marker does not match manifest".to_owned());
    }
    Ok(true)
}

fn copy_restore(old: &Path, target: &Path, dir: &Path) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or_else(|| "archive target has no parent".to_owned())?;
    let temp = generated_temp(target, "restore")?;
    if let Ok(meta) = fs::symlink_metadata(&temp) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(format!("unsafe restore temporary {}", temp.display()));
        }
        fs::remove_file(&temp).map_err(|error| error.to_string())?;
    }
    fs::copy(old, &temp).map_err(|error| error.to_string())?;
    sync_file(&temp)?;
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(format!("unsafe archive target {}", target.display()));
        }
        fs::remove_file(target).map_err(|error| error.to_string())?;
    }
    fs::rename(&temp, target).map_err(|error| error.to_string())?;
    sync_dir(parent)?;
    remove_generated_temps(target)?;
    sync_dir(dir)
}

fn install_staged(
    staged: &Path,
    target: &Path,
    expected_sha256: &str,
    dir: &Path,
) -> Result<(), String> {
    if sha256_file(staged)? != expected_sha256 {
        return Err(format!(
            "staged replacement checksum changed {}",
            staged.display()
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| "archive target has no parent".to_owned())?;
    let temp = generated_temp(target, "prune")?;
    if let Ok(meta) = fs::symlink_metadata(&temp) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(format!("unsafe replacement temporary {}", temp.display()));
        }
        fs::remove_file(&temp).map_err(|error| error.to_string())?;
    }
    fs::copy(staged, &temp).map_err(|error| error.to_string())?;
    sync_file(&temp)?;
    if sha256_file(&temp)? != expected_sha256 {
        return Err(format!("replacement checksum mismatch {}", temp.display()));
    }
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(format!("unsafe archive target {}", target.display()));
        }
        fs::remove_file(target).map_err(|error| error.to_string())?;
    }
    fs::rename(&temp, target).map_err(|error| error.to_string())?;
    sync_dir(parent)?;
    sync_dir(dir)
}

fn verify_prepared_target(target: &Path, entry: &TransactionEntry) -> Result<(), String> {
    let Ok(metadata) = fs::symlink_metadata(target) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("unsafe archive target {}", target.display()));
    }
    let digest = sha256_file(target)?;
    if let Some(new) = entry.new_sha256.as_deref()
        && digest == new
    {
        return Ok(());
    }
    if let Some(old) = entry.old_sha256.as_deref()
        && digest == old
    {
        return Ok(());
    }
    Err(format!(
        "archive target changed outside transaction {}",
        target.display()
    ))
}

/// Recover a prepared transaction. The caller holds the archive's exclusive
/// lock. Read-only callers must never call this function.
pub(crate) fn recover_pending_transaction(dir: &Path) -> Result<(), String> {
    cleanup_gc(dir)?;
    let tx = transaction_dir(dir);
    let Ok(metadata) = fs::symlink_metadata(&tx) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("unsafe prune transaction path {}", tx.display()));
    }
    validate_tx_dir(&tx)?;
    let manifest_path = tx.join(MANIFEST_NAME);
    let Ok(_) = fs::symlink_metadata(&manifest_path) else {
        // No manifest means no live archive mutation was authorized. Clean up
        // staged bytes only after validating the transaction tree.
        if fs::symlink_metadata(tx.join(COMMITTED_NAME)).is_ok() {
            return Err("commit receipt exists without a prepared manifest".to_owned());
        }
        return cleanup_tx(&tx, dir);
    };
    let (manifest, bytes) = read_manifest(&tx)?;
    let committed = read_commit_receipt(&tx, &bytes)?;
    validate_manifest(dir, &tx, &manifest, committed)?;
    if committed {
        for entry in &manifest.entries {
            let target = validate_archive_target(dir, &entry.rel)?;
            remove_generated_temps(&target)?;
        }
        return cleanup_tx(&tx, dir);
    }
    for entry in &manifest.entries {
        let target = validate_archive_target(dir, &entry.rel)?;
        verify_prepared_target(&target, entry)?;
    }
    for entry in manifest.entries.iter().rev() {
        let target = validate_archive_target(dir, &entry.rel)?;
        if entry.existed {
            copy_restore(&tx.join(&entry.old), &target, dir)?;
        } else if let Ok(meta) = fs::symlink_metadata(&target) {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(format!("unsafe archive target {}", target.display()));
            }
            fs::remove_file(&target).map_err(|error| error.to_string())?;
            if let Some(parent) = target.parent() {
                sync_dir(parent)?;
            }
            remove_generated_temps(&target)?;
        } else {
            remove_generated_temps(&target)?;
        }
    }
    sync_dir(dir)?;
    cleanup_tx(&tx, dir)
}

pub(crate) fn has_pending_transaction(dir: &Path) -> bool {
    fs::symlink_metadata(transaction_dir(dir)).is_ok()
}

fn prepare_entries(
    dir: &Path,
    tx: &Path,
    grouped: &BTreeMap<PathBuf, Vec<String>>,
    target_paths: &BTreeSet<PathBuf>,
) -> Result<Vec<TransactionEntry>, String> {
    let mut entries = Vec::new();
    let mut transaction_bytes = 0u64;
    for (index, path) in target_paths.iter().enumerate() {
        let rel = path
            .strip_prefix(dir)
            .map_err(|_| format!("archive path escapes {}", dir.display()))?
            .to_str()
            .ok_or_else(|| "archive path is not valid UTF-8".to_owned())?
            .to_owned();
        let target = validate_archive_target(dir, &rel)?;
        let existed = fs::symlink_metadata(&target).is_ok();
        let old_name = format!("old/{index}");
        let old_sha256 = if existed {
            let metadata = fs::symlink_metadata(&target).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!(
                    "{} is not a regular archive file",
                    target.display()
                ));
            }
            let old = tx.join(&old_name);
            fs::copy(&target, &old).map_err(|error| error.to_string())?;
            sync_file(&old)?;
            sync_dir(&tx.join("old"))?;
            add_transaction_bytes(&mut transaction_bytes, &old)?;
            Some(sha256_file(&old)?)
        } else {
            None
        };
        let lines = grouped.get(path).map_or(&[] as &[String], Vec::as_slice);
        let (new_name, new_sha256) = if lines.is_empty() {
            (None, None)
        } else {
            let name = format!("new/{index}");
            let digest = stage_file(&tx.join(&name), lines)?;
            add_transaction_bytes(&mut transaction_bytes, &tx.join(&name))?;
            (Some(name), Some(digest))
        };
        entries.push(TransactionEntry {
            rel,
            old: old_name,
            old_sha256,
            new: new_name,
            new_sha256,
            existed,
        });
    }
    Ok(entries)
}

fn install_entries(dir: &Path, tx: &Path, entries: &[TransactionEntry]) -> Result<(), String> {
    for entry in entries {
        let target = validate_archive_target(dir, &entry.rel)?;
        if let Ok(meta) = fs::symlink_metadata(&target) {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(format!("unsafe archive target {}", target.display()));
            }
            fs::remove_file(&target).map_err(|error| error.to_string())?;
        }
        if let Some(new) = &entry.new {
            let staged = tx.join(new);
            // Keep the staged bytes in the transaction. A second crash during
            // rollback can then still validate the manifest and distinguish
            // the old target from the intended replacement.
            let expected = entry
                .new_sha256
                .as_deref()
                .ok_or_else(|| "transaction entry lacks staged checksum".to_owned())?;
            install_staged(&staged, &target, expected, dir)?;
        }
        if let Some(parent) = target.parent() {
            sync_dir(parent)?;
        }
    }
    sync_dir(dir)
}

pub(crate) fn apply_transaction(
    dir: &Path,
    grouped: &BTreeMap<PathBuf, Vec<String>>,
    target_paths: &BTreeSet<PathBuf>,
) -> Result<(), String> {
    if target_paths.is_empty() {
        return Ok(());
    }
    if target_paths.len() > MAX_ENTRIES {
        return Err(format!(
            "prune transaction has {} targets; maximum is {MAX_ENTRIES}",
            target_paths.len()
        ));
    }
    let tx = transaction_dir(dir);
    let dir_metadata = fs::symlink_metadata(dir).map_err(|error| error.to_string())?;
    if dir_metadata.file_type().is_symlink() || !dir_metadata.is_dir() {
        return Err(format!(
            "{} is not a regular archive directory",
            dir.display()
        ));
    }
    if fs::symlink_metadata(&tx).is_ok() {
        return Err(format!(
            "stale prune transaction exists at {}",
            tx.display()
        ));
    }
    fs::create_dir(&tx).map_err(|error| error.to_string())?;
    fs::create_dir(tx.join("old")).map_err(|error| error.to_string())?;
    fs::create_dir(tx.join("new")).map_err(|error| error.to_string())?;
    sync_dir(&tx)?;
    let mut prepared = false;
    let result = (|| {
        let entries = prepare_entries(dir, &tx, grouped, target_paths)?;
        let manifest = TransactionManifest {
            schema: TX_SCHEMA.to_owned(),
            state: "prepared".to_owned(),
            entries,
        };
        let bytes = serde_json::to_vec(&manifest).map_err(|error| error.to_string())?;
        let mut manifest_bytes = bytes;
        manifest_bytes.push(b'\n');
        if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err("prune transaction manifest exceeds safety limit".to_owned());
        }
        write_atomic(&tx.join(MANIFEST_NAME), &manifest_bytes)?;
        sync_dir(&tx)?;
        sync_dir(dir)?;
        prepared = true;

        install_entries(dir, &tx, &manifest.entries)?;
        let mut hasher = Sha256::new();
        hasher.update(&manifest_bytes);
        let receipt = CommitReceipt {
            schema: COMMIT_SCHEMA.to_owned(),
            manifest_sha256: hex::encode(hasher.finalize()),
        };
        let mut receipt_bytes = serde_json::to_vec(&receipt).map_err(|error| error.to_string())?;
        receipt_bytes.push(b'\n');
        write_atomic(&tx.join(COMMITTED_NAME), &receipt_bytes)?;
        cleanup_tx(&tx, dir)
    })();
    if result.is_err() && !prepared {
        let _ = cleanup_tx(&tx, dir);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn safe_relative_rejects_parent_and_absolute_paths() {
        assert!(safe_relative("causes/2026-10.jsonl"));
        assert!(!safe_relative("../outside"));
        assert!(!safe_relative("/outside"));
    }

    #[test]
    fn interrupted_preparation_is_cleaned_without_touching_archive() {
        let root = tempdir().unwrap();
        let tx = transaction_dir(root.path());
        fs::create_dir_all(tx.join("old")).unwrap();
        fs::create_dir_all(tx.join("new")).unwrap();
        fs::write(root.path().join("causes.jsonl"), b"keep").unwrap();
        recover_pending_transaction(root.path()).unwrap();
        assert!(!tx.exists());
        assert_eq!(fs::read(root.path().join("causes.jsonl")).unwrap(), b"keep");
    }

    #[test]
    fn transaction_round_trip_replaces_and_cleans() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        let target = root.path().join("causes/2026-10.jsonl");
        fs::write(&target, b"old\n").unwrap();
        let mut grouped = BTreeMap::new();
        grouped.insert(target.clone(), vec!["new\n".to_owned()]);
        let targets = [target.clone()].into_iter().collect();
        apply_transaction(root.path(), &grouped, &targets).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new\n");
        assert!(!transaction_dir(root.path()).exists());
    }

    #[test]
    fn recovery_restores_after_staged_rename_before_commit() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        let target = root.path().join("causes/2026-10.jsonl");
        fs::write(&target, b"new\n").unwrap();
        let tx = transaction_dir(root.path());
        fs::create_dir_all(tx.join("old")).unwrap();
        fs::create_dir_all(tx.join("new")).unwrap();
        fs::write(tx.join("old/0"), b"old\n").unwrap();
        let manifest = TransactionManifest {
            schema: TX_SCHEMA.to_owned(),
            state: "prepared".to_owned(),
            entries: vec![TransactionEntry {
                rel: "causes/2026-10.jsonl".to_owned(),
                old: "old/0".to_owned(),
                old_sha256: Some(sha256_file(&tx.join("old/0")).unwrap()),
                new: Some("new/0".to_owned()),
                new_sha256: Some(sha256_file(&target).unwrap()),
                existed: true,
            }],
        };
        let mut bytes = serde_json::to_vec(&manifest).unwrap();
        bytes.push(b'\n');
        write_atomic(&tx.join(MANIFEST_NAME), &bytes).unwrap();
        recover_pending_transaction(root.path()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old\n");
        assert!(!tx.exists());
    }

    #[test]
    fn committed_receipt_recovery_preserves_the_published_replacement() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        let target = root.path().join("causes/2026-10.jsonl");
        fs::write(&target, b"new\n").unwrap();
        let tx = transaction_dir(root.path());
        fs::create_dir_all(tx.join("old")).unwrap();
        fs::create_dir_all(tx.join("new")).unwrap();
        fs::write(tx.join("old/0"), b"old\n").unwrap();
        let new_sha256 = sha256_file(&target).unwrap();
        let manifest = TransactionManifest {
            schema: TX_SCHEMA.to_owned(),
            state: "prepared".to_owned(),
            entries: vec![TransactionEntry {
                rel: "causes/2026-10.jsonl".to_owned(),
                old: "old/0".to_owned(),
                old_sha256: Some(sha256_file(&tx.join("old/0")).unwrap()),
                new: Some("new/0".to_owned()),
                new_sha256: Some(new_sha256),
                existed: true,
            }],
        };
        let mut manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        manifest_bytes.push(b'\n');
        write_atomic(&tx.join(MANIFEST_NAME), &manifest_bytes).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(&manifest_bytes);
        let receipt = CommitReceipt {
            schema: COMMIT_SCHEMA.to_owned(),
            manifest_sha256: hex::encode(hasher.finalize()),
        };
        let mut receipt_bytes = serde_json::to_vec(&receipt).unwrap();
        receipt_bytes.push(b'\n');
        write_atomic(&tx.join(COMMITTED_NAME), &receipt_bytes).unwrap();

        recover_pending_transaction(root.path()).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new\n");
        assert!(!tx.exists());
    }

    #[test]
    fn recovery_refuses_an_externally_modified_live_target() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        let target = root.path().join("causes/2026-10.jsonl");
        fs::write(&target, b"external\n").unwrap();
        let tx = transaction_dir(root.path());
        fs::create_dir_all(tx.join("old")).unwrap();
        fs::create_dir_all(tx.join("new")).unwrap();
        fs::write(tx.join("old/0"), b"old\n").unwrap();
        fs::write(tx.join("new/0"), b"new\n").unwrap();
        let manifest = TransactionManifest {
            schema: TX_SCHEMA.to_owned(),
            state: "prepared".to_owned(),
            entries: vec![TransactionEntry {
                rel: "causes/2026-10.jsonl".to_owned(),
                old: "old/0".to_owned(),
                old_sha256: Some(sha256_file(&tx.join("old/0")).unwrap()),
                new: Some("new/0".to_owned()),
                new_sha256: Some(sha256_file(&tx.join("new/0")).unwrap()),
                existed: true,
            }],
        };
        let mut bytes = serde_json::to_vec(&manifest).unwrap();
        bytes.push(b'\n');
        write_atomic(&tx.join(MANIFEST_NAME), &bytes).unwrap();

        let error = recover_pending_transaction(root.path()).unwrap_err();

        assert!(error.contains("changed outside transaction"), "{error}");
        assert_eq!(fs::read(&target).unwrap(), b"external\n");
        assert!(tx.exists());
    }

    #[test]
    fn apply_rejects_more_targets_than_the_transaction_entry_cap() {
        let root = tempdir().unwrap();
        let targets: BTreeSet<PathBuf> = (0..=MAX_ENTRIES)
            .map(|index| root.path().join(format!("causes/{index}.jsonl")))
            .collect();
        let error = apply_transaction(root.path(), &BTreeMap::new(), &targets).unwrap_err();
        assert!(error.contains("maximum"), "{error}");
        assert!(!transaction_dir(root.path()).exists());
    }

    #[test]
    fn recovery_rejects_manifest_target_outside_archive() {
        let root = tempdir().unwrap();
        let tx = transaction_dir(root.path());
        fs::create_dir_all(tx.join("old")).unwrap();
        fs::create_dir_all(tx.join("new")).unwrap();
        fs::write(tx.join("old/0"), b"old\n").unwrap();
        let manifest = TransactionManifest {
            schema: TX_SCHEMA.to_owned(),
            state: "prepared".to_owned(),
            entries: vec![TransactionEntry {
                rel: "../outside.jsonl".to_owned(),
                old: "old/0".to_owned(),
                old_sha256: Some(sha256_file(&tx.join("old/0")).unwrap()),
                new: None,
                new_sha256: None,
                existed: true,
            }],
        };
        let mut bytes = serde_json::to_vec(&manifest).unwrap();
        bytes.push(b'\n');
        write_atomic(&tx.join(MANIFEST_NAME), &bytes).unwrap();
        assert!(recover_pending_transaction(root.path()).is_err());
        assert!(tx.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_transaction_directory_is_refused() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), transaction_dir(root.path())).unwrap();
        assert!(recover_pending_transaction(root.path()).is_err());
    }

    #[test]
    fn bad_backup_is_rejected_before_any_target_is_changed() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        let target = root.path().join("causes/2026-10.jsonl");
        fs::write(&target, b"new\n").unwrap();
        let tx = transaction_dir(root.path());
        fs::create_dir_all(tx.join("old")).unwrap();
        fs::create_dir_all(tx.join("new")).unwrap();
        fs::write(tx.join("old/0"), b"corrupt\n").unwrap();
        fs::write(tx.join("new/0"), b"new\n").unwrap();
        let manifest = TransactionManifest {
            schema: TX_SCHEMA.to_owned(),
            state: "prepared".to_owned(),
            entries: vec![TransactionEntry {
                rel: "causes/2026-10.jsonl".to_owned(),
                old: "old/0".to_owned(),
                old_sha256: Some("0".repeat(64)),
                new: Some("new/0".to_owned()),
                new_sha256: Some(sha256_file(&tx.join("new/0")).unwrap()),
                existed: true,
            }],
        };
        let mut bytes = serde_json::to_vec(&manifest).unwrap();
        bytes.push(b'\n');
        write_atomic(&tx.join(MANIFEST_NAME), &bytes).unwrap();
        assert!(recover_pending_transaction(root.path()).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"new\n");
    }

    #[test]
    fn repeated_recovery_after_rollback_is_idempotent() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        let target = root.path().join("causes/2026-10.jsonl");
        fs::write(&target, b"new\n").unwrap();
        let tx = transaction_dir(root.path());
        fs::create_dir_all(tx.join("old")).unwrap();
        fs::create_dir_all(tx.join("new")).unwrap();
        fs::write(tx.join("old/0"), b"old\n").unwrap();
        fs::write(tx.join("new/0"), b"new\n").unwrap();
        let manifest = TransactionManifest {
            schema: TX_SCHEMA.to_owned(),
            state: "prepared".to_owned(),
            entries: vec![TransactionEntry {
                rel: "causes/2026-10.jsonl".to_owned(),
                old: "old/0".to_owned(),
                old_sha256: Some(sha256_file(&tx.join("old/0")).unwrap()),
                new: Some("new/0".to_owned()),
                new_sha256: Some(sha256_file(&tx.join("new/0")).unwrap()),
                existed: true,
            }],
        };
        let mut bytes = serde_json::to_vec(&manifest).unwrap();
        bytes.push(b'\n');
        write_atomic(&tx.join(MANIFEST_NAME), &bytes).unwrap();
        recover_pending_transaction(root.path()).unwrap();
        recover_pending_transaction(root.path()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old\n");
        assert!(!tx.exists());
    }

    #[test]
    fn gc_cleanup_never_restores_or_mutates_archive() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        let target = root.path().join("causes/2026-10.jsonl");
        fs::write(&target, b"current\n").unwrap();
        let gc = gc_dir(root.path());
        fs::create_dir_all(gc.join("old")).unwrap();
        fs::create_dir_all(gc.join("new")).unwrap();
        fs::write(gc.join("old/0"), b"stale\n").unwrap();
        recover_pending_transaction(root.path()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"current\n");
        assert!(!gc.exists());
    }

    #[test]
    fn rollback_cleans_orphaned_live_replacement_temporaries() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("causes")).unwrap();
        let target = root.path().join("causes/2026-10.jsonl");
        fs::write(&target, b"new\n").unwrap();
        fs::write(generated_temp(&target, "prune").unwrap(), b"new\n").unwrap();
        fs::write(generated_temp(&target, "restore").unwrap(), b"old\n").unwrap();
        let tx = transaction_dir(root.path());
        fs::create_dir_all(tx.join("old")).unwrap();
        fs::create_dir_all(tx.join("new")).unwrap();
        fs::write(tx.join("old/0"), b"old\n").unwrap();
        fs::write(tx.join("new/0"), b"new\n").unwrap();
        let manifest = TransactionManifest {
            schema: TX_SCHEMA.to_owned(),
            state: "prepared".to_owned(),
            entries: vec![TransactionEntry {
                rel: "causes/2026-10.jsonl".to_owned(),
                old: "old/0".to_owned(),
                old_sha256: Some(sha256_file(&tx.join("old/0")).unwrap()),
                new: Some("new/0".to_owned()),
                new_sha256: Some(sha256_file(&tx.join("new/0")).unwrap()),
                existed: true,
            }],
        };
        let mut bytes = serde_json::to_vec(&manifest).unwrap();
        bytes.push(b'\n');
        write_atomic(&tx.join(MANIFEST_NAME), &bytes).unwrap();
        recover_pending_transaction(root.path()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old\n");
        assert!(fs::symlink_metadata(generated_temp(&target, "prune").unwrap()).is_err());
        assert!(fs::symlink_metadata(generated_temp(&target, "restore").unwrap()).is_err());
    }
}
