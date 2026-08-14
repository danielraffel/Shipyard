use super::{
    MutationApplyContext, NEEDS_AGENT_LABEL, ObservedPr, RECOVERY_CONTEXT, StewardDecision,
    StewardLedger, StewardPolicy, acquire_pr_mutation_guard, classify_pr, handoff::ensure_label,
    merge_queue_snapshot, pull_request_with_required_checks, record_audit,
};

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
        | StewardDecision::Draft
        | StewardDecision::InvalidHead => return (None, None),
        _ => false,
    };
    if signal_is_converged(observed, needs_agent) {
        return (None, None);
    }

    let live = match revalidate_recovery_target(context, observed, policy, decision, ledger) {
        Ok(live) => live,
        Err(result) => return result,
    };
    if signal_is_converged(&live, needs_agent) {
        return (None, None);
    }

    apply_recovery_signal(context, &live, decision, needs_agent, ledger)
}

fn revalidate_recovery_target(
    context: &MutationApplyContext<'_>,
    observed: &ObservedPr,
    policy: &StewardPolicy,
    decision: &StewardDecision,
    ledger: &StewardLedger,
) -> Result<ObservedPr, (Option<String>, Option<String>)> {
    let positions = match merge_queue_snapshot(
        context.actions,
        &context.observation.repo,
        &context.observation.base,
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
    let live = match pull_request_with_required_checks(
        context.actions,
        &context.observation.repo,
        observed.fact.number,
        &context.observation.base,
        &positions,
        &policy.required_checks,
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
        clear_needs_agent(context, live)
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
    let encoded = super::observation::encode_path_segment(NEEDS_AGENT_LABEL);
    match context.actions.run_gh(&[
        "api".to_owned(),
        "-X".to_owned(),
        "DELETE".to_owned(),
        format!(
            "repos/{}/issues/{}/labels/{encoded}",
            context.observation.repo, pr.fact.number
        ),
    ]) {
        Ok(_) => Ok("needs_agent_cleared".to_owned()),
        Err(error) if error.to_string().contains("HTTP 404") => {
            Ok("needs_agent_cleared".to_owned())
        }
        Err(error) => Err(format!("could not clear needs-agent label: {error}")),
    }
}

fn write_recovery_status(
    context: &MutationApplyContext<'_>,
    pr: &ObservedPr,
    state: &str,
    description: &str,
) -> Result<(), String> {
    context
        .actions
        .run_gh(&[
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
        ])
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
    use super::*;
    use crate::merge_steward::{StewardCheck, StewardPullRequest};

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
                        app_id: None,
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
}
