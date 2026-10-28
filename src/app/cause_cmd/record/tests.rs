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

    fn with_error(mut self, needle: &str, error: &str) -> Self {
        self.routes
            .insert(0, (needle.to_owned(), format!("__ERROR__:{error}")));
        self
    }

    fn read(&self, args: &[String]) -> Result<String, String> {
        let path = args.last().cloned().unwrap_or_default();
        self.asked.lock().expect("lock").push(path.clone());
        let body = self
            .routes
            .iter()
            .find(|(needle, _)| path.contains(needle.as_str()))
            .map(|(_, body)| body.clone())
            .ok_or_else(|| format!("HTTP 404: {path}"))?;
        if let Some(error) = body.strip_prefix("__ERROR__:") {
            Err(error.to_owned())
        } else {
            Ok(body)
        }
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
        "event": "pull_request", "head_sha": "0123456789abcdef0123456789abcdef01234567", "head_branch": "feat/x",
        "pull_requests": [{"number": 42}], "created_at": "2026-10-14T10:00:00Z",
        "run_started_at": "2026-10-14T10:00:00Z",
        "html_url": "https://github.com/Generous-Corp/pulp/actions/runs/100"
    })
}

fn job(id: u64, conclusion: &str) -> Value {
    json!({
        "id": id, "name": "macos", "status": "completed", "conclusion": conclusion,
        "run_id": 100, "run_attempt": 1, "head_sha": "0123456789abcdef0123456789abcdef01234567", "head_branch": "feat/x",
        "completed_at": "2026-10-14T10:30:00Z",
        "html_url": format!("https://github.com/Generous-Corp/pulp/actions/runs/100/job/{id}"),
        "steps": [{"name": "Run ctest", "conclusion": conclusion, "number": 5,
                   "started_at": "2026-10-14T10:10:00Z", "completed_at": "2026-10-14T10:30:00Z"}]
    })
}

fn red_routes() -> Vec<(&'static str, Value)> {
    let mut cancelled = job(1001, "cancelled");
    cancelled["name"] = json!("linux");
    let mut successful = job(1002, "success");
    successful["name"] = json!("windows");
    vec![
        (
            "runs?status=completed",
            json!({"workflow_runs": [red_run()]}),
        ),
        (
            "runs/100/jobs",
            json!({"jobs": [job(1000, "failure"), cancelled, successful]}),
        ),
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": []}),
        ),
        ("runs/200/jobs", json!({"jobs": [job(2000, "success")]})),
    ]
}

fn rows(path: &Path) -> Vec<Value> {
    read_jsonl(path)
}

#[test]
fn run_jobs_are_paginated_with_a_bounded_page_count() {
    let first_page = Value::Array((0..100).map(|id| json!({"id": id})).collect());
    let fake = Fake::new(&[
        (
            "runs/100/jobs?filter=all&per_page=100&page=1",
            json!({"jobs": first_page}),
        ),
        (
            "runs/100/jobs?filter=all&per_page=100&page=2",
            json!({"jobs": [{"id": 100}]}),
        ),
    ]);
    let gh = |args: &[String]| fake.read(args);
    let jobs = list_run_jobs(
        &gh,
        &query(tempfile::tempdir().expect("tempdir").path()),
        100,
    )
    .expect("jobs");
    assert_eq!(jobs.len(), 101);
    assert_eq!(jobs[100]["id"], 100);
}

#[test]
fn run_listing_fails_closed_on_malformed_or_bounded_input() {
    let malformed = Fake::new(&[("runs?", json!({"workflow_runs": {}}))]);
    let gh = |args: &[String]| malformed.read(args);
    let error = list_runs(
        &gh,
        &query(tempfile::tempdir().expect("tempdir").path()),
        "status=completed",
    )
    .expect_err("malformed run listing");
    assert!(error.contains("workflow_runs[]"), "{error}");

    let full_page = Value::Array((0..100).map(|id| json!({"id": id})).collect());
    let bounded = Fake::new(&[("runs?", json!({"workflow_runs": full_page}))]);
    let gh = |args: &[String]| bounded.read(args);
    let error = list_runs(
        &gh,
        &query(tempfile::tempdir().expect("tempdir").path()),
        "status=completed",
    )
    .expect_err("bounded run listing");
    assert!(error.contains("10-page run listing bound"), "{error}");
}

#[test]
fn a_full_bounded_job_listing_is_not_treated_as_complete() {
    assert!(job_list_complete(&vec![json!({}); 999]));
    assert!(!job_list_complete(&vec![json!({}); 1000]));
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
    assert_eq!(row["head_sha"], "0123456789abcdef0123456789abcdef01234567");
    assert_eq!(
        (row["run_id"].clone(), row["job_id"].clone()),
        (json!(100), json!(1000))
    );
    assert_eq!(row["attempt"], 1);
    assert_eq!(row["completed_at"], "2026-10-14T10:30:00Z");
    // Named live, at this pass: time to cause is named_at minus completed_at.
    assert_eq!(row["named_at"], "2026-10-14T12:00:00Z");
    assert_eq!(row["retrospective"], false);
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
                "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        // The pull request fixed the red in c1, then merged main (m1, which
        // brought docs/main-only.md); the fix head is that merge. Both ends are
        // already in main, as they are once the pull request has landed, so
        // a merge base taken against today's main would erase every file.
        (
            "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98",
            json!({
                "status": "ahead", "total_commits": 3,
                "files": [{"filename": "test/ctest_script_inputs.json"}, {"filename": "docs/main-only.md"}],
                "commits": [
                    {"sha": "c1", "parents": [{"sha": "0123456789abcdef0123456789abcdef01234567"}]},
                    {"sha": "m1", "parents": [{"sha": "main0"}]},
                    {"sha": "fedcba9876543210fedcba9876543210fedcba98", "parents": [{"sha": "c1"}, {"sha": "m1"}]}
                ]
            }),
        ),
        ("commits/c1", json!({"files": [{"filename": "test/ctest_script_inputs.json"}]})),
        ("commits/m1", json!({"files": [{"filename": "docs/main-only.md"}]})),
        ("compare/main...", json!({"merge_base_commit": {"sha": "0123456789abcdef0123456789abcdef01234567"}})),
        ("runs/200/jobs", json!({"jobs": [job(2000, "success")]})),
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
        (
            json!("0123456789abcdef0123456789abcdef01234567"),
            json!("fedcba9876543210fedcba9876543210fedcba98")
        )
    );
    assert_eq!(
        (link["red_run_id"].clone(), link["red_attempt"].clone()),
        (json!(100), json!(1))
    );
    assert_eq!(link["fix_run_id"], 200);
    assert!(super::super::advice::valid_fixlink(link));
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
            && !path.contains("commits/fedcba9876543210fedcba9876543210fedcba98")),
        "{asked:?}"
    );

    // A structurally complete row is still deduplicated even if advice will
    // reject its unsafely named branch.
    let mut unsafe_link = link.clone();
    unsafe_link["branch"] = json!("fix/drop-github_pat_x");
    std::fs::write(
        dir.path().join("fixlinks.jsonl"),
        unsafe_link.to_string() + "\n",
    )
    .expect("unsafe complete fix-link");
    let unsafe_again = record(&gh, &query(dir.path())).expect("unsafe idempotence pass");
    assert_eq!(unsafe_again.fixlinks_written, 0);

    // A structurally incomplete row must not suppress backfill of the same
    // red attempt; once a complete row is written, the next pass is idempotent.
    std::fs::write(
        dir.path().join("fixlinks.jsonl"),
        json!({
            "schema": cause::SCHEMA,
            "source": "fix-link",
            "repo": "Generous-Corp/pulp",
            "red_run_id": 100,
            "red_attempt": 1,
            // A nonempty but malformed identity must not suppress producer backfill.
            "head_sha": "malformed",
            "fix_head_sha": "fedcba9876543210fedcba9876543210fedcba98",
            "derivation": "compare_red_head_to_fix_head"
        })
        .to_string()
            + "\n",
    )
    .expect("partial fix-link");
    let recovered = record(&gh, &query(dir.path())).expect("backfill pass");
    assert_eq!(recovered.fixlinks_written, 1);

    // Linked once: the next pass does not link it again.
    let again = record(&gh, &query(dir.path())).expect("pass");
    assert_eq!(again.fixlinks_written, 0);
    assert_eq!(rows(&dir.path().join("fixlinks.jsonl")).len(), 2);
}

#[test]
fn a_long_range_is_read_page_by_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");
    let chain = json!([
        {"sha": "c1", "parents": [{"sha": "0123456789abcdef0123456789abcdef01234567"}]},
        {"sha": "m1", "parents": [{"sha": "main0"}]},
        {"sha": "fedcba9876543210fedcba9876543210fedcba98", "parents": [{"sha": "c1"}, {"sha": "m1"}]}
    ]);
    let mut routes = red_routes();
    routes.retain(|(needle, _)| !needle.starts_with("runs?branch="));
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [{
                "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        // Paged reads first: the unpaged needle is a prefix of theirs.
        (
            "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98?per_page=100&page=1",
            json!({"status": "ahead", "total_commits": 3, "commits": chain}),
        ),
        (
            "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98?per_page=100&page=2",
            json!({"status": "ahead", "total_commits": 3, "commits": []}),
        ),
        // Unpaged, the listing stops short of the range.
        (
            "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98",
            json!({"status": "ahead", "total_commits": 3,
                   "files": [{"filename": "a.cpp"}, {"filename": "docs/main-only.md"}],
                   "commits": [{"sha": "c1", "parents": [{"sha": "0123456789abcdef0123456789abcdef01234567"}]}]}),
        ),
        ("commits/c1", json!({"files": [{"filename": "a.cpp"}]})),
    ]);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    record(&gh, &query(dir.path())).expect("green pass");
    let link = &rows(&dir.path().join("fixlinks.jsonl"))[0];
    assert_eq!(link["own_files_method"], "first_parent_non_merge_commits");
    assert_eq!(link["own_files"], json!(["a.cpp"]));
}

/// A red recorded, then a green push to `fedcba9876543210fedcba9876543210fedcba98` whose compare answers with
/// `compare` routes; returns the fix-link and every path asked.
fn fix_link_with(compare: Vec<(&'static str, Value)>) -> (Value, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");
    let mut routes = red_routes();
    routes.retain(|(needle, _)| !needle.starts_with("runs?branch="));
    routes.push((
        "runs?branch=feat%2Fx&status=success",
        json!({"workflow_runs": [{
            "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
            "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
        }]}),
    ));
    routes.extend(compare);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    record(&gh, &query(dir.path())).expect("green pass");
    let link = rows(&dir.path().join("fixlinks.jsonl"))[0].clone();
    (link, second.asked())
}

fn hundred_commits() -> Value {
    Value::Array(
        (0..100)
            .map(|i| json!({"sha": format!("x{i}"), "parents": [{"sha": format!("y{i}")}]}))
            .collect(),
    )
}

#[test]
fn a_range_past_the_paging_cap_is_too_many_commits() {
    let (link, asked) = fix_link_with(vec![
        (
            "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98?per_page=100",
            json!({"status": "ahead", "total_commits": 1001, "commits": hundred_commits()}),
        ),
        (
            "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98",
            json!({"status": "ahead", "total_commits": 1001,
                   "files": [{"filename": "a.cpp"}], "commits": []}),
        ),
    ]);
    assert_eq!(link["own_files"], Value::Null);
    assert_eq!(link["own_files_method"], "too_many_commits");
    assert_eq!(link["files_raw"], json!(["a.cpp"]));
    let pages = asked
        .iter()
        .filter(|path| path.contains("compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98?per_page=100"))
        .count();
    assert_eq!(pages, COMPARE_COMMIT_PAGES, "{asked:?}");
    assert!(
        asked.iter().all(|path| !path.contains("/commits/")),
        "{asked:?}"
    );
}

#[test]
fn pages_that_run_out_before_the_total_are_too_many_commits() {
    let (link, _) = fix_link_with(vec![
        (
            "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98?per_page=100&page=1",
            json!({"status": "ahead", "total_commits": 300, "commits": hundred_commits()}),
        ),
        (
            "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98?per_page=100",
            json!({"status": "ahead", "total_commits": 300, "commits": []}),
        ),
        (
            "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98",
            json!({"status": "ahead", "total_commits": 300,
                   "files": [{"filename": "a.cpp"}], "commits": []}),
        ),
    ]);
    assert_eq!(link["own_files"], Value::Null);
    assert_eq!(link["own_files_method"], "too_many_commits");
}

#[test]
fn scrub_removes_github_tokens() {
    let row = scrub(json!({"detail": [
        "pushed with ghp_0123456789abcdefghijABCDEFGHIJ012345",
        "app token ghs_0123456789abcdefghijABCDEF and gho_0123456789abcdefghijABCD",
        "user ghu_0123456789abcdefghijABCDEFGH refresh ghr_0123456789abcdefghijABCDEF",
        "fine-grained github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz"
    ]}));
    let text = row.to_string();
    for prefix in ["ghp_", "ghs_", "gho_", "ghu_", "ghr_", "github_pat_"] {
        assert!(!text.contains(prefix), "{text}");
    }
    assert_eq!(text.matches("<secret>").count(), 6, "{text}");
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
                "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        (
            "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98",
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
    let mut passed = job(1000, "success");
    passed["run_attempt"] = json!(2);
    routes[1] = (
        "runs/100/jobs",
        json!({"jobs": [job(1000, "failure"), passed]}),
    );
    let fake = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| fake.read(args);
    let report = record(&gh, &query(dir.path())).expect("pass");
    assert_eq!((report.causes_written, report.fixlinks_written), (1, 1));
    let link = &rows(&dir.path().join("fixlinks.jsonl"))[0];
    assert_eq!(link["derivation"], "rerun_green");
    assert_eq!(
        link["fix_head_sha"],
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(link["pushed_at"], "2026-10-14T10:45:00Z");
    assert_eq!(link["own_files"], json!([]));
    assert!(super::super::advice::valid_fixlink(link));
}

#[test]
fn scrubbed_job_names_still_match_the_api_when_proving_a_fix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut failed = job(1000, "failure");
    failed["name"] = json!("test-m1");
    let mut passed = job(1000, "success");
    passed["name"] = json!("test-m1");
    passed["run_attempt"] = json!(2);
    let mut rerun = red_run();
    rerun["conclusion"] = json!("success");
    rerun["run_attempt"] = json!(2);
    rerun["run_started_at"] = json!("2026-10-14T10:45:00Z");
    let mut routes = red_routes();
    routes[0] = ("runs?status=completed", json!({"workflow_runs": [rerun]}));
    routes[1] = ("runs/100/jobs", json!({"jobs": [failed, passed]}));
    let fake = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| fake.read(args);
    let report = record(&gh, &query(dir.path())).expect("scrubbed rerun");
    assert_eq!((report.causes_written, report.fixlinks_written), (1, 1));
}

#[test]
fn distinct_raw_job_names_that_scrub_alike_cannot_prove_a_fix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut failed = job(1000, "failure");
    failed["name"] = json!("test-m1");
    let mut routes = red_routes();
    routes[1] = ("runs/100/jobs", json!({"jobs": [failed.clone()]}));
    let first = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");

    let mut skipped = job(2000, "skipped");
    skipped["run_id"] = json!(200);
    skipped["head_sha"] = json!("fedcba9876543210fedcba9876543210fedcba98");
    skipped["name"] = json!("test-m1");
    let mut other_host = job(2001, "success");
    other_host["run_id"] = json!(200);
    other_host["head_sha"] = json!("fedcba9876543210fedcba9876543210fedcba98");
    other_host["name"] = json!("test-m3");
    let mut routes = red_routes();
    routes[1] = ("runs/100/jobs", json!({"jobs": [failed]}));
    routes.retain(|(needle, _)| {
        !needle.starts_with("runs?branch=") && !needle.contains("runs/200/jobs")
    });
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [{
                "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        ("runs/200/jobs", json!({"jobs": [skipped, other_host]})),
    ]);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("same scrubbed host");
    assert_eq!((report.fixlinks_written, report.reds_pending), (0, 1));
}

#[test]
fn duplicate_raw_job_names_cannot_prove_a_fix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let failed_a = job(1000, "failure");
    let mut failed_b = job(1001, "failure");
    failed_b["id"] = json!(1001);
    let mut routes = red_routes();
    routes[1] = (
        "runs/100/jobs",
        json!({"jobs": [failed_a.clone(), failed_b.clone()]}),
    );
    let first = Fake::new(&routes)
        .with_log("jobs/1000/logs", &drift_log())
        .with_log("jobs/1001/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");

    let mut green = job(2000, "success");
    green["run_id"] = json!(200);
    green["head_sha"] = json!("fedcba9876543210fedcba9876543210fedcba98");
    let mut routes = red_routes();
    routes[1] = ("runs/100/jobs", json!({"jobs": [failed_a, failed_b]}));
    routes.retain(|(needle, _)| {
        !needle.starts_with("runs?branch=") && !needle.contains("runs/200/jobs")
    });
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [{
                "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        ("runs/200/jobs", json!({"jobs": [green]})),
    ]);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("ambiguous red remains pending");
    assert_eq!((report.fixlinks_written, report.reds_pending), (0, 1));
}

#[test]
fn mixed_duplicate_job_names_cannot_prove_a_fix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let failed = job(1000, "failure");
    let mut routes = red_routes();
    routes[1] = (
        "runs/100/jobs",
        json!({"jobs": [failed.clone(), job(1001, "success")]}),
    );
    let first = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");

    let mut green = job(2000, "success");
    green["run_id"] = json!(200);
    green["head_sha"] = json!("fedcba9876543210fedcba9876543210fedcba98");
    let mut routes = red_routes();
    routes[1] = (
        "runs/100/jobs",
        json!({"jobs": [failed, job(1001, "success")]}),
    );
    routes.retain(|(needle, _)| {
        !needle.starts_with("runs?branch=") && !needle.contains("runs/200/jobs")
    });
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [{
                "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        ("runs/200/jobs", json!({"jobs": [green]})),
    ]);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("ambiguous red remains pending");
    assert_eq!((report.fixlinks_written, report.reds_pending), (0, 1));
}

#[test]
fn unnamed_failed_jobs_cannot_prove_a_fix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut unnamed = job(1000, "failure");
    unnamed["name"] = Value::Null;
    let mut routes = red_routes();
    routes[1] = ("runs/100/jobs", json!({"jobs": [unnamed]}));
    let fake = Fake::new(&routes);
    let gh = |args: &[String]| fake.read(args);
    let report = record(&gh, &query(dir.path())).expect("unnamed red pass");
    assert_eq!(report.causes_written, 0);
}

#[test]
fn a_selective_rerun_can_pass_failed_jobs_across_multiple_attempts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rerun = red_run();
    rerun["conclusion"] = json!("success");
    rerun["run_attempt"] = json!(3);
    rerun["run_started_at"] = json!("2026-10-14T10:45:00Z");
    let mut failed_a = job(1000, "failure");
    let mut failed_b = job(1001, "failure");
    failed_b["name"] = json!("linux");
    let mut passed_a = job(1002, "success");
    let mut passed_b = job(1003, "success");
    passed_b["name"] = json!("linux");
    failed_a["run_attempt"] = json!(1);
    failed_b["run_attempt"] = json!(1);
    passed_a["run_attempt"] = json!(2);
    passed_b["run_attempt"] = json!(3);
    let mut routes = red_routes();
    routes[0] = ("runs?status=completed", json!({"workflow_runs": [rerun]}));
    routes[1] = (
        "runs/100/jobs",
        json!({"jobs": [failed_a, failed_b, passed_a, passed_b]}),
    );
    let fake = Fake::new(&routes)
        .with_log("jobs/1000/logs", &drift_log())
        .with_log("jobs/1001/logs", &drift_log());
    let gh = |args: &[String]| fake.read(args);
    let report = record(&gh, &query(dir.path())).expect("selective rerun");
    assert_eq!((report.causes_written, report.fixlinks_written), (2, 1));
    assert_eq!(
        rows(&dir.path().join("fixlinks.jsonl"))[0]["derivation"],
        "rerun_green"
    );
}

#[test]
fn an_earlier_success_cannot_prove_a_later_failed_attempt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");

    let mut earlier_success = job(2000, "success");
    earlier_success["run_id"] = json!(200);
    earlier_success["head_sha"] = json!("fedcba9876543210fedcba9876543210fedcba98");
    earlier_success["run_attempt"] = json!(1);
    let mut latest_failure = earlier_success.clone();
    latest_failure["id"] = json!(2001);
    latest_failure["conclusion"] = json!("failure");
    latest_failure["run_attempt"] = json!(2);
    let mut routes = red_routes();
    routes.retain(|(needle, _)| {
        !needle.starts_with("runs?branch=") && !needle.contains("runs/200/jobs")
    });
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [{
                "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        (
            "runs/200/jobs",
            json!({"jobs": [earlier_success, latest_failure]}),
        ),
    ]);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("candidate pass");
    assert_eq!((report.fixlinks_written, report.reds_pending), (0, 1));
    assert!(!dir.path().join("fixlinks.jsonl").exists());
}

#[test]
fn a_green_workflow_run_that_skips_the_failed_job_is_not_a_fix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");

    let skipped = job(2000, "skipped");
    let mut routes = red_routes();
    routes.retain(|(needle, _)| {
        !needle.starts_with("runs?branch=") && !needle.contains("runs/200/jobs")
    });
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [{
                "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        ("runs/200/jobs", json!({"jobs": [skipped]})),
    ]);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("green pass");
    assert_eq!((report.fixlinks_written, report.reds_pending), (0, 1));
    assert!(!dir.path().join("fixlinks.jsonl").exists());
}

#[test]
fn a_zero_attempt_success_cannot_prove_a_fix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");

    let mut malformed = job(2000, "success");
    malformed["run_attempt"] = json!(0);
    let mut routes = red_routes();
    routes.retain(|(needle, _)| {
        !needle.starts_with("runs?branch=") && !needle.contains("runs/200/jobs")
    });
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [{
                "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        ("runs/200/jobs", json!({"jobs": [malformed]})),
    ]);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("malformed green pass");
    assert_eq!((report.fixlinks_written, report.reds_pending), (0, 1));
    assert!(!dir.path().join("fixlinks.jsonl").exists());
}

#[test]
fn a_later_green_run_is_used_when_the_earliest_candidate_skips_the_failed_job() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");

    let mut skipped = job(2000, "skipped");
    skipped["run_id"] = json!(200);
    skipped["head_sha"] = json!("fedcba9876543210fedcba9876543210fedcba98");
    let mut passed = job(2010, "success");
    passed["run_id"] = json!(201);
    passed["head_sha"] = json!("fedcba9876543210fedcba9876543210fedcba98");
    let mut routes = red_routes();
    routes.retain(|(needle, _)| {
        !needle.starts_with("runs?branch=") && !needle.contains("runs/200/jobs")
    });
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [
                {"id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98", "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1},
                {"id": 201, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98", "created_at": "2026-10-14T12:00:00Z", "run_attempt": 1}
            ]}),
        ),
        ("runs/200/jobs", json!({"jobs": [skipped]})),
        ("runs/201/jobs", json!({"jobs": [passed]})),
        (
            "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98",
            json!({"status": "ahead", "total_commits": 0, "files": [], "commits": []}),
        ),
    ]);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("green pass");
    assert_eq!((report.fixlinks_written, report.reds_pending), (1, 0));
    assert_eq!(
        rows(&dir.path().join("fixlinks.jsonl"))[0]["fix_run_id"],
        201
    );
}

#[test]
fn an_unreadable_earliest_candidate_keeps_the_red_pending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");

    let mut routes = red_routes();
    routes.retain(|(needle, _)| {
        !needle.starts_with("runs?branch=") && !needle.contains("runs/200/jobs")
    });
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [
                {"id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98", "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1},
                {"id": 201, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98", "created_at": "2026-10-14T12:00:00Z", "run_attempt": 1}
            ]}),
        ),
        ("runs/200/jobs", json!({"jobs": [job(2000, "success")] })),
        ("runs/201/jobs", json!({"jobs": [job(2010, "success")] })),
    ]);
    let second = Fake::new(&routes)
        .with_error("runs/200/jobs", "HTTP 500: unavailable")
        .with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("unreadable candidate");
    assert_eq!((report.fixlinks_written, report.reds_pending), (0, 1));
    assert!(!dir.path().join("fixlinks.jsonl").exists());
}

#[test]
fn an_unreadable_failed_sibling_keeps_the_red_pending_for_fixing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut red_jobs = vec![job(1000, "failure"), job(1001, "failure")];
    red_jobs[1]["name"] = json!("linux");
    let mut routes = red_routes();
    routes[1] = ("runs/100/jobs", json!({"jobs": red_jobs.clone()}));
    let first = Fake::new(&routes)
        .with_log("jobs/1000/logs", &drift_log())
        .with_error("jobs/1001/logs", "HTTP 500: unavailable");
    let gh = |args: &[String]| first.read(args);
    let first_report = record(&gh, &query(dir.path())).expect("red pass");
    assert_eq!(first_report.causes_written, 1);

    let mut routes = red_routes();
    routes[1] = ("runs/100/jobs", json!({"jobs": red_jobs.clone()}));
    routes.retain(|(needle, _)| {
        !needle.starts_with("runs?branch=") && !needle.contains("runs/200/jobs")
    });
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [{
                "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        ("runs/200/jobs", json!({"jobs": [job(2000, "success")]})),
    ]);
    let second = Fake::new(&routes)
        .with_log("jobs/1000/logs", &drift_log())
        .with_error("jobs/1001/logs", "HTTP 500: unavailable");
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("incomplete pass");
    assert_eq!((report.fixlinks_written, report.reds_pending), (0, 1));
    assert!(!dir.path().join("fixlinks.jsonl").exists());
}

#[test]
fn a_green_run_must_pass_every_failed_job_before_writing_a_fix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut red_jobs = vec![job(1000, "failure"), job(1001, "failure")];
    red_jobs[1]["name"] = json!("linux");
    let mut routes = red_routes();
    routes[1] = ("runs/100/jobs", json!({"jobs": red_jobs.clone()}));
    let first = Fake::new(&routes)
        .with_log("jobs/1000/logs", &drift_log())
        .with_log("jobs/1001/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    let red_report = record(&gh, &query(dir.path())).expect("red pass");
    assert_eq!(red_report.causes_written, 2);

    let mut routes = red_routes();
    routes[1] = ("runs/100/jobs", json!({"jobs": red_jobs}));
    routes.retain(|(needle, _)| {
        !needle.starts_with("runs?branch=") && !needle.contains("runs/200/jobs")
    });
    routes.extend([
        (
            "runs?branch=feat%2Fx&status=success",
            json!({"workflow_runs": [{
                "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
                "created_at": "2026-10-14T11:00:00Z", "run_attempt": 1
            }]}),
        ),
        ("runs/200/jobs", json!({"jobs": [job(2000, "success")] })),
    ]);
    let second = Fake::new(&routes)
        .with_log("jobs/1000/logs", &drift_log())
        .with_log("jobs/1001/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("green pass");
    assert_eq!((report.fixlinks_written, report.reds_pending), (0, 1));
    assert!(!dir.path().join("fixlinks.jsonl").exists());
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

#[test]
fn scrub_redacts_compiler_temp_paths_without_removing_the_flag() {
    let row = scrub(json!({
        "detail": " objective-c++ -ivfsstatcache /var/folders/hy/6y23234/T/cache",
        "private": "/private/var/folders/zz/0123456789abcdef0123456789abcdef01234567/T/cache"
    }));

    assert_eq!(row["detail"], " objective-c++ -ivfsstatcache <path>");
    assert_eq!(row["private"], "<path>");
}

/// One red run (`red`, with its job 1000 failing on the drift log), then a
/// later pass in which `fix_runs` are the green runs on `feat/x` and `extra`
/// answers the rest. Returns the second pass's report, the fix-links, and
/// every path the second pass asked.
fn second_pass(
    red: &Value,
    fix_runs: Value,
    extra: Vec<(&'static str, Value)>,
    now: &str,
) -> (RecordReport, Vec<Value>, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut routes = red_routes();
    // A job runs on its run's head: a merge-queue job on the queue's commit.
    let mut cancelled = job(1001, "cancelled");
    cancelled["name"] = json!("linux");
    let jobs: Vec<Value> = [job(1000, "failure"), cancelled]
        .into_iter()
        .map(|mut job| {
            job["head_sha"] = red["head_sha"].clone();
            job["head_branch"] = red["head_branch"].clone();
            job
        })
        .collect();
    routes[1] = ("runs/100/jobs", json!({"jobs": jobs}));
    routes[0] = ("runs?status=completed", json!({"workflow_runs": [red]}));
    let first = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(dir.path())).expect("red pass");
    routes.retain(|(needle, _)| !needle.starts_with("runs?branch="));
    routes.push(("runs?branch=feat%2Fx&status=success", fix_runs));
    routes.push(("runs/200/jobs", json!({"jobs": [job(2000, "success")]})));
    routes.extend(extra);
    let second = Fake::new(&routes).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let mut later = query(dir.path());
    now.clone_into(&mut later.now);
    let report = record(&gh, &later).expect("second pass");
    (
        report,
        rows(&dir.path().join("fixlinks.jsonl")),
        second.asked(),
    )
}

fn green(created: &str) -> Value {
    json!({"workflow_runs": [{
        "id": 200, "conclusion": "success", "head_sha": "fedcba9876543210fedcba9876543210fedcba98",
        "created_at": created, "run_attempt": 1
    }]})
}

#[test]
fn a_merge_queue_red_is_walked_back_to_the_pull_request_head_it_queued() {
    let mut red = red_run();
    red["head_sha"] = json!("1111111111111111111111111111111111111111");
    red["head_branch"] = json!("gh-readonly-queue/main/pr-42-base0");
    red["pull_requests"] = json!([]);
    red["event"] = json!("merge_group");
    let (report, links, asked) = second_pass(
        &red,
        green("2026-10-14T11:00:00Z"),
        vec![
            ("pulls/42", json!({"number": 42, "head": {"ref": "feat/x"}})),
            (
                "compare/1111111111111111111111111111111111111111...fedcba9876543210fedcba9876543210fedcba98",
                json!({"status": "diverged", "files": [{"filename": "a.cpp"}]}),
            ),
            (
                "commits/1111111111111111111111111111111111111111",
                json!({"parents": [{"sha": "base0"}, {"sha": "2222222222222222222222222222222222222222"}]}),
            ),
            (
                "compare/2222222222222222222222222222222222222222...fedcba9876543210fedcba9876543210fedcba98",
                json!({"status": "ahead", "total_commits": 2, "commits": [
                    {"sha": "c1", "parents": [{"sha": "2222222222222222222222222222222222222222"}]},
                    {"sha": "fedcba9876543210fedcba9876543210fedcba98", "parents": [{"sha": "c1"}]}
                ]}),
            ),
            ("commits/c1", json!({"files": [{"filename": "a.cpp"}]})),
            (
                "commits/fedcba9876543210fedcba9876543210fedcba98",
                json!({"files": [{"filename": "b.cpp"}]}),
            ),
        ],
        "2026-10-14T12:00:00Z",
    );
    let link = links
        .first()
        .unwrap_or_else(|| panic!("{report:?} {asked:?}"));
    assert_eq!(link["own_files_method"], "merge_queue_pr_head");
    assert_eq!(link["own_files"], json!(["a.cpp", "b.cpp"]));
    assert_eq!(link["pr"], 42);
}

fn force_pushed(commits: Value, fix_created: &str) -> (Value, Vec<String>) {
    let (_, links, asked) = second_pass(
        &red_run(),
        green(fix_created),
        vec![
            (
                "compare/0123456789abcdef0123456789abcdef01234567...fedcba9876543210fedcba9876543210fedcba98",
                json!({"status": "diverged", "files": [{"filename": "y.cpp"}]}),
            ),
            ("pulls/42/commits?per_page=100&page=1", commits),
            ("commits/p1", json!({"files": [{"filename": "x.cpp"}]})),
            (
                "commits/fedcba9876543210fedcba9876543210fedcba98",
                json!({"files": [{"filename": "y.cpp"}]}),
            ),
            ("commits/after", json!({"files": [{"filename": "z.cpp"}]})),
        ],
        "2026-10-20T12:00:00Z",
    );
    (links[0].clone(), asked)
}

#[test]
fn a_force_pushed_red_takes_the_pull_requests_commits_up_to_the_fix() {
    let (link, asked) = force_pushed(
        json!([
            {"sha": "p1", "parents": [{"sha": "base"}]},
            {"sha": "merge", "parents": [{"sha": "p1"}, {"sha": "main9"}]},
            {"sha": "fedcba9876543210fedcba9876543210fedcba98", "parents": [{"sha": "merge"}]},
            {"sha": "after", "parents": [{"sha": "fedcba9876543210fedcba9876543210fedcba98"}]}
        ]),
        "2026-10-14T11:00:00Z",
    );
    assert_eq!(link["own_files_method"], "post_force_push");
    assert_eq!(link["own_files"], json!(["x.cpp", "y.cpp"]));
    assert!(
        asked
            .iter()
            .all(|path| !path.ends_with("commits/after") && !path.ends_with("commits/merge")),
        "{asked:?}"
    );
}

#[test]
fn a_force_pushed_red_is_not_matched_after_72_hours_or_once_the_fix_is_gone() {
    let commits =
        json!([{"sha": "fedcba9876543210fedcba9876543210fedcba98", "parents": [{"sha": "base"}]}]);
    let (late, _) = force_pushed(commits.clone(), "2026-10-17T11:00:00Z");
    assert_eq!(late["own_files_method"], "post_force_push_too_late");
    assert_eq!(late["own_files"], Value::Null);
    let (gone, _) = force_pushed(
        json!([{"sha": "other", "parents": [{"sha": "base"}]}]),
        "2026-10-14T11:00:00Z",
    );
    assert_eq!(gone["own_files_method"], "post_force_push_superseded");
    assert_eq!(gone["own_files"], Value::Null);
}

#[test]
fn a_full_force_push_commit_listing_is_reported_as_truncated() {
    let full_page = Value::Array(
        (0..100)
            .map(|id| json!({"sha": format!("c{id}")}))
            .collect(),
    );
    let fake = Fake::new(&[("pulls/42/commits", full_page)]);
    let gh = |args: &[String]| fake.read(args);
    let red = Red {
        repo: "Generous-Corp/pulp".to_owned(),
        run_id: 100,
        attempt: 1,
        jobs: BTreeSet::from(["macos".to_owned()]),
        head_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        time: "2026-10-14T10:30:00Z".to_owned(),
        pr: Some(42),
        branch: "feat/x".to_owned(),
    };
    let fix = Fix {
        head_sha: "fedcba9876543210fedcba9876543210fedcba98".to_owned(),
        run_id: 200,
        pushed_at: "2026-10-14T11:00:00Z".to_owned(),
        derivation: "compare_red_head_to_fix_head",
    };
    let (own, method, truncated) = post_force_push(
        &gh,
        &query(tempfile::tempdir().expect("tempdir").path()),
        &red,
        &fix,
    )
    .expect("bounded force-push read");
    assert_eq!(
        (own, method, truncated),
        (None, "post_force_push_truncated", None)
    );
}

#[test]
fn pending_reds_are_counted_by_how_long_ago_they_went_red() {
    for (now, expected) in [
        ("2026-10-14T12:00:00Z", (1, 0, 0)),
        ("2026-10-16T09:00:00Z", (0, 1, 0)),
        ("2026-10-18T10:31:00Z", (0, 0, 1)),
    ] {
        let (report, links, _) = second_pass(&red_run(), json!({"workflow_runs": []}), vec![], now);
        assert!(links.is_empty());
        assert_eq!(report.reds_pending, 1, "{now}");
        assert_eq!(
            (
                report.pending_under_24h,
                report.pending_24h_to_72h,
                report.pending_over_72h
            ),
            expected,
            "{now}"
        );
    }
}

#[test]
fn malformed_archived_red_head_is_not_pending_for_fix_linking() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("causes")).expect("causes");
    std::fs::write(
        dir.path().join("causes/2026-10.jsonl"),
        json!({
            "failure_class": "build",
            "repo": "Generous-Corp/pulp",
            "run_id": 100,
            "attempt": 1,
            "head_sha": "short",
            "completed_at": "2026-10-14T10:30:00Z"
        })
        .to_string()
            + "\n",
    )
    .expect("archive");

    assert!(pending_reds(dir.path(), "2026-10-13T00:00:00Z", &BTreeSet::new()).is_empty());
}

#[test]
fn malformed_green_head_is_not_emitted_as_a_fix_link() {
    let red = Red {
        repo: "Generous-Corp/pulp".to_owned(),
        run_id: 100,
        attempt: 1,
        jobs: ["macos".to_owned()].into_iter().collect(),
        head_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        time: "2026-10-14T10:30:00Z".to_owned(),
        pr: Some(42),
        branch: "feat/x".to_owned(),
    };
    let malformed = json!({
        "id": 200,
        "conclusion": "success",
        "head_sha": "short",
        "created_at": "2026-10-14T11:00:00Z",
        "run_attempt": 1
    });
    let later = vec![&malformed];

    assert!(fixes_for(&red, &later, &BTreeMap::new()).is_empty());
}

#[test]
fn a_record_the_job_printed_names_the_cause_earlier_than_the_pass() {
    // The job ran `cause extract --notice` in a step after its failing one;
    // that step's completion, by GitHub's clock, is when the cause was named.
    let mut payload = job(1000, "failure");
    payload["steps"] = json!([
        {"name": "Run ctest", "conclusion": "failure", "number": 5,
         "started_at": "2026-10-14T10:10:00Z", "completed_at": "2026-10-14T10:30:00Z"},
        {"name": "Name the cause", "conclusion": "success", "number": 6,
         "started_at": "2026-10-14T10:30:05Z", "completed_at": "2026-10-14T10:30:20Z"}
    ]);
    let parsed: Job = serde_json::from_value(payload.clone()).expect("job");
    let place = Place {
        repo: "Generous-Corp/pulp".to_owned(),
        ..Place::default()
    };
    let printed = cause::build(&parsed, Some(&drift_log()), &place, None, false).expect("cause");
    let log = format!(
        "{}2026-10-14T10:30:10.0000000Z ##[notice]{}\n",
        drift_log(),
        serde_json::to_string(&printed).expect("record")
    );
    let mut routes = red_routes();
    routes[1] = ("runs/100/jobs", json!({"jobs": [payload]}));
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = Fake::new(&routes).with_log("jobs/1000/logs", &log);
    let gh = |args: &[String]| fake.read(args);
    record(&gh, &query(dir.path())).expect("pass");
    let row = &rows(&dir.path().join("causes/2026-10.jsonl"))[0];
    assert_eq!(row["format"], "generated-drift");
    assert_eq!(row["named_at"], "2026-10-14T10:30:20Z");
    assert_eq!(row["retrospective"], false);
    assert_eq!(row["completed_at"], "2026-10-14T10:30:00Z");
}
