//! [`p_owned`] against served activity logs and compares: every fail-closed
//! branch returns `Err`, and each has a control that settles.

use std::collections::BTreeMap;

use serde_json::json;

use super::{MAX_PUSHES_READ, p_owned};
use crate::app::merge_steward_cmd::carried_commits::{CommitList, OpenPr};

const ZERO: &str = "0000000000000000000000000000000000000000";
const BASE_TIP: &str = "b000000000000000000000000000000000000000";
const C: &str = "c100000000000000000000000000000000000000";
const Q_HEAD: &str = "c200000000000000000000000000000000000000";
const X: &str = "c300000000000000000000000000000000000000";
const P_BRANCH: &str = "feature/p";
const Q_BRANCH: &str = "feature/q";

fn q(created_at: &str, cross_repository: bool) -> OpenPr {
    OpenPr {
        number: 21,
        head_ref: Q_BRANCH.to_owned(),
        base_ref: "main".to_owned(),
        head_sha: Q_HEAD.to_owned(),
        cross_repository,
        author: None,
        commits: CommitList::Complete([C, Q_HEAD].iter().map(|sha| (*sha).to_owned()).collect()),
        stacked_on: None,
        created_at: Some(created_at.parse().expect("time")),
    }
}

/// `(hour, kind, before, after)`.
type Entry = (u32, &'static str, &'static str, &'static str);

fn entry((hour, kind, before, after): &Entry) -> String {
    json!({"at": format!("2026-11-27T{hour:02}:00:00Z"), "kind": kind, "before": before, "after": after})
        .to_string()
}

/// A served world: each branch's log, and `main...sha` as a list of commits
/// (or `None` for a compare over the cap).
struct World {
    logs: BTreeMap<&'static str, Vec<Entry>>,
    compares: BTreeMap<&'static str, Option<Vec<&'static str>>>,
}

impl World {
    /// P created at C (hour 1); Q created at its head, holding C (hour 2).
    fn parent_first() -> Self {
        Self {
            logs: BTreeMap::from([
                (P_BRANCH, vec![(1, "branch_creation", ZERO, C)]),
                (Q_BRANCH, vec![(2, "branch_creation", ZERO, Q_HEAD)]),
            ]),
            compares: BTreeMap::from([
                (C, Some(vec![C])),
                (Q_HEAD, Some(vec![C, Q_HEAD])),
                (X, Some(vec![X])),
            ]),
        }
    }

    fn settle(&self, q: &OpenPr, p_cross_repository: bool) -> Result<Vec<String>, String> {
        let run_gh = |argv: &[String]| -> Result<String, String> {
            let joined = argv.join(" ");
            if joined.contains("/activity?") {
                for (branch, entries) in &self.logs {
                    if joined.contains(&format!("ref=refs/heads/{branch}&")) {
                        return Ok(entries.iter().map(entry).collect::<Vec<_>>().join("\n"));
                    }
                }
                return Err("no log".to_owned());
            }
            for (sha, commits) in &self.compares {
                if joined.contains(&format!("compare/main...{sha}?")) {
                    return Ok(match commits {
                        Some(commits) => json!({
                            "total": commits.len(), "merge_base": BASE_TIP,
                            "commits": commits.iter().map(|sha| json!({"sha": sha, "parents": [BASE_TIP]})).collect::<Vec<_>>(),
                        }),
                        None => json!({"total": 300, "merge_base": BASE_TIP, "commits": []}),
                    }
                    .to_string());
                }
            }
            Err(format!("not served: {joined}"))
        };
        p_owned(
            &run_gh,
            "o/r",
            "main",
            P_BRANCH,
            p_cross_repository,
            q,
            &[C.to_owned()],
            &mut BTreeMap::new(),
            &mut BTreeMap::new(),
        )
        .map(|owned| owned.into_iter().collect())
    }
}

const OPENED: &str = "2026-11-27T23:00:00Z";

#[test]
fn the_control_world_settles_c_as_ps() {
    assert_eq!(
        World::parent_first().settle(&q(OPENED, false), false),
        Ok(vec![C.to_owned()])
    );
}

#[test]
fn a_cross_repository_branch_fails_closed() {
    let world = World::parent_first();
    assert!(
        world.settle(&q(OPENED, true), false).is_err(),
        "Q from a fork"
    );
    assert!(
        world.settle(&q(OPENED, false), true).is_err(),
        "P from a fork"
    );
}

#[test]
fn a_log_over_the_entry_cap_fails_closed() {
    let mut world = World::parent_first();
    let mut entries = vec![(1, "branch_creation", ZERO, C)];
    entries.extend((0..MAX_PUSHES_READ).map(|_| (1, "push", C, C)));
    world.logs.insert(P_BRANCH, entries);
    let error = world
        .settle(&q(OPENED, false), false)
        .expect_err("over the cap");
    assert!(error.contains("over the"), "{error}");
}

#[test]
fn an_over_cap_compare_while_walking_a_log_fails_closed() {
    let mut world = World::parent_first();
    world.compares.insert(C, None);
    let error = world
        .settle(&q(OPENED, false), false)
        .expect_err("over-cap compare");
    assert!(error.contains("over the read cap"), "{error}");
}

#[test]
fn a_log_that_never_shows_q_holding_c_fails_closed() {
    // The review's probe: Q's log shows only a creation at an unrelated
    // commit; P took C at hour 2. Q's head holds C, so the log is incomplete.
    let mut world = World::parent_first();
    world
        .logs
        .insert(P_BRANCH, vec![(2, "branch_creation", ZERO, C)]);
    world
        .logs
        .insert(Q_BRANCH, vec![(1, "branch_creation", ZERO, X)]);
    assert!(world.settle(&q(OPENED, false), false).is_err());

    // Even when the log's last entry names Q's head, a log whose entries up to
    // P's arrival never show C, and that has nothing after it, contradicts
    // Q's own diff.
    world
        .logs
        .insert(Q_BRANCH, vec![(1, "branch_creation", ZERO, Q_HEAD)]);
    world.compares.insert(Q_HEAD, Some(vec![Q_HEAD]));
    let error = world
        .settle(&q(OPENED, false), false)
        .expect_err("incomplete");
    assert!(error.contains("does not show it holding"), "{error}");

    // Control: Q's branch reaching its head after P's arrival settles C as P's.
    world.logs.insert(
        Q_BRANCH,
        vec![(1, "branch_creation", ZERO, X), (3, "push", X, Q_HEAD)],
    );
    world.compares.insert(Q_HEAD, Some(vec![C, Q_HEAD]));
    assert_eq!(
        world.settle(&q(OPENED, false), false),
        Ok(vec![C.to_owned()])
    );
}

#[test]
fn a_gap_in_qs_log_fails_closed() {
    let mut world = World::parent_first();
    world.logs.insert(
        Q_BRANCH,
        vec![
            (2, "branch_creation", ZERO, X),
            (3, "push", BASE_TIP, Q_HEAD),
        ],
    );
    let error = world.settle(&q(OPENED, false), false).expect_err("gap");
    assert!(error.contains("skips"), "{error}");

    // Control: the same entries chained.
    world.logs.insert(
        Q_BRANCH,
        vec![(2, "branch_creation", ZERO, X), (3, "push", X, Q_HEAD)],
    );
    assert_eq!(
        world.settle(&q(OPENED, false), false),
        Ok(vec![C.to_owned()])
    );
}

#[test]
fn a_q_branch_created_after_its_pull_request_was_opened_fails_closed() {
    // Renamed or recreated: Q's branch log starts after Q was opened.
    let world = World::parent_first();
    let error = world
        .settle(&q("2026-11-27T01:30:00Z", false), false)
        .expect_err("rename");
    assert!(error.contains("renamed or recreated"), "{error}");
}

#[test]
fn a_log_that_has_not_reached_qs_listed_head_fails_closed() {
    // Q's log ends at X (after P's arrival) while the listing names Q_HEAD:
    // the log lags the branch.
    let mut world = World::parent_first();
    world
        .logs
        .insert(P_BRANCH, vec![(2, "branch_creation", ZERO, C)]);
    world.logs.insert(
        Q_BRANCH,
        vec![
            (1, "branch_creation", ZERO, BASE_TIP),
            (3, "push", BASE_TIP, X),
        ],
    );
    world.compares.insert(BASE_TIP, Some(Vec::new()));
    let error = world
        .settle(&q(OPENED, false), false)
        .expect_err("lagging log");
    assert!(error.contains("has not reached"), "{error}");
}
