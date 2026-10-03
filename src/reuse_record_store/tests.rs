//! A record is filed only with a parsing `job.json`, the store keeps the
//! newest, and the base a plan compares against must have a known, equal
//! toolchain and platform and a merged commit.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use chrono::{TimeZone, Utc};
use serde_json::{Value, json};

use super::*;

fn now() -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000, 0)
        .single()
        .expect("timestamp")
}

fn record(store: &Path, commit: &str, job: &Value, age_secs: u64) -> PathBuf {
    let pending = create_pending(store, commit, now()).expect("pending");
    fs::write(
        pending.join(JOB_FILE),
        serde_json::to_vec(job).expect("json"),
    )
    .expect("write job");
    let Filed::Kept(path) = file(store, &pending, commit).expect("file") else {
        panic!("expected a kept record");
    };
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 - age_secs);
    fs::File::open(&path)
        .and_then(|f| f.set_modified(mtime))
        .expect("set mtime");
    path
}

fn identity(job: &Value) -> (Option<String>, Option<String>) {
    let get = |key: &str| job.get(key).and_then(Value::as_str).map(str::to_owned);
    (get("toolchain"), get("platform"))
}

#[test]
fn a_run_without_a_parsing_job_file_is_discarded() {
    let store = tempfile::tempdir().expect("store");
    for (contents, reason) in [
        (None, "no job.json"),
        (Some(""), "empty"),
        (Some("{not json"), "does not parse"),
    ] {
        let pending = create_pending(store.path(), "aaa", now()).expect("pending");
        if let Some(contents) = contents {
            fs::write(pending.join(JOB_FILE), contents).expect("write");
        }
        match file(store.path(), &pending, "aaa").expect("file") {
            Filed::Discarded(why) => assert!(why.contains(reason), "{why}"),
            Filed::Kept(path) => panic!("kept {}", path.display()),
        }
        assert!(!pending.exists(), "the pending directory is removed");
    }
    assert!(list(store.path()).is_empty());
}

#[cfg(unix)]
#[test]
fn a_pending_directory_is_private_to_the_runner() {
    use std::os::unix::fs::PermissionsExt;
    let store = tempfile::tempdir().expect("store");
    let pending = create_pending(store.path(), "aaa", now()).expect("pending");
    let mode = fs::metadata(&pending).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}

#[test]
fn the_store_keeps_only_the_newest_records() {
    let store = tempfile::tempdir().expect("store");
    let job = json!({"toolchain": "t", "platform": "p"});
    for age in 0..5_u64 {
        record(store.path(), &format!("commit{age}"), &job, age * 10);
    }
    prune(store.path(), 2).expect("prune");
    let kept: Vec<String> = list(store.path()).into_iter().map(|r| r.commit).collect();
    assert_eq!(kept, vec!["commit0", "commit1"], "newest two survive");
    assert!(
        !store.path().join("records").join("commit4").exists(),
        "an emptied commit directory is removed"
    );
}

#[test]
fn the_base_must_have_a_known_equal_identity_and_a_merged_commit() {
    let store = tempfile::tempdir().expect("store");
    let want = ("clang-1 sdk-27", "macos-arm64");
    record(
        store.path(),
        "unknown",
        &json!({"toolchain": "clang-1 sdk-27"}),
        0,
    );
    record(
        store.path(),
        "other",
        &json!({"toolchain": "clang-2 sdk-27", "platform": "macos-arm64"}),
        10,
    );
    record(
        store.path(),
        "unmerged",
        &json!({"toolchain": want.0, "platform": want.1}),
        20,
    );
    record(
        store.path(),
        "good-new",
        &json!({"toolchain": want.0, "platform": want.1}),
        30,
    );
    record(
        store.path(),
        "good-old",
        &json!({"toolchain": want.0, "platform": want.1}),
        40,
    );

    let chosen = select_base(store.path(), want, identity, |commit| {
        commit.starts_with("good")
    })
    .expect("a base");
    assert_eq!(chosen.commit, "good-new", "the newest qualifying record");

    let none = select_base(store.path(), want, identity, |_| false).expect_err("none merged");
    assert_eq!(
        none,
        NoBase::NoneQualify {
            unknown_identity: 1,
            other_identity: 1,
            not_merged: 3
        }
    );
}

#[test]
fn an_empty_store_says_so() {
    let store = tempfile::tempdir().expect("store");
    assert_eq!(
        select_base(store.path(), ("t", "p"), identity, |_| true),
        Err(NoBase::Empty)
    );
}

#[test]
fn a_repository_slug_is_one_directory() {
    let dir = store_dir(Path::new("/state"), "owner/repo");
    assert_eq!(dir, Path::new("/state/reuse-records/owner__repo"));
}
