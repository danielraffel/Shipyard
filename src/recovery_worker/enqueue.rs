use super::{
    EnqueueOutcome, RECOVERY_SCHEMA_VERSION, RecoveryError, RecoveryReceipt, RecoveryRecord,
    RecoveryRequest, RecoveryResult, RecoveryStatus, RecoveryStore, Utc, same_recovery_identity,
    validate_request,
};

impl RecoveryStore {
    /// Atomically create, reactivate, or deduplicate an exact recovery request.
    pub fn enqueue(&self, request: RecoveryRequest) -> RecoveryResult<EnqueueOutcome> {
        validate_request(&request)?;
        let _lock = self.lock()?;
        if let Some(existing) = self.load_unlocked(&request.id)? {
            return self.enqueue_existing_unlocked(existing, request);
        }

        let same_head = self.same_head_records_unlocked(&request)?;
        if let Some(outcome) = self.spent_head_outcome_unlocked(&same_head, None)? {
            return Ok(outcome);
        }
        if let Some(existing) = Self::single_pending_sibling(&same_head, None, &request)? {
            self.supersede_pending_unlocked(
                existing.clone(),
                &request.id,
                "same-head deterministic evidence or steward policy changed before claim",
            )?;
        }
        self.persist_new_pending_unlocked(request)?;
        Ok(EnqueueOutcome::Created)
    }

    fn enqueue_existing_unlocked(
        &self,
        existing: RecoveryRecord,
        request: RecoveryRequest,
    ) -> RecoveryResult<EnqueueOutcome> {
        if !same_recovery_identity(&existing.request, &request) {
            return Err(RecoveryError::IdentityCollision(request.id));
        }
        let config_changed = existing.request.config_signature != request.config_signature;
        if existing.receipt.attempt > 0 {
            if config_changed {
                return Err(RecoveryError::ConfigDrift {
                    expected: existing.request.config_signature,
                    observed: request.config_signature,
                });
            }
            return Ok(EnqueueOutcome::Existing);
        }
        if existing.receipt.status == RecoveryStatus::Pending && !config_changed {
            return Ok(EnqueueOutcome::Existing);
        }
        if existing.receipt.status == RecoveryStatus::Superseded {
            let same_head = self.same_head_records_unlocked(&request)?;
            if let Some(outcome) =
                self.spent_head_outcome_unlocked(&same_head, Some(&request.id))?
            {
                return Ok(outcome);
            }
            if let Some(sibling) =
                Self::single_pending_sibling(&same_head, Some(&request.id), &request)?
            {
                self.supersede_pending_unlocked(
                    sibling.clone(),
                    &request.id,
                    "same-head deterministic evidence returned to an earlier unattempted identity",
                )?;
            }
        } else if existing.receipt.status != RecoveryStatus::Pending {
            if config_changed {
                return Err(RecoveryError::ConfigDrift {
                    expected: existing.request.config_signature,
                    observed: request.config_signature,
                });
            }
            return Ok(EnqueueOutcome::Existing);
        }
        self.reactivate_unattempted_unlocked(existing, request)?;
        Ok(EnqueueOutcome::Created)
    }

    fn spent_head_outcome_unlocked(
        &self,
        records: &[RecoveryRecord],
        excluded_id: Option<&str>,
    ) -> RecoveryResult<Option<EnqueueOutcome>> {
        let Some(mut spent) = records
            .iter()
            .find(|record| {
                excluded_id.is_none_or(|id| record.request.id != id) && record.receipt.attempt > 0
            })
            .cloned()
        else {
            return Ok(None);
        };
        if spent.receipt.status == RecoveryStatus::Running {
            let now = Utc::now();
            spent.receipt.status = RecoveryStatus::Superseded;
            spent.receipt.superseded_by = None;
            spent.receipt.detail = Some(
                "same-head target, evidence, or steward policy changed after claim; attempt budget remains spent"
                    .to_owned(),
            );
            spent.receipt.completed_at = Some(now);
            spent.receipt.deferred_at = None;
            spent.receipt.updated_at = now;
            self.save_unlocked(&spent)?;
        }
        Ok(Some(EnqueueOutcome::HeadAlreadyTracked {
            existing_id: spent.request.id,
        }))
    }

    fn single_pending_sibling<'a>(
        records: &'a [RecoveryRecord],
        excluded_id: Option<&str>,
        request: &RecoveryRequest,
    ) -> RecoveryResult<Option<&'a RecoveryRecord>> {
        let pending = records
            .iter()
            .filter(|record| {
                excluded_id.is_none_or(|id| record.request.id != id)
                    && record.receipt.status == RecoveryStatus::Pending
            })
            .collect::<Vec<_>>();
        if pending.len() > 1 {
            return Err(RecoveryError::InvalidRequest(format!(
                "multiple pending recovery records exist for {}/#{} at {}",
                request.repo, request.pr, request.head_sha
            )));
        }
        Ok(pending.first().copied())
    }

    fn supersede_pending_unlocked(
        &self,
        mut record: RecoveryRecord,
        successor_id: &str,
        detail: &str,
    ) -> RecoveryResult<()> {
        let now = Utc::now();
        record.receipt.status = RecoveryStatus::Superseded;
        record.receipt.superseded_by = Some(successor_id.to_owned());
        record.receipt.detail = Some(detail.to_owned());
        record.receipt.completed_at = Some(now);
        record.receipt.deferred_at = None;
        record.receipt.updated_at = now;
        self.save_unlocked(&record)
    }

    fn reactivate_unattempted_unlocked(
        &self,
        mut existing: RecoveryRecord,
        request: RecoveryRequest,
    ) -> RecoveryResult<()> {
        let now = Utc::now();
        existing.request = request;
        existing.receipt.status = RecoveryStatus::Pending;
        existing.receipt.config_signature = existing.request.config_signature.clone();
        existing.receipt.max_attempts = self.max_attempts;
        existing.receipt.worker_generation = None;
        existing.receipt.started_at = None;
        existing.receipt.completed_at = None;
        existing.receipt.deferred_at = None;
        existing.receipt.updated_at = now;
        existing.receipt.superseded_by = None;
        existing.receipt.detail = None;
        existing.receipt.output = None;
        self.save_unlocked(&existing)
    }

    fn persist_new_pending_unlocked(&self, request: RecoveryRequest) -> RecoveryResult<()> {
        let now = Utc::now();
        self.save_unlocked(&RecoveryRecord {
            schema_version: RECOVERY_SCHEMA_VERSION,
            receipt: RecoveryReceipt {
                schema_version: RECOVERY_SCHEMA_VERSION,
                request_id: request.id.clone(),
                status: RecoveryStatus::Pending,
                attempt: 0,
                max_attempts: self.max_attempts,
                config_signature: request.config_signature.clone(),
                worker_generation: None,
                started_at: None,
                completed_at: None,
                deferred_at: None,
                updated_at: now,
                superseded_by: None,
                detail: None,
                output: None,
            },
            request,
        })
    }
}
