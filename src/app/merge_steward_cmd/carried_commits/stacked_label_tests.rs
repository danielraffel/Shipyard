//! A verified labelled stacked pull request (`stacked:<n>`, opened by
//! `shipyard pr --stacked-on <n>`) with no HOLD at its head is invisible to the
//! carried-commit guard until it is retargeted onto the steward's base.

use std::collections::BTreeSet;

use serde_json::json;

use super::{
    CommitList, Judgement, OpenPr, PassCache, excused_stack, judgement_of, refusal_with,
    stacked_label_parent,
};

const SHARED: &str = "5ba4ed0000000000000000000000000000000000";
const OWN: &str = "0e1f000000000000000000000000000000000000";
const Q_HEAD: &str = "9a9a000000000000000000000000000000000000";
const P_HEAD: &str = "7c7c000000000000000000000000000000000000";
const PARENT_HEAD: &str = "3d3d000000000000000000000000000000000000";
const ON_BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

const PARENT: u64 = 11828;
const PARENT_BRANCH: &str = "nodal/handoff-part-2";

fn set(shas: &[&str]) -> BTreeSet<String> {
    shas.iter().map(|sha| (*sha).to_owned()).collect()
}

fn stacked_q(base_ref: &str, stacked_on: Option<u64>) -> OpenPr {
    OpenPr {
        number: 11829,
        head_ref: "nodal/handoff-part-3".to_owned(),
        base_ref: base_ref.to_owned(),
        head_sha: Q_HEAD.to_owned(),
        cross_repository: false,
        author: None,
        commits: CommitList::Complete(set(&[SHARED, Q_HEAD])),
        stacked_on,
    }
}

fn parent(head_ref: &str) -> OpenPr {
    OpenPr {
        number: PARENT,
        head_ref: head_ref.to_owned(),
        base_ref: "main".to_owned(),
        head_sha: PARENT_HEAD.to_owned(),
        cross_repository: false,
        author: None,
        commits: CommitList::Complete(set(&[PARENT_HEAD])),
        stacked_on: None,
    }
}

fn unapproved() -> Judgement {
    Judgement::Unapproved("no APPROVE at this head".to_owned())
}

#[test]
fn a_verified_stack_without_a_hold_is_excused() {
    let q = stacked_q(PARENT_BRANCH, Some(PARENT));
    let others = [q.clone(), parent(PARENT_BRANCH)];
    assert!(excused_stack(
        &q,
        "main",
        PARENT_BRANCH,
        &others,
        &unapproved()
    ));
}

#[test]
fn control_the_same_stack_without_the_label_is_not_excused() {
    let q = stacked_q(PARENT_BRANCH, None);
    let others = [q.clone(), parent(PARENT_BRANCH)];
    assert!(!excused_stack(
        &q,
        "main",
        PARENT_BRANCH,
        &others,
        &unapproved()
    ));
}

#[test]
fn a_forged_label_is_not_excused_and_the_matching_label_is() {
    // `stacked:1`: no such open pull request.
    let forged = stacked_q(PARENT_BRANCH, Some(1));
    let others = [forged.clone(), parent(PARENT_BRANCH)];
    assert!(!excused_stack(
        &forged,
        "main",
        PARENT_BRANCH,
        &others,
        &unapproved()
    ));
    // `stacked:<n>` naming an open pull request whose head is not Q's base.
    let q = stacked_q(PARENT_BRANCH, Some(PARENT));
    let elsewhere = [q.clone(), parent("feat/unrelated")];
    assert!(!excused_stack(
        &q,
        "main",
        PARENT_BRANCH,
        &elsewhere,
        &unapproved()
    ));
    // Control: the matching label on the same instrument.
    let matching = [q.clone(), parent(PARENT_BRANCH)];
    assert!(excused_stack(
        &q,
        "main",
        PARENT_BRANCH,
        &matching,
        &unapproved()
    ));
    // A parent from a fork is no parent.
    let fork = [
        q.clone(),
        OpenPr {
            cross_repository: true,
            ..parent(PARENT_BRANCH)
        },
    ];
    assert!(!excused_stack(
        &q,
        "main",
        PARENT_BRANCH,
        &fork,
        &unapproved()
    ));
}

#[test]
fn a_hold_or_an_unreadable_judgement_excuses_nothing() {
    let q = stacked_q(PARENT_BRANCH, Some(PARENT));
    let others = [q.clone(), parent(PARENT_BRANCH)];
    assert!(!excused_stack(
        &q,
        "main",
        PARENT_BRANCH,
        &others,
        &Judgement::Held("held by infra-review2".to_owned())
    ));
    assert!(!excused_stack(
        &q,
        "main",
        PARENT_BRANCH,
        &others,
        &Judgement::Unreadable("timeout".to_owned())
    ));
}

#[test]
fn a_lingering_label_on_a_retargeted_pr_does_not_exempt_it() {
    let q = stacked_q("main", Some(PARENT));
    let others = [q.clone(), parent("main")];
    assert!(!excused_stack(
        &q,
        "main",
        PARENT_BRANCH,
        &others,
        &unapproved()
    ));
}

fn node(pr: &OpenPr, labels: &[String], total: usize) -> serde_json::Value {
    let CommitList::Complete(commits) = &pr.commits else {
        panic!("fixture needs commits");
    };
    json!({
        "number": pr.number,
        "headRefName": pr.head_ref,
        "baseRefName": pr.base_ref,
        "headRefOid": pr.head_sha,
        "isCrossRepository": pr.cross_repository,
        "author": {"login": "someone"},
        "labels": {"nodes": labels.iter().map(|name| json!({"name": name})).collect::<Vec<_>>()},
        "carriedCommits": {
            "totalCount": total,
            "nodes": commits.iter().map(|oid| json!({"commit": {"oid": oid}})).collect::<Vec<_>>(),
        },
    })
}

fn listing(label: Option<&str>, q_total: usize) -> String {
    let q = stacked_q(PARENT_BRANCH, None);
    let labels: Vec<String> = label.into_iter().map(str::to_owned).collect();
    json!({"data": {"repository": {"pullRequests": {
        "pageInfo": {"hasNextPage": false, "endCursor": null},
        "nodes": [node(&q, &labels, q_total), node(&parent(PARENT_BRANCH), &[], 1)],
    }}}})
    .to_string()
}

/// Admission of main-based P through the GitHub reads, with the listing
/// labelling Q `label`, P's compare reporting `p_total` commits, Q judged
/// `judgement`, and every other read failing (which would refuse P as
/// unreadable).
fn admit(
    label: Option<&str>,
    p_total: usize,
    q_total: usize,
    judgement: &Judgement,
) -> Option<super::Refusal> {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains(&format!("compare/main...{P_HEAD}")) {
            return Ok(json!({"total": p_total, "shas": [SHARED, OWN]}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(listing(label, q_total));
        }
        // The parent is an ordinary main-based pull request P shares nothing
        // with: its one root is not reachable from P's head.
        if joined.contains(&format!("compare/main...{PARENT_HEAD}")) {
            return Ok(json!({"total": 1, "merge_base": ON_BASE,
                "commits": [{"sha": PARENT_HEAD, "parents": [ON_BASE]}]})
            .to_string());
        }
        if joined.contains(&format!("compare/{P_HEAD}...{PARENT_HEAD}")) {
            return Ok("diverged".to_owned());
        }
        Err(format!("unexpected read: {joined}"))
    };
    let mut judge = |q: &OpenPr| {
        if q.number == PARENT {
            Judgement::Approved
        } else {
            judgement.clone()
        }
    };
    refusal_with(
        &run_gh,
        "Generous-Corp/pulp",
        "main",
        11796,
        "feat/independent",
        P_HEAD,
        &mut PassCache::default(),
        &mut judge,
    )
}

fn refused_on_q(refusal: Option<super::Refusal>) {
    let refusal = refusal.expect("refused");
    assert!(refusal.token.ends_with(":#11829"), "{refusal:?}");
}

#[test]
fn a_verified_label_on_an_unrelated_stack_does_not_exempt_the_candidate() {
    refused_on_q(admit(Some("stacked:11828"), 2, 2, &unapproved()));
}

#[test]
fn control_without_the_label_the_listing_refuses_the_candidate() {
    refused_on_q(admit(Some("enhancement"), 2, 2, &unapproved()));
}

#[test]
fn a_forged_label_in_the_listing_still_refuses() {
    refused_on_q(admit(Some("stacked:1"), 2, 2, &unapproved()));
}

#[test]
fn a_held_stack_in_the_listing_still_refuses() {
    refused_on_q(admit(
        Some("stacked:11828"),
        2,
        2,
        &Judgement::Held("held by infra-review2".to_owned()),
    ));
}

#[test]
fn an_over_cap_candidate_never_reads_a_verified_unheld_stack() {
    // The 2026-11-08 shape: a main-based PR held on a stacked PR's 283
    // commits. P over the cap walks every unrelated PR's own roots; an
    // An unrelated labelled stack is still checked and cannot be skipped.
    refused_on_q(admit(Some("stacked:11828"), 2724, 283, &unapproved()));
    refused_on_q(admit(None, 2724, 283, &unapproved()));
    refused_on_q(admit(
        Some("stacked:11828"),
        2724,
        283,
        &Judgement::Held("held".to_owned()),
    ));
}

#[test]
fn only_a_positive_number_parses_as_a_stacked_label() {
    assert_eq!(stacked_label_parent("stacked:11828"), Some(11828));
    assert_eq!(stacked_label_parent("stacked:"), None);
    assert_eq!(stacked_label_parent("stacked:0"), None);
    assert_eq!(stacked_label_parent("stacked:#12"), None);
    assert_eq!(stacked_label_parent("stacked"), None);
    assert_eq!(stacked_label_parent("unstacked:12"), None);
}

#[test]
fn a_hold_at_the_head_judges_held_and_guard_admission_fails_closed() {
    use crate::head_approval::{ApprovalBlocker, HeadGate};
    let refused = HeadGate::NotApproved {
        head: Q_HEAD.to_owned(),
    };
    let hold = ApprovalBlocker::Held {
        holder: "infra-review2".to_owned(),
        record: "comment",
        id: Some(1),
        at: None,
    };
    assert!(matches!(
        judgement_of(&refused, Some(&hold), true),
        Judgement::Held(_)
    ));
    // Control: no HOLD, comment admission: an ordinary unapproved head.
    assert!(matches!(
        judgement_of(&refused, Some(&ApprovalBlocker::NoVerdict), true),
        Judgement::Unapproved(_)
    ));
    // Guard admission cannot see a HOLD: never excused.
    assert!(matches!(
        judgement_of(&refused, Some(&ApprovalBlocker::NoVerdict), false),
        Judgement::Held(_)
    ));
    assert_eq!(
        judgement_of(&HeadGate::NotRequired, None, false),
        Judgement::Approved
    );
}
