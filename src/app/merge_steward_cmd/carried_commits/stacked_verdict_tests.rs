//! A candidate P that shares commits with another open pull request Q is
//! cleared by Q's verdict at Q's listed head, never by the ancestry of the
//! two heads alone. Each test runs one fixture twice, varying only Q's
//! verdict, P's age, or whether the shared commits are on the base, and
//! asserts the two outcomes differ, so a pass cannot come from a read failing
//! closed.
//!
//! The fixture serves every compare the guard makes, including the
//! `<Q head>...<P head>` ancestry read as `behind` (P's head is in Q's
//! history), so a guard that excused Q on ancestry would admit P in the held
//! and unapproved cases.
//!
//! Here a held or unapproved Q stands for a Q with a HOLD at least as new as
//! P's exact-head approval; how HOLD recency is read and compared is tested
//! in `parent_child_tests`. No label, opening order or ancestry excuses Q.

use std::cell::{Cell, RefCell};

use serde_json::json;

use super::{Judgement, OpenPr, PassCache, Refusal, refusal_with};

const BASE_TIP: &str = "b000000000000000000000000000000000000000";
const C: &str = "c100000000000000000000000000000000000000";
const E: &str = "c200000000000000000000000000000000000000";
const F: &str = "c300000000000000000000000000000000000000";
const OLD_Q: &str = "c400000000000000000000000000000000000000";
/// A P opened after Q.
const NEWER_P: u64 = 11940;
const Q_NUMBER: u64 = 11932;
const P_BRANCH: &str = "feature/p";
const Q_BRANCH: &str = "feature/q";

/// One commit and its parents.
type Node = (&'static str, &'static [&'static str]);

/// P's range `main...P.head` and Q's listed head and range `main...Q.head`,
/// each a linear chain given oldest first, the `stacked:<n>` label Q carries,
/// and any other open pull requests listed beside Q.
struct Fixture {
    p_number: u64,
    p_head: &'static str,
    p_range: &'static [Node],
    q_head: &'static str,
    q_range: &'static [Node],
    q_stacked_on: Option<u64>,
    others: &'static [Row],
}

/// One listed open pull request.
#[derive(Clone, Copy)]
struct Row {
    number: u64,
    head_ref: &'static str,
    base_ref: &'static str,
    head: &'static str,
    range: &'static [Node],
    stacked_on: Option<u64>,
}

impl Fixture {
    /// Q, then the other rows. Q targets main, so only a declared stack (its
    /// label, or a chain of rows) can relate it to P.
    fn rows(&self) -> Vec<Row> {
        let q = Row {
            number: Q_NUMBER,
            head_ref: Q_BRANCH,
            base_ref: "main",
            head: self.q_head,
            range: self.q_range,
            stacked_on: self.q_stacked_on,
        };
        std::iter::once(q)
            .chain(self.others.iter().copied())
            .collect()
    }
}

fn listing(rows: &[Row]) -> String {
    let nodes: Vec<_> = rows
        .iter()
        .map(|row| {
            let labels: Vec<_> = row
                .stacked_on
                .map(|parent| json!({"name": format!("stacked:{parent}")}))
                .into_iter()
                .collect();
            json!({
                "number": row.number, "headRefName": row.head_ref, "baseRefName": row.base_ref,
                "headRefOid": row.head, "isCrossRepository": false,
                "author": {"login": "someone"},
                "labels": {"nodes": labels},
                "carriedCommits": {"totalCount": row.range.len(),
                    "nodes": row.range.iter().map(|(oid, _)| json!({"commit": {"oid": oid}})).collect::<Vec<_>>()},
            })
        })
        .collect();
    json!({"data": {"repository": {"pullRequests": {
        "pageInfo": {"hasNextPage": false, "endCursor": null},
        "nodes": nodes,
    }}}})
    .to_string()
}

fn graph(range: &[Node]) -> String {
    json!({
        "total": range.len(),
        "merge_base": BASE_TIP,
        "commits": range.iter().map(|(sha, parents)| json!({"sha": sha, "parents": parents})).collect::<Vec<_>>(),
    })
    .to_string()
}

fn shas(range: &[Node]) -> String {
    json!({
        "total": range.len(),
        "merge_base": BASE_TIP,
        "shas": range.iter().map(|(sha, _)| sha).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Run the guard for P with Q judged by `verdict(q's listed head)`. Returns
/// the refusal and the heads Q was judged at.
fn run(fixture: &Fixture, verdict: &dyn Fn(&str) -> Judgement) -> (Option<Refusal>, Vec<String>) {
    let listed = listing(&fixture.rows());
    let ancestry_reads = Cell::new(0);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("OPEN") {
            return Ok(listed.clone());
        }
        // The paged compare reads use `?per_page=N&page=M`; the ancestry probe
        // alone ends in `?per_page=1`.
        if argv.iter().any(|arg| arg.ends_with("?per_page=1")) {
            // Every head-ancestry probe says the probed commit is in the
            // other head's history.
            ancestry_reads.set(ancestry_reads.get() + 1);
            return Ok("behind".to_owned());
        }
        for (head, range) in [
            (fixture.p_head, fixture.p_range),
            (fixture.q_head, fixture.q_range),
        ] {
            if joined.contains(&format!("compare/main...{head}?")) {
                return Ok(if joined.contains("parents") {
                    graph(range)
                } else {
                    shas(range)
                });
            }
        }
        // The landed-content proof: an unreadable base ref keeps the overlap.
        Err(format!("not served: {joined}"))
    };
    let judged_at = RefCell::new(Vec::new());
    let mut judge = |q: &OpenPr| {
        assert_eq!(q.number, Q_NUMBER);
        judged_at.borrow_mut().push(q.head_sha.clone());
        verdict(&q.head_sha)
    };
    let refusal = refusal_with(
        &run_gh,
        "Generous-Corp/pulp",
        "main",
        fixture.p_number,
        P_BRANCH,
        fixture.p_head,
        &mut PassCache::default(),
        &mut judge,
    );
    (refusal, judged_at.into_inner())
}

fn refused_token(refusal: Option<Refusal>) -> String {
    refusal.expect("refused").token
}

const CARRIES_Q: &str = "carries_unapproved_commits:#11932";

/// P stacks on Q: Q's range is `C`, P's range is `C, E` with P built on C.
const P_ON_Q: Fixture = Fixture {
    p_number: NEWER_P,
    p_head: E,
    p_range: &[(C, &[BASE_TIP]), (E, &[C])],
    q_head: C,
    q_range: &[(C, &[BASE_TIP])],
    q_stacked_on: None,
    others: &[],
};

/// The merge-back graph from the HOLD: Q introduced C, P (opened after Q)
/// built E on C, then Q advanced to F on top of E. P's head E is a proper
/// ancestor of Q's head F, and C is Q's commit.
const Q_ABOVE_P: Fixture = Fixture {
    p_number: NEWER_P,
    p_head: E,
    p_range: &[(C, &[BASE_TIP]), (E, &[C])],
    q_head: F,
    q_range: &[(C, &[BASE_TIP]), (E, &[C]), (F, &[E])],
    q_stacked_on: None,
    others: &[],
};

#[test]
fn p_stacked_on_an_approved_q_is_admitted() {
    let (refusal, judged) = run(&P_ON_Q, &|_| Judgement::Approved);
    assert_eq!(refusal, None);
    assert_eq!(judged, vec![C.to_owned()], "Q is judged at its listed head");

    // Control: the same graph with Q unapproved is refused.
    let (refusal, _) = run(&P_ON_Q, &|_| Judgement::Unapproved("no APPROVE".to_owned()));
    assert_eq!(refused_token(refusal), CARRIES_Q);
}

#[test]
fn p_stacked_on_a_held_q_is_refused() {
    let (refusal, judged) = run(&P_ON_Q, &|_| Judgement::Held("HOLD".to_owned()));
    assert_eq!(refused_token(refusal), CARRIES_Q);
    assert_eq!(judged, vec![C.to_owned()]);

    // Control: approving Q alone admits P.
    let (refusal, _) = run(&P_ON_Q, &|_| Judgement::Approved);
    assert_eq!(refusal, None);
}

#[test]
fn p_stacked_on_an_unapproved_q_is_refused() {
    let (refusal, _) = run(&P_ON_Q, &|_| Judgement::Unapproved("no APPROVE".to_owned()));
    assert_eq!(refused_token(refusal), CARRIES_Q);

    // Control.
    let (refusal, _) = run(&P_ON_Q, &|_| Judgement::Approved);
    assert_eq!(refusal, None);
}

#[test]
fn p_stacked_on_a_q_approved_at_a_different_head_is_refused() {
    // The exact-head gate approves only OLD_Q; Q's listed head is C.
    let approved_at = |approved: &'static str| {
        move |head: &str| {
            if head == approved {
                Judgement::Approved
            } else {
                Judgement::Unapproved(format!("approved at {approved}, not {head}"))
            }
        }
    };
    let (refusal, judged) = run(&P_ON_Q, &approved_at(OLD_Q));
    assert_eq!(refused_token(refusal), CARRIES_Q);
    assert_eq!(judged, vec![C.to_owned()]);

    // Control: an approval at the listed head admits P.
    let (refusal, _) = run(&P_ON_Q, &approved_at(C));
    assert_eq!(refusal, None);
}

#[test]
fn p_carrying_q_commits_already_on_the_base_is_admitted() {
    // C landed on main: P's range is only E, while the still-open Q (held)
    // lists C.
    let landed = Fixture {
        p_number: NEWER_P,
        p_head: E,
        p_range: &[(E, &[C])],
        q_head: C,
        q_range: &[(C, &[BASE_TIP])],
        q_stacked_on: None,
        others: &[],
    };
    let (refusal, judged) = run(&landed, &|_| Judgement::Held("HOLD".to_owned()));
    assert_eq!(refusal, None);
    assert!(judged.is_empty(), "no overlap, so Q is never judged");

    // Control: the same held Q refuses while C is not on the base.
    let (refusal, _) = run(&P_ON_Q, &|_| Judgement::Held("HOLD".to_owned()));
    assert_eq!(refused_token(refusal), CARRIES_Q);
}

#[test]
fn a_held_q_that_contains_p_head_still_refuses_p() {
    // The HOLD's counterexample: ancestry says Q sits above P, but C is Q's
    // held commit. Q's verdict, not ancestry, decides.
    let (refusal, judged) = run(&Q_ABOVE_P, &|_| Judgement::Held("HOLD on C".to_owned()));
    let refusal = refusal.expect("refused");
    assert_eq!(refusal.token, CARRIES_Q);
    assert!(
        refusal.detail.contains(C),
        "names the held commit: {}",
        refusal.detail
    );
    assert_eq!(judged, vec![F.to_owned()], "Q is judged at its listed head");

    // An unapproved (not held) Q above P refuses too.
    let (refusal, _) = run(&Q_ABOVE_P, &|_| {
        Judgement::Unapproved("no APPROVE".to_owned())
    });
    assert_eq!(refused_token(refusal), CARRIES_Q);

    // Control: once Q's head is approved, P is admitted.
    let (refusal, _) = run(&Q_ABOVE_P, &|_| Judgement::Approved);
    assert_eq!(refusal, None);
}
