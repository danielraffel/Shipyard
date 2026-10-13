//! Success-rate scorecard: per ISO week, how long merged pull requests took,
//! how often the merge queue ejected one, how often a push turned a required
//! check red, and which failing checks recur across pull requests.
//!
//! Like the rest of [`crate::gate_cost`] it only reads GitHub, so any past
//! window can be measured again, and it is split so the arithmetic can be
//! proven on captured answers: [`gather`] reads a [`ScorecardObservation`]
//! and [`compute`] turns it into a [`ScorecardReport`].
//!
//! Definitions (the same text is in `shipyard metrics gate-cost --help`):
//!
//! - **open to merge**: `mergedAt - createdAt` of every pull request merged
//!   into the base in the week, in hours; median and p90.
//! - **ejection rate**: queue removals GitHub made because checks failed
//!   (`RemovedFromMergeQueueEvent` by `github-merge-queue` with reason
//!   `failed_checks` or `checks_timed_out`) over queue entries
//!   (`AddedToMergeQueueEvent`), both dated in the week. It covers every
//!   required check, since GitHub's reason does. Other removals (`merged`,
//!   `merge_conflict`, `manual`) are reported by reason and actor, never as
//!   ejections. The pull requests read are those a merge-group run in the
//!   window names, plus those merged in it: an entry removed before any
//!   merge-group run was built for it is seen only through another run of the
//!   same pull request.
//! - **merge-group gate failure rate**: merge-group runs of the workflow whose
//!   final attempt of the gate job did not pass, over runs whose final gate
//!   attempt completed. It covers that one job, not the queue's other
//!   required checks, and a failed or cancelled group is not an ejection when
//!   the queue rebuilds it and the pull request stays queued. A run whose own
//!   conclusion is `failure` while its gate passed is counted apart.
//! - **red after push**: among the merged pull requests' commits that carry
//!   at least one required context, the share on which a required context is
//!   red (check run `failure`, `timed_out`, `startup_failure` or
//!   `action_required`; status `failure` or `error`). The latest check run of
//!   each name decides, as it does for GitHub's required checks, so a red
//!   re-run green on the same commit reads green. `cancelled` is not red
//!   (a newer push cancels the older head's runs) and is counted apart.
//! - **repeat failures**: check names red on commits of at least two
//!   different merged pull requests in the week, with those pull requests.
//! - **merge-head verdict**: for each merged pull request, the latest
//!   structured verdict naming its merge head (`headRefOid`) posted at or
//!   before `mergedAt`: a comment whose last line is `verdict: APPROVE|HOLD
//!   head:<sha>` or `reviewed: <sha>`, or an `APPROVED`/`CHANGES_REQUESTED`
//!   review on that commit, from the fleet App or an `OWNER`, `MEMBER` or
//!   `COLLABORATOR` who is not the author (the queue-arm guard's rule).
//!   `approve`, `hold` (merged anyway), `none`, or `unread` when GitHub listed
//!   only some of the comments or reviews and none of those decided. A
//!   comment is judged by its current body, so one posted before the merge
//!   but edited after it (`lastEditedAt`), and a review on the merge head
//!   since dismissed, are unknown: they decide nothing, and when one is at
//!   least as new as the verdict that would decide, the pull request reads
//!   `unread` rather than guessing what it said at the merge.
//!
//! Required contexts come from the base branch's classic protection, or from
//! `--required-context` when given (a repository without protection has
//! none). They are today's settings applied to the whole window.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use serde::Serialize;
use serde_json::Value;

use super::proxy::ejection_cause;
use super::{
    GateCostQuery, GateJobSample, MERGE_GROUP_EVENT, ReadCache, RunRef, SyncGhReader, TelemetryGap,
    collect_counted, cross_check_listing, parallel_map, percentile, read_json, read_pages,
    read_run_jobs, resolved_gate_jobs, round2, strings, text, timestamp,
};

pub mod cases;
pub mod flakes;

/// Report schema identifier.
pub const SCHEMA: &str = "shipyard-gate-cost-scorecard/v1";
/// GitHub search returns at most this many results for one query.
const SEARCH_CAP: u64 = 1000;
/// Search slice length: one day keeps every slice far under [`SEARCH_CAP`].
const SLICE_HOURS: i64 = 24;
/// GitHub lists at most this many commits of a pull request; #9755's 300
/// commits were answered as 250 of `totalCount` 300.
const PR_COMMITS_CAP: u64 = 250;
/// Reads of one GraphQL listing when GitHub answers a gateway error.
const GRAPHQL_ATTEMPTS: u32 = 3;
#[cfg(not(test))]
const GRAPHQL_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const GRAPHQL_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(0);
/// Characters of a failed read's message kept in the error; `gh` puts the
/// pages it did read in front of the reason.
const ERROR_TAIL: usize = 400;

const PRS_QUERY: &str = "query($q:String!,$endCursor:String){search(query:$q,type:ISSUE,first:100,after:$endCursor){issueCount pageInfo{hasNextPage endCursor} nodes{... on PullRequest{number createdAt mergedAt}}}}";
/// Commits of one pull request, 25 to a page: a page of whole pull requests
/// with their checks timed out (HTTP 504) on an 85-commit pull request.
const COMMITS_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!,$endCursor:String){repository(owner:$owner,name:$name){pullRequest(number:$number){commits(first:25,after:$endCursor){totalCount pageInfo{hasNextPage endCursor} nodes{commit{oid statusCheckRollup{contexts(first:100){totalCount pageInfo{hasNextPage endCursor} nodes{__typename ... on CheckRun{name conclusion startedAt completedAt} ... on StatusContext{context state createdAt}}}}}}}}}}";
const TIMELINE_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!,$endCursor:String){repository(owner:$owner,name:$name){pullRequest(number:$number){timelineItems(itemTypes:[ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT],first:100,after:$endCursor){filteredCount pageInfo{hasNextPage endCursor} nodes{__typename ... on AddedToMergeQueueEvent{createdAt actor{login}} ... on RemovedFromMergeQueueEvent{createdAt reason actor{login}}}}}}}";
/// A merged pull request's head, author, and last comments and reviews, for
/// the verdict on its merge head.
const VERDICTS_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid author{login} comments(last:100){totalCount nodes{author{login} authorAssociation body createdAt lastEditedAt}} reviews(last:100){totalCount nodes{author{login} authorAssociation state submittedAt commit{oid}}}}}}";
const CONTEXTS_QUERY: &str = "query($owner:String!,$name:String!,$oid:GitObjectID!,$endCursor:String){repository(owner:$owner,name:$name){object(oid:$oid){... on Commit{statusCheckRollup{contexts(first:100,after:$endCursor){totalCount pageInfo{hasNextPage endCursor} nodes{__typename ... on CheckRun{name conclusion startedAt completedAt} ... on StatusContext{context state createdAt}}}}}}}}";

/// What to measure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScorecardQuery {
    /// `OWNER/REPO`.
    pub repo: String,
    /// Workflow hosting the gate job, for merge-queue ejections.
    pub workflow: String,
    /// Exact gate job name.
    pub gate_job: String,
    /// Branch pull requests merge into.
    pub base_branch: String,
    /// Required contexts to use instead of the branch protection's.
    pub required_contexts: Vec<String>,
    /// Gate-job steps that run a pull-request head's test selection; when
    /// empty, selector recall is not measured.
    pub selection_steps: Vec<String>,
    /// Window start, inclusive.
    pub from: DateTime<Utc>,
    /// Window end, exclusive.
    pub to: DateTime<Utc>,
}

/// One check on a commit, as the latest run of its name left it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Check-run name or status context.
    pub name: String,
    /// Upper-case conclusion or state; `None` while pending.
    pub outcome: Option<String>,
    /// When it completed (else started); a status context's creation time.
    pub at: Option<DateTime<Utc>>,
}

/// One commit of a merged pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrCommit {
    /// Commit sha.
    pub oid: String,
    /// Its checks; empty when the commit carried none.
    pub checks: Vec<CheckOutcome>,
}

/// One merged pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedPr {
    /// Number.
    pub number: u64,
    /// Opened.
    pub created_at: DateTime<Utc>,
    /// Merged.
    pub merged_at: DateTime<Utc>,
    /// Its commits with their checks: every commit, or the first
    /// [`PR_COMMITS_CAP`] when GitHub lists no more.
    pub commits: Vec<PrCommit>,
    /// Commits the pull request has (`totalCount`).
    pub commits_total: u64,
    /// The latest trusted verdict on the merge head before the merge.
    pub verdict: MergeVerdict,
}

/// The latest trusted verdict naming a merged pull request's merge head,
/// posted at or before the merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeVerdict {
    /// An approval decided.
    Approve,
    /// A HOLD decided: the pull request merged anyway.
    Hold,
    /// No trusted verdict named the merge head.
    None,
    /// GitHub listed only some comments or reviews, and none of those decided.
    Unread,
}

/// One merge-group run and its final gate attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeGroupRun {
    /// Run id.
    pub run_id: u64,
    /// Run creation time; decides the week.
    pub created_at: DateTime<Utc>,
    /// The run's own conclusion.
    pub conclusion: Option<String>,
    /// The highest attempt of the gate job, when the run has one.
    pub gate: Option<GateJobSample>,
    /// That attempt's job id.
    pub gate_job_id: Option<u64>,
}

/// What happened to a pull request in the merge queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueEventKind {
    /// `AddedToMergeQueueEvent`.
    Added,
    /// `RemovedFromMergeQueueEvent`.
    Removed {
        /// GitHub's reason (`failed_checks`, `merged`, `manual`, ...).
        reason: String,
        /// Who removed it; `None` for a deleted account.
        actor: Option<String>,
    },
}

/// One merge-queue event on a pull request's timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueEvent {
    /// Pull request number.
    pub pr: u64,
    /// When it happened; decides the week.
    pub at: DateTime<Utc>,
    /// What happened.
    pub kind: QueueEventKind,
}

/// Everything [`compute`] needs.
#[derive(Clone, Debug)]
pub struct ScorecardObservation {
    /// The query.
    pub query: ScorecardQuery,
    /// Required contexts, and where they came from.
    pub required: Result<(Vec<String>, String), String>,
    /// Pull requests merged in the window.
    pub prs: Vec<MergedPr>,
    /// Merge-group runs created in the window.
    pub merge_group_runs: Vec<MergeGroupRun>,
    /// Merge-queue events of the pull requests read, at any date.
    pub queue_events: Vec<QueueEvent>,
    /// Failed merge-group gates with their groups' heads, when selector
    /// recall is measured.
    pub selection: Option<Vec<recall::GroupFailure>>,
    /// Reads that were incomplete or not cross-checked.
    pub gaps: Vec<TelemetryGap>,
}

/// Open-to-merge hours.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OpenToMerge {
    /// Median hours.
    pub median_hours: Option<f64>,
    /// 90th-percentile hours.
    pub p90_hours: Option<f64>,
}

/// Merge-queue entries and removals in one week, from the pull requests'
/// timelines.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MergeQueue {
    /// Queue entries (`AddedToMergeQueueEvent`).
    pub entries: usize,
    /// Removals GitHub made because checks failed or timed out.
    pub ejections: usize,
    /// `ejections / entries`.
    pub ejection_rate: Option<f64>,
    /// Every removal, by reason and then actor.
    pub removals: BTreeMap<String, BTreeMap<String, usize>>,
}

/// Merge-group runs whose gate job did not pass in one week.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GateFailures {
    /// Merge-group runs created in the week.
    pub runs: usize,
    /// Of those, runs whose final gate attempt completed.
    pub gated_runs: usize,
    /// Runs whose final gate attempt did not pass.
    pub failures: usize,
    /// Failures by cause (`gate_failed`, `starved`, ...).
    pub by_cause: BTreeMap<String, usize>,
    /// `failures / gated_runs`.
    pub failure_rate: Option<f64>,
    /// `gate_failed / gated_runs`: the gate ran and failed.
    pub gate_failed_rate: Option<f64>,
    /// Runs that concluded `failure` while their gate passed.
    pub run_failed_gate_passed: usize,
}

/// Red required checks per push in one week.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RedAfterPush {
    /// Commits carrying at least one required context.
    pub pushes: usize,
    /// Of those, commits with a red required context.
    pub red: usize,
    /// `red / pushes`.
    pub rate: Option<f64>,
    /// Commits whose only non-green required outcome was `cancelled`.
    pub cancelled_only: usize,
}

/// Merged pull requests by the verdict on their merge head, in one week.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MergeVerdicts {
    /// Approved heads.
    pub approve: usize,
    /// Held heads that merged anyway.
    pub hold: usize,
    /// Heads no trusted verdict named.
    pub none: usize,
    /// Heads whose verdict could not be read in full.
    pub unread: usize,
    /// The held pull requests that merged.
    pub held_merged: Vec<u64>,
}

/// One check name red across pull requests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepeatFailure {
    /// Check name.
    pub name: String,
    /// Whether it is a required context.
    pub required: bool,
    /// Pull requests with a commit on which it was red.
    pub prs: Vec<u64>,
}

/// One ISO week, clipped to the window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WeekScore {
    /// `YYYY-Www`.
    pub week: String,
    /// Start, inclusive.
    pub from: String,
    /// End, exclusive.
    pub to: String,
    /// Pull requests merged in the week.
    pub merged_prs: usize,
    /// Open to merge.
    pub open_to_merge: OpenToMerge,
    /// Merge-queue ejections.
    pub merge_queue: MergeQueue,
    /// Merge-group runs whose gate job did not pass.
    pub merge_group_gate: GateFailures,
    /// Red after push on required contexts; `None` with no required contexts.
    pub red_after_push: Option<RedAfterPush>,
    /// Check names red on two or more pull requests, most first.
    pub repeat_failures: Vec<RepeatFailure>,
    /// Merged pull requests by the verdict on their merge head.
    pub merge_verdicts: MergeVerdicts,
    /// Pull requests with more commits than GitHub lists; only their first
    /// [`PR_COMMITS_CAP`] commits are counted.
    pub capped_prs: Vec<u64>,
    /// Whether the heads of failed merge groups ran the failing tests;
    /// present when `--selection-step` is given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_recall: Option<recall::SelectionRecall>,
}

/// Which checks each rate judges: they differ, so two rates that disagree
/// can both be right.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Scopes {
    /// The ejection rate.
    pub merge_queue: String,
    /// The merge-group gate failure rate.
    pub merge_group_gate: String,
    /// The red-after-push rate.
    pub red_after_push: String,
}

/// The scorecard.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScorecardReport {
    /// [`SCHEMA`].
    pub schema: &'static str,
    /// Repository.
    pub repo: String,
    /// Base branch.
    pub base_branch: String,
    /// Workflow hosting the gate.
    pub workflow: String,
    /// Gate job.
    pub gate_job: String,
    /// Window start.
    pub from: String,
    /// Window end.
    pub to: String,
    /// Required contexts applied.
    pub required_contexts: Vec<String>,
    /// Gate-job steps read as a pull-request head's selection.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub selection_steps: Vec<String>,
    /// Where they came from, or why there are none.
    pub required_source: String,
    /// Which checks each rate judges.
    pub scopes: Scopes,
    /// Weeks, oldest first.
    pub weeks: Vec<WeekScore>,
    /// Reads that were incomplete or not cross-checked.
    pub gaps: Vec<TelemetryGap>,
    /// Tests that both passed and failed on one head commit in the window;
    /// present with `--flakes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_sha_flakes: Option<flakes::SameShaFlakes>,
}

/// Check-run conclusions and status states that make a check red.
fn is_red(outcome: &str) -> bool {
    matches!(
        outcome,
        "FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED" | "ERROR"
    )
}

/// Monday 00:00 UTC of the ISO week holding `time`.
fn week_start(time: DateTime<Utc>) -> DateTime<Utc> {
    let date = time.date_naive();
    let monday = date - Duration::days(i64::from(date.weekday().num_days_from_monday()));
    Utc.from_utc_datetime(&monday.and_hms_opt(0, 0, 0).expect("midnight"))
}

fn week_label(start: DateTime<Utc>) -> String {
    let week = start.iso_week();
    format!("{}-W{:02}", week.year(), week.week())
}

fn rfc3339(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Read one window from GitHub. Fails when a list cannot be read completely.
///
/// # Errors
///
/// When the merged pull requests or the merge-group runs cannot be read in
/// full.
pub fn gather(
    gh: &SyncGhReader<'_>,
    query: &ScorecardQuery,
    cache: &ReadCache,
) -> Result<ScorecardObservation, String> {
    let required = read_required(gh, query);
    let mut gaps = Vec::new();
    let mut prs: BTreeMap<u64, MergedPr> = BTreeMap::new();
    let mut runs: BTreeMap<u64, Value> = BTreeMap::new();
    let mut start = query.from;
    while start < query.to {
        let end = (start + Duration::hours(SLICE_HOURS)).min(query.to);
        for pr in read_merged_prs(gh, query, start, end)? {
            if pr.merged_at >= query.from && pr.merged_at < query.to {
                prs.insert(pr.number, pr);
            }
        }
        for run in read_merge_group_runs(gh, query, start, end)? {
            if let Some(id) = run.get("id").and_then(Value::as_u64) {
                runs.insert(id, run);
            }
        }
        start = end;
    }
    let mut runs: Vec<Value> = runs.into_values().collect();
    let listing_query = GateCostQuery {
        repo: query.repo.clone(),
        workflow: query.workflow.clone(),
        gate_job: query.gate_job.clone(),
        base_branch: query.base_branch.clone(),
        events: vec![MERGE_GROUP_EVENT.to_owned()],
        receipt_job: None,
        receipt_target: query.gate_job.clone(),
        from: query.from,
        to: query.to,
    };
    cross_check_listing(gh, &listing_query, MERGE_GROUP_EVENT, &mut runs, &mut gaps);
    let merge_group_runs = read_gates(gh, cache, query, &runs)?;
    let selection = (!query.selection_steps.is_empty())
        .then(|| recall::gather(gh, cache, query, &runs, &merge_group_runs));
    let queued: BTreeSet<u64> = runs
        .iter()
        .filter_map(|run| text(run, "head_branch"))
        .filter_map(|branch| queue_branch_pr(&branch))
        .chain(prs.keys().copied())
        .collect();
    let queue_events = read_queue_events(gh, query, &queued)?;
    Ok(ScorecardObservation {
        query: query.clone(),
        required,
        prs: prs.into_values().collect(),
        merge_group_runs,
        queue_events,
        selection,
        gaps,
    })
}

/// Required contexts: the flag's, else the classic branch protection's. A
/// ruleset that requires checks is refused rather than silently ignored.
fn read_required(
    gh: &SyncGhReader<'_>,
    query: &ScorecardQuery,
) -> Result<(Vec<String>, String), String> {
    if !query.required_contexts.is_empty() {
        return Ok((
            query.required_contexts.clone(),
            "--required-context".to_owned(),
        ));
    }
    let rules = read_json(
        gh,
        &strings(&[
            "api",
            &format!("repos/{}/rules/branches/{}", query.repo, query.base_branch),
        ]),
    )?;
    if rules.as_array().is_some_and(|rules| {
        rules
            .iter()
            .any(|rule| text(rule, "type").as_deref() == Some("required_status_checks"))
    }) {
        return Err(
            "a ruleset requires status checks on the base; pass them with --required-context"
                .to_owned(),
        );
    }
    let path = format!(
        "repos/{}/branches/{}/protection/required_status_checks",
        query.repo, query.base_branch
    );
    match read_json(gh, &strings(&["api", &path])) {
        Ok(protection) => {
            let contexts: Vec<String> = protection
                .get("contexts")
                .and_then(Value::as_array)
                .ok_or_else(|| "branch protection carried no contexts".to_owned())?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            Ok((contexts, "branch protection".to_owned()))
        }
        Err(error) if error.contains("Branch not protected") => {
            Ok((Vec::new(), "the base branch is not protected".to_owned()))
        }
        Err(error) => Err(error),
    }
}

/// A paginated GraphQL listing, read again from its first page when GitHub
/// answers a gateway error (`HTTP 502`, `503` or `504`), which a heavy page
/// does under load; any other failure, or the last attempt's, is returned
/// with its reason at the end.
fn read_graphql(gh: &SyncGhReader<'_>, args: &[String]) -> Result<Value, String> {
    let mut attempt = 1;
    loop {
        match read_json(gh, args) {
            Ok(value) => return Ok(value),
            Err(error) => {
                let transient = ["HTTP 502", "HTTP 503", "HTTP 504"]
                    .iter()
                    .any(|status| error.contains(status));
                if !transient || attempt >= GRAPHQL_ATTEMPTS {
                    let start = error.len().saturating_sub(ERROR_TAIL);
                    let start = (start..=error.len())
                        .find(|index| error.is_char_boundary(*index))
                        .unwrap_or(error.len());
                    return Err(format!(
                        "GraphQL read failed after {attempt} attempt(s): ...{}",
                        &error[start..]
                    ));
                }
                attempt += 1;
                std::thread::sleep(GRAPHQL_RETRY_PAUSE);
            }
        }
    }
}

/// Pull requests merged in `[from, to]` (GitHub's range is inclusive; the
/// caller keeps the half-open window).
fn read_merged_prs(
    gh: &SyncGhReader<'_>,
    query: &ScorecardQuery,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<MergedPr>, String> {
    let search = format!(
        "repo:{} is:pr is:merged base:{} merged:{}..{}",
        query.repo,
        query.base_branch,
        rfc3339(from),
        rfc3339(to)
    );
    let pages = read_graphql(
        gh,
        &strings(&[
            "api",
            "graphql",
            "--paginate",
            "--slurp",
            "-f",
            &format!("query={PRS_QUERY}"),
            "-f",
            &format!("q={search}"),
        ]),
    )?;
    let pages = pages.as_array().cloned().unwrap_or_else(|| vec![pages]);
    let mut nodes = Vec::new();
    let mut count = None;
    for page in &pages {
        if let Some(errors) = page.get("errors") {
            return Err(format!("merged pull requests: GraphQL errors {errors}"));
        }
        let search = page
            .pointer("/data/search")
            .ok_or_else(|| "merged pull requests: no search in the answer".to_owned())?;
        count = count.or_else(|| search.get("issueCount").and_then(Value::as_u64));
        nodes.extend(node_list(search, "merged pull requests")?);
    }
    let count = count.ok_or_else(|| "merged pull requests: no issueCount".to_owned())?;
    if count > SEARCH_CAP {
        return Err(format!(
            "{count} pull requests merged between {} and {} exceed GitHub's {SEARCH_CAP}-result \
             search cap",
            rfc3339(from),
            rfc3339(to)
        ));
    }
    if count != nodes.len() as u64 {
        return Err(format!(
            "merged pull requests: collected {} of issueCount {count}; refusing a partial read",
            nodes.len()
        ));
    }
    let (owner, name) = query
        .repo
        .split_once('/')
        .ok_or_else(|| format!("repo `{}` is not OWNER/REPO", query.repo))?;
    parallel_map(&nodes, |node| merged_pr(gh, owner, name, node))
        .into_iter()
        .collect()
}

fn merged_pr(
    gh: &SyncGhReader<'_>,
    owner: &str,
    name: &str,
    node: &Value,
) -> Result<MergedPr, String> {
    let number = node
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| "merged pull request without a number".to_owned())?;
    let created_at = timestamp(node, "createdAt")
        .ok_or_else(|| format!("pull request #{number} has no createdAt"))?;
    let merged_at = timestamp(node, "mergedAt")
        .ok_or_else(|| format!("pull request #{number} has no mergedAt"))?;
    let (listed, commits_total) = read_commits(gh, owner, name, number)?;
    let mut commits = Vec::new();
    for entry in &listed {
        let commit = entry
            .get("commit")
            .ok_or_else(|| format!("a commit of #{number} has no commit"))?;
        let oid = text(commit, "oid").ok_or_else(|| format!("a commit of #{number} has no oid"))?;
        // `null` is a commit without checks; a missing key is an answer that
        // was not read.
        let contexts = match commit.get("statusCheckRollup") {
            None => return Err(format!("{oid}: no statusCheckRollup in the answer")),
            Some(Value::Null) => Vec::new(),
            Some(rollup) => {
                let contexts = rollup
                    .get("contexts")
                    .ok_or_else(|| format!("{oid}: no contexts in the answer"))?;
                let first = node_list(contexts, &format!("checks of {oid}"))?;
                if contexts.get("totalCount").and_then(Value::as_u64) == Some(first.len() as u64) {
                    first
                } else {
                    read_contexts(gh, owner, name, &oid)?
                }
            }
        };
        commits.push(PrCommit {
            oid,
            checks: latest_by_name(&contexts),
        });
    }
    let verdict = read_merge_verdict(gh, owner, name, number, merged_at)?;
    Ok(MergedPr {
        number,
        created_at,
        merged_at,
        commits,
        commits_total,
        verdict,
    })
}

fn read_merge_verdict(
    gh: &SyncGhReader<'_>,
    owner: &str,
    name: &str,
    number: u64,
    merged_at: DateTime<Utc>,
) -> Result<MergeVerdict, String> {
    let answer = read_graphql(
        gh,
        &strings(&[
            "api",
            "graphql",
            "-f",
            &format!("query={VERDICTS_QUERY}"),
            "-f",
            &format!("owner={owner}"),
            "-f",
            &format!("name={name}"),
            "-F",
            &format!("number={number}"),
        ]),
    )?;
    if let Some(errors) = answer.get("errors") {
        return Err(format!("verdicts of #{number}: GraphQL errors {errors}"));
    }
    let pr = answer
        .pointer("/data/repository/pullRequest")
        .ok_or_else(|| format!("verdicts of #{number}: no pullRequest in the answer"))?;
    merge_verdict(pr, merged_at).map_err(|error| format!("verdicts of #{number}: {error}"))
}

/// The verdict on `pr`'s merge head from its comments and reviews (see the
/// module docs).
fn merge_verdict(pr: &Value, merged_at: DateTime<Utc>) -> Result<MergeVerdict, String> {
    let head = text(pr, "headRefOid").ok_or("no headRefOid")?;
    let author = pr.pointer("/author/login").and_then(Value::as_str);
    let trusted = |node: &Value| {
        node.pointer("/author/login")
            .and_then(Value::as_str)
            .is_some_and(|login| {
                crate::head_approval::trusted_reviewer(
                    login,
                    node.get("authorAssociation").and_then(Value::as_str),
                    author,
                )
            })
    };
    let mut verdicts: Vec<(DateTime<Utc>, bool)> = Vec::new();
    // The newest pre-merge entry whose verdict at the merge cannot be read.
    let mut unknown: Option<DateTime<Utc>> = None;
    let comments = pr.get("comments").ok_or("no comments")?;
    let comment_nodes = node_list(comments, "comments")?;
    for comment in &comment_nodes {
        let Some(at) = timestamp(comment, "createdAt") else {
            continue;
        };
        if at <= merged_at
            && timestamp(comment, "lastEditedAt").is_some_and(|edited| edited > merged_at)
            && trusted(comment)
        {
            unknown = unknown.max(Some(at));
            continue;
        }
        let body = comment.get("body").and_then(Value::as_str).unwrap_or("");
        if let Some((approve, named)) = crate::head_approval::verdict_line(body)
            && named.eq_ignore_ascii_case(&head)
            && at <= merged_at
            && trusted(comment)
        {
            verdicts.push((at, approve));
        }
    }
    let reviews = pr.get("reviews").ok_or("no reviews")?;
    let review_nodes = node_list(reviews, "reviews")?;
    for review in &review_nodes {
        let state = review.get("state").and_then(Value::as_str);
        let Some(at) = timestamp(review, "submittedAt") else {
            continue;
        };
        let on_head = review
            .pointer("/commit/oid")
            .and_then(Value::as_str)
            .is_some_and(|oid| oid.eq_ignore_ascii_case(&head));
        if !on_head || at > merged_at || !trusted(review) {
            continue;
        }
        match state {
            Some("APPROVED") => verdicts.push((at, true)),
            Some("CHANGES_REQUESTED") => verdicts.push((at, false)),
            // Dismissed since: what it said at the merge is no longer read.
            Some("DISMISSED") => unknown = unknown.max(Some(at)),
            _ => {}
        }
    }
    let decided = verdicts.iter().max_by_key(|(at, approve)| (*at, !*approve));
    if unknown.is_some_and(|unknown| decided.is_none_or(|(at, _)| unknown >= *at)) {
        return Ok(MergeVerdict::Unread);
    }
    if let Some((_, approve)) = decided {
        return Ok(if *approve {
            MergeVerdict::Approve
        } else {
            MergeVerdict::Hold
        });
    }
    let partial = |connection: &Value, listed: usize| {
        connection
            .get("totalCount")
            .and_then(Value::as_u64)
            .is_none_or(|total| total > listed as u64)
    };
    Ok(
        if partial(comments, comment_nodes.len()) || partial(reviews, review_nodes.len()) {
            MergeVerdict::Unread
        } else {
            MergeVerdict::None
        },
    )
}

fn merge_verdicts(prs: &[&MergedPr]) -> MergeVerdicts {
    let mut out = MergeVerdicts::default();
    for pr in prs {
        match pr.verdict {
            MergeVerdict::Approve => out.approve += 1,
            MergeVerdict::Hold => {
                out.hold += 1;
                out.held_merged.push(pr.number);
            }
            MergeVerdict::None => out.none += 1,
            MergeVerdict::Unread => out.unread += 1,
        }
    }
    out.held_merged.sort_unstable();
    out
}

/// Every commit of one pull request GitHub lists, with the first page of its
/// checks, and the pull request's commit count.
fn read_commits(
    gh: &SyncGhReader<'_>,
    owner: &str,
    name: &str,
    number: u64,
) -> Result<(Vec<Value>, u64), String> {
    let pages = read_graphql(
        gh,
        &strings(&[
            "api",
            "graphql",
            "--paginate",
            "--slurp",
            "-f",
            &format!("query={COMMITS_QUERY}"),
            "-f",
            &format!("owner={owner}"),
            "-f",
            &format!("name={name}"),
            "-F",
            &format!("number={number}"),
        ]),
    )?;
    let pages = pages.as_array().cloned().unwrap_or_else(|| vec![pages]);
    let mut nodes = Vec::new();
    let mut total = None;
    for page in &pages {
        if let Some(errors) = page.get("errors") {
            return Err(format!("commits of #{number}: GraphQL errors {errors}"));
        }
        let commits = page
            .pointer("/data/repository/pullRequest/commits")
            .ok_or_else(|| format!("commits of #{number}: no commits in the answer"))?;
        total = total.or_else(|| commits.get("totalCount").and_then(Value::as_u64));
        nodes.extend(node_list(commits, &format!("commits of #{number}"))?);
    }
    let total = commits_read(number, total, nodes.len() as u64)?;
    Ok((nodes, total))
}

/// The commit count of a read that collected `collected` of `total`: every
/// commit, or GitHub's cap of a longer pull request. Anything else is a
/// partial read.
fn commits_read(number: u64, total: Option<u64>, collected: u64) -> Result<u64, String> {
    match total {
        Some(total) if total == collected => Ok(total),
        Some(total) if total > PR_COMMITS_CAP && collected == PR_COMMITS_CAP => Ok(total),
        _ => Err(format!(
            "commits of #{number}: collected {collected} of totalCount {total:?}; refusing a \
             partial read"
        )),
    }
}

/// A connection's `nodes`; a missing array is an error, never zero items.
fn node_list(connection: &Value, what: &str) -> Result<Vec<Value>, String> {
    connection
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| format!("{what}: no nodes in the answer"))
}

/// Every check context of one commit, when the first page did not hold them.
fn read_contexts(
    gh: &SyncGhReader<'_>,
    owner: &str,
    name: &str,
    oid: &str,
) -> Result<Vec<Value>, String> {
    let pages = read_graphql(
        gh,
        &strings(&[
            "api",
            "graphql",
            "--paginate",
            "--slurp",
            "-f",
            &format!("query={CONTEXTS_QUERY}"),
            "-f",
            &format!("owner={owner}"),
            "-f",
            &format!("name={name}"),
            "-f",
            &format!("oid={oid}"),
        ]),
    )?;
    let pages = pages.as_array().cloned().unwrap_or_else(|| vec![pages]);
    let mut nodes = Vec::new();
    let mut total = None;
    for page in &pages {
        if let Some(errors) = page.get("errors") {
            return Err(format!("checks of {oid}: GraphQL errors {errors}"));
        }
        let contexts = page
            .pointer("/data/repository/object/statusCheckRollup/contexts")
            .ok_or_else(|| format!("checks of {oid}: no contexts in the answer"))?;
        total = total.or_else(|| contexts.get("totalCount").and_then(Value::as_u64));
        nodes.extend(node_list(contexts, &format!("checks of {oid}"))?);
    }
    if total != Some(nodes.len() as u64) {
        return Err(format!(
            "checks of {oid}: collected {} of totalCount {total:?}; refusing a partial read",
            nodes.len()
        ));
    }
    Ok(nodes)
}

/// The pull request a merge-queue branch was built for:
/// `gh-readonly-queue/<base>/pr-<number>-<sha>`.
fn queue_branch_pr(branch: &str) -> Option<u64> {
    let rest = branch.strip_prefix("gh-readonly-queue/")?;
    let (_, entry) = rest.rsplit_once("/pr-")?;
    entry.split_once('-')?.0.parse().ok()
}

/// Every merge-queue event of `prs`; [`compute`] keeps those dated in each
/// week.
fn read_queue_events(
    gh: &SyncGhReader<'_>,
    query: &ScorecardQuery,
    prs: &BTreeSet<u64>,
) -> Result<Vec<QueueEvent>, String> {
    let (owner, name) = query
        .repo
        .split_once('/')
        .ok_or_else(|| format!("repo `{}` is not OWNER/REPO", query.repo))?;
    let prs: Vec<u64> = prs.iter().copied().collect();
    let read = parallel_map(&prs, |number| {
        read_pr_queue_events(gh, owner, name, *number)
    });
    let mut events = Vec::new();
    for pr_events in read {
        events.extend(pr_events?);
    }
    Ok(events)
}

/// Every merge-queue event on one pull request's timeline.
///
/// With `itemTypes`, the connection's `totalCount` counts the whole timeline;
/// `filteredCount` is the number of matching events, so the read is checked
/// against that.
fn read_pr_queue_events(
    gh: &SyncGhReader<'_>,
    owner: &str,
    name: &str,
    number: u64,
) -> Result<Vec<QueueEvent>, String> {
    let pages = read_graphql(
        gh,
        &strings(&[
            "api",
            "graphql",
            "--paginate",
            "--slurp",
            "-f",
            &format!("query={TIMELINE_QUERY}"),
            "-f",
            &format!("owner={owner}"),
            "-f",
            &format!("name={name}"),
            "-F",
            &format!("number={number}"),
        ]),
    )?;
    let pages = pages.as_array().cloned().unwrap_or_else(|| vec![pages]);
    parse_queue_events(number, &pages)
}

/// The merge-queue events in one pull request's timeline pages.
fn parse_queue_events(number: u64, pages: &[Value]) -> Result<Vec<QueueEvent>, String> {
    let what = format!("queue events of #{number}");
    let mut nodes = Vec::new();
    let mut filtered = None;
    for page in pages {
        if let Some(errors) = page.get("errors") {
            return Err(format!("{what}: GraphQL errors {errors}"));
        }
        let items = page
            .pointer("/data/repository/pullRequest/timelineItems")
            .ok_or_else(|| format!("{what}: no timelineItems in the answer"))?;
        filtered = filtered.or_else(|| items.get("filteredCount").and_then(Value::as_u64));
        nodes.extend(node_list(items, &what)?);
    }
    if filtered != Some(nodes.len() as u64) {
        return Err(format!(
            "{what}: collected {} of filteredCount {filtered:?}; refusing a partial read",
            nodes.len()
        ));
    }
    nodes
        .iter()
        .map(|node| {
            let at = timestamp(node, "createdAt")
                .ok_or_else(|| format!("{what}: an event has no createdAt"))?;
            let kind = match text(node, "__typename").as_deref() {
                Some("AddedToMergeQueueEvent") => QueueEventKind::Added,
                Some("RemovedFromMergeQueueEvent") => QueueEventKind::Removed {
                    reason: text(node, "reason")
                        .ok_or_else(|| format!("{what}: a removal has no reason"))?,
                    actor: node.get("actor").and_then(|actor| text(actor, "login")),
                },
                other => return Err(format!("{what}: unexpected event {other:?}")),
            };
            Ok(QueueEvent {
                pr: number,
                at,
                kind,
            })
        })
        .collect()
}

/// The latest outcome of each check name: a re-run supersedes the run it
/// repeats, as it does for GitHub's required checks.
fn latest_by_name(contexts: &[Value]) -> Vec<CheckOutcome> {
    let mut latest: BTreeMap<String, (Option<DateTime<Utc>>, Option<String>)> = BTreeMap::new();
    for context in contexts {
        let (name, outcome, at) = match text(context, "__typename").as_deref() {
            Some("CheckRun") => (
                text(context, "name"),
                text(context, "conclusion"),
                timestamp(context, "completedAt").or_else(|| timestamp(context, "startedAt")),
            ),
            Some("StatusContext") => (
                text(context, "context"),
                text(context, "state"),
                timestamp(context, "createdAt"),
            ),
            _ => continue,
        };
        let Some(name) = name else { continue };
        let outcome = outcome.map(|outcome| outcome.to_ascii_uppercase());
        match latest.get(&name) {
            Some((Some(seen), _)) if at.is_none_or(|at| at < *seen) => {}
            _ => {
                latest.insert(name, (at, outcome));
            }
        }
    }
    latest
        .into_iter()
        .map(|(name, (at, outcome))| CheckOutcome { name, outcome, at })
        .collect()
}

fn read_merge_group_runs(
    gh: &SyncGhReader<'_>,
    query: &ScorecardQuery,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Value>, String> {
    let pages = read_pages(
        gh,
        &format!(
            "repos/{}/actions/workflows/{}/runs",
            query.repo, query.workflow
        ),
        &[
            format!("event={MERGE_GROUP_EVENT}"),
            format!("created={}..{}", rfc3339(from), rfc3339(to)),
            "per_page=100".to_owned(),
        ],
    )?;
    let runs = collect_counted(&pages, "workflow_runs", "merge-group runs")?;
    Ok(runs
        .into_iter()
        .filter(|run| {
            timestamp(run, "created_at").is_some_and(|at| at >= query.from && at < query.to)
        })
        .collect())
}

/// The final gate attempt of each merge-group run.
fn read_gates(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    query: &ScorecardQuery,
    runs: &[Value],
) -> Result<Vec<MergeGroupRun>, String> {
    let refs: Vec<(RunRef, &Value)> = runs
        .iter()
        .filter_map(|run| {
            Some((
                RunRef {
                    id: run.get("id").and_then(Value::as_u64)?,
                    attempt: run.get("run_attempt").and_then(Value::as_u64),
                    completed: text(run, "status").as_deref() == Some("completed"),
                },
                run,
            ))
        })
        .collect();
    let read = parallel_map(&refs, |(run, value)| {
        let jobs = read_run_jobs(gh, cache, &query.repo, run)?;
        let exact: Vec<&Value> = jobs
            .iter()
            .filter(|job| text(job, "name").as_deref() == Some(query.gate_job.as_str()))
            .collect();
        let candidates: Vec<&Value> = if exact.is_empty() {
            let picked = resolved_gate_jobs(&jobs, &query.gate_job);
            jobs.iter()
                .filter(|job| {
                    job.get("id")
                        .and_then(Value::as_u64)
                        .is_some_and(|id| picked.values().any(|chosen| *chosen == id))
                })
                .collect()
        } else {
            exact
        };
        let gate = candidates
            .into_iter()
            .map(|job| {
                (
                    GateJobSample::from_job(run.id, MERGE_GROUP_EVENT, job),
                    job.get("id").and_then(Value::as_u64),
                )
            })
            .max_by_key(|(sample, _)| sample.attempt);
        Ok::<_, String>(MergeGroupRun {
            run_id: run.id,
            created_at: timestamp(value, "created_at").unwrap_or(query.from),
            conclusion: text(value, "conclusion"),
            gate_job_id: gate.as_ref().and_then(|(_, id)| *id),
            gate: gate.map(|(sample, _)| sample),
        })
    });
    read.into_iter().collect()
}

#[allow(clippy::cast_precision_loss)]
fn share(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator > 0).then(|| round2(numerator as f64 / denominator as f64))
}

#[allow(clippy::cast_precision_loss)]
fn open_to_merge(prs: &[&MergedPr]) -> OpenToMerge {
    let mut hours: Vec<f64> = prs
        .iter()
        .map(|pr| (pr.merged_at - pr.created_at).num_seconds() as f64 / 3600.0)
        .collect();
    hours.sort_by(f64::total_cmp);
    OpenToMerge {
        median_hours: percentile(&hours, 0.5).map(round2),
        p90_hours: percentile(&hours, 0.9).map(round2),
    }
}

/// Removal reasons that mean the queue's required checks failed.
const EJECTION_REASONS: [&str; 2] = ["failed_checks", "checks_timed_out"];
/// The actor GitHub records for removals the merge queue itself makes.
const MERGE_QUEUE_ACTOR: &str = "github-merge-queue";

fn merge_queue(events: &[&QueueEvent]) -> MergeQueue {
    let mut entries = 0;
    let mut ejections = 0;
    let mut removals: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for event in events {
        match &event.kind {
            QueueEventKind::Added => entries += 1,
            QueueEventKind::Removed { reason, actor } => {
                let actor = actor.as_deref().unwrap_or("unknown");
                if actor == MERGE_QUEUE_ACTOR && EJECTION_REASONS.contains(&reason.as_str()) {
                    ejections += 1;
                }
                *removals
                    .entry(reason.clone())
                    .or_default()
                    .entry(actor.to_owned())
                    .or_insert(0) += 1;
            }
        }
    }
    MergeQueue {
        entries,
        ejections,
        ejection_rate: share(ejections, entries),
        removals,
    }
}

fn gate_failures(runs: &[&MergeGroupRun]) -> GateFailures {
    let mut by_cause: BTreeMap<String, usize> = BTreeMap::new();
    let mut gated_runs = 0;
    let mut run_failed_gate_passed = 0;
    for run in runs {
        let Some(gate) = run.gate.as_ref().filter(|gate| gate.status == "completed") else {
            continue;
        };
        gated_runs += 1;
        match ejection_cause(gate) {
            Some(cause) => *by_cause.entry(cause).or_insert(0) += 1,
            None if run.conclusion.as_deref() == Some("failure") => run_failed_gate_passed += 1,
            None => {}
        }
    }
    let failures = by_cause.values().sum();
    let gate_failed = by_cause.get("gate_failed").copied().unwrap_or(0);
    GateFailures {
        runs: runs.len(),
        gated_runs,
        failures,
        failure_rate: share(failures, gated_runs),
        gate_failed_rate: share(gate_failed, gated_runs),
        by_cause,
        run_failed_gate_passed,
    }
}

fn red_after_push(prs: &[&MergedPr], required: &BTreeSet<&str>) -> RedAfterPush {
    let mut pushes = 0;
    let mut red = 0;
    let mut cancelled_only = 0;
    for pr in prs {
        for commit in &pr.commits {
            let outcomes: Vec<&str> = commit
                .checks
                .iter()
                .filter(|check| required.contains(check.name.as_str()))
                .map(|check| check.outcome.as_deref().unwrap_or("PENDING"))
                .collect();
            if outcomes.is_empty() {
                continue;
            }
            pushes += 1;
            if outcomes.iter().any(|outcome| is_red(outcome)) {
                red += 1;
            } else if outcomes.contains(&"CANCELLED") {
                cancelled_only += 1;
            }
        }
    }
    RedAfterPush {
        pushes,
        red,
        rate: share(red, pushes),
        cancelled_only,
    }
}

fn repeat_failures(prs: &[&MergedPr], required: &BTreeSet<&str>) -> Vec<RepeatFailure> {
    let mut by_name: BTreeMap<&str, BTreeSet<u64>> = BTreeMap::new();
    for pr in prs {
        for commit in &pr.commits {
            for check in &commit.checks {
                if check.outcome.as_deref().is_some_and(is_red) {
                    by_name
                        .entry(check.name.as_str())
                        .or_default()
                        .insert(pr.number);
                }
            }
        }
    }
    let mut repeats: Vec<RepeatFailure> = by_name
        .into_iter()
        .filter(|(_, prs)| prs.len() >= 2)
        .map(|(name, prs)| RepeatFailure {
            name: name.to_owned(),
            required: required.contains(name),
            prs: prs.into_iter().collect(),
        })
        .collect();
    repeats.sort_by(|a, b| {
        b.prs
            .len()
            .cmp(&a.prs.len())
            .then_with(|| a.name.cmp(&b.name))
    });
    repeats
}

/// Score each ISO week of the window.
#[must_use]
pub fn compute(observation: &ScorecardObservation) -> ScorecardReport {
    let query = &observation.query;
    let (required_contexts, required_source) = match &observation.required {
        Ok((contexts, source)) => (contexts.clone(), source.clone()),
        Err(error) => (Vec::new(), format!("unread: {error}")),
    };
    let required: BTreeSet<&str> = required_contexts.iter().map(String::as_str).collect();
    let mut weeks = Vec::new();
    let mut start = query.from;
    while start < query.to {
        let end = (week_start(start) + Duration::days(7)).min(query.to);
        let prs: Vec<&MergedPr> = observation
            .prs
            .iter()
            .filter(|pr| pr.merged_at >= start && pr.merged_at < end)
            .collect();
        let runs: Vec<&MergeGroupRun> = observation
            .merge_group_runs
            .iter()
            .filter(|run| run.created_at >= start && run.created_at < end)
            .collect();
        let events: Vec<&QueueEvent> = observation
            .queue_events
            .iter()
            .filter(|event| event.at >= start && event.at < end)
            .collect();
        weeks.push(WeekScore {
            week: week_label(start),
            from: rfc3339(start),
            to: rfc3339(end),
            merged_prs: prs.len(),
            open_to_merge: open_to_merge(&prs),
            merge_queue: merge_queue(&events),
            merge_group_gate: gate_failures(&runs),
            red_after_push: (!required.is_empty()).then(|| red_after_push(&prs, &required)),
            repeat_failures: repeat_failures(&prs, &required),
            merge_verdicts: merge_verdicts(&prs),
            capped_prs: prs
                .iter()
                .filter(|pr| (pr.commits.len() as u64) < pr.commits_total)
                .map(|pr| pr.number)
                .collect(),
            selection_recall: observation.selection.as_ref().map(|failures| {
                let week: Vec<&recall::GroupFailure> = failures
                    .iter()
                    .filter(|failure| failure.created_at >= start && failure.created_at < end)
                    .collect();
                recall::compute(&query.repo, &week)
            }),
        });
        start = end;
    }
    let mut gaps = observation.gaps.clone();
    gaps.extend(unseen_required(observation, &required));
    ScorecardReport {
        schema: SCHEMA,
        repo: query.repo.clone(),
        base_branch: query.base_branch.clone(),
        workflow: query.workflow.clone(),
        gate_job: query.gate_job.clone(),
        from: rfc3339(query.from),
        to: rfc3339(query.to),
        scopes: Scopes {
            merge_queue: "every required check, as GitHub's removal reason reports it".to_owned(),
            merge_group_gate: format!(
                "job `{}` of {} in merge-group runs only",
                query.gate_job, query.workflow
            ),
            red_after_push: if required_contexts.is_empty() {
                "none: no required contexts".to_owned()
            } else {
                format!("required contexts: {}", required_contexts.join(", "))
            },
        },
        required_contexts,
        required_source,
        selection_steps: query.selection_steps.clone(),
        weeks,
        gaps,
        same_sha_flakes: None,
    }
}

/// Required contexts no merged pull request's commit carried: a misspelled
/// `--required-context` would otherwise read as a smaller red rate.
fn unseen_required(
    observation: &ScorecardObservation,
    required: &BTreeSet<&str>,
) -> Vec<TelemetryGap> {
    let seen: BTreeSet<&str> = observation
        .prs
        .iter()
        .flat_map(|pr| &pr.commits)
        .flat_map(|commit| &commit.checks)
        .map(|check| check.name.as_str())
        .collect();
    required
        .iter()
        .filter(|name| !observation.prs.is_empty() && !seen.contains(*name))
        .map(|name| TelemetryGap {
            signal: "required_contexts".to_owned(),
            reason: format!(
                "required context `{name}` is on no commit of a pull request merged in the \
                 window; check its spelling"
            ),
        })
        .collect()
}

fn opt(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| format!("{value}"))
}

/// Human rendering, one block per week.
#[must_use]
pub fn render(report: &ScorecardReport) -> String {
    let mut out = format!(
        "{} into {}, {} .. {}; ejections from {} job `{}`\n",
        report.repo, report.base_branch, report.from, report.to, report.workflow, report.gate_job
    );
    let _ = writeln!(
        out,
        "required contexts ({}): {}",
        report.required_source,
        if report.required_contexts.is_empty() {
            "none".to_owned()
        } else {
            report.required_contexts.join(", ")
        }
    );
    let _ = writeln!(
        out,
        "scopes: ejections judge {}; gate failures judge {}; red after push judges {}",
        report.scopes.merge_queue, report.scopes.merge_group_gate, report.scopes.red_after_push
    );
    for week in &report.weeks {
        let _ = writeln!(out, "{} ({} .. {}):", week.week, week.from, week.to);
        let _ = writeln!(
            out,
            "  merged PRs {}; open to merge median {} h, p90 {} h",
            week.merged_prs,
            opt(week.open_to_merge.median_hours),
            opt(week.open_to_merge.p90_hours)
        );
        let queue = &week.merge_queue;
        let removals: Vec<String> = queue
            .removals
            .iter()
            .flat_map(|(reason, actors)| {
                actors
                    .iter()
                    .map(move |(actor, count)| format!("{reason} by {actor} {count}"))
            })
            .collect();
        let _ = writeln!(
            out,
            "  merge queue: {} ejections of {} entries ({}); removals: {}",
            queue.ejections,
            queue.entries,
            opt(queue.ejection_rate),
            if removals.is_empty() {
                "none".to_owned()
            } else {
                removals.join(", ")
            }
        );
        let gate = &week.merge_group_gate;
        let _ = writeln!(
            out,
            "  merge-group gate ({} job `{}`): {} of {} runs failed ({}), gate failed {}; {} runs failed with the gate green",
            report.workflow,
            report.gate_job,
            gate.failures,
            gate.gated_runs,
            opt(gate.failure_rate),
            opt(gate.gate_failed_rate),
            gate.run_failed_gate_passed
        );
        match &week.red_after_push {
            Some(red) => {
                let _ = writeln!(
                    out,
                    "  red after push: {} of {} pushes ({}); {} cancelled only",
                    red.red,
                    red.pushes,
                    opt(red.rate),
                    red.cancelled_only
                );
            }
            None => {
                let _ = writeln!(out, "  red after push: n/a (no required contexts)");
            }
        }
        for repeat in &week.repeat_failures {
            let _ = writeln!(
                out,
                "  repeat failure{}: {} on {} PRs",
                if repeat.required { " (required)" } else { "" },
                repeat.name,
                repeat.prs.len()
            );
        }
        render_week_tail(&mut out, week);
    }
    for gap in &report.gaps {
        let _ = writeln!(out, "gap ({}): {}", gap.signal, gap.reason);
    }
    out
}

/// Capped pull requests and selector recall, after a week's rates.
fn render_week_tail(out: &mut String, week: &WeekScore) {
    let verdicts = &week.merge_verdicts;
    let held: Vec<String> = verdicts
        .held_merged
        .iter()
        .map(|pr| format!("#{pr}"))
        .collect();
    let _ = writeln!(
        out,
        "  merge-head verdict: {} approved, {} HOLD{}, {} none, {} unread",
        verdicts.approve,
        verdicts.hold,
        if held.is_empty() {
            String::new()
        } else {
            format!(" (merged anyway: {})", held.join(", "))
        },
        verdicts.none,
        verdicts.unread
    );
    if !week.capped_prs.is_empty() {
        let capped: Vec<String> = week.capped_prs.iter().map(|pr| format!("#{pr}")).collect();
        let _ = writeln!(
            out,
            "  only the first {PR_COMMITS_CAP} commits counted (GitHub lists no more): {}",
            capped.join(", ")
        );
    }
    if let Some(recall) = &week.selection_recall {
        recall::render(out, recall);
    }
}

pub mod recall;

#[cfg(test)]
mod tests;
