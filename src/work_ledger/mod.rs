//! Canonical work-item persistence and the protected continuation lane.
//!
//! Imports remain compatible with the legacy stores, while native continuation
//! publication and daemon dispatch use this ledger as their fenced authority.

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: i64 = 14;
const DATABASE_NAME: &str = "work-items.sqlite3";

macro_rules! candidate_params {
    ($candidate:expr, $now:expr) => {
        params![
            $candidate.work_id,
            $candidate.kind,
            $candidate.repo,
            $candidate.pr,
            $candidate.head_sha,
            $candidate.base_ref,
            $candidate.goal_id,
            $candidate.goal_generation,
            $candidate.lane,
            $candidate.role,
            $candidate.owner_id,
            $candidate.owner_generation,
            $candidate.terminal_adapter,
            $candidate.agent_adapter,
            $candidate.provider_adapter,
            $candidate.coordinator_route_ref,
            $candidate.repair_route_ref,
            $candidate.pr_truth,
            $candidate.acceptance_truth,
            $candidate.continuation_truth,
            $candidate.phase,
            $candidate.content_digest,
            $now,
        ]
    };
}

#[cfg(any(unix, test))]
mod actionable_scheduler;
mod canary_wake;
mod custody_inventory;
#[allow(dead_code)] // Live cmux/HerdR proof remains default-off until upstream contracts ship.
mod delivery_authority;
#[allow(dead_code)] // Some ownership lifecycle operations remain future-facing.
mod delivery_ownership;
#[allow(dead_code)] // Some generic adapter surfaces are exercised only by native dispatch.
mod dispatch;
#[allow(dead_code)] // Enabled with the cross-machine daemon transport canary.
mod durable_custody;
mod importer;
mod inventory;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use delivery_authority::verify_delivery_authority_at;
pub(crate) use delivery_authority::{
    DeliveryAuthorityExpectation, DeliveryAuthorityProbe, DeliveryAuthorityRefusal,
    DeliveryAuthorization, GitHubAuthorityObservation, ProcessIncarnation,
    ReconciliationAuthorization, TerminalAuthorityObservation, TerminalMutationEndpoint,
    verify_delivery_authority, verify_delivery_or_fresh_authority, verify_reconciliation_authority,
};
#[allow(unused_imports)] // Consumed by the later daemon/provider integration slice.
pub(crate) use delivery_ownership::{
    AgentContextChallenge, AgentContextReceipt, AgentOwnershipReceipt, AgentReturnChallenge,
    AgentReturnExpectation, AgentReturnReceipt,
};
#[cfg(test)]
pub(crate) use dispatch::reconciliation_fence_digest;
pub(crate) use dispatch::{
    DeliveryFence, FreshAgentLaunchProfile, FreshAgentProviderLaunchOptions,
    FreshAgentResumeExpectation, ProviderAdapter, ProviderAuthorizationOperation,
    ProviderCapability, ProviderLaunchRequest, ProviderOutcome, StoredProviderRequest,
    WakeConsumerPolicy, WakeDeliveryResult,
};
#[cfg(test)]
pub(crate) use dispatch::{WakeEnvelope, WakeProfileResolver};
#[allow(unused_imports)] // Consumed by the cross-machine transport adapter follow-up.
pub(crate) use durable_custody::{
    AuthenticatedCustodyControl, AuthenticatedCustodyControlReceipt, AuthenticatedCustodyReceipt,
    AuthenticatedCustodySuccessorRebind, AuthenticatedCustodySuccessorReceipt,
    AuthenticatedCustodyTransfer, AuthenticatedProcessedReceipt, CustodyControl,
    CustodyControlReceipt, CustodyEnvelope, CustodyKind, CustodyReceipt, CustodyRelation,
    CustodyStatus, CustodySuccessorRebind, CustodySuccessorReceipt, CustodyTransfer,
    CustodyTransportAuthenticator, InboxAuthority, InboxClaim, ProcessedReceipt, SenderClaim,
    authenticate_custody_control, authenticate_custody_control_receipt,
    authenticate_custody_receipt, authenticate_custody_successor_rebind,
    authenticate_custody_successor_receipt, authenticate_custody_transfer,
    authenticate_processed_receipt,
};
mod lifecycle;
mod native_publication;
#[cfg(test)]
pub(crate) use native_publication::tests::{
    policy as native_publication_test_policy, request as native_publication_test_request,
};
mod observation;
mod ownership_lease;
mod persistence;
mod policy;
mod projection_intents;
pub(crate) use projection_intents::PendingProjectionIntent;
#[allow(dead_code)] // Platform-specific helpers are not used on every target.
mod protected_objects;
mod registry;
#[allow(dead_code)] // Activated through the protected registry in a later phase.
mod route;
mod storage;
#[cfg(any(unix, test))]
pub(crate) use actionable_scheduler::NativeStewardDisposition;
#[cfg(unix)]
pub(crate) use actionable_scheduler::{
    DispatchProbeTargetRecord, MAX_DISPATCH_PROBE_TARGETS, NativeStewardApplyReport,
    dispatch_probe_target_key,
};
#[cfg(test)]
#[allow(unused_imports)] // Some target-specific test builds do not exercise the Unix carrier.
pub(crate) use custody_inventory::CustodyInventoryBinding;
#[cfg(any(unix, test))]
pub(crate) use custody_inventory::verify_custody_inventory_response;
pub(crate) use custody_inventory::{
    CustodyInventoryResolution, CustodyInventoryWireRequest, custody_inventory_request,
    verify_custody_inventory_inbox,
};
use importer::import_report;
#[cfg(test)]
use importer::{candidate, dry_run_report, scan_legacy, validate_legacy_record};
pub use inventory::{LocalWorkInventory, LocalWorkInventoryItem, local_work_inventory};
pub(crate) use lifecycle::deterministic_wake_id;
pub use lifecycle::{ContinuationSet, LifecycleState, WakeIntent};
#[cfg(unix)]
pub(crate) use native_publication::bind_legacy_native_policy;
#[allow(unused_imports)] // Consumed by the CLI/runtime integration follow-up.
pub(crate) use native_publication::{
    ExactProtectedProfileResolver, NativePublicationReport, NativePublicationRequest,
    verify_native_policy_binding, verify_native_policy_binding_for_repository,
};
pub use observation::ShadowPrTarget;
#[cfg(test)]
pub(crate) use ownership_lease::{OwnershipAdoptionProof, OwnershipLeaseFence};
pub(crate) use ownership_lease::{OwnershipAdoptionResult, OwnershipLease, OwnershipLeaseHolder};
pub use persistence::{apply_legacy_snapshot, plan_legacy_snapshot};
pub use policy::RepoPolicy;
pub(crate) use policy::validate_repo_policy;
#[allow(unused_imports)] // The production provider slice consumes this internal boundary.
pub(crate) use protected_objects::{ProtectedObjectKind, ProtectedObjectRecord};
#[cfg(test)]
use registry::{RouteRegistration, validated_route_exists};
use route::{AdapterBindingRecord, RouteProvenanceRecord};
pub use storage::absent_status;
use storage::{
    configure_durable, count, count_where, create_database_file_no_follow, migrate,
    protect_database_file, protect_ledger_directory, schema_version, synchronous_name,
    validate_protected_storage, verify_integrity, verify_open_lineage, verify_supported_schema,
};
#[cfg(test)]
use storage::{
    reconstruct_authentic_v10_schema_for_test, reconstruct_authentic_v11_schema_for_test,
    reconstruct_authentic_v12_schema_for_test,
};
#[cfg(test)]
pub(crate) use tests::ownership_lease_fixture;

/// Error returned by a fail-closed work-ledger operation.
#[derive(Debug)]
pub enum WorkLedgerError {
    /// Filesystem access failed.
    Io(std::io::Error),
    /// `SQLite` rejected an operation or found malformed storage.
    Sql(rusqlite::Error),
    /// A legacy JSON record is malformed.
    Json {
        /// Opaque digest of the source path; raw paths are not exposed.
        source: String,
        /// Parser error.
        error: serde_json::Error,
    },
    /// The database was written by a newer, unsupported implementation.
    UnsupportedSchema(i64),
    /// A numerically overlapping schema belongs to an incompatible ledger lineage.
    ForeignSchemaLineage {
        /// Ambiguous `SQLite` schema number.
        version: i64,
        /// Redacted lineage family.
        lineage: &'static str,
    },
    /// An invariant or generation fence refused a mutation.
    Refused(String),
}

impl Display for WorkLedgerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "work ledger I/O failed: {error}"),
            Self::Sql(error) => write!(formatter, "work ledger SQLite failed: {error}"),
            Self::Json { source, error } => {
                write!(formatter, "legacy source {source} is invalid JSON: {error}")
            }
            Self::UnsupportedSchema(version) => {
                write!(
                    formatter,
                    "unsupported work ledger schema version {version}"
                )
            }
            Self::ForeignSchemaLineage { version, lineage } => write!(
                formatter,
                "work ledger schema version {version} belongs to incompatible {lineage} lineage"
            ),
            Self::Refused(reason) => write!(formatter, "work ledger refused mutation: {reason}"),
        }
    }
}

impl std::error::Error for WorkLedgerError {}

impl From<std::io::Error> for WorkLedgerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for WorkLedgerError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

/// Result type for work-ledger operations.
pub type WorkLedgerResult<T> = Result<T, WorkLedgerError>;

/// A selected, redacted projection of one legacy lifecycle record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportCandidate {
    /// Opaque deterministic work identity.
    work_id: String,
    /// Legacy lifecycle family.
    kind: String,
    /// Canonical repository when present.
    repo: Option<String>,
    /// Pull-request number when present.
    pr: Option<u64>,
    /// Exact head when present.
    head_sha: Option<String>,
    /// Base ref when present.
    base_ref: Option<String>,
    /// Logical goal identity when present.
    goal_id: Option<String>,
    /// Current goal generation.
    goal_generation: u64,
    /// Lane identity, independent of platform policy.
    lane: Option<String>,
    /// Root, coordinator, or child.
    role: String,
    /// Opaque owner identity when present.
    owner_id: Option<String>,
    /// Current owner generation.
    owner_generation: u64,
    /// Terminal runtime adapter kind only; route is stored separately as a digest.
    terminal_adapter: Option<String>,
    /// Agent/session adapter kind only.
    agent_adapter: Option<String>,
    /// Provider-routing adapter kind only.
    provider_adapter: Option<String>,
    /// Opaque coordinator route reference.
    coordinator_route_ref: Option<String>,
    /// Opaque repair route reference.
    repair_route_ref: Option<String>,
    /// Independent pull-request terminal truth.
    pr_truth: String,
    /// Independent product acceptance truth.
    acceptance_truth: String,
    /// Independent continuation terminal truth.
    continuation_truth: String,
    /// Lifecycle phase.
    phase: String,
    /// Opaque digest of the source path and map key.
    source_ref: String,
    /// Digest of the exact legacy bytes or selected map value.
    content_digest: String,
    /// Legacy lifecycle timestamp used only to reconcile compatibility mirrors.
    pub(crate) source_updated_at: Option<String>,
}

/// Stable, non-sensitive ledger summary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LedgerStatus {
    /// Whether the database exists.
    pub exists: bool,
    /// Schema version, or zero when absent.
    pub schema_version: i64,
    /// `SQLite` journal mode.
    pub journal_mode: String,
    /// Durability setting on the inspected connection.
    pub synchronous: String,
    /// Whether foreign-key enforcement is active on the inspected connection.
    pub foreign_keys: String,
    /// Integrity verdict.
    pub integrity: String,
    /// Canonical work-item count.
    pub work_items: u64,
    /// Pending outbox count.
    pub pending_wakes: u64,
    /// Ambiguous outbox count, which requires reconciliation rather than retry.
    pub uncertain_wakes: u64,
    /// Authenticated transition projections waiting for the optional drainer.
    pub pending_projection_intents: u64,
    /// Projection intents isolated after a local digest or identity contradiction.
    pub quarantined_projection_intents: u64,
    /// Imported source count.
    pub imports: u64,
    /// Immutable protected-object metadata rows.
    pub protected_objects: u64,
    /// Durable provider-delivery rows.
    pub provider_deliveries: u64,
    /// Durable agent-ownership rows.
    pub agent_ownership: u64,
    /// Append-only activation ownership epochs.
    pub activation_epochs: u64,
    /// Activation is deliberately unavailable in this phase.
    pub activation_enabled: bool,
    /// Dispatch is deliberately unavailable in this phase.
    pub dispatch_enabled: bool,
}

/// Deterministic import report.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImportReport {
    /// Whether records were committed.
    pub applied: bool,
    /// Operating mode; this phase always reports `shadow`.
    pub mode: String,
    /// Total recognized candidates.
    pub candidates: usize,
    /// Newly inserted candidates.
    pub inserted: usize,
    /// Existing work items refreshed from a changed legacy source.
    pub updated: usize,
    /// Exact prior imports left unchanged.
    pub unchanged: usize,
    /// Counts by lifecycle family.
    pub by_kind: BTreeMap<String, usize>,
    /// Digest of the sorted candidate identities and content digests.
    pub plan_digest: String,
    /// Activation remains false.
    pub activation_enabled: bool,
    /// Dispatch remains false.
    pub dispatch_enabled: bool,
}

/// Machine-global SQLite/WAL work-item ledger.
#[derive(Clone, Debug)]
pub struct WorkLedger {
    path: PathBuf,
}

pub(super) fn validate_candidate(candidate: &ImportCandidate) -> WorkLedgerResult<()> {
    validate_opaque_ref("work_id", &candidate.work_id, "wi")?;
    validate_opaque_ref("source_ref", &candidate.source_ref, "src")?;
    validate_digest("content_digest", &candidate.content_digest)?;
    if !matches!(
        candidate.kind.as_str(),
        "ship_state"
            | "queue_request"
            | "queue_outcome"
            | "recovery"
            | "terminal_handoff"
            | "resume_record"
    ) {
        return Err(WorkLedgerError::Refused(
            "unsupported imported lifecycle kind".to_owned(),
        ));
    }
    if !matches!(candidate.role.as_str(), "root" | "coordinator" | "child") {
        return Err(WorkLedgerError::Refused("unsupported work role".to_owned()));
    }
    if candidate.phase != LifecycleState::ShadowImported.as_str() {
        return Err(WorkLedgerError::Refused(
            "legacy imports must remain shadow imported".to_owned(),
        ));
    }
    if candidate.goal_generation == 0 || candidate.owner_generation == 0 {
        return Err(WorkLedgerError::Refused(
            "goal and owner generations must be positive".to_owned(),
        ));
    }
    for (name, value, prefix) in [
        ("goal_id", candidate.goal_id.as_deref(), "goal"),
        ("owner_id", candidate.owner_id.as_deref(), "owner"),
        (
            "coordinator_route_ref",
            candidate.coordinator_route_ref.as_deref(),
            "route",
        ),
        (
            "repair_route_ref",
            candidate.repair_route_ref.as_deref(),
            "route",
        ),
    ] {
        if let Some(value) = value {
            validate_opaque_ref(name, value, prefix)?;
        }
    }
    if let Some(repo) = &candidate.repo
        && !is_canonical_repo_slug(repo)
    {
        return Err(WorkLedgerError::Refused(
            "imported repository is not canonical owner/repo".to_owned(),
        ));
    }
    if let Some(head) = &candidate.head_sha
        && !is_lower_hex(head, 40)
        && !is_lower_hex(head, 64)
    {
        return Err(WorkLedgerError::Refused(
            "imported head is not a full lowercase object ID".to_owned(),
        ));
    }
    for truth in [
        candidate.pr_truth.as_str(),
        candidate.acceptance_truth.as_str(),
        candidate.continuation_truth.as_str(),
    ] {
        if !matches!(truth, "pending" | "succeeded" | "failed" | "unknown") {
            return Err(WorkLedgerError::Refused(
                "imported terminal truth is unsupported".to_owned(),
            ));
        }
    }
    for (name, value) in [
        ("base_ref", candidate.base_ref.as_deref()),
        ("lane", candidate.lane.as_deref()),
        ("terminal_adapter", candidate.terminal_adapter.as_deref()),
        ("agent_adapter", candidate.agent_adapter.as_deref()),
        ("provider_adapter", candidate.provider_adapter.as_deref()),
    ] {
        if let Some(value) = value {
            validate_token(name, value)?;
        }
    }
    Ok(())
}

fn validate_token(name: &str, value: &str) -> WorkLedgerResult<()> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(WorkLedgerError::Refused(format!("invalid {name}")));
    }
    Ok(())
}

/// Validate the one canonical Linear-style durable workstream handle grammar.
pub(crate) fn validate_workstream_handle(value: &str) -> WorkLedgerResult<()> {
    let Some(number) = value.strip_prefix("GEN-") else {
        return Err(WorkLedgerError::Refused(
            "invalid canonical workstream handle".to_owned(),
        ));
    };
    let valid = !number.is_empty()
        && !number.starts_with('0')
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && value.len() <= 128;
    if !valid {
        return Err(WorkLedgerError::Refused(
            "invalid canonical workstream handle".to_owned(),
        ));
    }
    Ok(())
}

fn is_canonical_repo_slug(value: &str) -> bool {
    let Some((owner, repo)) = value.split_once('/') else {
        return false;
    };
    value == value.to_ascii_lowercase()
        && !repo.contains('/')
        && !owner.is_empty()
        && !repo.is_empty()
        && owner.trim() == owner
        && repo.trim() == repo
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && repo.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn validate_opaque_ref(name: &str, value: &str, prefix: &str) -> WorkLedgerResult<()> {
    let expected = format!("{prefix}_");
    let Some(hash) = value.strip_prefix(&expected) else {
        return Err(WorkLedgerError::Refused(format!("invalid {name}")));
    };
    if !is_lower_hex(hash, 64) {
        return Err(WorkLedgerError::Refused(format!("invalid {name}")));
    }
    Ok(())
}

fn validate_digest(name: &str, value: &str) -> WorkLedgerResult<()> {
    if !is_lower_hex(value, 64) {
        return Err(WorkLedgerError::Refused(format!("invalid {name}")));
    }
    Ok(())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn opaque_path_ref(state_dir: &Path, path: &Path, key: Option<&str>) -> String {
    let relative = path.strip_prefix(state_dir).unwrap_or(path);
    opaque_ref(
        "src",
        &format!("{}#{}", relative.to_string_lossy(), key.unwrap_or("")),
    )
}

fn opaque_ref(prefix: &str, value: &str) -> String {
    format!("{prefix}_{}", digest(value.as_bytes()))
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests;
