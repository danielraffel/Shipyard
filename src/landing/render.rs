//! Rendering the landing model for a reader and for a machine.
//!
//! The human form leads with the action, because the action is what an agent
//! got wrong: it hand-rebased a backlog that a live queue would have batched.
//! Every unknown is printed as `UNKNOWN` with the boundary that produced it,
//! never elided — a field quietly missing from a report reads as "nothing to
//! say about it", which is the same mistake in a different place.

use std::io::Write;

use crate::base_health::BaseHealthFinding;
use crate::base_health::tip::{TipHealth, TipVerdict};
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
    if let Some(jump) = &report.base_jump {
        writeln!(stdout, "  {}", jump.message.to_uppercase())?;
        for command in &jump.commands {
            writeln!(stdout, "    {command}")?;
        }
        writeln!(
            stdout,
            "  Every batch re-formed on a red base inherits the failure; jump the fix first."
        )?;
    }
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

    write_opt_in_targets(stdout, &report.opt_in_targets)?;

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

    writeln!(stdout, "BASE HEALTH")?;
    write_tip(stdout, &report.base_tip)?;
    match &report.base_health {
        BaseHealthFinding::Signal(observation) => {
            writeln!(
                stdout,
                "  detector  {}  run {} at {}",
                observation.signal.status.to_uppercase(),
                observation.run_id,
                observation.observed_at.to_rfc3339()
            )?;
            if !observation.signal.tests.is_empty() {
                writeln!(
                    stdout,
                    "            tests  {}",
                    observation.signal.tests.join(", ")
                )?;
            }
            if matches!(report.base_tip.verdict, TipVerdict::Healthy)
                && matches!(observation.signal.status.as_str(), "suspected" | "poisoned")
            {
                writeln!(
                    stdout,
                    "            ! disagrees with the tip's required jobs; a detector that \
                     reads a run's conclusion counts advisory failures"
                )?;
            }
        }
        BaseHealthFinding::NoSignal { detail } => {
            writeln!(stdout, "  detector  no signal ({detail})")?;
        }
        BaseHealthFinding::Unreadable { detail } => {
            writeln!(stdout, "  detector  UNKNOWN ({detail})")?;
        }
    }
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

fn write_tip<W: Write>(stdout: &mut W, tip: &TipHealth) -> std::io::Result<()> {
    let sha = tip.tip_sha.as_deref().unwrap_or("UNKNOWN");
    writeln!(
        stdout,
        "  {}  tip {sha} of `{}`",
        tip.verdict.label(),
        tip.base
    )?;
    match &tip.verdict {
        TipVerdict::Healthy => {
            let contexts = tip.required_contexts.join(", ");
            writeln!(
                stdout,
                "  required jobs passed on its merge group: {contexts}"
            )?;
        }
        TipVerdict::Red { failing } => {
            for context in failing {
                writeln!(
                    stdout,
                    "  {}  {}  run {} job {}{}",
                    context.context,
                    context.conclusion,
                    context.run_id,
                    context.job_id,
                    context
                        .url
                        .as_deref()
                        .map_or_else(String::new, |url| format!("  {url}"))
                )?;
                if !context.tests.is_empty() {
                    writeln!(stdout, "    tests  {}", context.tests.join(", "))?;
                }
            }
        }
        TipVerdict::Pending { waiting } => {
            writeln!(stdout, "  waiting on {}", waiting.join(", "))?;
        }
        TipVerdict::Unproven { detail } | TipVerdict::Unreadable { detail } => {
            writeln!(stdout, "  {detail}")?;
        }
    }
    // Only the runs that carried a required job; the rest are advisory to
    // this verdict and their conclusions are not it.
    let runs: Vec<String> = tip
        .runs
        .iter()
        .filter(|run| tip.jobs.iter().any(|job| job.run_id == run.id))
        .map(|run| format!("{} ({})", run.id, run.name.as_deref().unwrap_or("?")))
        .collect();
    if !runs.is_empty() {
        writeln!(stdout, "  merge_group runs  {}", runs.join(", "))?;
    }
    Ok(())
}

fn optional(value: Option<u64>) -> String {
    value.map_or_else(|| "UNKNOWN".to_owned(), |value| value.to_string())
}

/// The section naming local Shipyard targets that a plain `shipyard pr` does
/// not run. Omitted when there are none, since then every target runs.
pub(crate) fn write_opt_in_targets<W: Write>(
    stdout: &mut W,
    targets: &[crate::opt_in_targets::OptInTarget],
) -> std::io::Result<()> {
    if targets.is_empty() {
        return Ok(());
    }
    writeln!(stdout, "LOCAL SHIPYARD TARGETS")?;
    for target in targets {
        writeln!(stdout, "  {}", target.line())?;
    }
    writeln!(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base_health::tip::{ContextJob, FailingContext, TipRun};

    fn render(tip: &TipHealth) -> String {
        let mut out = Vec::new();
        write_tip(&mut out, tip).expect("render");
        String::from_utf8(out).expect("utf8")
    }

    #[test]
    fn red_tip_names_the_sha_the_failing_gate_and_its_tests() {
        let tip = TipHealth {
            base: "main".to_owned(),
            tip_sha: Some("abc123".to_owned()),
            verdict: TipVerdict::Red {
                failing: vec![FailingContext {
                    context: "macos".to_owned(),
                    conclusion: "failure".to_owned(),
                    run_id: 7,
                    job_id: 70,
                    url: None,
                    tests: vec!["42 - pulp-test-widgets (Failed)".to_owned()],
                }],
            },
            required_contexts: vec!["macos".to_owned()],
            runs: vec![
                TipRun {
                    id: 7,
                    name: Some("Build and Test".to_owned()),
                    status: Some("completed".to_owned()),
                    conclusion: Some("failure".to_owned()),
                    url: None,
                },
                TipRun {
                    id: 8,
                    name: Some("Unrelated".to_owned()),
                    status: Some("completed".to_owned()),
                    conclusion: Some("success".to_owned()),
                    url: None,
                },
            ],
            jobs: vec![ContextJob {
                context: "macos".to_owned(),
                run_id: 7,
                job_id: 70,
                status: "completed".to_owned(),
                conclusion: Some("failure".to_owned()),
                url: None,
            }],
            api_calls: 4,
        };
        let text = render(&tip);
        assert!(text.contains("RED  tip abc123 of `main`"), "{text}");
        assert!(text.contains("macos  failure  run 7 job 70"), "{text}");
        assert!(text.contains("pulp-test-widgets"), "{text}");
        assert!(
            text.contains("merge_group runs  7 (Build and Test)"),
            "{text}"
        );
        assert!(!text.contains("Unrelated"), "{text}");
    }
}
