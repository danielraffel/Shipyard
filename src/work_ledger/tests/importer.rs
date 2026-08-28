use super::*;

#[test]
fn queue_projection_reads_exact_identity_from_each_schema_location() {
    let request = candidate(
        "queue_request",
        opaque_ref("src", "request"),
        digest(b"request"),
        &serde_json::json!({
            "request": {
                "type": "ship",
                "repo": "Owner/Repo",
                "pr": 7,
                "base_branch": "main",
                "sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        }),
    );
    assert_eq!(request.repo.as_deref(), Some("owner/repo"));
    assert_eq!(request.pr, Some(7));
    assert_eq!(request.base_ref.as_deref(), Some("main"));
    assert_eq!(
        request.head_sha.as_deref(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );

    let outcome = candidate(
        "queue_outcome",
        opaque_ref("src", "outcome"),
        digest(b"outcome"),
        &serde_json::json!({
            "type": "ship",
            "pr": 8,
            "ship_state": {
                "repo": "Owner/Repo",
                "base_branch": "release",
                "head_sha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            }
        }),
    );
    assert_eq!(outcome.repo.as_deref(), Some("owner/repo"));
    assert_eq!(outcome.pr, Some(8));
    assert_eq!(outcome.base_ref.as_deref(), Some("release"));
    assert_eq!(
        outcome.head_sha.as_deref(),
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    );
}

#[test]
fn logical_ship_identity_ignores_mirror_storage_path() {
    let value = serde_json::json!({
        "repo": "owner/repo",
        "pr": 9,
        "head_sha": "cccccccccccccccccccccccccccccccccccccccc"
    });
    let first = candidate(
        "ship_state",
        opaque_ref("src", "legacy mirror"),
        digest(b"same"),
        &value,
    );
    let second = candidate(
        "ship_state",
        opaque_ref("src", "scoped active"),
        digest(b"same"),
        &value,
    );
    assert_eq!(first.work_id, second.work_id);
}

#[test]
fn recovery_status_and_resume_truth_are_preserved_conservatively() {
    let recovery = candidate(
        "recovery",
        opaque_ref("src", "recovery"),
        digest(b"recovery"),
        &serde_json::json!({
            "repo": "owner/repo",
            "pr": 10,
            "head_sha": "dddddddddddddddddddddddddddddddddddddddd",
            "status": "needs_agent"
        }),
    );
    assert_eq!(recovery.phase, "shadow_imported");

    let mut resume = sample_candidate();
    resume.phase = "resolved".to_owned();
    let resolved = candidate(
        "resume_record",
        resume.source_ref,
        resume.content_digest,
        &serde_json::json!({"phase": "resolved"}),
    );
    assert_eq!(resolved.continuation_truth, "unknown");

    let recovery_record = serde_json::json!({
        "request": {
            "id": "stable-recovery-id",
            "repo": "owner/repo",
            "pr": 10,
            "head_sha": "dddddddddddddddddddddddddddddddddddddddd"
        },
        "receipt": {"status": "escalated"}
    });
    let active = candidate(
        "recovery",
        opaque_ref("src", "active"),
        digest(b"active"),
        &recovery_record,
    );
    let archived = candidate(
        "recovery",
        opaque_ref("src", "archive"),
        digest(b"archive"),
        &recovery_record,
    );
    assert_eq!(active.work_id, archived.work_id);
}

#[test]
fn terminal_only_route_is_preserved_as_an_opaque_reference() {
    let projected = candidate(
        "resume_record",
        opaque_ref("src", "terminal-only"),
        digest(b"terminal-only"),
        &serde_json::json!({
            "terminal_adapter": {"kind": "herd_r", "route_id": "private-terminal-route"},
            "phase": "recorded"
        }),
    );
    assert_eq!(
        projected.repair_route_ref,
        Some(opaque_ref("route", "private-terminal-route"))
    );
}

#[test]
fn syntactically_valid_but_schema_invalid_legacy_record_fails_closed() {
    assert!(validate_legacy_record("ship_state", "src_test", &serde_json::json!({})).is_err());
    let mut invalid_ship = crate::ship_state::ShipState::new(
        1,
        "owner/repo",
        "head",
        "main",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "policy",
    );
    invalid_ship.pr = 0;
    assert!(
        validate_legacy_record(
            "ship_state",
            "src_test",
            &serde_json::to_value(invalid_ship).expect("ship json"),
        )
        .is_err()
    );
    assert!(
        validate_legacy_record(
            "resume_record",
            "src_test",
            &serde_json::json!({"schema_version": 99})
        )
        .is_err()
    );
    let malformed_route = serde_json::json!({
        "schema_version": 1,
        "resume_id": "resume",
        "terminal_handoff_key": "handoff",
        "repo": "owner/repo",
        "base": "main",
        "pr_number": 9,
        "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "routing_disposition": "original_owner",
        "terminal_adapter": {"kind": "cmux", "route_id": "route", "unknown": true},
        "dispatch_enabled": false,
        "phase": "recorded",
        "created_at": "2026-08-28T00:00:00Z",
        "updated_at": "2026-08-28T00:00:00Z"
    });
    assert!(validate_legacy_record("resume_record", "src_test", &malformed_route).is_err());
    let mut state = crate::ship_state::ShipState::new(
        10,
        "owner/repo",
        "head",
        "main",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "policy",
    );
    state.schema_version += 1;
    let invalid_outcome =
        crate::queue_request::QueuedExecutionOutcome::ship("job", 10, state, false);
    assert!(
        validate_legacy_record(
            "queue_outcome",
            "src_test",
            &serde_json::to_value(invalid_outcome).expect("outcome json"),
        )
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn symlinked_steward_ledger_is_rejected() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp");
    let outside = temp.path().join("outside.json");
    fs::write(&outside, "{}").expect("outside");
    symlink(&outside, temp.path().join("merge-steward.json")).expect("symlink");
    assert!(scan_legacy(temp.path()).is_err());
}

#[cfg(unix)]
#[test]
fn symlinked_scan_root_is_rejected() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp");
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).expect("outside");
    symlink(&outside, temp.path().join("ship")).expect("symlink");
    assert!(scan_legacy(temp.path()).is_err());
}

#[cfg(unix)]
#[test]
fn dangling_recovery_root_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp");
    fs::create_dir(temp.path().join("merge-steward")).expect("steward dir");
    symlink(
        temp.path().join("absent-recovery"),
        temp.path().join("merge-steward").join("recovery"),
    )
    .expect("dangling symlink");
    assert!(scan_legacy(temp.path()).is_err());
}

#[cfg(unix)]
#[test]
fn symlinked_state_directory_is_rejected_before_steward_read() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp");
    let real = temp.path().join("real");
    fs::create_dir(&real).expect("real");
    fs::write(real.join("merge-steward.json"), "{}").expect("steward");
    let alias = temp.path().join("alias");
    symlink(&real, &alias).expect("alias");
    assert!(scan_legacy(&alias).is_err());
}

#[cfg(unix)]
#[test]
fn scoped_ship_precedence_and_collision_marker_match_authoritative_store() {
    use crate::ship_state::{ShipState, ShipStateStore};
    use chrono::Duration as ChronoDuration;

    let temp = TempDir::new().expect("temp");
    let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
    let scoped = ShipState::new(
        42,
        "owner/repo",
        "head",
        "main",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "policy",
    );
    store.save(&scoped).expect("save scoped");
    let legacy_path = temp.path().join("ship").join("42.json");
    let mut newer = scoped.clone();
    newer.updated_at += ChronoDuration::seconds(1);
    fs::write(
        &legacy_path,
        format!("{}\n", serde_json::to_string_pretty(&newer).expect("json")),
    )
    .expect("newer legacy");
    let selected = scan_legacy(temp.path()).expect("scan newer");
    let ship = selected
        .iter()
        .find(|candidate| candidate.kind == "ship_state")
        .expect("ship");
    assert_eq!(
        ship.content_digest,
        digest(&fs::read(&legacy_path).expect("legacy"))
    );

    fs::write(
        temp.path().join("ship").join("42.scoped-collision"),
        b"repository-scoped\n",
    )
    .expect("collision marker");
    let selected = scan_legacy(temp.path()).expect("scan collision");
    let ship = selected
        .iter()
        .find(|candidate| candidate.kind == "ship_state")
        .expect("scoped ship");
    assert_ne!(
        ship.content_digest,
        digest(&fs::read(&legacy_path).expect("legacy"))
    );
}

#[cfg(unix)]
#[test]
fn queue_absence_claim_is_discovered_and_wrong_recovery_archive_shard_refused() {
    use crate::queue_absent_recovery::{
        QueueAbsentRecoveryRecord, QueueAbsentRecoveryStatus,
        RECOVERY_SCHEMA_VERSION as QUEUE_RECOVERY_VERSION, recovery_record_path,
    };
    use crate::recovery_worker::{
        EnqueueOutcome, RecoveryFailureFact, RecoveryRequest, RecoveryRequiredCheck, RecoveryStore,
    };

    let temp = TempDir::new().expect("temp");
    let queue_root = temp.path().join("queue").join("recovery");
    fs::create_dir_all(&queue_root).expect("queue recovery");
    let claim = QueueAbsentRecoveryRecord {
        schema_version: QUEUE_RECOVERY_VERSION,
        repo: "owner/repo".to_owned(),
        pr: 7,
        attempt: 1,
        branch: "head".to_owned(),
        base_branch: "main".to_owned(),
        head_sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
        source_job_id: "source".to_owned(),
        replacement_job_id: "replacement".to_owned(),
        generation: "generation".to_owned(),
        status: QueueAbsentRecoveryStatus::NeedsAgent,
        detail: None,
        updated_at: Utc::now(),
    };
    fs::write(
        recovery_record_path(temp.path(), &claim.repo, claim.pr),
        serde_json::to_vec(&claim).expect("claim json"),
    )
    .expect("claim");
    assert!(
        scan_legacy(temp.path())
            .expect("scan claim")
            .iter()
            .any(|candidate| {
                candidate.kind == "recovery" && candidate.repo.as_deref() == Some("owner/repo")
            })
    );

    let worker_root = temp.path().join("merge-steward").join("recovery");
    let store = RecoveryStore::new(&worker_root).expect("recovery store");
    let request = RecoveryRequest::new(
        "owner/repo",
        8,
        "main",
        "cccccccccccccccccccccccccccccccccccccccc",
        "failure",
        "required check failed",
        vec![RecoveryRequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
        }],
        vec![RecoveryFailureFact::RequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
            conclusion: "FAILURE".to_owned(),
            run_id: None,
        }],
        "policy",
        "config",
    )
    .expect("request");
    assert_eq!(
        store.enqueue(request.clone()).expect("enqueue"),
        EnqueueOutcome::Created
    );
    let mut malformed = serde_json::to_value(
        store
            .get(&request.id)
            .expect("get record")
            .expect("stored record"),
    )
    .expect("record json");
    malformed["request"]["id"] = serde_json::json!("é");
    assert!(validate_legacy_record("recovery", "src_test", &malformed).is_err());
    let active = worker_root.join(format!("{}.json", request.id));
    let wrong_active = worker_root.join("wrong.json");
    fs::write(&wrong_active, fs::read(&active).expect("active record"))
        .expect("wrong active filename");
    assert!(scan_legacy(temp.path()).is_err());
    fs::remove_file(wrong_active).expect("remove wrong active filename");
    let wrong = worker_root.join("archive").join("zz");
    fs::create_dir_all(&wrong).expect("wrong shard");
    fs::write(
        wrong.join(format!("{}.json", request.id)),
        fs::read(active).expect("active record"),
    )
    .expect("wrong archive");
    assert!(scan_legacy(temp.path()).is_err());
}

#[cfg(unix)]
#[test]
fn symlinked_ledger_directory_and_database_are_rejected() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp");
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).expect("outside");
    symlink(&outside, temp.path().join("work-ledger")).expect("directory symlink");
    assert!(WorkLedger::open_existing(temp.path()).is_err());
    assert!(WorkLedger::open(temp.path()).is_err());

    fs::remove_file(temp.path().join("work-ledger")).expect("remove symlink");
    fs::create_dir(temp.path().join("work-ledger")).expect("ledger dir");
    let outside_db = temp.path().join("outside.sqlite3");
    fs::write(&outside_db, "not a database").expect("outside db");
    symlink(
        &outside_db,
        temp.path().join("work-ledger").join(DATABASE_NAME),
    )
    .expect("database symlink");
    assert!(WorkLedger::open(temp.path()).is_err());

    let swapped = TempDir::new().expect("swapped temp");
    let ledger = WorkLedger::open(swapped.path()).expect("ledger");
    let real = swapped.path().join("real.sqlite3");
    fs::rename(ledger.path(), &real).expect("move database");
    symlink(&real, ledger.path()).expect("swap database for symlink");
    assert!(ledger.status().is_err());
}

#[cfg(unix)]
#[test]
fn existing_ledger_refuses_insecure_directory_or_database_modes() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    fs::set_permissions(ledger.path(), fs::Permissions::from_mode(0o644)).expect("database mode");
    assert!(WorkLedger::open_existing(temp.path()).is_err());
    assert!(WorkLedger::open(temp.path()).is_err());
    fs::set_permissions(ledger.path(), fs::Permissions::from_mode(0o600))
        .expect("restore database");
    fs::set_permissions(
        ledger.path().parent().expect("ledger directory"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("directory mode");
    assert!(WorkLedger::open_existing(temp.path()).is_err());
    assert!(WorkLedger::open(temp.path()).is_err());
}

#[cfg(unix)]
#[test]
fn newest_archived_ship_snapshot_wins_across_directory_layouts() {
    use crate::ship_state::ShipState;
    use chrono::Duration as ChronoDuration;

    let temp = TempDir::new().expect("temp");
    let archive = temp.path().join("ship").join("archive");
    let scoped_archive = archive.join(crate::ship_state::repository_key("owner/repo"));
    fs::create_dir_all(&scoped_archive).expect("archive dirs");
    let older = ShipState::new(
        51,
        "owner/repo",
        "head",
        "main",
        "dddddddddddddddddddddddddddddddddddddddd",
        "policy",
    );
    let mut newer = older.clone();
    newer.updated_at += ChronoDuration::seconds(2);
    fs::write(
        archive.join("51-20260827T120000Z.json"),
        serde_json::to_vec(&older).expect("older json"),
    )
    .expect("older");
    let newer_path = scoped_archive.join("51-20260827T120001Z.json");
    fs::write(&newer_path, serde_json::to_vec(&newer).expect("newer json")).expect("newer");
    let selected = scan_legacy(temp.path()).expect("scan");
    let ship = selected
        .iter()
        .find(|candidate| candidate.kind == "ship_state")
        .expect("ship");
    assert_eq!(
        ship.content_digest,
        digest(&fs::read(newer_path).expect("newer bytes"))
    );
}

#[test]
fn malformed_steward_lifecycle_map_is_rejected() {
    let temp = TempDir::new().expect("temp");
    fs::write(
        temp.path().join("merge-steward.json"),
        r#"{"terminal_handoffs": []}"#,
    )
    .expect("ledger");
    assert!(scan_legacy(temp.path()).is_err());

    fs::write(temp.path().join("merge-steward.json"), r"[]").expect("scalar ledger");
    assert!(scan_legacy(temp.path()).is_err());

    fs::write(
        temp.path().join("merge-steward.json"),
        serde_json::to_vec(&serde_json::json!({
            "terminal_handoffs": {
                "map-key": {
                    "dedupe_key": "embedded-key",
                    "repo": "owner/repo",
                    "base": "main",
                    "pr_number": 9,
                    "head_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "outcome": "success_continuation",
                    "phase": "recorded"
                }
            }
        }))
        .expect("steward json"),
    )
    .expect("mismatched ledger");
    assert!(scan_legacy(temp.path()).is_err());
}

#[test]
fn newer_schema_and_corruption_fail_closed() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .pragma_update(None, "journal_mode", "DELETE")
        .expect("delete journal");
    connection
        .pragma_update(None, "user_version", 99)
        .expect("version");
    drop(connection);
    assert!(matches!(
        WorkLedger::open_existing(temp.path()),
        Err(WorkLedgerError::UnsupportedSchema(99))
    ));
    assert!(matches!(
        WorkLedger::open(temp.path()),
        Err(WorkLedgerError::UnsupportedSchema(99))
    ));
    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect database");
    let journal: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode");
    assert_eq!(journal, "delete");
    assert!(
        !WorkLedger::path_at(temp.path())
            .with_extension("sqlite3-wal")
            .exists()
    );

    let corrupt = TempDir::new().expect("temp");
    let path = WorkLedger::path_at(corrupt.path());
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    fs::write(path, b"not sqlite").expect("write");
    assert!(WorkLedger::open_existing(corrupt.path()).is_err());
}

#[test]
fn dry_run_is_byte_stable_and_does_not_create_database() {
    let temp = TempDir::new().expect("temp");
    let candidates = vec![sample_candidate()];
    let first = serde_json::to_vec(&dry_run_report(&candidates)).expect("json");
    let second = serde_json::to_vec(&dry_run_report(&candidates)).expect("json");
    assert_eq!(first, second);
    assert!(!WorkLedger::path_at(temp.path()).exists());
}

#[cfg(unix)]
#[test]
fn missing_state_root_is_an_empty_noncreating_scan() {
    let temp = TempDir::new().expect("temp");
    let missing = temp.path().join("not-created");
    assert!(scan_legacy(&missing).expect("empty scan").is_empty());
    assert!(!missing.exists());
}

#[cfg(unix)]
#[test]
fn malformed_legacy_json_fails_the_whole_scan_closed() {
    let temp = TempDir::new().expect("temp");
    let requests = temp.path().join("queue").join("requests");
    fs::create_dir_all(&requests).expect("dir");
    fs::write(requests.join("bad.json"), b"{").expect("write");
    assert!(matches!(
        scan_legacy(temp.path()),
        Err(WorkLedgerError::Json { .. })
    ));
}

#[cfg(unix)]
#[test]
fn nested_queue_backups_are_not_authoritative_import_inputs() {
    let temp = TempDir::new().expect("temp");
    let nested = temp.path().join("queue").join("requests").join("backup");
    fs::create_dir_all(&nested).expect("nested");
    fs::write(nested.join("bad.json"), b"{").expect("write");
    assert!(scan_legacy(temp.path()).expect("scan").is_empty());
}

#[cfg(unix)]
#[test]
fn queue_root_filename_must_match_embedded_job_identity() {
    let temp = TempDir::new().expect("temp");
    let outcomes = temp.path().join("queue").join("outcomes");
    fs::create_dir_all(&outcomes).expect("outcomes");
    let state = crate::ship_state::ShipState::new(
        10,
        "owner/repo",
        "head",
        "main",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "policy",
    );
    let outcome = crate::queue_request::QueuedExecutionOutcome::ship("right", 10, state, false);
    fs::write(
        outcomes.join("wrong.json"),
        serde_json::to_vec(&outcome).expect("outcome json"),
    )
    .expect("write outcome");
    assert!(scan_legacy(temp.path()).is_err());
}

#[cfg(unix)]
#[test]
fn ship_state_path_must_match_embedded_repository_and_pr() {
    let temp = TempDir::new().expect("temp");
    let ship = temp.path().join("ship");
    fs::create_dir_all(&ship).expect("ship");
    let state = crate::ship_state::ShipState::new(
        10,
        "owner/repo",
        "head",
        "main",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "policy",
    );
    fs::write(
        ship.join("backup.json"),
        serde_json::to_vec(&state).expect("ship json"),
    )
    .expect("write ship");
    assert!(scan_legacy(temp.path()).is_err());
}
