//! A stale merge ref: an approved pull request whose required checks ran
//! against a merge ref GitHub built long ago, so they cannot go green on
//! their own.
//!
//! GitHub runs a pull request's checks on `refs/pull/<n>/merge`, the head
//! merged into the base as it stood when the ref was built. When nothing
//! rebuilds the ref (no push, no reopen), a red required check stays red
//! however far the base moves on: one approved pull request sat red for nine
//! days that way. The fix is a close and reopen, which rebuilds the ref and
//! reruns the checks; `shipyard advance --retrigger` does that after checking
//! its own policy, and the steward's dwell pass calls it
//! ([`super::dwell::ActKind::RetriggerStaleRef`]).
//!
//! The newest required run at the head dates the merge ref: it ran on the
//! ref that existed then. A ref is stale when the base has moved on by more
//! than [`StaleRefPolicy::max_lag_days`] since that run, or has taken at
//! least [`StaleRefPolicy::max_base_commits`] commits since it. Either limit
//! set to 0 turns that criterion off.
//!
//! Pure: no reads, no clock.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use super::{RequiredCheck, StewardCheck, selected_required_check};

/// The `blocked_on` token kind.
pub const TOKEN: &str = "stale_merge_ref";
/// `[auto_merge.stale_merge_ref]` in the base branch's `.shipyard/config.toml`.
pub const CONFIG_TABLE: &str = "stale_merge_ref";
/// Default: days the base may move on past the newest required run.
pub const DEFAULT_MAX_LAG_DAYS: u32 = 3;
/// Default: commits the base may take after the newest required run.
pub const DEFAULT_MAX_BASE_COMMITS: u32 = 30;
/// The most base commits one read lists (one page).
pub const BASE_COMMITS_PAGE: u32 = 100;

/// `[auto_merge.stale_merge_ref]`, as the base config sets it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct StaleRefPolicy {
    /// Days; 0 turns the lag criterion off.
    pub max_lag_days: u32,
    /// Commits; 0 turns the commit-count criterion off.
    pub max_base_commits: u32,
}

impl Default for StaleRefPolicy {
    fn default() -> Self {
        Self {
            max_lag_days: DEFAULT_MAX_LAG_DAYS,
            max_base_commits: DEFAULT_MAX_BASE_COMMITS,
        }
    }
}

impl StaleRefPolicy {
    /// The policy a parsed base config sets. Absent or mistyped keys keep
    /// their defaults.
    #[must_use]
    pub fn from_table(table: &toml::Table) -> Self {
        let mut policy = Self::default();
        let Some(section) = table
            .get(super::ejection_guards::CONFIG_TABLE)
            .and_then(toml::Value::as_table)
            .and_then(|auto| auto.get(CONFIG_TABLE))
            .and_then(toml::Value::as_table)
        else {
            return policy;
        };
        let number = |key: &str| {
            section
                .get(key)
                .and_then(toml::Value::as_integer)
                .and_then(|value| u32::try_from(value).ok())
        };
        if let Some(days) = number("max_lag_days") {
            policy.max_lag_days = days;
        }
        if let Some(commits) = number("max_base_commits") {
            policy.max_base_commits = commits;
        }
        policy
    }

    /// Whether either criterion is on.
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.max_lag_days > 0 || self.max_base_commits > 0
    }

    /// How many base commits one read must list: enough to count to
    /// [`Self::max_base_commits`], within one page, and at least the tip.
    #[must_use]
    pub fn commits_to_read(self) -> u32 {
        self.max_base_commits.clamp(1, BASE_COMMITS_PAGE)
    }
}

/// How far the base moved past the newest required run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Staleness {
    /// The newest required run at the head.
    pub runs_at: DateTime<Utc>,
    /// Whole days from that run to the base's tip commit.
    pub lag_days: i64,
    /// Base commits after that run, counted in the window read (a lower
    /// bound when the window is full).
    pub base_commits_since: usize,
}

/// The red required contexts a token names, when the token is one a stale
/// merge ref can explain: `required_failed:<contexts>` or
/// `required_never_ran:<contexts>`.
#[must_use]
pub fn candidate_contexts(token: &str) -> Option<&str> {
    let (kind, contexts) = token.split_once(':')?;
    (matches!(kind, "required_failed" | "required_never_ran") && !contexts.is_empty())
        .then_some(contexts)
}

/// `stale_merge_ref:<contexts>`.
#[must_use]
pub fn token(contexts: &str) -> String {
    format!("{TOKEN}:{contexts}")
}

/// When the newest required observation at the head was made, or `None`
/// when no required rule has one: then nothing dates the merge ref, and
/// a retrigger would be refused anyway.
#[must_use]
pub fn newest_required_run(
    checks: &[StewardCheck],
    required: &[RequiredCheck],
) -> Option<DateTime<Utc>> {
    required
        .iter()
        .filter_map(|rule| selected_required_check(checks, rule))
        .filter_map(|check| check.observed_at.as_deref())
        .filter_map(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .max()
}

/// Judge one head. `base_commits` are the base's commit times, newest first,
/// as one read listed them; an empty list judges nothing.
#[must_use]
pub fn judge(
    policy: StaleRefPolicy,
    runs_at: DateTime<Utc>,
    base_commits: &[DateTime<Utc>],
) -> Option<Staleness> {
    let tip = *base_commits.first()?;
    let lag = tip - runs_at;
    let since = base_commits.iter().filter(|at| **at > runs_at).count();
    let lagging = policy.max_lag_days > 0 && lag >= Duration::days(i64::from(policy.max_lag_days));
    let moved = policy.max_base_commits > 0
        && u32::try_from(since).unwrap_or(u32::MAX) >= policy.max_base_commits;
    (lagging || moved).then_some(Staleness {
        runs_at,
        lag_days: lag.num_days(),
        base_commits_since: since,
    })
}

/// What one `shipyard advance --retrigger --json` run came to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetriggerResult {
    /// Closed and reopened.
    Done(String),
    /// A guard refused the write, or this build has no `advance` command:
    /// nothing changed, and retrying soon would only be refused again.
    Blocked(String),
    /// `advance`'s own policy refused it; nothing changed.
    Refused(String),
    /// A fact could not be read; nothing changed.
    Deferred(String),
    /// It started and did not finish cleanly, or its answer is unknown:
    /// never retried at this head.
    Failed(String),
}

/// `retrigger_blocked: guard`: the write reached `ghapp` and a guard
/// (pr-close-guard) refused it.
pub const BLOCKED_GUARD: &str = "retrigger_blocked: guard";
/// `retrigger_blocked: advance_unavailable`: this build has no `shipyard
/// advance` command yet.
pub const BLOCKED_UNAVAILABLE: &str = "retrigger_blocked: advance_unavailable";

/// Read one `advance` run: its exit code (`None` when it was killed), its
/// standard output (the JSON envelope) and its standard error.
#[must_use]
pub fn classify_advance(exit: Option<i32>, stdout: &str, stderr: &str) -> RetriggerResult {
    let envelope: Option<serde_json::Value> = serde_json::from_str(stdout.trim()).ok();
    let field = |pointer: &str| {
        envelope
            .as_ref()
            .and_then(|value| value.pointer(pointer))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    match exit {
        Some(0) => RetriggerResult::Done(
            field("/outcome").unwrap_or_else(|| "closed and reopened".to_owned()),
        ),
        Some(1) => match field("/refused/condition") {
            Some(condition) if condition == "close_refused" => RetriggerResult::Blocked(format!(
                "{BLOCKED_GUARD} ({})",
                field("/refused/detail").unwrap_or_default()
            )),
            Some(condition) => RetriggerResult::Refused(format!(
                "retrigger_refused:{condition}: {}",
                field("/refused/detail").unwrap_or_default()
            )),
            None => RetriggerResult::Failed(format!(
                "advance exited 1 without a refusal: {}",
                stderr.trim()
            )),
        },
        Some(2) => match field("/unread") {
            Some(detail) => RetriggerResult::Deferred(format!("retrigger_unread: {detail}")),
            None if envelope.is_none() && stderr.contains("unrecognized subcommand") => {
                RetriggerResult::Blocked(format!("{BLOCKED_UNAVAILABLE} ({})", stderr.trim()))
            }
            // Exit 2 never acts (a usage or config error, or an unread
            // fact), so nothing changed: retry with backoff.
            None => RetriggerResult::Deferred(format!(
                "advance exited 2 without an answer: {}",
                stderr.trim()
            )),
        },
        Some(3) => RetriggerResult::Failed(format!(
            "retrigger_partial: {}",
            field("/outcome").unwrap_or_else(|| stderr.trim().to_owned())
        )),
        other => RetriggerResult::Failed(format!(
            "advance ended {}: {}",
            other.map_or_else(|| "by a signal".to_owned(), |code| format!("with {code}")),
            stderr.trim()
        )),
    }
}

#[cfg(test)]
mod tests;
