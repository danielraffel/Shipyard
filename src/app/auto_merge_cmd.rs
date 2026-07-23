use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{
    CliFailure,
    cli::{MergeMethod, MergeResult},
};
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::merge_queue::{
    DEFAULT_ERROR_BUDGET, DEFAULT_SETTLE_WINDOW, PollContext, QueuePollClass, classify_poll,
    parse_pr_observation, parse_queue_snapshot,
};
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
    /// Native GitHub auto-merge was armed for a queue-governed base branch.
    /// The ship state remains active until GitHub's merge queue lands the PR.
    Enqueued,
    /// The live PR head SHA advanced past the validated merge-candidate SHA
    /// (someone pushed new commits to the branch after validation). Refuse
    /// to merge the stale validated SHA; leave the ship state active so the
    /// new head can be re-validated. See issue #321.
    SupersededSha {
        validated: String,
        current: String,
    },
    Merged {
        cleanup_warning: Option<String>,
    },
}

const QUEUE_POLL_INTERVAL: Duration = Duration::from_secs(15);
const QUEUE_WAIT_TIMEOUT: Duration = Duration::from_secs(7_200);

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
            // Preflight (issue #321): before merging, confirm the live PR head
            // still points at the SHA we validated. If new commits landed on
            // the branch after validation, the validated evidence is stale and
            // merging would land unvalidated code. Refuse and leave the state
            // active so the new head can be re-validated.
            //
            // Fail closed: if the live head cannot be verified, do NOT merge
            // blind — report a merge failure instead.
            match fetch_live_head_sha(request.pr, cwd, request.pr_snapshot_file.as_deref()) {
                Some(live_head) => {
                    if !shas_match(&live_head, &state.head_sha) {
                        return Ok(AutoMergeOutcome::SupersededSha {
                            validated: state.head_sha.clone(),
                            current: live_head,
                        });
                    }
                }
                None => {
                    return Ok(AutoMergeOutcome::MergeFailed {
                        error: "failed to verify live PR head before merge".to_owned(),
                    });
                }
            }

            let merge_disposition = match merge_pr(
                cwd,
                &state,
                request.merge_method,
                request.delete_branch,
                request.admin,
                request.merge_command.as_deref(),
                request.merge_result,
            ) {
                Ok(disposition) => disposition,
                Err(error) => {
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
            };
            if merge_disposition == MergeDisposition::Enqueued {
                return Ok(AutoMergeOutcome::Enqueued);
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
    let outcome = execute_auto_merge(store, cwd, &request)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    render_auto_merge_outcome(outcome, pr, json, stdout)
}

/// Render an `AutoMergeOutcome` as a CLI event and map it to the
/// command's process exit code. Split out of `auto_merge` to keep that
/// function within the line budget.
fn render_auto_merge_outcome<W: Write>(
    outcome: AutoMergeOutcome,
    pr: u64,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match outcome {
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
                fields([("pr", Value::from(pr)), ("evidence", to_value(&evidence)?)]),
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
                    ("failing_targets", to_value(&failing_targets)?),
                    ("evidence", to_value(&evidence)?),
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
        AutoMergeOutcome::Enqueued => {
            render_event(stdout, json, "enqueued", fields([("pr", Value::from(pr))]))?;
            Ok(ExitCode::from(3))
        }
        AutoMergeOutcome::SupersededSha { validated, current } => {
            render_event(
                stdout,
                json,
                "superseded-sha",
                fields([
                    ("pr", Value::from(pr)),
                    ("validated", Value::from(validated)),
                    ("current", Value::from(current)),
                ]),
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

/// Serialize a value into a JSON `Value`, mapping serialization failures
/// to a `CliFailure` so render arms stay compact.
fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, CliFailure> {
    serde_json::to_value(value).map_err(|error| CliFailure::new(1, error.to_string()))
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

/// Fetch the live PR head SHA for the merge preflight (issue #321).
///
/// Returns `Some(full_sha)` when the head can be verified, `None` when it
/// cannot (so the caller can fail closed rather than merge a stale SHA).
///
/// Reuses the same `--pr-snapshot-file` injection seam as `pr_is_merged`,
/// accepting either the GraphQL `gh pr view --json` shape (`headRefOid`)
/// or the REST `gh api repos/:r/pulls/:n` shape (`head.sha`) so tests can
/// inject either. With no snapshot file it fetches the PR over REST.
fn fetch_live_head_sha(pr: u64, cwd: &Path, snapshot_file: Option<&Path>) -> Option<String> {
    let payload = if let Some(path) = snapshot_file {
        std::fs::read_to_string(path).ok()?
    } else {
        let client = gh_client(cwd).ok()?;
        let mut command = gh(&client, cwd).ok()?;
        let output = command
            .args(["api", &format!("repos/{{owner}}/{{repo}}/pulls/{pr}")])
            .current_dir(cwd)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let value = serde_json::from_str::<Value>(&payload).ok()?;
    head_sha_from_value(&value)
}

/// Extract a head SHA from either the GraphQL (`headRefOid`) or the REST
/// (`head.sha`) PR payload shape.
fn head_sha_from_value(value: &Value) -> Option<String> {
    if let Some(sha) = value
        .get("headRefOid")
        .and_then(Value::as_str)
        .filter(|sha| !sha.is_empty())
    {
        return Some(sha.to_owned());
    }
    value
        .get("head")
        .and_then(|head| head.get("sha"))
        .and_then(Value::as_str)
        .filter(|sha| !sha.is_empty())
        .map(str::to_owned)
}

/// Compare two head SHAs for full (not prefix) identity, case-insensitively
/// and tolerant of surrounding whitespace. Both sides must be non-empty, so an
/// empty or unreadable head never silently equals an empty validated SHA — the
/// preflight fails closed instead. Equality is full (never a prefix test), so a
/// short SHA can never satisfy a full one.
fn shas_match(a: &str, b: &str) -> bool {
    let a = a.trim();
    let b = b.trim();
    !a.is_empty() && !b.is_empty() && a.eq_ignore_ascii_case(b)
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

#[allow(clippy::too_many_arguments)]
fn merge_pr(
    cwd: &Path,
    state: &ShipState,
    merge_method: MergeMethod,
    delete_branch: bool,
    admin: bool,
    merge_command: Option<&Path>,
    merge_result: Option<MergeResult>,
) -> Result<MergeDisposition, String> {
    match merge_result {
        Some(MergeResult::Success) => return Ok(MergeDisposition::Merged),
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
    let queue_required = if custom_command {
        false
    } else {
        repository_requires_merge_queue(
            client
                .as_ref()
                .expect("built-in merge should have gh client"),
            cwd,
            &state.repo,
            &state.base_branch,
        )?
    };
    if queue_required {
        match queue_admission(
            client
                .as_ref()
                .expect("built-in merge should have gh client"),
            cwd,
            state,
        )? {
            QueueAdmission::AlreadyMerged => return Ok(MergeDisposition::Merged),
            QueueAdmission::AlreadyEnqueued => return Ok(MergeDisposition::Enqueued),
            QueueAdmission::Arm => {}
        }
    }
    if !custom_command {
        command.args(["pr", "merge", &state.pr.to_string(), "--repo", &state.repo]);
        // Defense in depth (issue #321): tell GitHub the exact head we
        // validated so the SERVER rejects the merge if the head drifted
        // between the preflight and this call. A custom `--merge-command`
        // path can't get this guard — the preflight above is its only
        // protection.
        command.args(["--match-head-commit", &state.head_sha]);
    }
    if queue_required {
        command.arg(MergeMethod::Merge.gh_flag());
        command.arg("--auto");
    } else {
        command.arg(merge_method.gh_flag());
    }
    if delete_branch && !queue_required {
        command.arg("--delete-branch");
    }
    if admin && queue_required {
        return Err(
            "`--admin` cannot be used on a merge-queue-governed branch because it bypasses the queue"
                .to_owned(),
        );
    }
    if admin {
        command.arg("--admin");
    }
    let output = command
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("failed to run merge command: {error}"))?;
    if output.status.success() {
        return Ok(if queue_required {
            MergeDisposition::Enqueued
        } else {
            MergeDisposition::Merged
        });
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let message = if stderr.is_empty() { stdout } else { stderr };

    // GraphQL exhausted but REST still has budget? Or GraphQL rejected an App
    // installation token before the REST merge atom? `gh pr merge` uses
    // GraphQL for the merge state probe; the actual merge atom is REST
    // (PUT /repos/:r/pulls/:n/merge), so fall back to a direct REST call
    // rather than failing the ship. Matches src/pr.rs's pattern for
    // gh pr list / create / view.
    if !custom_command
        && !queue_required
        && (crate::pr::is_graphql_rate_limited(&message)
            || is_graphql_merge_integration_blocked(&message))
    {
        let client = client
            .as_ref()
            .expect("built-in merge should have gh client");
        if crate::pr::is_graphql_rate_limited(&message) {
            crate::pr::report_rate_limit_fallback_with_client(client, "gh pr merge", cwd);
        } else {
            eprintln!(
                "shipyard: GraphQL PR merge is unavailable for this GitHub identity. Falling back to REST."
            );
        }
        merge_pr_rest(
            client,
            state.pr,
            cwd,
            &state.head_sha,
            merge_method,
            delete_branch,
        )?;
        return Ok(MergeDisposition::Merged);
    }
    Err(message)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueueAdmission {
    Arm,
    AlreadyEnqueued,
    AlreadyMerged,
}

fn queue_admission(
    client: &GhClient,
    cwd: &Path,
    state: &ShipState,
) -> Result<QueueAdmission, String> {
    let pages = fetch_queue_poll_pages(client, cwd, state)?;
    let body = pages
        .first()
        .ok_or_else(|| "merge-queue admission returned no pages".to_owned())?;
    let observation = parse_pr_observation(body)
        .map_err(|error| format!("merge-queue admission observation was malformed: {error}"))?;
    if !shas_match(&observation.head_sha, &state.head_sha) {
        return Err(format!(
            "live PR head {} superseded validated SHA {}",
            observation.head_sha, state.head_sha
        ));
    }
    if observation.merged {
        return Ok(QueueAdmission::AlreadyMerged);
    }
    match crate::merge_queue::parse_queue_pages(&pages, state.pr) {
        crate::merge_queue::QueuePollParse::Valid(snapshot) if snapshot.pr_found => {
            return Ok(QueueAdmission::AlreadyEnqueued);
        }
        crate::merge_queue::QueuePollParse::Errored(error) => {
            return Err(format!("merge-queue admission poll failed: {error}"));
        }
        crate::merge_queue::QueuePollParse::Valid(_) => {}
    }

    if removal_blocks_rearm(
        observation.removal_event_present,
        observation.removal_reason.as_deref(),
        observation.removal_at.as_deref(),
        state.created_at,
    ) {
        let reason = observation.removal_reason.as_deref().unwrap_or("UNKNOWN");
        return Err(format!(
            "merge queue already removed PR #{} with terminal reason {reason}; refusing to re-arm unchanged ship-state",
            state.pr
        ));
    }
    Ok(QueueAdmission::Arm)
}

fn removal_blocks_rearm(
    event_present: bool,
    reason: Option<&str>,
    removed_at: Option<&str>,
    ship_created_at: chrono::DateTime<chrono::Utc>,
) -> bool {
    if !event_present {
        return false;
    }
    let (Some(reason), Some(removed_at)) = (reason, removed_at) else {
        return true;
    };
    !reason.eq_ignore_ascii_case("invalid_merge_commit")
        && chrono::DateTime::parse_from_rfc3339(removed_at)
            .is_ok_and(|removed| removed.with_timezone(&chrono::Utc) >= ship_created_at)
}

/// Wait for GitHub's merge queue to land a previously armed PR.
///
/// Only a PR observed in the queue can be considered evicted. Re-enqueue is
/// limited to GitHub's `INVALID_MERGE_COMMIT` reason; failed checks, manual
/// removal, unknown reasons, head drift, and HTTP 403/rate-limit responses are
/// terminal and leave ship-state active for diagnosis.
pub(super) fn supervise_merge_queue(
    store: &ShipStateStore,
    cwd: &Path,
    pr: u64,
) -> AutoMergeOutcome {
    let Some(state) = store.get(pr) else {
        return AutoMergeOutcome::PrNotFound;
    };
    let Ok(client) = gh_client(cwd) else {
        return AutoMergeOutcome::MergeFailed {
            error: "github auth config failed while supervising merge queue".to_owned(),
        };
    };
    let started = Instant::now();
    let mut attempt_started = Instant::now();
    let mut attempt_started_at = chrono::Utc::now();
    let mut seen_in_queue = false;
    let mut consecutive_errors = 0_u32;

    while started.elapsed() < QUEUE_WAIT_TIMEOUT {
        match fetch_queue_poll_pages(&client, cwd, &state) {
            Ok(pages) => {
                let Some(body) = pages.first() else {
                    return AutoMergeOutcome::MergeFailed {
                        error: "merge-queue poll returned no pages".to_owned(),
                    };
                };
                let observation = match parse_pr_observation(body) {
                    Ok(observation) => observation,
                    Err(error) => {
                        return AutoMergeOutcome::MergeFailed {
                            error: format!("merge-queue PR observation was malformed: {error}"),
                        };
                    }
                };
                if !shas_match(&observation.head_sha, &state.head_sha) {
                    return AutoMergeOutcome::SupersededSha {
                        validated: state.head_sha,
                        current: observation.head_sha,
                    };
                }
                if observation.merged {
                    if let Err(error) = store.archive(pr) {
                        return AutoMergeOutcome::MergeFailed {
                            error: format!("PR merged but ship-state archive failed: {error}"),
                        };
                    }
                    return AutoMergeOutcome::Merged {
                        cleanup_warning: None,
                    };
                }

                let parsed = crate::merge_queue::parse_queue_pages(&pages, pr);
                let class = classify_poll(
                    &parsed,
                    &PollContext {
                        attempt_elapsed: attempt_started.elapsed(),
                        settle_window: DEFAULT_SETTLE_WINDOW,
                        seen_in_queue,
                        consecutive_errors,
                        error_budget: DEFAULT_ERROR_BUDGET,
                    },
                );
                match class {
                    QueuePollClass::Enqueued { .. } => {
                        consecutive_errors = 0;
                        if let crate::merge_queue::QueuePollParse::Valid(snapshot) = parsed
                            && snapshot.pr_found
                        {
                            seen_in_queue = true;
                        }
                    }
                    QueuePollClass::Evicted => {
                        consecutive_errors = 0;
                        let reason = observation.removal_reason.as_deref().unwrap_or("UNKNOWN");
                        let removal_is_current = observation
                            .removal_at
                            .as_deref()
                            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                            .is_some_and(|removed| {
                                removed.with_timezone(&chrono::Utc) >= attempt_started_at
                            });
                        if !reason.eq_ignore_ascii_case("invalid_merge_commit")
                            || !removal_is_current
                        {
                            return AutoMergeOutcome::MergeFailed {
                                error: format!(
                                    "merge queue removed PR #{pr} with terminal or stale reason {reason}; refusing to re-enqueue"
                                ),
                            };
                        }
                        if let Err(error) = arm_native_queue(&client, cwd, &state) {
                            return AutoMergeOutcome::MergeFailed { error };
                        }
                        seen_in_queue = false;
                        attempt_started = Instant::now();
                        attempt_started_at = chrono::Utc::now();
                    }
                    QueuePollClass::PrNotFound => {
                        return AutoMergeOutcome::MergeFailed {
                            error: format!(
                                "PR #{pr} was never observed in the merge queue after auto-merge was armed"
                            ),
                        };
                    }
                    QueuePollClass::PollError { reason } => {
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        if terminal_github_error(&reason) {
                            return AutoMergeOutcome::MergeFailed { error: reason };
                        }
                        if consecutive_errors >= DEFAULT_ERROR_BUDGET {
                            return AutoMergeOutcome::MergeFailed {
                                error: format!(
                                    "merge-queue polling exhausted its malformed-response budget: {reason}"
                                ),
                            };
                        }
                    }
                    QueuePollClass::TimedOut => {
                        return AutoMergeOutcome::MergeFailed {
                            error: "merge-queue polling exhausted its error budget".to_owned(),
                        };
                    }
                }
            }
            Err(error) => {
                if terminal_github_error(&error) {
                    return AutoMergeOutcome::MergeFailed { error };
                }
                consecutive_errors = consecutive_errors.saturating_add(1);
                if consecutive_errors >= DEFAULT_ERROR_BUDGET {
                    return AutoMergeOutcome::MergeFailed {
                        error: format!("merge-queue polling exhausted its error budget: {error}"),
                    };
                }
            }
        }
        thread::sleep(QUEUE_POLL_INTERVAL);
    }
    AutoMergeOutcome::MergeFailed {
        error: format!(
            "timed out after {}s waiting for GitHub's merge queue",
            QUEUE_WAIT_TIMEOUT.as_secs()
        ),
    }
}

fn fetch_queue_poll_pages(
    client: &GhClient,
    cwd: &Path,
    state: &ShipState,
) -> Result<Vec<Value>, String> {
    let (owner, name) = state
        .repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository slug {:?}", state.repo))?;
    let query = r#"query($owner:String!,$name:String!,$branch:String!,$pr:Int!,$after:String){repository(owner:$owner,name:$name){pullRequest(number:$pr){headRefOid merged timelineItems(last:1,itemTypes:[REMOVED_FROM_MERGE_QUEUE_EVENT]){nodes{... on RemovedFromMergeQueueEvent{reason createdAt}}}} mergeQueue(branch:$branch){entries(first:100,after:$after){nodes{position pullRequest{number}} pageInfo{hasNextPage endCursor}}}}}"#;
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut command = gh(client, cwd)?;
        command.args([
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-F",
            &format!("owner={owner}"),
            "-F",
            &format!("name={name}"),
            "-F",
            &format!("branch={}", state.base_branch),
            "-F",
            &format!("pr={}", state.pr),
        ]);
        if let Some(after) = cursor.as_deref() {
            command.args(["-F", &format!("after={after}")]);
        }
        let output = command
            .output()
            .map_err(|error| format!("failed to poll merge queue: {error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        let page: Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("merge-queue poll returned invalid JSON: {error}"))?;
        let parsed = parse_queue_snapshot(&page, state.pr);
        pages.push(page);
        if matches!(parsed, crate::merge_queue::QueuePollParse::Errored(_)) {
            return Ok(pages);
        }
        let info = pages
            .last()
            .and_then(|page| page.pointer("/data/repository/mergeQueue/entries/pageInfo"))
            .ok_or_else(|| "merge-queue page missing pageInfo".to_owned())?;
        let has_next = info
            .get("hasNextPage")
            .and_then(Value::as_bool)
            .ok_or_else(|| "merge-queue page missing pageInfo.hasNextPage".to_owned())?;
        if !has_next {
            return Ok(pages);
        }
        cursor = Some(
            info.get("endCursor")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "merge-queue page hasNextPage without a usable endCursor".to_owned()
                })?
                .to_owned(),
        );
    }
}

fn arm_native_queue(client: &GhClient, cwd: &Path, state: &ShipState) -> Result<(), String> {
    let mut command = gh(client, cwd)?;
    let output = command
        .args([
            "pr",
            "merge",
            &state.pr.to_string(),
            "--repo",
            &state.repo,
            "--match-head-commit",
            &state.head_sha,
            "--merge",
            "--auto",
        ])
        .output()
        .map_err(|error| format!("failed to re-enqueue merge-queue PR: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(format!("failed to re-enqueue merge-queue PR: {message}"))
}

fn terminal_github_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("http 403")
        || lower.contains("api rate limit exceeded")
        || lower.contains("rate limit")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MergeDisposition {
    Merged,
    Enqueued,
}

fn repository_requires_merge_queue(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    base_branch: &str,
) -> Result<bool, String> {
    let mut command = gh(client, cwd)?;
    let branch = encode_path_segment(base_branch);
    let endpoint = format!("repos/{repo}/rules/branches/{branch}?per_page=100");
    let output = command
        .args(["api", "--paginate", "--slurp", &endpoint])
        .output()
        .map_err(|error| format!("failed to inspect evaluated branch rules: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!(
            "failed to inspect evaluated branch rules for {repo}:{base_branch}: {stderr}"
        ));
    }
    let body: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("evaluated branch rules returned invalid JSON: {error}"))?;
    let pages = body
        .as_array()
        .ok_or_else(|| "paginated evaluated branch rules response is not an array".to_owned())?;
    let mut rules = Vec::new();
    for page in pages {
        let page = page
            .as_array()
            .ok_or_else(|| "evaluated branch rules page is not an array".to_owned())?;
        rules.extend(page.iter().cloned());
    }
    crate::merge_queue::rules_require_merge_queue(&Value::Array(rules))
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn is_graphql_merge_integration_blocked(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("graphql")
        && lower.contains("resource not accessible by integration")
        && lower.contains("mergepullrequest")
}

/// REST fallback for `gh pr merge` when GraphQL is rate-limited or App-token
/// GraphQL merge probing is unavailable.
///
/// `gh pr merge` queries the PR's mergeable state via GraphQL before issuing
/// the actual merge POST. When GraphQL is at 0/5000 the call fails, but
/// REST is independent (`PUT /repos/:repo/pulls/:n/merge`) and usually has
/// budget left. This function bypasses the GraphQL probe and calls REST
/// directly through `gh api`, then optionally deletes the head branch the
/// same way `gh pr merge --delete-branch` would.
///
/// Race protection (issue #266 + #321): the validated head SHA
/// (`expected_head_sha`, the SHA Shipyard actually validated) is passed
/// to the merge PUT as `sha=<oid>`, so GitHub rejects the merge
/// server-side if the live head no longer matches what we validated.
/// The auto-merge preflight (issue #321) already refused the merge if it
/// detected drift, but this is defense in depth for the window between
/// the preflight and the PUT. On a "Base branch was modified" 405, we
/// re-fetch the head once and retry exactly once if and only if the live
/// head SHA still equals the validated SHA (i.e., the modification was
/// purely on the base branch — typical when a sibling PR lands during
/// our merge attempt).
fn merge_pr_rest(
    client: &GhClient,
    pr: u64,
    cwd: &Path,
    expected_head_sha: &str,
    merge_method: MergeMethod,
    delete_branch: bool,
) -> Result<(), String> {
    let repo = repo_slug_for_rest(cwd)?;
    let info = pr_head_info_rest(client, &repo, pr, cwd)?;
    let endpoint = format!("repos/{repo}/pulls/{pr}/merge");

    let first = attempt_merge_put(client, &endpoint, expected_head_sha, merge_method, cwd);
    match first {
        Ok(()) => {}
        Err(error) if is_base_modified_405(&error) => {
            // Re-fetch head; only retry if the live head still equals the
            // validated SHA (i.e., a new commit did NOT land on the head
            // branch). Codex review on PR construction: head_sha invariance
            // is the load-bearing check; `mergeable` can be stale.
            let refreshed = pr_head_info_rest(client, &repo, pr, cwd)?;
            if !shas_match(&refreshed.sha, expected_head_sha) {
                return Err(format!(
                    "REST fallback: PR head moved from validated {} to {} between merge attempts; refusing to retry",
                    short_sha(expected_head_sha),
                    short_sha(&refreshed.sha)
                ));
            }
            attempt_merge_put(client, &endpoint, expected_head_sha, merge_method, cwd)
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
        "enqueued" => writeln!(
            stdout,
            "PR #{pr}: validated green and enqueued in GitHub's merge queue."
        ),
        "superseded-sha" => {
            let validated = data.get("validated").and_then(Value::as_str).unwrap_or("");
            let current = data.get("current").and_then(Value::as_str).unwrap_or("");
            writeln!(
                stdout,
                "PR #{pr}: refusing to merge - validated {} but live head is {}. Re-run shipyard ship to validate the new head.",
                short_sha(validated),
                short_sha(current)
            )
        }
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

    #[test]
    fn detects_graphql_merge_app_integration_block() {
        assert!(is_graphql_merge_integration_blocked(
            "GraphQL: Resource not accessible by integration (mergePullRequest)"
        ));
        assert!(!is_graphql_merge_integration_blocked(
            "GraphQL: Resource not accessible by integration (createPullRequest)"
        ));
        assert!(!is_graphql_merge_integration_blocked(
            "REST: Resource not accessible by integration (mergePullRequest)"
        ));
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

    // ── superseded-SHA preflight helpers (#321) ─────────────────────────

    #[test]
    fn head_sha_from_value_reads_graphql_head_ref_oid() {
        let v = serde_json::json!({ "headRefOid": "a".repeat(40) });
        assert_eq!(head_sha_from_value(&v), Some("a".repeat(40)));
    }

    #[test]
    fn head_sha_from_value_reads_rest_head_sha() {
        // The production snapshot-less path hits `gh api .../pulls/:n`, which
        // returns the REST `{ "head": { "sha": ... } }` shape — not headRefOid.
        let v = serde_json::json!({ "head": { "sha": "b".repeat(40) } });
        assert_eq!(head_sha_from_value(&v), Some("b".repeat(40)));
    }

    #[test]
    fn head_sha_from_value_prefers_head_ref_oid_when_both_present() {
        let v = serde_json::json!({
            "headRefOid": "a".repeat(40),
            "head": { "sha": "b".repeat(40) },
        });
        assert_eq!(head_sha_from_value(&v), Some("a".repeat(40)));
    }

    #[test]
    fn head_sha_from_value_returns_none_for_empty_or_missing() {
        assert_eq!(head_sha_from_value(&serde_json::json!({})), None);
        assert_eq!(
            head_sha_from_value(&serde_json::json!({ "headRefOid": "" })),
            None
        );
        assert_eq!(
            head_sha_from_value(&serde_json::json!({ "head": { "sha": "" } })),
            None
        );
    }

    #[test]
    fn shas_match_full_identity_case_insensitive() {
        let sha = "deadbeefcafef00d1234567890abcdef12345678";
        assert!(shas_match(sha, sha));
        assert!(shas_match(sha, &sha.to_uppercase()));
    }

    #[test]
    fn shas_match_tolerates_surrounding_whitespace() {
        // A SHA captured from `git rev-parse` carries a trailing newline; the
        // preflight must not read that as a superseded head and block a valid
        // merge.
        let sha = "deadbeefcafef00d1234567890abcdef12345678";
        assert!(shas_match(sha, &format!("{sha}\n")));
        assert!(shas_match(&format!("  {sha}  "), sha));
    }

    #[test]
    fn shas_match_rejects_mismatch_short_and_empty() {
        let full = "deadbeefcafef00d1234567890abcdef12345678";
        assert!(!shas_match(
            full,
            "deadbeef0000000000000000000000000000beef"
        ));
        // Full equality, never a prefix test — a short SHA never satisfies a full one.
        assert!(!shas_match(full, "deadbee"));
        // Empty never matches: an unreadable head fails closed, not silently equal.
        assert!(!shas_match("", ""));
        assert!(!shas_match(full, ""));
        assert!(!shas_match("   ", full));
    }

    #[test]
    fn terminal_github_errors_include_403_and_rate_limits_only() {
        assert!(terminal_github_error("HTTP 403: forbidden"));
        assert!(terminal_github_error("API rate limit exceeded"));
        assert!(!terminal_github_error(
            "HTTP 502: transient upstream failure"
        ));
    }

    #[test]
    fn terminal_removal_blocks_same_ship_but_not_new_validation() {
        let ship_created = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:00:00Z")
            .expect("time")
            .with_timezone(&chrono::Utc);
        assert!(removal_blocks_rearm(
            true,
            Some("failed_checks"),
            Some("2026-07-23T12:01:00Z"),
            ship_created,
        ));
        assert!(!removal_blocks_rearm(
            true,
            Some("invalid_merge_commit"),
            Some("2026-07-23T12:01:00Z"),
            ship_created,
        ));
        assert!(!removal_blocks_rearm(
            true,
            Some("failed_checks"),
            Some("2026-07-23T11:59:00Z"),
            ship_created,
        ));
        assert!(removal_blocks_rearm(
            true,
            None,
            Some("2026-07-23T12:01:00Z"),
            ship_created,
        ));
        assert!(!removal_blocks_rearm(false, None, None, ship_created));
    }

    #[test]
    fn branch_rule_path_segments_are_percent_encoded() {
        assert_eq!(encode_path_segment("main"), "main");
        assert_eq!(encode_path_segment("release/1.2"), "release%2F1.2");
        assert_eq!(encode_path_segment("topic name"), "topic%20name");
    }
}
