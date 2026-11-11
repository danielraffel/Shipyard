//! Deterministic superseded evidence, reported in shadow mode.
use std::cell::RefCell;

use super::*;
use crate::app::merge_steward_cmd::blocked_on::Stall;
use crate::app::merge_steward_cmd::superseded::{
    self, FileChange, Mention, annotate_with, label_successor, named_by, named_successors,
    superseded_numbers, tree_equal,
};

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
        text: text.to_owned(),
    }
}

#[test]
fn named_by_needs_a_merged_pr_in_the_same_repo() {
    let open = [mention(16, false, true, "Supersedes #15")];
    assert_eq!(named_by(15, &open), None, "not merged yet");
    let other_repo = [mention(16, true, false, "Supersedes #15")];
    assert_eq!(named_by(15, &other_repo), None);
    let merged = [mention(16, true, true, "Supersedes #15")];
    assert_eq!(named_by(15, &merged).map(|(n, _)| n), Some(16));
    let mere_mention = [mention(16, true, true, "Related to #15")];
    assert_eq!(named_by(15, &mere_mention), None);
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
                r#"{"data":{"repository":{"p15":{"body":"","labels":{"nodes":[]},
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
                r#"{{"data":{{"repository":{{"tip":{{"target":{{"oid":"cafe1111"}}}},
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
            r#"{"data":{"repository":{"p15":{"body":"","labels":{"nodes":[]},
            "comments":{"nodes":[]},"timelineItems":{"nodes":[{"source":{"number":16,
            "merged":true,"title":"labels","body":"Supersedes #15",
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
