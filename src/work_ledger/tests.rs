use super::route::{
    AgentName, AgentRoute, AgentRouteRecord, LaunchProfileRecord, NativeSessionRoute, OpaqueRef,
    ProviderRoute, ProviderRouteRecord, RouteProvenanceRecord, Sha256Digest, TerminalRoute,
    TerminalRouteRecord,
};
use super::*;
use tempfile::TempDir;

fn sample_candidate() -> ImportCandidate {
    candidate(
        "resume_record",
        opaque_ref("src", "resume"),
        digest(b"content"),
        &serde_json::json!({
            "resume_id": "resume-private-id",
            "repo": "DanielRaffel/Pulp",
            "pr_number": 42,
            "head_sha": "0123456789012345678901234567890123456789",
            "owner_id": "owner-private-id",
            "ownership_generation": 3,
            "terminal_adapter": {"kind": "cmux", "route_id": "secret-route"},
            "provider_adapter": {"kind": "launch_profile", "account": "secret-account"},
            "phase": "recorded"
        }),
    )
}

fn sample_registered_route(work_id: &str) -> (RouteRegistration, AdapterRegistryEntry) {
    let opaque = |label: &str| OpaqueRef::derive("test", label.as_bytes());
    let registry_ref = opaque("wezterm registry");
    let implementation = Sha256Digest::of_bytes(b"wezterm implementation");
    let configuration = Sha256Digest::of_bytes(b"wezterm configuration");
    let capabilities = Sha256Digest::of_bytes(b"wezterm capabilities");
    let provenance = RouteProvenanceRecord::new(
        TerminalRouteRecord::new(TerminalRoute::Registered {
            name: AgentName::parse("wezterm").expect("adapter name"),
            registry_ref: registry_ref.clone(),
            generation: 2,
            revision: 4,
            implementation_sha256: implementation.clone(),
            configuration_sha256: configuration.clone(),
            capabilities_sha256: capabilities.clone(),
            route_ref: opaque("wezterm route"),
        }),
        AgentRouteRecord::new(AgentRoute::Codex {
            session: NativeSessionRoute {
                native_session_ref: opaque("session"),
                native_resume_ref: opaque("resume"),
                account_ref: opaque("account"),
                model_ref: opaque("model"),
                wrapper_ref: opaque("wrapper"),
                session_headers_ref: opaque("headers"),
                session_headers_sha256: Sha256Digest::of_bytes(b"headers"),
            },
        }),
        ProviderRouteRecord::new(ProviderRoute::Subrouter {
            server_ref: opaque("server"),
            route_ref: opaque("subrouter route"),
        }),
        LaunchProfileRecord::new(
            opaque("profile"),
            3,
            1,
            Sha256Digest::of_bytes(b"binary"),
            opaque("wrapper"),
            Sha256Digest::of_bytes(b"config"),
            "subrouter".to_owned(),
        )
        .expect("profile"),
    )
    .expect("registered provenance");
    let route = RouteRegistration::new(
        opaque_ref("route", "registered adapter route"),
        work_id.to_owned(),
        "0123456789012345678901234567890123456789".to_owned(),
        1,
        opaque_ref("owner", "owner-private-id"),
        3,
        1,
        opaque_ref("machine", "m3"),
        provenance,
    )
    .expect("route registration");
    let adapter = AdapterRegistryEntry {
        registry_ref: registry_ref.as_str().to_owned(),
        axis: "terminal".to_owned(),
        name: "wezterm".to_owned(),
        generation: 2,
        revision: 4,
        implementation_digest: implementation.as_str().to_owned(),
        configuration_digest: configuration.as_str().to_owned(),
        capabilities_digest: capabilities.as_str().to_owned(),
    };
    (route, adapter)
}

fn sample_route(work_id: &str, work_generation: u64) -> RouteRegistration {
    let opaque = |label: &str| OpaqueRef::derive("test", label.as_bytes());
    let provenance = RouteProvenanceRecord::new(
        TerminalRouteRecord::new(TerminalRoute::Cmux {
            workspace_ref: opaque("workspace"),
            pane_ref: opaque("pane"),
            surface_ref: opaque("surface"),
        }),
        AgentRouteRecord::new(AgentRoute::Codex {
            session: NativeSessionRoute {
                native_session_ref: opaque("session"),
                native_resume_ref: opaque("resume"),
                account_ref: opaque("account"),
                model_ref: opaque("model"),
                wrapper_ref: opaque("wrapper"),
                session_headers_ref: opaque("headers"),
                session_headers_sha256: Sha256Digest::of_bytes(b"headers"),
            },
        }),
        ProviderRouteRecord::new(ProviderRoute::Subrouter {
            server_ref: opaque("server"),
            route_ref: opaque("subrouter route"),
        }),
        LaunchProfileRecord::new(
            opaque("profile"),
            3,
            1,
            Sha256Digest::of_bytes(b"binary"),
            opaque("wrapper"),
            Sha256Digest::of_bytes(b"config"),
            "subrouter".to_owned(),
        )
        .expect("profile"),
    )
    .expect("provenance");
    RouteRegistration::new(
        opaque_ref("route", "registered"),
        work_id.to_owned(),
        "0123456789012345678901234567890123456789".to_owned(),
        work_generation,
        opaque_ref("owner", "owner-private-id"),
        3,
        1,
        opaque_ref("machine", "m3"),
        provenance,
    )
    .expect("route registration")
}

mod importer;
mod lifecycle;
mod persistence;
