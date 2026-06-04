use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use chrono::Utc;
use glob::{Pattern, glob};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::CliFailure;
use super::cli::RunCommandEvidenceArgs;
use crate::config::LoadedConfig;
use crate::evidence::{CommandEvidenceArtifact, CommandEvidenceRecord, CommandEvidenceStore};
use crate::executor::dispatch::{ResolvedBackend, ResolvedTarget, resolve_targets};
use crate::executor::ssh::{shlex_quote, ssh_options};
use crate::executor::streaming::{
    StreamingCommand, StreamingCommandResult, StreamingCommandSpec, run_streaming_command,
};
use crate::job::ValidationMode;
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;

pub(super) fn run_command_evidence<W: Write>(
    args: &RunCommandEvidenceArgs,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let target = resolve_command_target(config, &args.target)?;
    let command = CommandRequest::for_target(&target, args, cwd)?;
    let name = args.name.as_deref().unwrap_or(&target.name);
    let id = evidence_id(name, &target.name);
    let store = CommandEvidenceStore::new(runtime_paths.state_dir.join("command-evidence"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let log_path = args
        .log_path
        .clone()
        .unwrap_or_else(|| default_log_path(&runtime_paths.state_dir, &id));
    let artifact_dir = store.artifact_dir(&id);
    let timeout = args
        .timeout_secs
        .map(Duration::from_secs)
        .or(command.default_timeout);

    let result = run_command(&command, &log_path, timeout)?;
    let mut artifact_errors = Vec::new();
    let artifacts = collect_artifacts(
        &command.artifact_backend,
        &args.artifacts,
        &artifact_dir,
        &mut artifact_errors,
    )?;
    let exit_ok = result.returncode == args.expect_code;
    let status = if exit_ok && artifact_errors.is_empty() {
        "pass"
    } else {
        "fail"
    }
    .to_owned();
    let (branch, sha) = git_identity(cwd);
    let bundle_path = store.bundle_dir(&id);
    let record = CommandEvidenceRecord {
        schema_version: 1,
        id,
        name: name.to_owned(),
        branch,
        sha,
        target_name: target.name.clone(),
        platform: target.platform.clone(),
        backend: target.backend_name.clone(),
        host: command.host.clone(),
        workdir: command.workdir.clone(),
        command: args.command.clone(),
        expected_exit_code: args.expect_code,
        exit_code: result.returncode,
        status,
        started_at: result.started_at,
        completed_at: result.completed_at,
        duration_secs: result.duration_secs,
        log_path: log_path.display().to_string(),
        log_excerpt: result.output,
        env_fingerprint: env_fingerprints(&args.env_fingerprints),
        artifacts,
        artifact_errors,
        bundle_path: bundle_path.display().to_string(),
    };
    store
        .record(&record)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    emit_run_command_evidence(stdout, &record, json_mode)?;
    Ok(if record.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

pub(super) fn show_command_evidence<W: Write>(
    id: Option<String>,
    list: bool,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let store = CommandEvidenceStore::new(state_dir.join("command-evidence"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if list {
        let records = store.list();
        if json_mode {
            write_json_envelope(
                stdout,
                "evidence.command",
                fields([("records", serde_json::to_value(&records)?)]),
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        } else if records.is_empty() {
            writeln!(stdout, "No command evidence")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        } else {
            for record in records {
                writeln!(
                    stdout,
                    "{} {} target={} exit={}/{} bundle={}",
                    record.id,
                    record.status,
                    record.target_name,
                    record.exit_code,
                    record.expected_exit_code,
                    record.bundle_path
                )
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    let record = if let Some(id) = id {
        store
            .get(&id)
            .ok_or_else(|| CliFailure::new(1, format!("Command evidence '{id}' not found")))?
    } else {
        store
            .latest()
            .ok_or_else(|| CliFailure::new(1, "No command evidence found"))?
    };
    if json_mode {
        write_json_envelope(
            stdout,
            "evidence.command",
            fields([("evidence", serde_json::to_value(&record)?)]),
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        write_command_evidence_summary(stdout, &record)?;
    }
    Ok(if record.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[derive(Debug)]
struct CommandRequest {
    command: StreamingCommandSpec,
    default_timeout: Option<Duration>,
    workdir: String,
    host: Option<String>,
    artifact_backend: ArtifactBackend,
}

impl CommandRequest {
    fn for_target(
        target: &ResolvedTarget,
        args: &RunCommandEvidenceArgs,
        local_cwd: &Path,
    ) -> Result<Self, CliFailure> {
        if args.command.is_empty() {
            return Err(CliFailure::new(1, "run command requires an argv"));
        }
        match &target.backend {
            ResolvedBackend::Local(local) => {
                let cwd = args
                    .target_cwd
                    .as_ref()
                    .map(PathBuf::from)
                    .or_else(|| local.cwd.clone())
                    .unwrap_or_else(|| local_cwd.to_path_buf());
                Ok(Self {
                    command: StreamingCommandSpec::Args(args.command.clone()),
                    default_timeout: Some(Duration::from_secs(local.timeout_secs)),
                    workdir: cwd.display().to_string(),
                    host: None,
                    artifact_backend: ArtifactBackend::Local { cwd },
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
                let remote_argv = args
                    .command
                    .iter()
                    .map(|arg| shlex_quote(arg))
                    .collect::<Vec<_>>()
                    .join(" ");
                let remote_command = format!("cd {} && {remote_argv}", shlex_quote(remote_cwd));
                let options = ssh_options(&ssh.ssh_options, ssh.identity_file.as_deref());
                let mut ssh_argv = Vec::with_capacity(3 + options.len());
                ssh_argv.push("ssh".to_owned());
                ssh_argv.extend(options.clone());
                ssh_argv.push(host.to_owned());
                ssh_argv.push(remote_command);

                Ok(Self {
                    command: StreamingCommandSpec::Args(ssh_argv),
                    default_timeout: Some(Duration::from_secs(ssh.timeout_secs)),
                    workdir: remote_cwd.to_owned(),
                    host: Some(host.to_owned()),
                    artifact_backend: ArtifactBackend::Ssh {
                        host: host.to_owned(),
                        remote_cwd: remote_cwd.to_owned(),
                        ssh_options: options,
                    },
                })
            }
            _ => Err(CliFailure::new(
                1,
                format!(
                    "run command supports local and ssh targets; target '{}' uses backend '{}'.",
                    target.name, target.backend_name
                ),
            )),
        }
    }
}

#[derive(Debug)]
enum ArtifactBackend {
    Local {
        cwd: PathBuf,
    },
    Ssh {
        host: String,
        remote_cwd: String,
        ssh_options: Vec<String>,
    },
}

fn resolve_command_target(
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

fn run_command(
    request: &CommandRequest,
    log_path: &Path,
    timeout: Option<Duration>,
) -> Result<StreamingCommandResult, CliFailure> {
    let mut streaming = StreamingCommand::shell(String::new());
    streaming.command = request.command.clone();
    streaming.cwd = match &request.artifact_backend {
        ArtifactBackend::Local { cwd } => Some(cwd.clone()),
        ArtifactBackend::Ssh { .. } => None,
    };
    streaming.log_path = Some(log_path.to_path_buf());
    streaming.timeout = timeout;
    run_streaming_command(streaming).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn collect_artifacts(
    backend: &ArtifactBackend,
    patterns: &[String],
    artifact_dir: &Path,
    artifact_errors: &mut Vec<String>,
) -> Result<Vec<CommandEvidenceArtifact>, CliFailure> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }
    fs::create_dir_all(artifact_dir).map_err(|error| CliFailure::new(1, error.to_string()))?;
    match backend {
        ArtifactBackend::Local { cwd } => {
            collect_local_artifacts(cwd, patterns, artifact_dir, artifact_errors)
        }
        ArtifactBackend::Ssh {
            host,
            remote_cwd,
            ssh_options,
        } => collect_ssh_artifacts(
            host,
            remote_cwd,
            ssh_options,
            patterns,
            artifact_dir,
            artifact_errors,
        ),
    }
}

fn collect_local_artifacts(
    cwd: &Path,
    patterns: &[String],
    artifact_dir: &Path,
    artifact_errors: &mut Vec<String>,
) -> Result<Vec<CommandEvidenceArtifact>, CliFailure> {
    let mut artifacts = Vec::new();
    let mut seen = BTreeSet::new();
    for pattern in patterns {
        let full_pattern = format!(
            "{}/{}",
            Pattern::escape(&cwd.display().to_string()),
            pattern
        );
        let mut matched = false;
        for entry in glob(&full_pattern).map_err(|error| CliFailure::new(1, error.to_string()))? {
            let path = entry.map_err(|error| CliFailure::new(1, error.to_string()))?;
            if !path.is_file() {
                continue;
            }
            let relative = path
                .strip_prefix(cwd)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            let relative = safe_relative_path(relative)?;
            let source = relative.display().to_string();
            if !seen.insert(source.clone()) {
                matched = true;
                continue;
            }
            let destination = artifact_dir.join(&relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
            fs::copy(&path, &destination).map_err(|error| CliFailure::new(1, error.to_string()))?;
            let size_bytes = destination
                .metadata()
                .map_err(|error| CliFailure::new(1, error.to_string()))?
                .len();
            artifacts.push(CommandEvidenceArtifact {
                pattern: pattern.clone(),
                source,
                path: destination.display().to_string(),
                size_bytes,
            });
            matched = true;
        }
        if !matched {
            artifact_errors.push(format!("artifact pattern matched no files: {pattern}"));
        }
    }
    Ok(artifacts)
}

fn collect_ssh_artifacts(
    host: &str,
    remote_cwd: &str,
    ssh_options: &[String],
    patterns: &[String],
    artifact_dir: &Path,
    artifact_errors: &mut Vec<String>,
) -> Result<Vec<CommandEvidenceArtifact>, CliFailure> {
    let mut artifacts = Vec::new();
    let mut seen = BTreeSet::new();
    for pattern in patterns {
        let matches = remote_find_matches(host, remote_cwd, ssh_options, pattern)?;
        if matches.is_empty() {
            artifact_errors.push(format!("artifact pattern matched no files: {pattern}"));
            continue;
        }
        for relative in matches {
            if !seen.insert(relative.clone()) {
                continue;
            }
            let relative_path = safe_relative_path(Path::new(&relative))?;
            let destination = artifact_dir.join(&relative_path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
            pull_ssh_file(host, remote_cwd, ssh_options, &relative, &destination)?;
            let size_bytes = destination
                .metadata()
                .map_err(|error| CliFailure::new(1, error.to_string()))?
                .len();
            artifacts.push(CommandEvidenceArtifact {
                pattern: pattern.clone(),
                source: relative,
                path: destination.display().to_string(),
                size_bytes,
            });
        }
    }
    Ok(artifacts)
}

fn remote_find_matches(
    host: &str,
    remote_cwd: &str,
    ssh_options: &[String],
    pattern: &str,
) -> Result<Vec<String>, CliFailure> {
    let pattern = normalize_remote_pattern(pattern)?;
    let remote = format!(
        "cd {} && find . -type f -path {} -print0",
        shlex_quote(remote_cwd),
        shlex_quote(&pattern)
    );
    let output = Command::new("ssh")
        .args(ssh_options)
        .arg(host)
        .arg(remote)
        .output()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "remote artifact lookup failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| {
            String::from_utf8_lossy(chunk)
                .trim_start_matches("./")
                .to_owned()
        })
        .collect())
}

fn pull_ssh_file(
    host: &str,
    remote_cwd: &str,
    ssh_options: &[String],
    relative: &str,
    destination: &Path,
) -> Result<(), CliFailure> {
    let remote = format!(
        "cd {} && cat {}",
        shlex_quote(remote_cwd),
        shlex_quote(&format!("./{relative}"))
    );
    let file = File::create(destination).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let status = Command::new("ssh")
        .args(ssh_options)
        .arg(host)
        .arg(remote)
        .stdout(Stdio::from(file))
        .status()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!("remote artifact pull failed for {relative}"),
        ))
    }
}

fn normalize_remote_pattern(pattern: &str) -> Result<String, CliFailure> {
    if pattern.trim().is_empty() {
        return Err(CliFailure::new(1, "artifact pattern cannot be empty"));
    }
    if pattern.starts_with('/') || pattern.contains("..") {
        return Err(CliFailure::new(
            1,
            format!("artifact pattern must be relative to the target cwd: {pattern}"),
        ));
    }
    let pattern = pattern.trim_start_matches("./");
    Ok(format!("./{pattern}"))
}

fn safe_relative_path(path: &Path) -> Result<PathBuf, CliFailure> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => output.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CliFailure::new(
                    1,
                    format!("artifact path escapes bundle directory: {}", path.display()),
                ));
            }
        }
    }
    if output.as_os_str().is_empty() {
        return Err(CliFailure::new(1, "artifact path cannot be empty"));
    }
    Ok(output)
}

fn git_identity(cwd: &Path) -> (Option<String>, Option<String>) {
    (
        git_optional(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]),
        git_optional(cwd, &["rev-parse", "HEAD"]),
    )
}

fn git_optional(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn env_fingerprints(names: &[String]) -> BTreeMap<String, String> {
    names
        .iter()
        .map(|name| {
            let value = std::env::var(name).map_or_else(
                |_| "unset".to_owned(),
                |value| format!("sha256:{}", hex_digest(value.as_bytes())),
            );
            (name.clone(), value)
        })
        .collect()
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn evidence_id(name: &str, target: &str) -> String {
    format!(
        "cmd-{}-{}-{}",
        Utc::now().format("%Y%m%d-%H%M%S-%3f"),
        sanitize_component(name),
        sanitize_component(target)
    )
}

fn default_log_path(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join("logs").join(format!("{id}.log"))
}

fn sanitize_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "command".to_owned()
    } else {
        sanitized
    }
}

fn emit_run_command_evidence<W: Write>(
    stdout: &mut W,
    record: &CommandEvidenceRecord,
    json_mode: bool,
) -> Result<(), CliFailure> {
    if json_mode {
        write_json_envelope(
            stdout,
            "run.command",
            fields([("evidence", serde_json::to_value(record)?)]),
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))
    } else {
        write_command_evidence_summary(stdout, record)
    }
}

fn write_command_evidence_summary<W: Write>(
    stdout: &mut W,
    record: &CommandEvidenceRecord,
) -> Result<(), CliFailure> {
    writeln!(stdout, "Command evidence {}: {}", record.id, record.status)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(
        stdout,
        "  target={} backend={} exit={}/{} duration={:.2}s",
        record.target_name,
        record.backend,
        record.exit_code,
        record.expected_exit_code,
        record.duration_secs
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "  log={}", record.log_path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(
        stdout,
        "  bundle={} artifacts={}",
        record.bundle_path,
        record.artifacts.len()
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    for error in &record.artifact_errors {
        writeln!(stdout, "  artifact-error={error}")
            .map_err(|write_error| CliFailure::new(1, write_error.to_string()))?;
    }
    Ok(())
}

fn fields(items: impl IntoIterator<Item = (&'static str, Value)>) -> BTreeMap<String, Value> {
    items
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::process::{Command, ExitCode, Stdio};

    use toml::Table;

    use super::{
        collect_local_artifacts, normalize_remote_pattern, run_command_evidence, safe_relative_path,
    };
    use crate::app::cli::RunCommandEvidenceArgs;
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use crate::identity::RuntimeMode;
    use crate::paths::RuntimePaths;

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

    fn seed_repo(repo: &std::path::Path) {
        std::fs::create_dir_all(repo).expect("repo dir");
        git(&["init", "-q"], repo);
        git(&["checkout", "-b", "feature"], repo);
        std::fs::write(repo.join("source.txt"), "initial\n").expect("seed source");
        git(&["add", "source.txt"], repo);
        git(&["commit", "-qm", "initial"], repo);
    }

    fn loaded_config(root: &std::path::Path, repo: &std::path::Path) -> LoadedConfig {
        let repo = repo.display().to_string().replace('\\', "\\\\");
        let config = format!(
            r#"
            [validation.default]
            command = "true"

            [targets.mac]
            backend = "local"
            platform = "macos-arm64"
            cwd = "{repo}"
            "#,
        )
        .parse::<Table>()
        .expect("config TOML");
        LoadedConfig {
            data: config,
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    #[test]
    fn run_command_evidence_records_artifact_bundle() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_repo(&repo);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let args = RunCommandEvidenceArgs {
            target: "mac".to_owned(),
            name: Some("smoke".to_owned()),
            expect_code: 0,
            target_cwd: None,
            artifacts: vec!["out/result.txt".to_owned()],
            log_path: None,
            timeout_secs: None,
            env_fingerprints: Vec::new(),
            command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "mkdir -p out && echo artifact > out/result.txt && echo PASS".to_owned(),
            ],
        };
        let mut stdout = Vec::new();

        let code = run_command_evidence(
            &args,
            &loaded_config(temp.path(), &repo),
            &repo,
            &paths,
            true,
            &mut stdout,
        )
        .expect("run command evidence");

        assert_eq!(code, ExitCode::SUCCESS);
        let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(output["command"], "run.command");
        let evidence = &output["evidence"];
        assert_eq!(evidence["status"], "pass");
        assert_eq!(evidence["target"], "mac");
        assert_eq!(evidence["artifacts"][0]["source"], "out/result.txt");
        let artifact_path = evidence["artifacts"][0]["path"].as_str().expect("path");
        assert_eq!(
            std::fs::read_to_string(artifact_path).expect("artifact"),
            "artifact\n"
        );
    }

    #[test]
    fn local_artifact_missing_pattern_records_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut errors = Vec::new();

        let artifacts = collect_local_artifacts(
            temp.path(),
            &["missing/*.txt".to_owned()],
            &temp.path().join("artifacts"),
            &mut errors,
        )
        .expect("collect local artifacts");

        assert!(artifacts.is_empty());
        assert_eq!(
            errors,
            vec!["artifact pattern matched no files: missing/*.txt"]
        );
    }

    #[test]
    fn artifact_paths_must_remain_relative() {
        assert!(safe_relative_path(std::path::Path::new("out/file.txt")).is_ok());
        assert!(safe_relative_path(std::path::Path::new("../file.txt")).is_err());
        assert!(safe_relative_path(std::path::Path::new("/tmp/file.txt")).is_err());
    }

    #[test]
    fn remote_patterns_are_relative_find_paths() {
        assert_eq!(
            normalize_remote_pattern("build/lib.so").expect("pattern"),
            "./build/lib.so"
        );
        assert!(normalize_remote_pattern("../secret").is_err());
        assert!(normalize_remote_pattern("/tmp/file").is_err());
    }
}
