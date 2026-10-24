//! Adapter tests against a fake `gh` that serves recorded GitHub API shapes.

use super::*;
use serde_json::json;
use std::fmt::Write as _;

const REPO: &str = "owner/repo";
const HEAD_OLD: &str = "1111111111111111111111111111111111111111";
const HEAD_ONE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD_TWO: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HEAD_THREE: &str = "cccccccccccccccccccccccccccccccccccccccc";
const HEAD_FIX: &str = "ffffffffffffffffffffffffffffffffffffffff";
const HEAD_BASE: &str = "9999999999999999999999999999999999999999";
const MAC: [&str; 6] = [
    "self-hosted",
    "macOS",
    "ARM64",
    "pulp-build",
    "pulp-build-vm",
    "pulp-build-pr-head",
];

struct Fixture {
    temp: tempfile::TempDir,
    routes: Vec<(String, Value)>,
}

impl Fixture {
    fn new() -> Self {
        let mut fixture = Self {
            temp: tempfile::tempdir().expect("temp"),
            routes: Vec::new(),
        };
        fixture.route("api repos/owner/repo", json!({"full_name": REPO}));
        fixture.route(
            "api repos/owner/repo/rules/branches/main --paginate --slurp",
            json!([[]]),
        );
        fixture.route(
            "api repos/owner/repo/branches/main/protection/required_status_checks",
            json!({"contexts": ["macos", "drift-fast"]}),
        );
        fixture
    }

    fn route(&mut self, args: &str, body: Value) {
        self.routes.push((args.to_owned(), body));
    }

    fn runs(&mut self, status: &str, runs: &[Value]) {
        self.route(
            &format!("api repos/owner/repo/actions/runs?status={status}&per_page=100&page=1"),
            json!({"workflow_runs": runs}),
        );
    }

    fn jobs(&mut self, run_id: u64, jobs: &[Value]) {
        self.route(
            &format!(
                "api repos/owner/repo/actions/runs/{run_id}/jobs?filter=latest&per_page=100&page=1"
            ),
            json!({"jobs": jobs}),
        );
    }

    fn head_checks(&mut self, sha: &str, checks: &[Value]) {
        self.route(
            &format!("api repos/owner/repo/commits/{sha}/check-runs?per_page=100&page=1"),
            json!({"check_runs": checks}),
        );
        self.route(
            &format!("api repos/owner/repo/commits/{sha}/statuses?per_page=100&page=1"),
            json!([]),
        );
    }

    /// Write the fake `gh`, the mutation-authority config, and return the
    /// runtime pieces. Every invocation is appended to `calls.log`.
    fn build(&self) -> (GitHubActions, RuntimePaths, PathBuf) {
        let dir = self.temp.path();
        let fixtures = dir.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures");
        let mut arms = String::new();
        for (index, (args, body)) in self.routes.iter().enumerate() {
            let file = fixtures.join(format!("{index}.json"));
            fs::write(&file, body.to_string()).expect("fixture");
            let _ = writeln!(arms, "  '{args}') cat '{}' ;;", file.display());
        }
        if !fixtures.join("mq.json").exists() {
            self.merge_queue(&[]);
        }
        let log = dir.join("calls.log");
        let script = format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"$*\" in\n\
             *' -X POST '*|'api -X POST '*) printf '{{}}' ;;\n\
             *'pr list --repo owner/repo'*) cat '{prs}' ;;\n\
             'api graphql '*) cat '{mq}' ;;\n{arms}\
             *) echo \"unexpected gh call: $*\" >&2; exit 1 ;;\nesac\n",
            log = log.display(),
            prs = fixtures.join("prs.json").display(),
            mq = fixtures.join("mq.json").display(),
        );
        let gh = dir.join("gh");
        crate::test_support::write_executable_script(&gh, &script);
        let global_dir = dir.join("global");
        let state_dir = dir.join("state");
        fs::create_dir_all(&global_dir).expect("global");
        fs::create_dir_all(&state_dir).expect("state");
        fs::write(
            global_dir.join("config.toml"),
            "[merge_queue]\nmutation_machine = \"studio\"\n",
        )
        .expect("authority");
        fs::write(state_dir.join("machine-tag"), "studio\n").expect("tag");
        let config = crate::config::LoadedConfig {
            data: toml::Table::new(),
            global_dir: global_dir.clone(),
            project_dir: None,
            local_dir: None,
            local_overlay_source: crate::config::LocalOverlaySource::None,
        };
        let actions = GitHubActions::from_loaded_config(dir, &config).with_gh_binary_for_tests(gh);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(global_dir),
            Some(state_dir),
        );
        (actions, paths, log)
    }

    /// Serve a merge queue holding `entries` as `(pr, head)` in order.
    fn merge_queue(&self, entries: &[(u64, &str)]) {
        let fixtures = self.temp.path().join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures");
        fs::write(
            fixtures.join("mq.json"),
            merge_queue_body(entries).to_string(),
        )
        .expect("mq");
    }

    fn prs(&self, prs: &[Value]) {
        let fixtures = self.temp.path().join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures");
        fs::write(
            fixtures.join("prs.json"),
            Value::from(prs.to_vec()).to_string(),
        )
        .expect("prs");
    }
}

fn merge_queue_body(entries: &[(u64, &str)]) -> Value {
    let nodes = entries
        .iter()
        .enumerate()
        .map(|(index, (number, head))| {
            json!({
                "position": index + 1,
                "enqueuedAt": "2026-10-24T08:00:00Z",
                "headCommit": {"oid": head},
                "pullRequest": {"number": number},
            })
        })
        .collect::<Vec<_>>();
    json!({"data": {"repository": {"mergeQueue": {"entries": {
        "nodes": nodes,
        "pageInfo": {"hasNextPage": false, "endCursor": null},
    }}}}})
}

/// Prepend a stateful arm: the first matching call gets `first`, later ones
/// get `later`.
fn patch_first_then(fixture: &Fixture, pattern: &str, first: &Value, later: &Value) {
    let dir = fixture.temp.path();
    let marker = dir.join(format!("seen-{}", pattern.len()));
    let first_file = dir.join(format!("first-{}.json", pattern.len()));
    let later_file = dir.join(format!("later-{}.json", pattern.len()));
    fs::write(&first_file, first.to_string()).expect("first");
    fs::write(&later_file, later.to_string()).expect("later");
    let script_path = dir.join("gh");
    let script = fs::read_to_string(&script_path).expect("script");
    let arm = format!(
        "case \"$*\" in\n  {pattern}) if [ -e '{m}' ]; then cat '{l}'; else : > '{m}'; cat '{f}'; fi; exit 0 ;;\nesac\n",
        m = marker.display(),
        l = later_file.display(),
        f = first_file.display(),
    );
    let patched = script.replacen("case \"$*\" in\n", &format!("{arm}case \"$*\" in\n"), 1);
    fs::remove_file(&script_path).expect("remove");
    crate::test_support::write_executable_script(&script_path, &patched);
}

fn pr_row(number: u64, head: &str, merge_state: &str, title: &str) -> Value {
    json!({
        "id": format!("PR_{number}"),
        "number": number,
        "isDraft": false,
        "baseRefName": "main",
        "headRefOid": head,
        "headRefName": format!("branch-{number}"),
        "mergeStateStatus": merge_state,
        "autoMergeRequest": null,
        "labels": [],
        "author": {"login": "bot"},
        "title": title,
    })
}

fn run_row(id: u64, pr: u64, head: &str, status: &str, event: &str, created: &str) -> Value {
    json!({
        "id": id,
        "workflow_id": 7,
        "run_attempt": 1,
        "name": "Build and Test",
        "head_sha": head,
        "head_branch": format!("branch-{pr}"),
        "status": status,
        "event": event,
        "pull_requests": if event == "pull_request" { json!([{"number": pr}]) } else { json!([]) },
        "created_at": format!("2026-10-24T{created}:00Z"),
    })
}

fn job_row(id: u64, status: &str, created: &str, labels: &[&str]) -> Value {
    json!({
        "id": id,
        "name": format!("job-{id}"),
        "status": status,
        "conclusion": null,
        "labels": labels,
        "created_at": format!("2026-10-24T{created}:00Z"),
    })
}

fn check_row(name: &str, conclusion: &str, run_id: u64) -> Value {
    json!({
        "name": name,
        "status": "completed",
        "conclusion": conclusion,
        "details_url": format!("https://github.com/owner/repo/actions/runs/{run_id}/job/1"),
        "completed_at": "2026-10-24T08:00:00Z",
    })
}

fn cancelled_run(mut run: Value) -> Value {
    run["status"] = json!("completed");
    run["conclusion"] = json!("cancelled");
    run
}

fn ledger_entry(repo: &str, run_id: u64, pr_number: u64, head: &str) -> RestoreEntry {
    RestoreEntry {
        repo: repo.to_owned(),
        base: "main".to_owned(),
        run_id,
        workflow: "Build and Test".to_owned(),
        pr_number,
        head_sha: head.to_owned(),
        status_at_cancel: "queued".to_owned(),
        cause: CancelCause::YieldedTo { pr_number: 10931 },
        recorded_at: "2026-10-24T10:00:00Z".to_owned(),
    }
}

fn runtime<'a>(
    temp: &'a tempfile::TempDir,
    paths: &'a RuntimePaths,
    actions: &'a GitHubActions,
) -> QueueDrainRuntime<'a> {
    QueueDrainRuntime {
        cwd: temp.path(),
        mode: RuntimeMode::Isolated,
        runtime_paths: paths,
        actions,
    }
}

fn args(apply: bool) -> QueueDrainArgs {
    QueueDrainArgs {
        repo: Some(REPO.to_owned()),
        base: "main".to_owned(),
        apply,
        ledger: None,
    }
}

fn mutations(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("-X POST"))
        .map(str::to_owned)
        .collect()
}

fn output_json(buffer: &[u8]) -> Value {
    serde_json::from_slice(buffer).expect("json output")
}

/// The motivating backlog: one fix PR behind macOS jobs from other PRs.
fn prioritize_fixture() -> Fixture {
    let mut fixture = Fixture::new();
    fixture.prs(&[
        pr_row(1, HEAD_ONE, "BLOCKED", "Feature one"),
        pr_row(2, HEAD_TWO, "BLOCKED", "Feature two"),
        pr_row(3, HEAD_THREE, "BLOCKED", "Feature three"),
        pr_row(10931, HEAD_FIX, "BLOCKED", "Fix macos gate on main"),
    ]);
    for status in ["queued", "waiting", "pending", "requested"] {
        fixture.runs(status, &[]);
    }
    fixture.runs(
        "in_progress",
        &[
            run_row(100, 1, HEAD_ONE, "in_progress", "pull_request", "09:00"),
            run_row(200, 2, HEAD_TWO, "in_progress", "pull_request", "09:01"),
            run_row(300, 3, HEAD_THREE, "in_progress", "pull_request", "11:00"),
            run_row(400, 3, HEAD_THREE, "in_progress", "merge_group", "09:02"),
            run_row(900, 10931, HEAD_FIX, "in_progress", "pull_request", "09:30"),
        ],
    );
    fixture.jobs(
        100,
        &[
            job_row(1001, "completed", "09:01", &["ubuntu-latest"]),
            job_row(1002, "queued", "10:05", &MAC),
        ],
    );
    fixture.jobs(
        200,
        &[
            job_row(2001, "in_progress", "10:00", &MAC),
            job_row(2002, "queued", "10:06", &MAC),
        ],
    );
    fixture.jobs(900, &[job_row(9002, "queued", "10:10", &MAC)]);
    fixture.head_checks(HEAD_FIX, &[]);
    fixture
}

#[test]
fn prioritize_cancels_only_queued_runs_ahead_and_records_them() {
    let fixture = prioritize_fixture();
    let (actions, paths, log) = fixture.build();
    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    let code = queue_prioritize_command(&args(true), 10931, &rt, true, &mut out).expect("run");
    assert_eq!(code, ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&out));
    let report = output_json(&out);
    assert_eq!(report["plan"]["yields"][0]["run_id"], 100);
    assert_eq!(report["plan"]["yields"].as_array().map(Vec::len), Some(1));
    assert_eq!(report["plan"]["kept"][0]["run_id"], 200);
    assert_eq!(
        mutations(&log),
        vec!["api -X POST repos/owner/repo/actions/runs/100/cancel".to_owned()]
    );
    // Run 300 was created after the fix's waiting job and 400 is a merge
    // group; neither job list is even read.
    let calls = fs::read_to_string(&log).expect("log");
    assert!(!calls.contains("runs/300/jobs"));
    assert!(!calls.contains("runs/400/jobs"));

    let ledger = load_ledger(&paths.state_dir.join(LEDGER_FILE)).expect("ledger");
    let entries = ledger.for_repo(REPO);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].run_id, 100);
    assert_eq!(entries[0].pr_number, 1);
    assert_eq!(
        entries[0].cause,
        CancelCause::YieldedTo { pr_number: 10931 }
    );
}

#[test]
fn prioritize_dry_run_mutates_nothing() {
    let fixture = prioritize_fixture();
    let (actions, paths, log) = fixture.build();
    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    queue_prioritize_command(&args(false), 10931, &rt, true, &mut out).expect("run");
    assert!(mutations(&log).is_empty());
    assert!(!paths.state_dir.join(LEDGER_FILE).exists());
}

#[test]
fn prioritize_skips_a_run_whose_job_started_before_the_cancel() {
    let mut fixture = prioritize_fixture();
    // The live re-read right before the cancel sees run 100's macOS job
    // started.
    fixture
        .routes
        .retain(|(route, _)| !route.contains("runs/100/jobs"));
    let (actions, paths, log) = fixture.build();
    // Serve a queued view on the first read and a started view afterwards.
    let state = fixture.temp.path().join("job-reads");
    let script_path = fixture.temp.path().join("gh");
    let script = fs::read_to_string(&script_path).expect("script");
    let queued = json!({"jobs": [job_row(1002, "queued", "10:05", &MAC)]}).to_string();
    let started = json!({"jobs": [job_row(1002, "in_progress", "10:05", &MAC)]}).to_string();
    let arm = format!(
        "case \"$*\" in\n  *'runs/100/jobs'*) if [ -e '{state}' ]; then printf '%s' '{started}'; else : > '{state}'; printf '%s' '{queued}'; fi; exit 0 ;;\nesac\n",
        state = state.display()
    );
    let patched = script.replacen("case \"$*\" in\n", &format!("{arm}case \"$*\" in\n"), 1);
    fs::remove_file(&script_path).expect("remove");
    crate::test_support::write_executable_script(&script_path, &patched);

    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    queue_prioritize_command(&args(true), 10931, &rt, true, &mut out).expect("run");
    let report = output_json(&out);
    assert_eq!(report["cancellations"][0]["outcome"], "skipped_job_started");
    assert!(mutations(&log).is_empty());
    let ledger = load_ledger(&paths.state_dir.join(LEDGER_FILE)).expect("ledger");
    assert!(ledger.entries.is_empty());
}

#[test]
fn prioritize_without_mutation_authority_cancels_nothing() {
    let fixture = prioritize_fixture();
    let (actions, paths, log) = fixture.build();
    fs::write(paths.state_dir.join("machine-tag"), "laptop\n").expect("tag");
    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    let code = queue_prioritize_command(&args(true), 10931, &rt, true, &mut out).expect("run");
    assert_eq!(code, ExitCode::from(1));
    assert_eq!(
        output_json(&out)["cancellations"][0]["outcome"],
        "refused_by_mutation_guard"
    );
    assert!(mutations(&log).is_empty());
}

#[test]
fn triage_cancels_doomed_runs_and_reports_the_unblocking_fix() {
    let mut fixture = Fixture::new();
    fixture.prs(&[
        pr_row(1, HEAD_ONE, "BLOCKED", "Feature one"),
        pr_row(2, HEAD_TWO, "DIRTY", "Feature two"),
        pr_row(3, HEAD_THREE, "BLOCKED", "Feature three"),
        pr_row(10931, HEAD_FIX, "BLOCKED", "Fix macos gate on main"),
    ]);
    fixture.runs(
        "queued",
        &[
            run_row(50, 1, HEAD_OLD, "queued", "pull_request", "08:00"),
            run_row(51, 1, HEAD_ONE, "queued", "pull_request", "08:10"),
            run_row(70, 3, HEAD_THREE, "queued", "pull_request", "08:20"),
            run_row(80, 3, HEAD_THREE, "queued", "merge_group", "08:30"),
        ],
    );
    for status in ["waiting", "pending", "requested"] {
        fixture.runs(status, &[]);
    }
    fixture.runs(
        "in_progress",
        &[run_row(
            60,
            2,
            HEAD_TWO,
            "in_progress",
            "pull_request",
            "08:05",
        )],
    );
    fixture.head_checks(HEAD_ONE, &[]);
    fixture.head_checks(HEAD_TWO, &[]);
    fixture.head_checks(HEAD_THREE, &[check_row("drift-fast", "failure", 69)]);
    fixture.head_checks(HEAD_FIX, &[]);
    fixture.route(
        "api repos/owner/repo/commits/main",
        json!({"sha": HEAD_BASE}),
    );
    fixture.head_checks(HEAD_BASE, &[check_row("macos", "failure", 5)]);
    fixture.route(
        "api repos/owner/repo/pulls?state=open&base=main&per_page=100&page=1",
        json!([{"number": 10931, "body": "Repairs the red gate."}]),
    );
    for run_id in [50, 60, 70] {
        fixture.route(
            &format!("api repos/owner/repo/actions/runs/{run_id}"),
            json!({"status": "queued", "conclusion": null}),
        );
    }
    let (actions, paths, log) = fixture.build();
    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    let code = queue_triage_command(&args(true), false, &rt, true, &mut out).expect("triage");
    assert_eq!(code, ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&out));
    let report = output_json(&out);
    assert_eq!(report["doomed"], 3);
    assert_eq!(report["red_on_base"], json!(["macos"]));
    assert_eq!(report["unblocking_candidates"][0]["pr_number"], 10931);
    assert!(report["prioritize"].is_null());
    assert_eq!(
        mutations(&log),
        vec![
            "api -X POST repos/owner/repo/actions/runs/50/cancel".to_owned(),
            "api -X POST repos/owner/repo/actions/runs/60/cancel".to_owned(),
            "api -X POST repos/owner/repo/actions/runs/70/cancel".to_owned(),
        ]
    );
    let ledger = load_ledger(&paths.state_dir.join(LEDGER_FILE)).expect("ledger");
    assert_eq!(
        ledger
            .for_repo(REPO)
            .iter()
            .map(|entry| entry.run_id)
            .collect::<Vec<_>>(),
        vec![50, 60, 70]
    );
}

#[test]
fn resume_reruns_restorable_entries_and_clears_the_ledger() {
    let mut fixture = Fixture::new();
    fixture.prs(&[
        pr_row(1, HEAD_ONE, "BLOCKED", "Feature one"),
        pr_row(2, HEAD_TWO, "DIRTY", "Feature two"),
        pr_row(3, HEAD_THREE, "BLOCKED", "Feature three"),
    ]);
    for status in ["queued", "waiting", "pending", "requested", "in_progress"] {
        fixture.runs(status, &[]);
    }
    for head in [HEAD_ONE, HEAD_TWO, HEAD_THREE] {
        fixture.head_checks(head, &[]);
    }
    fixture.route(
        "api repos/owner/repo/actions/runs/100",
        cancelled_run(run_row(
            100,
            1,
            HEAD_ONE,
            "completed",
            "pull_request",
            "09:00",
        )),
    );
    let (actions, paths, log) = fixture.build();
    let ledger_file = paths.state_dir.join(LEDGER_FILE);
    let mut ledger = RestoreLedger::default();
    let entry = |run_id: u64, pr_number: u64, head: &str| RestoreEntry {
        repo: REPO.to_owned(),
        base: "main".to_owned(),
        run_id,
        workflow: "Build and Test".to_owned(),
        pr_number,
        head_sha: head.to_owned(),
        status_at_cancel: "queued".to_owned(),
        cause: CancelCause::YieldedTo { pr_number: 10931 },
        recorded_at: "2026-10-24T10:00:00Z".to_owned(),
    };
    ledger.record(entry(100, 1, HEAD_ONE));
    ledger.record(entry(200, 2, HEAD_TWO));
    ledger.record(entry(300, 3, HEAD_OLD));
    ledger.record(entry(400, 4, HEAD_ONE));
    save_ledger(&ledger_file, &ledger).expect("seed");

    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    let code = queue_resume_command(&args(true), &rt, true, &mut out).expect("resume");
    assert_eq!(code, ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&out));
    assert_eq!(
        mutations(&log),
        vec!["api -X POST repos/owner/repo/actions/runs/100/rerun-failed-jobs".to_owned()]
    );
    let after = load_ledger(&ledger_file).expect("ledger");
    // Rerun (100), superseded (300), and closed (400) are cleared; the
    // still-conflicting entry (200) waits for a later resume.
    assert_eq!(
        after
            .for_repo(REPO)
            .iter()
            .map(|entry| entry.run_id)
            .collect::<Vec<_>>(),
        vec![200]
    );
}

#[test]
fn resume_skips_when_the_pull_request_head_moves_after_observation() {
    let mut fixture = Fixture::new();
    fixture.prs(&[pr_row(1, HEAD_ONE, "BLOCKED", "Feature one")]);
    for status in ["queued", "waiting", "pending", "requested", "in_progress"] {
        fixture.runs(status, &[]);
    }
    fixture.head_checks(HEAD_ONE, &[]);
    fixture.route(
        "api repos/owner/repo/actions/runs/100",
        cancelled_run(run_row(
            100,
            1,
            HEAD_ONE,
            "completed",
            "pull_request",
            "09:00",
        )),
    );
    let (actions, paths, log) = fixture.build();
    // The initial observation sees HEAD_ONE; the live recheck sees a newer
    // head, so the cancelled run must remain in the ledger without rerunning.
    patch_first_then(
        &fixture,
        "*'pr list --repo owner/repo'*",
        &json!([pr_row(1, HEAD_ONE, "BLOCKED", "Feature one")]),
        &json!([pr_row(1, HEAD_TWO, "BLOCKED", "Feature one")]),
    );
    let ledger_file = paths.state_dir.join(LEDGER_FILE);
    let mut ledger = RestoreLedger::default();
    ledger.record(RestoreEntry {
        repo: REPO.to_owned(),
        base: "main".to_owned(),
        run_id: 100,
        workflow: "Build and Test".to_owned(),
        pr_number: 1,
        head_sha: HEAD_ONE.to_owned(),
        status_at_cancel: "queued".to_owned(),
        cause: CancelCause::YieldedTo { pr_number: 10931 },
        recorded_at: "2026-10-24T10:00:00Z".to_owned(),
    });
    save_ledger(&ledger_file, &ledger).expect("seed");

    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    let code = queue_resume_command(&args(true), &rt, true, &mut out).expect("resume");
    assert_eq!(code, ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&out));
    assert_eq!(
        output_json(&out)["entries"][0]["outcome"],
        "skipped_pr_head_changed"
    );
    assert!(mutations(&log).is_empty());
    assert_eq!(load_ledger(&ledger_file).expect("ledger").entries.len(), 1);
}

#[test]
fn triage_never_cancels_merge_queue_push_or_enqueued_pull_request_runs() {
    let mut fixture = Fixture::new();
    fixture.prs(&[
        pr_row(1, HEAD_ONE, "BLOCKED", "Feature one"),
        pr_row(2, HEAD_TWO, "DIRTY", "Feature two"),
    ]);
    // PR 1 is enqueued; its old-head run would otherwise be superseded.
    fixture.merge_queue(&[(1, HEAD_ONE)]);
    let mut readonly = run_row(81, 2, HEAD_TWO, "queued", "pull_request", "08:31");
    readonly["head_branch"] = json!("gh-readonly-queue/main/pr-2-abc");
    let mut push = run_row(82, 0, HEAD_BASE, "queued", "push", "08:32");
    push["head_branch"] = json!("main");
    fixture.runs(
        "queued",
        &[
            run_row(50, 1, HEAD_OLD, "queued", "pull_request", "08:00"),
            run_row(80, 1, HEAD_ONE, "queued", "merge_group", "08:30"),
            readonly,
            push,
        ],
    );
    for status in ["waiting", "pending", "requested", "in_progress"] {
        fixture.runs(status, &[]);
    }
    fixture.route(
        "api repos/owner/repo/commits/main",
        json!({"sha": HEAD_BASE}),
    );
    fixture.head_checks(HEAD_BASE, &[]);
    let (actions, paths, log) = fixture.build();
    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    let code = queue_triage_command(&args(true), true, &rt, true, &mut out).expect("triage");
    assert_eq!(code, ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&out));
    let report = output_json(&out);
    let verdicts = report["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .map(|run| {
            (
                run["run_id"].as_u64().unwrap(),
                run["verdict"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(verdicts[&50], "in_merge_queue");
    assert_eq!(verdicts[&80], "not_pull_request");
    assert_eq!(verdicts[&81], "not_pull_request");
    assert_eq!(verdicts[&82], "not_pull_request");
    assert_eq!(report["doomed"], 0);
    assert!(mutations(&log).is_empty());
}

#[test]
fn a_pull_request_that_enters_the_merge_queue_before_the_cancel_keeps_its_runs() {
    let mut fixture = Fixture::new();
    fixture.prs(&[pr_row(1, HEAD_ONE, "BLOCKED", "Feature one")]);
    fixture.runs(
        "queued",
        &[run_row(50, 1, HEAD_OLD, "queued", "pull_request", "08:00")],
    );
    for status in ["waiting", "pending", "requested", "in_progress"] {
        fixture.runs(status, &[]);
    }
    fixture.route(
        "api repos/owner/repo/commits/main",
        json!({"sha": HEAD_BASE}),
    );
    fixture.head_checks(HEAD_BASE, &[]);
    let (actions, paths, log) = fixture.build();
    patch_first_then(
        &fixture,
        "'api graphql '*",
        &merge_queue_body(&[]),
        &merge_queue_body(&[(1, HEAD_ONE)]),
    );
    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    queue_triage_command(&args(true), true, &rt, true, &mut out).expect("triage");
    let report = output_json(&out);
    assert_eq!(report["doomed"], 1);
    assert_eq!(
        report["cancellations"][0]["outcome"],
        "skipped_pull_request_enqueued"
    );
    assert!(mutations(&log).is_empty());
    let ledger = load_ledger(&paths.state_dir.join(LEDGER_FILE)).expect("ledger");
    assert!(ledger.entries.is_empty());
}

#[test]
fn prioritize_keeps_runs_of_enqueued_pull_requests_ahead() {
    let fixture = prioritize_fixture();
    // PR 1 owns the only run that would otherwise yield.
    fixture.merge_queue(&[(1, HEAD_ONE)]);
    let (actions, paths, log) = fixture.build();
    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    queue_prioritize_command(&args(true), 10931, &rt, true, &mut out).expect("run");
    let report = output_json(&out);
    assert_eq!(report["plan"]["yields"], json!([]));
    assert!(
        report["plan"]["kept"]
            .as_array()
            .expect("kept")
            .iter()
            .any(|kept| kept["run_id"] == 100)
    );
    assert!(mutations(&log).is_empty());
}

#[test]
fn resume_holds_entries_whose_pull_request_is_enqueued() {
    let mut fixture = Fixture::new();
    fixture.prs(&[pr_row(1, HEAD_ONE, "BLOCKED", "Feature one")]);
    fixture.merge_queue(&[(1, HEAD_ONE)]);
    for status in ["queued", "waiting", "pending", "requested", "in_progress"] {
        fixture.runs(status, &[]);
    }
    fixture.head_checks(HEAD_ONE, &[]);
    let (actions, paths, log) = fixture.build();
    let ledger_file = paths.state_dir.join(LEDGER_FILE);
    let mut ledger = RestoreLedger::default();
    ledger.record(RestoreEntry {
        repo: REPO.to_owned(),
        base: "main".to_owned(),
        run_id: 100,
        workflow: "Build and Test".to_owned(),
        pr_number: 1,
        head_sha: HEAD_ONE.to_owned(),
        status_at_cancel: "queued".to_owned(),
        cause: CancelCause::YieldedTo { pr_number: 10931 },
        recorded_at: "2026-10-24T10:00:00Z".to_owned(),
    });
    save_ledger(&ledger_file, &ledger).expect("seed");
    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    let code = queue_resume_command(&args(true), &rt, true, &mut out).expect("resume");
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(
        output_json(&out)["entries"][0]["action"],
        "hold_for_merge_queue"
    );
    assert!(mutations(&log).is_empty());
    assert_eq!(load_ledger(&ledger_file).expect("ledger").entries.len(), 1);
}

#[test]
fn resume_refuses_forged_ledger_entries_and_keeps_them() {
    let mut fixture = Fixture::new();
    fixture.prs(&[
        pr_row(1, HEAD_ONE, "BLOCKED", "Feature one"),
        pr_row(2, HEAD_TWO, "BLOCKED", "Feature two"),
    ]);
    for status in ["queued", "waiting", "pending", "requested", "in_progress"] {
        fixture.runs(status, &[]);
    }
    fixture.head_checks(HEAD_ONE, &[]);
    fixture.head_checks(HEAD_TWO, &[]);
    // Each entry claims a cancelled PR-1 run at HEAD_ONE, but the live run is
    // a merge-group run, a push, a run at another head, or another PR's run.
    let mut merge_group = run_row(501, 1, HEAD_ONE, "completed", "merge_group", "09:00");
    merge_group["head_branch"] = json!("gh-readonly-queue/main/pr-1-abc");
    let mut push = run_row(502, 1, HEAD_ONE, "completed", "push", "09:00");
    push["head_branch"] = json!("main");
    let other_head = run_row(503, 1, HEAD_OLD, "completed", "pull_request", "09:00");
    let other_pr = run_row(504, 2, HEAD_ONE, "completed", "pull_request", "09:00");
    for (run_id, run) in [
        (501, merge_group),
        (502, push),
        (503, other_head),
        (504, other_pr),
    ] {
        fixture.route(
            &format!("api repos/owner/repo/actions/runs/{run_id}"),
            cancelled_run(run),
        );
    }
    let (actions, paths, log) = fixture.build();
    let ledger_file = paths.state_dir.join(LEDGER_FILE);
    let mut ledger = RestoreLedger::default();
    for run_id in [501, 502, 503, 504] {
        ledger.record(ledger_entry(REPO, run_id, 1, HEAD_ONE));
    }
    // An entry naming another repository is never acted on against it, nor
    // against the repository being resumed.
    ledger.record(ledger_entry("victim/other", 505, 1, HEAD_ONE));
    save_ledger(&ledger_file, &ledger).expect("seed");

    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    let code = queue_resume_command(&args(true), &rt, true, &mut out).expect("resume");
    assert_eq!(code, ExitCode::from(1), "{}", String::from_utf8_lossy(&out));
    let report = output_json(&out);
    let outcomes = report["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| {
            (
                entry["run_id"].as_u64().unwrap(),
                entry["outcome"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for run_id in [501, 502, 503, 504] {
        assert_eq!(outcomes[&run_id], "refused", "{report}");
    }
    assert_eq!(outcomes[&505], "refused_other_repository");
    assert!(mutations(&log).is_empty());
    let calls = fs::read_to_string(&log).expect("log");
    assert!(!calls.contains("victim/other"));
    assert!(!calls.contains("runs/505"));
    assert_eq!(load_ledger(&ledger_file).expect("ledger").entries.len(), 5);
}

#[test]
fn triage_lets_a_running_gate_finish_when_only_a_required_check_failed() {
    let mut fixture = Fixture::new();
    fixture.prs(&[
        pr_row(1, HEAD_ONE, "BLOCKED", "Feature one"),
        pr_row(2, HEAD_TWO, "BLOCKED", "Feature two"),
    ]);
    for status in ["queued", "waiting", "pending", "requested"] {
        fixture.runs(status, &[]);
    }
    fixture.runs(
        "in_progress",
        &[
            // Required check failed on PR 1's head, but its gate is running.
            run_row(60, 1, HEAD_ONE, "in_progress", "pull_request", "08:00"),
            // PR 2 advanced past this running run: superseded, cancelled.
            run_row(61, 2, HEAD_OLD, "in_progress", "pull_request", "08:01"),
        ],
    );
    fixture.head_checks(HEAD_ONE, &[check_row("macos", "failure", 59)]);
    fixture.route(
        "api repos/owner/repo/commits/main",
        json!({"sha": HEAD_BASE}),
    );
    fixture.head_checks(HEAD_BASE, &[]);
    fixture.route(
        "api repos/owner/repo/actions/runs/61",
        json!({"status": "in_progress", "conclusion": null}),
    );
    let (actions, paths, log) = fixture.build();
    let rt = runtime(&fixture.temp, &paths, &actions);
    let mut out = Vec::new();
    let code = queue_triage_command(&args(true), true, &rt, true, &mut out).expect("triage");
    assert_eq!(code, ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&out));
    let report = output_json(&out);
    assert_eq!(report["doomed"], 2);
    let cancel = report["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .map(|run| {
            (
                run["run_id"].as_u64().unwrap(),
                run["cancel"].as_bool().unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert!(!cancel[&60]);
    assert!(cancel[&61]);
    assert_eq!(
        mutations(&log),
        vec!["api -X POST repos/owner/repo/actions/runs/61/cancel".to_owned()]
    );
}
