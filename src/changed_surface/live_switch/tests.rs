//! The switch must read `live` only from the exact value on a fresh read, and
//! a trip must turn it off and file exactly one issue, without duplicating a
//! reason already filed and without sending anything on a dry run.

use std::cell::RefCell;

use chrono::{Duration, TimeZone, Utc};

use super::*;

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000 + seconds, 0)
        .single()
        .expect("timestamp")
}

#[test]
fn only_the_exact_value_live_is_live() {
    let cases: [(Result<Option<String>, String>, SwitchMode); 7] = [
        (Ok(Some("live".to_owned())), SwitchMode::Live),
        (Ok(Some(" live\n".to_owned())), SwitchMode::Live),
        (Ok(Some("off".to_owned())), SwitchMode::Shadow),
        (Ok(Some("LIVE".to_owned())), SwitchMode::Shadow),
        (Ok(Some(String::new())), SwitchMode::Shadow),
        (Ok(None), SwitchMode::Shadow),
        (Err("HTTP 502".to_owned()), SwitchMode::Shadow),
    ];
    for (read, expected) in cases {
        let reading = SwitchReading::classify("V", read.clone(), at(0));
        assert_eq!(reading.mode, expected, "{read:?}: {}", reading.reason);
    }
    let unreadable = SwitchReading::classify("V", Err("HTTP 502".to_owned()), at(0));
    assert!(
        unreadable.reason.contains("could not be read"),
        "{}",
        unreadable.reason
    );
    assert_eq!(unreadable.value, None);
}

#[test]
fn a_live_read_acts_only_while_fresh() {
    let reading = SwitchReading::classify("V", Ok(Some("live".to_owned())), at(0));
    let bound = Duration::seconds(600);
    assert_eq!(reading.effective_mode(at(600), bound).0, SwitchMode::Live);
    let (mode, reason) = reading.effective_mode(at(601), bound);
    assert_eq!(mode, SwitchMode::Shadow);
    assert!(reason.contains("stale"), "{reason}");
    // A read stamped after the plan cannot be aged, so it is not trusted.
    assert_eq!(reading.effective_mode(at(-1), bound).0, SwitchMode::Shadow);
    // A shadow read stays shadow however fresh.
    let off = SwitchReading::classify("V", Ok(Some("off".to_owned())), at(0));
    assert_eq!(off.effective_mode(at(0), bound).0, SwitchMode::Shadow);
}

fn issue(number: u64, repo_variable: &str, body_tail: &str) -> MarkedIssue {
    MarkedIssue {
        number,
        key: repo_variable.to_owned(),
        body: format!("{TRIP_MARKER_PREFIX}{repo_variable} -->\n{body_tail}"),
    }
}

#[test]
fn a_first_trip_turns_the_switch_off_and_opens_one_issue() {
    let plan = plan_trip(
        "o/r",
        "V",
        &Ok(Some("live".to_owned())),
        &[],
        "sampled re-run failed",
        at(0),
    );
    assert!(plan.set_off);
    let IssueAction::Open { title, body } = &plan.issue else {
        panic!("expected an open, got {:?}", plan.issue);
    };
    assert!(title.contains('V'), "{title}");
    assert!(body.starts_with(&trip_marker("o/r", "V")), "{body}");
    assert!(body.ends_with(": sampled re-run failed"), "{body}");
    // The marker it writes is the one the issue scan finds again.
    assert_eq!(
        marked_issue::marker_key(body, TRIP_MARKER_PREFIX).as_deref(),
        Some("o/r:V")
    );
}

#[test]
fn a_repeated_trip_adds_a_new_reason_once_and_never_duplicates_one() {
    let existing = issue(7, "o/r:V", "Trips:\n- 2026-10-01T00:00:00Z: audit mismatch");
    let other_variable = issue(8, "o/r:W", "Trips:\n- x: sampled re-run failed");
    let off = Ok(Some("off".to_owned()));

    let same = plan_trip(
        "o/r",
        "V",
        &off,
        &[other_variable.clone(), existing.clone()],
        "audit mismatch",
        at(0),
    );
    assert_eq!(
        same,
        TripPlan {
            set_off: false,
            issue: IssueAction::Nothing { number: 7 }
        }
    );

    let new = plan_trip(
        "o/r",
        "V",
        &off,
        &[other_variable, existing],
        "unreached > 0",
        at(0),
    );
    let IssueAction::Update { number, body } = &new.issue else {
        panic!("expected an update, got {:?}", new.issue);
    };
    assert_eq!(*number, 7, "the issue for this variable, not another's");
    assert!(
        body.contains(": audit mismatch\n- "),
        "keeps the earlier reason: {body}"
    );
    assert!(body.ends_with(": unreached > 0"), "{body}");
}

#[test]
fn anything_but_a_clean_off_is_written_off() {
    for current in [
        Ok(Some("live".to_owned())),
        Ok(None),
        Err("HTTP 502".to_owned()),
        Ok(Some("of".to_owned())),
    ] {
        assert!(
            plan_trip("o/r", "V", &current, &[], "r", at(0)).set_off,
            "{current:?}"
        );
    }
    assert!(!plan_trip("o/r", "V", &Ok(Some(" off\n".to_owned())), &[], "r", at(0)).set_off);
}

/// A fake `gh`: answers by endpoint, records every argv.
struct FakeGh {
    calls: RefCell<Vec<String>>,
    variable: Result<String, String>,
    issues: Result<String, String>,
    patch_variable: Result<String, String>,
}

impl FakeGh {
    fn new(variable: Result<&str, &str>, issues: &str) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            variable: variable.map(str::to_owned).map_err(str::to_owned),
            issues: Ok(issues.to_owned()),
            patch_variable: Ok(String::new()),
        }
    }

    fn call(&self, args: &[String]) -> Result<String, String> {
        let line = args.join(" ");
        self.calls.borrow_mut().push(line.clone());
        if line.starts_with("api repos/o/r/actions/variables/V ") {
            return self.variable.clone();
        }
        if line.starts_with("api --paginate repos/o/r/issues?") {
            return self.issues.clone();
        }
        if line.starts_with("api --method PATCH repos/o/r/actions/variables/V ") {
            return self.patch_variable.clone();
        }
        Ok(r#"{"number":42}"#.to_owned())
    }

    fn writes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|c| c.contains("--method"))
            .cloned()
            .collect()
    }
}

#[test]
fn a_dry_run_reads_but_sends_nothing() {
    let fake = FakeGh::new(Ok("live\n"), "");
    let gh = |args: &[String]| fake.call(args);
    let outcome = trip(&gh, "o/r", "V", "sampled re-run failed", at(0), false).expect("plans");
    assert!(!outcome.applied);
    assert!(
        outcome.variable.starts_with("would set"),
        "{}",
        outcome.variable
    );
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
}

#[test]
fn an_applied_trip_writes_the_variable_before_the_issue() {
    let fake = FakeGh::new(Ok("live\n"), "");
    let gh = |args: &[String]| fake.call(args);
    let outcome = trip(&gh, "o/r", "V", "sampled re-run failed", at(0), true).expect("trips");
    assert!(outcome.applied);
    let writes = fake.writes();
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert!(
        writes[0].starts_with("api --method PATCH repos/o/r/actions/variables/V "),
        "{writes:?}"
    );
    assert!(writes[0].contains("value=off"));
    assert!(
        writes[1].starts_with("api --method POST repos/o/r/issues "),
        "{writes:?}"
    );
}

#[test]
fn an_unreadable_issue_list_refuses_rather_than_risk_a_duplicate() {
    let mut fake = FakeGh::new(Ok("live\n"), "");
    fake.issues = Err("HTTP 502".to_owned());
    let gh = |args: &[String]| fake.call(args);
    let error = trip(&gh, "o/r", "V", "r", at(0), true).expect_err("refuses");
    assert!(error.contains("duplicate"), "{error}");
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
}

#[test]
fn a_failed_variable_write_still_files_the_issue_and_reports_the_failure() {
    let mut fake = FakeGh::new(Ok("live\n"), "");
    fake.patch_variable = Err("HTTP 403".to_owned());
    let gh = |args: &[String]| fake.call(args);
    let error = trip(&gh, "o/r", "V", "r", at(0), true).expect_err("reports");
    assert!(error.contains("could not set V=off"), "{error}");
    assert!(
        fake.writes()
            .iter()
            .any(|w| w.starts_with("api --method POST repos/o/r/issues ")),
        "{:?}",
        fake.writes()
    );
}

#[test]
fn an_already_off_switch_with_this_reason_filed_sends_nothing() {
    let raw = format!(
        r#"{{"number":7,"body":{}}}"#,
        serde_json::to_string(&format!("{} \n- t: r", trip_marker("o/r", "V"))).expect("json")
    );
    let fake = FakeGh::new(Ok("off\n"), &raw);
    let gh = |args: &[String]| fake.call(args);
    let outcome = trip(&gh, "o/r", "V", "r", at(0), true).expect("idempotent");
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
    assert!(outcome.issue.contains("#7"), "{}", outcome.issue);
}
