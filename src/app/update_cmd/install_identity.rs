//! `shipyard update` installs only the Developer ID release on macOS.
//!
//! The update runs the target tag's own install.sh, and a tag older than the
//! installer's identity check would install whatever it downloads. So the
//! update snapshots the installed pair first and, once the installer exits,
//! checks each installed binary with `codesign --verify --strict` against the
//! release's designated requirement (its identifier, an Apple-anchored
//! Developer ID Application certificate, team 95CX6P84C4). macOS privacy
//! grants follow that identity; any other binary is a new identity that raises
//! prompts nobody answers on an unattended host. A binary that fails is
//! replaced by the snapshot, and the update fails.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::app::CliFailure;

const RELEASE_TEAM: &str = "95CX6P84C4";
/// The system codesign, never a `PATH` lookup.
pub(super) const CODESIGN: &str = "/usr/bin/codesign";
/// The repository owner's developer mode for installing a local build. Never
/// set by agents or automation (docs/install.md); the refusal does not name it.
const UNSIGNED_DEV_ENV: &str = "SHIPYARD_INSTALL_ALLOW_UNSIGNED_DEV";
/// The binaries an install replaces, by codesign identifier.
const MEMBERS: [&str; 2] = ["shipyard", "shipyard-workstream-provider"];

/// The designated requirement of the release binary signed as `identifier`.
fn release_requirement(identifier: &str) -> String {
    format!(
        "identifier \"{identifier}\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] and certificate leaf[field.1.2.840.113635.100.6.1.13] and certificate leaf[subject.OU] = \"{RELEASE_TEAM}\""
    )
}

/// The codesign the update checks with: `None` off macOS and in developer mode.
pub(super) fn update_codesign() -> Option<&'static str> {
    if !cfg!(target_os = "macos") || std::env::var_os(UNSIGNED_DEV_ENV).is_some_and(|v| v == "1") {
        return None;
    }
    Some(CODESIGN)
}

/// What one installed member was before the update.
enum Previous {
    Absent,
    Symlink(PathBuf),
    /// A hard link (or copy) of the previous file, beside it.
    File(PathBuf),
}

/// The installed pair as it was before the installer ran.
pub(super) struct Snapshot {
    members: Vec<(PathBuf, &'static str, Previous)>,
}

impl Snapshot {
    pub(super) fn take(install_dir: &Path) -> Result<Self, CliFailure> {
        let mut members = Vec::new();
        for name in MEMBERS {
            let dest = install_dir.join(name);
            let previous = match std::fs::symlink_metadata(&dest) {
                Err(_) => Previous::Absent,
                Ok(meta) if meta.file_type().is_symlink() => Previous::Symlink(
                    std::fs::read_link(&dest).map_err(|error| snapshot_failure(&dest, &error))?,
                ),
                Ok(_) => {
                    let backup =
                        install_dir.join(format!(".{name}.identity-backup.{}", std::process::id()));
                    let _ = std::fs::remove_file(&backup);
                    // The installer moves the old file aside rather than
                    // rewriting it, so a hard link keeps its exact bytes.
                    if std::fs::hard_link(&dest, &backup).is_err() {
                        std::fs::copy(&dest, &backup)
                            .map_err(|error| snapshot_failure(&dest, &error))?;
                    }
                    Previous::File(backup)
                }
            };
            members.push((dest, name, previous));
        }
        Ok(Self { members })
    }

    /// Check every installed member; on any failure put the previous pair
    /// back and refuse. Removes the snapshot either way.
    pub(super) fn enforce(self, codesign: &str) -> Result<(), CliFailure> {
        let refused = self
            .members
            .iter()
            .filter(|(dest, _, _)| std::fs::symlink_metadata(dest).is_ok())
            .find_map(|(dest, identifier, _)| {
                verify(codesign, dest, identifier)
                    .err()
                    .map(|detail| (*identifier, detail))
            });
        let Some((identifier, detail)) = refused else {
            self.discard();
            return Ok(());
        };
        let restored = self.restore();
        Err(CliFailure::new(
            1,
            format!(
                "refused to activate {identifier}: the installed binary is not signed with \
                 Developer ID as identifier {identifier} by team {RELEASE_TEAM}, so macOS would \
                 treat it as a new privacy identity ({detail}). {}",
                match restored {
                    Ok(()) => "The previous Shipyard was restored.".to_owned(),
                    Err(error) => format!("Restoring the previous Shipyard failed: {error}"),
                }
            ),
        ))
    }

    fn restore(&self) -> Result<(), String> {
        let mut errors = Vec::new();
        for (dest, _, previous) in &self.members {
            let result = match previous {
                Previous::Absent => match std::fs::remove_file(dest) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                    _ => Ok(()),
                },
                Previous::Symlink(target) => {
                    let _ = std::fs::remove_file(dest);
                    symlink(target, dest)
                }
                Previous::File(backup) => std::fs::rename(backup, dest),
            };
            if let Err(error) = result {
                errors.push(format!("{}: {error}", dest.display()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn discard(self) {
        for (_, _, previous) in self.members {
            if let Previous::File(backup) = previous {
                let _ = std::fs::remove_file(backup);
            }
        }
    }
}

#[cfg(unix)]
fn symlink(target: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, dest)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _dest: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "symlinks are not restored on this platform",
    ))
}

fn snapshot_failure(path: &Path, error: &std::io::Error) -> CliFailure {
    CliFailure::new(
        1,
        format!(
            "failed to snapshot {} before updating; nothing was installed: {error}",
            path.display()
        ),
    )
}

fn verify(codesign: &str, binary: &Path, identifier: &str) -> Result<(), String> {
    let output = Command::new(codesign)
        .arg("--verify")
        .arg("--strict")
        .arg(format!("-R={}", release_requirement(identifier)))
        .arg(binary)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{codesign}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("codesign rejected it")
        .chars()
        .take(200)
        .collect())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn script(path: &Path, body: &str) {
        crate::test_support::write_executable_script_with_mode(
            path,
            &format!("#!/bin/sh\n{body}\n"),
            0o700,
        );
    }

    /// An install dir holding a previous pair, and an "installer" that
    /// replaces it the way install.sh does (move aside, move new in).
    fn installed_then_replaced(dir: &Path) -> Snapshot {
        script(&dir.join("shipyard"), "echo 'shipyard 0.100.0'");
        script(
            &dir.join("shipyard-workstream-provider"),
            "echo 'shipyard-workstream-provider 0.100.0'",
        );
        let snapshot = Snapshot::take(dir).expect("snapshot");
        for name in MEMBERS {
            let staged = dir.join(format!(".{name}.staged"));
            script(&staged, &format!("echo '{name} 0.200.0'"));
            std::fs::rename(&staged, dir.join(name)).expect("swap");
        }
        snapshot
    }

    fn contents(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).expect("member")
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .expect("dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn a_refused_binary_is_replaced_by_the_previous_pair() {
        let dir = tempfile::tempdir().expect("dir");
        let refuse = dir.path().join("codesign-refuses");
        script(
            &refuse,
            "echo 'test-requirement: code failed to satisfy specified code requirement(s)' >&2\nexit 3",
        );
        let install = dir.path().join("bin");
        std::fs::create_dir(&install).expect("bin");
        let snapshot = installed_then_replaced(&install);

        let error = snapshot
            .enforce(&refuse.display().to_string())
            .expect_err("refused");
        let message = error.message;
        assert!(
            message.contains("refused to activate shipyard"),
            "{message}"
        );
        assert!(message.contains("team 95CX6P84C4"), "{message}");
        assert!(
            message.contains("The previous Shipyard was restored."),
            "{message}"
        );
        assert!(!message.contains("ALLOW_UNSIGNED"), "{message}");
        assert!(contents(&install, "shipyard").contains("0.100.0"));
        assert!(contents(&install, "shipyard-workstream-provider").contains("0.100.0"));
        assert_eq!(
            leftovers(&install),
            ["shipyard", "shipyard-workstream-provider"]
        );
    }

    #[test]
    fn a_release_pair_is_kept_and_checked_against_each_identifier() {
        let dir = tempfile::tempdir().expect("dir");
        let calls = dir.path().join("calls");
        let accept = dir.path().join("codesign-accepts");
        script(
            &accept,
            &format!(
                "printf '%s|%s|%s\\n' \"$1\" \"$2\" \"$3\" >> '{}'\n[ \"$1\" = --verify ] && [ \"$2\" = --strict ] || exit 9",
                calls.display()
            ),
        );
        let install = dir.path().join("bin");
        std::fs::create_dir(&install).expect("bin");
        let snapshot = installed_then_replaced(&install);

        snapshot
            .enforce(&accept.display().to_string())
            .expect("accepted");
        assert!(contents(&install, "shipyard").contains("0.200.0"));
        assert_eq!(
            leftovers(&install),
            ["shipyard", "shipyard-workstream-provider"]
        );
        let calls = std::fs::read_to_string(calls).expect("calls");
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            [
                format!("--verify|--strict|-R={}", release_requirement("shipyard")),
                format!(
                    "--verify|--strict|-R={}",
                    release_requirement("shipyard-workstream-provider")
                ),
            ]
        );
    }

    #[test]
    fn a_first_install_that_is_refused_leaves_nothing_installed() {
        let dir = tempfile::tempdir().expect("dir");
        let install = dir.path().join("bin");
        std::fs::create_dir(&install).expect("bin");
        let snapshot = Snapshot::take(&install).expect("snapshot");
        script(&install.join("shipyard"), "echo 'shipyard 0.200.0'");
        assert!(snapshot.enforce("/nonexistent/codesign").is_err());
        assert!(leftovers(&install).is_empty());
    }

    /// The real codesign: an ad-hoc signed binary is not the release.
    #[cfg(target_os = "macos")]
    #[test]
    fn real_codesign_refuses_an_ad_hoc_binary() {
        let dir = tempfile::tempdir().expect("dir");
        let install = dir.path().join("bin");
        std::fs::create_dir(&install).expect("bin");
        let snapshot = installed_then_replaced(&install);
        let adhoc = install.join(".adhoc");
        std::fs::copy("/usr/bin/true", &adhoc).expect("copy");
        let signed = Command::new(CODESIGN)
            .args(["--force", "--sign", "-"])
            .arg(&adhoc)
            .output()
            .expect("codesign");
        assert!(signed.status.success(), "{signed:?}");
        std::fs::rename(&adhoc, install.join("shipyard")).expect("swap");

        assert!(snapshot.enforce(CODESIGN).is_err());
        assert!(contents(&install, "shipyard").contains("0.100.0"));
    }
}
