use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cloud_records::CloudRecordStore;
use crate::config::LoadedConfig;
use crate::evidence::EvidenceStore;
use crate::identity::RuntimeMode;
use crate::output::write_pretty_json;
use crate::paths::RuntimePaths;
use crate::ship_state::ShipStateStore;
use clap::Parser;

mod auth_cmd;
mod auto_merge_cmd;
mod branch_cmd;
mod capacity_cmd;
mod changed_surface_cmd;
mod changelog_cmd;
mod ci_cmd;
mod cleanup_cmd;
mod cli;
mod cloud_cmd;
mod cloud_read_cmd;
mod command_evidence_cmd;
mod config_cmd;
mod daemon_cmd;
mod dependency_cmd;
mod doctor_cmd;
mod execution_worker_cmd;
mod fleet_status_cmd;
mod fleet_update_cmd;
mod governance_cmd;
mod init_cmd;
mod local_linux_lease_cmd;
mod merge_queue_control_cmd;
mod merge_steward_cmd;
mod metrics_cmd;
mod paths_cmd;
mod pin_cmd;
mod pr_cmd;
mod profile_apply_cmd;
mod quarantine_cmd;
mod queue_cmd;
mod queue_observer_cmd;
mod release_bot_cmd;
mod reroute_cmd;
mod rescue_cmd;
mod run_cmd;
mod runner_cmd;
mod runner_kill_cmd;
mod runner_provision_cmd;
mod ship_cmd;
mod ship_state_cmd;
mod targets_cmd;
mod update_cmd;
mod wait_cmd;
mod watch_cmd;
mod watch_local_cmd;

use self::auth_cmd::auth_command;
use self::auto_merge_cmd::auto_merge;
use self::branch_cmd::branch_command;
use self::changed_surface_cmd::{ChangedSurfacePlanArgs, changed_surface_plan_command};
use self::changelog_cmd::changelog_command;
use self::ci_cmd::ci_command;
use self::cleanup_cmd::{
    CleanupCommandOptions, CleanupMode, CleanupOutput, CleanupScope, cleanup_command,
};
use self::cli::{
    Cli, Command, EvidenceCommand, MergeMethod, MergeResult, RunSubcommand, ShipStateCommand,
    TargetsCommand, WatchSubcommand,
};
use self::cloud_cmd::cloud_command;
use self::command_evidence_cmd::{run_command_evidence, show_command_evidence};
use self::config_cmd::config_command;
use self::daemon_cmd::daemon_command;
use self::dependency_cmd::dependency_command;
use self::doctor_cmd::doctor;
use self::execution_worker_cmd::execution_worker_command;
use self::governance_cmd::governance_command;
use self::init_cmd::init_command;
use self::merge_queue_control_cmd::merge_queue_control_command;
use self::metrics_cmd::metrics_command;
use self::paths_cmd::print_paths;
use self::pin_cmd::pin_command;
use self::pr_cmd::{PrCommandArgs, StewardHandoffPreference, pr_command};
use self::quarantine_cmd::quarantine_command;
use self::queue_cmd::{
    bump_command, cancel_command, evidence_command, logs_command, queue_command, status_command,
};
use self::queue_observer_cmd::{QueueObserverArgs, queue_observer_command};
use self::release_bot_cmd::release_bot_command;
use self::rescue_cmd::rescue_command;
use self::run_cmd::{
    FailFastMode, ReachabilityPolicy, RootMismatchPolicy, RunCommandArgs, TreeDriftPolicy,
    WarmPolicy, run_command,
};
use self::runner_cmd::runner_command;
use self::ship_cmd::{ShipCommandArgs, ship_command};
use self::ship_state_cmd::{
    ship_state_discard, ship_state_list, ship_state_reconcile, ship_state_show,
};
use self::targets_cmd::targets_command;
use self::update_cmd::update_command;
use self::wait_cmd::wait_command;
use self::watch_cmd::{WatchCommandContext, WatchCommandOptions, watch};
use self::watch_local_cmd::watch_local_command;

#[derive(Debug)]
pub(super) struct CliFailure {
    code: u8,
    message: String,
}

impl CliFailure {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(super) fn message(&self) -> &str {
        &self.message
    }
}

/// `shipyard ship` validated every target green but could not merge because the
/// request Shipyard sent GitHub was malformed — a Shipyard defect, not a PR
/// problem. Distinct from `1` (validation genuinely failed) and from `0` (merged,
/// or blocked by something on the PR) so automation can tell a stalled-green PR
/// from a red one.
pub(super) const SHIP_EXIT_MERGE_CLIENT_DEFECT: u8 = 8;
/// Local validation passed, but its durable scoped ship state disappeared, so
/// deterministic stewardship cannot safely own merge readiness.
pub(super) const SHIP_EXIT_VALIDATION_STATE_MISSING: u8 = 9;

pub(super) const WAIT_EXIT_TIMEOUT: u8 = 1;
pub(super) const WAIT_EXIT_RUN_TERMINAL_WRONG: u8 = 4;
pub(super) const WAIT_EXIT_INVALID: u8 = 5;
pub(super) const WAIT_EXIT_NO_FALLBACK: u8 = 6;
pub(super) const WAIT_EXIT_UNSUPPORTED: u8 = 7;

/// Run the CLI.
#[must_use]
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    run_with(cli, &mut stdout, &mut stderr)
}

fn run_with<W: Write, E: Write>(cli: Cli, stdout: &mut W, stderr: &mut E) -> ExitCode {
    match dispatch(cli, stdout, stderr) {
        Ok(code) => code,
        Err(error) => {
            if !error.message.is_empty() {
                let _ = writeln!(stderr, "{}", error.message);
            }
            ExitCode::from(error.code)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn dispatch<W: Write, E: Write>(
    cli: Cli,
    stdout: &mut W,
    _stderr: &mut E,
) -> Result<ExitCode, CliFailure> {
    let runtime_paths = RuntimePaths::current_with_overrides(
        cli.mode.into(),
        cli.global_dir.clone(),
        cli.state_dir.clone(),
    );
    let cwd = cli_cwd(&cli);

    match cli.command {
        Command::ExecutionWorker { job_id, generation } => {
            return execution_worker_command(
                &job_id,
                &generation,
                cli.mode.into(),
                &runtime_paths.global_dir,
                &runtime_paths.state_dir,
            );
        }
        Command::Paths => {
            handle_paths_command(cli.json, &runtime_paths, stdout)?;
        }
        Command::Pin { command } => {
            return pin_command(command, cli.mode.into(), &cwd, cli.json, stdout);
        }
        Command::Dependency { command } => {
            return dependency_command(command, &cwd, &runtime_paths, cli.json, stdout);
        }
        Command::Update(args) => {
            return update_command(
                &args,
                cli.mode.into(),
                &cwd,
                &runtime_paths,
                cli.json,
                stdout,
            );
        }
        Command::Config { command } => {
            return config_command(command, cli.mode.into(), &cwd, cli.json, stdout);
        }
        Command::Ci { command } => {
            return ci_command(command, cli.mode.into(), &cwd, cli.json, stdout);
        }
        Command::Metrics { command } => {
            return metrics_command(*command, &runtime_paths.state_dir, cli.json, stdout);
        }
        Command::Auth { command } => {
            return auth_command(
                command,
                cli.mode.into(),
                &cwd,
                &runtime_paths.global_dir,
                cli.json,
                stdout,
            );
        }
        command @ (Command::Init { .. }
        | Command::Changelog { .. }
        | Command::Branch { .. }
        | Command::Governance { .. }
        | Command::ReleaseBot { .. }) => {
            return handle_setup_command(
                command,
                cli.mode.into(),
                &cwd,
                &runtime_paths.state_dir,
                cli.json,
                stdout,
            );
        }
        command @ (Command::Status
        | Command::Evidence { .. }
        | Command::Logs { .. }
        | Command::Cancel { .. }
        | Command::Bump { .. }
        | Command::Queue
        | Command::Cleanup { .. }) => {
            return handle_state_command(
                command,
                cli.mode.into(),
                &cwd,
                &runtime_paths.state_dir,
                cli.json,
                stdout,
            );
        }
        Command::Targets { command } => {
            return handle_targets_command(
                command,
                cli.mode.into(),
                &cwd,
                &runtime_paths.state_dir,
                cli.json,
                stdout,
            );
        }
        Command::Quarantine { command } => {
            return quarantine_command(command, cli.mode.into(), &cwd, cli.json, stdout);
        }
        Command::Doctor {
            release_chain,
            runners,
            rate_limit,
        } => {
            handle_doctor_command(
                cli.json,
                cli.mode.into(),
                &cwd,
                &runtime_paths.state_dir,
                release_chain,
                runners,
                rate_limit,
                stdout,
            )?;
        }
        Command::Daemon { command } => {
            return handle_daemon_command(
                command,
                cli.mode.into(),
                cli.global_dir.clone(),
                cli.state_dir.clone(),
                &runtime_paths,
                cli.json,
                stdout,
            );
        }
        Command::MergeQueue { command } => {
            return merge_queue_control_command(
                command,
                &runtime_paths.state_dir,
                &runtime_paths.global_dir,
                &cwd,
                cli.mode.into(),
                cli.json,
                stdout,
            );
        }
        Command::QueueObserve {
            repo,
            base,
            follow,
            state_file,
            transition_log,
            replay,
            max_polls,
        } => {
            let config = LoadedConfig::load_from_cwd_with_global_dir(
                cli.mode.into(),
                &cwd,
                runtime_paths.global_dir.clone(),
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            return queue_observer_command(
                QueueObserverArgs {
                    repo,
                    base,
                    follow,
                    state_file,
                    transition_log,
                    replay,
                    max_polls,
                },
                &config,
                cli.mode.into(),
                &cwd,
                &runtime_paths,
                cli.json,
                stdout,
            );
        }
        Command::ChangedSurfacePlan { target, pr, repo } => {
            let config = LoadedConfig::load_from_cwd_with_global_dir(
                cli.mode.into(),
                &cwd,
                runtime_paths.global_dir.clone(),
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            return changed_surface_plan_command(
                ChangedSurfacePlanArgs { target, pr, repo },
                &config,
                &cwd,
                &runtime_paths.state_dir,
                cli.json,
                stdout,
            );
        }
        Command::Wait { command } => {
            return handle_wait_command(
                command,
                cli.mode.into(),
                &runtime_paths.daemon_socket,
                &cwd,
                cli.json,
                stdout,
            );
        }
        command => {
            return handle_operational_variant(
                command,
                cli.mode.into(),
                &cwd,
                &runtime_paths,
                cli.json,
                stdout,
            );
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn cli_cwd(cli: &Cli) -> PathBuf {
    cli.cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn handle_operational_variant<W: Write>(
    command: Command,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        command @ Command::Run { .. } => {
            handle_run_variant(command, mode, cwd, runtime_paths, json, stdout)
        }
        command @ Command::Ship { .. } => {
            handle_ship_variant(command, mode, cwd, runtime_paths, json, stdout)
        }
        command @ Command::Pr { .. } => {
            handle_pr_variant(command, mode, cwd, runtime_paths, json, stdout)
        }
        command @ Command::Cloud { .. } => {
            handle_cloud_variant(command, mode, cwd, &runtime_paths.state_dir, json, stdout)
        }
        command @ Command::Rescue(_) => handle_rescue_variant(command, mode, cwd, json, stdout),
        command @ Command::AutoMerge { .. } => {
            handle_auto_merge_variant(command, mode, runtime_paths, cwd, json, stdout)
        }
        command @ Command::Watch { .. } => {
            handle_watch_variant(command, mode, &runtime_paths.state_dir, cwd, json, stdout)
        }
        command @ Command::ShipState { .. } => {
            handle_ship_state_variant(&command, mode, cwd, &runtime_paths.state_dir, json, stdout)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Runner { command } => {
            handle_runner_command(command, mode, cwd, runtime_paths, json, stdout)
        }
        Command::Paths
        | Command::ExecutionWorker { .. }
        | Command::Pin { .. }
        | Command::Dependency { .. }
        | Command::Config { .. }
        | Command::Ci { .. }
        | Command::Metrics { .. }
        | Command::Auth { .. }
        | Command::Init { .. }
        | Command::Changelog { .. }
        | Command::Branch { .. }
        | Command::Governance { .. }
        | Command::ReleaseBot { .. }
        | Command::Status
        | Command::Evidence { .. }
        | Command::Logs { .. }
        | Command::Cancel { .. }
        | Command::Bump { .. }
        | Command::Queue
        | Command::QueueObserve { .. }
        | Command::ChangedSurfacePlan { .. }
        | Command::Cleanup { .. }
        | Command::Targets { .. }
        | Command::Quarantine { .. }
        | Command::Doctor { .. }
        | Command::Daemon { .. }
        | Command::MergeQueue { .. }
        | Command::Wait { .. }
        | Command::Update(_) => unreachable!("command handled by top-level dispatch"),
    }
}

fn handle_rescue_variant<W: Write>(
    command: Command,
    mode: RuntimeMode,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let Command::Rescue(args) = command else {
        unreachable!("rescue variant required")
    };
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    rescue_command(&args, &config, cwd, json, stdout)
}

fn handle_setup_command<W: Write>(
    command: Command,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        Command::Init { discover_only } => init_command(discover_only, mode, cwd, json, stdout),
        Command::Changelog { command } => changelog_command(command, mode, cwd, json, stdout),
        Command::Branch { command } => branch_command(command, mode, cwd, json, stdout),
        Command::Governance { command } => governance_command(command, mode, cwd, json, stdout),
        Command::ReleaseBot { command } => {
            release_bot_command(command, mode, cwd, state_dir, json, stdout)
        }
        _ => unreachable!("setup command helper only receives setup commands"),
    }
}

fn handle_targets_command<W: Write>(
    command: Option<TargetsCommand>,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    targets_command(command, mode, cwd, state_dir, json, stdout)
}

fn handle_state_command<W: Write>(
    command: Command,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        Command::Status => status_command(mode, cwd, state_dir, json, stdout),
        Command::Evidence { command, branch } => match command {
            Some(EvidenceCommand::Command { id, list }) => {
                show_command_evidence(id, list, state_dir, json, stdout)
            }
            None => evidence_command(branch, cwd, state_dir, json, stdout),
        },
        Command::Logs { job_id, target } => logs_command(&job_id, target, state_dir, stdout),
        Command::Cancel { job_id, reason } => {
            cancel_command(&job_id, reason.as_deref(), state_dir, json, stdout)
        }
        Command::Bump { job_id, priority } => {
            bump_command(&job_id, priority, state_dir, json, stdout)
        }
        Command::Queue => queue_command(state_dir, json, stdout),
        Command::Cleanup {
            pin,
            dry_run,
            apply,
            ship_state,
        } => cleanup_command(
            state_dir,
            mode,
            cwd,
            &CleanupCommandOptions {
                mode: CleanupMode::from_flags(dry_run, apply),
                scope: CleanupScope::from_flag(ship_state),
                output: CleanupOutput::from_json(json),
                pin,
            },
            stdout,
        ),
        _ => unreachable!("state command helper only receives state-like commands"),
    }
}

fn handle_paths_command<W: Write>(
    json: bool,
    runtime_paths: &RuntimePaths,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        write_pretty_json(stdout, runtime_paths)
    } else {
        print_paths(stdout, runtime_paths)
    }
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn handle_doctor_command<W: Write>(
    json: bool,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    release_chain: bool,
    runners: bool,
    rate_limit: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    doctor(
        json,
        mode,
        cwd,
        state_dir,
        release_chain,
        runners,
        rate_limit,
        stdout,
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn handle_daemon_command<W: Write>(
    command: self::cli::DaemonCommand,
    mode: RuntimeMode,
    global_dir: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    daemon_command(
        command,
        mode,
        global_dir,
        state_dir,
        runtime_paths,
        json,
        stdout,
    )
}

fn handle_wait_command<W: Write>(
    command: self::cli::WaitCommand,
    mode: RuntimeMode,
    daemon_socket: &Path,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    wait_command(command, mode, daemon_socket, cwd, json, stdout)
}

fn handle_runner_command<W: Write>(
    command: self::cli::RunnerCommand,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        self::cli::RunnerCommand::RecoveryWorker { once, drain, apply } => {
            self::merge_steward_cmd::recovery_worker::recovery_worker_command(
                self::merge_steward_cmd::recovery_worker::RecoveryWorkerCommandArgs {
                    once,
                    drain,
                    apply,
                },
                cwd,
                runtime_paths,
                json,
                stdout,
            )
        }
        command => {
            let config = crate::config::LoadedConfig::load_from_cwd(mode, cwd)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            runner_command(command, &config, mode, cwd, runtime_paths, json, stdout)
        }
    }
}

struct AutoMergeInvocation {
    pr: u64,
    merge_method: MergeMethod,
    delete_branch: bool,
    admin: bool,
    pr_snapshot_file: Option<PathBuf>,
    merge_command: Option<PathBuf>,
    merge_result: Option<MergeResult>,
}

fn handle_ship_variant<W: Write>(
    command: Command,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let Command::Ship {
        pr,
        base,
        auto_create_base,
        no_auto_create_base,
        no_warm,
        resume_from,
        allow_unreachable_targets,
        allow_fleet_epoch_drift,
        skip_targets,
        adopt_head,
        foreground,
    } = command
    else {
        unreachable!("ship variant required")
    };
    handle_ship_command(
        ShipCommandArgs {
            pr,
            base,
            auto_create_base: match (auto_create_base, no_auto_create_base) {
                (true, false) => Some(true),
                (false, true) => Some(false),
                _ => None,
            },
            no_warm,
            resume_from,
            merge_command: None,
            merge_result: None,
            gh_command: None,
            pr_snapshot_file: None,
            allow_unreachable_targets,
            allow_fleet_epoch_drift,
            skip_targets,
            adopt_head,
            steward_handoff: None,
            invocation: ship_cmd::ShipInvocation::Direct,
            foreground,
        },
        mode,
        cwd,
        runtime_paths,
        json,
        stdout,
    )
}

fn handle_pr_variant<W: Write>(
    command: Command,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let Command::Pr {
        base,
        apply_bumps,
        no_apply_bumps,
        allow_unreachable_targets,
        allow_fleet_epoch_drift,
        skip_targets,
        skip_bump,
        bump_reason,
        skip_skill_update,
        skill_reason,
        adopt_head,
        workstream_id,
        context_url,
        no_steward_handoff,
    } = command
    else {
        unreachable!("pr variant required")
    };
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    pr_command(
        PrCommandArgs {
            base,
            apply_bumps: apply_bumps && !no_apply_bumps,
            allow_unreachable_targets,
            allow_fleet_epoch_drift,
            skip_targets,
            skip_bump,
            bump_reason,
            skip_skill_update,
            skill_reason,
            adopt_head,
            workstream_id,
            context_url,
            steward_handoff_preference: if no_steward_handoff {
                StewardHandoffPreference::Disabled
            } else {
                StewardHandoffPreference::ProjectDefault
            },
            python_command: None,
        },
        &config,
        cwd,
        runtime_paths,
        json,
        stdout,
    )
}

fn handle_run_variant<W: Write>(
    command: Command,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let Command::Run {
        command: subcommand,
        targets,
        smoke,
        fail_fast,
        resume_from,
        allow_root_mismatch,
        allow_unreachable_targets,
        allow_fleet_epoch_drift,
        skip_targets,
        no_warm,
        allow_tree_drift,
        foreground,
    } = command
    else {
        unreachable!("run variant required")
    };
    if let Some(RunSubcommand::Command(args)) = subcommand {
        let config = LoadedConfig::load_from_cwd(mode, cwd)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return run_command_evidence(&args, &config, cwd, runtime_paths, json, stdout);
    }
    handle_run_command(
        RunCommandArgs {
            targets,
            mode: if smoke {
                crate::job::ValidationMode::Smoke
            } else {
                crate::job::ValidationMode::Full
            },
            fail_fast: FailFastMode::from_flag(fail_fast),
            resume_from,
            root_mismatch: RootMismatchPolicy::from_flag(allow_root_mismatch),
            reachability: ReachabilityPolicy::from_flag(allow_unreachable_targets),
            allow_fleet_epoch_drift,
            skip_targets,
            warm: WarmPolicy::from_no_warm_flag(no_warm),
            tree_drift: TreeDriftPolicy::from_flag(allow_tree_drift),
            foreground,
        },
        mode,
        cwd,
        runtime_paths,
        json,
        stdout,
    )
}

fn handle_cloud_variant<W: Write>(
    command: Command,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let Command::Cloud { command } = command else {
        unreachable!("cloud variant required")
    };
    handle_cloud_command(command, mode, cwd, state_dir, json, stdout)
}

fn handle_auto_merge_variant<W: Write>(
    command: Command,
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let Command::AutoMerge {
        pr,
        merge_method,
        delete_branch,
        no_delete_branch,
        admin,
        pr_snapshot_file,
        merge_command,
        merge_result,
    } = command
    else {
        unreachable!("auto-merge variant required")
    };
    handle_auto_merge(
        AutoMergeInvocation {
            pr,
            merge_method,
            delete_branch: delete_branch && !no_delete_branch,
            admin,
            pr_snapshot_file,
            merge_command,
            merge_result,
        },
        mode,
        &runtime_paths.state_dir,
        &runtime_paths.global_dir,
        cwd,
        json,
        stdout,
    )
}

fn handle_watch_variant<W: Write>(
    command: Command,
    mode: RuntimeMode,
    state_dir: &Path,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let Command::Watch {
        command,
        pr,
        follow,
        no_follow,
        interval,
    } = command
    else {
        unreachable!("watch variant required")
    };
    if let Some(WatchSubcommand::Local(args)) = command {
        let config = LoadedConfig::load_from_cwd(mode, cwd)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return watch_local_command(&args, &config, cwd, state_dir, json, stdout);
    }
    handle_watch_command(
        WatchInvocation {
            pr,
            follow,
            no_follow,
            interval,
        },
        state_dir,
        cwd,
        json,
        stdout,
    )
}

fn handle_ship_state_variant<W: Write>(
    command: &Command,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let Command::ShipState { command } = *command else {
        unreachable!("ship-state variant required")
    };
    let store = ShipStateStore::new(state_dir.join("ship"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    handle_ship_state_command(command, &store, mode, cwd, state_dir, json, stdout)
}

#[derive(Clone, Copy)]
struct WatchInvocation {
    pr: Option<u64>,
    follow: bool,
    no_follow: bool,
    interval: f64,
}

fn handle_ship_command<W: Write>(
    args: ShipCommandArgs,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    ship_command(args, &config, cwd, runtime_paths, json, stdout)
}

fn handle_run_command<W: Write>(
    args: RunCommandArgs,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    run_command(args, &config, cwd, runtime_paths, json, stdout)
}

fn handle_cloud_command<W: Write>(
    command: self::cli::CloudCommand,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let store = ShipStateStore::new(state_dir.join("ship"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let cloud_records = CloudRecordStore::new(state_dir.join("cloud"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    cloud_command(command, &store, &cloud_records, &config, cwd, json, stdout)
}

fn handle_auto_merge<W: Write>(
    invocation: AutoMergeInvocation,
    mode: RuntimeMode,
    state_dir: &Path,
    global_dir: &Path,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let store = ShipStateStore::new(state_dir.join("ship"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    auto_merge(
        &store,
        cwd,
        mode,
        global_dir,
        invocation.pr,
        invocation.merge_method,
        invocation.delete_branch,
        invocation.admin,
        invocation.pr_snapshot_file,
        invocation.merge_command,
        invocation.merge_result,
        json,
        stdout,
    )
}

fn handle_watch_command<W: Write>(
    invocation: WatchInvocation,
    state_dir: &Path,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let store = ShipStateStore::new(state_dir.join("ship"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let evidence = EvidenceStore::new(state_dir.join("evidence"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let follow = if invocation.no_follow {
        false
    } else {
        invocation.follow
    };
    watch(
        WatchCommandContext {
            store: &store,
            evidence_store: &evidence,
            cwd,
        },
        WatchCommandOptions {
            pr: invocation.pr,
            follow,
            interval: invocation.interval,
            json,
        },
        stdout,
    )
}

fn handle_ship_state_command<W: Write>(
    command: ShipStateCommand,
    store: &ShipStateStore,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    match command {
        ShipStateCommand::List => {
            let config = LoadedConfig::load_from_cwd(mode, cwd)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            let stale_after = crate::ship_liveness::orphan_stale_after(&config);
            crate::ship_liveness::with_liveness_context(state_dir, stale_after, |liveness| {
                ship_state_list(store, liveness, json, stdout)
            })
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        ShipStateCommand::Show { pr } => {
            let repository = self::branch_cmd::detect_repo_from_remote(cwd, None);
            ship_state_show(store, repository.as_deref(), pr, json, stdout)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        ShipStateCommand::Discard { pr } => {
            let repository = self::branch_cmd::detect_repo_from_remote(cwd, None);
            ship_state_discard(store, repository.as_deref(), pr, json, stdout)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        ShipStateCommand::Reconcile { pr, all } => {
            if all && pr.is_some() {
                return Err(CliFailure::new(2, "Pass either <pr> or --all, not both."));
            }
            if !all && pr.is_none() {
                return Err(CliFailure::new(
                    2,
                    "Usage: shipyard ship-state reconcile <pr> | --all",
                ));
            }
            ship_state_reconcile(store, mode, cwd, pr, all, json, stdout)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::process::ExitCode;
    use std::process::{Command, Stdio};
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use chrono::{TimeZone, Utc};
    use clap::Parser;
    use serde_json::Value;

    use super::{
        Cli, WAIT_EXIT_NO_FALLBACK, WAIT_EXIT_RUN_TERMINAL_WRONG, WAIT_EXIT_UNSUPPORTED, run_with,
        wait_cmd::parse_github_repo_slug,
    };
    use crate::cloud_records::{CloudRecordStore, CloudRunRecord};
    #[cfg(unix)]
    use crate::daemon_ipc::read_daemon_status;
    #[cfg(unix)]
    use crate::daemon_runtime::{DaemonRunConfig, run_blocking, stop_running};
    use crate::evidence::{EvidenceRecord, EvidenceStore};
    #[cfg(unix)]
    use crate::identity::RuntimeMode;
    use crate::job::{Job, Priority, TargetResult, TargetStatus, ValidationMode};
    use crate::queue::Queue;
    use crate::ship_state::{DispatchedRun, ShipState, ShipStateStore};
    use crate::warm_pool::{PoolEntry, WarmPool};

    fn git(args: &[&str], cwd: &std::path::Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git command should run");
        assert!(status.success(), "git command failed: {args:?}");
    }

    #[cfg(unix)]
    fn make_executable(path: &std::path::Path) {
        let mut permissions = std::fs::metadata(path)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod script");
    }

    fn seed_git_repo(repo: &std::path::Path, branch: &str) {
        std::fs::create_dir_all(repo).expect("repo dir");
        git(&["init", "--quiet", "--initial-branch=main"], repo);
        std::fs::write(repo.join("README.md"), "seed\n").expect("readme");
        git(&["add", "."], repo);
        git(&["commit", "-q", "-m", "seed"], repo);
        git(&["checkout", "-q", "-b", branch], repo);
    }

    #[cfg(unix)]
    fn spawn_test_daemon(
        state_dir: &std::path::Path,
        repos: Vec<String>,
    ) -> std::thread::JoinHandle<()> {
        let state_dir = state_dir.to_path_buf();
        thread::spawn(move || {
            run_blocking(DaemonRunConfig {
                mode: RuntimeMode::Isolated,
                global_dir: state_dir.clone(),
                state_dir,
                repos,
            })
            .expect("daemon runtime");
        })
    }

    #[cfg(unix)]
    fn wait_for_daemon(state_dir: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while read_daemon_status(state_dir).is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            read_daemon_status(state_dir).is_some(),
            "daemon did not come up"
        );
    }

    #[cfg(unix)]
    fn seed_registered_repos(state_dir: &std::path::Path, repos: &[&str]) {
        let daemon_dir = state_dir.join("daemon");
        std::fs::create_dir_all(&daemon_dir).expect("daemon dir");
        let payload = repos
            .iter()
            .enumerate()
            .map(|(index, repo)| {
                serde_json::json!({
                    "repo": repo,
                    "hook_id": u64::try_from(index + 1).expect("hook id"),
                })
            })
            .collect::<Vec<_>>();
        std::fs::write(
            daemon_dir.join("registrations.json"),
            serde_json::to_string_pretty(&payload).expect("registrations json"),
        )
        .expect("write registrations");
    }

    fn auto_merge_state(pr: u64, evidence: &[(&str, &str)]) -> ShipState {
        let repository = crate::app::branch_cmd::detect_repo_from_remote(
            &std::env::current_dir().expect("current directory"),
            None,
        )
        .unwrap_or_else(|| "owner/repo".to_owned());
        let mut state = ShipState::new(pr, repository, "feature/x", "main", "a".repeat(40), "p1");
        state.evidence_snapshot = evidence
            .iter()
            .map(|(target, status)| ((*target).to_owned(), (*status).to_owned()))
            .collect();
        state
    }

    fn run_for(target: &str, provider: &str, run_id: &str) -> DispatchedRun {
        let now = Utc::now();
        DispatchedRun {
            target: target.to_owned(),
            provider: provider.to_owned(),
            run_id: run_id.to_owned(),
            status: "in_progress".to_owned(),
            started_at: now,
            updated_at: now,
            attempt: 1,
            last_heartbeat_at: None,
            phase: None,
            required: true,
        }
    }

    fn write_test_workflow(root: &std::path::Path) {
        let workflows = root.join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).expect("workflow dir");
        std::fs::write(
            workflows.join("ci.yml"),
            "name: CI\non:\n  workflow_dispatch:\n    inputs:\n      runner_provider:\n        required: false\n",
        )
        .expect("workflow");
    }

    fn seed_consumer_pin_repo(root: &std::path::Path, version: &str) {
        std::fs::create_dir_all(root.join("tools")).expect("tools");
        git(&["init", "--quiet", "--initial-branch=main"], root);
        std::fs::write(
            root.join("tools").join("shipyard.toml"),
            format!("[shipyard]\nversion = \"{version}\"\nrepo = \"danielraffel/Shipyard\"\n"),
        )
        .expect("pin");
        std::fs::write(
            root.join("tools").join("install-shipyard.sh"),
            "#!/bin/sh\nexit 0\n",
        )
        .expect("installer");
        git(&["add", "."], root);
        git(&["commit", "-q", "-m", "seed"], root);
    }

    fn write_local_watch_config(root: &std::path::Path) {
        let project_dir = root.join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            format!(
                "[targets.local]\nbackend = \"local\"\nplatform = \"linux\"\ncwd = {:?}\n",
                root.display().to_string()
            ),
        )
        .expect("watch config");
    }

    #[test]
    fn ship_state_list_json_matches_command_envelope_shape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = ShipState::new(
            224,
            "danielraffel/pulp",
            "feature/test",
            "main",
            "abc123456789def",
            "policy0001",
        );
        state.pr_title = "Fix ARA controller".to_owned();
        state.dispatched_runs.push(DispatchedRun {
            target: "cloud".to_owned(),
            provider: "namespace".to_owned(),
            run_id: "24446948064".to_owned(),
            status: "in_progress".to_owned(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
            attempt: 1,
            last_heartbeat_at: None,
            phase: Some("build".to_owned()),
            required: true,
        });
        store.save(&state).expect("save");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "ship-state",
            "list",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());

        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["command"], "ship-state:list");
        assert_eq!(value["states"][0]["pr"], 224);
        assert_eq!(value["states"][0]["pr_title"], "Fix ARA controller");
        assert_eq!(
            value["states"][0]["dispatched_runs"][0]["run_id"],
            "24446948064"
        );
    }

    #[test]
    fn watch_local_streams_milestones_and_process_exit_terminal_event() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_local_watch_config(temp.path());
        let state_dir = temp.path().join("state");
        let log_path = temp.path().join("watch.log");
        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "isolated",
            "--state-dir",
            state_dir.to_str().expect("state path"),
            "--cwd",
            temp.path().to_str().expect("cwd path"),
            "watch",
            "local",
            "--target",
            "local",
            "--command",
            "printf '[1/3] compile\\nok\\n'",
            "--milestone-regex",
            r"\[[0-9]+/[0-9]+\]",
            "--log-path",
            log_path.to_str().expect("log path"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let text = String::from_utf8(stdout).expect("utf8");
        assert!(text.contains("[1/3] compile"));
        assert!(text.contains("shipyard milestone"));
        assert!(text.contains("shipyard terminal [process_exit]: returncode=0"));
        assert!(
            std::fs::read_to_string(log_path)
                .expect("log")
                .contains("[1/3] compile")
        );
    }

    #[test]
    fn watch_local_terminal_regex_stops_with_failure_status() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_local_watch_config(temp.path());
        let state_dir = temp.path().join("state");
        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "isolated",
            "--state-dir",
            state_dir.to_str().expect("state path"),
            "--cwd",
            temp.path().to_str().expect("cwd path"),
            "watch",
            "local",
            "--target",
            "local",
            "--command",
            "printf 'AUDIT FAIL bad\\n'",
            "--terminal-regex",
            "AUDIT FAIL",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(1));
        assert!(stderr.is_empty());
        let text = String::from_utf8(stdout).expect("utf8");
        assert!(text.contains("AUDIT FAIL bad"));
        assert!(text.contains("shipyard terminal [AUDIT FAIL]: AUDIT FAIL bad"));
        assert!(!text.contains("process_exit"));
    }

    #[test]
    fn ship_state_show_json_returns_nonzero_for_missing_pr() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "isolated",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "ship-state",
            "show",
            "999",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::FAILURE);
        assert!(stdout.is_empty());
        let stderr = String::from_utf8(stderr).expect("utf8");
        assert!(stderr.contains("No ship state for PR #999"));
    }

    #[test]
    fn isolated_ship_state_show_uses_checkout_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_git_repo(&repo, "feature/test");
        git(
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/Generous-Corp/pulp.git",
            ],
            &repo,
        );
        let state_dir = temp.path().join("state");
        let store = ShipStateStore::new(state_dir.join("ship")).expect("store");
        store
            .save(&ShipState::new(
                42,
                "Generous-Corp/pulp",
                "feature/test",
                "main",
                "pulp-head",
                "policy",
            ))
            .expect("Pulp state");
        store
            .save(&ShipState::new(
                42,
                "Generous-Corp/forge",
                "feature/modular",
                "main",
                "forge-head",
                "policy",
            ))
            .expect("Forge state");
        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "isolated",
            "--json",
            "--cwd",
            repo.to_str().expect("repo path"),
            "--state-dir",
            state_dir.to_str().expect("state path"),
            "ship-state",
            "show",
            "42",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["repo"], "Generous-Corp/pulp");
        assert_eq!(value["head_sha"], "pulp-head");
    }

    #[test]
    fn doctor_json_uses_expected_envelope_shape() {
        let cli = Cli::parse_from(["shipyard", "--json", "doctor"]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());

        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["command"], "doctor");
        assert!(value["ready"].is_boolean());
        assert!(value["checks"]["Core"].is_object());
        assert!(value["checks"]["Cloud providers"].is_object());
    }

    #[test]
    fn config_show_defaults_to_effective_config_json() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "isolated",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "config",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "config.show");
        assert!(value["config"].is_object());
    }

    #[test]
    fn init_json_writes_project_config_and_flattens_envelope() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .expect("cargo");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "init",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "init");
        assert_eq!(
            value["project"]["name"],
            temp.path()
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap()
        );
        assert_eq!(value["project"]["type"], "rust");
        assert_eq!(value["validation"]["default"]["build"], "cargo build");
        assert_eq!(value["targets"]["mac"]["backend"], "local");
        assert_eq!(value["targets"]["ubuntu"]["backend"], "cloud");
        assert!(temp.path().join(".shipyard").join("config.toml").exists());
        let gitignore = std::fs::read_to_string(temp.path().join(".gitignore")).expect("gitignore");
        assert!(gitignore.contains(".shipyard.local/"));
    }

    #[test]
    fn init_discover_only_preserves_python_write_behavior() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "init",
            "--discover-only",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let output = String::from_utf8(stdout).expect("utf8");
        assert!(output.contains("Detected config (not written):"));
        assert!(output.contains("\"project\""));
        assert!(temp.path().join(".shipyard").join("config.toml").exists());
    }

    #[test]
    fn init_shipyard_mode_human_uses_production_overlay_name() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "init",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        assert_eq!(
            String::from_utf8(stdout).expect("utf8"),
            "Shipyard configured. Try: shipyard run\n"
        );
        let gitignore = std::fs::read_to_string(temp.path().join(".gitignore")).expect("gitignore");
        assert_eq!(gitignore, ".shipyard.local/\n");
    }

    #[test]
    fn config_profiles_json_marks_active_profile() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            r#"
            [project]
            profile = "fast"

            [profiles.fast]
            targets = ["mac", "linux"]

            [profiles.full]
            targets = ["mac", "linux", "windows"]
            "#,
        )
        .expect("config");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "config",
            "profiles",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "config.profiles");
        assert_eq!(value["active"], "fast");
        let profiles = value["profiles"].as_array().expect("profiles");
        let fast = profiles
            .iter()
            .find(|profile| profile["name"] == "fast")
            .expect("fast profile");
        assert_eq!(fast["active"], true);
        assert_eq!(fast["targets"], serde_json::json!(["mac", "linux"]));
    }

    #[test]
    fn config_use_rewrites_project_profile() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        let config_path = project_dir.join("config.toml");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            &config_path,
            r#"
            [project]
            profile = "fast"

            [profiles.fast]
            targets = ["mac"]

            [profiles.full]
            targets = ["mac", "linux"]
            "#,
        )
        .expect("config");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "config",
            "use",
            "full",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "config.use");
        assert_eq!(value["profile"], "full");
        let updated = std::fs::read_to_string(config_path).expect("read config");
        assert!(updated.contains("profile = \"full\""));
    }

    #[test]
    fn queue_json_reports_empty_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "isolated",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "queue",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "queue");
        assert!(value["active"].is_null());
        assert_eq!(
            value["active_runs"].as_array().expect("active_runs").len(),
            0
        );
        assert_eq!(value["pending"].as_array().expect("pending").len(), 0);
        assert_eq!(value["recent"].as_array().expect("recent").len(), 0);
    }

    #[test]
    fn status_json_reports_empty_queue_and_targets() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "status",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "status");
        assert_eq!(value["queue"]["pending"], 0);
        assert_eq!(value["queue"]["running"], 0);
        assert_eq!(
            value["active_runs"].as_array().expect("active_runs").len(),
            0
        );
        assert!(value["targets"].as_object().expect("targets").is_empty());
    }

    #[test]
    fn queue_json_reports_multiple_active_runs_additively() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let first = queue
            .enqueue(Job::create(
                "abc123456789",
                "feature/first",
                vec!["linux".to_owned()],
                ValidationMode::Full,
                Priority::Normal,
            ))
            .expect("enqueue first")
            .start()
            .expect("start first");
        queue.update(&first).expect("update first");
        let second = queue
            .enqueue(Job::create(
                "def987654321",
                "feature/second",
                vec!["mac".to_owned()],
                ValidationMode::Full,
                Priority::Normal,
            ))
            .expect("enqueue second")
            .start()
            .expect("start second");
        queue.update(&second).expect("update second");

        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "isolated",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "queue",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "queue");
        assert_eq!(value["active"]["id"], first.id);
        assert_eq!(
            value["active_runs"].as_array().expect("active_runs").len(),
            2
        );
        assert_eq!(value["active_runs"][0]["id"], first.id);
        assert_eq!(value["active_runs"][1]["id"], second.id);
    }

    #[test]
    fn status_json_reports_multiple_active_runs_additively() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        for (sha, branch, target) in [
            ("abc123456789", "feature/first", "linux"),
            ("def987654321", "feature/second", "mac"),
        ] {
            let running = queue
                .enqueue(Job::create(
                    sha,
                    branch,
                    vec![target.to_owned()],
                    ValidationMode::Full,
                    Priority::Normal,
                ))
                .expect("enqueue")
                .start()
                .expect("start");
            queue.update(&running).expect("update");
        }

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "status",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "status");
        assert_eq!(value["queue"]["running"], 2);
        assert_eq!(value["active_run"]["branch"], "feature/first");
        assert_eq!(
            value["active_runs"].as_array().expect("active_runs").len(),
            2
        );
    }

    #[test]
    fn evidence_json_reports_empty_branch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "evidence",
            "main",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "evidence");
        assert_eq!(value["branch"], "main");
        assert!(value["evidence"].as_object().expect("evidence").is_empty());
    }

    #[test]
    fn evidence_json_reports_stored_branch_records() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = EvidenceStore::new(temp.path().join("evidence")).expect("evidence store");
        store
            .record(&EvidenceRecord {
                sha: "abc123456789".to_owned(),
                branch: "feature/evidence".to_owned(),
                workload_scope: None,
                target_name: "linux".to_owned(),
                validation_build_type: None,
                platform: "linux".to_owned(),
                status: "pass".to_owned(),
                backend: "local".to_owned(),
                source_head_sha: None,
                source_tree_sha: None,
                source_checkout_clean: None,
                full_execution: None,
                completed_at: Utc::now(),
                duration_secs: Some(1.5),
                host: None,
                primary_backend: None,
                failover_reason: None,
                provider: None,
                runner_profile: None,
                failure_class: None,
                reused_from: None,
                contract_digest: None,
                stages_signature: None,
            })
            .expect("record");
        let mut scoped = store
            .get_target("feature/evidence", "linux")
            .expect("legacy evidence");
        scoped.target_name = "macos".to_owned();
        scoped.platform = "macos".to_owned();
        store
            .record_scoped(&crate::evidence::run_evidence_scope(temp.path()), &scoped)
            .expect("scoped record");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "evidence",
            "feature/evidence",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "evidence");
        assert_eq!(value["branch"], "feature/evidence");
        assert_eq!(value["evidence"]["linux"]["sha"], "abc123456789");
        assert_eq!(value["evidence"]["linux"]["status"], "pass");
        assert_eq!(value["evidence"]["macos"]["status"], "pass");
    }

    #[test]
    fn bump_json_updates_pending_job_priority() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let job = queue
            .enqueue(Job::create(
                "abc123456789",
                "feature/config",
                vec!["linux".to_owned()],
                ValidationMode::Full,
                Priority::Normal,
            ))
            .expect("enqueue");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "bump",
            &job.id,
            "high",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "bump");
        assert_eq!(value["job"]["priority"], "high");
    }

    #[test]
    fn cancel_json_marks_pending_job_cancelled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let job = queue
            .enqueue(Job::create(
                "abc123456789",
                "feature/cancel",
                vec!["linux".to_owned()],
                ValidationMode::Full,
                Priority::Normal,
            ))
            .expect("enqueue");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "cancel",
            &job.id,
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "cancel");
        assert_eq!(value["job"]["status"], "cancelled");
        assert!(
            value["job"]["cancellation_reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("shipyard cancel"))
        );
    }

    #[test]
    fn cancel_json_requests_running_job_cancellation_without_freeing_capacity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let job = queue
            .enqueue(Job::create(
                "abc123456789",
                "feature/cancel-running",
                vec!["linux".to_owned()],
                ValidationMode::Full,
                Priority::Normal,
            ))
            .expect("enqueue");
        // Transition to Running first, to prove `cancel` reaches running jobs and
        // not just pending ones — the manual escape hatch for a wedged ship.
        let started = job.start().expect("start");
        queue.update(&started).expect("update running");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "cancel",
            &job.id,
            "--reason",
            "controller epoch 7 replaced exact head",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "cancel");
        assert_eq!(value["job"]["status"], "running");
        assert_eq!(
            value["job"]["cancellation_reason"],
            "controller epoch 7 replaced exact head"
        );
        assert_eq!(
            queue.get(&job.id).expect("get").expect("job").status,
            crate::job::JobStatus::Running
        );
        assert!(
            queue
                .get(&job.id)
                .expect("get")
                .expect("job")
                .cancel_requested_at
                .is_some()
        );
    }

    #[test]
    fn logs_prints_selected_target_log() {
        let temp = tempfile::tempdir().expect("tempdir");
        let log_path = temp.path().join("linux.log");
        std::fs::write(&log_path, "target log\n").expect("write log");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let job = queue
            .enqueue(Job::create(
                "abc123456789",
                "feature/logs",
                vec!["linux".to_owned()],
                ValidationMode::Full,
                Priority::Normal,
            ))
            .expect("enqueue");
        let mut result = TargetResult::new("linux", "linux", TargetStatus::Pass, "local");
        result.log_path = Some(log_path.to_string_lossy().into_owned());
        let running = job.start().expect("start").with_result(result);
        queue.update(&running).expect("update");
        let cli = Cli::parse_from([
            "shipyard",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "logs",
            &job.id,
            "--target",
            "linux",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        assert_eq!(String::from_utf8(stdout).expect("utf8"), "target log\n");
    }

    #[test]
    fn queue_human_reports_pending_and_recent_jobs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let pending = queue
            .enqueue(Job::create(
                "abc123456789",
                "feature/pending",
                vec!["linux".to_owned()],
                ValidationMode::Full,
                Priority::High,
            ))
            .expect("enqueue pending");
        let completed_seed = queue
            .enqueue(Job::create(
                "def987654321",
                "feature/completed",
                vec!["mac".to_owned()],
                ValidationMode::Full,
                Priority::Normal,
            ))
            .expect("enqueue completed");
        let completed = completed_seed
            .start()
            .expect("start")
            .with_result(TargetResult::new(
                "mac",
                "macos",
                TargetStatus::Pass,
                "local",
            ))
            .complete()
            .expect("complete");
        queue.update(&completed).expect("update completed");
        let cli = Cli::parse_from([
            "shipyard",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "queue",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let output = String::from_utf8(stdout).expect("utf8");
        assert!(output.contains("Queue"));
        assert!(output.contains("Pending (1)"));
        assert!(output.contains(&pending.id));
        assert!(output.contains("Recent (1)"));
        assert!(output.contains(&completed.id));
    }

    #[test]
    fn targets_default_json_lists_empty_targets() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "isolated",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "targets",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "targets.list");
        assert_eq!(value["targets"].as_array().expect("targets").len(), 0);
    }

    #[test]
    fn recovery_worker_dispatch_ignores_malformed_checkout_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(project_dir.join("config.toml"), "not = [valid toml")
            .expect("malformed checkout config");
        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "isolated",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "runner",
            "recovery-worker",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(1));
        let error = String::from_utf8(stderr).expect("utf8");
        assert!(error.contains("requires canonical machine-global paths"));
        assert!(!error.contains("TOML"));
        assert!(stdout.is_empty());
    }

    #[test]
    fn targets_list_json_reports_local_reachable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[targets.linux]\nbackend = \"local\"\nplatform = \"linux-x64\"\n",
        )
        .expect("write config");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "targets",
            "list",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "targets.list");
        assert_eq!(value["targets"][0]["name"], "linux");
        assert_eq!(value["targets"][0]["backend"], "local");
        assert_eq!(value["targets"][0]["platform"], "linux-x64");
        assert_eq!(value["targets"][0]["reachable"], true);
        assert_eq!(value["targets"][0]["active_backend"], "local");
    }

    #[test]
    fn targets_test_json_reports_local_active_backend() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[targets.mac]\nbackend = \"local\"\nplatform = \"macos-arm64\"\n",
        )
        .expect("write config");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "targets",
            "test",
            "mac",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "targets.test");
        assert_eq!(value["name"], "mac");
        assert_eq!(value["reachable"], true);
        assert_eq!(value["active_backend"], "local");
    }

    #[test]
    fn targets_add_json_appends_project_config_section() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        let config_path = project_dir.join("config.toml");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(&config_path, "[project]\nname = \"demo\"\n").expect("write config");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "targets",
            "add",
            "linux",
            "--backend",
            "local",
            "--platform",
            "linux-x64",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "targets.add");
        assert_eq!(value["name"], "linux");
        assert_eq!(value["config"]["backend"], "local");
        let config_text = std::fs::read_to_string(config_path).expect("read config");
        assert!(config_text.contains("[targets.linux]"));
        assert!(config_text.contains("platform = \"linux-x64\""));
    }

    #[test]
    fn targets_remove_json_removes_only_named_section() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        let config_path = project_dir.join("config.toml");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            &config_path,
            "[targets.linux]\nbackend = \"local\"\n\n[targets.mac]\nbackend = \"local\"\n",
        )
        .expect("write config");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "targets",
            "remove",
            "linux",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "targets.remove");
        assert_eq!(value["name"], "linux");
        let config_text = std::fs::read_to_string(config_path).expect("read config");
        assert!(!config_text.contains("[targets.linux]"));
        assert!(config_text.contains("[targets.mac]"));
    }

    #[test]
    fn targets_warm_status_json_reports_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = WarmPool::new(temp.path().join("warm_pool.json"));
        pool.save_entries(&[PoolEntry::new(
            "linux",
            "runner",
            "ssh",
            "/tmp/repo",
            "abc123456789",
            2_000_000_000.0,
            1_999_999_000.0,
        )])
        .expect("save pool");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "targets",
            "warm",
            "status",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "targets.warm.status");
        assert_eq!(value["entries"][0]["target"], "linux");
        assert_eq!(value["entries"][0]["host"], "runner");
        assert_eq!(value["entries"][0]["sha"], "abc123456789");
    }

    #[test]
    fn targets_warm_drain_json_removes_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = WarmPool::new(temp.path().join("warm_pool.json"));
        pool.save_entries(&[PoolEntry::new(
            "linux",
            "runner",
            "ssh",
            "/tmp/repo",
            "abc123456789",
            2_000_000_000.0,
            1_999_999_000.0,
        )])
        .expect("save pool");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "targets",
            "warm",
            "drain",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "targets.warm.drain");
        assert_eq!(value["drained"], 1);
        assert!(pool.all_entries().is_empty());
    }

    #[test]
    fn cleanup_json_reports_orphaned_state_artifacts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let orphan_log = temp.path().join("logs").join("orphan");
        let active_log = temp.path().join("logs").join("active");
        std::fs::create_dir_all(&orphan_log).expect("orphan log dir");
        std::fs::create_dir_all(&active_log).expect("active log dir");
        std::fs::write(orphan_log.join("out.log"), "orphan\n").expect("orphan log");
        std::fs::write(
            orphan_log.join(".retention.json"),
            r#"{"schema_version":1,"job_id":"orphan","terminal_at":"2020-01-01T00:00:00Z","failed":false,"reason":"passed"}"#,
        )
        .expect("retention manifest");
        std::fs::write(active_log.join("out.log"), "active\n").expect("active log");
        std::fs::write(
            temp.path().join("queue.json"),
            r#"{"jobs":[{"id":"active","status":"running"}]}"#,
        )
        .expect("queue file");
        std::fs::create_dir_all(temp.path().join("bundles")).expect("bundles dir");
        std::fs::write(temp.path().join("bundles").join("old.bundle"), "bundle").expect("bundle");
        std::fs::create_dir_all(temp.path().join("evidence")).expect("evidence dir");
        std::fs::write(temp.path().join("evidence").join("empty.json"), "{}").expect("evidence");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "cleanup",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "cleanup");
        assert_eq!(value["dry_run"], true);
        assert_eq!(value["count"], 3);
        assert!(
            value["items"]
                .as_array()
                .expect("items")
                .iter()
                .any(|item| {
                    item["kind"] == "log"
                        && item["path"]
                            .as_str()
                            .expect("path")
                            .replace('\\', "/")
                            .contains("logs/orphan")
                })
        );
        assert!(orphan_log.exists());
        assert!(active_log.exists());
    }

    #[test]
    fn cleanup_apply_deletes_orphaned_artifacts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let orphan_log = temp.path().join("logs").join("orphan");
        let bundle = temp.path().join("bundles").join("old.bundle");
        let evidence = temp.path().join("evidence").join("bad.json");
        std::fs::create_dir_all(&orphan_log).expect("orphan log dir");
        std::fs::write(orphan_log.join("out.log"), "orphan\n").expect("orphan log");
        std::fs::write(
            orphan_log.join(".retention.json"),
            r#"{"schema_version":1,"job_id":"orphan","terminal_at":"2020-01-01T00:00:00Z","failed":false,"reason":"passed"}"#,
        )
        .expect("retention manifest");
        std::fs::create_dir_all(temp.path().join("bundles")).expect("bundles dir");
        std::fs::write(&bundle, "bundle").expect("bundle");
        std::fs::create_dir_all(temp.path().join("evidence")).expect("evidence dir");
        std::fs::write(&evidence, "{not json").expect("evidence");
        std::fs::write(temp.path().join("queue.json"), r#"{"jobs":[]}"#).expect("queue");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "cleanup",
            "--apply",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["dry_run"], false);
        assert_eq!(value["count"], 3);
        assert!(!orphan_log.exists());
        assert!(!bundle.exists());
        assert!(!evidence.exists());
    }

    #[test]
    fn cleanup_ship_state_dry_run_reports_preview_note() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "cleanup",
            "--ship-state",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "cleanup");
        assert_eq!(value["ship_state"]["total"], 0);
        assert_eq!(
            value["ship_state"]["note"],
            "Active-file pruning is only computed during --apply."
        );
    }

    #[test]
    fn quarantine_default_json_lists_empty_entries_without_project() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "quarantine",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "quarantine.list");
        assert_eq!(value["entries"].as_array().expect("entries").len(), 0);
    }

    #[test]
    fn quarantine_list_json_omits_empty_optional_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[project]\nname = \"demo\"\n",
        )
        .expect("config");
        std::fs::write(
            project_dir.join("quarantine.toml"),
            "[[quarantine]]\ntarget = \"windows\"\nreason = \"flaky\"\nadded_at = \"2026-04-18\"\n\n[[quarantine]]\ntarget = \"linux\"\n",
        )
        .expect("quarantine");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "quarantine",
            "list",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "quarantine.list");
        let entries = value["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["reason"], "flaky");
        assert_eq!(entries[0]["added_at"], "2026-04-18");
        assert_eq!(entries[1]["target"], "linux");
        assert_eq!(entries[1].get("reason"), None);
        assert_eq!(entries[1].get("added_at"), None);
    }

    #[test]
    fn quarantine_add_json_writes_project_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[project]\nname = \"demo\"\n",
        )
        .expect("config");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "quarantine",
            "add",
            "windows",
            "--reason",
            "flaky",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "quarantine.add");
        assert_eq!(value["target"], "windows");
        assert_eq!(value["added"], true);
        let quarantine =
            std::fs::read_to_string(project_dir.join("quarantine.toml")).expect("quarantine");
        assert!(quarantine.contains("[[quarantine]]"));
        assert!(quarantine.contains("target = \"windows\""));
        assert!(quarantine.contains("reason = \"flaky\""));
    }

    #[test]
    fn quarantine_add_without_project_reports_init_hint() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "quarantine",
            "add",
            "windows",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(1));
        assert!(stdout.is_empty());
        let stderr = String::from_utf8(stderr).expect("stderr");
        assert!(stderr.contains("No .shipyard/ directory found"));
        assert!(stderr.contains("shipyard init"));
    }

    #[test]
    fn quarantine_remove_json_updates_project_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_dir = temp.path().join(".shipyard");
        let quarantine_path = project_dir.join("quarantine.toml");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[project]\nname = \"demo\"\n",
        )
        .expect("config");
        std::fs::write(
            &quarantine_path,
            "[[quarantine]]\ntarget = \"windows\"\nreason = \"flaky\"\nadded_at = \"2026-04-18\"\n\n[[quarantine]]\ntarget = \"linux\"\n",
        )
        .expect("quarantine");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "quarantine",
            "remove",
            "windows",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "quarantine.remove");
        assert_eq!(value["removed"], true);
        let quarantine = std::fs::read_to_string(quarantine_path).expect("quarantine");
        assert!(!quarantine.contains("target = \"windows\""));
        assert!(quarantine.contains("target = \"linux\""));
    }

    #[test]
    fn auto_merge_missing_state_exits_two() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "auto-merge",
            "999",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(2));
        assert!(stderr.is_empty());
        assert!(
            String::from_utf8(stdout)
                .expect("utf8")
                .contains("no ship state found")
        );
    }

    #[test]
    fn auto_merge_missing_state_already_merged_is_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        let snapshot = temp.path().join("pr.json");
        std::fs::write(&snapshot, r#"{"state":"MERGED"}"#).expect("write snapshot");
        let cli = Cli::parse_from([
            "shipyard",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "auto-merge",
            "500",
            "--pr-snapshot-file",
            snapshot.to_str().expect("snapshot"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        assert!(
            String::from_utf8(stdout)
                .expect("utf8")
                .contains("already merged")
        );
    }

    #[test]
    fn auto_merge_in_flight_exits_three() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store
            .save(&auto_merge_state(10, &[("macos", "pending")]))
            .expect("save");
        let cli = Cli::parse_from([
            "shipyard",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "auto-merge",
            "10",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(3));
        assert!(stderr.is_empty());
        assert!(
            String::from_utf8(stdout)
                .expect("utf8")
                .contains("in flight")
        );
    }

    #[test]
    fn auto_merge_required_failure_exits_one() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store
            .save(&auto_merge_state(
                11,
                &[("macos", "pass"), ("linux", "fail")],
            ))
            .expect("save");
        let cli = Cli::parse_from([
            "shipyard",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "auto-merge",
            "11",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(1));
        assert!(stderr.is_empty());
        let output = String::from_utf8(stdout).expect("utf8");
        assert!(output.contains("targets failed"));
        assert!(output.contains("linux"));
    }

    #[test]
    fn auto_merge_green_merges_and_archives_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store
            .save(&auto_merge_state(
                12,
                &[("macos", "pass"), ("linux", "pass")],
            ))
            .expect("save");
        // The auto-merge preflight (issue #321) verifies the live PR head
        // matches the validated SHA before merging. `auto_merge_state` seeds
        // `head_sha = "a"*40`; pin the live head to the same value so the
        // happy-path merge proceeds.
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            format!(r#"{{"headRefOid":"{}"}}"#, "a".repeat(40)),
        )
        .expect("write snapshot");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "auto-merge",
            "12",
            "--merge-method",
            "rebase",
            "--admin",
            "--merge-result",
            "success",
            "--pr-snapshot-file",
            snapshot.to_str().expect("snapshot path"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "auto-merge");
        assert_eq!(value["event"], "merged");
        assert_eq!(value["pr"], 12);
        assert!(store.get(12).is_none());
        assert_eq!(store.list_archived().len(), 1);
    }

    #[test]
    fn auto_merge_green_refuses_when_live_head_supersedes_validated_sha() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        // `auto_merge_state` seeds the validated `head_sha = "a"*40`.
        store
            .save(&auto_merge_state(
                42,
                &[("macos", "pass"), ("linux", "pass")],
            ))
            .expect("save");
        // Live head advanced to a different SHA after validation.
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            format!(r#"{{"headRefOid":"{}"}}"#, "b".repeat(40)),
        )
        .expect("write snapshot");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "auto-merge",
            "42",
            // Prove the merge WOULD have succeeded if the preflight let it
            // through — the refusal must come from the SHA check, not a
            // simulated merge failure.
            "--merge-result",
            "success",
            "--pr-snapshot-file",
            snapshot.to_str().expect("snapshot path"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(1));
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "auto-merge");
        assert_eq!(value["event"], "superseded-sha");
        assert_eq!(value["pr"], 42);
        assert_eq!(value["validated"], "a".repeat(40));
        assert_eq!(value["current"], "b".repeat(40));
        // State must stay active (not archived) so the new head can be
        // re-validated.
        assert!(store.get(42).is_some());
        assert_eq!(store.list_archived().len(), 0);
    }

    #[test]
    fn auto_merge_green_refuses_when_pr_is_retargeted_after_validation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store
            .save(&auto_merge_state(
                43,
                &[("macos", "pass"), ("linux", "pass")],
            ))
            .expect("save");
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            format!(
                r#"{{"headRefOid":"{}","baseRefName":"release"}}"#,
                "a".repeat(40)
            ),
        )
        .expect("write snapshot");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "auto-merge",
            "43",
            "--merge-result",
            "success",
            "--pr-snapshot-file",
            snapshot.to_str().expect("snapshot path"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(1));
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["event"], "merge-failed");
        assert!(
            value["error"]
                .as_str()
                .expect("error")
                .contains("retargeted from validated base main to release")
        );
        assert!(store.get(43).is_some());
        assert_eq!(store.list_archived().len(), 0);
    }

    #[test]
    fn auto_merge_green_fails_closed_when_live_head_is_unreadable() {
        // The single most important property: if the live PR head cannot be
        // verified (network error, missing/empty head field), the preflight
        // must NOT merge blind — it fails closed. Here the snapshot has no
        // readable head, so `fetch_live_head_sha` returns None even though the
        // merge itself is told to succeed.
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store
            .save(&auto_merge_state(
                7,
                &[("macos", "pass"), ("linux", "pass")],
            ))
            .expect("save");
        let snapshot = temp.path().join("pr.json");
        std::fs::write(&snapshot, r#"{"headRefOid":""}"#).expect("write snapshot");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "auto-merge",
            "7",
            "--merge-result",
            "success",
            "--pr-snapshot-file",
            snapshot.to_str().expect("snapshot path"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(1));
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "auto-merge");
        assert_eq!(value["event"], "merge-failed");
        assert_eq!(value["pr"], 7);
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("verify live PR head"),
            "error should explain the fail-closed head verification: {value:?}"
        );
        // State stays active for a retry once the head can be verified.
        assert!(store.get(7).is_some());
        assert_eq!(store.list_archived().len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn auto_merge_archives_when_merge_error_reports_already_merged() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store
            .save(&auto_merge_state(13, &[("macos", "pass")]))
            .expect("save");
        let merge = temp.path().join("merge-fails-after-merge.sh");
        std::fs::write(
            &merge,
            "#!/usr/bin/env bash\nprintf 'Pull request danielraffel/pulp#13 was already merged\\nfailed to delete local branch feature/x: used by worktree\\n' >&2\nexit 1\n",
        )
        .expect("merge script");
        make_executable(&merge);
        // Satisfy the issue #321 preflight: pin the live head to the validated
        // SHA ("a"*40 from `auto_merge_state`) so the custom merge command runs
        // and exercises the already-merged archive escape hatch.
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            format!(r#"{{"headRefOid":"{}"}}"#, "a".repeat(40)),
        )
        .expect("write snapshot");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "auto-merge",
            "13",
            "--merge-command",
            merge.to_str().expect("merge script"),
            "--pr-snapshot-file",
            snapshot.to_str().expect("snapshot path"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["event"], "merged");
        assert!(value["cleanup_warning"].as_str().is_some_and(|warning| {
            warning.contains("already merged") && warning.contains("worktree")
        }));
        assert!(store.get(13).is_none());
        assert_eq!(store.list_archived().len(), 1);
    }

    // Regression coverage for Shipyard issue #296: without an isolated
    // `--pr-snapshot-file`, `execute_auto_merge`'s failure path falls
    // through to `pr_is_merged`, which shells out to `gh pr view <pr>`
    // against the process CWD's `origin` remote. When that remote happens
    // to host a real merged PR with the same number (PR #14 in
    // danielraffel/Shipyard *is* merged), the test would observe
    // `AutoMergeOutcome::Merged` instead of the synthetic `MergeFailed`.
    // The snapshot file pins `pr_is_merged` to `state=OPEN` so the
    // failure-path archive escape hatch stays closed in the test.
    #[test]
    fn auto_merge_failure_preserves_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store
            .save(&auto_merge_state(14, &[("macos", "pass")]))
            .expect("save");
        // `state:OPEN` keeps the failure-path `pr_is_merged` escape hatch
        // closed; `headRefOid` matching the validated SHA ("a"*40) lets the
        // issue #321 preflight pass so the `--merge-result failure` path is
        // the thing under test.
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            format!(r#"{{"state":"OPEN","headRefOid":"{}"}}"#, "a".repeat(40)),
        )
        .expect("write snapshot");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "auto-merge",
            "14",
            "--merge-result",
            "failure",
            "--pr-snapshot-file",
            snapshot.to_str().expect("snapshot path"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(1));
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["event"], "merge-failed");
        assert!(store.get(14).is_some());
    }

    #[test]
    fn cloud_workflows_json_reports_discovered_workflows() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_test_workflow(temp.path());
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "workflows",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "cloud.workflows");
        assert_eq!(value["default"], "build");
        assert_eq!(value["workflows"]["ci"]["file"], "ci.yml");
        assert_eq!(value["workflows"]["build"]["file"], "ci.yml");
    }

    #[test]
    fn cloud_defaults_json_reports_resolved_dispatch_plan() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_test_workflow(temp.path());
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[project]\nname = \"demo\"\n\n[cloud]\nprovider = \"namespace\"\ndefault_workflow = \"ci\"\n",
        )
        .expect("config");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "defaults",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "cloud.defaults");
        assert_eq!(value["default_workflow"], "ci");
        assert_eq!(value["default_provider"], "namespace");
        assert_eq!(value["resolved"]["ci"]["ref"], "main");
        assert_eq!(value["resolved"]["ci"]["provider"], "namespace");
        assert_eq!(
            value["resolved"]["ci"]["dispatch_fields"]["runner_provider"],
            "namespace"
        );
    }

    #[test]
    fn cloud_workflows_human_reports_empty_discovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "workflows",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let stdout = String::from_utf8(stdout).expect("stdout");
        assert!(stdout.contains("No GitHub workflows discovered."));
    }

    #[test]
    fn cloud_defaults_human_renders_dispatch_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_test_workflow(temp.path());
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[project]\nname = \"demo\"\n\n[cloud]\nprovider = \"namespace\"\ndefault_workflow = \"ci\"\n",
        )
        .expect("config");
        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "defaults",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let stdout = String::from_utf8(stdout).expect("stdout");
        assert!(stdout.contains("default workflow: ci"));
        assert!(stdout.contains("default provider: namespace"));
        assert!(stdout.contains("ci.yml (runner_provider=namespace)"));
    }

    #[test]
    fn cloud_run_json_records_hidden_dispatch_run_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_test_workflow(temp.path());
        let state_dir = temp.path().join("state");
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[project]\nname = \"demo\"\n\n[cloud]\nprovider = \"namespace\"\ndefault_workflow = \"ci\"\n",
        )
        .expect("config");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            state_dir.to_str().expect("state path"),
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "run",
            "ci",
            "feature/x",
            "--provider",
            "namespace",
            "--run-id",
            "123456",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "cloud.run");
        assert_eq!(value["record"]["workflow_key"], "ci");
        assert_eq!(value["record"]["requested_ref"], "feature/x");
        assert_eq!(value["record"]["provider"], "namespace");
        assert_eq!(value["record"]["status"], "queued");
        assert_eq!(value["record"]["run_id"], "123456");
        assert_eq!(value["plan"]["ref"], "feature/x");
        assert!(value["plan"].get("ref_name").is_none());
        assert_eq!(
            value["plan"]["dispatch_fields"]["runner_provider"],
            "namespace"
        );
        let dispatch_id = value["record"]["dispatch_id"]
            .as_str()
            .expect("dispatch id");
        let records = CloudRecordStore::new(state_dir.join("cloud")).expect("store");
        let stored = records.get(dispatch_id).expect("record persisted");
        assert_eq!(stored.run_id.as_deref(), Some("123456"));
        assert_eq!(stored.status, "queued");
    }

    #[test]
    fn cloud_run_cli_selector_overrides_feed_dispatch_plan() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workflows = temp.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows).expect("workflow dir");
        std::fs::write(
            workflows.join("ci.yml"),
            "name: CI\non:\n  workflow_dispatch:\n    inputs:\n      runner_provider:\n        required: false\n      runner_selector:\n        required: false\n      runner_overrides:\n        required: false\n",
        )
        .expect("workflow");
        let state_dir = temp.path().join("state");
        let project_dir = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::write(
            project_dir.join("config.toml"),
            "[project]\nname = \"demo\"\n\n[cloud]\nprovider = \"github-hosted\"\ndefault_workflow = \"ci\"\n",
        )
        .expect("config");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            state_dir.to_str().expect("state path"),
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "run",
            "ci",
            "feature/x",
            "--runner-selector",
            "cli-selector",
            "--linux-runner-selector",
            "custom-linux",
            "--run-id",
            "456789",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(
            value["plan"]["dispatch_fields"]["runner_selector"],
            "cli-selector"
        );
        let runner_overrides: Value = serde_json::from_str(
            value["plan"]["dispatch_fields"]["runner_overrides"]
                .as_str()
                .expect("runner_overrides"),
        )
        .expect("runner_overrides json");
        assert_eq!(runner_overrides["linux-x64"], "custom-linux");
        assert_eq!(runner_overrides["windows-x64"], "windows-latest");
        assert_eq!(runner_overrides["macos-arm64"], "macos-15");
    }

    #[test]
    fn cloud_status_json_lists_records_newest_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = CloudRecordStore::new(temp.path().join("cloud")).expect("store");
        let mut older =
            CloudRunRecord::new("cloud-older", "ci", "ci.yml", "CI", "main", "namespace");
        older.updated_at = Some(Utc.with_ymd_and_hms(2026, 4, 24, 12, 0, 0).unwrap());
        let mut newer =
            CloudRunRecord::new("cloud-newer", "ci", "ci.yml", "CI", "main", "namespace");
        newer.updated_at = Some(Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).unwrap());
        store.save(&older).expect("older");
        store.save(&newer).expect("newer");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "status",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "cloud.status");
        let records = value["records"].as_array().expect("records");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["dispatch_id"], "cloud-newer");
        assert_eq!(records[1]["dispatch_id"], "cloud-older");
    }

    #[test]
    fn cloud_status_latest_human_reports_one_record() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = CloudRecordStore::new(temp.path().join("cloud")).expect("store");
        let mut older =
            CloudRunRecord::new("cloud-older", "ci", "ci.yml", "CI", "main", "namespace");
        older.updated_at = Some(Utc.with_ymd_and_hms(2026, 4, 24, 12, 0, 0).unwrap());
        let mut newer = CloudRunRecord::new(
            "cloud-newer",
            "release",
            "release.yml",
            "Release",
            "v1",
            "github-hosted",
        );
        newer.updated_at = Some(Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).unwrap());
        newer.status = "completed".to_owned();
        newer.conclusion = Some("success".to_owned());
        store.save(&older).expect("older");
        store.save(&newer).expect("newer");

        let cli = Cli::parse_from([
            "shipyard",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "status",
            "latest",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let stdout = String::from_utf8(stdout).expect("stdout");
        assert!(stdout.contains(
            "cloud-newer: release ref=v1 provider=github-hosted status=completed conclusion=success"
        ));
        assert!(!stdout.contains("cloud-older"));
    }

    #[test]
    fn cloud_add_lane_dry_run_does_not_mutate_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_test_workflow(temp.path());
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store
            .save(&auto_merge_state(20, &[("macos", "pending")]))
            .expect("save");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "add-lane",
            "--pr",
            "20",
            "--target",
            "windows",
            "--provider",
            "namespace",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "cloud.add-lane");
        assert_eq!(value["event"], "plan");
        assert_eq!(value["dry_run"], Value::Bool(true));
        assert!(store.get(20).expect("state").dispatched_runs.is_empty());
    }

    #[test]
    fn cloud_add_lane_apply_appends_run() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_test_workflow(temp.path());
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store
            .save(&auto_merge_state(21, &[("macos", "pending")]))
            .expect("save");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "add-lane",
            "--pr",
            "21",
            "--target",
            "windows",
            "--provider",
            "namespace",
            "--apply",
            "--run-id",
            "987654",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["event"], "applied");
        assert_eq!(value["run_id"], "987654");
        let state = store.get(21).expect("state");
        assert_eq!(state.dispatched_runs.len(), 1);
        assert_eq!(state.dispatched_runs[0].target, "windows");
        assert_eq!(state.dispatched_runs[0].provider, "namespace");
        assert_eq!(state.dispatched_runs[0].run_id, "987654");
        assert_eq!(state.dispatched_runs[0].status, "queued");
    }

    #[test]
    fn cloud_add_lane_existing_target_noops() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = auto_merge_state(22, &[("windows", "pending")]);
        state
            .dispatched_runs
            .push(run_for("windows", "namespace", "111"));
        store.save(&state).expect("save");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "add-lane",
            "--pr",
            "22",
            "--target",
            "windows",
            "--provider",
            "namespace",
            "--apply",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["event"], "noop");
        assert_eq!(value["already_tracked"], Value::Bool(true));
        assert_eq!(store.get(22).expect("state").dispatched_runs.len(), 1);
    }

    #[test]
    fn cloud_retarget_apply_replaces_existing_target_run() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_test_workflow(temp.path());
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = auto_merge_state(23, &[("macos", "pending")]);
        state.dispatched_runs.push(run_for("macos", "local", "111"));
        store.save(&state).expect("save");
        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "cloud",
            "retarget",
            "--pr",
            "23",
            "--target",
            "macos",
            "--provider",
            "namespace",
            "--apply",
            "--run-id",
            "222",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "cloud.retarget");
        assert_eq!(value["event"], "applied");
        assert_eq!(value["new_run_id"], "222");
        assert_eq!(value["run_cancel_fallback_used"], false);
        assert_eq!(value["stale_old_blocker_remains"], false);
        assert_eq!(value["stale_old_blocker_status"], "cleared");
        let state = store.get(23).expect("state");
        assert_eq!(state.dispatched_runs.len(), 1);
        assert_eq!(state.dispatched_runs[0].target, "macos");
        assert_eq!(state.dispatched_runs[0].provider, "namespace");
        assert_eq!(state.dispatched_runs[0].run_id, "222");
        assert_eq!(state.dispatched_runs[0].status, "queued");
    }

    #[test]
    fn pin_bump_no_pr_rewrites_pin_without_install_or_pr_steps() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("consumer");
        std::fs::create_dir_all(&repo).expect("repo");
        seed_consumer_pin_repo(&repo, "v0.40.0");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            repo.to_str().expect("repo path"),
            "pin",
            "bump",
            "--to",
            "0.99.0",
            "--no-pr",
            "--skip-verify",
            "--allow-downgrade",
            "--allow-redundant",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "pin");
        assert_eq!(value["event"], "bump");
        assert_eq!(value["result"], "edited");
        assert_eq!(value["from"], "v0.40.0");
        assert_eq!(value["to"], "v0.99.0");
        assert!(
            std::fs::read_to_string(repo.join("tools").join("shipyard.toml"))
                .expect("pin")
                .contains("version = \"v0.99.0\"")
        );
    }

    #[test]
    fn pin_show_refuses_without_consumer_pin() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "pin",
            "show",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(1));
        assert!(stdout.is_empty());
        assert!(
            String::from_utf8(stderr)
                .expect("stderr")
                .contains("tools/shipyard.toml")
        );
    }

    #[test]
    fn paths_json_uses_selected_mode_and_overrides() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "isolated",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "paths",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["mode"], "isolated");
        assert_eq!(value["binary_name"], "shipyard");
        assert_eq!(
            value["state_dir"].as_str(),
            Some(temp.path().to_string_lossy().as_ref())
        );
    }

    #[test]
    fn paths_human_prints_daemon_socket() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cli = Cli::parse_from([
            "shipyard",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "paths",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        let text = String::from_utf8(stdout).expect("utf8");
        assert!(text.contains("binary_name: shipyard"));
        assert!(text.contains("daemon_socket:"));
    }

    #[cfg(unix)]
    #[test]
    fn daemon_status_json_reports_running_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_registered_repos(temp.path(), &["owner/repo"]);
        let worker = spawn_test_daemon(temp.path(), vec!["owner/repo".to_owned()]);
        wait_for_daemon(temp.path());

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "daemon",
            "status",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());

        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "daemon:status");
        assert_eq!(value["running"], Value::Bool(true));
        assert_eq!(value["registered_repos"][0], "owner/repo");

        assert!(stop_running(temp.path()));
        worker.join().expect("join");
    }

    #[cfg(unix)]
    #[test]
    fn daemon_stop_json_reports_stopped_true() {
        let temp = tempfile::tempdir().expect("tempdir");
        let worker = spawn_test_daemon(temp.path(), vec!["owner/repo".to_owned()]);
        wait_for_daemon(temp.path());

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "daemon",
            "stop",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());

        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "daemon:stop");
        assert_eq!(value["stopped"], Value::Bool(true));

        worker.join().expect("join");
        assert!(read_daemon_status(temp.path()).is_none());
    }

    #[test]
    fn watch_no_active_ship_exits_2() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_git_repo(temp.path(), "feature/x");
        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "watch",
            "--no-follow",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(2));
        assert!(stderr.is_empty());
        assert!(
            String::from_utf8(stdout)
                .expect("utf8")
                .contains("No active ship state")
        );
    }

    #[test]
    fn watch_auto_detects_branch_and_returns_terminal_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_git_repo(temp.path(), "feature/x");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = ShipState::new(77, "owner/repo", "feature/x", "main", "a".repeat(40), "p1");
        state.evidence_snapshot = [("macos".to_owned(), "pass".to_owned())]
            .into_iter()
            .collect();
        state.dispatched_runs.push(DispatchedRun {
            target: "macos".to_owned(),
            provider: "local".to_owned(),
            run_id: "1".to_owned(),
            status: "completed".to_owned(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
            attempt: 1,
            last_heartbeat_at: None,
            phase: None,
            required: true,
        });
        store.save(&state).expect("save");

        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "watch",
            "--no-follow",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());
        assert!(String::from_utf8(stdout).expect("utf8").contains("PR #77"));
    }

    #[test]
    fn watch_missing_pr_exits_2_not_0() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_git_repo(temp.path(), "feature/x");
        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "watch",
            "--pr",
            "9999",
            "--no-follow",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(2));
        assert!(stderr.is_empty());
        assert!(
            String::from_utf8(stdout)
                .expect("utf8")
                .to_lowercase()
                .contains("no ship state found")
        );
    }

    #[test]
    fn watch_non_terminal_nofollow_exits_3() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_git_repo(temp.path(), "feature/x");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = ShipState::new(88, "owner/repo", "feature/x", "main", "a".repeat(40), "p1");
        state.evidence_snapshot = [("macos".to_owned(), "pending".to_owned())]
            .into_iter()
            .collect();
        store.save(&state).expect("save");

        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "watch",
            "--no-follow",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(3));
        assert!(stderr.is_empty());
        assert!(String::from_utf8(stdout).expect("utf8").contains("PR #88"));
    }

    #[test]
    fn watch_json_emits_update_with_required_field() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_git_repo(temp.path(), "feature/x");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = ShipState::new(12, "owner/repo", "feature/x", "main", "a".repeat(40), "p1");
        state.evidence_snapshot = [
            ("mac".to_owned(), "pass".to_owned()),
            ("windows".to_owned(), "fail".to_owned()),
        ]
        .into_iter()
        .collect();
        state.dispatched_runs = vec![
            DispatchedRun {
                target: "mac".to_owned(),
                provider: "local".to_owned(),
                run_id: "1".to_owned(),
                status: "completed".to_owned(),
                started_at: Utc::now(),
                updated_at: Utc::now(),
                attempt: 1,
                last_heartbeat_at: None,
                phase: None,
                required: true,
            },
            DispatchedRun {
                target: "windows".to_owned(),
                provider: "namespace".to_owned(),
                run_id: "2".to_owned(),
                status: "failed".to_owned(),
                started_at: Utc::now(),
                updated_at: Utc::now(),
                attempt: 1,
                last_heartbeat_at: None,
                phase: None,
                required: false,
            },
        ];
        store.save(&state).expect("save");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "watch",
            "--pr",
            "12",
            "--no-follow",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());

        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "watch");
        assert_eq!(value["event"], "update");
        let by_target = value["dispatched_runs"]
            .as_array()
            .expect("runs array")
            .iter()
            .map(|run| {
                (
                    run["target"].as_str().expect("target").to_owned(),
                    run.clone(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(by_target["mac"]["required"], Value::Bool(true));
        assert_eq!(by_target["windows"]["required"], Value::Bool(false));
    }

    #[test]
    fn watch_json_surfaces_reused_evidence() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_git_repo(temp.path(), "feature/x");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let mut state = ShipState::new(12, "owner/repo", "feature/x", "main", "a".repeat(40), "p1");
        state.evidence_snapshot = [
            ("macos".to_owned(), "pass".to_owned()),
            ("linux".to_owned(), "pass".to_owned()),
        ]
        .into_iter()
        .collect();
        store.save(&state).expect("save");
        evidence
            .record_scoped(
                &crate::evidence::repository_ship_evidence_scope(&state.repo, state.pr),
                &EvidenceRecord {
                    sha: state.head_sha.clone(),
                    branch: state.branch.clone(),
                    workload_scope: None,
                    target_name: "macos".to_owned(),
                    validation_build_type: None,
                    platform: "macos-arm64".to_owned(),
                    status: "pass".to_owned(),
                    backend: "reused".to_owned(),
                    source_head_sha: None,
                    source_tree_sha: None,
                    source_checkout_clean: None,
                    full_execution: None,
                    completed_at: Utc::now(),
                    duration_secs: None,
                    host: None,
                    primary_backend: None,
                    failover_reason: None,
                    provider: None,
                    runner_profile: None,
                    failure_class: None,
                    reused_from: Some("b".repeat(40)),
                    contract_digest: None,
                    stages_signature: None,
                },
            )
            .expect("record");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "watch",
            "--pr",
            "12",
            "--no-follow",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());

        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["evidence"]["macos"]["status"], "reused");
        assert_eq!(value["evidence"]["macos"]["reused_from"], "b".repeat(40));
        assert_eq!(value["evidence"]["linux"], "pass");
    }

    #[test]
    fn watch_human_surfaces_reused_evidence() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_git_repo(temp.path(), "feature/x");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence");
        let mut state = ShipState::new(13, "owner/repo", "feature/x", "main", "a".repeat(40), "p1");
        state.evidence_snapshot = [("macos".to_owned(), "pass".to_owned())]
            .into_iter()
            .collect();
        state.dispatched_runs.push(DispatchedRun {
            target: "macos".to_owned(),
            provider: "local".to_owned(),
            run_id: "1".to_owned(),
            status: "completed".to_owned(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
            attempt: 1,
            last_heartbeat_at: None,
            phase: None,
            required: true,
        });
        store.save(&state).expect("save");
        evidence
            .record_scoped(
                &crate::evidence::repository_ship_evidence_scope(&state.repo, state.pr),
                &EvidenceRecord {
                    sha: state.head_sha.clone(),
                    branch: state.branch.clone(),
                    workload_scope: None,
                    target_name: "macos".to_owned(),
                    validation_build_type: None,
                    platform: "macos-arm64".to_owned(),
                    status: "pass".to_owned(),
                    backend: "reused".to_owned(),
                    source_head_sha: None,
                    source_tree_sha: None,
                    source_checkout_clean: None,
                    full_execution: None,
                    completed_at: Utc::now(),
                    duration_secs: None,
                    host: None,
                    primary_backend: None,
                    failover_reason: None,
                    provider: None,
                    runner_profile: None,
                    failure_class: None,
                    reused_from: Some("cafebabe12345678".to_owned()),
                    contract_digest: None,
                    stages_signature: None,
                },
            )
            .expect("record");

        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "--state-dir",
            temp.path().to_str().expect("temp path"),
            "watch",
            "--pr",
            "13",
            "--no-follow",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());

        let output = String::from_utf8(stdout).expect("utf8");
        assert!(output.contains("reused"));
        assert!(output.contains("cafebab"));
    }

    #[test]
    fn wait_pr_json_match_exits_zero() {
        let temp = tempfile::tempdir().expect("tempdir");
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            serde_json::to_vec_pretty(&serde_json::json!({
                "number": 151,
                "headRefOid": "abc123",
                "state": "OPEN",
                "mergeable": "MERGEABLE",
                "mergeStateStatus": "CLEAN",
                "statusCheckRollup": [
                    {"name": "Linux", "conclusion": "SUCCESS", "state": "COMPLETED", "isRequired": true}
                ]
            }))
            .expect("json"),
        )
        .expect("write");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "wait",
            "pr",
            "151",
            "--state",
            "green",
            "--repo",
            "owner/repo",
            "--snapshot-file",
            snapshot.to_str().expect("snapshot"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty());

        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "wait:pr");
        assert_eq!(value["matched"], Value::Bool(true));
        assert_eq!(value["condition"]["type"], "pr_green");
        assert_eq!(value["condition"]["repo"], "owner/repo");
        assert_eq!(value["observed"]["head_sha"], "abc123");
    }

    #[test]
    fn wait_pr_nofallback_snapshot_miss_exits_six() {
        let temp = tempfile::tempdir().expect("tempdir");
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            serde_json::to_vec_pretty(&serde_json::json!({
                "number": 151,
                "headRefOid": "abc123",
                "state": "OPEN",
                "mergeable": "BLOCKED",
                "mergeStateStatus": "CLEAN",
                "statusCheckRollup": [
                    {"name": "Linux", "conclusion": "PENDING", "state": "IN_PROGRESS", "isRequired": true}
                ]
            }))
            .expect("json"),
        )
        .expect("write");

        let cli = Cli::parse_from([
            "shipyard",
            "--mode",
            "isolated",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "wait",
            "pr",
            "151",
            "--state",
            "green",
            "--no-fallback",
            "--repo",
            "owner/repo",
            "--snapshot-file",
            snapshot.to_str().expect("snapshot"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(WAIT_EXIT_NO_FALLBACK));
        assert!(stderr.is_empty());

        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["matched"], Value::Bool(false));
        assert_eq!(value["transport"], "polling");
    }

    #[test]
    fn wait_pr_green_unsupported_scope_exits_seven() {
        let temp = tempfile::tempdir().expect("tempdir");
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            serde_json::to_vec_pretty(&serde_json::json!({
                "number": 151,
                "headRefOid": "abc123",
                "mergeable": "BLOCKED",
                "mergeStateStatus": "BLOCKED_BY_RULESET",
                "statusCheckRollup": []
            }))
            .expect("json"),
        )
        .expect("write");

        let cli = Cli::parse_from([
            "shipyard",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "wait",
            "pr",
            "151",
            "--state",
            "green",
            "--repo",
            "owner/repo",
            "--snapshot-file",
            snapshot.to_str().expect("snapshot"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(WAIT_EXIT_UNSUPPORTED));
        assert!(stdout.is_empty());
        assert!(
            String::from_utf8(stderr)
                .expect("utf8")
                .contains("Rulesets / merge-queue governance")
        );
    }

    #[test]
    fn wait_run_success_wrong_terminal_exits_four() {
        let temp = tempfile::tempdir().expect("tempdir");
        let snapshot = temp.path().join("run.json");
        std::fs::write(
            &snapshot,
            serde_json::to_vec_pretty(&serde_json::json!({
                "databaseId": 42,
                "status": "completed",
                "conclusion": "failure"
            }))
            .expect("json"),
        )
        .expect("write");

        let cli = Cli::parse_from([
            "shipyard",
            "--json",
            "--cwd",
            temp.path().to_str().expect("temp path"),
            "wait",
            "run",
            "42",
            "--success",
            "--repo",
            "owner/repo",
            "--snapshot-file",
            snapshot.to_str().expect("snapshot"),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run_with(cli, &mut stdout, &mut stderr);

        assert_eq!(code, ExitCode::from(WAIT_EXIT_RUN_TERMINAL_WRONG));
        assert!(stderr.is_empty());

        let value: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "wait:run");
        assert_eq!(value["matched"], Value::Bool(false));
        assert_eq!(value["condition"]["require_success"], Value::Bool(true));
        assert_eq!(value["observed"]["conclusion"], "failure");
    }

    #[test]
    fn parse_github_repo_slug_handles_https_and_ssh() {
        assert_eq!(
            parse_github_repo_slug("https://github.com/danielraffel/Shipyard.git"),
            Some("danielraffel/Shipyard".to_owned())
        );
        assert_eq!(
            parse_github_repo_slug("git@github.com:danielraffel/Shipyard.git"),
            Some("danielraffel/Shipyard".to_owned())
        );
        assert_eq!(parse_github_repo_slug("file:///tmp/repo"), None);
    }
}
