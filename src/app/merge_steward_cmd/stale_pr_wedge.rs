use super::{
    BTreeMap, BTreeSet, CancellationReport, DurableMutationIntent, GitHubActions,
    MergeQueueMutationGuard, MutationControl, RepoObservation, StalePrRunWedgeCandidate,
    StalePrRunWedgeReceipt, StalePrRunWedgeReceiptPhase, StalePrRunWedgeRepoStatus, StewardLedger,
    StewardRun, Utc, acquire_run_mutation_guard_with_correlation, fetch_run_jobs, gh_json,
    parse_run, plan_stale_pr_run_wedges, pull_request, pull_request_is_managed,
    pull_request_opted_out, pull_request_provenance_blocked, record_audit, required_checks,
    run_mutation_state, save_ledger,
};
use std::path::Path;

const POLICY_ENABLED: &str = "pulp_required_local_macos_build_and_test";
const POLICY_DISABLED: &str = "disabled_non_pulp_or_macos_policy";
const CANCELLATION_REASON: &str = "stale_pull_request_concurrency_wedge";
const MAX_WEDGE_RECEIPTS: usize = 1_000;

pub(super) fn observe_candidates(
    actions: &GitHubActions,
    observation: &RepoObservation,
) -> Result<Vec<StalePrRunWedgeCandidate>, String> {
    if !policy_enabled(observation) {
        return Ok(Vec::new());
    }
    let mut runs = observation.runs.clone();
    for run in &mut runs {
        if run.event == "pull_request"
            && run.workflow == "Build and Test"
            && matches!(
                run.status.to_ascii_lowercase().as_str(),
                "queued" | "waiting" | "pending" | "requested" | "in_progress"
            )
        {
            run.jobs = fetch_run_jobs(actions, &observation.repo, run.id)?;
        }
    }
    Ok(plan_stale_pr_run_wedges(
        &observation.repo,
        &runs,
        &observation
            .prs
            .iter()
            .map(|pr| pr.fact.clone())
            .collect::<Vec<_>>(),
        &observation.required_checks,
    ))
}

pub(super) fn reconcile_receipts(
    actions: &GitHubActions,
    repo: &str,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
) -> Result<(), String> {
    let keys = ledger
        .stale_pr_run_wedge_receipts
        .iter()
        .filter(|(_, receipt)| receipt.candidate.repo.eq_ignore_ascii_case(repo))
        .filter(|(_, receipt)| {
            matches!(
                receipt.phase,
                StalePrRunWedgeReceiptPhase::Intent
                    | StalePrRunWedgeReceiptPhase::Accepted
                    | StalePrRunWedgeReceiptPhase::Uncertain
            )
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let mut changed = false;
    for key in keys {
        let receipt = ledger
            .stale_pr_run_wedge_receipts
            .get(&key)
            .cloned()
            .expect("receipt key came from ledger");
        let run = read_run(actions, repo, receipt.candidate.old_run_id)?;
        let Some((phase, detail)) = restart_receipt_transition(receipt.phase, &run.status) else {
            continue;
        };
        if phase == StalePrRunWedgeReceiptPhase::Terminal {
            DurableMutationIntent::resume(&receipt.mutation_correlation_id)?
                .supersede_if_uncertain(
                    &mutation_control.state_dir,
                    &mutation_control.global_dir,
                    "stale PR concurrency-wedge run is terminal",
                )?;
        }
        let current = ledger
            .stale_pr_run_wedge_receipts
            .get_mut(&key)
            .expect("receipt remains present");
        current.phase = phase;
        current.updated_at = Utc::now().to_rfc3339();
        current.detail = detail;
        changed = true;
    }
    if changed {
        trim_terminal_receipts(ledger);
        record_audit(
            ledger,
            repo,
            "stale-pr-run-wedge",
            "stale_pr_run_wedge_receipts_reconciled",
        );
        save_ledger(ledger_path, ledger).map_err(|error| error.message)?;
    }
    Ok(())
}

fn restart_receipt_transition(
    phase: StalePrRunWedgeReceiptPhase,
    run_status: &str,
) -> Option<(StalePrRunWedgeReceiptPhase, String)> {
    if run_status.eq_ignore_ascii_case("completed") {
        return Some((
            StalePrRunWedgeReceiptPhase::Terminal,
            format!("old run observed terminal with status={run_status}"),
        ));
    }
    (phase == StalePrRunWedgeReceiptPhase::Intent).then(|| {
        (
            StalePrRunWedgeReceiptPhase::Uncertain,
            "restart found a pre-acceptance intent; at-most-once policy refuses another cancellation"
                .to_owned(),
        )
    })
}

pub(super) fn dedupe_candidates(
    candidates: Vec<StalePrRunWedgeCandidate>,
    ledger: &StewardLedger,
) -> Vec<StalePrRunWedgeCandidate> {
    candidates
        .into_iter()
        .filter(|candidate| {
            !ledger
                .stale_pr_run_wedge_receipts
                .contains_key(&candidate_key(candidate))
        })
        .collect()
}

pub(super) fn reserved_run_ids(
    repo: &str,
    observed: &[StalePrRunWedgeCandidate],
    ledger: &StewardLedger,
) -> BTreeSet<u64> {
    observed
        .iter()
        .map(|candidate| candidate.old_run_id)
        .chain(
            ledger
                .stale_pr_run_wedge_receipts
                .values()
                .filter(|receipt| receipt.candidate.repo.eq_ignore_ascii_case(repo))
                .map(|receipt| receipt.candidate.old_run_id),
        )
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_candidate(
    actions: &GitHubActions,
    observation: &RepoObservation,
    candidate: &StalePrRunWedgeCandidate,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
    provenance_blocking_labels: &[String],
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
) -> CancellationReport {
    let Some(observed) = observation
        .runs
        .iter()
        .find(|run| run.id == candidate.old_run_id)
    else {
        return report(
            candidate.old_run_id,
            None,
            Some("planned stale run disappeared".to_owned()),
        );
    };
    if let Err(report) = initial_revalidation(
        actions,
        observation,
        candidate,
        opt_out_label,
        managed_label,
        handoff_context,
        provenance_blocking_labels,
    ) {
        return report;
    }
    let (guard, key) = match prepare_intent(
        observation,
        candidate,
        observed,
        ledger_path,
        ledger,
        mutation_control,
    ) {
        Ok(prepared) => prepared,
        Err(report) => return report,
    };
    attempt_cancellation(
        actions,
        observation,
        candidate,
        opt_out_label,
        managed_label,
        handoff_context,
        provenance_blocking_labels,
        ledger_path,
        ledger,
        guard,
        &key,
    )
}

#[allow(clippy::too_many_arguments)]
fn initial_revalidation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    candidate: &StalePrRunWedgeCandidate,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
    provenance_blocking_labels: &[String],
) -> Result<(), CancellationReport> {
    match revalidate_candidate(
        actions,
        observation,
        candidate,
        opt_out_label,
        managed_label,
        handoff_context,
        provenance_blocking_labels,
    ) {
        Ok(true) => Ok(()),
        Ok(false) => Err(report(
            candidate.old_run_id,
            Some("skipped_after_live_revalidation"),
            None,
        )),
        Err(error) => Err(report(candidate.old_run_id, None, Some(error))),
    }
}

fn prepare_intent(
    observation: &RepoObservation,
    candidate: &StalePrRunWedgeCandidate,
    observed: &StewardRun,
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    mutation_control: &MutationControl,
) -> Result<(MergeQueueMutationGuard, String), CancellationReport> {
    let intent = DurableMutationIntent::new();
    let mutation_state = match run_mutation_state(observation, observed) {
        Ok(state) => state,
        Err(error) => return Err(report(candidate.old_run_id, None, Some(error))),
    };
    if let Err(error) = intent.validate(
        &mutation_control.store,
        &mutation_control.global_dir,
        &mutation_state,
    ) {
        return Err(report(candidate.old_run_id, None, Some(error)));
    }
    let now = Utc::now().to_rfc3339();
    let key = candidate_key(candidate);
    ledger.stale_pr_run_wedge_receipts.insert(
        key.clone(),
        StalePrRunWedgeReceipt {
            candidate: candidate.clone(),
            phase: StalePrRunWedgeReceiptPhase::Intent,
            mutation_correlation_id: intent.correlation_id().to_owned(),
            created_at: now.clone(),
            updated_at: now,
            detail: "durable intent persisted before the sole cancellation attempt".to_owned(),
        },
    );
    trim_terminal_receipts(ledger);
    record_audit(
        ledger,
        &observation.repo,
        &format!("stale-pr-run-wedge:{}", candidate.old_run_id),
        "stale_pr_run_wedge_intent",
    );
    if let Err(error) = save_ledger(ledger_path, ledger) {
        return Err(report(
            candidate.old_run_id,
            None,
            Some(format!(
                "could not persist stale-run cancellation intent: {}",
                error.message
            )),
        ));
    }
    let guard = match acquire_run_mutation_guard_with_correlation(
        mutation_control,
        observation,
        observed,
        &format!(
            "runner steward cancel stale PR concurrency-wedge run {}",
            candidate.old_run_id
        ),
        intent.correlation_id(),
    ) {
        Ok(guard) => guard,
        Err(error) => {
            update_receipt(
                ledger,
                &key,
                StalePrRunWedgeReceiptPhase::Skipped,
                "mutation authority changed after durable intent; no request was sent",
            );
            let save_error = save_ledger(ledger_path, ledger).err();
            return Err(report(
                candidate.old_run_id,
                None,
                combined_errors(Some(error), save_error.map(|error| error.message)),
            ));
        }
    };
    Ok((guard, key))
}

#[allow(clippy::too_many_arguments)]
fn attempt_cancellation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    candidate: &StalePrRunWedgeCandidate,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
    provenance_blocking_labels: &[String],
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    guard: MergeQueueMutationGuard,
    key: &str,
) -> CancellationReport {
    match revalidate_candidate(
        actions,
        observation,
        candidate,
        opt_out_label,
        managed_label,
        handoff_context,
        provenance_blocking_labels,
    ) {
        Ok(false) => {
            update_receipt(
                ledger,
                key,
                StalePrRunWedgeReceiptPhase::Skipped,
                "exact final revalidation no longer proved the wedge",
            );
            let save_error = save_ledger(ledger_path, ledger).err();
            let guard_error = guard.finish("skipped_after_final_live_revalidation").err();
            return report(
                candidate.old_run_id,
                Some("skipped_after_final_live_revalidation"),
                combined_errors(save_error.map(|error| error.message), guard_error),
            );
        }
        Err(error) => {
            update_receipt(
                ledger,
                key,
                StalePrRunWedgeReceiptPhase::Skipped,
                "exact final revalidation failed before mutation; no request was sent",
            );
            let save_error = save_ledger(ledger_path, ledger).err();
            let guard_error = guard.finish("final_revalidation_failed").err();
            return report(
                candidate.old_run_id,
                None,
                combined_errors(
                    Some(error),
                    combined_errors(save_error.map(|error| error.message), guard_error),
                ),
            );
        }
        Ok(true) => {}
    }

    if let Err(error) = actions.cancel_workflow_run(&observation.repo, candidate.old_run_id) {
        update_receipt(
            ledger,
            key,
            StalePrRunWedgeReceiptPhase::Uncertain,
            "cancellation request outcome was not accepted; at-most-once policy refuses retry",
        );
        let save_error = save_ledger(ledger_path, ledger).err();
        // An ambiguous transport result must leave the central mutation
        // correlation uncertain. Dropping the unfinished guard records that
        // state; terminal run evidence reconciles the exact correlation on a
        // later steward pass.
        drop(guard);
        return report(
            candidate.old_run_id,
            None,
            combined_errors(
                Some(error.to_string()),
                save_error.map(|error| error.message),
            ),
        );
    }
    update_receipt(
        ledger,
        key,
        StalePrRunWedgeReceiptPhase::Accepted,
        "GitHub accepted the sole stale-run cancellation request",
    );
    record_audit(
        ledger,
        &observation.repo,
        &format!("stale-pr-run-wedge:{}", candidate.old_run_id),
        "stale_pr_run_wedge_cancel_accepted",
    );
    let save_error = save_ledger(ledger_path, ledger).err();
    let guard_error = guard.finish("cancel_accepted").err();
    report(
        candidate.old_run_id,
        Some("cancelled_stale_pr_concurrency_wedge"),
        combined_errors(save_error.map(|error| error.message), guard_error),
    )
}

#[allow(clippy::too_many_arguments)]
fn revalidate_candidate(
    actions: &GitHubActions,
    observation: &RepoObservation,
    candidate: &StalePrRunWedgeCandidate,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
    provenance_blocking_labels: &[String],
) -> Result<bool, String> {
    if !policy_enabled(observation) || !candidate.repo.eq_ignore_ascii_case(&observation.repo) {
        return Ok(false);
    }
    let Some(live_pr) = pull_request(
        actions,
        &observation.repo,
        candidate.pr_number,
        &observation.base,
        &BTreeMap::new(),
    )?
    else {
        return Ok(false);
    };
    if pull_request_opted_out(&live_pr, opt_out_label)
        || pull_request_provenance_blocked(&live_pr, provenance_blocking_labels)
        || !pull_request_is_managed(&live_pr, managed_label, handoff_context)
    {
        return Ok(false);
    }
    let mut old = read_run(actions, &observation.repo, candidate.old_run_id)?;
    let mut new = read_run(actions, &observation.repo, candidate.new_run_id)?;
    let live_required_checks = required_checks(actions, &observation.repo, &observation.base)?;
    old.jobs = fetch_run_jobs(actions, &observation.repo, old.id)?;
    new.jobs = fetch_run_jobs(actions, &observation.repo, new.id)?;
    Ok(plan_stale_pr_run_wedges(
        &observation.repo,
        &[old, new],
        &[live_pr.fact],
        &live_required_checks,
    )
    .first()
        == Some(candidate))
}

fn read_run(actions: &GitHubActions, repo: &str, run_id: u64) -> Result<StewardRun, String> {
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{repo}/actions/runs/{run_id}"),
        ],
        "stale PR concurrency-wedge run revalidation",
    )?;
    parse_run(&value).ok_or_else(|| format!("workflow run {run_id} was malformed"))
}

fn policy_enabled(observation: &RepoObservation) -> bool {
    observation.repo.eq_ignore_ascii_case("Generous-Corp/pulp")
        && observation
            .required_checks
            .iter()
            .any(|check| check.context == "macos")
}

pub(super) fn repo_status(
    observation: Option<&RepoObservation>,
    candidates: Vec<StalePrRunWedgeCandidate>,
    ledger: &StewardLedger,
    repo: &str,
) -> StalePrRunWedgeRepoStatus {
    let mut receipts = ledger
        .stale_pr_run_wedge_receipts
        .values()
        .filter(|receipt| receipt.candidate.repo.eq_ignore_ascii_case(repo))
        .cloned()
        .collect::<Vec<_>>();
    receipts.sort_by_key(|receipt| receipt.candidate.old_run_id);
    StalePrRunWedgeRepoStatus {
        policy: if observation.is_some_and(policy_enabled) {
            POLICY_ENABLED.to_owned()
        } else {
            POLICY_DISABLED.to_owned()
        },
        candidates,
        receipts,
    }
}

fn candidate_key(candidate: &StalePrRunWedgeCandidate) -> String {
    format!(
        "{}#{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        candidate.repo.to_ascii_lowercase(),
        candidate.pr_number,
        candidate.old_run_id,
        candidate.old_head_sha.to_ascii_lowercase(),
        candidate.old_run_attempt,
        candidate.new_run_id,
        candidate.new_head_sha.to_ascii_lowercase(),
        candidate.new_run_attempt,
        candidate.workflow_id,
        candidate.head_ref,
        candidate.local_required_job.id,
    )
}

fn update_receipt(
    ledger: &mut StewardLedger,
    key: &str,
    phase: StalePrRunWedgeReceiptPhase,
    detail: &str,
) {
    if let Some(receipt) = ledger.stale_pr_run_wedge_receipts.get_mut(key) {
        receipt.phase = phase;
        receipt.updated_at = Utc::now().to_rfc3339();
        detail.clone_into(&mut receipt.detail);
    }
    trim_terminal_receipts(ledger);
}

fn trim_terminal_receipts(ledger: &mut StewardLedger) {
    let excess = ledger
        .stale_pr_run_wedge_receipts
        .len()
        .saturating_sub(MAX_WEDGE_RECEIPTS);
    if excess == 0 {
        return;
    }
    let mut removable = ledger
        .stale_pr_run_wedge_receipts
        .iter()
        .filter(|(_, receipt)| {
            matches!(
                receipt.phase,
                StalePrRunWedgeReceiptPhase::Terminal | StalePrRunWedgeReceiptPhase::Skipped
            )
        })
        .map(|(key, receipt)| (receipt.updated_at.clone(), key.clone()))
        .collect::<Vec<_>>();
    removable.sort();
    for (_, key) in removable.into_iter().take(excess) {
        ledger.stale_pr_run_wedge_receipts.remove(&key);
    }
}

fn report(run_id: u64, mutation: Option<&str>, error: Option<String>) -> CancellationReport {
    CancellationReport {
        run_id,
        reason: CANCELLATION_REASON.to_owned(),
        mutation: mutation.map(str::to_owned),
        error,
    }
}

fn combined_errors(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}; {second}")),
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::identity::RuntimeMode;
    #[cfg(unix)]
    use crate::merge_steward::{CapacityPreemptionPolicy, StewardCheck, StewardCheckSource};
    use crate::merge_steward::{RequiredCheck, StewardJob, StewardPullRequest};
    #[cfg(unix)]
    use crate::ship_state::ShipStateStore;
    #[cfg(unix)]
    use std::fs;

    fn sha(byte: char) -> String {
        std::iter::repeat_n(byte, 40).collect()
    }

    fn candidate() -> StalePrRunWedgeCandidate {
        StalePrRunWedgeCandidate {
            repo: "Generous-Corp/pulp".to_owned(),
            pr_number: 7895,
            old_run_id: 100,
            old_head_sha: sha('a'),
            old_run_attempt: 1,
            new_run_id: 200,
            new_head_sha: sha('b'),
            new_run_attempt: 1,
            workflow_id: 77,
            workflow: "Build and Test".to_owned(),
            head_ref: "feature/wedge".to_owned(),
            local_required_job: StewardJob {
                id: 900,
                name: "macos".to_owned(),
                status: "in_progress".to_owned(),
                conclusion: None,
                labels: [
                    "self-hosted",
                    "macOS",
                    "ARM64",
                    "pulp-build",
                    "pulp-build-vm",
                    "pulp-build-pr-head",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                runner_name: Some("pulp-macos-gate-slot2".to_owned()),
            },
        }
    }

    fn receipt(phase: StalePrRunWedgeReceiptPhase) -> StalePrRunWedgeReceipt {
        StalePrRunWedgeReceipt {
            candidate: candidate(),
            phase,
            mutation_correlation_id: "correlation".to_owned(),
            created_at: "2026-08-29T00:00:00Z".to_owned(),
            updated_at: "2026-08-29T00:00:00Z".to_owned(),
            detail: String::new(),
        }
    }

    #[test]
    fn durable_receipt_dedupes_candidate_after_restart() {
        let candidate = candidate();
        let mut ledger = StewardLedger::default();
        ledger.stale_pr_run_wedge_receipts.insert(
            candidate_key(&candidate),
            receipt(StalePrRunWedgeReceiptPhase::Accepted),
        );
        assert!(dedupe_candidates(vec![candidate], &ledger).is_empty());
    }

    #[test]
    fn observed_and_receipted_wedges_are_reserved_from_generic_coalescing() {
        let observed = candidate();
        let mut receipted = candidate();
        receipted.old_run_id = 101;
        let mut ledger = StewardLedger::default();
        ledger.stale_pr_run_wedge_receipts.insert(
            candidate_key(&receipted),
            StalePrRunWedgeReceipt {
                candidate: receipted,
                ..receipt(StalePrRunWedgeReceiptPhase::Uncertain)
            },
        );

        assert_eq!(
            reserved_run_ids("Generous-Corp/pulp", &[observed], &ledger),
            BTreeSet::from([100, 101])
        );
    }

    #[test]
    fn crash_intent_becomes_uncertain_instead_of_authorizing_retry() {
        let candidate = candidate();
        let key = candidate_key(&candidate);
        let mut ledger = StewardLedger::default();
        ledger
            .stale_pr_run_wedge_receipts
            .insert(key.clone(), receipt(StalePrRunWedgeReceiptPhase::Intent));
        let (phase, detail) = restart_receipt_transition(
            ledger.stale_pr_run_wedge_receipts[&key].phase,
            "in_progress",
        )
        .expect("intent must reconcile");
        let current = ledger
            .stale_pr_run_wedge_receipts
            .get_mut(&key)
            .expect("receipt");
        current.phase = phase;
        current.detail = detail;
        assert_eq!(
            ledger.stale_pr_run_wedge_receipts[&key].phase,
            StalePrRunWedgeReceiptPhase::Uncertain
        );
        assert!(dedupe_candidates(vec![candidate], &ledger).is_empty());
    }

    #[test]
    fn accepted_restart_waits_without_duplicate_and_terminalizes_when_observed() {
        assert!(
            restart_receipt_transition(StalePrRunWedgeReceiptPhase::Accepted, "in_progress")
                .is_none(),
            "an accepted nonterminal cancellation is observed, never resent"
        );
        assert_eq!(
            restart_receipt_transition(StalePrRunWedgeReceiptPhase::Accepted, "completed")
                .map(|(phase, _)| phase),
            Some(StalePrRunWedgeReceiptPhase::Terminal)
        );
    }

    #[test]
    fn repo_status_is_explicitly_macos_policy_scoped() {
        let observation = RepoObservation {
            repo: "Generous-Corp/pulp".to_owned(),
            base: "main".to_owned(),
            allow_auto_merge: true,
            merge_queue: true,
            required_checks: vec![RequiredCheck {
                context: "macos".to_owned(),
                app_id: None,
            }],
            prs: Vec::new(),
            runs: Vec::new(),
            merge_group_heads: BTreeMap::new(),
            merge_group_enqueued_at: BTreeMap::new(),
            capacity_preemption_policy: super::super::CapacityPreemptionPolicy::pulp(),
            preemption_error: None,
        };
        assert_eq!(
            repo_status(
                Some(&observation),
                Vec::new(),
                &StewardLedger::default(),
                &observation.repo
            )
            .policy,
            POLICY_ENABLED
        );
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)] // End-to-end fixture keeps the two reads and sole POST visible.
    fn exact_final_revalidation_sends_one_cancel_and_persists_receipt() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp");
        let calls = temp.path().join("calls");
        let gh = temp.path().join("gh");
        let script = format!(
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"-X POST repos/Generous-Corp/pulp/actions/runs/100/cancel"*) printf '%s' '{{}}' ;;
  *"repos/Generous-Corp/pulp/rules/branches/main --paginate --slurp"*) printf '%s' '[[{{"type":"required_status_checks","parameters":{{"required_status_checks":[{{"context":"macos"}}]}}}}]]' ;;
  *"repos/Generous-Corp/pulp/branches/main/protection/required_status_checks"*) printf '%s' '{{"contexts":["macos"],"checks":[]}}' ;;
  *"pr view 7895"*) printf '%s' '{{"id":"PR_kw","number":7895,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","headRefName":"feature/wedge","mergeStateStatus":"BLOCKED","autoMergeRequest":null,"labels":[{{"name":"shipyard:managed"}}],"statusCheckRollup":[{{"__typename":"StatusContext","context":"shipyard/steward-handoff","state":"SUCCESS","createdAt":"2026-08-29T09:00:00Z"}}]}}' ;;
  *"actions/runs/100/jobs"*) printf '%s' '{{"jobs":[{{"id":900,"name":"macos","status":"in_progress","conclusion":null,"labels":["self-hosted","macOS","ARM64","pulp-build","pulp-build-vm","pulp-build-pr-head"],"runner_name":"pulp-macos-gate-slot2"}}]}}' ;;
  *"actions/runs/200/jobs"*) printf '%s' '{{"jobs":[]}}' ;;
  *"actions/runs/100"*) printf '%s' '{{"id":100,"workflow_id":77,"run_attempt":1,"name":"Build and Test","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature/wedge","status":"in_progress","event":"pull_request","pull_requests":[{{"number":7895}}],"created_at":"2026-08-29T08:55:28Z"}}' ;;
  *"actions/runs/200"*) printf '%s' '{{"id":200,"workflow_id":77,"run_attempt":1,"name":"Build and Test","head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","head_branch":"feature/wedge","status":"pending","event":"pull_request","pull_requests":[{{"number":7895}}],"created_at":"2026-08-29T09:00:57Z"}}' ;;
  *) printf '%s' '{{}}' ;;
esac
"#,
            calls.display()
        );
        fs::write(&gh, script).expect("fake gh");
        let mut permissions = fs::metadata(&gh).expect("gh metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&gh, permissions).expect("chmod gh");
        let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(gh);

        let mut live_pr = super::super::parse_pr(
            &serde_json::json!({
                "id": "PR_kw",
                "number": 7895,
                "isDraft": false,
                "headRefOid": sha('b'),
                "headRefName": "feature/wedge",
                "mergeStateStatus": "BLOCKED",
                "autoMergeRequest": null,
                "labels": [{"name": "shipyard:managed"}],
                "statusCheckRollup": []
            }),
            &BTreeMap::new(),
        )
        .expect("PR");
        live_pr.fact.checks.push(StewardCheck {
            name: "shipyard/steward-handoff".to_owned(),
            source: StewardCheckSource::StatusContext,
            app_id: None,
            check_run_id: None,
            status: "COMPLETED".to_owned(),
            conclusion: Some("SUCCESS".to_owned()),
            run_id: None,
            observed_at: None,
        });
        let mut old = StewardRun {
            id: 100,
            workflow_id: 77,
            run_attempt: 1,
            workflow: "Build and Test".to_owned(),
            head_sha: sha('a'),
            head_branch: "feature/wedge".to_owned(),
            status: "in_progress".to_owned(),
            event: "pull_request".to_owned(),
            pull_request_number: Some(7895),
            created_at: "2026-08-29T08:55:28Z".to_owned(),
            jobs: Vec::new(),
        };
        let mut new = old.clone();
        new.id = 200;
        new.head_sha = sha('b');
        new.status = "pending".to_owned();
        new.created_at = "2026-08-29T09:00:57Z".to_owned();
        let observation = RepoObservation {
            repo: "Generous-Corp/pulp".to_owned(),
            base: "main".to_owned(),
            allow_auto_merge: true,
            merge_queue: true,
            required_checks: vec![RequiredCheck {
                context: "macos".to_owned(),
                app_id: None,
            }],
            prs: vec![live_pr],
            runs: vec![old.clone(), new],
            merge_group_heads: BTreeMap::new(),
            merge_group_enqueued_at: BTreeMap::new(),
            capacity_preemption_policy: CapacityPreemptionPolicy::pulp(),
            preemption_error: None,
        };
        old.jobs = vec![candidate().local_required_job];
        let candidate = plan_stale_pr_run_wedges(
            &observation.repo,
            &[old, observation.runs[1].clone()],
            &[observation.prs[0].fact.clone()],
            &observation.required_checks,
        )
        .pop()
        .expect("wedge candidate");

        let global_dir = temp.path().join("global");
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&global_dir).expect("global");
        fs::create_dir_all(&state_dir).expect("state");
        fs::write(
            global_dir.join("config.toml"),
            "[merge_queue]\nmutation_machine = \"m1\"\n",
        )
        .expect("authority");
        fs::write(state_dir.join("machine-tag"), "m1\n").expect("machine");
        let control = MutationControl {
            store: ShipStateStore::new(state_dir.join("ship")).expect("store"),
            cwd: temp.path().to_path_buf(),
            mode: RuntimeMode::Shipyard,
            global_dir,
            state_dir,
        };
        let ledger_path = temp.path().join("merge-steward.json");
        let mut ledger = StewardLedger::default();
        let report = apply_candidate(
            &actions,
            &observation,
            &candidate,
            "shipyard:no-auto-merge",
            "shipyard:managed",
            "shipyard/steward-handoff",
            &["5·unresolved".to_owned()],
            &ledger_path,
            &mut ledger,
            &control,
        );
        assert_eq!(
            report.mutation.as_deref(),
            Some("cancelled_stale_pr_concurrency_wedge"),
            "{report:?}"
        );
        assert!(report.error.is_none(), "{report:?}");
        let calls = fs::read_to_string(calls).expect("calls");
        assert_eq!(calls.matches("-X POST").count(), 1, "{calls}");
        assert_eq!(
            ledger
                .stale_pr_run_wedge_receipts
                .values()
                .next()
                .map(|receipt| receipt.phase),
            Some(StalePrRunWedgeReceiptPhase::Accepted)
        );
        assert!(dedupe_candidates(vec![candidate], &ledger).is_empty());
    }

    #[allow(dead_code)]
    fn _type_anchor(_: StewardPullRequest) {}
}
