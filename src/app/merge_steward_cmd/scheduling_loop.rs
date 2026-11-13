//! The steward's scheduling loop: one state, one act, one deadline for every
//! open pull request, and who owns it when the deadline passes.
//!
//! Each tick puts a `loop` object on every open pull-request row the loop
//! covers (see `docs/scheduling-loop.md` for the schema the review, runner and
//! ledger lanes build against). It is derived only from what the pass already
//! read: the row's `blocked_on` token (required contexts only, one token per
//! row with the steward's own precedence), its decision (merge-queue
//! membership), the dwell entry (when the state began at this head), and the
//! pass-runner's review quarantine file on this host. It adds no GitHub read.
//!
//! [`classify`] is the whole contract: [`state_for`] maps the row to exactly
//! one [`LoopState`], and [`TABLE`] gives each state its act, first actor and
//! deadlines. A missed deadline raises the level by one (L0 the state's own
//! actor, L1 repair, L2 a stronger model on the exact head, L3 the
//! coordinator), never to the owner of the fleet.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::merge_steward::StewardDecision;

/// Environment variable naming the pass-runner's review quarantine file.
pub(crate) const QUARANTINE_ENV: &str = "SHIPYARD_REVIEW_QUARANTINE";
/// Machine-global config key naming it (`[loop] review_quarantine_file`).
pub(crate) const QUARANTINE_KEY: &str = "loop.review_quarantine_file";
/// Highest level.
pub(crate) const MAX_LEVEL: u8 = 3;

/// The loop states. Exactly one per covered pull request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LoopState {
    /// In the merge queue: the queue acts.
    Queued,
    /// Approved and green, not queued: enqueue the exact head.
    Ready,
    /// Green, waiting on a counting review of the exact head.
    AwaitingReview,
    /// Required checks are running.
    CheckRunning,
    /// Required checks wait for a runner; nothing is wrong with the PR.
    CapacityQueued,
    /// A required context failed at the head.
    RequiredRed,
    /// A counting HOLD names the head.
    Hold,
    /// GitHub reports a conflict with the base.
    Dirty,
    /// The pass-runner quarantined the head's review (it gave up).
    Quarantined,
    /// The queue ejected this head and the steward will not re-admit it.
    Ejected,
    /// Its merge group can never finish (a required context will never post).
    StuckGroup,
    /// The tooling around the PR failed (an interrupted or never-started
    /// check, an unreadable gate, a steward error): not the PR's result.
    Tooling,
}

/// Who moves the pull request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LoopActor {
    /// The steward's own deterministic act.
    Steward,
    /// The merge queue or CI: nothing for anyone to do.
    Queue,
    /// The standing reviewer.
    Reviewer,
    /// The adversarial reviewer.
    Adversary,
    /// The repair lane.
    Repair,
    /// A stronger model (or the other provider) on the exact head.
    StrongerModel,
    /// The coordinator inbox (`FLEET_PASS_ESCALATION_TARGET`).
    Coordinator,
}

/// One state's act, first actor and deadlines.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Row {
    pub(crate) state: LoopState,
    /// The act, a short verb phrase.
    pub(crate) action: &'static str,
    /// The level the state starts at (0, or 2 for a quarantined review,
    /// which goes to a stronger model at once).
    pub(crate) start: u8,
    /// The actor at the start level.
    pub(crate) first: LoopActor,
    /// L1's actor (normally repair; the adversary after a reviewer).
    pub(crate) second: LoopActor,
    /// Minutes from `since` to the first deadline; `None` when the state has
    /// no deadline (the queue or CI is acting).
    pub(crate) first_minutes: Option<i64>,
    /// Minutes from the first deadline to the next.
    pub(crate) second_minutes: i64,
    /// Minutes from the second deadline to the third.
    pub(crate) third_minutes: i64,
}

/// The table: every state's act and clock. One definition, documented in
/// `docs/scheduling-loop.md`.
pub(crate) const TABLE: [Row; 12] = [
    Row {
        start: 0,
        state: LoopState::Queued,
        action: "wait for the merge queue",
        first: LoopActor::Queue,
        second: LoopActor::Repair,
        first_minutes: None,
        second_minutes: 0,
        third_minutes: 0,
    },
    Row {
        start: 0,
        state: LoopState::Ready,
        action: "enqueue the exact head",
        first: LoopActor::Steward,
        second: LoopActor::Repair,
        first_minutes: Some(2),
        second_minutes: 30,
        third_minutes: 30,
    },
    Row {
        start: 0,
        state: LoopState::AwaitingReview,
        action: "review the exact head",
        first: LoopActor::Reviewer,
        second: LoopActor::Adversary,
        first_minutes: Some(30),
        second_minutes: 30,
        third_minutes: 30,
    },
    Row {
        start: 0,
        state: LoopState::CheckRunning,
        action: "wait for required checks",
        first: LoopActor::Queue,
        second: LoopActor::Repair,
        first_minutes: Some(60),
        second_minutes: 60,
        third_minutes: 60,
    },
    Row {
        start: 0,
        state: LoopState::CapacityQueued,
        action: "wait for a runner; the host is watched",
        first: LoopActor::Queue,
        second: LoopActor::Repair,
        first_minutes: None,
        second_minutes: 0,
        third_minutes: 0,
    },
    Row {
        start: 0,
        state: LoopState::RequiredRed,
        action: "claim and fix the red required check",
        first: LoopActor::Repair,
        second: LoopActor::Repair,
        first_minutes: Some(10),
        second_minutes: 50,
        third_minutes: 60,
    },
    Row {
        start: 0,
        state: LoopState::Hold,
        action: "claim and address the HOLD",
        first: LoopActor::Repair,
        second: LoopActor::Repair,
        first_minutes: Some(10),
        second_minutes: 50,
        third_minutes: 60,
    },
    Row {
        start: 0,
        state: LoopState::Dirty,
        action: "merge origin/main and regenerate",
        first: LoopActor::Repair,
        second: LoopActor::Repair,
        first_minutes: Some(5),
        second_minutes: 30,
        third_minutes: 30,
    },
    Row {
        start: 2,
        state: LoopState::Quarantined,
        action: "review the exact head with a stronger model",
        first: LoopActor::StrongerModel,
        second: LoopActor::StrongerModel,
        first_minutes: Some(30),
        second_minutes: 0,
        third_minutes: 0,
    },
    Row {
        start: 0,
        state: LoopState::Ejected,
        action: "re-queue the same head or push a new one",
        first: LoopActor::Steward,
        second: LoopActor::Repair,
        first_minutes: Some(10),
        second_minutes: 30,
        third_minutes: 30,
    },
    Row {
        start: 0,
        state: LoopState::StuckGroup,
        action: "dequeue as defect-fix",
        first: LoopActor::Steward,
        second: LoopActor::Repair,
        first_minutes: Some(30),
        second_minutes: 30,
        third_minutes: 30,
    },
    Row {
        start: 0,
        state: LoopState::Tooling,
        action: "re-run on another slot",
        first: LoopActor::Steward,
        second: LoopActor::Repair,
        first_minutes: Some(10),
        second_minutes: 30,
        third_minutes: 30,
    },
];

/// The table row of `state`.
#[must_use]
pub(crate) fn row(state: LoopState) -> &'static Row {
    TABLE
        .iter()
        .find(|row| row.state == state)
        .unwrap_or(&TABLE[0])
}

/// The `loop` object on one pull-request row.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Loop {
    pub(crate) state: LoopState,
    /// When the pull request entered this state at this head (RFC 3339).
    pub(crate) since: String,
    /// The act, a short verb phrase.
    pub(crate) action: String,
    /// Who acts at the current level.
    pub(crate) actor: LoopActor,
    /// When the current level's deadline passes; `null` when the state has
    /// no deadline (the queue or CI is acting) or at the last level.
    pub(crate) deadline_at: Option<String>,
    /// 0 at entry; one more per missed deadline, at most 3.
    pub(crate) level: u8,
}

/// Map a row to its state, or `None` when the loop does not cover it:
/// drafts, pull requests the steward does not own, and the labels the
/// pipeline skips by design (`owner-decision`, `superseded`, `PARKED/*`,
/// the opt-out label, provenance labels).
///
/// Precedence: a merge group that can never finish, then the merge queue,
/// then the state the row's `blocked_on` token names (the steward already
/// chose one token per row), with a quarantined review of the current head
/// replacing `awaiting_review` only.
#[must_use]
pub(crate) fn state_for(
    token: &str,
    decision: &StewardDecision,
    quarantined: bool,
) -> Option<LoopState> {
    let kind = token.split_once(':').map_or(token, |(kind, _)| kind);
    if kind == "merge_group_stalled" {
        return Some(LoopState::StuckGroup);
    }
    if matches!(decision, StewardDecision::Queued { .. }) || matches!(kind, "queued" | "admitted") {
        return Some(LoopState::Queued);
    }
    if matches!(
        kind,
        "draft"
            | "unmanaged"
            | "handoff_missing"
            | "owner_decision"
            | "excluded_label"
            | "provenance_blocked"
            | "direct_merge_refused"
            | "invalid_head"
    ) {
        return None;
    }
    let state = match kind {
        "ready"
        | "admission_pending"
        | "enqueue_refused"
        | "generated_path_batch"
        | "defect_fix_removed" => LoopState::Ready,
        "no_verdict"
        | "waiting_head_approval"
        | "approval_excluded"
        | "waiting_code_owner"
        | "code_owner_review"
        | "carries_unapproved_commits" => LoopState::AwaitingReview,
        "required_pending" | "waiting_full_test_tier" | "waiting_enqueue_requirements" => {
            LoopState::CheckRunning
        }
        "capacity_queued" => LoopState::CapacityQueued,
        "required_failed" => LoopState::RequiredRed,
        "held" => LoopState::Hold,
        "ejected_same_head" | "ejection_cap" => LoopState::Ejected,
        "merge_state" => match token.split_once(':').map_or("", |(_, state)| state) {
            "DIRTY" => LoopState::Dirty,
            // Behind is never an act: the merge queue tests on top of main.
            "BEHIND" => LoopState::Ready,
            _ => LoopState::CheckRunning,
        },
        // A check that was interrupted or never started, an unreadable gate,
        // a steward error, and any token this table does not know yet: the
        // tooling around the PR, re-run next tick and escalated if it stays.
        _ => LoopState::Tooling,
    };
    // The quarantine file is never pruned: it only means "the review gave
    // up", so it applies only while the head still waits on review. An
    // approved, green, red or held head is classified by its real state.
    Some(if quarantined && state == LoopState::AwaitingReview {
        LoopState::Quarantined
    } else {
        state
    })
}

/// The `loop` object for a state entered at `since`, judged at `now`.
#[must_use]
pub(crate) fn classify(state: LoopState, since: DateTime<Utc>, now: DateTime<Utc>) -> Loop {
    let row = row(state);
    let Some(first) = row.first_minutes else {
        return Loop {
            state,
            since: since.to_rfc3339(),
            action: row.action.to_owned(),
            actor: row.first,
            deadline_at: None,
            level: row.start,
        };
    };
    // One deadline per level still above the start, each counted from
    // `since`.
    let offsets = [
        first,
        first + row.second_minutes,
        first + row.second_minutes + row.third_minutes,
    ];
    let deadlines = offsets
        .iter()
        .take(usize::from(MAX_LEVEL - row.start))
        .map(|minutes| since + Duration::minutes(*minutes))
        .collect::<Vec<_>>();
    let missed = deadlines
        .iter()
        .filter(|deadline| now >= **deadline)
        .count();
    let level = (row.start + u8::try_from(missed).unwrap_or(MAX_LEVEL)).min(MAX_LEVEL);
    let actor = if level == row.start {
        row.first
    } else {
        match level {
            1 => row.second,
            2 => LoopActor::StrongerModel,
            _ => LoopActor::Coordinator,
        }
    };
    Loop {
        state,
        since: since.to_rfc3339(),
        action: row.action.to_owned(),
        actor,
        deadline_at: deadlines.get(missed).map(DateTime::to_rfc3339),
        level,
    }
}

/// Heads the pass-runner quarantined, keyed `owner/repo#n` (lowercase):
/// the head and when.
pub(crate) type Quarantine = BTreeMap<String, (String, Option<DateTime<Utc>>)>;

/// Parse the pass-runner's `review-quarantine.txt`: one line per head,
/// `<repo>#<pr> <head> <reason> [since=<epoch>]`. The last line for a pull
/// request wins. Malformed lines are skipped.
#[must_use]
pub(crate) fn parse_quarantine(text: &str) -> Quarantine {
    let mut out = Quarantine::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(reference), Some(head)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !reference.contains('#')
            || head.len() != 40
            || !head.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            continue;
        }
        let since = parts
            .filter_map(|part| part.strip_prefix("since="))
            .filter_map(|epoch| epoch.parse::<i64>().ok())
            .find_map(|epoch| DateTime::from_timestamp(epoch, 0));
        out.insert(
            reference.to_ascii_lowercase(),
            (head.to_ascii_lowercase(), since),
        );
    }
    out
}

/// The quarantine file this host names, if any: the environment variable,
/// then the machine-global key.
#[must_use]
pub(crate) fn quarantine_path(global_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(QUARANTINE_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let config =
        crate::config::LoadedConfig::load_machine_global_from_dir(global_dir?.to_path_buf())
            .ok()?;
    config
        .get_str(QUARANTINE_KEY)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(
            |path| match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
                (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
                _ => PathBuf::from(path),
            },
        )
}

/// Read the quarantine file. Absent, unnamed or unreadable: no quarantine,
/// so the state never appears.
#[must_use]
pub(crate) fn read_quarantine(path: Option<&Path>) -> Quarantine {
    path.and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| parse_quarantine(&text))
        .unwrap_or_default()
}

/// The steward-ledger record of when each covered pull request entered its
/// current state at its current head, so `since` survives ticks for states
/// no read dates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Seen {
    pub(crate) state: LoopState,
    pub(crate) head: String,
    pub(crate) since: DateTime<Utc>,
    /// The last pass that saw it in this state: a later state at the same
    /// head cannot have begun before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) observed_at: Option<DateTime<Utc>>,
}

/// The facts one row brings.
#[derive(Clone, Debug)]
pub(crate) struct RowFacts<'a> {
    pub(crate) number: u64,
    pub(crate) head: &'a str,
    pub(crate) token: &'a str,
    pub(crate) decision: &'a StewardDecision,
    /// The row's own `since` (the steward's dated reads).
    pub(crate) since: Option<DateTime<Utc>>,
    /// Which GitHub time `since` came from (`check_completed`,
    /// `hold_posted`, ...).
    pub(crate) since_source: Option<&'a str>,
    /// When the dwell entry at this head began, when its state maps to the
    /// same loop state.
    pub(crate) dwell_since: Option<DateTime<Utc>>,
    /// When the pull request entered the merge queue.
    pub(crate) enqueued_at: Option<DateTime<Utc>>,
}

/// The `loop` object for one row, and the record to keep. `seen` is the
/// last record for this pull request; `quarantine` is the host's file.
#[must_use]
pub(crate) fn for_row(
    repo: &str,
    facts: &RowFacts<'_>,
    seen: Option<&Seen>,
    quarantine: &Quarantine,
    now: DateTime<Utc>,
) -> Option<(Loop, Seen)> {
    let key = format!("{}#{}", repo.to_ascii_lowercase(), facts.number);
    let quarantined_at = quarantine
        .get(&key)
        .filter(|(head, _)| head.eq_ignore_ascii_case(facts.head));
    let state = state_for(facts.token, facts.decision, quarantined_at.is_some())?;
    let same_head = seen.filter(|seen| seen.head.eq_ignore_ascii_case(facts.head));
    let since = match same_head {
        // Unchanged state at this head: the clock keeps running.
        Some(seen) if seen.state == state => seen.since,
        // The state changed at this head: it began after the last pass that
        // saw the old one. Only evidence specific to the new state dates it
        // earlier than this pass; anything else (a green time that predates
        // an approval, say) would start it at a later level than it earned.
        Some(seen) => specific_evidence(state, facts, quarantined_at)
            .filter(|at| seen.observed_at.is_none_or(|floor| *at >= floor))
            .unwrap_or(now),
        // First sight of this head (a push, or the first pass after a
        // deploy): the earliest evidence the pass has.
        None => specific_evidence(state, facts, quarantined_at)
            .or(facts.dwell_since)
            .or(facts.since)
            .unwrap_or(now),
    }
    .min(now);
    Some((
        classify(state, since, now),
        Seen {
            state,
            head: facts.head.to_ascii_lowercase(),
            since,
            observed_at: Some(now),
        },
    ))
}

/// A time that dates entering `state` itself: the check that failed, the
/// HOLD that was posted, the removal from the queue, the enqueue, the
/// quarantine line, the first required check starting. `None` when the row
/// has none.
fn specific_evidence(
    state: LoopState,
    facts: &RowFacts<'_>,
    quarantined_at: Option<&(String, Option<DateTime<Utc>>)>,
) -> Option<DateTime<Utc>> {
    let source = |want: &[&str]| {
        facts
            .since_source
            .filter(|source| want.contains(source))
            .and(facts.since)
    };
    match state {
        LoopState::Queued => facts.enqueued_at,
        LoopState::Quarantined => quarantined_at.and_then(|(_, since)| *since),
        LoopState::RequiredRed => source(&["check_completed"]),
        LoopState::Hold => source(&["hold_posted"]),
        LoopState::Ejected => source(&["queue_removed"]),
        LoopState::AwaitingReview => source(&["checks_green"]),
        LoopState::CheckRunning | LoopState::CapacityQueued => source(&["check_started"]),
        LoopState::Ready | LoopState::Dirty | LoopState::StuckGroup | LoopState::Tooling => None,
    }
}

/// Fill `loop` on every covered row of one pass. Reads nothing from GitHub;
/// the quarantine file is read once by the caller. Only an applying pass
/// keeps the `since` records.
#[allow(clippy::too_many_arguments)] // One pass's facts.
pub(super) fn annotate(
    reports: &mut [super::PrReport],
    observation: &super::RepoObservation,
    dwell: Option<&crate::merge_steward::dwell::DwellRepoState>,
    seen: &mut BTreeMap<String, Seen>,
    quarantine: &Quarantine,
    apply: bool,
    now: DateTime<Utc>,
) {
    let repo = observation.repo.as_str();
    let prefix = format!("{}#", repo.to_ascii_lowercase());
    let time = |text: &str| {
        DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|at| at.with_timezone(&Utc))
    };
    let kind = |token: &str| {
        token
            .split_once(':')
            .map_or(token, |(kind, _)| kind)
            .to_owned()
    };
    let mut next = BTreeMap::new();
    for report in reports.iter_mut() {
        let token = report.blocked.blocked_on.clone();
        let dwell_since = dwell
            .and_then(|dwell| dwell.entries.get(&report.number))
            .filter(|entry| {
                entry.head.eq_ignore_ascii_case(&report.head_sha)
                    && kind(&entry.token) == kind(&token)
            })
            .map(|entry| entry.entered_at);
        let key = format!("{prefix}{}", report.number);
        // A row the pass deferred was not classified this pass: it keeps the
        // state it had, so its clock neither restarts nor jumps.
        if super::pass_budget::is_deferred(report) {
            if let Some(record) = seen
                .get(&key)
                .filter(|record| record.head.eq_ignore_ascii_case(&report.head_sha))
            {
                report.blocked.loop_state = Some(classify(record.state, record.since, now));
            }
            continue;
        }
        let facts = RowFacts {
            number: report.number,
            head: &report.head_sha,
            token: &token,
            decision: &report.decision,
            since: report.blocked.since.as_deref().and_then(time),
            since_source: report.blocked.since_source,
            dwell_since,
            enqueued_at: observation
                .merge_group_enqueued_at
                .get(&report.number)
                .and_then(|at| time(at)),
        };
        if let Some((state, record)) = for_row(repo, &facts, seen.get(&key), quarantine, now) {
            report.blocked.loop_state = Some(state);
            next.insert(key, record);
        }
    }
    if apply {
        // Drop only pull requests no longer open (merged or closed). A row
        // missing from a partial or deferred report keeps its record.
        let open = observation
            .prs
            .iter()
            .map(|pr| format!("{prefix}{}", pr.fact.number))
            .collect::<std::collections::BTreeSet<_>>();
        seen.retain(|key, _| !key.starts_with(&prefix) || open.contains(key));
        seen.extend(next);
    }
}
