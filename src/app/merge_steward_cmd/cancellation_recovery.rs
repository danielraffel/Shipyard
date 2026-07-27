use super::ledger::persist_pending_mutation_correlation;
use super::{
    BTreeMap, CANCEL_TERMINAL_POLL, CANCEL_TERMINAL_WAIT, CancellationReport, CapacityCancelError,
    DateTime, DurableMutationIntent, GitHubActions, Instant, MergeQueueMutationGuard,
    MutationControl, NonTerminalRun, Path, PendingCancellation, PendingCancellationPhase,
    PendingMutationKind, PendingRunState, RunCancellation, RunCancellationReason, StewardLedger,
    Utc, acquire_pending_cancellation_guard, active_runner_targets,
    cancel_capacity_preemption_after_revalidation, clear_pending_cancellation,
    finish_force_cancel_revalidation_failure, mark_cancellation_skipped, observe_repo,
    persist_capacity_evidence, persist_force_cancel_intent, read_current_pending_run_identity,
    read_pending_run, record_audit, revalidate_capacity_preemption, save_ledger, thread,
};

pub(super) fn resume_pending_cancellations(
    actions: &GitHubActions,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
) -> (
    BTreeMap<String, Vec<String>>,
    BTreeMap<String, Vec<CancellationReport>>,
) {
    let pending = ledger
        .pending_cancellations
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    let mut errors = BTreeMap::<String, Vec<String>>::new();
    let mut cancellations = BTreeMap::<String, Vec<CancellationReport>>::new();
    for (key, cancellation) in pending {
        match resume_pending_cancellation(
            actions,
            ledger_path,
            ledger,
            mutation_control,
            &key,
            &cancellation,
        ) {
            Ok(mutation) => cancellations
                .entry(cancellation.repo.clone())
                .or_default()
                .push(CancellationReport {
                    run_id: cancellation.run_id,
                    reason: cancellation.reason.clone(),
                    mutation: Some(mutation),
                    error: None,
                }),
            Err(error) => {
                record_audit(
                    ledger,
                    &cancellation.repo,
                    &format!("capacity-run:{}", cancellation.run_id),
                    "pending_cancellation_recovery_unhealthy",
                );
                let persistence = save_ledger(ledger_path, ledger).err();
                errors
                    .entry(cancellation.repo.clone())
                    .or_default()
                    .push(format!(
                        "pending cancellation recovery for run {} failed: {error}{}",
                        cancellation.run_id,
                        persistence.map_or_else(String::new, |save_error| format!(
                            "; recovery audit persistence also failed: {}",
                            save_error.message
                        ))
                    ));
            }
        }
    }
    (errors, cancellations)
}

pub(super) fn resume_pending_cancellation(
    actions: &GitHubActions,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
    key: &str,
    pending: &PendingCancellation,
) -> Result<String, String> {
    if pending.phase == PendingCancellationPhase::Skipped {
        return clear_recovered_skipped_cancellation(
            ledger,
            ledger_path,
            mutation_control,
            key,
            pending,
        );
    }
    match read_pending_run(actions, pending)? {
        PendingRunState::Terminal => {
            supersede_pending_uncertainty(mutation_control, pending)?;
            clear_pending_cancellation(
                ledger,
                ledger_path,
                key,
                pending,
                "pending_cancellation_observed_terminal",
            )?;
            Ok("recovered_terminal".to_owned())
        }
        PendingRunState::NonTerminal(_active)
            if pending.phase == PendingCancellationPhase::Intent =>
        {
            resume_pending_intent(actions, ledger_path, ledger, mutation_control, key, pending)
        }
        PendingRunState::NonTerminal(_active) => {
            supersede_pending_uncertainty(mutation_control, pending)?;
            let Some(active) = wait_for_pending_normal_terminalization(actions, pending)? else {
                clear_pending_cancellation(
                    ledger,
                    ledger_path,
                    key,
                    pending,
                    "pending_normal_cancel_terminalized",
                )?;
                return Ok("recovered_normal_cancel_terminal".to_owned());
            };
            resume_force_cancel_after_normal_wait(
                actions,
                ledger_path,
                ledger,
                mutation_control,
                key,
                pending,
                &active,
            )
        }
    }
}

pub(super) fn resume_force_cancel_after_normal_wait(
    actions: &GitHubActions,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
    key: &str,
    pending: &PendingCancellation,
    active: &NonTerminalRun,
) -> Result<String, String> {
    let targets = active_runner_targets(&active.jobs);
    persist_force_cancel_intent(
        ledger,
        ledger_path,
        &pending.repo,
        pending.run_id,
        &active.status,
        &targets,
    )
    .map_err(|error| error.message)?;
    let intent = DurableMutationIntent::new();
    persist_pending_mutation_correlation(
        ledger,
        ledger_path,
        key,
        intent.correlation_id(),
        PendingMutationKind::ForceCancel,
        "pending_force_cancel_intent",
    )?;
    let guard = acquire_pending_cancellation_guard(
        mutation_control,
        pending,
        &format!("runner steward resume force-cancel run {}", pending.run_id),
        &intent,
    )?;
    if let Err(error) = read_current_pending_run_identity(actions, pending) {
        let audit_error = finish_force_cancel_revalidation_failure(
            guard,
            ledger,
            ledger_path,
            &pending.repo,
            pending.run_id,
            "pending_force_cancel_revalidation_failed",
        )
        .err();
        return Err(format!(
            "exact force-cancel attempt revalidation failed: {error}{}",
            audit_error.map_or_else(String::new, |audit_error| format!(
                "; rejection audit also failed: {audit_error}"
            ))
        ));
    }
    actions
        .force_cancel_workflow_run(&pending.repo, pending.run_id)
        .map_err(|error| format!("exact force-cancel failed: {error}"))?;
    guard
        .finish("force_cancel_accepted")
        .map_err(|error| format!("force-cancel mutation audit failed: {error}"))?;
    record_audit(
        ledger,
        &pending.repo,
        &format!("capacity-run:{}", pending.run_id),
        "pending_force_cancel_accepted",
    );
    save_ledger(ledger_path, ledger).map_err(|error| {
        format!(
            "force-cancel accepted but recovery audit persistence failed: {}",
            error.message
        )
    })?;
    match wait_for_pending_terminalization(actions, pending)? {
        None => clear_pending_cancellation(
            ledger,
            ledger_path,
            key,
            pending,
            "pending_force_cancel_terminalized",
        )
        .map(|()| "recovered_force_cancel_terminal".to_owned()),
        Some(still_active) => Err(format!(
            "exact force-cancel accepted but run remains {} with active={}",
            still_active.status,
            active_runner_targets(&still_active.jobs)
        )),
    }
}

pub(super) fn clear_recovered_skipped_cancellation(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    mutation_control: &MutationControl,
    key: &str,
    pending: &PendingCancellation,
) -> Result<String, String> {
    supersede_pending_uncertainty(mutation_control, pending)?;
    clear_pending_cancellation(
        ledger,
        ledger_path,
        key,
        pending,
        "pending_skipped_cancellation_cleared",
    )?;
    Ok("recovered_skipped_cancellation".to_owned())
}

pub(super) fn wait_for_pending_terminalization(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<Option<NonTerminalRun>, String> {
    let deadline = Instant::now() + CANCEL_TERMINAL_WAIT;
    loop {
        match read_pending_run(actions, pending)? {
            PendingRunState::Terminal => return Ok(None),
            PendingRunState::NonTerminal(active)
                if Instant::now() + CANCEL_TERMINAL_POLL >= deadline =>
            {
                return Ok(Some(active));
            }
            PendingRunState::NonTerminal(_) => {
                thread::sleep(
                    CANCEL_TERMINAL_POLL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
    }
}

pub(super) fn wait_for_pending_normal_terminalization(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<Option<NonTerminalRun>, String> {
    let elapsed = DateTime::parse_from_rfc3339(&pending.initiated_at)
        .ok()
        .and_then(|started| (Utc::now() - started.with_timezone(&Utc)).to_std().ok())
        .unwrap_or_default();
    if elapsed >= CANCEL_TERMINAL_WAIT {
        return match read_pending_run(actions, pending)? {
            PendingRunState::Terminal => Ok(None),
            PendingRunState::NonTerminal(active) => Ok(Some(active)),
        };
    }
    let deadline = Instant::now()
        + CANCEL_TERMINAL_WAIT
            .checked_sub(elapsed)
            .expect("elapsed was checked against cancellation wait");
    loop {
        match read_pending_run(actions, pending)? {
            PendingRunState::Terminal => return Ok(None),
            PendingRunState::NonTerminal(active)
                if Instant::now() + CANCEL_TERMINAL_POLL >= deadline =>
            {
                return Ok(Some(active));
            }
            PendingRunState::NonTerminal(_) => thread::sleep(
                CANCEL_TERMINAL_POLL.min(deadline.saturating_duration_since(Instant::now())),
            ),
        }
    }
}

pub(super) fn resume_pending_intent(
    actions: &GitHubActions,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
    key: &str,
    pending: &PendingCancellation,
) -> Result<String, String> {
    let was_uncertain = pending_uncertainty(mutation_control, pending)?;
    let observation = observe_repo(actions, &pending.repo, &pending.base, true)?;
    let observed = observation
        .runs
        .iter()
        .find(|run| run.id == pending.run_id)
        .ok_or_else(|| {
            format!(
                "pending cancellation run {} disappeared from active observations",
                pending.run_id
            )
        })?;
    let cancellation = pending_run_cancellation(pending)?;
    let evidence = revalidate_capacity_preemption(
        actions,
        &observation,
        &cancellation,
        observed,
        &pending.front_head,
        &pending.opt_out_label,
    )?;
    let Some(evidence) = evidence else {
        return resolve_rejected_pending_intent(
            ledger,
            ledger_path,
            mutation_control,
            key,
            pending,
            was_uncertain,
        );
    };
    persist_capacity_evidence(
        &observation,
        &cancellation,
        &pending.front_head,
        &evidence,
        ledger_path,
        ledger,
    )?;
    if was_uncertain {
        supersede_pending_uncertainty(mutation_control, pending)?;
    }
    let intent = DurableMutationIntent::new();
    persist_pending_mutation_correlation(
        ledger,
        ledger_path,
        key,
        intent.correlation_id(),
        PendingMutationKind::NormalCancel,
        "pending_normal_cancel_retry_intent",
    )?;
    let guard = acquire_pending_cancellation_guard(
        mutation_control,
        pending,
        &format!("runner steward retry cancel run {}", pending.run_id),
        &intent,
    )?;
    match cancel_capacity_preemption_after_revalidation(
        actions,
        &observation,
        &cancellation,
        observed,
        &pending.front_head,
        &pending.opt_out_label,
    ) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return skip_recovered_intent_after_final_revalidation(ledger, ledger_path, key, guard);
        }
        Err(CapacityCancelError::Revalidation(error)) => {
            let audit_error = guard.finish("recovery_final_revalidation_failed").err();
            return Err(format!(
                "{error}{}",
                audit_error.map_or_else(String::new, |audit_error| format!(
                    "; mutation audit also failed: {audit_error}"
                ))
            ));
        }
        Err(CapacityCancelError::Mutation(error)) => {
            return Err(format!("exact normal cancellation retry failed: {error}"));
        }
    }
    finish_recovered_normal_cancel(
        actions,
        ledger_path,
        ledger,
        mutation_control,
        key,
        pending,
        guard,
    )
}

fn finish_recovered_normal_cancel(
    actions: &GitHubActions,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
    key: &str,
    pending: &PendingCancellation,
    guard: MergeQueueMutationGuard,
) -> Result<String, String> {
    let accepted = ledger
        .pending_cancellations
        .get_mut(key)
        .ok_or_else(|| "refreshed cancellation intent disappeared".to_owned())?;
    accepted.phase = PendingCancellationPhase::Accepted;
    record_audit(
        ledger,
        &pending.repo,
        &format!("capacity-run:{}", pending.run_id),
        "capacity_preemption_pending_after_recovery_acceptance",
    );
    save_ledger(ledger_path, ledger).map_err(|error| {
        format!(
            "normal cancellation retry accepted but pending phase persistence failed: {}",
            error.message
        )
    })?;
    guard
        .finish("cancel_accepted")
        .map_err(|error| format!("normal cancellation retry audit failed: {error}"))?;
    let accepted = ledger
        .pending_cancellations
        .get(key)
        .cloned()
        .ok_or_else(|| "accepted cancellation recovery record disappeared".to_owned())?;
    resume_pending_cancellation(
        actions,
        ledger_path,
        ledger,
        mutation_control,
        key,
        &accepted,
    )
}

fn skip_recovered_intent_after_final_revalidation(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    key: &str,
    guard: MergeQueueMutationGuard,
) -> Result<String, String> {
    let pending = ledger
        .pending_cancellations
        .get(key)
        .cloned()
        .ok_or_else(|| "refreshed cancellation intent disappeared".to_owned())?;
    mark_cancellation_skipped(ledger, ledger_path, key)?;
    guard
        .finish("skipped_after_recovery_final_revalidation")
        .map_err(|error| format!("mutation audit failed: {error}"))?;
    clear_pending_cancellation(
        ledger,
        ledger_path,
        key,
        &pending,
        "pending_intent_skipped_after_recovery_final_revalidation",
    )?;
    Ok("recovered_skipped_cancellation".to_owned())
}

pub(super) fn resolve_rejected_pending_intent(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    mutation_control: &MutationControl,
    key: &str,
    pending: &PendingCancellation,
    was_uncertain: bool,
) -> Result<String, String> {
    if was_uncertain {
        return Err(format!(
            "cancellation intent for run {} no longer passes capacity-safety revalidation, \
             but mutation {} is uncertain; preserving pending state until terminal proof",
            pending.run_id, pending.mutation_correlation_id
        ));
    }
    mark_cancellation_skipped(ledger, ledger_path, key)?;
    supersede_pending_uncertainty(mutation_control, pending)?;
    clear_pending_cancellation(
        ledger,
        ledger_path,
        key,
        pending,
        "pending_intent_skipped_after_revalidation",
    )?;
    Ok("recovered_skipped_cancellation".to_owned())
}

pub(super) fn pending_cancellation_reason(value: &str) -> Result<RunCancellationReason, String> {
    match value {
        "advisory_preamble_capacity_theft" => {
            Ok(RunCancellationReason::AdvisoryPreambleCapacityTheft)
        }
        "lower_priority_branch_preamble" => Ok(RunCancellationReason::LowerPriorityBranchPreamble),
        _ => Err(format!("unsupported pending cancellation reason `{value}`")),
    }
}

pub(super) fn pending_run_cancellation(
    pending: &PendingCancellation,
) -> Result<RunCancellation, String> {
    Ok(RunCancellation {
        run_id: pending.run_id,
        reason: pending_cancellation_reason(&pending.reason)?,
    })
}

pub(super) fn pending_uncertainty(
    control: &MutationControl,
    pending: &PendingCancellation,
) -> Result<bool, String> {
    let state_root = control
        .store
        .path()
        .parent()
        .unwrap_or(control.store.path());
    DurableMutationIntent::resume(&pending.mutation_correlation_id)?.is_uncertain(state_root)
}

pub(super) fn supersede_pending_uncertainty(
    control: &MutationControl,
    pending: &PendingCancellation,
) -> Result<(), String> {
    let state_root = control
        .store
        .path()
        .parent()
        .unwrap_or(control.store.path());
    DurableMutationIntent::resume(&pending.mutation_correlation_id)?.supersede_if_uncertain(
        state_root,
        &control.global_dir,
        &format!("steward durable {:?} recovery", pending.mutation_kind),
    )?;
    Ok(())
}
