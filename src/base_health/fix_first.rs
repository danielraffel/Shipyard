//! Fix-first while the base is red: confirm the base itself is broken, name
//! the one pull request that repairs it, and score those picks.
//!
//! The merge-group streak ([`super::merge_group`]) says a required context
//! failed in consecutive groups that carried different pull requests. On its
//! own that has been wrong more often than right, so the steward acts on it
//! only when the base tip's own newest verdict for the same context is also
//! a failure ([`judge_tip`]). The tip reading is one `GET` per poisoned
//! context, made only while the streak fires: a healthy pass reads nothing
//! more.
//!
//! While both hold, the fix is found without a repository-specific detector
//! ([`green_head_fixes`]): an open, non-draft pull request whose current head
//! is approved and whose own run of the failing context passed after the
//! tip went red. The failing check is green on the pull request while red on
//! the base, which is what a fix looks like. The facts come from the
//! steward's per-head census, so naming the fix costs no read. A pull
//! request that rode one of the failing groups is never the fix. One fix per
//! episode ([`pick`]): the only candidate, or the only one carrying a
//! [`FIX_LABELS`] label. Two or more without that are ambiguous: none is
//! jumped or held, and the refusal is reported.
//!
//! [`replay`] runs the same decision over recorded episodes, so a mode can be
//! promoted on evidence: it says whether the rule would have fired, which
//! pull request it would have jumped, which it would have held, and whether
//! the pick matches the pull request that actually repaired the base.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::merge_group::MIN_STREAK;

/// Label that marks a pull request as the fix for a red base. Preferred
/// among candidates that already qualify; it never makes a fix by itself.
pub const FIX_LABEL: &str = "main-red-fix";
/// Labels accepted as [`FIX_LABEL`].
pub const FIX_LABELS: [&str; 2] = [FIX_LABEL, "fixes-main"];
/// Check runs read for the tip, newest first.
const TIP_RUN_PAGE: u32 = 30;

/// One completed run of a required context on the base tip.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TipRun {
    /// Check-run id; a re-run has a larger one.
    pub id: u64,
    /// GitHub conclusion, lowercased.
    pub conclusion: String,
    /// When it completed.
    pub completed_at: Option<DateTime<Utc>>,
}

/// The base tip's verdict for one context.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TipReading {
    /// The tip commit the runs belong to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// The newest completed verdict is a failure.
    pub red: bool,
    /// When the tip's current run of failures began: the completion of the
    /// oldest failure after its last passing verdict. `None` unless `red`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub red_since: Option<DateTime<Utc>>,
    /// When the tip's newest verdict for the context completed, red or not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_at: Option<DateTime<Utc>>,
    /// Why the tip could not be read, when it could not. An unread tip is
    /// never red.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn is_pass(conclusion: &str) -> bool {
    matches!(conclusion, "success" | "neutral" | "skipped")
}

fn is_fail(conclusion: &str) -> bool {
    matches!(conclusion, "failure" | "timed_out" | "action_required")
}

/// Judge the tip from its completed runs of one context. Cancelled and stale
/// runs say nothing; the newest verdict decides.
#[must_use]
pub fn judge_tip(runs: &[TipRun]) -> (bool, Option<DateTime<Utc>>) {
    let mut runs = runs
        .iter()
        .filter(|run| is_pass(&run.conclusion) || is_fail(&run.conclusion))
        .collect::<Vec<_>>();
    runs.sort_by_key(|run| std::cmp::Reverse(run.id));
    let failures = runs
        .iter()
        .take_while(|run| is_fail(&run.conclusion))
        .collect::<Vec<_>>();
    if failures.is_empty() {
        return (false, None);
    }
    (true, failures.last().and_then(|run| run.completed_at))
}

/// When the newest verdict (pass or fail) among `runs` completed.
#[must_use]
pub fn newest_verdict_at(runs: &[TipRun]) -> Option<DateTime<Utc>> {
    runs.iter()
        .filter(|run| is_pass(&run.conclusion) || is_fail(&run.conclusion))
        .max_by_key(|run| run.id)
        .and_then(|run| run.completed_at)
}

/// Whether the base is confirmed broken on a context the merge groups
/// already show failing. The tip's own newest verdict failing confirms it.
/// So does a red-everywhere episode on the same check (three or more open
/// pull requests failing it the same way) that began after the tip's newest
/// verdict: a tip green only from before the episode is stale, as when a
/// date-dependent test breaks with nothing re-running the tip. A tip read
/// green after the episode began, or not read at all, confirms nothing.
/// Returns when the base went red.
#[must_use]
pub fn confirmed_since(
    tip: &TipReading,
    red_everywhere_since: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    if tip.error.is_some() {
        return None;
    }
    if tip.red {
        // A red tip with no known red time: confirmed, but read as red since
        // the end of time, so no head can be shown to pass after the break.
        return tip
            .red_since
            .or(red_everywhere_since)
            .or(Some(DateTime::<Utc>::MAX_UTC));
    }
    let since = red_everywhere_since?;
    tip.newest_at
        .is_none_or(|newest| newest < since)
        .then_some(since)
}

/// The one read the tip needs: the context's check runs on the commit the
/// base branch points at, every attempt included.
#[must_use]
pub fn tip_check_path(repo: &str, base: &str, context: &str) -> String {
    format!(
        "repos/{repo}/commits/{base}/check-runs?check_name={}&filter=all&per_page={TIP_RUN_PAGE}",
        encode(context)
    )
}

/// Percent-encode a query value (contexts carry spaces and `&`).
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Read [`tip_check_path`]'s answer: the tip commit and its completed runs
/// of `context`, from `app_id` when the rule pins one.
#[must_use]
pub fn parse_tip(value: &Value, context: &str, app_id: Option<u64>) -> TipReading {
    let mut sha = None;
    let runs = value
        .get("check_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|run| run.get("name").and_then(Value::as_str) == Some(context))
        .filter(|run| {
            app_id
                .is_none_or(|app_id| run.pointer("/app/id").and_then(Value::as_u64) == Some(app_id))
        })
        .filter(|run| run.get("status").and_then(Value::as_str) == Some("completed"))
        .filter_map(|run| {
            if sha.is_none() {
                sha = run
                    .get("head_sha")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some(TipRun {
                id: run.get("id").and_then(Value::as_u64)?,
                conclusion: run
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase(),
                completed_at: run
                    .get("completed_at")
                    .and_then(Value::as_str)
                    .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                    .map(|at| at.with_timezone(&Utc)),
            })
        })
        .collect::<Vec<_>>();
    let (red, red_since) = judge_tip(&runs);
    TipReading {
        sha,
        red,
        red_since,
        newest_at: newest_verdict_at(&runs),
        error: None,
    }
}

/// One open pull request as fix-first sees it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct FixCandidate {
    /// Pull-request number.
    pub number: u64,
    /// Drafts are never the fix.
    #[serde(default)]
    pub draft: bool,
    /// Labels on it.
    #[serde(default)]
    pub labels: Vec<String>,
    /// When its current head was approved; `None` while it is not.
    #[serde(default)]
    pub approved_at: Option<DateTime<Utc>>,
    /// When its current head's newest run of the failing context completed
    /// green; `None` when that run is missing, running or red.
    #[serde(default)]
    pub context_green_at: Option<DateTime<Utc>>,
    /// It rode one of the failing merge groups, so it did not fix them.
    #[serde(default)]
    pub rode_failing_group: bool,
}

/// A pull request that qualifies as the fix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GreenFix {
    /// Pull-request number.
    pub pr: u64,
    /// When its head was approved.
    pub approved_at: DateTime<Utc>,
    /// When its head passed the context.
    pub green_at: DateTime<Utc>,
    /// It carries one of [`FIX_LABELS`].
    pub labelled: bool,
}

impl GreenFix {
    /// The evidence line reported for it.
    #[must_use]
    pub fn evidence(&self, context: &str, red_since: DateTime<Utc>) -> String {
        format!(
            "green head: `{context}` passed on its head at {} after the base tip went red at {}; approved at {}{}",
            self.green_at.to_rfc3339(),
            red_since.to_rfc3339(),
            self.approved_at.to_rfc3339(),
            if self.labelled {
                "; fix label".to_owned()
            } else {
                String::new()
            }
        )
    }
}

/// Whether a candidate's head passed the context after the base went red,
/// approved or not: not a draft, never on a failing group, green after
/// `red_since`.
#[must_use]
pub fn passed_after(candidate: &FixCandidate, red_since: DateTime<Utc>) -> bool {
    !candidate.draft
        && !candidate.rode_failing_group
        && candidate.context_green_at.is_some_and(|at| at > red_since)
}

/// Every candidate that qualifies as the fix for a context red on the tip
/// since `red_since`: approved at its head, and its head's run of the
/// context passed after the tip went red. A green run that predates the
/// break proves nothing, because it never ran against the broken base.
#[must_use]
pub fn green_head_fixes(candidates: &[FixCandidate], red_since: DateTime<Utc>) -> Vec<GreenFix> {
    candidates
        .iter()
        .filter(|candidate| passed_after(candidate, red_since))
        .filter_map(|candidate| {
            let green_at = candidate.context_green_at?;
            Some(GreenFix {
                pr: candidate.number,
                approved_at: candidate.approved_at?,
                green_at,
                labelled: candidate
                    .labels
                    .iter()
                    .any(|label| FIX_LABELS.iter().any(|fix| label.eq_ignore_ascii_case(fix))),
            })
        })
        .collect()
}

/// The one fix, or a refusal when the evidence does not single one out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "pick", rename_all = "snake_case")]
pub enum Pick {
    /// Nothing qualifies.
    None,
    /// Exactly one pull request is the fix.
    One(GreenFix),
    /// Several qualify and no single [`FIX_LABELS`] label separates them:
    /// none is jumped, none is held, and the refusal is reported.
    Ambiguous {
        /// Every pull request that qualified, by number.
        prs: Vec<u64>,
    },
}

/// The one fix: the only candidate, or the only one carrying a
/// [`FIX_LABELS`] label. Anything else is [`Pick::Ambiguous`]: a jump goes
/// past the queue's own order, so it is never a guess between rivals.
#[must_use]
pub fn pick(fixes: &[GreenFix]) -> Pick {
    let mut prs = fixes.iter().map(|fix| fix.pr).collect::<Vec<_>>();
    prs.sort_unstable();
    prs.dedup();
    if prs.is_empty() {
        return Pick::None;
    }
    if prs.len() == 1 {
        return Pick::One(fixes[0].clone());
    }
    let labelled = fixes.iter().filter(|fix| fix.labelled).collect::<Vec<_>>();
    let mut labelled_prs = labelled.iter().map(|fix| fix.pr).collect::<Vec<_>>();
    labelled_prs.dedup();
    if labelled_prs.len() == 1 {
        return Pick::One(labelled[0].clone());
    }
    Pick::Ambiguous { prs }
}

/// One confirmed context, as fix-first judges it.
#[derive(Clone, Copy, Debug)]
pub struct ContextFacts<'a> {
    /// The context.
    pub context: &'a str,
    /// When the base went red on it; `None` when unknown, which exempts
    /// nobody.
    pub red_since: Option<DateTime<Utc>>,
    /// Every open pull request, with this context's facts.
    pub candidates: &'a [FixCandidate],
}

/// What fix-first decides over the confirmed contexts. The steward's
/// admission and [`replay`] both read it, so a replay scores exactly what
/// production would do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// Heads that passed every confirmed context after it went red, approved
    /// or not. Under a confirmed context they are the only ones admitted
    /// (the fix among them jumped), and every other approved head is held.
    pub covering: std::collections::BTreeSet<u64>,
    /// The fix, or why there is none.
    pub pick: Pick,
    /// Evidence lines per qualifying fix, one per context.
    pub evidence: std::collections::BTreeMap<u64, Vec<String>>,
}

impl Decision {
    /// Whether a confirmed episode holds this approved, non-draft head.
    #[must_use]
    pub fn holds(&self, pr: u64) -> bool {
        !self.covering.contains(&pr)
    }
}

/// Decide over the confirmed contexts. A head covers them only if it passed
/// each one after it went red ([`passed_after`]); a context with no known
/// red time empties the set. The fix must be in the set, so one that passes
/// context A but not B is never picked.
#[must_use]
pub fn decide(contexts: &[ContextFacts<'_>]) -> Decision {
    let mut covering: Option<std::collections::BTreeSet<u64>> = None;
    let mut found: Vec<(GreenFix, String)> = Vec::new();
    for facts in contexts {
        let Some(red_since) = facts.red_since else {
            covering = Some(std::collections::BTreeSet::new());
            continue;
        };
        let passing = facts
            .candidates
            .iter()
            .filter(|candidate| passed_after(candidate, red_since))
            .map(|candidate| candidate.number)
            .collect::<std::collections::BTreeSet<_>>();
        covering = Some(match covering {
            None => passing,
            Some(so_far) => so_far.intersection(&passing).copied().collect(),
        });
        for fix in green_head_fixes(facts.candidates, red_since) {
            let line = fix.evidence(facts.context, red_since);
            found.push((fix, line));
        }
    }
    let covering = covering.unwrap_or_default();
    let mut fixes: Vec<GreenFix> = Vec::new();
    let mut evidence: std::collections::BTreeMap<u64, Vec<String>> =
        std::collections::BTreeMap::new();
    for (fix, line) in found {
        if !covering.contains(&fix.pr) {
            continue;
        }
        evidence.entry(fix.pr).or_default().push(line);
        if !fixes.iter().any(|known| known.pr == fix.pr) {
            fixes.push(fix);
        }
    }
    Decision {
        pick: pick(&fixes),
        covering,
        evidence,
    }
}

/// One recorded episode with the facts needed to replay it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ReplayEpisode {
    /// `owner/repo@base`, as the shadow ledger keys it.
    pub key: String,
    /// The poisoned context.
    pub context: String,
    /// The shadow episode's start.
    pub started_at: DateTime<Utc>,
    /// Its end, once it ended.
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    /// The longest merge-group streak the shadow ledger saw.
    pub max_streak: usize,
    /// What the shadow hold would have held.
    #[serde(default)]
    pub would_hold: Vec<u64>,
    /// The moment to replay: facts later than this are not seen.
    pub decided_at: DateTime<Utc>,
    /// The tip's completed runs of the context.
    #[serde(default)]
    pub tip_runs: Vec<TipRun>,
    /// When a red-everywhere episode on the context's check began, if one
    /// did.
    #[serde(default)]
    pub red_everywhere_since: Option<DateTime<Utc>>,
    /// Open pull requests at `decided_at`.
    #[serde(default)]
    pub prs: Vec<FixCandidate>,
    /// The pull request that actually repaired the base.
    #[serde(default)]
    pub actual_fix: Option<u64>,
    /// Where the facts came from.
    #[serde(default)]
    pub note: String,
}

/// What fix-first would have done in one episode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReplayOutcome {
    /// The episode key.
    pub key: String,
    /// The context.
    pub context: String,
    /// When replayed.
    pub decided_at: DateTime<Utc>,
    /// Whether the conservative rule fired (streak and red tip).
    pub fires: bool,
    /// Why, for a person.
    pub why: String,
    /// The fix it names.
    pub fix: Option<u64>,
    /// Pull requests that all qualified, so none was named: admitted
    /// plainly, never jumped, never held.
    pub ambiguous: Vec<u64>,
    /// Approved pull requests it would not newly enqueue.
    pub held: Vec<u64>,
    /// The shadow ledger's would-hold set, for comparison.
    pub shadow_would_hold: Vec<u64>,
    /// The pull request that actually repaired the base.
    pub actual_fix: Option<u64>,
    /// `Some(true)` when the named fix is the actual one, `Some(false)` when
    /// it is another; `None` when nothing was named or nothing is known.
    pub pick_matches_actual: Option<bool>,
    /// Whether the actual fix ended up held: the one outcome that must
    /// never happen.
    pub actual_fix_held: bool,
}

/// Replay one episode at its `decided_at`.
#[must_use]
pub fn replay(episode: &ReplayEpisode) -> ReplayOutcome {
    let at = episode.decided_at;
    let seen = |time: Option<DateTime<Utc>>| time.filter(|time| *time <= at);
    let tip_runs = episode
        .tip_runs
        .iter()
        .filter(|run| seen(run.completed_at).is_some())
        .cloned()
        .collect::<Vec<_>>();
    let (tip_red, tip_red_since) = judge_tip(&tip_runs);
    let tip = TipReading {
        sha: None,
        red: tip_red,
        red_since: tip_red_since,
        newest_at: newest_verdict_at(&tip_runs),
        error: None,
    };
    let red_since = confirmed_since(&tip, seen(episode.red_everywhere_since));
    let red = red_since.is_some();
    let streak = episode.max_streak >= MIN_STREAK;
    let prs = episode
        .prs
        .iter()
        .map(|pr| FixCandidate {
            approved_at: seen(pr.approved_at),
            context_green_at: seen(pr.context_green_at),
            ..pr.clone()
        })
        .collect::<Vec<_>>();
    let fires = streak && red;
    let why = match (streak, red) {
        (false, _) => format!(
            "streak {} below {MIN_STREAK}: nothing acts",
            episode.max_streak
        ),
        (true, false) => {
            "neither the base tip nor a red-everywhere episode shows the base red: nothing acts"
                .to_owned()
        }
        (true, true) => format!(
            "streak {} and the base red since {} ({})",
            episode.max_streak,
            red_since.map_or_else(|| "an unknown time".to_owned(), |at| at.to_rfc3339()),
            if tip_red {
                "base tip red"
            } else {
                "red everywhere, tip verdict stale"
            }
        ),
    };
    // The same decision the steward makes live ([`decide`]).
    let decision = fires.then(|| {
        decide(&[ContextFacts {
            context: &episode.context,
            red_since,
            candidates: &prs,
        }])
    });
    let (fix, ambiguous) = match decision.as_ref().map(|decision| &decision.pick) {
        Some(Pick::One(fix)) => (Some(fix.pr), Vec::new()),
        Some(Pick::Ambiguous { prs }) => (None, prs.clone()),
        _ => (None, Vec::new()),
    };
    let held = decision.as_ref().map_or_else(Vec::new, |decision| {
        prs.iter()
            .filter(|pr| !pr.draft && pr.approved_at.is_some() && decision.holds(pr.number))
            .map(|pr| pr.number)
            .collect::<Vec<_>>()
    });
    ReplayOutcome {
        key: episode.key.clone(),
        context: episode.context.clone(),
        decided_at: at,
        fires,
        why,
        fix,
        ambiguous,
        actual_fix_held: episode
            .actual_fix
            .is_some_and(|actual| held.contains(&actual)),
        held,
        shadow_would_hold: episode.would_hold.clone(),
        actual_fix: episode.actual_fix,
        pick_matches_actual: match (fix, episode.actual_fix) {
            (Some(fix), Some(actual)) => Some(fix == actual),
            _ => None,
        },
    }
}

/// A score over replayed episodes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ReplayScore {
    /// Episodes replayed.
    pub episodes: usize,
    /// Episodes the conservative rule fired on.
    pub fired: usize,
    /// Fired episodes that named a fix.
    pub named: usize,
    /// Named fixes that were the actual one.
    pub correct: usize,
    /// Named fixes that were another pull request.
    pub wrong: usize,
    /// Episodes whose actual fix would have been held.
    pub actual_fix_held: usize,
}

/// Score replayed outcomes.
#[must_use]
pub fn score(outcomes: &[ReplayOutcome]) -> ReplayScore {
    ReplayScore {
        episodes: outcomes.len(),
        fired: outcomes.iter().filter(|outcome| outcome.fires).count(),
        named: outcomes
            .iter()
            .filter(|outcome| outcome.fix.is_some())
            .count(),
        correct: outcomes
            .iter()
            .filter(|outcome| outcome.pick_matches_actual == Some(true))
            .count(),
        wrong: outcomes
            .iter()
            .filter(|outcome| outcome.pick_matches_actual == Some(false))
            .count(),
        actual_fix_held: outcomes
            .iter()
            .filter(|outcome| outcome.actual_fix_held)
            .count(),
    }
}

#[cfg(test)]
mod tests;
