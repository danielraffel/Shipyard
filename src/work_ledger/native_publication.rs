//! Idempotent native publication of one exact fresh-agent continuation wake.

use serde::Serialize;

use super::dispatch::{FreshAgentLaunchProfile, WakeEnvelope, WakeProfileResolver};
use super::registry::RouteRegistration;
use super::route::{
    AdapterAxis, AdapterBindingRecord, AgentRoute, AgentRouteRecord, LaunchProfileRecord,
    NativeSessionRoute, OpaqueRef, ProviderRoute, ProviderRouteRecord, RouteProvenanceRecord,
    Sha256Digest, TerminalRoute, TerminalRouteRecord,
};
use super::{
    ContinuationSet, ImportCandidate, LifecycleState, OptionalExtension, WorkLedger,
    WorkLedgerError, WorkLedgerResult, digest, opaque_ref, params, validate_digest,
};
use crate::workstream_continuation_config::WorkstreamContinuationConfig;

/// Complete normalized authority needed to publish one native continuation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativePublicationRequest {
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) head_sha: String,
    pub(crate) workstream_handle: String,
    pub(crate) context_url: Option<String>,
    pub(crate) origin_machine: String,
    pub(crate) owner_id: String,
    pub(crate) owner_generation: u64,
    pub(crate) agent_provider: String,
    pub(crate) agent_session_id: String,
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
}

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
    pub(crate) fn plan_or_apply_native_continuation(
        state_dir: &std::path::Path,
        request: &NativePublicationRequest,
        policy: &WorkstreamContinuationConfig,
        apply: bool,
    ) -> WorkLedgerResult<NativePublicationReport> {
        // Authorization must precede even SQLite creation. A refused machine,
        // repository, or malformed profile is not a native ledger event.
        validate_request(request, policy)?;
        let ledger = if apply {
            Self::open(state_dir)?
        } else {
            Self::open_existing(state_dir)?.unwrap_or_else(|| Self {
                path: Self::path_at(state_dir),
            })
        };
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
        let identities = PublicationIdentities::new(request);
        let replay = self.publication_is_exact(request, &identities)?;
        let report = NativePublicationReport {
            applied: apply && !replay,
            replay,
            work_id: identities.work_id.clone(),
            route_ref: identities.route_ref.clone(),
            wake_id: identities.wake_id.clone(),
            profile_digest: request.profile_digest.clone(),
        };
        if !apply || replay {
            return Ok(report);
        }

        self.ensure_native_work_item(request, &identities)?;
        self.ensure_continuations(request, &identities.work_id)?;
        self.advance_to_actionable(&identities.work_id, request.owner_generation)?;

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

        let (phase, generation) = self.native_phase(&identities.work_id)?;
        if phase == LifecycleState::Actionable.as_str() {
            let wake = super::WakeIntent::new(
                &identities.work_id,
                generation + 1,
                request.owner_generation,
                identities.route_ref.clone(),
                request.profile_digest.clone(),
            )?;
            if wake.wake_id != identities.wake_id {
                return Err(WorkLedgerError::Refused(
                    "native publication wake identity drifted".to_owned(),
                ));
            }
            self.transition_with_wake(
                &identities.work_id,
                generation,
                request.owner_generation,
                LifecycleState::Dispatching,
                Some(&wake),
            )?;
        }
        if !self.publication_is_exact(request, &identities)? {
            return Err(WorkLedgerError::Refused(
                "native publication was not exact after apply".to_owned(),
            ));
        }
        Ok(report)
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
            base_ref: None,
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

    fn advance_to_actionable(&self, work_id: &str, owner_generation: u64) -> WorkLedgerResult<()> {
        loop {
            let (phase, generation) = self.native_phase(work_id)?;
            let next = match phase.as_str() {
                "shadow_imported" => LifecycleState::Published,
                "published" => LifecycleState::Ready,
                "ready" => LifecycleState::Managed,
                "managed" => LifecycleState::Actionable,
                "actionable" | "dispatching" | "agent_owned_repair" | "returned" | "terminal" => {
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
            None => self.register_route(route),
            Some(integrity) if integrity == route.envelope_integrity => Ok(()),
            Some(_) => Err(WorkLedgerError::Refused(
                "native publication route identity collides".to_owned(),
            )),
        }
    }

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
            || repo != request.repository
        {
            return Err(WorkLedgerError::Refused(
                "native publication work identity collides".to_owned(),
            ));
        }
        if !matches!(
            phase.as_str(),
            "dispatching" | "agent_owned_repair" | "returned" | "terminal"
        ) {
            return Ok(false);
        }
        let exact_wake: Option<bool> = connection
            .query_row(
                "SELECT work_item_id = ?2 AND route_ref = ?3 AND payload_digest = ?4
                 FROM outbox WHERE wake_id = ?1",
                params![
                    identities.wake_id,
                    identities.work_id,
                    identities.route_ref,
                    request.profile_digest,
                ],
                |row| row.get(0),
            )
            .optional()?;
        if exact_wake == Some(true) {
            Ok(true)
        } else if exact_wake.is_none() {
            Ok(false)
        } else {
            Err(WorkLedgerError::Refused(
                "native publication wake identity collides".to_owned(),
            ))
        }
    }
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
            "{}\n{}\n{}\n{}",
            request.repository, request.pull_request, request.head_sha, request.workstream_handle,
        );
        let authority_seed = format!(
            "{work_seed}\n{}\n{}",
            request.owner_generation, request.profile_digest,
        );
        let work_id = opaque_ref(
            "wi",
            &format!("shipyard-native-continuation-v1\n{work_seed}"),
        );
        let source_ref = opaque_ref(
            "src",
            &format!("shipyard-native-publication-v1\n{work_seed}"),
        );
        let route_ref = opaque_ref(
            "route",
            &format!("shipyard-native-route-v1\n{authority_seed}"),
        );
        let profile_ref = OpaqueRef::derive("launch-profile", request.profile_digest.as_bytes())
            .as_str()
            .to_owned();
        let publication_digest = digest(
            format!(
                "shipyard-native-publication-authority-v1\n{authority_seed}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
                request.context_url.as_deref().unwrap_or(""),
                request.origin_machine,
                request.owner_id,
                request.agent_provider,
                request.agent_session_id,
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

fn validate_request(
    request: &NativePublicationRequest,
    policy: &WorkstreamContinuationConfig,
) -> WorkLedgerResult<()> {
    if !policy.allows_repository(&request.repository)
        || request.origin_machine != policy.origin_machine
        || request.pull_request == 0
        || request.owner_generation == 0
        || request.profile_generation == 0
        || request.profile_revision == 0
        || request.profile_generation != request.owner_generation
        || !matches!(request.agent_provider.as_str(), "codex" | "claude")
        || request.profile_provider != policy.provider_wrapper.provider_id
        || request.profile_digest != digest(&request.protected_profile_bytes)
        || request.protected_profile_bytes.is_empty()
        || request.protected_profile_bytes.len() > 1_048_576
        || request.workstream_handle.is_empty()
        || request.workstream_handle.len() > 128
        || request.owner_id.is_empty()
        || request.owner_id.len() > 512
        || request.agent_session_id.is_empty()
        || request.agent_session_id.len() > 512
        || request.route_id.is_empty()
        || request.route_id.len() > 512
        || request.head_sha.len() != 40
        || !request
            .head_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || request.context_url.as_deref().is_some_and(|url| {
            url.is_empty() || url.len() > 4096 || url.chars().any(char::is_control)
        })
    {
        return Err(WorkLedgerError::Refused(
            "native publication authority is incomplete or unauthorized".to_owned(),
        ));
    }
    validate_digest("native profile digest", &request.profile_digest)?;
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
    let wrapper_ref = OpaqueRef::derive("provider-wrapper", wrapper.executable_sha256.as_bytes());
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
    let provider = adapter(
        AdapterAxis::Provider,
        &wrapper.provider_id,
        wrapper.executable_sha256.clone(),
        config_digest.clone(),
        digest(format!("{}\nfresh_agent\nidempotent", wrapper.adapter_id).as_bytes()),
    )?;
    let session = NativeSessionRoute {
        native_session_ref: OpaqueRef::derive(
            "native-session",
            request.agent_session_id.as_bytes(),
        ),
        native_resume_ref: OpaqueRef::derive("native-resume", request.workstream_handle.as_bytes()),
        account_ref: OpaqueRef::derive("account", request.agent_provider.as_bytes()),
        model_ref: OpaqueRef::derive("model", b"fresh-agent"),
        wrapper_ref: wrapper_ref.clone(),
        session_headers_ref: OpaqueRef::derive("session-headers", request.route_id.as_bytes()),
        session_headers_sha256: Sha256Digest::of_bytes(request.route_id.as_bytes()),
    };
    let agent_route = match request.agent_provider.as_str() {
        "codex" => AgentRoute::Codex { session },
        "claude" => AgentRoute::Claude { session },
        _ => unreachable!("validated agent provider"),
    };
    let provenance = RouteProvenanceRecord::new(
        TerminalRouteRecord::new(TerminalRoute::Registered {
            adapter: terminal.clone(),
            route_ref: OpaqueRef::derive("terminal-route", request.route_id.as_bytes()),
        }),
        AgentRouteRecord::new(agent.clone(), agent_route).map_err(route_error)?,
        ProviderRouteRecord::new(ProviderRoute::Registered {
            adapter: provider.clone(),
            route_ref: OpaqueRef::derive("provider-route", wrapper.adapter_id.as_bytes()),
        }),
        LaunchProfileRecord::new(
            OpaqueRef::parse(identities.profile_ref.clone()).map_err(route_error)?,
            request.profile_generation,
            request.profile_revision,
            Sha256Digest::parse(wrapper.executable_sha256.clone()).map_err(route_error)?,
            wrapper_ref,
            Sha256Digest::parse(config_digest).map_err(route_error)?,
            wrapper.provider_id.clone(),
        )
        .map_err(route_error)?,
    )
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
    Ok((route, vec![terminal, agent, provider]))
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
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::workstream_continuation_config::ProviderWrapperConfig;

    #[derive(Clone)]
    struct TestProfile {
        digest: String,
    }

    impl FreshAgentLaunchProfile for TestProfile {
        fn provider_id(&self) -> &str {
            "provider"
        }

        fn launch_argv(&self) -> &[String] {
            &[]
        }

        fn profile_digest(&self) -> WorkLedgerResult<String> {
            Ok(self.digest.clone())
        }

        fn permits_fresh_agent(&self) -> bool {
            true
        }
    }

    fn policy(repositories: Vec<String>) -> WorkstreamContinuationConfig {
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
        }
    }

    fn request() -> NativePublicationRequest {
        let protected_profile_bytes =
            b"shipyard-launch-profile-v1\0{\"schema_version\":1}".to_vec();
        NativePublicationRequest {
            repository: "owner/repo".to_owned(),
            pull_request: 43,
            head_sha: "a".repeat(40),
            workstream_handle: "GEN-43".to_owned(),
            context_url: Some("https://linear.example/GEN-43".to_owned()),
            origin_machine: "m5".to_owned(),
            owner_id: "agent-owner-43".to_owned(),
            owner_generation: 1,
            agent_provider: "codex".to_owned(),
            agent_session_id: "session-43".to_owned(),
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

    #[test]
    fn dry_run_is_non_mutating_and_apply_replays_exactly() {
        let temp = TempDir::new().expect("temp");
        let request = request();
        let policy = policy(vec![request.repository.clone()]);
        let planned =
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, false)
                .expect("plan");
        assert!(!planned.applied);
        assert!(!planned.replay);
        assert!(!WorkLedger::path_at(temp.path()).exists());

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
        let state: (String, String) = ledger
            .connect_read_only()
            .expect("connection")
            .query_row(
                "SELECT work.phase, wake.state FROM work_items work
                 JOIN outbox wake ON wake.work_item_id = work.id
                 WHERE work.id = ?1",
                [&planned.work_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("native state");
        assert_eq!(state, ("dispatching".to_owned(), "pending".to_owned()));
    }

    fn planned_with_apply(mut report: NativePublicationReport) -> NativePublicationReport {
        report.applied = true;
        report
    }

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

    #[test]
    fn resolver_returns_only_exact_protected_profile_bytes() {
        let temp = TempDir::new().expect("temp");
        let request = request();
        let policy = policy(vec![request.repository.clone()]);
        let report =
            WorkLedger::plan_or_apply_native_continuation(temp.path(), &request, &policy, true)
                .expect("apply");
        let ledger = WorkLedger::open_existing(temp.path())
            .expect("open")
            .expect("ledger");
        let expected_bytes = request.protected_profile_bytes.clone();
        let expected_digest = request.profile_digest.clone();
        let mut resolver = ExactProtectedProfileResolver::new(&ledger, move |bytes: &[u8]| {
            if bytes != expected_bytes {
                return Err(WorkLedgerError::Refused(
                    "unexpected profile bytes".to_owned(),
                ));
            }
            Ok(TestProfile {
                digest: expected_digest.clone(),
            })
        });
        let profile: TestProfile = resolver
            .resolve_exact(&report.work_id, &request.profile_digest)
            .expect("exact profile");
        assert_eq!(profile.digest, request.profile_digest);
        assert!(
            resolver
                .resolve_exact::<TestProfile>(&report.work_id, &digest(b"other"))
                .is_err()
        );
    }

    #[test]
    fn changed_profile_cannot_duplicate_one_stable_work_identity() {
        let temp = TempDir::new().expect("temp");
        let request = request();
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

    #[test]
    fn apply_resumes_an_exact_partial_publication() {
        let temp = TempDir::new().expect("temp");
        let request = request();
        let policy = policy(vec![request.repository.clone()]);
        let identities = PublicationIdentities::new(&request);
        let ledger = WorkLedger::open(temp.path()).expect("ledger");

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
