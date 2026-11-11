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
    recovery::RecoveryClearReclassification,
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
    // Settle earlier uncertain admissions first, so a pull request whose
    // enqueue GitHub plainly took or refused is not held behind it.
    let (uncertain_mutations, uncertain_reconcile_error) = match mutation_control {
        Some(control) if args.apply => match super::mutation_outcome::reconcile(
            actions,
            control,
            &observation.repo,
            &observation.base,
            ledger,
        ) {
            Ok(results) => (results, None),
            Err(error) => (
                Vec::new(),
                Some(format!("uncertain mutation reconciliation: {error}")),
            ),
        },
        _ => (Vec::new(), None),
    };
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
        approval_context: args.approval_context.clone(),
        required_checks: observation.required_checks.clone(),
        opt_out_label: args.opt_out_label.clone(),
        provenance_blocking_labels: args.provenance_blocking_labels.clone(),
        managed_label: Some(args.managed_label.clone()),
        handoff_context: args.handoff_context.clone(),
        max_transient_reruns: args.max_transient_reruns,
        ejection_guards: super::read_ejection_guards(actions, observation),
    };
    let base_poison = super::base_poison::observe(actions, observation, &policy, Utc::now());
    let (mut reports, pr_mutation_failed, listing) = apply_pr_plans_with_poison(
        actions,
        args,
        observation,
        &policy,
        ledger_path,
        ledger,
        mutation_control,
        base_poison.as_ref(),
    );
    let base_poison_report = base_poison.as_ref().map(|poison| {
        // The fast-track mutates (it cancels other runs and starts a review),
        // so it runs only under `merge_group_hold = "on"`; dry-run reports.
        let fast_track = match mutation_control {
            Some(control)
                if args.apply && poison.policy.hold == crate::base_health::PoisonHold::On =>
            {
                super::base_poison::fast_track(actions, control, observation, &policy, poison)
            }
            _ => super::base_poison::would_fast_track(poison),
        };
        super::base_poison::report(poison, &fast_track)
    });
    let pause_lines = if args.apply {
        super::poison_pause::reconcile(ledger, observation, base_poison.as_ref(), Utc::now())
    } else {
        Vec::new()
    };
    // Shadow ledger: every would-fire episode per rule, labelled when it
    // ends, and the promotion gate it feeds. Measurement only.
    let shadow = match (args.apply, mutation_control, base_poison.as_ref()) {
        (true, Some(control), Some(poison)) => {
            // Real holds and pauses, and would-be ones under dry-run, are
            // counted apart so the gate's cost side says which it measured.
            let marked = |prefix: &str| {
                reports
                    .iter()
                    .filter(|report| {
                        report.mutation.as_deref().is_some_and(|mutation| {
                            mutation.split(',').any(|part| {
                                part.starts_with(prefix) && part.contains("base_poisoned")
                            })
                        })
                    })
                    .map(|report| report.number)
                    .collect::<std::collections::BTreeSet<_>>()
            };
            let mut held = marked("held_");
            held.extend(marked("paused_"));
            let would = marked("would_");
            Some(super::shadow_ledger::record_pass(
                &control.state_dir,
                &observation.repo,
                &observation.base,
                Some(poison),
                &held,
                &would,
                Utc::now(),
            ))
        }
        _ => None,
    };
    let base_poison_report = match base_poison_report {
        Some(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "pause".to_owned(),
                    super::poison_pause::report(observation, base_poison.as_ref(), &pause_lines),
                );
                if let Some(shadow) = shadow {
                    object.insert("shadow".to_owned(), shadow);
                }
            }
            Some(value)
        }
        None if !pause_lines.is_empty() => Some(serde_json::json!({
            "pause": super::poison_pause::report(observation, None, &pause_lines),
        })),
        None => None,
    };
    let dwell_policy =
        super::dwell_pass::read_policy(actions, &observation.repo, &observation.base);
    let dwell_report = super::dwell_pass::run(
        &super::dwell_pass::DwellInput {
            observation,
            policy: &policy,
            dwell_policy: &dwell_policy,
            listing: listing.as_deref(),
            apply: args.apply,
            ledger_path,
            state_dir: mutation_control.map(|control| control.state_dir.as_path()),
            now: Utc::now(),
            deadline: args.pass_deadline,
        },
        &reports,
        ledger,
        &super::dwell_pass::GitHubDwellActs {
            actions,
            observation,
            control: mutation_control,
            approval: policy.required_checks.iter().find(|required| {
                required
                    .context
                    .eq_ignore_ascii_case(policy.approval_context.trim())
            }),
        },
    );
    // The backstop acts only on pull requests the steward declines to own, so
    // it can never contend with the enqueue path above. See `native_arm`.
    let (native_arm_status, native_arm_failed) =
        if args.arm_unqueued && super::pass_budget::deadline_passed(args.pass_deadline) {
            (
                super::native_arm::NativeArmRepoStatus {
                    policy: format!(
                        "deferred: the pass time budget is spent; nothing armed this pass ({})",
                        super::pass_budget::DEFERRED
                    ),
                    results: Vec::new(),
                },
                false,
            )
        } else if args.arm_unqueued {
            super::native_arm::apply_native_arm_backstop(
                actions,
                observation,
                &reports,
                &args.opt_out_label,
                args.apply,
                || {
                    super::native_arm::arming_authority(
                        mutation_control.expect("apply mode requires mutation control"),
                        &observation.repo,
                        &observation.base,
                    )
                },
            )
        } else {
            (
                super::native_arm::NativeArmRepoStatus {
                    policy: "disabled: pass --arm-unqueued to arm green, unqueued, unarmed pull \
                         requests the steward declines to own"
                        .to_owned(),
                    results: Vec::new(),
                },
                false,
            )
        };
    super::native_arm::annotate_carried_refusals(&native_arm_status, &mut reports);
    // Infra reruns act on every open pull request, owned or not. A dry run
    // never writes, so it reports what an enforcing pass would re-run.
    let infra_mode = match args.infra_rerun {
        super::infra_rerun::Mode::Enforce if !args.apply => super::infra_rerun::Mode::Shadow,
        mode => mode,
    };
    let mut infra_excluded = vec![args.opt_out_label.clone()];
    infra_excluded.extend(args.provenance_blocking_labels.iter().cloned());
    let (infra_rerun_status, infra_rerun_failed) = super::infra_rerun::sweep(
        &super::infra_rerun::Sweep {
            observation,
            mode: infra_mode,
            approval_context: &args.approval_context,
            excluded_labels: &infra_excluded,
            now_epoch: Utc::now().timestamp(),
            deadline: args.pass_deadline,
            ledger_path: args.apply.then_some(ledger_path),
        },
        ledger,
        &super::infra_rerun::GitHubActs {
            actions,
            repo: &observation.repo,
            required: observation
                .required_checks
                .iter()
                .map(|check| check.context.clone())
                .collect(),
            deadline: args.pass_deadline,
        },
    );
    let mut unhealthy = (args.preempt_capacity && observation.preemption_error.is_some())
        || native_arm_failed
        || infra_rerun_failed
        || pr_mutation_failed
        || recovery_witness_error.is_some()
        || terminal_handoff_reconcile_error.is_some()
        || wedge_reconcile_error.is_some()
        || wedge_observation_error.is_some()
        // Another process holding the control lock is mid-mutation, not a
        // fault: the next pass reconciles.
        || uncertain_reconcile_error
            .as_deref()
            .is_some_and(|error| !error.contains("another Shipyard process is performing"))
        || uncertain_mutations
            .iter()
            .any(|result| result.outcome == "uncertain");
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
            base_poison: base_poison_report,
            rollup_reconciliation: Vec::new(),
            uncertain_mutations,
            api_calls: None,
            rate_limit_deferred: None,
            exit_reasons: Vec::new(),
            dwell: Some(dwell_report),
            infra_rerun: Some(infra_rerun_status),
            repo: observation.repo.clone(),
            base: args.base.clone(),
            allow_auto_merge: observation.allow_auto_merge,
            merge_queue: observation.merge_queue,
            merge_path: policy.merge_path().to_owned(),
            required_contexts: observation
                .required_checks
                .iter()
                .map(RequiredCheck::label)
                .collect(),
            blocked_summary: super::blocked_on::summarize(&reports, Utc::now()),
            prs: reports,
            cancellations,
            native_auto_merge_backstop: native_arm_status,
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
                .chain(uncertain_reconcile_error)
                .chain(terminal_handoff_reconcile_error)
                .chain(wedge_reconcile_error)
                .chain(wedge_observation_error)
                .collect(),
        },
        unhealthy,
        capacity_preemptions_planned,
    )
}

/// [`apply_pr_plans_with_listing`] without the listing, for tests.
#[cfg(test)]
pub(super) fn apply_pr_plans(
    actions: &GitHubActions,
    args: &StewardCommandArgs,
    observation: &RepoObservation,
    policy: &StewardPolicy,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: Option<&MutationControl>,
) -> (Vec<PrReport>, bool) {
    let (reports, unhealthy, _) = apply_pr_plans_with_listing(
        actions,
        args,
        observation,
        policy,
        ledger_path,
        ledger,
        mutation_control,
    );
    (reports, unhealthy)
}

/// [`apply_pr_plans`], also handing back the open-pull-request listing the
/// carried-commit check read this pass, when it read one.
#[cfg(test)]
pub(super) fn apply_pr_plans_with_listing(
    actions: &GitHubActions,
    args: &StewardCommandArgs,
    observation: &RepoObservation,
    policy: &StewardPolicy,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: Option<&MutationControl>,
) -> (
    Vec<PrReport>,
    bool,
    Option<Vec<super::carried_commits::OpenPr>>,
) {
    apply_pr_plans_with_poison(
        actions,
        args,
        observation,
        policy,
        ledger_path,
        ledger,
        mutation_control,
        None,
    )
}

/// [`apply_pr_plans_with_listing`] under this pass's merge-group reading
/// ([`super::base_poison`]).
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(super) fn apply_pr_plans_with_poison(
    actions: &GitHubActions,
    args: &StewardCommandArgs,
    observation: &RepoObservation,
    policy: &StewardPolicy,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: Option<&MutationControl>,
    base_poison: Option<&super::base_poison::RepoPoison>,
) -> (
    Vec<PrReport>,
    bool,
    Option<Vec<super::carried_commits::OpenPr>>,
) {
    let mut unhealthy = false;
    let mutation_context = mutation_control.map(|mutation_control| MutationApplyContext {
        actions,
        observation,
        ledger_path,
        mutation_control,
        approval_blockers: std::cell::RefCell::default(),
        carried_commits: std::cell::RefCell::default(),
        changed_files: std::cell::RefCell::default(),
        ejection_guards: policy.ejection_guards.clone(),
        base_poison,
    });
    let mut sweep_pass = super::unapproved_admission::SweepPass::new(args.unapproved_dequeue);
    if args.apply {
        super::unapproved_admission::prune(ledger, observation);
    }
    // Rows are evaluated in `pass_budget` order, so that admission work comes
    // first and a row an earlier pass deferred is not deferred again, then
    // reported in census order.
    let census_decisions = observation
        .prs
        .iter()
        .map(|pr| {
            classify_pr(
                &pr.fact,
                policy,
                &attempts_for(ledger, &observation.repo, &pr.fact),
            )
        })
        .collect::<Vec<_>>();
    let order = super::pass_budget::processing_order(
        &observation.repo,
        &observation.base,
        &observation.prs,
        &census_decisions,
        ledger,
    );
    let mut budget = super::pass_budget::PassBudget::new(args.pass_deadline);
    let mut evaluate = |pr: &ObservedPr| {
        let attempts = attempts_for(ledger, &observation.repo, &pr.fact);
        let mut decision = classify_pr(&pr.fact, policy, &attempts);
        let mut reobserved: Option<Box<ObservedPr>> = None;
        // Every open PR on the base, owned or not, on every merge path:
        // an arm or queue entry whose current head the approval gate does
        // not admit is withdrawn before anything else acts on it. See
        // `unapproved_admission`.
        let (stale_mutation, stale_error) = if super::unapproved_admission::applies(pr, &decision) {
            super::unapproved_admission::sweep(
                actions,
                observation,
                mutation_context.as_ref(),
                args.apply,
                pr,
                &decision,
                &mut sweep_pass,
                ledger,
            )
        } else {
            (None, None)
        };
        unhealthy |= stale_error.is_some();
        let fold = |mutation: Option<String>, error: Option<String>| {
            (
                match (stale_mutation.clone(), mutation) {
                    (Some(stale), Some(next)) => Some(format!("{stale},{next}")),
                    (stale, next) => stale.or(next),
                },
                match (stale_error.clone(), error) {
                    (Some(stale), Some(next)) => Some(format!("{stale}; {next}")),
                    (stale, next) => stale.or(next),
                },
            )
        };
        if let Some(report) =
            super::poison_pause::waiting_report(ledger, observation, pr, &decision)
        {
            return report;
        }
        // While the base is poisoned a queued entry only burns a gate:
        // remove it and remember its place. See `poison_pause`.
        if !stale_mutation
            .as_deref()
            .is_some_and(|mutation| mutation.contains("dequeued_"))
            && let Some(mut report) = super::poison_pause::pause_pr(
                mutation_context.as_ref(),
                args.apply,
                observation,
                pr,
                &decision,
                base_poison,
                ledger,
            )
        {
            unhealthy |= report.error.is_some();
            (report.mutation, report.error) = fold(report.mutation, report.error);
            return report;
        }
        if let Some(mut report) =
            provenance_blocked_report(mutation_context.as_ref(), pr, policy, &decision, ledger)
        {
            unhealthy |= report.error.is_some();
            (report.mutation, report.error) = fold(report.mutation, report.error);
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
            if let Some(context) = mutation_context.as_ref() {
                let (management_mutation, management_error) =
                    reconcile_management_label(context, pr, policy, &decision, ledger);
                if let Some(management_mutation) = management_mutation {
                    mutation = Some(mutation.map_or(management_mutation.clone(), |prior| {
                        format!("{prior},{management_mutation}")
                    }));
                }
                error = management_error;
            } else {
                error = Some("management mutation requires mutation control".to_owned());
            }
        }
        if args.apply && error.is_none() {
            if let Some(context) = mutation_context.as_ref() {
                let (recovery_mutation, recovery_error) =
                    reconcile_recovery_signal(context, pr, policy, &decision, ledger);
                // A clear can lift the only thing the decision waited on;
                // act on the live head now rather than a pass later.
                let reclassification = if recovery_error.is_none() {
                    super::recovery::reclassify_after_recovery_clear(
                        context,
                        pr,
                        policy,
                        &decision,
                        recovery_mutation.as_deref(),
                        ledger,
                    )
                } else {
                    RecoveryClearReclassification::Unchanged
                };
                if let Some(recovery_mutation) = recovery_mutation {
                    mutation = Some(mutation.map_or(recovery_mutation.clone(), |prior| {
                        format!("{prior},{recovery_mutation}")
                    }));
                }
                match reclassification {
                    RecoveryClearReclassification::Unchanged => {}
                    RecoveryClearReclassification::Reclassified(live, live_decision) => {
                        reobserved = Some(live);
                        decision = live_decision;
                    }
                    RecoveryClearReclassification::ReadFailed(read_error) => {
                        let note = format!("recovery_clear_reread_failed:{read_error}");
                        mutation =
                            Some(mutation.map_or(note.clone(), |prior| format!("{prior},{note}")));
                    }
                }
                error = recovery_error;
            } else {
                error = Some("recovery mutation requires mutation control".to_owned());
            }
            if args.wake_owner
                && error.is_none()
                && super::recovery::should_wake_owner(
                    &observation.repo,
                    pr,
                    &decision,
                    ledger,
                    chrono::Utc::now(),
                )
            {
                if let Some(context) = mutation_context.as_ref() {
                    match super::recovery::wake_owner_comment(context, pr, &decision, ledger) {
                        Ok(action) => {
                            mutation = Some(
                                mutation
                                    .map_or(action.clone(), |prior| format!("{prior},{action}")),
                            );
                        }
                        Err(wake_error) => error = Some(wake_error),
                    }
                } else {
                    error = Some("owner wake requires mutation control".to_owned());
                }
            }
        }
        let pr = reobserved.as_deref().unwrap_or(pr);
        if args.apply
            && error.is_none()
            && pr.fact.auto_merge_active
            && super::exact_head_merge::owns_for_disarm(&decision)
        {
            // An armed request survives a later push by anyone with
            // write access, and the steward admits an owned head itself,
            // so none is left armed on an owned PR on any base, approved
            // or not. See `exact_head_merge`.
            if let Some(context) = mutation_context.as_ref() {
                let (disarm_mutation, disarm_error) =
                    super::exact_head_merge::disarm_owned_auto_merge(context, pr, ledger);
                if let Some(disarm_mutation) = disarm_mutation {
                    mutation = Some(mutation.map_or(disarm_mutation.clone(), |prior| {
                        format!("{prior},{disarm_mutation}")
                    }));
                }
                error = disarm_error;
            } else {
                error = Some("auto-merge disarm requires mutation control".to_owned());
            }
        }
        if args.apply && error.is_none() {
            if let Some(context) = mutation_context.as_ref() {
                let (pr_mutation, pr_error) = mutate_pr_with_recovery(
                    context,
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
            } else {
                error = Some("PR mutation requires mutation control".to_owned());
            }
        }
        unhealthy |= error.is_some();
        let (mutation, error) = fold(mutation, error);
        PrReport {
            number: pr.fact.number,
            head_sha: pr.fact.head_sha.clone(),
            decision,
            mutation,
            error,
            blocked: super::blocked_on::Stall::default(),
        }
    };
    let mut indexed = Vec::with_capacity(order.len());
    for index in order {
        let pr = &observation.prs[index];
        // Past the budget nothing more is read or written for any row: the
        // row keeps its census decision and is never admitted this pass.
        let report = match budget.exhausted() {
            Some(why) => {
                super::pass_budget::deferred_report(pr, census_decisions[index].clone(), why)
            }
            None => evaluate(pr),
        };
        budget.observe(&report);
        indexed.push((index, report));
    }
    indexed.sort_by_key(|(index, _)| *index);
    let mut reports = indexed
        .into_iter()
        .map(|(_, report)| report)
        .collect::<Vec<_>>();
    super::pass_budget::record_deferred(ledger, &observation.repo, &reports);
    let listing = mutation_context
        .as_ref()
        .and_then(|context| context.carried_commits.borrow_mut().take_open());
    let approval_blockers = mutation_context
        .map(|context| context.approval_blockers.take())
        .unwrap_or_default();
    super::blocked_on::annotate(
        actions,
        observation,
        policy,
        ledger,
        &approval_blockers,
        !args.apply,
        &mut reports,
    );
    (reports, unhealthy, listing)
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
        blocked: super::blocked_on::Stall::default(),
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
