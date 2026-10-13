//! Named causes of red cases, read from each case's job log by the same
//! extractor `shipyard diagnose` uses.
//!
//! A red required check, or a merge-group gate that did not pass, is a job
//! with a log. Its cause is usually in that log: the failing test of a `CTest`
//! summary, a compiler error, a runner failure GitHub recorded. This module
//! reads the job and its log, runs [`crate::diagnose::build`] on it, and
//! reduces the diagnosis to one [`CaseCause`]: a class, a signature, the
//! evidence lines and a confidence.
//!
//! A cause is **named** when its confidence is `high` or `medium`:
//!
//! | confidence | when |
//! |---|---|
//! | `high` | the log names failing tests, or GitHub's own record proves an infrastructure failure, a timeout, a supersede or a stale base |
//! | `medium` | no test, but scored error lines in the failing step |
//! | `low` | only the step's last lines before it exited, or a cancel with no recorded reason |
//! | `none` | nothing to read: no log, no evidence |
//!
//! What a completed job yields is cached, never the log. Causes read here
//! are retrospective: they say what the log held when the job finished,
//! which is the earliest an extractor could have named it, so `named_at` is
//! the job's completion and `retrospective` is true.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::super::{ReadCache, SyncGhReader, parallel_map, strings};
use super::cases::Case;
use super::{ScorecardObservation, ejection_cause, is_red};
use crate::diagnose::{self, CheckDiagnosis, Context, Job, classify};

/// Who named a cause read here.
pub const SOURCE_EXTRACTOR: &str = "extractor";
/// Cache key version: bump when a cause's shape or meaning changes.
const EXTRACT_VERSION: u32 = 1;
/// Most evidence lines kept per cause.
pub const MAX_EXCERPT: usize = diagnose::evidence::MAX_GROUP_LINES;

/// The cause of one red job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseCause {
    /// [`SOURCE_EXTRACTOR`] here; `human`, `agent` or `fix-link` elsewhere.
    pub source: String,
    /// `test`, `infra`, `cancelled`, `timeout`, `drift` or `unknown`.
    pub class: String,
    /// `high`, `medium`, `low` or `none`.
    pub confidence: String,
    /// The most specific name for the cause: the first failing test, else
    /// the most decisive evidence line.
    pub signature: Option<String>,
    /// The step that failed.
    pub failing_step: Option<String>,
    /// Failing tests, capped as `diagnose` caps them.
    pub failing_tests: Vec<String>,
    /// `diagnose`'s class and rule, verbatim.
    pub diagnose_class: String,
    /// `diagnose`'s rule.
    pub diagnose_rule: String,
    /// 1-based line of the evidence in the job log.
    pub evidence_line: Option<usize>,
    /// The evidence lines.
    pub excerpt: Vec<String>,
    /// When the cause was nameable: the job's completion.
    pub named_at: Option<String>,
    /// Read after the fact rather than recorded when the job finished.
    pub retrospective: bool,
}

impl CaseCause {
    /// Whether the cause counts as named (`high` or `medium`).
    #[must_use]
    pub fn named(&self) -> bool {
        matches!(self.confidence.as_str(), "high" | "medium")
    }
}

/// Reduce one check's diagnosis to its cause.
#[must_use]
pub fn from_diagnosis(check: &CheckDiagnosis, completed_at: Option<&str>) -> CaseCause {
    let rule = check.classification.rule.as_str();
    let tests = &check.failing_tests.names;
    let signal = check.evidence.iter().find(|group| group.kind == "signal");
    let tail = check.evidence.iter().find(|group| group.kind == "tail");
    let decisive = check.evidence.first();
    let (class, confidence, signature): (&str, &str, Option<String>) =
        match check.classification.class.as_str() {
            classify::INFRA => ("infra", "high", Some(check.classification.why.clone())),
            classify::INTERRUPTED => match rule {
                "timeout" => ("timeout", "high", Some(check.classification.why.clone())),
                "superseded" => ("cancelled", "high", Some(check.classification.why.clone())),
                _ => ("cancelled", "low", Some(check.classification.why.clone())),
            },
            classify::STALE_BASE => ("drift", "high", Some(check.classification.why.clone())),
            _ if !tests.is_empty() => ("test", "high", tests.first().cloned()),
            _ => match (signal, tail) {
                (Some(group), _) => ("unknown", "medium", group.lines.first().cloned()),
                (None, Some(group)) => ("unknown", "low", group.lines.last().cloned()),
                (None, None) => ("unknown", "none", None),
            },
        };
    CaseCause {
        source: SOURCE_EXTRACTOR.to_owned(),
        class: class.to_owned(),
        confidence: confidence.to_owned(),
        signature,
        failing_step: check.failing_step.as_ref().map(|step| step.name.clone()),
        failing_tests: tests.clone(),
        diagnose_class: check.classification.class.clone(),
        diagnose_rule: check.classification.rule.clone(),
        evidence_line: decisive.map(|group| group.line),
        excerpt: decisive
            .map(|group| group.lines.iter().take(MAX_EXCERPT).cloned().collect())
            .unwrap_or_default(),
        named_at: completed_at.map(str::to_owned),
        retrospective: true,
    }
}

/// Diagnose one red job from its jobs-API payload and log. `None` when the
/// job is not red.
#[must_use]
pub fn cause_of(job: &Job, log: Option<&str>) -> Option<CaseCause> {
    let mut logs = std::collections::HashMap::new();
    if let Some(log) = log {
        logs.insert(job.id, log.to_owned());
    }
    let doc = diagnose::build(
        std::slice::from_ref(job),
        &logs,
        std::slice::from_ref(&job.name),
        &Context::new(),
        diagnose::DEFAULT_MAX_BYTES,
    );
    doc.checks
        .first()
        .map(|check| from_diagnosis(check, job.completed_at.as_deref()))
}

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
    pub by_job: BTreeMap<u64, CaseCause>,
    /// Jobs that could not be read.
    pub unreadable: Vec<Unreadable>,
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

fn read_cause(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    id: u64,
) -> Result<CaseCause, String> {
    let key = format!("case-cause:v{EXTRACT_VERSION}:{repo}:{id}");
    if let Some(value) = cache.get(&key)
        && let Ok(cause) = serde_json::from_value(value)
    {
        return Ok(cause);
    }
    let payload = gh(&strings(&[
        "api",
        &format!("repos/{repo}/actions/jobs/{id}"),
    ]))
    .map_err(|error| format!("job {id}: {error}"))?;
    let job: Job = serde_json::from_str(&payload).map_err(|error| format!("job {id}: {error}"))?;
    if job.status.as_deref() != Some("completed") {
        return Err(format!("job {id} has not completed"));
    }
    let log = match gh(&strings(&[
        "api",
        &format!("repos/{repo}/actions/jobs/{id}/logs"),
    ])) {
        Ok(text) => Some(text),
        Err(error) if log_absent(&error) => None,
        Err(error) => return Err(format!("log of job {id}: {error}")),
    };
    let cause = cause_of(&job, log.as_deref()).ok_or_else(|| format!("job {id} did not fail"))?;
    if let Ok(value) = serde_json::to_value(&cause) {
        cache.put(&key, &value);
    }
    Ok(cause)
}

/// Read the cause of every job in `jobs`.
#[must_use]
pub fn gather(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    jobs: &BTreeSet<u64>,
) -> CausesRead {
    let ids: Vec<u64> = jobs.iter().copied().collect();
    let results = parallel_map(&ids, |id| read_cause(gh, cache, repo, *id));
    let mut read = CausesRead::default();
    for (id, result) in ids.into_iter().zip(results) {
        match result {
            Ok(cause) => {
                read.by_job.insert(id, cause);
            }
            Err(reason) => read.unreadable.push(Unreadable { job_id: id, reason }),
        }
    }
    read
}

/// Put each read cause on its case: `cause` and `evidence` are set, and
/// `evidence` leaves the row's `unread` list.
pub fn attach(rows: &mut [Case], read: &CausesRead) {
    for row in rows {
        let Some(cause) = row.job_id.and_then(|id| read.by_job.get(&id)) else {
            continue;
        };
        row.evidence = (!cause.excerpt.is_empty()).then(|| cause.excerpt.join("\n"));
        row.cause = Some(cause.clone());
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
    /// Cases with a named cause (`high` or `medium`).
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
    /// Whether every cause was read after the fact.
    pub retrospective: bool,
}

fn minutes_between(from: &str, to: &str) -> Option<f64> {
    let parse = |value: &str| chrono::DateTime::parse_from_rfc3339(value).ok();
    #[allow(clippy::cast_precision_loss)]
    let seconds = (parse(to)? - parse(from)?).num_seconds().max(0) as f64;
    Some(seconds / 60.0)
}

/// Score the red cases among `rows` (after [`attach`]).
#[must_use]
pub fn compute(rows: &[Case]) -> CauseNaming {
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
        retrospective: true,
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
        if !cause.named() {
            continue;
        }
        naming.named += 1;
        *naming.by_class.entry(cause.class.clone()).or_insert(0) += 1;
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
        "named causes: {}/{} red cases ({share}); {} without a job, {} unreadable",
        naming.named, naming.reds, naming.without_job, naming.unreadable
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
