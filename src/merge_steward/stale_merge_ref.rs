//! A stale merge ref: an approved pull request whose required checks went
//! red on a test merge commit that no longer exists, so they cannot go green
//! on their own.
//!
//! GitHub runs a `pull_request` workflow on `refs/pull/<n>/merge`, the head
//! merged into the base as it stood then. GitHub later rebuilds that ref when
//! the base moves, but nothing reruns the checks: a red measured on the old
//! merge commit stays red. One approved pull request sat red for nine days
//! that way. Age and base movement are not evidence of this (a real failure
//! is often old too); the evidence is the run itself:
//!
//! * every red required context's failing run is a `pull_request` run for
//!   this pull request at its current head ([`run_tested_base`]), and
//! * the base that run was tested on is not the first parent of the pull
//!   request's current test merge commit ([`merge_base_of`]): the red was
//!   measured on a merge commit that no longer exists
//!   ([`measured_on_old_merge`]);
//! * and every one of those contexts is green on the base tip
//!   ([`all_green_on_tip`]), so the same checks pass on the base.
//!
//! A red on the current merge ref is never stale, however old. The fix is a
//! close and reopen, which reruns the checks on the current ref; `shipyard
//! advance --retrigger` does that after checking its own policy, and the
//! steward's dwell pass calls it ([`super::dwell::ActKind::RetriggerStaleRef`]).
//!
//! Pure: no reads, no clock.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{RequiredCheck, StewardCheck, selected_required_check};

/// The `blocked_on` token kind.
pub const TOKEN: &str = "stale_merge_ref";
/// How long a "not stale" verdict is reused before the merge ref is read
/// again (GitHub may rebuild it in between).
pub const NOT_STALE_RECHECK_MINUTES: i64 = 6 * 60;
/// Most pull requests whose merge ref one pass reads fresh, per repository;
/// the rest wait for the next pass.
pub const MAX_FRESH_PER_PASS: usize = 5;

/// The red required contexts a token names, when the token is one a stale
/// merge ref can explain: `required_failed:<contexts>`.
#[must_use]
pub fn candidate_contexts(token: &str) -> Option<&str> {
    let contexts = token.strip_prefix("required_failed:")?;
    (!contexts.is_empty()).then_some(contexts)
}

/// `stale_merge_ref:<contexts>`.
#[must_use]
pub fn token(contexts: &str) -> String {
    format!("{TOKEN}:{contexts}")
}

/// The Actions run behind each red context in `contexts` (comma-joined), as
/// the required rule selects it at this head. `None` when any context has
/// no required rule, no completed non-green observation, or no Actions run:
/// then nothing can show which merge commit it was measured on.
#[must_use]
pub fn red_runs(
    contexts: &str,
    checks: &[StewardCheck],
    required: &[RequiredCheck],
) -> Option<Vec<u64>> {
    let mut runs = Vec::new();
    for context in contexts.split(',').map(str::trim) {
        if context.is_empty() {
            return None;
        }
        let rule = required
            .iter()
            .find(|rule| rule.context.eq_ignore_ascii_case(context))?;
        let check = selected_required_check(checks, rule)?;
        let green = check.conclusion.as_deref().is_some_and(|conclusion| {
            matches!(
                conclusion.to_ascii_uppercase().as_str(),
                "SUCCESS" | "NEUTRAL" | "SKIPPED"
            )
        });
        if !check.status.eq_ignore_ascii_case("COMPLETED") || green {
            return None;
        }
        runs.push(check.run_id?);
    }
    (!runs.is_empty()).then_some(runs)
}

/// The base commit the pull request's current test merge commit was built
/// on: its first parent, when it has exactly two and the second is `head`.
#[must_use]
pub fn merge_base_of(merge_commit: &Value, head: &str) -> Option<String> {
    let parents = merge_commit.get("parents")?.as_array()?;
    let sha = |index: usize| {
        parents
            .get(index)?
            .get("sha")?
            .as_str()
            .map(str::to_ascii_lowercase)
    };
    (parents.len() == 2 && sha(1)?.eq_ignore_ascii_case(head)).then(|| sha(0))?
}

/// The base a workflow run was tested on: for a `pull_request` run of pull
/// request `number` at `head`, its recorded base sha. `None` for any other
/// run (a push, a merge group, another pull request or head): it says
/// nothing about this pull request's merge ref.
#[must_use]
pub fn run_tested_base(run: &Value, number: u64, head: &str) -> Option<String> {
    if run.get("event").and_then(Value::as_str) != Some("pull_request") {
        return None;
    }
    run.get("pull_requests")?
        .as_array()?
        .iter()
        .find(|pr| {
            pr.get("number").and_then(Value::as_u64) == Some(number)
                && pr
                    .pointer("/head/sha")
                    .and_then(Value::as_str)
                    .is_some_and(|sha| sha.eq_ignore_ascii_case(head))
        })?
        .pointer("/base/sha")?
        .as_str()
        .map(str::to_ascii_lowercase)
}

/// Whether every red run was measured on a merge commit that no longer
/// exists: each one's tested base is known and differs from the current
/// merge ref's base. One unknown, or one on the current ref, is not stale.
#[must_use]
pub fn measured_on_old_merge(current_base: &str, tested: &[Option<String>]) -> bool {
    !tested.is_empty()
        && tested.iter().all(|base| {
            base.as_deref()
                .is_some_and(|base| !base.eq_ignore_ascii_case(current_base))
        })
}

/// Whether every context in `contexts` (comma-joined, as a token carries
/// them) reads green on the base tip, by `tip_green`. A context it could
/// not read, or one with no completed run on the tip, is not green.
#[must_use]
pub fn all_green_on_tip(
    contexts: &str,
    tip_green: &mut dyn FnMut(&str) -> Result<bool, String>,
) -> bool {
    contexts
        .split(',')
        .map(str::trim)
        .filter(|context| !context.is_empty())
        .all(|context| tip_green(context).unwrap_or(false))
        && contexts
            .split(',')
            .any(|context| !context.trim().is_empty())
}

/// One pull request's last merge-ref verdict, kept in the steward ledger so
/// the same red is not read every pass.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StaleMemo {
    /// The head judged.
    pub head: String,
    /// The red runs judged.
    pub runs: Vec<u64>,
    /// Whether they were measured on a merge commit that no longer exists.
    pub stale: bool,
    /// When.
    pub at: chrono::DateTime<chrono::Utc>,
}

impl StaleMemo {
    /// The verdict, when it still answers for `head` and `runs` at `now`: a
    /// stale verdict holds until the head or the runs change; a "not stale"
    /// one for [`NOT_STALE_RECHECK_MINUTES`].
    #[must_use]
    pub fn reuse(
        &self,
        head: &str,
        runs: &[u64],
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<bool> {
        (self.head.eq_ignore_ascii_case(head)
            && self.runs == runs
            && (self.stale || now - self.at < chrono::Duration::minutes(NOT_STALE_RECHECK_MINUTES)))
        .then_some(self.stale)
    }
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
        // Success needs the envelope that says what happened: an exit 0 with
        // no readable answer is unknown, and an unknown act is never retried.
        Some(0) => field("/outcome").map_or_else(
            || {
                RetriggerResult::Failed(format!(
                    "advance exited 0 without a readable outcome: {}",
                    stdout.trim()
                ))
            },
            RetriggerResult::Done,
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
