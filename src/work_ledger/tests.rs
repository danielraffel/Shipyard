use super::route::{
    AdapterAxis, AdapterBindingRecord, AgentName, AgentRoute, AgentRouteRecord,
    LaunchProfileRecord, NativeSessionRoute, OpaqueRef, ProviderRoute, ProviderRouteRecord,
    RouteProvenanceRecord, Sha256Digest, TerminalRoute, TerminalRouteRecord,
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

fn adapter_binding(axis: AdapterAxis, name: &str, label: &str) -> AdapterBindingRecord {
    let opaque = |value: &str| OpaqueRef::derive("test", value.as_bytes());
    AdapterBindingRecord::new(
        axis,
        name,
        opaque(&format!("{label} registry")),
        2,
        4,
        Sha256Digest::of_bytes(format!("{label} implementation").as_bytes()),
        Sha256Digest::of_bytes(format!("{label} configuration").as_bytes()),
        Sha256Digest::of_bytes(format!("{label} capabilities").as_bytes()),
    )
    .expect("adapter binding")
}

fn sample_registered_route(work_id: &str) -> (RouteRegistration, Vec<AdapterBindingRecord>) {
    let opaque = |label: &str| OpaqueRef::derive("test", label.as_bytes());
    let terminal_adapter = adapter_binding(AdapterAxis::Terminal, "wezterm", "wezterm");
    let agent_adapter = adapter_binding(AdapterAxis::Agent, "qwen", "qwen");
    let provenance = RouteProvenanceRecord::new(
        TerminalRouteRecord::new(TerminalRoute::Registered {
            adapter: terminal_adapter.clone(),
            route_ref: opaque("wezterm route"),
        }),
        AgentRouteRecord::new(
            agent_adapter.clone(),
            AgentRoute::Named {
                name: AgentName::parse("qwen").expect("qwen adapter"),
                session: NativeSessionRoute {
                    native_session_ref: opaque("session"),
                    native_resume_ref: opaque("resume"),
                    account_ref: opaque("account"),
                    model_ref: opaque("model"),
                    wrapper_ref: opaque("wrapper"),
                    session_headers_ref: opaque("headers"),
                    session_headers_sha256: Sha256Digest::of_bytes(b"headers"),
                },
            },
        )
        .expect("agent route"),
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
    (route, vec![terminal_adapter, agent_adapter])
}

fn sample_route(work_id: &str, work_generation: u64) -> (RouteRegistration, AdapterBindingRecord) {
    sample_route_labeled(work_id, work_generation, "")
}

fn sample_route_labeled(
    work_id: &str,
    work_generation: u64,
    suffix: &str,
) -> (RouteRegistration, AdapterBindingRecord) {
    let labeled = |label: &str| {
        if suffix.is_empty() {
            label.to_owned()
        } else {
            format!("{label}:{suffix}")
        }
    };
    let opaque = |label: &str| OpaqueRef::derive("test", labeled(label).as_bytes());
    let agent_adapter = adapter_binding(AdapterAxis::Agent, "codex", "codex");
    let provenance = RouteProvenanceRecord::new(
        TerminalRouteRecord::new(TerminalRoute::Cmux {
            workspace_ref: opaque("workspace"),
            pane_ref: opaque("pane"),
            surface_ref: opaque("surface"),
        }),
        AgentRouteRecord::new(
            agent_adapter.clone(),
            AgentRoute::Codex {
                session: NativeSessionRoute {
                    native_session_ref: opaque("session"),
                    native_resume_ref: opaque("resume"),
                    account_ref: opaque("account"),
                    model_ref: opaque("model"),
                    wrapper_ref: opaque("wrapper"),
                    session_headers_ref: opaque("headers"),
                    session_headers_sha256: Sha256Digest::of_bytes(b"headers"),
                },
            },
        )
        .expect("agent route"),
        ProviderRouteRecord::new(ProviderRoute::Subrouter {
            server_ref: opaque("server"),
            route_ref: opaque("subrouter route"),
        }),
        LaunchProfileRecord::new(
            OpaqueRef::derive("launch-profile", digest(b"profile").as_bytes()),
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
    let route = RouteRegistration::new(
        opaque_ref("route", &labeled("registered")),
        work_id.to_owned(),
        "0123456789012345678901234567890123456789".to_owned(),
        work_generation,
        opaque_ref("owner", "owner-private-id"),
        3,
        1,
        opaque_ref("machine", "m3"),
        provenance,
    )
    .expect("route registration");
    (route, agent_adapter)
}

mod dispatch;
mod importer;
mod lifecycle;
mod persistence;
mod protected_objects;
