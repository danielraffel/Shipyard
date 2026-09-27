//! `shipyard base-health` — read the base-poison signal and, when configured,
//! act on it.
//!
//! Without `--act` this is read-only: it prints the signal and, for a poisoned
//! base with a named fix pull request, the exact commands that jump it. With
//! `--act` it follows `base_health.auto_jump`, which defaults to `off`:
//!
//! - `off` returns before any GitHub read, so a scheduler can call it freely;
//! - `dry-run` records "would jump PR #n" once per episode and changes nothing;
//! - `on` dequeues the fix pull request and re-enqueues its exact head with
//!   `jump: true`.
//!
//! Every dry-run and live decision is appended to
//! `<state>/base-health/jump-decisions.jsonl`, so the pick can be scored later
//! against the pull request that actually repaired the base.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::Value;

use super::CliFailure;
use crate::base_health::{
    self, AutoJump, BaseHealthFinding, JumpAdvice, JumpDecision, already_recorded,
};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::landability::gate;

/// Parsed command-line arguments.
pub(super) struct BaseHealthArgs {
    pub(super) repo: Option<String>,
    pub(super) workflow: Option<String>,
    pub(super) act: bool,
    pub(super) json: bool,
}

/// Run `shipyard base-health`.
pub(super) fn base_health_command<W: Write>(
    args: BaseHealthArgs,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let auto_jump = AutoJump::parse(config.get_str(base_health::AUTO_JUMP_CONFIG_KEY))
        .map_err(|error| CliFailure::new(2, error))?;
    if args.act && auto_jump == AutoJump::Off {
        emit(
            stdout,
            args.json,
            &serde_json::json!({"auto_jump": auto_jump, "acted": false}),
            "auto-jump is off; nothing read, nothing changed",
        )?;
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let repo = args
        .repo
        .filter(|value| !value.trim().is_empty())
        .or_else(|| gate::resolve_repo(&config, cwd))
        .ok_or_else(|| CliFailure::new(1, "No repo detected. Pass --repo OWNER/REPO."))?;
    let workflow = args
        .workflow
        .or_else(|| {
            config
                .get_str(base_health::WORKFLOW_CONFIG_KEY)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| base_health::DEFAULT_WORKFLOW.to_owned());

    let actions = GitHubActions::from_loaded_config(cwd, &config);
    let reader = |gh_args: &[String]| actions.run_gh(gh_args).map_err(|error| error.to_string());
    let finding = base_health::read_latest(&reader, &repo, &workflow);
    let advice = base_health::jump_advice(&repo, &finding, Utc::now());

    let decision = if args.act {
        advice
            .as_ref()
            .map(|advice| act(&actions, state_dir, &repo, auto_jump, advice))
            .transpose()?
    } else {
        None
    };

    if args.json {
        let payload = serde_json::json!({
            "repo": repo,
            "workflow": workflow,
            "auto_jump": auto_jump,
            "finding": finding,
            "advice": advice,
            "decision": decision,
        });
        writeln!(
            stdout,
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        render_human(stdout, &finding, advice.as_ref(), decision.as_ref())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn emit<W: Write>(
    stdout: &mut W,
    json: bool,
    payload: &Value,
    text: &str,
) -> Result<(), CliFailure> {
    let line = if json {
        payload.to_string()
    } else {
        text.to_owned()
    };
    writeln!(stdout, "{line}").map_err(|error| CliFailure::new(1, error.to_string()))
}

/// Render the finding, advice and decision for a person.
pub(super) fn render_human<W: Write>(
    stdout: &mut W,
    finding: &BaseHealthFinding,
    advice: Option<&JumpAdvice>,
    decision: Option<&JumpDecision>,
) -> std::io::Result<()> {
    match finding {
        BaseHealthFinding::Signal(observation) => {
            let signal = &observation.signal;
            writeln!(
                stdout,
                "base health: {} (detector run {}, {})",
                signal.status.to_uppercase(),
                observation.run_id,
                observation.observed_at.to_rfc3339()
            )?;
            if !signal.reason.is_empty() {
                writeln!(stdout, "  {}", signal.reason)?;
            }
        }
        BaseHealthFinding::NoSignal { detail } => {
            writeln!(stdout, "base health: no signal ({detail})")?;
        }
        BaseHealthFinding::Unreadable { detail } => {
            writeln!(stdout, "base health: UNKNOWN ({detail})")?;
        }
    }
    if let Some(advice) = advice {
        writeln!(stdout, "  {}", advice.message)?;
        for command in &advice.commands {
            writeln!(stdout, "    {command}")?;
        }
    }
    if let Some(decision) = decision {
        writeln!(
            stdout,
            "  auto-jump {}: PR #{} {}",
            decision.mode, decision.pr, decision.outcome
        )?;
    }
    Ok(())
}

fn decisions_path(state_dir: &Path) -> PathBuf {
    state_dir.join("base-health").join("jump-decisions.jsonl")
}

fn load_decisions(path: &Path) -> Vec<JumpDecision> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn append_decision(path: &Path, decision: &JumpDecision) -> Result<(), CliFailure> {
    let io_error = |error: std::io::Error| {
        CliFailure::new(1, format!("could not record {}: {error}", path.display()))
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(io_error)?;
    let line = serde_json::to_string(decision).unwrap_or_default();
    writeln!(file, "{line}").map_err(io_error)
}

fn act(
    actions: &GitHubActions,
    state_dir: &Path,
    repo: &str,
    auto_jump: AutoJump,
    advice: &JumpAdvice,
) -> Result<JumpDecision, CliFailure> {
    let path = decisions_path(state_dir);
    let previous = load_decisions(&path);
    let mut decision = JumpDecision {
        decided_at: Utc::now().to_rfc3339(),
        repo: repo.to_owned(),
        mode: match auto_jump {
            AutoJump::DryRun => "dry_run".to_owned(),
            _ => "on".to_owned(),
        },
        pr: advice.pr,
        tests: advice.tests.clone(),
        signal_run_id: advice.signal_run_id,
        main_run_id: advice.main_run_id.clone(),
        outcome: "would_jump".to_owned(),
    };
    if already_recorded(&previous, &decision) {
        "already_decided".clone_into(&mut decision.outcome);
        return Ok(decision);
    }
    if auto_jump == AutoJump::On {
        decision.outcome = jump(actions, repo, advice.pr);
    }
    append_decision(&path, &decision)?;
    Ok(decision)
}

/// Dequeue and re-enqueue the fix pull request's exact head at the front.
fn jump(actions: &GitHubActions, repo: &str, pr: u64) -> String {
    let Some((owner, name)) = repo.split_once('/') else {
        return format!("failed: repo `{repo}` is not OWNER/REPO");
    };
    let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){id state headRefOid mergeQueueEntry{position}}}}";
    let raw = match actions.run_gh(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
        "-f".to_owned(),
        format!("owner={owner}"),
        "-f".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("number={pr}"),
    ]) {
        Ok(raw) => raw,
        Err(error) => return format!("failed: {error}"),
    };
    let value: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let Some(node) = value.pointer("/data/repository/pullRequest") else {
        return "failed: pull request not found".to_owned();
    };
    let field = |key: &str| {
        node.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let (id, head) = (field("id"), field("headRefOid"));
    if field("state") != "OPEN" {
        return "not_open".to_owned();
    }
    let position = node
        .pointer("/mergeQueueEntry/position")
        .and_then(Value::as_u64);
    if position == Some(1) {
        return "already_first".to_owned();
    }
    let mutate = |query: &str, extra: &[String]| {
        let mut args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={query}"),
            "-f".to_owned(),
            format!("id={id}"),
        ];
        args.extend_from_slice(extra);
        actions.run_gh_internal_queue_mutation(&args)
    };
    if position.is_some()
        && let Err(error) = mutate(base_health::DEQUEUE_MUTATION, &[])
    {
        return format!("failed: dequeue: {error}");
    }
    match mutate(
        base_health::JUMP_MUTATION,
        &["-f".to_owned(), format!("head={head}")],
    ) {
        Ok(raw)
            if serde_json::from_str::<Value>(&raw).is_ok_and(|value| {
                value.get("errors").is_none()
                    && value
                        .pointer("/data/enqueuePullRequest/mergeQueueEntry")
                        .is_some_and(|entry| !entry.is_null())
            }) =>
        {
            "jumped".to_owned()
        }
        Ok(raw) => format!("failed: enqueue: {raw}"),
        Err(error) => format!("failed: enqueue: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base_health::JumpAdvice;

    fn advice(main_run: &str) -> JumpAdvice {
        JumpAdvice {
            pr: 8933,
            tests: vec!["pulp-test-widgets".to_owned()],
            signal_run_id: 7,
            main_run_id: Some(main_run.to_owned()),
            message: "main red: pulp-test-widgets, fix PR #8933, jump it".to_owned(),
            commands: base_health::jump_commands("o/r", 8933),
        }
    }

    #[test]
    fn dry_run_records_would_jump_once_per_episode_and_touches_nothing() {
        let state = tempfile::tempdir().expect("tempdir");
        let actions = GitHubActions::new(state.path());

        let first = act(
            &actions,
            state.path(),
            "o/r",
            AutoJump::DryRun,
            &advice("900"),
        )
        .expect("first decision");
        assert_eq!(first.outcome, "would_jump");
        assert_eq!(first.mode, "dry_run");
        let repeat = act(
            &actions,
            state.path(),
            "o/r",
            AutoJump::DryRun,
            &advice("900"),
        )
        .expect("repeat");
        assert_eq!(repeat.outcome, "already_decided");
        let next_episode = act(
            &actions,
            state.path(),
            "o/r",
            AutoJump::DryRun,
            &advice("901"),
        )
        .expect("next episode");
        assert_eq!(next_episode.outcome, "would_jump");

        let recorded = load_decisions(&decisions_path(state.path()));
        assert_eq!(recorded.len(), 2, "one line per episode: {recorded:?}");
        assert_eq!(recorded[0].pr, 8933);
        assert_eq!(recorded[1].main_run_id.as_deref(), Some("901"));
    }

    #[test]
    fn act_with_auto_jump_off_reads_nothing_and_records_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut out = Vec::new();
        base_health_command(
            BaseHealthArgs {
                repo: Some("o/r".to_owned()),
                workflow: None,
                act: true,
                json: false,
            },
            RuntimeMode::Isolated,
            dir.path(),
            dir.path(),
            &mut out,
        )
        .expect("off is a clean no-op");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("auto-jump is off"), "{text}");
        assert!(!decisions_path(dir.path()).exists());
    }

    #[test]
    fn human_output_names_the_fix_and_the_commands() {
        let mut out = Vec::new();
        render_human(
            &mut out,
            &BaseHealthFinding::NoSignal {
                detail: "fixture".to_owned(),
            },
            Some(&advice("900")),
            None,
        )
        .expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("main red: pulp-test-widgets, fix PR #8933, jump it"),
            "{text}"
        );
        assert!(text.contains("dequeuePullRequest"), "{text}");
        assert!(text.contains("jump:true"), "{text}");
    }
}
