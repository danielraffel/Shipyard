//! `shipyard reuse switch|trip`: read the live-reuse kill switch, or turn it
//! off and file the tracking issue. The policy is in
//! [`crate::changed_surface::live_switch`]; this is the `gh` half.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use chrono::Utc;
use serde_json::Value;

use super::CliFailure;
use super::cli::{ReuseCommand, ReuseSwitchArgs, ReuseTripArgs};
use crate::changed_surface::live_switch::{self, SwitchMode};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::landability::gate;
use crate::output::write_json_envelope;

pub(super) fn reuse_command<W: Write>(
    command: ReuseCommand,
    config: &LoadedConfig,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let actions = GitHubActions::from_loaded_config(cwd, config);
    let gh = |args: &[String]| actions.run_gh(args).map_err(|error| error.to_string());
    match command {
        ReuseCommand::Switch(args) => switch(&gh, &args, config, cwd, json, stdout),
        ReuseCommand::Trip(args) => trip(&gh, &args, config, cwd, json, stdout),
    }
}

fn repo(explicit: Option<&str>, config: &LoadedConfig, cwd: &Path) -> Result<String, CliFailure> {
    explicit
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| gate::resolve_repo(config, cwd))
        .ok_or_else(|| CliFailure::new(1, "No repo detected. Pass --repo OWNER/REPO."))
}

fn emit<W: Write>(
    stdout: &mut W,
    json: bool,
    command: &str,
    data: BTreeMap<String, Value>,
    human: &str,
) -> Result<(), CliFailure> {
    if json {
        write_json_envelope(stdout, command, data)
            .map_err(|error| CliFailure::new(1, error.to_string()))
    } else {
        writeln!(stdout, "{human}").map_err(|error| CliFailure::new(1, error.to_string()))
    }
}

fn switch<F, W>(
    gh: &F,
    args: &ReuseSwitchArgs,
    config: &LoadedConfig,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure>
where
    F: Fn(&[String]) -> Result<String, String>,
    W: Write,
{
    let repo = repo(args.repo.as_deref(), config, cwd)?;
    let reading = live_switch::read_switch(gh, &repo, &args.variable, Utc::now());
    let mode = match reading.mode {
        SwitchMode::Live => "live",
        SwitchMode::Shadow => "shadow",
    };
    let mut data = BTreeMap::new();
    data.insert("repo".to_owned(), Value::from(repo.clone()));
    data.insert(
        "reading".to_owned(),
        serde_json::to_value(&reading).map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    emit(
        stdout,
        json,
        "reuse.switch",
        data,
        &format!("{repo} {}: {mode} ({})", reading.variable, reading.reason),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn trip<F, W>(
    gh: &F,
    args: &ReuseTripArgs,
    config: &LoadedConfig,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure>
where
    F: Fn(&[String]) -> Result<String, String>,
    W: Write,
{
    if args.reason.trim().is_empty() {
        return Err(CliFailure::new(
            2,
            "--reason must say why the switch is being turned off",
        ));
    }
    let repo = repo(args.repo.as_deref(), config, cwd)?;
    let outcome = live_switch::trip(
        gh,
        &repo,
        &args.variable,
        &args.reason,
        Utc::now(),
        args.apply,
    )
    .map_err(|error| CliFailure::new(1, error))?;
    let mut data = BTreeMap::new();
    data.insert("repo".to_owned(), Value::from(repo.clone()));
    data.insert(
        "outcome".to_owned(),
        serde_json::to_value(&outcome).map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    let dry = if outcome.applied {
        ""
    } else {
        " (dry run; pass --apply to send)"
    };
    emit(
        stdout,
        json,
        "reuse.trip",
        data,
        &format!("{repo}: {}; {}{dry}", outcome.variable, outcome.issue),
    )?;
    Ok(ExitCode::SUCCESS)
}
