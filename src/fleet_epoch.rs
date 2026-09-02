//! Fail-closed check that this host has converged to the declared fleet state.
//!
//! Fleet configuration (TART_HOME, cache roots, disk floors, routing
//! variables) is declared once in `planning/fleet/manifest.toml` and carries a
//! monotonically increasing `epoch`. Each host records the epoch it last
//! applied in a receipt. A host running behind the manifest is running against
//! infrastructure assumptions that have since changed.
//!
//! This is the third of three sync layers. Layer A pushes a change to agents
//! immediately (best effort). Layer B lets every checkout pull it from git
//! (eventual). Neither can guarantee a given host acted, so Layer B's
//! "eventual" is doing a lot of work -- which is why Layer C exists: at the
//! point of use, a host that has not converged refuses rather than proceeding
//! on stale assumptions.
//!
//! Absence is never a failure. A repository with no manifest, or a manifest
//! with no epoch, is simply not participating, and the check reports
//! [`FleetEpochStatus::NotConfigured`]. Only a manifest that exists and is
//! ahead of this host's receipt blocks anything.

use std::fmt::{Display, Formatter};
use std::path::Path;

use serde::Serialize;

/// Manifest path relative to the repository root.
pub const MANIFEST_RELATIVE_PATH: &str = "planning/fleet/manifest.toml";

/// Default receipt directory, relative to the repository root.
///
/// The manifest may override it via `meta.receipts_dir`; that value wins so
/// the publisher (`tools/fleet/apply.sh`) and this reader cannot drift apart.
pub const DEFAULT_RECEIPT_RELATIVE_DIR: &str = "planning/fleet/receipts";

/// Whether this host has converged to the declared fleet epoch.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FleetEpochStatus {
    /// No manifest, or a manifest that declares no epoch. Not participating.
    NotConfigured {
        /// Why the check does not apply.
        detail: String,
    },
    /// This host's receipt matches or exceeds the declared epoch.
    Converged {
        /// Epoch the manifest declares.
        declared: u64,
        /// Epoch this host last applied.
        applied: u64,
    },
    /// This host is behind the declared epoch, or has never applied.
    Behind {
        /// Epoch the manifest declares.
        declared: u64,
        /// Epoch this host last applied, if it ever did.
        applied: Option<u64>,
        /// Host the receipt was looked up for.
        host: String,
    },
    /// The manifest or receipt could not be read or parsed.
    ///
    /// Distinct from [`Self::Converged`] on purpose: "I could not check" must
    /// never be reported as "this is fine".
    Unobservable {
        /// What could not be read, and why.
        detail: String,
    },
}

impl FleetEpochStatus {
    /// Whether this status should stop work from proceeding.
    ///
    /// Both `Behind` and `Unobservable` block. An unreadable manifest is not
    /// evidence of convergence, so treating it as passing would defeat the
    /// entire point of a fail-closed check.
    #[must_use]
    pub fn blocks(&self) -> bool {
        matches!(self, Self::Behind { .. } | Self::Unobservable { .. })
    }
}

impl Display for FleetEpochStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured { detail } => {
                write!(f, "fleet manifest not configured: {detail}")
            }
            Self::Converged { declared, applied } => write!(
                f,
                "host converged to fleet epoch {declared} (applied {applied})"
            ),
            Self::Behind {
                declared,
                applied,
                host,
            } => write!(
                f,
                "host {host} is behind the declared fleet epoch: manifest is at {declared}, host \
                 applied {}. Run tools/fleet/apply.sh to converge before dispatching work.",
                applied.map_or_else(|| "nothing".to_owned(), |epoch| epoch.to_string())
            ),
            Self::Unobservable { detail } => write!(
                f,
                "fleet epoch could not be observed: {detail}. Refusing rather than assuming \
                 convergence."
            ),
        }
    }
}

/// Check whether this machine has converged to the epoch declared under
/// `repo_root`.
///
/// `machine` is the local machine name (`hostname -s`). Receipts carry both a
/// stable fleet host id (`macstudio`) and the machine's hostname
/// (`Daniels-Mac-Studio`); either may match, so a host keeps its identity when
/// the machine is renamed.
#[must_use]
pub fn check(repo_root: &Path, machine: &str) -> FleetEpochStatus {
    let manifest_path = repo_root.join(MANIFEST_RELATIVE_PATH);
    if !manifest_path.exists() {
        return FleetEpochStatus::NotConfigured {
            detail: format!("{} does not exist", manifest_path.display()),
        };
    }
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(error) => {
            return FleetEpochStatus::Unobservable {
                detail: format!("cannot read {}: {error}", manifest_path.display()),
            };
        }
    };
    let Ok(manifest) = manifest.parse::<toml::Table>() else {
        return FleetEpochStatus::Unobservable {
            detail: format!("cannot parse {}", manifest_path.display()),
        };
    };
    let Some(declared) = manifest
        .get("epoch")
        .and_then(toml::Value::as_integer)
        .and_then(|epoch| u64::try_from(epoch).ok())
    else {
        return FleetEpochStatus::NotConfigured {
            detail: format!("{} declares no epoch", manifest_path.display()),
        };
    };

    let receipts_dir = repo_root.join(
        manifest
            .get("meta")
            .and_then(toml::Value::as_table)
            .and_then(|meta| meta.get("receipts_dir"))
            .and_then(toml::Value::as_str)
            .unwrap_or(DEFAULT_RECEIPT_RELATIVE_DIR),
    );

    match read_applied_epoch(&receipts_dir, machine) {
        Ok(Some(applied)) if applied >= declared => {
            FleetEpochStatus::Converged { declared, applied }
        }
        Ok(applied) => FleetEpochStatus::Behind {
            declared,
            applied,
            host: machine.to_owned(),
        },
        Err(detail) => FleetEpochStatus::Unobservable { detail },
    }
}

/// Find this machine's receipt and read the epoch it converged to.
///
/// Returns `Ok(None)` when no receipt claims this machine. A receipt that
/// exists but cannot be read is an error, never a miss: an unreadable receipt
/// is not evidence that this host is unconverged, it is evidence that we do
/// not know, and the two must stay distinguishable.
fn read_applied_epoch(receipts_dir: &Path, machine: &str) -> Result<Option<u64>, String> {
    let Ok(entries) = std::fs::read_dir(receipts_dir) else {
        // A missing receipts directory means nothing has ever applied here.
        return Ok(None);
    };
    let mut applied = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read receipt {}: {error}", path.display()))?;
        let receipt: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("cannot parse receipt {}: {error}", path.display()))?;
        let matches_machine = ["hostname", "host"].iter().any(|field| {
            receipt
                .get(*field)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case(machine))
        });
        if !matches_machine {
            continue;
        }
        let epoch = receipt
            .get("epoch")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| format!("receipt {} declares no integer epoch", path.display()))?;
        applied.get_or_insert(epoch);
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::{FleetEpochStatus, check};

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, body).expect("write");
    }

    fn seed_manifest(root: &Path, body: &str) {
        write(&root.join(super::MANIFEST_RELATIVE_PATH), body);
    }

    /// Write a receipt in the shape `tools/fleet/apply.sh` actually emits:
    /// JSON, carrying both the stable fleet host id and the machine hostname.
    fn seed_receipt(root: &Path, host: &str, hostname: &str, epoch: u64) {
        write(
            &root
                .join(super::DEFAULT_RECEIPT_RELATIVE_DIR)
                .join(format!("{host}.json")),
            &format!(r#"{{"host": "{host}", "hostname": "{hostname}", "epoch": {epoch}}}"#),
        );
    }

    #[test]
    fn a_repo_with_no_manifest_is_not_participating() {
        // Absence must never block: most repositories will never have a fleet
        // manifest, and this check must not turn that into a failure.
        let sandbox = TempDir::new().expect("tempdir");

        let status = check(sandbox.path(), "Daniels-Mac-Studio");

        assert!(matches!(status, FleetEpochStatus::NotConfigured { .. }));
        assert!(!status.blocks());
    }

    #[test]
    fn a_manifest_without_an_epoch_is_not_participating() {
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "[hosts.macstudio]\ntart_home = \"/x\"\n");

        let status = check(sandbox.path(), "Daniels-Mac-Studio");

        assert!(matches!(status, FleetEpochStatus::NotConfigured { .. }));
        assert!(!status.blocks());
    }

    #[test]
    fn control_a_host_at_the_declared_epoch_is_converged() {
        // Control for the blocking tests below: proves a block is the epoch
        // gap, not a fixture that can never converge.
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        seed_receipt(sandbox.path(), "macstudio", "Daniels-Mac-Studio", 7);

        let status = check(sandbox.path(), "Daniels-Mac-Studio");

        assert_eq!(
            status,
            FleetEpochStatus::Converged {
                declared: 7,
                applied: 7,
            }
        );
        assert!(!status.blocks());
    }

    #[test]
    fn a_receipt_is_matched_by_its_stable_host_id_too() {
        // Callers may pass either the machine hostname or the fleet host id,
        // so a machine rename does not orphan a converged host.
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        seed_receipt(sandbox.path(), "macstudio", "Daniels-Mac-Studio", 7);

        assert!(!check(sandbox.path(), "macstudio").blocks());
    }

    #[test]
    fn the_manifest_can_relocate_the_receipts_directory() {
        // apply.sh honors meta.receipts_dir; if this reader did not, the two
        // halves would silently stop meeting.
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(
            sandbox.path(),
            "epoch = 7\n\n[meta]\nreceipts_dir = \"custom/receipts\"\n",
        );
        write(
            &sandbox.path().join("custom/receipts/macstudio.json"),
            r#"{"host": "macstudio", "hostname": "Daniels-Mac-Studio", "epoch": 7}"#,
        );

        assert!(!check(sandbox.path(), "Daniels-Mac-Studio").blocks());
    }

    #[test]
    fn a_host_ahead_of_the_manifest_is_still_converged() {
        // A host that applied a newer epoch (mid-rollout, before the manifest
        // commit landed in this checkout) is not stale.
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        seed_receipt(sandbox.path(), "macstudio", "Daniels-Mac-Studio", 9);

        assert!(!check(sandbox.path(), "Daniels-Mac-Studio").blocks());
    }

    #[test]
    fn a_host_behind_the_manifest_blocks() {
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        seed_receipt(sandbox.path(), "macstudio", "Daniels-Mac-Studio", 6);

        let status = check(sandbox.path(), "Daniels-Mac-Studio");

        assert_eq!(
            status,
            FleetEpochStatus::Behind {
                declared: 7,
                applied: Some(6),
                host: "Daniels-Mac-Studio".to_owned(),
            }
        );
        assert!(status.blocks());
        assert!(
            status.to_string().contains("tools/fleet/apply.sh"),
            "the message must say how to converge: {status}"
        );
    }

    #[test]
    fn a_host_that_never_applied_blocks() {
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");

        let status = check(sandbox.path(), "Daniels-Mac-Studio");

        assert_eq!(
            status,
            FleetEpochStatus::Behind {
                declared: 7,
                applied: None,
                host: "Daniels-Mac-Studio".to_owned(),
            }
        );
        assert!(status.blocks());
    }

    #[test]
    fn an_unreadable_receipt_blocks_rather_than_passing() {
        // The property this module exists for: "could not check" must never be
        // reported as "fine". A malformed receipt is not evidence of
        // convergence.
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        write(
            &sandbox
                .path()
                .join(super::DEFAULT_RECEIPT_RELATIVE_DIR)
                .join("macstudio.json"),
            "{not valid json",
        );

        let status = check(sandbox.path(), "Daniels-Mac-Studio");

        assert!(
            matches!(status, FleetEpochStatus::Unobservable { .. }),
            "got {status:?}"
        );
        assert!(status.blocks());
    }

    #[test]
    fn a_receipt_without_an_epoch_blocks() {
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        write(
            &sandbox
                .path()
                .join(super::DEFAULT_RECEIPT_RELATIVE_DIR)
                .join("macstudio.json"),
            r#"{"host": "macstudio", "hostname": "Daniels-Mac-Studio"}"#,
        );

        assert!(check(sandbox.path(), "Daniels-Mac-Studio").blocks());
    }

    #[test]
    fn each_host_is_checked_against_its_own_receipt() {
        // One converged host must not vouch for another.
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        seed_receipt(sandbox.path(), "macstudio", "Daniels-Mac-Studio", 7);

        assert!(!check(sandbox.path(), "Daniels-Mac-Studio").blocks());
        assert!(
            check(sandbox.path(), "m5").blocks(),
            "m5 has no receipt and must not inherit macstudio's convergence"
        );
    }

    #[test]
    fn an_unrelated_hosts_malformed_receipt_blocks_deterministically() {
        // Reading every file in the directory must not silently skip a sibling
        // host's malformed receipt. Directory iteration order must not decide
        // whether the same receipt set is observable.
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        seed_receipt(sandbox.path(), "macstudio", "Daniels-Mac-Studio", 7);
        write(
            &sandbox
                .path()
                .join(super::DEFAULT_RECEIPT_RELATIVE_DIR)
                .join("m5.json"),
            "{not valid json",
        );

        let status = check(sandbox.path(), "Daniels-Mac-Studio");

        assert!(
            status.blocks(),
            "a malformed receipt in the directory is unobservable, not ignorable: {status:?}"
        );
    }
}
