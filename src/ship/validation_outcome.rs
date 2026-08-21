//! Immutable validation proof and crash-recovered queue outcomes.
//!
//! Validation evidence belongs to the completed queue job. GitHub readiness
//! and merge operations are a separate, deterministic lifecycle and must not
//! rewrite a passing proof when they are pending or temporarily unavailable.

use std::path::Path;

use crate::executor::dispatch::ResolvedTarget;
use crate::job::{Job, ValidationMode};
use crate::queue::{Queue, QueueError};
use crate::queue_request::{
    QueueOutcomeStore, QueueRequestError, QueueRequestStore, QueuedExecutionKind,
    QueuedExecutionOutcome, QueuedShipDisposition, QueuedShipDispositionKind,
};
use crate::ship_state::{ShipState, ShipStateStore, compute_policy_signature};

use super::{
    ShipExecutionError, ShipExecutionRequest, unsaved_ship_state, update_ship_state_from_job,
};

pub(super) fn persist_recovered_outcomes(
    recovered: &[Job],
    state_dir: &Path,
    ship_state: &ShipStateStore,
) -> Result<(), ShipExecutionError> {
    if recovered.is_empty() {
        return Ok(());
    }
    let request_store = QueueRequestStore::new(state_dir).map_err(QueueRequestError::from)?;
    let outcome_store = QueueOutcomeStore::new(state_dir).map_err(QueueRequestError::from)?;
    for job in recovered {
        let Some(envelope) = request_store.load(&job.id)? else {
            continue;
        };
        let outcome = match envelope.kind {
            QueuedExecutionKind::Run => QueuedExecutionOutcome::run(job.id.clone()),
            QueuedExecutionKind::Ship => {
                let request = envelope.to_ship_request()?;
                let existing = ship_state.get_scoped(&request.repo, request.pr);
                recovered_ship_outcome(&request, job, existing)
            }
        };
        outcome_store.save(&outcome)?;
        Queue::new(state_dir)
            .map_err(QueueError::from)?
            .publish_terminal_manifest_if_current(job)?;
    }
    Ok(())
}

/// Persist the kind-specific durable outcome for one terminal queue job.
pub(crate) fn persist_terminal_outcome(
    job: &Job,
    state_dir: &Path,
) -> Result<(), ShipExecutionError> {
    let ship_state = ShipStateStore::new(state_dir.join("ship"))
        .map_err(|error| ShipExecutionError::ShipState(error.to_string()))?;
    persist_recovered_outcomes(std::slice::from_ref(job), state_dir, &ship_state)
}

pub(super) fn completed_validation_disposition(job: &Job) -> QueuedShipDisposition {
    if job.passed() {
        QueuedShipDisposition::new(
            QueuedShipDispositionKind::GreenPendingMergeReadiness,
            0,
            Some(
                "local validation completed; deterministic post-validation merge readiness has not completed",
            ),
        )
    } else {
        QueuedShipDisposition::new(
            QueuedShipDispositionKind::ValidationFailed,
            1,
            Some("one or more locally supervised validation targets failed"),
        )
    }
}

fn recovered_ship_outcome(
    request: &ShipExecutionRequest,
    job: &Job,
    existing: Option<ShipState>,
) -> QueuedExecutionOutcome {
    let resumed_existing_state = existing
        .as_ref()
        .is_some_and(|state| validation_proof_metadata_matches(request, job, state));
    let state = validation_proof_state(request, job, existing);
    let disposition = if job.passed() {
        QueuedShipDisposition::new(
            QueuedShipDispositionKind::PostValidationOperationalFailure,
            1,
            Some(
                "terminal validation proof was recovered after the worker outcome was unavailable; merge readiness requires deterministic re-evaluation",
            ),
        )
    } else {
        QueuedShipDisposition::new(
            QueuedShipDispositionKind::ValidationFailed,
            1,
            Some(
                "terminal validation outcome was recovered after the worker outcome was unavailable",
            ),
        )
    };
    QueuedExecutionOutcome::ship_with_post_validation(
        job.id.clone(),
        request.pr,
        state,
        resumed_existing_state,
        disposition,
    )
}

/// Build the immutable validation-proof snapshot for a completed ship job.
///
/// The active `ShipState` may concurrently describe transient GitHub merge
/// readiness. A terminal queue outcome instead derives target evidence solely
/// from the authoritative completed job. Captured PR metadata and merge-queue
/// timestamps are retained only when every validation identity field matches
/// the immutable request; a concurrently reactivated head falls back to a
/// request-bound state.
pub(crate) fn validation_proof_state(
    request: &ShipExecutionRequest,
    job: &Job,
    captured: Option<ShipState>,
) -> ShipState {
    let captured = captured.filter(|state| validation_proof_metadata_matches(request, job, state));
    let mut state = captured.unwrap_or_else(|| unsaved_ship_state(request, &job.target_names));
    state.evidence_snapshot.clear();
    state.dispatched_runs.clear();
    update_ship_state_from_job(&mut state, request, job);
    state
}

fn validation_proof_metadata_matches(
    request: &ShipExecutionRequest,
    job: &Job,
    state: &ShipState,
) -> bool {
    let validation_policy = policy_signature(&request.targets, &job.target_names, request.mode);
    state.pr == request.pr
        && state.repo.eq_ignore_ascii_case(&request.repo)
        && state.branch == request.branch
        && state.base_branch == request.base_branch
        && state.head_sha.eq_ignore_ascii_case(&request.sha)
        && state.policy_signature == validation_policy
}

pub(super) fn policy_signature(
    targets: &[ResolvedTarget],
    target_names: &[String],
    mode: ValidationMode,
) -> String {
    let platforms = targets
        .iter()
        .map(|target| target.platform.clone())
        .collect::<Vec<_>>();
    compute_policy_signature(&platforms, target_names, policy_mode_label(mode))
}

fn policy_mode_label(mode: ValidationMode) -> &'static str {
    match mode {
        ValidationMode::Full => "FULL",
        ValidationMode::Smoke => "SMOKE",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::Utc;

    use super::*;
    use crate::job::{JobKind, Priority, TargetResult, TargetStatus};
    use crate::log_retention::{TERMINAL_MANIFEST_FILE, read_terminal_manifest};
    use crate::queue_request::{QueuedExecutionEnvelope, QueuedExecutionOutcome};

    fn terminal_ship_job_with_missing_manifest(
        state_dir: &Path,
        request: &ShipExecutionRequest,
    ) -> (Job, std::path::PathBuf) {
        let pending = Job::create(
            &request.sha,
            &request.branch,
            vec!["local".to_owned()],
            request.mode,
            request.priority,
        )
        .with_kind(JobKind::Ship);
        let job = pending
            .start()
            .expect("start")
            .with_result(TargetResult::new(
                "local",
                "macos-arm64",
                TargetStatus::Pass,
                "local",
            ))
            .complete()
            .expect("complete");
        let mut queue = Queue::new(state_dir).expect("queue");
        queue.enqueue(pending).expect("enqueue");
        let log_dir = state_dir.join("logs").join(&job.id);
        std::fs::create_dir_all(&log_dir).expect("log dir");
        queue.update(&job).expect("terminal queue state");
        std::fs::remove_file(log_dir.join(TERMINAL_MANIFEST_FILE))
            .expect("simulate missing crash-recovery manifest");
        assert!(read_terminal_manifest(&log_dir).is_none());
        (job, log_dir)
    }

    #[test]
    fn recovered_terminal_ship_outcome_rebuilds_immutable_validation_proof() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        let request = ShipExecutionRequest {
            pr: 7_751,
            repo: "owner/repo".to_owned(),
            branch: "feature/validated".to_owned(),
            base_branch: "main".to_owned(),
            sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            commit_subject: "validated change".to_owned(),
            pr_url: Some("https://github.com/owner/repo/pull/7751".to_owned()),
            pr_title: Some("immutable validation proof".to_owned()),
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: true,
            fail_fast: false,
            resume_from: None,
            advisory_targets: BTreeSet::new(),
            adopt_head: false,
            pr_snapshot_file: None,
            targets: Vec::new(),
        };
        let (job, log_dir) = terminal_ship_job_with_missing_manifest(&state_dir, &request);
        QueueRequestStore::new(&state_dir)
            .expect("request store")
            .save(&QueuedExecutionEnvelope::from_ship_request(
                &job.id,
                temp.path(),
                &request,
            ))
            .expect("save request");

        let mut changed_policy = ShipState::new(
            request.pr,
            &request.repo,
            &request.branch,
            &request.base_branch,
            &request.sha,
            "same-head-new-policy",
        );
        changed_policy.pr_title = "metadata from a different validation contract".to_owned();
        changed_policy.update_evidence("local", "fail");
        changed_policy.merge_queue_observed_at = Some(Utc::now());
        ShipStateStore::new(state_dir.join("ship"))
            .expect("ship state")
            .save(&changed_policy)
            .expect("save changed policy state");

        persist_terminal_outcome(&job, &state_dir)
            .expect("supervisor recovery persists immutable proof");
        let manifest = read_terminal_manifest(&log_dir).expect("recovered terminal manifest");
        assert!(!manifest.failed);

        let outcome = QueueOutcomeStore::new(&state_dir)
            .expect("outcomes")
            .load(&job.id)
            .expect("load")
            .expect("recovered outcome");
        let QueuedExecutionOutcome::Ship {
            ship_state,
            resumed_existing_state,
            post_validation,
            ..
        } = outcome
        else {
            panic!("expected ship outcome");
        };
        assert!(job.passed());
        assert!(!resumed_existing_state);
        assert_eq!(
            ship_state.policy_signature,
            policy_signature(&request.targets, &job.target_names, request.mode)
        );
        assert_eq!(ship_state.pr_title, request.pr_title.as_deref().unwrap());
        assert_eq!(ship_state.evidence_snapshot["local"], "pass");
        assert_eq!(ship_state.merge_queue_observed_at, None);
        let disposition = post_validation.expect("operational recovery disposition");
        assert_eq!(
            disposition.kind,
            QueuedShipDispositionKind::PostValidationOperationalFailure
        );
        assert_eq!(disposition.exit_code, 1);
        assert!(
            disposition
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("merge readiness requires"))
        );
    }
}
