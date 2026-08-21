use super::{
    CliFailure, File, Path, PathBuf, RecoveryRequest, RecoveryStore, RecoveryWitness, Write,
    acquire_recovery_enqueue_lease, fs, recovery_lease_deadline, recovery_store_root,
};
use sha2::{Digest, Sha256};

pub(super) fn recovery_witness_path(state_dir: &Path, repo: &str, pr: u64) -> PathBuf {
    let key = format!("{}#{pr}", repo.to_ascii_lowercase());
    recovery_store_root(state_dir)
        .join("witnesses")
        .join(format!("{:x}.json", Sha256::digest(key.as_bytes())))
}

pub(super) fn write_recovery_witness(
    state_dir: &Path,
    repo: &str,
    pr: u64,
    request_id: &str,
    head_sha: &str,
    policy_signature: &str,
    failure_fingerprint: &str,
) -> Result<(), CliFailure> {
    let path = recovery_witness_path(state_dir, repo, pr);
    let parent = path
        .parent()
        .ok_or_else(|| CliFailure::new(1, "recovery witness path has no parent"))?;
    fs::create_dir_all(parent).map_err(|error| {
        CliFailure::new(
            1,
            format!("failed to create recovery witness directory: {error}"),
        )
    })?;
    let witness = RecoveryWitness {
        request_id: request_id.to_owned(),
        head_sha: head_sha.to_ascii_lowercase(),
        policy_signature: policy_signature.to_owned(),
        failure_fingerprint: failure_fingerprint.to_owned(),
        updated_at: chrono::Utc::now(),
    };
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        CliFailure::new(1, format!("failed to create recovery witness: {error}"))
    })?;
    serde_json::to_writer(&mut temporary, &witness).map_err(|error| {
        CliFailure::new(1, format!("failed to encode recovery witness: {error}"))
    })?;
    temporary
        .write_all(b"\n")
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| {
            CliFailure::new(1, format!("failed to flush recovery witness: {error}"))
        })?;
    temporary.persist(&path).map_err(|error| {
        CliFailure::new(
            1,
            format!("failed to persist recovery witness: {}", error.error),
        )
    })?;
    sync_directory(parent)?;
    Ok(())
}

pub(super) fn remove_recovery_witness(
    state_dir: &Path,
    repo: &str,
    pr: u64,
) -> Result<(), CliFailure> {
    let path = recovery_witness_path(state_dir, repo, pr);
    match fs::remove_file(&path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CliFailure::new(
            1,
            format!(
                "failed to remove recovery witness {}: {error}",
                path.display()
            ),
        )),
    }
}

fn remove_recovery_witness_for_head(
    state_dir: &Path,
    repo: &str,
    pr: u64,
    head_sha: &str,
) -> Result<(), CliFailure> {
    let path = recovery_witness_path(state_dir, repo, pr);
    let payload = match fs::read(&path) {
        Ok(payload) => payload,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!(
                    "failed to read recovery witness {}: {error}",
                    path.display()
                ),
            ));
        }
    };
    let witness = serde_json::from_slice::<RecoveryWitness>(&payload).map_err(|error| {
        CliFailure::new(
            1,
            format!("malformed deterministic recovery witness: {error}"),
        )
    })?;
    if !witness.head_sha.eq_ignore_ascii_case(head_sha) {
        return Ok(());
    }
    remove_recovery_witness(state_dir, repo, pr)
}

#[cfg(test)]
pub(in crate::app::merge_steward_cmd) fn has_recovery_witness(
    state_dir: &Path,
    repo: &str,
    pr: u64,
) -> Result<bool, String> {
    let path = recovery_witness_path(state_dir, repo, pr);
    match path.try_exists() {
        Ok(exists) => Ok(exists),
        Err(error) => Err(format!(
            "failed to inspect recovery witness {}: {error}",
            path.display()
        )),
    }
}

pub(in crate::app::merge_steward_cmd) fn with_recovery_clear_fence<T>(
    state_dir: &Path,
    repo: &str,
    pr: u64,
    head_sha: &str,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    fence_recovery_state(state_dir, repo, pr, head_sha)?;
    let operation_result = operation();
    let final_fence = fence_recovery_state(state_dir, repo, pr, head_sha);
    match (operation_result, final_fence) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(operation_error), Err(fence_error)) => Err(format!(
            "{operation_error}; final recovery clear fence failed: {fence_error}"
        )),
    }
}

fn fence_recovery_state(
    state_dir: &Path,
    repo: &str,
    pr: u64,
    head_sha: &str,
) -> Result<(), String> {
    let store = RecoveryStore::new(recovery_store_root(state_dir))
        .map_err(|error| format!("failed to open recovery store for clear fence: {error}"))?;
    let _lease = acquire_recovery_enqueue_lease(store.root(), recovery_lease_deadline()).map_err(
        |error| {
            format!(
                "failed to acquire recovery clear fence: {}",
                error.message()
            )
        },
    )?;
    store
        .supersede_active_target(
            repo,
            pr,
            head_sha,
            "deterministic stewardship cleared the exact-head recovery signal",
        )
        .map_err(|error| format!("failed to supersede cleared recovery work: {error}"))?;
    remove_recovery_witness_for_head(state_dir, repo, pr, head_sha).map_err(|error| {
        format!(
            "failed to invalidate cleared recovery witness: {}",
            error.message()
        )
    })?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), CliFailure> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to durably sync recovery witness directory: {error}"),
            )
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), CliFailure> {
    Ok(())
}

pub(super) fn verify_recovery_witness(
    state_dir: &Path,
    request: &RecoveryRequest,
) -> Result<(), CliFailure> {
    let path = recovery_witness_path(state_dir, &request.repo, request.pr);
    let payload = fs::read(&path).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "missing current deterministic recovery witness {}: {error}",
                path.display()
            ),
        )
    })?;
    let witness = serde_json::from_slice::<RecoveryWitness>(&payload).map_err(|error| {
        CliFailure::new(
            1,
            format!("malformed deterministic recovery witness: {error}"),
        )
    })?;
    if witness.request_id != request.id
        || !witness.head_sha.eq_ignore_ascii_case(&request.head_sha)
        || witness.policy_signature != request.policy_signature
        || witness.failure_fingerprint != request.failure_fingerprint
    {
        return Err(CliFailure::new(
            1,
            "deterministic recovery evidence or steward policy drifted",
        ));
    }
    Ok(())
}
