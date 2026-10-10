//! Why a required check is red, in one of five classes. Ordered rules; the
//! first that holds decides.
//!
//! | class | means | acts on |
//! |---|---|---|
//! | `infra` | the runner or a service failed, proven by GitHub's own record or a non-content step | nothing by itself |
//! | `interrupted` | cancelled: `superseded`, `timeout`, or `unknown` | nothing by itself |
//! | `stale_base` | the head is behind the protected base (configured marker) | nothing by itself |
//! | `flake_candidate` | every failing test also failed on other pull requests' heads | never exonerates |
//! | `real` | anything else | the owner fixes it |
//!
//! Two properties are load-bearing:
//!
//! - A cancelled job is never `real` and never `infra` without positive
//!   evidence. A runner-less cancel is starvation OR a supersede, and only the
//!   check run's annotations (GitHub's own words) tell them apart.
//! - `needs_starved` exonerates a failure only when the failing step is the
//!   fail-closed hold itself (a non-content step, or a fail-closed marker in
//!   its output) AND a sibling job is provably starved. A fast real lint
//!   failure beside a coincidentally starved sibling stays `real`.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::evidence::clip;
use super::{Context, Job, Step, seconds};

/// The runner or a service failed.
pub const INFRA: &str = "infra";
/// The job was cancelled.
pub const INTERRUPTED: &str = "interrupted";
/// The head is behind the protected base.
pub const STALE_BASE: &str = "stale_base";
/// Every failing test also fails elsewhere.
pub const FLAKE: &str = "flake_candidate";
/// A code failure.
pub const REAL: &str = "real";

/// Class, the rule that decided it, and the evidence in one line.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Classification {
    /// One of the five classes.
    pub class: String,
    /// The rule: `no_runner`, `superseded`, `timeout`, `unknown`,
    /// `needs_starved`, `lost_runner`, `stale_marker`, `non_content_step`,
    /// `infra_marker`, `failed_on_other_heads`, or `default`.
    pub rule: String,
    /// Why, in one line.
    pub why: String,
}

impl Classification {
    fn new(class: &str, rule: &str, why: impl Into<String>) -> Self {
        Self {
            class: class.to_owned(),
            rule: rule.to_owned(),
            why: why.into(),
        }
    }
}

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("diagnose classify pattern compiles")
}

static NOT_ACQUIRED: LazyLock<Regex> = LazyLock::new(|| pattern(r"was not acquired by Runner"));
static SUPERSEDED: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"Canceling since a higher priority waiting request"));
static TIMEOUT: LazyLock<Regex> = LazyLock::new(|| pattern(r"exceeded the maximum execution time"));
static CANCELED_BY: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"The run was canceled by @?(\S+?)\.?$"));

/// A failing step whose action the tree does not ordinarily cause. A hint,
/// never a certainty: a head can rewrite the workflow that runs it.
static NON_CONTENT_STEPS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)^set\s*up\s+job$",
        r"(?i)^complete\s+job$",
        r"(?i)^post\s+",
        r"(?i)^checkout\b",
        r"(?i)^run actions/checkout\b",
        r"(?i)^(?:upload|download)\b",
        r"(?i)^(?:restore|save)\b.*\bcache\b",
        r"(?i)^install\b.*\bdependenc",
        r"(?i)^install\s+ccache\b",
        r"(?i)^free\s+disk\s+space\b",
        r"(?i)^set\s*up\b.*\b(?:sdk|toolchain|python|node)\b",
    ]
    .into_iter()
    .map(pattern)
    .collect()
});

/// Transport and service failures; applied only when no test named the failure.
static INFRA_MARKERS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"HTTP Error 5\d\d",
        r"\b50[234] (?:Bad Gateway|Service Unavailable|Gateway Time-?out)",
        r"Connection reset by peer",
        r"Could not resolve host",
        r"Temporary failure in name resolution",
        r"The runner has received a shutdown signal",
        r"lost communication with the server",
        r"No space left on device",
        r"An error occurred trying to start process",
        r"Failed to CreateArtifact: .*job is completed",
        // An artifact upload that stalls after the suite passed. The step
        // window keeps it honest: a red test step is the failing step, so
        // a stall printed by a later upload never reaches this check.
        r"Upload progress stalled",
    ]
    .into_iter()
    .map(pattern)
    .collect()
});

/// The default fail-closed marker: "failing <gate> closed".
pub const DEFAULT_FAIL_CLOSED: &str = r"\bfail(?:ing|ed)?\b.{0,40}\bclosed\b";

/// [`DEFAULT_FAIL_CLOSED`], compiled.
#[must_use]
pub fn default_fail_closed() -> Vec<Regex> {
    vec![pattern(DEFAULT_FAIL_CLOSED)]
}

/// `(rule, why)` from a cancelled job's check-run annotations, or `None` when
/// they say nothing about the cause.
#[must_use]
pub fn cancel_cause(messages: &[String]) -> Option<(&'static str, String)> {
    for (rx, rule) in [
        (&*NOT_ACQUIRED, "no_runner"),
        (&*SUPERSEDED, "superseded"),
        (&*TIMEOUT, "timeout"),
    ] {
        if let Some(hit) = messages.iter().find(|message| rx.is_match(message)) {
            return Some((rule, short(hit)));
        }
    }
    messages
        .iter()
        .find(|message| CANCELED_BY.is_match(message.trim()))
        .map(|hit| ("unknown", short(hit)))
}

fn short(text: &str) -> String {
    clip(text.trim()).chars().take(160).collect()
}

fn non_content(step: &str) -> bool {
    NON_CONTENT_STEPS.iter().any(|rx| rx.is_match(step))
}

/// A cancelled job: `infra` only on GitHub's own "not acquired" record, else
/// `interrupted` with the recorded cause. Never `real`, never exonerated
/// without evidence.
fn cancelled(job: &Job, step: Option<&Step>, messages: &[String]) -> Classification {
    match cancel_cause(messages) {
        Some(("no_runner", why)) => Classification::new(INFRA, "no_runner", why),
        Some((rule, why)) => Classification::new(INTERRUPTED, rule, why),
        None => {
            let place = match step {
                Some(step) => format!("cancelled while running '{}'", step.name),
                None if job.steps().is_empty() => "cancelled before any step".to_owned(),
                None => "cancelled".to_owned(),
            };
            Classification::new(
                INTERRUPTED,
                "unknown",
                format!("{place}; no cause recorded"),
            )
        }
    }
}

/// A failure that is the fail-closed hold on a provably starved sibling: the
/// failing step is a non-content step or prints a fail-closed marker, AND a
/// sibling in the same run carries GitHub's "not acquired" record. Timing is
/// reported, never a condition.
fn needs_starved(
    job: &Job,
    siblings: &[Job],
    step_name: &str,
    window: &[String],
    context: &Context,
) -> Option<Classification> {
    if job.conclusion.as_deref() != Some("failure") {
        return None;
    }
    let notes = |id: i64| context.annotations.get(&id).map_or(&[][..], Vec::as_slice);
    let starved: Vec<&str> = siblings
        .iter()
        .filter(|sibling| {
            sibling.id != job.id
                && sibling.run_id == job.run_id
                && sibling.conclusion.as_deref() == Some("cancelled")
        })
        .filter(|sibling| matches!(cancel_cause(notes(sibling.id)), Some(("no_runner", _))))
        .map(|sibling| sibling.name.as_str())
        .take(3)
        .collect();
    if starved.is_empty() {
        return None;
    }
    let marker = window
        .iter()
        .find(|line| context.fail_closed.iter().any(|rx| rx.is_match(line)));
    if marker.is_none() && !non_content(step_name) {
        return None;
    }
    let ran = seconds(job.started_at.as_deref(), job.completed_at.as_deref())
        .map_or_else(|| "None".to_owned(), |value| value.to_string());
    let cause = marker.map_or_else(
        || format!("failed in '{step_name}'"),
        |line| clip(line.trim()).chars().take(100).collect(),
    );
    Some(Classification::new(
        INFRA,
        "needs_starved",
        format!(
            "{} never got a runner; {cause} ({ran}s)",
            starved.join(", ")
        ),
    ))
}

/// Classify one failing required job.
#[must_use]
pub fn classify(
    job: &Job,
    siblings: &[Job],
    step: Option<&Step>,
    tests: &[String],
    window: &[String],
    context: &Context,
) -> Classification {
    let notes = |id: i64| context.annotations.get(&id).map_or(&[][..], Vec::as_slice);
    match job.conclusion.as_deref() {
        Some("cancelled") => return cancelled(job, step, notes(job.id)),
        Some("timed_out") => {
            return Classification::new(INTERRUPTED, "timeout", "job reached its time limit");
        }
        _ => {}
    }
    let step_name = step.map_or("", |step| step.name.as_str());
    if let Some(found) = needs_starved(job, siblings, step_name, window, context) {
        return found;
    }
    if job.steps().is_empty() {
        return Classification::new(
            INFRA,
            "lost_runner",
            "job ended with no recorded steps on a named runner",
        );
    }
    for rx in &context.stale_markers {
        if let Some(hit) = window.iter().find(|line| rx.is_match(line)) {
            return Classification::new(STALE_BASE, "stale_marker", short(hit));
        }
    }
    if non_content(step_name) {
        return Classification::new(
            INFRA,
            "non_content_step",
            format!("failing step '{step_name}'"),
        );
    }
    if tests.is_empty() {
        if let Some(hit) = window
            .iter()
            .find(|line| INFRA_MARKERS.iter().any(|rx| rx.is_match(line)))
        {
            return Classification::new(INFRA, "infra_marker", short(hit));
        }
    } else if tests
        .iter()
        .all(|test| context.history.get(test).is_some_and(|prs| !prs.is_empty()))
    {
        let mut prs: Vec<u64> = tests
            .iter()
            .flat_map(|test| context.history[test].iter().copied())
            .collect();
        prs.sort_unstable();
        prs.dedup();
        let named: Vec<String> = prs.iter().take(5).map(u64::to_string).collect();
        return Classification::new(
            FLAKE,
            "failed_on_other_heads",
            format!(
                "every failing test also failed on other heads: PR {}",
                named.join(", ")
            ),
        );
    }
    Classification::new(
        REAL,
        "default",
        "no infra, stale-base, interruption or flake evidence",
    )
}
