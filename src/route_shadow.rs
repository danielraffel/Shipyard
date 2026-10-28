//! Shadow-only route and capability observations.
//!
//! This module consumes an immutable observation of the routes that an
//! authoritative scheduler would already consider. It does not probe hosts,
//! acquire leases, reorder candidates, or dispatch work. The configured
//! candidate order therefore remains the authority; the receipt only records
//! what each later candidate would have done if earlier candidates were not
//! available.

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::executor::dispatch::{ResolvedBackend, ResolvedTarget};
use crate::host_pool::HostPoolLease;
use crate::job::RouteAttempt;

/// Schema version for [`RouteCapabilitySnapshot`] and [`RouteShadowReceipt`].
pub const ROUTE_SHADOW_SCHEMA_VERSION: u32 = 1;
const OBSERVATION_QUEUE_CAPACITY: usize = 64;
/// Bound the number of per-request receipts retained by the shadow sink.
/// Retention runs only on the background writer, so admission never scans or
/// deletes receipt files.
const RECEIPT_RETENTION_LIMIT: usize = 256;

struct PendingObservation {
    state_dir: PathBuf,
    observation: RouteShadowObservation,
}

static OBSERVATION_SENDER: OnceLock<Option<SyncSender<PendingObservation>>> = OnceLock::new();

/// One immutable route/capability observation supplied to the shadow pass.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteCapabilitySnapshot {
    /// Schema version of this snapshot.
    pub schema_version: u32,
    /// Stable request identity supplied by the queue owner.
    pub request_id: String,
    /// Logical target being evaluated.
    pub target_name: String,
    /// Capabilities required by the target artifact.
    pub required_capabilities: Vec<String>,
    /// Candidates in authoritative configured order. This vector is never
    /// sorted by the shadow evaluator.
    pub candidates: Vec<RouteCandidateSnapshot>,
}

/// One route candidate observed without performing a probe or dispatch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteCandidateSnapshot {
    /// Stable route/member identity.
    pub route_id: String,
    /// Environment where execution would occur (for example `macos-arm64` or
    /// `github-hosted-macos`).
    pub execution_environment: String,
    /// Artifact capability the environment claims to provide. Keeping this
    /// separate from the execution environment leaves room for cross-build
    /// hosts without changing the route schema.
    pub artifact_capability: String,
    /// Capabilities observed for this candidate.
    pub capabilities: Vec<String>,
    /// Admission state already observed by the authoritative scheduler.
    pub availability: RouteAvailability,
}

/// Admission state captured in a route snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteAvailability {
    /// Candidate can accept the request now.
    Available,
    /// Candidate is configured but currently occupied.
    Busy,
    /// Candidate could not be reached by the authoritative observer.
    Unreachable,
    /// The observer could not establish a trustworthy state.
    Unknown,
}

/// Why a route was or was not admitted in the shadow comparison.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteCounterfactualOutcome {
    /// This route is the first available candidate in configured order.
    Authoritative,
    /// This route would be selected if all earlier candidates were unavailable.
    WouldAdmitIfEarlierUnavailable,
    /// The route lacks one or more required capabilities.
    CapabilityMismatch {
        /// Required capabilities absent from this route.
        missing: Vec<String>,
    },
    /// The route has the capabilities but is not currently admissible.
    Unavailable {
        /// Observed admission state.
        availability: RouteAvailability,
    },
}

/// Typed observation of the authoritative route and its counterfactuals.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteShadowReceipt {
    /// Schema version of this receipt.
    pub schema_version: u32,
    /// Request identity copied from the snapshot.
    pub request_id: String,
    /// Logical target copied from the snapshot.
    pub target_name: String,
    /// Canonical required capability set (sorted and deduplicated).
    pub required_capabilities: Vec<String>,
    /// Selected route, if any candidate was admissible.
    pub authoritative_route: Option<RouteSelection>,
    /// One typed outcome for every candidate, preserving configured order.
    pub counterfactuals: Vec<RouteCounterfactual>,
    /// Evaluator scope and ordering authority; this is not a claim about
    /// actions taken by the caller that supplied the snapshot.
    pub control: RouteShadowControl,
    /// Runtime provenance for an organic scheduler observation. Legacy
    /// receipt-only callers may leave this absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<RouteShadowObservationMetadata>,
}

/// Freshness and capability provenance attached to an organic observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteShadowObservationMetadata {
    /// Stable job/target correlation identity.
    pub correlation_id: String,
    /// Time at which the scheduler facts were read.
    pub observed_at: DateTime<Utc>,
    /// Latest time at which those facts may be consumed as fresh.
    pub expires_at: DateTime<Utc>,
    /// Source label for capability and admission facts. This is provenance
    /// context, not an authentication claim.
    pub capability_provenance: String,
    /// Dispatch attempt history after a probe/lease boundary. Probe-only,
    /// busy, deferred, and ship-side cancellation populations are absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_attempts: Option<Vec<RouteAttempt>>,
    /// Whether the snapshot was rebuilt from dispatch-time target data rather
    /// than carried from the admission observation.
    #[serde(default)]
    pub snapshot_rebuilt_at_dispatch: bool,
}

/// Selected route identity and its position in the ordered snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteSelection {
    /// Candidate route identity.
    pub route_id: String,
    /// Zero-based position in the authoritative candidate order.
    pub position: usize,
}

/// Per-candidate counterfactual outcome.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteCounterfactual {
    /// Candidate route identity.
    pub route_id: String,
    /// Zero-based position in the authoritative candidate order.
    pub position: usize,
    /// Execution environment copied from the snapshot.
    pub execution_environment: String,
    /// Artifact capability copied from the snapshot.
    pub artifact_capability: String,
    /// Typed comparison outcome.
    pub outcome: RouteCounterfactualOutcome,
}

/// Controls and provenance for a shadow receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteShadowControl {
    /// Authority whose ordering was evaluated.
    pub authority: String,
    /// Scope of this evaluator. This describes the evaluator, not any caller
    /// that supplied the snapshot.
    pub scope: RouteShadowScope,
}

/// Scope marker for a route shadow receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteShadowScope {
    /// The evaluator records an observation and does not own dispatch.
    ReceiptOnly,
}

/// Errors raised when a snapshot cannot be treated as authoritative input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteShadowError {
    /// Snapshot schema is not understood.
    UnsupportedSchema(u32),
    /// Required request identity is absent.
    EmptyRequestId,
    /// Required target identity is absent.
    EmptyTargetName,
    /// At least one ordered candidate is required.
    EmptyCandidates,
    /// Candidate identity is absent.
    EmptyRouteId {
        /// Position of the invalid candidate.
        position: usize,
    },
    /// Candidate identities must be unique to keep receipts unambiguous.
    DuplicateRouteId(String),
    /// Runtime observation correlation is absent or does not match the
    /// immutable snapshot request identity.
    InvalidCorrelation,
    /// Runtime observation was already stale when receipt emission began.
    StaleObservation,
}

impl std::fmt::Display for RouteShadowError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported route shadow schema {version}")
            }
            Self::EmptyRequestId => {
                formatter.write_str("route shadow request_id must not be empty")
            }
            Self::EmptyTargetName => {
                formatter.write_str("route shadow target_name must not be empty")
            }
            Self::EmptyCandidates => {
                formatter.write_str("route shadow candidates must not be empty")
            }
            Self::EmptyRouteId { position } => write!(
                formatter,
                "route shadow candidate {position} has an empty route_id"
            ),
            Self::DuplicateRouteId(route_id) => {
                write!(formatter, "route shadow route_id is duplicated: {route_id}")
            }
            Self::InvalidCorrelation => {
                formatter.write_str("route shadow observation correlation is invalid")
            }
            Self::StaleObservation => formatter.write_str("route shadow observation is stale"),
        }
    }
}

impl std::error::Error for RouteShadowError {}

/// Durable store for receipt-only route shadow observations.
///
/// The store is deliberately separate from queue admission. A caller that
/// already owns an immutable snapshot can record its typed receipt without
/// changing queue state or selecting a different route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteShadowReceiptStore {
    root: PathBuf,
}

impl RouteShadowReceiptStore {
    /// Create a store below the supplied Shipyard state directory.
    #[must_use]
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: state_dir.into().join("route_shadow"),
        }
    }

    /// Return the durable path for a request identity.
    #[must_use]
    pub fn path_for(&self, request_id: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", request_id_digest(request_id)))
    }

    /// Evaluate and durably write one receipt.
    pub fn record(
        &self,
        snapshot: &RouteCapabilitySnapshot,
    ) -> Result<RouteShadowReceipt, RouteShadowStoreError> {
        let receipt = evaluate(snapshot)?;
        let path = self.path_for(&snapshot.request_id);
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&path)
            .map_err(RouteShadowStoreError::Io)?;
        crate::durable_file::replace_json(&path, &receipt, true)
            .map_err(RouteShadowStoreError::Io)?;
        Ok(receipt)
    }

    /// Evaluate and durably write one fresh, provenance-bound scheduler
    /// observation. This is intentionally a separate API from the original
    /// pure snapshot recorder so legacy callers cannot accidentally claim
    /// runtime provenance.
    pub fn record_observation(
        &self,
        observation: &RouteShadowObservation,
    ) -> Result<RouteShadowReceipt, RouteShadowStoreError> {
        if observation.correlation_id.trim().is_empty()
            || observation.correlation_id != observation.snapshot.request_id
        {
            return Err(RouteShadowError::InvalidCorrelation.into());
        }
        let expires_at = observation.expires_at();
        if observation.capability_provenance.trim().is_empty()
            || observation.freshness <= Duration::zero()
            || Utc::now() > expires_at
        {
            return Err(RouteShadowError::StaleObservation.into());
        }
        let mut receipt = evaluate(&observation.snapshot)?;
        receipt.observation = Some(RouteShadowObservationMetadata {
            correlation_id: observation.correlation_id.clone(),
            observed_at: observation.observed_at,
            expires_at,
            capability_provenance: observation.capability_provenance.clone(),
            dispatch_attempts: observation.route_attempts.clone(),
            snapshot_rebuilt_at_dispatch: observation.capability_provenance
                == "shipyard.dispatch.attempts",
        });
        let path = self.path_for(&observation.snapshot.request_id);
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&path)
            .map_err(RouteShadowStoreError::Io)?;
        crate::durable_file::replace_json(&path, &receipt, true)
            .map_err(RouteShadowStoreError::Io)?;
        Ok(receipt)
    }

    /// Best-effort organic writer with no wait on the shared writer domain.
    /// Scheduler admission uses this only from its background sink, so a
    /// dropped or stalled observation can never delay a job.
    pub fn record_observation_best_effort(
        &self,
        observation: &RouteShadowObservation,
    ) -> Result<bool, RouteShadowStoreError> {
        self.record_observation_best_effort_with_retention_limit(
            observation,
            RECEIPT_RETENTION_LIMIT,
        )
    }

    fn record_observation_best_effort_with_retention_limit(
        &self,
        observation: &RouteShadowObservation,
        retention_limit: usize,
    ) -> Result<bool, RouteShadowStoreError> {
        if observation.correlation_id.trim().is_empty()
            || observation.correlation_id != observation.snapshot.request_id
        {
            return Err(RouteShadowError::InvalidCorrelation.into());
        }
        let expires_at = observation.expires_at();
        if observation.capability_provenance.trim().is_empty()
            || observation.freshness <= Duration::zero()
            || Utc::now() > expires_at
        {
            return Err(RouteShadowError::StaleObservation.into());
        }
        let mut receipt = evaluate(&observation.snapshot)?;
        receipt.observation = Some(RouteShadowObservationMetadata {
            correlation_id: observation.correlation_id.clone(),
            observed_at: observation.observed_at,
            expires_at,
            capability_provenance: observation.capability_provenance.clone(),
            dispatch_attempts: observation.route_attempts.clone(),
            snapshot_rebuilt_at_dispatch: observation.capability_provenance
                == "shipyard.dispatch.attempts",
        });
        let path = self.path_for(&observation.snapshot.request_id);
        let _writer_domain =
            match crate::writer_domain_lease::acquire_for_protected_path_with_timeout(
                &path,
                StdDuration::ZERO,
            ) {
                Ok(lease) => lease,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(false);
                }
                Err(error) => return Err(RouteShadowStoreError::Io(error)),
            };
        crate::durable_file::replace_json(&path, &receipt, false)
            .map_err(RouteShadowStoreError::Io)?;
        let _ = self.prune_receipts(retention_limit);
        Ok(true)
    }

    /// Attach the authoritative dispatch decision to an existing admission
    /// receipt without rebuilding or replacing its scheduler snapshot.
    /// Returns `Ok(None)` when no pending admission receipt exists yet.
    fn attach_selected_route_best_effort(
        &self,
        observation: &RouteShadowObservation,
    ) -> Result<Option<bool>, RouteShadowStoreError> {
        let Some(mut receipt) = self.load(&observation.snapshot.request_id)? else {
            return Ok(None);
        };
        let Some(metadata) = receipt.observation.as_mut() else {
            return Ok(None);
        };
        if metadata.correlation_id != observation.correlation_id {
            return Ok(None);
        }
        metadata
            .dispatch_attempts
            .clone_from(&observation.route_attempts);
        let path = self.path_for(&observation.snapshot.request_id);
        let _writer_domain =
            match crate::writer_domain_lease::acquire_for_protected_path_with_timeout(
                &path,
                StdDuration::ZERO,
            ) {
                Ok(lease) => lease,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(Some(false));
                }
                Err(error) => return Err(RouteShadowStoreError::Io(error)),
            };
        crate::durable_file::replace_json(&path, &receipt, false)
            .map_err(RouteShadowStoreError::Io)?;
        let _ = self.prune_receipts(RECEIPT_RETENTION_LIMIT);
        Ok(Some(true))
    }

    /// Keep receipt storage bounded without making retention failures affect
    /// the observation result. The newest files by filesystem modification
    /// time are retained; ties are resolved by path for deterministic tests.
    fn prune_receipts(&self, limit: usize) -> std::io::Result<()> {
        let mut entries = fs::read_dir(&self.root)?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .map(|entry| {
                let path = entry.path();
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                (modified, path)
            })
            .collect::<Vec<_>>();
        if entries.len() <= limit {
            return Ok(());
        }
        entries.sort_by(|(left_time, left_path), (right_time, right_path)| {
            left_time
                .cmp(right_time)
                .then_with(|| left_path.cmp(right_path))
        });
        let excess = entries.len() - limit;
        for (_, path) in entries.into_iter().take(excess) {
            let _ = fs::remove_file(path);
        }
        Ok(())
    }

    /// Load a previously recorded receipt, if present.
    pub fn load(
        &self,
        request_id: &str,
    ) -> Result<Option<RouteShadowReceipt>, RouteShadowStoreError> {
        let path = self.path_for(request_id);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(RouteShadowStoreError::Io(error)),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(RouteShadowStoreError::Json)
    }
}

/// Failure while evaluating or durably storing a route shadow receipt.
#[derive(Debug)]
pub enum RouteShadowStoreError {
    /// Snapshot evaluation failed.
    Evaluation(RouteShadowError),
    /// Durable filesystem operation failed.
    Io(std::io::Error),
    /// Receipt JSON could not be decoded.
    Json(serde_json::Error),
}

impl std::fmt::Display for RouteShadowStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Evaluation(error) => write!(formatter, "route shadow evaluation failed: {error}"),
            Self::Io(error) => write!(formatter, "route shadow receipt I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "route shadow receipt JSON failed: {error}"),
        }
    }
}

impl std::error::Error for RouteShadowStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Evaluation(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
        }
    }
}

impl From<RouteShadowError> for RouteShadowStoreError {
    fn from(error: RouteShadowError) -> Self {
        Self::Evaluation(error)
    }
}

fn request_id_digest(request_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(request_id.as_bytes());
    hex::encode(digest.finalize())
}

/// Evaluate an immutable route snapshot without touching dispatch state.
///
/// The first candidate that is both available and capable is authoritative.
/// Later capable candidates are recorded as counterfactuals, which gives a
/// benchmark/control pair without allowing shadow observations to reroute a
/// real job.
pub fn evaluate(
    snapshot: &RouteCapabilitySnapshot,
) -> Result<RouteShadowReceipt, RouteShadowError> {
    validate_snapshot(snapshot)?;
    let required = canonical_capabilities(&snapshot.required_capabilities);
    let authoritative_position = snapshot
        .candidates
        .iter()
        .enumerate()
        .find(|(_, candidate)| {
            candidate.availability == RouteAvailability::Available
                && missing_capabilities(candidate, &required).is_empty()
        })
        .map(|(position, _)| position);

    let counterfactuals = snapshot
        .candidates
        .iter()
        .enumerate()
        .map(|(position, candidate)| {
            let missing = missing_capabilities(candidate, &required);
            let outcome = if !missing.is_empty() {
                RouteCounterfactualOutcome::CapabilityMismatch { missing }
            } else if candidate.availability != RouteAvailability::Available {
                RouteCounterfactualOutcome::Unavailable {
                    availability: candidate.availability,
                }
            } else if Some(position) == authoritative_position {
                RouteCounterfactualOutcome::Authoritative
            } else {
                RouteCounterfactualOutcome::WouldAdmitIfEarlierUnavailable
            };
            RouteCounterfactual {
                route_id: candidate.route_id.clone(),
                position,
                execution_environment: candidate.execution_environment.clone(),
                artifact_capability: candidate.artifact_capability.clone(),
                outcome,
            }
        })
        .collect();

    Ok(RouteShadowReceipt {
        schema_version: ROUTE_SHADOW_SCHEMA_VERSION,
        request_id: snapshot.request_id.clone(),
        target_name: snapshot.target_name.clone(),
        required_capabilities: required,
        authoritative_route: authoritative_position.map(|position| RouteSelection {
            route_id: snapshot.candidates[position].route_id.clone(),
            position,
        }),
        counterfactuals,
        control: RouteShadowControl {
            authority: "ordered_route_selection".to_owned(),
            scope: RouteShadowScope::ReceiptOnly,
        },
        observation: None,
    })
}

/// Runtime scheduler observation passed to the receipt-only evaluator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteShadowObservation {
    /// Immutable scheduler-owned route snapshot.
    pub snapshot: RouteCapabilitySnapshot,
    /// Stable job/target identity.
    pub correlation_id: String,
    /// Snapshot read time.
    pub observed_at: DateTime<Utc>,
    /// Maximum age of the scheduler facts.
    pub freshness: Duration,
    /// Source label for capability and admission facts.
    pub capability_provenance: String,
    /// Actual route selected by the authoritative dispatcher, when known.
    pub route_attempts: Option<Vec<RouteAttempt>>,
}

impl RouteShadowObservation {
    fn expires_at(&self) -> DateTime<Utc> {
        self.observed_at + self.freshness
    }
}

/// Emit best-effort organic route observations from scheduler-owned target
/// facts. No probe, lease acquisition, queue mutation, or dispatch occurs;
/// write failures are deliberately ignored by the caller boundary.
pub fn record_scheduler_observation_best_effort(
    state_dir: &std::path::Path,
    job_id: &str,
    target: &ResolvedTarget,
    leases: &[HostPoolLease],
    observed_at: DateTime<Utc>,
) {
    let Some(snapshot) = scheduler_snapshot(job_id, target, leases, observed_at) else {
        return;
    };
    let observation = RouteShadowObservation {
        correlation_id: snapshot.request_id.clone(),
        snapshot,
        observed_at,
        freshness: Duration::seconds(30),
        capability_provenance: scheduler_provenance(target),
        route_attempts: None,
    };
    let Some(sender) = observation_sender() else {
        return;
    };
    let _ = enqueue_observation(
        sender,
        PendingObservation {
            state_dir: state_dir.to_path_buf(),
            observation,
        },
    );
}

/// Emit a post-dispatch observation bound to the route label returned by the
/// authoritative dispatcher. The route choice is copied into metadata; it is
/// never used to reroute, reacquire, or release a lease.
pub fn record_dispatch_observation_best_effort(
    state_dir: &std::path::Path,
    job_id: &str,
    target: &ResolvedTarget,
    route_attempts: &[RouteAttempt],
    observed_at: DateTime<Utc>,
) {
    let Some(pending) =
        dispatch_observation(state_dir, job_id, target, route_attempts, observed_at)
    else {
        return;
    };
    let Some(sender) = observation_sender() else {
        return;
    };
    let _ = enqueue_observation(sender, pending);
}

/// Build the post-dispatch observation, or `None` when no attempt names one of
/// the target's scheduler candidates.
fn dispatch_observation(
    state_dir: &std::path::Path,
    job_id: &str,
    target: &ResolvedTarget,
    route_attempts: &[RouteAttempt],
    observed_at: DateTime<Utc>,
) -> Option<PendingObservation> {
    let snapshot = scheduler_snapshot(job_id, target, &[], observed_at)?;
    route_attempts.iter().rev().find(|attempt| {
        snapshot
            .candidates
            .iter()
            .any(|candidate| candidate.route_id == attempt.route_id)
    })?;
    let observation = RouteShadowObservation {
        correlation_id: snapshot.request_id.clone(),
        snapshot: {
            // Preserve the immutable scheduler snapshot exactly as observed.
            snapshot
        },
        observed_at,
        freshness: Duration::seconds(30),
        capability_provenance: "shipyard.dispatch.attempts".to_owned(),
        route_attempts: Some(route_attempts.to_vec()),
    };
    Some(PendingObservation {
        state_dir: state_dir.to_path_buf(),
        observation,
    })
}

fn observation_sender() -> Option<&'static SyncSender<PendingObservation>> {
    OBSERVATION_SENDER
        .get_or_init(|| {
            let (sender, receiver) =
                mpsc::sync_channel::<PendingObservation>(OBSERVATION_QUEUE_CAPACITY);
            let worker_started = thread::Builder::new()
                .name("route-shadow-receipts".to_owned())
                .spawn(move || {
                    while let Ok(pending) = receiver.recv() {
                        write_pending_observation(pending);
                    }
                })
                .is_ok();
            worker_started.then_some(sender)
        })
        .as_ref()
}

/// Persist one queued observation. Runs only on the background writer.
fn write_pending_observation(pending: PendingObservation) {
    let store = RouteShadowReceiptStore::new(pending.state_dir);
    if pending
        .observation
        .route_attempts
        .as_ref()
        .is_some_and(|a| !a.is_empty())
    {
        match store.attach_selected_route_best_effort(&pending.observation) {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {}
        }
    }
    if store
        .load(&pending.observation.snapshot.request_id)
        .ok()
        .flatten()
        .and_then(|receipt| receipt.observation)
        .is_some_and(|metadata| {
            metadata.expires_at > pending.observation.observed_at
                && pending.observation.route_attempts.is_none()
        })
    {
        return;
    }
    let _ = store.record_observation_best_effort(&pending.observation);
}

fn enqueue_observation(
    sender: &SyncSender<PendingObservation>,
    pending: PendingObservation,
) -> bool {
    sender.try_send(pending).is_ok()
}

fn scheduler_snapshot(
    job_id: &str,
    target: &ResolvedTarget,
    leases: &[HostPoolLease],
    observed_at: DateTime<Utc>,
) -> Option<RouteCapabilitySnapshot> {
    let request_id = format!("{job_id}:{}", target.name);
    let (required_capabilities, candidates) = match &target.backend {
        ResolvedBackend::HostPool(pool) => {
            let candidates = pool
                .members
                .iter()
                .map(|member| {
                    let active = leases
                        .iter()
                        .filter(|lease| {
                            lease.pool_name == pool.pool_name
                                && lease.member_id == member.id
                                && !lease.is_stale(observed_at)
                        })
                        .count()
                        .try_into()
                        .unwrap_or(u32::MAX);
                    let availability = if active >= member.max_concurrency {
                        RouteAvailability::Busy
                    } else {
                        // Capacity is not reachability or dispatch admission.
                        RouteAvailability::Unknown
                    };
                    RouteCandidateSnapshot {
                        route_id: format!("host-pool:{}:{}", pool.pool_name, member.id),
                        execution_environment: member.target.platform.clone(),
                        artifact_capability: target.platform.clone(),
                        capabilities: member.capabilities.clone(),
                        availability,
                    }
                })
                .collect::<Vec<_>>();
            (pool.requires.clone(), candidates)
        }
        ResolvedBackend::Fallback(chain) => {
            let candidates = chain
                .backends
                .iter()
                .enumerate()
                .map(|(position, backend)| RouteCandidateSnapshot {
                    route_id: format!("fallback:{}:{position}", backend.label),
                    execution_environment: backend.target.platform.clone(),
                    artifact_capability: target.platform.clone(),
                    capabilities: backend.capabilities.clone(),
                    // No host probe or lease read exists for fallback
                    // backends here, so availability remains unknown.
                    availability: RouteAvailability::Unknown,
                })
                .collect::<Vec<_>>();
            (chain.requires.clone(), candidates)
        }
        ResolvedBackend::Local(_)
        | ResolvedBackend::Ssh(_)
        | ResolvedBackend::Windows(_)
        | ResolvedBackend::Cloud(_) => return None,
    };
    (!candidates.is_empty()).then_some(RouteCapabilitySnapshot {
        schema_version: ROUTE_SHADOW_SCHEMA_VERSION,
        request_id,
        target_name: target.name.clone(),
        required_capabilities,
        candidates,
    })
}

fn scheduler_provenance(target: &ResolvedTarget) -> String {
    match &target.backend {
        ResolvedBackend::HostPool(_) => "shipyard.scheduler.host_pool_config+leases".to_owned(),
        ResolvedBackend::Fallback(_) => "shipyard.scheduler.fallback_capabilities".to_owned(),
        ResolvedBackend::Local(_)
        | ResolvedBackend::Ssh(_)
        | ResolvedBackend::Windows(_)
        | ResolvedBackend::Cloud(_) => String::new(),
    }
}

fn validate_snapshot(snapshot: &RouteCapabilitySnapshot) -> Result<(), RouteShadowError> {
    if snapshot.schema_version != ROUTE_SHADOW_SCHEMA_VERSION {
        return Err(RouteShadowError::UnsupportedSchema(snapshot.schema_version));
    }
    if snapshot.request_id.trim().is_empty() {
        return Err(RouteShadowError::EmptyRequestId);
    }
    if snapshot.target_name.trim().is_empty() {
        return Err(RouteShadowError::EmptyTargetName);
    }
    if snapshot.candidates.is_empty() {
        return Err(RouteShadowError::EmptyCandidates);
    }
    let mut seen = std::collections::BTreeSet::new();
    for (position, candidate) in snapshot.candidates.iter().enumerate() {
        if candidate.route_id.trim().is_empty() {
            return Err(RouteShadowError::EmptyRouteId { position });
        }
        if !seen.insert(candidate.route_id.as_str()) {
            return Err(RouteShadowError::DuplicateRouteId(
                candidate.route_id.clone(),
            ));
        }
    }
    Ok(())
}

fn canonical_capabilities(capabilities: &[String]) -> Vec<String> {
    let mut canonical = capabilities
        .iter()
        .map(|capability| capability.trim().to_owned())
        .filter(|capability| !capability.is_empty())
        .collect::<Vec<_>>();
    canonical.sort();
    canonical.dedup();
    canonical
}

fn missing_capabilities(candidate: &RouteCandidateSnapshot, required: &[String]) -> Vec<String> {
    let capabilities = candidate
        .capabilities
        .iter()
        .map(|capability| capability.trim())
        .collect::<std::collections::BTreeSet<_>>();
    required
        .iter()
        .filter(|required| !capabilities.contains(required.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::PathBuf;
    use std::sync::mpsc;

    use chrono::{Duration, Utc};
    use fs2::FileExt;

    use super::{
        PendingObservation, ROUTE_SHADOW_SCHEMA_VERSION, RouteAvailability, RouteCandidateSnapshot,
        RouteCapabilitySnapshot, RouteCounterfactualOutcome, RouteShadowError,
        RouteShadowObservation, RouteShadowReceiptStore, dispatch_observation, enqueue_observation,
        evaluate, scheduler_snapshot, write_pending_observation,
    };
    use crate::executor::dispatch::{
        FallbackBackend, FallbackTargetConfig, ResolvedBackend, ResolvedHostPoolConfig,
        ResolvedHostPoolMember, ResolvedTarget, ResolvedValidation,
    };
    use crate::executor::local::{LocalTargetConfig, LocalValidationConfig};
    use crate::host_pool::HostPoolLease;
    use crate::job::RouteAttempt;

    fn candidate(
        route_id: &str,
        environment: &str,
        artifact: &str,
        capabilities: &[&str],
        availability: RouteAvailability,
    ) -> RouteCandidateSnapshot {
        RouteCandidateSnapshot {
            route_id: route_id.to_owned(),
            execution_environment: environment.to_owned(),
            artifact_capability: artifact.to_owned(),
            capabilities: capabilities
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            availability,
        }
    }

    fn snapshot(candidates: Vec<RouteCandidateSnapshot>) -> RouteCapabilitySnapshot {
        RouteCapabilitySnapshot {
            schema_version: ROUTE_SHADOW_SCHEMA_VERSION,
            request_id: "job-1".to_owned(),
            target_name: "mac".to_owned(),
            required_capabilities: vec!["arm64".to_owned(), "macos".to_owned()],
            candidates,
        }
    }

    #[test]
    fn preserves_ordered_authority_and_records_capable_fallback() {
        let receipt = evaluate(&snapshot(vec![
            candidate(
                "primary",
                "macos-arm64",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            ),
            candidate(
                "overflow",
                "github-hosted-macos",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            ),
        ]))
        .expect("receipt");

        assert_eq!(
            receipt
                .authoritative_route
                .as_ref()
                .map(|route| route.route_id.as_str()),
            Some("primary")
        );
        assert!(matches!(
            receipt.counterfactuals[0].outcome,
            RouteCounterfactualOutcome::Authoritative
        ));
        assert!(matches!(
            receipt.counterfactuals[1].outcome,
            RouteCounterfactualOutcome::WouldAdmitIfEarlierUnavailable
        ));
        assert!(matches!(
            receipt.control.scope,
            super::RouteShadowScope::ReceiptOnly
        ));
    }

    #[test]
    fn control_selects_overflow_only_when_primary_is_unavailable() {
        let receipt = evaluate(&snapshot(vec![
            candidate(
                "primary",
                "macos-arm64",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Busy,
            ),
            candidate(
                "overflow",
                "github-hosted-macos",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            ),
        ]))
        .expect("receipt");

        assert_eq!(
            receipt
                .authoritative_route
                .as_ref()
                .map(|route| route.route_id.as_str()),
            Some("overflow")
        );
        assert!(matches!(
            receipt.counterfactuals[0].outcome,
            RouteCounterfactualOutcome::Unavailable {
                availability: RouteAvailability::Busy
            }
        ));
        assert!(matches!(
            receipt.counterfactuals[1].outcome,
            RouteCounterfactualOutcome::Authoritative
        ));
    }

    #[test]
    fn capability_admission_is_independent_from_execution_environment() {
        let receipt = evaluate(&snapshot(vec![candidate(
            "cross-build",
            "linux-arm64",
            "macos-arm64",
            &["macos", "arm64"],
            RouteAvailability::Available,
        )]))
        .expect("receipt");

        assert_eq!(
            receipt
                .authoritative_route
                .as_ref()
                .map(|route| route.route_id.as_str()),
            Some("cross-build")
        );
        assert_eq!(
            receipt.counterfactuals[0].execution_environment,
            "linux-arm64"
        );
        assert_eq!(
            receipt.counterfactuals[0].artifact_capability,
            "macos-arm64"
        );
    }

    #[test]
    fn unavailable_and_missing_capabilities_are_typed() {
        let receipt = evaluate(&snapshot(vec![
            candidate(
                "wrong-cap",
                "linux",
                "linux",
                &["linux"],
                RouteAvailability::Available,
            ),
            candidate(
                "busy",
                "macos-arm64",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Busy,
            ),
        ]))
        .expect("receipt");

        assert_eq!(receipt.authoritative_route, None);
        assert!(matches!(
            receipt.counterfactuals[0].outcome,
            RouteCounterfactualOutcome::CapabilityMismatch { .. }
        ));
        assert!(matches!(
            receipt.counterfactuals[1].outcome,
            RouteCounterfactualOutcome::Unavailable {
                availability: RouteAvailability::Busy
            }
        ));
    }

    #[test]
    fn rejects_ambiguous_snapshots() {
        let mut invalid = snapshot(vec![
            candidate(
                "same",
                "macos-arm64",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            ),
            candidate(
                "same",
                "github-hosted-macos",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            ),
        ]);
        assert!(matches!(
            evaluate(&invalid),
            Err(RouteShadowError::DuplicateRouteId(_))
        ));
        invalid.schema_version = 99;
        assert!(matches!(
            evaluate(&invalid),
            Err(RouteShadowError::UnsupportedSchema(99))
        ));
    }

    #[test]
    fn receipt_store_evaluates_and_round_trips_durably() {
        let temp = tempfile::tempdir().expect("state directory");
        let store = super::RouteShadowReceiptStore::new(temp.path());
        let receipt = store
            .record(&snapshot(vec![candidate(
                "primary",
                "macos-arm64",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            )]))
            .expect("record receipt");
        let loaded = store
            .load("job-1")
            .expect("load receipt")
            .expect("receipt exists");

        assert_eq!(loaded, receipt);
        assert!(store.path_for("job-1").is_file());
        assert!(store.path_for("job-1").starts_with(temp.path()));
    }

    #[test]
    fn receipt_retention_prunes_oldest_json_files_by_count() {
        let temp = tempfile::tempdir().expect("state directory");
        let store = super::RouteShadowReceiptStore::new(temp.path());
        for request_id in ["old-a", "middle", "new"] {
            let mut snapshot = snapshot(vec![candidate(
                "primary",
                "macos-arm64",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            )]);
            snapshot.request_id = request_id.to_owned();
            store
                .record_observation_best_effort_with_retention_limit(
                    &RouteShadowObservation {
                        correlation_id: request_id.to_owned(),
                        snapshot,
                        observed_at: Utc::now(),
                        freshness: Duration::seconds(30),
                        capability_provenance: "test".to_owned(),
                        route_attempts: None,
                    },
                    2,
                )
                .expect("record receipt");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        assert!(!store.path_for("old-a").exists());
        assert!(store.path_for("middle").exists());
        assert!(store.path_for("new").exists());
    }

    #[test]
    fn organic_observation_binds_correlation_and_provenance() {
        let temp = tempfile::tempdir().expect("state directory");
        let store = super::RouteShadowReceiptStore::new(temp.path());
        let observed_at = Utc::now();
        let receipt = store
            .record_observation(&RouteShadowObservation {
                snapshot: snapshot(vec![candidate(
                    "primary",
                    "macos-arm64",
                    "macos-arm64",
                    &["macos", "arm64"],
                    RouteAvailability::Available,
                )]),
                correlation_id: "job-1".to_owned(),
                observed_at,
                freshness: Duration::seconds(30),
                capability_provenance: "shipyard.scheduler.test".to_owned(),
                route_attempts: None,
            })
            .expect("record organic observation");
        let metadata = receipt.observation.expect("observation metadata");
        assert_eq!(metadata.correlation_id, "job-1");
        assert_eq!(metadata.observed_at, observed_at);
        assert_eq!(metadata.capability_provenance, "shipyard.scheduler.test");
    }

    #[test]
    fn organic_writer_uses_zero_timeout_on_protected_state() {
        let runtime_paths =
            crate::paths::RuntimePaths::current(crate::identity::RuntimeMode::Shipyard);
        std::fs::create_dir_all(&runtime_paths.state_dir).expect("state directory");
        let lock_path = runtime_paths.state_dir.join(".sandbox-writer-domain.lock");
        let lock = File::create(&lock_path).expect("writer lock");
        lock.try_lock_exclusive().expect("hold writer lock");

        let store = RouteShadowReceiptStore::new(runtime_paths.state_dir.clone());
        let observation = RouteShadowObservation {
            snapshot: snapshot(vec![candidate(
                "primary",
                "macos-arm64",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            )]),
            correlation_id: "job-1".to_owned(),
            observed_at: Utc::now(),
            freshness: Duration::seconds(30),
            capability_provenance: "test".to_owned(),
            route_attempts: None,
        };
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = done_tx.send(store.record_observation_best_effort(&observation));
        });
        let result = done_rx.recv_timeout(std::time::Duration::from_secs(1));
        lock.unlock().expect("release writer lock");
        worker.join().expect("writer worker exits");
        let result = result.expect("zero-timeout writer must return promptly");
        assert!(matches!(result, Ok(false)));
    }

    #[test]
    fn stale_or_mismatched_organic_observation_is_refused() {
        let temp = tempfile::tempdir().expect("state directory");
        let store = super::RouteShadowReceiptStore::new(temp.path());
        let snapshot = snapshot(vec![candidate(
            "primary",
            "macos-arm64",
            "macos-arm64",
            &["macos", "arm64"],
            RouteAvailability::Available,
        )]);
        let stale = RouteShadowObservation {
            snapshot: snapshot.clone(),
            correlation_id: snapshot.request_id.clone(),
            observed_at: Utc::now() - Duration::minutes(2),
            freshness: Duration::seconds(1),
            capability_provenance: "shipyard.scheduler.test".to_owned(),
            route_attempts: None,
        };
        assert!(matches!(
            store.record_observation(&stale),
            Err(super::RouteShadowStoreError::Evaluation(
                RouteShadowError::StaleObservation
            ))
        ));
        let mismatch = RouteShadowObservation {
            snapshot,
            correlation_id: "other-job".to_owned(),
            observed_at: Utc::now(),
            freshness: Duration::seconds(30),
            capability_provenance: "shipyard.scheduler.test".to_owned(),
            route_attempts: None,
        };
        assert!(matches!(
            store.record_observation(&mismatch),
            Err(super::RouteShadowStoreError::Evaluation(
                RouteShadowError::InvalidCorrelation
            ))
        ));
    }

    fn host_pool_target() -> ResolvedTarget {
        let member_target = ResolvedTarget {
            name: "member".to_owned(),
            validation_build_type: None,
            platform: "macos-arm64".to_owned(),
            backend_name: "local".to_owned(),
            warm_keepalive_seconds: 0,
            host: None,
            backend: ResolvedBackend::Local(LocalTargetConfig::default()),
            validation: ResolvedValidation::Local(LocalValidationConfig::default()),
            failure_parser: None,
        };
        ResolvedTarget {
            name: "mac".to_owned(),
            validation_build_type: None,
            platform: "macos-arm64".to_owned(),
            backend_name: "host-pool".to_owned(),
            warm_keepalive_seconds: 0,
            host: None,
            backend: ResolvedBackend::HostPool(ResolvedHostPoolConfig {
                pool_name: "macs".to_owned(),
                strategy: "ordered".to_owned(),
                lease_stale_seconds: 180,
                heartbeat_interval_seconds: 15,
                requires: vec!["macos".to_owned(), "arm64".to_owned()],
                members: vec![ResolvedHostPoolMember {
                    id: "m1".to_owned(),
                    target: Box::new(member_target),
                    label: "M1".to_owned(),
                    profile_label: "macos-arm64".to_owned(),
                    max_concurrency: 1,
                    capabilities: vec!["macos".to_owned(), "arm64".to_owned()],
                }],
            }),
            validation: ResolvedValidation::HostPool,
            failure_parser: None,
        }
    }

    #[test]
    fn scheduler_snapshot_does_not_infer_available_from_capacity() {
        let observed_at = Utc::now();
        let target = host_pool_target();
        let snapshot = scheduler_snapshot("job-1", &target, &[], observed_at).expect("snapshot");
        assert_eq!(snapshot.candidates.len(), 1);
        assert_eq!(
            snapshot.candidates[0].availability,
            RouteAvailability::Unknown
        );
        assert_eq!(
            evaluate(&snapshot).expect("receipt").authoritative_route,
            None
        );

        let lease = HostPoolLease {
            lease_id: "lease-1".to_owned(),
            pool_name: "macs".to_owned(),
            member_id: "m1".to_owned(),
            target_name: "mac".to_owned(),
            backend: "local".to_owned(),
            host: None,
            job_id: Some("job-0".to_owned()),
            branch: "main".to_owned(),
            sha: "head".to_owned(),
            owner_pid: 1,
            acquired_at: observed_at,
            heartbeat_at: observed_at,
            expires_at: observed_at + Duration::seconds(30),
        };
        let busy =
            scheduler_snapshot("job-1", &target, &[lease], observed_at).expect("busy snapshot");
        assert_eq!(busy.candidates[0].availability, RouteAvailability::Busy);
    }

    #[test]
    fn runtime_sink_emits_pending_then_selected_route_receipt() {
        let temp = tempfile::tempdir().expect("state directory");
        let target = host_pool_target();
        let observed_at = Utc::now();
        super::record_scheduler_observation_best_effort(
            temp.path(),
            "job-runtime",
            &target,
            &[],
            observed_at,
        );
        let store = super::RouteShadowReceiptStore::new(temp.path());
        let path = store.path_for("job-runtime:mac");
        let mut pending = None;
        for _ in 0..5_000 {
            if let Ok(Some(receipt)) = store.load("job-runtime:mac") {
                pending = Some(receipt);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let pending = pending.expect("background sink receipt");
        assert!(
            pending
                .observation
                .expect("metadata")
                .dispatch_attempts
                .is_none()
        );
        assert!(matches!(
            pending.counterfactuals[0].outcome,
            RouteCounterfactualOutcome::Unavailable {
                availability: RouteAvailability::Unknown
            }
        ));
        assert!(path.is_file());

        super::record_dispatch_observation_best_effort(
            temp.path(),
            "job-runtime",
            &target,
            &[RouteAttempt {
                route_id: "host-pool:macs:m1".to_owned(),
                position: 0,
                outcome: crate::job::TargetStatus::Pass,
                attempted_at: Utc::now(),
            }],
            Utc::now(),
        );
        let mut selected = None;
        for _ in 0..5_000 {
            if let Ok(Some(receipt)) = store.load("job-runtime:mac")
                && receipt
                    .observation
                    .as_ref()
                    .and_then(|metadata| metadata.dispatch_attempts.as_ref())
                    .is_some()
            {
                selected = Some(receipt);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let selected = selected.expect("selected route receipt");
        let metadata = selected.observation.expect("metadata");
        assert!(!metadata.snapshot_rebuilt_at_dispatch);
        assert_eq!(
            metadata
                .dispatch_attempts
                .expect("dispatch attempts")
                .last()
                .expect("attempt")
                .route_id,
            "host-pool:macs:m1"
        );
        assert!(matches!(
            selected.counterfactuals[0].outcome,
            RouteCounterfactualOutcome::Unavailable {
                availability: RouteAvailability::Unknown
            }
        ));
    }

    #[test]
    fn nested_host_pool_attempt_keeps_outer_fallback_receipt() {
        let temp = tempfile::tempdir().expect("state directory");
        let pool_target = host_pool_target();
        let target = ResolvedTarget {
            name: "nested".to_owned(),
            backend_name: "fallback".to_owned(),
            backend: ResolvedBackend::Fallback(FallbackTargetConfig {
                backends: vec![FallbackBackend {
                    target: Box::new(pool_target),
                    label: "host-pool:macs".to_owned(),
                    profile_label: "macs".to_owned(),
                    capabilities: vec!["macos".to_owned(), "arm64".to_owned()],
                }],
                requires: Vec::new(),
                heartbeat_stale_secs: 60,
            }),
            ..host_pool_target()
        };
        // Drive the writer synchronously. The process-wide background sink is
        // shared with every other test that dispatches, and it drops on a full
        // queue by design, so polling it for this receipt races the suite.
        let pending = dispatch_observation(
            temp.path(),
            "job-nested",
            &target,
            &[
                RouteAttempt {
                    route_id: "fallback:host-pool:macs:0".to_owned(),
                    position: 0,
                    outcome: crate::job::TargetStatus::Pass,
                    attempted_at: Utc::now(),
                },
                RouteAttempt {
                    route_id: "host-pool:macs:m1".to_owned(),
                    position: 0,
                    outcome: crate::job::TargetStatus::Pass,
                    attempted_at: Utc::now(),
                },
            ],
            Utc::now(),
        )
        .expect("nested attempt matches the outer fallback candidate");
        write_pending_observation(pending);
        let receipt = RouteShadowReceiptStore::new(temp.path())
            .load("job-nested:nested")
            .expect("load receipt")
            .expect("nested fallback receipt");
        assert!(
            receipt
                .observation
                .as_ref()
                .expect("metadata")
                .snapshot_rebuilt_at_dispatch
        );
    }

    #[test]
    fn stalled_writer_queue_drops_without_blocking_admission() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let writer_release = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release = writer_release.clone();
        let writer = std::thread::spawn(move || {
            // Model a receipt writer stalled in filesystem I/O. The producer
            // must continue using only the bounded, nonblocking handoff.
            release.wait();
            let _ = receiver.recv();
        });
        let pending = || PendingObservation {
            state_dir: PathBuf::from("/unreachable"),
            observation: RouteShadowObservation {
                snapshot: snapshot(vec![candidate(
                    "primary",
                    "macos-arm64",
                    "macos-arm64",
                    &["macos", "arm64"],
                    RouteAvailability::Unknown,
                )]),
                correlation_id: "job-1".to_owned(),
                observed_at: Utc::now(),
                freshness: Duration::seconds(30),
                capability_provenance: "test".to_owned(),
                route_attempts: None,
            },
        };
        assert!(enqueue_observation(&sender, pending()));
        assert!(!enqueue_observation(&sender, pending()));
        let started_at = std::time::Instant::now();
        for _ in 0..128 {
            assert!(!enqueue_observation(&sender, pending()));
        }
        assert!(started_at.elapsed() < std::time::Duration::from_millis(100));
        writer_release.wait();
        writer.join().expect("stalled writer exits");
        assert!(!enqueue_observation(&sender, pending()));
    }

    #[test]
    fn enqueue_observation_rejects_disconnected_receiver() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let pending = PendingObservation {
            state_dir: PathBuf::from("/unreachable"),
            observation: RouteShadowObservation {
                snapshot: snapshot(vec![candidate(
                    "primary",
                    "macos-arm64",
                    "macos-arm64",
                    &["macos", "arm64"],
                    RouteAvailability::Unknown,
                )]),
                correlation_id: "job-1".to_owned(),
                observed_at: Utc::now(),
                freshness: Duration::seconds(30),
                capability_provenance: "test".to_owned(),
                route_attempts: None,
            },
        };
        assert!(!enqueue_observation(&sender, pending));
    }
}
