//! GitHub reads for the rules watcher: review trails, alarm issues, and
//! input-event files. Every request goes through a [`SyncGhReader`], so a
//! recording replays offline and nothing here can write.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{Alarm, InputEvent, TrailEntry, Verdict};
use crate::gate_cost::{SyncGhReader, read_pages};

/// Pull requests per batched review query.
const REVIEW_BATCH: usize = 25;
/// Title prefix of tartci's host alarm issues.
pub const ALARM_PREFIX: &str = "[tartci]";

fn time(value: &Value, key: &str) -> Option<DateTime<Utc>> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&Utc))
}

fn stamp(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn items(pages: Vec<Value>) -> Vec<Value> {
    pages
        .into_iter()
        .flat_map(|page| match page {
            Value::Array(items) => items,
            other => vec![other],
        })
        .collect()
}

/// The pull request an issue-comment `issue_url` names.
fn issue_number(comment: &Value) -> Option<u64> {
    comment
        .get("issue_url")
        .and_then(Value::as_str)?
        .rsplit('/')
        .next()?
        .parse()
        .ok()
}

/// Review trails of `prs`: every comment updated since `since` (one
/// repository-wide listing) and every review (batched GraphQL).
///
/// # Errors
/// When a listing cannot be read; a partial trail would read as "no verdict".
pub fn read_trails(
    gh: &SyncGhReader<'_>,
    repo: &str,
    prs: &BTreeSet<u64>,
    since: DateTime<Utc>,
) -> Result<BTreeMap<u64, Vec<TrailEntry>>, String> {
    let mut trails: BTreeMap<u64, Vec<TrailEntry>> = BTreeMap::new();
    if prs.is_empty() {
        return Ok(trails);
    }
    let pages = read_pages(
        gh,
        &format!("repos/{repo}/issues/comments"),
        &[format!("since={}", stamp(since)), "per_page=100".to_owned()],
    )?;
    for comment in items(pages) {
        let Some(number) = issue_number(&comment).filter(|number| prs.contains(number)) else {
            continue;
        };
        if let Some(entry) = comment_entry(&comment) {
            trails.entry(number).or_default().push(entry);
        }
    }
    read_reviews(gh, repo, prs, &mut trails)?;
    for trail in trails.values_mut() {
        trail.sort_by_key(|entry| (entry.at, entry.id));
    }
    Ok(trails)
}

/// One pull request's whole review trail: every comment it ever had, not just
/// those updated in the window, and its reviews. Read when an event about it
/// would fire, so an old verdict on a still-current head is not missed.
///
/// # Errors
/// When a listing cannot be read.
pub fn read_pr_trail(
    gh: &SyncGhReader<'_>,
    repo: &str,
    pr: u64,
) -> Result<Vec<TrailEntry>, String> {
    let pages = read_pages(
        gh,
        &format!("repos/{repo}/issues/{pr}/comments"),
        &["per_page=100".to_owned()],
    )?;
    let mut trails: BTreeMap<u64, Vec<TrailEntry>> = BTreeMap::new();
    let mut trail: Vec<TrailEntry> = items(pages).iter().filter_map(comment_entry).collect();
    read_reviews(gh, repo, &BTreeSet::from([pr]), &mut trails)?;
    trail.extend(trails.remove(&pr).unwrap_or_default());
    trail.sort_by_key(|entry| (entry.at, entry.id));
    Ok(trail)
}

/// A comment as a trail entry, when it carries a verdict or a READY.
fn comment_entry(comment: &Value) -> Option<TrailEntry> {
    let id = comment.get("id").and_then(Value::as_u64)?;
    let at = time(comment, "created_at")?;
    let login = comment
        .pointer("/user/login")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let body = comment
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let entry = TrailEntry::from_comment(id, login, at, body);
    (entry.verdict.is_some() || entry.ready_head.is_some()).then_some(entry)
}

/// Every review of `prs`, batched [`REVIEW_BATCH`] to a GraphQL query, into
/// `trails`.
fn read_reviews(
    gh: &SyncGhReader<'_>,
    repo: &str,
    prs: &BTreeSet<u64>,
    trails: &mut BTreeMap<u64, Vec<TrailEntry>>,
) -> Result<(), String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository {repo:?}"))?;
    let numbers: Vec<u64> = prs.iter().copied().collect();
    for batch in numbers.chunks(REVIEW_BATCH) {
        let mut fields = String::new();
        for number in batch {
            let _ = write!(
                fields,
                "p{number}:pullRequest(number:{number}){{reviews(last:50){{nodes{{databaseId author{{login}} state submittedAt commit{{oid}}}}}}}}"
            );
        }
        let query = format!("query{{repository(owner:\"{owner}\",name:\"{name}\"){{{fields}}}}}");
        let text = gh(&[
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={query}"),
        ])?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| format!("reviews: invalid JSON: {error}"))?;
        if let Some(errors) = value.get("errors") {
            return Err(format!("reviews: {errors}"));
        }
        for number in batch {
            let nodes = value
                .pointer(&format!("/data/repository/p{number}/reviews/nodes"))
                .and_then(Value::as_array);
            for node in nodes.into_iter().flatten() {
                if let Some(entry) = review_entry(node) {
                    trails.entry(*number).or_default().push(entry);
                }
            }
        }
    }
    Ok(())
}

/// An `APPROVED` or `CHANGES_REQUESTED` review as a verdict on its commit.
#[must_use]
pub fn review_entry(node: &Value) -> Option<TrailEntry> {
    let approve = match node.get("state").and_then(Value::as_str)? {
        "APPROVED" => true,
        "CHANGES_REQUESTED" => false,
        _ => return None,
    };
    Some(TrailEntry {
        id: node.get("databaseId").and_then(Value::as_u64)?,
        login: node
            .pointer("/author/login")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        at: time(node, "submittedAt")?,
        review: true,
        verdict: Some(Verdict {
            approve,
            head: node
                .pointer("/commit/oid")
                .and_then(Value::as_str)?
                .to_owned(),
        }),
        ready_head: None,
    })
}

/// Alarm issues (`[tartci] ...`) in `repo` updated since `since`.
///
/// # Errors
/// When the listing cannot be read.
pub fn read_alarms(
    gh: &SyncGhReader<'_>,
    repo: &str,
    since: DateTime<Utc>,
) -> Result<Vec<Alarm>, String> {
    let pages = read_pages(
        gh,
        &format!("repos/{repo}/issues"),
        &[
            "state=all".to_owned(),
            format!("since={}", stamp(since)),
            "per_page=100".to_owned(),
        ],
    )?;
    let mut alarms: Vec<Alarm> = items(pages)
        .into_iter()
        .filter(|issue| issue.get("pull_request").is_none())
        .filter_map(|issue| {
            let title = issue.get("title").and_then(Value::as_str)?;
            if !title.starts_with(ALARM_PREFIX) {
                return None;
            }
            Some(Alarm {
                repo: repo.to_owned(),
                number: issue.get("number").and_then(Value::as_u64)?,
                title: title.to_owned(),
                opened_at: time(&issue, "created_at")?,
                closed_at: time(&issue, "closed_at"),
            })
        })
        .collect();
    alarms.sort_by_key(|alarm| (alarm.opened_at, alarm.number));
    Ok(alarms)
}

/// Input events, one JSON object per line with at least `kind`, `key`, `at`.
///
/// # Errors
/// When the file cannot be read or a non-blank line is not such an object.
pub fn read_inputs(path: &Path) -> Result<Vec<InputEvent>, String> {
    let raw =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    raw.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .map_err(|error| format!("{}:{}: {error}", path.display(), index + 1))
        })
        .collect()
}

/// What remains of the GitHub App's hourly budget for one repository's
/// installation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateBudget {
    /// Core (REST) requests left.
    pub core: u64,
    /// GraphQL points left.
    pub graphql: u64,
    /// When the sooner-exhausted budget resets.
    pub reset: Option<DateTime<Utc>>,
}

impl RateBudget {
    /// The smaller of the two budgets.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.core.min(self.graphql)
    }
}

/// Read the App's remaining budget (`gh api rate_limit`, which costs none).
///
/// # Errors
/// When the answer cannot be read.
pub fn read_rate_budget(gh: &SyncGhReader<'_>) -> Result<RateBudget, String> {
    let text = gh(&["api".to_owned(), "rate_limit".to_owned()])?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("rate_limit: invalid JSON: {error}"))?;
    let field = |resource: &str, key: &str| {
        value
            .pointer(&format!("/resources/{resource}/{key}"))
            .and_then(Value::as_u64)
    };
    let core = field("core", "remaining").ok_or("rate_limit: no core budget")?;
    let graphql = field("graphql", "remaining").ok_or("rate_limit: no graphql budget")?;
    let reset_of = |resource: &str| {
        field(resource, "reset")
            .and_then(|epoch| i64::try_from(epoch).ok())
            .and_then(|epoch| DateTime::from_timestamp(epoch, 0))
    };
    let reset = if core <= graphql {
        reset_of("core")
    } else {
        reset_of("graphql")
    };
    Ok(RateBudget {
        core,
        graphql,
        reset,
    })
}

/// Whether a read failed on GitHub's rate limit (primary or secondary).
#[must_use]
pub fn rate_limited(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("rate limit") || error.contains("abuse detection")
}
