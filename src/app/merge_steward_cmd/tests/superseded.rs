//! Deterministic superseded evidence, reported in shadow mode.
use std::cell::RefCell;

use super::*;
use crate::app::merge_steward_cmd::blocked_on::Stall;
#[cfg(unix)]
use crate::app::merge_steward_cmd::cancellation::apply_pr_plans;
use crate::app::merge_steward_cmd::superseded::{
    self, FileChange, Mention, annotate_with as annotate_raw, label_successor, named_by,
    named_successors, superseded_numbers, tree_equal,
};

/// One shadow pass with an unspent budget.
fn annotate_with(
    read: superseded::Read<'_>,
    repo: &str,
    base: &str,
    ledger: &mut StewardLedger,
    reports: &mut [PrReport],
    now: DateTime<Utc>,
) -> Option<String> {
    annotate_raw(read, repo, base, ledger, reports, now, &|| false)
}

const REPO: &str = "danielraffel/fleet-ssh";
const HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const LABELS_BLOB: &str = "e2eb39ec0000000000000000000000000000000a";

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-11T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn file(path: &str, status: &str, sha: Option<&str>) -> FileChange {
    FileChange {
        path: path.to_owned(),
        previous: None,
        status: status.to_owned(),
        sha: sha.map(str::to_owned),
    }
}

fn base(entries: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
    entries
        .iter()
        .map(|(path, blob)| ((*path).to_owned(), blob.map(str::to_owned)))
        .collect()
}

#[test]
fn the_fleet_ssh_15_shape_is_tree_equal() {
    // #15 changes one file; #16 already landed the identical blob on main.
    let head = [file(
        "fleet/advertised-labels.json",
        "modified",
        Some(LABELS_BLOB),
    )];
    let tip = base(&[("fleet/advertised-labels.json", Some(LABELS_BLOB))]);
    let detail = tree_equal(&head, &tip).expect("identical on main");
    assert!(
        detail.contains("fleet/advertised-labels.json@e2eb39ec"),
        "{detail}"
    );
}

#[test]
fn a_file_main_lacks_or_holds_differently_is_not_equal() {
    let head = [
        file(
            "fleet/advertised-labels.json",
            "modified",
            Some(LABELS_BLOB),
        ),
        file("extra.txt", "added", Some("d1")),
    ];
    let tip = base(&[
        ("fleet/advertised-labels.json", Some(LABELS_BLOB)),
        ("extra.txt", None),
    ]);
    assert_eq!(tree_equal(&head, &tip), None, "extra.txt is not on main");
    let different = base(&[("fleet/advertised-labels.json", Some("ffff"))]);
    assert_eq!(tree_equal(&head[..1], &different), None);
    assert_eq!(
        tree_equal(&head[..1], &BTreeMap::new()),
        None,
        "unread path: no verdict"
    );
    let tree_there = base(&[("fleet/advertised-labels.json", Some("not-a-blob:Tree"))]);
    assert_eq!(tree_equal(&head[..1], &tree_there), None);
}

#[test]
fn removals_and_renames_need_the_same_on_main() {
    let removed = [file("old.txt", "removed", None)];
    assert!(tree_equal(&removed, &base(&[("old.txt", None)])).is_some());
    assert!(tree_equal(&removed, &base(&[("old.txt", Some("a"))])).is_none());
    assert!(
        tree_equal(&removed, &BTreeMap::new()).is_none(),
        "unread is not absent"
    );
    let mut renamed = file("new.txt", "renamed", Some("r1"));
    renamed.previous = Some("old.txt".to_owned());
    let landed = base(&[("new.txt", Some("r1")), ("old.txt", None)]);
    assert!(tree_equal(std::slice::from_ref(&renamed), &landed).is_some());
    // main has the new path but kept the old one: not the same tree
    let kept = base(&[("new.txt", Some("r1")), ("old.txt", Some("o1"))]);
    assert!(tree_equal(std::slice::from_ref(&renamed), &kept).is_none());
    assert_eq!(
        superseded::paths_to_read(&[renamed]),
        vec!["new.txt".to_owned(), "old.txt".to_owned()]
    );
}

#[test]
fn an_empty_or_truncated_list_proves_nothing() {
    assert!(tree_equal(&[], &BTreeMap::new()).is_none());
    let many: Vec<FileChange> = (0..300)
        .map(|i| file(&format!("f{i}"), "added", Some("s")))
        .collect();
    let all: BTreeMap<String, Option<String>> = (0..300)
        .map(|i| (format!("f{i}"), Some("s".to_owned())))
        .collect();
    assert!(tree_equal(&many, &all).is_none());
}

#[test]
fn supersedes_lists_and_successor_names_parse() {
    assert_eq!(
        superseded_numbers("Supersedes #14, #15 and #16.")
            .into_iter()
            .collect::<Vec<_>>(),
        vec![14, 15, 16]
    );
    assert!(superseded_numbers("Fixes #15; the superseded approach").is_empty());
    assert!(superseded_numbers("this supersedes the plan in #15").is_empty());
    assert_eq!(
        named_successors(&["Superseded by #16, closing.".to_owned()])
            .into_iter()
            .collect::<Vec<_>>(),
        vec![16]
    );
}

fn mention(number: u64, merged: bool, same_repo: bool, text: &str) -> Mention {
    Mention {
        number,
        merged,
        same_repo,
        base_ref: "main".to_owned(),
        text: text.to_owned(),
    }
}

#[test]
fn named_by_needs_a_merged_pr_in_the_same_repo() {
    let open = [mention(16, false, true, "Supersedes #15")];
    assert_eq!(named_by(15, "main", &open), None, "not merged yet");
    let other_repo = [mention(16, true, false, "Supersedes #15")];
    assert_eq!(named_by(15, "main", &other_repo), None);
    let merged = [mention(16, true, true, "Supersedes #15")];
    assert_eq!(named_by(15, "main", &merged).map(|(n, _)| n), Some(16));
    let mere_mention = [mention(16, true, true, "Related to #15")];
    assert_eq!(named_by(15, "main", &mere_mention), None);
}

#[test]
fn label_successor_needs_the_label_and_a_merged_successor() {
    let named: BTreeSet<u64> = [16].into_iter().collect();
    let merged: BTreeMap<u64, bool> = [(16, true)].into_iter().collect();
    let open: BTreeMap<u64, bool> = [(16, false)].into_iter().collect();
    assert_eq!(
        label_successor(15, true, &named, &merged).map(|(n, _)| n),
        Some(16)
    );
    assert_eq!(label_successor(15, false, &named, &merged), None);
    assert_eq!(label_successor(15, true, &named, &open), None);
    assert_eq!(label_successor(15, true, &BTreeSet::new(), &merged), None);
}

#[test]
fn the_kill_switch_reads_off() {
    for off in ["off", "0", "false", " NO "] {
        assert!(!superseded::enabled_from(Some(off)), "{off}");
    }
    assert!(superseded::enabled_from(None));
    assert!(superseded::enabled_from(Some("on")));
}

fn report(number: u64, token: &str) -> PrReport {
    PrReport {
        number,
        head_sha: HEAD.to_owned(),
        decision: StewardDecision::ArmMergeQueue,
        mutation: None,
        error: None,
        blocked: Stall {
            blocked_on: token.to_owned(),
            ..Stall::default()
        },
    }
}

/// A fake GitHub: no mentions, and #15's head identical on main.
fn fake(calls: &RefCell<Vec<String>>) -> impl Fn(&[String]) -> Result<String, String> + '_ {
    move |args: &[String]| {
        let joined = args.join(" ");
        calls.borrow_mut().push(joined.clone());
        if joined.contains("timelineItems") {
            return Ok(
                r#"{"data":{"repository":{"p15":{"body":"","merged":false,"isInMergeQueue":false,"labels":{"nodes":[]},
                "comments":{"nodes":[]},"timelineItems":{"nodes":[]}}}}}"#
                    .to_owned(),
            );
        }
        if joined.contains(&format!("compare/main...{HEAD}")) {
            return Ok(format!(
                r#"{{"ahead_by":1,"base_commit":{{"sha":"cafe0000"}},"files":[
                {{"filename":"fleet/advertised-labels.json","status":"modified","sha":"{LABELS_BLOB}"}}]}}"#
            ));
        }
        if joined.contains("object(expression:") {
            assert!(
                joined.contains(r#""main:fleet/advertised-labels.json""#),
                "{joined}"
            );
            return Ok(format!(
                r#"{{"data":{{"repository":{{"tip":{{"target":{{"oid":"cafe1111"}}}},"root":{{"__typename":"Tree"}},
                "f0":{{"__typename":"Blob","oid":"{LABELS_BLOB}"}}}}}}}}"#
            ));
        }
        Err(format!("unexpected read: {joined}"))
    }
}

#[test]
fn a_pass_reports_the_evidence_without_changing_blocked_on_and_caches_it() {
    let calls = RefCell::new(Vec::new());
    let read = fake(&calls);
    let mut ledger = StewardLedger::default();
    let mut reports = vec![report(15, "no_verdict"), report(20, "queued")];
    annotate_with(&read, REPO, "main", &mut ledger, &mut reports, now());
    let found = reports[0].blocked.superseded.as_ref().unwrap_or_else(|| {
        panic!(
            "evidence: {:?} calls {:?}",
            ledger.superseded_checks,
            calls.borrow()
        )
    });
    assert_eq!(found.kind, "tree_equal");
    assert!(found.action.contains("shadow"), "{}", found.action);
    assert_eq!(
        reports[0].blocked.blocked_on, "no_verdict",
        "shadow never changes the token"
    );
    assert!(
        reports[1].blocked.superseded.is_none(),
        "queued rows are skipped"
    );
    assert!(
        calls.borrow().iter().all(|c| !c.contains("-X")),
        "reads only"
    );
    let first = calls.borrow().len();
    assert_eq!(first, 3, "mentions + one compare + one blob read");
    // Ten minutes later: reused from the ledger, no read.
    let mut again = vec![report(15, "no_verdict")];
    annotate_with(
        &read,
        REPO,
        "main",
        &mut ledger,
        &mut again,
        now() + chrono::Duration::minutes(10),
    );
    assert_eq!(calls.borrow().len(), first);
    assert!(again[0].blocked.superseded.is_some());
    // A new head is read at once.
    let mut moved = vec![report(15, "no_verdict")];
    moved[0].head_sha = "c".repeat(40);
    annotate_with(
        &read,
        REPO,
        "main",
        &mut ledger,
        &mut moved,
        now() + chrono::Duration::minutes(11),
    );
    assert!(calls.borrow().len() > first);
    assert!(
        moved[0].blocked.superseded.is_none(),
        "unread new head has no evidence"
    );
}

#[test]
fn a_failed_read_records_unknown_not_clean() {
    let read = |_: &[String]| -> Result<String, String> { Err("rate limited".to_owned()) };
    let mut ledger = StewardLedger::default();
    let mut reports = vec![report(15, "no_verdict")];
    annotate_with(&read, REPO, "main", &mut ledger, &mut reports, now());
    let check = ledger.superseded_checks.values().next().expect("recorded");
    assert!(check.unknown && check.evidence.is_none(), "{check:?}");
    assert!(reports[0].blocked.superseded.is_none());
}

#[test]
fn a_merged_pr_naming_it_wins_without_any_compare() {
    let calls = RefCell::new(Vec::new());
    let read = |args: &[String]| -> Result<String, String> {
        calls.borrow_mut().push(args.join(" "));
        Ok(
            r#"{"data":{"repository":{"p15":{"body":"","merged":false,"isInMergeQueue":false,"labels":{"nodes":[]},
            "comments":{"nodes":[]},"timelineItems":{"nodes":[{"source":{"number":16,
            "merged":true,"baseRefName":"main","title":"labels","body":"Supersedes #15",
            "repository":{"nameWithOwner":"danielraffel/fleet-ssh"}}}]}}}}}"#
                .to_owned(),
        )
    };
    let mut ledger = StewardLedger::default();
    let mut reports = vec![report(15, "merge_state:CLEAN")];
    annotate_with(&read, REPO, "main", &mut ledger, &mut reports, now());
    let found = reports[0].blocked.superseded.as_ref().expect("evidence");
    assert_eq!(
        (found.kind.as_str(), found.successor),
        ("named_by", Some(16))
    );
    assert_eq!(calls.borrow().len(), 1);
}

#[test]
fn a_clean_tree_with_an_unreadable_mention_read_is_not_cached_clean() {
    // The mention read failed, so a merged "Supersedes #15" could be missed:
    // the head stays unchecked and is read again next pass.
    let blob_reads = RefCell::new(0);
    let read = |args: &[String]| -> Result<String, String> {
        let joined = args.join(" ");
        if joined.contains("timelineItems") {
            return Err("graphql timed out".to_owned());
        }
        if joined.contains("object(expression:") {
            *blob_reads.borrow_mut() += 1;
            // main holds a different blob: the tree proves "not superseded"
            return Ok(
                r#"{"data":{"repository":{"tip":null,"f0":{"__typename":"Blob","oid":"9"}}}}"#
                    .to_owned(),
            );
        }
        Ok(r#"{"ahead_by":1,"files":[{"filename":"a","status":"modified","sha":"1"}]}"#.to_owned())
    };
    let mut ledger = StewardLedger::default();
    let mut reports = vec![report(15, "no_verdict")];
    annotate_with(&read, REPO, "main", &mut ledger, &mut reports, now());
    let check = ledger.superseded_checks.values().next().expect("recorded");
    assert!(
        check.unknown,
        "a clean tree without the mention read is unknown, not clean"
    );
    assert_eq!(
        *blob_reads.borrow(),
        0,
        "without the queue state no tree is judged"
    );
}

#[test]
fn a_pass_that_spent_its_budget_reads_nothing() {
    let read = |args: &[String]| -> Result<String, String> { Err(format!("read {args:?}")) };
    let mut ledger = StewardLedger::default();
    let mut reports = vec![report(15, "no_verdict"), report(16, "deferred_pass_budget")];
    annotate_with(&read, REPO, "main", &mut ledger, &mut reports, now());
    assert!(ledger.superseded_checks.is_empty());
}

#[test]
fn a_failed_head_is_not_read_again_until_the_recheck_interval() {
    let calls = RefCell::new(0);
    let read = |_: &[String]| -> Result<String, String> {
        *calls.borrow_mut() += 1;
        Err("rate limited".to_owned())
    };
    let mut ledger = StewardLedger::default();
    annotate_with(
        &read,
        REPO,
        "main",
        &mut ledger,
        &mut [report(15, "no_verdict")],
        now(),
    );
    let first = *calls.borrow();
    annotate_with(
        &read,
        REPO,
        "main",
        &mut ledger,
        &mut [report(15, "no_verdict")],
        now() + chrono::Duration::minutes(5),
    );
    assert_eq!(*calls.borrow(), first, "steady state costs nothing");
    annotate_with(
        &read,
        REPO,
        "main",
        &mut ledger,
        &mut [report(15, "no_verdict")],
        now() + chrono::Duration::minutes(31),
    );
    assert!(*calls.borrow() > first);
}

/// A mention read for #15 with the given queue facts and no mentions.
fn queue_facts(merged: bool, in_queue: bool, events: &[&str]) -> String {
    let nodes: Vec<String> = events
        .iter()
        .map(|e| format!(r#"{{"__typename":"{e}"}}"#))
        .collect();
    format!(
        r#"{{"data":{{"repository":{{"p15":{{"body":"","merged":{merged},"isInMergeQueue":{in_queue},
        "labels":{{"nodes":[]}},"comments":{{"nodes":[]}},"queue":{{"nodes":[{}]}},
        "timelineItems":{{"nodes":[]}}}}}}}}}}"#,
        nodes.join(",")
    )
}

/// GitHub where #15's head is fully on main (`ahead_by` 0).
fn contained_github(
    mentions: String,
    calls: &RefCell<Vec<String>>,
) -> impl Fn(&[String]) -> Result<String, String> + '_ {
    move |args: &[String]| {
        let joined = args.join(" ");
        calls.borrow_mut().push(joined.clone());
        if joined.contains("timelineItems") {
            return Ok(mentions.clone());
        }
        if joined.contains("/compare/") {
            return Ok(r#"{"ahead_by":0,"base_commit":{"sha":"cafe0000"},"files":[]}"#.to_owned());
        }
        Err(format!("unexpected read: {joined}"))
    }
}

#[test]
fn a_contained_head_out_of_the_queue_is_contained() {
    let calls = RefCell::new(Vec::new());
    let read = contained_github(
        queue_facts(
            false,
            false,
            &["AddedToMergeQueueEvent", "RemovedFromMergeQueueEvent"],
        ),
        &calls,
    );
    let mut ledger = StewardLedger::default();
    let mut reports = vec![report(15, "merge_state:CLEAN")];
    annotate_with(&read, REPO, "main", &mut ledger, &mut reports, now());
    let found = reports[0].blocked.superseded.as_ref().expect("contained");
    assert_eq!(found.kind, "contained");
}

#[test]
fn a_head_the_queue_is_landing_is_never_judged() {
    // pulp#12014: queued 07:35:47Z, removed 08:04:26Z, merged 08:04:27Z.
    // In between its row did not read `queued`, yet main held its head.
    for facts in [
        queue_facts(false, true, &["AddedToMergeQueueEvent"]),
        queue_facts(false, false, &["AddedToMergeQueueEvent"]),
        queue_facts(
            true,
            false,
            &["AddedToMergeQueueEvent", "RemovedFromMergeQueueEvent"],
        ),
    ] {
        let calls = RefCell::new(Vec::new());
        let read = contained_github(facts.clone(), &calls);
        let mut ledger = StewardLedger::default();
        let mut reports = vec![report(15, "no_verdict")];
        annotate_with(&read, REPO, "main", &mut ledger, &mut reports, now());
        assert!(reports[0].blocked.superseded.is_none(), "{facts}");
        assert!(
            calls.borrow().iter().all(|c| !c.contains("/compare/")),
            "a landing head is not even compared: {facts}"
        );
    }
}

fn blob_reply(tip: &str, root: &str) -> String {
    format!(r#"{{"data":{{"repository":{{"tip":{tip},"root":{root},"f0":null}}}}}}"#)
}

#[test]
fn absence_needs_a_resolved_base_as_its_control() {
    // A delete-only PR: main must be proven to lack the file.
    let ok_reply = blob_reply(r#"{"target":{"oid":"cafe"}}"#, r#"{"__typename":"Tree"}"#);
    for (reply, expect) in [
        (ok_reply, true),
        (blob_reply("null", r#"{"__typename":"Tree"}"#), false),
        (blob_reply(r#"{"target":{"oid":"cafe"}}"#, "null"), false),
    ] {
        let reply_clone = reply.clone();
        let read = move |args: &[String]| -> Result<String, String> {
            let joined = args.join(" ");
            if joined.contains("object(expression:") {
                return Ok(reply_clone.clone());
            }
            Ok(r#"{"ahead_by":1,"files":[{"filename":"gone.txt","status":"removed"}]}"#.to_owned())
        };
        let found = superseded::tree_evidence(&read, REPO, "main", HEAD);
        if expect {
            assert_eq!(found.expect("read").expect("equal").kind, "tree_equal");
        } else {
            assert!(
                found.is_err(),
                "an unresolved base is unknown, never absent: {reply}"
            );
        }
    }
}

#[test]
fn successors_must_land_on_the_same_base_and_text_must_be_unqualified() {
    let mut other_base = mention(16, true, true, "Supersedes #15");
    other_base.base_ref = "release/1.x".to_owned();
    assert_eq!(named_by(15, "main", &[other_base]), None);
    let revert = mention(16, true, true, "Revert \"labels\"\nSupersedes #15");
    assert_eq!(named_by(15, "main", &[revert]), None);
    for text in [
        "This does not supersede #15",
        "partly supersedes #15",
        "It is not superseded by #16",
        "only partly superseded by #16",
    ] {
        assert!(superseded_numbers(text).is_empty(), "{text}");
        assert!(named_successors(&[text.to_owned()]).is_empty(), "{text}");
    }
    assert_eq!(
        named_successors(&["Superseded by #16.".to_owned()])
            .into_iter()
            .collect::<Vec<_>>(),
        vec![16]
    );
}

#[test]
fn a_spent_budget_does_not_read_named_successors_after_mentions() {
    let calls = RefCell::new(Vec::new());
    let read = |args: &[String]| -> Result<String, String> {
        let joined = args.join(" ");
        calls.borrow_mut().push(joined.clone());
        if joined.contains("timelineItems") {
            return Ok(
                r#"{"data":{"repository":{"p15":{"body":"superseded by #16","merged":false,
                "isInMergeQueue":false,"author":{"__typename":"Bot","login":"shipyard-local[bot]"},
                "labels":{"nodes":[{"name":"superseded"}]},"comments":{"nodes":[]},
                "queue":{"nodes":[]},"timelineItems":{"nodes":[]}}}}}"#
                    .to_owned(),
            );
        }
        Err(format!("unexpected read: {joined}"))
    };
    let asked = RefCell::new(0);
    let stop = || {
        *asked.borrow_mut() += 1;
        *asked.borrow() > 1
    };
    let (verdicts, error) =
        superseded::check_repo(&read, REPO, "main", &[(15, HEAD.to_owned())], &stop);
    assert!(error.is_none());
    assert!(verdicts.is_empty(), "the unread head stays due");
    assert_eq!(calls.borrow().len(), 1, "mentions is the last read");
}

#[test]
fn a_unicode_title_does_not_panic_revert_detection() {
    let mention = mention(16, true, true, "Reveré\nSupersedes #15");
    assert_eq!(named_by(15, "main", &[mention]).map(|(n, _)| n), Some(16));
}

#[test]
fn only_a_members_comment_names_a_successor() {
    let data = serde_json::json!({"p15": {"body": "", "merged": false, "isInMergeQueue": false,
        "labels": {"nodes": [{"name": "superseded"}]},
        "comments": {"nodes": [
            {"body": "superseded by #16", "authorAssociation": "NONE"},
            {"body": "superseded by #17", "authorAssociation": "MEMBER"}]},
        "queue": {"nodes": []}, "timelineItems": {"nodes": []}}});
    let parsed = superseded::parse_mentions(REPO, &[15], &data);
    let (_, texts, labelled) = parsed.by_pr.get(&15).expect("parsed");
    assert!(labelled);
    assert_eq!(
        named_successors(texts).into_iter().collect::<Vec<_>>(),
        vec![17]
    );
}

#[test]
fn only_the_allowlisted_fleet_bot_is_a_fleet_author() {
    let data = |login: &str| {
        serde_json::json!({"p15": {"body": "", "merged": false, "isInMergeQueue": false,
            "author": {"__typename": "Bot", "login": login},
            "labels": {"nodes": []}, "comments": {"nodes": []},
            "queue": {"nodes": []}, "timelineItems": {"nodes": []}}})
    };
    let unknown = superseded::parse_mentions(REPO, &[15], &data("other-bot"));
    assert!(!unknown.own.get(&15).expect("own row").1);
    let fleet = superseded::parse_mentions(REPO, &[15], &data("shipyard-local[bot]"));
    assert!(fleet.own.get(&15).expect("own row").1);
}

#[test]
fn mode_changes_copies_and_unchanged_prove_nothing() {
    for status in ["changed", "copied", "unchanged"] {
        let head = [file("a.sh", status, Some("s1"))];
        let tip = base(&[("a.sh", Some("s1"))]);
        assert!(tree_equal(&head, &tip).is_none(), "{status}");
    }
}

#[test]
fn the_budget_is_asked_before_every_head_and_a_rate_limit_stops_the_check() {
    // Budget: spent after the mention read, so no head is compared.
    let calls = RefCell::new(Vec::new());
    let read = contained_github(queue_facts(false, false, &[]), &calls);
    let asked = RefCell::new(0);
    // Asked before the pass, before the mention batch, then before the head:
    // spent at the third ask, after the mention read.
    let stop = || {
        *asked.borrow_mut() += 1;
        *asked.borrow() > 2
    };
    let mut ledger = StewardLedger::default();
    let mut reports = vec![report(15, "no_verdict")];
    annotate_raw(&read, REPO, "main", &mut ledger, &mut reports, now(), &stop);
    assert!(
        calls.borrow().iter().all(|c| !c.contains("/compare/")),
        "{:?}",
        calls.borrow()
    );
    assert!(
        calls.borrow().iter().any(|c| c.contains("timelineItems")),
        "the mention batch was read before the budget ran out"
    );
    assert!(
        ledger.superseded_checks.is_empty(),
        "an unread head stays due"
    );
    // A rate limit stops the check and is handed back for the cooldown.
    let limited = |_: &[String]| -> Result<String, String> {
        Err("HTTP 403: API rate limit exceeded for installation ID 1".to_owned())
    };
    let mut ledger = StewardLedger::default();
    let mut reports = vec![report(15, "no_verdict"), report(16, "no_verdict")];
    let error = annotate_with(&limited, REPO, "main", &mut ledger, &mut reports, now());
    assert!(error.is_some_and(|e| e.contains("rate limit")));
    assert!(ledger.superseded_checks.is_empty());
}

#[test]
fn mentions_are_read_in_batches_of_ten() {
    let calls = RefCell::new(Vec::new());
    let read = |args: &[String]| -> Result<String, String> {
        calls.borrow_mut().push(args.join(" "));
        Err("not found".to_owned())
    };
    let mut ledger = StewardLedger::default();
    let mut reports: Vec<PrReport> = (1..=12).map(|n| report(n, "no_verdict")).collect();
    annotate_with(&read, REPO, "main", &mut ledger, &mut reports, now());
    let mention_reads = calls
        .borrow()
        .iter()
        .filter(|c| c.contains("timelineItems"))
        .count();
    assert_eq!(mention_reads, 2);
    assert_eq!(
        ledger.superseded_checks.len(),
        12,
        "every head recorded unknown"
    );
}

#[test]
fn a_spent_budget_or_the_kill_switch_reads_nothing() {
    // `annotate` folds `SHIPYARD_STEWARD_SUPERSEDED=off` into `stop`.
    let calls = RefCell::new(Vec::new());
    let read = |args: &[String]| -> Result<String, String> {
        calls.borrow_mut().push(args.join(" "));
        Err("x".to_owned())
    };
    let mut ledger = StewardLedger::default();
    let mut reports = vec![report(15, "no_verdict")];
    annotate_raw(
        &read,
        REPO,
        "main",
        &mut ledger,
        &mut reports,
        now(),
        &|| true,
    );
    assert!(calls.borrow().is_empty());
}

#[cfg(unix)]
fn pass_args(apply: bool) -> StewardCommandArgs {
    StewardCommandArgs {
        repos: vec!["owner/repo".to_owned()],
        base: "main".to_owned(),
        opt_out_label: "steward:skip".to_owned(),
        provenance_blocking_labels: default_provenance_blocking_labels(),
        managed_label: MANAGED_LABEL.to_owned(),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 1,
        recover_hosted_setup_eviction_priority: false,
        coalesce: true,
        preempt_capacity: false,
        max_preemptions_per_head: 1,
        arm_unqueued: false,
        approval_context: crate::merge_steward::DEFAULT_APPROVAL_CONTEXT.to_owned(),
        wake_owner: false,
        unapproved_dequeue: crate::app::merge_steward_cmd::DequeueMode::Report,
        infra_rerun: crate::app::merge_steward_cmd::InfraRerunMode::Off,
        pass_budget: None,
        pass_deadline: None,
        apply,
        ledger: None,
    }
}

/// Only an apply pass (which persists the ledger cache) runs the detector.
#[cfg(unix)]
#[test]
fn only_an_apply_pass_reads_for_superseded_evidence() {
    for apply in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!(
                "printf '%s\\n' \"$*\" >> '{}'\necho \"unexpected: $*\" >&2; exit 90\n",
                log.display()
            ),
        );
        let mut pr = ready_pr();
        "DIRTY".clone_into(&mut pr.fact.merge_state);
        let observation = observation_for(pr, true);
        let control = mutation_control(&temp, "studio", "studio");
        let mut ledger = StewardLedger::default();
        let _ = apply_pr_plans(
            &actions,
            &pass_args(apply),
            &observation,
            &queue_policy(),
            &temp.path().join("ledger.json"),
            &mut ledger,
            apply.then_some(&control),
        );
        let calls = fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            calls.contains("timelineItems"),
            apply,
            "apply={apply}: {calls}"
        );
        assert_eq!(!ledger.superseded_checks.is_empty(), apply, "apply={apply}");
    }
}

#[test]
fn tree_evidence_is_closeable_only_for_a_fleet_pr_that_closes_no_issue() {
    let tree = || superseded::evidence("contained", None, None, "d".to_owned());
    assert!(superseded::gate_close(tree(), "Regenerates labels.", true).closeable);
    for (body, fleet) in [
        ("Fixes #12", true),
        ("closes owner/repo#3", true),
        ("ok", false),
    ] {
        let gated = superseded::gate_close(tree(), body, fleet);
        assert!(!gated.closeable, "{body} {fleet}");
        assert!(gated.not_closeable.is_some());
    }
    let text = superseded::evidence("named_by", Some(16), None, "d".to_owned());
    assert!(
        !superseded::gate_close(text, "", true).closeable,
        "text evidence never closes"
    );
}

#[test]
fn a_named_successor_merged_into_another_base_does_not_count() {
    let reply = |base: &str| {
        format!(
            r#"{{"data":{{"repository":{{"p15":{{"body":"","merged":false,"isInMergeQueue":false,
            "labels":{{"nodes":[{{"name":"superseded"}}]}},
            "comments":{{"nodes":[{{"body":"superseded by #16","authorAssociation":"OWNER"}}]}},
            "queue":{{"nodes":[]}},"timelineItems":{{"nodes":[{{"source":{{"number":16,"merged":true,
            "baseRefName":"{base}","title":"x","body":"",
            "repository":{{"nameWithOwner":"danielraffel/fleet-ssh"}}}}}}]}}}}}}}}}}"#
        )
    };
    for (base, expect) in [("main", true), ("release/1.x", false)] {
        let body = reply(base);
        let read = move |args: &[String]| -> Result<String, String> {
            if args.join(" ").contains("timelineItems") {
                return Ok(body.clone());
            }
            Ok(r#"{"ahead_by":1,"files":[]}"#.to_owned())
        };
        let mut ledger = StewardLedger::default();
        let mut reports = vec![report(15, "no_verdict")];
        annotate_with(&read, REPO, "main", &mut ledger, &mut reports, now());
        assert_eq!(
            reports[0]
                .blocked
                .superseded
                .as_ref()
                .map(|e| e.kind.as_str()),
            expect.then_some("label_successor"),
            "{base}"
        );
    }
}

#[test]
fn a_failed_successor_read_leaves_a_labelled_head_unknown_not_clean() {
    // Labelled, names #16 in a member comment, no cross-reference to #16; the
    // successor read fails (not a rate limit) while the tree looks clean.
    let mentions = r#"{"data":{"repository":{"p15":{"body":"","merged":false,"isInMergeQueue":false,
        "labels":{"nodes":[{"name":"superseded"}]},
        "comments":{"nodes":[{"body":"superseded by #16","authorAssociation":"OWNER"}]},
        "queue":{"nodes":[]},"timelineItems":{"nodes":[]}}}}}"#;
    let read = |args: &[String]| -> Result<String, String> {
        let joined = args.join(" ");
        if joined.contains("timelineItems") {
            return Ok(mentions.to_owned());
        }
        if joined.contains("s16: pullRequest") {
            return Err("graphql 502 bad gateway".to_owned());
        }
        if joined.contains("object(expression:") {
            return Ok(
                r#"{"data":{"repository":{"tip":{"target":{"oid":"c"}},"root":{"__typename":"Tree"},
                "f0":{"__typename":"Blob","oid":"9"}}}}"#
                    .to_owned(),
            );
        }
        Ok(r#"{"ahead_by":1,"base_commit":{"sha":"c"},"files":[{"filename":"a","status":"modified","sha":"1"}]}"#.to_owned())
    };
    let mut ledger = StewardLedger::default();
    let mut reports = vec![report(15, "no_verdict")];
    annotate_with(&read, REPO, "main", &mut ledger, &mut reports, now());
    let check = ledger.superseded_checks.values().next().expect("recorded");
    assert!(
        check.unknown,
        "a failed successor read is unknown, not clean"
    );
    assert!(reports[0].blocked.superseded.is_none());
}
