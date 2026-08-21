use super::{
    DateTime, MAX_GENERATION_BYTES, RECOVERY_SCHEMA_VERSION, RecoveryError, RecoveryRecord,
    RecoveryResult, RecoveryStatus, RecoveryStore, Utc, Write, fs, validate_id, validate_signature,
    validate_text,
};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::fs::File;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DurableRecoveryClaim {
    schema_version: u32,
    request_id: String,
    config_signature: String,
    worker_generation: String,
    attempt: u32,
    started_at: DateTime<Utc>,
}

impl RecoveryStore {
    pub(super) fn persist_claim_unlocked(&self, record: &RecoveryRecord) -> RecoveryResult<()> {
        let claim = claim_from_running_record(record)?;
        let payload = serde_json::to_vec_pretty(&claim)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        temporary.write_all(&payload)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(self.claim_path(&record.request.id))
            .map_err(|error| RecoveryError::Io(error.error))?;
        sync_claim_directory(&self.root)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn persist_claim_for_test(&self, record: &RecoveryRecord) -> RecoveryResult<()> {
        self.persist_claim_unlocked(record)
    }

    pub(super) fn apply_claim_if_present_unlocked(
        &self,
        record: &mut RecoveryRecord,
    ) -> RecoveryResult<()> {
        if record.receipt.status != RecoveryStatus::Pending {
            return Ok(());
        }
        let payload = match fs::read(self.claim_path(&record.request.id)) {
            Ok(payload) => payload,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let claim = serde_json::from_slice::<DurableRecoveryClaim>(&payload)?;
        validate_claim(&claim, record)?;
        record.receipt.status = RecoveryStatus::Running;
        record.receipt.attempt = claim.attempt;
        record.receipt.worker_generation = Some(claim.worker_generation);
        record.receipt.started_at = Some(claim.started_at);
        record.receipt.deferred_at = None;
        record.receipt.updated_at = claim.started_at;
        record.receipt.detail = None;
        Ok(())
    }

    pub(super) fn claim_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.claim"))
    }
}

fn claim_from_running_record(record: &RecoveryRecord) -> RecoveryResult<DurableRecoveryClaim> {
    if record.receipt.status != RecoveryStatus::Running || record.receipt.attempt == 0 {
        return Err(RecoveryError::InvalidRequest(
            "durable claim requires an attempt-consuming running record".to_owned(),
        ));
    }
    let worker_generation =
        record.receipt.worker_generation.clone().ok_or_else(|| {
            RecoveryError::InvalidRequest("running claim omitted worker".to_owned())
        })?;
    let started_at = record.receipt.started_at.ok_or_else(|| {
        RecoveryError::InvalidRequest("running claim omitted start time".to_owned())
    })?;
    Ok(DurableRecoveryClaim {
        schema_version: RECOVERY_SCHEMA_VERSION,
        request_id: record.request.id.clone(),
        config_signature: record.request.config_signature.clone(),
        worker_generation,
        attempt: record.receipt.attempt,
        started_at,
    })
}

fn validate_claim(claim: &DurableRecoveryClaim, record: &RecoveryRecord) -> RecoveryResult<()> {
    if claim.schema_version != RECOVERY_SCHEMA_VERSION {
        return Err(RecoveryError::SchemaVersion {
            surface: "claim",
            observed: claim.schema_version,
        });
    }
    validate_id(&claim.request_id)?;
    validate_signature("claim config_signature", &claim.config_signature)?;
    validate_text(
        "claim worker_generation",
        &claim.worker_generation,
        1,
        MAX_GENERATION_BYTES,
    )?;
    if claim.request_id != record.request.id {
        return Err(RecoveryError::IdentityCollision(claim.request_id.clone()));
    }
    if claim.config_signature != record.request.config_signature {
        return Err(RecoveryError::ConfigDrift {
            expected: record.request.config_signature.clone(),
            observed: claim.config_signature.clone(),
        });
    }
    if claim.attempt == 0 || claim.attempt > record.receipt.max_attempts {
        return Err(RecoveryError::InvalidRequest(format!(
            "durable claim attempt {} exceeds request budget {}",
            claim.attempt, record.receipt.max_attempts
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn sync_claim_directory(path: &std::path::Path) -> RecoveryResult<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
// Keep the fallible signature aligned with the durable Unix implementation.
#[allow(clippy::unnecessary_wraps)]
fn sync_claim_directory(_path: &std::path::Path) -> RecoveryResult<()> {
    Ok(())
}
