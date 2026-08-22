use super::{
    BTreeMap, BTreeSet, CapacityPreemptionPolicy, CapacityRevalidation, GitHubActions, Instant,
    MergeQueueMutationGuard, MutationControl, ObservedPr, PendingCancellation, RepoObservation,
    RequiredCheck, RunCancellation, RunCancellationReason, ShipState, StewardJob, StewardLedger,
    StewardPullRequest, StewardRun, Value, active_runs, attempt_key, coalescing_reason_authorizes,
    fetch_run_jobs, gh_json, gh_json_before, has_successful_status,
    hydrate_required_check_identities, hydrate_required_check_identities_before, is_full_sha,
    is_safe_capacity_preemption, merge_queue_snapshot, parse_pr, parse_run, plan_run_coalescing,
    pull_requests, queue_front_waits_for_pool, timestamp_old_enough,
};

#[allow(clippy::too_many_arguments)]
pub(super) fn revalidate_capacity_preemption(
    actions: &GitHubActions,
    observation: &RepoObservation,
    cancellation: &RunCancellation,
    observed: &StewardRun,
    expected_front: &str,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
    provenance_blocking_labels: &[String],
) -> Result<Option<CapacityRevalidation>, String> {
    let front_enqueued_at = match live_queue_front(actions, &observation.repo, &observation.base)? {
        Some((live_front, enqueued_at))
            if live_front.eq_ignore_ascii_case(expected_front)
                && timestamp_old_enough(&enqueued_at) =>
        {
            enqueued_at
        }
        _ => return Ok(None),
    };
    let Some(front_jobs) = live_queue_front_pool_jobs(
        actions,
        &observation.repo,
        expected_front,
        &observation.capacity_preemption_policy,
    )?
    else {
        return Ok(None);
    };
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!(
                "repos/{}/actions/runs/{}",
                observation.repo, cancellation.run_id
            ),
        ],
        "capacity-preemption run revalidation",
    )?;
    let mut live =
        parse_run(&value).ok_or_else(|| "live capacity-preemption run was malformed".to_owned())?;
    if !same_workflow_attempt(observed, &live) {
        return Ok(None);
    }
    live.jobs = fetch_run_jobs(actions, &observation.repo, cancellation.run_id)?;
    let candidate_pr_number = live
        .pull_request_number
        .ok_or_else(|| "capacity-preemption run no longer has a unique PR".to_owned())?;
    let Some(candidate_pr) = pull_request(
        actions,
        &observation.repo,
        candidate_pr_number,
        &observation.base,
        &BTreeMap::new(),
    )?
    else {
        return Ok(None);
    };
    let (mut all_current_heads, mut opted_out) = live_current_pull_request_state(
        actions,
        &observation.repo,
        &observation.base,
        opt_out_label,
        provenance_blocking_labels,
    )?;
    all_current_heads.insert(candidate_pr.fact.number, candidate_pr.fact.head_sha.clone());
    if pull_request_opted_out(&candidate_pr, opt_out_label)
        || pull_request_provenance_blocked(&candidate_pr, provenance_blocking_labels)
        || !managed_ownership_still_valid(
            observation,
            &candidate_pr,
            managed_label,
            handoff_context,
        )
    {
        opted_out.insert(candidate_pr.fact.number);
    }
    if !is_safe_capacity_preemption(
        &live,
        &opted_out,
        &observation.capacity_preemption_policy,
        cancellation.reason,
    ) {
        return Ok(None);
    }
    let current_pr_head = live
        .pull_request_number
        .and_then(|number| all_current_heads.get(&number).cloned());
    Ok(Some(CapacityRevalidation {
        candidate: live,
        front_enqueued_at,
        front_jobs,
        current_pr_head,
    }))
}

pub(super) fn same_workflow_attempt(observed: &StewardRun, live: &StewardRun) -> bool {
    live.head_sha.eq_ignore_ascii_case(&observed.head_sha)
        && live.workflow_id == observed.workflow_id
        && live.run_attempt == observed.run_attempt
}

pub(super) fn live_current_pull_request_state(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    opt_out_label: &str,
    provenance_blocking_labels: &[String],
) -> Result<(BTreeMap<u64, String>, BTreeSet<u64>), String> {
    let prs = pull_requests(actions, repo, base, &BTreeMap::new())?;
    Ok((
        current_pull_request_heads(&prs),
        authority_excluded_pull_requests(&prs, opt_out_label, provenance_blocking_labels),
    ))
}

pub(super) fn pull_request(
    actions: &GitHubActions,
    repo: &str,
    number: u64,
    expected_base: &str,
    queue_positions: &BTreeMap<u64, u64>,
) -> Result<Option<ObservedPr>, String> {
    pull_request_with_deadline(actions, repo, number, expected_base, queue_positions, None)
}

fn pull_request_with_deadline(
    actions: &GitHubActions,
    repo: &str,
    number: u64,
    expected_base: &str,
    queue_positions: &BTreeMap<u64, u64>,
    deadline: Option<Instant>,
) -> Result<Option<ObservedPr>, String> {
    let args = [
        "pr".to_owned(),
        "view".to_owned(),
        number.to_string(),
        "--repo".to_owned(),
        repo.to_owned(),
        "--json".to_owned(),
        "id,number,state,isDraft,baseRefName,headRefOid,headRefName,mergeStateStatus,autoMergeRequest,labels,statusCheckRollup".to_owned(),
    ];
    let value = match deadline {
        Some(deadline) => {
            gh_json_before(actions, &args, "capacity-preemption candidate PR", deadline)?
        }
        None => gh_json(actions, &args, "capacity-preemption candidate PR")?,
    };
    if value.get("state").and_then(Value::as_str) != Some("OPEN") {
        return Ok(None);
    }
    if value.get("baseRefName").and_then(Value::as_str) != Some(expected_base) {
        return Ok(None);
    }
    parse_pr(&value, queue_positions).map(Some)
}

pub(super) fn pull_request_with_required_checks(
    actions: &GitHubActions,
    repo: &str,
    number: u64,
    expected_base: &str,
    queue_positions: &BTreeMap<u64, u64>,
    required_checks: &[RequiredCheck],
) -> Result<Option<ObservedPr>, String> {
    let Some(mut pr) = pull_request(actions, repo, number, expected_base, queue_positions)? else {
        return Ok(None);
    };
    hydrate_required_check_identities(
        actions,
        repo,
        required_checks,
        std::slice::from_mut(&mut pr),
    )?;
    Ok(Some(pr))
}

pub(super) fn pull_request_with_required_checks_before(
    actions: &GitHubActions,
    repo: &str,
    number: u64,
    expected_base: &str,
    queue_positions: &BTreeMap<u64, u64>,
    required_checks: &[RequiredCheck],
    deadline: Instant,
) -> Result<Option<ObservedPr>, String> {
    let Some(mut pr) = pull_request_with_deadline(
        actions,
        repo,
        number,
        expected_base,
        queue_positions,
        Some(deadline),
    )?
    else {
        return Ok(None);
    };
    hydrate_required_check_identities_before(
        actions,
        repo,
        required_checks,
        std::slice::from_mut(&mut pr),
        deadline,
    )?;
    Ok(Some(pr))
}

pub(super) fn current_pull_request_heads(prs: &[ObservedPr]) -> BTreeMap<u64, String> {
    prs.iter()
        .map(|pr| (pr.fact.number, pr.fact.head_sha.clone()))
        .collect()
}

pub(super) fn opted_out_pull_requests(prs: &[ObservedPr], opt_out_label: &str) -> BTreeSet<u64> {
    prs.iter()
        .filter(|pr| pull_request_opted_out(pr, opt_out_label))
        .map(|pr| pr.fact.number)
        .collect()
}

pub(super) fn pull_request_opted_out(pr: &ObservedPr, opt_out_label: &str) -> bool {
    pr.fact
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(opt_out_label))
}

pub(super) fn pull_request_provenance_blocked(
    pr: &ObservedPr,
    provenance_blocking_labels: &[String],
) -> bool {
    provenance_blocking_labels.iter().any(|blocker| {
        pr.fact
            .labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case(blocker))
    })
}

pub(super) fn authority_excluded_pull_requests(
    prs: &[ObservedPr],
    opt_out_label: &str,
    provenance_blocking_labels: &[String],
) -> BTreeSet<u64> {
    prs.iter()
        .filter(|pr| {
            pull_request_opted_out(pr, opt_out_label)
                || pull_request_provenance_blocked(pr, provenance_blocking_labels)
        })
        .map(|pr| pr.fact.number)
        .collect()
}

/// Re-establish current-PR authority immediately before force-cancelling an
/// already accepted cancellation. The stale run SHA remains bound separately
/// by `read_current_pending_run_identity`; this read proves that the current PR
/// at the recorded number/base still permits Shipyard mutation.
pub(super) fn revalidate_pending_pr_authority(
    actions: &GitHubActions,
    pending: &PendingCancellation,
) -> Result<(), String> {
    let live = pull_request(
        actions,
        &pending.repo,
        pending.pr_number,
        &pending.base,
        &BTreeMap::new(),
    )?
    .ok_or_else(|| {
        "pending cancellation pull request is no longer open on its recorded base".to_owned()
    })?;
    if pull_request_provenance_blocked(&live, &pending.provenance_blocking_labels) {
        return Err("current pull request has a provenance-blocking label".to_owned());
    }
    if pull_request_opted_out(&live, &pending.opt_out_label) {
        return Err("current pull request has the steward opt-out label".to_owned());
    }
    if !pull_request_is_managed(&live, &pending.managed_label, &pending.handoff_context) {
        return Err(
            "current pull request no longer has exact-head steward management authority".to_owned(),
        );
    }
    Ok(())
}

pub(super) fn pull_request_is_managed(
    pr: &ObservedPr,
    managed_label: &str,
    handoff_context: &str,
) -> bool {
    let explicitly_managed = pr
        .fact
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(managed_label));
    let exact_head_handoff = has_successful_status(&pr.fact, handoff_context);
    explicitly_managed && exact_head_handoff
}

fn managed_ownership_still_valid(
    observation: &RepoObservation,
    live: &ObservedPr,
    managed_label: &str,
    handoff_context: &str,
) -> bool {
    let initial = observation
        .prs
        .iter()
        .find(|pr| pr.fact.number == live.fact.number);
    initial.is_some_and(|initial| {
        management_transition_valid(initial, live, managed_label, handoff_context)
    })
}

fn management_transition_valid(
    initial: &ObservedPr,
    live: &ObservedPr,
    managed_label: &str,
    handoff_context: &str,
) -> bool {
    !pull_request_is_managed(initial, managed_label, handoff_context)
        || pull_request_is_managed(live, managed_label, handoff_context)
}

pub(super) fn live_queue_front_pool_jobs(
    actions: &GitHubActions,
    repo: &str,
    expected_front: &str,
    policy: &CapacityPreemptionPolicy,
) -> Result<Option<Vec<StewardJob>>, String> {
    let mut runs = active_runs(actions, repo)?;
    for run in &mut runs {
        if run.event == "merge_group" && run.head_sha.eq_ignore_ascii_case(expected_front) {
            run.jobs = fetch_run_jobs(actions, repo, run.id)?;
        }
    }
    if !queue_front_waits_for_pool(&runs, expected_front, policy) {
        return Ok(None);
    }
    Ok(Some(
        runs.into_iter()
            .filter(|run| {
                run.event == "merge_group" && run.head_sha.eq_ignore_ascii_case(expected_front)
            })
            .flat_map(|run| run.jobs)
            .collect(),
    ))
}

pub(super) fn live_queue_front(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Option<(String, String)>, String> {
    let (enabled, positions, heads, enqueued) = merge_queue_snapshot(actions, repo, base)?;
    if !enabled {
        return Ok(None);
    }
    Ok(positions
        .iter()
        .min_by_key(|(_, position)| **position)
        .and_then(|(number, _)| Some((heads.get(number)?.clone(), enqueued.get(number)?.clone()))))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn revalidate_coalescing_cancellation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    observed: &StewardRun,
    cancellation: &RunCancellation,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
    provenance_blocking_labels: &[String],
) -> Result<bool, String> {
    if !coalescing_reason_authorizes(cancellation.reason) || !is_full_sha(&observed.head_sha) {
        return Ok(false);
    }
    let Some(pr_number) = observed
        .pull_request_number
        .or_else(|| merge_group_pr_number(observed))
    else {
        return Ok(false);
    };
    let Some(candidate_pr) = pull_request(
        actions,
        &observation.repo,
        pr_number,
        &observation.base,
        &BTreeMap::new(),
    )?
    else {
        return Ok(false);
    };
    if pull_request_opted_out(&candidate_pr, opt_out_label)
        || pull_request_provenance_blocked(&candidate_pr, provenance_blocking_labels)
        || !managed_ownership_still_valid(
            observation,
            &candidate_pr,
            managed_label,
            handoff_context,
        )
    {
        return Ok(false);
    }
    let mut current_heads = BTreeMap::new();
    current_heads.insert(pr_number, candidate_pr.fact.head_sha);
    let (_, _, merge_group_heads, _) =
        merge_queue_snapshot(actions, &observation.repo, &observation.base)?;
    let live_runs = active_runs(actions, &observation.repo)?;
    let opted_out = BTreeSet::new();
    let reason_reproved =
        plan_run_coalescing(&live_runs, &current_heads, &merge_group_heads, &opted_out)
            .iter()
            .any(|planned| {
                planned.run_id == cancellation.run_id && planned.reason == cancellation.reason
            });
    if !reason_reproved {
        return Ok(false);
    }
    exact_run_still_queued(actions, observation, observed, cancellation, pr_number)
}

pub(super) fn exact_run_still_queued(
    actions: &GitHubActions,
    observation: &RepoObservation,
    observed: &StewardRun,
    cancellation: &RunCancellation,
    pr_number: u64,
) -> Result<bool, String> {
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!(
                "repos/{}/actions/runs/{}",
                observation.repo, cancellation.run_id
            ),
        ],
        "coalescing exact-run revalidation",
    )?;
    let Some(exact) = parse_run(&value) else {
        return Ok(false);
    };
    Ok(exact.status.eq_ignore_ascii_case("queued")
        && exact.workflow_id == observed.workflow_id
        && exact.run_attempt == observed.run_attempt
        && exact.event == observed.event
        && exact.head_sha.eq_ignore_ascii_case(&observed.head_sha)
        && exact
            .pull_request_number
            .or_else(|| merge_group_pr_number(&exact))
            == Some(pr_number))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn authoritative_head_still_superseded(
    actions: &GitHubActions,
    observation: &RepoObservation,
    observed: &StewardRun,
    cancellation: &RunCancellation,
    opt_out_label: &str,
    managed_label: &str,
    handoff_context: &str,
    provenance_blocking_labels: &[String],
) -> Result<bool, String> {
    let Some(pr_number) = observed
        .pull_request_number
        .or_else(|| merge_group_pr_number(observed))
    else {
        return Ok(false);
    };
    let Some(candidate_pr) = pull_request(
        actions,
        &observation.repo,
        pr_number,
        &observation.base,
        &BTreeMap::new(),
    )?
    else {
        return Ok(false);
    };
    if pull_request_opted_out(&candidate_pr, opt_out_label)
        || pull_request_provenance_blocked(&candidate_pr, provenance_blocking_labels)
        || !managed_ownership_still_valid(
            observation,
            &candidate_pr,
            managed_label,
            handoff_context,
        )
    {
        return Ok(false);
    }
    let current_head = match cancellation.reason {
        RunCancellationReason::SupersededPullRequestHead => candidate_pr.fact.head_sha,
        RunCancellationReason::SupersededMergeGroupHead => {
            let (_, _, merge_group_heads, _) =
                merge_queue_snapshot(actions, &observation.repo, &observation.base)?;
            let Some(head) = merge_group_heads.get(&pr_number) else {
                return Ok(false);
            };
            head.clone()
        }
        _ => return Ok(false),
    };
    Ok(!current_head.eq_ignore_ascii_case(&observed.head_sha))
}

pub(super) fn merge_group_pr_number(run: &StewardRun) -> Option<u64> {
    if run.event != "merge_group" {
        return None;
    }
    crate::merge_queue_liveness::merge_group_pr(&run.head_branch)
}

pub(super) fn attempts_for(
    ledger: &StewardLedger,
    repo: &str,
    pr: &StewardPullRequest,
) -> BTreeMap<u64, u32> {
    pr.checks
        .iter()
        .filter_map(|check| {
            let run_id = check.run_id?;
            let key = attempt_key(repo, pr.number, &pr.head_sha, run_id);
            Some((
                run_id,
                ledger.transient_attempts.get(&key).copied().unwrap_or(0),
            ))
        })
        .collect()
}

pub(super) fn acquire_pr_mutation_guard(
    control: &MutationControl,
    observation: &RepoObservation,
    pr: &ObservedPr,
    action: &str,
) -> Result<MergeQueueMutationGuard, String> {
    let state = ShipState::new(
        pr.fact.number,
        &observation.repo,
        &pr.fact.head_branch,
        &observation.base,
        &pr.fact.head_sha,
        "runner-steward",
    );
    MergeQueueMutationGuard::acquire_in_mode(
        &control.store,
        &control.cwd,
        control.mode,
        &control.global_dir,
        &state,
        action,
    )
}

pub(super) fn acquire_run_mutation_guard(
    control: &MutationControl,
    observation: &RepoObservation,
    run: &StewardRun,
    action: &str,
) -> Result<MergeQueueMutationGuard, String> {
    let pr_number = run
        .pull_request_number
        .or_else(|| merge_group_pr_number(run))
        .ok_or_else(|| {
            format!(
                "workflow run {} has no pull-request identity; refusing an unaudited mutation",
                run.id
            )
        })?;
    let branch = observation
        .prs
        .iter()
        .find(|pr| pr.fact.number == pr_number)
        .map_or(run.head_branch.as_str(), |pr| pr.fact.head_branch.as_str());
    let state = ShipState::new(
        pr_number,
        &observation.repo,
        branch,
        &observation.base,
        &run.head_sha,
        "runner-steward",
    );
    MergeQueueMutationGuard::acquire_in_mode(
        &control.store,
        &control.cwd,
        control.mode,
        &control.global_dir,
        &state,
        action,
    )
}
