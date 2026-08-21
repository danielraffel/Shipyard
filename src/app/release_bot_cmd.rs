use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    CliFailure,
    auto_merge_cmd::{
        AutoMergeOutcome, AutoMergeRequest, execute_auto_merge, target_requires_merge_queue,
    },
    branch_cmd::detect_repo_from_remote,
    cli::{MergeMethod, ReleaseBotCommand, ReleaseBotHookCommand},
};
use crate::config::LoadedConfig;
use crate::evidence::canonical_repository;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::merge_queue_control::preflight_mutation_authority;
use crate::output::write_json_envelope;
use crate::ship_state::{ShipState, ShipStateStore};

const POST_TAG_WORKFLOW: &str = "post-tag-sync.yml";

pub(super) fn release_bot_command<W: Write>(
    command: ReleaseBotCommand,
    mode: RuntimeMode,
    cwd: &Path,
    state_root: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    release_bot_command_with(
        command, mode, &config, cwd, state_root, json_mode, stdout, None,
    )
}

#[allow(clippy::too_many_arguments)]
fn release_bot_command_with<W: Write>(
    command: ReleaseBotCommand,
    mode: RuntimeMode,
    config: &LoadedConfig,
    cwd: &Path,
    state_root: &Path,
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
                config,
                cwd,
                tag_pattern.as_deref().unwrap_or("v*"),
                shipyard_version
                    .as_deref()
                    .unwrap_or(env!("CARGO_PKG_VERSION")),
                json_mode,
                gh_command,
            ),
            ReleaseBotHookCommand::Run { tag } => hook_run(
                stdout,
                config,
                cwd,
                state_root,
                mode,
                tag.as_deref(),
                json_mode,
            ),
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
    config: &LoadedConfig,
    cwd: &Path,
    tag_pattern: &str,
    shipyard_version: &str,
    json_mode: bool,
    gh_command: Option<&Path>,
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
    let hook_config = load_hook_config(config);
    let signing_setup_script = hook_config
        .ssh_signing_setup_script
        .as_deref()
        .map(validate_workflow_script_path)
        .transpose()
        .map_err(|error| CliFailure::new(1, error))?;
    let trusted_config = LoadedConfig::load_machine_global_from_dir(config.global_dir.clone())
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to load trusted queue authority policy: {error}"),
            )
        })?;
    let runner = if let Some(machine) = trusted_config.get_str("merge_queue.mutation_machine") {
        crate::runner_provision::validate_machine_tag(machine)
            .map_err(|error| CliFailure::new(1, error))?;
        let repo_slug = detect_repo_from_remote(cwd, None).ok_or_else(|| {
            CliFailure::new(
                1,
                "can't detect repository name for queue-authority runner label",
            )
        })?;
        let repo = repo_slug.rsplit('/').next().unwrap_or(&repo_slug);
        let label = format!("{repo}-queue-authority-{machine}");
        ensure_runner_label(&repo_slug, &label, gh_command)?;
        format!("[self-hosted, {label}]")
    } else {
        "ubuntu-latest".to_owned()
    };
    fs::write(
        &target,
        render_workflow(tag_pattern, shipyard_version, &runner, signing_setup_script),
    )
    .map_err(|error| {
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

fn ensure_runner_label(
    repo_slug: &str,
    label: &str,
    gh_command: Option<&Path>,
) -> Result<(), CliFailure> {
    let output = gh(gh_command)
        .args([
            "api",
            &format!("repos/{repo_slug}/actions/runners"),
            "--paginate",
            "--jq",
            ".runners[].labels[].name",
        ])
        .output()
        .map_err(|error| CliFailure::new(1, format!("failed to inspect runner labels: {error}")))?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "failed to inspect runner label `{label}`: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    if String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|candidate| candidate.trim() == label)
    {
        return Ok(());
    }
    Err(CliFailure::new(
        1,
        format!(
            "dedicated queue-authority runner label `{label}` is not registered; run `shipyard runner register --count 1 --labels {label}` on the authority machine, ensuring that controller does not also carry the generic build label"
        ),
    ))
}

fn hook_run<W: Write>(
    stdout: &mut W,
    config: &LoadedConfig,
    cwd: &Path,
    state_root: &Path,
    mode: RuntimeMode,
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
    let result = run_hook(
        &hook_config,
        &resolved_tag,
        cwd,
        state_root,
        mode,
        &config.global_dir,
    );
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

fn validate_workflow_script_path(path: &str) -> Result<&str, String> {
    let valid = !path.is_empty()
        && !path.starts_with('-')
        && path.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && component
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        });
    if !valid {
        return Err(format!(
            "release.post_tag_hook.ssh_signing_setup_script must be a safe repository-relative path, got {path:?}"
        ));
    }
    Ok(path)
}

fn render_workflow(
    tag_pattern: &str,
    shipyard_version: &str,
    runner: &str,
    ssh_signing_setup_script: Option<&str>,
) -> String {
    let signing_setup = ssh_signing_setup_script.map_or_else(String::new, |script| {
        format!(
            r"      - name: Configure release bot SSH signing
        shell: bash
        env:
          RELEASE_BOT_SSH_SIGNING_KEY: ${{{{ secrets.RELEASE_BOT_SSH_SIGNING_KEY }}}}
        run: bash -- {script}

"
        )
    });
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
  pull-requests: write

env:
  SHIPYARD_VERSION: "{shipyard_version}"

jobs:
  sync:
    name: Regenerate docs for ${{{{ github.ref_name }}}}
    runs-on: {runner}
    steps:
      - name: Checkout release tag with full history
        uses: actions/checkout@v5
        with:
          ref: ${{{{ github.ref }}}}
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

{signing_setup}
      - name: Run post-tag docs sync
        shell: bash
        env:
          GITHUB_TOKEN: ${{{{ secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN }}}}
        run: |
          tag="${{GITHUB_REF#refs/tags/}}"
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
    ssh_signing_setup_script: Option<String>,
    // How the bot commit lands on `branch`:
    //   "direct" (default) — push --ff-only straight to `branch` (today's
    //     behavior). Incompatible with a GitHub "Require merge queue" rule,
    //     which rejects ALL direct pushes to the protected branch.
    //   "pr" — create or resume one immutable branch per tag, wait for required
    //     checks, then enqueue its exact head. Required when `branch` enforces
    //     a merge queue.
    push_mode: String,
    // PR-route branch prefix; the stable release tag is appended.
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
            ssh_signing_setup_script: None,
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
        ssh_signing_setup_script: section
            .get("ssh_signing_setup_script")
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
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

fn run_hook(
    config: &HookConfig,
    tag: &str,
    cwd: &Path,
    state_root: &Path,
    mode: RuntimeMode,
    global_dir: &Path,
) -> HookResult {
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
    if let Err(error) = commit_and_push_docs(
        cwd,
        state_root,
        mode,
        global_dir,
        config,
        tag,
        &diffed,
        &mut result,
    ) {
        result.error = Some(error);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn commit_and_push_docs(
    cwd: &Path,
    state_root: &Path,
    mode: RuntimeMode,
    global_dir: &Path,
    config: &HookConfig,
    tag: &str,
    diffed: &[String],
    result: &mut HookResult,
) -> Result<(), String> {
    validate_hook_push_mode(&config.push_mode)?;
    let mut add_args = vec![String::from("add"), String::from("--")];
    add_args.extend(diffed.iter().cloned());
    run_git_owned(cwd, &add_args)?;
    run_git(cwd, &["config", "user.name", &config.bot_name])?;
    run_git(cwd, &["config", "user.email", &config.bot_email])?;
    let subject = release_bot_commit_subject(&config.push_mode, tag);
    let mut commit_args = vec![
        "commit".to_owned(),
        "-m".to_owned(),
        subject,
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
    if config.push_mode == "pr" {
        let commit_date = run_git_capture(cwd, &["log", "-1", "--format=%cI", tag])?;
        run_git_owned_with_date(cwd, &commit_args, &commit_date)?;
    } else {
        run_git_owned(cwd, &commit_args)?;
    }
    result.committed = true;
    match config.push_mode.as_str() {
        "pr" => push_via_pr(cwd, state_root, mode, global_dir, config, tag, result),
        "direct" => push_direct(cwd, config, result),
        other => Err(format!(
            "invalid release.post_tag_hook.push_mode {other:?}; expected `direct` or `pr`"
        )),
    }
}

fn validate_hook_push_mode(push_mode: &str) -> Result<(), String> {
    match push_mode {
        "direct" | "pr" => Ok(()),
        other => Err(format!(
            "invalid release.post_tag_hook.push_mode {other:?}; expected `direct` or `pr`"
        )),
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
            &["push", &config.remote, &branch_push_refspec(&config.branch)],
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

fn branch_push_refspec(branch: &str) -> String {
    format!("HEAD:refs/heads/{branch}")
}

// push_mode = "pr": push the bot commit to an immutable branch, open a PR,
// wait for required checks, and enqueue exactly that SHA (a direct push would
// violate "Require merge queue"). `gh` authenticates via the ambient client.
#[allow(clippy::too_many_lines)]
fn push_via_pr(
    cwd: &Path,
    state_root: &Path,
    mode: RuntimeMode,
    global_dir: &Path,
    config: &HookConfig,
    tag: &str,
    result: &mut HookResult,
) -> Result<(), String> {
    let repo = detect_repo_from_remote(cwd, None)
        .ok_or_else(|| "can't detect owner/repo from git remote".to_owned())?;
    let preflight = if target_requires_merge_queue(cwd, &repo, &config.branch)? {
        Some(
            preflight_mutation_authority(
                state_root,
                cwd,
                mode,
                global_dir,
                &repo,
                &config.branch,
            )
            .map_err(|error| {
                format!(
                    "{error}; merge-queue PR mode now requires a dedicated authority host. Configure machine-global [merge_queue].mutation_machine, set its runner tag, and rerun `shipyard release-bot hook install` so the workflow no longer targets a GitHub-hosted runner"
                )
            })?,
        )
    } else {
        None
    };
    let local_commit = run_git_capture(cwd, &["rev-parse", "HEAD"])?;
    ensure_release_commit_is_based_on_target(cwd, config)?;
    let pr_branch = release_bot_branch(&config.pr_branch_prefix, tag);
    let existing = find_open_release_bot_pr(cwd, &pr_branch, &repo)?;
    let (target, commit) = if let Some(target) = existing {
        let commit = target["headRefOid"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "existing release-bot PR omitted headRefOid".to_owned())?
            .to_owned();
        if commit != local_commit {
            return Err(format!(
                "existing release-bot PR head {commit} does not match deterministic generated commit {local_commit} for tag `{tag}`"
            ));
        }
        validate_release_bot_repository(&target, &repo)?;
        validated_release_bot_pr(&target, &commit, &config.branch)?;
        match target["state"].as_str() {
            Some("MERGED") => return Ok(()),
            Some("OPEN") => {}
            Some(state) => {
                return Err(format!(
                    "release-bot PR for deterministic head {commit} is {state}; refusing to replace it"
                ));
            }
            None => return Err("existing release-bot PR omitted state".to_owned()),
        }
        (target, commit)
    } else {
        record_pr_push_attempt(result);
        attach_and_push_release_bot_branch(cwd, &config.remote, &pr_branch)?;
        result.pushed = true;
        create_release_bot_pr(cwd, config, tag, &pr_branch)?;
        let target = run_gh_json(
            cwd,
            &[
                "pr",
                "view",
                &pr_branch,
                "--json",
                "number,id,state,headRefOid,baseRefName,headRepository,headRepositoryOwner,isCrossRepository",
            ],
        )?;
        validate_release_bot_repository(&target, &repo)?;
        (target, local_commit)
    };
    // Verify the immutable ref before waiting and again immediately before the
    // server-side exact-head enqueue mutation.
    let (pr, _, _, _) = validated_release_bot_pr(&target, &commit, &config.branch)?;
    // The immutable PR now exists without any auto-merge authority. Release
    // process-wide serialization while required checks run; admission below
    // reacquires authority and revalidates the exact head and base.
    drop(preflight);
    let pr_text = pr.to_string();
    let target = run_gh_json(
        cwd,
        &[
            "pr",
            "view",
            &pr_text,
            "--json",
            "number,id,state,headRefOid,baseRefName,headRepository,headRepositoryOwner,isCrossRepository",
        ],
    )?;
    validate_release_bot_repository(&target, &repo)?;
    let (revalidated_pr, _, head, base) =
        validated_release_bot_pr(&target, &commit, &config.branch)?;
    if revalidated_pr != pr {
        return Err(format!(
            "release-bot PR identity changed from #{pr} to #{revalidated_pr}; refusing queue admission"
        ));
    }
    wait_for_required_pr_checks(cwd, pr)?;
    let store = ShipStateStore::new(state_root.join("ship"))
        .map_err(|error| format!("failed to open ship-state store: {error}"))?;
    seed_release_bot_ship_state(&store, pr, &repo, &pr_branch, &base, &head)?;
    let request = AutoMergeRequest {
        mode,
        global_dir: global_dir.to_path_buf(),
        pr,
        merge_method: MergeMethod::Merge,
        delete_branch: false,
        admin: false,
        pr_snapshot_file: None,
        merge_command: None,
        merge_result: None,
        expected_validation: None,
    };
    supervise_release_bot_admission(&store, cwd, &request)
}

fn record_pr_push_attempt(result: &mut HookResult) {
    result.attempts = 1;
}

fn attach_and_push_release_bot_branch(
    cwd: &Path,
    remote: &str,
    branch: &str,
) -> Result<(), String> {
    // Tag-triggered Actions checkouts are detached. Attach the deterministic
    // PR branch before pushing so repository pre-push hooks can validate the
    // branch state instead of rejecting an otherwise explicit refspec.
    run_git(cwd, &["switch", "-C", branch])?;
    run_git_supervised(cwd, &["push", remote, &branch_push_refspec(branch)])
}

fn ensure_release_commit_is_based_on_target(cwd: &Path, config: &HookConfig) -> Result<(), String> {
    run_git(cwd, &["fetch", &config.remote, &config.branch])?;
    let generated_parent = run_git_capture(cwd, &["rev-parse", "HEAD^"])?;
    let target = format!("{}/{}", config.remote, config.branch);
    let status = Command::new("git")
        .args(["merge-base", "--is-ancestor", &generated_parent, &target])
        .current_dir(cwd)
        .status()
        .map_err(|error| format!("failed to verify release tag ancestry: {error}"))?;
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "release tag commit {generated_parent} is not an ancestor of configured target {target}; refusing to open a PR that could contain unrelated divergent commits"
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RequiredCheckState {
    Passed,
    NoChecks,
    Pending,
    Failed(Vec<String>),
}

fn wait_for_required_pr_checks(cwd: &Path, pr: u64) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_hours(2);
    let mut no_checks_since = None;
    loop {
        match required_pr_check_state(cwd, pr)? {
            RequiredCheckState::Passed => return Ok(()),
            RequiredCheckState::NoChecks => {
                let observed_at = no_checks_since.get_or_insert_with(std::time::Instant::now);
                if observed_at.elapsed() >= Duration::from_secs(30) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_secs(5));
            }
            RequiredCheckState::Failed(checks) => {
                return Err(format!(
                    "release-bot PR #{pr} has failed required checks: {}",
                    checks.join(", ")
                ));
            }
            RequiredCheckState::Pending if std::time::Instant::now() >= deadline => {
                return Err(format!(
                    "timed out waiting for required checks on release-bot PR #{pr}"
                ));
            }
            RequiredCheckState::Pending => {
                no_checks_since = None;
                std::thread::sleep(Duration::from_secs(15));
            }
        }
    }
}

fn required_pr_check_state(cwd: &Path, pr: u64) -> Result<RequiredCheckState, String> {
    let output = gh(None)
        .args([
            "pr",
            "checks",
            &pr.to_string(),
            "--required",
            "--json",
            "bucket,name,state",
        ])
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("failed to inspect required checks for PR #{pr}: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    interpret_required_check_output(output.status.success(), &output.stdout, &stderr)
        .map_err(|error| format!("failed to inspect required checks for PR #{pr}: {error}"))
}

fn interpret_required_check_output(
    success: bool,
    stdout: &[u8],
    stderr: &str,
) -> Result<RequiredCheckState, String> {
    if success && let Ok(rows) = serde_json::from_slice::<Vec<Value>>(stdout) {
        return classify_required_check_rows(&rows);
    }
    if no_required_checks_reported(stderr) {
        return Ok(RequiredCheckState::NoChecks);
    }
    Err(stderr.to_owned())
}

fn no_required_checks_reported(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("no required checks reported on the") && lower.contains("branch")
}

fn classify_required_check_rows(rows: &[Value]) -> Result<RequiredCheckState, String> {
    if rows.is_empty() {
        return Ok(RequiredCheckState::NoChecks);
    }
    let mut pending = false;
    let mut failed = Vec::new();
    for row in rows {
        let name = row["name"].as_str().unwrap_or("<unnamed>").to_owned();
        match row["bucket"].as_str().unwrap_or("") {
            "pass" | "skipping" => {}
            "pending" => pending = true,
            "fail" | "cancel" => failed.push(name),
            bucket => {
                return Err(format!(
                    "required check {name} returned unknown bucket {bucket:?}"
                ));
            }
        }
    }
    if !failed.is_empty() {
        Ok(RequiredCheckState::Failed(failed))
    } else if pending {
        Ok(RequiredCheckState::Pending)
    } else {
        Ok(RequiredCheckState::Passed)
    }
}

fn seed_release_bot_ship_state(
    store: &ShipStateStore,
    pr: u64,
    repo: &str,
    head_branch: &str,
    base: &str,
    head: &str,
) -> Result<(), String> {
    let lock = store
        .lock_pr_scoped(repo, pr)
        .map_err(|error| format!("failed to lock release-bot ship-state: {error}"))?;
    let mut state = if let Some(existing) = store.get_locked_scoped(repo, pr, &lock) {
        if canonical_repository(&existing.repo) != canonical_repository(repo)
            || existing.base_branch != base
            || existing.head_sha != head
        {
            return Err(format!(
                "existing ship-state for release-bot PR #{pr} does not match {repo} {base} {head}"
            ));
        }
        existing
    } else {
        ShipState::new(pr, repo, head_branch.to_owned(), base, head, "release-bot")
    };
    state
        .evidence_snapshot
        .insert("release-bot-required-checks".to_owned(), "pass".to_owned());
    state.touch();
    store
        .save_scoped_locked(&state, &lock)
        .map_err(|error| format!("failed to persist release-bot ship-state: {error}"))
}

fn supervise_release_bot_admission(
    store: &ShipStateStore,
    cwd: &Path,
    request: &AutoMergeRequest,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_hours(2);
    loop {
        let outcome = execute_auto_merge(store, cwd, request)
            .map_err(|error| format!("release-bot queue supervision failed: {error}"))?;
        match outcome {
            AutoMergeOutcome::AlreadyMerged | AutoMergeOutcome::Merged { .. } => return Ok(()),
            AutoMergeOutcome::Enqueued => {
                let repository = super::branch_cmd::detect_repo_from_remote(cwd, None);
                let state = repository
                    .as_ref()
                    .and_then(|repo| store.get_scoped(repo, request.pr))
                    .or_else(|| store.get(request.pr))
                    .ok_or_else(|| {
                        format!("release-bot ship-state disappeared for PR #{}", request.pr)
                    })?;
                if state.merge_queue_enqueue_succeeded_at.is_some()
                    || state.merge_queue_observed_at.is_some()
                {
                    return Ok(());
                }
            }
            AutoMergeOutcome::MergeFailed { error } if merge_queue_lock_contended(&error) => {}
            outcome => {
                return Err(format!(
                    "release-bot exact-head queue supervision did not admit PR #{}: {outcome:?}",
                    request.pr
                ));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for authoritative queue admission of release-bot PR #{}",
                request.pr
            ));
        }
        std::thread::sleep(Duration::from_secs(15));
    }
}

fn merge_queue_lock_contended(error: &str) -> bool {
    error.contains("another Shipyard process is performing a merge-queue mutation")
        || error.contains("another Shipyard process owns merge-queue mutation authority")
}

fn find_open_release_bot_pr(
    cwd: &Path,
    branch: &str,
    expected_repo: &str,
) -> Result<Option<Value>, String> {
    let result = run_gh_json(
        cwd,
        &[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "1",
            "--json",
            "number,id,state,headRefOid,baseRefName,headRepository,headRepositoryOwner,isCrossRepository",
        ],
    )?;
    let target = result.as_array().and_then(|items| items.first()).cloned();
    if let Some(target) = target.as_ref() {
        validate_release_bot_repository(target, expected_repo)?;
    }
    Ok(target)
}

fn create_release_bot_pr(
    cwd: &Path,
    config: &HookConfig,
    tag: &str,
    branch: &str,
) -> Result<(), String> {
    let title = format!("docs: regenerate changelog for {tag}");
    let body = format!(
        "Automated by shipyard release-bot hook run after tag `{tag}` — routes the CHANGELOG.md regeneration through a PR so it lands via the merge queue instead of a direct push to `{}`.",
        config.branch
    );
    run_gh(
        cwd,
        &[
            "pr",
            "create",
            "--base",
            &config.branch,
            "--head",
            branch,
            "--title",
            &title,
            "--body",
            &body,
        ],
    )
    .map_err(|error| format!("gh pr create failed: {error}"))
}

fn release_bot_branch(prefix: &str, tag: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(tag.as_bytes()));
    format!("{prefix}-{}-{}", sanitize_ref_component(tag), &digest[..12])
}

fn release_bot_commit_subject(push_mode: &str, tag: &str) -> String {
    if push_mode == "pr" {
        format!("docs: regenerate changelog for {tag}")
    } else {
        format!("docs: regenerate changelog for {tag} [skip ci]")
    }
}

fn validate_release_bot_base(actual: &str, expected: &str) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "release-bot PR targets `{actual}`, expected configured base `{expected}`; refusing queue admission"
        ))
    }
}

fn validate_release_bot_repository(target: &Value, expected_repo: &str) -> Result<(), String> {
    let (expected_owner, expected_name) = expected_repo
        .split_once('/')
        .ok_or_else(|| format!("invalid expected repository `{expected_repo}`"))?;
    let owner = target["headRepositoryOwner"]["login"]
        .as_str()
        .ok_or_else(|| "gh pr view omitted headRepositoryOwner.login".to_owned())?;
    let name = target["headRepository"]["name"]
        .as_str()
        .ok_or_else(|| "gh pr view omitted headRepository.name".to_owned())?;
    let cross_repo = target["isCrossRepository"]
        .as_bool()
        .ok_or_else(|| "gh pr view omitted isCrossRepository".to_owned())?;
    if cross_repo || owner != expected_owner || name != expected_name {
        return Err(format!(
            "release-bot PR head belongs to {owner}/{name}, expected same-repository head in {expected_repo}"
        ));
    }
    Ok(())
}

fn validated_release_bot_pr(
    target: &Value,
    expected_head: &str,
    expected_base: &str,
) -> Result<(u64, String, String, String), String> {
    let pr = target["number"]
        .as_u64()
        .ok_or_else(|| "gh pr view omitted PR number".to_owned())?;
    let id = target["id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "gh pr view omitted GraphQL id".to_owned())?;
    let head = target["headRefOid"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "gh pr view omitted headRefOid".to_owned())?;
    let base = target["baseRefName"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "gh pr view omitted baseRefName".to_owned())?;
    validate_release_bot_head(head, expected_head)?;
    validate_release_bot_base(base, expected_base)?;
    Ok((pr, id.to_owned(), head.to_owned(), base.to_owned()))
}

fn validate_release_bot_head(actual: &str, expected: &str) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "release-bot PR head changed to `{actual}`, expected immutable head `{expected}`; refusing queue admission"
        ))
    }
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
            .is_ok_and(|output| !output.stdout.is_empty());
        if status {
            diffed.push(path.clone());
            continue;
        }
        let changed = Command::new("git")
            .args(["diff", "--quiet", "HEAD", "--", path])
            .current_dir(cwd)
            .status()
            .is_ok_and(|status| !status.success());
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

fn run_git_supervised(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let output = crate::supervised::git_push_supervised()
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

fn run_git_capture(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("failed to run git {}: {error}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
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

fn run_git_owned_with_date(cwd: &Path, args: &[String], date: &str) -> Result<(), String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
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

// Run `gh` in `cwd` via shipyard's ambient GhClient (same auth path as
// src/pr.rs). Used by push_via_pr to open, watch, and enqueue the changelog PR.
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

fn run_gh_json(cwd: &Path, args: &[&str]) -> Result<Value, String> {
    let output = gh(None)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("failed to run gh {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("gh {} returned malformed JSON: {error}", args.join(" ")))
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

    #[test]
    fn release_bot_state_seed_is_atomic_with_concurrent_provenance_update() {
        let temp = tempfile::tempdir().expect("temp");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let pr = 42;
        let mut initial = ShipState::new(
            pr,
            "owner/repo",
            "release/post-tag-sync/v0.79.0",
            "main",
            "a".repeat(40),
            "policy",
        );
        initial
            .evidence_snapshot
            .insert("initial".to_owned(), "pass".to_owned());
        store.save(&initial).expect("initial state");

        let lock = store.lock_pr_scoped("owner/repo", pr).expect("writer lock");
        let worker_store = store.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).expect("started");
            done_tx
                .send(seed_release_bot_ship_state(
                    &worker_store,
                    pr,
                    "owner/repo",
                    "release/post-tag-sync/v0.79.0",
                    "main",
                    &"a".repeat(40),
                ))
                .expect("result");
        });
        started_rx.recv().expect("worker started");
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "release-bot seed bypassed the existing PR-state lock"
        );

        let mut concurrent = store
            .get_locked_scoped("owner/repo", pr, &lock)
            .expect("state");
        concurrent
            .evidence_snapshot
            .insert("concurrent-provenance".to_owned(), "pass".to_owned());
        store
            .save_scoped_locked(&concurrent, &lock)
            .expect("concurrent update");
        drop(lock);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("seed completed")
            .expect("seed succeeded");
        worker.join().expect("worker");

        let saved = store.get_scoped("owner/repo", pr).expect("saved state");
        assert_eq!(saved.evidence_snapshot["initial"], "pass");
        assert_eq!(saved.evidence_snapshot["concurrent-provenance"], "pass");
        assert_eq!(
            saved.evidence_snapshot["release-bot-required-checks"],
            "pass"
        );
    }

    #[test]
    fn release_bot_state_seed_accepts_repository_case_alias() {
        let temp = tempfile::tempdir().expect("temp");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let pr = 42;
        let head = "a".repeat(40);
        let initial = ShipState::new(
            pr,
            "Owner/Repo",
            "release/post-tag-sync/v0.79.0",
            "main",
            &head,
            "policy",
        );
        store.save(&initial).expect("initial state");

        seed_release_bot_ship_state(
            &store,
            pr,
            "owner/repo",
            "release/post-tag-sync/v0.79.0",
            "main",
            &head,
        )
        .expect("case-only repository alias should resume existing state");

        let saved = store.get_scoped("owner/repo", pr).expect("saved state");
        assert_eq!(saved.repo, "Owner/Repo");
        assert_eq!(
            saved.evidence_snapshot["release-bot-required-checks"],
            "pass"
        );
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
        crate::test_support::compile_native_test_program(
            root,
            "gh",
            r#"
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let output = if args.starts_with("api repos/owner/repo/actions/runners") {
        "self-hosted\nmacos\narm64\nrepo-queue-authority-studio\n"
    } else if args.starts_with("api repos/owner/repo/actions/secrets") {
        "{\"secrets\":[{\"name\":\"RELEASE_BOT_TOKEN\",\"updated_at\":\"2026-04-25T09:30:00Z\"}]}\n"
    } else if args.starts_with("api repos/owner/other/actions/secrets") {
        "{\"secrets\":[{\"name\":\"RELEASE_BOT_TOKEN\",\"updated_at\":\"2026-04-25T08:00:00Z\"}]}\n"
    } else if args.starts_with("run list --workflow auto-release.yml --repo owner/repo") {
        "[{\"databaseId\":123,\"status\":\"completed\",\"conclusion\":\"failure\",\"createdAt\":\"2026-04-25T10:00:00Z\"}]\n"
    } else if args.starts_with("run view 123 --repo owner/repo --log-failed") {
        "fatal: Authentication failed\n"
    } else {
        eprintln!("unexpected gh args: {args}");
        std::process::exit(2);
    };
    print!("{output}");
}
"#,
        )
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

        let exit = hook_install(
            &mut output,
            &empty_config(),
            temp.path(),
            "shipyard-v*",
            "v0.50.0",
            true,
            None,
        )
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
        let workflow = render_workflow("v*", "v0.51.0", "ubuntu-latest", None);

        assert!(workflow.contains("secrets.RELEASE_BOT_TOKEN || secrets.GITHUB_TOKEN"));
        assert!(workflow.contains(r#"SHIPYARD_VERSION: "v0.51.0""#));
        assert!(workflow.contains(r#"tags: ["v*"]"#));
        assert!(workflow.contains("curl -fsSL \"https://generouscorp.com/Shipyard/install.sh\""));
        assert!(workflow.contains(r#"tag="${GITHUB_REF#refs/tags/}""#));
        assert!(!workflow.contains(r#"tag="${{GITHUB_REF#refs/tags/}}""#));
        assert!(workflow.contains("pull-requests: write"));
    }

    #[test]
    fn render_workflow_configures_repository_ssh_signing() {
        let script =
            validate_workflow_script_path("tools/scripts/configure_release_bot_ssh_signing.sh")
                .expect("safe script path");
        let workflow = render_workflow("v*", "v0.52.0", "ubuntu-latest", Some(script));

        assert!(workflow.contains("name: Configure release bot SSH signing"));
        assert!(
            workflow.contains(
                "RELEASE_BOT_SSH_SIGNING_KEY: ${{ secrets.RELEASE_BOT_SSH_SIGNING_KEY }}"
            )
        );
        assert!(
            workflow.contains("run: bash -- tools/scripts/configure_release_bot_ssh_signing.sh")
        );
        for unsafe_path in [
            "../sign.sh",
            "/tmp/sign.sh",
            "-x",
            "sign script.sh",
            "sign.sh\nrun: bad",
        ] {
            assert!(validate_workflow_script_path(unsafe_path).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn hook_install_routes_queue_authority_to_its_host_runner() {
        let temp = tempfile::tempdir().expect("tempdir");
        git(temp.path(), &["init"]);
        git(
            temp.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let global_dir = temp.path().join("global");
        fs::create_dir_all(&global_dir).expect("global config dir");
        fs::create_dir_all(temp.path().join(".shipyard")).expect("project config dir");
        fs::write(
            temp.path().join(".shipyard/config.toml"),
            "[merge_queue]\nmutation_machine = \"m1\"\n",
        )
        .expect("untrusted project config");
        fs::write(
            global_dir.join("config.toml"),
            "[merge_queue]\nmutation_machine = \"studio\"\n",
        )
        .expect("global config");
        let config = LoadedConfig::load(
            Some(global_dir),
            Some(temp.path().join(".shipyard")),
            None,
            LocalOverlaySource::None,
        )
        .expect("config");
        let mut output = Vec::new();

        let gh = fake_gh(temp.path());
        hook_install(
            &mut output,
            &config,
            temp.path(),
            "v*",
            "v0.79.0",
            true,
            Some(&gh),
        )
        .expect("install");
        let workflow = fs::read_to_string(
            temp.path()
                .join(".github/workflows")
                .join(POST_TAG_WORKFLOW),
        )
        .expect("workflow");
        assert!(workflow.contains("runs-on: [self-hosted, repo-queue-authority-studio]"));
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
ssh_signing_setup_script = "tools/configure-signing.sh"

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
        assert_eq!(
            parsed.ssh_signing_setup_script.as_deref(),
            Some("tools/configure-signing.sh")
        );
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
    fn hook_push_mode_rejects_typos_instead_of_falling_back_to_direct() {
        assert!(validate_hook_push_mode("direct").is_ok());
        assert!(validate_hook_push_mode("pr").is_ok());
        let error = validate_hook_push_mode("pull-request").expect_err("typo rejected");
        assert!(error.contains("expected `direct` or `pr`"));
    }

    #[test]
    fn pr_push_records_one_attempt() {
        let mut result = HookResult::default();
        record_pr_push_attempt(&mut result);
        assert_eq!(result.attempts, 1);
    }

    #[test]
    fn release_bot_pr_must_still_target_configured_base() {
        validate_release_bot_base("main", "main").expect("matching base");
        let error =
            validate_release_bot_base("release", "main").expect_err("retargeted PR rejected");
        assert!(error.contains("targets `release`"));
        assert!(error.contains("expected configured base `main`"));
    }

    #[test]
    fn release_bot_pr_must_keep_immutable_head() {
        let head = "a".repeat(40);
        validate_release_bot_head(&head, &head).expect("matching head");
        let error =
            validate_release_bot_head(&"b".repeat(40), &head).expect_err("changed head rejected");
        assert!(error.contains("expected immutable head"));
    }

    #[test]
    fn release_bot_pr_must_use_same_repository_head() {
        let same_repo = serde_json::json!({
            "headRepositoryOwner": {"login": "owner"},
            "headRepository": {"name": "repo"},
            "isCrossRepository": false,
        });
        validate_release_bot_repository(&same_repo, "owner/repo").expect("same repo");
        let fork = serde_json::json!({
            "headRepositoryOwner": {"login": "attacker"},
            "headRepository": {"name": "repo"},
            "isCrossRepository": true,
        });
        let error =
            validate_release_bot_repository(&fork, "owner/repo").expect_err("fork rejected");
        assert!(error.contains("expected same-repository head"));
    }

    #[test]
    fn release_bot_branch_is_stable_for_workflow_retries() {
        let first = release_bot_branch("release/post-tag-sync", "v0.79.0");
        assert_eq!(
            first,
            release_bot_branch("release/post-tag-sync", "v0.79.0")
        );
        assert!(first.starts_with("release/post-tag-sync-v0.79.0-"));
        assert_ne!(
            release_bot_branch("release/post-tag-sync", "v1/foo"),
            release_bot_branch("release/post-tag-sync", "v1-foo")
        );
    }

    #[cfg(unix)]
    #[test]
    fn release_bot_pr_push_attaches_detached_head_and_marks_push_supervised() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        fs::create_dir(&repo).expect("repo");
        git(
            temp.path(),
            &["init", "--bare", remote.to_str().expect("remote")],
        );
        git(&repo, &["init"]);
        git(&repo, &["config", "user.name", "release bot"]);
        git(&repo, &["config", "user.email", "release@example.com"]);
        fs::write(repo.join("CHANGELOG.md"), "release\n").expect("changelog");
        git(&repo, &["add", "CHANGELOG.md"]);
        git(&repo, &["commit", "-m", "release docs"]);
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().expect("remote")],
        );
        git(&repo, &["checkout", "--detach"]);

        let hook = repo.join(".git/hooks/pre-push");
        write_executable(
            &hook,
            r#"#!/bin/sh
test "$SHIPYARD_PR_RUNNING" = 1 || exit 41
test "$(git symbolic-ref --short HEAD)" = "release/post-tag-sync-v1.2.3-test" || exit 42
"#,
        );

        let branch = "release/post-tag-sync-v1.2.3-test";
        attach_and_push_release_bot_branch(&repo, "origin", branch).expect("push");
        let attached =
            run_git_capture(&repo, &["symbolic-ref", "--short", "HEAD"]).expect("attached branch");
        assert_eq!(attached, branch);
        let remote_head = run_git_capture(&remote, &["rev-parse", &format!("refs/heads/{branch}")])
            .expect("remote branch");
        let local_head = run_git_capture(&repo, &["rev-parse", "HEAD"]).expect("local head");
        assert_eq!(remote_head, local_head);
    }

    #[test]
    fn pr_mode_does_not_suppress_required_ci() {
        assert_eq!(
            release_bot_commit_subject("pr", "v0.79.0"),
            "docs: regenerate changelog for v0.79.0"
        );
        assert!(release_bot_commit_subject("direct", "v0.79.0").contains("[skip ci]"));
    }

    #[test]
    fn required_check_rows_use_structured_buckets() {
        assert_eq!(
            classify_required_check_rows(&[]).expect("empty"),
            RequiredCheckState::NoChecks
        );
        assert_eq!(
            classify_required_check_rows(&[serde_json::json!({
                "name": "macos",
                "bucket": "pending"
            })])
            .expect("pending"),
            RequiredCheckState::Pending
        );
        assert_eq!(
            classify_required_check_rows(&[serde_json::json!({
                "name": "windows",
                "bucket": "fail"
            })])
            .expect("failed"),
            RequiredCheckState::Failed(vec!["windows".to_owned()])
        );
        assert!(
            classify_required_check_rows(&[serde_json::json!({
                "name": "mystery",
                "bucket": "unknown"
            })])
            .expect_err("unknown rejected")
            .contains("unknown bucket")
        );
    }

    #[test]
    fn failed_required_check_query_cannot_turn_empty_json_into_no_checks() {
        let error = interpret_required_check_output(false, b"[]", "authentication failed")
            .expect_err("failed gh command must fail closed");
        assert_eq!(error, "authentication failed");
        assert_eq!(
            interpret_required_check_output(
                false,
                b"[]",
                "no required checks reported on the main branch"
            )
            .expect("known zero-check response"),
            RequiredCheckState::NoChecks
        );
    }

    #[test]
    fn no_required_checks_wording_is_recognized_without_text_status_parsing() {
        assert!(no_required_checks_reported(
            "no required checks reported on the main branch"
        ));
        assert!(!no_required_checks_reported(
            "required check macos is pending"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn annotated_tag_commit_date_resolves_to_one_rfc3339_line() {
        let temp = tempfile::tempdir().expect("temp");
        git(temp.path(), &["init"]);
        git(temp.path(), &["config", "user.name", "test user"]);
        git(temp.path(), &["config", "user.email", "test@example.com"]);
        fs::write(temp.path().join("README.md"), "seed\n").expect("readme");
        git(temp.path(), &["add", "README.md"]);
        git(temp.path(), &["commit", "-m", "seed"]);
        git(temp.path(), &["tag", "-a", "v0.79.0", "-m", "release"]);

        let commit_date =
            run_git_capture(temp.path(), &["log", "-1", "--format=%cI", "v0.79.0"]).expect("date");
        assert!(!commit_date.contains('\n'));
        assert!(chrono::DateTime::parse_from_rfc3339(&commit_date).is_ok());
    }

    #[test]
    fn release_bot_retries_only_merge_queue_lock_contention() {
        assert!(merge_queue_lock_contended(
            "another Shipyard process is performing a merge-queue mutation"
        ));
        assert!(merge_queue_lock_contended(
            "another Shipyard process owns merge-queue mutation authority for owner/repo"
        ));
        assert!(!merge_queue_lock_contended(
            "merge-queue mutation authority is studio; this machine is m1"
        ));
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

        let exit = hook_run(
            &mut output,
            &config,
            temp.path(),
            &temp.path().join("state"),
            RuntimeMode::Shipyard,
            None,
            true,
        )
        .expect("hook run");

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

        let error = hook_run(
            &mut output,
            &config,
            temp.path(),
            &temp.path().join("state"),
            RuntimeMode::Shipyard,
            None,
            true,
        )
        .expect_err("tag error");

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
            RuntimeMode::Shipyard,
            &config,
            &repo,
            &temp.path().join("state"),
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

        let result = run_hook(
            &config,
            "v0.50.0",
            temp.path(),
            &temp.path().join("state"),
            RuntimeMode::Shipyard,
            &temp.path().join("global"),
        );

        assert!(result.ran_command);
        assert_eq!(result.command_exit, 7);
        assert!(result.error.expect("error").contains("exited 7"));
    }

    #[cfg(unix)]
    #[test]
    fn run_hook_from_detached_head_commits_and_pushes_watched_docs_diff() {
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
        git(&repo, &["checkout", "--detach"]);
        let config = HookConfig {
            enabled: true,
            command: String::from("printf '\\nentry\\n' >> CHANGELOG.md"),
            max_push_attempts: 1,
            ..HookConfig::default()
        };

        let result = run_hook(
            &config,
            "v0.50.0",
            &repo,
            &temp.path().join("state"),
            RuntimeMode::Shipyard,
            &temp.path().join("global"),
        );

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

        let result = run_hook(
            &config,
            "nightly-2026-04-25",
            temp.path(),
            &temp.path().join("state"),
            RuntimeMode::Shipyard,
            &temp.path().join("global"),
        );

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
