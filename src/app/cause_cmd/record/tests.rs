use std::sync::Mutex;

use serde_json::{Value, json};

use super::*;

/// A fake GitHub: each path is answered by the first route whose needle it
/// contains; anything else is a 404. Every path asked for is kept.
struct Fake {
    routes: Vec<(String, String)>,
    asked: Mutex<Vec<String>>,
}

impl Fake {
    fn new(routes: &[(&str, Value)]) -> Self {
        Self {
            routes: routes
                .iter()
                .map(|(needle, body)| ((*needle).to_owned(), body.to_string()))
                .collect(),
            asked: Mutex::new(Vec::new()),
        }
    }

    fn with_log(mut self, needle: &str, text: &str) -> Self {
        self.routes.insert(0, (needle.to_owned(), text.to_owned()));
        self
    }

    fn read(&self, args: &[String]) -> Result<String, String> {
        let path = args.last().cloned().unwrap_or_default();
        self.asked.lock().expect("lock").push(path.clone());
        self.routes
            .iter()
            .find(|(needle, _)| path.contains(needle.as_str()))
            .map(|(_, body)| body.clone())
            .ok_or_else(|| format!("HTTP 404: {path}"))
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("lock").clone()
    }
}

fn drift_log() -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/c2/generated_drift.log",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("drift fixture")
}

fn query(dir: &Path) -> RecordQuery {
    RecordQuery {
        repo: "Generous-Corp/pulp".to_owned(),
        workflow: "build.yml".to_owned(),
        dir: dir.to_path_buf(),
        since: "2026-10-13T00:00:00Z".to_owned(),
        base: "main".to_owned(),
        now: "2026-10-14T12:00:00Z".to_owned(),
    }
}

fn red_run() -> Value {
    json!({
        "id": 100, "name": "Build", "run_attempt": 1, "conclusion": "failure",
        "event": "pull_request", "head_sha": "abc123", "head_branch": "feat/x",
        "pull_requests": [{"number": 42}], "created_at": "2026-10-14T10:00:00Z",
        "run_started_at": "2026-10-14T10:00:00Z",
        "html_url": "https://github.com/Generous-Corp/pulp/actions/runs/100"
    })
}

fn job(id: u64, conclusion: &str) -> Value {
    json!({
        "id": id, "name": "macos", "status": "completed", "conclusion": conclusion,
        "run_id": 100, "run_attempt": 1, "head_sha": "abc123", "head_branch": "feat/x",
        "completed_at": "2026-10-14T10:30:00Z",
        "html_url": format!("https://github.com/Generous-Corp/pulp/actions/runs/100/job/{id}"),
        "steps": [{"name": "Run ctest", "conclusion": conclusion, "number": 5,
                   "started_at": "2026-10-14T10:10:00Z", "completed_at": "2026-10-14T10:30:00Z"}]
    })
}

fn red_routes() -> Vec<(&'static str, Value)> {
    vec![
        (
            "runs?status=completed",
            json!({"workflow_runs": [red_run()]}),
        ),
        (
            "runs/100/jobs",
            json!({"jobs": [job(1000, "failure"), job(1001, "cancelled"), job(1002, "success")]}),
        ),
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": []}),
        ),
    ]
}

fn rows(path: &Path) -> Vec<Value> {
    read_jsonl(path)
}

#[test]
fn a_red_drift_job_gets_one_record_naming_its_generator() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| fake.read(args);
    let report = record(&gh, &query(dir.path())).expect("pass");
    assert_eq!(report.causes_written, 1);
    assert_eq!(report.cancelled_skipped, 1);
    assert_eq!(report.reds_pending, 1);
    let causes = rows(&dir.path().join("causes/2026-10.jsonl"));
    assert_eq!(causes.len(), 1);
    let row = &causes[0];
    assert_eq!(row["format"], "generated-drift");
    assert_eq!(row["failure_class"], "drift");
    assert_eq!(row["signature"], "script-test-inputs");
    assert_eq!(row["confidence"], "high");
    assert!(
        row["fix_hint"].as_str().is_some_and(
            |hint| hint.contains("script_test_inputs.py --build-dir build-macos --write")
        ),
        "{row}"
    );
    assert_eq!(row["head_sha"], "abc123");
    assert_eq!(
        (row["run_id"].clone(), row["job_id"].clone()),
        (json!(100), json!(1000))
    );
    assert_eq!(row["attempt"], 1);
    assert_eq!(row["completed_at"], "2026-10-14T10:30:00Z");
    assert_eq!(
        (row["pr"].clone(), row["owner"].clone()),
        (json!(42), json!("pr:42"))
    );
    assert_eq!(row["head_branch"], "feat/x");
    assert_eq!(row["provenance"]["extract_version"], c2::VERSION);
    assert_eq!(row["provenance"]["collector"], COLLECTOR);

    // A second pass finds it recorded and writes nothing again.
    let again = record(&gh, &query(dir.path())).expect("pass");
    assert_eq!((again.causes_written, again.already_recorded), (0, 1));
    assert_eq!(rows(&dir.path().join("causes/2026-10.jsonl")).len(), 1);
}

#[test]
fn a_green_run_writes_nothing_and_reads_no_jobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut green = red_run();
    green["conclusion"] = json!("success");
    let fake = Fake::new(&[("runs?status=completed", json!({"workflow_runs": [green]}))]);
    let gh = |args: &[String]| fake.read(args);
    let report = record(&gh, &query(dir.path())).expect("pass");
    assert_eq!(
        (
            report.runs_listed,
            report.runs_read,
            report.causes_written,
            report.fixlinks_written
        ),
        (1, 0, 0, 0)
    );
    assert!(!dir.path().join("causes").exists());
    assert!(!dir.path().join("fixlinks.jsonl").exists());
    assert!(
        fake.asked().iter().all(|path| !path.contains("/jobs")),
        "{:?}",
        fake.asked()
    );
}

#[test]
fn a_later_green_push_writes_a_fix_link_with_its_own_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");

    let mut routes = red_routes();
    routes.retain(|(needle, _)| !needle.starts_with("runs?branch="));
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [{
                "id": 200, "conclusion": "success", "head_sha": "def456",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        // The pull request fixed the red in c1, then merged main (m1, which
        // brought docs/main-only.md); the fix head is that merge. Both ends are
        // already in main, as they are once the pull request has landed, so
        // a merge base taken against today's main would erase every file.
        (
            "compare/abc123...def456",
            json!({
                "status": "ahead", "total_commits": 3,
                "files": [{"filename": "test/ctest_script_inputs.json"}, {"filename": "docs/main-only.md"}],
                "commits": [
                    {"sha": "c1", "parents": [{"sha": "abc123"}]},
                    {"sha": "m1", "parents": [{"sha": "main0"}]},
                    {"sha": "def456", "parents": [{"sha": "c1"}, {"sha": "m1"}]}
                ]
            }),
        ),
        ("commits/c1", json!({"files": [{"filename": "test/ctest_script_inputs.json"}]})),
        ("commits/m1", json!({"files": [{"filename": "docs/main-only.md"}]})),
        ("compare/main...", json!({"merge_base_commit": {"sha": "abc123"}})),
    ]);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("green pass");
    assert_eq!((report.fixlinks_written, report.reds_pending), (1, 0));
    let links = rows(&dir.path().join("fixlinks.jsonl"));
    assert_eq!(links.len(), 1);
    let link = &links[0];
    assert_eq!(link["derivation"], "compare_red_head_to_fix_head");
    assert_eq!(
        (link["head_sha"].clone(), link["fix_head_sha"].clone()),
        (json!("abc123"), json!("def456"))
    );
    assert_eq!(
        (link["red_run_id"].clone(), link["red_attempt"].clone()),
        (json!(100), json!(1))
    );
    assert_eq!(link["fix_run_id"], 200);
    assert_eq!(link["pushed_at"], "2026-10-14T11:00:00Z");
    assert_eq!(link["red_time"], "2026-10-14T10:30:00Z");
    assert_eq!(link["pr"], 42);
    assert_eq!(
        link["files_raw"],
        json!(["docs/main-only.md", "test/ctest_script_inputs.json"])
    );
    assert_eq!(link["own_files"], json!(["test/ctest_script_inputs.json"]));
    assert_eq!(link["own_files_method"], "first_parent_non_merge_commits");
    assert_eq!(link["own_files_truncated"], false);
    let asked = second.asked();
    assert!(
        asked.iter().all(|path| !path.contains("compare/main...")
            && !path.contains("commits/m1")
            && !path.contains("commits/def456")),
        "{asked:?}"
    );

    // Linked once: the next pass does not link it again.
    let again = record(&gh, &query(dir.path())).expect("pass");
    assert_eq!(again.fixlinks_written, 0);
    assert_eq!(rows(&dir.path().join("fixlinks.jsonl")).len(), 1);
}

#[test]
fn a_force_pushed_fix_has_no_own_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");
    let mut routes = red_routes();
    routes.retain(|(needle, _)| !needle.starts_with("runs?branch="));
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [{
                "id": 200, "conclusion": "success", "head_sha": "def456",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        (
            "compare/abc123...def456",
            json!({"status": "diverged", "files": [{"filename": "a.cpp"}], "commits": []}),
        ),
    ]);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    record(&gh, &query(dir.path())).expect("green pass");
    let link = &rows(&dir.path().join("fixlinks.jsonl"))[0];
    assert_eq!(link["files_raw"], json!(["a.cpp"]));
    assert_eq!(link["own_files"], Value::Null);
    assert_eq!(link["own_files_method"], "red_not_ancestor");
}

#[test]
fn a_rerun_that_passes_links_the_red_attempt_to_its_own_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rerun = red_run();
    rerun["conclusion"] = json!("success");
    rerun["run_attempt"] = json!(2);
    rerun["run_started_at"] = json!("2026-10-14T10:45:00Z");
    let mut routes = red_routes();
    routes[0] = ("runs?status=completed", json!({"workflow_runs": [rerun]}));
    let fake = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| fake.read(args);
    let report = record(&gh, &query(dir.path())).expect("pass");
    assert_eq!((report.causes_written, report.fixlinks_written), (1, 1));
    let link = &rows(&dir.path().join("fixlinks.jsonl"))[0];
    assert_eq!(link["derivation"], "rerun_green");
    assert_eq!(link["fix_head_sha"], "abc123");
    assert_eq!(link["pushed_at"], "2026-10-14T10:45:00Z");
    assert_eq!(link["own_files"], json!([]));
}

#[test]
fn a_merge_queue_red_is_owned_by_its_pull_request() {
    assert_eq!(
        queue_pr("gh-readonly-queue/main/pr-10261-4317349f"),
        Some(10261)
    );
    assert_eq!(queue_pr("feature/x"), None);
}

#[test]
fn scrub_removes_private_hosts_and_paths_but_keeps_urls() {
    let row = scrub(json!({
        "detail": "ran on Daniels-Mac-Studio-m3-4.local at /Users/admin/x from 10.0.0.5 token=abcdefghij",
        "provenance": {"run_url": "https://github.com/a/b/actions/runs/1"}
    }));
    let detail = row["detail"].as_str().expect("detail");
    for leaked in ["Mac-Studio", "/Users/", "10.0.0.5", "abcdefghij", "m3"] {
        assert!(!detail.contains(leaked), "{detail}");
    }
    assert_eq!(
        row["provenance"]["run_url"],
        "https://github.com/a/b/actions/runs/1"
    );
}
