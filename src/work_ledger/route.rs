//! Strict, secret-free provenance for terminal, agent, and provider routes.
//!
//! Route records retain only opaque references and content digests. Callers
//! must keep the referenced launch material in a separately protected store.

use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

const ROUTE_SCHEMA_VERSION: u32 = 1;
const OPAQUE_REF_PREFIX: &str = "opaque:sha256:";

/// A failure to construct or validate route provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RouteProvenanceError {
    InvalidOpaqueRef,
    InvalidSha256,
    InvalidAgentName,
    InvalidGeneration(&'static str),
    UnsupportedVersion { record: &'static str, version: u32 },
    IntegrityMismatch,
    Serialization(String),
}

impl Display for RouteProvenanceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOpaqueRef => formatter.write_str("invalid opaque SHA-256 reference"),
            Self::InvalidSha256 => formatter.write_str("invalid lowercase SHA-256 digest"),
            Self::InvalidAgentName => formatter.write_str("invalid named agent adapter"),
            Self::InvalidGeneration(field) => {
                write!(formatter, "{field} must be greater than zero")
            }
            Self::UnsupportedVersion { record, version } => {
                write!(formatter, "unsupported {record} schema version {version}")
            }
            Self::IntegrityMismatch => formatter.write_str("route provenance integrity mismatch"),
            Self::Serialization(error) => {
                write!(formatter, "route provenance serialization failed: {error}")
            }
        }
    }
}

impl std::error::Error for RouteProvenanceError {}

/// A non-reversible reference into a separately protected store.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(super) struct OpaqueRef(String);

impl OpaqueRef {
    /// Validate an already-redacted reference.
    pub(super) fn parse(value: impl Into<String>) -> Result<Self, RouteProvenanceError> {
        let value = value.into();
        let digest = value
            .strip_prefix(OPAQUE_REF_PREFIX)
            .ok_or(RouteProvenanceError::InvalidOpaqueRef)?;
        if !is_lower_hex_256(digest) {
            return Err(RouteProvenanceError::InvalidOpaqueRef);
        }
        Ok(Self(value))
    }

    /// Derive a domain-separated reference without retaining the source bytes.
    pub(super) fn derive(namespace: &str, source: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"shipyard.work-ledger.opaque-ref.v1\0");
        digest.update(namespace.as_bytes());
        digest.update(b"\0");
        digest.update(source);
        Self(format!(
            "{OPAQUE_REF_PREFIX}{}",
            hex::encode(digest.finalize())
        ))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for OpaqueRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// A canonical lowercase SHA-256 digest.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(super) struct Sha256Digest(String);

impl Sha256Digest {
    pub(super) fn parse(value: impl Into<String>) -> Result<Self, RouteProvenanceError> {
        let value = value.into();
        if !is_lower_hex_256(&value) {
            return Err(RouteProvenanceError::InvalidSha256);
        }
        Ok(Self(value))
    }

    pub(super) fn of_bytes(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Extensible non-core agent adapter name (`agy`, `qwen`, `kimi`, and peers).
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(super) struct AgentName(String);

impl AgentName {
    pub(super) fn parse(value: impl Into<String>) -> Result<Self, RouteProvenanceError> {
        let value = value.into();
        let valid = is_registry_name(&value) && value != "codex" && value != "claude";
        if !valid {
            return Err(RouteProvenanceError::InvalidAgentName);
        }
        Ok(Self(value))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for AgentName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Terminal runtime is independent from agent and provider routing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum TerminalRoute {
    Cmux {
        workspace_ref: OpaqueRef,
        pane_ref: OpaqueRef,
        surface_ref: OpaqueRef,
    },
    HerdR {
        session_ref: OpaqueRef,
        workspace_ref: OpaqueRef,
        tab_ref: OpaqueRef,
        pane_ref: OpaqueRef,
    },
    Registered {
        name: AgentName,
        registry_ref: OpaqueRef,
        generation: u64,
        revision: u64,
        implementation_sha256: Sha256Digest,
        configuration_sha256: Sha256Digest,
        capabilities_sha256: Sha256Digest,
        route_ref: OpaqueRef,
    },
}

/// Versioned terminal-runtime provenance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TerminalRouteRecord {
    pub schema_version: u32,
    pub route: TerminalRoute,
}

impl TerminalRouteRecord {
    pub(super) fn new(route: TerminalRoute) -> Self {
        Self {
            schema_version: ROUTE_SCHEMA_VERSION,
            route,
        }
    }

    fn validate(&self) -> Result<(), RouteProvenanceError> {
        validate_version("terminal route", self.schema_version)
    }
}

/// Native resume inputs are opaque but complete enough to reproduce routing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeSessionRoute {
    pub native_session_ref: OpaqueRef,
    pub native_resume_ref: OpaqueRef,
    pub account_ref: OpaqueRef,
    pub model_ref: OpaqueRef,
    pub wrapper_ref: OpaqueRef,
    pub session_headers_ref: OpaqueRef,
    pub session_headers_sha256: Sha256Digest,
}

/// Agent/session adapter, separate from its terminal and provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum AgentRoute {
    Codex {
        session: NativeSessionRoute,
    },
    Claude {
        session: NativeSessionRoute,
    },
    Named {
        name: AgentName,
        session: NativeSessionRoute,
    },
}

/// Versioned agent/session provenance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentRouteRecord {
    pub schema_version: u32,
    pub route: AgentRoute,
}

impl AgentRouteRecord {
    pub(super) fn new(route: AgentRoute) -> Self {
        Self {
            schema_version: ROUTE_SCHEMA_VERSION,
            route,
        }
    }

    fn validate(&self) -> Result<(), RouteProvenanceError> {
        validate_version("agent route", self.schema_version)
    }
}

/// Provider routing is explicit; it must never silently degrade to direct.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ProviderRoute {
    Direct {
        endpoint_ref: OpaqueRef,
    },
    Subrouter {
        server_ref: OpaqueRef,
        route_ref: OpaqueRef,
    },
    CliProxyApi {
        endpoint_ref: OpaqueRef,
        route_ref: OpaqueRef,
    },
    Registered {
        name: AgentName,
        registry_ref: OpaqueRef,
        generation: u64,
        revision: u64,
        implementation_sha256: Sha256Digest,
        configuration_sha256: Sha256Digest,
        capabilities_sha256: Sha256Digest,
        route_ref: OpaqueRef,
    },
}

/// Versioned provider-routing provenance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderRouteRecord {
    pub schema_version: u32,
    pub route: ProviderRoute,
}

impl ProviderRouteRecord {
    pub(super) fn new(route: ProviderRoute) -> Self {
        Self {
            schema_version: ROUTE_SCHEMA_VERSION,
            route,
        }
    }

    fn validate(&self) -> Result<(), RouteProvenanceError> {
        validate_version("provider route", self.schema_version)
    }
}

/// Immutable identity of the executable launch profile used for this route.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LaunchProfileRecord {
    pub schema_version: u32,
    pub profile_ref: OpaqueRef,
    pub generation: u64,
    pub revision: u64,
    pub executable_sha256: Sha256Digest,
    pub wrapper_ref: OpaqueRef,
    pub configuration_sha256: Sha256Digest,
    pub provider_kind: String,
}

impl LaunchProfileRecord {
    pub(super) fn new(
        profile_ref: OpaqueRef,
        generation: u64,
        revision: u64,
        executable_sha256: Sha256Digest,
        wrapper_ref: OpaqueRef,
        configuration_sha256: Sha256Digest,
        provider_kind: String,
    ) -> Result<Self, RouteProvenanceError> {
        let record = Self {
            schema_version: ROUTE_SCHEMA_VERSION,
            profile_ref,
            generation,
            revision,
            executable_sha256,
            wrapper_ref,
            configuration_sha256,
            provider_kind,
        };
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<(), RouteProvenanceError> {
        validate_version("launch profile", self.schema_version)?;
        if self.generation == 0 {
            return Err(RouteProvenanceError::InvalidGeneration(
                "launch profile generation",
            ));
        }
        if self.revision == 0 {
            return Err(RouteProvenanceError::InvalidGeneration(
                "launch profile revision",
            ));
        }
        if !is_registry_name(&self.provider_kind) {
            return Err(RouteProvenanceError::Serialization(
                "unsupported launch-profile provider".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct IntegrityPayload<'a> {
    schema_version: u32,
    terminal: &'a TerminalRouteRecord,
    agent: &'a AgentRouteRecord,
    provider: &'a ProviderRouteRecord,
    launch_profile: &'a LaunchProfileRecord,
}

/// Complete, integrity-bound route provenance. Deserialization alone is not
/// trust: callers must invoke [`RouteProvenanceRecord::validate`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RouteProvenanceRecord {
    pub schema_version: u32,
    pub terminal: TerminalRouteRecord,
    pub agent: AgentRouteRecord,
    pub provider: ProviderRouteRecord,
    pub launch_profile: LaunchProfileRecord,
    pub integrity_sha256: Sha256Digest,
}

impl RouteProvenanceRecord {
    pub(super) fn new(
        terminal: TerminalRouteRecord,
        agent: AgentRouteRecord,
        provider: ProviderRouteRecord,
        launch_profile: LaunchProfileRecord,
    ) -> Result<Self, RouteProvenanceError> {
        let mut record = Self {
            schema_version: ROUTE_SCHEMA_VERSION,
            terminal,
            agent,
            provider,
            launch_profile,
            integrity_sha256: Sha256Digest::of_bytes(&[]),
        };
        record.validate_components()?;
        record.integrity_sha256 = record.recompute_integrity()?;
        Ok(record)
    }

    /// Recompute the digest over canonical fixed-shape JSON without the digest.
    pub(super) fn recompute_integrity(&self) -> Result<Sha256Digest, RouteProvenanceError> {
        let payload = IntegrityPayload {
            schema_version: self.schema_version,
            terminal: &self.terminal,
            agent: &self.agent,
            provider: &self.provider,
            launch_profile: &self.launch_profile,
        };
        let bytes = serde_json::to_vec(&payload)
            .map_err(|error| RouteProvenanceError::Serialization(error.to_string()))?;
        Ok(Sha256Digest::of_bytes(&bytes))
    }

    /// Fail closed on missing, malformed, unsupported, or tampered provenance.
    pub(super) fn validate(&self) -> Result<(), RouteProvenanceError> {
        self.validate_components()?;
        if self.recompute_integrity()? != self.integrity_sha256 {
            return Err(RouteProvenanceError::IntegrityMismatch);
        }
        Ok(())
    }

    pub(super) fn terminal_kind(&self) -> &str {
        match &self.terminal.route {
            TerminalRoute::Cmux { .. } => "cmux",
            TerminalRoute::HerdR { .. } => "herdr",
            TerminalRoute::Registered { name, .. } => name.as_str(),
        }
    }

    pub(super) fn agent_kind(&self) -> &str {
        match &self.agent.route {
            AgentRoute::Codex { .. } => "codex",
            AgentRoute::Claude { .. } => "claude",
            AgentRoute::Named { name, .. } => name.as_str(),
        }
    }

    pub(super) fn provider_kind(&self) -> &str {
        match &self.provider.route {
            ProviderRoute::Direct { .. } => "direct",
            ProviderRoute::Subrouter { .. } => "subrouter",
            ProviderRoute::CliProxyApi { .. } => "cliproxyapi",
            ProviderRoute::Registered { name, .. } => name.as_str(),
        }
    }

    pub(super) fn launch_generation(&self) -> u64 {
        self.launch_profile.generation
    }

    pub(super) fn integrity(&self) -> &str {
        self.integrity_sha256.as_str()
    }

    pub(super) fn registered_adapters(&self) -> Vec<RegisteredAdapterBinding<'_>> {
        let mut bindings = Vec::new();
        if let TerminalRoute::Registered {
            name,
            registry_ref,
            generation,
            revision,
            implementation_sha256,
            configuration_sha256,
            capabilities_sha256,
            ..
        } = &self.terminal.route
        {
            bindings.push(RegisteredAdapterBinding {
                axis: "terminal",
                name: name.as_str(),
                registry_ref: registry_ref.as_str(),
                generation: *generation,
                revision: *revision,
                implementation_sha256: implementation_sha256.as_str(),
                configuration_sha256: configuration_sha256.as_str(),
                capabilities_sha256: capabilities_sha256.as_str(),
            });
        }
        if let ProviderRoute::Registered {
            name,
            registry_ref,
            generation,
            revision,
            implementation_sha256,
            configuration_sha256,
            capabilities_sha256,
            ..
        } = &self.provider.route
        {
            bindings.push(RegisteredAdapterBinding {
                axis: "provider",
                name: name.as_str(),
                registry_ref: registry_ref.as_str(),
                generation: *generation,
                revision: *revision,
                implementation_sha256: implementation_sha256.as_str(),
                configuration_sha256: configuration_sha256.as_str(),
                capabilities_sha256: capabilities_sha256.as_str(),
            });
        }
        bindings
    }

    fn validate_components(&self) -> Result<(), RouteProvenanceError> {
        validate_version("route provenance", self.schema_version)?;
        self.terminal.validate()?;
        self.agent.validate()?;
        self.provider.validate()?;
        self.launch_profile.validate()?;
        if matches!(&self.terminal.route, TerminalRoute::Registered { name, .. }
            if matches!(name.as_str(), "cmux" | "herdr"))
            || matches!(&self.provider.route, ProviderRoute::Registered { name, .. }
                if matches!(name.as_str(), "direct" | "subrouter" | "cliproxyapi"))
        {
            return Err(RouteProvenanceError::IntegrityMismatch);
        }
        for binding in self.registered_adapters() {
            if binding.generation == 0 || binding.revision == 0 {
                return Err(RouteProvenanceError::InvalidGeneration(
                    "registered adapter generation or revision",
                ));
            }
        }
        if self.provider_kind() != self.launch_profile.provider_kind {
            return Err(RouteProvenanceError::IntegrityMismatch);
        }
        let session_wrapper = match &self.agent.route {
            AgentRoute::Codex { session }
            | AgentRoute::Claude { session }
            | AgentRoute::Named { session, .. } => &session.wrapper_ref,
        };
        if session_wrapper != &self.launch_profile.wrapper_ref {
            return Err(RouteProvenanceError::IntegrityMismatch);
        }
        Ok(())
    }
}

pub(super) struct RegisteredAdapterBinding<'a> {
    pub axis: &'a str,
    pub name: &'a str,
    pub registry_ref: &'a str,
    pub generation: u64,
    pub revision: u64,
    pub implementation_sha256: &'a str,
    pub configuration_sha256: &'a str,
    pub capabilities_sha256: &'a str,
}

fn validate_version(record: &'static str, version: u32) -> Result<(), RouteProvenanceError> {
    if version != ROUTE_SCHEMA_VERSION {
        return Err(RouteProvenanceError::UnsupportedVersion { record, version });
    }
    Ok(())
}

fn is_lower_hex_256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_registry_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'0'..=b'9' => true,
            b'.' | b'_' | b'-' => index > 0,
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn opaque(label: &str) -> OpaqueRef {
        OpaqueRef::derive("test", label.as_bytes())
    }

    fn session() -> NativeSessionRoute {
        NativeSessionRoute {
            native_session_ref: opaque("native-session"),
            native_resume_ref: opaque("native-resume"),
            account_ref: opaque("account"),
            model_ref: opaque("model"),
            wrapper_ref: opaque("subrouter-wrapper"),
            session_headers_ref: opaque("session-headers"),
            session_headers_sha256: Sha256Digest::of_bytes(b"redacted session headers"),
        }
    }

    fn launch_profile() -> LaunchProfileRecord {
        LaunchProfileRecord::new(
            opaque("launch-profile"),
            3,
            7,
            Sha256Digest::of_bytes(b"executable"),
            opaque("subrouter-wrapper"),
            Sha256Digest::of_bytes(b"configuration"),
            "subrouter".to_owned(),
        )
        .expect("valid launch profile")
    }

    fn record() -> RouteProvenanceRecord {
        RouteProvenanceRecord::new(
            TerminalRouteRecord::new(TerminalRoute::HerdR {
                session_ref: opaque("herdr-session"),
                workspace_ref: opaque("herdr-workspace"),
                tab_ref: opaque("herdr-tab"),
                pane_ref: opaque("herdr-pane"),
            }),
            AgentRouteRecord::new(AgentRoute::Codex { session: session() }),
            ProviderRouteRecord::new(ProviderRoute::Subrouter {
                server_ref: opaque("subrouter-server"),
                route_ref: opaque("subrouter-route"),
            }),
            launch_profile(),
        )
        .expect("valid provenance")
    }

    #[test]
    fn round_trip_preserves_explicit_herdr_codex_subrouter_route() {
        let record = record();
        let encoded = serde_json::to_vec(&record).expect("encode provenance");
        let decoded: RouteProvenanceRecord =
            serde_json::from_slice(&encoded).expect("decode provenance");

        assert_eq!(decoded, record);
        decoded.validate().expect("integrity is valid");
    }

    #[test]
    fn supports_cmux_claude_direct_and_named_cli_proxy_routes() {
        let terminal = TerminalRouteRecord::new(TerminalRoute::Cmux {
            workspace_ref: opaque("workspace"),
            pane_ref: opaque("pane"),
            surface_ref: opaque("surface"),
        });
        let direct = RouteProvenanceRecord::new(
            terminal.clone(),
            AgentRouteRecord::new(AgentRoute::Claude { session: session() }),
            ProviderRouteRecord::new(ProviderRoute::Direct {
                endpoint_ref: opaque("direct-endpoint"),
            }),
            LaunchProfileRecord::new(
                opaque("direct-profile"),
                3,
                7,
                Sha256Digest::of_bytes(b"direct executable"),
                opaque("subrouter-wrapper"),
                Sha256Digest::of_bytes(b"direct configuration"),
                "direct".to_owned(),
            )
            .expect("direct profile"),
        )
        .expect("direct route");
        direct.validate().expect("direct route integrity");

        let named = RouteProvenanceRecord::new(
            terminal,
            AgentRouteRecord::new(AgentRoute::Named {
                name: AgentName::parse("qwen").expect("named adapter"),
                session: session(),
            }),
            ProviderRouteRecord::new(ProviderRoute::CliProxyApi {
                endpoint_ref: opaque("proxy-endpoint"),
                route_ref: opaque("proxy-route"),
            }),
            LaunchProfileRecord::new(
                opaque("proxy-profile"),
                3,
                7,
                Sha256Digest::of_bytes(b"proxy executable"),
                opaque("subrouter-wrapper"),
                Sha256Digest::of_bytes(b"proxy configuration"),
                "cliproxyapi".to_owned(),
            )
            .expect("proxy profile"),
        )
        .expect("proxy route");
        named.validate().expect("proxy route integrity");
    }

    #[test]
    fn tampering_with_provider_route_fails_integrity() {
        let mut value = serde_json::to_value(record()).expect("encode provenance");
        value["provider"]["route"]["route_ref"] =
            Value::String(opaque("different-route").as_str().to_owned());
        let decoded: RouteProvenanceRecord =
            serde_json::from_value(value).expect("shape remains valid");

        assert_eq!(
            decoded.validate(),
            Err(RouteProvenanceError::IntegrityMismatch)
        );
    }

    #[test]
    fn malformed_or_missing_provenance_fails_closed() {
        let mut unknown = serde_json::to_value(record()).expect("encode provenance");
        unknown["provider"]["unexpected"] = json!(true);
        assert!(serde_json::from_value::<RouteProvenanceRecord>(unknown).is_err());

        let mut missing = serde_json::to_value(record()).expect("encode provenance");
        missing["agent"]["route"]
            .as_object_mut()
            .expect("agent route object")
            .remove("session");
        assert!(serde_json::from_value::<RouteProvenanceRecord>(missing).is_err());

        let mut fallback = serde_json::to_value(record()).expect("encode provenance");
        fallback["provider"]["route"]["kind"] = json!("direct");
        assert!(serde_json::from_value::<RouteProvenanceRecord>(fallback).is_err());

        let mut missing_headers = serde_json::to_value(record()).expect("encode provenance");
        missing_headers["agent"]["route"]["session"]
            .as_object_mut()
            .expect("session object")
            .remove("session_headers_ref");
        assert!(serde_json::from_value::<RouteProvenanceRecord>(missing_headers).is_err());
    }

    #[test]
    fn registered_adapters_are_open_and_wrapper_drift_fails_closed() {
        let registered = RouteProvenanceRecord::new(
            TerminalRouteRecord::new(TerminalRoute::Registered {
                name: AgentName::parse("wezterm").expect("terminal name"),
                registry_ref: opaque("wezterm registry"),
                generation: 2,
                revision: 4,
                implementation_sha256: Sha256Digest::of_bytes(b"wezterm implementation"),
                configuration_sha256: Sha256Digest::of_bytes(b"wezterm configuration"),
                capabilities_sha256: Sha256Digest::of_bytes(b"wezterm capabilities"),
                route_ref: opaque("wezterm route"),
            }),
            AgentRouteRecord::new(AgentRoute::Named {
                name: AgentName::parse("kimi").expect("agent name"),
                session: session(),
            }),
            ProviderRouteRecord::new(ProviderRoute::Registered {
                name: AgentName::parse("future-router").expect("provider name"),
                registry_ref: opaque("future registry"),
                generation: 3,
                revision: 8,
                implementation_sha256: Sha256Digest::of_bytes(b"future implementation"),
                configuration_sha256: Sha256Digest::of_bytes(b"future configuration"),
                capabilities_sha256: Sha256Digest::of_bytes(b"future capabilities"),
                route_ref: opaque("future route"),
            }),
            LaunchProfileRecord::new(
                opaque("future profile"),
                1,
                1,
                Sha256Digest::of_bytes(b"future executable"),
                opaque("subrouter-wrapper"),
                Sha256Digest::of_bytes(b"future configuration"),
                "future-router".to_owned(),
            )
            .expect("future profile"),
        )
        .expect("registered route");
        registered.validate().expect("registered route valid");

        let mut mismatched = serde_json::to_value(record()).expect("encode route");
        mismatched["launch_profile"]["wrapper_ref"] = json!(opaque("different wrapper").as_str());
        mismatched["integrity_sha256"] = json!(Sha256Digest::of_bytes(b"stale").as_str());
        let mismatched: RouteProvenanceRecord =
            serde_json::from_value(mismatched).expect("decode mismatch");
        assert_eq!(
            mismatched.validate(),
            Err(RouteProvenanceError::IntegrityMismatch)
        );
    }

    #[test]
    fn opaque_refs_and_digests_require_exact_lowercase_shapes() {
        assert!(OpaqueRef::parse(format!("{OPAQUE_REF_PREFIX}{}", "a".repeat(64))).is_ok());
        assert!(OpaqueRef::parse("a".repeat(64)).is_err());
        assert!(OpaqueRef::parse(format!("{OPAQUE_REF_PREFIX}{}", "A".repeat(64))).is_err());
        assert!(Sha256Digest::parse("f".repeat(64)).is_ok());
        assert!(Sha256Digest::parse(format!("sha256:{}", "f".repeat(64))).is_err());
    }

    #[test]
    fn unsupported_versions_and_zero_launch_revisions_are_rejected() {
        let mut value = serde_json::to_value(record()).expect("encode provenance");
        value["terminal"]["schema_version"] = json!(2);
        let decoded: RouteProvenanceRecord =
            serde_json::from_value(value).expect("decode future version structurally");
        assert!(matches!(
            decoded.validate(),
            Err(RouteProvenanceError::UnsupportedVersion {
                record: "terminal route",
                version: 2
            })
        ));

        assert_eq!(
            LaunchProfileRecord::new(
                opaque("profile"),
                0,
                1,
                Sha256Digest::of_bytes(b"executable"),
                opaque("wrapper"),
                Sha256Digest::of_bytes(b"config"),
                "subrouter".to_owned(),
            ),
            Err(RouteProvenanceError::InvalidGeneration(
                "launch profile generation"
            ))
        );
    }

    #[test]
    fn named_agents_are_extensible_but_core_names_are_reserved() {
        for name in ["agy", "qwen", "kimi", "future-agent.v2"] {
            assert_eq!(AgentName::parse(name).expect("valid name").as_str(), name);
        }
        for name in ["codex", "claude", "Qwen", "../qwen", ""] {
            assert!(AgentName::parse(name).is_err(), "accepted {name:?}");
        }
    }

    #[test]
    fn serialized_provenance_contains_no_raw_launch_material() {
        let raw_secret = "secret-account-and-session-value";
        let reference = OpaqueRef::derive("account", raw_secret.as_bytes());
        assert!(!reference.as_str().contains(raw_secret));

        let encoded = serde_json::to_string(&record()).expect("encode provenance");
        assert!(!encoded.contains("native-session"));
        assert!(!encoded.contains("subrouter-wrapper"));
        assert!(!encoded.contains("redacted session headers"));
    }
}
