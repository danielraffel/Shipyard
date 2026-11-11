use std::cell::RefCell;
use std::collections::BTreeMap;

use serde_json::json;

use super::*;

const CONTEXT: &str = "Versioning & Skill-Sync";
const GATE: &str = "macos";

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("fixture time")
        .with_timezone(&Utc)
}

fn now() -> DateTime<Utc> {
    at("2026-11-11T02:00:00Z")
}

fn check(context: &str, conclusion: &str) -> GroupCheck {
    GroupCheck {
        context: context.to_owned(),
        conclusion: conclusion.to_owned(),
        check_id: Some(1),
        completed_at: Some(at("2026-11-11T01:30:00Z")),
        title: Some(format!("{context} {conclusion}")),
    }
}

fn group(sha: &str, prs: &[u64], checks: &[(&str, &str)]) -> MergeGroup {
    MergeGroup {
        head_sha: sha.to_owned(),
        prs: prs.iter().copied().collect(),
        checks: checks
            .iter()
            .map(|(context, conclusion)| check(context, conclusion))
            .collect(),
    }
}

fn required() -> Vec<String> {
    vec![CONTEXT.to_owned(), GATE.to_owned()]
}

fn required_rules() -> Vec<RequiredCheck> {
    vec![
        RequiredCheck {
            context: CONTEXT.to_owned(),
            app_id: None,
        },
        RequiredCheck {
            context: GATE.to_owned(),
            app_id: None,
        },
    ]
}

#[test]
fn two_consecutive_groups_with_different_prs_failing_one_context_poison_it() {
    let groups = [
        group(
            "b",
            &[12016, 12017],
            &[(CONTEXT, "failure"), (GATE, "success")],
        ),
        group("a", &[12001], &[(CONTEXT, "failure"), (GATE, "success")]),
    ];
    let health = detect(&groups, &required(), now());
    assert_eq!(health.poisoned_contexts(), vec![CONTEXT.to_owned()]);
    let poison = &health.poisoned()[0];
    assert_eq!(poison.groups.len(), 2);
    assert_eq!(
        poison.first_error.as_deref(),
        Some("Versioning & Skill-Sync failure")
    );
}

#[test]
fn a_single_failing_group_is_not_poison() {
    // Negative control for the streak length: one failure, then a pass.
    let groups = [
        group("b", &[12016], &[(CONTEXT, "failure")]),
        group("a", &[12001], &[(CONTEXT, "success")]),
    ];
    assert!(matches!(
        detect(&groups, &required(), now()),
        MergeGroupHealth::Clear { .. }
    ));
}

#[test]
fn groups_sharing_one_pull_request_point_at_that_pull_request_not_the_base() {
    // Negative control for the "different PR sets" rule: the same culprit
    // rides in both failing groups.
    let groups = [
        group("b", &[12016, 12017], &[(CONTEXT, "failure")]),
        group("a", &[12016], &[(CONTEXT, "failure")]),
    ];
    let health = detect(&groups, &required(), now());
    let MergeGroupHealth::Clear { detail, .. } = health else {
        panic!("expected clear, got {health:?}");
    };
    assert!(detail.contains("#12016"), "{detail}");
}

#[test]
fn identical_pr_sets_are_not_poison() {
    let groups = [
        group("b", &[7], &[(CONTEXT, "failure")]),
        group("a", &[7], &[(CONTEXT, "failure")]),
    ];
    assert!(detect(&groups, &required(), now()).poisoned().is_empty());
}

#[test]
fn a_newer_passing_group_clears_the_poison() {
    let poisoned = [
        group("b", &[2], &[(CONTEXT, "failure")]),
        group("a", &[1], &[(CONTEXT, "failure")]),
    ];
    assert!(!detect(&poisoned, &required(), now()).poisoned().is_empty());
    let mut cleared = vec![group("c", &[12025], &[(CONTEXT, "success")])];
    cleared.extend(poisoned);
    assert!(detect(&cleared, &required(), now()).poisoned().is_empty());
}

#[test]
fn groups_without_a_verdict_neither_break_nor_extend_a_streak() {
    let groups = [
        group("d", &[4], &[(CONTEXT, "")]),
        group("c", &[3], &[(CONTEXT, "failure")]),
        group("b", &[2], &[(CONTEXT, "cancelled")]),
        group("a", &[1], &[(CONTEXT, "failure")]),
    ];
    let health = detect(&groups, &required(), now());
    assert_eq!(health.poisoned()[0].groups.len(), 2);
    // Control: the same history with a pass in the middle is clear.
    let broken = [
        group("c", &[3], &[(CONTEXT, "failure")]),
        group("b", &[2], &[(CONTEXT, "success")]),
        group("a", &[1], &[(CONTEXT, "failure")]),
    ];
    assert!(detect(&broken, &required(), now()).poisoned().is_empty());
}

#[test]
fn a_stale_streak_is_not_acted_on() {
    let groups = [
        group("b", &[2], &[(CONTEXT, "failure")]),
        group("a", &[1], &[(CONTEXT, "failure")]),
    ];
    let later = at("2026-11-11T09:00:00Z");
    let health = detect(&groups, &required(), later);
    let MergeGroupHealth::Clear { detail, .. } = health else {
        panic!("expected clear, got {health:?}");
    };
    assert!(detail.contains("stale"), "{detail}");
    // Control: the same groups read soon after are poisoned.
    assert!(!detect(&groups, &required(), now()).poisoned().is_empty());
}

#[test]
fn merged_prs_reads_merge_commit_subjects_only() {
    let prs = merged_prs(&[
        "Merge pull request #12016 from Generous-Corp/fix/a\n\nbody",
        "fix: unrelated #999",
        "Merge pull request #12017 from x/y",
    ]);
    assert_eq!(prs, BTreeSet::from([12016, 12017]));
}

/// A reader over canned responses keyed by API path, recording every call.
struct Canned {
    responses: BTreeMap<String, Value>,
    calls: RefCell<Vec<String>>,
}

impl Canned {
    fn call(&self, args: &[String]) -> Result<String, String> {
        let path = args.get(1).ok_or("no path")?.clone();
        self.calls.borrow_mut().push(path.clone());
        let key = path.split('?').next().unwrap_or(&path).to_owned();
        self.responses
            .get(&key)
            .map(Value::to_string)
            .ok_or_else(|| format!("gh: Not Found (HTTP 404) {key}"))
    }
}

fn check_runs(rows: &[(&str, &str, u64)]) -> Value {
    json!({"check_runs": rows.iter().map(|(name, conclusion, id)| json!({
        "id": id,
        "name": name,
        "status": "completed",
        "conclusion": conclusion,
        "completed_at": "2026-11-11T01:30:00Z",
        "output": {"title": format!("{name} {conclusion}")},
    })).collect::<Vec<_>>()})
}

fn check_runs_with_app(rows: &[(&str, &str, u64)], app_id: u64) -> Value {
    json!({"check_runs": rows.iter().map(|(name, conclusion, id)| json!({
        "id": id,
        "name": name,
        "status": "completed",
        "conclusion": conclusion,
        "completed_at": "2026-11-11T01:30:00Z",
        "output": {"title": format!("{name} {conclusion}")},
        "app": {"id": app_id},
    })).collect::<Vec<_>>()})
}

fn merge_group_runs(heads: &[(&str, u64)]) -> Value {
    json!({"workflow_runs": heads.iter().map(|(sha, pr)| json!({
        "head_sha": sha,
        "head_branch": format!("gh-readonly-queue/main/pr-{pr}-{sha}"),
    })).collect::<Vec<_>>()})
}

#[test]
fn read_returns_unknown_when_a_failing_group_membership_compare_is_unreadable() {
    let canned = Canned {
        responses: BTreeMap::from([
            (
                "repos/o/r/actions/runs".to_owned(),
                merge_group_runs(&[("bbb", 12016), ("bbb", 12016), ("aaa", 12001)]),
            ),
            (
                "repos/o/r/commits/bbb/check-runs".to_owned(),
                check_runs(&[
                    (CONTEXT, "failure", 20),
                    (CONTEXT, "success", 10),
                    (GATE, "success", 21),
                ]),
            ),
            (
                "repos/o/r/commits/aaa/check-runs".to_owned(),
                check_runs(&[(CONTEXT, "failure", 11), (GATE, "success", 12)]),
            ),
            (
                "repos/o/r/compare/main...bbb".to_owned(),
                json!({"commits": [
                    {"commit": {"message": "Merge pull request #12015 from a/b"}},
                    {"commit": {"message": "Merge pull request #12016 from c/d"}},
                ]}),
            ),
            (
                "repos/o/r/check-runs/20/annotations".to_owned(),
                json!([
                    {"annotation_level": "warning", "message": "not this"},
                    {"annotation_level": "failure", "message": "Process completed with exit code 1."},
                    {"annotation_level": "failure", "message": "\nsource-contract recheck overdue: figma due 2026-11-10\nmore"},
                ]),
            ),
        ]),
        calls: RefCell::default(),
    };
    let health = read(
        &|args| canned.call(args),
        "o/r",
        "main",
        &required_rules(),
        now(),
    );
    assert!(
        matches!(health, MergeGroupHealth::Unknown { .. }),
        "{health:?}"
    );
}

#[test]
fn read_returns_unknown_when_membership_compare_has_no_commits_array() {
    let canned = Canned {
        responses: BTreeMap::from([
            (
                "repos/o/r/actions/runs".to_owned(),
                merge_group_runs(&[("bbb", 12016), ("aaa", 12001)]),
            ),
            (
                "repos/o/r/commits/bbb/check-runs".to_owned(),
                check_runs(&[(CONTEXT, "failure", 20), (GATE, "success", 21)]),
            ),
            (
                "repos/o/r/commits/aaa/check-runs".to_owned(),
                check_runs(&[(CONTEXT, "failure", 11), (GATE, "success", 12)]),
            ),
            ("repos/o/r/compare/main...bbb".to_owned(), json!({})),
        ]),
        calls: RefCell::default(),
    };
    let health = read(
        &|args| canned.call(args),
        "o/r",
        "main",
        &required_rules(),
        now(),
    );
    assert!(
        matches!(health, MergeGroupHealth::Unknown { .. }),
        "{health:?}"
    );
}

#[test]
fn read_ignores_a_same_name_check_from_the_wrong_integration() {
    let required = vec![
        RequiredCheck {
            context: CONTEXT.to_owned(),
            app_id: Some(15_368),
        },
        RequiredCheck {
            context: GATE.to_owned(),
            app_id: None,
        },
    ];
    let canned = Canned {
        responses: BTreeMap::from([
            (
                "repos/o/r/actions/runs".to_owned(),
                merge_group_runs(&[("bbb", 12016), ("aaa", 12001)]),
            ),
            (
                "repos/o/r/commits/bbb/check-runs".to_owned(),
                check_runs_with_app(&[(CONTEXT, "failure", 20), (GATE, "success", 21)], 99),
            ),
            (
                "repos/o/r/commits/aaa/check-runs".to_owned(),
                check_runs_with_app(&[(CONTEXT, "failure", 11), (GATE, "success", 12)], 99),
            ),
        ]),
        calls: RefCell::default(),
    };
    let health = read(&|args| canned.call(args), "o/r", "main", &required, now());
    assert!(
        matches!(health, MergeGroupHealth::Clear { .. }),
        "{health:?}"
    );
}

#[test]
fn read_names_the_context_its_first_error_and_every_carried_pr() {
    let canned = Canned {
        responses: BTreeMap::from([
            (
                "repos/o/r/actions/runs".to_owned(),
                merge_group_runs(&[("bbb", 12016), ("bbb", 12016), ("aaa", 12001)]),
            ),
            (
                "repos/o/r/commits/bbb/check-runs".to_owned(),
                check_runs(&[
                    (CONTEXT, "failure", 20),
                    (CONTEXT, "success", 10),
                    (GATE, "success", 21),
                ]),
            ),
            (
                "repos/o/r/commits/aaa/check-runs".to_owned(),
                check_runs(&[(CONTEXT, "failure", 11), (GATE, "success", 12)]),
            ),
            (
                "repos/o/r/compare/main...bbb".to_owned(),
                json!({"commits": [
                    {"commit": {"message": "Merge pull request #12015 from a/b"}},
                    {"commit": {"message": "Merge pull request #12016 from c/d"}},
                ]}),
            ),
            (
                "repos/o/r/compare/main...aaa".to_owned(),
                json!({"commits": [
                    {"commit": {"message": "Merge pull request #12001 from e/f"}},
                ]}),
            ),
            (
                "repos/o/r/check-runs/20/annotations".to_owned(),
                json!([
                    {"annotation_level": "warning", "message": "not this"},
                    {"annotation_level": "failure", "message": "Process completed with exit code 1."},
                    {"annotation_level": "failure", "message": "\nsource-contract recheck overdue: figma due 2026-11-10\nmore"},
                ]),
            ),
        ]),
        calls: RefCell::default(),
    };
    let health = read(
        &|args| canned.call(args),
        "o/r",
        "main",
        &required_rules(),
        now(),
    );
    let poison = health.poisoned().first().expect("poisoned").clone();
    assert_eq!(poison.context, CONTEXT);
    assert_eq!(
        poison.first_error.as_deref(),
        Some("source-contract recheck overdue: figma due 2026-11-10")
    );
    assert_eq!(poison.groups[0].prs, vec![12015, 12016]);
    assert_eq!(poison.groups[1].prs, vec![12001]);
}

#[test]
fn read_stops_after_the_newest_group_passes_everything() {
    let canned = Canned {
        responses: BTreeMap::from([
            (
                "repos/o/r/actions/runs".to_owned(),
                merge_group_runs(&[("ccc", 3), ("bbb", 2), ("aaa", 1)]),
            ),
            (
                "repos/o/r/commits/ccc/check-runs".to_owned(),
                check_runs(&[(CONTEXT, "success", 30), (GATE, "success", 31)]),
            ),
        ]),
        calls: RefCell::default(),
    };
    let health = read(
        &|args| canned.call(args),
        "o/r",
        "main",
        &required_rules(),
        now(),
    );
    assert!(
        matches!(health, MergeGroupHealth::Clear { .. }),
        "{health:?}"
    );
    let calls = canned.calls.borrow();
    assert_eq!(calls.len(), 2, "{calls:?}");
}

#[test]
fn an_unreadable_run_list_is_unknown_never_poisoned() {
    let canned = Canned {
        responses: BTreeMap::new(),
        calls: RefCell::default(),
    };
    let health = read(
        &|args| canned.call(args),
        "o/r",
        "main",
        &required_rules(),
        now(),
    );
    assert!(
        matches!(health, MergeGroupHealth::Unknown { .. }),
        "{health:?}"
    );
}

#[test]
fn the_strict_predicate_needs_one_readable_error_shared_by_every_group() {
    let line = |text: &str| Some(text.to_owned());
    assert!(same_first_error(&[line("overdue"), line("overdue")]));
    assert!(!same_first_error(&[line("overdue"), line("timeout")]));
    assert!(!same_first_error(&[line("overdue"), None]));
    assert!(!same_first_error(&[None, None]));
    assert!(!same_first_error(&[line("overdue")]));
}
