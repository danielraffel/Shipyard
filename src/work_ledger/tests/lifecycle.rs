use super::*;

#[test]
fn repeated_import_is_idempotent_and_redacted() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(ledger.path().parent().expect("ledger dir"))
                .expect("ledger dir metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let candidate = sample_candidate();
    assert_eq!(
        ledger
            .import(std::slice::from_ref(&candidate))
            .expect("first")
            .inserted,
        1
    );
    let second = ledger
        .import(std::slice::from_ref(&candidate))
        .expect("second");
    assert_eq!(second.inserted, 0);
    assert_eq!(second.updated, 0);
    assert_eq!(second.unchanged, 1);
    let plan = ledger
        .plan_import(std::slice::from_ref(&candidate))
        .expect("read-only plan");
    assert_eq!(plan.inserted, 0);
    assert_eq!(plan.updated, 0);
    assert_eq!(plan.unchanged, 1);
    let mut changed = candidate;
    changed.lane = Some("focused-refresh".to_owned());
    changed.content_digest = digest(b"changed content");
    let refreshed = ledger.import(&[changed]).expect("refresh");
    assert_eq!(refreshed.inserted, 0);
    assert_eq!(refreshed.updated, 1);
    assert_eq!(refreshed.unchanged, 0);
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{}", ledger.path().display(), suffix));
        if !path.exists() {
            continue;
        }
        let bytes = fs::read(&path).expect("ledger bytes");
        let haystack = String::from_utf8_lossy(&bytes);
        for forbidden in [
            "secret-route",
            "secret-account",
            "resume-private-id",
            "owner-private-id",
        ] {
            assert!(
                !haystack.contains(forbidden),
                "persisted {forbidden} in {}",
                path.display()
            );
        }
    }
}

#[test]
fn projection_bound_legacy_row_cannot_be_refreshed() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    let repository = candidate.repo.clone().expect("repository");
    let head = candidate.head_sha.clone().expect("head");
    ledger
        .import(std::slice::from_ref(&candidate))
        .expect("initial import");
    ledger
        .bind_workstream_projection(
            &work_id,
            "GEN-14",
            &digest(b"projection plan"),
            1,
            1,
            1,
            1,
            "github.com",
            "R_test_repository",
            &repository,
            &head,
        )
        .expect("projection binding");
    let mut changed = candidate;
    changed.lane = Some("stale-legacy-refresh".to_owned());
    changed.content_digest = digest(b"changed after projection binding");
    let planned = ledger
        .plan_import(std::slice::from_ref(&changed))
        .expect_err("projection-bound legacy dry-run must refuse");
    assert!(planned.to_string().contains("projection state"));
    let error = ledger
        .import(&[changed])
        .expect_err("projection-bound legacy row must be immutable");
    assert!(error.to_string().contains("projection state"));
}

#[test]
fn transition_and_wake_commit_together_with_generation_fence() {
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
    let (route, agent_adapter) = sample_route(&work_id, 5);
    let mut wrong_owner = route.clone();
    wrong_owner.owner_ref = opaque_ref("owner", "different-owner");
    wrong_owner.envelope_integrity = wrong_owner.compute_envelope_integrity();
    assert!(ledger.register_route(&wrong_owner).is_err());
    assert!(ledger.register_route(&route).is_err());
    ledger
        .register_adapter(&agent_adapter)
        .expect("register Codex adapter policy");
    ledger.register_route(&route).expect("register route");
    let bad = WakeIntent::new(&work_id, 99, 3, route.route_ref.clone(), digest(b"payload"))
        .expect("bad generation wake");
    assert!(
        ledger
            .transition_with_wake(&work_id, 5, 3, LifecycleState::Dispatching, Some(&bad))
            .is_err()
    );
    let connection = ledger.connect_read_only().expect("connection");
    let phase: String = connection
        .query_row(
            "SELECT phase FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("phase");
    assert_eq!(phase, "actionable");
    assert_eq!(
        count_where(&connection, "outbox", "state", "pending").expect("count"),
        0
    );

    let wake =
        WakeIntent::new(&work_id, 6, 3, route.route_ref.clone(), digest(b"payload")).expect("wake");
    assert_eq!(
        wake,
        WakeIntent::new(&work_id, 6, 3, route.route_ref.clone(), digest(b"payload"),)
            .expect("same wake")
    );
    let mut forged = wake.clone();
    forged.wake_id = opaque_ref("wake", "forged");
    assert!(
        ledger
            .transition_with_wake(&work_id, 5, 3, LifecycleState::Dispatching, Some(&forged),)
            .is_err()
    );
    ledger
        .transition_with_wake(&work_id, 5, 3, LifecycleState::Dispatching, Some(&wake))
        .expect("transition");
    let mut refreshed = sample_candidate();
    refreshed.phase = "resolved".to_owned();
    refreshed.content_digest = digest(b"changed after wake");
    assert!(ledger.import(&[refreshed]).is_err());
    assert!(
        ledger
            .transition_with_wake(&work_id, 5, 3, LifecycleState::Terminal, None)
            .is_err()
    );
    assert_eq!(ledger.status().expect("status").pending_wakes, 1);
    let connection = ledger.connect_read_only().expect("connection");
    let events: u64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("events");
    assert_eq!(events, 6, "import plus five legal transitions");
}

#[test]
fn registered_route_requires_exact_active_adapter_registry_record() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("import");
    let (route, adapters) = sample_registered_route(&work_id);
    let terminal_adapter = &adapters[0];
    let agent_adapter = &adapters[1];
    assert!(ledger.register_route(&route).is_err());
    ledger
        .register_adapter(terminal_adapter)
        .expect("register terminal adapter");
    assert!(
        ledger.register_route(&route).is_err(),
        "named Qwen must not register without its exact active agent adapter"
    );
    ledger
        .register_adapter(agent_adapter)
        .expect("register Qwen agent adapter");
    ledger.register_route(&route).expect("register route");

    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute(
            "UPDATE adapter_registry SET implementation_digest = ?1 WHERE registry_ref = ?2",
            params![
                digest(b"drifted Qwen implementation"),
                agent_adapter.registry_ref.as_str()
            ],
        )
        .expect("drift adapter");
    let transaction = connection
        .unchecked_transaction()
        .expect("read transaction");
    assert!(
        validated_route_exists(
            &transaction,
            &route.route_ref,
            &work_id,
            route.work_generation,
            route.owner_generation,
        )
        .is_err()
    );
    transaction
        .execute(
            "UPDATE adapter_registry SET implementation_digest = ?1, state = 'retired'
             WHERE registry_ref = ?2",
            params![
                agent_adapter.implementation_sha256.as_str(),
                agent_adapter.registry_ref.as_str()
            ],
        )
        .expect("retire exact agent adapter");
    assert!(
        validated_route_exists(
            &transaction,
            &route.route_ref,
            &work_id,
            route.work_generation,
            route.owner_generation,
        )
        .is_err()
    );
}

#[test]
fn claude_route_requires_an_explicit_active_agent_policy_record() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("import");
    let (mut route, _) = sample_route(&work_id, 1);
    let session = match &route.provenance.agent.route {
        AgentRoute::Codex { session } => session.clone(),
        _ => panic!("Codex fixture"),
    };
    let claude_adapter = adapter_binding(AdapterAxis::Agent, "claude", "claude");
    route.provenance.agent =
        AgentRouteRecord::new(claude_adapter.clone(), AgentRoute::Claude { session })
            .expect("Claude route");
    route.provenance.integrity_sha256 = route
        .provenance
        .recompute_integrity()
        .expect("route integrity");
    route.envelope_integrity = route.compute_envelope_integrity();

    assert!(ledger.register_route(&route).is_err());
    ledger
        .register_adapter(&claude_adapter)
        .expect("register explicit Claude policy");
    ledger
        .register_route(&route)
        .expect("register exact Claude route");
}

#[test]
fn opaque_boundaries_and_incomplete_continuations_fail_closed() {
    assert!(
        WakeIntent::new(
            "raw-work",
            1,
            1,
            "raw-route".to_owned(),
            "raw-digest".to_owned()
        )
        .is_err()
    );
    assert!(
        ContinuationSet::new("raw-success".to_owned(), None, digest(b"failure"), None,).is_err()
    );
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("import");
    assert!(
        ledger
            .transition_with_wake(&work_id, 1, 3, LifecycleState::Published, None)
            .is_err()
    );
    let connection = ledger.connect_read_only().expect("connection");
    let phase: String = connection
        .query_row(
            "SELECT phase FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("phase");
    assert_eq!(phase, "shadow_imported");
}

#[test]
fn continuation_revision_resets_both_outcome_states() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("import");
    ledger
        .record_continuations(
            &work_id,
            0,
            &ContinuationSet::new(digest(b"success-1"), None, digest(b"failure-1"), None)
                .expect("first"),
        )
        .expect("record first");
    let mut changed_import = sample_candidate();
    changed_import.content_digest = digest(b"changed after native contract");
    assert!(ledger.plan_import(&[changed_import.clone()]).is_err());
    assert!(ledger.import(&[changed_import]).is_err());
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute(
            "UPDATE continuation_contracts SET success_state = 'completed', failure_state = 'failed'
             WHERE work_item_id = ?1",
            [&work_id],
        )
        .expect("simulate progress");
    drop(connection);
    ledger
        .record_continuations(
            &work_id,
            1,
            &ContinuationSet::new(digest(b"success-2"), None, digest(b"failure-2"), None)
                .expect("second"),
        )
        .expect("revise");
    let connection = ledger.connect_read_only().expect("connection");
    let states: (String, String) = connection
        .query_row(
            "SELECT success_state, failure_state FROM continuation_contracts
             WHERE work_item_id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("states");
    assert_eq!(states, ("pending".to_owned(), "pending".to_owned()));
}

#[test]
fn audit_event_failure_rolls_back_transition_and_outbox() {
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
        .expect("contract");
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_transition_event
             BEFORE INSERT ON events WHEN NEW.kind = 'lifecycle_transition'
             BEGIN SELECT RAISE(ABORT, 'event failure'); END;",
        )
        .expect("trigger");
    drop(connection);
    assert!(
        ledger
            .transition_with_wake(&work_id, 1, 3, LifecycleState::Published, None)
            .is_err()
    );
    let connection = ledger.connect_read_only().expect("connection");
    let state: (String, u64) = connection
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("state");
    assert_eq!(state, ("shadow_imported".to_owned(), 1));
    assert_eq!(
        count_where(&connection, "outbox", "state", "pending").expect("outbox"),
        0
    );
}
