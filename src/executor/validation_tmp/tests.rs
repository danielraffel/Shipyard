use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::{Liveness, ReclaimPolicy, owner_path, plan};

struct Fake {
    alive: Vec<u32>,
    tmpdirs: Option<Vec<String>>,
}

impl Liveness for Fake {
    fn pid_alive(&self, pid: u32) -> bool {
        self.alive.contains(&pid)
    }

    fn live_tmpdirs(&self) -> Option<Vec<String>> {
        self.tmpdirs.clone()
    }
}

fn make(base: &Path, name: &str, owner: Option<u32>) -> std::path::PathBuf {
    let dir = base.join(name);
    fs::create_dir_all(dir.join("build")).expect("dir");
    fs::write(dir.join("build").join("blob"), vec![0_u8; 4096]).expect("blob");
    if let Some(pid) = owner {
        fs::write(owner_path(&dir), format!("{pid}\n")).expect("owner");
    }
    dir
}

const POLICY: ReclaimPolicy = ReclaimPolicy {
    min_age: Duration::from_hours(6),
    include_unowned: true,
};

fn later() -> SystemTime {
    SystemTime::now() + Duration::from_hours(7)
}

fn names(candidates: &[super::Candidate]) -> Vec<String> {
    let mut names: Vec<String> = candidates
        .iter()
        .map(|candidate| {
            candidate
                .path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn only_old_directories_nothing_can_still_use_are_reclaimed() {
    let temp = tempfile::tempdir().expect("temp");
    let base = temp.path();
    make(base, "shipyard-validation-deadowner", Some(4_000_001));
    make(base, "shipyard-validation-liveowner", Some(4_000_002));
    make(base, "shipyard-validation-unowned", None);
    let in_use = make(base, "shipyard-validation-inuse", None);
    make(base, "unrelated-dir", None);
    let liveness = Fake {
        alive: vec![4_000_002],
        tmpdirs: Some(vec![in_use.to_string_lossy().into_owned()]),
    };

    let planned = plan(base, later(), POLICY, &liveness);
    assert_eq!(
        names(&planned),
        [
            "shipyard-validation-deadowner",
            "shipyard-validation-unowned"
        ]
    );
    assert!(planned.iter().all(|candidate| candidate.bytes >= 4096));

    // Young directories are kept whoever owns them.
    assert!(plan(base, SystemTime::now(), POLICY, &liveness).is_empty());
}

#[test]
fn an_unreadable_process_listing_keeps_every_unowned_directory() {
    let temp = tempfile::tempdir().expect("temp");
    make(temp.path(), "shipyard-validation-unowned", None);
    let liveness = Fake {
        alive: Vec::new(),
        tmpdirs: None,
    };
    assert!(plan(temp.path(), later(), POLICY, &liveness).is_empty());
}

#[test]
fn the_automatic_policy_never_touches_unowned_directories() {
    let temp = tempfile::tempdir().expect("temp");
    make(temp.path(), "shipyard-validation-unowned", None);
    let liveness = Fake {
        alive: Vec::new(),
        tmpdirs: Some(Vec::new()),
    };
    let policy = ReclaimPolicy {
        include_unowned: false,
        ..POLICY
    };
    assert!(plan(temp.path(), later(), policy, &liveness).is_empty());
}

#[cfg(unix)]
#[test]
fn apply_removes_read_only_trees_and_their_owner_files() {
    use std::os::unix::fs::PermissionsExt;

    use super::apply;
    let temp = tempfile::tempdir().expect("temp");
    let dir = make(temp.path(), "shipyard-validation-locked", Some(4_000_003));
    fs::set_permissions(dir.join("build"), fs::Permissions::from_mode(0o500)).expect("lock");
    let liveness = Fake {
        alive: Vec::new(),
        tmpdirs: Some(Vec::new()),
    };
    let planned = plan(temp.path(), later(), POLICY, &liveness);
    assert_eq!(planned.len(), 1);
    assert!(apply(&planned) >= 4096);
    assert!(!dir.exists());
    assert!(!owner_path(&dir).exists());
}

#[cfg(unix)]
#[test]
fn a_symlink_named_like_a_validation_dir_is_never_followed() {
    let temp = tempfile::tempdir().expect("temp");
    let target = make(temp.path(), "precious", None);
    std::os::unix::fs::symlink(&target, temp.path().join("shipyard-validation-link"))
        .expect("link");
    let liveness = Fake {
        alive: Vec::new(),
        tmpdirs: Some(Vec::new()),
    };
    assert!(plan(temp.path(), later(), POLICY, &liveness).is_empty());
    assert!(target.join("build").join("blob").exists());
}
