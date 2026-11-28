//! The HOLD-recency rule end to end: P is refused for a commit it shares
//! with another open pull request Q only while some counting HOLD, on any
//! head of Q's or of P's own, is at least as new as P's exact-head APPROVE.
//! Labels, opening order and ancestry never clear a HOLD. The fixtures are
//! the live shapes from Generous-Corp/pulp (#12982, #12930, #12884) and
//! Generous-Corp/vellum (#798, #800, #801, #811 under #803 and #805), a child
//! that merges two parents, and the two shapes the review of #1754 named.
//! Each admitted shape has a control that adds or retimes one HOLD and is
//! refused, so a pass cannot come from a read failing closed or open.

use std::cell::RefCell;
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::json;

use super::{Judgement, OpenPr, PassCache, Refusal, recency_judgement, refusal_with};
use crate::head_approval::{HoldRecord, VerdictHistory};

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

/// `2026-11-27T<hour>:00:00Z`.
fn at(hour: u32) -> DateTime<Utc> {
    format!("2026-11-27T{hour:02}:00:00Z")
        .parse()
        .expect("time")
}

/// Every pull request's verdicts, by number; a missing one has none.
#[derive(Clone, Default)]
struct Verdicts(BTreeMap<u64, VerdictHistory>);

impl Verdicts {
    /// `number` approved at `head` at `hour`.
    fn approve(mut self, number: u64, head: &str, hour: u32) -> Self {
        self.0.entry(number).or_default().approvals.push(
            crate::head_approval::history::ApproveRecord {
                head: head.to_owned(),
                at: at(hour),
                comment: None,
            },
        );
        self
    }

    /// `number` held at `head` at `hour`.
    fn hold(mut self, number: u64, head: &str, hour: u32) -> Self {
        self.0.entry(number).or_default().holds.push(HoldRecord {
            head: head.to_owned(),
            at: at(hour),
            record: "comment",
            id: Some(u64::from(hour)),
        });
        self
    }

    fn of(&self, number: u64) -> VerdictHistory {
        self.0.get(&number).cloned().unwrap_or_default()
    }
}

/// Run the guard for the row numbered `p` among `rows` (all listed open).
/// Every other row's gate refuses its head (`gate`, default unreviewed), and
/// the HOLD-recency rule then judges it from `verdicts`.
fn run_gated(
    rows: &[Row],
    p: u64,
    verdicts: &Verdicts,
    gate: &dyn Fn(u64) -> Judgement,
) -> Option<Refusal> {
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
    let p_history = verdicts.of(p);
    let mut judge = |q: &OpenPr| {
        assert_ne!(q.number, p, "P is never judged as its own carrier");
        judged.borrow_mut().push(q.number);
        recency_judgement(
            gate(q.number),
            p,
            parent.head,
            &p_history,
            q.number,
            &verdicts.of(q.number),
        )
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

fn run(rows: &[Row], p: u64, verdicts: &Verdicts) -> Option<Refusal> {
    run_gated(rows, p, verdicts, &|_| {
        Judgement::Unapproved("head carries no approval".to_owned())
    })
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
fn a_parent_with_unreviewed_children_on_its_head_is_admitted() {
    let approved = Verdicts::default().approve(12982, A, 10);
    assert_eq!(run(&PULP_12982, 12982, &approved), None);

    // Control: a HOLD on one child after P's approval refuses P, and names it.
    let held = approved.clone().hold(12987, A_CHILD_2, 11);
    let refused = refusal(run(&PULP_12982, 12982, &held));
    assert_eq!(refused.token, "carries_unapproved_commits:#12987");
    assert!(refused.detail.contains(A), "names A: {}", refused.detail);
    assert!(
        refused
            .detail
            .contains(&format!("a HOLD on #12987's head {A_CHILD_2}")),
        "names the HOLD: {}",
        refused.detail
    );
    assert!(
        !refused.detail.contains("of #12984"),
        "the unheld child does not hold P: {}",
        refused.detail
    );
}

#[test]
fn an_approval_after_the_hold_admits_the_parent() {
    let hold_then_approve = Verdicts::default()
        .hold(12987, A_CHILD_2, 9)
        .approve(12982, A, 10);
    assert_eq!(run(&PULP_12982, 12982, &hold_then_approve), None);

    // Control: the same HOLD and approval, the other way round, refuses.
    let approve_then_hold = Verdicts::default()
        .approve(12982, A, 9)
        .hold(12987, A_CHILD_2, 10);
    assert_eq!(
        refusal(run(&PULP_12982, 12982, &approve_then_hold)).token,
        "carries_unapproved_commits:#12987"
    );

    // A HOLD at the very instant of the approval is not older: refused.
    let same_instant = Verdicts::default()
        .approve(12982, A, 10)
        .hold(12987, A_CHILD_2, 10);
    assert!(run(&PULP_12982, 12982, &same_instant).is_some());
}

#[test]
fn a_child_is_held_back_by_its_parent_only_through_a_newer_hold() {
    // #12984 carries #12982's commit. #12984's approval reviewed it, so with
    // no HOLD it is admitted; a HOLD on #12982 after that approval refuses.
    let approved = Verdicts::default().approve(12984, A_CHILD_1, 10);
    assert_eq!(run(&PULP_12982, 12984, &approved), None);
    let held = approved.hold(12982, A, 11);
    assert_eq!(
        refusal(run(&PULP_12982, 12984, &held)).token,
        "carries_unapproved_commits:#12982"
    );
}

#[test]
fn an_unreadable_child_still_refuses() {
    let approved = Verdicts::default().approve(12982, A, 10);
    let unreadable = |q: u64| {
        if q == 12984 {
            Judgement::Unreadable("#12984 moved after the listing".to_owned())
        } else {
            Judgement::Unapproved("no approval".to_owned())
        }
    };
    let refused = refusal(run_gated(&PULP_12982, 12982, &approved, &unreadable));
    assert_eq!(refused.token, "carried_commits_unreadable:#12984");

    // Control: the same child read as unreviewed admits P.
    assert_eq!(run(&PULP_12982, 12982, &approved), None);
}

#[test]
fn review_f1_a_stacked_label_never_clears_a_newer_hold() {
    // #12962 is labelled stacked:12930 and built on an older head of it.
    let approved = Verdicts::default().approve(12930, B4, 10);
    assert_eq!(run(&PULP_12930, 12930, &approved), None);
    let held = approved.hold(12962, B_CHILD, 11);
    let refused = refusal(run(&PULP_12930, 12930, &held));
    assert_eq!(refused.token, "carries_unapproved_commits:#12962");

    // Control: the label removed changes nothing either way.
    let unlabelled = [
        PULP_12930[0],
        Row {
            stacked_on: None,
            ..PULP_12930[1]
        },
    ];
    assert_eq!(
        refusal(run(&unlabelled, 12930, &held)).token,
        "carries_unapproved_commits:#12962"
    );
    assert_eq!(
        run(
            &unlabelled,
            12930,
            &Verdicts::default().approve(12930, B4, 10)
        ),
        None
    );
}

#[test]
fn review_f2_a_parent_reset_onto_a_newer_childs_held_commit_is_refused() {
    // P (#500, opened first) was reset onto C, a commit of the newer Q
    // (#501). Q was held at an old head after P's approval, then pushed, so
    // its current head carries no verdict at all. The lower number does not
    // help P.
    const C: &str = "aa00000000000000000000000000000000000000";
    const Q_OLD: &str = "ab00000000000000000000000000000000000000";
    const Q_NEW: &str = "ac00000000000000000000000000000000000000";
    let rows = [
        Row {
            number: 500,
            head: C,
            range: &[(C, &[BASE_TIP])],
            stacked_on: None,
        },
        Row {
            number: 501,
            head: Q_NEW,
            range: &[(C, &[BASE_TIP]), (Q_NEW, &[C])],
            stacked_on: Some(500),
        },
    ];
    let held = Verdicts::default().approve(500, C, 10).hold(501, Q_OLD, 11);
    let refused = refusal(run(&rows, 500, &held));
    assert_eq!(refused.token, "carries_unapproved_commits:#501");
    assert!(refused.detail.contains(Q_OLD), "{}", refused.detail);

    // The reviewer's other shape: Q labelled stacked:500, containing P's
    // exact head, held at its current head after P's approval. The label and
    // the containment used to clear it; now it is refused.
    let held_now = Verdicts::default().approve(500, C, 10).hold(501, Q_NEW, 11);
    assert_eq!(
        refusal(run(&rows, 500, &held_now)).token,
        "carries_unapproved_commits:#501"
    );

    // Control: a review of P after that HOLD admits it.
    assert_eq!(run(&rows, 500, &held.approve(500, C, 12)), None);
}

#[test]
fn a_hold_on_the_parents_own_older_head_after_its_approval_refuses() {
    let held = Verdicts::default()
        .approve(12982, A, 10)
        .hold(12982, BASE_TIP, 11);
    assert_eq!(
        refusal(run(&PULP_12982, 12982, &held)).token,
        "carries_unapproved_commits:#12984"
    );
    // Control: that HOLD before the approval does not.
    let earlier = Verdicts::default()
        .hold(12982, BASE_TIP, 9)
        .approve(12982, A, 10);
    assert_eq!(run(&PULP_12982, 12982, &earlier), None);
}

#[test]
fn a_parent_without_an_exact_head_approve_is_refused_by_any_hold() {
    // P's admission rests on an approval carried from another head: there is
    // nothing to date, so any HOLD holds it.
    let held = Verdicts::default()
        .approve(12982, BASE_TIP, 10)
        .hold(12987, A_CHILD_2, 1);
    assert_eq!(
        refusal(run(&PULP_12982, 12982, &held)).token,
        "carries_unapproved_commits:#12987"
    );
    // Control: with no HOLD anywhere it is admitted.
    assert_eq!(run(&PULP_12982, 12982, &Verdicts::default()), None);
}

#[test]
fn a_parent_with_two_unreviewed_children_on_an_older_head_is_admitted() {
    let approved = Verdicts::default().approve(12884, C2, 10);
    assert_eq!(run(&PULP_12884, 12884, &approved), None);

    // Control: one child held after the approval refuses.
    let held = approved.hold(12891, C_CHILD_2, 11);
    assert_eq!(
        refusal(run(&PULP_12884, 12884, &held)).token,
        "carries_unapproved_commits:#12891"
    );
}

#[test]
fn each_parent_on_a_line_of_unreviewed_children_is_admitted() {
    let approved = Verdicts::default()
        .approve(798, V1, 10)
        .approve(800, V2, 10)
        .approve(801, V3, 10)
        .approve(811, V4, 10);
    for parent in [798, 800, 801, 811] {
        assert_eq!(
            run(&VELLUM, parent, &approved),
            None,
            "#{parent} is admitted on its own approval"
        );
    }

    // Control: #803 held after #800's approval refuses #800, and a review of
    // #801 after the HOLD keeps #801 admitted.
    let held = approved.hold(803, V_803, 11).approve(801, V3, 12);
    assert_eq!(
        refusal(run(&VELLUM, 800, &held)).token,
        "carries_unapproved_commits:#803"
    );
    assert_eq!(run(&VELLUM, 801, &held), None);
}

#[test]
fn each_parent_of_an_unreviewed_merge_child_is_admitted() {
    let approved = Verdicts::default()
        .approve(100, E1, 10)
        .approve(101, F1, 10);
    for parent in [100, 101] {
        assert_eq!(
            run(&TWO_PARENTS, parent, &approved),
            None,
            "#{parent} is admitted on its own approval"
        );
    }

    // Control: the child held after both approvals refuses both parents.
    let held = approved.hold(110, M, 11);
    for parent in [100, 101] {
        assert_eq!(
            refusal(run(&TWO_PARENTS, parent, &held)).token,
            "carries_unapproved_commits:#110"
        );
    }
}
