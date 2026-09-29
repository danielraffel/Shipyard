//! Golden replay of `shipyard pr-watch` over trimmed real GitHub responses.
//!
//! `tests/fixtures/github/pr_watch/` holds every answer the gatherer asks for
//! when it reads `Generous-Corp/pulp` from 2026-09-22T06:15Z to
//! 2026-09-29T06:15Z, trimmed to seven pull requests: #8933, #8970, #9012,
//! #9018, #9019 (each stuck in a known way) and #9026, #9035 (merged
//! cleanly). See the README there for provenance.

use std::path::PathBuf;

use chrono::{DateTime, Duration, TimeZone, Utc};
use shipyard::gate_cost::ReadCache;
use shipyard::pr_watch::digest::DigestPolicy;
use shipyard::pr_watch::fixtures::FixtureReader;
use shipyard::pr_watch::replay::{Expectation, ReplayOptions, clean_merged, replay};
use shipyard::pr_watch::{FlagKind, RepoHistory, Thresholds, WatchQuery, evaluate, gather};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/github/pr_watch")
}

fn until() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 29, 6, 15, 0).unwrap()
}

fn history_with(thresholds: &Thresholds) -> RepoHistory {
    let reader = FixtureReader::load(&fixtures()).expect("fixtures");
    let read = |argv: &[String]| reader.read(argv);
    let query = WatchQuery {
        repo: "Generous-Corp/pulp".to_owned(),
        base: "main".to_owned(),
        workflow: "build.yml".to_owned(),
        required_checks: Vec::new(),
        from: until() - Duration::days(7),
        to: until(),
    };
    gather(&read, &ReadCache::disabled(), &query, thresholds).expect("gather from fixtures")
}

fn options(thresholds: Thresholds) -> ReplayOptions {
    ReplayOptions {
        tick: Duration::minutes(15),
        thresholds,
        digest: DigestPolicy::default(),
        expectations: ["8933=1,3,4,5", "8970=3", "9012=2", "9018=2", "9019=2"]
            .iter()
            .map(|text| text.parse::<Expectation>().unwrap())
            .collect(),
        control_merged_clean: true,
    }
}

fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, day, hour, minute, 0).unwrap()
}

fn kinds_at(history: &RepoHistory, pr: u64, when: DateTime<Utc>) -> Vec<FlagKind> {
    evaluate(history, when, &Thresholds::default())
        .into_iter()
        .filter(|flag| flag.pr == pr)
        .map(|flag| flag.kind)
        .collect()
}

#[test]
fn gather_reads_required_checks_from_branch_protection() {
    let history = history_with(&Thresholds::default());
    assert_eq!(
        history.required_checks,
        [
            "Enforce version & skill sync",
            "Build + prove + (owner-gated) deploy",
            "Vellum trusted freeze",
            "Vellum freeze",
            "macos"
        ]
    );
    assert_eq!(history.prs.len(), 7);
    assert!(history.gaps.is_empty(), "{:?}", history.gaps);
}

#[test]
fn golden_replay_raises_the_expected_flags_and_none_on_clean_merges() {
    let history = history_with(&Thresholds::default());
    assert_eq!(clean_merged(&history), vec![9026, 9035]);
    let report = replay(&history, &options(Thresholds::default()));
    for result in &report.expectations {
        assert!(result.pass, "#{} missing {:?}", result.pr, result.missing);
    }
    let control = report.control.as_ref().unwrap();
    assert!(
        control.pass,
        "clean merges flagged: {:?}",
        control.violations
    );
    assert!(report.pass);
}

#[test]
fn red_while_armed_waits_thirty_minutes_and_ends_at_the_next_push() {
    let history = history_with(&Thresholds::default());
    // #9018: armed; `macos` failed on 320e9c8a at 22:52Z; next push ~01:42Z.
    assert!(!kinds_at(&history, 9018, at(28, 23, 15)).contains(&FlagKind::RedWhileArmed));
    assert!(kinds_at(&history, 9018, at(28, 23, 30)).contains(&FlagKind::RedWhileArmed));
    assert!(kinds_at(&history, 9018, at(29, 1, 30)).contains(&FlagKind::RedWhileArmed));
    assert!(!kinds_at(&history, 9018, at(29, 1, 50)).contains(&FlagKind::RedWhileArmed));
}

#[test]
fn repeated_ejection_counts_named_failed_groups() {
    let history = history_with(&Thresholds::default());
    // #8970: named groups failed `macos` at 07:59Z and 08:30Z on 09-28.
    assert!(!kinds_at(&history, 8970, at(28, 8, 15)).contains(&FlagKind::RepeatedEjection));
    let flags = evaluate(&history, at(28, 9, 0), &Thresholds::default());
    let flag = flags
        .iter()
        .find(|flag| flag.pr == 8970 && flag.kind == FlagKind::RepeatedEjection)
        .expect("flag 3 on #8970");
    assert!(flag.evidence.contains("parent group"), "{}", flag.evidence);
}

#[test]
fn a_repeated_test_is_named_in_the_evidence() {
    let history = history_with(&Thresholds::default());
    let flags = evaluate(&history, at(29, 1, 0), &Thresholds::default());
    let flag = flags
        .iter()
        .find(|flag| flag.pr == 9012 && flag.kind == FlagKind::RepeatTestFailure)
        .expect("flag 1 on #9012");
    assert!(
        flag.evidence.contains("gpu-test-resource-locks"),
        "{}",
        flag.evidence
    );
}

#[test]
fn the_treadmill_is_inferred_from_moving_merge_bases() {
    let history = history_with(&Thresholds::default());
    let flags = evaluate(&history, at(27, 23, 45), &Thresholds::default());
    let flag = flags
        .iter()
        .find(|flag| flag.pr == 8933 && flag.kind == FlagKind::RebaseTreadmill)
        .expect("flag 4 on #8933");
    assert!(flag.evidence.contains("inferred"), "{}", flag.evidence);
}
