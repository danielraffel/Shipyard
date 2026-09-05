//! Fleet **service** assertions: pure classification for "is this lane actually
//! being served?", as opposed to "is this host up?".
//!
//! Sibling to [`crate::runner_watchdog`], and deliberately the same shape: no
//! I/O, no ambient clock, no process spawning. Callers gather the inputs — the
//! runner census, the declared lanes, the queued demand — and pass them in with
//! an explicit `now`. The CLI layer owns every `gh` call.
//!
//! ## Why a separate taxonomy
//!
//! [`RunnerHealth`](crate::runner_watchdog::RunnerHealth) answers "is this
//! runner wedged?". That is a different question from "is anything serving this
//! lane?", and collapsing the second into a boolean is what allowed a Linux
//! lane to go unserved for nineteen days while every passive signal read
//! healthy: the pool unit was `active (running)` (it was — and failing every 30
//! seconds), the required gate kept merging, and a hosted fallback absorbed the
//! work so the lane's *output* stayed green while its local half was dead.
//!
//! So each assertion returns a **typed verdict**, and the distinctions in
//! [`ServiceVerdict`] are the product:
//!
//! * `Unserved` vs `Idle` — an idle just-in-time pool legitimately registers
//!   nothing, so an empty census cannot by itself mean a lane is broken. This
//!   pair is only decidable by pairing the census with *demand*.
//! * `Unserved` vs `Starved` — a job queued on labels nothing advertises is
//!   unschedulable and will wait forever; a job queued while an online runner
//!   does advertise them is a scheduling or capacity problem. Same symptom,
//!   opposite remedies.
//! * `Unknown` vs anything else — a census that could not be read is not a
//!   pass. Folding an unreadable instrument into "healthy" is the failure mode
//!   these assertions exist to end.
//!
//! ## Both runner scopes are mandatory
//!
//! `repos/{owner}/{repo}/actions/runners` **omits org-registered runners
//! entirely**. On the fleet this was written against, three of the six declared
//! self-hosted lanes are served only by org-scope runners, so a repo-scope-only
//! census reports them unserved while they are online — the identical empty
//! reading it gives when the host is genuinely dead. [`assess_lane_service`]
//! therefore takes one census spanning both scopes, and records which scope
//! satisfied the lane so that blindness is visible in the output rather than
//! inferred.

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Default age, in seconds, that queued demand must reach before an unserved
/// lane is reported as a fault rather than as a transient.
///
/// Matches the queued-age threshold the fleet liveness tick already uses, so a
/// just-in-time pool that is mid-boot is not slandered as broken.
pub const DEFAULT_UNSERVED_AFTER_SECS: i64 = 900;

/// Which GitHub runner registration scope a runner was observed in.
///
/// Recorded per matched runner so that "this lane is served, but only by the
/// scope the obvious query omits" is legible in the output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerScope {
    /// `repos/{owner}/{repo}/actions/runners`.
    Repo,
    /// `orgs/{org}/actions/runners`.
    Org,
}

impl RunnerScope {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::Org => "org",
        }
    }
}

/// One runner observed in the census, in whichever scope it registered into.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RegisteredRunner {
    /// Runner name as GitHub reports it.
    pub name: String,
    /// Scope this observation came from.
    pub scope: RunnerScope,
    /// Whether GitHub reports the runner `online`.
    pub online: bool,
    /// Whether GitHub reports the runner busy with a job.
    pub busy: bool,
    /// Labels the runner advertises.
    pub labels: Vec<String>,
}

impl RegisteredRunner {
    /// Whether this runner advertises every label in `required`.
    ///
    /// GitHub schedules a job only onto a runner carrying **all** requested
    /// labels, and treats labels case-insensitively (`macOS` and `macos` are
    /// the same label), so the match is a case-insensitive superset test.
    #[must_use]
    pub fn advertises_all(&self, required: &[String]) -> bool {
        required.iter().all(|want| {
            self.labels
                .iter()
                .any(|have| have.eq_ignore_ascii_case(want))
        })
    }
}

/// What a `*_RUNS_ON_JSON`-style routing variable resolves to.
///
/// The live fleet uses three different encodings in the same namespace, so a
/// parser that assumes a JSON array silently mis-reads a third of them.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaneDeclaration {
    /// A JSON array containing `self-hosted` — a lane this fleet must serve.
    SelfHosted {
        /// Every label the lane requests.
        labels: Vec<String>,
    },
    /// A hosted runner label (`"macos-15"`, `["ubuntu-latest"]`). Nothing local
    /// is expected to serve it, so it is outside these assertions.
    Hosted {
        /// The requested hosted label set.
        labels: Vec<String>,
    },
    /// A bare routing sentinel such as `local-only` that names no runner label.
    Sentinel {
        /// The literal value.
        value: String,
    },
    /// The value could not be parsed into any of the above.
    ///
    /// Deliberately distinct from an empty lane: an unreadable declaration is
    /// an [`ServiceVerdict::Unknown`], never a pass.
    Unparsable {
        /// The raw variable value, for the operator to read.
        raw: String,
    },
}

impl LaneDeclaration {
    /// Snake-case discriminant, for JSON output and grouping.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SelfHosted { .. } => "self_hosted",
            Self::Hosted { .. } => "hosted",
            Self::Sentinel { .. } => "sentinel",
            Self::Unparsable { .. } => "unparsable",
        }
    }

    /// Labels this lane requests, if it requests any.
    #[must_use]
    pub fn labels(&self) -> &[String] {
        match self {
            Self::SelfHosted { labels } | Self::Hosted { labels } => labels,
            Self::Sentinel { .. } | Self::Unparsable { .. } => &[],
        }
    }
}

/// Parse a routing variable value into a [`LaneDeclaration`].
///
/// Handles every encoding observed on the live fleet:
///
/// * `["self-hosted","macOS","ARM64","pulp-build"]` — a JSON array;
/// * `"macos-15"` — a JSON string;
/// * `macos-15` — a bare string that is not valid JSON at all;
/// * `local-only` — a routing sentinel naming no label.
///
/// A lane is "self-hosted" when it requests the `self-hosted` label, which is
/// how GitHub itself distinguishes the two populations.
#[must_use]
pub fn parse_runs_on(raw: &str) -> LaneDeclaration {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return LaneDeclaration::Unparsable {
            raw: raw.to_owned(),
        };
    }

    let mut labels = match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Array(items)) => {
            let mut parsed = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    serde_json::Value::String(label) => parsed.push(label),
                    // A non-string array member means this is not a runs-on
                    // label set at all; do not guess at its meaning.
                    _ => {
                        return LaneDeclaration::Unparsable {
                            raw: raw.to_owned(),
                        };
                    }
                }
            }
            parsed
        }
        Ok(serde_json::Value::String(label)) => vec![label],
        // Not JSON (or JSON of an unexpected type): treat the literal text as a
        // single value. `local-only` and a bare `macos-15` both land here.
        _ => vec![trimmed.to_owned()],
    };

    if labels.is_empty() {
        return LaneDeclaration::Unparsable {
            raw: raw.to_owned(),
        };
    }

    if labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case("self-hosted"))
    {
        return LaneDeclaration::SelfHosted { labels };
    }

    // A single token that names no runner is a routing sentinel, not a lane.
    if labels.len() == 1 && is_routing_sentinel(&labels[0]) {
        return LaneDeclaration::Sentinel {
            value: labels.swap_remove(0),
        };
    }

    LaneDeclaration::Hosted { labels }
}

/// Routing sentinels that select a policy rather than name a runner label.
fn is_routing_sentinel(value: &str) -> bool {
    value.eq_ignore_ascii_case("local-only") || value.eq_ignore_ascii_case("none")
}

/// One queued job observed waiting on a label set.
///
/// Demand is what makes an empty census interpretable: without it, "nothing is
/// registered" cannot be told apart from "nothing was asked for".
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QueuedDemand {
    /// Labels the queued job requests.
    pub labels: Vec<String>,
    /// When the job entered the queue.
    pub queued_since: DateTime<Utc>,
}

impl QueuedDemand {
    /// Whether this queued job is asking for exactly the lane's label set.
    ///
    /// Matched as a case-insensitive superset in the job's direction: a job
    /// requesting every label the lane declares is demand for that lane.
    #[must_use]
    pub fn requests_all(&self, required: &[String]) -> bool {
        !required.is_empty()
            && required.iter().all(|want| {
                self.labels
                    .iter()
                    .any(|have| have.eq_ignore_ascii_case(want))
            })
    }
}

/// Tunables for [`assess_lane_service`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneServiceThresholds {
    /// How long queued demand must go unmet before an unserved or starved lane
    /// is reported as a fault rather than a transient.
    pub unserved_after_secs: i64,
}

impl Default for LaneServiceThresholds {
    fn default() -> Self {
        Self {
            unserved_after_secs: DEFAULT_UNSERVED_AFTER_SECS,
        }
    }
}

/// Typed outcome of a service assertion.
///
/// Ordered by severity so a host-level roll-up can take the worst verdict
/// without re-deriving precedence at each call site.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceVerdict {
    /// Demand can be satisfied, and this was proven rather than assumed.
    Served,
    /// Nothing is registered and nothing is asking. A just-in-time pool at
    /// rest is indistinguishable from a dead one by census alone, and this is
    /// the honest verdict for that state.
    Idle,
    /// Serving, but consuming a budget — latency against its ceiling, blind
    /// cycles against theirs, a restart counter climbing.
    Degraded,
    /// Demand exists and an online server for it exists, but the demand is not
    /// being reached. A scheduling or capacity fault, not a routing one.
    Starved,
    /// Declared local, served by nobody: aged demand exists and no online
    /// runner in **either** scope advertises the lane's labels. Unschedulable,
    /// and it will wait forever.
    Unserved,
    /// The instrument could not measure. Never folded into a pass.
    Unknown,
}

impl ServiceVerdict {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Served => "served",
            Self::Idle => "idle",
            Self::Degraded => "degraded",
            Self::Starved => "starved",
            Self::Unserved => "unserved",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this verdict should raise to a human.
    ///
    /// `Unknown` raises: an assertion that cannot see is not an assertion that
    /// passed.
    #[must_use]
    pub fn is_raise(self) -> bool {
        matches!(
            self,
            Self::Degraded | Self::Starved | Self::Unserved | Self::Unknown
        )
    }
}

/// Why an assertion could not measure.
///
/// [`ServiceVerdict::Unknown`] on its own repeats the mistake it exists to
/// catch: `SCAN BLIND … self-restarting for fresh gh auth` named authentication
/// for what was a timeout, and the supervisor then took a corrective action
/// that could not possibly help. Four very different facts otherwise collapse
/// into one opaque failure, and **an error that cannot distinguish its causes
/// sends the reader to the wrong subsystem**.
///
/// So an unknown verdict carries the boundary it hit, and — where one exists —
/// the permitted path that would have answered the same question.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Boundary {
    /// A command wrapper refused the verb itself. Not a permission fact: an
    /// equivalent permitted path commonly exists, and the refusal should name
    /// it rather than leave the caller to conclude it cannot act.
    Grammar,
    /// The query was well-formed and permitted, but issued in a scope that
    /// cannot see the answer — the repo runner census against an
    /// org-registered runner being the case this module was written for.
    Scope,
    /// The credential is the wrong principal. The operation is possible; this
    /// identity is not the one that can do it.
    Identity,
    /// This identity genuinely lacks the permission. The only one of the four
    /// where "you cannot do this" is the correct reading.
    Permission,
    /// A value could not be understood, so no assertion was made about it.
    Parse,
    /// The call did not complete — timeout, transport, rate limit. Notably not
    /// an authentication fact, however much it may resemble one.
    Transport,
}

impl Boundary {
    /// Snake-case string form used in JSON and human output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Grammar => "grammar",
            Self::Scope => "scope",
            Self::Identity => "identity",
            Self::Permission => "permission",
            Self::Parse => "parse",
            Self::Transport => "transport",
        }
    }

    /// Whether an equivalent permitted path is expected to exist.
    ///
    /// True for every boundary except [`Boundary::Permission`]: a grammar
    /// refusal, a wrong scope, a wrong identity and a failed call are all
    /// recoverable by asking differently, and reporting them as "cannot" is
    /// what makes a session stop at a wall that has a door in it.
    #[must_use]
    pub fn equivalent_path_may_exist(self) -> bool {
        !matches!(self, Self::Permission)
    }

    /// What the reader should try next, phrased as an action rather than a
    /// diagnosis.
    #[must_use]
    pub fn next_action(self) -> &'static str {
        match self {
            Self::Grammar => {
                "the verb was refused by a command grammar, not by GitHub — retry via a permitted \
                 equivalent (a raw API call usually is one) before concluding this is not allowed"
            }
            Self::Scope => {
                "re-issue the same query in the other scope; a repo-scope census cannot see an \
                 org-registered runner, and returns the same empty result either way"
            }
            Self::Identity => {
                "retry as the other configured identity; the operation is available, this \
                 principal is not the one that can perform it"
            }
            Self::Permission => {
                "grant the missing permission — unlike the other boundaries, asking differently \
                 will not help"
            }
            Self::Parse => "fix or remove the malformed value; nothing was asserted about it",
            Self::Transport => {
                "the call did not complete (timeout, transport, or rate limit). This is not an \
                 authentication fault; do not re-authenticate in response to it"
            }
        }
    }
}

/// A runner that satisfied a lane's label set.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LaneMatch {
    /// Runner name.
    pub name: String,
    /// Scope the runner registered into.
    pub scope: RunnerScope,
    /// Whether it was online at census time.
    pub online: bool,
    /// Whether it was busy at census time.
    pub busy: bool,
}

/// Verdict for one declared lane.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LaneReport {
    /// Name of the routing variable that declared the lane.
    pub variable: String,
    /// How the variable's value parsed.
    pub declaration: LaneDeclaration,
    /// The verdict.
    pub verdict: ServiceVerdict,
    /// Runners that advertise every label the lane requests, in either scope.
    pub matches: Vec<LaneMatch>,
    /// Age in seconds of the oldest queued job requesting this lane, if any.
    pub oldest_demand_secs: Option<i64>,
    /// How many queued jobs request this lane.
    pub demand_count: usize,
    /// Which boundary prevented a measurement. Always `Some` when the verdict
    /// is [`ServiceVerdict::Unknown`], and always `None` otherwise — an
    /// unknown that cannot say why is the failure this field exists to
    /// prevent.
    pub boundary: Option<Boundary>,
    /// Operator-facing explanation naming what was measured.
    pub detail: String,
}

impl LaneReport {
    /// Whether every runner satisfying this lane came from the org scope.
    ///
    /// True here means a repo-scope-only census would have called this lane
    /// unserved while it was online.
    #[must_use]
    pub fn served_only_by_org_scope(&self) -> bool {
        !self.matches.is_empty()
            && self
                .matches
                .iter()
                .all(|matched| matched.scope == RunnerScope::Org)
    }
}

/// Classify one declared lane against the runner census and the queued demand.
///
/// `census` must span **both** runner scopes; passing a repo-scope-only census
/// produces confidently wrong `Unserved` verdicts for org-registered lanes.
///
/// `census_boundary` is the instrument's own health: `None` when the census was
/// read successfully, or the [`Boundary`] that stopped it. When the census could
/// not be fetched the verdict is [`ServiceVerdict::Unknown`] regardless of what
/// the (empty) census would otherwise imply — an unreadable census and a
/// genuinely empty one produce the identical value, and only the caller knows
/// which it had. Requiring a named boundary rather than a bare `false` is what
/// keeps the resulting message from sending its reader to the wrong subsystem.
#[must_use]
pub fn assess_lane_service(
    variable: &str,
    raw_value: &str,
    census: &[RegisteredRunner],
    census_boundary: Option<Boundary>,
    demand: &[QueuedDemand],
    thresholds: LaneServiceThresholds,
    now: DateTime<Utc>,
) -> LaneReport {
    let declaration = parse_runs_on(raw_value);

    let mut report = LaneReport {
        variable: variable.to_owned(),
        declaration,
        verdict: ServiceVerdict::Unknown,
        matches: Vec::new(),
        oldest_demand_secs: None,
        demand_count: 0,
        boundary: None,
        detail: String::new(),
    };

    let required: Vec<String> = match &report.declaration {
        LaneDeclaration::SelfHosted { labels } => labels.clone(),
        LaneDeclaration::Hosted { labels } => {
            report.verdict = ServiceVerdict::Served;
            report.detail = format!(
                "hosted lane ({}) — served by GitHub, not by this fleet",
                labels.join(",")
            );
            return report;
        }
        LaneDeclaration::Sentinel { value } => {
            report.verdict = ServiceVerdict::Served;
            report.detail = format!("routing sentinel `{value}` — names no runner label");
            return report;
        }
        LaneDeclaration::Unparsable { raw } => {
            report.verdict = ServiceVerdict::Unknown;
            report.boundary = Some(Boundary::Parse);
            report.detail = format!(
                "could not parse routing value `{raw}` — {}",
                Boundary::Parse.next_action()
            );
            return report;
        }
    };

    if let Some(boundary) = census_boundary {
        report.verdict = ServiceVerdict::Unknown;
        report.boundary = Some(boundary);
        report.detail = format!(
            "runner census unreadable ({}) — an empty census and an unfetched one look \
             identical, so no service claim is made. Next: {}",
            boundary.as_str(),
            boundary.next_action()
        );
        return report;
    }

    report.matches = census
        .iter()
        .filter(|runner| runner.advertises_all(&required))
        .map(|runner| LaneMatch {
            name: runner.name.clone(),
            scope: runner.scope,
            online: runner.online,
            busy: runner.busy,
        })
        .collect();

    let matching_demand: Vec<&QueuedDemand> = demand
        .iter()
        .filter(|job| job.requests_all(&required))
        .collect();
    report.demand_count = matching_demand.len();
    report.oldest_demand_secs = matching_demand
        .iter()
        .map(|job| (now - job.queued_since).num_seconds())
        .max();

    classify_served_lane(&mut report, &required, thresholds);
    report
}

/// Decide the verdict for a self-hosted lane whose census and demand have
/// already been gathered onto `report`.
///
/// Split out from [`assess_lane_service`] so the gathering and the judgement
/// stay separately readable; the judgement is the part worth arguing about.
fn classify_served_lane(
    report: &mut LaneReport,
    required: &[String],
    thresholds: LaneServiceThresholds,
) {
    let online_matches = report.matches.iter().filter(|m| m.online).count();
    let aged_demand = report
        .oldest_demand_secs
        .is_some_and(|age| age >= thresholds.unserved_after_secs);

    let scopes = describe_scopes(&report.matches);

    if online_matches > 0 {
        if aged_demand {
            report.verdict = ServiceVerdict::Starved;
            report.detail = format!(
                "{online_matches} online runner(s) advertise these labels ({scopes}), \
                 but {} queued job(s) have waited up to {}s — scheduling or capacity, not routing",
                report.demand_count,
                report.oldest_demand_secs.unwrap_or_default()
            );
        } else {
            report.verdict = ServiceVerdict::Served;
            report.detail =
                format!("{online_matches} online runner(s) advertise these labels ({scopes})");
        }
        return;
    }

    // Nothing online can serve the lane. Demand is what decides whether that is
    // a fault or a pool legitimately at rest.
    if aged_demand {
        report.verdict = ServiceVerdict::Unserved;
        report.detail = if report.matches.is_empty() {
            format!(
                "declared local, served by nobody: no runner in the repo or org scope advertises \
                 [{}], and {} queued job(s) have waited up to {}s",
                required.join(","),
                report.demand_count,
                report.oldest_demand_secs.unwrap_or_default()
            )
        } else {
            format!(
                "declared local, served by nobody: {} runner(s) advertise these labels ({scopes}) \
                 but every one is offline, and {} queued job(s) have waited up to {}s",
                report.matches.len(),
                report.demand_count,
                report.oldest_demand_secs.unwrap_or_default()
            )
        };
        return;
    }

    report.verdict = ServiceVerdict::Idle;
    report.detail = if report.demand_count == 0 {
        format!(
            "no online runner advertises [{}] and nothing is queued on them — \
             indistinguishable from a just-in-time pool at rest",
            required.join(",")
        )
    } else {
        format!(
            "no online runner advertises [{}]; {} queued job(s) waiting up to {}s, \
             under the {}s threshold",
            required.join(","),
            report.demand_count,
            report.oldest_demand_secs.unwrap_or_default(),
            thresholds.unserved_after_secs
        )
    };
}

/// Render which scopes a set of matches came from, so org-scope-only service is
/// visible rather than inferred.
fn describe_scopes(matches: &[LaneMatch]) -> String {
    let repo = matches.iter().any(|m| m.scope == RunnerScope::Repo);
    let org = matches.iter().any(|m| m.scope == RunnerScope::Org);
    match (repo, org) {
        (true, true) => "repo+org scope".to_owned(),
        (true, false) => "repo scope".to_owned(),
        (false, true) => "org scope only".to_owned(),
        (false, false) => "no scope".to_owned(),
    }
}

/// Worst verdict across a set of lane reports, for a host- or fleet-level
/// roll-up.
///
/// An empty input is [`ServiceVerdict::Unknown`]: asserting nothing is not the
/// same as asserting everything passed.
#[must_use]
pub fn roll_up(reports: &[LaneReport]) -> ServiceVerdict {
    reports
        .iter()
        .map(|report| report.verdict)
        .max()
        .unwrap_or(ServiceVerdict::Unknown)
}

#[cfg(test)]
mod tests;
