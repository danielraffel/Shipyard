//! `shipyard cause record`: a read-only observer that keeps a labels archive
//! current as checks go red and green.
//!
//! One pass lists the workflow's completed runs since `--since`, and for each
//! red job not yet in the archive writes one `ci.cause/v1` record (C2 names the
//! cause, exactly as `cause extract` does). Then, for every red run attempt the
//! archive holds without a fix-link, it looks for the earliest later green run
//! of the same workflow on the same pull request or branch and writes a
//! fix-link: the red run, the fixing commit, and the files it changed.
//!
//! The layout is the backfill's (`<dir>/causes/YYYY-MM.jsonl` by the job's
//! `completed_at`, `<dir>/fixlinks.jsonl`), with the same fields, so live and
//! backfilled rows join. Nothing here runs inside `ship`, `pr` or `run`, and
//! it never writes to GitHub: it reads runs, jobs, logs, pulls and compares.
//! A red with no green yet stays pending and is looked at again next pass.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::LazyLock;

use fs2::FileExt;
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::diagnose::Job;
use crate::diagnose::c2;
use crate::diagnose::cause::{self, Place};
use crate::gate_cost::SyncGhReader;

/// Who wrote a row.
pub(super) const COLLECTOR: &str = "shipyard cause record";
/// The compare API lists at most this many files.
const COMPARE_FILE_CAP: usize = 300;
/// Most pages of runs one pass lists.
const MAX_RUN_PAGES: usize = 10;

pub(super) const OWN_FILES_RULE: &str = "own_files = files_raw (compare red_head...fix_head) minus the \
    files changed on the base branch between merge-base(red_head, base) and merge-base(fix_head, base); \
    rerun_same_head: []; branch_is_default and merge_base_unavailable: null; own_files_truncated when \
    either compare reached 300 files";
const FIXLINK_RULE: &str = "earliest successful run of the same workflow on the red's pull request \
    branch (or branch) created after the red attempt's last failed job completed; a later attempt of \
    the red run itself is rerun_green with pushed_at = its run_started_at; files_raw = compare red \
    head...fix head";

/// One observer pass.
#[derive(Clone, Debug)]
pub(super) struct RecordQuery {
    /// `OWNER/REPO`.
    pub repo: String,
    /// Workflow file name, for example `build.yml`.
    pub workflow: String,
    /// The archive: `data/ci-cause` in a private labels checkout.
    pub dir: std::path::PathBuf,
    /// Earliest run creation time read, RFC 3339.
    pub since: String,
    /// The repository's default branch.
    pub base: String,
    /// Collection time written on each row.
    pub now: String,
}

/// What one pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct RecordReport {
    /// Completed runs listed.
    pub runs_listed: usize,
    /// Runs whose jobs were read (red, or green after a rerun).
    pub runs_read: usize,
    /// Cause records appended.
    pub causes_written: usize,
    /// Red jobs already in the archive.
    pub already_recorded: usize,
    /// Cancelled jobs, which feed outage windows rather than causes.
    pub cancelled_skipped: usize,
    /// Fix-links appended.
    pub fixlinks_written: usize,
    /// Red run attempts in the window still without a green.
    pub reds_pending: usize,
    /// Reads that failed, with why.
    pub unreadable: Vec<String>,
}

fn api(gh: &SyncGhReader<'_>, path: &str) -> Result<Value, String> {
    let text = gh(&["api".to_owned(), path.to_owned()])?;
    serde_json::from_str(&text).map_err(|error| format!("{path}: {error}"))
}

/// Whether a log read failed because GitHub holds no log for the job.
fn log_absent(error: &str) -> bool {
    ["HTTP 404", "HTTP 410", "BlobNotFound"]
        .iter()
        .any(|marker| error.contains(marker))
}

static SCRUB: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (
            r"/(?:Users|Volumes|private/tmp|tmp|home)/[^\s'\x22]*",
            "<path>",
        ),
        (r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b", "<email>"),
        (r"\b(?:\d{1,3}\.){3}\d{1,3}\b", "<ip>"),
        (
            r"(?i)(token|api[_-]?key|secret|password)(\s*[:=]\s*)[A-Za-z0-9_./+=-]{8,}",
            "$1$2<secret>",
        ),
        (r"(?i)tailscale", "<vpn>"),
        (
            r"(?i)\b[\w-]*(?:mac-?studio|macmini|macbook)[\w.-]*",
            "<host>",
        ),
        (r"(?i)\b(?:m[135]s?|macstudio|macmini)\b", "<host>"),
    ]
    .into_iter()
    .map(|(pattern, replacement)| {
        (
            Regex::new(pattern).expect("scrub pattern compiles"),
            replacement,
        )
    })
    .collect()
});

/// Local paths, addresses, credentials and private host names out of every
/// string; run and job URLs are kept.
pub(super) fn scrub(value: Value) -> Value {
    match value {
        Value::String(mut text) => {
            for (pattern, replacement) in SCRUB.iter() {
                text = pattern.replace_all(&text, *replacement).into_owned();
            }
            Value::String(text)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(scrub).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    if key == "run_url" || key == "job_url" {
                        (key, value)
                    } else {
                        (key, scrub(value))
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

/// The pull request a run belongs to, its branch, and the owner key that
/// keeps two pushes of one pull request together: `pr:N`, else
/// `branch:<name>`, else `sha:<head>`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Owner {
    pr: Option<u64>,
    branch: Option<String>,
    key: String,
    method: &'static str,
}

fn queue_pr(branch: &str) -> Option<u64> {
    let rest = branch.strip_prefix("gh-readonly-queue/")?;
    rest.split("/pr-").nth(1)?.split('-').next()?.parse().ok()
}

fn owner_of(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    run: &Value,
    pulls: &mut BTreeMap<String, Vec<Value>>,
) -> Owner {
    let branch = run["head_branch"].as_str().unwrap_or_default().to_owned();
    let some_branch = (!branch.is_empty()).then(|| branch.clone());
    if let Some(pr) = run["pull_requests"]
        .as_array()
        .and_then(|prs| prs.first())
        .and_then(|pr| pr["number"].as_u64())
    {
        return Owner {
            pr: Some(pr),
            branch: some_branch,
            key: format!("pr:{pr}"),
            method: "run_pull_requests",
        };
    }
    if let Some(pr) = queue_pr(&branch) {
        return Owner {
            pr: Some(pr),
            branch: some_branch,
            key: format!("pr:{pr}"),
            method: "merge_queue_branch",
        };
    }
    if !branch.is_empty() && branch != query.base {
        // GitHub empties a run's pull_requests once the pull request moves
        // on; the branch still names it.
        let org = query.repo.split('/').next().unwrap_or_default();
        let found = pulls.entry(branch.clone()).or_insert_with(|| {
            api(
                gh,
                &format!(
                    "repos/{}/pulls?state=all&per_page=30&head={org}:{}",
                    query.repo,
                    percent_encode(&branch)
                ),
            )
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
        });
        let created = run["created_at"].as_str().unwrap_or_default();
        let pr = found
            .iter()
            .filter(|pr| {
                pr["created_at"].as_str().unwrap_or_default() <= created
                    && pr["closed_at"]
                        .as_str()
                        .is_none_or(|closed| created <= closed)
            })
            .max_by_key(|pr| pr["created_at"].as_str().unwrap_or_default().to_owned())
            .and_then(|pr| pr["number"].as_u64());
        return match pr {
            Some(pr) => Owner {
                pr: Some(pr),
                branch: some_branch,
                key: format!("pr:{pr}"),
                method: "branch_pulls_api",
            },
            None => Owner {
                pr: None,
                key: format!("branch:{branch}"),
                branch: some_branch,
                method: "branch",
            },
        };
    }
    Owner {
        pr: None,
        branch: some_branch,
        key: format!("sha:{}", run["head_sha"].as_str().unwrap_or_default()),
        method: "head_sha",
    }
}

fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Every cause row in the archive.
fn archived_causes(dir: &Path) -> Vec<Value> {
    let mut rows = Vec::new();
    if let Ok(entries) = fs::read_dir(dir.join("causes")) {
        let mut paths: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        paths.sort();
        for path in paths
            .into_iter()
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        {
            rows.extend(read_jsonl(&path));
        }
    }
    rows
}

fn append(path: &Path, rows: &[Value]) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    for row in rows {
        let line = serde_json::to_string(row).map_err(|error| error.to_string())?;
        writeln!(file, "{line}").map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn list_runs(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    filter: &str,
) -> Result<Vec<Value>, String> {
    let mut runs = Vec::new();
    for page in 1..=MAX_RUN_PAGES {
        let data = api(
            gh,
            &format!(
                "repos/{}/actions/workflows/{}/runs?{filter}&per_page=100&page={page}",
                query.repo, query.workflow
            ),
        )?;
        let items = data["workflow_runs"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let full = items.len() == 100;
        runs.extend(items);
        if !full {
            break;
        }
    }
    Ok(runs)
}

/// Whether a run's jobs can hold a red: it failed, or a rerun turned it green
/// after an earlier attempt failed (GitHub lists that run as a success).
fn may_hold_red(run: &Value) -> bool {
    match run["conclusion"].as_str() {
        Some("failure" | "timed_out") => true,
        Some("success") => run["run_attempt"].as_u64().unwrap_or(1) > 1,
        _ => false,
    }
}

/// One pass: causes for new red jobs, then fix-links for reds now green.
pub(super) fn record(gh: &SyncGhReader<'_>, query: &RecordQuery) -> Result<RecordReport, String> {
    fs::create_dir_all(&query.dir).map_err(|error| error.to_string())?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(query.dir.join(".lock"))
        .map_err(|error| error.to_string())?;
    lock.lock_exclusive().map_err(|error| error.to_string())?;

    let mut report = RecordReport::default();
    let mut pulls = BTreeMap::new();
    let recorded: BTreeSet<(String, u64)> = archived_causes(&query.dir)
        .iter()
        .filter_map(|row| Some((row["repo"].as_str()?.to_owned(), row["job_id"].as_u64()?)))
        .collect();
    let runs = list_runs(
        gh,
        query,
        &format!("status=completed&created=%3E%3D{}", query.since),
    )?;
    report.runs_listed = runs.len();
    let mut by_month: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut run_index: BTreeMap<u64, Value> = BTreeMap::new();
    for run in runs.iter().filter(|run| may_hold_red(run)) {
        let Some(run_id) = run["id"].as_u64() else {
            continue;
        };
        run_index.insert(run_id, run.clone());
        report.runs_read += 1;
        let jobs = match api(
            gh,
            &format!(
                "repos/{}/actions/runs/{run_id}/jobs?filter=all&per_page=100",
                query.repo
            ),
        ) {
            Ok(jobs) => jobs["jobs"].as_array().cloned().unwrap_or_default(),
            Err(error) => {
                report.unreadable.push(format!("run {run_id}: {error}"));
                continue;
            }
        };
        let owner = owner_of(gh, query, run, &mut pulls);
        for value in jobs {
            let Ok(job) = serde_json::from_value::<Job>(value.clone()) else {
                continue;
            };
            match job.conclusion.as_deref() {
                Some("failure" | "timed_out") => {}
                Some("cancelled") => {
                    report.cancelled_skipped += 1;
                    continue;
                }
                _ => continue,
            }
            let Ok(job_id) = u64::try_from(job.id) else {
                continue;
            };
            if recorded.contains(&(query.repo.clone(), job_id)) {
                report.already_recorded += 1;
                continue;
            }
            match cause_row(gh, query, run, &job, &owner) {
                Ok(Some(row)) => {
                    let month = row["completed_at"]
                        .as_str()
                        .and_then(|at| at.get(..7))
                        .unwrap_or("undated")
                        .to_owned();
                    by_month.entry(month).or_default().push(row);
                    report.causes_written += 1;
                }
                Ok(None) => {}
                Err(error) => report.unreadable.push(error),
            }
        }
    }
    for (month, rows) in &by_month {
        append(
            &query.dir.join("causes").join(format!("{month}.jsonl")),
            rows,
        )?;
    }
    let fixlinks = fixlinks(gh, query, &run_index, &mut pulls, &mut report)?;
    append(&query.dir.join("fixlinks.jsonl"), &fixlinks)?;
    report.fixlinks_written = fixlinks.len();
    Ok(report)
}

/// The cause row of one failed job, or `None` when its payload says it did
/// not fail.
fn cause_row(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    run: &Value,
    job: &Job,
    owner: &Owner,
) -> Result<Option<Value>, String> {
    let log = match gh(&[
        "api".to_owned(),
        format!("repos/{}/actions/jobs/{}/logs", query.repo, job.id),
    ]) {
        Ok(text) => Some(text),
        Err(error) if log_absent(&error) => None,
        Err(error) => return Err(format!("log of job {}: {error}", job.id)),
    };
    let place = Place {
        repo: query.repo.clone(),
        event: run["event"].as_str().map(str::to_owned),
        pr: owner.pr,
        ..Place::default()
    };
    let Some(mut cause) = cause::build(job, log.as_deref(), &place, job.completed_at.clone(), true)
    else {
        return Ok(None);
    };
    cause::adopt_printed_time(&mut cause, log.as_deref(), job, &query.repo);
    let mut row = serde_json::to_value(&cause).map_err(|error| error.to_string())?;
    let fields = row.as_object_mut().ok_or("cause is not an object")?;
    fields.insert("completed_at".into(), json!(job.completed_at));
    fields.insert(
        "workflow".into(),
        json!(run["name"].as_str().unwrap_or(&query.workflow)),
    );
    fields.insert("head_branch".into(), json!(owner.branch));
    fields.insert("owner".into(), json!(owner.key));
    fields.insert("owner_method".into(), json!(owner.method));
    fields.insert(
        "provenance".into(),
        json!({
            "run_url": run["html_url"],
            "job_url": job.html_url,
            "extract_version": c2::VERSION,
            "collected_at": query.now,
            "collector": COLLECTOR,
            "collector_version": env!("CARGO_PKG_VERSION"),
            "log_fetched": log.is_some(),
        }),
    );
    Ok(Some(scrub(row)))
}

/// One red run attempt awaiting its fix.
struct Red {
    repo: String,
    run_id: u64,
    attempt: u64,
    head_sha: String,
    time: String,
    pr: Option<u64>,
    branch: String,
}

fn pending_reds(dir: &Path, since: &str, linked: &BTreeSet<(String, u64, u64)>) -> Vec<Red> {
    let mut reds: BTreeMap<(String, u64, u64), Red> = BTreeMap::new();
    for row in archived_causes(dir) {
        if row["failure_class"] == "cancelled" {
            continue; // a cancel is not a red that a push fixes
        }
        let (Some(repo), Some(run_id), Some(head_sha), Some(completed)) = (
            row["repo"].as_str(),
            row["run_id"].as_u64(),
            row["head_sha"].as_str(),
            row["completed_at"].as_str(),
        ) else {
            continue;
        };
        if completed < since {
            continue;
        }
        let attempt = row["attempt"].as_u64().unwrap_or(1);
        let key = (repo.to_owned(), run_id, attempt);
        if linked.contains(&key) {
            continue;
        }
        let red = reds.entry(key).or_insert_with(|| Red {
            repo: repo.to_owned(),
            run_id,
            attempt,
            head_sha: head_sha.to_owned(),
            time: completed.to_owned(),
            pr: row["pr"].as_u64(),
            branch: row["head_branch"].as_str().unwrap_or_default().to_owned(),
        });
        if completed > red.time.as_str() {
            completed.clone_into(&mut red.time);
        }
    }
    reds.into_values().collect()
}

fn merge_base(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    sha: &str,
    cache: &mut BTreeMap<String, Option<String>>,
) -> Option<String> {
    cache
        .entry(sha.to_owned())
        .or_insert_with(|| {
            api(
                gh,
                &format!(
                    "repos/{}/compare/{}...{sha}?per_page=1",
                    query.repo, query.base
                ),
            )
            .ok()
            .and_then(|compare| {
                compare["merge_base_commit"]["sha"]
                    .as_str()
                    .map(str::to_owned)
            })
        })
        .clone()
}

fn compare_files(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    from: &str,
    to: &str,
) -> Result<Vec<String>, String> {
    let compare = api(gh, &format!("repos/{}/compare/{from}...{to}", query.repo))?;
    let mut files: Vec<String> = compare["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter_map(|file| file["filename"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    Ok(files)
}

/// A green run that ends a red: a later push, or a rerun of the red head.
struct Fix {
    head_sha: String,
    run_id: u64,
    pushed_at: String,
    derivation: &'static str,
}

/// The files a fix changed: the raw compare, and the pull request's own share.
struct FileSets {
    raw: Vec<String>,
    raw_truncated: bool,
    own: Option<Vec<String>>,
    method: &'static str,
    own_truncated: Option<bool>,
    main_count: Option<usize>,
}

/// The branch a red is fixed on: a merge-group red's pull request branch,
/// else its own.
fn fix_branch(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    red: &Red,
    pulls: &mut BTreeMap<String, Vec<Value>>,
) -> String {
    let Some(pr) = queue_pr(&red.branch) else {
        return red.branch.clone();
    };
    pulls
        .entry(format!("#pr-{pr}"))
        .or_insert_with(|| {
            api(gh, &format!("repos/{}/pulls/{pr}", query.repo))
                .map(|pull| vec![pull])
                .unwrap_or_default()
        })
        .first()
        .and_then(|pull| pull["head"]["ref"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn created(run: &Value) -> String {
    run["created_at"].as_str().unwrap_or_default().to_owned()
}

/// The rerun fix (a later attempt of the red run, else the earliest later
/// green on the red head) and the push fix (the earliest later green on
/// another head), when they exist.
fn fixes_for(red: &Red, later: &[&Value], run_index: &BTreeMap<u64, Value>) -> Vec<Fix> {
    let on_red_head = |run: &&&Value| run["head_sha"].as_str() == Some(red.head_sha.as_str());
    let rerun = run_index
        .get(&red.run_id)
        .filter(|run| {
            run["conclusion"] == "success" && run["run_attempt"].as_u64().unwrap_or(1) > red.attempt
        })
        .and_then(|run| run["run_started_at"].as_str())
        .map(|at| (red.run_id, at.to_owned()))
        .or_else(|| {
            later
                .iter()
                .filter(on_red_head)
                .min_by_key(|run| created(run))
                .map(|run| (run["id"].as_u64().unwrap_or_default(), created(run)))
        })
        .map(|(run_id, pushed_at)| Fix {
            head_sha: red.head_sha.clone(),
            run_id,
            pushed_at,
            derivation: "rerun_green",
        });
    let push = later
        .iter()
        .filter(|run| !on_red_head(run))
        .min_by_key(|run| created(run))
        .map(|run| Fix {
            head_sha: run["head_sha"].as_str().unwrap_or_default().to_owned(),
            run_id: run["id"].as_u64().unwrap_or_default(),
            pushed_at: created(run),
            derivation: "compare_red_head_to_fix_head",
        });
    rerun.into_iter().chain(push).collect()
}

/// The files a fix changed. A rerun changed none; on the base branch the
/// pull request's own share is undefined; elsewhere it is the raw compare
/// minus what the base branch changed between the two merge bases.
fn file_sets(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    red: &Red,
    fix: &Fix,
    branch: &str,
    bases: &mut BTreeMap<String, Option<String>>,
) -> Result<FileSets, String> {
    if fix.derivation == "rerun_green" {
        return Ok(FileSets {
            raw: Vec::new(),
            raw_truncated: false,
            own: Some(Vec::new()),
            method: "rerun_same_head",
            own_truncated: Some(false),
            main_count: None,
        });
    }
    let raw = compare_files(gh, query, &red.head_sha, &fix.head_sha)
        .map_err(|error| format!("compare for run {}: {error}", red.run_id))?;
    let raw_truncated = raw.len() >= COMPARE_FILE_CAP;
    let unset = |raw, method| FileSets {
        raw,
        raw_truncated,
        own: None,
        method,
        own_truncated: None,
        main_count: None,
    };
    if branch == query.base {
        return Ok(unset(raw, "branch_is_default"));
    }
    let (Some(a), Some(b)) = (
        merge_base(gh, query, &red.head_sha, bases),
        merge_base(gh, query, &fix.head_sha, bases),
    ) else {
        return Ok(unset(raw, "merge_base_unavailable"));
    };
    let main_files = if a == b {
        Vec::new()
    } else {
        compare_files(gh, query, &a, &b).unwrap_or_default()
    };
    let main: BTreeSet<&String> = main_files.iter().collect();
    let own = raw
        .iter()
        .filter(|file| !main.contains(file))
        .cloned()
        .collect();
    Ok(FileSets {
        own: Some(own),
        method: "raw_minus_main_between_merge_bases",
        own_truncated: Some(raw_truncated || main_files.len() >= COMPARE_FILE_CAP),
        main_count: Some(main_files.len()),
        raw,
        raw_truncated,
    })
}

fn fix_row(red: &Red, fix: &Fix, branch: &str, files: &FileSets) -> Value {
    let mut row = Map::new();
    row.insert("schema".into(), json!(cause::SCHEMA));
    row.insert("source".into(), json!("fix-link"));
    row.insert("repo".into(), json!(red.repo));
    row.insert("pr".into(), json!(red.pr));
    row.insert("head_sha".into(), json!(red.head_sha));
    row.insert("fix_head_sha".into(), json!(fix.head_sha));
    row.insert("pushed_at".into(), json!(fix.pushed_at));
    row.insert("files_raw".into(), json!(files.raw));
    row.insert("files_truncated".into(), json!(files.raw_truncated));
    row.insert("own_files".into(), json!(files.own));
    row.insert("own_files_method".into(), json!(files.method));
    row.insert("own_files_truncated".into(), json!(files.own_truncated));
    row.insert(
        "main_files_between_merge_bases".into(),
        json!(files.main_count),
    );
    row.insert("fix_run_id".into(), json!(fix.run_id));
    row.insert("red_run_id".into(), json!(red.run_id));
    row.insert("red_attempt".into(), json!(red.attempt));
    row.insert("red_time".into(), json!(red.time));
    row.insert("branch".into(), json!(branch));
    row.insert("derivation".into(), json!(fix.derivation));
    row.insert("rule".into(), json!(FIXLINK_RULE));
    row.insert("own_files_rule".into(), json!(OWN_FILES_RULE));
    row.insert("collector".into(), json!(COLLECTOR));
    scrub(Value::Object(row))
}

/// Fix-links for every red run attempt in the window that has turned green.
fn fixlinks(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    run_index: &BTreeMap<u64, Value>,
    pulls: &mut BTreeMap<String, Vec<Value>>,
    report: &mut RecordReport,
) -> Result<Vec<Value>, String> {
    let linked: BTreeSet<(String, u64, u64)> = read_jsonl(&query.dir.join("fixlinks.jsonl"))
        .iter()
        .filter_map(|row| {
            Some((
                row["repo"].as_str()?.to_owned(),
                row["red_run_id"].as_u64()?,
                row["red_attempt"].as_u64().unwrap_or(1),
            ))
        })
        .collect();
    let mut greens: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut bases = BTreeMap::new();
    let mut out = Vec::new();
    for red in pending_reds(&query.dir, &query.since, &linked) {
        if red.repo != query.repo {
            continue;
        }
        let branch = fix_branch(gh, query, &red, pulls);
        if branch.is_empty() {
            report.reds_pending += 1;
            continue;
        }
        let listed = greens.entry(branch.clone()).or_insert_with(|| {
            list_runs(
                gh,
                query,
                &format!(
                    "branch={}&status=success&created=%3E%3D{}",
                    percent_encode(&branch),
                    query.since
                ),
            )
            .unwrap_or_default()
        });
        let later: Vec<&Value> = listed
            .iter()
            .filter(|run| created(run).as_str() > red.time.as_str())
            .collect();
        let fixes = fixes_for(&red, &later, run_index);
        if fixes.is_empty() {
            report.reds_pending += 1;
            continue;
        }
        for fix in &fixes {
            let sets = file_sets(gh, query, &red, fix, &branch, &mut bases)?;
            out.push(fix_row(&red, fix, &branch, &sets));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
