use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
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
    /// Native GitHub queue admission is pending or active for a governed base.
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
const QUEUE_WAIT_TIMEOUT: Duration = Duration::from_hours(2);

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
    let Some(mut state) = store.get_locked(request.pr, &lock) else {
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
            if let Err(outcome) = validate_live_pr_before_merge(cwd, request, &state) {
                return Ok(outcome);
            }

            let merge_disposition = match merge_pr(
                cwd,
                &mut state,
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
            let cleanup_warning = match merge_disposition {
                MergeDisposition::Enqueued => {
                    if let Err(error) = store.save_locked(&state, &lock) {
                        return Ok(AutoMergeOutcome::MergeFailed {
                            error: format!("failed to persist merge-queue admission: {error}"),
                        });
                    }
                    return Ok(AutoMergeOutcome::Enqueued);
                }
                MergeDisposition::Merged { cleanup_warning } => cleanup_warning,
            };
            store
                .archive_locked(request.pr, &lock)
                .map_err(AutoMergeOperationError::Store)?;
            Ok(AutoMergeOutcome::Merged { cleanup_warning })
        }
    }
}

/// Bind validated evidence to the live PR head and target immediately before
/// selecting merge governance. Snapshot-backed tests have no authority to
/// revoke; production runs revoke any exact-head native merge authority on the
/// PR's current target before returning drift.
fn validate_live_pr_before_merge(
    cwd: &Path,
    request: &AutoMergeRequest,
    state: &ShipState,
) -> Result<(), AutoMergeOutcome> {
    let Some(live_pr) = fetch_live_pr_target(
        request.pr,
        cwd,
        request.pr_snapshot_file.as_deref(),
        &state.base_branch,
    ) else {
        return Err(AutoMergeOutcome::MergeFailed {
            error: "failed to verify live PR head and base before merge".to_owned(),
        });
    };
    let revoke = || {
        if request.pr_snapshot_file.is_some() || !owns_native_merge_authority(state) {
            return Ok(());
        }
        revoke_drifted_native_merge(cwd, &state_with_live_base(state, &live_pr.base_branch))
    };
    if !shas_match(&live_pr.head_sha, &state.head_sha) {
        if let Err(error) = revoke() {
            return Err(AutoMergeOutcome::MergeFailed {
                error: format!(
                    "live PR head {} superseded validated SHA {}, but native merge revocation failed: {error}",
                    live_pr.head_sha, state.head_sha
                ),
            });
        }
        return Err(AutoMergeOutcome::SupersededSha {
            validated: state.head_sha.clone(),
            current: live_pr.head_sha,
        });
    }
    if live_pr.base_branch != state.base_branch {
        if let Err(error) = revoke() {
            return Err(AutoMergeOutcome::MergeFailed {
                error: format!(
                    "PR #{} was retargeted from validated base {} to {}, but native merge revocation failed: {error}",
                    state.pr, state.base_branch, live_pr.base_branch
                ),
            });
        }
        return Err(AutoMergeOutcome::MergeFailed {
            error: format!(
                "PR #{} was retargeted from validated base {} to {}; refusing merge",
                state.pr, state.base_branch, live_pr.base_branch
            ),
        });
    }
    Ok(())
}

fn state_with_live_base(state: &ShipState, live_base: &str) -> ShipState {
    let mut revocation_state = state.clone();
    live_base.clone_into(&mut revocation_state.base_branch);
    revocation_state
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

struct LivePrTarget {
    head_sha: String,
    base_branch: String,
}

/// Fetch the live PR head SHA and base branch for the merge preflight.
///
/// Both values are authoritative: the head protects validated evidence and
/// the base selects the correct merge-governance path.
///
/// Reuses the same `--pr-snapshot-file` injection seam as `pr_is_merged`,
/// accepting either the GraphQL `gh pr view --json` shape (`headRefOid`)
/// or the REST `gh api repos/:r/pulls/:n` shape (`head.sha`, `base.ref`) so
/// tests can inject either. With no snapshot file it fetches the PR over REST.
fn fetch_live_pr_target(
    pr: u64,
    cwd: &Path,
    snapshot_file: Option<&Path>,
    snapshot_base_fallback: &str,
) -> Option<LivePrTarget> {
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
    let base_branch = base_branch_from_value(&value).or_else(|| {
        // Older deterministic fixtures only supplied the head. They do not
        // represent live GitHub authority, so retaining their seeded base is
        // safe while production REST responses remain fail-closed.
        snapshot_file
            .is_some()
            .then(|| snapshot_base_fallback.to_owned())
    })?;
    Some(LivePrTarget {
        head_sha: head_sha_from_value(&value)?,
        base_branch,
    })
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

fn base_branch_from_value(value: &Value) -> Option<String> {
    value
        .get("baseRefName")
        .and_then(Value::as_str)
        .filter(|base| !base.is_empty())
        .or_else(|| {
            value
                .get("base")
                .and_then(|base| base.get("ref"))
                .and_then(Value::as_str)
                .filter(|base| !base.is_empty())
        })
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn merge_pr(
    cwd: &Path,
    state: &mut ShipState,
    merge_method: MergeMethod,
    delete_branch: bool,
    admin: bool,
    merge_command: Option<&Path>,
    merge_result: Option<MergeResult>,
) -> Result<MergeDisposition, String> {
    match merge_result {
        Some(MergeResult::Success) => {
            return Ok(MergeDisposition::Merged {
                cleanup_warning: None,
            });
        }
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
    if let Some(client) = client.as_ref() {
        verify_live_merge_target(client, cwd, state, "before governance selection")?;
    }
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
        if admin {
            return Err(
                "`--admin` cannot be used on a merge-queue-governed branch because it bypasses the queue"
                    .to_owned(),
            );
        }
        let admission_started_at = chrono::Utc::now();
        match queue_admission(
            client
                .as_ref()
                .expect("built-in merge should have gh client"),
            cwd,
            state,
        )? {
            QueueAdmission::AlreadyMerged => {
                let cleanup_warning = if delete_branch {
                    delete_pr_head_branch(
                        client
                            .as_ref()
                            .expect("built-in merge should have gh client"),
                        cwd,
                        state,
                    )
                    .err()
                } else {
                    None
                };
                return Ok(MergeDisposition::Merged { cleanup_warning });
            }
            QueueAdmission::AlreadyQueued => {
                record_observed_queue_adoption(state, admission_started_at);
                return Ok(MergeDisposition::Enqueued);
            }
            QueueAdmission::AutoMergePending => {
                record_pending_auto_merge(state, admission_started_at);
                return Ok(MergeDisposition::Enqueued);
            }
            QueueAdmission::Arm { pr_id } => {
                let arm_succeeded = match arm_native_queue(
                    client
                        .as_ref()
                        .expect("built-in merge should have gh client"),
                    cwd,
                    state,
                    &pr_id,
                ) {
                    Ok(()) => true,
                    Err(error) if terminal_github_error(&error) => return Err(error),
                    Err(error) if enqueue_requirements_pending(&error) => false,
                    Err(error) => return Err(error),
                };
                state.merge_queue_attempt_started_at = Some(admission_started_at);
                state.merge_queue_observed_at = None;
                state.merge_queue_enqueue_succeeded_at = arm_succeeded.then(chrono::Utc::now);
                state.touch();
                return Ok(MergeDisposition::Enqueued);
            }
        }
    }
    if !custom_command {
        verify_live_merge_target(
            client
                .as_ref()
                .expect("built-in merge should have gh client"),
            cwd,
            state,
            "at classic merge mutation boundary",
        )?;
        command.args(["pr", "merge", &state.pr.to_string(), "--repo", &state.repo]);
        // Defense in depth (issue #321): tell GitHub the exact head we
        // validated so the SERVER rejects the merge if the head drifted
        // between the preflight and this call. A custom `--merge-command`
        // path can't get this guard — the preflight above is its only
        // protection.
        command.args(["--match-head-commit", &state.head_sha]);
    }
    command.arg(merge_method.gh_flag());
    if delete_branch && !queue_required {
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
        if let Some(client) = client.as_ref() {
            return classify_builtin_merge_success(client, cwd, state);
        }
        return Ok(MergeDisposition::Merged {
            cleanup_warning: None,
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
            &state.base_branch,
            merge_method,
            delete_branch,
        )?;
        return classify_builtin_merge_success(client, cwd, state);
    }
    Err(message)
}

fn verify_live_merge_target(
    client: &GhClient,
    cwd: &Path,
    state: &ShipState,
    phase: &str,
) -> Result<PrHeadInfo, String> {
    let info = pr_head_info_rest(client, &state.repo, state.pr, cwd)?;
    validate_live_merge_target_info(&info, state, phase)?;
    Ok(info)
}

fn validate_live_merge_target_info(
    info: &PrHeadInfo,
    state: &ShipState,
    phase: &str,
) -> Result<(), String> {
    if !shas_match(&info.sha, &state.head_sha) {
        return Err(format!(
            "{phase}: live PR head {} superseded validated SHA {}",
            info.sha, state.head_sha
        ));
    }
    if info.base_ref != state.base_branch {
        return Err(format!(
            "{phase}: PR #{} was retargeted from validated base {} to {}; refusing merge",
            state.pr, state.base_branch, info.base_ref
        ));
    }
    Ok(())
}

fn classify_builtin_merge_success(
    client: &GhClient,
    cwd: &Path,
    state: &mut ShipState,
) -> Result<MergeDisposition, String> {
    let info = verify_live_merge_target(client, cwd, state, "after merge mutation")?;
    if info.merged {
        return Ok(MergeDisposition::Merged {
            cleanup_warning: None,
        });
    }
    if !repository_requires_merge_queue(client, cwd, &state.repo, &state.base_branch)? {
        return Err(format!(
            "GitHub reported merge success for PR #{} but the PR is not merged and its target branch has no merge queue",
            state.pr
        ));
    }
    let admitted_at = chrono::Utc::now();
    state.merge_queue_attempt_started_at = Some(admitted_at);
    state.merge_queue_observed_at = None;
    state.merge_queue_enqueue_succeeded_at = Some(admitted_at);
    state.touch();
    Ok(MergeDisposition::Enqueued)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum QueueAdmission {
    Arm { pr_id: String },
    AlreadyQueued,
    AutoMergePending,
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
    if observation.base_branch != state.base_branch {
        return Err(format!(
            "PR #{} was retargeted from validated base {} to {} before queue admission",
            state.pr, state.base_branch, observation.base_branch
        ));
    }
    if observation.merged {
        return Ok(QueueAdmission::AlreadyMerged);
    }
    match crate::merge_queue::parse_queue_pages(&pages, state.pr) {
        crate::merge_queue::QueuePollParse::Valid(snapshot) if snapshot.pr_found => {
            return Ok(QueueAdmission::AlreadyQueued);
        }
        crate::merge_queue::QueuePollParse::Errored(error) => {
            return Err(format!("merge-queue admission poll failed: {error}"));
        }
        crate::merge_queue::QueuePollParse::Valid(_) => {}
    }
    if observation.auto_merge_active {
        if auto_merge_has_exact_head_proof(state) {
            return Ok(QueueAdmission::AutoMergePending);
        }
        return Err(format!(
            "PR #{} has pre-existing auto-merge authority that Shipyard cannot prove was bound to validated SHA {}; refusing to adopt it",
            state.pr, state.head_sha
        ));
    }

    if removal_blocks_rearm(
        observation.removal_event_present,
        observation.removal_reason.as_deref(),
        observation.removal_at.as_deref(),
        state,
    ) {
        let reason = observation.removal_reason.as_deref().unwrap_or("UNKNOWN");
        return Err(format!(
            "merge queue already removed PR #{} with terminal reason {reason}; refusing to re-arm unchanged ship-state",
            state.pr
        ));
    }
    if !queue_absence_allows_arm(
        observation.removal_event_present,
        observation.removal_reason.as_deref(),
        observation.removal_at.as_deref(),
        state,
    ) {
        return Err(format!(
            "merge queue no longer contains PR #{} after Shipyard previously admitted it; refusing to re-arm without an observed recoverable eviction",
            state.pr
        ));
    }
    Ok(QueueAdmission::Arm {
        pr_id: observation.id,
    })
}

fn removal_blocks_rearm(
    event_present: bool,
    reason: Option<&str>,
    removed_at: Option<&str>,
    state: &ShipState,
) -> bool {
    if !event_present {
        return false;
    }
    let (Some(reason), Some(removed_at)) = (reason, removed_at) else {
        return true;
    };
    let Ok(removed) = chrono::DateTime::parse_from_rfc3339(removed_at) else {
        return true;
    };
    let removed = removed.with_timezone(&chrono::Utc);
    let attempt_started = state
        .merge_queue_attempt_started_at
        .unwrap_or(state.created_at);
    if removed < attempt_started {
        return false;
    }
    !removal_authorizes_rearm(event_present, Some(reason), Some(removed_at), state)
}

fn removal_authorizes_rearm(
    event_present: bool,
    reason: Option<&str>,
    removed_at: Option<&str>,
    state: &ShipState,
) -> bool {
    if !event_present {
        return false;
    }
    let (Some(reason), Some(removed_at)) = (reason, removed_at) else {
        return false;
    };
    let Ok(removed) = chrono::DateTime::parse_from_rfc3339(removed_at) else {
        return false;
    };
    let removed = removed.with_timezone(&chrono::Utc);
    let attempt_started = state
        .merge_queue_attempt_started_at
        .unwrap_or(state.created_at);
    reason.eq_ignore_ascii_case("invalid_merge_commit")
        && state
            .merge_queue_observed_at
            .is_some_and(|observed| observed >= attempt_started && removed >= observed)
}

fn queue_absence_allows_arm(
    event_present: bool,
    reason: Option<&str>,
    removed_at: Option<&str>,
    state: &ShipState,
) -> bool {
    !owns_native_merge_authority(state)
        || removal_authorizes_rearm(event_present, reason, removed_at, state)
}

fn owns_native_merge_authority(state: &ShipState) -> bool {
    state.merge_queue_enqueue_succeeded_at.is_some() || state.merge_queue_observed_at.is_some()
}

fn auto_merge_has_exact_head_proof(state: &ShipState) -> bool {
    state.merge_queue_enqueue_succeeded_at.is_some()
}

fn record_observed_queue_adoption(
    state: &mut ShipState,
    admission_started_at: chrono::DateTime<chrono::Utc>,
) {
    state
        .merge_queue_attempt_started_at
        .get_or_insert(admission_started_at);
    state
        .merge_queue_observed_at
        .get_or_insert(admission_started_at);
    state.touch();
}

fn record_pending_auto_merge(
    state: &mut ShipState,
    admission_started_at: chrono::DateTime<chrono::Utc>,
) {
    state
        .merge_queue_attempt_started_at
        .get_or_insert(admission_started_at);
    // Preserve exact-head enqueue evidence from an earlier one-shot. A
    // pending auto-merge observation can be the eventual-consistency window
    // after that successful mutation.
    state.touch();
}

/// Wait for GitHub's merge queue to land a previously armed PR.
///
/// Only a PR observed in the queue can be considered evicted. Re-enqueue is
/// limited to GitHub's `INVALID_MERGE_COMMIT` reason; failed checks, manual
/// removal, unknown reasons, head drift, and HTTP 403/rate-limit responses are
/// terminal and leave ship-state active for diagnosis.
#[allow(clippy::too_many_lines)]
pub(super) fn supervise_merge_queue(
    store: &ShipStateStore,
    cwd: &Path,
    pr: u64,
    delete_branch: bool,
) -> AutoMergeOutcome {
    let Some(mut state) = store.get(pr) else {
        return AutoMergeOutcome::PrNotFound;
    };
    let Ok(client) = gh_client(cwd) else {
        return AutoMergeOutcome::MergeFailed {
            error: "github auth config failed while supervising merge queue".to_owned(),
        };
    };
    let started = Instant::now();
    let mut attempt_started = Instant::now();
    let mut attempt_started_at = state
        .merge_queue_attempt_started_at
        .unwrap_or(state.created_at);
    let mut seen_in_queue = state.merge_queue_observed_at.is_some();
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
                let parsed = crate::merge_queue::parse_queue_pages(&pages, pr);
                if matches!(&parsed, crate::merge_queue::QueuePollParse::Valid(_)) {
                    consecutive_errors = 0;
                }
                if !shas_match(&observation.head_sha, &state.head_sha) {
                    let queued = matches!(
                        &parsed,
                        crate::merge_queue::QueuePollParse::Valid(snapshot) if snapshot.pr_found
                    );
                    match with_current_queue_state_locked(
                        store,
                        &state,
                        state.merge_queue_attempt_started_at,
                        || revoke_native_queue(&client, cwd, &observation, queued),
                    ) {
                        Ok(Some(())) => {}
                        Ok(None) => {
                            return AutoMergeOutcome::SupersededSha {
                                validated: state.head_sha,
                                current: observation.head_sha,
                            };
                        }
                        Err(error) => {
                            return AutoMergeOutcome::MergeFailed {
                                error: format!(
                                    "failed to verify ship-state before native merge revocation: {error}"
                                ),
                            };
                        }
                    }
                    return AutoMergeOutcome::SupersededSha {
                        validated: state.head_sha,
                        current: observation.head_sha,
                    };
                }
                if observation.base_branch != state.base_branch {
                    let revocation_state = state_with_live_base(&state, &observation.base_branch);
                    if let Err(error) = with_current_queue_state_locked(
                        store,
                        &state,
                        state.merge_queue_attempt_started_at,
                        || revoke_drifted_native_merge(cwd, &revocation_state),
                    ) {
                        return AutoMergeOutcome::MergeFailed {
                            error: format!(
                                "PR #{pr} was retargeted from validated base {} to {}, but native merge revocation failed: {error}",
                                state.base_branch, observation.base_branch
                            ),
                        };
                    }
                    return AutoMergeOutcome::MergeFailed {
                        error: format!(
                            "PR #{pr} was retargeted from validated base {} to {}; refusing to accept queue outcome",
                            state.base_branch, observation.base_branch
                        ),
                    };
                }
                if observation.merged {
                    let cleanup_warning = if delete_branch {
                        delete_pr_head_branch(&client, cwd, &state).err()
                    } else {
                        None
                    };
                    if let Err(error) = archive_queue_state_if_current(
                        store,
                        &state,
                        state.merge_queue_attempt_started_at,
                    ) {
                        return AutoMergeOutcome::MergeFailed {
                            error: format!("PR merged but ship-state archive failed: {error}"),
                        };
                    }
                    return AutoMergeOutcome::Merged { cleanup_warning };
                }

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
                            if state.merge_queue_observed_at.is_none() {
                                let expected_attempt = state.merge_queue_attempt_started_at;
                                if let Err(error) = update_queue_state_if_current(
                                    store,
                                    &mut state,
                                    expected_attempt,
                                    |current| {
                                        current.merge_queue_observed_at = Some(chrono::Utc::now());
                                    },
                                ) {
                                    return AutoMergeOutcome::MergeFailed {
                                        error: format!(
                                            "failed to persist merge-queue observation: {error}"
                                        ),
                                    };
                                }
                            }
                        }
                    }
                    QueuePollClass::Evicted => {
                        consecutive_errors = 0;
                        let reason = observation.removal_reason.as_deref().unwrap_or("UNKNOWN");
                        let removal_is_current = removal_follows_queue_observation(
                            observation.removal_at.as_deref(),
                            attempt_started_at,
                            state.merge_queue_observed_at,
                        );
                        if !reason.eq_ignore_ascii_case("invalid_merge_commit")
                            || !removal_is_current
                        {
                            return AutoMergeOutcome::MergeFailed {
                                error: format!(
                                    "merge queue removed PR #{pr} with terminal or stale reason {reason}; refusing to re-enqueue"
                                ),
                            };
                        }
                        if let Err(error) = arm_native_queue(&client, cwd, &state, &observation.id)
                        {
                            return AutoMergeOutcome::MergeFailed { error };
                        }
                        let expected_attempt = state.merge_queue_attempt_started_at;
                        seen_in_queue = false;
                        attempt_started = Instant::now();
                        attempt_started_at = chrono::Utc::now();
                        if let Err(error) = update_queue_state_if_current(
                            store,
                            &mut state,
                            expected_attempt,
                            |current| {
                                current.merge_queue_observed_at = None;
                                current.merge_queue_attempt_started_at = Some(attempt_started_at);
                                current.merge_queue_enqueue_succeeded_at = Some(attempt_started_at);
                            },
                        ) {
                            return AutoMergeOutcome::MergeFailed {
                                error: format!(
                                    "failed to persist merge-queue re-enqueue attempt: {error}"
                                ),
                            };
                        }
                    }
                    QueuePollClass::PrNotFound => {
                        if observation.auto_merge_active {
                            if auto_merge_has_exact_head_proof(&state) {
                                thread::sleep(QUEUE_POLL_INTERVAL);
                                continue;
                            }
                            return AutoMergeOutcome::MergeFailed {
                                error: format!(
                                    "PR #{pr} has unowned auto-merge authority without durable exact-head enqueue proof; refusing to supervise it"
                                ),
                            };
                        }
                        if state.merge_queue_enqueue_succeeded_at.is_some() {
                            return AutoMergeOutcome::MergeFailed {
                                error: format!(
                                    "PR #{pr} disappeared after exact-head enqueue succeeded but before Shipyard observed queue membership; refusing to override a possible manual removal"
                                ),
                            };
                        }
                        match arm_native_queue(&client, cwd, &state, &observation.id) {
                            Ok(()) => {
                                let expected_attempt = state.merge_queue_attempt_started_at;
                                attempt_started = Instant::now();
                                attempt_started_at = chrono::Utc::now();
                                if let Err(error) = update_queue_state_if_current(
                                    store,
                                    &mut state,
                                    expected_attempt,
                                    |current| {
                                        current.merge_queue_attempt_started_at =
                                            Some(attempt_started_at);
                                        current.merge_queue_observed_at = None;
                                        current.merge_queue_enqueue_succeeded_at =
                                            Some(attempt_started_at);
                                    },
                                ) {
                                    return AutoMergeOutcome::MergeFailed {
                                        error: format!(
                                            "failed to persist pending queue admission: {error}"
                                        ),
                                    };
                                }
                                thread::sleep(QUEUE_POLL_INTERVAL);
                                continue;
                            }
                            Err(error) if terminal_github_error(&error) => {
                                return AutoMergeOutcome::MergeFailed { error };
                            }
                            Err(error) if enqueue_requirements_pending(&error) => {
                                thread::sleep(QUEUE_POLL_INTERVAL);
                                continue;
                            }
                            Err(error) => {
                                return AutoMergeOutcome::MergeFailed { error };
                            }
                        }
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

fn update_queue_state_if_current(
    store: &ShipStateStore,
    local: &mut ShipState,
    expected_attempt: Option<chrono::DateTime<chrono::Utc>>,
    update: impl FnOnce(&mut ShipState),
) -> Result<(), String> {
    let lock = store
        .lock_pr(local.pr)
        .map_err(|error| format!("failed to lock ship-state: {error}"))?;
    let Some(mut current) = store.get_locked(local.pr, &lock) else {
        return Err("active ship-state disappeared".to_owned());
    };
    if !shas_match(&current.head_sha, &local.head_sha)
        || current.merge_queue_attempt_started_at != expected_attempt
    {
        return Err(format!(
            "ship-state changed concurrently (expected head {} and attempt {:?}, found head {} and attempt {:?}); refusing stale overwrite",
            local.head_sha,
            expected_attempt,
            current.head_sha,
            current.merge_queue_attempt_started_at
        ));
    }
    update(&mut current);
    current.touch();
    store
        .save_locked(&current, &lock)
        .map_err(|error| format!("failed to save ship-state: {error}"))?;
    *local = current;
    Ok(())
}

fn archive_queue_state_if_current(
    store: &ShipStateStore,
    local: &ShipState,
    expected_attempt: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(), String> {
    let lock = store
        .lock_pr(local.pr)
        .map_err(|error| format!("failed to lock ship-state: {error}"))?;
    let Some(current) = store.get_locked(local.pr, &lock) else {
        return Err("active ship-state disappeared".to_owned());
    };
    if !shas_match(&current.head_sha, &local.head_sha)
        || current.merge_queue_attempt_started_at != expected_attempt
    {
        return Err(format!(
            "ship-state changed concurrently (expected head {} and attempt {:?}, found head {} and attempt {:?}); refusing stale archive",
            local.head_sha,
            expected_attempt,
            current.head_sha,
            current.merge_queue_attempt_started_at
        ));
    }
    store
        .archive_locked(local.pr, &lock)
        .map_err(|error| format!("failed to archive ship-state: {error}"))?;
    Ok(())
}

fn with_current_queue_state_locked<T>(
    store: &ShipStateStore,
    local: &ShipState,
    expected_attempt: Option<chrono::DateTime<chrono::Utc>>,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<Option<T>, String> {
    let lock = store
        .lock_pr(local.pr)
        .map_err(|error| format!("failed to lock ship-state: {error}"))?;
    let Some(current) = store.get_locked(local.pr, &lock) else {
        return Ok(None);
    };
    if !shas_match(&current.head_sha, &local.head_sha)
        || current.merge_queue_attempt_started_at != expected_attempt
    {
        return Ok(None);
    }
    action().map(Some)
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
    let query = r"query($owner:String!,$name:String!,$branch:String!,$pr:Int!,$after:String){repository(owner:$owner,name:$name){pullRequest(number:$pr){id headRefOid baseRefName merged autoMergeRequest{id} timelineItems(last:1,itemTypes:[REMOVED_FROM_MERGE_QUEUE_EVENT]){nodes{... on RemovedFromMergeQueueEvent{reason createdAt}}}} mergeQueue(branch:$branch){entries(first:100,after:$after){nodes{position pullRequest{number}} pageInfo{hasNextPage endCursor}}}}}";
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = BTreeSet::new();
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
        let Some(next_cursor) = advance_queue_cursor(info, &mut seen_cursors)? else {
            return Ok(pages);
        };
        cursor = Some(next_cursor);
    }
}

fn advance_queue_cursor(
    page_info: &Value,
    seen: &mut BTreeSet<String>,
) -> Result<Option<String>, String> {
    let has_next = page_info
        .get("hasNextPage")
        .and_then(Value::as_bool)
        .ok_or_else(|| "merge-queue page missing pageInfo.hasNextPage".to_owned())?;
    if !has_next {
        return Ok(None);
    }
    let next = page_info
        .get("endCursor")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "merge-queue page hasNextPage without a usable endCursor".to_owned())?
        .to_owned();
    if !seen.insert(next.clone()) {
        return Err(format!(
            "merge-queue pagination repeated cursor {next}; refusing an unbounded poll"
        ));
    }
    Ok(Some(next))
}

fn removal_follows_queue_observation(
    removed_at: Option<&str>,
    attempt_started_at: chrono::DateTime<chrono::Utc>,
    observed_at: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    removed_at
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .is_some_and(|removed| {
            let removed = removed.with_timezone(&chrono::Utc);
            removed >= attempt_started_at && observed_at.is_some_and(|observed| removed >= observed)
        })
}

fn arm_native_queue(
    client: &GhClient,
    cwd: &Path,
    state: &ShipState,
    pr_id: &str,
) -> Result<(), String> {
    let query = r"mutation($prId:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$prId,expectedHeadOid:$head}){mergeQueueEntry{id}}}";
    let mut command = gh(client, cwd)?;
    let output = command
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-F",
            &format!("prId={pr_id}"),
            "-F",
            &format!("head={}", state.head_sha),
        ])
        .output()
        .map_err(|error| format!("failed to enqueue merge-queue PR: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(format!("failed to enqueue merge-queue PR: {message}"))
}

fn enqueue_requirements_pending(message: &str) -> bool {
    if terminal_github_error(message) {
        return false;
    }
    let lower = message.to_ascii_lowercase();
    lower.contains("required status check")
        || lower.contains("required check")
        || lower.contains("required approving review")
        || lower.contains("required review")
        || lower.contains("requirements are not met")
}

fn revoke_native_queue(
    client: &GhClient,
    cwd: &Path,
    observation: &crate::merge_queue::QueuePrObservation,
    queued: bool,
) -> Result<(), String> {
    // Disable the pending request first so dequeue cannot immediately admit a
    // replacement head again through the still-active auto-merge authority.
    if observation.auto_merge_active {
        let query = r"mutation($prId:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$prId}){pullRequest{id}}}";
        run_queue_mutation(
            client,
            cwd,
            query,
            &observation.id,
            "disable native auto-merge",
        )?;
    }
    if queued {
        let query =
            r"mutation($prId:ID!){dequeuePullRequest(input:{id:$prId}){mergeQueueEntry{id}}}";
        run_queue_mutation(client, cwd, query, &observation.id, "dequeue drifted PR")?;
    }
    Ok(())
}

fn revoke_drifted_native_merge(cwd: &Path, state: &ShipState) -> Result<(), String> {
    let client = gh_client(cwd)?;
    let pages = fetch_queue_poll_pages(&client, cwd, state)?;
    let body = pages
        .first()
        .ok_or_else(|| "drift revocation returned no queue pages".to_owned())?;
    let observation = parse_pr_observation(body)
        .map_err(|error| format!("drift revocation observation was malformed: {error}"))?;
    let queued = matches!(
        crate::merge_queue::parse_queue_pages(&pages, state.pr),
        crate::merge_queue::QueuePollParse::Valid(snapshot) if snapshot.pr_found
    );
    revoke_native_queue(&client, cwd, &observation, queued)
}

fn run_queue_mutation(
    client: &GhClient,
    cwd: &Path,
    query: &str,
    pr_id: &str,
    action: &str,
) -> Result<(), String> {
    let mut command = gh(client, cwd)?;
    let output = command
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-F",
            &format!("prId={pr_id}"),
        ])
        .output()
        .map_err(|error| format!("failed to {action}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(format!("failed to {action}: {message}"))
}

fn terminal_github_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("http 401")
        || lower.contains("http 403")
        || lower.contains("bad credentials")
        || lower.contains("resource not accessible by integration")
        || lower.contains("api rate limit exceeded")
        || lower.contains("rate limit")
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MergeDisposition {
    Merged { cleanup_warning: Option<String> },
    Enqueued,
}

fn repository_requires_merge_queue(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    base_branch: &str,
) -> Result<bool, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository slug {repo:?}"))?;
    let query = r"query($owner:String!,$name:String!,$branch:String!){repository(owner:$owner,name:$name){mergeQueue(branch:$branch){id}}}";
    let mut queue_command = gh(client, cwd)?;
    let queue_output = queue_command
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-F",
            &format!("owner={owner}"),
            "-F",
            &format!("name={name}"),
            "-F",
            &format!("branch={base_branch}"),
        ])
        .output()
        .map_err(|error| format!("failed to inspect branch merge queue: {error}"))?;
    if !queue_output.status.success() {
        let stderr = String::from_utf8_lossy(&queue_output.stderr)
            .trim()
            .to_owned();
        return Err(format!(
            "failed to inspect branch merge queue for {repo}:{base_branch}: {stderr}"
        ));
    }
    let queue_body: Value = serde_json::from_slice(&queue_output.stdout)
        .map_err(|error| format!("branch merge-queue query returned invalid JSON: {error}"))?;
    let queue = queue_body
        .pointer("/data/repository/mergeQueue")
        .ok_or_else(|| "branch merge-queue query omitted repository authority".to_owned())?;
    if !queue.is_null() {
        return Ok(true);
    }

    // Retain evaluated-rules inspection as a fail-closed governance cross-check.
    // The mergeQueue object above is what covers both rulesets and classic
    // branch-protection queues; rules are still useful when GitHub has not yet
    // materialized that object.
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
            let _ = write!(encoded, "%{byte:02X}");
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
    expected_base: &str,
    merge_method: MergeMethod,
    delete_branch: bool,
) -> Result<(), String> {
    let repo = repo_slug_for_rest(cwd)?;
    let info = pr_head_info_rest(client, &repo, pr, cwd)?;
    if info.base_ref != expected_base {
        return Err(format!(
            "REST fallback: PR #{pr} was retargeted from validated base {expected_base} to {}; refusing merge",
            info.base_ref
        ));
    }
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
            if refreshed.base_ref != expected_base {
                return Err(format!(
                    "REST fallback: PR #{pr} was retargeted from validated base {expected_base} to {} between merge attempts; refusing to retry",
                    refreshed.base_ref
                ));
            }
            attempt_merge_put(client, &endpoint, expected_head_sha, merge_method, cwd)
                .map_err(|second| format!("{error} (retry: {second})"))?;
        }
        Err(error) => return Err(error),
    }

    if delete_branch && let Some(head_repo) = info.head_repo.as_deref() {
        let _ = delete_head_branch(client, cwd, head_repo, &info.head_ref, expected_head_sha);
    }
    Ok(())
}

fn delete_pr_head_branch(client: &GhClient, cwd: &Path, state: &ShipState) -> Result<(), String> {
    let info = pr_head_info_rest(client, &state.repo, state.pr, cwd)?;
    let Some(head_repo) = info.head_repo.as_deref() else {
        return Ok(());
    };
    delete_head_branch(client, cwd, head_repo, &info.head_ref, &state.head_sha)
}

fn delete_head_branch(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    head_ref: &str,
    expected_sha: &str,
) -> Result<(), String> {
    let output = client
        .prepare_git_command(cwd)
        .map_err(|error| format!("failed to prepare authenticated git cleanup: {error}"))?
        .args([
            "push",
            &format!("--force-with-lease=refs/heads/{head_ref}:{expected_sha}"),
            &format!("https://github.com/{repo}.git"),
            &format!(":refs/heads/{head_ref}"),
        ])
        .output()
        .map_err(|error| format!("failed to delete merged PR branch {head_ref}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("remote ref does not exist") || lower.contains("couldn't find remote ref") {
        return Ok(());
    }
    Err(format!(
        "PR merged but failed to atomically delete branch {repo}:{head_ref} at validated SHA {}: {stderr}",
        short_sha(expected_sha)
    ))
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
    head_repo: Option<String>,
    sha: String,
    base_ref: String,
    merged: bool,
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
    let head_repo = head
        .get("repo")
        .and_then(|repo| repo.get("full_name"))
        .and_then(Value::as_str)
        .filter(|repo| !repo.is_empty())
        .map(str::to_owned);
    let sha = head
        .get("sha")
        .and_then(Value::as_str)
        .ok_or_else(|| "REST fallback: PR JSON missing head.sha".to_owned())?
        .to_owned();
    let base_ref = value
        .pointer("/base/ref")
        .and_then(Value::as_str)
        .filter(|base| !base.is_empty())
        .ok_or_else(|| "REST fallback: PR JSON missing base.ref".to_owned())?
        .to_owned();
    let merged = value
        .get("merged")
        .and_then(Value::as_bool)
        .ok_or_else(|| "REST fallback: PR JSON missing merged state".to_owned())?;
    Ok(PrHeadInfo {
        head_ref,
        head_repo,
        sha,
        base_ref,
        merged,
    })
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
    fn base_branch_from_value_reads_graphql_and_rest_shapes() {
        assert_eq!(
            base_branch_from_value(&serde_json::json!({ "baseRefName": "release" })),
            Some("release".to_owned())
        );
        assert_eq!(
            base_branch_from_value(&serde_json::json!({ "base": { "ref": "main" } })),
            Some("main".to_owned())
        );
        assert_eq!(base_branch_from_value(&serde_json::json!({})), None);
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
    fn terminal_github_errors_include_auth_and_rate_limits() {
        assert!(terminal_github_error("HTTP 401: bad credentials"));
        assert!(terminal_github_error("HTTP 403: forbidden"));
        assert!(terminal_github_error(
            "GraphQL: Resource not accessible by integration"
        ));
        assert!(terminal_github_error("API rate limit exceeded"));
        assert!(!terminal_github_error(
            "HTTP 502: transient upstream failure"
        ));
    }

    #[test]
    fn enqueue_requirements_pending_never_swallows_terminal_auth_errors() {
        assert!(enqueue_requirements_pending(
            "Pull request is not mergeable because a required status check is pending"
        ));
        assert!(enqueue_requirements_pending(
            "Required approving review has not been submitted"
        ));
        assert!(!enqueue_requirements_pending(
            "HTTP 403: a required permission is missing"
        ));
        assert!(!enqueue_requirements_pending(
            "Resource not accessible by integration: review permission required"
        ));
        assert!(!enqueue_requirements_pending(
            "Pull request is not mergeable because it has conflicts"
        ));
    }

    #[test]
    fn live_merge_target_validation_binds_head_and_base() {
        let state = ShipState::new(3, "owner/repo", "feature/x", "main", "validated", "policy");
        let mut info = PrHeadInfo {
            head_ref: "feature/x".to_owned(),
            head_repo: Some("owner/repo".to_owned()),
            sha: "validated".to_owned(),
            base_ref: "main".to_owned(),
            merged: false,
        };
        assert!(validate_live_merge_target_info(&info, &state, "test").is_ok());
        info.base_ref = "release".to_owned();
        assert!(
            validate_live_merge_target_info(&info, &state, "test")
                .expect_err("retarget must fail")
                .contains("retargeted")
        );
        info.base_ref = "main".to_owned();
        info.sha = "different".to_owned();
        assert!(
            validate_live_merge_target_info(&info, &state, "test")
                .expect_err("head drift must fail")
                .contains("superseded")
        );
    }

    #[test]
    fn terminal_removal_blocks_same_ship_but_not_new_validation() {
        let ship_created = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:00:00Z")
            .expect("time")
            .with_timezone(&chrono::Utc);
        let mut state = ShipState::new(1, "owner/repo", "feature/x", "main", "abc", "policy");
        state.created_at = ship_created;
        state.updated_at = ship_created;
        state.merge_queue_attempt_started_at = Some(ship_created);
        assert!(removal_blocks_rearm(
            true,
            Some("failed_checks"),
            Some("2026-07-23T12:01:00Z"),
            &state,
        ));
        assert!(removal_blocks_rearm(
            true,
            Some("invalid_merge_commit"),
            Some("2026-07-23T12:01:00Z"),
            &state,
        ));
        assert!(!removal_blocks_rearm(
            true,
            Some("failed_checks"),
            Some("2026-07-23T11:59:00Z"),
            &state,
        ));
        assert!(removal_blocks_rearm(
            true,
            None,
            Some("2026-07-23T12:01:00Z"),
            &state,
        ));
        assert!(removal_blocks_rearm(
            true,
            Some("invalid_merge_commit"),
            Some("not-a-timestamp"),
            &state,
        ));
        assert!(removal_blocks_rearm(
            true,
            Some("failed_checks"),
            Some("not-a-timestamp"),
            &state,
        ));
        assert!(!removal_blocks_rearm(false, None, None, &state));

        state.merge_queue_observed_at = Some(
            chrono::DateTime::parse_from_rfc3339("2026-07-23T12:00:30Z")
                .expect("time")
                .with_timezone(&chrono::Utc),
        );
        assert!(!removal_blocks_rearm(
            true,
            Some("invalid_merge_commit"),
            Some("2026-07-23T12:01:00Z"),
            &state,
        ));
    }

    #[test]
    fn queue_state_update_preserves_newer_same_head_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut local = ShipState::new(7, "owner/repo", "feature/x", "main", "abc", "policy");
        let attempt = chrono::Utc::now();
        local.merge_queue_attempt_started_at = Some(attempt);
        store.save(&local).expect("seed state");

        let mut current = local.clone();
        current
            .evidence_snapshot
            .insert("macos".to_owned(), "pass".to_owned());
        store.save(&current).expect("save concurrent evidence");

        update_queue_state_if_current(&store, &mut local, Some(attempt), |state| {
            state.merge_queue_observed_at = Some(attempt);
        })
        .expect("same-head update");

        assert_eq!(
            with_current_queue_state_locked(&store, &local, Some(attempt), || Ok("ran"))
                .expect("current state"),
            Some("ran")
        );
        assert_eq!(
            local.evidence_snapshot.get("macos").map(String::as_str),
            Some("pass")
        );
        assert_eq!(local.merge_queue_observed_at, Some(attempt));
    }

    #[test]
    fn queue_state_update_and_archive_refuse_newer_head() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut local = ShipState::new(8, "owner/repo", "feature/x", "main", "old", "policy");
        let attempt = chrono::Utc::now();
        local.merge_queue_attempt_started_at = Some(attempt);
        store.save(&local).expect("seed state");

        let mut newer = local.clone();
        newer.head_sha = "new".to_owned();
        newer.merge_queue_attempt_started_at = None;
        store.save(&newer).expect("save adopted head");

        assert!(update_queue_state_if_current(&store, &mut local, Some(attempt), |_| {}).is_err());
        assert_eq!(
            with_current_queue_state_locked(&store, &local, Some(attempt), || Ok("must not run"))
                .expect("newer head"),
            None
        );
        assert!(archive_queue_state_if_current(&store, &local, Some(attempt)).is_err());
        assert_eq!(
            store.get(8).expect("newer state remains active").head_sha,
            "new"
        );
    }

    #[test]
    fn queue_removal_must_follow_persisted_observation() {
        let attempt = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:00:00Z")
            .expect("attempt")
            .with_timezone(&chrono::Utc);
        let observed = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:00:30Z")
            .expect("observed")
            .with_timezone(&chrono::Utc);
        assert!(!removal_follows_queue_observation(
            Some("2026-07-23T12:00:20Z"),
            attempt,
            Some(observed),
        ));
        assert!(removal_follows_queue_observation(
            Some("2026-07-23T12:01:00Z"),
            attempt,
            Some(observed),
        ));
        assert!(!removal_follows_queue_observation(
            Some("2026-07-23T12:01:00Z"),
            attempt,
            None,
        ));
    }

    #[test]
    fn native_merge_authority_requires_enqueue_or_observation() {
        let mut state = ShipState::new(9, "owner/repo", "feature/x", "main", "abc", "policy");
        assert!(!owns_native_merge_authority(&state));
        assert!(!auto_merge_has_exact_head_proof(&state));
        state.merge_queue_attempt_started_at = Some(chrono::Utc::now());
        assert!(!owns_native_merge_authority(&state));
        assert!(!auto_merge_has_exact_head_proof(&state));
        state.merge_queue_enqueue_succeeded_at = Some(chrono::Utc::now());
        assert!(owns_native_merge_authority(&state));
        assert!(auto_merge_has_exact_head_proof(&state));
        state.merge_queue_enqueue_succeeded_at = None;
        state.merge_queue_observed_at = Some(chrono::Utc::now());
        assert!(owns_native_merge_authority(&state));
    }

    #[test]
    fn prior_queue_authority_requires_observed_recoverable_eviction_to_rearm() {
        let attempt = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:00:00Z")
            .expect("attempt")
            .with_timezone(&chrono::Utc);
        let observed = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:00:30Z")
            .expect("observed")
            .with_timezone(&chrono::Utc);
        let mut state = ShipState::new(9, "owner/repo", "feature/x", "main", "abc", "policy");
        assert!(queue_absence_allows_arm(false, None, None, &state));

        state.merge_queue_attempt_started_at = Some(attempt);
        state.merge_queue_observed_at = Some(observed);
        assert!(!queue_absence_allows_arm(false, None, None, &state));
        assert!(!queue_absence_allows_arm(
            true,
            Some("MANUAL"),
            Some("2026-07-23T12:01:00Z"),
            &state,
        ));
        assert!(!queue_absence_allows_arm(
            true,
            Some("INVALID_MERGE_COMMIT"),
            Some("2026-07-23T12:00:20Z"),
            &state,
        ));
        assert!(queue_absence_allows_arm(
            true,
            Some("INVALID_MERGE_COMMIT"),
            Some("2026-07-23T12:01:00Z"),
            &state,
        ));
    }

    #[test]
    fn repeated_queue_adoption_preserves_original_authority_times() {
        let first = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:00:00Z")
            .expect("first")
            .with_timezone(&chrono::Utc);
        let later = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:05:00Z")
            .expect("later")
            .with_timezone(&chrono::Utc);
        let mut state = ShipState::new(10, "owner/repo", "feature/x", "main", "abc", "policy");
        record_observed_queue_adoption(&mut state, first);
        record_observed_queue_adoption(&mut state, later);
        assert_eq!(state.merge_queue_attempt_started_at, Some(first));
        assert_eq!(state.merge_queue_observed_at, Some(first));
        assert_eq!(state.merge_queue_enqueue_succeeded_at, None);
    }

    #[test]
    fn pending_auto_merge_preserves_successful_enqueue_evidence() {
        let first = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:00:00Z")
            .expect("first")
            .with_timezone(&chrono::Utc);
        let later = chrono::DateTime::parse_from_rfc3339("2026-07-23T12:05:00Z")
            .expect("later")
            .with_timezone(&chrono::Utc);
        let mut state = ShipState::new(11, "owner/repo", "feature/x", "main", "abc", "policy");
        state.merge_queue_attempt_started_at = Some(first);
        state.merge_queue_enqueue_succeeded_at = Some(first);
        record_pending_auto_merge(&mut state, later);
        assert_eq!(state.merge_queue_attempt_started_at, Some(first));
        assert_eq!(state.merge_queue_enqueue_succeeded_at, Some(first));

        let mut external = ShipState::new(12, "owner/repo", "feature/y", "main", "def", "policy");
        record_pending_auto_merge(&mut external, later);
        assert_eq!(external.merge_queue_attempt_started_at, Some(later));
        assert_eq!(external.merge_queue_enqueue_succeeded_at, None);
    }

    #[test]
    fn queue_cursor_repetition_fails_closed() {
        let mut seen = BTreeSet::new();
        let page = serde_json::json!({
            "hasNextPage": true,
            "endCursor": "cursor-1",
        });
        assert_eq!(
            advance_queue_cursor(&page, &mut seen).expect("first cursor"),
            Some("cursor-1".to_owned())
        );
        assert!(advance_queue_cursor(&page, &mut seen).is_err());
        assert_eq!(
            advance_queue_cursor(
                &serde_json::json!({ "hasNextPage": false, "endCursor": null }),
                &mut seen,
            )
            .expect("last page"),
            None
        );
    }

    #[test]
    fn branch_rule_path_segments_are_percent_encoded() {
        assert_eq!(encode_path_segment("main"), "main");
        assert_eq!(encode_path_segment("release/1.2"), "release%2F1.2");
        assert_eq!(encode_path_segment("topic name"), "topic%20name");
    }
}
