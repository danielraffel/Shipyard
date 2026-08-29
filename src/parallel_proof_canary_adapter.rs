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
mod tests {
    use std::collections::VecDeque;

    use super::*;
    fn config(path: PathBuf, digest: Sha256Digest) -> ParallelProofCanaryAdapterConfig {
        ParallelProofCanaryAdapterConfig {
            executable_path: path,
            executable_sha256: digest,
            deadline_seconds: 3,
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
            invocation_authority_sha256: Sha256Digest::of_bytes(b"invocation"),
            repository_id: 42,
            repository: "example/project".to_owned(),
            target: "mac".to_owned(),
            target_triple: "aarch64-apple-darwin".to_owned(),
            builder_host_id: "builder".to_owned(),
            worker_host_id: "worker".to_owned(),
        }
    }

    fn authority() -> CanaryAdapterAuthority {
        CanaryAdapterAuthority {
            repository_id: 42,
            repository: "example/project".to_owned(),
            target: "mac".to_owned(),
            target_triple: "aarch64-apple-darwin".to_owned(),
            builder_host_id: "builder".to_owned(),
            worker_host_id: "worker".to_owned(),
            correlation_id: "canary-1".to_owned(),
            manifest_digest: Sha256Digest::of_bytes(b"manifest"),
            invocation_authority_sha256: Sha256Digest::of_bytes(b"invocation"),
        }
    }

    #[derive(Default)]
    struct EchoRunner {
        payloads: VecDeque<serde_json::Value>,
        requests: Vec<serde_json::Value>,
        corrupt_authority: bool,
    }

    impl CanaryProtocolRunner for EchoRunner {
        fn invoke(&mut self, request: &[u8]) -> Result<Vec<u8>, ParallelProofError> {
            let request: serde_json::Value = serde_json::from_slice(request).unwrap();
            let authority = if self.corrupt_authority {
                serde_json::Value::String(Sha256Digest::of_bytes(b"wrong").as_str().to_owned())
            } else {
                request["authority_sha256"].clone()
            };
            let response = serde_json::json!({
                "schema_version": PROTOCOL_SCHEMA,
                "operation": request["operation"].clone(),
                "idempotency_key": request["idempotency_key"].clone(),
                "authority_sha256": authority,
                "payload_sha256": request["payload_sha256"].clone(),
                "result": self.payloads.pop_front().unwrap(),
                "model_calls": 0,
            });
            self.requests.push(request);
            Ok(serde_json::to_vec(&response).unwrap())
        }
    }

    struct UnknownFieldRunner;

    impl CanaryProtocolRunner for UnknownFieldRunner {
        fn invoke(&mut self, request: &[u8]) -> Result<Vec<u8>, ParallelProofError> {
            let request: serde_json::Value = serde_json::from_slice(request).unwrap();
            Ok(serde_json::to_vec(&serde_json::json!({
                "schema_version": PROTOCOL_SCHEMA,
                "operation": request["operation"],
                "idempotency_key": request["idempotency_key"],
                "authority_sha256": request["authority_sha256"],
                "payload_sha256": request["payload_sha256"],
                "result": {"kind":"host_observations","observations":[]},
                "model_calls": 0,
                "unexpected": true
            }))
            .unwrap())
        }
    }

    fn executor(runner: EchoRunner) -> ProductionParallelProofCanaryExecutor<EchoRunner> {
        let authority = authority();
        let authority_sha256 =
            protocol_digest("shipyard.canary-adapter.authority.v1", &authority).unwrap();
        ProductionParallelProofCanaryExecutor {
            runner,
            authority,
            authority_sha256,
            observation_count: 0,
            manifest: None,
            inventory: None,
            plan: None,
            pre_execution_hosts: Vec::new(),
        }
    }

    #[test]
    fn host_observation_phases_are_exact_and_idempotent() {
        let payload = serde_json::json!({"kind":"host_observations","observations":[]});
        let mut first = executor(EchoRunner {
            payloads: VecDeque::from([payload.clone(), payload.clone(), payload.clone()]),
            ..EchoRunner::default()
        });
        assert!(first.authenticated_host_observations().unwrap().is_empty());
        assert!(first.authenticated_host_observations().unwrap().is_empty());
        assert!(first.authenticated_host_observations().unwrap().is_empty());
        assert!(matches!(
            first.authenticated_host_observations(),
            Err(ParallelProofError::InvalidAttemptSequence(_))
        ));
        let operations: Vec<_> = first
            .runner
            .requests
            .iter()
            .map(|request| request["operation"].as_str().unwrap())
            .collect();
        assert_eq!(
            operations,
            [
                "observe_initial_hosts",
                "observe_pre_execution_hosts",
                "observe_final_hosts"
            ]
        );
        let keys: Vec<_> = first
            .runner
            .requests
            .iter()
            .map(|request| request["idempotency_key"].as_str().unwrap())
            .collect();
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[1], keys[2]);

        let mut replay = executor(EchoRunner {
            payloads: VecDeque::from([payload]),
            ..EchoRunner::default()
        });
        replay.authenticated_host_observations().unwrap();
        assert_eq!(
            first.runner.requests[0]["idempotency_key"],
            replay.runner.requests[0]["idempotency_key"]
        );
        assert_eq!(
            first.runner.requests[0]["authority"],
            replay.runner.requests[0]["authority"]
        );
        assert_eq!(first.runner.requests[0]["model_calls"], 0);
    }

    #[test]
    fn response_authority_and_strict_shape_fail_closed() {
        let payload = serde_json::json!({"kind":"host_observations","observations":[]});
        let mut wrong = executor(EchoRunner {
            payloads: VecDeque::from([payload]),
            corrupt_authority: true,
            ..EchoRunner::default()
        });
        assert!(matches!(
            wrong.authenticated_host_observations(),
            Err(ParallelProofError::BindingMismatch(
                "canary adapter response authority"
            ))
        ));

        let authority = authority();
        let authority_sha256 =
            protocol_digest("shipyard.canary-adapter.authority.v1", &authority).unwrap();
        let mut strict = ProductionParallelProofCanaryExecutor {
            runner: UnknownFieldRunner,
            authority,
            authority_sha256,
            observation_count: 0,
            manifest: None,
            inventory: None,
            plan: None,
            pre_execution_hosts: Vec::new(),
        };
        assert!(matches!(
            strict.authenticated_host_observations(),
            Err(ParallelProofError::CorruptRecord(_))
        ));
    }

    #[test]
    fn trusted_config_is_absent_by_default_and_partial_activation_refuses() {
        let global = tempfile::tempdir().unwrap();
        let loaded =
            LoadedConfig::load_machine_global_from_dir(global.path().to_path_buf()).unwrap();
        assert!(
            trusted_parallel_proof_canary_config(&loaded)
                .unwrap()
                .is_none()
        );

        fs::write(
            global.path().join("config.toml"),
            "[parallel_proof_canary]\nactivation_enabled = false\nrepository_id = 42\n",
        )
        .unwrap();
        let loaded =
            LoadedConfig::load_machine_global_from_dir(global.path().to_path_buf()).unwrap();
        assert!(trusted_parallel_proof_canary_config(&loaded).is_err());

        fs::write(
            global.path().join("config.toml"),
            format!(
                "[parallel_proof_canary]\n\
                 activation_enabled = true\n\
                 apply_enabled = true\n\
                 executable_path = \"/usr/bin/true\"\n\
                 executable_sha256 = \"{}\"\n\
                 deadline_seconds = 30\n\
                 max_stdout_bytes = 4096\n\
                 max_stderr_bytes = 4096\n\
                 invocation_authority_sha256 = \"{}\"\n\
                 repository_id = 42\n\
                 repository = \"example/project\"\n\
                 target = \"mac\"\n\
                 target_triple = \"aarch64-apple-darwin\"\n\
                 builder_host_id = \"builder\"\n\
                 worker_host_id = \"worker\"\n",
                "a".repeat(64),
                "b".repeat(64)
            ),
        )
        .unwrap();
        let loaded =
            LoadedConfig::load_machine_global_from_dir(global.path().to_path_buf()).unwrap();
        let activation = trusted_parallel_proof_canary_config(&loaded)
            .unwrap()
            .expect("enabled config");
        assert!(activation.apply_enabled);
        assert_eq!(activation.adapter.repository_id, 42);
        assert_eq!(activation.adapter.builder_host_id, "builder");
    }

    #[cfg(unix)]
    fn executable(root: &Path, name: &str, body: &str) -> (PathBuf, Sha256Digest) {
        use std::os::unix::fs::PermissionsExt as _;
        let source = root.join(format!("{name}.c"));
        let path = root.join(name);
        fs::write(&source, body).unwrap();
        let status = Command::new("cc")
            .args([
                source.as_os_str(),
                std::ffi::OsStr::new("-o"),
                path.as_os_str(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let bytes = fs::read(&path).unwrap();
        (path, Sha256Digest::of_bytes(&bytes))
    }

    #[cfg(unix)]
    #[test]
    fn pinned_runner_rejects_symlink_digest_timeout_and_output_limit() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let (path, digest) = executable(root.path(), "adapter", "int main(void) { return 0; }");
        let link = root.path().join("adapter-link");
        symlink(&path, &link).unwrap();
        let mut linked = DigestPinnedCanaryProtocolRunner::new(config(link, digest.clone()));
        assert!(linked.invoke(b"{}").is_err());

        let mut mismatched = DigestPinnedCanaryProtocolRunner::new(config(
            path.clone(),
            Sha256Digest::of_bytes(b"wrong"),
        ));
        assert!(matches!(
            mismatched.invoke(b"{}"),
            Err(ParallelProofError::BindingMismatch(
                "canary adapter executable digest"
            ))
        ));

        let (loud_path, loud_digest) = executable(
            root.path(),
            "loud",
            "#include <stdio.h>\nint main(void) { fputs(\"12345\", stdout); return 0; }",
        );
        let mut loud_config = config(loud_path, loud_digest);
        loud_config.max_stdout_bytes = 4;
        let mut loud = DigestPinnedCanaryProtocolRunner::new(loud_config);
        let loud_result = loud.invoke(b"{}");
        assert!(
            matches!(
                &loud_result,
                Err(ParallelProofError::LimitExceeded {
                    field: "canary adapter output bytes",
                    ..
                })
            ),
            "{loud_result:?}"
        );

        let (slow_path, slow_digest) = executable(
            root.path(),
            "slow",
            "#include <unistd.h>\nint main(void) { sleep(5); return 0; }",
        );
        let mut slow = DigestPinnedCanaryProtocolRunner::new(config(slow_path, slow_digest));
        assert!(matches!(
            slow.invoke(b"{}"),
            Err(ParallelProofError::CorruptRecord(_))
        ));
    }
}
