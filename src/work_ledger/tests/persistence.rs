use super::*;

fn install_exact_v1_registry_schema(
    ledger: &WorkLedger,
    fixtures: &[(&AdapterBindingRecord, &str)],
) {
    let mut connection = ledger.connect_read_write().expect("connection");
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
    assert_eq!(schema_version(&connection).expect("schema version"), 2);
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
