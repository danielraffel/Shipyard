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
use serde_json::{Value, json};

use super::CliFailure;
use super::cli::ReviewerClaimCommand;
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::landability::gate;
use crate::output::write_json_envelope;
use crate::ship_state::{ReviewerClaim, ReviewerClaimRequest, ShipStateStore};

const GITHUB_TIMEOUT: Duration = Duration::from_mins(1);
const MAX_DEADLINE_SECONDS: i64 = 24 * 60 * 60;

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
        ReviewerClaimCommand::Revoke {
            pr,
            repo,
            head,
            epoch,
            reviewer,
            check_name,
        } => revoke(
            &store,
            &actions,
            &config,
            cwd,
            pr,
            repo,
            &head,
            epoch,
            &reviewer,
            &check_name,
            json_output,
            stdout,
        ),
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
    let check_name = check_name.trim();
    if check_name.is_empty() {
        return Err(CliFailure::new(2, "--check-name must not be empty"));
    }
    let repo = resolve_repo(config, cwd, requested_repo)?;
    let head = live_head(actions, &repo, pr, head)?;
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
    let result = store.claim_reviewer_and_publish(request, |claim| {
        publish_receipt(actions, &repo, pr, check_name, claim).map(|_| claim.clone())
    });
    let claim = result.map_err(|error| CliFailure::new(1, error.to_string()))?;
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::String(repo));
        data.insert("pr".to_owned(), Value::from(pr));
        data.insert("head".to_owned(), Value::String(head));
        data.insert(
            "check_name".to_owned(),
            Value::String(check_name.to_owned()),
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

fn publish_receipt(
    actions: &GitHubActions,
    repo: &str,
    _pr: u64,
    check_name: &str,
    claim: &ReviewerClaim,
) -> Result<(), String> {
    let summary = serde_json::to_string(&json!({
        "schema": "shipyard-reviewer-claim/v1",
        "repo": repo,
        "pr": _pr,
        "head_sha": claim.head_sha,
        "epoch": claim.epoch,
        "reviewer": claim.reviewer,
        "claimed_at": claim.claimed_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        "deadline_at": claim.deadline_at.to_rfc3339_opts(SecondsFormat::Secs, true),
    }))
    .map_err(|error| error.to_string())?;
    let payload = json!({
        "name": check_name,
        "head_sha": claim.head_sha,
        "status": "completed",
        "conclusion": "success",
        "output": {
            "title": "Durable reviewer claim",
            "summary": summary,
        }
    });
    let body = serde_json::to_string(&payload).map_err(|error| error.to_string())?;
    let mut file = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
    file.write_all(body.as_bytes())
        .map_err(|error| error.to_string())?;
    actions
        .run_gh_with_timeout(
            &[
                "api".to_owned(),
                "--method".to_owned(),
                "POST".to_owned(),
                format!("repos/{repo}/check-runs"),
                "--input".to_owned(),
                file.path().to_string_lossy().into_owned(),
            ],
            GITHUB_TIMEOUT,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
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
    let current = store.get_scoped(&repo, pr).is_some_and(|state| {
        state.reviewer_claim_is_current(head, state.reviewer_claim_epoch, reviewer, Utc::now())
    });
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::String(repo));
        data.insert("pr".to_owned(), Value::from(pr));
        data.insert("head".to_owned(), Value::String(head.to_ascii_lowercase()));
        data.insert("reviewer".to_owned(), Value::String(reviewer.to_owned()));
        data.insert("current".to_owned(), Value::Bool(current));
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

fn revoke<W: Write>(
    store: &ShipStateStore,
    actions: &GitHubActions,
    config: &LoadedConfig,
    cwd: &Path,
    pr: u64,
    requested_repo: Option<String>,
    head: &str,
    epoch: u64,
    reviewer: &str,
    check_name: &str,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    validate_sha(head)?;
    if epoch == 0 || reviewer.trim().is_empty() {
        return Err(CliFailure::new(
            2,
            "--epoch must be positive and --reviewer non-empty",
        ));
    }
    let check_name = check_name.trim();
    if check_name.is_empty() {
        return Err(CliFailure::new(2, "--check-name must not be empty"));
    }
    let repo = resolve_repo(config, cwd, requested_repo)?;
    let revoked = store
        .with_pr_state_scoped_locked(&repo, pr, |state| {
            let Some(state) = state.as_mut() else {
                return Err(format!("no active ship state for {repo} PR #{pr}").into());
            };
            let matches = state.active_reviewer_claim.as_ref().is_some_and(|claim| {
                state.head_sha.eq_ignore_ascii_case(head)
                    && claim.head_sha.eq_ignore_ascii_case(head)
                    && claim.epoch == epoch
                    && claim.reviewer == reviewer.trim()
                    && state.reviewer_claim_epoch == epoch
            });
            if !matches {
                return Ok(false);
            }
            // Hold the same PR lock while the external receipt is invalidated;
            // a stale revoke must never damage a newer claimant's evidence.
            invalidate_receipts(actions, &repo, pr, head, check_name)
                .map_err(|error| error.message)?;
            Ok(state.revoke_reviewer_claim(head, epoch, reviewer.trim()))
        })
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::String(repo));
        data.insert("pr".to_owned(), Value::from(pr));
        data.insert("revoked".to_owned(), Value::Bool(revoked));
        write_json_envelope(stdout, "reviewer-claim", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "reviewer claim: {}",
            if revoked { "revoked" } else { "not current" }
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(if revoked {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn invalidate_receipts(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    head: &str,
    check_name: &str,
) -> Result<(), CliFailure> {
    let raw = actions
        .run_gh_with_timeout(
            &[
                "api".to_owned(),
                "--paginate".to_owned(),
                "--slurp".to_owned(),
                format!("repos/{repo}/commits/{head}/check-runs?per_page=100"),
            ],
            GITHUB_TIMEOUT,
        )
        .map_err(|error| CliFailure::new(1, format!("read reviewer claim receipts: {error}")))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| CliFailure::new(1, format!("parse reviewer claim receipts: {error}")))?;
    let pages: Vec<&Value> = match &value {
        Value::Array(pages) => pages.iter().collect(),
        Value::Object(_) => vec![&value],
        _ => Vec::new(),
    };
    if pages.is_empty() {
        return Err(CliFailure::new(
            1,
            "reviewer claim check-runs response omitted check_runs",
        ));
    }
    let runs = pages
        .into_iter()
        .flat_map(|page| page.get("check_runs").and_then(Value::as_array))
        .flatten();
    for run in runs.filter(|run| receipt_matches(run, repo, pr, head, check_name)) {
        let id = run.get("id").and_then(Value::as_u64).unwrap_or_default();
        let body = serde_json::to_string(&json!({
            "status": "completed",
            "conclusion": "failure",
            "output": {
                "title": "Reviewer claim revoked",
                "summary": "The durable reviewer claim was explicitly revoked.",
            }
        }))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        let mut file = tempfile::NamedTempFile::new()
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        file.write_all(body.as_bytes())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        actions
            .run_gh_with_timeout(
                &[
                    "api".to_owned(),
                    "--method".to_owned(),
                    "PATCH".to_owned(),
                    format!("repos/{repo}/check-runs/{id}"),
                    "--input".to_owned(),
                    file.path().to_string_lossy().into_owned(),
                ],
                GITHUB_TIMEOUT,
            )
            .map_err(|error| {
                CliFailure::new(1, format!("revoke reviewer claim receipt: {error}"))
            })?;
    }
    Ok(())
}

fn receipt_matches(run: &Value, repo: &str, pr: u64, head: &str, check_name: &str) -> bool {
    if run.get("name").and_then(Value::as_str) != Some(check_name)
        || run.get("id").and_then(Value::as_u64).is_none()
    {
        return false;
    }
    let Some(summary) = run
        .get("output")
        .and_then(Value::as_object)
        .and_then(|output| output.get("summary"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(summary) else {
        return false;
    };
    value.get("schema").and_then(Value::as_str) == Some("shipyard-reviewer-claim/v1")
        && value.get("repo").and_then(Value::as_str) == Some(repo)
        && value.get("pr").and_then(Value::as_u64) == Some(pr)
        && value
            .get("head_sha")
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case(head))
}
