//! Which failed workflow runs are interruptions that one rerun may repair.
//!
//! A conclusion string is not evidence. `CANCELLED` is what GitHub reports
//! for a run a person cancelled, a run a concurrency group superseded, a run
//! a newer push replaced, and a run whose job never reached a runner. Only
//! the last says nothing about the code, so only it is rerun.
//!
//! ## The signature
//!
//! Every job of the run's latest attempt that did not pass must be a
//! runner-less cancellation:
//!
//! * `conclusion == cancelled` and an empty `runner_name`: no runner ever
//!   took the job, so no code ran;
//! * created-to-completed within [`RUNNERLESS_CANCEL_MIN_MINUTES`] to
//!   [`RUNNERLESS_CANCEL_MAX_MINUTES`]: the window in which the fleet's
//!   runner-assignment timeout cancels a job nothing picked up. A superseding
//!   push or a concurrency cancel ends a job within seconds, and a person who
//!   cancels a long-queued run is outside the window as often as inside it;
//! * no [`crate::environment_requeue::SUPERSEDED_ANNOTATIONS`] text on the
//!   job's check run, because a concurrency supersede can also land inside the
//!   window with no runner.
//!
//! One job that failed, timed out, failed to start, or was cancelled after a
//! runner took it makes the whole run a genuine result. Missing timestamps and
//! an empty job list fail closed.
//!
//! ## The budget
//!
//! [`RERUN_BUDGET`] is one rerun per pull request, head, and required check.
//! It is a constant, not a default: [`clamp_rerun_budget`] caps any configured
//! value to it, so no flag or config can widen it.

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Reruns allowed per pull request, head, and required check.
pub const RERUN_BUDGET: u32 = 1;

/// Fewest whole minutes from job creation to cancellation that match the
/// runner-assignment timeout signature.
pub const RUNNERLESS_CANCEL_MIN_MINUTES: i64 = 16;

/// Most whole minutes from job creation to cancellation that match the
/// runner-assignment timeout signature.
pub const RUNNERLESS_CANCEL_MAX_MINUTES: i64 = 20;

/// Cap a configured rerun budget at [`RERUN_BUDGET`].
#[must_use]
pub const fn clamp_rerun_budget(configured: u32) -> u32 {
    if configured < RERUN_BUDGET {
        configured
    } else {
        RERUN_BUDGET
    }
}

/// One job of a run's latest attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterruptionJob {
    /// Job name.
    pub name: String,
    /// Lower-case conclusion, when completed.
    pub conclusion: Option<String>,
    /// Runner that took the job; `None` or blank when none did.
    pub runner_name: Option<String>,
    /// When the job was created (queued).
    pub created_at: Option<DateTime<Utc>>,
    /// When the job completed.
    pub completed_at: Option<DateTime<Utc>>,
    /// Whether the job's check run carries a concurrency-supersede annotation.
    pub superseded: bool,
}

impl InterruptionJob {
    fn passed(&self) -> bool {
        matches!(
            self.conclusion.as_deref(),
            Some("success" | "skipped" | "neutral")
        )
    }

    fn has_runner(&self) -> bool {
        self.runner_name
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty())
    }

    /// Whether this job could be a runner-less cancellation, before the
    /// annotation read that rules out a supersede.
    #[must_use]
    pub fn is_runnerless_cancel(&self) -> bool {
        self.conclusion.as_deref() == Some("cancelled") && !self.has_runner()
    }
}

/// The interruption a run matched.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InterruptionHit {
    /// Signature name.
    pub signature: &'static str,
    /// The job it matched.
    pub job: String,
    /// Whole minutes from creation to cancellation.
    pub waited_minutes: i64,
}

/// Classify one run's latest-attempt jobs.
///
/// # Errors
///
/// The reason the run is not an interruption: a job that did not pass and
/// does not match the signature, or no job to read.
pub fn classify_jobs(jobs: &[InterruptionJob]) -> Result<Vec<InterruptionHit>, String> {
    let mut hits = Vec::new();
    for job in jobs.iter().filter(|job| !job.passed()) {
        hits.push(classify_job(job)?);
    }
    if hits.is_empty() {
        return Err("no job of the run failed, so there is no interruption to repair".to_owned());
    }
    Ok(hits)
}

fn classify_job(job: &InterruptionJob) -> Result<InterruptionHit, String> {
    let conclusion = job.conclusion.as_deref().unwrap_or("none");
    if conclusion != "cancelled" {
        return Err(format!(
            "job `{}` concluded `{conclusion}`, which is a result, not an interruption",
            job.name
        ));
    }
    if job.has_runner() {
        return Err(format!(
            "job `{}` was cancelled after runner `{}` took it",
            job.name,
            job.runner_name.as_deref().unwrap_or_default()
        ));
    }
    if job.superseded {
        return Err(format!(
            "job `{}` was superseded by a higher-priority run in its concurrency group",
            job.name
        ));
    }
    let (Some(created), Some(completed)) = (job.created_at, job.completed_at) else {
        return Err(format!(
            "job `{}` has no readable created or completed time",
            job.name
        ));
    };
    let waited = completed.signed_duration_since(created).num_minutes();
    if !(RUNNERLESS_CANCEL_MIN_MINUTES..=RUNNERLESS_CANCEL_MAX_MINUTES).contains(&waited) {
        return Err(format!(
            "job `{}` was cancelled with no runner after {waited} min, outside the \
             {RUNNERLESS_CANCEL_MIN_MINUTES}-{RUNNERLESS_CANCEL_MAX_MINUTES} min \
             runner-assignment timeout",
            job.name
        ));
    }
    Ok(InterruptionHit {
        signature: "runnerless_cancel",
        job: job.name.clone(),
        waited_minutes: waited,
    })
}

/// Parse one job row from the Actions jobs API. The supersede flag is read
/// separately from the check run's annotations.
#[must_use]
pub fn job_from_api(row: &serde_json::Value) -> InterruptionJob {
    let text = |key: &str| {
        row.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let time = |key: &str| {
        text(key)
            .and_then(|at| DateTime::parse_from_rfc3339(&at).ok())
            .map(|at| at.with_timezone(&Utc))
    };
    InterruptionJob {
        name: text("name").unwrap_or_default(),
        conclusion: text("conclusion").map(|value| value.to_ascii_lowercase()),
        runner_name: text("runner_name"),
        created_at: time("created_at"),
        completed_at: time("completed_at"),
        superseded: false,
    }
}

/// Whether a check-run annotation list marks a concurrency supersede.
#[must_use]
pub fn annotations_mark_superseded(annotations: &serde_json::Value) -> bool {
    annotations
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|annotation| {
            annotation
                .get("message")
                .and_then(serde_json::Value::as_str)
        })
        .any(|message| {
            crate::environment_requeue::SUPERSEDED_ANNOTATIONS
                .iter()
                .any(|text| message.contains(text))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minute: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-11T00:00:00Z")
            .expect("time")
            .with_timezone(&Utc)
            + chrono::Duration::minutes(minute)
    }

    fn job(conclusion: &str, runner: Option<&str>, waited: i64) -> InterruptionJob {
        InterruptionJob {
            name: "macos".to_owned(),
            conclusion: Some(conclusion.to_owned()),
            runner_name: runner.map(str::to_owned),
            created_at: Some(at(0)),
            completed_at: Some(at(waited)),
            superseded: false,
        }
    }

    #[test]
    fn a_runnerless_cancel_inside_the_window_is_an_interruption() {
        for waited in [16, 18, 20] {
            let hits = classify_jobs(&[job("cancelled", Some(""), waited)]).expect("hit");
            assert_eq!(hits[0].signature, "runnerless_cancel");
            assert_eq!(hits[0].waited_minutes, waited);
        }
    }

    #[test]
    fn a_runnerless_cancel_outside_the_window_is_not() {
        for waited in [0, 15, 21, 45] {
            assert!(classify_jobs(&[job("cancelled", None, waited)]).is_err());
        }
    }

    #[test]
    fn a_cancel_after_a_runner_took_the_job_is_not() {
        assert!(classify_jobs(&[job("cancelled", Some("m3-vm-1"), 18)]).is_err());
    }

    #[test]
    fn failures_timeouts_and_startup_failures_are_results() {
        for conclusion in ["failure", "timed_out", "startup_failure", "stale"] {
            assert!(classify_jobs(&[job(conclusion, None, 18)]).is_err());
        }
    }

    #[test]
    fn one_genuine_failure_beside_an_interruption_refuses_the_run() {
        let jobs = [
            job("cancelled", None, 18),
            job("failure", Some("m3-vm-1"), 3),
        ];
        assert!(classify_jobs(&jobs).is_err());
    }

    #[test]
    fn a_superseded_or_untimed_job_and_an_empty_run_fail_closed() {
        let mut superseded = job("cancelled", None, 17);
        superseded.superseded = true;
        assert!(classify_jobs(&[superseded]).is_err());
        let mut untimed = job("cancelled", None, 17);
        untimed.completed_at = None;
        assert!(classify_jobs(&[untimed]).is_err());
        assert!(classify_jobs(&[]).is_err());
        assert!(classify_jobs(&[job("success", Some("m3"), 4)]).is_err());
    }

    #[test]
    fn the_budget_is_clamped_to_one() {
        assert_eq!(clamp_rerun_budget(0), 0);
        assert_eq!(clamp_rerun_budget(1), 1);
        assert_eq!(clamp_rerun_budget(7), 1);
        assert_eq!(clamp_rerun_budget(u32::MAX), 1);
    }

    #[test]
    fn supersede_annotations_are_recognized() {
        let marked = serde_json::json!([{"message": "Canceling since a higher priority waiting request for 'x' exists"}]);
        assert!(annotations_mark_superseded(&marked));
        assert!(!annotations_mark_superseded(&serde_json::json!([])));
    }
}
