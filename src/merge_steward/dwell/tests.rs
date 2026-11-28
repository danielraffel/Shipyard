use std::collections::BTreeSet;

use chrono::{DateTime, TimeZone, Utc};

use super::*;

const APPROVAL: &str = "review-verdict";
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap()
}

/// One representative `blocked_on` token per state.
fn token(state: DwellState) -> &'static str {
    match state {
        DwellState::NeedsReview => "no_verdict",
        DwellState::RequiredPending => "required_pending",
        DwellState::RequiredFailedInfra => "transient_rerun",
        DwellState::RequiredFailed => "required_failed:macos",
        DwellState::StaleVerdictCheck => "required_failed:review-verdict",
        DwellState::Dirty => "merge_state:DIRTY",
        DwellState::Held => "held",
        DwellState::RequiredNeverRan => "required_never_ran:macos",
        DwellState::ApprovedGreenNotQueued => "ready",
        DwellState::CarriesUnapproved => "carries_unapproved_commits:#7",
        DwellState::StackedBaseMerged => STACKED_BASE_MERGED,
        DwellState::StaleMergeRef => "stale_merge_ref:macos",
        // Not a `blocked_on` state: `track_stuck` keeps its clock.
        DwellState::ApprovedStuck => "",
        DwellState::BaseStalled => "base_stalled",
    }
}

fn sighting(number: u64, token: &str, head: &str, minutes_ago: Option<i64>) -> Sighting {
    Sighting {
        number,
        head: head.to_owned(),
        token: token.to_owned(),
        since: minutes_ago.map(|minutes| minutes_before(now(), minutes)),
    }
}

fn observed(numbers: &[u64]) -> BTreeSet<u64> {
    numbers.iter().copied().collect()
}

/// A repository the steward already tracks (not its first pass, which
/// seeds silently).
fn established() -> DwellRepoState {
    DwellRepoState {
        initialized: true,
        ..DwellRepoState::default()
    }
}

fn tracked_once(sightings: &[Sighting], policy: &DwellPolicy) -> DwellRepoState {
    let mut state = established();
    let numbers: Vec<u64> = sightings.iter().map(|s| s.number).collect();
    track(
        &mut state,
        sightings,
        &observed(&numbers),
        &BTreeSet::new(),
        true,
        policy,
        APPROVAL,
        now(),
    );
    state
}

#[test]
fn defaults_are_the_specified_limits() {
    let policy = DwellPolicy::default();
    let expected = [
        (DwellState::NeedsReview, 20),
        (DwellState::RequiredPending, 60),
        (DwellState::RequiredFailedInfra, 10),
        (DwellState::RequiredFailed, 45),
        (DwellState::StaleVerdictCheck, 10),
        (DwellState::Dirty, 30),
        (DwellState::Held, 30),
        (DwellState::RequiredNeverRan, 30),
        (DwellState::ApprovedGreenNotQueued, 5),
        (DwellState::CarriesUnapproved, 60),
        (DwellState::StackedBaseMerged, 10),
    ];
    for (state, minutes) in expected {
        assert_eq!(policy.limit(state), Some(minutes), "{state:?}");
    }
    assert_eq!(
        policy.act,
        ActMode::Shadow,
        "GitHub-writing acts ship in shadow"
    );
}

#[test]
fn retrigger_marks_are_retained_when_history_grows() {
    let mut state = established();
    state.mark_retrigger(1, HEAD, true, true, now());

    for number in 2..=2049 {
        let head = format!("{number:040x}");
        state.mark_retrigger(number, &head, true, true, now());
    }

    assert_eq!(state.retriggers.len(), 2049);
    assert!(state.retrigger_spent(1, HEAD).is_some());
}

#[test]
fn every_state_is_overdue_exactly_at_its_limit() {
    let policy = DwellPolicy::default();
    for state in DwellState::ALL {
        if matches!(state, DwellState::ApprovedStuck | DwellState::BaseStalled) {
            continue;
        }
        let limit = i64::from(policy.limit(state).unwrap());
        assert_eq!(
            state_for_token(token(state), APPROVAL),
            Some(state),
            "{state:?}"
        );
        let under = tracked_once(&[sighting(1, token(state), HEAD, Some(limit - 1))], &policy);
        assert!(
            overdue(&under, &policy, now()).is_empty(),
            "{state:?} under its limit"
        );
        let at = tracked_once(&[sighting(1, token(state), HEAD, Some(limit))], &policy);
        let rows = overdue(&at, &policy, now());
        assert_eq!(rows.len(), 1, "{state:?} at its limit");
        assert_eq!(rows[0].state, state);
        assert_eq!(rows[0].level, 1);
        assert_eq!(rows[0].over_minutes, 0);
    }
}

#[test]
fn a_configured_limit_overrides_and_zero_stops_tracking() {
    let table: toml::Table = "[auto_merge.dwell]\nneeds_review = 5\ndirty = 0\nact = \"apply\"\n\
         max_escalations_per_pass = 2\n[auto_merge.dwell.dispatch]\nmacos = \"build.yml\"\n\
         evil = \"--repo=x.yml\"\npath = \"../build.yml\"\n"
        .parse()
        .unwrap();
    let policy = DwellPolicy::from_table(&table);
    assert_eq!(policy.limit(DwellState::NeedsReview), Some(5));
    assert_eq!(policy.limit(DwellState::Dirty), None);
    assert_eq!(
        policy.limit(DwellState::Held),
        Some(30),
        "unset keys keep defaults"
    );
    assert_eq!(policy.act, ActMode::Apply);
    assert_eq!(policy.max_escalations_per_pass, 2);
    assert_eq!(
        policy.dispatch.into_iter().collect::<Vec<_>>(),
        vec![("macos".to_owned(), "build.yml".to_owned())],
        "only bare workflow file names are accepted"
    );
    let state = tracked_once(
        &[sighting(1, "merge_state:DIRTY", HEAD, Some(600))],
        &DwellPolicy::from_table(&table),
    );
    assert!(
        state.entries.is_empty(),
        "an untracked state keeps no entry"
    );
}

#[test]
fn token_mapping_covers_the_steward_vocabulary() {
    // An approval voided by a base change waits on a re-review.
    assert_eq!(
        state_for_token("approval_void:base_changed", APPROVAL),
        Some(DwellState::NeedsReview)
    );
    assert_eq!(
        state_for_token("approval_excluded:stale_head", APPROVAL),
        Some(DwellState::NeedsReview)
    );
    assert_eq!(
        state_for_token("approval_excluded:unattributed", APPROVAL),
        None
    );
    assert_eq!(
        state_for_token("required_failed:review-verdict,macos", APPROVAL),
        Some(DwellState::RequiredFailed)
    );
    assert_eq!(state_for_token("merge_state:BEHIND", APPROVAL), None);
    assert_eq!(
        state_for_token("admission_pending", APPROVAL),
        Some(DwellState::ApprovedGreenNotQueued)
    );
    for untracked in [
        "queued",
        "admitted",
        "draft",
        "owner_decision",
        "excluded_label:superseded",
        "unmanaged",
    ] {
        assert_eq!(state_for_token(untracked, APPROVAL), None, "{untracked}");
    }
}

#[test]
fn entry_dates_from_github_since_and_resets_on_state_or_head_change() {
    let policy = DwellPolicy::default();
    let mut state = tracked_once(&[sighting(1, "required_pending", HEAD, Some(30))], &policy);
    assert_eq!(state.entries[&1].entered_at, minutes_before(now(), 30));
    // Same state, same head, later pass: the entry keeps its start.
    track(
        &mut state,
        &[sighting(1, "required_pending", HEAD, Some(1))],
        &observed(&[1]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now(),
    );
    assert_eq!(state.entries[&1].entered_at, minutes_before(now(), 30));
    // New state: a `since` before the last sighting is clamped to it.
    track(
        &mut state,
        &[sighting(1, "no_verdict", HEAD, Some(500))],
        &observed(&[1]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now(),
    );
    assert_eq!(state.entries[&1].state, DwellState::NeedsReview);
    assert_eq!(
        state.entries[&1].entered_at,
        now(),
        "never earlier than the previous sighting"
    );
    // New head, same state: a new entry.
    state.entries.get_mut(&1).unwrap().escalated_level = 2;
    track(
        &mut state,
        &[sighting(1, "no_verdict", HEAD2, None)],
        &observed(&[1]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now(),
    );
    assert_eq!(state.entries[&1].head, HEAD2);
    assert_eq!(state.entries[&1].escalated_level, 0);
    // A `since` in the future never dates an entry ahead of now.
    let future = Sighting {
        since: Some(now() + chrono::Duration::minutes(5)),
        ..sighting(2, "held", HEAD, None)
    };
    let state = tracked_once(&[future], &policy);
    assert_eq!(state.entries[&2].entered_at, now());
}

#[test]
fn entries_of_pull_requests_no_longer_open_are_pruned_but_their_acts_outlive_them() {
    let policy = DwellPolicy::default();
    let mut state = tracked_once(
        &[
            sighting(1, "held", HEAD, Some(40)),
            sighting(2, STACKED_BASE_MERGED, HEAD, Some(40)),
        ],
        &policy,
    );
    state.record_act(
        1,
        ActRecord {
            kind: ActKind::Readmit,
            state: DwellState::ApprovedGreenNotQueued,
            head: HEAD.to_owned(),
            status: "done".to_owned(),
            at: now(),
            detail: None,
            attempts: 1,
        },
    );
    // The stacked listing was not read: the stacked entry survives.
    track(
        &mut state,
        &[],
        &observed(&[]),
        &BTreeSet::new(),
        false,
        &policy,
        APPROVAL,
        now(),
    );
    assert!(!state.entries.contains_key(&1));
    assert!(
        state.acts.contains_key(&1),
        "a pull request missing from one pass keeps its acts, so none repeats"
    );
    assert!(state.entries.contains_key(&2));
    // It was read and #2 is gone: pruned.
    track(
        &mut state,
        &[],
        &observed(&[]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now(),
    );
    assert!(state.entries.is_empty());
    // Acts expire by age only.
    track(
        &mut state,
        &[],
        &observed(&[]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now() + chrono::Duration::days(15),
    );
    assert!(state.acts.is_empty());
}

#[test]
fn levels_double_and_cap() {
    assert_eq!(level(9, 10, 4), 0);
    assert_eq!(level(10, 10, 4), 1);
    assert_eq!(level(19, 10, 4), 1);
    assert_eq!(level(20, 10, 4), 2);
    assert_eq!(level(40, 10, 4), 3);
    assert_eq!(level(80, 10, 4), 4);
    assert_eq!(level(100_000, 10, 4), 4);
}

fn plan_for(state: &DwellRepoState, policy: &DwellPolicy) -> Vec<Planned> {
    plan(
        state,
        overdue(state, policy, now()),
        policy,
        now(),
        &|_, _| Actable::Yes,
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_mechanical_act_runs_once_per_state_and_head() {
    let policy = DwellPolicy {
        act: ActMode::Apply,
        ..DwellPolicy::default()
    };
    let mut state = tracked_once(
        &[sighting(
            1,
            "required_failed:review-verdict",
            HEAD,
            Some(12),
        )],
        &policy,
    );
    let first = plan_for(&state, &policy);
    assert_eq!(
        first[0].act,
        Disposition::Act {
            kind: ActKind::RerunVerdictCheck
        }
    );
    assert!(
        !first[0].escalate,
        "the act gets its chance before an escalation"
    );
    state.record_act(
        1,
        ActRecord {
            kind: ActKind::RerunVerdictCheck,
            state: DwellState::StaleVerdictCheck,
            head: HEAD.to_owned(),
            status: "intent".to_owned(),
            at: now(),
            detail: None,
            attempts: 1,
        },
    );
    let again = plan_for(&state, &policy);
    assert_eq!(
        again[0].act,
        Disposition::Acted {
            kind: ActKind::RerunVerdictCheck,
            status: "intent".to_owned()
        }
    );
    // The state leaves and comes back at the same head: still acted.
    track(
        &mut state,
        &[sighting(1, "required_pending", HEAD, Some(1))],
        &observed(&[1]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now(),
    );
    track(
        &mut state,
        &[sighting(
            1,
            "required_failed:review-verdict",
            HEAD,
            Some(30),
        )],
        &observed(&[1]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        minutes_before(now(), -30),
    );
    let back = plan(
        &state,
        overdue(&state, &policy, minutes_before(now(), -60)),
        &policy,
        now(),
        &|_, _| Actable::Yes,
    );
    assert!(
        matches!(back[0].act, Disposition::Acted { .. }),
        "{:?}",
        back[0].act
    );
    assert!(
        back[0].escalate,
        "an act that did not clear the state escalates"
    );
    // A new head may be acted on again.
    track(
        &mut state,
        &[sighting(
            1,
            "required_failed:review-verdict",
            HEAD2,
            Some(30),
        )],
        &observed(&[1]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now(),
    );
    let fresh = plan(
        &state,
        overdue(&state, &policy, minutes_before(now(), -30)),
        &policy,
        now(),
        &|_, _| Actable::Yes,
    );
    assert_eq!(
        fresh[0].act,
        Disposition::Act {
            kind: ActKind::RerunVerdictCheck
        }
    );
}

#[test]
fn shadow_mode_reports_would_act_and_escalates_but_still_readmits() {
    let policy = DwellPolicy::default();
    let state = tracked_once(
        &[
            sighting(1, "required_never_ran:macos", HEAD, Some(31)),
            sighting(2, "ready", HEAD, Some(6)),
        ],
        &policy,
    );
    let planned = plan_for(&state, &policy);
    let by = |n: u64| planned.iter().find(|row| row.overdue.number == n).unwrap();
    assert_eq!(
        by(1).act,
        Disposition::WouldAct {
            kind: ActKind::DispatchNeverRan
        }
    );
    assert!(by(1).escalate);
    assert_eq!(
        by(2).act,
        Disposition::Act {
            kind: ActKind::Readmit
        },
        "re-admit writes nothing to GitHub"
    );
    let off = DwellPolicy {
        act: ActMode::Off,
        ..DwellPolicy::default()
    };
    assert!(
        plan_for(&state, &off)
            .iter()
            .all(|row| row.act == Disposition::NoAct)
    );
}

#[test]
fn a_never_ran_check_without_a_mapped_workflow_escalates() {
    let policy = DwellPolicy {
        act: ActMode::Apply,
        ..DwellPolicy::default()
    };
    let state = tracked_once(
        &[sighting(1, "required_never_ran:linux", HEAD, Some(31))],
        &policy,
    );
    let planned = plan(
        &state,
        overdue(&state, &policy, now()),
        &policy,
        now(),
        &|_, _| Actable::Never,
    );
    assert_eq!(planned[0].act, Disposition::NoAct);
    assert!(planned[0].escalate);
}

#[test]
fn never_acts_on_dirty_held_or_carried_unapproved_heads() {
    let policy = DwellPolicy {
        act: ActMode::Apply,
        ..DwellPolicy::default()
    };
    for state_kind in [
        DwellState::Dirty,
        DwellState::Held,
        DwellState::CarriesUnapproved,
        DwellState::NeedsReview,
        DwellState::RequiredFailed,
        DwellState::RequiredPending,
        DwellState::RequiredFailedInfra,
    ] {
        assert_eq!(state_kind.mechanical_act(), None, "{state_kind:?}");
        let state = tracked_once(
            &[sighting(1, token(state_kind), HEAD, Some(10_000))],
            &policy,
        );
        let planned = plan_for(&state, &policy);
        assert_eq!(planned[0].act, Disposition::NoAct, "{state_kind:?}");
        assert!(planned[0].escalate, "{state_kind:?} escalates instead");
    }
}

#[test]
fn no_act_kind_arms_merges_enqueues_or_updates() {
    for state in DwellState::ALL {
        if let Some(kind) = state.mechanical_act() {
            let name = kind.key();
            for forbidden in ["arm", "merge", "enqueue", "update", "rebase", "approve"] {
                assert!(!name.contains(forbidden), "{name} must not {forbidden}");
            }
        }
    }
}

#[test]
fn each_escalation_level_fires_once_and_the_cap_defers_the_rest_worst_first() {
    let policy = DwellPolicy {
        max_escalations_per_pass: 1,
        ..DwellPolicy::default()
    };
    let mut state = tracked_once(
        &[
            sighting(1, "held", HEAD, Some(31)),
            sighting(2, "merge_state:DIRTY", HEAD, Some(200)),
        ],
        &policy,
    );
    let planned = plan_for(&state, &policy);
    assert_eq!(
        planned[0].overdue.number, 2,
        "the most overdue relative to its limit goes first"
    );
    assert!(planned[0].escalate && !planned[1].escalate);
    assert!(planned[1].escalation_deferred);
    mark_escalated(&mut state, &planned);
    let next = plan_for(&state, &policy);
    let by = |n: u64| next.iter().find(|row| row.overdue.number == n).unwrap();
    assert!(
        !by(2).escalate,
        "level {} already sent",
        by(2).overdue.level
    );
    assert!(by(1).escalate, "the deferred escalation goes out next pass");
    mark_escalated(&mut state, &next);
    assert!(
        plan_for(&state, &policy).iter().all(|row| !row.escalate),
        "nothing new to say"
    );
    // Twice the limit: the next level.
    let roomy = DwellPolicy {
        max_escalations_per_pass: 5,
        ..policy
    };
    let later = plan(
        &state,
        overdue(&state, &roomy, minutes_before(now(), -40)),
        &roomy,
        now(),
        &|_, _| Actable::Yes,
    );
    assert!(
        later
            .iter()
            .find(|row| row.overdue.number == 1)
            .unwrap()
            .escalate
    );
}

#[test]
fn a_thousand_overdue_pull_requests_escalate_at_most_the_cap_per_pass() {
    let policy = DwellPolicy::default();
    let sightings: Vec<Sighting> = (1..=1_000)
        .map(|n| sighting(n, "held", HEAD, Some(45)))
        .collect();
    let state = tracked_once(&sightings, &policy);
    let planned = plan_for(&state, &policy);
    assert_eq!(
        planned.iter().filter(|row| row.escalate).count(),
        policy.max_escalations_per_pass
    );
}

fn record(status: &str, attempts: u32, at: DateTime<Utc>) -> ActRecord {
    ActRecord {
        kind: ActKind::DispatchNeverRan,
        state: DwellState::RequiredNeverRan,
        head: HEAD.to_owned(),
        status: status.to_owned(),
        at,
        detail: None,
        attempts,
    }
}

#[test]
fn an_act_missing_from_one_pass_and_back_is_not_repeated() {
    let policy = DwellPolicy {
        act: ActMode::Apply,
        ..DwellPolicy::default()
    };
    let mut state = tracked_once(
        &[sighting(1, "required_never_ran:macos", HEAD, Some(31))],
        &policy,
    );
    state.record_act(1, record("done", 1, now()));
    // A pass that did not see #1 (census fallback, closed and reopened).
    track(
        &mut state,
        &[],
        &observed(&[]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now(),
    );
    track(
        &mut state,
        &[sighting(1, "required_never_ran:macos", HEAD, Some(31))],
        &observed(&[1]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now(),
    );
    let later = now() + chrono::Duration::minutes(40);
    let planned = plan(
        &state,
        overdue(&state, &policy, later),
        &policy,
        later,
        &|_, _| Actable::Yes,
    );
    assert!(
        matches!(planned[0].act, Disposition::Acted { .. }),
        "{:?}",
        planned[0].act
    );
}

#[test]
fn a_state_entered_after_an_untracked_one_is_not_dated_before_it() {
    let policy = DwellPolicy::default();
    let mut state = tracked_once(&[sighting(1, "queued", HEAD, None)], &policy);
    assert!(state.entries.is_empty(), "queued is untracked");
    // Ejected: back as DIRTY with a since from the head commit, days old.
    track(
        &mut state,
        &[sighting(1, "merge_state:DIRTY", HEAD, Some(5_000))],
        &observed(&[1]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now() + chrono::Duration::minutes(2),
    );
    let entry = &state.entries[&1];
    assert_eq!(
        entry.entered_at,
        now(),
        "dated by the pass that last saw it queued"
    );
}

#[test]
fn the_first_pass_seeds_the_backlog_without_escalating_it() {
    let policy = DwellPolicy::default();
    let mut state = DwellRepoState::default();
    track(
        &mut state,
        &[
            sighting(1, "merge_state:DIRTY", HEAD, Some(500)),
            sighting(2, "held", HEAD, Some(35)),
        ],
        &observed(&[1, 2]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now(),
    );
    let planned = plan(
        &state,
        overdue(&state, &policy, now()),
        &policy,
        now(),
        &|_, _| Actable::Yes,
    );
    assert_eq!(planned.len(), 2, "still reported overdue");
    assert!(planned.iter().all(|row| !row.escalate), "{planned:?}");
    // A later level still escalates.
    let later = now() + chrono::Duration::minutes(30);
    let planned = plan(
        &state,
        overdue(&state, &policy, later),
        &policy,
        later,
        &|_, _| Actable::Yes,
    );
    assert!(
        planned
            .iter()
            .find(|row| row.overdue.number == 2)
            .unwrap()
            .escalate
    );
}

#[test]
fn a_failed_or_deferred_act_is_retried_with_backoff_at_most_three_times() {
    let at = now();
    assert!(!retry_due(
        &record("done", 1, at),
        at + chrono::Duration::days(1)
    ));
    assert!(
        !retry_due(&record("intent", 1, at), at + chrono::Duration::days(1)),
        "an intent may have happened"
    );
    assert!(!retry_due(
        &record("deferred", 1, at),
        at + chrono::Duration::minutes(14)
    ));
    assert!(retry_due(
        &record("deferred", 1, at),
        at + chrono::Duration::minutes(15)
    ));
    assert!(!retry_due(
        &record("refused", 2, at),
        at + chrono::Duration::minutes(29)
    ));
    assert!(retry_due(
        &record("refused", 2, at),
        at + chrono::Duration::minutes(30)
    ));
    assert!(
        !retry_due(&record("failed", 1, at), at + chrono::Duration::days(1)),
        "an attempted side effect may have reached GitHub: never again at this head"
    );
    assert!(!retry_due(
        &record("refused", MAX_ATTEMPTS, at),
        at + chrono::Duration::days(1)
    ));
    // Attempts count intents after a retryable outcome.
    let mut state = DwellRepoState::default();
    state.record_act(1, record("intent", 1, at));
    state.record_act(1, record("deferred", 1, at));
    state.record_act(1, record("intent", 1, at));
    assert_eq!(
        state
            .acted(1, DwellState::RequiredNeverRan, HEAD)
            .unwrap()
            .attempts,
        2
    );
    state.record_act(1, record("failed", 1, at));
    assert_eq!(
        state
            .acted(1, DwellState::RequiredNeverRan, HEAD)
            .unwrap()
            .attempts,
        2
    );
}

#[test]
fn an_act_that_cannot_run_now_waits_and_the_cap_bounds_acts_per_pass() {
    let policy = DwellPolicy {
        act: ActMode::Apply,
        max_acts_per_pass: 2,
        ..DwellPolicy::default()
    };
    let sightings: Vec<Sighting> = (1..=5)
        .map(|n| sighting(n, "required_never_ran:macos", HEAD, Some(31)))
        .collect();
    let state = tracked_once(&sightings, &policy);
    let planned = plan(
        &state,
        overdue(&state, &policy, now()),
        &policy,
        now(),
        &|row, _| {
            if row.number == 5 {
                Actable::NotNow("deferred".to_owned())
            } else {
                Actable::Yes
            }
        },
    );
    let acts = planned
        .iter()
        .filter(|row| matches!(row.act, Disposition::Act { .. }))
        .count();
    assert_eq!(acts, 2, "{planned:?}");
    let later: Vec<&Planned> = planned
        .iter()
        .filter(|row| matches!(row.act, Disposition::ActLater { .. }))
        .collect();
    assert_eq!(later.len(), 3);
    assert!(
        later.iter().all(|row| !row.escalate),
        "waiting acts do not escalate at level 1"
    );
}

#[test]
fn a_pass_with_no_open_pull_requests_does_not_reseed_the_next() {
    let policy = DwellPolicy::default();
    let mut state = DwellRepoState::default();
    track(
        &mut state,
        &[],
        &observed(&[]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now(),
    );
    track(
        &mut state,
        &[sighting(1, "held", HEAD, Some(35))],
        &observed(&[1]),
        &BTreeSet::new(),
        true,
        &policy,
        APPROVAL,
        now(),
    );
    let planned = plan(
        &state,
        overdue(&state, &policy, now()),
        &policy,
        now(),
        &|_, _| Actable::Yes,
    );
    assert!(planned[0].escalate, "only the very first pass seeds");
}

#[test]
fn a_blocked_act_retries_after_a_day_doubling_and_at_most_max_attempts() {
    let record = |attempts: u32| ActRecord {
        kind: ActKind::RetriggerStaleRef,
        state: DwellState::StaleMergeRef,
        head: HEAD.to_owned(),
        status: "blocked".to_owned(),
        at: now(),
        detail: Some("retrigger_blocked: guard".to_owned()),
        attempts,
    };
    let later = |minutes: i64| now() + Duration::minutes(minutes);
    assert!(!retry_due(&record(1), later(60)), "not within the hour");
    assert!(!retry_due(&record(1), later(23 * 60)), "not within the day");
    assert!(retry_due(&record(1), later(24 * 60)));
    assert!(
        !retry_due(&record(2), later(24 * 60)),
        "the second wait doubles"
    );
    assert!(retry_due(&record(2), later(48 * 60)));
    assert!(!retry_due(&record(MAX_ATTEMPTS), later(100 * 24 * 60)));
}

#[test]
fn a_stale_merge_ref_token_has_the_retrigger_act() {
    assert_eq!(
        state_for_token("stale_merge_ref:macos,linux", APPROVAL),
        Some(DwellState::StaleMergeRef)
    );
    assert_eq!(
        DwellState::StaleMergeRef.mechanical_act(),
        Some(ActKind::RetriggerStaleRef)
    );
    assert!(ActKind::RetriggerStaleRef.writes_github());
}

fn stuck(number: u64, head: &str, token: &str) -> StuckCandidate {
    StuckCandidate {
        number,
        head: head.to_owned(),
        token: token.to_owned(),
        placeholder_token: false,
    }
}

/// One pass of `track_stuck` `minutes` after `now()`: `candidates`, plus
/// `others` seen open (number, head) but not judged eligible.
fn stuck_pass(
    state: &mut DwellRepoState,
    candidates: &[StuckCandidate],
    others: &[(u64, &str)],
    minutes: i64,
    apply: bool,
) -> Vec<Planned> {
    let open = candidates
        .iter()
        .map(|candidate| (candidate.number, candidate.head.clone()))
        .chain(
            others
                .iter()
                .map(|(number, head)| (*number, (*head).to_owned())),
        )
        .collect();
    let rows = track_stuck(
        state,
        candidates,
        &open,
        &DwellPolicy::default(),
        now() + Duration::minutes(minutes),
    );
    if apply {
        mark_stuck_alerted(state, &rows);
    }
    rows
}

/// What each minute's pass is for one pull request at `head`.
#[derive(Clone, Copy)]
enum Seen {
    /// Judged eligible.
    Eligible,
    /// Seen open at the head, not judged eligible (a verdict re-run, queued,
    /// held, red, a deferred row).
    Open,
    /// No pass ran.
    Missing,
}

/// Run `schedule` (minute, what the pass sees) for `pr`; the minutes at which
/// it escalated.
fn escalations(
    state: &mut DwellRepoState,
    pr: &StuckCandidate,
    schedule: &[(i64, Seen)],
) -> Vec<i64> {
    let mut out = Vec::new();
    for (minute, seen) in schedule {
        let rows = match seen {
            Seen::Eligible => stuck_pass(state, std::slice::from_ref(pr), &[], *minute, true),
            Seen::Open => stuck_pass(state, &[], &[(pr.number, pr.head.as_str())], *minute, true),
            Seen::Missing => continue,
        };
        if rows.iter().any(|row| row.escalate) {
            out.push(*minute);
        }
    }
    out
}

/// A named pass schedule and the minutes it must escalate at.
type Schedule<'a> = (&'a str, Vec<(i64, Seen)>, Vec<i64>);

/// Five-minute passes from `from` to `to`, each `seen`.
fn every_five(from: i64, to: i64, seen: Seen) -> Vec<(i64, Seen)> {
    (from..=to)
        .step_by(5)
        .map(|minute| (minute, seen))
        .collect()
}

/// An eligible pull request is silent under 60 minutes, escalates exactly
/// once at 60, and is still reported (not escalating) on later eligible
/// passes. The row names why the steward has not merged it.
#[test]
fn an_approved_green_pull_request_stuck_past_the_bound_alerts_once() {
    let mut state = established();
    let pr = stuck(69, HEAD, "merge_state:UNSTABLE");
    assert!(escalations(&mut state, &pr, &every_five(0, 55, Seen::Eligible)).is_empty());
    let first = stuck_pass(&mut state, std::slice::from_ref(&pr), &[], 60, true);
    assert_eq!(first.len(), 1);
    assert!(first[0].escalate);
    assert_eq!(first[0].overdue.state, DwellState::ApprovedStuck);
    assert_eq!(first[0].overdue.token, "merge_state:UNSTABLE");
    assert_eq!(first[0].overdue.age_minutes, 60);
    assert_eq!(first[0].act, Disposition::NoAct);
    for minute in (65..=600).step_by(5) {
        let rows = stuck_pass(&mut state, std::slice::from_ref(&pr), &[], minute, true);
        assert_eq!(rows.len(), 1, "{minute}");
        assert!(!rows[0].escalate, "alerted once, not again at {minute}");
    }
}

/// Nothing a pass sees at the same head resets `first_eligible_at`: a blip
/// (one pass not eligible), a deferral, missing ticks, and a blip followed by
/// deferral and a missing tick (the timings from review) all alert on the
/// first eligible pass at or past 60 minutes from first eligibility. A pass
/// that is not eligible never alerts.
#[test]
fn blips_deferrals_and_missing_ticks_never_reset_the_clock() {
    let pr = stuck(12, HEAD, "merge_state:UNSTABLE");
    let schedules: Vec<Schedule<'_>> = vec![
        (
            "blip",
            [
                every_five(0, 45, Seen::Eligible),
                vec![(50, Seen::Open)],
                every_five(55, 70, Seen::Eligible),
            ]
            .concat(),
            vec![60],
        ),
        (
            "deferral across the bound",
            [
                every_five(0, 50, Seen::Eligible),
                every_five(55, 75, Seen::Open),
                vec![(80, Seen::Eligible)],
            ]
            .concat(),
            vec![80],
        ),
        (
            "missing ticks",
            vec![
                (0, Seen::Eligible),
                (30, Seen::Missing),
                (61, Seen::Eligible),
            ],
            vec![61],
        ),
        (
            "blip then deferral and a missing tick",
            [
                every_five(0, 45, Seen::Eligible),
                vec![
                    (50, Seen::Open),
                    (55, Seen::Open),
                    (60, Seen::Missing),
                    (65, Seen::Open),
                    (70, Seen::Eligible),
                ],
            ]
            .concat(),
            vec![70],
        ),
    ];
    for (name, schedule, expected) in schedules {
        let mut state = established();
        assert_eq!(escalations(&mut state, &pr, &schedule), expected, "{name}");
        let entry = &state.stuck_approved[&12];
        assert_eq!(entry.first_eligible_at, now(), "{name}: never reset");
    }
}

/// A second stuck episode at the same head (eligible, then a long stretch
/// queued or held, then eligible again) does not alert again: one alert per
/// (pull request, head). A new head is a new key with its own clock and
/// alert; a pull request no longer seen is dropped.
#[test]
fn one_alert_per_pull_request_and_head() {
    let mut state = established();
    let pr = stuck(7, HEAD, "excluded_label:x");
    let first = [
        every_five(0, 60, Seen::Eligible),
        every_five(65, 180, Seen::Open),
        every_five(185, 300, Seen::Eligible),
    ]
    .concat();
    assert_eq!(escalations(&mut state, &pr, &first), vec![60]);
    let moved = stuck(7, HEAD2, "excluded_label:x");
    assert_eq!(
        escalations(&mut state, &moved, &every_five(305, 365, Seen::Eligible)),
        vec![365]
    );
    stuck_pass(&mut state, &[], &[], 370, true);
    assert!(state.stuck_approved.is_empty());
}

/// A deferred row's placeholder token never replaces the last real reason.
#[test]
fn a_placeholder_token_keeps_the_last_real_reason() {
    let mut state = established();
    let pr = stuck(9, HEAD, "merge_state:UNSTABLE");
    stuck_pass(&mut state, std::slice::from_ref(&pr), &[], 0, true);
    let deferred = StuckCandidate {
        token: "deferred_pass_budget".to_owned(),
        placeholder_token: true,
        ..pr
    };
    let rows = stuck_pass(&mut state, &[deferred], &[], 60, true);
    assert_eq!(rows[0].overdue.token, "merge_state:UNSTABLE");
}

/// A pass that does not apply (a dry run, `shipyard wait`) reports the
/// alert but never consumes it: the next apply pass still escalates.
#[test]
fn a_dry_run_does_not_use_up_the_alert() {
    let mut state = established();
    let pr = [stuck(9, HEAD, "merge_state:UNSTABLE")];
    stuck_pass(&mut state, &pr, &[], 0, true);
    assert!(stuck_pass(&mut state, &pr, &[], 61, false)[0].escalate);
    assert!(
        stuck_pass(&mut state, &pr, &[], 62, true)[0].escalate,
        "the dry run did not consume it"
    );
    assert!(!stuck_pass(&mut state, &pr, &[], 63, true)[0].escalate);
}

/// `[auto_merge.dwell] approved_stuck = 0` turns the alarm off; another
/// value moves the bound.
#[test]
fn the_stuck_bound_is_configurable_and_zero_disables_it() {
    let policy = |raw: &str| DwellPolicy::from_table(&raw.parse::<toml::Table>().expect("toml"));
    assert_eq!(
        DwellPolicy::default().limit(DwellState::ApprovedStuck),
        Some(60)
    );
    assert_eq!(
        policy("[auto_merge.dwell]\napproved_stuck = 30\n").limit(DwellState::ApprovedStuck),
        Some(30)
    );
    let off = policy("[auto_merge.dwell]\napproved_stuck = 0\n");
    let mut state = established();
    let pr = [stuck(9, HEAD, "merge_state:UNSTABLE")];
    let open: BTreeMap<u64, String> = [(9, HEAD.to_owned())].into_iter().collect();
    track_stuck(&mut state, &pr, &open, &off, now());
    assert!(track_stuck(&mut state, &pr, &open, &off, now() + Duration::minutes(600)).is_empty());
}

/// A ledger written by the previous version (`since`, no other new field)
/// still reads.
#[test]
fn an_older_stuck_entry_still_reads() {
    let entry: StuckApproved = serde_json::from_str(
        r#"{"head":"aaaa","token":"held","since":"2026-11-09T12:00:00Z","alerted":true,"ineligible_since":null}"#,
    )
    .expect("entry");
    assert_eq!(entry.first_eligible_at, now());
    assert!(entry.alerted);
}

#[test]
fn base_stalled_names_longest_continuously_eligible_pr() {
    let mut state = DwellRepoState::default();
    let policy = DwellPolicy::default();
    let first = [BaseStalledCandidate {
        number: 100,
        head: HEAD.to_owned(),
    }];
    track_base_stalled(&mut state, Some(HEAD), &first, None, &policy, now());
    let both = [
        BaseStalledCandidate {
            number: 50,
            head: HEAD.to_owned(),
        },
        BaseStalledCandidate {
            number: 100,
            head: HEAD.to_owned(),
        },
    ];
    let rows = track_base_stalled(
        &mut state,
        Some(HEAD),
        &both,
        None,
        &policy,
        now() + chrono::Duration::minutes(61),
    );
    assert_eq!(rows[0].overdue.number, 100);
    // State survives a restart; a new head starts a fresh eligibility interval.
    let encoded = serde_json::to_string(&state).unwrap();
    let mut restored: DwellRepoState = serde_json::from_str(&encoded).unwrap();
    let changed = [
        BaseStalledCandidate {
            number: 50,
            head: HEAD.to_owned(),
        },
        BaseStalledCandidate {
            number: 100,
            head: "new-head".to_owned(),
        },
    ];
    let rows = track_base_stalled(
        &mut restored,
        Some(HEAD),
        &changed,
        None,
        &policy,
        now() + chrono::Duration::minutes(62),
    );
    assert_eq!(rows[0].overdue.number, 50);
    // Leaving and rejoining resets eligibility too.
    track_base_stalled(
        &mut restored,
        Some(HEAD),
        &changed[1..],
        None,
        &policy,
        now() + chrono::Duration::minutes(63),
    );
    let rows = track_base_stalled(
        &mut restored,
        Some(HEAD),
        &changed,
        None,
        &policy,
        now() + chrono::Duration::minutes(64),
    );
    assert_eq!(rows[0].overdue.number, 100);
}

#[test]
fn base_stalled_is_quiet_under_bound_then_alerts_once() {
    let mut state = established();
    let candidate = [BaseStalledCandidate {
        number: 7,
        head: HEAD.to_owned(),
    }];
    let policy = DwellPolicy::default();
    assert!(
        track_base_stalled(&mut state, Some(HEAD), &candidate, None, &policy, now()).is_empty()
    );
    let rows = track_base_stalled(
        &mut state,
        Some(HEAD),
        &candidate,
        Some("build".into()),
        &policy,
        now() + Duration::minutes(60),
    );
    assert!(rows[0].escalate);
    mark_base_stalled_alerted(&mut state, &rows);
    assert!(
        !track_base_stalled(
            &mut state,
            Some(HEAD),
            &candidate,
            None,
            &policy,
            now() + Duration::minutes(61)
        )[0]
        .escalate
    );
}

#[test]
fn base_stalled_rearms_only_after_head_change_and_zero_disables() {
    let mut state = established();
    let candidate = [BaseStalledCandidate {
        number: 7,
        head: HEAD.to_owned(),
    }];
    let policy = DwellPolicy::default();
    track_base_stalled(&mut state, Some(HEAD), &candidate, None, &policy, now());
    track_base_stalled(
        &mut state,
        Some(HEAD2),
        &candidate,
        None,
        &policy,
        now() + Duration::minutes(60),
    );
    assert!(
        track_base_stalled(
            &mut state,
            Some(HEAD2),
            &candidate,
            None,
            &policy,
            now() + Duration::minutes(120)
        )[0]
        .escalate
    );
    let off = DwellPolicy::from_table(&"[auto_merge.dwell]\nbase_stalled = 0".parse().unwrap());
    assert!(
        track_base_stalled(
            &mut state,
            Some(HEAD2),
            &candidate,
            None,
            &off,
            now() + Duration::minutes(600)
        )
        .is_empty()
    );
}

#[test]
fn base_stalled_disabled_keeps_no_clock_and_reenabling_starts_fresh() {
    let mut state = established();
    let candidate = [BaseStalledCandidate {
        number: 7,
        head: HEAD.to_owned(),
    }];
    let off = DwellPolicy::from_table(&"[auto_merge.dwell]\nbase_stalled = 0".parse().unwrap());
    assert!(track_base_stalled(&mut state, Some(HEAD), &candidate, None, &off, now()).is_empty());
    assert!(state.base_stalled.is_empty());
    let on = DwellPolicy::default();
    // Re-enabled 10 hours later: the clock starts now, so nothing is overdue.
    assert!(
        track_base_stalled(
            &mut state,
            Some(HEAD),
            &candidate,
            None,
            &on,
            now() + Duration::minutes(600)
        )
        .is_empty()
    );
}

#[test]
fn base_stalled_clock_restarts_after_an_empty_eligible_observation() {
    let mut state = established();
    let candidate = [BaseStalledCandidate {
        number: 7,
        head: HEAD.to_owned(),
    }];
    let policy = DwellPolicy::default();
    track_base_stalled(&mut state, Some(HEAD), &candidate, None, &policy, now());
    assert!(
        track_base_stalled(
            &mut state,
            Some(HEAD),
            &[],
            None,
            &policy,
            now() + Duration::minutes(30)
        )
        .is_empty()
    );
    assert!(state.base_stalled.is_empty());
    assert!(
        track_base_stalled(
            &mut state,
            Some(HEAD),
            &candidate,
            None,
            &policy,
            now() + Duration::minutes(90)
        )
        .is_empty()
    );
}
