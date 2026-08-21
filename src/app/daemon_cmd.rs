use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::Value;

use super::{CliFailure, cli::DaemonCommand};
use crate::daemon_ipc::read_daemon_status;
use crate::daemon_runtime::{
    DaemonRunConfig, DaemonRunError, DaemonSpawnFailedError, SpawnRequest, resolve_repos,
    run_blocking, spawn_detached, stop_running,
};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;

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
        let mut registered = status
            .get("registered_repos")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let missing_repo = repos.iter().any(|repo| !registered.contains(repo));
        if !missing_repo {
            return Ok(0);
        }
        registered.extend(repos);
        registered.sort();
        registered.dedup();
        if !stop_running(&runtime_paths.state_dir) {
            return Err(CliFailure::new(
                3,
                "daemon disappeared while registering a repository; retry the submission",
            ));
        }
        return spawn_execution_daemon(mode, runtime_paths, registered);
    }
    spawn_execution_daemon(mode, runtime_paths, repos)
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
    }
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
    let resolved_repos = resolve_repos(&runtime_paths.state_dir, repos);
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
        "daemon started (pid {pid}); registering {} repo(s).",
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

fn execute_daemon_refresh<F>(
    mode: RuntimeMode,
    global_dir_override: Option<PathBuf>,
    state_dir_override: Option<PathBuf>,
    runtime_paths: &RuntimePaths,
    explicit_repos: &[String],
    binary_override: Option<PathBuf>,
    spawn: F,
) -> Result<DaemonRefreshOutcome, DaemonRefreshError>
where
    F: FnOnce(&SpawnRequest) -> Result<u32, DaemonSpawnFailedError>,
{
    let prior_status = read_daemon_status(&runtime_paths.state_dir);
    let had_prior = prior_status.is_some();
    let prior_repos = if explicit_repos.is_empty() {
        registered_repos_from_status(prior_status.as_ref())
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
    let repos = if explicit_repos.is_empty() {
        prior_repos
    } else {
        resolve_repos(&runtime_paths.state_dir, explicit_repos)
    };
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

/// Refresh the daemon after a self-update has completed and the installed
/// binary has passed the installer's smoke verification. No existing daemon
/// is stopped before the caller reaches this function.
pub(super) fn refresh_after_verified_update(
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    installed_binary: PathBuf,
) -> Result<u32, String> {
    execute_daemon_refresh(
        mode,
        Some(runtime_paths.global_dir.clone()),
        Some(runtime_paths.state_dir.clone()),
        runtime_paths,
        &[],
        Some(installed_binary),
        spawn_detached,
    )
    .map(|outcome| outcome.new_pid)
    .map_err(|error| error.error)
}

fn registered_repos_from_status(status: Option<&Value>) -> Vec<String> {
    status
        .and_then(|status| status.get("registered_repos"))
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
            "daemon refreshed (new pid {}); registered {} repo(s).",
            outcome.new_pid,
            outcome.repos.len()
        )?;
    } else {
        writeln!(
            stdout,
            "no prior daemon; started fresh (pid {}); registered {} repo(s).",
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
        .get("registered_repos")
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
        "daemon running · tunnel={backend} · {url}\nsubscribers={subscribers} · repos={repos_text}"
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::ExitCode;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use serde_json::Value;

    use super::{
        DaemonRefreshError, DaemonRefreshOutcome, RuntimeMode, daemon_command,
        execute_daemon_refresh, registered_repos_from_status, render_daemon_refresh,
        render_daemon_refresh_error, render_daemon_start, render_daemon_status, render_daemon_stop,
        stop_running,
    };
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
            "daemon started (pid 4242); registering 1 repo(s).\n"
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
            "daemon refreshed (new pid 9090); registered 2 repo(s).\n"
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
            "no prior daemon; started fresh (pid 8080); registered 0 repo(s).\n"
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
    fn registered_repos_from_status_filters_to_strings() {
        let status = serde_json::json!({
            "registered_repos": ["owner/b", 10, null, "owner/a"]
        });

        assert_eq!(
            registered_repos_from_status(Some(&status)),
            vec!["owner/b", "owner/a"]
        );
        assert!(registered_repos_from_status(None).is_empty());
        assert!(registered_repos_from_status(Some(&serde_json::json!({}))).is_empty());
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
        assert!(text.contains("repos=owner/status"));

        assert!(stop_running(temp.path()));
        worker.join().expect("join");
    }

    #[cfg(unix)]
    #[test]
    fn daemon_run_with_repos_reports_already_running() {
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
        let temp = tempfile::tempdir().expect("tempdir");
        seed_registered_repos(temp.path(), &["owner/a", "owner/z"]);
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
            |_request: &SpawnRequest| Err(super::DaemonSpawnFailedError("boom".to_owned())),
        )
        .expect_err("spawn failure");

        assert!(!error.stopped_prior);
        assert_eq!(error.repos, vec!["owner/repo"]);
        assert_eq!(error.error, "boom");
        assert!(!stop_running(temp.path()));
    }
}
