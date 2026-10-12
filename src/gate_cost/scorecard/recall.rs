//! Selector recall, in observation mode: for every test a merge-group gate
//! failed on, did the head run of a pull request in that group run it?
//!
//! A pull-request head runs a selection of tests (named steps of the gate
//! job, `--selection-step`) and the merge group runs the full suite, so each
//! merge-group test failure is a labelled example of what the selection
//! should have caught. Nothing here changes what runs.
//!
//! - **failure**: a merge-group run created in the week whose final gate
//!   attempt failed, and the tests in that attempt's log under "The
//!   following tests FAILED". A failed gate with no such block (a build
//!   break, a crash) is counted apart.
//! - **group**: the pull request the queue branch names, and every entry
//!   ahead of it. A queue branch `gh-readonly-queue/<base>/pr-<n>-<sha>` is
//!   built on `<sha>`, which is either the base branch or the head of the
//!   entry ahead; it is the base once that entry's pull request had merged
//!   as exactly that commit before the run was created.
//! - **pull-request head**: the second parent of the entry's queue merge
//!   commit, and its gate job in every [`HEAD_EVENTS`] run of the workflow
//!   on that commit.
//! - **selected**: some pull-request head in the group ran the test in a
//!   selection step (whatever its outcome there: a test that passed on its
//!   head and failed in the group was selected, and failed for another
//!   reason). **missed**: every head was read and none ran it.
//!   **undetermined**: no head ran it and at least one could not be read.
//!   **recall** = selected / (selected + missed).
//!
//! Logs are read only for failed merge-group gates and the heads of their
//! groups. What a completed job's log yields is cached, never the log.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::ctest_log::{self, JobLog};
use super::super::{
    MERGE_GROUP_EVENT, ReadCache, RunRef, SyncGhReader, collect_counted, parallel_map, read_json,
    read_pages, read_run_jobs, round2, strings, text, timestamp,
};
use super::{MergeGroupRun, ScorecardQuery, queue_branch_pr};

/// Events of the runs on a pull-request head. Checks of a pull request
/// opened by an App are dispatched (`workflow_dispatch`), and their gate
/// job may skip steps that only a `pull_request` run takes; read as such,
/// it selected nothing.
pub const HEAD_EVENTS: [&str; 2] = ["pull_request", "workflow_dispatch"];
/// Longest chain of queue entries walked for one group.
const MAX_GROUP: usize = 20;
/// Cache key version: bump when an extract's shape or meaning changes.
const EXTRACT_VERSION: u32 = 1;

/// One test a merge-group gate failed on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedTest {
    /// Test name.
    pub name: String,
    /// `CTest`'s result: `Failed`, `Timeout`, ...
    pub result: String,
}

/// What one gate job's log shows about the selection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSelection {
    /// Tests a selection step ran, with the outcome of their last run.
    pub ran: BTreeMap<String, String>,
    /// Sum of the `selected` counts the selection steps announced in
    /// `::notice` JSON, when any did.
    pub declared_selected: Option<u64>,
    /// Sum of their `skipped_for_budget` counts: selected, never run.
    pub declared_budget_skipped: Option<u64>,
    /// Test results the job's other steps printed (a full suite run as
    /// evidence, say). They are not the selection.
    pub ran_outside: usize,
    /// Whether any selection step ran in the job; a job that has none (a
    /// stand-in for a change that needs no build, say) selected nothing.
    pub selection_ran: bool,
    /// Selection steps that ran but whose log section was not found.
    pub missing_sections: Vec<String>,
}

/// One pull-request head's selection, over every gate attempt on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadEvidence {
    /// The head's gate attempts were read.
    Read {
        /// Runs on the head commit ([`HEAD_EVENTS`]).
        runs: Vec<u64>,
        /// Whether a selection step ran in any gate attempt; when none did,
        /// nothing was selected.
        selection_ran: bool,
        /// The union of what the attempts ran.
        selection: JobSelection,
    },
    /// Why it could not be read.
    Unread(String),
}

/// One entry of a merge group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupMember {
    /// Pull request; `None` when the queue branch did not name one.
    pub pr: Option<u64>,
    /// Head of the entry's queue branch.
    pub queue_sha: String,
    /// The pull request's head commit, the queue merge's second parent.
    pub head_sha: Option<String>,
    /// What its head ran.
    pub evidence: HeadEvidence,
}

/// One merge-group run whose final gate attempt failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupFailure {
    /// Run id.
    pub run_id: u64,
    /// Run creation; decides the week.
    pub created_at: DateTime<Utc>,
    /// The tests it failed on, or why the log could not be read.
    pub failed: Result<Vec<FailedTest>, String>,
    /// The group's entries, or why they could not be resolved.
    pub members: Result<Vec<GroupMember>, String>,
}

/// One pull-request head's part in a case.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MemberStatus {
    /// Pull request.
    pub pr: Option<u64>,
    /// Its head commit.
    pub head_sha: Option<String>,
    /// Runs read for it ([`HEAD_EVENTS`]).
    pub runs: Vec<u64>,
    /// `ran`, `not_selected` or `unread`.
    pub status: &'static str,
    /// The test's outcome on the head, when it ran there.
    pub outcome: Option<String>,
    /// Why the head is `unread`.
    pub reason: Option<String>,
    /// Tests the head's selection steps ran.
    pub selection_size: Option<usize>,
    /// Whether a selection step ran on the head at all.
    pub selection_ran: Option<bool>,
    /// The selector's own `selected` count, when announced.
    pub declared_selected: Option<u64>,
    /// Selected but never run, when announced.
    pub declared_budget_skipped: Option<u64>,
    /// Tests other steps of the head's gate job ran.
    pub ran_outside: Option<usize>,
}

/// One merge-group test failure and whether a head ran the test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecallCase {
    /// Merge-group run.
    pub run_id: u64,
    /// Its page.
    pub run_url: String,
    /// Failing test.
    pub test: String,
    /// `CTest`'s result for it.
    pub result: String,
    /// `selected`, `missed` or `undetermined`.
    pub verdict: &'static str,
    /// Failed merge-group gates in the week that list the same test; more
    /// than one suggests the base branch, not one group, was red.
    pub failed_gates_with_test: usize,
    /// Why the group's members are unknown, when they are.
    pub reason: Option<String>,
    /// Each entry of the group.
    pub members: Vec<MemberStatus>,
}

/// Selector recall for one week.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SelectionRecall {
    /// Merge-group runs whose final gate attempt failed.
    pub failed_gate_runs: usize,
    /// Of those, runs whose log names failing tests.
    pub runs_with_test_failures: usize,
    /// Runs whose gate failed with no failing test listed.
    pub runs_without_test_failure: usize,
    /// Runs whose gate log could not be read.
    pub runs_unread: usize,
    /// (run, failing test) pairs.
    pub test_failures: usize,
    /// Pairs whose test a head in the group ran.
    pub selected: usize,
    /// Pairs whose test no head in the group ran, every head read.
    pub missed: usize,
    /// Pairs no head ran with at least one head unread.
    pub undetermined: usize,
    /// `selected / (selected + missed)`.
    pub recall: Option<f64>,
    /// Every pair, in run order.
    pub cases: Vec<RecallCase>,
}

/// The base commit a queue branch was built on.
fn queue_branch_base(branch: &str) -> Option<String> {
    let (_, entry) = branch
        .strip_prefix("gh-readonly-queue/")?
        .rsplit_once("/pr-")?;
    let (_, sha) = entry.split_once('-')?;
    (!sha.is_empty()).then(|| sha.to_owned())
}

/// Read the groups and heads behind every failed merge-group gate.
pub fn gather(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    query: &ScorecardQuery,
    listed: &[Value],
    runs: &[MergeGroupRun],
) -> Vec<GroupFailure> {
    let by_head: BTreeMap<String, &Value> = listed
        .iter()
        .filter_map(|run| Some((text(run, "head_sha")?, run)))
        .collect();
    let by_id: BTreeMap<u64, &Value> = listed
        .iter()
        .filter_map(|run| Some((run.get("id").and_then(Value::as_u64)?, run)))
        .collect();
    let mut failed: Vec<&MergeGroupRun> = runs
        .iter()
        .filter(|run| {
            run.gate.as_ref().is_some_and(|gate| {
                gate.status == "completed"
                    && matches!(gate.conclusion.as_deref(), Some("failure" | "timed_out"))
            })
        })
        .collect();
    failed.sort_by_key(|run| (run.created_at, run.run_id));
    let groups = parallel_map(&failed, |run| {
        let failed = run.gate_job_id.map_or_else(
            || Err("the gate job has no id".to_owned()),
            |job| failed_tests(gh, cache, &query.repo, job),
        );
        let entries = match by_id.get(&run.run_id) {
            Some(listed) => group(gh, cache, query, listed, &by_head),
            None => Err("the run is not in the listing".to_owned()),
        };
        (*run, failed, entries)
    });
    // A pull request sits in many groups; read each head once.
    let heads: Vec<String> = groups
        .iter()
        .filter_map(|(_, failed, entries)| {
            failed.as_ref().ok().filter(|tests| !tests.is_empty())?;
            entries.as_ref().ok()
        })
        .flatten()
        .filter_map(|entry| entry.head_sha.clone().ok())
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    let evidence: BTreeMap<&String, HeadEvidence> = heads
        .iter()
        .zip(parallel_map(&heads, |head| {
            head_evidence(gh, cache, query, head)
        }))
        .collect();
    groups
        .into_iter()
        .map(|(run, failed, entries)| {
            let members = entries.map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| GroupMember {
                        evidence: match &entry.head_sha {
                            Ok(head) => evidence.get(head).cloned().unwrap_or_else(|| {
                                HeadEvidence::Unread("not read: no failing test".to_owned())
                            }),
                            Err(error) => HeadEvidence::Unread(error.clone()),
                        },
                        pr: entry.pr,
                        queue_sha: entry.queue_sha,
                        head_sha: entry.head_sha.ok(),
                    })
                    .collect()
            });
            GroupFailure {
                run_id: run.run_id,
                created_at: run.created_at,
                failed,
                members,
            }
        })
        .collect()
}

/// One queue entry before its head is read.
struct Entry {
    pr: Option<u64>,
    queue_sha: String,
    head_sha: Result<String, String>,
}

fn read_log(gh: &SyncGhReader<'_>, repo: &str, job: u64) -> Result<String, String> {
    gh(&strings(&[
        "api",
        &format!("repos/{repo}/actions/jobs/{job}/logs"),
    ]))
    .map_err(|error| format!("log of job {job}: {error}"))
}

fn failed_tests(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    job: u64,
) -> Result<Vec<FailedTest>, String> {
    let key = format!("ctest-failed:v{EXTRACT_VERSION}:{repo}:{job}");
    if let Some(value) = cache.get(&key)
        && let Ok(tests) = serde_json::from_value(value)
    {
        return Ok(tests);
    }
    let text = read_log(gh, repo, job)?;
    let log = JobLog::new(&text);
    let tests: Vec<FailedTest> = ctest_log::failed_tests(log.lines())
        .into_iter()
        .map(|failure| FailedTest {
            name: failure.name,
            result: failure.result,
        })
        .collect();
    if let Ok(value) = serde_json::to_value(&tests) {
        cache.put(&key, &value);
    }
    Ok(tests)
}

/// The group's entries, the run's own first.
fn group(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    query: &ScorecardQuery,
    run: &Value,
    by_head: &BTreeMap<String, &Value>,
) -> Result<Vec<Entry>, String> {
    let created = timestamp(run, "created_at").ok_or("the run has no created_at")?;
    let mut entries = vec![run.clone()];
    let mut base = text(run, "head_branch")
        .as_deref()
        .and_then(queue_branch_base)
        .ok_or("the run's branch is not a merge-queue branch")?;
    loop {
        let ahead = match by_head.get(&base) {
            Some(run) => Some((*run).clone()),
            None => merge_group_run_on(gh, query, &base)?,
        };
        let Some(ahead) = ahead else { break };
        let pr = text(&ahead, "head_branch")
            .as_deref()
            .and_then(queue_branch_pr)
            .ok_or_else(|| format!("{base}: its run's branch names no pull request"))?;
        if landed_as(gh, cache, &query.repo, pr, &base, created)? {
            break;
        }
        if entries.len() >= MAX_GROUP {
            return Err(format!("more than {MAX_GROUP} entries ahead"));
        }
        base = text(&ahead, "head_branch")
            .as_deref()
            .and_then(queue_branch_base)
            .ok_or_else(|| format!("{base}: its run's branch is not a merge-queue branch"))?;
        entries.push(ahead);
    }
    Ok(entries
        .iter()
        .map(|entry| {
            let queue_sha = text(entry, "head_sha").unwrap_or_default();
            let base = text(entry, "head_branch")
                .as_deref()
                .and_then(queue_branch_base);
            Entry {
                pr: text(entry, "head_branch")
                    .as_deref()
                    .and_then(queue_branch_pr),
                head_sha: pr_head(gh, cache, &query.repo, &queue_sha, base.as_deref()),
                queue_sha,
            }
        })
        .collect())
}

/// The merge-group run whose head is `sha`, outside the window's listing.
fn merge_group_run_on(
    gh: &SyncGhReader<'_>,
    query: &ScorecardQuery,
    sha: &str,
) -> Result<Option<Value>, String> {
    let pages = read_pages(
        gh,
        &format!(
            "repos/{}/actions/workflows/{}/runs",
            query.repo, query.workflow
        ),
        &[
            format!("event={MERGE_GROUP_EVENT}"),
            format!("head_sha={sha}"),
            "per_page=100".to_owned(),
        ],
    )?;
    let runs = collect_counted(
        &pages,
        "workflow_runs",
        &format!("merge-group runs on {sha}"),
    )?;
    Ok(runs.into_iter().next())
}

/// Whether pull request `pr` had merged as exactly `sha` by `at`: then `sha`
/// was the base branch when a run created at `at` was built on it.
fn landed_as(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    pr: u64,
    sha: &str,
    at: DateTime<Utc>,
) -> Result<bool, String> {
    let key = format!("pull-merged:{repo}:{pr}");
    let pull = if let Some(value) = cache.get(&key) {
        value
    } else {
        let value = read_json(gh, &strings(&["api", &format!("repos/{repo}/pulls/{pr}")]))?;
        if timestamp(&value, "merged_at").is_some() {
            let settled = serde_json::json!({
                "merged_at": value.get("merged_at"),
                "merge_commit_sha": value.get("merge_commit_sha"),
            });
            cache.put(&key, &settled);
            settled
        } else {
            value
        }
    };
    Ok(text(&pull, "merge_commit_sha").as_deref() == Some(sha)
        && timestamp(&pull, "merged_at").is_some_and(|merged| merged <= at))
}

/// The pull-request head a queue merge commit brought in: its second parent,
/// once its first is the branch's base.
fn pr_head(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    queue_sha: &str,
    base: Option<&str>,
) -> Result<String, String> {
    let key = format!("git-commit-parents:{repo}:{queue_sha}");
    let parents = if let Some(value) = cache.get(&key) {
        value
    } else {
        let commit = read_json(
            gh,
            &strings(&["api", &format!("repos/{repo}/git/commits/{queue_sha}")]),
        )?;
        let parents = Value::Array(
            commit
                .get("parents")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("{queue_sha}: no parents in the answer"))?
                .iter()
                .filter_map(|parent| text(parent, "sha"))
                .map(Value::String)
                .collect(),
        );
        cache.put(&key, &parents);
        parents
    };
    let parents: Vec<&str> = parents
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    match (parents.as_slice(), base) {
        ([first, head], Some(base)) if *first == base => Ok((*head).to_owned()),
        _ => Err(format!(
            "{queue_sha}: parents {parents:?} are not the queue base {base:?} and a pull-request head"
        )),
    }
}

fn head_evidence(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    query: &ScorecardQuery,
    head: &str,
) -> HeadEvidence {
    match read_head(gh, cache, query, head) {
        Ok(evidence) => evidence,
        Err(error) => HeadEvidence::Unread(error),
    }
}

fn read_head(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    query: &ScorecardQuery,
    head: &str,
) -> Result<HeadEvidence, String> {
    let pages = read_pages(
        gh,
        &format!(
            "repos/{}/actions/workflows/{}/runs",
            query.repo, query.workflow
        ),
        &[format!("head_sha={head}"), "per_page=100".to_owned()],
    )?;
    let runs: Vec<Value> = collect_counted(&pages, "workflow_runs", &format!("runs on {head}"))?
        .into_iter()
        .filter(|run| text(run, "event").is_some_and(|event| HEAD_EVENTS.contains(&event.as_str())))
        .collect();
    if runs.is_empty() {
        return Err(format!(
            "no pull_request or workflow_dispatch run of {} on {head}",
            query.workflow
        ));
    }
    let mut run_ids = Vec::new();
    let mut selection_ran = false;
    let mut union = JobSelection::default();
    for run in &runs {
        let Some(id) = run.get("id").and_then(Value::as_u64) else {
            continue;
        };
        run_ids.push(id);
        let jobs = read_run_jobs(
            gh,
            cache,
            &query.repo,
            &RunRef {
                id,
                attempt: run.get("run_attempt").and_then(Value::as_u64),
                completed: text(run, "status").as_deref() == Some("completed"),
            },
        )?;
        for job in jobs
            .iter()
            .filter(|job| text(job, "name").as_deref() == Some(query.gate_job.as_str()))
        {
            if text(job, "conclusion").as_deref() == Some("skipped") {
                continue;
            }
            let selection = job_selection(gh, cache, query, job)?;
            if !selection.missing_sections.is_empty() {
                return Err(format!(
                    "job {}: selection step(s) {} ran but their log section was not found",
                    job.get("id").and_then(Value::as_u64).unwrap_or_default(),
                    selection.missing_sections.join(", ")
                ));
            }
            selection_ran |= selection.selection_ran;
            merge_selection(&mut union, selection);
        }
    }
    run_ids.sort_unstable();
    Ok(HeadEvidence::Read {
        runs: run_ids,
        selection_ran,
        selection: union,
    })
}

fn add(total: Option<u64>, more: Option<u64>) -> Option<u64> {
    match (total, more) {
        (None, None) => None,
        (total, more) => Some(total.unwrap_or(0) + more.unwrap_or(0)),
    }
}

fn merge_selection(union: &mut JobSelection, selection: JobSelection) {
    union.ran.extend(selection.ran);
    union.declared_selected = union.declared_selected.max(selection.declared_selected);
    union.declared_budget_skipped = union
        .declared_budget_skipped
        .max(selection.declared_budget_skipped);
    union.ran_outside = union.ran_outside.max(selection.ran_outside);
}

/// What one gate job ran in its selection steps; cached once the job is
/// complete.
fn job_selection(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    query: &ScorecardQuery,
    job: &Value,
) -> Result<JobSelection, String> {
    let id = job
        .get("id")
        .and_then(Value::as_u64)
        .ok_or("a gate job has no id")?;
    let ran_a_selection_step = ctest_log::job_steps(job).iter().any(|step| {
        step.conclusion.as_deref() != Some("skipped") && query.selection_steps.contains(&step.name)
    });
    if !ran_a_selection_step {
        return Ok(JobSelection::default());
    }
    let completed = text(job, "status").as_deref() == Some("completed");
    let key = format!(
        "selection:v{EXTRACT_VERSION}:{}:{id}:{}",
        query.repo,
        query.selection_steps.join("\u{1f}")
    );
    if completed
        && let Some(value) = cache.get(&key)
        && let Ok(selection) = serde_json::from_value(value)
    {
        return Ok(selection);
    }
    let text = read_log(gh, &query.repo, id)?;
    let selection = selection_from_log(&text, job, &query.selection_steps);
    if completed && let Ok(value) = serde_json::to_value(&selection) {
        cache.put(&key, &value);
    }
    Ok(selection)
}

/// What the selection steps of one job's log ran.
#[must_use]
pub fn selection_from_log(text: &str, job: &Value, wanted: &[String]) -> JobSelection {
    let steps = ctest_log::job_steps(job);
    let log = JobLog::new(text);
    let (lines, found) = log.lines_of(&steps, wanted);
    let ran_steps: BTreeSet<&str> = steps
        .iter()
        .filter(|step| step.conclusion.as_deref() != Some("skipped"))
        .map(|step| step.name.as_str())
        .collect();
    let missing_sections = wanted
        .iter()
        .filter(|name| ran_steps.contains(name.as_str()) && !found.contains(*name))
        .cloned()
        .collect();
    let selection_ran = wanted.iter().any(|name| ran_steps.contains(name.as_str()));
    let mut ran = BTreeMap::new();
    for result in ctest_log::results(&lines) {
        ran.insert(result.name, result.outcome);
    }
    let mut declared_selected = None;
    let mut declared_budget_skipped = None;
    for line in &lines {
        let Some(json) = line.trim().strip_prefix("##[notice]") else {
            continue;
        };
        let Ok(notice) = serde_json::from_str::<Value>(json) else {
            continue;
        };
        declared_selected = add(
            declared_selected,
            notice.get("selected").and_then(Value::as_u64),
        );
        declared_budget_skipped = add(
            declared_budget_skipped,
            notice.get("skipped_for_budget").and_then(Value::as_u64),
        );
    }
    let ran_outside = ctest_log::results(&log.lines_outside(&steps, wanted)).len();
    JobSelection {
        ran,
        declared_selected,
        declared_budget_skipped,
        ran_outside,
        selection_ran,
        missing_sections,
    }
}

fn member_status(member: &GroupMember, test: &str) -> MemberStatus {
    let mut status = MemberStatus {
        pr: member.pr,
        head_sha: member.head_sha.clone(),
        runs: Vec::new(),
        status: "unread",
        outcome: None,
        reason: None,
        selection_size: None,
        selection_ran: None,
        declared_selected: None,
        declared_budget_skipped: None,
        ran_outside: None,
    };
    match &member.evidence {
        HeadEvidence::Unread(reason) => status.reason = Some(reason.clone()),
        HeadEvidence::Read {
            runs,
            selection_ran,
            selection,
        } => {
            status.runs.clone_from(runs);
            status.selection_ran = Some(*selection_ran);
            status.selection_size = Some(selection.ran.len());
            status.declared_selected = selection.declared_selected;
            status.declared_budget_skipped = selection.declared_budget_skipped;
            status.ran_outside = Some(selection.ran_outside);
            match selection.ran.get(test) {
                Some(outcome) => {
                    status.status = "ran";
                    status.outcome = Some(outcome.clone());
                }
                None => status.status = "not_selected",
            }
        }
    }
    status
}

/// Score the failures of one week.
#[must_use]
pub fn compute(repo: &str, failures: &[&GroupFailure]) -> SelectionRecall {
    let mut recall = SelectionRecall {
        failed_gate_runs: failures.len(),
        runs_with_test_failures: 0,
        runs_without_test_failure: 0,
        runs_unread: 0,
        test_failures: 0,
        selected: 0,
        missed: 0,
        undetermined: 0,
        recall: None,
        cases: Vec::new(),
    };
    let mut gates_by_test: BTreeMap<&str, usize> = BTreeMap::new();
    for failure in failures {
        if let Ok(tests) = &failure.failed {
            for test in tests {
                *gates_by_test.entry(test.name.as_str()).or_insert(0) += 1;
            }
        }
    }
    for failure in failures {
        let tests = match &failure.failed {
            Err(_) => {
                recall.runs_unread += 1;
                continue;
            }
            Ok(tests) if tests.is_empty() => {
                recall.runs_without_test_failure += 1;
                continue;
            }
            Ok(tests) => tests,
        };
        recall.runs_with_test_failures += 1;
        for test in tests {
            let (members, reason) = match &failure.members {
                Ok(members) => (
                    members
                        .iter()
                        .map(|member| member_status(member, &test.name))
                        .collect::<Vec<_>>(),
                    None,
                ),
                Err(error) => (Vec::new(), Some(error.clone())),
            };
            let verdict = if members.iter().any(|member| member.status == "ran") {
                "selected"
            } else if reason.is_none() && members.iter().all(|member| member.status != "unread") {
                "missed"
            } else {
                "undetermined"
            };
            match verdict {
                "selected" => recall.selected += 1,
                "missed" => recall.missed += 1,
                _ => recall.undetermined += 1,
            }
            recall.test_failures += 1;
            recall.cases.push(RecallCase {
                run_id: failure.run_id,
                run_url: format!("https://github.com/{repo}/actions/runs/{}", failure.run_id),
                test: test.name.clone(),
                result: test.result.clone(),
                verdict,
                failed_gates_with_test: gates_by_test.get(test.name.as_str()).copied().unwrap_or(0),
                reason,
                members,
            });
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let judged = recall.selected + recall.missed;
    #[allow(clippy::cast_precision_loss)]
    {
        recall.recall = (judged > 0).then(|| round2(recall.selected as f64 / judged as f64));
    }
    recall
}

#[cfg(test)]
mod tests;
