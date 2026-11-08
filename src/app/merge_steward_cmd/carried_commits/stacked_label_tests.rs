//! A labelled stacked pull request (`stacked:<n>`, opened by
//! `shipyard pr --stacked-on <n>`) is invisible to the carried-commit guard
//! until it is retargeted onto the steward's base.

use std::collections::BTreeSet;

use serde_json::json;

use super::{
    Candidate, CommitList, Judgement, OpenPr, PassCache, carried, refusal_with,
    stacked_label_parent,
};

const SHARED: &str = "5ba4ed0000000000000000000000000000000000";
const OWN: &str = "0e1f000000000000000000000000000000000000";
const Q_HEAD: &str = "9a9a000000000000000000000000000000000000";
const P_HEAD: &str = "7c7c000000000000000000000000000000000000";

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

fn candidate(commits: &BTreeSet<String>) -> Candidate<'_> {
    Candidate {
        number: 11796,
        base_ref: "main",
        head_ref: "feat/independent",
        commits,
    }
}

#[test]
fn a_labelled_stacked_pr_is_never_judged_against_a_main_based_candidate() {
    let own = set(&[SHARED, OWN]);
    assert!(
        carried(
            &candidate(&own),
            &[stacked_q("nodal/handoff-part-2", Some(11828))]
        )
        .is_empty(),
        "a stacked:<n> pull request must not hold a main-based candidate"
    );
}

#[test]
fn control_the_same_stack_without_the_label_is_still_judged() {
    let own = set(&[SHARED, OWN]);
    assert_eq!(
        carried(&candidate(&own), &[stacked_q("nodal/handoff-part-2", None)]).len(),
        1,
        "an unlabelled stack keeps today's carried-commit check"
    );
}

#[test]
fn a_lingering_label_on_a_retargeted_pr_does_not_exempt_it() {
    let own = set(&[SHARED, OWN]);
    assert_eq!(
        carried(&candidate(&own), &[stacked_q("main", Some(11828))]).len(),
        1,
        "once on the steward's base the label no longer counts"
    );
}

fn listing(labels: &[&str], total: usize) -> String {
    json!({"data": {"repository": {"pullRequests": {
        "pageInfo": {"hasNextPage": false, "endCursor": null},
        "nodes": [{
            "number": 11829,
            "headRefName": "nodal/handoff-part-3",
            "baseRefName": "nodal/handoff-part-2",
            "headRefOid": Q_HEAD,
            "isCrossRepository": false,
            "author": {"login": "someone"},
            "labels": {"nodes": labels.iter().map(|name| json!({"name": name})).collect::<Vec<_>>()},
            "carriedCommits": {
                "totalCount": total,
                "nodes": [{"commit": {"oid": SHARED}}, {"commit": {"oid": Q_HEAD}}],
            },
        }],
    }}}})
    .to_string()
}

/// Admission of main-based P through the GitHub reads, with the listing
/// reporting Q under `labels`, P's compare reporting `p_total` commits, and
/// every other read failing (which would refuse P as unreadable).
fn admit(labels: &[&str], p_total: usize, q_total: usize) -> Option<super::Refusal> {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains(&format!("compare/main...{P_HEAD}")) {
            return Ok(json!({"total": p_total, "shas": [SHARED, OWN]}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(listing(labels, q_total));
        }
        Err(format!("unexpected read: {joined}"))
    };
    let mut judge = |_: &OpenPr| Judgement::Unapproved("no APPROVE at this head".to_owned());
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

#[test]
fn the_label_is_read_from_the_listing_and_admits_the_candidate() {
    assert_eq!(admit(&["stacked:11828"], 2, 2), None);
}

#[test]
fn control_without_the_label_the_listing_refuses_the_candidate() {
    let refusal = admit(&["enhancement"], 2, 2).expect("refused");
    assert!(
        refusal
            .token
            .starts_with("carries_unapproved_commits:#11829")
            || refusal.token.starts_with("carried_commits_unreadable"),
        "{refusal:?}"
    );
}

#[test]
fn an_over_cap_candidate_never_reads_a_labelled_stack() {
    // The 2026-11-08 shape: a main-based PR held on a stacked PR's 283
    // commits. P over the cap walks every unrelated PR's own roots; a
    // labelled stack is not one, so no read of it happens and P is admitted.
    assert_eq!(admit(&["stacked:11828"], 2724, 283), None);
    assert!(
        admit(&[], 2724, 283).is_some(),
        "control: the unlabelled over-cap stack still refuses or fails closed"
    );
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
