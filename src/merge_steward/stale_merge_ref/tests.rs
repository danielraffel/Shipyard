use chrono::{DateTime, Duration, Utc};
use serde_json::json;

use super::*;

const HEAD: &str = "1111111111111111111111111111111111111111";
const OLD_BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW_BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn check(name: &str, conclusion: &str, run: Option<u64>) -> StewardCheck {
    StewardCheck {
        name: name.to_owned(),
        source: super::super::StewardCheckSource::CheckRun,
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        run_id: run,
        observed_at: Some("2026-11-04T12:00:00Z".to_owned()),
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

fn run(event: &str, number: u64, head: &str, base: &str) -> Value {
    json!({"event": event, "pull_requests": [
        {"number": number, "head": {"sha": head}, "base": {"sha": base}}]})
}

#[test]
fn only_a_red_token_is_a_candidate() {
    assert_eq!(
        candidate_contexts("required_failed:macos,linux"),
        Some("macos,linux")
    );
    for token in [
        "required_never_ran:macos",
        "required_pending",
        "required_failed:",
        "ready",
        "merge_state:DIRTY",
        "merge_state:BEHIND",
        "held:review-verdict",
    ] {
        assert_eq!(candidate_contexts(token), None, "{token}");
    }
    assert_eq!(token("macos"), "stale_merge_ref:macos");
}

#[test]
fn every_red_context_needs_its_actions_run() {
    let required = [rule("macos"), rule("linux")];
    let checks = [
        check("macos", "FAILURE", Some(10)),
        check("linux", "FAILURE", Some(11)),
    ];
    assert_eq!(
        red_runs("macos,linux", &checks, &required),
        Some(vec![10, 11])
    );
    let no_run = [check("macos", "FAILURE", None)];
    assert_eq!(
        red_runs("macos", &no_run, &required),
        None,
        "not an Actions run"
    );
    let green = [check("macos", "SUCCESS", Some(10))];
    assert_eq!(red_runs("macos", &green, &required), None);
    assert_eq!(red_runs("lint", &checks, &required), None, "not required");
}

#[test]
fn the_current_merge_refs_base_is_its_first_parent_over_the_head() {
    let commit = json!({"parents": [{"sha": NEW_BASE}, {"sha": HEAD}]});
    assert_eq!(merge_base_of(&commit, HEAD).as_deref(), Some(NEW_BASE));
    assert_eq!(
        merge_base_of(&commit, OLD_BASE),
        None,
        "built on another head"
    );
    assert_eq!(
        merge_base_of(&json!({"parents": [{"sha": HEAD}]}), HEAD),
        None
    );
    assert_eq!(merge_base_of(&json!({}), HEAD), None);
}

#[test]
fn only_a_pull_request_run_of_this_head_names_a_tested_base() {
    assert_eq!(
        run_tested_base(&run("pull_request", 7, HEAD, OLD_BASE), 7, HEAD).as_deref(),
        Some(OLD_BASE)
    );
    for other in [
        run("push", 7, HEAD, OLD_BASE),
        run("merge_group", 7, HEAD, OLD_BASE),
        run("pull_request", 8, HEAD, OLD_BASE),
        run("pull_request", 7, OLD_BASE, OLD_BASE),
    ] {
        assert_eq!(run_tested_base(&other, 7, HEAD), None, "{other}");
    }
}

#[test]
fn a_red_on_the_current_merge_ref_is_never_stale_however_old() {
    // The vellum case: red measured on a merge commit GitHub has since
    // rebuilt.
    assert!(measured_on_old_merge(
        NEW_BASE,
        &[Some(OLD_BASE.to_owned())]
    ));
    // A genuine red on the merge ref that exists now: never stale.
    assert!(!measured_on_old_merge(
        NEW_BASE,
        &[Some(NEW_BASE.to_owned())]
    ));
    assert!(
        !measured_on_old_merge(
            NEW_BASE,
            &[Some(OLD_BASE.to_owned()), Some(NEW_BASE.to_owned())]
        ),
        "one red on the current ref is a real failure"
    );
    assert!(!measured_on_old_merge(
        NEW_BASE,
        &[Some(OLD_BASE.to_owned()), None]
    ));
    assert!(!measured_on_old_merge(NEW_BASE, &[]));
}

#[test]
fn a_memo_answers_until_the_head_or_runs_change_and_rechecks_not_stale() {
    let at = DateTime::parse_from_rfc3339("2026-11-13T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let memo = |stale| StaleMemo {
        head: HEAD.to_owned(),
        runs: vec![10],
        stale,
        at,
    };
    assert_eq!(
        memo(true).reuse(HEAD, &[10], at + Duration::days(30)),
        Some(true)
    );
    assert_eq!(
        memo(false).reuse(HEAD, &[10], at + Duration::hours(1)),
        Some(false)
    );
    assert_eq!(
        memo(false).reuse(HEAD, &[10], at + Duration::hours(7)),
        None
    );
    assert_eq!(
        memo(true).reuse(HEAD, &[11], at),
        None,
        "a rerun is judged again"
    );
    assert_eq!(memo(true).reuse(OLD_BASE, &[10], at), None);
}

#[test]
fn an_advance_run_is_read_by_its_exit_and_envelope() {
    assert!(matches!(
        classify_advance(Some(0), r#"{"outcome": "closed and reopened at head x"}"#, ""),
        RetriggerResult::Done(detail) if detail.contains("reopened")
    ));
    for unreadable in ["", "not json", r#"{"acted": true}"#] {
        assert!(
            matches!(
                classify_advance(Some(0), unreadable, ""),
                RetriggerResult::Failed(_)
            ),
            "exit 0 without an outcome is unknown, never done: {unreadable:?}"
        );
    }
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

#[test]
fn a_ref_is_stale_only_when_every_red_context_is_green_on_the_tip() {
    let tip = |green: &'static [&'static str], unread: &'static [&'static str]| {
        move |context: &str| {
            if unread.contains(&context) {
                Err("HTTP 502".to_owned())
            } else {
                Ok(green.contains(&context))
            }
        }
    };
    assert!(all_green_on_tip(
        "macos,linux",
        &mut tip(&["macos", "linux"], &[])
    ));
    assert!(
        !all_green_on_tip("macos,linux", &mut tip(&["macos"], &[])),
        "linux is red on the base too: a real failure, not a stale ref"
    );
    assert!(
        !all_green_on_tip("macos", &mut tip(&["macos"], &["macos"])),
        "an unread tip is not green"
    );
    assert!(!all_green_on_tip("", &mut tip(&[], &[])));
}
