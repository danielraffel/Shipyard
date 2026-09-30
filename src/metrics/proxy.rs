//! Load-independent proxy measures for the metrics store.
//!
//! Wall-clock job duration mostly tracks how busy the shared hosts were when
//! the job ran: the same change measured at 1am and at peak can differ by more
//! than any plausible real effect. A comparison built on p50/p90 duration is
//! therefore mostly a comparison of load. The proxies here count what the CI
//! system *did* instead of how long it took, so they stay comparable across
//! load regimes. Wall time is still reported, labelled as load-dependent
//! context, and `--basis wall-time` restores the duration-based verdict.
//!
//! Every proxy declares its detection floor: a minimum sample per window and a
//! minimum change it can distinguish from noise. Below the floor a comparison
//! says `insufficient_sample` instead of guessing.
//!
//! Definitions (same text in `shipyard metrics compare --help`):
//!
//! - `failure_share`: failed jobs / jobs that concluded success or failure.
//! - `cancelled_share`: cancelled jobs / all completed jobs.
//! - `starvation_share`: jobs cancelled before any runner was assigned / jobs
//!   whose runner assignment is known. Rows without assignment provenance are
//!   excluded, not guessed.
//! - `attempts_per_pr`: jobs / distinct pull requests, over jobs that carry
//!   pull-request identity. More attempts per PR means re-runs, pushes or
//!   queue ejections.
//! - `queue_wait_per_job_ahead_ms`: median over jobs with an authoritative
//!   queue timestamp of `wait / (1 + jobs ahead)`, where jobs ahead are other
//!   jobs of the same lane queued earlier and still waiting when this one was
//!   queued. Raw wait grows with queue depth; wait per job ahead does not.
//! - `cache_hit_rate`: jobs whose every reported step hit its cache / jobs
//!   that reported cache reuse at all.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// What a comparison's verdict is based on.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Load-independent proxies (the default).
    #[default]
    Proxy,
    /// Wall-clock p50/p90 duration (load-dependent).
    WallTime,
}

impl Basis {
    /// Stable name used in JSON and human output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::WallTime => "wall_time",
        }
    }
}

/// Label attached to every wall-clock number shown alongside a proxy verdict.
pub const WALL_CONTEXT_LABEL: &str = "context (load-dependent)";

/// One job row as the proxies see it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProxySample {
    /// Proxy lane key: the job class (target), never the host.
    pub lane: String,
    /// The job (check) name as the provider reported it. Required-vs-advisory
    /// classification matches this, not the lane.
    pub job: String,
    /// Terminal job status.
    pub status: String,
    /// `(repo, pr)` when the job carries pull-request identity.
    pub pr: Option<(String, i64)>,
    /// Provider-authoritative queue time.
    pub queued_at: Option<DateTime<Utc>>,
    /// Runner start time.
    pub started_at: Option<DateTime<Utc>>,
    /// Completion time.
    pub completed_at: Option<DateTime<Utc>>,
    /// Wall-clock duration, for context only.
    pub total_ms: Option<i64>,
    /// Whether a runner was ever assigned; `None` when unknown.
    pub runner_assigned: Option<bool>,
    /// Whether every reported step hit its cache; `None` when unreported.
    pub cache_hit: Option<bool>,
}

/// Which direction of a proxy is better.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Better {
    /// Smaller is better.
    Lower,
    /// Larger is better.
    Higher,
}

/// How a proxy's change is judged against noise.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Test {
    /// Two-proportion test: the change must exceed both two pooled standard
    /// errors and `min_abs`.
    Proportion { min_abs: f64 },
    /// Relative change of at least `min_rel` and absolute change of at least
    /// `min_abs`.
    Relative { min_rel: f64, min_abs: f64 },
}

/// One proxy measured over one window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProxyValue {
    /// Proxy name.
    pub name: &'static str,
    /// Measured value, `None` when nothing was measurable.
    pub value: Option<f64>,
    /// Samples behind `value` (the denominator).
    pub sample: usize,
    /// Minimum sample for a verdict.
    pub min_sample: usize,
    /// `sample >= min_sample`.
    pub sufficient: bool,
    /// Which direction is better.
    pub better: Better,
    /// Detection floor, in words.
    pub floor: &'static str,
}

/// Direction one proxy moved between two windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Moved in the better direction beyond the floor.
    Improved,
    /// Moved in the worse direction beyond the floor.
    Regressed,
    /// Did not move beyond the floor.
    Unchanged,
    /// One window is below the minimum sample.
    InsufficientSample,
}

/// One proxy across a before/after pair.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProxyDelta {
    /// Proxy name.
    pub name: &'static str,
    /// Value in the earlier window.
    pub before: Option<f64>,
    /// Value in the later window.
    pub after: Option<f64>,
    /// Samples in the earlier window.
    pub before_sample: usize,
    /// Samples in the later window.
    pub after_sample: usize,
    /// What `before_sample` and `after_sample` count. Every job-share proxy
    /// is over job rows (each job's own conclusion), never workflow runs.
    pub unit: &'static str,
    /// Minimum sample per window.
    pub min_sample: usize,
    /// Which direction is better.
    pub better: Better,
    /// Verdict for this proxy.
    pub direction: Direction,
    /// Detection floor, in words.
    pub floor: &'static str,
}

/// Overall verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Something improved and nothing regressed.
    Improved,
    /// Something regressed and nothing improved.
    Regressed,
    /// Both.
    Mixed,
    /// Measured, and nothing moved beyond its floor.
    Unchanged,
    /// Too few samples to say.
    InsufficientSample,
}

impl Verdict {
    /// Stable name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Improved => "improved",
            Self::Regressed => "regressed",
            Self::Mixed => "mixed",
            Self::Unchanged => "unchanged",
            Self::InsufficientSample => "insufficient_sample",
        }
    }
}

/// Wall-clock duration, shown as load-dependent context.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WallContext {
    /// Always [`WALL_CONTEXT_LABEL`].
    pub label: &'static str,
    /// Successful-job durations in the earlier window.
    pub before_sample: usize,
    /// Successful-job durations in the later window.
    pub after_sample: usize,
    /// Earlier p50.
    pub before_p50_ms: Option<i64>,
    /// Later p50.
    pub after_p50_ms: Option<i64>,
    /// Earlier p90.
    pub before_p90_ms: Option<i64>,
    /// Later p90.
    pub after_p90_ms: Option<i64>,
}

/// A before/after comparison.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Comparison {
    /// Basis the headline `verdict` uses.
    pub basis: Basis,
    /// Headline verdict under `basis`.
    pub verdict: Verdict,
    /// Verdict from the proxies, always computed.
    pub proxy_verdict: Verdict,
    /// Verdict from wall-clock p50, always computed (load-dependent).
    pub wall_time_verdict: Verdict,
    /// Per-proxy movement.
    pub proxies: Vec<ProxyDelta>,
    /// Wall-clock context.
    pub context: WallContext,
}

impl Comparison {
    /// One-line human summary: verdict, moved proxies, then wall context.
    #[must_use]
    pub fn summary(&self) -> String {
        let moved: Vec<String> = self
            .proxies
            .iter()
            .filter(|delta| matches!(delta.direction, Direction::Improved | Direction::Regressed))
            .map(|delta| {
                format!(
                    "{} {} {}->{} (n={}/{} {})",
                    delta.name,
                    match delta.direction {
                        Direction::Improved => "improved",
                        _ => "regressed",
                    },
                    fmt_value(delta.before),
                    fmt_value(delta.after),
                    delta.before_sample,
                    delta.after_sample,
                    delta.unit
                )
            })
            .collect();
        let insufficient: Vec<&str> = self
            .proxies
            .iter()
            .filter(|delta| delta.direction == Direction::InsufficientSample)
            .map(|delta| delta.name)
            .collect();
        let mut text = format!(
            "{} (basis {}; proxies {}, wall time {})",
            self.verdict.as_str(),
            self.basis.as_str(),
            self.proxy_verdict.as_str(),
            self.wall_time_verdict.as_str()
        );
        if !moved.is_empty() {
            text.push_str(": ");
            text.push_str(&moved.join(", "));
        }
        if !insufficient.is_empty() {
            text.push_str("; insufficient sample: ");
            text.push_str(&insufficient.join(", "));
        }
        let _ = write!(
            text,
            "; {}: p50 {}->{} ms, p90 {}->{} ms",
            self.context.label,
            fmt_ms(self.context.before_p50_ms),
            fmt_ms(self.context.after_p50_ms),
            fmt_ms(self.context.before_p90_ms),
            fmt_ms(self.context.after_p90_ms),
        );
        text
    }
}

fn fmt_value(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| format!("{value:.3}"))
}

fn fmt_ms(value: Option<i64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| value.to_string())
}

struct Spec {
    name: &'static str,
    unit: &'static str,
    min_sample: usize,
    better: Better,
    test: Test,
    floor: &'static str,
}

const SPECS: [Spec; 6] = [
    Spec {
        name: "failure_share",
        unit: "jobs (success+failure)",
        min_sample: 10,
        better: Better::Lower,
        test: Test::Proportion { min_abs: 0.05 },
        floor: "n>=10 success/failure jobs per window; change must exceed 2 pooled standard errors and 5 points",
    },
    Spec {
        name: "cancelled_share",
        unit: "jobs (completed)",
        min_sample: 10,
        better: Better::Lower,
        test: Test::Proportion { min_abs: 0.05 },
        floor: "n>=10 completed jobs per window; change must exceed 2 pooled standard errors and 5 points",
    },
    Spec {
        name: "starvation_share",
        unit: "jobs (runner assignment known)",
        min_sample: 10,
        better: Better::Lower,
        test: Test::Proportion { min_abs: 0.05 },
        floor: "n>=10 jobs with known runner assignment per window; change must exceed 2 pooled standard errors and 5 points",
    },
    Spec {
        name: "attempts_per_pr",
        unit: "pull requests",
        min_sample: 5,
        better: Better::Lower,
        test: Test::Relative {
            min_rel: 0.15,
            min_abs: 0.25,
        },
        floor: "n>=5 distinct pull requests per window; change must exceed 15% and 0.25 attempts",
    },
    Spec {
        name: "queue_wait_per_job_ahead_ms",
        unit: "jobs (queue-timed)",
        min_sample: 10,
        better: Better::Lower,
        test: Test::Relative {
            min_rel: 0.25,
            min_abs: 1000.0,
        },
        floor: "n>=10 jobs with provider queue timestamps per window; median must move 25% and 1s (timestamps are whole seconds)",
    },
    Spec {
        name: "cache_hit_rate",
        unit: "jobs (cache-reporting)",
        min_sample: 10,
        better: Better::Higher,
        test: Test::Proportion { min_abs: 0.05 },
        floor: "n>=10 jobs reporting cache reuse per window; change must exceed 2 pooled standard errors and 5 points",
    },
];

/// Minimum successful-duration samples per window for a wall-time verdict.
pub const WALL_MIN_SAMPLE: usize = 3;

pub(super) fn is_success(status: &str) -> bool {
    matches!(status, "pass" | "success")
}

pub(super) fn is_failure(status: &str) -> bool {
    matches!(
        status,
        "fail" | "failure" | "failed" | "timed_out" | "action_required" | "startup_failure"
    )
}

pub(super) fn is_cancelled(status: &str) -> bool {
    matches!(status, "cancelled" | "canceled")
}

#[allow(clippy::cast_precision_loss)]
fn share(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

#[allow(clippy::cast_precision_loss)]
fn median_f64(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        f64::midpoint(values[mid - 1], values[mid])
    } else {
        values[mid]
    })
}

/// `wait / (1 + jobs ahead)` for every sample with an authoritative queue
/// time and a start time. Jobs ahead are other samples queued strictly
/// earlier that had not started (or, never started, not finished) by the
/// moment this one was queued.
#[allow(clippy::cast_precision_loss)]
#[must_use]
pub fn wait_per_job_ahead_ms(samples: &[&ProxySample]) -> Vec<f64> {
    let mut out = Vec::new();
    for (index, sample) in samples.iter().enumerate() {
        let (Some(queued), Some(started)) = (sample.queued_at, sample.started_at) else {
            continue;
        };
        if sample.runner_assigned == Some(false) {
            continue;
        }
        let wait = (started - queued).num_milliseconds();
        if wait < 0 {
            continue;
        }
        let ahead = samples
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .filter(|(_, other)| {
                let Some(other_queued) = other.queued_at else {
                    return false;
                };
                if other_queued >= queued {
                    return false;
                }
                let left_queue = if other.runner_assigned == Some(false) {
                    other.completed_at
                } else {
                    other.started_at.or(other.completed_at)
                };
                left_queue.is_none_or(|left| left > queued)
            })
            .count();
        out.push(wait as f64 / (1 + ahead) as f64);
    }
    out
}

/// Measure every proxy over one window of samples.
#[must_use]
pub fn measure(samples: &[&ProxySample]) -> Vec<ProxyValue> {
    let classified = samples
        .iter()
        .filter(|sample| is_success(&sample.status) || is_failure(&sample.status))
        .count();
    let failed = samples
        .iter()
        .filter(|sample| is_failure(&sample.status))
        .count();
    let cancelled = samples
        .iter()
        .filter(|sample| is_cancelled(&sample.status))
        .count();
    let assignment_known: Vec<&&ProxySample> = samples
        .iter()
        .filter(|sample| sample.runner_assigned.is_some())
        .collect();
    let starved = assignment_known
        .iter()
        .filter(|sample| is_cancelled(&sample.status) && sample.runner_assigned == Some(false))
        .count();
    let with_pr: Vec<&(String, i64)> = samples
        .iter()
        .filter_map(|sample| sample.pr.as_ref())
        .collect();
    let distinct_prs = with_pr.iter().collect::<BTreeSet<_>>().len();
    let mut waits = wait_per_job_ahead_ms(samples);
    let wait_sample = waits.len();
    let cache: Vec<bool> = samples
        .iter()
        .filter_map(|sample| sample.cache_hit)
        .collect();
    let cache_hits = cache.iter().filter(|hit| **hit).count();

    let measured: [(Option<f64>, usize); 6] = [
        (share(failed, classified), classified),
        (share(cancelled, samples.len()), samples.len()),
        (
            share(starved, assignment_known.len()),
            assignment_known.len(),
        ),
        (share(with_pr.len(), distinct_prs), distinct_prs),
        (median_f64(&mut waits), wait_sample),
        (share(cache_hits, cache.len()), cache.len()),
    ];
    SPECS
        .iter()
        .zip(measured)
        .map(|(spec, (value, sample))| ProxyValue {
            name: spec.name,
            value,
            sample,
            min_sample: spec.min_sample,
            sufficient: sample >= spec.min_sample,
            better: spec.better,
            floor: spec.floor,
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn direction(spec: &Spec, before: &ProxyValue, after: &ProxyValue) -> Direction {
    let (Some(b), Some(a)) = (before.value, after.value) else {
        return Direction::InsufficientSample;
    };
    if !before.sufficient || !after.sufficient {
        return Direction::InsufficientSample;
    }
    let delta = a - b;
    let significant = match spec.test {
        Test::Proportion { min_abs } => {
            let (n1, n2) = (before.sample as f64, after.sample as f64);
            let pooled = (b * n1 + a * n2) / (n1 + n2);
            let se = (pooled * (1.0 - pooled) * (1.0 / n1 + 1.0 / n2)).sqrt();
            delta.abs() >= min_abs && delta.abs() > 2.0 * se
        }
        Test::Relative { min_rel, min_abs } => {
            delta.abs() >= min_abs && (b == 0.0 || (delta / b).abs() >= min_rel)
        }
    };
    if !significant {
        return Direction::Unchanged;
    }
    let better = match spec.better {
        Better::Lower => delta < 0.0,
        Better::Higher => delta > 0.0,
    };
    if better {
        Direction::Improved
    } else {
        Direction::Regressed
    }
}

fn combine(directions: impl Iterator<Item = Direction>) -> Verdict {
    let (mut improved, mut regressed, mut measured) = (false, false, false);
    for direction in directions {
        match direction {
            Direction::Improved => (improved, measured) = (true, true),
            Direction::Regressed => (regressed, measured) = (true, true),
            Direction::Unchanged => measured = true,
            Direction::InsufficientSample => {}
        }
    }
    match (measured, improved, regressed) {
        (false, _, _) => Verdict::InsufficientSample,
        (true, true, true) => Verdict::Mixed,
        (true, true, false) => Verdict::Improved,
        (true, false, true) => Verdict::Regressed,
        (true, false, false) => Verdict::Unchanged,
    }
}

fn success_durations(samples: &[&ProxySample]) -> Vec<i64> {
    let mut durations: Vec<i64> = samples
        .iter()
        .filter(|sample| is_success(&sample.status))
        .filter_map(|sample| sample.total_ms)
        .filter(|value| *value >= 0)
        .collect();
    durations.sort_unstable();
    durations
}

/// Same nearest-rank percentile the rest of the metrics store uses.
fn percentile(values: &[i64], percentile: usize) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let index = ((values.len() - 1) * percentile).div_ceil(100);
    values.get(index).copied()
}

/// Wall-clock verdict from successful-job p50, with the thresholds the store
/// has always used: 20% faster is improved, 25% slower is regressed.
fn wall_verdict(before: &[i64], after: &[i64]) -> Verdict {
    if before.len() < WALL_MIN_SAMPLE || after.len() < WALL_MIN_SAMPLE {
        return Verdict::InsufficientSample;
    }
    let (b, a) = (
        percentile(before, 50).unwrap_or_default(),
        percentile(after, 50).unwrap_or_default(),
    );
    if b > 0 && a * 100 <= b * 80 {
        Verdict::Improved
    } else if b > 0 && a * 100 >= b * 125 {
        Verdict::Regressed
    } else {
        Verdict::Unchanged
    }
}

/// Compare two windows. The headline verdict follows `basis`; both verdicts
/// are always reported.
#[must_use]
pub fn compare(before: &[&ProxySample], after: &[&ProxySample], basis: Basis) -> Comparison {
    let before_values = measure(before);
    let after_values = measure(after);
    let proxies: Vec<ProxyDelta> = SPECS
        .iter()
        .zip(before_values.iter().zip(after_values.iter()))
        .map(|(spec, (b, a))| ProxyDelta {
            name: spec.name,
            before: b.value,
            after: a.value,
            before_sample: b.sample,
            after_sample: a.sample,
            unit: spec.unit,
            min_sample: spec.min_sample,
            better: spec.better,
            direction: direction(spec, b, a),
            floor: spec.floor,
        })
        .collect();
    let proxy_verdict = combine(proxies.iter().map(|delta| delta.direction));
    let before_ms = success_durations(before);
    let after_ms = success_durations(after);
    let wall_time_verdict = wall_verdict(&before_ms, &after_ms);
    Comparison {
        basis,
        verdict: match basis {
            Basis::Proxy => proxy_verdict,
            Basis::WallTime => wall_time_verdict,
        },
        proxy_verdict,
        wall_time_verdict,
        proxies,
        context: WallContext {
            label: WALL_CONTEXT_LABEL,
            before_sample: before_ms.len(),
            after_sample: after_ms.len(),
            before_p50_ms: percentile(&before_ms, 50),
            after_p50_ms: percentile(&after_ms, 50),
            before_p90_ms: percentile(&before_ms, 90),
            after_p90_ms: percentile(&after_ms, 90),
        },
    }
}

#[cfg(test)]
mod tests;
