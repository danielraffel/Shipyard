//! One notice per poison episode, on the pull request the steward picked as
//! the fix.
//!
//! The hold is invisible to the people who could clear it: the fix was
//! picked and fast-tracked, but nobody was told, and a fix waiting on a red
//! required review (`release-path-review`) stalled because nobody knew to
//! run it. While a context is confirmed red on the base, the first pass that
//! sees an episode posts one comment on the fix. It names the failing check,
//! the red tip, how many pull requests are held, and what the fix still
//! needs. A required check that is red on the fix is named, and its review
//! is asked for.
//!
//! The comment is recorded in the fast-track receipts (`action =
//! "fix_notice"`), keyed by the base, the confirmed contexts and when each
//! went red, per fix pull request. A later pass in the same episode finds the
//! receipt and posts nothing; a new episode, or a new fix pick, posts again.
//! A failed post is retried after [`RETRY_MINUTES`]. No fix pick means
//! nothing is posted and the report says so.
//!
//! `[base_health] notify_fix_pr = "on" | "dry-run" | "off"` (default `on` on
//! a merge-queue base) switches it; it acts only while the hold is `on`.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};

use super::{FAST_TRACK_FILE, FastTrackReceipt, RepoPoison, append_receipt, load_receipts};
use crate::base_health::PoisonHold;
use crate::base_health::fix_first::{TipReading, UNKNOWN_ONSET};
use crate::merge_steward::{RequiredCheck, StewardCheck, selected_required_check};

/// The receipt action.
const ACTION: &str = "fix_notice";
/// How long a failed post waits before it is tried again.
const RETRY_MINUTES: i64 = 30;

/// Required checks of a fix that finished without passing, other than the
/// approval context (which every head lacks before review).
pub(in crate::app::merge_steward_cmd) fn red_required(
    checks: &[StewardCheck],
    required: &[RequiredCheck],
    approval_context: &str,
) -> Vec<String> {
    required
        .iter()
        .filter(|rule| rule.context != approval_context)
        .filter(|rule| {
            selected_required_check(checks, rule).is_some_and(|check| {
                check.status.eq_ignore_ascii_case("COMPLETED")
                    && !check.conclusion.as_deref().is_some_and(|conclusion| {
                        matches!(
                            conclusion.to_ascii_lowercase().as_str(),
                            "success" | "neutral" | "skipped"
                        )
                    })
            })
        })
        .map(|rule| rule.context.clone())
        .collect()
}

/// The episode: the base and each confirmed context with when it went red.
pub(super) fn episode_key(
    repo: &str,
    base: &str,
    confirmed: &BTreeMap<String, DateTime<Utc>>,
) -> String {
    let contexts = confirmed
        .iter()
        .map(|(context, since)| {
            let since = if *since == UNKNOWN_ONSET {
                "unknown".to_owned()
            } else {
                since.to_rfc3339()
            };
            format!("{context}@{since}")
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{}@{base}#{contexts}", repo.to_ascii_lowercase())
}

/// What the comment says.
pub(super) struct Facts<'a> {
    pub(super) base: &'a str,
    pub(super) contexts: Vec<String>,
    pub(super) tip: &'a BTreeMap<String, TipReading>,
    pub(super) held: usize,
    pub(super) fix: u64,
    /// Required checks red on the fix.
    pub(super) red_required: Vec<String>,
}

/// The comment body.
pub(super) fn body(facts: &Facts<'_>) -> String {
    let named = facts
        .contexts
        .iter()
        .map(|context| format!("`{context}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let tips = facts
        .contexts
        .iter()
        .filter_map(|context| facts.tip.get(context)?.sha.as_deref())
        .map(|sha| format!("`{}`", sha.get(..12).unwrap_or(sha)))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    let tip_text = if tips.is_empty() {
        String::new()
    } else {
        format!(" Red tip: {tips}.")
    };
    let needs = if facts.red_required.is_empty() {
        "It needs nothing further from the steward's side: once it is approved and green it is \
admitted ahead of the held pull requests."
            .to_owned()
    } else {
        let checks = facts
            .red_required
            .iter()
            .map(|check| format!("`{check}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "It is not admitted yet because the required check {checks} is red on its head. \
Please run or post that review for this head so the fix can land."
        )
    };
    format!(
        "Base poisoned: the required check {named} is failing on `{base}`.{tip_text} The steward \
is holding {held} approved, green pull request(s) until it passes and picked #{fix} (this pull \
request) as the fix.\n\n{needs}\n\n\
This is the only notice for this episode; it is posted again only if a new episode starts or the \
fix pick changes.",
        base = facts.base,
        held = facts.held,
        fix = facts.fix,
    )
}

/// Post the notice for each fix pick once per episode. Returns report lines.
///
/// `red_for` answers, for a fix number, the red required checks on its head,
/// or `None` when it is not open on the base.
#[allow(clippy::too_many_arguments)]
pub(in crate::app::merge_steward_cmd) fn notify(
    mode: PoisonHold,
    post: &dyn Fn(u64, &str) -> Result<(), String>,
    state_dir: &Path,
    repo: &str,
    base: &str,
    poison: &RepoPoison,
    held: usize,
    red_for: &dyn Fn(u64) -> Option<Vec<String>>,
    now: DateTime<Utc>,
) -> Vec<String> {
    if mode == PoisonHold::Off {
        return Vec::new();
    }
    let confirmed = super::confirmed_red_since(&poison.health, &poison.tip, &poison.red_everywhere);
    if confirmed.is_empty() {
        return Vec::new();
    }
    if poison.fixes.is_empty() {
        return vec!["fix_notice: no fix pick; nothing posted".to_owned()];
    }
    let key = episode_key(repo, base, &confirmed);
    let path = state_dir.join(FAST_TRACK_FILE);
    let mut lines = Vec::new();
    for number in poison.fixes.keys() {
        let Some(red) = red_for(*number) else {
            lines.push(format!("fix_notice #{number}: not open on {base}"));
            continue;
        };
        let text = body(&Facts {
            base,
            contexts: confirmed.keys().cloned().collect(),
            tip: &poison.tip,
            held,
            fix: *number,
            red_required: red,
        });
        if mode == PoisonHold::DryRun {
            lines.push(format!("fix_notice #{number}: would comment: {text}"));
            continue;
        }
        let last = load_receipts(&path).into_iter().rfind(|receipt| {
            receipt.action == ACTION
                && receipt.pr == *number
                && receipt.head == key
                && receipt.repo.eq_ignore_ascii_case(repo)
        });
        if let Some(receipt) = last {
            let retry = receipt.outcome.starts_with("failed")
                && DateTime::parse_from_rfc3339(&receipt.at).is_ok_and(|at| {
                    now - at.with_timezone(&Utc) >= chrono::Duration::minutes(RETRY_MINUTES)
                });
            if !retry {
                lines.push(format!("fix_notice #{number}: already posted this episode"));
                continue;
            }
        }
        let outcome = match post(*number, &text) {
            Ok(()) => "posted".to_owned(),
            Err(error) => format!("failed: {error}"),
        };
        lines.push(format!("fix_notice #{number}: {outcome}"));
        let receipt = FastTrackReceipt {
            repo: repo.to_owned(),
            pr: *number,
            head: key.clone(),
            action: ACTION.to_owned(),
            at: now.to_rfc3339(),
            outcome,
        };
        if let Err(error) = append_receipt(&path, &receipt) {
            lines.push(format!(
                "fix_notice #{number}: receipt not recorded: {error}"
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests;
