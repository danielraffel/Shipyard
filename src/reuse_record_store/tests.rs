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
    set_dir_modified(&path, mtime);
    path
}

/// Set a directory's modification time. Windows only allows that through a
/// handle opened for `FILE_WRITE_ATTRIBUTES` with backup semantics (a plain
/// `File::open` of a directory is refused there with "Access is denied").
fn set_dir_modified(dir: &Path, mtime: SystemTime) {
    #[cfg(windows)]
    let handle = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
    };
    #[cfg(not(windows))]
    let handle = fs::File::open(dir);
    handle
        .and_then(|f| f.set_modified(mtime))
        .expect("set directory mtime");
}

/// Reads `platform`/`toolchain` from `job.json`; a record is unusable when
/// it says so; commits starting `good` are merged.
struct Criteria;

impl BaseCriteria for Criteria {
    fn platform(&self, job: &Value) -> Option<String> {
        job.get("platform")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
    fn toolchain(&self, job: &Value) -> Option<String> {
        job.get("toolchain")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
    fn unusable(&self, record: &StoredRecord) -> Option<String> {
        record
            .job
            .get("unusable")
            .map(|_| "marked unusable".to_owned())
    }
    fn merged(&self, commit: &str) -> bool {
        commit.starts_with("good")
    }
}

#[test]
fn a_run_without_a_parsing_job_file_is_discarded() {
    let store = tempfile::tempdir().expect("store");
    for (contents, reason) in [
        (None, "no job.json"),
        (Some(""), "empty"),
        (Some("{not json"), "does not parse"),
        (Some("42"), "not a JSON object"),
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
fn the_base_must_have_a_stated_equal_identity_a_usable_record_and_a_merged_commit() {
    let store = tempfile::tempdir().expect("store");
    let (platform, toolchain) = ("macos-arm64", "clang-1 sdk-27");
    let job = |p: Option<&str>, t: Option<&str>| {
        let mut job = serde_json::Map::new();
        if let Some(p) = p {
            job.insert("platform".to_owned(), json!(p));
        }
        if let Some(t) = t {
            job.insert("toolchain".to_owned(), json!(t));
        }
        Value::Object(job)
    };
    // Newest first: each of these is refused for one reason.
    record(store.path(), "good-a", &job(None, Some(toolchain)), 0);
    // Another OS with the same toolchain string: platform is read first.
    record(
        store.path(),
        "good-b",
        &job(Some("linux-x86_64"), Some(toolchain)),
        10,
    );
    record(
        store.path(),
        "good-c",
        &job(Some(platform), Some("unknown")),
        20,
    );
    record(
        store.path(),
        "good-d",
        &job(Some(platform), Some("clang-2 sdk-27")),
        30,
    );
    let mut unusable = job(Some(platform), Some(toolchain));
    unusable["unusable"] = json!(true);
    record(store.path(), "good-e", &unusable, 40);
    record(
        store.path(),
        "unmerged",
        &job(Some(platform), Some(toolchain)),
        50,
    );
    record(
        store.path(),
        "good-new",
        &job(Some(platform), Some(toolchain)),
        60,
    );
    record(
        store.path(),
        "good-old",
        &job(Some(platform), Some(toolchain)),
        70,
    );

    let chosen = select_base(store.path(), platform, toolchain, &Criteria).expect("a base");
    assert_eq!(chosen.commit, "good-new", "the newest qualifying record");

    let none = select_base(store.path(), platform, "clang-9", &Criteria).expect_err("none match");
    assert_eq!(
        none,
        NoBase::NoneQualify(Refusals {
            unknown_platform: 1,
            other_platform: 1,
            unknown_toolchain: 1,
            other_toolchain: 5,
            unusable: 0,
            not_merged: 0,
        })
    );
    let NoBase::NoneQualify(refused) =
        select_base(store.path(), platform, toolchain, &NoneMerged).expect_err("none merged")
    else {
        panic!("expected refusals");
    };
    assert_eq!((refused.unusable, refused.not_merged), (1, 3));
}

struct NoneMerged;

impl BaseCriteria for NoneMerged {
    fn platform(&self, job: &Value) -> Option<String> {
        Criteria.platform(job)
    }
    fn toolchain(&self, job: &Value) -> Option<String> {
        Criteria.toolchain(job)
    }
    fn unusable(&self, record: &StoredRecord) -> Option<String> {
        Criteria.unusable(record)
    }
    fn merged(&self, _: &str) -> bool {
        false
    }
}

#[test]
fn an_empty_store_says_so() {
    let store = tempfile::tempdir().expect("store");
    assert_eq!(
        select_base(store.path(), "p", "t", &Criteria),
        Err(NoBase::Empty)
    );
}

#[test]
fn a_cancelled_runs_pending_directory_is_swept_later() {
    let store = tempfile::tempdir().expect("store");
    let old = create_pending(store.path(), "aaa", now()).expect("pending");
    let day_ago = SystemTime::now() - (PENDING_MAX_AGE + Duration::from_secs(60));
    set_dir_modified(&old, day_ago);
    let fresh = create_pending(store.path(), "bbb", now()).expect("pending");
    assert!(
        !old.exists(),
        "a run that never reached filing is cleaned up"
    );
    assert!(fresh.exists(), "a run in progress is left alone");
}

#[test]
fn a_repository_slug_is_one_directory() {
    let dir = store_dir(Path::new("/state"), "owner/repo");
    assert_eq!(dir, Path::new("/state/reuse-records/owner__repo"));
}
