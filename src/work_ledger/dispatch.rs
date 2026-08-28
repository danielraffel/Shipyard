//! Inert canonical wake scheduling and provider delivery.
//!
//! The CLI never enables this module yet. It is deliberately structured as a
//! two-transaction outbox consumer: claim is durable before the provider call,
//! and acknowledgement/retry/ambiguity is durable afterward. A restart never
//! repeats a non-idempotent launch whose outcome was not recorded.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use fs2::FileExt;

use super::lifecycle::record_event;
use super::registry::validated_route_matches_launch;
use super::route::OpaqueRef;
use super::{
    LifecycleState, OptionalExtension, Transaction, TransactionBehavior, Utc, WorkLedger,
    WorkLedgerError, WorkLedgerResult, configure_durable, create_database_file_no_follow, digest,
    opaque_ref, params, validate_digest, validate_token, verify_integrity, verify_supported_schema,
};

/// Runtime switches are intentionally unavailable through the CLI in this phase.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WakeConsumerPolicy {
    pub(crate) activation_enabled: bool,
    pub(crate) dispatch_enabled: bool,
}

/// The exact launch-profile surface used by the provider boundary.
///
/// Implementations must return the stored arrays directly. The consumer never
/// joins them into a shell command or reconstructs provider flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FreshAgentResumeExpectation<'a> {
    pub(crate) workstream_handle: &'a str,
    pub(crate) context_url: Option<&'a str>,
    pub(crate) plan_sha256: &'a str,
    pub(crate) root_revision: u64,
    pub(crate) issue_revision: u64,
    pub(crate) projection_revision: u64,
    pub(crate) material_event_revision: u64,
    pub(crate) checkpoint_id: &'a str,
    pub(crate) checkpoint_generation: u64,
    pub(crate) checkpoint_digest: &'a str,
    pub(crate) repository: &'a str,
    pub(crate) head_sha: &'a str,
    pub(crate) expected_resume_context_digest: &'a str,
    pub(crate) success_continuation_digest: &'a str,
    pub(crate) failure_continuation_digest: &'a str,
}

pub(crate) trait FreshAgentLaunchProfile {
    fn provider_id(&self) -> &str;
    fn launch_argv(&self) -> &[String];
    fn profile_digest(&self) -> WorkLedgerResult<String>;
    fn permits_fresh_agent(&self) -> bool;

    fn resume_expectation(&self) -> Option<FreshAgentResumeExpectation<'_>> {
        None
    }

    fn route_profile_ref(&self) -> WorkLedgerResult<String> {
        Ok(
            OpaqueRef::derive("launch-profile", self.profile_digest()?.as_bytes())
                .as_str()
                .to_owned(),
        )
    }
}

/// Protected profile lookup, kept separate from provider execution.
pub(crate) trait WakeProfileResolver {
    type Profile: FreshAgentLaunchProfile;

    fn resolve(&mut self, wake: &WakeEnvelope) -> WorkLedgerResult<Self::Profile>;
}

/// Exact capability contract for one machine-local provider adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderCapability {
    pub(crate) adapter_id: String,
    pub(crate) fresh_agent_launch: bool,
    pub(crate) idempotent_launch: bool,
}

/// Fences passed unchanged to launch and reconciliation calls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeliveryFence {
    pub(crate) wake_id: String,
    pub(crate) work_item_id: String,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) route_ref: String,
    pub(crate) payload_digest: String,
    pub(crate) attempt: u64,
    pub(crate) consumer_epoch: u64,
    pub(crate) consumer_owner_ref: String,
}

/// Host-local live ownership for the complete resolve/claim/provider/finalize
/// cycle. OS lock release is the only restart signal; durable epochs record
/// which owner claimed or recovered a wake.
struct ConsumerLease {
    file: File,
    lock_key: PathBuf,
    owner_ref: String,
}

impl Drop for ConsumerLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        if let Ok(mut active) = active_consumer_locks().lock() {
            active.remove(&self.lock_key);
        }
    }
}

static ACTIVE_CONSUMER_LOCKS: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();

fn active_consumer_locks() -> &'static Mutex<BTreeSet<PathBuf>> {
    ACTIVE_CONSUMER_LOCKS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Launch request passed to an adapter without shell translation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProviderLaunchRequest<'a> {
    pub(crate) fence: &'a DeliveryFence,
    pub(crate) argv: &'a [String],
}

/// Typed provider outcome. Digests refer to protected receipts or diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProviderOutcome {
    Acknowledged { receipt_digest: String },
    Retryable { error_digest: String },
    Uncertain { evidence_digest: String },
    Rejected { error_digest: String },
}

/// Machine-local adapter. `reconcile` inspects an already-claimed idempotency key;
/// it must not silently launch a second process.
pub(crate) trait ProviderAdapter {
    fn capability(&self, provider_id: &str) -> Option<ProviderCapability>;
    fn launch(&mut self, request: ProviderLaunchRequest<'_>) -> ProviderOutcome;
    fn reconcile(&mut self, fence: &DeliveryFence) -> ProviderOutcome;
}

/// Stable scheduler envelope; it contains no raw prompt, argv, or credential.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WakeEnvelope {
    pub(crate) wake_id: String,
    pub(crate) work_item_id: String,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) route_ref: String,
    pub(crate) payload_digest: String,
    state: String,
}

/// Result of one bounded consumer tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WakeDeliveryResult {
    Empty,
    Acknowledged,
    Retrying,
    Uncertain,
    Failed,
}

impl WorkLedger {
    /// Consume at most one canonical wake. Claimed wakes are reconciled before
    /// new pending work so a restart cannot strand an ambiguous launch.
    pub(crate) fn consume_one_wake<R, A>(
        &self,
        policy: WakeConsumerPolicy,
        resolver: &mut R,
        adapter: &mut A,
    ) -> WorkLedgerResult<WakeDeliveryResult>
    where
        R: WakeProfileResolver,
        A: ProviderAdapter,
    {
        if !policy.activation_enabled || !policy.dispatch_enabled {
            return Err(WorkLedgerError::Refused(
                "wake activation and dispatch must both be explicitly enabled".to_owned(),
            ));
        }
        let consumer = acquire_consumer_lease(&self.path)?;
        let Some(wake) = self.next_wake()? else {
            return Ok(WakeDeliveryResult::Empty);
        };
        let profile = resolver.resolve(&wake)?;
        let profile_digest = profile.profile_digest()?;
        if profile_digest != wake.payload_digest {
            return Err(WorkLedgerError::Refused(
                "resolved launch profile does not match wake payload digest".to_owned(),
            ));
        }
        let profile_ref = profile.route_profile_ref()?;
        if !profile.permits_fresh_agent() {
            return self.fail_without_launch(
                &wake,
                profile.provider_id(),
                profile.provider_id(),
                &profile_ref,
                &consumer,
                digest(b"fresh-agent recovery is not authorized by the launch profile"),
            );
        }
        let Some(capability) = adapter.capability(profile.provider_id()) else {
            return self.fail_without_launch(
                &wake,
                profile.provider_id(),
                profile.provider_id(),
                &profile_ref,
                &consumer,
                digest(b"provider adapter capability is unavailable"),
            );
        };
        validate_token("provider adapter ID", &capability.adapter_id)?;
        if !capability.fresh_agent_launch {
            return self.fail_without_launch(
                &wake,
                &capability.adapter_id,
                profile.provider_id(),
                &profile_ref,
                &consumer,
                digest(b"provider adapter lacks fresh-agent capability"),
            );
        }

        let (fence, recovered_claim, claim_idempotent) = self.claim_wake(
            &wake,
            &capability,
            &profile_ref,
            profile.provider_id(),
            &consumer,
        )?;
        let outcome = if recovered_claim {
            if claim_idempotent {
                adapter.reconcile(&fence)
            } else {
                ProviderOutcome::Uncertain {
                    evidence_digest: digest(
                        format!(
                            "non-idempotent claimed wake after restart\n{}",
                            fence.wake_id
                        )
                        .as_bytes(),
                    ),
                }
            }
        } else {
            adapter.launch(ProviderLaunchRequest {
                fence: &fence,
                argv: profile.launch_argv(),
            })
        };
        self.finalize_wake(&fence, outcome)
    }

    fn next_wake(&self) -> WorkLedgerResult<Option<WakeEnvelope>> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        connection
            .query_row(
                "SELECT wake_id, work_item_id, work_generation, owner_generation,
                        route_ref, payload_digest, state
                 FROM outbox WHERE state IN ('claimed', 'pending')
                 ORDER BY CASE state WHEN 'claimed' THEN 0 ELSE 1 END,
                          created_at, wake_id LIMIT 1",
                [],
                |row| {
                    Ok(WakeEnvelope {
                        wake_id: row.get(0)?,
                        work_item_id: row.get(1)?,
                        work_generation: row.get(2)?,
                        owner_generation: row.get(3)?,
                        route_ref: row.get(4)?,
                        payload_digest: row.get(5)?,
                        state: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    fn fail_without_launch(
        &self,
        wake: &WakeEnvelope,
        adapter_id: &str,
        provider_kind: &str,
        profile_ref: &str,
        consumer: &ConsumerLease,
        error_digest: String,
    ) -> WorkLedgerResult<WakeDeliveryResult> {
        validate_token("provider adapter ID", adapter_id)?;
        let capability = ProviderCapability {
            adapter_id: adapter_id.to_owned(),
            fresh_agent_launch: false,
            idempotent_launch: false,
        };
        let (fence, _, _) =
            self.claim_wake(wake, &capability, profile_ref, provider_kind, consumer)?;
        self.finalize_wake(&fence, ProviderOutcome::Rejected { error_digest })
    }

    /// Return `(fence, recovered_claim, claim_idempotent)`. `recovered_claim`
    /// is true when the selected wake was already claimed before this tick.
    fn claim_wake(
        &self,
        wake: &WakeEnvelope,
        capability: &ProviderCapability,
        profile_ref: &str,
        provider_kind: &str,
        consumer: &ConsumerLease,
    ) -> WorkLedgerResult<(DeliveryFence, bool, bool)> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let recovered_claim =
            validate_claim_candidate(&transaction, wake, profile_ref, provider_kind)?;
        let (attempt, claim_idempotent, consumer_epoch) = claim_attempt(
            &transaction,
            wake,
            capability,
            recovered_claim,
            &consumer.owner_ref,
        )?;
        transaction.commit()?;
        Ok((
            DeliveryFence {
                wake_id: wake.wake_id.clone(),
                work_item_id: wake.work_item_id.clone(),
                work_generation: wake.work_generation,
                owner_generation: wake.owner_generation,
                route_ref: wake.route_ref.clone(),
                payload_digest: wake.payload_digest.clone(),
                attempt,
                consumer_epoch,
                consumer_owner_ref: consumer.owner_ref.clone(),
            },
            recovered_claim,
            claim_idempotent,
        ))
    }

    fn finalize_wake(
        &self,
        fence: &DeliveryFence,
        outcome: ProviderOutcome,
    ) -> WorkLedgerResult<WakeDeliveryResult> {
        let (state, result, outcome_digest) = match outcome {
            ProviderOutcome::Acknowledged { receipt_digest } => (
                "acknowledged",
                WakeDeliveryResult::Acknowledged,
                receipt_digest,
            ),
            ProviderOutcome::Retryable { error_digest } => {
                ("retry", WakeDeliveryResult::Retrying, error_digest)
            }
            ProviderOutcome::Uncertain { evidence_digest } => {
                ("uncertain", WakeDeliveryResult::Uncertain, evidence_digest)
            }
            ProviderOutcome::Rejected { error_digest } => {
                ("failed", WakeDeliveryResult::Failed, error_digest)
            }
        };
        validate_digest("provider outcome digest", &outcome_digest)?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_claim(&transaction, fence)?;
        let now = Utc::now().to_rfc3339();
        let outbox_state = if state == "retry" { "pending" } else { state };
        let changed = transaction.execute(
            "UPDATE outbox SET state = ?1, transport_receipt_digest = ?2,
                    updated_at = ?3,
                    acknowledged_at = CASE WHEN ?1 = 'acknowledged' THEN ?3 ELSE NULL END
             WHERE wake_id = ?4 AND state = 'claimed'",
            params![outbox_state, outcome_digest, now, fence.wake_id],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "wake claim no longer matches during finalization".to_owned(),
            ));
        }
        let attempt_changed = transaction.execute(
            "UPDATE wake_attempts SET state = ?1, outcome_digest = ?2, finished_at = ?3
             WHERE wake_id = ?4 AND attempt = ?5 AND state = 'claimed'",
            params![state, outcome_digest, now, fence.wake_id, fence.attempt],
        )?;
        if attempt_changed != 1 {
            return Err(WorkLedgerError::Refused(
                "wake attempt no longer matches during finalization".to_owned(),
            ));
        }
        if state == "acknowledged" {
            let work_changed = transaction.execute(
                "UPDATE work_items SET phase = 'agent_owned_repair',
                        work_generation = work_generation + 1, updated_at = ?1
                 WHERE id = ?2 AND phase = 'dispatching'
                   AND work_generation = ?3 AND owner_generation = ?4",
                params![
                    now,
                    fence.work_item_id,
                    fence.work_generation,
                    fence.owner_generation,
                ],
            )?;
            if work_changed != 1 {
                return Err(WorkLedgerError::Refused(
                    "wake work generation changed before acknowledgement".to_owned(),
                ));
            }
            record_event(
                &transaction,
                &fence.work_item_id,
                fence.work_generation + 1,
                fence.owner_generation,
                "wake_acknowledged",
                Some(LifecycleState::Dispatching),
                LifecycleState::AgentOwnedRepair,
                &outcome_digest,
                &now,
            )?;
        }
        transaction.commit()?;
        Ok(result)
    }
}

fn validate_claim_candidate(
    transaction: &Transaction<'_>,
    wake: &WakeEnvelope,
    profile_ref: &str,
    provider_kind: &str,
) -> WorkLedgerResult<bool> {
    let stored: (String, u64, u64, String) = transaction
        .query_row(
            "SELECT o.state, w.work_generation, w.owner_generation, w.phase
             FROM outbox o JOIN work_items w ON w.id = o.work_item_id
             WHERE o.wake_id = ?1 AND o.work_item_id = ?2
               AND o.work_generation = ?3 AND o.owner_generation = ?4
               AND o.route_ref = ?5 AND o.payload_digest = ?6",
            params![
                wake.wake_id,
                wake.work_item_id,
                wake.work_generation,
                wake.owner_generation,
                wake.route_ref,
                wake.payload_digest,
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| WorkLedgerError::Refused("wake identity changed before claim".to_owned()))?;
    if stored.1 != wake.work_generation
        || stored.2 != wake.owner_generation
        || stored.3 != LifecycleState::Dispatching.as_str()
    {
        return Err(WorkLedgerError::Refused(
            "wake work or owner generation is stale".to_owned(),
        ));
    }
    if !matches!(stored.0.as_str(), "pending" | "claimed") {
        return Err(WorkLedgerError::Refused(
            "wake is no longer dispatchable".to_owned(),
        ));
    }
    let route_generation = wake.work_generation.checked_sub(1).ok_or_else(|| {
        WorkLedgerError::Refused("wake work generation cannot precede its route".to_owned())
    })?;
    if !validated_route_matches_launch(
        transaction,
        &wake.route_ref,
        &wake.work_item_id,
        route_generation,
        wake.owner_generation,
        profile_ref,
        provider_kind,
    )? {
        return Err(WorkLedgerError::Refused(
            "wake route is missing, stale, or belongs to different work".to_owned(),
        ));
    }
    Ok(stored.0 == "claimed")
}

fn claim_attempt(
    transaction: &Transaction<'_>,
    wake: &WakeEnvelope,
    capability: &ProviderCapability,
    recovered_claim: bool,
    consumer_owner_ref: &str,
) -> WorkLedgerResult<(u64, bool, u64)> {
    let existing: Option<(u64, String, bool)> = transaction
        .query_row(
            "SELECT attempt, adapter_id, idempotent FROM wake_attempts
             WHERE wake_id = ?1 AND state = 'claimed'
             ORDER BY attempt DESC LIMIT 1",
            [&wake.wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((attempt, adapter_id, idempotent)) = existing {
        if !recovered_claim
            || adapter_id != capability.adapter_id
            || idempotent != capability.idempotent_launch
        {
            return Err(WorkLedgerError::Refused(
                "claimed wake adapter capability changed".to_owned(),
            ));
        }
        let epoch = record_claim_epoch(
            transaction,
            &wake.wake_id,
            attempt,
            consumer_owner_ref,
            "recovery",
        )?;
        return Ok((attempt, idempotent, epoch));
    }

    let attempt: u64 = transaction.query_row(
        "SELECT coalesce(max(attempt), 0) + 1 FROM wake_attempts WHERE wake_id = ?1",
        [&wake.wake_id],
        |row| row.get(0),
    )?;
    let now = Utc::now().to_rfc3339();
    let changed = transaction.execute(
        "UPDATE outbox SET state = 'claimed', updated_at = ?1
         WHERE wake_id = ?2 AND state = ?3",
        params![now, wake.wake_id, wake.state],
    )?;
    if changed != 1 {
        return Err(WorkLedgerError::Refused(
            "wake was concurrently claimed or resolved".to_owned(),
        ));
    }
    // A pre-v3 claimed wake has no durable attempt proving the original
    // adapter's idempotency. Preserve it as non-idempotent so restart becomes
    // uncertain instead of trusting a capability observed after the fact.
    let claim_idempotent = capability.idempotent_launch && !recovered_claim;
    transaction.execute(
        "INSERT INTO wake_attempts
         (wake_id, attempt, state, adapter_id, idempotent, started_at)
         VALUES (?1, ?2, 'claimed', ?3, ?4, ?5)",
        params![
            wake.wake_id,
            attempt,
            capability.adapter_id,
            claim_idempotent,
            now,
        ],
    )?;
    let epoch = record_claim_epoch(
        transaction,
        &wake.wake_id,
        attempt,
        consumer_owner_ref,
        "claim",
    )?;
    Ok((attempt, claim_idempotent, epoch))
}

fn record_claim_epoch(
    transaction: &Transaction<'_>,
    wake_id: &str,
    attempt: u64,
    consumer_owner_ref: &str,
    kind: &str,
) -> WorkLedgerResult<u64> {
    let epoch: u64 = transaction.query_row(
        "SELECT coalesce(max(epoch), 0) + 1 FROM wake_claim_epochs",
        [],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO wake_claim_epochs
         (wake_id, attempt, epoch, owner_ref, kind, acquired_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            wake_id,
            attempt,
            epoch,
            consumer_owner_ref,
            kind,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(epoch)
}

fn verify_claim(transaction: &Transaction<'_>, fence: &DeliveryFence) -> WorkLedgerResult<()> {
    let exact: Option<bool> = transaction
        .query_row(
            "SELECT o.work_item_id = ?2 AND o.work_generation = ?3
                    AND o.owner_generation = ?4 AND o.route_ref = ?5
                    AND o.payload_digest = ?6 AND w.phase = 'dispatching'
                    AND w.work_generation = ?3 AND w.owner_generation = ?4
             FROM outbox o JOIN work_items w ON w.id = o.work_item_id
             WHERE o.wake_id = ?1 AND o.state = 'claimed'",
            params![
                fence.wake_id,
                fence.work_item_id,
                fence.work_generation,
                fence.owner_generation,
                fence.route_ref,
                fence.payload_digest,
            ],
            |row| row.get(0),
        )
        .optional()?;
    if exact != Some(true) {
        return Err(WorkLedgerError::Refused(
            "wake claim generation fence no longer matches".to_owned(),
        ));
    }
    let epoch_owner: Option<String> = transaction
        .query_row(
            "SELECT owner_ref FROM wake_claim_epochs
             WHERE wake_id = ?1 AND attempt = ?2 AND epoch = ?3
               AND epoch = (SELECT max(epoch) FROM wake_claim_epochs
                            WHERE wake_id = ?1 AND attempt = ?2)",
            params![fence.wake_id, fence.attempt, fence.consumer_epoch],
            |row| row.get(0),
        )
        .optional()?;
    if epoch_owner.as_deref() != Some(fence.consumer_owner_ref.as_str()) {
        return Err(WorkLedgerError::Refused(
            "wake consumer ownership epoch no longer matches".to_owned(),
        ));
    }
    Ok(())
}

fn acquire_consumer_lease(database_path: &Path) -> WorkLedgerResult<ConsumerLease> {
    let parent = database_path
        .parent()
        .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
    let lock_path = parent.join("wake-consumer.lock");
    let lock_key = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf())
        .join("wake-consumer.lock");
    {
        let mut active = active_consumer_locks()
            .lock()
            .map_err(|_| WorkLedgerError::Refused("consumer lock state is poisoned".to_owned()))?;
        if !active.insert(lock_key.clone()) {
            return Err(WorkLedgerError::Refused(
                "another live wake consumer owns this ledger".to_owned(),
            ));
        }
    }
    let result = (|| {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        create_database_file_no_follow(&lock_path)?;
        let file = OpenOptions::new().read(true).write(true).open(&lock_path)?;
        file.try_lock_exclusive().map_err(|error| {
            if is_lock_contention(&error) {
                WorkLedgerError::Refused("another live wake consumer owns this ledger".to_owned())
            } else {
                WorkLedgerError::Io(error)
            }
        })?;
        let identity = format!(
            "{}:{}:{:?}",
            std::process::id(),
            Utc::now().to_rfc3339(),
            std::thread::current().id()
        );
        Ok(ConsumerLease {
            file,
            lock_key: lock_key.clone(),
            owner_ref: opaque_ref("consumer", &identity),
        })
    })();
    if result.is_err()
        && let Ok(mut active) = active_consumer_locks().lock()
    {
        active.remove(&lock_key);
    }
    result
}

fn is_lock_contention(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock || matches!(error.raw_os_error(), Some(11 | 33 | 35))
}
