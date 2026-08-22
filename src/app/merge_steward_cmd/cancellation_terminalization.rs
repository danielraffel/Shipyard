use super::ledger::persist_pending_mutation_correlation;
use super::{
    CANCEL_TERMINAL_POLL, CANCEL_TERMINAL_WAIT, CapacityApplyContext, CliFailure,
    DurableMutationIntent, GitHubActions, Instant, MergeQueueMutationGuard, MutationControl,
    NonTerminalRun, Path, PendingCancellation, PendingMutationKind, PendingRunState, ShipState,
    StewardJob, StewardLedger, StewardRun, Value, fetch_run_jobs_before, gh_json, gh_json_timeout,
    parse_job, parse_run, record_audit, revalidate_pending_pr_authority, save_ledger, thread,
};

pub(super) fn read_pending_run(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<PendingRunState, String> {
    let run = read_pending_run_identity(actions, pending)?;
    let active_jobs = fetch_pending_run_jobs(actions, pending)?
        .into_iter()
        .filter(is_active_job)
        .collect::<Vec<_>>();
    let final_run = read_pending_run_identity(actions, pending)?;
    if final_run.status != run.status {
        return Err(format!(
            "pending cancellation run {} changed status during exact observation",
            pending.run_id
        ));
    }
    if run.status.eq_ignore_ascii_case("completed") && active_jobs.is_empty() {
        Ok(PendingRunState::Terminal)
    } else {
        Ok(PendingRunState::NonTerminal(NonTerminalRun {
            status: run.status,
            jobs: active_jobs,
        }))
    }
}

pub(super) fn read_pending_run_identity(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<StewardRun, String> {
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!(
                "repos/{}/actions/runs/{}/attempts/{}",
                pending.repo, pending.run_id, pending.run_attempt
            ),
        ],
        "pending cancellation workflow run",
    )?;
    let run = parse_run(&value).ok_or_else(|| {
        format!(
            "pending cancellation run {} response is malformed",
            pending.run_id
        )
    })?;
    if run.id != pending.run_id
        || run.workflow_id != pending.workflow_id
        || run.run_attempt != pending.run_attempt
        || !run.head_sha.eq_ignore_ascii_case(&pending.head_sha)
        || run.head_branch != pending.head_branch
    {
        return Err(format!(
            "pending cancellation run {} immutable identity changed",
            pending.run_id
        ));
    }
    Ok(run)
}

pub(super) fn read_current_pending_run_identity(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<(), String> {
    if !current_pending_run_identity_matches(actions, pending)? {
        return Err(format!(
            "current workflow run {} no longer matches pending attempt {}",
            pending.run_id, pending.run_attempt
        ));
    }
    Ok(())
}

pub(super) fn current_pending_run_identity_matches(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<bool, String> {
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{}/actions/runs/{}", pending.repo, pending.run_id),
        ],
        "current pending cancellation workflow run",
    )?;
    let run = parse_run(&value).ok_or_else(|| {
        format!(
            "current pending cancellation run {} response is malformed",
            pending.run_id
        )
    })?;
    Ok(run.id == pending.run_id
        && run.workflow_id == pending.workflow_id
        && run.run_attempt == pending.run_attempt
        && run.head_sha.eq_ignore_ascii_case(&pending.head_sha)
        && run.head_branch == pending.head_branch)
}

pub(super) fn fetch_pending_run_jobs(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<Vec<StewardJob>, String> {
    let mut all = Vec::new();
    for page in 1..=10 {
        let value = gh_json(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{}/actions/runs/{}/attempts/{}/jobs?filter=all&per_page=100&page={page}",
                    pending.repo, pending.run_id, pending.run_attempt
                ),
            ],
            "pending cancellation workflow jobs",
        )?;
        let rows = value
            .get("jobs")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("workflow run {} response missing jobs", pending.run_id))?;
        let count = rows.len();
        for row in rows {
            all.push(parse_job(row)?);
        }
        if count < 100 {
            return Ok(all);
        }
    }
    Err(format!(
        "workflow run {} attempt {} exceeds 1000 jobs; refusing partial recovery scan",
        pending.run_id, pending.run_attempt
    ))
}

pub(super) fn acquire_pending_cancellation_guard(
    control: &MutationControl,
    pending: &PendingCancellation,
    action: &str,
    intent: &DurableMutationIntent,
) -> Result<MergeQueueMutationGuard, String> {
    intent.acquire(
        &control.store,
        control.mode,
        &control.global_dir,
        &pending_cancellation_ship_state(pending),
        action,
    )
}

pub(super) fn validate_pending_cancellation_authority(
    control: &MutationControl,
    pending: &PendingCancellation,
) -> Result<(), String> {
    DurableMutationIntent::resume(&pending.mutation_correlation_id)?.validate(
        &control.store,
        &control.global_dir,
        &pending_cancellation_ship_state(pending),
    )
}

pub(super) fn pending_cancellation_ship_state(pending: &PendingCancellation) -> ShipState {
    ShipState::new(
        pending.pr_number,
        &pending.repo,
        &pending.head_branch,
        &pending.base,
        &pending.head_sha,
        "runner-steward",
    )
}

pub(super) fn clear_pending_cancellation(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    key: &str,
    pending: &PendingCancellation,
    action: &str,
) -> Result<(), String> {
    let Some(record) = ledger.pending_cancellations.remove(key) else {
        return Err(format!(
            "pending cancellation record for run {} disappeared",
            pending.run_id
        ));
    };
    record_audit(
        ledger,
        &pending.repo,
        &format!("capacity-run:{}", pending.run_id),
        action,
    );
    if let Err(error) = save_ledger(ledger_path, ledger) {
        ledger.pending_cancellations.insert(key.to_owned(), record);
        return Err(format!(
            "could not persist terminal pending-cancellation state: {}",
            error.message
        ));
    }
    Ok(())
}

pub(super) fn complete_capacity_cancellation(
    context: &CapacityApplyContext<'_>,
    expected_front: &str,
    final_live: &StewardRun,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    record_audit(
        ledger,
        &context.observation.repo,
        &format!(
            "front:{expected_front}:capacity-run:{}:{}",
            context.cancellation.run_id, final_live.head_sha
        ),
        &format!(
            "capacity_preemption_accepted:{:?}",
            context.cancellation.reason
        ),
    );
    if let Err(error) = save_ledger(context.ledger_path, ledger) {
        return (
            Some("cancelled_after_job_revalidation".to_owned()),
            Some(format!(
                "cancel accepted but completion audit failed: {}",
                error.message
            )),
        );
    }
    match wait_for_run_terminalization(
        context.actions,
        &context.observation.repo,
        context.cancellation.run_id,
    ) {
        Ok(None) => {
            if let Err(error) = clear_pending_for_run(
                ledger,
                context.ledger_path,
                &context.observation.repo,
                final_live.id,
                "capacity_preemption_terminalized",
            ) {
                return (
                    Some("cancelled_terminal".to_owned()),
                    Some(format!(
                        "cancel terminalized but completion audit failed: {error}"
                    )),
                );
            }
            (Some("cancelled_terminal".to_owned()), None)
        }
        Ok(Some(active)) => {
            force_cancel_nonterminal_run(context, context.cancellation.run_id, &active, ledger)
        }
        Err(error) => {
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("capacity-run:{}", context.cancellation.run_id),
                "cancel_terminalization_unreadable",
            );
            let _ = save_ledger(context.ledger_path, ledger);
            (
                Some("cancel_terminalization_unreadable".to_owned()),
                Some(format!(
                    "cancel accepted but terminalization could not be verified: {error}"
                )),
            )
        }
    }
}

pub(super) fn force_cancel_nonterminal_run(
    context: &CapacityApplyContext<'_>,
    run_id: u64,
    active: &NonTerminalRun,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let targets = active_runner_targets(&active.jobs);
    if let Err(error) = persist_force_cancel_intent(
        ledger,
        context.ledger_path,
        &context.observation.repo,
        run_id,
        &active.status,
        &targets,
    ) {
        return (
            Some("cancel_not_terminal".to_owned()),
            Some(format!(
                "cancel_not_terminal run {run_id} active={targets}; force-cancel intent persistence failed: {}",
                error.message
            )),
        );
    }
    let intent = DurableMutationIntent::new();
    if let Err(error) =
        persist_force_cancel_correlation(context, ledger, intent.correlation_id(), run_id)
    {
        return (Some("cancel_not_terminal".to_owned()), Some(error));
    }
    let pending = ledger
        .pending_cancellations
        .values()
        .find(|pending| pending.repo == context.observation.repo && pending.run_id == run_id)
        .cloned()
        .expect("persist_force_cancel_correlation required pending record");
    let guard = match acquire_pending_cancellation_guard(
        context.mutation_control,
        &pending,
        &format!("runner steward force-cancel run {run_id}"),
        &intent,
    ) {
        Ok(guard) => guard,
        Err(error) => {
            return (
                Some("cancel_not_terminal".to_owned()),
                Some(format!(
                    "cancel_not_terminal run {run_id} active={targets}; force-cancel authority failed: {error}"
                )),
            );
        }
    };
    if let Err(error) = revalidate_force_cancel_attempt(context, ledger, run_id) {
        return reject_initial_force_cancel_revalidation(guard, context, ledger, run_id, &error);
    }
    if let Err(error) = context
        .actions
        .force_cancel_workflow_run(&context.observation.repo, run_id)
    {
        audit_force_cancel_failure(
            ledger,
            context.ledger_path,
            &context.observation.repo,
            run_id,
        );
        return (
            Some("cancel_not_terminal".to_owned()),
            Some(format!(
                "cancel_not_terminal run {run_id} active={targets}; exact force-cancel failed: {error}"
            )),
        );
    }
    if let Err(error) = guard.finish("force_cancel_accepted") {
        return (
            Some("force_cancel_accepted_unverified".to_owned()),
            Some(format!(
                "force-cancel accepted for run {run_id}, but mutation audit failed: {error}"
            )),
        );
    }
    record_audit(
        ledger,
        &context.observation.repo,
        &format!("capacity-run:{run_id}"),
        "force_cancel_accepted",
    );
    if let Err(error) = save_ledger(context.ledger_path, ledger) {
        return (
            Some("force_cancel_accepted_unverified".to_owned()),
            Some(format!(
                "force-cancel accepted for run {run_id}, but audit persistence failed: {}",
                error.message
            )),
        );
    }
    verify_force_cancel_terminalization(context, run_id, ledger)
}

fn reject_initial_force_cancel_revalidation(
    guard: MergeQueueMutationGuard,
    context: &CapacityApplyContext<'_>,
    ledger: &mut StewardLedger,
    run_id: u64,
    error: &str,
) -> (Option<String>, Option<String>) {
    let audit_error = finish_force_cancel_revalidation_failure(
        guard,
        ledger,
        context.ledger_path,
        &context.observation.repo,
        run_id,
        "force_cancel_revalidation_failed",
    )
    .err();
    (
        Some("cancel_not_terminal".to_owned()),
        Some(format!(
            "exact force-cancel attempt revalidation failed: {error}{}",
            audit_error.map_or_else(String::new, |audit_error| format!(
                "; rejection audit also failed: {audit_error}"
            ))
        )),
    )
}

pub(super) fn revalidate_force_cancel_attempt(
    context: &CapacityApplyContext<'_>,
    ledger: &StewardLedger,
    run_id: u64,
) -> Result<(), String> {
    let pending = ledger
        .pending_cancellations
        .values()
        .find(|pending| pending.repo == context.observation.repo && pending.run_id == run_id)
        .ok_or_else(|| "pending cancellation record disappeared before force-cancel".to_owned())?;
    revalidate_pending_pr_authority(context.actions, pending)?;
    read_current_pending_run_identity(context.actions, pending)
}

pub(super) fn persist_force_cancel_correlation(
    context: &CapacityApplyContext<'_>,
    ledger: &mut StewardLedger,
    correlation_id: &str,
    run_id: u64,
) -> Result<(), String> {
    let key = ledger
        .pending_cancellations
        .iter()
        .find(|(_, pending)| pending.repo == context.observation.repo && pending.run_id == run_id)
        .map(|(key, _)| key.clone())
        .ok_or_else(|| {
            format!("cancel_not_terminal run {run_id}; pending cancellation record disappeared")
        })?;
    persist_pending_mutation_correlation(
        ledger,
        context.ledger_path,
        &key,
        correlation_id,
        PendingMutationKind::ForceCancel,
        "force_cancel_intent",
    )
}

pub(super) fn verify_force_cancel_terminalization(
    context: &CapacityApplyContext<'_>,
    run_id: u64,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    match wait_for_run_terminalization(context.actions, &context.observation.repo, run_id) {
        Ok(None) => {
            match clear_pending_for_run(
                ledger,
                context.ledger_path,
                &context.observation.repo,
                run_id,
                "force_cancel_terminalized",
            ) {
                Ok(()) => (Some("force_cancelled_terminal".to_owned()), None),
                Err(error) => (
                    Some("force_cancelled_terminal".to_owned()),
                    Some(format!(
                        "force-cancel terminalized run {run_id}, but audit persistence failed: {error}"
                    )),
                ),
            }
        }
        Ok(Some(still_active)) => {
            let still_targets = active_runner_targets(&still_active.jobs);
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("capacity-run:{run_id}"),
                &format!("force_cancel_not_terminal:targets={still_targets}"),
            );
            let _ = save_ledger(context.ledger_path, ledger);
            (
                Some("force_cancel_not_terminal".to_owned()),
                Some(format!(
                    "force_cancel_not_terminal run {run_id} active={still_targets}; exact-host, exact-run recycle handoff required"
                )),
            )
        }
        Err(error) => {
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("capacity-run:{run_id}"),
                "force_cancel_terminalization_unreadable",
            );
            let audit_error = save_ledger(context.ledger_path, ledger).err();
            (
                Some("force_cancel_terminalization_unreadable".to_owned()),
                Some(format!(
                    "force-cancel accepted for run {run_id}, but terminalization is unreadable: {error}{}",
                    audit_error.map_or_else(String::new, |save_error| format!(
                        "; audit persistence also failed: {}",
                        save_error.message
                    ))
                )),
            )
        }
    }
}

pub(super) fn clear_pending_for_run(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    repo: &str,
    run_id: u64,
    action: &str,
) -> Result<(), String> {
    let (key, pending) = ledger
        .pending_cancellations
        .iter()
        .find(|(_, pending)| pending.repo == repo && pending.run_id == run_id)
        .map(|(key, pending)| (key.clone(), pending.clone()))
        .ok_or_else(|| format!("pending cancellation record for run {run_id} disappeared"))?;
    clear_pending_cancellation(ledger, ledger_path, &key, &pending, action)
}

pub(super) fn audit_force_cancel_failure(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    repo: &str,
    run_id: u64,
) {
    record_audit(
        ledger,
        repo,
        &format!("capacity-run:{run_id}"),
        "force_cancel_failed",
    );
    let _ = save_ledger(ledger_path, ledger);
}

pub(super) fn finish_force_cancel_revalidation_failure(
    guard: MergeQueueMutationGuard,
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    repo: &str,
    run_id: u64,
    action: &str,
) -> Result<(), String> {
    let mutation_audit_error = guard.finish(action).err();
    record_audit(ledger, repo, &format!("capacity-run:{run_id}"), action);
    let ledger_audit_error = save_ledger(ledger_path, ledger).err();
    match (mutation_audit_error, ledger_audit_error) {
        (None, None) => Ok(()),
        (Some(error), None) => Err(format!("mutation audit failed: {error}")),
        (None, Some(error)) => Err(format!("ledger audit failed: {}", error.message)),
        (Some(mutation), Some(ledger)) => Err(format!(
            "mutation audit failed: {mutation}; ledger audit failed: {}",
            ledger.message
        )),
    }
}

pub(super) fn persist_force_cancel_intent(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    repo: &str,
    run_id: u64,
    status: &str,
    targets: &str,
) -> Result<(), CliFailure> {
    record_audit(
        ledger,
        repo,
        &format!("capacity-run:{run_id}"),
        &format!("cancel_not_terminal:status={status}:targets={targets};force_cancel_intent"),
    );
    save_ledger(ledger_path, ledger)
}

pub(super) fn wait_for_run_terminalization(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
) -> Result<Option<NonTerminalRun>, String> {
    let deadline = Instant::now() + CANCEL_TERMINAL_WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "cancel terminalization for run {run_id} reached its deadline"
            ));
        }
        let value = gh_json_timeout(
            actions,
            &[
                "api".to_owned(),
                format!("repos/{repo}/actions/runs/{run_id}"),
            ],
            "cancel terminalization",
            remaining,
        )?;
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| "cancel terminalization response missing status".to_owned())?
            .to_owned();
        let jobs = fetch_run_jobs_before(actions, repo, run_id, deadline)?;
        let active_jobs = jobs.into_iter().filter(is_active_job).collect::<Vec<_>>();
        if status == "completed" && active_jobs.is_empty() {
            return Ok(None);
        }
        if Instant::now() + CANCEL_TERMINAL_POLL >= deadline {
            return Ok(Some(NonTerminalRun {
                status,
                jobs: active_jobs,
            }));
        }
        thread::sleep(CANCEL_TERMINAL_POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

pub(super) fn is_active_job(job: &StewardJob) -> bool {
    matches!(
        job.status.as_str(),
        "queued" | "waiting" | "pending" | "requested" | "in_progress"
    )
}

pub(super) fn active_runner_targets(jobs: &[StewardJob]) -> String {
    if jobs.is_empty() {
        return "workflow-status-only".to_owned();
    }
    jobs.iter()
        .map(|job| {
            format!(
                "{}@{}",
                job.name,
                job.runner_name.as_deref().unwrap_or("unassigned")
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}
