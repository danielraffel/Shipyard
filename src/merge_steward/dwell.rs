//! Dwell-time limits: how long a pull request may sit in one blocked state
//! before the steward does something about it.
//!
//! The steward already names why every open pull request is not merging
//! (`blocked_on`, see `docs/automated-draining.md`). This module turns that
//! token into a [`DwellState`], remembers across passes when the pull request
//! entered it ([`DwellRepoState`], persisted in the steward ledger), and,
//! once the state outlives its configured limit, decides the one thing to do
//! ([`plan`]):
//!
//! * a **mechanical act** the decisions contract allows, taken once per pull
//!   request, state and head ([`ActKind`]): re-run a stale `review-verdict`
//!   check, dispatch a required check that never ran, move an approved green
//!   head to the front of the steward's own admission order, retarget a
//!   stacked pull request whose base merged;
//! * otherwise an **escalation**: one overdue record per level (the limit,
//!   then twice it, four times, ...), delivered to the owning session through
//!   the relay when an owner record exists, else to the overdue feed.
//!
//! Hard rules, enforced here and pinned by tests:
//!
//! * no act on a DIRTY, held or carried-unapproved head: those states have no
//!   [`ActKind`] at all, so a misconfiguration cannot add one;
//! * no act arms, enqueues, merges or merge-updates a pull request. Re-admit
//!   only reorders the steward's own admission pass, which keeps every gate it
//!   already has;
//! * every act is recorded before its side effect and never repeated for the
//!   same pull request, state and head.
//!
//! Pure: no reads, no clock. The steward adapter (`dwell_pass`) supplies the
//! rows, the time and the side effects.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// `[auto_merge.dwell]` in the base branch's `.shipyard/config.toml`.
pub const CONFIG_TABLE: &str = "dwell";
/// `[auto_merge.dwell.dispatch]`: required context to workflow file.
pub const DISPATCH_TABLE: &str = "dispatch";
/// Synthetic `blocked_on` of an open pull request stacked on a merged base.
pub const STACKED_BASE_MERGED: &str = "stacked_base_merged";

/// A blocked state with a dwell limit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DwellState {
    /// No counting verdict names the current head.
    NeedsReview,
    /// Required checks still run.
    RequiredPending,
    /// A required check failed on an infrastructure interruption the steward
    /// is re-running (`transient_rerun`).
    RequiredFailedInfra,
    /// A required check failed.
    RequiredFailed,
    /// Only the approval context (`review-verdict`) is red.
    StaleVerdictCheck,
    /// GitHub reports a merge conflict.
    Dirty,
    /// A counting reviewer holds the head.
    Held,
    /// A required check never started on the head.
    RequiredNeverRan,
    /// Green and approved, but not in the queue.
    ApprovedGreenNotQueued,
    /// Carries commits of another pull request nobody approved.
    CarriesUnapproved,
    /// Stacked on a pull request that merged; still based on its branch.
    StackedBaseMerged,
    /// Approved, with required checks that went red on a test merge commit
    /// that no longer exists ([`super::stale_merge_ref`]).
    StaleMergeRef,
}

impl DwellState {
    /// Every state, in report order.
    pub const ALL: [Self; 12] = [
        Self::NeedsReview,
        Self::RequiredPending,
        Self::RequiredFailedInfra,
        Self::RequiredFailed,
        Self::StaleVerdictCheck,
        Self::Dirty,
        Self::Held,
        Self::RequiredNeverRan,
        Self::ApprovedGreenNotQueued,
        Self::CarriesUnapproved,
        Self::StackedBaseMerged,
        Self::StaleMergeRef,
    ];

    /// The config key and report name.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::NeedsReview => "needs_review",
            Self::RequiredPending => "required_pending",
            Self::RequiredFailedInfra => "required_failed_infra",
            Self::RequiredFailed => "required_failed",
            Self::StaleVerdictCheck => "stale_verdict_check",
            Self::Dirty => "dirty",
            Self::Held => "held",
            Self::RequiredNeverRan => "required_never_ran",
            Self::ApprovedGreenNotQueued => "approved_green_not_queued",
            Self::CarriesUnapproved => "carries_unapproved",
            Self::StackedBaseMerged => "stacked_base_merged",
            Self::StaleMergeRef => "stale_merge_ref",
        }
    }

    /// The default limit, in minutes.
    #[must_use]
    pub const fn default_limit_minutes(self) -> u32 {
        match self {
            Self::NeedsReview => 20,
            Self::StackedBaseMerged
            | Self::RequiredFailedInfra
            | Self::StaleVerdictCheck
            | Self::StaleMergeRef => 10,
            Self::RequiredPending | Self::CarriesUnapproved => 60,
            Self::RequiredFailed => 45,
            Self::Dirty | Self::Held | Self::RequiredNeverRan => 30,
            Self::ApprovedGreenNotQueued => 5,
        }
    }

    /// The mechanical act this state allows, if any. DIRTY, held and
    /// carried-unapproved heads have none, by construction: the contract
    /// forbids touching a DIRTY pull request unattended, a HOLD needs a new
    /// head, and an unapproved carried commit needs a review.
    #[must_use]
    pub const fn mechanical_act(self) -> Option<ActKind> {
        match self {
            Self::StaleVerdictCheck => Some(ActKind::RerunVerdictCheck),
            Self::RequiredNeverRan => Some(ActKind::DispatchNeverRan),
            Self::ApprovedGreenNotQueued => Some(ActKind::Readmit),
            Self::StackedBaseMerged => Some(ActKind::RetargetStacked),
            Self::StaleMergeRef => Some(ActKind::RetriggerStaleRef),
            Self::NeedsReview
            | Self::RequiredPending
            | Self::RequiredFailedInfra
            | Self::RequiredFailed
            | Self::Dirty
            | Self::Held
            | Self::CarriesUnapproved => None,
        }
    }

    /// Who an escalation addresses when no owner record names a session.
    #[must_use]
    pub const fn escalation_role(self) -> &'static str {
        match self {
            Self::NeedsReview | Self::StaleVerdictCheck | Self::CarriesUnapproved => "reviewer",
            Self::RequiredPending
            | Self::RequiredFailedInfra
            | Self::RequiredNeverRan
            | Self::StaleMergeRef => "operator",
            Self::ApprovedGreenNotQueued => "steward",
            Self::RequiredFailed | Self::Dirty | Self::Held | Self::StackedBaseMerged => "author",
        }
    }

    /// The state a config key names.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.key() == key)
    }
}

/// A mechanical act a dwell limit may trigger.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActKind {
    /// Re-run the newest red `review-verdict` run after the approval gate,
    /// read live, admits the exact head with no HOLD.
    RerunVerdictCheck,
    /// Dispatch the workflow `[auto_merge.dwell.dispatch]` maps to each
    /// required context that never ran on the head.
    DispatchNeverRan,
    /// Evaluate the pull request first in the steward's next admission pass.
    Readmit,
    /// Retarget a pull request stacked on a merged base to the steward's base.
    RetargetStacked,
    /// Close and reopen an approved pull request whose merge ref is stale,
    /// through `shipyard advance --retrigger` and every refusal it applies.
    RetriggerStaleRef,
}

impl ActKind {
    /// Report name.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::RerunVerdictCheck => "rerun_verdict_check",
            Self::DispatchNeverRan => "dispatch_never_ran",
            Self::Readmit => "readmit",
            Self::RetargetStacked => "retarget_stacked",
            Self::RetriggerStaleRef => "retrigger_stale_ref",
        }
    }

    /// Whether the act writes to GitHub. Re-admit only reorders the
    /// steward's own pass, so it runs even in shadow mode.
    #[must_use]
    pub const fn writes_github(self) -> bool {
        !matches!(self, Self::Readmit)
    }
}

/// Whether mechanical acts that write to GitHub run.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActMode {
    /// Nothing is acted on; every overdue state escalates.
    Off,
    /// Acts are reported as `would_act` and escalate; re-admit still runs.
    #[default]
    Shadow,
    /// Acts run, once each.
    Apply,
}

/// `[auto_merge.dwell]`, as the base config sets it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DwellPolicy {
    /// Minutes per state; a state set to 0 is not tracked.
    pub limits: BTreeMap<DwellState, u32>,
    /// Whether GitHub-writing acts run.
    pub act: ActMode,
    /// Required context to workflow file, for [`ActKind::DispatchNeverRan`].
    pub dispatch: BTreeMap<String, String>,
    /// Most new escalations one pass emits per repository; the rest wait for
    /// the next pass, worst first.
    pub max_escalations_per_pass: usize,
    /// Most GitHub-writing acts one pass takes per repository; the rest wait
    /// for the next pass. Bounds an outage (every PR `required_never_ran` at
    /// once) to a trickle.
    pub max_acts_per_pass: usize,
    /// Highest escalation level: level `n` fires at `limit * 2^(n-1)`.
    pub max_level: u32,
    /// Why the config could not be read, when it could not; the defaults
    /// apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
}

impl Default for DwellPolicy {
    fn default() -> Self {
        Self {
            limits: DwellState::ALL
                .into_iter()
                .map(|state| (state, state.default_limit_minutes()))
                .collect(),
            act: ActMode::default(),
            dispatch: BTreeMap::new(),
            max_escalations_per_pass: 25,
            max_acts_per_pass: 3,
            max_level: 4,
            read_error: None,
        }
    }
}

impl DwellPolicy {
    /// The policy a parsed base config sets. Absent or mistyped keys keep
    /// their defaults; an unknown key is ignored.
    #[must_use]
    pub fn from_table(table: &toml::Table) -> Self {
        let mut policy = Self::default();
        let Some(section) = table
            .get(super::ejection_guards::CONFIG_TABLE)
            .and_then(toml::Value::as_table)
            .and_then(|auto| auto.get(CONFIG_TABLE))
            .and_then(toml::Value::as_table)
        else {
            return policy;
        };
        for state in DwellState::ALL {
            if let Some(minutes) = section
                .get(state.key())
                .and_then(toml::Value::as_integer)
                .and_then(|minutes| u32::try_from(minutes).ok())
            {
                policy.limits.insert(state, minutes);
            }
        }
        policy.act = match section.get("act").and_then(toml::Value::as_str) {
            Some("off") => ActMode::Off,
            Some("apply") => ActMode::Apply,
            _ => ActMode::Shadow,
        };
        if let Some(cap) = section
            .get("max_escalations_per_pass")
            .and_then(toml::Value::as_integer)
            .and_then(|cap| usize::try_from(cap).ok())
        {
            policy.max_escalations_per_pass = cap;
        }
        if let Some(cap) = section
            .get("max_acts_per_pass")
            .and_then(toml::Value::as_integer)
            .and_then(|cap| usize::try_from(cap).ok())
        {
            policy.max_acts_per_pass = cap;
        }
        if let Some(level) = section
            .get("max_level")
            .and_then(toml::Value::as_integer)
            .and_then(|level| u32::try_from(level).ok())
            .filter(|level| (1..=10).contains(level))
        {
            policy.max_level = level;
        }
        policy.dispatch = section
            .get(DISPATCH_TABLE)
            .and_then(toml::Value::as_table)
            .map(|map| {
                map.iter()
                    .filter_map(|(context, workflow)| {
                        let workflow = workflow.as_str()?.trim();
                        safe_workflow(workflow)
                            .then(|| (context.trim().to_owned(), workflow.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        policy
    }

    /// The limit of `state`, or `None` when it is not tracked.
    #[must_use]
    pub fn limit(&self, state: DwellState) -> Option<u32> {
        self.limits
            .get(&state)
            .copied()
            .filter(|minutes| *minutes > 0)
    }
}

/// A workflow file name the dispatch map may name: a bare `.yml`/`.yaml`
/// file, no path, no flag.
fn safe_workflow(workflow: &str) -> bool {
    !workflow.is_empty()
        && !workflow.starts_with('-')
        && std::path::Path::new(workflow)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml"))
        && workflow
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The dwell state a `blocked_on` token names, if any. `approval_context`
/// is the policy's approval context (`review-verdict`): a red required check
/// that is only it is [`DwellState::StaleVerdictCheck`].
#[must_use]
pub fn state_for_token(token: &str, approval_context: &str) -> Option<DwellState> {
    let (kind, arg) = token.split_once(':').unwrap_or((token, ""));
    match kind {
        "no_verdict" => Some(DwellState::NeedsReview),
        "approval_excluded" if matches!(arg, "stale_head" | "predates_head") => {
            Some(DwellState::NeedsReview)
        }
        "required_pending" | "capacity_queued" => Some(DwellState::RequiredPending),
        "transient_rerun" => Some(DwellState::RequiredFailedInfra),
        "required_failed" => {
            let approval = approval_context.trim();
            let only_approval = !approval.is_empty()
                && !arg.is_empty()
                && arg
                    .split(',')
                    .all(|context| context.trim().eq_ignore_ascii_case(approval));
            Some(if only_approval {
                DwellState::StaleVerdictCheck
            } else {
                DwellState::RequiredFailed
            })
        }
        "merge_state" if arg.eq_ignore_ascii_case("DIRTY") => Some(DwellState::Dirty),
        "held" => Some(DwellState::Held),
        "required_never_ran" => Some(DwellState::RequiredNeverRan),
        "ready" | "admission_pending" => Some(DwellState::ApprovedGreenNotQueued),
        "carries_unapproved_commits" => Some(DwellState::CarriesUnapproved),
        STACKED_BASE_MERGED => Some(DwellState::StackedBaseMerged),
        super::stale_merge_ref::TOKEN => Some(DwellState::StaleMergeRef),
        _ => None,
    }
}

/// One pull request's current dwell, persisted across passes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DwellEntry {
    /// The state.
    pub state: DwellState,
    /// The `blocked_on` token behind it, as last seen.
    pub token: String,
    /// The head it was observed at.
    pub head: String,
    /// When the pull request entered the state.
    pub entered_at: DateTime<Utc>,
    /// The last pass that saw it in the state.
    pub last_seen_at: DateTime<Utc>,
    /// Highest escalation level already emitted for this entry (or seeded).
    #[serde(default)]
    pub escalated_level: u32,
    /// The level the first pass seeded without emitting it: never relayed.
    #[serde(default)]
    pub seeded_level: u32,
}

/// One act, as recorded before (`intent`) and after its side effect.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActRecord {
    /// The act.
    pub kind: ActKind,
    /// The state and head it was taken for.
    pub state: DwellState,
    /// The head.
    pub head: String,
    /// `intent` (recorded before the side effect), `done`, `failed` (the
    /// side effect was attempted and GitHub refused it), `refused` (a live
    /// revalidation said no), `deferred` (a precondition could not be
    /// read; nothing was attempted) or `blocked` (a guard refused the write,
    /// or the command it needs is not in this build; nothing changed).
    pub status: String,
    /// When it was recorded.
    pub at: DateTime<Utc>,
    /// What happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Attempts at this act for this state and head, the current one
    /// included.
    #[serde(default = "one")]
    pub attempts: u32,
}

const fn one() -> u32 {
    1
}

/// Acts kept per pull request; older ones are dropped.
const ACTS_KEPT: usize = 64;
/// An act record is kept this long, whether or not its pull request is seen:
/// a pull request missing from one pass, or closed and reopened, never gets
/// an act repeated at a head it already acted on.
const ACT_TTL_DAYS: i64 = 14;
/// Most attempts at one act for one state and head.
pub const MAX_ATTEMPTS: u32 = 3;
/// Wait before the first retry; doubles per attempt.
const RETRY_BASE_MINUTES: i64 = 15;
/// Wait before retrying a `blocked` act (a guard refused it, or the command
/// it needs is not in this build): a day, doubling. Never a loop.
const BLOCKED_RETRY_BASE_MINUTES: i64 = 24 * 60;

/// One repository's dwell state, in the steward ledger.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DwellRepoState {
    /// The current dwell of every tracked pull request.
    #[serde(default)]
    pub entries: BTreeMap<u64, DwellEntry>,
    /// Acts taken, per pull request, newest last. An act is never repeated
    /// for a `(state, head)` it already names, even if the state comes back.
    #[serde(default)]
    pub acts: BTreeMap<u64, Vec<ActRecord>>,
    /// The last pass's ranking, closest to mergeable first: the next pass
    /// evaluates admission candidates in this order.
    #[serde(default)]
    pub admission_rank: Vec<u64>,
    /// Approved green heads that overstayed: evaluated before anything else
    /// in the next pass.
    #[serde(default)]
    pub readmit_first: Vec<u64>,
    /// The last pass that saw each pull request, in any state: a state
    /// entered after an untracked one (queued, draft) is never dated before
    /// it.
    #[serde(default)]
    pub seen: BTreeMap<u64, DateTime<Utc>>,
    /// Set by the first pass that tracked this repository and base: the
    /// backlog it found was seeded, not escalated.
    #[serde(default)]
    pub initialized: bool,
    /// The last merge-ref verdict per pull request
    /// ([`super::stale_merge_ref`]), so one red is not read every pass.
    #[serde(default)]
    pub stale_ref: BTreeMap<u64, super::stale_merge_ref::StaleMemo>,
    /// Every retrigger attempted, per pull request and head. Unlike act
    /// records these outlive [`ACT_TTL_DAYS`]: a reopen keeps the head, so a
    /// head whose ledger record expired would otherwise be retriggered again
    /// every two weeks.
    #[serde(default)]
    pub retriggers: Vec<RetriggerMark>,
}

/// Retrigger marks kept per repository; the oldest are dropped past this.
const RETRIGGER_MARKS_KEPT: usize = 2048;

/// The retrigger history of one pull request head.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RetriggerMark {
    /// Pull request number.
    pub pr: u64,
    /// The head.
    pub head: String,
    /// Retriggers attempted at this head.
    pub attempts: u32,
    /// One succeeded: never again at this head.
    pub done: bool,
    /// The last attempt.
    pub at: DateTime<Utc>,
}

impl DwellRepoState {
    /// Why `head` of pull request `number` may not be retriggered again, if
    /// it may not: one retrigger succeeded there, or [`MAX_ATTEMPTS`] were
    /// spent. Whatever the act-record TTL.
    #[must_use]
    pub fn retrigger_spent(&self, number: u64, head: &str) -> Option<String> {
        let mark = self
            .retriggers
            .iter()
            .find(|mark| mark.pr == number && mark.head.eq_ignore_ascii_case(head))?;
        if mark.done {
            Some(
                "retriggered once at this head already; only a new head retriggers again"
                    .to_owned(),
            )
        } else if mark.attempts >= MAX_ATTEMPTS {
            Some(format!(
                "{} retrigger attempts spent at this head",
                mark.attempts
            ))
        } else {
            None
        }
    }

    /// Count one retrigger attempt at `head`, and whether it succeeded.
    pub fn mark_retrigger(
        &mut self,
        number: u64,
        head: &str,
        attempt: bool,
        done: bool,
        at: DateTime<Utc>,
    ) {
        let index = self
            .retriggers
            .iter()
            .position(|mark| mark.pr == number && mark.head.eq_ignore_ascii_case(head));
        let mut mark = index.map_or_else(
            || RetriggerMark {
                pr: number,
                head: head.to_owned(),
                attempts: 0,
                done: false,
                at,
            },
            |index| self.retriggers.remove(index),
        );
        if attempt {
            mark.attempts += 1;
        }
        mark.done |= done;
        mark.at = at;
        self.retriggers.push(mark);
        if self.retriggers.len() > RETRIGGER_MARKS_KEPT {
            self.retriggers
                .drain(..self.retriggers.len() - RETRIGGER_MARKS_KEPT);
        }
    }

    /// Whether an act of `kind` was already taken for `state` at `head`.
    #[must_use]
    pub fn acted(&self, number: u64, state: DwellState, head: &str) -> Option<&ActRecord> {
        self.acts
            .get(&number)?
            .iter()
            .rev()
            .find(|record| record.state == state && record.head.eq_ignore_ascii_case(head))
    }

    /// Record an act (intent or outcome), replacing the same act's earlier
    /// record for the same head. An intent after a retryable outcome counts
    /// one more attempt; an outcome keeps its intent's count.
    pub fn record_act(&mut self, number: u64, mut record: ActRecord) {
        let acts = self.acts.entry(number).or_default();
        let same = |existing: &ActRecord| {
            existing.state == record.state
                && existing.kind == record.kind
                && existing.head.eq_ignore_ascii_case(&record.head)
        };
        if let Some(previous) = acts.iter().rev().find(|existing| same(existing)) {
            record.attempts = if record.status == "intent" && previous.status != "intent" {
                previous.attempts + 1
            } else {
                previous.attempts
            };
        }
        acts.retain(|existing| !same(existing));
        acts.push(record);
        if acts.len() > ACTS_KEPT {
            acts.drain(..acts.len() - ACTS_KEPT);
        }
    }
}

/// Whether a recorded act may be tried again now: only after `refused` or
/// `deferred` (nothing was attempted), or `blocked` (a guard refused the
/// write and nothing changed) after a day; never after `done`, `failed` or a
/// bare `intent` (each may have reached GitHub), and never past
/// [`MAX_ATTEMPTS`]; after a backoff that doubles per attempt.
#[must_use]
pub fn retry_due(record: &ActRecord, now: DateTime<Utc>) -> bool {
    // `failed` means the side effect was attempted: GitHub may have taken
    // part of it (a dispatch of several workflows, or a call that timed out
    // after GitHub accepted it), so it is never tried again at this head.
    // Only `refused` and `deferred`, which attempted nothing, retry.
    let base = match record.status.as_str() {
        "refused" | "deferred" => RETRY_BASE_MINUTES,
        "blocked" => BLOCKED_RETRY_BASE_MINUTES,
        _ => return false,
    };
    if record.attempts >= MAX_ATTEMPTS {
        return false;
    }
    let shift = record.attempts.saturating_sub(1).min(8);
    now - record.at >= Duration::minutes(base << shift)
}

/// What one pass saw of one pull request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sighting {
    /// Pull request number.
    pub number: u64,
    /// Current head.
    pub head: String,
    /// Its `blocked_on` token.
    pub token: String,
    /// When GitHub dates the state, if it does.
    pub since: Option<DateTime<Utc>>,
}

/// Fold one pass's sightings into `state`.
///
/// A pull request in an untracked state loses its entry; one that changed
/// state or head starts a new entry, dated by GitHub's `since` when that is
/// no earlier than the last pass that saw the pull request in any state
/// (else by that pass) and never later than `now`. `observed` is every pull
/// request this pass could see; entries for any other are removed. Act
/// records are kept for [`ACT_TTL_DAYS`] whatever is observed, so an act is
/// never repeated at a head because its pull request dropped out of one
/// pass. The first pass that tracks a repository seeds its entries as
/// already escalated at their current level, so turning the feature on does
/// not flood the feed with the existing backlog. An entry
/// survives unchanged when the pass saw its pull request but did not
/// classify it (`skipped`, a row the pass budget deferred), and a stacked
/// entry survives a pass that could not see stacked pull requests
/// (`stacked_seen` false).
#[allow(clippy::too_many_arguments)]
pub fn track(
    state: &mut DwellRepoState,
    sightings: &[Sighting],
    observed: &BTreeSet<u64>,
    skipped: &BTreeSet<u64>,
    stacked_seen: bool,
    policy: &DwellPolicy,
    approval_context: &str,
    now: DateTime<Utc>,
) {
    let first_pass = !state.initialized;
    state.initialized = true;
    let mut current = BTreeMap::new();
    for sighting in sightings {
        let Some(dwell) = state_for_token(&sighting.token, approval_context)
            .filter(|dwell| policy.limit(*dwell).is_some())
        else {
            continue;
        };
        let previous = state.entries.get(&sighting.number);
        let entry = match previous {
            Some(entry)
                if entry.state == dwell && entry.head.eq_ignore_ascii_case(&sighting.head) =>
            {
                DwellEntry {
                    token: sighting.token.clone(),
                    last_seen_at: now,
                    ..entry.clone()
                }
            }
            _ => {
                let floor = previous
                    .map(|entry| entry.last_seen_at)
                    .into_iter()
                    .chain(state.seen.get(&sighting.number).copied())
                    .max();
                let entered = sighting
                    .since
                    .map_or(now, |since| floor.map_or(since, |floor| since.max(floor)))
                    .min(now);
                let escalated_level = if first_pass {
                    policy.limit(dwell).map_or(0, |limit| {
                        level((now - entered).num_minutes(), limit, policy.max_level)
                    })
                } else {
                    0
                };
                DwellEntry {
                    state: dwell,
                    token: sighting.token.clone(),
                    head: sighting.head.clone(),
                    entered_at: entered,
                    last_seen_at: now,
                    escalated_level,
                    seeded_level: escalated_level,
                }
            }
        };
        current.insert(sighting.number, entry);
    }
    let seen: BTreeSet<u64> = sightings.iter().map(|sighting| sighting.number).collect();
    let kept: Vec<(u64, DwellEntry)> = state
        .entries
        .iter()
        .filter(|(number, entry)| {
            !seen.contains(number)
                && (skipped.contains(number)
                    || (!observed.contains(number)
                        && entry.state == DwellState::StackedBaseMerged
                        && !stacked_seen))
        })
        .map(|(number, entry)| (*number, entry.clone()))
        .collect();
    current.extend(kept);
    state.entries = current;
    let live: BTreeSet<u64> = observed
        .iter()
        .copied()
        .chain(state.entries.keys().copied())
        .collect();
    let expiry = now - Duration::days(ACT_TTL_DAYS);
    for records in state.acts.values_mut() {
        records.retain(|record| record.at >= expiry);
    }
    state.acts.retain(|_, records| !records.is_empty());
    state.readmit_first.retain(|number| live.contains(number));
    state.seen.retain(|number, _| live.contains(number));
    for number in seen.iter().chain(skipped) {
        state.seen.insert(*number, now);
    }
}

/// One pull request past its limit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Overdue {
    /// Pull request number.
    pub number: u64,
    /// The state.
    pub state: DwellState,
    /// The token behind it.
    pub token: String,
    /// The head.
    pub head: String,
    /// When it entered the state.
    pub entered_at: DateTime<Utc>,
    /// Minutes in the state.
    pub age_minutes: i64,
    /// The limit.
    pub limit_minutes: u32,
    /// Minutes past the limit.
    pub over_minutes: i64,
    /// Escalation level this age reaches.
    pub level: u32,
}

/// The escalation level `age` reaches against `limit`: 1 at the limit, 2 at
/// twice it, 3 at four times it, capped at `max_level`; 0 below the limit.
#[must_use]
pub fn level(age_minutes: i64, limit_minutes: u32, max_level: u32) -> u32 {
    let limit = i64::from(limit_minutes.max(1));
    if age_minutes < limit {
        return 0;
    }
    let mut level = 1;
    let mut threshold = limit;
    while level < max_level {
        threshold = threshold.saturating_mul(2);
        if age_minutes < threshold {
            break;
        }
        level += 1;
    }
    level
}

/// Every tracked entry past its limit, worst (most over, relative to its
/// limit) first.
#[must_use]
pub fn overdue(state: &DwellRepoState, policy: &DwellPolicy, now: DateTime<Utc>) -> Vec<Overdue> {
    let mut rows: Vec<Overdue> = state
        .entries
        .iter()
        .filter_map(|(number, entry)| {
            let limit = policy.limit(entry.state)?;
            let age = (now - entry.entered_at).num_minutes().max(0);
            let level = level(age, limit, policy.max_level);
            (level > 0).then(|| Overdue {
                number: *number,
                state: entry.state,
                token: entry.token.clone(),
                head: entry.head.clone(),
                entered_at: entry.entered_at,
                age_minutes: age,
                limit_minutes: limit,
                over_minutes: age - i64::from(limit),
                level,
            })
        })
        .collect();
    rows.sort_by(|left, right| {
        let ratio = |row: &Overdue| row.age_minutes * 1000 / i64::from(row.limit_minutes.max(1));
        ratio(right)
            .cmp(&ratio(left))
            .then(left.number.cmp(&right.number))
    });
    rows
}

/// What a pass does about one overdue pull request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
pub enum Disposition {
    /// Take the act now (record the intent first).
    Act {
        /// The act.
        kind: ActKind,
    },
    /// The act is allowed but not enabled (`act = "shadow"` or `"off"`).
    WouldAct {
        /// The act.
        kind: ActKind,
    },
    /// The act applies but cannot be taken this pass: a precondition is not
    /// observable (a row the pass budget deferred, a stacked pull request the
    /// pass could not see), the per-pass act cap or the pass budget is spent.
    /// Escalation still waits until level 2, as for an act taken now.
    ActLater {
        /// The act.
        kind: ActKind,
        /// Why not now.
        why: String,
    },
    /// The act was already taken for this state and head.
    Acted {
        /// The act.
        kind: ActKind,
        /// Its recorded outcome.
        status: String,
    },
    /// No mechanical act applies.
    NoAct,
}

/// One overdue pull request's plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Planned {
    /// The overdue row.
    #[serde(flatten)]
    pub overdue: Overdue,
    /// The act, if any.
    pub act: Disposition,
    /// Whether this pass emits a new escalation for it.
    pub escalate: bool,
    /// An escalation this pass owes but the per-pass cap deferred.
    pub escalation_deferred: bool,
}

/// Whether a row's act can be taken this pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Actable {
    /// Its preconditions are observable now.
    Yes,
    /// It never applies to this row (a never-ran context with no mapped
    /// workflow): escalate instead.
    Never,
    /// Not this pass, for this reason; try again on a later pass.
    NotNow(String),
}

/// Plan one pass: an act where one applies and was not taken (or is due a
/// bounded retry, [`retry_due`]), at most `max_acts_per_pass`, and an
/// escalation for each new level, at most `max_escalations_per_pass`, worst
/// first.
///
/// A pull request whose act runs, or waits for a later pass, is not
/// escalated at level 1: the act gets its chance first. Every other overdue
/// level escalates, including one whose act was already taken (it did not
/// clear the state). `actable` says whether a row's act can be taken now.
#[must_use]
pub fn plan(
    state: &DwellRepoState,
    rows: Vec<Overdue>,
    policy: &DwellPolicy,
    now: DateTime<Utc>,
    actable: &dyn Fn(&Overdue, ActKind) -> Actable,
) -> Vec<Planned> {
    let mut budget = policy.max_escalations_per_pass;
    let mut acts_left = policy.max_acts_per_pass;
    rows.into_iter()
        .map(|row| {
            let act = match row.state.mechanical_act() {
                None => Disposition::NoAct,
                Some(kind) => match state.acted(row.number, row.state, &row.head) {
                    Some(record) if !retry_due(record, now) => Disposition::Acted {
                        kind: record.kind,
                        status: record.status.clone(),
                    },
                    _ => match (policy.act, actable(&row, kind)) {
                        (ActMode::Off, _) | (_, Actable::Never) => Disposition::NoAct,
                        (ActMode::Shadow, _) if kind.writes_github() => {
                            Disposition::WouldAct { kind }
                        }
                        (_, Actable::NotNow(why)) => Disposition::ActLater { kind, why },
                        (ActMode::Apply | ActMode::Shadow, Actable::Yes)
                            if kind.writes_github() && acts_left == 0 =>
                        {
                            Disposition::ActLater {
                                kind,
                                why: "per-pass act cap reached".to_owned(),
                            }
                        }
                        (ActMode::Apply | ActMode::Shadow, Actable::Yes) => {
                            if kind.writes_github() {
                                acts_left -= 1;
                            }
                            Disposition::Act { kind }
                        }
                    },
                },
            };
            let already = state
                .entries
                .get(&row.number)
                .map_or(0, |entry| entry.escalated_level);
            let acting_first =
                matches!(act, Disposition::Act { .. } | Disposition::ActLater { .. })
                    && row.level <= 1;
            let owed = row.level > already && !acting_first;
            let escalate = owed && budget > 0;
            if escalate {
                budget -= 1;
            }
            Planned {
                overdue: row,
                act,
                escalate,
                escalation_deferred: owed && !escalate,
            }
        })
        .collect()
}

/// Remember the escalations a pass emitted, so each level fires once.
pub fn mark_escalated(state: &mut DwellRepoState, planned: &[Planned]) {
    for row in planned.iter().filter(|row| row.escalate) {
        if let Some(entry) = state.entries.get_mut(&row.overdue.number) {
            entry.escalated_level = entry.escalated_level.max(row.overdue.level);
        }
    }
}

/// The time `minutes` ago, for tests and callers dating a sighting.
#[must_use]
pub fn minutes_before(now: DateTime<Utc>, minutes: i64) -> DateTime<Utc> {
    now - Duration::minutes(minutes)
}

#[cfg(test)]
mod tests;
