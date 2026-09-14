//! The submission gate: the one production caller that turns a lane
//! assessment into a refusal.
//!
//! `assess_lane_service` has existed in this crate, fully tested, with **zero
//! production callers**. A classifier nothing calls is indistinguishable from
//! one that does not exist — which is precisely what the 2026-09-13 outage
//! demonstrated, six hours at a time. This module is the wire.
//!
//! ## Where the gate refuses, and where it declines to
//!
//! It refuses only on [`Schedulability::Unserved`]: a self-hosted label set
//! that no runner in either scope advertises and that no fresh host
//! attestation declares. Everything else — an unreadable census, an
//! unparsable expression, a starved lane, a missing attestation — is printed
//! loudly and allowed through, because each of those is a statement about the
//! *instrument*, and an instrument that cannot see must not be able to stop
//! the fleet.
//!
//! ## Opt-out, and why it is shaped this way
//!
//! `[landability] enabled = false` turns the gate off for a repository whose
//! self-hosted lanes are served by hosts that do not write an attestation. The
//! per-run escape is `--allow-unserved-lane <label>`, which prints the full
//! diagnosis as a warning and proceeds — the same shape as
//! `--allow-unreachable-targets`, and equally never set by automation.

use std::path::Path;

use chrono::Utc;

use super::assess::{AssessInput, assess};
use super::attestation::{AttestationSet, local_attestation_paths};
use super::gather::{FleetFacts, gather, read_workflow_at_ref};
use super::workflow::{WorkflowJob, parse_workflow_jobs};
use super::{LandabilityReport, Schedulability};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::fleet_service::LaneServiceThresholds;

/// Workflow files the gate reads when the config names none.
///
/// Deliberately a short default rather than "every file in the directory": a
/// gate that parsed forty workflows would spend its credibility on expressions
/// it cannot resolve, and the required contexts on this fleet are produced by
/// one file.
pub const DEFAULT_WORKFLOWS: &[&str] = &[".github/workflows/build.yml"];

/// Options the caller controls per invocation.
#[derive(Clone, Debug, Default)]
pub struct GateOptions {
    /// Labels the operator has explicitly waived for this run.
    pub allow_unserved_lanes: Vec<String>,
    /// Skip the gate entirely.
    pub skip: bool,
    /// Ignore the fact cache (used by the live control).
    pub ignore_cache: bool,
}

/// Outcome of the gate.
#[derive(Clone, Debug)]
pub struct GateOutcome {
    /// The full report, when one was produced.
    pub report: Option<LandabilityReport>,
    /// Lines to print as warnings.
    pub warnings: Vec<String>,
    /// Refusal text when the gate blocks.
    pub refusal: Option<String>,
    /// API calls this gate actually made.
    pub api_calls: u32,
}

impl GateOutcome {
    /// An outcome that neither blocks nor warns.
    #[must_use]
    pub fn skipped(reason: &str) -> Self {
        Self {
            report: None,
            warnings: vec![reason.to_owned()],
            refusal: None,
            api_calls: 0,
        }
    }
}

/// Whether the repository has opted into the gate. Default on.
#[must_use]
pub fn enabled(config: &LoadedConfig) -> bool {
    config
        .get("landability.enabled")
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
}

/// Workflow paths to read, from `[landability] workflows` or the default.
#[must_use]
pub fn workflow_paths(config: &LoadedConfig) -> Vec<String> {
    config
        .get("landability.workflows")
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|paths| !paths.is_empty())
        .unwrap_or_else(|| {
            DEFAULT_WORKFLOWS
                .iter()
                .map(|path| (*path).to_owned())
                .collect()
        })
}

/// Required contexts from `[governance] required_status_checks`, the fallback
/// when branch protection is unreadable.
#[must_use]
pub fn configured_contexts(config: &LoadedConfig) -> Vec<String> {
    config
        .get("governance.required_status_checks")
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Run the gate.
///
/// Returns an outcome rather than a `Result` because *every* path here has to
/// produce something printable: a gate that returns an opaque error on its own
/// failure teaches its operator to ignore it.
#[must_use]
pub fn run(
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    repo: &str,
    base: &str,
    options: &GateOptions,
) -> GateOutcome {
    if options.skip {
        return GateOutcome::skipped("landability gate skipped by request");
    }
    if !enabled(config) {
        return GateOutcome::skipped("landability gate disabled by [landability] enabled = false");
    }

    let actions = GitHubActions::from_loaded_config(cwd, config);
    let now = Utc::now();
    let facts: FleetFacts = gather(&actions, state_dir, repo, base, now, options.ignore_cache);

    let mut warnings = facts.warnings.clone();

    let configured = configured_contexts(config);
    let contexts = match &facts.required_contexts {
        Some(live) => {
            // Control: when both sources are readable the live set must cover
            // the configured one. A configured context missing from branch
            // protection means one of the two is wrong, and silently trusting
            // either is how a gate ends up checking nothing.
            for context in &configured {
                if !live.contains(context) {
                    warnings.push(format!(
                        "[governance] required_status_checks names `{context}`, which branch \
                         protection does not require; one of the two is stale"
                    ));
                }
            }
            live.clone()
        }
        None => configured.clone(),
    };

    if contexts.is_empty() {
        return GateOutcome {
            report: None,
            warnings: {
                warnings.push(
                    "landability: no required contexts from branch protection or config - \
                     nothing to check, which is not the same as nothing wrong"
                        .to_owned(),
                );
                warnings
            },
            refusal: None,
            api_calls: facts.api_calls,
        };
    }

    let jobs = read_jobs(config, cwd, base, &mut warnings);
    if jobs.is_empty() {
        warnings.push(
            "landability: no workflow jobs were parsed - the gate measured nothing, which reads \
             identically to a clean result and is not one"
                .to_owned(),
        );
        return GateOutcome {
            report: None,
            warnings,
            refusal: None,
            api_calls: facts.api_calls,
        };
    }

    let attestations = AttestationSet::read_from(&local_attestation_paths());
    let input = AssessInput {
        contexts: &contexts,
        contexts_source: &facts.contexts_source,
        jobs: &jobs,
        variables: &facts.variables,
        census: &facts.census,
        census_boundary: facts.census_boundary,
        variables_boundary: facts.variables_boundary,
        attestations: &attestations,
        thresholds: LaneServiceThresholds::default(),
        allow_unserved: &options.allow_unserved_lanes,
    };
    let report = assess(&input, now);

    describe_report(&report, &mut warnings);

    let refusal = if report.blocking().is_empty() {
        None
    } else {
        Some(report.render_refusal())
    };

    GateOutcome {
        report: Some(report),
        warnings,
        refusal,
        api_calls: facts.api_calls,
    }
}

/// Read the workflow jobs the gate analyses, from the local checkout.
///
/// Zero API calls, and the local checkout is the correct source anyway: the
/// workflow that runs on `pull_request` is the one on the base ref.
fn read_jobs(
    config: &LoadedConfig,
    cwd: &Path,
    base: &str,
    warnings: &mut Vec<String>,
) -> Vec<WorkflowJob> {
    let mut jobs: Vec<WorkflowJob> = Vec::new();
    for path in workflow_paths(config) {
        match read_workflow_at_ref(cwd, &format!("origin/{base}"), &path)
            .or_else(|_| read_workflow_at_ref(cwd, "HEAD", &path))
        {
            Ok(source) => jobs.extend(parse_workflow_jobs(&source)),
            Err(error) => warnings.push(format!(
                "landability: could not read {path} at origin/{base} or HEAD ({error}); the jobs \
                 in it were not checked"
            )),
        }
    }
    jobs
}

/// Turn the non-blocking half of a report into printable warnings.
///
/// Every one of these is a statement about the *instrument* or about a
/// problem with a different owner, and each says which — an unknown that
/// cannot name its boundary sends its reader to the wrong subsystem.
fn describe_report(report: &LandabilityReport, warnings: &mut Vec<String>) {
    warnings.extend(report.warnings.iter().cloned());
    for context in &report.contexts_without_producer {
        warnings.push(format!(
            "landability: required context `{context}` has no producing job in the workflows \
             read - it may come from another workflow or an external App, and was NOT checked"
        ));
    }
    for lane in &report.lanes {
        if lane.verdict == Schedulability::Starved {
            warnings.push(format!(
                "landability: lane `{}` for context `{}` is STARVED - {} (a scheduling problem: \
                 runner-group access, ephemeral consumption, or a workflow permission; not a \
                 runner restore)",
                lane.source, lane.context, lane.report.detail
            ));
        }
        if lane.verdict == Schedulability::Unknown {
            warnings.push(format!(
                "landability: lane `{}` for context `{}` is UNKNOWN - {}",
                lane.source, lane.context, lane.report.detail
            ));
        }
    }
}

/// Resolve the repository slug for the gate.
///
/// `[repository]` in config wins; otherwise the `origin` remote. Returns
/// `None` rather than erroring: a checkout with no GitHub remote is not a
/// landability failure, it is a repository the gate has nothing to say about.
#[must_use]
pub fn resolve_repo(config: &LoadedConfig, cwd: &Path) -> Option<String> {
    if let Some(repo) = config
        .get("repository")
        .and_then(toml::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return Some(repo.to_owned());
    }
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    crate::gh::parse_github_remote_slug(String::from_utf8_lossy(&output.stdout).trim())
}

/// Base branch the gate reads protection and workflows from.
#[must_use]
pub fn resolve_base(config: &LoadedConfig) -> String {
    config
        .get("governance.base_branch")
        .and_then(toml::Value::as_str)
        .or_else(|| config.get("base_branch").and_then(toml::Value::as_str))
        .unwrap_or("main")
        .to_owned()
}
