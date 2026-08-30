use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;

use super::*;

#[test]
fn non_file_existing_lock_refuses_without_mutation_or_reclamation() {
    let fixture = Fixture::new();
    let state = fixture.state();
    let lock = state.join("fleet-auth-support.lock");
    std::fs::create_dir(&lock).expect("non-file lock");

    assert!(!fixture.run(RunOptions::default()).success());
    assert!(lock.is_dir());
    assert!(!fixture.helper.exists());
    assert!(!fixture.wrapper.exists());
    assert!(
        !fixture
            .wrapper
            .with_file_name("ghapp.shipyard-context.json")
            .exists()
    );
    assert!(!state.join("fleet-auth-support.transaction").exists());
    assert!(
        !std::fs::read_dir(&state)
            .expect("state entries")
            .any(|entry| {
                entry
                    .expect("state entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".fleet-auth-support.lock.")
            })
    );
}

#[test]
fn malformed_symlinked_and_live_legacy_pid_locks_are_preserved() {
    for scenario in ["malformed", "symlink", "live"] {
        let fixture = Fixture::new();
        let lock = fixture.state().join("fleet-auth-support.lock");
        let pid = lock.join("pid");
        std::fs::create_dir(&lock).expect("legacy lock");
        match scenario {
            "malformed" => std::fs::write(&pid, b"not-a-pid\n").expect("malformed pid"),
            "symlink" => {
                let target = fixture.root.path().join("foreign-pid");
                std::fs::write(&target, b"99999999\n").expect("foreign pid target");
                symlink(&target, &pid).expect("pid symlink");
            }
            "live" => {
                std::fs::write(&pid, format!("{}\n", std::process::id())).expect("live pid");
            }
            _ => unreachable!(),
        }
        if scenario != "symlink" {
            std::fs::set_permissions(&pid, std::fs::Permissions::from_mode(0o600))
                .expect("pid mode");
        }

        assert!(
            !fixture.run(RunOptions::default()).success(),
            "{scenario} legacy pid must refuse"
        );
        assert!(lock.is_dir(), "{scenario} lock must be preserved");
        assert!(!fixture.helper.exists());
        assert!(!fixture.wrapper.exists());
    }
}

#[test]
fn invalid_advisory_guard_types_and_mode_refuse_before_artifact_mutation() {
    for scenario in ["directory", "symlink", "mode"] {
        let fixture = Fixture::new();
        let state = fixture.state();
        let guard = state.join("fleet-auth-support.guard");
        match scenario {
            "directory" => std::fs::create_dir(&guard).expect("guard directory"),
            "symlink" => {
                let target = fixture.root.path().join("foreign-guard");
                std::fs::write(&target, b"").expect("foreign guard");
                symlink(&target, &guard).expect("guard symlink");
            }
            "mode" => {
                std::fs::write(&guard, b"").expect("guard file");
                std::fs::set_permissions(&guard, std::fs::Permissions::from_mode(0o644))
                    .expect("guard mode");
            }
            _ => unreachable!(),
        }

        assert!(
            !fixture.run(RunOptions::default()).success(),
            "{scenario} guard must refuse"
        );
        assert!(!fixture.helper.exists());
        assert!(!fixture.wrapper.exists());
        assert!(!state.join("fleet-auth-support.lock").exists());
    }
}

#[test]
fn dead_legacy_directory_lock_is_reclaimed_under_advisory_guard() {
    let fixture = Fixture::new();
    let state = fixture.state();
    let lock = state.join("fleet-auth-support.lock");
    let pid = lock.join("pid");
    std::fs::create_dir(&lock).expect("legacy lock directory");
    std::fs::write(&pid, b"99999999\n").expect("dead legacy pid");
    std::fs::set_permissions(&pid, std::fs::Permissions::from_mode(0o600))
        .expect("legacy pid mode");

    assert!(fixture.run(RunOptions::default()).success());
    assert!(!lock.exists());
    let guard = state.join("fleet-auth-support.guard");
    assert_eq!(
        std::fs::metadata(&guard)
            .expect("advisory lock carrier")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn published_lock_preserves_the_legacy_shape_until_refresh_completes() {
    let fixture = Fixture::new();
    let observed = fixture.root.path().join("published-lock-observed");

    assert!(
        fixture
            .run(RunOptions {
                refresh: RefreshBehavior::ObservePublishedLock(observed.clone()),
                ..RunOptions::default()
            })
            .success()
    );
    assert!(observed.exists());
    assert!(!fixture.state().join("fleet-auth-support.lock").exists());
    let guard = fixture.state().join("fleet-auth-support.guard");
    assert!(guard.is_file());
    assert_eq!(
        std::fs::metadata(guard)
            .expect("guard metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn crash_before_lock_publication_leaves_no_partial_legacy_lock() {
    let fixture = Fixture::new();

    assert!(
        !fixture
            .run(RunOptions {
                lock_publish: LockPublishBehavior::CrashBeforePublish,
                ..RunOptions::default()
            })
            .success()
    );
    assert!(!fixture.state().join("fleet-auth-support.lock").exists());
    assert!(!fixture.helper.exists());
    assert!(!fixture.wrapper.exists());

    let staging = std::fs::read_dir(fixture.state())
        .expect("state entries")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with(".fleet-auth-support.lock.")
            })
        })
        .expect("private prepared lock");
    assert!(staging.is_dir());
    assert!(!staging.is_symlink());
    assert_eq!(
        std::fs::metadata(&staging)
            .expect("staging metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(staging.join("pid"))
            .expect("staged pid metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        fixture.run(RunOptions::default()).success(),
        "an unpublished private lock must not block the next client"
    );
}

#[test]
fn old_client_destination_race_refuses_without_deleting_foreign_lock() {
    let fixture = Fixture::new();
    let lock = fixture.state().join("fleet-auth-support.lock");

    assert!(
        !fixture
            .run(RunOptions {
                lock_publish: LockPublishBehavior::RaceWithLegacy,
                ..RunOptions::default()
            })
            .success()
    );
    assert!(lock.is_dir());
    assert_eq!(
        std::fs::read_to_string(lock.join("pid")).expect("foreign pid"),
        format!("{}\n", std::process::id())
    );
    assert_eq!(
        std::fs::read_dir(&lock)
            .expect("foreign lock entries")
            .count(),
        1,
        "the new client's nested private staging directory must be removed"
    );
    assert!(!fixture.helper.exists());
    assert!(!fixture.wrapper.exists());
    assert!(
        !fixture
            .state()
            .join("fleet-auth-support.transaction")
            .exists()
    );
}

#[test]
fn active_advisory_lock_refuses_concurrent_transaction_then_releases() {
    let fixture = Fixture::new();
    let state = fixture.state();
    let lock = state.join("fleet-auth-support.guard");
    let acquired = fixture.root.path().join("lock-acquired");
    std::fs::write(&lock, b"").expect("lock carrier");
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600))
        .expect("lock carrier mode");
    let mut holder = Command::new("/bin/bash")
        .args([
            "-c",
            "exec 9<>\"$1\"; /usr/bin/lockf -s -t 0 9 || exit 1; /usr/bin/touch \"$2\"; exec /bin/sleep 30",
            "holder",
        ])
        .arg(&lock)
        .arg(&acquired)
        .spawn()
        .expect("lock holder");
    for _ in 0..200 {
        if acquired.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(acquired.exists(), "holder did not acquire lock");

    assert!(!fixture.run(RunOptions::default()).success());
    assert!(!fixture.helper.exists());
    assert!(!fixture.wrapper.exists());
    holder.kill().expect("stop holder");
    holder.wait().expect("reap holder");

    assert!(fixture.run(RunOptions::default()).success());
}

#[test]
fn detached_post_commit_child_does_not_inherit_advisory_guard() {
    let fixture = Fixture::new();
    assert!(
        fixture
            .run(RunOptions {
                refresh: RefreshBehavior::SpawnDetached,
                ..RunOptions::default()
            })
            .success()
    );
    assert!(
        fixture.run(RunOptions::default()).success(),
        "a detached post-commit child must not retain the advisory guard"
    );
}

#[test]
fn foreign_replacement_of_legacy_pid_is_preserved_at_release() {
    let fixture = Fixture::new();
    assert!(
        !fixture
            .run(RunOptions {
                refresh: RefreshBehavior::ReplaceLegacyPid,
                ..RunOptions::default()
            })
            .success()
    );
    let pid = fixture.state().join("fleet-auth-support.lock/pid");
    assert_eq!(
        std::fs::read_to_string(&pid).expect("foreign pid"),
        "99999999\n"
    );
    assert_eq!(
        std::fs::read(&fixture.helper).expect("committed helper"),
        b"new helper\n"
    );
    assert_eq!(
        std::fs::read(&fixture.wrapper).expect("committed wrapper"),
        b"new wrapper\n"
    );
}

#[test]
fn resolver_failure_skips_refresh_and_refresh_failure_releases_both_lock_layers() {
    let fixture = Fixture::new();
    let refreshed = fixture.root.path().join("refresh-ran");
    assert!(
        !fixture
            .run(RunOptions {
                resolver_succeeds: false,
                refresh: RefreshBehavior::Touch(refreshed.clone()),
                ..RunOptions::default()
            })
            .success()
    );
    assert!(!refreshed.exists(), "failed resolver must not refresh");

    assert!(
        !fixture
            .run(RunOptions {
                refresh: RefreshBehavior::Fail,
                ..RunOptions::default()
            })
            .success()
    );
    let state = fixture.state();
    assert!(!state.join("fleet-auth-support.lock").exists());
    assert!(state.join("fleet-auth-support.guard").is_file());
    assert_eq!(
        std::fs::read(&fixture.helper).expect("committed helper"),
        b"new helper\n"
    );
    assert_eq!(
        std::fs::read(&fixture.wrapper).expect("committed wrapper"),
        b"new wrapper\n"
    );
    assert!(
        fixture.run(RunOptions::default()).success(),
        "refresh failure must release the advisory guard"
    );
}
