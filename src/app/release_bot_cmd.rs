use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    CliFailure,
    branch_cmd::detect_repo_from_remote,
    cli::{ReleaseBotCommand, ReleaseBotHookCommand},
};
use crate::config::LoadedConfig;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;

const POST_TAG_WORKFLOW: &str = "post-tag-sync.yml";

pub(super) fn release_bot_command<W: Write>(
    command: ReleaseBotCommand,
    mode: RuntimeMode,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    release_bot_command_with(command, &config, cwd, json_mode, stdout, None)
}

fn release_bot_command_with<W: Write>(
    command: ReleaseBotCommand,
    config: &LoadedConfig,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
    gh_command: Option<&Path>,
) -> Result<ExitCode, CliFailure> {
    match command {
        ReleaseBotCommand::Status { siblings } => {
            let repo = repo_slug(cwd)?;
            let state = detect_state(&repo, &siblings, gh_command);
            render_status(stdout, &state, json_mode)?;
            Ok(ExitCode::SUCCESS)
        }
        ReleaseBotCommand::Setup {
            shared_name,
            paste,
            siblings,
            verify,
            no_verify,
            reconfigure,
        } => {
            let repo = repo_slug(cwd)?;
            setup(
                stdout,
                &repo,
                &SetupOptions {
                    shared_name: shared_name.as_deref(),
                    paste,
                    siblings: &siblings,
                    verify: verify && !no_verify,
                    reconfigure,
                },
                gh_command,
            )
        }
        ReleaseBotCommand::Hook { command } => match command {
            ReleaseBotHookCommand::Install {
                tag_pattern,
                shipyard_version,
            } => hook_install(
                stdout,
                cwd,
                tag_pattern.as_deref().unwrap_or("v*"),
                shipyard_version
                    .as_deref()
                    .unwrap_or(env!("CARGO_PKG_VERSION")),
                json_mode,
            ),
            ReleaseBotHookCommand::Run { tag } => {
                hook_run(stdout, config, cwd, tag.as_deref(), json_mode)
            }
        },
    }
}

fn repo_slug(cwd: &Path) -> Result<String, CliFailure> {
    detect_repo_from_remote(cwd, None)
        .ok_or_else(|| CliFailure::new(1, "Can't detect owner/repo from git remote."))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReleaseBotState {
    repo_slug: String,
    secret_present: bool,
    secret_updated_at: Option<DateTime<Utc>>,
    last_auto_release_conclusion: Option<String>,
    last_auto_release_error_signature: Option<String>,
    other_repos_with_secret: Vec<String>,
}

fn detect_state(
    repo_slug: &str,
    siblings: &[String],
    gh_command: Option<&Path>,
) -> ReleaseBotState {
    let secrets = list_secrets(repo_slug, gh_command);
    let (secret_present, secret_updated_at) = secrets
        .as_ref()
        .and_then(|items| {
            items.iter().find(|secret| {
                secret.get("name").and_then(Value::as_str) == Some("RELEASE_BOT_TOKEN")
            })
        })
        .map_or((false, None), |secret| {
            (
                true,
                secret
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .and_then(parse_time),
            )
        });
    let last_run = last_workflow_run(repo_slug, "auto-release.yml", gh_command);
    let last_auto_release_conclusion = last_run
        .as_ref()
        .and_then(|run| run.get("conclusion"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let last_auto_release_error_signature =
        if last_auto_release_conclusion.as_deref() == Some("failure") {
            last_run
                .as_ref()
                .and_then(|run| run.get("databaseId"))
                .and_then(Value::as_u64)
                .and_then(|run_id| detect_checkout_auth_failure(repo_slug, run_id, gh_command))
        } else {
            None
        };
    let mut other_repos_with_secret = Vec::new();
    for sibling in siblings {
        if sibling == repo_slug {
            continue;
        }
        if list_secrets(sibling, gh_command).is_some_and(|items| {
            items.iter().any(|secret| {
                secret.get("name").and_then(Value::as_str) == Some("RELEASE_BOT_TOKEN")
            })
        }) {
            other_repos_with_secret.push(sibling.clone());
        }
    }

    ReleaseBotState {
        repo_slug: repo_slug.to_owned(),
        secret_present,
        secret_updated_at,
        last_auto_release_conclusion,
        last_auto_release_error_signature,
        other_repos_with_secret,
    }
}

fn render_status<W: Write>(
    stdout: &mut W,
    state: &ReleaseBotState,
    json_mode: bool,
) -> Result<(), CliFailure> {
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::from(state.repo_slug.clone()));
        data.insert(
            "secret_present".to_owned(),
            Value::from(state.secret_present),
        );
        data.insert(
            "secret_updated_at".to_owned(),
            state
                .secret_updated_at
                .map_or(Value::Null, |time| Value::from(time.to_rfc3339())),
        );
        data.insert(
            "last_auto_release_conclusion".to_owned(),
            optional_string(state.last_auto_release_conclusion.as_deref()),
        );
        data.insert(
            "last_auto_release_error_signature".to_owned(),
            optional_string(state.last_auto_release_error_signature.as_deref()),
        );
        data.insert(
            "other_repos_with_secret".to_owned(),
            Value::Array(
                state
                    .other_repos_with_secret
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect(),
            ),
        );
        write_json_envelope(stdout, "release-bot:status", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    for line in describe_state(state) {
        writeln!(stdout, "{line}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    if state.last_auto_release_error_signature.as_deref() == Some("auth") && state.secret_present {
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(
            stdout,
            "Diagnosis: the stored token is being rejected by actions/checkout."
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(
            stdout,
            "Either the PAT does not list this repo, or the stored secret value is stale. Run `shipyard release-bot setup --reconfigure` to fix."
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

struct SetupOptions<'a> {
    shared_name: Option<&'a str>,
    paste: bool,
    siblings: &'a [String],
    verify: bool,
    reconfigure: bool,
}

fn setup<W: Write>(
    stdout: &mut W,
    repo_slug: &str,
    options: &SetupOptions<'_>,
    gh_command: Option<&Path>,
) -> Result<ExitCode, CliFailure> {
    let state = detect_state(repo_slug, options.siblings, gh_command);
    for line in describe_state(&state) {
        writeln!(stdout, "{line}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    if state.secret_present && !options.reconfigure && !options.paste {
        writeln!(
            stdout,
            "RELEASE_BOT_TOKEN is already set. Pass --reconfigure to replace it, or run `shipyard doctor --release-chain`."
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ExitCode::SUCCESS);
    }

    let plan = plan_setup(&state, options.shared_name);
    if !options.paste {
        let (owner, repo) = repo_slug
            .split_once('/')
            .ok_or_else(|| CliFailure::new(1, "repo slug must be OWNER/REPO"))?;
        let pat_url = render_pat_creation_url(owner, repo, &plan.suggested_pat_name);
        writeln!(stdout, "Recommended PAT name: {}", plan.suggested_pat_name)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout, "Rationale: {}", plan.reasoning)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(
            stdout,
            "\nOpen this URL to create or edit the PAT:\n  {pat_url}"
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(
            stdout,
            "\nRequired repository permissions:\n  - Contents: Read and write\n  - Metadata: Read-only\n  - Workflows: Read and write when the bot touches workflows"
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }

    writeln!(stdout, "\nPaste the token, then press Enter:")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut token = String::new();
    std::io::stdin()
        .read_line(&mut token)
        .map_err(|error| CliFailure::new(1, format!("failed to read token: {error}")))?;
    let token = token.trim();
    if token.is_empty() {
        return Err(CliFailure::new(1, "Empty token. Aborting."));
    }
    set_secret(repo_slug, token, gh_command)?;
    writeln!(stdout, "Stored RELEASE_BOT_TOKEN on {repo_slug}.")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;

    if options.verify {
        writeln!(stdout, "Dispatching auto-release.yml to verify checkout...")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        match verify_token(repo_slug, gh_command) {
            Ok(conclusion) if conclusion == "success" => {
                writeln!(stdout, "actions/checkout accepted the token.")
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
            Ok(conclusion) => {
                writeln!(stdout, "Verification workflow concluded: {conclusion}.")
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
            Err(error) => {
                writeln!(stdout, "Verification dispatch failed: {error}")
                    .map_err(|io_error| CliFailure::new(1, io_error.to_string()))?;
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn hook_install<W: Write>(
    stdout: &mut W,
    cwd: &Path,
    tag_pattern: &str,
    shipyard_version: &str,
    json_mode: bool,
) -> Result<ExitCode, CliFailure> {
    let workflows_dir = cwd.join(".github").join("workflows");
    fs::create_dir_all(&workflows_dir).map_err(|error| {
        CliFailure::new(
            1,
            format!("failed to create {}: {error}", workflows_dir.display()),
        )
    })?;
    let target = workflows_dir.join(POST_TAG_WORKFLOW);
    let overwrote = target.exists();
    fs::write(&target, render_workflow(tag_pattern, shipyard_version)).map_err(|error| {
        CliFailure::new(1, format!("failed to write {}: {error}", target.display()))
    })?;
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("path".to_owned(), Value::from(target.display().to_string()));
        data.insert("overwrote".to_owned(), Value::from(overwrote));
        data.insert(
            "shipyard_version".to_owned(),
            Value::from(shipyard_version.to_owned()),
        );
        data.insert(
            "tag_pattern".to_owned(),
            Value::from(tag_pattern.to_owned()),
        );
        write_json_envelope(stdout, "release-bot:hook:install", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        let verb = if overwrote { "Overwrote" } else { "Wrote" };
        writeln!(stdout, "{verb} {}", target.display())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout, "  - fires on tag push matching {tag_pattern:?}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(
            stdout,
            "  - installs shipyard {shipyard_version} before running the hook"
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn hook_run<W: Write>(
    stdout: &mut W,
    config: &LoadedConfig,
    cwd: &Path,
    tag: Option<&str>,
    json_mode: bool,
) -> Result<ExitCode, CliFailure> {
    let hook_config = load_hook_config(config);
    if !hook_config.enabled {
        let result = HookResult {
            skipped_reason: Some(String::from("hook disabled in config")),
            ..HookResult::default()
        };
        render_hook_run(stdout, tag.unwrap_or(""), &result, json_mode)?;
        return Ok(ExitCode::SUCCESS);
    }
    let resolved_tag = tag.map(str::to_owned).or_else(|| {
        std::env::var("GITHUB_REF")
            .ok()
            .and_then(|value| value.strip_prefix("refs/tags/").map(str::to_owned))
    });
    let Some(resolved_tag) = resolved_tag else {
        return Err(CliFailure::new(
            2,
            "--tag is required (or set GITHUB_REF=refs/tags/<tag>).",
        ));
    };
    let result = run_hook(&hook_config, &resolved_tag, cwd);
    render_hook_run(stdout, &resolved_tag, &result, json_mode)?;
    Ok(if result.error.is_some() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn render_hook_run<W: Write>(
    stdout: &mut W,
    tag: &str,
    result: &HookResult,
    json_mode: bool,
) -> Result<(), CliFailure> {
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("tag".to_owned(), Value::from(tag.to_owned()));
        data.insert("ran_command".to_owned(), Value::from(result.ran_command));
        data.insert("command_exit".to_owned(), Value::from(result.command_exit));
        data.insert(
            "watched_diffed".to_owned(),
            Value::Array(
                result
                    .watched_diffed
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect(),
            ),
        );
        data.insert("committed".to_owned(), Value::from(result.committed));
        data.insert("pushed".to_owned(), Value::from(result.pushed));
        data.insert(
            "attempts".to_owned(),
            Value::from(u64::from(result.attempts)),
        );
        data.insert(
            "skipped_reason".to_owned(),
            optional_string(result.skipped_reason.as_deref()),
        );
        data.insert("error".to_owned(), optional_string(result.error.as_deref()));
        write_json_envelope(stdout, "release-bot:hook:run", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    if let Some(reason) = &result.skipped_reason {
        writeln!(stdout, "skipped: {reason}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if let Some(error) = &result.error {
        writeln!(stdout, "error: {error}")
            .map_err(|io_error| CliFailure::new(1, io_error.to_string()))?;
    } else if result.pushed {
        writeln!(stdout, "pushed docs sync for {tag}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if result.committed {
        writeln!(stdout, "committed docs sync for {tag}; push not needed")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "no watched diffs for {tag}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn describe_state(state: &ReleaseBotState) -> Vec<String> {
    let mut lines = vec![format!("repo: {}", state.repo_slug)];
    if state.secret_present {
        let when = state.secret_updated_at.map_or_else(
            || String::from("unknown"),
            |time| time.format("%Y-%m-%d").to_string(),
        );
        lines.push(format!("RELEASE_BOT_TOKEN: configured (set {when})"));
    } else {
        lines.push(String::from("RELEASE_BOT_TOKEN: missing"));
    }
    if let Some(conclusion) = &state.last_auto_release_conclusion {
        if state.last_auto_release_error_signature.as_deref() == Some("auth") {
            lines.push(format!(
                "last auto-release: {conclusion} (rejected at actions/checkout - PAT scope or secret value drift)"
            ));
        } else {
            lines.push(format!("last auto-release: {conclusion}"));
        }
    }
    if !state.other_repos_with_secret.is_empty() {
        lines.push(format!(
            "other repos with RELEASE_BOT_TOKEN: {}",
            state.other_repos_with_secret.join(", ")
        ));
    }
    lines
}

struct SetupPlan {
    suggested_pat_name: String,
    reasoning: String,
}

fn plan_setup(state: &ReleaseBotState, shared_name: Option<&str>) -> SetupPlan {
    if let Some(name) = shared_name {
        return SetupPlan {
            suggested_pat_name: name.to_owned(),
            reasoning: format!(
                "Using shared PAT name '{name}' as requested. Include every Shipyard consumer repo in its Selected repositories list."
            ),
        };
    }
    let repo_name = state
        .repo_slug
        .split_once('/')
        .map_or(state.repo_slug.as_str(), |(_, repo)| repo)
        .to_lowercase();
    let suggested_pat_name = format!("{repo_name}-release-bot");
    let reasoning = if state.other_repos_with_secret.is_empty() || state.secret_present {
        String::from(
            "A fresh per-project PAT is the least-privilege default - one compromised token affects one repo.",
        )
    } else {
        format!(
            "You already have RELEASE_BOT_TOKEN on another repo ({}). Reusing that PAT avoids a second rotation point.",
            state.other_repos_with_secret[0]
        )
    };
    SetupPlan {
        suggested_pat_name,
        reasoning,
    }
}

fn render_pat_creation_url(owner: &str, repo: &str, pat_name: &str) -> String {
    format!(
        "https://github.com/settings/personal-access-tokens/new?type=beta&name={}&description={}&expires_in=365&target_name={}",
        url_component(pat_name),
        url_component(&format!("Shipyard release bot for {owner}/{repo}")),
        url_component(owner),
    )
}

fn render_workflow(tag_pattern: &str, shipyard_version: &str) -> String {
    format!(
        r#"name: Post-tag docs sync

# Installed by `shipyard release-bot hook install`. Shipyard-owned file:
# re-running the install command overwrites this file in place.

on:
  push:
    tags: ["{tag_pattern}"]

concurrency:
  group: shipyard-post-tag-sync
  cancel-in-progress: false

permissions:
  contents: write

env:
  SHIPYARD_VERSION: "{shipyard_version}"

jobs:
  sync:
    name: Regenerate docs for ${{{{ github.ref_name }}}}
    runs-on: ubuntu-latest
    steps:
      - name: Checkout main with full history
        uses: actions/checkout@v5
        with:
          ref: main
          fetch-depth: 0
          fetch-tags: true
          persist-credentials: true
          token: ${{{{ secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN }}}}

      - name: Install shipyard (pinned)
        shell: bash
        run: |
          set -euo pipefail
          curl -fsSL "https://generouscorp.com/Shipyard/install.sh" | SHIPYARD_VERSION="$SHIPYARD_VERSION" bash
          shipyard --version

      - name: Run post-tag docs sync
        shell: bash
        env:
          GITHUB_TOKEN: ${{{{ secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN }}}}
        run: |
          tag="${{{{GITHUB_REF#refs/tags/}}}}"
          shipyard release-bot hook run --tag "$tag"
"#
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HookConfig {
    enabled: bool,
    command: String,
    watch: Vec<String>,
    trailers: Vec<String>,
    only_for_tag_pattern: String,
    max_push_attempts: u32,
    bot_name: String,
    bot_email: String,
    remote: String,
    branch: String,
    // How the bot commit lands on `branch`:
    //   "direct" (default) — push --ff-only straight to `branch` (today's
    //     behavior). Incompatible with a GitHub "Require merge queue" rule,
    //     which rejects ALL direct pushes to the protected branch.
    //   "pr" — push to a dedicated branch, open a PR, and arm auto-merge so the
    //     commit lands THROUGH the merge queue. Required when `branch` enforces
    //     a merge queue.
    push_mode: String,
    // PR-route branch name prefix (push_mode = "pr"); the tag is appended.
    pr_branch_prefix: String,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: String::from("shipyard changelog regenerate"),
            watch: vec![String::from("CHANGELOG.md")],
            trailers: default_hook_trailers(),
            only_for_tag_pattern: String::from("v*"),
            max_push_attempts: 5,
            bot_name: String::from("shipyard-release-bot"),
            bot_email: String::from("shipyard-release-bot@users.noreply.github.com"),
            remote: String::from("origin"),
            branch: String::from("main"),
            push_mode: String::from("direct"),
            pr_branch_prefix: String::from("release/post-tag-sync"),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HookResult {
    ran_command: bool,
    command_exit: i32,
    watched_diffed: Vec<String>,
    committed: bool,
    pushed: bool,
    attempts: u32,
    skipped_reason: Option<String>,
    error: Option<String>,
}

fn load_hook_config(config: &LoadedConfig) -> HookConfig {
    let Some(section) = config
        .get("release.post_tag_hook")
        .and_then(toml::Value::as_table)
    else {
        return HookConfig::default();
    };
    let mut cfg = HookConfig {
        enabled: section
            .get("enabled")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false),
        command: section
            .get("command")
            .and_then(toml::Value::as_str)
            .unwrap_or("shipyard changelog regenerate")
            .to_owned(),
        watch: string_array(section, "watch").unwrap_or_else(|| vec![String::from("CHANGELOG.md")]),
        trailers: string_array(section, "trailers").unwrap_or_else(default_hook_trailers),
        only_for_tag_pattern: section
            .get("only_for_tag_pattern")
            .and_then(toml::Value::as_str)
            .unwrap_or("v*")
            .to_owned(),
        max_push_attempts: section
            .get("max_push_attempts")
            .and_then(toml::Value::as_integer)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(5),
        remote: section
            .get("remote")
            .and_then(toml::Value::as_str)
            .unwrap_or("origin")
            .to_owned(),
        branch: section
            .get("branch")
            .and_then(toml::Value::as_str)
            .unwrap_or("main")
            .to_owned(),
        push_mode: section
            .get("push_mode")
            .and_then(toml::Value::as_str)
            .unwrap_or("direct")
            .to_owned(),
        pr_branch_prefix: section
            .get("pr_branch_prefix")
            .and_then(toml::Value::as_str)
            .unwrap_or("release/post-tag-sync")
            .to_owned(),
        ..HookConfig::default()
    };
    if let Some(identity) = section.get("bot_identity").and_then(toml::Value::as_table) {
        if let Some(name) = identity.get("name").and_then(toml::Value::as_str) {
            name.clone_into(&mut cfg.bot_name);
        }
        if let Some(email) = identity.get("email").and_then(toml::Value::as_str) {
            email.clone_into(&mut cfg.bot_email);
        }
    }
    cfg
}

fn run_hook(config: &HookConfig, tag: &str, cwd: &Path) -> HookResult {
    let mut result = HookResult::default();
    if !config.enabled {
        result.skipped_reason = Some(String::from("hook disabled in config"));
        return result;
    }
    if !glob_matches(&config.only_for_tag_pattern, tag) {
        result.skipped_reason = Some(format!(
            "tag {tag:?} does not match {:?}",
            config.only_for_tag_pattern
        ));
        return result;
    }
    let command = Command::new("sh")
        .arg("-c")
        .arg(&config.command)
        .current_dir(cwd)
        .output();
    result.ran_command = true;
    let output = match command {
        Ok(output) => output,
        Err(error) => {
            result.command_exit = 127;
            result.error = Some(format!(
                "command {:?} failed to spawn: {error}",
                config.command
            ));
            return result;
        }
    };
    result.command_exit = output.status.code().unwrap_or(1);
    if !output.status.success() {
        result.error = Some(format!(
            "command {:?} exited {}\nstdout: {}\nstderr: {}",
            config.command,
            result.command_exit,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
        return result;
    }

    result.watched_diffed = watched_diffs(cwd, &config.watch);
    if result.watched_diffed.is_empty() {
        return result;
    }
    let diffed = result.watched_diffed.clone();
    if let Err(error) = commit_and_push_docs(cwd, config, tag, &diffed, &mut result) {
        result.error = Some(error);
    }
    result
}

fn commit_and_push_docs(
    cwd: &Path,
    config: &HookConfig,
    tag: &str,
    diffed: &[String],
    result: &mut HookResult,
) -> Result<(), String> {
    let mut add_args = vec![String::from("add"), String::from("--")];
    add_args.extend(diffed.iter().cloned());
    run_git_owned(cwd, &add_args)?;
    run_git(cwd, &["config", "user.name", &config.bot_name])?;
    run_git(cwd, &["config", "user.email", &config.bot_email])?;
    let mut commit_args = vec![
        "commit".to_owned(),
        "-m".to_owned(),
        format!("docs: regenerate changelog for {tag} [skip ci]"),
        "-m".to_owned(),
        String::from(
            "Automated by shipyard release-bot hook run after tag push, so CHANGELOG.md and the GitHub Release page stay in sync.",
        ),
        "-m".to_owned(),
        String::new(),
    ];
    for trailer in &config.trailers {
        commit_args.push("-m".to_owned());
        commit_args.push(trailer.clone());
    }
    run_git_owned(cwd, &commit_args)?;
    result.committed = true;
    match config.push_mode.as_str() {
        "pr" => push_via_pr(cwd, config, tag, result),
        _ => push_direct(cwd, config, result),
    }
}

// push_mode = "direct" (default): --ff-only push straight to `branch`, with a
// fetch+rebase retry on a lost race. A GitHub "Require merge queue" rule rejects
// this (it blocks ALL direct pushes to the protected branch) — use "pr" there.
fn push_direct(cwd: &Path, config: &HookConfig, result: &mut HookResult) -> Result<(), String> {
    for attempt in 1..=config.max_push_attempts.max(1) {
        result.attempts = attempt;
        if run_git(
            cwd,
            &["push", &config.remote, &format!("HEAD:{}", config.branch)],
        )
        .is_ok()
        {
            result.pushed = true;
            return Ok(());
        }
        let _ = run_git(cwd, &["rebase", "--abort"]);
        run_git(cwd, &["fetch", &config.remote, &config.branch])?;
        run_git(
            cwd,
            &["rebase", &format!("{}/{}", config.remote, config.branch)],
        )?;
    }
    Err(format!(
        "git push failed after {} attempt(s)",
        config.max_push_attempts.max(1)
    ))
}

// push_mode = "pr": push the bot commit to a dedicated branch, open a PR, and
// arm auto-merge so it lands THROUGH the merge queue (the direct push a
// "Require merge queue" rule would reject). `gh` authenticates via GH_TOKEN in
// the caller's environment — the same mechanism shipyard's other gh usage
// (src/pr.rs) relies on.
fn push_via_pr(
    cwd: &Path,
    config: &HookConfig,
    tag: &str,
    result: &mut HookResult,
) -> Result<(), String> {
    let pr_branch = format!("{}-{}", config.pr_branch_prefix, sanitize_ref_component(tag));
    // Force-push the bot-owned throwaway branch (a stale branch from an aborted
    // run must not block a fresh sync). Safe — only the bot writes it.
    run_git(
        cwd,
        &["push", "--force", &config.remote, &format!("HEAD:{pr_branch}")],
    )?;
    result.pushed = true;
    let title = format!("docs: regenerate changelog for {tag}");
    let body = format!(
        "Automated by shipyard release-bot hook run after tag `{tag}` — routes the CHANGELOG.md regeneration through a PR so it lands via the merge queue instead of a direct push to `{}`.",
        config.branch
    );
    let created = run_gh(
        cwd,
        &[
            "pr", "create", "--base", &config.branch, "--head", &pr_branch, "--title", &title,
            "--body", &body,
        ],
    );
    if let Err(error) = created {
        // A concurrent/prior run already opened it — GitHub rejects a second PR
        // from the same head. Treat that as success and just (re-)arm auto-merge.
        if !error.contains("already exists") && !error.contains("a pull request for branch") {
            return Err(format!("gh pr create failed: {error}"));
        }
    }
    // Arm GitHub-native auto-merge with --merge (never --squash: the bot commit
    // carries the `Release: skip` trailer that must survive to prevent a
    // recursive auto-release). When a merge queue is enabled, --auto enqueues it.
    run_gh(cwd, &["pr", "merge", &pr_branch, "--auto", "--merge"])
        .map_err(|error| format!("gh pr merge --auto failed: {error}"))?;
    Ok(())
}

// Make a tag safe as a git ref component (tags like `v1.2.3` are already safe;
// this guards odd characters so `pr_branch` is always a valid ref).
fn sanitize_ref_component(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn watched_diffs(cwd: &Path, paths: &[String]) -> Vec<String> {
    let mut diffed = Vec::new();
    for path in paths {
        let status = Command::new("git")
            .args(["status", "--porcelain", "--", path])
            .current_dir(cwd)
            .output()
            .ok()
            .is_some_and(|output| !output.stdout.is_empty());
        if status {
            diffed.push(path.clone());
            continue;
        }
        let changed = Command::new("git")
            .args(["diff", "--quiet", "HEAD", "--", path])
            .current_dir(cwd)
            .status()
            .ok()
            .is_some_and(|status| !status.success());
        if changed {
            diffed.push(path.clone());
        }
    }
    diffed
}

fn list_secrets(repo_slug: &str, gh_command: Option<&Path>) -> Option<Vec<Value>> {
    let output = gh(gh_command)
        .args([
            "api",
            &format!("repos/{repo_slug}/actions/secrets"),
            "--paginate",
        ])
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    let data = serde_json::from_slice::<Value>(&output.stdout).ok()?;
    data.get("secrets")?.as_array().cloned()
}

fn last_workflow_run(repo_slug: &str, workflow: &str, gh_command: Option<&Path>) -> Option<Value> {
    let output = gh(gh_command)
        .args([
            "run",
            "list",
            "--workflow",
            workflow,
            "--repo",
            repo_slug,
            "--limit",
            "1",
            "--json",
            "databaseId,status,conclusion,createdAt",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice::<Vec<Value>>(&output.stdout)
        .ok()?
        .into_iter()
        .next()
}

fn detect_checkout_auth_failure(
    repo_slug: &str,
    run_id: u64,
    gh_command: Option<&Path>,
) -> Option<String> {
    let output = gh(gh_command)
        .args([
            "run",
            "view",
            &run_id.to_string(),
            "--repo",
            repo_slug,
            "--log-failed",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let log = String::from_utf8_lossy(&output.stdout).to_lowercase();
    (log.contains("could not read username") || log.contains("authentication failed"))
        .then(|| String::from("auth"))
}

fn set_secret(repo_slug: &str, token: &str, gh_command: Option<&Path>) -> Result<(), CliFailure> {
    let mut command = gh(gh_command);
    command.args([
        "secret",
        "set",
        "RELEASE_BOT_TOKEN",
        "--repo",
        repo_slug,
        "--body",
        "-",
    ]);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| CliFailure::new(1, format!("couldn't run `gh secret set`: {error}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| CliFailure::new(1, "failed to open gh stdin"))?
        .write_all(token.as_bytes())
        .map_err(|error| CliFailure::new(1, format!("failed to write token to gh: {error}")))?;
    let output = child.wait_with_output().map_err(|error| {
        CliFailure::new(1, format!("failed waiting for gh secret set: {error}"))
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!(
                "gh secret set failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ))
    }
}

fn verify_token(repo_slug: &str, gh_command: Option<&Path>) -> Result<String, String> {
    let baseline = last_workflow_run(repo_slug, "auto-release.yml", gh_command)
        .and_then(|run| run.get("databaseId").and_then(Value::as_u64));
    let dispatch = gh(gh_command)
        .args([
            "workflow",
            "run",
            "auto-release.yml",
            "--repo",
            repo_slug,
            "--ref",
            "main",
        ])
        .output()
        .map_err(|error| format!("couldn't dispatch verification workflow: {error}"))?;
    if !dispatch.status.success() {
        return Err(format!(
            "gh workflow run failed: {}",
            String::from_utf8_lossy(&dispatch.stderr).trim()
        ));
    }
    for _ in 0..30 {
        if let Some(run) = last_workflow_run(repo_slug, "auto-release.yml", gh_command)
            && run.get("status").and_then(Value::as_str) == Some("completed")
        {
            let run_id = run.get("databaseId").and_then(Value::as_u64);
            if baseline.is_none() || run_id > baseline {
                return Ok(run
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned());
            }
        }
        std::thread::sleep(Duration::from_secs(10));
    }
    Err(String::from(
        "verification workflow didn't complete in 5 min",
    ))
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("failed to run git {}: {error}", args.join(" ")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn run_git_owned(cwd: &Path, args: &[String]) -> Result<(), String> {
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_git(cwd, &arg_refs)
}

// Run `gh` in `cwd` via shipyard's ambient GhClient (same auth path as
// src/pr.rs). Used by push_via_pr to open + auto-merge the changelog PR.
fn run_gh(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let output = gh(None)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("failed to run gh {}: {error}", args.join(" ")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn gh(gh_command: Option<&Path>) -> Command {
    GhClient::ambient()
        .prepare_command(
            Path::new("."),
            gh_command,
            GhSupervision::Unsupervised,
            GhAuthPolicy::AmbientOnly,
        )
        .expect("ambient gh command preparation should not fail")
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn optional_string(value: Option<&str>) -> Value {
    value.map_or(Value::Null, Value::from)
}

fn string_array(table: &toml::Table, key: &str) -> Option<Vec<String>> {
    table.get(key)?.as_array().map(|items| {
        items
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect()
    })
}

fn default_hook_trailers() -> Vec<String> {
    vec![
        String::from("Version-Bump: sdk=skip reason=\"docs-only automated regeneration\""),
        String::from("Skill-Update: skip skill=ci reason=\"no workflow shape change\""),
        String::from("Release: skip reason=\"bot commit; prevent recursive auto-release\""),
    ]
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    glob_matches_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_matches_bytes(pattern: &[u8], text: &[u8]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            glob_matches_bytes(&pattern[1..], text)
                || (!text.is_empty() && glob_matches_bytes(pattern, &text[1..]))
        }
        (Some(pattern_byte), Some(text_byte)) if pattern_byte == text_byte => {
            glob_matches_bytes(&pattern[1..], &text[1..])
        }
        _ => false,
    }
}

fn url_component(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![char::from(byte)]
            }
            b' ' => vec!['+'],
            other => format!("%{other:02X}").chars().collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::ExitCode;

    use chrono::{TimeZone, Utc};
    use serde_json::Value;

    use super::*;
    use crate::config::LocalOverlaySource;

    fn config_from_toml(contents: &str) -> LoadedConfig {
        LoadedConfig {
            data: contents.parse::<toml::Table>().expect("config TOML"),
            global_dir: PathBuf::from("/tmp/global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn empty_config() -> LoadedConfig {
        config_from_toml("")
    }

    fn decode_envelope(output: &[u8]) -> Value {
        let text = std::str::from_utf8(output).expect("utf8");
        serde_json::from_str(text).expect("json")
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, contents).expect("write executable");
        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod");
    }

    #[cfg(unix)]
    fn fake_gh(root: &Path) -> PathBuf {
        let script = root.join("gh");
        write_executable(
            &script,
            r#"#!/bin/sh
case "$*" in
  api\ repos/owner/repo/actions/secrets*)
    printf '%s\n' '{"secrets":[{"name":"RELEASE_BOT_TOKEN","updated_at":"2026-04-25T09:30:00Z"}]}'
    ;;
  api\ repos/owner/other/actions/secrets*)
    printf '%s\n' '{"secrets":[{"name":"RELEASE_BOT_TOKEN","updated_at":"2026-04-25T08:00:00Z"}]}'
    ;;
  run\ list\ --workflow\ auto-release.yml\ --repo\ owner/repo*)
    printf '%s\n' '[{"databaseId":123,"status":"completed","conclusion":"failure","createdAt":"2026-04-25T10:00:00Z"}]'
    ;;
  run\ view\ 123\ --repo\ owner/repo\ --log-failed*)
    printf '%s\n' 'fatal: Authentication failed'
    ;;
  *)
    printf 'unexpected gh args: %s\n' "$*" >&2
    exit 2
    ;;
esac
"#,
        );
        script
    }

    #[cfg(unix)]
    fn git(cwd: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git spawn");
        assert!(
            output.status.success(),
            "git {} failed\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn status_json_matches_release_bot_contract() {
        let state = ReleaseBotState {
            repo_slug: String::from("owner/repo"),
            secret_present: true,
            secret_updated_at: Some(Utc.with_ymd_and_hms(2026, 4, 25, 9, 30, 0).unwrap()),
            last_auto_release_conclusion: Some(String::from("failure")),
            last_auto_release_error_signature: Some(String::from("auth")),
            other_repos_with_secret: vec![String::from("owner/other")],
        };
        let mut output = Vec::new();

        render_status(&mut output, &state, true).expect("status");

        let envelope = decode_envelope(&output);
        assert_eq!(envelope["command"], "release-bot:status");
        assert_eq!(envelope["repo"], "owner/repo");
        assert_eq!(envelope["secret_present"], true);
        assert_eq!(envelope["secret_updated_at"], "2026-04-25T09:30:00+00:00");
        assert_eq!(envelope["last_auto_release_conclusion"], "failure");
        assert_eq!(envelope["last_auto_release_error_signature"], "auth");
        assert_eq!(envelope["other_repos_with_secret"][0], "owner/other");
    }

    #[test]
    fn human_status_adds_auth_failure_diagnosis() {
        let state = ReleaseBotState {
            repo_slug: String::from("owner/repo"),
            secret_present: true,
            secret_updated_at: None,
            last_auto_release_conclusion: Some(String::from("failure")),
            last_auto_release_error_signature: Some(String::from("auth")),
            other_repos_with_secret: Vec::new(),
        };
        let mut output = Vec::new();

        render_status(&mut output, &state, false).expect("status");

        let text = String::from_utf8(output).expect("utf8");
        assert!(text.contains("RELEASE_BOT_TOKEN: configured"));
        assert!(text.contains("stored token is being rejected by actions/checkout"));
        assert!(text.contains("shipyard release-bot setup --reconfigure"));
    }

    #[test]
    fn hook_install_writes_workflow_and_json_envelope() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut output = Vec::new();

        let exit = hook_install(&mut output, temp.path(), "shipyard-v*", "v0.50.0", true)
            .expect("hook install");

        assert_eq!(exit, ExitCode::SUCCESS);
        let workflow = temp
            .path()
            .join(".github")
            .join("workflows")
            .join(POST_TAG_WORKFLOW);
        let contents = fs::read_to_string(&workflow).expect("workflow");
        assert!(contents.contains(r#"tags: ["shipyard-v*"]"#));
        assert!(contents.contains(r#"SHIPYARD_VERSION: "v0.50.0""#));
        assert!(contents.contains("shipyard release-bot hook run --tag"));
        let envelope = decode_envelope(&output);
        assert_eq!(envelope["command"], "release-bot:hook:install");
        assert_eq!(envelope["overwrote"], false);
        assert_eq!(envelope["shipyard_version"], "v0.50.0");
        assert_eq!(envelope["tag_pattern"], "shipyard-v*");
    }

    #[test]
    fn render_workflow_uses_release_bot_token_fallback() {
        let workflow = render_workflow("v*", "v0.51.0");

        assert!(workflow.contains("secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN"));
        assert!(workflow.contains(r#"SHIPYARD_VERSION: "v0.51.0""#));
        assert!(workflow.contains(r#"tags: ["v*"]"#));
        assert!(workflow.contains("curl -fsSL \"https://generouscorp.com/Shipyard/install.sh\""));
    }

    #[test]
    fn hook_config_parses_release_post_tag_hook_section() {
        let config = config_from_toml(
            r#"
[release.post_tag_hook]
enabled = true
command = "make docs"
watch = ["CHANGELOG.md", "docs/release.md"]
trailers = ["Release: skip reason=\"bot\""]
only_for_tag_pattern = "shipyard-v*"
max_push_attempts = 2
remote = "upstream"
branch = "stable"

[release.post_tag_hook.bot_identity]
name = "release bot"
email = "bot@example.com"
"#,
        );

        let parsed = load_hook_config(&config);

        assert!(parsed.enabled);
        assert_eq!(parsed.command, "make docs");
        assert_eq!(
            parsed.watch,
            vec![
                String::from("CHANGELOG.md"),
                String::from("docs/release.md")
            ]
        );
        assert_eq!(
            parsed.trailers,
            vec![String::from("Release: skip reason=\"bot\"")]
        );
        assert_eq!(parsed.only_for_tag_pattern, "shipyard-v*");
        assert_eq!(parsed.max_push_attempts, 2);
        assert_eq!(parsed.remote, "upstream");
        assert_eq!(parsed.branch, "stable");
        assert_eq!(parsed.bot_name, "release bot");
        assert_eq!(parsed.bot_email, "bot@example.com");
        // push_mode is unset in this section → defaults to the direct push.
        assert_eq!(parsed.push_mode, "direct");
        assert_eq!(parsed.pr_branch_prefix, "release/post-tag-sync");
    }

    #[test]
    fn hook_config_parses_pr_push_mode() {
        let config = config_from_toml(
            r#"
[release.post_tag_hook]
enabled = true
push_mode = "pr"
pr_branch_prefix = "bot/changelog"
"#,
        );

        let parsed = load_hook_config(&config);

        assert!(parsed.enabled);
        assert_eq!(parsed.push_mode, "pr");
        assert_eq!(parsed.pr_branch_prefix, "bot/changelog");
    }

    #[test]
    fn hook_config_defaults_push_mode_to_direct() {
        // An empty hook section (or none) keeps today's behavior — no surprise
        // switch to the PR route.
        assert_eq!(HookConfig::default().push_mode, "direct");
    }

    #[test]
    fn sanitize_ref_component_keeps_semver_tags_and_scrubs_odd_chars() {
        assert_eq!(sanitize_ref_component("v1.2.3"), "v1.2.3");
        assert_eq!(sanitize_ref_component("plugin-v0.4.0"), "plugin-v0.4.0");
        // spaces / slashes / other ref-hostile chars become '-'.
        assert_eq!(sanitize_ref_component("v1.0 rc/1"), "v1.0-rc-1");
    }

    #[test]
    fn hook_run_disabled_json_does_not_require_tag() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = empty_config();
        let mut output = Vec::new();

        let exit = hook_run(&mut output, &config, temp.path(), None, true).expect("hook run");

        assert_eq!(exit, ExitCode::SUCCESS);
        let envelope = decode_envelope(&output);
        assert_eq!(envelope["command"], "release-bot:hook:run");
        assert_eq!(envelope["tag"], "");
        assert_eq!(envelope["ran_command"], false);
        assert_eq!(envelope["skipped_reason"], "hook disabled in config");
    }

    #[test]
    fn hook_run_enabled_requires_tag_or_github_ref() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = config_from_toml(
            r"
[release.post_tag_hook]
enabled = true
",
        );
        let mut output = Vec::new();

        let error = hook_run(&mut output, &config, temp.path(), None, true).expect_err("tag error");

        assert_eq!(error.code, 2);
        assert!(error.message.contains("--tag is required"));
    }

    #[cfg(unix)]
    #[test]
    fn detect_state_reads_secret_siblings_and_auth_failures_from_gh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = fake_gh(temp.path());

        let state = detect_state("owner/repo", &[String::from("owner/other")], Some(&gh));

        assert!(state.secret_present);
        assert_eq!(
            state.secret_updated_at.expect("updated").to_rfc3339(),
            "2026-04-25T09:30:00+00:00"
        );
        assert_eq!(
            state.last_auto_release_conclusion.as_deref(),
            Some("failure")
        );
        assert_eq!(
            state.last_auto_release_error_signature.as_deref(),
            Some("auth")
        );
        assert_eq!(
            state.other_repos_with_secret,
            vec![String::from("owner/other")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn setup_existing_secret_without_reconfigure_exits_before_prompt() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = fake_gh(temp.path());
        let siblings = [String::from("owner/other")];
        let mut output = Vec::new();

        let exit = setup(
            &mut output,
            "owner/repo",
            &SetupOptions {
                shared_name: None,
                paste: false,
                siblings: &siblings,
                verify: false,
                reconfigure: false,
            },
            Some(&gh),
        )
        .expect("setup");

        assert_eq!(exit, ExitCode::SUCCESS);
        let text = String::from_utf8(output).expect("utf8");
        assert!(text.contains("RELEASE_BOT_TOKEN: configured"));
        assert!(text.contains("Pass --reconfigure to replace it"));
        assert!(text.contains("owner/other"));
    }

    #[test]
    fn setup_plan_honors_shared_pat_name() {
        let state = ReleaseBotState {
            repo_slug: String::from("owner/repo"),
            secret_present: false,
            secret_updated_at: None,
            last_auto_release_conclusion: None,
            last_auto_release_error_signature: None,
            other_repos_with_secret: vec![String::from("owner/other")],
        };

        let plan = plan_setup(&state, Some("shared-release-token"));

        assert_eq!(plan.suggested_pat_name, "shared-release-token");
        assert!(plan.reasoning.contains("shared PAT name"));
    }

    #[cfg(unix)]
    #[test]
    fn release_bot_status_command_uses_detected_repo_slug() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).expect("repo");
        git(&repo, &["init"]);
        git(
            &repo,
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let gh = fake_gh(temp.path());
        let config = empty_config();
        let mut output = Vec::new();

        let exit = release_bot_command_with(
            ReleaseBotCommand::Status {
                siblings: vec![String::from("owner/other")],
            },
            &config,
            &repo,
            true,
            &mut output,
            Some(&gh),
        )
        .expect("status");

        assert_eq!(exit, ExitCode::SUCCESS);
        let envelope = decode_envelope(&output);
        assert_eq!(envelope["command"], "release-bot:status");
        assert_eq!(envelope["repo"], "owner/repo");
        assert_eq!(envelope["last_auto_release_error_signature"], "auth");
    }

    #[cfg(unix)]
    #[test]
    fn run_hook_reports_command_failures() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = HookConfig {
            enabled: true,
            command: String::from("exit 7"),
            only_for_tag_pattern: String::from("v*"),
            ..HookConfig::default()
        };

        let result = run_hook(&config, "v0.50.0", temp.path());

        assert!(result.ran_command);
        assert_eq!(result.command_exit, 7);
        assert!(result.error.expect("error").contains("exited 7"));
    }

    #[cfg(unix)]
    #[test]
    fn run_hook_commits_and_pushes_watched_docs_diff() {
        let temp = tempfile::tempdir().expect("tempdir");
        let remote = temp.path().join("origin.git");
        let repo = temp.path().join("repo");
        std::process::Command::new("git")
            .args(["init", "--bare", remote.to_str().expect("remote path")])
            .output()
            .expect("git init bare");
        fs::create_dir(&repo).expect("repo");
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "test user"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        fs::write(repo.join("CHANGELOG.md"), "# Changelog\n").expect("changelog");
        git(&repo, &["add", "CHANGELOG.md"]);
        git(&repo, &["commit", "-m", "initial"]);
        git(&repo, &["branch", "-M", "main"]);
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("remote path"),
            ],
        );
        git(&repo, &["push", "origin", "main"]);
        let config = HookConfig {
            enabled: true,
            command: String::from("printf '\\nentry\\n' >> CHANGELOG.md"),
            max_push_attempts: 1,
            ..HookConfig::default()
        };

        let result = run_hook(&config, "v0.50.0", &repo);

        assert_eq!(result.error, None);
        assert!(result.ran_command);
        assert_eq!(result.command_exit, 0);
        assert_eq!(result.watched_diffed, vec![String::from("CHANGELOG.md")]);
        assert!(result.committed);
        assert!(result.pushed);
        assert_eq!(result.attempts, 1);
        let log = std::process::Command::new("git")
            .args(["log", "--oneline", "-1"])
            .current_dir(&repo)
            .output()
            .expect("git log");
        assert!(String::from_utf8_lossy(&log.stdout).contains("docs: regenerate changelog"));
    }

    #[test]
    fn run_hook_skips_nonmatching_tags_before_command() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = HookConfig {
            enabled: true,
            command: String::from("exit 99"),
            only_for_tag_pattern: String::from("v*"),
            ..HookConfig::default()
        };

        let result = run_hook(&config, "nightly-2026-04-25", temp.path());

        assert!(!result.ran_command);
        assert_eq!(
            result.skipped_reason.as_deref(),
            Some("tag \"nightly-2026-04-25\" does not match \"v*\"")
        );
    }

    #[test]
    fn glob_matching_covers_release_tag_patterns() {
        assert!(glob_matches("v*", "v0.50.0"));
        assert!(glob_matches("shipyard-v*", "shipyard-v0.50.0"));
        assert!(glob_matches("*-stable", "shipyard-stable"));
        assert!(!glob_matches("shipyard-v*", "gui-v0.50.0"));
    }

    #[test]
    fn pat_url_escapes_generated_fields() {
        let url = render_pat_creation_url("owner name", "repo", "shipyard release bot");

        assert!(url.contains("name=shipyard+release+bot"));
        assert!(url.contains("description=Shipyard+release+bot+for+owner+name%2Frepo"));
        assert!(url.contains("target_name=owner+name"));
    }
}
