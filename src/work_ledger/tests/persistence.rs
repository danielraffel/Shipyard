use super::*;
use crate::work_ledger::route::OpaqueRef;

fn install_exact_v4_schema(connection: &Connection) {
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TRIGGER protected_object_capacity;
             DROP TRIGGER protected_object_immutable;
             DROP TRIGGER protected_object_no_delete;
             DROP TRIGGER activation_epoch_identity_immutable;
             DROP TRIGGER activation_epoch_no_delete;
             DROP TRIGGER activation_epoch_release_fence;
             DROP TRIGGER provider_delivery_insert_fence;
             DROP TRIGGER provider_delivery_identity_immutable;
             DROP TRIGGER provider_delivery_no_delete;
             DROP TRIGGER provider_delivery_state_fence_insert;
             DROP TRIGGER provider_delivery_state_fence_update;
             DROP TRIGGER provider_delivery_observation_insert_fence;
             DROP TRIGGER provider_delivery_observation_immutable;
             DROP TRIGGER provider_delivery_observation_no_delete;
             DROP TRIGGER agent_ownership_insert_fence;
             DROP TRIGGER agent_ownership_identity_immutable;
             DROP TRIGGER agent_ownership_no_delete;
             DROP TRIGGER agent_ownership_context_fence;
             DROP TRIGGER agent_ownership_state_fence;
             DROP TRIGGER agent_ownership_receipt_immutable;
             DROP TRIGGER outbox_acknowledged_fence;
             DROP TRIGGER outbox_acknowledged_insert_fence;
             DROP TRIGGER wake_attempt_acknowledged_insert_fence;
             DROP TRIGGER wake_attempt_acknowledged_update_fence;
             DROP TABLE provider_delivery_observations;
             DROP TABLE agent_ownership;
             DROP TABLE provider_deliveries;
             DROP TABLE activation_epochs;
             DROP TABLE protected_objects;
             DROP TABLE wake_claim_epochs;
             DROP TABLE wake_attempts;
             ALTER TABLE outbox RENAME TO outbox_v5;
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
                route_ref, payload_digest, transport_receipt_digest,
                created_at, updated_at, acknowledged_at)
             SELECT wake_id, work_item_id, work_generation, owner_generation, state,
                    route_ref, payload_digest, transport_receipt_digest,
                    created_at, updated_at, acknowledged_at
               FROM outbox_v5;
             DROP TABLE outbox_v5;
             CREATE TABLE wake_attempts (
               wake_id TEXT NOT NULL REFERENCES outbox(wake_id) ON DELETE RESTRICT,
               attempt INTEGER NOT NULL CHECK(attempt > 0),
               state TEXT NOT NULL CHECK(state IN ('claimed', 'acknowledged', 'retry', 'uncertain', 'failed')),
               adapter_id TEXT NOT NULL,
               idempotent INTEGER NOT NULL CHECK(idempotent IN (0, 1)),
               outcome_digest TEXT,
               started_at TEXT NOT NULL,
               finished_at TEXT,
               PRIMARY KEY(wake_id, attempt)
             );
             CREATE TABLE wake_claim_epochs (
               wake_id TEXT NOT NULL,
               attempt INTEGER NOT NULL,
               epoch INTEGER NOT NULL CHECK(epoch > 0),
               owner_ref TEXT NOT NULL,
               kind TEXT NOT NULL CHECK(kind IN ('claim', 'recovery')),
               acquired_at TEXT NOT NULL,
               PRIMARY KEY(wake_id, attempt, epoch),
               FOREIGN KEY(wake_id, attempt) REFERENCES wake_attempts(wake_id, attempt)
                 ON DELETE RESTRICT
             );
             CREATE INDEX outbox_delivery ON outbox(state, created_at, wake_id);
             CREATE INDEX wake_attempts_recovery ON wake_attempts(state, started_at, wake_id);
             CREATE INDEX wake_claim_epoch_owner ON wake_claim_epochs(owner_ref, epoch);
             PRAGMA user_version = 4;
             PRAGMA foreign_keys = ON;",
        )
        .expect("install exact v4 schema");
}

fn install_exact_v1_registry_schema(
    ledger: &WorkLedger,
    fixtures: &[(&AdapterBindingRecord, &str)],
) {
    let mut connection = ledger.connect_read_write().expect("connection");
    install_exact_v4_schema(&connection);
    let transaction = connection.transaction().expect("v1 transaction");
    transaction
        .execute_batch(
            "ALTER TABLE adapter_registry RENAME TO adapter_registry_v2;
             CREATE TABLE adapter_registry (
               registry_ref TEXT PRIMARY KEY,
               axis TEXT NOT NULL CHECK(axis IN ('terminal', 'provider')),
               name TEXT NOT NULL,
               generation INTEGER NOT NULL CHECK(generation > 0),
               revision INTEGER NOT NULL CHECK(revision > 0),
               implementation_digest TEXT NOT NULL,
               configuration_digest TEXT NOT NULL,
               capabilities_digest TEXT NOT NULL,
               state TEXT NOT NULL CHECK(state IN ('active', 'retired')),
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               UNIQUE(axis, name, generation, revision)
             );",
        )
        .expect("exact v1 registry schema");
    for (adapter, state) in fixtures {
        transaction
            .execute(
                "INSERT INTO adapter_registry
                 (registry_ref, axis, name, generation, revision,
                  implementation_digest, configuration_digest, capabilities_digest,
                  state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                params![
                    adapter.registry_ref.as_str(),
                    adapter.axis.as_str(),
                    adapter.name,
                    adapter.generation,
                    adapter.revision,
                    adapter.implementation_sha256.as_str(),
                    adapter.configuration_sha256.as_str(),
                    adapter.capabilities_sha256.as_str(),
                    state,
                    "2026-08-27T00:00:00Z",
                ],
            )
            .expect("v1 adapter fixture");
    }
    transaction
        .execute_batch(
            "DROP TABLE adapter_registry_v2;
             DROP TABLE wake_claim_epochs;
             DROP TABLE wake_attempts;
             PRAGMA user_version = 1;",
        )
        .expect("finish v1 fixture");
    transaction.commit().expect("commit v1 fixture");
}

#[derive(serde::Serialize)]
struct LegacyIntegrityPayload<'a> {
    schema_version: &'a serde_json::Value,
    terminal: &'a serde_json::Value,
    agent: &'a serde_json::Value,
    provider: &'a serde_json::Value,
    launch_profile: &'a serde_json::Value,
}

fn insert_v1_route_and_wake(
    ledger: &WorkLedger,
    work_id: &str,
    route: &RouteRegistration,
) -> Vec<u8> {
    let mut payload = serde_json::to_value(&route.provenance).expect("route JSON");
    payload
        .get_mut("agent")
        .and_then(serde_json::Value::as_object_mut)
        .expect("agent route object")
        .remove("adapter")
        .expect("remove post-v1 agent binding");
    let integrity_payload = LegacyIntegrityPayload {
        schema_version: &payload["schema_version"],
        terminal: &payload["terminal"],
        agent: &payload["agent"],
        provider: &payload["provider"],
        launch_profile: &payload["launch_profile"],
    };
    let integrity = digest(&serde_json::to_vec(&integrity_payload).expect("v1 integrity payload"));
    payload["integrity_sha256"] = serde_json::Value::String(integrity.clone());
    let payload = serde_json::to_vec(&payload).expect("legacy route payload");
    let envelope_integrity = digest(
        format!(
            "shipyard-route-envelope-v1\0{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            route.route_ref.as_str(),
            route.work_id,
            route.head_sha,
            route.work_generation,
            route.owner_ref.as_str(),
            route.owner_generation,
            route.revision,
            route.origin_machine_ref.as_str(),
            integrity,
        )
        .as_bytes(),
    );
    assert!(
        serde_json::from_slice::<RouteProvenanceRecord>(&payload).is_err(),
        "the exact v1 route lacks a safely inferable agent binding"
    );
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute(
            "INSERT INTO route_records
             (route_ref, work_item_id, head_sha, work_generation, owner_ref,
              owner_generation, revision, origin_machine_ref, terminal_kind,
              agent_kind, provider_kind, payload_json, payload_digest,
              integrity_hash, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?15)",
            params![
                route.route_ref.as_str(),
                route.work_id,
                route.head_sha,
                route.work_generation,
                route.owner_ref.as_str(),
                route.owner_generation,
                route.revision,
                route.origin_machine_ref.as_str(),
                route.provenance.terminal_kind(),
                route.provenance.agent_kind(),
                route.provenance.provider_kind(),
                payload,
                digest(&payload),
                envelope_integrity,
                "2026-08-27T00:00:00Z",
            ],
        )
        .expect("v1 route fixture");
    connection
        .execute(
            "INSERT INTO outbox
             (wake_id, work_item_id, work_generation, owner_generation, state,
              route_ref, payload_digest, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?7)",
            params![
                opaque_ref("wake", "v1 route wake"),
                work_id,
                1,
                3,
                route.route_ref.as_str(),
                digest(b"v1 wake payload"),
                "2026-08-27T00:00:00Z",
            ],
        )
        .expect("v1 route-backed wake fixture");
    payload
}

#[test]
fn v1_registry_migrates_transactionally_and_accepts_exact_agent_binding() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("create current ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    let (route, adapters) = sample_registered_route(&work_id);
    let terminal_adapter = &adapters[0];
    let agent_adapter = &adapters[1];
    let provider_adapter = adapter_binding(AdapterAxis::Provider, "future-provider", "provider");
    install_exact_v1_registry_schema(
        &ledger,
        &[(terminal_adapter, "active"), (&provider_adapter, "retired")],
    );
    drop(ledger);

    let migrated = WorkLedger::open(temp.path()).expect("migrate exact v1 ledger");
    let connection = migrated.connect_read_only().expect("inspect migration");
    assert_eq!(schema_version(&connection).expect("schema version"), 6);
    let preserved: Vec<(String, String, String)> = {
        let mut statement = connection
            .prepare(
                "SELECT axis, name, state FROM adapter_registry
                 ORDER BY axis, name",
            )
            .expect("preserved query");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("preserved rows")
            .collect::<Result<_, _>>()
            .expect("collect rows")
    };
    assert_eq!(
        preserved,
        vec![
            (
                "provider".to_owned(),
                "future-provider".to_owned(),
                "retired".to_owned(),
            ),
            (
                "terminal".to_owned(),
                "wezterm".to_owned(),
                "active".to_owned(),
            ),
        ]
    );
    let registry_sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'adapter_registry'",
            [],
            |row| row.get(0),
        )
        .expect("registry schema");
    assert!(registry_sql.contains("'terminal', 'agent', 'provider'"));
    let protected_indexes: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_index_list('adapter_registry')
             WHERE origin IN ('pk', 'u')",
            [],
            |row| row.get(0),
        )
        .expect("registry indexes");
    assert_eq!(protected_indexes, 2, "primary and unique indexes survive");
    let foreign_key_violations: i64 = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .expect("foreign key check");
    assert_eq!(foreign_key_violations, 0);
    drop(connection);

    migrated
        .register_adapter(agent_adapter)
        .expect("register exact agent after migration");
    migrated.import(&[candidate]).expect("import work item");
    migrated
        .register_route(&route)
        .expect("route can proceed after v1 migration");
}

#[test]
fn v2_outbox_migrates_through_durable_attempts_and_claim_epochs() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("create current ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("work fixture");
    let connection = ledger.connect_read_write().expect("connection");
    install_exact_v4_schema(&connection);
    connection
        .execute_batch(
            "DROP TABLE wake_claim_epochs;
             DROP TABLE wake_attempts;
             PRAGMA user_version = 2;",
        )
        .expect("exact v2 schema");
    drop(connection);
    drop(ledger);

    let migrated = WorkLedger::open(temp.path()).expect("migrate v2 ledger");
    let connection = migrated.connect_read_only().expect("inspect migration");
    assert_eq!(schema_version(&connection).expect("schema version"), 6);
    let attempt_table: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = 'wake_attempts'",
            [],
            |row| row.get(0),
        )
        .expect("attempt table");
    assert_eq!(attempt_table, 1);
    let preserved_work: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("preserved work");
    assert_eq!(preserved_work, 1);
}

#[test]
fn v3_attempts_migrate_to_claim_epochs_without_rewriting_attempts() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("create current ledger");
    let connection = ledger.connect_read_write().expect("connection");
    install_exact_v4_schema(&connection);
    connection
        .execute_batch("DROP TABLE wake_claim_epochs; PRAGMA user_version = 3;")
        .expect("exact v3 schema");
    drop(connection);
    drop(ledger);

    let migrated = WorkLedger::open(temp.path()).expect("migrate v3 ledger");
    let connection = migrated.connect_read_only().expect("inspect migration");
    assert_eq!(schema_version(&connection).expect("schema version"), 6);
    let claim_table: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'table' AND name = 'wake_claim_epochs'",
            [],
            |row| row.get(0),
        )
        .expect("claim table");
    assert_eq!(claim_table, 1);
}

#[test]
fn v5_migrates_to_append_only_provider_delivery_observations() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("create current ledger");
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch(
            "DROP TRIGGER provider_delivery_observation_insert_fence;
             DROP TRIGGER provider_delivery_observation_no_delete;
             DROP TRIGGER provider_delivery_observation_immutable;
             DROP INDEX provider_delivery_observations_delivery;
             DROP TABLE provider_delivery_observations;
             PRAGMA user_version = 5;",
        )
        .expect("exact v5 surface");
    drop(connection);
    drop(ledger);

    let migrated = WorkLedger::open(temp.path()).expect("migrate v5 ledger");
    let connection = migrated.connect_read_only().expect("inspect migration");
    assert_eq!(schema_version(&connection).expect("schema version"), 6);
    let observation_table: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'table' AND name = 'provider_delivery_observations'",
            [],
            |row| row.get(0),
        )
        .expect("observation table");
    assert_eq!(observation_table, 1);
}

#[test]
fn v4_acknowledged_delivery_migrates_losslessly_to_split_v5_schema() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("create current ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("work fixture");
    let connection = ledger.connect_read_write().expect("connection");
    install_exact_v4_schema(&connection);
    let wake_id = opaque_ref("wake", "historical acknowledged delivery");
    connection
        .execute(
            "INSERT INTO outbox
             (wake_id, work_item_id, work_generation, owner_generation, state,
              route_ref, payload_digest, transport_receipt_digest,
              created_at, updated_at, acknowledged_at)
             VALUES (?1, ?2, 1, 3, 'acknowledged', ?3, ?4, ?5, ?6, ?6, ?6)",
            params![
                wake_id,
                work_id,
                opaque_ref("route", "historical route"),
                digest(b"historical payload"),
                digest(b"historical receipt"),
                "2026-08-28T00:00:00Z",
            ],
        )
        .expect("historical outbox");
    connection
        .execute(
            "INSERT INTO wake_attempts
             (wake_id, attempt, state, adapter_id, idempotent, outcome_digest,
              started_at, finished_at)
             VALUES (?1, 1, 'acknowledged', 'legacy-provider', 1, ?2, ?3, ?3)",
            params![
                wake_id,
                digest(b"historical receipt"),
                "2026-08-28T00:00:00Z",
            ],
        )
        .expect("historical attempt");
    connection
        .execute(
            "INSERT INTO wake_claim_epochs
             (wake_id, attempt, epoch, owner_ref, kind, acquired_at)
             VALUES (?1, 1, 1, ?2, 'claim', ?3)",
            params![
                wake_id,
                opaque_ref("consumer", "historical owner"),
                "2026-08-28T00:00:00Z",
            ],
        )
        .expect("historical claim epoch");
    drop(connection);
    drop(ledger);

    let migrated = WorkLedger::open(temp.path()).expect("migrate exact v4 ledger");
    let connection = migrated.connect_read_only().expect("inspect v5");
    assert_eq!(schema_version(&connection).expect("schema version"), 6);
    let outbox: (String, Option<String>, String) = connection
        .query_row(
            "SELECT state, provider_delivery_id, transport_receipt_digest
             FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("preserved outbox");
    assert_eq!(
        outbox,
        ("uncertain".to_owned(), None, digest(b"historical receipt"))
    );
    let attempt: (String, String) = connection
        .query_row(
            "SELECT state, adapter_id FROM wake_attempts
             WHERE wake_id = ?1 AND attempt = 1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("preserved attempt");
    assert_eq!(
        attempt,
        ("uncertain".to_owned(), "legacy-provider".to_owned())
    );
    let epoch: String = connection
        .query_row(
            "SELECT kind FROM wake_claim_epochs
             WHERE wake_id = ?1 AND attempt = 1 AND epoch = 1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("preserved epoch");
    assert_eq!(epoch, "claim");
    for table in [
        "protected_objects",
        "provider_deliveries",
        "agent_ownership",
        "activation_epochs",
    ] {
        let exists: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("v5 table");
        assert_eq!(exists, 1, "{table}");
    }

    let fresh_temp = TempDir::new().expect("fresh temp");
    let fresh = WorkLedger::open(fresh_temp.path()).expect("fresh v5 ledger");
    let fresh_connection = fresh.connect_read_only().expect("fresh schema");
    let schema_rows = |connection: &Connection| {
        let mut statement = connection
            .prepare(
                "SELECT type, name, sql FROM sqlite_schema
                 WHERE name IN ('outbox', 'wake_attempts', 'wake_claim_epochs',
                                'protected_objects', 'activation_epochs',
                                'provider_deliveries', 'agent_ownership',
                                'outbox_delivery', 'wake_attempts_recovery',
                                'wake_claim_epoch_owner')
                    OR name LIKE 'protected_object_%'
                    OR name LIKE 'activation_epoch_%'
                    OR name LIKE 'activation_epochs_%'
                    OR name LIKE 'provider_deliver%'
                    OR name LIKE 'agent_ownership_%'
                    OR name LIKE 'outbox_acknowledged_%'
                    OR name LIKE 'wake_attempt_acknowledged_%'
                 ORDER BY type, name",
            )
            .expect("schema query");
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .expect("schema rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("schema definitions")
    };
    assert_eq!(schema_rows(&connection), schema_rows(&fresh_connection));
}

#[cfg(unix)]
#[test]
fn delivered_state_requires_exact_provider_receipt_and_work_binding() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("work fixture");
    let request_bytes = b"provider request";
    let receipt_bytes = b"provider receipt";
    let request = ledger
        .put_protected_object(
            &work_id,
            ProtectedObjectKind::ProviderRequest,
            None,
            &digest(request_bytes),
            request_bytes,
        )
        .expect("request object");
    let receipt = ledger
        .put_protected_object(
            &work_id,
            ProtectedObjectKind::ProviderReceipt,
            None,
            &digest(receipt_bytes),
            receipt_bytes,
        )
        .expect("receipt object");
    let profile_bytes = b"launch profile";
    let profile_digest = digest(profile_bytes);
    let profile_ref = OpaqueRef::derive("launch-profile", profile_digest.as_bytes())
        .as_str()
        .to_owned();
    let profile = ledger
        .put_protected_object(
            &work_id,
            ProtectedObjectKind::LaunchProfile,
            Some(&profile_ref),
            &profile_digest,
            profile_bytes,
        )
        .expect("profile object");
    let agent_receipt_bytes = b"agent context receipt";
    let agent_receipt = ledger
        .put_protected_object(
            &work_id,
            ProtectedObjectKind::AgentReceipt,
            None,
            &digest(agent_receipt_bytes),
            agent_receipt_bytes,
        )
        .expect("agent receipt object");
    let wake_id = opaque_ref("wake", "v5 delivered wake");
    let delivery_id = opaque_ref("pd", "v5 delivery");
    let activation_id = opaque_ref("ae", "v5 activation");
    let connection = ledger.connect_read_write().expect("connection");
    configure_durable(&connection).expect("foreign keys");
    connection
        .execute(
            "INSERT INTO outbox
             (wake_id, work_item_id, work_generation, owner_generation, state,
              route_ref, profile_ref, payload_digest, created_at, updated_at)
             VALUES (?1, ?2, 1, 3, 'pending', ?3, ?4, ?5, ?6, ?6)",
            params![
                wake_id,
                work_id,
                opaque_ref("route", "v5 route"),
                profile_ref,
                profile_digest,
                "2026-08-28T00:00:00Z",
            ],
        )
        .expect("pending wake");
    assert!(
        connection
            .execute(
                "UPDATE outbox SET state = 'delivered' WHERE wake_id = ?1",
                [&wake_id],
            )
            .is_err(),
        "delivered cannot exist without a delivery ID"
    );
    connection
        .execute(
            "INSERT INTO wake_attempts
             (wake_id, attempt, state, adapter_id, idempotent, started_at)
             VALUES (?1, 1, 'claimed', 'provider-adapter', 1, ?2)",
            params![wake_id, "2026-08-28T00:00:00Z"],
        )
        .expect("attempt");
    connection
        .execute(
            "INSERT INTO activation_epochs
             (activation_id, work_item_id, work_generation, owner_generation,
              epoch, owner_ref, state, acquired_at)
             VALUES (?1, ?2, 1, 3, 1, ?3, 'active', ?4)",
            params![
                activation_id,
                work_id,
                opaque_ref("owner", "v5 owner"),
                "2026-08-28T00:00:00Z",
            ],
        )
        .expect("activation");
    assert!(
        connection
            .execute(
                "INSERT INTO provider_deliveries
                 (delivery_id, wake_id, attempt, activation_id, provider_id,
                  adapter_id, idempotency_key, request_object_ref, state,
                  created_at, updated_at)
                 VALUES (?1, ?2, 1, ?3, 'codex', 'wrong-adapter', ?4, ?5,
                         'prepared', ?6, ?6)",
                params![
                    opaque_ref("pd", "wrong adapter"),
                    wake_id,
                    activation_id,
                    opaque_ref("key", "wrong adapter"),
                    request.object_ref,
                    "2026-08-28T00:00:00Z",
                ],
            )
            .is_err(),
        "delivery adapter must match its exact attempt"
    );
    let released_activation = opaque_ref("ae", "released activation");
    connection
        .execute(
            "INSERT INTO activation_epochs
             (activation_id, work_item_id, work_generation, owner_generation,
              epoch, owner_ref, state, acquired_at, released_at)
             VALUES (?1, ?2, 1, 3, 2, ?3, 'released', ?4, ?4)",
            params![
                released_activation,
                work_id,
                opaque_ref("owner", "released owner"),
                "2026-08-28T00:00:00Z",
            ],
        )
        .expect("released activation fixture");
    assert!(
        connection
            .execute(
                "UPDATE activation_epochs SET released_at = ?2 WHERE activation_id = ?1",
                params![released_activation, "2026-08-28T00:02:00Z"],
            )
            .is_err(),
        "release evidence timestamp is write-once"
    );
    assert!(
        connection
            .execute(
                "INSERT INTO provider_deliveries
                 (delivery_id, wake_id, attempt, activation_id, provider_id,
                  adapter_id, idempotency_key, request_object_ref, state,
                  created_at, updated_at)
                 VALUES (?1, ?2, 1, ?3, 'codex', 'provider-adapter', ?4, ?5,
                         'prepared', ?6, ?6)",
                params![
                    opaque_ref("pd", "released activation delivery"),
                    wake_id,
                    released_activation,
                    opaque_ref("key", "released activation"),
                    request.object_ref,
                    "2026-08-28T00:00:00Z",
                ],
            )
            .is_err(),
        "released activation cannot authorize delivery"
    );
    connection
        .execute(
            "INSERT INTO wake_attempts
             (wake_id, attempt, state, adapter_id, idempotent, started_at)
             VALUES (?1, 2, 'claimed', 'provider-adapter', 1, ?2)",
            params![wake_id, "2026-08-28T00:00:00Z"],
        )
        .expect("second attempt");
    connection
        .execute(
            "INSERT INTO provider_deliveries
             (delivery_id, wake_id, attempt, activation_id, provider_id,
              adapter_id, idempotency_key, request_object_ref, state,
              created_at, updated_at)
             VALUES (?1, ?2, 2, ?3, 'codex', 'provider-adapter', ?4, ?5,
                     'prepared', ?6, ?6)",
            params![
                opaque_ref("pd", "prepared delivery"),
                wake_id,
                activation_id,
                opaque_ref("key", "prepared delivery"),
                request.object_ref,
                "2026-08-28T00:00:00Z",
            ],
        )
        .expect("prepared delivery");
    assert!(
        connection
            .execute(
                "UPDATE activation_epochs SET state = 'released', released_at = ?2
                 WHERE activation_id = ?1",
                params![activation_id, "2026-08-28T00:01:00Z"],
            )
            .is_err(),
        "activation cannot release while a delivery is nonterminal"
    );
    connection
        .execute(
            "UPDATE wake_attempts SET state = 'delivered', outcome_digest = ?2,
                                      finished_at = ?3
             WHERE wake_id = ?1 AND attempt = 1",
            params![wake_id, digest(receipt_bytes), "2026-08-28T00:00:00Z",],
        )
        .expect("delivered attempt");
    let insert_delivery = |receipt_object_ref: &str, idempotency: &str| {
        connection.execute(
            "INSERT INTO provider_deliveries
             (delivery_id, wake_id, attempt, activation_id, provider_id,
              adapter_id, idempotency_key, request_object_ref,
              receipt_object_ref, state, created_at, updated_at, delivered_at)
             VALUES (?1, ?2, 1, ?3, 'codex', 'provider-adapter', ?4, ?5, ?6,
                     'delivered', ?7, ?7, ?7)",
            params![
                delivery_id,
                wake_id,
                activation_id,
                idempotency,
                request.object_ref,
                receipt_object_ref,
                "2026-08-28T00:00:00Z",
            ],
        )
    };
    assert!(
        insert_delivery(&request.object_ref, &opaque_ref("key", "wrong receipt")).is_err(),
        "delivery cannot insert a request object as its receipt"
    );
    insert_delivery(&receipt.object_ref, &opaque_ref("key", "v5 idempotency")).expect("delivery");
    let ownership_id = opaque_ref("ao", "pending ownership");
    connection
        .execute(
            "INSERT INTO agent_ownership
             (ownership_id, work_item_id, work_generation, owner_generation,
              delivery_id, launch_profile_object_ref, state, created_at, updated_at)
             VALUES (?1, ?2, 1, 3, ?3, ?4, 'pending', ?5, ?5)",
            params![
                ownership_id,
                work_id,
                delivery_id,
                profile.object_ref,
                "2026-08-28T00:00:00Z",
            ],
        )
        .expect("pending ownership");
    connection
        .execute(
            "UPDATE agent_ownership SET state = 'uncertain' WHERE ownership_id = ?1",
            [&ownership_id],
        )
        .expect("uncertain ownership");
    connection
        .execute_batch("SAVEPOINT uncertain_resolution")
        .expect("savepoint");
    connection
        .execute(
            "UPDATE agent_ownership SET state = 'failed' WHERE ownership_id = ?1",
            [&ownership_id],
        )
        .expect("uncertain may resolve failed");
    connection
        .execute_batch("ROLLBACK TO uncertain_resolution; RELEASE uncertain_resolution")
        .expect("restore uncertain fixture");
    assert!(
        connection
            .execute(
                "UPDATE agent_ownership SET state = 'pending' WHERE ownership_id = ?1",
                [&ownership_id],
            )
            .is_err(),
        "uncertain ownership cannot retry without reconciliation"
    );
    assert!(
        connection
            .execute(
                "UPDATE agent_ownership
                 SET state = 'acknowledged', context_receipt_object_ref = ?2,
                     context_receipt_digest = ?3, acknowledged_at = ?4
                 WHERE ownership_id = ?1",
                params![
                    ownership_id,
                    request.object_ref,
                    request.content_digest,
                    "2026-08-28T00:00:00Z",
                ],
            )
            .is_err(),
        "acknowledged ownership requires an exact agent-receipt object"
    );
    connection
        .execute(
            "UPDATE agent_ownership
             SET state = 'acknowledged', context_receipt_object_ref = ?2,
                 context_receipt_digest = ?3, acknowledged_at = ?4
             WHERE ownership_id = ?1",
            params![
                ownership_id,
                agent_receipt.object_ref,
                agent_receipt.content_digest,
                "2026-08-28T00:00:00Z",
            ],
        )
        .expect("exact ownership acknowledgement");
    assert!(
        connection
            .execute(
                "UPDATE agent_ownership SET context_receipt_digest = ?2
                 WHERE ownership_id = ?1",
                params![ownership_id, request.content_digest],
            )
            .is_err(),
        "acknowledged ownership receipt is immutable"
    );
    assert!(
        connection
            .execute(
                "UPDATE agent_ownership SET work_generation = 2 WHERE ownership_id = ?1",
                [&ownership_id],
            )
            .is_err()
    );
    assert!(
        connection
            .execute(
                "DELETE FROM agent_ownership WHERE ownership_id = ?1",
                [&ownership_id]
            )
            .is_err()
    );
    assert!(
        connection
            .execute(
                "DELETE FROM provider_deliveries WHERE delivery_id = ?1",
                [&delivery_id]
            )
            .is_err()
    );
    assert!(
        connection
            .execute(
                "UPDATE activation_epochs SET owner_generation = 4 WHERE activation_id = ?1",
                [&activation_id],
            )
            .is_err()
    );
    assert!(
        connection
            .execute(
                "DELETE FROM protected_objects WHERE object_ref = ?1",
                [&request.object_ref],
            )
            .is_err()
    );
    connection
        .execute(
            "UPDATE outbox SET state = 'delivered', provider_delivery_id = ?2,
                               transport_receipt_digest = ?3, updated_at = ?4
             WHERE wake_id = ?1",
            params![
                wake_id,
                delivery_id,
                digest(receipt_bytes),
                "2026-08-28T00:00:00Z",
            ],
        )
        .expect("delivered wake");
    drop(connection);
    let status = ledger.status().expect("valid delivered status");
    assert_eq!(status.provider_deliveries, 2);
    assert_eq!(status.activation_epochs, 2);
    assert_eq!(status.agent_ownership, 1);

    let connection = ledger.connect_read_write().expect("tamper connection");
    assert!(
        connection
            .execute(
                "UPDATE provider_deliveries SET receipt_object_ref = ?2
                 WHERE delivery_id = ?1",
                params![delivery_id, request.object_ref],
            )
            .is_err(),
        "delivery cannot transition to a non-receipt object"
    );
}

#[test]
fn route_bearing_v1_ledger_is_preserved_for_explicit_reconciliation() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("create current ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("v1 work fixture");
    let (route, _) = sample_route(&work_id, 1);
    let terminal_adapter = adapter_binding(AdapterAxis::Terminal, "wezterm", "wezterm");
    let legacy_payload = insert_v1_route_and_wake(&ledger, &work_id, &route);
    install_exact_v1_registry_schema(&ledger, &[(&terminal_adapter, "active")]);
    drop(ledger);

    match WorkLedger::open(temp.path()) {
        Err(WorkLedgerError::Refused(reason)) => assert_eq!(
            reason,
            "schema v1 contains route records whose exact agent adapter binding cannot be inferred; explicit route reconciliation is required before v2 migration"
        ),
        other => panic!("expected precise reconciliation refusal, got {other:?}"),
    }
    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect v1");
    assert_eq!(schema_version(&connection).expect("schema version"), 1);
    for (table, expected) in [
        ("work_items", 1_i64),
        ("route_records", 1),
        ("adapter_registry", 1),
        ("outbox", 1),
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("preserved row count");
        assert_eq!(count, expected, "{table} must remain unchanged");
    }
    let preserved_payload: Vec<u8> = connection
        .query_row("SELECT payload_json FROM route_records", [], |row| {
            row.get(0)
        })
        .expect("preserved route payload");
    assert_eq!(preserved_payload, legacy_payload);
    let old_schema: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'adapter_registry'",
            [],
            |row| row.get(0),
        )
        .expect("old registry schema");
    assert!(old_schema.contains("'terminal', 'provider'"));
    assert!(!old_schema.contains("'agent'"));
}

#[test]
fn concurrent_v1_route_writer_is_fenced_before_migration_snapshot() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("create current ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("v1 work fixture");
    let (route, _) = sample_route(&work_id, 1);
    install_exact_v1_registry_schema(&ledger, &[]);

    let mut writer = ledger.connect_read_write().expect("writer connection");
    let writer_transaction = writer
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("hold v1 writer transaction");
    writer_transaction
        .execute(
            "INSERT INTO route_records
             (route_ref, work_item_id, head_sha, work_generation, owner_ref,
              owner_generation, revision, origin_machine_ref, terminal_kind,
              agent_kind, provider_kind, payload_json, payload_digest,
              integrity_hash, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'cmux', 'codex',
                     'subrouter', ?9, ?10, ?11, ?12, ?12)",
            params![
                route.route_ref.as_str(),
                route.work_id,
                route.head_sha,
                route.work_generation,
                route.owner_ref.as_str(),
                route.owner_generation,
                route.revision,
                route.origin_machine_ref.as_str(),
                br#"{"schema_version":1,"legacy_route":true}"#,
                digest(b"concurrent v1 route payload"),
                digest(b"concurrent v1 route integrity"),
                "2026-08-27T00:00:00Z",
            ],
        )
        .expect("uncommitted v1 route");

    let state_dir = temp.path().to_path_buf();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let migrator = std::thread::spawn(move || {
        let result = WorkLedger::open(&state_dir)
            .map(|_| ())
            .map_err(|error| error.to_string());
        sender.send(result).expect("send migration result");
    });
    assert!(
        matches!(
            receiver.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "exclusive migration must wait behind the active v1 route writer"
    );
    writer_transaction.commit().expect("commit v1 route");
    let migration_result = receiver
        .recv_timeout(Duration::from_secs(6))
        .expect("migration completes after writer");
    let refusal = migration_result.expect_err("route-bearing v1 must be preserved");
    assert!(refusal.contains("explicit route reconciliation is required"));
    migrator.join().expect("migrator thread");

    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect v1");
    assert_eq!(schema_version(&connection).expect("schema version"), 1);
    let routes: i64 = connection
        .query_row("SELECT COUNT(*) FROM route_records", [], |row| row.get(0))
        .expect("route count");
    assert_eq!(routes, 1, "committed legacy route must remain in v1");
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
}

#[test]
fn failed_v1_registry_rebuild_rolls_back_schema_and_rows() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("create current ledger");
    let terminal_adapter = adapter_binding(AdapterAxis::Terminal, "wezterm", "wezterm");
    install_exact_v1_registry_schema(&ledger, &[(&terminal_adapter, "active")]);
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch("CREATE TABLE adapter_registry_v1 (collision TEXT);")
        .expect("migration collision");
    drop(connection);
    drop(ledger);

    assert!(WorkLedger::open(temp.path()).is_err());
    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect rollback");
    assert_eq!(schema_version(&connection).expect("schema version"), 1);
    let rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM adapter_registry", [], |row| {
            row.get(0)
        })
        .expect("preserved row");
    assert_eq!(rows, 1);
    let old_schema: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'adapter_registry'",
            [],
            |row| row.get(0),
        )
        .expect("old schema");
    assert!(old_schema.contains("'terminal', 'provider'"));
    assert!(!old_schema.contains("'agent'"));
}

#[cfg(unix)]
#[test]
fn canonical_snapshot_apis_preserve_dry_run_and_idempotent_apply_boundaries() {
    let temp = TempDir::new().expect("temp");
    let state_dir = temp.path().join("state");

    let planned = plan_legacy_snapshot(&state_dir).expect("plan empty snapshot");
    assert!(!planned.applied);
    assert_eq!(planned.candidates, 0);
    assert!(!state_dir.exists());

    let first = apply_legacy_snapshot(&state_dir).expect("apply empty snapshot");
    assert!(first.applied);
    assert_eq!(first.inserted, 0);
    assert!(WorkLedger::path_at(&state_dir).is_file());

    let second = apply_legacy_snapshot(&state_dir).expect("reapply empty snapshot");
    assert!(second.applied);
    assert_eq!(second.inserted, 0);
    assert_eq!(second.plan_digest, first.plan_digest);
}

#[cfg(not(unix))]
#[test]
fn canonical_snapshot_apis_refuse_without_side_effects_on_unsupported_hosts() {
    let temp = TempDir::new().expect("temp");
    let state_dir = temp.path().join("state");
    let expected_reason = "work-ledger legacy import is currently supported only on Unix hosts";

    let plan_error = plan_legacy_snapshot(&state_dir).expect_err("plan must refuse");
    assert!(
        matches!(plan_error, WorkLedgerError::Refused(ref reason) if reason == expected_reason),
        "unexpected plan refusal: {plan_error}"
    );
    assert!(!state_dir.exists());

    let apply_error = apply_legacy_snapshot(&state_dir).expect_err("apply must refuse");
    assert!(
        matches!(apply_error, WorkLedgerError::Refused(ref reason) if reason == expected_reason),
        "unexpected apply refusal: {apply_error}"
    );
    assert!(!state_dir.exists());
}

#[test]
fn simulated_write_failure_rolls_back_the_import() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch(
            "CREATE TRIGGER simulate_write_failure
             BEFORE INSERT ON work_items
             BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END;",
        )
        .expect("trigger");
    drop(connection);
    assert!(ledger.import(&[sample_candidate()]).is_err());
    let status = ledger.status().expect("status");
    assert_eq!(status.work_items, 0);
    assert_eq!(status.imports, 0);
}

#[test]
fn truncated_database_fails_closed() {
    use std::fs::OpenOptions;

    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let length = fs::metadata(ledger.path()).expect("metadata").len();
    OpenOptions::new()
        .write(true)
        .open(ledger.path())
        .expect("open")
        .set_len(length / 2)
        .expect("truncate");
    assert!(WorkLedger::open_existing(temp.path()).is_err());
}

#[test]
fn repository_policy_is_revision_fenced_and_configurable() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let initial = RepoPolicy {
        repo: "generous-corp/forge".to_owned(),
        primary_platform: "macos".to_owned(),
        compatibility_mode: "independent".to_owned(),
        compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
        blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
        declared_dependency_lanes: vec!["linux".to_owned()],
        revision: 0,
    };
    let first = ledger.set_repo_policy(&initial, 0).expect("first");
    assert_eq!(first.revision, 1);
    assert!(first.compatibility_lane_may_block("linux", false));
    assert!(!first.compatibility_lane_may_block("windows", false));
    assert!(first.compatibility_lane_may_block("windows", true));
    assert!(!first.compatibility_lane_may_block("banana", true));
    let mut unknown_lane = initial.clone();
    unknown_lane.declared_dependency_lanes = vec!["banana".to_owned()];
    assert!(validate_repo_policy(&unknown_lane, 0).is_err());
    let mut unknown_compatibility = initial.clone();
    unknown_compatibility.compatibility_lanes = vec!["banana".to_owned()];
    unknown_compatibility.declared_dependency_lanes.clear();
    assert!(validate_repo_policy(&unknown_compatibility, 0).is_err());
    let mut unknown_primary = initial.clone();
    unknown_primary.primary_platform = "banana".to_owned();
    assert!(validate_repo_policy(&unknown_primary, 0).is_err());
    let mut whitespace_repo = initial.clone();
    whitespace_repo.repo = "owner/ repo".to_owned();
    assert!(validate_repo_policy(&whitespace_repo, 0).is_err());
    assert!(ledger.plan_repo_policy(&initial, 0).is_err());
    let mut current = initial.clone();
    current.revision = 1;
    assert_eq!(
        ledger
            .plan_repo_policy(&current, 1)
            .expect("current plan")
            .revision,
        2
    );
    assert!(ledger.set_repo_policy(&initial, 0).is_err());

    let changed = RepoPolicy {
        compatibility_mode: "blocking".to_owned(),
        blocking_rule: "all".to_owned(),
        revision: 1,
        ..first
    };
    let second = ledger.set_repo_policy(&changed, 1).expect("second");
    assert_eq!(second.revision, 2);
    assert_eq!(ledger.repo_policies().expect("policies"), vec![second]);
}
