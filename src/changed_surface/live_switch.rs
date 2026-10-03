//! The kill switch for live test reuse, and the trip that turns it off.
//!
//! A plan that would skip building and testing an executable because its
//! source key equals the planned base's only does so while a repository
//! variable named by the protected-base configuration reads exactly `live`.
//! Every other state is shadow: the plan is computed and recorded, and
//! nothing is skipped. That covers `off`, an unset variable, any other
//! value, a variable that could not be read, and a read older than the
//! freshness bound. The default is therefore safe without any setup.
//!
//! Tripping sets the variable to `off` from the host that observed the
//! problem (never from a pull request's workflow) and opens one tracking
//! issue, or adds the new reason to it. Repeating a trip with a reason the
//! issue already records changes nothing.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::marked_issue::{self, MarkedIssue};
use crate::repo_variable::{self, WriteOutcome};

/// The one value that lets a plan execute its skips.
pub const LIVE: &str = "live";
/// The value a trip writes.
pub const OFF: &str = "off";
/// Marker prefix of the tracking issue a trip opens.
pub const TRIP_MARKER_PREFIX: &str = "<!-- shipyard-reuse-trip: ";

/// Whether a plan may execute its skips.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchMode {
    /// Skips execute.
    Live,
    /// The plan is recorded and nothing is skipped.
    Shadow,
}

/// One read of the switch, as recorded in a plan's receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SwitchReading {
    /// The repository variable read.
    pub variable: String,
    /// Its value, `None` when unset or unreadable.
    pub value: Option<String>,
    /// When it was read.
    pub read_at: DateTime<Utc>,
    /// What the value alone permits.
    pub mode: SwitchMode,
    /// Why, in words.
    pub reason: String,
}

impl SwitchReading {
    /// Classify the outcome of reading `variable` (`Ok(None)`: unset).
    #[must_use]
    pub fn classify(
        variable: &str,
        read: Result<Option<String>, String>,
        read_at: DateTime<Utc>,
    ) -> Self {
        let (value, mode, reason) = match read {
            Ok(Some(value)) if value.trim() == LIVE => (
                Some(value),
                SwitchMode::Live,
                "the variable reads `live`".to_owned(),
            ),
            Ok(Some(value)) if value.trim() == OFF => (
                Some(value),
                SwitchMode::Shadow,
                "the variable reads `off`".to_owned(),
            ),
            Ok(Some(value)) => {
                let reason = format!("the variable reads {value:?}; only `live` executes skips");
                (Some(value), SwitchMode::Shadow, reason)
            }
            Ok(None) => (None, SwitchMode::Shadow, "the variable is unset".to_owned()),
            Err(error) => (
                None,
                SwitchMode::Shadow,
                format!("the variable could not be read: {error}"),
            ),
        };
        Self {
            variable: variable.to_owned(),
            value,
            read_at,
            mode,
            reason,
        }
    }

    /// The mode a plan made at `now` may act on: `Live` only when the value
    /// permits it and the read is no older than `max_age` (a read stamped in
    /// the future is stale too, since its age cannot be trusted).
    #[must_use]
    pub fn effective_mode(&self, now: DateTime<Utc>, max_age: Duration) -> (SwitchMode, String) {
        if self.mode != SwitchMode::Live {
            return (SwitchMode::Shadow, self.reason.clone());
        }
        let age = now - self.read_at;
        if age < Duration::zero() || age > max_age {
            return (
                SwitchMode::Shadow,
                format!(
                    "the read from {} is stale at {} (bound {}s)",
                    self.read_at.to_rfc3339_opts(SecondsFormat::Secs, true),
                    now.to_rfc3339_opts(SecondsFormat::Secs, true),
                    max_age.num_seconds()
                ),
            );
        }
        (SwitchMode::Live, self.reason.clone())
    }
}

/// Read the switch now.
pub fn read_switch<F>(gh: &F, repo: &str, variable: &str, now: DateTime<Utc>) -> SwitchReading
where
    F: Fn(&[String]) -> Result<String, String> + ?Sized,
{
    SwitchReading::classify(variable, repo_variable::read(gh, repo, variable), now)
}

/// The marker a trip issue for `repo`'s `variable` carries.
#[must_use]
pub fn trip_marker(repo: &str, variable: &str) -> String {
    format!("{TRIP_MARKER_PREFIX}{repo}:{variable} -->")
}

/// What to do with the tracking issue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IssueAction {
    /// Open a new tracking issue.
    Open {
        /// Issue title.
        title: String,
        /// Issue body, marker included.
        body: String,
    },
    /// Rewrite an existing tracking issue's body.
    Update {
        /// The issue.
        number: u64,
        /// The new body.
        body: String,
    },
    /// The issue already records this reason.
    Nothing {
        /// The issue that already records it.
        number: u64,
    },
}

/// The two actions of one trip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TripPlan {
    /// Write `off` to the variable (false only when it already reads `off`).
    pub set_off: bool,
    /// The tracking issue's action.
    pub issue: IssueAction,
}

/// Plan a trip of `repo`'s `variable` for `reason`.
///
/// `current` is the variable's read; anything but a clean `off` writes `off`,
/// so an unreadable variable is still turned off. `open_issues` are the
/// repository's open issues carrying [`TRIP_MARKER_PREFIX`]; the one for this
/// variable is reused.
#[must_use]
pub fn plan_trip(
    repo: &str,
    variable: &str,
    current: &Result<Option<String>, String>,
    open_issues: &[MarkedIssue],
    reason: &str,
    at: DateTime<Utc>,
) -> TripPlan {
    let set_off = !matches!(current, Ok(Some(value)) if value.trim() == OFF);
    let reason = reason.trim();
    let line = format!(
        "- {}: {reason}",
        at.to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let key = format!("{repo}:{variable}");
    let issue = match open_issues.iter().find(|issue| issue.key == key) {
        Some(existing)
            if existing
                .body
                .lines()
                .any(|l| l.ends_with(&format!(": {reason}"))) =>
        {
            IssueAction::Nothing {
                number: existing.number,
            }
        }
        Some(existing) => IssueAction::Update {
            number: existing.number,
            body: format!("{}\n{line}", existing.body.trim_end()),
        },
        None => IssueAction::Open {
            title: format!("Live test reuse tripped: {variable} set to {OFF}"),
            body: format!(
                "{marker}\n\
                 Shipyard set `{variable}` to `{OFF}`, so changed-surface plans run \
                 in shadow and skip nothing until someone sets it back to `{LIVE}`.\n\n\
                 Before re-enabling, find the cause of each trip below and confirm it \
                 is fixed; a sampled re-run failure means a skip would have hidden a \
                 failing test.\n\n\
                 Trips:\n{line}",
                marker = trip_marker(repo, variable),
            ),
        },
    };
    TripPlan { set_off, issue }
}

/// What a trip did, or would do.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TripOutcome {
    /// The variable's action.
    pub variable: String,
    /// The tracking issue's action.
    pub issue: String,
    /// Whether anything was sent.
    pub applied: bool,
}

/// Carry out a trip: read the variable and the open trip issues, plan, and
/// with `apply` write. The variable is written first, since turning live
/// reuse off is the action that protects; the issue follows even when that
/// write fails, so the failure is still reported somewhere a person reads.
///
/// # Errors
///
/// The `gh` message when the issue list cannot be read (the trip would
/// otherwise risk a duplicate issue) or when an applied write fails. A failed
/// variable write is reported after the issue has been attempted.
pub fn trip<F>(
    gh: &F,
    repo: &str,
    variable: &str,
    reason: &str,
    now: DateTime<Utc>,
    apply: bool,
) -> Result<TripOutcome, String>
where
    F: Fn(&[String]) -> Result<String, String> + ?Sized,
{
    let current = repo_variable::read(gh, repo, variable);
    let raw = gh(&marked_issue::list_open_args(repo)).map_err(|error| {
        format!("cannot read the open issues, so a trip could duplicate one: {error}")
    })?;
    let issues = marked_issue::parse(&raw, TRIP_MARKER_PREFIX);
    let plan = plan_trip(repo, variable, &current, &issues, reason, now);
    if !apply {
        return Ok(TripOutcome {
            variable: if plan.set_off {
                format!("would set {variable}={OFF}")
            } else {
                format!("{variable} already {OFF}")
            },
            issue: describe(&plan.issue, false),
            applied: false,
        });
    }
    let variable_result = if plan.set_off {
        repo_variable::write(gh, repo, variable, OFF).map(|outcome| match outcome {
            WriteOutcome::Updated => format!("set {variable}={OFF}"),
            WriteOutcome::Created => format!("created {variable}={OFF}"),
        })
    } else {
        Ok(format!("{variable} already {OFF}"))
    };
    let issue_result = apply_issue(gh, repo, &plan.issue);
    match (variable_result, issue_result) {
        (Ok(variable), Ok(issue)) => Ok(TripOutcome {
            variable,
            issue,
            applied: true,
        }),
        (Err(error), issue) => Err(format!(
            "could not set {variable}={OFF}: {error} (issue: {})",
            issue.unwrap_or_else(|issue_error| format!("also failed: {issue_error}"))
        )),
        (Ok(_), Err(error)) => Err(format!(
            "{variable} is {OFF}, but the tracking issue failed: {error}"
        )),
    }
}

fn describe(action: &IssueAction, applied: bool) -> String {
    let verb = |done: &str, would: &str| {
        if applied {
            done.to_owned()
        } else {
            would.to_owned()
        }
    };
    match action {
        IssueAction::Open { title, .. } => format!("{} {title:?}", verb("opened", "would open")),
        IssueAction::Update { number, .. } => format!(
            "{} #{number}",
            verb("added the reason to", "would add the reason to")
        ),
        IssueAction::Nothing { number } => format!("#{number} already records this reason"),
    }
}

fn apply_issue<F>(gh: &F, repo: &str, action: &IssueAction) -> Result<String, String>
where
    F: Fn(&[String]) -> Result<String, String> + ?Sized,
{
    let field = |name: &str, value: &str| ["-f".to_owned(), format!("{name}={value}")];
    match action {
        IssueAction::Open { title, body } => {
            let mut args = vec![
                "api".to_owned(),
                "--method".to_owned(),
                "POST".to_owned(),
                format!("repos/{repo}/issues"),
            ];
            args.extend(field("title", title));
            args.extend(field("body", body));
            gh(&args)?;
        }
        IssueAction::Update { number, body } => {
            let mut args = vec![
                "api".to_owned(),
                "--method".to_owned(),
                "PATCH".to_owned(),
                format!("repos/{repo}/issues/{number}"),
            ];
            args.extend(field("body", body));
            gh(&args)?;
        }
        IssueAction::Nothing { .. } => {}
    }
    Ok(describe(action, true))
}

#[cfg(test)]
mod tests;
