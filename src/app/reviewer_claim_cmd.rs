//! Durable reviewer claim acquisition and validation.
//!
//! A reviewer must acquire a claim before posting a verdict. Acquisition
//! persists the exact-head claim first and then publishes one authenticated
//! `shipyard-reviewer-claim/v1` check-run receipt. The ghapp reviewer guard
//! calls `validate` immediately before a verdict write, so an expired,
//! revoked, replaced, or wrong-head claim fails closed.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use serde_json::Value;

use super::CliFailure;
use super::cli::ReviewerClaimCommand;
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::gh::GhClient;
use crate::identity::RuntimeMode;
use crate::landability::gate;
use crate::output::write_json_envelope;
use crate::reviewer_receipt::GhAppCheckRunTransport;
use crate::ship_state::{ReviewerClaimRequest, ShipStateStore};

const GITHUB_TIMEOUT: Duration = Duration::from_mins(1);
const MAX_DEADLINE_SECONDS: i64 = 24 * 60 * 60;
const REVIEWER_CLAIM_CHECK_NAME: &str = "reviewer-claim";
const REVIEWER_CLAIM_APP_ID: u64 = 15368;

/// Run one durable reviewer claim operation.
pub(super) fn reviewer_claim_command<W: Write>(
    command: ReviewerClaimCommand,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let actions = GitHubActions::from_loaded_config(cwd, &config);
    let store = ShipStateStore::new(state_dir.join("ship"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;

    match command {
        ReviewerClaimCommand::Acquire {
            pr,
            repo,
            head,
            reviewer,
            deadline_seconds,
            check_name,
        } => acquire(
            &store,
            &actions,
            &config,
            cwd,
            pr,
            repo,
            &head,
            &reviewer,
            deadline_seconds,
            &check_name,
            json_output,
            stdout,
        ),
        ReviewerClaimCommand::Validate {
            pr,
            repo,
            head,
            reviewer,
        } => validate(
            &store,
            &config,
            cwd,
            pr,
            repo,
            &head,
            &reviewer,
            json_output,
            stdout,
        ),
        ReviewerClaimCommand::Revoke { .. } => Err(CliFailure::new(
            1,
            "reviewer claim revoke is disabled until authenticated App receipt invalidation is implemented",
        )),
    }
}

fn resolve_repo(
    config: &LoadedConfig,
    cwd: &Path,
    repo: Option<String>,
) -> Result<String, CliFailure> {
    repo.filter(|value| !value.trim().is_empty())
        .or_else(|| gate::resolve_repo(config, cwd))
        .ok_or_else(|| CliFailure::new(2, "No repo detected. Pass --repo OWNER/REPO."))
}

fn validate_sha(head: &str) -> Result<(), CliFailure> {
    if head.len() != 40 || !head.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(CliFailure::new(
            2,
            format!("--head must be a full 40-character commit sha, not {head:?}"),
        ));
    }
    Ok(())
}

fn live_head(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    requested: &str,
) -> Result<String, CliFailure> {
    let raw = actions
        .run_gh_with_timeout(
            &[
                "api".to_owned(),
                format!("repos/{repo}/pulls/{pr}"),
                "--jq".to_owned(),
                ".head.sha".to_owned(),
            ],
            GITHUB_TIMEOUT,
        )
        .map_err(|error| CliFailure::new(2, format!("read PR head: {error}")))?;
    let live = raw.trim();
    if live.len() != 40 || !live.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(CliFailure::new(2, format!("PR #{pr} has no readable head")));
    }
    if !live.eq_ignore_ascii_case(requested) {
        return Err(CliFailure::new(
            1,
            format!("PR #{pr} moved: requested {requested}, live head is {live}"),
        ));
    }
    Ok(live.to_ascii_lowercase())
}

fn acquire<W: Write>(
    store: &ShipStateStore,
    actions: &GitHubActions,
    config: &LoadedConfig,
    cwd: &Path,
    pr: u64,
    requested_repo: Option<String>,
    head: &str,
    reviewer: &str,
    deadline_seconds: i64,
    check_name: &str,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    validate_sha(head)?;
    let reviewer = reviewer.trim();
    if reviewer.is_empty() {
        return Err(CliFailure::new(2, "--reviewer must not be empty"));
    }
    if !(1..=MAX_DEADLINE_SECONDS).contains(&deadline_seconds) {
        return Err(CliFailure::new(
            2,
            format!("--deadline-seconds must be between 1 and {MAX_DEADLINE_SECONDS}"),
        ));
    }
    if check_name.trim() != REVIEWER_CLAIM_CHECK_NAME {
        return Err(CliFailure::new(
            2,
            format!("--check-name is fixed to {REVIEWER_CLAIM_CHECK_NAME}"),
        ));
    }
    let repo = resolve_repo(config, cwd, requested_repo)?;
    let actions = actions.clone().with_repo_override(&repo);
    let head = live_head(&actions, &repo, pr, head)?;
    let claimed_at = Utc::now();
    let deadline_at = claimed_at + chrono::Duration::seconds(deadline_seconds);
    let request = ReviewerClaimRequest {
        repository: &repo,
        pr,
        head_sha: &head,
        reviewer,
        claimed_at,
        deadline_at,
        now: claimed_at,
    };
    let gh = GhClient::from_loaded_config(config)
        .map_err(|error| CliFailure::new(2, format!("GitHub auth config: {error}")))?;
    let mut transport =
        GhAppCheckRunTransport::new(gh, cwd, &repo, "shipyard-review", REVIEWER_CLAIM_APP_ID)
            .map_err(|error| CliFailure::new(2, format!("GitHub App transport: {error}")))?;
    let result = store.claim_reviewer_and_publish(request, |claim| {
        let receipt = crate::reviewer_receipt::request_from_claim(&repo, pr, claim);
        crate::reviewer_receipt::publish_or_reconcile(&mut transport, &receipt)
            .map(|_| claim.clone())
    });
    let claim = result.map_err(|error| CliFailure::new(1, error.to_string()))?;
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::String(repo.clone()));
        data.insert("pr".to_owned(), Value::from(pr));
        data.insert("head".to_owned(), Value::String(head));
        data.insert(
            "check_name".to_owned(),
            Value::String(REVIEWER_CLAIM_CHECK_NAME.to_owned()),
        );
        data.insert(
            "claim".to_owned(),
            serde_json::to_value(&claim).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        write_json_envelope(stdout, "reviewer-claim", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "reviewer claim acquired for {repo}#{pr} head {head} epoch {} until {}",
            claim.epoch,
            claim.deadline_at.to_rfc3339_opts(SecondsFormat::Secs, true)
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn validate<W: Write>(
    store: &ShipStateStore,
    config: &LoadedConfig,
    cwd: &Path,
    pr: u64,
    requested_repo: Option<String>,
    head: &str,
    reviewer: &str,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    validate_sha(head)?;
    let repo = resolve_repo(config, cwd, requested_repo)?;
    let reviewer = reviewer.trim();
    let state = store.get_scoped(&repo, pr);
    let claim = state
        .as_ref()
        .and_then(|state| state.active_reviewer_claim.as_ref());
    let current = state.as_ref().is_some_and(|state| {
        state.reviewer_claim_is_current(head, state.reviewer_claim_epoch, reviewer, Utc::now())
    });
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::String(repo.clone()));
        data.insert("pr".to_owned(), Value::from(pr));
        data.insert("head".to_owned(), Value::String(head.to_ascii_lowercase()));
        data.insert("reviewer".to_owned(), Value::String(reviewer.to_owned()));
        data.insert("current".to_owned(), Value::Bool(current));
        data.insert(
            "claim_epoch".to_owned(),
            claim.map_or(Value::Null, |claim| Value::from(claim.epoch)),
        );
        data.insert(
            "external_id".to_owned(),
            claim.map_or(Value::Null, |claim| {
                Value::String(claim.receipt_id(&repo, pr))
            }),
        );
        write_json_envelope(stdout, "reviewer-claim", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "reviewer claim: {}",
            if current { "current" } else { "refused" }
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(if current {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}
