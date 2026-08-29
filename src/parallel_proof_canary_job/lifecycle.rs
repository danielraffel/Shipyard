/// Persist intent and launch exactly once. A replay only returns durable state.
pub fn launch_canary_job<B: CanaryJobBackend>(
    store: &CanaryJobStore,
    job: &ApprovedCanaryJob,
    controller_now_ms: u64,
    backend: &mut B,
) -> Result<CanaryJobTransition, ParallelProofError> {
    if controller_now_ms < job.approved_at_ms || controller_now_ms >= job.deadline_at_ms {
        return Err(ParallelProofError::InvalidField(
            "canary launch controller time",
        ));
    }
    store.submit(job)?;
    let snapshot = store.load(&job.job_id)?;
    if snapshot.receipts.len() != 1 {
        return replay_transition(store, snapshot);
    }
    let CanaryJobReceiptState::Prepared {
        launch_nonce_sha256,
    } = &snapshot.latest().receipt
    else {
        return Err(ParallelProofError::CorruptRecord(
            "canary initial receipt".to_owned(),
        ));
    };
    let launch_nonce_sha256 = launch_nonce_sha256.clone();
    let (snapshot, claim_outcome) =
        match store.claim_launch(&snapshot, launch_nonce_sha256.clone(), controller_now_ms) {
            Ok(claim) => claim,
            Err(ParallelProofError::ImmutableConflict(_)) => {
                return replay_transition(store, store.load(&job.job_id)?);
            }
            Err(error) => return Err(error),
        };
    if claim_outcome != StoreWriteOutcome::Created {
        return Ok(transition(snapshot, false));
    }
    let process = match backend.launch(job, &launch_nonce_sha256, controller_now_ms) {
        Ok(process) => process,
        Err(error) => {
            let failure = domain_digest("shipyard.canary-job.launch-error.v1", &error)?;
            return Ok(retryable_transition(snapshot, failure));
        }
    };
    process.validate(job, &launch_nonce_sha256)?;
    let snapshot = store.append(&snapshot, CanaryJobReceiptState::Running { process })?;
    Ok(CanaryJobTransition {
        snapshot,
        wake: false,
        wake_receipt_sequence: None,
        retryable_failure_sha256: None,
        launched: true,
    })
}

/// Reconcile one existing job without ever redispatching it.
pub fn reconcile_canary_job<B: CanaryJobBackend>(
    store: &CanaryJobStore,
    job_id: &str,
    controller_now_ms: u64,
    backend: &mut B,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let snapshot = store.load(job_id)?;
    if snapshot.is_terminal() {
        return replay_transition(store, snapshot);
    }
    if matches!(
        snapshot.latest().receipt,
        CanaryJobReceiptState::Prepared { .. }
    ) {
        // Submission alone is not proof that launch was attempted. Reconciliation
        // never dispatches; a later authorized launch invocation may acquire the
        // immutable launch claim.
        return Ok(transition(snapshot, false));
    }
    if controller_now_ms < last_observed_at_ms(&snapshot) {
        return Err(ParallelProofError::InvalidField(
            "canary reconcile controller time",
        ));
    }
    let (launch_nonce, process) = active_identity(&snapshot)?;
    let observation = if let Some(process) = process {
        backend.observe(&snapshot.job, process)
    } else {
        backend.discover(&snapshot.job, launch_nonce)
    };
    let observation = match observation {
        Ok(observation) => observation,
        Err(error) => {
            let failure = domain_digest("shipyard.canary-job.observation-error.v1", &error)?;
            return Ok(retryable_transition(snapshot, failure));
        }
    };
    reconcile_observation(
        store,
        &snapshot,
        controller_now_ms,
        launch_nonce,
        process,
        observation,
        backend,
    )
}

fn reconcile_observation<B: CanaryJobBackend>(
    store: &CanaryJobStore,
    snapshot: &CanaryJobSnapshot,
    controller_now_ms: u64,
    launch_nonce: &Sha256Digest,
    process: Option<&CanaryProcessTreeIdentity>,
    observation: CanaryProcessObservation,
    backend: &mut B,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let cancel_requested = CanaryJobStore::cancellation_requested(snapshot);
    let deadline_cancel = snapshot.job.cancellation.cancel_at_deadline
        && controller_now_ms >= snapshot.job.deadline_at_ms;
    let heartbeat_expired = process.is_some()
        && controller_now_ms.saturating_sub(last_observed_at_ms(snapshot))
            > snapshot.job.heartbeat_timeout_ms;
    match observation {
        CanaryProcessObservation::Alive(observed) => {
            observed.validate(&snapshot.job, launch_nonce)?;
            if let Some(expected) = process
                && observed != *expected
            {
                return finish_lost(store, snapshot, controller_now_ms, None);
            }
            if cancel_requested || deadline_cancel || heartbeat_expired {
                return reconcile_cancellation(
                    store,
                    snapshot,
                    &observed,
                    controller_now_ms,
                    CanaryJobTerminalOutcome::Cancelled,
                    backend,
                );
            }
            if process.is_some()
                && controller_now_ms.saturating_sub(last_observed_at_ms(snapshot))
                    < snapshot.job.heartbeat_interval_ms
            {
                return Ok(transition(snapshot.clone(), false));
            }
            let heartbeat_count = heartbeat_count(snapshot)?;
            if heartbeat_count >= snapshot.job.max_heartbeat_receipts {
                return reconcile_cancellation(
                    store,
                    snapshot,
                    &observed,
                    controller_now_ms,
                    CanaryJobTerminalOutcome::HeartbeatLimit,
                    backend,
                );
            }
            let receipt = if process.is_none() {
                CanaryJobReceiptState::Running { process: observed }
            } else {
                CanaryJobReceiptState::Heartbeat {
                    process: observed,
                    observed_at_ms: controller_now_ms,
                }
            };
            Ok(transition(store.append(snapshot, receipt)?, false))
        }
        CanaryProcessObservation::Exited {
            process: observed,
            exit_code,
            exited_at_ms,
            artifact,
        } => {
            observed.validate(&snapshot.job, launch_nonce)?;
            if let Some(expected) = process
                && observed != *expected
            {
                return finish_lost(store, snapshot, controller_now_ms, None);
            }
            if CanaryJobStore::cancellation_requested_at_ms(snapshot)
                .is_some_and(|requested_at_ms| exited_at_ms >= requested_at_ms)
            {
                let terminal = terminal_receipt(
                    CanaryJobTerminalOutcome::Cancelled,
                    Some(observed),
                    None,
                    None,
                    controller_now_ms,
                )?;
                return Ok(terminal_transition(store.append(snapshot, terminal)?));
            }
            finish_exit(
                store,
                snapshot,
                observed,
                exit_code,
                exited_at_ms,
                artifact,
                controller_now_ms,
            )
        }
        CanaryProcessObservation::Missing | CanaryProcessObservation::IdentityMismatch => {
            finish_lost(store, snapshot, controller_now_ms, None)
        }
    }
}

fn last_observed_at_ms(snapshot: &CanaryJobSnapshot) -> u64 {
    snapshot
        .receipts
        .iter()
        .rev()
        .find_map(|receipt| match receipt.receipt {
            CanaryJobReceiptState::Heartbeat { observed_at_ms, .. } => Some(observed_at_ms),
            CanaryJobReceiptState::Running { ref process } => Some(process.launched_at_ms),
            CanaryJobReceiptState::Launching { claimed_at_ms, .. } => Some(claimed_at_ms),
            CanaryJobReceiptState::Prepared { .. } | CanaryJobReceiptState::Terminal { .. } => None,
            CanaryJobReceiptState::CancellationRequested {
                requested_at_ms, ..
            }
            | CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
                requested_at_ms, ..
            } => Some(requested_at_ms),
        })
        .unwrap_or(snapshot.job.approved_at_ms)
}

fn heartbeat_count(snapshot: &CanaryJobSnapshot) -> Result<u32, ParallelProofError> {
    u32::try_from(
        snapshot
            .receipts
            .iter()
            .filter(|receipt| matches!(receipt.receipt, CanaryJobReceiptState::Heartbeat { .. }))
            .count(),
    )
    .map_err(|_| ParallelProofError::CorruptRecord("canary heartbeat count".to_owned()))
}

fn reconcile_cancellation<B: CanaryJobBackend>(
    store: &CanaryJobStore,
    snapshot: &CanaryJobSnapshot,
    process: &CanaryProcessTreeIdentity,
    controller_now_ms: u64,
    terminated_outcome: CanaryJobTerminalOutcome,
    backend: &mut B,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let (outcome, failure) =
        match backend.cancel(&snapshot.job, process, snapshot.job.cancellation.grace_ms) {
            Ok(CanaryCancellationObservation::Terminated) => (terminated_outcome, None),
            Ok(CanaryCancellationObservation::StillAlive) => (
                CanaryJobTerminalOutcome::CancellationUncertain,
                Some("process tree remained alive"),
            ),
            Ok(CanaryCancellationObservation::Missing) => (
                CanaryJobTerminalOutcome::CancellationUncertain,
                Some("process identity disappeared during cancellation"),
            ),
            Err(error) => {
                let terminal = terminal_receipt(
                    CanaryJobTerminalOutcome::CancellationUncertain,
                    Some(process.clone()),
                    None,
                    Some(&error),
                    controller_now_ms,
                )?;
                return Ok(terminal_transition(store.append(snapshot, terminal)?));
            }
        };
    let terminal = terminal_receipt(
        outcome,
        Some(process.clone()),
        None,
        failure,
        controller_now_ms,
    )?;
    Ok(terminal_transition(store.append(snapshot, terminal)?))
}

fn finish_exit(
    store: &CanaryJobStore,
    snapshot: &CanaryJobSnapshot,
    process: CanaryProcessTreeIdentity,
    exit_code: Option<i32>,
    exited_at_ms: u64,
    artifact: Option<CanaryJobArtifact>,
    controller_now_ms: u64,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let operation_sha256 = snapshot.job.operation.digest()?;
    let valid_artifact = if let Some(artifact) = artifact.as_ref() {
        artifact.schema_version == snapshot.job.success.artifact_schema_version
            && artifact.operation_sha256 == operation_sha256
            && artifact.bytes > 0
            && artifact.bytes <= snapshot.job.success.max_artifact_bytes
            && store.artifact_matches(&snapshot.job.job_id, artifact)?
    } else {
        false
    };
    if exited_at_ms < process.launched_at_ms || exited_at_ms > controller_now_ms {
        return Err(ParallelProofError::BindingMismatch(
            "canary process exit time",
        ));
    }
    let within_deadline = !snapshot.job.cancellation.cancel_at_deadline
        || exited_at_ms <= snapshot.job.deadline_at_ms;
    let succeeded = exit_code == Some(snapshot.job.success.required_exit_code)
        && valid_artifact
        && within_deadline;
    let terminal = terminal_receipt(
        if succeeded {
            CanaryJobTerminalOutcome::Succeeded
        } else {
            CanaryJobTerminalOutcome::Failed
        },
        Some(process),
        succeeded.then_some(artifact).flatten(),
        (!succeeded).then_some("exit or artifact predicate failed"),
        controller_now_ms,
    )?;
    Ok(terminal_transition(store.append(snapshot, terminal)?))
}

fn finish_lost(
    store: &CanaryJobStore,
    snapshot: &CanaryJobSnapshot,
    controller_now_ms: u64,
    failure: Option<&str>,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let process = active_identity(snapshot)?.1.cloned();
    let terminal = terminal_receipt(
        CanaryJobTerminalOutcome::Lost,
        process,
        None,
        failure,
        controller_now_ms,
    )?;
    Ok(terminal_transition(store.append(snapshot, terminal)?))
}

fn active_identity(
    snapshot: &CanaryJobSnapshot,
) -> Result<(&Sha256Digest, Option<&CanaryProcessTreeIdentity>), ParallelProofError> {
    let CanaryJobReceiptState::Prepared {
        launch_nonce_sha256,
    } = &snapshot.receipts[0].receipt
    else {
        return Err(ParallelProofError::CorruptRecord(
            "canary prepared receipt".to_owned(),
        ));
    };
    let process = snapshot
        .receipts
        .iter()
        .rev()
        .find_map(|receipt| match &receipt.receipt {
            CanaryJobReceiptState::Running { process }
            | CanaryJobReceiptState::Heartbeat { process, .. }
            | CanaryJobReceiptState::CancellationRequested { process, .. } => Some(process),
            _ => None,
        });
    Ok((launch_nonce_sha256, process))
}

fn terminal_receipt(
    outcome: CanaryJobTerminalOutcome,
    process: Option<CanaryProcessTreeIdentity>,
    artifact: Option<CanaryJobArtifact>,
    failure: Option<&str>,
    completed_at_ms: u64,
) -> Result<CanaryJobReceiptState, ParallelProofError> {
    if completed_at_ms == 0
        || (outcome == CanaryJobTerminalOutcome::Succeeded) != artifact.is_some()
    {
        return Err(ParallelProofError::InvalidField("canary terminal receipt"));
    }
    Ok(CanaryJobReceiptState::Terminal {
        outcome,
        process,
        artifact,
        failure_sha256: failure
            .map(|message| domain_digest("shipyard.canary-job.failure.v1", &message))
            .transpose()?,
        completed_at_ms,
    })
}

fn transition(snapshot: CanaryJobSnapshot, launched: bool) -> CanaryJobTransition {
    CanaryJobTransition {
        snapshot,
        wake: false,
        wake_receipt_sequence: None,
        retryable_failure_sha256: None,
        launched,
    }
}

fn terminal_transition(snapshot: CanaryJobSnapshot) -> CanaryJobTransition {
    let wake = match snapshot.latest().receipt {
        CanaryJobReceiptState::Terminal { outcome, .. } => {
            selected_for_wake(&snapshot.job, outcome)
        }
        _ => false,
    };
    CanaryJobTransition {
        wake_receipt_sequence: wake.then_some(snapshot.latest().sequence),
        snapshot,
        wake,
        retryable_failure_sha256: None,
        launched: false,
    }
}

fn replay_transition(
    store: &CanaryJobStore,
    snapshot: CanaryJobSnapshot,
) -> Result<CanaryJobTransition, ParallelProofError> {
    let wake = store.wake_pending(&snapshot)?;
    Ok(CanaryJobTransition {
        wake_receipt_sequence: wake.then_some(snapshot.latest().sequence),
        snapshot,
        wake,
        retryable_failure_sha256: None,
        launched: false,
    })
}

fn selected_for_wake(job: &ApprovedCanaryJob, outcome: CanaryJobTerminalOutcome) -> bool {
    match outcome {
        CanaryJobTerminalOutcome::Succeeded => job.wake.on_success,
        CanaryJobTerminalOutcome::Failed
        | CanaryJobTerminalOutcome::CancellationUncertain
        | CanaryJobTerminalOutcome::Lost
        | CanaryJobTerminalOutcome::HeartbeatLimit => job.wake.on_actionable_failure,
        CanaryJobTerminalOutcome::Cancelled | CanaryJobTerminalOutcome::CancelledBeforeLaunch => {
            false
        }
    }
}

fn valid_native_wake_acknowledgement(
    job: &ApprovedCanaryJob,
    acknowledgement: &CanaryWakeAcknowledgement,
) -> bool {
    match (
        job.schema_version,
        acknowledgement.native_wake_id.as_deref(),
        acknowledgement.native_delivery_sha256.as_ref(),
    ) {
        (LEGACY_JOB_SCHEMA_VERSION, None, None) => true,
        (CURRENT_JOB_SCHEMA_VERSION, Some(wake_id), Some(delivery_sha256)) => job
            .native_continuation
            .as_ref()
            .and_then(|binding| {
                native_wake_delivery_digest(
                    binding,
                    &acknowledgement.job_sha256,
                    &acknowledgement.receipt_sha256,
                    wake_id,
                )
                .ok()
            })
            .is_some_and(|expected| expected == *delivery_sha256),
        _ => false,
    }
}

/// Bind the native outbox identity to the exact canary terminal receipt.
pub(crate) fn native_wake_delivery_digest(
    binding: &CanaryNativeContinuationBinding,
    job_sha256: &Sha256Digest,
    terminal_receipt_sha256: &Sha256Digest,
    wake_id: &str,
) -> Result<Sha256Digest, ParallelProofError> {
    let expected_wake_id = native_wake_id(binding)?;
    if binding.schema_version != 1
        || binding.work_generation == 0
        || binding.owner_generation == 0
        || wake_id != expected_wake_id
    {
        return Err(ParallelProofError::InvalidField(
            "canary native wake delivery",
        ));
    }
    Ok(Sha256Digest::of_bytes(
        format!(
            "shipyard.canary-native-wake-delivery.v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            job_sha256.as_str(),
            terminal_receipt_sha256.as_str(),
            binding.work_item_id,
            binding.work_generation,
            binding.owner_generation,
            binding.route_ref,
            binding.profile_ref,
            binding.payload_digest,
            wake_id,
        )
        .as_bytes(),
    ))
}

fn native_wake_id(binding: &CanaryNativeContinuationBinding) -> Result<String, ParallelProofError> {
    let dispatch_generation =
        binding
            .work_generation
            .checked_add(2)
            .ok_or(ParallelProofError::InvalidField(
                "canary native work generation",
            ))?;
    Ok(crate::work_ledger::deterministic_wake_id(
        &binding.work_item_id,
        dispatch_generation,
        binding.owner_generation,
        &binding.route_ref,
        &binding.payload_digest,
    ))
}

fn retryable_transition(
    snapshot: CanaryJobSnapshot,
    retryable_failure_sha256: Sha256Digest,
) -> CanaryJobTransition {
    CanaryJobTransition {
        snapshot,
        wake: false,
        wake_receipt_sequence: None,
        retryable_failure_sha256: Some(retryable_failure_sha256),
        launched: false,
    }
}

// Receipt validation is intentionally one exhaustive state-transition table.
#[allow(clippy::too_many_lines)]
fn validate_receipt(
    job: &ApprovedCanaryJob,
    previous: &[CanaryJobReceipt],
    receipt: &CanaryJobReceipt,
    job_sha256: &Sha256Digest,
) -> Result<(), ParallelProofError> {
    receipt.digest()?;
    if receipt.job_sha256 != *job_sha256 || receipt.sequence as usize != previous.len() {
        return Err(ParallelProofError::CorruptRecord(
            "canary job receipt identity".to_owned(),
        ));
    }
    if let Some(prior) = previous.last()
        && (receipt.previous_receipt_sha256.as_ref() != Some(&prior.digest()?)
            || matches!(prior.receipt, CanaryJobReceiptState::Terminal { .. }))
    {
        return Err(ParallelProofError::CorruptRecord(
            "canary job receipt ordering".to_owned(),
        ));
    }
    let nonce = previous.first().and_then(|first| match &first.receipt {
        CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } => Some(launch_nonce_sha256),
        _ => None,
    });
    match &receipt.receipt {
        CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } if receipt.sequence == 0 => {
            let expected = domain_digest(
                "shipyard.canary-job.launch-nonce.v1",
                &(job_sha256, &job.owner.controller_incarnation),
            )?;
            if *launch_nonce_sha256 != expected {
                return Err(ParallelProofError::CorruptRecord(
                    "canary prepared launch nonce".to_owned(),
                ));
            }
        }
        CanaryJobReceiptState::Launching {
            launch_nonce_sha256,
            claimed_at_ms,
        } => {
            if !matches!(
                previous.last().map(|prior| &prior.receipt),
                Some(CanaryJobReceiptState::Prepared { .. })
            ) || Some(launch_nonce_sha256) != nonce
                || *claimed_at_ms < job.approved_at_ms
            {
                return Err(ParallelProofError::CorruptRecord(
                    "canary launching transition".to_owned(),
                ));
            }
        }
        CanaryJobReceiptState::Running { process } => {
            if !matches!(
                previous.last().map(|prior| &prior.receipt),
                Some(CanaryJobReceiptState::Launching { .. })
            ) {
                return Err(ParallelProofError::CorruptRecord(
                    "canary running transition".to_owned(),
                ));
            }
            process.validate(job, required_nonce(nonce)?)?;
            let claimed_at_ms = previous.last().and_then(|prior| match &prior.receipt {
                CanaryJobReceiptState::Launching { claimed_at_ms, .. } => Some(*claimed_at_ms),
                _ => None,
            });
            if claimed_at_ms != Some(process.launched_at_ms) {
                return Err(ParallelProofError::CorruptRecord(
                    "canary process launch claim time".to_owned(),
                ));
            }
        }
        CanaryJobReceiptState::Heartbeat {
            process,
            observed_at_ms,
        } => {
            let prior_process = previous.last().and_then(receipt_process).ok_or_else(|| {
                ParallelProofError::CorruptRecord("canary heartbeat transition".to_owned())
            })?;
            let prior_observed_at_ms = previous
                .last()
                .and_then(receipt_observed_at_ms)
                .unwrap_or(prior_process.launched_at_ms);
            if process != prior_process
                || *observed_at_ms < prior_observed_at_ms
                || *observed_at_ms < process.launched_at_ms
            {
                return Err(ParallelProofError::CorruptRecord(
                    "canary heartbeat identity or time".to_owned(),
                ));
            }
            process.validate(job, required_nonce(nonce)?)?;
        }
        CanaryJobReceiptState::CancellationRequested {
            process,
            requested_at_ms,
            ..
        } => {
            let prior_process = previous.last().and_then(receipt_process).ok_or_else(|| {
                ParallelProofError::CorruptRecord(
                    "canary cancellation request transition".to_owned(),
                )
            })?;
            let prior_observed_at_ms = previous
                .last()
                .and_then(receipt_observed_at_ms)
                .unwrap_or(prior_process.launched_at_ms);
            if process != prior_process || *requested_at_ms < prior_observed_at_ms {
                return Err(ParallelProofError::CorruptRecord(
                    "canary cancellation request identity or time".to_owned(),
                ));
            }
            process.validate(job, required_nonce(nonce)?)?;
        }
        CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
            launch_nonce_sha256,
            requested_at_ms,
            ..
        } => {
            let Some(CanaryJobReceiptState::Launching { claimed_at_ms, .. }) =
                previous.last().map(|prior| &prior.receipt)
            else {
                return Err(ParallelProofError::CorruptRecord(
                    "canary pre-identity cancellation transition".to_owned(),
                ));
            };
            if Some(launch_nonce_sha256) != nonce || *requested_at_ms < *claimed_at_ms {
                return Err(ParallelProofError::CorruptRecord(
                    "canary pre-identity cancellation binding".to_owned(),
                ));
            }
        }
        terminal @ CanaryJobReceiptState::Terminal { .. } => {
            validate_terminal_receipt(job, previous, terminal, nonce)?;
        }
        CanaryJobReceiptState::Prepared { .. } => {
            return Err(ParallelProofError::CorruptRecord(
                "canary job receipt transition".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_terminal_receipt(
    job: &ApprovedCanaryJob,
    previous: &[CanaryJobReceipt],
    terminal: &CanaryJobReceiptState,
    nonce: Option<&Sha256Digest>,
) -> Result<(), ParallelProofError> {
    let CanaryJobReceiptState::Terminal {
        outcome,
        process,
        artifact,
        completed_at_ms,
        ..
    } = terminal
    else {
        return Err(ParallelProofError::CorruptRecord(
            "canary terminal receipt type".to_owned(),
        ));
    };
    let prior_process = previous.iter().rev().find_map(receipt_process);
    let follows_launch_claim = matches!(
        previous.last().map(|prior| &prior.receipt),
        Some(
            CanaryJobReceiptState::Launching { .. }
                | CanaryJobReceiptState::CancellationRequestedBeforeIdentity { .. }
        )
    );
    let launch_claimed_at_ms = previous
        .iter()
        .rev()
        .find_map(|prior| match &prior.receipt {
            CanaryJobReceiptState::Launching { claimed_at_ms, .. } => Some(*claimed_at_ms),
            _ => None,
        });
    let prior_observed_at_ms = previous
        .iter()
        .rev()
        .find_map(receipt_observed_at_ms)
        .unwrap_or(job.approved_at_ms);
    if *completed_at_ms < prior_observed_at_ms
        || (*outcome == CanaryJobTerminalOutcome::Succeeded) != artifact.is_some()
        || (matches!(
            *outcome,
            CanaryJobTerminalOutcome::Succeeded
                | CanaryJobTerminalOutcome::Cancelled
                | CanaryJobTerminalOutcome::CancellationUncertain
                | CanaryJobTerminalOutcome::HeartbeatLimit
        ) && process.is_none())
        || prior_process.is_some_and(|prior| process.as_ref() != Some(prior))
        || (prior_process.is_none() && process.is_some() && !follows_launch_claim)
        || (follows_launch_claim
            && process
                .as_ref()
                .is_some_and(|process| Some(process.launched_at_ms) != launch_claimed_at_ms))
    {
        return Err(ParallelProofError::CorruptRecord(
            "canary terminal semantics".to_owned(),
        ));
    }
    if let Some(process) = process {
        process.validate(job, required_nonce(nonce)?)?;
    }
    if let Some(artifact) = artifact
        && (artifact.schema_version != job.success.artifact_schema_version
            || artifact.operation_sha256 != job.operation.digest()?
            || artifact.bytes == 0
            || artifact.bytes > job.success.max_artifact_bytes)
    {
        return Err(ParallelProofError::CorruptRecord(
            "canary terminal artifact predicate".to_owned(),
        ));
    }
    Ok(())
}

fn required_nonce(nonce: Option<&Sha256Digest>) -> Result<&Sha256Digest, ParallelProofError> {
    nonce.ok_or_else(|| ParallelProofError::CorruptRecord("canary launch nonce".to_owned()))
}

fn receipt_process(receipt: &CanaryJobReceipt) -> Option<&CanaryProcessTreeIdentity> {
    match &receipt.receipt {
        CanaryJobReceiptState::Running { process }
        | CanaryJobReceiptState::Heartbeat { process, .. }
        | CanaryJobReceiptState::CancellationRequested { process, .. } => Some(process),
        _ => None,
    }
}

fn receipt_observed_at_ms(receipt: &CanaryJobReceipt) -> Option<u64> {
    match &receipt.receipt {
        CanaryJobReceiptState::Launching { claimed_at_ms, .. } => Some(*claimed_at_ms),
        CanaryJobReceiptState::Running { process } => Some(process.launched_at_ms),
        CanaryJobReceiptState::Heartbeat { observed_at_ms, .. } => Some(*observed_at_ms),
        CanaryJobReceiptState::CancellationRequested {
            requested_at_ms, ..
        }
        | CanaryJobReceiptState::CancellationRequestedBeforeIdentity {
            requested_at_ms, ..
        } => Some(*requested_at_ms),
        _ => None,
    }
}

fn validate_distributed_observation(
    expected_artifact_bytes: u64,
    observation: &DistributedExecutionObservation,
) -> Result<(), ParallelProofError> {
    let delivery = &observation.delivery;
    if delivery.artifact_bytes_total != expected_artifact_bytes
        || delivery
            .artifact_bytes_reused
            .checked_add(delivery.artifact_bytes_transferred)
            != Some(expected_artifact_bytes)
        || observation.submit_to_receipt_ms == 0
        || observation.worker_active_ms < observation.shard_execution_ms
    {
        return Err(ParallelProofError::BindingMismatch(
            "canary typed response counters",
        ));
    }
    match (delivery.mode, delivery.interruption.as_ref()) {
        (ArtifactDeliveryMode::FullTransfer, None)
            if delivery.artifact_bytes_reused == 0
                && delivery.artifact_bytes_transferred == expected_artifact_bytes =>
        {
            Ok(())
        }
        (ArtifactDeliveryMode::ImmutableObjectReuse, None)
            if delivery.artifact_bytes_reused == expected_artifact_bytes
                && delivery.artifact_bytes_transferred == 0 =>
        {
            Ok(())
        }
        (ArtifactDeliveryMode::VerifiedPrefixResume, Some(interruption))
            if interruption.verified_resume_offset_bytes == delivery.artifact_bytes_reused
                && interruption.bytes_before_interruption
                    >= interruption.verified_resume_offset_bytes
                && interruption.bytes_after_resume == delivery.artifact_bytes_transferred
                && interruption.verified_resume_offset_bytes > 0
                && interruption.verified_resume_offset_bytes < expected_artifact_bytes =>
        {
            Ok(())
        }
        _ => Err(ParallelProofError::InvalidField(
            "canary typed response delivery",
        )),
    }
}

fn redact_log(bytes: &[u8], max: usize) -> Result<Vec<u8>, ParallelProofError> {
    if bytes.len() > max {
        return Err(ParallelProofError::LimitExceeded {
            field: "canary log segment bytes",
            max,
            found: bytes.len(),
        });
    }
    let text = String::from_utf8_lossy(bytes);
    let mut output = String::with_capacity(text.len());
    for line in text.lines() {
        if safe_structured_log_line(line) {
            output.push_str(line);
        } else {
            output.push_str("[REDACTED]");
        }
        output.push('\n');
    }
    let output = output.into_bytes();
    if output.len() > max {
        return Err(ParallelProofError::LimitExceeded {
            field: "redacted canary log segment bytes",
            max,
            found: output.len(),
        });
    }
    Ok(output)
}

fn safe_structured_log_line(line: &str) -> bool {
    let Some((key, value)) = line.split_once('=') else {
        return false;
    };
    match key {
        "phase" => matches!(
            value,
            "prepare" | "transfer" | "verify" | "dispatch" | "aggregate" | "cancel" | "complete"
        ),
        "status" => matches!(
            value,
            "started" | "running" | "succeeded" | "failed" | "cancelled"
        ),
        "progress" => value
            .strip_suffix('%')
            .unwrap_or(value)
            .parse::<u8>()
            .is_ok_and(|progress| progress <= 100),
        _ => false,
    }
}

fn validate_id(value: &str, field: &'static str) -> Result<(), ParallelProofError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return Err(ParallelProofError::InvalidField(field));
    }
    Ok(())
}

fn envelope_key(job_id: &str) -> String {
    format!("job-{job_id}-envelope")
}

fn input_key(job_id: &str) -> String {
    format!("job-{job_id}-input")
}

fn receipt_key(job_id: &str, sequence: u32) -> String {
    format!("job-{job_id}-receipt-{sequence:03}")
}

fn wake_ack_key(job_id: &str) -> String {
    format!("job-{job_id}-wake-ack")
}

fn log_key(job_id: &str, sequence: u32) -> String {
    format!("job-{job_id}-log-{sequence:03}")
}

fn artifact_key(job_id: &str) -> String {
    format!("job-{job_id}-artifact")
}

fn domain_digest<T: Serialize>(
    domain: &str,
    value: &T,
) -> Result<Sha256Digest, ParallelProofError> {
    let mut bytes = domain.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend(serde_json::to_vec(value)?);
    Ok(Sha256Digest::of_bytes(&bytes))
}

fn map_store_error(error: ImmutableStoreError) -> ParallelProofError {
    match error {
        ImmutableStoreError::InvalidRoot => ParallelProofError::InvalidField("canary job root"),
        ImmutableStoreError::UnsafePath(path) => {
            ParallelProofError::CorruptRecord(format!("unsafe canary job path {}", path.display()))
        }
        ImmutableStoreError::LimitExceeded { max, found } => ParallelProofError::LimitExceeded {
            field: "canary job record bytes",
            max,
            found,
        },
        ImmutableStoreError::Missing(key) => ParallelProofError::MissingRecord(key),
        ImmutableStoreError::Conflict(key) => ParallelProofError::ImmutableConflict(key),
        ImmutableStoreError::Io(error) => ParallelProofError::Io(error),
    }
}
