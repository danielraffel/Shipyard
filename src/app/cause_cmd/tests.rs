use std::collections::HashMap;
use std::path::PathBuf;

use super::*;

const NOW: &str = "2026-10-13T12:00:00Z";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}/tests/fixtures/c2/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
}

fn args(log: PathBuf) -> CauseExtractArgs {
    CauseExtractArgs {
        log,
        job_json: None,
        format: "record".to_owned(),
        notice: false,
        repo: None,
        run_id: None,
        attempt: None,
        job: None,
        head_sha: None,
        event: None,
        pr: None,
    }
}

fn runner_env() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("GITHUB_REPOSITORY", "Generous-Corp/pulp"),
        ("GITHUB_RUN_ID", "38308868279"),
        ("GITHUB_RUN_ATTEMPT", "2"),
        ("GITHUB_JOB", "macos"),
        ("GITHUB_SHA", "6cb6bad7"),
        ("GITHUB_EVENT_NAME", "merge_group"),
        ("RUNNER_NAME", "runner-1"),
    ])
}

fn run(args: &CauseExtractArgs) -> Result<String, CliFailure> {
    let env = runner_env();
    let lookup = |key: &str| env.get(key).map(|value| (*value).to_owned());
    extract(args, &lookup, NOW)
}

#[test]
fn a_record_takes_where_it_ran_from_the_runner() {
    let text = run(&args(fixture("ctest_summary.log"))).expect("extract");
    let record: CiCause = serde_json::from_str(&text).expect("record");
    assert_eq!(record.repo, "Generous-Corp/pulp");
    assert_eq!(record.run_id, Some(38_308_868_279));
    assert_eq!(record.attempt, Some(2));
    assert_eq!(record.job, "macos");
    // The jobs API id is unknown inside the job; the scorecard fills it.
    assert_eq!(record.job_id, None);
    assert_eq!(record.head_sha.as_deref(), Some("6cb6bad7"));
    assert_eq!(record.event.as_deref(), Some("merge_group"));
    assert_eq!(record.format, "ctest");
    assert_eq!(record.signature, "consumption-census-drift");
    assert_eq!(
        (record.failure_class.as_str(), record.confidence.as_str()),
        ("test", "high")
    );
    assert_eq!(record.named_at.as_deref(), Some(NOW));
    assert!(!record.retrospective);
}

#[test]
fn flags_override_the_environment() {
    let mut args = args(fixture("ctest_summary.log"));
    args.repo = Some("o/r".to_owned());
    args.job = Some("lint".to_owned());
    args.pr = Some(7);
    let record: CiCause = serde_json::from_str(&run(&args).expect("extract")).expect("record");
    assert_eq!((record.repo.as_str(), record.job.as_str()), ("o/r", "lint"));
    assert_eq!(record.pr, Some(7));
}

#[test]
fn an_assembled_job_is_read_from_its_output_not_judged_a_lost_runner() {
    // A job with no steps reads as a lost runner; the assembled job has one.
    let record: CiCause =
        serde_json::from_str(&run(&args(fixture("shell_exit.log"))).expect("extract"))
            .expect("record");
    assert_eq!(record.format, "shell-exit");
    assert_eq!(record.failed_step.as_deref(), Some(CAPTURED_STEP));
}

#[test]
fn the_notice_puts_the_record_in_the_log_and_it_reads_back() {
    let mut args = args(fixture("gate_policy.log"));
    args.notice = true;
    let line = run(&args).expect("extract");
    let data = line
        .strip_prefix("::notice title=ci-cause::")
        .expect("a ci-cause notice");
    assert!(!data.contains('\n'));
    // The runner logs a notice as `##[notice]<data>`, unescaped.
    let logged = format!(
        "2026-10-13T12:00:01.0000000Z ##[notice]{}\n",
        data.replace("%0A", "\n")
            .replace("%0D", "\r")
            .replace("%25", "%")
    );
    let back = cause::records_in_log(&logged);
    assert_eq!(back.len(), 1);
    args.notice = false;
    let direct: CiCause = serde_json::from_str(&run(&args).expect("extract")).expect("record");
    assert_eq!(back[0], direct);
}

#[test]
fn a_percent_sign_is_escaped_in_the_notice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("log.txt");
    // A gate verdict whose line carries a `%`.
    std::fs::write(&log, "Skill-sync check FAILED: 100% coverage required\n").expect("log");
    let mut args = args(log);
    args.notice = true;
    let line = run(&args).expect("extract");
    assert!(line.contains("100%25 coverage"), "{line}");
    assert!(!line.contains("100% coverage"));
}

#[test]
fn c2_format_prints_the_raw_records() {
    let mut args = args(fixture("clang.log"));
    args.format = "c2".to_owned();
    let text = run(&args).expect("extract");
    let records: Vec<c2::Record> = serde_json::from_str(&text).expect("records");
    let log = std::fs::read_to_string(fixture("clang.log")).expect("log");
    assert_eq!(records, c2::extract(&log));
}

#[test]
fn a_bad_format_or_a_missing_log_is_refused() {
    let mut bad = args(fixture("clang.log"));
    bad.format = "yaml".to_owned();
    assert_eq!(run(&bad).expect_err("format").code, 2);
    let missing = args(PathBuf::from("/nonexistent/log.txt"));
    assert_eq!(run(&missing).expect_err("log").code, 2);
}

fn record(run_id: u64, named_at: &str) -> String {
    let mut args = args(fixture("ctest_summary.log"));
    args.run_id = Some(run_id);
    let mut record: CiCause = serde_json::from_str(&run(&args).expect("extract")).expect("record");
    record.named_at = Some(named_at.to_owned());
    serde_json::to_string(&record).expect("json")
}

#[test]
fn the_archive_appends_by_month_and_never_rewrites() {
    let dir = tempfile::tempdir().expect("tempdir");
    let october = record(1, "2026-10-13T12:00:00Z");
    let september = record(2, "2026-09-30T23:59:59Z");
    let report = archive(dir.path(), &[october.clone(), september.clone()]).expect("archive");
    assert_eq!(report.appended, 2);
    let first = std::fs::read(dir.path().join("causes/2026-10.jsonl")).expect("october");
    assert!(dir.path().join("causes/2026-09.jsonl").exists());

    // The same records again, a case row carrying a third, and noise.
    let case_row = serde_json::json!({
        "schema": "shipyard-gate-cost-case/v1",
        "cause": serde_json::from_str::<Value>(&record(3, "2026-10-14T00:00:00Z")).expect("record"),
    })
    .to_string();
    let lines = vec![
        october,
        september,
        case_row,
        "{\"cause\": null}".to_owned(),
        "not json".to_owned(),
        String::new(),
    ];
    let report = archive(dir.path(), &lines).expect("archive");
    assert_eq!(
        report,
        ArchiveReport {
            appended: 1,
            already_archived: 2,
            without_record: 2,
        }
    );
    let after = std::fs::read(dir.path().join("causes/2026-10.jsonl")).expect("october");
    assert!(after.starts_with(&first), "earlier lines were rewritten");
    let text = String::from_utf8(after).expect("utf8");
    assert_eq!(text.lines().count(), 2);
}

#[test]
fn an_undated_record_goes_to_its_own_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut value: Value = serde_json::from_str(&record(4, "x")).expect("record");
    value["named_at"] = Value::Null;
    archive(dir.path(), &[value.to_string()]).expect("archive");
    assert!(dir.path().join("causes/undated.jsonl").exists());
}

#[test]
fn a_legacy_root_archive_is_refused_instead_of_being_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("2026-10.jsonl"), "not json\n").expect("legacy archive");

    let error = archive(dir.path(), &[record(5, "2026-10-13T12:00:00Z")])
        .expect_err("legacy root archive must be migrated explicitly");
    assert!(error.contains("legacy cause archive files"), "{error}");
    assert!(!dir.path().join("causes/2026-10.jsonl").exists());
}

#[test]
fn a_pull_request_record_names_the_head_not_the_merge_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let event = dir.path().join("event.json");
    std::fs::write(&event, r#"{"pull_request": {"head": {"sha": "0123abcd"}}}"#).expect("event");
    let env = runner_env();
    let path = event.to_string_lossy().into_owned();
    let lookup = |key: &str| {
        if key == "GITHUB_EVENT_PATH" {
            return Some(path.clone());
        }
        env.get(key).map(|value| (*value).to_owned())
    };
    let text = extract(&args(fixture("ctest_summary.log")), &lookup, NOW).expect("extract");
    let record: CiCause = serde_json::from_str(&text).expect("record");
    assert_eq!(record.head_sha.as_deref(), Some("0123abcd"));
}

#[test]
fn two_archivers_at_once_lose_and_duplicate_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let records: Vec<String> = (1..=40)
        .map(|run| record(run, "2026-10-13T12:00:00Z"))
        .collect();
    // Each writer holds 30 records; 20 are in both.
    let first: Vec<String> = records[..30].to_vec();
    let second: Vec<String> = records[10..].to_vec();
    let path = dir.path().to_path_buf();
    let reports: Vec<ArchiveReport> = std::thread::scope(|scope| {
        let a = scope.spawn(|| archive(&path, &first).expect("archive"));
        let b = scope.spawn(|| archive(&path, &second).expect("archive"));
        vec![a.join().expect("a"), b.join().expect("b")]
    });
    assert_eq!(reports.iter().map(|r| r.appended).sum::<usize>(), 40);
    assert_eq!(
        reports.iter().map(|r| r.already_archived).sum::<usize>(),
        20
    );
    let text = std::fs::read_to_string(dir.path().join("causes/2026-10.jsonl")).expect("archive");
    let ids: Vec<String> = text
        .lines()
        .map(|line| {
            serde_json::from_str::<CiCause>(line)
                .expect("whole line")
                .id()
        })
        .collect();
    let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
    assert_eq!(ids.len(), 40);
    assert_eq!(unique.len(), 40);
}

#[test]
fn auxiliary_jsonl_files_do_not_prevent_canonical_archiving() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fixlinks.jsonl"), "").unwrap();
    std::fs::write(dir.path().join("outages.jsonl"), "").unwrap();
    let report = archive(dir.path(), &[record(6, "2026-10-13T12:00:00Z")]).unwrap();
    assert_eq!(report.appended, 1);
}

#[test]
fn a_managed_archive_without_its_lock_fails_closed_before_reading() {
    let dir = tempfile::tempdir().expect("archive");
    std::fs::create_dir(dir.path().join("causes")).expect("causes directory");
    std::fs::write(
        dir.path().join(".shipyard-cause-archive.json"),
        br#"{"schema":"shipyard.cause-archive/v1","frozen":false}"#,
    )
    .expect("archive marker");

    let error = match lock_archive(dir.path(), false) {
        Ok(_) => panic!("missing managed lock must fail closed"),
        Err(error) => error,
    };

    assert!(error.contains("managed archive lacks its lock"), "{error}");
    assert!(!dir.path().join(".lock").exists());
}

#[test]
fn a_frozen_archive_is_readable_without_creating_a_lock() {
    let dir = tempfile::tempdir().expect("archive");
    std::fs::create_dir(dir.path().join("causes")).expect("causes directory");
    std::fs::write(
        dir.path().join(".shipyard-cause-archive.json"),
        br#"{"schema":"shipyard.cause-archive/v1","frozen":true}"#,
    )
    .expect("archive marker");

    let _reader = lock_archive(dir.path(), false).expect("frozen reader");

    assert!(!dir.path().join(".lock").exists());
}

#[cfg(unix)]
#[test]
fn archive_refuses_a_symlinked_causes_directory_without_writing_outside() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().expect("archive");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(
        dir.path().join(".shipyard-cause-archive.json"),
        br#"{"schema":"shipyard.cause-archive/v1","frozen":false}"#,
    )
    .expect("archive marker");
    symlink(outside.path(), dir.path().join("causes")).expect("causes symlink");

    let error = archive(dir.path(), &[record(99, "2026-10-13T12:00:00Z")]).unwrap_err();

    assert!(error.contains("real directory"), "{error}");
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
}
