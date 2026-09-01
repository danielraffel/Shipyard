use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{
    CliFailure, RuntimeMode, WAIT_EXIT_INVALID, WAIT_EXIT_NO_FALLBACK, WAIT_EXIT_TERMINAL_WRONG,
    WAIT_EXIT_TIMEOUT, WAIT_EXIT_UNSUPPORTED,
    cli::{WaitCommand, WaitPrState},
};
use crate::config::LoadedConfig;
use crate::log_retention::{TerminalLogManifest, read_terminal_manifest};
use crate::output::write_json_envelope;
use crate::queue::Queue;
use crate::wait as wait_logic;
use crate::wait_transport::{
    WaitOutcome, fetch_pr_green_snapshot_with_timeout, fetch_pr_snapshot_with_timeout,
    fetch_release_snapshot_with_timeout, fetch_run_snapshot_with_timeout, pr_event_filter,
    read_snapshot_file, release_event_filter, run_event_filter, wait_for_condition_with_timeout,
};

pub(super) fn wait_command<W: Write>(
    command: WaitCommand,
    mode: RuntimeMode,
    daemon_socket: &Path,
    state_dir: &Path,
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
        WaitCommand::Job {
            job_id,
            success,
            timeout,
            poll_interval,
        } => wait_job(
            state_dir,
            json,
            stdout,
            &job_id,
            success,
            timeout,
            poll_interval,
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
    let outcome = wait_for_condition_with_timeout(
        |snapshot| {
            wait_logic::evaluate_release(snapshot, manifest.as_deref())
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
        },
        |remaining| match snapshot_file {
            Some(path) => read_snapshot_file(path),
            None => fetch_release_snapshot_with_timeout(&repo, version, cwd, remaining),
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
    let mut terminal_wrong = false;
    let result = wait_for_condition_with_timeout(
        |snapshot| match state {
            WaitPrState::Green => evaluate_pr_green_for_wait(snapshot, &mut terminal_wrong),
            WaitPrState::Merged => wait_logic::evaluate_pr_state(snapshot, "merged")
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>),
            WaitPrState::Closed => wait_logic::evaluate_pr_state(snapshot, "closed")
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error>),
        },
        |remaining| match snapshot_file {
            Some(path) => read_snapshot_file(path),
            None => match state {
                WaitPrState::Green => {
                    fetch_pr_green_snapshot_with_timeout(&repo, pr_number, cwd, remaining)
                }
                WaitPrState::Merged | WaitPrState::Closed => {
                    fetch_pr_snapshot_with_timeout(&repo, pr_number, cwd, remaining)
                }
            },
        },
        event_filter,
        timeout_seconds,
        poll_interval,
        no_fallback,
        socket_path,
    );
    match result {
        Ok(mut outcome) => {
            if terminal_wrong {
                outcome.matched = false;
                if !json {
                    render_pr_terminal_failure(stdout, &outcome)
                        .map_err(|error| CliFailure::new(1, error.to_string()))?;
                    return Ok(ExitCode::from(WAIT_EXIT_TERMINAL_WRONG));
                }
            }
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
            if terminal_wrong {
                return Ok(ExitCode::from(WAIT_EXIT_TERMINAL_WRONG));
            }
            Ok(wait_exit_code(&outcome))
        }
        Err(error) => Err(wait_failure(error.as_ref())),
    }
}

fn render_pr_terminal_failure<W: Write>(
    stdout: &mut W,
    outcome: &WaitOutcome,
) -> std::io::Result<()> {
    let head = outcome
        .observed
        .get("head_sha")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let failures = outcome
        .observed
        .get("checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|check| {
            ["conclusion", "state"].iter().any(|field| {
                check
                    .get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|value| wait_logic::TERMINAL_FAILURE_CONCLUSIONS.contains(&value))
            })
        })
        .filter_map(|check| check.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    writeln!(
        stdout,
        "required checks failed for head {head}: {} after {:.3}s (transport={})",
        failures.join(", "),
        outcome.elapsed_seconds,
        outcome.transport
    )
}

fn evaluate_pr_green_for_wait(
    snapshot: Option<&Value>,
    terminal_wrong: &mut bool,
) -> crate::wait_transport::WaitResult<crate::wait::TruthResult> {
    match wait_logic::evaluate_pr_green(snapshot) {
        Ok(result) => Ok(result),
        Err(error) => match error.downcast::<wait_logic::PrFailedFastError>() {
            Ok(pr_failed) => {
                *terminal_wrong = true;
                Ok(crate::wait::TruthResult {
                    matched: true,
                    observed: pr_failed.observed,
                })
            }
            Err(error) => Err(error),
        },
    }
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
    if run_id.starts_with("sy-") {
        return Err(CliFailure::new(
            WAIT_EXIT_INVALID,
            format!(
                "{run_id} is a Shipyard queue job ID; use `shipyard wait job {run_id}` instead"
            ),
        ));
    }
    let repo = resolve_repo_slug(repo_override, cwd)?;
    let event_filter = run_event_filter(run_id, &repo);
    let condition = serde_json::json!({
        "type": "run",
        "run_id": run_id,
        "repo": repo,
        "require_success": require_success,
    });

    let mut terminal_wrong = false;
    let result = wait_for_condition_with_timeout(
        |snapshot| evaluate_run_for_wait(snapshot, require_success, &mut terminal_wrong),
        |remaining| match snapshot_file {
            Some(path) => read_snapshot_file(path),
            None => fetch_run_snapshot_with_timeout(&repo, run_id, cwd, remaining),
        },
        event_filter,
        timeout_seconds,
        poll_interval,
        no_fallback,
        socket_path,
    );
    match result {
        Ok(mut outcome) => {
            if terminal_wrong {
                outcome.matched = false;
            }
            render_wait_outcome(stdout, json, "wait:run", condition, &outcome)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            if terminal_wrong {
                return Ok(ExitCode::from(WAIT_EXIT_TERMINAL_WRONG));
            }
            Ok(wait_exit_code(&outcome))
        }
        Err(error) => Err(wait_failure(error.as_ref())),
    }
}

#[allow(clippy::too_many_arguments)]
fn wait_job<W: Write>(
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
    job_id: &str,
    require_success: bool,
    timeout_seconds: f64,
    poll_interval_seconds: f64,
) -> Result<ExitCode, CliFailure> {
    if !is_queue_job_id(job_id) {
        return Err(CliFailure::new(
            WAIT_EXIT_INVALID,
            "job id must be one sy-* path component",
        ));
    }
    if !timeout_seconds.is_finite()
        || timeout_seconds < 0.0
        || !poll_interval_seconds.is_finite()
        || poll_interval_seconds <= 0.0
    {
        return Err(CliFailure::new(
            WAIT_EXIT_INVALID,
            "timeout must be finite and non-negative; poll interval must be finite and positive",
        ));
    }
    let timeout = Duration::try_from_secs_f64(timeout_seconds)
        .map_err(|error| CliFailure::new(WAIT_EXIT_INVALID, error.to_string()))?;
    let poll_interval = Duration::try_from_secs_f64(poll_interval_seconds)
        .map_err(|error| CliFailure::new(WAIT_EXIT_INVALID, error.to_string()))?;
    let mut queue = Queue::new(state_dir).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let start = Instant::now();
    let Some(mut job) = queue
        .get(job_id)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
    else {
        return render_absent_job(
            stdout,
            json,
            state_dir,
            job_id,
            require_success,
            start.elapsed(),
        );
    };
    loop {
        let terminal = matches!(
            job.status,
            crate::job::JobStatus::Completed | crate::job::JobStatus::Cancelled
        );
        if terminal {
            let passed = job.passed();
            return render_job_wait(
                stdout,
                json,
                job_id,
                require_success,
                Some(&job),
                !require_success || passed,
                false,
                start.elapsed(),
            );
        }
        if start.elapsed() >= timeout {
            return render_job_wait(
                stdout,
                json,
                job_id,
                require_success,
                Some(&job),
                false,
                true,
                start.elapsed(),
            );
        }
        thread::sleep(poll_interval.min(timeout.saturating_sub(start.elapsed())));
        if start.elapsed() >= timeout {
            return render_job_wait(
                stdout,
                json,
                job_id,
                require_success,
                Some(&job),
                false,
                true,
                start.elapsed(),
            );
        }
        let Some(next) = queue
            .get(job_id)
            .map_err(|error| CliFailure::new(1, error.to_string()))?
        else {
            return render_absent_job(
                stdout,
                json,
                state_dir,
                job_id,
                require_success,
                start.elapsed(),
            );
        };
        job = next;
    }
}

#[allow(clippy::too_many_arguments)]
fn render_absent_job<W: Write>(
    stdout: &mut W,
    json: bool,
    state_dir: &Path,
    job_id: &str,
    require_success: bool,
    elapsed: Duration,
) -> Result<ExitCode, CliFailure> {
    if let Some(manifest) = read_terminal_manifest(&state_dir.join("logs").join(job_id))
        .filter(|manifest| manifest.job_id == job_id)
    {
        return render_retained_job_wait(
            stdout,
            json,
            job_id,
            require_success,
            &manifest,
            false,
            elapsed,
        );
    }
    render_job_wait(
        stdout,
        json,
        job_id,
        require_success,
        None,
        false,
        false,
        elapsed,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_retained_job_wait<W: Write>(
    stdout: &mut W,
    json: bool,
    job_id: &str,
    require_success: bool,
    manifest: &TerminalLogManifest,
    timed_out: bool,
    elapsed: Duration,
) -> Result<ExitCode, CliFailure> {
    let passed = !manifest.failed;
    let outcome = WaitOutcome {
        matched: !timed_out && (!require_success || passed),
        observed: BTreeMap::from([
            ("job_id".to_owned(), Value::String(job_id.to_owned())),
            (
                "status".to_owned(),
                Value::String(
                    if manifest.reason == "cancelled" {
                        "cancelled"
                    } else {
                        "completed"
                    }
                    .to_owned(),
                ),
            ),
            ("terminal".to_owned(), Value::Bool(true)),
            ("passed".to_owned(), Value::Bool(passed)),
        ]),
        transport: "queue".to_owned(),
        timed_out,
        elapsed_seconds: elapsed.as_secs_f64(),
        ..WaitOutcome::default()
    };
    render_wait_outcome(
        stdout,
        json,
        "wait:job",
        job_wait_condition(job_id, require_success),
        &outcome,
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(if timed_out {
        ExitCode::from(WAIT_EXIT_TIMEOUT)
    } else if require_success && !passed {
        ExitCode::from(WAIT_EXIT_TERMINAL_WRONG)
    } else {
        ExitCode::SUCCESS
    })
}

#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn render_job_wait<W: Write>(
    stdout: &mut W,
    json: bool,
    job_id: &str,
    require_success: bool,
    job: Option<&crate::job::Job>,
    matched: bool,
    timed_out: bool,
    elapsed: Duration,
) -> Result<ExitCode, CliFailure> {
    let terminal = job.is_some_and(|job| {
        matches!(
            job.status,
            crate::job::JobStatus::Completed | crate::job::JobStatus::Cancelled
        )
    });
    let outcome = WaitOutcome {
        matched,
        observed: BTreeMap::from([
            ("job_id".to_owned(), Value::String(job_id.to_owned())),
            (
                "status".to_owned(),
                job.map_or(Value::String("not_found".to_owned()), |job| {
                    serde_json::to_value(job.status).expect("job status serializes")
                }),
            ),
            (
                "terminal".to_owned(),
                job.map_or(Value::Null, |_| Value::Bool(terminal)),
            ),
            (
                "passed".to_owned(),
                job.filter(|_| terminal)
                    .map_or(Value::Null, |job| Value::Bool(job.passed())),
            ),
        ]),
        transport: "queue".to_owned(),
        timed_out,
        elapsed_seconds: elapsed.as_secs_f64(),
        ..WaitOutcome::default()
    };
    render_wait_outcome(
        stdout,
        json,
        "wait:job",
        job_wait_condition(job_id, require_success),
        &outcome,
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(if job.is_none() {
        ExitCode::from(WAIT_EXIT_INVALID)
    } else if timed_out {
        ExitCode::from(WAIT_EXIT_TIMEOUT)
    } else if require_success && !matched {
        ExitCode::from(WAIT_EXIT_TERMINAL_WRONG)
    } else {
        ExitCode::SUCCESS
    })
}

fn is_queue_job_id(job_id: &str) -> bool {
    let mut components = Path::new(job_id).components();
    job_id.starts_with("sy-")
        && matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

fn job_wait_condition(job_id: &str, require_success: bool) -> Value {
    serde_json::json!({
        "type": "job",
        "job_id": job_id,
        "require_success": require_success,
    })
}

fn evaluate_run_for_wait(
    snapshot: Option<&Value>,
    require_success: bool,
    terminal_wrong: &mut bool,
) -> crate::wait_transport::WaitResult<crate::wait::TruthResult> {
    match wait_logic::evaluate_run(snapshot, require_success) {
        Ok(result) => Ok(result),
        Err(error) => match error.downcast::<wait_logic::RunFailedFastError>() {
            Ok(run_failed) => {
                *terminal_wrong = true;
                Ok(crate::wait::TruthResult {
                    matched: true,
                    observed: run_failed.observed,
                })
            }
            Err(error) => Err(error),
        },
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
    use std::thread;
    use std::time::Duration;

    use serde_json::Value;

    use super::{
        RuntimeMode, WaitOutcome, WaitPrState, evaluate_pr_green_for_wait, evaluate_run_for_wait,
        parse_github_repo_slug, release_manifest, render_wait_outcome, resolve_repo_slug,
        wait_exit_code, wait_failure, wait_job, wait_pr, wait_release, wait_run,
    };
    use crate::app::{
        WAIT_EXIT_INVALID, WAIT_EXIT_NO_FALLBACK, WAIT_EXIT_TERMINAL_WRONG, WAIT_EXIT_TIMEOUT,
        WAIT_EXIT_UNSUPPORTED,
    };
    use crate::gh::GhPrepareError;
    use crate::job::{Job, Priority, TargetResult, TargetStatus, ValidationMode};
    use crate::queue::{KEEP_COMPLETED, Queue};
    use crate::wait as wait_logic;
    use crate::wait_transport::wait_for_condition_with_timeout;

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
    fn wait_pr_green_terminal_failure_fast_returns_terminal_wrong_exit_code() {
        let temp = tempfile::tempdir().expect("tempdir");
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            serde_json::json!({
                "number": 534,
                "headRefOid": "terminal-red-head",
                "mergeable": "MERGEABLE",
                "mergeStateStatus": "UNSTABLE",
                "statusCheckRollup": [
                    {"name": "Linux", "conclusion": "SUCCESS", "state": "COMPLETED", "isRequired": true},
                    {"name": "macOS", "conclusion": "FAILURE", "state": "COMPLETED", "isRequired": true}
                ],
                "_required_checks_known": true
            })
            .to_string(),
        )
        .expect("snapshot");
        let mut out = Vec::new();

        let code = wait_pr(
            &temp.path().join("missing.sock"),
            temp.path(),
            true,
            &mut out,
            534,
            WaitPrState::Green,
            10.0,
            10.0,
            false,
            Some("owner/repo".to_owned()),
            Some(&snapshot),
        )
        .expect("terminal failure is an observed outcome");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(code, ExitCode::from(WAIT_EXIT_TERMINAL_WRONG));
        assert_eq!(payload["matched"], false);
        assert_eq!(payload["condition"]["head_sha"], "terminal-red-head");
        assert_eq!(payload["observed"]["checks"][1]["conclusion"], "FAILURE");
        assert!(payload["elapsed_seconds"].as_f64().expect("elapsed") < 1.0);
    }

    #[test]
    fn wait_pr_green_terminal_failure_names_head_and_checks_for_humans() {
        let temp = tempfile::tempdir().expect("tempdir");
        let snapshot = temp.path().join("pr.json");
        std::fs::write(
            &snapshot,
            serde_json::json!({
                "number": 534,
                "headRefOid": "terminal-red-head",
                "mergeable": "MERGEABLE",
                "mergeStateStatus": "UNSTABLE",
                "statusCheckRollup": [
                    {"name": "Linux", "conclusion": "SUCCESS", "state": "COMPLETED", "isRequired": true},
                    {"name": "macOS", "conclusion": "FAILURE", "state": "COMPLETED", "isRequired": true}
                ],
                "_required_checks_known": true
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
            534,
            WaitPrState::Green,
            10.0,
            10.0,
            false,
            Some("owner/repo".to_owned()),
            Some(&snapshot),
        )
        .expect("terminal failure is an observed outcome");

        assert_eq!(code, ExitCode::from(WAIT_EXIT_TERMINAL_WRONG));
        let output = String::from_utf8(out).expect("utf8");
        assert!(
            output.starts_with("required checks failed for head terminal-red-head: macOS after ")
        );
        assert!(output.ends_with(" (transport=polling)\n"));
    }

    #[test]
    fn wait_pr_green_follows_head_movement_without_stale_terminal_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let snapshots = [
            serde_json::json!({
                "number": 534,
                "headRefOid": "old-head",
                "mergeable": "MERGEABLE",
                "mergeStateStatus": "UNSTABLE",
                "statusCheckRollup": [
                    {"name": "macOS", "conclusion": null, "state": "IN_PROGRESS", "isRequired": true}
                ],
                "_required_checks_known": true
            }),
            serde_json::json!({
                "number": 534,
                "headRefOid": "new-head",
                "mergeable": "MERGEABLE",
                "mergeStateStatus": "CLEAN",
                "statusCheckRollup": [
                    {"name": "macOS", "conclusion": "SUCCESS", "state": "COMPLETED", "isRequired": true}
                ],
                "_required_checks_known": true
            }),
        ];
        let mut calls = 0;
        let mut terminal_wrong = false;
        let outcome = wait_for_condition_with_timeout(
            |snapshot| evaluate_pr_green_for_wait(snapshot, &mut terminal_wrong),
            |_| {
                let snapshot = snapshots[calls.min(snapshots.len() - 1)].clone();
                calls += 1;
                Ok(Some(snapshot))
            },
            |_| true,
            1.0,
            0.01,
            false,
            &temp.path().join("missing.sock"),
        )
        .expect("new head should reach green");

        assert!(!terminal_wrong);
        assert!(outcome.matched);
        assert_eq!(outcome.observed["head_sha"], "new-head");
        assert!(calls >= 2);
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
        assert_eq!(code, ExitCode::from(WAIT_EXIT_TERMINAL_WRONG));
        assert_eq!(payload["command"], "wait:run");
        assert_eq!(payload["matched"], false);
        assert_eq!(payload["observed"]["run_id"], 100);
        assert_eq!(payload["observed"]["conclusion"], "failure");
    }

    #[test]
    fn wait_run_rejects_shipyard_job_id_before_repository_or_github_lookup() {
        let temp = tempfile::tempdir().expect("tempdir");
        let error = wait_run(
            &temp.path().join("missing.sock"),
            temp.path(),
            true,
            &mut Vec::new(),
            "sy-20260901-example",
            false,
            0.01,
            0.01,
            false,
            None,
            None,
        )
        .expect_err("queue job ID must not reach GitHub resolution");

        assert_eq!(error.code, WAIT_EXIT_INVALID);
        assert!(error.message.contains("wait job sy-20260901-example"));
    }

    #[test]
    fn wait_job_pending_timeout_is_typed_unknown_not_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let job = queue.enqueue(test_job()).expect("enqueue");
        let mut out = Vec::new();

        let code = wait_job(temp.path(), true, &mut out, &job.id, true, 0.0, 0.01)
            .expect("pending timeout");
        let payload: Value = serde_json::from_slice(&out).expect("json");

        assert_eq!(code, ExitCode::from(WAIT_EXIT_TIMEOUT));
        assert_eq!(payload["command"], "wait:job");
        assert_eq!(payload["transport"], "queue");
        assert_eq!(payload["matched"], false);
        assert_eq!(payload["observed"]["status"], "pending");
        assert_eq!(payload["observed"]["terminal"], false);
        assert!(payload["observed"]["passed"].is_null());
    }

    #[test]
    fn wait_job_rejects_nonfinite_and_zero_poll_durations_without_panicking() {
        let temp = tempfile::tempdir().expect("tempdir");
        for (timeout, poll_interval) in [
            (f64::INFINITY, 1.0),
            (1.0, f64::INFINITY),
            (1e300, 1.0),
            (1.0, 1e300),
            (1.0, 0.0),
            (-1.0, 1.0),
        ] {
            let error = wait_job(
                temp.path(),
                true,
                &mut Vec::new(),
                "sy-duration-control",
                true,
                timeout,
                poll_interval,
            )
            .expect_err("invalid duration must refuse");
            assert_eq!(error.code, WAIT_EXIT_INVALID);
        }
    }

    #[test]
    fn wait_job_rejects_path_shaped_ids_before_queue_or_manifest_lookup() {
        let temp = tempfile::tempdir().expect("tempdir");
        let error = wait_job(
            temp.path(),
            true,
            &mut Vec::new(),
            "sy-/../../other-job",
            true,
            1.0,
            0.01,
        )
        .expect_err("invalid ID must refuse");
        assert_eq!(error.code, WAIT_EXIT_INVALID);
        assert!(!temp.path().join("queue.json").exists());
    }

    #[test]
    fn wait_job_terminal_success_and_failure_obey_success_requirement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let passed = terminal_job(&mut queue, TargetStatus::Pass);
        let failed = terminal_job(&mut queue, TargetStatus::Fail);

        let mut passed_out = Vec::new();
        assert_eq!(
            wait_job(
                temp.path(),
                true,
                &mut passed_out,
                &passed.id,
                true,
                1.0,
                0.01,
            )
            .expect("passed"),
            ExitCode::SUCCESS
        );
        let passed_payload: Value = serde_json::from_slice(&passed_out).expect("passed json");
        assert_eq!(passed_payload["matched"], true);
        assert_eq!(passed_payload["observed"]["passed"], true);

        let mut failed_out = Vec::new();
        assert_eq!(
            wait_job(
                temp.path(),
                true,
                &mut failed_out,
                &failed.id,
                true,
                1.0,
                0.01,
            )
            .expect("failed"),
            ExitCode::from(WAIT_EXIT_TERMINAL_WRONG)
        );
        let failed_payload: Value = serde_json::from_slice(&failed_out).expect("failed json");
        assert_eq!(failed_payload["matched"], false);
        assert_eq!(failed_payload["observed"]["status"], "completed");
        assert_eq!(failed_payload["observed"]["passed"], false);

        assert_eq!(
            wait_job(
                temp.path(),
                true,
                &mut Vec::new(),
                &failed.id,
                false,
                1.0,
                0.01,
            )
            .expect("terminal without success requirement"),
            ExitCode::SUCCESS
        );
    }

    #[test]
    fn wait_job_cancelled_and_missing_states_never_imply_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let pending = queue.enqueue(test_job()).expect("enqueue");
        let cancelled = pending.cancel().expect("cancel");
        queue.update(&cancelled).expect("update");

        assert_eq!(
            wait_job(
                temp.path(),
                true,
                &mut Vec::new(),
                &cancelled.id,
                false,
                1.0,
                0.01,
            )
            .expect("cancelled terminal"),
            ExitCode::SUCCESS
        );
        assert_eq!(
            wait_job(
                temp.path(),
                true,
                &mut Vec::new(),
                &cancelled.id,
                true,
                1.0,
                0.01,
            )
            .expect("cancelled is not success"),
            ExitCode::from(WAIT_EXIT_TERMINAL_WRONG)
        );

        let mut missing_out = Vec::new();
        assert_eq!(
            wait_job(
                temp.path(),
                true,
                &mut missing_out,
                "sy-missing",
                true,
                1.0,
                0.01,
            )
            .expect("missing is typed"),
            ExitCode::from(WAIT_EXIT_INVALID)
        );
        let missing: Value = serde_json::from_slice(&missing_out).expect("missing json");
        assert_eq!(missing["matched"], false);
        assert_eq!(missing["observed"]["status"], "not_found");
        assert!(missing["observed"]["terminal"].is_null());
        assert!(missing["observed"]["passed"].is_null());
    }

    #[test]
    fn wait_job_observes_durable_pending_to_completed_transition() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let pending = queue.enqueue(test_job()).expect("enqueue");
        let state_dir = temp.path().to_path_buf();
        let job_id = pending.id.clone();
        let updater = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            let mut queue = Queue::new(&state_dir).expect("updater queue");
            let pending = queue.get(&job_id).expect("get").expect("job");
            let running = pending
                .start()
                .expect("start")
                .with_result(TargetResult::new(
                    "linux",
                    "linux",
                    TargetStatus::Pass,
                    "local",
                ));
            queue.update(&running).expect("running");
            queue
                .update(&running.complete().expect("complete"))
                .expect("completed");
        });

        let mut out = Vec::new();
        let code = wait_job(temp.path(), true, &mut out, &pending.id, true, 1.0, 0.01)
            .expect("transition observed");
        updater.join().expect("updater");

        assert_eq!(code, ExitCode::SUCCESS);
        let payload: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(payload["observed"]["status"], "completed");
        assert_eq!(payload["observed"]["passed"], true);
    }

    #[test]
    fn wait_job_observes_one_initial_terminal_snapshot_at_zero_timeout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let pending = queue.enqueue(test_job()).expect("enqueue");
        let running = pending
            .start()
            .expect("start")
            .with_result(TargetResult::new(
                "linux",
                "linux",
                TargetStatus::Pass,
                "local",
            ));
        queue.update(&running).expect("running");
        let completed = running.complete().expect("complete");
        queue.update(&completed).expect("complete");

        let mut out = Vec::new();
        let code = wait_job(temp.path(), true, &mut out, &completed.id, true, 0.0, 0.01)
            .expect("terminal");
        let payload: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(payload["matched"], true);
    }

    #[test]
    fn wait_job_reads_cancelled_no_log_manifest_after_queue_eviction() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let pending = queue.enqueue(test_job()).expect("enqueue");
        let cancelled = pending.cancel().expect("cancel");
        queue.update(&cancelled).expect("cancelled");

        for index in 0..=KEEP_COMPLETED {
            let mut job = test_job();
            job.sha = format!("newer-{index}");
            let pending = queue.enqueue(job).expect("enqueue newer");
            let running = pending
                .start()
                .expect("start")
                .with_result(TargetResult::new(
                    "linux",
                    "linux",
                    TargetStatus::Pass,
                    "local",
                ));
            queue.update(&running).expect("running");
            queue
                .update(&running.complete().expect("complete"))
                .expect("completed");
        }
        assert!(queue.get(&cancelled.id).expect("get").is_none());

        let mut out = Vec::new();
        let code = wait_job(temp.path(), true, &mut out, &cancelled.id, false, 1.0, 0.01)
            .expect("retained terminal");
        let payload: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(payload["observed"]["status"], "cancelled");
        assert_eq!(payload["observed"]["passed"], false);
    }

    fn test_job() -> Job {
        Job::create(
            "abc123",
            "feature/wait-job",
            vec!["linux".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        )
    }

    fn terminal_job(queue: &mut Queue, status: TargetStatus) -> Job {
        let pending = queue.enqueue(test_job()).expect("enqueue");
        let running = pending
            .start()
            .expect("start")
            .with_result(TargetResult::new("linux", "linux", status, "local"));
        queue.update(&running).expect("running");
        let completed = running.complete().expect("complete");
        queue.update(&completed).expect("completed");
        completed
    }

    #[test]
    fn wait_run_failure_fast_preserves_transient_snapshot_count() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut calls = 0;
        let mut terminal_wrong = false;
        let mut outcome = wait_for_condition_with_timeout(
            |snapshot| evaluate_run_for_wait(snapshot, true, &mut terminal_wrong),
            |_| {
                calls += 1;
                if calls == 1 {
                    return Err(Box::new(GhPrepareError::HelperFailed {
                        program: "helper".to_owned(),
                        status: Some(1),
                        stderr: "service unavailable".to_owned(),
                    }) as Box<dyn std::error::Error>);
                }
                Ok(Some(serde_json::json!({
                    "databaseId": 100,
                    "status": "completed",
                    "conclusion": "failure"
                })))
            },
            |_| true,
            1.0,
            0.01,
            false,
            &temp.path().join("missing.sock"),
        )
        .expect("terminal failure should stop the wait");

        assert!(terminal_wrong);
        assert!(outcome.matched);
        assert_eq!(outcome.transient_errors, 1);
        assert_eq!(outcome.observed["conclusion"], "failure");

        outcome.matched = false;
        assert!(!outcome.matched);
        assert!(!outcome.timed_out);
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
