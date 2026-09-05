//! Fleet **relay** assertions: does every *declared* SSH-relay hop connect
//! inside its budget, in the order it is declared?
//!
//! Sibling to [`crate::fleet_service`] and [`crate::fleet_supervisor`], and
//! deliberately the same shape: pure classification, no I/O, no ambient clock,
//! no process spawning. The caller runs the probes — one per declared hop, in
//! declared order — and passes the measurements in with an explicit `now`. The
//! verdict taxonomy is imported wholesale, never re-declared, so a host roll-up
//! can take the worst verdict across service, supervisor and relay assertions
//! without translating between vocabularies.
//!
//! ## The incident
//!
//! A local `http_connect_ssh_relay` is invoked with an ordered hop list —
//! `--relay-host macmini --relay-host m1`. `macmini` was unreachable, so every
//! connection paid the failed **first** hop before falling back to the second:
//!
//! | host | via proxy | direct |
//! |---|---|---|
//! | M3 (dead first hop) | 18s, then timeout | 2s |
//! | M5 (relay working, fallback tax) | 5.5s | 0.2s |
//!
//! **The relay still worked.** "Does the proxy answer?" was GREEN the entire
//! time, because the fallback succeeded — which is exactly why nobody looked at
//! the relay. What the dead hop cost was a *latency tax*, and that tax silently
//! exceeded a **downstream** budget: a supervisor whose queue scan inherits a
//! 15s timeout (see [`crate::fleet_supervisor::DEFAULT_SCAN_TIMEOUT_SECS`])
//! went blind, booted no VM, and a release lane starved for roughly a day.
//! Twice.
//!
//! So the assertion is per **declared hop**, in order — never "does the proxy
//! answer". A single aggregate answer cannot see a dead hop that something
//! downstream is silently paying for.
//!
//! ## Three things this module refuses to collapse
//!
//! **1. Position, because the tax depends on it.** A hop that fails *before*
//! the first hop that answers is paid by every single connection. The identical
//! hop failing *after* it is never reached and costs nothing today. Same defect,
//! two different facts, so [`HopReport::attempted`] and the reported tax
//! separate a live latency tax from a fallback that has already been lost.
//!
//! Both raise. `Idle` in this taxonomy means nothing is asking, and a relay in
//! use is being asked, so an unreachable fallback is not at rest — it is
//! redundancy gone, silently, which is precisely the shape of the incident
//! behind this module. What differs is the message, chosen by what the defect
//! costs rather than by the verdict.
//!
//! **2. The connect time, into pass/fail.** A hop at 4.9s of a 5s budget and a
//! hop at 0.2s are not the same lane, and the first is one bad afternoon from
//! being the incident. [`HopReport::budget_ratio`] is reported at *every*
//! verdict — the same discipline
//! [`crate::fleet_supervisor::assess_scan_budget`] applies to scan latency.
//!
//! **3. An unmeasured hop, into a healthy one.** A probe that could not run is
//! [`ServiceVerdict::Unknown`] with a named [`Boundary`], never a pass. The
//! reason is specific and embarrassing: diagnosis took a day because every
//! attempted reproduction used `env -i`, which strips the `*_proxy` variables —
//! so the control ran without the very relay it was controlling for, and came
//! back clean. A probe must run in the real environment or say that it did not.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::fleet_service::{Boundary, ServiceVerdict};

/// Seconds a single declared hop is allowed to take to connect.
///
/// Sized against what a hop costs when the relay is healthy — measured across
/// the live fleet at 0.16s, 0.19s and 0.27s — not against what a downstream
/// consumer can survive. A budget set by what still happens to work is how the
/// 15s supervisor scan came to be the thing that noticed.
pub const DEFAULT_HOP_BUDGET_SECS: f64 = 5.0;

/// Tunables for [`assess_relay`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RelayThresholds {
    /// Seconds one declared hop may take to connect before it is over budget.
    pub hop_budget_secs: f64,
}

impl Default for RelayThresholds {
    fn default() -> Self {
        Self {
            hop_budget_secs: DEFAULT_HOP_BUDGET_SECS,
        }
    }
}

/// What a single hop probe observed.
///
/// Raw measurement only: whether the budget was met is a judgement, and it is
/// made in [`assess_relay`] so the same measurement can be re-judged against a
/// different budget without re-probing. Elapsed time is carried by every
/// failure mode as well as by success, because the failures are what the tax is
/// made of.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeOutcome {
    /// The hop accepted a connection after this many seconds.
    Connected {
        /// Wall-clock seconds until the connection was established.
        elapsed_secs: f64,
    },
    /// The host answered and refused the port.
    Refused {
        /// Wall-clock seconds spent before the refusal.
        elapsed_secs: f64,
    },
    /// The host never answered; the probe hit its own deadline.
    TimedOut {
        /// Wall-clock seconds spent before giving up.
        elapsed_secs: f64,
    },
    /// The hop's name did not resolve.
    Unresolved {
        /// Wall-clock seconds spent in resolution.
        elapsed_secs: f64,
    },
    /// The probe itself did not run or could not be trusted.
    ///
    /// Distinct from every failure above: those are facts about the hop, this
    /// is the absence of a fact. Folding it into a pass is the failure mode
    /// these assertions exist to end.
    Unmeasured {
        /// Why the probe could not measure.
        boundary: Boundary,
    },
}

impl ProbeOutcome {
    /// Seconds the probe spent, when it spent a measurable amount.
    #[must_use]
    pub fn elapsed_secs(self) -> Option<f64> {
        match self {
            Self::Connected { elapsed_secs }
            | Self::Refused { elapsed_secs }
            | Self::TimedOut { elapsed_secs }
            | Self::Unresolved { elapsed_secs } => Some(elapsed_secs),
            Self::Unmeasured { .. } => None,
        }
    }

    /// Whether the hop accepted a connection at all, budget aside.
    ///
    /// This is the fact that stops the fallback chain: a hop that answers
    /// slowly still answers, so nothing after it is ever tried.
    #[must_use]
    pub fn connected(self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    /// The boundary that stopped the probe, when one did.
    #[must_use]
    pub fn boundary(self) -> Option<Boundary> {
        match self {
            Self::Unmeasured { boundary } => Some(boundary),
            _ => None,
        }
    }
}

/// One declared hop and what probing it observed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HopProbe {
    /// Host as the relay declares it, in declared order.
    pub host: String,
    /// What the probe saw.
    pub outcome: ProbeOutcome,
}

impl HopProbe {
    /// A hop that accepted a connection after `elapsed_secs`.
    #[must_use]
    pub fn connected(host: &str, elapsed_secs: f64) -> Self {
        Self {
            host: host.to_owned(),
            outcome: ProbeOutcome::Connected { elapsed_secs },
        }
    }

    /// A hop that answered and refused the port.
    #[must_use]
    pub fn refused(host: &str, elapsed_secs: f64) -> Self {
        Self {
            host: host.to_owned(),
            outcome: ProbeOutcome::Refused { elapsed_secs },
        }
    }

    /// A hop that never answered.
    #[must_use]
    pub fn timed_out(host: &str, elapsed_secs: f64) -> Self {
        Self {
            host: host.to_owned(),
            outcome: ProbeOutcome::TimedOut { elapsed_secs },
        }
    }

    /// A hop whose name did not resolve.
    #[must_use]
    pub fn unresolved(host: &str, elapsed_secs: f64) -> Self {
        Self {
            host: host.to_owned(),
            outcome: ProbeOutcome::Unresolved { elapsed_secs },
        }
    }

    /// A hop the probe could not measure, and why.
    #[must_use]
    pub fn unmeasured(host: &str, boundary: Boundary) -> Self {
        Self {
            host: host.to_owned(),
            outcome: ProbeOutcome::Unmeasured { boundary },
        }
    }
}

/// A hop's measurement judged against its budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HopOutcome {
    /// Connected, inside the budget.
    WithinBudget,
    /// Connected, but took at least the whole budget to do it.
    OverBudget,
    /// The host refused the port.
    Refused,
    /// The host never answered.
    TimedOut,
    /// The name did not resolve.
    Unresolved,
    /// The probe could not measure this hop.
    Unmeasurable,
}

impl HopOutcome {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WithinBudget => "within_budget",
            Self::OverBudget => "over_budget",
            Self::Refused => "refused",
            Self::TimedOut => "timed_out",
            Self::Unresolved => "unresolved",
            Self::Unmeasurable => "unmeasurable",
        }
    }

    /// Whether the hop answered at all — and therefore ended the fallback
    /// chain, whatever it cost to do so.
    #[must_use]
    pub fn connects(self) -> bool {
        matches!(self, Self::WithinBudget | Self::OverBudget)
    }

    /// What to do about this hop, phrased as an action rather than a diagnosis.
    #[must_use]
    pub fn remedy(self) -> &'static str {
        match self {
            Self::WithinBudget => {
                "nothing — this hop answers inside its budget; keep watching the ratio, not the \
                 boolean"
            }
            Self::OverBudget => {
                "reduce this hop's connect cost, or raise the declared budget deliberately. Until \
                 one of those happens, every connection routed through it pays the difference, and \
                 the first thing to notice will be a downstream timeout that names something else"
            }
            Self::Refused => {
                "the host answered and refused the port — restore its listener, or take it out of \
                 the hop list; while it sits ahead of a working hop it is pure tax"
            }
            Self::TimedOut => {
                "the host did not answer — restore it or remove it from the hop list. Ahead of a \
                 working hop it costs every connection its full timeout, and the relay keeps \
                 reporting success"
            }
            Self::Unresolved => {
                "the hop's name does not resolve — fix the name or the resolver. An unresolvable \
                 hop can never serve; it can only tax"
            }
            Self::Unmeasurable => {
                "re-run the probe in the real environment. Do NOT reproduce with `env -i`: it \
                 strips the `*_proxy` variables, so the control runs without the relay it is \
                 controlling for and comes back clean"
            }
        }
    }
}

/// Verdict for one declared hop, at its declared position.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HopReport {
    /// Host as declared.
    pub host: String,
    /// 1-based position in the declared hop order.
    pub position: usize,
    /// How many hops the relay declares in total.
    pub hop_count: usize,
    /// The measurement judged against the budget.
    pub outcome: HopOutcome,
    /// Whether a connection actually reaches this hop.
    ///
    /// True for every hop up to and including the first one that answers.
    /// False for a hop behind it, which is never tried — and this single bit is
    /// what separates a defect that taxes every connection from a defect that
    /// costs nothing today.
    pub attempted: bool,
    /// Seconds the probe measured, when it measured any.
    pub connect_secs: Option<f64>,
    /// Budget in force for this hop.
    pub budget_secs: f64,
    /// Observed time as a fraction of the budget. Reported at every verdict,
    /// never collapsed to pass/fail.
    pub budget_ratio: Option<f64>,
    /// The verdict.
    pub verdict: ServiceVerdict,
    /// Why this hop could not be measured. `Some` exactly when the verdict is
    /// [`ServiceVerdict::Unknown`].
    pub boundary: Option<Boundary>,
    /// What to do about this hop.
    pub remedy: String,
    /// Operator-facing explanation naming what was measured.
    pub detail: String,
}

/// Verdict for one relay's whole declared hop chain.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RelayReport {
    /// Name of the relay this chain belongs to.
    pub relay: String,
    /// Hosts in declared order.
    pub declared_hops: Vec<String>,
    /// Per-hop verdicts, in declared order.
    pub hops: Vec<HopReport>,
    /// The verdict.
    pub verdict: ServiceVerdict,
    /// Why the assertion could not measure. `Some` exactly when the verdict is
    /// [`ServiceVerdict::Unknown`].
    pub boundary: Option<Boundary>,
    /// Budget each hop was judged against.
    pub hop_budget_secs: f64,
    /// 1-based position of the first hop that answers, when one does.
    pub first_answering_position: Option<usize>,
    /// Seconds every connection pays to failing hops **before** reaching the
    /// first hop that answers.
    ///
    /// This is the incident, as a number: 18s on the dead-first-hop path while
    /// the relay reported success the whole time. `None` when no hop answers —
    /// then nothing is a tax, because nothing gets through.
    pub tax_secs: Option<f64>,
    /// Operator-facing explanation naming what was measured and what it costs.
    pub detail: String,
    /// When the assertion was made.
    pub observed_at: DateTime<Utc>,
}

impl RelayReport {
    /// Whether at least one declared hop answered.
    ///
    /// This is the question the relay was judged by throughout the incident,
    /// and it was **true the entire time** — the fallback succeeded, so the
    /// proxy answered, so the relay looked fine while a dead first hop taxed
    /// every connection into a downstream timeout.
    ///
    /// It is offered as a *fact*, never as a verdict. Gate on
    /// [`RelayReport::verdict`]; this exists so a caller can show that the
    /// naive check passes on exactly the configuration that broke a lane.
    #[must_use]
    pub fn any_hop_connected(&self) -> bool {
        self.hops.iter().any(|hop| hop.outcome.connects())
    }

    /// Hops whose failure is paid by every connection.
    #[must_use]
    pub fn taxing_hops(&self) -> Vec<&HopReport> {
        self.hops
            .iter()
            .filter(|hop| hop.attempted && !hop.outcome.connects())
            .collect()
    }
}

/// Assert that every declared hop connects inside its budget, in order.
///
/// `probes` must be in **declared order** — the order the relay tries them —
/// because position is what decides whether a broken hop is a live tax or a
/// latent loss of fallback. Passing them sorted, deduplicated, or "healthiest
/// first" produces confident nonsense.
///
/// Verdict rules, in precedence order:
///
/// * any hop the probe could not measure → [`ServiceVerdict::Unknown`] with
///   that hop's [`Boundary`]. An unmeasured hop is never a pass, even when
///   every other hop is healthy;
/// * no hop answers at all → [`ServiceVerdict::Unserved`]: the relay is
///   severed, not taxed;
/// * otherwise the worst per-hop verdict. A failing hop is
///   [`ServiceVerdict::Degraded`] whether or not any connection reaches it —
///   an unreachable fallback is redundancy already lost, not a hop at rest —
///   and the detail distinguishes the two by the tax actually paid.
#[must_use]
pub fn assess_relay(
    relay: &str,
    probes: &[HopProbe],
    thresholds: RelayThresholds,
    now: DateTime<Utc>,
) -> RelayReport {
    let mut report = RelayReport {
        relay: relay.to_owned(),
        declared_hops: probes.iter().map(|probe| probe.host.clone()).collect(),
        hops: Vec::new(),
        verdict: ServiceVerdict::Unknown,
        boundary: None,
        hop_budget_secs: thresholds.hop_budget_secs,
        first_answering_position: None,
        tax_secs: None,
        detail: String::new(),
        observed_at: now,
    };

    if probes.is_empty() {
        report.boundary = Some(Boundary::Parse);
        report.detail = format!(
            "relay `{relay}` declares no hop, so nothing was asserted about it. An empty hop list \
             and an unread one produce the identical result. {}",
            Boundary::Parse.next_action()
        );
        return report;
    }

    if thresholds.hop_budget_secs <= 0.0 || thresholds.hop_budget_secs.is_nan() {
        report.boundary = Some(Boundary::Parse);
        report.detail = format!(
            "relay `{relay}` resolves to a non-positive hop budget of {}s — no connect-time claim \
             can be made against it. {}",
            thresholds.hop_budget_secs,
            Boundary::Parse.next_action()
        );
        return report;
    }

    let first_answering = probes.iter().position(|probe| probe.outcome.connected());
    report.first_answering_position = first_answering.map(|index| index + 1);
    report.hops = probes
        .iter()
        .enumerate()
        .map(|(index, probe)| classify_hop(probe, index, probes.len(), first_answering, thresholds))
        .collect();
    report.tax_secs = first_answering.map(|answering| {
        probes[..answering]
            .iter()
            .filter_map(|probe| probe.outcome.elapsed_secs())
            .sum()
    });

    decide_relay_verdict(&mut report);
    report
}

/// Judge one hop's measurement at its declared position.
fn classify_hop(
    probe: &HopProbe,
    index: usize,
    hop_count: usize,
    first_answering: Option<usize>,
    thresholds: RelayThresholds,
) -> HopReport {
    // Every hop up to and including the first one that answers is actually
    // dialled; anything behind it is never reached.
    let attempted = first_answering.is_none_or(|answering| index <= answering);
    let connect_secs = probe.outcome.elapsed_secs();
    let budget_ratio = connect_secs.map(|secs| secs / thresholds.hop_budget_secs);
    let outcome = judge_outcome(probe.outcome, thresholds.hop_budget_secs);

    let (verdict, boundary) = match outcome {
        HopOutcome::Unmeasurable => (ServiceVerdict::Unknown, probe.outcome.boundary()),
        HopOutcome::WithinBudget => (ServiceVerdict::Served, None),
        // A defect no connection currently reaches still raises, and this is
        // the deliberate call. `Idle` in this taxonomy means nothing is asking;
        // a relay in use is being asked, so an unreachable fallback is not at
        // rest — it is redundancy that has already been lost, silently. That is
        // the exact shape of the incident behind this module: a hop was dead
        // for days and surfaced only during the outage its deadness deepened.
        //
        // Both cases are `Degraded` rather than one being suppressed, but they
        // are never conflated: `attempted` and the tax figure separate the hop
        // every connection pays for from the one that costs nothing yet, and
        // the detail says which.
        _ => (ServiceVerdict::Degraded, None),
    };

    let mut report = HopReport {
        host: probe.host.clone(),
        position: index + 1,
        hop_count,
        outcome,
        attempted,
        connect_secs,
        budget_secs: thresholds.hop_budget_secs,
        budget_ratio,
        verdict,
        boundary,
        remedy: outcome.remedy().to_owned(),
        detail: String::new(),
    };
    report.detail = hop_detail(&report, first_answering, probe.outcome.boundary());
    report
}

/// Map a raw measurement onto a budget-aware outcome.
fn judge_outcome(outcome: ProbeOutcome, budget_secs: f64) -> HopOutcome {
    match outcome {
        ProbeOutcome::Connected { elapsed_secs } => {
            if elapsed_secs >= budget_secs {
                HopOutcome::OverBudget
            } else {
                HopOutcome::WithinBudget
            }
        }
        ProbeOutcome::Refused { .. } => HopOutcome::Refused,
        ProbeOutcome::TimedOut { .. } => HopOutcome::TimedOut,
        ProbeOutcome::Unresolved { .. } => HopOutcome::Unresolved,
        ProbeOutcome::Unmeasured { .. } => HopOutcome::Unmeasurable,
    }
}

/// Render the connect time against its budget, which is the form that keeps a
/// hop at 98% of budget distinguishable from one at 4%.
fn ratio_phrase(report: &HopReport) -> String {
    match (report.connect_secs, report.budget_ratio) {
        (Some(secs), Some(ratio)) => format!(
            "{secs:.2}s against a {:.1}s budget ({:.0}% of it)",
            report.budget_secs,
            ratio * 100.0
        ),
        _ => format!(
            "no measured time against a {:.1}s budget",
            report.budget_secs
        ),
    }
}

/// Compose the operator-facing message for one hop.
fn hop_detail(
    report: &HopReport,
    first_answering: Option<usize>,
    boundary: Option<Boundary>,
) -> String {
    let position = format!("hop {} of {}", report.position, report.hop_count);
    match report.outcome {
        HopOutcome::Unmeasurable => {
            let boundary = boundary.unwrap_or(Boundary::Transport);
            format!(
                "{position} `{}`: probe did not measure ({}) — not a pass, because an unprobed hop \
                 and a healthy one produce the identical silence. Next: {}",
                report.host,
                boundary.as_str(),
                boundary.next_action()
            )
        }
        HopOutcome::WithinBudget => format!(
            "{position} `{}` connected in {}",
            report.host,
            ratio_phrase(report)
        ),
        HopOutcome::OverBudget if report.attempted => format!(
            "{position} `{}` connected in {} — over budget, and every connection through this \
             relay pays it",
            report.host,
            ratio_phrase(report)
        ),
        HopOutcome::OverBudget => format!(
            "{position} `{}` connected in {} — over budget, but never reached: {} answers first, \
             so nothing pays it today. It is the fallback, and the fallback is already slow",
            report.host,
            ratio_phrase(report),
            answering_phrase(first_answering)
        ),
        _ if report.attempted => format!(
            "{position} `{}` {} after {} — it is dialled BEFORE {}, so every connection pays this \
             before the fallback succeeds",
            report.host,
            report.outcome.as_str(),
            ratio_phrase(report),
            answering_phrase(first_answering)
        ),
        _ => format!(
            "{position} `{}` {} after {} — never reached, because {} answers first. It costs \
             nothing today; what is gone is the fallback it was",
            report.host,
            report.outcome.as_str(),
            ratio_phrase(report),
            answering_phrase(first_answering)
        ),
    }
}

/// Name the hop that ends the fallback chain, for use inside a hop's message.
fn answering_phrase(first_answering: Option<usize>) -> String {
    first_answering.map_or_else(
        || "any answering hop (there is none)".to_owned(),
        |index| format!("the hop at position {}", index + 1),
    )
}

/// Roll the per-hop verdicts up into the relay's, and explain the result.
fn decide_relay_verdict(report: &mut RelayReport) {
    if let Some(unknown) = report
        .hops
        .iter()
        .find(|hop| hop.verdict == ServiceVerdict::Unknown)
    {
        report.verdict = ServiceVerdict::Unknown;
        report.boundary = unknown.boundary;
        report.detail = format!(
            "relay `{}`: {} — so no claim is made about this relay, however healthy the other {} \
             hop(s) measured.",
            report.relay,
            unknown.detail,
            report.hops.len().saturating_sub(1)
        );
        return;
    }

    if report.first_answering_position.is_none() {
        report.verdict = ServiceVerdict::Unserved;
        report.detail = unserved_detail(report);
        return;
    }

    report.verdict = report
        .hops
        .iter()
        .map(|hop| hop.verdict)
        .max()
        .unwrap_or(ServiceVerdict::Unknown);
    // The message is chosen by what the defect COSTS, not by the verdict. A
    // dead fallback and a dead first hop are both `Degraded`, and telling them
    // apart is the entire point of tracking position — describing a shadowed
    // hop as a latency tax would report a cost nobody is paying.
    let any_attempted_failure = report
        .hops
        .iter()
        .any(|hop| hop.attempted && hop.verdict.is_raise());
    report.detail = match report.verdict {
        ServiceVerdict::Degraded if any_attempted_failure => degraded_detail(report),
        ServiceVerdict::Degraded => idle_detail(report),
        _ => served_detail(report),
    };
}

/// Message for a relay no hop answers on.
fn unserved_detail(report: &RelayReport) -> String {
    let spent: f64 = report
        .hops
        .iter()
        .filter_map(|hop| hop.connect_secs)
        .sum::<f64>();
    format!(
        "relay `{}` is severed: none of its {} declared hop(s) [{}] answered, after spending \
         {spent:.2}s trying. This is not a tax, it is an outage — nothing gets through, and a \
         caller with its own timeout will report whatever it was doing instead.",
        report.relay,
        report.hops.len(),
        report.declared_hops.join(", ")
    )
}

/// Message for a relay that answers while paying for a defective hop.
fn degraded_detail(report: &RelayReport) -> String {
    let taxing: Vec<String> = report
        .taxing_hops()
        .iter()
        .map(|hop| {
            format!(
                "`{}` ({}, position {})",
                hop.host,
                hop.outcome.as_str(),
                hop.position
            )
        })
        .collect();
    let over_budget: Vec<String> = report
        .hops
        .iter()
        .filter(|hop| hop.attempted && hop.outcome == HopOutcome::OverBudget)
        .map(|hop| {
            format!(
                "`{}` ({:.0}% of budget)",
                hop.host,
                hop.budget_ratio.unwrap_or_default() * 100.0
            )
        })
        .collect();

    let mut parts: Vec<String> = Vec::new();
    if !taxing.is_empty() {
        parts.push(format!("{} fail before it", taxing.join(", ")));
    }
    if !over_budget.is_empty() {
        parts.push(format!("{} answers over budget", over_budget.join(", ")));
    }

    format!(
        "relay `{}` ANSWERS — on the hop at position {} — and that is exactly why this is easy to \
         miss: {}. Every connection pays {:.2}s of tax before the fallback succeeds, so \"does the \
         proxy answer?\" stays green while the cost lands on whatever downstream budget is \
         smallest. Next: {}",
        report.relay,
        report.first_answering_position.unwrap_or_default(),
        parts.join("; "),
        report.tax_secs.unwrap_or_default(),
        report
            .taxing_hops()
            .first()
            .map_or(HopOutcome::OverBudget, |hop| hop.outcome)
            .remedy()
    )
}

/// Message for a relay whose only defective hops are ones nothing reaches.
fn idle_detail(report: &RelayReport) -> String {
    let unreached: Vec<String> = report
        .hops
        .iter()
        .filter(|hop| !hop.attempted && !hop.outcome.connects())
        .map(|hop| {
            format!(
                "`{}` ({}, position {})",
                hop.host,
                hop.outcome.as_str(),
                hop.position
            )
        })
        .collect();
    format!(
        "relay `{}` answers on the hop at position {} with no tax: {} sit behind it and are never \
         dialled, so no connection pays for them. Nothing is slow today; what is gone is the \
         fallback, and the next failure of the leading hop severs the relay.",
        report.relay,
        report.first_answering_position.unwrap_or_default(),
        unreached.join(", ")
    )
}

/// Message for a relay whose every declared hop connects inside its budget.
fn served_detail(report: &RelayReport) -> String {
    let measured: Vec<String> = report
        .hops
        .iter()
        .map(|hop| {
            format!(
                "`{}` {:.0}%",
                hop.host,
                hop.budget_ratio.unwrap_or_default() * 100.0
            )
        })
        .collect();
    format!(
        "relay `{}`: all {} declared hop(s) connect inside the {:.1}s budget, in declared order — \
         {} of budget. No connection pays a failed hop before reaching a working one.",
        report.relay,
        report.hops.len(),
        report.hop_budget_secs,
        measured.join(", ")
    )
}

/// What a bounded self-heal proposal concluded.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// A different hop list is proposed.
    Proposed,
    /// The declared order is already the one this would propose.
    AlreadyOptimal,
    /// No proposal is safe to make.
    Refused,
}

/// A bounded, reviewable proposal for a healthier hop order.
///
/// **Nothing here is executed.** This is a description of an edit for a human
/// or a caller to apply, and it is deliberately incapable of applying itself:
/// the whole incident was a relay that kept reporting success, and an
/// auto-remediation on top of that signal would have edited the hop list on the
/// strength of the same reading that hid the fault.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RelayProposal {
    /// What was concluded.
    pub status: ProposalStatus,
    /// The hop order as declared.
    pub current_order: Vec<String>,
    /// The hop order proposed instead. Empty when refused.
    pub proposed_order: Vec<String>,
    /// Hops proposed for removal.
    pub dropped: Vec<String>,
    /// Hops this proposal declined to drop, because dropping them would leave
    /// the relay with nothing that connects.
    pub refused_drops: Vec<String>,
    /// Why, in operator-facing terms.
    pub rationale: String,
}

impl RelayProposal {
    /// Whether applying this proposal would change anything.
    #[must_use]
    pub fn is_change(&self) -> bool {
        self.status == ProposalStatus::Proposed
    }
}

/// Propose — never apply — a hop order that stops paying for broken hops.
///
/// Two bounded edits, in this order:
///
/// 1. **Drop** a hop that does not connect at all. It can never serve; ahead of
///    a working hop it is pure tax, and behind one it is a fallback that is
///    already gone.
/// 2. **Order** the survivors healthiest-first: hops inside their budget in
///    declared order, then hops over budget cheapest-first, so the tax any
///    connection can pay is the smallest available.
///
/// And three refusals, which are the point of returning a proposal rather than
/// a patch:
///
/// * any hop the probe could not measure → refuse outright. A proposal built on
///   an unmeasured hop is a guess wearing a remedy's clothes;
/// * no hop connects → refuse. The relay is severed; reordering nothing is
///   still nothing;
/// * dropping an over-budget hop would leave **no** hop that connects → keep
///   it, and say so in [`RelayProposal::refused_drops`]. A slow relay is worth
///   having; a severed one is not.
#[must_use]
pub fn propose_hop_order(report: &RelayReport) -> RelayProposal {
    let mut proposal = RelayProposal {
        status: ProposalStatus::Refused,
        current_order: report.declared_hops.clone(),
        proposed_order: Vec::new(),
        dropped: Vec::new(),
        refused_drops: Vec::new(),
        rationale: String::new(),
    };

    if let Some(unmeasured) = report
        .hops
        .iter()
        .find(|hop| hop.outcome == HopOutcome::Unmeasurable)
    {
        proposal.rationale = format!(
            "refusing to propose a hop order for `{}`: hop `{}` at position {} was not measured \
             ({}), and a reorder decided from an unmeasured hop is a guess. Next: {}",
            report.relay,
            unmeasured.host,
            unmeasured.position,
            unmeasured.boundary.map_or("unknown", Boundary::as_str),
            unmeasured
                .boundary
                .unwrap_or(Boundary::Transport)
                .next_action()
        );
        return proposal;
    }

    let within: Vec<&HopReport> = report
        .hops
        .iter()
        .filter(|hop| hop.outcome == HopOutcome::WithinBudget)
        .collect();
    let mut over: Vec<&HopReport> = report
        .hops
        .iter()
        .filter(|hop| hop.outcome == HopOutcome::OverBudget)
        .collect();
    let broken: Vec<&HopReport> = report
        .hops
        .iter()
        .filter(|hop| !hop.outcome.connects())
        .collect();

    if within.is_empty() && over.is_empty() {
        proposal.rationale = format!(
            "refusing to propose a hop order for `{}`: not one of its {} declared hop(s) connects, \
             so every reorder and every drop severs the relay just as thoroughly as it is severed \
             now. Restore a hop before touching the order.",
            report.relay,
            report.hops.len()
        );
        return proposal;
    }

    // Cheapest-first among the slow ones, so if the relay must run on an
    // over-budget hop it runs on the least expensive one available.
    over.sort_by(|left, right| {
        left.budget_ratio
            .unwrap_or(f64::MAX)
            .total_cmp(&right.budget_ratio.unwrap_or(f64::MAX))
    });

    proposal.dropped = broken.iter().map(|hop| hop.host.clone()).collect();
    if within.is_empty() {
        // Dropping these would leave nothing that connects at all.
        proposal.refused_drops = over.iter().map(|hop| hop.host.clone()).collect();
        proposal.proposed_order = over.iter().map(|hop| hop.host.clone()).collect();
    } else {
        proposal
            .dropped
            .extend(over.iter().map(|hop| hop.host.clone()));
        proposal.proposed_order = within.iter().map(|hop| hop.host.clone()).collect();
    }

    proposal.status = if proposal.proposed_order == proposal.current_order {
        ProposalStatus::AlreadyOptimal
    } else {
        ProposalStatus::Proposed
    };
    proposal.rationale = proposal_rationale(report, &proposal);
    proposal
}

/// Explain a proposal in terms of the tax it removes.
fn proposal_rationale(report: &RelayReport, proposal: &RelayProposal) -> String {
    if proposal.status == ProposalStatus::AlreadyOptimal {
        return format!(
            "no change proposed for `{}`: every declared hop already connects inside the {:.1}s \
             budget, in an order that pays nothing before the first answer.",
            report.relay, report.hop_budget_secs
        );
    }

    let mut parts: Vec<String> = Vec::new();
    if !proposal.dropped.is_empty() {
        parts.push(format!(
            "drop {} — {} cannot serve, and ahead of a working hop {} only adds latency every \
             connection pays",
            proposal.dropped.join(", "),
            if proposal.dropped.len() == 1 {
                "it"
            } else {
                "they"
            },
            if proposal.dropped.len() == 1 {
                "it"
            } else {
                "they"
            }
        ));
    }
    if !proposal.refused_drops.is_empty() {
        parts.push(format!(
            "KEEP {} despite missing the budget — refusing to drop the last hop(s) that connect at \
             all, because a slow relay is recoverable and a severed one is an outage",
            proposal.refused_drops.join(", ")
        ));
    }
    parts.push(format!(
        "order the survivors {} so the first hop dialled is the cheapest one that answers",
        proposal.proposed_order.join(" → ")
    ));

    format!(
        "proposal for `{}` (describe only — nothing is applied): {}. This removes {:.2}s of tax \
         from every connection.",
        report.relay,
        parts.join("; "),
        report.tax_secs.unwrap_or_default()
    )
}

#[cfg(test)]
mod tests;
