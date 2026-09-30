//! Reading the facts behind one pull request's landing verdict.
//!
//! Every request is a read through a reader (`gh` argv in, text out), so tests
//! replay recorded responses and nothing here can write. Reads, in order, with
//! their bound:
//!
//! | read | calls |
//! |---|---|
//! | GraphQL: head, head branch, base, creation, merge-queue timeline | 1 |
//! | `branches/{base}/protection/required_status_checks` | 1 |
//! | current head's check runs (every attempt) | 1 |
//! | current head's commit statuses, only when a required context has no check run | 0-1 |
//! | `pull_request` runs of the head branch, to find earlier heads | 1 |
//! | check runs of earlier heads whose runs failed | ≤ [`MAX_PRIOR_HEADS`] |
//! | `merge_group` runs created while the pull request sat in the queue | ≤ [`MAX_QUEUE_WINDOWS`] |
//! | jobs of failed merge-group runs named for the pull request | ≤ [`MAX_OWN_GROUP_RUNS`] |
//! | logs of this pull request's failed required jobs | ≤ [`MAX_OWN_LOGS`] |
//! | the failing workflow's failed `pull_request` and `merge_group` runs in the window | ≤ 2 per failing workflow, ≤ [`MAX_SHARED_WORKFLOWS`] workflows |
//! | jobs + log per other pull request's failing run, until the shared test settles | ≤ 2 × [`MAX_SHARED_RUNS`] |
//!
//! Merge groups are found through the pull request's own queue residency
//! windows rather than a repository-wide listing: every workflow of a group is
//! its own run, so on a busy repository the latest hundred `merge_group` runs
//! cover a few hours at most, and the groups that matter are exactly the ones
//! created while the pull request was queued.
//!
//! Failure signatures of completed job logs are served from the PR watch read
//! cache when one is supplied. The reported count is calls actually sent.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use super::verdict::VerdictFacts;
use crate::gate_cost::ReadCache;
use crate::merge_queue_liveness::merge_group_pr;
use crate::pr_watch::{
    CheckFact, GroupRun, HeadFact, PrHistory, RepoHistory, Thresholds, failure_signatures, short,
};

/// Earlier heads whose check runs are read.
pub const MAX_PRIOR_HEADS: usize = 6;
/// Most recent queue residency windows searched for merge groups.
pub const MAX_QUEUE_WINDOWS: usize = 4;
/// Failed merge-group runs named for the pull request whose jobs are read.
pub const MAX_OWN_GROUP_RUNS: usize = 6;
/// Logs of this pull request's failed required jobs read at most.
pub const MAX_OWN_LOGS: usize = 8;
/// Failing workflows searched for other pull requests' failures.
pub const MAX_SHARED_WORKFLOWS: usize = 2;
/// Other pull requests' failed runs inspected for the shared-failure test.
pub const MAX_SHARED_RUNS: usize = 10;
/// Look-back for earlier heads' queue windows.
pub const LOOKBACK_HOURS: i64 = 72;
/// Slack after a queue removal during which a group run may still be created.
const WINDOW_SLACK_MINUTES: i64 = 5;

/// Pull request facts and its merge-queue timeline, in one round trip.
pub const PULL_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid headRefName baseRefName createdAt timelineItems(last:50,itemTypes:[ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT,MERGED_EVENT]){nodes{__typename ... on AddedToMergeQueueEvent{createdAt} ... on RemovedFromMergeQueueEvent{createdAt} ... on MergedEvent{createdAt}}}} pullRequests(states:OPEN,first:100,orderBy:{field:UPDATED_AT,direction:DESC}){nodes{number headRefName}}}}";

/// A read-only GitHub reader: `gh` argv in, stdout out.
pub type Reader<'a> = dyn Fn(&[String]) -> Result<String, String> + 'a;

/// Why the verdict could not be computed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatherFailure {
    /// Current head, when it was read before the failure.
    pub head_sha: Option<String>,
    /// What failed.
    pub detail: String,
    /// Gaps recorded before the failure.
    pub gaps: Vec<String>,
    /// Calls spent.
    pub api_calls: u32,
}

struct Counted<'a> {
    reader: &'a Reader<'a>,
    calls: Cell<u32>,
}

impl Counted<'_> {
    fn raw(&self, args: &[String]) -> Result<String, String> {
        self.calls.set(self.calls.get() + 1);
        (self.reader)(args)
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        let text = self.raw(&["api".to_owned(), path.to_owned()])?;
        serde_json::from_str(&text).map_err(|error| format!("{path}: invalid JSON: {error}"))
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn time(value: &Value, key: &str) -> Option<DateTime<Utc>> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&Utc))
}

fn stamp(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn check_fact(value: &Value) -> Option<CheckFact> {
    Some(CheckFact {
        name: text(value, "name")?,
        id: value.get("id").and_then(Value::as_u64)?,
        status: text(value, "status").unwrap_or_default(),
        conclusion: text(value, "conclusion"),
        started_at: time(value, "started_at"),
        completed_at: time(value, "completed_at"),
        signatures: Vec::new(),
    })
}

fn run_id_from_url(url: Option<&str>) -> Option<u64> {
    let rest = url?.split("/actions/runs/").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Required check runs of one commit (every attempt), and each check run's
/// workflow run id.
fn head_checks(
    gh: &Counted<'_>,
    repo: &str,
    sha: &str,
    required: &[String],
    gaps: &mut Vec<String>,
) -> Result<(Vec<CheckFact>, BTreeMap<u64, u64>), String> {
    let value = gh.get(&format!(
        "repos/{repo}/commits/{sha}/check-runs?filter=all&per_page=100"
    ))?;
    let items = value
        .get("check_runs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(total) = value.get("total_count").and_then(Value::as_u64)
        && total > items.len() as u64
    {
        gaps.push(format!(
            "head {}: read {} of {total} check runs; an older attempt may be missing",
            short(sha),
            items.len()
        ));
    }
    let mut runs = BTreeMap::new();
    let mut checks = Vec::new();
    for item in &items {
        let Some(fact) = check_fact(item) else {
            continue;
        };
        if !required.contains(&fact.name) {
            continue;
        }
        if let Some(run) = run_id_from_url(item.get("html_url").and_then(Value::as_str)) {
            runs.insert(fact.id, run);
        }
        checks.push(fact);
    }
    Ok((checks, runs))
}

/// Failure signatures of one failed job, cached once read.
fn signatures(
    gh: &Counted<'_>,
    cache: &ReadCache,
    repo: &str,
    job: u64,
) -> Result<Vec<String>, String> {
    let key = format!("pr-watch:signatures:{repo}:{job}");
    if let Some(Value::Array(items)) = cache.get(&key) {
        return Ok(items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect());
    }
    let log = gh.raw(&[
        "api".to_owned(),
        format!("repos/{repo}/actions/jobs/{job}/logs"),
    ])?;
    let found = failure_signatures(&log);
    cache.put(
        &key,
        &Value::Array(found.iter().cloned().map(Value::String).collect()),
    );
    Ok(found)
}

/// What the pull request itself looks like.
struct PullFacts {
    head_sha: String,
    head_ref: String,
    base: String,
    created_at: Option<DateTime<Utc>>,
    /// Queue residency windows, oldest first.
    queue_windows: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    /// Open pull requests by head branch: a `pull_request` run's own
    /// `pull_requests` array is often empty, and without this those runs
    /// could not be attributed to anybody.
    open_by_branch: BTreeMap<String, u64>,
}

fn read_pull(
    gh: &Counted<'_>,
    repo: &str,
    pr: u64,
    now: DateTime<Utc>,
) -> Result<PullFacts, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("repo `{repo}` is not OWNER/REPO"))?;
    let raw = gh.raw(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={PULL_QUERY}"),
        "-F".to_owned(),
        format!("owner={owner}"),
        "-F".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("number={pr}"),
    ])?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|error| format!("malformed GraphQL JSON: {error}"))?;
    let node = value
        .pointer("/data/repository/pullRequest")
        .filter(|node| node.is_object())
        .ok_or_else(|| {
            value
                .pointer("/errors/0/message")
                .and_then(Value::as_str)
                .map_or_else(
                    || "no pullRequest in the response".to_owned(),
                    str::to_owned,
                )
        })?;
    let head_sha = text(node, "headRefOid").ok_or("pull request carried no headRefOid")?;
    let mut windows = Vec::new();
    let mut open: Option<DateTime<Utc>> = None;
    for event in node
        .pointer("/timelineItems/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(at) = time(event, "createdAt") else {
            continue;
        };
        match event.get("__typename").and_then(Value::as_str) {
            Some("AddedToMergeQueueEvent") => {
                if let Some(start) = open.take() {
                    windows.push((start, at));
                }
                open = Some(at);
            }
            Some("RemovedFromMergeQueueEvent" | "MergedEvent") => {
                if let Some(start) = open.take() {
                    windows.push((start, at + Duration::minutes(WINDOW_SLACK_MINUTES)));
                }
            }
            _ => {}
        }
    }
    if let Some(start) = open {
        windows.push((start, now));
    }
    let open_by_branch = value
        .pointer("/data/repository/pullRequests/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| Some((text(node, "headRefName")?, node.get("number")?.as_u64()?)))
        .collect();
    Ok(PullFacts {
        open_by_branch,
        head_sha,
        head_ref: text(node, "headRefName").unwrap_or_default(),
        base: text(node, "baseRefName").unwrap_or_else(|| "main".to_owned()),
        created_at: time(node, "createdAt"),
        queue_windows: windows,
    })
}

/// Branch protection's required contexts. `Ok(empty)` for an unprotected base.
fn read_required(gh: &Counted<'_>, repo: &str, base: &str) -> Result<Vec<String>, String> {
    match gh.get(&format!(
        "repos/{repo}/branches/{base}/protection/required_status_checks"
    )) {
        Ok(value) => {
            if value.get("contexts").is_none() && value.get("checks").is_none() {
                // A response with neither list is not "no required checks";
                // it is not a required-checks response at all.
                return Err(format!(
                    "required checks of `{base}` unreadable: the response carried neither \
                     `contexts` nor `checks`"
                ));
            }
            let mut contexts: Vec<String> = value
                .get("contexts")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            for check in value
                .get("checks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(context) = text(check, "context")
                    && !contexts.contains(&context)
                {
                    contexts.push(context);
                }
            }
            Ok(contexts)
        }
        Err(error) if error.to_ascii_lowercase().contains("not protected") => Ok(Vec::new()),
        Err(error) => Err(format!("required checks of `{base}` unreadable: {error}")),
    }
}

/// A listed workflow run.
#[derive(Clone)]
struct ListedRun {
    id: u64,
    workflow_id: Option<u64>,
    head_sha: String,
    head_branch: String,
    created_at: DateTime<Utc>,
    conclusion: Option<String>,
    status: String,
    pr: Option<u64>,
}

fn listed_runs(value: &Value) -> Vec<ListedRun> {
    value
        .get("workflow_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|run| {
            let head_branch = text(run, "head_branch").unwrap_or_default();
            Some(ListedRun {
                id: run.get("id").and_then(Value::as_u64)?,
                workflow_id: run.get("workflow_id").and_then(Value::as_u64),
                head_sha: text(run, "head_sha").unwrap_or_default(),
                created_at: time(run, "created_at")?,
                conclusion: text(run, "conclusion"),
                status: text(run, "status").unwrap_or_default(),
                pr: merge_group_pr(&head_branch).or_else(|| {
                    run.pointer("/pull_requests/0/number")
                        .and_then(Value::as_u64)
                }),
                head_branch,
            })
        })
        .collect()
}

fn group_run(run: &ListedRun, required_jobs: Vec<CheckFact>) -> GroupRun {
    GroupRun {
        id: run.id,
        pr: merge_group_pr(&run.head_branch),
        head_sha: run.head_sha.clone(),
        parent_sha: None,
        created_at: run.created_at,
        conclusion: run.conclusion.clone(),
        required_jobs,
        attribution: None,
    }
}

/// Required jobs (latest attempt) of one run.
fn run_jobs(
    gh: &Counted<'_>,
    repo: &str,
    run: u64,
    required: &[String],
) -> Result<Vec<CheckFact>, String> {
    let value = gh.get(&format!(
        "repos/{repo}/actions/runs/{run}/jobs?filter=latest&per_page=100"
    ))?;
    Ok(value
        .get("jobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(check_fact)
        .filter(|job| required.contains(&job.name))
        .collect())
}

/// Read every fact the verdict needs for `pr`.
///
/// # Errors
/// When the pull request, its base's required checks, or its current head's
/// check runs cannot be read: without them no verdict is honest.
pub fn gather(
    reader: &Reader<'_>,
    cache: &ReadCache,
    repo: &str,
    pr: u64,
    now: DateTime<Utc>,
) -> Result<VerdictFacts, GatherFailure> {
    let gh = Counted {
        reader,
        calls: Cell::new(0),
    };
    let mut gaps: Vec<String> = Vec::new();
    let fail = |head: Option<&str>, detail: String, gaps: &[String]| GatherFailure {
        head_sha: head.map(str::to_owned),
        detail,
        gaps: gaps.to_vec(),
        api_calls: gh.calls.get(),
    };

    let pull = read_pull(&gh, repo, pr, now)
        .map_err(|error| fail(None, format!("PR #{pr} unreadable: {error}"), &gaps))?;
    let head = Some(pull.head_sha.as_str());
    let required =
        read_required(&gh, repo, &pull.base).map_err(|error| fail(head, error, &gaps))?;
    let (current_checks, current_runs) =
        head_checks(&gh, repo, &pull.head_sha, &required, &mut gaps).map_err(|error| {
            fail(
                head,
                format!("check runs of the current head unreadable: {error}"),
                &gaps,
            )
        })?;
    let statuses = read_statuses(&gh, repo, &pull.head_sha, &required, &current_checks)
        .map_err(|error| fail(head, error, &gaps))?;

    let branch_runs = read_branch_runs(&gh, repo, &pull, &mut gaps);
    let first_seen = first_seen_by_head(&branch_runs);
    let mut heads = read_prior_heads(
        &gh,
        repo,
        &pull,
        &required,
        &branch_runs,
        &first_seen,
        &mut gaps,
    );
    let (mut group_runs, group_listing, unread_groups) =
        read_own_groups(&gh, repo, pr, &pull, &required, now, &mut gaps);

    let mut current_checks = current_checks;
    let unreadable = fill_signatures(
        &gh,
        cache,
        repo,
        &mut current_checks,
        &mut heads,
        &mut group_runs,
        &mut gaps,
    );

    heads.push(HeadFact {
        sha: pull.head_sha.clone(),
        first_seen_at: first_seen.get(&pull.head_sha).copied().unwrap_or(now),
        gate_runs: Vec::new(),
        checks: current_checks,
        merge_base: None,
    });
    let mut history = own_history(repo, pr, &pull, &required, heads, group_runs, now);

    let workflows = failing_workflows(
        &gh,
        repo,
        &history,
        pr,
        &current_runs,
        &branch_runs,
        &group_listing,
        &mut gaps,
    );
    shared_scan(
        &gh,
        cache,
        &SharedScan {
            repo,
            pr,
            now,
            required: &required,
            workflows: &workflows,
            open_by_branch: &pull.open_by_branch,
        },
        &mut history,
        &mut gaps,
    );

    Ok(VerdictFacts {
        pr,
        head_sha: pull.head_sha,
        base: pull.base,
        required,
        history,
        statuses,
        unreadable_logs: unreadable,
        unread_group_runs: unread_groups,
        now,
        gaps,
        api_calls: gh.calls.get(),
    })
}

/// The pull request's own history in PR watch's shape, so flag 1 can read it.
fn own_history(
    repo: &str,
    pr: u64,
    pull: &PullFacts,
    required: &[String],
    heads: Vec<HeadFact>,
    group_runs: Vec<GroupRun>,
    now: DateTime<Utc>,
) -> RepoHistory {
    let this = PrHistory {
        number: pr,
        head_ref: pull.head_ref.clone(),
        head_sha: pull.head_sha.clone(),
        created_at: pull.created_at,
        heads,
        timeline_complete: true,
        ..PrHistory::default()
    };
    RepoHistory {
        repo: repo.to_owned(),
        base: pull.base.clone(),
        from: now - Duration::hours(LOOKBACK_HOURS),
        to: now,
        required_checks: required.to_vec(),
        prs: BTreeMap::from([(pr, this)]),
        group_runs,
        gaps: Vec::new(),
    }
}

/// Read failure signatures of this pull request's failed required jobs, the
/// current head first, then earlier heads and groups newest first, within
/// [`MAX_OWN_LOGS`]. Returns the failed job ids whose log was not read.
fn fill_signatures(
    gh: &Counted<'_>,
    cache: &ReadCache,
    repo: &str,
    current: &mut [CheckFact],
    heads: &mut [HeadFact],
    group_runs: &mut [GroupRun],
    gaps: &mut Vec<String>,
) -> BTreeSet<u64> {
    let mut unreadable = BTreeSet::new();
    let mut budget = MAX_OWN_LOGS;
    let mut skipped = 0usize;
    let mut fill = |checks: &mut [CheckFact], gaps: &mut Vec<String>| {
        for check in checks.iter_mut().filter(|check| check.failed()) {
            if budget == 0 {
                unreadable.insert(check.id);
                skipped += 1;
                continue;
            }
            budget -= 1;
            match signatures(gh, cache, repo, check.id) {
                Ok(found) => check.signatures = found,
                Err(error) => {
                    unreadable.insert(check.id);
                    gaps.push(format!("log of job {} unreadable: {error}", check.id));
                }
            }
        }
    };
    fill(current, gaps);
    let mut lanes: Vec<(DateTime<Utc>, bool, usize)> = heads
        .iter()
        .enumerate()
        .map(|(index, head)| (head.first_seen_at, true, index))
        .chain(
            group_runs
                .iter()
                .enumerate()
                .map(|(index, run)| (run.created_at, false, index)),
        )
        .collect();
    lanes.sort_by_key(|(at, _, _)| std::cmp::Reverse(*at));
    for (_, is_head, index) in lanes {
        if is_head {
            fill(&mut heads[index].checks, gaps);
        } else {
            fill(&mut group_runs[index].required_jobs, gaps);
        }
    }
    if skipped > 0 {
        gaps.push(format!(
            "log budget ({MAX_OWN_LOGS}) spent; {skipped} older failed job(s) were not compared"
        ));
    }
    unreadable
}

/// Commit statuses for required contexts that have no check run.
fn read_statuses(
    gh: &Counted<'_>,
    repo: &str,
    sha: &str,
    required: &[String],
    checks: &[CheckFact],
) -> Result<BTreeMap<String, String>, String> {
    let mut statuses = BTreeMap::new();
    if required
        .iter()
        .all(|name| checks.iter().any(|check| check.name == *name))
    {
        return Ok(statuses);
    }
    let value = gh
        .get(&format!("repos/{repo}/commits/{sha}/status"))
        .map_err(|error| format!("commit statuses of the current head unreadable: {error}"))?;
    for status in value
        .get("statuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(context), Some(state)) = (text(status, "context"), text(status, "state"))
            && required.contains(&context)
        {
            // `statuses` is newest first: keep the first per context.
            statuses.entry(context).or_insert(state);
        }
    }
    Ok(statuses)
}

/// `pull_request` runs of the head branch since the pull request opened.
fn read_branch_runs(
    gh: &Counted<'_>,
    repo: &str,
    pull: &PullFacts,
    gaps: &mut Vec<String>,
) -> Vec<ListedRun> {
    let slack = Duration::minutes(2);
    match gh.get(&format!(
        "repos/{repo}/actions/runs?branch={}&event=pull_request&per_page=100",
        pull.head_ref
    )) {
        Ok(value) => listed_runs(&value)
            .into_iter()
            .filter(|run| {
                pull.created_at
                    .is_none_or(|created| run.created_at + slack >= created)
            })
            .collect(),
        Err(error) => {
            gaps.push(format!(
                "runs of `{}` unreadable ({error}); earlier heads were not compared",
                pull.head_ref
            ));
            Vec::new()
        }
    }
}

fn first_seen_by_head(runs: &[ListedRun]) -> BTreeMap<String, DateTime<Utc>> {
    let mut first_seen: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
    for run in runs {
        let seen = first_seen
            .entry(run.head_sha.clone())
            .or_insert(run.created_at);
        *seen = (*seen).min(run.created_at);
    }
    first_seen
}

/// Required check runs of earlier heads that had a failed run.
fn read_prior_heads(
    gh: &Counted<'_>,
    repo: &str,
    pull: &PullFacts,
    required: &[String],
    branch_runs: &[ListedRun],
    first_seen: &BTreeMap<String, DateTime<Utc>>,
    gaps: &mut Vec<String>,
) -> Vec<HeadFact> {
    let failed: BTreeSet<&str> = branch_runs
        .iter()
        .filter(|run| run.conclusion.as_deref() == Some("failure"))
        .map(|run| run.head_sha.as_str())
        .collect();
    let mut prior: Vec<(&String, &DateTime<Utc>)> = first_seen
        .iter()
        .filter(|(sha, _)| **sha != pull.head_sha && failed.contains(sha.as_str()))
        .collect();
    prior.sort_by_key(|(_, at)| **at);
    if prior.len() > MAX_PRIOR_HEADS {
        gaps.push(format!(
            "{} earlier failed heads; compared the latest {MAX_PRIOR_HEADS}",
            prior.len()
        ));
        prior.drain(..prior.len() - MAX_PRIOR_HEADS);
    }
    let mut heads = Vec::new();
    for (sha, at) in prior {
        match head_checks(gh, repo, sha, required, gaps) {
            Ok((checks, _)) => heads.push(HeadFact {
                sha: sha.clone(),
                first_seen_at: *at,
                gate_runs: Vec::new(),
                checks,
                merge_base: None,
            }),
            Err(error) => gaps.push(format!(
                "head {}: check runs unreadable: {error}",
                short(sha)
            )),
        }
    }
    heads
}

/// Merge-group runs named for the pull request, found through its queue
/// residency windows. Returns the group runs (failed ones with their required
/// jobs), every listed run (for workflow lookup), and failed runs whose jobs
/// could not be read.
fn read_own_groups(
    gh: &Counted<'_>,
    repo: &str,
    pr: u64,
    pull: &PullFacts,
    required: &[String],
    now: DateTime<Utc>,
    gaps: &mut Vec<String>,
) -> (Vec<GroupRun>, Vec<ListedRun>, BTreeSet<u64>) {
    let horizon = now - Duration::hours(LOOKBACK_HOURS);
    let windows: Vec<&(DateTime<Utc>, DateTime<Utc>)> = pull
        .queue_windows
        .iter()
        .filter(|(_, end)| *end >= horizon)
        .collect();
    let skip = windows.len().saturating_sub(MAX_QUEUE_WINDOWS);
    if skip > 0 {
        gaps.push(format!(
            "{} queue residencies in {LOOKBACK_HOURS}h; searched the latest {MAX_QUEUE_WINDOWS} for merge groups",
            windows.len()
        ));
    }
    let mut listed: Vec<ListedRun> = Vec::new();
    for (start, end) in windows.into_iter().skip(skip) {
        match gh.get(&format!(
            "repos/{repo}/actions/runs?event=merge_group&created={}..{}&per_page=100",
            stamp(*start),
            stamp(*end)
        )) {
            Ok(value) => {
                if value.get("total_count").and_then(Value::as_u64).unwrap_or(0) > 100 {
                    gaps.push(format!(
                        "more than 100 merge-group runs between {} and {}; read the latest 100",
                        stamp(*start),
                        stamp(*end)
                    ));
                }
                listed.extend(listed_runs(&value));
            }
            Err(error) => gaps.push(format!(
                "merge-group runs between {} and {} unreadable ({error}); those groups were not compared",
                stamp(*start),
                stamp(*end)
            )),
        }
    }
    listed.sort_by_key(|run| std::cmp::Reverse((run.created_at, run.id)));
    listed.dedup_by_key(|run| run.id);
    let mut runs = Vec::new();
    let mut unread = BTreeSet::new();
    let mut failed_read = 0usize;
    let mut failed_total = 0usize;
    for run in listed
        .iter()
        .filter(|run| run.pr == Some(pr) && run.status == "completed")
    {
        if run.conclusion.as_deref() != Some("failure") {
            runs.push(group_run(run, Vec::new()));
            continue;
        }
        failed_total += 1;
        if failed_read >= MAX_OWN_GROUP_RUNS {
            unread.insert(run.id);
            runs.push(group_run(run, Vec::new()));
            continue;
        }
        failed_read += 1;
        match run_jobs(gh, repo, run.id, required) {
            Ok(jobs) => runs.push(group_run(run, jobs)),
            Err(error) => {
                gaps.push(format!(
                    "merge-group run {}: jobs unreadable: {error}",
                    run.id
                ));
                unread.insert(run.id);
                runs.push(group_run(run, Vec::new()));
            }
        }
    }
    if failed_total > failed_read {
        gaps.push(format!(
            "{failed_total} failed merge-group runs named for #{pr}; read jobs of the latest {failed_read}"
        ));
    }
    runs.sort_by_key(|run| (run.created_at, run.id));
    (runs, listed, unread)
}

/// Workflows of the required jobs failing on the current head or in its
/// latest failed merge group, at most [`MAX_SHARED_WORKFLOWS`].
#[allow(clippy::too_many_arguments)]
fn failing_workflows(
    gh: &Counted<'_>,
    repo: &str,
    history: &RepoHistory,
    pr: u64,
    current_runs: &BTreeMap<u64, u64>,
    branch_runs: &[ListedRun],
    group_listing: &[ListedRun],
    gaps: &mut Vec<String>,
) -> Vec<u64> {
    let mut runs: Vec<u64> = Vec::new();
    if let Some(current) = history.prs.get(&pr).and_then(|entry| entry.heads.last()) {
        for check in current.checks.iter().filter(|check| check.failed()) {
            if let Some(run) = current_runs.get(&check.id)
                && !runs.contains(run)
            {
                runs.push(*run);
            }
        }
    }
    if let Some(group) = history
        .group_runs
        .iter()
        .rev()
        .find(|group| group.pr == Some(pr) && group.gate_failed())
        && !runs.contains(&group.id)
    {
        runs.push(group.id);
    }
    let mut workflows = Vec::new();
    for run in runs {
        let known = branch_runs
            .iter()
            .chain(group_listing)
            .find(|listed| listed.id == run)
            .and_then(|listed| listed.workflow_id);
        let workflow = known.or_else(|| match gh.get(&format!("repos/{repo}/actions/runs/{run}")) {
            Ok(value) => value.get("workflow_id").and_then(Value::as_u64),
            Err(error) => {
                gaps.push(format!(
                    "run {run} unreadable ({error}); its workflow was not searched for shared failures"
                ));
                None
            }
        });
        if let Some(workflow) = workflow
            && !workflows.contains(&workflow)
        {
            workflows.push(workflow);
        }
        if workflows.len() >= MAX_SHARED_WORKFLOWS {
            break;
        }
    }
    workflows
}

struct SharedScan<'a> {
    repo: &'a str,
    pr: u64,
    now: DateTime<Utc>,
    required: &'a [String],
    workflows: &'a [u64],
    open_by_branch: &'a BTreeMap<String, u64>,
}

/// Read other pull requests' failures of this pull request's current failing
/// tests until each has enough other pull requests or the budget runs out.
#[allow(clippy::too_many_lines)]
fn shared_scan(
    gh: &Counted<'_>,
    cache: &ReadCache,
    scan: &SharedScan<'_>,
    history: &mut RepoHistory,
    gaps: &mut Vec<String>,
) {
    let thresholds = Thresholds::default();
    let window = scan.now - Duration::hours(thresholds.pre_existing_window_hours);
    let mut wanted: Vec<(String, String)> = Vec::new();
    let mut want = |check: &CheckFact| {
        for sig in &check.signatures {
            let pair = (check.name.clone(), sig.clone());
            if !wanted.contains(&pair) {
                wanted.push(pair);
            }
        }
    };
    if let Some(current) = history
        .prs
        .get(&scan.pr)
        .and_then(|entry| entry.heads.last())
    {
        current
            .checks
            .iter()
            .filter(|check| check.failed())
            .for_each(&mut want);
    }
    if let Some(group) = history
        .group_runs
        .iter()
        .rev()
        .find(|group| group.pr == Some(scan.pr) && group.gate_failed())
    {
        group
            .required_jobs
            .iter()
            .filter(|job| job.failed())
            .for_each(&mut want);
    }
    if wanted.is_empty() || scan.workflows.is_empty() {
        return;
    }
    let mut candidates: Vec<ListedRun> = Vec::new();
    for workflow in scan.workflows {
        for event in ["pull_request", "merge_group"] {
            match gh.get(&format!(
                "repos/{}/actions/workflows/{workflow}/runs?event={event}&status=failure&created=%3E%3D{}&per_page=100",
                scan.repo,
                stamp(window)
            )) {
                Ok(value) => candidates.extend(listed_runs(&value)),
                Err(error) => gaps.push(format!(
                    "failed {event} runs of workflow {workflow} unreadable ({error}); the shared-failure test did not see them"
                )),
            }
        }
    }
    for run in &mut candidates {
        if run.pr.is_none() {
            run.pr = scan.open_by_branch.get(&run.head_branch).copied();
        }
    }
    candidates.sort_by_key(|run| std::cmp::Reverse((run.created_at, run.id)));
    candidates.dedup_by_key(|run| run.id);
    let unattributed = candidates.iter().filter(|run| run.pr.is_none()).count();
    candidates.retain(|run| run.pr.is_some_and(|other| other != scan.pr));
    let eligible: BTreeSet<u64> = candidates.iter().filter_map(|run| run.pr).collect();

    let mut seen_prs: BTreeSet<u64> = BTreeSet::new();
    let mut matched: BTreeMap<(String, String), BTreeSet<u64>> = BTreeMap::new();
    let settled = |matched: &BTreeMap<(String, String), BTreeSet<u64>>| {
        wanted.iter().all(|pair| {
            matched
                .get(pair)
                .is_some_and(|prs| prs.len() >= thresholds.pre_existing_other_prs)
        })
    };
    let mut inspected = 0usize;
    for run in &candidates {
        if settled(&matched) || inspected >= MAX_SHARED_RUNS {
            break;
        }
        let Some(other) = run.pr else {
            continue;
        };
        if !seen_prs.insert(other) {
            continue;
        }
        inspected += 1;
        let mut jobs = match run_jobs(gh, scan.repo, run.id, scan.required) {
            Ok(jobs) => jobs,
            Err(error) => {
                gaps.push(format!("jobs of run {} unreadable: {error}", run.id));
                continue;
            }
        };
        jobs.retain(CheckFact::failed);
        for job in &mut jobs {
            if !wanted.iter().any(|(name, _)| *name == job.name) {
                continue;
            }
            match signatures(gh, cache, scan.repo, job.id) {
                Ok(found) => job.signatures = found,
                Err(error) => gaps.push(format!("log of job {} unreadable: {error}", job.id)),
            }
            for sig in &job.signatures {
                let key = (job.name.clone(), sig.clone());
                if wanted.contains(&key) {
                    matched.entry(key).or_default().insert(other);
                }
            }
        }
        if jobs.is_empty() {
            continue;
        }
        if merge_group_pr(&run.head_branch).is_some() {
            history.group_runs.push(group_run(run, jobs));
        } else {
            let entry = history.prs.entry(other).or_insert_with(|| PrHistory {
                number: other,
                timeline_complete: true,
                ..PrHistory::default()
            });
            entry.heads.push(HeadFact {
                sha: run.head_sha.clone(),
                first_seen_at: run.created_at,
                gate_runs: Vec::new(),
                checks: jobs,
                merge_base: None,
            });
        }
    }
    history
        .group_runs
        .sort_by_key(|run| (run.created_at, run.id));
    if !settled(&matched) && eligible.len() > inspected {
        gaps.push(format!(
            "shared-failure test inspected {inspected} of {} other failing PRs in {}h; a test not marked shared may still be failing elsewhere",
            eligible.len(),
            thresholds.pre_existing_window_hours
        ));
    }
    if unattributed > 0 {
        gaps.push(format!(
            "{unattributed} failing run(s) named no pull request and were not compared"
        ));
    }
}
