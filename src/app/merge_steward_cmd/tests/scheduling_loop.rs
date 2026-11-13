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
        ("defect_fix_removed:defect-fix", Some(LoopState::Ejected)),
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
fn a_stuck_group_outranks_the_queue_and_a_quarantine_replaces_awaiting_review() {
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
        Some(LoopState::Hold),
        "a quarantine replaces awaiting_review only"
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
    for state in [LoopState::Queued, LoopState::CapacityQueued] {
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
        since_source: None,
        dwell_since: None,
        enqueued_at: None,
    };
    let quarantine = BTreeMap::new();
    let seen = Seen {
        state: LoopState::RequiredRed,
        head: HEAD.to_owned(),
        since: ago(90),
        observed_at: Some(ago(5)),
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
        since_source: None,
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

/// Review finding 1: the quarantine file is never pruned, so it applies only
/// while the head still waits on review. The same head, since approved and
/// green (or held, or red), is classified by its real state.
#[test]
fn a_quarantined_head_that_is_now_approved_is_ready_not_quarantined() {
    let quarantine = parse_quarantine(&format!(
        "Generous-Corp/vellum#5\t{HEAD}\treviewer_unverified_exhausted\tsince=1763049600\n"
    ));
    let facts = |token: &'static str| RowFacts {
        number: 5,
        head: HEAD,
        token,
        decision: &ARM,
        since: None,
        since_source: None,
        dwell_since: None,
        enqueued_at: None,
    };
    for (token, want) in [
        ("ready", LoopState::Ready),
        ("held:review-verdict", LoopState::Hold),
        ("required_failed:macos", LoopState::RequiredRed),
        ("required_pending", LoopState::CheckRunning),
        ("no_verdict", LoopState::Quarantined),
    ] {
        let (state, _) = for_row(REPO, &facts(token), None, &quarantine, now()).expect("covered");
        assert_eq!(state.state, want, "{token}");
    }
}

/// Review finding 2: `awaiting_review` to `ready` at the same head. The row's
/// green time predates the approval; `ready` starts at the first pass that
/// saw it, at level 0, and does not page repair.
#[test]
fn a_state_change_at_the_same_head_starts_when_first_seen() {
    let waiting = Seen {
        state: LoopState::AwaitingReview,
        head: HEAD.to_owned(),
        since: ago(100),
        observed_at: Some(ago(5)),
    };
    let ready = RowFacts {
        number: 5,
        head: HEAD,
        token: "ready",
        decision: &ARM,
        since: Some(ago(100)),
        since_source: Some("checks_green"),
        dwell_since: None,
        enqueued_at: None,
    };
    let (state, record) =
        for_row(REPO, &ready, Some(&waiting), &BTreeMap::new(), now()).expect("covered");
    assert_eq!(state.state, LoopState::Ready);
    assert_eq!(state.since, now().to_rfc3339());
    assert_eq!((state.level, state.actor), (0, LoopActor::Steward));
    assert_eq!(record.observed_at, Some(now()));
    // check_running to ready: the same.
    let running = Seen {
        state: LoopState::CheckRunning,
        ..waiting.clone()
    };
    let (state, _) =
        for_row(REPO, &ready, Some(&running), &BTreeMap::new(), now()).expect("covered");
    assert_eq!(state.level, 0);
    // Evidence specific to the new state, after the old state was last
    // seen, dates it: the required check that failed two minutes ago.
    let red = RowFacts {
        token: "required_failed:macos",
        since: Some(ago(2)),
        since_source: Some("check_completed"),
        ..ready.clone()
    };
    let (state, _) = for_row(REPO, &red, Some(&running), &BTreeMap::new(), now()).expect("covered");
    assert_eq!(state.since, ago(2).to_rfc3339());
    // Evidence older than the last sighting of the old state is not trusted.
    let older = RowFacts {
        since: Some(ago(40)),
        ..red
    };
    let (state, _) =
        for_row(REPO, &older, Some(&running), &BTreeMap::new(), now()).expect("covered");
    assert_eq!(state.since, now().to_rfc3339());
}

/// Review finding 3: `admission_pending` is ready; a merge state GitHub keeps
/// BLOCKED (or UNKNOWN, UNSTABLE) is `check_running` and escalates after an
/// hour instead of never.
#[test]
fn admission_pending_is_ready_and_a_lasting_merge_state_escalates() {
    assert_eq!(
        state_for("admission_pending", &ARM, false),
        Some(LoopState::Ready)
    );
    for token in [
        "merge_state:BLOCKED",
        "merge_state:UNKNOWN",
        "merge_state:UNSTABLE",
    ] {
        assert_eq!(
            state_for(token, &ARM, false),
            Some(LoopState::CheckRunning),
            "{token}"
        );
    }
    let early = classify(LoopState::CheckRunning, ago(59), now());
    assert_eq!((early.level, early.actor), (0, LoopActor::Queue));
    let late = classify(LoopState::CheckRunning, ago(61), now());
    assert_eq!((late.level, late.actor), (1, LoopActor::Repair));
}

/// Review finding 4: a pull request missing from this pass's rows (a
/// partial or deferred report) keeps its record; only a pull request no
/// longer open loses it. A deferred row keeps the state it had.
#[test]
fn a_partial_report_keeps_records_and_only_closed_pull_requests_drop() {
    let mut observation = observation_for(ready_pr(), true);
    observation.repo = REPO.to_owned();
    // ready_pr is #42 and open; #7 is not in the open list (merged).
    let open_key = "generous-corp/vellum#42".to_owned();
    let closed_key = "generous-corp/vellum#7".to_owned();
    let record = Seen {
        state: LoopState::RequiredRed,
        head: HEAD.to_owned(),
        since: ago(90),
        observed_at: Some(ago(5)),
    };
    let mut seen = BTreeMap::from([
        (open_key.clone(), record.clone()),
        (closed_key.clone(), record.clone()),
    ]);
    let mut reports: Vec<PrReport> = Vec::new();
    scheduling_loop::annotate(
        &mut reports,
        &observation,
        None,
        &mut seen,
        &BTreeMap::new(),
        true,
        now(),
    );
    assert!(
        seen.contains_key(&open_key),
        "missing from the rows, still open: kept"
    );
    assert!(!seen.contains_key(&closed_key), "no longer open: dropped");
    // A deferred row keeps its state and since.
    let mut deferred = vec![row(42, ARM, "deferred_pass_budget", None)];
    deferred[0].mutation = Some(crate::app::merge_steward_cmd::pass_budget::DEFERRED.to_owned());
    scheduling_loop::annotate(
        &mut deferred,
        &observation,
        None,
        &mut seen,
        &BTreeMap::new(),
        true,
        now(),
    );
    let state = deferred[0].blocked.loop_state.as_ref().expect("kept");
    assert_eq!(state.state, LoopState::RequiredRed);
    assert_eq!(state.since, ago(90).to_rfc3339());
}

/// Review finding 6: the pass-runner's real line is tab-separated, with
/// extra fields.
#[test]
fn the_real_tab_separated_quarantine_line_parses() {
    let line = "Generous-Corp/pulp#12192\teadd0feb4c53603d07151cc1938eb26257eb87d1\treviewer_unverified_exhausted\tattempts=2\tproviders=claude,codex\tsince=1794581918\n";
    let quarantine = parse_quarantine(line);
    let (head, since) = quarantine.get("generous-corp/pulp#12192").expect("parsed");
    assert_eq!(head, "eadd0feb4c53603d07151cc1938eb26257eb87d1");
    assert_eq!(*since, DateTime::from_timestamp(1_794_581_918, 0));
}
