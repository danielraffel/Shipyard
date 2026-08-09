use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use chrono::Utc;
use regex::Regex;
use serde_json::Value;

use super::CliFailure;
use super::cli::WatchLocalArgs;
use crate::config::LoadedConfig;
use crate::executor::dispatch::{ResolvedBackend, ResolvedTarget, resolve_targets};
use crate::executor::ssh::{shlex_quote, ssh_options};
use crate::executor::streaming::{
    StreamLineAction, StreamingCommand, StreamingCommandSpec, run_streaming_command,
};
use crate::job::ValidationMode;
use crate::output::write_json_envelope;

pub(super) fn watch_local_command<W: Write>(
    args: &WatchLocalArgs,
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let target = resolve_watch_target(config, &args.target)?;
    let milestone_patterns = compile_patterns("milestone", &args.milestone_regex)?;
    let terminal_patterns = compile_patterns("terminal", &args.terminal_regex)?;
    let log_path = args
        .log_path
        .clone()
        .unwrap_or_else(|| default_log_path(state_dir, &args.target));
    let timeout = args.timeout_secs.map(Duration::from_secs);
    let command_label = args.command.clone();
    let backend_label = target.backend_name.clone();

    let request = streaming_request(&target, args, cwd, &log_path, timeout)?;
    let context = WatchEventContext {
        json,
        target: &target.name,
        backend: &backend_label,
        command: &command_label,
        log_path: &log_path,
        milestone_patterns: &milestone_patterns,
        terminal_patterns: &terminal_patterns,
    };
    run_stream_with_events(request, &context, stdout)
}

fn resolve_watch_target(
    config: &LoadedConfig,
    requested: &str,
) -> Result<ResolvedTarget, CliFailure> {
    let targets = resolve_targets(config, ValidationMode::Full)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    targets
        .into_iter()
        .find(|target| target.name == requested)
        .ok_or_else(|| CliFailure::new(1, format!("Unknown target '{requested}'")))
}

#[derive(Debug)]
struct WatchCommandRequest {
    command: StreamingCommandSpec,
    cwd: Option<PathBuf>,
    log_path: PathBuf,
    timeout: Option<Duration>,
}

impl WatchCommandRequest {
    fn into_streaming(
        self,
        line_callback: &mut dyn FnMut(&str) -> StreamLineAction,
    ) -> StreamingCommand<'_> {
        let mut request = StreamingCommand::shell(String::new());
        request.command = self.command;
        request.cwd = self.cwd;
        request.log_path = Some(self.log_path);
        request.timeout = self.timeout;
        request.line_callback = Some(line_callback);
        request
    }
}

fn streaming_request(
    target: &ResolvedTarget,
    args: &WatchLocalArgs,
    local_cwd: &Path,
    log_path: &Path,
    timeout: Option<Duration>,
) -> Result<WatchCommandRequest, CliFailure> {
    match &target.backend {
        ResolvedBackend::Local(local) => {
            let cwd = args
                .target_cwd
                .as_ref()
                .map(PathBuf::from)
                .or_else(|| local.cwd.clone())
                .or_else(|| Some(local_cwd.to_path_buf()));
            Ok(WatchCommandRequest {
                command: StreamingCommandSpec::Shell(args.command.clone()),
                cwd,
                log_path: log_path.to_path_buf(),
                timeout: timeout.or_else(|| Some(Duration::from_secs(local.timeout_secs))),
            })
        }
        ResolvedBackend::Ssh(ssh) => {
            let Some(host) = ssh.host.as_deref().filter(|host| !host.trim().is_empty()) else {
                return Err(CliFailure::new(
                    1,
                    format!(
                        "Target '{}' is misconfigured: no `host` field.",
                        target.name
                    ),
                ));
            };
            let remote_cwd = args.target_cwd.as_deref().unwrap_or(&ssh.repo_path);
            let remote_command = format!("cd {} && {}", shlex_quote(remote_cwd), args.command);
            let mut ssh_argv = Vec::with_capacity(3 + ssh.ssh_options.len());
            ssh_argv.push("ssh".to_owned());
            ssh_argv.extend(ssh_options(&ssh.ssh_options, ssh.identity_file.as_deref()));
            ssh_argv.push(host.to_owned());
            ssh_argv.push(remote_command);

            Ok(WatchCommandRequest {
                command: StreamingCommandSpec::Args(ssh_argv),
                cwd: None,
                log_path: log_path.to_path_buf(),
                timeout: timeout.or_else(|| Some(Duration::from_secs(ssh.timeout_secs))),
            })
        }
        _ => Err(CliFailure::new(
            1,
            format!(
                "watch local supports local and ssh targets; target '{}' uses backend '{}'.",
                target.name, target.backend_name
            ),
        )),
    }
}

#[derive(Clone)]
struct CompiledPattern {
    source: String,
    regex: Regex,
}

#[derive(Clone)]
struct LineMatch {
    pattern: String,
    line: String,
}

struct WatchEventContext<'a> {
    json: bool,
    target: &'a str,
    backend: &'a str,
    command: &'a str,
    log_path: &'a Path,
    milestone_patterns: &'a [CompiledPattern],
    terminal_patterns: &'a [CompiledPattern],
}

fn run_stream_with_events<W: Write>(
    request: WatchCommandRequest,
    context: &WatchEventContext<'_>,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut write_error = None;
    let mut terminal_match = None;
    let result = {
        let mut line_callback = |line: &str| {
            handle_stream_line(line, context, stdout, &mut write_error, &mut terminal_match)
        };
        let request = request.into_streaming(&mut line_callback);
        run_streaming_command(request).map_err(|error| CliFailure::new(1, error.to_string()))?
    };
    if let Some(error) = write_error {
        return Err(CliFailure::new(1, error));
    }
    emit_completion(
        stdout,
        &CompletionEvent {
            json: context.json,
            target: context.target,
            backend: context.backend,
            command: context.command,
            log_path: context.log_path,
            returncode: result.returncode,
            duration_secs: result.duration_secs,
            terminal_match: terminal_match.as_ref(),
            stopped_early: result.termination_reason.is_some(),
        },
    )?;
    Ok(watch_exit_code(
        result.returncode,
        result.termination_reason.is_some(),
    ))
}

fn handle_stream_line<W: Write>(
    line: &str,
    context: &WatchEventContext<'_>,
    stdout: &mut W,
    write_error: &mut Option<String>,
    terminal_match: &mut Option<LineMatch>,
) -> StreamLineAction {
    let display = trim_line(line);
    emit_output_line(line, display, context, stdout, write_error);
    emit_milestones(display, context, stdout, write_error);
    match_terminal(display, context, stdout, write_error, terminal_match)
}

fn emit_output_line<W: Write>(
    raw_line: &str,
    display: &str,
    context: &WatchEventContext<'_>,
    stdout: &mut W,
    write_error: &mut Option<String>,
) {
    if context.json {
        record_write(
            write_error,
            emit_event(
                stdout,
                "output",
                context.target,
                context.backend,
                fields([
                    ("line", Value::from(display.to_owned())),
                    (
                        "log_path",
                        Value::from(context.log_path.display().to_string()),
                    ),
                ]),
            ),
        );
    } else {
        record_write(write_error, write!(stdout, "{raw_line}"));
    }
}

fn emit_milestones<W: Write>(
    display: &str,
    context: &WatchEventContext<'_>,
    stdout: &mut W,
    write_error: &mut Option<String>,
) {
    for pattern in context.milestone_patterns {
        if pattern.regex.is_match(display) {
            emit_milestone(pattern, display, context, stdout, write_error);
        }
    }
}

fn emit_milestone<W: Write>(
    pattern: &CompiledPattern,
    display: &str,
    context: &WatchEventContext<'_>,
    stdout: &mut W,
    write_error: &mut Option<String>,
) {
    if context.json {
        record_write(
            write_error,
            emit_event(
                stdout,
                "milestone",
                context.target,
                context.backend,
                fields([
                    ("pattern", Value::from(pattern.source.clone())),
                    ("line", Value::from(display.to_owned())),
                    (
                        "log_path",
                        Value::from(context.log_path.display().to_string()),
                    ),
                ]),
            ),
        );
    } else {
        record_write(
            write_error,
            writeln!(
                stdout,
                "shipyard milestone [{}]: {}",
                pattern.source, display
            ),
        );
    }
}

fn match_terminal<W: Write>(
    display: &str,
    context: &WatchEventContext<'_>,
    stdout: &mut W,
    write_error: &mut Option<String>,
    terminal_match: &mut Option<LineMatch>,
) -> StreamLineAction {
    let Some(pattern) = context
        .terminal_patterns
        .iter()
        .find(|pattern| pattern.regex.is_match(display))
    else {
        return StreamLineAction::Continue;
    };
    let matched = LineMatch {
        pattern: pattern.source.clone(),
        line: display.to_owned(),
    };
    *terminal_match = Some(matched.clone());
    emit_terminal_match(&matched, context, stdout, write_error);
    StreamLineAction::Terminate(format!("terminal regex matched: {}", matched.pattern))
}

fn emit_terminal_match<W: Write>(
    matched: &LineMatch,
    context: &WatchEventContext<'_>,
    stdout: &mut W,
    write_error: &mut Option<String>,
) {
    if context.json {
        record_write(
            write_error,
            emit_event(
                stdout,
                "terminal",
                context.target,
                context.backend,
                fields([
                    ("reason", Value::from("terminal_regex")),
                    ("pattern", Value::from(matched.pattern.clone())),
                    ("line", Value::from(matched.line.clone())),
                    (
                        "log_path",
                        Value::from(context.log_path.display().to_string()),
                    ),
                ]),
            ),
        );
    } else {
        record_write(
            write_error,
            writeln!(
                stdout,
                "shipyard terminal [{}]: {}",
                matched.pattern, matched.line
            ),
        );
    }
}

fn compile_patterns(kind: &str, patterns: &[String]) -> Result<Vec<CompiledPattern>, CliFailure> {
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(pattern)
                .map(|regex| CompiledPattern {
                    source: pattern.clone(),
                    regex,
                })
                .map_err(|error| {
                    CliFailure::new(1, format!("Invalid {kind} regex {pattern:?}: {error}"))
                })
        })
        .collect()
}

struct CompletionEvent<'a> {
    json: bool,
    target: &'a str,
    backend: &'a str,
    command: &'a str,
    log_path: &'a Path,
    returncode: i32,
    duration_secs: f64,
    terminal_match: Option<&'a LineMatch>,
    stopped_early: bool,
}

fn emit_completion<W: Write>(
    stdout: &mut W,
    event: &CompletionEvent<'_>,
) -> Result<(), CliFailure> {
    if event.terminal_match.is_some() {
        return Ok(());
    }
    if event.json {
        emit_event(
            stdout,
            "terminal",
            event.target,
            event.backend,
            fields([
                ("reason", Value::from("process_exit")),
                ("command", Value::from(event.command.to_owned())),
                ("returncode", Value::from(event.returncode)),
                ("duration_secs", Value::from(event.duration_secs)),
                ("stopped_early", Value::from(event.stopped_early)),
                (
                    "log_path",
                    Value::from(event.log_path.display().to_string()),
                ),
            ]),
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))
    } else {
        writeln!(
            stdout,
            "shipyard terminal [process_exit]: returncode={} duration={:.2}s log={}",
            event.returncode,
            event.duration_secs,
            event.log_path.display()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))
    }
}

fn emit_event<W: Write>(
    stdout: &mut W,
    event: &str,
    target: &str,
    backend: &str,
    mut data: BTreeMap<String, Value>,
) -> Result<(), Box<dyn std::error::Error>> {
    data.insert("event".to_owned(), Value::from(event.to_owned()));
    data.insert("target".to_owned(), Value::from(target.to_owned()));
    data.insert("backend".to_owned(), Value::from(backend.to_owned()));
    write_json_envelope(stdout, "watch.local", data)?;
    stdout.flush()?;
    Ok(())
}

fn fields(values: impl IntoIterator<Item = (&'static str, Value)>) -> BTreeMap<String, Value> {
    values
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn record_write(write_error: &mut Option<String>, result: Result<(), impl std::fmt::Display>) {
    if write_error.is_none()
        && let Err(error) = result
    {
        *write_error = Some(error.to_string());
    }
}

fn default_log_path(state_dir: &Path, target: &str) -> PathBuf {
    state_dir.join("watch").join(format!(
        "{}-{}.log",
        sanitize_path_component(target),
        Utc::now().format("%Y%m%dT%H%M%SZ")
    ))
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn trim_line(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn watch_exit_code(returncode: i32, stopped_early: bool) -> ExitCode {
    if stopped_early {
        return ExitCode::from(1);
    }
    if returncode == 0 {
        ExitCode::SUCCESS
    } else if (1..=255).contains(&returncode) {
        u8::try_from(returncode).map_or_else(|_| ExitCode::from(1), ExitCode::from)
    } else {
        ExitCode::from(1)
    }
}

#[cfg(test)]
mod tests {
    use super::{sanitize_path_component, streaming_request, trim_line, watch_exit_code};
    use crate::app::cli::WatchLocalArgs;
    use crate::executor::dispatch::{ResolvedBackend, ResolvedTarget, ResolvedValidation};
    use crate::executor::ssh::{SshTargetConfig, SshValidation};
    use crate::executor::streaming::StreamingCommandSpec;
    use std::process::ExitCode;

    #[test]
    fn watch_exit_code_preserves_process_status() {
        assert_eq!(watch_exit_code(0, false), ExitCode::SUCCESS);
        assert_eq!(watch_exit_code(7, false), ExitCode::from(7));
        assert_eq!(watch_exit_code(-1, false), ExitCode::from(1));
        assert_eq!(watch_exit_code(0, true), ExitCode::from(1));
    }

    #[test]
    fn log_path_component_is_sanitized() {
        assert_eq!(sanitize_path_component("linux/v8 vm"), "linux-v8-vm");
    }

    #[test]
    fn trim_line_removes_newlines_only() {
        assert_eq!(trim_line("  build done \r\n"), "  build done ");
    }

    #[test]
    fn ssh_target_builds_remote_watch_command() {
        let args = WatchLocalArgs {
            target: "linux".to_owned(),
            command: "ninja -C out".to_owned(),
            target_cwd: None,
            milestone_regex: Vec::new(),
            terminal_regex: Vec::new(),
            log_path: None,
            timeout_secs: None,
        };
        let mut ssh = SshTargetConfig {
            name: "linux".to_owned(),
            platform: "linux-arm64".to_owned(),
            host: Some("builder".to_owned()),
            repo_path: "/work/v8-builder".to_owned(),
            identity_file: Some("/tmp/id_ed25519".to_owned()),
            ..SshTargetConfig::default()
        };
        ssh.ssh_options = vec!["-p".to_owned(), "2222".to_owned()];
        let target = ResolvedTarget {
            name: "linux".to_owned(),
            validation_build_type: None,
            platform: "linux-arm64".to_owned(),
            backend_name: "ssh".to_owned(),
            warm_keepalive_seconds: 0,
            host: Some("builder".to_owned()),
            backend: ResolvedBackend::Ssh(ssh),
            validation: ResolvedValidation::Ssh {
                validation: SshValidation::Command(String::new()),
                contract: None,
            },
            failure_parser: None,
        };

        let request = streaming_request(
            &target,
            &args,
            std::path::Path::new("/unused"),
            std::path::Path::new("/tmp/watch.log"),
            None,
        )
        .expect("request");

        let StreamingCommandSpec::Args(ssh_argv) = request.command else {
            panic!("ssh request should use argv form");
        };
        assert_eq!(ssh_argv[0], "ssh");
        assert_eq!(
            ssh_argv[1..6],
            ["-p", "2222", "-i", "/tmp/id_ed25519", "builder"]
        );
        assert_eq!(ssh_argv[6], "cd /work/v8-builder && ninja -C out");
    }
}
