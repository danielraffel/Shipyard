use super::*;
use crate::app::merge_steward_cmd::TerminalProvenanceKind;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn handoff(
    provider: Option<&str>,
    transport: Option<&str>,
    cmux_surface_id: Option<&str>,
) -> TerminalHandoff {
    TerminalHandoff {
        dedupe_key: format!("owner/repo@main#7:{HEAD}:failure:trigger"),
        repo: "owner/repo".to_owned(),
        base: "main".to_owned(),
        pr_number: 7,
        head_sha: HEAD.to_owned(),
        outcome: TerminalHandoffOutcome::ActionableFailure,
        trigger: "actionable_terminal_failure".to_owned(),
        next_action: "wake_exact_owner_for_causal_repair".to_owned(),
        origin_machine: Some("m3".to_owned()),
        owner_id: Some("owner-exact".to_owned()),
        ownership_generation: Some(4),
        owner_disposition: "original_owner".to_owned(),
        owner_route_id: Some("route-exact".to_owned()),
        owner_provider: provider.map(str::to_owned),
        resume_transport: transport.map(str::to_owned),
        owner_terminal_provenance: cmux_surface_id.map(|_| TerminalProvenanceKind::Cmux),
        wake_consumer_available: false,
        failure_contexts: vec!["windows@app=9".to_owned()],
        phase: TerminalHandoffPhase::Recorded,
        created_at: "2026-08-27T00:00:00Z".to_owned(),
        updated_at: "2026-08-27T00:00:00Z".to_owned(),
    }
}

#[test]
fn provider_native_route_precedes_cmux_fallback() {
    let record = record_for(&handoff(
        Some("codex"),
        Some("codex_queue"),
        Some("surface-7"),
    ))
    .expect("record");
    assert!(matches!(
        record.adapter,
        Some(ResumeAdapterV1::ProviderNative {
            ref provider,
            ref transport,
            ref route_id,
        }) if provider == "codex" && transport == "codex_queue" && route_id == "route-exact"
    ));
    assert!(!record.dispatch_enabled);
}

#[test]
fn cmux_is_only_a_fallback_adapter() {
    let record = record_for(&handoff(
        Some("future-provider"),
        Some("unsupported-native-route"),
        Some("surface-7"),
    ))
    .expect("record");
    assert!(matches!(
        record.adapter,
        Some(ResumeAdapterV1::Cmux {
            ref route_id,
        }) if route_id == "route-exact"
    ));
    assert_eq!(
        record.routing_disposition,
        ResumeRoutingDisposition::OriginalOwner
    );
    assert!(!record.dispatch_enabled);
}

#[test]
fn missing_route_stays_recorded_and_unroutable() {
    let mut terminal = handoff(None, None, None);
    terminal.owner_disposition = "route_registry_required".to_owned();
    terminal.owner_route_id = None;
    let record = record_for(&terminal).expect("record");
    assert_eq!(
        record.routing_disposition,
        ResumeRoutingDisposition::RouteRegistryRequired
    );
    assert_eq!(record.adapter, None);
    assert!(!record.dispatch_enabled);
}

#[test]
fn exact_head_and_owner_generation_fence_resume_identity() {
    let first = record_for(&handoff(Some("codex"), Some("codex_queue"), None)).expect("first");
    let mut newer_owner = handoff(Some("codex"), Some("codex_queue"), None);
    newer_owner.ownership_generation = Some(5);
    let second = record_for(&newer_owner).expect("new owner");
    let mut newer_head = newer_owner;
    newer_head.head_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned();
    newer_head.dedupe_key = format!("owner/repo@main#7:{}:failure:trigger", newer_head.head_sha);
    let third = record_for(&newer_head).expect("new head");
    assert_ne!(first.resume_id, second.resume_id);
    assert_ne!(second.resume_id, third.resume_id);
}

#[test]
fn reconciliation_is_idempotent_and_resolves_removed_authority() {
    let mut ledger = StewardLedger::default();
    let terminal = handoff(Some("claude"), Some("claude_resume"), None);
    ledger
        .terminal_handoffs
        .insert(terminal.dedupe_key.clone(), terminal);
    assert!(reconcile_resume_records(&mut ledger).expect("first reconcile"));
    let first = ledger.resume_records.clone();
    assert!(!reconcile_resume_records(&mut ledger).expect("replay"));
    assert_eq!(ledger.resume_records, first);

    ledger.terminal_handoffs.clear();
    assert!(reconcile_resume_records(&mut ledger).expect("resolve"));
    assert!(
        ledger
            .resume_records
            .values()
            .all(|record| record.phase == ResumeRecordPhase::Resolved)
    );
}
