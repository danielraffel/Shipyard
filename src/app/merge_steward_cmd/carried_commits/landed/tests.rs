use std::cell::Cell;
use std::collections::BTreeMap;

use serde_json::json;

use super::{Carried, Judgement, without_landed_commits};
use crate::app::merge_steward_cmd::carried_commits::{CommitList, OpenPr, PassCache, refusal_with};

const TIP: &str = "7100000000000000000000000000000000000000";
const TIP_TREE: &str = "7200000000000000000000000000000000000000";
const MERGE: &str = "0749ffa000000000000000000000000000000000";
const MERGE_TREE: &str = "7300000000000000000000000000000000000000";
const FIRST: &str = "7400000000000000000000000000000000000000";
const FIRST_TREE: &str = "7500000000000000000000000000000000000000";
const PICK: &str = "7600000000000000000000000000000000000000";
const MAIN_COPY: &str = "7700000000000000000000000000000000000000";
const PARENT: &str = "7800000000000000000000000000000000000000";
const Q_HEAD: &str = "7900000000000000000000000000000000000000";

/// An in-memory repository answering the reads `landed` makes, in the shape
/// its `--jq` filters leave them.
#[derive(Default)]
struct Fake {
    /// commit sha -> (tree, parents, author date)
    commits: BTreeMap<String, (String, Vec<String>, String)>,
    /// tree sha -> entries (name, mode, type, sha)
    trees: BTreeMap<String, Vec<(String, String, String, String)>>,
    diffs: BTreeMap<String, String>,
    /// single-parent tip commits listed for any path
    tip_listing: Vec<String>,
    fail_on: Option<&'static str>,
    calls: Cell<usize>,
}

impl Fake {
    fn commit(&mut self, sha: &str, tree: &str, parents: &[&str]) {
        self.commits.insert(
            sha.to_owned(),
            (
                tree.to_owned(),
                parents.iter().map(|p| (*p).to_owned()).collect(),
                "2026-11-01T00:00:00Z".to_owned(),
            ),
        );
    }

    fn tree(&mut self, sha: &str, entries: &[(&str, &str, &str, &str)]) {
        self.trees.insert(
            sha.to_owned(),
            entries
                .iter()
                .map(|(n, m, k, s)| {
                    (
                        (*n).to_owned(),
                        (*m).to_owned(),
                        (*k).to_owned(),
                        (*s).to_owned(),
                    )
                })
                .collect(),
        );
    }

    fn run(&self, argv: &[String]) -> Result<String, String> {
        self.calls.set(self.calls.get() + 1);
        let joined = argv.join(" ");
        if self.fail_on.is_some_and(|needle| joined.contains(needle)) {
            return Err("boom".to_owned());
        }
        if joined.contains("git/ref/heads/main") {
            return Ok(json!({"sha": TIP, "kind": "commit"}).to_string());
        }
        if let Some(sha) = joined.split("git/commits/").nth(1) {
            let sha = sha.split_whitespace().next().unwrap_or_default();
            let (tree, parents, date) = self.commits.get(sha).ok_or("no such commit")?;
            return Ok(json!({"tree": tree, "parents": parents, "date": date}).to_string());
        }
        if let Some(sha) = joined.split("git/trees/").nth(1) {
            let sha = sha.split_whitespace().next().unwrap_or_default();
            let entries = self.trees.get(sha).ok_or("no such tree")?;
            let tree: Vec<_> = entries
                .iter()
                .map(|(n, m, k, s)| json!({"name": n, "mode": m, "kind": k, "sha": s}))
                .collect();
            return Ok(json!({"truncated": false, "tree": tree}).to_string());
        }
        if joined.contains("commits?sha=") {
            return Ok(json!(self.tip_listing).to_string());
        }
        if joined.contains("vnd.github.diff") {
            let sha = joined.rsplit("/commits/").next().unwrap_or_default();
            return self
                .diffs
                .get(sha)
                .cloned()
                .ok_or_else(|| "no diff".to_owned());
        }
        Err(format!("unexpected: {joined}"))
    }
}

fn shares(sha: &str) -> (Carried, Judgement) {
    (
        Carried::Shares {
            number: 11772,
            head_sha: Q_HEAD.to_owned(),
            shared: vec![sha.to_owned()],
        },
        Judgement::Unapproved("no approval".to_owned()),
    )
}

fn check(fake: &Fake, sha: &str) -> Option<(Carried, Judgement)> {
    without_landed_commits(&|argv| fake.run(argv), "o/r", "main", shares(sha))
}

/// Main has a.rs (blob A) and d/b.rs (blob B); the merge brings the same two
/// files to a first parent that had older versions: the #11773 shape.
fn merge_fake(merge_a: &str) -> Fake {
    let mut fake = Fake::default();
    fake.commit(TIP, TIP_TREE, &[PARENT]);
    fake.tree(
        TIP_TREE,
        &[
            ("a.rs", "100644", "blob", "aaaa"),
            ("d", "040000", "tree", "dtip"),
        ],
    );
    fake.tree("dtip", &[("b.rs", "100644", "blob", "bbbb")]);
    fake.commit(FIRST, FIRST_TREE, &[PARENT]);
    fake.tree(
        FIRST_TREE,
        &[
            ("a.rs", "100644", "blob", "a0a0"),
            ("d", "040000", "tree", "dold"),
        ],
    );
    fake.tree("dold", &[("b.rs", "100644", "blob", "b0b0")]);
    fake.commit(
        MERGE,
        MERGE_TREE,
        &[FIRST, "7a00000000000000000000000000000000000000"],
    );
    fake.tree(
        MERGE_TREE,
        &[
            ("a.rs", "100644", "blob", merge_a),
            ("d", "040000", "tree", "dmerge"),
        ],
    );
    fake.tree("dmerge", &[("b.rs", "100644", "blob", "bbbb")]);
    fake
}

#[test]
fn a_merge_carrying_only_landed_content_is_admitted() {
    assert_eq!(check(&merge_fake("aaaa"), MERGE), None);
}

#[test]
fn the_same_merge_with_one_extra_line_is_refused() {
    let fake = merge_fake("a1a1");
    assert_eq!(check(&fake, MERGE), Some(shares(MERGE)));
}

#[test]
fn a_merge_whose_mode_differs_from_the_tip_is_refused() {
    let mut fake = merge_fake("aaaa");
    fake.tree(
        MERGE_TREE,
        &[
            ("a.rs", "100755", "blob", "aaaa"),
            ("d", "040000", "tree", "dmerge"),
        ],
    );
    assert_eq!(check(&fake, MERGE), Some(shares(MERGE)));
}

#[test]
fn a_merge_that_removes_a_path_the_tip_still_has_is_refused() {
    let mut fake = merge_fake("aaaa");
    fake.tree(MERGE_TREE, &[("d", "040000", "tree", "dmerge")]);
    assert_eq!(check(&fake, MERGE), Some(shares(MERGE)));
}

#[test]
fn a_merge_that_removes_a_path_the_tip_also_lacks_is_admitted() {
    let mut fake = merge_fake("aaaa");
    fake.tree(TIP_TREE, &[("d", "040000", "tree", "dtip")]);
    fake.tree(MERGE_TREE, &[("d", "040000", "tree", "dmerge")]);
    assert_eq!(check(&fake, MERGE), None);
}

#[test]
fn a_merge_in_a_new_directory_is_compared_to_the_tips_directory() {
    let mut fake = merge_fake("aaaa");
    fake.tree(FIRST_TREE, &[("a.rs", "100644", "blob", "a0a0")]);
    assert_eq!(check(&fake, MERGE), None);
    fake.tree("dmerge", &[("b.rs", "100644", "blob", "b1b1")]);
    assert_eq!(check(&fake, MERGE), Some(shares(MERGE)));
}

const DIFF: &str = "diff --git a/a.rs b/a.rs\nindex 1111111..2222222 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -3,1 +3,2 @@\n x\n+y\n";
const DIFF_MOVED: &str = "diff --git a/a.rs b/a.rs\nindex 3333333..4444444 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -90,1 +90,2 @@\n x\n+y\n";
const DIFF_OTHER: &str = "diff --git a/a.rs b/a.rs\nindex 1111111..2222222 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -3,1 +3,2 @@\n x\n+z\n";

fn pick_fake(on_main: &str) -> Fake {
    let mut fake = Fake::default();
    fake.commit(TIP, TIP_TREE, &[PARENT]);
    fake.commit(PICK, "7b00000000000000000000000000000000000000", &[PARENT]);
    fake.commit(
        MAIN_COPY,
        "7c00000000000000000000000000000000000000",
        &[PARENT],
    );
    fake.diffs.insert(PICK.to_owned(), DIFF.to_owned());
    fake.diffs.insert(MAIN_COPY.to_owned(), on_main.to_owned());
    fake.tip_listing = vec![MAIN_COPY.to_owned()];
    fake.tree(TIP_TREE, &[]);
    fake
}

#[test]
fn a_cherry_pick_with_the_patch_id_of_a_commit_on_main_is_admitted() {
    assert_eq!(check(&pick_fake(DIFF_MOVED), PICK), None);
}

#[test]
fn a_commit_whose_patch_differs_from_main_is_refused() {
    assert_eq!(check(&pick_fake(DIFF_OTHER), PICK), Some(shares(PICK)));
}

#[test]
fn a_commit_with_no_equivalent_on_main_is_refused() {
    let mut fake = pick_fake(DIFF_MOVED);
    fake.tip_listing.clear();
    assert_eq!(check(&fake, PICK), Some(shares(PICK)));
}

/// Every read a commit kind makes, failed one at a time, must refuse.
#[test]
fn a_git_error_refuses() {
    let merge_reads = ["git/ref/heads", "git/commits/", "git/trees/"];
    let pick_reads = [
        "git/ref/heads",
        "git/commits/",
        "vnd.github.diff",
        "commits?sha=",
    ];
    for needle in merge_reads {
        let mut fake = merge_fake("aaaa");
        fake.fail_on = Some(needle);
        assert!(
            check(&fake, MERGE).is_some(),
            "merge admitted despite {needle} failing"
        );
    }
    for needle in pick_reads {
        let mut fake = pick_fake(DIFF_MOVED);
        fake.fail_on = Some(needle);
        assert!(
            check(&fake, PICK).is_some(),
            "pick admitted despite {needle} failing"
        );
    }
}

#[test]
fn an_approved_overlap_and_an_unbounded_one_are_not_examined() {
    let fake = merge_fake("a1a1");
    let approved = (shares(MERGE).0, Judgement::Approved);
    assert_eq!(
        without_landed_commits(&|argv| fake.run(argv), "o/r", "main", approved.clone()),
        Some(approved)
    );
    let unbounded = (
        Carried::Unbounded {
            number: 1,
            total: 400,
        },
        Judgement::Unapproved("x".to_owned()),
    );
    assert_eq!(
        without_landed_commits(&|argv| fake.run(argv), "o/r", "main", unbounded.clone()),
        Some(unbounded)
    );
    assert_eq!(fake.calls.get(), 0);
}

#[test]
fn an_orphan_or_empty_commit_is_refused() {
    let mut fake = pick_fake(DIFF_MOVED);
    fake.commit(PICK, "7b00000000000000000000000000000000000000", &[]);
    assert_eq!(check(&fake, PICK), Some(shares(PICK)));
    let mut fake = pick_fake(DIFF_MOVED);
    fake.diffs.insert(PICK.to_owned(), String::new());
    assert_eq!(check(&fake, PICK), Some(shares(PICK)));
}

#[test]
fn the_guard_admits_the_landed_merge_shape_and_refuses_it_with_one_extra_line() {
    let q = OpenPr {
        number: 11772,
        head_ref: "feature/platform-async-menu".to_owned(),
        base_ref: "main".to_owned(),
        head_sha: Q_HEAD.to_owned(),
        cross_repository: false,
        author: None,
        commits: CommitList::Complete(std::iter::once(MERGE.to_owned()).collect()),
        stacked_on: None,
    };
    let listing = json!({"data": {"repository": {"pullRequests": {
        "pageInfo": {"hasNextPage": false, "endCursor": null},
        "nodes": [{
            "number": 11772, "headRefName": q.head_ref, "baseRefName": "main",
            "headRefOid": Q_HEAD, "isCrossRepository": false,
            "author": {"login": "someone"},
            "carriedCommits": {"totalCount": 1, "nodes": [{"commit": {"oid": MERGE}}]},
        }],
    }}}})
    .to_string();
    for (merge_a, admitted) in [("aaaa", true), ("a1a1", false)] {
        let fake = merge_fake(merge_a);
        let run_gh = |argv: &[String]| -> Result<String, String> {
            let joined = argv.join(" ");
            if joined.contains("compare/main...") {
                return Ok(json!({"total": 1, "shas": [MERGE]}).to_string());
            }
            if joined.contains("OPEN") {
                return Ok(listing.clone());
            }
            fake.run(argv)
        };
        let mut judge = |_: &OpenPr| Judgement::Unapproved("not approved".to_owned());
        let refusal = refusal_with(
            &run_gh,
            "o/r",
            "main",
            11773,
            "feature/platform-async-menu-b",
            MERGE,
            &mut PassCache::default(),
            &mut judge,
        );
        if admitted {
            assert_eq!(refusal, None);
        } else {
            let refusal = refusal.expect("refused");
            assert_eq!(refusal.token, "carries_unapproved_commits:#11772");
        }
    }
}
