use super::{
    GitHubActions, ObservedPr, QueueWitness, RepoObservation, StewardCommandArgs, StewardLedger,
    Value, WitnessRequiredCheck, is_full_sha, save_ledger,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};

const MAX_REMOVAL_TO_RUN_DELAY: ChronoDuration = ChronoDuration::minutes(10);
const MAX_RECOVERY_AGE: ChronoDuration = ChronoDuration::hours(2);
const MAX_SETUP_LOG_BYTES: usize = 256 * 1024;
const GITHUB_ACTIONS_APP_ID: u64 = 15_368;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct QueueRecoveryEvidence {
    pub(super) base_sha: String,
    pub(super) merge_group_head: String,
    pub(super) removed_at: String,
    pub(super) run_id: u64,
    pub(super) job_id: u64,
}

pub(super) fn record_queue_witnesses(
    actions: &GitHubActions,
    observation: &RepoObservation,
    args: &StewardCommandArgs,
    ledger_path: &std::path::Path,
    ledger: &mut StewardLedger,
) -> Result<(), String> {
    let base_sha_before = current_base_sha(actions, observation)?;
    let required_checks_before =
        super::observation::required_checks(actions, &observation.repo, &observation.base)?;
    let (enabled, positions, merge_group_heads, enqueued_at) =
        super::merge_queue_snapshot(actions, &observation.repo, &observation.base)?;
    if !enabled {
        return Ok(());
    }
    let observed_at = Utc::now().to_rfc3339();
    let mut changed = false;
    for observed_pr in &observation.prs {
        let Some(position) = positions.get(&observed_pr.fact.number).copied() else {
            continue;
        };
        if position != 1 {
            continue;
        }
        let Some(pr) = super::pull_request_with_required_checks(
            actions,
            &observation.repo,
            observed_pr.fact.number,
            &observation.base,
            &positions,
            &required_checks_before,
        )?
        else {
            continue;
        };
        if !pr
            .fact
            .head_sha
            .eq_ignore_ascii_case(&observed_pr.fact.head_sha)
            || !super::pull_request_is_managed(&pr, &args.managed_label, &args.handoff_context)
        {
            continue;
        }
        let Some(merge_group_head) = merge_group_heads.get(&pr.fact.number) else {
            continue;
        };
        let Some(enqueued_at) = enqueued_at.get(&pr.fact.number) else {
            continue;
        };
        if !is_full_sha(&pr.fact.head_sha) || !is_full_sha(merge_group_head) {
            continue;
        }
        if !merge_group_contains_base(actions, observation, merge_group_head, &base_sha_before)? {
            continue;
        }
        let required_checks_after =
            super::observation::required_checks(actions, &observation.repo, &observation.base)?;
        let base_sha_after = current_base_sha(actions, observation)?;
        if base_sha_after != base_sha_before || required_checks_after != required_checks_before {
            return Ok(());
        }
        let key = witness_key(&observation.repo, pr.fact.number);
        ledger.queue_witnesses.insert(
            key,
            QueueWitness {
                repo: observation.repo.clone(),
                base: observation.base.clone(),
                base_sha: base_sha_after,
                pr_number: pr.fact.number,
                pr_head: pr.fact.head_sha.clone(),
                merge_group_head: merge_group_head.clone(),
                position,
                enqueued_at: enqueued_at.clone(),
                observed_at: observed_at.clone(),
                required_checks: witness_required_checks(&required_checks_after),
            },
        );
        changed = true;
    }
    if changed {
        save_ledger(ledger_path, ledger).map_err(|error| {
            format!(
                "could not persist merge-queue recovery witness: {}",
                error.message
            )
        })?;
    }
    Ok(())
}

pub(super) fn recovery_evidence(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
    ledger: &StewardLedger,
) -> Result<Option<QueueRecoveryEvidence>, String> {
    if pr.fact.queue_position.is_some() {
        return Ok(None);
    }
    let Some(witness) = ledger
        .queue_witnesses
        .get(&witness_key(&observation.repo, pr.fact.number))
    else {
        return Ok(None);
    };
    if witness.repo != observation.repo
        || witness.base != observation.base
        || witness.pr_number != pr.fact.number
        || !witness.pr_head.eq_ignore_ascii_case(&pr.fact.head_sha)
        || !is_full_sha(&witness.merge_group_head)
        || witness.position != 1
    {
        return Ok(None);
    }
    if current_base_sha(actions, observation)? != witness.base_sha {
        return Ok(None);
    }
    let current_required =
        super::observation::required_checks(actions, &observation.repo, &observation.base)?;
    if current_required != observation.required_checks
        || witness.required_checks != witness_required_checks(&current_required)
    {
        return Ok(None);
    }
    let Some(removed_at) = failed_checks_removal(actions, observation, pr, witness)? else {
        return Ok(None);
    };
    let Some((run_id, job_id)) = hosted_setup_failure(actions, observation, witness, &removed_at)?
    else {
        return Ok(None);
    };
    let evidence = QueueRecoveryEvidence {
        base_sha: witness.base_sha.clone(),
        merge_group_head: witness.merge_group_head.clone(),
        removed_at,
        run_id,
        job_id,
    };
    if ledger
        .queue_recovery_receipts
        .contains_key(&receipt_key(&evidence))
    {
        return Ok(None);
    }
    Ok(Some(evidence))
}

pub(super) fn receipt_key(evidence: &QueueRecoveryEvidence) -> String {
    format!(
        "{}:{}:{}:{}",
        evidence.merge_group_head, evidence.removed_at, evidence.run_id, evidence.job_id
    )
}

pub(super) fn final_mutable_authority_matches(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
    evidence: &QueueRecoveryEvidence,
) -> Result<bool, String> {
    let current_required =
        super::observation::required_checks(actions, &observation.repo, &observation.base)?;
    if current_required != observation.required_checks {
        return Ok(false);
    }
    let Some((owner, name)) = observation.repo.split_once('/') else {
        return Err(format!("invalid repository slug `{}`", observation.repo));
    };
    let qualified_base = format!("refs/heads/{}", observation.base);
    let query = "query finalRecoveryAuthority($owner:String!,$name:String!,$number:Int!,$branch:String!,$qualifiedBase:String!){repository(owner:$owner,name:$name){ref(qualifiedName:$qualifiedBase){target{oid}} pullRequest(number:$number){state baseRefName headRefOid} mergeQueue(branch:$branch){entries(first:100){nodes{pullRequest{number}} pageInfo{hasNextPage}}}}}";
    let value = super::gh_json(
        actions,
        &[
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={query}"),
            "-F".to_owned(),
            format!("owner={owner}"),
            "-F".to_owned(),
            format!("name={name}"),
            "-F".to_owned(),
            format!("number={}", pr.fact.number),
            "-F".to_owned(),
            format!("branch={}", observation.base),
            "-F".to_owned(),
            format!("qualifiedBase={qualified_base}"),
        ],
        "final queue-recovery authority",
    )?;
    let repository = value
        .pointer("/data/repository")
        .ok_or_else(|| "final queue-recovery authority missing repository".to_owned())?;
    if repository
        .pointer("/mergeQueue/entries/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        return Err("final queue-recovery authority has a partial queue snapshot".to_owned());
    }
    let entries = repository
        .pointer("/mergeQueue/entries/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "final queue-recovery authority missing merge queue entries".to_owned())?;
    Ok(repository
        .pointer("/pullRequest/state")
        .and_then(Value::as_str)
        == Some("OPEN")
        && repository
            .pointer("/pullRequest/baseRefName")
            .and_then(Value::as_str)
            == Some(observation.base.as_str())
        && repository
            .pointer("/pullRequest/headRefOid")
            .and_then(Value::as_str)
            .is_some_and(|head| head.eq_ignore_ascii_case(&pr.fact.head_sha))
        && repository
            .pointer("/ref/target/oid")
            .and_then(Value::as_str)
            .is_some_and(|base| base.eq_ignore_ascii_case(&evidence.base_sha))
        && !entries.iter().any(|entry| {
            entry.pointer("/pullRequest/number").and_then(Value::as_u64) == Some(pr.fact.number)
        }))
}

fn failed_checks_removal(
    actions: &GitHubActions,
    observation: &RepoObservation,
    pr: &ObservedPr,
    witness: &QueueWitness,
) -> Result<Option<String>, String> {
    let Some((owner, name)) = observation.repo.split_once('/') else {
        return Err(format!("invalid repository slug `{}`", observation.repo));
    };
    let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){timelineItems(last:100,itemTypes:[REMOVED_FROM_MERGE_QUEUE_EVENT,ADDED_TO_MERGE_QUEUE_EVENT]){nodes{__typename ... on RemovedFromMergeQueueEvent{createdAt reason} ... on AddedToMergeQueueEvent{createdAt}} pageInfo{hasPreviousPage}}}}}";
    let value = super::gh_json(
        actions,
        &[
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={query}"),
            "-F".to_owned(),
            format!("owner={owner}"),
            "-F".to_owned(),
            format!("name={name}"),
            "-F".to_owned(),
            format!("number={}", pr.fact.number),
        ],
        "merge-queue removal timeline",
    )?;
    let events = value
        .pointer("/data/repository/pullRequest/timelineItems/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "merge-queue removal timeline missing timelineItems.nodes".to_owned())?;
    if value
        .pointer("/data/repository/pullRequest/timelineItems/pageInfo/hasPreviousPage")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        return Err(
            "merge-queue removal timeline exceeds 100; refusing a partial history".to_owned(),
        );
    }
    let [.., admission, latest] = events.as_slice() else {
        return Ok(None);
    };
    if latest.get("__typename").and_then(Value::as_str) != Some("RemovedFromMergeQueueEvent")
        || !latest
            .get("reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.eq_ignore_ascii_case("failed_checks"))
    {
        return Ok(None);
    }
    if admission.get("__typename").and_then(Value::as_str) != Some("AddedToMergeQueueEvent")
        || admission.get("createdAt").and_then(Value::as_str) != Some(&witness.enqueued_at)
    {
        return Ok(None);
    }
    let Some(removed_at) = latest.get("createdAt").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(removed) = parse_time(removed_at) else {
        return Ok(None);
    };
    let Some(observed) = parse_time(&witness.observed_at) else {
        return Ok(None);
    };
    let Some(enqueued) = parse_time(&witness.enqueued_at) else {
        return Ok(None);
    };
    if removed < observed || observed < enqueued || !removal_is_fresh(removed, Utc::now()) {
        return Ok(None);
    }
    Ok(Some(removed_at.to_owned()))
}

#[allow(clippy::too_many_lines)] // One fail-closed evidence chain validates run, check, job, and log.
fn hosted_setup_failure(
    actions: &GitHubActions,
    observation: &RepoObservation,
    witness: &QueueWitness,
    removed_at: &str,
) -> Result<Option<(u64, u64)>, String> {
    let value = super::gh_json(
        actions,
        &[
            "api".to_owned(),
            format!(
                "repos/{}/actions/runs?event=merge_group&status=completed&head_sha={}&per_page=100",
                observation.repo, witness.merge_group_head
            ),
        ],
        "completed merge-group runs",
    )?;
    let rows = value
        .get("workflow_runs")
        .and_then(Value::as_array)
        .ok_or_else(|| "completed merge-group runs missing workflow_runs".to_owned())?;
    if rows.len() == 100 {
        return Err(
            "completed merge-group runs reached 100; refusing a partial history".to_owned(),
        );
    }
    let removed = parse_time(removed_at).expect("validated removal time");
    let enqueued = parse_time(&witness.enqueued_at).expect("validated enqueue time");
    let matching = rows
        .iter()
        .filter(|run| {
            run.get("event").and_then(Value::as_str) == Some("merge_group")
                && run.get("status").and_then(Value::as_str) == Some("completed")
                && run.get("conclusion").and_then(Value::as_str) == Some("failure")
                && run
                    .get("head_sha")
                    .and_then(Value::as_str)
                    .is_some_and(|head| head.eq_ignore_ascii_case(&witness.merge_group_head))
                && run
                    .get("created_at")
                    .and_then(Value::as_str)
                    .and_then(parse_time)
                    .is_some_and(|created| created >= enqueued && created <= removed)
                && run
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .and_then(parse_time)
                    .is_some_and(|updated| {
                        updated <= removed && removed - updated <= MAX_REMOVAL_TO_RUN_DELAY
                    })
        })
        .collect::<Vec<_>>();
    let [run] = matching.as_slice() else {
        return Ok(None);
    };
    let Some(run_id) = run.get("id").and_then(Value::as_u64) else {
        return Ok(None);
    };
    let checks =
        super::complete_checks_for_head(actions, &observation.repo, &witness.merge_group_head)?;
    let selected = observation
        .required_checks
        .iter()
        .map(|required| crate::merge_steward::selected_required_check(&checks, required))
        .collect::<Option<Vec<_>>>();
    let Some(selected) = selected else {
        return Ok(None);
    };
    let failed_required = selected
        .iter()
        .filter(|check| {
            check.status.eq_ignore_ascii_case("completed")
                && check
                    .conclusion
                    .as_deref()
                    .is_some_and(|conclusion| conclusion.eq_ignore_ascii_case("failure"))
        })
        .collect::<Vec<_>>();
    let [failed_required] = failed_required.as_slice() else {
        return Ok(None);
    };
    if !failed_required_check_matches_run(failed_required, run_id)
        || selected.iter().any(|check| {
            !check.status.eq_ignore_ascii_case("completed")
                || !required_check_has_allowed_conclusion(check)
        })
    {
        return Ok(None);
    }
    let jobs = super::gh_json(
        actions,
        &[
            "api".to_owned(),
            format!(
                "repos/{}/actions/runs/{run_id}/jobs?filter=all&per_page=100",
                observation.repo
            ),
        ],
        "failed merge-group jobs",
    )?;
    let jobs = jobs
        .get("jobs")
        .and_then(Value::as_array)
        .ok_or_else(|| "failed merge-group jobs missing jobs".to_owned())?;
    if jobs.len() == 100 {
        return Err("failed merge-group jobs reached 100; refusing a partial job list".to_owned());
    }
    let failed = jobs
        .iter()
        .filter(|job| job.get("conclusion").and_then(Value::as_str) == Some("failure"))
        .collect::<Vec<_>>();
    let [job] = failed.as_slice() else {
        return Ok(None);
    };
    if jobs.iter().any(|job| {
        !matches!(
            job.get("conclusion").and_then(Value::as_str),
            Some("success" | "neutral" | "skipped" | "failure")
        )
    }) || !is_hosted_setup_job(job)
    {
        return Ok(None);
    }
    let Some(job_id) = job.get("id").and_then(Value::as_u64) else {
        return Ok(None);
    };
    let log = actions
        .run_gh_with_timeout_bounded(
            &[
                "api".to_owned(),
                format!("repos/{}/actions/jobs/{job_id}/logs", observation.repo),
            ],
            std::time::Duration::from_secs(20),
            MAX_SETUP_LOG_BYTES,
            64 * 1024,
        )
        .map_err(|error| format!("failed setup-job log fetch failed: {error}"))?;
    if !provider_internal_dns_failure(&log) {
        return Ok(None);
    }
    Ok(Some((run_id, job_id)))
}

fn is_hosted_setup_job(job: &Value) -> bool {
    if job.get("runner_group_name").and_then(Value::as_str) != Some("GitHub Actions") {
        return false;
    }
    let labels = job
        .get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let hosted = matches!(
        labels.as_slice(),
        ["ubuntu-latest"
            | "windows-latest"
            | "macos-latest"
            | "macos-13"
            | "macos-14"
            | "macos-15"]
    );
    if !hosted
        || labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case("self-hosted"))
    {
        return false;
    }
    let steps = job
        .get("steps")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    matches!(steps, [step]
        if step.get("name").and_then(Value::as_str) == Some("Set up job")
            && step.get("status").and_then(Value::as_str) == Some("completed")
            && step.get("conclusion").and_then(Value::as_str) == Some("failure"))
}

fn failed_required_check_matches_run(
    check: &crate::merge_steward::StewardCheck,
    run_id: u64,
) -> bool {
    check.source == crate::merge_steward::StewardCheckSource::CheckRun
        && check.app_id == Some(GITHUB_ACTIONS_APP_ID)
        && check.run_id == Some(run_id)
}

fn required_check_has_allowed_conclusion(check: &crate::merge_steward::StewardCheck) -> bool {
    check.conclusion.as_deref().is_some_and(|conclusion| {
        matches!(
            conclusion.to_ascii_lowercase().as_str(),
            "success" | "neutral" | "skipped" | "failure"
        )
    })
}

fn provider_internal_dns_failure(log: &str) -> bool {
    log.contains("internal-api.service.")
        && log.contains(".github.net")
        && log.contains("Name or service not known")
        && log.contains("Failed to download archive")
        && log.contains("after 3 attempts")
}

fn witness_key(repo: &str, pr_number: u64) -> String {
    format!("{repo}#{pr_number}")
}

fn witness_required_checks(
    checks: &[crate::merge_steward::RequiredCheck],
) -> Vec<WitnessRequiredCheck> {
    checks
        .iter()
        .map(|check| WitnessRequiredCheck {
            context: check.context.clone(),
            app_id: check.app_id,
        })
        .collect()
}

fn current_base_sha(
    actions: &GitHubActions,
    observation: &RepoObservation,
) -> Result<String, String> {
    let value = super::gh_json(
        actions,
        &[
            "api".to_owned(),
            format!(
                "repos/{}/commits/{}",
                observation.repo,
                crate::required_check_policy::encode_path_segment(&observation.base)
            ),
        ],
        "merge-queue base revision",
    )?;
    value
        .get("sha")
        .and_then(Value::as_str)
        .filter(|sha| is_full_sha(sha))
        .map(str::to_owned)
        .ok_or_else(|| "merge-queue base revision missing exact SHA".to_owned())
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn merge_group_contains_base(
    actions: &GitHubActions,
    observation: &RepoObservation,
    merge_group_head: &str,
    base_sha: &str,
) -> Result<bool, String> {
    let value = super::gh_json(
        actions,
        &[
            "api".to_owned(),
            format!("repos/{}/commits/{merge_group_head}", observation.repo),
        ],
        "merge-group parent revision",
    )?;
    let parents = value
        .get("parents")
        .and_then(Value::as_array)
        .ok_or_else(|| "merge-group commit missing parents".to_owned())?;
    Ok(parents_include_base(parents, base_sha))
}

fn parents_include_base(parents: &[Value], base_sha: &str) -> bool {
    parents.iter().any(|parent| {
        parent
            .get("sha")
            .and_then(Value::as_str)
            .is_some_and(|sha| sha.eq_ignore_ascii_case(base_sha))
    })
}

fn removal_is_fresh(removed: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    removed <= now && now - removed <= MAX_RECOVERY_AGE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosted_setup_job_requires_one_exact_setup_step_and_hosted_label() {
        let valid = serde_json::json!({
            "runner_group_name": "GitHub Actions",
            "labels": ["ubuntu-latest"],
            "steps": [{"name":"Set up job","status":"completed","conclusion":"failure"}]
        });
        assert!(is_hosted_setup_job(&valid));
        let mut self_hosted = valid.clone();
        self_hosted["labels"] = serde_json::json!(["self-hosted", "linux"]);
        assert!(!is_hosted_setup_job(&self_hosted));
        let mut checkout = valid.clone();
        checkout["steps"] = serde_json::json!([
            {"name":"Set up job","status":"completed","conclusion":"success"},
            {"name":"Checkout","status":"completed","conclusion":"failure"}
        ]);
        assert!(!is_hosted_setup_job(&checkout));
    }

    #[test]
    fn dns_signature_rejects_generic_setup_failure() {
        assert!(provider_internal_dns_failure(
            "Name or service not known (internal-api.service.iad.github.net:443); Failed to download archive after 3 attempts"
        ));
        assert!(!provider_internal_dns_failure(
            "The action version does not exist"
        ));
        assert!(!provider_internal_dns_failure(
            "Could not resolve github.com"
        ));
    }

    #[test]
    fn removal_evidence_expires_after_two_hours() {
        let now = parse_time("2026-08-25T22:00:00Z").expect("now");
        assert!(removal_is_fresh(
            parse_time("2026-08-25T20:00:00Z").expect("boundary"),
            now
        ));
        assert!(!removal_is_fresh(
            parse_time("2026-08-25T19:59:59Z").expect("stale"),
            now
        ));
        assert!(!removal_is_fresh(
            parse_time("2026-08-25T22:00:01Z").expect("future"),
            now
        ));
    }

    #[test]
    fn required_failure_must_be_a_github_actions_check_run() {
        let check = crate::merge_steward::StewardCheck {
            name: "required".to_owned(),
            source: crate::merge_steward::StewardCheckSource::CheckRun,
            app_id: Some(GITHUB_ACTIONS_APP_ID),
            check_run_id: None,
            status: "COMPLETED".to_owned(),
            conclusion: Some("FAILURE".to_owned()),
            run_id: Some(42),
            observed_at: None,
        };
        assert!(failed_required_check_matches_run(&check, 42));
        let mut status = check.clone();
        status.source = crate::merge_steward::StewardCheckSource::StatusContext;
        assert!(!failed_required_check_matches_run(&status, 42));
        let mut foreign_app = check;
        foreign_app.app_id = Some(1);
        assert!(!failed_required_check_matches_run(&foreign_app, 42));
        let mut cancelled = foreign_app;
        cancelled.conclusion = Some("CANCELLED".to_owned());
        assert!(!required_check_has_allowed_conclusion(&cancelled));
    }

    #[test]
    fn speculative_commit_must_name_the_pinned_base_as_a_parent() {
        let parents = serde_json::json!([
            {"sha":"dddddddddddddddddddddddddddddddddddddddd"},
            {"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
        ]);
        let parents = parents.as_array().expect("parents");
        assert!(parents_include_base(
            parents,
            "dddddddddddddddddddddddddddddddddddddddd"
        ));
        assert!(!parents_include_base(
            parents,
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        ));
    }
}
