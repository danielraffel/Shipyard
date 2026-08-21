use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::Duration as StdDuration;

use chrono::{Duration, Utc};

use super::{CliFailure, ship_cmd::finish_background_ship};
use crate::execution_supervisor::verify_worker_authority;
use crate::identity::RuntimeMode;
use crate::queue::{Queue, QueueDeferredRequeue};
use crate::queue_request::{
    QueueOutcomeStore, QueueRequestStore, QueuedExecutionKind, QueuedExecutionOutcome,
    QueuedShipDisposition,
};
use crate::ship::{
    ShipExecutionError, execute_started_queued_job, persist_terminal_outcome,
    validation_proof_state,
};
use crate::ship_state::ShipState;

pub(super) fn execution_worker_command(
    job_id: &str,
    generation: &str,
    mode: RuntimeMode,
    global_dir: &Path,
    state_dir: &Path,
) -> Result<ExitCode, CliFailure> {
    verify_worker_authority(state_dir, job_id, generation)
        .map_err(|error| CliFailure::new(3, format!("worker authority rejected: {error}")))?;

    match execute_started_queued_job(job_id, mode, global_dir, state_dir) {
        Ok((kind, job)) => {
            if cancellation_is_waiting_for_supervisor(state_dir, job_id)? {
                await_supervisor_cancellation();
            }
            if kind == QueuedExecutionKind::Ship {
                let envelope = QueueRequestStore::new(state_dir)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?
                    .load(job_id)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?
                    .ok_or_else(|| {
                        CliFailure::new(1, "ship request disappeared after execution")
                    })?;
                let request = envelope
                    .to_ship_request()
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
                let finish = finish_background_ship(&request, &job, mode, global_dir, state_dir)
                    .and_then(|(code, terminal_state, post_validation)| {
                        persist_background_ship_outcome(
                            state_dir,
                            job_id,
                            &request,
                            &job,
                            terminal_state,
                            post_validation,
                        )?;
                        Ok(code)
                    });
                return match finish {
                    Ok(code) => Ok(code),
                    Err(error) => {
                        if let Err(persist_error) = persist_ship_completion_failure(
                            state_dir,
                            job_id,
                            &request,
                            &job,
                            error.message(),
                        ) {
                            return Err(CliFailure::new(
                                1,
                                format!(
                                    "{}; additionally failed to persist the typed completion disposition: {}",
                                    error.message(),
                                    persist_error.message()
                                ),
                            ));
                        }
                        Err(error)
                    }
                };
            }
            Ok(if job.passed() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Err(ShipExecutionError::SchedulerDeferred(reason)) => {
            requeue_scheduler_deferred_job(state_dir, job_id, reason)?;
            await_supervisor_cancellation();
        }
        Err(error) => {
            let mut queue = Queue::new(state_dir)
                .map_err(|queue_error| CliFailure::new(1, queue_error.to_string()))?;
            if queue
                .get(job_id)
                .map_err(|queue_error| CliFailure::new(1, queue_error.to_string()))?
                .is_some_and(|job| {
                    job.status == crate::job::JobStatus::Running
                        && job.cancel_requested_at.is_some()
                })
            {
                await_supervisor_cancellation();
            }
            if let Some(completed) = queue
                .complete_running_uncertain(job_id, &error.to_string())
                .map_err(|queue_error| CliFailure::new(1, queue_error.to_string()))?
            {
                if completed.status == crate::job::JobStatus::Running
                    && completed.cancel_requested_at.is_some()
                {
                    await_supervisor_cancellation();
                }
                persist_terminal_outcome(&completed, state_dir)
                    .map_err(|persist_error| CliFailure::new(1, persist_error.to_string()))?;
            }
            Err(CliFailure::new(1, error.to_string()))
        }
    }
}

fn persist_ship_completion_failure(
    state_dir: &Path,
    job_id: &str,
    request: &crate::ship::ShipExecutionRequest,
    job: &crate::job::Job,
    detail: &str,
) -> Result<(), CliFailure> {
    let disposition_kind = if job.passed() {
        crate::queue_request::QueuedShipDispositionKind::PostValidationOperationalFailure
    } else {
        crate::queue_request::QueuedShipDispositionKind::ValidationFailed
    };
    persist_background_ship_outcome(
        state_dir,
        job_id,
        request,
        job,
        None,
        QueuedShipDisposition::new(disposition_kind, 1, Some(detail)),
    )
}

fn persist_background_ship_outcome(
    state_dir: &Path,
    job_id: &str,
    request: &crate::ship::ShipExecutionRequest,
    job: &crate::job::Job,
    terminal_state: Option<ShipState>,
    post_validation: QueuedShipDisposition,
) -> Result<(), CliFailure> {
    // Validation completion is authoritative even if separately-owned active
    // ship state is missing or carries a transient non-ready GitHub snapshot.
    // Persist target evidence reconstructed from the immutable completed job;
    // merge readiness stays in the active steward-owned state machine.
    let validation_state = validation_proof_state(request, job, terminal_state);
    persist_completed_ship_outcome(
        state_dir,
        job_id,
        request.pr,
        job,
        validation_state,
        Some(post_validation),
    )
}

fn persist_completed_ship_outcome(
    state_dir: &Path,
    job_id: &str,
    pr: u64,
    job: &crate::job::Job,
    terminal_state: ShipState,
    post_validation: Option<QueuedShipDisposition>,
) -> Result<(), CliFailure> {
    let outcome_store =
        QueueOutcomeStore::new(state_dir).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let resumed_existing_state = outcome_store
        .load(job_id)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .and_then(|outcome| match outcome {
            QueuedExecutionOutcome::Ship {
                resumed_existing_state,
                ..
            } => Some(resumed_existing_state),
            QueuedExecutionOutcome::Run { .. } => None,
        })
        .unwrap_or(false);
    let outcome = match post_validation {
        Some(post_validation) => QueuedExecutionOutcome::ship_with_post_validation(
            job.id.clone(),
            pr,
            terminal_state,
            resumed_existing_state,
            post_validation,
        ),
        None => {
            QueuedExecutionOutcome::ship(job.id.clone(), pr, terminal_state, resumed_existing_state)
        }
    };
    outcome_store
        .save(&outcome)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn requeue_scheduler_deferred_job(
    state_dir: &Path,
    job_id: &str,
    reason: String,
) -> Result<(), CliFailure> {
    let mut queue =
        Queue::new(state_dir).map_err(|queue_error| CliFailure::new(1, queue_error.to_string()))?;
    let requeued = queue
        .requeue_deferred_daemon_worker(QueueDeferredRequeue {
            job_id: job_id.to_owned(),
            reason,
            defer_until: Some(Utc::now() + Duration::seconds(5)),
        })
        .map_err(|queue_error| CliFailure::new(1, queue_error.to_string()))?;
    let Some(_requeued) = requeued else {
        return Err(CliFailure::new(
            1,
            format!("scheduler-deferred worker job {job_id} is no longer running"),
        ));
    };
    Ok(())
}

fn cancellation_is_waiting_for_supervisor(
    state_dir: &Path,
    job_id: &str,
) -> Result<bool, CliFailure> {
    let mut queue =
        Queue::new(state_dir).map_err(|queue_error| CliFailure::new(1, queue_error.to_string()))?;
    Ok(queue
        .get(job_id)
        .map_err(|queue_error| CliFailure::new(1, queue_error.to_string()))?
        .is_some_and(|job| {
            job.status == crate::job::JobStatus::Running && job.cancel_requested_at.is_some()
        }))
}

fn await_supervisor_cancellation() -> ! {
    loop {
        thread::sleep(StdDuration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{
        Job, JobKind, JobStatus, Priority, TargetResult, TargetStatus, ValidationMode,
    };
    use crate::queue_request::{
        QueueRequestStore, QueuedExecutionEnvelope, QueuedShipDispositionKind,
    };
    use crate::ship::ShipExecutionRequest;

    fn completed_ship_fixture(state_dir: &Path) -> (ShipExecutionRequest, Job) {
        let request = ShipExecutionRequest {
            pr: 7751,
            repo: "owner/repo".to_owned(),
            branch: "feature/validated".to_owned(),
            base_branch: "main".to_owned(),
            sha: "a".repeat(40),
            commit_subject: "validated".to_owned(),
            pr_url: None,
            pr_title: None,
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: true,
            fail_fast: false,
            resume_from: None,
            advisory_targets: std::collections::BTreeSet::new(),
            adopt_head: false,
            pr_snapshot_file: None,
            targets: Vec::new(),
        };
        let job = Job::create(
            &request.sha,
            &request.branch,
            vec!["local".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        )
        .with_kind(JobKind::Ship)
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
        QueueRequestStore::new(state_dir)
            .expect("request store")
            .save(&QueuedExecutionEnvelope::from_ship_request(
                &job.id, state_dir, &request,
            ))
            .expect("save request");
        (request, job)
    }

    #[test]
    fn completed_ship_outcome_uses_captured_state_after_active_state_archives() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut job = Job::create(
            "abc",
            "main",
            vec!["local".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        job.id = "captured-terminal".to_owned();
        let old_state = ShipState::new(42, "owner/repo", "main", "main", "old", "policy");
        QueueOutcomeStore::new(temp.path())
            .expect("outcomes")
            .save(&QueuedExecutionOutcome::ship(
                job.id.clone(),
                42,
                old_state,
                true,
            ))
            .expect("old outcome");
        let captured = ShipState::new(42, "owner/repo", "main", "main", "validated", "policy");

        persist_completed_ship_outcome(temp.path(), &job.id, 42, &job, captured.clone(), None)
            .expect("persist captured state");

        let outcome = QueueOutcomeStore::new(temp.path())
            .expect("outcomes")
            .load(&job.id)
            .expect("load")
            .expect("outcome");
        let QueuedExecutionOutcome::Ship {
            ship_state,
            resumed_existing_state,
            ..
        } = outcome
        else {
            panic!("expected ship outcome");
        };
        assert_eq!(ship_state, captured);
        assert!(resumed_existing_state);
    }

    #[test]
    fn missing_active_state_persists_green_job_without_uncertain_reclassification() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (request, job) = completed_ship_fixture(temp.path());

        persist_background_ship_outcome(
            temp.path(),
            &job.id,
            &request,
            &job,
            None,
            QueuedShipDisposition::new(
                QueuedShipDispositionKind::GreenValidationStateMissing,
                crate::app::SHIP_EXIT_VALIDATION_STATE_MISSING,
                Some("recover scoped ship state"),
            ),
        )
        .expect("persist independently of active ship state");

        assert!(job.passed(), "completed validation result remains green");
        let outcome = QueueOutcomeStore::new(temp.path())
            .expect("outcomes")
            .load(&job.id)
            .expect("load")
            .expect("outcome");
        let QueuedExecutionOutcome::Ship {
            ship_state,
            post_validation,
            ..
        } = outcome
        else {
            panic!("expected ship outcome");
        };
        assert_eq!(ship_state.pr, 7751);
        assert_eq!(ship_state.head_sha, request.sha);
        assert_eq!(
            ship_state
                .evidence_snapshot
                .get("local")
                .map(String::as_str),
            Some("pass")
        );
        let post_validation = post_validation.expect("typed post-validation disposition");
        assert_eq!(
            post_validation.kind,
            QueuedShipDispositionKind::GreenValidationStateMissing
        );
        assert_eq!(
            post_validation.exit_code,
            crate::app::SHIP_EXIT_VALIDATION_STATE_MISSING
        );
        assert_eq!(
            post_validation.detail.as_deref(),
            Some("recover scoped ship state")
        );
    }

    #[test]
    fn post_validation_error_never_reclassifies_completed_green_proof() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (request, job) = completed_ship_fixture(temp.path());
        let mut queue = Queue::new(temp.path()).expect("queue");
        queue.enqueue(job.clone()).expect("completed job fixture");

        persist_ship_completion_failure(
            temp.path(),
            &job.id,
            &request,
            &job,
            "post-validation state store unavailable\nretry later",
        )
        .expect("persist separate operational disposition");

        let durable_job = queue.get(&job.id).expect("queue").expect("job");
        assert_eq!(durable_job, job);
        assert!(durable_job.passed());
        let outcome = QueueOutcomeStore::new(temp.path())
            .expect("outcomes")
            .load(&job.id)
            .expect("load")
            .expect("outcome");
        let QueuedExecutionOutcome::Ship {
            ship_state,
            post_validation,
            ..
        } = outcome
        else {
            panic!("expected ship outcome");
        };
        assert_eq!(
            ship_state
                .evidence_snapshot
                .get("local")
                .map(String::as_str),
            Some("pass")
        );
        let post_validation = post_validation.expect("typed operational disposition");
        assert_eq!(
            post_validation.kind,
            QueuedShipDispositionKind::PostValidationOperationalFailure
        );
        assert_eq!(post_validation.exit_code, 1);
        assert_eq!(
            post_validation.detail.as_deref(),
            Some("post-validation state store unavailable retry later")
        );
        let loaded = crate::ship::load_ship_outcome(&mut queue, temp.path(), &job.id)
            .expect("normal outcome API preserves post-validation disposition");
        let loaded_disposition = loaded
            .post_validation
            .expect("typed disposition remains visible to consumers");
        assert_eq!(
            loaded_disposition.kind,
            QueuedShipDispositionKind::PostValidationOperationalFailure
        );
        assert_eq!(loaded_disposition.exit_code, 1);
        assert_eq!(
            loaded_disposition.detail.as_deref(),
            Some("post-validation state store unavailable retry later")
        );
    }

    #[test]
    fn finish_error_preserves_failed_validation_disposition() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (request, passing_job) = completed_ship_fixture(temp.path());
        let failed_job = passing_job.with_result(TargetResult::new(
            "local",
            "macos-arm64",
            TargetStatus::Fail,
            "local",
        ));
        assert!(!failed_job.passed());

        persist_ship_completion_failure(
            temp.path(),
            &failed_job.id,
            &request,
            &failed_job,
            "queued configuration became unavailable",
        )
        .expect("persist failed validation with typed finish error");

        let outcome = QueueOutcomeStore::new(temp.path())
            .expect("outcomes")
            .load(&failed_job.id)
            .expect("load")
            .expect("outcome");
        let QueuedExecutionOutcome::Ship {
            ship_state,
            post_validation,
            ..
        } = outcome
        else {
            panic!("expected ship outcome");
        };
        assert_eq!(
            ship_state
                .evidence_snapshot
                .get("local")
                .map(String::as_str),
            Some("fail")
        );
        let disposition = post_validation.expect("typed validation disposition");
        assert_eq!(
            disposition.kind,
            QueuedShipDispositionKind::ValidationFailed
        );
        assert_eq!(disposition.exit_code, 1);
    }

    #[test]
    fn transient_failed_readiness_snapshot_cannot_overwrite_green_validation_proof() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (request, job) = completed_ship_fixture(temp.path());
        let observed_at = Utc::now();
        let validation_policy =
            crate::ship_state::compute_policy_signature(&[], &job.target_names, "FULL");
        let mut transient = ShipState::new(
            request.pr,
            &request.repo,
            &request.branch,
            &request.base_branch,
            &request.sha,
            &validation_policy,
        );
        transient.update_evidence("local", "fail");
        transient.merge_queue_observed_at = Some(observed_at);

        persist_background_ship_outcome(
            temp.path(),
            &job.id,
            &request,
            &job,
            Some(transient),
            QueuedShipDisposition::new(
                QueuedShipDispositionKind::GreenPendingMergeReadiness,
                0,
                Some("GitHub readiness pending"),
            ),
        )
        .expect("persist immutable completed validation proof");

        let outcome = QueueOutcomeStore::new(temp.path())
            .expect("outcomes")
            .load(&job.id)
            .expect("load")
            .expect("outcome");
        let QueuedExecutionOutcome::Ship { ship_state, .. } = outcome else {
            panic!("expected ship outcome");
        };
        assert!(job.passed());
        assert_eq!(
            ship_state
                .evidence_snapshot
                .get("local")
                .map(String::as_str),
            Some("pass")
        );
        assert!(
            ship_state
                .dispatched_runs
                .iter()
                .all(|run| run.status == "completed")
        );
        assert_eq!(ship_state.merge_queue_observed_at, Some(observed_at));

        let mut newer_head = ShipState::new(
            request.pr,
            &request.repo,
            "feature/newer",
            &request.base_branch,
            "b".repeat(40),
            "newer-policy",
        );
        newer_head.pr_title = "newer head metadata".to_owned();
        newer_head.merge_queue_observed_at = Some(Utc::now());
        persist_background_ship_outcome(
            temp.path(),
            &job.id,
            &request,
            &job,
            Some(newer_head),
            QueuedShipDisposition::new(
                QueuedShipDispositionKind::GreenPendingMergeReadiness,
                0,
                Some("GitHub readiness pending"),
            ),
        )
        .expect("persist request-bound proof across concurrent head reactivation");
        let outcome = QueueOutcomeStore::new(temp.path())
            .expect("outcomes")
            .load(&job.id)
            .expect("load")
            .expect("outcome");
        let QueuedExecutionOutcome::Ship { ship_state, .. } = outcome else {
            panic!("expected ship outcome");
        };
        assert_eq!(ship_state.head_sha, request.sha);
        assert_eq!(ship_state.branch, request.branch);
        assert_eq!(
            ship_state.pr_title,
            request.pr_title.as_deref().unwrap_or_default()
        );
        assert_eq!(ship_state.merge_queue_observed_at, None);
    }

    #[test]
    fn same_head_changed_policy_cannot_misbind_validation_proof() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (request, job) = completed_ship_fixture(temp.path());
        let validation_policy =
            crate::ship_state::compute_policy_signature(&[], &job.target_names, "FULL");
        let mut changed_policy = ShipState::new(
            request.pr,
            &request.repo,
            &request.branch,
            &request.base_branch,
            &request.sha,
            "newer-policy",
        );
        changed_policy.pr_title = "same head, different validation contract".to_owned();
        changed_policy.merge_queue_observed_at = Some(Utc::now());

        persist_background_ship_outcome(
            temp.path(),
            &job.id,
            &request,
            &job,
            Some(changed_policy),
            QueuedShipDisposition::new(
                QueuedShipDispositionKind::GreenPendingMergeReadiness,
                0,
                Some("GitHub readiness pending"),
            ),
        )
        .expect("reject metadata from a different same-head validation contract");

        let outcome = QueueOutcomeStore::new(temp.path())
            .expect("outcomes")
            .load(&job.id)
            .expect("load")
            .expect("outcome");
        let QueuedExecutionOutcome::Ship { ship_state, .. } = outcome else {
            panic!("expected ship outcome");
        };
        assert_eq!(ship_state.policy_signature, validation_policy);
        assert_eq!(
            ship_state.pr_title,
            request.pr_title.as_deref().unwrap_or_default()
        );
        assert_eq!(ship_state.merge_queue_observed_at, None);
    }

    #[test]
    fn scheduler_deferred_worker_returns_to_pending_without_terminal_outcome() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let mut job = Job::create(
            "abc",
            "main",
            vec!["local".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        job.id = "deferred-worker".to_owned();
        queue.enqueue(job).expect("enqueue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        queue
            .start_pending_jobs_for_drain(&lock, &["deferred-worker".to_owned()])
            .expect("start");
        drop(lock);

        requeue_scheduler_deferred_job(
            temp.path(),
            "deferred-worker",
            "host pool temporarily unavailable".to_owned(),
        )
        .expect("requeue");

        let deferred = queue.get("deferred-worker").expect("read").expect("job");
        assert_eq!(deferred.status, JobStatus::Running);
        assert_eq!(deferred.scheduler_defer_count, 1);
        assert_eq!(
            deferred.scheduler_defer_reason.as_deref(),
            Some("host pool temporarily unavailable")
        );
        assert!(deferred.scheduler_defer_until.is_some());
        assert!(deferred.results.is_empty());
    }

    #[test]
    fn cancellation_request_cannot_be_requeued_by_scheduler_deferral() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let mut job = Job::create(
            "abc",
            "main",
            vec!["local".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        job.id = "cancelled-deferred-worker".to_owned();
        queue.enqueue(job).expect("enqueue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        queue
            .start_pending_jobs_for_drain(&lock, &["cancelled-deferred-worker".to_owned()])
            .expect("start");
        drop(lock);
        queue
            .request_cancel(
                "cancelled-deferred-worker",
                Some("operator cancel".to_owned()),
            )
            .expect("request cancel");

        requeue_scheduler_deferred_job(
            temp.path(),
            "cancelled-deferred-worker",
            "capacity unavailable".to_owned(),
        )
        .expect("preserve cancellation for supervisor");

        assert_eq!(
            queue
                .get("cancelled-deferred-worker")
                .expect("read")
                .expect("job")
                .status,
            JobStatus::Running
        );
    }

    #[test]
    fn cancellation_after_daemon_deferral_retains_running_claim_until_supervisor_ack() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let mut job = Job::create(
            "abc",
            "main",
            vec!["local".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        job.id = "deferred-cancel-race".to_owned();
        queue.enqueue(job).expect("enqueue");
        let lock = queue.acquire_drain_lock().expect("lock").expect("owned");
        queue
            .start_pending_jobs_for_drain(&lock, &["deferred-cancel-race".to_owned()])
            .expect("start");
        drop(lock);
        requeue_scheduler_deferred_job(
            temp.path(),
            "deferred-cancel-race",
            "capacity unavailable".to_owned(),
        )
        .expect("defer");
        let requested = queue
            .request_cancel("deferred-cancel-race", Some("operator cancel".to_owned()))
            .expect("request")
            .expect("job");
        assert_eq!(requested.status, JobStatus::Running);
        assert!(requested.cancel_requested_at.is_some());
    }
}
