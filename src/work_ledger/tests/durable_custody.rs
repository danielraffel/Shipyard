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
    _temp: TempDir,
    ledger: WorkLedger,
    envelope: CustodyEnvelope,
}

fn source_fixture(label: &str) -> Fixture {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    ledger.import(&[sample_candidate()]).expect("import work");
    let connection = ledger.connect_read_only().expect("connection");
    let (work_id, work_generation, owner_generation): (String, i64, i64) = connection
        .query_row(
            "SELECT id, work_generation, owner_generation FROM work_items LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
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
        "GEN-14".to_owned(),
        11,
        opaque_ref("machine", "m3"),
        opaque_ref("incarnation", "m3 daemon one"),
        CustodyRelation::wake(),
    )
    .expect("envelope");
    Fixture {
        _temp: temp,
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
            &digest(b"authenticated cancel authority"),
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
            None,
            &digest(b"different cancel authority"),
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
