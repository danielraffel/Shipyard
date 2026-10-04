use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

use super::{
    CliFailure,
    cli::{DaemonCommand, DaemonLauncherCommand},
};
use crate::daemon_ipc::read_daemon_status;
use crate::daemon_runtime::{
    DaemonRunConfig, DaemonRunError, DaemonSpawnFailedError, SpawnRequest, normalize_repos,
    resolve_repos, run_blocking, spawn_detached, stop_running,
};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::registrar::{Registrar, SUBSCRIBED_EVENTS};
use crate::repo_slug;
use crate::webhook_reconcile::{
    CONSECUTIVE_FAILED_DELIVERY_ALARM, DesiredWebhook, Finding, FindingCode, HostIdentity,
    ReconcileReport, Severity, reconcile,
};

/// Ensure the daemon that owns queued execution is live.
pub(super) fn ensure_execution_daemon(
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    repos: Vec<String>,
) -> Result<u32, CliFailure> {
    if let Some(status) = read_daemon_status(&runtime_paths.state_dir) {
        let running_version = status
            .get("shipyard_version")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if running_version != env!("CARGO_PKG_VERSION") {
            return Err(CliFailure::new(
                3,
                format!(
                    "running daemon version {running_version} cannot own jobs submitted by Shipyard {}; run `shipyard daemon refresh` first",
                    env!("CARGO_PKG_VERSION")
                ),
            ));
        }
        let configured = status
            .get("configured_repos")
            .or_else(|| status.get("registered_repos"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let requested = normalize_repos(repos);
        if configured_repositories_with_missing(configured.clone(), &requested).is_none() {
            return Ok(0);
        }
        // Only a repository the daemon does not watch costs a resolution: a
        // checkout whose remote still names a renamed repository must not
        // restart the daemon on every submission.
        let requested = resolve_watch_list(mode, &runtime_paths.state_dir, requested);
        let Some(configured) = configured_repositories_with_missing(configured, &requested) else {
            return Ok(0);
        };
        if !stop_running(&runtime_paths.state_dir) {
            return Err(CliFailure::new(
                3,
                "daemon disappeared while registering a repository; retry the submission",
            ));
        }
        return spawn_execution_daemon(mode, runtime_paths, configured);
    }
    let repos = resolve_watch_list(mode, &runtime_paths.state_dir, normalize_repos(repos));
    spawn_execution_daemon(mode, runtime_paths, repos)
}

/// Replace every watched slug GitHub now reports under another name with that
/// name, and say so on stderr (the daemon log, for a detached daemon). A slug
/// that cannot be resolved is kept: an offline probe must never drop a
/// repository from the watch list.
fn resolve_watch_list(mode: RuntimeMode, state_dir: &Path, repos: Vec<String>) -> Vec<String> {
    if repos.is_empty() {
        return repos;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registrar = Registrar::new_with_context(mode, state_dir, &cwd);
    let (resolved, renames) = repo_slug::canonicalize(repos, |repo| {
        let mut anonymous = repo_slug::anonymous_probe;
        let mut authenticated = |slug: &str| registrar.probe_repo_name(slug);
        repo_slug::resolve(repo, &mut [&mut anonymous, &mut authenticated])
    });
    for (from, to) in &renames {
        let _ = crate::writer_domain_lease::write_stderr(format_args!(
            "shipyard daemon: {from} now resolves to {to} on GitHub; watching {to}"
        ));
    }
    resolved
}

fn configured_repositories_with_missing(
    configured: Vec<String>,
    requested: &[String],
) -> Option<Vec<String>> {
    let mut configured = normalize_repos(configured);
    requested
        .iter()
        .any(|repo| !configured.contains(repo))
        .then(|| {
            configured.extend_from_slice(requested);
            configured.sort();
            configured.dedup();
            configured
        })
}

fn spawn_execution_daemon(
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    repos: Vec<String>,
) -> Result<u32, CliFailure> {
    let binary = std::env::current_exe()
        .map_err(|error| CliFailure::new(3, format!("failed to locate current binary: {error}")))?;
    spawn_detached(&SpawnRequest {
        binary,
        mode,
        global_dir_override: Some(runtime_paths.global_dir.clone()),
        state_dir_override: Some(runtime_paths.state_dir.clone()),
        state_dir: runtime_paths.state_dir.clone(),
        repos,
    })
    .map_err(|error| CliFailure::new(3, error.to_string()))
}

pub(super) fn daemon_command<W: Write>(
    command: DaemonCommand,
    mode: RuntimeMode,
    global_dir_override: Option<PathBuf>,
    state_dir_override: Option<PathBuf>,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        DaemonCommand::Start { repos, no_detach } => daemon_start(
            mode,
            global_dir_override,
            state_dir_override,
            runtime_paths,
            json,
            stdout,
            &repos,
            no_detach,
        ),
        DaemonCommand::Run { repos } => daemon_run(mode, runtime_paths, &repos),
        DaemonCommand::Stop => {
            let stopped = stop_running(&runtime_paths.state_dir);
            render_daemon_stop(stdout, json, stopped)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            Ok(ExitCode::SUCCESS)
        }
        DaemonCommand::Refresh { repos } => daemon_refresh(
            mode,
            global_dir_override,
            state_dir_override,
            runtime_paths,
            json,
            stdout,
            &repos,
        ),
        DaemonCommand::Status => {
            render_daemon_status(stdout, json, &runtime_paths.state_dir)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            Ok(ExitCode::SUCCESS)
        }
        DaemonCommand::Reconcile { repos } => {
            daemon_reconcile(mode, runtime_paths, json, stdout, &repos)
        }
        DaemonCommand::PruneWebhooks { repos, apply } => {
            super::daemon_prune_cmd::daemon_prune_webhooks(
                mode,
                runtime_paths,
                json,
                stdout,
                &repos,
                apply,
            )
        }
        DaemonCommand::Launcher { command } => daemon_launcher_command(
            command,
            mode,
            global_dir_override.as_deref(),
            state_dir_override.as_deref(),
            runtime_paths,
            json,
            stdout,
        ),
        DaemonCommand::Supervise {
            exec,
            repos,
            contract,
            in_place,
        } => daemon_supervise(
            mode,
            global_dir_override,
            state_dir_override,
            runtime_paths,
            exec,
            repos,
            contract,
            in_place,
            stdout,
        ),
        DaemonCommand::LauncherProbe { paths, result } => daemon_launcher_probe(&paths, &result),
    }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn daemon_supervise<W: Write>(
    mode: RuntimeMode,
    global_dir_override: Option<PathBuf>,
    state_dir_override: Option<PathBuf>,
    runtime_paths: &RuntimePaths,
    exec: Option<PathBuf>,
    repos: Vec<String>,
    contract: bool,
    in_place: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if contract {
        writeln!(stdout, "{}", crate::daemon_launcher::SUPERVISE_CONTRACT)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ExitCode::SUCCESS);
    }
    let binary = exec.ok_or_else(|| CliFailure::new(2, "--exec is required"))?;
    let request = SpawnRequest {
        binary,
        mode,
        global_dir_override,
        state_dir_override,
        state_dir: runtime_paths.state_dir.clone(),
        repos,
    };
    if in_place {
        let error = crate::daemon_launcher::exec_daemon_in_place(&request);
        return Err(CliFailure::new(1, error.to_string()));
    }
    let code = crate::daemon_launcher::supervise(&request)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}

#[cfg(not(unix))]
#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
fn daemon_supervise<W: Write>(
    _mode: RuntimeMode,
    _global_dir_override: Option<PathBuf>,
    _state_dir_override: Option<PathBuf>,
    _runtime_paths: &RuntimePaths,
    _exec: Option<PathBuf>,
    _repos: Vec<String>,
    _contract: bool,
    _in_place: bool,
    _stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    Err(CliFailure::new(
        2,
        "daemon supervise is only supported on Unix",
    ))
}

#[cfg(unix)]
fn daemon_launcher_probe(paths: &[PathBuf], result: &Path) -> Result<ExitCode, CliFailure> {
    let ok = crate::daemon_launcher::probe(paths, result)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[cfg(not(unix))]
fn daemon_launcher_probe(_paths: &[PathBuf], _result: &Path) -> Result<ExitCode, CliFailure> {
    Err(CliFailure::new(
        2,
        "daemon launcher-probe is only supported on Unix",
    ))
}

#[cfg(target_os = "macos")]
fn daemon_launcher_command<W: Write>(
    command: DaemonLauncherCommand,
    mode: RuntimeMode,
    global_dir_override: Option<&Path>,
    state_dir_override: Option<&Path>,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    use crate::daemon_launcher as launcher;

    let home = crate::paths::home_dir();
    let state_dir = &runtime_paths.state_dir;
    match command {
        DaemonLauncherCommand::Install {
            probe_paths,
            wait_secs,
        } => {
            let source = std::env::current_exe()
                .and_then(std::fs::canonicalize)
                .map_err(|error| {
                    CliFailure::new(3, format!("failed to locate current binary: {error}"))
                })?;
            let probe_paths = if probe_paths.is_empty() {
                launcher::default_probe_paths(
                    &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
                )
            } else {
                probe_paths
            };
            let mut mode_arguments: Vec<std::ffi::OsString> =
                vec!["--mode".into(), mode.as_str().into()];
            if let Some(global_dir) = global_dir_override {
                mode_arguments.extend(["--global-dir".into(), global_dir.into()]);
            }
            if let Some(state) = state_dir_override {
                mode_arguments.extend(["--state-dir".into(), state.into()]);
            }
            let mut launchctl = launcher::SystemLaunchctl::for_current_user();
            let record = launcher::install(
                &mut launcher::InstallPlan {
                    source,
                    home,
                    state_dir: state_dir.clone(),
                    mode_arguments,
                    probe_paths,
                    wait: std::time::Duration::from_secs(wait_secs),
                    launchctl: &mut launchctl,
                },
                &mut std::io::stderr(),
            )
            .map_err(|error| CliFailure::new(3, error))?;
            render_launcher_record(stdout, json, "daemon:launcher:install", Some(&record), true)
        }
        DaemonLauncherCommand::Status => {
            let record = launcher::read_record(state_dir);
            let active = launcher::active_launcher(state_dir).is_some();
            render_launcher_record(
                stdout,
                json,
                "daemon:launcher:status",
                record.as_ref(),
                active,
            )
        }
        DaemonLauncherCommand::Uninstall => {
            let existed =
                launcher::uninstall(state_dir, &home).map_err(|error| CliFailure::new(3, error))?;
            if json {
                let mut data = BTreeMap::new();
                data.insert("was_installed".to_owned(), Value::Bool(existed));
                write_json_envelope(stdout, "daemon:launcher:uninstall", data)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            } else if existed {
                writeln!(
                    stdout,
                    "launcher deactivated; the next `shipyard daemon refresh` spawns the daemon directly."
                )
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            } else {
                writeln!(stdout, "launcher was not installed.")
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::needless_pass_by_value)]
fn daemon_launcher_command<W: Write>(
    _command: DaemonLauncherCommand,
    _mode: RuntimeMode,
    _global_dir_override: Option<&Path>,
    _state_dir_override: Option<&Path>,
    _runtime_paths: &RuntimePaths,
    _json: bool,
    _stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    Err(CliFailure::new(
        2,
        "the daemon launcher manages a macOS privacy identity and is only available on macOS",
    ))
}

#[cfg(target_os = "macos")]
fn render_launcher_record<W: Write>(
    stdout: &mut W,
    json: bool,
    command: &str,
    record: Option<&crate::daemon_launcher::LauncherRecord>,
    active: bool,
) -> Result<ExitCode, CliFailure> {
    let failure = |error: &dyn std::fmt::Display| CliFailure::new(1, error.to_string());
    if json {
        let mut data = BTreeMap::new();
        data.insert("installed".to_owned(), Value::Bool(record.is_some()));
        data.insert("active".to_owned(), Value::Bool(active));
        data.insert(
            "record".to_owned(),
            serde_json::to_value(record).map_err(|error| failure(&error))?,
        );
        write_json_envelope(stdout, command, data).map_err(|error| failure(&error))?;
        return Ok(ExitCode::SUCCESS);
    }
    match record {
        None => writeln!(
            stdout,
            "launcher not installed; the daemon inherits the privacy identity of whatever starts it."
        )
        .map_err(|error| failure(&error))?,
        Some(record) => writeln!(
            stdout,
            "launcher {} ({})\n  path: {}\n  launchd label: {}\n  probed: {}\n  installed by {} at {}",
            if active { "active" } else { "INACTIVE" },
            if active {
                "the daemon is started through launchd"
            } else {
                "the launcher file changed or is missing; rerun `shipyard daemon launcher install`"
            },
            record.launcher_path.display(),
            record.label,
            record
                .probed_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            record.installed_by_version,
            record.installed_at
        )
        .map_err(|error| failure(&error))?,
    }
    Ok(ExitCode::SUCCESS)
}

/// Compare the webhook this host INTENDS against the one GitHub HOLDS.
///
/// The daemon already knows its own tunnel URL and prints it; GitHub already
/// serves the registered hook. Both were individually correct throughout the
/// outage that motivated this command. Only the comparison was missing, and a
/// comparison nobody performs has no symptom — so this exists to perform it on
/// demand, from a scheduler, and to exit with a code a shell can branch on.
fn daemon_reconcile<W: Write>(
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
    requested_repos: &[String],
) -> Result<ExitCode, CliFailure> {
    let identity = crate::tunnel::probe_tailscale().host_identity();
    let desired = DesiredWebhook::for_identity(&identity, &SUBSCRIBED_EVENTS);

    let repos = resolve_repos(&runtime_paths.state_dir, requested_repos);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registrar = Registrar::new_with_context(mode, &runtime_paths.state_dir, &cwd);

    let mut reports: Vec<(String, ReconcileReport)> = Vec::new();

    // The daemon's own advertised URL is a third party to this comparison. If
    // it disagrees with this host's identity, the daemon is serving a stale
    // name and every hook it registers will inherit the staleness — so the
    // disagreement is reported before any repository is consulted.
    let mut preflight = daemon_url_findings(&runtime_paths.state_dir, &identity);
    preflight.extend(public_ingress_findings(&runtime_paths.state_dir));

    if repos.is_empty() {
        preflight.push(Finding::new(
            FindingCode::ObservationUnreadable,
            Severity::Warn,
            "no repositories are configured, so no webhook could be compared".to_owned(),
            "Pass --repo, or configure repositories, before reading this as a clean result."
                .to_owned(),
        ));
    }

    for repo in &repos {
        let desired_url = desired
            .as_ref()
            .map_or_else(String::new, |desired| desired.callback_url.clone());
        let observation = registrar.observe(repo, &desired_url);
        let report = reconcile(
            &identity,
            desired.as_ref(),
            observation.as_ref(),
            CONSECUTIVE_FAILED_DELIVERY_ALARM,
        );
        reports.push((repo.clone(), report));
    }

    let severity = reports
        .iter()
        .map(|(_, report)| report.severity())
        .chain(preflight.iter().map(|finding| finding.severity))
        .max()
        .unwrap_or(Severity::Warn);

    render_reconcile(stdout, json, &identity, &preflight, &reports, severity)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;

    let code = u8::try_from(ReconcileReport::exit_code(severity)).unwrap_or(2);
    Ok(ExitCode::from(code))
}

/// Compare a running daemon's advertised tunnel URL against this host's
/// identity. Returns no findings when no daemon is running: that is a separate
/// condition with its own existing check, not a webhook drift.
fn daemon_url_findings(state_dir: &Path, identity: &HostIdentity) -> Vec<Finding> {
    let Some(status) = read_daemon_status(state_dir) else {
        return Vec::new();
    };
    let advertised = status
        .get("tunnel")
        .and_then(Value::as_object)
        .and_then(|tunnel| tunnel.get("url"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let HostIdentity::Known(name) = identity else {
        return Vec::new();
    };
    match advertised {
        Some(url) if !url_names_host(&url, name) => vec![Finding::new(
            FindingCode::UrlDrift,
            Severity::Alarm,
            format!("the running daemon advertises {url}, which does not name this host ({name})"),
            "The daemon is serving a stale tunnel URL. Restart it so it \
             republishes under this host's current name; every hook it \
             registers until then inherits the stale name."
                .to_owned(),
        )],
        _ => Vec::new(),
    }
}

/// Whether `url`'s host component is exactly `name`.
///
/// Substring matching would be close enough for today's inputs and wrong in
/// general: it accepts a URL that merely mentions the host in its PATH, and it
/// accepts any host that this name is a prefix of. Both are "the URL is not
/// this host" answered as agreement, which is the one direction this
/// comparison must never get wrong — a false match reports no drift, and no
/// drift is indistinguishable from nobody having looked.
fn url_names_host(url: &str, name: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split('/').next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = host_port
        .rsplit_once(':')
        .filter(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
        .map_or(host_port, |(host, _)| host);
    host.trim_end_matches('.')
        .eq_ignore_ascii_case(name.trim_end_matches('.'))
}

fn render_reconcile<W: Write>(
    stdout: &mut W,
    json: bool,
    identity: &HostIdentity,
    preflight: &[Finding],
    reports: &[(String, ReconcileReport)],
    severity: Severity,
) -> Result<(), Box<dyn std::error::Error>> {
    let identity_text = match identity {
        HostIdentity::Known(name) => name.clone(),
        HostIdentity::Unreadable { detail } => format!("UNREADABLE ({detail})"),
    };

    if json {
        let mut data = BTreeMap::new();
        data.insert("identity".to_owned(), Value::String(identity_text));
        data.insert("severity".to_owned(), Value::String(severity.to_string()));
        data.insert(
            "exit_code".to_owned(),
            Value::from(ReconcileReport::exit_code(severity)),
        );
        let finding_json = |finding: &Finding| {
            serde_json::json!({
                "code": finding.code.as_str(),
                "severity": finding.severity.to_string(),
                "summary": finding.summary,
                "remedy": finding.remedy,
            })
        };
        data.insert(
            "preflight".to_owned(),
            Value::Array(preflight.iter().map(finding_json).collect()),
        );
        data.insert(
            "repos".to_owned(),
            Value::Array(
                reports
                    .iter()
                    .map(|(repo, report)| {
                        serde_json::json!({
                            "repo": repo,
                            "severity": report.severity().to_string(),
                            "findings": report.findings.iter().map(finding_json).collect::<Vec<_>>(),
                        })
                    })
                    .collect(),
            ),
        );
        write_json_envelope(stdout, "daemon:reconcile", data)?;
        return Ok(());
    }

    writeln!(stdout, "host identity: {identity_text}")?;
    for finding in preflight {
        writeln!(
            stdout,
            "  [{}] {}: {}",
            finding.severity,
            finding.code.as_str(),
            finding.summary
        )?;
        writeln!(stdout, "      -> {}", finding.remedy)?;
    }
    for (repo, report) in reports {
        writeln!(stdout, "{repo}: {}", report.severity())?;
        for finding in &report.findings {
            writeln!(
                stdout,
                "  [{}] {}: {}",
                finding.severity,
                finding.code.as_str(),
                finding.summary
            )?;
            if finding.severity != Severity::Ok {
                writeln!(stdout, "      -> {}", finding.remedy)?;
            }
        }
    }
    writeln!(stdout, "verdict: {severity}")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn daemon_start<W: Write>(
    mode: RuntimeMode,
    global_dir_override: Option<PathBuf>,
    state_dir_override: Option<PathBuf>,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
    repos: &[String],
    no_detach: bool,
) -> Result<ExitCode, CliFailure> {
    let resolved_repos = resolve_watch_list(
        mode,
        &runtime_paths.state_dir,
        resolve_repos(&runtime_paths.state_dir, repos),
    );
    if no_detach {
        return daemon_run_with_repos(mode, runtime_paths, resolved_repos);
    }

    let binary = std::env::current_exe()
        .map_err(|error| CliFailure::new(3, format!("failed to locate current binary: {error}")))?;
    let pid = spawn_detached(&SpawnRequest {
        binary,
        mode,
        global_dir_override,
        state_dir_override,
        state_dir: runtime_paths.state_dir.clone(),
        repos: resolved_repos.clone(),
    })
    .map_err(|error| CliFailure::new(3, error.to_string()))?;

    render_daemon_start(stdout, json, pid, &resolved_repos)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
fn daemon_refresh<W: Write>(
    mode: RuntimeMode,
    global_dir_override: Option<PathBuf>,
    state_dir_override: Option<PathBuf>,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
    repos: &[String],
) -> Result<ExitCode, CliFailure> {
    match execute_daemon_refresh(
        mode,
        global_dir_override,
        state_dir_override,
        runtime_paths,
        repos,
        None,
        |repos| resolve_watch_list(mode, &runtime_paths.state_dir, repos),
        spawn_detached,
    ) {
        Ok(outcome) => {
            render_daemon_refresh(stdout, json, &outcome)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            render_daemon_refresh_error(stdout, json, &error)
                .map_err(|render_error| CliFailure::new(1, render_error.to_string()))?;
            if json {
                Err(CliFailure::new(3, ""))
            } else {
                Err(CliFailure::new(3, error.error))
            }
        }
    }
}

fn daemon_run(
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    repos: &[String],
) -> Result<ExitCode, CliFailure> {
    let resolved_repos = resolve_repos(&runtime_paths.state_dir, repos);
    daemon_run_with_repos(mode, runtime_paths, resolved_repos)
}

fn daemon_run_with_repos(
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    repos: Vec<String>,
) -> Result<ExitCode, CliFailure> {
    spawn_rederive_sweep(mode, runtime_paths);
    match run_blocking(DaemonRunConfig {
        mode,
        global_dir: runtime_paths.global_dir.clone(),
        state_dir: runtime_paths.state_dir.clone(),
        repos,
    }) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(DaemonRunError::AlreadyRunning) => {
            Err(CliFailure::new(2, "daemon already running".to_owned()))
        }
        Err(error) => Err(CliFailure::new(1, error.to_string())),
    }
}

/// Keyed runs left without a host re-derivation, at most this many, are
/// picked up when the daemon starts.
const STARTUP_REDERIVE_CAP: usize = 8;

/// Re-derive, in the background, keyed runs whose completion path never
/// recorded a verdict (a crash, a restart). Each trip, if any, runs `gh` with
/// the configuration of the checkout that ran the keyed run.
fn spawn_rederive_sweep(mode: RuntimeMode, runtime_paths: &RuntimePaths) {
    let state_dir = runtime_paths.state_dir.clone();
    let global_dir = runtime_paths.global_dir.clone();
    std::thread::spawn(move || {
        let gh = |checkout: &Path, args: &[String]| {
            let config = crate::config::LoadedConfig::load_from_cwd_with_global_dir(
                mode,
                checkout,
                global_dir.clone(),
            )
            .map_err(|error| error.to_string())?;
            crate::cloud::GitHubActions::from_loaded_config(checkout, &config)
                .run_gh(args)
                .map_err(|error| error.to_string())
        };
        for (identity, outcome) in
            super::reuse_rederive::sweep(&state_dir, STARTUP_REDERIVE_CAP, &gh)
        {
            if let Err(error) = outcome {
                eprintln!(
                    "shipyard daemon: host re-derivation of {} PR #{} {} at {} failed: {error}",
                    identity.repository, identity.pull_request, identity.target, identity.head_sha
                );
            }
        }
    });
}

fn render_daemon_start<W: Write>(
    stdout: &mut W,
    json: bool,
    pid: u32,
    repos: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("pid".to_owned(), Value::from(pid));
        data.insert("repos".to_owned(), serde_json::to_value(repos)?);
        write_json_envelope(stdout, "daemon:start", data)?;
        return Ok(());
    }

    writeln!(
        stdout,
        "daemon started (pid {pid}); advertising {} repo(s).",
        repos.len()
    )?;
    Ok(())
}

fn render_daemon_stop<W: Write>(
    stdout: &mut W,
    json: bool,
    stopped: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("stopped".to_owned(), Value::Bool(stopped));
        write_json_envelope(stdout, "daemon:stop", data)?;
        return Ok(());
    }

    if stopped {
        writeln!(stdout, "daemon stopped.")?;
    } else {
        writeln!(stdout, "daemon wasn't running.")?;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct DaemonRefreshOutcome {
    stopped_prior: bool,
    new_pid: u32,
    repos: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct DaemonRefreshError {
    stopped_prior: bool,
    repos: Vec<String>,
    error: String,
}

#[allow(clippy::too_many_arguments)]
fn execute_daemon_refresh<R, F>(
    mode: RuntimeMode,
    global_dir_override: Option<PathBuf>,
    state_dir_override: Option<PathBuf>,
    runtime_paths: &RuntimePaths,
    explicit_repos: &[String],
    binary_override: Option<PathBuf>,
    resolve_watch: R,
    spawn: F,
) -> Result<DaemonRefreshOutcome, DaemonRefreshError>
where
    R: FnOnce(Vec<String>) -> Vec<String>,
    F: FnOnce(&SpawnRequest) -> Result<u32, DaemonSpawnFailedError>,
{
    let prior_status = read_daemon_status(&runtime_paths.state_dir);
    let had_prior = prior_status.is_some();
    let prior_repos = if explicit_repos.is_empty() {
        configured_repos_from_status(prior_status.as_ref())
    } else {
        Vec::new()
    };
    let stopped_prior = stop_running(&runtime_paths.state_dir);
    if had_prior && !stopped_prior && read_daemon_status(&runtime_paths.state_dir).is_some() {
        return Err(DaemonRefreshError {
            stopped_prior: false,
            repos: prior_repos,
            error: "prior daemon did not stop; refusing to report a refreshed daemon".to_owned(),
        });
    }
    // A refresh re-resolves the watch set rather than copying the prior
    // daemon's slugs verbatim, so a repository renamed while the daemon ran
    // is watched under its new name from here on.
    let repos = resolve_watch(if explicit_repos.is_empty() {
        prior_repos
    } else {
        resolve_repos(&runtime_paths.state_dir, explicit_repos)
    });
    let binary = binary_override
        .map_or_else(std::env::current_exe, Ok)
        .map_err(|error| DaemonRefreshError {
            stopped_prior,
            repos: repos.clone(),
            error: format!("failed to locate current binary: {error}"),
        })?;
    let request = SpawnRequest {
        binary,
        mode,
        global_dir_override,
        state_dir_override,
        state_dir: runtime_paths.state_dir.clone(),
        repos: repos.clone(),
    };
    let new_pid = spawn(&request).map_err(|error| DaemonRefreshError {
        stopped_prior,
        repos: repos.clone(),
        error: error.to_string(),
    })?;

    Ok(DaemonRefreshOutcome {
        stopped_prior,
        new_pid,
        repos,
    })
}

fn configured_repos_from_status(status: Option<&Value>) -> Vec<String> {
    status
        .and_then(|status| {
            status
                .get("configured_repos")
                .or_else(|| status.get("registered_repos"))
        })
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn render_daemon_refresh<W: Write>(
    stdout: &mut W,
    json: bool,
    outcome: &DaemonRefreshOutcome,
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "stopped_prior".to_owned(),
            Value::Bool(outcome.stopped_prior),
        );
        data.insert("new_pid".to_owned(), Value::from(outcome.new_pid));
        data.insert("repos".to_owned(), serde_json::to_value(&outcome.repos)?);
        write_json_envelope(stdout, "daemon:refresh", data)?;
        return Ok(());
    }

    if outcome.stopped_prior {
        writeln!(
            stdout,
            "daemon refreshed (new pid {}); advertising {} repo(s).",
            outcome.new_pid,
            outcome.repos.len()
        )?;
    } else {
        writeln!(
            stdout,
            "no prior daemon; started fresh (pid {}); advertising {} repo(s).",
            outcome.new_pid,
            outcome.repos.len()
        )?;
    }
    Ok(())
}

fn render_daemon_refresh_error<W: Write>(
    stdout: &mut W,
    json: bool,
    error: &DaemonRefreshError,
) -> Result<(), Box<dyn std::error::Error>> {
    if !json {
        return Ok(());
    }

    let mut data = BTreeMap::new();
    data.insert("ok".to_owned(), Value::Bool(false));
    data.insert("stopped_prior".to_owned(), Value::Bool(error.stopped_prior));
    data.insert("error".to_owned(), Value::String(error.error.clone()));
    data.insert("repos".to_owned(), serde_json::to_value(&error.repos)?);
    write_json_envelope(stdout, "daemon:refresh", data)?;
    Ok(())
}

fn render_daemon_status<W: Write>(
    stdout: &mut W,
    json: bool,
    state_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = read_daemon_status(state_dir);
    if json {
        let mut data = BTreeMap::new();
        if let Some(status) = status {
            data.insert("running".to_owned(), Value::Bool(true));
            let Value::Object(map) = status else {
                return Err("daemon status must serialize as an object".into());
            };
            for (key, value) in map {
                data.insert(key, value);
            }
        } else {
            data.insert("running".to_owned(), Value::Bool(false));
        }
        write_json_envelope(stdout, "daemon:status", data)?;
        return Ok(());
    }

    let Some(status) = status else {
        writeln!(stdout, "daemon is not running.")?;
        return Ok(());
    };

    let tunnel = status
        .get("tunnel")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let url = tunnel.get("url").and_then(Value::as_str).unwrap_or("—");
    let backend = tunnel.get("backend").and_then(Value::as_str).unwrap_or("—");
    let subscribers = status
        .get("subscribers")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let repos = status
        .get("configured_repos")
        .or_else(|| status.get("registered_repos"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let repos_text = if repos.is_empty() {
        "—".to_owned()
    } else {
        repos.join(", ")
    };

    writeln!(
        stdout,
        "daemon running · tunnel={backend} · {url}\nsubscribers={subscribers} · advertises={repos_text}"
    )?;
    if let Some(line) = public_ingress_line(tunnel.get("public_ingress")) {
        writeln!(stdout, "{line}")?;
    }
    Ok(())
}

/// One status line for the daemon's last public-ingress self-check.
fn public_ingress_line(ingress: Option<&Value>) -> Option<String> {
    let ingress = ingress?.as_object()?;
    let state = ingress.get("state").and_then(Value::as_str)?;
    let detail = ingress
        .get("detail")
        .and_then(Value::as_str)
        .filter(|detail| !detail.is_empty());
    Some(match (state, detail) {
        ("ok", _) => "public ingress: ok".to_owned(),
        ("failing", detail) => format!(
            "public ingress: FAILING ({}); GitHub cannot reach this daemon, waits fall back to polling",
            detail.unwrap_or("no detail")
        ),
        (other, detail) => format!(
            "public ingress: {other} ({})",
            detail.unwrap_or("no detail")
        ),
    })
}

/// A daemon whose own public-ingress self-check is failing receives no
/// webhooks, whatever GitHub's hook configuration says.
fn public_ingress_findings(state_dir: &Path) -> Vec<Finding> {
    let Some(status) = read_daemon_status(state_dir) else {
        return Vec::new();
    };
    let ingress = status
        .get("tunnel")
        .and_then(|tunnel| tunnel.get("public_ingress"));
    if ingress
        .and_then(|ingress| ingress.get("state"))
        .and_then(Value::as_str)
        != Some("failing")
    {
        return Vec::new();
    }
    vec![Finding::new(
        FindingCode::EndpointUnreachable,
        Severity::Alarm,
        public_ingress_line(ingress).unwrap_or_default(),
        "The public relays cannot reach this host's tunnel. The daemon re-applies \
         its funnel after two failing checks; if it stays failing, toggle the \
         funnel or check the node's Funnel state in the Tailscale admin console."
            .to_owned(),
    )]
}

#[cfg(test)]
mod tests {
    use super::{public_ingress_line, url_names_host};

    #[test]
    fn public_ingress_line_names_a_failing_check_and_its_reason() {
        let failing = serde_json::json!({
            "state": "failing",
            "detail": "208.111.34.11: TLS handshake failed at the relay",
            "checked_at": 1.0,
            "consecutive_failures": 1,
        });
        let line = public_ingress_line(Some(&failing)).expect("line");
        assert!(
            line.starts_with("public ingress: FAILING (208.111.34.11: TLS"),
            "{line}"
        );
        assert!(line.contains("waits fall back to polling"), "{line}");
        let ok = serde_json::json!({"state": "ok", "detail": ""});
        assert_eq!(
            public_ingress_line(Some(&ok)).as_deref(),
            Some("public ingress: ok")
        );
        // An older daemon reports no ingress check at all; say nothing.
        assert_eq!(public_ingress_line(None), None);
        assert_eq!(public_ingress_line(Some(&serde_json::Value::Null)), None);
    }

    /// The daemon's advertised URL is compared against this host's identity to
    /// catch a daemon serving a stale name. A false MATCH is the dangerous
    /// direction: it reports no drift, and no drift reads exactly like nobody
    /// having looked.
    #[test]
    fn a_url_that_merely_mentions_the_host_does_not_name_it() {
        let name = "daniels-mac-studio-3.taile2001.ts.net";

        // Control: the real URL for this host matches, so a rejection below
        // cannot be the matcher refusing everything.
        assert!(url_names_host(&format!("https://{name}/webhook"), name));

        // Substring matching accepts both of these. Neither is this host.
        assert!(
            !url_names_host(&format!("https://elsewhere.example/{name}"), name),
            "the host lives in the authority, not the path"
        );
        assert!(
            !url_names_host(
                "https://daniels-mac-studio-3.taile2001.ts.net/webhook",
                "daniels-mac-studio"
            ),
            "a name must not match a host it is merely a prefix of"
        );
    }

    #[test]
    fn host_matching_ignores_scheme_port_case_and_trailing_dot() {
        let name = "daniels-mac-studio-3.taile2001.ts.net";
        assert!(url_names_host(
            "https://daniels-mac-studio-3.taile2001.ts.net",
            name
        ));
        assert!(url_names_host(
            "https://daniels-mac-studio-3.taile2001.ts.net:8443/webhook",
            name
        ));
        assert!(url_names_host(
            "https://DANIELS-MAC-STUDIO-3.TAILE2001.TS.NET/webhook",
            name
        ));
        // `tailscale status --json` reports DNSName with a trailing dot.
        assert!(url_names_host(
            "https://daniels-mac-studio-3.taile2001.ts.net./webhook",
            name
        ));
        // A collision-suffixed rename is a different host.
        assert!(!url_names_host(
            "https://daniels-mac-studio.taile2001.ts.net/webhook",
            name
        ));
    }

    use std::process::ExitCode;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use serde_json::Value;

    use super::{
        DaemonRefreshError, DaemonRefreshOutcome, RuntimeMode, configured_repos_from_status,
        configured_repositories_with_missing, daemon_command, execute_daemon_refresh,
        render_daemon_refresh, render_daemon_refresh_error, render_daemon_start,
        render_daemon_status, render_daemon_stop, stop_running,
    };

    #[test]
    fn configured_repository_membership_uses_canonical_policy_identity() {
        assert_eq!(
            configured_repositories_with_missing(
                vec!["generous-corp/pulp".to_owned()],
                &["generous-corp/pulp".to_owned()],
            ),
            None
        );
        assert_eq!(
            configured_repositories_with_missing(
                vec!["Generous-Corp/Pulp".to_owned()],
                &["generous-corp/forge".to_owned()],
            ),
            Some(vec![
                "generous-corp/forge".to_owned(),
                "generous-corp/pulp".to_owned(),
            ])
        );
    }
    #[cfg(unix)]
    use super::{DaemonRunConfig, daemon_run_with_repos, read_daemon_status, run_blocking};
    use crate::app::cli::DaemonCommand;
    use crate::daemon_runtime::SpawnRequest;
    use crate::paths::RuntimePaths;

    #[cfg(unix)]
    fn spawn_test_daemon(
        state_dir: &std::path::Path,
        repos: Vec<String>,
    ) -> std::thread::JoinHandle<()> {
        let state_dir = state_dir.to_path_buf();
        std::thread::spawn(move || {
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
            std::thread::sleep(Duration::from_millis(10));
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

    fn runtime_paths(state_dir: &std::path::Path) -> RuntimePaths {
        RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            None,
            Some(state_dir.to_path_buf()),
        )
    }

    #[test]
    fn render_daemon_start_json_and_human_contracts() {
        let mut json_out = Vec::new();
        render_daemon_start(
            &mut json_out,
            true,
            4242,
            &["owner/a".to_owned(), "owner/b".to_owned()],
        )
        .expect("json render");
        let payload: Value = serde_json::from_slice(&json_out).expect("json payload");
        assert_eq!(payload["command"], "daemon:start");
        assert_eq!(payload["pid"], 4242);
        assert_eq!(payload["repos"][0], "owner/a");
        assert_eq!(payload["repos"][1], "owner/b");

        let mut human_out = Vec::new();
        render_daemon_start(&mut human_out, false, 4242, &["owner/a".to_owned()])
            .expect("human render");
        assert_eq!(
            String::from_utf8(human_out).expect("utf8"),
            "daemon started (pid 4242); advertising 1 repo(s).\n"
        );
    }

    #[test]
    fn render_daemon_stop_json_and_human_contracts() {
        let mut json_out = Vec::new();
        render_daemon_stop(&mut json_out, true, true).expect("json render");
        let payload: Value = serde_json::from_slice(&json_out).expect("json payload");
        assert_eq!(payload["command"], "daemon:stop");
        assert_eq!(payload["stopped"], true);

        let mut stopped_out = Vec::new();
        render_daemon_stop(&mut stopped_out, false, true).expect("human render");
        assert_eq!(
            String::from_utf8(stopped_out).expect("utf8"),
            "daemon stopped.\n"
        );

        let mut missing_out = Vec::new();
        render_daemon_stop(&mut missing_out, false, false).expect("human render");
        assert_eq!(
            String::from_utf8(missing_out).expect("utf8"),
            "daemon wasn't running.\n"
        );
    }

    #[test]
    fn render_daemon_refresh_json_human_and_error_contracts() {
        let outcome = DaemonRefreshOutcome {
            stopped_prior: true,
            new_pid: 9090,
            repos: vec!["owner/a".to_owned(), "owner/b".to_owned()],
        };
        let mut json_out = Vec::new();
        render_daemon_refresh(&mut json_out, true, &outcome).expect("json render");
        let payload: Value = serde_json::from_slice(&json_out).expect("json payload");
        assert_eq!(payload["command"], "daemon:refresh");
        assert_eq!(payload["stopped_prior"], true);
        assert_eq!(payload["new_pid"], 9090);
        assert_eq!(payload["repos"][1], "owner/b");

        let mut human_out = Vec::new();
        render_daemon_refresh(&mut human_out, false, &outcome).expect("human render");
        assert_eq!(
            String::from_utf8(human_out).expect("utf8"),
            "daemon refreshed (new pid 9090); advertising 2 repo(s).\n"
        );

        let fresh = DaemonRefreshOutcome {
            stopped_prior: false,
            new_pid: 8080,
            repos: Vec::new(),
        };
        let mut fresh_out = Vec::new();
        render_daemon_refresh(&mut fresh_out, false, &fresh).expect("human render");
        assert_eq!(
            String::from_utf8(fresh_out).expect("utf8"),
            "no prior daemon; started fresh (pid 8080); advertising 0 repo(s).\n"
        );

        let error = DaemonRefreshError {
            stopped_prior: false,
            repos: vec!["owner/a".to_owned()],
            error: "spawn failed".to_owned(),
        };
        let mut error_out = Vec::new();
        render_daemon_refresh_error(&mut error_out, true, &error).expect("json render");
        let payload: Value = serde_json::from_slice(&error_out).expect("json payload");
        assert_eq!(payload["command"], "daemon:refresh");
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["error"], "spawn failed");

        let mut human_error_out = Vec::new();
        render_daemon_refresh_error(&mut human_error_out, false, &error).expect("human render");
        assert!(human_error_out.is_empty());
    }

    #[test]
    fn configured_repos_from_status_is_authoritative_and_filters_to_strings() {
        let status = serde_json::json!({
            "configured_repos": ["owner/b", 10, null, "owner/a"],
            "registered_repos": ["owner/registered-only"]
        });

        assert_eq!(
            configured_repos_from_status(Some(&status)),
            vec!["owner/b", "owner/a"]
        );
        assert_eq!(
            configured_repos_from_status(Some(
                &serde_json::json!({"registered_repos": ["owner/legacy"]}),
            )),
            vec!["owner/legacy"]
        );
        assert!(configured_repos_from_status(None).is_empty());
        assert!(configured_repos_from_status(Some(&serde_json::json!({}))).is_empty());
    }

    #[test]
    fn daemon_command_stop_json_reports_not_running() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = runtime_paths(temp.path());
        let mut out = Vec::new();

        let code = daemon_command(
            DaemonCommand::Stop,
            RuntimeMode::Isolated,
            None,
            Some(temp.path().to_path_buf()),
            &paths,
            true,
            &mut out,
        )
        .expect("stop should succeed");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(payload["command"], "daemon:stop");
        assert_eq!(payload["stopped"], false);
    }

    #[test]
    fn render_daemon_status_reports_not_running() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut json_out = Vec::new();
        render_daemon_status(&mut json_out, true, temp.path()).expect("json render");
        let payload: Value = serde_json::from_slice(&json_out).expect("json payload");
        assert_eq!(payload["command"], "daemon:status");
        assert_eq!(payload["running"], false);

        let mut human_out = Vec::new();
        render_daemon_status(&mut human_out, false, temp.path()).expect("human render");
        assert_eq!(
            String::from_utf8(human_out).expect("utf8"),
            "daemon is not running.\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn render_daemon_status_reports_running_daemon() {
        let _process_fixture = crate::test_support::lock_process_tree_for_test();
        let temp = tempfile::tempdir().expect("tempdir");
        seed_registered_repos(temp.path(), &["owner/status"]);
        let worker = spawn_test_daemon(temp.path(), vec!["owner/status".to_owned()]);
        wait_for_daemon(temp.path());

        let mut json_out = Vec::new();
        render_daemon_status(&mut json_out, true, temp.path()).expect("json render");
        let payload: Value = serde_json::from_slice(&json_out).expect("json payload");
        assert_eq!(payload["command"], "daemon:status");
        assert_eq!(payload["running"], true);
        assert_eq!(payload["registered_repos"][0], "owner/status");

        let mut human_out = Vec::new();
        render_daemon_status(&mut human_out, false, temp.path()).expect("human render");
        let text = String::from_utf8(human_out).expect("utf8");
        assert!(text.contains("daemon running"));
        assert!(text.contains("advertises=owner/status"));

        assert!(stop_running(temp.path()));
        worker.join().expect("join");
    }

    #[test]
    fn daemon_human_output_says_advertise_not_operate() {
        // `--repo` only controls what the daemon ADVERTISES from its status
        // endpoint; it is not the daemon's working set. Human-facing strings
        // that say "registering"/"registered"/"watches" invite reading the
        // status banner as the set of repos the daemon acts on, which is how a
        // stale, unresolvable slug once survived unnoticed across restarts.
        let mut out = Vec::new();
        render_daemon_start(&mut out, false, 4242, &["owner/a".to_owned()]).unwrap();
        let start = String::from_utf8(out).unwrap();
        assert!(start.contains("advertising"), "start: {start}");
        for implies_operation in ["registering", "registered", "watches"] {
            assert!(
                !start.contains(implies_operation),
                "start text must not imply the daemon operates on these repos: {start}"
            );
        }

        let mut out = Vec::new();
        render_daemon_refresh(
            &mut out,
            false,
            &DaemonRefreshOutcome {
                stopped_prior: true,
                new_pid: 9090,
                repos: vec!["owner/a".to_owned(), "owner/b".to_owned()],
            },
        )
        .unwrap();
        let refresh = String::from_utf8(out).unwrap();
        assert!(refresh.contains("advertising"), "refresh: {refresh}");
        for implies_operation in ["registering", "registered", "watches"] {
            assert!(
                !refresh.contains(implies_operation),
                "refresh text must not imply operation: {refresh}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn daemon_run_with_repos_reports_already_running() {
        let _process_fixture = crate::test_support::lock_process_tree_for_test();
        let temp = tempfile::tempdir().expect("tempdir");
        let worker = spawn_test_daemon(temp.path(), vec!["owner/run".to_owned()]);
        wait_for_daemon(temp.path());

        let err = daemon_run_with_repos(
            RuntimeMode::Isolated,
            &runtime_paths(temp.path()),
            vec!["owner/run".to_owned()],
        )
        .expect_err("second run should fail");

        assert_eq!(err.code, 2);
        assert_eq!(err.message, "daemon already running");
        assert!(stop_running(temp.path()));
        worker.join().expect("join");
    }

    #[cfg(unix)]
    #[test]
    fn refresh_reuses_prior_daemon_repos_when_none_are_explicit() {
        let _process_fixture = crate::test_support::lock_process_tree_for_test();
        let temp = tempfile::tempdir().expect("tempdir");
        // Successful remote registration is deliberately divergent: refresh
        // authority comes from the daemon's configured watch set, not from the
        // subset whose webhook setup happened to succeed.
        seed_registered_repos(temp.path(), &["owner/registered-only"]);
        let worker = spawn_test_daemon(
            temp.path(),
            vec![
                "owner/z".to_owned(),
                "owner/a".to_owned(),
                "owner/a".to_owned(),
            ],
        );
        wait_for_daemon(temp.path());

        let outcome = execute_daemon_refresh(
            RuntimeMode::Isolated,
            None,
            Some(temp.path().to_path_buf()),
            &runtime_paths(temp.path()),
            &[],
            None,
            |repos| repos,
            |request: &SpawnRequest| {
                assert_eq!(request.repos, vec!["owner/a", "owner/z"]);
                Ok(4321)
            },
        )
        .expect("refresh outcome");

        assert!(outcome.stopped_prior);
        assert_eq!(outcome.new_pid, 4321);
        assert_eq!(outcome.repos, vec!["owner/a", "owner/z"]);
        worker.join().expect("join");
    }

    #[cfg(unix)]
    #[test]
    fn refresh_explicit_repos_override_prior_status_and_are_normalized() {
        let _process_fixture = crate::test_support::lock_process_tree_for_test();
        let temp = tempfile::tempdir().expect("tempdir");
        let worker = spawn_test_daemon(temp.path(), vec!["owner/old".to_owned()]);
        wait_for_daemon(temp.path());

        let outcome = execute_daemon_refresh(
            RuntimeMode::Isolated,
            None,
            Some(temp.path().to_path_buf()),
            &runtime_paths(temp.path()),
            &[
                "owner/b".to_owned(),
                "owner/a".to_owned(),
                "owner/b".to_owned(),
            ],
            None,
            |repos| repos,
            |request: &SpawnRequest| {
                assert_eq!(request.repos, vec!["owner/a", "owner/b"]);
                Ok(1234)
            },
        )
        .expect("refresh outcome");

        assert!(outcome.stopped_prior);
        assert_eq!(outcome.repos, vec!["owner/a", "owner/b"]);
        worker.join().expect("join");
    }

    #[cfg(unix)]
    #[test]
    fn refresh_rewatches_a_renamed_repository_under_its_new_name() {
        let _process_fixture = crate::test_support::lock_process_tree_for_test();
        let temp = tempfile::tempdir().expect("tempdir");
        let worker = spawn_test_daemon(
            temp.path(),
            vec!["danielraffel/pulp".to_owned(), "owner/other".to_owned()],
        );
        wait_for_daemon(temp.path());

        let outcome = execute_daemon_refresh(
            RuntimeMode::Isolated,
            None,
            Some(temp.path().to_path_buf()),
            &runtime_paths(temp.path()),
            &[],
            None,
            |repos| {
                crate::repo_slug::canonicalize(repos, |repo| {
                    if repo == "danielraffel/pulp" {
                        crate::repo_slug::SlugResolution::Renamed {
                            to: "Generous-Corp/pulp".to_owned(),
                        }
                    } else {
                        crate::repo_slug::SlugResolution::Canonical
                    }
                })
                .0
            },
            |request: &SpawnRequest| {
                assert_eq!(request.repos, vec!["generous-corp/pulp", "owner/other"]);
                Ok(77)
            },
        )
        .expect("refresh outcome");

        assert_eq!(outcome.repos, vec!["generous-corp/pulp", "owner/other"]);
        worker.join().expect("join");
    }

    #[test]
    fn refresh_allows_empty_repo_list_without_running_daemon() {
        let temp = tempfile::tempdir().expect("tempdir");
        let installed_binary = temp.path().join("installed-shipyard");

        let outcome = execute_daemon_refresh(
            RuntimeMode::Isolated,
            None,
            Some(temp.path().to_path_buf()),
            &runtime_paths(temp.path()),
            &[],
            Some(installed_binary.clone()),
            |repos| repos,
            |request: &SpawnRequest| {
                assert!(request.repos.is_empty());
                assert_eq!(request.binary, installed_binary);
                Ok(999)
            },
        )
        .expect("refresh outcome");

        assert!(!outcome.stopped_prior);
        assert!(outcome.repos.is_empty());
    }

    #[test]
    fn refresh_reports_spawn_failure_with_context() {
        let temp = tempfile::tempdir().expect("tempdir");

        let error = execute_daemon_refresh(
            RuntimeMode::Isolated,
            None,
            Some(temp.path().to_path_buf()),
            &runtime_paths(temp.path()),
            &["owner/repo".to_owned()],
            None,
            |repos| repos,
            |_request: &SpawnRequest| Err(super::DaemonSpawnFailedError("boom".to_owned())),
        )
        .expect_err("spawn failure");

        assert!(!error.stopped_prior);
        assert_eq!(error.repos, vec!["owner/repo"]);
        assert_eq!(error.error, "boom");
        assert!(!stop_running(temp.path()));
    }
}
