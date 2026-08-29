//! Optional, local-first transition projection outbox.
//!
//! Stewardship producers append deterministic transition records and return.
//! A separate, explicitly invoked reconciler may project those records to an
//! external system. No network, model, provider SDK, or execution authority is
//! present in this module.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 1;
const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_NOTE_BYTES: usize = 2_048;
const MAX_REASON_BYTES: usize = 512;
/// Maximum wall-clock duration of one adapter submit/readback attempt.
pub const PROJECTION_CLAIM_LEASE_MS: u64 = 60_000;

/// A transition that may be projected to an external status surface.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    /// Custody moved to another worker or session.
    Handoff,
    /// Progress is waiting on a declared dependency or condition.
    Waiting,
    /// Previously waiting work can now be acted upon.
    Actionable,
    /// A new exact source head became authoritative.
    NewHead,
    /// A merge was proven complete.
    Merge,
    /// The configured closure gate was proven.
    ConfiguredClosure,
}

/// Exact, non-secret evidence used to bind a projected transition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectionEvidence {
    /// Authenticated source or planning revision (40- or 64-digit hex).
    pub source_revision: String,
    /// Exact repository head when the transition is head-specific.
    pub exact_head: Option<String>,
    /// Digest of the local receipt proving the transition.
    pub receipt_sha256: String,
}

impl ProjectionEvidence {
    fn validate(&self) -> Result<(), ProjectionError> {
        validate_hex_identity(&self.source_revision, "source_revision", &[40, 64])?;
        if let Some(head) = &self.exact_head {
            validate_hex_identity(head, "exact_head", &[40, 64])?;
        }
        validate_hex_identity(&self.receipt_sha256, "receipt_sha256", &[64])
    }

    /// Stable digest of the exact evidence tuple.
    #[must_use]
    pub fn identity(&self) -> String {
        canonical_digest(self)
    }
}

/// Caller-owned transition input. `seal` validates and derives stable IDs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransitionDraft {
    /// Stable Linear-style workstream handle or another non-secret handle.
    pub workstream_id: String,
    /// Strictly increasing sequence within the workstream.
    pub sequence: u64,
    /// State transition being projected.
    pub kind: TransitionKind,
    /// Exact evidence for this transition.
    pub evidence: ProjectionEvidence,
    /// Older transition replaced by this one, if any.
    pub supersedes_transition_id: Option<String>,
    /// Optional human-readable context. Known credential shapes are redacted.
    pub note: Option<String>,
}

impl TransitionDraft {
    /// Validate, redact, and derive stable transition/evidence identities.
    pub fn seal(self) -> Result<ProjectedTransition, ProjectionError> {
        validate_workstream_id(&self.workstream_id)?;
        if self.sequence == 0 {
            return Err(ProjectionError::Invalid(
                "transition sequence must be positive".to_owned(),
            ));
        }
        self.evidence.validate()?;
        if let Some(id) = &self.supersedes_transition_id {
            validate_hex_identity(id, "supersedes_transition_id", &[64])?;
        }
        let note = self.note.map(|value| redact_note(&value));
        let evidence_identity = self.evidence.identity();
        let transition_id = canonical_digest(&TransitionIdentity {
            schema_version: SCHEMA_VERSION,
            workstream_id: &self.workstream_id,
            sequence: self.sequence,
            kind: self.kind,
            evidence_identity: &evidence_identity,
            supersedes_transition_id: &self.supersedes_transition_id,
            note: &note,
        });
        Ok(ProjectedTransition {
            schema_version: SCHEMA_VERSION,
            transition_id,
            workstream_id: self.workstream_id,
            sequence: self.sequence,
            kind: self.kind,
            evidence: self.evidence,
            evidence_identity,
            supersedes_transition_id: self.supersedes_transition_id,
            note,
        })
    }
}

#[derive(Serialize)]
struct TransitionIdentity<'a> {
    schema_version: u32,
    workstream_id: &'a str,
    sequence: u64,
    kind: TransitionKind,
    evidence_identity: &'a str,
    supersedes_transition_id: &'a Option<String>,
    note: &'a Option<String>,
}

/// Immutable transition supplied to an external adapter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectedTransition {
    /// On-disk and adapter contract version.
    pub schema_version: u32,
    /// Stable idempotency key for this exact transition.
    pub transition_id: String,
    /// Stable workstream handle.
    pub workstream_id: String,
    /// Strict workstream ordering key.
    pub sequence: u64,
    /// Transition classification.
    pub kind: TransitionKind,
    /// Exact source evidence.
    pub evidence: ProjectionEvidence,
    /// Stable digest of `evidence`.
    pub evidence_identity: String,
    /// Older pending transition replaced by this one.
    pub supersedes_transition_id: Option<String>,
    /// Redacted, bounded context.
    pub note: Option<String>,
}

impl ProjectedTransition {
    fn validate_identity(&self) -> Result<(), ProjectionError> {
        validate_workstream_id(&self.workstream_id)?;
        if self.schema_version != SCHEMA_VERSION || self.sequence == 0 {
            return Err(ProjectionError::Corrupt(
                "queued transition schema or sequence is invalid".to_owned(),
            ));
        }
        self.evidence.validate()?;
        if self
            .note
            .as_ref()
            .is_some_and(|note| note.len() > MAX_NOTE_BYTES)
        {
            return Err(ProjectionError::Corrupt(
                "queued transition note exceeds its bound".to_owned(),
            ));
        }
        if let Some(id) = &self.supersedes_transition_id {
            validate_hex_identity(id, "supersedes_transition_id", &[64])?;
        }
        if self.evidence.identity() != self.evidence_identity {
            return Err(ProjectionError::Corrupt(
                "queued transition evidence identity is invalid".to_owned(),
            ));
        }
        let expected = canonical_digest(&TransitionIdentity {
            schema_version: self.schema_version,
            workstream_id: &self.workstream_id,
            sequence: self.sequence,
            kind: self.kind,
            evidence_identity: &self.evidence_identity,
            supersedes_transition_id: &self.supersedes_transition_id,
            note: &self.note,
        });
        if expected != self.transition_id {
            return Err(ProjectionError::Corrupt(
                "queued transition identity is invalid".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Result of adding a transition to the local outbox.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueOutcome {
    /// Projection is disabled; no validation, I/O, or side effect occurred.
    Disabled,
    /// A new durable record was appended.
    Queued,
    /// The byte-identical transition was already durable.
    AlreadyQueued,
}

/// Adapter result from an idempotent submission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitReceipt {
    /// Adapter-specific stable object/comment/event identity.
    pub external_id: String,
    /// Must echo `ProjectedTransition::transition_id`.
    pub idempotency_key: String,
}

/// Adapter readback proving the projected state is externally observable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionReadback {
    /// Stable transition identity found externally.
    pub transition_id: String,
    /// Stable exact-evidence identity found externally.
    pub evidence_identity: String,
}

/// Sanitized adapter failure. Secrets must not be placed in `reason`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterFailure {
    /// Bounded diagnostic class/message; credential shapes are redacted.
    pub reason: String,
    /// Whether reconciliation should retain the item for another attempt.
    pub retryable: bool,
}

/// External adapter boundary. Implementations may use Linear, a file bridge,
/// or another system, but execute only when a caller explicitly reconciles.
pub trait TransitionProjectionAdapter {
    /// Idempotently submit using `transition.transition_id` as the key. The
    /// complete submit/readback operation must be bounded below
    /// [`PROJECTION_CLAIM_LEASE_MS`].
    fn submit(&mut self, transition: &ProjectedTransition)
    -> Result<SubmitReceipt, AdapterFailure>;

    /// Read the external object back before Shipyard records an acknowledgement.
    fn readback(&mut self, receipt: &SubmitReceipt) -> Result<ProjectionReadback, AdapterFailure>;
}

/// Result of one explicit adapter reconciliation step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReconcileOutcome {
    /// Projection is disabled and the adapter was not called.
    Disabled,
    /// Nothing is currently eligible.
    Idle,
    /// One transition was read back and durably acknowledged.
    Acknowledged {
        /// Stable transition identity acknowledged externally.
        transition_id: String,
    },
    /// One transition remains queued for retry.
    RetryQueued {
        /// Stable transition identity retained for retry.
        transition_id: String,
        /// Earliest retry time.
        retry_at_unix_ms: u64,
    },
    /// A permanent adapter refusal was recorded and will not auto-retry.
    Refused {
        /// Stable transition identity permanently refused.
        transition_id: String,
    },
}

/// Durable status of one outbox transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransitionStatus {
    /// Immutable transition.
    pub transition: ProjectedTransition,
    /// Number of persisted adapter attempts.
    pub attempts: u32,
    /// Earliest time another attempt may run.
    pub retry_at_unix_ms: u64,
    /// Whether exact readback was acknowledged.
    pub acknowledged: bool,
    /// Whether the item was permanently refused.
    pub refused: bool,
    /// Whether a later transition supersedes it.
    pub superseded: bool,
    /// Active adapter claim deadline, or zero when unclaimed.
    pub claim_until_unix_ms: u64,
    claim_id: Option<String>,
}

/// Optional crash-consistent transition projection outbox.
#[derive(Debug)]
pub struct TransitionOutbox {
    storage: Option<Storage>,
}

#[derive(Debug)]
struct Storage {
    root: PathBuf,
    log_path: PathBuf,
    lock_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "record", rename_all = "snake_case")]
enum LogRecord {
    Enqueue {
        transition: ProjectedTransition,
    },
    Attempt {
        transition_id: String,
        claim_id: String,
        attempt: u32,
        retry_at_unix_ms: u64,
        retryable: bool,
        reason: String,
    },
    Ack {
        transition_id: String,
        claim_id: String,
        evidence_identity: String,
        external_id_sha256: String,
    },
    Claim {
        transition_id: String,
        claim_id: String,
        claim_until_unix_ms: u64,
    },
}

/// Outbox failures never grant or change stewardship execution authority.
#[derive(Debug)]
pub enum ProjectionError {
    /// Invalid or contradictory local input/state.
    Invalid(String),
    /// Local persistence failed.
    Io(std::io::Error),
    /// Durable log contents were contradictory or malformed.
    Corrupt(String),
}

impl Display for ProjectionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Corrupt(message) => formatter.write_str(message),
            Self::Io(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for ProjectionError {}

impl From<std::io::Error> for ProjectionError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl TransitionOutbox {
    /// Construct a zero-effect disabled projection boundary.
    #[must_use]
    pub const fn disabled() -> Self {
        Self { storage: None }
    }

    /// Open a private local outbox, repairing only an uncommitted partial tail.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ProjectionError> {
        let root = root.into();
        validate_root(&root)?;
        if !root.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                match fs::DirBuilder::new().mode(0o700).create(&root) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            #[cfg(not(unix))]
            match fs::create_dir(&root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        ensure_private_directory(&root)?;
        let storage = Storage {
            log_path: root.join("transitions.ndjson"),
            lock_path: root.join("outbox.lock"),
            root,
        };
        storage.with_exclusive_log(|log| {
            repair_partial_tail(log)?;
            Ok(())
        })?;
        Ok(Self {
            storage: Some(storage),
        })
    }

    /// Durably enqueue one transition. Disabled mode performs no validation.
    pub fn enqueue(&self, draft: TransitionDraft) -> Result<EnqueueOutcome, ProjectionError> {
        let Some(storage) = &self.storage else {
            return Ok(EnqueueOutcome::Disabled);
        };
        let transition = draft.seal()?;
        storage.with_exclusive_log(|log| {
            let records = read_records(log, true)?;
            let statuses = materialize(&records)?;
            if let Some(existing) = statuses.get(&transition.transition_id) {
                return if existing.transition == transition {
                    Ok(EnqueueOutcome::AlreadyQueued)
                } else {
                    Err(ProjectionError::Corrupt(
                        "transition identity collided with different content".to_owned(),
                    ))
                };
            }
            let workstream = statuses
                .values()
                .filter(|status| status.transition.workstream_id == transition.workstream_id);
            let max_sequence = workstream
                .clone()
                .map(|status| status.transition.sequence)
                .max();
            if max_sequence.is_some_and(|sequence| transition.sequence <= sequence) {
                return Err(ProjectionError::Invalid(format!(
                    "workstream {} transition sequence {} is not newer than durable sequence {}",
                    transition.workstream_id,
                    transition.sequence,
                    max_sequence.unwrap_or_default()
                )));
            }
            if let Some(supersedes) = &transition.supersedes_transition_id {
                let prior = statuses.get(supersedes).ok_or_else(|| {
                    ProjectionError::Invalid(
                        "superseded transition is not present in this outbox".to_owned(),
                    )
                })?;
                if prior.transition.workstream_id != transition.workstream_id
                    || prior.transition.sequence >= transition.sequence
                {
                    return Err(ProjectionError::Invalid(
                        "supersession must name an older transition in the same workstream"
                            .to_owned(),
                    ));
                }
                if prior.claim_until_unix_ms > current_unix_ms()? {
                    return Err(ProjectionError::Invalid(
                        "superseded transition is actively claimed for external projection"
                            .to_owned(),
                    ));
                }
            }
            append_record(log, &LogRecord::Enqueue { transition })?;
            Ok(EnqueueOutcome::Queued)
        })
    }

    /// Return a deterministic snapshot ordered by workstream, sequence, and ID.
    pub fn snapshot(&self) -> Result<Vec<TransitionStatus>, ProjectionError> {
        let Some(storage) = &self.storage else {
            return Ok(Vec::new());
        };
        storage.with_exclusive_log(|log| {
            let mut statuses = materialize(&read_records(log, true)?)?
                .into_values()
                .collect::<Vec<_>>();
            statuses.sort_by(|left, right| {
                left.transition
                    .workstream_id
                    .cmp(&right.transition.workstream_id)
                    .then(left.transition.sequence.cmp(&right.transition.sequence))
                    .then(
                        left.transition
                            .transition_id
                            .cmp(&right.transition.transition_id),
                    )
            });
            Ok(statuses)
        })
    }

    /// Reconcile at most one eligible transition through an external adapter.
    /// This method is never called implicitly by stewardship.
    pub fn reconcile_one<A: TransitionProjectionAdapter>(
        &self,
        adapter: &mut A,
        now_unix_ms: u64,
    ) -> Result<ReconcileOutcome, ProjectionError> {
        let Some(_) = &self.storage else {
            return Ok(ReconcileOutcome::Disabled);
        };
        let claim_now_unix_ms = current_unix_ms()?;
        let mut claimed = None;
        for status in self.snapshot()?.into_iter().filter(|status| {
            !status.acknowledged
                && !status.refused
                && !status.superseded
                && status.retry_at_unix_ms <= now_unix_ms
                && status.claim_until_unix_ms <= claim_now_unix_ms
        }) {
            let attempt = status.attempts.saturating_add(1);
            if let Some(claim_id) =
                self.claim(&status.transition.transition_id, attempt, now_unix_ms)?
            {
                claimed = Some((status.transition, attempt, claim_id));
                break;
            }
        }
        let Some((transition, attempt, claim_id)) = claimed else {
            return Ok(ReconcileOutcome::Idle);
        };
        let receipt = match adapter.submit(&transition) {
            Ok(receipt) => receipt,
            Err(failure) => {
                return self.record_failure(&transition, &claim_id, attempt, now_unix_ms, &failure);
            }
        };
        if receipt.idempotency_key != transition.transition_id || receipt.external_id.is_empty() {
            return self.record_failure(
                &transition,
                &claim_id,
                attempt,
                now_unix_ms,
                &AdapterFailure {
                    reason: "adapter returned a mismatched idempotency receipt".to_owned(),
                    retryable: false,
                },
            );
        }
        let readback = match adapter.readback(&receipt) {
            Ok(readback) => readback,
            Err(failure) => {
                return self.record_failure(&transition, &claim_id, attempt, now_unix_ms, &failure);
            }
        };
        if readback.transition_id != transition.transition_id
            || readback.evidence_identity != transition.evidence_identity
        {
            return self.record_failure(
                &transition,
                &claim_id,
                attempt,
                now_unix_ms,
                &AdapterFailure {
                    reason: "external readback did not match exact transition evidence".to_owned(),
                    retryable: true,
                },
            );
        }
        self.append_checked(&LogRecord::Ack {
            transition_id: transition.transition_id.clone(),
            claim_id,
            evidence_identity: transition.evidence_identity,
            external_id_sha256: digest_bytes(receipt.external_id.as_bytes()),
        })?;
        Ok(ReconcileOutcome::Acknowledged {
            transition_id: transition.transition_id,
        })
    }

    fn record_failure(
        &self,
        transition: &ProjectedTransition,
        claim_id: &str,
        attempt: u32,
        now_unix_ms: u64,
        failure: &AdapterFailure,
    ) -> Result<ReconcileOutcome, ProjectionError> {
        let retry_at_unix_ms = if failure.retryable {
            now_unix_ms.saturating_add(retry_delay_ms(attempt))
        } else {
            0
        };
        self.append_checked(&LogRecord::Attempt {
            transition_id: transition.transition_id.clone(),
            claim_id: claim_id.to_owned(),
            attempt,
            retry_at_unix_ms,
            retryable: failure.retryable,
            reason: redact_reason(&failure.reason),
        })?;
        Ok(if failure.retryable {
            ReconcileOutcome::RetryQueued {
                transition_id: transition.transition_id.clone(),
                retry_at_unix_ms,
            }
        } else {
            ReconcileOutcome::Refused {
                transition_id: transition.transition_id.clone(),
            }
        })
    }

    fn claim(
        &self,
        transition_id: &str,
        attempt: u32,
        now_unix_ms: u64,
    ) -> Result<Option<String>, ProjectionError> {
        let storage = self.storage.as_ref().ok_or_else(|| {
            ProjectionError::Corrupt("disabled outbox cannot claim records".to_owned())
        })?;
        storage.with_exclusive_log(|log| {
            let statuses = materialize(&read_records(log, true)?)?;
            let claim_now_unix_ms = current_unix_ms()?;
            let Some(status) = statuses.get(transition_id) else {
                return Err(ProjectionError::Corrupt(
                    "claim names an unknown transition".to_owned(),
                ));
            };
            if status.acknowledged
                || status.refused
                || status.superseded
                || status.retry_at_unix_ms > now_unix_ms
                || status.claim_until_unix_ms > claim_now_unix_ms
                || attempt != status.attempts + 1
            {
                return Ok(None);
            }
            let claim_id =
                digest_bytes(format!("{transition_id}:{attempt}:{now_unix_ms}").as_bytes());
            append_record(
                log,
                &LogRecord::Claim {
                    transition_id: transition_id.to_owned(),
                    claim_id: claim_id.clone(),
                    claim_until_unix_ms: claim_now_unix_ms
                        .saturating_add(PROJECTION_CLAIM_LEASE_MS),
                },
            )?;
            Ok(Some(claim_id))
        })
    }

    fn append_checked(&self, record: &LogRecord) -> Result<(), ProjectionError> {
        let storage = self.storage.as_ref().ok_or_else(|| {
            ProjectionError::Corrupt("disabled outbox cannot append records".to_owned())
        })?;
        storage.with_exclusive_log(|log| {
            let statuses = materialize(&read_records(log, true)?)?;
            match record {
                LogRecord::Attempt {
                    transition_id,
                    claim_id,
                    attempt,
                    ..
                } => {
                    let status = statuses.get(transition_id).ok_or_else(|| {
                        ProjectionError::Corrupt("attempt names an unknown transition".to_owned())
                    })?;
                    if status.acknowledged
                        || status.refused
                        || status.claim_id.as_deref() != Some(claim_id)
                        || *attempt != status.attempts + 1
                    {
                        return Err(ProjectionError::Corrupt(
                            "attempt does not follow durable transition state".to_owned(),
                        ));
                    }
                }
                LogRecord::Ack {
                    transition_id,
                    claim_id,
                    evidence_identity,
                    ..
                } => {
                    let status = statuses.get(transition_id).ok_or_else(|| {
                        ProjectionError::Corrupt("ack names an unknown transition".to_owned())
                    })?;
                    if status.transition.evidence_identity != *evidence_identity
                        || status.claim_id.as_deref() != Some(claim_id)
                    {
                        return Err(ProjectionError::Corrupt(
                            "ack evidence identity differs from queued transition".to_owned(),
                        ));
                    }
                    if status.acknowledged {
                        return Ok(());
                    }
                }
                LogRecord::Enqueue { .. } => {
                    return Err(ProjectionError::Corrupt(
                        "enqueue must use the ordering-aware append path".to_owned(),
                    ));
                }
                LogRecord::Claim { .. } => {
                    return Err(ProjectionError::Corrupt(
                        "claim must use the eligibility-aware append path".to_owned(),
                    ));
                }
            }
            append_record(log, record)
        })
    }
}

impl Storage {
    fn with_exclusive_log<T>(
        &self,
        operation: impl FnOnce(&mut File) -> Result<T, ProjectionError>,
    ) -> Result<T, ProjectionError> {
        ensure_private_directory(&self.root)?;
        let lock = open_private_file(&self.lock_path, false)?;
        lock.lock_exclusive()?;
        let result = (|| {
            let mut log = open_private_file(&self.log_path, true)?;
            #[cfg(unix)]
            File::open(&self.root)?.sync_all()?;
            repair_partial_tail(&mut log)?;
            operation(&mut log)
        })();
        FileExt::unlock(&lock)?;
        result
    }
}

fn append_record(log: &mut File, record: &LogRecord) -> Result<(), ProjectionError> {
    let mut bytes = serde_json::to_vec(record).map_err(|error| {
        ProjectionError::Corrupt(format!("could not encode outbox record: {error}"))
    })?;
    bytes.push(b'\n');
    if log.metadata()?.len().saturating_add(bytes.len() as u64) > MAX_LOG_BYTES {
        return Err(ProjectionError::Invalid(
            "transition outbox reached its bounded log limit".to_owned(),
        ));
    }
    log.seek(SeekFrom::End(0))?;
    log.write_all(&bytes)?;
    log.sync_all()?;
    Ok(())
}

fn read_records(log: &mut File, repair: bool) -> Result<Vec<LogRecord>, ProjectionError> {
    if repair {
        repair_partial_tail(log)?;
    }
    log.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    log.take(MAX_LOG_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LOG_BYTES {
        return Err(ProjectionError::Corrupt(
            "transition outbox exceeds its bounded log limit".to_owned(),
        ));
    }
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line).map_err(|error| {
                ProjectionError::Corrupt(format!("transition outbox record is invalid: {error}"))
            })
        })
        .collect()
}

fn repair_partial_tail(log: &mut File) -> Result<(), ProjectionError> {
    let length = log.metadata()?.len();
    if length == 0 {
        return Ok(());
    }
    if length > MAX_LOG_BYTES {
        return Err(ProjectionError::Corrupt(
            "transition outbox exceeds its bounded log limit".to_owned(),
        ));
    }
    log.seek(SeekFrom::Start(0))?;
    let capacity = usize::try_from(length).map_err(|_| {
        ProjectionError::Corrupt("transition outbox length exceeds this platform".to_owned())
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    log.read_to_end(&mut bytes)?;
    if bytes.last() == Some(&b'\n') {
        return Ok(());
    }
    let committed_length = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    log.set_len(committed_length as u64)?;
    log.sync_all()?;
    Ok(())
}

fn materialize(
    records: &[LogRecord],
) -> Result<BTreeMap<String, TransitionStatus>, ProjectionError> {
    let mut statuses = BTreeMap::new();
    let mut superseded = BTreeSet::new();
    for record in records {
        match record {
            LogRecord::Enqueue { transition } => {
                materialize_enqueue(&mut statuses, &mut superseded, transition)?;
            }
            LogRecord::Claim {
                transition_id,
                claim_id,
                claim_until_unix_ms,
            } => {
                let status = statuses.get_mut(transition_id).ok_or_else(|| {
                    ProjectionError::Corrupt("claim precedes transition enqueue".to_owned())
                })?;
                if status.acknowledged
                    || status.refused
                    || *claim_until_unix_ms == 0
                    || (status.claim_id.is_some()
                        && *claim_until_unix_ms <= status.claim_until_unix_ms)
                {
                    return Err(ProjectionError::Corrupt(
                        "claim record contradicts durable state".to_owned(),
                    ));
                }
                status.claim_id = Some(claim_id.clone());
                status.claim_until_unix_ms = *claim_until_unix_ms;
            }
            LogRecord::Attempt {
                transition_id,
                claim_id,
                attempt,
                retry_at_unix_ms,
                retryable,
                ..
            } => {
                let status = statuses.get_mut(transition_id).ok_or_else(|| {
                    ProjectionError::Corrupt("attempt precedes transition enqueue".to_owned())
                })?;
                if status.acknowledged
                    || status.refused
                    || status.claim_id.as_deref() != Some(claim_id)
                    || *attempt != status.attempts + 1
                {
                    return Err(ProjectionError::Corrupt(
                        "attempt record contradicts durable state".to_owned(),
                    ));
                }
                status.attempts = *attempt;
                status.retry_at_unix_ms = *retry_at_unix_ms;
                status.refused = !retryable;
                status.claim_id = None;
                status.claim_until_unix_ms = 0;
            }
            LogRecord::Ack {
                transition_id,
                claim_id,
                evidence_identity,
                ..
            } => {
                let status = statuses.get_mut(transition_id).ok_or_else(|| {
                    ProjectionError::Corrupt("ack precedes transition enqueue".to_owned())
                })?;
                if status.transition.evidence_identity != *evidence_identity
                    || status.refused
                    || status.claim_id.as_deref() != Some(claim_id)
                {
                    return Err(ProjectionError::Corrupt(
                        "ack contradicts queued transition evidence or refusal".to_owned(),
                    ));
                }
                status.acknowledged = true;
                status.retry_at_unix_ms = 0;
                status.claim_id = None;
                status.claim_until_unix_ms = 0;
            }
        }
    }
    for id in superseded {
        let status = statuses.get_mut(&id).ok_or_else(|| {
            ProjectionError::Corrupt("supersession names an unknown transition".to_owned())
        })?;
        status.superseded = true;
    }
    Ok(statuses)
}

fn materialize_enqueue(
    statuses: &mut BTreeMap<String, TransitionStatus>,
    superseded: &mut BTreeSet<String>,
    transition: &ProjectedTransition,
) -> Result<(), ProjectionError> {
    transition.validate_identity()?;
    if statuses.contains_key(&transition.transition_id) {
        return Err(ProjectionError::Corrupt(
            "duplicate transition enqueue record".to_owned(),
        ));
    }
    if statuses.values().any(|status| {
        status.transition.workstream_id == transition.workstream_id
            && status.transition.sequence >= transition.sequence
    }) {
        return Err(ProjectionError::Corrupt(
            "durable workstream transition ordering is invalid".to_owned(),
        ));
    }
    if let Some(prior) = &transition.supersedes_transition_id {
        let prior_status = statuses.get(prior).ok_or_else(|| {
            ProjectionError::Corrupt("supersession precedes its transition".to_owned())
        })?;
        if prior_status.transition.workstream_id != transition.workstream_id
            || prior_status.transition.sequence >= transition.sequence
        {
            return Err(ProjectionError::Corrupt(
                "durable supersession crosses workstreams or ordering".to_owned(),
            ));
        }
        superseded.insert(prior.clone());
    }
    statuses.insert(
        transition.transition_id.clone(),
        TransitionStatus {
            transition: transition.clone(),
            attempts: 0,
            retry_at_unix_ms: 0,
            acknowledged: false,
            refused: false,
            superseded: false,
            claim_until_unix_ms: 0,
            claim_id: None,
        },
    );
    Ok(())
}

fn retry_delay_ms(attempt: u32) -> u64 {
    1_000_u64.saturating_mul(2_u64.saturating_pow(attempt.saturating_sub(1).min(10)))
}

fn current_unix_ms() -> Result<u64, ProjectionError> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ProjectionError::Invalid("system clock precedes Unix epoch".to_owned()))?;
    u64::try_from(duration.as_millis())
        .map_err(|_| ProjectionError::Invalid("system clock exceeds outbox range".to_owned()))
}

fn validate_root(root: &Path) -> Result<(), ProjectionError> {
    if root.file_name().is_none()
        || root
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(ProjectionError::Invalid(
            "transition outbox root must be a lexically normal path".to_owned(),
        ));
    }
    if fs::symlink_metadata(root).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ProjectionError::Invalid(
            "transition outbox root must not be a symlink".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), ProjectionError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ProjectionError::Invalid(
            "transition outbox root is not a real directory".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != nix::unistd::Uid::effective().as_raw() {
            return Err(ProjectionError::Invalid(
                "transition outbox root is not owned by the current user".to_owned(),
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn open_private_file(path: &Path, append: bool) -> Result<File, ProjectionError> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ProjectionError::Invalid(format!(
            "transition outbox file must not be a symlink: {}",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).append(append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW);
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != nix::unistd::Uid::effective().as_raw() {
            return Err(ProjectionError::Invalid(
                "transition outbox file is unsafe".to_owned(),
            ));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn validate_workstream_id(value: &str) -> Result<(), ProjectionError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(ProjectionError::Invalid(
            "workstream_id must be a bounded non-secret stable handle".to_owned(),
        ));
    }
    Ok(())
}

fn validate_hex_identity(
    value: &str,
    field: &str,
    lengths: &[usize],
) -> Result<(), ProjectionError> {
    if !lengths.contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ProjectionError::Invalid(format!(
            "{field} must be a lowercase hexadecimal identity of an allowed length"
        )));
    }
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(ProjectionError::Invalid(format!(
            "{field} must use canonical lowercase hexadecimal"
        )));
    }
    Ok(())
}

fn redact_note(value: &str) -> String {
    let bounded = truncate_utf8(value, MAX_NOTE_BYTES);
    let mut redact_next = false;
    let mut redacted = Vec::new();
    for word in bounded.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        let secret_shape = word.starts_with("ghp_")
            || word.starts_with("github_pat_")
            || lower.starts_with("token=")
            || lower.starts_with("password=")
            || lower.starts_with("authorization:")
            || lower.starts_with("private_key=")
            || lower.starts_with("secret=")
            || lower == "bearer";
        if redact_next || secret_shape {
            redacted.push("[REDACTED]");
        } else {
            redacted.push(word);
        }
        redact_next = matches!(lower.as_str(), "bearer" | "authorization:");
    }
    redacted.join(" ")
}

fn redact_reason(value: &str) -> String {
    truncate_utf8(&redact_note(value), MAX_REASON_BYTES)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn canonical_digest(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("canonical identity structures are serializable");
    digest_bytes(&bytes)
}

fn digest_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use super::*;

    fn draft(sequence: u64, kind: TransitionKind) -> TransitionDraft {
        TransitionDraft {
            workstream_id: "GEN-14".to_owned(),
            sequence,
            kind,
            evidence: ProjectionEvidence {
                source_revision: "a".repeat(64),
                exact_head: Some("b".repeat(40)),
                receipt_sha256: format!("{sequence:064x}"),
            },
            supersedes_transition_id: None,
            note: Some("ready".to_owned()),
        }
    }

    #[derive(Default)]
    struct Adapter {
        calls: Arc<AtomicUsize>,
        failures: usize,
        wrong_readback: bool,
        last_evidence_identity: Option<String>,
    }

    impl TransitionProjectionAdapter for Adapter {
        fn submit(
            &mut self,
            transition: &ProjectedTransition,
        ) -> Result<SubmitReceipt, AdapterFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.failures > 0 {
                self.failures -= 1;
                return Err(AdapterFailure {
                    reason: "token=secret temporary outage".to_owned(),
                    retryable: true,
                });
            }
            self.last_evidence_identity = Some(transition.evidence_identity.clone());
            Ok(SubmitReceipt {
                external_id: "linear-comment-1".to_owned(),
                idempotency_key: transition.transition_id.clone(),
            })
        }

        fn readback(
            &mut self,
            receipt: &SubmitReceipt,
        ) -> Result<ProjectionReadback, AdapterFailure> {
            Ok(ProjectionReadback {
                transition_id: receipt.idempotency_key.clone(),
                evidence_identity: if self.wrong_readback {
                    "0".repeat(64)
                } else {
                    self.last_evidence_identity
                        .clone()
                        .expect("submitted evidence")
                },
            })
        }
    }

    #[test]
    fn disabled_mode_has_zero_effect_and_never_calls_adapter() {
        let outbox = TransitionOutbox::disabled();
        let invalid = TransitionDraft {
            workstream_id: String::new(),
            ..draft(1, TransitionKind::Waiting)
        };
        assert_eq!(outbox.enqueue(invalid).unwrap(), EnqueueOutcome::Disabled);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut adapter = Adapter {
            calls: Arc::clone(&calls),
            ..Adapter::default()
        };
        assert_eq!(
            outbox.reconcile_one(&mut adapter, 0).unwrap(),
            ReconcileOutcome::Disabled
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn identities_are_stable_and_notes_are_redacted() {
        let mut input = draft(1, TransitionKind::Handoff);
        input.note = Some("token=abc ghp_secret Bearer raw-secret safe".to_owned());
        let first = input.clone().seal().unwrap();
        let second = input.seal().unwrap();
        assert_eq!(first.transition_id, second.transition_id);
        assert_eq!(first.evidence_identity, second.evidence_identity);
        assert_eq!(
            first.note.as_deref(),
            Some("[REDACTED] [REDACTED] [REDACTED] [REDACTED] safe")
        );
        assert!(!serde_json::to_string(&first).unwrap().contains("secret"));
    }

    #[test]
    fn every_transition_kind_has_a_stable_wire_name() {
        let names = [
            (TransitionKind::Handoff, "\"handoff\""),
            (TransitionKind::Waiting, "\"waiting\""),
            (TransitionKind::Actionable, "\"actionable\""),
            (TransitionKind::NewHead, "\"new_head\""),
            (TransitionKind::Merge, "\"merge\""),
            (TransitionKind::ConfiguredClosure, "\"configured_closure\""),
        ];
        for (kind, name) in names {
            assert_eq!(serde_json::to_string(&kind).unwrap(), name);
        }
    }

    #[test]
    fn restart_replays_pending_and_exact_readback_ack_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let transition = draft(1, TransitionKind::Handoff).seal().unwrap();
        {
            let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
            assert_eq!(
                outbox.enqueue(draft(1, TransitionKind::Handoff)).unwrap(),
                EnqueueOutcome::Queued
            );
        }
        let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
        let mut adapter = Adapter::default();
        assert_eq!(
            outbox.reconcile_one(&mut adapter, 1).unwrap(),
            ReconcileOutcome::Acknowledged {
                transition_id: transition.transition_id.clone()
            }
        );
        assert_eq!(
            outbox.reconcile_one(&mut adapter, 1).unwrap(),
            ReconcileOutcome::Idle
        );
        assert!(outbox.snapshot().unwrap()[0].acknowledged);
    }

    #[test]
    fn retry_and_readback_mismatch_remain_queued_with_backoff() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
        outbox.enqueue(draft(1, TransitionKind::Waiting)).unwrap();
        let mut adapter = Adapter {
            failures: 1,
            ..Adapter::default()
        };
        assert!(matches!(
            outbox.reconcile_one(&mut adapter, 100).unwrap(),
            ReconcileOutcome::RetryQueued {
                retry_at_unix_ms: 1_100,
                ..
            }
        ));
        assert_eq!(
            outbox.reconcile_one(&mut adapter, 1_099).unwrap(),
            ReconcileOutcome::Idle
        );
        adapter.wrong_readback = true;
        assert!(matches!(
            outbox.reconcile_one(&mut adapter, 1_100).unwrap(),
            ReconcileOutcome::RetryQueued {
                retry_at_unix_ms: 3_100,
                ..
            }
        ));
        let bytes = fs::read(temp.path().join("outbox/transitions.ndjson")).unwrap();
        assert!(!String::from_utf8(bytes).unwrap().contains("secret"));
    }

    #[test]
    fn ordering_and_supersession_skip_obsolete_pending_state() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
        let first = draft(1, TransitionKind::Waiting).seal().unwrap();
        outbox.enqueue(draft(1, TransitionKind::Waiting)).unwrap();
        assert!(
            outbox
                .enqueue(draft(1, TransitionKind::Actionable))
                .is_err()
        );
        let mut next = draft(2, TransitionKind::Actionable);
        next.supersedes_transition_id = Some(first.transition_id);
        outbox.enqueue(next).unwrap();
        let snapshot = outbox.snapshot().unwrap();
        assert!(snapshot[0].superseded);
        let mut adapter = Adapter::default();
        let result = outbox.reconcile_one(&mut adapter, 0).unwrap();
        assert!(matches!(result, ReconcileOutcome::Acknowledged { .. }));
        assert_eq!(outbox.snapshot().unwrap()[1].transition.sequence, 2);
    }

    #[test]
    fn active_claim_blocks_concurrent_supersession_until_exact_ack() {
        struct SupersedingAdapter<'a> {
            outbox: &'a TransitionOutbox,
            blocked: bool,
            evidence_identity: Option<String>,
        }

        impl TransitionProjectionAdapter for SupersedingAdapter<'_> {
            fn submit(
                &mut self,
                transition: &ProjectedTransition,
            ) -> Result<SubmitReceipt, AdapterFailure> {
                let mut newer = draft(2, TransitionKind::Actionable);
                newer.supersedes_transition_id = Some(transition.transition_id.clone());
                self.blocked = self.outbox.enqueue(newer).is_err();
                self.evidence_identity = Some(transition.evidence_identity.clone());
                Ok(SubmitReceipt {
                    external_id: "external-claimed".to_owned(),
                    idempotency_key: transition.transition_id.clone(),
                })
            }

            fn readback(
                &mut self,
                receipt: &SubmitReceipt,
            ) -> Result<ProjectionReadback, AdapterFailure> {
                Ok(ProjectionReadback {
                    transition_id: receipt.idempotency_key.clone(),
                    evidence_identity: self.evidence_identity.clone().unwrap(),
                })
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
        outbox.enqueue(draft(1, TransitionKind::Waiting)).unwrap();
        let mut adapter = SupersedingAdapter {
            outbox: &outbox,
            blocked: false,
            evidence_identity: None,
        };
        assert!(matches!(
            outbox.reconcile_one(&mut adapter, 0).unwrap(),
            ReconcileOutcome::Acknowledged { .. }
        ));
        assert!(adapter.blocked);
    }

    #[test]
    fn expired_claim_is_reclaimed_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let outbox = TransitionOutbox::open(&root).unwrap();
        let transition = draft(1, TransitionKind::Waiting).seal().unwrap();
        outbox.enqueue(draft(1, TransitionKind::Waiting)).unwrap();
        let storage = outbox.storage.as_ref().unwrap();
        storage
            .with_exclusive_log(|log| {
                append_record(
                    log,
                    &LogRecord::Claim {
                        transition_id: transition.transition_id,
                        claim_id: "c".repeat(64),
                        claim_until_unix_ms: 1,
                    },
                )
            })
            .unwrap();
        drop(outbox);

        let reopened = TransitionOutbox::open(root).unwrap();
        let mut adapter = Adapter::default();
        assert!(matches!(
            reopened.reconcile_one(&mut adapter, 2).unwrap(),
            ReconcileOutcome::Acknowledged { .. }
        ));
    }

    #[test]
    fn active_claim_does_not_starve_an_unrelated_ready_transition() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
        let first = draft(1, TransitionKind::Waiting).seal().unwrap();
        outbox.enqueue(draft(1, TransitionKind::Waiting)).unwrap();
        let mut second_draft = draft(1, TransitionKind::Actionable);
        second_draft.workstream_id = "GEN-15".to_owned();
        let second = second_draft.clone().seal().unwrap();
        outbox.enqueue(second_draft).unwrap();
        let storage = outbox.storage.as_ref().unwrap();
        storage
            .with_exclusive_log(|log| {
                append_record(
                    log,
                    &LogRecord::Claim {
                        transition_id: first.transition_id,
                        claim_id: "d".repeat(64),
                        claim_until_unix_ms: current_unix_ms()?.saturating_add(10_000),
                    },
                )
            })
            .unwrap();
        let mut adapter = Adapter::default();
        assert_eq!(
            outbox.reconcile_one(&mut adapter, 0).unwrap(),
            ReconcileOutcome::Acknowledged {
                transition_id: second.transition_id
            }
        );
    }

    #[test]
    fn replay_rejects_individually_valid_but_reordered_transitions() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
        outbox.enqueue(draft(2, TransitionKind::NewHead)).unwrap();
        let storage = outbox.storage.as_ref().unwrap();
        storage
            .with_exclusive_log(|log| {
                append_record(
                    log,
                    &LogRecord::Enqueue {
                        transition: draft(1, TransitionKind::Waiting).seal().unwrap(),
                    },
                )
            })
            .unwrap();
        assert!(outbox.snapshot().is_err());
    }

    #[test]
    fn partial_tail_is_removed_without_losing_committed_records() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let outbox = TransitionOutbox::open(&root).unwrap();
        outbox.enqueue(draft(1, TransitionKind::NewHead)).unwrap();
        let path = root.join("transitions.ndjson");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"record\":\"enqueue\"").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let reopened = TransitionOutbox::open(&root).unwrap();
        assert_eq!(reopened.snapshot().unwrap().len(), 1);
        assert_eq!(fs::read(&path).unwrap().last(), Some(&b'\n'));
    }

    #[test]
    fn complete_malformed_record_fails_closed_without_truncation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let outbox = TransitionOutbox::open(&root).unwrap();
        outbox.enqueue(draft(1, TransitionKind::NewHead)).unwrap();
        let path = root.join("transitions.ndjson");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{not-json}\n").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let before = fs::read(&path).unwrap();
        let reopened = TransitionOutbox::open(&root).unwrap();
        assert!(reopened.snapshot().is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn concurrent_writers_serialize_without_lost_transitions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        TransitionOutbox::open(&root).unwrap();
        let mut threads = Vec::new();
        for index in 0..8_u64 {
            let root = root.clone();
            threads.push(thread::spawn(move || {
                let outbox = TransitionOutbox::open(root).unwrap();
                let mut item = draft(index + 1, TransitionKind::NewHead);
                item.workstream_id = format!("GEN-{index}");
                outbox.enqueue(item).unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        let outbox = TransitionOutbox::open(root).unwrap();
        assert_eq!(outbox.snapshot().unwrap().len(), 8);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_storage_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(TransitionOutbox::open(link).is_err());

        let root = temp.path().join("outbox");
        fs::create_dir(&root).unwrap();
        symlink(root.join("elsewhere"), root.join("transitions.ndjson")).unwrap();
        assert!(TransitionOutbox::open(root).is_err());
    }
}
