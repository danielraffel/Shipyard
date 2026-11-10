//! A pull request that targets the base but contains another open pull
//! request's whole head sits above it: the shared commits are the lower pull
//! request's.

use std::cell::Cell;

use serde_json::json;

use super::{Judgement, OpenPr, PassCache, refusal_with};

const PA: &str = "a100000000000000000000000000000000000000";
const PB: &str = "a200000000000000000000000000000000000000";
const QC: &str = "a300000000000000000000000000000000000000";
const PH: &str = PB;
const QH: &str = QC;
const PARENT_BRANCH: &str = "feature/preset-program-map";
const CHILD_BRANCH: &str = "feature/preset-program-wiring";

fn listing(number: u64, branch: &str, head: &str, commits: &[&str]) -> String {
    json!({"data": {"repository": {"pullRequests": {
        "pageInfo": {"hasNextPage": false, "endCursor": null},
        "nodes": [{
            "number": number, "headRefName": branch, "baseRefName": "main",
            "headRefOid": head, "isCrossRepository": false,
            "author": {"login": "someone"},
            "carriedCommits": {"totalCount": commits.len(),
                "nodes": commits.iter().map(|oid| json!({"commit": {"oid": oid}})).collect::<Vec<_>>()},
        }],
    }}}})
    .to_string()
}

/// Run the guard for a candidate whose own range is `own` and whose only
/// open neighbour is `other`. `ancestry` is the compare status of
/// `<other head>...<candidate head>` (`None` makes that read fail).
#[allow(clippy::too_many_arguments)]
fn refuse(
    number: u64,
    branch: &str,
    head: &str,
    own: &[&str],
    other: (u64, &str, &str, &[&str]),
    ancestry: Option<&str>,
    judged: &Cell<usize>,
) -> Option<super::Refusal> {
    let (other_number, other_branch, other_head, other_commits) = other;
    let listed = listing(other_number, other_branch, other_head, other_commits);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains(&format!("compare/main...{head}")) {
            return Ok(json!({"total": own.len(), "shas": own}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(listed.clone());
        }
        if joined.contains(&format!("compare/{other_head}...{head}?per_page=1")) {
            return ancestry.map(str::to_owned).ok_or_else(|| "boom".to_owned());
        }
        Err(format!("unexpected: {joined}"))
    };
    let mut judge = |_: &OpenPr| {
        judged.set(judged.get() + 1);
        Judgement::Unapproved("held".to_owned())
    };
    refusal_with(
        &run_gh,
        "Generous-Corp/pulp",
        "main",
        number,
        branch,
        head,
        &mut PassCache::default(),
        &mut judge,
    )
}

#[test]
fn a_parent_is_admitted_when_a_child_targeting_the_base_contains_its_commits() {
    // The child (#11932) is a PR on main whose history includes every parent
    // commit. The parent's head is an ancestor of the child's: `<child>...<parent>`
    // is `behind`.
    let judged = Cell::new(0);
    let refusal = refuse(
        11930,
        PARENT_BRANCH,
        PH,
        &[PA, PB],
        (11932, CHILD_BRANCH, QH, &[PA, PB, QC]),
        Some("behind"),
        &judged,
    );
    assert_eq!(refusal, None);
    assert_eq!(judged.get(), 0, "the child's approval is never read");
}

#[test]
fn the_child_stays_refused_while_the_parent_is_unmerged() {
    // Candidate is the child; the parent's head is not below... the child's
    // head is not an ancestor of the parent's: `<parent>...<child>` is `ahead`.
    let judged = Cell::new(0);
    let refusal = refuse(
        11932,
        CHILD_BRANCH,
        QH,
        &[PA, PB, QC],
        (11930, PARENT_BRANCH, PH, &[PA, PB]),
        Some("ahead"),
        &judged,
    )
    .expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#11930");
}

#[test]
fn a_candidate_that_merged_a_held_branch_in_is_still_refused() {
    // P merged Q's head: Q's head is an ancestor of P's, so P's head is not
    // an ancestor of Q's (`<q>...<p>` is `ahead`, not `behind`).
    let judged = Cell::new(0);
    let refusal = refuse(
        11930,
        PARENT_BRANCH,
        PH,
        &[PA, PB],
        (11932, CHILD_BRANCH, PA, &[PA]),
        Some("ahead"),
        &judged,
    )
    .expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#11932");
}

#[test]
fn an_unreadable_ancestry_read_keeps_the_refusal() {
    let judged = Cell::new(0);
    let refusal = refuse(
        11930,
        PARENT_BRANCH,
        PH,
        &[PA, PB],
        (11932, CHILD_BRANCH, QH, &[PA, PB, QC]),
        None,
        &judged,
    )
    .expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#11932");
}

#[test]
fn identical_heads_are_not_excused() {
    let judged = Cell::new(0);
    let refusal = refuse(
        11930,
        PARENT_BRANCH,
        PH,
        &[PA, PB],
        (11932, CHILD_BRANCH, PH, &[PA, PB]),
        Some("identical"),
        &judged,
    )
    .expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#11932");
}
