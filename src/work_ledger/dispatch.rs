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
use rusqlite::params_from_iter;
use serde::{Deserialize, Serialize};

use super::lifecycle::record_event;
use super::registry::validated_route_matches_launch;
use super::route::OpaqueRef;
use super::{
    LifecycleState, OptionalExtension, ProtectedObjectKind, Transaction, TransactionBehavior, Utc,
    WorkLedger, WorkLedgerError, WorkLedgerResult, configure_durable,
    create_database_file_no_follow, digest, opaque_ref, params, validate_digest, validate_token,
    verify_integrity, verify_supported_schema,
};

/// A wake may consume at most this many provider delivery attempts. A
/// retryable outcome on the final attempt is terminal so a permanently
/// unavailable provider cannot grow attempts and protected receipts forever.
const MAX_PROVIDER_DELIVERY_ATTEMPTS: u64 = 3;

/// Runtime switches are intentionally unavailable through the CLI in this phase.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WakeConsumerPolicy {
    pub(crate) activation_enabled: bool,
    pub(crate) dispatch_enabled: bool,
    /// Canonical, sorted lowercase GitHub repositories this consumer owns.
    pub(crate) authorized_repositories: Vec<String>,
}

/// The exact launch-profile surface used by the provider boundary.
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
    fn provider_launch_options(&self) -> FreshAgentProviderLaunchOptions;
    fn profile_digest(&self) -> WorkLedgerResult<String>;
    fn permits_fresh_agent(&self) -> bool;

    /// Exact immutable profile bytes whose digest is `profile_digest`.
    fn protected_profile_bytes(&self) -> WorkLedgerResult<Vec<u8>> {
        Err(WorkLedgerError::Refused(
            "launch profile does not expose exact protected bytes".to_owned(),
        ))
    }

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

/// Minimal provider-owned launch choices copied from validated profile metadata.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FreshAgentProviderLaunchOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model_id: Option<String>,
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
    pub(crate) activation_id: String,
    pub(crate) delivery_id: String,
    pub(crate) request_object_ref: String,
    pub(crate) profile_ref: String,
    pub(crate) adapter_id: String,
    pub(crate) provider_id: String,
    pub(crate) idempotency_key: String,
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

/// Launch request passed to an adapter without shell translation or argv.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProviderLaunchRequest<'a> {
    pub(crate) fence: &'a DeliveryFence,
}

/// Typed provider outcome. Digests refer to protected receipts or diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProviderOutcome {
    Delivered { receipt: Vec<u8> },
    Retryable { evidence: Vec<u8> },
    Uncertain { evidence: Vec<u8> },
    Rejected { evidence: Vec<u8> },
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
    pub(crate) repository: String,
    state: String,
}

/// Result of one bounded consumer tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WakeDeliveryResult {
    Empty,
    Delivered,
    Retrying,
    Uncertain,
    Failed,
}

fn classify_provider_outcome(
    attempt: u64,
    outcome: ProviderOutcome,
    rejection_event: &'static str,
) -> (
    &'static str,
    WakeDeliveryResult,
    Vec<u8>,
    Option<&'static str>,
) {
    match outcome {
        ProviderOutcome::Delivered { receipt } => {
            ("delivered", WakeDeliveryResult::Delivered, receipt, None)
        }
        ProviderOutcome::Retryable { evidence } if attempt >= MAX_PROVIDER_DELIVERY_ATTEMPTS => (
            "failed",
            WakeDeliveryResult::Failed,
            evidence,
            Some("provider_retry_exhausted"),
        ),
        ProviderOutcome::Retryable { evidence } => {
            ("retry", WakeDeliveryResult::Retrying, evidence, None)
        }
        ProviderOutcome::Uncertain { evidence } => {
            ("uncertain", WakeDeliveryResult::Uncertain, evidence, None)
        }
        ProviderOutcome::Rejected { evidence } => (
            "failed",
            WakeDeliveryResult::Failed,
            evidence,
            Some(rejection_event),
        ),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredResumeExpectation {
    pub(crate) workstream_handle: String,
    pub(crate) context_url: Option<String>,
    pub(crate) plan_sha256: String,
    pub(crate) root_revision: u64,
    pub(crate) issue_revision: u64,
    pub(crate) material_event_revision: u64,
    pub(crate) projection_revision: u64,
    pub(crate) checkpoint_id: String,
    pub(crate) checkpoint_generation: u64,
    pub(crate) checkpoint_digest: String,
    pub(crate) repository: String,
    pub(crate) head_sha: String,
    pub(crate) expected_resume_context_digest: String,
    pub(crate) success_continuation_digest: String,
    pub(crate) failure_continuation_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredProviderRequest {
    pub(crate) schema_version: u32,
    pub(crate) wake_id: String,
    pub(crate) attempt: u64,
    pub(crate) adapter_id: String,
    pub(crate) provider_id: String,
    pub(crate) idempotency_key: String,
    pub(crate) profile_ref: String,
    pub(crate) profile_object_ref: String,
    pub(crate) profile_digest: String,
    pub(crate) launch_options: FreshAgentProviderLaunchOptions,
    pub(crate) resume: StoredResumeExpectation,
}

fn stored_resume_expectation(
    value: FreshAgentResumeExpectation<'_>,
) -> WorkLedgerResult<StoredResumeExpectation> {
    validate_digest("resume plan digest", value.plan_sha256)?;
    validate_digest("resume checkpoint digest", value.checkpoint_digest)?;
    validate_digest(
        "expected resume context digest",
        value.expected_resume_context_digest,
    )?;
    validate_digest(
        "success continuation digest",
        value.success_continuation_digest,
    )?;
    validate_digest(
        "failure continuation digest",
        value.failure_continuation_digest,
    )?;
    if value.projection_revision == 0
        || value.checkpoint_generation == 0
        || !is_canonical_workstream_handle(value.workstream_handle)
        || value.checkpoint_id.is_empty()
        || value.checkpoint_id.len() > 128
        || crate::evidence::canonical_repository(value.repository) != value.repository
        || !is_exact_git_sha(value.head_sha)
        || value
            .context_url
            .is_some_and(|url| !is_secret_free_context_url(url))
    {
        return Err(WorkLedgerError::Refused(
            "fresh-agent resume authority is incomplete or malformed".to_owned(),
        ));
    }
    Ok(StoredResumeExpectation {
        workstream_handle: value.workstream_handle.to_owned(),
        context_url: value.context_url.map(ToOwned::to_owned),
        plan_sha256: value.plan_sha256.to_owned(),
        root_revision: value.root_revision,
        issue_revision: value.issue_revision,
        material_event_revision: value.material_event_revision,
        projection_revision: value.projection_revision,
        checkpoint_id: value.checkpoint_id.to_owned(),
        checkpoint_generation: value.checkpoint_generation,
        checkpoint_digest: value.checkpoint_digest.to_owned(),
        repository: value.repository.to_owned(),
        head_sha: value.head_sha.to_owned(),
        expected_resume_context_digest: value.expected_resume_context_digest.to_owned(),
        success_continuation_digest: value.success_continuation_digest.to_owned(),
        failure_continuation_digest: value.failure_continuation_digest.to_owned(),
    })
}

fn is_canonical_workstream_handle(value: &str) -> bool {
    let Some((team, number)) = value.split_once('-') else {
        return false;
    };
    !team.is_empty()
        && team.len() <= 16
        && team.bytes().all(|byte| byte.is_ascii_uppercase())
        && !number.is_empty()
        && !number.starts_with('0')
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_secret_free_context_url(value: &str) -> bool {
    let Some(remainder) = value.strip_prefix("https://") else {
        return false;
    };
    let authority = remainder.split('/').next().unwrap_or_default();
    !authority.is_empty()
        && !authority.contains('@')
        && value.len() <= 4096
        && !value.contains(['?', '#'])
        && !value.chars().any(char::is_control)
        && !value.chars().any(char::is_whitespace)
}

fn is_exact_git_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
        validate_consumer_policy(&policy)?;
        let consumer = acquire_consumer_lease(&self.path)?;
        let Some(wake) = self.next_wake(&policy)? else {
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
                b"fresh-agent recovery is not authorized by the launch profile".to_vec(),
            );
        }
        let Some(capability) = adapter.capability(profile.provider_id()) else {
            return self.fail_without_launch(
                &wake,
                profile.provider_id(),
                profile.provider_id(),
                &profile_ref,
                &consumer,
                b"provider adapter capability is unavailable".to_vec(),
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
                b"provider adapter lacks fresh-agent capability".to_vec(),
            );
        }

        let profile_bytes = profile.protected_profile_bytes()?;
        if digest(&profile_bytes) != profile_digest {
            return Err(WorkLedgerError::Refused(
                "protected launch profile bytes do not match its digest".to_owned(),
            ));
        }
        let resume = stored_resume_expectation(profile.resume_expectation().ok_or_else(|| {
            WorkLedgerError::Refused(
                "fresh-agent launch profile lacks immutable resume authority".to_owned(),
            )
        })?)?;
        let launch_options = profile.provider_launch_options();
        if crate::evidence::canonical_repository(&resume.repository) != wake.repository {
            return Err(WorkLedgerError::Refused(
                "launch profile repository does not match selected work".to_owned(),
            ));
        }
        let profile_object = self.put_protected_object(
            &wake.work_item_id,
            ProtectedObjectKind::LaunchProfile,
            Some(&profile_ref),
            &profile_digest,
            &profile_bytes,
        )?;

        let (claim_fence, recovered_claim, claim_idempotent) = self.claim_wake(
            &wake,
            &capability,
            &profile_ref,
            profile.provider_id(),
            &consumer,
        )?;
        let fence = self.prepare_delivery(
            claim_fence,
            &capability,
            profile.provider_id(),
            launch_options,
            resume,
            &profile_object.object_ref,
        )?;
        let outcome = if recovered_claim {
            if claim_idempotent {
                adapter.reconcile(&fence)
            } else {
                ProviderOutcome::Uncertain {
                    evidence: format!(
                        "non-idempotent claimed wake after restart\n{}",
                        fence.wake_id
                    )
                    .into_bytes(),
                }
            }
        } else {
            self.mark_delivery_launched(&fence)?;
            adapter.launch(ProviderLaunchRequest { fence: &fence })
        };
        self.finalize_wake(&fence, outcome)
    }

    fn mark_delivery_launched(&self, fence: &DeliveryFence) -> WorkLedgerResult<()> {
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
        let changed = transaction.execute(
            "UPDATE provider_deliveries SET state = 'launched', updated_at = ?1
             WHERE delivery_id = ?2 AND wake_id = ?3 AND attempt = ?4
               AND activation_id = ?5 AND adapter_id = ?6
               AND idempotency_key = ?7 AND request_object_ref = ?8
               AND state = 'prepared'",
            params![
                Utc::now().to_rfc3339(),
                fence.delivery_id,
                fence.wake_id,
                fence.attempt,
                fence.activation_id,
                fence.adapter_id,
                fence.idempotency_key,
                fence.request_object_ref,
            ],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "provider delivery changed before submit".to_owned(),
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Reconcile one uncertain provider delivery by its original idempotency
    /// fence. This API never invokes `launch`.
    pub(crate) fn reconcile_uncertain_wake<A: ProviderAdapter>(
        &self,
        policy: &WakeConsumerPolicy,
        wake_id: &str,
        adapter: &mut A,
    ) -> WorkLedgerResult<WakeDeliveryResult> {
        validate_consumer_policy(policy)?;
        let _consumer = acquire_consumer_lease(&self.path)?;
        let (fence, repository) = self.uncertain_fence(wake_id)?;
        if policy
            .authorized_repositories
            .binary_search(&repository)
            .is_err()
        {
            return Err(WorkLedgerError::Refused(
                "uncertain delivery repository is not authorized".to_owned(),
            ));
        }
        let capability = adapter.capability(&fence.provider_id).ok_or_else(|| {
            WorkLedgerError::Refused(
                "provider capability is unavailable for reconciliation".to_owned(),
            )
        })?;
        if capability.adapter_id != fence.adapter_id {
            return Err(WorkLedgerError::Refused(
                "provider adapter changed before evidence reconciliation".to_owned(),
            ));
        }
        let outcome = adapter.reconcile(&fence);
        self.finalize_uncertain_wake(&fence, outcome)
    }

    /// Select at most one current uncertain delivery for a permitted repository.
    pub(crate) fn next_uncertain_wake_id(
        &self,
        policy: &WakeConsumerPolicy,
    ) -> WorkLedgerResult<Option<String>> {
        validate_consumer_policy(policy)?;
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let placeholders = std::iter::repeat_n("?", policy.authorized_repositories.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT wake.wake_id
             FROM outbox wake
             JOIN work_items work ON work.id = wake.work_item_id
             JOIN provider_deliveries delivery ON delivery.wake_id = wake.wake_id
             WHERE wake.state = 'uncertain' AND delivery.state = 'uncertain'
               AND work.phase = 'dispatching'
               AND work.work_generation = wake.work_generation
               AND work.owner_generation = wake.owner_generation
               AND lower(work.repo) IN ({placeholders})
             ORDER BY wake.updated_at, wake.wake_id LIMIT 1"
        );
        connection
            .query_row(
                &sql,
                params_from_iter(policy.authorized_repositories.iter()),
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Whether one claimed or pending wake belongs to this exact repository
    /// allowlist. Unauthorized work must not keep the daemon lane spinning.
    pub(crate) fn has_authorized_pending_wake(
        &self,
        policy: &WakeConsumerPolicy,
    ) -> WorkLedgerResult<bool> {
        validate_consumer_policy(policy)?;
        self.next_wake(policy).map(|wake| wake.is_some())
    }

    fn uncertain_fence(&self, wake_id: &str) -> WorkLedgerResult<(DeliveryFence, String)> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        connection
            .query_row(
                "SELECT wake.work_item_id, wake.work_generation, wake.owner_generation,
                        wake.route_ref, wake.payload_digest, delivery.attempt,
                        claim.epoch, claim.owner_ref, delivery.activation_id,
                        delivery.delivery_id, delivery.request_object_ref, wake.profile_ref,
                        delivery.adapter_id, delivery.provider_id, delivery.idempotency_key,
                        lower(work.repo)
                 FROM outbox wake
                 JOIN provider_deliveries delivery ON delivery.wake_id = wake.wake_id
                 JOIN wake_claim_epochs claim
                   ON claim.wake_id = delivery.wake_id AND claim.attempt = delivery.attempt
                  AND claim.epoch = (SELECT max(epoch) FROM wake_claim_epochs
                                     WHERE wake_id = delivery.wake_id
                                       AND attempt = delivery.attempt)
                 JOIN work_items work ON work.id = wake.work_item_id
                 WHERE wake.wake_id = ?1 AND wake.state = 'uncertain'
                   AND delivery.state = 'uncertain'
                   AND work.phase = 'dispatching'
                   AND work.work_generation = wake.work_generation
                   AND work.owner_generation = wake.owner_generation",
                [wake_id],
                |row| {
                    Ok((
                        DeliveryFence {
                            wake_id: wake_id.to_owned(),
                            work_item_id: row.get(0)?,
                            work_generation: row.get(1)?,
                            owner_generation: row.get(2)?,
                            route_ref: row.get(3)?,
                            payload_digest: row.get(4)?,
                            attempt: row.get(5)?,
                            consumer_epoch: row.get(6)?,
                            consumer_owner_ref: row.get(7)?,
                            activation_id: row.get(8)?,
                            delivery_id: row.get(9)?,
                            request_object_ref: row.get(10)?,
                            profile_ref: row.get(11)?,
                            adapter_id: row.get(12)?,
                            provider_id: row.get(13)?,
                            idempotency_key: row.get(14)?,
                        },
                        row.get(15)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                WorkLedgerError::Refused(
                    "wake is not an exact uncertain provider delivery".to_owned(),
                )
            })
    }

    fn finalize_uncertain_wake(
        &self,
        fence: &DeliveryFence,
        outcome: ProviderOutcome,
    ) -> WorkLedgerResult<WakeDeliveryResult> {
        let (state, result, response_bytes, failure_event) =
            classify_provider_outcome(fence.attempt, outcome, "provider_reconciliation_failed");
        let outcome_digest = digest(&response_bytes);
        let response = self.put_protected_object(
            &fence.work_item_id,
            ProtectedObjectKind::ProviderReceipt,
            None,
            &outcome_digest,
            &response_bytes,
        )?;
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
        verify_uncertain_fence(&transaction, fence)?;
        let now = Utc::now().to_rfc3339();
        append_delivery_observation(
            &transaction,
            fence,
            "uncertain",
            state,
            &response.object_ref,
            &outcome_digest,
            &now,
        )?;
        let attempt_changed = transaction.execute(
            "UPDATE wake_attempts SET state = ?1, outcome_digest = ?2, finished_at = ?3
             WHERE wake_id = ?4 AND attempt = ?5 AND state = 'uncertain'
               AND adapter_id = ?6",
            params![
                state,
                outcome_digest,
                now,
                fence.wake_id,
                fence.attempt,
                fence.adapter_id,
            ],
        )?;
        let delivery_changed = transaction.execute(
            "UPDATE provider_deliveries
             SET state = ?1, receipt_object_ref = ?2, updated_at = ?3,
                 delivered_at = CASE WHEN ?1 = 'delivered' THEN ?3 ELSE NULL END
             WHERE delivery_id = ?4 AND wake_id = ?5 AND attempt = ?6
               AND idempotency_key = ?7 AND state = 'uncertain'",
            params![
                state,
                response.object_ref,
                now,
                fence.delivery_id,
                fence.wake_id,
                fence.attempt,
                fence.idempotency_key,
            ],
        )?;
        let outbox_state = if state == "retry" { "pending" } else { state };
        let wake_changed = transaction.execute(
            "UPDATE outbox SET state = ?1, transport_receipt_digest = ?2,
                    provider_delivery_id = CASE WHEN ?1 = 'delivered' THEN ?3 ELSE NULL END,
                    updated_at = ?4
             WHERE wake_id = ?5 AND state = 'uncertain'",
            params![
                outbox_state,
                outcome_digest,
                fence.delivery_id,
                now,
                fence.wake_id
            ],
        )?;
        if attempt_changed != 1 || delivery_changed != 1 || wake_changed != 1 {
            return Err(WorkLedgerError::Refused(
                "uncertain delivery changed during evidence reconciliation".to_owned(),
            ));
        }
        if let Some(event_kind) = failure_event {
            transition_dispatch_failure(&transaction, fence, &outcome_digest, &now, event_kind)?;
        }
        transaction.commit()?;
        Ok(result)
    }

    fn next_wake(&self, policy: &WakeConsumerPolicy) -> WorkLedgerResult<Option<WakeEnvelope>> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let placeholders = std::iter::repeat_n("?", policy.authorized_repositories.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT wake.wake_id, wake.work_item_id, wake.work_generation,
                    wake.owner_generation, wake.route_ref, wake.payload_digest,
                    wake.state, lower(work.repo)
             FROM outbox wake
             JOIN work_items work ON work.id = wake.work_item_id
             WHERE wake.state IN ('claimed', 'pending')
               AND lower(work.repo) IN ({placeholders})
             ORDER BY CASE wake.state WHEN 'claimed' THEN 0 ELSE 1 END,
                      wake.created_at, wake.wake_id LIMIT 1"
        );
        connection
            .query_row(
                &sql,
                params_from_iter(policy.authorized_repositories.iter()),
                |row| {
                    Ok(WakeEnvelope {
                        wake_id: row.get(0)?,
                        work_item_id: row.get(1)?,
                        work_generation: row.get(2)?,
                        owner_generation: row.get(3)?,
                        route_ref: row.get(4)?,
                        payload_digest: row.get(5)?,
                        state: row.get(6)?,
                        repository: row.get(7)?,
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
        evidence: Vec<u8>,
    ) -> WorkLedgerResult<WakeDeliveryResult> {
        validate_token("provider adapter ID", adapter_id)?;
        let capability = ProviderCapability {
            adapter_id: adapter_id.to_owned(),
            fresh_agent_launch: false,
            idempotent_launch: false,
        };
        let (fence, _, _) =
            self.claim_wake(wake, &capability, profile_ref, provider_kind, consumer)?;
        self.finalize_without_delivery(&fence, &evidence)
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
        let profile_changed = transaction.execute(
            "UPDATE outbox SET profile_ref = ?1
             WHERE wake_id = ?2 AND (profile_ref IS NULL OR profile_ref = ?1)",
            params![profile_ref, wake.wake_id],
        )?;
        if profile_changed != 1 {
            return Err(WorkLedgerError::Refused(
                "wake launch profile binding changed before claim".to_owned(),
            ));
        }
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
                activation_id: String::new(),
                delivery_id: String::new(),
                request_object_ref: String::new(),
                profile_ref: profile_ref.to_owned(),
                adapter_id: capability.adapter_id.clone(),
                provider_id: provider_kind.to_owned(),
                idempotency_key: String::new(),
            },
            recovered_claim,
            claim_idempotent,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_delivery(
        &self,
        mut fence: DeliveryFence,
        capability: &ProviderCapability,
        provider_id: &str,
        launch_options: FreshAgentProviderLaunchOptions,
        resume: StoredResumeExpectation,
        profile_object_ref: &str,
    ) -> WorkLedgerResult<DeliveryFence> {
        let activation_id = opaque_ref(
            "ae",
            &format!(
                "shipyard-activation-v1\n{}\n{}\n{}\n{}",
                fence.wake_id, fence.attempt, fence.work_generation, fence.owner_generation
            ),
        );
        let delivery_id = opaque_ref(
            "pd",
            &format!(
                "shipyard-provider-delivery-v1\n{}\n{}\n{}",
                fence.wake_id, fence.attempt, capability.adapter_id
            ),
        );
        let idempotency_key = digest(
            format!(
                "shipyard-provider-idempotency-v1\n{}\n{}\n{}\n{}\n{}\n{}",
                fence.wake_id,
                fence.attempt,
                capability.adapter_id,
                provider_id,
                fence.profile_ref,
                fence.payload_digest,
            )
            .as_bytes(),
        );
        let request = StoredProviderRequest {
            schema_version: 2,
            wake_id: fence.wake_id.clone(),
            attempt: fence.attempt,
            adapter_id: capability.adapter_id.clone(),
            provider_id: provider_id.to_owned(),
            idempotency_key: idempotency_key.clone(),
            profile_ref: fence.profile_ref.clone(),
            profile_object_ref: profile_object_ref.to_owned(),
            profile_digest: fence.payload_digest.clone(),
            launch_options,
            resume,
        };
        let request_bytes = serde_json::to_vec(&request).map_err(|error| {
            WorkLedgerError::Refused(format!("provider request is not serializable: {error}"))
        })?;
        let request_digest = digest(&request_bytes);
        let request_object = self.put_protected_object(
            &fence.work_item_id,
            ProtectedObjectKind::ProviderRequest,
            None,
            &request_digest,
            &request_bytes,
        )?;

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
        verify_claim(&transaction, &fence)?;
        let now = Utc::now().to_rfc3339();
        let original_claim: (u64, String) = transaction.query_row(
            "SELECT epoch, owner_ref FROM wake_claim_epochs
             WHERE wake_id = ?1 AND attempt = ?2 AND kind = 'claim'
             ORDER BY epoch LIMIT 1",
            params![fence.wake_id, fence.attempt],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let existing_activation: Option<(String, u64, u64, u64, String, String)> = transaction
            .query_row(
                "SELECT work_item_id, work_generation, owner_generation, epoch, owner_ref, state
                 FROM activation_epochs WHERE activation_id = ?1",
                [&activation_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        match existing_activation {
            None => {
                transaction.execute(
                    "INSERT INTO activation_epochs
                     (activation_id, work_item_id, work_generation, owner_generation,
                      epoch, owner_ref, state, acquired_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
                    params![
                        activation_id,
                        fence.work_item_id,
                        fence.work_generation,
                        fence.owner_generation,
                        original_claim.0,
                        original_claim.1,
                        now,
                    ],
                )?;
            }
            Some((work, work_generation, owner_generation, epoch, owner_ref, state))
                if work == fence.work_item_id
                    && work_generation == fence.work_generation
                    && owner_generation == fence.owner_generation
                    && epoch == original_claim.0
                    && owner_ref == original_claim.1
                    && state == "active" => {}
            Some(_) => {
                return Err(WorkLedgerError::Refused(
                    "activation epoch collides with different authority".to_owned(),
                ));
            }
        }
        let existing_delivery: Option<(
            String,
            u64,
            String,
            String,
            String,
            String,
            String,
            String,
        )> = transaction
            .query_row(
                "SELECT wake_id, attempt, activation_id, adapter_id,
                        idempotency_key, request_object_ref, provider_id, state
                 FROM provider_deliveries WHERE delivery_id = ?1",
                [&delivery_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?;
        match existing_delivery {
            None => {
                transaction.execute(
                    "INSERT INTO provider_deliveries
                     (delivery_id, wake_id, attempt, activation_id, provider_id,
                      adapter_id, idempotency_key, request_object_ref, state,
                      created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'prepared', ?9, ?9)",
                    params![
                        delivery_id,
                        fence.wake_id,
                        fence.attempt,
                        activation_id,
                        provider_id,
                        capability.adapter_id,
                        idempotency_key,
                        request_object.object_ref,
                        now,
                    ],
                )?;
            }
            Some((
                wake_id,
                attempt,
                existing_activation,
                adapter_id,
                existing_idempotency,
                request_ref,
                existing_provider,
                state,
            )) if wake_id == fence.wake_id
                && attempt == fence.attempt
                && existing_activation == activation_id
                && adapter_id == capability.adapter_id
                && existing_idempotency == idempotency_key
                && request_ref == request_object.object_ref
                && existing_provider == provider_id
                && matches!(state.as_str(), "prepared" | "launched") => {}
            Some(_) => {
                return Err(WorkLedgerError::Refused(
                    "provider delivery collides with different authority".to_owned(),
                ));
            }
        }
        transaction.commit()?;
        fence.activation_id = activation_id;
        fence.delivery_id = delivery_id;
        fence.request_object_ref = request_object.object_ref;
        fence.adapter_id = capability.adapter_id.clone();
        fence.provider_id = provider_id.to_owned();
        fence.idempotency_key = idempotency_key;
        Ok(fence)
    }

    fn finalize_wake(
        &self,
        fence: &DeliveryFence,
        outcome: ProviderOutcome,
    ) -> WorkLedgerResult<WakeDeliveryResult> {
        let (state, result, response_bytes, failure_event) =
            classify_provider_outcome(fence.attempt, outcome, "provider_delivery_failed");
        let outcome_digest = digest(&response_bytes);
        let response = self.put_protected_object(
            &fence.work_item_id,
            ProtectedObjectKind::ProviderReceipt,
            None,
            &outcome_digest,
            &response_bytes,
        )?;
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
        let from_state: String = transaction.query_row(
            "SELECT state FROM provider_deliveries
             WHERE delivery_id = ?1 AND wake_id = ?2 AND attempt = ?3
               AND activation_id = ?4 AND adapter_id = ?5
               AND idempotency_key = ?6 AND request_object_ref = ?7
               AND state IN ('prepared', 'launched')",
            params![
                fence.delivery_id,
                fence.wake_id,
                fence.attempt,
                fence.activation_id,
                fence.adapter_id,
                fence.idempotency_key,
                fence.request_object_ref,
            ],
            |row| row.get(0),
        )?;
        append_delivery_observation(
            &transaction,
            fence,
            &from_state,
            state,
            &response.object_ref,
            &outcome_digest,
            &now,
        )?;
        let outbox_state = if state == "retry" { "pending" } else { state };
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
        let delivery_changed = transaction.execute(
            "UPDATE provider_deliveries
             SET state = ?1, receipt_object_ref = ?2, updated_at = ?3,
                 delivered_at = CASE WHEN ?1 = 'delivered' THEN ?3 ELSE NULL END
             WHERE delivery_id = ?4 AND wake_id = ?5 AND attempt = ?6
               AND activation_id = ?7 AND adapter_id = ?8
               AND idempotency_key = ?9 AND request_object_ref = ?10
               AND state IN ('prepared', 'launched')",
            params![
                state,
                response.object_ref,
                now,
                fence.delivery_id,
                fence.wake_id,
                fence.attempt,
                fence.activation_id,
                fence.adapter_id,
                fence.idempotency_key,
                fence.request_object_ref,
            ],
        )?;
        if delivery_changed != 1 {
            return Err(WorkLedgerError::Refused(
                "provider delivery no longer matches during finalization".to_owned(),
            ));
        }
        let changed = transaction.execute(
            "UPDATE outbox SET state = ?1, transport_receipt_digest = ?2,
                    provider_delivery_id = CASE WHEN ?1 = 'delivered' THEN ?3 ELSE NULL END,
                    updated_at = ?4, acknowledged_at = NULL
             WHERE wake_id = ?5 AND state = 'claimed'",
            params![
                outbox_state,
                outcome_digest,
                fence.delivery_id,
                now,
                fence.wake_id
            ],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "wake claim no longer matches during finalization".to_owned(),
            ));
        }
        let activation_changed = transaction.execute(
            "UPDATE activation_epochs SET state = 'released', released_at = ?1
             WHERE activation_id = ?2 AND state = 'active'",
            params![now, fence.activation_id],
        )?;
        if activation_changed != 1 {
            return Err(WorkLedgerError::Refused(
                "provider activation no longer matches during finalization".to_owned(),
            ));
        }
        if let Some(event_kind) = failure_event {
            transition_dispatch_failure(&transaction, fence, &outcome_digest, &now, event_kind)?;
        }
        transaction.commit()?;
        Ok(result)
    }

    fn finalize_without_delivery(
        &self,
        fence: &DeliveryFence,
        evidence: &[u8],
    ) -> WorkLedgerResult<WakeDeliveryResult> {
        let evidence_digest = digest(evidence);
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
        let attempt_changed = transaction.execute(
            "UPDATE wake_attempts SET state = 'failed', outcome_digest = ?1, finished_at = ?2
             WHERE wake_id = ?3 AND attempt = ?4 AND state = 'claimed'",
            params![evidence_digest, now, fence.wake_id, fence.attempt],
        )?;
        let wake_changed = transaction.execute(
            "UPDATE outbox SET state = 'failed', transport_receipt_digest = ?1,
                    updated_at = ?2, acknowledged_at = NULL
             WHERE wake_id = ?3 AND state = 'claimed'",
            params![evidence_digest, now, fence.wake_id],
        )?;
        if attempt_changed != 1 || wake_changed != 1 {
            return Err(WorkLedgerError::Refused(
                "wake changed during pre-delivery failure".to_owned(),
            ));
        }
        transition_dispatch_failure(
            &transaction,
            fence,
            &evidence_digest,
            &now,
            "provider_refused_before_delivery",
        )?;
        transaction.commit()?;
        Ok(WakeDeliveryResult::Failed)
    }
}

fn validate_consumer_policy(policy: &WakeConsumerPolicy) -> WorkLedgerResult<()> {
    if !policy.activation_enabled || !policy.dispatch_enabled {
        return Err(WorkLedgerError::Refused(
            "wake activation and dispatch must both be explicitly enabled".to_owned(),
        ));
    }
    if policy.authorized_repositories.is_empty()
        || policy.authorized_repositories.len() > 256
        || policy.authorized_repositories.iter().any(|repository| {
            repository != &repository.to_ascii_lowercase()
                || repository.trim() != repository
                || repository.split('/').count() != 2
                || repository.split('/').any(str::is_empty)
        })
        || !policy
            .authorized_repositories
            .windows(2)
            .all(|pair| pair[0] < pair[1])
    {
        return Err(WorkLedgerError::Refused(
            "wake consumer repositories must be a nonempty sorted canonical allowlist".to_owned(),
        ));
    }
    Ok(())
}

fn transition_dispatch_failure(
    transaction: &Transaction<'_>,
    fence: &DeliveryFence,
    evidence_digest: &str,
    now: &str,
    event_kind: &str,
) -> WorkLedgerResult<()> {
    let changed = transaction.execute(
        "UPDATE work_items SET phase = 'actionable', work_generation = work_generation + 1,
                updated_at = ?1
         WHERE id = ?2 AND phase = 'dispatching'
           AND work_generation = ?3 AND owner_generation = ?4",
        params![
            now,
            fence.work_item_id,
            fence.work_generation,
            fence.owner_generation,
        ],
    )?;
    if changed != 1 {
        return Err(WorkLedgerError::Refused(
            "failed delivery work generation changed before recovery".to_owned(),
        ));
    }
    record_event(
        transaction,
        &fence.work_item_id,
        fence.work_generation + 1,
        fence.owner_generation,
        event_kind,
        Some(LifecycleState::Dispatching),
        LifecycleState::Actionable,
        evidence_digest,
        now,
    )?;
    Ok(())
}

fn verify_uncertain_fence(
    transaction: &Transaction<'_>,
    fence: &DeliveryFence,
) -> WorkLedgerResult<()> {
    let exact: bool = transaction.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM outbox wake
           JOIN work_items work ON work.id = wake.work_item_id
           JOIN provider_deliveries delivery ON delivery.wake_id = wake.wake_id
           JOIN wake_attempts attempt
             ON attempt.wake_id = delivery.wake_id AND attempt.attempt = delivery.attempt
           JOIN activation_epochs activation
             ON activation.activation_id = delivery.activation_id
           WHERE wake.wake_id = ?1 AND wake.work_item_id = ?2
             AND wake.work_generation = ?3 AND wake.owner_generation = ?4
             AND wake.route_ref = ?5 AND wake.payload_digest = ?6
             AND wake.state = 'uncertain' AND work.phase = 'dispatching'
             AND work.work_generation = ?3 AND work.owner_generation = ?4
             AND delivery.delivery_id = ?7 AND delivery.attempt = ?8
             AND delivery.activation_id = ?9 AND delivery.request_object_ref = ?10
             AND delivery.adapter_id = ?11 AND delivery.provider_id = ?12
             AND delivery.idempotency_key = ?13 AND delivery.state = 'uncertain'
             AND attempt.state = 'uncertain' AND attempt.adapter_id = ?11
             AND activation.state = 'released'
             AND EXISTS (
               SELECT 1 FROM wake_claim_epochs claim
                WHERE claim.wake_id = ?1 AND claim.attempt = ?8 AND claim.epoch = ?14
                  AND claim.owner_ref = ?15
                  AND claim.epoch = (SELECT max(latest.epoch) FROM wake_claim_epochs latest
                                      WHERE latest.wake_id = ?1 AND latest.attempt = ?8)
             ))",
        params![
            fence.wake_id,
            fence.work_item_id,
            fence.work_generation,
            fence.owner_generation,
            fence.route_ref,
            fence.payload_digest,
            fence.delivery_id,
            fence.attempt,
            fence.activation_id,
            fence.request_object_ref,
            fence.adapter_id,
            fence.provider_id,
            fence.idempotency_key,
            fence.consumer_epoch,
            fence.consumer_owner_ref,
        ],
        |row| row.get(0),
    )?;
    if !exact {
        return Err(WorkLedgerError::Refused(
            "uncertain provider authority changed during reconciliation".to_owned(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn append_delivery_observation(
    transaction: &Transaction<'_>,
    fence: &DeliveryFence,
    from_state: &str,
    to_state: &str,
    receipt_object_ref: &str,
    outcome_digest: &str,
    observed_at: &str,
) -> WorkLedgerResult<()> {
    let sequence: u64 = transaction.query_row(
        "SELECT coalesce(max(sequence), 0) + 1
           FROM provider_delivery_observations WHERE delivery_id = ?1",
        [&fence.delivery_id],
        |row| row.get(0),
    )?;
    let observation_id = opaque_ref(
        "ro",
        &format!(
            "shipyard-provider-delivery-observation-v1\n{}\n{}\n{}\n{}\n{}",
            fence.delivery_id, sequence, from_state, to_state, outcome_digest
        ),
    );
    transaction.execute(
        "INSERT INTO provider_delivery_observations
         (observation_id, delivery_id, sequence, work_generation, owner_generation,
          from_state, to_state, receipt_object_ref, outcome_digest, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            observation_id,
            fence.delivery_id,
            sequence,
            fence.work_generation,
            fence.owner_generation,
            from_state,
            to_state,
            receipt_object_ref,
            outcome_digest,
            observed_at,
        ],
    )?;
    Ok(())
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
