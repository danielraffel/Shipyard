use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::github::remaining_timeout;
use super::{
    CliFailure, MAX_RECEIPT_DETAIL_BYTES, RecoveryRequest, RecoveryStatus, RecoveryStore,
    TERMINAL_PERSIST_TIMEOUT_SECONDS,
};

/// Token proving that a durable attempt has been claimed.
///
/// All fallible post-claim work runs through [`Self::run`], which turns every
/// error into a terminal failure receipt. Callers cannot accidentally scatter
/// branch-specific terminalization across the recovery state transition.
pub(super) struct ClaimedRecovery<'a> {
    store: &'a RecoveryStore,
    request: &'a RecoveryRequest,
    policy_signature: &'a str,
}

impl<'a> ClaimedRecovery<'a> {
    pub(super) fn begin(
        store: &'a RecoveryStore,
        request: &'a RecoveryRequest,
        policy_signature: &'a str,
        worker_generation: &str,
    ) -> Result<Self, CliFailure> {
        store
            .begin(&request.id, policy_signature, worker_generation)
            .map_err(|error| {
                recover_failed_begin(
                    store,
                    request,
                    policy_signature,
                    worker_generation,
                    format!("failed to claim recovery request {}: {error}", request.id),
                )
            })?;
        Ok(Self {
            store,
            request,
            policy_signature,
        })
    }

    pub(super) fn run<T>(
        self,
        operation: impl FnOnce(&Self) -> Result<T, CliFailure>,
    ) -> Result<T, CliFailure> {
        operation(&self).map_err(|error| {
            fail_after_claim(
                self.store,
                self.request,
                self.policy_signature,
                error.message(),
            )
        })
    }

    pub(super) fn worker_deadline(
        &self,
        record_deadline: Instant,
        timeout_seconds: u64,
    ) -> Result<Instant, CliFailure> {
        remaining_timeout(record_deadline, "recovery model execution")
            .map_err(|error| {
                CliFailure::new(
                    1,
                    format!(
                        "claimed recovery request {}: {}",
                        self.request.id,
                        error.message()
                    ),
                )
            })
            .map(|_| {
                let policy_deadline = Instant::now() + Duration::from_secs(timeout_seconds);
                record_deadline.min(policy_deadline)
            })
    }
}

pub(super) fn worker_generation(scratch_dir: &Path) -> Result<String, CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(scratch_dir)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::create_dir_all(scratch_dir).map_err(|error| {
        CliFailure::new(
            1,
            format!("failed to create worker scratch directory: {error}"),
        )
    })?;
    let marker = tempfile::NamedTempFile::new_in(scratch_dir).map_err(|error| {
        CliFailure::new(1, format!("failed to generate worker identity: {error}"))
    })?;
    let random_name = marker
        .path()
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| CliFailure::new(1, "worker identity path is not UTF-8"))?;
    Ok(format!(
        "{}-{}",
        std::process::id(),
        hex::encode(&Sha256::digest(random_name.as_bytes())[..12])
    ))
}

pub(super) fn fail_after_claim(
    store: &RecoveryStore,
    request: &RecoveryRequest,
    policy_signature: &str,
    detail: impl AsRef<str>,
) -> CliFailure {
    let detail = bounded_detail(detail.as_ref(), MAX_RECEIPT_DETAIL_BYTES);
    // The record-wide deadline bounds GitHub/model work, but terminal
    // persistence needs its own small budget. Otherwise an expired model
    // deadline or prior store-lock timeout can strand a spent claim in Running.
    let terminal_store = store
        .clone()
        .with_lock_deadline(Instant::now() + Duration::from_secs(TERMINAL_PERSIST_TIMEOUT_SECONDS));
    match terminal_store.fail(&request.id, policy_signature, &detail) {
        Ok(_) => CliFailure::new(1, detail),
        Err(error) => CliFailure::new(
            1,
            format!("{detail}; terminal failure receipt could not be persisted: {error}"),
        ),
    }
}

pub(super) fn recover_failed_begin(
    store: &RecoveryStore,
    request: &RecoveryRequest,
    policy_signature: &str,
    worker_generation: &str,
    detail: impl AsRef<str>,
) -> CliFailure {
    let detail = bounded_detail(detail.as_ref(), MAX_RECEIPT_DETAIL_BYTES);
    // `begin` first persists a no-clobber claim marker, then materializes the
    // Running JSON record. A failure in that second write has already spent the
    // attempt. Reload through a fresh deadline so the marker is applied and the
    // spent attempt is terminalized instead of being reported as pre-claim.
    let terminal_store = store
        .clone()
        .with_lock_deadline(Instant::now() + Duration::from_secs(TERMINAL_PERSIST_TIMEOUT_SECONDS));
    match terminal_store.get(&request.id) {
        Ok(Some(record))
            if record.receipt.status == RecoveryStatus::Running
                && record.receipt.worker_generation.as_deref() == Some(worker_generation) =>
        {
            fail_after_claim(&terminal_store, request, policy_signature, &detail)
        }
        Ok(_) => CliFailure::new(1, detail),
        Err(error) => CliFailure::new(
            1,
            format!("{detail}; failed to reload possible durable claim marker: {error}"),
        ),
    }
}

pub(super) fn process_failure_detail(exit_code: Option<i32>, stderr: &[u8]) -> String {
    let tail = String::from_utf8_lossy(stderr);
    let tail = tail.trim();
    let prefix = exit_code.map_or_else(
        || "worker exited by signal".to_owned(),
        |code| format!("worker exited with status {code}"),
    );
    if tail.is_empty() {
        prefix
    } else {
        bounded_detail(&format!("{prefix}: {tail}"), MAX_RECEIPT_DETAIL_BYTES)
    }
}

pub(super) fn bounded_detail(detail: &str, max_bytes: usize) -> String {
    const PREFIX: &str = "...[truncated] ";
    let sanitized = detail
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    if sanitized.len() <= max_bytes {
        return sanitized;
    }
    let tail_budget = max_bytes.saturating_sub(PREFIX.len());
    let mut start = sanitized.len().saturating_sub(tail_budget);
    while !sanitized.is_char_boundary(start) {
        start += 1;
    }
    format!("{PREFIX}{}", &sanitized[start..])
}
