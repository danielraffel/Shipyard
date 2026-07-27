use super::{
    BTreeMap, BTreeSet, CapacityPreemptionPolicy, CapacityRevalidation, GitHubActions,
    MergeQueueMutationGuard, MutationControl, ObservedPr, RepoObservation, RequiredCheck,
    RunCancellation, ShipState, StewardJob, StewardLedger, StewardPullRequest, StewardRun, Value,
    active_runs, attempt_key, fetch_run_jobs, gh_json, hydrate_required_check_identities,
    is_full_sha, is_safe_capacity_preemption, merge_queue_snapshot, parse_pr, parse_run,
    plan_run_coalescing, pull_requests, queue_front_waits_for_pool, timestamp_old_enough,
};

pub(super) fn revalidate_capacity_preemption(
    actions: &GitHubActions,
    observation: &RepoObservation,
    cancellation: &RunCancellation,
    observed: &StewardRun,
    expected_front: &str,
    opt_out_label: &str,
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
    )?;
    all_current_heads.insert(candidate_pr.fact.number, candidate_pr.fact.head_sha.clone());
    if pull_request_opted_out(&candidate_pr, opt_out_label) {
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
) -> Result<(BTreeMap<u64, String>, BTreeSet<u64>), String> {
    let prs = pull_requests(actions, repo, base, &BTreeMap::new())?;
    Ok((
        current_pull_request_heads(&prs),
        opted_out_pull_requests(&prs, opt_out_label),
    ))
}

pub(super) fn pull_request(
    actions: &GitHubActions,
    repo: &str,
    number: u64,
    expected_base: &str,
    queue_positions: &BTreeMap<u64, u64>,
) -> Result<Option<ObservedPr>, String> {
    let value = gh_json(
        actions,
        &[
            "pr".to_owned(),
            "view".to_owned(),
            number.to_string(),
            "--repo".to_owned(),
            repo.to_owned(),
            "--json".to_owned(),
            "id,number,state,isDraft,baseRefName,headRefOid,headRefName,mergeStateStatus,autoMergeRequest,labels,statusCheckRollup".to_owned(),
        ],
        "capacity-preemption candidate PR",
    )?;
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

pub(super) fn revalidate_coalescing_cancellation(
    actions: &GitHubActions,
    observation: &RepoObservation,
    observed: &StewardRun,
    cancellation: &RunCancellation,
    opt_out_label: &str,
) -> Result<bool, String> {
    if !is_full_sha(&observed.head_sha) {
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
    if pull_request_opted_out(&candidate_pr, opt_out_label) {
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
