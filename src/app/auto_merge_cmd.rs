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
use crate::config::LoadedConfig;
use crate::gh::{GhAuthPolicy, GhAuthSourceSummary, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::merge_queue::{
    DEFAULT_ERROR_BUDGET, DEFAULT_SETTLE_WINDOW, PollContext, QueuePollClass, classify_poll,
    parse_pr_observation, parse_queue_snapshot,
};
use crate::merge_queue_control::MergeQueueMutationGuard;
use crate::output::write_json_envelope;
use crate::ship_state::{ShipState, ShipStatePrLock, ShipStateStore};
use crate::watch::ship_terminal_verdict;

pub(super) struct AutoMergeRequest {
    pub(super) mode: RuntimeMode,
    pub(super) global_dir: PathBuf,
    pub(super) pr: u64,
    pub(super) merge_method: MergeMethod,
    pub(super) delete_branch: bool,
    pub(super) admin: bool,
    pub(super) pr_snapshot_file: Option<PathBuf>,
    pub(super) merge_command: Option<PathBuf>,
    pub(super) merge_result: Option<MergeResult>,
    /// Exact validation identity whose completed proof authorized this merge
    /// phase. Standalone `auto-merge` calls intentionally leave this absent;
    /// `ship` always supplies its captured terminal state so a later same-PR
    /// submission cannot redirect the old job's merge authority.
    pub(super) expected_validation: Option<ValidatedShipIdentity>,
}

/// Immutable part of a ship-state that binds validation proof to merge
/// authority. Queue timestamps and evidence may advance during stewardship;
/// these fields may not change without a new validation request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ValidatedShipIdentity {
    repo: String,
    pr: u64,
    branch: String,
    base_branch: String,
    head_sha: String,
    policy_signature: String,
}

impl From<&ShipState> for ValidatedShipIdentity {
    fn from(state: &ShipState) -> Self {
        Self {
            repo: state.repo.clone(),
            pr: state.pr,
            branch: state.branch.clone(),
            base_branch: state.base_branch.clone(),
            head_sha: state.head_sha.clone(),
            policy_signature: state.policy_signature.clone(),
        }
    }
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
    /// A newer same-PR submission replaced a completed validation identity
    /// without necessarily changing the head SHA. No merge mutation ran.
    ValidationIdentityMismatch {
        detail: String,
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
    let repository = super::branch_cmd::detect_repo_from_remote(cwd, None);
    let discovered = repository.as_ref().map_or_else(
        || store.get(request.pr),
        |repository| store.get_scoped(repository, request.pr),
    );
    let Some(discovered) = discovered else {
        return Ok(
            if pr_is_merged(request.pr, cwd, request.pr_snapshot_file.as_deref()) {
                AutoMergeOutcome::AlreadyMerged
            } else {
                AutoMergeOutcome::PrNotFound
            },
        );
    };
    let lock = store
        .lock_pr_scoped(&discovered.repo, request.pr)
        .map_err(AutoMergeOperationError::Store)?;
    let Some(mut state) = store.get_locked_scoped(&discovered.repo, request.pr, &lock) else {
        return Ok(AutoMergeOutcome::PrNotFound);
    };
    if let Some(outcome) = request
        .expected_validation
        .as_ref()
        .and_then(|expected| validation_identity_mismatch(expected, &state))
    {
        return Ok(outcome);
    }

    match ship_terminal_verdict(&state) {
        None => Ok(AutoMergeOutcome::InFlight {
            evidence: state.evidence_snapshot,
        }),
        Some(false) => Ok(AutoMergeOutcome::TargetFailed {
            failing_targets: failing_required_targets(&state),
            evidence: state.evidence_snapshot,
        }),
        Some(true) => {
            if let Err(outcome) = validate_live_pr_before_merge(store, cwd, request, &state) {
                return Ok(outcome);
            }

            let merge_disposition = match merge_pr(
                store,
                &lock,
                cwd,
                &mut state,
                request.mode,
                &request.global_dir,
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
                            .archive_scoped_locked(&state.repo, request.pr, &lock)
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
                    if let Err(error) = store.save_scoped_locked(&state, &lock) {
                        return Ok(AutoMergeOutcome::MergeFailed {
                            error: format!("failed to persist merge-queue admission: {error}"),
                        });
                    }
                    return Ok(AutoMergeOutcome::Enqueued);
                }
                MergeDisposition::Merged { cleanup_warning } => cleanup_warning,
            };
            store
                .archive_scoped_locked(&state.repo, request.pr, &lock)
                .map_err(AutoMergeOperationError::Store)?;
            Ok(AutoMergeOutcome::Merged { cleanup_warning })
        }
    }
}

fn validation_identity_mismatch(
    expected: &ValidatedShipIdentity,
    current: &ShipState,
) -> Option<AutoMergeOutcome> {
    let mut changed = Vec::new();
    if !expected.repo.eq_ignore_ascii_case(&current.repo) {
        changed.push("repository");
    }
    if expected.pr != current.pr {
        changed.push("pull-request number");
    }
    if expected.branch != current.branch {
        changed.push("head branch");
    }
    if expected.base_branch != current.base_branch {
        changed.push("base branch");
    }
    if !shas_match(&expected.head_sha, &current.head_sha) {
        changed.push("head SHA");
    }
    if expected.policy_signature != current.policy_signature {
        changed.push("validation policy");
    }
    (!changed.is_empty()).then(|| AutoMergeOutcome::ValidationIdentityMismatch {
        detail: format!(
            "completed validation identity for PR #{} was replaced before merge ({changed}); refusing stale merge authority",
            expected.pr,
            changed = changed.join(", ")
        ),
    })
}

fn same_validation_identity(left: &ShipState, right: &ShipState) -> bool {
    validation_identity_mismatch(&ValidatedShipIdentity::from(left), right).is_none()
}

/// Bind validated evidence to the live PR head and target immediately before
/// selecting merge governance. Snapshot-backed tests have no authority to
/// revoke; production runs revoke any exact-head native merge authority on the
/// PR's current target before returning drift.
fn validate_live_pr_before_merge(
    store: &ShipStateStore,
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
        revoke_drifted_native_merge(
            store,
            cwd,
            request.mode,
            &request.global_dir,
            &state_with_live_base(state, &live_pr.base_branch),
        )
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
    mode: RuntimeMode,
    global_dir: &Path,
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
        mode,
        global_dir: global_dir.to_path_buf(),
        pr,
        merge_method,
        delete_branch,
        admin,
        pr_snapshot_file,
        merge_command,
        merge_result,
        expected_validation: None,
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
        AutoMergeOutcome::ValidationIdentityMismatch { detail } => {
            render_event(
                stdout,
                json,
                "validation-identity-mismatch",
                fields([("pr", Value::from(pr)), ("detail", Value::from(detail))]),
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
    store: &ShipStateStore,
    lock: &ShipStatePrLock,
    cwd: &Path,
    state: &mut ShipState,
    mode: RuntimeMode,
    global_dir: &Path,
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
    let mut client = if custom_command {
        None
    } else {
        Some(gh_client(cwd)?)
    };
    let mut isolated_branch_cleanup = false;
    if let Some(client) = client.as_mut()
        && delete_branch
    {
        isolated_branch_cleanup = require_branch_cleanup_git(client, cwd, global_dir)?;
    }
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
                        global_dir,
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
                ensure_unstacked(
                    client
                        .as_ref()
                        .expect("built-in merge should have gh client"),
                    cwd,
                    state,
                    global_dir,
                )?;
                let guard = MergeQueueMutationGuard::acquire_in_mode(
                    store,
                    cwd,
                    mode,
                    global_dir,
                    state,
                    "enqueue pull request",
                )?;
                state.merge_queue_attempt_started_at = Some(admission_started_at);
                state.merge_queue_observed_at = None;
                state.merge_queue_enqueue_succeeded_at = None;
                state.merge_queue_enqueue_started_at = Some(admission_started_at);
                state.touch();
                if let Err(error) = store.save_scoped_locked(state, lock) {
                    guard.finish("rejected").map_err(|audit_error| {
                        format!(
                            "failed to persist uncertain queue admission: {error}; additionally failed to close pre-network mutation audit: {audit_error}"
                        )
                    })?;
                    return Err(format!(
                        "failed to persist uncertain queue admission: {error}"
                    ));
                }
                let arm_result = arm_native_queue(
                    client
                        .as_ref()
                        .expect("built-in merge should have gh client"),
                    cwd,
                    state,
                    &pr_id,
                    guard,
                );
                let success_guard = match arm_result {
                    Ok(guard) => {
                        state.merge_queue_enqueue_started_at = None;
                        state.merge_queue_enqueue_succeeded_at = Some(chrono::Utc::now());
                        guard
                    }
                    Err(QueueArmError::Rejected { error, guard }) => {
                        state.merge_queue_enqueue_started_at = None;
                        state.merge_queue_enqueue_succeeded_at = None;
                        state.touch();
                        store
                            .save_scoped_locked(state, lock)
                            .map_err(|persist_error| {
                                format!(
                                    "failed to persist rejected queue admission: {persist_error}"
                                )
                            })?;
                        (*guard).finish("rejected")?;
                        if terminal_github_error(&error) {
                            return Err(error);
                        }
                        return Ok(MergeDisposition::Enqueued);
                    }
                    Err(QueueArmError::Uncertain(error)) => {
                        return Err(format!(
                            "{error}; enqueue outcome is uncertain, so Shipyard retained its durable pre-mutation marker"
                        ));
                    }
                };
                state.touch();
                store.save_scoped_locked(state, lock).map_err(|error| {
                    format!("failed to persist successful queue admission: {error}")
                })?;
                success_guard.finish("success")?;
                return Ok(MergeDisposition::Enqueued);
            }
        }
    }
    if custom_command {
        command.arg(merge_method.gh_flag());
        if delete_branch {
            command.arg("--delete-branch");
        }
        if admin {
            command.arg("--admin");
        }
    } else {
        verify_live_merge_target(
            client
                .as_ref()
                .expect("built-in merge should have gh client"),
            cwd,
            state,
            "at classic merge mutation boundary",
        )?;
        ensure_unstacked_for_classic_merge(
            client
                .as_ref()
                .expect("built-in merge should have gh client"),
            cwd,
            state,
            global_dir,
        )?;
        command.args(classic_merge_args(
            state,
            merge_method,
            delete_branch && !isolated_branch_cleanup,
            admin,
        ));
    }
    let output = command
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("failed to run merge command: {error}"))?;
    if output.status.success() {
        if let Some(client) = client.as_ref() {
            let disposition = classify_builtin_merge_success(client, cwd, state)?;
            return Ok(cleanup_confirmed_merge(
                disposition,
                isolated_branch_cleanup,
                || delete_pr_head_branch(client, cwd, global_dir, state),
            ));
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
        // Do not repeat stack discovery here: this path exists specifically
        // because GraphQL became unavailable after the earlier mutation-boundary
        // check. The REST merge still carries the exact validated head SHA, and
        // another GraphQL dependency would make the fallback unreachable.
        merge_pr_rest(
            client,
            state.pr,
            cwd,
            &state.head_sha,
            &state.base_branch,
            merge_method,
        )?;
        let disposition = classify_builtin_merge_success(client, cwd, state)?;
        return Ok(cleanup_confirmed_merge(disposition, delete_branch, || {
            delete_pr_head_branch(client, cwd, global_dir, state)
        }));
    }
    Err(message)
}

fn ensure_unstacked(
    client: &GhClient,
    cwd: &Path,
    state: &ShipState,
    global_dir: &Path,
) -> Result<(), String> {
    inspect_unstacked(client, cwd, state, global_dir)
        .map_err(crate::stacked_pr::StackInspectionError::into_message)
}

fn inspect_unstacked(
    client: &GhClient,
    cwd: &Path,
    state: &ShipState,
    global_dir: &Path,
) -> Result<(), crate::stacked_pr::StackInspectionError> {
    let inspection = crate::stacked_pr::fetch_inspection(
        client,
        cwd,
        &state.repo,
        &state.base_branch,
        state.pr,
        global_dir,
    )?;
    crate::stacked_pr::ensure_unstacked(&state.repo, state.pr, &state.head_sha, &inspection)
        .map_err(crate::stacked_pr::StackInspectionError::validation)
}

fn ensure_unstacked_for_classic_merge(
    client: &GhClient,
    cwd: &Path,
    state: &ShipState,
    global_dir: &Path,
) -> Result<(), String> {
    allow_classic_rest_fallback(inspect_unstacked(client, cwd, state, global_dir))
}

fn allow_classic_rest_fallback(
    result: Result<(), crate::stacked_pr::StackInspectionError>,
) -> Result<(), String> {
    match result {
        Err(error) if error.is_graphql_rate_limited() => {
            // The classic REST merge endpoint cannot merge a formal stack; the
            // asynchronous endpoint is required for that. Preserve Shipyard's
            // independent REST quota fallback when this last read exhausts
            // GraphQL, while retaining the exact validated-head REST guard.
            eprintln!(
                "shipyard: stack inspection exhausted GraphQL at the classic merge boundary; continuing to the server-enforced exact-head merge path"
            );
            Ok(())
        }
        Ok(()) => Ok(()),
        Err(error) => Err(error.into_message()),
    }
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
    state.merge_queue_enqueue_started_at = None;
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
    if state.merge_queue_enqueue_started_at.is_some() {
        return false;
    }
    !owns_native_merge_authority(state)
        || removal_authorizes_rearm(event_present, reason, removed_at, state)
}

fn owns_native_merge_authority(state: &ShipState) -> bool {
    state.merge_queue_enqueue_started_at.is_some()
        || state.merge_queue_enqueue_succeeded_at.is_some()
        || state.merge_queue_observed_at.is_some()
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
    state.merge_queue_enqueue_started_at = None;
    state.touch();
}

fn record_pending_auto_merge(
    state: &mut ShipState,
    admission_started_at: chrono::DateTime<chrono::Utc>,
) {
    state
        .merge_queue_attempt_started_at
        .get_or_insert(admission_started_at);
    state.merge_queue_enqueue_started_at = None;
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
    mode: RuntimeMode,
    global_dir: &Path,
    pr: u64,
    delete_branch: bool,
    expected_validation: Option<&ValidatedShipIdentity>,
) -> AutoMergeOutcome {
    let repository = super::branch_cmd::detect_repo_from_remote(cwd, None);
    let Some(mut state) = repository.as_ref().map_or_else(
        || store.get(pr),
        |repository| store.get_scoped(repository, pr),
    ) else {
        return AutoMergeOutcome::PrNotFound;
    };
    if let Some(outcome) =
        expected_validation.and_then(|expected| validation_identity_mismatch(expected, &state))
    {
        return outcome;
    }
    let Ok(mut client) = gh_client(cwd) else {
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
        if let Some(expected) = expected_validation {
            let current = repository.as_ref().map_or_else(
                || store.get(pr),
                |repository| store.get_scoped(repository, pr),
            );
            let Some(current) = current else {
                return AutoMergeOutcome::PrNotFound;
            };
            if let Some(outcome) = validation_identity_mismatch(expected, &current) {
                return outcome;
            }
        }
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
                        || {
                            revoke_native_queue(
                                store,
                                &client,
                                cwd,
                                mode,
                                global_dir,
                                &state,
                                &observation,
                                queued,
                            )
                        },
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
                        || {
                            revoke_drifted_native_merge(
                                store,
                                cwd,
                                mode,
                                global_dir,
                                &revocation_state,
                            )
                        },
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
                        require_branch_cleanup_git(&mut client, cwd, global_dir)
                            .err()
                            .or_else(|| {
                                delete_pr_head_branch(&client, cwd, global_dir, &state).err()
                            })
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
                        if let Err(error) = ensure_unstacked(&client, cwd, &state, global_dir) {
                            return AutoMergeOutcome::MergeFailed { error };
                        }
                        let guard = match MergeQueueMutationGuard::acquire_in_mode(
                            store,
                            cwd,
                            mode,
                            global_dir,
                            &state,
                            "enqueue pull request",
                        ) {
                            Ok(guard) => guard,
                            Err(error) => {
                                return AutoMergeOutcome::MergeFailed { error };
                            }
                        };
                        let expected_attempt = match mark_queue_enqueue_started(store, &mut state) {
                            Ok(expected) => expected,
                            Err(error) => {
                                if let Err(audit_error) = guard.finish("rejected") {
                                    return AutoMergeOutcome::MergeFailed {
                                        error: format!(
                                            "{error}; additionally failed to close pre-network mutation audit: {audit_error}"
                                        ),
                                    };
                                }
                                return AutoMergeOutcome::MergeFailed { error };
                            }
                        };
                        let success_guard = match arm_native_queue(
                            &client,
                            cwd,
                            &state,
                            &observation.id,
                            guard,
                        ) {
                            Ok(guard) => guard,
                            Err(QueueArmError::Rejected { error, guard }) => {
                                if let Err(persist_error) =
                                    finish_queue_enqueue(store, &mut state, expected_attempt, false)
                                {
                                    return AutoMergeOutcome::MergeFailed {
                                        error: format!(
                                            "{error}; additionally failed to persist definite rejection: {persist_error}"
                                        ),
                                    };
                                }
                                if let Err(audit_error) = (*guard).finish("rejected") {
                                    return AutoMergeOutcome::MergeFailed {
                                        error: format!(
                                            "{error}; additionally failed to close rejected mutation audit: {audit_error}"
                                        ),
                                    };
                                }
                                return AutoMergeOutcome::MergeFailed { error };
                            }
                            Err(QueueArmError::Uncertain(error)) => {
                                return AutoMergeOutcome::MergeFailed {
                                    error: format!(
                                        "{error}; enqueue outcome is uncertain, so Shipyard retained its durable pre-mutation marker"
                                    ),
                                };
                            }
                        };
                        seen_in_queue = false;
                        attempt_started = Instant::now();
                        if let Err(error) =
                            finish_queue_enqueue(store, &mut state, expected_attempt, true)
                        {
                            return AutoMergeOutcome::MergeFailed {
                                error: format!(
                                    "failed to persist merge-queue re-enqueue attempt: {error}"
                                ),
                            };
                        }
                        if let Err(error) = success_guard.finish("success") {
                            return AutoMergeOutcome::MergeFailed { error };
                        }
                        attempt_started_at = state
                            .merge_queue_attempt_started_at
                            .expect("successful enqueue persists attempt time");
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
                        if state.merge_queue_enqueue_started_at.is_some() {
                            return AutoMergeOutcome::MergeFailed {
                                error: format!(
                                    "PR #{pr} has an uncertain prior exact-head enqueue mutation; refusing to re-arm without queue observation"
                                ),
                            };
                        }
                        if let Err(error) = ensure_unstacked(&client, cwd, &state, global_dir) {
                            return AutoMergeOutcome::MergeFailed { error };
                        }
                        let guard = match MergeQueueMutationGuard::acquire_in_mode(
                            store,
                            cwd,
                            mode,
                            global_dir,
                            &state,
                            "enqueue pull request",
                        ) {
                            Ok(guard) => guard,
                            Err(error) => {
                                return AutoMergeOutcome::MergeFailed { error };
                            }
                        };
                        let expected_attempt = match mark_queue_enqueue_started(store, &mut state) {
                            Ok(expected) => expected,
                            Err(error) => {
                                if let Err(audit_error) = guard.finish("rejected") {
                                    return AutoMergeOutcome::MergeFailed {
                                        error: format!(
                                            "{error}; additionally failed to close pre-network mutation audit: {audit_error}"
                                        ),
                                    };
                                }
                                return AutoMergeOutcome::MergeFailed { error };
                            }
                        };
                        match arm_native_queue(&client, cwd, &state, &observation.id, guard) {
                            Ok(success_guard) => {
                                attempt_started = Instant::now();
                                if let Err(error) =
                                    finish_queue_enqueue(store, &mut state, expected_attempt, true)
                                {
                                    return AutoMergeOutcome::MergeFailed {
                                        error: format!(
                                            "failed to persist pending queue admission: {error}"
                                        ),
                                    };
                                }
                                if let Err(error) = success_guard.finish("success") {
                                    return AutoMergeOutcome::MergeFailed { error };
                                }
                                attempt_started_at = state
                                    .merge_queue_attempt_started_at
                                    .expect("successful enqueue persists attempt time");
                                thread::sleep(QUEUE_POLL_INTERVAL);
                                continue;
                            }
                            Err(QueueArmError::Rejected { error, guard }) => {
                                if let Err(persist_error) =
                                    finish_queue_enqueue(store, &mut state, expected_attempt, false)
                                {
                                    return AutoMergeOutcome::MergeFailed {
                                        error: format!(
                                            "{error}; additionally failed to clear uncertain admission: {persist_error}"
                                        ),
                                    };
                                }
                                if let Err(audit_error) = (*guard).finish("rejected") {
                                    return AutoMergeOutcome::MergeFailed {
                                        error: format!(
                                            "{error}; additionally failed to close rejected mutation audit: {audit_error}"
                                        ),
                                    };
                                }
                                if enqueue_requirements_pending(&error) {
                                    thread::sleep(QUEUE_POLL_INTERVAL);
                                    continue;
                                }
                                return AutoMergeOutcome::MergeFailed { error };
                            }
                            Err(QueueArmError::Uncertain(error)) => {
                                return AutoMergeOutcome::MergeFailed {
                                    error: format!(
                                        "{error}; enqueue outcome is uncertain, so Shipyard retained its durable pre-mutation marker"
                                    ),
                                };
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
        .lock_pr_scoped(&local.repo, local.pr)
        .map_err(|error| format!("failed to lock ship-state: {error}"))?;
    let Some(mut current) = store.get_locked_scoped(&local.repo, local.pr, &lock) else {
        return Err("active ship-state disappeared".to_owned());
    };
    if !same_validation_identity(&current, local)
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
        .save_scoped_locked(&current, &lock)
        .map_err(|error| format!("failed to save ship-state: {error}"))?;
    *local = current;
    Ok(())
}

fn mark_queue_enqueue_started(
    store: &ShipStateStore,
    state: &mut ShipState,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
    let expected_attempt = state.merge_queue_attempt_started_at;
    let started_at = chrono::Utc::now();
    update_queue_state_if_current(store, state, expected_attempt, |current| {
        current.merge_queue_enqueue_started_at = Some(started_at);
    })?;
    Ok(expected_attempt)
}

fn finish_queue_enqueue(
    store: &ShipStateStore,
    state: &mut ShipState,
    expected_attempt: Option<chrono::DateTime<chrono::Utc>>,
    succeeded: bool,
) -> Result<(), String> {
    let finished_at = chrono::Utc::now();
    update_queue_state_if_current(store, state, expected_attempt, |current| {
        current.merge_queue_enqueue_started_at = None;
        if succeeded {
            current.merge_queue_attempt_started_at = Some(finished_at);
            current.merge_queue_observed_at = None;
            current.merge_queue_enqueue_succeeded_at = Some(finished_at);
        }
    })
}

fn archive_queue_state_if_current(
    store: &ShipStateStore,
    local: &ShipState,
    expected_attempt: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(), String> {
    let lock = store
        .lock_pr_scoped(&local.repo, local.pr)
        .map_err(|error| format!("failed to lock ship-state: {error}"))?;
    let Some(current) = store.get_locked_scoped(&local.repo, local.pr, &lock) else {
        return Err("active ship-state disappeared".to_owned());
    };
    if !same_validation_identity(&current, local)
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
        .archive_scoped_locked(&local.repo, local.pr, &lock)
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
        .lock_pr_scoped(&local.repo, local.pr)
        .map_err(|error| format!("failed to lock ship-state: {error}"))?;
    let Some(current) = store.get_locked_scoped(&local.repo, local.pr, &lock) else {
        return Ok(None);
    };
    if !same_validation_identity(&current, local)
        || current.merge_queue_attempt_started_at != expected_attempt
    {
        return Ok(None);
    }
    action().map(Some)
}

/// Every field GitHub's GraphQL `AutoMergeRequest` type exposes. The type is a
/// plain OBJECT implementing no interfaces — notably not `Node` — so it has no
/// `id`, and selecting one makes GitHub reject the *whole* enclosing query with
/// `Field 'id' doesn't exist on type 'AutoMergeRequest'`.
///
/// Refresh with:
/// `gh api graphql -f query='{__type(name:"AutoMergeRequest"){fields{name}}}'`
///
/// Exists to be asserted against, not read at run time — the tests below check
/// the poll query's selection against it.
#[cfg_attr(not(test), allow(dead_code))]
const AUTO_MERGE_REQUEST_FIELDS: [&str; 7] = [
    "authorEmail",
    "commitBody",
    "commitHeadline",
    "enabledAt",
    "enabledBy",
    "mergeMethod",
    "pullRequest",
];

/// Presence probe for `pullRequest.autoMergeRequest`. The poll only needs to
/// know whether the node is null, but GraphQL forbids an empty selection set on
/// an object, so exactly one real field must be named. Keep this a member of
/// [`AUTO_MERGE_REQUEST_FIELDS`]; `queue_poll_query_selects_only_real_auto_merge_fields`
/// enforces that.
const AUTO_MERGE_REQUEST_PROBE: &str = "enabledAt";

/// The merge-queue poll query.
///
/// Built rather than inlined so the `autoMergeRequest` selection is a single
/// named constant a test can validate against the schema field list. An invalid
/// selection here is not cosmetic: [`queue_admission`] issues this query before
/// any mutation, so it takes down merge-queue admission for every
/// queue-governed repository.
fn queue_poll_query() -> String {
    format!(
        "query($owner:String!,$name:String!,$branch:String!,$pr:Int!,$after:String){{repository(owner:$owner,name:$name){{pullRequest(number:$pr){{id headRefOid baseRefName merged autoMergeRequest{{{AUTO_MERGE_REQUEST_PROBE}}} timelineItems(last:1,itemTypes:[REMOVED_FROM_MERGE_QUEUE_EVENT]){{nodes{{... on RemovedFromMergeQueueEvent{{reason createdAt}}}}}}}} mergeQueue(branch:$branch){{entries(first:100,after:$after){{nodes{{position pullRequest{{number}}}} pageInfo{{hasNextPage endCursor}}}}}}}}}}"
    )
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
    let query = queue_poll_query();
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

#[derive(Debug)]
enum QueueArmError {
    Rejected {
        error: String,
        guard: Box<MergeQueueMutationGuard>,
    },
    Uncertain(String),
}

fn arm_native_queue(
    client: &GhClient,
    cwd: &Path,
    state: &ShipState,
    pr_id: &str,
    guard: MergeQueueMutationGuard,
) -> Result<MergeQueueMutationGuard, QueueArmError> {
    let query = r"mutation($prId:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$prId,expectedHeadOid:$head}){mergeQueueEntry{id}}}";
    let mut command = match gh(client, cwd) {
        Ok(command) => command,
        Err(error) => {
            return Err(QueueArmError::Rejected {
                error,
                guard: Box::new(guard),
            });
        }
    };
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
        .map_err(|error| {
            QueueArmError::Uncertain(format!("failed to enqueue merge-queue PR: {error}"))
        })?;
    if output.status.success() {
        return Ok(guard);
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let error = format!("failed to enqueue merge-queue PR: {message}");
    if definitive_enqueue_rejection(&error) {
        Err(QueueArmError::Rejected {
            error,
            guard: Box::new(guard),
        })
    } else {
        drop(guard);
        Err(QueueArmError::Uncertain(error))
    }
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

fn definitive_enqueue_rejection(message: &str) -> bool {
    terminal_github_error(message) || enqueue_requirements_pending(message)
}

#[allow(clippy::too_many_arguments)]
fn revoke_native_queue(
    store: &ShipStateStore,
    client: &GhClient,
    cwd: &Path,
    mode: RuntimeMode,
    global_dir: &Path,
    state: &ShipState,
    observation: &crate::merge_queue::QueuePrObservation,
    queued: bool,
) -> Result<(), String> {
    // A ship-state record owns only the exact head it validated. Once the PR
    // advances, a queue entry or auto-merge request belongs to the newer head
    // and may be carrying fresh required-check work. Revoking it from stale
    // state would discard that work without any red or authority transition.
    if !native_merge_authority_owned_by_ship_state(&state.head_sha, &observation.head_sha) {
        return Ok(());
    }
    // Disable the pending request first so dequeue cannot immediately admit a
    // replacement head again through the still-active auto-merge authority.
    if observation.auto_merge_active {
        let query = r"mutation($prId:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$prId}){pullRequest{id}}}";
        run_queue_mutation(
            store,
            client,
            cwd,
            mode,
            global_dir,
            state,
            query,
            &observation.id,
            "disable native auto-merge",
        )?;
    }
    if queued {
        let query =
            r"mutation($prId:ID!){dequeuePullRequest(input:{id:$prId}){mergeQueueEntry{id}}}";
        run_queue_mutation(
            store,
            client,
            cwd,
            mode,
            global_dir,
            state,
            query,
            &observation.id,
            "dequeue drifted PR",
        )?;
    }
    Ok(())
}

fn native_merge_authority_owned_by_ship_state(validated_head: &str, live_head: &str) -> bool {
    shas_match(validated_head, live_head)
}

fn revoke_drifted_native_merge(
    store: &ShipStateStore,
    cwd: &Path,
    mode: RuntimeMode,
    global_dir: &Path,
    state: &ShipState,
) -> Result<(), String> {
    let client = gh_client(cwd)?;
    let pages = fetch_queue_poll_pages(&client, cwd, state)?;
    let body = pages
        .first()
        .ok_or_else(|| "drift revocation returned no queue pages".to_owned())?;
    let observation = parse_pr_observation(body)
        .map_err(|error| format!("drift revocation observation was malformed: {error}"))?;
    let queued = match crate::merge_queue::parse_queue_pages(&pages, state.pr) {
        crate::merge_queue::QueuePollParse::Valid(snapshot) => snapshot.pr_found,
        crate::merge_queue::QueuePollParse::Errored(error) => {
            return Err(format!(
                "drift revocation could not prove queue membership: {error}"
            ));
        }
    };
    revoke_native_queue(
        store,
        &client,
        cwd,
        mode,
        global_dir,
        state,
        &observation,
        queued,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_queue_mutation(
    store: &ShipStateStore,
    client: &GhClient,
    cwd: &Path,
    mode: RuntimeMode,
    global_dir: &Path,
    state: &ShipState,
    query: &str,
    pr_id: &str,
    action: &str,
) -> Result<(), String> {
    let mut command = gh(client, cwd)?;
    // The ghapp wrapper rejects raw queue-removal mutations. This marker is
    // limited to Shipyard's exact-head, machine-authorized, write-ahead-audited
    // path and lets that guard distinguish it from an ad-hoc GraphQL call.
    command.env("SHIPYARD_INTERNAL_QUEUE_MUTATION", "1");
    let guard =
        MergeQueueMutationGuard::acquire_in_mode(store, cwd, mode, global_dir, state, action)?;
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
        guard.finish("success")?;
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if definitive_mutation_rejection(&message) {
        guard.finish("rejected")?;
    }
    Err(format!("failed to {action}: {message}"))
}

fn definitive_mutation_rejection(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    terminal_github_error(message)
        || [
            "pull request is not in the merge queue",
            "pull request is not queued",
            "auto-merge is not enabled",
            "auto merge is not enabled",
            "could not resolve to a node",
        ]
        .iter()
        .any(|reason| lower.contains(reason))
        || ["400", "404", "405", "409", "410", "422"]
            .iter()
            .any(|status| lower.contains(&format!("(http {status})")))
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

fn cleanup_confirmed_merge(
    disposition: MergeDisposition,
    requested: bool,
    cleanup: impl FnOnce() -> Result<(), String>,
) -> MergeDisposition {
    match disposition {
        MergeDisposition::Merged { cleanup_warning } if requested => MergeDisposition::Merged {
            cleanup_warning: cleanup_warning.or_else(|| cleanup().err()),
        },
        other => other,
    }
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
    if live_merge_queue_present(&queue_body)? {
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
        return match merge_queue_requirement_from_observations(&queue_body, Err(&stderr)).map_err(
            |error| {
                format!(
                    "failed to inspect evaluated branch rules for {repo}:{base_branch}: {error}"
                )
            },
        )? {
            MergeQueueRequirement::Required => Ok(true),
            MergeQueueRequirement::Classic => Ok(false),
            MergeQueueRequirement::PrivateFreeClassicFallback => {
                eprintln!(
                    "shipyard: evaluated branch rules are unavailable on this private-free repository; the authoritative mergeQueue query returned null, so continuing with classic exact-head merge"
                );
                Ok(false)
            }
        };
    }
    let body: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("evaluated branch rules returned invalid JSON: {error}"))?;
    Ok(matches!(
        merge_queue_requirement_from_observations(&queue_body, Ok(&body))?,
        MergeQueueRequirement::Required
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MergeQueueRequirement {
    Required,
    Classic,
    PrivateFreeClassicFallback,
}

fn merge_queue_requirement_from_observations(
    queue_body: &Value,
    evaluated_rules: Result<&Value, &str>,
) -> Result<MergeQueueRequirement, String> {
    if live_merge_queue_present(queue_body)? {
        return Ok(MergeQueueRequirement::Required);
    }
    let body = match evaluated_rules {
        Ok(body) => body,
        Err(stderr) if evaluated_rules_unavailable_on_private_free_plan(stderr) => {
            return Ok(MergeQueueRequirement::PrivateFreeClassicFallback);
        }
        Err(stderr) => return Err(stderr.to_owned()),
    };
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
    crate::merge_queue::rules_require_merge_queue(&Value::Array(rules)).map(|required| {
        if required {
            MergeQueueRequirement::Required
        } else {
            MergeQueueRequirement::Classic
        }
    })
}

fn live_merge_queue_present(body: &Value) -> Result<bool, String> {
    if let Some(errors) = body.get("errors") {
        let errors = errors.as_array().ok_or_else(|| {
            "branch merge-queue query returned malformed GraphQL errors".to_owned()
        })?;
        if !errors.is_empty() {
            return Err("branch merge-queue query returned GraphQL errors".to_owned());
        }
    }
    body.pointer("/data/repository/mergeQueue")
        .map(|queue| !queue.is_null())
        .ok_or_else(|| "branch merge-queue query omitted repository authority".to_owned())
}

/// Signature phrases GitHub uses when it rejects a GraphQL *document* — the
/// query Shipyard sent is invalid, independent of the PR, the repository, or
/// branch protection. Kept narrow and phrase-based because these arrive as
/// prose on `gh`'s stderr with no machine-readable code attached.
const GRAPHQL_MALFORMED_QUERY_SIGNATURES: [&str; 6] = [
    // Selecting a field the type does not expose (`undefinedField`).
    "doesn't exist on type",
    "does not exist on type",
    // Selecting an object without subfields, or a scalar with them.
    "must have a selection of subfields",
    "must not have a selection since type",
    // Bad argument name/value, or an undeclared/unused variable.
    "is not defined by operation",
    "parse error on",
];

/// Whether a `gh` failure means Shipyard sent a malformed GraphQL query.
///
/// This distinction matters for the hand-back: a malformed query is a *Shipyard
/// defect* the operator cannot fix by waiting or by adjusting branch protection,
/// so the merge diagnostic must not send them to investigate the PR. Fails
/// closed — an unrecognised message is treated as an ordinary merge rejection.
pub(super) fn is_graphql_malformed_query_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    GRAPHQL_MALFORMED_QUERY_SIGNATURES
        .iter()
        .any(|signature| lower.contains(signature))
}

const PRIVATE_FREE_RULES_ENTITLEMENT: &str =
    "Upgrade to GitHub Pro or make this repository public to enable this feature.";

fn evaluated_rules_unavailable_on_private_free_plan(stderr: &str) -> bool {
    let expected = format!("{PRIVATE_FREE_RULES_ENTITLEMENT} (HTTP 403)");
    stderr
        .trim()
        .strip_prefix("gh: ")
        .unwrap_or_else(|| stderr.trim())
        == expected
}

pub(super) fn target_requires_merge_queue(
    cwd: &Path,
    repo: &str,
    base_branch: &str,
) -> Result<bool, String> {
    let client = gh_client(cwd)?;
    repository_requires_merge_queue(&client, cwd, repo, base_branch)
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
/// directly through `gh api`. The caller confirms the merged state before
/// running optional branch cleanup and preserving any cleanup warning.
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

    Ok(())
}

fn delete_pr_head_branch(
    client: &GhClient,
    cwd: &Path,
    global_dir: &Path,
    state: &ShipState,
) -> Result<(), String> {
    let info = pr_head_info_rest(client, &state.repo, state.pr, cwd)?;
    let Some(head_repo) = info.head_repo.as_deref() else {
        return Ok(());
    };
    delete_head_branch(
        client,
        cwd,
        global_dir,
        head_repo,
        &info.head_ref,
        &state.head_sha,
    )
}

fn branch_cleanup_git_authority(
    client: &GhClient,
    cwd: &Path,
    global_dir: &Path,
) -> Result<Option<GhClient>, String> {
    let primary_auth = client
        .auth_summary(cwd, GhAuthPolicy::Default)
        .map_err(|error| format!("failed to inspect Git auth for branch cleanup: {error}"))?;
    if matches!(primary_auth.source, GhAuthSourceSummary::GhCli) {
        return Ok(None);
    }

    // A repository may configure the GitHub identity used for ordinary API
    // calls, but it may never choose the native Git executable that receives
    // that credential. Reload only the machine-global layer for the cleanup
    // Git binary; the primary client retains ownership of its credential.
    let config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
        .map_err(|error| format!("failed to load trusted branch-cleanup config: {error}"))?;
    GhClient::from_loaded_config(&config)
        .map(Some)
        .map_err(|error| format!("failed to load trusted branch-cleanup Git config: {error}"))
}

fn require_branch_cleanup_git(
    client: &mut GhClient,
    cwd: &Path,
    global_dir: &Path,
) -> Result<bool, String> {
    let auth = client
        .pin_command_auth(cwd)
        .map_err(|error| format!("failed to pin Git auth for branch cleanup: {error}"))?;
    if matches!(auth.source, GhAuthSourceSummary::GhCli) {
        return Ok(false);
    }
    if let Some(git_authority) = branch_cleanup_git_authority(client, cwd, global_dir)? {
        git_authority
            .prepare_privileged_git_command(cwd)
            .map_err(|error| {
                format!(
                    "--delete-branch requires trusted isolated Git cleanup before merge: {error}"
                )
            })?;
        return Ok(true);
    }
    Ok(false)
}

fn delete_head_branch(
    client: &GhClient,
    cwd: &Path,
    global_dir: &Path,
    repo: &str,
    head_ref: &str,
    expected_sha: &str,
) -> Result<(), String> {
    let git_authority = branch_cleanup_git_authority(client, cwd, global_dir)?;
    let isolated = if let Some(git_authority) = git_authority.as_ref() {
        let parent = tempfile::tempdir()
            .map_err(|error| format!("failed to create isolated Git cleanup root: {error}"))?;
        let repository = parent.path().join("repository");
        std::fs::create_dir(&repository).map_err(|error| {
            format!("failed to create isolated Git cleanup repository: {error}")
        })?;
        let output = git_authority
            .prepare_privileged_git_command(&repository)
            .map_err(|error| format!("failed to prepare trusted Git cleanup: {error}"))?
            .args(["init", "--quiet", "--bare"])
            .output()
            .map_err(|error| format!("failed to initialize isolated Git cleanup: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "failed to initialize isolated Git cleanup: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Some((parent, repository))
    } else {
        None
    };
    let git_cwd = isolated
        .as_ref()
        .map_or(cwd, |(_, repository)| repository.as_path());
    let mut command = if let Some(git_authority) = git_authority.as_ref() {
        client.prepare_git_command_with_binary_authority(git_cwd, git_authority)
    } else {
        client.prepare_git_command(git_cwd)
    }
    .map_err(|error| format!("failed to prepare authenticated git cleanup: {error}"))?;
    let output = command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "push",
            "--no-verify",
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
        .args(rest_merge_args(endpoint, head_sha, merge_method))
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

fn classic_merge_args(
    state: &ShipState,
    merge_method: MergeMethod,
    delete_branch: bool,
    admin: bool,
) -> Vec<String> {
    let mut args = vec![
        "pr".to_owned(),
        "merge".to_owned(),
        state.pr.to_string(),
        "--repo".to_owned(),
        state.repo.clone(),
        "--match-head-commit".to_owned(),
        state.head_sha.clone(),
        merge_method.gh_flag().to_owned(),
    ];
    if delete_branch {
        args.push("--delete-branch".to_owned());
    }
    if admin {
        args.push("--admin".to_owned());
    }
    args
}

fn rest_merge_args(endpoint: &str, head_sha: &str, merge_method: MergeMethod) -> Vec<String> {
    vec![
        "api".to_owned(),
        "-X".to_owned(),
        "PUT".to_owned(),
        endpoint.to_owned(),
        "-f".to_owned(),
        format!("merge_method={}", merge_method.rest_value()),
        "-f".to_owned(),
        format!("sha={head_sha}"),
    ]
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
        "validation-identity-mismatch" => writeln!(
            stdout,
            "PR #{pr}: {}",
            data.get("detail").and_then(Value::as_str).unwrap_or("")
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

    #[cfg(unix)]
    #[test]
    fn app_branch_cleanup_uses_only_machine_global_trusted_git_before_merge() {
        let global = tempfile::tempdir().expect("global config");
        std::fs::write(
            global.path().join("config.toml"),
            r#"
                [github.auth]
                source = "command"
                token_command = ["/bin/echo", "ghs_app_token"]
                "#,
        )
        .expect("machine-global config without trusted Git");
        let native = std::env::current_exe().expect("native test executable");
        let config = crate::config::LoadedConfig {
            data: format!(
                r#"
                [github.auth]
                source = "command"
                token_command = ["/bin/echo", "ghs_app_token"]
                privileged_git_binary = {}
                "#,
                toml::Value::String(native.display().to_string())
            )
            .parse::<toml::Table>()
            .expect("config TOML"),
            global_dir: global.path().to_path_buf(),
            project_dir: None,
            local_dir: None,
            local_overlay_source: crate::config::LocalOverlaySource::None,
        };
        let mut client = GhClient::from_loaded_config(&config).expect("App client");
        let error = require_branch_cleanup_git(&mut client, Path::new("/tmp"), global.path())
            .expect_err("a layered privileged Git override must not authorize cleanup");
        assert!(error.contains("--delete-branch requires trusted isolated Git cleanup"));

        std::fs::write(
            global.path().join("config.toml"),
            format!(
                r#"
                [github.auth]
                source = "command"
                token_command = ["/bin/echo", "ghs_app_token"]
                privileged_git_binary = {}
                "#,
                toml::Value::String(native.display().to_string())
            ),
        )
        .expect("machine-global trusted Git config");
        assert!(
            require_branch_cleanup_git(&mut client, Path::new("/tmp"), global.path())
                .expect("machine-global trusted Git authorizes cleanup")
        );

        let mut ambient = GhClient::ambient();
        assert!(
            !require_branch_cleanup_git(&mut ambient, Path::new("/tmp"), global.path())
                .expect("ambient cleanup retains its existing Git authority")
        );
    }

    #[test]
    fn completed_validation_cannot_merge_replacement_same_head_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let head = "a".repeat(40);
        let mut completed =
            ShipState::new(7751, "owner/repo", "feature/a", "main", &head, "policy-a");
        completed.update_evidence("local", "pass");

        // Job B reused the PR and exact head but changed its validation policy
        // after job A captured its terminal proof. Without the request-bound
        // identity guard, A's synthetic successful merge archives B's state.
        let mut replacement = completed.clone();
        replacement.policy_signature = "policy-b".to_owned();
        replacement.touch();
        store.save(&replacement).expect("replacement state");
        let request = AutoMergeRequest {
            mode: RuntimeMode::Isolated,
            global_dir: temp.path().join("global"),
            pr: 7751,
            merge_method: MergeMethod::Squash,
            delete_branch: true,
            admin: false,
            pr_snapshot_file: None,
            merge_command: None,
            merge_result: Some(MergeResult::Success),
            expected_validation: Some(ValidatedShipIdentity::from(&completed)),
        };

        let outcome = execute_auto_merge(&store, temp.path(), &request).expect("merge phase");
        assert!(matches!(
            outcome,
            AutoMergeOutcome::ValidationIdentityMismatch { ref detail }
                if detail.contains("validation policy")
        ));
        assert_eq!(
            store
                .get_scoped("owner/repo", 7751)
                .expect("replacement remains active")
                .policy_signature,
            "policy-b"
        );
        assert!(
            store.list_archived().is_empty(),
            "stale post-validation work must not archive the replacement state"
        );
    }

    // ── merge-queue poll query shape ────────────────────────────────────

    /// Extract the selection set of `autoMergeRequest{…}` from a query string.
    fn auto_merge_request_selection(query: &str) -> Vec<String> {
        let start = query
            .find("autoMergeRequest{")
            .map(|at| at + "autoMergeRequest{".len())
            .expect("poll query selects autoMergeRequest");
        let end = start
            + query[start..]
                .find('}')
                .expect("autoMergeRequest selection is closed");
        query[start..end]
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn queue_poll_query_selects_only_real_auto_merge_fields() {
        // A field GitHub's schema does not expose makes the whole poll query
        // fail, and `queue_admission` runs it before any mutation — so a stale
        // selection breaks merge-queue admission for every queue-governed repo.
        // Catch that here rather than at merge time.
        let selection = auto_merge_request_selection(&queue_poll_query());
        assert!(
            !selection.is_empty(),
            "GraphQL forbids an empty selection set on an object"
        );
        for field in &selection {
            assert!(
                AUTO_MERGE_REQUEST_FIELDS.contains(&field.as_str()),
                "autoMergeRequest{{{field}}} is not a field of GitHub's AutoMergeRequest type; \
                 valid fields: {AUTO_MERGE_REQUEST_FIELDS:?}"
            );
        }
    }

    #[test]
    fn queue_poll_query_never_selects_auto_merge_request_id() {
        // `AutoMergeRequest` is a plain OBJECT implementing no interfaces, so it
        // is not a `Node` and has never had an `id`. Selecting one produced
        // `gh: Field 'id' doesn't exist on type 'AutoMergeRequest'`.
        assert!(
            !queue_poll_query().contains("autoMergeRequest{id}"),
            "autoMergeRequest has no id field"
        );
        assert!(!AUTO_MERGE_REQUEST_FIELDS.contains(&"id"));
    }

    #[test]
    fn queue_poll_query_keeps_the_fields_the_parsers_read() {
        // The poll response feeds `parse_pr_observation` and
        // `parse_queue_pages`; dropping any of these silently degrades the
        // queue state machine rather than failing loudly.
        let query = queue_poll_query();
        for required in [
            "headRefOid",
            "baseRefName",
            "merged",
            "autoMergeRequest{",
            "REMOVED_FROM_MERGE_QUEUE_EVENT",
            "mergeQueue(branch:$branch)",
            "hasNextPage",
            "endCursor",
        ] {
            assert!(query.contains(required), "poll query lost {required}");
        }
    }

    // ── malformed-GraphQL-query detector ────────────────────────────────

    #[test]
    fn detects_the_auto_merge_request_id_schema_error_verbatim() {
        // Exactly what `gh` printed on stderr when the poll query selected
        // `autoMergeRequest{id}`.
        assert!(is_graphql_malformed_query_error(
            "gh: Field 'id' doesn't exist on type 'AutoMergeRequest'"
        ));
    }

    #[test]
    fn classic_stack_inspection_preserves_only_graphql_rate_limit_fallback() {
        assert_eq!(
            allow_classic_rest_fallback(Err(crate::stacked_pr::StackInspectionError::query(
                "GraphQL: API rate limit already exceeded for user ID 123".to_owned()
            ))),
            Ok(())
        );
        assert_eq!(
            allow_classic_rest_fallback(Err(crate::stacked_pr::StackInspectionError::validation(
                "stack metadata was malformed".to_owned(),
            ),)),
            Err("stack metadata was malformed".to_owned())
        );
        let poisoned_policy = "protected-base stacked_pr_mode must be one of off, observe, or apply; got \"graphql rate limit\"";
        assert_eq!(
            allow_classic_rest_fallback(Err(crate::stacked_pr::StackInspectionError::validation(
                poisoned_policy.to_owned()
            ),)),
            Err(poisoned_policy.to_owned())
        );
    }

    #[test]
    fn detects_other_malformed_query_shapes() {
        for message in [
            "gh: Field 'mergeQueue' must have a selection of subfields",
            "gh: Field 'merged' must not have a selection since type 'Boolean' has no subfields",
            "gh: Variable $after is not defined by operation 'poll'",
            "gh: Parse error on '}' (RCURLY)",
            "Field 'headRefOid' does not exist on type 'PullRequest'",
        ] {
            assert!(
                is_graphql_malformed_query_error(message),
                "should classify as malformed: {message}"
            );
        }
    }

    #[test]
    fn does_not_classify_genuine_merge_rejections_as_malformed() {
        // These are real, PR-side blocks. Misclassifying them would suppress the
        // branch-protection guidance that is correct for them.
        for message in [
            "gh: Pull request is not mergeable",
            "gh: Required status check \"macos\" is expected.",
            "gh: Changes must be approved by a reviewer",
            "! The merge strategy for main is set by the merge queue",
            "gh: Resource not accessible by integration (mergePullRequest)",
            "HTTP 405: Base branch was modified. Review and try the merge again.",
        ] {
            assert!(
                !is_graphql_malformed_query_error(message),
                "should NOT classify as malformed: {message}"
            );
        }
    }

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

    #[test]
    fn detects_exact_private_free_rules_entitlement() {
        assert!(evaluated_rules_unavailable_on_private_free_plan(
            "gh: Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)"
        ));
        assert!(evaluated_rules_unavailable_on_private_free_plan(
            "Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)"
        ));
    }

    #[test]
    fn private_free_rules_detector_rejects_other_auth_and_entitlement_errors() {
        assert!(!evaluated_rules_unavailable_on_private_free_plan(
            "gh: Resource not accessible by integration (HTTP 403)"
        ));
        assert!(!evaluated_rules_unavailable_on_private_free_plan(
            "gh: Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 401)"
        ));
        assert!(!evaluated_rules_unavailable_on_private_free_plan(
            "gh: upgrade to github pro or make this repository public to enable this feature. (HTTP 403)"
        ));
        assert!(!evaluated_rules_unavailable_on_private_free_plan(
            "gh: Upgrade to GitHub Pro or make this repository public. (HTTP 403)"
        ));
        assert!(!evaluated_rules_unavailable_on_private_free_plan(
            "gh: Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)\ngh: Resource not accessible by integration (HTTP 403)"
        ));
    }

    #[test]
    fn live_merge_queue_requires_explicit_null_before_classic_fallback() {
        assert!(
            live_merge_queue_present(&serde_json::json!({
                "data": {"repository": {"mergeQueue": {"id": "MQ_kwDO"}}}
            }))
            .expect("non-null queue")
        );
        assert!(
            !live_merge_queue_present(&serde_json::json!({
                "data": {"repository": {"mergeQueue": null}}
            }))
            .expect("explicit null queue")
        );
        assert!(
            live_merge_queue_present(&serde_json::json!({
                "data": {"repository": {}}
            }))
            .expect_err("missing queue authority must fail closed")
            .contains("omitted repository authority")
        );
        assert!(
            live_merge_queue_present(&serde_json::json!({
                "errors": [{"message": "partial failure"}],
                "data": {"repository": {"mergeQueue": null}}
            }))
            .expect_err("GraphQL errors plus null queue must fail closed")
            .contains("GraphQL errors")
        );
    }

    #[test]
    fn private_free_fallback_requires_authoritative_null_and_exact_rules_error() {
        let queue_null = serde_json::json!({
            "data": {"repository": {"mergeQueue": null}}
        });
        let exact_error = format!("gh: {PRIVATE_FREE_RULES_ENTITLEMENT} (HTTP 403)");
        assert_eq!(
            merge_queue_requirement_from_observations(&queue_null, Err(&exact_error)),
            Ok(MergeQueueRequirement::PrivateFreeClassicFallback)
        );

        let errors_and_null = serde_json::json!({
            "errors": [{"message": "partial failure"}],
            "data": {"repository": {"mergeQueue": null}}
        });
        assert!(
            merge_queue_requirement_from_observations(&errors_and_null, Err(&exact_error)).is_err()
        );

        let mixed_error =
            format!("{exact_error}\ngh: Resource not accessible by integration (HTTP 403)");
        assert!(merge_queue_requirement_from_observations(&queue_null, Err(&mixed_error)).is_err());

        let malformed_rules = serde_json::json!([
            [{"type": "merge_queue"}, {"parameters": {}}]
        ]);
        assert!(
            merge_queue_requirement_from_observations(&queue_null, Ok(&malformed_rules)).is_err()
        );
    }

    #[test]
    fn classic_and_rest_merge_commands_keep_server_side_head_guards() {
        let state = ShipState::new(
            30,
            "Generous-Corp/forge",
            "feature/x",
            "main",
            "b07b9f1ac9069484e2fa8fdb2319b134c69c3c56",
            "policy",
        );
        let mut classic = Command::new("gh");
        classic.args(classic_merge_args(&state, MergeMethod::Squash, true, false));
        let classic_args = classic
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            classic_args,
            vec![
                "pr",
                "merge",
                "30",
                "--repo",
                "Generous-Corp/forge",
                "--match-head-commit",
                "b07b9f1ac9069484e2fa8fdb2319b134c69c3c56",
                "--squash",
                "--delete-branch",
            ]
        );

        let mut rest = Command::new("gh");
        rest.args(rest_merge_args(
            "repos/Generous-Corp/forge/pulls/30/merge",
            &state.head_sha,
            MergeMethod::Squash,
        ));
        let rest_args = rest
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            rest_args,
            vec![
                "api",
                "-X",
                "PUT",
                "repos/Generous-Corp/forge/pulls/30/merge",
                "-f",
                "merge_method=squash",
                "-f",
                "sha=b07b9f1ac9069484e2fa8fdb2319b134c69c3c56",
            ]
        );
    }

    #[test]
    fn classic_merge_can_delegate_branch_cleanup_without_losing_head_guard() {
        let state = ShipState::new(
            30,
            "Generous-Corp/forge",
            "feature/x",
            "main",
            "b07b9f1ac9069484e2fa8fdb2319b134c69c3c56",
            "policy",
        );
        let args = classic_merge_args(&state, MergeMethod::Squash, false, false);

        assert!(!args.iter().any(|arg| arg == "--delete-branch"));
        assert!(args.iter().any(|arg| arg == "--match-head-commit"));
        assert!(args.iter().any(|arg| arg == &state.head_sha));
    }

    #[test]
    fn confirmed_merge_preserves_cleanup_failure_but_enqueued_state_never_cleans() {
        let merged = cleanup_confirmed_merge(
            MergeDisposition::Merged {
                cleanup_warning: None,
            },
            true,
            || Err("lease mismatch".to_owned()),
        );
        assert_eq!(
            merged,
            MergeDisposition::Merged {
                cleanup_warning: Some("lease mismatch".to_owned()),
            }
        );

        let called = std::cell::Cell::new(false);
        let enqueued = cleanup_confirmed_merge(MergeDisposition::Enqueued, true, || {
            called.set(true);
            Ok(())
        });
        assert_eq!(enqueued, MergeDisposition::Enqueued);
        assert!(!called.get());
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
        assert!(definitive_mutation_rejection(
            "GraphQL: pull request is not in the merge queue"
        ));
        assert!(definitive_mutation_rejection("request rejected (HTTP 422)"));
        assert!(!definitive_mutation_rejection(
            "request timed out (HTTP 408)"
        ));
        assert!(!definitive_mutation_rejection(
            "too many requests (HTTP 429)"
        ));
        assert!(!definitive_mutation_rejection(
            "connection reset after request body was sent"
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
        assert!(definitive_enqueue_rejection(
            "Pull request is not mergeable because a required status check is pending"
        ));
        assert!(definitive_enqueue_rejection("HTTP 403: forbidden"));
        assert!(!definitive_enqueue_rejection(
            "failed to enqueue merge-queue PR: operation timed out"
        ));
    }

    #[test]
    fn enqueue_requirements_pending_recognizes_no_required_checks_wording() {
        assert!(enqueue_requirements_pending(
            "no required checks reported on the main branch"
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
        state.merge_queue_enqueue_started_at = Some(chrono::Utc::now());
        assert!(owns_native_merge_authority(&state));
        assert!(!auto_merge_has_exact_head_proof(&state));
        state.merge_queue_enqueue_started_at = None;
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

        state.merge_queue_enqueue_started_at = Some(observed);
        assert!(!queue_absence_allows_arm(
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
    fn queue_enqueue_marker_brackets_mutation_and_preserves_uncertainty() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = ShipState::new(13, "owner/repo", "feature/x", "main", "abc", "policy");
        store.save(&state).expect("save state");

        let expected_attempt =
            mark_queue_enqueue_started(&store, &mut state).expect("persist pre-mutation marker");
        assert!(state.merge_queue_enqueue_started_at.is_some());
        assert!(
            store
                .get(state.pr)
                .expect("persisted state")
                .merge_queue_enqueue_started_at
                .is_some()
        );

        finish_queue_enqueue(&store, &mut state, expected_attempt, true)
            .expect("persist successful mutation");
        assert!(state.merge_queue_enqueue_started_at.is_none());
        assert!(state.merge_queue_attempt_started_at.is_some());
        assert!(state.merge_queue_enqueue_succeeded_at.is_some());

        let expected_attempt =
            mark_queue_enqueue_started(&store, &mut state).expect("persist retry marker");
        finish_queue_enqueue(&store, &mut state, expected_attempt, false)
            .expect("persist rejected mutation");
        assert!(state.merge_queue_enqueue_started_at.is_none());
        assert!(state.merge_queue_enqueue_succeeded_at.is_some());
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
    fn stale_ship_state_cannot_own_native_merge_authority_for_a_newer_head() {
        assert!(!native_merge_authority_owned_by_ship_state(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ));
        assert!(native_merge_authority_owned_by_ship_state(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ));
    }

    #[test]
    fn branch_rule_path_segments_are_percent_encoded() {
        assert_eq!(encode_path_segment("main"), "main");
        assert_eq!(encode_path_segment("release/1.2"), "release%2F1.2");
        assert_eq!(encode_path_segment("topic name"), "topic%20name");
    }
}
