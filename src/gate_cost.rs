//! Gate cost: how many required-gate minutes a repository spends per merged
//! pull request, how full its merge-queue batches are, and how often a merge
//! group reused an earlier receipt instead of re-running the gate.
//!
//! Everything is read from GitHub and nothing is written. The module is split
//! in two so the arithmetic can be proven on a fixture:
//!
//! - [`gather`] reads a window of workflow runs, jobs, merge-queue pushes,
//!   receipt-decision annotations and the merged-PR count through a
//!   [`GhReader`], paginating every list completely and refusing to report a
//!   list whose collected length disagrees with the API's `total_count`;
//! - [`compute`] turns that [`GateCostObservation`] into a [`GateCostReport`].
//!
//! Definitions (the same text is in `shipyard metrics gate-cost --help`):
//!
//! - **gate minutes**: wall minutes (`completed_at - started_at`) of every job
//!   named exactly `gate_job` in runs of `workflow` whose event is one of the
//!   configured events and whose run was created in the window. Every attempt
//!   counts, and so do failed and cancelled jobs: waste is a cost.
//! - **gate minutes per merged PR**: total gate minutes divided by the number
//!   of pull requests merged into `base_branch` in the window.
//! - **batch fullness**: pull requests per merge-queue push to `base_branch`
//!   (one `merge_queue_merge` repository activity), counted by walking the
//!   pushed head's first-parent chain back to the pre-push commit, compared
//!   with the ruleset's `max_entries_to_merge`.
//! - **receipt reuse rate**: merge-group runs of `workflow` in which a
//!   `shipyard-receipt-decision/v1` annotation reported verdict `reuse` for the
//!   receipt target, divided by all merge-group runs of `workflow`.
//!
//! Gate minutes mostly track host load, so the report leads with the
//! count-based proxies in [`proxy`]: gate runs per merged PR, starvation,
//! placement against runner labels, queue wait per job ahead, merge-queue
//! attempts and ejections, and push cancellations. Minutes stay in the report
//! as load-dependent context.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::metrics::job_name;
use crate::validation_signals::{self, GhReader, ReceiptDecision};

pub mod cache;
pub mod proxy;
pub use cache::{ReadCache, ReadStats};
pub use proxy::{GateProxies, PlacementSample, RunMeta, RunnerCensus};

/// A [`GhReader`] that may be shared across the bounded read workers.
pub type SyncGhReader<'a> = dyn Fn(&[String]) -> Result<String, String> + Sync + 'a;

/// Report schema identifier.
pub const SCHEMA: &str = "shipyard-gate-cost/v1";
/// Event name GitHub gives merge-queue workflow runs.
pub const MERGE_GROUP_EVENT: &str = "merge_group";
/// Repository activity type for one merge-queue push.
pub const MERGE_QUEUE_ACTIVITY: &str = "merge_queue_merge";
/// GitHub caps filtered workflow-run listings at this many results.
const RUN_LISTING_CAP: u64 = 1000;
/// Longest first-parent walk attempted for one merge-queue push.
const MAX_BATCH_WALK: u32 = 50;
/// Concurrent GitHub reads. Small on purpose: GitHub's secondary rate limit
/// penalises bursts, and the gate host this runs on is shared.
const READ_WORKERS: usize = 4;

/// Map `items` through `work` on at most [`READ_WORKERS`] threads, keeping
/// input order.
pub(crate) fn parallel_map<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut slots: Vec<Option<R>> = std::iter::repeat_with(|| None).take(items.len()).collect();
    let results = std::sync::Mutex::new(&mut slots);
    std::thread::scope(|scope| {
        for _ in 0..READ_WORKERS.min(items.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let result = work(item);
                    results.lock().expect("result slots")[index] = Some(result);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| slot.expect("every index is claimed exactly once"))
        .collect()
}

/// What to measure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateCostQuery {
    /// `OWNER/REPO`.
    pub repo: String,
    /// Workflow file name or id that hosts the gate job.
    pub workflow: String,
    /// Exact job name of the required gate.
    pub gate_job: String,
    /// Protected branch the queue merges into.
    pub base_branch: String,
    /// Workflow-run events that count as gate runs.
    pub events: Vec<String>,
    /// Only read receipt annotations from jobs with this exact name. `None`
    /// reads every job of the merge-group run that actually ran.
    pub receipt_job: Option<String>,
    /// `target` field a reuse decision must carry to count.
    pub receipt_target: String,
    /// Window start, inclusive.
    pub from: DateTime<Utc>,
    /// Window end, exclusive.
    pub to: DateTime<Utc>,
}

/// One gate job attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateJobSample {
    /// Workflow run id.
    pub run_id: u64,
    /// Workflow-run event.
    pub event: String,
    /// Run attempt the job belongs to.
    pub attempt: u64,
    /// `status` (`completed`, `in_progress`, ...).
    pub status: String,
    /// `conclusion`, when completed.
    pub conclusion: Option<String>,
    /// Runner start time.
    pub started_at: Option<DateTime<Utc>>,
    /// Completion time.
    pub completed_at: Option<DateTime<Utc>>,
    /// Job creation (queue) time.
    pub created_at: Option<DateTime<Utc>>,
    /// Runner that picked the job up; `None` when no runner ever did.
    pub runner_name: Option<String>,
    /// Labels the job requested.
    pub labels: Vec<String>,
}

impl GateJobSample {
    /// One job from the Actions jobs API (`actions/jobs/{id}` or a run's
    /// `jobs[]` entry). A blank `runner_name` reads as no runner.
    #[must_use]
    pub fn from_job(run_id: u64, event: &str, job: &Value) -> Self {
        Self {
            run_id,
            event: event.to_owned(),
            attempt: job.get("run_attempt").and_then(Value::as_u64).unwrap_or(1),
            status: text(job, "status").unwrap_or_default(),
            conclusion: text(job, "conclusion"),
            started_at: timestamp(job, "started_at"),
            completed_at: timestamp(job, "completed_at"),
            created_at: timestamp(job, "created_at"),
            runner_name: text(job, "runner_name").filter(|name| !name.trim().is_empty()),
            labels: job_labels(job),
        }
    }
}

/// One merge-queue push to the base branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchSample {
    /// Branch head after the push.
    pub after: String,
    /// When the push happened.
    pub timestamp: DateTime<Utc>,
    /// First-parent commits the push added, or `None` when the walk did not
    /// reach the pre-push commit.
    pub entries: Option<u32>,
}

/// What one merge-group run said about receipt reuse for the target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReuseOutcome {
    /// A decision with verdict `reuse` for the target.
    Reused,
    /// Only decisions with another verdict for the target.
    Refused,
    /// No decision for the target was published.
    NoDecision,
    /// Annotations could not be read.
    Unreadable,
}

/// Everything [`compute`] needs.
#[derive(Clone, Debug, PartialEq)]
pub struct GateCostObservation {
    /// The query this observation answers.
    pub query: GateCostQuery,
    /// Distinct gate-workflow run ids per event, including runs whose gate
    /// job never appeared.
    pub runs_by_event: BTreeMap<String, BTreeSet<u64>>,
    /// Every gate job attempt found.
    pub gate_jobs: Vec<GateJobSample>,
    /// Pull requests merged into the base branch in the window.
    pub merged_prs: Result<u64, String>,
    /// Merge-queue pushes in the window.
    pub batches: Result<Vec<BatchSample>, String>,
    /// Ruleset `max_entries_to_merge`.
    pub max_entries_to_merge: Option<u64>,
    /// Ruleset `max_entries_to_build`.
    pub max_entries_to_build: Option<u64>,
    /// Ruleset merge method.
    pub merge_method: Option<String>,
    /// Receipt outcome per merge-group run id.
    pub reuse: BTreeMap<u64, ReuseOutcome>,
    /// Live merge-queue depth at observation time.
    pub current_queue_depth: Result<u64, String>,
    /// Problems reading the ruleset, kept for the gap list.
    pub ruleset_error: Option<String>,
    /// Run metadata per gate-workflow run id.
    pub run_meta: BTreeMap<u64, RunMeta>,
    /// Every job of every name in the gate-workflow runs read.
    pub placement_jobs: Vec<PlacementSample>,
    /// Registered runners' label sets.
    pub runner_census: Result<RunnerCensus, String>,
    /// How many reads went to GitHub and how many the disk cache answered.
    pub reads: ReadStats,
    /// Problems found while listing gate runs: a created-window listing that
    /// an independent event walk contradicted, or a walk that could not run.
    pub listing_gaps: Vec<TelemetryGap>,
    /// Per event, runs in which no job was named exactly `gate_job`.
    pub gate_absent: BTreeMap<String, GateAbsence>,
}

/// Runs of one event in which no job carried the gate's exact name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GateAbsence {
    /// Runs with no exactly-named gate job.
    pub runs: usize,
    /// Of those, runs whose gate was matched through a job whose reported
    /// name is an unevaluated expression or matrix placeholder resolving to
    /// the gate name, and that job never got a runner: GitHub leaves `name:`
    /// unevaluated for a job cancelled or skipped before it started.
    pub resolved_unstarted: usize,
    /// Of those, runs whose matched job ran under its unevaluated name: the
    /// workflow's gate `name:` itself no longer evaluates to the gate.
    pub resolved_ran: usize,
    /// Up to three distinct reported names that resolved to the gate.
    pub resolved_names: Vec<String>,
}

/// Duration statistics for one class of gate runs.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LaneStats {
    /// Distinct workflow runs.
    pub runs: usize,
    /// Gate job attempts, including skipped ones.
    pub jobs: usize,
    /// Completed gate job attempts that were not skipped.
    pub jobs_ran: usize,
    /// Gate jobs still running when observed (excluded from minutes).
    pub jobs_in_progress: usize,
    /// Sum of wall minutes over completed attempts.
    pub gate_minutes: f64,
    /// Wall minutes of completed attempts whose conclusion was not success.
    pub wasted_minutes: f64,
    /// Median wall minutes over attempts that ran.
    pub median_minutes: Option<f64>,
    /// 25th percentile.
    pub p25_minutes: Option<f64>,
    /// 75th percentile.
    pub p75_minutes: Option<f64>,
    /// Attempt count per conclusion.
    pub by_conclusion: BTreeMap<String, usize>,
}

/// Merge-queue batch fullness.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BatchStats {
    /// Merge-queue pushes in the window.
    pub batches: usize,
    /// Pushes whose size could not be resolved.
    pub unresolved: usize,
    /// Pull requests landed through resolved pushes.
    pub prs_in_batches: u64,
    /// Mean pull requests per resolved push.
    pub mean_prs_per_batch: Option<f64>,
    /// Ruleset `max_entries_to_merge`.
    pub max_entries_to_merge: Option<u64>,
    /// Ruleset `max_entries_to_build`.
    pub max_entries_to_build: Option<u64>,
    /// Mean fraction of `max_entries_to_merge` used.
    pub mean_fullness: Option<f64>,
    /// Resolved pushes that were at capacity.
    pub at_capacity: usize,
    /// Pushes by size.
    pub distribution: BTreeMap<u32, usize>,
}

/// Receipt reuse.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReuseStats {
    /// Target a reuse decision must name.
    pub target: String,
    /// All merge-group runs of the workflow (the denominator).
    pub merge_group_runs: usize,
    /// Runs that reused a receipt.
    pub reused: usize,
    /// Runs that published a non-reuse decision.
    pub refused: usize,
    /// Runs that published no decision for the target.
    pub no_decision: usize,
    /// Runs whose annotations could not be read.
    pub unreadable: usize,
    /// `reused / merge_group_runs`.
    pub rate: Option<f64>,
}

/// One signal the report could not measure, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TelemetryGap {
    /// Signal name.
    pub signal: String,
    /// Why it is missing or partial.
    pub reason: String,
}

/// The gate-cost report.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GateCostReport {
    /// [`SCHEMA`].
    pub schema: &'static str,
    /// `OWNER/REPO`.
    pub repo: String,
    /// Gate workflow.
    pub workflow: String,
    /// Gate job name.
    pub gate_job: String,
    /// Base branch.
    pub base_branch: String,
    /// Window start (RFC 3339).
    pub from: String,
    /// Window end (RFC 3339).
    pub to: String,
    /// Stats for gate runs not triggered by the merge queue (PR heads).
    pub pr_head: LaneStats,
    /// Stats for merge-group gate runs.
    pub merge_group: LaneStats,
    /// Total gate minutes, both classes.
    pub gate_minutes: f64,
    /// Total wasted gate minutes, both classes.
    pub wasted_gate_minutes: f64,
    /// Pull requests merged into the base branch in the window.
    pub merged_prs: Option<u64>,
    /// Headline: `gate_minutes / merged_prs`.
    pub gate_minutes_per_merged_pr: Option<f64>,
    /// PR-head gate runs per merged PR (a pushes-per-PR proxy).
    pub pr_head_runs_per_merged_pr: Option<f64>,
    /// Merge-group gate runs per merged PR. GitHub builds one merge group per
    /// queue entry, so a full batch does not by itself reduce this below 1.
    pub merge_group_runs_per_merged_pr: Option<f64>,
    /// Batch fullness.
    pub batches: BatchStats,
    /// Receipt reuse.
    pub reuse: ReuseStats,
    /// Live queue depth at observation time. Not historical.
    pub current_queue_depth: Option<u64>,
    /// Signals not measured, or measured only partly.
    pub telemetry_gaps: Vec<TelemetryGap>,
    /// Count-based, load-independent proxies. Lead with these; the minutes
    /// above are load-dependent context.
    pub proxies: GateProxies,
    /// Requests this report cost: sent to GitHub, and served from the cache.
    pub reads: ReadStats,
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[allow(clippy::cast_precision_loss)]
fn minutes(sample: &GateJobSample) -> Option<f64> {
    let (start, end) = (sample.started_at?, sample.completed_at?);
    let millis = (end - start).num_milliseconds().max(0);
    Some(millis as f64 / 60_000.0)
}

/// Linear-interpolated percentile of an ascending slice.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let position = p * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let weight = position - lower as f64;
    Some(sorted[lower] + (sorted[upper] - sorted[lower]) * weight)
}

fn lane_stats(runs: usize, jobs: &[&GateJobSample]) -> LaneStats {
    let mut ran = Vec::new();
    let mut gate_minutes = 0.0;
    let mut wasted_minutes = 0.0;
    let mut by_conclusion = BTreeMap::new();
    let mut in_progress = 0;
    for job in jobs {
        if job.status != "completed" {
            in_progress += 1;
            continue;
        }
        let conclusion = job.conclusion.clone().unwrap_or_else(|| "none".to_owned());
        *by_conclusion.entry(conclusion.clone()).or_insert(0) += 1;
        let wall = minutes(job).unwrap_or(0.0);
        gate_minutes += wall;
        if conclusion != "success" && conclusion != "skipped" {
            wasted_minutes += wall;
        }
        if conclusion != "skipped" {
            ran.push(wall);
        }
    }
    ran.sort_by(f64::total_cmp);
    LaneStats {
        runs,
        jobs: jobs.len(),
        jobs_ran: ran.len(),
        jobs_in_progress: in_progress,
        gate_minutes: round2(gate_minutes),
        wasted_minutes: round2(wasted_minutes),
        median_minutes: percentile(&ran, 0.5).map(round2),
        p25_minutes: percentile(&ran, 0.25).map(round2),
        p75_minutes: percentile(&ran, 0.75).map(round2),
        by_conclusion,
    }
}

#[allow(clippy::cast_precision_loss)]
fn ratio(numerator: f64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| round2(numerator / denominator as f64))
}

/// Turn an observation into the report. Pure: no I/O.
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
pub fn compute(observation: &GateCostObservation) -> GateCostReport {
    let query = &observation.query;
    let mut gaps = Vec::new();
    let is_merge_group = |event: &str| event == MERGE_GROUP_EVENT;

    let mg_jobs: Vec<&GateJobSample> = observation
        .gate_jobs
        .iter()
        .filter(|job| is_merge_group(&job.event))
        .collect();
    let pr_jobs: Vec<&GateJobSample> = observation
        .gate_jobs
        .iter()
        .filter(|job| !is_merge_group(&job.event))
        .collect();
    let mg_runs = observation
        .runs_by_event
        .get(MERGE_GROUP_EVENT)
        .map_or(0, BTreeSet::len);
    let pr_runs: usize = observation
        .runs_by_event
        .iter()
        .filter(|(event, _)| !is_merge_group(event))
        .map(|(_, runs)| runs.len())
        .sum();
    let pr_head = lane_stats(pr_runs, &pr_jobs);
    let merge_group = lane_stats(mg_runs, &mg_jobs);
    let gate_minutes = round2(pr_head.gate_minutes + merge_group.gate_minutes);
    let wasted_gate_minutes = round2(pr_head.wasted_minutes + merge_group.wasted_minutes);
    if pr_head.jobs_in_progress + merge_group.jobs_in_progress > 0 {
        gaps.push(TelemetryGap {
            signal: "gate_minutes".to_owned(),
            reason: format!(
                "{} gate job(s) still running; their minutes are excluded",
                pr_head.jobs_in_progress + merge_group.jobs_in_progress
            ),
        });
    }

    let merged_prs = match &observation.merged_prs {
        Ok(count) => Some(*count),
        Err(error) => {
            gaps.push(TelemetryGap {
                signal: "merged_prs".to_owned(),
                reason: error.clone(),
            });
            None
        }
    };
    let gate_minutes_per_merged_pr = merged_prs.and_then(|count| ratio(gate_minutes, count));
    let pr_head_runs_per_merged_pr = merged_prs.and_then(|count| ratio(pr_runs as f64, count));
    let merge_group_runs_per_merged_pr = merged_prs.and_then(|count| ratio(mg_runs as f64, count));

    let mut distribution = BTreeMap::new();
    let (mut batch_count, mut unresolved, mut prs_in_batches, mut at_capacity) = (0, 0, 0u64, 0);
    match &observation.batches {
        Ok(batches) => {
            batch_count = batches.len();
            for batch in batches {
                match batch.entries {
                    Some(entries) => {
                        *distribution.entry(entries).or_insert(0) += 1;
                        prs_in_batches += u64::from(entries);
                        if observation
                            .max_entries_to_merge
                            .is_some_and(|max| u64::from(entries) >= max)
                        {
                            at_capacity += 1;
                        }
                    }
                    None => unresolved += 1,
                }
            }
            if unresolved > 0 {
                gaps.push(TelemetryGap {
                    signal: "batch_fullness".to_owned(),
                    reason: format!(
                        "{unresolved} merge-queue push(es) could not be walked back to the \
                         pre-push commit and are excluded from the mean"
                    ),
                });
            }
        }
        Err(error) => gaps.push(TelemetryGap {
            signal: "batch_fullness".to_owned(),
            reason: error.clone(),
        }),
    }
    if observation
        .merge_method
        .as_deref()
        .is_some_and(|method| method.eq_ignore_ascii_case("rebase"))
    {
        gaps.push(TelemetryGap {
            signal: "batch_fullness".to_owned(),
            reason: "ruleset merge method is REBASE, so first-parent commits per push count \
                     commits, not pull requests"
                .to_owned(),
        });
    }
    if let Some(error) = &observation.ruleset_error {
        gaps.push(TelemetryGap {
            signal: "max_entries_to_merge".to_owned(),
            reason: error.clone(),
        });
    }
    let resolved = batch_count - unresolved;
    let mean_prs_per_batch = ratio(prs_in_batches as f64, resolved as u64);
    let mean_fullness = match (mean_prs_per_batch, observation.max_entries_to_merge) {
        (Some(_), Some(max)) if max > 0 => Some(round2(
            prs_in_batches as f64 / (resolved as f64 * max as f64),
        )),
        _ => None,
    };

    let mut reuse = ReuseStats {
        target: query.receipt_target.clone(),
        merge_group_runs: mg_runs,
        reused: 0,
        refused: 0,
        no_decision: 0,
        unreadable: 0,
        rate: None,
    };
    if let Some(runs) = observation.runs_by_event.get(MERGE_GROUP_EVENT) {
        for run in runs {
            match observation
                .reuse
                .get(run)
                .unwrap_or(&ReuseOutcome::NoDecision)
            {
                ReuseOutcome::Reused => reuse.reused += 1,
                ReuseOutcome::Refused => reuse.refused += 1,
                ReuseOutcome::NoDecision => reuse.no_decision += 1,
                ReuseOutcome::Unreadable => reuse.unreadable += 1,
            }
        }
    }
    reuse.rate = ratio(reuse.reused as f64, mg_runs as u64);
    if !query.events.iter().any(|event| is_merge_group(event)) {
        gaps.push(TelemetryGap {
            signal: "receipt_reuse".to_owned(),
            reason: "merge_group is not among the counted events".to_owned(),
        });
    } else if mg_runs > 0 && reuse.no_decision + reuse.unreadable > 0 {
        gaps.push(TelemetryGap {
            signal: "receipt_reuse".to_owned(),
            reason: format!(
                "{} of {mg_runs} merge-group run(s) published no readable \
                 shipyard-receipt-decision for target `{}`; they count as not reused",
                reuse.no_decision + reuse.unreadable,
                query.receipt_target
            ),
        });
    }

    gaps.extend(observation.listing_gaps.iter().cloned());
    for (event, absence) in &observation.gate_absent {
        let total = observation
            .runs_by_event
            .get(event)
            .map_or(0, BTreeSet::len);
        let mut reason = format!(
            "{} of {total} `{event}` run(s) have no job named exactly `{}`",
            absence.runs, query.gate_job
        );
        if absence.resolved_unstarted > 0 {
            let _ = write!(
                reason,
                "; {} were cancelled before the gate started, which leaves every job name \
                 unevaluated, and count as one gate attempt each through the unevaluated name",
                absence.resolved_unstarted
            );
        }
        if absence.resolved_ran > 0 {
            let _ = write!(
                reason,
                "; {} ran the gate under an unevaluated name, so the workflow's gate `name:` no \
                 longer evaluates to `{}` and should be made a literal again",
                absence.resolved_ran, query.gate_job
            );
        }
        let resolved = absence.resolved_unstarted + absence.resolved_ran;
        if resolved > 0 {
            let _ = write!(reason, " (names: {})", absence.resolved_names.join(" | "));
        }
        if absence.runs > resolved {
            let _ = write!(
                reason,
                "; {} run(s) carry no gate job at all and add no gate minutes (renamed, \
                 moved to another workflow, or not scheduled)",
                absence.runs - resolved
            );
        }
        gaps.push(TelemetryGap {
            signal: "gate_job_name".to_owned(),
            reason,
        });
    }

    let current_queue_depth = match &observation.current_queue_depth {
        Ok(depth) => Some(*depth),
        Err(error) => {
            gaps.push(TelemetryGap {
                signal: "current_queue_depth".to_owned(),
                reason: error.clone(),
            });
            None
        }
    };
    let proxies = proxy::compute(observation, merged_prs);
    if let Some(reason) = &proxies.placement.census_reason {
        gaps.push(TelemetryGap {
            signal: "runner_census".to_owned(),
            reason: reason.clone(),
        });
    }
    gaps.push(TelemetryGap {
        signal: "push_type".to_owned(),
        reason: "push cancellations are not split by push type: that needs the pull \
                 request timeline per run"
            .to_owned(),
    });
    gaps.push(TelemetryGap {
        signal: "queue_depth_history".to_owned(),
        reason: "GitHub exposes only the live merge-queue depth; depth at each batch \
                 formation is not recorded, so shallow-queue batches cannot be told apart \
                 from under-filled ones"
            .to_owned(),
    });

    GateCostReport {
        schema: SCHEMA,
        repo: query.repo.clone(),
        workflow: query.workflow.clone(),
        gate_job: query.gate_job.clone(),
        base_branch: query.base_branch.clone(),
        from: query.from.to_rfc3339(),
        to: query.to.to_rfc3339(),
        pr_head,
        merge_group,
        gate_minutes,
        wasted_gate_minutes,
        merged_prs,
        gate_minutes_per_merged_pr,
        pr_head_runs_per_merged_pr,
        merge_group_runs_per_merged_pr,
        batches: BatchStats {
            batches: batch_count,
            unresolved,
            prs_in_batches,
            mean_prs_per_batch,
            max_entries_to_merge: observation.max_entries_to_merge,
            max_entries_to_build: observation.max_entries_to_build,
            mean_fullness,
            at_capacity,
            distribution,
        },
        reuse,
        current_queue_depth,
        telemetry_gaps: gaps,
        proxies,
        reads: observation.reads,
    }
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn read_json(gh: &GhReader<'_>, args: &[String]) -> Result<Value, String> {
    let text = gh(args)?;
    serde_json::from_str(&text).map_err(|error| format!("unparseable JSON from GitHub: {error}"))
}

/// `gh api --paginate --slurp` over a list endpoint: every page, as an array.
pub(crate) fn read_pages(
    gh: &GhReader<'_>,
    path: &str,
    fields: &[String],
) -> Result<Vec<Value>, String> {
    let mut args = strings(&["api", "--paginate", "--slurp", "-X", "GET", path]);
    for field in fields {
        args.push("-f".to_owned());
        args.push(field.clone());
    }
    match read_json(gh, &args)? {
        Value::Array(pages) => Ok(pages),
        other => Ok(vec![other]),
    }
}

/// Items from every page under `key`, checked against the first page's
/// `total_count` so a short read is an error rather than a smaller number.
pub(crate) fn collect_counted(
    pages: &[Value],
    key: &str,
    what: &str,
) -> Result<Vec<Value>, String> {
    let items: Vec<Value> = pages
        .iter()
        .filter_map(|page| page.get(key).and_then(Value::as_array))
        .flatten()
        .cloned()
        .collect();
    let total = pages
        .first()
        .and_then(|page| page.get("total_count"))
        .and_then(Value::as_u64);
    match total {
        Some(total) if total != items.len() as u64 => Err(format!(
            "{what}: collected {} of total_count {total}; refusing a partial read",
            items.len()
        )),
        _ => Ok(items),
    }
}

fn timestamp(value: &Value, key: &str) -> Option<DateTime<Utc>> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&Utc))
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// GitHub's activity endpoint only filters by a period ending now.
fn activity_period(now: DateTime<Utc>, from: DateTime<Utc>) -> &'static str {
    let span = now - from;
    if span <= Duration::days(1) {
        "day"
    } else if span <= Duration::days(7) {
        "week"
    } else if span <= Duration::days(30) {
        "month"
    } else if span <= Duration::days(90) {
        "quarter"
    } else {
        "year"
    }
}

/// `max_entries_to_merge`, `max_entries_to_build`, `merge_method`.
type QueueRule = (Option<u64>, Option<u64>, Option<String>);

fn read_ruleset(gh: &GhReader<'_>, query: &GateCostQuery) -> Result<QueueRule, String> {
    let rules = read_json(
        gh,
        &strings(&[
            "api",
            &format!("repos/{}/rules/branches/{}", query.repo, query.base_branch),
        ]),
    )?;
    let queue = rules
        .as_array()
        .into_iter()
        .flatten()
        .find(|rule| rule.get("type").and_then(Value::as_str) == Some("merge_queue"))
        .and_then(|rule| rule.get("parameters"))
        .ok_or_else(|| format!("no merge_queue rule applies to `{}`", query.base_branch))?;
    Ok((
        queue.get("max_entries_to_merge").and_then(Value::as_u64),
        queue.get("max_entries_to_build").and_then(Value::as_u64),
        text(queue, "merge_method"),
    ))
}

fn read_batches(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    query: &GateCostQuery,
    now: DateTime<Utc>,
    max_entries: Option<u64>,
) -> Result<Vec<BatchSample>, String> {
    if now - query.from > Duration::days(365) {
        return Err(
            "window starts more than a year ago; repository activity is not \
                    retained that long"
                .to_owned(),
        );
    }
    let pages = read_pages(
        gh,
        &format!("repos/{}/activity", query.repo),
        &[
            format!("ref=refs/heads/{}", query.base_branch),
            format!("activity_type={MERGE_QUEUE_ACTIVITY}"),
            format!("time_period={}", activity_period(now, query.from)),
            "per_page=100".to_owned(),
        ],
    )?;
    let walk_cap = max_entries
        .and_then(|max| u32::try_from(max.saturating_mul(4)).ok())
        .unwrap_or(MAX_BATCH_WALK)
        .clamp(1, MAX_BATCH_WALK);
    let pushes: Vec<(DateTime<Utc>, String, String)> = pages
        .iter()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|activity| {
            let time = timestamp(activity, "timestamp")?;
            (time >= query.from && time < query.to).then_some(())?;
            Some((time, text(activity, "before")?, text(activity, "after")?))
        })
        .collect();
    Ok(parallel_map(&pushes, |(time, before, after)| BatchSample {
        after: after.clone(),
        timestamp: *time,
        entries: walk_first_parent(gh, cache, &query.repo, after, before, walk_cap),
    }))
}

fn walk_first_parent(
    gh: &GhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    after: &str,
    before: &str,
    cap: u32,
) -> Option<u32> {
    let mut sha = after.to_owned();
    for steps in 0..=cap {
        if sha == before {
            return Some(steps);
        }
        // A commit's parents are fixed by its hash, so this answer never goes
        // stale.
        let key = format!("commit-parents:{repo}:{sha}");
        let parents = if let Some(parents) = cache.get(&key) {
            parents
        } else {
            let commit = read_json(
                gh,
                &strings(&["api", &format!("repos/{repo}/git/commits/{sha}")]),
            )
            .ok()?;
            let parents = commit.get("parents")?.clone();
            cache.put(&key, &parents);
            parents
        };
        parents
            .as_array()?
            .first()?
            .get("sha")?
            .as_str()?
            .clone_into(&mut sha);
    }
    None
}

/// Every annotation page of one check run. A completed check run's
/// annotations are final, so only those answers are cached.
fn read_annotation_pages(
    gh: &GhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    job: &Value,
    id: u64,
) -> Result<Vec<Value>, String> {
    let completed = text(job, "status").as_deref() == Some("completed");
    let key = format!("check-run-annotations:{repo}:{id}");
    if completed && let Some(Value::Array(pages)) = cache.get(&key) {
        return Ok(pages);
    }
    let pages = read_pages(
        gh,
        &format!("repos/{repo}/check-runs/{id}/annotations"),
        &["per_page=100".to_owned()],
    )?;
    if completed {
        cache.put(&key, &Value::Array(pages.clone()));
    }
    Ok(pages)
}

fn read_reuse(
    gh: &GhReader<'_>,
    cache: &ReadCache,
    query: &GateCostQuery,
    jobs: &[Value],
) -> ReuseOutcome {
    for job in jobs {
        let wanted = query.receipt_job.as_deref().map_or_else(
            || text(job, "conclusion").is_some_and(|c| c != "skipped"),
            |name| text(job, "name").as_deref() == Some(name),
        );
        let Some(id) = job.get("id").and_then(Value::as_u64).filter(|_| wanted) else {
            continue;
        };
        let Ok(pages) = read_annotation_pages(gh, cache, &query.repo, job, id) else {
            return ReuseOutcome::Unreadable;
        };
        let annotations: Vec<_> = pages
            .iter()
            .flat_map(validation_signals::parse_annotations)
            .collect();
        let mut decided = false;
        for decision in validation_signals::receipt_decisions_from_annotations(&annotations, None) {
            if let ReceiptDecision::Parsed {
                target, verdict, ..
            } = decision
                && target == query.receipt_target
            {
                if verdict == "reuse" {
                    return ReuseOutcome::Reused;
                }
                decided = true;
            }
        }
        // One job publishes a run's decision for a target. Once it is found
        // the remaining jobs cannot change the outcome, and each would cost a
        // read.
        if decided {
            return ReuseOutcome::Refused;
        }
    }
    ReuseOutcome::NoDecision
}

fn read_merged_prs(gh: &GhReader<'_>, query: &GateCostQuery) -> Result<u64, String> {
    let q = format!(
        "repo:{} is:pr is:merged base:{} merged:{}..{}",
        query.repo,
        query.base_branch,
        query.from.format("%Y-%m-%dT%H:%M:%SZ"),
        query.to.format("%Y-%m-%dT%H:%M:%SZ"),
    );
    let result = read_json(
        gh,
        &strings(&[
            "api",
            "-X",
            "GET",
            "search/issues",
            "-f",
            &format!("q={q}"),
            "-f",
            "per_page=1",
        ]),
    )?;
    if result.get("incomplete_results").and_then(Value::as_bool) == Some(true) {
        return Err("search reported incomplete_results; merged-PR count is unreliable".into());
    }
    result
        .get("total_count")
        .and_then(Value::as_u64)
        .ok_or_else(|| "search response carried no total_count".to_owned())
}

fn read_queue_depth(gh: &GhReader<'_>, query: &GateCostQuery) -> Result<u64, String> {
    let (owner, name) = query
        .repo
        .split_once('/')
        .ok_or_else(|| format!("repo `{}` is not OWNER/REPO", query.repo))?;
    let result = read_json(
        gh,
        &strings(&[
            "api",
            "graphql",
            "-f",
            "query=query($owner:String!,$name:String!,$branch:String!){repository(owner:$owner,name:$name){mergeQueue(branch:$branch){entries{totalCount}}}}",
            "-f",
            &format!("owner={owner}"),
            "-f",
            &format!("name={name}"),
            "-f",
            &format!("branch={}", query.base_branch),
        ]),
    )?;
    result
        .pointer("/data/repository/mergeQueue/entries/totalCount")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("no merge queue on `{}`", query.base_branch))
}

/// Read one window from GitHub without a disk cache.
pub fn gather(
    gh: &SyncGhReader<'_>,
    query: &GateCostQuery,
    now: DateTime<Utc>,
) -> Result<GateCostObservation, String> {
    gather_cached(gh, query, now, &ReadCache::disabled())
}

/// The jobs of one run. Cached only when the run attempt listed is completed
/// and every job in the answer is too: a re-run bumps the attempt, so an
/// attempt that finished can gain no further jobs.
fn read_run_jobs(
    gh: &GhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    run: &RunRef,
) -> Result<Vec<Value>, String> {
    let key = run
        .attempt
        .filter(|_| run.completed)
        .map(|attempt| format!("run-jobs:{repo}:{}:attempt-{attempt}", run.id));
    if let Some(Value::Array(jobs)) = key.as_deref().and_then(|key| cache.get(key)) {
        return Ok(jobs);
    }
    let job_pages = read_pages(
        gh,
        &format!("repos/{repo}/actions/runs/{}/jobs", run.id),
        &["filter=all".to_owned(), "per_page=100".to_owned()],
    )?;
    let jobs = collect_counted(&job_pages, "jobs", &format!("jobs of run {}", run.id))?;
    let settled = jobs
        .iter()
        .all(|job| text(job, "status").as_deref() == Some("completed"));
    if let Some(key) = key.filter(|_| settled) {
        cache.put(&key, &Value::Array(jobs.clone()));
    }
    Ok(jobs)
}

/// The identity of one listed run that decides whether its jobs may be cached.
struct RunRef {
    id: u64,
    attempt: Option<u64>,
    completed: bool,
}

/// Read one window from GitHub, serving settled answers from `cache`. Fails
/// only when the gate runs themselves cannot be read completely; every other
/// signal degrades to a gap.
#[allow(clippy::too_many_lines)]
pub fn gather_cached(
    gh: &SyncGhReader<'_>,
    query: &GateCostQuery,
    now: DateTime<Utc>,
    cache: &ReadCache,
) -> Result<GateCostObservation, String> {
    let counted = |args: &[String]| {
        cache.note_github_read();
        gh(args)
    };
    let gh: &SyncGhReader<'_> = &counted;
    let created = format!(
        "created={}..{}",
        query.from.format("%Y-%m-%dT%H:%M:%SZ"),
        query.to.format("%Y-%m-%dT%H:%M:%SZ")
    );
    let mut runs_by_event: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    let mut gate_jobs = Vec::new();
    let mut reuse = BTreeMap::new();
    let mut run_meta = BTreeMap::new();
    let mut placement_jobs = Vec::new();
    let mut listing_gaps = Vec::new();
    let mut gate_absent: BTreeMap<String, GateAbsence> = BTreeMap::new();
    for event in &query.events {
        let pages = read_pages(
            gh,
            &format!(
                "repos/{}/actions/workflows/{}/runs",
                query.repo, query.workflow
            ),
            &[
                format!("event={event}"),
                created.clone(),
                "per_page=100".to_owned(),
            ],
        )?;
        if let Some(total) = pages
            .first()
            .and_then(|page| page.get("total_count"))
            .and_then(Value::as_u64)
            && total > RUN_LISTING_CAP
        {
            return Err(format!(
                "{total} `{event}` runs in the window exceed GitHub's {RUN_LISTING_CAP}-result \
                 listing cap; narrow the window"
            ));
        }
        let mut runs = collect_counted(&pages, "workflow_runs", &format!("`{event}` runs"))?;
        cross_check_listing(gh, query, event, &mut runs, &mut listing_gaps);
        let entry = runs_by_event.entry(event.clone()).or_default();
        let run_refs: Vec<RunRef> = runs
            .iter()
            .filter_map(|run| {
                Some(RunRef {
                    id: run.get("id").and_then(Value::as_u64)?,
                    attempt: run.get("run_attempt").and_then(Value::as_u64),
                    completed: text(run, "status").as_deref() == Some("completed"),
                })
            })
            .collect();
        for run in &runs {
            if let Some(id) = run.get("id").and_then(Value::as_u64) {
                run_meta.insert(
                    id,
                    RunMeta {
                        event: event.clone(),
                        head_branch: text(run, "head_branch"),
                        head_sha: text(run, "head_sha"),
                        created_at: timestamp(run, "created_at"),
                    },
                );
            }
        }
        entry.extend(run_refs.iter().map(|run| run.id));
        let per_run = parallel_map(&run_refs, |run| {
            let jobs = read_run_jobs(gh, cache, &query.repo, run)?;
            let outcome = (event == MERGE_GROUP_EVENT).then(|| read_reuse(gh, cache, query, &jobs));
            Ok::<_, String>((run.id, jobs, outcome))
        });
        for result in per_run {
            let (run_id, jobs, outcome) = result?;
            let exact = jobs
                .iter()
                .any(|job| text(job, "name").as_deref() == Some(query.gate_job.as_str()));
            // No exactly-named gate job: GitHub reported the gate's name
            // unevaluated. One job per attempt stands for the gate: the one
            // that got a runner, else the first that was not skipped. The
            // gate's skipped or cancelled alternates carry the same resolved
            // name and must not multiply the attempt.
            let fallback = if exact {
                BTreeMap::new()
            } else {
                resolved_gate_jobs(&jobs, &query.gate_job)
            };
            for job in &jobs {
                let runner_name = text(job, "runner_name").filter(|name| !name.trim().is_empty());
                let labels = job_labels(job);
                let name = text(job, "name").unwrap_or_default();
                placement_jobs.push(PlacementSample {
                    name: name.clone(),
                    labels,
                    runner_assigned: runner_name.is_some(),
                    conclusion: text(job, "conclusion"),
                });
                let is_gate = if exact {
                    name == query.gate_job
                } else {
                    job.get("id")
                        .and_then(Value::as_u64)
                        .is_some_and(|id| fallback.values().any(|picked| *picked == id))
                };
                if !is_gate {
                    continue;
                }
                if !exact {
                    let absence = gate_absent.entry(event.clone()).or_default();
                    if absence.resolved_names.len() < 3 && !absence.resolved_names.contains(&name) {
                        absence.resolved_names.push(name.clone());
                    }
                }
                gate_jobs.push(GateJobSample::from_job(run_id, event, job));
            }
            if !exact {
                let ran = jobs.iter().any(|job| {
                    job.get("id")
                        .and_then(Value::as_u64)
                        .is_some_and(|id| fallback.values().any(|picked| *picked == id))
                        && text(job, "runner_name").is_some_and(|name| !name.trim().is_empty())
                });
                let absence = gate_absent.entry(event.clone()).or_default();
                absence.runs += 1;
                if ran {
                    absence.resolved_ran += 1;
                } else if !fallback.is_empty() {
                    absence.resolved_unstarted += 1;
                }
            }
            if let Some(outcome) = outcome {
                reuse.insert(run_id, outcome);
            }
        }
    }
    let (max_entries_to_merge, max_entries_to_build, merge_method, ruleset_error) =
        match read_ruleset(gh, query) {
            Ok((merge, build, method)) => (merge, build, method, None),
            Err(error) => (None, None, None, Some(error)),
        };
    let merged_prs = read_merged_prs(gh, query);
    let batches = read_batches(gh, cache, query, now, max_entries_to_merge);
    let current_queue_depth = read_queue_depth(gh, query);
    let runner_census = read_runner_census(gh, &query.repo);
    Ok(GateCostObservation {
        query: query.clone(),
        runs_by_event,
        gate_jobs,
        merged_prs,
        batches,
        max_entries_to_merge,
        max_entries_to_build,
        merge_method,
        reuse,
        current_queue_depth,
        ruleset_error,
        run_meta,
        placement_jobs,
        runner_census,
        reads: cache.stats(),
        listing_gaps,
        gate_absent,
    })
}

/// Per run attempt, the id of the one job that stands for `gate_job` when no
/// job carries its exact name: among jobs whose unevaluated name resolves to
/// it and that were not skipped, the first that got a runner, else the first.
fn resolved_gate_jobs(jobs: &[Value], gate_job: &str) -> BTreeMap<u64, u64> {
    let mut picked: BTreeMap<u64, (bool, u64)> = BTreeMap::new();
    for job in jobs {
        let (Some(id), Some(name)) = (job.get("id").and_then(Value::as_u64), text(job, "name"))
        else {
            continue;
        };
        if !job_name::matches(gate_job, &name)
            || text(job, "conclusion").as_deref() == Some("skipped")
        {
            continue;
        }
        let attempt = job.get("run_attempt").and_then(Value::as_u64).unwrap_or(1);
        let ran = text(job, "runner_name").is_some_and(|name| !name.trim().is_empty());
        match picked.get(&attempt) {
            Some((true, _)) => {}
            Some((false, _)) if !ran => {}
            _ => {
                picked.insert(attempt, (ran, id));
            }
        }
    }
    picked
        .into_iter()
        .map(|(attempt, (_, id))| (attempt, id))
        .collect()
}

/// Pages of the unfiltered event listing walked for the cross-check; 1000
/// runs, the same ceiling as a filtered listing.
const WALK_MAX_PAGES: u32 = 10;

/// Cross-check a `created=` window listing against the plain event listing,
/// which GitHub serves newest first without its search backend.
///
/// The `created` filter has been observed to answer with a short list whose
/// own `total_count` agrees with it (11 `pull_request` runs where 624 existed),
/// which [`collect_counted`] cannot catch. Runs the walk finds in the window
/// that the filtered listing missed are added to `runs`, and the disagreement
/// is reported as a gap rather than silently corrected.
fn cross_check_listing(
    gh: &GhReader<'_>,
    query: &GateCostQuery,
    event: &str,
    runs: &mut Vec<Value>,
    gaps: &mut Vec<TelemetryGap>,
) {
    let listed: BTreeSet<u64> = runs
        .iter()
        .filter_map(|run| run.get("id").and_then(Value::as_u64))
        .collect();
    let path = format!(
        "repos/{}/actions/workflows/{}/runs",
        query.repo, query.workflow
    );
    let mut seen = BTreeSet::new();
    let mut missed = Vec::new();
    let mut reached_start = false;
    for page in 1..=WALK_MAX_PAGES {
        let args = strings(&[
            "api",
            "-X",
            "GET",
            &path,
            "-f",
            &format!("event={event}"),
            "-f",
            "per_page=100",
            "-f",
            &format!("page={page}"),
        ]);
        let items = match read_json(gh, &args) {
            Ok(value) => value
                .get("workflow_runs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            Err(error) => {
                gaps.push(TelemetryGap {
                    signal: "run_listing".to_owned(),
                    reason: format!(
                        "`{event}` run listing could not be cross-checked against the event \
                         walk: {error}"
                    ),
                });
                return;
            }
        };
        let mut oldest = None;
        for run in &items {
            let Some(id) = run.get("id").and_then(Value::as_u64) else {
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            let Some(created) = timestamp(run, "created_at") else {
                continue;
            };
            oldest = Some(oldest.map_or(created, |old: DateTime<Utc>| old.min(created)));
            if created >= query.from && created < query.to && !listed.contains(&id) {
                missed.push(run.clone());
            }
        }
        if items.len() < 100 || oldest.is_none_or(|oldest| oldest < query.from) {
            reached_start = true;
            break;
        }
    }
    if !missed.is_empty() {
        gaps.push(TelemetryGap {
            signal: "run_listing".to_owned(),
            reason: format!(
                "the `created` window listing returned {} `{event}` run(s) but the event walk \
                 found {} more in the window; they are included (GitHub answered the filtered \
                 listing short without flagging it)",
                listed.len(),
                missed.len()
            ),
        });
        runs.extend(missed);
    }
    if !reached_start {
        gaps.push(TelemetryGap {
            signal: "run_listing".to_owned(),
            reason: format!(
                "the `{event}` event walk stopped at {WALK_MAX_PAGES} pages before the window \
                 start, so a short filtered listing older than that would go unnoticed"
            ),
        });
    }
}

fn job_labels(job: &Value) -> Vec<String> {
    job.get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn runner_label_sets(pages: &[Value]) -> Vec<BTreeSet<String>> {
    pages
        .iter()
        .filter_map(|page| page.get("runners").and_then(Value::as_array))
        .flatten()
        .map(|runner| {
            runner
                .get("labels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|label| label.get("name").and_then(Value::as_str))
                .map(str::to_lowercase)
                .collect()
        })
        .collect()
}

/// Registered runners' labels, repository and organisation scope. Offline
/// runners count: they still advertise their labels. Ephemeral runners that
/// are not registered right now are invisible here, which is why placement
/// also treats a label set as served when any job in the window got a runner.
fn read_runner_census(gh: &GhReader<'_>, repo: &str) -> Result<RunnerCensus, String> {
    let mut census = RunnerCensus::default();
    let mut read_any = false;
    let mut scopes = vec![(format!("repos/{repo}/actions/runners"), "repository")];
    if let Some((owner, _)) = repo.split_once('/') {
        scopes.push((format!("orgs/{owner}/actions/runners"), "organisation"));
    }
    let mut errors = Vec::new();
    for (path, scope) in scopes {
        match read_pages(gh, &path, &["per_page=100".to_owned()]) {
            Ok(pages) => {
                read_any = true;
                census.label_sets.extend(runner_label_sets(&pages));
            }
            Err(error) => {
                census.unread_scopes.push(scope.to_owned());
                errors.push(format!("{scope} runners: {error}"));
            }
        }
    }
    if read_any {
        Ok(census)
    } else {
        Err(errors.join("; "))
    }
}

/// Parse a window length such as `48h`, `2d`, or `90m`.
pub fn parse_window(value: &str) -> Result<Duration, String> {
    let trimmed = value.trim();
    let unit_len = trimmed.chars().last().map_or(0, char::len_utf8);
    let (number, unit) = trimmed.split_at(trimmed.len() - unit_len);
    let amount: i64 = number
        .parse()
        .map_err(|_| format!("invalid window {value:?}; use Nd, Nh, or Nm, for example 48h"))?;
    if amount <= 0 {
        return Err(format!("window {value:?} must be positive"));
    }
    match unit {
        "d" => Ok(Duration::days(amount)),
        "h" => Ok(Duration::hours(amount)),
        "m" => Ok(Duration::minutes(amount)),
        _ => Err(format!(
            "invalid window {value:?}; use Nd, Nh, or Nm, for example 48h"
        )),
    }
}

#[cfg(test)]
mod tests;
