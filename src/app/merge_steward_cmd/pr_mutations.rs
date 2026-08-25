use super::{
    GitHubActions, MutationApplyContext, ObservedPr, Path, RepoObservation, StewardDecision,
    StewardLedger, StewardPolicy, Value, acquire_pr_mutation_guard, attempt_key, attempts_for,
    classify_pr, enqueue_requirements_pending, gh_json, merge_queue_snapshot, parse_run,
    pull_request_with_required_checks,
    queue_priority_recovery::{
        QueueRecoveryEvidence, final_mutable_authority_matches, receipt_key, recovery_evidence,
    },
    record_audit, save_ledger,
};

#[cfg(test)]
pub(super) fn mutate_pr(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    mutate_pr_with_recovery(context, pr, policy, decision, ledger, false)
}

pub(super) fn mutate_pr_with_recovery(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    ledger: &mut StewardLedger,
    recover_hosted_setup_eviction_priority: bool,
) -> (Option<String>, Option<String>) {
    if !matches!(
        decision,
        StewardDecision::ArmMergeQueue | StewardDecision::RerunTransient { .. }
    ) {
        return (None, None);
    }
    let queue_positions = match merge_queue_snapshot(
        context.actions,
        &context.observation.repo,
        &context.observation.base,
    ) {
        Ok((enabled, positions, _, _)) if enabled == policy.merge_queue => positions,
        Ok(_) => {
            return (
                Some("skipped_after_queue_capability_change".to_owned()),
                None,
            );
        }
        Err(error) => return (None, Some(error)),
    };
    let live_pr = match pull_request_with_required_checks(
        context.actions,
        &context.observation.repo,
        pr.fact.number,
        &context.observation.base,
        &queue_positions,
        &policy.required_checks,
    ) {
        Ok(Some(live_pr)) => live_pr,
        Ok(None) => return (Some("skipped_after_live_revalidation".to_owned()), None),
        Err(error) => return (None, Some(error)),
    };
    let attempts = attempts_for(ledger, &context.observation.repo, &live_pr.fact);
    if classify_pr(&live_pr.fact, policy, &attempts) != *decision {
        return (Some("skipped_after_live_revalidation".to_owned()), None);
    }
    let pr = &live_pr;
    match decision {
        StewardDecision::ArmMergeQueue => enqueue_pull_request_with_recovery(
            context,
            pr,
            policy,
            ledger,
            recover_hosted_setup_eviction_priority,
        ),
        StewardDecision::RerunTransient { run_ids } => {
            mutate_transient_reruns(context, pr, policy, run_ids, ledger)
        }
        _ => (None, None),
    }
}

#[cfg(test)]
pub(super) fn enqueue_pull_request(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    enqueue_pull_request_with_recovery(context, pr, policy, ledger, false)
}

fn enqueue_pull_request_with_recovery(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    ledger: &mut StewardLedger,
    recover_hosted_setup_eviction_priority: bool,
) -> (Option<String>, Option<String>) {
    let recovery = if recover_hosted_setup_eviction_priority {
        recovery_evidence(context.actions, context.observation, pr, ledger).unwrap_or_else(|_| {
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("pr:{}:{}", pr.fact.number, pr.fact.head_sha),
                "queue_priority_recovery_unreadable_fell_back",
            );
            None
        })
    } else {
        None
    };
    let guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        pr,
        if recovery.is_some() {
            "runner steward restore hosted setup eviction priority"
        } else {
            "runner steward enqueue pull request"
        },
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };
    match inspect_pull_request_stack(context, pr) {
        Ok(inspection) => {
            let message = crate::stacked_pr::ensure_unstacked(
                &context.observation.repo,
                pr.fact.number,
                &pr.fact.head_sha,
                &inspection,
            )
            .err();
            let Some(message) = message else {
                // The exact-head observation proved this is an ordinary PR.
                // Continue through the unchanged enqueue path below.
                return enqueue_unstacked_pull_request(
                    context,
                    pr,
                    policy,
                    ledger,
                    guard,
                    recovery.as_ref(),
                );
            };
            let audit_error = guard.finish("rejected_stacked_pull_request").err();
            (
                None,
                Some(audit_error.map_or(message.clone(), |error| {
                    format!("{message}; mutation audit also failed: {error}")
                })),
            )
        }
        Err(error) => {
            let audit_error = guard.finish("stack_inspection_failed").err();
            (
                None,
                Some(audit_error.map_or(error.clone(), |audit_error| {
                    format!("{error}; mutation audit also failed: {audit_error}")
                })),
            )
        }
    }
}

#[allow(clippy::too_many_lines)] // One guarded transaction keeps revalidation and its receipt adjacent.
fn enqueue_unstacked_pull_request(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    ledger: &mut StewardLedger,
    guard: crate::merge_queue_control::MergeQueueMutationGuard,
    recovery: Option<&QueueRecoveryEvidence>,
) -> (Option<String>, Option<String>) {
    let pr = match final_enqueue_revalidation(context, pr, policy, ledger) {
        Ok(Some(pr)) => pr,
        Ok(None) => {
            return match guard.finish("skipped_after_final_live_revalidation") {
                Ok(()) => (
                    Some("skipped_after_final_live_revalidation".to_owned()),
                    None,
                ),
                Err(error) => (
                    Some("skipped_after_final_live_revalidation".to_owned()),
                    Some(format!("enqueue skip mutation audit failed: {error}")),
                ),
            };
        }
        Err(error) => {
            let audit_error = guard.finish("final_live_revalidation_failed").err();
            return (
                None,
                Some(audit_error.map_or(error.clone(), |audit_error| {
                    format!("{error}; mutation audit also failed: {audit_error}")
                })),
            );
        }
    };
    if let Some(observed_recovery) = recovery {
        match recovery_evidence(context.actions, context.observation, &pr, ledger) {
            Ok(Some(live_recovery)) if live_recovery == *observed_recovery => {}
            Ok(_) => {
                return match guard.finish("skipped_after_final_recovery_revalidation") {
                    Ok(()) => (
                        Some("skipped_after_final_recovery_revalidation".to_owned()),
                        None,
                    ),
                    Err(error) => (
                        Some("skipped_after_final_recovery_revalidation".to_owned()),
                        Some(format!("recovery skip mutation audit failed: {error}")),
                    ),
                };
            }
            Err(error) => {
                let audit_error = guard.finish("final_recovery_revalidation_failed").err();
                return (
                    None,
                    Some(audit_error.map_or(error.clone(), |audit_error| {
                        format!("{error}; mutation audit also failed: {audit_error}")
                    })),
                );
            }
        }
        let pr = match final_enqueue_revalidation(context, &pr, policy, ledger) {
            Ok(Some(pr)) => pr,
            Ok(None) => {
                return match guard.finish("skipped_after_final_management_authority") {
                    Ok(()) => (
                        Some("skipped_after_final_management_authority".to_owned()),
                        None,
                    ),
                    Err(error) => (
                        Some("skipped_after_final_management_authority".to_owned()),
                        Some(format!("management skip mutation audit failed: {error}")),
                    ),
                };
            }
            Err(error) => {
                let audit_error = guard.finish("final_management_authority_failed").err();
                return (
                    None,
                    Some(audit_error.map_or(error.clone(), |audit_error| {
                        format!("{error}; mutation audit also failed: {audit_error}")
                    })),
                );
            }
        };
        match final_mutable_authority_matches(
            context.actions,
            context.observation,
            &pr,
            observed_recovery,
        ) {
            Ok(true) => {}
            Ok(false) => {
                return match guard.finish("skipped_after_final_mutable_authority") {
                    Ok(()) => (
                        Some("skipped_after_final_mutable_authority".to_owned()),
                        None,
                    ),
                    Err(error) => (
                        Some("skipped_after_final_mutable_authority".to_owned()),
                        Some(format!("authority skip mutation audit failed: {error}")),
                    ),
                };
            }
            Err(error) => {
                let audit_error = guard.finish("final_mutable_authority_failed").err();
                return (
                    None,
                    Some(audit_error.map_or(error.clone(), |audit_error| {
                        format!("{error}; mutation audit also failed: {audit_error}")
                    })),
                );
            }
        }
    }
    if let Some(recovery) = recovery {
        let key = receipt_key(recovery);
        ledger.queue_recovery_receipts.insert(
            key,
            super::QueueRecoveryReceipt {
                repo: context.observation.repo.clone(),
                base: context.observation.base.clone(),
                pr_number: pr.fact.number,
                pr_head: pr.fact.head_sha.clone(),
                base_sha: recovery.base_sha.clone(),
                merge_group_head: recovery.merge_group_head.clone(),
                removed_at: recovery.removed_at.clone(),
                run_id: recovery.run_id,
                job_id: recovery.job_id,
                attempted_at: chrono::Utc::now().to_rfc3339(),
                phase: super::QueueRecoveryPhase::Intent,
            },
        );
        if let Err(error) = save_ledger(context.ledger_path, ledger) {
            let message = format!(
                "could not persist queue-recovery receipt: {}",
                error.message
            );
            let audit_error = guard.finish("recovery_receipt_persistence_failed").err();
            return (
                None,
                Some(audit_error.map_or(message.clone(), |audit_error| {
                    format!("{message}; mutation audit also failed: {audit_error}")
                })),
            );
        }
        let post_receipt_pr = match final_enqueue_revalidation(context, &pr, policy, ledger) {
            Ok(Some(pr)) => pr,
            Ok(None) => {
                return match guard.finish("skipped_after_recovery_receipt_authority") {
                    Ok(()) => (
                        Some("skipped_after_recovery_receipt_authority".to_owned()),
                        None,
                    ),
                    Err(error) => (
                        Some("skipped_after_recovery_receipt_authority".to_owned()),
                        Some(format!("post-receipt skip mutation audit failed: {error}")),
                    ),
                };
            }
            Err(error) => {
                let audit_error = guard.finish("recovery_receipt_authority_failed").err();
                return (
                    None,
                    Some(audit_error.map_or(error.clone(), |audit_error| {
                        format!("{error}; mutation audit also failed: {audit_error}")
                    })),
                );
            }
        };
        match final_mutable_authority_matches(
            context.actions,
            context.observation,
            &post_receipt_pr,
            recovery,
        ) {
            Ok(true) => {}
            Ok(false) => {
                return match guard.finish("skipped_after_recovery_receipt_mutable_authority") {
                    Ok(()) => (
                        Some("skipped_after_recovery_receipt_mutable_authority".to_owned()),
                        None,
                    ),
                    Err(error) => (
                        Some("skipped_after_recovery_receipt_mutable_authority".to_owned()),
                        Some(format!("post-receipt authority audit failed: {error}")),
                    ),
                };
            }
            Err(error) => {
                let audit_error = guard
                    .finish("recovery_receipt_mutable_authority_failed")
                    .err();
                return (
                    None,
                    Some(audit_error.map_or(error.clone(), |audit_error| {
                        format!("{error}; mutation audit also failed: {audit_error}")
                    })),
                );
            }
        }
    }
    let query = if recovery.is_some() {
        "mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head,jump:true}){mergeQueueEntry{position}}}"
    } else {
        "mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head}){mergeQueueEntry{position}}}"
    };
    let result = context.actions.run_gh(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
        "-F".to_owned(),
        format!("id={}", pr.node_id),
        "-F".to_owned(),
        format!("head={}", pr.fact.head_sha),
    ]);
    match result {
        Ok(raw)
            if serde_json::from_str::<Value>(&raw).is_ok_and(|value| {
                value
                    .pointer("/data/enqueuePullRequest/mergeQueueEntry")
                    .is_some_and(|entry| !entry.is_null())
                    && value.get("errors").is_none()
            }) =>
        {
            if let Err(error) = guard.finish("enqueued") {
                return (
                    Some("enqueued".to_owned()),
                    Some(format!(
                        "enqueue succeeded but mutation audit failed: {error}"
                    )),
                );
            }
            let action = if let Some(recovery) = recovery {
                if let Some(receipt) = ledger
                    .queue_recovery_receipts
                    .get_mut(&receipt_key(recovery))
                {
                    receipt.phase = super::QueueRecoveryPhase::Accepted;
                }
                "enqueue_exact_head_jump_hosted_setup_eviction"
            } else {
                "enqueue_exact_head"
            };
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("pr:{}:{}", pr.fact.number, pr.fact.head_sha),
                action,
            );
            if recovery.is_some() {
                if let Err(error) = save_ledger(context.ledger_path, ledger) {
                    return (
                        Some("enqueued_priority_restored".to_owned()),
                        Some(format!(
                            "priority restoration succeeded but receipt persistence failed: {}",
                            error.message
                        )),
                    );
                }
                (Some("enqueued_priority_restored".to_owned()), None)
            } else {
                (Some("enqueued".to_owned()), None)
            }
        }
        Ok(raw) => {
            let audit_error = guard
                .finish("rejected")
                .err()
                .map_or_else(String::new, |error| {
                    format!("; mutation audit also failed: {error}")
                });
            (
                None,
                Some(format!(
                    "GitHub enqueue returned no mergeQueueEntry: {raw}{audit_error}"
                )),
            )
        }
        Err(error) => {
            let message = error.to_string();
            if enqueue_requirements_pending(&message) {
                match guard.finish("rejected_requirements") {
                    Ok(()) => (Some("waiting_enqueue_requirements".to_owned()), None),
                    Err(error) => (
                        Some("waiting_enqueue_requirements".to_owned()),
                        Some(format!(
                            "enqueue requirements rejected but mutation audit failed: {error}"
                        )),
                    ),
                }
            } else {
                (None, Some(message))
            }
        }
    }
}

fn final_enqueue_revalidation(
    context: &MutationApplyContext<'_>,
    observed: &ObservedPr,
    policy: &StewardPolicy,
    ledger: &StewardLedger,
) -> Result<Option<ObservedPr>, String> {
    let (enabled, queue_positions, _, _) = merge_queue_snapshot(
        context.actions,
        &context.observation.repo,
        &context.observation.base,
    )?;
    if enabled != policy.merge_queue {
        return Ok(None);
    }
    let Some(live) = pull_request_with_required_checks(
        context.actions,
        &context.observation.repo,
        observed.fact.number,
        &context.observation.base,
        &queue_positions,
        &policy.required_checks,
    )?
    else {
        return Ok(None);
    };
    if !live
        .fact
        .head_sha
        .eq_ignore_ascii_case(&observed.fact.head_sha)
    {
        return Ok(None);
    }
    let attempts = attempts_for(ledger, &context.observation.repo, &live.fact);
    if matches!(
        classify_pr(&live.fact, policy, &attempts),
        StewardDecision::ArmMergeQueue
    ) {
        Ok(Some(live))
    } else {
        Ok(None)
    }
}

fn inspect_pull_request_stack(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
) -> Result<crate::stacked_pr::StackInspection, String> {
    let membership_args =
        crate::stacked_pr::membership_query_args(&context.observation.repo, pr.fact.number)?;
    let membership_raw = context
        .actions
        .run_gh(&membership_args)
        .map_err(|error| format!("failed to discover pull request stack base: {error}"))?;
    let initial_stack = crate::stacked_pr::parse_membership_json(&membership_raw)?;
    let policy_ref =
        crate::stacked_pr::rollout_policy_ref(&context.observation.base, initial_stack.as_ref())?;
    let args = crate::stacked_pr::inspection_query_args(
        &context.observation.repo,
        &policy_ref,
        pr.fact.number,
    )?;
    let raw = context
        .actions
        .run_gh(&args)
        .map_err(|error| format!("failed to inspect pull request stack: {error}"))?;
    let mut inspection = crate::stacked_pr::parse_json(&raw)?;
    crate::stacked_pr::validate_policy_ref(&context.observation.base, &policy_ref, &inspection)?;
    crate::stacked_pr::apply_trusted_global_override(
        &mut inspection,
        &context.mutation_control.global_dir,
    )?;
    Ok(inspection)
}

#[allow(clippy::too_many_lines)]
pub(super) fn mutate_transient_reruns(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    run_ids: &[u64],
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let mut errors = Vec::new();
    let mut rerun = Vec::new();
    let mut exhausted = Vec::new();
    for run_id in run_ids {
        let guard = match acquire_pr_mutation_guard(
            context.mutation_control,
            context.observation,
            pr,
            &format!("runner steward rerun failed run {run_id}"),
        ) {
            Ok(guard) => guard,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let key = attempt_key(
            &context.observation.repo,
            pr.fact.number,
            &pr.fact.head_sha,
            *run_id,
        );
        *ledger.transient_attempts.entry(key.clone()).or_default() += 1;
        record_audit(
            ledger,
            &context.observation.repo,
            &format!("run:{run_id}:{}", pr.fact.head_sha),
            "rerun_transient_intent",
        );
        if let Err(error) = save_ledger(context.ledger_path, ledger) {
            let audit_error = guard.finish("intent_persistence_failed").err();
            return (
                None,
                Some(format!(
                    "could not persist transient rerun intent: {}{}",
                    error.message,
                    audit_error.map_or_else(String::new, |error| format!(
                        "; mutation audit also failed: {error}"
                    ))
                )),
            );
        }
        match revalidate_transient_rerun(
            context.actions,
            context.observation,
            pr,
            policy,
            *run_id,
            ledger,
        ) {
            Ok(TransientRerunRevalidation::Stale) => {
                if let Err(error) = guard.finish("skipped_after_live_revalidation") {
                    errors.push(format!("rerun skip mutation audit failed: {error}"));
                    continue;
                }
                if let Err(error) = rollback_transient_attempt(
                    ledger,
                    context.ledger_path,
                    &key,
                    &context.observation.repo,
                    *run_id,
                    &pr.fact.head_sha,
                    "rerun_transient_skipped_after_live_revalidation",
                ) {
                    errors.push(error);
                }
                continue;
            }
            Ok(TransientRerunRevalidation::Exhausted { accepted_attempts }) => {
                let synchronized = accepted_attempts.max(policy.max_transient_reruns);
                ledger.transient_attempts.insert(key.clone(), synchronized);
                record_audit(
                    ledger,
                    &context.observation.repo,
                    &format!("run:{run_id}:{}", pr.fact.head_sha),
                    "rerun_transient_exhausted_from_github_attempt",
                );
                if let Err(error) = save_ledger(context.ledger_path, ledger) {
                    errors.push(format!(
                        "could not persist exhausted GitHub rerun attempt: {}",
                        error.message
                    ));
                }
                if let Err(error) = guard.finish("exhausted_github_run_attempt") {
                    errors.push(format!("rerun exhaustion audit failed: {error}"));
                }
                exhausted.push(*run_id);
                continue;
            }
            Err(error) => {
                let audit_error = guard.finish("revalidation_failed").err();
                let rollback_error = audit_error.is_none().then(|| {
                    rollback_transient_attempt(
                        ledger,
                        context.ledger_path,
                        &key,
                        &context.observation.repo,
                        *run_id,
                        &pr.fact.head_sha,
                        "rerun_transient_revalidation_failed",
                    )
                    .err()
                });
                errors.push(format!(
                    "{error}{}{}",
                    audit_error.map_or_else(String::new, |error| format!(
                        "; mutation audit also failed: {error}"
                    )),
                    rollback_error
                        .flatten()
                        .map_or_else(String::new, |error| format!("; {error}"))
                ));
                continue;
            }
            Ok(TransientRerunRevalidation::Eligible) => {}
        }
        match context
            .actions
            .rerun_failed_run(&context.observation.repo, *run_id)
        {
            Ok(()) => {
                if let Err(error) = guard.finish("rerun_accepted") {
                    errors.push(format!(
                        "rerun accepted for run {run_id}, but mutation audit failed: {error}"
                    ));
                    continue;
                }
                rerun.push(*run_id);
                record_audit(
                    ledger,
                    &context.observation.repo,
                    &format!("run:{run_id}:{}", pr.fact.head_sha),
                    "rerun_transient",
                );
            }
            Err(error) => errors.push(error.to_string()),
        }
    }
    if errors.is_empty() {
        let mutation = match (rerun.is_empty(), exhausted.is_empty()) {
            (false, true) => format!("reran {rerun:?}"),
            (true, false) => format!("transient rerun budget exhausted for {exhausted:?}"),
            (false, false) => {
                format!("reran {rerun:?}; transient rerun budget exhausted for {exhausted:?}")
            }
            (true, true) => "no transient reruns remained eligible".to_owned(),
        };
        (Some(mutation), None)
    } else {
        (None, Some(errors.join("; ")))
    }
}

pub(super) fn rollback_transient_attempt(
    ledger: &mut StewardLedger,
    ledger_path: &Path,
    key: &str,
    repo: &str,
    run_id: u64,
    head_sha: &str,
    action: &str,
) -> Result<(), String> {
    let prior = ledger.transient_attempts.get(key).copied().unwrap_or(0);
    if prior <= 1 {
        ledger.transient_attempts.remove(key);
    } else {
        ledger
            .transient_attempts
            .insert(key.to_owned(), prior.saturating_sub(1));
    }
    record_audit(ledger, repo, &format!("run:{run_id}:{head_sha}"), action);
    if let Err(error) = save_ledger(ledger_path, ledger) {
        ledger.transient_attempts.insert(key.to_owned(), prior);
        return Err(format!(
            "could not persist transient rerun rollback: {}",
            error.message
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TransientRerunRevalidation {
    Eligible,
    Exhausted { accepted_attempts: u32 },
    Stale,
}

pub(super) fn revalidate_transient_rerun(
    actions: &GitHubActions,
    observation: &RepoObservation,
    observed_pr: &ObservedPr,
    policy: &StewardPolicy,
    run_id: u64,
    ledger: &StewardLedger,
) -> Result<TransientRerunRevalidation, String> {
    let (_, queue_positions, _, _) =
        merge_queue_snapshot(actions, &observation.repo, &observation.base)?;
    let Some(live_pr) = pull_request_with_required_checks(
        actions,
        &observation.repo,
        observed_pr.fact.number,
        &observation.base,
        &queue_positions,
        &policy.required_checks,
    )?
    else {
        return Ok(TransientRerunRevalidation::Stale);
    };
    if !live_pr
        .fact
        .head_sha
        .eq_ignore_ascii_case(&observed_pr.fact.head_sha)
    {
        return Ok(TransientRerunRevalidation::Stale);
    }
    let mut attempts = attempts_for(ledger, &observation.repo, &live_pr.fact);
    if let Some(count) = attempts.get_mut(&run_id) {
        *count = count.saturating_sub(1);
    }
    let eligible = matches!(
        classify_pr(&live_pr.fact, policy, &attempts),
        StewardDecision::RerunTransient { run_ids } if run_ids.contains(&run_id)
    );
    if !eligible {
        return Ok(TransientRerunRevalidation::Stale);
    }
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{}/actions/runs/{run_id}", observation.repo),
        ],
        "transient exact-run revalidation",
    )?;
    let Some(run) = parse_run(&value) else {
        return Ok(TransientRerunRevalidation::Stale);
    };
    let conclusion = value
        .get("conclusion")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase();
    if !run.status.eq_ignore_ascii_case("completed")
        || !run.head_sha.eq_ignore_ascii_case(&live_pr.fact.head_sha)
        || run.pull_request_number != Some(live_pr.fact.number)
        || !matches!(
            conclusion.as_str(),
            "CANCELLED" | "TIMED_OUT" | "STARTUP_FAILURE" | "STALE"
        )
    {
        return Ok(TransientRerunRevalidation::Stale);
    }
    if run_attempt_allows_transient_rerun(run.run_attempt, policy.max_transient_reruns) {
        Ok(TransientRerunRevalidation::Eligible)
    } else {
        Ok(TransientRerunRevalidation::Exhausted {
            accepted_attempts: u32::try_from(run.run_attempt.saturating_sub(1)).unwrap_or(u32::MAX),
        })
    }
}

/// Fence the bounded retry budget with GitHub's durable workflow-attempt
/// identity, not only the local steward ledger. GitHub keeps one run ID and
/// increments `run_attempt` after each accepted rerun. If a controller dies
/// after GitHub accepts a rerun but before its external ledger cache is saved,
/// the next controller must still refuse another accepted retry.
pub(super) fn run_attempt_allows_transient_rerun(
    run_attempt: u64,
    max_transient_reruns: u32,
) -> bool {
    let already_accepted = run_attempt.saturating_sub(1);
    already_accepted < u64::from(max_transient_reruns)
}
