use super::*;
use crate::app::merge_steward_cmd::TerminalProvenanceKind;
use crate::app::merge_steward_cmd::handoff::ProviderRouteReferenceV1;
use crate::app::merge_steward_cmd::ledger::load_ledger;
use crate::app::merge_steward_cmd::resume_record::{
    AgentAdapterV1, ProviderAdapterV1, ResumeRecordPhase,
};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn owner(route: &str) -> TerminalOwnerRoute {
    TerminalOwnerRoute {
        origin_machine: "m3".to_owned(),
        owner_id: "owner-exact".to_owned(),
        ownership_generation: 1,
        owner_disposition: "original_owner".to_owned(),
        route_id: Some(route.to_owned()),
        provider: Some("codex".to_owned()),
        resume_transport: Some("codex_queue".to_owned()),
        terminal_provenance: Some(TerminalProvenanceKind::Absent),
        provider_route: None,
    }
}

fn subrouter_provider_route(generation: u64) -> ProviderRouteReferenceV1 {
    ProviderRouteReferenceV1 {
        profile_digest: "a".repeat(64),
        integrity_hash: "b".repeat(64),
        generation,
        revision: 2,
        provider: "subrouter".to_owned(),
        account: Some("account-a".to_owned()),
        model: Some("gpt-5.6-sol".to_owned()),
    }
}

fn fresh_agent_owner() -> TerminalOwnerRoute {
    TerminalOwnerRoute {
        origin_machine: "m3".to_owned(),
        owner_id: "fresh-agent-only".to_owned(),
        ownership_generation: 1,
        owner_disposition: "fresh_agent_only".to_owned(),
        route_id: None,
        provider: None,
        resume_transport: None,
        terminal_provenance: None,
        provider_route: None,
    }
}

#[test]
fn replay_is_idempotent_but_owner_drift_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
        vec!["windows@app=9".to_owned()],
    )
    .expect("first");
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
        vec!["windows@app=9".to_owned()],
    )
    .expect("replay");
    let error = persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-b")),
        vec!["windows@app=9".to_owned()],
    )
    .expect_err("owner drift");
    assert!(error.message().contains("identity changed"));

    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
        vec!["macos@app=42".to_owned()],
    )
    .expect("a distinct exact failure trigger supersedes stale same-head evidence");
    assert_eq!(ledger.terminal_handoffs.len(), 1);
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .next()
            .expect("current failure")
            .failure_contexts,
        vec!["macos@app=42"]
    );
}

#[test]
fn legacy_terminal_record_without_typed_provenance_replays_idempotently() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("legacy-route")),
        vec!["windows@app=9".to_owned()],
    )
    .expect("current record");
    let record = ledger
        .terminal_handoffs
        .values_mut()
        .next()
        .expect("terminal record");
    record.owner_terminal_provenance = None;
    save_ledger(&path, &ledger).expect("legacy ledger");

    let mut restarted = load_ledger(&path).expect("restart legacy ledger");
    persist_actionable_failure(
        &path,
        &mut restarted,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("legacy-route")),
        vec!["windows@app=9".to_owned()],
    )
    .expect("legacy replay");
    assert_eq!(restarted.terminal_handoffs.len(), 1);
    let record = restarted
        .terminal_handoffs
        .values()
        .next()
        .expect("terminal record");
    assert_eq!(record.owner_terminal_provenance, None);
    assert!(!record.wake_consumer_available);
}

#[test]
fn no_op_terminal_reconciliation_backfills_legacy_resume_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("legacy-route")),
        vec!["windows@app=9".to_owned()],
    )
    .expect("seed current ledger");
    ledger.resume_records.clear();
    save_ledger(&path, &ledger).expect("seed legacy ledger without resume projection");

    let mut restarted = load_ledger(&path).expect("restart legacy ledger");
    resolve_terminal_handoffs(&path, &mut restarted, "different/repo", "main", 99, HEAD)
        .expect("no-op terminal reconciliation backfills projection");

    assert_eq!(restarted.terminal_handoffs.len(), 1);
    assert_eq!(restarted.resume_records.len(), 1);
    let persisted = load_ledger(&path).expect("reload migrated ledger");
    assert_eq!(persisted.resume_records, restarted.resume_records);
}

#[test]
fn legacy_absent_provenance_enriches_to_cmux_without_weakening_route_fences() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    let mut legacy = owner("cmux-route");
    legacy.provider = Some("future-provider".to_owned());
    legacy.resume_transport = Some("future-native".to_owned());
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(legacy.clone()),
        vec!["windows@app=9".to_owned()],
    )
    .expect("legacy absent provenance");
    assert_eq!(
        ledger
            .resume_records
            .values()
            .next()
            .expect("legacy resume")
            .terminal_adapter,
        None
    );

    legacy.terminal_provenance = Some(TerminalProvenanceKind::Cmux);
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(legacy),
        vec!["windows@app=9".to_owned()],
    )
    .expect("compatible cmux enrichment");
    let handoff = ledger.terminal_handoffs.values().next().expect("handoff");
    assert_eq!(
        handoff.owner_terminal_provenance,
        Some(TerminalProvenanceKind::Cmux)
    );
    let resume = ledger.resume_records.values().next().expect("resume");
    assert_eq!(resume.terminal_adapter, None);
    assert!(!resume.dispatch_enabled);

    let mut drifted = owner("different-route");
    drifted.provider = Some("future-provider".to_owned());
    drifted.resume_transport = Some("future-native".to_owned());
    drifted.terminal_provenance = Some(TerminalProvenanceKind::Cmux);
    assert!(
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            Some(drifted),
            vec!["windows@app=9".to_owned()],
        )
        .expect_err("route drift remains fenced")
        .message()
        .contains("identity changed")
    );
}

#[test]
fn legacy_handoff_accepts_only_fenced_provider_route_enrichment() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
        vec!["windows@app=9".to_owned()],
    )
    .expect("legacy handoff");

    let mut mismatched = owner("route-a");
    mismatched.provider_route = Some(subrouter_provider_route(2));
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(mismatched),
        vec!["windows@app=9".to_owned()],
    )
    .expect_err("provider generation cannot exceed the unchanged owner generation");
    assert!(
        ledger
            .terminal_handoffs
            .values()
            .all(|handoff| handoff.provider_route.is_none())
    );

    let mut enriched = owner("route-a");
    enriched.provider_route = Some(subrouter_provider_route(1));
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(enriched),
        vec!["windows@app=9".to_owned()],
    )
    .expect("validated provider route enriches the legacy handoff");
    let restarted = load_ledger(&path).expect("restart enriched ledger");
    assert!(
        restarted
            .terminal_handoffs
            .values()
            .all(|handoff| handoff.provider_route.is_some())
    );
    assert!(matches!(
        restarted
            .resume_records
            .values()
            .next()
            .expect("resume")
            .provider_adapter,
        Some(ProviderAdapterV1::LaunchProfile {
            ref provider,
            generation: 1,
            ..
        }) if provider == "subrouter"
    ));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the monotonic route lifecycle is clearer as one ordered scenario"
)]
fn route_resolution_and_owner_transfer_are_monotonic() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        None,
        vec!["macos@app=42".to_owned()],
    )
    .expect("unresolved route");
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(fresh_agent_owner()),
        vec!["macos@app=42".to_owned()],
    )
    .expect("resolve known route-less fallback");
    let mut generation_two = owner("route-generation-2");
    generation_two.owner_id = "replacement-owner".to_owned();
    generation_two.ownership_generation = 2;
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(generation_two),
        vec!["macos@app=42".to_owned()],
    )
    .expect("transfer route");
    let record = ledger.terminal_handoffs.values().next().expect("record");
    assert_eq!(record.owner_id.as_deref(), Some("replacement-owner"));
    assert_eq!(record.ownership_generation, Some(2));
    assert_eq!(record.owner_route_id.as_deref(), Some("route-generation-2"));

    let error = persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-generation-1")),
        vec!["macos@app=42".to_owned()],
    )
    .expect_err("stale generation");
    assert!(error.message().contains("identity changed"));

    let mut unroutable = owner("discarded-route");
    unroutable.owner_disposition = "unroutable_private_route".to_owned();
    unroutable.route_id = None;
    unroutable.provider = None;
    unroutable.resume_transport = None;
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        8,
        HEAD,
        Some(unroutable.clone()),
        vec!["windows@app=9".to_owned()],
    )
    .expect("unroutable snapshot");
    unroutable.owner_id = "tampered-owner".to_owned();
    let error = persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        8,
        HEAD,
        Some(unroutable),
        vec!["windows@app=9".to_owned()],
    )
    .expect_err("unroutable replay cannot rewrite owner");
    assert!(error.message().contains("identity changed"));
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        8,
        HEAD,
        Some(owner("validated-route")),
        vec!["windows@app=9".to_owned()],
    )
    .expect("validated route resolves unroutable snapshot");

    let mut trusted = owner("trusted-route");
    trusted.provider_route = Some(subrouter_provider_route(1));
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        9,
        HEAD,
        Some(trusted),
        vec!["linux@app=3".to_owned()],
    )
    .expect("trusted route");
    let mut degraded = owner("discarded-route");
    degraded.owner_id = "untrusted-fallback-owner".to_owned();
    degraded.owner_disposition = "unroutable_private_route".to_owned();
    degraded.route_id = None;
    degraded.provider = None;
    degraded.resume_transport = None;
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        9,
        HEAD,
        Some(degraded),
        vec!["linux@app=3".to_owned()],
    )
    .expect("route degradation preserves obligation");
    let degraded_record = ledger
        .terminal_handoffs
        .values()
        .find(|record| record.pr_number == 9)
        .expect("degraded record");
    assert_eq!(degraded_record.owner_id.as_deref(), Some("owner-exact"));
    assert_eq!(
        degraded_record.owner_disposition,
        "unroutable_private_route"
    );
    assert_eq!(degraded_record.owner_route_id, None);
    assert_eq!(
        degraded_record.provider_route,
        Some(subrouter_provider_route(1))
    );
    let degraded_resume = ledger
        .resume_records
        .values()
        .find(|record| record.pr_number == 9)
        .expect("degraded resume record");
    assert!(matches!(
        degraded_resume.provider_adapter,
        Some(ProviderAdapterV1::LaunchProfile {
            ref provider,
            generation: 1,
            ..
        }) if provider == "subrouter"
    ));
}

#[test]
fn base_scoped_observation_resolves_only_proven_head_supersession() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    persist_success_continuation(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
    )
    .expect("success");
    persist_success_continuation(
        &path,
        &mut ledger,
        "owner/repo",
        "MAIN",
        7,
        HEAD,
        Some(owner("route-case-distinct")),
    )
    .expect("case-distinct base");
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        8,
        HEAD,
        Some(owner("route-b")),
        vec!["macos@app=42".to_owned()],
    )
    .expect("failure");
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "release",
        9,
        HEAD,
        Some(owner("route-release")),
        vec!["windows@app=9".to_owned()],
    )
    .expect("other base failure");
    let current_heads =
        BTreeMap::from([(7, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned())]);
    resolve_superseded_terminal_handoffs(&path, &mut ledger, "owner/repo", "main", &current_heads)
        .expect("resolve stale");
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .find(|record| record.pr_number == 7 && record.base == "main")
            .expect("superseded head")
            .phase,
        TerminalHandoffPhase::Resolved
    );
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .find(|record| record.pr_number == 7 && record.base == "MAIN")
            .expect("case-distinct base")
            .phase,
        TerminalHandoffPhase::Pending
    );
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .find(|record| record.pr_number == 8)
            .expect("closed main PR")
            .phase,
        TerminalHandoffPhase::Resolved
    );
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .find(|record| record.pr_number == 9)
            .expect("other base")
            .phase,
        TerminalHandoffPhase::Recorded
    );
}

#[test]
fn deterministic_convergence_resolves_recorded_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
        vec!["macos@app=42".to_owned()],
    )
    .expect("failure");
    persist_success_continuation(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
    )
    .expect("pending success");
    resolve_terminal_handoffs(&path, &mut ledger, "owner/repo", "main", 7, HEAD).expect("resolve");
    assert!(
        ledger
            .terminal_handoffs
            .values()
            .all(|record| record.phase == TerminalHandoffPhase::Resolved)
    );
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
        vec!["macos@app=42".to_owned()],
    )
    .expect("recur");
    let restarted = load_ledger(&path).expect("restart");
    assert_eq!(
        restarted
            .terminal_handoffs
            .values()
            .find(|record| record.outcome == TerminalHandoffOutcome::ActionableFailure)
            .expect("record")
            .phase,
        TerminalHandoffPhase::Recorded
    );
}

#[test]
fn same_head_failure_supersedes_ambiguous_pending_success() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    persist_success_continuation(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
    )
    .expect("durable intent precedes ambiguous enqueue response");
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .next()
            .expect("pending continuation")
            .phase,
        TerminalHandoffPhase::Pending
    );

    let blocked_parent = temp.path().join("not-a-directory");
    std::fs::write(&blocked_parent, "occupied").expect("blocked parent");
    persist_actionable_failure(
        &blocked_parent.join("ledger.json"),
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
        vec!["macos@app=42".to_owned()],
    )
    .expect_err("failed publication rolls back both outcome changes");
    assert_eq!(ledger.terminal_handoffs.len(), 1);
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .next()
            .expect("original continuation")
            .phase,
        TerminalHandoffPhase::Pending
    );

    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
        vec!["macos@app=42".to_owned()],
    )
    .expect("same-head required failure");

    let restarted = load_ledger(&path).expect("restart");
    let unresolved = restarted
        .terminal_handoffs
        .values()
        .filter(|record| {
            !matches!(
                record.phase,
                TerminalHandoffPhase::Applied | TerminalHandoffPhase::Resolved
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(unresolved.len(), 1);
    assert_eq!(
        unresolved[0].outcome,
        TerminalHandoffOutcome::ActionableFailure
    );
    assert_eq!(unresolved[0].phase, TerminalHandoffPhase::Recorded);
    assert!(
        restarted.terminal_handoffs.values().any(|record| {
            record.outcome == TerminalHandoffOutcome::SuccessContinuation
                && record.phase == TerminalHandoffPhase::Resolved
        }),
        "the ambiguous success intent remains as resolved audit history"
    );
}

#[test]
fn ambiguous_save_distinguishes_absent_target_from_published_empty_ledger() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source.json");
    let mut ledger = StewardLedger::default();
    persist_actionable_failure(
        &source,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
        vec!["windows@app=9".to_owned()],
    )
    .expect("fallback obligation");
    let fallback = ledger.terminal_handoffs.clone();
    let fallback_resume_records = ledger.resume_records.clone();

    ledger.terminal_handoffs.clear();
    ledger.resume_records.clear();
    reconcile_after_ambiguous_save(
        &temp.path().join("absent.json"),
        &mut ledger,
        fallback.clone(),
        fallback_resume_records.clone(),
    );
    assert_eq!(ledger.terminal_handoffs, fallback);
    assert_eq!(ledger.resume_records, fallback_resume_records);

    let published_empty = temp.path().join("published-empty.json");
    save_ledger(&published_empty, &StewardLedger::default()).expect("published empty ledger");
    reconcile_after_ambiguous_save(
        &published_empty,
        &mut ledger,
        fallback,
        fallback_resume_records,
    );
    assert!(ledger.terminal_handoffs.is_empty());
    assert!(ledger.resume_records.is_empty());
}

#[test]
fn typed_terminal_provenance_is_durable_but_never_enables_wake() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    let mut herdr_owner = owner("herdr-route");
    herdr_owner.terminal_provenance = Some(TerminalProvenanceKind::HerdR);
    herdr_owner.provider_route = Some(subrouter_provider_route(1));
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(herdr_owner),
        vec!["macos@app=42".to_owned()],
    )
    .expect("typed wake intent");

    let restarted = load_ledger(&path).expect("restart");
    let record = restarted
        .terminal_handoffs
        .values()
        .next()
        .expect("terminal handoff");
    assert_eq!(
        record.owner_terminal_provenance,
        Some(TerminalProvenanceKind::HerdR)
    );
    assert!(!record.wake_consumer_available);
    assert_eq!(record.phase, TerminalHandoffPhase::Recorded);
    let resume = restarted
        .resume_records
        .values()
        .next()
        .expect("HerdR resume");
    assert_eq!(resume.terminal_adapter, None);
    assert!(matches!(
        resume.agent_adapter,
        Some(AgentAdapterV1::Native {
            ref provider,
            ref transport,
            ref route_id,
        }) if provider == "codex" && transport == "codex_queue" && route_id == "herdr-route"
    ));
    assert!(matches!(
        resume.provider_adapter,
        Some(ProviderAdapterV1::LaunchProfile {
            ref profile_digest,
            generation: 1,
            revision: 2,
            ref provider,
            ..
        }) if profile_digest == &"a".repeat(64) && provider == "subrouter"
    ));
    assert!(!resume.dispatch_enabled);
}

#[test]
fn resume_intent_survives_restart_and_failed_replacement_as_one_ledger_image() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
        vec!["windows@app=9".to_owned()],
    )
    .expect("durable actionable handoff and resume intent");

    let mut restarted = load_ledger(&path).expect("restart ledger");
    assert_eq!(restarted.terminal_handoffs.len(), 1);
    assert_eq!(restarted.resume_records.len(), 1);
    let resume = restarted
        .resume_records
        .values()
        .next()
        .expect("resume intent");
    assert_eq!(resume.head_sha, HEAD);
    assert_eq!(resume.ownership_generation, Some(1));
    assert_eq!(resume.phase, ResumeRecordPhase::Recorded);
    assert!(!resume.dispatch_enabled);
    assert!(matches!(
        resume.agent_adapter,
        Some(AgentAdapterV1::Native {
            ref provider,
            ref transport,
            ref route_id,
        }) if provider == "codex" && transport == "codex_queue" && route_id == "route-a"
    ));

    let before_handoffs = restarted.terminal_handoffs.clone();
    let before_resumes = restarted.resume_records.clone();
    let blocked_parent = temp.path().join("not-a-directory");
    std::fs::write(&blocked_parent, "occupied").expect("block ledger parent");
    let blocked_path = blocked_parent.join("ledger.json");
    resolve_terminal_handoffs(&blocked_path, &mut restarted, "owner/repo", "main", 7, HEAD)
        .expect_err("atomic ledger publication fails");
    assert_eq!(restarted.terminal_handoffs, before_handoffs);
    assert_eq!(restarted.resume_records, before_resumes);
}

#[test]
fn queued_reconciliation_completes_uncertain_success_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    assert!(
        !reconcile_queued_success_continuation(&path, &mut ledger, "owner/repo", "main", 7, HEAD,)
            .expect("no intent")
    );
    persist_success_continuation(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
    )
    .expect("intent");
    assert!(
        reconcile_queued_success_continuation(&path, &mut ledger, "owner/repo", "main", 7, HEAD,)
            .expect("first reconciliation")
    );
    assert!(
        !reconcile_queued_success_continuation(&path, &mut ledger, "owner/repo", "main", 7, HEAD,)
            .expect("deduplicated reconciliation")
    );
    persist_success_continuation(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        HEAD,
        Some(owner("route-a")),
    )
    .expect("rearm exact-head continuation");
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .next()
            .expect("rearmed")
            .phase,
        TerminalHandoffPhase::Pending
    );
    assert!(
        reconcile_queued_success_continuation(&path, &mut ledger, "owner/repo", "main", 7, HEAD,)
            .expect("reconciled re-enqueue")
    );
}

#[test]
fn retention_discards_only_applied_records_and_fails_closed_on_pending_capacity() {
    let mut ledger = StewardLedger::default();
    for pr_number in 1..=MAX_TERMINAL_HANDOFFS as u64 {
        let dedupe_key = format!("owner/repo#{pr_number}:{HEAD}:success");
        ledger.terminal_handoffs.insert(
            dedupe_key.clone(),
            TerminalHandoff {
                dedupe_key,
                repo: "owner/repo".to_owned(),
                base: "main".to_owned(),
                pr_number,
                head_sha: HEAD.to_owned(),
                outcome: TerminalHandoffOutcome::SuccessContinuation,
                trigger: "required_checks_terminal_success".to_owned(),
                next_action: "arm_merge_queue_exact_head".to_owned(),
                origin_machine: None,
                owner_id: None,
                ownership_generation: None,
                owner_disposition: "route_registry_required".to_owned(),
                owner_route_id: None,
                owner_provider: None,
                resume_transport: None,
                owner_terminal_provenance: None,
                provider_route: None,
                wake_consumer_available: false,
                failure_contexts: Vec::new(),
                phase: TerminalHandoffPhase::Pending,
                created_at: "2026-08-27T00:00:00Z".to_owned(),
                updated_at: format!("2026-08-27T00:{:02}:00Z", pr_number % 60),
            },
        );
    }
    assert!(make_capacity_for_terminal_handoff(&mut ledger).is_err());
    ledger
        .terminal_handoffs
        .values_mut()
        .next()
        .expect("record")
        .phase = TerminalHandoffPhase::Applied;
    make_capacity_for_terminal_handoff(&mut ledger).expect("evict applied record");
    assert_eq!(ledger.terminal_handoffs.len(), MAX_TERMINAL_HANDOFFS - 1);
}
