//! Ownership by first arrival, end to end. P is refused for a commit it
//! shares with an open pull request Q that Q introduced, unless P's branch
//! held that commit strictly before Q's branch did, by the repository
//! activity log. A stacked child that targets the base therefore never blocks
//! its parent, held or not, while a commit that came to P from Q always
//! refuses P until Q is approved, however recent P's approval or however old
//! Q's HOLD. The fixtures are the live shapes from Generous-Corp/pulp (#12982,
//! #12930, #12884) and Generous-Corp/vellum (#798, #800, #801, #811 under
//! #803 and #805), a child that merges two parents, and the shapes the
//! reviews of #1754 named (1754-r1-f1, 1754-r1-f2).

use serde_json::json;

use super::{Judgement, OpenPr, PassCache, Refusal, refusal_with};

const BASE_TIP: &str = "b000000000000000000000000000000000000000";

/// In `mid_life`, leaves P out of the listing.
const UNLISTED_P: u64 = u64::MAX;

/// The `before` of a creation.
const ZERO: &str = "0000000000000000000000000000000000000000";

/// When every listed pull request was opened: after each fixture's branch
/// creations, as on GitHub, where a branch is pushed before its pull request
/// is opened.
const OPENED: &str = "2026-11-27T23:00:00Z";

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
                "createdAt": OPENED,
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

/// Each row's branch created once, at its head, in the order `numbers` gives.
fn created_in_order(rows: &[Row], numbers: &[u64]) -> Vec<(u64, Vec<(u32, &'static str)>)> {
    numbers
        .iter()
        .enumerate()
        .map(|(index, number)| {
            let row = rows.iter().find(|row| row.number == *number).expect("row");
            (
                *number,
                vec![(u32::try_from(index).expect("small") + 1, row.head)],
            )
        })
        .collect()
}

/// Run the guard for the row numbered `p` among `rows` (all listed open),
/// each other row judged `gate(number)`, branches logged as `logs` (`(hour,
/// sha the branch moved to)`, the first entry its creation), and `extra`
/// (`main...sha`) ranges for logged shas that are no row's head.
fn run_full(
    rows: &[Row],
    p: u64,
    logs: &[(u64, Vec<(u32, &'static str)>)],
    extra: &[(&str, &[Node])],
    gate: &dyn Fn(u64) -> Judgement,
) -> Option<Refusal> {
    run_logged(rows, p, logs, extra, &[], gate)
}

/// [`run_full`], where the logs of the rows in `mid_life` start after their
/// branch's creation (the log was truncated).
fn run_logged(
    rows: &[Row],
    p: u64,
    logs: &[(u64, Vec<(u32, &'static str)>)],
    extra: &[(&str, &[Node])],
    mid_life: &[u64],
    gate: &dyn Fn(u64) -> Judgement,
) -> Option<Refusal> {
    // P is listed like every open pull request, except when a test leaves
    // it out (`UNLISTED_P` in `mid_life`).
    let listed_rows: Vec<Row> = rows
        .iter()
        .copied()
        .filter(|row| !(row.number == p && mid_life.contains(&UNLISTED_P)))
        .collect();
    let listed = listing(&listed_rows);
    let run_gh = |argv: &[String]| -> Result<String, String> {
        let joined = argv.join(" ");
        if joined.contains("OPEN") {
            return Ok(listed.clone());
        }
        if joined.contains("/activity?") {
            for (number, entries) in logs {
                if joined.contains(&format!("ref=refs/heads/{}&", branch(*number))) {
                    return Ok(entries
                        .iter()
                        .enumerate()
                        .map(|(index, (hour, after))| {
                            // A `+` before the sha marks a later re-creation.
                            let (recreated, after) = after
                                .strip_prefix('+')
                                .map_or((false, *after), |sha| (true, sha));
                            let kind = if recreated || (index == 0 && !mid_life.contains(number)) {
                                "branch_creation"
                            } else {
                                "push"
                            };
                            // Each entry starts where the previous one left
                            // the branch; a creation starts from nothing.
                            let before = if kind == "branch_creation" || index == 0 {
                                ZERO.to_owned()
                            } else {
                                entries[index - 1].1.trim_start_matches('+').to_owned()
                            };
                            json!({"at": format!("2026-11-27T{hour:02}:00:00Z"),
                                "kind": kind, "before": before, "after": after})
                            .to_string()
                        })
                        .collect::<Vec<_>>()
                        .join("\n"));
                }
            }
            return Err("activity not served".to_owned());
        }
        if argv.iter().any(|arg| arg.ends_with("?per_page=1")) {
            return Ok("behind".to_owned());
        }
        for row in rows {
            if joined.contains(&format!("compare/main...{}?", row.head)) {
                return Ok(compare(row.range, joined.contains("parents")));
            }
        }
        for (sha, range) in extra {
            if joined.contains(&format!("compare/main...{sha}?")) {
                return Ok(compare(range, joined.contains("parents")));
            }
        }
        // The landed-content proof: an unreadable base ref keeps the overlap.
        Err(format!("not served: {joined}"))
    };
    let parent = rows.iter().find(|row| row.number == p).expect("P is a row");
    let mut judge = |q: &OpenPr| {
        assert_ne!(q.number, p, "P is never judged as its own carrier");
        gate(q.number)
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

/// Rows created in `order`, every other row judged `gate`.
fn run(rows: &[Row], p: u64, order: &[u64], gate: &dyn Fn(u64) -> Judgement) -> Option<Refusal> {
    run_full(rows, p, &created_in_order(rows, order), &[], gate)
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
fn a_parent_with_unreviewed_or_held_children_on_its_head_is_admitted() {
    for gate in [unreviewed as fn(u64) -> Judgement, held] {
        assert_eq!(run(&PULP_12982, 12982, &[12982, 12984, 12987], &gate), None);
    }

    // Control: had the children's branches held A first, A is theirs.
    let refused = refusal(run(&PULP_12982, 12982, &[12984, 12982, 12987], &unreviewed));
    assert_eq!(refused.token, "carries_unapproved_commits:#12984");
    assert!(
        refused
            .detail
            .contains("#12984's branch held 1 of them before #12982's did"),
        "{}",
        refused.detail
    );
}

#[test]
fn a_child_carrying_its_unapproved_parents_commits_is_refused() {
    // The parent's branch held A first, so A is the parent's: the child,
    // whose diff carries A, waits for the parent's approval.
    let refused = refusal(run(&PULP_12982, 12984, &[12982, 12984, 12987], &unreviewed));
    assert_eq!(refused.token, "carries_unapproved_commits:#12982");

    // Control: the parent approved admits the child.
    let parent_approved = |q: u64| {
        if q == 12982 {
            Judgement::Approved
        } else {
            unreviewed(q)
        }
    };
    assert_eq!(
        run(&PULP_12982, 12984, &[12982, 12984, 12987], &parent_approved),
        None
    );
}

#[test]
fn a_parent_with_a_held_child_on_an_older_head_is_admitted() {
    // #12962 branched off #12930's older head B3; #12930 moved on to B4.
    let logs = vec![(12930, vec![(1, B3), (3, B4)]), (12962, vec![(2, B_CHILD)])];
    let extra: [(&str, &[Node]); 1] = [(B3, &[(B1, &[BASE_TIP]), (B2, &[B1]), (B3, &[B2])])];
    assert_eq!(run_full(&PULP_12930, 12930, &logs, &extra, &held), None);

    // Control: the child's branch first.
    let child_first = vec![(12962, vec![(1, B_CHILD)]), (12930, vec![(2, B4)])];
    assert_eq!(
        refusal(run_full(&PULP_12930, 12930, &child_first, &extra, &held)).token,
        "carries_unapproved_commits:#12962"
    );
}

#[test]
fn a_parent_with_two_children_on_an_older_head_is_admitted() {
    let logs = vec![
        (12884, vec![(1, C1), (4, C2)]),
        (12889, vec![(2, C_CHILD_1)]),
        (12891, vec![(3, C_CHILD_2)]),
    ];
    let extra: [(&str, &[Node]); 1] = [(C1, &[(C1, &[BASE_TIP])])];
    assert_eq!(run_full(&PULP_12884, 12884, &logs, &extra, &held), None);

    // Control: P's log never shows C1 before C2 arrived, after both
    // children's branches.
    let late = vec![
        (12884, vec![(4, C2)]),
        (12889, vec![(2, C_CHILD_1)]),
        (12891, vec![(3, C_CHILD_2)]),
    ];
    assert!(run_full(&PULP_12884, 12884, &late, &extra, &held).is_some());
}

#[test]
fn each_parent_on_a_line_is_admitted_and_a_later_split_is_not() {
    // #798, #800, #801 were each pushed before the children; #811's branch
    // was made later from a point #803 already held.
    let logs = vec![
        (798, vec![(1, V1)]),
        (800, vec![(2, V2)]),
        (801, vec![(3, V3)]),
        (803, vec![(4, V_803)]),
        (805, vec![(5, V_805)]),
        (811, vec![(6, V4)]),
    ];
    let children_unapproved = |q: u64| {
        if matches!(q, 803 | 805 | 811) {
            held(q)
        } else {
            Judgement::Approved
        }
    };
    for parent in [798, 800, 801] {
        assert_eq!(
            run_full(&VELLUM, parent, &logs, &[], &children_unapproved),
            None,
            "#{parent} is admitted"
        );
    }
    assert_eq!(
        refusal(run_full(&VELLUM, 811, &logs, &[], &children_unapproved)).token,
        "carries_unapproved_commits:#803"
    );
}

#[test]
fn each_parent_of_a_merge_child_is_admitted() {
    for parent in [100, 101] {
        assert_eq!(
            run(&TWO_PARENTS, parent, &[100, 101, 110], &held),
            None,
            "#{parent} is admitted"
        );
    }
    // Control: the merge child's branch first holds both parents' commits.
    for parent in [100, 101] {
        assert_eq!(
            refusal(run(&TWO_PARENTS, parent, &[110, 100, 101], &held)).token,
            "carries_unapproved_commits:#110"
        );
    }
}

/// Q (#21, newer) introduced C and has no verdict at all; the older P (#20)
/// was then reset onto C and approved.
const C: &str = "aa00000000000000000000000000000000000000";
const P0: &str = "ab00000000000000000000000000000000000000";
const Q_TIP: &str = "ac00000000000000000000000000000000000000";
const RESET: [Row; 2] = [
    Row {
        number: 20,
        head: C,
        range: &[(C, &[BASE_TIP])],
        stacked_on: None,
    },
    Row {
        number: 21,
        head: Q_TIP,
        range: &[(C, &[BASE_TIP]), (Q_TIP, &[C])],
        stacked_on: Some(20),
    },
];

#[test]
fn review_r1_f2_a_parent_reset_onto_an_unheld_childs_commit_is_refused() {
    // The finding as written: no HOLD anywhere, Q merely unapproved.
    let logs = vec![(20, vec![(1, P0), (3, C)]), (21, vec![(2, Q_TIP)])];
    let extra: [(&str, &[Node]); 1] = [(P0, &[(P0, &[BASE_TIP])])];
    let refused = refusal(run_full(&RESET, 20, &logs, &extra, &unreviewed));
    assert_eq!(refused.token, "carries_unapproved_commits:#21");
    assert!(refused.detail.contains(C), "{}", refused.detail);

    // The fast-forward variant (no force-push): P's branch moved to Q's tip.
    let ff_rows = [
        Row {
            head: Q_TIP,
            range: &[(C, &[BASE_TIP]), (Q_TIP, &[C])],
            ..RESET[0]
        },
        RESET[1],
    ];
    let ff = vec![(20, vec![(1, P0), (3, Q_TIP)]), (21, vec![(2, Q_TIP)])];
    assert!(run_full(&ff_rows, 20, &ff, &extra, &unreviewed).is_some());

    // Control: Q approved admits P.
    assert_eq!(
        run_full(&RESET, 20, &logs, &extra, &|_| Judgement::Approved),
        None
    );
}

#[test]
fn review_r1_f1_an_ancestor_of_a_held_tip_never_lands_through_p() {
    // Q introduced C and is held at its tip, which P does not carry; P
    // carries only C, and P's approval may be as new as you like: no verdict
    // time is read, and C is Q's.
    let logs = vec![(21, vec![(1, Q_TIP)]), (20, vec![(2, C)])];
    let refused = refusal(run_full(&RESET, 20, &logs, &[], &held));
    assert_eq!(refused.token, "carries_unapproved_commits:#21");

    // Control: Q approved admits P.
    assert_eq!(
        run_full(&RESET, 20, &logs, &[], &|_| Judgement::Approved),
        None
    );
}

#[test]
fn an_unreadable_or_truncated_log_keeps_the_commit_the_childs() {
    // No activity served for the child's branch: fail closed.
    let p_only = vec![(12982, vec![(1, A)])];
    let refused = refusal(run_full(&PULP_12982, 12982, &p_only, &[], &unreviewed));
    assert!(
        refused.detail.contains("could not be settled"),
        "{}",
        refused.detail
    );

    // A child's log that does not start at its branch's creation: where it
    // first held A may be lost, so A stays the child's.
    let logs = created_in_order(&PULP_12982, &[12982, 12984, 12987]);
    let refused = refusal(run_logged(
        &PULP_12982,
        12982,
        &logs,
        &[],
        &[12984],
        &unreviewed,
    ));
    assert_eq!(refused.token, "carries_unapproved_commits:#12984");

    // The parent's and a child's branches holding A at the same instant: not
    // strictly first, so A is the child's.
    let same_time = vec![
        (12982, vec![(1, A)]),
        (12984, vec![(1, A_CHILD_1)]),
        (12987, vec![(3, A_CHILD_2)]),
    ];
    assert!(run_full(&PULP_12982, 12982, &same_time, &[], &unreviewed).is_some());

    // Control: the same logs, complete and in order, admit P.
    assert_eq!(run_full(&PULP_12982, 12982, &logs, &[], &unreviewed), None);
}

#[test]
fn a_parent_branch_reused_after_a_deletion_cannot_claim_its_earlier_life() {
    // P's branch name held C in an earlier life (hour 1), was deleted, and
    // was created again at C after Q's branch held C (hour 3).
    let logs = vec![
        (
            20,
            vec![(1, C), (3, "+aa00000000000000000000000000000000000000")],
        ),
        (21, vec![(2, Q_TIP)]),
    ];
    assert_eq!(
        refusal(run_full(&RESET, 20, &logs, &[], &unreviewed)).token,
        "carries_unapproved_commits:#21"
    );

    // Control: one life from hour 1, C is P's.
    let one_life = vec![(20, vec![(1, C)]), (21, vec![(2, Q_TIP)])];
    assert_eq!(run_full(&RESET, 20, &one_life, &[], &unreviewed), None);
}

#[test]
fn a_parent_missing_from_the_listing_owns_nothing() {
    let logs = created_in_order(&PULP_12982, &[12982, 12984, 12987]);
    let refused = refusal(run_logged(
        &PULP_12982,
        12982,
        &logs,
        &[],
        &[UNLISTED_P],
        &unreviewed,
    ));
    assert_eq!(refused.token, "carries_unapproved_commits:#12984");
    assert!(
        refused.detail.contains("could not be settled"),
        "{}",
        refused.detail
    );

    // Control: listed, the same logs admit P.
    assert_eq!(run_full(&PULP_12982, 12982, &logs, &[], &unreviewed), None);
}
