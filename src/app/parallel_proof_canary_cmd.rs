use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

use super::CliFailure;
use crate::config::LoadedConfig;
use crate::output::write_pretty_json;
use crate::parallel_proof::{
    ParallelProofContext, ParallelProofManifest, ShardPlan, TestInventory,
};
use crate::parallel_proof_canary::{CanaryTimingEstimate, PulpMacCanaryPolicy};
use crate::parallel_proof_canary_adapter::{
    DigestPinnedCanaryProtocolRunner, ProductionParallelProofCanaryExecutor,
    canary_invocation_authority_digest, trusted_parallel_proof_canary_config,
};
use crate::parallel_proof_canary_job::{
    ApprovedCanaryJob, ApprovedCanaryOperation, CanaryCancellationPolicy,
    CanaryCancellationRequest, CanaryJobOwner, CanaryJobReceiptState, CanaryJobStore,
    CanaryLogPolicy, CanarySuccessPredicate, CanaryWakePredicate,
};
use crate::parallel_proof_canary_job_adapter::daemon_supports_canary_jobs;

const INVOCATION_SCHEMA: u32 = 1;
const MAX_INVOCATION_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParallelProofCanaryInvocation {
    schema_version: u32,
    correlation_id: String,
    policy: PulpMacCanaryPolicy,
    timing: CanaryTimingEstimate,
    manifest: ParallelProofManifest,
    inventory: TestInventory,
    plan: ShardPlan,
    custody: CanaryCustodyAuthority,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanaryCustodyAuthority {
    job_id: String,
    controller_id: String,
    controller_incarnation: String,
    approval_sha256: crate::parallel_proof::Sha256Digest,
    release_sha256: crate::parallel_proof::Sha256Digest,
    builder_session_generation: u64,
    worker_session_generation: u64,
    cache_authority_sha256: crate::parallel_proof::Sha256Digest,
    storage_authority_sha256: crate::parallel_proof::Sha256Digest,
    approved_at_ms: u64,
    deadline_at_ms: u64,
    heartbeat_interval_ms: u64,
    heartbeat_timeout_ms: u64,
    max_heartbeat_receipts: u32,
    cancellation_grace_ms: u64,
    log_segment_bytes: u32,
    max_log_segments: u32,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ParallelProofCanaryCommandOutput {
    Plan {
        correlation_id: String,
        configured: bool,
        apply_enabled: bool,
        scope_matches: bool,
        model_calls: u64,
    },
    Submitted {
        correlation_id: String,
        job_id: String,
        job_sha256: crate::parallel_proof::Sha256Digest,
        write_outcome: &'static str,
        model_calls: u64,
    },
    JobStatus {
        job_id: String,
        job_sha256: crate::parallel_proof::Sha256Digest,
        sequence: u32,
        state: CanaryJobReceiptState,
        model_calls: u64,
    },
    CancellationRequested {
        job_id: String,
        write_outcome: &'static str,
        model_calls: u64,
    },
}

#[allow(clippy::too_many_lines)]
pub(super) fn parallel_proof_canary_command<W: std::io::Write>(
    request_path: Option<&Path>,
    apply: bool,
    status: Option<&str>,
    cancel: Option<&str>,
    config: &LoadedConfig,
    state_dir: &Path,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let jobs_root = state_dir.join("parallel-proof-canary").join("jobs");
    if let Some(job_id) = status {
        let snapshot = CanaryJobStore::load_read_only(jobs_root, job_id)
            .map_err(|error| canary_failure(&error))?;
        let latest = snapshot.latest();
        write_pretty_json(
            stdout,
            &ParallelProofCanaryCommandOutput::JobStatus {
                job_id: job_id.to_owned(),
                job_sha256: snapshot
                    .job
                    .digest()
                    .map_err(|error| canary_failure(&error))?,
                sequence: latest.sequence,
                state: latest.receipt.clone(),
                model_calls: 0,
            },
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ExitCode::SUCCESS);
    }
    let request_path = request_path.ok_or_else(|| CliFailure::new(2, "--request is required"))?;
    let (invocation, invocation_bytes) = read_invocation(request_path)?;
    let proof = ParallelProofContext::new(
        &invocation.manifest,
        &invocation.inventory,
        &invocation.plan,
    )
    .map_err(|error| canary_failure(&error))?;
    if invocation.schema_version != INVOCATION_SCHEMA {
        return Err(CliFailure::new(2, "unsupported canary invocation schema"));
    }
    if let Some(job_id) = cancel {
        if job_id != invocation.custody.job_id {
            return Err(CliFailure::new(
                2,
                "cancel job does not match request custody",
            ));
        }
        let store = CanaryJobStore::open(jobs_root).map_err(|error| canary_failure(&error))?;
        let snapshot = store.load(job_id).map_err(|error| canary_failure(&error))?;
        let ApprovedCanaryOperation::ParallelProofDistributedShadow { request_sha256, .. } =
            &snapshot.job.operation;
        if *request_sha256 != crate::parallel_proof::Sha256Digest::of_bytes(&invocation_bytes)
            || snapshot.job.owner.controller_id != invocation.custody.controller_id
            || snapshot.job.owner.approval_sha256 != invocation.custody.approval_sha256
        {
            return Err(CliFailure::new(
                2,
                "cancel request does not authenticate job custody",
            ));
        }
        let requested_at_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| CliFailure::new(1, "controller clock is before epoch"))?
                .as_millis(),
        )
        .map_err(|_| CliFailure::new(1, "controller clock exceeds supported range"))?;
        let outcome = store
            .request_cancel(
                job_id,
                &CanaryCancellationRequest {
                    job_sha256: snapshot
                        .job
                        .digest()
                        .map_err(|error| canary_failure(&error))?,
                    controller_id: invocation.custody.controller_id,
                    approval_sha256: invocation.custody.approval_sha256,
                    requested_at_ms,
                },
            )
            .map_err(|error| canary_failure(&error))?;
        write_pretty_json(
            stdout,
            &ParallelProofCanaryCommandOutput::CancellationRequested {
                job_id: job_id.to_owned(),
                write_outcome: match outcome {
                    crate::parallel_proof::StoreWriteOutcome::Created => "created",
                    crate::parallel_proof::StoreWriteOutcome::AlreadyPresent => "already_present",
                },
                model_calls: 0,
            },
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ExitCode::SUCCESS);
    }
    let activation = trusted_parallel_proof_canary_config(config)
        .map_err(|error| CliFailure::new(2, error.to_string()))?;
    let invocation_authority_sha256 = canary_invocation_authority_digest(
        &invocation.policy,
        &invocation.timing,
        &invocation.manifest,
        &invocation.inventory,
        &invocation.plan,
    )
    .map_err(|error| canary_failure(&error))?;
    let scope_matches = activation.as_ref().is_some_and(|activation| {
        let adapter = &activation.adapter;
        adapter.repository_id == invocation.policy.repository_id
            && adapter.repository == invocation.policy.repository
            && adapter.target == invocation.policy.target
            && adapter.target_triple == invocation.policy.target_triple
            && adapter.builder_host_id == invocation.policy.builder_host_id
            && adapter.worker_host_id == invocation.policy.worker_host_id
            && invocation.manifest.source.repository_id == adapter.repository_id
            && invocation.manifest.source.repository == adapter.repository
            && invocation.manifest.build.target_triple == adapter.target_triple
            && adapter.invocation_authority_sha256 == invocation_authority_sha256
    });

    if !apply {
        write_pretty_json(
            stdout,
            &ParallelProofCanaryCommandOutput::Plan {
                correlation_id: invocation.correlation_id,
                configured: activation.is_some(),
                apply_enabled: activation
                    .as_ref()
                    .is_some_and(|activation| activation.apply_enabled),
                scope_matches,
                model_calls: 0,
            },
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ExitCode::SUCCESS);
    }

    let activation = activation.ok_or_else(|| {
        CliFailure::new(
            2,
            "parallel-proof canary apply is disabled in trusted machine-global policy",
        )
    })?;
    if !activation.apply_enabled
        || !scope_matches
        || !invocation.policy.enabled
        || invocation.policy.assessed_at_ms != 0
    {
        return Err(CliFailure::new(
            2,
            "parallel-proof canary apply authority does not match the exact request",
        ));
    }
    let daemon_status = crate::daemon_ipc::read_daemon_status(state_dir).ok_or_else(|| {
        CliFailure::new(
            3,
            "parallel-proof canary requires a live daemon-owned typed worker lane",
        )
    })?;
    if !daemon_supports_canary_jobs(&daemon_status) {
        return Err(CliFailure::new(
            3,
            "running daemon lacks parallel_proof_canary_job_v1; refresh after installing the integrated binary",
        ));
    }
    // Validate that the exact typed production driver can consume this proof
    // and activation before custody is published. Construction performs no I/O.
    let _worker_executor = ProductionParallelProofCanaryExecutor::new(
        DigestPinnedCanaryProtocolRunner::new(activation.adapter.clone()),
        &activation.adapter,
        proof,
        &invocation.policy,
        invocation.correlation_id.clone(),
    )
    .map_err(|error| canary_failure(&error))?;
    let store_parent = state_dir.join("parallel-proof-canary");
    crate::writer_domain_lease::ensure_protected_dir_all(&store_parent)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let custody = &invocation.custody;
    let manifest_sha256 = invocation
        .manifest
        .digest(&invocation.inventory, &invocation.plan)
        .map_err(|error| canary_failure(&error))?;
    let job = ApprovedCanaryJob {
        schema_version: 1,
        job_id: custody.job_id.clone(),
        correlation_id: invocation.correlation_id.clone(),
        owner: CanaryJobOwner {
            controller_id: custody.controller_id.clone(),
            controller_incarnation: custody.controller_incarnation.clone(),
            approval_sha256: custody.approval_sha256.clone(),
        },
        operation: ApprovedCanaryOperation::ParallelProofDistributedShadow {
            repository_id: invocation.policy.repository_id,
            repository: invocation.policy.repository.clone(),
            target: invocation.policy.target.clone(),
            target_triple: invocation.policy.target_triple.clone(),
            builder_host_id: invocation.policy.builder_host_id.clone(),
            worker_host_id: invocation.policy.worker_host_id.clone(),
            manifest_sha256,
            request_sha256: crate::parallel_proof::Sha256Digest::of_bytes(&invocation_bytes),
            release_sha256: custody.release_sha256.clone(),
            builder_session_generation: custody.builder_session_generation,
            worker_session_generation: custody.worker_session_generation,
            cache_authority_sha256: custody.cache_authority_sha256.clone(),
            storage_authority_sha256: custody.storage_authority_sha256.clone(),
            artifact_bytes_total: invocation.manifest.artifact.size_bytes,
            invocation_authority_sha256,
            adapter_executable_sha256: activation.adapter.executable_sha256.clone(),
        },
        approved_at_ms: custody.approved_at_ms,
        deadline_at_ms: custody.deadline_at_ms,
        heartbeat_interval_ms: custody.heartbeat_interval_ms,
        heartbeat_timeout_ms: custody.heartbeat_timeout_ms,
        max_heartbeat_receipts: custody.max_heartbeat_receipts,
        success: CanarySuccessPredicate {
            required_exit_code: 0,
            artifact_schema_version: 1,
            max_artifact_bytes: 1024 * 1024,
        },
        cancellation: CanaryCancellationPolicy {
            grace_ms: custody.cancellation_grace_ms,
            cancel_at_deadline: true,
        },
        wake: CanaryWakePredicate {
            on_success: true,
            on_actionable_failure: true,
        },
        logs: CanaryLogPolicy {
            segment_bytes: custody.log_segment_bytes,
            max_segments: custody.max_log_segments,
        },
    };
    let store =
        CanaryJobStore::open(store_parent.join("jobs")).map_err(|error| canary_failure(&error))?;
    store
        .record_input(&job, &invocation_bytes)
        .map_err(|error| canary_failure(&error))?;
    let write_outcome = store.submit(&job).map_err(|error| canary_failure(&error))?;
    let output = ParallelProofCanaryCommandOutput::Submitted {
        correlation_id: invocation.correlation_id,
        job_id: job.job_id.clone(),
        job_sha256: job.digest().map_err(|error| canary_failure(&error))?,
        write_outcome: match write_outcome {
            crate::parallel_proof::StoreWriteOutcome::Created => "created",
            crate::parallel_proof::StoreWriteOutcome::AlreadyPresent => "already_present",
        },
        model_calls: 0,
    };
    write_pretty_json(stdout, &output).map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

fn read_invocation(path: &Path) -> Result<(ParallelProofCanaryInvocation, Vec<u8>), CliFailure> {
    if !bounded_normalized_absolute_path(path) {
        return Err(CliFailure::new(
            2,
            "--request must be a normalized absolute path",
        ));
    }
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(path)
            .map_err(|_| CliFailure::new(2, "canary request cannot be opened no-follow"))?
    };
    #[cfg(not(unix))]
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| CliFailure::new(2, "canary request cannot be opened"))?;
    let initial = validate_request_file(path, &file)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_INVOCATION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| CliFailure::new(2, error.to_string()))?;
    if bytes.len() as u64 > MAX_INVOCATION_BYTES {
        return Err(CliFailure::new(2, "canary request exceeds its byte limit"));
    }
    let middle = validate_request_file(path, &file)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| CliFailure::new(2, error.to_string()))?;
    let mut repeated = Vec::new();
    (&mut file)
        .take(MAX_INVOCATION_BYTES + 1)
        .read_to_end(&mut repeated)
        .map_err(|error| CliFailure::new(2, error.to_string()))?;
    let final_identity = validate_request_file(path, &file)?;
    if initial != middle || middle != final_identity || bytes != repeated {
        return Err(CliFailure::new(
            2,
            "canary request changed while being read",
        ));
    }
    let invocation = serde_json::from_slice(&repeated)
        .map_err(|_| CliFailure::new(2, "canary request is not strict schema-v1 JSON"))?;
    Ok((invocation, repeated))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequestFileIdentity {
    len: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

fn validate_request_file(
    path: &Path,
    file: &std::fs::File,
) -> Result<RequestFileIdentity, CliFailure> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file
        .metadata()
        .map_err(|error| CliFailure::new(2, error.to_string()))?;
    if !metadata.is_file() || metadata.len() > MAX_INVOCATION_BYTES {
        return Err(CliFailure::new(
            2,
            "canary request must be a bounded regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let bound = std::fs::symlink_metadata(path)
            .map_err(|error| CliFailure::new(2, error.to_string()))?;
        if metadata.permissions().mode() & 0o077 != 0
            || metadata.nlink() != 1
            || metadata.uid() != nix::unistd::Uid::effective().as_raw()
            || metadata.dev() != bound.dev()
            || metadata.ino() != bound.ino()
        {
            return Err(CliFailure::new(
                2,
                "canary request must be private and descriptor-bound",
            ));
        }
    }
    Ok(RequestFileIdentity {
        len: metadata.len(),
        #[cfg(unix)]
        dev: metadata.dev(),
        #[cfg(unix)]
        ino: metadata.ino(),
        #[cfg(unix)]
        modified_seconds: metadata.mtime(),
        #[cfg(unix)]
        modified_nanoseconds: metadata.mtime_nsec(),
        #[cfg(unix)]
        changed_seconds: metadata.ctime(),
        #[cfg(unix)]
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

fn bounded_normalized_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= 4096
        && path.components().collect::<PathBuf>() == path
        && !path
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
}

fn canary_failure(error: &crate::parallel_proof::ParallelProofError) -> CliFailure {
    CliFailure::new(1, error.to_string())
}
