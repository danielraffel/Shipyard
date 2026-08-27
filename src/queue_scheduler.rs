//! Cooperative queue scheduler planning primitives.
//!
//! This module intentionally stops short of starting workers. It provides the
//! host-pool capacity math that the P2b scheduler admission loop will use.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Utc};

use crate::config::LoadedConfig;
use crate::evidence::canonical_repository;
use crate::gh::{self, GhClient};
use crate::host_pool::{HostPoolConfig, HostPoolLease};
use crate::job::{DEFAULT_RUNNING_JOB_STALE_SECONDS, Job, JobStatus};
use crate::queue::{
    DrainLock, Queue, QueueError, QueuePendingCancellation, STALE_RUNNING_CANCEL_REASON,
};
use crate::queue_request::{
    HostPoolDemand, JobResourcePlan, QueueRequestError, QueueRequestStore, QueuedExecutionEnvelope,
    QueuedExecutionRequest, VmSlotDemand,
};

const ALREADY_MERGED_REOBSERVE_INTERVAL: StdDuration = StdDuration::from_secs(30);

/// Standard cancellation reason for pending jobs whose durable request envelope
/// cannot be loaded by the scheduler admit pass.
pub const ORPHANED_PENDING_REQUEST_REASON: &str = "Queued request envelope missing or unreadable";

/// One pending job plus the scheduler-facing request data needed for admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingAdmissionRequest {
    /// Queue job id.
    pub job_id: String,
    /// Persisted resource plan when the request envelope loaded cleanly.
    pub resource_plan: Option<JobResourcePlan>,
    /// Request-envelope load failure reason, when missing or unreadable.
    pub missing_request_reason: Option<String>,
}

impl PendingAdmissionRequest {
    /// Build a loaded pending admission request from a queued execution
    /// envelope.
    #[must_use]
    pub fn loaded(envelope: &QueuedExecutionEnvelope) -> Self {
        Self {
            job_id: envelope.job_id.clone(),
            resource_plan: Some(envelope.resource_plan.clone()),
            missing_request_reason: None,
        }
    }

    /// Build a pending admission request whose durable request envelope is
    /// missing or unreadable.
    #[must_use]
    pub fn missing(job_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            job_id: job_id.into(),
            resource_plan: None,
            missing_request_reason: Some(reason.into()),
        }
    }
}

/// Output of one pure scheduler admit pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AdmitPassPlan {
    /// Pending jobs that can transition to running in this pass.
    pub admitted: Vec<String>,
    /// Pending jobs that remain blocked by resource conflicts or capacity.
    pub deferred: Vec<DeferredAdmission>,
    /// Pending jobs whose durable request envelope is missing or unreadable.
    pub orphaned: Vec<OrphanedPendingJob>,
}

/// One pending job deferred by scheduler admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeferredAdmission {
    /// Queue job id.
    pub job_id: String,
    /// Admission blockers.
    pub blockers: Vec<SchedulerAdmissionBlocker>,
}

/// One pending job whose request envelope could not be used for scheduling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrphanedPendingJob {
    /// Queue job id.
    pub job_id: String,
    /// Cancellation reason to apply through the drain-owned queue primitive.
    pub reason: String,
}

/// Output of a request-store-backed admit pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestBackedAdmitPass {
    /// Pure admit-pass plan.
    pub plan: AdmitPassPlan,
    /// Running jobs whose request envelopes could not be loaded. The scheduler
    /// should avoid starting additional work when this is non-empty.
    pub running_request_errors: Vec<RequestLoadError>,
    /// Same-PR ship decisions for future drain-owned cancellation/defer wiring.
    pub same_pr_ship_admission: SamePrShipAdmission,
    /// Pending jobs whose PR was already merged while they waited in queue.
    pub already_merged_cancellations: Vec<AlreadyMergedCancellation>,
}

/// Durable request envelope load problem observed by the scheduler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestLoadError {
    /// Queue job id.
    pub job_id: String,
    /// Human-readable reason.
    pub reason: String,
}

/// Cancellation for a pending job whose PR was already merged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlreadyMergedCancellation {
    /// Queue job id to cancel.
    pub job_id: String,
    /// Pull request number that was merged.
    pub pr: u64,
}

/// Bounded, repository-aware observer for pending ships whose PR may have
/// merged while the job waited for admission.
pub struct AlreadyMergedObserver {
    client: Option<GhClient>,
    observations: BTreeMap<(String, u64), CachedMergedObservation>,
}

struct CachedMergedObservation {
    observed_at: Instant,
    merged_head: Option<String>,
}

impl AlreadyMergedObserver {
    /// Build an observer from the effective Shipyard GitHub auth configuration.
    /// Invalid auth fails closed: pending jobs remain queued instead of being
    /// cancelled from an unauthenticated or ambiguous observation.
    #[must_use]
    pub fn from_config(config: &LoadedConfig) -> Self {
        Self {
            client: GhClient::from_loaded_config(config).ok(),
            observations: BTreeMap::new(),
        }
    }

    pub(crate) fn observe_pending(
        &mut self,
        jobs: &[Job],
        request_store: &QueueRequestStore,
        cwd: &Path,
        snapshot_file: Option<&Path>,
    ) -> Vec<AlreadyMergedCancellation> {
        let client = self.client.clone();
        self.observe_pending_with(jobs, request_store, |repo, pr| {
            gh::pr_merged_head_sha(client.as_ref(), repo, pr, cwd, snapshot_file)
        })
    }

    pub(crate) fn observe_running(
        &mut self,
        jobs: &[Job],
        request_store: &QueueRequestStore,
        cwd: &Path,
        snapshot_file: Option<&Path>,
    ) -> Vec<AlreadyMergedCancellation> {
        let client = self.client.clone();
        self.observe_running_with(jobs, request_store, |repo, pr| {
            gh::pr_merged_head_sha(client.as_ref(), repo, pr, cwd, snapshot_file)
        })
    }

    pub(crate) fn observe_pending_with(
        &mut self,
        jobs: &[Job],
        request_store: &QueueRequestStore,
        fetch: impl FnMut(&str, u64) -> Option<String>,
    ) -> Vec<AlreadyMergedCancellation> {
        self.observe_ship_with_status(jobs, request_store, JobStatus::Pending, fetch)
    }

    pub(crate) fn observe_running_with(
        &mut self,
        jobs: &[Job],
        request_store: &QueueRequestStore,
        fetch: impl FnMut(&str, u64) -> Option<String>,
    ) -> Vec<AlreadyMergedCancellation> {
        self.observe_ship_with_status(jobs, request_store, JobStatus::Running, fetch)
    }

    fn observe_ship_with_status(
        &mut self,
        jobs: &[Job],
        request_store: &QueueRequestStore,
        status: JobStatus,
        mut fetch: impl FnMut(&str, u64) -> Option<String>,
    ) -> Vec<AlreadyMergedCancellation> {
        let mut jobs_by_pr = BTreeMap::<(String, u64), Vec<(String, String)>>::new();
        for job in jobs.iter().filter(|job| job.status == status) {
            let Some(envelope) = request_store.load(&job.id).ok().flatten() else {
                continue;
            };
            let QueuedExecutionRequest::Ship(request) = envelope.request else {
                continue;
            };
            jobs_by_pr
                .entry((canonical_repository(&request.repo), request.pr))
                .or_default()
                .push((job.id.clone(), request.sha));
        }

        let now = Instant::now();
        let mut cancellations = Vec::new();
        for ((repo, pr), observed_jobs) in jobs_by_pr {
            let cache_key = (repo.clone(), pr);
            let fresh_cached = self.observations.get(&cache_key).filter(|cached| {
                now.saturating_duration_since(cached.observed_at)
                    < ALREADY_MERGED_REOBSERVE_INTERVAL
            });
            let merged_head = if let Some(cached) = fresh_cached {
                cached.merged_head.clone()
            } else {
                let merged_head = fetch(&repo, pr);
                self.observations.insert(
                    cache_key,
                    CachedMergedObservation {
                        observed_at: now,
                        merged_head: merged_head.clone(),
                    },
                );
                merged_head
            };
            let Some(merged_head) = merged_head else {
                continue;
            };
            cancellations.extend(observed_jobs.into_iter().filter_map(
                |(job_id, expected_head)| {
                    (merged_head == expected_head)
                        .then_some(AlreadyMergedCancellation { job_id, pr })
                },
            ));
        }
        cancellations
    }
}

/// Queue mutations applied from one request-backed admit pass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AppliedAdmitPass {
    /// Pending jobs transitioned to running.
    pub started: Vec<Job>,
    /// Pending jobs cancelled before admission.
    pub cancelled: Vec<Job>,
    /// Whether starts were skipped because running request envelopes could not
    /// be loaded.
    pub skipped_starts_due_to_running_request_errors: bool,
    /// Whether starts were skipped because a job planned as stale-running was no
    /// longer stale at apply time (its worker resumed heartbeating). The plan is
    /// then based on a freed claim that is actually still held, so starts are
    /// deferred to the next pass, which replans against fresh liveness.
    pub skipped_starts_due_to_revived_stale_running: bool,
    /// Stale running same-PR ship jobs reaped during this pass.
    pub stale_running_cancelled: Vec<Job>,
}

/// Same-PR `shipyard ship` admission decisions derived from queued request
/// envelopes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SamePrShipAdmission {
    /// Older pending same-PR ship jobs that should be cancelled by the drain
    /// owner.
    pub pending_cancellations: Vec<SamePrShipPendingCancellation>,
    /// Pending same-PR ship jobs blocked by an already-running ship job.
    pub running_conflicts: Vec<SamePrShipRunningConflict>,
    /// Running same-PR ship jobs whose worker has gone silent past the
    /// heartbeat-staleness threshold. They no longer block pending work and
    /// should be reaped by the drain owner.
    pub stale_running_cancellations: Vec<SamePrShipStaleRunningCancellation>,
}

/// Older pending same-PR ship job selected for cancellation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SamePrShipPendingCancellation {
    /// Queue job id to cancel.
    pub job_id: String,
    /// Newer pending job id that supersedes this one.
    pub superseded_by_job_id: String,
    /// Repository slug.
    pub repo: String,
    /// Pull request number.
    pub pr: u64,
    /// Cancellation reason to apply through the drain-owned queue primitive.
    pub reason: String,
}

/// Running same-PR ship job reaped because its worker heartbeat went stale.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SamePrShipStaleRunningCancellation {
    /// Running queue job id to cancel.
    pub job_id: String,
    /// Repository slug.
    pub repo: String,
    /// Pull request number.
    pub pr: u64,
    /// Cancellation reason to apply through the drain-owned queue primitive.
    pub reason: String,
}

/// Pending same-PR ship job blocked by a running ship job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SamePrShipRunningConflict {
    /// Pending queue job id.
    pub pending_job_id: String,
    /// Running queue job id.
    pub running_job_id: String,
    /// Repository slug.
    pub repo: String,
    /// Pull request number.
    pub pr: u64,
    /// Human-readable defer/refusal reason.
    pub reason: String,
}

/// One reason a queued job cannot be admitted beside running jobs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerAdmissionBlocker {
    /// A scheduler-exclusive claim is already held by a running job.
    ExclusiveClaim {
        /// Conflicting claim.
        claim: String,
    },
    /// Host-pool capacity is not available for the candidate.
    HostPoolCapacity(HostPoolCapacityDeficit),
    /// VM-slot capacity is not available for the candidate.
    VmSlotCapacity(VmSlotCapacityDeficit),
}

/// One host-pool capacity deficit that prevents admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostPoolCapacityDeficit {
    /// Pool name.
    pub pool_name: String,
    /// Stable demand capability key.
    pub capability_key: String,
    /// Candidate slots requested for this pool/capability group.
    pub requested_slots: u32,
    /// Slots left after running reservations and active leases.
    pub available_slots: u32,
}

/// Available VM slots for one slot key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmSlotCapacity {
    /// Stable slot key, e.g. `macos`.
    pub key: String,
    /// Slots available to the scheduler before accounting for running queued
    /// jobs and newly admitted jobs in this pass.
    pub slots: u32,
}

/// One VM-slot capacity deficit that prevents admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmSlotCapacityDeficit {
    /// Stable slot key, e.g. `macos`.
    pub key: String,
    /// Candidate slots requested for this slot key.
    pub requested_slots: u32,
    /// Slots left after running reservations.
    pub available_slots: u32,
}

/// Return every blocker for admitting `candidate` alongside currently running
/// resource plans and active host-pool leases.
#[must_use]
pub fn admission_blockers(
    candidate: &JobResourcePlan,
    running: &[&JobResourcePlan],
    pools: &[HostPoolConfig],
    leases: &[HostPoolLease],
    now: DateTime<Utc>,
) -> Vec<SchedulerAdmissionBlocker> {
    admission_blockers_with_vm_slots(candidate, running, pools, leases, &[], now)
}

/// Return every blocker for admitting `candidate`, including VM-slot capacity
/// when a capacity snapshot is supplied.
#[must_use]
pub fn admission_blockers_with_vm_slots(
    candidate: &JobResourcePlan,
    running: &[&JobResourcePlan],
    pools: &[HostPoolConfig],
    leases: &[HostPoolLease],
    vm_slots: &[VmSlotCapacity],
    now: DateTime<Utc>,
) -> Vec<SchedulerAdmissionBlocker> {
    let mut blockers = exclusive_claim_blockers(candidate, running);
    blockers.extend(
        host_pool_capacity_deficits(candidate, running, pools, leases, now)
            .into_iter()
            .map(SchedulerAdmissionBlocker::HostPoolCapacity),
    );
    blockers.extend(
        vm_slot_capacity_deficits(candidate, running, vm_slots)
            .into_iter()
            .map(SchedulerAdmissionBlocker::VmSlotCapacity),
    );
    blockers
}

/// Return true when `candidate` can run beside currently running plans.
#[must_use]
pub fn can_admit(
    candidate: &JobResourcePlan,
    running: &[&JobResourcePlan],
    pools: &[HostPoolConfig],
    leases: &[HostPoolLease],
    now: DateTime<Utc>,
) -> bool {
    admission_blockers_with_vm_slots(candidate, running, pools, leases, &[], now).is_empty()
}

/// Greedily plan one scheduler admission pass without mutating queue state.
///
/// `pending` must already be sorted in queue order. This function admits each
/// compatible job into an in-memory occupied set before evaluating later
/// pending jobs, matching the scheduler loop's greedy behavior.
#[must_use]
pub fn plan_admit_pass(
    pending: &[PendingAdmissionRequest],
    running: &[JobResourcePlan],
    pools: &[HostPoolConfig],
    leases: &[HostPoolLease],
    now: DateTime<Utc>,
) -> AdmitPassPlan {
    plan_admit_pass_with_vm_slots(pending, running, pools, leases, &[], now)
}

/// Greedily plan one scheduler admission pass, including VM-slot capacity when
/// a capacity snapshot is supplied.
#[must_use]
pub fn plan_admit_pass_with_vm_slots(
    pending: &[PendingAdmissionRequest],
    running: &[JobResourcePlan],
    pools: &[HostPoolConfig],
    leases: &[HostPoolLease],
    vm_slots: &[VmSlotCapacity],
    now: DateTime<Utc>,
) -> AdmitPassPlan {
    let mut plan = AdmitPassPlan::default();
    let mut occupied = running.to_vec();
    for candidate in pending {
        let Some(resource_plan) = candidate.resource_plan.as_ref() else {
            plan.orphaned.push(OrphanedPendingJob {
                job_id: candidate.job_id.clone(),
                reason: orphan_reason(candidate.missing_request_reason.as_deref()),
            });
            continue;
        };
        let occupied_refs = occupied.iter().collect::<Vec<_>>();
        let blockers = admission_blockers_with_vm_slots(
            resource_plan,
            &occupied_refs,
            pools,
            leases,
            vm_slots,
            now,
        );
        if blockers.is_empty() {
            plan.admitted.push(candidate.job_id.clone());
            occupied.push(resource_plan.clone());
        } else {
            plan.deferred.push(DeferredAdmission {
                job_id: candidate.job_id.clone(),
                blockers,
            });
        }
    }
    plan
}

/// Load request envelopes for queue jobs and run a pure scheduler admit pass.
///
/// Pending jobs with missing or unreadable request envelopes are reported as
/// orphaned pending jobs. Running jobs with missing or unreadable request
/// envelopes are surfaced separately so the future drain loop can avoid
/// admitting new work when occupied resources are unknown.
#[must_use]
pub fn plan_admit_pass_from_jobs(
    jobs: &[Job],
    request_store: &QueueRequestStore,
    pools: &[HostPoolConfig],
    leases: &[HostPoolLease],
    now: DateTime<Utc>,
) -> RequestBackedAdmitPass {
    plan_admit_pass_from_jobs_with_vm_slots(jobs, request_store, pools, leases, &[], now)
}

/// Load request envelopes for queue jobs and run a pure scheduler admit pass,
/// including VM-slot capacity when a capacity snapshot is supplied.
#[must_use]
pub fn plan_admit_pass_from_jobs_with_vm_slots(
    jobs: &[Job],
    request_store: &QueueRequestStore,
    pools: &[HostPoolConfig],
    leases: &[HostPoolLease],
    vm_slots: &[VmSlotCapacity],
    now: DateTime<Utc>,
) -> RequestBackedAdmitPass {
    let stale_after = chrono::Duration::seconds(DEFAULT_RUNNING_JOB_STALE_SECONDS);
    let same_pr_ship_admission = same_pr_ship_admission(jobs, request_store, now, stale_after);
    let same_pr_excluded = same_pr_ship_admission
        .pending_cancellations
        .iter()
        .map(|cancellation| cancellation.job_id.as_str())
        .chain(
            same_pr_ship_admission
                .running_conflicts
                .iter()
                .map(|conflict| conflict.pending_job_id.as_str()),
        )
        .collect::<BTreeSet<_>>();
    let pending = sorted_pending_jobs(jobs)
        .iter()
        .filter(|job| !same_pr_excluded.contains(job.id.as_str()))
        .filter(|job| {
            job.scheduler_defer_until
                .is_none_or(|defer_until| defer_until <= now)
        })
        .map(|job| pending_admission_request(job, request_store))
        .collect::<Vec<_>>();
    // Only the stale running jobs this pass will actually reap (dead same-PR
    // ship workers) are dropped from resource accounting — their claims are
    // released in the apply pass, so they must not reserve capacity here too.
    // Any other stale running job stays in accounting; its claims keep blocking
    // until startup recovery clears it, which is the conservative, pre-existing
    // behavior. Keeping the exclusion set equal to the reap set avoids freeing a
    // claim for a job that is never cancelled.
    let stale_reaped = same_pr_ship_admission
        .stale_running_cancellations
        .iter()
        .map(|cancellation| cancellation.job_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut running = Vec::new();
    let mut running_request_errors = Vec::new();
    for job in jobs
        .iter()
        .filter(|job| job.status == JobStatus::Running && !stale_reaped.contains(job.id.as_str()))
    {
        match load_resource_plan(&job.id, request_store) {
            Ok(Some(plan)) => running.push(plan),
            Ok(None) => running_request_errors.push(RequestLoadError {
                job_id: job.id.clone(),
                reason: ORPHANED_PENDING_REQUEST_REASON.to_owned(),
            }),
            Err(error) => running_request_errors.push(RequestLoadError {
                job_id: job.id.clone(),
                reason: format!("{ORPHANED_PENDING_REQUEST_REASON}: {error}"),
            }),
        }
    }
    RequestBackedAdmitPass {
        plan: plan_admit_pass_with_vm_slots(&pending, &running, pools, leases, vm_slots, now),
        running_request_errors,
        same_pr_ship_admission,
        already_merged_cancellations: Vec::new(),
    }
}

/// Apply the queue mutations from a request-backed admit pass.
///
/// This does not spawn workers. It only performs drain-owned queue state
/// transitions that are safe before worker ownership exists: cancelling
/// orphaned/superseded pending jobs and transitioning admitted jobs to running.
pub fn apply_admit_pass_for_drain(
    queue: &mut Queue,
    drain_lock: &DrainLock,
    pass: &RequestBackedAdmitPass,
) -> Result<AppliedAdmitPass, QueueError> {
    let cancellations = admit_pass_cancellations(pass);
    let cancelled = queue.cancel_pending_jobs_for_drain(drain_lock, &cancellations)?;
    // Reap stale running same-PR jobs (dead workers). `cancel_stale_running_jobs`
    // re-checks staleness under the state lock with a fresh `now`, so a worker
    // that resumed heartbeating between planning and apply is never reaped.
    let stale_running_ids = pass
        .same_pr_ship_admission
        .stale_running_cancellations
        .iter()
        .map(|cancellation| cancellation.job_id.clone())
        .collect::<Vec<_>>();
    let stale_running_cancelled = queue.cancel_stale_running_jobs(
        &stale_running_ids,
        Utc::now(),
        chrono::Duration::seconds(DEFAULT_RUNNING_JOB_STALE_SECONDS),
        STALE_RUNNING_CANCEL_REASON,
    )?;
    // If a job planned as stale-running was not actually reaped, its worker
    // resumed heartbeating between planning and apply. The plan freed that
    // worker's claim and may have admitted a conflicting same-PR job on the
    // strength of it, so starting now could double-run. Defer starts to the next
    // pass, which replans against the revived worker's fresh liveness.
    let skipped_starts_due_to_revived_stale_running =
        stale_running_cancelled.len() < stale_running_ids.len();
    let skipped_starts_due_to_running_request_errors = !pass.running_request_errors.is_empty();
    let started = if skipped_starts_due_to_running_request_errors
        || skipped_starts_due_to_revived_stale_running
    {
        Vec::new()
    } else {
        queue.start_pending_jobs_for_drain(drain_lock, &pass.plan.admitted)?
    };
    Ok(AppliedAdmitPass {
        started,
        cancelled,
        skipped_starts_due_to_running_request_errors,
        skipped_starts_due_to_revived_stale_running,
        stale_running_cancelled,
    })
}

fn admit_pass_cancellations(pass: &RequestBackedAdmitPass) -> Vec<QueuePendingCancellation> {
    pass.plan
        .orphaned
        .iter()
        .map(|orphan| QueuePendingCancellation {
            job_id: orphan.job_id.clone(),
            reason: orphan.reason.clone(),
        })
        .chain(
            pass.same_pr_ship_admission
                .pending_cancellations
                .iter()
                .map(|cancellation| QueuePendingCancellation {
                    job_id: cancellation.job_id.clone(),
                    reason: cancellation.reason.clone(),
                }),
        )
        .chain(
            pass.already_merged_cancellations
                .iter()
                .map(|cancellation| QueuePendingCancellation {
                    job_id: cancellation.job_id.clone(),
                    reason: crate::queue::ALREADY_MERGED_CANCEL_REASON.to_owned(),
                }),
        )
        .collect()
}

/// Return host-pool deficits for admitting `candidate` alongside already
/// running resource plans and active, non-stale host-pool leases.
#[must_use]
pub fn host_pool_capacity_deficits(
    candidate: &JobResourcePlan,
    running: &[&JobResourcePlan],
    pools: &[HostPoolConfig],
    leases: &[HostPoolLease],
    now: DateTime<Utc>,
) -> Vec<HostPoolCapacityDeficit> {
    let pool_map = pools
        .iter()
        .map(|pool| (pool.name.as_str(), pool))
        .collect::<BTreeMap<_, _>>();
    let mut deficits = Vec::new();
    for demand in combined_demands(&candidate.host_pools) {
        let capacity = available_slots_for(&demand, &pool_map, running, leases, now);
        if demand.slots > capacity {
            deficits.push(HostPoolCapacityDeficit {
                pool_name: demand.pool_name,
                capability_key: demand.capability_key,
                requested_slots: demand.slots,
                available_slots: capacity,
            });
        }
    }
    deficits
}

/// Return VM-slot deficits for admitting `candidate` alongside already running
/// resource plans. An empty capacity snapshot disables VM-slot gating for
/// backwards compatibility with projects that have not configured Tart slots.
#[must_use]
pub fn vm_slot_capacity_deficits(
    candidate: &JobResourcePlan,
    running: &[&JobResourcePlan],
    capacities: &[VmSlotCapacity],
) -> Vec<VmSlotCapacityDeficit> {
    if capacities.is_empty() || candidate.vm_slots.is_empty() {
        return Vec::new();
    }
    let capacity = capacities
        .iter()
        .map(|slot| (slot.key.as_str(), slot.slots))
        .collect::<BTreeMap<_, _>>();
    let mut deficits = Vec::new();
    for demand in combined_vm_demands(&candidate.vm_slots) {
        let available = capacity
            .get(demand.key.as_str())
            .copied()
            .unwrap_or(0)
            .saturating_sub(running_vm_reservations(running, &demand.key));
        if demand.slots > available {
            deficits.push(VmSlotCapacityDeficit {
                key: demand.key,
                requested_slots: demand.slots,
                available_slots: available,
            });
        }
    }
    deficits
}

fn exclusive_claim_blockers(
    candidate: &JobResourcePlan,
    running: &[&JobResourcePlan],
) -> Vec<SchedulerAdmissionBlocker> {
    let occupied = running
        .iter()
        .flat_map(|plan| plan.exclusive_claims.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    let mut conflicts = candidate
        .exclusive_claims
        .iter()
        .filter(|claim| occupied.contains(claim.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    conflicts.sort();
    conflicts.dedup();
    conflicts
        .into_iter()
        .map(|claim| SchedulerAdmissionBlocker::ExclusiveClaim { claim })
        .collect()
}

fn pending_admission_request(
    job: &Job,
    request_store: &QueueRequestStore,
) -> PendingAdmissionRequest {
    match load_resource_plan(&job.id, request_store) {
        Ok(Some(resource_plan)) => PendingAdmissionRequest {
            job_id: job.id.clone(),
            resource_plan: Some(resource_plan),
            missing_request_reason: None,
        },
        Ok(None) => PendingAdmissionRequest::missing(&job.id, ORPHANED_PENDING_REQUEST_REASON),
        Err(error) => PendingAdmissionRequest::missing(&job.id, error.to_string()),
    }
}

fn load_resource_plan(
    job_id: &str,
    request_store: &QueueRequestStore,
) -> Result<Option<JobResourcePlan>, QueueRequestError> {
    let Some(envelope) = request_store.load(job_id)? else {
        return Ok(None);
    };
    if envelope.job_id != job_id {
        return Err(QueueRequestError::InvalidSnapshot {
            reason: format!(
                "queued execution request for {job_id} belongs to {}",
                envelope.job_id
            ),
        });
    }
    Ok(Some(envelope.resource_plan))
}

fn same_pr_ship_admission(
    jobs: &[Job],
    request_store: &QueueRequestStore,
    now: DateTime<Utc>,
    stale_after: chrono::Duration,
) -> SamePrShipAdmission {
    let mut by_pr = BTreeMap::<(String, u64), SamePrShipGroup>::new();
    for job in jobs
        .iter()
        .filter(|job| matches!(job.status, JobStatus::Pending | JobStatus::Running))
    {
        let Some((ship_key, foreground_owned)) = load_ship_key(&job.id, request_store) else {
            continue;
        };
        let group = by_pr.entry(ship_key).or_default();
        match job.status {
            JobStatus::Pending => group.pending.push(job.clone()),
            // A running job only blocks pending same-PR work while its worker is
            // alive. One whose heartbeat has gone stale was abandoned (e.g. the
            // process was killed); set it aside for reaping so it never blocks
            // forever.
            JobStatus::Running if foreground_owned && job.is_stale_running(now, stale_after) => {
                group.stale_running.push(job.clone());
            }
            JobStatus::Running => group.running.push(job.clone()),
            _ => {}
        }
    }

    let mut admission = SamePrShipAdmission::default();
    for ((repo, pr), mut group) in by_pr {
        group.running.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        group.pending.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        for stale in &group.stale_running {
            admission
                .stale_running_cancellations
                .push(SamePrShipStaleRunningCancellation {
                    job_id: stale.id.clone(),
                    repo: repo.clone(),
                    pr,
                    reason: STALE_RUNNING_CANCEL_REASON.to_owned(),
                });
        }
        if let Some(running) = group.running.first() {
            admission
                .running_conflicts
                .extend(
                    group
                        .pending
                        .iter()
                        .map(|pending| SamePrShipRunningConflict {
                            pending_job_id: pending.id.clone(),
                            running_job_id: running.id.clone(),
                            repo: repo.clone(),
                            pr,
                            reason: same_pr_running_reason(&repo, pr, &running.id),
                        }),
                );
            continue;
        }
        let Some(newest) = group.pending.last() else {
            continue;
        };
        admission
            .pending_cancellations
            .extend(group.pending.iter().rev().skip(1).map(|pending| {
                SamePrShipPendingCancellation {
                    job_id: pending.id.clone(),
                    superseded_by_job_id: newest.id.clone(),
                    repo: repo.clone(),
                    pr,
                    reason: same_pr_superseded_reason(&repo, pr, &newest.id),
                }
            }));
    }
    admission
}

#[derive(Default)]
struct SamePrShipGroup {
    pending: Vec<Job>,
    running: Vec<Job>,
    stale_running: Vec<Job>,
}

fn load_ship_key(job_id: &str, request_store: &QueueRequestStore) -> Option<((String, u64), bool)> {
    let envelope = request_store.load(job_id).ok().flatten()?;
    if envelope.job_id != job_id {
        return None;
    }
    let foreground_owned = envelope.is_foreground_owned();
    match envelope.request {
        QueuedExecutionRequest::Ship(request) => Some((
            (canonical_repository(&request.repo), request.pr),
            foreground_owned,
        )),
        QueuedExecutionRequest::Run(_) => None,
    }
}

fn same_pr_superseded_reason(repo: &str, pr: u64, newer_job_id: &str) -> String {
    format!("Superseded by newer queued ship for {repo}#{pr} ({newer_job_id})")
}

fn same_pr_running_reason(repo: &str, pr: u64, running_job_id: &str) -> String {
    format!("Same-PR ship already running for {repo}#{pr} ({running_job_id})")
}

fn sorted_pending_jobs(jobs: &[Job]) -> Vec<Job> {
    let mut pending = jobs
        .iter()
        .filter(|job| job.status == JobStatus::Pending)
        .cloned()
        .collect::<Vec<_>>();
    pending.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.created_at.cmp(&right.created_at))
    });
    pending
}

fn orphan_reason(detail: Option<&str>) -> String {
    let Some(detail) = detail.filter(|detail| !detail.trim().is_empty()) else {
        return ORPHANED_PENDING_REQUEST_REASON.to_owned();
    };
    if detail == ORPHANED_PENDING_REQUEST_REASON {
        return ORPHANED_PENDING_REQUEST_REASON.to_owned();
    }
    format!("{ORPHANED_PENDING_REQUEST_REASON}: {detail}")
}

fn available_slots_for(
    candidate: &HostPoolDemand,
    pools: &BTreeMap<&str, &HostPoolConfig>,
    running: &[&JobResourcePlan],
    leases: &[HostPoolLease],
    now: DateTime<Utc>,
) -> u32 {
    let Some(pool) = pools.get(candidate.pool_name.as_str()) else {
        return 0;
    };
    let eligible_members = eligible_member_ids(pool, &candidate.requires);
    let total = pool
        .members
        .iter()
        .filter(|member| eligible_members.contains(member.id.as_str()))
        .map(|member| member.max_concurrency)
        .sum::<u32>();
    let active_leases = leases
        .iter()
        .filter(|lease| {
            lease.pool_name == candidate.pool_name
                && eligible_members.contains(lease.member_id.as_str())
                && !lease.is_stale(now)
        })
        .count()
        .try_into()
        .unwrap_or(u32::MAX);
    let running_reservations = running
        .iter()
        .flat_map(|plan| plan.host_pools.iter())
        .filter(|demand| demand.pool_name == candidate.pool_name)
        .filter(|demand| {
            let running_members = eligible_member_ids(pool, &demand.requires);
            !running_members.is_disjoint(&eligible_members)
        })
        .map(|demand| demand.slots)
        .sum::<u32>();

    total
        .saturating_sub(active_leases)
        .saturating_sub(running_reservations)
}

fn eligible_member_ids<'a>(pool: &'a HostPoolConfig, requires: &[String]) -> BTreeSet<&'a str> {
    pool.members
        .iter()
        .filter(|member| {
            requires.iter().all(|required| {
                member
                    .capabilities
                    .iter()
                    .any(|capability| capability == required)
            })
        })
        .map(|member| member.id.as_str())
        .collect()
}

fn combined_demands(demands: &[HostPoolDemand]) -> Vec<HostPoolDemand> {
    let mut combined = Vec::<HostPoolDemand>::new();
    for demand in demands {
        if let Some(existing) = combined.iter_mut().find(|existing| {
            existing.pool_name == demand.pool_name
                && existing.capability_key == demand.capability_key
                && existing.requires == demand.requires
        }) {
            existing.slots = existing.slots.saturating_add(demand.slots);
            continue;
        }
        combined.push(demand.clone());
    }
    combined
}

fn combined_vm_demands(demands: &[VmSlotDemand]) -> Vec<VmSlotDemand> {
    let mut combined = Vec::<VmSlotDemand>::new();
    for demand in demands {
        if let Some(existing) = combined
            .iter_mut()
            .find(|existing| existing.key == demand.key)
        {
            existing.slots = existing.slots.saturating_add(demand.slots);
            continue;
        }
        combined.push(demand.clone());
    }
    combined
}

fn running_vm_reservations(running: &[&JobResourcePlan], key: &str) -> u32 {
    running
        .iter()
        .flat_map(|plan| plan.vm_slots.iter())
        .filter(|demand| demand.key == key)
        .map(|demand| demand.slots)
        .sum()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use chrono::{Duration, Utc};

    use super::{
        AlreadyMergedObserver, ORPHANED_PENDING_REQUEST_REASON, PendingAdmissionRequest,
        SchedulerAdmissionBlocker, apply_admit_pass_for_drain, can_admit,
        host_pool_capacity_deficits, plan_admit_pass, plan_admit_pass_from_jobs,
    };
    use crate::executor::dispatch::{
        ResolvedBackend, ResolvedHostPoolConfig, ResolvedTarget, ResolvedValidation,
    };
    use crate::host_pool::{HostPoolConfig, HostPoolLease, HostPoolMemberConfig};
    use crate::job::{Job, JobStatus, Priority, ValidationMode};
    use crate::queue::Queue;
    use crate::queue_request::{
        HostPoolDemand, JobResourcePlan, QUEUED_EXECUTION_SCHEMA_VERSION, QueueRequestStore,
        QueuedExecutionEnvelope, QueuedExecutionKind, QueuedExecutionRequest, QueuedRunRequest,
        QueuedShipRequest, VmSlotDemand,
    };
    use crate::ship::ShipExecutionRequest;

    fn pool() -> HostPoolConfig {
        HostPoolConfig {
            name: "local_macs".to_owned(),
            strategy: "ordered".to_owned(),
            lease_stale_seconds: 180,
            heartbeat_interval_seconds: 15,
            members: vec![
                HostPoolMemberConfig {
                    id: "mac-a".to_owned(),
                    backend_type: "ssh".to_owned(),
                    host: Some("mac-a".to_owned()),
                    repo_path: Some("/repo".to_owned()),
                    cwd: None,
                    max_concurrency: 1,
                    capabilities: vec!["macos".to_owned(), "arm64".to_owned()],
                },
                HostPoolMemberConfig {
                    id: "mac-b".to_owned(),
                    backend_type: "ssh".to_owned(),
                    host: Some("mac-b".to_owned()),
                    repo_path: Some("/repo".to_owned()),
                    cwd: None,
                    max_concurrency: 1,
                    capabilities: vec!["macos".to_owned(), "arm64".to_owned()],
                },
            ],
        }
    }

    fn plan(slots: u32) -> JobResourcePlan {
        JobResourcePlan {
            targets: Vec::new(),
            exclusive_claims: Vec::new(),
            cloud_targets: Vec::new(),
            host_pools: vec![HostPoolDemand {
                pool_name: "local_macs".to_owned(),
                requires: vec!["arm64".to_owned(), "macos".to_owned()],
                slots,
                capability_key: "arm64+macos".to_owned(),
            }],
            vm_slots: Vec::new(),
        }
    }

    fn fleet_pool() -> HostPoolConfig {
        HostPoolConfig {
            name: "local_macs".to_owned(),
            strategy: "ordered".to_owned(),
            lease_stale_seconds: 180,
            heartbeat_interval_seconds: 15,
            members: vec![
                HostPoolMemberConfig {
                    id: "m3".to_owned(),
                    backend_type: "ssh".to_owned(),
                    host: Some("m3".to_owned()),
                    repo_path: Some("/repo".to_owned()),
                    cwd: None,
                    max_concurrency: 1,
                    capabilities: vec!["arm64".to_owned(), "pulp-full".to_owned()],
                },
                HostPoolMemberConfig {
                    id: "m1".to_owned(),
                    backend_type: "ssh".to_owned(),
                    host: Some("m1".to_owned()),
                    repo_path: Some("/repo".to_owned()),
                    cwd: None,
                    max_concurrency: 1,
                    capabilities: vec!["arm64".to_owned(), "forge-modular".to_owned()],
                },
                HostPoolMemberConfig {
                    id: "m5".to_owned(),
                    backend_type: "ssh".to_owned(),
                    host: Some("m5".to_owned()),
                    repo_path: Some("/repo".to_owned()),
                    cwd: None,
                    max_concurrency: 1,
                    capabilities: vec!["arm64".to_owned(), "forge-sequencer".to_owned()],
                },
                HostPoolMemberConfig {
                    id: "controller".to_owned(),
                    backend_type: "local".to_owned(),
                    host: None,
                    repo_path: None,
                    cwd: Some(PathBuf::from("/repo")),
                    max_concurrency: 1,
                    capabilities: vec!["arm64".to_owned(), "vellum".to_owned()],
                },
            ],
        }
    }

    fn capability_ship_envelope(
        job_id: &str,
        repository: &str,
        pr: u64,
        branch: &str,
        capability: &str,
    ) -> QueuedExecutionEnvelope {
        let target = ResolvedTarget {
            name: capability.to_owned(),
            validation_build_type: None,
            platform: "macos-arm64".to_owned(),
            backend_name: "host-pool".to_owned(),
            warm_keepalive_seconds: 0,
            host: None,
            backend: ResolvedBackend::HostPool(ResolvedHostPoolConfig {
                pool_name: "local_macs".to_owned(),
                strategy: "ordered".to_owned(),
                lease_stale_seconds: 180,
                heartbeat_interval_seconds: 15,
                requires: vec!["arm64".to_owned(), capability.to_owned()],
                members: Vec::new(),
            }),
            validation: ResolvedValidation::HostPool,
            failure_parser: None,
        };
        let request = ShipExecutionRequest {
            pr,
            repo: repository.to_owned(),
            branch: branch.to_owned(),
            base_branch: "main".to_owned(),
            sha: format!("sha-{job_id}"),
            commit_subject: String::new(),
            pr_url: None,
            pr_title: None,
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            advisory_targets: BTreeSet::new(),
            adopt_head: false,
            pr_snapshot_file: None,
            metadata_authority_receipt: None,
            targets: vec![target],
        };
        QueuedExecutionEnvelope::from_ship_request(job_id, "/repo", &request)
    }

    fn claim_plan(claims: &[&str]) -> JobResourcePlan {
        JobResourcePlan {
            targets: Vec::new(),
            exclusive_claims: claims.iter().map(|claim| (*claim).to_owned()).collect(),
            cloud_targets: Vec::new(),
            host_pools: Vec::new(),
            vm_slots: Vec::new(),
        }
    }

    fn cloud_plan(target: &str) -> JobResourcePlan {
        JobResourcePlan {
            targets: vec![target.to_owned()],
            exclusive_claims: Vec::new(),
            cloud_targets: vec![target.to_owned()],
            host_pools: Vec::new(),
            vm_slots: Vec::new(),
        }
    }

    fn vm_plan(key: &str, slots: u32) -> JobResourcePlan {
        JobResourcePlan {
            targets: Vec::new(),
            exclusive_claims: Vec::new(),
            cloud_targets: Vec::new(),
            host_pools: Vec::new(),
            vm_slots: vec![VmSlotDemand {
                key: key.to_owned(),
                slots,
            }],
        }
    }

    fn lease(member_id: &str, expires_at: chrono::DateTime<Utc>) -> HostPoolLease {
        HostPoolLease {
            lease_id: format!("lease-{member_id}"),
            pool_name: "local_macs".to_owned(),
            member_id: member_id.to_owned(),
            target_name: "mac".to_owned(),
            backend: "ssh".to_owned(),
            host: Some(member_id.to_owned()),
            job_id: Some("job-running".to_owned()),
            branch: "main".to_owned(),
            sha: "abc123".to_owned(),
            owner_pid: 123,
            acquired_at: Utc::now(),
            heartbeat_at: Utc::now(),
            expires_at,
        }
    }

    fn job(id: &str, status: JobStatus, priority: Priority, created_offset_secs: i64) -> Job {
        Job {
            id: id.to_owned(),
            sha: "abc123".to_owned(),
            branch: "main".to_owned(),
            mode: ValidationMode::Full,
            kind: None,
            workload_scope: None,
            target_names: vec!["mac".to_owned()],
            priority,
            status,
            created_at: Utc::now() + Duration::seconds(created_offset_secs),
            started_at: None,
            completed_at: None,
            cancellation_reason: None,
            cancel_requested_at: None,
            scheduler_defer_reason: None,
            scheduler_defer_count: 0,
            scheduler_defer_until: None,
            resource_claims: Vec::new(),
            results: BTreeMap::new(),
        }
    }

    fn save_plan(store: &QueueRequestStore, job_id: &str, resource_plan: JobResourcePlan) {
        store
            .save(&QueuedExecutionEnvelope {
                schema_version: QUEUED_EXECUTION_SCHEMA_VERSION,
                job_id: job_id.to_owned(),
                kind: QueuedExecutionKind::Run,
                cwd: PathBuf::from("/repo"),
                created_at: Utc::now(),
                execution_owner: crate::queue_request::QueuedExecutionOwner::LegacyUnspecified,
                provenance: None,
                resource_plan,
                request: QueuedExecutionRequest::Run(QueuedRunRequest {
                    branch: "main".to_owned(),
                    sha: "abc123".to_owned(),
                    mode: ValidationMode::Full,
                    priority: Priority::Normal,
                    warm_disabled: false,
                    fail_fast: false,
                    resume_from: None,
                    targets: Vec::new(),
                }),
            })
            .expect("save request");
    }

    fn save_ship(store: &QueueRequestStore, job_id: &str, repo: &str, pr: u64) {
        store
            .save(&QueuedExecutionEnvelope {
                schema_version: QUEUED_EXECUTION_SCHEMA_VERSION,
                job_id: job_id.to_owned(),
                kind: QueuedExecutionKind::Ship,
                cwd: PathBuf::from("/repo"),
                created_at: Utc::now(),
                execution_owner: crate::queue_request::QueuedExecutionOwner::LegacyUnspecified,
                provenance: None,
                resource_plan: claim_plan(&[&format!("ship-state:{repo}:pr-{pr}")]),
                request: QueuedExecutionRequest::Ship(QueuedShipRequest {
                    pr,
                    repo: repo.to_owned(),
                    branch: "feature".to_owned(),
                    base_branch: "main".to_owned(),
                    sha: "abc123".to_owned(),
                    commit_subject: "subject".to_owned(),
                    pr_url: None,
                    pr_title: None,
                    mode: ValidationMode::Full,
                    priority: Priority::Normal,
                    warm_disabled: false,
                    fail_fast: false,
                    resume_from: None,
                    advisory_targets: std::collections::BTreeSet::default(),
                    adopt_head: false,
                    metadata_authority_receipt: None,
                    targets: Vec::new(),
                }),
            })
            .expect("save ship request");
    }

    #[test]
    fn host_pool_capacity_allows_second_job_when_two_members_exist() {
        let now = Utc::now();
        let running = plan(1);
        let candidate = plan(1);

        let deficits = host_pool_capacity_deficits(&candidate, &[&running], &[pool()], &[], now);

        assert!(deficits.is_empty());
    }

    #[test]
    fn host_pool_capacity_blocks_when_running_reservations_exhaust_members() {
        let now = Utc::now();
        let running = plan(2);
        let candidate = plan(1);

        let deficits = host_pool_capacity_deficits(&candidate, &[&running], &[pool()], &[], now);

        assert_eq!(deficits.len(), 1);
        assert_eq!(deficits[0].available_slots, 0);
        assert_eq!(deficits[0].requested_slots, 1);
    }

    #[test]
    fn host_pool_capacity_counts_only_non_stale_leases() {
        let now = Utc::now();
        let candidate = plan(1);
        let leases = vec![
            lease("mac-a", now + Duration::seconds(60)),
            lease("mac-b", now - Duration::seconds(1)),
        ];

        let deficits = host_pool_capacity_deficits(&candidate, &[], &[pool()], &leases, now);

        assert!(deficits.is_empty());
    }

    #[test]
    fn host_pool_capacity_reports_missing_pool_as_deficit() {
        let now = Utc::now();
        let candidate = plan(1);

        let deficits = host_pool_capacity_deficits(&candidate, &[], &[], &[], now);

        assert_eq!(deficits.len(), 1);
        assert_eq!(deficits[0].pool_name, "local_macs");
        assert_eq!(deficits[0].available_slots, 0);
    }

    #[test]
    fn admission_blocks_same_local_cwd() {
        let now = Utc::now();
        let running = claim_plan(&["local-cwd:/repo"]);
        let candidate = claim_plan(&["local-cwd:/repo"]);

        let blockers = super::admission_blockers(&candidate, &[&running], &[], &[], now);

        assert_eq!(
            blockers,
            [SchedulerAdmissionBlocker::ExclusiveClaim {
                claim: "local-cwd:/repo".to_owned(),
            }]
        );
        assert!(!can_admit(&candidate, &[&running], &[], &[], now));
    }

    #[test]
    fn admission_blocks_same_remote_and_ship_state_claims() {
        let now = Utc::now();
        let running = claim_plan(&[
            "ssh-repo:mac:/repo",
            r"ssh-windows-repo:win:C:\repo",
            "ship-state:danielraffel/shipyard:pr-42",
        ]);

        for claim in [
            "ssh-repo:mac:/repo",
            r"ssh-windows-repo:win:C:\repo",
            "ship-state:danielraffel/shipyard:pr-42",
        ] {
            let candidate = claim_plan(&[claim]);
            assert!(
                !can_admit(&candidate, &[&running], &[], &[], now),
                "{claim}"
            );
        }
    }

    #[test]
    fn admission_allows_unrelated_cloud_jobs() {
        let now = Utc::now();
        let running = cloud_plan("linux");
        let candidate = cloud_plan("windows");

        assert!(can_admit(&candidate, &[&running], &[], &[], now));
    }

    #[test]
    fn admission_allows_two_host_pool_jobs_when_two_members_exist() {
        let now = Utc::now();
        let running = plan(1);
        let candidate = plan(1);

        assert!(can_admit(&candidate, &[&running], &[pool()], &[], now));
    }

    #[test]
    fn admission_blocks_host_pool_when_capacity_exhausted() {
        let now = Utc::now();
        let running = plan(2);
        let candidate = plan(1);

        let blockers = super::admission_blockers(&candidate, &[&running], &[pool()], &[], now);

        assert!(matches!(
            blockers.as_slice(),
            [SchedulerAdmissionBlocker::HostPoolCapacity(deficit)]
                if deficit.pool_name == "local_macs" && deficit.available_slots == 0
        ));
    }

    #[test]
    fn admission_blocks_macos_vm_slot_when_capacity_exhausted() {
        let now = Utc::now();
        let running = vm_plan("macos", 2);
        let candidate = vm_plan("macos", 1);
        let capacities = [super::VmSlotCapacity {
            key: "macos".to_owned(),
            slots: 2,
        }];

        let blockers = super::admission_blockers_with_vm_slots(
            &candidate,
            &[&running],
            &[],
            &[],
            &capacities,
            now,
        );

        assert!(matches!(
            blockers.as_slice(),
            [SchedulerAdmissionBlocker::VmSlotCapacity(deficit)]
                if deficit.key == "macos" && deficit.available_slots == 0
        ));
    }

    #[test]
    fn admit_pass_vm_slots_defer_macos_but_not_linux() {
        let now = Utc::now();
        let capacities = [super::VmSlotCapacity {
            key: "macos".to_owned(),
            slots: 1,
        }];
        let first = PendingAdmissionRequest {
            job_id: "mac-a".to_owned(),
            resource_plan: Some(vm_plan("macos", 1)),
            missing_request_reason: None,
        };
        let second = PendingAdmissionRequest {
            job_id: "mac-b".to_owned(),
            resource_plan: Some(vm_plan("macos", 1)),
            missing_request_reason: None,
        };
        let linux = PendingAdmissionRequest {
            job_id: "linux".to_owned(),
            resource_plan: Some(cloud_plan("linux")),
            missing_request_reason: None,
        };

        let plan = super::plan_admit_pass_with_vm_slots(
            &[first, second, linux],
            &[],
            &[],
            &[],
            &capacities,
            now,
        );

        assert_eq!(plan.admitted, ["mac-a", "linux"]);
        assert_eq!(plan.deferred.len(), 1);
        assert_eq!(plan.deferred[0].job_id, "mac-b");
        assert!(matches!(
            plan.deferred[0].blockers.as_slice(),
            [SchedulerAdmissionBlocker::VmSlotCapacity(deficit)]
                if deficit.key == "macos" && deficit.available_slots == 0
        ));
    }

    #[test]
    fn admission_does_not_serialize_against_unclaimed_fallback_secondary() {
        let now = Utc::now();
        let running = claim_plan(&["ssh-repo:mac-b:/repo-b"]);
        let candidate = claim_plan(&["ssh-repo:mac-a:/repo-a"]);

        assert!(can_admit(&candidate, &[&running], &[], &[], now));
    }

    #[test]
    fn admit_pass_greedily_admits_and_defers_against_newly_admitted_jobs() {
        let now = Utc::now();
        let first = PendingAdmissionRequest {
            job_id: "job-a".to_owned(),
            resource_plan: Some(claim_plan(&["local-cwd:/repo"])),
            missing_request_reason: None,
        };
        let second = PendingAdmissionRequest {
            job_id: "job-b".to_owned(),
            resource_plan: Some(claim_plan(&["local-cwd:/repo"])),
            missing_request_reason: None,
        };
        let third = PendingAdmissionRequest {
            job_id: "job-c".to_owned(),
            resource_plan: Some(claim_plan(&["local-cwd:/other"])),
            missing_request_reason: None,
        };

        let plan = plan_admit_pass(&[first, second, third], &[], &[], &[], now);

        assert_eq!(plan.admitted, ["job-a", "job-c"]);
        assert_eq!(plan.deferred.len(), 1);
        assert_eq!(plan.deferred[0].job_id, "job-b");
        assert_eq!(
            plan.deferred[0].blockers,
            [SchedulerAdmissionBlocker::ExclusiveClaim {
                claim: "local-cwd:/repo".to_owned(),
            }]
        );
        assert!(plan.orphaned.is_empty());
    }

    #[test]
    fn admit_pass_reports_missing_request_envelopes_as_orphaned() {
        let now = Utc::now();
        let missing = PendingAdmissionRequest::missing("job-missing", "No such file");

        let plan = plan_admit_pass(&[missing], &[], &[], &[], now);

        assert!(plan.admitted.is_empty());
        assert!(plan.deferred.is_empty());
        assert_eq!(plan.orphaned.len(), 1);
        assert_eq!(plan.orphaned[0].job_id, "job-missing");
        assert_eq!(
            plan.orphaned[0].reason,
            format!("{ORPHANED_PENDING_REQUEST_REASON}: No such file")
        );
    }

    #[test]
    fn request_backed_admit_pass_waits_until_scheduler_deferral_expires() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("request store");
        let now = Utc::now();
        let mut deferred = job("deferred", JobStatus::Pending, Priority::High, -1);
        deferred.scheduler_defer_until = Some(now + Duration::seconds(5));
        let ready = job("ready", JobStatus::Pending, Priority::Normal, 0);
        save_plan(&store, &deferred.id, cloud_plan("deferred"));
        save_plan(&store, &ready.id, cloud_plan("ready"));

        let waiting =
            plan_admit_pass_from_jobs(&[deferred.clone(), ready.clone()], &store, &[], &[], now);
        assert_eq!(waiting.plan.admitted, std::slice::from_ref(&ready.id));

        let expired = plan_admit_pass_from_jobs(
            &[deferred, ready],
            &store,
            &[],
            &[],
            now + Duration::seconds(5),
        );
        assert_eq!(expired.plan.admitted, ["deferred", "ready"]);
    }

    #[test]
    fn admit_pass_defers_host_pool_capacity_and_allows_later_independent_job() {
        let now = Utc::now();
        let running = plan(2);
        let host_pool_candidate = PendingAdmissionRequest {
            job_id: "job-pool".to_owned(),
            resource_plan: Some(plan(1)),
            missing_request_reason: None,
        };
        let cloud_candidate = PendingAdmissionRequest {
            job_id: "job-cloud".to_owned(),
            resource_plan: Some(cloud_plan("linux")),
            missing_request_reason: None,
        };

        let plan = plan_admit_pass(
            &[host_pool_candidate, cloud_candidate],
            &[running],
            &[pool()],
            &[],
            now,
        );

        assert_eq!(plan.admitted, ["job-cloud"]);
        assert_eq!(plan.deferred.len(), 1);
        assert_eq!(plan.deferred[0].job_id, "job-pool");
        assert!(matches!(
            plan.deferred[0].blockers.as_slice(),
            [SchedulerAdmissionBlocker::HostPoolCapacity(deficit)]
                if deficit.pool_name == "local_macs"
        ));
    }

    #[test]
    fn blocked_pulp_proof_does_not_head_of_line_block_forge_products_or_vellum() {
        let now = Utc::now();
        let running_pulp = capability_ship_envelope(
            "pulp-pr-7718",
            "Generous-Corp/pulp",
            7718,
            "feature/shared",
            "pulp-full",
        );
        let blocked_pulp = capability_ship_envelope(
            "pulp-pr-7730",
            "Generous-Corp/pulp",
            7730,
            "feature/other",
            "pulp-full",
        );
        let forge_modular = capability_ship_envelope(
            "forge-modular-pr-127",
            "Generous-Corp/forge",
            127,
            "feature/shared",
            "forge-modular",
        );
        let forge_sequencer = capability_ship_envelope(
            "forge-sequencer-pr-128",
            "Generous-Corp/forge",
            128,
            "feature/shared",
            "forge-sequencer",
        );
        let vellum = capability_ship_envelope(
            "vellum-pr-96",
            "Generous-Corp/vellum",
            96,
            "feature/shared",
            "vellum",
        );

        let pending = [
            PendingAdmissionRequest::loaded(&blocked_pulp),
            PendingAdmissionRequest::loaded(&forge_modular),
            PendingAdmissionRequest::loaded(&forge_sequencer),
            PendingAdmissionRequest::loaded(&vellum),
        ];

        let plan = plan_admit_pass(
            &pending,
            &[running_pulp.resource_plan],
            &[fleet_pool()],
            &[],
            now,
        );

        assert_eq!(
            plan.admitted,
            [
                "forge-modular-pr-127",
                "forge-sequencer-pr-128",
                "vellum-pr-96",
            ]
        );
        assert_eq!(plan.deferred.len(), 1);
        assert_eq!(plan.deferred[0].job_id, "pulp-pr-7730");
        assert!(
            matches!(
                plan.deferred[0].blockers.as_slice(),
                [SchedulerAdmissionBlocker::HostPoolCapacity(deficit)]
                    if deficit.capability_key == "arm64+pulp-full"
                        && deficit.available_slots == 0
            ),
            "unexpected blockers: {:?}",
            plan.deferred[0].blockers
        );
    }

    #[test]
    fn request_backed_admit_pass_loads_requests_and_sorts_pending() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        save_plan(&store, "low", claim_plan(&["local-cwd:/low"]));
        save_plan(&store, "high-old", claim_plan(&["local-cwd:/high-old"]));
        save_plan(&store, "high-new", claim_plan(&["local-cwd:/high-new"]));
        let jobs = vec![
            job("low", JobStatus::Pending, Priority::Low, -30),
            job("high-new", JobStatus::Pending, Priority::High, -10),
            job("high-old", JobStatus::Pending, Priority::High, -20),
        ];

        let pass = plan_admit_pass_from_jobs(&jobs, &store, &[], &[], Utc::now());

        assert_eq!(pass.plan.admitted, ["high-old", "high-new", "low"]);
        assert!(pass.plan.deferred.is_empty());
        assert!(pass.plan.orphaned.is_empty());
        assert!(pass.running_request_errors.is_empty());
    }

    #[test]
    fn request_backed_admit_pass_reports_missing_pending_request() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let jobs = vec![job("missing", JobStatus::Pending, Priority::Normal, 0)];

        let pass = plan_admit_pass_from_jobs(&jobs, &store, &[], &[], Utc::now());

        assert!(pass.plan.admitted.is_empty());
        assert_eq!(pass.plan.orphaned.len(), 1);
        assert_eq!(pass.plan.orphaned[0].job_id, "missing");
        assert_eq!(
            pass.plan.orphaned[0].reason,
            ORPHANED_PENDING_REQUEST_REASON
        );
    }

    #[test]
    fn request_backed_admit_pass_reports_missing_running_request() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        save_plan(&store, "pending", claim_plan(&["local-cwd:/pending"]));
        let jobs = vec![
            job("running", JobStatus::Running, Priority::Normal, -10),
            job("pending", JobStatus::Pending, Priority::Normal, 0),
        ];

        let pass = plan_admit_pass_from_jobs(&jobs, &store, &[], &[], Utc::now());

        assert_eq!(pass.running_request_errors.len(), 1);
        assert_eq!(pass.running_request_errors[0].job_id, "running");
        assert_eq!(
            pass.running_request_errors[0].reason,
            ORPHANED_PENDING_REQUEST_REASON
        );
        assert_eq!(pass.plan.admitted, ["pending"]);
    }

    #[test]
    fn request_backed_admit_pass_reports_older_pending_same_pr_ship_cancellations() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        save_ship(&store, "older", "danielraffel/shipyard", 42);
        save_ship(&store, "newer", "danielraffel/shipyard", 42);
        save_ship(&store, "other", "danielraffel/shipyard", 43);
        let jobs = vec![
            job("older", JobStatus::Pending, Priority::Normal, -20),
            job("newer", JobStatus::Pending, Priority::Normal, -10),
            job("other", JobStatus::Pending, Priority::Normal, -5),
        ];

        let pass = plan_admit_pass_from_jobs(&jobs, &store, &[], &[], Utc::now());

        assert_eq!(pass.plan.admitted, ["newer", "other"]);
        assert_eq!(
            pass.same_pr_ship_admission.pending_cancellations,
            [super::SamePrShipPendingCancellation {
                job_id: "older".to_owned(),
                superseded_by_job_id: "newer".to_owned(),
                repo: "danielraffel/shipyard".to_owned(),
                pr: 42,
                reason: "Superseded by newer queued ship for danielraffel/shipyard#42 (newer)"
                    .to_owned(),
            }]
        );
        assert!(pass.same_pr_ship_admission.running_conflicts.is_empty());
    }

    #[test]
    fn request_backed_admit_pass_reports_pending_same_pr_ship_running_conflict() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        save_ship(&store, "running", "danielraffel/shipyard", 42);
        save_ship(&store, "pending", "danielraffel/shipyard", 42);
        save_ship(&store, "other", "danielraffel/shipyard", 43);
        let jobs = vec![
            job("running", JobStatus::Running, Priority::Normal, -20),
            job("pending", JobStatus::Pending, Priority::Normal, -10),
            job("other", JobStatus::Pending, Priority::Normal, -5),
        ];

        let pass = plan_admit_pass_from_jobs(&jobs, &store, &[], &[], Utc::now());

        assert_eq!(pass.plan.admitted, ["other"]);
        assert_eq!(
            pass.same_pr_ship_admission.running_conflicts,
            [super::SamePrShipRunningConflict {
                pending_job_id: "pending".to_owned(),
                running_job_id: "running".to_owned(),
                repo: "danielraffel/shipyard".to_owned(),
                pr: 42,
                reason: "Same-PR ship already running for danielraffel/shipyard#42 (running)"
                    .to_owned(),
            }]
        );
        assert!(pass.same_pr_ship_admission.pending_cancellations.is_empty());
    }

    #[test]
    fn request_backed_admit_pass_reaps_stale_same_pr_running_and_admits_pending() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        save_ship(&store, "stale-running", "danielraffel/shipyard", 42);
        save_ship(&store, "pending", "danielraffel/shipyard", 42);
        let mut stale = job("stale-running", JobStatus::Running, Priority::Normal, -20);
        stale.started_at =
            Some(Utc::now() - Duration::seconds(super::DEFAULT_RUNNING_JOB_STALE_SECONDS + 60));
        let jobs = vec![
            stale,
            job("pending", JobStatus::Pending, Priority::Normal, -10),
        ];

        let pass = plan_admit_pass_from_jobs(&jobs, &store, &[], &[], Utc::now());

        // The stale running ship no longer blocks: the pending same-PR ship is
        // admitted, no running conflict is reported, and the dead worker is
        // surfaced for reaping (and so does not hold its pr-42 claim).
        assert_eq!(pass.plan.admitted, ["pending"]);
        assert!(pass.same_pr_ship_admission.running_conflicts.is_empty());
        assert_eq!(
            pass.same_pr_ship_admission.stale_running_cancellations,
            [super::SamePrShipStaleRunningCancellation {
                job_id: "stale-running".to_owned(),
                repo: "danielraffel/shipyard".to_owned(),
                pr: 42,
                reason: super::STALE_RUNNING_CANCEL_REASON.to_owned(),
            }]
        );
    }

    #[test]
    fn request_backed_admit_pass_preserves_stale_daemon_owned_same_pr_running() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        save_ship(&store, "stale-running", "danielraffel/shipyard", 42);
        let mut daemon = store.load("stale-running").expect("load").expect("request");
        daemon.execution_owner = crate::queue_request::QueuedExecutionOwner::Daemon;
        store.save(&daemon).expect("save daemon owner");
        save_ship(&store, "pending", "danielraffel/shipyard", 42);
        let mut stale = job("stale-running", JobStatus::Running, Priority::Normal, -20);
        stale.started_at =
            Some(Utc::now() - Duration::seconds(super::DEFAULT_RUNNING_JOB_STALE_SECONDS + 60));
        let jobs = vec![
            stale,
            job("pending", JobStatus::Pending, Priority::Normal, -10),
        ];

        let pass = plan_admit_pass_from_jobs(&jobs, &store, &[], &[], Utc::now());

        assert!(pass.plan.admitted.is_empty());
        assert!(
            pass.same_pr_ship_admission
                .stale_running_cancellations
                .is_empty()
        );
        assert_eq!(pass.same_pr_ship_admission.running_conflicts.len(), 1);
    }

    #[test]
    fn request_backed_admit_pass_fails_closed_for_mismatched_running_envelope() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        save_plan(&store, "running", claim_plan(&["host:m1"]));
        let mut mismatched = store.load("running").expect("load").expect("request");
        mismatched.job_id = "different-job".to_owned();
        std::fs::write(
            store.path_for("running"),
            serde_json::to_vec(&mismatched).expect("encode"),
        )
        .expect("write mismatch");
        save_plan(&store, "pending", claim_plan(&["host:m1"]));
        let jobs = vec![
            job("running", JobStatus::Running, Priority::Normal, -20),
            job("pending", JobStatus::Pending, Priority::Normal, -10),
        ];

        let pass = plan_admit_pass_from_jobs(&jobs, &store, &[], &[], Utc::now());

        assert_eq!(pass.plan.admitted, ["pending"]);
        assert_eq!(pass.running_request_errors.len(), 1);
        assert_eq!(pass.running_request_errors[0].job_id, "running");
    }

    #[test]
    fn apply_admit_pass_reaps_stale_running_and_starts_admitted_pending() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("queue")).expect("queue");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        save_ship(&store, "stale-running", "danielraffel/shipyard", 42);
        save_ship(&store, "pending", "danielraffel/shipyard", 42);

        let mut stale = job("stale-running", JobStatus::Running, Priority::Normal, -20);
        stale.started_at =
            Some(Utc::now() - Duration::seconds(super::DEFAULT_RUNNING_JOB_STALE_SECONDS + 60));
        let pending = job("pending", JobStatus::Pending, Priority::Normal, -10);
        queue.enqueue(stale).expect("enqueue stale");
        queue.enqueue(pending).expect("enqueue pending");

        let drain = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");
        let jobs = queue.get_all().expect("jobs");
        let pass = plan_admit_pass_from_jobs(&jobs, &store, &[], &[], Utc::now());
        let applied = apply_admit_pass_for_drain(&mut queue, &drain, &pass).expect("apply");

        // The dead worker's job is reaped to Cancelled, and the pending same-PR
        // ship it was blocking is started.
        assert_eq!(applied.stale_running_cancelled.len(), 1);
        assert_eq!(applied.stale_running_cancelled[0].id, "stale-running");
        assert_eq!(
            applied
                .started
                .iter()
                .map(|job| job.id.as_str())
                .collect::<Vec<_>>(),
            ["pending"]
        );
        assert_eq!(
            queue
                .get("stale-running")
                .expect("get")
                .expect("job")
                .status,
            JobStatus::Cancelled
        );
        assert_eq!(
            queue.get("pending").expect("get").expect("job").status,
            JobStatus::Running
        );
    }

    #[test]
    fn apply_admit_pass_defers_starts_when_planned_stale_running_revived() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("queue")).expect("queue");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        save_ship(&store, "running", "danielraffel/shipyard", 42);
        save_ship(&store, "pending", "danielraffel/shipyard", 42);

        let mut stale = job("running", JobStatus::Running, Priority::Normal, -20);
        stale.started_at =
            Some(Utc::now() - Duration::seconds(super::DEFAULT_RUNNING_JOB_STALE_SECONDS + 60));
        let pending = job("pending", JobStatus::Pending, Priority::Normal, -10);
        queue.enqueue(stale).expect("enqueue running");
        queue.enqueue(pending).expect("enqueue pending");

        // Plan while the worker looks stale: pending is admitted, the running job
        // is queued for reaping.
        let jobs = queue.get_all().expect("jobs");
        let pass = plan_admit_pass_from_jobs(&jobs, &store, &[], &[], Utc::now());
        assert_eq!(pass.plan.admitted, ["pending"]);
        assert_eq!(
            pass.same_pr_ship_admission
                .stale_running_cancellations
                .len(),
            1
        );

        // The worker resumes heartbeating before apply (it was only quiet, not
        // dead).
        let mut revived = queue.get("running").expect("get").expect("job");
        revived.started_at = Some(Utc::now());
        queue.update(&revived).expect("revive");

        let drain = queue
            .acquire_drain_lock()
            .expect("drain lock")
            .expect("available");
        let applied = apply_admit_pass_for_drain(&mut queue, &drain, &pass).expect("apply");

        // The revived worker is not reaped, and the conflicting pending start is
        // deferred to the next pass rather than double-running the PR.
        assert!(applied.stale_running_cancelled.is_empty());
        assert!(applied.skipped_starts_due_to_revived_stale_running);
        assert!(applied.started.is_empty());
        assert_eq!(
            queue.get("running").expect("get").expect("job").status,
            JobStatus::Running
        );
        assert_eq!(
            queue.get("pending").expect("get").expect("job").status,
            JobStatus::Pending
        );
    }

    #[test]
    fn apply_admit_pass_cancels_orphans_and_same_pr_then_starts_admitted_jobs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("queue")).expect("queue");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let mut admitted = job("admitted", JobStatus::Pending, Priority::Normal, -30);
        admitted.target_names = vec!["admitted".to_owned()];
        let mut orphan = job("orphan", JobStatus::Pending, Priority::Normal, -20);
        orphan.target_names = vec!["orphan".to_owned()];
        let mut older_ship = job("older-ship", JobStatus::Pending, Priority::Normal, -10);
        older_ship.target_names = vec!["older-ship".to_owned()];
        let mut newer_ship = job("newer-ship", JobStatus::Pending, Priority::Normal, 0);
        newer_ship.target_names = vec!["newer-ship".to_owned()];

        for queued in [
            admitted.clone(),
            orphan.clone(),
            older_ship.clone(),
            newer_ship.clone(),
        ] {
            queue.enqueue(queued).expect("enqueue");
        }
        save_plan(&store, "admitted", claim_plan(&["local-cwd:/admitted"]));
        save_ship(&store, "older-ship", "danielraffel/shipyard", 42);
        save_ship(&store, "newer-ship", "danielraffel/shipyard", 42);
        let pass = plan_admit_pass_from_jobs(
            &[admitted, orphan, older_ship, newer_ship],
            &store,
            &[],
            &[],
            Utc::now(),
        );
        let lock = queue.acquire_drain_lock().expect("lock").expect("held");

        let applied = apply_admit_pass_for_drain(&mut queue, &lock, &pass).expect("apply");

        assert_eq!(
            applied
                .started
                .iter()
                .map(|job| job.id.as_str())
                .collect::<Vec<_>>(),
            ["admitted", "newer-ship"]
        );
        assert_eq!(
            applied
                .cancelled
                .iter()
                .map(|job| job.id.as_str())
                .collect::<Vec<_>>(),
            ["orphan", "older-ship"]
        );
        assert!(!applied.skipped_starts_due_to_running_request_errors);
        assert_eq!(
            queue
                .get("admitted")
                .expect("admitted")
                .expect("job")
                .status,
            JobStatus::Running
        );
        assert_eq!(
            queue.get("orphan").expect("orphan").expect("job").status,
            JobStatus::Cancelled
        );
        assert_eq!(
            queue.get("older-ship").expect("older").expect("job").status,
            JobStatus::Cancelled
        );
        assert_eq!(
            queue.get("newer-ship").expect("newer").expect("job").status,
            JobStatus::Running
        );
    }

    #[test]
    fn apply_admit_pass_skips_starts_when_running_request_envelopes_are_unknown() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut queue = Queue::new(temp.path().join("queue")).expect("queue");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        let running = job("running", JobStatus::Running, Priority::Normal, -20);
        let pending = job("pending", JobStatus::Pending, Priority::Normal, -10);

        queue.enqueue(running.clone()).expect("running");
        queue.enqueue(pending.clone()).expect("pending");
        save_plan(&store, "pending", claim_plan(&["local-cwd:/pending"]));
        let pass = plan_admit_pass_from_jobs(&[running, pending], &store, &[], &[], Utc::now());
        assert_eq!(pass.plan.admitted, ["pending"]);
        assert_eq!(pass.running_request_errors.len(), 1);
        let lock = queue.acquire_drain_lock().expect("lock").expect("held");

        let applied = apply_admit_pass_for_drain(&mut queue, &lock, &pass).expect("apply");

        assert!(applied.started.is_empty());
        assert!(applied.cancelled.is_empty());
        assert!(applied.skipped_starts_due_to_running_request_errors);
        assert_eq!(
            queue.get("pending").expect("pending").expect("job").status,
            JobStatus::Pending
        );
    }

    /// Positive test: a pending ship job whose PR is observed as merged gets an
    /// `already_merged` cancellation in the admit pass.
    #[test]
    fn observe_already_merged_cancels_pending_ship_job() {
        // Create a temporary "merged" snapshot file. The `headRefOid` matches
        // the `sha` recorded by `save_ship` ("abc123"), so the head-SHA guard
        // is satisfied and the jobs are cancelled.
        let merged_snapshot = tempfile::NamedTempFile::new().expect("temp");
        std::fs::write(
            merged_snapshot.path(),
            r#"{"state":"MERGED","headRefOid":"abc123"}"#,
        )
        .expect("write snapshot");

        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");

        // Enqueue two ship jobs — same repo, different PRs.
        save_ship(&store, "one", "Generous-Corp/pulp", 99);
        save_ship(&store, "two", "Generous-Corp/pulp", 100);
        let one = job("one", JobStatus::Pending, Priority::Normal, -10);
        let two = job("two", JobStatus::Pending, Priority::Normal, -5);

        let mut observer = AlreadyMergedObserver {
            client: None,
            observations: BTreeMap::new(),
        };
        let jobs = [one, two];
        let cancellations = observer.observe_pending(
            &jobs,
            &store,
            merged_snapshot.path(),
            Some(merged_snapshot.path()),
        );

        // Both jobs' PRs report merged, so both should appear.
        assert_eq!(cancellations.len(), 2);
        assert!(
            cancellations
                .iter()
                .any(|c| c.job_id == "one" && c.pr == 99)
        );
        assert!(
            cancellations
                .iter()
                .any(|c| c.job_id == "two" && c.pr == 100)
        );
    }

    /// Negative test: when the snapshot file says OPEN (not merged), no
    /// `already_merged` cancellations are produced.
    #[test]
    fn observe_already_merged_does_not_cancel_open_pr() {
        let open_snapshot = tempfile::NamedTempFile::new().expect("temp");
        std::fs::write(open_snapshot.path(), r#"{"state":"OPEN"}"#).expect("write snapshot");

        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");

        save_ship(&store, "open-pr", "Generous-Corp/pulp", 42);
        let job = job("open-pr", JobStatus::Pending, Priority::Normal, 0);

        let mut observer = AlreadyMergedObserver {
            client: None,
            observations: BTreeMap::new(),
        };
        let cancellations = observer.observe_pending(
            &[job],
            &store,
            open_snapshot.path(),
            Some(open_snapshot.path()),
        );

        assert!(cancellations.is_empty());
    }

    /// Head-SHA guard: when the PR is merged but at a DIFFERENT head than the
    /// job was queued to validate, no cancellation is produced (fail closed).
    #[test]
    fn observe_already_merged_does_not_cancel_when_merged_head_differs() {
        // Merged, but `headRefOid` differs from `save_ship`'s sha ("abc123").
        let mismatched_snapshot = tempfile::NamedTempFile::new().expect("temp");
        std::fs::write(
            mismatched_snapshot.path(),
            r#"{"state":"MERGED","headRefOid":"deadbeef"}"#,
        )
        .expect("write snapshot");

        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");

        save_ship(&store, "drifted-pr", "Generous-Corp/pulp", 7);
        let job = job("drifted-pr", JobStatus::Pending, Priority::Normal, 0);

        let mut observer = AlreadyMergedObserver {
            client: None,
            observations: BTreeMap::new(),
        };
        let cancellations = observer.observe_pending(
            &[job],
            &store,
            mismatched_snapshot.path(),
            Some(mismatched_snapshot.path()),
        );

        assert!(cancellations.is_empty());
    }

    #[test]
    fn observe_already_merged_deduplicates_by_repository_and_pr_and_throttles_rechecks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = QueueRequestStore::new(temp.path()).expect("store");
        save_ship(&store, "one", "owner/one", 42);
        save_ship(&store, "two", "owner/one", 42);
        save_ship(&store, "other-repo", "owner/two", 42);
        let jobs = [
            job("one", JobStatus::Pending, Priority::Normal, -10),
            job("two", JobStatus::Pending, Priority::Normal, -5),
            job("other-repo", JobStatus::Pending, Priority::Normal, 0),
        ];
        let mut observer = AlreadyMergedObserver {
            client: None,
            observations: BTreeMap::new(),
        };
        let mut calls = Vec::new();

        let first = observer.observe_pending_with(&jobs, &store, |repo, pr| {
            calls.push((repo.to_owned(), pr));
            Some("abc123".to_owned())
        });
        let second = observer.observe_pending_with(&jobs, &store, |repo, pr| {
            calls.push((repo.to_owned(), pr));
            Some("abc123".to_owned())
        });

        assert_eq!(first.len(), 3);
        assert_eq!(second.len(), 3);
        assert_eq!(
            calls,
            [("owner/one".to_owned(), 42), ("owner/two".to_owned(), 42)]
        );
    }
}
