//! Idempotent native publication of one exact continuation authority.
//!
//! Publication is deliberately inert: it records identity, checkpoint,
//! route, profile, and continuation contracts, but it never creates a wake.
//! The daemon's exact-head actionable producer is the sole bridge from a
//! managed record to `dispatching` plus one transactional outbox row.

use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::dispatch::{FreshAgentLaunchProfile, WakeEnvelope, WakeProfileResolver};
use super::registry::RouteRegistration;
use super::route::{
    AdapterAxis, AdapterBindingRecord, AgentName, AgentRoute, AgentRouteRecord,
    LaunchProfileRecord, NativeDeliveryAuthorityRecord, NativeSessionRoute, OpaqueRef,
    ProviderRoute, ProviderRouteRecord, RouteProvenanceRecord, Sha256Digest, TerminalRoute,
    TerminalRouteRecord,
};
use super::{
    ContinuationSet, ImportCandidate, LifecycleState, OptionalExtension, WorkLedger,
    WorkLedgerError, WorkLedgerResult, digest, opaque_ref, params, validate_digest,
};
use crate::terminal_delivery_authority::TerminalCapabilityRequest;
use crate::workstream_continuation_config::WorkstreamContinuationConfig;

/// Complete normalized authority needed to publish one native continuation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativePublicationRequest {
    pub(crate) repository_provider: String,
    pub(crate) repository_id: String,
    /// Prior canonical coordinate authenticated by a provider stable-ID redirect.
    pub(crate) legacy_repository_alias: Option<String>,
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) head_sha: String,
    pub(crate) base_ref: String,
    pub(crate) base_sha: String,
    pub(crate) github_installation_id: u64,
    pub(crate) repo_policy_revision: u64,
    pub(crate) terminal_authority: TerminalCapabilityRequest,
    pub(crate) workstream_handle: String,
    pub(crate) plan_sha256: String,
    pub(crate) root_revision: u64,
    pub(crate) issue_revision: u64,
    pub(crate) projection_revision: u64,
    pub(crate) material_event_revision: u64,
    pub(crate) context_url: Option<String>,
    pub(crate) origin_machine: String,
    pub(crate) owner_id: String,
    pub(crate) owner_generation: u64,
    pub(crate) agent_provider: String,
    pub(crate) agent_session_id: String,
    pub(crate) route_account: String,
    pub(crate) route_model: String,
    pub(crate) route_wrapper: String,
    pub(crate) native_resume_digest: String,
    pub(crate) route_environment_digest: String,
    pub(crate) route_id: String,
    pub(crate) profile_generation: u64,
    pub(crate) profile_revision: u64,
    pub(crate) profile_provider: String,
    pub(crate) profile_digest: String,
    pub(crate) protected_profile_bytes: Vec<u8>,
    pub(crate) success_continuation_digest: String,
    pub(crate) failure_continuation_digest: String,
}

/// Stable dry-run/apply result; no private route or profile bytes are exposed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct NativePublicationReport {
    pub(crate) applied: bool,
    pub(crate) replay: bool,
    pub(crate) work_id: String,
    pub(crate) route_ref: String,
    pub(crate) wake_id: String,
    pub(crate) profile_digest: String,
    pub(crate) repo_policy_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct NativePolicyBindingV1 {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repository_id: Option<String>,
    repository: String,
    pull_request: u64,
    head_sha: String,
    repo_policy_revision: u64,
    work_id: String,
}

type LegacyProjectionBinding = (
    String,
    String,
    u64,
    u64,
    u64,
    u64,
    Option<String>,
    Option<String>,
    String,
    String,
);

type RepositoryCoordinateBinding = (
    String,
    String,
    String,
    String,
    u64,
    u64,
    u64,
    u64,
    String,
    String,
);

/// Exact protected-profile lookup used by the daemon's typed decoder.
pub(crate) struct ExactProtectedProfileResolver<'a, D> {
    ledger: &'a WorkLedger,
    decode: D,
}

impl<'a, D> ExactProtectedProfileResolver<'a, D> {
    #[allow(dead_code)] // Activated by the daemon wake-loop integration slice.
    pub(crate) fn new(ledger: &'a WorkLedger, decode: D) -> Self {
        Self { ledger, decode }
    }

    pub(crate) fn resolve_exact<P>(
        &mut self,
        work_id: &str,
        profile_digest: &str,
    ) -> WorkLedgerResult<P>
    where
        D: FnMut(&[u8]) -> WorkLedgerResult<P>,
        P: FreshAgentLaunchProfile,
    {
        let bytes = self
            .ledger
            .protected_launch_profile_bytes(work_id, profile_digest)?;
        (self.decode)(&bytes)
    }
}

impl<D, P> WakeProfileResolver for ExactProtectedProfileResolver<'_, D>
where
    D: FnMut(&[u8]) -> WorkLedgerResult<P>,
    P: FreshAgentLaunchProfile,
{
    type Profile = P;

    fn resolve(&mut self, wake: &WakeEnvelope) -> WorkLedgerResult<Self::Profile> {
        self.resolve_exact(&wake.work_item_id, &wake.payload_digest)
    }
}

impl WorkLedger {
    /// Whether the daemon has durably accepted responsibility for this wake.
    ///
    /// Publication alone is not consumer availability. This becomes true only
    /// after the daemon has completed a fenced provider delivery. Merely
    /// claiming, retrying, or becoming uncertain is not enough to let the
    /// originating agent relinquish the final monitoring obligation.
    #[cfg(all(test, unix))]
    pub(crate) fn native_wake_consumer_owns(&self, wake_id: &str) -> WorkLedgerResult<bool> {
        let connection = self.connect_read_only()?;
        let observed: Option<(String, bool)> = connection
            .query_row(
                "SELECT wake.state,
                        EXISTS(
                          SELECT 1 FROM wake_attempts attempt
                           WHERE attempt.wake_id = wake.wake_id
                             AND attempt.state IN
                               ('claimed', 'delivered', 'acknowledged', 'retry', 'uncertain')
                        )
                   FROM outbox wake WHERE wake.wake_id = ?1",
                [wake_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(observed.is_some_and(|(state, attempt)| {
            attempt && matches!(state.as_str(), "delivered" | "acknowledged")
        }))
    }

    pub(crate) fn plan_or_apply_native_continuation(
        state_dir: &std::path::Path,
        request: &NativePublicationRequest,
        policy: &WorkstreamContinuationConfig,
        apply: bool,
    ) -> WorkLedgerResult<NativePublicationReport> {
        // Authorization must precede even SQLite creation. A refused machine,
        // repository, or malformed profile is not a native ledger event.
        validate_request(request, policy)?;
        if !cfg!(unix) {
            return Err(WorkLedgerError::Refused(
                "native publication requires Unix policy-binding verification".to_owned(),
            ));
        }
        let ledger = Self::open_existing(state_dir)?.ok_or_else(|| {
            WorkLedgerError::Refused("explicit repository policy is unavailable".to_owned())
        })?;
        ledger.verify_repo_policy_revision(&request.repository, request.repo_policy_revision)?;
        if apply {
            let planned = ledger.publish_native_continuation(request, policy, false)?;
            // The v2 policy authority must be durable before the SQLite identity can
            // be enriched or moved. A crash can leave a harmless policy precursor,
            // never an enriched database that lacks its policy fence.
            persist_native_policy_binding(state_dir, request, &planned)?;
        }
        ledger.publish_native_continuation(request, policy, apply)
    }

    /// Plan or apply one exact native publication. Replays return the same IDs.
    pub(crate) fn publish_native_continuation(
        &self,
        request: &NativePublicationRequest,
        policy: &WorkstreamContinuationConfig,
        apply: bool,
    ) -> WorkLedgerResult<NativePublicationReport> {
        validate_request(request, policy)?;
        let (identities, legacy_identity) = self.select_publication_identities(request)?;
        let was_exact = self.publication_is_exact(request, &identities)?;
        if !apply {
            return Ok(native_publication_report(
                request,
                &identities,
                false,
                was_exact,
            ));
        }

        if !was_exact {
            self.ensure_native_work_item(request, &identities)?;
            if !legacy_identity {
                self.ensure_projection_binding(request, &identities.work_id)?;
            }
            self.ensure_continuations(request, &identities.work_id)?;
            self.advance_to_managed(&identities.work_id, request.owner_generation)?;

            let (route, adapters) = native_route(request, policy, &identities)?;
            for adapter in &adapters {
                self.ensure_adapter(adapter)?;
            }
            self.ensure_route(&route)?;
            self.put_protected_object(
                &identities.work_id,
                super::ProtectedObjectKind::LaunchProfile,
                Some(&identities.profile_ref),
                &request.profile_digest,
                &request.protected_profile_bytes,
            )?;
        }

        if !self.publication_is_exact(request, &identities)? {
            return Err(WorkLedgerError::Refused(
                "native publication was not exact after apply".to_owned(),
            ));
        }
        let enriched = legacy_identity
            && self.enrich_legacy_projection_repository_identity(
                &identities.work_id,
                &request.workstream_handle,
                &request.plan_sha256,
                request.root_revision,
                request.issue_revision,
                request.projection_revision,
                request.material_event_revision,
                &request.repository_provider,
                &request.repository_id,
                request
                    .legacy_repository_alias
                    .as_deref()
                    .unwrap_or(&request.repository),
                &request.repository,
                &request.head_sha,
                &identities.publication_digest,
                &identities.route_ref,
                &identities.profile_ref,
                &request.profile_digest,
                &request.success_continuation_digest,
                &request.failure_continuation_digest,
                request.pull_request,
                request.owner_generation,
            )?;
        let coordinate_changed =
            self.reconcile_projection_repository_coordinate(request, &identities.work_id)?;
        self.verify_exact_shadow_target(request)?;
        Ok(native_publication_report(
            request,
            &identities,
            !was_exact || enriched || coordinate_changed,
            was_exact && !enriched && !coordinate_changed,
        ))
    }

    #[allow(clippy::too_many_lines)] // Legacy and immutable identity selection share one refusal boundary.
    fn select_publication_identities(
        &self,
        request: &NativePublicationRequest,
    ) -> WorkLedgerResult<(PublicationIdentities, bool)> {
        let current = PublicationIdentities::new(request);
        let mut legacy_request = request.clone();
        if let Some(alias) = &request.legacy_repository_alias {
            legacy_request.repository.clone_from(alias);
        }
        let requested_legacy = PublicationIdentities::legacy(&legacy_request);
        let requested_legacy_work_id = requested_legacy.work_id.clone();
        let connection = self.connect_read_only()?;
        let current_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_items WHERE id = ?1)",
            [&current.work_id],
            |row| row.get(0),
        )?;
        let requested_legacy_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_items WHERE id = ?1)",
            [&requested_legacy.work_id],
            |row| row.get(0),
        )?;
        let mut identity_statement = connection.prepare(
            "SELECT binding.work_item_id, import.source_ref, work.source_digest,
                    route.route_ref, profile.profile_ref
               FROM workstream_projection_bindings binding
               JOIN work_items work ON work.id = binding.work_item_id
               JOIN imports import
                 ON import.work_item_id = work.id AND import.content_digest = work.source_digest
               JOIN route_records route ON route.work_item_id = work.id
               JOIN protected_objects profile
                 ON profile.work_item_id = work.id AND profile.kind = 'launch_profile'
              WHERE binding.repository_provider = ?1 AND binding.repository_id = ?2
                AND binding.workstream_handle = ?3 AND work.pr = ?4
                AND binding.exact_head = ?5 AND binding.work_item_id != ?6
              ORDER BY binding.work_item_id LIMIT 2",
        )?;
        let identity_candidates = identity_statement
            .query_map(
                params![
                    request.repository_provider,
                    request.repository_id,
                    request.workstream_handle,
                    request.pull_request,
                    request.head_sha,
                    current.work_id,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        if identity_candidates.len() > 1 {
            return Err(WorkLedgerError::Refused(
                "immutable repository identity resolves to multiple legacy publications".to_owned(),
            ));
        }
        let legacy = if requested_legacy_exists {
            Some(requested_legacy)
        } else if let Some((work_id, source_ref, publication_digest, route_ref, profile_ref)) =
            identity_candidates.first()
        {
            let expected_profile_ref =
                OpaqueRef::derive("launch-profile", request.profile_digest.as_bytes())
                    .as_str()
                    .to_owned();
            if profile_ref != &expected_profile_ref {
                return Err(WorkLedgerError::Refused(
                    "immutable repository identity resolves to different launch authority"
                        .to_owned(),
                ));
            }
            let wake_id = opaque_ref(
                "wake",
                &format!(
                    "{}\n{}\n{}\n{}\n{}",
                    work_id, 6, request.owner_generation, route_ref, request.profile_digest,
                ),
            );
            Some(PublicationIdentities {
                work_id: work_id.clone(),
                source_ref: source_ref.clone(),
                route_ref: route_ref.clone(),
                profile_ref: profile_ref.clone(),
                wake_id,
                publication_digest: publication_digest.clone(),
            })
        } else {
            None
        };
        if let Some(legacy) = &legacy {
            let legacy_binding: Option<LegacyProjectionBinding> = connection
                .query_row(
                    "SELECT workstream_handle, plan_sha256, root_revision, issue_revision,
                            projection_revision, material_event_revision, repository_provider,
                            repository_id, repository, exact_head
                       FROM workstream_projection_bindings WHERE work_item_id = ?1",
                    [&legacy.work_id],
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
                            row.get(8)?,
                            row.get(9)?,
                        ))
                    },
                )
                .optional()?;
            let Some(binding) = legacy_binding else {
                return Err(WorkLedgerError::Refused(
                    "legacy native publication lacks its projection identity".to_owned(),
                ));
            };
            let repository_identity_matches = match (&binding.6, &binding.7) {
                (None, None) => {
                    binding.8
                        == request
                            .legacy_repository_alias
                            .as_deref()
                            .unwrap_or(&request.repository)
                }
                (Some(provider), Some(identity)) => {
                    provider == &request.repository_provider && identity == &request.repository_id
                }
                _ => false,
            };
            if binding.0 != request.workstream_handle
                || binding.1 != request.plan_sha256
                || binding.2 != request.root_revision
                || binding.3 != request.issue_revision
                || binding.4 != request.projection_revision
                || binding.5 != request.material_event_revision
                || !repository_identity_matches
                || binding.9 != request.head_sha
            {
                return Err(WorkLedgerError::Refused(
                    "legacy native publication projection identity disagrees".to_owned(),
                ));
            }
        }
        let unproven_legacy_coordinate: bool = connection.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM workstream_projection_bindings binding
               JOIN work_items work ON work.id = binding.work_item_id
                WHERE binding.repository_provider IS NULL AND binding.repository_id IS NULL
                  AND binding.workstream_handle = ?1 AND work.pr = ?2
                  AND binding.exact_head = ?3 AND binding.work_item_id != ?4
             )",
            params![
                request.workstream_handle,
                request.pull_request,
                request.head_sha,
                legacy
                    .as_ref()
                    .map_or(requested_legacy_work_id.as_str(), |value| {
                        value.work_id.as_str()
                    }),
            ],
            |row| row.get(0),
        )?;
        if unproven_legacy_coordinate {
            return Err(WorkLedgerError::Refused(
                "legacy native publication coordinate equivalence is unproven".to_owned(),
            ));
        }
        match (current_exists, legacy) {
            (true, Some(_)) => Err(WorkLedgerError::Refused(
                "native publication has overlapping legacy and immutable repository identities"
                    .to_owned(),
            )),
            (true | false, None) => Ok((current, false)),
            (false, Some(legacy)) => Ok((legacy, true)),
        }
    }

    pub(crate) fn protected_launch_profile_bytes(
        &self,
        work_id: &str,
        profile_digest: &str,
    ) -> WorkLedgerResult<Vec<u8>> {
        let connection = self.connect_read_only()?;
        let expected_profile_ref = OpaqueRef::derive("launch-profile", profile_digest.as_bytes())
            .as_str()
            .to_owned();
        let object_ref: String = connection
            .query_row(
                "SELECT object_ref FROM protected_objects
                 WHERE work_item_id = ?1 AND kind = 'launch_profile'
                   AND content_digest = ?2 AND profile_ref = ?3",
                params![work_id, profile_digest, expected_profile_ref],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                WorkLedgerError::Refused("wake has no exact protected launch profile".to_owned())
            })?;
        let (record, bytes) = self.open_protected_object(&object_ref)?;
        if record.work_item_id != work_id
            || record.kind != "launch_profile"
            || record.content_digest != profile_digest
            || record.profile_ref.as_deref() != Some(expected_profile_ref.as_str())
        {
            return Err(WorkLedgerError::Refused(
                "protected launch profile authority changed".to_owned(),
            ));
        }
        Ok(bytes)
    }

    fn ensure_native_work_item(
        &self,
        request: &NativePublicationRequest,
        identities: &PublicationIdentities,
    ) -> WorkLedgerResult<()> {
        let candidate = ImportCandidate {
            work_id: identities.work_id.clone(),
            kind: "terminal_handoff".to_owned(),
            repo: Some(request.repository.clone()),
            pr: Some(request.pull_request),
            head_sha: Some(request.head_sha.clone()),
            base_ref: Some(request.base_ref.clone()),
            goal_id: Some(opaque_ref("goal", &request.workstream_handle)),
            goal_generation: 1,
            lane: Some("fresh_agent_continuation".to_owned()),
            role: "root".to_owned(),
            owner_id: Some(opaque_ref("owner", &request.owner_id)),
            owner_generation: request.owner_generation,
            terminal_adapter: Some("session_host".to_owned()),
            agent_adapter: Some(request.agent_provider.clone()),
            provider_adapter: Some(request.profile_provider.clone()),
            coordinator_route_ref: None,
            repair_route_ref: Some(identities.route_ref.clone()),
            pr_truth: "unknown".to_owned(),
            acceptance_truth: "unknown".to_owned(),
            continuation_truth: "pending".to_owned(),
            phase: LifecycleState::ShadowImported.as_str().to_owned(),
            source_ref: identities.source_ref.clone(),
            content_digest: identities.publication_digest.clone(),
            source_updated_at: None,
        };
        self.import_candidates(&[candidate])?;
        Ok(())
    }

    fn ensure_projection_binding(
        &self,
        request: &NativePublicationRequest,
        work_id: &str,
    ) -> WorkLedgerResult<()> {
        self.bind_workstream_projection(
            work_id,
            &request.workstream_handle,
            &request.plan_sha256,
            request.root_revision,
            request.issue_revision,
            request.projection_revision,
            request.material_event_revision,
            &request.repository_provider,
            &request.repository_id,
            &request.repository,
            &request.head_sha,
        )
    }

    fn ensure_continuations(
        &self,
        request: &NativePublicationRequest,
        work_id: &str,
    ) -> WorkLedgerResult<()> {
        let connection = self.connect_read_only()?;
        let existing: Option<(String, String, u64)> = connection
            .query_row(
                "SELECT success_contract_digest, failure_contract_digest, revision
                 FROM continuation_contracts WHERE work_item_id = ?1",
                [work_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match existing {
            None => {
                self.record_continuations(
                    work_id,
                    0,
                    &ContinuationSet::new(
                        request.success_continuation_digest.clone(),
                        None,
                        request.failure_continuation_digest.clone(),
                        None,
                    )?,
                )?;
            }
            Some((success, failure, revision))
                if success == request.success_continuation_digest
                    && failure == request.failure_continuation_digest
                    && revision == 1 => {}
            Some(_) => {
                return Err(WorkLedgerError::Refused(
                    "native publication continuation authority differs".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn advance_to_managed(&self, work_id: &str, owner_generation: u64) -> WorkLedgerResult<()> {
        loop {
            let (phase, generation) = self.native_phase(work_id)?;
            let next = match phase.as_str() {
                "shadow_imported" => LifecycleState::Published,
                "published" => LifecycleState::Ready,
                "ready" => LifecycleState::Managed,
                "managed" | "waiting" | "actionable" | "dispatching" | "agent_owned_repair"
                | "returned" | "terminal" => {
                    return Ok(());
                }
                _ => {
                    return Err(WorkLedgerError::Refused(
                        "native publication found an incompatible lifecycle".to_owned(),
                    ));
                }
            };
            self.transition_with_wake(work_id, generation, owner_generation, next, None)?;
        }
    }

    fn native_phase(&self, work_id: &str) -> WorkLedgerResult<(String, u64)> {
        self.connect_read_only()?
            .query_row(
                "SELECT phase, work_generation FROM work_items WHERE id = ?1",
                [work_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(Into::into)
    }

    #[allow(clippy::too_many_lines, clippy::type_complexity)]
    fn ensure_adapter(&self, adapter: &AdapterBindingRecord) -> WorkLedgerResult<()> {
        let connection = self.connect_read_only()?;
        let existing: Option<(String, String, u64, u64, String, String, String, String)> =
            connection
                .query_row(
                    "SELECT axis, name, generation, revision, implementation_digest,
                            configuration_digest, capabilities_digest, state
                     FROM adapter_registry WHERE registry_ref = ?1",
                    [adapter.registry_ref.as_str()],
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
        let exact = (
            adapter.axis.as_str().to_owned(),
            adapter.name.clone(),
            adapter.generation,
            adapter.revision,
            adapter.implementation_sha256.as_str().to_owned(),
            adapter.configuration_sha256.as_str().to_owned(),
            adapter.capabilities_sha256.as_str().to_owned(),
            "active".to_owned(),
        );
        match existing {
            None => self.register_adapter(adapter),
            Some(stored) if stored == exact => Ok(()),
            Some(_) => Err(WorkLedgerError::Refused(
                "native publication adapter identity collides".to_owned(),
            )),
        }
    }

    fn ensure_route(&self, route: &RouteRegistration) -> WorkLedgerResult<()> {
        let connection = self.connect_read_only()?;
        let existing: Option<String> = connection
            .query_row(
                "SELECT integrity_hash FROM route_records WHERE route_ref = ?1",
                [&route.route_ref],
                |row| row.get(0),
            )
            .optional()?;
        match existing {
            None => self.register_staged_route(route),
            Some(integrity) if integrity == route.envelope_integrity => Ok(()),
            Some(_) => Err(WorkLedgerError::Refused(
                "native publication route identity collides".to_owned(),
            )),
        }
    }

    #[allow(clippy::too_many_lines, clippy::type_complexity)]
    fn publication_is_exact(
        &self,
        request: &NativePublicationRequest,
        identities: &PublicationIdentities,
    ) -> WorkLedgerResult<bool> {
        let Some(connection) = self
            .path
            .exists()
            .then(|| self.connect_read_only())
            .transpose()?
        else {
            return Ok(false);
        };
        let work: Option<(String, String, Option<u64>, Option<String>, u64, String)> = connection
            .query_row(
                "SELECT phase, source_digest, pr, head_sha, owner_generation, repo
                 FROM work_items WHERE id = ?1",
                [&identities.work_id],
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
        let Some((phase, source_digest, pr, head, owner_generation, repo)) = work else {
            return Ok(false);
        };
        if source_digest != identities.publication_digest
            || pr != Some(request.pull_request)
            || head.as_deref() != Some(request.head_sha.as_str())
            || owner_generation != request.owner_generation
        {
            return Err(WorkLedgerError::Refused(
                "native publication work identity collides".to_owned(),
            ));
        }
        let binding: Option<(Option<String>, Option<String>, String, String)> = connection
            .query_row(
                "SELECT repository_provider, repository_id, repository, exact_head
                   FROM workstream_projection_bindings WHERE work_item_id = ?1",
                [&identities.work_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((repository_provider, repository_id, binding_repository, binding_head)) = binding
        else {
            return Ok(false);
        };
        let repository_identity_matches =
            match (repository_provider.as_deref(), repository_id.as_deref()) {
                (None, None) => true,
                (Some(provider), Some(identity)) => {
                    provider == request.repository_provider && identity == request.repository_id
                }
                _ => false,
            };
        if !repository_identity_matches
            || binding_repository != repo
            || binding_head != request.head_sha
        {
            return Err(WorkLedgerError::Refused(
                "native publication repository identity collides".to_owned(),
            ));
        }
        if !matches!(
            phase.as_str(),
            "managed"
                | "waiting"
                | "actionable"
                | "dispatching"
                | "agent_owned_repair"
                | "returned"
                | "terminal"
        ) {
            return Ok(false);
        }
        let exact_continuations: bool = connection.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM continuation_contracts
                WHERE work_item_id = ?1 AND success_contract_digest = ?2
                  AND failure_contract_digest = ?3 AND revision = 1
             )",
            params![
                identities.work_id,
                request.success_continuation_digest,
                request.failure_continuation_digest,
            ],
            |row| row.get(0),
        )?;
        if !exact_continuations {
            return Ok(false);
        }
        let exact_route: Option<bool> = connection
            .query_row(
                "SELECT work_item_id = ?2 AND head_sha = ?3 AND owner_generation = ?4
                 FROM route_records WHERE route_ref = ?1",
                params![
                    identities.route_ref,
                    identities.work_id,
                    request.head_sha,
                    request.owner_generation,
                ],
                |row| row.get(0),
            )
            .optional()?;
        if exact_route != Some(true) {
            return if exact_route.is_none() {
                Ok(false)
            } else {
                Err(WorkLedgerError::Refused(
                    "native publication route identity collides".to_owned(),
                ))
            };
        }
        let exact_profile: bool = connection.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM protected_objects
                WHERE work_item_id = ?1 AND kind = 'launch_profile'
                  AND profile_ref = ?2 AND content_digest = ?3
             )",
            params![
                identities.work_id,
                identities.profile_ref,
                request.profile_digest
            ],
            |row| row.get(0),
        )?;
        Ok(exact_profile)
    }

    #[allow(clippy::too_many_lines)]
    fn reconcile_projection_repository_coordinate(
        &self,
        request: &NativePublicationRequest,
        work_item_id: &str,
    ) -> WorkLedgerResult<bool> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let mut connection = self.connect_read_write()?;
        super::configure_durable(&connection)?;
        super::verify_supported_schema(&connection)?;
        let transaction =
            connection.transaction_with_behavior(super::TransactionBehavior::Immediate)?;
        let stored: RepositoryCoordinateBinding = transaction
            .query_row(
                "SELECT binding.repository_provider, binding.repository_id,
                            binding.repository, binding.workstream_handle,
                            binding.root_revision, binding.issue_revision,
                            binding.projection_revision, binding.material_event_revision,
                            binding.exact_head, work.repo
                       FROM workstream_projection_bindings binding
                       JOIN work_items work ON work.id = binding.work_item_id
                      WHERE binding.work_item_id = ?1",
                [work_item_id],
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
                        row.get(8)?,
                        row.get(9)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => WorkLedgerError::Refused(
                    "repository coordinate reconciliation lacks an exact binding".to_owned(),
                ),
                other => WorkLedgerError::Sql(other),
            })?;
        if stored.0 != request.repository_provider
            || stored.1 != request.repository_id
            || stored.3 != request.workstream_handle
            || stored.4 != request.root_revision
            || stored.5 != request.issue_revision
            || stored.6 != request.projection_revision
            || stored.7 != request.material_event_revision
            || stored.8 != request.head_sha
            || stored.9 != stored.2
        {
            return Err(WorkLedgerError::Refused(
                "repository coordinate reconciliation fence disagrees".to_owned(),
            ));
        }
        if stored.2 == request.repository {
            transaction.commit()?;
            return Ok(false);
        }
        let binding_changed = transaction.execute(
            "UPDATE workstream_projection_bindings SET repository = ?2
              WHERE work_item_id = ?1 AND repository_provider = ?3 AND repository_id = ?4
                AND repository = ?5 AND exact_head = ?6",
            params![
                work_item_id,
                request.repository,
                request.repository_provider,
                request.repository_id,
                stored.2,
                request.head_sha,
            ],
        )?;
        let work_changed = transaction.execute(
            "UPDATE work_items SET repo = ?2
              WHERE id = ?1 AND repo = ?3 AND pr = ?4 AND head_sha = ?5",
            params![
                work_item_id,
                request.repository,
                stored.9,
                request.pull_request,
                request.head_sha,
            ],
        )?;
        if binding_changed != 1 || work_changed != 1 {
            return Err(WorkLedgerError::Refused(
                "repository coordinate reconciliation lost its compare-and-swap".to_owned(),
            ));
        }
        transaction.commit()?;
        Ok(true)
    }

    fn verify_exact_shadow_target(
        &self,
        request: &NativePublicationRequest,
    ) -> WorkLedgerResult<()> {
        self.verify_repo_policy_revision(&request.repository, request.repo_policy_revision)?;
        let matches: u64 = self.connect_read_only()?.query_row(
            "SELECT COUNT(*)
               FROM workstream_projection_bindings binding
               JOIN work_items work ON work.id = binding.work_item_id
              WHERE binding.repository_provider = ?1 AND binding.repository_id = ?2
                AND binding.repository = ?3 AND work.pr = ?4 AND binding.exact_head = ?5",
            params![
                request.repository_provider,
                request.repository_id,
                request.repository,
                request.pull_request,
                request.head_sha,
            ],
            |row| row.get(0),
        )?;
        if matches != 1 {
            return Err(WorkLedgerError::Refused(
                "native publication is not visible as one exact shadow target".to_owned(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn verify_native_policy_binding(
    state_dir: &Path,
    repository: &str,
    pull_request: u64,
    head_sha: &str,
) -> WorkLedgerResult<()> {
    let ledger = WorkLedger::open_existing(state_dir)?
        .ok_or_else(|| WorkLedgerError::Refused("native work ledger is unavailable".to_owned()))?;
    let connection = ledger.connect_read_only()?;
    let mut statement = connection.prepare(
        "SELECT binding.repository_provider, binding.repository_id
           FROM workstream_projection_bindings binding
           JOIN work_items work ON work.id = binding.work_item_id
          WHERE binding.repository = ?1 AND work.pr = ?2 AND binding.exact_head = ?3
          ORDER BY binding.repository_provider, binding.repository_id LIMIT 2",
    )?;
    let candidates = statement
        .query_map(params![repository, pull_request, head_sha], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let [(repository_provider, repository_id)] = candidates.as_slice() else {
        return Err(WorkLedgerError::Refused(
            "native policy binding repository identity is absent or ambiguous".to_owned(),
        ));
    };
    let binding = read_native_policy_binding(
        state_dir,
        repository_provider.as_deref(),
        repository_id.as_deref(),
        repository,
        pull_request,
        head_sha,
    )?
    .ok_or_else(|| WorkLedgerError::Refused("native policy binding is unavailable".to_owned()))?;
    let version_identity_valid = match binding.schema_version {
        1 => binding.repository_provider.is_none() && binding.repository_id.is_none(),
        2 => {
            repository_provider.is_some()
                && repository_id.is_some()
                && binding.repository_provider.as_ref() == repository_provider.as_ref()
                && binding.repository_id.as_ref() == repository_id.as_ref()
        }
        _ => false,
    };
    if !version_identity_valid
        || binding.repository != repository
        || binding.pull_request != pull_request
        || binding.head_sha != head_sha
        || binding.repo_policy_revision == 0
    {
        return Err(WorkLedgerError::Refused(
            "native policy binding is invalid".to_owned(),
        ));
    }
    ledger.verify_repo_policy_revision(repository, binding.repo_policy_revision)?;
    Ok(())
}

#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn verify_native_policy_binding_for_repository(
    state_dir: &Path,
    repository_provider: &str,
    repository_id: &str,
    repository: &str,
    pull_request: u64,
    head_sha: &str,
) -> WorkLedgerResult<()> {
    super::validate_token("repository provider", repository_provider)?;
    super::validate_token("repository identity", repository_id)?;
    let ledger = WorkLedger::open_existing(state_dir)?
        .ok_or_else(|| WorkLedgerError::Refused("native work ledger is unavailable".to_owned()))?;
    let connection = ledger.connect_read_only()?;
    let mut statement = connection.prepare(
        "SELECT binding.work_item_id
           FROM workstream_projection_bindings binding
           JOIN work_items work ON work.id = binding.work_item_id
          WHERE binding.repository_provider = ?1 AND binding.repository_id = ?2
            AND binding.repository = ?3 AND work.pr = ?4 AND binding.exact_head = ?5
          ORDER BY binding.work_item_id LIMIT 2",
    )?;
    let work_ids = statement
        .query_map(
            params![
                repository_provider,
                repository_id,
                repository,
                pull_request,
                head_sha,
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let [work_id] = work_ids.as_slice() else {
        return Err(WorkLedgerError::Refused(
            "native policy binding immutable repository identity is absent or ambiguous".to_owned(),
        ));
    };
    let binding = read_native_policy_binding(
        state_dir,
        Some(repository_provider),
        Some(repository_id),
        repository,
        pull_request,
        head_sha,
    )?
    .ok_or_else(|| WorkLedgerError::Refused("native policy binding is unavailable".to_owned()))?;
    if binding.schema_version != 2
        || binding.repository_provider.as_deref() != Some(repository_provider)
        || binding.repository_id.as_deref() != Some(repository_id)
        || binding.repository != repository
        || binding.pull_request != pull_request
        || binding.head_sha != head_sha
        || binding.work_id != *work_id
        || binding.repo_policy_revision == 0
    {
        return Err(WorkLedgerError::Refused(
            "native policy binding is invalid".to_owned(),
        ));
    }
    ledger
        .verify_repo_policy_revision(repository, binding.repo_policy_revision)
        .map(|_| ())
}

#[cfg(unix)]
#[allow(dead_code)] // Explicit manual compatibility path; active stewardship requires v2 identity.
pub(crate) fn bind_legacy_native_policy(
    state_dir: &Path,
    repository: &str,
    pull_request: u64,
    head_sha: &str,
    work_id: &str,
) -> WorkLedgerResult<()> {
    let ledger = WorkLedger::open_existing(state_dir)?
        .ok_or_else(|| WorkLedgerError::Refused("native work ledger is unavailable".to_owned()))?;
    let policy = ledger.repo_policy(repository)?.ok_or_else(|| {
        WorkLedgerError::Refused("explicit repository policy is unavailable".to_owned())
    })?;
    let connection = ledger.connect_read_only()?;
    let exact_work: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_items WHERE id = ?1
          AND kind = 'terminal_handoff' AND lower(repo) = lower(?2)
          AND pr = ?3 AND lower(head_sha) = lower(?4)
          AND phase IN ('managed', 'waiting', 'actionable', 'dispatching',
                        'agent_owned_repair', 'returned'))",
        params![work_id, repository, pull_request, head_sha],
        |row| row.get(0),
    )?;
    let repository_identity: (Option<String>, Option<String>) = connection
        .query_row(
            "SELECT repository_provider, repository_id
               FROM workstream_projection_bindings WHERE work_item_id = ?1
                 AND repository = ?2 AND exact_head = ?3",
            params![work_id, repository, head_sha],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| {
            WorkLedgerError::Refused(
                "legacy publication lacks one exact projection binding".to_owned(),
            )
        })?;
    if !exact_work {
        return Err(WorkLedgerError::Refused(
            "legacy publication is not one exact managed shadow target".to_owned(),
        ));
    }
    let binding = NativePolicyBindingV1 {
        schema_version: u32::from(repository_identity.0.is_some()) + 1,
        repository_provider: repository_identity.0,
        repository_id: repository_identity.1,
        repository: repository.to_owned(),
        pull_request,
        head_sha: head_sha.to_owned(),
        repo_policy_revision: policy.revision,
        work_id: work_id.to_owned(),
    };
    persist_native_policy_binding_value(state_dir, repository, pull_request, head_sha, &binding)
}

fn native_policy_binding_path(
    state_dir: &Path,
    repository_provider: Option<&str>,
    repository_id: Option<&str>,
    repository: &str,
    pull_request: u64,
    head_sha: &str,
) -> PathBuf {
    let key = match (repository_provider, repository_id) {
        (Some(provider), Some(identity)) => digest(
            format!(
                "shipyard-native-policy-binding-v2\n{provider}\n{identity}\n{repository}\n{pull_request}\n{head_sha}"
            )
            .as_bytes(),
        ),
        _ => digest(format!("{repository}\n{pull_request}\n{head_sha}").as_bytes()),
    };
    state_dir
        .join("work-ledger")
        .join("native-policy-bindings")
        .join(format!("{key}.json"))
}

fn persist_native_policy_binding(
    state_dir: &Path,
    request: &NativePublicationRequest,
    report: &NativePublicationReport,
) -> WorkLedgerResult<()> {
    let binding = NativePolicyBindingV1 {
        schema_version: 2,
        repository_provider: Some(request.repository_provider.clone()),
        repository_id: Some(request.repository_id.clone()),
        repository: request.repository.clone(),
        pull_request: request.pull_request,
        head_sha: request.head_sha.clone(),
        repo_policy_revision: request.repo_policy_revision,
        work_id: report.work_id.clone(),
    };
    persist_native_policy_binding_value(
        state_dir,
        &request.repository,
        request.pull_request,
        &request.head_sha,
        &binding,
    )
}

fn persist_native_policy_binding_value(
    state_dir: &Path,
    repository: &str,
    pull_request: u64,
    head_sha: &str,
    binding: &NativePolicyBindingV1,
) -> WorkLedgerResult<()> {
    let path = native_policy_binding_path(
        state_dir,
        binding.repository_provider.as_deref(),
        binding.repository_id.as_deref(),
        repository,
        pull_request,
        head_sha,
    );
    let directory = path.parent().ok_or_else(|| {
        WorkLedgerError::Refused("native policy binding has no parent".to_owned())
    })?;
    crate::writer_domain_lease::ensure_protected_dir_all(directory)?;
    let _writer = crate::writer_domain_lease::acquire_for_protected_path(directory)?;
    if let Some(existing) = read_native_policy_binding(
        state_dir,
        binding.repository_provider.as_deref(),
        binding.repository_id.as_deref(),
        repository,
        pull_request,
        head_sha,
    )? {
        return if existing == *binding {
            Ok(())
        } else {
            Err(WorkLedgerError::Refused(
                "native policy binding changed".to_owned(),
            ))
        };
    }
    let bytes = serde_json::to_vec(binding).map_err(|_| {
        WorkLedgerError::Refused("native policy binding cannot be serialized".to_owned())
    })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".native-policy-")
        .suffix(".tmp")
        .tempfile_in(directory)?;
    temporary.as_file_mut().write_all(&bytes)?;
    temporary.as_file_mut().sync_all()?;
    crate::queue::replace_file_with_windows_retry(temporary.path(), &path)?;
    OpenOptions::new().read(true).open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn read_native_policy_binding(
    state_dir: &Path,
    repository_provider: Option<&str>,
    repository_id: Option<&str>,
    repository: &str,
    pull_request: u64,
    head_sha: &str,
) -> WorkLedgerResult<Option<NativePolicyBindingV1>> {
    let path = native_policy_binding_path(
        state_dir,
        repository_provider,
        repository_id,
        repository,
        pull_request,
        head_sha,
    );
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 16 * 1024
    {
        return Err(WorkLedgerError::Refused(
            "native policy binding metadata is unsafe".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 {
        return Err(WorkLedgerError::Refused(
            "native policy binding is oversized".to_owned(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| WorkLedgerError::Refused("native policy binding is malformed".to_owned()))
}

#[cfg(not(unix))]
fn read_native_policy_binding(
    _state_dir: &Path,
    _repository_provider: Option<&str>,
    _repository_id: Option<&str>,
    _repository: &str,
    _pull_request: u64,
    _head_sha: &str,
) -> WorkLedgerResult<Option<NativePolicyBindingV1>> {
    Err(WorkLedgerError::Refused(
        "native policy binding verification requires a Unix controller".to_owned(),
    ))
}

struct PublicationIdentities {
    work_id: String,
    source_ref: String,
    route_ref: String,
    profile_ref: String,
    wake_id: String,
    publication_digest: String,
}

impl PublicationIdentities {
    fn new(request: &NativePublicationRequest) -> Self {
        let work_seed = format!(
            "{}\n{}\n{}\n{}\n{}",
            request.repository_provider,
            request.repository_id,
            request.pull_request,
            request.head_sha,
            request.workstream_handle,
        );
        Self::from_work_seed(request, &work_seed, false)
    }

    fn legacy(request: &NativePublicationRequest) -> Self {
        let work_seed = format!(
            "{}\n{}\n{}\n{}",
            request.repository, request.pull_request, request.head_sha, request.workstream_handle,
        );
        Self::from_work_seed(request, &work_seed, true)
    }

    fn from_work_seed(request: &NativePublicationRequest, work_seed: &str, legacy: bool) -> Self {
        let authority_seed = format!(
            "{work_seed}\n{}\n{}",
            request.owner_generation, request.profile_digest,
        );
        let identity_revision = if legacy { "v1" } else { "v2" };
        let work_id = opaque_ref(
            "wi",
            &format!("shipyard-native-continuation-{identity_revision}\n{work_seed}"),
        );
        let source_ref = opaque_ref(
            "src",
            &format!("shipyard-native-publication-{identity_revision}\n{work_seed}"),
        );
        let route_ref = opaque_ref(
            "route",
            &format!("shipyard-native-route-{identity_revision}\n{authority_seed}"),
        );
        let profile_ref = OpaqueRef::derive("launch-profile", request.profile_digest.as_bytes())
            .as_str()
            .to_owned();
        let publication_digest = digest(
            format!(
                "shipyard-native-publication-authority-{}\n{authority_seed}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
                if legacy { "v3" } else { "v4" },
                request.context_url.as_deref().unwrap_or(""),
                request.plan_sha256,
                request.root_revision,
                request.issue_revision,
                request.projection_revision,
                request.material_event_revision,
                request.base_ref,
                request.base_sha,
                request.github_installation_id,
                request.origin_machine,
                request.owner_id,
                request.agent_provider,
                request.agent_session_id,
                request.route_account,
                request.route_model,
                request.route_wrapper,
                request.native_resume_digest,
                request.route_id,
                request.profile_generation,
                request.profile_revision,
                request.profile_provider,
                request.success_continuation_digest,
                request.failure_continuation_digest,
            )
            .as_bytes(),
        );
        let wake_id = opaque_ref(
            "wake",
            &format!(
                "{}\n{}\n{}\n{}\n{}",
                work_id, 6, request.owner_generation, route_ref, request.profile_digest,
            ),
        );
        Self {
            work_id,
            source_ref,
            route_ref,
            profile_ref,
            wake_id,
            publication_digest,
        }
    }
}

fn native_publication_report(
    request: &NativePublicationRequest,
    identities: &PublicationIdentities,
    applied: bool,
    replay: bool,
) -> NativePublicationReport {
    NativePublicationReport {
        applied,
        replay,
        work_id: identities.work_id.clone(),
        route_ref: identities.route_ref.clone(),
        wake_id: identities.wake_id.clone(),
        profile_digest: request.profile_digest.clone(),
        repo_policy_revision: request.repo_policy_revision,
    }
}

fn validate_request(
    request: &NativePublicationRequest,
    policy: &WorkstreamContinuationConfig,
) -> WorkLedgerResult<()> {
    super::validate_workstream_handle(&request.workstream_handle)?;
    super::validate_token("repository identity", &request.repository_id)?;
    let (terminal_provider, terminal_session) = match &request.terminal_authority {
        TerminalCapabilityRequest::Cmux {
            provider_kind,
            native_session_id,
            ..
        }
        | TerminalCapabilityRequest::HerdR {
            provider_kind,
            native_session_id,
            ..
        } => (provider_kind, native_session_id),
    };
    if !policy.allows_repository(&request.repository)
        || request.repository_provider != "github.com"
        || request.repository_id.is_empty()
        || request.repository_id.len() > 512
        || !request
            .repository_id
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'/' | b'\\'))
        || request
            .legacy_repository_alias
            .as_deref()
            .is_some_and(|alias| {
                !super::is_canonical_repo_slug(alias) || alias == request.repository
            })
        || request.origin_machine != policy.origin_machine
        || request.pull_request == 0
        || request.github_installation_id == 0
        || request.owner_generation == 0
        || request.profile_generation == 0
        || request.profile_revision == 0
        || request.profile_generation != request.owner_generation
        || (!matches!(request.agent_provider.as_str(), "codex" | "claude")
            && AgentName::parse(request.agent_provider.clone()).is_err())
        || request.profile_provider != policy.provider_wrapper.provider_id
        || request.profile_digest != digest(&request.protected_profile_bytes)
        || request.protected_profile_bytes.is_empty()
        || request.protected_profile_bytes.len() > 1_048_576
        || request.workstream_handle.len() > 128
        || request.projection_revision == 0
        || request.owner_id.is_empty()
        || request.owner_id.len() > 512
        || request.agent_session_id.is_empty()
        || request.agent_session_id.len() > 512
        || terminal_provider != &request.agent_provider
        || terminal_session != &request.agent_session_id
        || request.route_id.is_empty()
        || request.route_wrapper.is_empty()
        || std::path::Path::new(&request.route_wrapper)
            .file_name()
            .and_then(|value| value.to_str())
            != Some("subrouter")
        || request.route_id.len() > 512
        || request.head_sha.len() != 40
        || !request
            .head_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || request.base_sha.len() != 40
        || !request
            .base_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || request.base_ref.is_empty()
        || request.base_ref.len() > 255
        || request.context_url.as_deref().is_some_and(|url| {
            url.is_empty() || url.len() > 4096 || url.chars().any(char::is_control)
        })
    {
        return Err(WorkLedgerError::Refused(
            "native publication authority is incomplete or unauthorized".to_owned(),
        ));
    }
    validate_digest("native profile digest", &request.profile_digest)?;
    validate_digest("native projection plan digest", &request.plan_sha256)?;
    validate_digest("native resume digest", &request.native_resume_digest)?;
    validate_digest(
        "native route environment digest",
        &request.route_environment_digest,
    )?;
    validate_digest(
        "native success continuation digest",
        &request.success_continuation_digest,
    )?;
    validate_digest(
        "native failure continuation digest",
        &request.failure_continuation_digest,
    )?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn native_route(
    request: &NativePublicationRequest,
    policy: &WorkstreamContinuationConfig,
    identities: &PublicationIdentities,
) -> WorkLedgerResult<(RouteRegistration, Vec<AdapterBindingRecord>)> {
    let wrapper = &policy.provider_wrapper;
    let config_digest = digest(
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}",
            wrapper.executable_path.display(),
            wrapper.executable_sha256,
            wrapper.provider_id,
            wrapper.adapter_id,
            wrapper.deadline_seconds,
            wrapper.max_stdout_bytes,
            wrapper.max_stderr_bytes,
        )
        .as_bytes(),
    );
    let provider_wrapper_ref =
        OpaqueRef::derive("provider-wrapper", wrapper.executable_sha256.as_bytes());
    let terminal = adapter(
        AdapterAxis::Terminal,
        "session_host",
        digest(b"shipyard-native-session-host-v1"),
        digest(request.route_id.as_bytes()),
        digest(b"fresh-session-route"),
    )?;
    let agent = adapter(
        AdapterAxis::Agent,
        &request.agent_provider,
        digest(request.agent_provider.as_bytes()),
        digest(request.agent_session_id.as_bytes()),
        digest(b"fresh-agent-resume"),
    )?;
    let session = NativeSessionRoute {
        native_session_ref: OpaqueRef::derive(
            "native-session",
            request.agent_session_id.as_bytes(),
        ),
        native_resume_ref: OpaqueRef::derive(
            "native-resume",
            request.native_resume_digest.as_bytes(),
        ),
        account_ref: OpaqueRef::derive("account", request.route_account.as_bytes()),
        model_ref: OpaqueRef::derive("model", request.route_model.as_bytes()),
        wrapper_ref: provider_wrapper_ref.clone(),
        session_headers_ref: OpaqueRef::derive(
            "session-headers-and-routing-wrapper",
            request.route_environment_digest.as_bytes(),
        ),
        session_headers_sha256: Sha256Digest::parse(request.route_environment_digest.clone())
            .map_err(route_error)?,
    };
    let agent_route = match request.agent_provider.as_str() {
        "codex" => AgentRoute::Codex { session },
        "claude" => AgentRoute::Claude { session },
        provider => AgentRoute::Named {
            name: AgentName::parse(provider.to_owned()).map_err(route_error)?,
            session,
        },
    };
    let provenance = RouteProvenanceRecord::new(
        TerminalRouteRecord::new(TerminalRoute::Registered {
            adapter: terminal.clone(),
            route_ref: OpaqueRef::derive("terminal-route", request.route_id.as_bytes()),
        }),
        AgentRouteRecord::new(agent.clone(), agent_route).map_err(route_error)?,
        ProviderRouteRecord::new(ProviderRoute::Subrouter {
            server_ref: OpaqueRef::derive("subrouter-server", request.route_wrapper.as_bytes()),
            route_ref: OpaqueRef::derive(
                "subrouter-route",
                request.native_resume_digest.as_bytes(),
            ),
        }),
        LaunchProfileRecord::new(
            OpaqueRef::parse(identities.profile_ref.clone()).map_err(route_error)?,
            request.profile_generation,
            request.profile_revision,
            Sha256Digest::parse(wrapper.executable_sha256.clone()).map_err(route_error)?,
            provider_wrapper_ref,
            Sha256Digest::parse(config_digest).map_err(route_error)?,
            "subrouter".to_owned(),
        )
        .map_err(route_error)?
        .bind_execution_provider(wrapper.provider_id.clone())
        .map_err(route_error)?,
    )
    .map_err(route_error)?
    .bind_delivery_authority(NativeDeliveryAuthorityRecord {
        github_installation_id: request.github_installation_id,
        base_ref: request.base_ref.clone(),
        base_sha: request.base_sha.clone(),
        terminal: request.terminal_authority.clone(),
    })
    .map_err(route_error)?;
    let route = RouteRegistration::new(
        identities.route_ref.clone(),
        identities.work_id.clone(),
        request.head_sha.clone(),
        5,
        opaque_ref("owner", &request.owner_id),
        request.owner_generation,
        1,
        opaque_ref("machine", &request.origin_machine),
        provenance,
    )?;
    Ok((route, vec![terminal, agent]))
}

fn adapter(
    axis: AdapterAxis,
    name: &str,
    implementation: String,
    configuration: String,
    capabilities: String,
) -> WorkLedgerResult<AdapterBindingRecord> {
    AdapterBindingRecord::new(
        axis,
        name,
        OpaqueRef::derive("adapter", format!("{}\n{name}", axis.as_str()).as_bytes()),
        1,
        1,
        Sha256Digest::parse(implementation).map_err(route_error)?,
        Sha256Digest::parse(configuration).map_err(route_error)?,
        Sha256Digest::parse(capabilities).map_err(route_error)?,
    )
    .map_err(route_error)
}

fn route_error(error: impl std::fmt::Display) -> WorkLedgerError {
    WorkLedgerError::Refused(format!("native publication route is invalid: {error}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::work_ledger::{
        DeliveryAuthorization, ProviderAuthorizationOperation, ReconciliationAuthorization,
    };
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::work_ledger::{
        DeliveryFence, FreshAgentResumeExpectation, ProviderAdapter, ProviderCapability,
        ProviderLaunchRequest, ProviderOutcome, WakeEnvelope, WakeProfileResolver,
    };
    #[cfg(unix)]
    use crate::work_ledger::{NativeStewardDisposition, WakeConsumerPolicy, WakeDeliveryResult};
    use crate::workstream_continuation_config::ProviderWrapperConfig;

    #[derive(Clone)]
    struct TestProfile {
        request: NativePublicationRequest,
    }

    impl FreshAgentLaunchProfile for TestProfile {
        fn provider_id(&self) -> &'static str {
            "provider"
        }

        fn provider_launch_options(&self) -> crate::work_ledger::FreshAgentProviderLaunchOptions {
            crate::work_ledger::FreshAgentProviderLaunchOptions::default()
        }

        fn profile_digest(&self) -> WorkLedgerResult<String> {
            Ok(self.request.profile_digest.clone())
        }

        fn permits_fresh_agent(&self) -> bool {
            true
        }

        fn protected_profile_bytes(&self) -> WorkLedgerResult<Vec<u8>> {
            Ok(self.request.protected_profile_bytes.clone())
        }

        fn resume_expectation(&self) -> Option<FreshAgentResumeExpectation<'_>> {
            Some(FreshAgentResumeExpectation {
                workstream_handle: &self.request.workstream_handle,
                context_url: self.request.context_url.as_deref(),
                plan_sha256: "1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f",
                root_revision: 1,
                issue_revision: 1,
                projection_revision: 1,
                material_event_revision: 1,
                checkpoint_id: "checkpoint-test",
                checkpoint_generation: 1,
                checkpoint_digest: "2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f",
                repository: &self.request.repository,
                head_sha: &self.request.head_sha,
                expected_resume_context_digest: "3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f",
                success_continuation_digest: &self.request.success_continuation_digest,
                failure_continuation_digest: &self.request.failure_continuation_digest,
            })
        }
    }

    struct Resolver(TestProfile);

    impl WakeProfileResolver for Resolver {
        type Profile = TestProfile;

        fn resolve(&mut self, _wake: &WakeEnvelope) -> WorkLedgerResult<Self::Profile> {
            Ok(self.0.clone())
        }
    }

    struct Adapter;

    impl ProviderAdapter for Adapter {
        fn capability(&self, provider_id: &str) -> Option<ProviderCapability> {
            (provider_id == "provider").then(|| ProviderCapability {
                adapter_id: "provider-wrapper-v1".to_owned(),
                fresh_agent_launch: true,
                idempotent_launch: true,
            })
        }

        fn authorize(
            &mut self,
            fence: &DeliveryFence,
            _operation: ProviderAuthorizationOperation,
        ) -> Result<DeliveryAuthorization, ProviderOutcome> {
            Ok(DeliveryAuthorization::for_test(
                fence.work_generation,
                fence.owner_generation,
            ))
        }

        fn authorize_reconciliation(
            &mut self,
            fence: &DeliveryFence,
        ) -> Result<ReconciliationAuthorization, ProviderOutcome> {
            Ok(ReconciliationAuthorization::for_test(
                crate::work_ledger::reconciliation_fence_digest(fence),
            ))
        }

        fn launch(
            &mut self,
            _request: ProviderLaunchRequest<'_>,
            _authority: DeliveryAuthorization,
        ) -> ProviderOutcome {
            ProviderOutcome::Delivered {
                receipt: b"provider accepted agent".to_vec(),
            }
        }

        fn reconcile(
            &mut self,
            _fence: &DeliveryFence,
            _authority: DeliveryAuthorization,
        ) -> ProviderOutcome {
            ProviderOutcome::Delivered {
                receipt: b"provider reconciled agent".to_vec(),
            }
        }

        fn reconcile_read_only(
            &mut self,
            _fence: &DeliveryFence,
            _authority: ReconciliationAuthorization,
        ) -> ProviderOutcome {
            ProviderOutcome::Delivered {
                receipt: b"provider reconciled agent".to_vec(),
            }
        }
    }

    pub(crate) fn policy(repositories: Vec<String>) -> WorkstreamContinuationConfig {
        WorkstreamContinuationConfig {
            origin_machine: "m5".to_owned(),
            repositories,
            provider_wrapper: ProviderWrapperConfig {
                executable_path: PathBuf::from("/opt/shipyard/provider-wrapper"),
                executable_sha256: digest(b"wrapper"),
                provider_id: "provider".to_owned(),
                adapter_id: "provider-wrapper-v1".to_owned(),
                deadline_seconds: 30,
                max_stdout_bytes: 65_536,
                max_stderr_bytes: 65_536,
            },
            terminal_trust: Box::new(crate::workstream_continuation_config::TerminalTrustConfig {
                cmux_signing_team_id: "7WLXT3NR37".to_owned(),
            }),
        }
    }

    pub(crate) fn request() -> NativePublicationRequest {
        let protected_profile_bytes =
            b"shipyard-launch-profile-v1\0{\"schema_version\":1}".to_vec();
        NativePublicationRequest {
            repository_provider: "github.com".to_owned(),
            repository_id: "R_test_repository".to_owned(),
            legacy_repository_alias: None,
            repository: "owner/repo".to_owned(),
            pull_request: 43,
            head_sha: "a".repeat(40),
            base_ref: "main".into(),
            base_sha: "b".repeat(40),
            github_installation_id: 42,
            repo_policy_revision: 1,
            terminal_authority:
                crate::terminal_delivery_authority::TerminalCapabilityRequest::Cmux {
                    cli_path: "/test/cmux".into(),
                    socket_path: "/test/cmux.sock".into(),
                    surface_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into(),
                    workspace_id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into(),
                    native_session_id: "session-43".into(),
                    provider_kind: "codex".into(),
                    process: crate::terminal_delivery_authority::LocalProcessIncarnation {
                        boot_id: "boot".into(),
                        pid: 42,
                        start_identity: "start".into(),
                    },
                },
            workstream_handle: "GEN-43".to_owned(),
            plan_sha256: digest(b"GEN-43-plan"),
            root_revision: 1,
            issue_revision: 1,
            projection_revision: 1,
            material_event_revision: 1,
            context_url: Some("https://linear.example/GEN-43".to_owned()),
            origin_machine: "m5".to_owned(),
            owner_id: "agent-owner-43".to_owned(),
            owner_generation: 1,
            agent_provider: "codex".to_owned(),
            agent_session_id: "session-43".to_owned(),
            route_account: "account-a".into(),
            route_model: "model-a".into(),
            route_wrapper: "subrouter".into(),
            native_resume_digest: digest(b"subrouter resume session-43"),
            route_environment_digest: digest(b"subrouter route environment"),
            route_id: "route-43".to_owned(),
            profile_generation: 1,
            profile_revision: 1,
            profile_provider: "provider".to_owned(),
            profile_digest: digest(&protected_profile_bytes),
            protected_profile_bytes,
            success_continuation_digest: digest(b"success"),
            failure_continuation_digest: digest(b"failure"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_refuses_before_creating_native_storage() {
        let temp = TempDir::new().expect("temp");
        let state_dir = temp.path().join("state");
        let request = request();
        let error = WorkLedger::plan_or_apply_native_continuation(
            &state_dir,
            &request,
            &policy(vec![request.repository.clone()]),
            true,
        )
        .expect_err("Windows native publication must fail closed");
        assert!(error.to_string().contains("requires Unix"));
        assert!(!state_dir.exists());
    }

    #[cfg(unix)]
    fn seed_repo_policy(state_dir: &std::path::Path, repository: &str) {
        let ledger = WorkLedger::open(state_dir).expect("ledger");
        ledger
            .set_repo_policy(
                &crate::work_ledger::RepoPolicy {
                    repo: repository.to_owned(),
                    primary_platform: "macos".to_owned(),
                    compatibility_mode: "independent".to_owned(),
                    compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
                    blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                    declared_dependency_lanes: Vec::new(),
                    revision: 0,
                },
                0,
            )
            .expect("repo policy");
    }

    #[cfg(unix)]
    fn seed_unbound_legacy_v11(
        state_dir: &std::path::Path,
        request: &NativePublicationRequest,
    ) -> PublicationIdentities {
        seed_repo_policy(state_dir, &request.repository);
        let ledger = WorkLedger::open(state_dir).expect("ledger");
        let legacy = PublicationIdentities::legacy(request);
        ledger
            .ensure_native_work_item(request, &legacy)
            .expect("legacy work item");
        ledger
            .connect_read_write()
            .expect("legacy fixture")
            .execute(
                "INSERT INTO workstream_projection_bindings
                 (work_item_id, workstream_handle, plan_sha256, root_revision, issue_revision,
                  projection_revision, material_event_revision, repository_provider,
                  repository_id, repository, exact_head, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8, ?9,
                         '2026-08-31T00:00:00Z')",
                params![
                    legacy.work_id,
                    request.workstream_handle,
                    request.plan_sha256,
                    request.root_revision,
                    request.issue_revision,
                    request.projection_revision,
                    request.material_event_revision,
                    request.repository,
                    request.head_sha,
                ],
            )
            .expect("legacy NULL repository binding");
        legacy
    }

    #[cfg(unix)]
    #[test]
    fn dry_run_is_non_mutating_and_apply_replays_exactly() {
        let temp = TempDir::new().expect("temp");
        let request = request();
        seed_repo_policy(temp.path(), &request.repository);
        let policy = policy(vec![request.repository.clone()]);
        let planned =
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, false)
                .expect("plan");
        assert!(!planned.applied);
        assert!(!planned.replay);
        assert!(WorkLedger::path_at(temp.path()).exists());

        let applied =
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, true)
                .expect("apply");
        assert!(applied.applied);
        assert!(!applied.replay);
        assert_eq!(applied, planned_with_apply(planned.clone()));

        let replay =
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, true)
                .expect("replay");
        assert!(!replay.applied);
        assert!(replay.replay);
        assert_eq!(replay.work_id, planned.work_id);
        assert_eq!(replay.route_ref, planned.route_ref);
        assert_eq!(replay.wake_id, planned.wake_id);

        let ledger = WorkLedger::open_existing(temp.path())
            .expect("open")
            .expect("ledger");
        let state: (String, u64) = ledger
            .connect_read_only()
            .expect("connection")
            .query_row(
                "SELECT work.phase, (SELECT COUNT(*) FROM outbox)
                   FROM work_items work WHERE work.id = ?1",
                [&planned.work_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("native state");
        assert_eq!(state, ("managed".to_owned(), 0));
        ledger
            .apply_native_steward_disposition(
                &request.repository,
                request.pull_request,
                &request.head_sha,
                NativeStewardDisposition::Actionable,
            )
            .expect("exact actionable observation");
        assert!(
            !ledger
                .native_wake_consumer_owns(&planned.wake_id)
                .expect("pending")
        );

        let mut resolver = Resolver(TestProfile {
            request: request.clone(),
        });
        let delivered = ledger
            .consume_one_wake(
                WakeConsumerPolicy {
                    activation_enabled: true,
                    dispatch_enabled: true,
                    authorized_repositories: vec![request.repository.clone()],
                },
                &mut resolver,
                &mut Adapter,
            )
            .expect("daemon delivery");
        assert_eq!(delivered, WakeDeliveryResult::Delivered);
        assert!(
            ledger
                .native_wake_consumer_owns(&planned.wake_id)
                .expect("delivered")
        );
    }

    #[cfg(unix)]
    #[test]
    fn absent_or_drifted_repo_policy_refuses_native_publication() {
        let absent = TempDir::new().expect("temp");
        let request = request();
        let continuation = policy(vec![request.repository.clone()]);
        assert!(
            WorkLedger::plan_or_apply_native_continuation(
                absent.path(),
                &request,
                &continuation,
                true
            )
            .is_err()
        );
        assert!(!WorkLedger::path_at(absent.path()).exists());

        let drifted = TempDir::new().expect("temp");
        seed_repo_policy(drifted.path(), &request.repository);
        let ledger = WorkLedger::open_existing(drifted.path())
            .expect("open")
            .expect("ledger");
        WorkLedger::plan_or_apply_native_continuation(
            drifted.path(),
            &request,
            &continuation,
            true,
        )
        .expect("initial publication");
        let mut changed = ledger
            .repo_policy(&request.repository)
            .expect("policy")
            .expect("present");
        changed.compatibility_mode = "blocking".to_owned();
        changed.blocking_rule = "all".to_owned();
        ledger.set_repo_policy(&changed, 1).expect("revise");
        assert!(
            verify_native_policy_binding(
                drifted.path(),
                &request.repository,
                request.pull_request,
                &request.head_sha
            )
            .is_err()
        );
        assert!(
            WorkLedger::plan_or_apply_native_continuation(
                drifted.path(),
                &request,
                &continuation,
                true
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_named_agent_uses_open_registry_without_lifecycle_changes() {
        let temp = TempDir::new().expect("temp");
        let mut request = request();
        seed_repo_policy(temp.path(), &request.repository);
        request.agent_provider = "qwen".into();
        let TerminalCapabilityRequest::Cmux { provider_kind, .. } = &mut request.terminal_authority
        else {
            unreachable!()
        };
        *provider_kind = "qwen".into();
        let policy = policy(vec![request.repository.clone()]);
        WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, true)
            .expect("named provider publication");
    }

    #[cfg(unix)]
    fn planned_with_apply(mut report: NativePublicationReport) -> NativePublicationReport {
        report.applied = true;
        report
    }

    #[cfg(unix)]
    #[test]
    fn repository_and_machine_authorization_fail_before_storage_creation() {
        for (policy, request) in [
            (policy(vec!["owner/other".to_owned()]), request()),
            (
                policy(vec!["owner/repo".to_owned()]),
                NativePublicationRequest {
                    origin_machine: "m3".to_owned(),
                    ..request()
                },
            ),
            (
                policy(vec!["owner/repo".to_owned()]),
                NativePublicationRequest {
                    route_wrapper: "codex".to_owned(),
                    ..request()
                },
            ),
            (
                policy(vec!["owner/repo".to_owned()]),
                NativePublicationRequest {
                    workstream_handle: "GEN-43\nspoof".to_owned(),
                    ..request()
                },
            ),
        ] {
            let temp = TempDir::new().expect("temp");
            assert!(
                WorkLedger::plan_or_apply_native_continuation(
                    temp.path(),
                    &request,
                    &policy,
                    true,
                )
                .is_err()
            );
            assert!(!WorkLedger::path_at(temp.path()).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn cross_provider_terminal_authority_fails_before_storage_creation() {
        let temp = TempDir::new().expect("temp");
        let mut request = request();
        let TerminalCapabilityRequest::Cmux { provider_kind, .. } = &mut request.terminal_authority
        else {
            panic!("cmux fixture")
        };
        *provider_kind = "claude".to_owned();
        let policy = policy(vec![request.repository.clone()]);
        assert!(
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, true)
                .is_err()
        );
        assert!(!WorkLedger::path_at(temp.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn cross_session_terminal_authority_fails_before_storage_creation() {
        let temp = TempDir::new().expect("temp");
        let mut request = request();
        let TerminalCapabilityRequest::Cmux {
            native_session_id, ..
        } = &mut request.terminal_authority
        else {
            panic!("cmux fixture")
        };
        *native_session_id = "different-session".to_owned();
        let policy = policy(vec![request.repository.clone()]);
        assert!(
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, true)
                .is_err()
        );
        assert!(!WorkLedger::path_at(temp.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn resolver_returns_only_exact_protected_profile_bytes() {
        let temp = TempDir::new().expect("temp");
        let request = request();
        seed_repo_policy(temp.path(), &request.repository);
        let policy = policy(vec![request.repository.clone()]);
        let report =
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, true)
                .expect("apply");
        let ledger = WorkLedger::open_existing(temp.path())
            .expect("open")
            .expect("ledger");
        let expected_bytes = request.protected_profile_bytes.clone();
        let expected_digest = request.profile_digest.clone();
        let profile_template = request.clone();
        let mut resolver = ExactProtectedProfileResolver::new(&ledger, move |bytes: &[u8]| {
            if bytes != expected_bytes {
                return Err(WorkLedgerError::Refused(
                    "unexpected profile bytes".to_owned(),
                ));
            }
            let mut profile_request = profile_template.clone();
            profile_request.profile_digest.clone_from(&expected_digest);
            Ok(TestProfile {
                request: profile_request,
            })
        });
        let profile: TestProfile = resolver
            .resolve_exact(&report.work_id, &request.profile_digest)
            .expect("exact profile");
        assert_eq!(profile.request.profile_digest, request.profile_digest);
        assert!(
            resolver
                .resolve_exact::<TestProfile>(&report.work_id, &digest(b"other"))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn immutable_repository_identity_preserves_rename_and_isolates_slug_reuse() {
        let original = request();
        let original_ids = PublicationIdentities::new(&original);

        let renamed = NativePublicationRequest {
            repository: "owner/renamed-repo".to_owned(),
            ..original.clone()
        };
        let renamed_ids = PublicationIdentities::new(&renamed);
        assert_eq!(original_ids.work_id, renamed_ids.work_id);
        assert_eq!(original_ids.source_ref, renamed_ids.source_ref);
        assert_eq!(original_ids.route_ref, renamed_ids.route_ref);
        assert_eq!(
            original_ids.publication_digest,
            renamed_ids.publication_digest
        );

        let reused_slug = NativePublicationRequest {
            repository_id: "R_different_repository".to_owned(),
            ..original.clone()
        };
        let reused_ids = PublicationIdentities::new(&reused_slug);
        assert_ne!(original_ids.work_id, reused_ids.work_id);
        assert_ne!(original_ids.source_ref, reused_ids.source_ref);
        assert_ne!(original_ids.route_ref, reused_ids.route_ref);
        assert_ne!(
            original_ids.publication_digest,
            reused_ids.publication_digest
        );

        let legacy = PublicationIdentities::legacy(&original);
        assert_ne!(legacy.work_id, original_ids.work_id);
        assert_ne!(legacy.source_ref, original_ids.source_ref);
        assert_ne!(legacy.route_ref, original_ids.route_ref);
        assert_ne!(legacy.publication_digest, original_ids.publication_digest);
    }

    #[cfg(unix)]
    #[test]
    fn repository_rename_updates_coordinate_in_place_and_exact_replay_is_a_no_op() {
        let temp = TempDir::new().expect("temp");
        let original = request();
        seed_repo_policy(temp.path(), &original.repository);
        let original_policy = policy(vec![original.repository.clone()]);
        let first = WorkLedger::plan_or_apply_native_continuation(
            temp.path(),
            &original,
            &original_policy,
            true,
        )
        .expect("original coordinate");
        let renamed = NativePublicationRequest {
            repository: "owner/renamed-repo".to_owned(),
            ..original.clone()
        };
        seed_repo_policy(temp.path(), &renamed.repository);
        let renamed_policy = policy(vec![renamed.repository.clone()]);

        let moved = WorkLedger::plan_or_apply_native_continuation(
            temp.path(),
            &renamed,
            &renamed_policy,
            true,
        )
        .expect("renamed coordinate");
        let replay = WorkLedger::plan_or_apply_native_continuation(
            temp.path(),
            &renamed,
            &renamed_policy,
            true,
        )
        .expect("renamed replay");

        assert_eq!(moved.work_id, first.work_id);
        assert!(moved.applied);
        assert!(!moved.replay);
        assert!(!replay.applied);
        assert!(replay.replay);
        let inventory = super::super::local_work_inventory(temp.path()).expect("inventory");
        assert_eq!(inventory.items.len(), 1);
        assert_eq!(inventory.items[0].repository, renamed.repository);
        assert_eq!(
            inventory.items[0].repository_id.as_deref(),
            Some("R_test_repository")
        );
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn immutable_repository_identity_prevents_slug_reuse_dedup_collision() {
        let temp = TempDir::new().expect("temp");
        let original = request();
        seed_repo_policy(temp.path(), &original.repository);
        let policy = policy(vec![original.repository.clone()]);
        let first =
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &original, &policy, true)
                .expect("first repository incarnation");
        let reused_slug = NativePublicationRequest {
            repository_id: "R_different_repository".to_owned(),
            ..original.clone()
        };

        let second =
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &reused_slug, &policy, true)
                .expect("reused slug with distinct immutable repository");

        assert_ne!(first.work_id, second.work_id);
        assert_ne!(first.route_ref, second.route_ref);
        assert_ne!(first.wake_id, second.wake_id);
        let inventory = super::super::local_work_inventory(temp.path()).expect("inventory");
        assert!(inventory.complete);
        assert_eq!(inventory.items.len(), 2);
        assert_ne!(
            inventory.items[0].repository_id,
            inventory.items[1].repository_id
        );

        let ledger = WorkLedger::open_existing(temp.path())
            .expect("open")
            .expect("ledger");
        let targets = ledger.shadow_pr_targets().expect("shadow targets");
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.repository_id.as_deref())
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                Some("R_different_repository"),
                Some("R_test_repository"),
            ])
        );
        assert!(
            ledger
                .native_steward_base_ref(
                    &original.repository,
                    original.pull_request,
                    &original.head_sha,
                )
                .is_err()
        );
        assert!(
            ledger
                .apply_native_steward_disposition(
                    &original.repository,
                    original.pull_request,
                    &original.head_sha,
                    NativeStewardDisposition::Waiting,
                )
                .is_err()
        );
        let selected = ledger
            .apply_native_steward_disposition_for_repository(
                Some(&original.repository_provider),
                Some(&original.repository_id),
                &original.repository,
                original.pull_request,
                &original.head_sha,
                NativeStewardDisposition::Waiting,
            )
            .expect("identity-bound disposition");
        assert!(selected.matched);
        assert!(selected.changed);
        let connection = ledger.connect_read_only().expect("read phases");
        let mut statement = connection
            .prepare(
                "SELECT binding.repository_id, work.phase, COUNT(intent.work_item_id)
                   FROM work_items work
                   JOIN workstream_projection_bindings binding
                     ON binding.work_item_id = work.id
                   LEFT JOIN projection_intents intent ON intent.work_item_id = work.id
                  WHERE work.repo = ?1 AND work.pr = ?2 AND work.head_sha = ?3
                  GROUP BY binding.repository_id, work.phase
                  ORDER BY binding.repository_id",
            )
            .expect("phase query");
        let phases = statement
            .query_map(
                params![
                    original.repository,
                    original.pull_request,
                    original.head_sha
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                },
            )
            .expect("phase rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("phases");
        assert_eq!(
            phases,
            vec![
                ("R_different_repository".to_owned(), "managed".to_owned(), 1,),
                ("R_test_repository".to_owned(), "managed".to_owned(), 2),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn first_legacy_redirect_is_policy_first_atomic_and_recoverable() {
        let temp = TempDir::new().expect("temp");
        let original = request();
        let legacy = seed_unbound_legacy_v11(temp.path(), &original);
        let redirected = NativePublicationRequest {
            legacy_repository_alias: Some(original.repository.clone()),
            repository: "new-owner/renamed-repo".to_owned(),
            ..original.clone()
        };
        seed_repo_policy(temp.path(), &redirected.repository);
        let redirected_policy = policy(vec![redirected.repository.clone()]);

        let precursor = NativePolicyBindingV1 {
            schema_version: 2,
            repository_provider: Some(redirected.repository_provider.clone()),
            repository_id: Some(redirected.repository_id.clone()),
            repository: redirected.repository.clone(),
            pull_request: redirected.pull_request,
            head_sha: redirected.head_sha.clone(),
            repo_policy_revision: redirected.repo_policy_revision,
            work_id: legacy.work_id.clone(),
        };
        persist_native_policy_binding_value(
            temp.path(),
            &redirected.repository,
            redirected.pull_request,
            &redirected.head_sha,
            &precursor,
        )
        .expect("simulate crash after policy precursor");
        let before = super::super::local_work_inventory(temp.path()).expect("before recovery");
        assert_eq!(before.items.len(), 1);
        assert_eq!(before.items[0].repository, original.repository);
        assert_eq!(before.items[0].repository_id, None);

        let recovered = WorkLedger::plan_or_apply_native_continuation(
            temp.path(),
            &redirected,
            &redirected_policy,
            true,
        )
        .expect("recover redirect publication");
        assert_eq!(recovered.work_id, legacy.work_id);
        let after = super::super::local_work_inventory(temp.path()).expect("after recovery");
        assert_eq!(after.items.len(), 1);
        assert_eq!(after.items[0].repository, redirected.repository);
        assert_eq!(
            after.items[0].repository_id.as_deref(),
            Some(redirected.repository_id.as_str())
        );

        let conflict = TempDir::new().expect("conflict temp");
        let conflict_legacy = seed_unbound_legacy_v11(conflict.path(), &original);
        seed_repo_policy(conflict.path(), &redirected.repository);
        let conflicting = NativePolicyBindingV1 {
            work_id: "wi_conflicting_policy_authority".to_owned(),
            ..precursor
        };
        persist_native_policy_binding_value(
            conflict.path(),
            &redirected.repository,
            redirected.pull_request,
            &redirected.head_sha,
            &conflicting,
        )
        .expect("conflicting policy fixture");
        assert!(
            WorkLedger::plan_or_apply_native_continuation(
                conflict.path(),
                &redirected,
                &redirected_policy,
                true,
            )
            .is_err()
        );
        let refused = super::super::local_work_inventory(conflict.path()).expect("refused state");
        assert_eq!(refused.items.len(), 1);
        assert_eq!(refused.items[0].work_item_id, conflict_legacy.work_id);
        assert_eq!(refused.items[0].repository, original.repository);
        assert_eq!(refused.items[0].repository_id, None);
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn legacy_native_identity_is_enriched_in_place_and_replays_without_duplication() {
        let temp = TempDir::new().expect("temp");
        let request = request();
        let publication_policy = policy(vec![request.repository.clone()]);
        let ledger = WorkLedger::open(temp.path()).expect("ledger");
        ledger
            .set_repo_policy(
                &crate::work_ledger::RepoPolicy {
                    repo: request.repository.clone(),
                    primary_platform: "macos".to_owned(),
                    compatibility_mode: "independent".to_owned(),
                    compatibility_lanes: vec!["linux".to_owned()],
                    blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                    declared_dependency_lanes: Vec::new(),
                    revision: 0,
                },
                0,
            )
            .expect("repo policy");
        let legacy = PublicationIdentities::legacy(&request);
        ledger
            .ensure_native_work_item(&request, &legacy)
            .expect("legacy work item");
        let connection = ledger.connect_read_write().expect("legacy fixture");
        connection
            .execute(
                "INSERT INTO workstream_projection_bindings
                 (work_item_id, workstream_handle, plan_sha256, root_revision, issue_revision,
                  projection_revision, material_event_revision, repository_provider,
                  repository_id, repository, exact_head, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8, ?9,
                         '2026-08-31T00:00:00Z')",
                params![
                    legacy.work_id,
                    request.workstream_handle,
                    request.plan_sha256,
                    request.root_revision,
                    request.issue_revision,
                    request.projection_revision,
                    request.material_event_revision,
                    request.repository,
                    request.head_sha,
                ],
            )
            .expect("legacy NULL repository binding");
        drop(connection);

        let first = WorkLedger::plan_or_apply_native_continuation(
            temp.path(),
            &request,
            &publication_policy,
            true,
        )
        .expect("legacy continuation");
        let replay = WorkLedger::plan_or_apply_native_continuation(
            temp.path(),
            &request,
            &publication_policy,
            true,
        )
        .expect("legacy continuation replay");

        assert_eq!(first.work_id, legacy.work_id);
        assert_eq!(replay.work_id, legacy.work_id);
        assert!(replay.replay);
        let inventory = super::super::local_work_inventory(temp.path()).expect("inventory");
        assert!(inventory.complete);
        assert_eq!(inventory.items.len(), 1);
        assert_eq!(
            inventory.items[0].repository_id.as_deref(),
            Some(request.repository_id.as_str())
        );
        let renamed = NativePublicationRequest {
            repository: "owner/renamed-repo".to_owned(),
            ..request.clone()
        };
        seed_repo_policy(temp.path(), &renamed.repository);
        let renamed_policy = policy(vec![renamed.repository.clone()]);
        let moved = WorkLedger::plan_or_apply_native_continuation(
            temp.path(),
            &renamed,
            &renamed_policy,
            true,
        )
        .expect("enriched legacy rename");
        let moved_replay = WorkLedger::plan_or_apply_native_continuation(
            temp.path(),
            &renamed,
            &renamed_policy,
            true,
        )
        .expect("enriched legacy rename replay");
        assert_eq!(moved.work_id, legacy.work_id);
        assert!(moved.applied);
        assert!(!moved.replay);
        assert!(moved_replay.replay);
        let moved_inventory = super::super::local_work_inventory(temp.path()).expect("inventory");
        assert_eq!(moved_inventory.items.len(), 1);
        assert_eq!(moved_inventory.items[0].repository, renamed.repository);
        let changed_profile_bytes = b"changed legacy launch authority".to_vec();
        let changed_profile = NativePublicationRequest {
            profile_digest: digest(&changed_profile_bytes),
            protected_profile_bytes: changed_profile_bytes,
            ..renamed.clone()
        };
        assert!(
            WorkLedger::plan_or_apply_native_continuation(
                temp.path(),
                &changed_profile,
                &renamed_policy,
                true,
            )
            .is_err()
        );
        assert_eq!(
            super::super::local_work_inventory(temp.path())
                .expect("inventory after authority refusal")
                .items
                .len(),
            1
        );
        let conflicting_identity = NativePublicationRequest {
            repository_id: "R_conflicting_repository".to_owned(),
            ..renamed.clone()
        };
        let reused = WorkLedger::plan_or_apply_native_continuation(
            temp.path(),
            &conflicting_identity,
            &renamed_policy,
            true,
        )
        .expect("slug reuse is a separate immutable repository");
        assert_ne!(reused.work_id, legacy.work_id);
        assert_eq!(
            super::super::local_work_inventory(temp.path())
                .expect("inventory after reuse")
                .items
                .len(),
            2
        );
        let stale_revision = NativePublicationRequest {
            projection_revision: request.projection_revision + 1,
            ..renamed
        };
        assert!(
            WorkLedger::plan_or_apply_native_continuation(
                temp.path(),
                &stale_revision,
                &renamed_policy,
                true,
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_legacy_completion_does_not_permanently_mark_identity_complete() {
        let temp = TempDir::new().expect("temp");
        let request = request();
        let publication_policy = policy(vec![request.repository.clone()]);
        let ledger = WorkLedger::open(temp.path()).expect("ledger");
        ledger
            .set_repo_policy(
                &crate::work_ledger::RepoPolicy {
                    repo: request.repository.clone(),
                    primary_platform: "macos".to_owned(),
                    compatibility_mode: "independent".to_owned(),
                    compatibility_lanes: vec!["linux".to_owned()],
                    blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                    declared_dependency_lanes: Vec::new(),
                    revision: 0,
                },
                0,
            )
            .expect("repo policy");
        let legacy = PublicationIdentities::legacy(&request);
        ledger
            .ensure_native_work_item(&request, &legacy)
            .expect("legacy work item");
        ledger
            .connect_read_write()
            .expect("legacy fixture")
            .execute(
                "INSERT INTO workstream_projection_bindings
                 (work_item_id, workstream_handle, plan_sha256, root_revision, issue_revision,
                  projection_revision, material_event_revision, repository_provider,
                  repository_id, repository, exact_head, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, ?8, ?9,
                         '2026-08-31T00:00:00Z')",
                params![
                    legacy.work_id,
                    request.workstream_handle,
                    request.plan_sha256,
                    request.root_revision,
                    request.issue_revision,
                    request.projection_revision,
                    request.material_event_revision,
                    request.repository,
                    request.head_sha,
                ],
            )
            .expect("legacy NULL repository binding");
        let changed_profile_bytes = b"different protected profile".to_vec();
        let changed = NativePublicationRequest {
            profile_digest: digest(&changed_profile_bytes),
            protected_profile_bytes: changed_profile_bytes,
            ..request
        };

        assert!(
            WorkLedger::plan_or_apply_native_continuation(
                temp.path(),
                &changed,
                &publication_policy,
                true,
            )
            .is_err()
        );
        let inventory = super::super::local_work_inventory(temp.path()).expect("inventory");
        assert!(!inventory.complete);
        assert_eq!(inventory.items[0].repository_provider, None);
        assert_eq!(inventory.items[0].repository_id, None);

        let renamed = NativePublicationRequest {
            repository: "owner/renamed-repo".to_owned(),
            ..changed
        };
        seed_repo_policy(temp.path(), &renamed.repository);
        assert!(
            WorkLedger::plan_or_apply_native_continuation(
                temp.path(),
                &renamed,
                &policy(vec![renamed.repository.clone()]),
                true,
            )
            .is_err()
        );
        let after_refusal = super::super::local_work_inventory(temp.path()).expect("inventory");
        assert_eq!(after_refusal.items.len(), 1);
        assert_eq!(after_refusal.items[0].repository, "owner/repo");
        assert!(!after_refusal.complete);
    }

    #[cfg(unix)]
    #[test]
    fn changed_profile_cannot_duplicate_one_stable_work_identity() {
        let temp = TempDir::new().expect("temp");
        let request = request();
        seed_repo_policy(temp.path(), &request.repository);
        let policy = policy(vec![request.repository.clone()]);
        let first =
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, true)
                .expect("first");
        let changed_bytes = b"shipyard-launch-profile-v1\0{\"schema_version\":2}".to_vec();
        let changed = NativePublicationRequest {
            profile_digest: digest(&changed_bytes),
            protected_profile_bytes: changed_bytes,
            ..request
        };
        assert!(
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &changed, &policy, true,)
                .is_err()
        );
        let planned_changed = PublicationIdentities::new(&changed);
        assert_eq!(first.work_id, planned_changed.work_id);
    }

    #[cfg(unix)]
    #[test]
    fn apply_resumes_an_exact_partial_publication() {
        let temp = TempDir::new().expect("temp");
        let request = request();
        let policy = policy(vec![request.repository.clone()]);
        let identities = PublicationIdentities::new(&request);
        let ledger = WorkLedger::open(temp.path()).expect("ledger");
        ledger
            .set_repo_policy(
                &crate::work_ledger::RepoPolicy {
                    repo: request.repository.clone(),
                    primary_platform: "macos".to_owned(),
                    compatibility_mode: "independent".to_owned(),
                    compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
                    blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                    declared_dependency_lanes: Vec::new(),
                    revision: 0,
                },
                0,
            )
            .expect("repo policy");

        // Model a crash after the immutable work item and continuation pair
        // landed, but before lifecycle advancement, route binding, or wake.
        ledger
            .ensure_native_work_item(&request, &identities)
            .expect("work item");
        ledger
            .ensure_continuations(&request, &identities.work_id)
            .expect("continuations");

        let completed =
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, true)
                .expect("resume partial publication");
        assert!(completed.applied);
        assert!(!completed.replay);

        let replay =
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, true)
                .expect("exact replay");
        assert!(replay.replay);
        assert_eq!(completed.work_id, replay.work_id);
        assert_eq!(completed.wake_id, replay.wake_id);
    }
}
