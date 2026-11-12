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
//! The one stack that is excused without Q's approval: Q declares the stack
//! (a `stacked:<P>` label, or a chain of open pull requests' labels and base
//! branches reaching P), P is the older pull request, Q's head contains P's
//! exact head, and the shared commits are on P's first-parent chain (the
//! parent of a stack whose child targets the base and is held or
//! unreviewed). Work P merged in, an undeclared Q, a Q built on an older head
//! of P's, and a P newer than Q stay refused, and the refusal names the fix.

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
/// A P opened before Q.
const OLDER_P: u64 = 11930;
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

/// The stack from the deadlock: P (opened first) is C then E; Q, opened
/// later, labelled `stacked:<P>` and targeting the base, branched off P's
/// head and added F. The same graph as [`Q_ABOVE_P`], with P the older pull
/// request.
const STACK_ON_OLDER_P: Fixture = Fixture {
    p_number: OLDER_P,
    q_stacked_on: Some(OLDER_P),
    ..Q_ABOVE_P
};

#[test]
fn an_older_parent_is_admitted_past_its_held_or_unreviewed_child() {
    for verdict in [
        Judgement::Held("HOLD: carries the parent's commits".to_owned()),
        Judgement::Unapproved("no APPROVE".to_owned()),
    ] {
        let (refusal, _) = run(&STACK_ON_OLDER_P, &|_| verdict.clone());
        assert_eq!(
            refusal, None,
            "parent admitted past a child judged {verdict:?}"
        );
    }

    // Control: the same graph and verdict with P opened after Q is the HOLD's
    // counterexample, and is refused.
    let (refusal, _) = run(&Q_ABOVE_P, &|_| Judgement::Held("HOLD".to_owned()));
    assert_eq!(refused_token(refusal), CARRIES_Q);
}

#[test]
fn an_older_parent_whose_head_the_child_lacks_is_refused() {
    // P moved on to G after Q branched off E: Q carries P's older head, not
    // the head P is admitted at.
    const G: &str = "c500000000000000000000000000000000000000";
    let moved = Fixture {
        p_number: OLDER_P,
        p_head: G,
        p_range: &[(C, &[BASE_TIP]), (E, &[C]), (G, &[E])],
        q_head: F,
        q_range: &[(C, &[BASE_TIP]), (E, &[C]), (F, &[E])],
        q_stacked_on: Some(OLDER_P),
        others: &[],
    };
    let refusal = run(&moved, &|_| Judgement::Held("HOLD".to_owned()))
        .0
        .expect("refused");
    assert_eq!(refusal.token, CARRIES_Q);
    assert!(
        refusal
            .detail
            .contains("#11932 is on an older head of #11930: merge #11930 into #11932"),
        "names the fix: {}",
        refusal.detail
    );

    // Control: once Q contains P's current head, P is admitted.
    let caught_up = Fixture {
        q_head: F,
        q_range: &[(C, &[BASE_TIP]), (E, &[C]), (G, &[E]), (F, &[G])],
        ..moved
    };
    let (refusal, _) = run(&caught_up, &|_| Judgement::Held("HOLD".to_owned()));
    assert_eq!(refusal, None);
}

#[test]
fn an_older_parent_that_merged_in_the_childs_held_work_is_refused() {
    // P (older) merged Y, a commit off its own first-parent chain, and Q,
    // built on P's head, holds Y. Only the commits on P's first-parent chain
    // are P's own, so Y still refuses P.
    const Y: &str = "c600000000000000000000000000000000000000";
    const M: &str = "c700000000000000000000000000000000000000";
    let merged_in = Fixture {
        p_number: OLDER_P,
        p_head: M,
        p_range: &[(C, &[BASE_TIP]), (Y, &[BASE_TIP]), (M, &[C, Y])],
        q_head: F,
        q_range: &[(C, &[BASE_TIP]), (Y, &[BASE_TIP]), (M, &[C, Y]), (F, &[M])],
        q_stacked_on: Some(OLDER_P),
        others: &[],
    };
    let refusal = run(&merged_in, &|_| Judgement::Held("HOLD on Y".to_owned()))
        .0
        .expect("refused");
    assert_eq!(refusal.token, CARRIES_Q);
    assert!(refusal.detail.contains(Y), "names Y: {}", refusal.detail);
    assert!(
        !refusal.detail.contains(C),
        "C is on P's first-parent chain, so P's own: {}",
        refusal.detail
    );

    // Control: approving Q admits P.
    let (refusal, _) = run(&merged_in, &|_| Judgement::Approved);
    assert_eq!(refusal, None);
}

/// P was opened first (empty), then reset onto C, a commit a newer Q
/// introduced and is held on, and Q was then built on P's current head. From
/// the graph and the opening order alone this is the older-parent stack, so
/// only the declaration separates it: undeclared, it is refused, and the
/// detail names the label that clears it.
#[test]
fn an_undeclared_older_parent_is_refused_and_told_which_label_clears_it() {
    let reset_onto_q = Fixture {
        p_number: OLDER_P,
        p_head: C,
        p_range: &[(C, &[BASE_TIP])],
        q_head: F,
        q_range: &[(C, &[BASE_TIP]), (F, &[C])],
        q_stacked_on: None,
        others: &[],
    };
    let refusal = run(&reset_onto_q, &|_| Judgement::Held("HOLD on C".to_owned()))
        .0
        .expect("refused");
    assert_eq!(refusal.token, CARRIES_Q);
    assert!(refusal.detail.contains(C), "names C: {}", refusal.detail);
    assert!(
        refusal.detail.contains(
            "#11932 built on #11930's head but is not labelled stacked:11930: label it \
             (shipyard pr --stacked-on 11930) or retarget #11932 onto #11930's branch"
        ),
        "names the label that clears it: {}",
        refusal.detail
    );

    // Control: the same graph declared `stacked:<P>` is admitted. This is the
    // residual that remains, now declared by Q's author: the label event
    // names who declared it.
    let declared = Fixture {
        q_stacked_on: Some(OLDER_P),
        ..reset_onto_q
    };
    let (refusal, judged) = run(&declared, &|_| Judgement::Held("HOLD on C".to_owned()));
    assert_eq!(refusal, None);
    assert!(
        judged.is_empty() || judged == vec![F.to_owned()],
        "Q is judged, if at all, at its listed head: {judged:?}"
    );
}

/// An open pull request R between P and Q in age, with a commit P does not
/// carry.
const R_NUMBER: u64 = 11931;
const R_HEAD: &str = "c800000000000000000000000000000000000000";

#[test]
fn a_declared_stack_reaches_the_parent_through_an_intermediate_pull_request() {
    const R_LABELLED: [Row; 1] = [Row {
        number: R_NUMBER,
        head_ref: "feature/r",
        base_ref: "main",
        head: R_HEAD,
        range: &[(R_HEAD, &[BASE_TIP])],
        stacked_on: Some(OLDER_P),
    }];
    const R_ON_P_BRANCH: [Row; 1] = [Row {
        number: R_NUMBER,
        head_ref: "feature/r",
        base_ref: P_BRANCH,
        head: R_HEAD,
        range: &[(R_HEAD, &[E])],
        stacked_on: None,
    }];
    const R_UNRELATED: [Row; 1] = [Row {
        number: R_NUMBER,
        head_ref: "feature/r",
        base_ref: "main",
        head: R_HEAD,
        range: &[(R_HEAD, &[BASE_TIP])],
        stacked_on: None,
    }];
    for others in [&R_LABELLED[..], &R_ON_P_BRANCH[..]] {
        let through_r = Fixture {
            q_stacked_on: Some(R_NUMBER),
            others,
            ..STACK_ON_OLDER_P
        };
        let (refusal, _) = run(&through_r, &|_| Judgement::Held("HOLD".to_owned()));
        assert_eq!(refusal, None, "P reached through R");

        // Control: with R undeclared and off P's branch, Q's label on R no
        // longer reaches P.
        let (refusal, _) = run(
            &Fixture {
                others: &R_UNRELATED,
                ..through_r
            },
            &|_| Judgement::Held("HOLD".to_owned()),
        );
        assert_eq!(refused_token(refusal), CARRIES_Q);
    }
}

#[test]
fn a_declaration_does_not_clear_a_newer_parent() {
    let declared_newer = Fixture {
        q_stacked_on: Some(NEWER_P),
        ..Q_ABOVE_P
    };
    let refusal = run(&declared_newer, &|_| {
        Judgement::Held("HOLD on C".to_owned())
    })
    .0
    .expect("refused");
    assert_eq!(refusal.token, CARRIES_Q);
    assert!(
        refusal.detail.contains(
            "#11932 built on #11940's head but #11940 was opened after #11932: retarget \
             #11932 onto #11940's branch"
        ),
        "says P was opened after Q: {}",
        refusal.detail
    );

    // Control: the same declared stack with P the older pull request is
    // admitted, so only the opening order separates the two.
    let (refusal, _) = run(&STACK_ON_OLDER_P, &|_| Judgement::Held("HOLD".to_owned()));
    assert_eq!(refusal, None);
}
