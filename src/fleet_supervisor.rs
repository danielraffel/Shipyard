//! Fleet **supervisor** assertions: can the thing that decides to boot a VM
//! actually see the work?
//!
//! Sibling to [`crate::fleet_service`], and deliberately the same shape: pure
//! classification, no I/O, no ambient clock, no process spawning. Callers read
//! the supervisor's log and pass the lines in with an explicit `now`. The
//! verdict taxonomy is reused wholesale — [`ServiceVerdict`] and [`Boundary`]
//! are imported, never re-declared, so a host roll-up can take the worst
//! verdict across service and supervisor assertions without translating
//! between two vocabularies.
//!
//! ## The incident
//!
//! A tartci macOS supervisor decides whether to boot a VM by scanning GitHub's
//! queue. When that scan cannot finish inside its budget the supervisor logs:
//!
//! ```text
//! SCAN BLIND (gh queue scan failed) 6/9 — NOT idling as empty (running_macos_vms=1/2)
//! SCAN BLIND ~180s — self-restarting the supervisor for fresh gh auth
//! ```
//!
//! and when it can see, it logs a cycle carrying `queued=N`:
//!
//! ```text
//! • yielding 20s (queued=2 priority_demand=2 running_macos_vms=1/2) — priority lane 'Build and Test' has the slot
//! ```
//!
//! A supervisor that cannot see the queue boots no VM, so no runner registers,
//! so every job on that lane's labels queues forever — and **nothing in GitHub
//! shows a fault**. The jobs are simply `queued`, which is also what they look
//! like one second after being created. It cost roughly a day, twice.
//!
//! ## Three separate root causes, three separate assertions
//!
//! **1. Blindness is a ratio over a window, never a boolean and never a single
//! sample.** Measured on the live release supervisor: of its last 2000 log
//! lines, 1598 were `SCAN BLIND` and 80 carried `queued=`; the last blind line
//! was about seventy lines from the end, and that tail read entirely healthy.
//! *Sampled at that instant the supervisor is green.* A "is it blind right
//! now?" check passes. So [`assess_supervisor_scan`] counts blind against
//! sighted cycles across the whole window and reports the fraction against a
//! budget, and it additionally reads the supervisor's own consecutive-blind
//! counter out of `N/9` — a burst of nine in a row is a different and worse
//! fact than nine scattered across a day, and one signal cannot stand in for
//! the other.
//!
//! **2. The message names the wrong subsystem.** `gh queue scan failed` and
//! `self-restarting the supervisor for fresh gh auth` both point at
//! authentication. Authentication was never broken; the scan timed out. The
//! supervisor then performed an auth restart that could not possibly help, and
//! every human reading the log went to the same wrong place.
//! [`classify_scan_failure`] therefore maps a timeout to [`Boundary::Transport`]
//! — whose `next_action()` already says not to re-authenticate in response —
//! and reaches [`Boundary::Permission`] only on actual credential evidence
//! (`HTTP 401`, `Bad credentials`). The literal word "auth" in the supervisor's
//! own remediation sentence is deliberately not credential evidence.
//!
//! **3. A budget set by absence is the shape of the bug.** The scan budget
//! defaults to `TARTCI_GH_TIMEOUT_SECS`, or [`DEFAULT_SCAN_TIMEOUT_SECS`] when
//! that is unset. The fleet gate lane raises its own to
//! `TARTCI_ASSIGNMENT_SCAN_TIMEOUT_SECS=180` in its plist; the release
//! supervisor plists declare no scan timeout at all. The identical proxy
//! latency is therefore absorbed by one lane and fatal to the other, and
//! nothing in either lane's configuration mentions the difference — one of them
//! simply does not say. [`assess_scan_budget`] takes the declared timeout as an
//! `Option`, where `None` means *inherits by omission*, and reports observed
//! latency as a fraction of whichever budget applies rather than collapsing it
//! to pass/fail.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::fleet_service::{Boundary, ServiceVerdict};

/// Scan budget, in seconds, a tartci supervisor inherits when neither its lane
/// nor `TARTCI_GH_TIMEOUT_SECS` declares one.
///
/// This is the number the release supervisors run on without ever naming it,
/// and it is twelve times smaller than the 180s the gate lane declares for the
/// same query against the same API through the same proxy.
pub const DEFAULT_SCAN_TIMEOUT_SECS: i64 = 15;

/// Fraction of scan cycles in a window allowed to be blind before the
/// supervisor is reported as [`ServiceVerdict::Degraded`].
///
/// Deliberately small. A blind cycle is a cycle in which the supervisor may
/// have declined to boot a VM that work was waiting for, so a few percent is
/// already a lane running on luck.
pub const DEFAULT_MAX_BLIND_RATIO: f64 = 0.05;

/// Consecutive blind cycles tolerated before the run itself raises,
/// independently of the ratio.
pub const DEFAULT_MAX_CONSECUTIVE_BLIND: usize = 3;

/// Fraction of the scan budget observed latency may reach before the lane is
/// reported as consuming its budget rather than living inside it.
pub const DEFAULT_BUDGET_PRESSURE_RATIO: f64 = 0.8;

/// Tunables for [`assess_supervisor_scan`] and [`assess_scan_budget`].
///
/// There is deliberately **no minimum window size**. A "measure the ratio only
/// once you have N cycles" guard is exactly the escape hatch that lets a short
/// but entirely blind window read as a pass, and inventing a blind spot inside
/// a blindness detector is not a trade worth making. A one-cycle window that
/// was blind reports one of one blind, which is the truth.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanThresholds {
    /// Blind-to-total cycle fraction tolerated across the window.
    pub max_blind_ratio: f64,
    /// Longest run of adjacent blind cycles tolerated.
    pub max_consecutive_blind: usize,
    /// Budget assumed for a lane that declares no scan timeout.
    pub default_scan_timeout_secs: i64,
    /// Fraction of the scan budget observed latency may reach before raising.
    pub budget_pressure_ratio: f64,
}

impl Default for ScanThresholds {
    fn default() -> Self {
        Self {
            max_blind_ratio: DEFAULT_MAX_BLIND_RATIO,
            max_consecutive_blind: DEFAULT_MAX_CONSECUTIVE_BLIND,
            default_scan_timeout_secs: DEFAULT_SCAN_TIMEOUT_SECS,
            budget_pressure_ratio: DEFAULT_BUDGET_PRESSURE_RATIO,
        }
    }
}

/// What a window of supervisor log lines says about its scan cycles.
///
/// Every field is a count or a sample, never a verdict: parsing and judgement
/// are split so a caller can widen the window, re-judge with different
/// thresholds, or serialise the raw measurement alongside the conclusion.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SupervisorScanWindow {
    /// How many log lines were offered, including ones that were not cycles.
    pub lines_scanned: usize,
    /// Cycles in which the supervisor could not read the queue.
    pub blind_cycles: usize,
    /// Cycles in which it could — the ones carrying `queued=`.
    pub sighted_cycles: usize,
    /// Lines that were neither. Recorded so "this window contained no cycles
    /// at all" is distinguishable from "this window was empty".
    pub unrecognized_lines: usize,
    /// Longest run of adjacent blind cycles observed in the window.
    ///
    /// Counted from the cycle sequence, independently of whatever counter the
    /// supervisor printed — two instruments for one fact, because the printed
    /// counter resets on restart and the sequence does not.
    pub max_consecutive_blind: usize,
    /// Blind run still open at the end of the window. Zero when the window
    /// ends healthy, which is precisely the reading that made a tail sample
    /// look green while the window was three-quarters blind.
    pub trailing_consecutive_blind: usize,
    /// Highest consecutive-blind count the supervisor printed itself, read out
    /// of the `N/9` field.
    pub reported_consecutive_blind: Option<usize>,
    /// The ceiling in that same `N/9` field — the count at which the
    /// supervisor self-restarts.
    pub consecutive_ceiling: Option<usize>,
    /// Scan latencies, in seconds, carried by lines that stated one.
    pub observed_latencies_secs: Vec<i64>,
    /// Boundary the supervisor's own scan failures classify into.
    ///
    /// Distinct from a report's `boundary`, which says why *this assertion*
    /// could not measure. This one says what the *supervisor* hit, and its
    /// whole purpose is to read `transport` on a log that says "auth".
    pub observed_boundary: Option<Boundary>,
}

impl SupervisorScanWindow {
    /// Recognised scan cycles, blind plus sighted.
    #[must_use]
    pub fn total_cycles(&self) -> usize {
        self.blind_cycles + self.sighted_cycles
    }

    /// Fraction of recognised cycles that were blind.
    ///
    /// `None` when no cycle was recognised: a window with no denominator has
    /// no ratio, and returning zero there would fabricate a pass.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn blind_ratio(&self) -> Option<f64> {
        let total = self.total_cycles();
        if total == 0 {
            return None;
        }
        Some(self.blind_cycles as f64 / total as f64)
    }

    /// Worst scan latency observed, if any line stated one.
    #[must_use]
    pub fn worst_latency_secs(&self) -> Option<i64> {
        self.observed_latencies_secs.iter().copied().max()
    }
}

/// Whether a log line is a scan cycle that could not read the queue.
#[must_use]
pub fn is_blind_cycle(line: &str) -> bool {
    line.to_ascii_lowercase().contains("scan blind")
}

/// Whether a log line is a scan cycle that did read the queue.
///
/// The `queued=` field is the tell: the supervisor can only print a queue
/// depth it actually obtained.
#[must_use]
pub fn is_sighted_cycle(line: &str) -> bool {
    line.contains("queued=")
}

/// Classify a supervisor log line's failure into the boundary it actually hit.
///
/// Returns `None` for a line that is not a failure at all.
///
/// The distinction this exists for: `SCAN BLIND ~180s — self-restarting the
/// supervisor for fresh gh auth` is a **timeout**, and maps to
/// [`Boundary::Transport`]. It mentions authentication only because the
/// supervisor's remediation is an auth restart, and that remediation is the
/// bug. So the word "auth" is not credential evidence here; `HTTP 401`,
/// `Bad credentials` and an explicit `gh auth login` prompt are. A rate limit
/// is likewise transport rather than permission, however much a `403` looks
/// like the latter.
#[must_use]
pub fn classify_scan_failure(line: &str) -> Option<Boundary> {
    let lower = line.to_ascii_lowercase();
    if !looks_like_scan_failure(&lower) {
        return None;
    }
    if has_credential_evidence(&lower) {
        return Some(Boundary::Permission);
    }
    Some(Boundary::Transport)
}

/// Whether a line reports a scan that did not deliver an answer.
fn looks_like_scan_failure(lower: &str) -> bool {
    const MARKERS: [&str; 9] = [
        "scan blind",
        "scan failed",
        "timed out",
        "timeout",
        "deadline exceeded",
        "bad credentials",
        "http 401",
        "http 403",
        "requires authentication",
    ];
    MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Whether a line carries evidence that the *credential* — not the call — was
/// the thing that failed.
fn has_credential_evidence(lower: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "bad credentials",
        "http 401",
        "http 403",
        "requires authentication",
        "authentication failed",
        "gh auth login",
    ];
    // A rate limit is a completed-call fact, and re-authenticating in response
    // to one is the same wrong move this module exists to stop.
    if lower.contains("rate limit") {
        return false;
    }
    MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Read the supervisor's own consecutive-blind counter out of an `N/M` field.
///
/// The trap this guards: the same line carries a second fraction,
/// `running_macos_vms=1/2`, and a naive scan for "a number over a number"
/// happily reports the VM census as the blind counter. Only a bare token — one
/// with no `=` in it — is the counter.
fn parse_blind_counter(line: &str) -> Option<(usize, usize)> {
    line.split_whitespace().find_map(|token| {
        if token.contains('=') {
            return None;
        }
        let trimmed = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '/');
        let (seen, ceiling) = trimmed.split_once('/')?;
        Some((seen.parse().ok()?, ceiling.parse().ok()?))
    })
}

/// Read a stated scan latency, in seconds, out of a log line.
///
/// A marker is required — `~180s`, or an explicit `scan=…s`. The healthy cycle
/// line also contains `yielding 20s`, which is a *sleep* the supervisor chose,
/// not a latency it suffered; reading that as a scan time would report the
/// healthiest lines as the slowest.
fn parse_latency_secs(line: &str) -> Option<i64> {
    line.split_whitespace().find_map(|token| {
        let raw = token.trim_matches(|c: char| c == '(' || c == ')' || c == ',');
        let digits = raw
            .strip_prefix('~')
            .or_else(|| raw.strip_prefix("scan="))
            .or_else(|| raw.strip_prefix("scan_secs="))?;
        digits.strip_suffix('s').unwrap_or(digits).parse().ok()
    })
}

/// Fold a newly observed boundary into whatever the window already carries.
///
/// Real credential evidence outranks a timeout: a window containing one
/// genuine `401` among a thousand timeouts has an auth problem *as well*, and
/// that is the fact worth surfacing. Everything else defers to the incumbent.
fn escalate_boundary(previous: Boundary, next: Boundary) -> Boundary {
    if previous == Boundary::Transport {
        next
    } else {
        previous
    }
}

/// Extract scan-cycle counts, runs, counters and latencies from raw log lines.
///
/// `lines` is expected in chronological order, oldest first, which is what
/// makes `max_consecutive_blind` and `trailing_consecutive_blind` meaningful.
/// Lines that are neither cycle shape are counted but do not break a blind
/// run: an unrelated log line between two blind scans does not constitute a
/// successful scan between them.
#[must_use]
pub fn parse_scan_window(lines: &[&str]) -> SupervisorScanWindow {
    let mut window = SupervisorScanWindow {
        lines_scanned: lines.len(),
        ..SupervisorScanWindow::default()
    };
    let mut run = 0usize;

    for line in lines {
        if is_blind_cycle(line) {
            window.blind_cycles += 1;
            run += 1;
            window.max_consecutive_blind = window.max_consecutive_blind.max(run);
            absorb_blind_line(&mut window, line);
        } else if is_sighted_cycle(line) {
            window.sighted_cycles += 1;
            run = 0;
            if let Some(secs) = parse_latency_secs(line) {
                window.observed_latencies_secs.push(secs);
            }
        } else {
            window.unrecognized_lines += 1;
        }
    }

    window.trailing_consecutive_blind = run;
    window
}

/// Record the counter, latency and boundary a single blind line carries.
fn absorb_blind_line(window: &mut SupervisorScanWindow, line: &str) {
    if let Some((seen, ceiling)) = parse_blind_counter(line) {
        window.reported_consecutive_blind = Some(
            window
                .reported_consecutive_blind
                .map_or(seen, |previous| previous.max(seen)),
        );
        window.consecutive_ceiling = Some(ceiling);
    }
    if let Some(secs) = parse_latency_secs(line) {
        window.observed_latencies_secs.push(secs);
    }
    if let Some(boundary) = classify_scan_failure(line) {
        window.observed_boundary = Some(
            window
                .observed_boundary
                .map_or(boundary, |previous| escalate_boundary(previous, boundary)),
        );
    }
}

/// Verdict for one supervisor's scan window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SupervisorReport {
    /// The verdict.
    pub verdict: ServiceVerdict,
    /// Why *this assertion* could not measure. Always `Some` when the verdict
    /// is [`ServiceVerdict::Unknown`], and always `None` otherwise.
    pub boundary: Option<Boundary>,
    /// Why the *supervisor's own* scans failed, when any did. Carried at every
    /// verdict, because a degraded-but-serving supervisor still needs its
    /// failures pointed at the right subsystem.
    pub observed_boundary: Option<Boundary>,
    /// Log lines the window was drawn from.
    pub lines_scanned: usize,
    /// Blind cycles in the window.
    pub blind_cycles: usize,
    /// Sighted cycles in the window.
    pub sighted_cycles: usize,
    /// Blind fraction, `None` when no cycle was recognised.
    pub blind_ratio: Option<f64>,
    /// Longest observed run of adjacent blind cycles.
    pub max_consecutive_blind: usize,
    /// Blind run still open at the end of the window.
    pub trailing_consecutive_blind: usize,
    /// Highest consecutive-blind count the supervisor printed.
    pub reported_consecutive_blind: Option<usize>,
    /// The self-restart ceiling the supervisor printed alongside it.
    pub consecutive_ceiling: Option<usize>,
    /// When the assertion was made.
    pub observed_at: DateTime<Utc>,
    /// Operator-facing explanation naming what was measured and what to do.
    pub detail: String,
}

/// Judge a parsed scan window: is this supervisor seeing the work?
///
/// Two independent signals, either of which raises:
///
/// * the **ratio** of blind to total cycles across the whole window, which is
///   the only one that catches a supervisor whose recent tail reads healthy;
/// * the longest **run** of adjacent blind cycles, and the supervisor's own
///   printed counter against its self-restart ceiling, which catch a short
///   total outage that a long window's ratio would dilute below the budget.
///
/// A window containing no recognisable cycle is [`ServiceVerdict::Unknown`]
/// with a named boundary, never a pass. An instrument that read nothing has
/// not observed health.
#[must_use]
pub fn assess_supervisor_scan(
    window: &SupervisorScanWindow,
    thresholds: ScanThresholds,
    now: DateTime<Utc>,
) -> SupervisorReport {
    let mut report = SupervisorReport {
        verdict: ServiceVerdict::Unknown,
        boundary: None,
        observed_boundary: window.observed_boundary,
        lines_scanned: window.lines_scanned,
        blind_cycles: window.blind_cycles,
        sighted_cycles: window.sighted_cycles,
        blind_ratio: window.blind_ratio(),
        max_consecutive_blind: window.max_consecutive_blind,
        trailing_consecutive_blind: window.trailing_consecutive_blind,
        reported_consecutive_blind: window.reported_consecutive_blind,
        consecutive_ceiling: window.consecutive_ceiling,
        observed_at: now,
        detail: String::new(),
    };

    let Some(ratio) = window.blind_ratio() else {
        report.boundary = Some(Boundary::Parse);
        report.detail = format!(
            "no supervisor scan cycle recognised in {} log line(s): neither a `SCAN BLIND` nor a \
             `queued=` cycle was present, so nothing was asserted about this supervisor. {}",
            window.lines_scanned,
            Boundary::Parse.next_action()
        );
        return report;
    };

    let ratio_exceeded = ratio > thresholds.max_blind_ratio;
    let run_exceeded = window.max_consecutive_blind > thresholds.max_consecutive_blind;
    let ceiling_reached = window
        .reported_consecutive_blind
        .zip(window.consecutive_ceiling)
        .is_some_and(|(seen, ceiling)| ceiling > 0 && seen >= ceiling);

    if ratio_exceeded || run_exceeded || ceiling_reached {
        report.verdict = ServiceVerdict::Degraded;
        report.detail = degraded_detail(
            window,
            thresholds,
            ratio,
            (ratio_exceeded, run_exceeded, ceiling_reached),
        );
    } else {
        report.verdict = ServiceVerdict::Served;
        report.detail = served_detail(window, thresholds, ratio);
    }
    report
}

/// Compose the operator-facing message for a raising supervisor window.
fn degraded_detail(
    window: &SupervisorScanWindow,
    thresholds: ScanThresholds,
    ratio: f64,
    signals: (bool, bool, bool),
) -> String {
    let (ratio_exceeded, run_exceeded, ceiling_reached) = signals;
    let mut parts: Vec<String> = Vec::new();

    if ratio_exceeded {
        parts.push(format!(
            "{} of {} scan cycles blind ({:.1}%, budget {:.1}%)",
            window.blind_cycles,
            window.total_cycles(),
            ratio * 100.0,
            thresholds.max_blind_ratio * 100.0
        ));
    }
    if run_exceeded {
        parts.push(format!(
            "longest blind run {} (budget {})",
            window.max_consecutive_blind, thresholds.max_consecutive_blind
        ));
    }
    if ceiling_reached {
        parts.push(format!(
            "supervisor's own counter reached {}/{} — its self-restart ceiling",
            window.reported_consecutive_blind.unwrap_or_default(),
            window.consecutive_ceiling.unwrap_or_default()
        ));
    }

    let tail = if window.trailing_consecutive_blind == 0 {
        " The window ENDS healthy, so a tail sample of this supervisor reads green; the ratio \
         above is the only signal that does not."
    } else {
        ""
    };

    format!(
        "supervisor is not reliably seeing the queue: {}.{tail} A blind cycle may have declined to \
         boot a VM that work was waiting on, and the resulting jobs stay `queued` with no fault \
         visible anywhere in GitHub. Next: {}",
        parts.join("; "),
        window
            .observed_boundary
            .unwrap_or(Boundary::Transport)
            .next_action()
    )
}

/// Compose the operator-facing message for a supervisor within its budgets.
fn served_detail(window: &SupervisorScanWindow, thresholds: ScanThresholds, ratio: f64) -> String {
    if window.blind_cycles == 0 {
        return format!(
            "{} scan cycle(s), none blind — the supervisor read the queue on every cycle in this \
             window",
            window.total_cycles()
        );
    }
    format!(
        "{} of {} scan cycles blind ({:.1}%), inside the {:.1}% budget; longest blind run {} \
         (budget {})",
        window.blind_cycles,
        window.total_cycles(),
        ratio * 100.0,
        thresholds.max_blind_ratio * 100.0,
        window.max_consecutive_blind,
        thresholds.max_consecutive_blind
    )
}

/// Verdict for one lane's scan-timeout budget against what it actually costs.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScanBudgetReport {
    /// The lane the budget belongs to.
    pub lane: String,
    /// The timeout the lane declares, or `None` when it declares none.
    pub declared_timeout_secs: Option<i64>,
    /// The budget actually in force.
    pub effective_budget_secs: i64,
    /// Whether that budget arrived by omission rather than by decision.
    pub inherits_default: bool,
    /// Worst latency observed against it.
    pub worst_latency_secs: Option<i64>,
    /// Worst latency as a fraction of the budget. Reported rather than
    /// collapsed to pass/fail: the release lane and the gate lane observe the
    /// same seconds and differ only in this number.
    pub worst_budget_ratio: Option<f64>,
    /// How many samples reached or exceeded the budget outright.
    pub over_budget_samples: usize,
    /// How many samples were measured.
    pub sample_count: usize,
    /// The verdict.
    pub verdict: ServiceVerdict,
    /// Why the assertion could not measure. `Some` exactly when the verdict is
    /// [`ServiceVerdict::Unknown`].
    pub boundary: Option<Boundary>,
    /// Operator-facing explanation.
    pub detail: String,
}

/// Judge a lane's scan budget against the latencies observed under it.
///
/// `declared_timeout_secs` of `None` means the lane names no scan timeout and
/// therefore inherits [`ScanThresholds::default_scan_timeout_secs`]. That
/// distinction is the whole assertion: the gate lane declares 180s and the
/// release lane declares nothing, so the same proxy latency is comfortable in
/// one and fatal in the other, and neither lane's configuration says so.
///
/// Budget pressure raises for a declared budget too — a lane observing 175s
/// against a declared 180s is in trouble whether or not it chose the number.
/// Omission changes the *message*, not the rule; what it changes in practice is
/// the size of the budget, and a twelvefold difference is enough to decide the
/// verdict on its own.
#[must_use]
pub fn assess_scan_budget(
    lane: &str,
    declared_timeout_secs: Option<i64>,
    observed_latencies_secs: &[i64],
    thresholds: ScanThresholds,
) -> ScanBudgetReport {
    let inherits_default = declared_timeout_secs.is_none();
    let effective = declared_timeout_secs.unwrap_or(thresholds.default_scan_timeout_secs);
    let mut report = ScanBudgetReport {
        lane: lane.to_owned(),
        declared_timeout_secs,
        effective_budget_secs: effective,
        inherits_default,
        worst_latency_secs: observed_latencies_secs.iter().copied().max(),
        worst_budget_ratio: None,
        over_budget_samples: observed_latencies_secs
            .iter()
            .filter(|secs| **secs >= effective)
            .count(),
        sample_count: observed_latencies_secs.len(),
        verdict: ServiceVerdict::Unknown,
        boundary: None,
        detail: String::new(),
    };

    if effective <= 0 {
        report.boundary = Some(Boundary::Parse);
        report.detail = format!(
            "lane `{lane}` resolves to a non-positive scan budget of {effective}s — no latency \
             claim can be made against it. {}",
            Boundary::Parse.next_action()
        );
        return report;
    }

    let Some(worst) = report.worst_latency_secs else {
        report.boundary = Some(Boundary::Parse);
        report.detail = format!(
            "lane `{lane}` declares a {effective}s scan budget but the window stated no scan \
             latency, so nothing was asserted about it. A budget nothing was measured against is \
             not a budget that was met."
        );
        return report;
    };

    #[allow(clippy::cast_precision_loss)]
    let budget_ratio = worst as f64 / effective as f64;
    report.worst_budget_ratio = Some(budget_ratio);
    finish_budget_verdict(&mut report, thresholds, worst, budget_ratio);
    report
}

/// Decide and explain a budget verdict whose ratio has already been computed.
fn finish_budget_verdict(
    report: &mut ScanBudgetReport,
    thresholds: ScanThresholds,
    worst: i64,
    budget_ratio: f64,
) {
    let source = if report.inherits_default {
        format!(
            "inherited by omission ({}s — the lane declares no scan timeout)",
            report.effective_budget_secs
        )
    } else {
        format!("declared {}s", report.effective_budget_secs)
    };

    if budget_ratio >= 1.0 {
        report.verdict = ServiceVerdict::Degraded;
        report.detail = format!(
            "lane `{}` scan budget {source}: worst observed latency {worst}s is {:.0}% of it, and \
             {} of {} samples reached it. Scans at or past the budget go blind, and a blind \
             supervisor boots no VM while its jobs sit `queued` with no visible fault. {}",
            report.lane,
            budget_ratio * 100.0,
            report.over_budget_samples,
            report.sample_count,
            over_budget_remedy(report.inherits_default)
        );
        return;
    }

    if budget_ratio >= thresholds.budget_pressure_ratio {
        report.verdict = ServiceVerdict::Degraded;
        report.detail = format!(
            "lane `{}` scan budget {source}: worst observed latency {worst}s is {:.0}% of it \
             (pressure threshold {:.0}%). Nothing has failed yet; the margin has. {}",
            report.lane,
            budget_ratio * 100.0,
            thresholds.budget_pressure_ratio * 100.0,
            over_budget_remedy(report.inherits_default)
        );
        return;
    }

    report.verdict = ServiceVerdict::Served;
    report.detail = format!(
        "lane `{}` scan budget {source}: worst observed latency {worst}s is {:.0}% of it across {} \
         sample(s)",
        report.lane,
        budget_ratio * 100.0,
        report.sample_count
    );
}

/// What to do about a budget the observed latency is consuming.
fn over_budget_remedy(inherits_default: bool) -> &'static str {
    if inherits_default {
        "Next: declare this lane's scan timeout explicitly rather than letting it inherit the \
         default — a sibling lane already raises its own, which is why the identical latency is \
         harmless there. This is a timeout, not a credential fault; do not re-authenticate in \
         response to it."
    } else {
        "Next: raise the declared budget or reduce the scan's cost. This is a timeout, not a \
         credential fault; do not re-authenticate in response to it."
    }
}

#[cfg(test)]
mod tests;
