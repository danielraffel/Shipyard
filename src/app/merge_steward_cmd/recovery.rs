use super::{
    Duration, Instant, MutationApplyContext, NEEDS_AGENT_LABEL, ObservedPr, RECOVERY_CONTEXT,
    StewardDecision, StewardLedger, StewardPolicy, UNMANAGED_LABEL, acquire_pr_mutation_guard,
    classify_pr,
    handoff::{add_label, ensure_label, remove_label, run_steward_write},
    merge_queue_snapshot_before, pull_request_with_required_checks_before, record_audit,
};
use crate::merge_steward::StewardCheck;
use crate::recovery_worker::{RecoveryFailureFact, RecoveryRequiredCheck};
use sha2::{Digest, Sha256};

use super::recovery_worker::{
    RecoveryEnqueueDisposition, RecoveryEnqueueLease, acquire_recovery_publication_lease,
    enqueue_recovery_request, recovery_publication_is_enabled, with_recovery_clear_fence,
    with_recovery_clear_fence_held,
};
use super::terminal_handoff::{persist_actionable_failure, resolve_terminal_handoffs};

const RECOVERY_REVALIDATION_TIMEOUT: Duration = Duration::from_secs(20);

fn recovery_revalidation_deadline() -> Instant {
    Instant::now() + RECOVERY_REVALIDATION_TIMEOUT
}

pub(super) fn reconcile_management_label(
    context: &MutationApplyContext<'_>,
    observed: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let unmanaged = matches!(
        decision,
        StewardDecision::Unmanaged | StewardDecision::HandoffMissing
    );
    if management_label_is_converged(observed, unmanaged) {
        return (None, None);
    }
    let live = match revalidate_recovery_target(
        context,
        observed,
        policy,
        decision,
        ledger,
        recovery_revalidation_deadline(),
    ) {
        Ok(live) => live,
        Err(result) => return result,
    };
    if management_label_is_converged(&live, unmanaged) {
        return (None, None);
    }
    let action_name = if unmanaged {
        "runner steward label unmanaged"
    } else {
        "runner steward clear unmanaged"
    };
    let guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        &live,
        action_name,
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };
    let result = if unmanaged {
        ensure_label(
            context.actions,
            &context.observation.repo,
            UNMANAGED_LABEL,
            "6E7781",
            "Not handed to Shipyard; adopt, opt out, retarget, or close",
        )
        .and_then(|()| {
            add_label(
                context.actions,
                &context.observation.repo,
                live.fact.number,
                UNMANAGED_LABEL,
            )
        })
        .map(|()| "unmanaged_label_added".to_owned())
        .map_err(|error| error.message)
    } else {
        remove_label(
            context.actions,
            &context.observation.repo,
            live.fact.number,
            UNMANAGED_LABEL,
        )
        .map(|()| "unmanaged_label_cleared".to_owned())
        .map_err(|error| error.message)
    };
    match result {
        Ok(action) => {
            let audit_result = guard.finish(&action);
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("pr:{}:{}", live.fact.number, live.fact.head_sha),
                &action,
            );
            match audit_result {
                Ok(()) => (Some(action), None),
                Err(error) => (
                    Some(action),
                    Some(format!("management label mutation audit failed: {error}")),
                ),
            }
        }
        Err(error) => {
            let audit_error = guard.finish("management_label_failed").err();
            (
                None,
                Some(audit_error.map_or(error.clone(), |audit| {
                    format!("{error}; mutation audit also failed: {audit}")
                })),
            )
        }
    }
}

pub(super) fn reconcile_recovery_signal(
    context: &MutationApplyContext<'_>,
    observed: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let needs_agent = match decision {
        StewardDecision::NeedsUpdate { .. } | StewardDecision::RequiredFailed { .. } => true,
        StewardDecision::Unmanaged
        | StewardDecision::HandoffMissing
        | StewardDecision::OptedOut
        | StewardDecision::ProvenanceBlocked { .. }
        | StewardDecision::Draft
        | StewardDecision::InvalidHead => {
            // These exact-head states explicitly remove recovery authority.
            // Fence both model work and any previously recorded owner wake so
            // a later dispatcher cannot resurrect opted-out or invalid work.
            return revalidate_excluded_recovery_clear(context, observed, policy, decision, ledger);
        }
        _ => false,
    };
    if signal_is_converged(observed, needs_agent) {
        return if needs_agent {
            enqueue_recovery_after_revalidation(context, observed, policy, decision, ledger)
        } else {
            fence_converged_recovery_clear(context, observed, ledger)
        };
    }

    let live = match revalidate_recovery_target(
        context,
        observed,
        policy,
        decision,
        ledger,
        recovery_revalidation_deadline(),
    ) {
        Ok(live) => live,
        Err(result) => return result,
    };
    if signal_is_converged(&live, needs_agent) {
        return if needs_agent {
            enqueue_recovery_after_revalidation(context, &live, policy, decision, ledger)
        } else {
            fence_converged_recovery_clear(context, &live, ledger)
        };
    }

    let (mutation, error) = apply_recovery_signal(context, &live, decision, needs_agent, ledger);
    if !needs_agent || error.is_some() {
        return (mutation, error);
    }
    let (enqueue_mutation, enqueue_error) =
        enqueue_recovery_after_revalidation(context, &live, policy, decision, ledger);
    (combine_mutations(mutation, enqueue_mutation), enqueue_error)
}

fn revalidate_excluded_recovery_clear(
    context: &MutationApplyContext<'_>,
    observed: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    // Exclusion was observed before this mutation pass. Hold the same lease
    // that orders recovery publication while proving the exact head still has
    // the same exclusion decision, then clear both durable surfaces without
    // releasing that fence in between.
    let lease = match acquire_recovery_publication_lease(&context.mutation_control.state_dir) {
        Ok(lease) => lease,
        Err(error) => return (None, Some(error.message().to_owned())),
    };
    let live = match revalidate_recovery_target(
        context,
        observed,
        policy,
        decision,
        ledger,
        recovery_revalidation_deadline(),
    ) {
        Ok(live) => live,
        Err(result) => return result,
    };
    match with_recovery_clear_fence_held(
        &context.mutation_control.state_dir,
        &context.observation.repo,
        live.fact.number,
        &live.fact.head_sha,
        &lease,
        || {
            resolve_terminal_handoffs(
                context.ledger_path,
                ledger,
                &context.observation.repo,
                &context.observation.base,
                live.fact.number,
                &live.fact.head_sha,
            )
            .map_err(|error| error.message)
        },
    ) {
        Ok(()) => (None, None),
        Err(error) => (None, Some(error)),
    }
}

fn enqueue_recovery_after_revalidation(
    context: &MutationApplyContext<'_>,
    observed: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    // Publication and deterministic clear share this exclusive lease. Hold it
    // across the final live read and durable publication so a clear that wins
    // first is visible to revalidation, while a clear that loses waits and
    // resolves the newly published exact-head record afterward. This applies
    // even when model-driven recovery is disabled: the terminal handoff is
    // itself recovery publication.
    let publication_lease =
        match acquire_recovery_publication_lease(&context.mutation_control.state_dir) {
            Ok(lease) => lease,
            Err(error) => {
                return (
                    None,
                    Some(format!(
                        "mandatory terminal handoff publication lease failed: {}",
                        error.message()
                    )),
                );
            }
        };
    let deadline = recovery_revalidation_deadline();
    let live =
        match revalidate_recovery_target(context, observed, policy, decision, ledger, deadline) {
            Ok(live) => live,
            Err((mutation, None)) => return (mutation, None),
            Err((mutation, Some(error))) => {
                return (
                    mutation,
                    Some(format!(
                        "mandatory terminal handoff live revalidation failed: {error}"
                    )),
                );
            }
        };
    if !signal_is_converged(&live, true) {
        return (
            Some("recovery_skipped_after_signal_revalidation".to_owned()),
            None,
        );
    }
    let failure_contexts = match normalized_recovery_facts(decision, policy, &live.fact.checks) {
        Ok(Some((_, facts))) => facts.iter().map(failure_fact_component).collect(),
        Ok(None) => Vec::new(),
        Err(error) => return (None, Some(error)),
    };
    // Route corruption cannot suppress the durable failure fact. Preserve it
    // as unresolved; the future authenticated registry decides routability.
    let owner = super::handoff::terminal_owner_route_or_unresolved(
        &context.mutation_control.state_dir,
        &context.observation.repo,
        live.fact.number,
        &live.fact.head_sha,
    );
    if let Err(error) = persist_actionable_failure(
        context.ledger_path,
        ledger,
        &context.observation.repo,
        &context.observation.base,
        live.fact.number,
        &live.fact.head_sha,
        owner,
        failure_contexts,
    ) {
        return (None, Some(error.message));
    }
    match recovery_publication_is_enabled(
        &context.mutation_control.global_dir,
        &context.mutation_control.state_dir,
        &context.observation.repo,
    ) {
        Ok(true) => {}
        Ok(false) => return (Some("actionable_failure_persisted".to_owned()), None),
        Err(error) => return deferred_recovery_request(error.message()),
    }
    enqueue_recovery(context, &live, policy, decision, publication_lease)
}

fn fence_converged_recovery_clear(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    // Always fence the durable target, even when no witness exists. Enqueue
    // persists its record before its witness, and a crash in that gap must not
    // leave active work behind after deterministic stewardship has converged.
    match with_recovery_clear_fence(
        &context.mutation_control.state_dir,
        &context.observation.repo,
        pr.fact.number,
        &pr.fact.head_sha,
        || {
            resolve_terminal_handoffs(
                context.ledger_path,
                ledger,
                &context.observation.repo,
                &context.observation.base,
                pr.fact.number,
                &pr.fact.head_sha,
            )
            .map_err(|error| error.message)
        },
    ) {
        Ok(()) => (None, None),
        Err(error) => (None, Some(error)),
    }
}

fn enqueue_recovery(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    publication_lease: RecoveryEnqueueLease,
) -> (Option<String>, Option<String>) {
    let (failure_summary, failure_facts) =
        match normalized_recovery_facts(decision, policy, &pr.fact.checks) {
            Ok(Some(facts)) => facts,
            Ok(None) => return (None, None),
            Err(error) => {
                return (
                    Some(format!(
                        "recovery_request_deferred:{}",
                        truncate_description(&error)
                    )),
                    None,
                );
            }
        };
    let mut fingerprint_components = vec![failure_summary.clone()];
    fingerprint_components.extend(failure_facts.iter().map(failure_fact_component));
    let failure_fingerprint = digest_components(fingerprint_components.iter().map(String::as_str));
    let policy_signature = steward_policy_signature(policy);
    let required_checks = policy
        .required_checks
        .iter()
        .map(|required| RecoveryRequiredCheck {
            context: required.context.clone(),
            app_id: required.app_id,
        })
        .collect();
    match enqueue_recovery_request(
        &context.mutation_control.global_dir,
        &context.mutation_control.state_dir,
        publication_lease,
        &context.observation.repo,
        pr.fact.number,
        &context.observation.base,
        &pr.fact.head_sha,
        policy.merge_queue,
        &policy.opt_out_label,
        &failure_fingerprint,
        &failure_summary,
        required_checks,
        failure_facts,
        &policy_signature,
    ) {
        Ok(RecoveryEnqueueDisposition::Disabled | RecoveryEnqueueDisposition::Existing(_)) => {
            (None, None)
        }
        Ok(RecoveryEnqueueDisposition::Created(id)) => {
            (Some(format!("recovery_request_created:{id}")), None)
        }
        // Model recovery is an optional exception lane. Surface its failure in
        // the per-PR report, but never make deterministic stewardship unhealthy
        // or prevent unrelated queue progress.
        Err(error) => deferred_recovery_request(error.message()),
    }
}

fn deferred_recovery_request(error: &str) -> (Option<String>, Option<String>) {
    (
        Some(format!(
            "recovery_request_deferred:{}",
            truncate_description(error)
        )),
        None,
    )
}

fn normalized_recovery_facts(
    decision: &StewardDecision,
    policy: &StewardPolicy,
    checks: &[StewardCheck],
) -> Result<Option<(String, Vec<RecoveryFailureFact>)>, String> {
    match decision {
        StewardDecision::NeedsUpdate { merge_state } => Ok(Some((
            "pull request requires an exact-head update".to_owned(),
            vec![RecoveryFailureFact::MergeState {
                state: merge_state.to_ascii_uppercase(),
            }],
        ))),
        StewardDecision::RequiredFailed { contexts } => {
            let mut normalized = Vec::new();
            for label in contexts {
                let matches = policy
                    .required_checks
                    .iter()
                    .filter(|required| required.label() == *label)
                    .collect::<Vec<_>>();
                if matches.len() != 1 {
                    return Err(format!(
                        "required-check display label `{label}` does not map to exactly one structured policy identity"
                    ));
                }
                let required = matches[0];
                let selected = crate::merge_steward::selected_required_check(checks, required)
                    .ok_or_else(|| {
                        format!(
                            "required-check display label `{label}` has no selected current check"
                        )
                    })?;
                if !selected.status.eq_ignore_ascii_case("COMPLETED") {
                    return Err(format!(
                        "required-check display label `{label}` selected a non-terminal check"
                    ));
                }
                let conclusion = selected
                    .conclusion
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        format!(
                            "required-check display label `{label}` selected a completed check without a conclusion"
                        )
                    })?
                    .to_ascii_uppercase();
                if !matches!(
                    conclusion.as_str(),
                    "ACTION_REQUIRED"
                        | "CANCELLED"
                        | "FAILURE"
                        | "STALE"
                        | "STARTUP_FAILURE"
                        | "TIMED_OUT"
                ) {
                    return Err(format!(
                        "required-check display label `{label}` selected non-failing conclusion `{conclusion}`"
                    ));
                }
                normalized.push(RecoveryFailureFact::RequiredCheck {
                    context: required.context.clone(),
                    app_id: required.app_id,
                    conclusion,
                    run_id: selected.run_id,
                });
            }
            normalized.sort_by(|left, right| {
                failure_fact_component(left).cmp(&failure_fact_component(right))
            });
            normalized.dedup();
            Ok(Some((
                "one or more required checks failed".to_owned(),
                normalized,
            )))
        }
        _ => Ok(None),
    }
}

fn failure_fact_component(fact: &RecoveryFailureFact) -> String {
    match fact {
        RecoveryFailureFact::MergeState { state } => format!("merge_state:{state}"),
        RecoveryFailureFact::RequiredCheck {
            context,
            app_id,
            conclusion,
            run_id,
        } => {
            let producer =
                app_id.map_or_else(|| "unbound".to_owned(), |app_id| format!("app_id={app_id}"));
            let run = run_id.map_or_else(|| "no_run".to_owned(), |run_id| run_id.to_string());
            format!("required_check:{context}:{producer}:conclusion={conclusion}:run_id={run}")
        }
    }
}

fn steward_policy_signature(policy: &StewardPolicy) -> String {
    let mut required = policy
        .required_checks
        .iter()
        .map(|check| (check.context.as_str(), check.app_id))
        .collect::<Vec<_>>();
    required.sort();
    let mut components = vec![
        format!("merge_queue={}", policy.merge_queue),
        format!("native_auto_merge={}", policy.native_auto_merge),
        format!("opt_out_label={}", policy.opt_out_label),
        format!(
            "managed_label={}",
            policy.managed_label.as_deref().unwrap_or_default()
        ),
        format!("handoff_context={}", policy.handoff_context),
        format!("max_transient_reruns={}", policy.max_transient_reruns),
    ];
    let mut provenance_blockers = policy.provenance_blocking_labels.clone();
    provenance_blockers.sort_by_key(|label| label.to_ascii_lowercase());
    for label in provenance_blockers {
        components.push(format!("provenance_blocking_label={label}"));
    }
    for (context, app_id) in required {
        components.push("required_check".to_owned());
        components.push(context.to_owned());
        match app_id {
            Some(app_id) => {
                components.push("app_id_some".to_owned());
                components.push(app_id.to_string());
            }
            None => components.push("app_id_none".to_owned()),
        }
    }
    digest_components(components.iter().map(String::as_str))
}

fn digest_components<'a>(components: impl IntoIterator<Item = &'a str>) -> String {
    let mut digest = Sha256::new();
    for component in components {
        digest.update(component.len().to_be_bytes());
        digest.update(component.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn combine_mutations(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(format!("{left};{right}")),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

pub(super) fn revalidate_recovery_target(
    context: &MutationApplyContext<'_>,
    observed: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    ledger: &StewardLedger,
    deadline: Instant,
) -> Result<ObservedPr, (Option<String>, Option<String>)> {
    let positions = match merge_queue_snapshot_before(
        context.actions,
        &context.observation.repo,
        &context.observation.base,
        deadline,
    ) {
        Ok((enabled, positions, _, _)) if enabled == policy.merge_queue => positions,
        Ok(_) => {
            return Err((
                Some("recovery_skipped_queue_capability_change".to_owned()),
                None,
            ));
        }
        Err(error) => return Err((None, Some(error))),
    };
    let live = match pull_request_with_required_checks_before(
        context.actions,
        &context.observation.repo,
        observed.fact.number,
        &context.observation.base,
        &positions,
        &policy.required_checks,
        deadline,
    ) {
        Ok(Some(pr))
            if pr
                .fact
                .head_sha
                .eq_ignore_ascii_case(&observed.fact.head_sha) =>
        {
            pr
        }
        Ok(_) => {
            return Err((
                Some("recovery_skipped_after_live_revalidation".to_owned()),
                None,
            ));
        }
        Err(error) => return Err((None, Some(error))),
    };
    let attempts = super::attempts_for(ledger, &context.observation.repo, &live.fact);
    if classify_pr(&live.fact, policy, &attempts) != *decision {
        return Err((
            Some("recovery_skipped_after_live_revalidation".to_owned()),
            None,
        ));
    }
    Ok(live)
}

fn apply_recovery_signal(
    context: &MutationApplyContext<'_>,
    live: &ObservedPr,
    decision: &StewardDecision,
    needs_agent: bool,
    ledger: &mut StewardLedger,
) -> (Option<String>, Option<String>) {
    let guard = match acquire_pr_mutation_guard(
        context.mutation_control,
        context.observation,
        live,
        if needs_agent {
            "runner steward signal needs-agent"
        } else {
            "runner steward clear needs-agent"
        },
    ) {
        Ok(guard) => guard,
        Err(error) => return (None, Some(error)),
    };

    let result = if needs_agent {
        signal_needs_agent(context, live, decision)
    } else {
        with_recovery_clear_fence(
            &context.mutation_control.state_dir,
            &context.observation.repo,
            live.fact.number,
            &live.fact.head_sha,
            || {
                let action = clear_needs_agent(context, live)?;
                resolve_terminal_handoffs(
                    context.ledger_path,
                    ledger,
                    &context.observation.repo,
                    &context.observation.base,
                    live.fact.number,
                    &live.fact.head_sha,
                )
                .map_err(|error| error.message)?;
                Ok(action)
            },
        )
    };
    match result {
        Ok(action) => {
            let audit_result = guard.finish(&action);
            record_audit(
                ledger,
                &context.observation.repo,
                &format!("pr:{}:{}", live.fact.number, live.fact.head_sha),
                &action,
            );
            match audit_result {
                Ok(()) => (Some(action), None),
                Err(error) => (
                    Some(action),
                    Some(format!("recovery mutation audit failed: {error}")),
                ),
            }
        }
        Err(error) => {
            let audit_error = guard.finish("recovery_signal_failed").err();
            (
                None,
                Some(audit_error.map_or(error.clone(), |audit| {
                    format!("{error}; mutation audit also failed: {audit}")
                })),
            )
        }
    }
}

fn signal_needs_agent(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    decision: &StewardDecision,
) -> Result<String, String> {
    let reason = match decision {
        StewardDecision::NeedsUpdate { merge_state } => format!("Needs agent: merge {merge_state}"),
        StewardDecision::RequiredFailed { contexts } => {
            let joined = contexts.join(", ");
            truncate_description(&format!("Needs agent: {joined}"))
        }
        _ => return Err("recovery signal requested for a non-blocking decision".to_owned()),
    };
    write_recovery_status(context, pr, "failure", &reason)?;
    super::handoff::verify_exact_open_pr(
        context.actions,
        &context.observation.repo,
        pr.fact.number,
        &pr.fact.head_sha,
    )
    .map_err(|error| {
        format!(
            "recovery target changed before label mutation: {}",
            error.message()
        )
    })?;
    ensure_label(
        context.actions,
        &context.observation.repo,
        NEEDS_AGENT_LABEL,
        "B60205",
        "Shipyard requires semantic or code recovery",
    )
    .map_err(|error| error.message)?;
    super::handoff::add_label(
        context.actions,
        &context.observation.repo,
        pr.fact.number,
        NEEDS_AGENT_LABEL,
    )
    .map_err(|error| error.message)?;
    Ok("needs_agent_signaled".to_owned())
}

fn clear_needs_agent(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
) -> Result<String, String> {
    write_recovery_status(context, pr, "success", "Shipyard recovery clear")?;
    super::handoff::verify_exact_open_pr(
        context.actions,
        &context.observation.repo,
        pr.fact.number,
        &pr.fact.head_sha,
    )
    .map_err(|error| {
        format!(
            "recovery target changed before label mutation: {}",
            error.message()
        )
    })?;
    remove_label(
        context.actions,
        &context.observation.repo,
        pr.fact.number,
        NEEDS_AGENT_LABEL,
    )
    .map(|()| "needs_agent_cleared".to_owned())
    .map_err(|error| error.message)
}

fn write_recovery_status(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    state: &str,
    description: &str,
) -> Result<(), String> {
    run_steward_write(
        context.actions,
        &[
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!(
                "repos/{}/statuses/{}",
                context.observation.repo, pr.fact.head_sha
            ),
            "-f".to_owned(),
            format!("state={state}"),
            "-f".to_owned(),
            format!("context={RECOVERY_CONTEXT}"),
            "-f".to_owned(),
            format!("description={}", truncate_description(description)),
            "-f".to_owned(),
            format!(
                "target_url=https://github.com/{}/pull/{}",
                context.observation.repo, pr.fact.number
            ),
        ],
    )
    .map(|_| ())
    .map_err(|error| format!("could not write recovery status: {error}"))
}

fn latest_recovery_state(pr: &ObservedPr) -> Option<&str> {
    pr.fact
        .checks
        .iter()
        .filter(|check| check.name.eq_ignore_ascii_case(RECOVERY_CONTEXT))
        .max_by_key(|check| check.observed_at.as_deref().unwrap_or_default())
        .and_then(|check| check.conclusion.as_deref())
}

fn has_label(pr: &ObservedPr, label: &str) -> bool {
    pr.fact
        .labels
        .iter()
        .any(|value| value.eq_ignore_ascii_case(label))
}

fn management_label_is_converged(pr: &ObservedPr, unmanaged: bool) -> bool {
    has_label(pr, UNMANAGED_LABEL) == unmanaged
}

fn signal_is_converged(pr: &ObservedPr, needs_agent: bool) -> bool {
    let labelled = has_label(pr, NEEDS_AGENT_LABEL);
    let state = latest_recovery_state(pr);
    (needs_agent && labelled && state == Some("FAILURE"))
        || (!needs_agent && !labelled && state != Some("FAILURE"))
}

fn truncate_description(value: &str) -> String {
    value.chars().take(140).collect()
}

#[cfg(test)]
mod tests {
    use super::super::MANAGED_LABEL;
    use super::*;
    use crate::merge_steward::{RequiredCheck, StewardCheck, StewardPullRequest};

    fn pr(labels: Vec<&str>, recovery_states: Vec<(&str, &str)>) -> ObservedPr {
        ObservedPr {
            node_id: "PR_node".to_owned(),
            fact: StewardPullRequest {
                number: 7,
                head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                head_branch: "feature".to_owned(),
                draft: false,
                merge_state: "CLEAN".to_owned(),
                auto_merge_active: false,
                queue_position: None,
                labels: labels.into_iter().map(str::to_owned).collect(),
                checks: recovery_states
                    .into_iter()
                    .map(|(conclusion, observed_at)| StewardCheck {
                        name: RECOVERY_CONTEXT.to_owned(),
                        source: crate::merge_steward::StewardCheckSource::StatusContext,
                        app_id: None,
                        check_run_id: None,
                        status: "COMPLETED".to_owned(),
                        conclusion: Some(conclusion.to_owned()),
                        run_id: None,
                        observed_at: Some(observed_at.to_owned()),
                    })
                    .collect(),
            },
            check_rollup_maybe_truncated: false,
        }
    }

    #[test]
    fn needs_agent_signal_is_deduplicated_and_latest_status_wins() {
        let signalled = pr(
            vec![NEEDS_AGENT_LABEL],
            vec![
                ("SUCCESS", "2026-08-12T00:00:00Z"),
                ("FAILURE", "2026-08-13T00:00:00Z"),
            ],
        );
        assert!(signal_is_converged(&signalled, true));
        assert!(!signal_is_converged(&signalled, false));

        let cleared = pr(
            vec![],
            vec![
                ("FAILURE", "2026-08-12T00:00:00Z"),
                ("SUCCESS", "2026-08-13T00:00:00Z"),
            ],
        );
        assert!(signal_is_converged(&cleared, false));
        assert!(!signal_is_converged(&cleared, true));
    }

    #[test]
    fn status_without_matching_label_is_repaired_not_treated_as_converged() {
        assert!(!signal_is_converged(
            &pr(vec![], vec![("FAILURE", "2026-08-13T00:00:00Z")]),
            true
        ));
        assert!(!signal_is_converged(
            &pr(
                vec![NEEDS_AGENT_LABEL],
                vec![("SUCCESS", "2026-08-13T00:00:00Z")]
            ),
            false
        ));
    }

    #[test]
    fn unmanaged_label_is_deduplicated_and_cleared_after_handoff() {
        assert!(management_label_is_converged(
            &pr(vec![UNMANAGED_LABEL], vec![]),
            true
        ));
        assert!(!management_label_is_converged(&pr(vec![], vec![]), true));
        assert!(management_label_is_converged(&pr(vec![], vec![]), false));
        assert!(!management_label_is_converged(
            &pr(vec![UNMANAGED_LABEL, MANAGED_LABEL], vec![]),
            false
        ));
    }

    #[test]
    fn recovery_facts_are_normalized_without_contributor_prose() {
        let decision = StewardDecision::RequiredFailed {
            contexts: vec!["macos".to_owned(), "linux".to_owned(), "macos".to_owned()],
        };
        let policy = StewardPolicy {
            merge_queue: true,
            native_auto_merge: true,
            required_checks: vec![
                RequiredCheck {
                    context: "linux".to_owned(),
                    app_id: None,
                },
                RequiredCheck {
                    context: "macos".to_owned(),
                    app_id: None,
                },
            ],
            opt_out_label: "shipyard:no-auto-merge".to_owned(),
            provenance_blocking_labels: vec!["5·unresolved".to_owned()],
            managed_label: Some(MANAGED_LABEL.to_owned()),
            handoff_context: "shipyard/steward-handoff".to_owned(),
            max_transient_reruns: 1,
        };
        let checks = vec![
            StewardCheck {
                name: "linux".to_owned(),
                source: crate::merge_steward::StewardCheckSource::CheckRun,
                app_id: None,
                check_run_id: None,
                status: "COMPLETED".to_owned(),
                conclusion: Some("FAILURE".to_owned()),
                run_id: Some(101),
                observed_at: Some("2026-08-21T08:00:00Z".to_owned()),
            },
            StewardCheck {
                name: "macos".to_owned(),
                source: crate::merge_steward::StewardCheckSource::CheckRun,
                app_id: None,
                check_run_id: None,
                status: "COMPLETED".to_owned(),
                conclusion: Some("TIMED_OUT".to_owned()),
                run_id: None,
                observed_at: Some("2026-08-21T08:00:00Z".to_owned()),
            },
        ];
        let (summary, contexts) = normalized_recovery_facts(&decision, &policy, &checks)
            .expect("unambiguous policy")
            .expect("failure facts");
        assert_eq!(summary, "one or more required checks failed");
        assert_eq!(
            contexts,
            vec![
                RecoveryFailureFact::RequiredCheck {
                    context: "linux".to_owned(),
                    app_id: None,
                    conclusion: "FAILURE".to_owned(),
                    run_id: Some(101),
                },
                RecoveryFailureFact::RequiredCheck {
                    context: "macos".to_owned(),
                    app_id: None,
                    conclusion: "TIMED_OUT".to_owned(),
                    run_id: None,
                }
            ]
        );

        let literal = StewardDecision::RequiredFailed {
            contexts: vec!["lint (app_id=7)".to_owned()],
        };
        let literal_policy = StewardPolicy {
            required_checks: vec![RequiredCheck {
                context: "lint (app_id=7)".to_owned(),
                app_id: None,
            }],
            ..policy
        };
        let literal_checks = [StewardCheck {
            name: "lint (app_id=7)".to_owned(),
            source: crate::merge_steward::StewardCheckSource::StatusContext,
            app_id: None,
            check_run_id: None,
            status: "COMPLETED".to_owned(),
            conclusion: Some("FAILURE".to_owned()),
            run_id: None,
            observed_at: Some("2026-08-21T08:00:00Z".to_owned()),
        }];
        let (_, facts) = normalized_recovery_facts(&literal, &literal_policy, &literal_checks)
            .expect("literal label is unambiguous")
            .expect("failure facts");
        assert_eq!(
            facts,
            vec![RecoveryFailureFact::RequiredCheck {
                context: "lint (app_id=7)".to_owned(),
                app_id: None,
                conclusion: "FAILURE".to_owned(),
                run_id: None,
            }]
        );
    }

    #[test]
    fn recovery_policy_signature_is_order_independent_and_sensitive() {
        let policy = |checks: Vec<RequiredCheck>, reruns| StewardPolicy {
            merge_queue: true,
            native_auto_merge: true,
            required_checks: checks,
            opt_out_label: "shipyard:no-auto-merge".to_owned(),
            provenance_blocking_labels: vec!["5·unresolved".to_owned()],
            managed_label: Some(MANAGED_LABEL.to_owned()),
            handoff_context: "shipyard/steward-handoff".to_owned(),
            max_transient_reruns: reruns,
        };
        let linux = RequiredCheck {
            context: "linux".to_owned(),
            app_id: Some(1),
        };
        let macos = RequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
        };
        let first = steward_policy_signature(&policy(vec![linux.clone(), macos.clone()], 1));
        let reordered = steward_policy_signature(&policy(vec![macos, linux], 1));
        let changed = steward_policy_signature(&policy(Vec::new(), 2));
        let literal_display = steward_policy_signature(&policy(
            vec![RequiredCheck {
                context: "lint (app_id=7)".to_owned(),
                app_id: None,
            }],
            1,
        ));
        let structured_identity = steward_policy_signature(&policy(
            vec![RequiredCheck {
                context: "lint".to_owned(),
                app_id: Some(7),
            }],
            1,
        ));
        assert_eq!(first, reordered);
        assert_ne!(first, changed);
        assert_ne!(literal_display, structured_identity);
    }
}
