use std::path::Path;
use std::process::ExitCode;

use super::{CliFailure, ship_cmd::finish_background_ship};
use crate::execution_supervisor::verify_worker_authority;
use crate::identity::RuntimeMode;
use crate::queue::Queue;
use crate::queue_request::{QueueRequestStore, QueuedExecutionKind};
use crate::ship::{execute_started_queued_job, persist_terminal_outcome};

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
                return finish_background_ship(&request, &job, mode, global_dir, state_dir);
            }
            Ok(if job.passed() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Err(error) => {
            let mut queue = Queue::new(state_dir)
                .map_err(|queue_error| CliFailure::new(1, queue_error.to_string()))?;
            if let Some(completed) = queue
                .complete_running_uncertain(job_id, &error.to_string())
                .map_err(|queue_error| CliFailure::new(1, queue_error.to_string()))?
            {
                persist_terminal_outcome(&completed, state_dir)
                    .map_err(|persist_error| CliFailure::new(1, persist_error.to_string()))?;
            }
            Err(CliFailure::new(1, error.to_string()))
        }
    }
}
