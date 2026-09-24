//! `shipyard rescue --superseded-merge-group` — reap zombie merge-group runs.
//!
//! When GitHub re-forms a merge-queue batch it deletes the old
//! `gh-readonly-queue/<base>/pr-<n>-<sha>` ref, but a `merge_group` workflow
//! run already started on that ref keeps running. Its result can never be
//! used, yet it holds a self-hosted runner until it finishes.
//!
//! The detector is the ref itself: a `merge_group` run whose head branch is
//! absent from a *successful* `git ls-remote` of the queue refs is validating
//! a deleted batch. "Is the PR still queued?" is deliberately not the test —
//! a superseded batch's PR usually is, in a newer batch.
//!
//! Safety rules, in order:
//! - Runs are listed BEFORE the refs are read. GitHub creates the queue ref
//!   before the run, so a listed run whose ref is missing from the later
//!   listing had its ref deleted; a brand-new batch cannot be misread.
//! - The ref listing must include the remote's `HEAD` as an in-band control.
//!   A listing without it (wrong remote, truncated output) is uncertainty, and
//!   uncertainty keeps every run.
//! - A run with zero active jobs is the ghost shape GitHub will not cancel
//!   (HTTP 409 on cancel and force-cancel); it holds no runner, so it is
//!   skipped rather than fought. A 409 at cancel time is recorded the same way.
//! - Only `merge_group` runs that pass the bulk-cancellation policy (release
//!   workflows are always protected) are ever considered.
//! - `--apply` honours the machine's merge-queue hold
//!   (`shipyard merge-queue hold`) and refuses before any mutation.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use serde_json::{Value, json};

use super::{CliFailure, RESCUE_EVENT, RESCUE_LIST_MAX_PAGES};
use crate::cloud::{GitHubActions, QueuedRun};
use crate::output::write_json_envelope;
use crate::workflow_cancellation::is_bulk_run_cancellation_safe;

/// Prefix GitHub uses for every speculative merge-queue branch.
const QUEUE_REF_PREFIX: &str = "gh-readonly-queue/";

/// Run statuses that can still hold (or be about to take) a runner.
const IN_FLIGHT_STATUSES: [&str; 3] = ["in_progress", "queued", "waiting"];

/// Inputs for one reap pass.
pub(super) struct ReapRequest<'a> {
    pub(super) repo: &'a str,
    pub(super) apply: bool,
    pub(super) state_root: &'a Path,
}

/// What `git ls-remote` proved about the live queue refs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum QueueRefs {
    /// The listing succeeded and carried the `HEAD` control line. Holds the
    /// live branch names (without `refs/heads/`).
    Confirmed(BTreeSet<String>),
    /// The listing failed or could not be trusted; nothing may be cancelled.
    Unknown(String),
}

/// Per-run verdict, before any mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Decision {
    /// Ref confirmed absent and the run still has active jobs.
    Cancel { active_jobs: usize },
    /// Ref live, or the evidence was incomplete.
    Keep(String),
    /// Zero-job ghost; GitHub refuses to cancel these and they hold no runner.
    SkipGhost(String),
}

/// Final per-run outcome, after the optional mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Outcome {
    WouldCancel,
    Cancelled,
    Kept,
    SkippedGhost,
    Failed(String),
}

impl Outcome {
    fn label(&self) -> &'static str {
        match self {
            Self::WouldCancel => "would-cancel",
            Self::Cancelled => "cancelled",
            Self::Kept => "keep",
            Self::SkippedGhost => "skipped-ghost",
            Self::Failed(_) => "failed",
        }
    }
}

/// Pick the remote to list: `origin` when it is the repository being reaped,
/// otherwise the repository's GitHub URL.
pub(super) fn queue_ref_remote(cwd: &Path, repo: &str) -> String {
    let origin_slug = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            super::parse_github_repo_slug(String::from_utf8_lossy(&output.stdout).trim())
        });
    if origin_slug.is_some_and(|slug| slug.eq_ignore_ascii_case(repo)) {
        "origin".to_owned()
    } else {
        format!("https://github.com/{repo}.git")
    }
}

/// Run `git ls-remote <remote> HEAD 'refs/heads/gh-readonly-queue/*'`.
pub(super) fn ls_remote_queue_refs(cwd: &Path, remote: &str) -> Result<String, String> {
    let pattern = format!("refs/heads/{QUEUE_REF_PREFIX}*");
    let output = std::process::Command::new("git")
        .args(["ls-remote", remote, "HEAD", &pattern])
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("could not spawn git ls-remote: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-remote {remote} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("ls-remote output: {error}"))
}

/// Parse an ls-remote listing. The `HEAD` line is the control: a successful
/// listing that omits it measured the wrong thing, so it proves nothing.
pub(super) fn parse_queue_refs(listing: Result<String, String>) -> QueueRefs {
    let stdout = match listing {
        Ok(stdout) => stdout,
        Err(error) => return QueueRefs::Unknown(error),
    };
    let mut saw_head = false;
    let mut live = BTreeSet::new();
    for line in stdout.lines() {
        let Some((sha, name)) = line.split_once('\t') else {
            continue;
        };
        if sha.trim().is_empty() {
            continue;
        }
        let name = name.trim();
        if name == "HEAD" {
            saw_head = true;
        } else if let Some(branch) = name.strip_prefix("refs/heads/")
            && branch.starts_with(QUEUE_REF_PREFIX)
        {
            live.insert(branch.to_owned());
        }
    }
    if saw_head {
        QueueRefs::Confirmed(live)
    } else {
        QueueRefs::Unknown(
            "ls-remote succeeded but omitted the HEAD control line; listing not trusted".to_owned(),
        )
    }
}

/// Whether a listed run is a merge-group run this mode may consider at all.
pub(super) fn is_reap_candidate(run: &QueuedRun) -> bool {
    run.event == "merge_group"
        && run.head_branch.starts_with(QUEUE_REF_PREFIX)
        && IN_FLIGHT_STATUSES.contains(&run.status.as_str())
        && is_bulk_run_cancellation_safe(run)
}

/// Decide one candidate. `active_jobs` is only consulted once the ref is
/// confirmed absent, so a live batch never costs a jobs read.
pub(super) fn decide(
    run: &QueuedRun,
    refs: &QueueRefs,
    active_jobs: impl FnOnce() -> Result<usize, String>,
) -> Decision {
    let live = match refs {
        QueueRefs::Unknown(reason) => {
            return Decision::Keep(format!(
                "queue refs unverified ({reason}); never cancel on uncertainty"
            ));
        }
        QueueRefs::Confirmed(live) => live,
    };
    if live.contains(&run.head_branch) {
        return Decision::Keep("queue ref is live".to_owned());
    }
    match active_jobs() {
        Err(error) => Decision::Keep(format!(
            "queue ref absent, but jobs could not be read ({error}); kept"
        )),
        Ok(0) => Decision::SkipGhost(
            "queue ref absent and the run has zero active jobs (ghost shape GitHub will not cancel; holds no runner)"
                .to_owned(),
        ),
        Ok(active_jobs) => Decision::Cancel { active_jobs },
    }
}

/// Entry point for the mode. `list_refs` is invoked exactly once, after runs
/// are listed.
pub(super) fn reap_superseded_merge_groups<W: Write>(
    request: &ReapRequest<'_>,
    actions: &GitHubActions,
    list_refs: impl FnOnce() -> Result<String, String>,
    remote_label: &str,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if request.apply
        && let Some(hold) = crate::merge_queue_control::hold_status(request.state_root)
            .map_err(|error| CliFailure::new(1, error))?
    {
        return Err(CliFailure::new(
            1,
            format!(
                "merge-queue mutations are held on this machine ({hold}); refusing --apply. \
                 Run without --apply to audit, or `shipyard merge-queue resume` first."
            ),
        ));
    }

    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for status in IN_FLIGHT_STATUSES {
        let runs = actions
            .list_runs_with_status_paginated(request.repo, status, None, RESCUE_LIST_MAX_PAGES)
            .map_err(|error| {
                CliFailure::new(1, format!("Could not list {status} runs: {error}"))
            })?;
        for run in runs {
            if is_reap_candidate(&run) && seen.insert(run.database_id) {
                candidates.push(run);
            }
        }
    }

    let refs = parse_queue_refs(list_refs());

    let mut rows = Vec::with_capacity(candidates.len());
    let mut any_failure = matches!(refs, QueueRefs::Unknown(_)) && !candidates.is_empty();
    for run in &candidates {
        let decision = decide(run, &refs, || {
            actions
                .active_jobs(request.repo, run.database_id)
                .map(|jobs| jobs.len())
                .map_err(|error| error.to_string())
        });
        let (outcome, evidence) = act(request, actions, run, &decision);
        if matches!(outcome, Outcome::Failed(_)) {
            any_failure = true;
        }
        rows.push(row(run, &decision, &outcome, &evidence));
    }

    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), json!("rescue"));
        data.insert("mode".to_owned(), json!("superseded-merge-group"));
        data.insert("repo".to_owned(), json!(request.repo));
        data.insert("apply".to_owned(), json!(request.apply));
        data.insert("remote".to_owned(), json!(remote_label));
        data.insert("queue_refs".to_owned(), refs_json(&refs));
        data.insert("candidate_count".to_owned(), json!(candidates.len()));
        data.insert("runs".to_owned(), Value::Array(rows));
        write_json_envelope(stdout, RESCUE_EVENT, data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "{}",
            render_human(request, remote_label, &refs, &rows)
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(if any_failure {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn act(
    request: &ReapRequest<'_>,
    actions: &GitHubActions,
    run: &QueuedRun,
    decision: &Decision,
) -> (Outcome, String) {
    match decision {
        Decision::Keep(reason) => (Outcome::Kept, reason.clone()),
        Decision::SkipGhost(reason) => (Outcome::SkippedGhost, reason.clone()),
        Decision::Cancel { active_jobs } => {
            let evidence = format!(
                "queue ref absent from ls-remote; {active_jobs} active job(s) still hold capacity"
            );
            if !request.apply {
                return (Outcome::WouldCancel, evidence);
            }
            match actions.cancel_workflow_run(request.repo, run.database_id) {
                Ok(()) => (Outcome::Cancelled, evidence),
                Err(error) if error.to_string().contains("409") => (
                    Outcome::SkippedGhost,
                    format!("{evidence}; GitHub refused cancel with 409 (ghost shape), left alone"),
                ),
                Err(error) => {
                    let message = error.to_string();
                    (
                        Outcome::Failed(message.clone()),
                        format!("{evidence}; cancel failed: {message}"),
                    )
                }
            }
        }
    }
}

fn row(run: &QueuedRun, decision: &Decision, outcome: &Outcome, evidence: &str) -> Value {
    json!({
        "run_id": run.database_id,
        "workflow": if run.workflow_name.is_empty() { &run.name } else { &run.workflow_name },
        "branch": run.head_branch,
        "run_status": run.status,
        "created_at": run.created_at,
        "decision": match decision {
            Decision::Cancel { .. } => "cancel",
            Decision::Keep(_) => "keep",
            Decision::SkipGhost(_) => "skip-ghost",
        },
        "status": outcome.label(),
        "evidence": evidence,
        "url": run.url,
    })
}

fn refs_json(refs: &QueueRefs) -> Value {
    match refs {
        QueueRefs::Confirmed(live) => json!({"verified": true, "live": live}),
        QueueRefs::Unknown(reason) => json!({"verified": false, "reason": reason}),
    }
}

fn render_human(
    request: &ReapRequest<'_>,
    remote_label: &str,
    refs: &QueueRefs,
    rows: &[Value],
) -> String {
    let mut lines = Vec::new();
    let mode = if request.apply {
        "apply"
    } else {
        "dry-run; pass --apply to cancel"
    };
    lines.push(format!(
        "Superseded merge_group reap for {} ({mode}).",
        request.repo
    ));
    match refs {
        QueueRefs::Confirmed(live) => {
            lines.push(format!(
                "Queue refs: `git ls-remote {remote_label}` verified (HEAD control present); {} live gh-readonly-queue ref(s).",
                live.len()
            ));
            for branch in live {
                lines.push(format!("  live: {branch}"));
            }
        }
        QueueRefs::Unknown(reason) => lines.push(format!(
            "Queue refs: UNVERIFIED ({reason}). Every run is kept."
        )),
    }
    if rows.is_empty() {
        lines.push("No in-flight merge_group runs.".to_owned());
        return lines.join("\n");
    }
    for row in rows {
        let text = |key: &str| {
            row.get(key)
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_owned()
        };
        let run_id = row
            .get("run_id")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        lines.push(format!(
            "  • run {run_id} {} [{} since {}] {} → {} — {}",
            text("workflow"),
            text("run_status"),
            text("created_at"),
            text("branch"),
            text("status"),
            text("evidence"),
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests;
