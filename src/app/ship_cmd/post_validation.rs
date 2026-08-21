use std::borrow::Cow;
#[cfg(test)]
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::{
    AutoMergeOutcome, AutoMergeRequest, CliFailure, LoadedConfig, MergeMethod, MergeResult,
    RuntimeMode, SHIP_EXIT_MERGE_CLIENT_DEFECT, SHIP_EXIT_VALIDATION_STATE_MISSING, ShipStateStore,
    WedgeClass, WedgeInputs, classify_wedge, execute_auto_merge,
    fetch_head_and_status_check_rollup_with_cwd, is_graphql_malformed_query_error, sha_matches,
    supervise_merge_queue, validated_green_contexts,
};
use crate::app::auto_merge_cmd::ValidatedShipIdentity;
use crate::queue_request::{QueuedShipDisposition, QueuedShipDispositionKind};
use crate::ship_state::ShipState;

/// Typed result of the merge-readiness phase that follows local validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ShipRenderState {
    ValidationFailed,
    Merged,
    /// Shipyard's locally-supervised targets all passed, but deterministic
    /// stewardship refused the merge phase or the downstream merge call was
    /// rejected. The wrapped value describes that merge-readiness outcome.
    GreenNotMerged(String),
    /// Validation remains green while deterministic stewardship waits for a
    /// live GitHub/ship-state readiness snapshot.
    GreenPendingMergeReadiness(String),
    /// Validation remains green, but its durable scoped state disappeared, so
    /// deterministic stewardship cannot continue until state is recovered.
    GreenValidationStateMissing(String),
    /// A merge rejection proven to be a flaky required leg on the validated
    /// SHA, with every failed required context mapped to a green local target.
    GreenNotMergedFlakyRequired {
        error: String,
        red_contexts: Vec<String>,
    },
    /// Shipyard sent GitHub a malformed merge request; the PR is not at fault.
    GreenNotMergedClientDefect(String),
    /// The live PR head advanced past the SHA Shipyard validated.
    GreenNotMergedHeadSuperseded {
        validated: String,
        current: String,
    },
}

impl ShipRenderState {
    pub(super) fn merged(&self) -> bool {
        matches!(self, Self::Merged)
    }

    pub(super) fn merge_error(&self) -> Option<Cow<'_, str>> {
        match self {
            Self::ValidationFailed | Self::Merged => None,
            Self::GreenNotMerged(error)
            | Self::GreenPendingMergeReadiness(error)
            | Self::GreenValidationStateMissing(error)
            | Self::GreenNotMergedClientDefect(error)
            | Self::GreenNotMergedFlakyRequired { error, .. } => Some(Cow::Borrowed(error)),
            Self::GreenNotMergedHeadSuperseded { validated, current } => Some(Cow::Owned(format!(
                "live PR head {current} superseded the validated SHA {validated}; re-run shipyard ship to validate the new head"
            ))),
        }
    }

    pub(super) fn status(&self) -> &'static str {
        match self {
            Self::ValidationFailed => "validation_failed",
            Self::Merged => "merged",
            Self::GreenNotMerged(_) => "green_not_merged",
            Self::GreenPendingMergeReadiness(_) => "green_pending_merge_readiness",
            Self::GreenValidationStateMissing(_) => "green_validation_state_missing",
            Self::GreenNotMergedFlakyRequired { .. } => "green_not_merged_flaky_required",
            Self::GreenNotMergedClientDefect(_) => "green_not_merged_client_defect",
            Self::GreenNotMergedHeadSuperseded { .. } => "green_not_merged_head_superseded",
        }
    }

    pub(super) fn exit_code(&self) -> ExitCode {
        match self {
            Self::ValidationFailed => ExitCode::from(1),
            Self::GreenNotMergedClientDefect(_) => ExitCode::from(SHIP_EXIT_MERGE_CLIENT_DEFECT),
            Self::GreenValidationStateMissing(_) => {
                ExitCode::from(SHIP_EXIT_VALIDATION_STATE_MISSING)
            }
            Self::Merged
            | Self::GreenNotMerged(_)
            | Self::GreenPendingMergeReadiness(_)
            | Self::GreenNotMergedFlakyRequired { .. }
            | Self::GreenNotMergedHeadSuperseded { .. } => ExitCode::SUCCESS,
        }
    }

    pub(super) fn queued_disposition(&self) -> QueuedShipDisposition {
        let kind = match self {
            Self::ValidationFailed => QueuedShipDispositionKind::ValidationFailed,
            Self::Merged => QueuedShipDispositionKind::Merged,
            Self::GreenNotMerged(_) => QueuedShipDispositionKind::GreenNotMerged,
            Self::GreenPendingMergeReadiness(_) => {
                QueuedShipDispositionKind::GreenPendingMergeReadiness
            }
            Self::GreenValidationStateMissing(_) => {
                QueuedShipDispositionKind::GreenValidationStateMissing
            }
            Self::GreenNotMergedFlakyRequired { .. } => {
                QueuedShipDispositionKind::GreenNotMergedFlakyRequired
            }
            Self::GreenNotMergedClientDefect(_) => {
                QueuedShipDispositionKind::GreenNotMergedClientDefect
            }
            Self::GreenNotMergedHeadSuperseded { .. } => {
                QueuedShipDispositionKind::GreenNotMergedHeadSuperseded
            }
        };
        let exit_code = match self {
            Self::ValidationFailed => 1,
            Self::GreenNotMergedClientDefect(_) => SHIP_EXIT_MERGE_CLIENT_DEFECT,
            Self::GreenValidationStateMissing(_) => SHIP_EXIT_VALIDATION_STATE_MISSING,
            Self::Merged
            | Self::GreenNotMerged(_)
            | Self::GreenPendingMergeReadiness(_)
            | Self::GreenNotMergedFlakyRequired { .. }
            | Self::GreenNotMergedHeadSuperseded { .. } => 0,
        };
        let detail = self.merge_error().map(Cow::into_owned);
        QueuedShipDisposition::new(kind, exit_code, detail.as_deref())
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn post_run_merge_state(
    pr: u64,
    cwd: &Path,
    store: &ShipStateStore,
    config: &LoadedConfig,
    mode: RuntimeMode,
    repo: &str,
    validation_passed: bool,
    validated_state: &ShipState,
    merge_command: Option<PathBuf>,
    merge_result: Option<MergeResult>,
    pr_snapshot_file: Option<PathBuf>,
) -> Result<ShipRenderState, CliFailure> {
    if !validation_passed {
        return Ok(ShipRenderState::ValidationFailed);
    }
    let request = AutoMergeRequest {
        mode,
        global_dir: config.global_dir.clone(),
        pr,
        merge_method: MergeMethod::Squash,
        delete_branch: true,
        admin: false,
        pr_snapshot_file,
        merge_command,
        merge_result,
        expected_validation: Some(ValidatedShipIdentity::from(validated_state)),
    };
    match execute_auto_merge(store, cwd, &request)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
    {
        AutoMergeOutcome::Merged { .. } | AutoMergeOutcome::AlreadyMerged => {
            Ok(ShipRenderState::Merged)
        }
        AutoMergeOutcome::Enqueued => {
            match supervise_merge_queue(
                store,
                cwd,
                mode,
                &config.global_dir,
                pr,
                true,
                request.expected_validation.as_ref(),
            ) {
                AutoMergeOutcome::Merged { .. } | AutoMergeOutcome::AlreadyMerged => {
                    Ok(ShipRenderState::Merged)
                }
                AutoMergeOutcome::SupersededSha { validated, current } => {
                    Ok(ShipRenderState::GreenNotMergedHeadSuperseded { validated, current })
                }
                AutoMergeOutcome::ValidationIdentityMismatch { detail } => {
                    Ok(ShipRenderState::GreenNotMerged(detail))
                }
                // Queue supervision re-runs the merge-queue poll query, so it
                // can surface the same malformed-query defect as admission.
                AutoMergeOutcome::MergeFailed { error } => Ok(green_not_merged(error)),
                AutoMergeOutcome::PrNotFound => Ok(green_validation_state_missing(pr)),
                pending @ (AutoMergeOutcome::InFlight { .. }
                | AutoMergeOutcome::Enqueued
                | AutoMergeOutcome::TargetFailed { .. }) => {
                    Ok(pending_merge_readiness(pr, &pending))
                }
            }
        }
        AutoMergeOutcome::MergeFailed { error } => {
            Ok(classify_merge_failure(store, config, cwd, repo, pr, error))
        }
        AutoMergeOutcome::SupersededSha { validated, current } => {
            Ok(ShipRenderState::GreenNotMergedHeadSuperseded { validated, current })
        }
        AutoMergeOutcome::ValidationIdentityMismatch { detail } => {
            Ok(ShipRenderState::GreenNotMerged(detail))
        }
        AutoMergeOutcome::PrNotFound => Ok(green_validation_state_missing(pr)),
        pending @ (AutoMergeOutcome::InFlight { .. } | AutoMergeOutcome::TargetFailed { .. }) => {
            Ok(pending_merge_readiness(pr, &pending))
        }
    }
}

fn pending_merge_readiness(pr: u64, outcome: &AutoMergeOutcome) -> ShipRenderState {
    let detail = match outcome {
        AutoMergeOutcome::InFlight { .. } | AutoMergeOutcome::Enqueued => {
            "merge readiness is still in flight"
        }
        AutoMergeOutcome::TargetFailed { .. } => "the merge-readiness snapshot is not yet green",
        _ => unreachable!("only non-ready outcomes reach the readiness classifier"),
    };
    green_pending_merge_readiness(pr, detail)
}

pub(super) fn green_not_merged(error: String) -> ShipRenderState {
    if is_graphql_malformed_query_error(&error) {
        ShipRenderState::GreenNotMergedClientDefect(error)
    } else {
        ShipRenderState::GreenNotMerged(error)
    }
}

pub(super) fn green_pending_merge_readiness(pr: u64, detail: &str) -> ShipRenderState {
    ShipRenderState::GreenPendingMergeReadiness(format!(
        "PR #{pr}: local validation passed; {detail}; deterministic stewardship retains merge authority"
    ))
}

pub(super) fn green_validation_state_missing(pr: u64) -> ShipRenderState {
    ShipRenderState::GreenValidationStateMissing(format!(
        "PR #{pr}: local validation passed, but its durable scoped ship state is missing; deterministic stewardship cannot own merge readiness until state is recovered"
    ))
}

fn classify_merge_failure(
    store: &ShipStateStore,
    config: &LoadedConfig,
    cwd: &Path,
    repo: &str,
    pr: u64,
    error: String,
) -> ShipRenderState {
    if is_graphql_malformed_query_error(&error) {
        return ShipRenderState::GreenNotMergedClientDefect(error);
    }
    let Some(state) = store.get_scoped(repo, pr) else {
        return ShipRenderState::GreenNotMerged(error);
    };
    let green = validated_green_contexts(&state, config);
    if green.is_empty() {
        return ShipRenderState::GreenNotMerged(error);
    }
    let Ok((live_head, rollup)) =
        fetch_head_and_status_check_rollup_with_cwd(RuntimeMode::Shipyard, cwd, repo, pr)
    else {
        return ShipRenderState::GreenNotMerged(error);
    };
    if !sha_matches(&live_head, &state.head_sha) {
        return ShipRenderState::GreenNotMerged(error);
    }
    match classify_wedge(&WedgeInputs {
        rollup: &rollup,
        validated_green_contexts: &green,
    }) {
        WedgeClass::FlakyRequired { red_contexts } => {
            ShipRenderState::GreenNotMergedFlakyRequired {
                error,
                red_contexts,
            }
        }
        WedgeClass::RequiredStillPending | WedgeClass::NotRecoverable { .. } => {
            ShipRenderState::GreenNotMerged(error)
        }
    }
}

pub(super) fn render_green_pending_merge_readiness<W: Write>(
    stdout: &mut W,
    pr: u64,
    detail: &str,
) -> std::io::Result<()> {
    writeln!(
        stdout,
        "Shipyard-validated targets passed for PR #{pr}; that validation proof remains green."
    )?;
    writeln!(stdout, "  merge readiness: {detail}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "Do not rerun validation. Deterministic stewardship owns merge readiness."
    )?;
    writeln!(
        stdout,
        "Wait on the live policy instead: `shipyard wait pr {pr} --state green`."
    )
}

pub(super) fn render_green_validation_state_missing<W: Write>(
    stdout: &mut W,
    pr: u64,
    detail: &str,
) -> std::io::Result<()> {
    writeln!(
        stdout,
        "Shipyard-validated targets passed for PR #{pr}; that validation proof remains green."
    )?;
    writeln!(stdout, "  operational failure: {detail}")?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "Do not mark validation failed or automatically rerun it. Recover the durable scoped ship state before waiting for or attempting the merge."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_ready_outcomes_are_green_pending_and_never_request_validation_reruns() {
        let outcomes = [
            AutoMergeOutcome::InFlight {
                evidence: BTreeMap::new(),
            },
            AutoMergeOutcome::TargetFailed {
                failing_targets: vec!["required".to_owned()],
                evidence: BTreeMap::new(),
            },
        ];
        for outcome in outcomes {
            let state = pending_merge_readiness(7751, &outcome);
            assert_eq!(state.status(), "green_pending_merge_readiness");
            assert_eq!(
                format!("{:?}", state.exit_code()),
                format!("{:?}", ExitCode::SUCCESS)
            );
            let ShipRenderState::GreenPendingMergeReadiness(detail) = state else {
                panic!("non-ready state must remain distinct from validation failure");
            };
            let mut output = Vec::new();
            render_green_pending_merge_readiness(&mut output, 7751, &detail).expect("render");
            let output = String::from_utf8(output).expect("UTF-8");
            assert!(output.contains("validation proof remains green"));
            assert!(output.contains("Do not rerun validation"));
            assert!(output.contains("shipyard wait pr 7751 --state green"));
            assert!(!output.contains("shipyard ship --pr"));
        }
    }

    #[test]
    fn missing_ship_state_is_a_distinct_green_proof_operational_failure() {
        let state = green_validation_state_missing(7751);
        assert_eq!(state.status(), "green_validation_state_missing");
        assert_eq!(
            format!("{:?}", state.exit_code()),
            format!("{:?}", ExitCode::from(SHIP_EXIT_VALIDATION_STATE_MISSING))
        );
        let ShipRenderState::GreenValidationStateMissing(detail) = state else {
            panic!("missing state must not be classified as validation failure or readiness")
        };
        let mut output = Vec::new();
        render_green_validation_state_missing(&mut output, 7751, &detail).expect("render");
        let output = String::from_utf8(output).expect("UTF-8");
        assert!(output.contains("validation proof remains green"));
        assert!(output.contains("operational failure"));
        assert!(output.contains("Do not mark validation failed"));
        assert!(output.contains("Recover the durable scoped ship state"));
        assert!(!output.contains("shipyard wait pr"));
        assert!(!output.contains("shipyard ship --pr"));
    }
}
