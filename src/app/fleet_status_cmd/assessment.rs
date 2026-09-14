use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;

use crate::capacity::HostCapacity;
use crate::fleet_slot::QueuedJobReport;
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
    pub(super) storage: StorageProbe,
    pub(super) storage_problems: Vec<String>,
    pub(super) attestation: AttestationProbe,
    pub(super) attestation_problems: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct StorageProbe {
    pub(super) readable: bool,
    pub(super) source: String,
    pub(super) disk_path: String,
    pub(super) disk_available_kibibyte: Option<u64>,
    pub(super) disk_floor_kibibyte: u64,
    pub(super) ccache_size_kibibyte: Option<u64>,
    pub(super) ccache_max_kibibyte: Option<u64>,
}

/// One persistent runner the host attester reported as broken.
///
/// A dormant plist and a crash loop both read as `broken` in the artifact, so
/// this carries the two fields that separate them. Only a loaded, looping
/// runner is a service fault; an unloaded one is migration residue.
#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct BrokenRunner {
    pub(super) label: String,
    pub(super) loaded: bool,
    pub(super) crash_loop: bool,
    pub(super) runs: u64,
    pub(super) registered: bool,
}

/// A host's self-attestation, read from the artifact its launchd attester writes.
///
/// `readable` is false whenever the probe could not reach a judgement, and
/// `boundary` names why. The two travel together: a boundary is set if and only
/// if the probe is unreadable, so an unreadable attestation can never be folded
/// into a pass.
#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct AttestationProbe {
    pub(super) readable: bool,
    pub(super) source: String,
    pub(super) boundary: Option<String>,
    pub(super) written_at: Option<String>,
    pub(super) age_secs: Option<i64>,
    pub(super) interval_secs: Option<u64>,
    pub(super) launchd_readable: Option<bool>,
    pub(super) writer_sha256: Option<String>,
    pub(super) persistent_runner_count: usize,
    pub(super) jit_lane_count: usize,
    pub(super) broken: Vec<BrokenRunner>,
}

impl AttestationProbe {
    /// The runners that are actually failing service: loaded and looping.
    ///
    /// A runner whose plist is installed but not loaded spawns nothing, so it
    /// cannot be failing to serve. Reporting one as a fault would bury the
    /// single real crash loop under nine dormant entries.
    pub(super) fn crash_looping(&self) -> impl Iterator<Item = &BrokenRunner> {
        self.broken.iter().filter(|r| r.loaded && r.crash_loop)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct RepositoryRunner {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) status: String,
    pub(super) busy: bool,
    pub(super) labels: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct RunnerInventory {
    pub(super) readable: bool,
    pub(super) source: String,
    pub(super) runners: Vec<RepositoryRunner>,
}

#[derive(Clone, Debug)]
pub(super) struct ExpectedHostConfig {
    pub(super) name: String,
    pub(super) active: bool,
    pub(super) min_online: u32,
    pub(super) labels: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ExpectedHostStatus {
    pub(super) name: String,
    pub(super) active: bool,
    pub(super) min_online: u32,
    pub(super) labels: Vec<String>,
    pub(super) matching_runners: Vec<String>,
    pub(super) online: usize,
    pub(super) idle: usize,
    pub(super) problem: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct RoutingMismatch {
    pub(super) run_id: u64,
    pub(super) workflow: String,
    pub(super) job: String,
    pub(super) requested_labels: Vec<String>,
    pub(super) idle_candidates: Vec<String>,
    pub(super) reason: String,
}

/// Result of the wedged-queued-job sweep.
///
/// `examined` is carried alongside the findings on purpose. An empty
/// `raising` list means either "no queued job is wedged" or "no queued job was
/// looked at", and nothing else in the output distinguishes those. Reporting
/// the count the sweep actually reached makes a silent instrument visible
/// instead of reading as a clean pass.
#[derive(Debug, Default)]
pub(super) struct WedgedQueuedJobs {
    /// Queued jobs the sweep classified.
    pub(super) examined: usize,
    /// Those whose verdict raises.
    pub(super) raising: Vec<QueuedJobReport>,
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
    pub(super) runners: RunnerInventory,
    pub(super) expected_hosts: Vec<ExpectedHostStatus>,
    pub(super) routing_mismatches: Vec<RoutingMismatch>,
    pub(super) wedged_queued: WedgedQueuedJobs,
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
