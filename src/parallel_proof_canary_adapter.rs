//! Digest-pinned production protocol for the default-off parallel-proof canary.
//!
//! The executable is configured only in trusted machine-global policy. It
//! receives one strict JSON request per idempotent operation on stdin and must
//! return one strict JSON response on stdout. No shell text, ambient
//! environment, project configuration, or executable-provided authority is
//! trusted by this module.

use std::fmt::{Display, Formatter};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::config::LoadedConfig;
use crate::parallel_proof::{
    ParallelProofContext, ParallelProofError, ParallelProofManifest, Sha256Digest, ShardPlan,
    TestInventory,
};
use crate::parallel_proof_canary::{CanaryHostObservation, PulpMacCanaryPolicy};
use crate::parallel_proof_canary_driver::{DistributedExecutionObservation, PulpMacCanaryExecutor};
use crate::parallel_proof_canary_receipt::SingleHostControlReceipt;

const POLICY_KEY: &str = "parallel_proof_canary";
const PROTOCOL_SCHEMA: u32 = 1;
const MAX_EXECUTABLE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PROTOCOL_BYTES: u64 = 1024 * 1024;
const MAX_DEADLINE_SECONDS: u64 = 3_600;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParallelProofCanaryAdapterConfig {
    pub(crate) executable_path: PathBuf,
    pub(crate) executable_sha256: Sha256Digest,
    pub(crate) deadline_seconds: u64,
    pub(crate) max_stdout_bytes: u64,
    pub(crate) max_stderr_bytes: u64,
    pub(crate) invocation_authority_sha256: Sha256Digest,
    pub(crate) repository_id: u64,
    pub(crate) repository: String,
    pub(crate) target: String,
    pub(crate) target_triple: String,
    pub(crate) builder_host_id: String,
    pub(crate) worker_host_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParallelProofCanaryActivation {
    pub(crate) apply_enabled: bool,
    pub(crate) adapter: ParallelProofCanaryAdapterConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParallelProofCanaryConfigError(String);

impl Display for ParallelProofCanaryConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ParallelProofCanaryConfigError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    #[serde(default)]
    activation_enabled: bool,
    #[serde(default)]
    apply_enabled: bool,
    executable_path: Option<String>,
    executable_sha256: Option<String>,
    deadline_seconds: Option<u64>,
    max_stdout_bytes: Option<u64>,
    max_stderr_bytes: Option<u64>,
    invocation_authority_sha256: Option<String>,
    repository_id: Option<u64>,
    repository: Option<String>,
    target: Option<String>,
    target_triple: Option<String>,
    builder_host_id: Option<String>,
    worker_host_id: Option<String>,
}

pub(crate) fn trusted_parallel_proof_canary_config(
    config: &LoadedConfig,
) -> Result<Option<ParallelProofCanaryActivation>, ParallelProofCanaryConfigError> {
    let trusted = LoadedConfig::load_machine_global_from_dir(config.global_dir.clone())
        .map_err(|error| refusal(format!("load trusted {POLICY_KEY} policy: {error}")))?;
    let Some(value) = trusted.get(POLICY_KEY) else {
        return Ok(None);
    };
    let raw: RawPolicy = value
        .clone()
        .try_into()
        .map_err(|error| refusal(format!("decode {POLICY_KEY}: {error}")))?;
    if !raw.activation_enabled {
        if raw.apply_enabled || raw_has_authority(&raw) {
            return Err(refusal(format!(
                "{POLICY_KEY} is disabled but contains activation-only fields"
            )));
        }
        return Ok(None);
    }
    if !raw.apply_enabled {
        return Err(refusal(format!(
            "{POLICY_KEY} requires activation_enabled and apply_enabled together"
        )));
    }
    Ok(Some(ParallelProofCanaryActivation {
        apply_enabled: true,
        adapter: validate_enabled(raw)?,
    }))
}

fn raw_has_authority(raw: &RawPolicy) -> bool {
    raw.executable_path.is_some()
        || raw.executable_sha256.is_some()
        || raw.deadline_seconds.is_some()
        || raw.max_stdout_bytes.is_some()
        || raw.max_stderr_bytes.is_some()
        || raw.invocation_authority_sha256.is_some()
        || raw.repository_id.is_some()
        || raw.repository.is_some()
        || raw.target.is_some()
        || raw.target_triple.is_some()
        || raw.builder_host_id.is_some()
        || raw.worker_host_id.is_some()
}

fn validate_enabled(
    raw: RawPolicy,
) -> Result<ParallelProofCanaryAdapterConfig, ParallelProofCanaryConfigError> {
    let executable_path = PathBuf::from(required(raw.executable_path, "executable_path")?);
    if !bounded_normalized_absolute_path(&executable_path) {
        return Err(refusal(format!(
            "{POLICY_KEY}.executable_path must be a bounded normalized absolute path"
        )));
    }
    let executable_sha256 =
        Sha256Digest::parse(required(raw.executable_sha256, "executable_sha256")?)
            .map_err(|_| refusal(format!("{POLICY_KEY}.executable_sha256 is invalid")))?;
    let deadline_seconds = required(raw.deadline_seconds, "deadline_seconds")?;
    let max_stdout_bytes = required(raw.max_stdout_bytes, "max_stdout_bytes")?;
    let max_stderr_bytes = required(raw.max_stderr_bytes, "max_stderr_bytes")?;
    let invocation_authority_sha256 = Sha256Digest::parse(required(
        raw.invocation_authority_sha256,
        "invocation_authority_sha256",
    )?)
    .map_err(|_| {
        refusal(format!(
            "{POLICY_KEY}.invocation_authority_sha256 is invalid"
        ))
    })?;
    if !(1..=MAX_DEADLINE_SECONDS).contains(&deadline_seconds)
        || !(1..=MAX_PROTOCOL_BYTES).contains(&max_stdout_bytes)
        || !(1..=MAX_PROTOCOL_BYTES).contains(&max_stderr_bytes)
    {
        return Err(refusal(format!(
            "{POLICY_KEY} execution bounds are outside the supported range"
        )));
    }
    let repository_id = required(raw.repository_id, "repository_id")?;
    let repository = required(raw.repository, "repository")?;
    let target = required(raw.target, "target")?;
    let target_triple = required(raw.target_triple, "target_triple")?;
    let builder_host_id = required(raw.builder_host_id, "builder_host_id")?;
    let worker_host_id = required(raw.worker_host_id, "worker_host_id")?;
    if repository_id == 0
        || !canonical_repository(&repository)
        || !bounded_id(&target)
        || !bounded_id(&target_triple)
        || !bounded_id(&builder_host_id)
        || !bounded_id(&worker_host_id)
        || builder_host_id == worker_host_id
    {
        return Err(refusal(format!(
            "{POLICY_KEY} repository, target, or host authority is invalid"
        )));
    }
    Ok(ParallelProofCanaryAdapterConfig {
        executable_path,
        executable_sha256,
        deadline_seconds,
        max_stdout_bytes,
        max_stderr_bytes,
        invocation_authority_sha256,
        repository_id,
        repository,
        target,
        target_triple,
        builder_host_id,
        worker_host_id,
    })
}

fn required<T>(value: Option<T>, field: &str) -> Result<T, ParallelProofCanaryConfigError> {
    value.ok_or_else(|| refusal(format!("{POLICY_KEY}.{field} is required")))
}

fn refusal(message: impl Into<String>) -> ParallelProofCanaryConfigError {
    ParallelProofCanaryConfigError(message.into())
}

fn bounded_normalized_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= 4096
        && path.components().collect::<PathBuf>() == path
        && !path
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
}

fn canonical_repository(value: &str) -> bool {
    let mut parts = value.split('/');
    let valid = |part: &str| {
        !part.is_empty()
            && part.len() <= 100
            && part.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
    };
    let owner = parts.next().unwrap_or_default();
    let repository = parts.next().unwrap_or_default();
    parts.next().is_none() && valid(owner) && valid(repository)
}

fn bounded_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct CanaryAdapterAuthority {
    repository_id: u64,
    repository: String,
    target: String,
    target_triple: String,
    builder_host_id: String,
    worker_host_id: String,
    correlation_id: String,
    manifest_digest: Sha256Digest,
    invocation_authority_sha256: Sha256Digest,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CanaryAdapterOperation {
    ObserveInitialHosts,
    RunSingleHostControl,
    ObservePreExecutionHosts,
    RunDistributedShadow,
    ObserveFinalHosts,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CanaryAdapterRequestPayload<'a> {
    ObserveHosts,
    SingleHostControl {
        manifest: &'a ParallelProofManifest,
        inventory: &'a TestInventory,
        plan: &'a ShardPlan,
        host: &'a CanaryHostObservation,
    },
    DistributedShadow {
        manifest: &'a ParallelProofManifest,
        inventory: &'a TestInventory,
        plan: &'a ShardPlan,
        hosts: &'a [CanaryHostObservation],
    },
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct CanaryAdapterRequest<'a> {
    schema_version: u32,
    operation: CanaryAdapterOperation,
    idempotency_key: String,
    authority_sha256: Sha256Digest,
    payload_sha256: Sha256Digest,
    authority: &'a CanaryAdapterAuthority,
    payload: CanaryAdapterRequestPayload<'a>,
    model_calls: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CanaryAdapterResponse {
    schema_version: u32,
    operation: CanaryAdapterOperation,
    idempotency_key: String,
    authority_sha256: Sha256Digest,
    payload_sha256: Sha256Digest,
    result: CanaryAdapterResponsePayload,
    model_calls: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CanaryAdapterResponsePayload {
    HostObservations {
        observations: Vec<CanaryHostObservation>,
    },
    SingleHostControl {
        receipt: SingleHostControlReceipt,
    },
    DistributedShadow {
        observation: DistributedExecutionObservation,
    },
}

pub(crate) trait CanaryProtocolRunner {
    fn invoke(&mut self, request: &[u8]) -> Result<Vec<u8>, ParallelProofError>;
}

pub(crate) struct DigestPinnedCanaryProtocolRunner {
    config: ParallelProofCanaryAdapterConfig,
}

impl DigestPinnedCanaryProtocolRunner {
    #[must_use]
    pub(crate) fn new(config: ParallelProofCanaryAdapterConfig) -> Self {
        Self { config }
    }
}

impl CanaryProtocolRunner for DigestPinnedCanaryProtocolRunner {
    fn invoke(&mut self, request: &[u8]) -> Result<Vec<u8>, ParallelProofError> {
        if request.len() as u64 > MAX_PROTOCOL_BYTES {
            return Err(ParallelProofError::LimitExceeded {
                field: "canary adapter request bytes",
                max: usize::try_from(MAX_PROTOCOL_BYTES).unwrap_or(usize::MAX),
                found: request.len(),
            });
        }
        run_digest_pinned(&self.config, request)
    }
}

pub(crate) struct ProductionParallelProofCanaryExecutor<R> {
    runner: R,
    authority: CanaryAdapterAuthority,
    authority_sha256: Sha256Digest,
    observation_count: u8,
    manifest: Option<ParallelProofManifest>,
    inventory: Option<TestInventory>,
    plan: Option<ShardPlan>,
    pre_execution_hosts: Vec<CanaryHostObservation>,
}

impl<R: CanaryProtocolRunner> ProductionParallelProofCanaryExecutor<R> {
    pub(crate) fn new(
        runner: R,
        config: &ParallelProofCanaryAdapterConfig,
        proof: ParallelProofContext<'_>,
        policy: &PulpMacCanaryPolicy,
        correlation_id: String,
    ) -> Result<Self, ParallelProofError> {
        let manifest_digest = proof.manifest.digest(proof.inventory, proof.plan)?;
        if !policy.enabled
            || config.repository_id != policy.repository_id
            || config.repository != policy.repository
            || config.target != policy.target
            || config.target_triple != policy.target_triple
            || config.builder_host_id != policy.builder_host_id
            || config.worker_host_id != policy.worker_host_id
            || proof.manifest.source.repository_id != policy.repository_id
            || proof.manifest.source.repository != policy.repository
            || proof.manifest.build.target_triple != policy.target_triple
        {
            return Err(ParallelProofError::BindingMismatch(
                "parallel-proof canary adapter authority",
            ));
        }
        let authority = CanaryAdapterAuthority {
            repository_id: policy.repository_id,
            repository: policy.repository.clone(),
            target: policy.target.clone(),
            target_triple: policy.target_triple.clone(),
            builder_host_id: policy.builder_host_id.clone(),
            worker_host_id: policy.worker_host_id.clone(),
            correlation_id,
            manifest_digest,
            invocation_authority_sha256: config.invocation_authority_sha256.clone(),
        };
        let authority_sha256 = protocol_digest("shipyard.canary-adapter.authority.v1", &authority)?;
        Ok(Self {
            runner,
            authority,
            authority_sha256,
            observation_count: 0,
            manifest: Some(proof.manifest.clone()),
            inventory: Some(proof.inventory.clone()),
            plan: Some(proof.plan.clone()),
            pre_execution_hosts: Vec::new(),
        })
    }

    fn invoke(
        &mut self,
        operation: CanaryAdapterOperation,
        payload: CanaryAdapterRequestPayload<'_>,
    ) -> Result<CanaryAdapterResponsePayload, ParallelProofError> {
        let payload_sha256 = protocol_digest("shipyard.canary-adapter.payload.v1", &payload)?;
        let idempotency_key = protocol_digest(
            "shipyard.canary-adapter.operation.v1",
            &(
                PROTOCOL_SCHEMA,
                self.authority_sha256.clone(),
                operation,
                payload_sha256.clone(),
            ),
        )?
        .as_str()
        .to_owned();
        let request = CanaryAdapterRequest {
            schema_version: PROTOCOL_SCHEMA,
            operation,
            idempotency_key: idempotency_key.clone(),
            authority_sha256: self.authority_sha256.clone(),
            payload_sha256: payload_sha256.clone(),
            authority: &self.authority,
            payload,
            model_calls: 0,
        };
        let request_bytes = serde_json::to_vec(&request)?;
        let response_bytes = self.runner.invoke(&request_bytes)?;
        if response_bytes.len() as u64 > MAX_PROTOCOL_BYTES {
            return Err(ParallelProofError::LimitExceeded {
                field: "canary adapter response bytes",
                max: usize::try_from(MAX_PROTOCOL_BYTES).unwrap_or(usize::MAX),
                found: response_bytes.len(),
            });
        }
        let response: CanaryAdapterResponse = serde_json::from_slice(&response_bytes)
            .map_err(|_| ParallelProofError::CorruptRecord("canary adapter response".into()))?;
        if response.schema_version != PROTOCOL_SCHEMA
            || response.operation != operation
            || response.idempotency_key != idempotency_key
            || response.authority_sha256 != self.authority_sha256
            || response.payload_sha256 != payload_sha256
            || response.model_calls != 0
        {
            return Err(ParallelProofError::BindingMismatch(
                "canary adapter response authority",
            ));
        }
        Ok(response.result)
    }
}

impl<R: CanaryProtocolRunner> PulpMacCanaryExecutor for ProductionParallelProofCanaryExecutor<R> {
    fn controller_now_ms(&mut self) -> Result<u64, ParallelProofError> {
        u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ParallelProofError::InvalidField("controller clock"))?
                .as_millis(),
        )
        .map_err(|_| ParallelProofError::InvalidField("controller clock"))
    }

    fn authenticated_host_observations(
        &mut self,
    ) -> Result<Vec<CanaryHostObservation>, ParallelProofError> {
        let operation = match self.observation_count {
            0 => CanaryAdapterOperation::ObserveInitialHosts,
            1 => CanaryAdapterOperation::ObservePreExecutionHosts,
            2 => CanaryAdapterOperation::ObserveFinalHosts,
            _ => {
                return Err(ParallelProofError::InvalidAttemptSequence(
                    "canary adapter host observation sequence".to_owned(),
                ));
            }
        };
        let result = self.invoke(operation, CanaryAdapterRequestPayload::ObserveHosts)?;
        self.observation_count += 1;
        match result {
            CanaryAdapterResponsePayload::HostObservations { observations } => {
                if self.observation_count == 2 {
                    self.pre_execution_hosts.clone_from(&observations);
                }
                Ok(observations)
            }
            _ => Err(ParallelProofError::BindingMismatch(
                "canary adapter host response",
            )),
        }
    }

    fn run_single_host_control(
        &mut self,
        proof: ParallelProofContext<'_>,
        host: &CanaryHostObservation,
    ) -> Result<SingleHostControlReceipt, ParallelProofError> {
        if self.observation_count != 1 {
            return Err(ParallelProofError::InvalidAttemptSequence(
                "canary adapter control sequence".to_owned(),
            ));
        }
        match self.invoke(
            CanaryAdapterOperation::RunSingleHostControl,
            CanaryAdapterRequestPayload::SingleHostControl {
                manifest: proof.manifest,
                inventory: proof.inventory,
                plan: proof.plan,
                host,
            },
        )? {
            CanaryAdapterResponsePayload::SingleHostControl { receipt } => Ok(receipt),
            _ => Err(ParallelProofError::BindingMismatch(
                "canary adapter control response",
            )),
        }
    }

    fn run_distributed_shadow(
        &mut self,
        manifest_digest: &Sha256Digest,
    ) -> Result<DistributedExecutionObservation, ParallelProofError> {
        if self.observation_count != 2 || manifest_digest != &self.authority.manifest_digest {
            return Err(ParallelProofError::BindingMismatch(
                "canary adapter distributed sequence",
            ));
        }
        let manifest = self
            .manifest
            .clone()
            .ok_or(ParallelProofError::InvalidField(
                "canary adapter distributed manifest",
            ))?;
        let inventory = self
            .inventory
            .clone()
            .ok_or(ParallelProofError::InvalidField(
                "canary adapter distributed inventory",
            ))?;
        let plan = self.plan.clone().ok_or(ParallelProofError::InvalidField(
            "canary adapter distributed plan",
        ))?;
        let hosts = self.pre_execution_hosts.clone();
        match self.invoke(
            CanaryAdapterOperation::RunDistributedShadow,
            CanaryAdapterRequestPayload::DistributedShadow {
                manifest: &manifest,
                inventory: &inventory,
                plan: &plan,
                hosts: &hosts,
            },
        )? {
            CanaryAdapterResponsePayload::DistributedShadow { observation } => Ok(observation),
            _ => Err(ParallelProofError::BindingMismatch(
                "canary adapter distributed response",
            )),
        }
    }
}

fn protocol_digest(
    domain: &str,
    value: &impl Serialize,
) -> Result<Sha256Digest, ParallelProofError> {
    let payload = serde_json::to_vec(value)?;
    let mut bytes = Vec::with_capacity(16 + domain.len() + payload.len());
    bytes.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    bytes.extend_from_slice(domain.as_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    bytes.extend_from_slice(&payload);
    Ok(Sha256Digest::of_bytes(&bytes))
}

pub(crate) fn canary_invocation_authority_digest(
    policy: &PulpMacCanaryPolicy,
    timing: &crate::parallel_proof_canary::CanaryTimingEstimate,
    manifest: &ParallelProofManifest,
    inventory: &TestInventory,
    plan: &ShardPlan,
) -> Result<Sha256Digest, ParallelProofError> {
    protocol_digest(
        "shipyard.canary-adapter.invocation-authority.v1",
        &(policy, timing, manifest, inventory, plan),
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)]
fn run_digest_pinned(
    config: &ParallelProofCanaryAdapterConfig,
    request: &[u8],
) -> Result<Vec<u8>, ParallelProofError> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(&config.executable_path)
        .map_err(|_| ParallelProofError::InvalidField("canary adapter executable"))?;
    let metadata = source.metadata()?;
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.len() == 0
        || metadata.len() > MAX_EXECUTABLE_BYTES
    {
        return Err(ParallelProofError::InvalidField(
            "canary adapter executable",
        ));
    }
    let private_directory = tempfile::tempdir()?;
    fs::set_permissions(private_directory.path(), fs::Permissions::from_mode(0o700))?;
    let snapshot_path = private_directory.path().join("canary-adapter");
    let mut snapshot = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o500)
        .open(&snapshot_path)?;
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; 32 * 1024].into_boxed_slice();
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied = copied.saturating_add(count as u64);
        if copied > MAX_EXECUTABLE_BYTES {
            return Err(ParallelProofError::LimitExceeded {
                field: "canary adapter executable bytes",
                max: usize::try_from(MAX_EXECUTABLE_BYTES).unwrap_or(usize::MAX),
                found: usize::try_from(copied).unwrap_or(usize::MAX),
            });
        }
        hasher.update(&buffer[..count]);
        snapshot.write_all(&buffer[..count])?;
    }
    snapshot.sync_all()?;
    if copied != metadata.len()
        || hex::encode(hasher.finalize()) != config.executable_sha256.as_str()
    {
        return Err(ParallelProofError::BindingMismatch(
            "canary adapter executable digest",
        ));
    }
    drop(snapshot);
    let mut verified = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(&snapshot_path)?;
    let mut verified_hasher = Sha256::new();
    std::io::copy(&mut verified, &mut HashWriter(&mut verified_hasher))?;
    if hex::encode(verified_hasher.finalize()) != config.executable_sha256.as_str() {
        return Err(ParallelProofError::BindingMismatch(
            "canary adapter executable snapshot",
        ));
    }
    verified.seek(SeekFrom::Start(0))?;
    let mut magic = [0_u8; 4];
    verified.read_exact(&mut magic)?;
    let native = magic == [0x7f, b'E', b'L', b'F']
        || matches!(magic, [0xcf, 0xfa, 0xed, 0xfe] | [0xfe, 0xed, 0xfa, 0xcf])
        || matches!(magic, [0xca, 0xfe, 0xba, 0xbe] | [0xbe, 0xba, 0xfe, 0xca]);
    if !native {
        return Err(ParallelProofError::InvalidField(
            "canary adapter must be a native executable image",
        ));
    }
    let pinned = verified.metadata()?;
    let rebound = fs::symlink_metadata(&snapshot_path)?;
    if pinned.dev() != rebound.dev()
        || pinned.ino() != rebound.ino()
        || rebound.file_type().is_symlink()
    {
        return Err(ParallelProofError::BindingMismatch(
            "canary adapter executable snapshot path",
        ));
    }
    // Darwin does not provide a usable fexecve. The snapshot therefore stays
    // descriptor-pinned inside a fresh 0700 directory, with its pathname bound
    // to the verified inode immediately before spawn.
    let mut command = Command::new(&snapshot_path);
    command.env_clear().current_dir("/");
    let output = crate::process::run_output_with_input_until(
        &mut command,
        request,
        Instant::now() + Duration::from_secs(config.deadline_seconds),
        "parallel-proof canary adapter",
    )
    .map_err(|error| ParallelProofError::CorruptRecord(error.to_string()))?;
    if output.stdout.len() as u64 > config.max_stdout_bytes
        || output.stderr.len() as u64 > config.max_stderr_bytes
    {
        return Err(ParallelProofError::LimitExceeded {
            field: "canary adapter output bytes",
            max: usize::try_from(config.max_stdout_bytes.max(config.max_stderr_bytes))
                .unwrap_or(usize::MAX),
            found: output.stdout.len().max(output.stderr.len()),
        });
    }
    if !output.status.success() {
        return Err(ParallelProofError::CorruptRecord(
            "canary adapter exited unsuccessfully".to_owned(),
        ));
    }
    Ok(output.stdout)
}

#[cfg(not(unix))]
fn run_digest_pinned(
    _config: &ParallelProofCanaryAdapterConfig,
    _request: &[u8],
) -> Result<Vec<u8>, ParallelProofError> {
    Err(ParallelProofError::InvalidField(
        "canary adapter exact executable snapshots are unsupported on this platform",
    ))
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

#[cfg(test)]
mod tests;
