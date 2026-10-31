//! Install and audit the `ghapp` wrapper's optional queue guards.
//!
//! `scripts/ghapp` runs `queue-removal-guard`, `queue-arm-guard`,
//! `branch-refresh-guard`, `merge-guard`, `verdict-hold-hook`,
//! `verdict-session-stamp` and `reviewer-claim-guard` from
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
//!
//! Review-before-arm is on by default in both twins (the arm guard and
//! `head_approval`). The only way off is the opt-out file
//! ([`REVIEW_OPT_OUT`]), written with a reason by [`opt_out_review`] and
//! logged to [`SWITCH_LOG`]; `guards status` and `doctor` show a host that is
//! off.

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
pub const MANAGED_GUARDS: [ManagedGuard; 7] = [
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
    // Refuses `pr merge` on a repository that cannot enforce required checks
    // server-side until its configured checks are green. Standalone: it shares
    // no parser with the queue guards.
    ManagedGuard {
        name: "merge-guard",
        source_path: "scripts/ghapp_merge_guard.sh",
        contents: include_bytes!("../scripts/ghapp_merge_guard.sh"),
    },
    // Before a HOLD verdict posts, dequeues or disarms that pull request at
    // exactly that head. Never refuses; loads the removal guard's parser.
    ManagedGuard {
        name: "verdict-hold-hook",
        source_path: "scripts/ghapp_verdict_hold_hook.py",
        contents: include_bytes!("../scripts/ghapp_verdict_hold_hook.py"),
    },
    // Stamps a verdict comment with the posting session, so the arm guard can
    // refuse an arm from the session that approved it. Never refuses.
    ManagedGuard {
        name: "verdict-session-stamp",
        source_path: "scripts/ghapp_verdict_session_stamp.py",
        contents: include_bytes!("../scripts/ghapp_verdict_session_stamp.py"),
    },
    // Refuses an exact-head verdict when the protected base enables durable
    // reviewer claims and the posting reviewer has no current local claim.
    ManagedGuard {
        name: "reviewer-claim-guard",
        source_path: "scripts/ghapp_reviewer_claim_guard.py",
        contents: include_bytes!("../scripts/ghapp_reviewer_claim_guard.py"),
    },
];

/// Printed with every `shipyard guards` report: installing into the shared
/// directory changes what older Shipyard binaries on the host may do.
pub const FLEET_NOTE: &str = "note: this directory is shared by every Shipyard binary on the \
     host. Older binaries enqueue without Shipyard's internal marker, so once queue-arm-guard is \
     installed their same-head re-enqueues after failed_checks/merge_conflict (the intended \
     ALLGREEN refusal) or after a manual removal, and any enqueue whose live state cannot be read, \
     are refused. Update every Shipyard binary on the host first; see docs/ghapp-guards.md.";

/// The review-before-arm rule, as `guards status` names it.
pub const REVIEW_SWITCH: &str = "review-before-arm";
/// Present when an operator opted this host out of review-before-arm; holds
/// the reason, the time and the binary that recorded it. The arm guard
/// (`HOST_OPT_OUT_FILE`) and `head_approval` (`HOST_OPT_OUT_FILE`) read it.
pub const REVIEW_OPT_OUT: &str = "review-before-arm.opt-out";
/// Append-only log of every change to the review switch.
pub const SWITCH_LOG: &str = "switches.jsonl";

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

/// State of the review-before-arm rule on this host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SwitchState {
    /// On: the default, with no opt-out file.
    Current,
    /// Turned off on purpose, with the recorded reason.
    OptedOut {
        /// The opt-out record (`reason`, `at`, `by`), as written.
        record: String,
    },
    /// Something at the opt-out path that is not a valid opt-out record:
    /// the guards keep the rule ON, and status fails until it is fixed or
    /// removed.
    Unreadable {
        /// Why it does not count.
        detail: String,
    },
}

/// The review rule's audit row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SwitchAudit {
    /// [`REVIEW_SWITCH`].
    pub name: String,
    /// Absolute path of the opt-out file.
    pub path: PathBuf,
    /// What is there.
    #[serde(flatten)]
    pub state: SwitchState,
}

/// Read the review-before-arm rule's state in `dir`: on unless it holds a
/// valid opt-out record, by the guards' own rule
/// ([`crate::head_approval::read_opt_out`]).
#[must_use]
pub fn audit_review_switch(dir: &Path) -> SwitchAudit {
    let path = dir.join(REVIEW_OPT_OUT);
    let state = match crate::head_approval::read_opt_out(&path) {
        crate::head_approval::OptOut::Absent => SwitchState::Current,
        crate::head_approval::OptOut::Valid { record, .. } => SwitchState::OptedOut { record },
        crate::head_approval::OptOut::Invalid { detail } => SwitchState::Unreadable {
            detail: format!("{detail}; review before arm stays on"),
        },
    };
    SwitchAudit {
        name: REVIEW_SWITCH.to_owned(),
        path,
        state,
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn log_switch(dir: &Path, change: &str, reason: Option<&str>) -> Result<(), String> {
    let mut record = serde_json::json!({
        "at": now_rfc3339(),
        "switch": REVIEW_SWITCH,
        "change": change,
        "reason": reason,
        "by": format!("shipyard {}", env!("CARGO_PKG_VERSION")),
    });
    if let Some(map) = record.as_object_mut() {
        map.extend(crate::head_approval::writer_identity());
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(SWITCH_LOG))
        .map_err(|error| format!("open {}: {error}", dir.join(SWITCH_LOG).display()))?;
    writeln!(file, "{record}").map_err(|error| format!("log switch change: {error}"))
}

/// Opt this host out of review-before-arm, with a reason that is recorded in
/// the opt-out file and the switch log. Later installs leave it off.
///
/// # Errors
///
/// An empty reason, or a write that fails.
pub fn opt_out_review(dir: &Path, reason: &str) -> Result<SwitchAudit, String> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err("opting out of review-before-arm needs a reason".to_owned());
    }
    fs::create_dir_all(dir)
        .map_err(|error| format!("create guards directory {}: {error}", dir.display()))?;
    let record = serde_json::json!({
        "reason": reason,
        "at": now_rfc3339(),
        "by": format!("shipyard {}", env!("CARGO_PKG_VERSION")),
    });
    fs::write(dir.join(REVIEW_OPT_OUT), format!("{record}\n"))
        .map_err(|error| format!("write {}: {error}", dir.join(REVIEW_OPT_OUT).display()))?;
    log_switch(dir, "opted out", Some(reason))?;
    Ok(audit_review_switch(dir))
}

/// Undo an opt-out: remove it, logged. The rule is on again.
///
/// # Errors
///
/// A write that fails.
pub fn opt_in_review(dir: &Path) -> Result<SwitchAudit, String> {
    fs::create_dir_all(dir)
        .map_err(|error| format!("create guards directory {}: {error}", dir.display()))?;
    match fs::remove_file(dir.join(REVIEW_OPT_OUT)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove {REVIEW_OPT_OUT}: {error}")),
    }
    log_switch(dir, "opted back in", None)?;
    Ok(audit_review_switch(dir))
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
    let switch = audit_review_switch(dir);
    let switch_entry = match &switch.state {
        SwitchState::Current => DoctorEntry {
            ok: true,
            version: Some("on".to_owned()),
            detail: None,
            error: None,
        },
        // Off on purpose is reported, not failed: the opt-out is the
        // operator's recorded decision.
        SwitchState::OptedOut { record } => DoctorEntry {
            ok: true,
            version: Some("OFF (opted out)".to_owned()),
            detail: Some(record.clone()),
            error: None,
        },
        SwitchState::Unreadable { detail } => DoctorEntry {
            ok: false,
            version: None,
            detail: Some(detail.clone()),
            error: Some("unreadable".to_owned()),
        },
    };
    let mut section: BTreeMap<String, DoctorEntry> = audit(dir)
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
        .collect();
    section.insert(switch.name, switch_entry);
    Some(section)
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
    fn review_before_arm_is_on_by_default_and_off_only_by_a_logged_opt_out() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path();
        assert_eq!(audit_review_switch(dir).state, SwitchState::Current);
        install(dir, false).expect("install");
        assert!(
            !dir.join(REVIEW_SWITCH).exists(),
            "install no longer needs an enable file"
        );
        assert!(doctor_section(dir, dir).expect("section")[REVIEW_SWITCH].ok);
        assert!(opt_out_review(dir, "  ").is_err(), "a reason is required");
        assert!(
            !dir.join(REVIEW_OPT_OUT).exists(),
            "a refused opt-out writes nothing"
        );
        let audit = opt_out_review(dir, "lab host, no landings").expect("opt out");
        assert!(
            matches!(&audit.state, SwitchState::OptedOut { record } if record.contains("lab host, no landings")),
            "{audit:?}"
        );
        install(dir, false).expect("install");
        assert!(
            dir.join(REVIEW_OPT_OUT).exists(),
            "install leaves an opt-out in place"
        );
        let off = doctor_section(dir, dir).expect("section");
        assert_eq!(
            off[REVIEW_SWITCH].version.as_deref(),
            Some("OFF (opted out)")
        );
        let log = fs::read_to_string(dir.join(SWITCH_LOG)).expect("log");
        assert!(
            log.contains("\"opted out\"") && log.contains("lab host, no landings"),
            "{log}"
        );
        // Each row says which process wrote it.
        let identity = crate::head_approval::writer_identity();
        for line in log.lines() {
            let row: serde_json::Value = serde_json::from_str(line).expect("json row");
            assert_eq!(row.get("session"), identity.get("session"), "{line}");
            assert_eq!(row.get("agent"), identity.get("agent"), "{line}");
        }
        opt_in_review(dir).expect("opt in");
        assert_eq!(audit_review_switch(dir).state, SwitchState::Current);
        assert!(
            fs::read_to_string(dir.join(SWITCH_LOG))
                .expect("log")
                .contains("opted back in")
        );
    }

    #[test]
    fn only_a_written_opt_out_record_turns_review_off() {
        type Make = fn(&Path);
        let opt_out = |dir: &Path| {
            // The guards' own predicate, so the audit and the guards cannot drift.
            matches!(
                crate::head_approval::read_opt_out(&dir.join(REVIEW_OPT_OUT)),
                crate::head_approval::OptOut::Valid { .. }
            )
        };
        let bad: [(&str, Make); 5] = [
            ("empty", |path| fs::write(path, "").expect("write")),
            ("plain text", |path| {
                fs::write(path, "lab host").expect("write");
            }),
            ("no reason", |path| {
                fs::write(path, r#"{"reason":" ","at":"t","by":"me"}"#).expect("write");
            }),
            ("a directory", |path| fs::create_dir(path).expect("dir")),
            ("a symlink to a directory", |path| {
                let target = path.with_extension("target");
                fs::create_dir(&target).expect("dir");
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, path).expect("link");
                #[cfg(not(unix))]
                fs::create_dir(path).expect("dir");
            }),
        ];
        for (label, make) in bad {
            let temp = tempfile::tempdir().expect("temp");
            make(&temp.path().join(REVIEW_OPT_OUT));
            assert!(
                !opt_out(temp.path()),
                "{label}: the guards must keep review on"
            );
            assert!(
                matches!(
                    audit_review_switch(temp.path()).state,
                    SwitchState::Unreadable { .. }
                ),
                "{label}: status must fail, not report on or off"
            );
            assert!(
                !doctor_section(temp.path(), temp.path()).expect("section")[REVIEW_SWITCH].ok,
                "{label}: doctor must fail"
            );
        }
        // A dangling symlink is nothing there: review on, and status says so.
        #[cfg(unix)]
        {
            let temp = tempfile::tempdir().expect("temp");
            std::os::unix::fs::symlink("/nonexistent", temp.path().join(REVIEW_OPT_OUT))
                .expect("link");
            assert!(!opt_out(temp.path()));
            assert_eq!(audit_review_switch(temp.path()).state, SwitchState::Current);
        }
        let temp = tempfile::tempdir().expect("temp");
        opt_out_review(temp.path(), "lab host").expect("opt out");
        assert!(opt_out(temp.path()), "the written record is an opt-out");
    }

    #[test]
    fn every_twin_reads_the_same_opt_out_file() {
        let guard = String::from_utf8_lossy(MANAGED_GUARDS[1].contents);
        assert!(
            guard.contains(&format!("HOST_OPT_OUT_FILE = \"{REVIEW_OPT_OUT}\"")),
            "the arm guard reads a different opt-out file"
        );
        assert_eq!(crate::head_approval::HOST_OPT_OUT_FILE, REVIEW_OPT_OUT);
    }

    #[test]
    fn doctor_skips_hosts_without_ghapp() {
        let temp = tempfile::tempdir().expect("temp");
        assert!(doctor_section(&temp.path().join("absent"), temp.path()).is_none());
    }
}
