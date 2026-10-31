//! CLI transport for durable reviewer-dispatch receipts.
//!
//! These commands only record launcher evidence and reconcile it with
//! independently authenticated observations. They never launch a reviewer or
//! create a verdict, so a missing or stale observation remains fail-closed.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use super::CliFailure;
use super::cli::{
    ReviewerDispatchCommand, ReviewerDispatchObserveArgs, ReviewerDispatchRecordArgs,
};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::head_approval::Approval;
use crate::identity::RuntimeMode;
use crate::output::write_pretty_json;
use crate::review_verdict::review_verdict;
use crate::reviewer_dispatch::{
    self, DispatchReceipt, LaunchResult, ObservedVerdict, Reconciliation, ReconciliationStatus,
};

/// Run a reviewer-dispatch receipt operation.
pub(super) fn reviewer_dispatch_command<W: Write>(
    command: ReviewerDispatchCommand,
    state_dir: &Path,
    mode: RuntimeMode,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        ReviewerDispatchCommand::Record(args) => record_command(&args, state_dir, json, stdout),
        ReviewerDispatchCommand::Observe(args) => {
            observe_command(&args, state_dir, mode, cwd, json, stdout)
        }
    }
}

fn record_command<W: Write>(
    args: &ReviewerDispatchRecordArgs,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let result = parse_result(&args.result)?;
    let started_at = parse_time("started_at", &args.started_at)?;
    let deadline_at = parse_time("deadline_at", &args.deadline_at)?;
    let receipt = DispatchReceipt {
        schema: reviewer_dispatch::SCHEMA.to_owned(),
        repo: args.repo.clone(),
        pr: args.pr,
        head_sha: args.head.clone(),
        reviewer: args.reviewer.clone(),
        attempt_id: args.attempt_id.clone(),
        attempt: args.attempt,
        launcher_identity: args.launcher_identity.clone(),
        session_id: args.session_id.clone(),
        dispatch_id: args.dispatch_id.clone(),
        started_at,
        deadline_at,
        result,
        error: args.error.clone(),
    };
    let path = args
        .store
        .clone()
        .unwrap_or_else(|| reviewer_dispatch::default_path(state_dir, &args.repo));
    reviewer_dispatch::append(&path, &receipt).map_err(|error| CliFailure::new(2, error))?;
    if json {
        write_pretty_json(stdout, &receipt)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "reviewer-dispatch: recorded attempt {} for {} PR #{} at {}",
            receipt.attempt,
            receipt.repo,
            receipt.pr,
            path.display()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn observe_command<W: Write>(
    args: &ReviewerDispatchObserveArgs,
    state_dir: &Path,
    mode: RuntimeMode,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if args.head.len() != 40 || !args.head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CliFailure::new(
            2,
            "--head must be a full 40-character commit sha",
        ));
    }
    let path = args
        .store
        .clone()
        .unwrap_or_else(|| reviewer_dispatch::default_path(state_dir, &args.repo));
    let receipts = reviewer_dispatch::read(&path).map_err(|error| CliFailure::new(2, error))?;
    let latest =
        reviewer_dispatch::latest_for_subject(&receipts, &args.repo, args.pr, &args.reviewer)
            .ok_or_else(|| {
                CliFailure::new(
                    2,
                    format!(
                        "no reviewer-dispatch receipt for {} PR #{} reviewer {} exact head {}",
                        args.repo, args.pr, args.reviewer, args.head
                    ),
                )
            })?;
    let explicit_now = args
        .now
        .as_deref()
        .map(|value| parse_time("now", value))
        .transpose()?;
    let observation = authenticated_verdict(args, mode, cwd)?;
    let now = explicit_now.unwrap_or_else(Utc::now);
    let reconciliation = reconcile_and_persist(
        &receipts,
        latest,
        &observation.live_head,
        observation.verdict.as_ref(),
        now,
        args.max_retries,
        state_dir,
        &args.repo,
        args.store.as_deref(),
    )?;
    render_reconciliation(stdout, json, &reconciliation)?;
    if reconciliation.status == ReconciliationStatus::VerdictPosted {
        Ok(ExitCode::SUCCESS)
    } else {
        Err(CliFailure::new(
            1,
            format!(
                "reviewer-dispatch observation is {}: {}",
                reconciliation.status, reconciliation.detail
            ),
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_and_persist(
    receipts: &[DispatchReceipt],
    latest: &DispatchReceipt,
    current_head: &str,
    verdict: Option<&ObservedVerdict>,
    now: DateTime<Utc>,
    max_retries: u32,
    state_dir: &Path,
    repo: &str,
    store: Option<&Path>,
) -> Result<Reconciliation, CliFailure> {
    let retry_allowed = reviewer_dispatch::retry_budget_available(receipts, latest, max_retries);
    let reconciliation =
        reviewer_dispatch::reconcile(latest, now, current_head, verdict, retry_allowed);
    let observation_path = store.map_or_else(
        || reviewer_dispatch::default_observation_path(state_dir, repo),
        reviewer_dispatch::observation_path_for_store,
    );
    reviewer_dispatch::append_observation(&observation_path, &reconciliation)
        .map_err(|error| CliFailure::new(2, error))?;
    Ok(reconciliation)
}

struct AuthenticatedObservation {
    live_head: String,
    verdict: Option<ObservedVerdict>,
}

fn authenticated_verdict(
    args: &ReviewerDispatchObserveArgs,
    mode: RuntimeMode,
    cwd: &Path,
) -> Result<AuthenticatedObservation, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let actions = GitHubActions::from_loaded_config(cwd, &config);
    let run_gh = |gh_args: &[String]| {
        actions
            .run_gh_with_timeout(gh_args, std::time::Duration::from_mins(1))
            .map_err(|error| error.to_string())
    };
    let verdict =
        review_verdict(&run_gh, &args.repo, args.pr, Some(&args.head), false).map_err(|error| {
            CliFailure::new(2, format!("review-verdict observation failed: {error}"))
        })?;
    let live_head = verdict
        .live_head
        .clone()
        .unwrap_or_else(|| verdict.head.clone());
    let approval_at = direct_approval_time(&run_gh, args, &verdict)?;
    let trusted = verdict.required
        && verdict.allowed()
        && verdict
            .approval
            .as_ref()
            .is_some_and(|approval| direct_exact_approval(approval, &args.reviewer))
        && live_head.eq_ignore_ascii_case(&args.head)
        && approval_at.is_some();
    let observed_verdict = approval_at.and_then(|posted_at| {
        trusted.then(|| ObservedVerdict::trusted_exact_head(verdict.head, posted_at))
    });
    Ok(AuthenticatedObservation {
        live_head,
        verdict: observed_verdict,
    })
}

fn direct_approval_time(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    args: &ReviewerDispatchObserveArgs,
    verdict: &crate::review_verdict::Verdict,
) -> Result<Option<DateTime<Utc>>, CliFailure> {
    let Some(approval) = verdict.approval.as_ref() else {
        return Ok(None);
    };
    let expected_login = match approval {
        Approval::Review { login, .. } | Approval::Marker { login, .. } => {
            if !login.eq_ignore_ascii_case(&args.reviewer) {
                return Ok(None);
            }
            login
        }
        Approval::Carried { .. } | Approval::Rollup { .. } => return Ok(None),
    };
    let review_rows = run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        format!("repos/{}/pulls/{}/reviews", args.repo, args.pr),
        "--jq".to_owned(),
        ".[] | {login: .user.login, state: .state, commit_id: .commit_id, submitted_at: .submitted_at}".to_owned(),
    ])
    .map_err(|error| CliFailure::new(2, format!("review observation failed: {error}")))?;
    let mut latest_review_at = None;
    for row in review_rows.lines().filter(|line| !line.trim().is_empty()) {
        let row: Value = serde_json::from_str(row).map_err(|error| {
            CliFailure::new(2, format!("malformed review observation: {error}"))
        })?;
        if row.get("state").and_then(Value::as_str) == Some("APPROVED")
            && row
                .get("login")
                .and_then(Value::as_str)
                .is_some_and(|login| login.eq_ignore_ascii_case(expected_login))
            && row
                .get("commit_id")
                .and_then(Value::as_str)
                .is_some_and(|sha| sha.eq_ignore_ascii_case(&args.head))
            && let Some(at) = row.get("submitted_at").and_then(Value::as_str)
        {
            let parsed = parse_time("review submitted_at", at)?;
            if latest_review_at.is_none_or(|current| parsed > current) {
                latest_review_at = Some(parsed);
            }
        }
    }
    if latest_review_at.is_some() {
        return Ok(latest_review_at);
    }
    let comment_rows = run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        format!("repos/{}/issues/{}/comments", args.repo, args.pr),
        "--jq".to_owned(),
        ".[] | {login: .user.login, body: .body, created_at: .created_at}".to_owned(),
    ])
    .map_err(|error| CliFailure::new(2, format!("comment observation failed: {error}")))?;
    let marker = format!("verdict: APPROVE head:{}", args.head);
    let mut latest_comment_at = None;
    for row in comment_rows.lines().filter(|line| !line.trim().is_empty()) {
        let row: Value = serde_json::from_str(row).map_err(|error| {
            CliFailure::new(2, format!("malformed comment observation: {error}"))
        })?;
        if row
            .get("login")
            .and_then(Value::as_str)
            .is_some_and(|login| login.eq_ignore_ascii_case(expected_login))
            && row
                .get("body")
                .and_then(Value::as_str)
                .is_some_and(|body| body.lines().any(|line| line.trim() == marker))
            && let Some(at) = row.get("created_at").and_then(Value::as_str)
        {
            let parsed = parse_time("verdict comment created_at", at)?;
            if latest_comment_at.is_none_or(|current| parsed > current) {
                latest_comment_at = Some(parsed);
            }
        }
    }
    Ok(latest_comment_at)
}

fn direct_exact_approval(approval: &Approval, reviewer: &str) -> bool {
    match approval {
        Approval::Review { login, .. } | Approval::Marker { login, .. } => {
            login.eq_ignore_ascii_case(reviewer)
        }
        Approval::Carried { .. } | Approval::Rollup { .. } => false,
    }
}

fn render_reconciliation<W: Write>(
    stdout: &mut W,
    json: bool,
    reconciliation: &Reconciliation,
) -> Result<(), CliFailure> {
    if json {
        write_pretty_json(stdout, reconciliation)
            .map_err(|error| CliFailure::new(1, error.to_string()))
    } else {
        writeln!(
            stdout,
            "reviewer-dispatch: {} (attempt {}, retry_allowed={}): {}",
            reconciliation.status,
            reconciliation.attempt,
            reconciliation.retry_allowed,
            reconciliation.detail
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))
    }
}

fn parse_result(value: &str) -> Result<LaunchResult, CliFailure> {
    match value {
        "requested" => Ok(LaunchResult::Requested),
        "started" => Ok(LaunchResult::Started),
        "failed" => Ok(LaunchResult::Failed),
        other => Err(CliFailure::new(
            2,
            format!(
                "invalid reviewer-dispatch result {other:?}; expected requested, started, or failed"
            ),
        )),
    }
}

fn parse_time(name: &str, value: &str) -> Result<DateTime<Utc>, CliFailure> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| {
            CliFailure::new(
                2,
                format!("invalid {name} RFC 3339 timestamp {value:?}: {error}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn record_args(store: &Path) -> ReviewerDispatchRecordArgs {
        ReviewerDispatchRecordArgs {
            repo: "owner/repo".to_owned(),
            pr: 7,
            head: "a".repeat(40),
            reviewer: "infra-review2".to_owned(),
            attempt_id: "attempt-1".to_owned(),
            attempt: 1,
            launcher_identity: "m3/launcher".to_owned(),
            session_id: Some("session-1".to_owned()),
            dispatch_id: "dispatch-1".to_owned(),
            started_at: "2026-10-29T00:00:00Z".to_owned(),
            deadline_at: "2026-10-29T00:10:00Z".to_owned(),
            result: "started".to_owned(),
            error: None,
            store: Some(store.to_owned()),
        }
    }

    #[test]
    fn record_then_observe_requires_trusted_exact_head_verdict() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("receipts.jsonl");
        let args = record_args(&store);
        let mut output = Vec::new();
        assert_eq!(
            record_command(&args, directory.path(), true, &mut output).unwrap(),
            ExitCode::SUCCESS
        );

        let receipts = reviewer_dispatch::read(&store).unwrap();
        let latest =
            reviewer_dispatch::latest_for_subject(&receipts, &args.repo, args.pr, &args.reviewer)
                .unwrap();
        let observed_at = DateTime::parse_from_rfc3339("2026-10-29T00:05:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let reconciliation = reconcile_and_persist(
            &receipts,
            latest,
            &args.head,
            None,
            observed_at,
            1,
            directory.path(),
            &args.repo,
            Some(&store),
        )
        .unwrap();
        assert_eq!(reconciliation.status, ReconciliationStatus::Unobserved);
        assert!(!reconciliation.retry_allowed);

        let verdict = ObservedVerdict::trusted_exact_head(args.head.clone(), observed_at);
        let reconciliation = reconcile_and_persist(
            &receipts,
            latest,
            &args.head,
            Some(&verdict),
            observed_at,
            1,
            directory.path(),
            &args.repo,
            Some(&store),
        )
        .unwrap();
        assert_eq!(reconciliation.status, ReconciliationStatus::VerdictPosted);
    }
}
