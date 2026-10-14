use serde_json::json;

use super::{attribute, parse_ledgers, timeline_events};

const REPO: &str = "Generous-Corp/pulp";
const H1: &str = "1111111111111111111111111111111111111111";
const H2: &str = "2222222222222222222222222222222222222222";

/// A timeline shaped like pulp#10219 on 2026-10-14: two pushes, then the
/// merge-queue enqueue GitHub attributes only to the fleet App.
fn timeline_10219() -> serde_json::Value {
    json!({"data": {"repository": {"pullRequest": {
    "number": 10219, "headRefOid": H2, "merged": false,
    "timelineItems": {"nodes": [
        {"__typename": "PullRequestCommit",
         "commit": {"oid": H1, "committedDate": "2026-10-14T02:40:00Z"}},
        {"__typename": "PullRequestCommit",
         "commit": {"oid": H2, "committedDate": "2026-10-14T03:05:00Z"}},
        {"__typename": "AddedToMergeQueueEvent", "createdAt": "2026-10-14T03:12:05Z",
         "actor": {"login": "shipyard-local"}}
    ]}}}}})
}

fn line(
    host: &str,
    session: &str,
    at: &str,
    pr: u64,
    head: &str,
    verb: &str,
    verdict: &str,
) -> String {
    json!({"at": at, "host": host, "session": session, "repo": REPO, "pr": pr,
           "head": head, "verb": verb, "verdict": verdict, "review": "on"})
    .to_string()
}

/// The fleet's ledgers as `who` reads them: one string per host.
fn fleet_ledgers() -> Vec<(&'static str, String)> {
    vec![
        (
            "studio",
            [line(
                "Daniels-Mac-Studio",
                "sess-m3",
                "2026-10-14T03:00:00Z",
                10182,
                H2,
                "arm",
                "allowed",
            )]
            .join("\n"),
        ),
        (
            "m5studio",
            [
                // An earlier arm of the old head, refused, and the one that landed.
                line(
                    "Daniels-M5-Studio",
                    "codex-7f3e",
                    "2026-10-14T02:50:00Z",
                    10219,
                    H1,
                    "arm",
                    "refused",
                ),
                line(
                    "Daniels-M5-Studio",
                    "codex-7f3e",
                    "2026-10-14T03:12:01Z",
                    10219,
                    H2,
                    "arm",
                    "allowed",
                ),
            ]
            .join("\n"),
        ),
        (
            "m1",
            [
                // Same pull request and verb, older head: loses on the head.
                line(
                    "m1",
                    "sess-m1",
                    "2026-10-14T03:11:58Z",
                    10219,
                    H1,
                    "arm",
                    "allowed",
                ),
                "not json".to_owned(),
            ]
            .join("\n"),
        ),
    ]
}

fn read_all(ledgers: &[(&str, String)]) -> Vec<super::LedgerEntry> {
    ledgers
        .iter()
        .flat_map(|(host, arms)| parse_ledgers(host, arms, "").0)
        .collect()
}

#[test]
fn the_10219_enqueue_is_attributed_to_its_host_and_session() {
    let events = timeline_events(&timeline_10219()).expect("timeline");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].head.as_deref(),
        Some(H2),
        "the head at enqueue time"
    );
    let found = attribute(REPO, 10219, &events, &read_all(&fleet_ledgers()));
    assert_eq!(found[0].who(), "Daniels-M5-Studio/codex-7f3e");
    assert_eq!(found[0].head_match, Some(true));
}

/// Negative control: with no ledger entry for the event (a landing through
/// native gh passes no guard), the answer is `unattributed`, never a guess
/// from a nearby entry for another pull request or a refused arm.
#[test]
fn an_event_no_ledger_explains_is_unattributed() {
    let events = timeline_events(&timeline_10219()).expect("timeline");
    let ledgers = vec![
        (
            "studio",
            line(
                "m3",
                "sess-m3",
                "2026-10-14T03:12:00Z",
                10182,
                H2,
                "arm",
                "allowed",
            ),
        ),
        (
            "m5studio",
            line(
                "m5s",
                "codex-7f3e",
                "2026-10-14T03:12:01Z",
                10219,
                H2,
                "arm",
                "refused",
            ),
        ),
    ];
    let found = attribute(REPO, 10219, &events, &read_all(&ledgers));
    assert_eq!(found[0].who(), "unattributed");
    assert!(found[0].entry.is_none());
}

#[test]
fn time_breaks_ties_only_among_entries_that_agree_on_the_key() {
    let events = timeline_events(&timeline_10219()).expect("timeline");
    let ledgers = vec![(
        "m5studio",
        [
            line(
                "m5s",
                "early",
                "2026-10-14T03:01:00Z",
                10219,
                H2,
                "arm",
                "allowed",
            ),
            line(
                "m5s",
                "late",
                "2026-10-14T03:12:03Z",
                10219,
                H2,
                "arm",
                "allowed",
            ),
            // Two hours before: outside the window.
            line(
                "m5s",
                "stale",
                "2026-10-14T01:00:00Z",
                10219,
                H2,
                "arm",
                "allowed",
            ),
        ]
        .join("\n"),
    )];
    let found = attribute(REPO, 10219, &events, &read_all(&ledgers));
    assert_eq!(found[0].who(), "m5s/late");
}

#[test]
fn an_entry_for_the_events_head_beats_a_nearer_entry_for_another_head() {
    let events = timeline_events(&timeline_10219()).expect("timeline");
    let ledgers = vec![(
        "m5studio",
        [
            line(
                "m5s",
                "right-head",
                "2026-10-14T03:06:00Z",
                10219,
                H2,
                "arm",
                "allowed",
            ),
            // Nearer in time, but it armed the old head.
            line(
                "m5s",
                "old-head",
                "2026-10-14T03:12:00Z",
                10219,
                H1,
                "arm",
                "allowed",
            ),
        ]
        .join("\n"),
    )];
    let found = attribute(REPO, 10219, &events, &read_all(&ledgers));
    assert_eq!(found[0].who(), "m5s/right-head");
    assert_eq!(found[0].head_match, Some(true));
}

#[test]
fn a_merge_and_a_removal_join_by_their_own_verbs() {
    let timeline = json!({"data": {"repository": {"pullRequest": {
    "number": 7, "headRefOid": H2, "merged": true,
    "timelineItems": {"nodes": [
        {"__typename": "RemovedFromMergeQueueEvent", "createdAt": "2026-10-14T04:00:00Z",
         "reason": "manual", "actor": {"login": "shipyard-local"}, "beforeCommit": {"oid": H1}},
        {"__typename": "MergedEvent", "createdAt": "2026-10-14T05:00:00Z",
         "actor": {"login": "shipyard-local"}}
    ]}}}}});
    let events = timeline_events(&timeline).expect("timeline");
    let removal = json!({"at": "2026-10-14T03:59:58+00:00", "host": "m1", "session": "sess-r",
        "reason": "defect", "targets": [{"repo": REPO, "pr": 7, "head": H1}]})
    .to_string();
    let arms = line(
        "m3",
        "sess-merge",
        "2026-10-14T04:59:59Z",
        7,
        H2,
        "merge",
        "allowed",
    );
    let (entries, skipped) = parse_ledgers("m1", &arms, &removal);
    assert_eq!(skipped, 0);
    let found = attribute(REPO, 7, &events, &entries);
    assert_eq!(found[0].event.verb, "remove");
    assert_eq!(found[0].who(), "m1/sess-r");
    assert_eq!(found[1].event.verb, "merge");
    assert_eq!(found[1].who(), "m3/sess-merge");
}

#[test]
fn a_line_without_host_takes_the_host_it_was_read_from_and_bad_lines_are_counted() {
    let arms = json!({"at": "2026-10-14T03:12:01Z", "repo": REPO, "pr": 1, "head": H1,
        "verb": "arm", "verdict": "allowed"})
    .to_string();
    let (entries, skipped) = parse_ledgers("m5studio", &format!("{arms}\n{{bad"), "");
    assert_eq!(skipped, 1);
    assert_eq!(entries[0].host, "m5studio");
    assert_eq!(entries[0].session, "unattributed");
}
