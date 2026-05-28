use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};

use serde_json::Value;

use super::{CliFailure, cli::BranchCommand, wait_cmd::parse_github_repo_slug};
use crate::branch::{BranchApplyResult, BranchApplyStatus};
use crate::config::LoadedConfig;
use crate::governance::{GovernanceGh, resolve_branch_rules};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;

pub(super) fn branch_command<W: Write>(
    command: BranchCommand,
    mode: RuntimeMode,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    branch_command_with(command, &config, cwd, json_mode, stdout, None, None)
}

fn branch_command_with<W: Write>(
    command: BranchCommand,
    config: &LoadedConfig,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
    git_command: Option<&Path>,
    gh_command: Option<&Path>,
) -> Result<ExitCode, CliFailure> {
    match command {
        BranchCommand::Apply {
            create_name,
            base_branch,
            target_branch,
        } => branch_apply(
            config,
            cwd,
            create_name.as_deref(),
            &base_branch,
            target_branch.as_deref(),
            json_mode,
            stdout,
            git_command,
            gh_command,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn branch_apply<W: Write>(
    config: &LoadedConfig,
    cwd: &Path,
    create_name: Option<&str>,
    base_branch: &str,
    target_branch: Option<&str>,
    json_mode: bool,
    stdout: &mut W,
    git_command: Option<&Path>,
    gh_command: Option<&Path>,
) -> Result<ExitCode, CliFailure> {
    let branch_name = create_name.or(target_branch).ok_or_else(|| {
        CliFailure::new(1, "Specify a branch name (positional) or --create <name>")
    })?;
    let repo = detect_repo_from_remote(cwd, git_command)
        .ok_or_else(|| CliFailure::new(1, "Could not detect repo from git remote."))?;
    let rules = resolve_branch_rules(&config.data, branch_name)
        .map_err(|error| CliFailure::new(1, error))?;
    let gh = GovernanceGh::from_loaded_config(cwd, config, gh_command)
        .map_err(|error| CliFailure::new(1, error))?;

    let result = if create_name.is_some() {
        crate::branch::create_branch_and_apply_rules(
            cwd,
            &repo,
            branch_name,
            base_branch,
            &rules,
            git_command,
            &gh,
        )
    } else {
        crate::branch::apply_branch_rules(&repo, branch_name, &rules, &gh)
    };

    render_result(stdout, &result, json_mode)?;
    Ok(if result.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

pub(super) fn detect_repo_from_remote(cwd: &Path, git_command: Option<&Path>) -> Option<String> {
    let output = git_command
        .map_or_else(|| Command::new("git"), Command::new)
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_github_repo_slug(&String::from_utf8_lossy(&output.stdout))
}

fn render_result<W: Write>(
    stdout: &mut W,
    result: &BranchApplyResult,
    json_mode: bool,
) -> Result<(), CliFailure> {
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("branch".to_owned(), Value::from(result.branch.clone()));
        data.insert("status".to_owned(), Value::from(result.status.as_str()));
        data.insert("message".to_owned(), Value::from(result.message.clone()));
        data.insert("ok".to_owned(), Value::from(result.ok()));
        write_json_envelope(stdout, "branch.apply", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }

    let marker = match result.status {
        BranchApplyStatus::RulesApplied | BranchApplyStatus::Created => "OK",
        BranchApplyStatus::AlreadyExists => "INFO",
        BranchApplyStatus::RulesFailed | BranchApplyStatus::GitFailed => "ERROR",
    };
    writeln!(stdout, "  {marker} {}", result.message)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use clap::Parser;
    use serde_json::json;
    use tempfile::TempDir;
    use toml::Table;

    use super::branch_command_with;
    use crate::app::cli::{BranchCommand, Cli, Command};
    use crate::config::{LoadedConfig, LocalOverlaySource};

    #[test]
    fn cli_surface_parses_branch_apply_create() {
        let cli = Cli::parse_from([
            "shipyard",
            "branch",
            "apply",
            "--create",
            "develop/demo",
            "--base",
            "main",
        ]);

        let Command::Branch { command } = cli.command else {
            panic!("expected branch command");
        };
        let BranchCommand::Apply {
            create_name,
            base_branch,
            target_branch,
        } = command;
        assert_eq!(create_name.as_deref(), Some("develop/demo"));
        assert_eq!(base_branch, "main");
        assert_eq!(target_branch, None);
    }

    #[test]
    fn branch_apply_create_renders_json_success() {
        let temp = TempDir::new().expect("tempdir");
        let trace = temp.path().join("trace");
        let payload = temp.path().join("payload.json");
        let git = write_script(
            temp.path(),
            "git",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
if [ "$1" = "remote" ]; then
  printf 'git@github.com:owner/repo.git\n'
  exit 0
fi
if [ "$1" = "ls-remote" ] && [ "$3" = "--heads" ]; then
  exit 2
fi
if [ "$1" = "ls-remote" ] && [ "$3" = "origin" ]; then
  printf 'abcdef1234567890\trefs/heads/main\n'
  exit 0
fi
if [ "$1" = "push" ]; then
  exit 0
fi
exit 99
"#,
                trace.display()
            ),
        );
        let gh = write_script(
            temp.path(),
            "gh",
            &format!(
                r"#!/bin/sh
cat > '{}'
exit 0
",
                payload.display()
            ),
        );
        let config = loaded_config(temp.path());
        let mut stdout = Vec::new();

        let code = branch_command_with(
            BranchCommand::Apply {
                create_name: Some("develop/demo".to_owned()),
                base_branch: "main".to_owned(),
                target_branch: None,
            },
            &config,
            temp.path(),
            true,
            &mut stdout,
            Some(&git),
            Some(&gh),
        )
        .expect("command");

        assert_eq!(code, std::process::ExitCode::SUCCESS);
        let value: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(value["command"], "branch.apply");
        assert_eq!(value["branch"], "develop/demo");
        assert_eq!(value["status"], "rules_applied");
        assert_eq!(value["ok"], json!(true));
        assert!(
            fs::read_to_string(trace)
                .expect("trace")
                .contains("push origin")
        );
        assert!(
            fs::read_to_string(payload)
                .expect("payload")
                .contains("required_status_checks")
        );
    }

    #[test]
    fn branch_apply_requires_a_branch_name() {
        let temp = TempDir::new().expect("tempdir");
        let git = write_script(
            temp.path(),
            "git",
            r#"#!/bin/sh
if [ "$1" = "remote" ]; then
  printf 'https://github.com/owner/repo.git\n'
  exit 0
fi
exit 99
"#,
        );
        let mut stdout = Vec::new();

        let error = branch_command_with(
            BranchCommand::Apply {
                create_name: None,
                base_branch: "main".to_owned(),
                target_branch: None,
            },
            &loaded_config(temp.path()),
            temp.path(),
            false,
            &mut stdout,
            Some(&git),
            None,
        )
        .expect_err("missing branch should fail");

        assert_eq!(error.code, 1);
        assert!(error.message.contains("Specify a branch name"));
        assert!(stdout.is_empty());
    }

    fn loaded_config(root: &std::path::Path) -> LoadedConfig {
        let data = r#"
            [project]
            profile = "multi"

            [governance]
            required_status_checks = ["ci"]
        "#
        .parse::<Table>()
        .expect("toml");
        LoadedConfig {
            data,
            global_dir: root.join("global"),
            project_dir: Some(root.join(".shipyard")),
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn write_script(dir: &std::path::Path, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).expect("write script");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod");
        path
    }
}
