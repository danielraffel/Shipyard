use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use super::{
    CliFailure, RuntimeMode, WAIT_EXIT_INVALID, WAIT_EXIT_NO_FALLBACK,
    WAIT_EXIT_RUN_TERMINAL_WRONG, WAIT_EXIT_TIMEOUT, WAIT_EXIT_UNSUPPORTED,
    cli::{WaitCommand, WaitPrState},
};
use crate::config::LoadedConfig;
use crate::output::write_json_envelope;
use crate::wait as wait_logic;
use crate::wait_transport::{
    WaitOutcome, fetch_pr_snapshot, fetch_release_snapshot, fetch_run_snapshot, pr_event_filter,
    read_snapshot_file, release_event_filter, run_event_filter, wait_for_condition,
};

pub(super) fn wait_command<W: Write>(
    command: WaitCommand,
    mode: RuntimeMode,
    daemon_socket: &Path,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        WaitCommand::Release {
            version,
            timeout,
            poll_interval,
            no_fallback,
            repo,
            snapshot_file,
        } => wait_release(
            mode,
            daemon_socket,
            cwd,
            json,
            stdout,
            &version,
            timeout,
            poll_interval,
            no_fallback,
            repo,
            snapshot_file.as_deref(),
        ),
        WaitCommand::Pr {
            pr_number,
            state,
            timeout,
            poll_interval,
            no_fallback,
            repo,
            snapshot_file,
        } => wait_pr(
            daemon_socket,
            cwd,
            json,
            stdout,
            pr_number,
            state,
            timeout,
            poll_interval,
            no_fallback,
            repo,
            snapshot_file.as_deref(),
        ),
        WaitCommand::Run {
            run_id,
            success,
            timeout,
            poll_interval,
            no_fallback,
            repo,
            snapshot_file,
        } => wait_run(
            daemon_socket,
            cwd,
            json,
            stdout,
            &run_id,
            success,
            timeout,
            poll_interval,
            no_fallback,
            repo,
            snapshot_file.as_deref(),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn wait_release<W: Write>(
    mode: RuntimeMode,
    socket_path: &Path,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
    version: &str,
    timeout_seconds: f64,
    poll_interval: f64,
    no_fallback: bool,
    repo_override: Option<String>,
    snapshot_file: Option<&Path>,
) -> Result<ExitCode, CliFailure> {
    let repo = resolve_repo_slug(repo_override, cwd)?;
    let manifest = release_manifest(mode, cwd)?;
    let event_filter = release_event_filter(version, &repo);
    let outcome = wait_for_condition(
        |snapshot| {
            wait_logic::evaluate_release(snapshot, manifest.as_deref())
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
        },
        || match snapshot_file {
            Some(path) => read_snapshot_file(path),
            None => fetch_release_snapshot(&repo, version, cwd),
        },
        event_filter,
        timeout_seconds,
        poll_interval,
        no_fallback,
        socket_path,
    )
    .map_err(|error| wait_failure(error.as_ref()))?;

    render_wait_outcome(
        stdout,
        json,
        "wait:release",
        serde_json::json!({
            "type": "release",
            "repo": repo,
            "tag": version,
            "manifest": manifest,
        }),
        &outcome,
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;

    Ok(wait_exit_code(&outcome))
}

#[allow(clippy::too_many_arguments)]
fn wait_pr<W: Write>(
    socket_path: &Path,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
    pr_number: u64,
    state: WaitPrState,
    timeout_seconds: f64,
    poll_interval: f64,
    no_fallback: bool,
    repo_override: Option<String>,
    snapshot_file: Option<&Path>,
) -> Result<ExitCode, CliFailure> {
    let repo = resolve_repo_slug(repo_override, cwd)?;
    let event_filter = pr_event_filter(pr_number, &repo);
    let outcome = wait_for_condition(
        |snapshot| match state {
            WaitPrState::Green => wait_logic::evaluate_pr_green(snapshot),
            WaitPrState::Merged => wait_logic::evaluate_pr_state(snapshot, "merged")
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>),
            WaitPrState::Closed => wait_logic::evaluate_pr_state(snapshot, "closed")
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>),
        },
        || match snapshot_file {
            Some(path) => read_snapshot_file(path),
            None => fetch_pr_snapshot(&repo, pr_number, cwd),
        },
        event_filter,
        timeout_seconds,
        poll_interval,
        no_fallback,
        socket_path,
    )
    .map_err(|error| wait_failure(error.as_ref()))?;

    render_wait_outcome(
        stdout,
        json,
        "wait:pr",
        serde_json::json!({
            "type": format!("pr_{}", state.as_str()),
            "pr": pr_number,
            "repo": repo,
            "head_sha": outcome.observed.get("head_sha").cloned().unwrap_or(Value::Null),
        }),
        &outcome,
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;

    Ok(wait_exit_code(&outcome))
}

#[allow(clippy::too_many_arguments)]
fn wait_run<W: Write>(
    socket_path: &Path,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
    run_id: &str,
    require_success: bool,
    timeout_seconds: f64,
    poll_interval: f64,
    no_fallback: bool,
    repo_override: Option<String>,
    snapshot_file: Option<&Path>,
) -> Result<ExitCode, CliFailure> {
    let repo = resolve_repo_slug(repo_override, cwd)?;
    let event_filter = run_event_filter(run_id, &repo);
    let condition = serde_json::json!({
        "type": "run",
        "run_id": run_id,
        "repo": repo,
        "require_success": require_success,
    });

    match wait_for_condition(
        |snapshot| wait_logic::evaluate_run(snapshot, require_success),
        || match snapshot_file {
            Some(path) => read_snapshot_file(path),
            None => fetch_run_snapshot(&repo, run_id, cwd),
        },
        event_filter,
        timeout_seconds,
        poll_interval,
        no_fallback,
        socket_path,
    ) {
        Ok(outcome) => {
            render_wait_outcome(stdout, json, "wait:run", condition, &outcome)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            Ok(wait_exit_code(&outcome))
        }
        Err(error) => {
            if let Some(run_failed) = error.downcast_ref::<wait_logic::RunFailedFastError>() {
                let outcome = WaitOutcome {
                    observed: run_failed.observed.clone(),
                    transport: "polling".to_owned(),
                    daemon_unavailable: true,
                    ..WaitOutcome::default()
                };
                render_wait_outcome(stdout, json, "wait:run", condition, &outcome)
                    .map_err(|render_error| CliFailure::new(1, render_error.to_string()))?;
                return Ok(ExitCode::from(WAIT_EXIT_RUN_TERMINAL_WRONG));
            }
            Err(wait_failure(error.as_ref()))
        }
    }
}

fn release_manifest(mode: RuntimeMode, cwd: &Path) -> Result<Option<Vec<String>>, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let Some(value) = config.get("release.artifacts") else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Ok(None);
    };

    let manifest = items
        .iter()
        .filter_map(|item| match item {
            toml::Value::String(name) => Some(name.clone()),
            toml::Value::Table(table) => table
                .get("name")
                .and_then(toml::Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>();

    Ok((!manifest.is_empty()).then_some(manifest))
}

fn resolve_repo_slug(explicit: Option<String>, cwd: &Path) -> Result<String, CliFailure> {
    if let Some(repo) = explicit {
        return Ok(repo);
    }

    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .map_err(|_| CliFailure::new(WAIT_EXIT_INVALID, repo_resolution_error()))?;
    if !output.status.success() {
        return Err(CliFailure::new(WAIT_EXIT_INVALID, repo_resolution_error()));
    }

    let remote = String::from_utf8_lossy(&output.stdout);
    parse_github_repo_slug(&remote)
        .ok_or_else(|| CliFailure::new(WAIT_EXIT_INVALID, repo_resolution_error()))
}

pub(super) fn parse_github_repo_slug(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/');
    let remote = remote.strip_suffix(".git").unwrap_or(remote);

    [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
        "http://github.com/",
    ]
    .iter()
    .find_map(|prefix| remote.strip_prefix(prefix))
    .and_then(|path| {
        let mut parts = path.split('/');
        let owner = parts.next()?;
        let repo = parts.next()?;
        if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
            return None;
        }
        Some(format!("{owner}/{repo}"))
    })
}

fn repo_resolution_error() -> &'static str {
    "couldn't resolve the current repo from the git remote."
}

fn wait_failure(error: &(dyn std::error::Error + 'static)) -> CliFailure {
    if let Some(invalid) = error.downcast_ref::<wait_logic::InvalidInputError>() {
        return CliFailure::new(WAIT_EXIT_INVALID, invalid.to_string());
    }
    if let Some(unsupported) = error.downcast_ref::<wait_logic::UnsupportedScopeError>() {
        return CliFailure::new(WAIT_EXIT_UNSUPPORTED, unsupported.to_string());
    }
    CliFailure::new(1, error.to_string())
}

fn render_wait_outcome<W: Write>(
    stdout: &mut W,
    json: bool,
    command: &str,
    condition: Value,
    outcome: &WaitOutcome,
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("matched".to_owned(), Value::Bool(outcome.matched));
        data.insert("condition".to_owned(), condition);
        data.insert(
            "observed".to_owned(),
            serde_json::to_value(&outcome.observed)?,
        );
        data.insert(
            "transport".to_owned(),
            Value::from(outcome.transport.clone()),
        );
        data.insert(
            "fallback_used".to_owned(),
            Value::Bool(outcome.fallback_used),
        );
        data.insert(
            "events_received".to_owned(),
            Value::from(outcome.events_received),
        );
        data.insert(
            "transient_errors".to_owned(),
            Value::from(outcome.transient_errors),
        );
        data.insert(
            "elapsed_seconds".to_owned(),
            Value::from((outcome.elapsed_seconds * 1000.0).round() / 1000.0),
        );
        write_json_envelope(stdout, command, data)?;
        return Ok(());
    }

    if outcome.matched {
        writeln!(
            stdout,
            "matched after {:.3}s (transport={}, events={})",
            outcome.elapsed_seconds, outcome.transport, outcome.events_received
        )?;
    } else if outcome.timed_out {
        writeln!(
            stdout,
            "timeout after {:.3}s (transport={})",
            outcome.elapsed_seconds, outcome.transport
        )?;
    } else if outcome.fallback_disabled_hit {
        writeln!(
            stdout,
            "daemon unavailable and snapshot didn't match; --no-fallback set"
        )?;
    }

    Ok(())
}

fn wait_exit_code(outcome: &WaitOutcome) -> ExitCode {
    if outcome.matched {
        ExitCode::SUCCESS
    } else if outcome.fallback_disabled_hit {
        ExitCode::from(WAIT_EXIT_NO_FALLBACK)
    } else {
        ExitCode::from(WAIT_EXIT_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::{Command, ExitCode};

    use serde_json::Value;

    use super::{
        RuntimeMode, WaitOutcome, WaitPrState, parse_github_repo_slug, release_manifest,
        render_wait_outcome, resolve_repo_slug, wait_exit_code, wait_failure, wait_pr,
        wait_release, wait_run,
    };
    use crate::app::{
        WAIT_EXIT_INVALID, WAIT_EXIT_NO_FALLBACK, WAIT_EXIT_RUN_TERMINAL_WRONG, WAIT_EXIT_TIMEOUT,
        WAIT_EXIT_UNSUPPORTED,
    };
    use crate::wait as wait_logic;

    #[test]
    fn parse_github_repo_slug_supports_common_remote_forms() {
        assert_eq!(
            parse_github_repo_slug("git@github.com:danielraffel/pulp.git\n"),
            Some("danielraffel/pulp".to_owned())
        );
        assert_eq!(
            parse_github_repo_slug("ssh://git@github.com/danielraffel/Shipyard.git/"),
            Some("danielraffel/Shipyard".to_owned())
        );
        assert_eq!(
            parse_github_repo_slug("https://github.com/owner/repo"),
            Some("owner/repo".to_owned())
        );
        assert_eq!(
            parse_github_repo_slug("https://example.com/owner/repo"),
            None
        );
        assert_eq!(
            parse_github_repo_slug("https://github.com/owner/repo/extra"),
            None
        );
    }

    #[test]
    fn resolve_repo_slug_uses_explicit_or_origin_remote() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            resolve_repo_slug(Some("owner/explicit".to_owned()), temp.path())
                .expect("explicit repo"),
            "owner/explicit"
        );

        git(temp.path(), &["init", "--quiet", "--initial-branch=main"]);
        git(
            temp.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/danielraffel/pulp.git",
            ],
        );

        assert_eq!(
            resolve_repo_slug(None, temp.path()).expect("origin repo"),
            "danielraffel/pulp"
        );
    }

    #[test]
    fn resolve_repo_slug_reports_invalid_context() {
        let temp = tempfile::tempdir().expect("tempdir");

        let err = resolve_repo_slug(None, temp.path()).expect_err("invalid repo context");

        assert_eq!(err.code, WAIT_EXIT_INVALID);
        assert!(err.message.contains("couldn't resolve the current repo"));
    }

    #[test]
    fn release_manifest_reads_string_and_table_artifacts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project).expect("project config dir");
        std::fs::write(
            project.join("config.toml"),
            r#"
[release]
artifacts = [
  "shipyard-linux",
  { name = "shipyard-macos" },
  { other = "ignored" },
  12,
]
"#,
        )
        .expect("config");

        assert_eq!(
            release_manifest(RuntimeMode::Isolated, temp.path()).expect("manifest"),
            Some(vec![
                "shipyard-linux".to_owned(),
                "shipyard-macos".to_owned()
            ])
        );
    }

    #[test]
    fn wait_failure_maps_typed_errors_to_exit_codes() {
        let invalid = wait_logic::InvalidInputError("bad input".to_owned());
        let unsupported = wait_logic::UnsupportedScopeError("rulesets unsupported".to_owned());
        let generic = std::io::Error::other("plain failure");

        let err = wait_failure(&invalid);
        assert_eq!(err.code, WAIT_EXIT_INVALID);
        assert_eq!(err.message, "bad input");

        let err = wait_failure(&unsupported);
        assert_eq!(err.code, WAIT_EXIT_UNSUPPORTED);
        assert_eq!(err.message, "rulesets unsupported");

        let err = wait_failure(&generic);
        assert_eq!(err.code, 1);
        assert_eq!(err.message, "plain failure");
    }

    #[test]
    fn render_wait_outcome_json_rounds_elapsed_and_preserves_observed() {
        let outcome = WaitOutcome {
            matched: true,
            observed: BTreeMap::from([("state".to_owned(), Value::from("MERGED"))]),
            transport: "daemon".to_owned(),
            fallback_used: false,
            events_received: 3,
            transient_errors: 2,
            elapsed_seconds: 1.23456,
            ..WaitOutcome::default()
        };
        let mut out = Vec::new();

        render_wait_outcome(
            &mut out,
            true,
            "wait:pr",
            serde_json::json!({"type": "pr_merged", "pr": 42}),
            &outcome,
        )
        .expect("render");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(payload["command"], "wait:pr");
        assert_eq!(payload["matched"], true);
        assert_eq!(payload["condition"]["pr"], 42);
        assert_eq!(payload["observed"]["state"], "MERGED");
        assert_eq!(payload["transport"], "daemon");
        assert_eq!(payload["events_received"], 3);
        assert_eq!(payload["transient_errors"], 2);
        assert_eq!(payload["elapsed_seconds"], 1.235);
    }

    #[test]
    fn render_wait_outcome_human_contracts_cover_terminal_states() {
        let mut matched = Vec::new();
        render_wait_outcome(
            &mut matched,
            false,
            "wait:run",
            serde_json::json!({}),
            &WaitOutcome {
                matched: true,
                transport: "polling".to_owned(),
                events_received: 2,
                elapsed_seconds: 0.5,
                ..WaitOutcome::default()
            },
        )
        .expect("matched render");
        assert_eq!(
            String::from_utf8(matched).expect("utf8"),
            "matched after 0.500s (transport=polling, events=2)\n"
        );

        let mut timeout = Vec::new();
        render_wait_outcome(
            &mut timeout,
            false,
            "wait:run",
            serde_json::json!({}),
            &WaitOutcome {
                timed_out: true,
                transport: "polling".to_owned(),
                elapsed_seconds: 3.0,
                ..WaitOutcome::default()
            },
        )
        .expect("timeout render");
        assert_eq!(
            String::from_utf8(timeout).expect("utf8"),
            "timeout after 3.000s (transport=polling)\n"
        );

        let mut no_fallback = Vec::new();
        render_wait_outcome(
            &mut no_fallback,
            false,
            "wait:run",
            serde_json::json!({}),
            &WaitOutcome {
                fallback_disabled_hit: true,
                ..WaitOutcome::default()
            },
        )
        .expect("no fallback render");
        assert_eq!(
            String::from_utf8(no_fallback).expect("utf8"),
            "daemon unavailable and snapshot didn't match; --no-fallback set\n"
        );
    }

    #[test]
    fn wait_exit_code_matches_timeout_and_no_fallback_contracts() {
        assert_eq!(
            wait_exit_code(&WaitOutcome {
                matched: true,
                ..WaitOutcome::default()
            }),
            ExitCode::SUCCESS
        );
        assert_eq!(
            wait_exit_code(&WaitOutcome {
                fallback_disabled_hit: true,
                ..WaitOutcome::default()
            }),
            ExitCode::from(WAIT_EXIT_NO_FALLBACK)
        );
        assert_eq!(
            wait_exit_code(&WaitOutcome::default()),
            ExitCode::from(WAIT_EXIT_TIMEOUT)
        );
    }

    #[test]
    fn wait_release_matches_snapshot_file_and_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project).expect("project config dir");
        std::fs::write(
            project.join("config.toml"),
            "[release]\nartifacts = [\"shipyard-linux\", { name = \"shipyard-macos\" }]\n",
        )
        .expect("config");
        let snapshot = temp.path().join("release.json");
        std::fs::write(
            &snapshot,
            serde_json::json!({
                "draft": false,
                "assets": [
                    {"name": "shipyard-linux", "state": "uploaded", "size": 10},
                    {"name": "shipyard-macos", "state": "uploaded", "size": 20}
                ]
            })
            .to_string(),
        )
        .expect("snapshot");
        let mut out = Vec::new();

        let code = wait_release(
            RuntimeMode::Isolated,
            &temp.path().join("missing.sock"),
            temp.path(),
            true,
            &mut out,
            "v1.0.0",
            0.01,
            0.01,
            false,
            Some("owner/repo".to_owned()),
            Some(&snapshot),
        )
        .expect("wait release");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(payload["command"], "wait:release");
        assert_eq!(payload["matched"], true);
        assert_eq!(payload["condition"]["tag"], "v1.0.0");
        assert_eq!(payload["condition"]["manifest"][1], "shipyard-macos");
    }

    #[test]
    fn wait_pr_matches_closed_snapshot_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            serde_json::json!({
                "number": 42,
                "state": "CLOSED",
                "merged": false,
                "headRefOid": "abc"
            })
            .to_string(),
        )
        .expect("snapshot");
        let mut out = Vec::new();

        let code = wait_pr(
            &temp.path().join("missing.sock"),
            temp.path(),
            false,
            &mut out,
            42,
            WaitPrState::Closed,
            0.01,
            0.01,
            false,
            Some("owner/repo".to_owned()),
            Some(&snapshot),
        )
        .expect("wait pr");

        assert_eq!(code, ExitCode::SUCCESS);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.starts_with("matched after "));
        assert!(text.contains("(transport=polling, events=0)"));
    }

    #[test]
    fn wait_run_success_failure_fast_returns_terminal_wrong_exit_code() {
        let temp = tempfile::tempdir().expect("tempdir");
        let snapshot = temp.path().join("run.json");
        std::fs::write(
            &snapshot,
            serde_json::json!({
                "databaseId": 100,
                "status": "completed",
                "conclusion": "failure"
            })
            .to_string(),
        )
        .expect("snapshot");
        let mut out = Vec::new();

        let code = wait_run(
            &temp.path().join("missing.sock"),
            temp.path(),
            true,
            &mut out,
            "100",
            true,
            0.01,
            0.01,
            false,
            Some("owner/repo".to_owned()),
            Some(&snapshot),
        )
        .expect("wait run");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(code, ExitCode::from(WAIT_EXIT_RUN_TERMINAL_WRONG));
        assert_eq!(payload["command"], "wait:run");
        assert_eq!(payload["matched"], false);
        assert_eq!(payload["observed"]["run_id"], 100);
        assert_eq!(payload["observed"]["conclusion"], "failure");
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("git should run");
        assert!(
            status.success(),
            "git failed in {}: {args:?}",
            cwd.display()
        );
    }
}
