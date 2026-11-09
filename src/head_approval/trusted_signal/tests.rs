//! A trusted signal admits a head only from its own workflow's latest run on
//! that head, once the base branch has the workflow.

use std::cell::RefCell;

use serde_json::{Value, json};

use super::{TrustedSignal, apply, judge, required, run_id_from_url, signals_from_section};
use crate::head_approval::{ApprovalPolicy, HeadGate, evaluate_with_policy};

const REPO: &str = "Generous-Corp/vellum";
const HEAD: &str = "11120fb7d8b2feee6a49117b692400f314491280";
const OTHER: &str = "09b407e14b0708d94eaaafb8db3e921f2c417240";
const WORKFLOW: &str = ".github/workflows/framework-trusted-verify.yml";
const NAME: &str = "framework-trusted-verify";
const PR: u64 = 272;
const HEAD_REF: &str = "feature/trusted-verify";
/// `main` as it is now.
const MAIN_TIP: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// The pull request's `base.sha` snapshot, from before the workflow landed.
const OLD_BASE: &str = "0b5a7afaac13aa2c7e0c4b5bb5f0b3f26c35a8a1";
const GITHUB_TREE: &str = "1111111111111111111111111111111111111111";
const WORKFLOWS_TREE: &str = "2222222222222222222222222222222222222222";
const OLD_GITHUB_TREE: &str = "4444444444444444444444444444444444444444";
const OLD_WORKFLOWS_TREE: &str = "5555555555555555555555555555555555555555";
const BLOB: &str = "3333333333333333333333333333333333333333";

fn signal() -> TrustedSignal {
    TrustedSignal {
        name: NAME.to_owned(),
        workflow: WORKFLOW.to_owned(),
    }
}

fn run_url(id: u64) -> String {
    format!("https://github.com/{REPO}/actions/runs/{id}")
}

fn genuine_run(id: u64) -> Value {
    json!({"id": id, "path": WORKFLOW, "event": "pull_request_target", "head_sha": HEAD,
           "head_branch": HEAD_REF, "status": "completed", "conclusion": "success",
           "repository": REPO})
}

fn status(id: u64, state: &str, url: &str) -> Value {
    json!({"id": id, "state": state, "context": NAME, "target_url": url})
}

fn check(id: u64, app_id: u64) -> Value {
    json!({"id": id, "name": NAME, "app_id": app_id})
}

fn pull(number: u64, base: &str) -> Value {
    json!({"number": number, "base": base, "head_sha": HEAD})
}

fn tree(entries: &[(&str, &str, &str)]) -> String {
    let entries: Vec<Value> = entries
        .iter()
        .map(|(path, kind, sha)| json!({"path": path, "type": kind, "sha": sha}))
        .collect();
    json!({"truncated": false, "tree": entries}).to_string()
}

fn lines(rows: &[Value]) -> String {
    rows.iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A base read that fails.
#[derive(Clone, Copy, PartialEq)]
enum Unreadable {
    Pull,
    MainRef,
}

/// A stand-in GitHub: statuses, check runs, workflow runs, the pull request,
/// its base branch's trees, related pull requests, and every call it saw.
struct World {
    statuses: Vec<Value>,
    checks: Vec<Value>,
    runs: Vec<Value>,
    fail: bool,
    /// Whether `main`'s current tip has the trusted workflow file.
    workflow_on_main: bool,
    /// Which base read fails, if any.
    unreadable: Option<Unreadable>,
    /// Pull requests with this head branch, and with this head commit.
    by_branch: Vec<Value>,
    by_commit: Vec<Value>,
    calls: RefCell<Vec<String>>,
}

impl World {
    fn new(statuses: Vec<Value>, checks: Vec<Value>, runs: Vec<Value>) -> Self {
        Self {
            statuses,
            checks,
            runs,
            fail: false,
            workflow_on_main: true,
            unreadable: None,
            by_branch: vec![pull(PR, "main")],
            by_commit: vec![pull(PR, "main")],
            calls: RefCell::new(Vec::new()),
        }
    }

    fn genuine() -> Self {
        Self::new(
            vec![status(1, "success", &run_url(7))],
            vec![],
            vec![genuine_run(7)],
        )
    }

    fn gh(&self, argv: &[String]) -> Result<String, String> {
        let path = argv
            .iter()
            .find(|arg| arg.starts_with("repos/"))
            .cloned()
            .unwrap_or_default();
        let jq = argv
            .iter()
            .position(|arg| arg == "--jq")
            .and_then(|index| argv.get(index + 1))
            .cloned()
            .unwrap_or_default();
        self.calls.borrow_mut().push(path.clone());
        if path == format!("repos/{REPO}/pulls/{PR}") {
            if self.unreadable == Some(Unreadable::Pull) {
                return Err("HTTP 502".to_owned());
            }
            if jq == ".base.sha" {
                return Ok(OLD_BASE.to_owned());
            }
            return Ok(json!({"base_ref": "main", "head_ref": HEAD_REF,
                             "head_owner": "Generous-Corp"})
            .to_string());
        }
        if path == format!("repos/{REPO}/git/ref/heads/main") {
            return if self.unreadable == Some(Unreadable::MainRef) {
                Err("HTTP 502".to_owned())
            } else {
                Ok(MAIN_TIP.to_owned())
            };
        }
        if let Some(sha) = path.strip_prefix(&format!("repos/{REPO}/git/trees/")) {
            return match sha {
                MAIN_TIP => Ok(tree(&[(".github", "tree", GITHUB_TREE)])),
                GITHUB_TREE => Ok(tree(&[("workflows", "tree", WORKFLOWS_TREE)])),
                WORKFLOWS_TREE if self.workflow_on_main => Ok(tree(&[
                    ("framework-trusted-verify.yml", "blob", BLOB),
                    ("review-verdict.yml", "blob", BLOB),
                ])),
                OLD_BASE => Ok(tree(&[(".github", "tree", OLD_GITHUB_TREE)])),
                OLD_GITHUB_TREE => Ok(tree(&[("workflows", "tree", OLD_WORKFLOWS_TREE)])),
                WORKFLOWS_TREE | OLD_WORKFLOWS_TREE => {
                    Ok(tree(&[("review-verdict.yml", "blob", BLOB)]))
                }
                _ => Err("HTTP 404".to_owned()),
            };
        }
        if self.fail {
            return Err("HTTP 502".to_owned());
        }
        if path.starts_with(&format!("repos/{REPO}/pulls?")) {
            return Ok(lines(&self.by_branch));
        }
        if path.starts_with(&format!("repos/{REPO}/commits/{HEAD}/pulls")) {
            return Ok(lines(&self.by_commit));
        }
        if path.contains("/statuses") {
            return Ok(lines(&self.statuses));
        }
        if path.contains("/check-runs") {
            return Ok(lines(&self.checks));
        }
        if path.starts_with(&format!(
            "repos/{REPO}/actions/workflows/framework-trusted-verify.yml/runs?"
        )) {
            // GitHub filters by workflow, head sha and event.
            let listed: Vec<Value> = self
                .runs
                .iter()
                .filter(|run| {
                    run["path"] == WORKFLOW
                        && run["head_sha"] == HEAD
                        && run["event"] == "pull_request_target"
                })
                .cloned()
                .collect();
            return Ok(lines(&listed));
        }
        if let Some(id) = path.strip_prefix(&format!("repos/{REPO}/actions/runs/")) {
            let id: u64 = id.parse().map_err(|_| "bad id".to_owned())?;
            return self
                .runs
                .iter()
                .find(|run| run["id"].as_u64() == Some(id))
                .map(Value::to_string)
                .ok_or_else(|| "HTTP 404".to_owned());
        }
        Err(format!("unexpected call {path}"))
    }

    fn judge(&self) -> Result<Option<String>, String> {
        judge(&|argv| self.gh(argv), REPO, HEAD, HEAD_REF, &signal())
    }

    fn apply(&self, repo: &str, gate: HeadGate) -> Result<HeadGate, String> {
        apply(&|argv| self.gh(argv), repo, PR, HEAD, &[], gate)
    }

    fn refused_by_apply(&self, needle: &str) {
        match self.apply(REPO, HeadGate::NotRequired).expect("readable") {
            HeadGate::UntrustedSignal { detail, .. } => {
                assert!(detail.contains(needle), "{detail:?} lacks {needle:?}");
            }
            gate => panic!("admitted: {gate:?}"),
        }
    }
}

fn refused(world: &World, needle: &str) {
    let detail = world.judge().expect("readable").expect("refused");
    assert!(detail.contains(needle), "{detail:?} lacks {needle:?}");
}

#[test]
fn a_genuine_status_admits() {
    let world = World::genuine();
    assert_eq!(world.judge().unwrap(), None);
    assert_eq!(
        world.apply(REPO, HeadGate::NotRequired).unwrap(),
        HeadGate::NotRequired
    );
}

#[test]
fn a_merge_group_run_does_not_admit_a_pull_request_head() {
    let mut run = genuine_run(7);
    run["event"] = json!("merge_group");
    let world = World::new(vec![status(1, "success", &run_url(7))], vec![], vec![run]);
    refused(&world, "ran on `merge_group`");
}

#[test]
fn a_same_named_check_run_is_a_spoof_even_beside_a_genuine_status() {
    for app_id in [15368, 42] {
        let mut world = World::genuine();
        world.checks = vec![check(5, app_id)];
        refused(&world, "a same-named check run is a spoof");
    }
}

#[test]
fn a_pending_newest_status_is_refused() {
    let mut world = World::genuine();
    world.statuses.push(status(2, "pending", &run_url(7)));
    refused(
        &world,
        "the newest `framework-trusted-verify` status is `pending`",
    );
}

#[test]
fn a_failing_newest_status_is_refused() {
    let mut world = World::genuine();
    world.statuses.push(status(2, "failure", &run_url(7)));
    refused(
        &world,
        "the newest `framework-trusted-verify` status is `failure`",
    );
}

#[test]
fn a_spoofed_pull_request_run_is_refused() {
    let mut run = genuine_run(7);
    run["event"] = json!("pull_request");
    let world = World::new(vec![status(1, "success", &run_url(7))], vec![], vec![run]);
    refused(&world, "ran on `pull_request`");
}

#[test]
fn a_run_of_another_workflow_is_refused() {
    let mut run = genuine_run(7);
    run["path"] = json!(".github/workflows/spoof.yml");
    let world = World::new(vec![status(1, "success", &run_url(7))], vec![], vec![run]);
    refused(&world, "is `.github/workflows/spoof.yml`");
}

#[test]
fn a_run_for_another_head_is_refused() {
    let mut run = genuine_run(7);
    run["head_sha"] = json!(OTHER);
    let world = World::new(vec![status(1, "success", &run_url(7))], vec![], vec![run]);
    refused(&world, "ran for 09b407e14b07");
}

#[test]
fn a_run_on_another_branch_is_refused() {
    let mut run = genuine_run(7);
    run["head_branch"] = json!("weak");
    let world = World::new(vec![status(1, "success", &run_url(7))], vec![], vec![run]);
    refused(&world, "ran on branch `weak`");
}

#[test]
fn a_failed_run_is_refused_even_under_a_success_status() {
    let mut run = genuine_run(7);
    run["conclusion"] = json!("failure");
    let world = World::new(vec![status(1, "success", &run_url(7))], vec![], vec![run]);
    refused(&world, "is completed/failure");
}

#[test]
fn a_replayed_older_success_over_a_newer_failure_is_refused() {
    let mut newer = genuine_run(8);
    newer["conclusion"] = json!("failure");
    let mut world = World::genuine();
    world.runs.push(newer);
    refused(&world, "the newest run 8 of");
}

#[test]
fn a_replayed_older_success_over_a_newer_running_run_is_refused() {
    let mut newer = genuine_run(8);
    newer["status"] = json!("in_progress");
    newer["conclusion"] = Value::Null;
    let mut world = World::genuine();
    world.runs.push(newer);
    refused(&world, "is in_progress/none");
}

#[test]
fn the_newest_status_must_name_the_newest_run() {
    // Both runs succeeded, but the newest status still points at the older one.
    let mut world = World::genuine();
    world.runs.push(genuine_run(8));
    refused(&world, "names run 7, not the newest run 8");
}

#[test]
fn an_unresolvable_or_foreign_url_is_refused() {
    for (url, needle) in [
        (
            "https://evil.example/Generous-Corp/vellum/actions/runs/7",
            "not a github.com",
        ),
        (
            "https://github.com/someone/vellum/actions/runs/7",
            "names `someone/vellum`",
        ),
        (
            "https://github.com/Generous-Corp/vellum/pull/272",
            "not an Actions run URL",
        ),
        (
            "https://github.com/Generous-Corp/vellum/actions/runs/7?x=1",
            "not an exact",
        ),
        (
            "https://github.com/Generous-Corp/vellum/actions/runs/7/logs",
            "not an exact",
        ),
        (
            "https://github.com/Generous-Corp/vellum/actions/runs/7/job/9",
            "not an exact",
        ),
        ("", "not a github.com"),
    ] {
        let world = World::new(
            vec![status(1, "success", url)],
            vec![],
            vec![genuine_run(7)],
        );
        refused(&world, needle);
    }
}

#[test]
fn a_run_in_another_repository_is_refused() {
    let mut run = genuine_run(7);
    run["repository"] = json!("someone/vellum");
    let world = World::new(vec![status(1, "success", &run_url(7))], vec![], vec![run]);
    refused(&world, "belongs to `someone/vellum`");
}

#[test]
fn a_missing_signal_is_refused() {
    let other =
        json!({"id": 1, "state": "success", "context": "something-else", "target_url": run_url(7)});
    let world = World::new(vec![other], vec![], vec![genuine_run(7)]);
    refused(&world, "no `framework-trusted-verify` status on head");
}

#[test]
fn a_spoof_beside_a_genuine_signal_is_refused() {
    let mut spoof = genuine_run(6);
    spoof["event"] = json!("pull_request");
    // The spoof is the older status, so only the every-status rule sees it;
    // the newest status is the genuine one.
    let mut world = World::genuine();
    world.statuses = vec![
        status(1, "success", &run_url(6)),
        status(2, "success", &run_url(7)),
    ];
    world.runs.push(spoof);
    refused(&world, "names run 6");
}

#[test]
fn another_pull_request_with_this_head_branch_on_another_base_refuses() {
    let mut world = World::genuine();
    world.by_branch.push(pull(300, "weak"));
    world.refused_by_apply("#300 with this head targets `weak`");
}

#[test]
fn an_older_pull_request_on_this_branch_with_another_base_admits() {
    // Once opened from this branch against another base, then closed: a
    // different head, so it says nothing about this head's runs.
    let mut world = World::genuine();
    let mut older = pull(299, "develop");
    older["head_sha"] = json!(OTHER);
    world.by_branch.push(older);
    assert_eq!(
        world.apply(REPO, HeadGate::NotRequired).unwrap(),
        HeadGate::NotRequired
    );
}

#[test]
fn another_pull_request_with_this_head_commit_on_another_base_refuses() {
    let mut world = World::genuine();
    world.by_commit.push(pull(301, "weak"));
    world.refused_by_apply("#301 with this head targets `weak`");
    // A pull request merely containing the commit, not headed by it, is fine.
    let mut world = World::genuine();
    let mut containing = pull(302, "weak");
    containing["head_sha"] = json!(OTHER);
    world.by_commit.push(containing);
    assert_eq!(
        world.apply(REPO, HeadGate::NotRequired).unwrap(),
        HeadGate::NotRequired
    );
}

#[test]
fn it_is_enforced_when_main_has_the_workflow_though_base_sha_predates_it() {
    // The pull request's base.sha snapshot lacks the workflow; main has it now.
    let world = World::new(vec![], vec![], vec![]);
    world.refused_by_apply("no `framework-trusted-verify` status");
}

#[test]
fn it_is_skipped_while_main_lacks_the_workflow() {
    let mut world = World::new(vec![], vec![], vec![]);
    world.workflow_on_main = false;
    assert_eq!(
        world.apply(REPO, HeadGate::NotRequired).unwrap(),
        HeadGate::NotRequired
    );
    let calls = world.calls.borrow();
    assert!(
        calls.iter().all(|call| call.contains("/pulls/272")
            || call.contains("/git/ref/heads/main")
            || call.contains("/git/trees/")),
        "a skipped signal reads only the pull request and main's tree: {calls:?}"
    );
}

#[test]
fn a_pull_request_deleting_the_workflow_is_still_enforced() {
    // Main still has the file, so main decides; the head's tree is never read.
    let world = World::new(vec![], vec![], vec![]);
    world.refused_by_apply("no `framework-trusted-verify` status");
    assert!(
        world
            .calls
            .borrow()
            .iter()
            .all(|call| !call.contains(&format!("git/trees/{HEAD}"))),
        "only main's tree decides enforcement"
    );
}

#[test]
fn an_unreadable_base_is_an_error_not_an_admission() {
    let mut world = World::genuine();
    world.unreadable = Some(Unreadable::Pull);
    assert!(world.apply(REPO, HeadGate::NotRequired).is_err());
    let mut world = World::genuine();
    world.unreadable = Some(Unreadable::MainRef);
    assert!(world.apply(REPO, HeadGate::NotRequired).is_err());
}

#[test]
fn an_unreadable_signal_is_an_error_not_an_admission() {
    let mut world = World::genuine();
    world.fail = true;
    assert!(world.judge().is_err());
    assert!(world.apply(REPO, HeadGate::NotRequired).is_err());
    let world = World::new(
        vec![status(1, "success", &run_url(9))],
        vec![],
        vec![genuine_run(7)],
    );
    assert!(
        world.judge().is_err(),
        "a run that cannot be fetched is unreadable"
    );
}

#[test]
fn urls_parse_strictly() {
    assert_eq!(run_id_from_url(&run_url(42), REPO), Ok(42));
    assert_eq!(
        run_id_from_url(
            "https://github.com/generous-corp/VELLUM/actions/runs/42",
            REPO
        ),
        Ok(42)
    );
    for url in [
        "https://github.com/Generous-Corp/vellum/actions/runs/0",
        "https://github.com/Generous-Corp/vellum/actions/runs/04",
        "https://github.com/Generous-Corp/vellum/actions/runs/x",
        "https://github.com/Generous-Corp/vellum/actions/runs/4/job/",
        "https://github.com/Generous-Corp/vellum/actions/runs/4/attempts/2",
        "https://github.com/Generous-Corp/vellum/actions/runs/4/job/9",
        "https://github.com/Generous-Corp/vellum//actions/runs/4",
        "http://github.com/Generous-Corp/vellum/actions/runs/4",
        "https://github.com.evil/Generous-Corp/vellum/actions/runs/4",
    ] {
        assert!(run_id_from_url(url, REPO).is_err(), "{url}");
    }
}

#[test]
fn vellum_requires_it_and_other_repositories_do_not() {
    assert_eq!(required(REPO, &[]), vec![signal()]);
    assert_eq!(required("generous-corp/VELLUM", &[]), vec![signal()]);
    for repo in [
        "Generous-Corp/pulp",
        "danielraffel/Shipyard",
        "danielraffel/tartci",
    ] {
        assert!(required(repo, &[]).is_empty(), "{repo}");
        let world = World::new(vec![], vec![], vec![]);
        let gate = world.apply(repo, HeadGate::NotRequired).unwrap();
        assert_eq!(gate, HeadGate::NotRequired);
        assert!(world.calls.borrow().is_empty(), "{repo} must cost no reads");
    }
}

#[test]
fn a_config_adds_signals_and_a_bad_entry_refuses_everything() {
    let table: toml::Table = toml::from_str(
        "[auto_merge]\ntrusted_signals = [{ name = \"x\", workflow = \".github/workflows/x.yml\" }]\n",
    )
    .unwrap();
    let signals = signals_from_section(table.get("auto_merge").and_then(toml::Value::as_table));
    assert_eq!(
        signals,
        vec![TrustedSignal {
            name: "x".into(),
            workflow: ".github/workflows/x.yml".into()
        }]
    );
    assert_eq!(required("danielraffel/tartci", &signals), signals);
    assert_eq!(required(REPO, &signals).len(), 2);
    let policy = ApprovalPolicy::from_table(&table);
    assert_eq!(policy.trusted_signals, signals);

    let world = World::new(vec![], vec![], vec![]);
    for bad in [
        "[auto_merge]\ntrusted_signals = [{ name = \"x\" }]\n",
        "[auto_merge]\ntrusted_signals = [{ name = \"x\", workflow = \"ci/x.yml\" }]\n",
    ] {
        let bad: toml::Table = toml::from_str(bad).unwrap();
        let signals = signals_from_section(bad.get("auto_merge").and_then(toml::Value::as_table));
        assert_eq!(signals.len(), 1);
        let detail = judge(
            &|argv| world.gh(argv),
            "danielraffel/tartci",
            HEAD,
            HEAD_REF,
            &signals[0],
        )
        .unwrap();
        assert!(detail.is_some_and(|detail| detail.contains("cannot be verified")));
    }
}

#[test]
fn a_refused_head_stays_refused_and_an_admitted_one_is_judged() {
    let world = World::new(vec![], vec![], vec![]);
    let held = HeadGate::NotApproved {
        head: HEAD.to_owned(),
    };
    assert_eq!(world.apply(REPO, held.clone()).unwrap(), held);
    assert!(world.calls.borrow().is_empty());
    let gate = world.apply(REPO, HeadGate::NotRequired).unwrap();
    assert!(matches!(gate, HeadGate::UntrustedSignal { .. }), "{gate:?}");
    assert!(!gate.allows());
}

#[test]
fn the_steward_gate_requires_it_for_vellum() {
    // The steward's evaluator: a policy that requires no approval still
    // admits a Vellum head only with the genuine signal.
    let policy = ApprovalPolicy::default();
    let missing = World::new(vec![], vec![], vec![]);
    let gate = evaluate_with_policy(
        &|argv| missing.gh(argv),
        REPO,
        PR,
        HEAD,
        &policy,
        &Value::Null,
    )
    .unwrap();
    assert!(matches!(gate, HeadGate::UntrustedSignal { .. }), "{gate:?}");
    let genuine = World::genuine();
    let gate = evaluate_with_policy(
        &|argv| genuine.gh(argv),
        REPO,
        PR,
        HEAD,
        &policy,
        &Value::Null,
    )
    .unwrap();
    assert_eq!(gate, HeadGate::NotRequired);
    let pulp = World::new(vec![], vec![], vec![]);
    let gate = evaluate_with_policy(
        &|argv| pulp.gh(argv),
        "Generous-Corp/pulp",
        1,
        HEAD,
        &policy,
        &Value::Null,
    )
    .unwrap();
    assert_eq!(gate, HeadGate::NotRequired);
}
