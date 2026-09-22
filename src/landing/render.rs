//! Rendering the landing model for a reader and for a machine.
//!
//! The human form leads with the action, because the action is what an agent
//! got wrong: it hand-rebased a backlog that a live queue would have batched.
//! Every unknown is printed as `UNKNOWN` with the boundary that produced it,
//! never elided — a field quietly missing from a report reads as "nothing to
//! say about it", which is the same mistake in a different place.

use std::io::Write;

use crate::landing::placement::Placement;
use crate::landing::{LandingReport, SurfaceOutcome, Verdict};

/// Write the machine-readable form.
pub fn write_json<W: Write>(stdout: &mut W, report: &LandingReport) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(report)
        .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"));
    writeln!(stdout, "{text}")
}

/// Write the human-readable form.
#[allow(clippy::too_many_lines)]
pub fn write_human<W: Write>(stdout: &mut W, report: &LandingReport) -> std::io::Result<()> {
    writeln!(
        stdout,
        "How work lands in {} (base `{}`)",
        report.repo, report.base
    )?;
    writeln!(stdout)?;

    writeln!(stdout, "ACTION")?;
    writeln!(stdout, "  {}", report.enqueue.action.to_uppercase())?;
    if let Some(command) = &report.enqueue.command {
        writeln!(stdout, "  {command}")?;
    }
    writeln!(stdout, "  {}", report.enqueue.rationale)?;
    writeln!(stdout)?;

    writeln!(stdout, "MERGE QUEUE")?;
    match &report.merge_queue.verdict {
        Verdict::Present(config) => {
            writeln!(
                stdout,
                "  PRESENT{}",
                config
                    .ruleset_name
                    .as_deref()
                    .map_or_else(String::new, |name| format!("  ruleset `{name}`"))
            )?;
            writeln!(
                stdout,
                "  enforcement         {}",
                config.enforcement.as_deref().unwrap_or("UNKNOWN")
            )?;
            writeln!(
                stdout,
                "  grouping strategy   {}",
                config.grouping_strategy.as_deref().unwrap_or("UNKNOWN")
            )?;
            writeln!(
                stdout,
                "  merge method        {}",
                config.merge_method.as_deref().unwrap_or("UNKNOWN")
            )?;
            writeln!(
                stdout,
                "  max entries merge   {}",
                optional(config.max_entries_to_merge)
            )?;
            writeln!(
                stdout,
                "  max entries build   {}",
                optional(config.max_entries_to_build)
            )?;
            writeln!(
                stdout,
                "  min entries merge   {}",
                optional(config.min_entries_to_merge)
            )?;
            writeln!(
                stdout,
                "  check timeout (min) {}",
                optional(config.check_response_timeout_minutes)
            )?;
            writeln!(
                stdout,
                "  observed by         {}",
                config.observed_by.join(", ")
            )?;
        }
        Verdict::Absent => writeln!(
            stdout,
            "  ABSENT  every surface that can express a queue was read and none found one"
        )?,
        Verdict::Unknown { boundary, detail } => {
            writeln!(stdout, "  UNKNOWN  boundary={}", boundary.as_str())?;
            writeln!(stdout, "  {detail}")?;
        }
    }
    for note in &report.merge_queue.disagreements {
        writeln!(stdout, "  ! {note}")?;
    }
    writeln!(stdout)?;

    writeln!(stdout, "UP-TO-DATE (STRICT) PROTECTION")?;
    match &report.strict.verdict {
        Verdict::Present(true) => writeln!(stdout, "  ON")?,
        Verdict::Present(false) => writeln!(stdout, "  OFF")?,
        Verdict::Absent => writeln!(stdout, "  NO REQUIRED-CHECK PROTECTION ON THIS BRANCH")?,
        Verdict::Unknown { boundary, detail } => {
            writeln!(stdout, "  UNKNOWN  boundary={}", boundary.as_str())?;
            writeln!(stdout, "  {detail}")?;
        }
    }
    writeln!(stdout, "  {}", report.strict.implication)?;
    writeln!(stdout)?;

    writeln!(
        stdout,
        "REQUIRED CHECKS  (source: {})",
        report.required_checks.contexts_source
    )?;
    if report.required_checks.checks.is_empty() {
        writeln!(stdout, "  none")?;
    }
    for check in &report.required_checks.checks {
        match &check.placement {
            Placement::GithubHosted {
                runner_name,
                runner_group,
                run_id,
                ..
            } => writeln!(
                stdout,
                "  github-hosted  {}  ({runner_name}, group `{runner_group}`, run {run_id})",
                check.context
            )?,
            Placement::SelfHosted {
                runner_name,
                runner_group,
                run_id,
                ..
            } => writeln!(
                stdout,
                "  self-hosted    {}  ({runner_name}, group `{runner_group}`, run {run_id})",
                check.context
            )?,
            Placement::NoEvidence { detail } => {
                writeln!(stdout, "  no-evidence    {}", check.context)?;
                writeln!(stdout, "                 {detail}")?;
            }
            Placement::Unknown { boundary, detail } => {
                writeln!(
                    stdout,
                    "  UNKNOWN        {}  boundary={}",
                    check.context,
                    boundary.as_str()
                )?;
                writeln!(stdout, "                 {detail}")?;
            }
        }
        for conflict in &check.conflicts {
            writeln!(stdout, "                 ! {conflict}")?;
        }
    }
    for note in &report.required_checks.notes {
        writeln!(stdout, "  note: {note}")?;
    }
    writeln!(stdout)?;

    writeln!(stdout, "BACKLOG")?;
    if let Some(unreadable) = &report.backlog.unreadable {
        writeln!(
            stdout,
            "  UNKNOWN  boundary={}",
            unreadable.boundary.as_str()
        )?;
        writeln!(stdout, "  {}", unreadable.detail)?;
    } else {
        for (state, count) in &report.backlog.by_state {
            writeln!(stdout, "  {state:<10} {count}")?;
        }
        writeln!(
            stdout,
            "  auto-merge {}",
            optional(report.backlog.auto_merge_enabled)
        )?;
        writeln!(stdout, "  drafts     {}", optional(report.backlog.drafts))?;
    }
    writeln!(stdout, "  {}", report.backlog.interpretation)?;
    writeln!(stdout)?;

    writeln!(stdout, "SURFACES CONSULTED")?;
    for surface in &report.surfaces {
        match &surface.outcome {
            SurfaceOutcome::Read => {
                writeln!(
                    stdout,
                    "  read          {}  {}",
                    surface.surface, surface.query
                )?;
            }
            SurfaceOutcome::Inexpressible { detail } => {
                writeln!(
                    stdout,
                    "  cannot-say    {}  {}",
                    surface.surface, surface.query
                )?;
                writeln!(stdout, "                {detail}")?;
            }
            SurfaceOutcome::Unreadable { boundary, detail } => {
                writeln!(
                    stdout,
                    "  UNREADABLE    {}  boundary={}",
                    surface.surface,
                    boundary.as_str()
                )?;
                writeln!(stdout, "                {detail}")?;
            }
        }
    }
    for warning in &report.warnings {
        writeln!(stdout, "  warning: {warning}")?;
    }
    writeln!(stdout)?;
    writeln!(stdout, "{} API calls", report.api_calls)?;
    Ok(())
}

fn optional(value: Option<u64>) -> String {
    value.map_or_else(|| "UNKNOWN".to_owned(), |value| value.to_string())
}
