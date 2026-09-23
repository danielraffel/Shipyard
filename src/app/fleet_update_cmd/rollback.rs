//! Automatic rollback of a host that updated but did not verify.
//!
//! A host that fails post-update verification is left in a state nobody chose:
//! a new binary that does not answer as itself, a daemon on the wrong version,
//! or guards that did not land. The rollout reinstalls the version that host
//! ran before the update through the same governed path (release authority,
//! staged install, daemon refresh), verifies it the same way, and reports
//! "rolled back to vX". When the rollback cannot be performed or does not
//! verify, the host is reported loudly as needing an operator.

use serde::Serialize;

use super::verify::HostVerification;
use super::{HostUpdateEvidence, HostUpdatePlan, MIN_FLEET_UPDATE_TARGET, tag_at_least};

/// What a rollback attempt did.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "rollback", rename_all = "snake_case")]
pub(super) enum RollbackOutcome {
    /// The previous version is installed again and verified.
    RolledBack {
        to: String,
        verification: Box<HostVerification>,
    },
    /// No rollback happened, or it did not verify. The host needs an operator.
    Failed { to: Option<String>, reason: String },
}

impl RollbackOutcome {
    pub(super) fn summary(&self) -> String {
        match self {
            Self::RolledBack { to, .. } => format!("rolled back to {to} (verified)"),
            Self::Failed {
                to: Some(to),
                reason,
            } => {
                format!("ROLLBACK TO {to} FAILED: {reason}; the host needs an operator")
            }
            Self::Failed { to: None, reason } => {
                format!("ROLLBACK NOT POSSIBLE: {reason}; the host needs an operator")
            }
        }
    }
}

/// The tag this host ran before the update, if a governed rollback to it is
/// possible.
pub(super) fn rollback_target(
    plan: &HostUpdatePlan,
    evidence: &HostUpdateEvidence,
) -> Result<String, String> {
    let previous = evidence.before_pair.primary.semantic_version.trim();
    if previous.is_empty() {
        return Err("the version installed before the update was not recorded".to_owned());
    }
    let tag = format!("v{previous}");
    if tag == plan.target {
        return Err(format!(
            "the host already ran {tag} before this update, so there is no earlier version to restore"
        ));
    }
    if !tag_at_least(&tag, MIN_FLEET_UPDATE_TARGET) {
        return Err(format!(
            "the previous version {tag} predates the governed fleet-update path"
        ));
    }
    Ok(tag)
}
