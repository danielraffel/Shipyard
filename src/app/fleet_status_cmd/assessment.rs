use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;

use crate::capacity::HostCapacity;
use crate::merge_queue_liveness::{MergeQueueLivenessReport, ReleaseLivenessReport};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum ObservationReason {
    GitHubAuthFailed,
    GitHubRateLimited,
    GitHubObservationFailed,
    ObservationTruncated,
    AuxiliaryObservationUnavailable,
    ReleaseStale,
}

pub(super) struct DoctorProbe {
    pub(super) readable: bool,
    pub(super) source: String,
    pub(super) digest: Option<Value>,
}

pub(super) struct HostFleetStatus {
    pub(super) capacity: HostCapacity,
    pub(super) doctor: DoctorProbe,
    pub(super) supervisor_count: usize,
    pub(super) fresh_supervisor_count: usize,
    pub(super) stale_supervisor_count: usize,
    pub(super) problem_count: usize,
    pub(super) github_runner_count: usize,
    pub(super) stale_vm_count: usize,
    pub(super) routable: bool,
    pub(super) problems: Vec<Value>,
    pub(super) supervisors: Vec<Value>,
}

#[derive(Debug)]
pub(super) struct QueuedSummary {
    pub(super) readable: bool,
    pub(super) source: String,
    pub(super) count: usize,
    pub(super) oldest_age_secs: Option<i64>,
}

pub(super) struct MergeQueueProbe {
    pub(super) readable: bool,
    pub(super) source: String,
    pub(super) report: Option<MergeQueueLivenessReport>,
    pub(super) reason_codes: Vec<ObservationReason>,
}

pub(super) struct ReleaseProbe {
    pub(super) readable: bool,
    pub(super) source: String,
    pub(super) report: Option<ReleaseLivenessReport>,
    pub(super) reason_codes: Vec<ObservationReason>,
}

#[allow(clippy::struct_excessive_bools)]
pub(in crate::app) struct FleetAssessment {
    pub(super) repo: String,
    pub(super) target: String,
    pub(super) free: u32,
    pub(super) routable_free_slots: u32,
    pub(super) capacity_unreadable: bool,
    pub(super) doctor_unreadable: bool,
    pub(super) supervisor_unhealthy: bool,
    pub(super) problem_hosts: bool,
    pub(super) queued_age_threshold_secs: i64,
    pub(super) queue_run_limit: u32,
    pub(super) queued_age_with_capacity: bool,
    pub(super) queue: QueuedSummary,
    pub(super) base: String,
    pub(super) merge_queue_stall_threshold_secs: i64,
    pub(super) merge_queue: MergeQueueProbe,
    pub(super) release_stale_threshold_secs: i64,
    pub(super) release: ReleaseProbe,
    pub(super) hosts: Vec<HostFleetStatus>,
    pub(super) observation_reason_codes: Vec<ObservationReason>,
    pub(super) observation_incomplete: bool,
    pub(super) should_fail: bool,
}

impl FleetAssessment {
    pub(in crate::app) fn exit_code(&self) -> ExitCode {
        if self.should_fail {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        }
    }
}
