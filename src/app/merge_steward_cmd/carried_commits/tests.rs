use std::cell::Cell;
use std::collections::BTreeSet;

use serde_json::json;

use super::{
    Candidate, Carried, CommitList, Judgement, OpenPr, PassCache, carried, decide,
    live_head_matches, refusal_with,
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
        stacked_on: None,
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
fn the_incident_through_github_reads_is_refused_and_revalidates_each_candidate() {
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
    assert_eq!(
        judged.get(),
        2,
        "a carried head is revalidated for each candidate"
    );
}

#[test]
fn a_carried_approval_is_not_reused_after_a_later_candidate_sees_a_new_verdict() {
    let q = held();
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 3, "shas": [HELD_A, HELD_B, OWN]}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(listing(&[(&q, 3)]).to_string());
        }
        Err(format!("unexpected: {joined}"))
    };
    let judged = Cell::new(0);
    let mut judge = |_: &OpenPr| {
        let call = judged.get();
        judged.set(call + 1);
        if call == 0 {
            Judgement::Approved
        } else {
            Judgement::Unapproved("the carried head moved after the first candidate".to_owned())
        }
    };
    let mut cache = PassCache::default();
    assert_eq!(
        refusal_with(
            &run_gh,
            "owner/repo",
            "main",
            11369,
            "feat/real-world-inputs-t8",
            OWN,
            &mut cache,
            &mut judge,
        ),
        None
    );
    let refusal = refusal_with(
        &run_gh,
        "owner/repo",
        "main",
        11370,
        "feat/real-world-inputs-t8",
        OWN,
        &mut cache,
        &mut judge,
    )
    .expect("the later candidate must not reuse the stale approval");
    assert_eq!(refusal.token, "carries_unapproved_commits:#11361");
    assert_eq!(judged.get(), 2);
}

#[test]
fn a_candidate_over_the_commit_cap_with_an_unreadable_listing_fails_closed() {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 400, "shas": [OWN]}).to_string());
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
fn a_rollup_shaped_branch_is_checked_like_any_other_carried_pr() {
    let own = set(&[OWN]);
    let rollup = open(
        9,
        &format!("{}9", crate::rollup::BRANCH_PREFIX),
        "main",
        HELD_HEAD,
        &[OWN, HELD_A, HELD_HEAD],
    );
    assert_eq!(
        carried(&candidate(&own), std::slice::from_ref(&rollup)).len(),
        1
    );
    // A spoofed rollup prefix receives the same check.
    let spoofed = open(
        10,
        "shipyard-train/rollup-spoof",
        "main",
        HELD_HEAD,
        &[OWN, HELD_A, HELD_HEAD],
    );
    assert_eq!(carried(&candidate(&own), &[spoofed]).len(), 1);
}

#[test]
fn a_carried_pr_pushed_after_the_listing_is_not_judged_at_its_listed_head() {
    let live = |head: Option<&str>| json!({"data": {"repository": {"pullRequest": {"number": 11361, "headRefOid": head}}}});
    assert_eq!(live_head_matches(&live(Some(HELD_HEAD)), &held()), Ok(()));
    let moved = live_head_matches(&live(Some(OWN)), &held()).expect_err("moved head");
    assert!(moved.contains(HELD_HEAD) && moved.contains(OWN), "{moved}");
    assert!(live_head_matches(&live(None), &held()).is_err());
}

// 2026-11-01 stack: #11363 (t4, base main) is P; #11364 (t6) is on t4; #11365,
// #11367 and #11368 are on t6. 9c34474f is P's own commit that every child
// inherits.
const P_OWN: &str = "9c34474f00000000000000000000000000000000";
const T6_OWN: &str = "66666666000000000000000000000000000000aa";
const CHILD_OWN: &str = "cccccccc00000000000000000000000000000000";
const P_HEAD_BRANCH: &str = "feat/real-world-inputs-t4";

fn stack() -> Vec<OpenPr> {
    vec![
        open(
            11364,
            "feat/real-world-inputs-t6",
            P_HEAD_BRANCH,
            "t6head",
            &[P_OWN, T6_OWN],
        ),
        open(
            11365,
            "feat/c5",
            "feat/real-world-inputs-t6",
            "c5head",
            &[P_OWN, T6_OWN, CHILD_OWN],
        ),
        open(
            11367,
            "feat/c7",
            "feat/real-world-inputs-t6",
            "c7head",
            &[P_OWN, T6_OWN],
        ),
        open(
            11368,
            "feat/c8",
            "feat/real-world-inputs-t6",
            "c8head",
            &[P_OWN, T6_OWN],
        ),
    ]
}

fn p_candidate(commits: &BTreeSet<String>) -> Candidate<'_> {
    Candidate {
        number: 11363,
        base_ref: "main",
        head_ref: P_HEAD_BRANCH,
        commits,
    }
}

#[test]
fn transitive_descendants_of_the_candidate_never_block_it() {
    let own = set(&[P_OWN]);
    assert!(carried(&p_candidate(&own), &stack()).is_empty());
}

#[test]
fn a_cycle_in_base_chains_ends_the_walk() {
    let own = set(&[P_OWN]);
    let a = open(1, "a", "b", "ah", &[P_OWN]);
    let b = open(2, "b", "a", "bh", &[P_OWN]);
    assert_eq!(carried(&p_candidate(&own), &[a, b]).len(), 2);
}

#[test]
fn the_live_stack_through_github_reads_admits_the_origin_pr() {
    let others = stack();
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [P_OWN]}).to_string());
        }
        if joined.contains("OPEN") {
            let refs: Vec<(&OpenPr, usize)> = others
                .iter()
                .map(|pr| {
                    let CommitList::Complete(c) = &pr.commits else {
                        unreachable!()
                    };
                    (pr, c.len())
                })
                .collect();
            return Ok(listing(&refs).to_string());
        }
        Err(format!("unexpected: {joined}"))
    };
    let mut judge = |_: &OpenPr| Judgement::Unapproved("no APPROVE".to_owned());
    let refusal = refusal_with(
        &run_gh,
        "Generous-Corp/pulp",
        "main",
        11363,
        P_HEAD_BRANCH,
        P_OWN,
        &mut PassCache::default(),
        &mut judge,
    );
    assert_eq!(refusal, None);
}

#[test]
fn an_inheritor_not_in_the_chain_is_cleared_by_its_own_diff() {
    // Q is on base X (not P's chain) and lists P's commit, but X already has
    // it, so Q's own diff against X does not: Q merely inherits it.
    let q = open(20, "feat/q", "feat/x", "qhead", &[P_OWN, CHILD_OWN]);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [P_OWN]}).to_string());
        }
        if joined.contains("compare/feat/x...qhead") {
            return Ok(graph(&[(CHILD_OWN, &["feat-x-tip"])]));
        }
        if joined.contains("OPEN") {
            return Ok(listing(&[(&q, 2)]).to_string());
        }
        Err(format!("unexpected: {joined}"))
    };
    let mut judge = |_: &OpenPr| Judgement::Unapproved("held".to_owned());
    let refusal = refusal_with(
        &run_gh,
        "o/r",
        "main",
        11363,
        P_HEAD_BRANCH,
        P_OWN,
        &mut PassCache::default(),
        &mut judge,
    );
    assert_eq!(refusal, None);
}

// First-parent attribution. On 2026-11-08 #11765, #11760 and #11750 (each
// based on main) were held by #11772, #11780 and #11792: each stacked Q had
// merged P's branch without being stacked on P, so every shared commit sat on
// P's first-parent chain and reached Q only through a merge's second parent.
const FP_MAIN: &str = "f0000000000000000000000000000000000000aa";
const FP_P1: &str = "f1000000000000000000000000000000000000aa";
const FP_P2: &str = "f2000000000000000000000000000000000000aa";
const FP_Q1: &str = "f3000000000000000000000000000000000000aa";
const FP_QM: &str = "f4000000000000000000000000000000000000aa";
const FP_PM: &str = "f5000000000000000000000000000000000000aa";
const FP_H1: &str = "f6000000000000000000000000000000000000aa";
const FP_H2: &str = "f7000000000000000000000000000000000000aa";

type Graph<'a> = &'a [(&'a str, &'a [&'a str])];

/// P (#11363, base main) at `p_head` with `p_graph` as `main...p_head`, and
/// `others`, each with its own `q.base...q.head` graph. A compare asking for
/// parents is served the graph; any other compare its shas.
fn first_parent_refusal(
    p_head: &str,
    p_graph: Graph<'_>,
    others: &[(&OpenPr, Graph<'_>)],
) -> Option<super::Refusal> {
    let listed = listing(
        &others
            .iter()
            .map(|(q, _)| {
                let CommitList::Complete(c) = &q.commits else {
                    unreachable!()
                };
                (*q, c.len())
            })
            .collect::<Vec<_>>(),
    )
    .to_string();
    let mut compares = vec![("main".to_owned(), p_head.to_owned(), p_graph)];
    for (q, g) in others {
        compares.push((q.base_ref.clone(), q.head_sha.clone(), *g));
    }
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("OPEN") {
            return Ok(listed.clone());
        }
        for (base, head, g) in &compares {
            if joined.contains(&format!("compare/{base}...{head}?")) {
                if joined.contains("parents") {
                    return Ok(graph(g));
                }
                let shas: Vec<&str> = g.iter().map(|(sha, _)| *sha).collect();
                return Ok(json!({"total": shas.len(), "shas": shas}).to_string());
            }
        }
        Err(format!("unexpected: {joined}"))
    };
    let mut judge = |_: &OpenPr| Judgement::Unapproved("no counted APPROVE".to_owned());
    refusal_with(
        &run_gh,
        "Generous-Corp/pulp",
        "main",
        11363,
        P_HEAD_BRANCH,
        p_head,
        &mut PassCache::default(),
        &mut judge,
    )
}

const P_LINE: Graph<'static> = &[(FP_P1, &[FP_MAIN]), (FP_P2, &[FP_P1])];

#[test]
fn a_stacked_pr_that_merged_the_candidates_branch_does_not_hold_it() {
    // Q (base feat/x) merged P's branch: P1 and P2 are on P's first-parent
    // chain and reached Q only as QM's second parent.
    let q = open(21, "feat/q2", "feat/x", FP_QM, &[FP_P1, FP_P2, FP_QM]);
    let q_graph: Graph<'_> = &[
        (FP_P1, &[FP_MAIN]),
        (FP_P2, &[FP_P1]),
        (FP_QM, &["feat-x-tip", FP_P2]),
    ];
    assert_eq!(first_parent_refusal(FP_P2, P_LINE, &[(&q, q_graph)]), None);

    // A refusal P still gets from another pull request says why Q is not in it.
    let held = open(30, "feat/held", "main", FP_H2, &[FP_H1, FP_H2]);
    let held_graph: Graph<'_> = &[(FP_H1, &[FP_MAIN]), (FP_H2, &[FP_H1])];
    let p_graph: Graph<'_> = &[
        (FP_P1, &[FP_MAIN]),
        (FP_P2, &[FP_P1]),
        (FP_H1, &[FP_MAIN]),
        (FP_H2, &[FP_H1]),
        (FP_PM, &[FP_P2, FP_H2]),
    ];
    let refusal = first_parent_refusal(FP_PM, p_graph, &[(&q, q_graph), (&held, held_graph)])
        .expect("the held pull request still refuses");
    assert_eq!(refusal.token, "carries_unapproved_commits:#30");
    assert!(
        refusal
            .detail
            .contains("#21 merged #11363's branch; not carried"),
        "{}",
        refusal.detail
    );
    assert!(!refusal.detail.contains("of #21"), "{}", refusal.detail);
}

#[test]
fn a_candidate_that_merged_a_held_branch_is_refused() {
    // Q (base feat/x) merged the held branch H1..H2, and P merged that same
    // held branch: H1 and H2 are a merge's second parent on both sides, so on
    // neither first-parent chain. Not being P's own, they are carried.
    let q = open(31, "feat/held", "feat/x", FP_QM, &[FP_H1, FP_H2, FP_QM]);
    let q_graph: Graph<'_> = &[
        (FP_H1, &[FP_MAIN]),
        (FP_H2, &[FP_H1]),
        (FP_QM, &["feat-x-tip", FP_H2]),
    ];
    let p_graph: Graph<'_> = &[
        (FP_P1, &[FP_MAIN]),
        (FP_H1, &[FP_MAIN]),
        (FP_H2, &[FP_H1]),
        (FP_PM, &[FP_P1, FP_H2]),
    ];
    let refusal = first_parent_refusal(FP_PM, p_graph, &[(&q, q_graph)]).expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#31");
    assert!(refusal.detail.contains(FP_H1), "{}", refusal.detail);
    assert!(refusal.detail.contains(FP_H2), "{}", refusal.detail);
}

#[test]
fn a_candidate_fast_forwarded_onto_a_held_prs_commits_is_refused() {
    // P built straight on Q's commits: they are on both first-parent chains.
    let q = open(32, "feat/held", "main", FP_H2, &[FP_H1, FP_H2]);
    let q_graph: Graph<'_> = &[(FP_H1, &[FP_MAIN]), (FP_H2, &[FP_H1])];
    let p_graph: Graph<'_> = &[(FP_H1, &[FP_MAIN]), (FP_H2, &[FP_H1]), (FP_P1, &[FP_H2])];
    let refusal = first_parent_refusal(FP_P1, p_graph, &[(&q, q_graph)]).expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#32");
    assert!(refusal.detail.contains(FP_H1), "{}", refusal.detail);
    assert!(refusal.detail.contains(FP_H2), "{}", refusal.detail);
}

#[test]
fn a_pr_stacked_on_the_candidate_still_never_blocks_it() {
    let q = open(
        33,
        "feat/on-p",
        P_HEAD_BRANCH,
        FP_Q1,
        &[FP_P1, FP_P2, FP_Q1],
    );
    let q_graph: Graph<'_> = &[(FP_Q1, &[FP_P2])];
    assert_eq!(first_parent_refusal(FP_P2, P_LINE, &[(&q, q_graph)]), None);
}

#[test]
fn a_merge_of_the_candidates_branch_with_one_real_q_commit_is_refused_for_that_commit() {
    // P is built on Q1, Q's own first-parent commit; Q later merged P's branch,
    // which is how P1 and P2 reached Q. Only Q1 is carried.
    let q = open(
        34,
        "feat/q4",
        "feat/x",
        FP_QM,
        &[FP_Q1, FP_P1, FP_P2, FP_QM],
    );
    let q_graph: Graph<'_> = &[
        (FP_Q1, &[FP_MAIN]),
        (FP_P1, &[FP_Q1]),
        (FP_P2, &[FP_P1]),
        (FP_QM, &[FP_Q1, FP_P2]),
    ];
    let p_graph: Graph<'_> = &[(FP_Q1, &[FP_MAIN]), (FP_P1, &[FP_Q1]), (FP_P2, &[FP_P1])];
    let refusal = first_parent_refusal(FP_P2, p_graph, &[(&q, q_graph)]).expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#34");
    assert!(
        refusal.detail.contains("carries 1 commit(s) of #34"),
        "{}",
        refusal.detail
    );
    assert!(refusal.detail.contains(FP_Q1), "{}", refusal.detail);
    assert!(!refusal.detail.contains(FP_P1), "{}", refusal.detail);
    assert!(!refusal.detail.contains(FP_P2), "{}", refusal.detail);
}

#[test]
fn an_unreadable_first_parent_chain_keeps_the_overlap() {
    // Q merged P, but P's first-parent chain cannot be read: fail closed.
    let q = open(35, "feat/q5", "feat/x", FP_QM, &[FP_P1, FP_P2, FP_QM]);
    let q_graph = graph(&[
        (FP_P1, &[FP_MAIN]),
        (FP_P2, &[FP_P1]),
        (FP_QM, &["feat-x-tip", FP_P2]),
    ]);
    let listed = listing(&[(&q, 3)]).to_string();
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("OPEN") {
            return Ok(listed.clone());
        }
        if joined.contains(&format!("compare/main...{FP_P2}?")) && !joined.contains("parents") {
            return Ok(json!({"total": 2, "shas": [FP_P1, FP_P2]}).to_string());
        }
        if joined.contains(&format!("compare/feat/x...{FP_QM}?")) {
            return Ok(q_graph.clone());
        }
        Err("HTTP 502".to_owned())
    };
    let refusal = refusal_with(
        &run_gh,
        "o/r",
        "main",
        11363,
        P_HEAD_BRANCH,
        FP_P2,
        &mut PassCache::default(),
        &mut |_| Judgement::Unapproved("held".to_owned()),
    )
    .expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#35");
}

#[test]
fn an_over_cap_first_parent_chain_keeps_the_overlap() {
    // Q (base main) merged P, but Q's own compare is over the 250-commit cap,
    // so its first-parent chain cannot be read: the block stays.
    let q = open(36, "feat/q6", "main", FP_QM, &[FP_P1, FP_P2, FP_QM]);
    let listed = listing(&[(&q, 3)]).to_string();
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("OPEN") {
            return Ok(listed.clone());
        }
        if joined.contains(&format!("compare/main...{FP_P2}?")) {
            return Ok(if joined.contains("parents") {
                graph(P_LINE)
            } else {
                json!({"total": 2, "shas": [FP_P1, FP_P2]}).to_string()
            });
        }
        if joined.contains(&format!("compare/main...{FP_QM}?")) {
            return Ok(json!({"total": 300, "commits": []}).to_string());
        }
        Err(format!("unexpected: {joined}"))
    };
    let refusal = refusal_with(
        &run_gh,
        "o/r",
        "main",
        11363,
        P_HEAD_BRANCH,
        FP_P2,
        &mut PassCache::default(),
        &mut |_| Judgement::Unapproved("held".to_owned()),
    )
    .expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#36");
    assert!(refusal.detail.contains(FP_P1), "{}", refusal.detail);
}

#[test]
fn a_candidate_that_merged_a_held_pr_and_then_main_is_refused() {
    // P merged held Q (base main), then merged main again. Q's commits stay
    // off P's first-parent chain; main's new tip is on the base.
    let q = open(37, "feat/held", "main", FP_H2, &[FP_H1, FP_H2]);
    let q_graph: Graph<'_> = &[(FP_H1, &[FP_MAIN]), (FP_H2, &[FP_H1])];
    let p_graph: Graph<'_> = &[
        (FP_P1, &[FP_MAIN]),
        (FP_H1, &[FP_MAIN]),
        (FP_H2, &[FP_H1]),
        (FP_PM, &[FP_P1, FP_H2]),
        (FP_P2, &[FP_PM, "main-new-tip"]),
    ];
    let refusal = first_parent_refusal(FP_P2, p_graph, &[(&q, q_graph)]).expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#37");
    assert!(refusal.detail.contains(FP_H1), "{}", refusal.detail);
    assert!(refusal.detail.contains(FP_H2), "{}", refusal.detail);
}

#[test]
fn a_criss_cross_merge_is_refused_for_qs_own_commit_only() {
    // P merged Q1 and Q (base feat/x) merged P1: P1 is P's own, Q1 is Q's.
    let q = open(38, "feat/q8", "feat/x", FP_QM, &[FP_Q1, FP_P1, FP_QM]);
    let q_graph: Graph<'_> = &[
        (FP_Q1, &[FP_MAIN]),
        (FP_P1, &[FP_MAIN]),
        (FP_QM, &[FP_Q1, FP_P1]),
    ];
    let p_graph: Graph<'_> = &[
        (FP_P1, &[FP_MAIN]),
        (FP_Q1, &[FP_MAIN]),
        (FP_PM, &[FP_P1, FP_Q1]),
    ];
    let refusal = first_parent_refusal(FP_PM, p_graph, &[(&q, q_graph)]).expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#38");
    assert!(refusal.detail.contains(FP_Q1), "{}", refusal.detail);
    assert!(!refusal.detail.contains(FP_P1), "{}", refusal.detail);
}

#[test]
fn residual_a_candidate_built_on_commits_a_held_pr_merged_in_is_cleared() {
    // Named residual, documented as current behaviour: held Q (base feat/x)
    // is merge(feat/x tip, X2), taking X1..X2 in as a second parent, and P is
    // built straight on X2. X1 and X2 are on P's first-parent chain and not
    // on Q's, which the graph cannot tell from Q having merged P's branch, so
    // Q is cleared and X lands under P's own approval.
    let q = open(39, "feat/q9", "feat/x", FP_QM, &[FP_H1, FP_H2, FP_QM]);
    let q_graph: Graph<'_> = &[
        (FP_H1, &[FP_MAIN]),
        (FP_H2, &[FP_H1]),
        (FP_QM, &["feat-x-tip", FP_H2]),
    ];
    let p_graph: Graph<'_> = &[(FP_H1, &[FP_MAIN]), (FP_H2, &[FP_H1]), (FP_P1, &[FP_H2])];
    assert_eq!(first_parent_refusal(FP_P1, p_graph, &[(&q, q_graph)]), None);
}

#[test]
fn an_unreadable_own_diff_fails_closed() {
    let q = open(22, "feat/q3", "feat/x", "q3head", &[P_OWN]);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [P_OWN]}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(listing(&[(&q, 1)]).to_string());
        }
        Err("boom".to_owned())
    };
    let mut judge = |_: &OpenPr| Judgement::Unapproved("held".to_owned());
    let refusal = refusal_with(
        &run_gh,
        "o/r",
        "main",
        11363,
        P_HEAD_BRANCH,
        P_OWN,
        &mut PassCache::default(),
        &mut judge,
    )
    .expect("refused");
    assert_eq!(refusal.token, "carried_commits_unreadable:#22");
}

// 2026-11-05: #11450 is stacked on feature/unfreeze-value-source. Its listing
// is 278 commits, over the cap, but its own diff against its own base is
// three commits, and every approved green pull request on main was refused
// `carried_commits_over_cap:#11450` regardless.
const STACKED_BASE: &str = "feature/unfreeze-value-source";
const STACKED_HEAD: &str = "11450000000000000000000000000000000000aa";
const STACKED_OWN: [&str; 3] = [
    "11450000000000000000000000000000000000a1",
    "11450000000000000000000000000000000000a2",
    "11450000000000000000000000000000000000a3",
];
const INDEPENDENT: &str = "11558000000000000000000000000000000000aa";

fn over_cap_listing(number: u64, head_ref: &str, base_ref: &str, head: &str) -> serde_json::Value {
    json!({"data": {"repository": {"pullRequests": {
        "pageInfo": {"hasNextPage": false, "endCursor": null},
        "nodes": [{
            "number": number,
            "headRefName": head_ref,
            "baseRefName": base_ref,
            "headRefOid": head,
            "isCrossRepository": false,
            "author": {"login": "someone"},
            "carriedCommits": {"totalCount": 278, "nodes": []},
        }],
    }}}})
}

fn independent_refusal(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    judged: &Cell<usize>,
) -> Option<super::Refusal> {
    let mut judge = |_: &OpenPr| {
        judged.set(judged.get() + 1);
        Judgement::Unapproved("no counted APPROVE".to_owned())
    };
    refusal_with(
        run_gh,
        "Generous-Corp/pulp",
        "main",
        11558,
        "feat/independent",
        INDEPENDENT,
        &mut PassCache::default(),
        &mut judge,
    )
}

#[test]
fn an_unapproved_over_cap_stacked_pr_does_not_block_an_independent_pr() {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [INDEPENDENT]}).to_string());
        }
        if joined.contains(&format!("compare/{STACKED_BASE}...{STACKED_HEAD}")) {
            return Ok(graph(&[
                (STACKED_OWN[0], &["base-tip"]),
                (STACKED_OWN[1], &[STACKED_OWN[0]]),
                (STACKED_OWN[2], &[STACKED_OWN[1]]),
            ]));
        }
        if joined.contains("OPEN") {
            return Ok(
                over_cap_listing(11450, "feat/stacked", STACKED_BASE, STACKED_HEAD).to_string(),
            );
        }
        Err(format!("unexpected: {joined}"))
    };
    let judged = Cell::new(0);
    assert_eq!(independent_refusal(&run_gh, &judged), None);
    assert_eq!(judged.get(), 0, "a cleared own diff reads no approval");
}

#[test]
fn an_over_cap_own_diff_that_does_not_reach_the_candidate_does_not_block_it() {
    // Q's own diff is itself over the cap, Q is unapproved, but Q's head does
    // not reach P's commit, so P is admitted.
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [INDEPENDENT]}).to_string());
        }
        if joined.contains(&format!("compare/{STACKED_BASE}...{STACKED_HEAD}")) {
            return Ok(json!({"total": 278, "shas": STACKED_OWN}).to_string());
        }
        if joined.contains(&format!("compare/{STACKED_HEAD}...{INDEPENDENT}")) {
            return Ok("diverged".to_owned());
        }
        if joined.contains("OPEN") {
            return Ok(
                over_cap_listing(11450, "feat/stacked", STACKED_BASE, STACKED_HEAD).to_string(),
            );
        }
        Err(format!("unexpected: {joined}"))
    };
    let judged = Cell::new(0);
    assert_eq!(independent_refusal(&run_gh, &judged), None);
    assert_eq!(judged.get(), 1);
}

#[test]
fn an_over_cap_own_diff_that_carries_the_candidates_commit_is_refused() {
    // Q's head reaches P's commit and Q's base does not: Q introduced it.
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [INDEPENDENT]}).to_string());
        }
        if joined.contains(&format!("compare/{STACKED_BASE}...{STACKED_HEAD}")) {
            return Ok(json!({"total": 278, "shas": STACKED_OWN}).to_string());
        }
        if joined.contains(&format!("compare/{STACKED_HEAD}...{INDEPENDENT}")) {
            return Ok("behind".to_owned());
        }
        if joined.contains(&format!("compare/{STACKED_BASE}...{INDEPENDENT}")) {
            return Ok("ahead".to_owned());
        }
        if joined.contains("OPEN") {
            return Ok(
                over_cap_listing(11450, "feat/stacked", STACKED_BASE, STACKED_HEAD).to_string(),
            );
        }
        Err(format!("unexpected: {joined}"))
    };
    let refusal = independent_refusal(&run_gh, &Cell::new(0)).expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#11450");
    assert!(refusal.detail.contains(INDEPENDENT), "{}", refusal.detail);
}

#[test]
fn an_over_cap_own_diff_inheriting_the_candidates_commit_from_its_base_is_cleared() {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [INDEPENDENT]}).to_string());
        }
        if joined.contains(&format!("compare/{STACKED_BASE}...{STACKED_HEAD}")) {
            return Ok(json!({"total": 278, "shas": STACKED_OWN}).to_string());
        }
        if joined.contains(&format!("compare/{STACKED_HEAD}...{INDEPENDENT}"))
            || joined.contains(&format!("compare/{STACKED_BASE}...{INDEPENDENT}"))
        {
            return Ok("behind".to_owned());
        }
        if joined.contains("OPEN") {
            return Ok(
                over_cap_listing(11450, "feat/stacked", STACKED_BASE, STACKED_HEAD).to_string(),
            );
        }
        Err(format!("unexpected: {joined}"))
    };
    assert_eq!(independent_refusal(&run_gh, &Cell::new(0)), None);
}

#[test]
fn an_over_cap_own_diff_with_too_many_candidate_commits_to_probe_fails_closed() {
    // Too many of P's commits to probe one by one, and each is its own root,
    // so the roots are too many to probe as well.
    let many: Vec<String> = (0..=super::MAX_PROBED_COMMITS)
        .map(|i| format!("{i:040x}"))
        .collect();
    let many_roots = graph(
        &many
            .iter()
            .map(|sha| (sha.as_str(), &["base-tip"][..]))
            .collect::<Vec<_>>(),
    );
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") && joined.contains("parents") {
            return Ok(many_roots.clone());
        }
        if joined.contains("compare/main...") {
            return Ok(json!({"total": many.len(), "shas": many}).to_string());
        }
        if joined.contains(&format!("compare/{STACKED_BASE}...{STACKED_HEAD}")) {
            return Ok(json!({"total": 278, "shas": STACKED_OWN}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(
                over_cap_listing(11450, "feat/stacked", STACKED_BASE, STACKED_HEAD).to_string(),
            );
        }
        Err(format!("unexpected: {joined}"))
    };
    let refusal = independent_refusal(&run_gh, &Cell::new(0)).expect("refused");
    assert_eq!(refusal.token, "carried_commits_over_cap:#11450");
}

#[test]
fn an_over_cap_own_diff_with_an_unreadable_probe_fails_closed() {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [INDEPENDENT]}).to_string());
        }
        if joined.contains(&format!("compare/{STACKED_BASE}...{STACKED_HEAD}")) {
            return Ok(json!({"total": 278, "shas": STACKED_OWN}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(
                over_cap_listing(11450, "feat/stacked", STACKED_BASE, STACKED_HEAD).to_string(),
            );
        }
        Err("HTTP 502".to_owned())
    };
    let refusal = independent_refusal(&run_gh, &Cell::new(0)).expect("refused");
    assert_eq!(refusal.token, "carried_commits_unreadable:#11450");
}

#[test]
fn an_approved_over_cap_own_diff_is_not_probed() {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [INDEPENDENT]}).to_string());
        }
        if joined.contains(&format!("compare/{STACKED_BASE}...{STACKED_HEAD}")) {
            return Ok(json!({"total": 278, "shas": STACKED_OWN}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(
                over_cap_listing(11450, "feat/stacked", STACKED_BASE, STACKED_HEAD).to_string(),
            );
        }
        Err(format!("unexpected: {joined}"))
    };
    let refusal = refusal_with(
        &run_gh,
        "Generous-Corp/pulp",
        "main",
        11558,
        "feat/independent",
        INDEPENDENT,
        &mut PassCache::default(),
        &mut |_| Judgement::Approved,
    );
    assert_eq!(refusal, None);
}

#[test]
fn a_compare_over_one_page_is_read_page_by_page() {
    let shas: Vec<String> = (0..150).map(|i| format!("{i:040x}")).collect();
    let pages = Cell::new(0);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        pages.set(pages.get() + 1);
        let page = if joined.contains("&page=1 ") {
            &shas[..100]
        } else if joined.contains("&page=2 ") {
            &shas[100..]
        } else {
            return Err(format!("unexpected: {joined}"));
        };
        Ok(json!({"total": 150, "shas": page}).to_string())
    };
    let CommitList::Complete(read) =
        super::compare_commits(&run_gh, "o/r", "main", "h").expect("readable")
    else {
        panic!("150 commits are under the cap");
    };
    assert_eq!(read.len(), 150);
    assert_eq!(pages.get(), 2);
}

// A candidate P over the commit cap (vellum's re-rooted mirror-history import:
// 2,724 commits) cannot list its own commits, so each other open Q's own
// commits are read with their parents and P's head is probed for the roots.
const BIG_HEAD: &str = "fbbd56fd00000000000000000000000000000000";
const MAIN_TIP: &str = "0000000000000000000000000000000000000001";
const Q_HEAD: &str = "7777777700000000000000000000000000000000";
const Q_ROOT: &str = "7777777700000000000000000000000000000001";
const Q_TIP: &str = Q_HEAD;

fn graph(commits: &[(&str, &[&str])]) -> String {
    json!({
        "total": commits.len(),
        "commits": commits
            .iter()
            .map(|(sha, parents)| json!({"sha": sha, "parents": parents}))
            .collect::<Vec<_>>(),
    })
    .to_string()
}

fn open_q(number: u64, head_ref: &str, base_ref: &str, head: &str) -> OpenPr {
    open(number, head_ref, base_ref, head, &[head])
}

/// Every GitHub read an over-cap candidate makes, served from `reads`, which
/// answers a read it recognises and `None` for any other; an unrecognised read
/// fails the test. P's own compare is always over the cap.
fn big_refusal(
    others: &[&OpenPr],
    reads: &dyn Fn(&str) -> Option<Result<String, String>>,
    judge: &mut dyn FnMut(&OpenPr) -> Judgement,
) -> Option<super::Refusal> {
    let listed = listing(&others.iter().map(|q| (*q, 1)).collect::<Vec<_>>()).to_string();
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains(&format!("compare/main...{BIG_HEAD}")) {
            return Ok(json!({"total": 2724, "shas": []}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(listed.clone());
        }
        reads(&joined).unwrap_or_else(|| panic!("unexpected read: {joined}"))
    };
    refusal_with(
        &run_gh,
        "Generous-Corp/vellum",
        "main",
        229,
        "m2/combined-reexport",
        BIG_HEAD,
        &mut PassCache::default(),
        judge,
    )
}

fn graph_read(joined: &str, head: &str) -> bool {
    joined.contains(&format!("compare/main...{head}")) && joined.contains("parents")
}

fn probe_read(joined: &str, root: &str) -> bool {
    joined.contains(&format!("compare/{BIG_HEAD}...{root}?per_page=1"))
}

#[test]
fn an_over_cap_candidate_carrying_no_open_prs_commits_is_admitted() {
    let q = open_q(228, "feat/other", "main", Q_HEAD);
    let reads = |joined: &str| {
        if graph_read(joined, Q_HEAD) {
            return Some(Ok(graph(&[(Q_ROOT, &[MAIN_TIP]), (Q_TIP, &[Q_ROOT])])));
        }
        probe_read(joined, Q_ROOT).then(|| Ok("diverged".to_owned()))
    };
    let judged = Cell::new(0);
    let mut judge = |_: &OpenPr| {
        judged.set(judged.get() + 1);
        Judgement::Unapproved("no counted APPROVE".to_owned())
    };
    assert_eq!(big_refusal(&[&q], &reads, &mut judge), None);
    assert_eq!(
        judged.get(),
        0,
        "a head carrying nothing of Q reads no approval"
    );
}

#[test]
fn an_over_cap_candidate_carrying_an_unapproved_prs_commit_is_refused() {
    let q = open_q(228, "feat/other", "main", Q_HEAD);
    let reads = |joined: &str| {
        if graph_read(joined, Q_HEAD) {
            return Some(Ok(graph(&[(Q_ROOT, &[MAIN_TIP]), (Q_TIP, &[Q_ROOT])])));
        }
        probe_read(joined, Q_ROOT).then(|| Ok("behind".to_owned()))
    };
    let refusal = big_refusal(&[&q], &reads, &mut |_| {
        Judgement::Unapproved("no counted APPROVE".to_owned())
    })
    .expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#228");
    assert!(refusal.detail.contains(Q_ROOT), "{}", refusal.detail);

    // The same head is admitted once Q's head is approved.
    assert_eq!(
        big_refusal(&[&q], &reads, &mut |_| Judgement::Approved),
        None
    );
}

#[test]
fn an_over_cap_candidate_carrying_only_a_branch_merged_into_q_is_refused() {
    // Q merged a side branch; P carries only that branch's commit, which is
    // a second root of Q's own commits.
    const SIDE: &str = "5151515100000000000000000000000000000000";
    let q = open_q(228, "feat/other", "main", Q_HEAD);
    let reads = |joined: &str| {
        if graph_read(joined, Q_HEAD) {
            return Some(Ok(graph(&[
                (Q_ROOT, &[MAIN_TIP]),
                (SIDE, &[MAIN_TIP]),
                (Q_TIP, &[Q_ROOT, SIDE]),
            ])));
        }
        if probe_read(joined, Q_ROOT) {
            return Some(Ok("diverged".to_owned()));
        }
        probe_read(joined, SIDE).then(|| Ok("behind".to_owned()))
    };
    let refusal = big_refusal(&[&q], &reads, &mut |_| {
        Judgement::Unapproved("held".to_owned())
    })
    .expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#228");
    assert!(refusal.detail.contains(SIDE), "{}", refusal.detail);
    assert!(!refusal.detail.contains(Q_ROOT), "{}", refusal.detail);
}

#[test]
fn an_over_cap_candidate_is_not_blocked_by_commits_q_only_inherits() {
    // Q is stacked on another branch; P carries that branch's commit, not Q's.
    const INHERITED: &str = "1212121200000000000000000000000000000000";
    let q = open_q(230, "feat/stacked", "feat/base", Q_HEAD);
    let reads = |joined: &str| {
        if graph_read(joined, Q_HEAD) {
            return Some(Ok(graph(&[
                (INHERITED, &[MAIN_TIP]),
                (Q_ROOT, &[INHERITED]),
            ])));
        }
        if joined.contains(&format!("compare/feat/base...{Q_HEAD}")) {
            return Some(Ok(json!({"total": 1, "shas": [Q_ROOT]}).to_string()));
        }
        assert!(
            !probe_read(joined, INHERITED),
            "an inherited commit is not probed"
        );
        probe_read(joined, Q_ROOT).then(|| Ok("diverged".to_owned()))
    };
    let mut judge = |_: &OpenPr| Judgement::Unapproved("held".to_owned());
    assert_eq!(big_refusal(&[&q], &reads, &mut judge), None);
}

#[test]
fn an_over_cap_candidates_base_branch_pr_and_stacked_prs_are_not_read() {
    let base_pr = open_q(1, "main", "release", Q_HEAD);
    let stacked = open_q(2, "feat/on-top", "m2/combined-reexport", Q_HEAD);
    let reads = |_: &str| None;
    let mut judge = |_: &OpenPr| Judgement::Unapproved("held".to_owned());
    assert_eq!(big_refusal(&[&base_pr, &stacked], &reads, &mut judge), None);
}

#[test]
fn an_over_cap_candidate_fails_closed_on_an_unapproved_over_cap_q() {
    let q = open_q(228, "feat/other", "main", Q_HEAD);
    let reads = |joined: &str| {
        graph_read(joined, Q_HEAD).then(|| Ok(json!({"total": 300, "commits": []}).to_string()))
    };
    let refusal = big_refusal(&[&q], &reads, &mut |_| {
        Judgement::Unapproved("no counted APPROVE".to_owned())
    })
    .expect("refused");
    assert_eq!(refusal.token, "carried_commits_over_cap:#228");
    assert!(refusal.detail.contains("300"), "{}", refusal.detail);

    // An approved over-cap Q never refuses.
    assert_eq!(
        big_refusal(&[&q], &reads, &mut |_| Judgement::Approved),
        None
    );
}

#[test]
fn an_over_cap_candidate_fails_closed_on_an_unreadable_read() {
    let q = open_q(228, "feat/other", "main", Q_HEAD);
    let unreadable_probe = |joined: &str| {
        if graph_read(joined, Q_HEAD) {
            return Some(Ok(graph(&[(Q_ROOT, &[MAIN_TIP])])));
        }
        probe_read(joined, Q_ROOT).then(|| Err("HTTP 502".to_owned()))
    };
    let refusal =
        big_refusal(&[&q], &unreadable_probe, &mut |_| Judgement::Approved).expect("refused");
    assert_eq!(refusal.token, "carried_commits_unreadable:#228");
    assert!(refusal.detail.contains("HTTP 502"), "{}", refusal.detail);

    let unreadable_graph =
        |joined: &str| graph_read(joined, Q_HEAD).then(|| Err("HTTP 403".to_owned()));
    let refusal =
        big_refusal(&[&q], &unreadable_graph, &mut |_| Judgement::Approved).expect("refused");
    assert_eq!(refusal.token, "carried_commits_unreadable:#228");

    let malformed = |joined: &str| {
        graph_read(joined, Q_HEAD)
            .then(|| Ok(json!({"total": 1, "commits": [{"sha": Q_ROOT}]}).to_string()))
    };
    let refusal = big_refusal(&[&q], &malformed, &mut |_| Judgement::Approved).expect("refused");
    assert_eq!(refusal.token, "carried_commits_unreadable:#228");
}

#[test]
fn an_over_cap_candidate_whose_probe_budget_is_spent_fails_closed_on_an_unapproved_q() {
    use super::{MAX_PROBED_COMMITS, MAX_REVERSE_PROBES};
    let per_q = MAX_PROBED_COMMITS;
    let qs: Vec<OpenPr> = (0..=MAX_REVERSE_PROBES / per_q)
        .map(|i| {
            open_q(
                300 + i as u64,
                &format!("feat/q{i}"),
                "main",
                &format!("{i:040x}"),
            )
        })
        .collect();
    let roots = |i: usize| -> Vec<String> {
        (0..per_q)
            .map(|r| format!("{:020x}{:020x}", i + 1, r + 1))
            .collect()
    };
    let reads = |joined: &str| {
        for (i, q) in qs.iter().enumerate() {
            if graph_read(joined, &q.head_sha) {
                let roots = roots(i);
                let commits: Vec<(&str, &[&str])> = roots
                    .iter()
                    .map(|sha| (sha.as_str(), &[MAIN_TIP][..]))
                    .collect();
                return Some(Ok(graph(&commits)));
            }
        }
        joined
            .contains(&format!("compare/{BIG_HEAD}..."))
            .then(|| Ok("diverged".to_owned()))
    };
    let refs: Vec<&OpenPr> = qs.iter().collect();
    let last = qs.last().expect("qs").number;
    let refusal = big_refusal(&refs, &reads, &mut |q| {
        if q.number == last {
            Judgement::Unapproved("held".to_owned())
        } else {
            Judgement::Approved
        }
    })
    .expect("refused");
    assert_eq!(refusal.token, format!("carried_commits_over_cap:#{last}"));
    assert_eq!(
        big_refusal(&refs, &reads, &mut |_| Judgement::Approved),
        None
    );
}

/// One compare page of a linear history `all`, holding `all[range]`.
fn graph_page(
    total: usize,
    merge_base: &str,
    all: &[String],
    range: std::ops::Range<usize>,
) -> String {
    json!({
        "total": total,
        "merge_base": merge_base,
        "commits": range
            .map(|i| {
                let parent = if i == 0 { MAIN_TIP } else { all[i - 1].as_str() };
                json!({"sha": all[i], "parents": [parent]})
            })
            .collect::<Vec<_>>(),
    })
    .to_string()
}

#[test]
fn an_over_cap_candidate_fails_closed_when_the_base_moves_between_pages() {
    // Q has 150 own commits; ten of them land on main between page 1 and
    // page 2, so page 2 shifts and the count (now 140) would still add up.
    let q = open_q(228, "feat/other", "main", Q_HEAD);
    let shas: Vec<String> = (1..=150).map(|i| format!("{i:040x}")).collect();
    let reads = |joined: &str| {
        if !graph_read(joined, Q_HEAD) {
            return None;
        }
        Some(Ok(if joined.contains("&page=1") {
            graph_page(150, "aaaa", &shas, 0..100)
        } else {
            graph_page(140, "bbbb", &shas, 110..150)
        }))
    };
    let refusal = big_refusal(&[&q], &reads, &mut |_| Judgement::Approved).expect("refused");
    assert_eq!(refusal.token, "carried_commits_unreadable:#228");
    assert!(
        refusal.detail.contains("moved between pages"),
        "{}",
        refusal.detail
    );

    // Control: the same pages from one unmoved base are read in full.
    let steady = |joined: &str| {
        if graph_read(joined, Q_HEAD) {
            return Some(Ok(if joined.contains("&page=1") {
                graph_page(150, "aaaa", &shas, 0..100)
            } else {
                graph_page(150, "aaaa", &shas, 100..150)
            }));
        }
        joined
            .contains(&format!("compare/{BIG_HEAD}..."))
            .then(|| Ok("diverged".to_owned()))
    };
    assert_eq!(
        big_refusal(&[&q], &steady, &mut |_| Judgement::Approved),
        None
    );
}

#[test]
fn a_candidates_own_compare_fails_closed_when_the_base_moves_between_pages() {
    let shas: Vec<String> = (1..=150).map(|i| format!("{i:040x}")).collect();
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") && joined.contains("&page=1") {
            return Ok(
                json!({"total": 150, "merge_base": "aaaa", "shas": shas[..100]}).to_string(),
            );
        }
        if joined.contains("compare/main...") {
            return Ok(
                json!({"total": 140, "merge_base": "bbbb", "shas": shas[110..]}).to_string(),
            );
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
    assert_eq!(refusal.token, "carried_commits_unreadable");
    assert!(
        refusal.detail.contains("moved between pages"),
        "{}",
        refusal.detail
    );
}

#[test]
fn an_over_cap_candidate_is_refused_for_a_commit_a_stacked_q_introduced() {
    // Q targets another branch; its own diff keeps Q_ROOT, which P carries.
    const INHERITED: &str = "1212121200000000000000000000000000000000";
    let q = open_q(230, "feat/stacked", "feat/base", Q_HEAD);
    let reads = |joined: &str| {
        if graph_read(joined, Q_HEAD) {
            return Some(Ok(graph(&[
                (INHERITED, &[MAIN_TIP]),
                (Q_ROOT, &[INHERITED]),
            ])));
        }
        if joined.contains(&format!("compare/feat/base...{Q_HEAD}")) {
            return Some(Ok(json!({"total": 1, "shas": [Q_ROOT]}).to_string()));
        }
        probe_read(joined, Q_ROOT).then(|| Ok("behind".to_owned()))
    };
    let refusal = big_refusal(&[&q], &reads, &mut |_| {
        Judgement::Unapproved("held".to_owned())
    })
    .expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#230");
    assert!(refusal.detail.contains(Q_ROOT), "{}", refusal.detail);
}

#[test]
fn an_over_cap_candidate_reads_each_open_prs_roots_once_per_pass() {
    let q = open_q(228, "feat/other", "main", Q_HEAD);
    let listed = listing(&[(&q, 1)]).to_string();
    let graph_reads = Cell::new(0);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains(&format!("compare/main...{BIG_HEAD}")) {
            return Ok(json!({"total": 2724, "shas": []}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(listed.clone());
        }
        if graph_read(&joined, Q_HEAD) {
            graph_reads.set(graph_reads.get() + 1);
            return Ok(graph(&[(Q_ROOT, &[MAIN_TIP])]));
        }
        if probe_read(&joined, Q_ROOT) {
            return Ok("diverged".to_owned());
        }
        Err(format!("unexpected: {joined}"))
    };
    let mut cache = PassCache::default();
    for _ in 0..2 {
        let refusal = refusal_with(
            &run_gh,
            "Generous-Corp/vellum",
            "main",
            229,
            "m2/combined-reexport",
            BIG_HEAD,
            &mut cache,
            &mut |_| Judgement::Unapproved("held".to_owned()),
        );
        assert_eq!(refusal, None);
    }
    assert_eq!(
        graph_reads.get(),
        1,
        "a second admission path reuses Q's roots"
    );
}

#[test]
fn an_over_cap_stacked_q_is_narrowed_before_the_over_cap_decision() {
    // Q inherits a large history from its base, but its own diff is small and
    // does not reach P. The inherited history must not make Q unbounded.
    const STACKED_BASE: &str = "feat/base";
    const INHERITED: &str = "8888888800000000000000000000000000000000";
    const OWN_ROOT: &str = "8888888800000000000000000000000000000001";
    let q = open_q(231, "feat/stacked", STACKED_BASE, Q_HEAD);
    let reads = |joined: &str| {
        if joined.contains(&format!("compare/main...{Q_HEAD}")) {
            return Some(Ok(json!({
                "total": 300,
                "merge_base": MAIN_TIP,
                "commits": [("inherited", [MAIN_TIP])],
            })
            .to_string()));
        }
        if joined.contains(&format!("compare/{STACKED_BASE}...{Q_HEAD}")) {
            return Some(Ok(graph(&[(OWN_ROOT, &[INHERITED])])));
        }
        probe_read(joined, OWN_ROOT).then(|| Ok("diverged".to_owned()))
    };
    assert_eq!(
        big_refusal(&[&q], &reads, &mut |_| {
            Judgement::Unapproved("no counted APPROVE".to_owned())
        }),
        None
    );
}

// The 2026-11-09 (UTC) incident: #11796 (57 commits off main, one root) was refused
// with `carried_commits_over_cap:#11829`. #11829 is stacked on another branch
// and its own diff counts 283 commits, over the cap, and it is unapproved;
// P's 57 commits are over the per-commit probe limit. Probing the root of
// P's commits settles it.
const WIDE_HEAD: &str = "ca48f12096fe589b1885384b2efe4aa0d8399de9";
const WIDE_ROOT: &str = "14224c9fe5d992ed587dd9901d2548ea5f0560e2";
const MIDI_BASE: &str = "feature/midi-learn-feedback";
const MIDI_HEAD: &str = "ba19976dcd4af55d4bdd596710fab3dd430a29f5";

/// P's 57 commits: a chain from `WIDE_ROOT` (whose parent is on main) to
/// `WIDE_HEAD`, with one merge of main part way up, plus `extra_roots`
/// (each with a parent on main) merged in at the head.
fn wide_chain(extra_roots: &[&str]) -> (Vec<String>, String) {
    let mut shas: Vec<String> = vec![WIDE_ROOT.to_owned()];
    shas.extend((1..56 - extra_roots.len()).map(|i| format!("{i:040x}")));
    shas.extend(extra_roots.iter().map(|sha| (*sha).to_owned()));
    shas.push(WIDE_HEAD.to_owned());
    let mut commits: Vec<(String, Vec<String>)> = Vec::new();
    let chain_len = shas.len() - 1 - extra_roots.len();
    for (i, sha) in shas.iter().take(chain_len).enumerate() {
        let parents = match i {
            0 => vec!["main-old".to_owned()],
            20 => vec![shas[i - 1].clone(), "main-mid".to_owned()],
            _ => vec![shas[i - 1].clone()],
        };
        commits.push((sha.clone(), parents));
    }
    for root in extra_roots {
        commits.push(((*root).to_owned(), vec!["main-old".to_owned()]));
    }
    let mut head_parents = vec![shas[chain_len - 1].clone()];
    head_parents.extend(extra_roots.iter().map(|sha| (*sha).to_owned()));
    commits.push((WIDE_HEAD.to_owned(), head_parents));
    let refs: Vec<(&str, Vec<&str>)> = commits
        .iter()
        .map(|(sha, parents)| (sha.as_str(), parents.iter().map(String::as_str).collect()))
        .collect();
    let graph_json = graph(
        &refs
            .iter()
            .map(|(sha, parents)| (*sha, parents.as_slice()))
            .collect::<Vec<_>>(),
    );
    (shas, graph_json)
}

/// #11796's admission against an over-cap, unapproved #11829, with `graph`
/// as P's `main...P` read with parents and `probe` answering
/// `compare/{MIDI_HEAD}...{root}` (`None` fails the test).
fn wide_refusal(
    shas: &[String],
    graph_json: &str,
    probe: &dyn Fn(&str) -> Option<Result<String, String>>,
    probes: &Cell<usize>,
) -> Option<super::Refusal> {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains(&format!("compare/main...{WIDE_HEAD}")) && joined.contains("parents") {
            return Ok(graph_json.to_owned());
        }
        if joined.contains(&format!("compare/main...{WIDE_HEAD}")) {
            return Ok(json!({"total": shas.len(), "shas": shas}).to_string());
        }
        if joined.contains(&format!("compare/{MIDI_BASE}...{MIDI_HEAD}")) {
            return Ok(json!({"total": 283, "shas": []}).to_string());
        }
        if joined.contains("OPEN") {
            return Ok(
                over_cap_listing(11829, "feature/midi-learn-view", MIDI_BASE, MIDI_HEAD)
                    .to_string(),
            );
        }
        if let Some(root) = joined
            .split(&format!("compare/{MIDI_HEAD}..."))
            .nth(1)
            .and_then(|rest| rest.split('?').next())
        {
            probes.set(probes.get() + 1);
            if let Some(answer) = probe(root) {
                return answer;
            }
        }
        panic!("unexpected read: {joined}")
    };
    refusal_with(
        &run_gh,
        "Generous-Corp/pulp",
        "main",
        11796,
        "feature/audio-decoder-fuzz-20261107",
        WIDE_HEAD,
        &mut PassCache::default(),
        &mut |_| Judgement::Unapproved("head ba19976dcd4a carries no approval".to_owned()),
    )
}

#[test]
fn a_wide_candidate_whose_root_an_over_cap_q_does_not_reach_is_admitted() {
    let (shas, graph_json) = wide_chain(&[]);
    assert!(
        shas.len() > super::MAX_PROBED_COMMITS,
        "too many to probe one by one"
    );
    let probes = Cell::new(0);
    let refusal = wide_refusal(
        &shas,
        &graph_json,
        &|root| (root == WIDE_ROOT).then(|| Ok("diverged".to_owned())),
        &probes,
    );
    assert_eq!(refusal, None);
    assert_eq!(probes.get(), 1, "one root, one probe");
}

#[test]
fn a_wide_candidate_that_carries_an_over_cap_qs_commits_is_still_refused() {
    // Q's head reaches P's root: Q (or a branch Q merged) has P's history, so
    // the root probe cannot clear it and P fails closed as before.
    let (shas, graph_json) = wide_chain(&[]);
    let refusal = wide_refusal(
        &shas,
        &graph_json,
        &|root| (root == WIDE_ROOT).then(|| Ok("behind".to_owned())),
        &Cell::new(0),
    )
    .expect("refused");
    assert_eq!(refusal.token, "carried_commits_over_cap:#11829");
}

#[test]
fn a_criss_cross_history_with_a_merge_base_off_main_is_refused() {
    // P and Q have two merge bases: one on main (the one the compare API may
    // name) and X, a commit off main that P merged in and Q also reaches.
    // X is a root of P's commits, so the root probe sees it, whatever single
    // merge base the API reports.
    const CROSS: &str = "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2";
    let (shas, graph_json) = wide_chain(&[CROSS]);
    let refusal = wide_refusal(
        &shas,
        &graph_json,
        &|root| match root {
            WIDE_ROOT => Some(Ok("diverged".to_owned())),
            CROSS => Some(Ok("behind".to_owned())),
            _ => None,
        },
        &Cell::new(0),
    )
    .expect("refused");
    assert_eq!(refusal.token, "carried_commits_over_cap:#11829");
}

#[test]
fn a_wide_candidate_whose_commit_graph_is_unreadable_fails_closed() {
    let (shas, _) = wide_chain(&[]);
    let refusal =
        wide_refusal(&shas, "{\"error\": 502}", &|_| None, &Cell::new(0)).expect("refused");
    assert_eq!(refusal.token, "carried_commits_unreadable:#11829");
}

#[test]
fn a_wide_candidate_whose_root_probe_is_unreadable_fails_closed() {
    let (shas, graph_json) = wide_chain(&[]);
    let refusal = wide_refusal(
        &shas,
        &graph_json,
        &|_| Some(Err("HTTP 502".to_owned())),
        &Cell::new(0),
    )
    .expect("refused");
    assert_eq!(refusal.token, "carried_commits_unreadable:#11829");
}

#[test]
fn a_wide_candidate_whose_commit_graph_is_incomplete_fails_closed() {
    // The graph names fewer commits than its total: a missing commit could
    // hide the real root, so nothing is probed and P fails closed.
    let (shas, graph_json) = wide_chain(&[]);
    let mut short: serde_json::Value = serde_json::from_str(&graph_json).expect("json");
    short["commits"]
        .as_array_mut()
        .expect("commits")
        .retain(|commit| commit["sha"] != WIDE_ROOT);
    let probes = Cell::new(0);
    let refusal = wide_refusal(&shas, &short.to_string(), &|_| None, &probes).expect("refused");
    assert_eq!(refusal.token, "carried_commits_over_cap:#11829");
    assert_eq!(probes.get(), 0);
}

#[test]
fn an_orphan_root_with_no_parents_is_probed_like_any_root() {
    // A commit of P's with no parents (an unrelated history merged in) is a
    // root of P's commits and is probed like any other.
    const ORPHAN: &str = "0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e";
    let (shas, graph_json) = wide_chain(&[ORPHAN]);
    let mut orphaned: serde_json::Value = serde_json::from_str(&graph_json).expect("json");
    for commit in orphaned["commits"].as_array_mut().expect("commits") {
        if commit["sha"] == ORPHAN {
            commit["parents"] = json!([]);
        }
    }
    let probes = Cell::new(0);
    let refusal = wide_refusal(
        &shas,
        &orphaned.to_string(),
        &|root| match root {
            WIDE_ROOT | ORPHAN => Some(Ok("diverged".to_owned())),
            _ => None,
        },
        &probes,
    );
    assert_eq!(refusal, None);
    assert_eq!(
        probes.get(),
        2,
        "both roots, the orphan included, are probed"
    );
}
