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
    pub(super) base: String,
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
    let base = args.base.clone();
    let merge_groups = match super::merge_steward_cmd::required_contexts(&actions, &repo, &base) {
        Ok(contexts) => {
            base_health::merge_group::read(&reader, &repo, &base, &contexts, Utc::now())
        }
        Err(detail) => base_health::merge_group::MergeGroupHealth::Unknown { detail },
    };
    let advice = base_health::jump_advice(&repo, &finding, Utc::now())
        .or_else(|| merge_group_advice(&reader, &repo, &base, &merge_groups));

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
            "merge_groups": merge_groups,
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
            .and_then(|()| writeln!(stdout, "{}", merge_groups.summary()))
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Jump advice from merge-group poison: only when exactly one open pull
/// request is named as the fix, so the advice never picks between rivals.
/// Its episode key is the newest failing merge-group head.
fn merge_group_advice(
    reader: &dyn Fn(&[String]) -> Result<String, String>,
    repo: &str,
    base: &str,
    health: &base_health::merge_group::MergeGroupHealth,
) -> Option<JumpAdvice> {
    let poisoned = health.poisoned();
    let first = poisoned.first()?;
    let contexts = health.poisoned_contexts();
    let (fixes, _) = super::merge_steward_cmd::named_fixes(reader, repo, base, &contexts);
    let mut fixes = fixes.into_keys();
    let pr = fixes.next()?;
    if fixes.next().is_some() {
        return None;
    }
    let head = first.groups.first().map(|group| group.head_sha.clone());
    Some(JumpAdvice {
        pr,
        tests: contexts.clone(),
        signal_run_id: first.check_id.unwrap_or_default(),
        main_run_id: head.map(|head| format!("merge-group:{head}")),
        message: format!(
            "merge groups red: {}, fix PR #{pr}, jump it",
            contexts.join(", ")
        ),
        commands: base_health::jump_commands(repo, pr),
    })
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
    let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){id state headRefOid baseRefName mergeQueueEntry{position}}}}";
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
    // The jump goes past the ghapp guard with the internal marker, so the
    // guard's tier rule is applied here, before the dequeue.
    let base = field("baseRefName");
    if base.is_empty() {
        return "failed: the pull request carried no base branch".to_owned();
    }
    let run_gh = |args: &[String]| actions.run_gh(args).map_err(|error| error.to_string());
    // A jump goes past the queue's own ordering, so it needs the strongest
    // admission there is: a trusted reviewer's approval naming this exact
    // head. "Not required" is not enough here.
    if let Err(refusal) = approved_exact_head(&run_gh, repo, owner, name, pr, &head, &base) {
        return refusal;
    }
    match crate::deferred_tier::deferred_tier_checks(&run_gh, repo, pr, &base, &head) {
        Ok(checks) => {
            if let Some(detail) = crate::deferred_tier::explain_all(&checks) {
                return format!("refused: {detail}");
            }
        }
        Err(detail) => return format!("failed: test tier unreadable: {detail}"),
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
    let head_arg = ["-f".to_owned(), format!("head={head}")];
    let jumped = enqueue_outcome(mutate(base_health::JUMP_MUTATION, &head_arg));
    let Err(jump_error) = jumped else {
        return "jumped".to_owned();
    };
    // The fix was taken out of the queue (or never in it) and the jump did
    // not land: put the same head back at the end rather than leave the one
    // pull request that repairs the base unqueued. A later tick retries the
    // jump, because a failed decision is never recorded as made.
    match enqueue_outcome(mutate(base_health::PLAIN_ENQUEUE_MUTATION, &head_arg)) {
        Ok(()) => format!("failed: jump: {jump_error}; re-enqueued at the back"),
        Err(restore_error) => format!(
            "failed: jump: {jump_error}; re-enqueue also failed, PR #{pr} is NOT queued: \
             {restore_error}"
        ),
    }
}

/// Whether an enqueue mutation answer holds a queue entry.
fn enqueue_outcome<E: std::fmt::Display>(answer: Result<String, E>) -> Result<(), String> {
    match answer {
        Ok(raw)
            if serde_json::from_str::<Value>(&raw).is_ok_and(|value| {
                value.get("errors").is_none()
                    && value
                        .pointer("/data/enqueuePullRequest/mergeQueueEntry")
                        .is_some_and(|entry| !entry.is_null())
            }) =>
        {
            Ok(())
        }
        Ok(raw) => Err(raw),
        Err(error) => Err(error.to_string()),
    }
}

/// `Ok` only when a trusted reviewer's approval names `head` exactly
/// ([`crate::head_approval::HeadGate::Approved`]); otherwise the outcome
/// to record.
fn approved_exact_head(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    repo: &str,
    owner: &str,
    name: &str,
    pr: u64,
    head: &str,
    base: &str,
) -> Result<(), String> {
    let queue = run_gh(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={}", crate::pr_queue_state::PR_QUEUE_STATE_QUERY),
        "-F".to_owned(),
        format!("owner={owner}"),
        "-F".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("number={pr}"),
    ])
    .and_then(|raw| serde_json::from_str::<Value>(&raw).map_err(|error| error.to_string()))
    .map_err(|error| format!("failed: queue state unreadable: {error}"))?;
    match crate::head_approval::evaluate(run_gh, repo, pr, head, base, &queue) {
        Ok(crate::head_approval::HeadGate::Approved { .. }) => Ok(()),
        Ok(gate) => Err(format!(
            "refused: not approved at the exact head: {}",
            gate.explain()
        )),
        Err(error) => Err(format!("failed: approval unreadable: {error}")),
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
        let actions = GitHubActions::offline_for_tests(state.path());

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
                base: "main".to_owned(),
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

    /// The jump goes past the ghapp guard, so it applies the tier rule itself:
    /// a fix PR whose tiered check ran only its deferred lane is neither
    /// dequeued nor re-enqueued.
    #[cfg(unix)]
    #[test]
    fn the_jump_refuses_a_head_whose_tiered_check_is_deferred() {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls.log");
        let summary = r#"{\"schema\":\"shipyard-test-tier/v1\",\"check\":\"macos\",\"tier\":\"deferred\",\"run_id\":700,\"run_attempt\":1,\"trigger\":\"pull_request\",\"actor\":\"author\",\"full_lane_workflow\":\"build.yml\"}"#;
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *contents/.shipyard/config.toml*) printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["pulp-reviewer"]' '[queue]' 'tiered_required_checks = ["macos"]' 'tier_record_app = "pulp-tier-records"' ;;
  *"--paginate"*"/reviews"*) ;;
  *"--paginate"*"/comments"*) printf '%s\n' '{approval}' ;;
  *pulls/8933*) printf '%s' '{{"number":8933,"user":{{"login":"author","type":"User"}}}}' ;;
  *isInMergeQueue*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":8933,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","isInMergeQueue":true,"mergeQueueEntry":{{"state":"QUEUED","position":3}},"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *check-runs*) printf '%s' '[{{"total_count":2,"check_runs":[{{"id":9,"name":"macos","status":"completed","app":{{"slug":"github-actions"}}}},{{"id":750,"name":"shipyard-test-tier/macos","status":"completed","app":{{"slug":"pulp-tier-records"}},"output":{{"summary":"{summary}"}}}}]}}]' ;;
  *actions/jobs/9*) printf '%s' '{{"id":9,"run_id":700,"run_attempt":1}}' ;;
  *headRefOid*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_1","state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","baseRefName":"main","mergeQueueEntry":{{"position":3}}}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
            approval = jump_approval(),
        );
        let gh = temp.path().join("gh");
        crate::test_support::write_executable_script(&gh, &script);
        let config = crate::config::LoadedConfig {
            data: toml::Table::new(),
            global_dir: temp.path().join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: crate::config::LocalOverlaySource::None,
        };
        let actions =
            GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(gh);
        let outcome = jump(&actions, "o/r", 8933);
        assert!(outcome.starts_with("refused: "), "{outcome}");
        assert!(
            outcome.contains("dispatch `build.yml` with `pr_number=8933` as the fleet App and wait for the full lane"),
            "{outcome}"
        );
        let calls = std::fs::read_to_string(&log).expect("log");
        assert!(!calls.contains("dequeuePullRequest"), "{calls}");
        assert!(!calls.contains("enqueuePullRequest"), "{calls}");
    }

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn jump_approval() -> String {
        format!(
            r#"{{"id":6390564321,"login":"pulp-reviewer","type":"User","association":"MEMBER","at":"2026-11-11T01:53:36Z","body":"verdict: APPROVE head:{HEAD}"}}"#
        )
    }

    /// Run `jump` for PR 8933 (queued at position 3) against a fake `gh`:
    /// `comments` is its verdict history, `jump` and `plain` are GitHub's
    /// answers to the enqueue with and without `jump: true`.
    #[cfg(unix)]
    fn run_jump(comments: &str, jump_answer: &str, plain_answer: &str) -> (String, String) {
        let temp = tempfile::tempdir().expect("temp");
        let log = temp.path().join("calls.log");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *contents/.shipyard/config.toml*) printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["pulp-reviewer"]' ;;
  *"--paginate"*"/reviews"*) ;;
  *"--paginate"*"/comments"*) printf '%s\n' '{comments}' ;;
  *pulls/8933*) printf '%s' '{{"number":8933,"user":{{"login":"author","type":"User"}}}}' ;;
  *check-runs*) printf '%s' '[{{"total_count":0,"check_runs":[]}}]' ;;
  *isInMergeQueue*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":8933,"state":"OPEN","headRefOid":"{HEAD}","isInMergeQueue":true,"mergeQueueEntry":{{"state":"QUEUED","position":3}},"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *headRefOid*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_1","state":"OPEN","headRefOid":"{HEAD}","baseRefName":"main","mergeQueueEntry":{{"position":3}}}}}}}}}}' ;;
  *dequeuePullRequest*) printf '%s' '{{"data":{{"dequeuePullRequest":{{"mergeQueueEntry":{{"id":"MQE_1"}}}}}}}}' ;;
  *enqueuePullRequest*jump:true*) printf '%s' '{jump_answer}' ;;
  *enqueuePullRequest*) printf '%s' '{plain_answer}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
        );
        let gh = temp.path().join("gh");
        crate::test_support::write_executable_script(&gh, &script);
        let config = crate::config::LoadedConfig {
            data: toml::Table::new(),
            global_dir: temp.path().join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: crate::config::LocalOverlaySource::None,
        };
        let actions =
            GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(gh);
        let outcome = jump(&actions, "o/r", 8933);
        (outcome, std::fs::read_to_string(&log).unwrap_or_default())
    }

    const QUEUED_FIRST: &str =
        r#"{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"position":1}}}}"#;
    const QUEUED_LAST: &str =
        r#"{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"position":4}}}}"#;
    const REFUSED: &str = r#"{"data":{"enqueuePullRequest":null},"errors":[{"type":"FORBIDDEN","message":"no jump"}]}"#;

    #[cfg(unix)]
    #[test]
    fn the_jump_needs_a_trusted_approval_of_the_exact_head() {
        let (outcome, calls) = run_jump("", QUEUED_FIRST, QUEUED_LAST);
        assert!(
            outcome.starts_with("refused: not approved at the exact head"),
            "{outcome}"
        );
        assert!(!calls.contains("dequeuePullRequest"), "{calls}");
        assert!(!calls.contains("enqueuePullRequest"), "{calls}");
        // An approval naming another head is no approval of this one.
        let stale = jump_approval().replace(HEAD, &"b".repeat(40));
        let (outcome, calls) = run_jump(&stale, QUEUED_FIRST, QUEUED_LAST);
        assert!(outcome.starts_with("refused: "), "{outcome}");
        assert!(!calls.contains("dequeuePullRequest"), "{calls}");
    }

    #[cfg(unix)]
    #[test]
    fn an_approved_fix_is_jumped() {
        let (outcome, calls) = run_jump(&jump_approval(), QUEUED_FIRST, QUEUED_LAST);
        assert_eq!(outcome, "jumped", "{calls}");
        assert!(calls.contains("dequeuePullRequest"));
        assert_eq!(calls.matches("enqueuePullRequest").count(), 1, "{calls}");
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_jump_after_the_dequeue_puts_the_fix_back_in_the_queue() {
        let (outcome, calls) = run_jump(&jump_approval(), REFUSED, QUEUED_LAST);
        assert!(outcome.starts_with("failed: jump: "), "{outcome}");
        assert!(outcome.ends_with("re-enqueued at the back"), "{outcome}");
        let enqueues = calls
            .lines()
            .filter(|line| line.contains("enqueuePullRequest") && !line.contains("dequeue"))
            .collect::<Vec<_>>();
        assert_eq!(enqueues.len(), 2, "{calls}");
        assert!(enqueues[0].contains("jump:true") && !enqueues[1].contains("jump:true"));
        // A failed outcome is not a decision made: the next tick retries.
        let failed = JumpDecision {
            decided_at: String::new(),
            repo: "o/r".to_owned(),
            mode: "on".to_owned(),
            pr: 8933,
            tests: Vec::new(),
            signal_run_id: 7,
            main_run_id: Some("900".to_owned()),
            outcome: outcome.clone(),
        };
        assert!(!already_recorded(std::slice::from_ref(&failed), &failed));
    }

    #[cfg(unix)]
    #[test]
    fn a_fix_left_unqueued_says_so() {
        let (outcome, _calls) = run_jump(&jump_approval(), REFUSED, REFUSED);
        assert!(outcome.contains("is NOT queued"), "{outcome}");
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
