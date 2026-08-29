use super::{
    CancellationReport, CapacityApplyContext, DateTime, GitHubActions, MergeQueueMutationGuard,
    MutationApplyContext, MutationControl, ObservedPr, PREEMPT_AFTER_SECS, Path, PrReport,
    QueueFrontPressure, RepoObservation, RepoReport, RequiredCheck, RunCancellation,
    RunCancellationReason, StewardCommandArgs, StewardDecision, StewardLedger, StewardPolicy,
    StewardRun, Utc, acquire_run_mutation_guard, apply_capacity_preemption, attempts_for,
    authoritative_head_still_superseded, classify_pr, coalescing_reason_authorizes,
    current_pull_request_heads, exact_run_still_queued, merge_group_pr_number,
    opted_out_pull_requests, plan_capacity_preemptions, plan_run_coalescing,
    pr_mutations::mutate_pr_with_recovery,
    pull_request_is_managed, pull_request_provenance_blocked,
    queue_priority_recovery::record_queue_witnesses,
    reconcile_management_label, reconcile_recovery_signal, record_audit,
    revalidate_coalescing_cancellation,
    terminal_handoff::{
        reconcile_queued_success_continuation, resolve_superseded_terminal_handoffs,
    },
};

#[allow(clippy::too_many_lines)]
pub(super) fn apply_repo_plan(
    actions: &GitHubActions,
    args: &StewardCommandArgs,
    observation: &RepoObservation,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    remaining_preemptions: usize,
    mutation_control: Option<&MutationControl>,
) -> (RepoReport, bool, usize) {
    let wedge_reconcile_error = if args.apply && args.coalesce {
        super::stale_pr_wedge::reconcile_receipts(
            actions,
            &observation.repo,
            ledger_path,
            ledger,
            mutation_control.expect("apply mode requires mutation control"),
        )
        .err()
    } else {
        None
    };
    let wedge_observation = if args.coalesce {
        super::stale_pr_wedge::observe_candidates(actions, observation)
    } else {
        Ok(Vec::new())
    };
    let wedge_observation_error = wedge_observation.as_ref().err().cloned();
    let observed_wedge_candidates = wedge_observation.unwrap_or_default();
    let reserved_wedge_run_ids = super::stale_pr_wedge::reserved_run_ids(
        &observation.repo,
        &observed_wedge_candidates,
        ledger,
    );
    let wedge_candidates =
        super::stale_pr_wedge::dedupe_candidates(observed_wedge_candidates, ledger);
    let terminal_handoff_reconcile_error = if args.apply {
        let current_heads = observation
            .prs
            .iter()
            .map(|pr| (pr.fact.number, pr.fact.head_sha.clone()))
            .collect();
        resolve_superseded_terminal_handoffs(
            ledger_path,
            ledger,
            &observation.repo,
            &observation.base,
            &current_heads,
        )
        .err()
        .map(|error| error.message)
    } else {
        None
    };
    let recovery_witness_error = if args.apply && args.recover_hosted_setup_eviction_priority {
        record_queue_witnesses(actions, observation, args, ledger_path, ledger).err()
    } else {
        None
    };
    let policy = StewardPolicy {
        merge_queue: observation.merge_queue,
        native_auto_merge: observation.allow_auto_merge,
        required_checks: observation.required_checks.clone(),
        opt_out_label: args.opt_out_label.clone(),
        provenance_blocking_labels: args.provenance_blocking_labels.clone(),
        managed_label: Some(args.managed_label.clone()),
        handoff_context: args.handoff_context.clone(),
        max_transient_reruns: args.max_transient_reruns,
    };
    let (reports, pr_mutation_failed) = apply_pr_plans(
        actions,
        args,
        observation,
        &policy,
        ledger_path,
        ledger,
        mutation_control,
    );
    let mut unhealthy = (args.preempt_capacity && observation.preemption_error.is_some())
        || pr_mutation_failed
        || recovery_witness_error.is_some()
        || terminal_handoff_reconcile_error.is_some()
        || wedge_reconcile_error.is_some()
        || wedge_observation_error.is_some();
    let mut planned_cancellations = Vec::new();
    if args.coalesce {
        let current_heads = current_pull_request_heads(&observation.prs);
        let opted_out = excluded_pull_requests(
            observation,
            &args.opt_out_label,
            &args.managed_label,
            &args.handoff_context,
            &args.provenance_blocking_labels,
        );
        planned_cancellations.extend(plan_run_coalescing(
            &observation.runs,
            &current_heads,
            &observation.merge_group_heads,
            &opted_out,
        ));
        planned_cancellations
            .retain(|cancellation| !reserved_wedge_run_ids.contains(&cancellation.run_id));
    }
    planned_cancellations.extend(plan_repo_capacity_preemptions(
        args,
        observation,
        ledger,
        remaining_preemptions,
    ));
    let capacity_preemptions_planned = planned_cancellations
        .iter()
        .filter(|cancellation| {
            matches!(
                cancellation.reason,
                RunCancellationReason::AdvisoryPreambleCapacityTheft
                    | RunCancellationReason::LowerPriorityBranchPreamble
            )
        })
        .count();
    let mut cancellations = Vec::new();
    if args.apply
        && let Some(candidate) = wedge_candidates.first()
    {
        let report = super::stale_pr_wedge::apply_candidate(
            actions,
            observation,
            candidate,
            &args.opt_out_label,
            &args.managed_label,
            &args.handoff_context,
            &args.provenance_blocking_labels,
            ledger_path,
            ledger,
            mutation_control.expect("apply mode requires mutation control"),
        );
        unhealthy |= report.error.is_some();
        cancellations.push(report);
    }
    for cancellation in planned_cancellations {
        let (mutation, error) = if args.apply {
            apply_run_cancellation(
                actions,
                observation,
                &cancellation,
                &args.opt_out_label,
                &args.managed_label,
                &args.handoff_context,
                &args.provenance_blocking_labels,
                ledger_path,
                ledger,
                mutation_control.expect("apply mode requires mutation control"),
            )
        } else {
            (None, None)
        };
        if error.is_some() {
            unhealthy = true;
        }
        cancellations.push(CancellationReport {
            run_id: cancellation.run_id,
            reason: cancellation_reason_label(cancellation.reason),
            mutation,
            error,
        });
    }
    (
        RepoReport {
            repo: observation.repo.clone(),
            base: args.base.clone(),
            allow_auto_merge: observation.allow_auto_merge,
            merge_queue: observation.merge_queue,
            merge_path: if observation.merge_queue {
                "native_queue_exact_head".to_owned()
            } else {
                "direct_merge_refused_server_enforcement_required".to_owned()
            },
            required_contexts: observation
                .required_checks
                .iter()
                .map(RequiredCheck::label)
                .collect(),
            prs: reports,
            cancellations,
            stale_pr_run_wedge: super::stale_pr_wedge::repo_status(
                Some(observation),
                wedge_candidates,
                ledger,
                &observation.repo,
            ),
            errors: observation
                .preemption_error
                .iter()
                .filter(|_| args.preempt_capacity)
                .cloned()
                .chain(recovery_witness_error)
                .chain(terminal_handoff_reconcile_error)
                .chain(wedge_reconcile_error)
                .chain(wedge_observation_error)
                .collect(),
        },
        unhealthy,
        capacity_preemptions_planned,
    )
}

pub(super) fn apply_pr_plans(
    actions: &GitHubActions,
    args: &StewardCommandArgs,
    observation: &RepoObservation,
    policy: &StewardPolicy,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: Option<&MutationControl>,
) -> (Vec<PrReport>, bool) {
    let mut unhealthy = false;
    let mutation_context = mutation_control.map(|mutation_control| MutationApplyContext {
        actions,
        observation,
        ledger_path,
        mutation_control,
    });
    let reports = observation
        .prs
        .iter()
        .map(|pr| {
            let attempts = attempts_for(ledger, &observation.repo, &pr.fact);
            let decision = classify_pr(&pr.fact, policy, &attempts);
            if let Some(report) =
                provenance_blocked_report(mutation_context.as_ref(), pr, policy, &decision, ledger)
            {
                unhealthy |= report.error.is_some();
                return report;
            }
            let (mut mutation, mut error) = (None, None);
            if args.apply && matches!(decision, StewardDecision::Queued { .. }) {
                match reconcile_queued_success_continuation(
                    ledger_path,
                    ledger,
                    &observation.repo,
                    &observation.base,
                    pr.fact.number,
                    &pr.fact.head_sha,
                ) {
                    Ok(true) => mutation = Some("success_continuation_reconciled".to_owned()),
                    Ok(false) => {}
                    Err(failure) => error = Some(failure.message),
                }
            }
            if args.apply && error.is_none() {
                let (management_mutation, management_error) = reconcile_management_label(
                    mutation_context
                        .as_ref()
                        .expect("apply mode requires mutation control"),
                    pr,
                    policy,
                    &decision,
                    ledger,
                );
                if let Some(management_mutation) = management_mutation {
                    mutation = Some(mutation.map_or(management_mutation.clone(), |prior| {
                        format!("{prior},{management_mutation}")
                    }));
                }
                error = management_error;
            }
            if args.apply && error.is_none() {
                let (recovery_mutation, recovery_error) = reconcile_recovery_signal(
                    mutation_context
                        .as_ref()
                        .expect("apply mode requires mutation control"),
                    pr,
                    policy,
                    &decision,
                    ledger,
                );
                if let Some(recovery_mutation) = recovery_mutation {
                    mutation = Some(mutation.map_or(recovery_mutation.clone(), |prior| {
                        format!("{prior},{recovery_mutation}")
                    }));
                }
                error = recovery_error;
            }
            if args.apply && error.is_none() {
                let (pr_mutation, pr_error) = mutate_pr_with_recovery(
                    mutation_context
                        .as_ref()
                        .expect("apply mode requires mutation control"),
                    pr,
                    policy,
                    &decision,
                    ledger,
                    args.recover_hosted_setup_eviction_priority,
                );
                if let Some(pr_mutation) = pr_mutation {
                    mutation = Some(mutation.map_or(pr_mutation.clone(), |prior| {
                        format!("{prior},{pr_mutation}")
                    }));
                }
                error = pr_error;
            }
            unhealthy |= error.is_some();
            PrReport {
                number: pr.fact.number,
                head_sha: pr.fact.head_sha.clone(),
                decision,
                mutation,
                error,
            }
        })
        .collect();
    (reports, unhealthy)
}

fn provenance_blocked_report(
    mutation_context: Option<&MutationApplyContext<'_>>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    ledger: &mut StewardLedger,
) -> Option<PrReport> {
    if !matches!(decision, StewardDecision::ProvenanceBlocked { .. }) {
        return None;
    }
    let (mutation, error) = mutation_context.map_or((None, None), |context| {
        reconcile_recovery_signal(context, pr, policy, decision, ledger)
    });
    Some(PrReport {
        number: pr.fact.number,
        head_sha: pr.fact.head_sha.clone(),
        decision: decision.clone(),
        mutation,
        error,
    })
}

pub(super) fn plan_repo_capacity_preemptions(
    args: &StewardCommandArgs,
    observation: &RepoObservation,
    ledger: &StewardLedger,
    remaining_preemptions: usize,
) -> Vec<RunCancellation> {
    if !args.preempt_capacity
        || args.max_preemptions_per_head == 0
        || observation.preemption_error.is_some()
    {
        return Vec::new();
    }
    let Some(pressure) = queue_front_pressure(observation) else {
        return Vec::new();
    };
    let prefix = format!("{}:", observation.repo);
    let attempted = ledger
        .preemption_attempts
        .iter()
        .filter(|(_, count)| **count >= args.max_preemptions_per_head)
        .filter_map(|(key, _)| key.strip_prefix(&prefix).map(str::to_owned))
        .collect();
    let opted_out = excluded_pull_requests(
        observation,
        &args.opt_out_label,
        &args.managed_label,
        &args.handoff_context,
        &args.provenance_blocking_labels,
    );
    plan_capacity_preemptions(
        &observation.runs,
        &opted_out,
        &observation.capacity_preemption_policy,
        &pressure,
        &attempted,
        remaining_preemptions,
    )
}

fn excluded_pull_requests(
    observation: &RepoObservation,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
    provenance_blocking_labels: &[String],
) -> std::collections::BTreeSet<u64> {
    let mut excluded = opted_out_pull_requests(&observation.prs, opt_out_label);
    excluded.extend(
        observation
            .prs
            .iter()
            .filter(|pr| !pull_request_is_managed(pr, managed_label, handoff_context))
            .map(|pr| pr.fact.number),
    );
    excluded.extend(
        observation
            .prs
            .iter()
            .filter(|pr| pull_request_provenance_blocked(pr, provenance_blocking_labels))
            .map(|pr| pr.fact.number),
    );
    excluded
}

pub(super) fn cancellation_reason_label(reason: RunCancellationReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{reason:?}").to_ascii_lowercase())
}

pub(super) fn queue_front_pressure(observation: &RepoObservation) -> Option<QueueFrontPressure> {
    let front = queue_front_pr(observation)?;
    let head_sha = observation
        .merge_group_heads
        .get(&front.fact.number)?
        .to_owned();
    let enqueued_at = observation
        .merge_group_enqueued_at
        .get(&front.fact.number)?;
    Some(QueueFrontPressure {
        head_sha,
        old_enough: timestamp_old_enough(enqueued_at),
    })
}

pub(super) fn queue_front_head(observation: &RepoObservation) -> Option<&str> {
    let front = queue_front_pr(observation)?;
    observation
        .merge_group_heads
        .get(&front.fact.number)
        .map(String::as_str)
}

pub(super) fn queue_front_pr(observation: &RepoObservation) -> Option<&ObservedPr> {
    Some(
        observation
            .prs
            .iter()
            .filter_map(|pr| pr.fact.queue_position.map(|position| (position, pr)))
            .min_by_key(|(position, _)| *position)?
            .1,
    )
}

pub(super) fn timestamp_old_enough(timestamp: &str) -> bool {
    DateTime::parse_from_rfc3339(timestamp).is_ok_and(|created| {
        (Utc::now() - created.with_timezone(&Utc)).num_seconds() >= PREEMPT_AFTER_SECS
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_run_cancellation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    cancellation: &RunCancellation,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
    provenance_blocking_labels: &[String],
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
) -> (Option<String>, Option<String>) {
    if matches!(
        cancellation.reason,
        RunCancellationReason::AdvisoryPreambleCapacityTheft
            | RunCancellationReason::LowerPriorityBranchPreamble
    ) {
        return apply_capacity_preemption(
            &CapacityApplyContext {
                actions,
                observation,
                cancellation,
                ledger_path,
                mutation_control,
                managed_label,
                handoff_context,
                provenance_blocking_labels,
            },
            opt_out_label,
            ledger,
        );
    }
    if !coalescing_reason_authorizes(cancellation.reason) {
        return (
            Some("skipped_non_authorizing_cancellation_reason".to_owned()),
            None,
        );
    }
    let Some(observed) = observation
        .runs
        .iter()
        .find(|run| run.id == cancellation.run_id)
    else {
        return (None, Some("planned run observation disappeared".to_owned()));
    };
    apply_superseded_run_cancellation(
        actions,
        observation,
        observed,
        cancellation,
        opt_out_label,
        managed_label,
        handoff_context,
        provenance_blocking_labels,
        ledger,
        mutation_control,
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_superseded_run_cancellation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    observed: &StewardRun,
    cancellation: &RunCancellation,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
    provenance_blocking_labels: &[String],
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
) -> (Option<String>, Option<String>) {
    match revalidate_coalescing_cancellation(
        actions,
        observation,
        observed,
        cancellation,
        opt_out_label,
        managed_label,
        handoff_context,
        provenance_blocking_labels,
    ) {
        Ok(false) => (Some("skipped_after_live_revalidation".to_owned()), None),
        Ok(true) => match acquire_final_cancellation_guard(
            actions,
            observation,
            observed,
            cancellation,
            opt_out_label,
            managed_label,
            handoff_context,
            provenance_blocking_labels,
            mutation_control,
        ) {
            Ok(guard) => {
                send_superseded_cancellation(actions, observation, cancellation, ledger, guard)
            }
            Err(result) => result,
        },
        Err(error) => (None, Some(error)),
    }
}

#[allow(clippy::too_many_arguments)]
fn acquire_final_cancellation_guard(
    actions: &GitHubActions,
    observation: &RepoObservation,
    observed: &StewardRun,
    cancellation: &RunCancellation,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
    provenance_blocking_labels: &[String],
    mutation_control: &MutationControl,
) -> Result<MergeQueueMutationGuard, (Option<String>, Option<String>)> {
    let guard = acquire_run_mutation_guard(
        mutation_control,
        observation,
        observed,
        &format!("runner steward cancel run {}", cancellation.run_id),
    )
    .map_err(|error| (None, Some(error)))?;
    match revalidate_coalescing_cancellation(
        actions,
        observation,
        observed,
        cancellation,
        opt_out_label,
        managed_label,
        handoff_context,
        provenance_blocking_labels,
    ) {
        Ok(false) => {
            return Err(finish_guard_skip(
                guard,
                "skipped_after_final_live_revalidation",
            ));
        }
        Err(error) => {
            return Err(finish_guard_error(
                guard,
                "final_revalidation_failed",
                &error,
            ));
        }
        Ok(true) => {}
    }
    match authoritative_head_still_superseded(
        actions,
        observation,
        observed,
        cancellation,
        opt_out_label,
        managed_label,
        handoff_context,
        provenance_blocking_labels,
    ) {
        Ok(false) => {
            return Err(finish_guard_skip(
                guard,
                "skipped_after_final_authority_check",
            ));
        }
        Err(error) => {
            return Err(finish_guard_error(
                guard,
                "final_authority_check_failed",
                &error,
            ));
        }
        Ok(true) => {}
    }
    let Some(pr_number) = observed
        .pull_request_number
        .or_else(|| merge_group_pr_number(observed))
    else {
        let error = format!("workflow run {} lost pull-request identity", observed.id);
        return Err(finish_guard_error(
            guard,
            "final_run_identity_missing",
            &error,
        ));
    };
    match exact_run_still_queued(actions, observation, observed, cancellation, pr_number) {
        Ok(true) => Ok(guard),
        Ok(false) => Err(finish_guard_skip(
            guard,
            "skipped_after_final_exact_run_check",
        )),
        Err(error) => Err(finish_guard_error(
            guard,
            "final_exact_run_check_failed",
            &error,
        )),
    }
}

fn finish_guard_skip(
    guard: MergeQueueMutationGuard,
    outcome: &str,
) -> (Option<String>, Option<String>) {
    match guard.finish(outcome) {
        Ok(()) => (Some(outcome.to_owned()), None),
        Err(error) => (None, Some(error)),
    }
}

fn finish_guard_error(
    guard: MergeQueueMutationGuard,
    outcome: &str,
    error: &str,
) -> (Option<String>, Option<String>) {
    let audit_error = guard.finish(outcome).err();
    (
        None,
        Some(format!(
            "{error}{}",
            audit_error.map_or_else(String::new, |audit_error| format!(
                "; mutation audit also failed: {audit_error}"
            ))
        )),
    )
}

fn send_superseded_cancellation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    cancellation: &RunCancellation,
    ledger: &mut StewardLedger,
    guard: MergeQueueMutationGuard,
) -> (Option<String>, Option<String>) {
    match actions.cancel_workflow_run(&observation.repo, cancellation.run_id) {
        Ok(()) => {
            if let Err(error) = guard.finish("cancel_accepted") {
                return (
                    Some("cancelled".to_owned()),
                    Some(format!(
                        "cancel accepted but mutation audit failed: {error}"
                    )),
                );
            }
            record_audit(
                ledger,
                &observation.repo,
                &format!("run:{}", cancellation.run_id),
                "cancel_revalidated_queued_run",
            );
            (Some("cancelled".to_owned()), None)
        }
        Err(error) => {
            let message = error.to_string();
            if definitive_cancel_rejection(&message) {
                let audit_error = guard.finish("rejected").err();
                return (
                    Some("cancel_rejected".to_owned()),
                    Some(format!(
                        "{message}{}",
                        audit_error.map_or_else(String::new, |audit_error| format!(
                            "; mutation audit also failed: {audit_error}"
                        ))
                    )),
                );
            }
            (None, Some(message))
        }
    }
}

fn definitive_cancel_rejection(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("http 409")
        || lower.contains("http 422")
        || lower.contains("already completed")
        || lower.contains("cannot cancel")
        || lower.contains("can't cancel")
        || lower.contains("not in progress")
}
