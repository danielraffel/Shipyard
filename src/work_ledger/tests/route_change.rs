use super::*;

fn actionable() -> (TempDir, WorkLedger, RouteRegistration) {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let candidate = sample_candidate();
    let id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("import");
    ledger
        .record_continuations(
            &id,
            0,
            &ContinuationSet::new(digest(b"ok"), None, digest(b"fail"), None)
                .expect("continuations"),
        )
        .expect("record");
    for (generation, state) in [
        (1, LifecycleState::Published),
        (2, LifecycleState::Ready),
        (3, LifecycleState::Managed),
        (4, LifecycleState::Actionable),
    ] {
        ledger
            .transition_with_wake(&id, generation, 3, state, None)
            .expect("transition");
    }
    let (route, adapter) = sample_route(&id, 5);
    ledger.register_adapter(&adapter).expect("adapter");
    ledger.register_route(&route).expect("route");
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute(
            "UPDATE work_items SET repair_route_ref=?1 WHERE id=?2",
            params![route.route_ref, id],
        )
        .expect("pointer");
    drop(connection);
    (temp, ledger, route)
}

fn same_target(source: &RouteRegistration) -> RouteRegistration {
    let mut provenance = source.provenance.clone();
    provenance.terminal = TerminalRouteRecord::new(TerminalRoute::Cmux {
        workspace_ref: OpaqueRef::derive("test", b"new workspace"),
        pane_ref: OpaqueRef::derive("test", b"new pane"),
        surface_ref: OpaqueRef::derive("test", b"new surface"),
    });
    provenance.integrity_sha256 = provenance.recompute_integrity().expect("integrity");
    RouteRegistration::new(
        opaque_ref("route", "same target"),
        source.work_id.clone(),
        source.head_sha.clone(),
        source.work_generation + 1,
        source.owner_ref.clone(),
        source.owner_generation,
        source.revision + 1,
        source.origin_machine_ref.clone(),
        provenance,
    )
    .expect("target")
}

fn fresh_target(claim: &FreshOwnerTransferClaim) -> RouteRegistration {
    let source = &claim.source;
    let mut provenance = source.provenance.clone();
    if let AgentRoute::Codex { session } = &mut provenance.agent.route {
        session.native_session_ref = OpaqueRef::derive("test", b"new native session");
        session.native_resume_ref = OpaqueRef::derive("test", b"new native resume");
    }
    provenance.launch_profile.generation = claim.target_owner_generation;
    provenance.launch_profile.revision += 1;
    provenance.integrity_sha256 = provenance.recompute_integrity().expect("integrity");
    RouteRegistration::new(
        claim.target_route_ref.clone(),
        source.work_id.clone(),
        source.head_sha.clone(),
        claim.target_work_generation,
        claim.target_owner_ref.clone(),
        claim.target_owner_generation,
        source.revision + 1,
        source.origin_machine_ref.clone(),
        provenance,
    )
    .expect("fresh target")
}

#[test]
fn same_session_rebind_is_atomic_preserving_and_exactly_replayable() {
    let (_t, ledger, source) = actionable();
    let target = same_target(&source);
    let challenge = ledger
        .prepare_same_session_rebind(&source, target.route_ref.clone())
        .expect("prepare");
    assert_eq!(
        challenge,
        ledger
            .prepare_same_session_rebind(&source, target.route_ref.clone())
            .expect("prepare replay")
    );
    let receipt = VerifiedTerminalRebindReceipt::verified(
        challenge.change_id.clone(),
        target.route_ref.clone(),
        digest(b"terminal proof"),
    );
    let mut forged_challenge = challenge.clone();
    forged_challenge.source.provenance.provider = ProviderRouteRecord::new(ProviderRoute::Direct {
        endpoint_ref: OpaqueRef::derive("test", b"forged direct"),
    });
    forged_challenge
        .source
        .provenance
        .launch_profile
        .provider_kind = "direct".to_owned();
    forged_challenge.source.provenance.integrity_sha256 = forged_challenge
        .source
        .provenance
        .recompute_integrity()
        .expect("integrity");
    forged_challenge.source.envelope_integrity =
        forged_challenge.source.compute_envelope_integrity();
    assert!(
        ledger
            .apply_same_session_rebind(&forged_challenge, &receipt, &target)
            .is_err()
    );
    let applied = ledger
        .apply_same_session_rebind(&challenge, &receipt, &target)
        .expect("apply");
    assert_eq!(
        applied,
        ledger
            .apply_same_session_rebind(&challenge, &receipt, &target)
            .expect("lost response replay")
    );
    assert_eq!(target.owner_generation, source.owner_generation);
    let c = ledger.connect_read_write().expect("connection");
    assert!(c.execute("UPDATE route_changes SET state='failed',receipt_kind='definitive_not_delivered' WHERE change_id=?1",[&challenge.change_id]).is_err());
    assert_eq!(target.provenance.agent, source.provenance.agent);
    assert_eq!(target.provenance.provider, source.provenance.provider);
    assert_eq!(
        target.provenance.launch_profile,
        source.provenance.launch_profile
    );
}

#[test]
fn same_session_rebind_rejects_provider_fallback_and_double_target() {
    let (_t, ledger, source) = actionable();
    let target = same_target(&source);
    let _ = ledger
        .prepare_same_session_rebind(&source, target.route_ref.clone())
        .expect("prepare");
    assert!(
        ledger
            .prepare_same_session_rebind(&source, opaque_ref("route", "other target"))
            .is_err()
    );
    let mut direct = target.clone();
    direct.provenance.provider = ProviderRouteRecord::new(ProviderRoute::Direct {
        endpoint_ref: OpaqueRef::derive("test", b"direct"),
    });
    direct.provenance.launch_profile.provider_kind = "direct".to_owned();
    direct.provenance.integrity_sha256 =
        direct.provenance.recompute_integrity().expect("integrity");
    direct.envelope_integrity = direct.compute_envelope_integrity();
    let challenge = ledger
        .prepare_same_session_rebind(&source, target.route_ref.clone())
        .expect("replay");
    let receipt = VerifiedTerminalRebindReceipt::verified(
        challenge.change_id.clone(),
        target.route_ref.clone(),
        digest(b"terminal proof"),
    );
    assert!(
        ledger
            .apply_same_session_rebind(&challenge, &receipt, &direct)
            .is_err()
    );
}

fn fresh_started(
    ledger: &WorkLedger,
    source: &RouteRegistration,
) -> (FreshOwnerTransferClaim, StartedFreshOwnerTransfer) {
    let dead = DeadNativeSessionReceipt::verified(
        native_session(source).to_owned(),
        digest(b"native process dead"),
    );
    let claim = ledger
        .prepare_fresh_owner_transfer(
            source,
            &dead,
            &digest(b"checkpoint"),
            &opaque_ref("owner", "new owner"),
            &opaque_ref("route", "fresh target"),
        )
        .expect("prepare");
    let started = ledger
        .mark_fresh_owner_transfer_started(&claim, digest(b"adapter start"))
        .expect("start");
    assert_eq!(
        started,
        ledger
            .mark_fresh_owner_transfer_started(&claim, digest(b"adapter start"))
            .expect("start replay")
    );
    (claim, started)
}

#[test]
fn accepted_fresh_transfer_advances_owner_once_and_replays() {
    let (_t, ledger, source) = actionable();
    let (claim, started) = fresh_started(&ledger, &source);
    let target = fresh_target(&claim);
    let receipt = FreshOwnerTransferReceipt::verified(
        claim.change_id.clone(),
        FreshTransferReceiptKind::Accepted,
        digest(b"accepted"),
        claim.target_route_ref.clone(),
    );
    let first = ledger
        .reconcile_fresh_owner_transfer(&started, &receipt, Some(&target))
        .expect("ack");
    assert_eq!(
        first,
        ledger
            .reconcile_fresh_owner_transfer(&started, &receipt, Some(&target))
            .expect("replay")
    );
    assert_eq!(target.owner_generation, source.owner_generation + 1);
    let c = ledger.connect_read_write().expect("connection");
    assert!(c.execute("UPDATE route_changes SET delivery_started_at=NULL,adapter_evidence_digest=NULL,start_integrity=NULL WHERE change_id=?1",[&claim.change_id]).is_err());
}

#[test]
fn uncertain_never_retries_and_definitive_failure_has_current_recovery_route() {
    let (_t, ledger, source) = actionable();
    let (claim, started) = fresh_started(&ledger, &source);
    let uncertain = FreshOwnerTransferReceipt::verified(
        claim.change_id.clone(),
        FreshTransferReceiptKind::Uncertain,
        digest(b"unknown"),
        claim.target_route_ref.clone(),
    );
    assert!(matches!(
        ledger
            .reconcile_fresh_owner_transfer(&started, &uncertain, None)
            .expect("uncertain"),
        FreshTransferDisposition::Uncertain(_)
    ));
    assert!(
        ledger
            .mark_fresh_owner_transfer_started(&claim, digest(b"adapter start"))
            .is_err()
    );
    let drift = ledger.connect_read_write().expect("drift connection");
    drift
        .execute(
            "UPDATE work_items SET work_generation=work_generation+1 WHERE id=?1",
            [&source.work_id],
        )
        .expect("drift uncertain fence");
    drop(drift);
    assert!(
        ledger
            .reconcile_fresh_owner_transfer(&started, &uncertain, None)
            .is_err()
    );
    let (_t2, ledger2, source2) = actionable();
    let (claim2, started2) = fresh_started(&ledger2, &source2);
    let failed = FreshOwnerTransferReceipt::verified(
        claim2.change_id.clone(),
        FreshTransferReceiptKind::DefinitiveNotDelivered,
        digest(b"not delivered"),
        claim2.target_route_ref.clone(),
    );
    assert!(matches!(
        ledger2
            .reconcile_fresh_owner_transfer(&started2, &failed, None)
            .expect("failure"),
        FreshTransferDisposition::NotDelivered(_)
    ));
    let c = ledger2.connect_read_write().expect("connection");
    let tx = c.unchecked_transaction().expect("tx");
    assert!(
        validated_route_exists(
            &tx,
            &claim2.recovery_route_ref,
            &source2.work_id,
            claim2.target_work_generation,
            source2.owner_generation
        )
        .expect("route")
    );
    let recovery = load_validated_route(&tx, &claim2.recovery_route_ref)
        .expect("load recovery")
        .expect("recovery route");
    drop(tx);
    drop(c);
    assert!(
        ledger2
            .prepare_same_session_rebind(&recovery, opaque_ref("route", "forbidden same session"))
            .is_err()
    );
    let drift = ledger2.connect_read_write().expect("drift connection");
    drift
        .execute(
            "UPDATE work_items SET work_generation=work_generation+1 WHERE id=?1",
            [&source2.work_id],
        )
        .expect("consume recovery fence");
    drop(drift);
    assert!(
        ledger2
            .reconcile_fresh_owner_transfer(&started2, &failed, None)
            .is_err()
    );
}

#[test]
fn fresh_transfer_rejects_late_owner_and_generation_overflow() {
    let (_stale_temp, stale_ledger, stale_source) = actionable();
    let dead = DeadNativeSessionReceipt::verified(
        native_session(&stale_source).to_owned(),
        digest(b"dead"),
    );
    let stale_claim = stale_ledger
        .prepare_fresh_owner_transfer(
            &stale_source,
            &dead,
            &digest(b"checkpoint"),
            &opaque_ref("owner", "stale owner"),
            &opaque_ref("route", "stale target"),
        )
        .expect("prepare stale claim");
    let mut forged_claim = stale_claim.clone();
    forged_claim.source.revision += 1;
    forged_claim.source.envelope_integrity = forged_claim.source.compute_envelope_integrity();
    assert!(
        stale_ledger
            .mark_fresh_owner_transfer_started(&forged_claim, digest(b"adapter evidence"))
            .is_err()
    );
    let stale_connection = stale_ledger.connect_read_write().expect("connection");
    stale_connection
        .execute(
            "UPDATE work_items SET owner_generation=owner_generation+1 WHERE id=?1",
            [&stale_source.work_id],
        )
        .expect("drift before start");
    drop(stale_connection);
    assert!(
        stale_ledger
            .mark_fresh_owner_transfer_started(&stale_claim, digest(b"adapter evidence"))
            .is_err()
    );
    let (_t, ledger, source) = actionable();
    let (claim, started) = fresh_started(&ledger, &source);
    let target = fresh_target(&claim);
    let c = ledger.connect_read_write().expect("connection");
    c.execute(
        "UPDATE work_items SET owner_generation=owner_generation+1 WHERE id=?1",
        [&source.work_id],
    )
    .expect("late owner");
    drop(c);
    let receipt = FreshOwnerTransferReceipt::verified(
        claim.change_id.clone(),
        FreshTransferReceiptKind::Accepted,
        digest(b"accepted"),
        claim.target_route_ref.clone(),
    );
    assert!(
        ledger
            .reconcile_fresh_owner_transfer(&started, &receipt, Some(&target))
            .is_err()
    );
    let mut overflow = source;
    overflow.work_generation = u64::MAX;
    assert!(
        ledger
            .prepare_fresh_owner_transfer(
                &overflow,
                &DeadNativeSessionReceipt::verified(
                    native_session(&overflow).to_owned(),
                    digest(b"dead")
                ),
                &digest(b"checkpoint"),
                &opaque_ref("owner", "next"),
                &opaque_ref("route", "next")
            )
            .is_err()
    );
}
