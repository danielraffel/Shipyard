use super::*;
use chrono::{Duration as ChronoDuration, Utc};

struct TestAuthenticator;

impl CustodyTransportAuthenticator for TestAuthenticator {
    fn authenticate(
        &mut self,
        peer_machine_ref: &str,
        payload_digest: &str,
    ) -> WorkLedgerResult<String> {
        Ok(digest(
            format!("test-mtls\n{peer_machine_ref}\n{payload_digest}").as_bytes(),
        ))
    }
}

struct Fixture {
    temp: TempDir,
    ledger: WorkLedger,
    envelope: CustodyEnvelope,
}

fn source_fixture(label: &str) -> Fixture {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    ledger.import(&[sample_candidate()]).expect("import work");
    let connection = ledger.connect_read_only().expect("connection");
    let (work_id, work_generation, owner_generation, work_authority_digest): (
        String,
        i64,
        i64,
        String,
    ) = connection
        .query_row(
            "SELECT id, work_generation, owner_generation, source_digest
               FROM work_items LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("work identity");
    drop(connection);
    let wake_id = opaque_ref("wake", label);
    let content_digest = digest(b"cross-machine-content");
    let connection = ledger.connect_read_write().expect("write connection");
    let now = Utc::now().to_rfc3339();
    connection
        .execute(
            "INSERT INTO outbox
             (wake_id, work_item_id, work_generation, owner_generation, state,
              route_ref, payload_digest, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?7)",
            params![
                wake_id,
                work_id,
                work_generation,
                owner_generation,
                opaque_ref("route", "source route"),
                content_digest,
                now
            ],
        )
        .expect("source wake");
    drop(connection);
    let envelope = CustodyEnvelope::new(
        wake_id,
        work_id,
        u64::try_from(work_generation).expect("positive work generation"),
        u64::try_from(owner_generation).expect("positive owner generation"),
        content_digest,
        work_authority_digest,
        "GEN-14".to_owned(),
        11,
        opaque_ref("machine", "m3"),
        opaque_ref("incarnation", "m3 daemon one"),
        CustodyRelation::wake(),
    )
    .expect("envelope");
    Fixture {
        temp,
        ledger,
        envelope,
    }
}

fn stage(fixture: &Fixture) {
    fixture
        .ledger
        .stage_cross_machine_custody(
            &fixture.envelope,
            &opaque_ref("machine", "m5"),
            &opaque_ref("incarnation", "m5 daemon one"),
            &opaque_ref("route", "m5 terminal route"),
            "herdr",
            &digest(b"authenticated rebind authority"),
        )
        .expect("stage custody");
}

struct DeliveredFixture {
    source: Fixture,
    receiver_temp: TempDir,
    receiver: WorkLedger,
    transfer: CustodyTransfer,
}

fn delivered_fixture(label: &str) -> DeliveredFixture {
    let source = source_fixture(label);
    stage(&source);
    let claim = source
        .ledger
        .claim_custody_send(
            &source.envelope.message_id,
            &opaque_ref("owner", "sender"),
            Utc::now() + ChronoDuration::seconds(30),
        )
        .expect("sender claim");
    let transfer = source.ledger.custody_transfer(&claim).expect("transfer");
    let receiver_temp = TempDir::new().expect("receiver temp");
    let receiver = WorkLedger::open(receiver_temp.path()).expect("receiver ledger");
    let receipt = receiver
        .accept_custody(
            &authenticate_custody_transfer(&mut TestAuthenticator, transfer.clone())
                .expect("authenticate transfer"),
            &transfer.target_machine_ref,
            &transfer.target_incarnation_ref,
        )
        .expect("destination commit");
    source
        .ledger
        .acknowledge_remote_custody(
            &claim,
            &authenticate_custody_receipt(
                &mut TestAuthenticator,
                &transfer.target_machine_ref,
                receipt,
            )
            .expect("authenticate custody receipt"),
        )
        .expect("source acknowledges destination custody");
    DeliveredFixture {
        source,
        receiver_temp,
        receiver,
        transfer,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn gate_0b_3_accepted_custody_uses_two_phase_authenticated_successor_commit() {
    let delivered = delivered_fixture("successor recovery");
    let new_incarnation = opaque_ref("incarnation", "m5 daemon successor");
    let new_authority = digest(b"successor mutation authority");
    let successor_proof = digest(b"authenticated successor launch proof");
    let rebind = delivered
        .source
        .ledger
        .prepare_custody_successor_rebind_for_test(
            &delivered.source.envelope.message_id,
            &delivered.transfer.target_incarnation_ref,
            &new_incarnation,
            &opaque_ref("route", "m5 successor route"),
            "herdr",
            &new_authority,
            &successor_proof,
            // This is a positive crash/reopen fixture, not an expiry-boundary
            // test. Full Windows CI can spend more than a second between the
            // durable prepare and authenticated receiver acceptance.
            Utc::now() + ChronoDuration::seconds(30),
        )
        .expect("durably prepare successor");
    drop(delivered.source.ledger);
    let source = WorkLedger::open_existing(delivered.source.temp.path())
        .expect("source reopen")
        .expect("source ledger persists prepared successor");
    let authenticated = authenticate_custody_successor_rebind(
        &mut TestAuthenticator,
        &delivered.source.envelope.source_machine_ref,
        rebind.clone(),
    )
    .expect("authenticate successor request");
    let successor_receipt = delivered
        .receiver
        .accept_custody_successor_rebind(
            &authenticated,
            &delivered.transfer.target_machine_ref,
            &new_incarnation,
            &rebind.new_target_route_ref,
            &rebind.terminal_adapter,
            &rebind.new_authority_digest,
        )
        .expect("destination prepares successor custody");
    assert_eq!(
        delivered
            .receiver
            .accept_custody_successor_rebind(
                &authenticated,
                &delivered.transfer.target_machine_ref,
                &new_incarnation,
                &rebind.new_target_route_ref,
                &rebind.terminal_adapter,
                &rebind.new_authority_digest,
            )
            .expect("lost successor receipt replay"),
        successor_receipt
    );
    drop(delivered.receiver);
    let receiver = WorkLedger::open_existing(delivered.receiver_temp.path())
        .expect("receiver reopen")
        .expect("successor commit survives restart");
    source
        .acknowledge_custody_successor_rebind_for_test(
            &authenticate_custody_successor_receipt(
                &mut TestAuthenticator,
                &delivered.transfer.target_machine_ref,
                successor_receipt.clone(),
            )
            .expect("authenticate successor receipt"),
        )
        .expect("source commits successor acknowledgement");
    source
        .acknowledge_custody_successor_rebind_for_test(
            &authenticate_custody_successor_receipt(
                &mut TestAuthenticator,
                &delivered.transfer.target_machine_ref,
                successor_receipt.clone(),
            )
            .expect("authenticate replay"),
        )
        .expect("source acknowledgement replay is idempotent");
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    assert_eq!(
        receiver
            .accept_custody_successor_rebind(
                &authenticated,
                &delivered.transfer.target_machine_ref,
                &new_incarnation,
                &rebind.new_target_route_ref,
                &rebind.terminal_adapter,
                &rebind.new_authority_digest,
            )
            .expect("expired exact receiver replay returns its durable receipt"),
        successor_receipt
    );
    source
        .acknowledge_custody_successor_rebind_for_test(
            &authenticate_custody_successor_receipt(
                &mut TestAuthenticator,
                &delivered.transfer.target_machine_ref,
                successor_receipt.clone(),
            )
            .expect("authenticate expired acknowledgement replay"),
        )
        .expect("expired exact source replay remains idempotent");
    receiver
        .commit_custody_successor_rebind(
            &authenticate_custody_successor_receipt(
                &mut TestAuthenticator,
                &delivered.source.envelope.source_machine_ref,
                successor_receipt,
            )
            .expect("authenticate source finalization"),
        )
        .expect("receiver commits only after source acknowledgement");
    assert!(
        receiver
            .accept_custody(
                &authenticate_custody_transfer(&mut TestAuthenticator, delivered.transfer.clone(),)
                    .expect("authenticate late old delivery"),
                &delivered.transfer.target_machine_ref,
                &delivered.transfer.target_incarnation_ref,
            )
            .is_err(),
        "late old-incarnation delivery must not revive old custody"
    );
    let claim = receiver
        .claim_custody_inbox(
            &delivered.source.envelope.message_id,
            &opaque_ref("owner", "successor consumer"),
            Utc::now() + ChronoDuration::seconds(30),
        )
        .expect("successor claims migrated inbox");
    assert!(
        receiver
            .apply_custody_effect(
                &claim,
                &InboxAuthority::new(
                    11,
                    delivered.transfer.target_incarnation_ref,
                    delivered.transfer.rebind_authority_digest,
                )
                .expect("old authority"),
                &digest(b"successor effect"),
                |_| panic!("old incarnation cannot apply"),
            )
            .is_err()
    );
    let processed = receiver
        .apply_custody_effect(
            &claim,
            &InboxAuthority::new(11, new_incarnation.clone(), new_authority.clone())
                .expect("successor authority"),
            &digest(b"successor effect"),
            |_| Ok(()),
        )
        .expect("successor applies one effect");
    drop(receiver);
    let receiver = WorkLedger::open_existing(delivered.receiver_temp.path())
        .expect("processed successor receiver reopen")
        .expect("processed successor receipt validates against effective binding");
    let pending = receiver
        .processed_custody_receipts(1)
        .expect("processed successor receipt remains sendable after reopen");
    assert_eq!(
        pending,
        vec![(
            processed.clone(),
            delivered.source.envelope.source_machine_ref.clone()
        )]
    );
    assert_eq!(
        receiver
            .apply_custody_effect(
                &claim,
                &InboxAuthority::new(11, new_incarnation.clone(), new_authority.clone(),)
                    .expect("successor replay authority"),
                &digest(b"successor effect"),
                |_| panic!("processed successor replay cannot apply twice"),
            )
            .expect("processed successor replay returns canonical receipt"),
        processed
    );
    source
        .acknowledge_remote_processed(
            &authenticate_processed_receipt(
                &mut TestAuthenticator,
                &delivered.transfer.target_machine_ref,
                processed.clone(),
            )
            .expect("authenticate processed receipt"),
        )
        .expect("source closes retained custody");
    receiver
        .acknowledge_processed_delivery(&processed, &delivered.source.envelope.source_machine_ref)
        .expect("receiver records processed delivery acknowledgement");
    receiver
        .acknowledge_processed_delivery(&processed, &delivered.source.envelope.source_machine_ref)
        .expect("processed delivery acknowledgement replay is idempotent");
    drop(receiver);
    WorkLedger::open_existing(delivered.receiver_temp.path())
        .expect("acknowledged processed successor reopen")
        .expect("acknowledged processed successor remains valid");
}

#[test]
#[allow(clippy::too_many_lines)] // One Gate 0B.3 claim-versus-finalization race proof.
fn gate_0b_3_successor_rebind_and_consumer_claim_are_race_fenced() {
    let delivered = delivered_fixture("successor claim race");
    let old_claim = delivered
        .receiver
        .claim_custody_inbox(
            &delivered.source.envelope.message_id,
            &opaque_ref("owner", "old daemon"),
            Utc::now() + ChronoDuration::seconds(30),
        )
        .expect("old daemon claim");
    let successor_proof = digest(b"successor proof");
    let new_incarnation = opaque_ref("incarnation", "new daemon");
    let rebind = delivered
        .source
        .ledger
        .prepare_custody_successor_rebind_for_test(
            &delivered.source.envelope.message_id,
            &delivered.transfer.target_incarnation_ref,
            &new_incarnation,
            &opaque_ref("route", "new route"),
            "cmux",
            &digest(b"new authority"),
            &successor_proof,
            Utc::now() + ChronoDuration::minutes(5),
        )
        .expect("prepare successor");
    let authenticated = authenticate_custody_successor_rebind(
        &mut TestAuthenticator,
        &delivered.source.envelope.source_machine_ref,
        rebind.clone(),
    )
    .expect("authenticate successor");
    assert!(
        delivered
            .receiver
            .accept_custody_successor_rebind(
                &authenticated,
                &delivered.transfer.target_machine_ref,
                &new_incarnation,
                &rebind.new_target_route_ref,
                &rebind.terminal_adapter,
                &rebind.new_authority_digest,
            )
            .is_err(),
        "an active old consumer wins the first race"
    );
    delivered
        .receiver
        .connect_read_write()
        .expect("expire old claim")
        .execute(
            "UPDATE custody_inbox_claims SET expires_at = ?3
              WHERE message_id = ?1 AND epoch = ?2",
            params![
                old_claim.message_id,
                old_claim.epoch,
                (Utc::now() - ChronoDuration::seconds(1)).to_rfc3339()
            ],
        )
        .expect("simulate offline old daemon");
    let receipt = delivered
        .receiver
        .accept_custody_successor_rebind(
            &authenticated,
            &delivered.transfer.target_machine_ref,
            &new_incarnation,
            &rebind.new_target_route_ref,
            &rebind.terminal_adapter,
            &rebind.new_authority_digest,
        )
        .expect("expired old consumer permits successor prepare");
    assert!(
        delivered
            .receiver
            .claim_custody_inbox(
                &delivered.source.envelope.message_id,
                &opaque_ref("owner", "late old consumer"),
                Utc::now() + ChronoDuration::minutes(1),
            )
            .is_err(),
        "receiver prepare pins the old consumer boundary until final commit"
    );
    delivered
        .source
        .ledger
        .acknowledge_custody_successor_rebind_for_test(
            &authenticate_custody_successor_receipt(
                &mut TestAuthenticator,
                &delivered.transfer.target_machine_ref,
                receipt.clone(),
            )
            .expect("authenticate prepared receipt"),
        )
        .expect("source acknowledges successor");
    delivered
        .receiver
        .commit_custody_successor_rebind(
            &authenticate_custody_successor_receipt(
                &mut TestAuthenticator,
                &delivered.source.envelope.source_machine_ref,
                receipt,
            )
            .expect("authenticate source finalization"),
        )
        .expect("receiver finalizes successor");
    assert!(
        delivered
            .receiver
            .apply_custody_effect(
                &old_claim,
                &InboxAuthority::new(
                    11,
                    delivered.transfer.target_incarnation_ref,
                    delivered.transfer.rebind_authority_digest,
                )
                .expect("old authority"),
                &digest(b"old effect"),
                |_| panic!("late old daemon cannot mutate"),
            )
            .is_err()
    );
}

#[test]
#[allow(clippy::too_many_lines)] // End-to-end protocol proof keeps every custody boundary visible.
fn cross_machine_custody_is_local_persist_before_ack_and_exactly_once_effect() {
    let source = source_fixture("complete transfer");
    stage(&source);
    let stale_claim = source
        .ledger
        .claim_custody_send(
            &source.envelope.message_id,
            &opaque_ref("owner", "sender"),
            Utc::now() + ChronoDuration::seconds(30),
        )
        .expect("sender claim");
    let transfer = source
        .ledger
        .custody_transfer(&stale_claim)
        .expect("transfer");
    let authenticated = authenticate_custody_transfer(&mut TestAuthenticator, transfer.clone())
        .expect("authenticated transfer");
    source
        .ledger
        .connect_read_write()
        .expect("expire sender lease")
        .execute(
            "UPDATE custody_sender_claims SET expires_at = ?3
              WHERE message_id = ?1 AND epoch = ?2",
            params![
                stale_claim.message_id,
                stale_claim.epoch,
                (Utc::now() - ChronoDuration::seconds(1)).to_rfc3339()
            ],
        )
        .expect("simulate sender restart after lost receipt");
    let claim = source
        .ledger
        .claim_custody_send(
            &source.envelope.message_id,
            &opaque_ref("owner", "replacement sender"),
            Utc::now() + ChronoDuration::seconds(30),
        )
        .expect("replacement sender claim");
    let retry_transfer = source
        .ledger
        .custody_transfer(&claim)
        .expect("replacement transfer");
    assert_eq!(
        retry_transfer, transfer,
        "renewing a local claim must preserve the cross-host wire identity"
    );
    let retry_authenticated = authenticate_custody_transfer(&mut TestAuthenticator, retry_transfer)
        .expect("authenticated retry transfer");

    let receiver_temp = TempDir::new().expect("receiver temp");
    let receiver = WorkLedger::open(receiver_temp.path()).expect("receiver ledger");
    assert_ne!(
        source.ledger.path(),
        receiver.path(),
        "one local WAL DB per host"
    );
    let encoded = serde_json::to_string(&transfer).expect("transfer json");
    assert!(!encoded.contains("path"));
    assert!(!encoded.contains("sqlite"));
    assert!(!encoded.contains("database"));

    assert!(
        receiver
            .accept_custody(
                &authenticated,
                &transfer.target_machine_ref,
                &opaque_ref("incarnation", "different daemon"),
            )
            .is_err(),
        "a receiver must refuse an envelope for another local incarnation"
    );
    assert_eq!(
        receiver
            .connect_read_only()
            .expect("inspect refused transfer")
            .query_row("SELECT COUNT(*) FROM custody_inbox", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("inbox remains empty"),
        0
    );
    let custody = receiver
        .accept_custody(
            &authenticated,
            &transfer.target_machine_ref,
            &transfer.target_incarnation_ref,
        )
        .expect("persist receiver inbox");
    let duplicate = receiver
        .accept_custody(
            &retry_authenticated,
            &transfer.target_machine_ref,
            &transfer.target_incarnation_ref,
        )
        .expect("idempotent redelivery");
    assert_eq!(duplicate, custody);
    drop(receiver);
    let receiver = WorkLedger::open_existing(receiver_temp.path())
        .expect("restart receiver")
        .expect("persisted receiver ledger");
    let receiver_connection = receiver.connect_read_only().expect("receiver inspect");
    assert_eq!(
        receiver_connection
            .query_row(
                "SELECT state FROM custody_inbox WHERE message_id = ?1",
                [&source.envelope.message_id],
                |row| row.get::<_, String>(0),
            )
            .expect("inbox state"),
        "received"
    );
    drop(receiver_connection);

    let wrong_peer_receipt = authenticate_custody_receipt(
        &mut TestAuthenticator,
        &opaque_ref("machine", "attacker"),
        custody.clone(),
    )
    .expect("well-formed wrong peer witness");
    assert!(
        source
            .ledger
            .acknowledge_remote_custody(&claim, &wrong_peer_receipt)
            .is_err(),
        "integrity without the authenticated target identity is insufficient"
    );
    let authenticated_receipt = authenticate_custody_receipt(
        &mut TestAuthenticator,
        &transfer.target_machine_ref,
        custody.clone(),
    )
    .expect("authenticated custody receipt");
    assert!(
        source
            .ledger
            .acknowledge_remote_custody(&stale_claim, &authenticated_receipt)
            .is_err(),
        "an expired sender claim cannot mutate state after a replacement claim"
    );
    source
        .ledger
        .acknowledge_remote_custody(&claim, &authenticated_receipt)
        .expect("sender accepts receiver custody");
    let source_connection = source.ledger.connect_read_only().expect("source inspect");
    assert_eq!(
        source_connection
            .query_row(
                "SELECT state FROM custody_outbox WHERE message_id = ?1",
                [&source.envelope.message_id],
                |row| row.get::<_, String>(0),
            )
            .expect("outbox state"),
        "custody_accepted",
        "delivery is not processing"
    );
    drop(source_connection);

    let inbox_claim = receiver
        .claim_custody_inbox(
            &source.envelope.message_id,
            &opaque_ref("owner", "receiver"),
            Utc::now() + ChronoDuration::seconds(30),
        )
        .expect("inbox claim");
    let wrong_authority = InboxAuthority::new(
        12,
        transfer.target_incarnation_ref.clone(),
        transfer.rebind_authority_digest.clone(),
    )
    .expect("wrong authority shape");
    assert!(
        receiver
            .apply_custody_effect(
                &inbox_claim,
                &wrong_authority,
                &digest(b"ledger effect"),
                |_| panic!("stale authority must refuse before effect"),
            )
            .is_err()
    );
    let authority = InboxAuthority::new(
        11,
        transfer.target_incarnation_ref.clone(),
        transfer.rebind_authority_digest.clone(),
    )
    .expect("authority");
    let effect_digest = digest(b"ledger effect");
    let processed = receiver
        .apply_custody_effect(&inbox_claim, &authority, &effect_digest, |tx| {
            tx.execute(
                "INSERT INTO repo_policies
                 (repo, primary_platform, compatibility_mode, compatibility_lanes_json,
                  blocking_rule, declared_dependency_lanes_json, revision, updated_at)
                 VALUES ('danielraffel/pulp', 'macos', 'independent', '[]',
                         'declared_dependency_or_shared_integrity', '[]', 1, ?1)",
                [Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
        .expect("atomic effect and processed receipt");
    drop(receiver);
    let receiver = WorkLedger::open_existing(receiver_temp.path())
        .expect("reopen processed receiver")
        .expect("processed receiver persists");
    let replay = receiver
        .apply_custody_effect(&inbox_claim, &authority, &effect_digest, |_| {
            panic!("idempotent replay must not repeat the ledger effect")
        })
        .expect("idempotent processed replay");
    assert_eq!(replay, processed);
    let receiver_connection = receiver.connect_read_only().expect("receiver inspect");
    assert_eq!(
        receiver_connection
            .query_row(
                "SELECT COUNT(*) FROM repo_policies WHERE repo = 'danielraffel/pulp'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("effect count"),
        1
    );
    drop(receiver_connection);

    source
        .ledger
        .acknowledge_remote_processed(
            &authenticate_processed_receipt(
                &mut TestAuthenticator,
                &transfer.target_machine_ref,
                processed.clone(),
            )
            .expect("authenticated processed receipt"),
        )
        .expect("final processed acknowledgement");
    let source_connection = source.ledger.connect_read_only().expect("source inspect");
    assert_eq!(
        source_connection
            .query_row(
                "SELECT state FROM custody_outbox WHERE message_id = ?1",
                [&source.envelope.message_id],
                |row| row.get::<_, String>(0),
            )
            .expect("final state"),
        "processed"
    );
    drop(source_connection);

    assert_eq!(
        receiver
            .processed_custody_receipts(8)
            .expect("pending processed acknowledgement")
            .len(),
        1
    );
    receiver
        .acknowledge_processed_delivery(&processed, &source.envelope.source_machine_ref)
        .expect("persist receiver-side processed acknowledgement");
    drop(receiver);
    let receiver = WorkLedger::open_existing(receiver_temp.path())
        .expect("reopen receiver after processed acknowledgement")
        .expect("receiver acknowledgement persists");
    assert!(
        receiver
            .processed_custody_receipts(8)
            .expect("processed acknowledgement suppresses replay")
            .is_empty()
    );

    let receiver_connection = receiver.connect_read_write().expect("corruption fixture");
    receiver_connection
        .execute_batch(
            "DROP TRIGGER custody_inbox_processed_receipt_immutable;
             UPDATE custody_inbox SET processed_receipt_json = x'7b7d';",
        )
        .expect("simulate offline receipt corruption");
    drop(receiver_connection);
    drop(receiver);
    assert!(
        WorkLedger::open_existing(receiver_temp.path()).is_err(),
        "reopen must digest-validate the durable processed receipt"
    );
}

#[test]
fn target_rebind_is_epoch_fenced_and_post_process_changes_are_new_messages() {
    let source = source_fixture("rebind and correction");
    stage(&source);
    let original_incarnation = opaque_ref("incarnation", "m5 daemon one");
    let successor_incarnation = opaque_ref("incarnation", "m5 daemon two");
    assert_eq!(
        source
            .ledger
            .rebind_unprocessed_custody_target(
                &source.envelope.message_id,
                &original_incarnation,
                &opaque_ref("machine", "m5"),
                &successor_incarnation,
                &opaque_ref("route", "m5 successor route"),
                "cmux",
                &digest(b"authenticated successor binding"),
            )
            .expect("rebind"),
        2
    );
    assert!(
        source
            .ledger
            .rebind_unprocessed_custody_target(
                &source.envelope.message_id,
                &original_incarnation,
                &opaque_ref("machine", "m5"),
                &opaque_ref("incarnation", "m5 daemon three"),
                &opaque_ref("route", "m5 third route"),
                "herdr",
                &digest(b"authority"),
            )
            .is_err()
    );
    let claim = source
        .ledger
        .claim_custody_send(
            &source.envelope.message_id,
            &opaque_ref("owner", "sender"),
            Utc::now() + ChronoDuration::seconds(30),
        )
        .expect("claim");
    let transfer = source.ledger.custody_transfer(&claim).expect("transfer");
    assert_eq!(transfer.rebind_epoch, 2);
    assert_eq!(transfer.target_incarnation_ref, successor_incarnation);
    assert_eq!(transfer.terminal_adapter, "cmux");

    source
        .ledger
        .cancel_or_supersede_unprocessed_custody(
            &source.envelope.message_id,
            None,
            &digest(b"cancel authority"),
        )
        .expect_err("an actively claimed message cannot be silently cancelled");

    let correction = CustodyEnvelope::new(
        source.envelope.wake_id.clone(),
        source.envelope.work_item_id.clone(),
        source.envelope.work_generation,
        source.envelope.owner_generation,
        source.envelope.content_digest.clone(),
        source.envelope.work_authority_digest.clone(),
        source.envelope.workstream_handle.clone(),
        source.envelope.workstream_revision + 1,
        source.envelope.source_machine_ref.clone(),
        source.envelope.source_incarnation_ref.clone(),
        CustodyRelation::correction(source.envelope.message_id.clone()),
    )
    .expect("correction envelope");
    assert!(
        source
            .ledger
            .stage_cross_machine_custody(
                &correction,
                &opaque_ref("machine", "m5"),
                &successor_incarnation,
                &opaque_ref("route", "correction route"),
                "herdr",
                &digest(b"correction authority"),
            )
            .is_err(),
        "a correction is append-only and only valid after processing"
    );
}

#[test]
fn unclaimed_message_can_be_cancelled_but_never_erased() {
    let source = source_fixture("cancel pending");
    stage(&source);
    source
        .ledger
        .cancel_or_supersede_unprocessed_custody(
            &source.envelope.message_id,
            None,
            &digest(b"cancel authority"),
        )
        .expect("cancel");
    let connection = source.ledger.connect_read_only().expect("inspect");
    let (state, events): (String, i64) = connection
        .query_row(
            "SELECT state,
                    (SELECT COUNT(*) FROM custody_events WHERE message_id = ?1)
               FROM custody_outbox WHERE message_id = ?1",
            [&source.envelope.message_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("retained cancelled message");
    assert_eq!(state, "cancelled");
    assert_eq!(events, 2);
    assert!(
        connection
            .execute(
                "DELETE FROM custody_events WHERE message_id = ?1",
                [&source.envelope.message_id],
            )
            .is_err()
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One end-to-end control test keeps replay races visible.
fn authenticated_remote_cancel_wins_after_delivery_but_before_processing() {
    let source = source_fixture("remote cancel");
    stage(&source);
    stage(&source); // exact replay is idempotent
    let claim = source
        .ledger
        .claim_custody_send(
            &source.envelope.message_id,
            &opaque_ref("owner", "sender"),
            Utc::now() + ChronoDuration::seconds(30),
        )
        .expect("claim");
    let transfer = source.ledger.custody_transfer(&claim).expect("transfer");
    let authenticated = authenticate_custody_transfer(&mut TestAuthenticator, transfer.clone())
        .expect("authenticate transfer");
    let receiver_temp = TempDir::new().expect("receiver temp");
    let receiver = WorkLedger::open(receiver_temp.path()).expect("receiver");
    let custody = receiver
        .accept_custody(
            &authenticated,
            &transfer.target_machine_ref,
            &transfer.target_incarnation_ref,
        )
        .expect("accept");
    source
        .ledger
        .acknowledge_remote_custody(
            &claim,
            &authenticate_custody_receipt(
                &mut TestAuthenticator,
                &transfer.target_machine_ref,
                custody,
            )
            .expect("authenticated custody receipt"),
        )
        .expect("custody ack");

    let control = source
        .ledger
        .prepare_remote_custody_control(
            &source.envelope.message_id,
            None,
            &digest(b"authenticated rebind authority"),
        )
        .expect("prepare durable control");
    let authenticated_control = authenticate_custody_control(
        &mut TestAuthenticator,
        &source.envelope.source_machine_ref,
        control.clone(),
    )
    .expect("authenticate control transport");
    let receipt = receiver
        .apply_remote_custody_control(&authenticated_control)
        .expect("receiver applies before read");
    assert_eq!(
        receiver
            .apply_remote_custody_control(&authenticated_control)
            .expect("exact receiver control replay"),
        receipt
    );
    let conflicting_control = source
        .ledger
        .prepare_remote_custody_control(
            &source.envelope.message_id,
            Some(&opaque_ref("wm", "successor custody message")),
            &digest(b"authenticated rebind authority"),
        )
        .expect("prepare competing control");
    assert_ne!(conflicting_control.control_id, control.control_id);
    assert!(
        receiver
            .apply_remote_custody_control(
                &authenticate_custody_control(
                    &mut TestAuthenticator,
                    &source.envelope.source_machine_ref,
                    conflicting_control,
                )
                .expect("authenticate competing control"),
            )
            .is_err(),
        "a terminal receiver must not acknowledge a distinct same-kind control"
    );
    let authenticated_receipt = authenticate_custody_control_receipt(
        &mut TestAuthenticator,
        &transfer.target_machine_ref,
        receipt,
    )
    .expect("authenticated control receipt");
    source
        .ledger
        .acknowledge_remote_custody_control(&authenticated_receipt)
        .expect("sender records receiver control ack");
    source
        .ledger
        .acknowledge_remote_custody_control(&authenticated_receipt)
        .expect("exact sender receipt replay");
    let source_connection = source.ledger.connect_read_only().expect("source inspect");
    assert_eq!(
        source_connection
            .query_row(
                "SELECT state FROM custody_outbox WHERE message_id = ?1",
                [&source.envelope.message_id],
                |row| row.get::<_, String>(0),
            )
            .expect("source state"),
        "cancelled"
    );
    assert_eq!(
        source_connection
            .query_row(
                "SELECT COUNT(*) FROM custody_events
                  WHERE message_id = ?1 AND side = 'sender' AND kind = 'cancelled'",
                [&source.envelope.message_id],
                |row| row.get::<_, i64>(0),
            )
            .expect("one sender terminal event"),
        1
    );
    let receiver_connection = receiver.connect_read_only().expect("receiver inspect");
    assert_eq!(
        receiver_connection
            .query_row(
                "SELECT state FROM custody_inbox WHERE message_id = ?1",
                [&source.envelope.message_id],
                |row| row.get::<_, String>(0),
            )
            .expect("receiver state"),
        "cancelled"
    );
    assert!(
        receiver
            .claim_custody_inbox(
                &source.envelope.message_id,
                &opaque_ref("owner", "receiver"),
                Utc::now() + ChronoDuration::seconds(30),
            )
            .is_err()
    );
}
