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
use crate::parallel_proof_canary_driver::{
    ArtifactDeliveryObservation, DistributedExecutionObservation, ObservedCacheUse,
    PulpMacCanaryDriverOutcome, PulpMacCanaryEvidenceStore, drive_pulp_mac_canary,
};
use crate::parallel_proof_canary_job::{
    ApprovedCanaryJob, ApprovedCanaryOperation, CanaryCancellationPolicy,
    CanaryCancellationRequest, CanaryJobOwner, CanaryJobReceiptState, CanaryJobStore,
    CanaryLogPolicy, CanaryNativeContinuationBinding, CanarySuccessPredicate, CanaryWakePredicate,
};
use crate::parallel_proof_canary_job_adapter::daemon_supports_canary_jobs;

const LEGACY_INVOCATION_SCHEMA: u32 = 1;
const CURRENT_INVOCATION_SCHEMA: u32 = 2;
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

pub(super) fn parallel_proof_canary_worker_command(
    job_id: &str,
    generation: &str,
    config: &LoadedConfig,
    state_dir: &Path,
) -> Result<ExitCode, CliFailure> {
    let process = crate::parallel_proof_canary_job_adapter::verify_canary_worker_authority(
        state_dir, job_id, generation,
    )
    .map_err(|error| CliFailure::new(3, error))?;
    let running_binary =
        std::env::current_exe().map_err(|error| CliFailure::new(3, error.to_string()))?;
    let running_binary_sha256 =
        crate::parallel_proof_canary_job_adapter::executable_digest(&running_binary)
            .map_err(|error| CliFailure::new(3, error))?;
    if running_binary_sha256 != process.executable_sha256 {
        return Err(CliFailure::new(
            3,
            "canary worker running binary digest does not match launch authority",
        ));
    }
    let result = run_parallel_proof_canary_worker(job_id, config, state_dir, &process);
    let (exit_code, artifact) = match result {
        Ok(artifact) => {
            if let Err(error) =
                record_worker_log(state_dir, job_id, b"phase=complete\nstatus=succeeded\n")
            {
                let _ = crate::parallel_proof_canary_job_adapter::record_worker_completion(
                    state_dir, job_id, generation, 1, None,
                );
                return Err(CliFailure::new(1, error));
            }
            (0, Some(artifact))
        }
        Err(error) => {
            let _ = record_worker_log(state_dir, job_id, b"phase=complete\nstatus=failed\n");
            let _ = crate::parallel_proof_canary_job_adapter::record_worker_completion(
                state_dir, job_id, generation, 1, None,
            );
            return Err(error);
        }
    };
    crate::parallel_proof_canary_job_adapter::record_worker_completion(
        state_dir, job_id, generation, exit_code, artifact,
    )
    .map_err(|error| CliFailure::new(1, error))?;
    Ok(ExitCode::SUCCESS)
}

fn record_worker_log(state_dir: &Path, job_id: &str, bytes: &[u8]) -> Result<(), String> {
    let store = CanaryJobStore::open(state_dir.join("parallel-proof-canary").join("jobs"))
        .map_err(|error| error.to_string())?;
    store
        .record_log_segment(job_id, 0, bytes)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[allow(clippy::too_many_lines)]
fn run_parallel_proof_canary_worker(
    job_id: &str,
    config: &LoadedConfig,
    state_dir: &Path,
    process: &crate::parallel_proof_canary_job::CanaryProcessTreeIdentity,
) -> Result<crate::parallel_proof_canary_job::CanaryJobArtifact, CliFailure> {
    let store = CanaryJobStore::open(state_dir.join("parallel-proof-canary").join("jobs"))
        .map_err(|error| canary_failure(&error))?;
    let snapshot = store.load(job_id).map_err(|error| canary_failure(&error))?;
    let input = store
        .load_input(&snapshot.job)
        .map_err(|error| canary_failure(&error))?;
    let invocation: ParallelProofCanaryInvocation = serde_json::from_slice(&input)
        .map_err(|_| CliFailure::new(2, "canary worker input is not strict schema-v1 JSON"))?;
    let proof = ParallelProofContext::new(
        &invocation.manifest,
        &invocation.inventory,
        &invocation.plan,
    )
    .map_err(|error| canary_failure(&error))?;
    let manifest_sha256 = invocation
        .manifest
        .digest(&invocation.inventory, &invocation.plan)
        .map_err(|error| canary_failure(&error))?;
    let invocation_authority_sha256 = canary_invocation_authority_digest(
        &invocation.policy,
        &invocation.timing,
        &invocation.manifest,
        &invocation.inventory,
        &invocation.plan,
    )
    .map_err(|error| canary_failure(&error))?;
    let activation = trusted_parallel_proof_canary_config(config)
        .map_err(|error| CliFailure::new(2, error.to_string()))?
        .ok_or_else(|| CliFailure::new(2, "canary activation disappeared after admission"))?;
    let ApprovedCanaryOperation::ParallelProofDistributedShadow {
        repository_id,
        repository,
        target,
        target_triple,
        builder_host_id,
        worker_host_id,
        manifest_sha256: admitted_manifest,
        request_sha256,
        release_sha256,
        builder_session_generation,
        worker_session_generation,
        cache_authority_sha256,
        storage_authority_sha256,
        artifact_bytes_total,
        invocation_authority_sha256: admitted_invocation,
        adapter_executable_sha256,
        worker_executable_sha256,
    } = &snapshot.job.operation;
    let expected_release = authority_digest(&(
        &invocation.manifest.build,
        &invocation.manifest.artifact,
        &invocation.manifest.trust,
    ))?;
    let expected_cache = authority_digest(&invocation.policy.required_cache_generations)?;
    if snapshot.job.job_id != invocation.custody.job_id
        || snapshot.job.correlation_id != invocation.correlation_id
        || snapshot.job.owner.controller_id != invocation.custody.controller_id
        || snapshot.job.owner.controller_incarnation != invocation.custody.controller_incarnation
        || snapshot.job.owner.approval_sha256 != invocation.custody.approval_sha256
        || snapshot.job.native_continuation != invocation.custody.native_continuation
        || *repository_id != invocation.policy.repository_id
        || *repository != invocation.policy.repository
        || *target != invocation.policy.target
        || *target_triple != invocation.policy.target_triple
        || *builder_host_id != invocation.policy.builder_host_id
        || *worker_host_id != invocation.policy.worker_host_id
        || *admitted_manifest != manifest_sha256
        || *request_sha256 != crate::parallel_proof::Sha256Digest::of_bytes(&input)
        || *release_sha256 != invocation.custody.release_sha256
        || *release_sha256 != expected_release
        || *builder_session_generation != invocation.custody.builder_session_generation
        || *worker_session_generation != invocation.custody.worker_session_generation
        || *cache_authority_sha256 != invocation.custody.cache_authority_sha256
        || *cache_authority_sha256 != expected_cache
        || *storage_authority_sha256 != invocation.custody.storage_authority_sha256
        || *artifact_bytes_total != invocation.manifest.artifact.size_bytes
        || *admitted_invocation != invocation_authority_sha256
        || *adapter_executable_sha256 != activation.adapter.executable_sha256
        || *worker_executable_sha256 != process.executable_sha256
    {
        return Err(CliFailure::new(2, "canary worker custody binding mismatch"));
    }
    let mut executor = ProductionParallelProofCanaryExecutor::new(
        DigestPinnedCanaryProtocolRunner::new(activation.adapter.clone()),
        &activation.adapter,
        proof,
        &invocation.policy,
        invocation.correlation_id.clone(),
    )
    .map_err(|error| canary_failure(&error))?;
    let evidence_store =
        PulpMacCanaryEvidenceStore::open(state_dir.join("parallel-proof-canary").join("evidence"))
            .map_err(|error| canary_failure(&error))?;
    let PulpMacCanaryDriverOutcome::Recorded { evidence, .. } = drive_pulp_mac_canary(
        proof,
        &invocation.policy,
        &invocation.timing,
        invocation.correlation_id,
        &mut executor,
        &evidence_store,
    )
    .map_err(|error| canary_failure(&error))?
    else {
        return Err(CliFailure::new(
            2,
            "canary worker did not produce admitted evidence",
        ));
    };
    let observed_cache_generations = evidence
        .receipt
        .caches
        .iter()
        .map(|cache| cache.generation.clone())
        .collect::<Vec<_>>();
    if evidence.receipt.builder_session_generation != *builder_session_generation
        || evidence.receipt.worker_session_generation != *worker_session_generation
        || authority_digest(&observed_cache_generations)? != *cache_authority_sha256
        || authority_digest(&evidence.pre_execution_host_observations)? != *storage_authority_sha256
    {
        return Err(CliFailure::new(2, "canary worker observed authority drift"));
    }
    let receipt = &evidence.receipt;
    let observation = DistributedExecutionObservation {
        delivery: ArtifactDeliveryObservation {
            mode: receipt.delivery_mode,
            artifact_bytes_total: receipt.artifact_bytes_total,
            artifact_bytes_reused: receipt.artifact_bytes_reused,
            artifact_bytes_transferred: receipt.artifact_bytes_transferred,
            interruption: evidence.interrupted_transfer.clone(),
        },
        setup_ms: receipt.setup_ms,
        transfer_ms: receipt.transfer_ms,
        verification_ms: receipt.verification_ms,
        dispatch_ms: receipt.dispatch_ms,
        shard_execution_ms: receipt.shard_execution_ms,
        worker_active_ms: receipt.worker_active_ms,
        submit_to_receipt_ms: receipt.submit_to_receipt_ms,
        caches: receipt
            .caches
            .iter()
            .map(|cache| ObservedCacheUse {
                generation: cache.generation.clone(),
                usage: cache.usage,
            })
            .collect(),
    };
    store
        .record_artifact(
            job_id,
            &crate::parallel_proof_canary_job::CanaryJobResponse {
                schema_version: snapshot.job.success.artifact_schema_version,
                operation_sha256: snapshot
                    .job
                    .operation
                    .digest()
                    .map_err(|error| canary_failure(&error))?,
                job_sha256: snapshot
                    .job
                    .digest()
                    .map_err(|error| canary_failure(&error))?,
                launch_nonce_sha256: process.launch_nonce_sha256.clone(),
                observation,
            },
        )
        .map_err(|error| canary_failure(&error))
}

fn authority_digest(
    value: &impl Serialize,
) -> Result<crate::parallel_proof::Sha256Digest, CliFailure> {
    serde_json::to_vec(value)
        .map(|bytes| crate::parallel_proof::Sha256Digest::of_bytes(&bytes))
        .map_err(|error| CliFailure::new(2, error.to_string()))
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
    #[serde(default)]
    native_continuation: Option<CanaryNativeContinuationBinding>,
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
    if !matches!(
        invocation.schema_version,
        LEGACY_INVOCATION_SCHEMA | CURRENT_INVOCATION_SCHEMA
    ) {
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
    if invocation.schema_version != CURRENT_INVOCATION_SCHEMA
        || invocation.custody.native_continuation.is_none()
    {
        return Err(CliFailure::new(
            2,
            "parallel-proof canary apply requires schema-v2 native continuation authority",
        ));
    }
    if invocation.custody.release_sha256
        != authority_digest(&(
            &invocation.manifest.build,
            &invocation.manifest.artifact,
            &invocation.manifest.trust,
        ))?
        || invocation.custody.cache_authority_sha256
            != authority_digest(&invocation.policy.required_cache_generations)?
    {
        return Err(CliFailure::new(
            2,
            "parallel-proof canary release or cache authority does not match the request",
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
    let native_continuation = custody
        .native_continuation
        .clone()
        .ok_or_else(|| CliFailure::new(2, "canary native continuation authority is missing"))?;
    let ledger = crate::work_ledger::WorkLedger::open_existing(state_dir)
        .map_err(|error| CliFailure::new(2, error.to_string()))?
        .ok_or_else(|| CliFailure::new(2, "native work ledger is unavailable"))?;
    ledger
        .verify_canary_continuation_binding(&native_continuation)
        .map_err(|error| CliFailure::new(2, error.to_string()))?;
    let worker_executable_sha256 = crate::parallel_proof_canary_job_adapter::executable_digest(
        &std::env::current_exe().map_err(|error| CliFailure::new(1, error.to_string()))?,
    )
    .map_err(|error| CliFailure::new(1, error))?;
    let manifest_sha256 = invocation
        .manifest
        .digest(&invocation.inventory, &invocation.plan)
        .map_err(|error| canary_failure(&error))?;
    let job = ApprovedCanaryJob {
        schema_version: 2,
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
            worker_executable_sha256,
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
        native_continuation: Some(native_continuation),
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
    #[cfg(not(unix))]
    let _ = path;
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
