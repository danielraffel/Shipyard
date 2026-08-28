//! Durable, repository-neutral state for bounded recovery workers.
//!
//! This module deliberately contains no model, GitHub, queue, merge, or release
//! integration. Callers first make an exact-head recovery decision, persist it
//! here, and only then hand the resulting request to a separately supervised
//! worker. The worker's output remains advisory until deterministic Shipyard
//! policy accepts it.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Schema version shared by recovery requests, receipts, outputs, and records.
pub const RECOVERY_SCHEMA_VERSION: u32 = 1;

const MAX_REPO_BYTES: usize = 256;
const MAX_BASE_REF_BYTES: usize = 255;
const MAX_LABEL_BYTES: usize = 255;
const MAX_SIGNATURE_BYTES: usize = 512;
const MAX_FAILURE_SUMMARY_BYTES: usize = 4_096;
const MAX_FAILED_CONTEXTS: usize = 64;
const MAX_FAILED_CONTEXT_BYTES: usize = 2_048;
const MAX_GENERATION_BYTES: usize = 256;
const MAX_DETAIL_BYTES: usize = 1_200;
const MAX_PENDING_LIMIT: usize = 1_024;
const DEFAULT_MAX_ATTEMPTS: u32 = 1;
const DEFAULT_OPT_OUT_LABEL: &str = "shipyard:no-auto-merge";

pub(crate) fn is_file_lock_contended(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        // LockFileEx reports lock and sharing contention as raw Win32 errors;
        // Rust does not currently normalize either one to WouldBlock.
        matches!(error.raw_os_error(), Some(32 | 33))
    }
    #[cfg(not(windows))]
    false
}

/// Durable recovery lifecycle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStatus {
    /// Accepted but not yet claimed by a worker.
    Pending,
    /// Claimed by one bounded worker attempt.
    Running,
    /// Worker returned a validated non-escalation result.
    Triaged,
    /// Worker explicitly routed the request to a stronger agent or human.
    Escalated,
    /// A newer exact head or request made this record stale.
    Superseded,
    /// The bounded worker attempt failed operationally.
    Failed,
}

impl RecoveryStatus {
    /// Whether no further worker transition is permitted for this record.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Triaged | Self::Escalated | Self::Superseded | Self::Failed
        )
    }
}

/// Structured recovery-worker verdict.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryVerdict {
    /// Reserved for a future repair-authorized worker. Phase 1 rejects it.
    BoundedRepair,
    /// A stronger agent or human must take over.
    Escalate,
    /// Reserved for a future diagnostics-enabled worker. Phase 1 rejects it.
    NoChange,
}

/// Coarse failure category used by deterministic recovery policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCategory {
    /// Compiler, linker, formatter, or linter failure.
    Compile,
    /// Test failure with a bounded code or fixture repair surface.
    Test,
    /// Source-control conflict or dirty-head condition.
    Conflict,
    /// Security, credentials, provenance, or trust-boundary concern.
    Security,
    /// CI, queue, release, or workflow-policy concern.
    Workflow,
    /// Runner, network, host, or provider infrastructure concern.
    Infra,
    /// Cause could not be classified confidently.
    Unknown,
}

/// Worker confidence in its classification.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryConfidence {
    /// Insufficient evidence for autonomous repair.
    Low,
    /// Evidence is useful but still warrants bounded policy checks.
    Medium,
    /// Evidence strongly supports the stated bounded classification.
    High,
}

/// One deterministic failure fact selected by Shipyard policy.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryFailureFact {
    /// GitHub reports a merge state that requires a new validated head.
    MergeState {
        /// Exact normalized GitHub merge state.
        state: String,
    },
    /// One required check is still terminally failed.
    RequiredCheck {
        /// Literal required-check context; never a display-label encoding.
        context: String,
        /// Required GitHub App database ID, when policy pins the producer.
        app_id: Option<u64>,
        /// Exact terminal conclusion selected by deterministic stewardship.
        conclusion: String,
        /// Exact workflow run identity, when GitHub exposed one.
        run_id: Option<u64>,
    },
}

/// One exact required-check identity from the deterministic steward policy.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRequiredCheck {
    /// Literal required-check context; never a display-label encoding.
    pub context: String,
    /// Required GitHub App database ID, when policy pins the producer.
    pub app_id: Option<u64>,
}

/// Immutable exact-head request presented to a recovery worker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecoveryRequest {
    /// Request schema version.
    pub schema_version: u32,
    /// Stable SHA-256 identifier for the recovery identity tuple.
    pub id: String,
    /// Canonical `owner/repository` slug.
    pub repo: String,
    /// Pull-request number.
    pub pr: u64,
    /// Exact target branch observed by the steward at enqueue time.
    pub base_ref: String,
    /// Exact immutable pull-request head SHA.
    pub head_sha: String,
    /// Whether the target branch used native merge-queue precedence.
    pub merge_queue: bool,
    /// Exact opt-out label from the steward policy that authorized enqueue.
    pub opt_out_label: String,
    /// Stable fingerprint of the deterministic failure evidence.
    pub failure_fingerprint: String,
    /// Bounded summary derived from Shipyard-normalized failure facts.
    pub failure_summary: String,
    /// Complete structured required-check policy observed at enqueue time.
    pub required_checks: Vec<RecoveryRequiredCheck>,
    /// Bounded structured merge-state or failed-check facts selected by Shipyard.
    pub failure_facts: Vec<RecoveryFailureFact>,
    /// Signature of the policy that authorized this request.
    pub policy_signature: String,
    /// Signature of the trusted worker configuration selected at enqueue time.
    pub config_signature: String,
    /// Request creation timestamp.
    pub created_at: DateTime<Utc>,
}

impl RecoveryRequest {
    /// Construct and validate one exact-head recovery request.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: impl Into<String>,
        pr: u64,
        base_ref: impl Into<String>,
        head_sha: impl Into<String>,
        failure_fingerprint: impl Into<String>,
        failure_summary: impl Into<String>,
        required_checks: Vec<RecoveryRequiredCheck>,
        failure_facts: Vec<RecoveryFailureFact>,
        policy_signature: impl Into<String>,
        config_signature: impl Into<String>,
    ) -> RecoveryResult<Self> {
        Self::new_with_steward_policy(
            repo,
            pr,
            base_ref,
            head_sha,
            false,
            DEFAULT_OPT_OUT_LABEL,
            failure_fingerprint,
            failure_summary,
            required_checks,
            failure_facts,
            policy_signature,
            config_signature,
        )
    }

    /// Construct a request carrying exact merge-queue and opt-out policy.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_steward_policy(
        repo: impl Into<String>,
        pr: u64,
        base_ref: impl Into<String>,
        head_sha: impl Into<String>,
        merge_queue: bool,
        opt_out_label: impl Into<String>,
        failure_fingerprint: impl Into<String>,
        failure_summary: impl Into<String>,
        mut required_checks: Vec<RecoveryRequiredCheck>,
        mut failure_facts: Vec<RecoveryFailureFact>,
        policy_signature: impl Into<String>,
        config_signature: impl Into<String>,
    ) -> RecoveryResult<Self> {
        let repo = repo.into().to_ascii_lowercase();
        let base_ref = base_ref.into();
        let head_sha = head_sha.into().to_ascii_lowercase();
        let opt_out_label = opt_out_label.into();
        let failure_fingerprint = failure_fingerprint.into();
        let failure_summary = failure_summary.into();
        let policy_signature = policy_signature.into();
        let config_signature = config_signature.into();
        required_checks.sort();
        failure_facts.sort();
        validate_request_fields(
            &repo,
            pr,
            &base_ref,
            &head_sha,
            merge_queue,
            &opt_out_label,
            &failure_fingerprint,
            &failure_summary,
            &required_checks,
            &failure_facts,
            &policy_signature,
            &config_signature,
        )?;
        let id = recovery_id(
            &repo,
            pr,
            &base_ref,
            &head_sha,
            merge_queue,
            &opt_out_label,
            &failure_fingerprint,
            &failure_summary,
            &required_checks,
            &failure_facts,
            &policy_signature,
        );
        Ok(Self {
            schema_version: RECOVERY_SCHEMA_VERSION,
            id,
            repo,
            pr,
            base_ref,
            head_sha,
            merge_queue,
            opt_out_label,
            failure_fingerprint,
            failure_summary,
            required_checks,
            failure_facts,
            policy_signature,
            config_signature,
            created_at: Utc::now(),
        })
    }
}

/// Strict structured result returned by a recovery worker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryOutput {
    /// Output schema version.
    pub schema_version: u32,
    /// Requested deterministic next route.
    pub verdict: RecoveryVerdict,
    /// Coarse failure classification.
    pub category: RecoveryCategory,
    /// Confidence in this classification.
    pub confidence: RecoveryConfidence,
    /// Reserved for a future diagnostics-enabled worker. Phase 1 requires empty.
    pub evidence: Vec<String>,
    /// Reserved for a future repair-authorized worker. Phase 1 requires empty.
    pub candidate_paths: Vec<String>,
    /// Reserved for a future repair-authorized worker. Phase 1 requires empty.
    pub focused_tests: Vec<String>,
}

/// Mutable durable receipt for one immutable recovery request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecoveryReceipt {
    /// Receipt schema version.
    pub schema_version: u32,
    /// Stable request identifier.
    pub request_id: String,
    /// Current durable lifecycle state.
    pub status: RecoveryStatus,
    /// Number of worker attempts already claimed.
    pub attempt: u32,
    /// Maximum attempts permitted for this request.
    pub max_attempts: u32,
    /// Trusted worker configuration signature used by the current attempt.
    pub config_signature: String,
    /// Unpredictable process-generation value for the current worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_generation: Option<String>,
    /// When the current attempt started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// When this record became terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Last pre-claim operational deferral used for fair pending scheduling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_at: Option<DateTime<Utc>>,
    /// Last durable update.
    pub updated_at: DateTime<Utc>,
    /// Newer request that superseded this one, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Bounded operational or supersedence detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Validated structured worker output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<RecoveryOutput>,
}

/// One durably persisted request and its mutable receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecoveryRecord {
    /// Record schema version.
    pub schema_version: u32,
    /// Immutable request.
    pub request: RecoveryRequest,
    /// Mutable receipt.
    pub receipt: RecoveryReceipt,
}

/// Whether enqueue created a record or found its exact durable duplicate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnqueueOutcome {
    /// A new or safely reconfigured unattempted pending record was persisted.
    Created,
    /// The exact request already existed and was left unchanged.
    Existing,
    /// A different evidence or policy identity already owns this exact head.
    HeadAlreadyTracked {
        /// Durable sibling record that owns the one-attempt budget.
        existing_id: String,
    },
}

/// Atomically persisted recovery request store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryStore {
    root: PathBuf,
    max_attempts: u32,
    lock_deadline: Option<Instant>,
}

impl RecoveryStore {
    /// Open a store rooted at the supplied recovery-state directory.
    pub fn new(root: impl Into<PathBuf>) -> RecoveryResult<Self> {
        Self::with_max_attempts(root, DEFAULT_MAX_ATTEMPTS)
    }

    /// Open a store with an explicit bounded attempt policy.
    pub fn with_max_attempts(root: impl Into<PathBuf>, max_attempts: u32) -> RecoveryResult<Self> {
        if max_attempts == 0 {
            return Err(RecoveryError::InvalidRequest(
                "max_attempts must be positive".to_owned(),
            ));
        }
        let root = root.into();
        crate::writer_domain_lease::ensure_protected_dir_all(&root)?;
        Ok(Self {
            root,
            max_attempts,
            lock_deadline: None,
        })
    }

    /// Apply one caller-owned deadline to every subsequent store lock.
    #[must_use]
    pub(crate) fn with_lock_deadline(mut self, deadline: Instant) -> Self {
        self.lock_deadline = Some(deadline);
        self
    }

    /// Store root supplied by the caller.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Load and fully validate one durable record.
    pub fn get(&self, id: &str) -> RecoveryResult<Option<RecoveryRecord>> {
        validate_id(id)?;
        let _lock = self.lock()?;
        self.load_unlocked(id)
    }

    /// Claim the request for one bounded worker attempt.
    pub fn begin(
        &self,
        id: &str,
        config_signature: &str,
        worker_generation: &str,
    ) -> RecoveryResult<RecoveryRecord> {
        validate_signature("config_signature", config_signature)?;
        validate_text(
            "worker_generation",
            worker_generation,
            1,
            MAX_GENERATION_BYTES,
        )?;
        validate_id(id)?;
        let _lock = self.lock()?;
        let mut record = self
            .load_unlocked(id)?
            .ok_or_else(|| RecoveryError::NotFound(id.to_owned()))?;
        ensure_config(&record, config_signature)?;
        let newly_claimed = match record.receipt.status {
            RecoveryStatus::Pending => {
                if record.receipt.attempt >= record.receipt.max_attempts {
                    return Err(RecoveryError::AttemptsExhausted {
                        id: id.to_owned(),
                        max_attempts: record.receipt.max_attempts,
                    });
                }
                let now = Utc::now();
                record.receipt.status = RecoveryStatus::Running;
                record.receipt.attempt += 1;
                record.receipt.worker_generation = Some(worker_generation.to_owned());
                record.receipt.started_at = Some(now);
                record.receipt.deferred_at = None;
                record.receipt.updated_at = now;
                record.receipt.detail = None;
                true
            }
            RecoveryStatus::Running
                if record.receipt.worker_generation.as_deref() == Some(worker_generation) =>
            {
                false
            }
            _ if record.receipt.attempt >= record.receipt.max_attempts => {
                return Err(RecoveryError::AttemptsExhausted {
                    id: id.to_owned(),
                    max_attempts: record.receipt.max_attempts,
                });
            }
            status => return Err(invalid_transition(id, status, "running")),
        };
        validate_record(&record)?;
        if newly_claimed {
            self.persist_claim_unlocked(&record)?;
        }
        self.save_unlocked(&record)?;
        Ok(record)
    }

    /// Persist one validated terminal worker output.
    pub fn complete(
        &self,
        id: &str,
        config_signature: &str,
        output: RecoveryOutput,
    ) -> RecoveryResult<RecoveryRecord> {
        output.validate()?;
        self.update(id, |record| {
            output.validate_for_request(&record.request)?;
            ensure_config(record, config_signature)?;
            if record.receipt.status != RecoveryStatus::Running {
                return Err(invalid_transition(
                    id,
                    record.receipt.status,
                    "triaged or escalated",
                ));
            }
            let now = Utc::now();
            record.receipt.status = if output.verdict == RecoveryVerdict::Escalate {
                RecoveryStatus::Escalated
            } else {
                RecoveryStatus::Triaged
            };
            record.receipt.output = Some(output);
            record.receipt.completed_at = Some(now);
            record.receipt.deferred_at = None;
            record.receipt.updated_at = now;
            Ok(())
        })
    }

    /// Mark a running bounded attempt as operationally failed.
    pub fn fail(
        &self,
        id: &str,
        config_signature: &str,
        detail: impl Into<String>,
    ) -> RecoveryResult<RecoveryRecord> {
        let detail = detail.into();
        validate_text("failure detail", &detail, 1, MAX_DETAIL_BYTES)?;
        self.update(id, |record| {
            ensure_config(record, config_signature)?;
            if record.receipt.status != RecoveryStatus::Running {
                return Err(invalid_transition(id, record.receipt.status, "failed"));
            }
            let now = Utc::now();
            record.receipt.status = RecoveryStatus::Failed;
            record.receipt.detail = Some(detail);
            record.receipt.completed_at = Some(now);
            record.receipt.deferred_at = None;
            record.receipt.updated_at = now;
            Ok(())
        })
    }

    /// Mark pending or running work stale in favor of a newer request.
    pub fn supersede(
        &self,
        id: &str,
        successor_id: Option<&str>,
        detail: impl Into<String>,
    ) -> RecoveryResult<RecoveryRecord> {
        if let Some(successor_id) = successor_id {
            validate_id(successor_id)?;
            if id == successor_id {
                return Err(RecoveryError::InvalidRequest(
                    "a recovery request cannot supersede itself".to_owned(),
                ));
            }
        }
        let detail = detail.into();
        validate_text("supersedence detail", &detail, 1, MAX_DETAIL_BYTES)?;
        self.update(id, |record| {
            if record.receipt.status == RecoveryStatus::Superseded {
                if successor_id.is_some() && record.receipt.superseded_by.as_deref() != successor_id
                {
                    return Err(RecoveryError::InvalidRequest(format!(
                        "recovery request {id} already has a different superseding identity"
                    )));
                }
                return Ok(());
            }
            if !matches!(
                record.receipt.status,
                RecoveryStatus::Pending | RecoveryStatus::Running
            ) {
                return Err(invalid_transition(id, record.receipt.status, "superseded"));
            }
            let now = Utc::now();
            record.receipt.status = RecoveryStatus::Superseded;
            record.receipt.superseded_by = successor_id.map(str::to_owned);
            record.receipt.detail = Some(detail);
            record.receipt.completed_at = Some(now);
            record.receipt.deferred_at = None;
            record.receipt.updated_at = now;
            Ok(())
        })
    }

    /// Supersede every active request for one exact PR target.
    ///
    /// Callers use this while holding their external evidence/witness lease so
    /// deterministic recovery can fence a concurrent advisory worker.
    pub fn supersede_active_target(
        &self,
        repo: &str,
        pr: u64,
        head_sha: &str,
        detail: impl Into<String>,
    ) -> RecoveryResult<Vec<String>> {
        validate_repo(&repo.to_ascii_lowercase())?;
        if pr == 0 {
            return Err(RecoveryError::InvalidRequest(
                "pull-request number must be positive".to_owned(),
            ));
        }
        if head_sha.len() != 40 || !head_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(RecoveryError::InvalidRequest(
                "head_sha must be a full 40-character hexadecimal SHA-1".to_owned(),
            ));
        }
        let detail = detail.into();
        validate_text("supersedence detail", &detail, 1, MAX_DETAIL_BYTES)?;
        let normalized_repo = repo.to_ascii_lowercase();
        let normalized_head = head_sha.to_ascii_lowercase();
        let _lock = self.lock()?;
        let mut superseded = Vec::new();
        for id in self.record_ids_unlocked()? {
            let Some(mut record) = self.load_unlocked(&id)? else {
                return Err(RecoveryError::NotFound(id));
            };
            if record.request.repo != normalized_repo
                || record.request.pr != pr
                || record.request.head_sha != normalized_head
                || !matches!(
                    record.receipt.status,
                    RecoveryStatus::Pending | RecoveryStatus::Running
                )
            {
                continue;
            }
            let now = Utc::now();
            record.receipt.status = RecoveryStatus::Superseded;
            record.receipt.superseded_by = None;
            record.receipt.detail = Some(detail.clone());
            record.receipt.completed_at = Some(now);
            record.receipt.deferred_at = None;
            record.receipt.updated_at = now;
            self.save_unlocked(&record)?;
            superseded.push(id);
        }
        Ok(superseded)
    }

    /// Fail running claims whose start time predates a caller-supplied cutoff.
    ///
    /// Selection and return order are deterministic. Re-running reconciliation
    /// is idempotent because only `running` records can transition.
    pub fn reconcile_stale_running(
        &self,
        cutoff: DateTime<Utc>,
        detail: impl Into<String>,
    ) -> RecoveryResult<Vec<RecoveryRecord>> {
        self.reconcile_running_where(detail, |record| {
            record
                .receipt
                .started_at
                .is_some_and(|start| start < cutoff)
        })
    }

    /// Fail every running claim after external ownership proves no worker is alive.
    ///
    /// The CLI calls this only while holding the inherited machine-global model
    /// lease. Acquiring that lease proves a prior worker and its child model
    /// have both closed their handles, so wall-clock age is neither necessary
    /// nor safe as a one-shot reconciliation gate.
    pub(crate) fn reconcile_orphaned_running(
        &self,
        detail: impl Into<String>,
    ) -> RecoveryResult<Vec<RecoveryRecord>> {
        self.reconcile_running_where(detail, |_| true)
    }

    fn reconcile_running_where(
        &self,
        detail: impl Into<String>,
        predicate: impl Fn(&RecoveryRecord) -> bool,
    ) -> RecoveryResult<Vec<RecoveryRecord>> {
        let detail = detail.into();
        validate_text("reconciliation detail", &detail, 1, MAX_DETAIL_BYTES)?;
        let _lock = self.lock()?;
        let mut stale = self
            .record_ids_unlocked()?
            .into_iter()
            .filter_map(|id| match self.load_unlocked(&id) {
                Ok(Some(record))
                    if record.receipt.status == RecoveryStatus::Running && predicate(&record) =>
                {
                    Some(Ok(record))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<RecoveryResult<Vec<_>>>()?;
        stale.sort_by(|left, right| left.request.id.cmp(&right.request.id));

        let reconciled_at = Utc::now();
        for record in &mut stale {
            record.receipt.status = RecoveryStatus::Failed;
            record.receipt.detail = Some(detail.clone());
            record.receipt.completed_at = Some(reconciled_at);
            record.receipt.deferred_at = None;
            record.receipt.updated_at = reconciled_at;
            validate_record(record)?;
            self.save_unlocked(record)?;
        }
        Ok(stale)
    }

    fn update(
        &self,
        id: &str,
        mutate: impl FnOnce(&mut RecoveryRecord) -> RecoveryResult<()>,
    ) -> RecoveryResult<RecoveryRecord> {
        validate_id(id)?;
        let _lock = self.lock()?;
        let mut record = self
            .load_unlocked(id)?
            .ok_or_else(|| RecoveryError::NotFound(id.to_owned()))?;
        mutate(&mut record)?;
        validate_record(&record)?;
        self.save_unlocked(&record)?;
        Ok(record)
    }

    fn load_unlocked(&self, id: &str) -> RecoveryResult<Option<RecoveryRecord>> {
        let Some(mut record) = self.load_record_unlocked(id)? else {
            return Ok(None);
        };
        self.apply_claim_if_present_unlocked(&mut record)?;
        validate_record(&record)?;
        Ok(Some(record))
    }

    fn same_head_records_unlocked(
        &self,
        request: &RecoveryRequest,
    ) -> RecoveryResult<Vec<RecoveryRecord>> {
        if let Some(owner) = self.load_head_owner_unlocked(request)? {
            return Ok(vec![owner]);
        }
        // Compatibility/recovery fallback for an interrupted write that saved
        // an active record before its head-owner index. The hot set is capped,
        // so this scan is bounded and never visits cold terminal history.
        let mut records = Vec::new();
        for id in self.record_ids_unlocked()? {
            let record = self
                .load_unlocked(&id)?
                .ok_or_else(|| RecoveryError::NotFound(id.clone()))?;
            if record.request.repo == request.repo
                && record.request.pr == request.pr
                && record.request.head_sha == request.head_sha
            {
                records.push(record);
            }
        }
        if let [owner] = records.as_slice() {
            self.persist_head_owner_unlocked(owner)?;
        }
        Ok(records)
    }

    fn record_ids_unlocked(&self) -> RecoveryResult<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| {
                    RecoveryError::InvalidRequest(format!(
                        "recovery record path is not UTF-8: {}",
                        path.display()
                    ))
                })?
                .to_owned();
            validate_id(&id)?;
            ids.push(id);
        }
        ids.sort();
        Ok(ids)
    }

    fn has_record_files_unlocked(&self) -> RecoveryResult<bool> {
        for entry in fs::read_dir(&self.root)? {
            if entry?.path().extension().and_then(std::ffi::OsStr::to_str) == Some("json") {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }
}

/// Errors returned by recovery state and policy operations.
#[derive(Debug)]
pub enum RecoveryError {
    /// Filesystem operation failed.
    Io(std::io::Error),
    /// Durable JSON could not be encoded or decoded.
    Json(serde_json::Error),
    /// A request field violated the durable identity contract.
    InvalidRequest(String),
    /// Structured worker output violated the bounded schema contract.
    InvalidOutput(String),
    /// A stored schema is not supported by this binary.
    SchemaVersion {
        /// Schema surface being decoded.
        surface: &'static str,
        /// Unsupported version found in state.
        observed: u32,
    },
    /// A request ID unexpectedly named different immutable content.
    IdentityCollision(String),
    /// Trusted worker configuration changed after enqueue.
    ConfigDrift {
        /// Signature captured by the request.
        expected: String,
        /// Signature observed before a later transition.
        observed: String,
    },
    /// A lifecycle transition was not legal from the durable current state.
    InvalidTransition {
        /// Request identifier.
        id: String,
        /// Current lifecycle state.
        status: RecoveryStatus,
        /// Requested destination or action.
        requested: &'static str,
    },
    /// The bounded attempt budget is exhausted.
    AttemptsExhausted {
        /// Request identifier.
        id: String,
        /// Durable maximum attempt count.
        max_attempts: u32,
    },
    /// No durable record exists for this request ID.
    NotFound(String),
}

impl Display for RecoveryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "recovery store I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "recovery JSON failed: {error}"),
            Self::InvalidRequest(message) => {
                write!(formatter, "invalid recovery request: {message}")
            }
            Self::InvalidOutput(message) => write!(formatter, "invalid recovery output: {message}"),
            Self::SchemaVersion { surface, observed } => write!(
                formatter,
                "unsupported recovery {surface} schema version {observed}"
            ),
            Self::IdentityCollision(id) => {
                write!(formatter, "recovery identity collision for {id}")
            }
            Self::ConfigDrift { expected, observed } => write!(
                formatter,
                "recovery worker configuration drifted from {expected} to {observed}"
            ),
            Self::InvalidTransition {
                id,
                status,
                requested,
            } => write!(
                formatter,
                "recovery request {id} cannot transition from {status:?} to {requested}"
            ),
            Self::AttemptsExhausted { id, max_attempts } => write!(
                formatter,
                "recovery request {id} exhausted its {max_attempts} attempt budget"
            ),
            Self::NotFound(id) => write!(formatter, "recovery request {id} was not found"),
        }
    }
}

impl Error for RecoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for RecoveryError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for RecoveryError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Result type for durable recovery state and policy operations.
pub type RecoveryResult<T> = Result<T, RecoveryError>;

mod validation;
pub use validation::recovery_id;
pub(crate) use validation::validate_record;
use validation::{
    ensure_config, invalid_transition, same_recovery_identity, validate_id, validate_repo,
    validate_request, validate_request_fields, validate_signature, validate_text,
};
#[path = "recovery_worker/archive.rs"]
mod archive;
#[path = "recovery_worker/claim.rs"]
mod claim;
#[path = "recovery_worker/enqueue.rs"]
mod enqueue;
#[path = "recovery_worker/output.rs"]
mod output;
#[path = "recovery_worker/pending.rs"]
mod pending;
#[path = "recovery_worker/store_lock.rs"]
mod store_lock;
#[cfg(test)]
#[path = "recovery_worker/store_lock_tests.rs"]
mod store_lock_tests;
#[cfg(test)]
#[path = "recovery_worker/tests.rs"]
mod tests;
