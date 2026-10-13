//! `shipyard inspect`: read a repository's CI and say what to change.
//!
//! Read-only and on demand. Nothing on the path of `ship`, `pr`, the guards
//! or the daemon calls it, and it loads nothing until it runs. Today it
//! answers `--profile`: the facts every check will declare `applies_when`
//! over, each with the file line or API row it was read from.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use super::CliFailure;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::inspect::profile::{self, gather};
use crate::landability::gate;

/// Each GitHub read may take this long.
const GITHUB_READ_TIMEOUT: Duration = Duration::from_mins(1);
/// Config key naming a label to show beside the detected ones.
const PROFILE_CONFIG_KEY: &str = "inspect.profile";

/// What the caller asked for.
pub(super) struct InspectArgs {
    /// Print the detected project profile.
    pub profile: bool,
    /// Exact `OWNER/REPO`, when given.
    pub repo: Option<String>,
    /// Branch, when given.
    pub base: Option<String>,
    /// Workflows whose latest run may be read.
    pub max_runs: Option<usize>,
    /// Emit the machine-readable form.
    pub json: bool,
}

/// Run `shipyard inspect`.
pub(super) fn inspect_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    args: InspectArgs,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    if !args.profile {
        return Err(CliFailure::new(
            2,
            "`shipyard inspect` answers `--profile` so far: run `shipyard inspect --profile` to \
             see the facts the checks will apply to.",
        ));
    }
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let repo = args
        .repo
        .filter(|value| !value.trim().is_empty())
        .or_else(|| gate::resolve_repo(&config, cwd))
        .ok_or_else(|| {
            CliFailure::new(
                2,
                "No repo detected. Pass --repo OWNER/REPO or run inside a git clone with a \
                 GitHub origin.",
            )
        })?;
    let base = args
        .base
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| gate::resolve_base(&config));
    let label_override = config
        .get_str(PROFILE_CONFIG_KEY)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    let actions = crate::cloud::GitHubActions::from_loaded_config(cwd, &config);
    let reader = |gh_args: &[String]| {
        actions
            .run_gh_with_timeout(gh_args, GITHUB_READ_TIMEOUT)
            .map_err(|error| error.to_string())
    };
    let (snapshot, api_calls) = gather::gather(
        &reader,
        &gather::GatherOptions {
            repo: &repo,
            base: &base,
            max_runs: args.max_runs.unwrap_or(gather::DEFAULT_MAX_RUNS).min(50),
        },
    );
    let report = profile::detect(&repo, &base, &snapshot, api_calls, label_override);
    if args.json {
        serde_json::to_writer_pretty(&mut *stdout, &report)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        write!(stdout, "{}", profile::render(&report))
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{InspectArgs, inspect_command};
    use crate::app::cli::{Cli, Command};
    use crate::identity::RuntimeMode;

    #[test]
    fn cli_surface_parses_inspect_profile() {
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "inspect",
            "--profile",
            "--repo",
            "o/r",
            "--base",
            "trunk",
            "--max-runs",
            "3",
        ]);
        assert!(cli.json);
        let Command::Inspect {
            profile,
            repo,
            base,
            max_runs,
        } = cli.command
        else {
            panic!("expected inspect");
        };
        assert!(profile);
        assert_eq!(repo.as_deref(), Some("o/r"));
        assert_eq!(base.as_deref(), Some("trunk"));
        assert_eq!(max_runs, Some(3));
    }

    /// Without `--profile` there is nothing to answer yet, and it says what is.
    #[test]
    fn inspect_without_a_question_names_the_one_it_answers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut out = Vec::new();
        let failure = inspect_command(
            RuntimeMode::Isolated,
            dir.path(),
            InspectArgs {
                profile: false,
                repo: Some("o/r".to_owned()),
                base: None,
                max_runs: None,
                json: false,
            },
            &mut out,
        )
        .expect_err("refused");
        assert_eq!(failure.code, 2);
        assert!(
            failure.message.contains("shipyard inspect --profile"),
            "{}",
            failure.message
        );
        assert!(out.is_empty());
    }
}
