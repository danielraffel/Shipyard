use super::{
    ActiveRunObservation, BTreeSet, CheckObservation, DateTime, Deserialize, Digest, GitHubActions,
    JobObservation, LoadedConfig, MAX_DETAILED_WORKFLOW_RUNS, MAX_ENROLLMENT_LOOKUPS_PER_TICK,
    MERGE_QUEUE_QUERY, MergeQueueLivenessInputs, MergeQueueProbe, OBSERVATION_MAX_PAGES,
    OBSERVATION_PAGE_SIZE, ObservationReason, Path, PathBuf, QueuedSummary, ReleaseProbe,
    Serialize, Sha256, Utc, Value, assess_merge_queue_liveness, fs, parse_check_observations,
    parse_merge_queue_entries,
};

pub(super) struct ObservedRuns {
    pub(super) runs: Vec<ActiveRunObservation>,
    pub(super) truncated: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct EnrollmentSnapshot {
    entries: Vec<EnrollmentSnapshotEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct EnrollmentSnapshotEntry {
    pr: u64,
    head_sha: Option<String>,
    #[serde(alias = "observed_at")]
    enqueued_at: String,
    #[serde(default)]
    head_observed_at: Option<String>,
    #[serde(default)]
    auto_merge_cleared: bool,
    #[serde(default)]
    last_checked_at: Option<String>,
}

pub(super) fn required_status_checks(config: &LoadedConfig) -> Vec<String> {
    config
        .get("governance.required_status_checks")
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn fetch_merge_queue_entries(
    actions: &GitHubActions,
    owner: &str,
    name: &str,
    base: &str,
    max_pages: u32,
) -> Result<(Vec<crate::merge_queue_liveness::MergeQueueEntry>, bool), String> {
    let mut cursor: Option<String> = None;
    let mut entries = Vec::new();
    for page in 1..=max_pages.max(1) {
        let mut args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={MERGE_QUEUE_QUERY}"),
            "-F".to_owned(),
            format!("owner={owner}"),
            "-F".to_owned(),
            format!("name={name}"),
            "-F".to_owned(),
            format!("branch={base}"),
        ];
        if let Some(cursor) = &cursor {
            args.extend(["-F".to_owned(), format!("cursor={cursor}")]);
        }
        let raw = actions
            .run_gh(&args)
            .map_err(|error| format!("inspect merge queue page {page} failed: {error}"))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("could not parse merge-queue page {page}: {error}"))?;
        entries.extend(parse_merge_queue_entries(&value)?);
        let page_info = value.pointer("/data/repository/mergeQueue/entries/pageInfo");
        let has_next = page_info
            .and_then(|info| info.get("hasNextPage"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !has_next {
            entries.sort_by_key(|entry| entry.position);
            return Ok((entries, false));
        }
        cursor = page_info
            .and_then(|info| info.get("endCursor"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            return Err("merge-queue pagination says more pages but has no endCursor".to_owned());
        }
    }
    entries.sort_by_key(|entry| entry.position);
    Ok((entries, true))
}

pub(super) fn enrollment_snapshot_path(state_dir: &Path, repo: &str, base: &str) -> PathBuf {
    let digest = format!("{:x}", Sha256::digest(format!("{repo}\0{base}").as_bytes()));
    let key = format!("{}-{}", repo.replace('/', "-"), &digest[..24]);
    state_dir.join("fleet-liveness").join(format!("{key}.json"))
}

#[allow(clippy::too_many_lines)]
pub(super) fn reconcile_enrollment_snapshot(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    state_dir: &Path,
    entries: &mut [crate::merge_queue_liveness::MergeQueueEntry],
    queue_snapshot_complete: bool,
) -> Result<(Vec<u64>, bool), String> {
    let path = enrollment_snapshot_path(state_dir, repo, base);
    let previous = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<EnrollmentSnapshot>(&raw)
            .map_err(|error| format!("parse fleet enrollment snapshot failed: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => EnrollmentSnapshot::default(),
        Err(error) => return Err(format!("read fleet enrollment snapshot failed: {error}")),
    };
    let observed_now = Utc::now().to_rfc3339();
    for entry in entries.iter_mut() {
        let previous_entry = previous.entries.iter().find(|candidate| {
            candidate.pr == entry.pr
                && candidate.head_sha == entry.head_sha
                && entry.enqueued_at.as_deref() == Some(candidate.enqueued_at.as_str())
        });
        entry.head_observed_at = Some(
            previous_entry
                .and_then(|candidate| candidate.head_observed_at.clone())
                .unwrap_or_else(|| observed_now.clone()),
        );
    }
    let current = entries
        .iter()
        .map(|entry| entry.pr)
        .collect::<BTreeSet<_>>();
    let mut cleared = Vec::new();
    let mut retained = Vec::new();
    let mut candidates = previous
        .entries
        .into_iter()
        .filter(|entry| !current.contains(&entry.pr))
        .collect::<Vec<_>>();
    if !queue_snapshot_complete {
        retained.append(&mut candidates);
    }
    candidates.sort_by(|left, right| {
        left.last_checked_at
            .as_deref()
            .unwrap_or(&left.enqueued_at)
            .cmp(
                right
                    .last_checked_at
                    .as_deref()
                    .unwrap_or(&right.enqueued_at),
            )
    });
    let mut truncated = false;
    for (index, previous_entry) in candidates.into_iter().enumerate() {
        if index >= MAX_ENROLLMENT_LOOKUPS_PER_TICK {
            if previous_entry.auto_merge_cleared {
                cleared.push(previous_entry.pr);
            }
            retained.push(previous_entry);
            truncated = true;
            continue;
        }
        let raw = actions
            .run_gh(&[
                "api".to_owned(),
                format!("repos/{repo}/pulls/{}", previous_entry.pr),
            ])
            .map_err(|error| {
                format!(
                    "inspect prior queue PR #{} enrollment failed: {error}",
                    previous_entry.pr
                )
            })?;
        let pull: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("could not parse prior queue PR JSON: {error}"))?;
        if pull.get("state").and_then(Value::as_str) == Some("open") {
            let pull_base = pull
                .pointer("/base/ref")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "prior queue PR #{} response missing base.ref",
                        previous_entry.pr
                    )
                })?;
            if pull_base != base {
                continue;
            }
            retained.push(EnrollmentSnapshotEntry {
                pr: previous_entry.pr,
                head_sha: previous_entry.head_sha,
                enqueued_at: previous_entry.enqueued_at,
                head_observed_at: previous_entry.head_observed_at,
                auto_merge_cleared: pull.get("auto_merge").is_none_or(Value::is_null),
                last_checked_at: Some(Utc::now().to_rfc3339()),
            });
            if pull.get("auto_merge").is_none_or(Value::is_null) {
                cleared.push(previous_entry.pr);
            }
        }
    }
    let snapshot = EnrollmentSnapshot {
        entries: entries
            .iter()
            .map(|entry| EnrollmentSnapshotEntry {
                pr: entry.pr,
                head_sha: entry.head_sha.clone(),
                enqueued_at: entry
                    .enqueued_at
                    .clone()
                    .unwrap_or_else(|| observed_now.clone()),
                head_observed_at: entry.head_observed_at.clone(),
                auto_merge_cleared: false,
                last_checked_at: None,
            })
            .chain(retained)
            .collect(),
    };
    let parent = path
        .parent()
        .ok_or_else(|| "fleet snapshot path has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create fleet snapshot directory failed: {error}"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("create fleet snapshot temp file failed: {error}"))?;
    serde_json::to_writer(&mut temp, &snapshot)
        .map_err(|error| format!("serialize fleet snapshot failed: {error}"))?;
    temp.persist(&path)
        .map_err(|error| format!("persist fleet snapshot failed: {error}"))?;
    cleared.sort_unstable();
    Ok((cleared, truncated))
}

pub(super) fn classify_observation_error(reason: &str) -> ObservationReason {
    let reason = reason.to_ascii_lowercase();
    if reason.contains("rate limit")
        || reason.contains("rate_limit")
        || reason.contains("secondary rate")
    {
        ObservationReason::GitHubRateLimited
    } else if reason.contains("authentication")
        || reason.contains("bad credentials")
        || reason.contains("resource not accessible")
        || reason.contains("forbidden")
        || reason.contains("unauthorized")
        || reason.contains("http 401")
        || reason.contains("http 403")
    {
        ObservationReason::GitHubAuthFailed
    } else {
        ObservationReason::GitHubObservationFailed
    }
}

pub(super) fn observation_reason_codes(
    merge_queue: &MergeQueueProbe,
    release: &ReleaseProbe,
) -> Vec<ObservationReason> {
    let mut reasons = merge_queue
        .reason_codes
        .iter()
        .chain(release.reason_codes.iter())
        .copied()
        .collect::<Vec<_>>();
    if release
        .report
        .as_ref()
        .is_some_and(|report| report.stale_with_unreleased_commits)
    {
        reasons.push(ObservationReason::ReleaseStale);
    }
    reasons.sort_by_key(|reason| format!("{reason:?}"));
    reasons.dedup();
    reasons
}

#[allow(clippy::too_many_arguments)]
pub(super) fn inspect_merge_queue_liveness(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    state_dir: &Path,
    required_contexts: &[String],
    eligible_host_classes: &[String],
    routable_free_slots: u32,
    stall_threshold_secs: i64,
    active_runs: &[ActiveRunObservation],
    observation_truncated: bool,
) -> Result<MergeQueueProbe, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository slug `{repo}`"))?;
    let (mut entries, queue_truncated) =
        fetch_merge_queue_entries(actions, owner, name, base, OBSERVATION_MAX_PAGES)?;
    let (enrollment_cleared_prs, enrollment_truncated) = reconcile_enrollment_snapshot(
        actions,
        repo,
        base,
        state_dir,
        &mut entries,
        !queue_truncated,
    )?;
    let mut observation_truncated =
        observation_truncated || queue_truncated || enrollment_truncated;
    let Some(front) = entries.first() else {
        return Ok(MergeQueueProbe {
            readable: true,
            source: "github (queue empty or not configured)".to_owned(),
            reason_codes: observation_truncated
                .then_some(ObservationReason::ObservationTruncated)
                .into_iter()
                .collect(),
            report: Some(assess_merge_queue_liveness(MergeQueueLivenessInputs {
                entries: &[],
                checks: &[],
                active_runs,
                required_contexts,
                eligible_host_classes,
                routable_free_slots,
                stall_threshold_secs,
                now: Utc::now(),
                enrollment_cleared_prs: &enrollment_cleared_prs,
                observation_truncated,
            })),
        });
    };

    let checks = match front.head_sha.as_deref() {
        Some(sha) => {
            let (checks, truncated) = fetch_check_observations(actions, repo, sha)?;
            observation_truncated |= truncated;
            checks
        }
        None => Vec::new(),
    };
    Ok(MergeQueueProbe {
        readable: true,
        source: "github".to_owned(),
        reason_codes: observation_truncated
            .then_some(ObservationReason::ObservationTruncated)
            .into_iter()
            .collect(),
        report: Some(assess_merge_queue_liveness(MergeQueueLivenessInputs {
            entries: &entries,
            checks: &checks,
            active_runs,
            required_contexts,
            eligible_host_classes,
            routable_free_slots,
            stall_threshold_secs,
            now: Utc::now(),
            enrollment_cleared_prs: &enrollment_cleared_prs,
            observation_truncated,
        })),
    })
}

pub(super) fn fetch_check_observations(
    actions: &GitHubActions,
    repo: &str,
    sha: &str,
) -> Result<(Vec<CheckObservation>, bool), String> {
    let mut checks = Vec::new();
    let mut truncated = false;
    for page in 1..=OBSERVATION_MAX_PAGES {
        let raw = actions
            .run_gh(&[
                "api".to_owned(),
                format!(
                    "repos/{repo}/commits/{sha}/check-runs?per_page={OBSERVATION_PAGE_SIZE}&page={page}"
                ),
            ])
            .map_err(|error| format!("inspect front merge-group checks failed: {error}"))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("could not parse front check-runs JSON: {error}"))?;
        let page_checks = parse_check_observations(&value)?;
        let page_len = page_checks.len();
        checks.extend(page_checks);
        if page_len < OBSERVATION_PAGE_SIZE {
            break;
        }
        if page == OBSERVATION_MAX_PAGES {
            truncated = true;
        }
    }
    for page in 1..=OBSERVATION_MAX_PAGES {
        let raw = actions
            .run_gh(&[
                "api".to_owned(),
                format!(
                    "repos/{repo}/commits/{sha}/statuses?per_page={OBSERVATION_PAGE_SIZE}&page={page}"
                ),
            ])
            .map_err(|error| format!("inspect front commit statuses failed: {error}"))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("could not parse front commit-statuses JSON: {error}"))?;
        let statuses = value
            .as_array()
            .ok_or_else(|| "commit-statuses response is not an array".to_owned())?;
        checks.extend(statuses.iter().filter_map(|status| {
            let state = status.get("state")?.as_str()?;
            Some(CheckObservation {
                name: status.get("context")?.as_str()?.to_owned(),
                status: if state == "pending" {
                    "in_progress".to_owned()
                } else {
                    "completed".to_owned()
                },
                started_at: status
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .or_else(|| status.get("created_at").and_then(Value::as_str))
                    .map(str::to_owned),
                conclusion: (state != "pending").then(|| {
                    if state == "error" {
                        "failure".to_owned()
                    } else {
                        state.to_owned()
                    }
                }),
            })
        }));
        if statuses.len() < OBSERVATION_PAGE_SIZE {
            break;
        }
        if page == OBSERVATION_MAX_PAGES {
            truncated = true;
        }
    }
    Ok((checks, truncated))
}

pub(super) fn fetch_observed_workflow_runs(
    actions: &GitHubActions,
    repo: &str,
    run_limit: u32,
) -> Result<ObservedRuns, String> {
    let limit = usize::try_from(run_limit.clamp(1, MAX_DETAILED_WORKFLOW_RUNS))
        .expect("u32 run limit fits usize");
    let mut runs_by_status = Vec::new();
    let mut truncated = false;
    for status in ["in_progress", "queued"] {
        let raw = actions
            .run_gh(&[
                "api".to_owned(),
                format!(
                    "repos/{repo}/actions/runs?status={status}&per_page={OBSERVATION_PAGE_SIZE}&page=1"
                ),
            ])
            .map_err(|error| format!("list {status} workflow runs failed: {error}"))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("could not parse {status} workflow runs JSON: {error}"))?;
        let runs = value
            .get("workflow_runs")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{status} workflow runs response missing workflow_runs"))?;
        truncated |= runs.len() == OBSERVATION_PAGE_SIZE;
        runs_by_status.push(runs.clone());
    }
    let raw_runs = select_bounded_runs(&runs_by_status, limit);
    truncated |= runs_by_status.iter().map(Vec::len).sum::<usize>() > raw_runs.len();
    let mut observations = Vec::new();
    for run in &raw_runs {
        let Some(run_id) = run.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let (jobs, jobs_truncated) = fetch_run_jobs(actions, repo, run_id)?;
        truncated |= jobs_truncated;
        observations.push(ActiveRunObservation {
            run_id,
            workflow: run
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            head_branch: run
                .get("head_branch")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            head_sha: run
                .get("head_sha")
                .and_then(Value::as_str)
                .map(str::to_owned),
            status: run
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            created_at: run
                .get("created_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
            pull_requests: run
                .get("pull_requests")
                .and_then(Value::as_array)
                .map(|pull_requests| {
                    pull_requests
                        .iter()
                        .filter_map(|pr| pr.get("number").and_then(Value::as_u64))
                        .collect()
                })
                .unwrap_or_default(),
            url: run
                .get("html_url")
                .and_then(Value::as_str)
                .map(str::to_owned),
            jobs,
        });
    }
    Ok(ObservedRuns {
        runs: observations,
        truncated,
    })
}

pub(super) fn fetch_run_jobs(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
) -> Result<(Vec<JobObservation>, bool), String> {
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            format!(
                "repos/{repo}/actions/runs/{run_id}/jobs?per_page={OBSERVATION_PAGE_SIZE}&page=1"
            ),
        ])
        .map_err(|error| format!("list jobs for workflow run {run_id} failed: {error}"))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("could not parse jobs for workflow run {run_id}: {error}"))?;
    let jobs = value
        .get("jobs")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("workflow run {run_id} response missing jobs"))?;
    Ok((
        parse_job_observations(jobs),
        jobs.len() == OBSERVATION_PAGE_SIZE,
    ))
}

pub(super) fn select_bounded_runs(runs_by_status: &[Vec<Value>], limit: usize) -> Vec<Value> {
    let mut selected = Vec::with_capacity(limit);
    let mut indices = vec![0usize; runs_by_status.len()];
    while selected.len() < limit {
        let mut progressed = false;
        for (status_index, runs) in runs_by_status.iter().enumerate() {
            let index = &mut indices[status_index];
            if *index < runs.len() && selected.len() < limit {
                selected.push(runs[*index].clone());
                *index += 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    selected
}

pub(super) fn parse_job_observations(jobs: &[Value]) -> Vec<JobObservation> {
    jobs.iter()
        .filter_map(|job| {
            Some(JobObservation {
                name: job.get("name")?.as_str()?.to_owned(),
                status: job.get("status")?.as_str()?.to_owned(),
                runner_name: job
                    .get("runner_name")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
                labels: job
                    .get("labels")
                    .and_then(Value::as_array)
                    .map(|labels| {
                        labels
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

pub(super) fn queued_macos_summary(runs: &[ActiveRunObservation], target: &str) -> QueuedSummary {
    let mut count = 0usize;
    let mut oldest_age_secs: Option<i64> = None;
    let now = Utc::now();
    for run in runs {
        if !run.jobs.iter().any(|job| {
            job.status == "queued"
                && job
                    .name
                    .to_ascii_lowercase()
                    .contains(&target.to_ascii_lowercase())
        }) {
            continue;
        }
        count += 1;
        // A downstream job can become queued long after its workflow starts.
        // Without a job-level queued timestamp, only a wholly queued workflow
        // has an authoritative age proxy. Still count downstream work, but do
        // not turn upstream runtime into a false queue-age alert.
        if run.status != "queued" {
            continue;
        }
        if let Some(created_at) = run.created_at.as_deref()
            && let Ok(ts) = DateTime::parse_from_rfc3339(created_at)
        {
            let age = (now - ts.with_timezone(&Utc)).num_seconds().max(0);
            oldest_age_secs = Some(oldest_age_secs.map_or(age, |oldest| oldest.max(age)));
        }
    }
    QueuedSummary {
        readable: true,
        source: "github".to_owned(),
        count,
        oldest_age_secs,
    }
}
