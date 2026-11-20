//! An approved head GitHub holds `UNSTABLE` on a red check that is neither
//! required nor advisory goes to the repair lane (`[auto_merge]
//! repair_unstable_reds`), within a route limit of two red runs per check at
//! one head; the next goes to the coordinator.
use super::*;
use crate::app::merge_steward_cmd::blocked_on::{GateRead, RowFacts, classify};
use crate::app::merge_steward_cmd::scheduling_loop::{self, LoopActor, LoopState};
use crate::app::merge_steward_cmd::unstable_repair::{self, Routes};
use crate::merge_steward::UnstableRed;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const AT: &str = "2026-11-19T09:00:00Z";

fn check(name: &str, conclusion: &str, id: u64) -> Value {
    json!({"__typename": "CheckRun", "name": name, "status": "COMPLETED",
           "conclusion": conclusion, "completedAt": AT, "databaseId": id,
           "detailsUrl": format!("https://github.com/owner/repo/actions/runs/{id}")})
}

fn unstable_pr(red: &str, red_conclusion: &str, macos: &str, red_id: u64) -> ObservedPr {
    parse_pr(
        &json!({
            "id": "PR_12", "number": 12, "isDraft": false, "baseRefName": "main",
            "headRefOid": HEAD, "headRefName": "feature", "mergeStateStatus": "UNSTABLE",
            "autoMergeRequest": null, "author": {"login": "app/shipyard-local"}, "labels": [],
            "statusCheckRollup": [check("macos", macos, 100), check(red, red_conclusion, red_id)],
        }),
        &BTreeMap::new(),
    )
    .expect("PR")
}

fn no_queue_policy(flag: bool, advisory: Option<&[&str]>) -> StewardPolicy {
    let mut policy = queue_policy();
    policy.merge_queue = false;
    policy.ejection_guards.repair_unstable_reds = flag;
    policy.ejection_guards.unstable_advisory_checks =
        advisory.map(|names| names.iter().map(|name| (*name).to_owned()).collect());
    policy
}

fn blocked(
    pr: &ObservedPr,
    policy: &StewardPolicy,
    approval: GateRead<'_>,
) -> (String, Option<String>) {
    let decision = classify_pr(&pr.fact, policy, &BTreeMap::new());
    let (stall, _) = classify(
        &RowFacts {
            pr: &pr.fact,
            decision: &decision,
            mutation: None,
            error: None,
            approval,
            enqueued_at: None,
            ejection: None,
        },
        policy,
    );
    (stall.blocked_on, stall.blocked_detail)
}

const SYNC: &str = "Enforce version & skill sync";

#[test]
fn an_approved_unstable_head_with_a_red_non_advisory_check_goes_to_repair() {
    let pr = unstable_pr(SYNC, "FAILURE", "SUCCESS", 201);
    let (token, detail) = blocked(&pr, &no_queue_policy(true, None), GateRead::Admits);
    assert_eq!(token, format!("unstable_failed:{SYNC}"));
    assert!(
        detail.as_deref().is_some_and(|d| d.contains("repair lane")),
        "{detail:?}"
    );
    assert_eq!(
        scheduling_loop::state_for(
            &token,
            &classify_pr(&pr.fact, &no_queue_policy(true, None), &BTreeMap::new()),
            false
        ),
        Some(LoopState::RequiredRed)
    );
    let next = crate::app::merge_steward_cmd::next_action::for_row(
        &crate::app::merge_steward_cmd::next_action::RowContext {
            repo: "owner/repo",
            base: "main",
            pr: &pr.fact,
            policy: &no_queue_policy(true, None),
            error: None,
        },
        &crate::app::merge_steward_cmd::blocked_on::Stall {
            blocked_on: token,
            ..Default::default()
        },
    );
    assert_eq!(
        next.actor,
        crate::app::merge_steward_cmd::next_action::Actor::Repair
    );
}

#[test]
fn an_advisory_listed_red_check_is_not_routed() {
    let pr = unstable_pr("risk-tier", "FAILURE", "SUCCESS", 201);
    let (token, _) = blocked(
        &pr,
        &no_queue_policy(true, Some(&["risk-tier"])),
        GateRead::Admits,
    );
    assert!(!token.starts_with("unstable_failed"), "{token}");
    // Control: the same red check, not listed, is routed.
    let (unlisted, _) = blocked(
        &pr,
        &no_queue_policy(true, Some(&["zizmor"])),
        GateRead::Admits,
    );
    assert_eq!(unlisted, "unstable_failed:risk-tier");
}

#[test]
fn a_required_red_is_unchanged() {
    // `macos` is required: the head is required_failed, never unstable_failed.
    let pr = unstable_pr(SYNC, "FAILURE", "FAILURE", 201);
    let off = blocked(&pr, &no_queue_policy(false, None), GateRead::Admits).0;
    let on = blocked(&pr, &no_queue_policy(true, None), GateRead::Admits).0;
    assert!(on.starts_with("required_failed:"), "{on}");
    assert_eq!(on, off);
    // Control: with the required check green, the same red is routed.
    let green = unstable_pr(SYNC, "FAILURE", "SUCCESS", 201);
    assert!(
        blocked(&green, &no_queue_policy(true, None), GateRead::Admits)
            .0
            .starts_with("unstable_failed:")
    );
}

#[test]
fn the_flag_off_is_unchanged() {
    let pr = unstable_pr(SYNC, "FAILURE", "SUCCESS", 201);
    assert_eq!(
        blocked(&pr, &no_queue_policy(false, None), GateRead::Admits).0,
        "merge_state:UNSTABLE"
    );
    // Control: the flag on routes the same head.
    assert!(
        blocked(&pr, &no_queue_policy(true, None), GateRead::Admits)
            .0
            .starts_with("unstable_failed:")
    );
}

#[test]
fn flag_on_only_adds_the_token_when_no_consumer_reads_it() {
    let pr = unstable_pr(SYNC, "FAILURE", "SUCCESS", 201);
    let off = no_queue_policy(false, None);
    let on = no_queue_policy(true, None);
    // The admission decision and all merge behavior remain identical: this
    // feature only gives the report a more actionable blocked_on value.
    assert_eq!(
        classify_pr(&pr.fact, &off, &BTreeMap::new()),
        classify_pr(&pr.fact, &on, &BTreeMap::new())
    );
    assert_eq!(
        blocked(&pr, &off, GateRead::Admits).0,
        "merge_state:UNSTABLE"
    );
    assert_eq!(
        blocked(&pr, &on, GateRead::Admits).0,
        format!("unstable_failed:{SYNC}")
    );
}

#[test]
fn a_pending_check_or_an_unapproved_head_is_not_routed() {
    let pending = parse_pr(
        &json!({
            "id": "PR_12", "number": 12, "isDraft": false, "baseRefName": "main",
            "headRefOid": HEAD, "headRefName": "feature", "mergeStateStatus": "UNSTABLE",
            "autoMergeRequest": null, "author": {"login": "app/shipyard-local"}, "labels": [],
            "statusCheckRollup": [check("macos", "SUCCESS", 100),
                {"__typename": "CheckRun", "name": SYNC, "status": "IN_PROGRESS",
                 "conclusion": null, "startedAt": AT, "databaseId": 201,
                 "detailsUrl": "https://github.com/owner/repo/actions/runs/201"}],
        }),
        &BTreeMap::new(),
    )
    .expect("PR");
    let policy = no_queue_policy(true, None);
    assert_eq!(
        blocked(&pending, &policy, GateRead::Admits).0,
        "merge_state:UNSTABLE"
    );
    // Not approved: the gate refuses, or nothing says the base needs approval.
    let red = unstable_pr(SYNC, "FAILURE", "SUCCESS", 201);
    // Control: the same red head, approved, is routed.
    assert!(
        blocked(&red, &policy, GateRead::Admits)
            .0
            .starts_with("unstable_failed:")
    );
    let blocker = crate::head_approval::ApprovalBlocker::NoVerdict;
    assert!(
        !blocked(&red, &policy, GateRead::Refused(&blocker))
            .0
            .starts_with("unstable_failed")
    );
    let mut unapproved = policy.clone();
    unapproved.approval_context = String::new();
    assert_eq!(
        blocked(&red, &unapproved, GateRead::Unread).0,
        "merge_state:UNSTABLE"
    );
}

#[test]
fn a_merge_queue_base_never_reads_the_flag() {
    let table: toml::Table = "[auto_merge]\nrepair_unstable_reds = true\n"
        .parse()
        .expect("toml");
    assert!(!crate::merge_steward::EjectionGuards::from_table(&table, true).repair_unstable_reds);
    assert!(crate::merge_steward::EjectionGuards::from_table(&table, false).repair_unstable_reds);
    let empty: toml::Table = "[auto_merge]\n".parse().expect("toml");
    assert!(!crate::merge_steward::EjectionGuards::from_table(&empty, false).repair_unstable_reds);
}

fn red(id: u64) -> UnstableRed {
    UnstableRed {
        name: SYNC.to_owned(),
        check_id: Some(id),
    }
}

#[test]
fn the_same_check_routed_twice_at_one_head_goes_to_the_coordinator() {
    let mut routes = BTreeMap::new();
    let open = std::iter::once("owner/repo#12".to_owned()).collect();
    let key = "owner/repo#12".to_owned();
    // Route 1 and route 2: two distinct red runs.
    for id in [201, 202] {
        let (token, _) = unstable_repair::token(&[red(id)], routes.get(&key), HEAD);
        assert!(token.starts_with("unstable_failed:"), "run {id}: {token}");
        unstable_repair::record(
            &mut routes,
            "owner/repo#",
            &[(key.clone(), HEAD.to_owned(), vec![red(id)])],
            &open,
        );
    }
    // The same run seen again is not a new route.
    assert!(
        unstable_repair::token(&[red(202)], routes.get(&key), HEAD)
            .0
            .starts_with("unstable_failed:")
    );
    // A third red run at the same head: the coordinator, not a third repair.
    let (token, detail) = unstable_repair::token(&[red(203)], routes.get(&key), HEAD);
    assert_eq!(token, format!("unstable_red_exhausted:{SYNC}"));
    assert!(detail.contains("coordinator"), "{detail}");
    let pr = unstable_pr(SYNC, "FAILURE", "SUCCESS", 203);
    let decision = classify_pr(&pr.fact, &no_queue_policy(true, None), &BTreeMap::new());
    let facts = scheduling_loop::RowFacts {
        number: 12,
        head: HEAD,
        token: &token,
        decision: &decision,
        since: None,
        since_source: None,
        dwell_since: None,
        enqueued_at: None,
    };
    let (state, _) =
        scheduling_loop::for_row("owner/repo", &facts, None, &BTreeMap::new(), Utc::now())
            .expect("loop");
    assert_eq!(state.actor, LoopActor::Coordinator);
    // A new head starts over.
    let other = "b".repeat(40);
    assert!(
        unstable_repair::token(&[red(204)], routes.get(&key), &other)
            .0
            .starts_with("unstable_failed:")
    );
    // A closed pull request is dropped.
    unstable_repair::record(
        &mut routes,
        "owner/repo#",
        &[],
        &std::collections::BTreeSet::new(),
    );
    assert!(routes.is_empty());
}

#[test]
fn routes_survive_a_ledger_round_trip() {
    let routes = Routes {
        head: HEAD.to_owned(),
        runs: vec![format!("{SYNC}\t201")],
    };
    let text = serde_json::to_string(&routes).expect("json");
    assert_eq!(
        serde_json::from_str::<Routes>(&text).expect("parse"),
        routes
    );
}
