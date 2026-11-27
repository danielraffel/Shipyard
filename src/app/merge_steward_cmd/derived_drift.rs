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
//! 2. Every required check other than the approval context (a verdict waits
//!    for green) has completed, and each failing one is a check run with an
//!    id (a commit status has no job log).
//! 3. A failed run of *this* pull request at the exact head
//!    ([`crate::apply_derived::run_belongs`]) uploaded a configured,
//!    unexpired artifact for that head: the runs read, then one artifact
//!    listing per matching run, newest first, at most [`MAX_RUNS`].
//! 4. The head commit is not itself an apply-derived commit: one that is
//!    already applied the artifact and the drift stayed, so applying again
//!    would loop.
//! 5. Each failing check's job has exactly one failed step, and that step's
//!    `The following tests FAILED:` blocks name at least one test, all of
//!    them drift tests ([`crate::apply_derived::drift_only`]).
//!
//! Any read that fails leaves the ordinary `required_failed` advice. A drift
//! step that fails stops the steps behind it, so tests later in that job are
//! unknown until the next round.

use serde_json::Value;

use super::next_action::NextAction;
use crate::apply_derived::{self, FailingCheck};
use crate::merge_steward::{
    StewardCheckSource, StewardPolicy, StewardPullRequest, selected_required_check,
};

/// Matching failed runs at the head whose artifacts are listed.
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
        let check = selected_required_check(&pr.checks, required)?;
        if !check.status.eq_ignore_ascii_case("COMPLETED") {
            // Still running: "nothing else is red" is not known yet.
            return None;
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
    let run_id = run_with_artifact(read, repo, pr, config)?;
    if head_is_apply_commit(read, repo, &pr.head_sha)? {
        return None;
    }
    let checks: Vec<FailingCheck> = failing
        .into_iter()
        .map(|(context, job_id)| FailingCheck {
            context,
            failed_tests: failed_step_tests(read, repo, job_id),
        })
        .collect();
    apply_derived::drift_only(config, &checks, true)
        .then(|| NextAction::derived_drift(repo, pr.number, &pr.head_sha, run_id))
}

fn json(read: Read<'_>, path: &str) -> Result<Value, String> {
    serde_json::from_str(&read(&["api".to_owned(), path.to_owned()])?)
        .map_err(|error| error.to_string())
}

/// The newest failed run of this pull request at its head that uploaded a
/// configured artifact for that head.
fn run_with_artifact(
    read: Read<'_>,
    repo: &str,
    pr: &StewardPullRequest,
    config: &apply_derived::DerivedFilesConfig,
) -> Option<u64> {
    let head = pr.head_sha.as_str();
    let runs = apply_derived::runs_at_head(&|path| json(read, path), repo, head).ok()?;
    let mut ids: Vec<u64> = runs
        .iter()
        .filter(|run| apply_derived::run_belongs(run, repo, pr.number, head, &pr.head_branch))
        .map(|run| run.id)
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids.into_iter().take(MAX_RUNS).find(|run_id| {
        json(
            read,
            &format!("repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100"),
        )
        .ok()
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
                            && artifact
                                .pointer("/workflow_run/head_sha")
                                .and_then(Value::as_str)
                                .is_some_and(|sha| sha.eq_ignore_ascii_case(head))
                    }),
            )
        })
        .unwrap_or(false)
    })
}

/// Whether the head commit is an apply-derived commit; `None` when unread.
fn head_is_apply_commit(read: Read<'_>, repo: &str, head: &str) -> Option<bool> {
    let commit = json(read, &format!("repos/{repo}/commits/{head}")).ok()?;
    let message = commit.pointer("/commit/message")?.as_str()?;
    Some(message.starts_with(apply_derived::COMMIT_SUBJECT_PREFIX))
}

/// The tests the job's one failed step lists as failed; `None` when the job
/// or its log cannot be read, or it has other than exactly one failed step.
fn failed_step_tests(read: Read<'_>, repo: &str, job_id: u64) -> Option<Vec<String>> {
    let job = json(read, &format!("repos/{repo}/actions/jobs/{job_id}")).ok()?;
    let failed: Vec<&Value> = job
        .get("steps")?
        .as_array()?
        .iter()
        .filter(|step| step.get("conclusion").and_then(Value::as_str) == Some("failure"))
        .collect();
    let [step] = failed.as_slice() else {
        return None;
    };
    let started_at = step.get("started_at")?.as_str()?;
    let log = crate::gate_cost::fetch_job_log(read, repo, job_id).ok()?;
    Some(crate::environment_requeue::failed_tests_in_step(
        &log, started_at,
    ))
}
