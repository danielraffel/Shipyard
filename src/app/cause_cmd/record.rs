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
use crate::gate_cost::{SyncGhReader, fetch_job_log};

mod labels;

/// Who wrote a row.
pub(super) const COLLECTOR: &str = "shipyard cause record";
/// The compare API lists at most this many files.
const COMPARE_FILE_CAP: usize = 300;
/// Most pages of 100 commits one compare is read for.
const COMPARE_COMMIT_PAGES: usize = 10;
/// Most pages of runs one pass lists.
const MAX_RUN_PAGES: usize = 10;

pub(super) const OWN_FILES_RULE: &str = "own_files = the union of the files of the non-merge \
    commits on fix_head's first-parent chain back to red_head (commits and parents from compare \
    red_head...fix_head; files from commits/<sha>); merges of the base branch sit on that chain as \
    merge commits and are skipped. rerun_same_head: []; null when red_head is not an ancestor of \
    fix_head (red_not_ancestor), the range exceeds 1000 commits (too_many_commits), \
    the chain leaves the listed commits (chain_broken), or the fix is on the default branch \
    (branch_is_default). When the red is not an ancestor: a merge-queue red walks the fix back to the \
    pull request head the queue merged (merge_queue_pr_head); else, within 72 hours, own_files are the \
    non-merge commits up to the fix in the pull request's commit list (post_force_push), null when the \
    fix is no longer in that list (post_force_push_superseded) or came later (post_force_push_too_late). \
    own_files_truncated when a commit lists 300 or more files";
const FIXLINK_RULE: &str = "earliest successful run of the same workflow on the red's pull request \
    branch (or branch) created after the red attempt's last failed job completed; a later attempt of \
    the red run itself is rerun_green, while a different successful run on the same head is \
    same_head_green; pushed_at = run_started_at for rerun_green and created_at for same_head_green \
    or compare_red_head_to_fix_head; files_raw = compare red head...fix head";

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
    /// Of those, how many went red under 24 h, 24 to 72 h, and over 72 h
    /// before this pass: what is still young against what is likely unfixed
    /// or unlinkable.
    pub pending_under_24h: usize,
    /// Pending 24 to 72 h.
    pub pending_24h_to_72h: usize,
    /// Pending over 72 h.
    pub pending_over_72h: usize,
    /// Bare exits labelled by their step.
    pub labelled_by_step: usize,
    /// Summary jobs that took the cause of the job they summarise.
    pub inherited: usize,
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
            r"/(?:Users|Volumes|private/(?:tmp|var/folders)|var/folders|tmp|home)/[^\s'\x22]*",
            "<path>",
        ),
        (r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b", "<email>"),
        (r"\b(?:\d{1,3}\.){3}\d{1,3}\b", "<ip>"),
        (
            r"(?i)(token|api[_-]?key|secret|password)(\s*[:=]\s*)[A-Za-z0-9_./+=-]{8,}",
            "$1$2<secret>",
        ),
        (r"\b(?:gh[oprsu])_[A-Za-z0-9]{20,}\b", "<secret>"),
        (r"\bgithub_pat_[A-Za-z0-9_]{20,}\b", "<secret>"),
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

/// Whether text contains anything the archive recorder would redact.
pub(super) fn needs_scrub(text: &str) -> bool {
    SCRUB.iter().any(|(pattern, _)| pattern.is_match(text))
}

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
    let archived = archived_causes(&query.dir);
    let recorded: BTreeSet<(String, u64)> = archived
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
        let mut run_rows = Vec::new();
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
                Ok(Some(row)) => run_rows.push(row),
                Ok(None) => {}
                Err(error) => report.unreadable.push(error),
            }
        }
        report.inherited += labels::inherit_from_summarised(&mut run_rows, &archived);
        for row in run_rows {
            if row["labelled_by"] == "step" {
                report.labelled_by_step += 1;
            }
            let month = row["completed_at"]
                .as_str()
                .and_then(|at| at.get(..7))
                .unwrap_or("undated")
                .to_owned();
            by_month.entry(month).or_default().push(row);
            report.causes_written += 1;
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
    let log = match u64::try_from(job.id)
        .map_err(|error| error.to_string())
        .and_then(|id| fetch_job_log(gh, &query.repo, id))
    {
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
    // The observer names a red while it watches, so the cause is named at
    // this pass, not after the fact: `named_at` minus `completed_at` is the
    // time to cause. A record the job printed itself can only move it earlier.
    let Some(mut cause) = cause::build(job, log.as_deref(), &place, Some(query.now.clone()), false)
    else {
        return Ok(None);
    };
    cause::adopt_printed_time(&mut cause, log.as_deref(), job, &query.repo);
    let label = labels::label_bare_exit(&mut cause, log.as_deref());
    let mut row = serde_json::to_value(&cause).map_err(|error| error.to_string())?;
    let fields = row.as_object_mut().ok_or("cause is not an object")?;
    if let Some(label) = label {
        fields.insert("labelled_by".into(), json!(label));
    }
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

fn file_names(value: &Value) -> Vec<String> {
    let mut files: Vec<String> = value["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter_map(|file| file["filename"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// `own_files`, its method, and whether a commit's file list was cut short.
type OwnFiles = (Option<Vec<String>>, &'static str, Option<bool>);

/// The pull request's own files between `compare`'s two ends: the non-merge
/// commits on the newer end's first-parent chain back to the older end. A
/// merge of the base branch is a merge commit on that chain and is skipped,
/// so what the base branch changed never counts. Today's base branch is never
/// consulted: once the pull request has merged, both ends are its ancestors.
fn own_by_first_parent(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    compare: &Value,
    red: &str,
    fix: &str,
) -> Result<OwnFiles, String> {
    if !matches!(compare["status"].as_str(), Some("ahead" | "identical")) {
        return Ok((None, "red_not_ancestor", None));
    }
    let total = compare["total_commits"].as_u64().unwrap_or_default();
    let mut commits = compare["commits"].as_array().cloned().unwrap_or_default();
    if total > commits.len() as u64 {
        // compare lists 250 commits unpaged; paged, 100 at a time.
        commits.clear();
        for page in 1..=COMPARE_COMMIT_PAGES {
            let listed = api(
                gh,
                &format!(
                    "repos/{}/compare/{red}...{fix}?per_page=100&page={page}",
                    query.repo
                ),
            )?;
            let more = listed["commits"].as_array().cloned().unwrap_or_default();
            if more.is_empty() {
                break;
            }
            commits.extend(more);
            if commits.len() as u64 >= total {
                break;
            }
        }
    }
    if total > commits.len() as u64 {
        return Ok((None, "too_many_commits", None));
    }
    let by_sha: BTreeMap<&str, &Value> = commits
        .iter()
        .filter_map(|commit| Some((commit["sha"].as_str()?, commit)))
        .collect();
    let mut chain = Vec::new();
    let mut current = fix;
    while current != red {
        let Some(commit) = by_sha.get(current) else {
            return Ok((None, "chain_broken", None));
        };
        let Some(parent) = commit["parents"][0]["sha"].as_str() else {
            return Ok((None, "chain_broken", None));
        };
        chain.push(*commit);
        current = parent;
    }
    let mut own = BTreeSet::new();
    let mut truncated = false;
    for commit in chain {
        if commit["parents"].as_array().map_or(0, Vec::len) != 1 {
            continue; // a merge, of the base branch or another branch
        }
        let sha = commit["sha"].as_str().unwrap_or_default();
        let detail = api(gh, &format!("repos/{}/commits/{sha}", query.repo))?;
        let names = file_names(&detail);
        truncated |= names.len() >= COMPARE_FILE_CAP;
        own.extend(names);
    }
    Ok((
        Some(own.into_iter().collect()),
        "first_parent_non_merge_commits",
        Some(truncated),
    ))
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

/// The rerun fix (a later attempt of the red run), a later green run on the
/// same head, and the push fix (the earliest later green on another head),
/// when they exist.
fn fixes_for(red: &Red, later: &[&Value], run_index: &BTreeMap<u64, Value>) -> Vec<Fix> {
    let on_red_head = |run: &&&Value| run["head_sha"].as_str() == Some(red.head_sha.as_str());
    let rerun = run_index
        .get(&red.run_id)
        .filter(|run| {
            run["conclusion"] == "success" && run["run_attempt"].as_u64().unwrap_or(1) > red.attempt
        })
        .and_then(|run| run["run_started_at"].as_str())
        .map(|at| (red.run_id, at.to_owned(), "rerun_green"))
        .or_else(|| {
            later
                .iter()
                .filter(on_red_head)
                .min_by_key(|run| created(run))
                .map(|run| {
                    (
                        run["id"].as_u64().unwrap_or_default(),
                        created(run),
                        "same_head_green",
                    )
                })
        })
        .map(|(run_id, pushed_at, derivation)| Fix {
            head_sha: red.head_sha.clone(),
            run_id,
            pushed_at,
            derivation,
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

/// Longest a force-pushed red waits for its pull request's next green push.
const FORCE_PUSH_WINDOW_HOURS: i64 = 72;

fn minutes_between(from: &str, to: &str) -> Option<i64> {
    let parse = |at: &str| chrono::DateTime::parse_from_rfc3339(at).ok();
    Some((parse(to)? - parse(from)?).num_minutes())
}

/// Own files for a red whose head is not an ancestor of its fix.
///
/// A merge-queue red's head is the queue's merge commit; its second parent
/// is the pull request's head when it was queued, and the fix is walked back
/// to that (`merge_queue_pr_head`). Otherwise, or when that head was
/// force-pushed away too, the red was force-pushed: within 72 hours the fix
/// is matched in the pull request's commit list, and its own files are the
/// non-merge commits up to it there (`post_force_push`).
fn recover_own(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    red: &Red,
    fix: &Fix,
) -> Result<OwnFiles, String> {
    if queue_pr(&red.branch).is_some() {
        let queued = api(
            gh,
            &format!("repos/{}/commits/{}", query.repo, red.head_sha),
        )?;
        if let Some(pr_head) = queued["parents"]
            .as_array()
            .filter(|parents| parents.len() == 2)
            .and_then(|parents| parents[1]["sha"].as_str())
        {
            let compare = api(
                gh,
                &format!("repos/{}/compare/{pr_head}...{}", query.repo, fix.head_sha),
            )?;
            if let (Some(own), _, truncated) =
                own_by_first_parent(gh, query, &compare, pr_head, &fix.head_sha)?
            {
                return Ok((Some(own), "merge_queue_pr_head", truncated));
            }
        }
    }
    post_force_push(gh, query, red, fix)
}

fn post_force_push(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    red: &Red,
    fix: &Fix,
) -> Result<OwnFiles, String> {
    let Some(pr) = red.pr else {
        return Ok((None, "red_not_ancestor", None));
    };
    if minutes_between(&red.time, &fix.pushed_at)
        .is_none_or(|minutes| minutes > FORCE_PUSH_WINDOW_HOURS * 60)
    {
        return Ok((None, "post_force_push_too_late", None));
    }
    let mut commits = Vec::new();
    for page in 1..=3 {
        let listed = api(
            gh,
            &format!(
                "repos/{}/pulls/{pr}/commits?per_page=100&page={page}",
                query.repo
            ),
        )?;
        let more = listed.as_array().cloned().unwrap_or_default();
        let full = more.len() == 100;
        commits.extend(more);
        if !full {
            break;
        }
    }
    let Some(at) = commits
        .iter()
        .position(|commit| commit["sha"].as_str() == Some(fix.head_sha.as_str()))
    else {
        return Ok((None, "post_force_push_superseded", None));
    };
    let mut own = BTreeSet::new();
    let mut truncated = false;
    for commit in &commits[..=at] {
        if commit["parents"].as_array().map_or(0, Vec::len) != 1 {
            continue;
        }
        let sha = commit["sha"].as_str().unwrap_or_default();
        let names = file_names(&api(gh, &format!("repos/{}/commits/{sha}", query.repo))?);
        truncated |= names.len() >= COMPARE_FILE_CAP;
        own.extend(names);
    }
    Ok((
        Some(own.into_iter().collect()),
        "post_force_push",
        Some(truncated),
    ))
}

/// The files a fix changed. A rerun changed none; on the base branch the
/// pull request's own share is undefined; elsewhere it is the first-parent
/// walk from the fix back to the red.
fn file_sets(
    gh: &SyncGhReader<'_>,
    query: &RecordQuery,
    red: &Red,
    fix: &Fix,
    branch: &str,
) -> Result<FileSets, String> {
    if matches!(fix.derivation, "rerun_green" | "same_head_green") {
        return Ok(FileSets {
            raw: Vec::new(),
            raw_truncated: false,
            own: Some(Vec::new()),
            method: "rerun_same_head",
            own_truncated: Some(false),
        });
    }
    let compare = api(
        gh,
        &format!(
            "repos/{}/compare/{}...{}",
            query.repo, red.head_sha, fix.head_sha
        ),
    )
    .map_err(|error| format!("compare for run {}: {error}", red.run_id))?;
    let raw = file_names(&compare);
    let raw_truncated = raw.len() >= COMPARE_FILE_CAP;
    let (own, method, own_truncated) = if branch == query.base {
        (None, "branch_is_default", None)
    } else {
        match own_by_first_parent(gh, query, &compare, &red.head_sha, &fix.head_sha) {
            // A recovery read that fails leaves the red unlinked by its own
            // commits, as it was, rather than failing the whole fix-link.
            Ok((None, "red_not_ancestor", _)) => {
                recover_own(gh, query, red, fix).or(Ok((None, "red_not_ancestor", None)))
            }
            other => other,
        }
        .map_err(|error| format!("own files for run {}: {error}", red.run_id))?
    };
    Ok(FileSets {
        raw,
        raw_truncated,
        own,
        method,
        own_truncated,
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

/// Count a red still without a fix, by how long ago it went red.
fn pending(report: &mut RecordReport, red: &Red, now: &str) {
    report.reds_pending += 1;
    match minutes_between(&red.time, now) {
        Some(minutes) if minutes < 24 * 60 => report.pending_under_24h += 1,
        Some(minutes) if minutes <= 72 * 60 => report.pending_24h_to_72h += 1,
        _ => report.pending_over_72h += 1,
    }
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
    let mut out = Vec::new();
    for red in pending_reds(&query.dir, &query.since, &linked) {
        if red.repo != query.repo {
            continue;
        }
        let branch = fix_branch(gh, query, &red, pulls);
        if branch.is_empty() {
            pending(report, &red, &query.now);
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
            pending(report, &red, &query.now);
            continue;
        }
        for fix in &fixes {
            let sets = file_sets(gh, query, &red, fix, &branch)?;
            out.push(fix_row(&red, fix, &branch, &sets));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
