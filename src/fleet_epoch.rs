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
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Manifest path relative to the repository root.
pub const MANIFEST_RELATIVE_PATH: &str = "planning/fleet/manifest.toml";

/// Receipt directory relative to the repository root.
pub const RECEIPT_RELATIVE_DIR: &str = ".shipyard.local/fleet-receipts";

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

/// Check whether `host` has converged to the epoch declared under `repo_root`.
#[must_use]
pub fn check(repo_root: &Path, host: &str) -> FleetEpochStatus {
    let manifest_path = repo_root.join(MANIFEST_RELATIVE_PATH);
    if !manifest_path.exists() {
        return FleetEpochStatus::NotConfigured {
            detail: format!("{} does not exist", manifest_path.display()),
        };
    }
    let Some(declared) = read_declared_epoch(&manifest_path) else {
        return match std::fs::read_to_string(&manifest_path) {
            Ok(_) => FleetEpochStatus::NotConfigured {
                detail: format!("{} declares no epoch", manifest_path.display()),
            },
            Err(error) => FleetEpochStatus::Unobservable {
                detail: format!("cannot read {}: {error}", manifest_path.display()),
            },
        };
    };

    match read_applied_epoch(&receipt_path(repo_root, host)) {
        Ok(Some(applied)) if applied >= declared => {
            FleetEpochStatus::Converged { declared, applied }
        }
        Ok(applied) => FleetEpochStatus::Behind {
            declared,
            applied,
            host: host.to_owned(),
        },
        Err(detail) => FleetEpochStatus::Unobservable { detail },
    }
}

/// Path of the receipt a host writes after applying the manifest.
#[must_use]
pub fn receipt_path(repo_root: &Path, host: &str) -> PathBuf {
    repo_root
        .join(RECEIPT_RELATIVE_DIR)
        .join(format!("{host}.toml"))
}

fn read_declared_epoch(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let table = text.parse::<toml::Table>().ok()?;
    table.get("epoch")?.as_integer()?.try_into().ok()
}

/// Read the epoch a host recorded, distinguishing "never applied" from
/// "receipt exists but is unreadable".
fn read_applied_epoch(path: &Path) -> Result<Option<u64>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read receipt {}: {error}", path.display()))?;
    let table = text
        .parse::<toml::Table>()
        .map_err(|error| format!("cannot parse receipt {}: {error}", path.display()))?;
    let epoch = table
        .get("epoch")
        .ok_or_else(|| format!("receipt {} declares no epoch", path.display()))?
        .as_integer()
        .ok_or_else(|| format!("receipt {} epoch is not an integer", path.display()))?;
    u64::try_from(epoch)
        .map(Some)
        .map_err(|_| format!("receipt {} epoch is negative", path.display()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::{FleetEpochStatus, check, receipt_path};

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, body).expect("write");
    }

    fn seed_manifest(root: &Path, body: &str) {
        write(&root.join(super::MANIFEST_RELATIVE_PATH), body);
    }

    #[test]
    fn a_repo_with_no_manifest_is_not_participating() {
        // Absence must never block: most repositories will never have a fleet
        // manifest, and this check must not turn that into a failure.
        let sandbox = TempDir::new().expect("tempdir");

        let status = check(sandbox.path(), "m3");

        assert!(matches!(status, FleetEpochStatus::NotConfigured { .. }));
        assert!(!status.blocks());
    }

    #[test]
    fn a_manifest_without_an_epoch_is_not_participating() {
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "[hosts.m3]\ntart_home = \"/x\"\n");

        let status = check(sandbox.path(), "m3");

        assert!(matches!(status, FleetEpochStatus::NotConfigured { .. }));
        assert!(!status.blocks());
    }

    #[test]
    fn control_a_host_at_the_declared_epoch_is_converged() {
        // Control for the blocking tests below: proves a block is the epoch
        // gap, not a fixture that can never converge.
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        write(&receipt_path(sandbox.path(), "m3"), "epoch = 7\n");

        let status = check(sandbox.path(), "m3");

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
    fn a_host_ahead_of_the_manifest_is_still_converged() {
        // A host that applied a newer epoch (e.g. mid-rollout, before the
        // manifest commit landed in this checkout) is not stale.
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        write(&receipt_path(sandbox.path(), "m3"), "epoch = 9\n");

        assert!(!check(sandbox.path(), "m3").blocks());
    }

    #[test]
    fn a_host_behind_the_manifest_blocks() {
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        write(&receipt_path(sandbox.path(), "m3"), "epoch = 6\n");

        let status = check(sandbox.path(), "m3");

        assert_eq!(
            status,
            FleetEpochStatus::Behind {
                declared: 7,
                applied: Some(6),
                host: "m3".to_owned(),
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

        let status = check(sandbox.path(), "m3");

        assert_eq!(
            status,
            FleetEpochStatus::Behind {
                declared: 7,
                applied: None,
                host: "m3".to_owned(),
            }
        );
        assert!(status.blocks());
    }

    #[test]
    fn an_unreadable_receipt_blocks_rather_than_passing() {
        // The property this whole module exists for: "could not check" must
        // never be reported as "fine". A malformed receipt is not evidence of
        // convergence.
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        write(
            &receipt_path(sandbox.path(), "m3"),
            "this is not valid toml = = =\n",
        );

        let status = check(sandbox.path(), "m3");

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
            &receipt_path(sandbox.path(), "m3"),
            "applied_at = \"now\"\n",
        );

        assert!(check(sandbox.path(), "m3").blocks());
    }

    #[test]
    fn each_host_is_checked_against_its_own_receipt() {
        // One converged host must not vouch for another.
        let sandbox = TempDir::new().expect("tempdir");
        seed_manifest(sandbox.path(), "epoch = 7\n");
        write(&receipt_path(sandbox.path(), "m3"), "epoch = 7\n");

        assert!(!check(sandbox.path(), "m3").blocks());
        assert!(
            check(sandbox.path(), "m5").blocks(),
            "m5 has no receipt and must not inherit m3's convergence"
        );
    }
}
