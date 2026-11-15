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
use crate::gate_cost::ReadCache;
use crate::identity::RuntimeMode;
use crate::landability::gate;
use crate::landing::gather::{DEFAULT_MAX_JOB_READS, DEFAULT_RUN_SAMPLE, GatherOptions};
use crate::landing::{EXIT_LANDING_UNKNOWN, gather, pr_state, render};

fn repository_config_query(repo: &str, base: &str) -> String {
    format!(
        "repos/{repo}/contents/.shipyard/config.toml?ref={}",
        crate::required_check_policy::encode_path_segment(base)
    )
}

/// Which base-config tables Shipyard no longer reads that this config still
/// sets. Rollups and the merge train were removed; nothing depends on these
/// tables, so a repository that keeps them is warned, never failed.
fn retired_tables(table: &toml::Table) -> Vec<&'static str> {
    let mut found = Vec::new();
    if table.contains_key("train") {
        found.push("[train]");
    }
    if table
        .get("auto_merge")
        .and_then(toml::Value::as_table)
        .is_some_and(|auto_merge| auto_merge.contains_key("rollup"))
    {
        found.push("[auto_merge.rollup]");
    }
    found
}

/// The warning for retired tables, when the base config sets any.
fn retired_tables_warning(repo: &str, base: &str, table: &toml::Table) -> Option<String> {
    let found = retired_tables(table);
    (!found.is_empty()).then(|| {
        format!(
            "{} in .shipyard/config.toml on {repo}@{base} are no longer read (rollups and the \
             merge train were removed); delete them",
            found.join(" / ")
        )
    })
}

/// Read the base's tracked config only to warn about retired tables. A read
/// that fails is reported as a surface and changes no finding.
fn retired_config_read(
    actions: &crate::cloud::GitHubActions,
    repo: &str,
    base: &str,
) -> (crate::landing::SurfaceRead, Option<String>) {
    let query = repository_config_query(repo, base);
    let surface = |outcome| crate::landing::SurfaceRead {
        surface: "repository_config".to_owned(),
        query: format!("GET /{query}"),
        outcome,
    };
    let raw = match actions.run_gh(&[
        "api".to_owned(),
        "-H".to_owned(),
        "Accept: application/vnd.github.raw+json".to_owned(),
        query.clone(),
    ]) {
        Ok(raw) => raw,
        Err(error)
            if error.to_string().contains("404") || error.to_string().contains("Not Found") =>
        {
            return (
                surface(crate::landing::SurfaceOutcome::Absent {
                    detail: format!("no .shipyard/config.toml on {repo}@{base}"),
                }),
                None,
            );
        }
        Err(error) => {
            return (
                surface(crate::landing::SurfaceOutcome::Unreadable {
                    boundary: crate::fleet_service::Boundary::Transport,
                    detail: format!(
                        ".shipyard/config.toml on {repo}@{base} is unreadable: {error}"
                    ),
                }),
                None,
            );
        }
    };
    match raw.parse::<toml::Table>() {
        Ok(table) => (
            surface(crate::landing::SurfaceOutcome::Read),
            retired_tables_warning(repo, base, &table),
        ),
        Err(error) => (
            surface(crate::landing::SurfaceOutcome::Unreadable {
                boundary: crate::fleet_service::Boundary::Parse,
                detail: format!(".shipyard/config.toml on {repo}@{base} does not parse: {error}"),
            }),
            None,
        ),
    }
}

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
    /// Classify one pull request's queue state instead of the repo model.
    pub pr: Option<u64>,
    /// Emit the machine-readable form.
    pub json: bool,
    /// Shipyard's state directory, for the read cache that serves completed
    /// job-log signatures to the `--pr` verdict. `None` disables caching.
    pub state_dir: Option<std::path::PathBuf>,
}

/// Run `shipyard landing`.
#[allow(clippy::too_many_lines)] // Keep the read-only landing command's evidence order auditable.
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
        pr,
        json,
        state_dir,
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

    let actions = crate::cloud::GitHubActions::from_loaded_config(cwd, &config)
        // Route the App token to the repository asked about, not the checkout's.
        .with_repo_override(&repo);
    if let Some(pr) = pr {
        let environment_opt_in = config
            .get(crate::environment_requeue::CONFIG_KEY)
            .and_then(toml::Value::as_bool)
            == Some(true);
        let cache = state_dir.map_or_else(ReadCache::disabled, |dir| {
            // The PR watch cache: its signature entries are keyed by job id
            // and never change once a job completed, so both commands share.
            ReadCache::open(
                &dir.join("pr-watch")
                    .join("cache")
                    .join(repo.replace('/', "__").to_ascii_lowercase()),
                std::time::SystemTime::now(),
            )
            .with_job_logs({
                let (job_logs, warnings) =
                    crate::job_logs::JobLogPass::for_state_dir(&dir, &config);
                for warning in warnings {
                    eprintln!("shipyard: {warning}");
                }
                job_logs
            })
        });
        let report = pr_state::gather_cached(&actions, &repo, pr, environment_opt_in, &cache);
        if json {
            pr_state::write_json(stdout, &report)
        } else {
            pr_state::write_human(stdout, &report)
        }
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if report.is_unknown() {
            return Err(CliFailure::new(
                EXIT_LANDING_UNKNOWN,
                format!(
                    "PR #{pr}'s merge-queue state or landing verdict could not be determined; \
                     do not act on it as if it were unarmed, green, or flaky"
                ),
            ));
        }
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let mut report = gather::gather(
        &actions,
        &GatherOptions {
            repo: &repo,
            base: &base,
            run_sample: run_sample.unwrap_or(DEFAULT_RUN_SAMPLE).clamp(1, 100),
            max_job_reads: max_job_reads.unwrap_or(DEFAULT_MAX_JOB_READS).clamp(0, 50),
            base_health_workflow: config
                .get_str(crate::base_health::WORKFLOW_CONFIG_KEY)
                .unwrap_or(crate::base_health::DEFAULT_WORKFLOW),
        },
    );

    let (surface, warning) = retired_config_read(&actions, &repo, &base);
    report.api_calls += 1;
    report.surfaces.push(surface);
    report.warnings.extend(warning);

    report.opt_in_targets = crate::opt_in_targets::from_config(&config);

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

#[cfg(test)]
mod tests;
