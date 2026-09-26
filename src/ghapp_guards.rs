//! Install and audit the `ghapp` wrapper's optional queue guards.
//!
//! `scripts/ghapp` runs `queue-removal-guard`, `queue-arm-guard` and
//! `branch-refresh-guard` from
//! `$SHIPYARD_GHAPP_GUARDS_DIR` (default `~/.config/shipyard/guards`) when they
//! are present and executable. Nothing else puts them there, so a host keeps
//! whatever copy somebody placed by hand, possibly months stale. This module
//! bundles the repository's guard sources into the binary so
//! `shipyard guards install` can place the exact copies this build was tested
//! with, and `shipyard guards status` / `shipyard doctor` can say when an
//! installed copy is missing or differs from them.
//!
//! `pr-close-guard` is deliberately not managed here: it is a mandatory,
//! release-matched member of the authenticated `ghapp` generation and is
//! published by `shipyard fleet update`.
//!
//! The arm and branch-refresh guards load their request parser from the
//! removal guard sitting next to them, so all three are installed together.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::doctor::DoctorEntry;

/// A guard this build bundles.
#[derive(Clone, Copy, Debug)]
pub struct ManagedGuard {
    /// Installed file name, as `scripts/ghapp` invokes it.
    pub name: &'static str,
    /// Repository source path, for messages.
    pub source_path: &'static str,
    /// Exact bundled bytes.
    pub contents: &'static [u8],
}

/// Every guard `shipyard guards install` manages, in install order: the arm
/// and branch-refresh guards import the removal guard's request parser, so the
/// parser lands first.
pub const MANAGED_GUARDS: [ManagedGuard; 3] = [
    ManagedGuard {
        name: "queue-removal-guard",
        source_path: "scripts/ghapp_queue_removal_guard.py",
        contents: include_bytes!("../scripts/ghapp_queue_removal_guard.py"),
    },
    ManagedGuard {
        name: "queue-arm-guard",
        source_path: "scripts/ghapp_queue_arm_guard.py",
        contents: include_bytes!("../scripts/ghapp_queue_arm_guard.py"),
    },
    ManagedGuard {
        name: "branch-refresh-guard",
        source_path: "scripts/ghapp_branch_refresh_guard.py",
        contents: include_bytes!("../scripts/ghapp_branch_refresh_guard.py"),
    },
];

/// Printed with every `shipyard guards` report: installing into the shared
/// directory changes what older Shipyard binaries on the host may do.
pub const FLEET_NOTE: &str = "note: this directory is shared by every Shipyard binary on the \
     host. Older binaries enqueue without Shipyard's internal marker, so once queue-arm-guard is \
     installed their same-head re-enqueues after failed_checks/merge_conflict (the intended \
     ALLGREEN refusal) or after a manual removal, and any enqueue whose live state cannot be read, \
     are refused. Update every Shipyard binary on the host first; see docs/ghapp-guards.md.";

/// Where `scripts/ghapp` looks for guards on this host.
#[must_use]
pub fn default_guards_dir() -> PathBuf {
    std::env::var_os("SHIPYARD_GHAPP_GUARDS_DIR")
        .filter(|value| !value.is_empty())
        .map_or_else(
            || crate::paths::home_dir().join(".config/shipyard/guards"),
            PathBuf::from,
        )
}

/// Installed state of one managed guard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GuardState {
    /// Byte-identical to the bundled copy and executable.
    Current,
    /// Not installed, so `ghapp` silently skips it.
    Missing,
    /// Installed content differs from this build's copy.
    Stale {
        /// SHA-256 of the installed file.
        installed_sha256: String,
    },
    /// Content matches but `ghapp` will not run it.
    NotExecutable,
    /// A symlink or other non-regular file this command will not overwrite.
    Unmanaged {
        /// What was found.
        detail: String,
    },
    /// The file could not be read.
    Unreadable {
        /// The I/O error.
        detail: String,
    },
}

/// One guard's audit row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GuardAudit {
    /// Installed file name.
    pub name: String,
    /// Absolute installed path.
    pub path: PathBuf,
    /// SHA-256 of this build's bundled copy.
    pub expected_sha256: String,
    /// What is installed.
    #[serde(flatten)]
    pub state: GuardState,
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

/// Compare every managed guard in `dir` against this build's copy.
#[must_use]
pub fn audit(dir: &Path) -> Vec<GuardAudit> {
    MANAGED_GUARDS
        .iter()
        .map(|guard| {
            let path = dir.join(guard.name);
            let expected_sha256 = sha256_hex(guard.contents);
            let state = match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardState::Missing,
                Err(error) => GuardState::Unreadable {
                    detail: error.to_string(),
                },
                Ok(metadata) if !metadata.file_type().is_file() => GuardState::Unmanaged {
                    detail: if metadata.file_type().is_symlink() {
                        "a symlink".to_owned()
                    } else {
                        "not a regular file".to_owned()
                    },
                },
                Ok(metadata) => match fs::read(&path) {
                    Err(error) => GuardState::Unreadable {
                        detail: error.to_string(),
                    },
                    Ok(bytes) if bytes != guard.contents => GuardState::Stale {
                        installed_sha256: sha256_hex(&bytes),
                    },
                    Ok(_) if !is_executable(&metadata) => GuardState::NotExecutable,
                    Ok(_) => GuardState::Current,
                },
            };
            GuardAudit {
                name: guard.name.to_owned(),
                path,
                expected_sha256,
                state,
            }
        })
        .collect()
}

/// What `install` did, or would do, for one guard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstallAction {
    /// Installed file name.
    pub name: String,
    /// Absolute installed path.
    pub path: PathBuf,
    /// `unchanged`, `installed`, `replaced`, or `refused`.
    pub action: String,
    /// Why, when refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Install every managed guard into `dir`.
///
/// Each file is written to a temporary sibling and renamed into place, so a
/// concurrently running `ghapp` sees either the old or the new guard, never a
/// partial one. A symlink or other non-regular file is never overwritten.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or a file cannot be
/// written; guards already replaced stay replaced.
pub fn install(dir: &Path, dry_run: bool) -> Result<Vec<InstallAction>, String> {
    let audits = audit(dir);
    if !dry_run {
        fs::create_dir_all(dir)
            .map_err(|error| format!("create guards directory {}: {error}", dir.display()))?;
    }
    let mut actions = Vec::new();
    for (guard, audit) in MANAGED_GUARDS.iter().zip(audits) {
        let (action, detail) = match &audit.state {
            GuardState::Current => ("unchanged", None),
            GuardState::Missing => ("installed", None),
            GuardState::Stale { .. } | GuardState::NotExecutable => ("replaced", None),
            GuardState::Unmanaged { detail } | GuardState::Unreadable { detail } => {
                ("refused", Some(detail.clone()))
            }
        };
        if !dry_run && matches!(action, "installed" | "replaced") {
            write_atomically(dir, guard)?;
        }
        actions.push(InstallAction {
            name: guard.name.to_owned(),
            path: audit.path,
            action: action.to_owned(),
            detail,
        });
    }
    Ok(actions)
}

fn write_atomically(dir: &Path, guard: &ManagedGuard) -> Result<(), String> {
    let target = dir.join(guard.name);
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|error| format!("stage {}: {error}", guard.name))?;
    temp.write_all(guard.contents)
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|error| format!("write {}: {error}", guard.name))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("chmod {}: {error}", guard.name))?;
    }
    temp.persist(&target)
        .map_err(|error| format!("install {}: {error}", target.display()))?;
    Ok(())
}

/// Doctor section for the managed guards, or `None` when this host has no
/// `ghapp` wrapper and no guards directory (the guards could not run anyway).
#[must_use]
pub fn doctor_section(dir: &Path, home: &Path) -> Option<BTreeMap<String, DoctorEntry>> {
    if !dir.exists() && !home.join(".local/bin/ghapp").exists() {
        return None;
    }
    Some(
        audit(dir)
            .into_iter()
            .map(|audit| {
                let fix = "run `shipyard guards install`".to_owned();
                let entry = match &audit.state {
                    GuardState::Current => DoctorEntry {
                        ok: true,
                        version: Some(format!("current ({})", &audit.expected_sha256[..12])),
                        detail: None,
                        error: None,
                    },
                    GuardState::Missing => DoctorEntry {
                        ok: false,
                        version: None,
                        detail: Some(format!(
                            "ghapp skips an absent guard, so this protection is off; {fix}"
                        )),
                        error: Some("not installed".to_owned()),
                    },
                    GuardState::Stale { installed_sha256 } => DoctorEntry {
                        ok: false,
                        version: Some("stale".to_owned()),
                        detail: Some(format!(
                            "installed sha256 {} differs from this build's {}; {fix}",
                            &installed_sha256[..12],
                            &audit.expected_sha256[..12]
                        )),
                        error: None,
                    },
                    GuardState::NotExecutable => DoctorEntry {
                        ok: false,
                        version: None,
                        detail: Some(format!("ghapp only runs executable guards; {fix}")),
                        error: Some("not executable".to_owned()),
                    },
                    GuardState::Unmanaged { detail } | GuardState::Unreadable { detail } => {
                        DoctorEntry {
                            ok: false,
                            version: None,
                            detail: Some(format!("{} is {detail}", audit.path.display())),
                            error: Some("unmanaged".to_owned()),
                        }
                    }
                };
                (audit.name, entry)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_guards_are_the_repository_sources() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for guard in MANAGED_GUARDS {
            assert_eq!(
                fs::read(root.join(guard.source_path)).expect("source"),
                guard.contents,
                "{}",
                guard.name
            );
        }
    }

    #[test]
    fn every_parser_consumer_installs_after_the_removal_guard() {
        let names = MANAGED_GUARDS.map(|guard| guard.name);
        assert_eq!(names[0], "queue-removal-guard");
        for consumer in ["queue-arm-guard", "branch-refresh-guard"] {
            assert!(names[1..].contains(&consumer), "{consumer} is not managed");
        }
    }

    #[cfg(unix)]
    #[test]
    fn install_places_missing_and_replaces_stale_guards_then_reports_current() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path().join("guards");
        assert!(
            audit(&dir)
                .iter()
                .all(|row| row.state == GuardState::Missing)
        );

        let planned = install(&dir, true).expect("dry run");
        assert!(planned.iter().all(|row| row.action == "installed"));
        assert!(!dir.exists(), "dry run must not write");

        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("queue-removal-guard"), b"#!/bin/sh\nexit 0\n").expect("stale");
        let done = install(&dir, false).expect("install");
        assert_eq!(done[0].action, "replaced");
        assert_eq!(done[1].action, "installed");
        assert!(
            audit(&dir)
                .iter()
                .all(|row| row.state == GuardState::Current),
            "{:?}",
            audit(&dir)
        );
        let again = install(&dir, false).expect("idempotent");
        assert!(again.iter().all(|row| row.action == "unchanged"));
    }

    #[cfg(unix)]
    #[test]
    fn stale_and_non_executable_copies_are_reported() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path();
        fs::write(dir.join("queue-removal-guard"), b"old").expect("stale");
        fs::write(dir.join("queue-arm-guard"), MANAGED_GUARDS[1].contents).expect("copy");
        fs::set_permissions(
            dir.join("queue-arm-guard"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");
        let rows = audit(dir);
        assert!(matches!(rows[0].state, GuardState::Stale { .. }));
        assert_eq!(rows[1].state, GuardState::NotExecutable);
        let section = doctor_section(dir, dir).expect("guards dir exists");
        assert!(!section["queue-removal-guard"].ok);
        assert!(
            section["queue-removal-guard"]
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("shipyard guards install"))
        );
        assert!(!section["queue-arm-guard"].ok);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_guard_is_never_overwritten() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path();
        let elsewhere = dir.join("elsewhere");
        fs::write(&elsewhere, b"owned by someone else").expect("target");
        std::os::unix::fs::symlink(&elsewhere, dir.join("queue-arm-guard")).expect("link");
        let actions = install(dir, false).expect("install");
        assert_eq!(actions[1].action, "refused");
        assert_eq!(
            fs::read(&elsewhere).expect("target"),
            b"owned by someone else"
        );
    }

    #[test]
    fn fleet_note_names_the_blast_radius_and_the_doc_not_the_overrides() {
        assert!(FLEET_NOTE.contains("failed_checks/merge_conflict"));
        assert!(FLEET_NOTE.contains("docs/ghapp-guards.md"));
        assert!(!FLEET_NOTE.contains("GHAPP_ALLOW"));
        assert!(!FLEET_NOTE.contains("SHIPYARD_INTERNAL"));
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docs/ghapp-guards.md")
                .is_file()
        );
    }

    #[test]
    fn doctor_skips_hosts_without_ghapp() {
        let temp = tempfile::tempdir().expect("temp");
        assert!(doctor_section(&temp.path().join("absent"), temp.path()).is_none());
    }
}
