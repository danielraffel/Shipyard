//! Name `shipyard apply-derived` when a pull request is red only on
//! derived-file drift.
//!
//! A `required_failed` row keeps its token (reports, dwell and the scheduling
//! loop match on it); only its next action changes. The advice needs, in
//! order, each read spent only when the one before allowed it:
//!
//! 1. A base config whose `[derived_files]` table names artifacts and drift
//!    tests ([`crate::apply_derived::DerivedFilesConfig::advises`]). Without
//!    one nothing is read.
//! 2. Every failing required check (the approval context aside: a verdict
//!    waits for green) is a check run with an id (a commit status has no
//!    job log).
//! 3. A failed workflow run at the exact head uploaded a configured,
//!    unexpired artifact: one runs read, then one artifact listing per failed
//!    run, newest first, at most [`MAX_RUNS`].
//! 4. Each failing check's job log names at least one failed test, all of
//!    them drift tests ([`crate::apply_derived::drift_only`]).
//!
//! Any read that fails leaves the ordinary `required_failed` advice.

use serde_json::Value;

use super::next_action::NextAction;
use crate::apply_derived::{self, FailingCheck};
use crate::merge_steward::{
    StewardCheckSource, StewardPolicy, StewardPullRequest, selected_required_check,
};

/// Failed runs at the head whose artifacts are listed.
pub(super) const MAX_RUNS: usize = 5;

type Read<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

/// The apply-derived advice for a row, when it applies.
pub(super) fn advice(
    read: Read<'_>,
    repo: &str,
    pr: &StewardPullRequest,
    policy: &StewardPolicy,
    blocked_on: &str,
) -> Option<NextAction> {
    if blocked_on.split(':').next() != Some("required_failed") {
        return None;
    }
    let config = &policy.ejection_guards.derived_files;
    if !config.advises() {
        return None;
    }
    let approval = policy.approval_context.trim();
    let mut failing = Vec::new();
    for required in &policy.required_checks {
        if required.context.eq_ignore_ascii_case(approval) {
            continue;
        }
        let Some(check) = selected_required_check(&pr.checks, required) else {
            continue;
        };
        if !check.status.eq_ignore_ascii_case("COMPLETED") {
            continue;
        }
        let conclusion = check.conclusion.as_deref().unwrap_or_default();
        if ["SUCCESS", "NEUTRAL", "SKIPPED"]
            .iter()
            .any(|ok| conclusion.eq_ignore_ascii_case(ok))
        {
            continue;
        }
        // A commit status's id is not a job id: only a check run has a log.
        if check.source != StewardCheckSource::CheckRun {
            return None;
        }
        failing.push((check.name.clone(), check.check_id?));
    }
    if failing.is_empty() {
        return None;
    }
    let run_id = run_with_artifact(read, repo, &pr.head_sha, config)?;
    let checks: Vec<FailingCheck> = failing
        .into_iter()
        .map(|(context, job_id)| FailingCheck {
            context,
            failed_tests: failed_tests(read, repo, job_id),
        })
        .collect();
    apply_derived::drift_only(config, &checks, true)
        .then(|| NextAction::derived_drift(repo, pr.number, &pr.head_sha, run_id))
}

fn json(read: Read<'_>, path: String) -> Option<Value> {
    serde_json::from_str(&read(&["api".to_owned(), path]).ok()?).ok()
}

/// The newest failed run at `head` that uploaded a configured artifact.
fn run_with_artifact(
    read: Read<'_>,
    repo: &str,
    head: &str,
    config: &apply_derived::DerivedFilesConfig,
) -> Option<u64> {
    let runs = json(
        read,
        format!("repos/{repo}/actions/runs?head_sha={head}&per_page=50"),
    )?;
    let mut failed: Vec<u64> = runs
        .get("workflow_runs")?
        .as_array()?
        .iter()
        .filter(|run| run.get("conclusion").and_then(Value::as_str) == Some("failure"))
        .filter(|run| {
            run.get("head_sha")
                .and_then(Value::as_str)
                .is_none_or(|sha| sha.eq_ignore_ascii_case(head))
        })
        .filter_map(|run| run.get("id").and_then(Value::as_u64))
        .collect();
    failed.sort_unstable_by(|a, b| b.cmp(a));
    failed.into_iter().take(MAX_RUNS).find(|run_id| {
        json(
            read,
            format!("repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100"),
        )
        .and_then(|listing| {
            Some(
                listing
                    .get("artifacts")?
                    .as_array()?
                    .iter()
                    .any(|artifact| {
                        artifact
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| config.mapping(name).is_some())
                            && artifact.get("expired").and_then(Value::as_bool) == Some(false)
                    }),
            )
        })
        .unwrap_or(false)
    })
}

/// The tests a job's log lists as failed; `None` when it cannot be read.
fn failed_tests(read: Read<'_>, repo: &str, job_id: u64) -> Option<Vec<String>> {
    let log = crate::gate_cost::fetch_job_log(read, repo, job_id).ok()?;
    let log = crate::gate_cost::ctest_log::JobLog::new(&log);
    Some(
        crate::gate_cost::ctest_log::failed_tests(log.lines())
            .into_iter()
            .map(|failure| failure.name)
            .collect(),
    )
}
