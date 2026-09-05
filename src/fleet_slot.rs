//! Fleet **slot** assertions: pure classification for "both macOS VM slots are
//! free, work is queued, and nothing booted — is that a defect, and whose?"
//!
//! Sibling to [`crate::fleet_service`], and deliberately the same shape: no
//! I/O, no ambient clock, no process spawning. Callers gather the supervisor
//! readings and the priority-lane census and pass them in with an explicit
//! `now`. The CLI layer owns every `gh`, `ssh` and `tart` call.
//!
//! ## Why capacity needs its own assertion
//!
//! [`ServiceVerdict::Unserved`] and [`ServiceVerdict::Starved`] both point
//! somewhere real, and neither points here. `Unserved` sends an operator to the
//! routing variables — which, in the state this module was written against,
//! were entirely correct. `Starved` sends them to capacity — which was sitting
//! *idle*, two free slots on the host, refusing itself. The fault was a
//! supervisor **declining** to use capacity it had, so the verdict has to carry
//! *why it declined*; otherwise the remedy is a guess.
//!
//! The measured state: one host, both macOS VM slots stopped, three supervisors
//! reading the same repository's queue, twenty-second ticks.
//!
//! ```text
//! release:         yielding 20s (queued=2 priority_demand=1 running_macos_vms=0/2)
//!                    — priority lane 'Build and Test' has the slot
//! pulp-gate:       waiting 20s (queued=0 running_macos_vms=0/2 priority_demand=0)
//! pulp-gate.slot2: waiting 20s (queued=0 running_macos_vms=0/2 priority_demand=0)
//! ```
//!
//! The release lane reserved a slot for a priority lane that, by that lane's own
//! two supervisors, had nothing queued. The count behind `priority_demand=1`
//! includes jobs already `in_progress` — deliberately, to cover a race where a
//! priority run flips to in-progress via its GitHub-hosted resolver job before
//! its self-hosted leg queues — so a job that can never occupy a self-hosted
//! slot still reserved one. The same counter also fails closed: a scan that
//! errors prints `1` as well.
//!
//! **Four distinct causes print the identical line, and their remedies do not
//! overlap.** That is the whole problem, and [`WithholdCause`] is the answer:
//!
//! | Cause | Correct? | Remedy |
//! |---|---|---|
//! | [`WithholdCause::QueuedPriorityDemand`] | yes | wait; the system is working |
//! | [`WithholdCause::UnusablePriorityJob`] | no | stop counting jobs that cannot take a self-hosted slot |
//! | [`WithholdCause::FailClosedScan`] | defensible, but invisible | fix the scan |
//! | [`WithholdCause::HostHealthSaturation`] | yes | free memory; forcing a boot would harm |
//!
//! ## Verdict mapping, and why it is a mapping rather than a new enum
//!
//! [`ServiceVerdict`] is shared and closed on purpose: a second parallel verdict
//! enum would force every roll-up to learn two severity orders and would let the
//! two drift. So withholding is a **dedicated report type** that maps onto the
//! shared verdict:
//!
//! * `QueuedPriorityDemand` → [`ServiceVerdict::Served`]. Holding a slot for
//!   genuinely queued priority work is the reservation doing its job.
//! * `HostHealthSaturation` → [`ServiceVerdict::Served`]. A host refusing to
//!   boot into memory saturation is protecting the jobs already running;
//!   booting anyway is the harm. Raising here would train operators to ignore
//!   the raise — which is precisely how the real defect hid among identical
//!   lines.
//! * `UnusablePriorityJob` → [`ServiceVerdict::Degraded`]. Service continues,
//!   but it is consuming a budget it should not: a slot per tick, indefinitely.
//! * `FailClosedScan` → [`ServiceVerdict::Degraded`]. Failing closed is the
//!   right reflex and the wrong steady state; the reservation is not evidence of
//!   demand, and nothing currently says so out loud.
//! * `Unreadable` → [`ServiceVerdict::Unknown`] plus a named [`Boundary`]. A
//!   supervisor log that could not be read or understood is never a pass.
//!
//! `Idle` is deliberately unused. It asserts that nothing is asking, and in
//! every withholding case something *is* asking; it would also outrank `Served`
//! in a roll-up and make a quiet, healthy supervisor look worse than a busy one.
//! Widening `ServiceVerdict` with a `Withheld` variant was the other option and
//! was rejected: the four causes do not share a severity, so a single new
//! variant could not carry the distinction that is the entire point.
//!
//! ## Cross-supervisor coherence needs no oracle
//!
//! [`assess_supervisor_coherence`] is the assertion that would have fired on the
//! reading above immediately, with nothing to compare against. Two supervisors
//! on the same host watching the same repository, where one yields *citing* the
//! other's lane while that lane's own supervisors report zero queued work, prove
//! between them that at least one reading is wrong. The disagreement is the
//! signal; no ground truth is required, and no scan has to be re-run.
//!
//! What it deliberately does **not** encode is "all the numbers must match".
//! Supervisors watch different label sets, so two lanes reporting different
//! `queued=` counts is the normal case, not a defect. Only the cited pair is
//! checked.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::fleet_service::{Boundary, ServiceVerdict};

/// Default age, in seconds, that a supervisor's own queued work must reach
/// before a declined free slot is judged rather than tolerated.
///
/// Shorter than [`crate::fleet_service::DEFAULT_UNSERVED_AFTER_SECS`] on
/// purpose: an unserved lane may be waiting on a human, whereas a withheld slot
/// is re-decided every twenty seconds by a process that already has the
/// capacity in hand. A just-in-time VM boot plus runner registration lands
/// inside five minutes on this fleet, so demand older than that is past any
/// legitimate transient.
pub const DEFAULT_WITHHELD_AFTER_SECS: i64 = 300;

/// Why a supervisor tick declined to take a free slot, as the supervisor itself
/// stated it.
///
/// The stated reason is a fact about the supervisor, not a diagnosis: three of
/// the four causes in [`WithholdCause`] all present as
/// [`YieldState::ForPriorityLane`], which is why the citation alone can never
/// close the question.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum YieldState {
    /// The supervisor is ready to take work (`waiting …`).
    Waiting,
    /// Yielding because a priority lane is said to hold the slot.
    ForPriorityLane {
        /// The lane named in the citation, as written — a workflow name such as
        /// `Build and Test`, not a supervisor name.
        lane: String,
    },
    /// Yielding because the host-health gate reported saturation
    /// (`host_health_yield=1`).
    ForHostHealth,
    /// Yielding with no reason given. Fail-closed: an instrument that declines
    /// without saying why has not been read, so this is never a pass.
    Unexplained,
}

impl YieldState {
    /// Whether this tick declined the slot.
    #[must_use]
    pub fn yielded(&self) -> bool {
        !matches!(self, Self::Waiting)
    }

    /// Snake-case discriminant, for JSON output and grouping.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::ForPriorityLane { .. } => "for_priority_lane",
            Self::ForHostHealth => "for_host_health",
            Self::Unexplained => "unexplained",
        }
    }
}

/// One supervisor tick, as read from its log.
///
/// `host` and `repo` are the join keys for [`assess_supervisor_coherence`]:
/// contradictions are only meaningful between supervisors sharing both.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SupervisorObservation {
    /// Host the supervisor runs on.
    pub host: String,
    /// Repository whose queue it scanned, as `owner/name`.
    pub repo: String,
    /// Supervisor name, as it labels its own log lines (`release`,
    /// `pulp-gate.slot2`).
    pub lane: String,
    /// The priority workflow this supervisor's slot serves, if it serves one.
    ///
    /// Supplied from configuration, never from the log line: a citation names a
    /// *workflow* (`Build and Test`) while a supervisor names *itself*
    /// (`pulp-gate`), so without this field the two can never be joined and the
    /// coherence assertion silently checks nothing.
    pub priority_lane: Option<String>,
    /// Jobs the supervisor saw queued for its own lane.
    pub queued: u32,
    /// The priority-demand count it reserved capacity against.
    pub priority_demand: u32,
    /// macOS VMs running on the host at tick time.
    pub running_vms: u32,
    /// macOS VM slots the host is allowed to run.
    pub capacity: u32,
    /// What the tick did, and the reason it gave.
    pub yield_state: YieldState,
}

impl SupervisorObservation {
    /// Slots free at tick time. Saturating: a host over its own cap has none.
    #[must_use]
    pub fn free_slots(&self) -> u32 {
        self.capacity.saturating_sub(self.running_vms)
    }

    /// Whether this tick declined the slot.
    #[must_use]
    pub fn yielded(&self) -> bool {
        self.yield_state.yielded()
    }

    /// Whether this supervisor serves the named priority workflow.
    ///
    /// Case-insensitive: the citation is free text copied out of a workflow
    /// name, and casing has already differed between the two in practice.
    #[must_use]
    pub fn serves_priority_lane(&self, lane: &str) -> bool {
        self.priority_lane
            .as_deref()
            .is_some_and(|served| served.eq_ignore_ascii_case(lane))
    }
}

/// Per-supervisor facts a log line cannot carry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisorContext<'a> {
    /// Host the supervisor runs on.
    pub host: &'a str,
    /// Repository it scans, as `owner/name`.
    pub repo: &'a str,
    /// The priority workflow this supervisor serves, from configuration.
    pub priority_lane: Option<&'a str>,
}

/// A supervisor reading that could not be turned into an observation.
///
/// Carries a [`Boundary`] rather than a bare message because the remedies
/// diverge: a garbled line is fixed in the supervisor's formatter, an absent log
/// is fixed on the host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SupervisorLogError {
    /// Which boundary stopped the read.
    pub boundary: Boundary,
    /// What was wrong, in operator terms.
    pub detail: String,
    /// The offending input, so the reader can see it without re-fetching.
    pub raw: String,
}

impl SupervisorLogError {
    /// Build the [`ServiceVerdict::Unknown`] report this failure must produce.
    ///
    /// Exists so no caller can accidentally drop an unreadable supervisor on the
    /// floor: the only thing you can do with the error is turn it into a report
    /// that raises.
    #[must_use]
    pub fn into_report(self, host: &str, repo: &str, lane: &str) -> SlotWithholdReport {
        let detail = format!(
            "supervisor reading unusable ({}): {} — no slot claim is made. Next: {}",
            self.boundary.as_str(),
            self.detail,
            self.boundary.next_action()
        );
        SlotWithholdReport {
            host: host.to_owned(),
            repo: repo.to_owned(),
            lane: lane.to_owned(),
            cause: WithholdCause::Unreadable,
            verdict: ServiceVerdict::Unknown,
            boundary: Some(self.boundary),
            free_slots: 0,
            oldest_demand_secs: None,
            detail,
        }
    }
}

/// Parse one supervisor log line into an observation.
///
/// Both live forms are accepted:
///
/// ```text
/// release:  yielding 20s (queued=2 priority_demand=1 running_macos_vms=0/2) — priority lane 'Build and Test' has the slot
/// pulp-gate: waiting 20s (queued=0 running_macos_vms=0/2 priority_demand=0)
/// ```
///
/// Unknown `key=value` fields are ignored so a supervisor may add telemetry
/// without breaking the parser, but a *known* key with an unreadable value is an
/// error rather than a default — a missing count that silently reads as zero is
/// the same class of bug as the counter this module exists to judge.
pub fn parse_supervisor_line(
    ctx: SupervisorContext<'_>,
    line: &str,
) -> Result<SupervisorObservation, SupervisorLogError> {
    let trimmed = line.trim();
    let (supervisor, rest) = trimmed
        .split_once(':')
        .ok_or_else(|| log_error(Boundary::Parse, "no `<lane>:` prefix", trimmed))?;
    let supervisor = supervisor.trim();
    if supervisor.is_empty() {
        return Err(log_error(Boundary::Parse, "empty lane name", trimmed));
    }

    let rest = rest.trim_start();
    let verb = rest.split_whitespace().next().unwrap_or_default();
    let yielded = match verb {
        "yielding" => true,
        "waiting" => false,
        other => {
            return Err(log_error(
                Boundary::Parse,
                &format!("unrecognised verb `{other}` (expected `yielding` or `waiting`)"),
                trimmed,
            ));
        }
    };

    let open = rest
        .find('(')
        .ok_or_else(|| log_error(Boundary::Parse, "no `(...)` field group", trimmed))?;
    let close = rest[open..]
        .find(')')
        .map(|offset| open + offset)
        .ok_or_else(|| log_error(Boundary::Parse, "unterminated `(...)` field group", trimmed))?;
    let fields = parse_fields(&rest[open + 1..close], trimmed)?;
    let (running_vms, capacity) = fields.slots;

    Ok(SupervisorObservation {
        host: ctx.host.to_owned(),
        repo: ctx.repo.to_owned(),
        lane: supervisor.to_owned(),
        priority_lane: ctx.priority_lane.map(str::to_owned),
        queued: fields.queued,
        priority_demand: fields.priority_demand,
        running_vms,
        capacity,
        yield_state: yield_state_for(yielded, fields.host_health_yield, &rest[close + 1..]),
    })
}

/// The `key=value` group inside a supervisor line's parentheses.
struct SupervisorFields {
    queued: u32,
    priority_demand: u32,
    slots: (u32, u32),
    host_health_yield: bool,
}

fn parse_fields(group: &str, raw: &str) -> Result<SupervisorFields, SupervisorLogError> {
    let mut queued = None;
    let mut priority_demand = None;
    let mut slots = None;
    let mut host_health_yield = false;

    for token in group.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        match key {
            "queued" => queued = Some(parse_count(key, value, raw)?),
            "priority_demand" => priority_demand = Some(parse_count(key, value, raw)?),
            "running_macos_vms" | "running_vms" => slots = Some(parse_slots(value, raw)?),
            "host_health_yield" => host_health_yield = matches!(value, "1" | "true"),
            // Forward compatibility: a supervisor may report fields this
            // assertion does not read yet.
            _ => {}
        }
    }

    Ok(SupervisorFields {
        queued: require_field(queued, "queued", raw)?,
        priority_demand: require_field(priority_demand, "priority_demand", raw)?,
        slots: require_field(slots, "running_macos_vms", raw)?,
        host_health_yield,
    })
}

fn require_field<T>(value: Option<T>, key: &str, raw: &str) -> Result<T, SupervisorLogError> {
    value.ok_or_else(|| log_error(Boundary::Parse, &format!("missing `{key}=`"), raw))
}

fn parse_count(key: &str, value: &str, raw: &str) -> Result<u32, SupervisorLogError> {
    value.parse::<u32>().map_err(|_| {
        log_error(
            Boundary::Parse,
            &format!("`{key}={value}` is not a count"),
            raw,
        )
    })
}

fn parse_slots(value: &str, raw: &str) -> Result<(u32, u32), SupervisorLogError> {
    let (running, capacity) = value.split_once('/').ok_or_else(|| {
        log_error(
            Boundary::Parse,
            &format!("`running_macos_vms={value}` is not a `running/capacity` pair"),
            raw,
        )
    })?;
    Ok((
        parse_count("running_macos_vms", running, raw)?,
        parse_count("running_macos_vms", capacity, raw)?,
    ))
}

/// Decide the stated reason for a tick.
///
/// Host-health saturation wins over a priority citation when both appear: a
/// saturated host must not boot regardless of whether the citation is sound, so
/// naming the citation first would send the operator to a counter while the
/// machine is out of memory.
fn yield_state_for(yielded: bool, host_health_yield: bool, suffix: &str) -> YieldState {
    if !yielded {
        return YieldState::Waiting;
    }
    if host_health_yield {
        return YieldState::ForHostHealth;
    }
    match cited_priority_lane(suffix) {
        Some(lane) => YieldState::ForPriorityLane { lane },
        None => YieldState::Unexplained,
    }
}

/// Extract the lane from a `— priority lane 'X' has the slot` suffix.
fn cited_priority_lane(suffix: &str) -> Option<String> {
    let after = suffix.split_once("priority lane")?.1.trim_start();
    let quoted = after.strip_prefix('\'')?;
    let (lane, _) = quoted.split_once('\'')?;
    let lane = lane.trim();
    (!lane.is_empty()).then(|| lane.to_owned())
}

fn log_error(boundary: Boundary, detail: &str, raw: &str) -> SupervisorLogError {
    SupervisorLogError {
        boundary,
        detail: detail.to_owned(),
        raw: raw.to_owned(),
    }
}

/// State of a job the priority-demand scan counted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorityJobState {
    /// Waiting for a runner. The only state that can still take a free slot.
    Queued,
    /// GitHub has picked a runner for it.
    Assigned,
    /// Executing. On a priority workflow this is commonly the hosted resolver
    /// job, whose progress says nothing about the self-hosted leg.
    InProgress,
}

impl PriorityJobState {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Assigned => "assigned",
            Self::InProgress => "in_progress",
        }
    }
}

/// One job behind a `priority_demand` count.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PriorityJob {
    /// Job name, for the operator to look up.
    pub name: String,
    /// What the job is doing.
    pub state: PriorityJobState,
    /// Whether the job routes to a self-hosted runner at all. A hosted job
    /// cannot occupy a macOS VM slot no matter what state it is in.
    pub routes_self_hosted: bool,
}

impl PriorityJob {
    /// Whether this job could actually take the withheld slot.
    ///
    /// The reservation is only justified by a job that is both self-hosted and
    /// still queued. Counting anything else reserves capacity for work that will
    /// never arrive.
    #[must_use]
    pub fn can_take_a_free_slot(&self) -> bool {
        self.routes_self_hosted && self.state == PriorityJobState::Queued
    }
}

/// What the priority-demand scan actually found, alongside the supervisor's own
/// backlog.
///
/// This is the evidence the log line lacks. Without it the four causes are
/// indistinguishable, which is the defect being judged.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PriorityDemandEvidence {
    /// Boundary that stopped the scan, if it failed and the count was produced
    /// by failing closed rather than by counting.
    pub scan_boundary: Option<Boundary>,
    /// The jobs the scan counted toward `priority_demand`.
    pub counted_jobs: Vec<PriorityJob>,
    /// When each job on the observing supervisor's own lane entered the queue.
    /// This is the demand the withheld slot is denying.
    pub own_queued_since: Vec<DateTime<Utc>>,
}

/// Tunables for [`assess_slot_withholding`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlotWithholdThresholds {
    /// How old the observing supervisor's own demand must be before a declined
    /// free slot is judged.
    pub withheld_after_secs: i64,
}

impl Default for SlotWithholdThresholds {
    fn default() -> Self {
        Self {
            withheld_after_secs: DEFAULT_WITHHELD_AFTER_SECS,
        }
    }
}

/// Why capacity was withheld, at the resolution the remedy needs.
///
/// Ordering is declaration order and carries no meaning; severity lives in the
/// [`ServiceVerdict`] each cause maps to, so there is exactly one severity order
/// in the crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WithholdCause {
    /// Nothing is being withheld: the supervisor is ready, or no slot is free,
    /// or its own demand is still inside the transient window.
    None,
    /// A priority job is genuinely queued and can take the slot. Correct.
    QueuedPriorityDemand,
    /// The reservation is held for work that cannot occupy a self-hosted slot —
    /// already assigned or in progress, hosted-only, or not present at all.
    UnusablePriorityJob,
    /// The scan failed and the count was produced by failing closed, so the
    /// reservation is not evidence of demand.
    FailClosedScan,
    /// The host-health gate reported saturation. Correct: booting into it would
    /// harm the jobs already running.
    HostHealthSaturation,
    /// The supervisor reading could not be understood or was absent.
    Unreadable,
}

impl WithholdCause {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::QueuedPriorityDemand => "queued_priority_demand",
            Self::UnusablePriorityJob => "unusable_priority_job",
            Self::FailClosedScan => "fail_closed_scan",
            Self::HostHealthSaturation => "host_health_saturation",
            Self::Unreadable => "unreadable",
        }
    }

    /// The shared verdict this cause maps onto. See the module docs for the
    /// justification of each arm.
    #[must_use]
    pub fn verdict(self) -> ServiceVerdict {
        match self {
            Self::None | Self::QueuedPriorityDemand | Self::HostHealthSaturation => {
                ServiceVerdict::Served
            }
            Self::UnusablePriorityJob | Self::FailClosedScan => ServiceVerdict::Degraded,
            Self::Unreadable => ServiceVerdict::Unknown,
        }
    }

    /// Whether the withholding was the right thing to do.
    ///
    /// Distinct from `!verdict().is_raise()` in intent: this asks whether the
    /// supervisor behaved correctly, which is the question an operator triaging
    /// a stalled queue is actually asking.
    #[must_use]
    pub fn is_correct_behaviour(self) -> bool {
        matches!(
            self,
            Self::None | Self::QueuedPriorityDemand | Self::HostHealthSaturation
        )
    }
}

/// Verdict for one supervisor tick that had a slot available.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SlotWithholdReport {
    /// Host the supervisor runs on.
    pub host: String,
    /// Repository it scanned.
    pub repo: String,
    /// Supervisor name.
    pub lane: String,
    /// Which of the four causes applies.
    pub cause: WithholdCause,
    /// The shared verdict, from [`WithholdCause::verdict`].
    pub verdict: ServiceVerdict,
    /// Which boundary prevented a measurement. Always `Some` when the verdict is
    /// [`ServiceVerdict::Unknown`], and always `None` otherwise — an unknown
    /// that cannot say why is the failure this field exists to prevent.
    pub boundary: Option<Boundary>,
    /// Slots free at tick time.
    pub free_slots: u32,
    /// Age of the oldest job queued on this supervisor's own lane.
    pub oldest_demand_secs: Option<i64>,
    /// Operator-facing explanation naming what was measured and what to do.
    pub detail: String,
}

/// Classify one supervisor tick: was declining this free slot a defect?
///
/// `evidence` is mandatory rather than optional because the log line alone
/// cannot separate the four causes — that indistinguishability is the fault
/// being judged, and an assertion that accepted only the line would reproduce
/// it.
#[must_use]
pub fn assess_slot_withholding(
    observation: &SupervisorObservation,
    evidence: &PriorityDemandEvidence,
    thresholds: SlotWithholdThresholds,
    now: DateTime<Utc>,
) -> SlotWithholdReport {
    let free_slots = observation.free_slots();
    let oldest_demand_secs = evidence
        .own_queued_since
        .iter()
        .map(|since| (now - *since).num_seconds())
        .max();

    let (cause, boundary, detail) = classify_withholding(
        observation,
        evidence,
        thresholds,
        free_slots,
        oldest_demand_secs,
    );

    SlotWithholdReport {
        host: observation.host.clone(),
        repo: observation.repo.clone(),
        lane: observation.lane.clone(),
        cause,
        verdict: cause.verdict(),
        boundary,
        free_slots,
        oldest_demand_secs,
        detail,
    }
}

/// The judgement half of [`assess_slot_withholding`], kept separate from the
/// arithmetic so the part worth arguing about reads on its own.
fn classify_withholding(
    observation: &SupervisorObservation,
    evidence: &PriorityDemandEvidence,
    thresholds: SlotWithholdThresholds,
    free_slots: u32,
    oldest_demand_secs: Option<i64>,
) -> (WithholdCause, Option<Boundary>, String) {
    // Checked before the gates below: a supervisor that declines without saying
    // why has not been read, and an unread instrument is never a pass — not even
    // when nothing is currently being denied.
    if observation.yield_state == YieldState::Unexplained {
        return (
            WithholdCause::Unreadable,
            Some(Boundary::Parse),
            format!(
                "`{}` yielded without naming a reason, so the withholding cannot be judged. \
                 Next: {}",
                observation.lane,
                Boundary::Parse.next_action()
            ),
        );
    }

    if let Some(detail) =
        not_withholding_detail(observation, thresholds, free_slots, oldest_demand_secs)
    {
        return (WithholdCause::None, None, detail);
    }

    let age = oldest_demand_secs.unwrap_or_default();
    if observation.yield_state == YieldState::ForHostHealth {
        return (
            WithholdCause::HostHealthSaturation,
            None,
            format!(
                "host_health_yield=1 on {host}: the host refused to boot into memory saturation \
                 while {free_slots} slot(s) sat free and `{lane}` had work waiting {age}s. This is \
                 a correct refusal, not a slot defect — forcing a boot would harm the jobs already \
                 running. Free memory on {host}.",
                host = observation.host,
                lane = observation.lane,
            ),
        );
    }

    let cited = match &observation.yield_state {
        YieldState::ForPriorityLane { lane } => lane.as_str(),
        _ => "",
    };
    classify_priority_citation(observation, evidence, cited, free_slots, age)
}

/// Distinguish causes 1, 2 and 3, which all print the same citation.
fn classify_priority_citation(
    observation: &SupervisorObservation,
    evidence: &PriorityDemandEvidence,
    cited: &str,
    free_slots: u32,
    age: i64,
) -> (WithholdCause, Option<Boundary>, String) {
    let lane = &observation.lane;
    let demand = observation.priority_demand;

    if let Some(boundary) = evidence.scan_boundary {
        return (
            WithholdCause::FailClosedScan,
            None,
            format!(
                "the priority-demand scan failed ({boundary}) and fail-closed to \
                 priority_demand={demand}, so `{lane}` reserved {free_slots} free slot(s) against a \
                 count that was never measured while its own work waited {age}s. Failing closed is \
                 defensible and invisible: fix the scan. Next: {next}",
                boundary = boundary.as_str(),
                next = boundary.next_action(),
            ),
        );
    }

    let usable: Vec<&PriorityJob> = evidence
        .counted_jobs
        .iter()
        .filter(|job| job.can_take_a_free_slot())
        .collect();
    if let Some(first) = usable.first() {
        return (
            WithholdCause::QueuedPriorityDemand,
            None,
            format!(
                "priority lane '{cited}' has {count} job(s) genuinely queued for a self-hosted \
                 slot (e.g. `{name}`), so `{lane}` holding {free_slots} slot(s) is the reservation \
                 working as intended. Wait.",
                count = usable.len(),
                name = first.name,
            ),
        );
    }

    (
        WithholdCause::UnusablePriorityJob,
        None,
        format!(
            "priority lane '{cited}' has no job that can occupy a self-hosted slot — \
             {census} — yet priority_demand={demand} reserved {free_slots} free slot(s) while \
             `{lane}` had work waiting {age}s. Stop counting jobs that cannot take the slot.",
            census = describe_unusable_jobs(&evidence.counted_jobs),
        ),
    )
}

/// Render the counted-job census for the cause-2 detail, so the reader can see
/// which job the reservation was held for.
fn describe_unusable_jobs(jobs: &[PriorityJob]) -> String {
    if jobs.is_empty() {
        return "the scan succeeded and counted no job at all".to_owned();
    }
    let listed = jobs
        .iter()
        .map(|job| {
            let routing = if job.routes_self_hosted {
                "self-hosted"
            } else {
                "hosted-only"
            };
            format!("`{}` is {} and {routing}", job.name, job.state.as_str())
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("the scan counted {} job(s): {listed}", jobs.len())
}

/// Why this tick is not withholding anything, if it is not.
fn not_withholding_detail(
    observation: &SupervisorObservation,
    thresholds: SlotWithholdThresholds,
    free_slots: u32,
    oldest_demand_secs: Option<i64>,
) -> Option<String> {
    let lane = &observation.lane;
    if !observation.yielded() {
        return Some(format!(
            "`{lane}` is waiting and ready to take work; nothing is withheld"
        ));
    }
    if free_slots == 0 {
        return Some(format!(
            "`{lane}` yielded with no free slot to withhold ({}/{} macOS VMs running)",
            observation.running_vms, observation.capacity
        ));
    }
    match oldest_demand_secs {
        None => Some(format!(
            "`{lane}` yielded with {free_slots} slot(s) free but nothing of its own queued"
        )),
        Some(age) if age < thresholds.withheld_after_secs => Some(format!(
            "`{lane}` yielded with {free_slots} slot(s) free and its oldest work {age}s old, \
             inside the {}s transient window",
            thresholds.withheld_after_secs
        )),
        Some(_) => None,
    }
}

/// One proven inconsistency between two supervisors on the same host and repo.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Contradiction {
    /// Host both supervisors run on.
    pub host: String,
    /// Repository both scanned.
    pub repo: String,
    /// The supervisor that yielded, citing another lane.
    pub citing_lane: String,
    /// The priority workflow it cited.
    pub cited_lane: String,
    /// The supervisors serving that workflow, which reported no queued work.
    pub corroborating_lanes: Vec<String>,
    /// The citing supervisor's own queued count.
    pub citing_queued: u32,
    /// The priority-demand count it reserved against.
    pub citing_priority_demand: u32,
    /// Slots free at the citing supervisor's tick.
    pub free_slots: u32,
    /// Operator-facing statement of the contradiction.
    pub detail: String,
}

/// Verdict for a set of supervisor observations checked against each other.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CoherenceReport {
    /// The shared verdict.
    pub verdict: ServiceVerdict,
    /// Which boundary prevented a measurement, when the verdict is
    /// [`ServiceVerdict::Unknown`].
    pub boundary: Option<Boundary>,
    /// How many observations were compared.
    pub observed: usize,
    /// Proven inconsistencies.
    pub contradictions: Vec<Contradiction>,
    /// Citations that no observed supervisor could confirm or deny, as
    /// `lane -> cited lane` phrases. Not a pass: the check abstained.
    pub uncorroborated: Vec<String>,
    /// Operator-facing summary.
    pub detail: String,
}

/// Check a host's supervisors against each other, with no external oracle.
///
/// Only one shape is treated as contradictory, and it is the one that needs no
/// ground truth: a supervisor yields citing a priority lane, while every
/// observed supervisor *serving* that lane reports zero queued work. Between
/// them those readings cannot both be right.
///
/// Differing `queued=` counts across lanes are **not** contradictory — lanes
/// watch different label sets, so disagreement there is the normal case.
///
/// Observations are compared only within the same `host` + `repo`, so a fleet's
/// worth may be passed in one call.
#[must_use]
pub fn assess_supervisor_coherence(observations: &[SupervisorObservation]) -> CoherenceReport {
    let mut report = CoherenceReport {
        verdict: ServiceVerdict::Served,
        boundary: None,
        observed: observations.len(),
        contradictions: Vec::new(),
        uncorroborated: Vec::new(),
        detail: String::new(),
    };

    if observations.len() < 2 {
        report.verdict = ServiceVerdict::Unknown;
        report.boundary = Some(Boundary::Scope);
        report.detail = format!(
            "{} observation(s): a supervisor cannot contradict itself, so nothing was checked. \
             Next: {}",
            observations.len(),
            Boundary::Scope.next_action()
        );
        return report;
    }

    for citing in observations {
        let YieldState::ForPriorityLane { lane: cited } = &citing.yield_state else {
            continue;
        };
        let peers = peers_serving(observations, citing, cited);
        if peers.is_empty() {
            report
                .uncorroborated
                .push(format!("`{}` cited '{cited}'", citing.lane));
            continue;
        }
        if peers.iter().all(|peer| peer.queued == 0) {
            report
                .contradictions
                .push(contradiction(citing, cited, &peers));
        }
    }

    finish_coherence(report)
}

/// Observations on the same host and repo that serve the cited lane.
fn peers_serving<'a>(
    observations: &'a [SupervisorObservation],
    citing: &SupervisorObservation,
    cited: &str,
) -> Vec<&'a SupervisorObservation> {
    observations
        .iter()
        .filter(|peer| {
            peer.host == citing.host
                && peer.repo == citing.repo
                && peer.lane != citing.lane
                && peer.serves_priority_lane(cited)
        })
        .collect()
}

fn contradiction(
    citing: &SupervisorObservation,
    cited: &str,
    peers: &[&SupervisorObservation],
) -> Contradiction {
    let names: Vec<String> = peers.iter().map(|peer| peer.lane.clone()).collect();
    let detail = format!(
        "`{citing_lane}` yielded citing priority lane '{cited}', but the {count} supervisor(s) \
         serving '{cited}' on {host} ({listed}) each report queued=0. Between them these readings \
         cannot both be right, and {free} macOS slot(s) are free while `{citing_lane}` has \
         {queued} job(s) of its own queued.",
        citing_lane = citing.lane,
        count = peers.len(),
        host = citing.host,
        listed = names.join(", "),
        free = citing.free_slots(),
        queued = citing.queued,
    );
    Contradiction {
        host: citing.host.clone(),
        repo: citing.repo.clone(),
        citing_lane: citing.lane.clone(),
        cited_lane: cited.to_owned(),
        corroborating_lanes: names,
        citing_queued: citing.queued,
        citing_priority_demand: citing.priority_demand,
        free_slots: citing.free_slots(),
        detail,
    }
}

/// Settle the verdict once every citation has been checked.
///
/// A proven contradiction is reported as itself even though `Unknown` sorts more
/// severe in [`ServiceVerdict`]'s ordering: `Unknown` means "could not measure",
/// and here something *was* measured. The citations that could not be checked
/// stay listed in `uncorroborated` so the gap remains visible.
fn finish_coherence(mut report: CoherenceReport) -> CoherenceReport {
    if !report.contradictions.is_empty() {
        // Work exists, capacity exists, and neither is reaching the other:
        // `Starved` is exactly that, and points the reader at scheduling rather
        // than at routing. Without idle slots and waiting work the inconsistency
        // is real but denies nothing yet, which is a budget being consumed.
        let denying = report
            .contradictions
            .iter()
            .any(|found| found.free_slots > 0 && found.citing_queued > 0);
        report.verdict = if denying {
            ServiceVerdict::Starved
        } else {
            ServiceVerdict::Degraded
        };
        report.detail = report
            .contradictions
            .iter()
            .map(|found| found.detail.clone())
            .collect::<Vec<_>>()
            .join(" ");
        return report;
    }

    if !report.uncorroborated.is_empty() {
        report.verdict = ServiceVerdict::Unknown;
        report.boundary = Some(Boundary::Scope);
        report.detail = format!(
            "no supervisor observed on this host serves the cited lane(s): {}. Nothing was \
             disproven and nothing was confirmed. Next: {}",
            report.uncorroborated.join(", "),
            Boundary::Scope.next_action()
        );
        return report;
    }

    report.detail = format!(
        "{} supervisor observation(s) agree: every priority-lane citation is corroborated by a \
         supervisor of that lane reporting queued work",
        report.observed
    );
    report
}

#[cfg(test)]
mod tests;
