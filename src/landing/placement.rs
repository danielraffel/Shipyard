//! Where each required check actually executes.
//!
//! ## `runs-on:` is a request, not a fact
//!
//! The obvious way to answer "does this gate run on a hosted runner or one of
//! ours" is to read the workflow's `runs-on:`. It is the wrong source twice
//! over. First, the value is routinely an indirection —
//! `fromJSON(vars.SOME_RUNS_ON_JSON)` — whose contents live in repository
//! variables that the workflow file does not contain. Second, even when it is
//! a literal, it names the labels a job *asks for*; whether anything answered,
//! and what, is a property of the run.
//!
//! So placement is derived from a completed job's own `runner_name` and
//! `runner_group_name`. Those fields are written by whatever machine actually
//! picked the job up.
//!
//! ## A skipped job is not evidence
//!
//! A job that never ran reports `runner_name: null`, and on a
//! conditional-heavy workflow most jobs in any given run are skipped. Reading
//! those as "no runner" would report every gate as unplaceable. A job only
//! contributes evidence when it carries a runner identity, which is exactly
//! the set of jobs that executed.

use serde::Serialize;
use serde_json::Value;

use crate::fleet_service::Boundary;

/// GitHub's own runner group for hosted capacity.
const HOSTED_GROUP: &str = "GitHub Actions";

/// Prefix every hosted runner's ephemeral name carries.
const HOSTED_NAME_PREFIX: &str = "GitHub Actions ";

/// Where one required context was last seen executing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "placement", rename_all = "snake_case")]
pub enum Placement {
    /// Ran on GitHub-hosted capacity.
    GithubHosted {
        /// Runner that picked the job up.
        runner_name: String,
        /// Runner group it belonged to.
        runner_group: String,
        /// Labels the job requested.
        requested_labels: Vec<String>,
        /// Run the evidence came from.
        run_id: u64,
    },
    /// Ran on a self-hosted runner.
    SelfHosted {
        /// Runner that picked the job up.
        runner_name: String,
        /// Runner group it belonged to.
        runner_group: String,
        /// Labels the job requested.
        requested_labels: Vec<String>,
        /// Run the evidence came from.
        run_id: u64,
    },
    /// The context was found, but no job carrying a runner identity matched
    /// it in the runs that were read.
    NoEvidence {
        /// What was searched and why it came back empty.
        detail: String,
    },
    /// The runs could not be read at all.
    Unknown {
        /// Which class of limit stopped the read.
        boundary: Boundary,
        /// What came back instead.
        detail: String,
    },
}

/// One required context and where it runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CheckPlacement {
    /// Required status-check context name.
    pub context: String,
    /// Where it was observed executing.
    pub placement: Placement,
    /// Anything that looked internally inconsistent, such as a job on hosted
    /// capacity that nonetheless requested the `self-hosted` label.
    pub conflicts: Vec<String>,
}

/// Placement for every required context, plus how the contexts were obtained.
#[derive(Clone, Debug, Serialize)]
pub struct PlacementFinding {
    /// Which surface supplied the required-context list.
    pub contexts_source: String,
    /// One entry per required context.
    pub checks: Vec<CheckPlacement>,
    /// Instrument problems worth printing.
    pub notes: Vec<String>,
}

/// One executed job, reduced to the fields placement depends on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobObservation {
    /// Job name, which is what a status-check context is named after.
    pub name: String,
    /// Runner identity, absent when the job did not execute.
    pub runner_name: Option<String>,
    /// Runner group, absent when the job did not execute.
    pub runner_group: Option<String>,
    /// Labels the job requested.
    pub requested_labels: Vec<String>,
    /// Run this job belonged to.
    pub run_id: u64,
}

/// Parse `GET /repos/{owner}/{repo}/actions/runs/{id}/jobs`.
#[must_use]
pub fn parse_jobs(raw: &str, run_id: u64) -> Vec<JobObservation> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    value
        .get("jobs")
        .and_then(Value::as_array)
        .map(|jobs| {
            jobs.iter()
                .filter_map(|job| {
                    Some(JobObservation {
                        name: job.get("name")?.as_str()?.to_owned(),
                        runner_name: job
                            .get("runner_name")
                            .and_then(Value::as_str)
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned),
                        runner_group: job
                            .get("runner_group_name")
                            .and_then(Value::as_str)
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned),
                        requested_labels: job
                            .get("labels")
                            .and_then(Value::as_array)
                            .map(|labels| {
                                labels
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        run_id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Classify every required context against the jobs that were observed.
///
/// `observations` should be ordered most-recent-first; the first job carrying
/// a runner identity wins, because a gate that moved between runners should
/// report where it runs now rather than where it used to.
#[must_use]
pub fn classify_checks(
    contexts: &[String],
    observations: &[JobObservation],
    runs_read: usize,
    read_error: Option<&(Boundary, String)>,
) -> Vec<CheckPlacement> {
    contexts
        .iter()
        .map(|context| {
            if let Some((boundary, detail)) = read_error {
                return CheckPlacement {
                    context: context.clone(),
                    placement: Placement::Unknown {
                        boundary: *boundary,
                        detail: detail.clone(),
                    },
                    conflicts: Vec::new(),
                };
            }
            let matched = observations
                .iter()
                .find(|job| job.name == *context && job.runner_name.is_some());
            if let Some(job) = matched {
                return classify_job(context, job);
            }
            // A job that exists but never executed is a different fact from a
            // job that was never found, and only the first tells the reader
            // the name is right and the condition is what kept it from
            // running.
            let named_but_skipped = observations.iter().any(|job| job.name == *context);
            let detail = if named_but_skipped {
                format!(
                    "a job named `{context}` was found but was skipped in every run read, so it \
                     carries no runner identity; the context is satisfied without executing"
                )
            } else {
                format!(
                    "no job named `{context}` in the {runs_read} most recent completed runs; the \
                     context may be produced by a job with a different name, or it may not have \
                     run recently"
                )
            };
            CheckPlacement {
                context: context.clone(),
                placement: Placement::NoEvidence { detail },
                conflicts: Vec::new(),
            }
        })
        .collect()
}

fn classify_job(context: &str, job: &JobObservation) -> CheckPlacement {
    let runner_name = job.runner_name.clone().unwrap_or_default();
    let runner_group = job.runner_group.clone().unwrap_or_default();
    let hosted = runner_name.starts_with(HOSTED_NAME_PREFIX)
        || runner_group.eq_ignore_ascii_case(HOSTED_GROUP);
    let requests_self_hosted = job
        .requested_labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case("self-hosted"));

    let mut conflicts = Vec::new();
    if hosted && requests_self_hosted {
        conflicts.push(format!(
            "`{context}` requested the `self-hosted` label but executed on `{runner_name}` in \
             group `{runner_group}`; the runner identity is the fact and the label is the request"
        ));
    }
    if !hosted && !requests_self_hosted {
        conflicts.push(format!(
            "`{context}` executed on `{runner_name}` in group `{runner_group}`, which is not \
             GitHub-hosted capacity, yet the job did not request `self-hosted`"
        ));
    }

    let placement = if hosted {
        Placement::GithubHosted {
            runner_name,
            runner_group,
            requested_labels: job.requested_labels.clone(),
            run_id: job.run_id,
        }
    } else {
        Placement::SelfHosted {
            runner_name,
            runner_group,
            requested_labels: job.requested_labels.clone(),
            run_id: job.run_id,
        }
    };

    CheckPlacement {
        context: context.to_owned(),
        placement,
        conflicts,
    }
}

/// Parse required contexts from `GET .../branches/{b}/protection`.
#[must_use]
pub fn required_contexts(protection: &Value) -> Vec<String> {
    let mut contexts: Vec<String> = protection
        .pointer("/required_status_checks/contexts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if let Some(checks) = protection
        .pointer("/required_status_checks/checks")
        .and_then(Value::as_array)
    {
        for check in checks {
            if let Some(context) = check.get("context").and_then(Value::as_str)
                && !contexts.iter().any(|existing| existing == context)
            {
                contexts.push(context.to_owned());
            }
        }
    }
    contexts.sort();
    contexts.dedup();
    contexts
}
