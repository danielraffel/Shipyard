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
}

/// One check run observed on the front merge-group commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckObservation {
    /// Check-run context name.
    pub name: String,
    /// GitHub status (`queued`, `in_progress`, or `completed`).
    pub status: String,
    /// Time at which GitHub started the check.
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
    /// Required contexts with a terminal failing conclusion. Advisory checks
    /// are excluded because matching is limited to governance-required names.
    pub failed_required_contexts: Vec<String>,
    /// True when an aged front has no required-check progress despite idle,
    /// routable M1/M3/M5 capacity.
    pub front_stalled_with_idle_capacity: bool,
    /// True when an aged front is stalled while optional or superseded work
    /// occupies queue-eligible capacity.
    pub front_blocked_by_capacity_occupiers: bool,
    /// Active merge-group jobs consuming eligible runners away from the front.
    pub capacity_occupiers: Vec<CapacityOccupier>,
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
    /// Age of the latest release at observation time.
    pub age_secs: i64,
    /// True when unreleased commits exist and the release is older than policy.
    pub stale_with_unreleased_commits: bool,
}

/// Classify release freshness without making any release mutation.
pub fn assess_release_liveness(
    tag: String,
    published_at: String,
    commits_ahead: u64,
    stale_threshold_secs: i64,
    now: DateTime<Utc>,
) -> Result<ReleaseLivenessReport, String> {
    let published = DateTime::parse_from_rfc3339(&published_at)
        .map_err(|error| format!("invalid latest release published_at: {error}"))?;
    let age_secs = (now - published.with_timezone(&Utc)).num_seconds().max(0);
    Ok(ReleaseLivenessReport {
        tag,
        published_at,
        commits_ahead,
        age_secs,
        stale_with_unreleased_commits: commits_ahead > 0 && age_secs >= stale_threshold_secs.max(0),
    })
}

impl MergeQueueLivenessReport {
    /// Whether the observation should make a fleet monitor exit non-zero.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        self.front_stalled_with_idle_capacity
            || self.front_blocked_by_capacity_occupiers
            || self
                .capacity_occupiers
                .iter()
                .any(|occupier| occupier.kind == OccupierKind::Superseded)
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
}

/// Parse merge-queue entries from the dedicated GraphQL observation.
pub fn parse_merge_queue_entries(body: &Value) -> Result<Vec<MergeQueueEntry>, String> {
    if body
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        return Err("merge-queue GraphQL response contains errors".to_owned());
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
    let marker = rest.find("/pr-")?;
    rest[marker + 4..]
        .split('-')
        .next()
        .and_then(|value| value.parse().ok())
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
    } = inputs;
    let front = entries.iter().min_by_key(|entry| entry.position).cloned();
    let matching_checks = checks
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
    let stalled_required_contexts =
        stalled_contexts(checks, required_contexts, stall_threshold_secs, now);
    let failed_required_contexts = required_contexts
        .iter()
        .filter(|required| {
            checks.iter().any(|check| {
                required.eq_ignore_ascii_case(&check.name)
                    && check.conclusion.as_deref().is_some_and(|conclusion| {
                        matches!(
                            conclusion,
                            "failure"
                                | "cancelled"
                                | "timed_out"
                                | "action_required"
                                | "startup_failure"
                        )
                    })
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let front_old_enough = front.as_ref().is_some_and(|entry| {
        entry.enqueued_at.as_deref().is_some_and(|enqueued_at| {
            DateTime::parse_from_rfc3339(enqueued_at).is_ok_and(|enqueued| {
                (now - enqueued.with_timezone(&Utc)).num_seconds() >= stall_threshold_secs.max(0)
            })
        })
    });
    let queued_prs = entries
        .iter()
        .map(|entry| entry.pr)
        .collect::<BTreeSet<_>>();
    let front_pr = front.as_ref().map(|entry| entry.pr);
    let mut capacity_occupiers = Vec::new();
    for run in active_runs {
        let merge_pr = merge_group_pr(&run.head_branch);
        if merge_pr == front_pr {
            continue;
        }
        let kind = match merge_pr {
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
    let required_work_stalled = front_old_enough
        && (!stalled_required_contexts.is_empty() || !failed_required_contexts.is_empty());
    let front_stalled_with_idle_capacity = required_work_stalled && routable_free_slots > 0;
    let front_blocked_by_capacity_occupiers =
        required_work_stalled && !capacity_occupiers.is_empty();

    let mut reason_codes = Vec::new();
    if front.is_none() {
        reason_codes.push(LivenessReason::QueueEmpty);
    } else if !required_work_stalled {
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
    for occupier in &capacity_occupiers {
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
        reason_codes,
    }
}

fn stalled_contexts(
    checks: &[CheckObservation],
    required_contexts: &[String],
    stall_threshold_secs: i64,
    now: DateTime<Utc>,
) -> Vec<String> {
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

fn job_is_on_eligible_host(job: &JobObservation, classes: &[String]) -> bool {
    job.runner_name
        .iter()
        .chain(job.labels.iter())
        .any(|value| {
            let tokens = value
                .split(|character: char| !character.is_ascii_alphanumeric())
                .filter(|token| !token.is_empty());
            tokens.into_iter().any(|token| {
                classes
                    .iter()
                    .any(|class| token.eq_ignore_ascii_case(class))
            })
        })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn ts(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).single().expect("timestamp")
    }

    #[test]
    fn parses_and_orders_queue_entries() {
        let body = serde_json::json!({
            "data": {"repository": {"mergeQueue": {"entries": {"nodes": [
                {"position": 1, "enqueuedAt": "2026-07-26T00:00:00Z",
                 "headCommit": {"oid": "bbb"}, "pullRequest": {"number": 22}},
                {"position": 0, "enqueuedAt": "2026-07-25T23:00:00Z",
                 "headCommit": {"oid": "aaa"}, "pullRequest": {"number": 11}}
            ]}}}}
        });
        let parsed = parse_merge_queue_entries(&body).expect("parse");
        assert_eq!(parsed[0].pr, 11);
        assert_eq!(parsed[0].head_sha.as_deref(), Some("aaa"));
    }

    #[test]
    fn aged_front_without_started_checks_and_with_free_slots_alerts() {
        let entries = vec![MergeQueueEntry {
            pr: 11,
            position: 0,
            head_sha: Some("aaa".to_owned()),
            enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        }];
        let checks = vec![CheckObservation {
            name: "macOS".to_owned(),
            status: "queued".to_owned(),
            started_at: None,
            conclusion: None,
        }];
        let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
            entries: &entries,
            checks: &checks,
            active_runs: &[],
            required_contexts: &["macOS".to_owned()],
            eligible_host_classes: &["m1".to_owned(), "m3".to_owned(), "m5".to_owned()],
            routable_free_slots: 2,
            stall_threshold_secs: 60,
            now: ts(120),
        });
        assert!(report.front_stalled_with_idle_capacity);
        assert_eq!(report.materialized_required_checks, 1);
        assert_eq!(report.progressed_required_checks, 0);
    }

    #[test]
    fn progress_or_no_idle_capacity_suppresses_front_alert() {
        let entries = vec![MergeQueueEntry {
            pr: 11,
            position: 0,
            head_sha: Some("aaa".to_owned()),
            enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        }];
        let checks = vec![CheckObservation {
            name: "macOS".to_owned(),
            status: "in_progress".to_owned(),
            started_at: Some("1970-01-01T00:01:59Z".to_owned()),
            conclusion: None,
        }];
        let progressed = assess_merge_queue_liveness(MergeQueueLivenessInputs {
            entries: &entries,
            checks: &checks,
            active_runs: &[],
            required_contexts: &["macOS".to_owned()],
            eligible_host_classes: &["m5".to_owned()],
            routable_free_slots: 1,
            stall_threshold_secs: 60,
            now: ts(120),
        });
        assert!(!progressed.front_stalled_with_idle_capacity);
        assert_eq!(progressed.reason_codes, [LivenessReason::NormalSerialWait]);
        let no_capacity = assess_merge_queue_liveness(MergeQueueLivenessInputs {
            entries: &entries,
            checks: &[],
            active_runs: &[],
            required_contexts: &["macOS".to_owned()],
            eligible_host_classes: &["m5".to_owned()],
            routable_free_slots: 0,
            stall_threshold_secs: 60,
            now: ts(120),
        });
        assert!(!no_capacity.front_stalled_with_idle_capacity);
    }

    #[test]
    fn identifies_superseded_and_non_front_capacity_occupiers() {
        let entries = vec![
            MergeQueueEntry {
                pr: 11,
                position: 0,
                head_sha: Some("aaa".to_owned()),
                enqueued_at: None,
            },
            MergeQueueEntry {
                pr: 22,
                position: 1,
                head_sha: Some("bbb".to_owned()),
                enqueued_at: None,
            },
        ];
        let run = |id, pr, runner: &str| ActiveRunObservation {
            run_id: id,
            workflow: "Build / Test".to_owned(),
            head_branch: format!("gh-readonly-queue/main/pr-{pr}-deadbeef"),
            pull_requests: vec![pr],
            url: None,
            jobs: vec![JobObservation {
                name: "macOS".to_owned(),
                status: "in_progress".to_owned(),
                runner_name: Some(runner.to_owned()),
                labels: vec!["self-hosted".to_owned()],
            }],
        };
        let active_runs = [run(1, 22, "pulp-m3-01"), run(2, 33, "pulp-m5-01")];
        let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
            entries: &entries,
            checks: &[],
            active_runs: &active_runs,
            required_contexts: &[],
            eligible_host_classes: &["m1".to_owned(), "m3".to_owned(), "m5".to_owned()],
            routable_free_slots: 0,
            stall_threshold_secs: 60,
            now: ts(120),
        });
        assert_eq!(report.capacity_occupiers.len(), 2);
        assert_eq!(report.capacity_occupiers[0].kind, OccupierKind::NonFront);
        assert_eq!(report.capacity_occupiers[1].kind, OccupierKind::Superseded);
        assert!(report.needs_attention());
    }

    #[test]
    fn ignores_hosted_and_unrelated_active_jobs() {
        let entries = vec![MergeQueueEntry {
            pr: 11,
            position: 0,
            head_sha: None,
            enqueued_at: None,
        }];
        let run = ActiveRunObservation {
            run_id: 9,
            workflow: "Build".to_owned(),
            head_branch: "gh-readonly-queue/main/pr-99-deadbeef".to_owned(),
            pull_requests: vec![99],
            url: None,
            jobs: vec![JobObservation {
                name: "Linux".to_owned(),
                status: "in_progress".to_owned(),
                runner_name: Some("GitHub Actions 42".to_owned()),
                labels: vec!["ubuntu-latest".to_owned()],
            }],
        };
        let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
            entries: &entries,
            checks: &[],
            active_runs: &[run],
            required_contexts: &[],
            eligible_host_classes: &["m1".to_owned(), "m3".to_owned(), "m5".to_owned()],
            routable_free_slots: 1,
            stall_threshold_secs: 60,
            now: ts(120),
        });
        assert!(report.capacity_occupiers.is_empty());
    }

    #[test]
    fn stale_required_check_and_optional_work_expose_useful_progress_wedge() {
        let entries = vec![MergeQueueEntry {
            pr: 11,
            position: 0,
            head_sha: Some("aaa".to_owned()),
            enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        }];
        let checks = vec![CheckObservation {
            name: "macos".to_owned(),
            status: "in_progress".to_owned(),
            started_at: Some("1970-01-01T00:00:10Z".to_owned()),
            conclusion: None,
        }];
        let optional = ActiveRunObservation {
            run_id: 77,
            workflow: "Examples".to_owned(),
            head_branch: "feature/example".to_owned(),
            pull_requests: vec![99],
            url: None,
            jobs: vec![JobObservation {
                name: "Validate examples (macOS)".to_owned(),
                status: "in_progress".to_owned(),
                runner_name: Some("pulp-vm-m1-01".to_owned()),
                labels: vec!["self-hosted".to_owned()],
            }],
        };
        let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
            entries: &entries,
            checks: &checks,
            active_runs: &[optional],
            required_contexts: &["macos".to_owned()],
            eligible_host_classes: &["m1".to_owned()],
            routable_free_slots: 0,
            stall_threshold_secs: 60,
            now: ts(120),
        });
        assert_eq!(report.stalled_required_contexts, ["macos"]);
        assert!(report.front_blocked_by_capacity_occupiers);
        assert_eq!(
            report.capacity_occupiers[0].kind,
            OccupierKind::OptionalNonQueue
        );
        assert!(
            report
                .reason_codes
                .contains(&LivenessReason::OptionalCapacityTheft)
        );
    }

    #[test]
    fn release_staleness_requires_age_and_unreleased_commits() {
        let stale = assess_release_liveness(
            "v1.0.0".to_owned(),
            "1970-01-01T00:00:00Z".to_owned(),
            3,
            60,
            ts(120),
        )
        .expect("release");
        assert!(stale.stale_with_unreleased_commits);
        let current = assess_release_liveness(
            "v1.0.0".to_owned(),
            "1970-01-01T00:00:00Z".to_owned(),
            0,
            60,
            ts(120),
        )
        .expect("release");
        assert!(!current.stale_with_unreleased_commits);
    }

    #[test]
    fn required_failure_is_distinct_from_advisory_red() {
        let entries = vec![MergeQueueEntry {
            pr: 11,
            position: 0,
            head_sha: Some("aaa".to_owned()),
            enqueued_at: Some("1970-01-01T00:00:00Z".to_owned()),
        }];
        let checks = vec![
            CheckObservation {
                name: "macos".to_owned(),
                status: "completed".to_owned(),
                started_at: Some("1970-01-01T00:00:10Z".to_owned()),
                conclusion: Some("failure".to_owned()),
            },
            CheckObservation {
                name: "advisory lint".to_owned(),
                status: "completed".to_owned(),
                started_at: Some("1970-01-01T00:00:10Z".to_owned()),
                conclusion: Some("failure".to_owned()),
            },
        ];
        let report = assess_merge_queue_liveness(MergeQueueLivenessInputs {
            entries: &entries,
            checks: &checks,
            active_runs: &[],
            required_contexts: &["macos".to_owned()],
            eligible_host_classes: &["m5".to_owned()],
            routable_free_slots: 1,
            stall_threshold_secs: 60,
            now: ts(120),
        });
        assert_eq!(report.failed_required_contexts, ["macos"]);
        assert!(
            report
                .reason_codes
                .contains(&LivenessReason::FrontRequiredFailed)
        );
    }
}
