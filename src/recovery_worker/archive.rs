use super::{
    RECOVERY_SCHEMA_VERSION, RecoveryError, RecoveryRecord, RecoveryRequest, RecoveryResult,
    RecoveryStore, validate_id, validate_record,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_ACTIVE_RECORDS: usize = 1_024;
const ARCHIVE_DIR: &str = "archive";
const HEAD_INDEX_DIR: &str = "head-index";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DurableHeadOwner {
    schema_version: u32,
    repo: String,
    pr: u64,
    head_sha: String,
    request_id: String,
}

impl RecoveryStore {
    /// Load one exact active or cold archived record without enumerating either
    /// directory. A terminal archive wins over a crash-left active copy.
    pub(super) fn load_record_unlocked(&self, id: &str) -> RecoveryResult<Option<RecoveryRecord>> {
        for path in [self.archived_record_path(id), self.record_path(id)] {
            let payload = match fs::read(&path) {
                Ok(payload) => payload,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let record = serde_json::from_slice::<RecoveryRecord>(&payload)?;
            validate_record(&record)?;
            if record.request.id != id {
                return Err(RecoveryError::IdentityCollision(id.to_owned()));
            }
            return Ok(Some(record));
        }
        Ok(None)
    }

    /// Resolve the sole durable owner of an exact repository/PR/head without
    /// scanning historical terminal receipts.
    pub(super) fn load_head_owner_unlocked(
        &self,
        request: &RecoveryRequest,
    ) -> RecoveryResult<Option<RecoveryRecord>> {
        let path = self.head_owner_path(request);
        let payload = match fs::read(&path) {
            Ok(payload) => payload,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let owner = serde_json::from_slice::<DurableHeadOwner>(&payload)?;
        if owner.schema_version != RECOVERY_SCHEMA_VERSION {
            return Err(RecoveryError::SchemaVersion {
                surface: "head owner",
                observed: owner.schema_version,
            });
        }
        validate_id(&owner.request_id)?;
        if owner.repo != request.repo
            || owner.pr != request.pr
            || owner.head_sha != request.head_sha
        {
            return Err(RecoveryError::InvalidRequest(format!(
                "recovery head-owner index {} does not match {}/#{} at {}",
                path.display(),
                request.repo,
                request.pr,
                request.head_sha
            )));
        }
        let record = self
            .load_unlocked(&owner.request_id)?
            .ok_or_else(|| RecoveryError::NotFound(owner.request_id.clone()))?;
        if record.request.repo != request.repo
            || record.request.pr != request.pr
            || record.request.head_sha != request.head_sha
        {
            return Err(RecoveryError::IdentityCollision(owner.request_id));
        }
        Ok(Some(record))
    }

    /// Persist an active record in the bounded hot set or a terminal record in
    /// the sharded cold archive. The per-head owner is written before the old
    /// location or claim is removed, so interruption fails closed.
    pub(super) fn save_unlocked(&self, record: &RecoveryRecord) -> RecoveryResult<()> {
        validate_record(record)?;
        let active_path = self.record_path(&record.request.id);
        let archived_path = self.archived_record_path(&record.request.id);
        if !record.receipt.status.is_terminal()
            && !active_path.exists()
            && self.record_ids_unlocked()?.len() >= MAX_ACTIVE_RECORDS
        {
            return Err(RecoveryError::InvalidRequest(format!(
                "recovery active-record limit of {MAX_ACTIVE_RECORDS} is exhausted"
            )));
        }

        let destination = if record.receipt.status.is_terminal() {
            &archived_path
        } else {
            &active_path
        };
        persist_json(destination, record)?;
        self.persist_head_owner_unlocked(record)?;

        if record.receipt.status.is_terminal() {
            remove_if_exists(&active_path)?;
            remove_if_exists(&self.claim_path(&record.request.id))?;
        } else {
            remove_if_exists(&archived_path)?;
        }
        sync_directory(&self.root)?;
        Ok(())
    }

    pub(super) fn persist_head_owner_unlocked(
        &self,
        record: &RecoveryRecord,
    ) -> RecoveryResult<()> {
        persist_json(
            &self.head_owner_path(&record.request),
            &DurableHeadOwner {
                schema_version: RECOVERY_SCHEMA_VERSION,
                repo: record.request.repo.clone(),
                pr: record.request.pr,
                head_sha: record.request.head_sha.clone(),
                request_id: record.request.id.clone(),
            },
        )
    }

    pub(super) fn archived_record_path(&self, id: &str) -> PathBuf {
        self.root
            .join(ARCHIVE_DIR)
            .join(&id[..2])
            .join(format!("{id}.json"))
    }

    fn head_owner_path(&self, request: &RecoveryRequest) -> PathBuf {
        let mut hasher = Sha256::new();
        let pr = request.pr.to_string();
        for value in [
            request.repo.as_bytes(),
            pr.as_bytes(),
            request.head_sha.as_bytes(),
        ] {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value);
        }
        let key = format!("{:x}", hasher.finalize());
        self.root
            .join(HEAD_INDEX_DIR)
            .join(&key[..2])
            .join(format!("{key}.json"))
    }
}

fn persist_json(path: &Path, value: &impl Serialize) -> RecoveryResult<()> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
    let parent = path.parent().ok_or_else(|| {
        RecoveryError::InvalidRequest(format!("durable path has no parent: {}", path.display()))
    })?;
    fs::create_dir_all(parent)?;
    let payload = serde_json::to_vec_pretty(value)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&payload)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| RecoveryError::Io(error.error))?;
    sync_directory(parent)
}

fn remove_if_exists(path: &Path) -> RecoveryResult<()> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> RecoveryResult<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
// Keep the fallible signature aligned with the durable Unix implementation.
#[allow(clippy::unnecessary_wraps)]
fn sync_directory(_path: &Path) -> RecoveryResult<()> {
    Ok(())
}
