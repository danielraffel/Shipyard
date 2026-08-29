use std::io::Write;

use serde_json::{Value, json};

use super::post_validation::{
    render_green_pending_merge_readiness, render_green_validation_state_missing,
};
use super::{AppliedStewardHandoff, CliFailure, RenderedDiagnostics, ShipRenderState, fields};
use crate::diagnostics::FailureKind;
use crate::output::write_json_envelope;

pub(super) fn render_json<W: Write>(
    stdout: &mut W,
    pr: u64,
    outcome: &crate::ship::ShipExecutionOutcome,
    state: &ShipRenderState,
    diagnostics: &[RenderedDiagnostics],
    steward_handoff: Option<&AppliedStewardHandoff>,
) -> Result<(), CliFailure> {
    let merged = state.merged();
    // Only the flaky-required wedge carries recovery contexts; every other
    // state leaves this an empty array so the envelope shape stays stable.
    let flaky_recovery: Vec<Value> = match state {
        ShipRenderState::GreenNotMergedFlakyRequired { red_contexts, .. } => red_contexts
            .iter()
            .map(|name| Value::String(name.clone()))
            .collect(),
        _ => Vec::new(),
    };
    let diag_payload: Vec<Value> = diagnostics
        .iter()
        .map(|entry| {
            json!({
                "failed_target": entry.target.target_name,
                "status": entry.target.status,
                "kind": failure_kind_label(entry.kind),
                "cloud_run_id": entry.target.cloud_run_id,
                "cloud_job_id": entry.target.cloud_job_id,
                "cloud_job_url": entry.target.cloud_job_url,
                "failed_step": entry.target.cloud_failed_step,
                "details": entry.details,
            })
        })
        .collect();
    write_json_envelope(
        stdout,
        "ship",
        fields([
            ("pr", Value::from(pr)),
            ("merged", Value::Bool(merged)),
            // `merged:false` alone cannot tell a caller whether validation failed
            // or validation passed and only the merge call broke. These two do.
            ("status", Value::from(state.status())),
            (
                "merge_error",
                state.merge_error().map_or(Value::Null, Value::from),
            ),
            ("run", outcome.job.to_json_value()),
            ("ship_state", json!(outcome.ship_state)),
            (
                "resumed_existing_state",
                Value::Bool(outcome.resumed_existing_state),
            ),
            // Foreground callers classify merge readiness after validation,
            // so the final render state is authoritative even when the
            // execution outcome predates that classification.
            ("post_validation", json!(state.queued_disposition())),
            ("diagnostics", Value::Array(diag_payload)),
            ("flaky_required_recovery", Value::Array(flaky_recovery)),
            (
                "steward_handoff",
                steward_handoff.map_or(Value::Null, |receipt| {
                    json!({
                        "context": "shipyard/steward-handoff",
                        "state": "success",
                        "head": receipt.head,
                        "workstream_id": receipt.workstream_id,
                        "context_url": receipt.context_url,
                        "monitoring_transferred": receipt.monitoring_transferred,
                        "wake_consumer_available": receipt.monitoring_transferred,
                        "publication_work_id": receipt.publication_work_id,
                        "publication_route_ref": receipt.publication_route_ref,
                        "publication_wake_id": receipt.publication_wake_id,
                    })
                }),
            ),
        ]),
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

pub(super) fn render_human<W: Write>(
    stdout: &mut W,
    pr: u64,
    state: &ShipRenderState,
    diagnostics: &[RenderedDiagnostics],
) -> Result<(), CliFailure> {
    let result = match state {
        ShipRenderState::ValidationFailed => render_validation_failed(stdout, pr, diagnostics),
        ShipRenderState::Merged => writeln!(stdout, "PR #{pr} merged. All green."),
        ShipRenderState::GreenNotMerged(error) => render_green_not_merged(stdout, pr, error),
        ShipRenderState::GreenPendingMergeReadiness(detail) => {
            render_green_pending_merge_readiness(stdout, pr, detail)
        }
        ShipRenderState::GreenValidationStateMissing(detail) => {
            render_green_validation_state_missing(stdout, pr, detail)
        }
        ShipRenderState::GreenNotMergedClientDefect(error) => {
            render_green_not_merged_client_defect(stdout, pr, error)
        }
        ShipRenderState::GreenNotMergedHeadSuperseded { validated, current } => {
            render_green_not_merged_head_superseded(stdout, pr, validated, current)
        }
        ShipRenderState::GreenNotMergedFlakyRequired {
            error,
            red_contexts,
        } => render_green_not_merged_flaky(stdout, pr, error, red_contexts),
    };
    result.map_err(|error| CliFailure::new(1, error.to_string()))
}

/// Issue #301 (2/3). The previous render claimed "All green but
/// merge failed" — misleading when the actual cause is GitHub
/// branch protection waiting on checks Shipyard doesn't supervise
/// (e.g. GHA-hosted Linux/Windows still `in_progress` while local
/// macOS already passed). Surface the underlying error verbatim
/// and point the user at the two unblocks they can pick from.
pub(super) fn render_green_not_merged<W: Write>(
    stdout: &mut W,
    pr: u64,
    error: &str,
) -> std::io::Result<()> {
    writeln!(
        stdout,
        "Shipyard-validated targets passed, but the merge attempt was rejected for PR #{pr}:"
    )?;
    writeln!(stdout, "  reason: {error}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "This usually means GitHub branch protection requires checks Shipyard"
    )?;
    writeln!(
        stdout,
        "doesn't supervise (e.g. GHA-hosted Linux/Windows still in_progress). Either:"
    )?;
    writeln!(
        stdout,
        "  * re-run `shipyard ship --pr {pr}` after the remaining checks complete, or"
    )?;
    writeln!(
        stdout,
        "  * enable native auto-merge: `gh pr merge {pr} --squash --auto`"
    )?;
    Ok(())
}

/// Hand-back for a merge blocked by Shipyard's *own* malformed request. The
/// generic [`render_green_not_merged`] guidance is actively wrong here: it blames
/// branch protection and unsupervised checks, sending the reader to investigate a
/// PR that is very likely mergeable. Name the defect, and give the unblock that
/// works — the merge itself is safe to arm, because every gate already passed.
pub(super) fn render_green_not_merged_client_defect<W: Write>(
    stdout: &mut W,
    pr: u64,
    error: &str,
) -> std::io::Result<()> {
    writeln!(
        stdout,
        "Shipyard-validated targets passed. The merge was NOT rejected by GitHub's"
    )?;
    writeln!(
        stdout,
        "branch protection — Shipyard sent a malformed GraphQL request:"
    )?;
    writeln!(stdout, "  reason: {error}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "This is a Shipyard defect, not a problem with PR #{pr}. Waiting will not"
    )?;
    writeln!(
        stdout,
        "clear it and branch protection is not worth investigating. Please report it"
    )?;
    writeln!(
        stdout,
        "with the reason above: https://github.com/danielraffel/Shipyard/issues"
    )?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "To land PR #{pr} now — every gate already passed, so this bypasses nothing:"
    )?;
    writeln!(stdout, "  gh pr merge {pr} --auto")?;
    writeln!(
        stdout,
        "Omit any merge-strategy flag: on a merge-queue-governed branch the queue"
    )?;
    writeln!(
        stdout,
        "owns the strategy and `--squash` is refused. Add it only if the base branch"
    )?;
    writeln!(stdout, "has no merge queue.")?;
    Ok(())
}

/// Hand-back for a merge Shipyard itself refused because the head moved. GitHub
/// never rejected anything here, so the generic branch-protection guidance is
/// wrong twice over: there is no protection rule to inspect, and waiting cannot
/// help — the green evidence describes a commit that is no longer the head. The
/// only fix is to validate the new head.
pub(super) fn render_green_not_merged_head_superseded<W: Write>(
    stdout: &mut W,
    pr: u64,
    validated: &str,
    current: &str,
) -> std::io::Result<()> {
    writeln!(
        stdout,
        "Shipyard-validated targets passed, but PR #{pr} was NOT merged: its head"
    )?;
    writeln!(stdout, "moved after validation completed.")?;
    writeln!(stdout, "  validated: {validated}")?;
    writeln!(stdout, "  live head: {current}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "Merging now would land a commit no target ever validated, so Shipyard"
    )?;
    writeln!(
        stdout,
        "refused. GitHub rejected nothing — branch protection is not involved and"
    )?;
    writeln!(stdout, "waiting will not clear it. Validate the new head:")?;
    writeln!(stdout, "  shipyard ship --pr {pr} --adopt-head")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "If you did not expect the head to move, check for an unpushed local commit"
    )?;
    writeln!(stdout, "or a concurrent push before re-shipping.")?;
    Ok(())
}

/// Recovery guidance for a *flaky required leg* wedge — a required check that is
/// RED on the exact SHA Shipyard validated green. Unlike the generic hand-back,
/// this is a known-recoverable case: re-dispatch the flaky leg and arm the
/// merge, both one-liners. Motivated by the ~hour lost hand-cranking
/// cancel+rerun when the `macos` required leg flaked under runner load.
pub(super) fn render_green_not_merged_flaky<W: Write>(
    stdout: &mut W,
    pr: u64,
    error: &str,
    red_contexts: &[String],
) -> std::io::Result<()> {
    let checks = red_contexts.join(", ");
    writeln!(
        stdout,
        "Shipyard-validated targets passed, but the merge was rejected for PR #{pr}:"
    )?;
    writeln!(stdout, "  reason: {error}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "Required check(s) [{checks}] are RED on the exact SHA Shipyard just"
    )?;
    writeln!(
        stdout,
        "validated green — a flaky required leg, not a real regression. Recover it:"
    )?;
    writeln!(
        stdout,
        "  * re-dispatch the flaky leg:   `shipyard rescue {pr} --rerun-failed`"
    )?;
    writeln!(
        stdout,
        "  * arm the merge for when it's green: `gh pr merge {pr} --squash --auto`"
    )?;
    Ok(())
}

fn render_validation_failed<W: Write>(
    stdout: &mut W,
    pr: u64,
    diagnostics: &[RenderedDiagnostics],
) -> std::io::Result<()> {
    writeln!(stdout, "\u{2717} Validation failed. PR #{pr} not merged.")?;
    if diagnostics.is_empty() {
        writeln!(
            stdout,
            "  (no per-target diagnostics; rerun with --json for raw run state)"
        )?;
        return Ok(());
    }
    for (idx, entry) in diagnostics.iter().enumerate() {
        if idx > 0 {
            writeln!(stdout)?;
        }
        match entry.kind {
            FailureKind::Cancelled => {
                writeln!(
                    stdout,
                    "  \u{223C} Validation cancelled (concurrency-replaced or skipped); not a failure"
                )?;
                writeln!(stdout, "    Target:  {}", entry.target.target_name)?;
            }
            FailureKind::TimedOut => {
                writeln!(
                    stdout,
                    "  \u{2717} Validation timed out{}",
                    entry
                        .target
                        .error_message
                        .as_deref()
                        .map(|m| format!(" — {m}"))
                        .unwrap_or_default(),
                )?;
                writeln!(stdout, "    Target:  {}", entry.target.target_name)?;
            }
            FailureKind::Failed => {
                let provider = entry
                    .target
                    .provider
                    .as_deref()
                    .map(|p| format!(" (cloud={p})"))
                    .unwrap_or_default();
                writeln!(
                    stdout,
                    "    Target:  {}{provider}",
                    entry.target.target_name
                )?;
                if let Some(details) = entry.details.as_ref() {
                    if let Some(job) = details.job.as_ref() {
                        writeln!(stdout, "    Job:     {}", job.name)?;
                        if !job.html_url.is_empty() {
                            writeln!(stdout, "    URL:     {}", job.html_url)?;
                        }
                        if let Some(step) = job.failed_step.as_deref() {
                            writeln!(stdout, "    Step:    \"{step}\"")?;
                        }
                    } else if let Some(run_id) = details.run_id {
                        writeln!(
                            stdout,
                            "    Run ID:  {run_id} (failed-job lookup unavailable)"
                        )?;
                    }
                    if !details.failure_summary.is_empty() {
                        writeln!(stdout, "    Tests:")?;
                        for line in &details.failure_summary {
                            writeln!(stdout, "      {line}")?;
                        }
                        if details.failure_summary_truncated {
                            writeln!(stdout, "      (truncated; see job log for full list)")?;
                        }
                    } else if details.log_tail.is_some() {
                        writeln!(stdout, "    Tests:   (no recognised footer; see job URL)")?;
                    }
                } else if let Some(message) = entry.target.error_message.as_deref() {
                    writeln!(stdout, "    Error:   {message}")?;
                }
            }
        }
    }
    writeln!(
        stdout,
        "    Action:  run `shipyard watch --pr {pr}` to follow recovery, or push fix."
    )?;
    Ok(())
}

fn failure_kind_label(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Cancelled => "cancelled",
        FailureKind::TimedOut => "timed_out",
        FailureKind::Failed => "failed",
    }
}
