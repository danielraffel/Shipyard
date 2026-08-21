use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::Duration as StdDuration;

use chrono::{Duration, Utc};

use super::{CliFailure, ship_cmd::finish_background_ship};
use crate::execution_supervisor::verify_worker_authority;
use crate::identity::RuntimeMode;
use crate::queue::{Queue, QueueDeferredRequeue};
use crate::queue_request::{QueueRequestStore, QueuedExecutionKind};
use crate::ship::{ShipExecutionError, execute_started_queued_job, persist_terminal_outcome};

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
                    .and_then(|code| {
                        // Refresh the typed handoff from the ship state written
                        // by the merge phase; the validation-time copy predates
                        // that terminal disposition.
                        persist_terminal_outcome(&job, state_dir)
                            .map_err(|error| CliFailure::new(1, error.to_string()))?;
                        Ok(code)
                    });
                return match finish {
                    Ok(code) => Ok(code),
                    Err(error) => {
                        let mut queue = Queue::new(state_dir)
                            .map_err(|queue_error| CliFailure::new(1, queue_error.to_string()))?;
                        if let Some(uncertain) = queue
                            .reclassify_completed_uncertain(&job, error.message())
                            .map_err(|queue_error| CliFailure::new(1, queue_error.to_string()))?
                        {
                            persist_terminal_outcome(&uncertain, state_dir).map_err(
                                |persist_error| CliFailure::new(1, persist_error.to_string()),
                            )?;
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
    use crate::job::{Job, JobStatus, Priority, ValidationMode};

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
