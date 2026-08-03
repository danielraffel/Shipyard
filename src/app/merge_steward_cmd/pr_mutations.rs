use super::{
    GitHubActions, MutationApplyContext, ObservedPr, Path, RepoObservation, StewardDecision,
    StewardLedger, StewardPolicy, Value, acquire_pr_mutation_guard, attempt_key, attempts_for,
    classify_pr, enqueue_requirements_pending, gh_json, merge_queue_snapshot, parse_run,
    pull_request_with_required_checks, record_audit, save_ledger,
};

pub(super) fn mutate_pr(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    ledger: &mut StewardLedger,
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
        StewardDecision::ArmMergeQueue => enqueue_pull_request(context, pr, ledger),
        StewardDecision::RerunTransient { run_ids } => {
            mutate_transient_reruns(context, pr, policy, run_ids, ledger)
        }
        _ => (None, None),
    }
}

pub(super) fn enqueue_pull_request(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        pr,
        "runner steward enqueue pull request",
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };
    let stack = crate::stacked_pr::query_args(&context.observation.repo, pr.fact.number)
        .and_then(|args| {
            context
                .actions
                .run_gh(&args)
                .map_err(|error| format!("failed to inspect pull request stack: {error}"))
        })
        .and_then(|raw| crate::stacked_pr::parse_json(&raw));
    match stack {
        Ok(Some(stack)) => {
            let message = crate::stacked_pr::unsupported_message(pr.fact.number, &stack);
            let audit_error = guard.finish("rejected_stacked_pull_request").err();
            return (
                None,
                Some(audit_error.map_or(message.clone(), |error| {
                    format!("{message}; mutation audit also failed: {error}")
                })),
            );
        }
        Ok(None) => {}
        Err(error) => {
            let audit_error = guard.finish("stack_inspection_failed").err();
            return (
                None,
                Some(audit_error.map_or(error.clone(), |audit_error| {
                    format!("{error}; mutation audit also failed: {audit_error}")
                })),
            );
        }
    }
    let query = "mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head}){mergeQueueEntry{position}}}";
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
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("pr:{}:{}", pr.fact.number, pr.fact.head_sha),
                "enqueue_exact_head",
            );
            (Some("enqueued".to_owned()), None)
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
            Ok(false) => {
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
            Ok(true) => {}
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
        (Some(format!("reran {rerun:?}")), None)
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

pub(super) fn revalidate_transient_rerun(
    actions: &GitHubActions,
    observation: &RepoObservation,
    observed_pr: &ObservedPr,
    policy: &StewardPolicy,
    run_id: u64,
    ledger: &StewardLedger,
) -> Result<bool, String> {
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
        return Ok(false);
    };
    if !live_pr
        .fact
        .head_sha
        .eq_ignore_ascii_case(&observed_pr.fact.head_sha)
    {
        return Ok(false);
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
        return Ok(false);
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
        return Ok(false);
    };
    let conclusion = value
        .get("conclusion")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase();
    Ok(run.status.eq_ignore_ascii_case("completed")
        && run.head_sha.eq_ignore_ascii_case(&live_pr.fact.head_sha)
        && run.pull_request_number == Some(live_pr.fact.number)
        && matches!(
            conclusion.as_str(),
            "CANCELLED" | "TIMED_OUT" | "STARTUP_FAILURE" | "STALE"
        ))
}
