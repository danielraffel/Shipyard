//! One-shot authority required before a continuation delivery may start.
//!
//! Stored route labels are provenance only.  This module accepts fresh GitHub
//! and terminal observations, binds them to one exact delivery fence, and
//! returns a non-cloneable witness that the provider boundary must consume.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

pub(crate) use crate::terminal_delivery_authority::TerminalMutationEndpoint;

use super::digest;

const MAX_GITHUB_OBSERVATION_AGE: Duration = Duration::seconds(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeliveryAuthorityExpectation {
    pub(crate) installation_id: u64,
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) head_sha: String,
    pub(crate) base_ref: String,
    pub(crate) base_sha: String,
    pub(crate) requested_terminal_instance: String,
    pub(crate) requested_process: ProcessIncarnation,
    pub(crate) native_session_id: String,
    pub(crate) source_work_generation: u64,
    pub(crate) source_owner_generation: u64,
    pub(crate) target_work_generation: u64,
    pub(crate) target_owner_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitHubAuthorityObservation {
    pub(crate) app_authenticated: bool,
    pub(crate) installation_id: u64,
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) head_sha: String,
    pub(crate) base_ref: String,
    pub(crate) base_sha: String,
    pub(crate) observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessIncarnation {
    pub(crate) boot_id: String,
    pub(crate) pid: u32,
    pub(crate) start_identity: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalAuthorityObservation {
    pub(crate) requested_terminal_instance: String,
    pub(crate) actual_terminal_instance: String,
    pub(crate) process: ProcessIncarnation,
    pub(crate) native_session_id: String,
    /// Exact endpoint on which the terminal evidence was observed.
    pub(crate) mutation_endpoint: TerminalMutationEndpoint,
    pub(crate) observed_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryAuthorityRefusal {
    GitHubAppAuthorityUnavailable,
    InstallationMismatch,
    RepositoryMismatch,
    PullRequestMismatch,
    HeadMismatch,
    BaseRefMissing,
    BaseRefMismatch,
    ObservationStale,
    TerminalAuthorityUnavailable,
    TerminalInstanceMismatch,
    ProcessIncarnationMismatch,
    NativeSessionMismatch,
    GenerationMismatch,
    MethodMissing,
    NoTerminalMatch,
    MultipleTerminalMatches,
    StaticRouteMetadataOnly,
    DirectProviderForbidden,
}

impl DeliveryAuthorityRefusal {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::GitHubAppAuthorityUnavailable => "github_app_authority_unavailable",
            Self::InstallationMismatch => "github_installation_mismatch",
            Self::RepositoryMismatch => "repository_mismatch",
            Self::PullRequestMismatch => "pull_request_mismatch",
            Self::HeadMismatch => "head_mismatch",
            Self::BaseRefMissing => "base_ref_missing",
            Self::BaseRefMismatch => "base_ref_mismatch",
            Self::ObservationStale => "github_observation_stale",
            Self::TerminalAuthorityUnavailable => "terminal_authority_unavailable",
            Self::TerminalInstanceMismatch => "terminal_instance_mismatch",
            Self::ProcessIncarnationMismatch => "process_incarnation_mismatch",
            Self::NativeSessionMismatch => "native_session_mismatch",
            Self::GenerationMismatch => "generation_mismatch",
            Self::MethodMissing => "terminal_method_missing",
            Self::NoTerminalMatch => "terminal_no_match",
            Self::MultipleTerminalMatches => "terminal_multiple_matches",
            Self::StaticRouteMetadataOnly => "static_route_metadata_only",
            Self::DirectProviderForbidden => "direct_provider_forbidden",
        }
    }
}

pub(crate) trait DeliveryAuthorityProbe {
    fn observe_github(
        &mut self,
        expected: &DeliveryAuthorityExpectation,
    ) -> Result<GitHubAuthorityObservation, DeliveryAuthorityRefusal>;

    /// This is deliberately one call returning one non-reusable observation.
    fn verify_terminal_once(
        &mut self,
        expected: &DeliveryAuthorityExpectation,
    ) -> Result<TerminalAuthorityObservation, DeliveryAuthorityRefusal>;
}

#[derive(Serialize)]
struct GitHubReceiptPayload<'a> {
    domain: &'static str,
    installation_id: u64,
    repository: &'a str,
    pull_request: u64,
    head_sha: &'a str,
    base_ref: &'a str,
    base_sha: &'a str,
    observed_at: &'a str,
}

/// Non-cloneable witness consumed by exactly one provider operation.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct DeliveryAuthorization {
    github_receipt_digest: String,
    target: DeliveryAuthorizationTarget,
    mutation_endpoint: TerminalMutationEndpoint,
    source_work_generation: u64,
    source_owner_generation: u64,
    target_work_generation: u64,
    target_owner_generation: u64,
}

#[derive(Debug, Eq, PartialEq)]
enum DeliveryAuthorizationTarget {
    OriginalSession {
        terminal_instance: String,
        process: ProcessIncarnation,
        native_session_id: String,
    },
    FreshCheckpoint,
}

/// Non-cloneable, read-only authority for inspecting one original provider
/// idempotency fence. It cannot authorize launch, prompt delivery, or terminal
/// mutation. The provider boundary must consume it exactly once.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ReconciliationAuthorization {
    github_receipt_digest: String,
    terminal_endpoint: TerminalMutationEndpoint,
    fence_digest: String,
}

impl ReconciliationAuthorization {
    pub(crate) fn receipt_digest(&self) -> &str {
        &self.github_receipt_digest
    }

    pub(crate) fn terminal_endpoint(&self) -> &TerminalMutationEndpoint {
        &self.terminal_endpoint
    }

    pub(crate) fn fence_digest(&self) -> &str {
        &self.fence_digest
    }

    #[cfg(test)]
    pub(crate) fn for_test(fence_digest: String) -> Self {
        Self {
            github_receipt_digest: "0".repeat(64),
            terminal_endpoint: TerminalMutationEndpoint::Cmux {
                executable_path: "/test/cmux".to_owned(),
                socket_path: "/test/cmux.sock".to_owned(),
            },
            fence_digest,
        }
    }
}

impl DeliveryAuthorization {
    pub(crate) fn receipt_digest(&self) -> &str {
        &self.github_receipt_digest
    }

    pub(crate) fn terminal_instance(&self) -> &str {
        match &self.target {
            DeliveryAuthorizationTarget::OriginalSession {
                terminal_instance, ..
            } => terminal_instance,
            DeliveryAuthorizationTarget::FreshCheckpoint => "",
        }
    }

    pub(crate) fn is_fresh_checkpoint(&self) -> bool {
        matches!(self.target, DeliveryAuthorizationTarget::FreshCheckpoint)
    }

    pub(crate) fn into_mutation_endpoint_for(
        self,
        work_generation: u64,
        owner_generation: u64,
    ) -> Result<TerminalMutationEndpoint, DeliveryAuthorityRefusal> {
        if self.source_work_generation != work_generation
            || self.source_owner_generation != owner_generation
            || self.target_work_generation != work_generation
            || self.target_owner_generation != owner_generation
        {
            return Err(DeliveryAuthorityRefusal::GenerationMismatch);
        }
        Ok(self.mutation_endpoint)
    }

    #[cfg(test)]
    pub(crate) fn for_test(work_generation: u64, owner_generation: u64) -> Self {
        Self {
            github_receipt_digest: "0".repeat(64),
            target: DeliveryAuthorizationTarget::OriginalSession {
                terminal_instance: "test:terminal".to_owned(),
                process: ProcessIncarnation {
                    boot_id: "test-boot".to_owned(),
                    pid: 1,
                    start_identity: "test-start".to_owned(),
                },
                native_session_id: "test-session".to_owned(),
            },
            mutation_endpoint: TerminalMutationEndpoint::Cmux {
                executable_path: "/test/cmux-a".to_owned(),
                socket_path: "/test/cmux-a.sock".to_owned(),
            },
            source_work_generation: work_generation,
            source_owner_generation: owner_generation,
            target_work_generation: work_generation,
            target_owner_generation: owner_generation,
        }
    }
}

pub(crate) fn verify_delivery_authority<P: DeliveryAuthorityProbe>(
    probe: &mut P,
    expected: &DeliveryAuthorityExpectation,
) -> Result<DeliveryAuthorization, DeliveryAuthorityRefusal> {
    verify_delivery_authority_inner(probe, expected, None)
}

/// Mint read-only reconciliation authority without requiring the original
/// terminal occupant to remain alive. Fresh GitHub App/head/base evidence is
/// still mandatory, and the witness is bound to both the authenticated
/// terminal endpoint and immutable provider-delivery fence.
pub(crate) fn verify_reconciliation_authority<P: DeliveryAuthorityProbe>(
    probe: &mut P,
    expected: &DeliveryAuthorityExpectation,
    terminal_endpoint: TerminalMutationEndpoint,
    fence_digest: String,
) -> Result<ReconciliationAuthorization, DeliveryAuthorityRefusal> {
    let github_receipt_digest = verify_github_authority(probe, expected, None)?;
    validate_fence_digest(&fence_digest)?;
    Ok(ReconciliationAuthorization {
        github_receipt_digest,
        terminal_endpoint,
        fence_digest,
    })
}

#[cfg(test)]
pub(crate) fn verify_delivery_authority_at<P: DeliveryAuthorityProbe>(
    probe: &mut P,
    expected: &DeliveryAuthorityExpectation,
    now: DateTime<Utc>,
) -> Result<DeliveryAuthorization, DeliveryAuthorityRefusal> {
    verify_delivery_authority_inner(probe, expected, Some(now))
}

fn verify_delivery_authority_inner<P: DeliveryAuthorityProbe>(
    probe: &mut P,
    expected: &DeliveryAuthorityExpectation,
    fixed_now: Option<DateTime<Utc>>,
) -> Result<DeliveryAuthorization, DeliveryAuthorityRefusal> {
    let github_receipt_digest = verify_github_authority(probe, expected, fixed_now)?;

    let terminal = probe.verify_terminal_once(expected)?;
    verify_terminal_authority(expected, &terminal, fixed_now)?;

    Ok(DeliveryAuthorization {
        github_receipt_digest,
        target: DeliveryAuthorizationTarget::OriginalSession {
            terminal_instance: terminal.actual_terminal_instance,
            process: terminal.process,
            native_session_id: terminal.native_session_id,
        },
        mutation_endpoint: terminal.mutation_endpoint,
        source_work_generation: expected.source_work_generation,
        source_owner_generation: expected.source_owner_generation,
        target_work_generation: expected.target_work_generation,
        target_owner_generation: expected.target_owner_generation,
    })
}

/// Verify one exact delivery boundary and choose the only legal target. A live
/// original receives an in-place wake. A fresh checkpoint owner is authorized
/// only when the same one-shot terminal probe definitively proves that the
/// original process is absent or has a different incarnation.
pub(crate) fn verify_delivery_or_fresh_authority<P: DeliveryAuthorityProbe>(
    probe: &mut P,
    expected: &DeliveryAuthorityExpectation,
    terminal_endpoint: TerminalMutationEndpoint,
    fence_digest: &str,
) -> Result<DeliveryAuthorization, DeliveryAuthorityRefusal> {
    let github_receipt_digest = verify_github_authority(probe, expected, None)?;
    match probe.verify_terminal_once(expected) {
        Ok(terminal) => {
            verify_terminal_authority(expected, &terminal, None)?;
            Ok(DeliveryAuthorization {
                github_receipt_digest,
                target: DeliveryAuthorizationTarget::OriginalSession {
                    terminal_instance: terminal.actual_terminal_instance,
                    process: terminal.process,
                    native_session_id: terminal.native_session_id,
                },
                mutation_endpoint: terminal.mutation_endpoint,
                source_work_generation: expected.source_work_generation,
                source_owner_generation: expected.source_owner_generation,
                target_work_generation: expected.target_work_generation,
                target_owner_generation: expected.target_owner_generation,
            })
        }
        Err(
            DeliveryAuthorityRefusal::NoTerminalMatch
            | DeliveryAuthorityRefusal::ProcessIncarnationMismatch,
        ) => {
            validate_fence_digest(fence_digest)?;
            Ok(DeliveryAuthorization {
                github_receipt_digest,
                target: DeliveryAuthorizationTarget::FreshCheckpoint,
                mutation_endpoint: terminal_endpoint,
                source_work_generation: expected.source_work_generation,
                source_owner_generation: expected.source_owner_generation,
                target_work_generation: expected.target_work_generation,
                target_owner_generation: expected.target_owner_generation,
            })
        }
        Err(refusal) => Err(refusal),
    }
}

fn validate_fence_digest(fence_digest: &str) -> Result<(), DeliveryAuthorityRefusal> {
    if fence_digest.len() == 64
        && fence_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(DeliveryAuthorityRefusal::TerminalAuthorityUnavailable)
    }
}

fn verify_github_authority<P: DeliveryAuthorityProbe>(
    probe: &mut P,
    expected: &DeliveryAuthorityExpectation,
    fixed_now: Option<DateTime<Utc>>,
) -> Result<String, DeliveryAuthorityRefusal> {
    if expected.base_ref.is_empty() {
        return Err(DeliveryAuthorityRefusal::BaseRefMissing);
    }
    let github = probe.observe_github(expected)?;
    if !github.app_authenticated {
        return Err(DeliveryAuthorityRefusal::GitHubAppAuthorityUnavailable);
    }
    if github.installation_id != expected.installation_id {
        return Err(DeliveryAuthorityRefusal::InstallationMismatch);
    }
    if github.repository != expected.repository {
        return Err(DeliveryAuthorityRefusal::RepositoryMismatch);
    }
    if github.pull_request != expected.pull_request {
        return Err(DeliveryAuthorityRefusal::PullRequestMismatch);
    }
    if github.head_sha != expected.head_sha {
        return Err(DeliveryAuthorityRefusal::HeadMismatch);
    }
    if github.base_ref != expected.base_ref {
        return Err(DeliveryAuthorityRefusal::BaseRefMismatch);
    }
    if github.base_sha != expected.base_sha {
        return Err(DeliveryAuthorityRefusal::BaseRefMismatch);
    }
    let age = fixed_now
        .unwrap_or_else(Utc::now)
        .signed_duration_since(github.observed_at);
    if age < Duration::zero() || age > MAX_GITHUB_OBSERVATION_AGE {
        return Err(DeliveryAuthorityRefusal::ObservationStale);
    }
    let observed_at = github.observed_at.to_rfc3339();
    let receipt = serde_json::to_vec(&GitHubReceiptPayload {
        domain: "shipyard-github-delivery-authority-v1",
        installation_id: github.installation_id,
        repository: &github.repository,
        pull_request: github.pull_request,
        head_sha: &github.head_sha,
        base_ref: &github.base_ref,
        base_sha: &github.base_sha,
        observed_at: &observed_at,
    })
    .expect("fixed GitHub authority receipt is serializable");

    Ok(digest(&receipt))
}

fn verify_terminal_authority(
    expected: &DeliveryAuthorityExpectation,
    terminal: &TerminalAuthorityObservation,
    fixed_now: Option<DateTime<Utc>>,
) -> Result<(), DeliveryAuthorityRefusal> {
    if terminal.requested_terminal_instance != expected.requested_terminal_instance {
        return Err(DeliveryAuthorityRefusal::TerminalInstanceMismatch);
    }
    if terminal.actual_terminal_instance != expected.requested_terminal_instance {
        return Err(DeliveryAuthorityRefusal::TerminalInstanceMismatch);
    }
    let terminal_age = fixed_now
        .unwrap_or_else(Utc::now)
        .signed_duration_since(terminal.observed_at);
    if terminal_age < Duration::zero() || terminal_age > MAX_GITHUB_OBSERVATION_AGE {
        return Err(DeliveryAuthorityRefusal::ObservationStale);
    }
    if terminal.process.pid == 0
        || terminal.process.boot_id.is_empty()
        || terminal.process.start_identity.is_empty()
        || terminal.process != expected.requested_process
    {
        return Err(DeliveryAuthorityRefusal::ProcessIncarnationMismatch);
    }
    if terminal.native_session_id != expected.native_session_id {
        return Err(DeliveryAuthorityRefusal::NativeSessionMismatch);
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/delivery_authority.rs"]
mod tests;
