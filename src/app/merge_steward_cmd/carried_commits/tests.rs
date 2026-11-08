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
        first_parents: None,
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
            return Ok(json!({"total": 3, "shas": STACKED_OWN}).to_string());
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
    let many: Vec<String> = (0..=super::MAX_PROBED_COMMITS)
        .map(|i| format!("{i:040x}"))
        .collect();
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
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

// A stack whose bottom pull request lands first (Shipyard#1162): the middle
// pull request merged the bottom one's branch but targets a branch that is no
// longer an open pull request, so it is not recognised as stacked on it.
const STACK_MAIN: &str = "5700000000000000000000000000000000000000";
const BOTTOM_1: &str = "5700000000000000000000000000000000000b01";
const BOTTOM_2: &str = "5700000000000000000000000000000000000b02";
const MIDDLE_1: &str = "5700000000000000000000000000000000000a01";
const MIDDLE_MERGE: &str = "5700000000000000000000000000000000000a02";
const TOP_1: &str = "5700000000000000000000000000000000000c01";

/// An open pull request listing whose commits carry their first parents.
fn parented_listing(prs: &[(&OpenPr, &[(&str, Option<&str>)])]) -> serde_json::Value {
    let nodes = prs
        .iter()
        .map(|(pr, commits)| {
            json!({
                "number": pr.number,
                "headRefName": pr.head_ref,
                "baseRefName": pr.base_ref,
                "headRefOid": pr.head_sha,
                "isCrossRepository": false,
                "author": {"login": "someone"},
                "carriedCommits": {
                    "totalCount": commits.len(),
                    "nodes": commits.iter().map(|(oid, parent)| json!({"commit": {
                        "oid": oid,
                        "parents": {"nodes": parent.map(|p| vec![json!({"oid": p})]).unwrap_or_default()},
                    }})).collect::<Vec<_>>(),
                },
            })
        })
        .collect::<Vec<_>>();
    json!({"data": {"repository": {"pullRequests": {
        "pageInfo": {"hasNextPage": false, "endCursor": null},
        "nodes": nodes,
    }}}})
}

fn own_compare(commits: &[(&str, &str)]) -> String {
    json!({
        "total": commits.len(),
        "merge_base": STACK_MAIN,
        "shas": commits.iter().map(|(sha, _)| sha).collect::<Vec<_>>(),
        "first_parents": commits.iter().map(|(_, parent)| parent).collect::<Vec<_>>(),
    })
    .to_string()
}

struct Stack {
    bottom: OpenPr,
    middle: OpenPr,
    top: OpenPr,
}

fn stack3() -> Stack {
    Stack {
        bottom: open(
            11765,
            "feature/bottom",
            "main",
            BOTTOM_2,
            &[BOTTOM_1, BOTTOM_2],
        ),
        middle: open(
            11772,
            "feature/middle",
            "feature/merged-away",
            MIDDLE_MERGE,
            &[MIDDLE_1, BOTTOM_1, BOTTOM_2, MIDDLE_MERGE],
        ),
        top: open(11790, "feature/top", "feature/middle", TOP_1, &[TOP_1]),
    }
}

const BOTTOM_LINE: [(&str, Option<&str>); 2] =
    [(BOTTOM_1, Some(STACK_MAIN)), (BOTTOM_2, Some(BOTTOM_1))];
// The middle merged the bottom's branch: the merge's first parent is its own.
const MIDDLE_LINE: [(&str, Option<&str>); 4] = [
    (MIDDLE_1, Some(STACK_MAIN)),
    (BOTTOM_1, Some(STACK_MAIN)),
    (BOTTOM_2, Some(BOTTOM_1)),
    (MIDDLE_MERGE, Some(MIDDLE_1)),
];
const TOP_LINE: [(&str, Option<&str>); 1] = [(TOP_1, Some(MIDDLE_MERGE))];

fn stack_refusal(
    listing: &str,
    candidate: &OpenPr,
    own: &str,
    judged: &Cell<usize>,
    judgement: &Judgement,
) -> Option<super::Refusal> {
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains(&format!("compare/main...{}", candidate.head_sha)) {
            return Ok(own.to_owned());
        }
        if joined.contains(&format!("compare/feature/merged-away...{MIDDLE_MERGE}")) {
            return Ok(
                json!({"total": 4, "shas": [MIDDLE_1, BOTTOM_1, BOTTOM_2, MIDDLE_MERGE]})
                    .to_string(),
            );
        }
        if joined.contains("OPEN") {
            return Ok(listing.to_owned());
        }
        Err(format!("unexpected: {joined}"))
    };
    let mut judge = |_: &OpenPr| {
        judged.set(judged.get() + 1);
        judgement.clone()
    };
    refusal_with(
        &run_gh,
        "Generous-Corp/pulp",
        "main",
        candidate.number,
        &candidate.head_ref,
        &candidate.head_sha,
        &mut PassCache::default(),
        &mut judge,
    )
}

#[test]
fn a_stacks_approved_bottom_is_not_held_by_a_child_that_merged_it() {
    let stack = stack3();
    let listing = parented_listing(&[
        (&stack.bottom, &BOTTOM_LINE),
        (&stack.middle, &MIDDLE_LINE),
        (&stack.top, &TOP_LINE),
    ])
    .to_string();
    let own = own_compare(&[(BOTTOM_1, STACK_MAIN), (BOTTOM_2, BOTTOM_1)]);
    let judged = Cell::new(0);
    let unapproved = Judgement::Unapproved("no counted APPROVE".to_owned());
    assert_eq!(
        stack_refusal(&listing, &stack.bottom, &own, &judged, &unapproved),
        None
    );
    assert_eq!(judged.get(), 0, "the bottom's own commits read no approval");
}

#[test]
fn an_approved_middle_is_still_held_by_its_unapproved_bottom() {
    let stack = stack3();
    let listing = parented_listing(&[
        (&stack.bottom, &BOTTOM_LINE),
        (&stack.middle, &MIDDLE_LINE),
        (&stack.top, &TOP_LINE),
    ])
    .to_string();
    let own = own_compare(&[
        (MIDDLE_1, STACK_MAIN),
        (BOTTOM_1, STACK_MAIN),
        (BOTTOM_2, BOTTOM_1),
        (MIDDLE_MERGE, MIDDLE_1),
    ]);
    let judged = Cell::new(0);
    let unapproved = Judgement::Unapproved("no counted APPROVE".to_owned());
    let refusal = stack_refusal(&listing, &stack.middle, &own, &judged, &unapproved)
        .expect("the middle carries the unapproved bottom");
    assert_eq!(refusal.token, "carries_unapproved_commits:#11765");
    assert!(refusal.detail.contains(BOTTOM_1), "{}", refusal.detail);
    assert!(refusal.detail.contains(BOTTOM_2), "{}", refusal.detail);
}

#[test]
fn a_head_built_on_an_unapproved_prs_line_is_still_refused() {
    // P branched from Q's commits: they are on both first-parent lines.
    const P_TIP: &str = "5700000000000000000000000000000000000d01";
    let q = open(11800, "feature/q", "main", BOTTOM_2, &[BOTTOM_1, BOTTOM_2]);
    let p = open(11801, "feature/p", "main", P_TIP, &[]);
    let listing = parented_listing(&[(&q, &BOTTOM_LINE)]).to_string();
    let own = own_compare(&[
        (BOTTOM_1, STACK_MAIN),
        (BOTTOM_2, BOTTOM_1),
        (P_TIP, BOTTOM_2),
    ]);
    let judged = Cell::new(0);
    let unapproved = Judgement::Unapproved("held".to_owned());
    let refusal = stack_refusal(&listing, &p, &own, &judged, &unapproved).expect("refused");
    assert_eq!(refusal.token, "carries_unapproved_commits:#11800");
}

#[test]
fn without_parents_a_stacks_bottom_is_judged_as_before() {
    // A listing without parents (read over REST) drops nothing.
    let stack = stack3();
    let listing = listing(&[(&stack.bottom, 2), (&stack.middle, 4), (&stack.top, 1)]).to_string();
    let own = own_compare(&[(BOTTOM_1, STACK_MAIN), (BOTTOM_2, BOTTOM_1)]);
    let judged = Cell::new(0);
    let unapproved = Judgement::Unapproved("no counted APPROVE".to_owned());
    let refusal = stack_refusal(&listing, &stack.bottom, &own, &judged, &unapproved)
        .expect("refused without parents");
    assert_eq!(refusal.token, "carries_unapproved_commits:#11772");
}
