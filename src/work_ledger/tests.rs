use super::delivery::*;
use super::route::{
    AdapterAxis, AdapterBindingRecord, AgentName, AgentRoute, AgentRouteRecord,
    LaunchProfileRecord, NativeSessionRoute, OpaqueRef, ProviderRoute, ProviderRouteRecord,
    RouteProvenanceRecord, Sha256Digest, TerminalRoute, TerminalRouteRecord,
};
use super::route_change::*;
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
    #[derive(serde::Serialize)]
    struct FileBinding<'a> {
        path: &'a str,
        sha256: &'a str,
    }
    #[derive(serde::Serialize)]
    struct WrapperBinding<'a> {
        companion_path: &'a str,
        companion_sha256: &'a str,
        subrouter_path: &'a str,
        subrouter_sha256: &'a str,
    }
    let opaque = |label: &str| OpaqueRef::derive("test", label.as_bytes());
    let account_digest = digest(b"account");
    let headers_digest = digest(b"headers");
    let subrouter_digest = digest(b"subrouter executable");
    let companion_digest = digest(b"binary");
    let account = serde_json::to_vec(&FileBinding {
        path: "/Users/test/.config/pulp/secrets/subrouter-account",
        sha256: &account_digest,
    })
    .expect("account binding");
    let headers = serde_json::to_vec(&FileBinding {
        path: "/Users/test/.config/pulp/secrets/subrouter-headers",
        sha256: &headers_digest,
    })
    .expect("headers binding");
    let wrapper = serde_json::to_vec(&WrapperBinding {
        companion_path: "/usr/bin/false",
        companion_sha256: &companion_digest,
        subrouter_path: "/Users/test/.local/bin/subrouter",
        subrouter_sha256: &subrouter_digest,
    })
    .expect("wrapper binding");
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
                    native_resume_ref: OpaqueRef::derive(
                        "native-resume-id",
                        b"cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                    ),
                    account_ref: OpaqueRef::derive("subrouter-account-file", &account),
                    model_ref: OpaqueRef::derive("subrouter-model-id", b"gpt-5.6-sol"),
                    wrapper_ref: OpaqueRef::derive("subrouter-wrapper", &wrapper),
                    session_headers_ref: OpaqueRef::derive(
                        "subrouter-session-headers-file",
                        &headers,
                    ),
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
            OpaqueRef::derive("subrouter-wrapper", &wrapper),
            Sha256Digest::of_bytes(b"config"),
            "subrouter".to_owned(),
        )
        .expect("profile"),
    )
    .expect("provenance");
    let route = RouteRegistration::new(
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
    .expect("route registration");
    (route, agent_adapter)
}

mod delivery;
mod importer;
mod lifecycle;
mod persistence;
mod protected_objects;
mod route_change;
