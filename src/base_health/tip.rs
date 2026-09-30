//! Read the base branch's tip directly: did the commit at the tip pass the
//! required gate?
//!
//! Under a merge queue with the `MERGE` method the base tip *is* the head of
//! the merge group that landed it, so the `merge_group` workflow runs whose
//! `head_sha` equals the tip carry the gate verdict for exactly that commit.
//! No tree comparison is needed; a SHA lookup is enough.
//!
//! The verdict is read from the **required jobs**, never from a run's
//! conclusion. A run concludes `failure` when any of its jobs fails, including
//! an advisory one, so a run conclusion reports a green tip as red whenever an
//! advisory leg is broken. The required contexts come from branch protection;
//! for GitHub Actions a check-run context is the job's name.
//!
//! A tip with no `merge_group` run was not landed through the queue (a direct
//! or admin push), and is reported as unproven rather than guessed at.
//!
//! Every read is a `GET`: commit, run listing, per-run jobs until every
//! required context is found, and, only when a required job failed, that job's
//! log for the failing test names.

use serde::Serialize;
use serde_json::Value;

use crate::diagnostics::{AutoParser, FailureParser, log_tail_clean};
use crate::validation_signals::GhReader;

/// How many `merge_group` runs for the tip are read at most.
const RUN_SAMPLE: u32 = 30;
/// How many failing required jobs have their log read for test names.
const MAX_LOG_READS: usize = 2;
/// How much of a failing job's log the test-name parsers see.
const LOG_TAIL_BYTES: usize = 256 * 1024;
/// How many failing test names are kept per job.
const MAX_TESTS: usize = 20;

/// The tip verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum TipVerdict {
    /// Every required context ran on the tip's merge group and passed.
    Healthy,
    /// A required context failed on the tip's merge group.
    Red {
        /// The failing required contexts.
        failing: Vec<FailingContext>,
    },
    /// A required context has not finished on the tip's merge group yet.
    Pending {
        /// Required contexts still running, queued, or not yet created.
        waiting: Vec<String>,
    },
    /// Nothing proves the tip either way.
    Unproven {
        /// Why.
        detail: String,
    },
    /// GitHub could not be read.
    Unreadable {
        /// What failed.
        detail: String,
    },
}

impl TipVerdict {
    /// Upper-case label for the human report.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Healthy => "HEALTHY",
            Self::Red { .. } => "RED",
            Self::Pending { .. } => "PENDING",
            Self::Unproven { .. } => "UNPROVEN",
            Self::Unreadable { .. } => "UNKNOWN",
        }
    }
}

/// One failing required context on the tip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FailingContext {
    /// Required context (job name).
    pub context: String,
    /// The job's conclusion.
    pub conclusion: String,
    /// Workflow run the job belongs to.
    pub run_id: u64,
    /// The job.
    pub job_id: u64,
    /// Link to the job, when GitHub gave one.
    pub url: Option<String>,
    /// Failing tests parsed from the job log, when any parser recognised them.
    pub tests: Vec<String>,
}

/// One required job observed on the tip's merge group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContextJob {
    /// Required context (job name).
    pub context: String,
    /// Workflow run.
    pub run_id: u64,
    /// Job.
    pub job_id: u64,
    /// `queued`, `in_progress`, `completed`, ...
    pub status: String,
    /// Conclusion, once completed.
    pub conclusion: Option<String>,
    /// Link to the job.
    pub url: Option<String>,
}

/// One `merge_group` run for the tip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TipRun {
    /// Run id.
    pub id: u64,
    /// Workflow name.
    pub name: Option<String>,
    /// Run status.
    pub status: Option<String>,
    /// Run conclusion. Reported for context only; never the verdict.
    pub conclusion: Option<String>,
    /// Link to the run.
    pub url: Option<String>,
}

/// The tip reading.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TipHealth {
    /// Base branch.
    pub base: String,
    /// Tip SHA, when it could be read.
    pub tip_sha: Option<String>,
    /// The verdict.
    pub verdict: TipVerdict,
    /// Required contexts judged.
    pub required_contexts: Vec<String>,
    /// `merge_group` runs whose `head_sha` is the tip, newest first.
    pub runs: Vec<TipRun>,
    /// Required jobs found on those runs.
    pub jobs: Vec<ContextJob>,
    /// API calls spent.
    pub api_calls: u32,
}

const PASSING: &[&str] = &["success", "neutral"];

fn is_failing(conclusion: &str) -> bool {
    !PASSING.contains(&conclusion) && conclusion != "skipped"
}

/// Judge the tip from its required jobs.
///
/// `runs_complete` says whether every `merge_group` run for the tip has
/// finished: a required context not found on an unfinished run may simply not
/// have been created yet, while one not found once all runs finished was never
/// going to run there.
#[must_use]
pub fn judge(required: &[String], jobs: &[ContextJob], runs_complete: bool) -> TipVerdict {
    if required.is_empty() {
        return TipVerdict::Unproven {
            detail: "no required contexts are configured to judge the tip by".to_owned(),
        };
    }
    let mut failing = Vec::new();
    let mut waiting = Vec::new();
    let mut unproven = Vec::new();
    for context in required {
        let matching: Vec<&ContextJob> =
            jobs.iter().filter(|job| &job.context == context).collect();
        if let Some(job) = matching.iter().find(|job| {
            job.status == "completed" && job.conclusion.as_deref().is_some_and(is_failing)
        }) {
            failing.push(FailingContext {
                context: context.clone(),
                conclusion: job.conclusion.clone().unwrap_or_default(),
                run_id: job.run_id,
                job_id: job.job_id,
                url: job.url.clone(),
                tests: Vec::new(),
            });
        } else if matching.iter().any(|job| job.status != "completed")
            || (matching.is_empty() && !runs_complete)
        {
            waiting.push(context.clone());
        } else if !matching.iter().any(|job| {
            job.conclusion
                .as_deref()
                .is_some_and(|c| PASSING.contains(&c))
        }) {
            unproven.push(if matching.is_empty() {
                format!("`{context}` did not run")
            } else {
                format!("`{context}` was skipped")
            });
        }
    }
    if !failing.is_empty() {
        TipVerdict::Red { failing }
    } else if !waiting.is_empty() {
        TipVerdict::Pending { waiting }
    } else if !unproven.is_empty() {
        TipVerdict::Unproven {
            detail: format!(
                "the tip's merge-group runs finished but {}",
                unproven.join(", ")
            ),
        }
    } else {
        TipVerdict::Healthy
    }
}

struct Reads<'a, 'b> {
    gh: &'a GhReader<'b>,
    calls: u32,
}

impl Reads<'_, '_> {
    fn raw(&mut self, path: &str) -> Result<String, String> {
        self.calls += 1;
        (self.gh)(&["api".to_owned(), path.to_owned()])
    }

    fn json(&mut self, path: &str) -> Result<Value, String> {
        let raw = self.raw(path)?;
        serde_json::from_str(&raw).map_err(|error| format!("malformed JSON from `{path}`: {error}"))
    }
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Read the tip of `base` and judge it by `required` contexts.
#[must_use]
pub fn read_tip(gh: &GhReader<'_>, repo: &str, base: &str, required: &[String]) -> TipHealth {
    let mut reads = Reads { gh, calls: 0 };
    let mut health = TipHealth {
        base: base.to_owned(),
        tip_sha: None,
        verdict: TipVerdict::Unproven {
            detail: String::new(),
        },
        required_contexts: required.to_vec(),
        runs: Vec::new(),
        jobs: Vec::new(),
        api_calls: 0,
    };
    health.verdict = read_into(&mut reads, repo, base, required, &mut health);
    health.api_calls = reads.calls;
    health
}

fn read_into(
    reads: &mut Reads<'_, '_>,
    repo: &str,
    base: &str,
    required: &[String],
    health: &mut TipHealth,
) -> TipVerdict {
    let commit = match reads.json(&format!("repos/{repo}/commits/{base}")) {
        Ok(commit) => commit,
        Err(error) => return TipVerdict::Unreadable { detail: error },
    };
    let Some(tip) = string(&commit, "sha") else {
        return TipVerdict::Unreadable {
            detail: format!("`repos/{repo}/commits/{base}` carries no sha"),
        };
    };
    health.tip_sha = Some(tip.clone());
    let listing = match reads.json(&format!(
        "repos/{repo}/actions/runs?head_sha={tip}&event=merge_group&per_page={RUN_SAMPLE}"
    )) {
        Ok(listing) => listing,
        Err(error) => return TipVerdict::Unreadable { detail: error },
    };
    let runs: Vec<Value> = listing
        .get("workflow_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        // The filter is GitHub's; re-check it so a server that ignored it
        // cannot make another commit's run speak for the tip.
        .filter(|run| {
            string(run, "head_sha").is_some_and(|sha| sha.eq_ignore_ascii_case(&tip))
                && string(run, "event").is_none_or(|event| event == "merge_group")
        })
        .cloned()
        .collect();
    health.runs = runs
        .iter()
        .filter_map(|run| {
            Some(TipRun {
                id: run.get("id").and_then(Value::as_u64)?,
                name: string(run, "name"),
                status: string(run, "status"),
                conclusion: string(run, "conclusion"),
                url: string(run, "html_url"),
            })
        })
        .collect();
    if health.runs.is_empty() {
        return TipVerdict::Unproven {
            detail: format!(
                "no merge_group run built {tip}; it did not land through the queue (direct or \
                 admin push), so no gate ran on it"
            ),
        };
    }
    if required.is_empty() {
        return judge(required, &[], true);
    }
    let runs_complete = health
        .runs
        .iter()
        .all(|run| run.status.as_deref() == Some("completed"));
    for run in health.runs.clone() {
        let jobs = match reads.json(&format!(
            "repos/{repo}/actions/runs/{}/jobs?per_page=100",
            run.id
        )) {
            Ok(jobs) => jobs,
            Err(error) => return TipVerdict::Unreadable { detail: error },
        };
        for job in jobs
            .get("jobs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = string(job, "name") else {
                continue;
            };
            if !required.contains(&name) {
                continue;
            }
            let Some(job_id) = job.get("id").and_then(Value::as_u64) else {
                continue;
            };
            health.jobs.push(ContextJob {
                context: name,
                run_id: run.id,
                job_id,
                status: string(job, "status").unwrap_or_default(),
                conclusion: string(job, "conclusion"),
                url: string(job, "html_url"),
            });
        }
        if required
            .iter()
            .all(|context| health.jobs.iter().any(|job| &job.context == context))
        {
            break;
        }
    }
    let mut verdict = judge(required, &health.jobs, runs_complete);
    attach_test_names(reads, repo, &mut verdict);
    verdict
}

/// Read failing test names from the logs of at most [`MAX_LOG_READS`] failing
/// required jobs. Test names are a convenience: a log that cannot be read
/// leaves the verdict and the job link intact.
fn attach_test_names(reads: &mut Reads<'_, '_>, repo: &str, verdict: &mut TipVerdict) {
    let TipVerdict::Red { failing } = verdict else {
        return;
    };
    for context in failing.iter_mut().take(MAX_LOG_READS) {
        if let Ok(raw) = reads.raw(&format!(
            "repos/{repo}/actions/jobs/{}/logs",
            context.job_id
        )) {
            let mut tests = AutoParser.parse(&log_tail_clean(&raw, LOG_TAIL_BYTES));
            tests.truncate(MAX_TESTS);
            context.tests = tests;
        }
    }
}

#[cfg(test)]
mod tests;
