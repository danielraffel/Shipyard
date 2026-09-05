//! Fleet **guard** assertions: pure classification for "is the guard that would
//! have caught this actually armed on the host?", and "is the copy running
//! there still the copy the repo thinks it is?".
//!
//! Sibling to [`crate::fleet_service`], deliberately the same shape and reusing
//! its taxonomy: no I/O, no ambient clock, no process spawning. The caller runs
//! `systemctl show`, stats the paths and hashes the bytes; this module only
//! classifies what it was handed, with an explicit `now`.
//!
//! ## Two incidents, pointing opposite ways
//!
//! **A guard in the repo is not a guard until something proves it is armed on
//! the host.** A Linux CI host served zero work for nineteen days. The reaper
//! that would have recovered it existed the whole time: an 18 KB script, a
//! `.service`, a `.timer`, documentation, an install runbook. On that host the
//! *script* had been installed months earlier — a stale 4.8 KB copy — and the
//! service and timer units **were never installed at all**, so nothing ever
//! invoked it. The pool crash-looped to `NRestarts=36088` behind a `systemctl
//! status` that read `active (running)` throughout. Every passive signal was
//! green; the guard had simply never run.
//!
//! **The mirror image is more dangerous, because a working host has no
//! symptom.** A host carrying changes the repo does not have raises nothing: it
//! works. A "drift detected → redeploy from the repo" reflex then deletes
//! whatever it was carrying, and the loss surfaces later, as whatever breaks.
//!
//! **Compare only the artifact that actually executes.** This assertion was
//! written directly after a drift measurement that was confidently wrong. A
//! populated, plausible install directory — sitting at the path the docs and
//! the probes both used — turned out to be inert, and the number computed from
//! it described nothing that runs. The supervisors were executing a
//! content-addressed generation elsewhere, byte-identical to a real commit and
//! merely some commits behind it: zero drift, where the abandoned directory
//! suggested hundreds of lines of it.
//!
//! An inert copy of a real thing passes every check except *is this the artifact
//! that executes?*, so [`ArtifactObservation`] carries [`ExecProvenance`] and
//! this module refuses to render a drift verdict for a path nobody proved is the
//! exec target. Being unable to compare is reported; it is never a pass.
//!
//! So neither assertion returns a boolean, and the drift verdict is not even
//! two-valued. [`DriftState`] separates *behind the repo* (deploy) from *ahead
//! of the repo* (upstream the delta, and do **not** redeploy) because those two
//! present as the identical "digests differ" symptom and have opposite
//! remedies — the same trap [`crate::fleet_service`] documents for `Unserved`
//! versus `Starved`. It also reports the delta's **size**, so that two changed
//! lines and two hundred do not sound the same alarm.
//!
//! ## `NRestarts` is a level; the trigger has to be a rate
//!
//! The crash-loop above is the obvious thing to assert on, and asserting on the
//! absolute counter is the obvious way to do it. It is wrong. `NRestarts` is
//! monotonic and survives the repair: the host that crash-looped to 36088 reads
//! `NRestarts=36089` **today, while perfectly healthy**, because the counter
//! only clears on a daemon-level reset nobody performs. An alarm wired to the
//! absolute value is permanently red, and a permanently red alarm is
//! operationally identical to no alarm at all.
//!
//! So [`assess_restart_churn`] triggers on the **delta against a caller-supplied
//! previous baseline over a known interval**, and reports the absolute only as
//! context. A first observation has no baseline and therefore cannot answer a
//! rate question at all: that is [`ServiceVerdict::Unknown`] with
//! [`Boundary::Scope`] — a single instant is the wrong scope for a rate, in the
//! same sense that a repo-scope census is the wrong scope for an org runner.
//! The same reading applies to an artifact with no recorded upstream ref: a
//! one-sided observation cannot answer a two-sided question, and folding that
//! into a pass is the failure mode this module exists to end.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::fleet_service::{Boundary, ServiceVerdict};

/// Default restart ceiling: how many restarts a unit may accumulate within one
/// window before its churn is reported as a fault.
///
/// A handful of restarts is ordinary supervision. The incident this guards
/// against accumulated tens of thousands, so the ceiling only has to be low
/// enough to catch a loop, not tuned.
pub const DEFAULT_RESTART_CEILING_PER_WINDOW: u64 = 3;

/// Default window, in seconds, that [`DEFAULT_RESTART_CEILING_PER_WINDOW`] is
/// expressed against.
pub const DEFAULT_RESTART_WINDOW_SECS: i64 = 3600;

/// Which kind of systemd unit an observation describes.
///
/// The distinction is load-bearing: a timer's arming includes a next elapse,
/// and a service's does not. Demanding an elapse of a service would fail every
/// healthy host; not demanding one of a timer is the hole incident A fell
/// through.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// A `.service` unit.
    Service,
    /// A `.timer` unit.
    Timer,
}

impl UnitKind {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Timer => "timer",
        }
    }
}

/// `UnitFileState` as `systemctl show` reports it.
///
/// Kept as the observed vocabulary rather than reduced to a boolean, because
/// `static` and `masked` are neither "enabled" nor "disabled" and each carries
/// a different remedy: a `static` unit *cannot* be enabled and is armed by
/// whatever invokes it, while a `masked` unit is wired to never start.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnablementState {
    /// `enabled` (or `enabled-runtime`).
    Enabled,
    /// `disabled`.
    Disabled,
    /// `static` — no `[Install]` section, so `systemctl enable` is a no-op.
    Static,
    /// `masked` — symlinked to `/dev/null`; it can never start.
    Masked,
    /// Any other value systemd reported, preserved verbatim.
    Other(String),
}

impl EnablementState {
    /// The observed string form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Static => "static",
            Self::Masked => "masked",
            Self::Other(raw) => raw,
        }
    }
}

/// Whether a unit file exists on the host at all.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "presence", rename_all = "snake_case")]
pub enum UnitPresence {
    /// `LoadState=not-found`: no unit file of this name on this host. The state
    /// incident A was actually in, and the one a sibling unit's `systemctl
    /// status` can never reveal.
    Absent,
    /// A unit file exists, with the `UnitFileState` systemd reported.
    Present {
        /// `UnitFileState`.
        state: EnablementState,
    },
    /// The unit could not be inspected. Never folded into a pass.
    Unreadable {
        /// Why the inspection did not answer.
        boundary: Boundary,
    },
}

/// A previously recorded `NRestarts` reading, and when it was taken.
///
/// Required to say anything at all about restart churn: without it there is a
/// level and no rate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct RestartBaseline {
    /// `NRestarts` at `observed_at`.
    pub count: u64,
    /// When the baseline reading was taken.
    pub observed_at: DateTime<Utc>,
}

/// The `NRestarts` counter for one unit, now and previously.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct RestartObservation {
    /// `NRestarts` as systemd reports it now. Monotonic; it survives the repair
    /// that fixed the loop, so it is context and never the trigger.
    pub current: u64,
    /// The caller's previous reading. `None` on a first observation.
    pub baseline: Option<RestartBaseline>,
}

/// Tunables for [`assess_restart_churn`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartChurnThresholds {
    /// Restarts permitted within `window_secs` before the churn is a fault.
    pub max_per_window: u64,
    /// Window the ceiling is expressed against. Deltas measured over a shorter
    /// or longer interval are scaled onto it.
    pub window_secs: i64,
}

impl Default for RestartChurnThresholds {
    fn default() -> Self {
        Self {
            max_per_window: DEFAULT_RESTART_CEILING_PER_WINDOW,
            window_secs: DEFAULT_RESTART_WINDOW_SECS,
        }
    }
}

/// Verdict for one unit's restart churn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RestartReport {
    /// The verdict. [`ServiceVerdict::Degraded`] when the *rate* exceeds the
    /// ceiling — never because the absolute count is large.
    pub verdict: ServiceVerdict,
    /// `NRestarts` now, reported as context only.
    pub current: u64,
    /// Restarts accumulated since the baseline, when one exists.
    pub delta: Option<u64>,
    /// Seconds between the baseline reading and `now`.
    pub elapsed_secs: Option<i64>,
    /// `delta` scaled onto [`RestartChurnThresholds::window_secs`].
    pub projected_per_window: Option<u64>,
    /// Why no rate could be measured. Always `Some` when the verdict is
    /// [`ServiceVerdict::Unknown`], and `None` otherwise.
    pub boundary: Option<Boundary>,
    /// Operator-facing explanation naming what was measured.
    pub detail: String,
}

/// Which of the four arming states a unit is in.
///
/// `enabled` alone is insufficient, which is why [`Self::NoNextElapse`] exists
/// as its own value: a timer that is enabled but has no next elapse fires
/// never, and is exactly as useless as one that was never installed. Collapsing
/// it into "enabled" is how a host reports itself armed while nothing runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmingState {
    /// No unit file on the host.
    NotInstalled,
    /// The unit file exists but will not start at boot.
    NotEnabled,
    /// The unit file exists and is masked: wired to never start.
    Masked,
    /// Enabled, but the timer has no next elapse. It will never fire.
    NoNextElapse,
    /// Enabled (or `static` and invoked by its timer) and, for a timer, with a
    /// next elapse scheduled.
    Armed,
    /// The unit could not be inspected.
    Undetermined,
}

impl ArmingState {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::NotEnabled => "not_enabled",
            Self::Masked => "masked",
            Self::NoNextElapse => "no_next_elapse",
            Self::Armed => "armed",
            Self::Undetermined => "undetermined",
        }
    }
}

/// One unit as the caller observed it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UnitObservation {
    /// Unit name, as the runbook names it (`tartci-reaper.timer`).
    pub unit: String,
    /// Whether this is a service or a timer.
    pub kind: UnitKind,
    /// Whether the unit file exists, and its `UnitFileState`.
    pub presence: UnitPresence,
    /// `NextElapseUSecRealtime`, for a timer. `None` on a timer means it is
    /// scheduled to fire at no point in the future.
    pub next_elapse: Option<DateTime<Utc>>,
    /// The unit's `NRestarts` counter. `None` when the caller observed no
    /// counter, which is the ordinary case for a timer.
    pub restarts: Option<RestartObservation>,
}

/// Verdict for one unit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UnitArmingReport {
    /// Unit name.
    pub unit: String,
    /// Service or timer.
    pub kind: UnitKind,
    /// Combined verdict over arming and restart churn.
    pub verdict: ServiceVerdict,
    /// The arming distinction, kept separate from the verdict so "not enabled"
    /// and "enabled but never fires" stay legible after roll-up.
    pub arming: ArmingState,
    /// Whether the unit is armed.
    pub armed: bool,
    /// The timer's next elapse, when it has one.
    pub next_elapse: Option<DateTime<Utc>>,
    /// Restart churn, as context. `None` when no counter was observed.
    pub restarts: Option<RestartReport>,
    /// Why the unit could not be judged, when it could not be.
    pub boundary: Option<Boundary>,
    /// Operator-facing explanation naming what was measured.
    pub detail: String,
}

/// Whether the executable the units would invoke is on the host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "presence", rename_all = "snake_case")]
pub enum PayloadPresence {
    /// The script or binary is present.
    Installed,
    /// Nothing at that path.
    Absent,
    /// The path could not be stat'd. Never folded into a pass.
    Unreadable {
        /// Why the stat did not answer.
        boundary: Boundary,
    },
}

/// A guard as the caller observed it: its payload, plus every unit the host
/// runbook says must be enabled for it to run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuardObservation {
    /// Guard name, as the runbook names it.
    pub name: String,
    /// Where the payload is expected on this host.
    pub payload_path: String,
    /// Whether the payload is there.
    pub payload: PayloadPresence,
    /// Units the runbook declares. An empty list cannot prove anything, and is
    /// reported as such rather than as a pass.
    pub units: Vec<UnitObservation>,
}

/// Verdict for one guard.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GuardArmingReport {
    /// Guard name.
    pub guard: String,
    /// Worst finding across the payload and every declared unit.
    pub verdict: ServiceVerdict,
    /// Where the payload was expected.
    pub payload_path: String,
    /// Whether the payload was there.
    pub payload: PayloadPresence,
    /// Per-unit verdicts.
    pub units: Vec<UnitArmingReport>,
    /// How many declared units are armed.
    pub armed_units: usize,
    /// Why the guard could not be judged, when it could not be.
    pub boundary: Option<Boundary>,
    /// Operator-facing explanation naming what was measured.
    pub detail: String,
}

/// Classify one unit's restart churn from a delta, never from the level.
///
/// `now` is injected; the baseline inside `observation` is whatever the caller
/// recorded on a previous pass. A missing baseline is [`ServiceVerdict::Unknown`]
/// with [`Boundary::Scope`]: one sample is the wrong scope for a rate question,
/// and answering it anyway — in either direction — is how a permanently red or
/// permanently green restart alarm gets built.
#[must_use]
pub fn assess_restart_churn(
    observation: &RestartObservation,
    thresholds: RestartChurnThresholds,
    now: DateTime<Utc>,
) -> RestartReport {
    let mut report = RestartReport {
        verdict: ServiceVerdict::Unknown,
        current: observation.current,
        delta: None,
        elapsed_secs: None,
        projected_per_window: None,
        boundary: None,
        detail: String::new(),
    };
    let current = observation.current;

    let Some(baseline) = observation.baseline else {
        report.boundary = Some(Boundary::Scope);
        report.detail = format!(
            "NRestarts={current} with no previous reading: the counter is monotonic and survives \
             every repair, so its level says nothing. Record this reading as a baseline and the \
             next pass can measure a rate (boundary: scope)"
        );
        return report;
    };

    let elapsed = (now - baseline.observed_at).num_seconds();
    report.elapsed_secs = Some(elapsed);
    if elapsed <= 0 {
        report.boundary = Some(Boundary::Parse);
        report.detail = format!(
            "baseline is timestamped {elapsed}s relative to now, so no interval exists to \
             measure a rate over (boundary: parse)"
        );
        return report;
    }

    let Some(delta) = current.checked_sub(baseline.count) else {
        report.boundary = Some(Boundary::Parse);
        report.detail = format!(
            "NRestarts went backwards ({} -> {current}): the interval spans a counter reset, so \
             the delta across it is not a restart count (boundary: parse)",
            baseline.count
        );
        return report;
    };

    report.delta = Some(delta);
    report.projected_per_window = Some(project_per_window(delta, elapsed, thresholds.window_secs));
    finish_churn_verdict(&mut report, delta, elapsed, thresholds);
    report
}

/// Decide the churn verdict once the delta and interval are known.
fn finish_churn_verdict(
    report: &mut RestartReport,
    delta: u64,
    elapsed: i64,
    thresholds: RestartChurnThresholds,
) {
    let current = report.current;
    let projected = report.projected_per_window.unwrap_or(delta);
    let window = thresholds.window_secs;
    let ceiling = thresholds.max_per_window;

    if exceeds_ceiling(delta, elapsed, thresholds) {
        report.verdict = ServiceVerdict::Degraded;
        report.detail = format!(
            "{delta} restart(s) in the last {elapsed}s (~{projected} per {window}s) is over the \
             ceiling of {ceiling} per {window}s. NRestarts={current} is the level and is context \
             only; the delta is the trigger"
        );
        return;
    }

    report.verdict = ServiceVerdict::Served;
    report.detail = format!(
        "{delta} restart(s) in the last {elapsed}s (~{projected} per {window}s), within the \
         ceiling of {ceiling} per {window}s. NRestarts={current} is monotonic and survives the \
         repair that ended the loop, so the level is reported as context and never raises"
    );
}

/// Whether a delta over `elapsed` seconds exceeds the ceiling once scaled onto
/// the threshold window.
///
/// Integer math throughout: a rate comparison that rounds is a rate comparison
/// that disagrees with the number printed beside it.
fn exceeds_ceiling(delta: u64, elapsed: i64, thresholds: RestartChurnThresholds) -> bool {
    i128::from(delta) * i128::from(thresholds.window_secs)
        > i128::from(thresholds.max_per_window) * i128::from(elapsed)
}

/// Scale a delta measured over `elapsed` seconds onto `window_secs`.
fn project_per_window(delta: u64, elapsed: i64, window_secs: i64) -> u64 {
    if elapsed <= 0 {
        return delta;
    }
    let scaled = i128::from(delta) * i128::from(window_secs) / i128::from(elapsed);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// Classify one unit: installed, enabled, and — for a timer — actually
/// scheduled to fire.
#[must_use]
pub fn assess_unit_arming(
    observation: &UnitObservation,
    thresholds: RestartChurnThresholds,
    now: DateTime<Utc>,
) -> UnitArmingReport {
    let (arming, boundary, detail) = classify_arming(observation);
    let churn = observation
        .restarts
        .as_ref()
        .map(|restarts| assess_restart_churn(restarts, thresholds, now));

    let arming_verdict = match arming {
        ArmingState::Armed => ServiceVerdict::Served,
        ArmingState::Undetermined => ServiceVerdict::Unknown,
        _ => ServiceVerdict::Unserved,
    };
    let verdict = combine_unit_verdict(arming_verdict, churn.as_ref().map(|c| c.verdict));

    let boundary = boundary.or_else(|| {
        churn
            .as_ref()
            .filter(|c| c.verdict == ServiceVerdict::Unknown)
            .and_then(|c| c.boundary)
    });

    let detail = match churn.as_ref() {
        Some(churn) if churn.verdict != ServiceVerdict::Served => {
            format!("{detail}; restarts: {}", churn.detail)
        }
        _ => detail,
    };

    UnitArmingReport {
        unit: observation.unit.clone(),
        kind: observation.kind,
        verdict,
        arming,
        armed: arming == ArmingState::Armed,
        next_elapse: observation.next_elapse,
        restarts: churn,
        boundary,
        detail,
    }
}

/// Decide the arming state, the boundary and the explanation for one unit.
fn classify_arming(observation: &UnitObservation) -> (ArmingState, Option<Boundary>, String) {
    let unit = observation.unit.as_str();
    let state = match &observation.presence {
        UnitPresence::Absent => {
            return (
                ArmingState::NotInstalled,
                None,
                format!(
                    "{unit} has no unit file on this host, so nothing invokes it. A sibling \
                     unit's `systemctl status` reads healthy either way and cannot reveal this"
                ),
            );
        }
        UnitPresence::Unreadable { boundary } => {
            return (
                ArmingState::Undetermined,
                Some(*boundary),
                format!(
                    "{unit} could not be inspected ({}) — an uninspectable unit is not an armed \
                     one. Next: {}",
                    boundary.as_str(),
                    boundary.next_action()
                ),
            );
        }
        UnitPresence::Present { state } => state,
    };

    match state {
        EnablementState::Masked => (
            ArmingState::Masked,
            None,
            format!("{unit} is masked: it is wired to never start, so it can never fire"),
        ),
        EnablementState::Enabled => classify_enabled_unit(observation),
        EnablementState::Static if observation.kind == UnitKind::Service => (
            ArmingState::Armed,
            None,
            format!(
                "{unit} is static — it carries no [Install] section, so it is armed by the timer \
                 that invokes it rather than by being enabled"
            ),
        ),
        other => (
            ArmingState::NotEnabled,
            None,
            format!(
                "{unit} is installed but not enabled (UnitFileState={}), so it will not start at \
                 boot",
                other.as_str()
            ),
        ),
    }
}

/// Decide arming for a unit that is installed and enabled.
///
/// This is where `enabled` stops being sufficient: for a timer the next elapse
/// is the difference between a guard that runs and a guard that merely exists.
fn classify_enabled_unit(observation: &UnitObservation) -> (ArmingState, Option<Boundary>, String) {
    let unit = observation.unit.as_str();
    if observation.kind == UnitKind::Service {
        return (ArmingState::Armed, None, format!("{unit} is enabled"));
    }

    match observation.next_elapse {
        None => (
            ArmingState::NoNextElapse,
            None,
            format!(
                "{unit} is enabled but has no next elapse, so it fires never — exactly as useless \
                 as never having been installed, and it reports enabled the whole time"
            ),
        ),
        Some(next) => (
            ArmingState::Armed,
            None,
            format!("{unit} is enabled and next elapses at {next}"),
        ),
    }
}

/// Combine a unit's arming verdict with its restart churn.
///
/// A *known* fault outranks a blind instrument, so an arming fault is kept even
/// when the churn is [`ServiceVerdict::Unknown`]: reporting "unknown" over a
/// missing unit would send the reader to the restart counter instead of to the
/// unit that was never installed. Where arming is clean, an unmeasurable churn
/// is still not a pass.
fn combine_unit_verdict(arming: ServiceVerdict, churn: Option<ServiceVerdict>) -> ServiceVerdict {
    let Some(churn) = churn else {
        return arming;
    };
    if arming.is_raise() && arming != ServiceVerdict::Unknown {
        return arming;
    }
    arming.max(churn)
}

/// Classify a whole guard: its payload plus every unit the runbook declares.
///
/// The interesting case is the one incident A was in — payload present, units
/// absent — which no per-unit verdict can name on its own, because "the guard
/// is installed and has never once been invoked" is a statement about the pair.
#[must_use]
pub fn assess_guard_arming(
    observation: &GuardObservation,
    thresholds: RestartChurnThresholds,
    now: DateTime<Utc>,
) -> GuardArmingReport {
    let units: Vec<UnitArmingReport> = observation
        .units
        .iter()
        .map(|unit| assess_unit_arming(unit, thresholds, now))
        .collect();
    let armed_units = units.iter().filter(|unit| unit.armed).count();

    let mut report = GuardArmingReport {
        guard: observation.name.clone(),
        verdict: ServiceVerdict::Unknown,
        payload_path: observation.payload_path.clone(),
        payload: observation.payload.clone(),
        units,
        armed_units,
        boundary: None,
        detail: String::new(),
    };

    summarize_guard(&mut report, observation);
    report
}

/// Fold the payload observation and the per-unit verdicts into one guard-level
/// verdict and explanation.
fn summarize_guard(report: &mut GuardArmingReport, observation: &GuardObservation) {
    let guard = observation.name.as_str();
    let path = observation.payload_path.as_str();
    let declared = report.units.len();

    if let PayloadPresence::Unreadable { boundary } = observation.payload {
        report.verdict = ServiceVerdict::Unknown;
        report.boundary = Some(boundary);
        report.detail = format!(
            "{guard}: the payload at {path} could not be stat'd ({}) — not a pass. Next: {}",
            boundary.as_str(),
            boundary.next_action()
        );
        return;
    }

    if declared == 0 {
        report.verdict = ServiceVerdict::Unknown;
        report.boundary = Some(Boundary::Scope);
        report.detail = format!(
            "{guard}: no units were declared for it, so nothing here can prove it is armed. \
             Asserting over an empty unit set is not the same as asserting it passed \
             (boundary: scope)"
        );
        return;
    }

    let all_absent = report
        .units
        .iter()
        .all(|unit| unit.arming == ArmingState::NotInstalled);

    if observation.payload == PayloadPresence::Installed && all_absent {
        report.verdict = ServiceVerdict::Unserved;
        report.detail = format!(
            "{guard}: the payload is installed at {path} but none of its {declared} declared \
             unit(s) exist on this host — the guard is present and never invoked. This is the \
             state that let a host serve zero work for nineteen days while every passive signal \
             read healthy"
        );
        return;
    }

    if observation.payload == PayloadPresence::Absent {
        report.verdict = ServiceVerdict::Unserved;
        report.detail = format!(
            "{guard}: {} of {declared} declared unit(s) are armed, but the payload they invoke is \
             absent from {path} — every firing fails",
            report.armed_units
        );
        return;
    }

    let worst = report
        .units
        .iter()
        .map(|unit| unit.verdict)
        .max()
        .unwrap_or(ServiceVerdict::Unknown);
    report.verdict = worst;
    report.boundary = report
        .units
        .iter()
        .find(|unit| unit.verdict == worst)
        .and_then(|unit| unit.boundary);
    report.detail = if worst == ServiceVerdict::Served {
        format!("{guard}: payload installed at {path} and all {declared} declared unit(s) armed")
    } else {
        let faults: Vec<String> = report
            .units
            .iter()
            .filter(|unit| unit.verdict != ServiceVerdict::Served)
            .map(|unit| unit.detail.clone())
            .collect();
        format!(
            "{guard}: {} of {declared} declared unit(s) armed — {}",
            report.armed_units,
            faults.join("; ")
        )
    };
}

/// How many lines separate the installed copy from the repo copy.
///
/// Reported so that two changed lines and the two hundred and twenty-seven
/// measured on a live host do not sound like the same alarm.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct LineDelta {
    /// Lines present in the installed copy and not in the repo copy.
    pub added: usize,
    /// Lines present in the repo copy and not in the installed copy.
    pub removed: usize,
}

impl LineDelta {
    /// Total changed lines.
    #[must_use]
    pub fn changed(self) -> usize {
        self.added + self.removed
    }
}

/// What the caller recorded about where an installed artifact came from.
///
/// Both digests are needed, and for opposite reasons. `compare_digest` says
/// *whether* the copies differ; `deployed_digest` is the only thing that says
/// *which way*, because an untouched copy still matching its deploy-time digest
/// means the repo moved, while a copy that no longer matches it was edited on
/// the host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UpstreamRecord {
    /// Repo-relative path of the copy this was deployed from.
    pub repo_path: String,
    /// The ref recorded at deploy time.
    pub deployed_ref: String,
    /// Digest of the repo copy at `deployed_ref`. `None` when the deploy left
    /// no digest behind, which makes the direction unrecoverable.
    pub deployed_digest: Option<String>,
    /// The ref being compared against now.
    pub compare_ref: String,
    /// Digest of the repo copy at `compare_ref`. `None` when it could not be
    /// read.
    pub compare_digest: Option<String>,
}

/// Whether the compared path was proven to be the thing that actually runs.
///
/// A drift number computed against a path nobody execs describes a directory,
/// not a fleet. That mistake has already been made here once, convincingly, so
/// the provenance is a required field rather than an assumption: an unproven
/// path yields [`DriftState::Undetermined`], never a clean bill of health.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecProvenance {
    /// Something the caller resolved — a service definition, a launcher, a
    /// wrapper chain — names this exact path as what it executes.
    ProvenExecTarget {
        /// How it was proven, named so the claim can be audited.
        resolved_via: String,
    },
    /// The path was chosen because it looked right. Not sufficient.
    Unproven,
}

impl ExecProvenance {
    /// Whether a drift comparison against this path means anything.
    #[must_use]
    pub fn is_proven(&self) -> bool {
        matches!(self, Self::ProvenExecTarget { .. })
    }
}

/// One installed artifact, and the repo copy it is recorded as coming from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArtifactObservation {
    /// Artifact name, for reporting.
    pub name: String,
    /// Where the installed copy lives on the host.
    pub installed_path: String,
    /// Digest of the bytes on the host now. `None` when the file could not be
    /// read.
    pub installed_digest: Option<String>,
    /// Provenance recorded at deploy time. `None` when nobody wrote it down.
    pub upstream: Option<UpstreamRecord>,
    /// Size of the difference against the repo copy, in changed lines.
    pub delta: Option<LineDelta>,
    /// Whether `installed_path` was proven to be what actually executes.
    pub exec_provenance: ExecProvenance,
}

/// Three-way drift state, with three different remedies.
///
/// Not a boolean, and deliberately not two-valued either. [`Self::BehindRepo`]
/// and [`Self::AheadOfRepo`] produce the identical "the digests differ"
/// observation and want opposite actions: deploy, versus upstream the delta and
/// leave the host alone. Treating the second as the first turns a working host
/// into a broken one: it licenses a redeploy that deletes whatever the host was
/// carrying, with no symptom until something later fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftState {
    /// Installed bytes match the repo copy at the compared ref.
    InSync,
    /// The installed copy is untouched since deploy and the repo has advanced.
    BehindRepo,
    /// The installed copy carries changes the repo does not have.
    AheadOfRepo,
    /// The copies differ, or could not be compared, and the direction is not
    /// recoverable from what was recorded.
    Undetermined,
}

impl DriftState {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InSync => "in_sync",
            Self::BehindRepo => "behind_repo",
            Self::AheadOfRepo => "ahead_of_repo",
            Self::Undetermined => "undetermined",
        }
    }

    /// What the reader should do, phrased as an action.
    #[must_use]
    pub fn remedy(self) -> &'static str {
        match self {
            Self::InSync => "nothing to do",
            Self::BehindRepo => "deploy the repo copy onto the host",
            Self::AheadOfRepo => {
                "upstream the host's changes into the repo first — do NOT redeploy, a redeploy \
                 silently deletes them"
            }
            Self::Undetermined => {
                "record the deploy-time digest for this artifact; do NOT redeploy on the strength \
                 of `differs`, because that is equally consistent with the host being ahead"
            }
        }
    }
}

/// Verdict for one installed artifact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DriftReport {
    /// Artifact name.
    pub artifact: String,
    /// Where the installed copy lives.
    pub installed_path: String,
    /// The three-way state.
    pub state: DriftState,
    /// Roll-up verdict for the shared taxonomy.
    pub verdict: ServiceVerdict,
    /// Size of the delta in changed lines, when the caller measured one.
    pub changed_lines: Option<usize>,
    /// Whether the repo also moved since the deploy — a host that is ahead and
    /// a repo that has advanced have diverged, and the delta must be upstreamed
    /// before either side can be reconciled.
    pub repo_advanced: bool,
    /// Why no comparison could be made. Always `Some` when the verdict is
    /// [`ServiceVerdict::Unknown`], and `None` otherwise.
    pub boundary: Option<Boundary>,
    /// Operator-facing explanation naming what was measured.
    pub detail: String,
}

/// Compare an installed artifact against the repo copy at a named ref, and
/// return which of the three directions it is in plus the size of the delta.
///
/// An artifact with no recorded upstream ref is [`ServiceVerdict::Unknown`] with
/// [`Boundary::Scope`]: there is nothing to compare against a ref nobody wrote
/// down, and a one-sided observation cannot answer a two-sided question.
#[must_use]
pub fn assess_artifact_drift(observation: &ArtifactObservation) -> DriftReport {
    let mut report = DriftReport {
        artifact: observation.name.clone(),
        installed_path: observation.installed_path.clone(),
        state: DriftState::Undetermined,
        verdict: ServiceVerdict::Unknown,
        changed_lines: observation.delta.map(LineDelta::changed),
        repo_advanced: false,
        boundary: None,
        detail: String::new(),
    };

    let name = observation.name.as_str();
    let path = observation.installed_path.as_str();

    // Refuse to compare a path nobody proved is what runs. A populated,
    // plausible directory can be entirely inert, and a number computed from it
    // describes a directory rather than a fleet — the mistake this field exists
    // to make impossible.
    if !observation.exec_provenance.is_proven() {
        report.boundary = Some(Boundary::Scope);
        report.detail = format!(
            "{name}: `{path}` was not proven to be what executes, so any drift number \
             computed against it describes a directory rather than the running code. \
             Resolve the service definition or launcher to its exec target first \
             (boundary: scope)"
        );
        return report;
    }

    let Some(upstream) = observation.upstream.as_ref() else {
        report.boundary = Some(Boundary::Scope);
        report.detail = format!(
            "{name}: the copy at {path} records no upstream ref, so there is no second side to \
             compare it against. {} (boundary: scope)",
            DriftState::Undetermined.remedy()
        );
        return report;
    };

    let Some(installed) = observation.installed_digest.as_deref() else {
        report.boundary = Some(Boundary::Transport);
        report.detail = format!(
            "{name}: the installed copy at {path} could not be read, so no comparison against \
             {} was made (boundary: transport)",
            upstream.compare_ref
        );
        return report;
    };

    let Some(compare) = upstream.compare_digest.as_deref() else {
        report.boundary = Some(Boundary::Transport);
        report.detail = format!(
            "{name}: {} at {} could not be read, so the installed copy at {path} was compared \
             against nothing (boundary: transport)",
            upstream.repo_path, upstream.compare_ref
        );
        return report;
    };

    report.repo_advanced = upstream
        .deployed_digest
        .as_deref()
        .is_some_and(|deployed| deployed != compare);

    classify_drift(&mut report, upstream, installed, compare);
    report
}

/// Decide the drift direction once both digests are in hand.
fn classify_drift(
    report: &mut DriftReport,
    upstream: &UpstreamRecord,
    installed: &str,
    compare: &str,
) {
    let name = report.artifact.as_str();
    let path = report.installed_path.as_str();
    let compare_ref = upstream.compare_ref.as_str();
    let changed = describe_delta(report.changed_lines);

    if installed == compare {
        report.state = DriftState::InSync;
        report.verdict = ServiceVerdict::Served;
        report.detail = format!(
            "{name}: {path} matches {} at {compare_ref}",
            upstream.repo_path
        );
        return;
    }

    let Some(deployed) = upstream.deployed_digest.as_deref() else {
        report.state = DriftState::Undetermined;
        report.verdict = ServiceVerdict::Unknown;
        report.boundary = Some(Boundary::Scope);
        report.detail = format!(
            "{name}: {path} differs from {} at {compare_ref}{changed}, but the deploy-time digest \
             was never recorded, so the direction is unrecoverable. {} (boundary: scope)",
            upstream.repo_path,
            DriftState::Undetermined.remedy()
        );
        return;
    };

    report.verdict = ServiceVerdict::Degraded;
    if installed == deployed {
        report.state = DriftState::BehindRepo;
        report.detail = format!(
            "{name}: {path} is untouched since it was deployed from {}, and {} has advanced at \
             {compare_ref}{changed} — the host is behind the repo. {}",
            upstream.deployed_ref,
            upstream.repo_path,
            DriftState::BehindRepo.remedy()
        );
        return;
    }

    report.state = DriftState::AheadOfRepo;
    let divergence = if report.repo_advanced {
        format!(
            " (and {} has itself advanced since {}, so the two have diverged)",
            upstream.repo_path, upstream.deployed_ref
        )
    } else {
        String::new()
    };
    report.detail = format!(
        "{name}: {path} no longer matches the digest it was deployed with from {}, so it carries \
         host-side changes the repo does not have{changed}{divergence} — the host is AHEAD of the \
         repo, not behind it. {}",
        upstream.deployed_ref,
        DriftState::AheadOfRepo.remedy()
    );
}

/// Render the delta size for a detail string, when one was measured.
fn describe_delta(changed_lines: Option<usize>) -> String {
    match changed_lines {
        Some(lines) => format!(" ({lines} changed line(s))"),
        None => String::new(),
    }
}

/// Worst verdict across a set of guard findings, for a host-level roll-up.
///
/// An empty input is [`ServiceVerdict::Unknown`]: asserting nothing is not the
/// same as asserting everything passed.
#[must_use]
pub fn roll_up(verdicts: &[ServiceVerdict]) -> ServiceVerdict {
    verdicts
        .iter()
        .copied()
        .max()
        .unwrap_or(ServiceVerdict::Unknown)
}

#[cfg(test)]
mod tests;
