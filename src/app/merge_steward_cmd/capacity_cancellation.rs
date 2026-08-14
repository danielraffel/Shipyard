use super::{
    CapacityApplyContext, CapacityRevalidation, DurableMutationIntent, GitHubActions,
    MergeQueueMutationGuard, Path, PendingCancellation, PendingCancellationPhase,
    PendingMutationKind, RepoObservation, RunCancellation, RunCancellationReason, StewardLedger,
    StewardRun, Utc, acquire_pending_cancellation_guard, cancellation_reason_label,
    clear_pending_cancellation, complete_capacity_cancellation, merge_group_pr_number,
    preemption_key, queue_front_head, record_audit, revalidate_capacity_preemption, save_ledger,
    validate_pending_cancellation_authority,
};

pub(super) enum CapacityCancelError {
    Revalidation(String),
    Mutation(String),
}

pub(super) fn cancel_capacity_preemption_after_revalidation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    cancellation: &RunCancellation,
    observed: &StewardRun,
    expected_front: &str,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
) -> Result<Option<CapacityRevalidation>, CapacityCancelError> {
    if cancellation.reason != RunCancellationReason::AdvisoryPreambleCapacityTheft {
        return Ok(None);
    }
    let evidence = revalidate_capacity_preemption(
        actions,
        observation,
        cancellation,
        observed,
        expected_front,
        opt_out_label,
        managed_label,
        handoff_context,
    )
    .map_err(CapacityCancelError::Revalidation)?;
    let Some(evidence) = evidence else {
        return Ok(None);
    };
    actions
        .cancel_workflow_run(&observation.repo, cancellation.run_id)
        .map_err(|error| CapacityCancelError::Mutation(error.to_string()))?;
    Ok(Some(evidence))
}

pub(super) fn apply_capacity_preemption(
    context: &CapacityApplyContext<'_>,
    opt_out_label: &str,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let Some(observed) = context
        .observation
        .runs
        .iter()
        .find(|run| run.id == context.cancellation.run_id)
    else {
        return (None, Some("planned run observation disappeared".to_owned()));
    };
    let Some(expected_front) = queue_front_head(context.observation) else {
        return (Some("skipped_after_front_revalidation".to_owned()), None);
    };
    let (guard, pending) =
        match prepare_capacity_preemption(context, opt_out_label, ledger, observed, expected_front)
        {
            Ok(Some(prepared)) => prepared,
            Ok(None) => {
                return (
                    Some("skipped_after_precancel_revalidation".to_owned()),
                    None,
                );
            }
            Err(error) => return (None, Some(error)),
        };
    let latest = match revalidate_capacity_preemption(
        context.actions,
        context.observation,
        context.cancellation,
        observed,
        expected_front,
        opt_out_label,
        context.managed_label,
        context.handoff_context,
    ) {
        Ok(Some(latest)) => latest,
        Ok(None) => {
            return skip_after_attempt_revalidation(guard, &pending, context.ledger_path, ledger);
        }
        Err(error) => {
            let audit_error = guard.finish("attempt_revalidation_failed").err();
            return (
                None,
                Some(format!(
                    "{error}{}",
                    audit_error.map_or_else(String::new, |audit_error| format!(
                        "; mutation audit also failed: {audit_error}"
                    ))
                )),
            );
        }
    };
    if let Err(error) = persist_capacity_evidence(
        context.observation,
        context.cancellation,
        expected_front,
        &latest,
        context.ledger_path,
        ledger,
    ) {
        let audit_error = guard.finish("attempt_evidence_persistence_failed").err();
        return (
            None,
            Some(format!(
                "{error}{}",
                audit_error.map_or_else(String::new, |audit_error| format!(
                    "; mutation audit also failed: {audit_error}"
                ))
            )),
        );
    }
    let latest = match cancel_capacity_preemption_after_revalidation(
        context.actions,
        context.observation,
        context.cancellation,
        observed,
        expected_front,
        opt_out_label,
        context.managed_label,
        context.handoff_context,
    ) {
        Ok(Some(latest)) => latest,
        Ok(None) => {
            return skip_after_attempt_revalidation(guard, &pending, context.ledger_path, ledger);
        }
        Err(CapacityCancelError::Revalidation(error)) => {
            let audit_error = guard.finish("final_revalidation_failed").err();
            return (
                None,
                Some(format!(
                    "{error}{}",
                    audit_error.map_or_else(String::new, |audit_error| format!(
                        "; mutation audit also failed: {audit_error}"
                    ))
                )),
            );
        }
        Err(CapacityCancelError::Mutation(error)) => return (None, Some(error)),
    };
    finish_accepted_capacity_preemption(context, expected_front, &latest.candidate, guard, ledger)
}

fn finish_accepted_capacity_preemption(
    context: &CapacityApplyContext<'_>,
    expected_front: &str,
    latest: &StewardRun,
    guard: MergeQueueMutationGuard,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    if let Err(error) = mark_cancellation_accepted(context, expected_front, latest, ledger) {
        return (
            Some("cancelled_after_job_revalidation".to_owned()),
            Some(format!(
                "cancel accepted but pending recovery persistence failed: {error}"
            )),
        );
    }
    if let Err(error) = guard.finish("cancel_accepted") {
        return (
            Some("cancelled_after_job_revalidation".to_owned()),
            Some(format!(
                "cancel accepted but mutation audit failed: {error}"
            )),
        );
    }
    complete_capacity_cancellation(context, expected_front, latest, ledger)
}

fn skip_after_attempt_revalidation(
    guard: MergeQueueMutationGuard,
    pending: &PendingCancellation,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let key = pending_cancellation_key(pending);
    if let Err(error) = mark_cancellation_skipped(ledger, ledger_path, &key) {
        return (None, Some(error));
    }
    if let Err(error) = guard.finish("skipped_after_attempt_revalidation") {
        return (None, Some(format!("mutation audit failed: {error}")));
    }
    if let Err(error) = clear_pending_cancellation(
        ledger,
        ledger_path,
        &key,
        pending,
        "skipped_after_attempt_revalidation",
    ) {
        return (None, Some(error));
    }
    (Some("skipped_after_attempt_revalidation".to_owned()), None)
}

pub(super) fn prepare_capacity_preemption(
    context: &CapacityApplyContext<'_>,
    opt_out_label: &str,
    ledger: &mut StewardLedger,
    observed: &StewardRun,
    expected_front: &str,
) -> Result<Option<(MergeQueueMutationGuard, PendingCancellation)>, String> {
    let (guard, pending) =
        start_capacity_preemption(context, opt_out_label, ledger, observed, expected_front)?;
    let cancel_live = match revalidate_capacity_preemption(
        context.actions,
        context.observation,
        context.cancellation,
        observed,
        expected_front,
        opt_out_label,
        context.managed_label,
        context.handoff_context,
    ) {
        Ok(Some(evidence)) => evidence,
        Ok(None) => {
            mark_cancellation_skipped(
                ledger,
                context.ledger_path,
                &pending_cancellation_key(&pending),
            )?;
            guard
                .finish("skipped_after_precancel_revalidation")
                .map_err(|error| format!("mutation audit failed: {error}"))?;
            clear_pending_cancellation(
                ledger,
                context.ledger_path,
                &pending_cancellation_key(&pending),
                &pending,
                "skipped_after_precancel_revalidation",
            )?;
            return Ok(None);
        }
        Err(error) => {
            let audit_error = guard.finish("revalidation_failed").err();
            return Err(format!(
                "{error}{}",
                audit_error.map_or_else(String::new, |error| format!(
                    "; mutation audit also failed: {error}"
                ))
            ));
        }
    };
    if let Err(error) = persist_capacity_evidence(
        context.observation,
        context.cancellation,
        expected_front,
        &cancel_live,
        context.ledger_path,
        ledger,
    ) {
        let audit_error = guard.finish("evidence_persistence_failed").err();
        return Err(format!(
            "{error}{}",
            audit_error.map_or_else(String::new, |error| format!(
                "; mutation audit also failed: {error}"
            ))
        ));
    }
    Ok(Some((guard, pending)))
}

pub(super) fn start_capacity_preemption(
    context: &CapacityApplyContext<'_>,
    opt_out_label: &str,
    ledger: &mut StewardLedger,
    observed: &StewardRun,
    expected_front: &str,
) -> Result<(MergeQueueMutationGuard, PendingCancellation), String> {
    let intent = DurableMutationIntent::new();
    let pending = pending_cancellation(
        context,
        expected_front,
        observed,
        intent.correlation_id(),
        PendingCancellationPhase::Intent,
        opt_out_label,
    )?;
    validate_pending_cancellation_authority(context.mutation_control, &pending)?;
    let key = format!("{}:{}", context.observation.repo, preemption_key(observed));
    *ledger.preemption_attempts.entry(key).or_default() += 1;
    record_audit(
        ledger,
        &context.observation.repo,
        &format!(
            "front:{expected_front}:capacity-run:{}:{}",
            context.cancellation.run_id, observed.head_sha
        ),
        &format!(
            "capacity_preemption_started:{:?}",
            context.cancellation.reason
        ),
    );
    persist_pending_cancellation(context.ledger_path, ledger, pending.clone())?;
    let guard = acquire_pending_cancellation_guard(
        context.mutation_control,
        &pending,
        &format!(
            "runner steward preempt capacity run {}",
            context.cancellation.run_id
        ),
        &intent,
    )?;
    Ok((guard, pending))
}

pub(super) fn persist_pending_cancellation(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    pending: PendingCancellation,
) -> Result<(), String> {
    let key = pending_cancellation_key(&pending);
    let repo = pending.repo.clone();
    let run_id = pending.run_id;
    let phase = pending.phase;
    ledger.pending_cancellations.insert(key, pending);
    record_audit(
        ledger,
        &repo,
        &format!("capacity-run:{run_id}"),
        &format!("capacity_preemption_pending:{phase:?}"),
    );
    save_ledger(ledger_path, ledger).map_err(|error| error.message)
}

pub(super) fn mark_cancellation_accepted(
    context: &CapacityApplyContext<'_>,
    expected_front: &str,
    run: &StewardRun,
    ledger: &mut StewardLedger,
) -> Result<(), String> {
    let probe = pending_cancellation_key_parts(
        &context.observation.repo,
        context.cancellation.run_id,
        &run.head_sha,
        expected_front,
    );
    let pending = ledger
        .pending_cancellations
        .get_mut(&probe)
        .ok_or_else(|| "pending cancellation intent disappeared".to_owned())?;
    pending.phase = PendingCancellationPhase::Accepted;
    record_audit(
        ledger,
        &context.observation.repo,
        &format!("capacity-run:{}", context.cancellation.run_id),
        "capacity_preemption_pending_after_acceptance",
    );
    save_ledger(context.ledger_path, ledger).map_err(|error| error.message)
}

pub(super) fn mark_cancellation_skipped(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    key: &str,
) -> Result<(), String> {
    let pending = ledger
        .pending_cancellations
        .get_mut(key)
        .ok_or_else(|| "pending cancellation intent disappeared".to_owned())?;
    pending.phase = PendingCancellationPhase::Skipped;
    let repo = pending.repo.clone();
    let run_id = pending.run_id;
    record_audit(
        ledger,
        &repo,
        &format!("capacity-run:{run_id}"),
        "capacity_preemption_skipped_before_mutation",
    );
    save_ledger(ledger_path, ledger).map_err(|error| error.message)
}

pub(super) fn pending_cancellation(
    context: &CapacityApplyContext<'_>,
    front_head: &str,
    run: &StewardRun,
    correlation_id: &str,
    phase: PendingCancellationPhase,
    opt_out_label: &str,
) -> Result<PendingCancellation, String> {
    let pr_number = run
        .pull_request_number
        .or_else(|| merge_group_pr_number(run))
        .ok_or_else(|| {
            format!(
                "workflow run {} has no pull-request identity; refusing an unaudited cancellation",
                run.id
            )
        })?;
    Ok(PendingCancellation {
        repo: context.observation.repo.clone(),
        base: context.observation.base.clone(),
        run_id: run.id,
        workflow_id: run.workflow_id,
        run_attempt: run.run_attempt,
        head_sha: run.head_sha.clone(),
        head_branch: run.head_branch.clone(),
        pr_number,
        front_head: front_head.to_owned(),
        initiated_at: Utc::now().to_rfc3339(),
        phase,
        mutation_correlation_id: correlation_id.to_owned(),
        mutation_kind: PendingMutationKind::NormalCancel,
        reason: cancellation_reason_label(context.cancellation.reason),
        opt_out_label: opt_out_label.to_owned(),
        managed_label: context.managed_label.to_owned(),
        handoff_context: context.handoff_context.to_owned(),
    })
}

pub(super) fn pending_cancellation_key(pending: &PendingCancellation) -> String {
    pending_cancellation_key_parts(
        &pending.repo,
        pending.run_id,
        &pending.head_sha,
        &pending.front_head,
    )
}

pub(super) fn pending_cancellation_key_parts(
    repo: &str,
    run_id: u64,
    head: &str,
    front: &str,
) -> String {
    format!("{repo}#{run_id}:{head}:{front}")
}

pub(super) fn persist_capacity_evidence(
    observation: &RepoObservation,
    cancellation: &RunCancellation,
    expected_front: &str,
    evidence: &CapacityRevalidation,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
) -> Result<(), String> {
    let final_evidence = serde_json::json!({
        "front_head": expected_front,
        "front_enqueued_at": evidence.front_enqueued_at,
        "front_jobs": evidence.front_jobs,
        "candidate_run": evidence.candidate.id,
        "candidate_head": evidence.candidate.head_sha,
        "candidate_jobs": evidence.candidate.jobs,
        "current_pr_head": evidence.current_pr_head,
    });
    record_audit(
        ledger,
        &observation.repo,
        &format!("capacity-run:{}", cancellation.run_id),
        &format!("capacity_preemption_precancel_evidence:{final_evidence}"),
    );
    save_ledger(ledger_path, ledger).map_err(|error| {
        format!(
            "could not persist final preemption evidence: {}",
            error.message
        )
    })
}
