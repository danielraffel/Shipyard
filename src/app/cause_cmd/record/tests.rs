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
    read_jsonl_if_present(path).expect("valid JSONL")
}

#[test]
fn malformed_archive_refuses_before_github_or_new_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let causes = dir.path().join("causes");
    std::fs::create_dir_all(&causes).expect("causes dir");
    let archive = causes.join("2026-10.jsonl");
    std::fs::write(
        &archive,
        "{\"repo\":\"owner/repo\",\"job_id\":1}\n{not-json}\n",
    )
    .expect("archive");
    let fake = Fake::new(&[]);
    let gh = |args: &[String]| fake.read(args);

    let error = record(&gh, &query(dir.path())).expect_err("malformed archive must fail closed");
    assert!(
        error.contains(&format!("{} line 2", archive.display())),
        "{error}"
    );
    assert!(
        fake.asked().is_empty(),
        "archive errors must precede API writes"
    );
    assert!(!dir.path().join("fixlinks.jsonl").exists());
}

#[test]
fn invalid_cause_archive_shape_refuses_before_github_or_new_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = dir.path().join("causes/2026-10.jsonl");
    std::fs::create_dir_all(archive.parent().expect("archive parent")).expect("causes dir");
    std::fs::write(
        &archive,
        "{\"schema\":\"ci.cause/v1\",\"repo\":\"owner/repo\",\"job_id\":\"one\"}\n",
    )
    .expect("archive");
    let fake = Fake::new(&red_routes());
    let gh = |args: &[String]| fake.read(args);

    let error = record(&gh, &query(dir.path())).expect_err("invalid archive must fail closed");
    assert!(error.contains("job_id"), "{error}");
    assert!(
        fake.asked().is_empty(),
        "archive errors must precede API writes"
    );
    assert!(!dir.path().join("fixlinks.jsonl").exists());
}

#[test]
fn invalid_cause_archive_schema_refuses_before_github_or_new_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = dir.path().join("causes/2026-10.jsonl");
    std::fs::create_dir_all(archive.parent().expect("archive parent")).expect("causes dir");
    std::fs::write(
        &archive,
        r#"{"schema":"other/v1","repo":"owner/repo","job_id":1}"#,
    )
    .expect("archive");
    let fake = Fake::new(&[]);
    let gh = |args: &[String]| fake.read(args);

    let error = record(&gh, &query(dir.path())).expect_err("invalid schema must fail closed");
    assert!(error.contains("schema ci.cause/v1"), "{error}");
    assert!(
        fake.asked().is_empty(),
        "archive errors must precede API writes"
    );
    assert!(!dir.path().join("fixlinks.jsonl").exists());
}

#[test]
fn unreadable_archive_refuses_before_github_or_new_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = dir.path().join("causes/2026-10.jsonl");
    std::fs::create_dir_all(&archive).expect("archive directory");
    let fake = Fake::new(&[]);
    let gh = |args: &[String]| fake.read(args);

    let error = record(&gh, &query(dir.path())).expect_err("unreadable archive must fail closed");
    // Windows and Unix format temporary paths differently (and Windows may
    // report a directory read as `Access is denied` instead of `Is a
    // directory`). Keep the assertion tied to the archive entry while making
    // the diagnostic check portable across both platforms.
    assert!(error.contains("2026-10.jsonl"), "{error}");
    assert!(
        fake.asked().is_empty(),
        "archive errors must precede API writes"
    );
    assert!(!dir.path().join("fixlinks.jsonl").exists());
}

#[test]
fn malformed_fixlink_archive_refuses_before_github_or_new_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixlinks = dir.path().join("fixlinks.jsonl");
    std::fs::write(&fixlinks, "{not-json}\n").expect("fix-links archive");
    let fake = Fake::new(&red_routes());
    let gh = |args: &[String]| fake.read(args);

    let error = record(&gh, &query(dir.path())).expect_err("malformed fix-links must fail closed");
    assert!(
        error.contains(&format!("{} line 1", fixlinks.display())),
        "{error}"
    );
    assert!(
        fake.asked().is_empty(),
        "archive errors must precede API writes"
    );
    assert!(!dir.path().join("causes").exists());
}

#[test]
fn malformed_fixlink_shape_refuses_before_github_or_new_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixlinks = dir.path().join("fixlinks.jsonl");
    std::fs::write(
        &fixlinks,
        "{\"schema\":\"ci.cause/v1\",\"source\":\"fix-link\",\"repo\":\"owner/repo\"}\n",
    )
    .expect("fix-links archive");
    let fake = Fake::new(&red_routes());
    let gh = |args: &[String]| fake.read(args);

    let error = record(&gh, &query(dir.path())).expect_err("malformed fix-link must fail closed");
    assert!(error.contains("red_run_id"), "{error}");
    assert!(
        fake.asked().is_empty(),
        "archive errors must precede API writes"
    );
    assert!(!dir.path().join("causes").exists());
}

#[test]
fn invalid_fixlink_metadata_refuses_before_github_or_new_records() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixlinks = dir.path().join("fixlinks.jsonl");
    std::fs::write(
        &fixlinks,
        r#"{"schema":"other/v1","source":"other","repo":"owner/repo","red_run_id":1,"red_attempt":1}"#,
    )
    .expect("fix-links archive");
    let fake = Fake::new(&[]);
    let gh = |args: &[String]| fake.read(args);

    let error = record(&gh, &query(dir.path())).expect_err("invalid metadata must fail closed");
    assert!(error.contains("schema ci.cause/v1"), "{error}");
    assert!(
        fake.asked().is_empty(),
        "archive errors must precede API writes"
    );
    assert!(!dir.path().join("causes").exists());
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
fn archive_output_is_consumed_by_record_without_duplicate() {
    let source = tempfile::tempdir().expect("source archive");
    let first = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| first.read(args);
    record(&gh, &query(source.path())).expect("record source cause");
    let archived =
        std::fs::read_to_string(source.path().join("causes/2026-10.jsonl")).expect("source cause");

    let dir = tempfile::tempdir().expect("archive");
    super::super::archive(
        dir.path(),
        &archived.lines().map(str::to_owned).collect::<Vec<_>>(),
    )
    .expect("archive cause");

    let second = Fake::new(&red_routes()).with_log("jobs/1000/logs", &drift_log());
    let gh = |args: &[String]| second.read(args);
    let report = record(&gh, &query(dir.path())).expect("record archived cause");
    assert_eq!((report.causes_written, report.already_recorded), (0, 1));
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
            "fix_head_sha": "def456",
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
        {"sha": "c1", "parents": [{"sha": "abc123"}]},
        {"sha": "m1", "parents": [{"sha": "main0"}]},
        {"sha": "def456", "parents": [{"sha": "c1"}, {"sha": "m1"}]}
    ]);
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
        // Paged reads first: the unpaged needle is a prefix of theirs.
        (
            "compare/abc123...def456?per_page=100&page=1",
            json!({"status": "ahead", "total_commits": 3, "commits": chain}),
        ),
        (
            "compare/abc123...def456?per_page=100&page=2",
            json!({"status": "ahead", "total_commits": 3, "commits": []}),
        ),
        // Unpaged, the listing stops short of the range.
        (
            "compare/abc123...def456",
            json!({"status": "ahead", "total_commits": 3,
                   "files": [{"filename": "a.cpp"}, {"filename": "docs/main-only.md"}],
                   "commits": [{"sha": "c1", "parents": [{"sha": "abc123"}]}]}),
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

/// A red recorded, then a green push to `def456` whose compare answers with
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
            "id": 200, "conclusion": "success", "head_sha": "def456",
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
            "compare/abc123...def456?per_page=100",
            json!({"status": "ahead", "total_commits": 1001, "commits": hundred_commits()}),
        ),
        (
            "compare/abc123...def456",
            json!({"status": "ahead", "total_commits": 1001,
                   "files": [{"filename": "a.cpp"}], "commits": []}),
        ),
    ]);
    assert_eq!(link["own_files"], Value::Null);
    assert_eq!(link["own_files_method"], "too_many_commits");
    assert_eq!(link["files_raw"], json!(["a.cpp"]));
    let pages = asked
        .iter()
        .filter(|path| path.contains("compare/abc123...def456?per_page=100"))
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
            "compare/abc123...def456?per_page=100&page=1",
            json!({"status": "ahead", "total_commits": 300, "commits": hundred_commits()}),
        ),
        (
            "compare/abc123...def456?per_page=100",
            json!({"status": "ahead", "total_commits": 300, "commits": []}),
        ),
        (
            "compare/abc123...def456",
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

#[test]
fn scrub_redacts_compiler_temp_paths_without_removing_the_flag() {
    let row = scrub(json!({
        "detail": " objective-c++ -ivfsstatcache /var/folders/hy/6y23234/T/cache",
        "private": "/private/var/folders/zz/abc123/T/cache"
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
    let jobs: Vec<Value> = [job(1000, "failure"), job(1001, "cancelled")]
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
        "id": 200, "conclusion": "success", "head_sha": "def456",
        "created_at": created, "run_attempt": 1
    }]})
}

#[test]
fn a_merge_queue_red_is_walked_back_to_the_pull_request_head_it_queued() {
    let mut red = red_run();
    red["head_sha"] = json!("q1");
    red["head_branch"] = json!("gh-readonly-queue/main/pr-42-base0");
    red["pull_requests"] = json!([]);
    red["event"] = json!("merge_group");
    let (report, links, asked) = second_pass(
        &red,
        green("2026-10-14T11:00:00Z"),
        vec![
            ("pulls/42", json!({"number": 42, "head": {"ref": "feat/x"}})),
            (
                "compare/q1...def456",
                json!({"status": "diverged", "files": [{"filename": "a.cpp"}]}),
            ),
            (
                "commits/q1",
                json!({"parents": [{"sha": "base0"}, {"sha": "prhead"}]}),
            ),
            (
                "compare/prhead...def456",
                json!({"status": "ahead", "total_commits": 2, "commits": [
                    {"sha": "c1", "parents": [{"sha": "prhead"}]},
                    {"sha": "def456", "parents": [{"sha": "c1"}]}
                ]}),
            ),
            ("commits/c1", json!({"files": [{"filename": "a.cpp"}]})),
            ("commits/def456", json!({"files": [{"filename": "b.cpp"}]})),
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
                "compare/abc123...def456",
                json!({"status": "diverged", "files": [{"filename": "y.cpp"}]}),
            ),
            ("pulls/42/commits?per_page=100&page=1", commits),
            ("commits/p1", json!({"files": [{"filename": "x.cpp"}]})),
            ("commits/def456", json!({"files": [{"filename": "y.cpp"}]})),
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
            {"sha": "def456", "parents": [{"sha": "merge"}]},
            {"sha": "after", "parents": [{"sha": "def456"}]}
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
    let commits = json!([{"sha": "def456", "parents": [{"sha": "base"}]}]);
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

#[test]
fn legacy_root_causes_refuse_before_github_reads() {
    let dir = tempfile::tempdir().expect("archive");
    std::fs::create_dir(dir.path().join("causes")).expect("canonical directory");
    let legacy = dir.path().join("2026-10.jsonl");
    std::fs::write(&legacy, "legacy bytes\n").expect("legacy archive");
    let fake = Fake::new(&red_routes());
    let gh = |args: &[String]| fake.read(args);
    let error = record(&gh, &query(dir.path())).expect_err("legacy archive refused");
    assert!(error.contains("legacy cause archive files"), "{error}");
    assert!(fake.asked().is_empty());
    assert_eq!(std::fs::read_to_string(legacy).unwrap(), "legacy bytes\n");
    assert!(!dir.path().join("causes/2026-10.jsonl").exists());
}
