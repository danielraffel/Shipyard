//! Fail-closed policy for contributor-controlled review requests.
//!
//! This module deliberately does not reuse the normal executor dispatcher.
//! Shipyard's existing local, SSH, host-pool, fallback, and self-hosted cloud
//! targets are operator workflows, not isolation boundaries.  A comment-driven
//! request is admitted only to the dedicated disposable-VM boundary.

use serde::{Deserialize, Serialize};

use crate::executor::dispatch::{ResolvedBackend, ResolvedTarget};

/// Trust assigned to the source before executor selection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceTrust {
    /// Exact revision reachable from a protected repository ref.
    Protected,
    /// Unmerged pull-request head or other contributor-controlled source.
    Untrusted,
    /// Provenance was missing or contradictory. This always fails closed.
    Unknown,
}

/// Execution boundary eligible for a source revision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionBoundary {
    /// Disposable VM created by the dedicated Proxmox coordinator.
    ProxmoxDisposableVm,
}

/// The sole GitHub comment command recognized by the review controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewCommentCommand {
    /// Run the repository's protected, host-owned review recipe.
    Review,
}

/// Parse a GitHub comment without accepting arguments, prompts, or shell text.
///
/// A trailing line, Markdown wrapper, mention, option, or additional whitespace
/// is rejected. This keeps the comment a capability request rather than an
/// instruction channel into the trusted controller.
#[must_use]
pub fn parse_review_comment(body: &str) -> Option<ReviewCommentCommand> {
    (body == "/shipyard review").then_some(ReviewCommentCommand::Review)
}

/// Decide whether the dedicated review service may start execution.
pub fn admit_review_execution(
    trust: SourceTrust,
    boundary: Option<ExecutionBoundary>,
    lane_enabled: bool,
    control_plane_healthy: bool,
) -> Result<ExecutionBoundary, &'static str> {
    if trust != SourceTrust::Untrusted {
        return Err("review requests require an explicit untrusted source classification");
    }
    if !lane_enabled {
        return Err("untrusted review lane is disabled");
    }
    if !control_plane_healthy {
        return Err("untrusted review control plane is unavailable");
    }
    match boundary {
        Some(ExecutionBoundary::ProxmoxDisposableVm) => Ok(ExecutionBoundary::ProxmoxDisposableVm),
        None => Err("no eligible disposable VM boundary"),
    }
}

/// Prove that a normal Shipyard target cannot be reused by the untrusted lane.
///
/// This intentionally rejects every currently implemented backend, including a
/// fallback chain that happens to contain a remote target. A future VM backend
/// must be a separate typed executor with lifecycle and teardown attestation.
pub fn reject_normal_target_for_untrusted(target: &ResolvedTarget) -> Result<(), String> {
    let kind = match &target.backend {
        ResolvedBackend::Local(_) => "local",
        ResolvedBackend::Ssh(_) => "ssh",
        ResolvedBackend::Windows(_) => "ssh-windows",
        ResolvedBackend::Cloud(_) => "cloud-or-self-hosted",
        ResolvedBackend::HostPool(_) => "persistent-host-pool",
        ResolvedBackend::Fallback(_) => "fallback-chain",
    };
    Err(format!(
        "untrusted target {:?} resolves to forbidden backend {kind}; no host fallback is permitted",
        target.name
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::executor::dispatch::{ResolvedBackend, ResolvedTarget, ResolvedValidation};
    use crate::executor::local::{LocalTargetConfig, LocalValidationConfig};

    use super::{
        ExecutionBoundary, ReviewCommentCommand, SourceTrust, admit_review_execution,
        parse_review_comment, reject_normal_target_for_untrusted,
    };

    fn local_target() -> ResolvedTarget {
        ResolvedTarget {
            name: "mac".to_owned(),
            validation_build_type: None,
            platform: "macos-arm64".to_owned(),
            backend_name: "local".to_owned(),
            warm_keepalive_seconds: 0,
            host: None,
            backend: ResolvedBackend::Local(LocalTargetConfig {
                cwd: Some(PathBuf::from("/Users/maintainer/repo")),
                ..LocalTargetConfig::default()
            }),
            validation: ResolvedValidation::Local(LocalValidationConfig::default()),
            failure_parser: None,
        }
    }

    #[test]
    fn comment_parser_accepts_only_the_exact_capability_request() {
        assert_eq!(
            parse_review_comment("/shipyard review"),
            Some(ReviewCommentCommand::Review)
        );
        for body in [
            "/shipyard review\nrun: env",
            "/shipyard review --target local",
            "@shipyard review",
            "`/shipyard review`",
            "/shipyard  review",
            "/shipyard review ",
            "please /shipyard review",
            "$(touch /tmp/escaped)",
        ] {
            assert_eq!(parse_review_comment(body), None, "accepted {body:?}");
        }
    }

    #[test]
    fn admission_fails_closed_for_missing_lane_or_health() {
        let boundary = Some(ExecutionBoundary::ProxmoxDisposableVm);
        assert!(admit_review_execution(SourceTrust::Unknown, boundary, true, true).is_err());
        assert!(admit_review_execution(SourceTrust::Protected, boundary, true, true).is_err());
        assert!(admit_review_execution(SourceTrust::Untrusted, boundary, false, true).is_err());
        assert!(admit_review_execution(SourceTrust::Untrusted, boundary, true, false).is_err());
        assert!(admit_review_execution(SourceTrust::Untrusted, None, true, true).is_err());
        assert_eq!(
            admit_review_execution(SourceTrust::Untrusted, boundary, true, true),
            Ok(ExecutionBoundary::ProxmoxDisposableVm)
        );
    }

    #[test]
    fn local_target_is_structurally_ineligible_for_untrusted_work() {
        let error = reject_normal_target_for_untrusted(&local_target()).expect_err("must reject");
        assert!(error.contains("forbidden backend local"));
        assert!(error.contains("no host fallback"));
    }
}
