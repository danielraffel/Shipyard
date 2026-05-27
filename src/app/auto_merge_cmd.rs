use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::Value;

use super::{
    CliFailure,
    cli::{MergeMethod, MergeResult},
};
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::ship_state::{ShipState, ShipStateStore};
use crate::watch::ship_terminal_verdict;

pub(super) struct AutoMergeRequest {
    pub(super) pr: u64,
    pub(super) merge_method: MergeMethod,
    pub(super) delete_branch: bool,
    pub(super) admin: bool,
    pub(super) pr_snapshot_file: Option<PathBuf>,
    pub(super) merge_command: Option<PathBuf>,
    pub(super) merge_result: Option<MergeResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum AutoMergeOutcome {
    AlreadyMerged,
    PrNotFound,
    InFlight {
        evidence: BTreeMap<String, String>,
    },
    TargetFailed {
        failing_targets: Vec<String>,
        evidence: BTreeMap<String, String>,
    },
    MergeFailed {
        error: String,
    },
    Merged {
        cleanup_warning: Option<String>,
    },
}

#[derive(Debug)]
pub(super) enum AutoMergeOperationError {
    Store(std::io::Error),
}

impl std::fmt::Display for AutoMergeOperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for AutoMergeOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
        }
    }
}

pub(super) fn execute_auto_merge(
    store: &ShipStateStore,
    cwd: &Path,
    request: &AutoMergeRequest,
) -> Result<AutoMergeOutcome, AutoMergeOperationError> {
    let lock = store
        .lock_pr(request.pr)
        .map_err(AutoMergeOperationError::Store)?;
    let Some(state) = store.get_locked(request.pr, &lock) else {
        return Ok(
            if pr_is_merged(request.pr, cwd, request.pr_snapshot_file.as_deref()) {
                AutoMergeOutcome::AlreadyMerged
            } else {
                AutoMergeOutcome::PrNotFound
            },
        );
    };

    match ship_terminal_verdict(&state) {
        None => Ok(AutoMergeOutcome::InFlight {
            evidence: state.evidence_snapshot,
        }),
        Some(false) => Ok(AutoMergeOutcome::TargetFailed {
            failing_targets: failing_required_targets(&state),
            evidence: state.evidence_snapshot,
        }),
        Some(true) => {
            if let Err(error) = merge_pr(
                request.pr,
                cwd,
                request.merge_method,
                request.delete_branch,
                request.admin,
                request.merge_command.as_deref(),
                request.merge_result,
            ) {
                if merge_error_confirms_merged(&error)
                    || pr_is_merged(request.pr, cwd, request.pr_snapshot_file.as_deref())
                {
                    store
                        .archive_locked(request.pr, &lock)
                        .map_err(AutoMergeOperationError::Store)?;
                    return Ok(AutoMergeOutcome::Merged {
                        cleanup_warning: Some(error),
                    });
                }
                return Ok(AutoMergeOutcome::MergeFailed { error });
            }
            store
                .archive_locked(request.pr, &lock)
                .map_err(AutoMergeOperationError::Store)?;
            Ok(AutoMergeOutcome::Merged {
                cleanup_warning: None,
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn auto_merge<W: Write>(
    store: &ShipStateStore,
    cwd: &Path,
    pr: u64,
    merge_method: MergeMethod,
    delete_branch: bool,
    admin: bool,
    pr_snapshot_file: Option<PathBuf>,
    merge_command: Option<PathBuf>,
    merge_result: Option<MergeResult>,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let request = AutoMergeRequest {
        pr,
        merge_method,
        delete_branch,
        admin,
        pr_snapshot_file,
        merge_command,
        merge_result,
    };
    match execute_auto_merge(store, cwd, &request)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
    {
        AutoMergeOutcome::AlreadyMerged => {
            render_event(
                stdout,
                json,
                "already-merged",
                fields([("pr", Value::from(pr))]),
            )?;
            Ok(ExitCode::SUCCESS)
        }
        AutoMergeOutcome::PrNotFound => {
            render_event(
                stdout,
                json,
                "pr-not-found",
                fields([("pr", Value::from(pr))]),
            )?;
            Ok(ExitCode::from(2))
        }
        AutoMergeOutcome::InFlight { evidence } => {
            render_event(
                stdout,
                json,
                "in-flight",
                fields([
                    ("pr", Value::from(pr)),
                    (
                        "evidence",
                        serde_json::to_value(&evidence)
                            .map_err(|error| CliFailure::new(1, error.to_string()))?,
                    ),
                ]),
            )?;
            Ok(ExitCode::from(3))
        }
        AutoMergeOutcome::TargetFailed {
            failing_targets,
            evidence,
        } => {
            render_event(
                stdout,
                json,
                "target-failed",
                fields([
                    ("pr", Value::from(pr)),
                    (
                        "failing_targets",
                        serde_json::to_value(&failing_targets)
                            .map_err(|error| CliFailure::new(1, error.to_string()))?,
                    ),
                    (
                        "evidence",
                        serde_json::to_value(&evidence)
                            .map_err(|error| CliFailure::new(1, error.to_string()))?,
                    ),
                ]),
            )?;
            Ok(ExitCode::from(1))
        }
        AutoMergeOutcome::MergeFailed { error } => {
            render_event(
                stdout,
                json,
                "merge-failed",
                fields([("pr", Value::from(pr)), ("error", Value::from(error))]),
            )?;
            Ok(ExitCode::from(1))
        }
        AutoMergeOutcome::Merged { cleanup_warning } => {
            let mut data = fields([("pr", Value::from(pr))]);
            if let Some(warning) = cleanup_warning {
                data.insert("cleanup_warning".to_owned(), Value::from(warning));
            }
            render_event(stdout, json, "merged", data)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn pr_is_merged(pr: u64, cwd: &Path, snapshot_file: Option<&Path>) -> bool {
    let payload = if let Some(path) = snapshot_file {
        std::fs::read_to_string(path).ok()
    } else {
        let Ok(client) = gh_client(cwd) else {
            return false;
        };
        let Ok(mut command) = gh(&client, cwd) else {
            return false;
        };
        let output = command
            .args(["pr", "view", &pr.to_string(), "--json", "state"])
            .output()
            .ok();
        let Some(output) = output else {
            return false;
        };
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    payload
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| {
            value
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|state| state.eq_ignore_ascii_case("merged"))
}

fn gh_client(cwd: &Path) -> Result<GhClient, String> {
    GhClient::from_cwd(RuntimeMode::Shipyard, cwd)
        .map_err(|error| format!("github auth config failed: {error}"))
}

fn gh(client: &GhClient, cwd: &Path) -> Result<Command, String> {
    client
        .prepare_command(cwd, None, GhSupervision::Supervised, GhAuthPolicy::Default)
        .map_err(|error| format!("gh command preparation failed: {error}"))
}

fn merge_pr(
    pr: u64,
    cwd: &Path,
    merge_method: MergeMethod,
    delete_branch: bool,
    admin: bool,
    merge_command: Option<&Path>,
    merge_result: Option<MergeResult>,
) -> Result<(), String> {
    match merge_result {
        Some(MergeResult::Success) => return Ok(()),
        Some(MergeResult::Failure) => return Err("simulated merge failure".to_owned()),
        None => {}
    }

    let custom_command = merge_command.is_some();
    let client = if custom_command {
        None
    } else {
        Some(gh_client(cwd)?)
    };
    let mut command = if let Some(merge_command) = merge_command {
        Command::new(merge_command)
    } else {
        gh(
            client
                .as_ref()
                .expect("built-in merge should have gh client"),
            cwd,
        )?
    };
    if !custom_command {
        command.args(["pr", "merge", &pr.to_string()]);
    }
    command.arg(merge_method.gh_flag());
    if delete_branch {
        command.arg("--delete-branch");
    }
    if admin {
        command.arg("--admin");
    }
    let output = command
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("failed to run merge command: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let message = if stderr.is_empty() { stdout } else { stderr };

    // GraphQL exhausted but REST still has budget? `gh pr merge` uses GraphQL
    // for the merge state probe; the actual merge atom is REST
    // (PUT /repos/:r/pulls/:n/merge), so fall back to a direct REST call
    // rather than failing the ship. Matches src/pr.rs's pattern for
    // gh pr list / create / view.
    if !custom_command && crate::pr::is_graphql_rate_limited(&message) {
        let client = client
            .as_ref()
            .expect("built-in merge should have gh client");
        crate::pr::report_rate_limit_fallback_with_client(client, "gh pr merge", cwd);
        return merge_pr_rest(client, pr, cwd, merge_method, delete_branch);
    }
    Err(message)
}

/// REST fallback for `gh pr merge` when GraphQL is rate-limited.
///
/// `gh pr merge` queries the PR's mergeable state via GraphQL before issuing
/// the actual merge POST. When GraphQL is at 0/5000 the call fails, but
/// REST is independent (`PUT /repos/:repo/pulls/:n/merge`) and usually has
/// budget left. This function bypasses the GraphQL probe and calls REST
/// directly through `gh api`, then optionally deletes the head branch the
/// same way `gh pr merge --delete-branch` would.
///
/// Race protection (issue #266): the original head SHA is fetched first
/// and passed to the merge PUT as `sha=<oid>`, so GitHub rejects the
/// merge server-side if the head changed between our snapshot and the
/// PUT. On a "Base branch was modified" 405, we re-fetch the head once
/// and retry exactly once if and only if the head SHA is unchanged
/// (i.e., the modification was purely on the base branch — typical
/// when a sibling PR lands during our merge attempt).
fn merge_pr_rest(
    client: &GhClient,
    pr: u64,
    cwd: &Path,
    merge_method: MergeMethod,
    delete_branch: bool,
) -> Result<(), String> {
    let repo = repo_slug_for_rest(cwd)?;
    let info = pr_head_info_rest(client, &repo, pr, cwd)?;
    let endpoint = format!("repos/{repo}/pulls/{pr}/merge");

    let first = attempt_merge_put(client, &endpoint, &info.sha, merge_method, cwd);
    match first {
        Ok(()) => {}
        Err(error) if is_base_modified_405(&error) => {
            // Re-fetch head; only retry if the head SHA is unchanged
            // (i.e., a new commit did NOT land on the head branch).
            // Codex review on PR construction: head_sha invariance is
            // the load-bearing check; `mergeable` can be stale.
            let refreshed = pr_head_info_rest(client, &repo, pr, cwd)?;
            if refreshed.sha != info.sha {
                return Err(format!(
                    "REST fallback: PR head moved from {} to {} between merge attempts; refusing to retry",
                    short_sha(&info.sha),
                    short_sha(&refreshed.sha)
                ));
            }
            attempt_merge_put(client, &endpoint, &refreshed.sha, merge_method, cwd)
                .map_err(|second| format!("{error} (retry: {second})"))?;
        }
        Err(error) => return Err(error),
    }

    if delete_branch {
        // Best-effort delete; mirrors `gh pr merge --delete-branch` which
        // also tolerates a missing branch silently.
        if let Ok(mut command) = gh(client, cwd) {
            let _ = command
                .args([
                    "api",
                    "-X",
                    "DELETE",
                    &format!("repos/{repo}/git/refs/heads/{}", info.head_ref),
                ])
                .status();
        }
    }
    Ok(())
}

/// Issue the PUT /repos/:r/pulls/:n/merge call with the merge method
/// and a server-side `sha` race guard. Returns Ok on 2xx, Err with
/// the gh stderr (or stdout when stderr empty) on any non-2xx.
fn attempt_merge_put(
    client: &GhClient,
    endpoint: &str,
    head_sha: &str,
    merge_method: MergeMethod,
    cwd: &Path,
) -> Result<(), String> {
    let output = gh(client, cwd)?
        .args([
            "api",
            "-X",
            "PUT",
            endpoint,
            "-f",
            &format!("merge_method={}", merge_method.rest_value()),
            "-f",
            &format!("sha={head_sha}"),
        ])
        .output()
        .map_err(|error| format!("REST fallback: failed to invoke gh api: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Err(format!(
        "REST fallback: gh api PUT {endpoint} failed: {}",
        if stderr.is_empty() { stdout } else { stderr }
    ))
}

/// Detect the canonical GitHub error body for "the base branch
/// advanced between the merge check and the merge call". GitHub
/// returns this as HTTP 405 with body
/// `{"message":"Base branch was modified. ..."}`. The 405 itself
/// surfaces in `gh api` stderr alongside the body text, so a
/// substring match on the message is the reliable detector.
pub(crate) fn is_base_modified_405(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("405") && lower.contains("base branch was modified")
}

fn short_sha(sha: &str) -> &str {
    if sha.len() > 7 { &sha[..7] } else { sha }
}

/// Subset of the PR REST payload that the REST merge path needs.
struct PrHeadInfo {
    head_ref: String,
    sha: String,
}

fn pr_head_info_rest(
    client: &GhClient,
    repo: &str,
    pr: u64,
    cwd: &Path,
) -> Result<PrHeadInfo, String> {
    let output = gh(client, cwd)?
        .args(["api", &format!("repos/{repo}/pulls/{pr}")])
        .output()
        .map_err(|error| format!("REST fallback: gh api PR fetch failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "REST fallback: gh api repos/{repo}/pulls/{pr} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("REST fallback: failed to parse PR JSON: {error}"))?;
    let head = value
        .get("head")
        .ok_or_else(|| "REST fallback: PR JSON missing head".to_owned())?;
    let head_ref = head
        .get("ref")
        .and_then(Value::as_str)
        .ok_or_else(|| "REST fallback: PR JSON missing head.ref".to_owned())?
        .to_owned();
    let sha = head
        .get("sha")
        .and_then(Value::as_str)
        .ok_or_else(|| "REST fallback: PR JSON missing head.sha".to_owned())?
        .to_owned();
    Ok(PrHeadInfo { head_ref, sha })
}

fn repo_slug_for_rest(cwd: &Path) -> Result<String, String> {
    let output = crate::supervised::git_supervised()
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("REST fallback: git remote probe failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "REST fallback: git remote probe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let remote = String::from_utf8_lossy(&output.stdout);
    parse_github_remote_slug(remote.trim()).ok_or_else(|| {
        format!(
            "REST fallback: remote.origin.url is not a supported GitHub remote: {}",
            remote.trim()
        )
    })
}

fn parse_github_remote_slug(remote: &str) -> Option<String> {
    crate::gh::parse_github_remote_slug(remote)
}

// (`pr_head_branch_rest` was superseded by `pr_head_info_rest` which
//  returns both the head ref and the head SHA so the merge PUT can
//  use the SHA as a race-guard. See issue #266.)

fn merge_error_confirms_merged(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("pull request") && lower.contains("already merged")
}

fn failing_required_targets(state: &ShipState) -> Vec<String> {
    let advisory_targets = state
        .dispatched_runs
        .iter()
        .filter(|run| !run.required)
        .map(|run| run.target.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    state
        .evidence_snapshot
        .iter()
        .filter(|(target, status)| *status != "pass" && !advisory_targets.contains(target.as_str()))
        .map(|(target, _)| target.clone())
        .collect()
}

fn render_event<W: Write>(
    stdout: &mut W,
    json: bool,
    event: &str,
    mut data: BTreeMap<String, Value>,
) -> Result<(), CliFailure> {
    if json {
        data.insert("event".to_owned(), Value::from(event.to_owned()));
        write_json_envelope(stdout, "auto-merge", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }

    let pr = data.get("pr").and_then(Value::as_u64).unwrap_or_default();
    match event {
        "already-merged" => writeln!(stdout, "PR #{pr}: already merged - idempotent no-op."),
        "pr-not-found" => writeln!(
            stdout,
            "PR #{pr}: no ship state found (typo / never shipped)."
        ),
        "in-flight" => writeln!(
            stdout,
            "PR #{pr}: ship still in flight - evidence {}.",
            data.get("evidence").unwrap_or(&Value::Null)
        ),
        "target-failed" => {
            let targets = data
                .get("failing_targets")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            writeln!(
                stdout,
                "PR #{pr}: refusing to merge - targets failed: {targets}"
            )
        }
        "merge-failed" => writeln!(
            stdout,
            "PR #{pr}: merge attempt failed - {}",
            data.get("error").and_then(Value::as_str).unwrap_or("")
        ),
        "merged" => {
            if let Some(warning) = data.get("cleanup_warning").and_then(Value::as_str) {
                writeln!(stdout, "PR #{pr}: merged. Cleanup warning: {warning}")
            } else {
                writeln!(stdout, "PR #{pr}: merged.")
            }
        }
        _ => Ok(()),
    }
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn fields(items: impl IntoIterator<Item = (&'static str, Value)>) -> BTreeMap<String, Value> {
    items
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 405 base-branch-modified detector (issue #266) ──────────────────

    #[test]
    fn is_base_modified_405_matches_canonical_github_error_body() {
        let msg = "HTTP 405: Base branch was modified. Review and try the merge again.";
        assert!(is_base_modified_405(msg));
    }

    #[test]
    fn is_base_modified_405_is_case_insensitive() {
        let msg = "http 405: BASE BRANCH WAS MODIFIED";
        assert!(is_base_modified_405(msg));
    }

    #[test]
    fn is_base_modified_405_rejects_unrelated_405_errors() {
        // 405 on a different endpoint or with a different message must not match —
        // we only retry the merge for the specific base-modified case.
        assert!(!is_base_modified_405(
            "HTTP 405: Method Not Allowed (Required status check is pending)"
        ));
    }

    #[test]
    fn is_base_modified_405_rejects_base_modified_without_405_code() {
        // Defense: only retry when GitHub returned the 405 status, not on
        // arbitrary text containing the phrase.
        assert!(!is_base_modified_405("Base branch was modified."));
    }

    // ── short_sha helper ────────────────────────────────────────────────

    #[test]
    fn short_sha_truncates_full_sha_to_seven_chars() {
        let full = "deadbeefcafef00d1234567890abcdef12345678";
        assert_eq!(short_sha(full), "deadbee");
    }

    #[test]
    fn short_sha_returns_input_when_already_short() {
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
    }
}
