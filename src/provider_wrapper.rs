//! Strict, bounded provider-wrapper protocol and execution boundary.
//!
//! This module deliberately owns no queue or lifecycle state. Callers persist
//! the delivery fence before invoking it and decide what to do with the typed
//! result afterward. In particular, `delivered` means only that the provider
//! accepted a session; it is not proof that the session reconstructed or
//! acknowledged its expected resume context.
#![allow(dead_code)] // Activated only by the later durable-consumer integration slice.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
#[cfg(any(target_os = "macos", all(test, unix)))]
use std::process::Stdio;
use std::time::Duration;
#[cfg(any(target_os = "macos", all(test, unix)))]
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(target_os = "macos")]
use crate::process::ProcessTree;
use crate::workstream_continuation_config::ProviderWrapperConfig;
use crate::workstream_continuation_config::registered_provider_route_shape;

#[cfg(unix)]
mod execution_sentinel;
#[cfg(target_os = "linux")]
mod linux_parent;
#[cfg(target_os = "macos")]
use execution_sentinel::terminate_sentinel_processes;
#[cfg(target_os = "linux")]
use execution_sentinel::{
    LinuxSentinelSupervisorSpecV3, LinuxSupervisorCleanupV3, LinuxSupervisorProviderV3,
    LinuxSupervisorResultV3, MAX_SPEC_BYTES, READY_FRAME, RESULT_FRAME_PREFIX,
    SPEC_ADMISSION_BUDGET,
};
#[cfg(target_os = "linux")]
use linux_parent::run_provider_wrapper_linux_supervised;
#[cfg(all(test, target_os = "linux"))]
use linux_parent::{
    finish_linux_supervisor_result, send_linux_supervisor_admission, write_linux_supervisor_spec,
};

/// Wire version 2 introduces tagged terminal endpoints. It is deliberately
/// incompatible with the cmux-only v1 request so mixed deployments refuse.
pub(crate) const PROVIDER_WRAPPER_SCHEMA_VERSION: u32 = 2;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_WRAPPER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 4 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TEARDOWN_BUDGET: Duration = Duration::from_millis(500);
// `lsof` can take multiple seconds to receive CPU on a saturated macOS build
// host. Keep cleanup bounded, but leave enough time to prove and kill an
// escaped setsid descendant instead of returning uncertain with it alive.
const SENTINEL_TEARDOWN_BUDGET: Duration = Duration::from_secs(10);
const EXECUTION_SENTINEL_FD_ENV: &str = "SHIPYARD_PROVIDER_SENTINEL_FD";
#[cfg(test)]
const PROVIDER_EXECUTION_TEST_CONCURRENCY: usize = 4;
#[cfg(test)]
static PROVIDER_EXECUTION_TEST_PERMITS: std::sync::Mutex<usize> =
    std::sync::Mutex::new(PROVIDER_EXECUTION_TEST_CONCURRENCY);
#[cfg(test)]
static PROVIDER_EXECUTION_TEST_READY: std::sync::Condvar = std::sync::Condvar::new();

#[cfg(test)]
struct ProviderExecutionTestPermit;

#[cfg(test)]
impl Drop for ProviderExecutionTestPermit {
    fn drop(&mut self) {
        if let Ok(mut available) = PROVIDER_EXECUTION_TEST_PERMITS.lock() {
            *available += 1;
            PROVIDER_EXECUTION_TEST_READY.notify_one();
        }
    }
}

#[cfg(test)]
fn provider_execution_test_permit() -> Result<ProviderExecutionTestPermit, ProviderWrapperRefusal> {
    let mut available = PROVIDER_EXECUTION_TEST_PERMITS
        .lock()
        .map_err(|_| refusal("provider wrapper test execution permits are poisoned"))?;
    while *available == 0 {
        available = PROVIDER_EXECUTION_TEST_READY
            .wait(available)
            .map_err(|_| refusal("provider wrapper test execution permits are poisoned"))?;
    }
    *available -= 1;
    Ok(ProviderExecutionTestPermit)
}

/// Exact operation requested from the protected wrapper.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderWrapperOperationV1 {
    Submit,
    Reconcile,
}

/// Exact current delivery fence plus a key derived from its stable fields.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderDeliveryFenceV1 {
    pub(crate) wake_id: String,
    pub(crate) work_item_id: String,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) route_ref: String,
    pub(crate) payload_digest: String,
    pub(crate) attempt: u64,
    pub(crate) consumer_epoch: u64,
    pub(crate) consumer_owner_ref: String,
    pub(crate) idempotency_key: String,
}

impl ProviderDeliveryFenceV1 {
    /// Set the key to the canonical digest of this exact delivery fence.
    pub(crate) fn bind_idempotency_key(&mut self) {
        self.idempotency_key = self.expected_idempotency_key();
    }

    fn expected_idempotency_key(&self) -> String {
        #[derive(Serialize)]
        struct Inputs<'a> {
            domain: &'static str,
            wake_id: &'a str,
            work_item_id: &'a str,
            work_generation: u64,
            owner_generation: u64,
            route_ref: &'a str,
            payload_digest: &'a str,
            attempt: u64,
        }
        let bytes = serde_json::to_vec(&Inputs {
            domain: "shipyard-provider-delivery-v1",
            wake_id: &self.wake_id,
            work_item_id: &self.work_item_id,
            work_generation: self.work_generation,
            owner_generation: self.owner_generation,
            route_ref: &self.route_ref,
            payload_digest: &self.payload_digest,
            attempt: self.attempt,
        })
        .expect("serializing fixed delivery inputs cannot fail");
        hex::encode(Sha256::digest(bytes))
    }
}

/// Immutable context a fresh provider session must reconstruct before owning work.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FreshResumeExpectationV1 {
    pub(crate) workstream_handle: String,
    pub(crate) context_url: Option<String>,
    pub(crate) plan_sha256: String,
    pub(crate) root_revision: u64,
    pub(crate) issue_revision: u64,
    pub(crate) material_event_revision: u64,
    pub(crate) projection_revision: u64,
    pub(crate) checkpoint_id: String,
    pub(crate) checkpoint_generation: u64,
    pub(crate) checkpoint_digest: String,
    pub(crate) repository: String,
    pub(crate) worktree_path: String,
    pub(crate) head_sha: String,
    pub(crate) expected_resume_context_digest: String,
    pub(crate) success_continuation_digest: String,
    pub(crate) failure_continuation_digest: String,
}

/// One strictly versioned wrapper request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderWrapperRequestV1 {
    pub(crate) schema_version: u32,
    pub(crate) operation: ProviderWrapperOperationV1,
    pub(crate) provider_id: String,
    pub(crate) adapter_id: String,
    pub(crate) delivery_fence: ProviderDeliveryFenceV1,
    /// Exact terminal endpoint accepted by the one-shot delivery authority.
    pub(crate) terminal_endpoint: TerminalEndpointV1,
    /// Whether this operation targets the proven live original session or a
    /// separately-authorized checkpoint-only replacement.
    pub(crate) delivery_target: ProviderDeliveryTargetV1,
    /// Exact private Subrouter route decoded from the protected launch profile.
    pub(crate) protected_route: ProtectedProviderRouteV1,
    pub(crate) resume_expectation: FreshResumeExpectationV1,
    pub(crate) launch_options: ProviderLaunchOptionsV1,
}

/// The exact cmux process/socket pair authorized for this delivery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CmuxEndpointV1 {
    pub(crate) executable_path: String,
    pub(crate) socket_path: String,
    /// Apple signing team accepted by trusted machine-global product policy.
    pub(crate) signing_team_id: String,
}

/// Terminal-neutral transport authority. A registered shape is not activation:
/// each adapter must still prove its capabilities before any terminal action.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "adapter", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TerminalEndpointV1 {
    Cmux(CmuxEndpointV1),
    HerdR {
        socket_path: String,
        server_incarnation: Option<String>,
        direct_fresh_launch_proven: bool,
    },
}

/// Prompt-free provider route retained only inside protected execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProtectedProviderRouteV1 {
    /// Exact Subrouter resume argv. The adapter appends only Shipyard's prompt.
    pub(crate) argv: Vec<String>,
    /// Exact no-session Subrouter argv used only for fresh checkpoint launch.
    pub(crate) fresh_argv: Vec<String>,
    /// Digest of the exact Subrouter executable accepted by the profile.
    pub(crate) executable_sha256: String,
    /// Exact private routing headers/environment from the protected profile.
    pub(crate) environment: BTreeMap<String, String>,
    /// Selected Subrouter account, retained only inside the protected request.
    pub(crate) account_id: Option<String>,
    /// Native provider session that the exact resume argv must select.
    pub(crate) native_session_id: String,
    /// Digest of the protected profile from which this route was decoded.
    pub(crate) profile_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ProviderDeliveryTargetV1 {
    OriginalSession { surface_id: String },
    FreshCheckpoint,
    ReconcileOnly,
}

/// Narrow user intent that the digest-pinned provider adapter translates into
/// its own command grammar. Arbitrary argv never crosses this boundary.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderLaunchOptionsV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: Option<ProviderReasoningEffortV1>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderReasoningEffortV1 {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultra,
}

/// The only success meaning exposed by this boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderAcceptanceV1 {
    ProviderSessionAccepted,
}

/// Strict provider result. Variant fields make retry safety explicit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ProviderWrapperOutcomeV1 {
    Delivered {
        acceptance: ProviderAcceptanceV1,
        provider_session_ref: String,
        receipt_digest: String,
    },
    Retryable {
        launch_state: NotAcceptedV1,
        error_digest: String,
    },
    Uncertain {
        launch_state: UnknownV1,
        evidence_digest: String,
    },
    Rejected {
        launch_state: NotAcceptedV1,
        error_digest: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NotAcceptedV1 {
    NotAccepted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnknownV1 {
    Unknown,
}

/// One strictly bound wrapper response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderWrapperResponseV1 {
    pub(crate) schema_version: u32,
    pub(crate) operation: ProviderWrapperOperationV1,
    pub(crate) provider_id: String,
    pub(crate) adapter_id: String,
    pub(crate) idempotency_key: String,
    pub(crate) outcome: ProviderWrapperOutcomeV1,
}

/// Result returned to the future durable consumer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProviderWrapperRunResult {
    Delivered {
        provider_session_ref: String,
        provider_receipt_digest: String,
        response_receipt: ProtectedProviderResponseV1,
    },
    Retryable {
        error_digest: String,
        response_receipt: ProtectedProviderResponseV1,
    },
    Uncertain {
        evidence_digest: String,
        response_receipt: Option<ProtectedProviderResponseV1>,
    },
    Rejected {
        error_digest: String,
        response_receipt: ProtectedProviderResponseV1,
    },
}

/// Canonical strict wrapper response ready for protected-object persistence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProtectedProviderResponseV1 {
    pub(crate) canonical_bytes: Vec<u8>,
    pub(crate) response_digest: String,
}

/// Refusal before provider execution. The caller must not reinterpret this as delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderWrapperRefusal(String);

impl Display for ProviderWrapperRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProviderWrapperRefusal {}

/// Explicit non-secret environment passed after `env_clear`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProviderWrapperEnvironment(BTreeMap<String, OsString>);

impl ProviderWrapperEnvironment {
    /// Construct an environment from the small portable wrapper allowlist.
    pub(crate) fn new(
        entries: impl IntoIterator<Item = (String, OsString)>,
    ) -> Result<Self, ProviderWrapperRefusal> {
        let mut values = BTreeMap::new();
        for (name, value) in entries {
            if !matches!(
                name.as_str(),
                "HOME" | "TMPDIR" | "SYSTEMROOT" | "USERPROFILE"
            ) {
                return Err(refusal("wrapper environment key is not allowlisted"));
            }
            if value.len() > MAX_VALUE_BYTES || values.insert(name, value).is_some() {
                return Err(refusal(
                    "wrapper environment value is invalid or duplicated",
                ));
            }
        }
        Ok(Self(values))
    }
}

/// Whether this target has a protected exact-snapshot execution boundary.
#[must_use]
pub(crate) const fn provider_wrapper_execution_supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

/// The platform guarantee supplied after the configured bytes are verified.
///
/// Darwin has no public descriptor-based exec primitive. Its guarantee is
/// therefore explicitly scoped to Shipyard's control-plane threat model: the
/// machine account and other same-UID processes are trusted. This is the same
/// boundary protecting Shipyard's machine-global policy and ledger database;
/// an actively hostile same-UID process can replace either authority already.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderExecutableIdentityBoundary {
    /// Linux executes a write/grow/shrink/seal-protected memfd.
    KernelSealedSnapshot,
    /// macOS executes a private, reverified snapshot with trusted same-UID code.
    TrustedSameUidPrivateSnapshot,
    /// The target cannot prove a supported execution identity.
    Unsupported,
}

#[must_use]
pub(crate) const fn provider_executable_identity_boundary() -> ProviderExecutableIdentityBoundary {
    #[cfg(target_os = "linux")]
    {
        ProviderExecutableIdentityBoundary::KernelSealedSnapshot
    }
    #[cfg(target_os = "macos")]
    {
        ProviderExecutableIdentityBoundary::TrustedSameUidPrivateSnapshot
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        ProviderExecutableIdentityBoundary::Unsupported
    }
}

/// Execute one request through the immutable configured wrapper.
pub(crate) fn run_provider_wrapper(
    config: &ProviderWrapperConfig,
    environment: &ProviderWrapperEnvironment,
    request: &ProviderWrapperRequestV1,
) -> Result<ProviderWrapperRunResult, ProviderWrapperRefusal> {
    // The subprocess fixtures invoke host-wide process observation. Bound
    // their test-only concurrency without serializing unrelated private
    // sentinels or making a multi-call test wait behind the whole suite.
    #[cfg(test)]
    let _test_execution = provider_execution_test_permit()?;
    validate_request(config, request)?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = environment;
        Ok(uncertain("platform-cannot-prove-exact-wrapper-execution"))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let request_bytes = serde_json::to_vec(request)
            .map_err(|_| refusal("provider wrapper request cannot be serialized"))?;
        if request_bytes.len() > MAX_REQUEST_BYTES {
            return Err(refusal(
                "provider wrapper request exceeds the bounded input limit",
            ));
        }
        run_provider_wrapper_unix(config, environment, request, &request_bytes)
    }
}

pub(crate) fn validate_request(
    config: &ProviderWrapperConfig,
    request: &ProviderWrapperRequestV1,
) -> Result<(), ProviderWrapperRefusal> {
    if request.schema_version != PROVIDER_WRAPPER_SCHEMA_VERSION
        || request.provider_id != config.provider_id
        || request.adapter_id != config.adapter_id
    {
        return Err(refusal(
            "provider wrapper request identity or schema mismatch",
        ));
    }
    let fence = &request.delivery_fence;
    if fence.work_generation == 0
        || fence.owner_generation == 0
        || fence.attempt == 0
        || fence.consumer_epoch == 0
    {
        return Err(refusal(
            "delivery fence generations and attempt must be nonzero",
        ));
    }
    for value in [
        &fence.wake_id,
        &fence.work_item_id,
        &fence.route_ref,
        &fence.consumer_owner_ref,
    ] {
        validate_token(value)?;
    }
    validate_digest(&fence.payload_digest)?;
    validate_digest(&fence.idempotency_key)?;
    if fence.idempotency_key != fence.expected_idempotency_key() {
        return Err(refusal(
            "provider wrapper idempotency key does not bind the exact delivery fence",
        ));
    }
    let resume = &request.resume_expectation;
    for value in [
        &resume.workstream_handle,
        &resume.checkpoint_id,
        &resume.worktree_path,
    ] {
        validate_value(value)?;
    }
    validate_workstream_handle(&resume.workstream_handle)?;
    validate_repository(&resume.repository)?;
    let worktree_path = Path::new(&resume.worktree_path);
    if !worktree_path.is_absolute()
        || worktree_path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(refusal(
            "fresh-resume worktree path must be normalized and absolute",
        ));
    }
    if let Some(context_url) = &resume.context_url {
        validate_value(context_url)?;
        let authority = context_url
            .strip_prefix("https://")
            .and_then(|remainder| remainder.split('/').next());
        if authority.is_none_or(|value| value.is_empty() || value.contains('@'))
            || context_url.contains(['?', '#'])
        {
            return Err(refusal(
                "fresh-resume context URL must be secret-free canonical HTTPS",
            ));
        }
    }
    for digest in [
        &resume.plan_sha256,
        &resume.checkpoint_digest,
        &resume.expected_resume_context_digest,
        &resume.success_continuation_digest,
        &resume.failure_continuation_digest,
    ] {
        validate_digest(digest)?;
    }
    // Root, issue, and material-event revision zero are legitimate genesis
    // values. Projection and checkpoint generations are the freshness fences
    // and therefore must already exist before a fresh session can launch.
    if resume.projection_revision == 0 || resume.checkpoint_generation == 0 {
        return Err(refusal(
            "fresh-resume projection and checkpoint generations must be nonzero",
        ));
    }
    if resume.head_sha.len() != 40
        || !resume
            .head_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(refusal(
            "fresh-resume head must be an exact lowercase 40-hex commit",
        ));
    }
    validate_terminal_endpoint(&request.terminal_endpoint)?;
    if let ProviderDeliveryTargetV1::OriginalSession { surface_id } = &request.delivery_target {
        validate_token(surface_id)?;
    }
    validate_protected_route(request)?;
    validate_launch_options(&request.launch_options)?;
    Ok(())
}

fn validate_cmux_endpoint(endpoint: &CmuxEndpointV1) -> Result<(), ProviderWrapperRefusal> {
    for value in [&endpoint.executable_path, &endpoint.socket_path] {
        validate_value(value)?;
        let path = Path::new(value);
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(refusal(
                "cmux endpoint must contain normalized absolute paths",
            ));
        }
    }
    if endpoint.signing_team_id.len() != 10
        || !endpoint
            .signing_team_id
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        return Err(refusal("cmux signing team identity is invalid"));
    }
    Ok(())
}

fn validate_terminal_endpoint(endpoint: &TerminalEndpointV1) -> Result<(), ProviderWrapperRefusal> {
    match endpoint {
        TerminalEndpointV1::Cmux(endpoint) => validate_cmux_endpoint(endpoint),
        TerminalEndpointV1::HerdR {
            socket_path,
            server_incarnation,
            direct_fresh_launch_proven,
        } => {
            validate_value(socket_path)?;
            let path = Path::new(socket_path);
            if !path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
            {
                return Err(refusal("HerdR endpoint must be a normalized absolute path"));
            }
            let Some(incarnation) = server_incarnation else {
                return Err(refusal("HerdR server incarnation proof is required"));
            };
            validate_token(incarnation)?;
            if !direct_fresh_launch_proven {
                return Err(refusal("HerdR direct fresh-launch proof is required"));
            }
            Ok(())
        }
    }
}

fn validate_protected_route(
    request: &ProviderWrapperRequestV1,
) -> Result<(), ProviderWrapperRefusal> {
    const MAX_ROUTE_ITEMS: usize = 64;
    const MAX_ROUTE_BYTES: usize = 16 * 1024;
    const MAX_ROUTE_ENVIRONMENT: usize = 16;

    let route = &request.protected_route;
    if registered_provider_route_shape(&request.provider_id).is_none() {
        return Err(refusal("provider has no registered Subrouter route shape"));
    }
    validate_digest(&route.executable_sha256)?;
    validate_digest(&route.profile_digest)?;
    if route.profile_digest != request.delivery_fence.payload_digest
        || route.argv.len() < 3
        || route.argv.len() > MAX_ROUTE_ITEMS
        || route.fresh_argv.len() < 2
        || route.fresh_argv.len() > MAX_ROUTE_ITEMS
        || route.argv.iter().map(String::len).sum::<usize>() > MAX_ROUTE_BYTES
        || route.fresh_argv.iter().map(String::len).sum::<usize>() > MAX_ROUTE_BYTES
    {
        return Err(refusal(
            "protected provider route does not bind the exact profile",
        ));
    }
    for value in &route.argv {
        validate_value(value)?;
    }
    for value in &route.fresh_argv {
        validate_value(value)?;
    }
    let executable_path = Path::new(&route.argv[0]);
    let executable = executable_path.file_name().and_then(|value| value.to_str());
    if !executable_path.is_absolute()
        || executable_path.components().collect::<PathBuf>() != executable_path
        || executable != Some("subrouter")
        || route.argv[1] != request.provider_id
    {
        return Err(refusal(
            "protected provider route is not the exact Subrouter provider",
        ));
    }
    if route.fresh_argv[0] != route.argv[0] || route.fresh_argv[1] != request.provider_id {
        return Err(refusal(
            "protected fresh provider route is not the exact Subrouter provider",
        ));
    }
    if route.argv[2..]
        .iter()
        .any(|value| value.chars().any(char::is_whitespace))
    {
        return Err(refusal("protected provider route must remain prompt-free"));
    }
    validate_value(&route.native_session_id)?;
    if !exact_provider_route(
        &route.argv[2..],
        request.launch_options.model_id.as_deref(),
        request
            .launch_options
            .reasoning_effort
            .map(provider_reasoning_effort_name),
        Some(&route.native_session_id),
    ) {
        return Err(refusal(
            "protected provider route does not bind session or provider selection",
        ));
    }
    if !exact_provider_route(
        &route.fresh_argv[2..],
        request.launch_options.model_id.as_deref(),
        request
            .launch_options
            .reasoning_effort
            .map(provider_reasoning_effort_name),
        None,
    ) {
        return Err(refusal(
            "protected fresh provider route must use real no-session launch grammar",
        ));
    }
    validate_protected_route_environment(request, MAX_ROUTE_ENVIRONMENT)
}

pub(crate) fn exact_provider_route(
    tail: &[String],
    expected_model: Option<&str>,
    expected_reasoning: Option<&str>,
    expected_session: Option<&str>,
) -> bool {
    let mut models = Vec::new();
    let mut reasoning = Vec::new();
    let mut sessions = Vec::new();
    let mut resume_markers = 0;
    let mut index = 0;
    while index < tail.len() {
        let value = tail[index].as_str();
        match value {
            "resume" => resume_markers += 1,
            "--resume" => {
                resume_markers += 1;
                index += 1;
                let Some(session) = tail.get(index).map(String::as_str) else {
                    return false;
                };
                sessions.push(session);
            }
            "--model" => {
                index += 1;
                let Some(model) = tail.get(index).map(String::as_str) else {
                    return false;
                };
                models.push(model);
            }
            "--effort" | "--reasoning-effort" => {
                index += 1;
                let Some(effort) = tail.get(index).map(String::as_str) else {
                    return false;
                };
                reasoning.push(effort);
            }
            "-c" => {
                index += 1;
                let Some(setting) = tail.get(index).map(String::as_str) else {
                    return false;
                };
                let Some(effort) = setting.strip_prefix("model_reasoning_effort=") else {
                    return false;
                };
                reasoning.push(effort.trim_matches('"'));
            }
            _ => {
                if let Some(model) = value.strip_prefix("--model=") {
                    models.push(model);
                } else if let Some(effort) = value
                    .strip_prefix("--effort=")
                    .or_else(|| value.strip_prefix("--reasoning-effort="))
                {
                    reasoning.push(effort);
                } else if expected_session == Some(value) {
                    sessions.push(value);
                } else {
                    return false;
                }
            }
        }
        index += 1;
    }
    if models
        .iter()
        .chain(&reasoning)
        .any(|value| value.is_empty())
    {
        return false;
    }
    let session_matches = match expected_session {
        Some(expected) => sessions == [expected] && resume_markers == 1,
        None => sessions.is_empty() && resume_markers == 0,
    };
    session_matches
        && selections_match(&models, expected_model)
        && selections_match(&reasoning, expected_reasoning)
}

fn selections_match(observed: &[&str], expected: Option<&str>) -> bool {
    match expected {
        Some(expected) => observed == [expected],
        None => observed.is_empty(),
    }
}

fn validate_protected_route_environment(
    request: &ProviderWrapperRequestV1,
    max_environment: usize,
) -> Result<(), ProviderWrapperRefusal> {
    let route = &request.protected_route;
    if route.environment.len() > max_environment {
        return Err(refusal("protected provider route environment is too large"));
    }
    for (name, value) in &route.environment {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            || !name.starts_with("SUBROUTER_")
        {
            return Err(refusal(
                "protected provider route environment key is invalid",
            ));
        }
        validate_value(value)?;
    }
    let expected_account_key = subrouter_account_environment_key(&request.provider_id);
    let account_entries = route
        .environment
        .iter()
        .filter(|(name, _)| name.ends_with("_ACCOUNT_ID"))
        .collect::<Vec<_>>();
    match route.account_id.as_deref() {
        Some(account)
            if account_entries.len() == 1
                && account_entries[0].0 == &expected_account_key
                && account_entries[0].1 == account =>
        {
            validate_value(account)?;
        }
        None if account_entries.is_empty() => {}
        _ => {
            return Err(refusal(
                "protected provider route account selection is invalid",
            ));
        }
    }
    Ok(())
}

pub(crate) const fn provider_reasoning_effort_name(
    value: ProviderReasoningEffortV1,
) -> &'static str {
    match value {
        ProviderReasoningEffortV1::Low => "low",
        ProviderReasoningEffortV1::Medium => "medium",
        ProviderReasoningEffortV1::High => "high",
        ProviderReasoningEffortV1::Xhigh => "xhigh",
        ProviderReasoningEffortV1::Max => "max",
        ProviderReasoningEffortV1::Ultra => "ultra",
    }
}

pub(crate) fn subrouter_account_environment_key(provider_id: &str) -> String {
    format!(
        "SUBROUTER_{}_ACCOUNT_ID",
        provider_id
            .chars()
            .map(|character| if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            })
            .collect::<String>()
    )
}

fn validate_launch_options(
    options: &ProviderLaunchOptionsV1,
) -> Result<(), ProviderWrapperRefusal> {
    if options.model_id.as_ref().is_some_and(|model_id| {
        model_id.is_empty()
            || model_id.len() > 128
            || !model_id.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-' | b':')
            })
    }) {
        return Err(refusal(
            "provider model ID must be a bounded canonical token",
        ));
    }
    Ok(())
}

fn validate_workstream_handle(value: &str) -> Result<(), ProviderWrapperRefusal> {
    let Some((team, number)) = value.split_once('-') else {
        return Err(refusal("fresh-resume workstream handle is invalid"));
    };
    if team.is_empty()
        || team.len() > 16
        || !team.bytes().all(|byte| byte.is_ascii_uppercase())
        || number.is_empty()
        || number.starts_with('0')
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(refusal("fresh-resume workstream handle is invalid"));
    }
    Ok(())
}

fn validate_repository(value: &str) -> Result<(), ProviderWrapperRefusal> {
    let mut parts = value.split('/');
    let owner = parts.next().unwrap_or_default();
    let repository = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || !valid_repository_component(owner)
        || !valid_repository_component(repository)
    {
        return Err(refusal(
            "fresh-resume repository must be canonical lowercase owner/repository",
        ));
    }
    Ok(())
}

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn validate_token(value: &str) -> Result<(), ProviderWrapperRefusal> {
    if value.is_empty()
        || value.len() > 256
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/')
        })
    {
        return Err(refusal("delivery fence contains an invalid token"));
    }
    Ok(())
}

fn validate_value(value: &str) -> Result<(), ProviderWrapperRefusal> {
    if value.is_empty() || value.len() > MAX_VALUE_BYTES || value.chars().any(char::is_control) {
        return Err(refusal(
            "fresh-resume expectation contains an invalid value",
        ));
    }
    Ok(())
}

/// Provider references cross a durable authority boundary, so they are opaque
/// identifiers rather than arbitrary provider output. In particular, URLs,
/// credentials, bearer values, and query-like key/value strings are refused
/// before anything can persist them.
fn validate_provider_session_ref(
    value: &str,
    expected_provider: &str,
) -> Result<(), ProviderWrapperRefusal> {
    let Some((provider, opaque_id)) = value
        .strip_prefix("session:")
        .and_then(|remainder| remainder.split_once(':'))
    else {
        return Err(refusal(
            "provider session reference must use session:provider:opaque-id form",
        ));
    };
    if provider != expected_provider
        || opaque_id.is_empty()
        || value.len() > 256
        || value.contains("://")
        || opaque_id.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
        })
    {
        return Err(refusal(
            "provider session reference must be a bounded opaque identifier",
        ));
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<(), ProviderWrapperRefusal> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(refusal("provider wrapper digest must be lowercase SHA-256"));
    }
    Ok(())
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)] // Keep the ordered snapshot, execution, and cleanup proof chain adjacent.
fn run_provider_wrapper_unix(
    config: &ProviderWrapperConfig,
    environment: &ProviderWrapperEnvironment,
    request: &ProviderWrapperRequestV1,
    request_bytes: &[u8],
) -> Result<ProviderWrapperRunResult, ProviderWrapperRefusal> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(&config.executable_path)
        .map_err(|_| refusal("provider wrapper cannot be opened without following symlinks"))?;
    let metadata = source
        .metadata()
        .map_err(|_| refusal("provider wrapper metadata is unreadable"))?;
    if !metadata.file_type().is_file()
        || metadata.mode() & 0o111 == 0
        || metadata.len() == 0
        || metadata.len() > MAX_WRAPPER_BYTES
    {
        return Err(refusal(
            "provider wrapper must be a bounded regular executable",
        ));
    }

    // First snapshot the bytes read from the one no-follow source descriptor.
    // Platform preparation below executes these verified bytes without ever
    // reopening the configured path.
    let mut executable = tempfile::tempfile()
        .map_err(|_| refusal("provider wrapper executable snapshot cannot be created"))?;
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = vec![0u8; 32 * 1024].into_boxed_slice();
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|_| refusal("provider wrapper executable cannot be read"))?;
        if read == 0 {
            break;
        }
        copied = copied.saturating_add(read as u64);
        if copied > MAX_WRAPPER_BYTES {
            return Err(refusal(
                "provider wrapper executable exceeds its byte limit",
            ));
        }
        hasher.update(&buffer[..read]);
        executable
            .write_all(&buffer[..read])
            .map_err(|_| refusal("provider wrapper executable snapshot cannot be written"))?;
    }
    if copied != metadata.len() || hex::encode(hasher.finalize()) != config.executable_sha256 {
        return Err(refusal(
            "provider wrapper executable digest or length changed",
        ));
    }
    executable
        .set_permissions(std::fs::Permissions::from_mode(0o500))
        .and_then(|()| executable.sync_all())
        .and_then(|()| executable.seek(SeekFrom::Start(0)).map(drop))
        .map_err(|_| refusal("provider wrapper executable snapshot cannot be sealed"))?;
    let Some(prepared) = prepare_platform_executable(executable, &config.executable_sha256)? else {
        return Ok(uncertain("platform-cannot-prove-exact-wrapper-execution"));
    };

    #[cfg(target_os = "linux")]
    {
        run_provider_wrapper_linux_supervised(
            config,
            environment,
            request,
            request_bytes,
            &prepared,
        )
    }

    #[cfg(target_os = "macos")]
    {
        let mut stdin = tempfile::tempfile()
            .map_err(|_| refusal("provider wrapper input capture cannot be created"))?;
        stdin
            .write_all(request_bytes)
            .and_then(|()| stdin.seek(SeekFrom::Start(0)).map(drop))
            .map_err(|_| refusal("provider wrapper input cannot be prepared"))?;
        let mut stdout = tempfile::tempfile()
            .map_err(|_| refusal("provider wrapper stdout capture cannot be created"))?;
        let stderr = tempfile::tempfile()
            .map_err(|_| refusal("provider wrapper stderr capture cannot be created"))?;
        let execution_scope = tempfile::tempdir()
            .map_err(|_| refusal("provider wrapper execution scope cannot be created"))?;
        let sentinel_path = execution_scope.path().join("execution.sentinel");
        let sentinel = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&sentinel_path)
            .map_err(|_| refusal("provider wrapper execution sentinel cannot be created"))?;
        drop(sentinel);
        // Keep the parent descriptor table untouched. Clearing CLOEXEC in this
        // multi-threaded process allowed an unrelated concurrent child to inherit
        // another invocation's sentinel and be mistaken for its descendant. The
        // fixed system shell opens only this invocation's private sentinel in the
        // child immediately before replacing itself with the verified snapshot.
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "exec 9<>\"$1\" || exit 126\nshift\nexec \"$@\"",
            "shipyard-provider-sentinel",
        ]);
        command.arg(&sentinel_path).arg(&prepared.path);
        command.env_clear().envs(environment.0.iter());
        command.env(EXECUTION_SENTINEL_FD_ENV, "9");
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout.try_clone().map_err(|_| {
                refusal("provider wrapper stdout capture cannot be cloned")
            })?))
            .stderr(Stdio::from(stderr.try_clone().map_err(|_| {
                refusal("provider wrapper stderr capture cannot be cloned")
            })?));

        let deadline = Instant::now() + Duration::from_secs(config.deadline_seconds);
        let Ok(mut process) = ProcessTree::spawn(&mut command) else {
            return Ok(uncertain("verified-wrapper-launch-outcome-unknown"));
        };
        let mut status = None;
        let mut uncertain_reason = None;
        loop {
            if capture_exceeds(&stdout, config.max_stdout_bytes)
                || capture_exceeds(&stderr, config.max_stderr_bytes)
            {
                uncertain_reason = Some("provider-wrapper-output-limit");
                break;
            }
            match process.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    break;
                }
                Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
                Ok(None) => {
                    uncertain_reason = Some("provider-wrapper-timeout");
                    break;
                }
                Err(_) => {
                    uncertain_reason = Some("provider-wrapper-wait-outcome-unknown");
                    break;
                }
            }
        }
        let tree_cleanup_deadline = Instant::now() + TEARDOWN_BUDGET;
        process.terminate_until(tree_cleanup_deadline);
        let sentinel_cleanup = terminate_sentinel_processes(
            &sentinel_path,
            Instant::now() + SENTINEL_TEARDOWN_BUDGET,
            POLL_INTERVAL,
        );
        if capture_exceeds(&stdout, config.max_stdout_bytes)
            || capture_exceeds(&stderr, config.max_stderr_bytes)
        {
            return Ok(uncertain("provider-wrapper-output-limit"));
        }
        if !sentinel_cleanup.proven {
            return Ok(uncertain("provider-wrapper-cleanup-unproven"));
        }
        if sentinel_cleanup.residual_detected {
            return Ok(uncertain("provider-wrapper-descendant-violation"));
        }
        if let Some(reason) = uncertain_reason {
            return Ok(uncertain(reason));
        }
        let Some(status) = status else {
            return Ok(uncertain("provider-wrapper-exit-outcome-unknown"));
        };
        if !status.success() {
            return Ok(uncertain("provider-wrapper-nonzero-post-launch"));
        }
        let stdout_bytes = read_capture(&mut stdout, config.max_stdout_bytes)
            .ok_or_else(|| refusal("provider wrapper stdout capture is unreadable"))?;
        map_response(request, &stdout_bytes)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (config, environment, request, request_bytes, prepared);
        Ok(uncertain("platform-cannot-prove-exact-wrapper-execution"))
    }
}

#[cfg(all(target_os = "linux", not(test)))]
fn linux_supervisor_command() -> Command {
    // Bind the one-shot supervisor to the daemon's already-running image.
    // Unlike current_exe's release pathname, /proc/self/exe survives atomic
    // replacement and unlink during rolling deployment.
    let mut command = Command::new("/proc/self/exe");
    command.arg("provider-sentinel-supervisor");
    command
}

#[cfg(all(target_os = "linux", test))]
fn linux_supervisor_command() -> Command {
    let mut command = Command::new("/proc/self/exe");
    command.args([
        "--exact",
        "provider_wrapper::tests::linux_sentinel_supervisor_subprocess_helper",
        "--ignored",
        "--nocapture",
    ]);
    command
}

#[cfg(target_os = "linux")]
fn drain_linux_supervisor_channel(
    channel: &mut File,
    frames: &mut Vec<u8>,
    max_frames: usize,
) -> bool {
    let mut buffer = [0u8; 512];
    loop {
        match channel.read(&mut buffer) {
            Ok(0) => return true,
            Ok(count) => {
                if frames.len().saturating_add(count) > max_frames {
                    return false;
                }
                frames.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return true,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
}

#[cfg(target_os = "linux")]
fn parse_linux_supervisor_frames(bytes: &[u8]) -> Option<(bool, LinuxSupervisorResultV3)> {
    parse_linux_supervisor_frames_strict(linux_supervisor_protocol_slice(bytes))
}

#[cfg(target_os = "linux")]
fn parse_linux_supervisor_frames_strict(bytes: &[u8]) -> Option<(bool, LinuxSupervisorResultV3)> {
    let (startup, result_frame) = if let Some(rest) = bytes.strip_prefix(READY_FRAME) {
        (true, rest)
    } else {
        (false, bytes)
    };
    let json = result_frame
        .strip_prefix(RESULT_FRAME_PREFIX)?
        .strip_suffix(b"\n")?;
    let result = serde_json::from_slice(json).ok()?;
    Some((startup, result))
}

#[cfg(target_os = "linux")]
fn linux_supervisor_protocol_slice(bytes: &[u8]) -> &[u8] {
    #[cfg(test)]
    {
        // The libtest subprocess helper shares stdout with the harness. Real
        // production-binary integration tests exercise the strict no-noise
        // branch below; unit tests isolate only the uniquely framed protocol.
        let ready = bytes
            .windows(READY_FRAME.len())
            .position(|window| window == READY_FRAME);
        let result = bytes
            .windows(RESULT_FRAME_PREFIX.len())
            .position(|window| window == RESULT_FRAME_PREFIX);
        let start = match (ready, result) {
            (Some(ready), Some(result)) => ready.min(result),
            (Some(ready), None) => ready,
            (None, Some(result)) => result,
            (None, None) => return &[],
        };
        if let Some(result) = result {
            let result_body = result + RESULT_FRAME_PREFIX.len();
            if let Some(newline) = bytes[result_body..].iter().position(|byte| *byte == b'\n') {
                return &bytes[start..=result_body + newline];
            }
        }
        &bytes[start..]
    }
    #[cfg(not(test))]
    {
        bytes
    }
}

/// Hidden CLI entry for the Linux-only one-shot sentinel supervisor.
pub(crate) fn run_provider_sentinel_supervisor_command() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        execution_sentinel::run_linux_sentinel_supervisor()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("provider sentinel supervisor is supported only on Linux".into())
    }
}

#[cfg(unix)]
struct PreparedExecutable {
    #[cfg(not(target_os = "linux"))]
    path: std::path::PathBuf,
    file: File,
    _private_directory: Option<tempfile::TempDir>,
}

#[cfg(all(unix, target_os = "linux"))]
fn prepare_platform_executable(
    mut executable: File,
    expected_sha256: &str,
) -> Result<Option<PreparedExecutable>, ProviderWrapperRefusal> {
    use std::os::unix::fs::PermissionsExt;

    let descriptor = rustix::fs::memfd_create(
        "shipyard-provider-wrapper",
        rustix::fs::MemfdFlags::ALLOW_SEALING
            | rustix::fs::MemfdFlags::CLOEXEC
            | rustix::fs::MemfdFlags::EXEC,
    )
    .map_err(|_| refusal("platform cannot create a sealable wrapper snapshot"))?;
    let mut sealed = File::from(descriptor);
    executable
        .seek(SeekFrom::Start(0))
        .and_then(|_| std::io::copy(&mut executable, &mut sealed).map(drop))
        .and_then(|()| sealed.set_permissions(std::fs::Permissions::from_mode(0o500)))
        .and_then(|()| sealed.sync_all())
        .map_err(|_| refusal("platform cannot populate the sealed wrapper snapshot"))?;
    let required_seals = rustix::fs::SealFlags::WRITE
        | rustix::fs::SealFlags::GROW
        | rustix::fs::SealFlags::SHRINK
        | rustix::fs::SealFlags::SEAL;
    rustix::fs::fcntl_add_seals(&sealed, required_seals)
        .map_err(|_| refusal("platform cannot seal the verified wrapper snapshot"))?;
    let observed_seals = rustix::fs::fcntl_get_seals(&sealed)
        .map_err(|_| refusal("platform cannot verify wrapper snapshot seals"))?;
    if !observed_seals.contains(required_seals) {
        return Err(refusal("verified wrapper snapshot is not fully sealed"));
    }
    sealed
        .seek(SeekFrom::Start(0))
        .map_err(|_| refusal("sealed wrapper snapshot cannot be rewound"))?;
    let mut sealed_hasher = Sha256::new();
    std::io::copy(&mut sealed, &mut HashWriter(&mut sealed_hasher))
        .map_err(|_| refusal("sealed wrapper snapshot cannot be rehashed"))?;
    if hex::encode(sealed_hasher.finalize()) != expected_sha256 {
        return Err(refusal("sealed wrapper snapshot digest changed"));
    }

    Ok(Some(PreparedExecutable {
        file: sealed,
        _private_directory: None,
    }))
}

#[cfg(all(unix, target_os = "macos"))]
fn prepare_platform_executable(
    mut executable: File,
    expected_sha256: &str,
) -> Result<Option<PreparedExecutable>, ProviderWrapperRefusal> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    // Darwin has no fexecve. A mode-0700 randomized directory containing a
    // create-new snapshot is its strongest non-unsafe execution boundary. The
    // configured path is never reopened; the private snapshot is opened
    // no-follow and rehashed before the kernel sees its path.
    let private_directory = tempfile::tempdir()
        .map_err(|_| refusal("private wrapper execution directory cannot be created"))?;
    std::fs::set_permissions(
        private_directory.path(),
        std::fs::Permissions::from_mode(0o700),
    )
    .map_err(|_| refusal("private wrapper execution directory cannot be protected"))?;
    let path = private_directory.path().join("wrapper");
    let mut named = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o500)
        .open(&path)
        .map_err(|_| refusal("private wrapper snapshot cannot be created"))?;
    executable
        .seek(SeekFrom::Start(0))
        .and_then(|_| std::io::copy(&mut executable, &mut named).map(drop))
        .and_then(|()| named.sync_all())
        .map_err(|_| refusal("private wrapper snapshot cannot be sealed"))?;
    drop(named);
    let mut verified = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(&path)
        .map_err(|_| refusal("private wrapper snapshot cannot be reopened no-follow"))?;
    let metadata = verified
        .metadata()
        .map_err(|_| refusal("private wrapper snapshot metadata is unreadable"))?;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(refusal(
            "private wrapper snapshot is not a regular executable",
        ));
    }
    let mut hasher = Sha256::new();
    std::io::copy(&mut verified, &mut HashWriter(&mut hasher))
        .map_err(|_| refusal("private wrapper snapshot cannot be rehashed"))?;
    if hex::encode(hasher.finalize()) != expected_sha256 {
        return Err(refusal("private wrapper snapshot digest changed"));
    }
    Ok(Some(PreparedExecutable {
        path,
        file: verified,
        _private_directory: Some(private_directory),
    }))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn prepare_platform_executable(
    _executable: File,
    _expected_sha256: &str,
) -> Result<Option<PreparedExecutable>, ProviderWrapperRefusal> {
    Ok(None)
}

#[cfg(unix)]
struct HashWriter<'a>(&'a mut Sha256);

#[cfg(unix)]
impl Write for HashWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
fn capture_exceeds(file: &File, limit: u64) -> bool {
    file.metadata()
        .map_or(true, |metadata| metadata.len() > limit)
}

#[cfg(unix)]
fn read_capture(file: &mut File, limit: u64) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

fn map_response(
    request: &ProviderWrapperRequestV1,
    bytes: &[u8],
) -> Result<ProviderWrapperRunResult, ProviderWrapperRefusal> {
    let response: ProviderWrapperResponseV1 = match serde_json::from_slice(bytes) {
        Ok(response) => response,
        Err(_) => return Ok(uncertain("provider-wrapper-malformed-response")),
    };
    if response.schema_version != PROVIDER_WRAPPER_SCHEMA_VERSION
        || response.operation != request.operation
        || response.provider_id != request.provider_id
        || response.adapter_id != request.adapter_id
        || response.idempotency_key != request.delivery_fence.idempotency_key
    {
        return Ok(uncertain("provider-wrapper-response-fence-mismatch"));
    }
    let canonical_bytes = serde_json::to_vec(&response)
        .map_err(|_| refusal("strict provider response cannot be canonicalized"))?;
    let response_receipt = ProtectedProviderResponseV1 {
        response_digest: hex::encode(Sha256::digest(&canonical_bytes)),
        canonical_bytes,
    };
    let result = match response.outcome {
        ProviderWrapperOutcomeV1::Delivered {
            acceptance: ProviderAcceptanceV1::ProviderSessionAccepted,
            provider_session_ref,
            receipt_digest,
        } if validate_provider_session_ref(&provider_session_ref, &request.provider_id).is_ok()
            && validate_digest(&receipt_digest).is_ok() =>
        {
            ProviderWrapperRunResult::Delivered {
                provider_session_ref,
                provider_receipt_digest: receipt_digest,
                response_receipt,
            }
        }
        ProviderWrapperOutcomeV1::Retryable {
            launch_state: NotAcceptedV1::NotAccepted,
            error_digest,
        } if validate_digest(&error_digest).is_ok() => ProviderWrapperRunResult::Retryable {
            error_digest,
            response_receipt,
        },
        ProviderWrapperOutcomeV1::Uncertain {
            launch_state: UnknownV1::Unknown,
            evidence_digest,
        } if validate_digest(&evidence_digest).is_ok() => ProviderWrapperRunResult::Uncertain {
            evidence_digest,
            response_receipt: Some(response_receipt),
        },
        ProviderWrapperOutcomeV1::Rejected {
            launch_state: NotAcceptedV1::NotAccepted,
            error_digest,
        } if validate_digest(&error_digest).is_ok() => ProviderWrapperRunResult::Rejected {
            error_digest,
            response_receipt,
        },
        _ => uncertain("provider-wrapper-invalid-outcome"),
    };
    Ok(result)
}

fn uncertain(reason: &str) -> ProviderWrapperRunResult {
    ProviderWrapperRunResult::Uncertain {
        evidence_digest: hex::encode(Sha256::digest(reason.as_bytes())),
        response_receipt: None,
    }
}

fn refusal(message: impl Into<String>) -> ProviderWrapperRefusal {
    ProviderWrapperRefusal(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn digest(value: &str) -> String {
        hex::encode(Sha256::digest(value.as_bytes()))
    }

    fn native_absolute_test_path(leaf: &str) -> String {
        if cfg!(windows) {
            format!(r"C:\Shipyard\{leaf}")
        } else {
            format!("/tmp/shipyard/{leaf}")
        }
    }

    fn request(operation: ProviderWrapperOperationV1) -> ProviderWrapperRequestV1 {
        let mut fence = ProviderDeliveryFenceV1 {
            wake_id: "wake-1".into(),
            work_item_id: "item-1".into(),
            work_generation: 7,
            owner_generation: 3,
            route_ref: "route-1".into(),
            payload_digest: digest("payload"),
            attempt: 2,
            consumer_epoch: 9,
            consumer_owner_ref: "owner-1".into(),
            idempotency_key: String::new(),
        };
        fence.bind_idempotency_key();
        ProviderWrapperRequestV1 {
            schema_version: PROVIDER_WRAPPER_SCHEMA_VERSION,
            operation,
            delivery_target: ProviderDeliveryTargetV1::FreshCheckpoint,
            provider_id: "codex".into(),
            adapter_id: "codex-wrapper-v1".into(),
            terminal_endpoint: TerminalEndpointV1::Cmux(CmuxEndpointV1 {
                executable_path: native_absolute_test_path("cmux"),
                socket_path: native_absolute_test_path("cmux.sock"),
                signing_team_id: "7WLXT3NR37".into(),
            }),
            protected_route: ProtectedProviderRouteV1 {
                argv: vec![
                    native_absolute_test_path("subrouter"),
                    "codex".into(),
                    "resume".into(),
                    "--model".into(),
                    "gpt-5.6-sol".into(),
                    "-c".into(),
                    "model_reasoning_effort=\"medium\"".into(),
                    "session-1".into(),
                ],
                fresh_argv: vec![
                    native_absolute_test_path("subrouter"),
                    "codex".into(),
                    "--model".into(),
                    "gpt-5.6-sol".into(),
                    "-c".into(),
                    "model_reasoning_effort=\"medium\"".into(),
                ],
                executable_sha256: "9".repeat(64),
                environment: BTreeMap::new(),
                account_id: None,
                native_session_id: "session-1".into(),
                profile_digest: fence.payload_digest.clone(),
            },
            delivery_fence: fence,
            resume_expectation: FreshResumeExpectationV1 {
                workstream_handle: "GEN-43".into(),
                context_url: Some("https://linear.app/example/issue/GEN-43".into()),
                plan_sha256: digest("plan"),
                root_revision: 5,
                issue_revision: 7,
                material_event_revision: 11,
                projection_revision: 17,
                checkpoint_id: "checkpoint-1".into(),
                checkpoint_generation: 4,
                checkpoint_digest: digest("checkpoint"),
                repository: "generous-corp/shipyard".into(),
                worktree_path: native_absolute_test_path("worktree"),
                head_sha: "a".repeat(40),
                expected_resume_context_digest: digest("resume"),
                success_continuation_digest: digest("success"),
                failure_continuation_digest: digest("failure"),
            },
            launch_options: ProviderLaunchOptionsV1 {
                model_id: Some("gpt-5.6-sol".into()),
                reasoning_effort: Some(ProviderReasoningEffortV1::Medium),
            },
        }
    }

    #[test]
    fn request_and_response_reject_unknown_fields() {
        let mut value = serde_json::to_value(request(ProviderWrapperOperationV1::Submit)).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("token".into(), "secret".into());
        assert!(serde_json::from_value::<ProviderWrapperRequestV1>(value).is_err());

        let response = serde_json::json!({
            "schema_version": 1,
            "operation": "submit",
            "provider_id": "codex",
            "adapter_id": "codex-wrapper-v1",
            "idempotency_key": digest("key"),
            "outcome": {"status": "rejected", "launch_state": "not_accepted", "error_digest": digest("error"), "extra": true}
        });
        assert!(serde_json::from_value::<ProviderWrapperResponseV1>(response).is_err());
    }

    #[test]
    fn reconcile_preserves_idempotency_key_and_operation() {
        let submit = request(ProviderWrapperOperationV1::Submit);
        let reconcile = request(ProviderWrapperOperationV1::Reconcile);
        assert_eq!(
            submit.delivery_fence.idempotency_key,
            reconcile.delivery_fence.idempotency_key
        );
        assert_ne!(submit.operation, reconcile.operation);
        let bytes = serde_json::to_vec(&reconcile).unwrap();
        assert_eq!(
            serde_json::from_slice::<ProviderWrapperRequestV1>(&bytes)
                .unwrap()
                .operation,
            ProviderWrapperOperationV1::Reconcile
        );
    }

    #[test]
    fn changed_delivery_fence_refuses_before_launch() {
        let mut request = request(ProviderWrapperOperationV1::Submit);
        request.delivery_fence.work_generation += 1;
        let config = config(Path::new("/does/not/matter"), digest("unused"));
        assert!(
            run_provider_wrapper(&config, &ProviderWrapperEnvironment::default(), &request)
                .unwrap_err()
                .to_string()
                .contains("idempotency key")
        );
    }

    #[test]
    fn resume_identity_refuses_normalization_variants_before_launch() {
        let config = config(Path::new("/does/not/matter"), digest("unused"));
        for mutate in [
            |request: &mut ProviderWrapperRequestV1| {
                request.resume_expectation.head_sha = "A".repeat(40);
            },
            |request: &mut ProviderWrapperRequestV1| {
                request.resume_expectation.repository = "Generous-Corp/Shipyard".into();
            },
            |request: &mut ProviderWrapperRequestV1| {
                request.resume_expectation.worktree_path = "../worktree".into();
            },
            |request: &mut ProviderWrapperRequestV1| {
                request.resume_expectation.context_url =
                    Some("https://linear.app/issue/GEN-43?token=secret".into());
            },
        ] {
            let mut request = request(ProviderWrapperOperationV1::Submit);
            mutate(&mut request);
            assert!(
                run_provider_wrapper(&config, &ProviderWrapperEnvironment::default(), &request)
                    .is_err()
            );
        }
    }

    #[test]
    fn provider_owned_launch_contract_has_no_argv_or_secret_channel() {
        let mut value = serde_json::to_value(request(ProviderWrapperOperationV1::Submit)).unwrap();
        value.as_object_mut().unwrap().insert(
            "launch_argv".into(),
            serde_json::json!(["codex", "--password", "secret"]),
        );
        assert!(serde_json::from_value::<ProviderWrapperRequestV1>(value).is_err());

        let mut invalid_model = request(ProviderWrapperOperationV1::Submit);
        invalid_model.launch_options.model_id = Some("gpt-5.6-sol --password=secret".into());
        let config = config(Path::new("/does/not/matter"), digest("unused"));
        assert!(validate_request(&config, &invalid_model).is_err());
    }

    #[test]
    fn provider_session_reference_is_opaque_and_never_credential_bearing() {
        for value in [
            "https://provider.example/session?token=secret",
            "session=secret",
            "user@example.com",
            "Bearer secret",
            "session/child",
        ] {
            assert!(
                validate_provider_session_ref(value, "codex").is_err(),
                "accepted {value:?}"
            );
        }
        assert!(validate_provider_session_ref("session:codex:01HZX_abc-9", "codex").is_ok());
        assert!(validate_provider_session_ref("session:claude:01HZX_abc-9", "codex").is_err());
    }

    #[test]
    fn genesis_authority_revisions_are_valid_but_freshness_fences_are_not() {
        let config = config(Path::new("/does/not/matter"), digest("unused"));
        let mut genesis = request(ProviderWrapperOperationV1::Submit);
        genesis.resume_expectation.root_revision = 0;
        genesis.resume_expectation.issue_revision = 0;
        genesis.resume_expectation.material_event_revision = 0;
        assert!(validate_request(&config, &genesis).is_ok());

        genesis.resume_expectation.projection_revision = 0;
        assert!(validate_request(&config, &genesis).is_err());
    }

    #[test]
    fn recovered_consumer_uses_same_provider_idempotency_key() {
        let first = request(ProviderWrapperOperationV1::Submit);
        let mut recovered = request(ProviderWrapperOperationV1::Reconcile);
        recovered.delivery_fence.consumer_epoch += 1;
        recovered.delivery_fence.consumer_owner_ref = "successor-owner".into();
        assert_eq!(
            first.delivery_fence.idempotency_key,
            recovered.delivery_fence.expected_idempotency_key()
        );
    }

    #[test]
    fn response_mapping_never_conflates_acceptance_with_resume_ack() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let response = ProviderWrapperResponseV1 {
            schema_version: PROVIDER_WRAPPER_SCHEMA_VERSION,
            operation: request.operation,
            provider_id: request.provider_id.clone(),
            adapter_id: request.adapter_id.clone(),
            idempotency_key: request.delivery_fence.idempotency_key.clone(),
            outcome: ProviderWrapperOutcomeV1::Delivered {
                acceptance: ProviderAcceptanceV1::ProviderSessionAccepted,
                provider_session_ref: "session:codex:session-1".into(),
                receipt_digest: digest("receipt"),
            },
        };
        let result = map_response(&request, &serde_json::to_vec(&response).unwrap()).unwrap();
        assert!(matches!(
            result,
            ProviderWrapperRunResult::Delivered {
                provider_session_ref,
                provider_receipt_digest,
                response_receipt,
            } if provider_session_ref == "session:codex:session-1"
                && provider_receipt_digest == digest("receipt")
                && response_receipt.response_digest == hex::encode(Sha256::digest(&response_receipt.canonical_bytes))
        ));
    }

    #[test]
    fn protected_receipt_uses_canonical_response_not_provider_raw_bytes() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let response = ProviderWrapperResponseV1 {
            schema_version: PROVIDER_WRAPPER_SCHEMA_VERSION,
            operation: request.operation,
            provider_id: request.provider_id.clone(),
            adapter_id: request.adapter_id.clone(),
            idempotency_key: request.delivery_fence.idempotency_key.clone(),
            outcome: ProviderWrapperOutcomeV1::Delivered {
                acceptance: ProviderAcceptanceV1::ProviderSessionAccepted,
                provider_session_ref: "session:codex:session-1".into(),
                receipt_digest: digest("receipt"),
            },
        };
        let canonical = serde_json::to_vec(&response).unwrap();
        let mut raw = b" \n".to_vec();
        raw.extend_from_slice(&canonical);
        raw.extend_from_slice(b" \n");
        let result = map_response(&request, &raw).unwrap();
        let ProviderWrapperRunResult::Delivered {
            response_receipt, ..
        } = result
        else {
            panic!("strict response should map to delivered");
        };
        assert_eq!(response_receipt.canonical_bytes, canonical);
        assert_ne!(response_receipt.canonical_bytes, raw);
        assert_eq!(
            response_receipt.response_digest,
            hex::encode(Sha256::digest(&response_receipt.canonical_bytes))
        );
    }

    fn config(path: &Path, executable_sha256: String) -> ProviderWrapperConfig {
        ProviderWrapperConfig {
            executable_path: path.to_path_buf(),
            executable_sha256,
            provider_id: "codex".into(),
            adapter_id: "codex-wrapper-v1".into(),
            // Success fixtures may share a macOS host with expensive Pulp
            // compilation. Timeout-specific tests replace this value with a
            // shorter bound and retain a deliberately slower child.
            deadline_seconds: 15,
            max_stdout_bytes: 4096,
            max_stderr_bytes: 4096,
        }
    }

    #[cfg(unix)]
    fn wrapper_c(source_body: &str) -> (tempfile::TempDir, std::path::PathBuf, String) {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("wrapper.c");
        let path = directory.path().join("wrapper");
        let contents = format!(
            "#include <dirent.h>\n#include <errno.h>\n#include <signal.h>\n#include <stdio.h>\n#include <stdlib.h>\n#include <string.h>\n#include <unistd.h>\n#include <sys/wait.h>\n#ifdef __linux__\n#include <sys/prctl.h>\n#endif\nint main(void) {{ {source_body} }}\n"
        );
        fs::write(&source, contents).unwrap();
        assert!(
            Command::new("cc")
                .args(["-o"])
                .arg(&path)
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let bytes = fs::read(&path).unwrap();
        let sha = hex::encode(Sha256::digest(bytes));
        (directory, path, sha)
    }

    #[cfg(unix)]
    fn assert_pid_eventually_not_running(pid: &str, failure_message: &str) {
        let pid = pid
            .trim()
            .parse::<u32>()
            .expect("fixture wrote a valid PID");
        let pid = pid.to_string();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let output = Command::new("/bin/ps")
                .args(["-o", "state=", "-p", &pid])
                .output()
                .unwrap();
            let state = String::from_utf8(output.stdout).unwrap();
            let state = state.trim();
            if !output.status.success() && state.is_empty() && output.stderr.is_empty() {
                return;
            }
            assert!(output.status.success(), "malformed process-state probe");
            let Some(state) = state.chars().next() else {
                panic!("process-state probe returned no state");
            };
            if state == 'Z' {
                return;
            }
            assert!(Instant::now() < deadline, "{failure_message}");
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    #[cfg(unix)]
    fn response_program(request: &ProviderWrapperRequestV1, status: &str) -> String {
        let outcome = match status {
            "delivered" => ProviderWrapperOutcomeV1::Delivered {
                acceptance: ProviderAcceptanceV1::ProviderSessionAccepted,
                provider_session_ref: "session:codex:session-1".into(),
                receipt_digest: digest("receipt"),
            },
            "retryable" => ProviderWrapperOutcomeV1::Retryable {
                launch_state: NotAcceptedV1::NotAccepted,
                error_digest: digest("retry"),
            },
            _ => unreachable!(),
        };
        let response = ProviderWrapperResponseV1 {
            schema_version: PROVIDER_WRAPPER_SCHEMA_VERSION,
            operation: request.operation,
            provider_id: request.provider_id.clone(),
            adapter_id: request.adapter_id.clone(),
            idempotency_key: request.delivery_fence.idempotency_key.clone(),
            outcome,
        };
        let bytes = serde_json::to_vec(&response)
            .unwrap()
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let operation = match request.operation {
            ProviderWrapperOperationV1::Submit => "submit",
            ProviderWrapperOperationV1::Reconcile => "reconcile",
        };
        format!(
            "char input[65537] = {{0}}; size_t count = 0; while (count < sizeof(input) - 1) {{ size_t room = sizeof(input) - 1 - count; size_t chunk = room < 7 ? room : 7; ssize_t got = read(STDIN_FILENO, input + count, chunk); if (got > 0) {{ count += (size_t)got; continue; }} if (got == 0) break; if (errno == EINTR) continue; return 90; }} if (count == 0 || strstr(input, \"\\\"operation\\\":\\\"{operation}\\\"\") == NULL || getenv(\"GITHUB_TOKEN\") != NULL) return 91; unsigned char output[] = {{{bytes}}}; size_t written = 0; while (written < sizeof(output)) {{ size_t remaining = sizeof(output) - written; size_t chunk = remaining < 7 ? remaining : 7; ssize_t sent = write(STDOUT_FILENO, output + written, chunk); if (sent > 0) {{ written += (size_t)sent; continue; }} if (sent < 0 && errno == EINTR) continue; return 92; }} return 0;"
        )
    }

    #[cfg(unix)]
    #[test]
    fn verified_snapshot_executes_and_maps_strict_response() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let (_directory, path, sha) = wrapper_c(&response_program(&request, "delivered"));
        let result = run_provider_wrapper(
            &config(&path, sha),
            &ProviderWrapperEnvironment::default(),
            &request,
        )
        .unwrap();
        assert!(
            matches!(result, ProviderWrapperRunResult::Delivered { .. }),
            "unexpected result: {result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_invokes_reconcile_once_and_does_not_submit() {
        let request = request(ProviderWrapperOperationV1::Reconcile);
        let (_directory, path, sha) = wrapper_c(&response_program(&request, "retryable"));
        let result = run_provider_wrapper(
            &config(&path, sha),
            &ProviderWrapperEnvironment::default(),
            &request,
        )
        .unwrap();
        assert!(matches!(
            result,
            ProviderWrapperRunResult::Retryable { error_digest, response_receipt }
                if error_digest == digest("retry") && !response_receipt.canonical_bytes.is_empty()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn lost_submit_is_followed_by_same_key_reconcile_without_second_submit() {
        let submit = request(ProviderWrapperOperationV1::Submit);
        let reconcile = request(ProviderWrapperOperationV1::Reconcile);
        let directory = tempfile::tempdir().unwrap();
        let submit_count = directory.path().join("submit.count");
        let reconcile_count = directory.path().join("reconcile.count");
        let response = ProviderWrapperResponseV1 {
            schema_version: PROVIDER_WRAPPER_SCHEMA_VERSION,
            operation: ProviderWrapperOperationV1::Reconcile,
            provider_id: reconcile.provider_id.clone(),
            adapter_id: reconcile.adapter_id.clone(),
            idempotency_key: reconcile.delivery_fence.idempotency_key.clone(),
            outcome: ProviderWrapperOutcomeV1::Retryable {
                launch_state: NotAcceptedV1::NotAccepted,
                error_digest: digest("retry"),
            },
        };
        let response_bytes = serde_json::to_vec(&response)
            .unwrap()
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let body = format!(
            "char input[65537] = {{0}}; fread(input, 1, sizeof(input) - 1, stdin); \
             if (strstr(input, \"\\\"operation\\\":\\\"submit\\\"\") != NULL) {{ \
                 FILE *f = fopen(\"{}\", \"a\"); fputc('1', f); fclose(f); \
                 fputs(\"lost-response\", stdout); return 0; \
             }} \
             FILE *s = fopen(\"{}\", \"r\"); if (s == NULL || fgetc(s) != '1' || fgetc(s) != EOF) return 92; fclose(s); \
             FILE *r = fopen(\"{}\", \"a\"); fputc('1', r); fclose(r); \
             unsigned char output[] = {{{}}}; return fwrite(output, 1, sizeof(output), stdout) == sizeof(output) ? 0 : 1;",
            submit_count.display(),
            submit_count.display(),
            reconcile_count.display(),
            response_bytes,
        );
        let (_wrapper_dir, path, sha) = wrapper_c(&body);
        let mut config = config(&path, sha);
        // This two-invocation fixture runs inside the fully parallel library
        // suite, where process startup can exceed the tiny default test budget.
        // Keep its private wrapper/state while allowing both exact invocations
        // the same production-scale launch budget.
        config.deadline_seconds = 15;
        assert!(matches!(
            run_provider_wrapper(&config, &ProviderWrapperEnvironment::default(), &submit).unwrap(),
            ProviderWrapperRunResult::Uncertain { .. }
        ));
        let reconcile_result =
            run_provider_wrapper(&config, &ProviderWrapperEnvironment::default(), &reconcile)
                .unwrap();
        let cleanup_unproven = matches!(
            &reconcile_result,
            ProviderWrapperRunResult::Uncertain {
                evidence_digest,
                response_receipt: None,
            } if evidence_digest == &digest("provider-wrapper-cleanup-unproven")
        );
        assert!(
            matches!(
                &reconcile_result,
                ProviderWrapperRunResult::Retryable { .. }
            ) || (cfg!(target_os = "macos") && cleanup_unproven),
            "reconcile must map its response or preserve macOS cleanup uncertainty: {reconcile_result:?}"
        );
        assert_eq!(fs::read(&submit_count).unwrap(), b"1");
        assert_eq!(fs::read(&reconcile_count).unwrap(), b"1");
        assert_eq!(
            submit.delivery_fence.idempotency_key,
            reconcile.delivery_fence.idempotency_key
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_digest_mismatch_and_non_executable_refuse() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let (directory, path, sha) = wrapper_c("return 0;");
        let link = directory.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(
            run_provider_wrapper(
                &config(&link, sha.clone()),
                &ProviderWrapperEnvironment::default(),
                &request
            )
            .is_err()
        );
        assert!(
            run_provider_wrapper(
                &config(&path, digest("wrong")),
                &ProviderWrapperEnvironment::default(),
                &request
            )
            .is_err()
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            run_provider_wrapper(
                &config(&path, sha),
                &ProviderWrapperEnvironment::default(),
                &request
            )
            .is_err()
        );

        let oversized = directory.path().join("oversized");
        let oversized_file = File::create(&oversized).unwrap();
        oversized_file.set_len(MAX_WRAPPER_BYTES + 1).unwrap();
        fs::set_permissions(&oversized, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            run_provider_wrapper(
                &config(&oversized, digest("irrelevant")),
                &ProviderWrapperEnvironment::default(),
                &request
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_over_limit_are_uncertain_and_descendants_are_killed() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let (directory, path, sha) = wrapper_c(
            "pid_t child = fork(); if (child == 0) { sleep(30); return 0; } const char *home = getenv(\"HOME\"); char path[4096]; snprintf(path, sizeof(path), \"%s/child.pid\", home); FILE *file = fopen(path, \"w\"); fprintf(file, \"%d\", child); fclose(file); waitpid(child, 0, 0); return 0;",
        );
        let mut timeout_config = config(&path, sha);
        // The full macOS suite runs thousands of process fixtures in parallel.
        // Keep this below the child's 30-second sleep while allowing the
        // wrapper enough scheduler time to publish its descendant receipt.
        timeout_config.deadline_seconds = 15;
        let environment = ProviderWrapperEnvironment::new([(
            "HOME".into(),
            directory.path().as_os_str().to_owned(),
        )])
        .unwrap();
        assert!(matches!(
            run_provider_wrapper(&timeout_config, &environment, &request).unwrap(),
            ProviderWrapperRunResult::Uncertain { .. }
        ));
        let child_pid = fs::read_to_string(directory.path().join("child.pid")).unwrap();
        assert_pid_eventually_not_running(&child_pid, "descendant survived timeout");

        let (_directory, path, sha) = wrapper_c(
            "char block[4096] = {0}; for (;;) { if (write(1, block, sizeof(block)) < 0) return 1; }",
        );
        let mut limit_config = config(&path, sha);
        limit_config.max_stdout_bytes = 32;
        let started = Instant::now();
        assert!(matches!(
            run_provider_wrapper(
                &limit_config,
                &ProviderWrapperEnvironment::default(),
                &request,
            )
            .unwrap(),
            ProviderWrapperRunResult::Uncertain { .. }
        ));
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "output-limit cleanup plus serialized fixture admission wedged"
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_setsid_descendant_without_pipe_reader_threads() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let (directory, path, sha) = wrapper_c(
            "pid_t child = fork(); if (child == 0) { setsid(); const char *home = getenv(\"HOME\"); char path[4096]; snprintf(path, sizeof(path), \"%s/detached.pid\", home); FILE *file = fopen(path, \"w\"); fprintf(file, \"%d\", getpid()); fclose(file); sleep(30); return 0; } waitpid(child, 0, 0); return 0;",
        );
        let mut timeout_config = config(&path, sha);
        timeout_config.deadline_seconds = 15;
        let environment = ProviderWrapperEnvironment::new([(
            "HOME".into(),
            directory.path().as_os_str().to_owned(),
        )])
        .unwrap();
        let started = Instant::now();
        assert!(matches!(
            run_provider_wrapper(&timeout_config, &environment, &request).unwrap(),
            ProviderWrapperRunResult::Uncertain { .. }
        ));
        assert!(
            started.elapsed() < Duration::from_secs(45),
            "timeout cleanup plus serialized fixture admission wedged"
        );
        let child_pid = fs::read_to_string(directory.path().join("detached.pid")).unwrap();
        assert_pid_eventually_not_running(&child_pid, "setsid descendant survived timeout");
    }

    #[cfg(unix)]
    #[test]
    fn successful_parent_exit_with_setsid_child_is_refused_and_terminated() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let directory = tempfile::tempdir().unwrap();
        let detached_pid = directory.path().join("detached-success.pid");
        let detached_pid_staging = directory.path().join("detached-success.pid.tmp");
        let body = format!(
            "pid_t child = fork(); if (child == 0) {{ setsid(); FILE *file = fopen(\"{}\", \"w\"); fprintf(file, \"%d\", getpid()); fclose(file); rename(\"{}\", \"{}\"); sleep(30); return 0; }} \
             for (int i = 0; i < 5000 && access(\"{}\", F_OK) != 0; ++i) usleep(1000); {}",
            detached_pid_staging.display(),
            detached_pid_staging.display(),
            detached_pid.display(),
            detached_pid.display(),
            response_program(&request, "delivered"),
        );
        let (_wrapper_dir, path, sha) = wrapper_c(&body);
        let mut wrapper_config = config(&path, sha);
        wrapper_config.deadline_seconds = 10;
        let result = run_provider_wrapper(
            &wrapper_config,
            &ProviderWrapperEnvironment::default(),
            &request,
        )
        .unwrap();
        assert!(matches!(result, ProviderWrapperRunResult::Uncertain { .. }));
        let child_pid = fs::read_to_string(&detached_pid).unwrap();
        assert_pid_eventually_not_running(
            &child_pid,
            "setsid child survived successful wrapper-parent exit",
        );
    }

    #[cfg(unix)]
    #[test]
    fn successful_parent_exit_with_stopped_setsid_child_leaves_no_orphan() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let directory = tempfile::tempdir().unwrap();
        let detached_pid = directory.path().join("detached-stopped.pid");
        let detached_pid_staging = directory.path().join("detached-stopped.pid.tmp");
        let body = format!(
            "pid_t child = fork(); if (child == 0) {{ setsid(); FILE *file = fopen(\"{}\", \"w\"); fprintf(file, \"%d\", getpid()); fclose(file); rename(\"{}\", \"{}\"); raise(SIGSTOP); return 0; }} \
             for (int i = 0; i < 5000 && access(\"{}\", F_OK) != 0; ++i) usleep(1000); {}",
            detached_pid_staging.display(),
            detached_pid_staging.display(),
            detached_pid.display(),
            detached_pid.display(),
            response_program(&request, "delivered"),
        );
        let (_wrapper_dir, path, sha) = wrapper_c(&body);
        let result = run_provider_wrapper(
            &config(&path, sha),
            &ProviderWrapperEnvironment::default(),
            &request,
        )
        .unwrap();
        assert!(
            matches!(
                &result,
                ProviderWrapperRunResult::Uncertain {
                    evidence_digest,
                    response_receipt: None,
                } if *evidence_digest == digest("provider-wrapper-descendant-violation")
            ),
            "unexpected Linux subreaper result: {result:?}"
        );
        let child_pid = fs::read_to_string(&detached_pid).unwrap();
        assert_pid_eventually_not_running(
            &child_pid,
            "stopped setsid child survived exact sentinel cleanup",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_supervisor_uses_the_running_image_across_release_replacement() {
        let command = linux_supervisor_command();
        assert_eq!(command.get_program(), "/proc/self/exe");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_running_image_survives_installed_path_replacement_and_unlink() {
        use wait_timeout::ChildExt;

        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("shipyard-installed");
        let retired = directory.path().join("shipyard-running-retired");
        let ready = directory.path().join("ready");
        let proceed = directory.path().join("proceed");
        let outcome = directory.path().join("outcome");
        fs::copy(std::env::current_exe().unwrap(), &installed).unwrap();
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = Command::new(&installed)
            .args([
                "--exact",
                "provider_wrapper::tests::linux_release_replacement_subprocess_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("SHIPYARD_TEST_RELEASE_READY", &ready)
            .env("SHIPYARD_TEST_RELEASE_PROCEED", &proceed)
            .env("SHIPYARD_TEST_RELEASE_OUTCOME", &outcome)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "release helper did not become ready"
            );
            std::thread::sleep(POLL_INTERVAL);
        }
        fs::rename(&installed, &retired).unwrap();
        fs::write(&installed, b"replacement is intentionally not executable").unwrap();
        fs::remove_file(&retired).unwrap();
        fs::write(&proceed, b"go").unwrap();
        let status = child
            .wait_timeout(Duration::from_secs(20))
            .unwrap()
            .expect("release replacement helper timed out");
        assert!(status.success());
        assert_eq!(fs::read_to_string(outcome).unwrap(), "delivered");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "subprocess entry for installed-path replacement proof"]
    fn linux_release_replacement_subprocess_helper() {
        let ready = PathBuf::from(std::env::var_os("SHIPYARD_TEST_RELEASE_READY").unwrap());
        let proceed = PathBuf::from(std::env::var_os("SHIPYARD_TEST_RELEASE_PROCEED").unwrap());
        let outcome = PathBuf::from(std::env::var_os("SHIPYARD_TEST_RELEASE_OUTCOME").unwrap());
        fs::write(&ready, b"ready").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !proceed.exists() {
            assert!(
                Instant::now() < deadline,
                "release parent never continued helper"
            );
            std::thread::sleep(POLL_INTERVAL);
        }
        let request = request(ProviderWrapperOperationV1::Submit);
        let (_directory, wrapper, sha) = wrapper_c(&response_program(&request, "delivered"));
        let result = run_provider_wrapper(
            &config(&wrapper, sha),
            &ProviderWrapperEnvironment::default(),
            &request,
        )
        .unwrap();
        assert!(matches!(result, ProviderWrapperRunResult::Delivered { .. }));
        fs::write(outcome, b"delivered").unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_parent_refuses_crashed_supervisor_even_with_forged_success_frame() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let result = LinuxSupervisorResultV3 {
            schema_version: 3,
            provider: LinuxSupervisorProviderV3::Success,
            cleanup: LinuxSupervisorCleanupV3::Clean,
            stdout: b"forged delivered response".to_vec(),
        };
        let mut frames = READY_FRAME.to_vec();
        frames.extend_from_slice(RESULT_FRAME_PREFIX);
        frames.extend_from_slice(&serde_json::to_vec(&result).unwrap());
        frames.push(b'\n');
        assert!(matches!(
            finish_linux_supervisor_result(&request, false, true, None, &frames).unwrap(),
            ProviderWrapperRunResult::Uncertain {
                evidence_digest,
                response_receipt: None,
            } if evidence_digest == digest("provider-wrapper-cleanup-unproven")
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_parent_parser_accepts_only_the_exact_production_frame_shape() {
        let result = LinuxSupervisorResultV3 {
            schema_version: 3,
            provider: LinuxSupervisorProviderV3::Success,
            cleanup: LinuxSupervisorCleanupV3::Clean,
            stdout: Vec::new(),
        };
        let mut exact = READY_FRAME.to_vec();
        exact.extend_from_slice(RESULT_FRAME_PREFIX);
        exact.extend_from_slice(&serde_json::to_vec(&result).unwrap());
        exact.push(b'\n');
        assert!(parse_linux_supervisor_frames_strict(&exact).is_some());
        for malformed in [
            [&b"noise"[..], exact.as_slice()].concat(),
            [exact.as_slice(), &b"noise"[..]].concat(),
            exact[..exact.len() - 1].to_vec(),
            [exact.as_slice(), exact.as_slice()].concat(),
            [
                RESULT_FRAME_PREFIX,
                &serde_json::to_vec(&result).unwrap(),
                b"\n",
                READY_FRAME,
            ]
            .concat(),
        ] {
            assert!(
                parse_linux_supervisor_frames_strict(&malformed).is_none(),
                "accepted malformed supervisor frame: {malformed:?}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_subreaper_terminates_hidden_setsid_dumpable_zero_descendant() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let directory = tempfile::tempdir().unwrap();
        let detached_pid = directory.path().join("linux-hidden.pid");
        let detached_pid_staging = directory.path().join("linux-hidden.pid.tmp");
        let body = format!(
            "pid_t child = fork(); if (child == 0) {{ setsid(); prctl(PR_SET_DUMPABLE, 0, 0, 0, 0); signal(SIGTERM, SIG_IGN); FILE *file = fopen(\"{}\", \"w\"); fprintf(file, \"%d\", getpid()); fclose(file); rename(\"{}\", \"{}\"); sleep(30); return 0; }} \
             for (int i = 0; i < 5000 && access(\"{}\", F_OK) != 0; ++i) usleep(1000); {}",
            detached_pid_staging.display(),
            detached_pid_staging.display(),
            detached_pid.display(),
            detached_pid.display(),
            response_program(&request, "delivered"),
        );
        let (_wrapper_dir, path, sha) = wrapper_c(&body);
        let result = run_provider_wrapper(
            &config(&path, sha),
            &ProviderWrapperEnvironment::default(),
            &request,
        )
        .unwrap();
        assert!(
            matches!(
                &result,
                ProviderWrapperRunResult::Uncertain {
                    evidence_digest,
                    response_receipt: None,
                } if *evidence_digest == digest("provider-wrapper-descendant-violation")
            ),
            "unexpected Linux hidden-descendant result: {result:?}"
        );
        let child_pid = fs::read_to_string(&detached_pid).unwrap();
        assert_pid_eventually_not_running(
            &child_pid,
            "dumpable-zero setsid child survived subreaper cleanup",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_subreaper_kills_descendant_that_closes_sentinel_fd() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let directory = tempfile::tempdir().unwrap();
        let detached_pid = directory.path().join("linux-closed-fd.pid");
        let detached_pid_staging = directory.path().join("linux-closed-fd.pid.tmp");
        let body = format!(
            "pid_t child = fork(); if (child == 0) {{ setsid(); close(9); FILE *file = fopen(\"{}\", \"w\"); fprintf(file, \"%d\", getpid()); fclose(file); rename(\"{}\", \"{}\"); sleep(30); return 0; }} \
             for (int i = 0; i < 5000 && access(\"{}\", F_OK) != 0; ++i) usleep(1000); {}",
            detached_pid_staging.display(),
            detached_pid_staging.display(),
            detached_pid.display(),
            detached_pid.display(),
            response_program(&request, "delivered"),
        );
        let (_wrapper_dir, path, sha) = wrapper_c(&body);
        let result = run_provider_wrapper(
            &config(&path, sha),
            &ProviderWrapperEnvironment::default(),
            &request,
        )
        .unwrap();
        assert!(
            matches!(
                &result,
                ProviderWrapperRunResult::Uncertain {
                    evidence_digest,
                    response_receipt: None,
                } if *evidence_digest == digest("provider-wrapper-descendant-violation")
            ),
            "unexpected Linux closed-fd result: {result:?}"
        );
        let child_pid = fs::read_to_string(&detached_pid).unwrap();
        assert_pid_eventually_not_running(
            &child_pid,
            "closed-fd setsid child survived subreaper cleanup",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_dedicated_supervisor_never_kills_unrelated_nondumpable_process() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let (_unrelated_dir, unrelated_path, _sha) =
            wrapper_c("prctl(PR_SET_DUMPABLE, 0, 0, 0, 0); sleep(30); return 0;");
        let mut unrelated = Command::new(unrelated_path).spawn().unwrap();
        let (_wrapper_dir, path, sha) = wrapper_c(&response_program(&request, "delivered"));
        let result = run_provider_wrapper(
            &config(&path, sha),
            &ProviderWrapperEnvironment::default(),
            &request,
        )
        .unwrap();
        assert!(matches!(result, ProviderWrapperRunResult::Delivered { .. }));
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "dedicated supervisor crossed into an unrelated same-UID process"
        );
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_control_eof_triggers_bounded_subreaper_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        use wait_timeout::ChildExt;

        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let descendant_pid = directory.path().join("abandoned.pid");
        let body = format!(
            "pid_t child = fork(); if (child == 0) {{ setsid(); FILE *file = fopen(\"{}\", \"w\"); fprintf(file, \"%d\", getpid()); fclose(file); sleep(30); return 0; }} waitpid(child, 0, 0); return 0;",
            descendant_pid.display(),
        );
        let (_wrapper_dir, wrapper, sha) = wrapper_c(&body);
        let spec = LinuxSentinelSupervisorSpecV3 {
            schema_version: 3,
            request_bytes: Vec::new(),
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
            provider_deadline_millis: 10_000,
        };
        let spec_bytes = serde_json::to_vec(&spec).unwrap();
        let mut framed_spec = Vec::with_capacity(spec_bytes.len() + 4);
        framed_spec.extend_from_slice(&u32::try_from(spec_bytes.len()).unwrap().to_be_bytes());
        framed_spec.extend_from_slice(&spec_bytes);
        let executable = prepare_platform_executable(File::open(&wrapper).unwrap(), &sha)
            .unwrap()
            .unwrap();
        let (parent_socket, child_socket) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
            None,
        )
        .unwrap();
        let mut command = linux_supervisor_command();
        command
            .env_clear()
            .env("HOME", directory.path())
            .stdin(Stdio::from(child_socket))
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut supervisor = command.spawn().unwrap();
        let mut control = std::os::unix::net::UnixStream::from(parent_socket);
        assert!(send_linux_supervisor_admission(
            &mut control,
            &executable.file,
            &framed_spec,
            Instant::now() + Duration::from_secs(2)
        ));
        let mut channel = supervisor.stdout.take().unwrap();
        let mut frames = Vec::new();
        let mut chunk = [0u8; 512];
        while !linux_supervisor_protocol_slice(&frames).starts_with(READY_FRAME) {
            let count = channel.read(&mut chunk).unwrap();
            assert_ne!(count, 0, "supervisor exited before readiness frame");
            frames.extend_from_slice(&chunk[..count]);
        }
        let pid_deadline = Instant::now() + Duration::from_secs(5);
        while !descendant_pid.exists() {
            assert!(
                Instant::now() < pid_deadline,
                "descendant PID was not published"
            );
            std::thread::sleep(POLL_INTERVAL);
        }
        drop(control);
        let status = supervisor
            .wait_timeout(Duration::from_secs(4))
            .unwrap()
            .expect("control EOF cleanup exceeded its bound");
        assert!(status.success());
        channel.read_to_end(&mut frames).unwrap();
        let (_, result) = parse_linux_supervisor_frames(&frames).unwrap();
        assert_eq!(result.provider, LinuxSupervisorProviderV3::ControlEof);
        assert_eq!(result.cleanup, LinuxSupervisorCleanupV3::ResidualTerminated);
        assert_pid_eventually_not_running(
            &fs::read_to_string(&descendant_pid).unwrap(),
            "control EOF left a live descendant",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_partial_spec_write_to_stopped_supervisor_is_deadline_bounded() {
        let (parent, child_socket) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
            None,
        )
        .unwrap();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "kill -STOP $$; sleep 30"])
            .stdin(Stdio::from(child_socket))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut control = std::os::unix::net::UnixStream::from(parent);
        let bytes = vec![7u8; MAX_SPEC_BYTES];
        let started = Instant::now();
        assert!(!write_linux_supervisor_spec(
            &mut control,
            &bytes,
            started + Duration::from_millis(50)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(control);
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_parallel_supervisors_do_not_cross_kill() {
        let clean_request = request(ProviderWrapperOperationV1::Submit);
        let violating_request = clean_request.clone();
        let clean_body = format!(
            "usleep(250000); {}",
            response_program(&clean_request, "delivered")
        );
        let (_clean_dir, clean_path, clean_sha) = wrapper_c(&clean_body);
        let violation_dir = tempfile::tempdir().unwrap();
        let child_pid = violation_dir.path().join("parallel-violation.pid");
        let violating_body = format!(
            "pid_t child = fork(); if (child < 0) return 91; if (child == 0) {{ setsid(); sleep(30); return 0; }} FILE *file = fopen(\"{}\", \"w\"); if (file == NULL) return 92; fprintf(file, \"%d\", child); fclose(file); {}",
            child_pid.display(),
            response_program(&violating_request, "delivered"),
        );
        let (_violating_dir, violating_path, violating_sha) = wrapper_c(&violating_body);
        let clean = std::thread::spawn(move || {
            run_provider_wrapper(
                &config(&clean_path, clean_sha),
                &ProviderWrapperEnvironment::default(),
                &clean_request,
            )
            .unwrap()
        });
        let violating = std::thread::spawn(move || {
            run_provider_wrapper(
                &config(&violating_path, violating_sha),
                &ProviderWrapperEnvironment::default(),
                &violating_request,
            )
            .unwrap()
        });
        assert!(matches!(
            clean.join().unwrap(),
            ProviderWrapperRunResult::Delivered { .. }
        ));
        assert!(matches!(
            violating.join().unwrap(),
            ProviderWrapperRunResult::Uncertain {
                evidence_digest,
                response_receipt: None,
            } if evidence_digest == digest("provider-wrapper-descendant-violation")
        ));
        let pid_deadline = Instant::now() + Duration::from_secs(5);
        while !child_pid.exists() {
            assert!(
                Instant::now() < pid_deadline,
                "parallel child PID was not published"
            );
            std::thread::sleep(POLL_INTERVAL);
        }
        assert_pid_eventually_not_running(
            &fs::read_to_string(child_pid).unwrap(),
            "parallel violating child survived isolated cleanup",
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_parallel_supervisors_receive_only_their_exact_executable_fd() {
        let delivered_request = request(ProviderWrapperOperationV1::Submit);
        let retryable_request = delivered_request.clone();
        let descriptor_count = "DIR *directory = opendir(\"/proc/self/fd\"); if (directory == NULL) return 93; struct dirent *entry; int memfds = 0; char path[512]; char target[512]; while ((entry = readdir(directory)) != NULL) { snprintf(path, sizeof(path), \"/proc/self/fd/%s\", entry->d_name); ssize_t length = readlink(path, target, sizeof(target) - 1); if (length < 0) continue; target[length] = '\\0'; if (strstr(target, \"memfd:shipyard-provider-wrapper\") != NULL) memfds++; } closedir(directory); if (memfds != 1) return 94; ";
        let (_delivered_dir, delivered_path, delivered_sha) = wrapper_c(&format!(
            "{descriptor_count}{}",
            response_program(&delivered_request, "delivered")
        ));
        let (_retryable_dir, retryable_path, retryable_sha) = wrapper_c(&format!(
            "{descriptor_count}{}",
            response_program(&retryable_request, "retryable")
        ));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let delivered_barrier = barrier.clone();
        let delivered = std::thread::spawn(move || {
            let prepared =
                prepare_platform_executable(File::open(&delivered_path).unwrap(), &delivered_sha)
                    .unwrap()
                    .unwrap();
            delivered_barrier.wait();
            assert!(
                Command::new("/bin/sh")
                    .args([
                        "-c",
                        "count=0; for fd in /proc/self/fd/*; do target=$(readlink \"$fd\" 2>/dev/null || true); case \"$target\" in *memfd:shipyard-provider-wrapper*) count=$((count + 1));; esac; done; test \"$count\" -eq 0",
                    ])
                    .status()
                    .unwrap()
                    .success(),
                "unrelated concurrent child inherited a wrapper capability"
            );
            run_provider_wrapper_linux_supervised(
                &config(&delivered_path, delivered_sha),
                &ProviderWrapperEnvironment::default(),
                &delivered_request,
                &serde_json::to_vec(&delivered_request).unwrap(),
                &prepared,
            )
            .unwrap()
        });
        let retryable_barrier = barrier;
        let retryable = std::thread::spawn(move || {
            let prepared =
                prepare_platform_executable(File::open(&retryable_path).unwrap(), &retryable_sha)
                    .unwrap()
                    .unwrap();
            retryable_barrier.wait();
            assert!(
                Command::new("/bin/sh")
                    .args([
                        "-c",
                        "count=0; for fd in /proc/self/fd/*; do target=$(readlink \"$fd\" 2>/dev/null || true); case \"$target\" in *memfd:shipyard-provider-wrapper*) count=$((count + 1));; esac; done; test \"$count\" -eq 0",
                    ])
                    .status()
                    .unwrap()
                    .success(),
                "unrelated concurrent child inherited a wrapper capability"
            );
            run_provider_wrapper_linux_supervised(
                &config(&retryable_path, retryable_sha),
                &ProviderWrapperEnvironment::default(),
                &retryable_request,
                &serde_json::to_vec(&retryable_request).unwrap(),
                &prepared,
            )
            .unwrap()
        });
        assert!(matches!(
            delivered.join().unwrap(),
            ProviderWrapperRunResult::Delivered { .. }
        ));
        assert!(matches!(
            retryable.join().unwrap(),
            ProviderWrapperRunResult::Retryable { .. }
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_production_executable_capability_can_start_at_fd9() {
        let executable = std::env::current_exe().unwrap();
        let status = Command::new("/bin/sh")
            .args([
                "-c",
                "exec 3</dev/null; exec 4</dev/null; exec 5</dev/null; exec 6</dev/null; exec 7</dev/null; exec 8</dev/null; exec 9>&-; exec \"$@\"",
                "shipyard-fd9-regression",
            ])
            .arg(executable)
            .args([
                "--exact",
                "provider_wrapper::tests::linux_production_executable_fd9_subprocess_helper",
                "--ignored",
                "--nocapture",
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "subprocess entry for exact production fd-9 capability proof"]
    fn linux_production_executable_fd9_subprocess_helper() {
        use std::os::fd::AsRawFd;

        let request = request(ProviderWrapperOperationV1::Submit);
        let (_directory, path, sha) = wrapper_c(&response_program(&request, "delivered"));
        let source_at_nine = File::open(&path).unwrap();
        assert_eq!(source_at_nine.as_raw_fd(), 9);
        let source = File::from(rustix::io::fcntl_dupfd_cloexec(&source_at_nine, 10).unwrap());
        drop(source_at_nine);
        let prepared = prepare_platform_executable(source, &sha).unwrap().unwrap();
        assert_eq!(prepared.file.as_raw_fd(), 9);
        assert!(
            rustix::io::fcntl_getfd(&prepared.file)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        let request_bytes = serde_json::to_vec(&request).unwrap();
        let result = run_provider_wrapper_linux_supervised(
            &config(&path, sha),
            &ProviderWrapperEnvironment::default(),
            &request,
            &request_bytes,
            &prepared,
        )
        .unwrap();
        assert!(matches!(result, ProviderWrapperRunResult::Delivered { .. }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "subprocess entry for the production Linux sentinel supervisor"]
    fn linux_sentinel_supervisor_subprocess_helper() {
        run_provider_sentinel_supervisor_command().expect("production sentinel supervisor");
    }

    #[cfg(unix)]
    #[test]
    fn malformed_nonzero_and_response_fence_mismatch_are_uncertain() {
        let request = request(ProviderWrapperOperationV1::Submit);
        for body in [
            "fputs(\"nope\", stdout); return 0;",
            "return 7;",
            "fputs(\"{\\\"schema_version\\\":1}\", stdout); return 0;",
        ] {
            let (_directory, path, sha) = wrapper_c(body);
            assert!(matches!(
                run_provider_wrapper(
                    &config(&path, sha),
                    &ProviderWrapperEnvironment::default(),
                    &request
                )
                .unwrap(),
                ProviderWrapperRunResult::Uncertain { .. }
            ));
        }
    }

    #[test]
    fn environment_is_explicitly_allowlisted() {
        assert!(
            ProviderWrapperEnvironment::new([("GITHUB_TOKEN".into(), OsString::from("secret"))])
                .is_err()
        );
        assert!(ProviderWrapperEnvironment::new([("HOME".into(), OsString::from("/tmp"))]).is_ok());
    }

    #[test]
    fn provider_selections_are_symmetric_with_protected_metadata() {
        let selected = vec![
            "resume".to_owned(),
            "--model".to_owned(),
            "model-a".to_owned(),
            "--effort=high".to_owned(),
            "session-a".to_owned(),
        ];
        assert!(exact_provider_route(
            &selected,
            Some("model-a"),
            Some("high"),
            Some("session-a")
        ));
        assert!(!exact_provider_route(
            &selected,
            None,
            Some("high"),
            Some("session-a")
        ));
        assert!(!exact_provider_route(
            &selected,
            Some("model-a"),
            None,
            Some("session-a")
        ));
        let duplicated = [selected.clone(), vec!["--model".into(), "model-b".into()]].concat();
        assert!(!exact_provider_route(
            &duplicated,
            Some("model-a"),
            Some("high"),
            Some("session-a")
        ));
        let unsafe_flag = [selected, vec!["--dangerously-bypass-approvals".into()]].concat();
        assert!(!exact_provider_route(
            &unsafe_flag,
            Some("model-a"),
            Some("high"),
            Some("session-a")
        ));
    }

    #[test]
    fn registered_subrouter_routes_include_qwen_agy_and_kimi_without_claiming_execution() {
        for provider in ["qwen", "agy", "kimi"] {
            assert!(registered_provider_route_shape(provider).is_some());
            let mut request = request(ProviderWrapperOperationV1::Submit);
            request.provider_id = provider.to_owned();
            request.protected_route.argv[1] = provider.to_owned();
            request.protected_route.fresh_argv[1] = provider.to_owned();
            let mut config = config(Path::new("/does/not/matter"), digest("unused"));
            config.provider_id = provider.to_owned();
            assert!(validate_request(&config, &request).is_ok(), "{provider}");
        }
        assert_eq!(registered_provider_route_shape("unregistered"), None);
    }

    #[test]
    fn herdr_endpoint_requires_both_incarnation_and_direct_launch_proof() {
        let config = config(Path::new("/does/not/matter"), digest("unused"));
        let mut request = request(ProviderWrapperOperationV1::Submit);
        request.terminal_endpoint = TerminalEndpointV1::HerdR {
            socket_path: native_absolute_test_path("herdr.sock"),
            server_incarnation: None,
            direct_fresh_launch_proven: true,
        };
        assert!(validate_request(&config, &request).is_err());
        request.terminal_endpoint = TerminalEndpointV1::HerdR {
            socket_path: native_absolute_test_path("herdr.sock"),
            server_incarnation: Some("server-epoch-1".into()),
            direct_fresh_launch_proven: false,
        };
        assert!(validate_request(&config, &request).is_err());
        request.terminal_endpoint = TerminalEndpointV1::HerdR {
            socket_path: native_absolute_test_path("herdr.sock"),
            server_incarnation: Some("server-epoch-1".into()),
            direct_fresh_launch_proven: true,
        };
        assert!(validate_request(&config, &request).is_ok());
    }

    #[test]
    fn cmux_only_v1_wire_requests_refuse_instead_of_cross_decoding() {
        let config = config(Path::new("/does/not/matter"), digest("unused"));
        let mut request = request(ProviderWrapperOperationV1::Submit);
        request.schema_version = 1;
        assert!(validate_request(&config, &request).is_err());
    }
}
