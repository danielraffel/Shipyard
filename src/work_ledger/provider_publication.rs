//! Protected native provider request and receipt publication.
//!
//! This module performs no provider or terminal calls. It binds an already
//! claimed delivery to the exact Subrouter route and digest-pinned wrapper,
//! then publishes canonical request/receipt bytes in the v7 protected-object
//! store. A later default-off consumer may cross the external-call boundary
//! only after this request publication succeeds.

use std::path::Component;

use serde::{Deserialize, Serialize};

use super::delivery::{DeliveryClaim, DeliveryRouteIdentity, StartedDelivery};
use super::protected_objects::{ProtectedObjectKind, ProtectedObjectRecord};
use super::route::ProviderRoute;
use super::{WorkLedger, WorkLedgerError, WorkLedgerResult, digest, validate_digest};
use crate::provider_wrapper::{
    FreshResumeExpectationV1, ProtectedProviderResponseV1, ProviderDeliveryFenceV1,
    ProviderLaunchOptionsV1, ProviderWrapperConfig, ProviderWrapperOperationV1,
    ProviderWrapperRequestV1, ProviderWrapperResponseV1, validate_config,
    validate_protected_response, validate_request,
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

impl WorkLedger {
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
            .map(|bytes| {
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
    pub(super) fn publish_native_provider_request(
        &self,
        claim: &DeliveryClaim,
        operation: ProviderWrapperOperationV1,
        config: &ProviderWrapperConfig,
        head_authority: RepositoryHeadAuthorityV1,
        resume_expectation: FreshResumeExpectationV1,
        launch_options: ProviderLaunchOptionsV1,
    ) -> WorkLedgerResult<PublishedProviderRequest> {
        validate_native_wrapper_route(claim, config, &head_authority, &resume_expectation)?;
        let fence = delivery_fence(claim);
        let wrapper = ProviderWrapperRequestV1 {
            schema_version: 1,
            operation,
            provider_id: config.provider_id.clone(),
            adapter_id: config.adapter_id.clone(),
            delivery_fence: fence,
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
}

fn validate_native_wrapper_route(
    claim: &DeliveryClaim,
    config: &ProviderWrapperConfig,
    authority: &RepositoryHeadAuthorityV1,
    resume: &FreshResumeExpectationV1,
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
