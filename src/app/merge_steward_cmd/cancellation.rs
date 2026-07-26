use super::{
    CancellationReport, CapacityApplyContext, DateTime, GitHubActions, MutationApplyContext,
    MutationControl, ObservedPr, PREEMPT_AFTER_SECS, Path, PrReport, QueueFrontPressure,
    RepoObservation, RepoReport, RequiredCheck, RunCancellation, RunCancellationReason,
    StewardCommandArgs, StewardLedger, StewardPolicy, Utc, acquire_run_mutation_guard,
    apply_capacity_preemption, attempts_for, classify_pr, current_pull_request_heads, mutate_pr,
    opted_out_pull_requests, plan_capacity_preemptions, plan_run_coalescing, record_audit,
    revalidate_coalescing_cancellation,
};

pub(super) fn apply_repo_plan(
    actions: &GitHubActions,
    args: &StewardCommandArgs,
    observation: &RepoObservation,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    remaining_preemptions: usize,
    mutation_control: Option<&MutationControl>,
) -> (RepoReport, bool, usize) {
    let policy = StewardPolicy {
        merge_queue: observation.merge_queue,
        native_auto_merge: observation.allow_auto_merge,
        required_checks: observation.required_checks.clone(),
        opt_out_label: args.opt_out_label.clone(),
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
    let mut unhealthy =
        (args.preempt_capacity && observation.preemption_error.is_some()) || pr_mutation_failed;
    let mut planned_cancellations = Vec::new();
    if args.coalesce {
        let current_heads = current_pull_request_heads(&observation.prs);
        let opted_out = opted_out_pull_requests(&observation.prs, &args.opt_out_label);
        planned_cancellations.extend(plan_run_coalescing(
            &observation.runs,
            &current_heads,
            &observation.merge_group_heads,
            &opted_out,
        ));
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
    for cancellation in planned_cancellations {
        let (mutation, error) = if args.apply {
            apply_run_cancellation(
                actions,
                observation,
                &cancellation,
                &args.opt_out_label,
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
                "private_free_exact_head_rest".to_owned()
            },
            required_contexts: observation
                .required_checks
                .iter()
                .map(RequiredCheck::label)
                .collect(),
            prs: reports,
            cancellations,
            errors: if args.preempt_capacity {
                observation.preemption_error.iter().cloned().collect()
            } else {
                Vec::new()
            },
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
            let (mutation, error) = if args.apply {
                mutate_pr(
                    mutation_context
                        .as_ref()
                        .expect("apply mode requires mutation control"),
                    pr,
                    policy,
                    &decision,
                    ledger,
                )
            } else {
                (None, None)
            };
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
    let current_heads = current_pull_request_heads(&observation.prs);
    let opted_out = opted_out_pull_requests(&observation.prs, &args.opt_out_label);
    plan_capacity_preemptions(
        &observation.runs,
        &current_heads,
        &opted_out,
        &observation.capacity_preemption_policy,
        &pressure,
        &attempted,
        remaining_preemptions,
    )
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
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .is_some_and(|created| {
            (Utc::now() - created.with_timezone(&Utc)).num_seconds() >= PREEMPT_AFTER_SECS
        })
}

pub(super) fn apply_run_cancellation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    cancellation: &RunCancellation,
    opt_out_label: &str,
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
            },
            opt_out_label,
            ledger,
        );
    }
    let Some(observed) = observation
        .runs
        .iter()
        .find(|run| run.id == cancellation.run_id)
    else {
        return (None, Some("planned run observation disappeared".to_owned()));
    };
    match revalidate_coalescing_cancellation(
        actions,
        observation,
        observed,
        cancellation,
        opt_out_label,
    ) {
        Ok(false) => (Some("skipped_after_live_revalidation".to_owned()), None),
        Ok(true) => {
            let guard = match acquire_run_mutation_guard(
                mutation_control,
                observation,
                observed,
                &format!("runner steward cancel run {}", cancellation.run_id),
            ) {
                Ok(guard) => guard,
                Err(error) => return (None, Some(error)),
            };
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
                Err(error) => (None, Some(error.to_string())),
            }
        }
        Err(error) => (None, Some(error)),
    }
}
