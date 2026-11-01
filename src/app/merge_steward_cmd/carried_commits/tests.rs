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
            return Ok(json!({"total": 1, "shas": [CHILD_OWN]}).to_string());
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

#[test]
fn a_pr_that_merged_the_candidates_branch_without_stacking_is_still_refused() {
    // Q (base X) merged P's branch: P's commit is in Q's own diff against X.
    // The sets cannot tell P's origin commit from Q's copy, so the unapproved
    // Q conservatively holds P; Q must drop the merge or land first.
    let q = open(21, "feat/q2", "feat/x", "q2head", &[P_OWN, CHILD_OWN]);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("compare/main...") {
            return Ok(json!({"total": 1, "shas": [P_OWN]}).to_string());
        }
        if joined.contains("compare/feat/x...q2head") {
            return Ok(json!({"total": 2, "shas": [P_OWN, CHILD_OWN]}).to_string());
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
    )
    .expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#21");
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
