use super::*;
#[cfg(unix)]
use crate::work_ledger::route::OpaqueRef;
use crate::work_ledger::{ProtectedObjectKind, digest};

fn ledger_with_work() -> (TempDir, WorkLedger, String) {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("work fixture");
    (temp, ledger, work_id)
}

#[cfg(not(unix))]
#[test]
fn unsupported_production_platform_uses_test_only_storage_backend() {
    use crate::work_ledger::protected_objects::PRODUCTION_PROTECTED_OBJECT_STORAGE_SUPPORTED;

    assert!(!std::hint::black_box(
        PRODUCTION_PROTECTED_OBJECT_STORAGE_SUPPORTED
    ));
    let (temp, ledger, work_id) = ledger_with_work();
    assert_eq!(
        ledger.status().expect("absent storage").protected_objects,
        0
    );
    let bytes = b"test-only protected object";
    ledger
        .put_protected_object(
            &work_id,
            ProtectedObjectKind::ProviderRequest,
            None,
            &digest(bytes),
            bytes,
        )
        .expect("test-only backend");
    assert_eq!(ledger.status().expect("test status").protected_objects, 1);
    assert!(temp.path().join("work-ledger/protected-objects").is_dir());
}

#[cfg(unix)]
#[test]
fn protected_object_round_trip_is_exact_idempotent_and_profile_addressable() {
    use std::os::unix::fs::PermissionsExt;

    let (temp, ledger, work_id) = ledger_with_work();
    let bytes = br#"{"argv":["agent","resume","GEN-43"]}"#;
    let expected_digest = digest(bytes);
    let profile_ref = OpaqueRef::derive("launch-profile", expected_digest.as_bytes())
        .as_str()
        .to_owned();

    let first = ledger
        .put_protected_object(
            &work_id,
            ProtectedObjectKind::LaunchProfile,
            Some(&profile_ref),
            &expected_digest,
            bytes,
        )
        .expect("put protected profile");
    let replay = ledger
        .put_protected_object(
            &work_id,
            ProtectedObjectKind::LaunchProfile,
            Some(&profile_ref),
            &expected_digest,
            bytes,
        )
        .expect("exact replay");
    assert_eq!(replay, first);
    let (opened, observed) = ledger
        .open_protected_object(&first.object_ref)
        .expect("open protected profile");
    assert_eq!(opened, first);
    assert_eq!(observed, bytes);

    let connection = ledger.connect_read_only().expect("inspect binding");
    let bound: String = connection
        .query_row(
            "SELECT object_ref FROM protected_objects
             WHERE work_item_id = ?1 AND profile_ref = ?2",
            params![work_id, profile_ref],
            |row| row.get(0),
        )
        .expect("profile binding");
    assert_eq!(bound, first.object_ref);
    assert_eq!(ledger.status().expect("status").protected_objects, 1);

    let directory = temp.path().join("work-ledger/protected-objects");
    assert_eq!(
        fs::metadata(&directory)
            .expect("directory mode")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let entries = fs::read_dir(&directory)
        .expect("object directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("object entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0]
            .metadata()
            .expect("object mode")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn protected_object_refuses_unreviewed_bytes_bounds_and_profile_smuggling() {
    let (_temp, ledger, work_id) = ledger_with_work();
    let bytes = b"provider request";
    let expected_digest = digest(bytes);
    let profile_ref = OpaqueRef::derive("launch-profile", expected_digest.as_bytes())
        .as_str()
        .to_owned();

    assert!(
        ledger
            .put_protected_object(
                &work_id,
                ProtectedObjectKind::ProviderRequest,
                None,
                &digest(b"different"),
                bytes,
            )
            .is_err()
    );
    assert!(
        ledger
            .put_protected_object(
                &work_id,
                ProtectedObjectKind::ProviderRequest,
                Some(&profile_ref),
                &expected_digest,
                bytes,
            )
            .is_err()
    );
    let oversized = vec![0_u8; 1_048_577];
    assert!(
        ledger
            .put_protected_object(
                &work_id,
                ProtectedObjectKind::AgentReceipt,
                None,
                &digest(&oversized),
                &oversized,
            )
            .is_err()
    );
    assert_eq!(ledger.status().expect("status").protected_objects, 0);
}

#[cfg(unix)]
#[test]
fn protected_object_open_rejects_symlink_and_hard_link_substitution() {
    use std::os::unix::fs::{MetadataExt, symlink};

    let (temp, ledger, work_id) = ledger_with_work();
    let bytes = b"provider receipt";
    let record = ledger
        .put_protected_object(
            &work_id,
            ProtectedObjectKind::ProviderReceipt,
            None,
            &digest(bytes),
            bytes,
        )
        .expect("put receipt");
    let directory = temp.path().join("work-ledger/protected-objects");
    let path = fs::read_dir(&directory)
        .expect("objects")
        .next()
        .expect("object entry")
        .expect("object")
        .path();
    let external = temp.path().join("external");
    fs::write(&external, bytes).expect("external file");
    fs::remove_file(&path).expect("remove stored object");
    symlink(&external, &path).expect("replace with symlink");
    assert!(ledger.open_protected_object(&record.object_ref).is_err());

    fs::remove_file(&path).expect("remove symlink");
    fs::hard_link(&external, &path).expect("replace with hard link");
    assert_eq!(fs::metadata(&path).expect("hard link metadata").nlink(), 2);
    assert!(ledger.open_protected_object(&record.object_ref).is_err());
}

#[cfg(unix)]
#[test]
fn pending_crash_file_is_reconciled_but_unsafe_pending_is_refused() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let (temp, ledger, _work_id) = ledger_with_work();
    let directory = temp.path().join("work-ledger/protected-objects");
    fs::create_dir(&directory).expect("object directory");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).expect("directory mode");
    let pending = directory.join(".pending-crash-fixture");
    fs::write(&pending, b"unpublished bytes").expect("pending fixture");
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o600)).expect("pending mode");
    assert_eq!(
        ledger
            .status()
            .expect("status repairs pending")
            .protected_objects,
        0
    );
    assert!(!pending.exists());

    let external = temp.path().join("external-pending");
    fs::write(&external, b"external").expect("external fixture");
    symlink(&external, &pending).expect("unsafe pending symlink");
    assert!(ledger.status().is_err());
    assert!(
        pending.symlink_metadata().is_ok(),
        "unsafe entry is never removed"
    );
}

#[cfg(unix)]
#[test]
fn final_before_database_crash_recovers_only_by_exact_replay() {
    use crate::work_ledger::protected_objects::{derive_object_ref, storage_name};
    use std::os::unix::fs::PermissionsExt;

    let (temp, ledger, work_id) = ledger_with_work();
    let bytes = b"crash-bound provider request";
    let expected_digest = digest(bytes);
    let object_ref = derive_object_ref(
        &work_id,
        ProtectedObjectKind::ProviderRequest,
        None,
        &expected_digest,
        bytes.len(),
    );
    let directory = temp.path().join("work-ledger/protected-objects");
    fs::create_dir(&directory).expect("crash object directory");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).expect("directory mode");
    let final_path = directory.join(storage_name(&object_ref).expect("storage name"));
    fs::write(&final_path, bytes).expect("published final before DB commit");
    fs::set_permissions(&final_path, fs::Permissions::from_mode(0o600)).expect("object mode");
    assert!(
        ledger.status().is_err(),
        "unregistered final is quarantined"
    );
    drop(ledger);

    let reopened = WorkLedger::open(temp.path()).expect("reopen permits exact recovery");
    let recovered = reopened
        .put_protected_object(
            &work_id,
            ProtectedObjectKind::ProviderRequest,
            None,
            &expected_digest,
            bytes,
        )
        .expect("exact replay registers existing final");
    assert_eq!(recovered.object_ref, object_ref);
    assert_eq!(
        reopened
            .status()
            .expect("healthy after replay")
            .protected_objects,
        1
    );
}

#[cfg(unix)]
#[test]
fn missing_registered_object_fails_exact_read_and_status() {
    let (temp, ledger, work_id) = ledger_with_work();
    let bytes = b"registered receipt";
    let record = ledger
        .put_protected_object(
            &work_id,
            ProtectedObjectKind::ProviderReceipt,
            None,
            &digest(bytes),
            bytes,
        )
        .expect("registered object");
    let path = fs::read_dir(temp.path().join("work-ledger/protected-objects"))
        .expect("objects")
        .next()
        .expect("entry")
        .expect("object")
        .path();
    fs::remove_file(path).expect("simulate missing authority");
    assert!(ledger.open_protected_object(&record.object_ref).is_err());
    assert!(ledger.status().is_err());
    drop(ledger);
    let reopened = WorkLedger::open_existing(temp.path())
        .expect("constructor remains inspection-capable")
        .expect("ledger exists");
    assert!(reopened.open_protected_object(&record.object_ref).is_err());
}

#[test]
fn protected_object_aggregate_bound_is_a_database_invariant() {
    let (_temp, ledger, work_id) = ledger_with_work();
    let connection = ledger.connect_read_write().expect("connection");
    for index in 0..16 {
        let identity = format!("capacity-{index}");
        let object_ref = opaque_ref("po", &identity);
        let storage_name = format!("object-{}.blob", &object_ref[3..]);
        connection
            .execute(
                "INSERT INTO protected_objects
                 (object_ref, work_item_id, kind, storage_name, content_digest,
                  byte_length, created_at)
                 VALUES (?1, ?2, 'provider_request', ?3, ?4, 1048576, ?5)",
                params![
                    object_ref,
                    work_id,
                    storage_name,
                    digest(identity.as_bytes()),
                    "2026-08-28T00:00:00Z",
                ],
            )
            .expect("within aggregate capacity");
    }
    let identity = "capacity-overflow";
    let object_ref = opaque_ref("po", identity);
    let storage_name = format!("object-{}.blob", &object_ref[3..]);
    assert!(
        connection
            .execute(
                "INSERT INTO protected_objects
                 (object_ref, work_item_id, kind, storage_name, content_digest,
                  byte_length, created_at)
                 VALUES (?1, ?2, 'provider_request', ?3, ?4, 1, ?5)",
                params![
                    object_ref,
                    work_id,
                    storage_name,
                    digest(identity.as_bytes()),
                    "2026-08-28T00:00:00Z",
                ],
            )
            .is_err()
    );
}
