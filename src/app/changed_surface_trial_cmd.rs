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
    ReceiptFile, TrialIdentity, TrialState, TrialStatus, audit_applied,
    evaluate_stale_base_execution, evaluate_stale_base_terminal, evaluate_trial, rejected_trial,
    result_directory,
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
    live_base_sha: String,
    context_digest: String,
    stale_receipt_sha256: String,
}

#[derive(Clone, Copy)]
struct StaleTrialInputs<'a> {
    ordinary_evidence_present: bool,
    activation: Option<&'a [u8]>,
    cleanup: Option<&'a [u8]>,
    results: &'a [(String, Vec<u8>)],
    expected_live_base_sha: Option<&'a str>,
    expected_context_digest: Option<&'a str>,
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
        (TrialState::Ready | TrialState::Terminal | TrialState::KeyedShadowRecorded, _) => {
            ExitCode::SUCCESS
        }
        (TrialState::Collecting, _) => ExitCode::from(3),
    })
}

fn read_trial(identity: &TrialIdentity, result_dir: &Path) -> TrialStatus {
    read_trial_with_final_snapshot_hook(identity, result_dir, || {})
}

/// Read a head's trial: its top-level evidence and, when the head holds
/// keyed payload directories, every one of them. Each directory is evaluated
/// exactly as a single run is, and the head's verdict is the one whose result
/// the adapter recorded last.
fn read_trial_with_final_snapshot_hook<F>(
    identity: &TrialIdentity,
    result_root: &Path,
    mut before_final_snapshot: F,
) -> TrialStatus
where
    F: FnMut(),
{
    let top = read_evidence_dir(identity, result_root, &mut before_final_snapshot);
    let payloads = crate::changed_surface::trial::payload_dirs(result_root);
    // A stale-base generation is its own evidence lane and never keyed.
    if payloads.is_empty() || result_root.join(STALE_CURRENT_RECEIPT).exists() {
        return top;
    }
    let mut candidates = vec![Candidate {
        payload_sha256: None,
        dir: result_root.to_path_buf(),
        status: top,
    }];
    for (digest, dir) in payloads {
        let mut status = read_evidence_dir(identity, &dir, &mut before_final_snapshot);
        let prefix = format!(
            "{}{digest}/",
            crate::changed_surface::trial::PAYLOAD_DIR_PREFIX
        );
        status.activation_receipt = status
            .activation_receipt
            .map(|name| format!("{prefix}{name}"));
        status.result_receipt = status.result_receipt.map(|name| format!("{prefix}{name}"));
        status.activation_conflicts = Vec::new();
        candidates.push(Candidate {
            payload_sha256: Some(digest),
            dir,
            status,
        });
    }
    merge_candidates(identity, candidates)
}

/// One evidence directory of a head and its own evaluation.
struct Candidate {
    payload_sha256: Option<String>,
    dir: PathBuf,
    status: TrialStatus,
}

impl Candidate {
    /// Whether the directory holds any evidence at all; an empty top level
    /// beside keyed payload directories is not a run.
    fn has_evidence(&self) -> bool {
        self.status.activation_receipt.is_some()
            || self.status.result_receipt_count > 0
            || self.status.state == TrialState::Rejected
    }

    fn disposition(&self) -> String {
        if let Some(keyed) = &self.status.keyed {
            return keyed.disposition.clone();
        }
        read_regular_receipt(&self.dir.join(ACTIVATION_RECEIPT))
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|activation| {
                activation
                    .pointer("/plan/disposition")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| crate::changed_surface::BOUNDED.to_owned())
    }

    /// The adapter's own record time for this directory's result.
    fn recorded_at(&self) -> Option<u64> {
        let name = self.status.result_receipt.as_deref()?;
        let file = name.rsplit('/').next()?;
        read_regular_receipt(&self.dir.join(file))
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|result| result.get("recorded_at_unix_ns").and_then(Value::as_u64))
    }

    fn keyed_run(&self) -> crate::changed_surface::trial::KeyedRun {
        crate::changed_surface::trial::KeyedRun {
            payload_sha256: self.payload_sha256.clone(),
            state: self.status.state,
            reason: self.status.reason.clone(),
            result_receipt: self.status.result_receipt.clone(),
            keyed: self.status.keyed.clone(),
        }
    }
}

/// The head's status from its evidence directories: the newest result by the
/// adapter's `recorded_at_unix_ns` (result name breaks a tie) decides, and
/// the status names it. A directory rejected before any result fails the
/// head closed, and a result without a record time is rejected rather than
/// ordered by file time.
fn merge_candidates(identity: &TrialIdentity, candidates: Vec<Candidate>) -> TrialStatus {
    let activation_conflicts = candidates[0].status.activation_conflicts.clone();
    let result_receipt_count = candidates
        .iter()
        .map(|candidate| candidate.status.result_receipt_count)
        .sum();
    let candidates: Vec<Candidate> = candidates
        .into_iter()
        .filter(Candidate::has_evidence)
        .collect();
    let mut timed = Vec::new();
    for candidate in &candidates {
        if candidate.status.result_receipt.is_none() {
            if candidate.status.state == TrialState::Rejected {
                let mut status = candidate.status.clone();
                status.activation_conflicts = activation_conflicts;
                return status;
            }
            continue;
        }
        let Some(recorded_at) = candidate.recorded_at() else {
            let mut status = rejected_trial(
                identity,
                candidate.status.activation_receipt.clone(),
                result_receipt_count,
                candidate.status.result_receipt.clone(),
                "result_without_recorded_at",
            );
            status.activation_conflicts = activation_conflicts;
            return status;
        };
        timed.push((recorded_at, candidate));
    }
    timed.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.status.result_receipt.cmp(&a.1.status.result_receipt))
    });
    let keyed_runs: Vec<_> = timed
        .iter()
        .map(|(_, candidate)| *candidate)
        .chain(
            candidates
                .iter()
                .filter(|candidate| candidate.status.result_receipt.is_none()),
        )
        .filter(|candidate| candidate.payload_sha256.is_some() || candidate.status.keyed.is_some())
        .map(Candidate::keyed_run)
        .collect();
    let decided = timed
        .first()
        .map(|(_, candidate)| *candidate)
        .or_else(|| candidates.first());
    let Some(decided) = decided else {
        let mut status = TrialStatus::new_collecting(identity);
        status.activation_conflicts = activation_conflicts;
        return status;
    };
    let mut status = decided.status.clone();
    if let Some(result_receipt) = decided.status.result_receipt.clone() {
        status.verdict_source = Some(crate::changed_surface::trial::VerdictSource {
            payload_sha256: decided.payload_sha256.clone(),
            disposition: decided.disposition(),
            result_receipt,
        });
    }
    status.result_receipt_count = result_receipt_count;
    status.activation_conflicts = activation_conflicts;
    status.keyed_runs = keyed_runs;
    status
}

#[allow(clippy::too_many_lines)]
fn read_evidence_dir<F>(
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
            expected_live_base_sha: current_generation
                .as_ref()
                .map(|generation| generation.live_base_sha.as_str()),
            expected_context_digest: current_generation
                .as_ref()
                .map(|generation| generation.context_digest.as_str()),
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
    let rederivations = match read_named_receipts(
        result_dir,
        crate::changed_surface::trial::REDERIVATION_RECEIPT_PREFIX,
    ) {
        Ok(receipts) => receipts,
        Err(failure) => {
            return rejected_trial(
                identity,
                Some(ACTIVATION_RECEIPT.to_owned()),
                results.len(),
                failure.receipt,
                failure.reason,
            );
        }
    };
    let rederivation_files = rederivations
        .iter()
        .map(|(name, bytes)| ReceiptFile { name, bytes })
        .collect::<Vec<_>>();
    let mut status = evaluate_trial(identity, activation, &result_files, &rederivation_files);
    if let Some(keyed) = status.keyed.as_mut() {
        keyed.record_audit(
            bound_audit(result_dir).as_ref(),
            run_applied_audit(result_dir, &results),
        );
    }
    status.activation_conflicts = activation_conflicts(result_dir);
    if !matches!(
        status.state,
        TrialState::Ready | TrialState::KeyedShadowRecorded
    ) {
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
    let final_rederivations = read_named_receipts(
        result_dir,
        crate::changed_surface::trial::REDERIVATION_RECEIPT_PREFIX,
    )
    .ok();
    if final_activation != activation_bytes
        || final_results != results
        || final_rederivations.as_ref() != Some(&rederivations)
    {
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
        live_base_sha: generation.live_base_sha,
        context_digest: generation.context_digest,
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
    let Some((name, bytes)) = stale.first() else {
        if inputs.expected_live_base_sha.is_some()
            || inputs.expected_context_digest.is_some()
            || inputs.expected_receipt_sha256.is_some()
        {
            let mut status =
                rejected_trial(identity, None, 0, None, "stale_generation_receipt_missing");
            status.state = TrialState::Terminal;
            status.shadow_disposition =
                Some(crate::changed_surface::StaleBaseShadowDisposition::Invalidated);
            return Some(status);
        }
        return None;
    };
    if let (Some(expected_live_base), Some(expected_context)) = (
        inputs.expected_live_base_sha,
        inputs.expected_context_digest,
    ) {
        let generation_matches = serde_json::from_slice::<
            crate::changed_surface::StaleBaseShadowReceipt,
        >(bytes)
        .is_ok_and(|receipt| {
            receipt.live_protected_base_sha == expected_live_base
                && crate::changed_surface::stale_base_context_digest(&receipt) == expected_context
        });
        if !generation_matches {
            let mut status = rejected_trial(
                identity,
                Some(name.clone()),
                stale.len(),
                Some(name.clone()),
                "stale_generation_context_mismatch",
            );
            status.state = TrialState::Terminal;
            status.shadow_disposition =
                Some(crate::changed_surface::StaleBaseShadowDisposition::Invalidated);
            return Some(status);
        }
    }
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

/// The audit status a keyed run's binding file records, if any.
fn bound_audit(
    result_dir: &Path,
) -> Option<crate::changed_surface::executable_reuse::AuditBinding> {
    let bytes = read_regular_receipt(
        &result_dir.join(crate::changed_surface::EXECUTABLE_REUSE_BINDING_FILE),
    )
    .ok()
    .flatten()?;
    serde_json::from_slice::<crate::changed_surface::executable_reuse::ExecutableReuseBinding>(
        &bytes,
    )
    .ok()?
    .audit
}

/// Whether the run's single result and the key manifest beside it show the
/// staged audit report in effect; see [`audit_applied`].
fn run_applied_audit(result_dir: &Path, results: &[(String, Vec<u8>)]) -> Result<(), String> {
    let [(_, bytes)] = results else {
        return Err("no single result".to_owned());
    };
    let result =
        serde_json::from_slice::<Value>(bytes).map_err(|_| "unreadable result".to_owned())?;
    let manifest = read_regular_receipt(&result_dir.join("executable-keys.json"))
        .ok()
        .flatten();
    audit_applied(&result, manifest.as_deref())
}

/// The `activation_conflict` diagnostics a ship wrote into this trial
/// directory, oldest first. Diagnostics are evidence of what ran, never of a
/// result, so an unreadable one is skipped rather than failing the read.
fn activation_conflicts(result_dir: &Path) -> Vec<Value> {
    let Ok(files) = read_named_receipts(result_dir, "fallback-") else {
        return Vec::new();
    };
    files
        .iter()
        .filter_map(|(_, bytes)| serde_json::from_slice::<Value>(bytes).ok())
        .filter(|value| {
            value.get("category").and_then(Value::as_str) == Some("activation_conflict")
        })
        .collect()
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
                TrialState::KeyedShadowRecorded => "keyed_shadow_recorded",
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
    fn an_activation_conflict_is_reported_in_the_trial_status() {
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
        fs::write(
            result_dir.join("fallback-1-2-0.json"),
            json!({"category": "activation_conflict", "status": "unkeyed: activation_conflict",
                   "existing_payload_sha256": "aaaa", "planned_payload_sha256": "bbbb",
                   "differing_fields": ["executable_reuse.candidates"]})
            .to_string(),
        )
        .expect("diagnostic");
        fs::write(
            result_dir.join("fallback-1-2-1.json"),
            json!({"category": "full_fallback"}).to_string(),
        )
        .expect("other diagnostic");
        let mut output = Vec::new();
        changed_surface_trial_status_command(&args(), &state, true, &mut output).expect("status");
        let output: Value = serde_json::from_slice(&output).expect("json");
        let conflicts = output["trial"]["activation_conflicts"]
            .as_array()
            .expect("listed");
        assert_eq!(conflicts.len(), 1, "only activation conflicts are listed");
        assert_eq!(conflicts[0]["status"], "unkeyed: activation_conflict");
        assert_eq!(conflicts[0]["planned_payload_sha256"], "bbbb");
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
        let first_dir = result_root.join("stale-generations").join(&first_context);
        fs::create_dir_all(&first_dir).expect("first generation");
        let first_bytes =
            serde_json::to_vec(&full_required_stale_receipt(SHA_B)).expect("first stale receipt");
        fs::write(
            first_dir.join(format!("stale-base-shadow-{SHA_B}-{DIGEST_C}.json")),
            first_bytes,
        )
        .expect("first receipt");
        let current_live_base = "d".repeat(40);
        let current_receipt = full_required_stale_receipt(&current_live_base);
        let current_context = crate::changed_surface::stale_base_context_digest(
            &serde_json::from_value(current_receipt.clone()).expect("typed current receipt"),
        );
        let current_dir = result_root.join("stale-generations").join(&current_context);
        fs::create_dir_all(&current_dir).expect("current generation");
        let current_bytes = serde_json::to_vec(&current_receipt).expect("current stale receipt");
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
        let receipt = full_required_stale_receipt(SHA_B);
        let context = crate::changed_surface::stale_base_context_digest(
            &serde_json::from_value(receipt.clone()).expect("typed receipt"),
        );
        let generation_dir = result_root.join("stale-generations").join(&context);
        fs::create_dir_all(&generation_dir).expect("generation");
        fs::write(
            generation_dir.join(format!("stale-base-shadow-{SHA_B}-{DIGEST_C}.json")),
            serde_json::to_vec(&receipt).expect("stale receipt"),
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
    fn current_generation_with_missing_receipt_is_terminally_invalidated() {
        let temp = tempfile::tempdir().expect("tempdir");
        let identity = TrialIdentity {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            head_sha: SHA_A.to_owned(),
        };
        let result_root = result_directory(temp.path(), &identity);
        let context = "d".repeat(64);
        fs::create_dir_all(result_root.join("stale-generations").join(&context))
            .expect("generation");
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
                "stale_receipt_sha256": DIGEST_C
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
        assert_eq!(status.reason, "stale_generation_receipt_missing");
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

    fn identity() -> TrialIdentity {
        TrialIdentity {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            head_sha: "a".repeat(40),
        }
    }

    /// One run's evidence written into `dir`: a keyed bounded activation for
    /// `payload` (or an unkeyed one when `keyed` is false) and its result,
    /// recorded at `recorded_at` (omitted when `None`).
    fn write_run(dir: &Path, payload: &str, keyed: bool, recorded_at: Option<u64>) {
        fs::create_dir_all(dir).expect("evidence dir");
        let mut activation = activation();
        activation["plan"]["execution_payload_digest"] = json!(payload);
        let mut result = result();
        result["execution_payload_sha256"] = json!(payload);
        if keyed {
            activation["plan"]["disposition"] = json!("keyed_bounded_shadow");
            result["selected_execution_disposition"] = json!("keyed_bounded_shadow");
            result["executable_reuse"] =
                json!({"would_skip_tests": ["a"], "false_skip_count": 0, "false_skips": []});
        }
        if let Some(recorded_at) = recorded_at {
            result["recorded_at_unix_ns"] = json!(recorded_at);
        }
        fs::write(dir.join(ACTIVATION_RECEIPT), activation.to_string()).expect("activation");
        fs::write(dir.join("result-1.json"), result.to_string()).expect("result");
    }

    const PAYLOAD_1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const PAYLOAD_2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    #[test]
    fn a_reship_with_a_new_candidate_set_keeps_both_runs_and_the_newest_decides() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = result_directory(&temp.path().join("state"), &identity());
        write_run(
            &crate::changed_surface::trial::payload_dir(&root, PAYLOAD_1),
            PAYLOAD_1,
            true,
            Some(10),
        );
        write_run(
            &crate::changed_surface::trial::payload_dir(&root, PAYLOAD_2),
            PAYLOAD_2,
            true,
            Some(20),
        );
        let status = read_trial(&identity(), &root);
        assert_eq!(status.state, TrialState::Ready, "{}", status.reason);
        let source = status.verdict_source.expect("named");
        assert_eq!(source.payload_sha256.as_deref(), Some(PAYLOAD_2));
        assert_eq!(source.disposition, "keyed_bounded_shadow");
        assert_eq!(
            source.result_receipt,
            format!("payload-{PAYLOAD_2}/result-1.json")
        );
        assert_eq!(status.result_receipt_count, 2);
        let listed: Vec<_> = status
            .keyed_runs
            .iter()
            .map(|run| run.payload_sha256.as_deref())
            .collect();
        assert_eq!(listed, [Some(PAYLOAD_2), Some(PAYLOAD_1)], "newest first");
    }

    #[test]
    fn an_unkeyed_top_level_run_and_a_keyed_payload_run_coexist() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = result_directory(&temp.path().join("state"), &identity());
        write_run(&root, DIGEST_C, false, Some(30));
        write_run(
            &crate::changed_surface::trial::payload_dir(&root, PAYLOAD_1),
            PAYLOAD_1,
            true,
            Some(10),
        );
        let status = read_trial(&identity(), &root);
        assert_eq!(status.state, TrialState::Ready, "{}", status.reason);
        let source = status.verdict_source.expect("named");
        assert_eq!(source.payload_sha256, None, "the newer unkeyed run decides");
        assert_eq!(source.disposition, "bounded");
        assert_eq!(source.result_receipt, "result-1.json");
        assert_eq!(status.keyed_runs.len(), 1);
        assert_eq!(
            status.keyed_runs[0].payload_sha256.as_deref(),
            Some(PAYLOAD_1)
        );
    }

    #[test]
    fn a_payload_result_without_a_record_time_is_rejected_not_ordered_by_mtime() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = result_directory(&temp.path().join("state"), &identity());
        write_run(
            &crate::changed_surface::trial::payload_dir(&root, PAYLOAD_1),
            PAYLOAD_1,
            true,
            Some(10),
        );
        write_run(
            &crate::changed_surface::trial::payload_dir(&root, PAYLOAD_2),
            PAYLOAD_2,
            true,
            None,
        );
        let status = read_trial(&identity(), &root);
        assert_eq!(status.state, TrialState::Rejected);
        assert_eq!(status.reason, "result_without_recorded_at");
    }

    #[test]
    fn a_payload_result_naming_another_payload_is_still_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = result_directory(&temp.path().join("state"), &identity());
        let dir = crate::changed_surface::trial::payload_dir(&root, PAYLOAD_1);
        write_run(&dir, PAYLOAD_1, true, Some(10));
        let mut result: Value =
            serde_json::from_slice(&fs::read(dir.join("result-1.json")).expect("read"))
                .expect("json");
        result["execution_payload_sha256"] = json!(PAYLOAD_2);
        fs::write(dir.join("result-1.json"), result.to_string()).expect("write");
        let status = read_trial(&identity(), &root);
        assert_eq!(status.state, TrialState::Rejected);
        assert_eq!(status.reason, "shadow_result_digest_or_count_mismatch");
        assert_eq!(
            status.result_receipt.as_deref(),
            Some(format!("payload-{PAYLOAD_1}/result-1.json").as_str())
        );
    }

    #[test]
    fn a_staged_audit_counts_only_when_the_run_applied_it_cleanly() {
        use crate::changed_surface::executable_reuse::{AuditBinding, ExecutableReuseBinding};
        let temp = tempfile::tempdir().expect("tempdir");
        let root = result_directory(&temp.path().join("state"), &identity());
        let binding = ExecutableReuseBinding {
            candidates: Vec::new(),
            rules_digest: DIGEST_C.to_owned(),
            derivation_code_dir: "/derivation".to_owned(),
            derivation_code_sha256: DIGEST_C.to_owned(),
            sample_seed: DIGEST_C.to_owned(),
            sample_percent: 5,
            build_dir: "build".to_owned(),
            audit: Some(AuditBinding::Staged {
                run_id: "7".to_owned(),
                audit_commit: "b".repeat(40),
                commits_behind: 2,
                report_sha256: DIGEST_C.to_owned(),
            }),
        };
        let observe = |audit_status: &str, key_code_status: &str| {
            write_run(&root, PAYLOAD_1, true, None);
            fs::write(
                root.join(crate::changed_surface::EXECUTABLE_REUSE_BINDING_FILE),
                serde_json::to_vec(&binding).expect("binding"),
            )
            .expect("binding file");
            let manifest = json!({"producer": {"audit_status": key_code_status}}).to_string();
            fs::write(root.join("executable-keys.json"), &manifest).expect("manifest");
            let path = root.join("result-1.json");
            let mut result: Value =
                serde_json::from_slice(&fs::read(&path).expect("result")).expect("json");
            result["executable_reuse"]["derived"] = json!({
                "audit": {"status": audit_status},
                "key_manifest_sha256": format!("{:x}", Sha256::digest(manifest.as_bytes())),
            });
            fs::write(&path, result.to_string()).expect("result");
            let keyed = read_trial(&identity(), &root).keyed.expect("keyed summary");
            (keyed.audit, keyed.reuse_observation)
        };
        assert_eq!(observe("applied", "clean"), ("staged".to_owned(), true));
        assert_eq!(
            observe("applied", "not_clean"),
            (
                "staged_unconfirmed: key code audit status not_clean".to_owned(),
                false
            )
        );
        assert_eq!(
            observe("base_key_code_predates_audit", "absent"),
            (
                "staged_unconfirmed: runner audit status base_key_code_predates_audit".to_owned(),
                false
            )
        );
    }

    #[test]
    fn an_old_layout_keyed_trial_reads_in_place_and_is_never_moved() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = result_directory(&temp.path().join("state"), &identity());
        // The pre-payload-directory layout: keyed activation, context and
        // result at the top level of the trial directory.
        write_run(&root, PAYLOAD_1, true, None);
        fs::write(
            root.join(crate::changed_surface::executable_reuse::KEYED_CONTEXT_RECEIPT),
            b"{}",
        )
        .expect("context");
        let before: Vec<_> = {
            let mut names: Vec<_> = fs::read_dir(&root)
                .expect("dir")
                .flatten()
                .map(|entry| entry.file_name())
                .collect();
            names.sort();
            names
        };
        let status = read_trial(&identity(), &root);
        assert_eq!(status.state, TrialState::Ready, "{}", status.reason);
        assert_eq!(status.result_receipt.as_deref(), Some("result-1.json"));
        assert_eq!(status.verdict_source, None, "a single run needs no source");
        assert!(status.keyed.is_some());
        let mut after: Vec<_> = fs::read_dir(&root)
            .expect("dir")
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        after.sort();
        assert_eq!(before, after, "nothing is moved, renamed or added");
    }
}
