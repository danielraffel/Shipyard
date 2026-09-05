//! Tests for the escalation policy.
//!
//! Each behaviour is paired with a **planted control that must go red**. The
//! recurring shape here: for every rule that suppresses an action, there is a
//! sibling test proving the same input *does* act once the rule is satisfied —
//! otherwise a policy that never escalates anything would pass the whole file.

use chrono::{Duration, TimeZone, Utc};

use super::*;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 5, 2, 0, 0).unwrap()
}

fn raising(verdict: ServiceVerdict, minutes_raising: i64) -> SubjectState {
    SubjectState {
        key: "host=macpro lane=PULP_AUTO_LINUX_RUNS_ON_JSON".to_owned(),
        host: "macpro".to_owned(),
        lane: "PULP_AUTO_LINUX_RUNS_ON_JSON".to_owned(),
        verdict,
        boundary: None,
        detail: "declared local, served by nobody: no runner in the repo or org scope \
                 advertises these labels"
            .to_owned(),
        attempted: vec![],
        next_action: "Destroy the orphaned clones holding the VMID range, then confirm a \
                      runner registers."
            .to_owned(),
        raising_since: Some(now() - Duration::minutes(minutes_raising)),
        healthy_since: None,
    }
}

fn healthy(minutes_healthy: i64) -> SubjectState {
    SubjectState {
        verdict: ServiceVerdict::Served,
        detail: "2 online runner(s) advertise these labels (org scope only)".to_owned(),
        raising_since: None,
        healthy_since: Some(now() - Duration::minutes(minutes_healthy)),
        ..raising(ServiceVerdict::Unserved, 0)
    }
}

fn issue_for(subject: &SubjectState, body: &str) -> TrackingIssue {
    TrackingIssue {
        number: 4242,
        key: subject.key.clone(),
        body: body.to_owned(),
    }
}

fn decide(subject: &SubjectState, existing: Option<&TrackingIssue>) -> EscalationAction {
    decide_escalation(subject, existing, EscalationThresholds::default(), now())
}

// ---------------------------------------------------------------------------
// Hysteresis on the way up — a flicker is not a fault
// ---------------------------------------------------------------------------

/// Every signal on this fleet flickers, so a single raising sample must not
/// open an issue. A flapping alarm is ignored, and the one real occurrence is
/// ignored with it.
#[test]
fn a_brief_raise_does_not_open_anything() {
    let action = decide(&raising(ServiceVerdict::Unserved, 2), None);
    assert_eq!(action.kind(), "nothing");
    assert!(!action.is_mutation());
}

/// Planted control for the test above. Same subject, same verdict, same
/// absence of an issue — only the duration crosses the threshold. Without this
/// the suppression rule could be suppressing everything forever.
#[test]
fn control_a_sustained_raise_does_open() {
    let subject = raising(ServiceVerdict::Unserved, 20);
    let action = decide(&subject, None);
    match action {
        EscalationAction::Open { key, title, body } => {
            assert_eq!(key, subject.key);
            assert!(title.contains("macpro"), "{title}");
            assert!(title.contains("unserved"), "{title}");
            assert!(body.contains(&subject_marker(&subject.key)), "{body}");
        }
        other => panic!("expected an open, got {other:?}"),
    }
}

#[test]
fn the_boundary_at_exactly_the_threshold_opens() {
    let subject = raising(ServiceVerdict::Unserved, 15);
    assert_eq!(decide(&subject, None).kind(), "open");
}

/// A raising verdict with no start time is an incomplete observation. It must
/// not open (the duration is unknowable) and must not be silently dropped as
/// healthy either.
#[test]
fn a_raise_without_a_start_time_is_reported_not_assumed() {
    let mut subject = raising(ServiceVerdict::Unserved, 20);
    subject.raising_since = None;
    match decide(&subject, None) {
        EscalationAction::Nothing { reason } => {
            assert!(reason.contains("no raising-since"), "{reason}");
        }
        other => panic!("expected nothing, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Hysteresis on the way down — recovery has to hold
// ---------------------------------------------------------------------------

#[test]
fn a_brief_recovery_does_not_close_the_issue() {
    let subject = healthy(5);
    let issue = issue_for(&subject, "stale body");
    match decide(&subject, Some(&issue)) {
        EscalationAction::Nothing { reason } => {
            assert!(reason.contains("clear threshold"), "{reason}");
        }
        other => panic!("expected nothing, got {other:?}"),
    }
}

/// Planted control: the same recovered subject, held long enough.
#[test]
fn control_a_sustained_recovery_does_close_the_issue() {
    let subject = healthy(45);
    let issue = issue_for(&subject, "stale body");
    match decide(&subject, Some(&issue)) {
        EscalationAction::Close { number, comment } => {
            assert_eq!(number, 4242);
            assert!(comment.contains("Recovered"), "{comment}");
            assert!(comment.contains("macpro"), "{comment}");
        }
        other => panic!("expected a close, got {other:?}"),
    }
}

/// Clearing must be slower than raising, or a fault that briefly looks fixed
/// closes its own issue and starts the cycle over.
#[test]
fn the_clear_threshold_is_slower_than_the_raise_threshold() {
    let thresholds = EscalationThresholds::default();
    assert!(thresholds.clear_after_secs > thresholds.raise_after_secs);
}

#[test]
fn a_healthy_subject_with_no_issue_does_nothing() {
    let action = decide(&healthy(600), None);
    assert_eq!(action.kind(), "nothing");
}

#[test]
fn a_recovery_without_a_healthy_since_keeps_the_issue_open() {
    let mut subject = healthy(600);
    subject.healthy_since = None;
    let issue = issue_for(&subject, "body");
    match decide(&subject, Some(&issue)) {
        EscalationAction::Nothing { reason } => {
            assert!(reason.contains("stays open"), "{reason}");
        }
        other => panic!("expected nothing, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Not re-posting an unchanged body
// ---------------------------------------------------------------------------

#[test]
fn an_unchanged_body_is_left_alone() {
    let subject = raising(ServiceVerdict::Unserved, 20);
    let EscalationAction::Open { body, .. } = decide(&subject, None) else {
        panic!("expected an open");
    };
    let issue = issue_for(&subject, &body);
    match decide(&subject, Some(&issue)) {
        EscalationAction::Nothing { reason } => {
            assert!(reason.contains("unchanged"), "{reason}");
        }
        other => panic!("expected nothing, got {other:?}"),
    }
}

/// Planted control for the test above: a genuine content change must still be
/// posted. Otherwise "leave it alone" would silently swallow every update.
#[test]
fn control_a_changed_body_is_updated() {
    let subject = raising(ServiceVerdict::Unserved, 20);
    let issue = issue_for(&subject, "an older, different body");
    match decide(&subject, Some(&issue)) {
        EscalationAction::Update { number, body } => {
            assert_eq!(number, 4242);
            assert!(body.contains("macpro"), "{body}");
        }
        other => panic!("expected an update, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Matching the right issue
// ---------------------------------------------------------------------------

/// Editing an unrelated report is worse than opening a duplicate, so a
/// mismatched key is treated as no issue at all.
#[test]
fn an_issue_for_a_different_subject_is_not_adopted() {
    let subject = raising(ServiceVerdict::Unserved, 20);
    let other = TrackingIssue {
        number: 9999,
        key: "host=m5 lane=SOMETHING_ELSE".to_owned(),
        body: "unrelated".to_owned(),
    };
    assert_eq!(decide(&subject, Some(&other)).kind(), "open");
}

/// One issue per subject. A single roll-up would hide the second instance of a
/// fault behind the first — which is precisely how a supervisor fault on one
/// host stayed hidden for hours after its twin was found and repaired.
#[test]
fn each_subject_gets_its_own_issue() {
    let mut first = raising(ServiceVerdict::Unserved, 20);
    first.key = "host=m3 lane=release".to_owned();
    first.host = "m3".to_owned();
    let mut second = raising(ServiceVerdict::Unserved, 20);
    second.key = "host=m5 lane=release".to_owned();
    second.host = "m5".to_owned();

    let actions = decide_all(
        &[first, second],
        &[],
        EscalationThresholds::default(),
        now(),
    );
    assert_eq!(actions.len(), 2);
    assert!(actions.iter().all(|action| action.kind() == "open"));
    let keys: Vec<&str> = actions
        .iter()
        .filter_map(|action| match action {
            EscalationAction::Open { key, .. } => Some(key.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(keys, vec!["host=m3 lane=release", "host=m5 lane=release"]);
}

#[test]
fn decide_all_matches_each_subject_to_its_own_existing_issue() {
    let mut first = raising(ServiceVerdict::Unserved, 20);
    first.key = "host=m3 lane=release".to_owned();
    let mut second = healthy(600);
    second.key = "host=m5 lane=release".to_owned();

    let existing = vec![TrackingIssue {
        number: 77,
        key: "host=m5 lane=release".to_owned(),
        body: "old".to_owned(),
    }];
    let actions = decide_all(
        &[first, second],
        &existing,
        EscalationThresholds::default(),
        now(),
    );
    assert_eq!(actions[0].kind(), "open");
    assert_eq!(actions[1].kind(), "close");
}

// ---------------------------------------------------------------------------
// Every verdict that raises, raises — including the one that means "I cannot see"
// ---------------------------------------------------------------------------

/// An assertion that could not measure is not an assertion that passed, so it
/// escalates like any other fault — and the body must name its boundary and the
/// action that follows from it.
#[test]
fn an_unknown_verdict_escalates_and_names_its_boundary() {
    let mut subject = raising(ServiceVerdict::Unknown, 20);
    subject.boundary = Some(Boundary::Transport);
    match decide(&subject, None) {
        EscalationAction::Open { body, .. } => {
            assert!(body.contains("transport"), "{body}");
            assert!(body.contains("This is not a pass"), "{body}");
            assert!(
                body.contains("not an authentication"),
                "the boundary's own remedy must reach the reader: {body}"
            );
        }
        other => panic!("expected an open, got {other:?}"),
    }
}

#[test]
fn every_raising_verdict_opens_and_no_passing_verdict_does() {
    for verdict in [
        ServiceVerdict::Degraded,
        ServiceVerdict::Starved,
        ServiceVerdict::Unserved,
        ServiceVerdict::Unknown,
    ] {
        assert_eq!(
            decide(&raising(verdict, 20), None).kind(),
            "open",
            "{} must escalate",
            verdict.as_str()
        );
    }

    // Control: the verdicts that mean "fine" must never open, however long
    // they have been reported.
    for verdict in [ServiceVerdict::Served, ServiceVerdict::Idle] {
        let mut subject = healthy(600);
        subject.verdict = verdict;
        assert_eq!(
            decide(&subject, None).kind(),
            "nothing",
            "{} must not escalate",
            verdict.as_str()
        );
    }
}

// ---------------------------------------------------------------------------
// The body has to be usable
// ---------------------------------------------------------------------------

/// An alert that only says something is wrong makes its reader start from
/// nothing, which is most of the cost of the incidents this exists to prevent.
#[test]
fn the_body_names_host_lane_what_was_tried_and_what_to_do() {
    let mut subject = raising(ServiceVerdict::Unserved, 20);
    subject.attempted = vec![
        "reaped 0 clones — provenance unreadable on 3 of them, so none were touched".to_owned(),
    ];
    let EscalationAction::Open { body, .. } = decide(&subject, None) else {
        panic!("expected an open");
    };

    assert!(body.contains("macpro"), "host: {body}");
    assert!(
        body.contains("PULP_AUTO_LINUX_RUNS_ON_JSON"),
        "lane: {body}"
    );
    assert!(body.contains("provenance unreadable"), "attempted: {body}");
    assert!(body.contains("Destroy the orphaned clones"), "next: {body}");
    assert!(body.contains("served by nobody"), "detail: {body}");
}

/// When nothing was attempted the body must say so explicitly, rather than
/// leaving a reader to guess whether a self-heal ran and failed.
#[test]
fn an_empty_attempt_list_is_stated_rather_than_omitted() {
    let EscalationAction::Open { body, .. } = decide(&raising(ServiceVerdict::Unserved, 20), None)
    else {
        panic!("expected an open");
    };
    assert!(body.contains("Nothing —"), "{body}");
}

/// The marker is how an issue is matched back to its subject without depending
/// on a title a human may edit.
#[test]
fn the_body_carries_a_stable_machine_readable_marker() {
    let subject = raising(ServiceVerdict::Unserved, 20);
    let EscalationAction::Open { body, .. } = decide(&subject, None) else {
        panic!("expected an open");
    };
    assert!(body.starts_with(&subject_marker(&subject.key)));
    assert!(subject_marker("k").contains("shipyard-fleet-subject"));
}

#[test]
fn action_kinds_and_mutation_flags_are_stable() {
    assert!(
        EscalationAction::Open {
            key: "k".to_owned(),
            title: "t".to_owned(),
            body: "b".to_owned()
        }
        .is_mutation()
    );
    assert!(
        EscalationAction::Update {
            number: 1,
            body: "b".to_owned()
        }
        .is_mutation()
    );
    assert!(
        EscalationAction::Close {
            number: 1,
            comment: "c".to_owned()
        }
        .is_mutation()
    );
    assert!(
        !EscalationAction::Nothing {
            reason: "r".to_owned()
        }
        .is_mutation()
    );
}
