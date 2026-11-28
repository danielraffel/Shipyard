//! A parent pull request P is judged on its own approval when its only
//! "carried" commits are its own line, which an unreviewed child Q (opened
//! after P, refused only for want of an APPROVE) branched off, at P's current
//! head or an older one, labelled `stacked:<P>` or not. The fixtures are the
//! live shapes from Generous-Corp/pulp (#12982, #12930, #12884) and
//! Generous-Corp/vellum (#798, #800, #801, #811 under #803 and #805), plus a
//! child that merges two parents. Each admitted shape has a control that
//! changes one input (the child held, or P newer than the child) and is
//! refused, so a pass cannot come from a read failing closed or open.
//!
//! The real catches stay: P merged an unrelated unapproved pull request's
//! branch, a newer P built on an unapproved Q, a parent that merged its
//! unapproved child back in, and a child whose approval cannot be read.

use std::cell::RefCell;

use serde_json::json;

use super::{Judgement, OpenPr, PassCache, Refusal, refusal_with};

const BASE_TIP: &str = "b000000000000000000000000000000000000000";

/// One commit and its parents.
type Node = (&'static str, &'static [&'static str]);

/// One listed open pull request; every row targets `main`.
#[derive(Clone, Copy)]
struct Row {
    number: u64,
    head: &'static str,
    range: &'static [Node],
    stacked_on: Option<u64>,
}

fn branch(number: u64) -> String {
    format!("feature/{number}")
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
                "number": row.number, "headRefName": branch(row.number), "baseRefName": "main",
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

fn compare(range: &[Node], parents: bool) -> String {
    if parents {
        json!({
            "total": range.len(),
            "merge_base": BASE_TIP,
            "commits": range.iter().map(|(sha, parents)| json!({"sha": sha, "parents": parents})).collect::<Vec<_>>(),
        })
    } else {
        json!({
            "total": range.len(),
            "merge_base": BASE_TIP,
            "shas": range.iter().map(|(sha, _)| sha).collect::<Vec<_>>(),
        })
    }
    .to_string()
}

/// Run the guard for the row numbered `p` among `rows` (all listed open), with
/// each other row judged by `verdict(number)`.
fn run(rows: &[Row], p: u64, verdict: &dyn Fn(u64) -> Judgement) -> Option<Refusal> {
    let listed = listing(rows);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("OPEN") {
            return Ok(listed.clone());
        }
        if argv.iter().any(|arg| arg.ends_with("?per_page=1")) {
            return Ok("behind".to_owned());
        }
        for row in rows {
            if joined.contains(&format!("compare/main...{}?", row.head)) {
                return Ok(compare(row.range, joined.contains("parents")));
            }
        }
        // The landed-content proof: an unreadable base ref keeps the overlap.
        Err(format!("not served: {joined}"))
    };
    let parent = rows.iter().find(|row| row.number == p).expect("P is a row");
    let judged = RefCell::new(Vec::new());
    let mut judge = |q: &OpenPr| {
        assert_ne!(q.number, p, "P is never judged as its own carrier");
        judged.borrow_mut().push(q.number);
        verdict(q.number)
    };
    refusal_with(
        &run_gh,
        "Generous-Corp/pulp",
        "main",
        p,
        &branch(p),
        parent.head,
        &mut PassCache::default(),
        &mut judge,
    )
}

fn unreviewed(_: u64) -> Judgement {
    Judgement::Unapproved("head carries no approval".to_owned())
}

fn held(_: u64) -> Judgement {
    Judgement::Held("HOLD".to_owned())
}

/// Unreviewed except `number`, which is held.
fn held_only(number: u64) -> impl Fn(u64) -> Judgement {
    move |q| if q == number { held(q) } else { unreviewed(q) }
}

fn refusal(refusal: Option<Refusal>) -> Refusal {
    refusal.expect("refused")
}

// pulp #12982: two undeclared children built on the parent's current head.
const A: &str = "a100000000000000000000000000000000000000";
const A_CHILD_1: &str = "a200000000000000000000000000000000000000";
const A_CHILD_2: &str = "a300000000000000000000000000000000000000";
const PULP_12982: [Row; 3] = [
    Row {
        number: 12982,
        head: A,
        range: &[(A, &[BASE_TIP])],
        stacked_on: None,
    },
    Row {
        number: 12984,
        head: A_CHILD_1,
        range: &[(A, &[BASE_TIP]), (A_CHILD_1, &[A])],
        stacked_on: None,
    },
    Row {
        number: 12987,
        head: A_CHILD_2,
        range: &[(A, &[BASE_TIP]), (A_CHILD_2, &[A])],
        stacked_on: None,
    },
];

#[test]
fn a_parent_with_unreviewed_children_on_its_head_is_admitted() {
    assert_eq!(run(&PULP_12982, 12982, &unreviewed), None);

    // Control: one child held, the same graph is refused for that child, and
    // the child keeps the advice to label or retarget it.
    let refused = refusal(run(&PULP_12982, 12982, &held_only(12987)));
    assert_eq!(refused.token, "carries_unapproved_commits:#12987");
    assert!(refused.detail.contains(A), "names A: {}", refused.detail);
    assert!(
        refused
            .detail
            .contains("#12987 built on #12982's head but is not labelled stacked:12982"),
        "keeps the child's advice: {}",
        refused.detail
    );
    assert!(
        !refused.detail.contains("of #12984"),
        "the unreviewed child does not hold P: {}",
        refused.detail
    );
}

#[test]
fn a_child_carrying_an_unreviewed_parents_commits_is_refused() {
    // The children are newer than #12982, so each carries #12982's commit
    // (unreviewed here) and is refused, as before.
    let refused = refusal(run(&PULP_12982, 12984, &unreviewed));
    assert_eq!(refused.token, "carries_unapproved_commits:#12982");
    assert!(
        refused
            .detail
            .contains("#12982 built on #12984's head but #12984 was opened after #12982"),
        "says the child is newer: {}",
        refused.detail
    );

    // Control: approving the parent admits the child.
    let approved_parent = |q: u64| {
        if q == 12982 {
            Judgement::Approved
        } else {
            unreviewed(q)
        }
    };
    assert_eq!(run(&PULP_12982, 12984, &approved_parent), None);
}

#[test]
fn an_unreadable_child_on_the_parents_head_still_refuses() {
    let unreadable = |q: u64| {
        if q == 12984 {
            Judgement::Unreadable("#12984 moved after the listing".to_owned())
        } else {
            unreviewed(q)
        }
    };
    let refused = refusal(run(&PULP_12982, 12982, &unreadable));
    assert_eq!(refused.token, "carried_commits_unreadable:#12984");

    // Control: the same child read as unreviewed admits P.
    assert_eq!(run(&PULP_12982, 12982, &unreviewed), None);
}

// pulp #12930: a labelled child on an older head of the parent.
const B1: &str = "b100000000000000000000000000000000000000";
const B2: &str = "b200000000000000000000000000000000000000";
const B3: &str = "b300000000000000000000000000000000000000";
const B4: &str = "b400000000000000000000000000000000000000";
const B_CHILD: &str = "b500000000000000000000000000000000000000";
const PULP_12930: [Row; 2] = [
    Row {
        number: 12930,
        head: B4,
        range: &[(B1, &[BASE_TIP]), (B2, &[B1]), (B3, &[B2]), (B4, &[B3])],
        stacked_on: None,
    },
    Row {
        number: 12962,
        head: B_CHILD,
        range: &[
            (B1, &[BASE_TIP]),
            (B2, &[B1]),
            (B3, &[B2]),
            (B_CHILD, &[B3]),
        ],
        stacked_on: Some(12930),
    },
];

#[test]
fn a_parent_with_an_unreviewed_child_on_an_older_head_is_admitted() {
    assert_eq!(run(&PULP_12930, 12930, &unreviewed), None);

    // The same child without the label is admitted too.
    let undeclared = [
        PULP_12930[0],
        Row {
            stacked_on: None,
            ..PULP_12930[1]
        },
    ];
    assert_eq!(run(&undeclared, 12930, &unreviewed), None);

    // Control: the child held keeps the exact-head proof, and is refused.
    let refused = refusal(run(&PULP_12930, 12930, &held));
    assert_eq!(refused.token, "carries_unapproved_commits:#12962");
    assert!(
        refused
            .detail
            .contains("#12962 is on an older head of #12930: merge #12930 into #12962"),
        "keeps the child's advice: {}",
        refused.detail
    );
}

// pulp #12884: two labelled children on an older head of the parent.
const C1: &str = "c100000000000000000000000000000000000000";
const C2: &str = "c200000000000000000000000000000000000000";
const C_CHILD_1: &str = "c300000000000000000000000000000000000000";
const C_CHILD_2: &str = "c400000000000000000000000000000000000000";
const PULP_12884: [Row; 3] = [
    Row {
        number: 12884,
        head: C2,
        range: &[(C1, &[BASE_TIP]), (C2, &[C1])],
        stacked_on: None,
    },
    Row {
        number: 12889,
        head: C_CHILD_1,
        range: &[(C1, &[BASE_TIP]), (C_CHILD_1, &[C1])],
        stacked_on: Some(12884),
    },
    Row {
        number: 12891,
        head: C_CHILD_2,
        range: &[(C1, &[BASE_TIP]), (C_CHILD_2, &[C1])],
        stacked_on: Some(12884),
    },
];

#[test]
fn a_parent_with_two_unreviewed_children_on_an_older_head_is_admitted() {
    assert_eq!(run(&PULP_12884, 12884, &unreviewed), None);

    // Control: P opened after the children (the children built on P's
    // commits is then indistinguishable from P built on theirs) is refused.
    let newer = [
        Row {
            number: 12895,
            ..PULP_12884[0]
        },
        PULP_12884[1],
        PULP_12884[2],
    ];
    let refused = refusal(run(&newer, 12895, &unreviewed));
    assert_eq!(refused.token, "carries_unapproved_commits:#12889");
    assert!(refused.detail.contains("of #12891"), "{}", refused.detail);
}

// vellum: #798, #800 and #801 are successive heads of one line, #811 (opened
// after #803) is a later point on it, and #803 and #805 (both unreviewed) are
// built on #811's head.
const V1: &str = "d100000000000000000000000000000000000000";
const V2: &str = "d200000000000000000000000000000000000000";
const V3: &str = "d300000000000000000000000000000000000000";
const V4: &str = "d400000000000000000000000000000000000000";
const V_803: &str = "d500000000000000000000000000000000000000";
const V_805: &str = "d600000000000000000000000000000000000000";
const LINE: [Node; 4] = [(V1, &[BASE_TIP]), (V2, &[V1]), (V3, &[V2]), (V4, &[V3])];
const VELLUM: [Row; 6] = [
    Row {
        number: 798,
        head: V1,
        range: &[(V1, &[BASE_TIP])],
        stacked_on: None,
    },
    Row {
        number: 800,
        head: V2,
        range: &[(V1, &[BASE_TIP]), (V2, &[V1])],
        stacked_on: None,
    },
    Row {
        number: 801,
        head: V3,
        range: &[(V1, &[BASE_TIP]), (V2, &[V1]), (V3, &[V2])],
        stacked_on: None,
    },
    Row {
        number: 811,
        head: V4,
        range: &LINE,
        stacked_on: None,
    },
    Row {
        number: 803,
        head: V_803,
        range: &[LINE[0], LINE[1], LINE[2], LINE[3], (V_803, &[V4])],
        stacked_on: None,
    },
    Row {
        number: 805,
        head: V_805,
        range: &[LINE[0], LINE[1], LINE[2], LINE[3], (V_805, &[V4])],
        stacked_on: None,
    },
];

#[test]
fn each_older_parent_under_shared_unreviewed_children_is_admitted() {
    // Each older parent also carries its own older parents' commits; those
    // are approved here, so only the children are in question.
    let children_unreviewed = |q: u64| {
        if matches!(q, 803 | 805 | 811) {
            unreviewed(q)
        } else {
            Judgement::Approved
        }
    };
    for parent in [798, 800, 801] {
        assert_eq!(
            run(&VELLUM, parent, &children_unreviewed),
            None,
            "#{parent} is admitted on its own approval"
        );
    }

    // #811 was opened after #803 and #805: it may be built on their commits,
    // so it stays refused, and the detail says why.
    let refused = refusal(run(&VELLUM, 811, &children_unreviewed));
    assert_eq!(refused.token, "carries_unapproved_commits:#803");
    assert!(
        refused
            .detail
            .contains("#803 built on #811's head but #811 was opened after #803"),
        "{}",
        refused.detail
    );

    // Control: #803 held refuses #800.
    let child_held = |q: u64| {
        if q == 803 {
            held(q)
        } else {
            children_unreviewed(q)
        }
    };
    let refused = refusal(run(&VELLUM, 800, &child_held));
    assert_eq!(refused.token, "carries_unapproved_commits:#803");
}

// A child that merges two parents: M = merge(E1, F1), first parent E1.
const E1: &str = "e100000000000000000000000000000000000000";
const F1: &str = "f100000000000000000000000000000000000000";
const M: &str = "e200000000000000000000000000000000000000";
const TWO_PARENTS: [Row; 3] = [
    Row {
        number: 100,
        head: E1,
        range: &[(E1, &[BASE_TIP])],
        stacked_on: None,
    },
    Row {
        number: 101,
        head: F1,
        range: &[(F1, &[BASE_TIP])],
        stacked_on: None,
    },
    Row {
        number: 110,
        head: M,
        range: &[(E1, &[BASE_TIP]), (F1, &[BASE_TIP]), (M, &[E1, F1])],
        stacked_on: None,
    },
];

#[test]
fn each_parent_of_an_unreviewed_merge_child_is_admitted() {
    for parent in [100, 101] {
        assert_eq!(
            run(&TWO_PARENTS, parent, &unreviewed),
            None,
            "#{parent} is admitted on its own approval"
        );
    }

    // Control: the child held refuses the parent on its first-parent chain;
    // the one it merged in is P's own line either way.
    let refused = refusal(run(&TWO_PARENTS, 100, &held));
    assert_eq!(refused.token, "carries_unapproved_commits:#110");
    assert_eq!(run(&TWO_PARENTS, 101, &held), None);
}

#[test]
fn a_pr_that_merged_an_unrelated_unapproved_branch_is_refused() {
    // P (older) merged Y, Q's commit; Q is not built on P.
    const P1: &str = "f200000000000000000000000000000000000000";
    const Y: &str = "f300000000000000000000000000000000000000";
    const PM: &str = "f400000000000000000000000000000000000000";
    let rows = [
        Row {
            number: 199,
            head: PM,
            range: &[(P1, &[BASE_TIP]), (Y, &[BASE_TIP]), (PM, &[P1, Y])],
            stacked_on: None,
        },
        Row {
            number: 200,
            head: Y,
            range: &[(Y, &[BASE_TIP])],
            stacked_on: None,
        },
    ];
    let refused = refusal(run(&rows, 199, &unreviewed));
    assert_eq!(refused.token, "carries_unapproved_commits:#200");
    assert!(refused.detail.contains(Y), "names Y: {}", refused.detail);

    // Control: approving Q admits P.
    assert_eq!(run(&rows, 199, &|_| Judgement::Approved), None);
}

#[test]
fn a_newer_pr_built_on_an_unapproved_pr_is_refused() {
    // The normal stack: Q introduced G, P (newer) built H on G, and Q has
    // since moved past P's head.
    const G: &str = "f500000000000000000000000000000000000000";
    const H: &str = "f600000000000000000000000000000000000000";
    const K: &str = "f700000000000000000000000000000000000000";
    let rows = [
        Row {
            number: 301,
            head: H,
            range: &[(G, &[BASE_TIP]), (H, &[G])],
            stacked_on: None,
        },
        Row {
            number: 300,
            head: K,
            range: &[(G, &[BASE_TIP]), (H, &[G]), (K, &[H])],
            stacked_on: None,
        },
    ];
    let refused = refusal(run(&rows, 301, &unreviewed));
    assert_eq!(refused.token, "carries_unapproved_commits:#300");
    assert!(refused.detail.contains(G), "names G: {}", refused.detail);

    // Control: approving Q admits P.
    assert_eq!(run(&rows, 301, &|_| Judgement::Approved), None);
}

#[test]
fn a_parent_that_merged_its_unreviewed_child_back_is_refused() {
    // Q branched off P's older head P1 and added Q1; P then merged Q back.
    // P1 is P's own; Q1 came in through the merge's second parent.
    const P1: &str = "f800000000000000000000000000000000000000";
    const P2: &str = "f900000000000000000000000000000000000000";
    const Q1: &str = "fa00000000000000000000000000000000000000";
    const PM: &str = "fb00000000000000000000000000000000000000";
    let rows = [
        Row {
            number: 400,
            head: PM,
            range: &[(P1, &[BASE_TIP]), (P2, &[P1]), (Q1, &[P1]), (PM, &[P2, Q1])],
            stacked_on: None,
        },
        Row {
            number: 401,
            head: Q1,
            range: &[(P1, &[BASE_TIP]), (Q1, &[P1])],
            stacked_on: None,
        },
    ];
    let refused = refusal(run(&rows, 400, &unreviewed));
    assert_eq!(refused.token, "carries_unapproved_commits:#401");
    assert!(refused.detail.contains(Q1), "names Q1: {}", refused.detail);
    assert!(
        !refused.detail.contains(P1),
        "P1 is P's own: {}",
        refused.detail
    );

    // Control: approving Q admits P.
    assert_eq!(run(&rows, 400, &|_| Judgement::Approved), None);
}
