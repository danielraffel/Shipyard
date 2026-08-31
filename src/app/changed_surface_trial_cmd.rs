//! Read-only command boundary for changed-surface shadow trial receipts.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::CliFailure;
use crate::changed_surface::trial::{
    ReceiptFile, TrialIdentity, TrialState, TrialStatus, evaluate_stale_base_execution,
    evaluate_stale_base_terminal, evaluate_trial, rejected_trial, result_directory,
};
use crate::output::write_json_envelope;

const ACTIVATION_RECEIPT: &str = "activation-shadow_compare.json";
const STALE_ACTIVATION_RECEIPT: &str = "stale-activation-shadow_compare.json";
const STALE_CLEANUP_RECEIPT: &str = "stale-cleanup-shadow_compare.json";
const STALE_CURRENT_RECEIPT: &str = "stale-current.json";
const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;

pub(super) struct ChangedSurfaceTrialStatusArgs {
    pub(super) repository: String,
    pub(super) pull_request: u64,
    pub(super) target: String,
    pub(super) head_sha: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentStaleGeneration {
    schema_version: u32,
    repository: String,
    pull_request: u64,
    target: String,
    head_sha: String,
    live_base_sha: String,
    context_digest: String,
    stale_receipt_sha256: String,
}

struct SelectedStaleGeneration {
    pointer_bytes: Vec<u8>,
    result_dir: PathBuf,
    stale_receipt_sha256: String,
}

#[derive(Clone, Copy)]
struct StaleTrialInputs<'a> {
    ordinary_evidence_present: bool,
    activation: Option<&'a [u8]>,
    cleanup: Option<&'a [u8]>,
    results: &'a [(String, Vec<u8>)],
    expected_receipt_sha256: Option<&'a str>,
}

pub(super) fn changed_surface_trial_status_command<W: Write>(
    args: &ChangedSurfaceTrialStatusArgs,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let identity = TrialIdentity {
        repository: args.repository.clone(),
        pull_request: args.pull_request,
        target: args.target.clone(),
        head_sha: args.head_sha.clone(),
    };
    let result_dir = result_directory(state_dir, &identity);
    let status = read_trial(&identity, &result_dir);
    emit_status(&status, &result_dir, json, stdout)?;
    Ok(match (status.state, status.shadow_disposition) {
        (
            TrialState::Terminal,
            Some(crate::changed_surface::StaleBaseShadowDisposition::Invalidated),
        )
        | (TrialState::Rejected, _) => ExitCode::from(1),
        (TrialState::Ready | TrialState::Terminal, _) => ExitCode::SUCCESS,
        (TrialState::Collecting, _) => ExitCode::from(3),
    })
}

fn read_trial(identity: &TrialIdentity, result_dir: &Path) -> TrialStatus {
    read_trial_with_final_snapshot_hook(identity, result_dir, || {})
}

#[allow(clippy::too_many_lines)]
fn read_trial_with_final_snapshot_hook<F>(
    identity: &TrialIdentity,
    result_root: &Path,
    mut before_final_snapshot: F,
) -> TrialStatus
where
    F: FnMut(),
{
    let current_generation = match select_current_stale_generation(identity, result_root) {
        Ok(selected) => selected,
        Err(reason) => {
            return rejected_trial(
                identity,
                Some(STALE_CURRENT_RECEIPT.to_owned()),
                0,
                None,
                reason,
            );
        }
    };
    let result_dir = current_generation
        .as_ref()
        .map_or(result_root, |generation| generation.result_dir.as_path());
    let activation_path = result_dir.join(ACTIVATION_RECEIPT);
    let activation_bytes = match read_regular_receipt(&activation_path) {
        Ok(bytes) => bytes,
        Err(reason) => {
            return rejected_trial(
                identity,
                Some(ACTIVATION_RECEIPT.to_owned()),
                0,
                None,
                reason,
            );
        }
    };
    let stale_activation_bytes =
        match read_regular_receipt(&result_dir.join(STALE_ACTIVATION_RECEIPT)) {
            Ok(bytes) => bytes,
            Err(reason) => {
                return rejected_trial(
                    identity,
                    Some(STALE_ACTIVATION_RECEIPT.to_owned()),
                    0,
                    None,
                    reason,
                );
            }
        };
    let stale_cleanup_bytes = match read_regular_receipt(&result_dir.join(STALE_CLEANUP_RECEIPT)) {
        Ok(bytes) => bytes,
        Err(reason) => {
            return rejected_trial(
                identity,
                Some(STALE_CLEANUP_RECEIPT.to_owned()),
                0,
                None,
                reason,
            );
        }
    };
    let results = match read_result_receipts(result_dir) {
        Ok(results) => results,
        Err(failure) => {
            return rejected_trial(
                identity,
                activation_bytes
                    .as_ref()
                    .map(|_| ACTIVATION_RECEIPT.to_owned()),
                failure.observed,
                failure.receipt,
                failure.reason,
            );
        }
    };
    if let Some(status) = read_stale_trial(
        identity,
        result_dir,
        StaleTrialInputs {
            ordinary_evidence_present: activation_bytes.is_some(),
            activation: stale_activation_bytes.as_deref(),
            cleanup: stale_cleanup_bytes.as_deref(),
            results: &results,
            expected_receipt_sha256: current_generation
                .as_ref()
                .map(|generation| generation.stale_receipt_sha256.as_str()),
        },
        &mut before_final_snapshot,
    ) {
        if current_generation.as_ref().is_some_and(|generation| {
            read_regular_receipt(&result_root.join(STALE_CURRENT_RECEIPT))
                != Ok(Some(generation.pointer_bytes.clone()))
        }) {
            return rejected_trial(
                identity,
                Some(STALE_CURRENT_RECEIPT.to_owned()),
                status.result_receipt_count,
                status.result_receipt,
                "stale_generation_changed_during_read",
            );
        }
        return status;
    }
    let activation = activation_bytes.as_deref().map(|bytes| ReceiptFile {
        name: ACTIVATION_RECEIPT,
        bytes,
    });
    let result_files = results
        .iter()
        .map(|(name, bytes)| ReceiptFile { name, bytes })
        .collect::<Vec<_>>();
    let status = evaluate_trial(identity, activation, &result_files);
    if status.state != TrialState::Ready {
        return status;
    }

    before_final_snapshot();
    let final_activation = match read_regular_receipt(&activation_path) {
        Ok(bytes) => bytes,
        Err(reason) => {
            return rejected_trial(
                identity,
                Some(ACTIVATION_RECEIPT.to_owned()),
                results.len(),
                status.result_receipt,
                reason,
            );
        }
    };
    let final_results = match read_result_receipts(result_dir) {
        Ok(results) => results,
        Err(failure) => {
            return rejected_trial(
                identity,
                Some(ACTIVATION_RECEIPT.to_owned()),
                failure.observed,
                failure.receipt,
                failure.reason,
            );
        }
    };
    if final_activation != activation_bytes || final_results != results {
        return rejected_trial(
            identity,
            final_activation
                .as_ref()
                .map(|_| ACTIVATION_RECEIPT.to_owned()),
            final_results.len(),
            final_results.first().map(|(name, _)| name.clone()),
            "trial_evidence_changed_during_read",
        );
    }
    status
}

fn select_current_stale_generation(
    identity: &TrialIdentity,
    result_root: &Path,
) -> Result<Option<SelectedStaleGeneration>, &'static str> {
    let bytes = read_regular_receipt(&result_root.join(STALE_CURRENT_RECEIPT))?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let generation: CurrentStaleGeneration =
        serde_json::from_slice(&bytes).map_err(|_| "malformed_stale_generation")?;
    if generation.schema_version != 1
        || generation.repository != identity.repository
        || generation.pull_request != identity.pull_request
        || generation.target != identity.target
        || generation.head_sha != identity.head_sha
        || !canonical_hex(&generation.live_base_sha, 40)
        || !canonical_hex(&generation.context_digest, 64)
        || !canonical_hex(&generation.stale_receipt_sha256, 64)
    {
        return Err("stale_generation_identity_or_digest_mismatch");
    }
    let directory = result_root
        .join("stale-generations")
        .join(&generation.context_digest);
    Ok(Some(SelectedStaleGeneration {
        pointer_bytes: bytes,
        result_dir: directory,
        stale_receipt_sha256: generation.stale_receipt_sha256,
    }))
}

fn canonical_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[allow(clippy::too_many_lines)]
fn read_stale_trial<F>(
    identity: &TrialIdentity,
    result_dir: &Path,
    inputs: StaleTrialInputs<'_>,
    before_final_snapshot: &mut F,
) -> Option<TrialStatus>
where
    F: FnMut(),
{
    let stale = match read_named_receipts(result_dir, "stale-base-shadow-") {
        Ok(stale) => stale,
        Err(failure) => {
            return Some(rejected_trial(
                identity,
                None,
                failure.observed,
                failure.receipt,
                failure.reason,
            ));
        }
    };
    if stale.len() > 1 {
        let mut status = rejected_trial(
            identity,
            None,
            stale.len(),
            None,
            "ambiguous_stale_base_shadow_receipts",
        );
        status.state = TrialState::Terminal;
        status.shadow_disposition =
            Some(crate::changed_surface::StaleBaseShadowDisposition::Invalidated);
        return Some(status);
    }
    if inputs.ordinary_evidence_present && !stale.is_empty() {
        let mut status = rejected_trial(
            identity,
            Some(ACTIVATION_RECEIPT.to_owned()),
            stale.len(),
            stale.first().map(|(name, _)| name.clone()),
            "ambiguous_stale_and_activated_trial_generations",
        );
        status.state = TrialState::Terminal;
        status.shadow_disposition =
            Some(crate::changed_surface::StaleBaseShadowDisposition::Invalidated);
        return Some(status);
    }
    let (name, bytes) = stale.first()?;
    if inputs
        .expected_receipt_sha256
        .is_some_and(|expected| format!("{:x}", Sha256::digest(bytes)) != expected)
    {
        let mut status = rejected_trial(
            identity,
            Some(name.clone()),
            stale.len(),
            Some(name.clone()),
            "stale_generation_receipt_digest_mismatch",
        );
        status.state = TrialState::Terminal;
        status.shadow_disposition =
            Some(crate::changed_surface::StaleBaseShadowDisposition::Invalidated);
        return Some(status);
    }
    let status = if let (Some(activation), Some(cleanup)) = (inputs.activation, inputs.cleanup) {
        let result_files = inputs
            .results
            .iter()
            .map(|(name, bytes)| ReceiptFile { name, bytes })
            .collect::<Vec<_>>();
        evaluate_stale_base_execution(
            identity,
            ReceiptFile { name, bytes },
            ReceiptFile {
                name: STALE_ACTIVATION_RECEIPT,
                bytes: activation,
            },
            ReceiptFile {
                name: STALE_CLEANUP_RECEIPT,
                bytes: cleanup,
            },
            &result_files,
        )
    } else if inputs.activation.is_some() || inputs.cleanup.is_some() || !inputs.results.is_empty()
    {
        let mut status = rejected_trial(
            identity,
            inputs
                .activation
                .map(|_| STALE_ACTIVATION_RECEIPT.to_owned()),
            inputs.results.len(),
            inputs.results.first().map(|(name, _)| name.clone()),
            "incomplete_stale_base_execution_generation",
        );
        status.state = TrialState::Terminal;
        status.shadow_disposition =
            Some(crate::changed_surface::StaleBaseShadowDisposition::Invalidated);
        status
    } else if inputs.results.is_empty() {
        evaluate_stale_base_terminal(identity, ReceiptFile { name, bytes })
    } else {
        unreachable!()
    };
    before_final_snapshot();
    let final_ordinary_evidence_absent = matches!(
        read_regular_receipt(&result_dir.join(ACTIVATION_RECEIPT)),
        Ok(None)
    );
    let final_stale_activation = read_regular_receipt(&result_dir.join(STALE_ACTIVATION_RECEIPT));
    let final_stale_cleanup = read_regular_receipt(&result_dir.join(STALE_CLEANUP_RECEIPT));
    let final_results = read_result_receipts(result_dir);
    Some(
        match read_named_receipts(result_dir, "stale-base-shadow-") {
            Ok(final_stale)
                if final_stale == stale
                    && final_ordinary_evidence_absent
                    && matches!(final_stale_activation, Ok(ref value) if value.as_deref() == inputs.activation)
                    && matches!(final_stale_cleanup, Ok(ref value) if value.as_deref() == inputs.cleanup)
                    && matches!(final_results, Ok(ref value) if value == inputs.results) =>
            {
                status
            }
            _ => {
                let mut changed = rejected_trial(
                    identity,
                    None,
                    stale.len(),
                    Some(name.clone()),
                    "trial_evidence_changed_during_read",
                );
                changed.state = TrialState::Terminal;
                changed.shadow_disposition =
                    Some(crate::changed_surface::StaleBaseShadowDisposition::Invalidated);
                changed
            }
        },
    )
}

struct ReceiptReadFailure {
    reason: &'static str,
    observed: usize,
    receipt: Option<String>,
}

fn read_result_receipts(result_dir: &Path) -> Result<Vec<(String, Vec<u8>)>, ReceiptReadFailure> {
    let paths = read_named_receipts(result_dir, "result-")?;
    if paths.len() > 1 {
        return Err(ReceiptReadFailure {
            reason: "ambiguous_shadow_results",
            observed: paths.len(),
            receipt: None,
        });
    }
    Ok(paths)
}

fn read_named_receipts(
    result_dir: &Path,
    prefix: &str,
) -> Result<Vec<(String, Vec<u8>)>, ReceiptReadFailure> {
    let directory_metadata = match fs::symlink_metadata(result_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => {
            return Err(ReceiptReadFailure {
                reason: "unreadable_trial_directory",
                observed: 0,
                receipt: None,
            });
        }
    };
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(ReceiptReadFailure {
            reason: "unsafe_trial_directory",
            observed: 0,
            receipt: None,
        });
    }
    let entries = fs::read_dir(result_dir).map_err(|_| ReceiptReadFailure {
        reason: "unreadable_trial_directory",
        observed: 0,
        receipt: None,
    })?;
    let mut entries = entries
        .map(|entry| entry.map(|entry| (entry.file_name(), entry.path())))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ReceiptReadFailure {
            reason: "unreadable_trial_directory",
            observed: 0,
            receipt: None,
        })?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut paths = Vec::new();
    for (raw_name, path) in entries {
        let name = raw_name.into_string().map_err(|_| ReceiptReadFailure {
            reason: "unsafe_trial_directory_entry",
            observed: paths.len(),
            receipt: None,
        })?;
        if name.starts_with(prefix) {
            if Path::new(&name).extension() != Some(std::ffi::OsStr::new("json")) {
                return Err(ReceiptReadFailure {
                    reason: if prefix == "result-" {
                        "unexpected_result_entry"
                    } else {
                        "unexpected_stale_base_shadow_entry"
                    },
                    observed: paths.len() + 1,
                    receipt: Some(name),
                });
            }
            paths.push((name, path));
        }
    }
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    let mut receipts = Vec::with_capacity(paths.len());
    for (name, path) in paths {
        match read_regular_receipt(&path) {
            Ok(Some(bytes)) => receipts.push((name, bytes)),
            Ok(None) | Err(_) => {
                return Err(ReceiptReadFailure {
                    reason: if prefix == "result-" {
                        "unsafe_or_unreadable_result_receipt"
                    } else {
                        "unsafe_or_unreadable_stale_base_shadow_receipt"
                    },
                    observed: receipts.len() + 1,
                    receipt: Some(name),
                });
            }
        }
    }
    Ok(receipts)
}

fn read_regular_receipt(path: &Path) -> Result<Option<Vec<u8>>, &'static str> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("unreadable_receipt"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("unsafe_receipt_file");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path).map_err(|_| "unreadable_receipt")?;
    let opened_metadata = file.metadata().map_err(|_| "unreadable_receipt")?;
    if !opened_metadata.is_file() || opened_metadata.len() > MAX_RECEIPT_BYTES {
        return Err("receipt_too_large");
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "unreadable_receipt")?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RECEIPT_BYTES {
        return Err("receipt_too_large");
    }
    Ok(Some(bytes))
}

fn emit_status<W: Write>(
    status: &TrialStatus,
    result_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        write_json_envelope(
            stdout,
            "changed-surface-trial-status",
            std::collections::BTreeMap::from([
                (
                    "result_dir".to_owned(),
                    Value::String(result_dir.display().to_string()),
                ),
                (
                    "trial".to_owned(),
                    serde_json::to_value(status)
                        .map_err(|error| CliFailure::new(1, error.to_string()))?,
                ),
            ]),
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "{}: {} PR #{} target {} at {} ({})",
            match status.state {
                TrialState::Collecting => "collecting",
                TrialState::Ready => "ready",
                TrialState::Terminal => "terminal",
                TrialState::Rejected => "rejected",
            },
            status.repository,
            status.pull_request,
            status.target,
            short_sha(&status.head_sha),
            status.reason
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DIGEST_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn args() -> ChangedSurfaceTrialStatusArgs {
        ChangedSurfaceTrialStatusArgs {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            head_sha: "a".repeat(40),
        }
    }

    fn activation() -> Value {
        json!({
            "schema_version": 2,
            "machine_mode": "shadow_compare",
            "plan": {
                "schema_version": 2,
                "repository": "owner/repo",
                "pull_request": 42,
                "target": "mac",
                "base_sha": SHA_B,
                "head_sha": SHA_A,
                "tree_sha": SHA_B,
                "policy_digest": DIGEST_C,
                "changed_paths_digest": DIGEST_C,
                "validation_contract_digest": DIGEST_C,
                "workflow_digest": DIGEST_C,
                "selection_receipt_digest": DIGEST_C,
                "selected_tests_digest": DIGEST_C,
                "selected_build_targets_digest": DIGEST_C,
                "execution_payload_digest": DIGEST_C,
                "selected_count": 6,
                "selected_build_target_count": 2,
                "selection_tier": "affected",
                "stage": "build_and_test",
                "command": "protected adapter"
            }
        })
    }

    fn result() -> Value {
        json!({
            "schema_version": 2,
            "repository": "owner/repo",
            "pull_request": 42,
            "target": "mac",
            "base_sha": SHA_B,
            "head_sha": SHA_A,
            "tree_sha": SHA_B,
            "execution_payload_sha256": DIGEST_C,
            "policy_digest": DIGEST_C,
            "selection_receipt_digest": DIGEST_C,
            "validation_contract_digest": DIGEST_C,
            "workflow_digest": DIGEST_C,
            "selected_tests_digest": DIGEST_C,
            "selected_build_targets_digest": DIGEST_C,
            "selected_logical_count": 6,
            "selected_build_target_count": 2,
            "verification_duration_seconds": 0.2,
            "selected_duration_seconds": 2.0,
            "selected_build_duration_seconds": 3.0,
            "full_duration_seconds": 20.0,
            "full_build_incremental_duration_seconds": 7.0,
            "full_build_estimated_total_duration_seconds": 10.0,
            "selected_returncode": 0,
            "selected_build_returncode": 0,
            "full_returncode": 0,
            "full_build_returncode": 0,
            "full_authoritative": true,
            "comparison_verdict": "matched_pass",
            "graduation_eligible": true
        })
    }

    fn full_required_stale_receipt(live_base_sha: &str) -> Value {
        json!({
            "schema_version": 1,
            "disposition": "full_required",
            "merge_authority": "blocked_until_current_merge_tree",
            "repository": "owner/repo",
            "pull_request": 42,
            "target": "mac",
            "head_sha": SHA_A,
            "head_tree_sha": SHA_B,
            "old_protected_base_sha": SHA_A,
            "live_protected_base_sha": live_base_sha,
            "merge_base_sha": SHA_A,
            "changed_paths_digest": DIGEST_C,
            "protected_base_delta_digest": DIGEST_C,
            "old_workflow_digest": DIGEST_C,
            "live_workflow_digest": DIGEST_C,
            "validation_contract_digest": DIGEST_C,
            "integration_changed_paths_digest": DIGEST_C,
            "reason": "test_topology_drift"
        })
    }

    #[test]
    fn missing_trial_is_collecting_and_does_not_create_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = temp.path().join("state");
        let mut output = Vec::new();
        let exit = changed_surface_trial_status_command(&args(), &state, true, &mut output)
            .expect("status");
        assert_eq!(exit, ExitCode::from(3));
        assert!(!state.exists());
        let output: Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(output["trial"]["state"], "collecting");
        assert_eq!(output["trial"]["reason"], "waiting_for_shadow_activation");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_activation_is_rejected_without_following_it() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let state = temp.path().join("state");
        let identity = TrialIdentity {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            head_sha: "a".repeat(40),
        };
        let result_dir = result_directory(&state, &identity);
        fs::create_dir_all(&result_dir).expect("result dir");
        let outside = temp.path().join("outside.json");
        fs::write(&outside, b"{}").expect("outside");
        symlink(&outside, result_dir.join(ACTIVATION_RECEIPT)).expect("symlink");
        let mut output = Vec::new();
        let exit = changed_surface_trial_status_command(&args(), &state, true, &mut output)
            .expect("status");
        assert_eq!(exit, ExitCode::from(1));
        let output: Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(output["trial"]["state"], "rejected");
        assert_eq!(output["trial"]["reason"], "unsafe_receipt_file");
    }

    #[test]
    fn multiple_results_reject_before_receipt_payloads_are_trusted() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = temp.path().join("state");
        let identity = TrialIdentity {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            head_sha: "a".repeat(40),
        };
        let result_dir = result_directory(&state, &identity);
        fs::create_dir_all(&result_dir).expect("result dir");
        fs::write(result_dir.join(ACTIVATION_RECEIPT), b"{}").expect("activation");
        fs::write(result_dir.join("result-1.json"), b"{}").expect("result one");
        fs::write(result_dir.join("result-2.json"), b"{}").expect("result two");
        let mut output = Vec::new();
        let exit = changed_surface_trial_status_command(&args(), &state, true, &mut output)
            .expect("status");
        assert_eq!(exit, ExitCode::from(1));
        let output: Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(output["trial"]["state"], "rejected");
        assert_eq!(output["trial"]["reason"], "ambiguous_shadow_results");
        assert_eq!(output["trial"]["result_receipt_count"], 2);
    }

    #[test]
    fn result_appended_during_validation_is_rejected_before_ready() {
        let temp = tempfile::tempdir().expect("tempdir");
        let identity = TrialIdentity {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            head_sha: SHA_A.to_owned(),
        };
        let result_dir = result_directory(temp.path(), &identity);
        fs::create_dir_all(&result_dir).expect("result dir");
        fs::write(
            result_dir.join(ACTIVATION_RECEIPT),
            serde_json::to_vec(&activation()).expect("activation json"),
        )
        .expect("activation");
        let result_bytes = serde_json::to_vec(&result()).expect("result json");
        fs::write(result_dir.join("result-1.json"), &result_bytes).expect("first result");

        let status = read_trial_with_final_snapshot_hook(&identity, &result_dir, || {
            fs::write(result_dir.join("result-2.json"), &result_bytes).expect("second result");
        });

        assert_eq!(status.state, TrialState::Rejected);
        assert_eq!(status.reason, "ambiguous_shadow_results");
        assert_eq!(status.result_receipt_count, 2);
    }

    #[test]
    fn stale_base_full_required_is_terminal_instead_of_waiting_for_activation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let identity = TrialIdentity {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            head_sha: SHA_A.to_owned(),
        };
        let result_dir = result_directory(temp.path(), &identity);
        fs::create_dir_all(&result_dir).expect("result dir");
        let receipt = full_required_stale_receipt(SHA_B);
        fs::write(
            result_dir.join(format!("stale-base-shadow-{SHA_B}-{DIGEST_C}.json")),
            serde_json::to_vec(&receipt).expect("receipt"),
        )
        .expect("write receipt");

        let status = read_trial(&identity, &result_dir);
        assert_eq!(status.state, TrialState::Terminal);
        assert_eq!(status.reason, "stale_base_full_required");
        assert_eq!(
            status.shadow_disposition,
            Some(crate::changed_surface::StaleBaseShadowDisposition::FullRequired)
        );

        fs::write(
            result_dir.join(ACTIVATION_RECEIPT),
            serde_json::to_vec(&activation()).expect("activation json"),
        )
        .expect("activation");
        let status = read_trial(&identity, &result_dir);
        assert_eq!(status.state, TrialState::Terminal);
        assert_eq!(
            status.shadow_disposition,
            Some(crate::changed_surface::StaleBaseShadowDisposition::Invalidated)
        );
        assert_eq!(
            status.reason,
            "ambiguous_stale_and_activated_trial_generations"
        );
    }

    #[test]
    fn current_generation_selects_latest_base_without_ambiguity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let identity = TrialIdentity {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            head_sha: SHA_A.to_owned(),
        };
        let result_root = result_directory(temp.path(), &identity);
        let first_context = "d".repeat(64);
        let current_context = "e".repeat(64);
        let first_dir = result_root.join("stale-generations").join(&first_context);
        let current_dir = result_root.join("stale-generations").join(&current_context);
        fs::create_dir_all(&first_dir).expect("first generation");
        fs::create_dir_all(&current_dir).expect("current generation");
        let first_bytes =
            serde_json::to_vec(&full_required_stale_receipt(SHA_B)).expect("first stale receipt");
        fs::write(
            first_dir.join(format!("stale-base-shadow-{SHA_B}-{DIGEST_C}.json")),
            first_bytes,
        )
        .expect("first receipt");
        let current_live_base = "d".repeat(40);
        let current_bytes = serde_json::to_vec(&full_required_stale_receipt(&current_live_base))
            .expect("current stale receipt");
        fs::write(
            current_dir.join(format!(
                "stale-base-shadow-{current_live_base}-{DIGEST_C}.json"
            )),
            &current_bytes,
        )
        .expect("current receipt");
        fs::write(
            result_root.join(STALE_CURRENT_RECEIPT),
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "repository": "owner/repo",
                "pull_request": 42,
                "target": "mac",
                "head_sha": SHA_A,
                "live_base_sha": current_live_base,
                "context_digest": current_context,
                "stale_receipt_sha256": format!("{:x}", Sha256::digest(&current_bytes))
            }))
            .expect("generation pointer"),
        )
        .expect("write generation pointer");

        let status = read_trial(&identity, &result_root);
        assert_eq!(status.state, TrialState::Terminal);
        assert_eq!(status.reason, "stale_base_full_required");
        assert_eq!(status.result_receipt_count, 0);
    }

    #[test]
    fn current_generation_refuses_receipt_digest_mismatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let identity = TrialIdentity {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            head_sha: SHA_A.to_owned(),
        };
        let result_root = result_directory(temp.path(), &identity);
        let context = "d".repeat(64);
        let generation_dir = result_root.join("stale-generations").join(&context);
        fs::create_dir_all(&generation_dir).expect("generation");
        fs::write(
            generation_dir.join(format!("stale-base-shadow-{SHA_B}-{DIGEST_C}.json")),
            serde_json::to_vec(&full_required_stale_receipt(SHA_B)).expect("stale receipt"),
        )
        .expect("write stale receipt");
        fs::write(
            result_root.join(STALE_CURRENT_RECEIPT),
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "repository": "owner/repo",
                "pull_request": 42,
                "target": "mac",
                "head_sha": SHA_A,
                "live_base_sha": SHA_B,
                "context_digest": context,
                "stale_receipt_sha256": "f".repeat(64)
            }))
            .expect("generation pointer"),
        )
        .expect("write generation pointer");

        let status = read_trial(&identity, &result_root);
        assert_eq!(status.state, TrialState::Terminal);
        assert_eq!(
            status.shadow_disposition,
            Some(crate::changed_surface::StaleBaseShadowDisposition::Invalidated)
        );
        assert_eq!(status.reason, "stale_generation_receipt_digest_mismatch");
    }

    #[test]
    fn malformed_result_classification_is_independent_of_insertion_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir_all(&first).expect("first dir");
        fs::create_dir_all(&second).expect("second dir");
        fs::write(first.join("result-z.txt"), b"{}").expect("first z");
        fs::write(first.join("result-a.tmp"), b"{}").expect("first a");
        fs::write(second.join("result-a.tmp"), b"{}").expect("second a");
        fs::write(second.join("result-z.txt"), b"{}").expect("second z");

        let first = read_result_receipts(&first).expect_err("first rejected");
        let second = read_result_receipts(&second).expect_err("second rejected");

        assert_eq!(first.reason, second.reason);
        assert_eq!(first.observed, second.observed);
        assert_eq!(first.receipt, second.receipt);
        assert_eq!(first.reason, "unexpected_result_entry");
        assert_eq!(first.receipt.as_deref(), Some("result-a.tmp"));
    }
}
