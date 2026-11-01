use std::cell::Cell;
use std::collections::BTreeSet;

use serde_json::json;

use super::{
    Candidate, Carried, CommitList, Judgement, OpenPr, PassCache, carried, decide, refusal_with,
};

// The 2026-11-01 incident: #11361 (t2) held at 883bb3f6; #11369 (t8) merged t3,
// which carried #11361's c26a23be and a0148424.
const HELD_A: &str = "c26a23be00000000000000000000000000000000";
const HELD_B: &str = "a014842400000000000000000000000000000000";
const HELD_HEAD: &str = "883bb3f600000000000000000000000000000000";
const OWN: &str = "f35930c500000000000000000000000000000000";
const ON_BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn set(shas: &[&str]) -> BTreeSet<String> {
    shas.iter().map(|sha| (*sha).to_owned()).collect()
}

fn open(number: u64, head_ref: &str, base_ref: &str, head: &str, shas: &[&str]) -> OpenPr {
    OpenPr {
        number,
        head_ref: head_ref.to_owned(),
        base_ref: base_ref.to_owned(),
        head_sha: head.to_owned(),
        cross_repository: false,
        author: None,
        commits: CommitList::Complete(set(shas)),
    }
}

fn held() -> OpenPr {
    open(
        11361,
        "feat/real-world-inputs-t2",
        "main",
        HELD_HEAD,
        &[HELD_A, HELD_B, HELD_HEAD],
    )
}

fn candidate(commits: &BTreeSet<String>) -> Candidate<'_> {
    Candidate {
        number: 11369,
        base_ref: "main",
        head_ref: "feat/real-world-inputs-t8",
        head_sha: OWN,
        commits,
    }
}

fn judge_all(judgement: &Judgement, overlaps: Vec<Carried>) -> Vec<(Carried, Judgement)> {
    overlaps
        .into_iter()
        .map(|overlap| (overlap, judgement.clone()))
        .collect()
}

#[test]
fn incident_shape_a_head_carrying_a_held_prs_commits_is_refused() {
    let own = set(&[HELD_A, HELD_B, OWN]);
    let overlaps = carried(&candidate(&own), &[held()]);
    assert_eq!(
        overlaps,
        vec![Carried::Shares {
            number: 11361,
            head_sha: HELD_HEAD.to_owned(),
            shared: vec![HELD_B.to_owned(), HELD_A.to_owned()],
        }]
    );
    let refusal = decide(
        "main",
        &judge_all(&Judgement::Unapproved("held".to_owned()), overlaps),
    )
    .expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#11361");
    assert!(refusal.detail.contains(HELD_A), "{}", refusal.detail);
    assert!(refusal.detail.contains(HELD_B), "{}", refusal.detail);
    assert!(refusal.detail.contains(HELD_HEAD), "{}", refusal.detail);
}

#[test]
fn a_carried_pr_approved_at_its_head_allows_admission() {
    let own = set(&[HELD_A, HELD_B, OWN]);
    let overlaps = carried(&candidate(&own), &[held()]);
    assert_eq!(overlaps.len(), 1);
    assert_eq!(
        decide("main", &judge_all(&Judgement::Approved, overlaps)),
        None
    );
}

#[test]
fn no_overlap_allows_admission() {
    let own = set(&[OWN]);
    assert!(carried(&candidate(&own), &[held()]).is_empty());
    assert_eq!(decide("main", &[]), None);
}

#[test]
fn shared_commits_already_on_the_base_allow_admission() {
    // Q's listing still names a commit the base now has; P's compare set,
    // being base-relative, does not, so there is nothing to carry.
    let q = open(7, "feature/q", "main", HELD_HEAD, &[ON_BASE, HELD_HEAD]);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [OWN]}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(listing(&[(&q, 2)]).to_string());
        }
        Err(format!("unexpected: {joined}"))
    };
    let judged = Cell::new(0);
    let mut judge = |_: &OpenPr| {
        judged.set(judged.get() + 1);
        Judgement::Unapproved("held".to_owned())
    };
    let refusal = refusal_with(
        &run_gh,
        "owner/repo",
        "main",
        11369,
        "feat/real-world-inputs-t8",
        OWN,
        &mut PassCache::default(),
        &mut judge,
    );
    assert_eq!(refusal, None);
    assert_eq!(judged.get(), 0, "no overlap reads no approval");
}

#[test]
fn the_candidates_own_base_branch_pr_and_a_pr_stacked_on_it_never_block() {
    let own = set(&[HELD_A, OWN]);
    let base_pr = open(1, "main", "release", HELD_HEAD, &[HELD_A]);
    let stacked_on_p = open(
        2,
        "feat/next",
        "feat/real-world-inputs-t8",
        HELD_HEAD,
        &[OWN],
    );
    assert!(carried(&candidate(&own), &[base_pr.clone(), stacked_on_p]).is_empty());
    // A fork's branch that happens to be named like the base is not it.
    let fork = OpenPr {
        cross_repository: true,
        ..base_pr
    };
    assert_eq!(carried(&candidate(&own), &[fork]).len(), 1);
}

#[test]
fn an_unbounded_pr_fails_closed_unless_approved() {
    let own = set(&[OWN]);
    let big = OpenPr {
        commits: CommitList::OverCap { total: 300 },
        ..held()
    };
    let overlaps = carried(&candidate(&own), &[big]);
    let refusal = decide(
        "main",
        &judge_all(&Judgement::Unapproved("held".to_owned()), overlaps.clone()),
    )
    .expect("refused");
    assert_eq!(refusal.token, "carried_commits_over_cap:#11361");
    assert_eq!(
        decide("main", &judge_all(&Judgement::Approved, overlaps)),
        None
    );
}

#[test]
fn an_unreadable_approval_fails_closed() {
    let own = set(&[HELD_A, OWN]);
    let overlaps = carried(&candidate(&own), &[held()]);
    let refusal = decide(
        "main",
        &judge_all(&Judgement::Unreadable("rate limited".to_owned()), overlaps),
    )
    .expect("refused");
    assert_eq!(refusal.token, "carried_commits_unreadable:#11361");
}

fn listing(prs: &[(&OpenPr, usize)]) -> serde_json::Value {
    let nodes = prs
        .iter()
        .map(|(pr, total)| {
            let CommitList::Complete(commits) = &pr.commits else {
                panic!("listing fixture needs commits");
            };
            json!({
                "number": pr.number,
                "headRefName": pr.head_ref,
                "baseRefName": pr.base_ref,
                "headRefOid": pr.head_sha,
                "isCrossRepository": pr.cross_repository,
                "author": {"login": "someone"},
                "carriedCommits": {
                    "totalCount": total,
                    "nodes": commits.iter().map(|oid| json!({"commit": {"oid": oid}})).collect::<Vec<_>>(),
                },
            })
        })
        .collect::<Vec<_>>();
    json!({"data": {"repository": {"pullRequests": {
        "pageInfo": {"hasNextPage": false, "endCursor": null},
        "nodes": nodes,
    }}}})
}

#[test]
fn the_incident_through_github_reads_is_refused_and_reads_are_cached_per_pass() {
    let q = held();
    let listings = Cell::new(0);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 3, "shas": [HELD_A, HELD_B, OWN]}).to_string());
        }
        if joined.contains("OPEN") {
            listings.set(listings.get() + 1);
            return Ok(listing(&[(&q, 3)]).to_string());
        }
        Err(format!("unexpected: {joined}"))
    };
    let judged = Cell::new(0);
    let mut judge = |pr: &OpenPr| {
        judged.set(judged.get() + 1);
        assert_eq!(pr.head_sha, HELD_HEAD);
        Judgement::Unapproved("a HOLD names this head".to_owned())
    };
    let mut cache = PassCache::default();
    for number in [11369, 11370] {
        let refusal = refusal_with(
            &run_gh,
            "Generous-Corp/pulp",
            "main",
            number,
            "feat/real-world-inputs-t8",
            OWN,
            &mut cache,
            &mut judge,
        )
        .expect("refused");
        assert_eq!(refusal.token, "carries_unapproved_commits:#11361");
    }
    assert_eq!(listings.get(), 1, "open pull requests read once per pass");
    assert_eq!(judged.get(), 1, "a carried head judged once per pass");
}

#[test]
fn a_candidate_over_the_commit_cap_fails_closed() {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 400, "shas": [OWN]}).to_string());
        }
        Err(format!("unexpected: {joined}"))
    };
    let refusal = refusal_with(
        &run_gh,
        "owner/repo",
        "main",
        11369,
        "feat/real-world-inputs-t8",
        OWN,
        &mut PassCache::default(),
        &mut |_| Judgement::Approved,
    )
    .expect("refused");
    assert_eq!(refusal.token, "carried_commits_over_cap:#11369");
}

#[test]
fn an_unreadable_listing_fails_closed() {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [OWN]}).to_string());
        }
        Err("HTTP 502".to_owned())
    };
    let refusal = refusal_with(
        &run_gh,
        "owner/repo",
        "main",
        11369,
        "feat/real-world-inputs-t8",
        OWN,
        &mut PassCache::default(),
        &mut |_| Judgement::Approved,
    )
    .expect("refused");
    assert_eq!(refusal.token, "carried_commits_unreadable");
    assert!(refusal.detail.contains("HTTP 502"), "{}", refusal.detail);
}

#[test]
fn a_rollup_that_merged_the_candidate_does_not_block_it_but_any_other_rollup_does() {
    let own = set(&[OWN]);
    let rollup = open(
        9,
        &format!("{}9", crate::rollup::BRANCH_PREFIX),
        "main",
        HELD_HEAD,
        &[OWN, HELD_A, HELD_HEAD],
    );
    assert!(carried(&candidate(&own), std::slice::from_ref(&rollup)).is_empty());
    // A rollup branch that does not hold P's head is judged like any Q.
    let own = set(&[HELD_A, "dddddddddddddddddddddddddddddddddddddddd"]);
    let other = Candidate {
        head_sha: "dddddddddddddddddddddddddddddddddddddddd",
        ..candidate(&own)
    };
    assert_eq!(carried(&other, &[rollup]).len(), 1);
}
