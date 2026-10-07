//! Read one window of GitHub history into a [`RepoHistory`].
//!
//! Every request goes through a [`SyncGhReader`] (argv in, text out), so a
//! test replays recorded responses and nothing here can write. Answers that
//! can no longer change are served from a [`ReadCache`]: the required jobs of
//! a completed run attempt, the failure signatures of a completed job log, the
//! required check runs of a head that is no longer any open pull request's
//! head, and a head's merge base.
//!
//! Reads, in order:
//!
//! 1. required check names: the query's, else branch protection's contexts;
//! 2. pull requests updated in the window, with their queue timelines
//!    (GraphQL search, 50 per page);
//! 3. gate-workflow runs for `pull_request` and `merge_group`, one day of
//!    creation time per listing so no listing meets GitHub's 1000-result cap;
//! 4. required check runs of every head seen;
//! 5. required jobs of every merge-group run that did not succeed;
//! 6. the log of every failed required job, reduced to failure signatures;
//! 7. merge bases, only for heads that could make a flag-4 candidate.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

use super::flags::{Thresholds, gate_cancelled};
use super::{
    CheckFact, GroupRun, HeadFact, PrHistory, QueueEvent, QueueEventKind, RepoHistory, RunFact,
    failure_signatures,
};
use crate::gate_cost::{ReadCache, SyncGhReader, collect_counted, parallel_map, read_pages};
use crate::merge_queue_liveness::merge_group_pr;

/// Pull requests per search page. Each carries up to 100 timeline items.
const SEARCH_PAGE: u32 = 50;
/// Guard against a runaway cursor loop.
const MAX_SEARCH_PAGES: u32 = 40;

/// GraphQL search over pull requests with their queue timelines.
pub const SEARCH_QUERY: &str = "query($q:String!,$cursor:String){search(query:$q,type:ISSUE,first:50,after:$cursor){issueCount pageInfo{hasNextPage endCursor} nodes{... on PullRequest{number title url state createdAt mergedAt closedAt headRefName headRefOid changedFiles commits{totalCount} labels(first:20){nodes{name}} timelineItems(last:100,itemTypes:[HEAD_REF_FORCE_PUSHED_EVENT,ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT,AUTO_MERGE_ENABLED_EVENT,AUTO_MERGE_DISABLED_EVENT,MERGED_EVENT,CLOSED_EVENT,REOPENED_EVENT]){pageInfo{hasPreviousPage} nodes{__typename ... on HeadRefForcePushedEvent{createdAt afterCommit{oid}} ... on AddedToMergeQueueEvent{createdAt} ... on RemovedFromMergeQueueEvent{createdAt reason} ... on AutoMergeEnabledEvent{createdAt} ... on AutoMergeDisabledEvent{createdAt reason} ... on MergedEvent{createdAt} ... on ClosedEvent{createdAt} ... on ReopenedEvent{createdAt}}}}}}}";

/// What to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchQuery {
    /// `OWNER/REPO`.
    pub repo: String,
    /// Base branch.
    pub base: String,
    /// Gate workflow file (hosts the required job that merge groups run).
    pub workflow: String,
    /// Required check names. Empty reads branch protection.
    pub required_checks: Vec<String>,
    /// Window start.
    pub from: DateTime<Utc>,
    /// Window end.
    pub to: DateTime<Utc>,
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn read_json(gh: &SyncGhReader<'_>, args: &[String]) -> Result<Value, String> {
    let text = gh(args)?;
    serde_json::from_str(&text).map_err(|error| {
        format!(
            "gh {}: invalid JSON: {error}",
            args[..args.len().min(3)].join(" ")
        )
    })
}

fn time(value: &Value, key: &str) -> Option<DateTime<Utc>> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&Utc))
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn stamp(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Read the window.
///
/// # Errors
/// When required checks, the pull-request search, or a run listing cannot be
/// read completely. Everything later degrades to a recorded gap.
pub fn gather(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    query: &WatchQuery,
    thresholds: &Thresholds,
) -> Result<RepoHistory, String> {
    let counted = |args: &[String]| {
        cache.note_github_read();
        gh(args)
    };
    let gh: &SyncGhReader<'_> = &counted;
    let required_checks = if query.required_checks.is_empty() {
        read_required_checks(gh, query)?
    } else {
        query.required_checks.clone()
    };
    let mut history = RepoHistory {
        repo: query.repo.clone(),
        base: query.base.clone(),
        from: query.from,
        to: query.to,
        required_checks,
        ..RepoHistory::default()
    };
    history.prs = search_prs(gh, query, &mut history.gaps)?;
    let pr_runs = list_runs(gh, query, "pull_request")?;
    let group_runs = list_runs(gh, query, "merge_group")?;
    attach_heads(&mut history.prs, &pr_runs);
    read_head_checks(gh, cache, &mut history);
    history.group_runs = read_group_runs(
        gh,
        cache,
        query,
        &history.required_checks,
        &group_runs,
        &mut history.gaps,
    );
    read_signatures(gh, cache, &mut history);
    read_merge_bases(gh, cache, &mut history, thresholds);
    Ok(history)
}

/// Branch protection's required contexts.
fn read_required_checks(gh: &SyncGhReader<'_>, query: &WatchQuery) -> Result<Vec<String>, String> {
    let branch = read_json(
        gh,
        &strings(&[
            "api",
            &format!("repos/{}/branches/{}", query.repo, query.base),
        ]),
    )?;
    let contexts: Vec<String> = branch
        .pointer("/protection/required_status_checks/contexts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    if contexts.is_empty() {
        return Err(format!(
            "no required status checks readable on {}:{}; set [pr_watch] required_checks",
            query.repo, query.base
        ));
    }
    Ok(contexts)
}

fn search_prs(
    gh: &SyncGhReader<'_>,
    query: &WatchQuery,
    gaps: &mut Vec<String>,
) -> Result<BTreeMap<u64, PrHistory>, String> {
    let q = format!(
        "repo:{} is:pr base:{} updated:>={} created:<={}",
        query.repo,
        query.base,
        stamp(query.from),
        stamp(query.to)
    );
    let mut prs = BTreeMap::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_SEARCH_PAGES {
        let mut args = strings(&["api", "graphql", "-f"]);
        args.push(format!("query={SEARCH_QUERY}"));
        args.push("-f".to_owned());
        args.push(format!("q={q}"));
        if let Some(cursor) = &cursor {
            args.push("-f".to_owned());
            args.push(format!("cursor={cursor}"));
        }
        let page = read_json(gh, &args)?;
        if let Some(errors) = page.get("errors") {
            return Err(format!("pull-request search: {errors}"));
        }
        let search = page
            .pointer("/data/search")
            .ok_or("pull-request search: no data.search")?;
        for node in search
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(pr) = parse_pr(node) {
                if !pr.timeline_complete {
                    gaps.push(format!(
                        "#{}: timeline has more than 100 queue events; earliest not read",
                        pr.number
                    ));
                }
                prs.insert(pr.number, pr);
            }
        }
        let next = search
            .pointer("/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        cursor = search
            .pointer("/pageInfo/endCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if !next || cursor.is_none() {
            let total = search
                .get("issueCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if total > 1000 {
                gaps.push(format!(
                    "search matched {total} pull requests; GitHub returns at most 1000"
                ));
            }
            return Ok(prs);
        }
    }
    let _ = SEARCH_PAGE;
    gaps.push("pull-request search stopped at the page limit".to_owned());
    Ok(prs)
}

/// One search node into a [`PrHistory`] without heads.
#[must_use]
pub fn parse_pr(node: &Value) -> Option<PrHistory> {
    let number = node.get("number").and_then(Value::as_u64)?;
    let timeline = node.get("timelineItems");
    let mut events = Vec::new();
    for item in timeline
        .and_then(|t| t.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(at) = time(item, "createdAt") else {
            continue;
        };
        let kind = match item.get("__typename").and_then(Value::as_str) {
            Some("AutoMergeEnabledEvent") => QueueEventKind::Armed,
            Some("AutoMergeDisabledEvent") => QueueEventKind::Disarmed {
                reason: text(item, "reason"),
            },
            Some("AddedToMergeQueueEvent") => QueueEventKind::Enqueued,
            Some("RemovedFromMergeQueueEvent") => QueueEventKind::Removed {
                reason: text(item, "reason")
                    .unwrap_or_default()
                    .to_ascii_lowercase(),
            },
            Some("HeadRefForcePushedEvent") => QueueEventKind::ForcePushed {
                after: item
                    .pointer("/afterCommit/oid")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            Some("MergedEvent") => QueueEventKind::Merged,
            Some("ClosedEvent") => QueueEventKind::Closed,
            Some("ReopenedEvent") => QueueEventKind::Reopened,
            _ => continue,
        };
        events.push(QueueEvent { at, kind });
    }
    events.sort_by_key(|event| event.at);
    let timeline_complete = !timeline
        .and_then(|t| t.pointer("/pageInfo/hasPreviousPage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(PrHistory {
        number,
        title: text(node, "title").unwrap_or_default(),
        url: text(node, "url").unwrap_or_default(),
        created_at: time(node, "createdAt"),
        merged_at: time(node, "mergedAt"),
        closed_at: time(node, "closedAt"),
        changed_files: node
            .get("changedFiles")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        commits: node
            .pointer("/commits/totalCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        head_ref: text(node, "headRefName").unwrap_or_default(),
        head_sha: text(node, "headRefOid").unwrap_or_default(),
        labels: node
            .pointer("/labels/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|label| text(label, "name"))
            .collect(),
        heads: Vec::new(),
        events,
        timeline_complete,
    })
}

/// One listed workflow run.
#[derive(Clone, Debug)]
struct ListedRun {
    id: u64,
    head_branch: String,
    head_sha: String,
    created_at: DateTime<Utc>,
    updated_at: Option<DateTime<Utc>>,
    status: String,
    conclusion: Option<String>,
    attempt: u64,
}

fn list_runs(
    gh: &SyncGhReader<'_>,
    query: &WatchQuery,
    event: &str,
) -> Result<Vec<ListedRun>, String> {
    let mut by_id: BTreeMap<u64, ListedRun> = BTreeMap::new();
    let mut start = query.from;
    while start < query.to {
        let end = (start + Duration::days(1)).min(query.to);
        let pages = read_pages(
            gh,
            &format!(
                "repos/{}/actions/workflows/{}/runs",
                query.repo, query.workflow
            ),
            &[
                format!("event={event}"),
                format!("created={}..{}", stamp(start), stamp(end)),
                "per_page=100".to_owned(),
            ],
        )?;
        let runs = collect_counted(
            &pages,
            "workflow_runs",
            &format!("`{event}` runs {}..{}", stamp(start), stamp(end)),
        )?;
        for run in runs {
            let (Some(id), Some(created_at)) = (
                run.get("id").and_then(Value::as_u64),
                time(&run, "created_at"),
            ) else {
                continue;
            };
            by_id.insert(
                id,
                ListedRun {
                    id,
                    head_branch: text(&run, "head_branch").unwrap_or_default(),
                    head_sha: text(&run, "head_sha").unwrap_or_default(),
                    created_at,
                    updated_at: time(&run, "updated_at"),
                    status: text(&run, "status").unwrap_or_default(),
                    conclusion: text(&run, "conclusion"),
                    attempt: run.get("run_attempt").and_then(Value::as_u64).unwrap_or(1),
                },
            );
        }
        start = end;
    }
    Ok(by_id.into_values().collect())
}

/// Attach `pull_request` runs (and force-push events) to pull requests as
/// heads, by head branch and lifetime.
fn attach_heads(prs: &mut BTreeMap<u64, PrHistory>, runs: &[ListedRun]) {
    let slack = Duration::minutes(2);
    let mut by_branch: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for pr in prs.values() {
        by_branch
            .entry(pr.head_ref.as_str())
            .or_default()
            .push(pr.number);
    }
    let mut assigned: BTreeMap<u64, Vec<&ListedRun>> = BTreeMap::new();
    for run in runs {
        let Some(candidates) = by_branch.get(run.head_branch.as_str()) else {
            continue;
        };
        let owner = candidates.iter().copied().find(|number| {
            let pr = &prs[number];
            pr.created_at
                .is_none_or(|created| run.created_at + slack >= created)
                && pr
                    .closed_at
                    .is_none_or(|closed| run.created_at <= closed + slack)
        });
        if let Some(owner) = owner {
            assigned.entry(owner).or_default().push(run);
        }
    }
    for pr in prs.values_mut() {
        let mut heads: BTreeMap<String, HeadFact> = BTreeMap::new();
        for run in assigned.get(&pr.number).into_iter().flatten() {
            let head = heads
                .entry(run.head_sha.clone())
                .or_insert_with(|| HeadFact {
                    sha: run.head_sha.clone(),
                    first_seen_at: run.created_at,
                    ..HeadFact::default()
                });
            head.first_seen_at = head.first_seen_at.min(run.created_at);
            head.gate_runs.push(RunFact {
                id: run.id,
                created_at: run.created_at,
                status: run.status.clone(),
                conclusion: run.conclusion.clone(),
            });
        }
        for event in &pr.events {
            if let QueueEventKind::ForcePushed { after: Some(after) } = &event.kind {
                let head = heads.entry(after.clone()).or_insert_with(|| HeadFact {
                    sha: after.clone(),
                    first_seen_at: event.at,
                    ..HeadFact::default()
                });
                head.first_seen_at = head.first_seen_at.min(event.at);
            }
        }
        if !pr.head_sha.is_empty()
            && heads.is_empty()
            && let Some(created) = pr.created_at
        {
            heads.insert(
                pr.head_sha.clone(),
                HeadFact {
                    sha: pr.head_sha.clone(),
                    first_seen_at: created,
                    ..HeadFact::default()
                },
            );
        }
        let mut heads: Vec<HeadFact> = heads.into_values().collect();
        for head in &mut heads {
            head.gate_runs.sort_by_key(|run| (run.created_at, run.id));
        }
        heads.sort_by(|a, b| {
            a.first_seen_at
                .cmp(&b.first_seen_at)
                .then_with(|| a.sha.cmp(&b.sha))
        });
        pr.heads = heads;
    }
}

fn check_fact(value: &Value, name: String) -> Option<CheckFact> {
    Some(CheckFact {
        name,
        id: value.get("id").and_then(Value::as_u64)?,
        status: text(value, "status").unwrap_or_default(),
        conclusion: text(value, "conclusion"),
        started_at: time(value, "started_at"),
        completed_at: time(value, "completed_at"),
        signatures: Vec::new(),
        // Present (possibly null) only on an Actions job, never on a check run.
        runner_name: value
            .get("runner_name")
            .map(|name| name.as_str().unwrap_or_default().to_owned()),
    })
}

fn checks_to_value(checks: &[CheckFact]) -> Value {
    Value::Array(
        checks
            .iter()
            .map(|check| {
                let mut value = json!({
                    "id": check.id,
                    "name": check.name,
                    "status": check.status,
                    "conclusion": check.conclusion,
                    "started_at": check.started_at.map(stamp),
                    "completed_at": check.completed_at.map(stamp),
                });
                if let Some(runner) = &check.runner_name {
                    value["runner_name"] = json!(runner);
                }
                value
            })
            .collect(),
    )
}

fn checks_from_value(value: &Value) -> Vec<CheckFact> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| check_fact(item, text(item, "name")?))
        .collect()
}

fn required_from(items: &[Value], required: &[String], name_key: &str) -> Vec<CheckFact> {
    items
        .iter()
        .filter_map(|item| {
            let name = text(item, name_key)?;
            required
                .contains(&name)
                .then(|| check_fact(item, name))
                .flatten()
        })
        .collect()
}

/// Required check runs of every head. Cached once every required check on the
/// head is complete and the head can no longer be re-run into relevance: it is
/// not the current head of an open pull request.
fn read_head_checks(gh: &SyncGhReader<'_>, cache: &ReadCache, history: &mut RepoHistory) {
    let required = history.required_checks.clone();
    let repo = history.repo.clone();
    let mut work: Vec<(u64, usize, String, bool)> = Vec::new();
    for pr in history.prs.values() {
        let open = pr.merged_at.is_none() && pr.closed_at.is_none();
        for (index, head) in pr.heads.iter().enumerate() {
            let settled_head = !open || head.sha != pr.head_sha;
            work.push((pr.number, index, head.sha.clone(), settled_head));
        }
    }
    let results = parallel_map(&work, |(_, _, sha, settled_head)| {
        let key = format!("pr-watch:check-runs:{repo}:{sha}");
        if *settled_head && let Some(value) = cache.get(&key) {
            return Ok(checks_from_value(&value));
        }
        let pages = read_pages(
            gh,
            &format!("repos/{repo}/commits/{sha}/check-runs"),
            &["filter=all".to_owned(), "per_page=100".to_owned()],
        )?;
        let items = collect_counted(&pages, "check_runs", &format!("check runs of {sha}"))?;
        let checks = required_from(&items, &required, "name");
        if *settled_head && checks.iter().all(|check| check.status == "completed") {
            cache.put(&key, &checks_to_value(&checks));
        }
        Ok::<_, String>(checks)
    });
    for ((number, index, sha, _), result) in work.iter().zip(results) {
        match result {
            Ok(checks) => {
                if let Some(head) = history
                    .prs
                    .get_mut(number)
                    .and_then(|pr| pr.heads.get_mut(*index))
                {
                    head.checks = checks;
                }
            }
            Err(error) => history.gaps.push(format!(
                "#{number} head {}: check runs unreadable: {error}",
                super::short(sha)
            )),
        }
    }
}

fn read_group_runs(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    query: &WatchQuery,
    required: &[String],
    runs: &[ListedRun],
    gaps: &mut Vec<String>,
) -> Vec<GroupRun> {
    let results = parallel_map(runs, |run| {
        let mut group = GroupRun {
            id: run.id,
            pr: merge_group_pr(&run.head_branch),
            head_sha: run.head_sha.clone(),
            parent_sha: run
                .head_branch
                .rsplit_once('-')
                .map(|(_, sha)| sha.to_owned())
                .filter(|sha| sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit())),
            created_at: run.created_at,
            completed_at: (run.status == "completed")
                .then_some(run.updated_at)
                .flatten(),
            conclusion: run.conclusion.clone(),
            required_jobs: Vec::new(),
            attribution: None,
        };
        let needs_jobs = run.status == "completed" && run.conclusion.as_deref() == Some("failure");
        if !needs_jobs {
            return Ok(group);
        }
        let key = format!(
            "pr-watch:group-jobs-v2:{}:{}:attempt-{}",
            query.repo, run.id, run.attempt
        );
        if let Some(value) = cache.get(&key) {
            group.required_jobs = checks_from_value(&value);
            return Ok(group);
        }
        let pages = read_pages(
            gh,
            &format!("repos/{}/actions/runs/{}/jobs", query.repo, run.id),
            &["filter=all".to_owned(), "per_page=100".to_owned()],
        )?;
        let jobs = collect_counted(&pages, "jobs", &format!("jobs of run {}", run.id))?;
        group.required_jobs = required_from(&jobs, required, "name");
        if group
            .required_jobs
            .iter()
            .all(|job| job.status == "completed")
        {
            cache.put(&key, &checks_to_value(&group.required_jobs));
        }
        Ok::<_, String>(group)
    });
    let mut out = Vec::new();
    for (run, result) in runs.iter().zip(results) {
        match result {
            Ok(group) => out.push(group),
            Err(error) => gaps.push(format!(
                "merge-group run {}: jobs unreadable: {error}",
                run.id
            )),
        }
    }
    out.sort_by_key(|run| (run.created_at, run.id));
    out
}

/// Failure signatures for every failed required job, from its log.
fn read_signatures(gh: &SyncGhReader<'_>, cache: &ReadCache, history: &mut RepoHistory) {
    let repo = history.repo.clone();
    let mut ids: BTreeSet<u64> = BTreeSet::new();
    for pr in history.prs.values() {
        for head in &pr.heads {
            ids.extend(head.checks.iter().filter(|c| c.failed()).map(|c| c.id));
        }
    }
    for run in &history.group_runs {
        ids.extend(
            run.required_jobs
                .iter()
                .filter(|c| c.failed())
                .map(|c| c.id),
        );
    }
    let ids: Vec<u64> = ids.into_iter().collect();
    let results = parallel_map(&ids, |id| {
        let key = format!("pr-watch:signatures:{repo}:{id}");
        if let Some(Value::Array(items)) = cache.get(&key) {
            return Ok(items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect());
        }
        let log = gh(&strings(&[
            "api",
            &format!("repos/{repo}/actions/jobs/{id}/logs"),
        ]))?;
        let signatures = failure_signatures(&log);
        cache.put(
            &key,
            &Value::Array(signatures.iter().cloned().map(Value::String).collect()),
        );
        Ok::<Vec<String>, String>(signatures)
    });
    let mut by_id: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    let mut unreadable = 0_u64;
    for (id, result) in ids.iter().zip(results) {
        match result {
            Ok(signatures) => {
                by_id.insert(*id, signatures);
            }
            Err(_) => unreadable += 1,
        }
    }
    if unreadable > 0 {
        history.gaps.push(format!(
            "{unreadable} failed job log(s) unreadable; their signatures are unknown"
        ));
    }
    let apply = |check: &mut CheckFact| {
        if let Some(signatures) = by_id.get(&check.id) {
            check.signatures.clone_from(signatures);
        }
    };
    for pr in history.prs.values_mut() {
        for head in &mut pr.heads {
            head.checks
                .iter_mut()
                .filter(|c| c.failed())
                .for_each(apply);
        }
    }
    for run in &mut history.group_runs {
        run.required_jobs
            .iter_mut()
            .filter(|c| c.failed())
            .for_each(apply);
    }
}

/// Merge bases for heads adjacent to a cancelled-gate replacement, only on
/// pull requests with enough such replacements inside one flag-4 window to
/// matter. A head already reachable from the base branch (it merged) has no
/// useful merge base and stays unknown.
fn read_merge_bases(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    history: &mut RepoHistory,
    thresholds: &Thresholds,
) {
    let window = Duration::hours(thresholds.replacement_window_hours);
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    for pr in history.prs.values() {
        let times: Vec<(DateTime<Utc>, usize)> = pr
            .heads
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| gate_cancelled(&pair[0]))
            .map(|(index, pair)| (pair[1].first_seen_at, index))
            .collect();
        for (at, _) in &times {
            let inside: Vec<usize> = times
                .iter()
                .filter(|(other, _)| *other <= *at && *other > *at - window)
                .map(|(_, index)| *index)
                .collect();
            if inside.len() >= thresholds.replacements {
                for index in inside {
                    wanted.insert(pr.heads[index].sha.clone());
                    wanted.insert(pr.heads[index + 1].sha.clone());
                }
            }
        }
    }
    let repo = history.repo.clone();
    let base = history.base.clone();
    let shas: Vec<String> = wanted.into_iter().collect();
    let results = parallel_map(&shas, |sha| {
        let key = format!("pr-watch:merge-base:{repo}:{base}:{sha}");
        if let Some(Value::String(found)) = cache.get(&key) {
            return Ok(Some(found));
        }
        let answer = read_json(
            gh,
            &strings(&[
                "api",
                &format!("repos/{repo}/compare/{base}...{sha}"),
                "--jq",
                "{status: .status, merge_base: .merge_base_commit.sha}",
            ]),
        )?;
        let status = text(&answer, "status").unwrap_or_default();
        let merge_base = text(&answer, "merge_base");
        // `behind`/`identical`: the head is already in the base branch, so the
        // merge base is the head itself and says nothing about when it was
        // pushed. Unknown, and not cached.
        if matches!(status.as_str(), "ahead" | "diverged") {
            if let Some(found) = &merge_base {
                cache.put(&key, &Value::String(found.clone()));
            }
            return Ok(merge_base);
        }
        Ok::<Option<String>, String>(None)
    });
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    let mut unreadable = 0_u64;
    for (sha, result) in shas.iter().zip(results) {
        match result {
            Ok(Some(merge_base)) => {
                found.insert(sha.clone(), merge_base);
            }
            Ok(None) => {}
            Err(_) => unreadable += 1,
        }
    }
    if unreadable > 0 {
        history
            .gaps
            .push(format!("{unreadable} merge base(s) unreadable"));
    }
    for pr in history.prs.values_mut() {
        for head in &mut pr.heads {
            if let Some(merge_base) = found.get(&head.sha) {
                head.merge_base = Some(merge_base.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{check_fact, checks_from_value, checks_to_value};
    use serde_json::json;

    #[test]
    fn a_job_without_a_runner_stays_distinct_from_a_check_run_through_the_cache() {
        let job = json!({"id": 1, "status": "completed", "conclusion": "failure",
            "started_at": null, "completed_at": "2026-10-06T12:00:00Z", "runner_name": null});
        let ran = json!({"id": 2, "status": "completed", "conclusion": "failure",
            "started_at": "2026-10-06T11:40:00Z", "completed_at": "2026-10-06T12:00:00Z",
            "runner_name": "pulp-gate-m3-1"});
        let check_run = json!({"id": 3, "status": "completed", "conclusion": "failure",
            "started_at": "2026-10-06T11:40:00Z", "completed_at": "2026-10-06T12:00:00Z"});
        let facts = [
            check_fact(&job, "macos".to_owned()).unwrap(),
            check_fact(&ran, "macos".to_owned()).unwrap(),
            check_fact(&check_run, "macos".to_owned()).unwrap(),
        ];
        assert_eq!(facts[0].runner_name.as_deref(), Some(""));
        assert!(facts[0].never_ran());
        assert_eq!(facts[1].runner_name.as_deref(), Some("pulp-gate-m3-1"));
        assert!(!facts[1].never_ran());
        assert_eq!(facts[2].runner_name, None);
        assert!(!facts[2].never_ran());
        // The cache round trip keeps all three apart: an unknown runner must
        // not come back as "never ran", nor the reverse.
        assert_eq!(checks_from_value(&checks_to_value(&facts)), facts);
    }
}
