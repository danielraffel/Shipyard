//! Read-only merge-queue liveness classification for fleet monitoring.
//!
//! The transport lives in `app::fleet_status_cmd`; this module only parses
//! GitHub observations and correlates them with eligible self-hosted capacity.
//! It deliberately never cancels workflow runs.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

/// One entry in the repository's merge queue.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MergeQueueEntry {
    /// Pull request number.
    pub pr: u64,
    /// Zero-based queue position.
    pub position: u64,
    /// Speculative merge-group commit, when GitHub has materialized it.
    pub head_sha: Option<String>,
    /// Time at which GitHub enqueued the entry.
    pub enqueued_at: Option<String>,
    /// First durable observation of this exact merge-group head within the
    /// current queue enrollment.
    pub head_observed_at: Option<String>,
}

/// One check run observed on the front merge-group commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckObservation {
    /// Check-run context name.
    pub name: String,
    /// GitHub status (`queued`, `in_progress`, or `completed`).
    pub status: String,
    /// Time at which GitHub created or started the check.
    pub started_at: Option<String>,
    /// Terminal conclusion, when completed.
    pub conclusion: Option<String>,
}

/// One workflow job observed on an active merge-group run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobObservation {
    /// Job display name.
    pub name: String,
    /// GitHub status.
    pub status: String,
    /// Runner name, when the job has been assigned.
    pub runner_name: Option<String>,
    /// Job labels returned by GitHub.
    pub labels: Vec<String>,
}

/// One active workflow run and its jobs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveRunObservation {
    /// Workflow run database ID.
    pub run_id: u64,
    /// Workflow name.
    pub workflow: String,
    /// Merge-group branch.
    pub head_branch: String,
    /// Exact workflow-run head commit.
    pub head_sha: Option<String>,
    /// GitHub workflow-run status.
    pub status: String,
    /// Time at which GitHub created the run.
    pub created_at: Option<String>,
    /// Pull requests associated with this run.
    pub pull_requests: Vec<u64>,
    /// Browser URL, when present.
    pub url: Option<String>,
    /// Jobs in the run.
    pub jobs: Vec<JobObservation>,
}

/// Why a non-front merge-group run is consuming eligible fleet capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OccupierKind {
    /// Work outside the merge queue is consuming queue-eligible capacity.
    OptionalNonQueue,
    /// The PR is still in the queue, but it is not the front entry.
    NonFront,
    /// The PR is no longer in the queue; the run is superseded.
    Superseded,
}

/// Stable, agent-neutral reason codes emitted by `runner fleet-status`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LivenessReason {
    /// Queue has no front entry.
    QueueEmpty,
    /// Front is progressing; followers are in a normal serialized wait.
    NormalSerialWait,
    /// A required front context is missing, queued, or stale in progress.
    FrontRequiredStaleOrMissing,
    /// A configured required context has a terminal failing conclusion.
    FrontRequiredFailed,
    /// Routable queue-eligible capacity is idle while required work is stalled.
    IdleEligibleCapacity,
    /// Non-queue work owns queue-eligible capacity.
    OptionalCapacityTheft,
    /// A superseded merge-group run owns queue-eligible capacity.
    SupersededCapacityTheft,
    /// A later queue entry owns queue-eligible capacity.
    NonFrontCapacityOwner,
    /// A previously queued PR is now open without auto-merge enrollment.
    AutoMergeEnrollmentCleared,
    /// A bounded GitHub observation omitted additional records.
    ObservationTruncated,
}

/// An active non-front merge-group job assigned to an eligible runner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CapacityOccupier {
    /// Workflow run database ID.
    pub run_id: u64,
    /// Pull request encoded in the merge-group branch.
    pub pr: Option<u64>,
    /// Workflow name.
    pub workflow: String,
    /// Job name.
    pub job: String,
    /// Assigned runner.
    pub runner_name: String,
    /// Whether the run is non-front or fully superseded.
    pub kind: OccupierKind,
    /// Browser URL, when present.
    pub url: Option<String>,
}

/// Read-only queue/fleet correlation result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MergeQueueLivenessReport {
    /// Queue front, if any.
    pub front: Option<MergeQueueEntry>,
    /// Configured required contexts used for matching. Empty means all observed
    /// checks were treated as liveness signals.
    pub required_contexts: Vec<String>,
    /// Number of matching check runs materialized on the front merge group.
    pub materialized_required_checks: usize,
    /// Number of matching checks that have started or completed.
    pub progressed_required_checks: usize,
    /// Required contexts that have not materialized, remain queued, or have
    /// stayed in progress past the stall threshold.
    pub stalled_required_contexts: Vec<String>,
    /// Required contexts with a terminal failing conclusion. When governance
    /// exposes no configured names, all observed checks are liveness signals.
    pub failed_required_contexts: Vec<String>,
    /// True when an aged front has no required-check progress despite idle,
    /// routable M1/M3/M5 capacity.
    pub front_stalled_with_idle_capacity: bool,
    /// True when an aged front is stalled while optional or superseded work
    /// occupies queue-eligible capacity.
    pub front_blocked_by_capacity_occupiers: bool,
    /// Active merge-group jobs consuming eligible runners away from the front.
    pub capacity_occupiers: Vec<CapacityOccupier>,
    /// Previously queued PRs now open without auto-merge enrollment.
    pub enrollment_cleared_prs: Vec<u64>,
    /// Stable reasons for agents, schedulers, and dashboards.
    pub reason_codes: Vec<LivenessReason>,
}

/// Latest-release freshness relative to the monitored base branch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReleaseLivenessReport {
    /// Latest release tag.
    pub tag: String,
    /// Latest release publication timestamp.
    pub published_at: String,
    /// Commits by which the base branch is ahead of the release tag.
    pub commits_ahead: u64,
    /// Commits that are not obvious docs/changelog/skip-only updates.
    pub releasable_commits_ahead: u64,
    /// Oldest classified releasable commit after the latest release.
    pub oldest_releasable_commit_at: Option<String>,
    /// Version recorded on the monitored base, when exposed by the repository.
    pub base_version: Option<String>,
    /// Release tag normalized to a plain version.
    pub released_version: String,
    /// Whether the base version has failed to move beyond the release.
    pub version_unchanged: Option<bool>,
    /// Open issue count whose title describes a release incident.
    pub open_release_incident_issues: Option<u64>,
    /// Most recent successful auto-release workflow completion, when present.
    pub latest_successful_release_workflow_at: Option<String>,
    /// Age of releasable work, bounded to begin no earlier than publication.
    pub age_secs: i64,
    /// True when bounded releasable-work age exceeds policy.
    pub stale_with_unreleased_commits: bool,
}

/// Classify release freshness without making any release mutation.
#[allow(clippy::too_many_arguments)]
pub fn assess_release_liveness(
    tag: String,
    published_at: String,
    commits_ahead: u64,
    releasable_commits_ahead: u64,
    oldest_releasable_commit_at: Option<String>,
    base_version: Option<String>,
    open_release_incident_issues: Option<u64>,
    latest_successful_release_workflow_at: Option<String>,
    stale_threshold_secs: i64,
    now: DateTime<Utc>,
) -> Result<ReleaseLivenessReport, String> {
    let published = DateTime::parse_from_rfc3339(&published_at)
        .map_err(|error| format!("invalid latest release published_at: {error}"))?
        .with_timezone(&Utc);
    if published > now {
        return Err(format!(
            "latest release published_at {published} is in the future"
        ));
    }
    let age_secs = oldest_releasable_commit_at
        .as_deref()
        .map(DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|error| format!("invalid oldest releasable commit timestamp: {error}"))?
        .map(|committed| committed.with_timezone(&Utc))
        .map_or_else(
            || {
                Ok(if releasable_commits_ahead > 0 {
                    (now - published).num_seconds().max(0)
                } else {
                    0
                })
            },
            |committed| {
                if committed > now {
                    return Err(format!(
                        "oldest releasable commit timestamp {committed} is in the future"
                    ));
                }
                Ok((now - committed.max(published)).num_seconds().max(0))
            },
        );
    let age_secs = age_secs?;
    let released_version = tag.trim_start_matches('v').to_owned();
    let version_unchanged = base_version
        .as_deref()
        .map(|version| version.trim_start_matches('v') == released_version);
    Ok(ReleaseLivenessReport {
        tag,
        published_at,
        commits_ahead,
        releasable_commits_ahead,
        oldest_releasable_commit_at,
        base_version,
        released_version,
        version_unchanged,
        open_release_incident_issues,
        latest_successful_release_workflow_at,
        age_secs,
        stale_with_unreleased_commits: releasable_commits_ahead > 0
            && age_secs >= stale_threshold_secs.max(0),
    })
}

impl MergeQueueLivenessReport {
    /// Whether the observation should make a fleet monitor exit non-zero.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        !self.failed_required_contexts.is_empty()
            || self.front_stalled_with_idle_capacity
            || self.front_blocked_by_capacity_occupiers
            || self
                .capacity_occupiers
                .iter()
                .any(|occupier| occupier.kind == OccupierKind::Superseded)
            || !self.enrollment_cleared_prs.is_empty()
    }
}

/// Inputs for one stateless liveness assessment.
#[derive(Clone, Copy)]
pub struct MergeQueueLivenessInputs<'a> {
    /// Current merge-queue entries.
    pub entries: &'a [MergeQueueEntry],
    /// Checks observed on the front merge-group SHA.
    pub checks: &'a [CheckObservation],
    /// Active workflow runs and their jobs.
    pub active_runs: &'a [ActiveRunObservation],
    /// Required contexts from repository governance.
    pub required_contexts: &'a [String],
    /// Fleet host-class names eligible for local work.
    pub eligible_host_classes: &'a [String],
    /// Idle, routable slots across those host classes.
    pub routable_free_slots: u32,
    /// Queue-front age required before alerting.
    pub stall_threshold_secs: i64,
    /// Observation time.
    pub now: DateTime<Utc>,
    /// Durable enrollment-loss observations from the transport.
    pub enrollment_cleared_prs: &'a [u64],
    /// Whether a bounded observation omitted records.
    pub observation_truncated: bool,
}

/// Parse merge-queue entries from the dedicated GraphQL observation.
pub fn parse_merge_queue_entries(body: &Value) -> Result<Vec<MergeQueueEntry>, String> {
    if let Some(errors) = body
        .get("errors")
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty())
    {
        let details = errors
            .iter()
            .map(|error| {
                error.get("message").and_then(Value::as_str).map_or_else(
                    || error.to_string(),
                    |message| format!("{message} ({error})"),
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!(
            "merge-queue GraphQL response contains errors: {details}"
        ));
    }
    let queue = body
        .pointer("/data/repository/mergeQueue")
        .ok_or_else(|| "merge-queue response missing repository.mergeQueue".to_owned())?;
    if queue.is_null() {
        return Ok(Vec::new());
    }
    let nodes = queue
        .pointer("/entries/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "merge-queue response missing entries.nodes".to_owned())?;
    let mut entries = Vec::with_capacity(nodes.len());
    for node in nodes {
        let pr = node
            .pointer("/pullRequest/number")
            .and_then(Value::as_u64)
            .ok_or_else(|| "merge-queue entry missing pullRequest.number".to_owned())?;
        let position = node
            .get("position")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("merge-queue entry for PR #{pr} missing position"))?;
        entries.push(MergeQueueEntry {
            pr,
            position,
            head_sha: node
                .pointer("/headCommit/oid")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            enqueued_at: node
                .get("enqueuedAt")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            head_observed_at: None,
        });
    }
    entries.sort_by_key(|entry| entry.position);
    Ok(entries)
}

/// Parse check runs from the GitHub checks API.
pub fn parse_check_observations(body: &Value) -> Result<Vec<CheckObservation>, String> {
    let checks = body
        .get("check_runs")
        .and_then(Value::as_array)
        .ok_or_else(|| "check-runs response missing check_runs".to_owned())?;
    Ok(checks
        .iter()
        .filter_map(|check| {
            Some(CheckObservation {
                name: check.get("name")?.as_str()?.to_owned(),
                status: check.get("status")?.as_str()?.to_owned(),
                started_at: check
                    .get("started_at")
                    .and_then(Value::as_str)
                    .or_else(|| check.get("created_at").and_then(Value::as_str))
                    .map(str::to_owned),
                conclusion: check
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect())
}

/// Extract a PR number from GitHub's merge-group branch convention.
#[must_use]
pub fn merge_group_pr(branch: &str) -> Option<u64> {
    let rest = branch.strip_prefix("gh-readonly-queue/")?;
    let marker = rest.rfind("/pr-")?;
    rest[marker + 4..]
        .split('-')
        .next()
        .and_then(|value| value.parse().ok())
}

fn entry_liveness_started_at(entry: &MergeQueueEntry) -> Option<DateTime<Utc>> {
    let parse = |timestamp: Option<&str>| {
        timestamp
            .map(DateTime::parse_from_rfc3339)
            .transpose()
            .map(|value| value.map(|timestamp| timestamp.with_timezone(&Utc)))
    };
    let enqueued_at = parse(entry.enqueued_at.as_deref()).ok()?;
    let head_observed_at = parse(entry.head_observed_at.as_deref()).ok()?;
    [enqueued_at, head_observed_at].into_iter().flatten().max()
}

/// Correlate merge-queue, check-run, active-run, and fleet observations.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn assess_merge_queue_liveness(
    inputs: MergeQueueLivenessInputs<'_>,
) -> MergeQueueLivenessReport {
    let MergeQueueLivenessInputs {
        entries,
        checks,
        active_runs,
        required_contexts,
        eligible_host_classes,
        routable_free_slots,
        stall_threshold_secs,
        now,
        enrollment_cleared_prs,
        observation_truncated,
    } = inputs;
    let front = entries.iter().min_by_key(|entry| entry.position).cloned();
    let front_old_enough = front.as_ref().is_some_and(|entry| {
        entry_liveness_started_at(entry).is_some_and(|liveness_started_at| {
            (now - liveness_started_at).num_seconds() >= stall_threshold_secs.max(0)
        })
    });
    let current_checks = current_check_observations(checks);
    let matching_checks = current_checks
        .iter()
        .filter(|check| {
            required_contexts.is_empty()
                || required_contexts
                    .iter()
                    .any(|required| required.eq_ignore_ascii_case(&check.name))
        })
        .collect::<Vec<_>>();
    let progressed_required_checks = matching_checks
        .iter()
        .filter(|check| check.status != "queued")
        .count();
    let stalled_required_contexts = stalled_contexts(
        &current_checks,
        required_contexts,
        stall_threshold_secs,
        now,
        front_old_enough,
    );
    let failed_required_contexts: Vec<String> = if required_contexts.is_empty() {
        current_checks
            .iter()
            .filter(|check| is_terminal_failure(check.conclusion.as_deref()))
            .map(|check| check.name.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    } else {
        required_contexts
            .iter()
            .filter(|required| {
                current_checks.iter().any(|check| {
                    required.eq_ignore_ascii_case(&check.name)
                        && is_terminal_failure(check.conclusion.as_deref())
                })
            })
            .cloned()
            .collect()
    };
    let queued_prs = entries
        .iter()
        .map(|entry| entry.pr)
        .collect::<BTreeSet<_>>();
    let front_pr = front.as_ref().map(|entry| entry.pr);
    let mut capacity_occupiers = Vec::new();
    for run in active_runs {
        let merge_pr = merge_group_pr(&run.head_branch);
        // GitHub materializes MergeQueueEntry.headCommit as the temporary
        // merge-group commit (distinct from pullRequest.headRefOid). Actions
        // merge_group runs use that same commit as workflow_run.head_sha.
        let exact_front = merge_pr == front_pr
            && front
                .as_ref()
                .and_then(|entry| entry.head_sha.as_deref())
                .zip(run.head_sha.as_deref())
                .is_some_and(|(front_sha, run_sha)| front_sha == run_sha);
        if exact_front {
            continue;
        }
        let kind = match merge_pr {
            Some(pr) if Some(pr) == front_pr => OccupierKind::Superseded,
            Some(pr) if queued_prs.contains(&pr) => OccupierKind::NonFront,
            Some(_) => OccupierKind::Superseded,
            None => OccupierKind::OptionalNonQueue,
        };
        for job in &run.jobs {
            if job.status != "in_progress" || !job_is_on_eligible_host(job, eligible_host_classes) {
                continue;
            }
            let Some(runner_name) = job.runner_name.clone() else {
                continue;
            };
            capacity_occupiers.push(CapacityOccupier {
                run_id: run.run_id,
                pr: merge_pr.or_else(|| run.pull_requests.first().copied()),
                workflow: run.workflow.clone(),
                job: job.name.clone(),
                runner_name,
                kind,
                url: run.url.clone(),
            });
        }
    }
    let required_work_stalled = front_old_enough && !stalled_required_contexts.is_empty();
    let front_stalled_with_idle_capacity = required_work_stalled && routable_free_slots > 0;
    let front_blocked_by_capacity_occupiers =
        required_work_stalled && !capacity_occupiers.is_empty();

    let mut reason_codes = Vec::new();
    if front.is_none() {
        reason_codes.push(LivenessReason::QueueEmpty);
    } else if stalled_required_contexts.is_empty() && failed_required_contexts.is_empty() {
        reason_codes.push(LivenessReason::NormalSerialWait);
    }
    if !stalled_required_contexts.is_empty() {
        reason_codes.push(LivenessReason::FrontRequiredStaleOrMissing);
    }
    if !failed_required_contexts.is_empty() {
        reason_codes.push(LivenessReason::FrontRequiredFailed);
    }
    if front_stalled_with_idle_capacity {
        reason_codes.push(LivenessReason::IdleEligibleCapacity);
    }
    if !enrollment_cleared_prs.is_empty() {
        reason_codes.push(LivenessReason::AutoMergeEnrollmentCleared);
    }
    if observation_truncated {
        reason_codes.push(LivenessReason::ObservationTruncated);
    }
    for occupier in &capacity_occupiers {
        if !front_blocked_by_capacity_occupiers && occupier.kind != OccupierKind::Superseded {
            continue;
        }
        let reason = match occupier.kind {
            OccupierKind::OptionalNonQueue => LivenessReason::OptionalCapacityTheft,
            OccupierKind::Superseded => LivenessReason::SupersededCapacityTheft,
            OccupierKind::NonFront => LivenessReason::NonFrontCapacityOwner,
        };
        if !reason_codes.contains(&reason) {
            reason_codes.push(reason);
        }
    }

    MergeQueueLivenessReport {
        front,
        required_contexts: required_contexts.to_vec(),
        materialized_required_checks: matching_checks.len(),
        progressed_required_checks,
        stalled_required_contexts,
        failed_required_contexts,
        front_stalled_with_idle_capacity,
        front_blocked_by_capacity_occupiers,
        capacity_occupiers,
        enrollment_cleared_prs: enrollment_cleared_prs.to_vec(),
        reason_codes,
    }
}

fn stalled_contexts(
    checks: &[CheckObservation],
    required_contexts: &[String],
    stall_threshold_secs: i64,
    now: DateTime<Utc>,
    front_old_enough: bool,
) -> Vec<String> {
    if !front_old_enough {
        return Vec::new();
    }
    let stalled = |check: &CheckObservation| {
        check.status == "queued"
            || (check.status == "in_progress"
                && check.started_at.as_deref().is_some_and(|started_at| {
                    DateTime::parse_from_rfc3339(started_at).is_ok_and(|started| {
                        (now - started.with_timezone(&Utc)).num_seconds()
                            >= stall_threshold_secs.max(0)
                    })
                }))
    };
    if required_contexts.is_empty() {
        if checks.is_empty() && front_old_enough {
            return vec!["at-least-one-current-head-check".to_owned()];
        }
        return checks
            .iter()
            .filter(|check| stalled(check))
            .map(|check| check.name.clone())
            .collect();
    }
    required_contexts
        .iter()
        .filter(|required| {
            let matches = checks
                .iter()
                .filter(|check| required.eq_ignore_ascii_case(&check.name))
                .collect::<Vec<_>>();
            matches.is_empty() || matches.iter().all(|check| stalled(check))
        })
        .cloned()
        .collect()
}

fn current_check_observations(checks: &[CheckObservation]) -> Vec<CheckObservation> {
    let mut current = std::collections::BTreeMap::<String, &CheckObservation>::new();
    for check in checks {
        let key = check.name.to_ascii_lowercase();
        current
            .entry(key)
            .and_modify(|observed| {
                if check_observation_recency(check) > check_observation_recency(observed) {
                    *observed = check;
                }
            })
            .or_insert(check);
    }
    current.into_values().cloned().collect()
}

fn check_observation_recency(check: &CheckObservation) -> (bool, &str, bool) {
    (
        check.started_at.is_none() && !check.status.eq_ignore_ascii_case("completed"),
        check.started_at.as_deref().unwrap_or_default(),
        check.status.eq_ignore_ascii_case("completed"),
    )
}

fn is_terminal_failure(conclusion: Option<&str>) -> bool {
    matches!(
        conclusion,
        Some(
            "failure" | "cancelled" | "timed_out" | "action_required" | "startup_failure" | "stale"
        )
    )
}

fn job_is_on_eligible_host(job: &JobObservation, classes: &[String]) -> bool {
    job.runner_name
        .iter()
        .chain(job.labels.iter())
        .any(|value| {
            classes
                .iter()
                .any(|class| delimited_class_match(value, class))
        })
}

fn delimited_class_match(value: &str, class: &str) -> bool {
    let value = value.as_bytes();
    let class = class.as_bytes();
    !class.is_empty()
        && value
            .windows(class.len())
            .enumerate()
            .any(|(start, window)| {
                let end = start + class.len();
                window.eq_ignore_ascii_case(class)
                    && (start == 0 || !value[start - 1].is_ascii_alphanumeric())
                    && (end == value.len() || !value[end].is_ascii_alphanumeric())
            })
}

#[cfg(test)]
mod tests;
