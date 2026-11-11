//! The shadow ledger for base-poison rules, and the promotion gate it feeds.
//!
//! Every base-poison rule starts in shadow: it reports what it would do and
//! changes nothing. This ledger records each would-fire episode per rule and
//! context, labels it once it ends, and computes whether the rule has earned
//! enforcement ([`gate`]). The rules measured:
//!
//! * `streak`: the detector's verdict (a context failing in consecutive merge
//!   groups that carried different pull requests).
//! * `strict`: `streak`, and every group in it failed with the same first
//!   error line.
//!
//! An episode is **corroborated** (a true fire) when, while it lasted, the
//! streak grew to include at least one more failing group: the failure kept
//! reproducing on pull requests that did not cause it. An episode that ends
//! with its first two groups is labelled a false fire: the base passed the
//! next time anything was tried. Its cost is every pull request it held,
//! for as long as it held them; a true fire saves one gate per held entry.
//!
//! A rule may move from shadow to enforcing only when all of these hold over
//! at least [`MIN_EVENTS`] ended episodes and [`MIN_DAYS`] days, whichever is
//! later: precision at least [`MIN_PRECISION_DELAY`] (rules that delay or
//! remove work), false-fire cost below true-fire savings, and a one-line
//! config flips it back (every rule here is a `[base_health]` key).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::base_poison::RepoPoison;

/// Open episodes, rewritten each pass.
const OPEN_FILE: &str = "base-health/shadow-open.json";
/// Ended episodes, one JSON line each.
const ENDED_FILE: &str = "base-health/shadow-ended.jsonl";
/// Ended episodes a gate needs.
pub(super) const MIN_EVENTS: usize = 10;
/// Days of record a gate needs.
pub(super) const MIN_DAYS: i64 = 14;
/// Precision a rule that delays or removes work needs.
pub(super) const MIN_PRECISION_DELAY: f64 = 0.9;
/// Gate minutes a true fire saves per held entry (one avoided group).
const GATE_MINUTES: f64 = 40.0;

/// A count as a float, saturating (counts here are small).
fn count<T: TryInto<u32>>(value: T) -> f64 {
    f64::from(value.try_into().unwrap_or(u32::MAX))
}

/// One would-fire episode of one rule on one context.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(super) struct Episode {
    /// `owner/repo@base`.
    pub(super) key: String,
    /// The required context.
    pub(super) context: String,
    /// `streak` or `strict`.
    pub(super) rule: String,
    /// RFC 3339.
    pub(super) started_at: String,
    /// RFC 3339 of the last pass that saw it firing.
    pub(super) last_seen_at: String,
    /// The longest streak seen.
    pub(super) max_streak: usize,
    /// Pull requests it really held or paused at any point.
    pub(super) held: BTreeSet<u64>,
    /// Pull requests it would have held or paused (dry-run) at any point.
    #[serde(default)]
    pub(super) would_hold: BTreeSet<u64>,
    /// Set when it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) corroborated: Option<bool>,
}

impl Episode {
    /// Every pull request it touched, really or in shadow: the gate's cost
    /// side is the same question either way.
    fn affected(&self) -> usize {
        self.held.union(&self.would_hold).count()
    }

    fn minutes(&self) -> f64 {
        let at = |text: &str| DateTime::parse_from_rfc3339(text).map(|at| at.with_timezone(&Utc));
        match (at(&self.started_at), at(&self.last_seen_at)) {
            (Ok(start), Ok(end)) => count((end - start).num_seconds().max(0)) / 60.0,
            _ => 0.0,
        }
    }
}

/// Which rules fire this pass, by `(context, rule)`, with the streak length.
pub(super) fn firing(poison: &RepoPoison) -> BTreeMap<(String, String), usize> {
    let mut out = BTreeMap::new();
    for context in poison.health.poisoned() {
        let streak = context.groups.len();
        out.insert((context.context.clone(), "streak".to_owned()), streak);
        if context.same_first_error == Some(true) {
            out.insert((context.context.clone(), "strict".to_owned()), streak);
        }
    }
    out
}

/// Advance the open episodes with this pass's reading: extend the ones still
/// firing, open new ones, end and label the rest. Returns the episodes that
/// ended this pass.
pub(super) fn advance(
    open: &mut Vec<Episode>,
    key: &str,
    firing: &BTreeMap<(String, String), usize>,
    held: &BTreeSet<u64>,
    would: &BTreeSet<u64>,
    now: DateTime<Utc>,
) -> Vec<Episode> {
    let stamp = now.to_rfc3339();
    let mut ended = Vec::new();
    open.retain_mut(|episode| {
        if episode.key != key {
            return true;
        }
        if let Some(streak) = firing.get(&(episode.context.clone(), episode.rule.clone())) {
            episode.last_seen_at.clone_from(&stamp);
            episode.max_streak = episode.max_streak.max(*streak);
            episode.held.extend(held);
            episode.would_hold.extend(would);
            true
        } else {
            let mut done = episode.clone();
            done.corroborated = Some(done.max_streak > crate::base_health::merge_group::MIN_STREAK);
            ended.push(done);
            false
        }
    });
    for ((context, rule), streak) in firing {
        if !open.iter().any(|episode| {
            episode.key == key && &episode.context == context && &episode.rule == rule
        }) {
            open.push(Episode {
                key: key.to_owned(),
                context: context.clone(),
                rule: rule.clone(),
                started_at: stamp.clone(),
                last_seen_at: stamp.clone(),
                max_streak: *streak,
                held: held.clone(),
                would_hold: would.clone(),
                corroborated: None,
            });
        }
    }
    ended
}

/// The promotion gate for one rule over its ended episodes.
pub(super) fn gate(rule: &str, ended: &[Episode], now: DateTime<Utc>) -> Value {
    let episodes = ended
        .iter()
        .filter(|episode| episode.rule == rule)
        .collect::<Vec<_>>();
    let fires = episodes.len();
    let true_fires = episodes
        .iter()
        .filter(|episode| episode.corroborated == Some(true))
        .count();
    let first = episodes
        .iter()
        .filter_map(|episode| DateTime::parse_from_rfc3339(&episode.started_at).ok())
        .map(|at| at.with_timezone(&Utc))
        .min();
    let days = first.map_or(0, |first| (now - first).num_days());
    let precision = (fires > 0).then(|| count(true_fires) / count(fires));
    let false_minutes: f64 = episodes
        .iter()
        .filter(|episode| episode.corroborated != Some(true))
        .map(|episode| episode.minutes() * count(episode.affected().max(1)))
        .sum();
    let true_minutes: f64 = episodes
        .iter()
        .filter(|episode| episode.corroborated == Some(true))
        .map(|episode| GATE_MINUTES * count(episode.affected()))
        .sum();
    let enough = fires >= MIN_EVENTS && days >= MIN_DAYS;
    let eligible = enough
        && precision.is_some_and(|precision| precision >= MIN_PRECISION_DELAY)
        && false_minutes < true_minutes;
    json!({
        "rule": rule,
        "fires": fires,
        "true_fires": true_fires,
        "days": days,
        "precision": precision,
        "false_fire_pr_minutes": false_minutes.round(),
        "true_fire_saved_pr_minutes": true_minutes.round(),
        "eligible_to_enforce": eligible,
        "needs": format!(
            ">= {MIN_EVENTS} ended episodes and >= {MIN_DAYS} days, precision >= {MIN_PRECISION_DELAY}, false-fire cost below savings"
        ),
    })
}

fn read_open(state_dir: &Path) -> Vec<Episode> {
    fs::read_to_string(state_dir.join(OPEN_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn read_ended(state_dir: &Path) -> Vec<Episode> {
    fs::read_to_string(state_dir.join(ENDED_FILE))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Record this pass for `repo@base` and report the gate per rule. A write
/// failure is reported, never fatal: the ledger is measurement only.
pub(super) fn record_pass(
    state_dir: &Path,
    repo: &str,
    base: &str,
    poison: Option<&RepoPoison>,
    held: &BTreeSet<u64>,
    would: &BTreeSet<u64>,
    now: DateTime<Utc>,
) -> Value {
    let key = format!("{}@{base}", repo.to_ascii_lowercase());
    let firing = poison.map(firing).unwrap_or_default();
    // An unread history neither extends nor ends an episode.
    if poison.is_some_and(|poison| {
        matches!(
            poison.health,
            crate::base_health::merge_group::MergeGroupHealth::Unknown { .. }
        )
    }) {
        return json!({"skipped": "merge groups unread"});
    }
    let mut open = read_open(state_dir);
    let ended = advance(&mut open, &key, &firing, held, would, now);
    let mut error = None;
    if let Some(parent) = state_dir.join(OPEN_FILE).parent()
        && let Err(failure) = fs::create_dir_all(parent)
    {
        error = Some(failure.to_string());
    }
    let tmp = state_dir.join(OPEN_FILE).with_extension("json.tmp");
    if let Err(failure) = fs::write(&tmp, serde_json::to_string(&open).unwrap_or_default())
        .and_then(|()| fs::rename(&tmp, state_dir.join(OPEN_FILE)))
    {
        error = Some(failure.to_string());
    }
    if !ended.is_empty() {
        match fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state_dir.join(ENDED_FILE))
        {
            Ok(mut file) => {
                for episode in &ended {
                    let _ = writeln!(
                        file,
                        "{}",
                        serde_json::to_string(episode).unwrap_or_default()
                    );
                }
            }
            Err(failure) => error = Some(failure.to_string()),
        }
    }
    let history = read_ended(state_dir)
        .into_iter()
        .filter(|episode| episode.key == key)
        .collect::<Vec<_>>();
    let gates = ["streak", "strict"]
        .iter()
        .map(|rule| gate(rule, &history, now))
        .collect::<Vec<_>>();
    json!({
        "open": open.iter().filter(|episode| episode.key == key).collect::<Vec<_>>(),
        "ended_this_pass": ended,
        "gate": gates,
        "error": error,
    })
}
