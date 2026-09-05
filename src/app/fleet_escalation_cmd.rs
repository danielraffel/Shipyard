//! Carry a fleet escalation off the host: read the open tracking issues, and
//! open, edit or close them.
//!
//! The policy lives in [`crate::fleet_escalation`] and is pure. This is the half
//! that touches GitHub, kept separate so the decision stays testable without a
//! network and so every mutation has exactly one place to audit.
//!
//! ## Why an issue, and why off the host
//!
//! A journal line on a broken machine is not a signal; nobody reads that
//! machine. Worse, a check that runs *on* the failing host cannot speak when the
//! host is the thing that failed. A GitHub issue is the escalation surface that
//! survives the host, and the shape — open on degradation, auto-close on
//! recovery — is already proven in this stack by the release watchdog.
//!
//! ## Dry-run is the default
//!
//! Every entry point here takes an explicit `apply` flag and defaults to
//! reporting what it *would* do. An escalation system that starts opening issues
//! the first time it runs against a fleet nobody has calibrated it on produces a
//! wall of noise, and a wall of noise is how the next real fault gets missed.

use serde_json::Value;

use crate::cloud::{GitHubActions, GitHubError};
use crate::fleet_escalation::{EscalationAction, TrackingIssue, subject_marker};

/// Marker prefix embedded in every body this module writes.
const MARKER_PREFIX: &str = "<!-- shipyard-fleet-subject: ";

/// What happened when an action was carried out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedEscalation {
    /// Which action kind was handled.
    pub kind: &'static str,
    /// The subject key or issue number the action addressed.
    pub target: String,
    /// Whether the mutation was actually performed.
    pub applied: bool,
    /// Human-readable outcome.
    pub detail: String,
}

/// Read the open tracking issues this module owns.
///
/// Matched by the embedded marker rather than by title: a human may retitle an
/// issue, and losing track of one would open a duplicate beside it. Issues
/// without a marker belong to somebody else and are ignored entirely — this
/// module must never edit a report it did not write.
///
/// # Errors
///
/// Returns the underlying [`GitHubError`] when the issue list cannot be read.
/// The caller must treat that as `Unknown`, never as "nothing is open" — an
/// unreadable list and an empty one produce the same empty vector, and acting on
/// the second reading when the first is true opens duplicates.
pub fn fetch_tracking_issues(
    actions: &GitHubActions,
    repo: &str,
) -> Result<Vec<TrackingIssue>, GitHubError> {
    let raw = actions.run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        format!("repos/{repo}/issues?state=open&per_page=100"),
        "--jq".to_owned(),
        ".[]".to_owned(),
    ])?;

    let mut issues = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // The issues endpoint returns pull requests too. A PR carrying our
        // marker would be a very odd thing to close as "recovered".
        if value.get("pull_request").is_some() {
            continue;
        }
        let body = value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(key) = parse_marker(body) else {
            continue;
        };
        let Some(number) = value.get("number").and_then(Value::as_u64) else {
            continue;
        };
        issues.push(TrackingIssue {
            number,
            key,
            body: body.to_owned(),
        });
    }
    Ok(issues)
}

/// Extract the subject key from a body's marker, if it carries one.
fn parse_marker(body: &str) -> Option<String> {
    let start = body.find(MARKER_PREFIX)? + MARKER_PREFIX.len();
    let rest = &body[start..];
    let end = rest.find("-->")?;
    let key = rest[..end].trim();
    if key.is_empty() {
        return None;
    }
    Some(key.to_owned())
}

/// Carry out one escalation action.
///
/// With `apply` false nothing is sent; the returned record describes what would
/// have happened. Every mutation goes through `gh api` with an explicit method,
/// so the audit trail is the argv.
///
/// # Errors
///
/// Returns the underlying [`GitHubError`] when a mutation is attempted and the
/// API call fails.
pub fn apply_escalation(
    actions: &GitHubActions,
    repo: &str,
    action: &EscalationAction,
    apply: bool,
) -> Result<AppliedEscalation, GitHubError> {
    match action {
        EscalationAction::Nothing { reason } => Ok(AppliedEscalation {
            kind: "nothing",
            target: String::new(),
            applied: false,
            detail: reason.clone(),
        }),
        EscalationAction::Open { key, title, body } => {
            if !apply {
                return Ok(AppliedEscalation {
                    kind: "open",
                    target: key.clone(),
                    applied: false,
                    detail: format!("would open a tracking issue titled {title:?}"),
                });
            }
            let raw = actions.run_gh(&[
                "api".to_owned(),
                "--method".to_owned(),
                "POST".to_owned(),
                format!("repos/{repo}/issues"),
                "-f".to_owned(),
                format!("title={title}"),
                "-f".to_owned(),
                format!("body={body}"),
            ])?;
            let number = serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|value| value.get("number").and_then(Value::as_u64));
            Ok(AppliedEscalation {
                kind: "open",
                target: key.clone(),
                applied: true,
                detail: match number {
                    Some(number) => format!("opened #{number}"),
                    None => "opened (issue number not returned)".to_owned(),
                },
            })
        }
        EscalationAction::Update { number, body } => {
            if !apply {
                return Ok(AppliedEscalation {
                    kind: "update",
                    target: number.to_string(),
                    applied: false,
                    detail: format!("would edit #{number} in place"),
                });
            }
            actions.run_gh(&[
                "api".to_owned(),
                "--method".to_owned(),
                "PATCH".to_owned(),
                format!("repos/{repo}/issues/{number}"),
                "-f".to_owned(),
                format!("body={body}"),
            ])?;
            Ok(AppliedEscalation {
                kind: "update",
                target: number.to_string(),
                applied: true,
                detail: format!("edited #{number}"),
            })
        }
        EscalationAction::Close { number, comment } => {
            if !apply {
                return Ok(AppliedEscalation {
                    kind: "close",
                    target: number.to_string(),
                    applied: false,
                    detail: format!("would comment on and close #{number}"),
                });
            }
            // Comment first, then close. If the close fails the reader still
            // has the recovery evidence; if the order were reversed a failure
            // would leave a silently closed issue with no explanation.
            actions.run_gh(&[
                "api".to_owned(),
                "--method".to_owned(),
                "POST".to_owned(),
                format!("repos/{repo}/issues/{number}/comments"),
                "-f".to_owned(),
                format!("body={comment}"),
            ])?;
            actions.run_gh(&[
                "api".to_owned(),
                "--method".to_owned(),
                "PATCH".to_owned(),
                format!("repos/{repo}/issues/{number}"),
                "-f".to_owned(),
                "state=closed".to_owned(),
            ])?;
            Ok(AppliedEscalation {
                kind: "close",
                target: number.to_string(),
                applied: true,
                detail: format!("closed #{number} with a recovery comment"),
            })
        }
    }
}

/// Carry out a batch, stopping at the first failure.
///
/// Stopping rather than continuing is deliberate: the actions were decided
/// against one snapshot of the open issues, and a failed mutation means that
/// snapshot is no longer trustworthy. Pressing on risks opening a duplicate of
/// something the failed call already created.
///
/// # Errors
///
/// Returns the first [`GitHubError`] encountered, along with the records for the
/// actions already carried out.
#[must_use]
pub fn apply_all(
    actions: &GitHubActions,
    repo: &str,
    decisions: &[EscalationAction],
    apply: bool,
) -> (Vec<AppliedEscalation>, Option<GitHubError>) {
    let mut applied = Vec::new();
    for action in decisions {
        match apply_escalation(actions, repo, action, apply) {
            Ok(record) => applied.push(record),
            Err(error) => return (applied, Some(error)),
        }
    }
    (applied, None)
}

/// Render the marker a body must carry for this module to recognise it later.
#[must_use]
pub fn marker_for(key: &str) -> String {
    subject_marker(key)
}

// Gated on unix as a whole rather than per-item. Every test here drives a
// fake `gh` that is a `#!/bin/sh` script, so none of them can run on Windows —
// and gating them individually left helpers and imports behind as dead code
// that only the skipped platform could see. One gate cannot be applied
// inconsistently.
#[cfg(all(test, unix))]
mod tests;
