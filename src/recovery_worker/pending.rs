//! Bounded pending enumeration and fair pre-claim deferral.

use super::{
    MAX_DETAIL_BYTES, MAX_PENDING_LIMIT, RecoveryError, RecoveryRecord, RecoveryResult,
    RecoveryStatus, RecoveryStore, Utc, fs, validate_id, validate_record, validate_signature,
    validate_text,
};

impl RecoveryStore {
    /// Return pending requests in deterministic scheduling order.
    pub fn pending(&self, limit: usize) -> RecoveryResult<Vec<RecoveryRecord>> {
        validate_pending_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let _lock = self.lock()?;
        self.pending_unlocked(limit)
    }

    /// Return a read-only pending snapshot without creating or writing a lock.
    ///
    /// When the store lock already exists this takes a shared lock, so it cannot
    /// observe the in-place attempt fence mid-write. A missing lock linearizes
    /// as an empty snapshot; records without their expected lock fail closed.
    pub fn pending_read_only(&self, limit: usize) -> RecoveryResult<Vec<RecoveryRecord>> {
        validate_pending_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(_lock) = self.read_lock_if_present()? else {
            if self.has_record_files_unlocked()? {
                return Err(RecoveryError::InvalidRequest(
                    "recovery records exist without the durable store lock".to_owned(),
                ));
            }
            return Ok(Vec::new());
        };
        self.pending_unlocked(limit)
    }

    fn pending_unlocked(&self, limit: usize) -> RecoveryResult<Vec<RecoveryRecord>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| {
                    RecoveryError::InvalidRequest(format!(
                        "recovery record path is not UTF-8: {}",
                        path.display()
                    ))
                })?;
            validate_id(id)?;
            let record = self
                .load_unlocked(id)?
                .ok_or_else(|| RecoveryError::NotFound(id.to_owned()))?;
            if record.receipt.status == RecoveryStatus::Pending {
                records.push(record);
            }
        }
        records.sort_by(|left, right| {
            left.receipt
                .deferred_at
                .cmp(&right.receipt.deferred_at)
                .then(left.request.created_at.cmp(&right.request.created_at))
                .then(left.request.id.cmp(&right.request.id))
        });
        records.truncate(limit);
        Ok(records)
    }

    /// Move one exact pending record behind untouched work without spending an attempt.
    ///
    /// The expected configuration fences a stale worker from deferring a record
    /// that was reactivated under newer machine policy. Deferral timestamps are
    /// advanced past every existing deferred record under the store lock, so
    /// repeated failures rotate fairly even when the wall clock has coarse
    /// resolution.
    pub(crate) fn defer_pending(
        &self,
        id: &str,
        expected_config_signature: &str,
        detail: impl Into<String>,
    ) -> RecoveryResult<bool> {
        validate_id(id)?;
        validate_signature("config_signature", expected_config_signature)?;
        let detail = detail.into();
        validate_text("deferral detail", &detail, 1, MAX_DETAIL_BYTES)?;
        let _lock = self.lock()?;
        let mut record = self
            .load_unlocked(id)?
            .ok_or_else(|| RecoveryError::NotFound(id.to_owned()))?;
        if record.receipt.status != RecoveryStatus::Pending
            || record.request.config_signature != expected_config_signature
            || record.receipt.config_signature != expected_config_signature
        {
            return Ok(false);
        }
        let latest_deferral = self
            .record_ids_unlocked()?
            .into_iter()
            .map(|record_id| self.load_unlocked(&record_id))
            .collect::<RecoveryResult<Vec<_>>>()?
            .into_iter()
            .flatten()
            .filter(|candidate| candidate.receipt.status == RecoveryStatus::Pending)
            .filter_map(|candidate| candidate.receipt.deferred_at)
            .max();
        let now = Utc::now();
        let deferred_at = match latest_deferral {
            Some(latest) => latest
                .checked_add_signed(chrono::Duration::nanoseconds(1))
                .ok_or_else(|| {
                    RecoveryError::InvalidRequest(
                        "pending deferral timestamp cannot advance".to_owned(),
                    )
                })?
                .max(now),
            None => now,
        };
        record.receipt.deferred_at = Some(deferred_at);
        record.receipt.updated_at = deferred_at;
        record.receipt.detail = Some(detail);
        validate_record(&record)?;
        self.save_unlocked(&record)?;
        Ok(true)
    }
}

fn validate_pending_limit(limit: usize) -> RecoveryResult<()> {
    if limit > MAX_PENDING_LIMIT {
        Err(RecoveryError::InvalidRequest(format!(
            "pending limit must not exceed {MAX_PENDING_LIMIT}"
        )))
    } else {
        Ok(())
    }
}
