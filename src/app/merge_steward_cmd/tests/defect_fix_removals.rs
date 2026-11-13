//! Defect-fix removals are honoured: the 2026-11-13 Generous-Corp/vellum#406
//! sequence. The coordinator removed head `ef8a1db2` with reason `defect-fix`
//! at 17:05:03Z; 40 seconds later the steward enqueued the same head again.
use chrono::{DateTime, Utc};

use super::*;
use crate::app::merge_steward_cmd::defect_fix_removals::{self, Cache, Reading};
use crate::app::merge_steward_cmd::pr_mutations::mutate_pr_with_recovery;

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("time")
        .with_timezone(&Utc)
}

/// The guard's real line shape (keys as the queue-removal guard writes them).
const GUARD_LINES: &str = concat!(
    r#"{"argv": ["api", "graphql"], "at": "2026-10-24T19:49:26+00:00", "fix_pr": null, "host": "Daniels-M5-Studio", "note": "queued head is unreviewed", "reason": "defect-fix", "session": "s", "targets": [{"head": "197059FB3F00B59C5ED3279A89112F2B3924768F", "pr": 10920, "queue_state": "UNMERGEABLE", "repo": "Generous-Corp/pulp"}]}"#,
    "\n",
    r#"{"at": "2026-10-25T10:00:00+00:00", "reason": "superseded", "targets": [{"head": "aaaa", "pr": 1, "repo": "o/r"}]}"#,
    "\n",
    "not json\n",
    r#"{"at": "2026-10-25T10:00:00+00:00", "reason": "defect-fix", "targets": [{"pr": 2, "repo": "o/r"}]}"#,
    "\n",
);

#[test]
fn the_guard_ledger_is_parsed_defect_fix_only_and_malformed_lines_counted() {
    let (removals, skipped) = defect_fix_removals::parse(GUARD_LINES);
    assert_eq!(removals.len(), 1);
    assert_eq!(removals[0].pr, 10920);
    assert_eq!(removals[0].head, "197059fb3f00b59c5ed3279a89112f2b3924768f");
    assert_eq!(skipped, 2, "the non-JSON line and the target with no head");
}

#[test]
fn a_missing_ledger_is_no_record_and_an_unreadable_one_keeps_the_last_good_read() {
    let temp = tempfile::tempdir().expect("temp");
    let mut cache = Cache::default();
    let missing = defect_fix_removals::read(
        &temp.path().join("absent.jsonl"),
        &mut cache,
        at("2026-11-13T17:00:00Z"),
    );
    assert_eq!(missing.error, None);
    assert!(missing.removals.is_empty());
    let good = temp.path().join("queue-removals.jsonl");
    fs::write(&good, GUARD_LINES).expect("ledger");
    let read = defect_fix_removals::read(&good, &mut cache, at("2026-11-13T17:00:00Z"));
    assert_eq!(read.removals.len(), 1);
    // A directory cannot be read as a file: the last good read is honoured.
    let unreadable = temp.path().join("dir.jsonl");
    fs::create_dir_all(&unreadable).expect("dir");
    let fresh = defect_fix_removals::read(&unreadable, &mut cache, at("2026-11-13T17:10:00Z"));
    assert!(fresh.error.is_some());
    assert!(fresh.fresh_error, "ten minutes after the last good read");
    assert_eq!(
        fresh.removals.len(),
        1,
        "the cached removal is still honoured"
    );
    let stale = defect_fix_removals::read(&unreadable, &mut cache, at("2026-11-13T18:00:00Z"));
    assert!(!stale.fresh_error, "an old error is only logged");
    assert_eq!(stale.removals.len(), 1);
    assert!(stale.lines()[0].starts_with("queue-removals ledger unreadable"));
}

#[test]
fn a_fresh_error_holds_only_a_head_removed_since_the_last_good_read() {
    let reading = Reading {
        error: Some("EIO".to_owned()),
        last_good_at: Some(at("2026-11-13T17:00:00Z")),
        fresh_error: true,
        ..Reading::default()
    };
    let head = "e".repeat(40);
    let queue = |removed_at: &str| {
        serde_json::json!({"data": {"repository": {"pullRequest": {"timelineItems": {"nodes": [
            {"__typename": "RemovedFromMergeQueueEvent", "reason": "manual",
             "createdAt": removed_at, "beforeCommit": {"oid": head}}
        ]}}}}})
    };
    assert!(defect_fix_removals::unjudgeable(
        &reading,
        &queue("2026-11-13T17:05:03Z"),
        &head
    ));
    assert!(!defect_fix_removals::unjudgeable(
        &reading,
        &queue("2026-11-13T16:00:00Z"),
        &head
    ));
    let stale = Reading {
        fresh_error: false,
        ..reading
    };
    assert!(!defect_fix_removals::unjudgeable(
        &stale,
        &queue("2026-11-13T17:05:03Z"),
        &head
    ));
}

/// The #406 sequence through the steward's admission: the removal is
/// recorded at 17:05:03Z, the pass at 17:05:43Z reads it and refuses to put
/// the same head back, before reading anything from GitHub. A new head goes
/// on to the usual admission.
#[cfg(unix)]
#[test]
fn a_defect_fix_removal_refuses_re_enqueue_of_that_head_and_a_new_head_proceeds() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'\necho 'no read expected' >&2\nexit 2",
            log.display()
        ),
    );
    let pr = ready_pr();
    let mut observation = observation_for(pr.clone(), true);
    observation.repo = "Generous-Corp/vellum".to_owned();
    let ledger_file = temp.path().join("queue-removals.jsonl");
    defect_fix_removals::append(
        &ledger_file,
        "Generous-Corp/vellum",
        pr.fact.number,
        &pr.fact.head_sha,
        "framework-trusted-judge failed before framework-trusted-verify was posted",
        at("2026-11-13T17:05:03Z"),
    )
    .expect("append");
    let mut cache = Cache::default();
    let reading = defect_fix_removals::read(&ledger_file, &mut cache, at("2026-11-13T17:05:43Z"));
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let mut context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    context.defect_fix = Some(&reading);
    let mut ledger = StewardLedger::default();
    let (mutation, error) = mutate_pr_with_recovery(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
        false,
    );
    assert_eq!(mutation.as_deref(), Some("defect_fix_removed:defect-fix"));
    assert_eq!(error, None);
    assert!(
        fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "refused before any read or mutation"
    );
    // A new head of the same pull request is not blocked: it goes on to the
    // usual admission (here, the fake's first read fails).
    let mut moved = pr.clone();
    moved.fact.head_sha = "f".repeat(40);
    let (mutation, _error) = mutate_pr_with_recovery(
        &context,
        &moved,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
        false,
    );
    assert_ne!(mutation.as_deref(), Some("defect_fix_removed:defect-fix"));
    assert!(
        !fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "the new head reached the usual admission reads"
    );
}

#[test]
fn a_refused_row_reads_defect_fix_removed_with_the_repair_lane_next() {
    let pr = ready_pr();
    let reading = Reading {
        removals: defect_fix_removals::parse(&format!(
            r#"{{"at": "2026-11-13T17:05:03Z", "reason": "defect-fix", "host": "m3", "note": "doomed group", "targets": [{{"repo": "owner/repo", "pr": {}, "head": "{}"}}]}}"#,
            pr.fact.number, pr.fact.head_sha
        ))
        .0,
        ..Reading::default()
    };
    let mut reports = vec![PrReport {
        number: pr.fact.number,
        head_sha: pr.fact.head_sha.clone(),
        decision: StewardDecision::ArmMergeQueue,
        mutation: None,
        error: None,
        blocked: crate::app::merge_steward_cmd::blocked_on::Stall::default(),
    }];
    crate::app::merge_steward_cmd::merge_group_stall::annotate_defect_fix(
        &reading,
        "owner/repo",
        &mut reports,
    );
    assert_eq!(
        reports[0].blocked.blocked_on,
        "defect_fix_removed:defect-fix"
    );
    assert_eq!(
        reports[0]
            .blocked
            .next_action
            .as_ref()
            .map(|next| next.actor),
        Some(crate::app::merge_steward_cmd::next_action::Actor::Repair)
    );
    assert!(
        reports[0]
            .blocked
            .blocked_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("from m3: doomed group"))
    );
}
