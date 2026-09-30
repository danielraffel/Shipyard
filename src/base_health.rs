//! Base health: read a repository's `base-poison-signal/v1` and say what to do.
//!
//! When the default branch itself is red, every merge-queue batch re-formed on
//! top of it inherits the failure and is ejected, and each ejection costs a
//! full gate. The only move that breaks the cycle is to put the pull request
//! that fixes the base at the front of the queue. A repository's own detector
//! decides whether the base is poisoned and which open pull request owns the
//! failing test; this module reads that decision, never re-derives it.
//!
//! The signal is a GitHub annotation titled [`SIGNAL_TITLE`] whose message is
//! a JSON object with `schema` = [`SIGNAL_SCHEMA`], published by the latest
//! completed run of the detector workflow. Only `status == "poisoned"` with a
//! named `candidate_fix_pr` produces jump advice, and only while the signal is
//! younger than [`MAX_SIGNAL_AGE`]: an old reading describes a base that has
//! probably moved.
//!
//! Acting on the advice is a separate, opt-in decision ([`AutoJump`]), which
//! defaults to [`AutoJump::Off`].

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::validation_signals::{Annotation, GhReader, parse_annotations};

pub mod tip;

/// Annotation title the detector publishes.
pub const SIGNAL_TITLE: &str = "base-poison-signal";
/// Schema identifier inside the annotation message.
pub const SIGNAL_SCHEMA: &str = "base-poison-signal/v1";
/// Detector workflow read when no other is configured.
pub const DEFAULT_WORKFLOW: &str = "main-health-detector.yml";
/// Config key naming the detector workflow.
pub const WORKFLOW_CONFIG_KEY: &str = "base_health.workflow";
/// Config key selecting [`AutoJump`].
pub const AUTO_JUMP_CONFIG_KEY: &str = "base_health.auto_jump";
/// Status the detector reports for a base whose own suite fails.
pub const STATUS_POISONED: &str = "poisoned";
/// Oldest signal that still produces jump advice.
pub const MAX_SIGNAL_AGE: Duration = Duration::hours(2);
/// Completed detector runs examined, newest first, for one with a signal.
const RUN_SAMPLE: u32 = 5;

/// The detector's record, as published.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct BaseSignal {
    /// `healthy`, `unproven`, `suspected` or `poisoned`.
    pub status: String,
    /// Whether the detector observed the failure on the base itself.
    #[serde(default)]
    pub safe_to_pause_queue: bool,
    /// Failing tests the verdict is about.
    #[serde(default)]
    pub tests: Vec<String>,
    /// The open pull request that decisively owns the failing test, if any.
    #[serde(default)]
    pub candidate_fix_pr: Option<u64>,
    /// Run on the base whose suite the verdict read.
    #[serde(default)]
    pub main_run_id: Option<String>,
    /// Detector's own explanation.
    #[serde(default)]
    pub reason: String,
}

/// One signal and where it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SignalObservation {
    /// The published record.
    pub signal: BaseSignal,
    /// Detector run that published it.
    pub run_id: u64,
    /// When that run finished.
    pub observed_at: DateTime<Utc>,
    /// Link to the run.
    pub run_url: Option<String>,
}

/// What reading the signal produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BaseHealthFinding {
    /// A signal was found.
    Signal(SignalObservation),
    /// The detector is absent, or none of its recent runs published a signal.
    NoSignal {
        /// Why there is nothing to read.
        detail: String,
    },
    /// The detector could not be read.
    Unreadable {
        /// What failed.
        detail: String,
    },
}

/// Advice to put the named fix pull request at the front of the queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JumpAdvice {
    /// Pull request to jump.
    pub pr: u64,
    /// Failing tests on the base.
    pub tests: Vec<String>,
    /// Detector run the advice came from.
    pub signal_run_id: u64,
    /// Base run whose suite failed, as the detector named it.
    pub main_run_id: Option<String>,
    /// One line for a person.
    pub message: String,
    /// Shell commands that perform the jump by hand, in order.
    pub commands: Vec<String>,
}

/// Whether Shipyard acts on jump advice by itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoJump {
    /// Advice is printed; nothing is recorded or changed.
    #[default]
    Off,
    /// Each decision is recorded as "would jump PR #n"; the queue is untouched.
    DryRun,
    /// The fix pull request is dequeued and re-enqueued with `jump: true`.
    On,
}

impl AutoJump {
    /// Parse the config value. Anything unrecognised is an error rather than
    /// a silent default, because a typo must never switch the queue on.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim) {
            None | Some("off" | "") => Ok(Self::Off),
            Some("dry-run" | "dry_run") => Ok(Self::DryRun),
            Some("on") => Ok(Self::On),
            Some(other) => Err(format!(
                "{AUTO_JUMP_CONFIG_KEY} = {other:?} is not one of off, dry-run, on"
            )),
        }
    }
}

/// The last well-formed signal among `annotations`, if any.
#[must_use]
pub fn parse_signal(annotations: &[Annotation]) -> Option<Result<BaseSignal, String>> {
    annotations
        .iter()
        .rev()
        .find(|annotation| annotation.title == SIGNAL_TITLE)
        .map(|annotation| {
            let value: Value = serde_json::from_str(&annotation.message)
                .map_err(|error| format!("{SIGNAL_TITLE} message is not JSON: {error}"))?;
            let schema = value.get("schema").and_then(Value::as_str).unwrap_or("");
            if schema != SIGNAL_SCHEMA {
                return Err(format!(
                    "{SIGNAL_TITLE} carries schema {schema:?}, expected {SIGNAL_SCHEMA:?}"
                ));
            }
            serde_json::from_value(value)
                .map_err(|error| format!("{SIGNAL_TITLE} is malformed: {error}"))
        })
}

fn read_json(gh: &GhReader<'_>, path: &str) -> Result<Value, String> {
    let raw = gh(&["api".to_owned(), path.to_owned()])?;
    serde_json::from_str(&raw).map_err(|error| format!("malformed JSON from `{path}`: {error}"))
}

fn is_not_found(error: &str) -> bool {
    error.to_ascii_lowercase().contains("http 404")
}

/// Read the newest signal the detector workflow published.
///
/// Runs are read newest first; a run that concluded other than `success` or
/// `failure` (cancelled, skipped) is not evidence and is passed over. The
/// detector may fail its own run on purpose when it finds a poisoned base, so
/// `failure` is read like `success`.
#[must_use]
pub fn read_latest(gh: &GhReader<'_>, repo: &str, workflow: &str) -> BaseHealthFinding {
    let path = format!(
        "repos/{repo}/actions/workflows/{workflow}/runs?status=completed&per_page={RUN_SAMPLE}"
    );
    let runs = match read_json(gh, &path) {
        Ok(value) => value,
        Err(error) if is_not_found(&error) => {
            return BaseHealthFinding::NoSignal {
                detail: format!("{repo} has no `{workflow}` workflow"),
            };
        }
        Err(error) => return BaseHealthFinding::Unreadable { detail: error },
    };
    let runs = runs
        .get("workflow_runs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for run in &runs {
        let conclusion = run.get("conclusion").and_then(Value::as_str).unwrap_or("");
        if !matches!(conclusion, "success" | "failure") {
            continue;
        }
        let Some(run_id) = run.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let observed_at = run
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.with_timezone(&Utc));
        let jobs = match read_json(gh, &format!("repos/{repo}/actions/runs/{run_id}/jobs")) {
            Ok(value) => value,
            Err(error) => return BaseHealthFinding::Unreadable { detail: error },
        };
        for job in jobs
            .get("jobs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(job_id) = job.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let annotations =
                match read_json(gh, &format!("repos/{repo}/check-runs/{job_id}/annotations")) {
                    Ok(value) => parse_annotations(&value),
                    Err(error) => return BaseHealthFinding::Unreadable { detail: error },
                };
            match parse_signal(&annotations) {
                Some(Ok(signal)) => {
                    let Some(observed_at) = observed_at else {
                        return BaseHealthFinding::Unreadable {
                            detail: format!("detector run {run_id} carries no updated_at"),
                        };
                    };
                    return BaseHealthFinding::Signal(SignalObservation {
                        signal,
                        run_id,
                        observed_at,
                        run_url: run
                            .get("html_url")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    });
                }
                Some(Err(error)) => return BaseHealthFinding::Unreadable { detail: error },
                None => {}
            }
        }
        // The newest evidence-bearing run is authoritative. Falling back to an
        // older run would report a base the detector has since re-judged.
        return BaseHealthFinding::NoSignal {
            detail: format!("detector run {run_id} published no {SIGNAL_TITLE} annotation"),
        };
    }
    BaseHealthFinding::NoSignal {
        detail: format!("no completed `{workflow}` run in the last {RUN_SAMPLE}"),
    }
}

/// Jump advice, when the base is poisoned, a fix pull request is named and
/// the signal is fresh.
#[must_use]
pub fn jump_advice(
    repo: &str,
    finding: &BaseHealthFinding,
    now: DateTime<Utc>,
) -> Option<JumpAdvice> {
    let BaseHealthFinding::Signal(observation) = finding else {
        return None;
    };
    let signal = &observation.signal;
    if signal.status != STATUS_POISONED || now - observation.observed_at > MAX_SIGNAL_AGE {
        return None;
    }
    let pr = signal.candidate_fix_pr?;
    let tests = if signal.tests.is_empty() {
        "an unnamed test".to_owned()
    } else {
        signal.tests.join(", ")
    };
    Some(JumpAdvice {
        pr,
        tests: signal.tests.clone(),
        signal_run_id: observation.run_id,
        main_run_id: signal.main_run_id.clone(),
        message: format!("main red: {tests}, fix PR #{pr}, jump it"),
        commands: jump_commands(repo, pr),
    })
}

/// GraphQL that removes a pull request from the merge queue.
pub const DEQUEUE_MUTATION: &str =
    "mutation($id:ID!){dequeuePullRequest(input:{id:$id}){mergeQueueEntry{id}}}";
/// GraphQL that enqueues an exact head at the front of the merge queue.
pub const JUMP_MUTATION: &str = "mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head,jump:true}){mergeQueueEntry{position}}}";

/// The by-hand jump. The dequeue fails harmlessly when the pull request is not
/// queued; the enqueue pins the head it read so a later push is not jumped.
#[must_use]
pub fn jump_commands(repo: &str, pr: u64) -> Vec<String> {
    vec![
        format!("id=$(ghapp api repos/{repo}/pulls/{pr} --jq .node_id)"),
        format!("head=$(ghapp api repos/{repo}/pulls/{pr} --jq .head.sha)"),
        format!("ghapp api graphql -f query='{DEQUEUE_MUTATION}' -f id=\"$id\""),
        format!("ghapp api graphql -f query='{JUMP_MUTATION}' -f id=\"$id\" -f head=\"$head\""),
    ]
}

/// One auto-jump decision, as recorded for later scoring.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct JumpDecision {
    /// When the decision was made (RFC 3339).
    pub decided_at: String,
    /// `OWNER/REPO`.
    pub repo: String,
    /// `dry_run` or `on`.
    pub mode: String,
    /// Pull request picked.
    pub pr: u64,
    /// Failing tests on the base.
    pub tests: Vec<String>,
    /// Detector run the pick came from.
    pub signal_run_id: u64,
    /// Base run the detector named; the episode key.
    pub main_run_id: Option<String>,
    /// `would_jump`, `jumped`, `already_first`, `not_open`, or `failed: ...`.
    pub outcome: String,
}

impl JumpDecision {
    /// Two decisions about the same base run and pull request are one episode.
    #[must_use]
    pub fn episode_key(&self) -> (String, Option<String>, u64) {
        (
            self.repo.to_ascii_lowercase(),
            self.main_run_id.clone(),
            self.pr,
        )
    }
}

/// Whether `decision` repeats one already recorded. Repeated ticks over one
/// poisoned base must count as one episode, or the proxy counts ticks.
#[must_use]
pub fn already_recorded(previous: &[JumpDecision], decision: &JumpDecision) -> bool {
    let key = decision.episode_key();
    // A failed live attempt is not a decision made: the next tick may retry.
    previous.iter().any(|earlier| {
        earlier.mode == decision.mode
            && earlier.episode_key() == key
            && !earlier.outcome.starts_with("failed")
    })
}

#[cfg(test)]
mod tests;
