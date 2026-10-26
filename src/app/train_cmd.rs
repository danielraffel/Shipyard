//! `shipyard train`: resolve the repository and GitHub transport, then hand
//! the action to the merge steward's train ([`super::merge_steward_cmd::train`]).

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use super::CliFailure;
use super::cli::TrainCommand;
use super::merge_steward_cmd::train::{Host, TrainAction, train_command};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::landability::gate;

pub(super) fn train<W: Write>(
    command: TrainCommand,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let repo = |repo: Option<String>| {
        repo.filter(|value| !value.trim().is_empty())
            .or_else(|| gate::resolve_repo(&config, cwd))
            .ok_or_else(|| CliFailure::new(1, "No repo detected. Pass --repo OWNER/REPO."))
    };
    let action = match command {
        TrainCommand::Status {
            repo: name,
            base,
            record,
        } => TrainAction::Status {
            repo: repo(name)?,
            base,
            record,
        },
        TrainCommand::Metrics {
            repo: name,
            since_hours,
        } => TrainAction::Metrics {
            repo: repo(name)?,
            since_hours,
        },
        TrainCommand::Pause { reason } => TrainAction::Pause { reason },
        TrainCommand::Resume { reason } => TrainAction::Resume { reason },
    };
    let actions = GitHubActions::from_loaded_config(cwd, &config);
    train_command(&actions, action, &Host::from_env(), state_dir, json, stdout)
}
