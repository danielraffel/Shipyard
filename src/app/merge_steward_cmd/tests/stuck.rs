//! Stuck rows: red on a required context or held at the head, with the
//! time it began and the escalation level, from what the pass already has.
use chrono::{DateTime, Duration, Utc};

use super::*;
use crate::app::merge_steward_cmd::blocked_on::Stall;
use crate::app::merge_steward_cmd::next_action::{Actor, NextAction};
use crate::app::merge_steward_cmd::stuck;
use crate::merge_steward::dwell::{DwellEntry, DwellRepoState, DwellState};
use crate::merge_steward::stuck::Escalate;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-13T16:00:00Z")
        .expect("time")
        .with_timezone(&Utc)
}

fn row(
    number: u64,
    decision: StewardDecision,
    token: &str,
    since: Option<DateTime<Utc>>,
) -> PrReport {
    PrReport {
        number,
        head_sha: HEAD.to_owned(),
        decision,
        mutation: None,
        error: None,
        blocked: Stall {
            blocked_on: token.to_owned(),
            since: since.map(|at| at.to_rfc3339()),
            next_action: Some(NextAction {
                actor: Actor::Author,
                action: "fix and push".to_owned(),
                stop: false,
                escalate: None,
            }),
            ..Stall::default()
        },
    }
}

fn dwell(number: u64, state: DwellState, head: &str, minutes_ago: i64) -> DwellRepoState {
    let mut dwell = DwellRepoState::default();
    dwell.entries.insert(
        number,
        DwellEntry {
            state,
            token: state.key().to_owned(),
            head: head.to_owned(),
            entered_at: now() - Duration::minutes(minutes_ago),
            last_seen_at: now(),
            escalated_level: 0,
            seeded_level: 0,
        },
    );
    dwell
}

fn escalate(report: &PrReport) -> Option<Escalate> {
    report
        .blocked
        .next_action
        .as_ref()
        .and_then(|next| next.escalate)
}

#[test]
fn a_red_required_head_escalates_to_its_owner_at_30_minutes_and_repair_at_60() {
    for (minutes, want) in [
        (20, None),
        (35, Some(Escalate::Owner)),
        (65, Some(Escalate::Repair)),
    ] {
        let mut reports = vec![row(
            397,
            StewardDecision::ArmMergeQueue,
            "required_failed:macos",
            None,
        )];
        let state = dwell(397, DwellState::RequiredFailed, HEAD, minutes);
        stuck::annotate(&mut reports, Some(&state), now());
        assert_eq!(
            reports[0].blocked.stuck_since.as_deref(),
            Some((now() - Duration::minutes(minutes)).to_rfc3339().as_str())
        );
        assert_eq!(escalate(&reports[0]), want, "{minutes} min");
    }
}

#[test]
fn a_hold_at_the_head_is_stuck_too() {
    let mut reports = vec![row(
        5,
        StewardDecision::ArmMergeQueue,
        "held:review-verdict",
        Some(now() - Duration::minutes(40)),
    )];
    stuck::annotate(&mut reports, None, now());
    assert_eq!(escalate(&reports[0]), Some(Escalate::Owner));
}

/// vellum#412: queued at position 1 with a stale, non-required `judge`
/// check run red on its head. A merge-queue entry reads `queued` and is
/// never stuck, whatever its head's checks or its dwell history say.
#[test]
fn a_queued_pull_request_is_never_stuck() {
    let state = dwell(412, DwellState::RequiredFailed, HEAD, 200);
    let mut reports = vec![
        row(412, StewardDecision::Queued { position: 1 }, "queued", None),
        // Even a stale red token on a queued row does not make it stuck.
        row(
            413,
            StewardDecision::Queued { position: 2 },
            "required_failed:judge",
            Some(now() - Duration::minutes(200)),
        ),
    ];
    stuck::annotate(&mut reports, Some(&state), now());
    for report in &reports {
        assert_eq!(report.blocked.stuck_since, None, "#{}", report.number);
        assert_eq!(escalate(report), None, "#{}", report.number);
    }
}

#[test]
fn a_push_starts_the_clock_over() {
    // The dwell entry is for an older head; the new head failed 10 min ago.
    let state = dwell(9, DwellState::RequiredFailed, &"b".repeat(40), 120);
    let mut reports = vec![row(
        9,
        StewardDecision::ArmMergeQueue,
        "required_failed:macos",
        Some(now() - Duration::minutes(10)),
    )];
    stuck::annotate(&mut reports, Some(&state), now());
    assert_eq!(escalate(&reports[0]), None);
    assert_eq!(
        reports[0].blocked.stuck_since.as_deref(),
        Some((now() - Duration::minutes(10)).to_rfc3339().as_str())
    );
}

#[test]
fn an_infra_rerun_an_undated_row_and_other_states_are_not_stuck() {
    let infra = dwell(1, DwellState::RequiredFailedInfra, HEAD, 120);
    let mut reports = vec![row(
        1,
        StewardDecision::ArmMergeQueue,
        "required_failed:macos",
        None,
    )];
    stuck::annotate(&mut reports, Some(&infra), now());
    assert_eq!(escalate(&reports[0]), None, "the steward is re-running it");
    let mut undated = vec![row(
        2,
        StewardDecision::ArmMergeQueue,
        "required_failed:macos",
        None,
    )];
    stuck::annotate(&mut undated, None, now());
    assert_eq!(undated[0].blocked.stuck_since, None);
    let mut pending = vec![row(
        3,
        StewardDecision::ArmMergeQueue,
        "required_pending",
        Some(now() - Duration::minutes(120)),
    )];
    stuck::annotate(&mut pending, None, now());
    assert_eq!(escalate(&pending[0]), None);
}

/// The tick JSON carries the signal where the relay reads it.
#[test]
fn the_row_serializes_stuck_since_and_escalate() {
    let mut reports = vec![row(
        397,
        StewardDecision::ArmMergeQueue,
        "required_failed:macos",
        Some(now() - Duration::minutes(70)),
    )];
    stuck::annotate(&mut reports, None, now());
    let value = serde_json::to_value(&reports[0]).expect("json");
    assert_eq!(value["next_action"]["escalate"], "repair");
    assert!(value["stuck_since"].is_string(), "{value}");
    let events = crate::relay::steward_overdue(&serde_json::json!({"repos": [{"prs": [value]}]}));
    assert!(events.contains_key(&397));
}
