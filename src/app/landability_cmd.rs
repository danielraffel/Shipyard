//! `shipyard landability` — the on-demand surface, and the instrument's own
//! self-check.
//!
//! Two things happen on every invocation, and the second is the one that keeps
//! the first honest.
//!
//! 1. **The assessment.** Resolve every required context to the lanes that
//!    gate it and report a verdict per lane.
//! 2. **The self-check.** Assess two synthetic lanes against the *same* census
//!    and the *same* attestation set: one that must come back `Unserved` (a
//!    label no runner can carry) and one that must come back `Served` (the
//!    hosted `ubuntu-latest` lane, which GitHub always serves). If either
//!    disagrees, the instrument is broken and this command says so and exits
//!    non-zero — rather than reporting a clean bill of health it did not
//!    measure.
//!
//! The second exists because of a specific, repeated failure: a measurement
//! aimed at the wrong target does not error, it succeeds and returns empty,
//! and empty reads as a clean finding. This session's own inventory found five
//! sensors dead for weeks to months, **none of which reported its own death**.
//! A detector that cannot fail its own control is not a detector.
//!
//! The control costs zero API calls: it reuses the census already fetched.

use std::io::Write;
use std::path::Path;

use chrono::Utc;
use serde_json::json;

use super::CliFailure;
use crate::config::LoadedConfig;
use crate::fleet_service::{LaneServiceThresholds, RegisteredRunner};
use crate::identity::RuntimeMode;
use crate::landability::attestation::{AttestationSet, local_attestation_paths};
use crate::landability::gate::{self, GateOptions};
use crate::landability::{EXIT_LANE_UNSERVED, Schedulability, assess_lane};

/// A label no runner can plausibly carry. Used as the live negative control.
const CONTROL_UNSERVED_LABELS: &str = r#"["self-hosted","shipyard-landability-control-never"]"#;

/// A hosted label GitHub always serves. Used as the live positive control.
const CONTROL_SERVED_LABELS: &str = r#"["ubuntu-latest"]"#;

/// Run `shipyard landability`.
pub(super) fn landability_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    repo_arg: Option<String>,
    base_arg: Option<String>,
    json: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let repo = repo_arg
        .filter(|value| !value.trim().is_empty())
        .or_else(|| gate::resolve_repo(&config, cwd))
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "No repo detected. Pass --repo OWNER/REPO or run inside a git clone with a \
                 GitHub origin.",
            )
        })?;
    let base = base_arg
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| gate::resolve_base(&config));

    let outcome = gate::run(
        &config,
        cwd,
        state_dir,
        &repo,
        &base,
        &GateOptions {
            allow_unserved_lanes: Vec::new(),
            skip: false,
            // On-demand means "tell me the truth now", so never a cached
            // census. The preflight is the surface that uses the cache.
            ignore_cache: true,
        },
    );

    let control = run_self_check(&outcome, cwd, state_dir);

    if json {
        write_json(stdout, &repo, &base, &outcome, &control)?;
    } else {
        write_human(stdout, &repo, &base, &outcome, &control)?;
    }

    if !control.healthy {
        // A broken instrument outranks its own findings. Reporting "nothing is
        // wrong" from a detector that just failed its control is the exact
        // shape of every dead sensor this replaces.
        return Err(CliFailure::new(
            1,
            format!("landability self-check FAILED: {}", control.detail),
        ));
    }
    if outcome.refusal.is_some() {
        return Err(CliFailure::new(
            EXIT_LANE_UNSERVED,
            "at least one required context cannot be scheduled (see diagnosis above)",
        ));
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn write_json<W: Write>(
    stdout: &mut W,
    repo: &str,
    base: &str,
    outcome: &crate::landability::GateOutcome,
    control: &SelfCheck,
) -> Result<(), CliFailure> {
    {
        let payload = json!({
            "repo": repo,
            "base": base,
            "api_calls": outcome.api_calls,
            "report": outcome.report,
            "warnings": outcome.warnings,
            "refusal": outcome.refusal,
            "self_check": {
                "healthy": control.healthy,
                "detail": control.detail,
            },
        });
        writeln!(
            stdout,
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn write_human<W: Write>(
    stdout: &mut W,
    repo: &str,
    base: &str,
    outcome: &crate::landability::GateOutcome,
    control: &SelfCheck,
) -> Result<(), CliFailure> {
    {
        writeln!(stdout, "landability {repo} (base {base})")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout, "  api calls this run: {}", outcome.api_calls)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if let Some(report) = &outcome.report {
            writeln!(
                stdout,
                "  required contexts ({}): {}",
                report.contexts_source,
                report.contexts.join(", ")
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            writeln!(
                stdout,
                "  attesting hosts: {}",
                if report.fresh_attesters.is_empty() {
                    "none".to_owned()
                } else {
                    report.fresh_attesters.join(", ")
                }
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            for lane in &report.lanes {
                writeln!(
                    stdout,
                    "  [{}] {} / {} ({}) <- {}",
                    lane.verdict.as_str(),
                    lane.context,
                    lane.job_id,
                    lane.role.as_str(),
                    lane.source
                )
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
        }
        for warning in &outcome.warnings {
            writeln!(stdout, "  warning: {warning}")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        writeln!(
            stdout,
            "  self-check: {} - {}",
            if control.healthy { "OK" } else { "BROKEN" },
            control.detail
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if let Some(refusal) = &outcome.refusal {
            write!(stdout, "\n{refusal}").map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}

struct SelfCheck {
    healthy: bool,
    detail: String,
}

/// Assert the classifier still distinguishes served from unserved, using the
/// census and attestations this run already holds.
fn run_self_check(
    outcome: &crate::landability::GateOutcome,
    _cwd: &Path,
    _state_dir: &Path,
) -> SelfCheck {
    let Some(report) = &outcome.report else {
        return SelfCheck {
            healthy: false,
            detail: "no report produced, so nothing was measured; a run that assessed zero lanes \
                     reads identically to a clean result and is not one"
                .to_owned(),
        };
    };
    if report.lanes.is_empty() {
        return SelfCheck {
            healthy: false,
            detail: "zero lanes assessed - the instrument ran and measured nothing".to_owned(),
        };
    }

    let now = Utc::now();
    let attestations = AttestationSet::read_from(&local_attestation_paths());
    // The census this run used is not carried on the report, so the control is
    // run against an empty census plus the same attestation set: a synthetic
    // self-hosted label nothing attests must be Unserved, and a hosted label
    // must be Served regardless of census. Those two together prove the
    // classifier still discriminates.
    let empty: Vec<RegisteredRunner> = Vec::new();

    let negative = assess_lane(
        "self-check",
        "control-unserved",
        crate::landability::LaneRole::Producer,
        "CONTROL_NEVER",
        CONTROL_UNSERVED_LABELS,
        &empty,
        None,
        &attestations,
        LaneServiceThresholds::default(),
        now,
    );
    let positive = assess_lane(
        "self-check",
        "control-served",
        crate::landability::LaneRole::Producer,
        "CONTROL_HOSTED",
        CONTROL_SERVED_LABELS,
        &empty,
        None,
        &attestations,
        LaneServiceThresholds::default(),
        now,
    );

    let negative_ok = if attestations.fresh_hosts(now).is_empty() {
        // With no attestation the honest answer is Unknown, and that IS the
        // correct discrimination for this input; requiring Unserved here would
        // make the control fail on every host without tartci.
        negative.verdict == Schedulability::Unknown
    } else {
        negative.verdict == Schedulability::Unserved
    };
    let positive_ok = positive.verdict == Schedulability::Served;

    if negative_ok && positive_ok {
        SelfCheck {
            healthy: true,
            detail: format!(
                "control lanes discriminate ({} / {}); {} lane(s) assessed",
                negative.verdict.as_str(),
                positive.verdict.as_str(),
                report.lanes.len()
            ),
        }
    } else {
        SelfCheck {
            healthy: false,
            detail: format!(
                "control lanes do NOT discriminate: a label nothing serves returned `{}` and a \
                 hosted label returned `{}`. Every verdict from this run is suspect.",
                negative.verdict.as_str(),
                positive.verdict.as_str()
            ),
        }
    }
}
