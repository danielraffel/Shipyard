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
    PulpMacCanaryDriverOutcome, PulpMacCanaryEvidenceStore, drive_pulp_mac_canary,
};

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
    Disabled {
        correlation_id: String,
        model_calls: u64,
    },
    Ineligible {
        correlation_id: String,
        decision: crate::parallel_proof_canary::PulpMacCanaryDecision,
        model_calls: u64,
    },
    Recorded {
        correlation_id: String,
        receipt_sha256: crate::parallel_proof::Sha256Digest,
        write_outcome: &'static str,
        model_calls: u64,
    },
}

#[allow(clippy::too_many_lines)]
pub(super) fn parallel_proof_canary_command<W: std::io::Write>(
    request_path: &Path,
    apply: bool,
    config: &LoadedConfig,
    state_dir: &Path,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let invocation = read_invocation(request_path)?;
    let proof = ParallelProofContext::new(
        &invocation.manifest,
        &invocation.inventory,
        &invocation.plan,
    )
    .map_err(|error| canary_failure(&error))?;
    if invocation.schema_version != INVOCATION_SCHEMA {
        return Err(CliFailure::new(2, "unsupported canary invocation schema"));
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
    let runner = DigestPinnedCanaryProtocolRunner::new(activation.adapter.clone());
    let mut executor = ProductionParallelProofCanaryExecutor::new(
        runner,
        &activation.adapter,
        proof,
        &invocation.policy,
        invocation.correlation_id.clone(),
    )
    .map_err(|error| canary_failure(&error))?;
    let store_parent = state_dir.join("parallel-proof-canary");
    crate::writer_domain_lease::ensure_protected_dir_all(&store_parent)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let store = PulpMacCanaryEvidenceStore::open(store_parent.join("evidence"))
        .map_err(|error| canary_failure(&error))?;
    let outcome = drive_pulp_mac_canary(
        proof,
        &invocation.policy,
        &invocation.timing,
        invocation.correlation_id.clone(),
        &mut executor,
        &store,
    )
    .map_err(|error| canary_failure(&error))?;
    let output = match outcome {
        PulpMacCanaryDriverOutcome::Disabled => ParallelProofCanaryCommandOutput::Disabled {
            correlation_id: invocation.correlation_id,
            model_calls: 0,
        },
        PulpMacCanaryDriverOutcome::Ineligible(decision) => {
            ParallelProofCanaryCommandOutput::Ineligible {
                correlation_id: invocation.correlation_id,
                decision,
                model_calls: 0,
            }
        }
        PulpMacCanaryDriverOutcome::Recorded {
            evidence,
            write_outcome,
        } => ParallelProofCanaryCommandOutput::Recorded {
            correlation_id: invocation.correlation_id,
            receipt_sha256: evidence.receipt_sha256.clone(),
            write_outcome: match write_outcome {
                crate::parallel_proof::StoreWriteOutcome::Created => "created",
                crate::parallel_proof::StoreWriteOutcome::AlreadyPresent => "already_present",
            },
            model_calls: 0,
        },
    };
    write_pretty_json(stdout, &output).map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

fn read_invocation(path: &Path) -> Result<ParallelProofCanaryInvocation, CliFailure> {
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
    serde_json::from_slice(&repeated)
        .map_err(|_| CliFailure::new(2, "canary request is not strict schema-v1 JSON"))
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
