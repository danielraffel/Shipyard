//! The merge train: keep a repository's merge queue fed with every approved,
//! green pull request, and measure how well it is fed.
//!
//! ## Off by default, and off means today
//!
//! The train is opt-in per repository with `[train] enabled = true` in the
//! base branch's `.shipyard/config.toml`, read through the GitHub API (never
//! from a head), as `[auto_merge] arm_requires_head_approval` is. A host kill
//! switch always wins over that config: `SHIPYARD_TRAIN=off` (every
//! repository) or `SHIPYARD_TRAIN=off:<owner/repo>`, or a [`PAUSE_FILE`] in
//! the guards directory written by `shipyard train pause --reason`. A pause
//! file that cannot be parsed still pauses.
//!
//! With the train off, no existing landing path consults this module: ship,
//! auto-merge, the steward, the native-arm backstop, the carrier and
//! base-health make exactly the calls they make today.
//!
//! ## What it measures
//!
//! The goal is merges per hour and approved-to-merged latency, not batch
//! size: under GitHub's HEADGREEN grouping every queue entry builds its own
//! cumulative group, so a larger batch saves no merge-group runs. The waste
//! measure is the minutes the queue sat empty while a ready pull request
//! waited outside it. Every figure comes from durable [`Sample`]s that
//! `shipyard train status --record` appends, one per pass.
//!
//! This module is transport-free: callers supply observed facts and receive
//! stages, switches and metrics.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// The environment kill switch: `off`, `0` or `false` for every repository,
/// or `off:<owner/repo>` for one.
pub const KILL_ENV: &str = "SHIPYARD_TRAIN";
/// The pause file in the guards directory.
pub const PAUSE_FILE: &str = "train.pause";
/// Samples further apart than this count as a gap, not as idle time.
pub const MAX_SAMPLE_GAP_MINUTES: i64 = 30;
/// Merges this close together are one merge-queue batch.
pub const BATCH_WINDOW_SECONDS: i64 = 90;

/// Whether the train runs for one repository, and what decided it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TrainSwitch {
    /// `[train] enabled = true` and no kill switch.
    On,
    /// Off, for this reason.
    Off {
        /// What turned it off.
        by: OffBy,
    },
}

impl TrainSwitch {
    /// Whether the train runs.
    #[must_use]
    pub const fn is_on(&self) -> bool {
        matches!(self, Self::On)
    }
}

/// What turned the train off.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum OffBy {
    /// [`KILL_ENV`] names this repository or every repository.
    KillEnv {
        /// The variable's value.
        value: String,
    },
    /// The [`PAUSE_FILE`] exists.
    KillPause {
        /// The recorded reason, or why the file does not parse.
        reason: String,
    },
    /// The base config has no `[train]`, or `enabled` is not `true`.
    ConfigOff,
    /// The base config could not be read or parsed.
    ConfigUnreadable {
        /// What went wrong.
        detail: String,
    },
}

impl TrainSwitch {
    /// One line for a human.
    #[must_use]
    pub fn explain(&self) -> String {
        match self {
            Self::On => "on: `[train] enabled = true` in the base config".to_owned(),
            Self::Off { by } => match by {
                OffBy::KillEnv { value } => format!("off: killed by {KILL_ENV}={value}"),
                OffBy::KillPause { reason } => {
                    format!("off: paused by the guards-dir {PAUSE_FILE} ({reason})")
                }
                OffBy::ConfigOff => {
                    "off: the base config does not set `[train] enabled = true`".to_owned()
                }
                OffBy::ConfigUnreadable { detail } => {
                    format!("off: the base config is unreadable ({detail})")
                }
            },
        }
    }
}

/// The kill switch for `repo`, or `None` when nothing kills it. Checked
/// before any GitHub read, so a killed train reads nothing.
///
/// `env` is [`KILL_ENV`]'s value; `pause` is the [`PAUSE_FILE`]'s state
/// (`None` when absent, else its reason or why it does not parse).
#[must_use]
pub fn kill_switch(env: Option<&str>, pause: Option<String>, repo: &str) -> Option<OffBy> {
    if let Some(value) = env.map(str::trim) {
        let killed = ["off", "0", "false"]
            .iter()
            .any(|word| value.eq_ignore_ascii_case(word))
            || value.split_once(':').is_some_and(|(word, target)| {
                word.eq_ignore_ascii_case("off") && target.trim().eq_ignore_ascii_case(repo)
            });
        if killed {
            return Some(OffBy::KillEnv {
                value: value.to_owned(),
            });
        }
    }
    pause.map(|reason| OffBy::KillPause { reason })
}

/// The [`PAUSE_FILE`] at `path`: `None` when it is absent, else its recorded
/// reason, or why it does not parse. Anything present pauses.
#[must_use]
pub fn read_pause(path: &std::path::Path) -> Option<String> {
    match crate::head_approval::read_opt_out(path) {
        crate::head_approval::OptOut::Absent => None,
        crate::head_approval::OptOut::Valid { reason, .. } => Some(reason),
        crate::head_approval::OptOut::Invalid { detail } => Some(format!(
            "unparseable pause file, which still pauses: {detail}"
        )),
    }
}

/// Whether the base config's text turns the train on.
///
/// # Errors
///
/// When the text does not parse, or `[train] enabled` is not a boolean.
pub fn enabled_in_config(text: &str) -> Result<bool, String> {
    let table = text
        .parse::<toml::Table>()
        .map_err(|error| format!("does not parse: {error}"))?;
    match table.get("train").and_then(|train| train.get("enabled")) {
        None => Ok(false),
        Some(toml::Value::Boolean(enabled)) => Ok(*enabled),
        Some(other) => Err(format!("`train.enabled` is {other}, not a boolean")),
    }
}

/// Where one open pull request stands on its way into the merge queue.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// A draft.
    Draft,
    /// In the merge queue.
    Queued,
    /// Conflicts with its base.
    Conflict,
    /// A required check has not completed on the head.
    ChecksRunning,
    /// A required check failed on the head.
    ChecksFailed,
    /// A required check was interrupted and may be rerun.
    ChecksInterrupted,
    /// The queue ejected this exact head and no new head followed.
    EjectedSameHead,
    /// Already armed for native auto-merge, not yet queued.
    Armed,
    /// No approval `head_approval` accepts names (or carries to) the head.
    NeedsReview,
    /// Opted out, provenance-blocked, or otherwise not landable.
    Held,
    /// A tiered required check has not proven its full lane.
    TierBlocked,
    /// Green, approved, full tier: ready to enter the queue.
    Ready,
}

impl Stage {
    /// Whether a pull request in this stage counts as approved for the
    /// approved-to-merged measure.
    #[must_use]
    pub const fn approved(self) -> bool {
        matches!(
            self,
            Self::Ready | Self::TierBlocked | Self::Queued | Self::Armed
        )
    }
}

/// The stage the steward's own decision implies, before approval and tier
/// are read. `None` means the steward would arm it: read approval next.
#[must_use]
pub fn stage_from_decision(decision: &crate::merge_steward::StewardDecision) -> Option<Stage> {
    use crate::merge_steward::StewardDecision as D;
    match decision {
        D::Queued { .. } => Some(Stage::Queued),
        D::Draft => Some(Stage::Draft),
        D::NeedsUpdate { .. } => Some(Stage::Conflict),
        D::WaitingRequired { .. } => Some(Stage::ChecksRunning),
        D::RequiredFailed { .. } => Some(Stage::ChecksFailed),
        D::RerunTransient { .. } => Some(Stage::ChecksInterrupted),
        D::ArmMergeQueue => None,
        _ => Some(Stage::Held),
    }
}

/// One pull request's place in a [`Sample`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PrSample {
    /// Its stage.
    pub stage: Stage,
    /// Its head when sampled.
    pub head: String,
}

/// A merged pull request, as a sample records it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Merge {
    /// Pull-request number.
    pub number: u64,
    /// When it merged (RFC 3339).
    pub merged_at: String,
}

/// One pass's observation: the unit every metric is computed from.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Sample {
    /// When the pass observed (RFC 3339).
    pub at: String,
    /// Entries in the merge queue.
    pub queue_depth: u64,
    /// Every open pull request's stage, by number.
    pub prs: BTreeMap<u64, PrSample>,
    /// Pull requests merged since the previous sample.
    #[serde(default)]
    pub merged: Vec<Merge>,
}

impl Sample {
    /// Pull requests in [`Stage::Ready`].
    #[must_use]
    pub fn ready(&self) -> usize {
        self.prs
            .values()
            .filter(|pr| pr.stage == Stage::Ready)
            .count()
    }
}

/// What a window of samples says.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Metrics {
    /// Samples in the window.
    pub samples: usize,
    /// Hours the samples span.
    pub hours: f64,
    /// Pull requests merged in the window.
    pub merges: usize,
    /// Merges per hour.
    pub merges_per_hour: f64,
    /// Approved-to-merged minutes, median, over merges whose approval was
    /// sampled.
    pub approved_to_merged_p50_minutes: Option<i64>,
    /// The same, 95th percentile.
    pub approved_to_merged_p95_minutes: Option<i64>,
    /// Merges whose approval was sampled (the latency population).
    pub latency_population: usize,
    /// Minutes the queue was empty while a ready pull request waited.
    pub queue_empty_while_ready_minutes: i64,
    /// Merged batches by size: `{size: count}`.
    pub batch_sizes: BTreeMap<usize, usize>,
    /// Distinct heads sampled per merged pull request that was sampled: the
    /// PR-head churn each merge cost (a lower bound between samples).
    pub heads_per_merged_pr: Option<f64>,
}

/// Compute [`Metrics`] over `samples` (any order).
#[must_use]
pub fn metrics(samples: &[Sample]) -> Metrics {
    let mut ordered: Vec<(DateTime<Utc>, &Sample)> = samples
        .iter()
        .filter_map(|sample| Some((parse_time(&sample.at)?, sample)))
        .collect();
    ordered.sort_by_key(|(at, _)| *at);
    let hours = match (ordered.first(), ordered.last()) {
        (Some((first, _)), Some((last, _))) => {
            f64::from(i32::try_from((*last - *first).num_seconds()).unwrap_or(i32::MAX)) / 3600.0
        }
        _ => 0.0,
    };
    let mut first_approved: BTreeMap<u64, DateTime<Utc>> = BTreeMap::new();
    let mut heads: BTreeMap<u64, std::collections::BTreeSet<&str>> = BTreeMap::new();
    let mut merge_times: BTreeMap<u64, DateTime<Utc>> = BTreeMap::new();
    let mut idle_minutes = 0;
    for (index, (at, sample)) in ordered.iter().enumerate() {
        for (number, pr) in &sample.prs {
            heads.entry(*number).or_default().insert(pr.head.as_str());
            if pr.stage.approved() {
                first_approved.entry(*number).or_insert(*at);
            }
        }
        for merge in &sample.merged {
            if let Some(merged_at) = parse_time(&merge.merged_at) {
                merge_times.entry(merge.number).or_insert(merged_at);
            }
        }
        if let Some((next, _)) = ordered.get(index + 1) {
            let gap = (*next - *at).num_minutes();
            if sample.queue_depth == 0 && sample.ready() > 0 && gap <= MAX_SAMPLE_GAP_MINUTES {
                idle_minutes += gap;
            }
        }
    }
    let mut latencies: Vec<i64> = merge_times
        .iter()
        .filter_map(|(number, merged_at)| {
            let approved = first_approved.get(number)?;
            (merged_at >= approved).then(|| (*merged_at - *approved).num_minutes())
        })
        .collect();
    latencies.sort_unstable();
    let merges = merge_times.len();
    let churn: Vec<usize> = merge_times
        .keys()
        .filter_map(|number| heads.get(number).map(std::collections::BTreeSet::len))
        .collect();
    let heads_per_merged_pr = (!churn.is_empty()).then(|| {
        f64::from(u32::try_from(churn.iter().sum::<usize>()).unwrap_or(u32::MAX))
            / f64::from(u32::try_from(churn.len()).unwrap_or(u32::MAX))
    });
    Metrics {
        samples: ordered.len(),
        hours,
        merges,
        merges_per_hour: if hours > 0.0 {
            f64::from(u32::try_from(merges).unwrap_or(u32::MAX)) / hours
        } else {
            0.0
        },
        approved_to_merged_p50_minutes: percentile(&latencies, 50),
        approved_to_merged_p95_minutes: percentile(&latencies, 95),
        latency_population: latencies.len(),
        queue_empty_while_ready_minutes: idle_minutes,
        batch_sizes: batch_sizes(merge_times.values().copied().collect()),
        heads_per_merged_pr,
    }
}

/// Group merge times into batches: merges within [`BATCH_WINDOW_SECONDS`] of
/// the batch's first merge land together.
fn batch_sizes(mut times: Vec<DateTime<Utc>>) -> BTreeMap<usize, usize> {
    times.sort_unstable();
    let mut sizes = BTreeMap::new();
    let mut start: Option<DateTime<Utc>> = None;
    let mut size = 0usize;
    for at in times {
        match start {
            Some(first) if at - first <= Duration::seconds(BATCH_WINDOW_SECONDS) => size += 1,
            _ => {
                if size > 0 {
                    *sizes.entry(size).or_insert(0) += 1;
                }
                start = Some(at);
                size = 1;
            }
        }
    }
    if size > 0 {
        *sizes.entry(size).or_insert(0) += 1;
    }
    sizes
}

/// The nearest-rank percentile of sorted `values`.
fn percentile(values: &[i64], percent: usize) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let rank = (percent * values.len()).div_ceil(100).max(1);
    values.get(rank - 1).copied()
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

#[cfg(test)]
mod tests;
