/// Crash-consistent immutable custody store for approved canary jobs.
#[derive(Clone, Debug)]
pub struct CanaryJobStore {
    records: ImmutableByteStore,
    inputs: ImmutableByteStore,
    artifacts: ImmutableByteStore,
    logs: ImmutableByteStore,
}

impl CanaryJobStore {
    /// Open a private controller-owned job root.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ParallelProofError> {
        let root = root.into();
        crate::writer_domain_lease::ensure_protected_dir_all(&root)
            .map_err(ParallelProofError::Io)?;
        Ok(Self {
            records: ImmutableByteStore::open(root.join("records"), MAX_RECORD_BYTES)
                .map_err(map_store_error)?,
            inputs: ImmutableByteStore::open(root.join("inputs"), MAX_INPUT_BYTES)
                .map_err(map_store_error)?,
            artifacts: ImmutableByteStore::open(root.join("artifacts"), MAX_RECORD_BYTES)
                .map_err(map_store_error)?,
            logs: ImmutableByteStore::open(root.join("logs"), MAX_RECORD_BYTES)
                .map_err(map_store_error)?,
        })
    }

    /// Load one snapshot without creating, migrating, reconciling, or deleting
    /// storage. This is the only legal backing for a read-only status query.
    pub fn load_read_only(
        root: impl Into<PathBuf>,
        job_id: &str,
    ) -> Result<CanaryJobSnapshot, ParallelProofError> {
        let root = root.into();
        let records = ImmutableByteStore::open_read_only(root.join("records"), MAX_RECORD_BYTES)
            .map_err(map_store_error)?;
        let artifacts =
            ImmutableByteStore::open_read_only(root.join("artifacts"), MAX_RECORD_BYTES)
                .map_err(map_store_error)?;
        load_snapshot_from_stores(&records, &artifacts, job_id)
    }

    /// Persist the exact private invocation bytes before daemon admission.
    pub fn record_input(
        &self,
        job: &ApprovedCanaryJob,
        bytes: &[u8],
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        job.validate()?;
        let ApprovedCanaryOperation::ParallelProofDistributedShadow { request_sha256, .. } =
            &job.operation;
        if Sha256Digest::of_bytes(bytes) != *request_sha256 {
            return Err(ParallelProofError::BindingMismatch(
                "canary job request bytes",
            ));
        }
        self.inputs
            .put(&input_key(&job.job_id), bytes)
            .map_err(map_store_error)
    }

    /// Load the exact immutable invocation, rejecting envelope drift.
    pub fn load_input(&self, job: &ApprovedCanaryJob) -> Result<Vec<u8>, ParallelProofError> {
        job.validate()?;
        let bytes = self
            .inputs
            .load(&input_key(&job.job_id))
            .map_err(map_store_error)?;
        let ApprovedCanaryOperation::ParallelProofDistributedShadow { request_sha256, .. } =
            &job.operation;
        if Sha256Digest::of_bytes(&bytes) != *request_sha256 {
            return Err(ParallelProofError::BindingMismatch(
                "canary job request bytes",
            ));
        }
        Ok(bytes)
    }

    /// Persist immutable intent before any backend launch is legal.
    pub fn submit(&self, job: &ApprovedCanaryJob) -> Result<StoreWriteOutcome, ParallelProofError> {
        job.validate()?;
        let job_sha256 = job.digest()?;
        let envelope_outcome = self
            .records
            .put(&envelope_key(&job.job_id), &serde_json::to_vec(job)?)
            .map_err(map_store_error)?;
        let launch_nonce_sha256 = domain_digest(
            "shipyard.canary-job.launch-nonce.v1",
            &(job_sha256.clone(), &job.owner.controller_incarnation),
        )?;
        let prepared = CanaryJobReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            job_sha256,
            sequence: 0,
            previous_receipt_sha256: None,
            receipt: CanaryJobReceiptState::Prepared {
                launch_nonce_sha256,
            },
        };
        validate_receipt(job, &[], &prepared, &prepared.job_sha256)?;
        let receipt_outcome = self.put_receipt(&job.job_id, &prepared)?;
        Ok(
            if envelope_outcome == StoreWriteOutcome::Created
                || receipt_outcome == StoreWriteOutcome::Created
            {
                StoreWriteOutcome::Created
            } else {
                StoreWriteOutcome::AlreadyPresent
            },
        )
    }

    /// Load and validate the complete bounded immutable receipt chain.
    pub fn load(&self, job_id: &str) -> Result<CanaryJobSnapshot, ParallelProofError> {
        let snapshot = self.load_receipt_chain(job_id)?;
        if let CanaryJobReceiptState::Terminal {
            outcome: CanaryJobTerminalOutcome::Succeeded,
            artifact: Some(artifact),
            ..
        } = &snapshot.latest().receipt
            && !self.artifact_matches(job_id, artifact)?
        {
            return Err(ParallelProofError::CorruptRecord(
                "canary job success artifact".to_owned(),
            ));
        }
        Ok(snapshot)
    }

    /// Enumerate exact nonterminal job identifiers for one daemon tick.
    /// Receipt records share the immutable directory and are ignored only when
    /// they do not carry an envelope's `owner` and `operation` fields.
    pub fn pending_job_ids(&self) -> Result<Vec<String>, ParallelProofError> {
        let (job_ids, errors) = self.pending_job_scan()?;
        if let Some(error) = errors.into_iter().next() {
            return Err(ParallelProofError::CorruptRecord(error));
        }
        Ok(job_ids)
    }

    /// Enumerate valid pending jobs without allowing one corrupt record to
    /// starve unrelated custody. Callers must surface the returned warnings.
    pub(crate) fn pending_job_scan(
        &self,
    ) -> Result<(Vec<String>, Vec<String>), ParallelProofError> {
        let mut job_ids = Vec::new();
        let mut errors = Vec::new();
        for record in self
            .records
            .list_record_results()
            .map_err(map_store_error)?
        {
            let bytes = match record {
                Ok(bytes) => bytes,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            let value: serde_json::Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            if value.get("owner").is_none() || value.get("operation").is_none() {
                continue;
            }
            let job: ApprovedCanaryJob = match serde_json::from_value(value) {
                Ok(job) => job,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            if let Err(error) = job.validate() {
                errors.push(error.to_string());
                continue;
            }
            let snapshot = match self.load(&job.job_id) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            if !snapshot.is_terminal() {
                job_ids.push(job.job_id);
            }
        }
        job_ids.sort();
        job_ids.dedup();
        Ok((job_ids, errors))
    }

    fn load_receipt_chain(&self, job_id: &str) -> Result<CanaryJobSnapshot, ParallelProofError> {
        load_receipt_chain_from_store(&self.records, job_id)
    }

    /// Persist an authenticated cancel request without overwriting a contradiction.
    pub fn request_cancel(
        &self,
        job_id: &str,
        request: &CanaryCancellationRequest,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        for _ in 0..3 {
            let result = self.request_cancel_once(job_id, request);
            if !matches!(result, Err(ParallelProofError::ImmutableConflict(_))) {
                return result;
            }
        }
        Err(ParallelProofError::ImmutableConflict(job_id.to_owned()))
    }

    // Keep the same-sequence cancellation arbitration contiguous and auditable.
    #[allow(clippy::too_many_lines)]
    fn request_cancel_once(
        &self,
        job_id: &str,
        request: &CanaryCancellationRequest,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        let snapshot = self.load(job_id)?;
        validate_id(&request.controller_id, "canary cancel controller")?;
        if request.job_sha256 != snapshot.job.digest()?
            || request.controller_id != snapshot.job.owner.controller_id
            || request.approval_sha256 != snapshot.job.owner.approval_sha256
            || request.requested_at_ms < snapshot.job.approved_at_ms
        {
            return Err(ParallelProofError::AuthenticationFailed);
        }
        if let CanaryJobReceiptState::Terminal { outcome, .. } = snapshot.latest().receipt {
            return if outcome == CanaryJobTerminalOutcome::CancelledBeforeLaunch {
                Ok(StoreWriteOutcome::AlreadyPresent)
            } else {
                Err(ParallelProofError::InvalidAttemptSequence(
                    "canary cancellation lost terminal arbitration".to_owned(),
                ))
            };
        }
        if matches!(
            snapshot.latest().receipt,
            CanaryJobReceiptState::Prepared { .. }
        ) {
            let terminal = terminal_receipt(
                CanaryJobTerminalOutcome::CancelledBeforeLaunch,
                None,
                None,
                None,
                request.requested_at_ms,
            )?;
            let previous = snapshot.latest();
            let receipt = CanaryJobReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                job_sha256: snapshot.job.digest()?,
                sequence: previous.sequence + 1,
                previous_receipt_sha256: Some(previous.digest()?),
                receipt: terminal,
            };
            validate_receipt(
                &snapshot.job,
                &snapshot.receipts,
                &receipt,
                &receipt.job_sha256,
            )?;
            match self.put_receipt(job_id, &receipt) {
                Ok(outcome) => return Ok(outcome),
                Err(ParallelProofError::ImmutableConflict(_)) => {
                    // Launch won the same-sequence arbitration. Continue by
                    // competing a cancellation receipt with its next state.
                }
                Err(error) => return Err(error),
            }
        }
        let snapshot = self.load(job_id)?;
        let request_sha256 = domain_digest("shipyard.canary-job.cancel-request.v1", request)?;
        if let CanaryJobReceiptState::CancellationRequested {
            request_sha256: existing,
            ..
        }
        | CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
            request_sha256: existing,
            ..
        } = &snapshot.latest().receipt
        {
            return if *existing == request_sha256 {
                Ok(StoreWriteOutcome::AlreadyPresent)
            } else {
                Err(ParallelProofError::ImmutableConflict(job_id.to_owned()))
            };
        }
        if snapshot.is_terminal() {
            return Err(ParallelProofError::InvalidAttemptSequence(
                "canary cancellation lost terminal arbitration".to_owned(),
            ));
        }
        let (launch_nonce_sha256, process) = active_identity(&snapshot)?;
        let previous = snapshot.latest();
        let cancellation_state = if let Some(process) = process.cloned() {
            CanaryJobReceiptState::CancellationRequested {
                process,
                request_sha256: request_sha256.clone(),
                requested_at_ms: request.requested_at_ms,
            }
        } else if matches!(
            snapshot.latest().receipt,
            CanaryJobReceiptState::Launching { .. }
        ) {
            CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
                launch_nonce_sha256: launch_nonce_sha256.clone(),
                request_sha256: request_sha256.clone(),
                requested_at_ms: request.requested_at_ms,
            }
        } else {
            return Err(ParallelProofError::InvalidAttemptSequence(
                "canary cancellation has no launch custody".to_owned(),
            ));
        };
        let receipt = CanaryJobReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            job_sha256: snapshot.job.digest()?,
            sequence: previous.sequence + 1,
            previous_receipt_sha256: Some(previous.digest()?),
            receipt: cancellation_state,
        };
        validate_receipt(
            &snapshot.job,
            &snapshot.receipts,
            &receipt,
            &receipt.job_sha256,
        )?;
        match self.put_receipt(job_id, &receipt) {
            Ok(outcome) => Ok(outcome),
            Err(ParallelProofError::ImmutableConflict(_)) => {
                let latest = self.load(job_id)?;
                match &latest.latest().receipt {
                    CanaryJobReceiptState::CancellationRequested {
                        request_sha256: existing,
                        ..
                    }
                    | CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
                        request_sha256: existing,
                        ..
                    } if *existing == request_sha256 => Ok(StoreWriteOutcome::AlreadyPresent),
                    CanaryJobReceiptState::Terminal { .. } => {
                        Err(ParallelProofError::InvalidAttemptSequence(
                            "canary cancellation lost terminal arbitration".to_owned(),
                        ))
                    }
                    _ => Err(ParallelProofError::ImmutableConflict(job_id.to_owned())),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Durably acknowledge delivery of the selected terminal wake.
    pub fn acknowledge_wake(
        &self,
        job_id: &str,
        acknowledgement: &CanaryWakeAcknowledgement,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        let snapshot = self.load(job_id)?;
        let CanaryJobReceiptState::Terminal {
            outcome,
            completed_at_ms,
            ..
        } = &snapshot.latest().receipt
        else {
            return Err(ParallelProofError::InvalidAttemptSequence(
                "canary wake is not terminal".to_owned(),
            ));
        };
        if !selected_for_wake(&snapshot.job, *outcome)
            || acknowledgement.job_sha256 != snapshot.job.digest()?
            || acknowledgement.receipt_sha256 != snapshot.latest().digest()?
            || acknowledgement.controller_id != snapshot.job.owner.controller_id
            || acknowledgement.approval_sha256 != snapshot.job.owner.approval_sha256
            || acknowledgement.acknowledged_at_ms < *completed_at_ms
            || !valid_native_wake_acknowledgement(&snapshot.job, acknowledgement)
        {
            return Err(ParallelProofError::AuthenticationFailed);
        }
        self.records
            .put(&wake_ack_key(job_id), &serde_json::to_vec(acknowledgement)?)
            .map_err(map_store_error)
    }

    /// Record one redacted immutable log segment. Segments rotate by sequence and cap.
    pub fn record_log_segment(
        &self,
        job_id: &str,
        sequence: u32,
        bytes: &[u8],
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        let snapshot = self.load(job_id)?;
        if sequence >= snapshot.job.logs.max_segments {
            return Err(ParallelProofError::LimitExceeded {
                field: "canary log segments",
                max: snapshot.job.logs.max_segments as usize,
                found: sequence as usize + 1,
            });
        }
        let redacted = redact_log(bytes, snapshot.job.logs.segment_bytes as usize)?;
        self.logs
            .put(&log_key(job_id, sequence), &redacted)
            .map_err(map_store_error)
    }

    /// Persist the exact typed response bytes before a success receipt is legal.
    pub fn record_artifact(
        &self,
        job_id: &str,
        response: &CanaryJobResponse,
    ) -> Result<CanaryJobArtifact, ParallelProofError> {
        let snapshot = self.load(job_id)?;
        if matches!(
            snapshot.latest().receipt,
            CanaryJobReceiptState::Prepared { .. }
                | CanaryJobReceiptState::CancellationRequestedBeforeIdentity { .. }
                | CanaryJobReceiptState::Terminal { .. }
        ) {
            return Err(ParallelProofError::InvalidAttemptSequence(
                "canary artifact requires launch custody".to_owned(),
            ));
        }
        let (launch_nonce_sha256, _) = active_identity(&snapshot)?;
        response.validate(&snapshot.job, launch_nonce_sha256)?;
        let bytes = serde_json::to_vec(response)?;
        if bytes.is_empty() || bytes.len() > snapshot.job.success.max_artifact_bytes as usize {
            return Err(ParallelProofError::LimitExceeded {
                field: "canary job artifact bytes",
                max: snapshot.job.success.max_artifact_bytes as usize,
                found: bytes.len(),
            });
        }
        let artifact = CanaryJobArtifact {
            schema_version: snapshot.job.success.artifact_schema_version,
            operation_sha256: snapshot.job.operation.digest()?,
            content_sha256: Sha256Digest::of_bytes(&bytes),
            bytes: u32::try_from(bytes.len())
                .map_err(|_| ParallelProofError::InvalidField("canary artifact size"))?,
        };
        self.artifacts
            .put(&artifact_key(job_id), &bytes)
            .map_err(map_store_error)?;
        Ok(artifact)
    }

    fn artifact_matches(
        &self,
        job_id: &str,
        artifact: &CanaryJobArtifact,
    ) -> Result<bool, ParallelProofError> {
        match self.artifacts.load(&artifact_key(job_id)) {
            Ok(bytes) => {
                let snapshot = self.load_receipt_chain(job_id)?;
                let response: CanaryJobResponse = serde_json::from_slice(&bytes)?;
                let (launch_nonce_sha256, _) = active_identity(&snapshot)?;
                response.validate(&snapshot.job, launch_nonce_sha256)?;
                Ok(bytes.len() == artifact.bytes as usize
                    && Sha256Digest::of_bytes(&bytes) == artifact.content_sha256)
            }
            Err(ImmutableStoreError::Missing(_)) => Ok(false),
            Err(error) => Err(map_store_error(error)),
        }
    }

    /// Load one already-redacted immutable log segment.
    pub fn load_log_segment(
        &self,
        job_id: &str,
        sequence: u32,
    ) -> Result<Vec<u8>, ParallelProofError> {
        let snapshot = self.load(job_id)?;
        if sequence >= snapshot.job.logs.max_segments {
            return Err(ParallelProofError::InvalidField("canary log sequence"));
        }
        self.logs
            .load(&log_key(job_id, sequence))
            .map_err(map_store_error)
    }

    fn cancellation_requested(snapshot: &CanaryJobSnapshot) -> bool {
        matches!(
            snapshot.latest().receipt,
            CanaryJobReceiptState::CancellationRequested { .. }
                | CanaryJobReceiptState::CancellationRequestedBeforeIdentity { .. }
        )
    }

    fn cancellation_requested_at_ms(snapshot: &CanaryJobSnapshot) -> Option<u64> {
        match snapshot.latest().receipt {
            CanaryJobReceiptState::CancellationRequested {
                requested_at_ms, ..
            }
            | CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
                requested_at_ms, ..
            } => Some(requested_at_ms),
            _ => None,
        }
    }

    fn wake_pending(&self, snapshot: &CanaryJobSnapshot) -> Result<bool, ParallelProofError> {
        let CanaryJobReceiptState::Terminal {
            outcome,
            completed_at_ms,
            ..
        } = &snapshot.latest().receipt
        else {
            return Ok(false);
        };
        if !selected_for_wake(&snapshot.job, *outcome) {
            return Ok(false);
        }
        match self.records.load(&wake_ack_key(&snapshot.job.job_id)) {
            Ok(bytes) => {
                let acknowledgement: CanaryWakeAcknowledgement = serde_json::from_slice(&bytes)?;
                if acknowledgement.job_sha256 != snapshot.job.digest()?
                    || acknowledgement.receipt_sha256 != snapshot.latest().digest()?
                    || acknowledgement.controller_id != snapshot.job.owner.controller_id
                    || acknowledgement.approval_sha256 != snapshot.job.owner.approval_sha256
                    || acknowledgement.acknowledged_at_ms < *completed_at_ms
                    || !valid_native_wake_acknowledgement(&snapshot.job, &acknowledgement)
                {
                    return Err(ParallelProofError::AuthenticationFailed);
                }
                Ok(false)
            }
            Err(ImmutableStoreError::Missing(_)) => Ok(true),
            Err(error) => Err(map_store_error(error)),
        }
    }

    fn append(
        &self,
        snapshot: &CanaryJobSnapshot,
        receipt: CanaryJobReceiptState,
    ) -> Result<CanaryJobSnapshot, ParallelProofError> {
        if snapshot.is_terminal() {
            return Err(ParallelProofError::InvalidAttemptSequence(
                "canary job is terminal".to_owned(),
            ));
        }
        let previous = snapshot.latest();
        let next = CanaryJobReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            job_sha256: snapshot.job.digest()?,
            sequence: previous.sequence + 1,
            previous_receipt_sha256: Some(previous.digest()?),
            receipt,
        };
        validate_receipt(&snapshot.job, &snapshot.receipts, &next, &next.job_sha256)?;
        self.put_receipt(&snapshot.job.job_id, &next)?;
        self.load(&snapshot.job.job_id)
    }

    fn claim_launch(
        &self,
        snapshot: &CanaryJobSnapshot,
        launch_nonce_sha256: Sha256Digest,
        claimed_at_ms: u64,
    ) -> Result<(CanaryJobSnapshot, StoreWriteOutcome), ParallelProofError> {
        let previous = snapshot.latest();
        let claim = CanaryJobReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            job_sha256: snapshot.job.digest()?,
            sequence: previous.sequence + 1,
            previous_receipt_sha256: Some(previous.digest()?),
            receipt: CanaryJobReceiptState::Launching {
                launch_nonce_sha256,
                claimed_at_ms,
            },
        };
        validate_receipt(&snapshot.job, &snapshot.receipts, &claim, &claim.job_sha256)?;
        let outcome = self.put_receipt(&snapshot.job.job_id, &claim)?;
        Ok((self.load(&snapshot.job.job_id)?, outcome))
    }

    fn put_receipt(
        &self,
        job_id: &str,
        receipt: &CanaryJobReceipt,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        receipt.digest()?;
        self.records
            .put(
                &receipt_key(job_id, receipt.sequence),
                &serde_json::to_vec(receipt)?,
            )
            .map_err(map_store_error)
    }
}

fn load_snapshot_from_stores(
    records: &ImmutableByteStore,
    artifacts: &ImmutableByteStore,
    job_id: &str,
) -> Result<CanaryJobSnapshot, ParallelProofError> {
    let snapshot = load_receipt_chain_from_store(records, job_id)?;
    if let CanaryJobReceiptState::Terminal {
        outcome: CanaryJobTerminalOutcome::Succeeded,
        artifact: Some(artifact),
        ..
    } = &snapshot.latest().receipt
    {
        let bytes = artifacts
            .load(&artifact_key(job_id))
            .map_err(map_store_error)?;
        let response: CanaryJobResponse = serde_json::from_slice(&bytes)?;
        let (launch_nonce_sha256, _) = active_identity(&snapshot)?;
        response.validate(&snapshot.job, launch_nonce_sha256)?;
        if bytes.len() != artifact.bytes as usize
            || Sha256Digest::of_bytes(&bytes) != artifact.content_sha256
        {
            return Err(ParallelProofError::CorruptRecord(
                "canary job success artifact".to_owned(),
            ));
        }
    }
    Ok(snapshot)
}

fn load_receipt_chain_from_store(
    records: &ImmutableByteStore,
    job_id: &str,
) -> Result<CanaryJobSnapshot, ParallelProofError> {
    validate_id(job_id, "canary job id")?;
    let job: ApprovedCanaryJob = serde_json::from_slice(
        &records
            .load(&envelope_key(job_id))
            .map_err(map_store_error)?,
    )?;
    job.validate()?;
    if job.job_id != job_id {
        return Err(ParallelProofError::CorruptRecord(
            "canary job logical key".to_owned(),
        ));
    }
    let job_sha256 = job.digest()?;
    let mut receipts = Vec::new();
    let maximum = job.max_heartbeat_receipts + 5;
    for sequence in 0..maximum {
        let bytes = match records.load(&receipt_key(job_id, sequence)) {
            Ok(bytes) => bytes,
            Err(ImmutableStoreError::Missing(_)) => break,
            Err(error) => return Err(map_store_error(error)),
        };
        let receipt: CanaryJobReceipt = serde_json::from_slice(&bytes)?;
        validate_receipt(&job, &receipts, &receipt, &job_sha256)?;
        receipts.push(receipt);
    }
    if receipts.is_empty() {
        return Err(ParallelProofError::CorruptRecord(
            "canary job missing prepared receipt".to_owned(),
        ));
    }
    if records
        .contains(&receipt_key(job_id, maximum))
        .map_err(map_store_error)?
    {
        return Err(ParallelProofError::CorruptRecord(
            "canary job receipt limit".to_owned(),
        ));
    }
    Ok(CanaryJobSnapshot { job, receipts })
}
