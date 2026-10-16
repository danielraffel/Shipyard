//! Named causes of red cases, read from each case's job log.
//!
//! A red required check, or a merge-group gate that did not pass, is a job
//! with a log. This module reads the job and its log and builds its
//! `ci.cause/v1` record with [`crate::diagnose::cause::build`]: C2 names the
//! cause, `diagnose` adds GitHub's own record and the evidence lines. A cause
//! is **named** when its class is not `unknown` and its confidence is `high`
//! or `medium` (see [`crate::diagnose::cause`]).
//!
//! The cause is always built here from the log. A job that named its own
//! cause (`shipyard cause extract --notice` in an `if: failure()` step)
//! carries a record in its log; when that record is bound to this job (repo,
//! head, run, attempt, job name), names the same cause, and sits in a step the
//! jobs API timed, the cause is not retrospective and its `named_at` is that
//! step's completion by GitHub's clock. Otherwise the cause is retrospective:
//! `named_at` is the job's completion, the earliest an extractor could have
//! named it. A record printed by the job is never trusted for its content or
//! its time: pull-request code can print one. What a completed job yields is
//! cached, never the log.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::Serialize;

use super::super::{ReadCache, SyncGhReader, parallel_map, strings};
use super::cases::Case;
use super::{ScorecardObservation, ejection_cause, is_red};
use crate::diagnose::Job;
use crate::diagnose::cause::{self, CiCause, Place};

/// Default most job logs one run reads (`--causes-max-jobs`). Each costs two
/// GitHub API calls; a cached job costs none and does not count.
pub const DEFAULT_MAX_JOBS: usize = 200;
/// Why a job over that budget was not read.
pub const OVER_BUDGET: &str = "skipped: over the --causes-max-jobs budget";
/// Cache key version: bump when a cause's shape or meaning changes.
const EXTRACT_VERSION: u32 = 7;

/// A red job whose cause could not be read, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Unreadable {
    /// Job id.
    pub job_id: u64,
    /// Why.
    pub reason: String,
}

/// The causes read for a window's red jobs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CausesRead {
    /// Cause by job id.
    pub by_job: BTreeMap<u64, CiCause>,
    /// Jobs that could not be read, including those over the budget.
    pub unreadable: Vec<Unreadable>,
    /// Most uncached jobs this read could fetch.
    pub budget: usize,
    /// Uncached jobs left unread because the budget ran out.
    pub skipped_for_budget: usize,
}

/// The jobs of every red case the scorecard read: red required checks on
/// merged pull requests' commits, and merge-group gates that did not pass.
#[must_use]
pub fn red_jobs(observation: &ScorecardObservation) -> BTreeSet<u64> {
    let required: BTreeSet<&str> = observation
        .required
        .as_ref()
        .map(|(contexts, _)| contexts.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let checks = observation
        .prs
        .iter()
        .flat_map(|pr| &pr.commits)
        .flat_map(|commit| &commit.checks)
        .filter(|check| {
            required.contains(check.name.as_str()) && check.outcome.as_deref().is_some_and(is_red)
        })
        .filter_map(|check| check.job_id);
    let gates = observation
        .merge_group_runs
        .iter()
        .filter(|run| {
            run.gate
                .as_ref()
                .is_some_and(|gate| gate.status == "completed" && ejection_cause(gate).is_some())
        })
        .filter_map(|run| run.gate_job_id);
    checks.chain(gates).collect()
}

/// Whether a log read failed because GitHub holds no log for the job, as
/// opposed to a read that failed.
fn log_absent(error: &str) -> bool {
    ["HTTP 404", "HTTP 410", "BlobNotFound"]
        .iter()
        .any(|marker| error.contains(marker))
}

fn cache_key(repo: &str, id: u64) -> String {
    format!("case-cause:v{EXTRACT_VERSION}:{repo}:{id}")
}

fn cached(cache: &ReadCache, repo: &str, id: u64) -> Option<CiCause> {
    serde_json::from_value(cache.get(&cache_key(repo, id))?).ok()
}

fn read_cause(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    id: u64,
) -> Result<CiCause, String> {
    let key = cache_key(repo, id);
    let payload = gh(&strings(&[
        "api",
        &format!("repos/{repo}/actions/jobs/{id}"),
    ]))
    .map_err(|error| format!("job {id}: {error}"))?;
    let job: Job = serde_json::from_str(&payload).map_err(|error| format!("job {id}: {error}"))?;
    if job.status.as_deref() != Some("completed") {
        return Err(format!("job {id} has not completed"));
    }
    let log = match super::super::fetch_job_log(gh, repo, id) {
        Ok(text) => Some(text),
        Err(error) if log_absent(&error) => None,
        Err(error) => return Err(format!("log of job {id}: {error}")),
    };
    let place = Place {
        repo: repo.to_owned(),
        ..Place::default()
    };
    let mut cause = cause::build(&job, log.as_deref(), &place, job.completed_at.clone(), true)
        .ok_or_else(|| format!("job {id} did not fail"))?;
    cause::adopt_printed_time(&mut cause, log.as_deref(), &job, repo);
    if let Ok(value) = serde_json::to_value(&cause) {
        cache.put(&key, &value);
    }
    Ok(cause)
}

/// Read the cause of every job in `jobs`. A cached cause costs nothing; at
/// most `max_jobs` uncached jobs are fetched, newest first (Actions job ids
/// grow with creation time), and the rest are unreadable with
/// [`OVER_BUDGET`], so they count as unread rather than as a cause.
#[must_use]
pub fn gather(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    jobs: &BTreeSet<u64>,
    max_jobs: usize,
) -> CausesRead {
    let mut read = CausesRead {
        budget: max_jobs,
        ..CausesRead::default()
    };
    let mut uncached = Vec::new();
    for &id in jobs {
        match cached(cache, repo, id) {
            Some(cause) => {
                read.by_job.insert(id, cause);
            }
            None => uncached.push(id),
        }
    }
    uncached.sort_unstable_by(|a, b| b.cmp(a));
    let over = uncached.split_off(max_jobs.min(uncached.len()));
    let results = parallel_map(&uncached, |id| read_cause(gh, cache, repo, *id));
    for (id, result) in uncached.into_iter().zip(results) {
        match result {
            Ok(cause) => {
                read.by_job.insert(id, cause);
            }
            Err(reason) => read.unreadable.push(Unreadable { job_id: id, reason }),
        }
    }
    read.skipped_for_budget = over.len();
    read.unreadable
        .extend(over.into_iter().map(|job_id| Unreadable {
            job_id,
            reason: OVER_BUDGET.to_owned(),
        }));
    read
}

/// Put each read cause on its case: `cause` and `evidence` are set, and
/// `evidence` leaves the row's `unread` list.
pub fn attach(rows: &mut [Case], read: &CausesRead) {
    for row in rows {
        let Some(cause) = row.job_id.and_then(|id| read.by_job.get(&id)) else {
            continue;
        };
        let mut cause = cause.clone();
        cause.pr = cause.pr.or(row.pr);
        row.evidence =
            (!cause.evidence.excerpt.is_empty()).then(|| cause.evidence.excerpt.join("\n"));
        row.cause = Some(cause);
        row.unread.retain(|field| *field != "evidence");
    }
}

/// How many red cases have a named cause, and how fast.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CauseNaming {
    /// Red cases: red required checks and failed merge-group gates.
    pub reds: usize,
    /// Of those, cases with no job to read (a status context, or a check
    /// GitHub listed without an id).
    pub without_job: usize,
    /// Cases whose job could not be read.
    pub unreadable: usize,
    /// Cases with a named cause: a class, at `high` or `medium` confidence.
    pub named: usize,
    /// `named / reds`.
    pub named_share: Option<f64>,
    /// Cases by confidence; `unread` for those without a cause.
    pub by_confidence: BTreeMap<String, usize>,
    /// Named cases by class.
    pub by_class: BTreeMap<String, usize>,
    /// Median minutes from the case to its cause's `named_at`.
    pub time_to_named_median_minutes: Option<f64>,
    /// Named cases with both times.
    pub time_to_named_n: usize,
    /// Causes the job named in its own log, rather than read after it.
    pub recorded_in_job: usize,
    /// Whether every cause was read after the fact.
    pub retrospective: bool,
    /// Most uncached jobs the read could fetch (`--causes-max-jobs`).
    pub budget: usize,
    /// Red jobs left unread because the budget ran out; counted in
    /// `unreadable`, never as a cause.
    pub skipped_for_budget: usize,
}

fn minutes_between(from: &str, to: &str) -> Option<f64> {
    let parse = |value: &str| chrono::DateTime::parse_from_rfc3339(value).ok();
    #[allow(clippy::cast_precision_loss)]
    let seconds = (parse(to)? - parse(from)?).num_seconds().max(0) as f64;
    Some(seconds / 60.0)
}

/// Score the red cases among `rows` (after [`attach`]) read by `read`.
#[must_use]
pub fn compute(rows: &[Case], read: &CausesRead) -> CauseNaming {
    let reds: Vec<&Case> = rows
        .iter()
        .filter(|row| matches!(row.kind, "required_check_red" | "merge_group_gate_failed"))
        .collect();
    let mut naming = CauseNaming {
        reds: reds.len(),
        without_job: reds.iter().filter(|row| row.job_id.is_none()).count(),
        unreadable: 0,
        named: 0,
        named_share: None,
        by_confidence: BTreeMap::new(),
        by_class: BTreeMap::new(),
        time_to_named_median_minutes: None,
        time_to_named_n: 0,
        recorded_in_job: 0,
        retrospective: true,
        budget: read.budget,
        skipped_for_budget: read.skipped_for_budget,
    };
    let mut minutes = Vec::new();
    for row in &reds {
        let Some(cause) = &row.cause else {
            if row.job_id.is_some() {
                naming.unreadable += 1;
            }
            *naming.by_confidence.entry("unread".to_owned()).or_insert(0) += 1;
            continue;
        };
        *naming
            .by_confidence
            .entry(cause.confidence.clone())
            .or_insert(0) += 1;
        naming.retrospective &= cause.retrospective;
        naming.recorded_in_job += usize::from(!cause.retrospective);
        if !cause.named() {
            continue;
        }
        naming.named += 1;
        *naming
            .by_class
            .entry(cause.failure_class.clone())
            .or_insert(0) += 1;
        if let (Some(at), Some(named_at)) = (&row.created_at, &cause.named_at)
            && let Some(value) = minutes_between(at, named_at)
        {
            minutes.push(value);
        }
    }
    #[allow(clippy::cast_precision_loss)]
    {
        naming.named_share = (naming.reds > 0).then(|| naming.named as f64 / naming.reds as f64);
    }
    naming.time_to_named_n = minutes.len();
    minutes.sort_by(f64::total_cmp);
    naming.time_to_named_median_minutes = match minutes.len() {
        0 => None,
        n if n % 2 == 1 => Some(minutes[n / 2]),
        n => Some(f64::midpoint(minutes[n / 2 - 1], minutes[n / 2])),
    };
    naming
}

/// The human-readable lines.
#[must_use]
pub fn render(naming: &CauseNaming) -> String {
    let mut out = String::new();
    let share = naming.named_share.map_or_else(
        || "n/a".to_owned(),
        |share| format!("{:.0}%", share * 100.0),
    );
    let _ = writeln!(
        out,
        "named causes: {}/{} red cases ({share}); {} without a job, {} unreadable \
         ({} skipped over the --causes-max-jobs budget of {})",
        naming.named,
        naming.reds,
        naming.without_job,
        naming.unreadable,
        naming.skipped_for_budget,
        naming.budget
    );
    let confidence: Vec<String> = naming
        .by_confidence
        .iter()
        .map(|(name, count)| format!("{name} {count}"))
        .collect();
    let _ = writeln!(out, "  by confidence: {}", confidence.join(", "));
    let class: Vec<String> = naming
        .by_class
        .iter()
        .map(|(name, count)| format!("{name} {count}"))
        .collect();
    let _ = writeln!(out, "  named by class: {}", class.join(", "));
    let time = naming
        .time_to_named_median_minutes
        .map_or_else(|| "n/a".to_owned(), |minutes| format!("{minutes:.1} min"));
    let _ = writeln!(out, "  named in the job itself: {}", naming.recorded_in_job);
    let _ = writeln!(
        out,
        "  time to named cause: median {time} (n={}){}",
        naming.time_to_named_n,
        if naming.retrospective {
            ", retrospective: the cause was nameable when its job finished, but nothing recorded it then"
        } else {
            ""
        }
    );
    out
}

#[cfg(test)]
mod tests;
