//! One-shot authority required before a continuation delivery may start.
//!
//! Stored route labels are provenance only.  This module accepts fresh GitHub
//! and terminal observations, binds them to one exact delivery fence, and
//! returns a non-cloneable witness that the provider boundary must consume.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use super::digest;

const MAX_GITHUB_OBSERVATION_AGE: Duration = Duration::seconds(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeliveryAuthorityExpectation {
    pub(crate) installation_id: u64,
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) head_sha: String,
    pub(crate) base_ref: String,
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
    pub(crate) source_work_generation: u64,
    pub(crate) source_owner_generation: u64,
    pub(crate) target_work_generation: u64,
    pub(crate) target_owner_generation: u64,
    /// True only when the verifier observed a source+target generation CAS.
    pub(crate) transactionally_rebound: bool,
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
    observed_at: &'a str,
}

/// Non-cloneable witness consumed by exactly one provider operation.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct DeliveryAuthorization {
    github_receipt_digest: String,
    terminal_instance: String,
    process: ProcessIncarnation,
    native_session_id: String,
    source_work_generation: u64,
    source_owner_generation: u64,
    target_work_generation: u64,
    target_owner_generation: u64,
}

impl DeliveryAuthorization {
    pub(crate) fn receipt_digest(&self) -> &str {
        &self.github_receipt_digest
    }

    pub(crate) fn terminal_instance(&self) -> &str {
        &self.terminal_instance
    }

    #[cfg(test)]
    pub(crate) fn for_test(work_generation: u64, owner_generation: u64) -> Self {
        Self {
            github_receipt_digest: "0".repeat(64),
            terminal_instance: "test:terminal".to_owned(),
            process: ProcessIncarnation {
                boot_id: "test-boot".to_owned(),
                pid: 1,
                start_identity: "test-start".to_owned(),
            },
            native_session_id: "test-session".to_owned(),
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
    now: DateTime<Utc>,
) -> Result<DeliveryAuthorization, DeliveryAuthorityRefusal> {
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
    let age = now.signed_duration_since(github.observed_at);
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
        observed_at: &observed_at,
    })
    .expect("fixed GitHub authority receipt is serializable");

    let terminal = probe.verify_terminal_once(expected)?;
    if terminal.requested_terminal_instance != expected.requested_terminal_instance {
        return Err(DeliveryAuthorityRefusal::TerminalInstanceMismatch);
    }
    if terminal.actual_terminal_instance != expected.requested_terminal_instance
        && !terminal.transactionally_rebound
    {
        return Err(DeliveryAuthorityRefusal::TerminalInstanceMismatch);
    }
    let terminal_age = now.signed_duration_since(terminal.observed_at);
    if terminal_age < Duration::zero() || terminal_age > MAX_GITHUB_OBSERVATION_AGE {
        return Err(DeliveryAuthorityRefusal::ObservationStale);
    }
    if terminal.process.pid == 0
        || terminal.process.boot_id.is_empty()
        || terminal.process.start_identity.is_empty()
        || (!terminal.transactionally_rebound && terminal.process != expected.requested_process)
    {
        return Err(DeliveryAuthorityRefusal::ProcessIncarnationMismatch);
    }
    if terminal.native_session_id != expected.native_session_id {
        return Err(DeliveryAuthorityRefusal::NativeSessionMismatch);
    }
    let observed_generations = (
        terminal.source_work_generation,
        terminal.source_owner_generation,
        terminal.target_work_generation,
        terminal.target_owner_generation,
    );
    let expected_generations = (
        expected.source_work_generation,
        expected.source_owner_generation,
        expected.target_work_generation,
        expected.target_owner_generation,
    );
    if observed_generations != expected_generations {
        return Err(DeliveryAuthorityRefusal::GenerationMismatch);
    }

    Ok(DeliveryAuthorization {
        github_receipt_digest: digest(&receipt),
        terminal_instance: terminal.actual_terminal_instance,
        process: terminal.process,
        native_session_id: terminal.native_session_id,
        source_work_generation: terminal.source_work_generation,
        source_owner_generation: terminal.source_owner_generation,
        target_work_generation: terminal.target_work_generation,
        target_owner_generation: terminal.target_owner_generation,
    })
}

#[cfg(test)]
#[path = "tests/delivery_authority.rs"]
mod tests;
