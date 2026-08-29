//! Protected native provider request and receipt publication.
//!
//! Publication binds an already claimed delivery to the exact Subrouter route
//! and digest-pinned wrapper in the v7 protected-object store. The default-off
//! consumer below may cross the external-call boundary only after that exact
//! request is recovered, and it reconciles rather than redispatches after the
//! durable delivery-start fence.

use std::path::Component;

use serde::{Deserialize, Serialize};

use super::delivery::{DeliveryClaim, DeliveryRouteIdentity, StartedDelivery};
use super::protected_objects::{ProtectedObjectKind, ProtectedObjectRecord};
use super::route::{ProviderRoute, TerminalRoute};
use super::{WorkLedger, WorkLedgerError, WorkLedgerResult, digest, validate_digest};
use crate::provider_wrapper::{
    FreshResumeExpectationV1, ProtectedProviderResponseV1, ProviderDeliveryFenceV1,
    ProviderLaunchOptionsV1, ProviderTerminalRouteV1, ProviderWrapperConfig,
    ProviderWrapperEnvironment, ProviderWrapperOperationV1, ProviderWrapperRequestV1,
    ProviderWrapperResponseV1, ProviderWrapperRunResult, SubrouterRoutingV1, run_provider_wrapper,
    validate_config, validate_protected_response, validate_request,
};

const NATIVE_PROVIDER_SCHEMA_VERSION: u32 = 1;
const MAX_HEAD_AUTHORITY_AGE_SECONDS: i64 = 300;

/// Fresh GitHub App installation proof for the exact repository head.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RepositoryHeadAuthorityV1 {
    pub(super) repository_id: String,
    pub(super) installation_ref: String,
    pub(super) repository: String,
    pub(super) head_sha: String,
    pub(super) observed_at: chrono::DateTime<chrono::Utc>,
    pub(super) receipt_digest: String,
}

/// Secret-bearing request persisted before Shipyard may contact Subrouter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeProviderRequestV1 {
    pub(crate) schema_version: u32,
    /// The complete terminal/agent/provider route remains axis-separated.
    pub(crate) route: DeliveryRouteIdentity,
    pub(crate) head_authority: RepositoryHeadAuthorityV1,
    pub(crate) wrapper: ProviderWrapperRequestV1,
}

/// Route-attested native receipt stored instead of the bare provider response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeProviderReceiptV1 {
    pub(crate) schema_version: u32,
    pub(crate) request_digest: String,
    pub(crate) route_integrity: String,
    pub(crate) native_session_ref: String,
    pub(crate) native_resume_ref: String,
    pub(crate) routing_generation: u64,
    pub(crate) launch_generation: u64,
    pub(crate) launch_revision: u64,
    pub(crate) agent_adapter_generation: u64,
    pub(crate) agent_adapter_revision: u64,
    pub(crate) response: ProviderWrapperResponseV1,
}

/// Durable request publication safe to pass to the future consumer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PublishedProviderRequest {
    pub(super) object: ProtectedObjectRecord,
    pub(super) request: NativeProviderRequestV1,
    pub(super) canonical_bytes: Vec<u8>,
    pub(super) digest: String,
}

/// Durable receipt publication; provider acceptance is not resume acceptance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PublishedProviderReceipt {
    pub(super) object: ProtectedObjectRecord,
    pub(super) digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeProviderTickResult {
    NoWork,
    Progressed,
}

enum NativeProviderAction {
    Claimed(DeliveryClaim, PublishedProviderRequest),
    Started(StartedDelivery),
    Uncertain(DeliveryClaim, PublishedProviderRequest),
}

pub(super) trait NativeWrapperRunner {
    fn run(
        &mut self,
        config: &ProviderWrapperConfig,
        environment: &ProviderWrapperEnvironment,
        request: &ProviderWrapperRequestV1,
    ) -> Result<ProviderWrapperRunResult, String>;
}

struct ProductionNativeWrapperRunner;

impl NativeWrapperRunner for ProductionNativeWrapperRunner {
    fn run(
        &mut self,
        config: &ProviderWrapperConfig,
        environment: &ProviderWrapperEnvironment,
        request: &ProviderWrapperRequestV1,
    ) -> Result<ProviderWrapperRunResult, String> {
        run_provider_wrapper(config, environment, request).map_err(|error| error.to_string())
    }
}

impl WorkLedger {
    /// Consume or reconcile at most one already-published native request.
    /// Pending wakes without a protected request remain untouched.
    pub(crate) fn native_provider_tick(
        &self,
        config: &ProviderWrapperConfig,
        environment: &ProviderWrapperEnvironment,
        authorized_repositories: &[String],
    ) -> WorkLedgerResult<NativeProviderTickResult> {
        self.native_provider_tick_with(
            config,
            environment,
            authorized_repositories,
            &mut ProductionNativeWrapperRunner,
        )
    }

    pub(super) fn native_provider_tick_with(
        &self,
        config: &ProviderWrapperConfig,
        environment: &ProviderWrapperEnvironment,
        authorized_repositories: &[String],
        runner: &mut impl NativeWrapperRunner,
    ) -> WorkLedgerResult<NativeProviderTickResult> {
        // One thread-reentrant writer-domain lease spans selection, the
        // delivery-start fence, the external call, and receipt/state commit.
        // Nested ledger writers reuse it; a sandbox snapshot can never split
        // one publication saga into misleading before/after evidence.
        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(self.writer_parent()?)?;
        let Some(action) = self.next_native_provider_action(config, authorized_repositories)?
        else {
            return Ok(NativeProviderTickResult::NoWork);
        };
        match action {
            NativeProviderAction::Claimed(claim, request) => {
                let started = self.mark_delivery_started(&claim)?;
                let result = runner.run(config, environment, &request.request.wrapper);
                self.finish_native_submit(&started, &request, result)?;
            }
            NativeProviderAction::Started(started) => {
                self.mark_delivery_uncertain(
                    &started,
                    &digest(b"daemon-recovered-started-provider-delivery"),
                )?;
            }
            NativeProviderAction::Uncertain(claim, request) => {
                self.reconcile_native_provider(config, environment, &claim, &request, runner)?;
            }
        }
        Ok(NativeProviderTickResult::Progressed)
    }

    fn next_native_provider_action(
        &self,
        config: &ProviderWrapperConfig,
        authorized_repositories: &[String],
    ) -> WorkLedgerResult<Option<NativeProviderAction>> {
        let connection = self.connect_read_only()?;
        let mut statement = connection.prepare(
            "SELECT state, claim_payload_json, delivery_started_at, delivery_start_digest
             FROM outbox
             WHERE state IN ('uncertain', 'delivery_started', 'claimed')
               AND claim_payload_json IS NOT NULL
             ORDER BY CASE state WHEN 'uncertain' THEN 0 WHEN 'delivery_started' THEN 1 ELSE 2 END,
                      created_at, wake_id LIMIT 4097",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.len() > 4096 {
            return Err(WorkLedgerError::Refused(
                "native provider candidate scan exceeds its bound".to_owned(),
            ));
        }
        drop(statement);
        drop(connection);
        for (state, claim_bytes, started_at, start_digest) in rows {
            let claim: DeliveryClaim = serde_json::from_slice(&claim_bytes).map_err(|_| {
                WorkLedgerError::Refused("native provider claim is malformed".to_owned())
            })?;
            claim.validate_identity()?;
            let Some(request) = self.published_request_for_claim(&claim)? else {
                continue;
            };
            validate_config(config).map_err(wrapper_refusal)?;
            validate_request(config, &request.request.wrapper).map_err(wrapper_refusal)?;
            if request.request.wrapper.operation != ProviderWrapperOperationV1::Submit
                || request.request.wrapper.provider_id != claim.route.agent_kind
                || request.request.wrapper.adapter_id != "subrouter"
            {
                return Err(WorkLedgerError::Refused(
                    "published provider request is not an exact Subrouter submission".to_owned(),
                ));
            }
            let repository = &request.request.wrapper.resume_expectation.repository;
            if !authorized_repositories
                .iter()
                .any(|candidate| candidate == repository)
            {
                continue;
            }
            return match state.as_str() {
                "claimed" => Ok(Some(NativeProviderAction::Claimed(claim, request))),
                "delivery_started" => {
                    let started_at = started_at
                        .ok_or_else(|| {
                            WorkLedgerError::Refused(
                                "started provider delivery lacks its timestamp".to_owned(),
                            )
                        })?
                        .parse::<chrono::DateTime<chrono::Utc>>()
                        .map_err(|_| {
                            WorkLedgerError::Refused(
                                "started provider delivery timestamp is malformed".to_owned(),
                            )
                        })?;
                    let started = StartedDelivery {
                        claim,
                        started_at,
                        start_identity_digest: start_digest.ok_or_else(|| {
                            WorkLedgerError::Refused(
                                "started provider delivery lacks its exact digest".to_owned(),
                            )
                        })?,
                    };
                    started.validate_identity()?;
                    Ok(Some(NativeProviderAction::Started(started)))
                }
                "uncertain" => Ok(Some(NativeProviderAction::Uncertain(claim, request))),
                _ => unreachable!("query restricts native provider states"),
            };
        }
        Ok(None)
    }

    fn published_request_for_claim(
        &self,
        claim: &DeliveryClaim,
    ) -> WorkLedgerResult<Option<PublishedProviderRequest>> {
        let expected_fence = delivery_fence(claim);
        let mut matches = self
            .protected_objects_for_work_kind(&claim.work_id, ProtectedObjectKind::ProviderRequest)?
            .into_iter()
            .map(|(object, bytes)| {
                let request =
                    serde_json::from_slice::<NativeProviderRequestV1>(&bytes).map_err(|_| {
                        WorkLedgerError::Refused(
                            "protected provider request is not strict schema v1".to_owned(),
                        )
                    })?;
                let request_digest = digest(&bytes);
                Ok(PublishedProviderRequest {
                    object,
                    request,
                    canonical_bytes: bytes,
                    digest: request_digest,
                })
            })
            .collect::<WorkLedgerResult<Vec<_>>>()?
            .into_iter()
            .filter(|published| {
                published.request.schema_version == NATIVE_PROVIDER_SCHEMA_VERSION
                    && published.request.wrapper.delivery_fence == expected_fence
                    && published.request.route == claim.route
                    && published.request.wrapper.resume_expectation.head_sha == claim.head_sha
            });
        let first = matches.next();
        if matches.next().is_some() {
            return Err(WorkLedgerError::Refused(
                "delivery has ambiguous protected provider requests".to_owned(),
            ));
        }
        Ok(first)
    }

    fn finish_native_submit(
        &self,
        started: &StartedDelivery,
        request: &PublishedProviderRequest,
        result: Result<ProviderWrapperRunResult, String>,
    ) -> WorkLedgerResult<()> {
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.mark_delivery_uncertain(started, &digest(error.as_bytes()))?;
                return Ok(());
            }
        };
        match result {
            ProviderWrapperRunResult::Delivered {
                provider_session_ref: _,
                response_receipt,
                ..
            } => {
                let receipt =
                    self.publish_native_provider_receipt(started, request, &response_receipt)?;
                let delivery = super::delivery::DeliveryReceipt::new(
                    started,
                    request
                        .request
                        .wrapper
                        .subrouter_routing
                        .native_session_ref
                        .clone(),
                    receipt.digest,
                )?;
                self.acknowledge_delivery(started, &delivery)
            }
            ProviderWrapperRunResult::Retryable {
                response_receipt, ..
            }
            | ProviderWrapperRunResult::Rejected {
                response_receipt, ..
            } => {
                let receipt =
                    self.publish_native_provider_receipt(started, request, &response_receipt)?;
                self.mark_delivery_uncertain(started, &receipt.digest)
            }
            ProviderWrapperRunResult::Uncertain {
                evidence_digest,
                response_receipt,
            } => {
                let evidence = if let Some(response) = response_receipt {
                    self.publish_native_provider_receipt(started, request, &response)?
                        .digest
                } else {
                    evidence_digest
                };
                self.mark_delivery_uncertain(started, &evidence)
            }
        }
    }

    fn reconcile_native_provider(
        &self,
        config: &ProviderWrapperConfig,
        environment: &ProviderWrapperEnvironment,
        claim: &DeliveryClaim,
        request: &PublishedProviderRequest,
        runner: &mut impl NativeWrapperRunner,
    ) -> WorkLedgerResult<()> {
        let mut reconcile_request = request.request.wrapper.clone();
        reconcile_request.operation = ProviderWrapperOperationV1::Reconcile;
        validate_request(config, &reconcile_request).map_err(wrapper_refusal)?;
        let Ok(result) = runner.run(config, environment, &reconcile_request) else {
            return Ok(());
        };
        let start_digest = self.uncertain_start_digest(claim)?;
        match result {
            ProviderWrapperRunResult::Delivered {
                provider_session_ref: _,
                response_receipt,
                ..
            } => {
                let receipt = self.publish_native_reconciliation_receipt(
                    claim,
                    request,
                    &reconcile_request,
                    &response_receipt,
                )?;
                let delivery = super::delivery::DeliveryReceipt::accepted_after_uncertainty(
                    claim,
                    &start_digest,
                    request
                        .request
                        .wrapper
                        .subrouter_routing
                        .native_session_ref
                        .clone(),
                    receipt.digest,
                )?;
                self.reconcile_uncertain_delivery(claim, &delivery)
            }
            ProviderWrapperRunResult::Retryable {
                response_receipt, ..
            }
            | ProviderWrapperRunResult::Rejected {
                response_receipt, ..
            } => {
                let receipt = self.publish_native_reconciliation_receipt(
                    claim,
                    request,
                    &reconcile_request,
                    &response_receipt,
                )?;
                let delivery = super::delivery::DeliveryReceipt::not_delivered_after_uncertainty(
                    claim,
                    &start_digest,
                    &receipt.digest,
                )?;
                self.reconcile_uncertain_delivery(claim, &delivery)
            }
            ProviderWrapperRunResult::Uncertain {
                response_receipt: Some(response),
                ..
            } => {
                self.publish_native_reconciliation_receipt(
                    claim,
                    request,
                    &reconcile_request,
                    &response,
                )?;
                Ok(())
            }
            ProviderWrapperRunResult::Uncertain { .. } => Ok(()),
        }
    }

    fn uncertain_start_digest(&self, claim: &DeliveryClaim) -> WorkLedgerResult<String> {
        let connection = self.connect_read_only()?;
        connection
            .query_row(
                "SELECT delivery_start_digest FROM outbox
                 WHERE wake_id = ?1 AND claim_id = ?2 AND state = 'uncertain'",
                rusqlite::params![claim.wake_id, claim.claim_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Recover the exact protected resume expectation for one acknowledged
    /// delivery. Context acknowledgement must never accept caller-invented
    /// reconstruction evidence.
    pub(super) fn expected_resume_context_digest(
        &self,
        claim: &DeliveryClaim,
    ) -> WorkLedgerResult<String> {
        let expected_fence = delivery_fence(claim);
        let mut matches = self
            .protected_objects_for_work_kind(&claim.work_id, ProtectedObjectKind::ProviderRequest)?
            .into_iter()
            .map(|(_record, bytes)| {
                serde_json::from_slice::<NativeProviderRequestV1>(&bytes).map_err(|_| {
                    WorkLedgerError::Refused(
                        "protected provider request is not strict schema v1".to_owned(),
                    )
                })
            })
            .collect::<WorkLedgerResult<Vec<_>>>()?
            .into_iter()
            .filter(|request| {
                request.schema_version == NATIVE_PROVIDER_SCHEMA_VERSION
                    && request.wrapper.delivery_fence == expected_fence
                    && request.route == claim.route
                    && request.wrapper.resume_expectation.head_sha == claim.head_sha
            });
        let request = matches.next().ok_or_else(|| {
            WorkLedgerError::Refused(
                "acknowledged delivery has no matching protected provider request".to_owned(),
            )
        })?;
        if matches.next().is_some() {
            return Err(WorkLedgerError::Refused(
                "acknowledged delivery has ambiguous protected provider requests".to_owned(),
            ));
        }
        validate_digest(
            "expected resume context",
            &request
                .wrapper
                .resume_expectation
                .expected_resume_context_digest,
        )?;
        Ok(request
            .wrapper
            .resume_expectation
            .expected_resume_context_digest)
    }

    /// Publish an exact request without crossing the external delivery boundary.
    #[allow(clippy::too_many_arguments)] // Keep every independently fenced authority axis explicit.
    pub(super) fn publish_native_provider_request(
        &self,
        claim: &DeliveryClaim,
        operation: ProviderWrapperOperationV1,
        config: &ProviderWrapperConfig,
        subrouter_routing: SubrouterRoutingV1,
        head_authority: RepositoryHeadAuthorityV1,
        resume_expectation: FreshResumeExpectationV1,
        launch_options: ProviderLaunchOptionsV1,
    ) -> WorkLedgerResult<PublishedProviderRequest> {
        validate_native_wrapper_route(
            claim,
            config,
            &subrouter_routing,
            &head_authority,
            &resume_expectation,
            &launch_options,
        )?;
        let fence = delivery_fence(claim);
        let wrapper = ProviderWrapperRequestV1 {
            schema_version: 1,
            operation,
            provider_id: config.provider_id.clone(),
            adapter_id: config.adapter_id.clone(),
            delivery_fence: fence,
            subrouter_routing,
            resume_expectation,
            launch_options,
        };
        validate_request(config, &wrapper).map_err(wrapper_refusal)?;
        let request = NativeProviderRequestV1 {
            schema_version: NATIVE_PROVIDER_SCHEMA_VERSION,
            route: claim.route.clone(),
            head_authority,
            wrapper,
        };
        let canonical_bytes = serde_json::to_vec(&request).map_err(|_| {
            WorkLedgerError::Refused("native provider request cannot be serialized".to_owned())
        })?;
        let request_digest = digest(&canonical_bytes);

        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(self.writer_parent()?)?;
        let mut connection = self.delivery_connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        super::delivery::verify_claim(&transaction, claim, "claimed")?;
        transaction.commit()?;
        let object = self.put_protected_object_with_writer_domain(
            &claim.work_id,
            ProtectedObjectKind::ProviderRequest,
            None,
            &request_digest,
            &canonical_bytes,
        )?;
        Ok(PublishedProviderRequest {
            object,
            request,
            canonical_bytes,
            digest: request_digest,
        })
    }

    /// Persist the strict wrapper response before acknowledging delivery.
    pub(super) fn publish_native_provider_receipt(
        &self,
        started: &StartedDelivery,
        request: &PublishedProviderRequest,
        response: &ProtectedProviderResponseV1,
    ) -> WorkLedgerResult<PublishedProviderReceipt> {
        validate_published_request_for_started(started, request)?;
        validate_protected_response(&request.request.wrapper, response).map_err(wrapper_refusal)?;
        let decoded: ProviderWrapperResponseV1 = serde_json::from_slice(&response.canonical_bytes)
            .map_err(|_| {
                WorkLedgerError::Refused("provider response is not strict schema v1".to_owned())
            })?;
        let wrapper = &request.request.wrapper;
        if decoded.schema_version != 1
            || decoded.operation != wrapper.operation
            || decoded.provider_id != wrapper.provider_id
            || decoded.adapter_id != wrapper.adapter_id
            || decoded.idempotency_key != wrapper.delivery_fence.idempotency_key
        {
            return Err(WorkLedgerError::Refused(
                "provider response does not match its exact published request".to_owned(),
            ));
        }

        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(self.writer_parent()?)?;
        let mut connection = self.delivery_connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        super::delivery::verify_started(&transaction, started)?;
        transaction.commit()?;
        let receipt = NativeProviderReceiptV1 {
            schema_version: NATIVE_PROVIDER_SCHEMA_VERSION,
            request_digest: request.digest.clone(),
            route_integrity: request.request.route.route_integrity.clone(),
            native_session_ref: wrapper.subrouter_routing.native_session_ref.clone(),
            native_resume_ref: wrapper.subrouter_routing.native_resume_ref.clone(),
            routing_generation: request.request.route.route_revision,
            launch_generation: request.request.route.launch_generation,
            launch_revision: request.request.route.launch_revision,
            agent_adapter_generation: request.request.route.agent.adapter.generation,
            agent_adapter_revision: request.request.route.agent.adapter.revision,
            response: decoded,
        };
        let receipt_bytes = serde_json::to_vec(&receipt).map_err(|_| {
            WorkLedgerError::Refused("native provider receipt cannot be serialized".to_owned())
        })?;
        let receipt_digest = digest(&receipt_bytes);
        let object = self.put_protected_object_with_writer_domain(
            &started.claim.work_id,
            ProtectedObjectKind::ProviderReceipt,
            None,
            &receipt_digest,
            &receipt_bytes,
        )?;
        Ok(PublishedProviderReceipt {
            object,
            digest: receipt_digest,
        })
    }

    fn publish_native_reconciliation_receipt(
        &self,
        claim: &DeliveryClaim,
        original: &PublishedProviderRequest,
        reconcile_request: &ProviderWrapperRequestV1,
        response: &ProtectedProviderResponseV1,
    ) -> WorkLedgerResult<PublishedProviderReceipt> {
        if reconcile_request.operation != ProviderWrapperOperationV1::Reconcile
            || reconcile_request.delivery_fence != original.request.wrapper.delivery_fence
            || original.request.wrapper.operation != ProviderWrapperOperationV1::Submit
        {
            return Err(WorkLedgerError::Refused(
                "provider reconciliation does not derive from the exact protected submission"
                    .to_owned(),
            ));
        }
        validate_protected_response(reconcile_request, response).map_err(wrapper_refusal)?;
        let decoded: ProviderWrapperResponseV1 = serde_json::from_slice(&response.canonical_bytes)
            .map_err(|_| {
                WorkLedgerError::Refused(
                    "provider reconciliation response is not strict schema v1".to_owned(),
                )
            })?;
        if decoded.operation != ProviderWrapperOperationV1::Reconcile
            || decoded.provider_id != reconcile_request.provider_id
            || decoded.adapter_id != reconcile_request.adapter_id
            || decoded.idempotency_key != reconcile_request.delivery_fence.idempotency_key
        {
            return Err(WorkLedgerError::Refused(
                "provider reconciliation response differs from its exact fence".to_owned(),
            ));
        }
        let receipt = NativeProviderReceiptV1 {
            schema_version: NATIVE_PROVIDER_SCHEMA_VERSION,
            request_digest: original.digest.clone(),
            route_integrity: original.request.route.route_integrity.clone(),
            native_session_ref: reconcile_request
                .subrouter_routing
                .native_session_ref
                .clone(),
            native_resume_ref: reconcile_request
                .subrouter_routing
                .native_resume_ref
                .clone(),
            routing_generation: original.request.route.route_revision,
            launch_generation: original.request.route.launch_generation,
            launch_revision: original.request.route.launch_revision,
            agent_adapter_generation: original.request.route.agent.adapter.generation,
            agent_adapter_revision: original.request.route.agent.adapter.revision,
            response: decoded,
        };
        let bytes = serde_json::to_vec(&receipt).map_err(|_| {
            WorkLedgerError::Refused(
                "provider reconciliation receipt cannot be serialized".to_owned(),
            )
        })?;
        let receipt_digest = digest(&bytes);
        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(self.writer_parent()?)?;
        let mut connection = self.delivery_connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        super::delivery::verify_claim(&transaction, claim, "uncertain")?;
        transaction.commit()?;
        let object = self.put_protected_object_with_writer_domain(
            &claim.work_id,
            ProtectedObjectKind::ProviderReceipt,
            None,
            &receipt_digest,
            &bytes,
        )?;
        Ok(PublishedProviderReceipt {
            object,
            digest: receipt_digest,
        })
    }
}

#[allow(clippy::too_many_lines)] // Preserve the ordered route/authority refusal chain in one audit surface.
fn validate_native_wrapper_route(
    claim: &DeliveryClaim,
    config: &ProviderWrapperConfig,
    routing: &SubrouterRoutingV1,
    authority: &RepositoryHeadAuthorityV1,
    resume: &FreshResumeExpectationV1,
    launch_options: &ProviderLaunchOptionsV1,
) -> WorkLedgerResult<()> {
    claim.validate_identity()?;
    validate_config(config).map_err(wrapper_refusal)?;
    let normalized = config
        .executable_path
        .components()
        .collect::<std::path::PathBuf>();
    if !config.executable_path.is_absolute()
        || normalized != config.executable_path
        || config
            .executable_path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(WorkLedgerError::Refused(
            "provider wrapper path must be normalized and absolute".to_owned(),
        ));
    }
    if config.adapter_id != "subrouter"
        || claim.route.provider_kind != "subrouter"
        || !matches!(claim.route.provider, ProviderRoute::Subrouter { .. })
    {
        return Err(WorkLedgerError::Refused(
            "native provider publication requires the exact Subrouter route; direct fallback is forbidden"
                .to_owned(),
        ));
    }
    let terminal_matches = match (&routing.terminal, &claim.route.terminal) {
        (
            ProviderTerminalRouteV1::Cmux {
                workspace_ref,
                pane_ref,
                surface_ref,
            },
            TerminalRoute::Cmux {
                workspace_ref: expected_workspace,
                pane_ref: expected_pane,
                surface_ref: expected_surface,
            },
        ) => {
            workspace_ref == expected_workspace.as_str()
                && pane_ref == expected_pane.as_str()
                && surface_ref == expected_surface.as_str()
        }
        (
            ProviderTerminalRouteV1::HerdR {
                session_ref,
                workspace_ref,
                tab_ref,
                pane_ref,
            },
            TerminalRoute::HerdR {
                session_ref: expected_session,
                workspace_ref: expected_workspace,
                tab_ref: expected_tab,
                pane_ref: expected_pane,
            },
        ) => {
            session_ref == expected_session.as_str()
                && workspace_ref == expected_workspace.as_str()
                && tab_ref == expected_tab.as_str()
                && pane_ref == expected_pane.as_str()
        }
        _ => false,
    };
    let provider_matches = matches!(
        &claim.route.provider,
        ProviderRoute::Subrouter { server_ref, route_ref }
            if routing.server_ref == server_ref.as_str()
                && routing.provider_route_ref == route_ref.as_str()
    );
    if !terminal_matches
        || !provider_matches
        || routing.native_session_ref != claim.route.native_session_ref
        || routing.native_resume_ref != claim.route.native_resume_ref
        || routing.account_ref != claim.route.account_ref
        || routing.model_ref != claim.route.model_ref
        || routing.wrapper_ref != claim.route.wrapper_ref
        || routing.companion_sha256 != config.executable_sha256
        || routing.session_headers_ref != claim.route.session_headers_ref
        || routing.session_headers_file.sha256 != claim.route.session_headers_sha256
        || routing.routing_generation != claim.route.route_revision
        || routing.launch_generation != claim.route.launch_generation
        || routing.launch_revision != claim.route.launch_revision
        || routing.agent_adapter_generation != claim.route.agent.adapter.generation
        || routing.agent_adapter_revision != claim.route.agent.adapter.revision
        || routing.agent_executable_sha256
            != claim.route.agent.adapter.implementation_sha256.as_str()
    {
        return Err(WorkLedgerError::Refused(
            "protected Subrouter routing material differs from the claimed route".to_owned(),
        ));
    }
    validate_authenticated_launch_bindings(claim, config, routing, launch_options)?;
    if config.provider_id != claim.route.agent_kind
        || config.executable_sha256 != claim.route.executable_sha256
        || resume.head_sha != claim.head_sha
        || authority.head_sha != claim.head_sha
        || authority.repository != resume.repository
    {
        return Err(WorkLedgerError::Refused(
            "provider wrapper, agent, or exact-head authority differs from the claimed route"
                .to_owned(),
        ));
    }
    super::route::OpaqueRef::parse(authority.installation_ref.clone()).map_err(|_| {
        WorkLedgerError::Refused("GitHub App installation authority is invalid".to_owned())
    })?;
    if !authority.repository_id.starts_with("R_") || authority.repository_id.len() > 128 {
        return Err(WorkLedgerError::Refused(
            "GitHub repository authority is invalid".to_owned(),
        ));
    }
    validate_digest("GitHub App head authority", &authority.receipt_digest)?;
    let authority_age = claim
        .claimed_at
        .signed_duration_since(authority.observed_at)
        .num_seconds();
    if !(0..=MAX_HEAD_AUTHORITY_AGE_SECONDS).contains(&authority_age) {
        return Err(WorkLedgerError::Refused(
            "GitHub App head authority is stale or postdates the delivery claim".to_owned(),
        ));
    }
    // These opaque identities are deliberately preserved independently. The
    // canonical request serializes all of them plus route revision (routing
    // generation), so a terminal move cannot erase provider provenance.
    for value in [
        &claim.route.account_ref,
        &claim.route.model_ref,
        &claim.route.wrapper_ref,
        &claim.route.session_headers_ref,
    ] {
        super::route::OpaqueRef::parse(value.clone()).map_err(|_| {
            WorkLedgerError::Refused(
                "native provider route contains an invalid opaque ref".to_owned(),
            )
        })?;
    }
    validate_digest(
        "provider session headers",
        &claim.route.session_headers_sha256,
    )?;
    if claim.route.route_revision == 0 {
        return Err(WorkLedgerError::Refused(
            "provider routing generation must be nonzero".to_owned(),
        ));
    }
    Ok(())
}

fn validate_authenticated_launch_bindings(
    claim: &DeliveryClaim,
    config: &ProviderWrapperConfig,
    routing: &SubrouterRoutingV1,
    launch_options: &ProviderLaunchOptionsV1,
) -> WorkLedgerResult<()> {
    #[derive(Serialize)]
    struct FileBinding<'a> {
        path: &'a str,
        sha256: &'a str,
    }
    #[derive(Serialize)]
    struct WrapperBinding<'a> {
        companion_path: &'a str,
        companion_sha256: &'a str,
        subrouter_path: &'a str,
        subrouter_sha256: &'a str,
    }
    let companion_path = config.executable_path.to_str().ok_or_else(|| {
        WorkLedgerError::Refused("provider companion path is not UTF-8".to_owned())
    })?;
    let model_id = launch_options.model_id.as_deref().ok_or_else(|| {
        WorkLedgerError::Refused("native Subrouter launch requires an exact model id".to_owned())
    })?;
    let expected_resume =
        super::route::OpaqueRef::derive("native-resume-id", routing.native_resume_id.as_bytes());
    let account = serde_json::to_vec(&FileBinding {
        path: &routing.account_file.path,
        sha256: &routing.account_file.sha256,
    })
    .map_err(|_| WorkLedgerError::Refused("account binding is unserializable".to_owned()))?;
    let expected_account = super::route::OpaqueRef::derive("subrouter-account-file", &account);
    let expected_model = super::route::OpaqueRef::derive("subrouter-model-id", model_id.as_bytes());
    let wrapper = serde_json::to_vec(&WrapperBinding {
        companion_path,
        companion_sha256: &routing.companion_sha256,
        subrouter_path: &routing.subrouter_executable_path,
        subrouter_sha256: &routing.subrouter_executable_sha256,
    })
    .map_err(|_| WorkLedgerError::Refused("wrapper binding is unserializable".to_owned()))?;
    let expected_wrapper = super::route::OpaqueRef::derive("subrouter-wrapper", &wrapper);
    let headers = serde_json::to_vec(&FileBinding {
        path: &routing.session_headers_file.path,
        sha256: &routing.session_headers_file.sha256,
    })
    .map_err(|_| WorkLedgerError::Refused("session header binding is unserializable".to_owned()))?;
    let expected_headers =
        super::route::OpaqueRef::derive("subrouter-session-headers-file", &headers);
    if expected_resume.as_str() != claim.route.native_resume_ref
        || expected_account.as_str() != claim.route.account_ref
        || expected_model.as_str() != claim.route.model_ref
        || expected_wrapper.as_str() != claim.route.wrapper_ref
        || expected_headers.as_str() != claim.route.session_headers_ref
    {
        return Err(WorkLedgerError::Refused(
            "live Subrouter launch material is not authenticated by the claimed route".to_owned(),
        ));
    }
    Ok(())
}

fn validate_published_request_for_started(
    started: &StartedDelivery,
    request: &PublishedProviderRequest,
) -> WorkLedgerResult<()> {
    started.validate_identity()?;
    let wrapper = &request.request.wrapper;
    let expected_fence = delivery_fence(&started.claim);
    if request.request.schema_version != NATIVE_PROVIDER_SCHEMA_VERSION
        || request.request.route != started.claim.route
        || wrapper.delivery_fence != expected_fence
        || digest(&request.canonical_bytes) != request.digest
        || serde_json::to_vec(&request.request).map_err(|_| {
            WorkLedgerError::Refused("published provider request cannot be reserialized".to_owned())
        })? != request.canonical_bytes
    {
        return Err(WorkLedgerError::Refused(
            "published provider request does not match the started delivery".to_owned(),
        ));
    }
    Ok(())
}

fn delivery_fence(claim: &DeliveryClaim) -> ProviderDeliveryFenceV1 {
    let mut fence = ProviderDeliveryFenceV1 {
        ledger_incarnation_ref: claim.ledger_incarnation_ref.clone(),
        dispatcher_epoch_ref: claim.dispatcher_epoch_ref.clone(),
        wake_id: claim.wake_id.clone(),
        claim_id: claim.claim_id.clone(),
        work_item_id: claim.work_id.clone(),
        work_generation: claim.work_generation,
        owner_generation: claim.owner_generation,
        route_ref: claim.route_ref.clone(),
        payload_digest: claim.payload_digest.clone(),
        attempt: claim.claim_attempt,
        claimant_ref: claim.claimant_ref.clone(),
        idempotency_key: String::new(),
    };
    fence.bind_idempotency_key();
    fence
}

fn wrapper_refusal(error: impl std::fmt::Display) -> WorkLedgerError {
    WorkLedgerError::Refused(format!("provider wrapper request refused: {error}"))
}
