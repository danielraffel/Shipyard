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
    let mut state = DwellRepoState::default();
    state.seen.insert(0, minutes_before(now(), 1));
    state
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
fn every_state_is_overdue_exactly_at_its_limit() {
    let policy = DwellPolicy::default();
    for state in DwellState::ALL {
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
        &record("failed", 2, at),
        at + chrono::Duration::minutes(29)
    ));
    assert!(retry_due(
        &record("failed", 2, at),
        at + chrono::Duration::minutes(30)
    ));
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
