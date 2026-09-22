//! `shipyard landing` — report how work actually merges in a repository.
//!
//! Placed beside [`super::landability_cmd`] rather than inside `status`
//! because the two answer adjacent questions about the same repository and
//! neither is Shipyard's own state: `landability` asks whether a pull
//! request's required contexts can be *scheduled*, and this asks what happens
//! to a pull request whose contexts are green. `shipyard status` reports
//! Shipyard's queue, targets and evidence; folding a GitHub-policy reading
//! into it would blur the one boundary that keeps that feed trustworthy,
//! which is that everything in it is a fact about Shipyard.
//!
//! The command is read-only. It exits non-zero when the merge mechanism could
//! not be determined, because a caller that scripts around this needs to tell
//! "no queue" from "could not see whether there is a queue", and an exit code
//! is the only part of the output that every caller reads.

use std::io::Write;
use std::path::Path;

use super::CliFailure;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::landability::gate;
use crate::landing::gather::{DEFAULT_MAX_JOB_READS, DEFAULT_RUN_SAMPLE, GatherOptions};
use crate::landing::{EXIT_LANDING_UNKNOWN, gather, render};

/// What the caller asked for, as parsed from the command line.
pub(super) struct LandingArgs {
    /// Exact `OWNER/REPO`, when given.
    pub repo: Option<String>,
    /// Base branch, when given.
    pub base: Option<String>,
    /// How many recent completed runs to consider for placement.
    pub run_sample: Option<usize>,
    /// Upper bound on per-run job reads.
    pub max_job_reads: Option<usize>,
    /// Emit the machine-readable form.
    pub json: bool,
}

/// Run `shipyard landing`.
pub(super) fn landing_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    args: LandingArgs,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let LandingArgs {
        repo: repo_arg,
        base: base_arg,
        run_sample,
        max_job_reads,
        json,
    } = args;
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

    let actions = crate::cloud::GitHubActions::from_loaded_config(cwd, &config);
    let report = gather::gather(
        &actions,
        &GatherOptions {
            repo: &repo,
            base: &base,
            run_sample: run_sample.unwrap_or(DEFAULT_RUN_SAMPLE).clamp(1, 100),
            max_job_reads: max_job_reads.unwrap_or(DEFAULT_MAX_JOB_READS).clamp(0, 50),
        },
    );

    if json {
        render::write_json(stdout, &report)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        render::write_human(stdout, &report)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }

    if report.has_unknown() {
        return Err(CliFailure::new(
            EXIT_LANDING_UNKNOWN,
            "the landing mechanism could not be fully determined; treat the UNKNOWN fields above \
             as unmeasured rather than as absent",
        ));
    }
    Ok(std::process::ExitCode::SUCCESS)
}
