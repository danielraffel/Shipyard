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
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::process::ProcessTree;
use crate::workstream_continuation_config::ProviderWrapperConfig;

mod execution_sentinel;
#[cfg(not(unix))]
use execution_sentinel::SentinelCleanup;
use execution_sentinel::terminate_sentinel_processes;

const SCHEMA_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_WRAPPER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 4 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TEARDOWN_BUDGET: Duration = Duration::from_millis(500);
#[cfg(target_os = "macos")]
const SENTINEL_TEARDOWN_BUDGET: Duration = Duration::from_secs(2);
#[cfg(not(target_os = "macos"))]
const SENTINEL_TEARDOWN_BUDGET: Duration = TEARDOWN_BUDGET;
const EXECUTION_SENTINEL_FD_ENV: &str = "SHIPYARD_PROVIDER_SENTINEL_FD";
#[cfg(test)]
static PROVIDER_EXECUTION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    pub(crate) resume_expectation: FreshResumeExpectationV1,
    pub(crate) launch_options: ProviderLaunchOptionsV1,
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
    return ProviderExecutableIdentityBoundary::KernelSealedSnapshot;
    #[cfg(target_os = "macos")]
    return ProviderExecutableIdentityBoundary::TrustedSameUidPrivateSnapshot;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return ProviderExecutableIdentityBoundary::Unsupported;
}

/// Execute one request through the immutable configured wrapper.
pub(crate) fn run_provider_wrapper(
    config: &ProviderWrapperConfig,
    environment: &ProviderWrapperEnvironment,
    request: &ProviderWrapperRequestV1,
) -> Result<ProviderWrapperRunResult, ProviderWrapperRefusal> {
    // The subprocess fixtures share a deliberately tight deadline and invoke
    // host-wide process observation. Serialize only tests so unrelated test
    // threads cannot manufacture scheduler/lsof starvation failures.
    #[cfg(test)]
    let _test_execution = PROVIDER_EXECUTION_TEST_LOCK
        .lock()
        .map_err(|_| refusal("provider wrapper test execution lock is poisoned"))?;
    validate_request(config, request)?;
    if !provider_wrapper_execution_supported() {
        return Ok(uncertain("platform-cannot-prove-exact-wrapper-execution"));
    }
    let request_bytes = serde_json::to_vec(request)
        .map_err(|_| refusal("provider wrapper request cannot be serialized"))?;
    if request_bytes.len() > MAX_REQUEST_BYTES {
        return Err(refusal(
            "provider wrapper request exceeds the bounded input limit",
        ));
    }

    #[cfg(not(unix))]
    {
        let _ = (environment, request_bytes);
        return Ok(uncertain("platform-cannot-prove-exact-wrapper-execution"));
    }

    #[cfg(unix)]
    run_provider_wrapper_unix(config, environment, request, &request_bytes)
}

pub(crate) fn validate_request(
    config: &ProviderWrapperConfig,
    request: &ProviderWrapperRequestV1,
) -> Result<(), ProviderWrapperRefusal> {
    if request.schema_version != SCHEMA_VERSION
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
    validate_launch_options(&request.launch_options)?;
    Ok(())
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
    #[cfg(unix)]
    let sentinel_fd = {
        use std::os::fd::AsRawFd;
        rustix::io::fcntl_setfd(&sentinel, rustix::io::FdFlags::empty())
            .map_err(|_| refusal("provider wrapper execution sentinel cannot be inherited"))?;
        sentinel.as_raw_fd()
    };
    let mut command = Command::new(&prepared.path);
    command.env_clear().envs(environment.0.iter());
    #[cfg(unix)]
    command.env(EXECUTION_SENTINEL_FD_ENV, sentinel_fd.to_string());
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
    #[cfg(unix)]
    rustix::io::fcntl_setfd(&sentinel, rustix::io::FdFlags::CLOEXEC)
        .map_err(|_| refusal("provider wrapper execution sentinel cannot be isolated"))?;
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
    #[cfg(unix)]
    let sentinel_cleanup = terminate_sentinel_processes(
        &sentinel_path,
        Instant::now() + SENTINEL_TEARDOWN_BUDGET,
        POLL_INTERVAL,
    );
    #[cfg(not(unix))]
    let sentinel_cleanup = SentinelCleanup {
        proven: true,
        residual_detected: false,
    };
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

#[cfg(unix)]
struct PreparedExecutable {
    path: std::path::PathBuf,
    _file: File,
    _private_directory: Option<tempfile::TempDir>,
}

#[cfg(all(unix, target_os = "linux"))]
fn prepare_platform_executable(
    mut executable: File,
    expected_sha256: &str,
) -> Result<Option<PreparedExecutable>, ProviderWrapperRefusal> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    let descriptor = rustix::fs::memfd_create(
        "shipyard-provider-wrapper",
        rustix::fs::MemfdFlags::ALLOW_SEALING | rustix::fs::MemfdFlags::EXEC,
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

    rustix::io::fcntl_setfd(&sealed, rustix::io::FdFlags::empty()).map_err(|_| {
        refusal("platform cannot preserve the verified wrapper descriptor across exec")
    })?;
    let path = std::path::PathBuf::from(format!("/proc/self/fd/{}", sealed.as_raw_fd()));
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(PreparedExecutable {
        path,
        _file: sealed,
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
        _file: verified,
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

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_exceeds(file: &File, limit: u64) -> bool {
    file.metadata()
        .map_or(true, |metadata| metadata.len() > limit)
}

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
    if response.schema_version != SCHEMA_VERSION
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
            schema_version: 1,
            operation,
            provider_id: "codex".into(),
            adapter_id: "codex-wrapper-v1".into(),
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
            schema_version: 1,
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
            schema_version: 1,
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
            deadline_seconds: 2,
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
            "#include <stdio.h>\n#include <stdlib.h>\n#include <string.h>\n#include <unistd.h>\n#include <sys/wait.h>\nint main(void) {{ {source_body} }}\n"
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
    fn assert_pid_eventually_gone(pid: &str, failure_message: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = Command::new("kill")
                .args(["-0", "--", pid.trim()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            if !status.success() {
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
            schema_version: 1,
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
            "char input[65537] = {{0}}; size_t count = fread(input, 1, sizeof(input) - 1, stdin); if (count == 0 || strstr(input, \"\\\"operation\\\":\\\"{operation}\\\"\") == NULL || getenv(\"GITHUB_TOKEN\") != NULL) return 91; unsigned char output[] = {{{bytes}}}; return fwrite(output, 1, sizeof(output), stdout) == sizeof(output) ? 0 : 1;"
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
            schema_version: 1,
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
        let config = config(&path, sha);
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
        timeout_config.deadline_seconds = 5;
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
        assert_pid_eventually_gone(&child_pid, "descendant survived timeout");

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
    fn timeout_reaps_setsid_descendant_without_pipe_reader_threads() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let (directory, path, sha) = wrapper_c(
            "pid_t child = fork(); if (child == 0) { setsid(); const char *home = getenv(\"HOME\"); char path[4096]; snprintf(path, sizeof(path), \"%s/detached.pid\", home); FILE *file = fopen(path, \"w\"); fprintf(file, \"%d\", getpid()); fclose(file); sleep(30); return 0; } waitpid(child, 0, 0); return 0;",
        );
        let mut timeout_config = config(&path, sha);
        timeout_config.deadline_seconds = 5;
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
            started.elapsed() < Duration::from_secs(30),
            "timeout cleanup plus serialized fixture admission wedged"
        );
        let child_pid = fs::read_to_string(directory.path().join("detached.pid")).unwrap();
        assert_pid_eventually_gone(&child_pid, "setsid descendant survived timeout");
    }

    #[cfg(unix)]
    #[test]
    fn successful_parent_exit_with_setsid_child_is_refused_and_reaped() {
        let request = request(ProviderWrapperOperationV1::Submit);
        let directory = tempfile::tempdir().unwrap();
        let detached_pid = directory.path().join("detached-success.pid");
        let body = format!(
            "pid_t child = fork(); if (child == 0) {{ setsid(); FILE *file = fopen(\"{}\", \"w\"); fprintf(file, \"%d\", getpid()); fclose(file); sleep(30); return 0; }} \
             for (int i = 0; i < 5000 && access(\"{}\", F_OK) != 0; ++i) usleep(1000); {}",
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
        assert_pid_eventually_gone(
            &child_pid,
            "setsid child survived successful wrapper-parent exit",
        );
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
}
