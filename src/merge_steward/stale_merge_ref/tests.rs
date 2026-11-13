use super::*;

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("time")
        .with_timezone(&Utc)
}

fn check(name: &str, observed_at: &str) -> StewardCheck {
    StewardCheck {
        name: name.to_owned(),
        source: super::super::StewardCheckSource::CheckRun,
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("FAILURE".to_owned()),
        run_id: None,
        observed_at: Some(observed_at.to_owned()),
        check_id: Some(1),
        description: None,
    }
}

fn rule(name: &str) -> RequiredCheck {
    RequiredCheck {
        context: name.to_owned(),
        app_id: None,
    }
}

/// Base commit times, newest first: one per hour back from `tip`.
fn commits(tip: &str, count: i64) -> Vec<DateTime<Utc>> {
    (0..count)
        .map(|hour| at(tip) - Duration::hours(hour))
        .collect()
}

#[test]
fn the_policy_defaults_and_reads_the_base_config() {
    assert_eq!(
        StaleRefPolicy::default(),
        StaleRefPolicy {
            max_lag_days: 3,
            max_base_commits: 30
        }
    );
    let table: toml::Table =
        "[auto_merge.stale_merge_ref]\nmax_lag_days = 7\nmax_base_commits = 0\n"
            .parse()
            .expect("toml");
    let policy = StaleRefPolicy::from_table(&table);
    assert_eq!(policy.max_lag_days, 7);
    assert_eq!(policy.max_base_commits, 0);
    assert!(policy.enabled());
    assert_eq!(policy.commits_to_read(), 1, "the tip is always read");
    let off: toml::Table = "[auto_merge.stale_merge_ref]\nmax_lag_days = 0\nmax_base_commits = 0\n"
        .parse()
        .expect("toml");
    assert!(!StaleRefPolicy::from_table(&off).enabled());
    let mistyped: toml::Table = "[auto_merge.stale_merge_ref]\nmax_lag_days = \"x\"\n"
        .parse()
        .expect("toml");
    assert_eq!(
        StaleRefPolicy::from_table(&mistyped),
        StaleRefPolicy::default()
    );
    let big = StaleRefPolicy {
        max_lag_days: 0,
        max_base_commits: 500,
    };
    assert_eq!(big.commits_to_read(), BASE_COMMITS_PAGE);
}

#[test]
fn a_base_that_moved_on_for_days_makes_the_ref_stale() {
    let runs = at("2026-11-04T12:00:00Z");
    let stale = judge(
        StaleRefPolicy::default(),
        runs,
        &commits("2026-11-13T12:00:00Z", 5),
    )
    .expect("stale");
    assert_eq!(stale.lag_days, 9);
    assert_eq!(stale.base_commits_since, 5);
    assert!(
        judge(
            StaleRefPolicy::default(),
            runs,
            &commits("2026-11-06T12:00:00Z", 5)
        )
        .is_none(),
        "two days and five commits is within both limits"
    );
}

#[test]
fn enough_base_commits_make_the_ref_stale_within_a_day() {
    let runs = at("2026-11-13T00:00:00Z");
    // Hourly commits from 06:00 the next day back: 30 land after the run.
    let stale = judge(
        StaleRefPolicy::default(),
        runs,
        &commits("2026-11-14T06:00:00Z", 40),
    )
    .expect("stale");
    assert_eq!(stale.base_commits_since, 30);
    assert_eq!(stale.lag_days, 1);
    // 23 commits after the run, under a day: within both limits.
    assert!(
        judge(
            StaleRefPolicy::default(),
            runs,
            &commits("2026-11-13T23:00:00Z", 40)
        )
        .is_none()
    );
    let lag_only = StaleRefPolicy {
        max_lag_days: 3,
        max_base_commits: 0,
    };
    assert!(judge(lag_only, runs, &commits("2026-11-14T06:00:00Z", 40)).is_none());
}

#[test]
fn nothing_is_judged_without_base_commits_or_required_runs() {
    assert!(judge(StaleRefPolicy::default(), at("2026-11-01T00:00:00Z"), &[]).is_none());
    assert!(
        newest_required_run(&[check("lint", "2026-11-01T00:00:00Z")], &[rule("macos")]).is_none()
    );
    assert_eq!(
        newest_required_run(
            &[
                check("macos", "2026-11-01T00:00:00Z"),
                check("linux", "2026-11-02T00:00:00Z"),
                check("lint", "2026-11-09T00:00:00Z"),
            ],
            &[rule("macos"), rule("linux")]
        ),
        Some(at("2026-11-02T00:00:00Z")),
        "the newest required run; checks that are not required are ignored"
    );
}

#[test]
fn only_red_and_never_ran_tokens_are_candidates() {
    assert_eq!(
        candidate_contexts("required_failed:macos,linux"),
        Some("macos,linux")
    );
    assert_eq!(
        candidate_contexts("required_never_ran:macos"),
        Some("macos")
    );
    for token in [
        "required_pending",
        "required_failed:",
        "ready",
        "held:review-verdict",
    ] {
        assert_eq!(candidate_contexts(token), None, "{token}");
    }
    assert_eq!(token("macos"), "stale_merge_ref:macos");
}

#[test]
fn an_advance_run_is_read_by_its_exit_and_envelope() {
    assert!(matches!(
        classify_advance(Some(0), r#"{"outcome": "closed and reopened at head x"}"#, ""),
        RetriggerResult::Done(detail) if detail.contains("reopened")
    ));
    let guard = classify_advance(
        Some(1),
        r#"{"acted": false, "refused": {"condition": "close_refused", "detail": "pr-close-guard: refusing PR #7"}}"#,
        "",
    );
    assert!(
        matches!(&guard, RetriggerResult::Blocked(detail) if detail.starts_with(BLOCKED_GUARD) && detail.contains("pr-close-guard")),
        "{guard:?}"
    );
    assert!(matches!(
        classify_advance(Some(1), r#"{"refused": {"condition": "repair_claim_live", "detail": "lane"}}"#, ""),
        RetriggerResult::Refused(detail) if detail.starts_with("retrigger_refused:repair_claim_live")
    ));
    assert!(matches!(
        classify_advance(
            Some(2),
            r#"{"unread": "check runs at head unreadable"}"#,
            ""
        ),
        RetriggerResult::Deferred(_)
    ));
    assert!(matches!(
        classify_advance(Some(2), "", "error: unrecognized subcommand 'advance'"),
        RetriggerResult::Blocked(detail) if detail.starts_with(BLOCKED_UNAVAILABLE)
    ));
    assert!(matches!(
        classify_advance(Some(3), r#"{"outcome": "closed, and the reopen failed twice"}"#, ""),
        RetriggerResult::Failed(detail) if detail.starts_with("retrigger_partial")
    ));
    assert!(matches!(
        classify_advance(Some(2), "not json", "config error"),
        RetriggerResult::Deferred(_)
    ));
    for (exit, stdout) in [(Some(1), ""), (None, ""), (Some(101), "")] {
        assert!(
            matches!(
                classify_advance(exit, stdout, "boom"),
                RetriggerResult::Failed(_)
            ),
            "{exit:?} {stdout}"
        );
    }
}
