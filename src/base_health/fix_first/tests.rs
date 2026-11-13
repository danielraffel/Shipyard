use serde_json::json;

use super::*;

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("fixture time")
        .with_timezone(&Utc)
}

fn run(id: u64, conclusion: &str, completed: &str) -> TipRun {
    TipRun {
        id,
        conclusion: conclusion.to_owned(),
        completed_at: Some(at(completed)),
    }
}

fn candidate(number: u64, approved: Option<&str>, green: Option<&str>) -> FixCandidate {
    FixCandidate {
        number,
        draft: false,
        labels: Vec::new(),
        approved_at: approved.map(at),
        context_green_at: green.map(at),
        rode_failing_group: false,
    }
}

fn load(text: &str) -> Vec<ReplayEpisode> {
    serde_json::from_str(text).expect("fixture parses")
}

#[test]
fn the_newest_tip_verdict_decides_and_red_since_is_the_first_failure_of_the_run() {
    let runs = [
        run(1, "success", "2026-11-13T09:00:00Z"),
        run(2, "failure", "2026-11-13T10:00:00Z"),
        run(3, "cancelled", "2026-11-13T10:10:00Z"),
        run(4, "failure", "2026-11-13T10:20:00Z"),
    ];
    assert_eq!(judge_tip(&runs), (true, Some(at("2026-11-13T10:00:00Z"))));
    let mut green = runs.to_vec();
    green.push(run(5, "success", "2026-11-13T10:30:00Z"));
    assert_eq!(judge_tip(&green), (false, None), "a green tip ends it");
    assert_eq!(judge_tip(&[]), (false, None), "no verdict is not red");
}

#[test]
fn the_tip_reading_keeps_only_completed_runs_of_the_pinned_app() {
    let value = json!({"check_runs": [
        {"id": 9, "name": "provenance-verify", "status": "in_progress", "conclusion": null,
         "head_sha": "abc", "app": {"id": 15368}},
        {"id": 8, "name": "provenance-verify", "status": "completed", "conclusion": "failure",
         "completed_at": "2026-11-13T10:32:36Z", "head_sha": "abc", "app": {"id": 1}},
        {"id": 7, "name": "provenance-verify", "status": "completed", "conclusion": "success",
         "completed_at": "2026-11-13T09:53:01Z", "head_sha": "abc", "app": {"id": 15368}},
    ]});
    let pinned = parse_tip(&value, "provenance-verify", Some(15368));
    assert!(!pinned.red, "another app's failure never reddens the tip");
    let any = parse_tip(&value, "provenance-verify", None);
    assert!(any.red);
    assert_eq!(any.sha.as_deref(), Some("abc"));
    assert_eq!(any.red_since, Some(at("2026-11-13T10:32:36Z")));
}

#[test]
fn the_tip_path_encodes_the_context() {
    assert_eq!(
        tip_check_path("o/r", "main", "Enforce version & skill sync"),
        "repos/o/r/commits/main/check-runs?check_name=Enforce%20version%20%26%20skill%20sync&filter=all&per_page=30"
    );
}

#[test]
fn a_fix_needs_an_approved_head_that_passed_after_the_tip_went_red() {
    let since = at("2026-11-13T10:32:36Z");
    let candidates = [
        candidate(
            1,
            Some("2026-11-13T10:41:00Z"),
            Some("2026-11-13T10:35:00Z"),
        ),
        candidate(
            2,
            Some("2026-11-13T10:03:00Z"),
            Some("2026-11-13T09:58:00Z"),
        ),
        candidate(3, None, Some("2026-11-13T10:40:00Z")),
        candidate(4, Some("2026-11-13T10:00:00Z"), None),
        FixCandidate {
            draft: true,
            ..candidate(
                5,
                Some("2026-11-13T10:00:00Z"),
                Some("2026-11-13T10:40:00Z"),
            )
        },
        FixCandidate {
            rode_failing_group: true,
            ..candidate(
                6,
                Some("2026-11-13T10:00:00Z"),
                Some("2026-11-13T10:40:00Z"),
            )
        },
    ];
    let fixes = green_head_fixes(&candidates, since);
    assert_eq!(
        fixes.iter().map(|fix| fix.pr).collect::<Vec<_>>(),
        vec![1],
        "green before the break, unapproved, not green, draft and rode-a-failing-group all fail"
    );
}

#[test]
fn two_candidates_are_ambiguous_unless_exactly_one_carries_a_fix_label() {
    let since = at("2026-11-13T10:00:00Z");
    let mut candidates = vec![
        candidate(
            30,
            Some("2026-11-13T10:50:00Z"),
            Some("2026-11-13T10:40:00Z"),
        ),
        candidate(
            20,
            Some("2026-11-13T10:45:00Z"),
            Some("2026-11-13T10:40:00Z"),
        ),
    ];
    let fixes = green_head_fixes(&candidates, since);
    assert_eq!(
        pick(&fixes),
        Pick::Ambiguous { prs: vec![20, 30] },
        "never a silent guess between rivals"
    );
    candidates[0].labels = vec![FIX_LABEL.to_owned()];
    let picked = pick(&green_head_fixes(&candidates, since));
    assert!(
        matches!(picked, Pick::One(ref fix) if fix.pr == 30),
        "{picked:?}"
    );
    candidates[0].labels = vec!["fixes-main".to_owned()];
    let picked = pick(&green_head_fixes(&candidates, since));
    assert!(
        matches!(picked, Pick::One(ref fix) if fix.pr == 30),
        "fixes-main too"
    );
    candidates[1].labels = vec![FIX_LABEL.to_owned()];
    assert!(matches!(
        pick(&green_head_fixes(&candidates, since)),
        Pick::Ambiguous { .. }
    ));
    assert_eq!(pick(&[]), Pick::None);
}

#[test]
fn an_ambiguous_replay_names_no_fix_and_holds_neither_candidate() {
    let episode = ReplayEpisode {
        key: "o/r@main".to_owned(),
        context: "ctx".to_owned(),
        started_at: at("2026-11-13T10:00:00Z"),
        ended_at: None,
        max_streak: 2,
        would_hold: Vec::new(),
        decided_at: at("2026-11-13T11:00:00Z"),
        red_everywhere_since: None,
        tip_runs: vec![run(1, "failure", "2026-11-13T10:00:00Z")],
        prs: vec![
            candidate(
                1,
                Some("2026-11-13T10:30:00Z"),
                Some("2026-11-13T10:20:00Z"),
            ),
            candidate(
                2,
                Some("2026-11-13T10:31:00Z"),
                Some("2026-11-13T10:21:00Z"),
            ),
            candidate(3, Some("2026-11-13T10:32:00Z"), None),
        ],
        actual_fix: Some(1),
        note: String::new(),
    };
    let outcome = replay(&episode);
    assert!(outcome.fires);
    assert_eq!(outcome.fix, None);
    assert_eq!(outcome.ambiguous, vec![1, 2]);
    assert_eq!(outcome.held, vec![3]);
    assert!(!outcome.actual_fix_held);
}

#[test]
fn a_single_failure_without_the_streak_does_nothing() {
    let episode = ReplayEpisode {
        key: "o/r@main".to_owned(),
        context: "ctx".to_owned(),
        started_at: at("2026-11-13T10:00:00Z"),
        ended_at: None,
        max_streak: 1,
        would_hold: Vec::new(),
        decided_at: at("2026-11-13T11:00:00Z"),
        red_everywhere_since: None,
        tip_runs: vec![run(1, "failure", "2026-11-13T10:00:00Z")],
        prs: vec![
            candidate(
                1,
                Some("2026-11-13T10:30:00Z"),
                Some("2026-11-13T10:20:00Z"),
            ),
            candidate(2, Some("2026-11-13T10:30:00Z"), None),
        ],
        actual_fix: Some(1),
        note: String::new(),
    };
    let outcome = replay(&episode);
    assert!(!outcome.fires);
    assert_eq!(outcome.fix, None);
    assert!(outcome.held.is_empty());
}

/// Vellum, 2026-11-13: `provenance-verify` red on the tip from 10:32. At
/// 10:37 #408 (green only before the break) formed a doomed group; at 10:46
/// #409 (green after the break, approved 10:41) went to the back. Fix-first
/// holds #408 and names #409.
#[test]
fn vellum_replay_jumps_409_and_holds_408() {
    let episodes = load(include_str!(
        "../../../tests/fixtures/base_health/vellum-2026-11-13.json"
    ));
    let outcomes = episodes.iter().map(replay).collect::<Vec<_>>();
    let first = &outcomes[0];
    assert!(first.fires, "{}", first.why);
    assert_eq!(first.fix, None, "#409 is not approved yet at 10:37");
    assert_eq!(first.held, vec![408], "#408 is not enqueued");
    let second = &outcomes[1];
    assert!(second.fires);
    assert_eq!(second.fix, Some(409));
    assert_eq!(second.held, vec![408]);
    assert_eq!(second.pick_matches_actual, Some(true));
    assert!(outcomes.iter().all(|outcome| !outcome.actual_fix_held));
    let score = score(&outcomes);
    assert_eq!(
        (score.fired, score.named, score.correct, score.wrong),
        (2, 1, 1, 0)
    );
}

/// Pulp, 2026-11-13: the date-dependent test failed two merge groups, but
/// the tip's only run passed before midnight and nothing re-ran it, so the
/// conservative rule does not fire. With a PR-free control configured for
/// the context the tip would read red: #12140 and #12148 are held, and no
/// fix is named because none was approved before the episode ended.
#[test]
fn pulp_replay_does_not_fire_without_a_tip_run_and_holds_with_a_control() {
    let episodes = load(include_str!(
        "../../../tests/fixtures/base_health/pulp-2026-11-13.json"
    ));
    let real = replay(&episodes[0]);
    assert!(!real.fires, "{}", real.why);
    assert!(real.held.is_empty());
    assert_eq!(real.fix, None);
    let control = replay(&episodes[1]);
    assert!(control.fires, "{}", control.why);
    assert_eq!(
        control.fix, None,
        "#12154 and #12153 were approved after 01:00"
    );
    assert_eq!(control.held, vec![12140, 12148]);
    assert!(!control.actual_fix_held);
}

#[test]
fn red_everywhere_confirms_only_over_a_stale_or_red_tip() {
    let episode = Some(at("2026-11-13T00:40:00Z"));
    let stale_green = TipReading {
        newest_at: Some(at("2026-11-12T23:28:44Z")),
        ..TipReading::default()
    };
    assert_eq!(
        confirmed_since(&stale_green, episode),
        episode,
        "a tip green only from before the episode is stale"
    );
    let fresh_green = TipReading {
        newest_at: Some(at("2026-11-13T00:50:00Z")),
        ..TipReading::default()
    };
    assert_eq!(
        confirmed_since(&fresh_green, episode),
        None,
        "a tip that passed after the episode began ends it"
    );
    assert_eq!(
        confirmed_since(&stale_green, None),
        None,
        "the tip alone is green"
    );
    let unread = TipReading {
        error: Some("HTTP 502".to_owned()),
        ..TipReading::default()
    };
    assert_eq!(
        confirmed_since(&unread, episode),
        None,
        "an unread tip confirms nothing"
    );
    let red = TipReading {
        red: true,
        red_since: Some(at("2026-11-13T00:30:00Z")),
        ..TipReading::default()
    };
    assert_eq!(
        confirmed_since(&red, episode),
        Some(at("2026-11-13T00:30:00Z"))
    );
}

/// The Pulp tip never re-ran after the date-dependent break. A
/// red-everywhere episode on the same check, had one begun at 00:40, would
/// confirm the streak without a control run.
#[test]
fn pulp_replay_fires_on_red_everywhere_over_the_stale_tip() {
    let mut episodes = load(include_str!(
        "../../../tests/fixtures/base_health/pulp-2026-11-13.json"
    ));
    let mut episode = episodes.remove(0);
    episode.decided_at = at("2026-11-13T01:00:00Z");
    episode.red_everywhere_since = Some(at("2026-11-13T00:40:00Z"));
    let outcome = replay(&episode);
    assert!(outcome.fires, "{}", outcome.why);
    assert!(outcome.why.contains("tip verdict stale"), "{}", outcome.why);
    assert_eq!(outcome.held, vec![12140, 12148]);
    assert_eq!(outcome.fix, None);
}

/// A labelled fix is picked while a second approved head also passed the
/// context after the break. Production admits that rival (it covers every
/// confirmed context), so the replay must not report it held.
#[test]
fn a_rival_green_head_beside_a_labelled_fix_is_not_held_in_replay() {
    let mut labelled = candidate(
        1,
        Some("2026-11-13T10:30:00Z"),
        Some("2026-11-13T10:20:00Z"),
    );
    labelled.labels = vec![FIX_LABEL.to_owned()];
    let rival = candidate(
        2,
        Some("2026-11-13T10:31:00Z"),
        Some("2026-11-13T10:21:00Z"),
    );
    let stale = candidate(
        3,
        Some("2026-11-13T10:32:00Z"),
        Some("2026-11-13T09:00:00Z"),
    );
    let episode = ReplayEpisode {
        key: "o/r@main".to_owned(),
        context: "ctx".to_owned(),
        started_at: at("2026-11-13T10:00:00Z"),
        ended_at: None,
        max_streak: 2,
        would_hold: Vec::new(),
        decided_at: at("2026-11-13T11:00:00Z"),
        red_everywhere_since: None,
        tip_runs: vec![run(1, "failure", "2026-11-13T10:00:00Z")],
        prs: vec![labelled.clone(), rival.clone(), stale.clone()],
        actual_fix: Some(1),
        note: String::new(),
    };
    let outcome = replay(&episode);
    assert_eq!(outcome.fix, Some(1));
    assert_eq!(outcome.held, vec![3], "the rival is admitted, as live");
    // The live decision over the same facts says the same.
    let prs = [labelled, rival, stale];
    let decision = decide(&[ContextFacts {
        context: "ctx",
        red_since: Some(at("2026-11-13T10:00:00Z")),
        candidates: &prs,
    }]);
    assert!(matches!(decision.pick, Pick::One(ref fix) if fix.pr == 1));
    assert!(!decision.holds(2));
    assert!(decision.holds(3));
}
