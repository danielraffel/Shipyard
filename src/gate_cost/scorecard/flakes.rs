//! Same-SHA flakes: tests that both passed and failed on one head commit
//! within the window, read from the gate job's `CTest` output.
//!
//! The code under test is identical on one commit, so a test with both
//! outcomes there is flaky by definition (Buildkite and Trunk use the same
//! rule). Two sources show both outcomes on one commit:
//!
//! - **retry**: one gate job printed a failure and a pass for the test, as
//!   `ctest --repeat until-pass:N` does when a retry passes, or as two steps
//!   that both run it do;
//! - **rerun**: different gate jobs on the commit (re-run attempts, or
//!   several runs on one pull-request head) disagree.
//!
//! A test's flake count is the number of commits on which it did both. This
//! is a report: it changes no label and quarantines nothing.
//!
//! Every completed gate job of the window's runs is read, so the first run
//! over a window reads one log per job; what a completed job's log yields is
//! cached, never the log.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::Duration;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::ctest_log::{self, JobLog};
use super::super::{
    ReadCache, RunRef, SyncGhReader, collect_counted, parallel_map, read_pages, read_run_jobs,
    strings, text, timestamp,
};
use super::{GRAPHQL_ATTEMPTS, GRAPHQL_RETRY_PAUSE, ScorecardQuery, rfc3339};

/// Events whose runs are read.
pub const EVENTS: [&str; 3] = ["pull_request", "merge_group", "workflow_dispatch"];
/// Tests listed in the report.
pub const TOP: usize = 10;
/// Cache key version: bump when an extract's shape or meaning changes.
const EXTRACT_VERSION: u32 = 1;
/// Run-listing slice: one day keeps each listing under GitHub's cap.
const SLICE_HOURS: i64 = 24;

/// Pass and failure counts of one test in one job.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    /// Results reading `Passed`.
    pub passed: u32,
    /// Results that ran and did not pass.
    pub failed: u32,
}

/// What one gate job's log said about the tests that failed in it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobOutcomes {
    /// Every test with a failed result, with all of its results in the job.
    pub failed: BTreeMap<String, Tally>,
    /// Result lines in the job.
    pub results: usize,
}

/// One completed gate job on a head commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateJob {
    /// Workflow run.
    pub run_id: u64,
    /// Job id.
    pub job_id: u64,
    /// The run's event.
    pub event: String,
    /// The run's head commit.
    pub head_sha: String,
    /// What its log showed, or why it could not be read.
    pub outcomes: Result<JobOutcomes, String>,
    /// For a commit with several gate jobs: the job's results for tests
    /// that failed in another job on the commit and not in this one.
    pub cross: Result<BTreeMap<String, Tally>, String>,
}

/// Everything [`compute`] needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlakeObservation {
    /// Completed gate jobs of the window's runs.
    pub jobs: Vec<GateJob>,
    /// Runs whose jobs could not be listed.
    pub unlisted_runs: Vec<(u64, String)>,
}

/// One commit on which a test both passed and failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FlakeOccurrence {
    /// The head commit.
    pub head_sha: String,
    /// `retry` (one job showed both) or `rerun` (jobs disagreed).
    pub kind: &'static str,
    /// Failed results on the commit.
    pub failed: u32,
    /// Passed results on the commit.
    pub passed: u32,
    /// The gate jobs on the commit that ran the test.
    pub jobs: Vec<String>,
}

/// One flaky test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FlakyTest {
    /// Test name.
    pub name: String,
    /// Commits on which it both passed and failed.
    pub flake_count: usize,
    /// Of those, commits where one job showed both.
    pub retry: usize,
    /// Of those, commits where only different jobs disagreed.
    pub rerun: usize,
    /// Each commit, newest job first.
    pub occurrences: Vec<FlakeOccurrence>,
}

/// The same-SHA flake list for a window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SameShaFlakes {
    /// Events read.
    pub events: Vec<String>,
    /// Gate jobs whose log was read.
    pub jobs_read: usize,
    /// Gate jobs whose log could not be read, with why (first ten).
    pub jobs_unread: usize,
    /// Reasons for the first unread jobs and unlisted runs.
    pub unread_examples: Vec<String>,
    /// Runs whose jobs could not be listed.
    pub runs_unlisted: usize,
    /// Distinct head commits with a gate job read.
    pub head_shas: usize,
    /// Of those, commits with more than one gate job read.
    pub shas_with_several_jobs: usize,
    /// Distinct tests that both passed and failed on some commit.
    pub flaky_tests: usize,
    /// The [`TOP`] tests with the most such commits.
    pub top: Vec<FlakyTest>,
}

/// `read` again while GitHub answers a gateway error (`HTTP 502`, `503` or
/// `504`), as a page of runs or a large log does under load; any other
/// failure, or the last attempt's, is returned. One such answer must not
/// cost a window that read thousands of logs.
fn retrying<T>(read: impl Fn() -> Result<T, String>) -> Result<T, String> {
    let mut attempt = 1;
    loop {
        match read() {
            Err(error)
                if attempt < GRAPHQL_ATTEMPTS
                    && ["HTTP 502", "HTTP 503", "HTTP 504"]
                        .iter()
                        .any(|status| error.contains(status)) =>
            {
                attempt += 1;
                std::thread::sleep(GRAPHQL_RETRY_PAUSE);
            }
            result => return result,
        }
    }
}

/// Read every completed gate job of the window's runs.
///
/// # Errors
///
/// When a run listing cannot be read completely.
pub fn gather(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    query: &ScorecardQuery,
) -> Result<FlakeObservation, String> {
    let mut runs: BTreeMap<u64, Value> = BTreeMap::new();
    for event in EVENTS {
        let mut start = query.from;
        while start < query.to {
            let end = (start + Duration::hours(SLICE_HOURS)).min(query.to);
            let pages = retrying(|| {
                read_pages(
                    gh,
                    &format!(
                        "repos/{}/actions/workflows/{}/runs",
                        query.repo, query.workflow
                    ),
                    &[
                        format!("event={event}"),
                        format!("created={}..{}", rfc3339(start), rfc3339(end)),
                        "per_page=100".to_owned(),
                    ],
                )
            })?;
            for run in collect_counted(&pages, "workflow_runs", &format!("{event} runs"))? {
                let in_window = timestamp(&run, "created_at")
                    .is_some_and(|at| at >= query.from && at < query.to);
                if let (true, Some(id)) = (in_window, run.get("id").and_then(Value::as_u64)) {
                    runs.insert(id, run);
                }
            }
            start = end;
        }
    }
    let runs: Vec<Value> = runs.into_values().collect();
    let listed = parallel_map(&runs, |run| gate_jobs(gh, cache, query, run));
    let mut jobs: Vec<(Value, Value)> = Vec::new();
    let mut unlisted_runs = Vec::new();
    for (run, listed) in runs.iter().zip(listed) {
        match listed {
            Ok(found) => jobs.extend(found.into_iter().map(|job| (run.clone(), job))),
            Err(error) => unlisted_runs.push((
                run.get("id").and_then(Value::as_u64).unwrap_or_default(),
                error,
            )),
        }
    }
    let outcomes = parallel_map(&jobs, |(_, job)| job_outcomes(gh, cache, &query.repo, job));
    let mut read: Vec<GateJob> = jobs
        .iter()
        .zip(outcomes)
        .map(|((run, job), outcomes)| GateJob {
            run_id: run.get("id").and_then(Value::as_u64).unwrap_or_default(),
            job_id: job.get("id").and_then(Value::as_u64).unwrap_or_default(),
            event: text(run, "event").unwrap_or_default(),
            head_sha: text(run, "head_sha").unwrap_or_default(),
            outcomes,
            cross: Ok(BTreeMap::new()),
        })
        .collect();
    // Each job's results for the tests that failed in another job on its
    // commit and not in it; a commit with one job asks nothing.
    let mut failed_on: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for job in &read {
        if let Ok(outcomes) = &job.outcomes {
            failed_on
                .entry(job.head_sha.clone())
                .or_default()
                .extend(outcomes.failed.keys().cloned());
        }
    }
    let wanted: Vec<(usize, BTreeSet<String>)> = read
        .iter()
        .enumerate()
        .filter_map(|(index, job)| {
            let own = job.outcomes.as_ref().ok()?;
            let names: BTreeSet<String> = failed_on
                .get(&job.head_sha)?
                .iter()
                .filter(|name| !own.failed.contains_key(*name))
                .cloned()
                .collect();
            (!names.is_empty()).then_some((index, names))
        })
        .collect();
    let cross = parallel_map(&wanted, |(index, names)| {
        let job = &jobs[*index].1;
        tallies_of(gh, cache, &query.repo, job, names)
    });
    for ((index, _), tallies) in wanted.iter().zip(cross) {
        read[*index].cross = tallies;
    }
    Ok(FlakeObservation {
        jobs: read,
        unlisted_runs,
    })
}

/// The completed gate jobs of one run that got a runner, every attempt.
fn gate_jobs(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    query: &ScorecardQuery,
    run: &Value,
) -> Result<Vec<Value>, String> {
    let id = run
        .get("id")
        .and_then(Value::as_u64)
        .ok_or("a run has no id")?;
    let jobs = retrying(|| {
        read_run_jobs(
            gh,
            cache,
            &query.repo,
            &RunRef {
                id,
                attempt: run.get("run_attempt").and_then(Value::as_u64),
                completed: text(run, "status").as_deref() == Some("completed"),
            },
        )
    })?;
    Ok(jobs
        .into_iter()
        .filter(|job| text(job, "name").as_deref() == Some(query.gate_job.as_str()))
        .filter(|job| text(job, "status").as_deref() == Some("completed"))
        .filter(|job| text(job, "conclusion").as_deref() != Some("skipped"))
        .filter(|job| text(job, "runner_name").is_some_and(|name| !name.trim().is_empty()))
        .collect())
}

fn read_log(gh: &SyncGhReader<'_>, repo: &str, job: u64) -> Result<String, String> {
    retrying(|| {
        gh(&strings(&[
            "api",
            &format!("repos/{repo}/actions/jobs/{job}/logs"),
        ]))
    })
    .map_err(|error| format!("log of job {job}: {error}"))
}

fn job_id(job: &Value) -> Result<u64, String> {
    job.get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| "a gate job has no id".to_owned())
}

/// The tests that failed in one job, with all their results there; cached.
fn job_outcomes(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    job: &Value,
) -> Result<JobOutcomes, String> {
    let id = job_id(job)?;
    let key = format!("ctest-outcomes:v{EXTRACT_VERSION}:{repo}:{id}");
    if let Some(value) = cache.get(&key)
        && let Ok(outcomes) = serde_json::from_value(value)
    {
        return Ok(outcomes);
    }
    let outcomes = outcomes_from_log(&read_log(gh, repo, id)?);
    if let Ok(value) = serde_json::to_value(&outcomes) {
        cache.put(&key, &value);
    }
    Ok(outcomes)
}

/// The tests that failed in one job's log, with every result they had there.
#[must_use]
pub fn outcomes_from_log(text: &str) -> JobOutcomes {
    let log = JobLog::new(text);
    let results = ctest_log::results(log.lines());
    let failed: BTreeSet<&str> = results
        .iter()
        .filter(|result| result.failed())
        .map(|result| result.name.as_str())
        .collect();
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    for result in results
        .iter()
        .filter(|result| failed.contains(result.name.as_str()))
    {
        let tally = tallies.entry(result.name.clone()).or_default();
        if result.passed() {
            tally.passed += 1;
        } else if result.failed() {
            tally.failed += 1;
        }
    }
    JobOutcomes {
        failed: tallies,
        results: results.len(),
    }
}

/// One job's results for `names`; cached under the names' digest.
fn tallies_of(
    gh: &SyncGhReader<'_>,
    cache: &ReadCache,
    repo: &str,
    job: &Value,
    names: &BTreeSet<String>,
) -> Result<BTreeMap<String, Tally>, String> {
    let id = job_id(job)?;
    let mut digest = Sha256::new();
    for name in names {
        digest.update(name.as_bytes());
        digest.update([0]);
    }
    let key = format!(
        "ctest-tallies:v{EXTRACT_VERSION}:{repo}:{id}:{:x}",
        digest.finalize()
    );
    if let Some(value) = cache.get(&key)
        && let Ok(tallies) = serde_json::from_value(value)
    {
        return Ok(tallies);
    }
    let text = read_log(gh, repo, id)?;
    let log = JobLog::new(&text);
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    for result in ctest_log::results(log.lines()) {
        if !names.contains(&result.name) {
            continue;
        }
        let tally = tallies.entry(result.name.clone()).or_default();
        if result.passed() {
            tally.passed += 1;
        } else if result.failed() {
            tally.failed += 1;
        }
    }
    if let Ok(value) = serde_json::to_value(&tallies) {
        cache.put(&key, &value);
    }
    Ok(tallies)
}

fn job_url(repo: &str, job: &GateJob) -> String {
    format!(
        "https://github.com/{repo}/actions/runs/{}/job/{}",
        job.run_id, job.job_id
    )
}

/// The flake list.
#[must_use]
pub fn compute(repo: &str, observation: &FlakeObservation) -> SameShaFlakes {
    let mut unread_examples: Vec<String> = Vec::new();
    let mut jobs_read = 0;
    let mut jobs_unread = 0;
    let mut by_sha: BTreeMap<&str, Vec<&GateJob>> = BTreeMap::new();
    for job in &observation.jobs {
        match &job.outcomes {
            Ok(_) => {
                jobs_read += 1;
                by_sha.entry(job.head_sha.as_str()).or_default().push(job);
            }
            Err(error) => {
                jobs_unread += 1;
                if unread_examples.len() < 10 {
                    unread_examples.push(format!("job {}: {error}", job.job_id));
                }
            }
        }
        if let Err(error) = &job.cross
            && unread_examples.len() < 10
        {
            unread_examples.push(format!(
                "job {} (other jobs' failures): {error}",
                job.job_id
            ));
        }
    }
    for (run, error) in &observation.unlisted_runs {
        if unread_examples.len() < 10 {
            unread_examples.push(format!("run {run}: {error}"));
        }
    }
    let mut flaky: BTreeMap<String, Vec<FlakeOccurrence>> = BTreeMap::new();
    for (sha, jobs) in &by_sha {
        // Per test: (results on the commit, a single job showed both, jobs).
        let mut tests: BTreeMap<&str, (Tally, bool, Vec<String>)> = BTreeMap::new();
        for job in jobs {
            let Ok(outcomes) = &job.outcomes else {
                continue;
            };
            let cross = job.cross.as_ref().ok();
            let seen = outcomes.failed.iter().chain(cross.into_iter().flatten());
            for (name, tally) in seen {
                if tally.passed + tally.failed == 0 {
                    continue;
                }
                let entry = tests.entry(name.as_str()).or_default();
                entry.0.passed += tally.passed;
                entry.0.failed += tally.failed;
                entry.1 |= tally.passed > 0 && tally.failed > 0;
                entry.2.push(job_url(repo, job));
            }
        }
        for (name, (tally, retry, jobs)) in tests {
            if tally.passed > 0 && tally.failed > 0 {
                flaky
                    .entry(name.to_owned())
                    .or_default()
                    .push(FlakeOccurrence {
                        head_sha: (*sha).to_owned(),
                        kind: if retry { "retry" } else { "rerun" },
                        failed: tally.failed,
                        passed: tally.passed,
                        jobs,
                    });
            }
        }
    }
    let flaky_tests = flaky.len();
    let mut top: Vec<FlakyTest> = flaky
        .into_iter()
        .map(|(name, occurrences)| FlakyTest {
            flake_count: occurrences.len(),
            retry: occurrences.iter().filter(|o| o.kind == "retry").count(),
            rerun: occurrences.iter().filter(|o| o.kind == "rerun").count(),
            name,
            occurrences,
        })
        .collect();
    top.sort_by(|a, b| {
        b.flake_count
            .cmp(&a.flake_count)
            .then_with(|| a.name.cmp(&b.name))
    });
    top.truncate(TOP);
    SameShaFlakes {
        events: EVENTS.iter().map(|event| (*event).to_owned()).collect(),
        jobs_read,
        jobs_unread,
        unread_examples,
        runs_unlisted: observation.unlisted_runs.len(),
        head_shas: by_sha.len(),
        shas_with_several_jobs: by_sha.values().filter(|jobs| jobs.len() > 1).count(),
        flaky_tests,
        top,
    }
}

/// Human rendering.
#[must_use]
pub fn render(flakes: &SameShaFlakes) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "same-SHA flakes ({} runs): {} test(s) passed and failed on one commit; {} gate jobs read \
         on {} commits ({} with several jobs), {} unread, {} runs unlisted",
        flakes.events.join(", "),
        flakes.flaky_tests,
        flakes.jobs_read,
        flakes.head_shas,
        flakes.shas_with_several_jobs,
        flakes.jobs_unread,
        flakes.runs_unlisted
    );
    for test in &flakes.top {
        let _ = writeln!(
            out,
            "  {} on {} commit(s) ({} retry, {} rerun): {}",
            test.name,
            test.flake_count,
            test.retry,
            test.rerun,
            test.occurrences
                .iter()
                .take(3)
                .flat_map(|occurrence| occurrence.jobs.iter().take(1))
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    for example in &flakes.unread_examples {
        let _ = writeln!(out, "  unread: {example}");
    }
    out
}

#[cfg(test)]
mod tests;
