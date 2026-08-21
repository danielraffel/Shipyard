//! Governed exact-version rollout for configured Shipyard host classes.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::time::Duration;

use serde_json::Value;

use super::CliFailure;
use crate::capacity::{HostClassConfig, parse_host_classes};
use crate::config::LoadedConfig;
use crate::executor::ssh::shlex_quote;
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::paths::{RuntimePaths, home_dir, unattended_tool_path};

const REMOTE_MINIMAL_PATH: &str =
    "/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
const HOST_UPDATE_TIMEOUT: Duration = Duration::from_mins(10);
const REMOTE_UPDATE_TIMEOUT: Duration = Duration::from_mins(9);
const MIN_FLEET_UPDATE_TARGET: [u64; 3] = [0, 100, 0];
const REMOTE_SUPERVISOR: &str = r#"use strict;
use warnings;
use POSIX qw(WNOHANG setsid);
my $seconds = shift @ARGV;
my $pid = fork();
die "fork failed: $!" unless defined $pid;
if ($pid == 0) {
    setsid() >= 0 or die "setsid failed: $!";
    exec @ARGV;
    die "exec failed: $!";
}
local $SIG{ALRM} = sub {
    kill 'TERM', -$pid;
    my $leader_reaped = 0;
    for (1..50) {
        my $done = waitpid($pid, WNOHANG);
        if ($done == $pid) {
            $leader_reaped = 1;
            last;
        }
        select undef, undef, undef, 0.1;
    }
    # The leader may exit on TERM while an installer/download descendant in
    # the same session ignores it. Always close the whole group with KILL.
    kill 'KILL', -$pid;
    waitpid($pid, 0) unless $leader_reaped;
    exit 124;
};
alarm $seconds;
waitpid($pid, 0);
alarm 0;
my $status = $?;
exit(($status & 127) ? 128 + ($status & 127) : $status >> 8);
"#;

pub(super) struct FleetUpdateArgs {
    pub(super) to: String,
    pub(super) apply: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HostUpdatePlan {
    class: String,
    ssh: Option<String>,
    binary: PathBuf,
    target: String,
    command: String,
    runtime_mode: RuntimeMode,
    global_dir: PathBuf,
    state_dir: PathBuf,
}

pub(super) fn fleet_update_command<W: Write>(
    args: &FleetUpdateArgs,
    _mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let target = normalize_exact_tag(&args.to)?;
    // Fleet mutation topology is machine policy. Never let a repository's
    // tracked overlay select SSH destinations or executable paths.
    let config = LoadedConfig::load_machine_global_from_dir(runtime_paths.global_dir.clone())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let classes = parse_host_classes(&config.data).map_err(|error| CliFailure::new(2, error))?;
    if classes.is_empty() {
        return Err(CliFailure::new(
            1,
            "No [host_class.<name>] configured — fleet-update has no rollout targets.",
        ));
    }
    let plans = classes
        .iter()
        .map(|class| host_update_plan(class, &target))
        .collect::<Result<Vec<_>, _>>()?;

    if !args.apply {
        render_plan(stdout, json, &target, &plans)?;
        return Ok(ExitCode::SUCCESS);
    }

    let mut failures = Vec::new();
    for plan in &plans {
        let result = execute_plan(plan);
        match result {
            Ok(PlanExecution::Completed(status)) if status.success() => {
                render_host_result(stdout, json, &target, plan, true, None)?;
            }
            Ok(PlanExecution::Completed(status)) => {
                let code = status.code().unwrap_or(-1);
                failures.push(format!("{} exited {code}", plan.class));
                render_host_result(
                    stdout,
                    json,
                    &target,
                    plan,
                    false,
                    Some(&format!("update command exited {code}")),
                )?;
            }
            Ok(PlanExecution::TimedOut) => {
                failures.push(format!(
                    "{} timed out after {}s",
                    plan.class,
                    HOST_UPDATE_TIMEOUT.as_secs()
                ));
                render_host_result(
                    stdout,
                    json,
                    &target,
                    plan,
                    false,
                    Some(&format!(
                        "update timed out after {}s; supervised process tree terminated",
                        HOST_UPDATE_TIMEOUT.as_secs()
                    )),
                )?;
            }
            Err(error) => {
                failures.push(format!("{}: {error}", plan.class));
                render_host_result(stdout, json, &target, plan, false, Some(&error.to_string()))?;
            }
        }
    }
    if failures.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    Err(CliFailure::new(
        1,
        format!("fleet update incomplete: {}", failures.join("; ")),
    ))
}

fn normalize_exact_tag(raw: &str) -> Result<String, CliFailure> {
    let raw = raw.trim();
    let version = raw.strip_prefix('v').unwrap_or(raw);
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.chars().all(|ch| ch.is_ascii_digit()))
    {
        return Err(CliFailure::new(
            2,
            "fleet-update --to requires an exact stable vMAJOR.MINOR.PATCH tag",
        ));
    }
    let parsed = parts
        .iter()
        .map(|part| part.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| {
            CliFailure::new(
                2,
                "fleet-update --to version components must fit in unsigned 64-bit integers",
            )
        })?;
    if parsed.as_slice() < MIN_FLEET_UPDATE_TARGET.as_slice() {
        return Err(CliFailure::new(
            2,
            "fleet-update cannot safely bootstrap or validate targets older than v0.100.0; use that release's documented manual rollback procedure",
        ));
    }
    Ok(format!("v{version}"))
}

#[allow(clippy::too_many_lines)] // One fail-closed validation boundary for the complete host profile.
fn host_update_plan(class: &HostClassConfig, target: &str) -> Result<HostUpdatePlan, CliFailure> {
    if let Some(host) = &class.ssh
        && (host.starts_with('-') || host.chars().any(char::is_control))
    {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{}.ssh is not a valid SSH destination",
                class.class
            ),
        ));
    }
    let binary = match (&class.ssh, &class.shipyard_bin) {
        (_, Some(binary)) => PathBuf::from(binary),
        (None, None) => std::env::current_exe().map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to resolve local Shipyard binary: {error}"),
            )
        })?,
        (Some(_), None) => {
            return Err(CliFailure::new(
                2,
                format!(
                    "host_class.{}.shipyard_bin must name the absolute remote binary; relative lookup is launch-environment drift and cannot establish binary identity",
                    class.class
                ),
            ));
        }
    };
    let is_remote = class.ssh.is_some();
    let binary_is_absolute = if is_remote {
        class
            .shipyard_bin
            .as_deref()
            .is_some_and(|path| path.starts_with('/'))
    } else {
        binary.is_absolute()
    };
    if !binary_is_absolute {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{}.shipyard_bin must be absolute; relative lookup is launch-environment drift, not proof the tool is absent",
                class.class
            ),
        ));
    }
    let expected_binary_name = if class.ssh.is_some() {
        "shipyard".to_owned()
    } else {
        format!("shipyard{}", std::env::consts::EXE_SUFFIX)
    };
    let binary_name = if is_remote {
        class
            .shipyard_bin
            .as_deref()
            .and_then(|path| path.rsplit('/').next())
    } else {
        binary.file_name().and_then(|name| name.to_str())
    };
    if binary_name != Some(expected_binary_name.as_str()) {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{}.shipyard_bin must end in /{} because the verified installer replaces that exact filename",
                class.class, expected_binary_name
            ),
        ));
    }
    let mode = class.shipyard_mode.as_deref().ok_or_else(|| {
        CliFailure::new(
            2,
            format!(
                "host_class.{}.shipyard_mode is required to identify the daemon context",
                class.class
            ),
        )
    })?;
    let runtime_mode = match mode {
        "shipyard" => RuntimeMode::Shipyard,
        "isolated" => RuntimeMode::Isolated,
        _ => {
            return Err(CliFailure::new(
                2,
                format!("host_class.{}.shipyard_mode is invalid", class.class),
            ));
        }
    };
    let global_dir = PathBuf::from(class.shipyard_global_dir.as_deref().ok_or_else(|| {
        CliFailure::new(
            2,
            format!(
                "host_class.{}.shipyard_global_dir is required to identify the daemon context",
                class.class
            ),
        )
    })?);
    let state_dir = PathBuf::from(class.shipyard_state_dir.as_deref().ok_or_else(|| {
        CliFailure::new(
            2,
            format!(
                "host_class.{}.shipyard_state_dir is required to identify the daemon context",
                class.class
            ),
        )
    })?);
    let daemon_paths_are_absolute = if is_remote {
        class
            .shipyard_global_dir
            .as_deref()
            .is_some_and(|path| path.starts_with('/'))
            && class
                .shipyard_state_dir
                .as_deref()
                .is_some_and(|path| path.starts_with('/'))
    } else {
        global_dir.is_absolute() && state_dir.is_absolute()
    };
    if !daemon_paths_are_absolute {
        return Err(CliFailure::new(
            2,
            format!(
                "host_class.{} daemon global/state directories must be absolute",
                class.class
            ),
        ));
    }
    let command = if class.ssh.is_some() {
        let helper = class.github_cli.as_deref().ok_or_else(|| {
            CliFailure::new(
                2,
                format!(
                    "host_class.{}.github_cli must name the absolute governed auth helper for bootstrap rollout",
                    class.class
                ),
            )
        })?;
        let helper = PathBuf::from(helper);
        if !helper.to_string_lossy().starts_with('/') {
            return Err(CliFailure::new(
                2,
                format!(
                    "host_class.{}.github_cli must be absolute for bootstrap rollout",
                    class.class
                ),
            ));
        }
        remote_update_command(
            &binary,
            target,
            &helper,
            runtime_mode.as_str(),
            &global_dir,
            &state_dir,
        )
    } else {
        String::new()
    };
    let mut plan = HostUpdatePlan {
        class: class.class.clone(),
        ssh: class.ssh.clone(),
        binary,
        target: target.to_owned(),
        command,
        runtime_mode,
        global_dir,
        state_dir,
    };
    if plan.ssh.is_none() {
        plan.command = local_update_command(&plan);
    }
    Ok(plan)
}

fn remote_update_command(
    binary: &Path,
    target: &str,
    auth_helper: &Path,
    mode: &str,
    global_dir: &Path,
    state_dir: &Path,
) -> String {
    let install_dir = binary.parent().unwrap_or_else(|| Path::new("/"));
    let version = target.strip_prefix('v').unwrap_or(target);
    let installer_url =
        format!("https://raw.githubusercontent.com/danielraffel/Shipyard/{target}/install.sh");
    let script = format!(
        "set -euo pipefail\n\
         token=\"$({} auth token)\"\n\
         installer=\"$(/usr/bin/mktemp)\"\n\
         staging_dir=\"$(/usr/bin/mktemp -d)\"\n\
         trap '/bin/rm -f \"$installer\"; /bin/rm -rf \"$staging_dir\"' EXIT\n\
         /usr/bin/curl -fsSL --output \"$installer\" {}\n\
         SHIPYARD_GITHUB_TOKEN=\"$token\" SHIPYARD_VERSION={} SHIPYARD_INSTALL_DIR=\"$staging_dir\" SHIPYARD_CURL_BIN=/usr/bin/curl /bin/bash \"$installer\"\n\
         staged_binary=\"$staging_dir/shipyard\"\n\
         staged_version=\"$(\"$staged_binary\" --version)\"\n\
         test \"$staged_version\" = {}\n\
         \"$staged_binary\" --mode {} --global-dir {} --state-dir {} update --to {} --check --unattended-fleet >/dev/null\n\
         SHIPYARD_GITHUB_TOKEN=\"$token\" SHIPYARD_VERSION={} SHIPYARD_INSTALL_DIR={} SHIPYARD_CURL_BIN=/usr/bin/curl /bin/bash \"$installer\"\n\
         unset token\n\
         actual_version=\"$({} --version)\"\n\
         test \"$actual_version\" = {}\n\
         {} --mode {} --global-dir {} --state-dir {} update --to {} --check --unattended-fleet >/dev/null\n\
         {} --mode {} --global-dir {} --state-dir {} daemon refresh",
        shlex_quote(&auth_helper.display().to_string()),
        shlex_quote(&installer_url),
        shlex_quote(version),
        shlex_quote(&format!("shipyard {version}")),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(target),
        shlex_quote(version),
        shlex_quote(&install_dir.display().to_string()),
        shlex_quote(&binary.display().to_string()),
        shlex_quote(&format!("shipyard {version}")),
        shlex_quote(&binary.display().to_string()),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(target),
        shlex_quote(&binary.display().to_string()),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
    );
    format!(
        "/usr/bin/env -i HOME=\"$HOME\" PATH={} /usr/bin/perl -e {} {} /bin/bash -c {}",
        REMOTE_MINIMAL_PATH,
        shlex_quote(REMOTE_SUPERVISOR),
        REMOTE_UPDATE_TIMEOUT.as_secs(),
        shlex_quote(&script),
    )
}

fn local_update_command(plan: &HostUpdatePlan) -> String {
    format!(
        "/usr/bin/env -i HOME={} PATH={} {} --mode {} --global-dir {} --state-dir {} update --to {} --refresh-daemon --unattended-fleet",
        shlex_quote(&home_dir().display().to_string()),
        shlex_quote(&unattended_tool_path().to_string_lossy()),
        shlex_quote(&plan.binary.display().to_string()),
        plan.runtime_mode.as_str(),
        shlex_quote(&plan.global_dir.display().to_string()),
        shlex_quote(&plan.state_dir.display().to_string()),
        shlex_quote(&plan.target),
    )
}

#[derive(Debug)]
enum PlanExecution {
    Completed(ExitStatus),
    TimedOut,
}

fn execute_plan(plan: &HostUpdatePlan) -> std::io::Result<PlanExecution> {
    execute_plan_with_timeout(plan, HOST_UPDATE_TIMEOUT)
}

fn execute_plan_with_timeout(
    plan: &HostUpdatePlan,
    timeout: Duration,
) -> std::io::Result<PlanExecution> {
    let mut command = if let Some(host) = &plan.ssh {
        let mut command = Command::new(ssh_binary());
        command.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            "-o",
            "StrictHostKeyChecking=yes",
        ]);
        command.arg(host).arg(&plan.command);
        command
    } else {
        let mut command = Command::new(&plan.binary);
        command
            .args(["--mode", plan.runtime_mode.as_str(), "--global-dir"])
            .arg(&plan.global_dir)
            .arg("--state-dir")
            .arg(&plan.state_dir)
            .args([
                "update",
                "--to",
                &plan.target,
                "--refresh-daemon",
                "--unattended-fleet",
            ])
            .env_clear()
            .env("HOME", home_dir())
            .env("PATH", unattended_tool_path());
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut process = crate::process::ProcessTree::spawn(&mut command)?;
    if process.wait_timeout(timeout)?.is_none() {
        process.terminate();
        return Ok(PlanExecution::TimedOut);
    }
    process.wait().map(PlanExecution::Completed)
}

fn ssh_binary() -> PathBuf {
    [PathBuf::from("/usr/bin/ssh"), PathBuf::from("/bin/ssh")]
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("ssh"))
}

fn render_plan<W: Write>(
    stdout: &mut W,
    json: bool,
    target: &str,
    plans: &[HostUpdatePlan],
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("plan"));
        data.insert("target".to_owned(), Value::from(target));
        data.insert("apply".to_owned(), Value::Bool(false));
        data.insert(
            "hosts".to_owned(),
            Value::Array(
                plans
                    .iter()
                    .map(|plan| {
                        serde_json::json!({
                            "class": plan.class,
                            "ssh": plan.ssh,
                            "binary": plan.binary,
                            "command": plan.command,
                        })
                    })
                    .collect(),
            ),
        );
        write_json_envelope(stdout, "runner.fleet-update", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "Fleet update plan for {target}:")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for plan in plans {
            let route = plan.ssh.as_deref().unwrap_or("local");
            writeln!(stdout, "  {} ({route}): {}", plan.class, plan.command)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        writeln!(stdout, "Re-run with --apply to execute.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn render_host_result<W: Write>(
    stdout: &mut W,
    json: bool,
    target: &str,
    plan: &HostUpdatePlan,
    ok: bool,
    error: Option<&str>,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("host_result"));
        data.insert("target".to_owned(), Value::from(target));
        data.insert("host_class".to_owned(), Value::from(plan.class.clone()));
        data.insert("ok".to_owned(), Value::Bool(ok));
        if let Some(error) = error {
            data.insert("error".to_owned(), Value::from(error));
        }
        write_json_envelope(stdout, "runner.fleet-update", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if ok {
        writeln!(
            stdout,
            "{}: updated to {target}; daemon refreshed",
            plan.class
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "{}: FAILED ({})",
            plan.class,
            error.unwrap_or("unknown error")
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(ssh: Option<&str>, shipyard_bin: Option<&str>) -> HostClassConfig {
        HostClassConfig {
            class: "m5".to_owned(),
            ssh: ssh.map(str::to_owned),
            cap: 2,
            tart_bin: "/opt/homebrew/bin/tart".to_owned(),
            tartci_bin: "/Users/ci/.local/bin/tartci".to_owned(),
            shipyard_bin: shipyard_bin.map(str::to_owned),
            shipyard_mode: Some("shipyard".to_owned()),
            shipyard_global_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
            shipyard_state_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
            github_cli: Some("/Users/ci/.local/bin/ghapp".to_owned()),
            tart_home: Some("/Users/ci/VMs".to_owned()),
            labels: Vec::new(),
        }
    }

    #[test]
    fn remote_plan_uses_absolute_binary_and_minimal_canonical_path() {
        let plan = host_update_plan(
            &host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard")),
            "v0.98.1",
        )
        .expect("plan");
        assert!(plan.command.starts_with("/usr/bin/env -i HOME=\"$HOME\""));
        assert!(plan.command.contains("/opt/homebrew/bin"));
        assert!(plan.command.contains("/usr/bin/perl -e"));
        assert!(plan.command.contains("TERM"));
        assert!(plan.command.contains("KILL"));
        assert!(plan.command.contains("waitpid"));
        assert!(plan.command.contains(&format!(
            " {} /bin/bash -c",
            REMOTE_UPDATE_TIMEOUT.as_secs()
        )));
        assert!(plan.command.contains("/Users/ci/.local/bin/shipyard"));
        assert!(plan.command.contains("/Users/ci/.local/bin/ghapp"));
        assert!(plan.command.contains("Shipyard/v0.98.1/install.sh"));
        assert!(plan.command.contains("--mode shipyard"));
        assert!(
            plan.command
                .contains("/Users/ci/Library/Application Support/shipyard")
        );
        assert!(
            plan.command
                .contains("update --to v0.98.1 --check --unattended-fleet")
        );
        let preflight = plan
            .command
            .find("staged_binary")
            .expect("staged authenticated preflight");
        let replacement = plan
            .command
            .find("SHIPYARD_INSTALL_DIR=/Users/ci/.local/bin")
            .expect("real install destination");
        assert!(
            preflight < replacement,
            "governed config and helper must pass before binary replacement"
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_plan_preserves_host_class_daemon_context() {
        let mut class = host(None, Some("/Users/ci/.local/bin/shipyard"));
        class.shipyard_mode = Some("isolated".to_owned());
        class.shipyard_global_dir = Some("/tmp/governed config".to_owned());
        class.shipyard_state_dir = Some("/tmp/governed state".to_owned());
        let plan = host_update_plan(&class, "v0.98.1").expect("plan");
        let command = local_update_command(&plan);

        assert!(command.contains("--mode isolated"));
        assert!(command.contains("--global-dir '/tmp/governed config'"));
        assert!(command.contains("--state-dir '/tmp/governed state'"));
        assert!(command.ends_with("--refresh-daemon --unattended-fleet"));
    }

    #[test]
    fn stripped_path_is_launch_environment_drift_not_absence() {
        let error = host_update_plan(&host(Some("m5"), None), "v0.98.1")
            .expect_err("remote relative lookup must fail closed");
        assert!(error.message.contains("launch-environment drift"));
        assert!(!error.message.contains("not installed"));
        assert!(!error.message.contains("absent"));
    }

    #[test]
    fn option_like_ssh_destination_is_rejected_before_spawn() {
        let error = host_update_plan(
            &host(
                Some("-oProxyCommand=/tmp/untrusted"),
                Some("/Users/ci/.local/bin/shipyard"),
            ),
            "v0.98.1",
        )
        .expect_err("SSH option injection");
        assert!(error.message.contains("not a valid SSH destination"));
    }

    #[test]
    fn remote_bootstrap_requires_absolute_governed_auth_helper() {
        let mut config = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
        config.github_cli = Some("ghapp".to_owned());
        let error = host_update_plan(&config, "v0.98.1").expect_err("relative helper");
        assert!(
            error
                .message
                .contains("github_cli must be absolute for bootstrap rollout")
        );
    }

    #[test]
    fn remote_rollout_requires_an_explicit_daemon_context() {
        let mut config = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
        config.shipyard_state_dir = None;
        let error = host_update_plan(&config, "v0.100.0").expect_err("missing context");
        assert!(
            error
                .message
                .contains("shipyard_state_dir is required to identify the daemon context")
        );
    }

    #[test]
    fn remote_bootstrap_rejects_a_filename_the_installer_cannot_replace() {
        let error = host_update_plan(
            &host(Some("m5-lan"), Some("/Users/ci/.local/bin/current")),
            "v0.98.1",
        )
        .expect_err("renamed binary");
        assert!(error.message.contains("must end in /shipyard"));
    }

    #[cfg(unix)]
    #[test]
    fn local_rollout_rejects_a_filename_the_installer_cannot_replace() {
        let error = host_update_plan(&host(None, Some("/Users/ci/.local/bin/current")), "v0.98.1")
            .expect_err("renamed local binary");
        assert!(error.message.contains("must end in /shipyard"));
    }

    #[cfg(unix)]
    #[test]
    fn one_stalled_host_is_terminated_at_its_bound() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let binary = temp.path().join("shipyard");
        std::fs::write(&binary, "#!/bin/sh\nsleep 60\n").expect("fixture");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        let plan = host_update_plan(&host(None, binary.to_str()), "v0.98.1").expect("plan");
        assert!(matches!(
            execute_plan_with_timeout(&plan, Duration::from_millis(50)).expect("execution"),
            PlanExecution::TimedOut
        ));
    }

    #[cfg(unix)]
    #[test]
    fn remote_supervisor_kills_term_ignoring_descendants_after_leader_exits() {
        let temp = tempfile::tempdir().expect("temp dir");
        let pid_file = temp.path().join("descendant.pid");
        let worker = format!(
            "(trap '' TERM; echo $$ > {}; while :; do sleep 1; done) & wait $!",
            shlex_quote(&pid_file.display().to_string())
        );
        let status = Command::new("/usr/bin/perl")
            .args(["-e", REMOTE_SUPERVISOR, "1", "/bin/bash", "-c", &worker])
            .status()
            .expect("remote supervisor fixture");
        assert_eq!(status.code(), Some(124));

        let pid = std::fs::read_to_string(pid_file)
            .expect("descendant pid")
            .trim()
            .to_owned();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline
            && Command::new("/bin/kill")
                .args(["-0", &pid])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !Command::new("/bin/kill")
                .args(["-0", &pid])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success()),
            "TERM-ignoring descendant survived the remote timeout boundary"
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_absolute_tool_survives_a_stripped_non_login_path() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let homebrew_bin = temp.path().join("opt/homebrew/bin");
        std::fs::create_dir_all(&homebrew_bin).expect("homebrew bin");
        let tart = homebrew_bin.join("tart");
        std::fs::write(&tart, "#!/bin/sh\nexit 0\n").expect("tool fixture");
        std::fs::set_permissions(&tart, std::fs::Permissions::from_mode(0o755))
            .expect("executable fixture");
        let shipyard = homebrew_bin.join("shipyard");
        std::fs::write(&shipyard, "#!/bin/sh\nexit 0\n").expect("Shipyard fixture");
        std::fs::set_permissions(&shipyard, std::fs::Permissions::from_mode(0o755))
            .expect("executable Shipyard fixture");

        let hidden = Command::new("/usr/bin/env")
            .args([
                "-i",
                "PATH=/usr/bin:/bin",
                "/bin/sh",
                "-c",
                "command -v tart",
            ])
            .output()
            .expect("stripped-path probe");
        assert!(
            !hidden.status.success(),
            "ambient lookup must miss the fixture"
        );

        let plan = host_update_plan(&host(Some("m5-lan"), shipyard.to_str()), "v0.98.1")
            .expect("an absolute profile path remains authoritative");
        assert_eq!(plan.binary, shipyard);
        assert!(
            plan.command
                .contains(&shlex_quote(&shipyard.display().to_string()))
        );
    }

    #[test]
    fn exact_release_tag_is_required() {
        assert_eq!(normalize_exact_tag("0.100.0").expect("tag"), "v0.100.0");
        assert!(normalize_exact_tag("v0.99.0").is_err());
        assert!(normalize_exact_tag("v0.98.1").is_err());
        assert!(normalize_exact_tag("latest").is_err());
        assert!(normalize_exact_tag("v0.98").is_err());
        assert!(normalize_exact_tag("v0.98.1-rc1").is_err());
        assert!(normalize_exact_tag("v18446744073709551616.0.0").is_err());
    }
}
