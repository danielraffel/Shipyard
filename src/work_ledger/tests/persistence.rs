use super::*;

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
