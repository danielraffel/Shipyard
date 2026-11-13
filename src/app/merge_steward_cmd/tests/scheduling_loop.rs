//! The scheduling loop contract: one state per covered pull request, from
//! the row the pass already built, with its act, actor, deadline and level.
use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use super::*;
use crate::app::merge_steward_cmd::blocked_on::Stall;
use crate::app::merge_steward_cmd::scheduling_loop::{
    self, LoopActor, LoopState, RowFacts, Seen, classify, for_row, parse_quarantine, state_for,
};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REPO: &str = "Generous-Corp/vellum";

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-13T16:00:00Z")
        .expect("time")
        .with_timezone(&Utc)
}

fn ago(minutes: i64) -> DateTime<Utc> {
    now() - Duration::minutes(minutes)
}

const ARM: StewardDecision = StewardDecision::ArmMergeQueue;

#[test]
fn every_blocked_on_token_maps_to_exactly_one_state_or_is_outside_the_loop() {
    let cases = [
        ("ready", Some(LoopState::Ready)),
        ("merge_state:BEHIND", Some(LoopState::Ready)),
        ("enqueue_refused:no", Some(LoopState::Ready)),
        ("generated_path_batch:#7", Some(LoopState::Ready)),
        ("no_verdict", Some(LoopState::AwaitingReview)),
        (
            "approval_excluded:unattributed",
            Some(LoopState::AwaitingReview),
        ),
        ("waiting_code_owner:@x", Some(LoopState::AwaitingReview)),
        (
            "carries_unapproved_commits:#9",
            Some(LoopState::AwaitingReview),
        ),
        ("required_pending", Some(LoopState::CheckRunning)),
        ("merge_state:UNKNOWN", Some(LoopState::CheckRunning)),
        ("capacity_queued", Some(LoopState::CapacityQueued)),
        ("required_failed:macos", Some(LoopState::RequiredRed)),
        ("held:review-verdict", Some(LoopState::Hold)),
        ("merge_state:DIRTY", Some(LoopState::Dirty)),
        ("ejected_same_head:failed_checks", Some(LoopState::Ejected)),
        ("ejection_cap", Some(LoopState::Ejected)),
        (
            "merge_group_stalled:framework-trusted-verify",
            Some(LoopState::StuckGroup),
        ),
        ("transient_rerun", Some(LoopState::Tooling)),
        ("required_never_ran:macos", Some(LoopState::Tooling)),
        ("steward_error", Some(LoopState::Tooling)),
        ("approval_unread", Some(LoopState::Tooling)),
        ("some_future_token", Some(LoopState::Tooling)),
        ("draft", None),
        ("unmanaged", None),
        ("owner_decision", None),
        ("excluded_label:superseded", None),
        ("provenance_blocked:needs-provenance", None),
    ];
    for (token, want) in cases {
        assert_eq!(state_for(token, &ARM, false), want, "{token}");
    }
}

/// vellum#412: queued at position 1 while a stale, non-required `judge`
/// check run is red on the head. In the merge queue a pull request is
/// `queued`, never `required_red`, whatever its row's token says.
#[test]
fn a_queued_pull_request_is_never_required_red() {
    let queued = StewardDecision::Queued { position: 1 };
    for token in [
        "queued",
        "required_failed:judge",
        "held:review-verdict",
        "merge_state:DIRTY",
    ] {
        assert_eq!(
            state_for(token, &queued, false),
            Some(LoopState::Queued),
            "{token}"
        );
    }
    assert_eq!(state_for("admitted", &ARM, false), Some(LoopState::Queued));
    let mut reports = vec![row(
        412,
        queued.clone(),
        "required_failed:judge",
        Some(ago(200)),
    )];
    let mut observation = observation_for(ready_pr(), true);
    observation.repo = REPO.to_owned();
    observation.merge_group_enqueued_at = BTreeMap::from([(412, ago(20).to_rfc3339())]);
    let mut seen = BTreeMap::new();
    scheduling_loop::annotate(
        &mut reports,
        &observation,
        None,
        &mut seen,
        &BTreeMap::new(),
        true,
        now(),
    );
    let state = reports[0].blocked.loop_state.as_ref().expect("covered");
    assert_eq!(state.state, LoopState::Queued);
    assert_eq!(state.actor, LoopActor::Queue);
    assert_eq!(state.deadline_at, None);
    assert_eq!(state.level, 0);
    assert_eq!(
        state.since,
        ago(20).to_rfc3339(),
        "since entering the queue"
    );
}

#[test]
fn a_stuck_group_outranks_the_queue_and_a_quarantine_outranks_the_token() {
    let queued = StewardDecision::Queued { position: 1 };
    assert_eq!(
        state_for("merge_group_stalled:x", &queued, true),
        Some(LoopState::StuckGroup)
    );
    assert_eq!(state_for("queued", &queued, true), Some(LoopState::Queued));
    assert_eq!(
        state_for("no_verdict", &ARM, true),
        Some(LoopState::Quarantined)
    );
    assert_eq!(
        state_for("held:review-verdict", &ARM, true),
        Some(LoopState::Quarantined)
    );
    assert_eq!(
        state_for("draft", &ARM, true),
        None,
        "outside the loop stays outside"
    );
}

#[test]
fn each_missed_deadline_raises_the_level_one_rung() {
    let at = |state, minutes| classify(state, ago(minutes), now());
    // ready: steward within 2 min, then repair, stronger model, coordinator.
    let ready = at(LoopState::Ready, 1);
    assert_eq!((ready.level, ready.actor), (0, LoopActor::Steward));
    assert_eq!(ready.deadline_at, Some(ago(-1).to_rfc3339()));
    assert_eq!(at(LoopState::Ready, 3).actor, LoopActor::Repair);
    assert_eq!(at(LoopState::Ready, 33).actor, LoopActor::StrongerModel);
    let last = at(LoopState::Ready, 63);
    assert_eq!(
        (last.level, last.actor, last.deadline_at),
        (3, LoopActor::Coordinator, None)
    );
    // required_red: a repair claim in 10 min, takeover at 60 without a push.
    assert_eq!(at(LoopState::RequiredRed, 5).actor, LoopActor::Repair);
    let claim_missed = at(LoopState::RequiredRed, 15);
    assert_eq!(
        (claim_missed.level, claim_missed.actor),
        (1, LoopActor::Repair)
    );
    assert_eq!(
        claim_missed.deadline_at,
        Some(ago(-45).to_rfc3339()),
        "takeover at 60 min"
    );
    assert_eq!(
        at(LoopState::RequiredRed, 61).actor,
        LoopActor::StrongerModel
    );
    assert_eq!(at(LoopState::Hold, 121).actor, LoopActor::Coordinator);
    // awaiting_review: the reviewer for 30 min, then the adversary.
    assert_eq!(at(LoopState::AwaitingReview, 29).actor, LoopActor::Reviewer);
    assert_eq!(
        at(LoopState::AwaitingReview, 31).actor,
        LoopActor::Adversary
    );
    // dirty: 5 min.
    assert_eq!(at(LoopState::Dirty, 6).level, 1);
    // quarantined: a stronger model at once (level 2), then the coordinator.
    let quarantined = at(LoopState::Quarantined, 0);
    assert_eq!(
        (quarantined.level, quarantined.actor),
        (2, LoopActor::StrongerModel)
    );
    assert_eq!(at(LoopState::Quarantined, 31).actor, LoopActor::Coordinator);
    // No deadline while the queue or CI acts.
    for state in [
        LoopState::Queued,
        LoopState::CheckRunning,
        LoopState::CapacityQueued,
    ] {
        let waiting = at(state, 600);
        assert_eq!((waiting.level, waiting.deadline_at), (0, None), "{state:?}");
    }
}

#[test]
fn since_is_the_earliest_evidence_at_this_head_and_a_new_head_starts_over() {
    let facts = |head: &'static str, since: Option<DateTime<Utc>>| RowFacts {
        number: 5,
        head,
        token: "required_failed:macos",
        decision: &ARM,
        since,
        dwell_since: None,
        enqueued_at: None,
    };
    let quarantine = BTreeMap::new();
    let seen = Seen {
        state: LoopState::RequiredRed,
        head: HEAD.to_owned(),
        since: ago(90),
    };
    let (carried, _) = for_row(
        REPO,
        &facts(HEAD, Some(ago(10))),
        Some(&seen),
        &quarantine,
        now(),
    )
    .expect("covered");
    assert_eq!(
        carried.since,
        ago(90).to_rfc3339(),
        "the record carries across ticks"
    );
    let other = "b".repeat(40);
    let other: &'static str = Box::leak(other.into_boxed_str());
    let (fresh, record) = for_row(
        REPO,
        &facts(other, Some(ago(10))),
        Some(&seen),
        &quarantine,
        now(),
    )
    .expect("covered");
    assert_eq!(fresh.since, ago(10).to_rfc3339(), "a push starts it over");
    assert_eq!(record.head, other);
    let (undated, _) =
        for_row(REPO, &facts(HEAD, None), None, &quarantine, now()).expect("covered");
    assert_eq!(undated.since, now().to_rfc3339(), "first seen now");
}

#[test]
fn the_quarantine_file_names_heads_and_only_the_current_head_is_quarantined() {
    let text = format!(
        "Generous-Corp/vellum#5 {HEAD} reviewer_unverified_exhausted since=1763049600\nnot a line\nGenerous-Corp/pulp#9 short reason\n"
    );
    let quarantine = parse_quarantine(&text);
    assert_eq!(quarantine.len(), 1);
    let facts = |head: &'static str| RowFacts {
        number: 5,
        head,
        token: "no_verdict",
        decision: &ARM,
        since: None,
        dwell_since: None,
        enqueued_at: None,
    };
    let (state, _) = for_row(REPO, &facts(HEAD), None, &quarantine, now()).expect("covered");
    assert_eq!(state.state, LoopState::Quarantined);
    assert_eq!(
        state.since,
        DateTime::from_timestamp(1_763_049_600, 0)
            .expect("t")
            .to_rfc3339()
    );
    let other: &'static str = Box::leak("c".repeat(40).into_boxed_str());
    let (state, _) = for_row(REPO, &facts(other), None, &quarantine, now()).expect("covered");
    assert_eq!(
        state.state,
        LoopState::AwaitingReview,
        "a new head is not quarantined"
    );
    assert!(
        scheduling_loop::read_quarantine(None).is_empty(),
        "no file, no state"
    );
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
            ..Stall::default()
        },
    }
}

#[test]
fn the_row_carries_the_loop_object_and_only_an_applying_pass_keeps_records() {
    let mut observation = observation_for(ready_pr(), true);
    observation.repo = REPO.to_owned();
    let mut reports = vec![
        row(397, ARM, "required_failed:macos", Some(ago(65))),
        row(8, ARM, "draft", None),
    ];
    let mut seen = BTreeMap::new();
    scheduling_loop::annotate(
        &mut reports,
        &observation,
        None,
        &mut seen,
        &BTreeMap::new(),
        false,
        now(),
    );
    assert!(seen.is_empty(), "a report pass keeps nothing");
    let value = serde_json::to_value(&reports[0]).expect("json");
    let state = &value["loop"];
    assert_eq!(state["state"], "required_red");
    assert_eq!(state["actor"], "stronger_model");
    assert_eq!(state["level"], 2);
    assert_eq!(state["since"], ago(65).to_rfc3339());
    assert!(state["deadline_at"].is_string());
    assert!(state["action"].is_string());
    assert!(
        serde_json::to_value(&reports[1])
            .expect("json")
            .get("loop")
            .is_none()
    );
    scheduling_loop::annotate(
        &mut reports,
        &observation,
        None,
        &mut seen,
        &BTreeMap::new(),
        true,
        now(),
    );
    assert_eq!(seen.len(), 1);
}
