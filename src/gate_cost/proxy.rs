//! Count-based, load-independent proxies for the gate-cost report.
//!
//! Gate minutes mostly measure how busy the shared hosts were. These proxies
//! count what the queue and the runners did instead, so two windows taken
//! under different load stay comparable. Each block carries its sample and
//! minimum sample; below the minimum, `sufficient` is false and a reader must
//! not draw a verdict from it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{GateCostObservation, GateJobSample, MERGE_GROUP_EVENT};
use crate::metrics::proxy::{ProxySample, wait_per_job_ahead_ms};

/// Sample size behind one proxy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Evidence {
    /// Samples behind the proxy.
    pub sample: usize,
    /// Minimum sample before the proxy supports a verdict.
    pub min_sample: usize,
    /// `sample >= min_sample`.
    pub sufficient: bool,
    /// Detection floor, in words.
    pub floor: &'static str,
}

fn evidence(sample: usize, min_sample: usize, floor: &'static str) -> Evidence {
    Evidence {
        sample,
        min_sample,
        sufficient: sample >= min_sample,
        floor,
    }
}

/// Metadata of one gate-workflow run from the run listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunMeta {
    /// Workflow-run event.
    pub event: String,
    /// Head branch.
    pub head_branch: Option<String>,
    /// Head commit.
    pub head_sha: Option<String>,
    /// Run creation time.
    pub created_at: Option<DateTime<Utc>>,
}

/// One job of any name in a gate-workflow run, for placement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacementSample {
    /// Job name (the job class).
    pub name: String,
    /// Labels the job requested.
    pub labels: Vec<String>,
    /// Whether a runner was ever assigned.
    pub runner_assigned: bool,
    /// `conclusion`, when completed.
    pub conclusion: Option<String>,
}

/// Label sets advertised by registered runners at observation time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunnerCensus {
    /// One entry per registered runner, lower-cased.
    pub label_sets: Vec<BTreeSet<String>>,
    /// Scopes that could not be read (for example the organisation).
    pub unread_scopes: Vec<String>,
}

/// Gate runs per merged PR, split by class.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunsPerMergedPr {
    /// Pull requests merged in the window.
    pub merged_prs: Option<u64>,
    /// PR-head gate runs.
    pub pr_head_runs: usize,
    /// Merge-group gate runs.
    pub merge_group_runs: usize,
    /// Gate attempts that ran and did not succeed.
    pub wasted_attempts: usize,
    /// `pr_head_runs / merged_prs`.
    pub pr_head_per_merged_pr: Option<f64>,
    /// `merge_group_runs / merged_prs`.
    pub merge_group_per_merged_pr: Option<f64>,
    /// `wasted_attempts / merged_prs`.
    pub wasted_per_merged_pr: Option<f64>,
    /// Sample: merged PRs.
    pub evidence: Evidence,
}

/// Required-gate jobs cancelled before any runner was assigned.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Starvation {
    /// Completed, non-skipped gate attempts.
    pub gate_attempts: usize,
    /// Of those, cancelled with no runner ever assigned.
    pub cancelled_before_runner: usize,
    /// Of `cancelled_before_runner`, jobs whose run was superseded by a push
    /// to the same PR: withdrawn by the author, not starved by capacity.
    pub superseded_by_push: usize,
    /// `cancelled_before_runner / gate_attempts`.
    pub share: Option<f64>,
    /// Sample: gate attempts.
    pub evidence: Evidence,
}

/// Placement of one job class.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct JobClassPlacement {
    /// Non-skipped jobs.
    pub jobs: usize,
    /// Jobs a runner picked up.
    pub assigned: usize,
    /// Jobs never assigned whose exact label set nothing serves.
    pub unserved: usize,
    /// Jobs never assigned whose label set is served (capacity, not
    /// placement).
    pub waiting_or_starved: usize,
    /// `assigned / (assigned + unserved)`.
    pub placement_correct_share: Option<f64>,
}

/// Jobs requesting label sets no runner serves.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Placement {
    /// `available`, `partial` or `unavailable`.
    pub census: String,
    /// Why the census is partial or unavailable.
    pub census_reason: Option<String>,
    /// Non-skipped jobs of every name in the gate-workflow runs read.
    pub jobs: usize,
    /// Jobs whose exact label set no runner serves.
    pub unserved_label_jobs: usize,
    /// Those label sets.
    pub unserved_label_sets: Vec<Vec<String>>,
    /// `(assigned) / (assigned + unserved)` over all jobs.
    pub placement_correct_share: Option<f64>,
    /// Per job class.
    pub by_job_class: BTreeMap<String, JobClassPlacement>,
    /// Sample: jobs.
    pub evidence: Evidence,
}

/// Queue wait normalised by queue depth at job creation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QueueWaitPerJobAhead {
    /// Median of `wait / (1 + gate jobs ahead)`, seconds.
    pub median_seconds: Option<f64>,
    /// Median raw wait, seconds. Load-dependent context.
    pub raw_median_wait_seconds: Option<f64>,
    /// Sample: gate jobs a runner picked up.
    pub evidence: Evidence,
}

/// Merge-queue attempts and ejections.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MergeQueue {
    /// Merge-group gate attempts that ran.
    pub attempts: usize,
    /// `attempts / merged_prs`.
    pub attempts_per_merged_pr: Option<f64>,
    /// Merge-group runs whose final gate attempt did not succeed.
    pub ejections: usize,
    /// Ejections by cause: `gate_failed`, `starved`,
    /// `cancelled_after_start`, or the raw conclusion.
    pub ejections_by_cause: BTreeMap<String, usize>,
    /// Sample: merged PRs.
    pub evidence: Evidence,
}

/// PR-head gate runs cancelled because the PR was pushed again.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PushCancellations {
    /// PR-head runs whose gate was cancelled.
    pub cancelled_pr_head_runs: usize,
    /// Of those, superseded by a newer run on the same branch at another
    /// commit, created before the cancellation completed.
    pub superseded_by_push: usize,
    /// `superseded_by_push / pr_head_runs`.
    pub share_of_pr_head_runs: Option<f64>,
    /// Push type (force vs fast-forward) is not read: it needs the PR
    /// timeline per run, which is not cheap.
    pub by_push_type: Option<BTreeMap<String, usize>>,
    /// Sample: PR-head runs.
    pub evidence: Evidence,
}

/// Every count-based proxy in the gate-cost report.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GateProxies {
    /// Always `count-based, load-independent`.
    pub basis: &'static str,
    /// Gate runs per merged PR.
    pub runs_per_merged_pr: RunsPerMergedPr,
    /// Starvation.
    pub starvation: Starvation,
    /// Placement.
    pub placement: Placement,
    /// Queue wait per job ahead.
    pub queue_wait_per_job_ahead: QueueWaitPerJobAhead,
    /// Merge queue.
    pub merge_queue: MergeQueue,
    /// Push cancellations.
    pub push_cancellations: PushCancellations,
}

#[allow(clippy::cast_precision_loss)]
fn per(numerator: usize, denominator: Option<u64>) -> Option<f64> {
    denominator
        .filter(|count| *count > 0)
        .map(|count| (numerator as f64 / count as f64 * 100.0).round() / 100.0)
}

#[allow(clippy::cast_precision_loss)]
fn share(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator > 0).then(|| (numerator as f64 / denominator as f64 * 1000.0).round() / 1000.0)
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    let value = if values.len().is_multiple_of(2) {
        f64::midpoint(values[mid - 1], values[mid])
    } else {
        values[mid]
    };
    Some((value * 10.0).round() / 10.0)
}

fn assigned(job: &GateJobSample) -> bool {
    job.runner_name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
}

fn ran(job: &GateJobSample) -> bool {
    job.status == "completed" && job.conclusion.as_deref() != Some("skipped")
}

fn starved(job: &GateJobSample) -> bool {
    job.conclusion.as_deref() == Some("cancelled") && !assigned(job)
}

/// Why a required job that did not pass ejected its batch: `gate_failed`
/// (failure or timeout), `starved` (cancelled before any runner took it),
/// `cancelled_after_start`, the raw conclusion otherwise, or `None` for a
/// passing job. Reads only GitHub's job facts.
pub(crate) fn ejection_cause(job: &GateJobSample) -> Option<String> {
    match job.conclusion.as_deref() {
        Some("success" | "skipped" | "neutral") => None,
        Some("failure" | "timed_out") => Some("gate_failed".to_owned()),
        Some("cancelled") if !assigned(job) => Some("starved".to_owned()),
        Some("cancelled") => Some("cancelled_after_start".to_owned()),
        Some(other) => Some(other.to_owned()),
        None => Some("no_conclusion".to_owned()),
    }
}

fn normalize(labels: &[String]) -> BTreeSet<String> {
    labels.iter().map(|label| label.to_lowercase()).collect()
}

fn placement(observation: &GateCostObservation) -> Placement {
    let (census, census_reason, advertised) = match &observation.runner_census {
        Ok(census) if census.unread_scopes.is_empty() => {
            ("available", None, census.label_sets.clone())
        }
        Ok(census) => (
            "partial",
            Some(format!(
                "could not read: {}",
                census.unread_scopes.join(", ")
            )),
            census.label_sets.clone(),
        ),
        Err(error) => ("unavailable", Some(error.clone()), Vec::new()),
    };
    let considered: Vec<&PlacementSample> = observation
        .placement_jobs
        .iter()
        .filter(|job| job.conclusion.as_deref() != Some("skipped"))
        .collect();
    let served_by_history: BTreeSet<BTreeSet<String>> = considered
        .iter()
        .filter(|job| job.runner_assigned)
        .map(|job| normalize(&job.labels))
        .collect();
    let served = |labels: &BTreeSet<String>| {
        served_by_history.contains(labels)
            || advertised.iter().any(|runner| labels.is_subset(runner))
    };
    let mut by_job_class: BTreeMap<String, JobClassPlacement> = BTreeMap::new();
    let mut unserved_sets: BTreeSet<Vec<String>> = BTreeSet::new();
    let (mut assigned_total, mut unserved_total) = (0, 0);
    for job in &considered {
        let class = by_job_class
            .entry(job.name.clone())
            .or_insert(JobClassPlacement {
                jobs: 0,
                assigned: 0,
                unserved: 0,
                waiting_or_starved: 0,
                placement_correct_share: None,
            });
        class.jobs += 1;
        let labels = normalize(&job.labels);
        if job.runner_assigned {
            class.assigned += 1;
            assigned_total += 1;
        } else if served(&labels) {
            class.waiting_or_starved += 1;
        } else {
            class.unserved += 1;
            unserved_total += 1;
            unserved_sets.insert(labels.into_iter().collect());
        }
    }
    for class in by_job_class.values_mut() {
        class.placement_correct_share = share(class.assigned, class.assigned + class.unserved);
    }
    Placement {
        census: census.to_owned(),
        census_reason,
        jobs: considered.len(),
        unserved_label_jobs: unserved_total,
        unserved_label_sets: unserved_sets.into_iter().collect(),
        placement_correct_share: share(assigned_total, assigned_total + unserved_total),
        by_job_class,
        evidence: evidence(
            considered.len(),
            10,
            "n>=10 jobs; a label set counts as served when a registered runner advertises \
             it or any job with that exact set got a runner in the window, so an \
             ephemeral pool that minted no runner all window reads as unserved",
        ),
    }
}

/// PR-head runs whose gate was cancelled after a newer commit on the same
/// branch started a run.
fn superseded_runs(observation: &GateCostObservation) -> (usize, BTreeSet<u64>) {
    let pr_runs: Vec<(&u64, &RunMeta)> = observation
        .run_meta
        .iter()
        .filter(|(_, meta)| meta.event != MERGE_GROUP_EVENT)
        .collect();
    let mut cancelled = 0;
    let mut superseded = BTreeSet::new();
    for (run_id, meta) in &pr_runs {
        let Some(cancel_time) = observation
            .gate_jobs
            .iter()
            .filter(|job| job.run_id == **run_id && job.conclusion.as_deref() == Some("cancelled"))
            .filter_map(|job| job.completed_at)
            .max()
        else {
            continue;
        };
        cancelled += 1;
        let (Some(branch), Some(created)) = (&meta.head_branch, meta.created_at) else {
            continue;
        };
        let newer_push = pr_runs.iter().any(|(other_id, other)| {
            other_id != run_id
                && other.head_branch.as_ref() == Some(branch)
                && other.head_sha != meta.head_sha
                && other
                    .created_at
                    .is_some_and(|time| time > created && time <= cancel_time)
        });
        if newer_push {
            superseded.insert(**run_id);
        }
    }
    (cancelled, superseded)
}

fn push_cancellations(observation: &GateCostObservation) -> PushCancellations {
    let pr_runs = observation
        .run_meta
        .values()
        .filter(|meta| meta.event != MERGE_GROUP_EVENT)
        .count();
    let (cancelled, superseded) = superseded_runs(observation);
    PushCancellations {
        cancelled_pr_head_runs: cancelled,
        superseded_by_push: superseded.len(),
        share_of_pr_head_runs: share(superseded.len(), pr_runs),
        by_push_type: None,
        evidence: evidence(
            pr_runs,
            10,
            "n>=10 PR-head runs; a cancellation counts only when a newer run on the same \
             branch at another commit was created before it completed",
        ),
    }
}

fn queue_wait(observation: &GateCostObservation) -> QueueWaitPerJobAhead {
    let samples: Vec<ProxySample> = observation
        .gate_jobs
        .iter()
        .filter(|job| job.status == "completed" && job.conclusion.as_deref() != Some("skipped"))
        .map(|job| ProxySample {
            lane: String::new(),
            status: job.conclusion.clone().unwrap_or_default(),
            queued_at: job.created_at,
            started_at: assigned(job).then_some(job.started_at).flatten(),
            completed_at: job.completed_at,
            runner_assigned: Some(assigned(job)),
            ..ProxySample::default()
        })
        .collect();
    let refs: Vec<&ProxySample> = samples.iter().collect();
    let mut per_ahead: Vec<f64> = wait_per_job_ahead_ms(&refs)
        .into_iter()
        .map(|ms| ms / 1000.0)
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let mut raw: Vec<f64> = samples
        .iter()
        .filter(|sample| sample.runner_assigned == Some(true))
        .filter_map(|sample| Some((sample.started_at? - sample.queued_at?).num_milliseconds()))
        .filter(|ms| *ms >= 0)
        .map(|ms| ms as f64 / 1000.0)
        .collect();
    QueueWaitPerJobAhead {
        evidence: evidence(
            per_ahead.len(),
            10,
            "n>=10 gate jobs with created_at and a runner; GitHub timestamps are whole \
             seconds, so movements under ~1s are noise",
        ),
        median_seconds: median(&mut per_ahead),
        raw_median_wait_seconds: median(&mut raw),
    }
}

/// Compute every proxy from an observation. Pure.
#[must_use]
pub fn compute(observation: &GateCostObservation, merged_prs: Option<u64>) -> GateProxies {
    let is_mg = |job: &&GateJobSample| job.event == MERGE_GROUP_EVENT;
    let ran_jobs: Vec<&GateJobSample> = observation
        .gate_jobs
        .iter()
        .filter(|job| ran(job))
        .collect();
    let wasted = ran_jobs
        .iter()
        .filter(|job| job.conclusion.as_deref() != Some("success"))
        .count();
    let mg_runs = observation
        .runs_by_event
        .get(MERGE_GROUP_EVENT)
        .map_or(0, BTreeSet::len);
    let pr_runs: usize = observation
        .runs_by_event
        .iter()
        .filter(|(event, _)| *event != MERGE_GROUP_EVENT)
        .map(|(_, runs)| runs.len())
        .sum();
    let merged_sample = usize::try_from(merged_prs.unwrap_or(0)).unwrap_or(usize::MAX);

    let mut final_attempt: BTreeMap<u64, &GateJobSample> = BTreeMap::new();
    for job in observation
        .gate_jobs
        .iter()
        .filter(|job| is_mg(job) && job.status == "completed")
    {
        final_attempt
            .entry(job.run_id)
            .and_modify(|current| {
                if job.attempt > current.attempt {
                    *current = job;
                }
            })
            .or_insert(job);
    }
    let mut ejections_by_cause: BTreeMap<String, usize> = BTreeMap::new();
    for job in final_attempt.values() {
        if let Some(cause) = ejection_cause(job) {
            *ejections_by_cause.entry(cause).or_insert(0) += 1;
        }
    }
    let mg_attempts = ran_jobs.iter().filter(|job| is_mg(job)).count();
    let starved_count = ran_jobs.iter().filter(|job| starved(job)).count();
    let (_, superseded) = superseded_runs(observation);
    let starved_by_push = ran_jobs
        .iter()
        .filter(|job| starved(job) && superseded.contains(&job.run_id))
        .count();

    GateProxies {
        basis: "count-based, load-independent",
        runs_per_merged_pr: RunsPerMergedPr {
            merged_prs,
            pr_head_runs: pr_runs,
            merge_group_runs: mg_runs,
            wasted_attempts: wasted,
            pr_head_per_merged_pr: per(pr_runs, merged_prs),
            merge_group_per_merged_pr: per(mg_runs, merged_prs),
            wasted_per_merged_pr: per(wasted, merged_prs),
            evidence: evidence(
                merged_sample,
                5,
                "n>=5 merged PRs; ratios are rounded to 0.01",
            ),
        },
        starvation: Starvation {
            gate_attempts: ran_jobs.len(),
            cancelled_before_runner: starved_count,
            superseded_by_push: starved_by_push,
            share: share(starved_count, ran_jobs.len()),
            evidence: evidence(
                ran_jobs.len(),
                10,
                "n>=10 gate attempts; a job is starved when cancelled with no runner_name, \
                 which GitHub leaves empty only for jobs no runner picked up",
            ),
        },
        placement: placement(observation),
        queue_wait_per_job_ahead: queue_wait(observation),
        merge_queue: MergeQueue {
            attempts: mg_attempts,
            attempts_per_merged_pr: per(mg_attempts, merged_prs),
            ejections: ejections_by_cause.values().sum(),
            ejections_by_cause,
            evidence: evidence(
                merged_sample,
                5,
                "n>=5 merged PRs; the cause is the final gate attempt's conclusion",
            ),
        },
        push_cancellations: push_cancellations(observation),
    }
}
