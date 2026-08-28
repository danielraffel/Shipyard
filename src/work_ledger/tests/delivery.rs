use super::*;
use chrono::{Duration as ChronoDuration, TimeZone};
use std::sync::{Arc, Barrier};

fn at(second: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 28, 12, 0, second)
        .single()
        .expect("timestamp")
}

fn pending_delivery() -> (TempDir, WorkLedger, String, String, AdapterBindingRecord) {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("import");
    ledger
        .record_continuations(
            &work_id,
            0,
            &ContinuationSet::new(digest(b"success"), None, digest(b"failure"), None)
                .expect("continuations"),
        )
        .expect("record continuations");
    for (generation, state) in [
        (1, LifecycleState::Published),
        (2, LifecycleState::Ready),
        (3, LifecycleState::Managed),
        (4, LifecycleState::Actionable),
    ] {
        ledger
            .transition_with_wake(&work_id, generation, 3, state, None)
            .expect("legal transition");
    }
    let (route, adapter) = sample_route(&work_id, 5);
    ledger.register_adapter(&adapter).expect("adapter");
    ledger.register_route(&route).expect("route");
    let wake =
        WakeIntent::new(&work_id, 6, 3, route.route_ref.clone(), digest(b"payload")).expect("wake");
    let wake_id = wake.wake_id.clone();
    ledger
        .transition_with_wake(&work_id, 5, 3, LifecycleState::Dispatching, Some(&wake))
        .expect("dispatching");
    (temp, ledger, work_id, wake_id, adapter)
}

#[test]
fn exact_route_generation_precedes_wake_and_claim_by_one() {
    let (_temp, ledger, _work_id, wake_id, adapter) = pending_delivery();
    let claim = ledger
        .claim_wake(&wake_id, &opaque_ref("machine", "m3"), at(0), at(30))
        .expect("claim route N for work and wake N+1");

    assert_eq!(claim.work_generation, 6);
    assert_eq!(claim.route.terminal_kind, "cmux");
    assert_eq!(claim.route.agent_kind, "codex");
    assert_eq!(claim.route.provider_kind, "subrouter");
    assert!(matches!(claim.route.terminal, TerminalRoute::Cmux { .. }));
    assert!(matches!(claim.route.agent.route, AgentRoute::Codex { .. }));
    assert_eq!(claim.route.agent.adapter, adapter);
    assert!(matches!(
        claim.route.provider,
        ProviderRoute::Subrouter { .. }
    ));
    assert_eq!(claim.route.launch_generation, claim.owner_generation);
    assert!(claim.route.native_resume_ref.starts_with("opaque:sha256:"));
    assert!(claim.route.account_ref.starts_with("opaque:sha256:"));
    assert!(claim.route.model_ref.starts_with("opaque:sha256:"));
}

#[test]
fn generic_agent_ownership_bypass_is_refused() {
    let (_temp, ledger, work_id, _wake_id, _adapter) = pending_delivery();

    let error = ledger
        .transition_with_wake(&work_id, 6, 3, LifecycleState::AgentOwnedRepair, None)
        .expect_err("typed accepted receipt is required");

    assert!(matches!(error, WorkLedgerError::Refused(_)));
    let connection = ledger.connect_read_only().expect("connection");
    assert_eq!(
        connection
            .query_row(
                "SELECT phase FROM work_items WHERE id = ?1",
                [&work_id],
                |row| row.get::<_, String>(0),
            )
            .expect("phase"),
        "dispatching"
    );
    assert_eq!(
        count_where(&connection, "outbox", "state", "pending").expect("pending wake"),
        1
    );
}

#[test]
fn generic_terminal_transition_cannot_strand_an_active_wake() {
    let (_temp, ledger, work_id, _wake_id, _adapter) = pending_delivery();

    let error = ledger
        .transition_with_wake(&work_id, 6, 3, LifecycleState::Terminal, None)
        .expect_err("active delivery requires a typed outcome");

    assert!(matches!(error, WorkLedgerError::Refused(_)));
    let connection = ledger.connect_read_only().expect("connection");
    assert_eq!(
        connection
            .query_row(
                "SELECT phase FROM work_items WHERE id = ?1",
                [&work_id],
                |row| row.get::<_, String>(0),
            )
            .expect("phase"),
        "dispatching"
    );
    assert_eq!(
        count_where(&connection, "outbox", "state", "pending").expect("pending wake"),
        1
    );
}

#[test]
fn concurrent_claim_is_singleton() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for machine in ["m1", "m5"] {
        let ledger = ledger.clone();
        let wake_id = wake_id.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            ledger.claim_wake(&wake_id, &opaque_ref("machine", machine), at(0), at(30))
        }));
    }
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    let connection = ledger.connect_read_only().expect("connection");
    assert_eq!(
        count_where(&connection, "outbox", "state", "claimed").expect("claimed"),
        1
    );
}

#[test]
fn expired_unstarted_claim_requeues_with_monotonic_attempt() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let first = ledger
        .claim_wake(&wake_id, &opaque_ref("machine", "m1"), at(0), at(10))
        .expect("first claim");
    assert_eq!(
        ledger
            .reconcile_expired_claim(&wake_id, at(11), &digest(b"restart"))
            .expect("requeue"),
        ExpiredClaimDisposition::RequeuedUnstarted
    );
    let second = ledger
        .claim_wake(&wake_id, &opaque_ref("machine", "m5"), at(12), at(22))
        .expect("second claim");
    assert_eq!(first.claim_attempt, 1);
    assert_eq!(second.claim_attempt, 2);
    assert_ne!(first.claim_id, second.claim_id);
    assert_ne!(first.identity_digest, second.identity_digest);
}

#[test]
fn started_expiry_is_uncertain_and_never_requeued() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = ledger
        .claim_wake(&wake_id, &opaque_ref("machine", "m3"), at(0), at(10))
        .expect("claim");
    ledger
        .mark_delivery_started(&claim, at(1))
        .expect("started boundary");
    assert_eq!(
        ledger
            .reconcile_expired_claim(&wake_id, at(11), &digest(b"restart ambiguity"))
            .expect("uncertain"),
        ExpiredClaimDisposition::MarkedUncertain
    );
    assert!(
        ledger
            .claim_wake(&wake_id, &opaque_ref("machine", "m1"), at(12), at(22),)
            .is_err()
    );
    let connection = ledger.connect_read_only().expect("connection");
    let row: (String, String, String) = connection
        .query_row(
            "SELECT state, receipt_kind, receipt_digest FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("uncertain row");
    assert_eq!(row.0, "uncertain");
    assert_eq!(row.1, "uncertain");
    validate_digest("receipt", &row.2).expect("opaque receipt digest");
}

#[test]
fn uncertain_delivery_acceptance_transfers_ownership_without_retry() {
    let (temp, ledger, work_id, wake_id, _adapter) = pending_delivery();
    let claim = ledger
        .claim_wake(&wake_id, &opaque_ref("machine", "m3"), at(0), at(10))
        .expect("claim");
    ledger
        .mark_delivery_started(&claim, at(1))
        .expect("started boundary");
    ledger
        .reconcile_expired_claim(&wake_id, at(11), &digest(b"restart ambiguity"))
        .expect("uncertain");
    drop(claim);
    drop(ledger);
    let ledger = WorkLedger::open(temp.path()).expect("restart ledger");
    let claim = ledger
        .recover_uncertain_claim(&wake_id)
        .expect("recover exact durable claim");
    let receipt = DeliveryReceipt::accepted_after_uncertainty(
        &claim,
        claim.route.native_session_ref.clone(),
        digest(b"late exact acceptance"),
    )
    .expect("accepted receipt");

    ledger
        .reconcile_uncertain_delivery(&claim, &receipt, at(12))
        .expect("resolve accepted");

    let connection = ledger.connect_read_only().expect("connection");
    let row: (String, String) = connection
        .query_row(
            "SELECT state, receipt_kind FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("resolved wake");
    assert_eq!(row, ("acknowledged".to_owned(), "accepted".to_owned()));
    let phase: String = connection
        .query_row(
            "SELECT phase FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("phase");
    assert_eq!(phase, "agent_owned_repair");
}

#[test]
fn uncertain_non_delivery_returns_actionable_but_never_pending() {
    let (_temp, ledger, work_id, wake_id, _adapter) = pending_delivery();
    let claim = ledger
        .claim_wake(&wake_id, &opaque_ref("machine", "m3"), at(0), at(10))
        .expect("claim");
    ledger
        .mark_delivery_started(&claim, at(1))
        .expect("started boundary");
    ledger
        .reconcile_expired_claim(&wake_id, at(11), &digest(b"restart ambiguity"))
        .expect("uncertain");
    let receipt = DeliveryReceipt::not_delivered_after_uncertainty(
        &claim,
        &digest(b"provider proves no delivery"),
    )
    .expect("not-delivered receipt");

    ledger
        .reconcile_uncertain_delivery(&claim, &receipt, at(12))
        .expect("resolve not delivered");

    let connection = ledger.connect_read_only().expect("connection");
    let row: (String, String) = connection
        .query_row(
            "SELECT state, receipt_kind FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("resolved wake");
    assert_eq!(
        row,
        ("failed".to_owned(), "reconciled_not_delivered".to_owned())
    );
    assert_eq!(
        count_where(&connection, "outbox", "state", "pending").expect("pending"),
        0
    );
    let phase: String = connection
        .query_row(
            "SELECT phase FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("phase");
    assert_eq!(phase, "actionable");
}

#[test]
fn exact_receipt_acknowledges_and_transfers_repair_ownership_atomically() {
    let (_temp, ledger, work_id, wake_id, adapter) = pending_delivery();
    let claim = ledger
        .claim_wake(&wake_id, &opaque_ref("machine", "m3"), at(0), at(30))
        .expect("claim");
    let started = ledger
        .mark_delivery_started(&claim, at(1))
        .expect("started");
    assert!(
        DeliveryReceipt::new(
            &started,
            opaque_ref("session", "different session"),
            digest(b"wrong adapter receipt"),
        )
        .is_err(),
        "accepted receipt must prove the exact claimed native session"
    );
    let receipt = DeliveryReceipt::new(
        &started,
        started.claim.route.native_session_ref.clone(),
        digest(b"adapter receipt"),
    )
    .expect("receipt");
    let mut wrong_started = started.clone();
    wrong_started.claim.claim_attempt += 1;
    assert!(
        ledger
            .acknowledge_delivery(&wrong_started, &receipt, at(2))
            .is_err()
    );
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute(
            "UPDATE adapter_registry SET state = 'retired' WHERE registry_ref = ?1",
            [adapter.registry_ref.as_str()],
        )
        .expect("retire after accepted delivery");
    drop(connection);
    ledger
        .acknowledge_delivery(&started, &receipt, at(2))
        .expect("acknowledge exact receipt");
    let connection = ledger.connect_read_only().expect("connection");
    let work: (String, u64) = connection
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("work");
    assert_eq!(work, ("agent_owned_repair".to_owned(), 7));
    let outbox: (String, String) = connection
        .query_row(
            "SELECT state, receipt_kind FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("outbox");
    assert_eq!(outbox, ("acknowledged".to_owned(), "accepted".to_owned()));
}

#[test]
fn definitive_pre_delivery_failure_returns_to_actionable_with_receipt() {
    let (_temp, ledger, work_id, wake_id, _adapter) = pending_delivery();
    let claim = ledger
        .claim_wake(&wake_id, &opaque_ref("machine", "m3"), at(0), at(30))
        .expect("claim");
    assert!(
        ledger
            .transition_with_wake(&work_id, 6, 3, LifecycleState::Actionable, None)
            .is_err(),
        "generic lifecycle mutation must not bypass the definitive receipt"
    );
    ledger
        .fail_unstarted_claim(&claim, &digest(b"quota unavailable"), at(1))
        .expect("definitive failure");
    let connection = ledger.connect_read_only().expect("connection");
    let work: (String, u64) = connection
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("work");
    assert_eq!(work, ("actionable".to_owned(), 7));
    let outbox: (String, String, String) = connection
        .query_row(
            "SELECT state, receipt_kind, receipt_digest FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("outbox");
    assert_eq!(outbox.0, "failed");
    assert_eq!(outbox.1, "definitive_pre_delivery_failure");
    validate_digest("receipt", &outbox.2).expect("receipt digest");
}

#[test]
fn adapter_drift_after_claim_refuses_delivery_start() {
    let (_temp, ledger, _work_id, wake_id, adapter) = pending_delivery();
    let claim = ledger
        .claim_wake(&wake_id, &opaque_ref("machine", "m3"), at(0), at(30))
        .expect("claim");
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute(
            "UPDATE adapter_registry SET implementation_digest = ?1
             WHERE registry_ref = ?2",
            params![
                digest(b"drifted implementation"),
                adapter.registry_ref.as_str()
            ],
        )
        .expect("drift adapter");
    drop(connection);
    assert!(ledger.mark_delivery_started(&claim, at(1)).is_err());
    let connection = ledger.connect_read_only().expect("connection");
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("state");
    assert_eq!(state, "claimed");
}

#[test]
fn acknowledgment_event_failure_rolls_back_receipt_and_work_transition() {
    let (_temp, ledger, work_id, wake_id, _adapter) = pending_delivery();
    let claim = ledger
        .claim_wake(&wake_id, &opaque_ref("machine", "m3"), at(0), at(30))
        .expect("claim");
    let started = ledger.mark_delivery_started(&claim, at(1)).expect("start");
    let receipt = DeliveryReceipt::new(
        &started,
        started.claim.route.native_session_ref.clone(),
        digest(b"transport evidence"),
    )
    .expect("receipt");
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_wake_ack_event
             BEFORE INSERT ON events WHEN NEW.kind = 'wake_acknowledged'
             BEGIN SELECT RAISE(ABORT, 'event failure'); END;",
        )
        .expect("trigger");
    drop(connection);
    assert!(
        ledger
            .acknowledge_delivery(&started, &receipt, at(2))
            .is_err()
    );
    let connection = ledger.connect_read_only().expect("connection");
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("outbox state");
    assert_eq!(state, "delivery_started");
    let work: (String, u64) = connection
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("work");
    assert_eq!(work, ("dispatching".to_owned(), 6));
}

#[test]
fn outbox_shape_constraints_reject_untyped_delivery_state() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let connection = ledger.connect_read_write().expect("connection");
    assert!(
        connection
            .execute(
                "UPDATE outbox SET state = 'delivery_started' WHERE wake_id = ?1",
                [&wake_id],
            )
            .is_err()
    );
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("state");
    assert_eq!(state, "pending");
}

fn assert_terminal_receipt_kind_is_required(ledger: &WorkLedger, wake_id: &str) {
    let connection = ledger.connect_read_write().expect("connection");
    for (state, delivery_started_at) in [
        ("acknowledged", Some("2026-08-28T12:00:01Z")),
        ("uncertain", Some("2026-08-28T12:00:01Z")),
        ("failed", None),
        ("failed", Some("2026-08-28T12:00:01Z")),
    ] {
        let result = connection.execute(
            "UPDATE outbox SET state = ?1,
                    claim_id = 'claim', claimant_ref = 'opaque:sha256:claimant',
                    claim_attempt = 1, claim_identity_digest = 'identity',
                    claim_payload_json = x'7b7d',
                    claimed_at = '2026-08-28T12:00:00Z',
                    lease_expires_at = '2026-08-28T12:00:30Z',
                    delivery_started_at = ?2, receipt_kind = NULL,
                    receipt_digest = 'receipt',
                    completed_at = '2026-08-28T12:00:02Z'
              WHERE wake_id = ?3",
            params![state, delivery_started_at, wake_id],
        );
        assert!(result.is_err(), "{state} must reject a NULL receipt kind");
    }
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [wake_id],
            |row| row.get(0),
        )
        .expect("state after rejected terminal writes");
    assert_eq!(state, "pending");
}

#[test]
fn fresh_v3_outbox_rejects_null_receipt_kind_for_every_terminal_state() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    assert_terminal_receipt_kind_is_required(&ledger, &wake_id);
}

fn install_exact_v2_outbox(ledger: &WorkLedger) {
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch(
            "DROP INDEX outbox_delivery;
             ALTER TABLE outbox RENAME TO outbox_v3;
             CREATE TABLE outbox (
               wake_id TEXT PRIMARY KEY,
               work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
               work_generation INTEGER NOT NULL,
               owner_generation INTEGER NOT NULL,
               state TEXT NOT NULL CHECK(state IN ('pending', 'claimed', 'acknowledged', 'uncertain', 'failed')),
               route_ref TEXT NOT NULL,
               payload_digest TEXT NOT NULL,
               transport_receipt_digest TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               acknowledged_at TEXT
             );
             INSERT INTO outbox
               (wake_id, work_item_id, work_generation, owner_generation, state,
                route_ref, payload_digest, created_at, updated_at)
             SELECT wake_id, work_item_id, work_generation, owner_generation, state,
                    route_ref, payload_digest, created_at, updated_at FROM outbox_v3;
             DROP TABLE outbox_v3;
             CREATE INDEX outbox_delivery ON outbox(state, created_at, wake_id);
             PRAGMA user_version = 2;",
        )
        .expect("exact v2 outbox");
}

#[test]
fn v2_pending_outbox_migrates_to_v3_without_losing_wake() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    install_exact_v2_outbox(&ledger);
    drop(ledger);
    let migrated = WorkLedger::open(temp.path()).expect("migrate v2");
    let connection = migrated.connect_read_only().expect("connection");
    assert_eq!(schema_version(&connection).expect("version"), 3);
    let row: (String, u64, Option<String>) = connection
        .query_row(
            "SELECT state, claim_attempt, claim_id FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("preserved wake");
    assert_eq!(row, ("pending".to_owned(), 0, None));
    assert_terminal_receipt_kind_is_required(&migrated, &wake_id);
}

#[test]
fn full_v1_pending_outbox_migrates_to_v3_with_terminal_receipt_constraints() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    install_exact_v2_outbox(&ledger);
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute("DELETE FROM route_records", [])
        .expect("remove route records from the exact positive v1 fixture");
    drop(connection);
    super::persistence::install_exact_v1_registry_schema(&ledger, &[]);
    drop(ledger);

    let migrated = WorkLedger::open(temp.path()).expect("migrate full v1 ledger");
    let connection = migrated.connect_read_only().expect("connection");
    assert_eq!(schema_version(&connection).expect("version"), 3);
    let row: (String, u64, Option<String>) = connection
        .query_row(
            "SELECT state, claim_attempt, claim_id FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("preserved v1 wake");
    assert_eq!(row, ("pending".to_owned(), 0, None));
    drop(connection);
    assert_terminal_receipt_kind_is_required(&migrated, &wake_id);
}

#[test]
fn v2_nonpending_outbox_refuses_migration_without_mutation() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    install_exact_v2_outbox(&ledger);
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute(
            "UPDATE outbox SET state = 'uncertain' WHERE wake_id = ?1",
            [&wake_id],
        )
        .expect("legacy nonpending row");
    drop(connection);
    drop(ledger);
    assert!(matches!(
        WorkLedger::open(temp.path()),
        Err(WorkLedgerError::Refused(reason))
            if reason.contains("explicit outbox reconciliation")
    ));
    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect v2");
    assert_eq!(schema_version(&connection).expect("version"), 2);
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("preserved state");
    assert_eq!(state, "uncertain");
}

#[test]
fn failed_v2_outbox_rebuild_rolls_back_schema_and_pending_wake() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    install_exact_v2_outbox(&ledger);
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch("CREATE TABLE outbox_v2 (collision TEXT);")
        .expect("migration collision");
    drop(connection);
    drop(ledger);
    assert!(WorkLedger::open(temp.path()).is_err());
    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect v2");
    assert_eq!(schema_version(&connection).expect("version"), 2);
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("preserved wake");
    assert_eq!(state, "pending");
}

#[test]
fn failed_second_stage_v1_to_v3_upgrade_rolls_back_registry_and_version() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("current ledger");
    install_exact_v2_outbox(&ledger);
    let terminal_adapter = adapter_binding(AdapterAxis::Terminal, "wezterm", "wezterm");
    super::persistence::install_exact_v1_registry_schema(&ledger, &[(&terminal_adapter, "active")]);
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch("CREATE TABLE outbox_v2 (collision TEXT);")
        .expect("second-stage collision");
    drop(connection);
    drop(ledger);

    assert!(WorkLedger::open(temp.path()).is_err());
    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect v1");
    assert_eq!(schema_version(&connection).expect("version"), 1);
    let registry_sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'adapter_registry'",
            [],
            |row| row.get(0),
        )
        .expect("registry schema");
    assert!(registry_sql.contains("'terminal', 'provider'"));
    assert!(!registry_sql.contains("'agent'"));
    let rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM adapter_registry", [], |row| {
            row.get(0)
        })
        .expect("preserved registry");
    assert_eq!(rows, 1);
}

#[test]
fn delivery_records_only_opaque_identity_and_uses_no_external_callback() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = ledger
        .claim_wake(
            &wake_id,
            &opaque_ref("machine", "raw-machine-secret"),
            at(0),
            at(30),
        )
        .expect("pure ledger claim");
    let encoded = serde_json::to_string(&claim).expect("claim JSON");
    for forbidden in [
        "raw-machine-secret",
        "secret-account",
        "resume-private-id",
        "owner-private-id",
    ] {
        assert!(!encoded.contains(forbidden), "claim leaked {forbidden}");
        for suffix in ["", "-wal", "-shm"] {
            let path = std::path::PathBuf::from(format!("{}{}", ledger.path().display(), suffix));
            if path.exists() {
                let bytes = fs::read(path).expect("ledger bytes");
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(forbidden),
                    "ledger leaked {forbidden}"
                );
            }
        }
    }
}

#[test]
fn claim_lease_is_bounded() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    assert!(
        ledger
            .claim_wake(
                &wake_id,
                &opaque_ref("machine", "m3"),
                at(0),
                at(0) + ChronoDuration::minutes(6),
            )
            .is_err()
    );
}
