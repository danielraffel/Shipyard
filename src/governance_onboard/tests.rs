use std::cell::RefCell;
use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::*;
use crate::governance_audit::GithubReader;

const REPO: &str = "danielraffel/spectr";
const VERSION: &str = "0.414.0";
const PIN: &str = "341cefbde6e06d9454c5233291640c2687c6e7ba";
const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HEAD_C: &str = "cccccccccccccccccccccccccccccccccccccccc";

/// A GET-only fake keyed by path. Unlisted paths answer 404; listed errors fail.
#[derive(Default)]
struct Fake {
    json: BTreeMap<String, Value>,
    raw: BTreeMap<String, String>,
    errors: BTreeMap<String, String>,
    calls: RefCell<Vec<String>>,
}

impl Fake {
    fn answer<T: Clone>(&self, map: &BTreeMap<String, T>, path: &str) -> Result<Option<T>, String> {
        self.calls.borrow_mut().push(path.to_owned());
        if let Some(error) = self.errors.get(path) {
            return Err(error.clone());
        }
        Ok(map.get(path).cloned())
    }
}

impl GithubReader for Fake {
    fn get_json(&self, path: &str) -> Result<Option<Value>, String> {
        self.answer(&self.json, path)
    }

    fn get_raw(&self, path: &str) -> Result<Option<String>, String> {
        self.answer(&self.raw, path)
    }
}

fn status_path(head: &str) -> String {
    format!("repos/{REPO}/commits/{head}/status?per_page=100")
}

fn runs_path(head: &str) -> String {
    format!(
        "repos/{REPO}/commits/{head}/check-runs?check_name=review-verdict&app_id=15368&filter=latest"
    )
}

fn rulesets_path() -> String {
    format!("repos/{REPO}/rulesets?includes_parents=true&per_page=100&page=1")
}

fn pulls_path(page: usize) -> String {
    format!("repos/{REPO}/pulls?state=open&per_page=100&page={page}")
}

fn config_path() -> String {
    format!("repos/{REPO}/contents/.shipyard/config.toml?ref=main")
}

fn no_status() -> Value {
    json!({"total_count": 0, "statuses": []})
}

fn no_runs() -> Value {
    json!({"total_count": 0, "check_runs": []})
}

/// Three open PRs: A already has a review-verdict status, B has an Actions
/// check run, C has neither; D is a draft.
fn healthy() -> Fake {
    let mut fake = Fake::default();
    fake.json
        .insert(format!("repos/{REPO}"), json!({"default_branch": "main"}));
    fake.json.insert(
        format!("repos/{SHIPYARD_REPO}/commits/v{VERSION}"),
        json!({"sha": PIN}),
    );
    fake.json.insert(
        rulesets_path(),
        json!([{"id": 1, "name": "main", "enforcement": "active"}]),
    );
    fake.json.insert(
        pulls_path(1),
        json!([
            {"number": 1, "draft": false, "head": {"sha": HEAD_A}},
            {"number": 2, "draft": false, "head": {"sha": HEAD_B}},
            {"number": 3, "draft": false, "head": {"sha": HEAD_C}},
            {"number": 4, "draft": true, "head": {"sha": "d".repeat(40)}},
        ]),
    );
    fake.json.insert(
        status_path(HEAD_A),
        json!({"total_count": 2, "statuses": [
            {"context": "ci", "state": "success"},
            {"context": "review-verdict", "state": "pending"}
        ]}),
    );
    fake.json.insert(runs_path(HEAD_A), no_runs());
    fake.json.insert(status_path(HEAD_B), no_status());
    fake.json.insert(
        runs_path(HEAD_B),
        json!({"total_count": 1, "check_runs": [
            {"id": 77, "conclusion": "failure", "app": {"id": 15368}}
        ]}),
    );
    fake.json.insert(status_path(HEAD_C), no_status());
    fake.json.insert(runs_path(HEAD_C), no_runs());
    fake
}

fn request() -> OnboardRequest {
    OnboardRequest {
        repo: REPO.to_owned(),
        profile: "fleet".to_owned(),
        reviewer_signers: vec!["pulp-review".to_owned()],
        stamp_sessions: vec!["fleet-pass-reviewer-*".to_owned()],
        shipyard_version: VERSION.to_owned(),
        ..OnboardRequest::default()
    }
}

#[test]
fn allowlist_rejects_an_empty_list_and_an_extra_file() {
    assert!(validate_path_allowlist(&[]).is_err());
    assert!(validate_path_allowlist(&["src/lib.rs".to_owned()]).is_err());
    let all: Vec<String> = ALLOWED_PATHS
        .iter()
        .map(|path| (*path).to_owned())
        .collect();
    assert!(validate_path_allowlist(&all).is_ok());
}

#[test]
fn a_non_owned_repository_is_refused_before_any_read() {
    let fake = healthy();
    let mut request = request();
    request.repo = "upstream/project".to_owned();
    let error = build_plan(&fake, &request).expect_err("refused");
    assert_eq!(error.exit_code(), 1, "{error:?}");
    assert!(fake.calls.borrow().is_empty(), "{:?}", fake.calls.borrow());
}

#[test]
fn repo_slugs_with_path_components_are_refused() {
    for bad in [
        "./x",
        "danielraffel/..",
        "danielraffel/.",
        "a/b/c",
        "a b/c",
        "/x",
        "x/",
    ] {
        assert!(validate_repo(bad).is_err(), "{bad}");
    }
    assert!(validate_repo("danielraffel/spectr.v2").is_ok());
}

#[test]
fn through_accepts_every_step_name() {
    for step in STEPS {
        assert_eq!(resolve_through(Some(step), false), Ok(*step));
    }
    assert_eq!(resolve_through(None, true), Ok("audited"));
    assert_eq!(resolve_through(None, false), Ok("enrolled"));
    assert!(resolve_through(Some("pr"), true).is_err());
    assert!(resolve_through(Some("bogus"), false).is_err());
}

#[test]
fn the_rendered_workflow_runs_the_pinned_shipyard_commit() {
    let pin = ShipyardPin {
        version: VERSION.to_owned(),
        commit: PIN.to_owned(),
    };
    let rendered = render_review_verdict(&pin);
    assert!(rendered.contains("          repository: danielraffel/Shipyard\n"));
    assert!(rendered.contains(&format!(
        "          # Shipyard v{VERSION}\n          ref: {PIN}\n"
    )));
    assert!(!rendered.contains("default_branch"), "{rendered}");
    assert!(!rendered.contains("@COMMIT@") && !rendered.contains("@VERSION@"));
    assert!(rendered.contains("  workflow_dispatch:\n    inputs:\n      pull_request:\n"));
}

#[test]
fn the_rendered_concurrency_group_matches_shipyards_own_workflow() {
    let own = include_str!("../../.github/workflows/review-verdict.yml");
    let group = |text: &str| -> Vec<String> {
        text.lines()
            .map(str::trim)
            .filter(|line| line.starts_with("group: review-verdict-"))
            .map(str::to_owned)
            .collect()
    };
    let pin = ShipyardPin {
        version: VERSION.to_owned(),
        commit: PIN.to_owned(),
    };
    assert_eq!(group(&render_review_verdict(&pin)), group(own));
    assert_eq!(group(own).len(), 1);
}

#[test]
fn the_pin_must_be_a_full_commit() {
    let mut fake = Fake::default();
    let path = format!("repos/{SHIPYARD_REPO}/commits/v{VERSION}");
    assert_eq!(resolve_pin(&fake, VERSION).expect_err("404").exit_code(), 9);
    fake.json.insert(path.clone(), json!({"sha": "abc123"}));
    assert_eq!(
        resolve_pin(&fake, VERSION).expect_err("short").exit_code(),
        9
    );
    fake.json.insert(path, json!({"sha": PIN.to_uppercase()}));
    assert_eq!(resolve_pin(&fake, VERSION).expect("pin").commit, PIN);
}

#[test]
fn config_render_keeps_every_unrelated_key_and_comment() {
    let existing = "\
# project policy
[project]
profile = \"multi\"

[auto_merge]
arm_requires_head_approval = true # strict pr_author rule
queue = true
reviewer_signers = [\"infra-review2\"]

[validation]
pr_gates = true
extra = 3
";
    let rendered = render_config(
        Some(existing),
        &[],
        &["fleet-pass-reviewer-*".to_owned()],
        false,
    )
    .expect("render");
    for kept in [
        "# project policy",
        "profile = \"multi\"",
        "arm_requires_head_approval = true # strict pr_author rule",
        "queue = true",
        "reviewer_signers = [\"infra-review2\"]",
        "extra = 3",
    ] {
        assert!(rendered.text.contains(kept), "{kept}\n{}", rendered.text);
    }
    let table: toml::Table = toml::from_str(&rendered.text).expect("toml");
    assert_eq!(
        table["auto_merge"]["fleet_review_required"].as_bool(),
        Some(true)
    );
    assert_eq!(
        table["validation"]["pr_gates"].as_bool(),
        Some(true),
        "an existing true is never lowered"
    );
    assert_eq!(
        rendered.changed_keys,
        vec![
            "auto_merge.reviewer_stamp_sessions",
            "auto_merge.fleet_review_required",
        ]
    );
}

#[test]
fn pr_gates_is_added_only_when_absent_and_the_repo_has_no_gate_scripts() {
    let s = ["s".to_owned()];
    let g = ["g".to_owned()];
    let pr_gates = |existing: Option<&str>, gates: bool| -> Option<bool> {
        let rendered = render_config(existing, &s, &g, gates).expect("render");
        let table: toml::Table = toml::from_str(&rendered.text).expect("toml");
        table
            .get("validation")
            .and_then(|validation| validation.get("pr_gates"))
            .and_then(toml::Value::as_bool)
    };
    assert_eq!(pr_gates(None, false), Some(false));
    assert_eq!(
        pr_gates(None, true),
        None,
        "a repo with gate scripts keeps its gates"
    );
    for gates in [false, true] {
        assert_eq!(
            pr_gates(Some("[validation]\npr_gates = true\n"), gates),
            Some(true)
        );
        assert_eq!(
            pr_gates(Some("[validation]\npr_gates = false\n"), gates),
            Some(false)
        );
    }
}

#[test]
fn gate_script_paths_follow_the_config_overrides() {
    assert_eq!(
        gate_script_paths(None),
        [
            "tools/scripts/skill_sync_check.py",
            "scripts/skill_sync_check.py",
            "tools/scripts/version_bump_check.py",
            "scripts/version_bump_check.py",
        ]
    );
    assert_eq!(
        gate_script_paths(Some(
            "[validation]\nskill_sync_script = \"./ci/skills.py\"\n"
        )),
        [
            "ci/skills.py",
            "tools/scripts/version_bump_check.py",
            "scripts/version_bump_check.py",
        ]
    );
}

#[test]
fn a_key_replacing_a_table_header_is_written_with_spaces() {
    let rendered = render_config(
        Some("[auto_merge.reviewer_signers]\nx = 1\n"),
        &["s".to_owned()],
        &["g".to_owned()],
        false,
    )
    .expect("render");
    assert!(
        rendered.text.contains("reviewer_signers = [\"s\"]"),
        "{}",
        rendered.text
    );
}

#[test]
fn config_render_without_signers_anywhere_is_refused() {
    let error = render_config(None, &[], &["s".to_owned()], false).expect_err("no signers");
    assert_eq!(error.exit_code(), 1);
    let error = render_config(None, &["s".to_owned()], &[], false).expect_err("no sessions");
    assert_eq!(error.exit_code(), 1);
    let fresh = render_config(None, &["s".to_owned()], &["g".to_owned()], false).expect("fresh");
    assert!(fresh.text.contains("fleet_review_required = true"));
}

#[test]
fn a_config_edit_outside_the_named_keys_is_refused() {
    let before: toml::Table = toml::from_str("[auto_merge]\nqueue = true\n").expect("toml");
    let after: toml::Table =
        toml::from_str("[auto_merge]\nqueue = false\nfleet_review_required = true\n")
            .expect("toml");
    assert!(require_only_named_keys_changed(&before, &after).is_err());
    let named: toml::Table =
        toml::from_str("[auto_merge]\nqueue = true\nfleet_review_required = true\n").expect("toml");
    assert!(require_only_named_keys_changed(&before, &named).is_ok());
}

#[test]
fn the_sweep_skips_heads_that_already_carry_a_verdict() {
    let fake = healthy();
    let pulls = read_all_pages(&fake, &format!("repos/{REPO}/pulls?state=open")).expect("pulls");
    let rows = sweep_rows(&fake, REPO, &pulls).expect("sweep");
    let summary: Vec<(u64, &str)> = rows
        .iter()
        .map(|row| (row.number, row.trigger.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (1, "skip-existing"),
            (2, "skip-existing"),
            (3, "workflow_dispatch")
        ],
        "a draft is never swept"
    );
    assert_eq!(rows[0].status_state.as_deref(), Some("pending"));
    assert_eq!(rows[1].check_run_id, Some(77));
    assert_eq!(rows[1].conclusion.as_deref(), Some("failure"));
}

#[test]
fn a_check_run_from_another_app_does_not_count() {
    let mut fake = healthy();
    fake.json.insert(
        runs_path(HEAD_C),
        json!({"total_count": 1, "check_runs": [
            {"id": 9, "conclusion": "success", "app": {"id": 999}}
        ]}),
    );
    let pulls = read_all_pages(&fake, &format!("repos/{REPO}/pulls?state=open")).expect("pulls");
    let rows = sweep_rows(&fake, REPO, &pulls).expect("sweep");
    assert_eq!(rows[2].trigger, "workflow_dispatch");
    assert_eq!(rows[2].check_run_id, None);
}

#[test]
fn every_failed_read_stops_the_plan_with_exit_9() {
    let paths = [
        format!("repos/{REPO}"),
        format!("repos/{SHIPYARD_REPO}/commits/v{VERSION}"),
        config_path(),
        rulesets_path(),
        pulls_path(1),
        status_path(HEAD_C),
        runs_path(HEAD_C),
    ];
    for path in paths {
        let mut fake = healthy();
        fake.errors.insert(path.clone(), "HTTP 502".to_owned());
        let error = build_plan(&fake, &request()).expect_err(&path);
        assert_eq!(error.exit_code(), 9, "{path}: {error:?}");
    }
    for path in [
        rulesets_path(),
        pulls_path(1),
        status_path(HEAD_C),
        runs_path(HEAD_C),
    ] {
        let mut fake = healthy();
        fake.json.remove(&path);
        let error = build_plan(&fake, &request()).expect_err(&path);
        assert_eq!(error.exit_code(), 9, "404 {path}: {error:?}");
    }
}

#[test]
fn a_gate_script_on_the_base_keeps_pr_gates_unset_in_the_plan() {
    let mut fake = healthy();
    fake.raw.insert(
        format!("repos/{REPO}/contents/scripts/version_bump_check.py?ref=main"),
        "#!/usr/bin/env python3\n".to_owned(),
    );
    let plan = build_plan(&fake, &request()).expect("plan");
    let config = plan["rendered"][".shipyard/config.toml"]
        .as_str()
        .expect("config");
    assert!(!config.contains("pr_gates"), "{config}");
    let control = build_plan(&healthy(), &request()).expect("plan");
    assert!(
        control["rendered"][".shipyard/config.toml"]
            .as_str()
            .expect("config")
            .contains("pr_gates = false"),
        "control: no gate scripts adds the opt-out"
    );
    let mut broken = healthy();
    broken.errors.insert(
        format!("repos/{REPO}/contents/tools/scripts/skill_sync_check.py?ref=main"),
        "HTTP 502".to_owned(),
    );
    assert_eq!(
        build_plan(&broken, &request())
            .expect_err("unreadable")
            .exit_code(),
        9
    );
}

#[test]
fn a_status_list_longer_than_one_page_is_unreadable() {
    let mut fake = healthy();
    fake.json.insert(
        status_path(HEAD_C),
        json!({"total_count": 101, "statuses": [{"context": "ci", "state": "success"}]}),
    );
    assert_eq!(
        build_plan(&fake, &request())
            .expect_err("truncated")
            .exit_code(),
        9
    );
}

#[test]
fn open_pulls_are_read_past_the_first_page() {
    let mut fake = healthy();
    let page: Vec<Value> = (0..100)
        .map(|n| json!({"number": 100 + n, "draft": true, "head": {"sha": HEAD_C}}))
        .collect();
    fake.json.insert(pulls_path(1), Value::Array(page));
    fake.json.insert(
        pulls_path(2),
        json!([{"number": 500, "draft": false, "head": {"sha": HEAD_C}}]),
    );
    let plan = build_plan(&fake, &request()).expect("plan");
    let prs = plan["steps"][6]["prs"].as_array().expect("prs");
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0]["number"], 500);
}

#[test]
fn the_plan_is_read_only_and_marks_the_included_steps() {
    let fake = healthy();
    let mut request = request();
    request.through = Some("evaluated".to_owned());
    let plan = build_plan(&fake, &request).expect("plan");
    assert_eq!(plan["mode"], "plan-only");
    assert_eq!(plan["through"], "evaluated");
    assert_eq!(plan["shipyard_pin"]["commit"], PIN);
    let steps = plan["steps"].as_array().expect("steps");
    let names: Vec<&str> = steps.iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(names, STEPS);
    let included: Vec<bool> = steps
        .iter()
        .filter_map(|s| s["included"].as_bool())
        .collect();
    assert_eq!(
        included,
        [
            true, true, true, true, true, true, true, false, false, false, false
        ]
    );
    assert!(
        steps
            .iter()
            .all(|step| step.get("unproducible_on_main").is_none()),
        "no hardcoded bootstrap predicate"
    );
    for path in fake.calls.borrow().iter() {
        assert!(!path.contains("-X") && !path.contains("--method"), "{path}");
    }
}

#[test]
fn require_checks_reach_the_tightening_step_bound_to_actions() {
    let fake = healthy();
    let mut request = request();
    request.require_checks = vec![
        "macos".to_owned(),
        " lint ".to_owned(),
        "macos".to_owned(),
        "review-verdict".to_owned(),
    ];
    let plan = build_plan(&fake, &request).expect("plan");
    let expected = json!([
        {"context": "review-verdict", "integration_id": 15368},
        {"context": "macos", "integration_id": 15368},
        {"context": "lint", "integration_id": 15368},
    ]);
    let tightened = &plan["steps"][5];
    assert_eq!(tightened["name"], "tightened");
    assert_eq!(tightened["required_status_checks_to_add"], expected);
    assert_eq!(plan["required_status_checks_to_add"], expected);

    let control = build_plan(&fake, &self::request()).expect("plan");
    assert_eq!(
        control["steps"][5]["required_status_checks_to_add"],
        json!([{"context": "review-verdict", "integration_id": 15368}])
    );
}

#[test]
fn a_blank_require_check_is_refused_before_any_read() {
    let fake = healthy();
    let mut request = request();
    request.require_checks = vec!["  ".to_owned()];
    assert_eq!(
        build_plan(&fake, &request).expect_err("blank").exit_code(),
        1
    );
    assert!(fake.calls.borrow().is_empty());
}
