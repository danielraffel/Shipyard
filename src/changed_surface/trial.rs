//! Read-only verification of changed-surface shadow-comparison receipts.
//!
//! The adapter writes append-only result receipts into the exact activation
//! directory selected by Shipyard. This module compares those untrusted bytes
//! with the immutable activation plan. It never starts work, mutates evidence,
//! or promotes a selector policy.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Current stable trial-status schema.
pub const TRIAL_STATUS_SCHEMA_VERSION: u32 = 2;
/// Result schema emitted by the current changed-surface adapter.
const RESULT_RECEIPT_SCHEMA_VERSION: u32 = 2;

/// Exact identity supplied by the operator querying a trial.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrialIdentity {
    /// Canonical `owner/repo` identity.
    pub repository: String,
    /// Pull request number.
    pub pull_request: u64,
    /// Exact target name.
    pub target: String,
    /// Exact pull-request head SHA.
    pub head_sha: String,
}

/// One regular receipt file read by the command boundary.
#[derive(Clone, Copy, Debug)]
pub struct ReceiptFile<'a> {
    /// Basename used only for diagnostics and stable output.
    pub name: &'a str,
    /// Complete receipt bytes.
    pub bytes: &'a [u8],
}

/// Stable state of one exact-head shadow comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialState {
    /// Activation or comparison output has not arrived yet.
    Collecting,
    /// The single observed receipt is exact-bound and proves a matched pass.
    Ready,
    /// Planning reached a safe terminal shadow-only disposition without a
    /// runnable bounded activation.
    Terminal,
    /// Observed evidence is malformed, contradictory, or non-passing.
    Rejected,
}

/// Stable read-only status returned by `changed-surface-trial-status`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrialStatus {
    /// Status schema version.
    pub schema_version: u32,
    /// Current state.
    pub state: TrialState,
    /// Exact repository identity.
    pub repository: String,
    /// Exact pull request.
    pub pull_request: u64,
    /// Exact target.
    pub target: String,
    /// Exact head SHA.
    pub head_sha: String,
    /// Shadow activation receipt, when observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation_receipt: Option<String>,
    /// Number of append-only comparison receipts observed.
    pub result_receipt_count: usize,
    /// Result receipt that established readiness or rejection, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_receipt: Option<String>,
    /// Validated selected/full wall-clock comparison, when the current plan
    /// includes build-target selection telemetry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing: Option<TrialTiming>,
    /// Typed stale-base result when planning terminated before activation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_disposition: Option<super::StaleBaseShadowDisposition>,
    /// Stable bounded reason for the current state.
    pub reason: String,
}

/// Validated timing and estimated savings for one matched shadow comparison.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrialTiming {
    /// Time spent verifying the immutable selected-test input.
    pub verification_duration_seconds: f64,
    /// Time spent building the selected producer targets.
    pub selected_build_duration_seconds: f64,
    /// Time spent running the selected tests.
    pub selected_test_duration_seconds: f64,
    /// Input verification plus selected build and selected test wall time.
    pub selected_total_duration_seconds: f64,
    /// Incremental remainder of the full build after the selected build.
    pub full_build_incremental_duration_seconds: f64,
    /// Estimated full build time: selected build plus incremental remainder.
    pub full_build_estimated_total_duration_seconds: f64,
    /// Time spent running the full test corpus.
    pub full_test_duration_seconds: f64,
    /// Estimated full build plus full test wall time.
    pub full_estimated_total_duration_seconds: f64,
    /// Estimated seconds saved by the selected path. May be negative.
    pub estimated_savings_seconds: f64,
    /// Full-path time divided by selected-path time, when selected time is nonzero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_speedup_ratio: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ActivationReceipt {
    schema_version: u32,
    machine_mode: String,
    plan: ActivationPlan,
}

#[derive(Debug, Deserialize)]
struct StaleActivationReceipt {
    schema_version: u32,
    machine_mode: String,
    merge_authority: super::MergeAuthority,
    stale_context_digest: String,
    stale_receipt_sha256: String,
    plan: ActivationPlan,
}

#[derive(Debug, Deserialize)]
struct StaleCleanupReceipt {
    schema_version: u32,
    context_digest: String,
    integration_commit_sha: String,
    integration_tree_sha: String,
    disposition: String,
}

#[derive(Debug, Deserialize)]
struct ActivationPlan {
    schema_version: u32,
    repository: String,
    pull_request: u64,
    target: String,
    base_sha: String,
    head_sha: String,
    tree_sha: String,
    policy_digest: String,
    changed_paths_digest: String,
    validation_contract_digest: String,
    workflow_digest: String,
    selection_receipt_digest: String,
    selected_tests_digest: String,
    selected_build_targets_digest: Option<String>,
    execution_payload_digest: String,
    selected_count: usize,
    selected_build_target_count: usize,
    selection_tier: super::SelectionTier,
    stage: String,
}

pub(crate) fn validate_stale_activation_for_cleanup(
    stale: &super::StaleBaseShadowReceipt,
    stale_bytes: &[u8],
    activation_bytes: &[u8],
) -> Result<(), String> {
    let activation: StaleActivationReceipt = serde_json::from_slice(activation_bytes)
        .map_err(|error| format!("decode stale integration activation: {error}"))?;
    if activation.schema_version != activation.plan.schema_version
        || activation.machine_mode != "shadow_compare"
        || activation.merge_authority != super::MergeAuthority::BlockedUntilCurrentMergeTree
        || activation.stale_context_digest != super::stale_base_context_digest(stale)
        || sha256(stale_bytes) != activation.stale_receipt_sha256
        || activation.plan.repository != stale.repository
        || activation.plan.pull_request != stale.pull_request
        || activation.plan.target != stale.target
        || Some(activation.plan.head_sha.as_str()) != stale.integration_commit_sha.as_deref()
        || Some(activation.plan.tree_sha.as_str()) != stale.integration_tree_sha.as_deref()
        || activation.plan.base_sha != stale.live_protected_base_sha
        || activation.plan.validation_contract_digest != stale.validation_contract_digest
        || activation.plan.workflow_digest != stale.live_workflow_digest
        || validate_stale_plan_selection(&activation.plan, stale).is_err()
    {
        return Err("stale integration activation identity or linkage mismatch".to_owned());
    }
    Ok(())
}

#[derive(Serialize)]
struct ExecutionPayloadBinding<'a> {
    schema_version: u32,
    repository: &'a str,
    pull_request: u64,
    target: &'a str,
    base_sha: &'a str,
    head_sha: &'a str,
    tree_sha: &'a str,
    policy_digest: &'a str,
    selection_receipt_digest: &'a str,
    validation_contract_digest: &'a str,
    workflow_digest: &'a str,
    selected_tests_digest: &'a str,
    selected_tests: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_build_targets_digest: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_build_targets: Option<&'a [String]>,
}

#[derive(Debug, Deserialize)]
struct ResultReceipt {
    schema_version: u32,
    repository: String,
    pull_request: u64,
    target: String,
    base_sha: String,
    head_sha: String,
    tree_sha: String,
    execution_payload_sha256: String,
    policy_digest: String,
    selection_receipt_digest: String,
    validation_contract_digest: String,
    workflow_digest: String,
    selected_tests_digest: String,
    selected_build_targets_digest: Option<String>,
    selected_logical_count: usize,
    selected_build_target_count: usize,
    #[serde(default)]
    verification_duration_seconds: Option<f64>,
    #[serde(default)]
    selected_duration_seconds: Option<f64>,
    #[serde(default)]
    selected_build_duration_seconds: Option<f64>,
    #[serde(default)]
    full_duration_seconds: Option<f64>,
    #[serde(default)]
    full_build_incremental_duration_seconds: Option<f64>,
    #[serde(default)]
    full_build_estimated_total_duration_seconds: Option<f64>,
    selected_returncode: i32,
    selected_build_returncode: Option<i32>,
    full_returncode: Option<i32>,
    full_build_returncode: Option<i32>,
    full_authoritative: bool,
    comparison_verdict: String,
    graduation_eligible: bool,
}

/// Return the machine-global directory for one exact trial identity.
///
/// Repository and target components retain a digest suffix so distinct inputs
/// cannot collapse onto the same sanitized path.
#[must_use]
pub fn result_directory(state_dir: &Path, identity: &TrialIdentity) -> PathBuf {
    state_dir
        .join("changed-surface-results")
        .join(path_component(&identity.repository))
        .join(identity.pull_request.to_string())
        .join(path_component(&identity.head_sha))
        .join(path_component(&identity.target))
}

/// Validate the shadow activation and every observed append-only result.
#[must_use]
pub fn evaluate_trial(
    identity: &TrialIdentity,
    activation: Option<ReceiptFile<'_>>,
    results: &[ReceiptFile<'_>],
) -> TrialStatus {
    let mut status = TrialStatus {
        schema_version: TRIAL_STATUS_SCHEMA_VERSION,
        state: TrialState::Collecting,
        repository: identity.repository.clone(),
        pull_request: identity.pull_request,
        target: identity.target.clone(),
        head_sha: identity.head_sha.clone(),
        activation_receipt: activation.map(|receipt| receipt.name.to_owned()),
        result_receipt_count: results.len(),
        result_receipt: None,
        timing: None,
        shadow_disposition: None,
        reason: "waiting_for_shadow_activation".to_owned(),
    };

    if let Err(reason) = validate_identity(identity) {
        return reject(status, None, reason);
    }
    let Some(activation_file) = activation else {
        return if results.is_empty() {
            status
        } else {
            reject(status, None, "result_without_shadow_activation")
        };
    };
    let Ok(activation) = serde_json::from_slice::<ActivationReceipt>(activation_file.bytes) else {
        return reject(status, None, "malformed_shadow_activation");
    };
    if let Err(reason) = validate_activation(identity, &activation) {
        return reject(status, None, reason);
    }
    if results.is_empty() {
        "waiting_for_shadow_result".clone_into(&mut status.reason);
        return status;
    }
    if results.len() != 1 {
        return reject(status, None, "ambiguous_shadow_results");
    }

    for result_file in results {
        let Ok(result) = serde_json::from_slice::<ResultReceipt>(result_file.bytes) else {
            return reject(status, Some(result_file.name), "malformed_shadow_result");
        };
        if let Err(reason) = validate_result(&activation.plan, &result) {
            return reject(status, Some(result_file.name), reason);
        }
        status.result_receipt = Some(result_file.name.to_owned());
        status.timing = timing(&activation.plan, &result);
    }
    status.state = TrialState::Ready;
    "matched_pass".clone_into(&mut status.reason);
    status
}

/// Validate a terminal stale-base receipt when no runnable activation exists.
#[must_use]
pub fn evaluate_stale_base_terminal(
    identity: &TrialIdentity,
    receipt_file: ReceiptFile<'_>,
) -> TrialStatus {
    let mut status = TrialStatus {
        schema_version: TRIAL_STATUS_SCHEMA_VERSION,
        state: TrialState::Terminal,
        repository: identity.repository.clone(),
        pull_request: identity.pull_request,
        target: identity.target.clone(),
        head_sha: identity.head_sha.clone(),
        activation_receipt: None,
        result_receipt_count: 0,
        result_receipt: Some(receipt_file.name.to_owned()),
        timing: None,
        shadow_disposition: Some(super::StaleBaseShadowDisposition::Invalidated),
        reason: "malformed_stale_base_shadow_receipt".to_owned(),
    };
    if let Err(reason) = validate_identity(identity) {
        reason.clone_into(&mut status.reason);
        return status;
    }
    let Ok(receipt) = serde_json::from_slice::<super::StaleBaseShadowReceipt>(receipt_file.bytes)
    else {
        return status;
    };
    if receipt.schema_version != 1
        || receipt.merge_authority != super::MergeAuthority::BlockedUntilCurrentMergeTree
        || receipt.repository != identity.repository
        || receipt.pull_request != identity.pull_request
        || receipt.target != identity.target
        || receipt.head_sha != identity.head_sha
        || !valid_sha(&receipt.head_tree_sha)
        || !valid_sha(&receipt.old_protected_base_sha)
        || !valid_sha(&receipt.live_protected_base_sha)
        || !valid_sha(&receipt.merge_base_sha)
        || !valid_digest(&receipt.changed_paths_digest)
        || !valid_digest(&receipt.protected_base_delta_digest)
        || !valid_digest(&receipt.old_workflow_digest)
        || !valid_digest(&receipt.live_workflow_digest)
        || !valid_digest(&receipt.validation_contract_digest)
        || !valid_digest(&receipt.integration_changed_paths_digest)
    {
        "stale_base_shadow_identity_or_contract_mismatch".clone_into(&mut status.reason);
        return status;
    }
    status.shadow_disposition = Some(receipt.disposition);
    match receipt.disposition {
        super::StaleBaseShadowDisposition::Recomputed
        | super::StaleBaseShadowDisposition::Reused => {
            let selection_valid = receipt
                .shadow_selection
                .as_deref()
                .is_some_and(|selection| {
                    selection.planned_suite == super::PlannedSuite::Bounded
                        && selection.authoritative_suite == super::PlannedSuite::Full
                        && selection.shadow_only
                        && selection.shadow_context_digest.as_deref()
                            == Some(super::stale_base::stale_base_context_digest(&receipt).as_str())
                        && selection.repository == receipt.repository
                        && selection.pull_request == receipt.pull_request
                        && selection.target == receipt.target
                        && Some(selection.head_sha.as_str())
                            == receipt.integration_commit_sha.as_deref()
                        && Some(selection.tree_sha.as_str())
                            == receipt.integration_tree_sha.as_deref()
                        && selection.pr_base_sha == receipt.live_protected_base_sha
                        && selection.protected_ref_sha == receipt.live_protected_base_sha
                        && selection.merge_base_sha == receipt.live_protected_base_sha
                });
            if !receipt
                .integration_tree_sha
                .as_deref()
                .is_some_and(valid_sha)
                || !receipt
                    .integration_commit_sha
                    .as_deref()
                    .is_some_and(valid_sha)
                || !receipt
                    .old_policy_digest
                    .as_deref()
                    .is_some_and(valid_digest)
                || !receipt
                    .live_policy_digest
                    .as_deref()
                    .is_some_and(valid_digest)
                || !selection_valid
            {
                status.state = TrialState::Terminal;
                status.shadow_disposition = Some(super::StaleBaseShadowDisposition::Invalidated);
                "stale_base_shadow_selection_mismatch".clone_into(&mut status.reason);
                return status;
            }
            status.state = TrialState::Terminal;
            status.reason = format!("stale_base_{}", disposition_name(receipt.disposition));
        }
        super::StaleBaseShadowDisposition::Blocked
        | super::StaleBaseShadowDisposition::Invalidated
        | super::StaleBaseShadowDisposition::FullRequired => {
            status.state = TrialState::Terminal;
            status.reason = format!("stale_base_{}", disposition_name(receipt.disposition));
        }
    }
    status
}

/// Validate one selected-only stale-base integration execution. The result is
/// terminal shadow evidence and can never become `Ready` merge authority.
#[must_use]
pub fn evaluate_stale_base_execution(
    identity: &TrialIdentity,
    stale_file: ReceiptFile<'_>,
    activation_file: ReceiptFile<'_>,
    cleanup_file: ReceiptFile<'_>,
    results: &[ReceiptFile<'_>],
) -> TrialStatus {
    let mut status = evaluate_stale_base_terminal(identity, stale_file);
    status.activation_receipt = Some(activation_file.name.to_owned());
    status.result_receipt_count = results.len();
    if status.state != TrialState::Terminal
        || !matches!(
            status.shadow_disposition,
            Some(
                super::StaleBaseShadowDisposition::Recomputed
                    | super::StaleBaseShadowDisposition::Reused
            )
        )
    {
        status.shadow_disposition = Some(super::StaleBaseShadowDisposition::Invalidated);
        "invalid_outer_stale_base_execution_receipt".clone_into(&mut status.reason);
        return status;
    }
    let Ok(stale) = serde_json::from_slice::<super::StaleBaseShadowReceipt>(stale_file.bytes)
    else {
        return status;
    };
    let Ok(activation) = serde_json::from_slice::<StaleActivationReceipt>(activation_file.bytes)
    else {
        status.shadow_disposition = Some(super::StaleBaseShadowDisposition::Invalidated);
        "malformed_stale_base_activation".clone_into(&mut status.reason);
        return status;
    };
    let Ok(cleanup) = serde_json::from_slice::<StaleCleanupReceipt>(cleanup_file.bytes) else {
        status.shadow_disposition = Some(super::StaleBaseShadowDisposition::Invalidated);
        "malformed_stale_base_cleanup_receipt".clone_into(&mut status.reason);
        return status;
    };
    if validate_stale_activation_for_cleanup(&stale, stale_file.bytes, activation_file.bytes)
        .is_err()
        || cleanup.schema_version != 1
        || cleanup.context_digest != activation.stale_context_digest
        || cleanup.integration_commit_sha != activation.plan.head_sha
        || cleanup.integration_tree_sha != activation.plan.tree_sha
        || cleanup.disposition != "cleaned"
    {
        status.shadow_disposition = Some(super::StaleBaseShadowDisposition::Invalidated);
        "stale_base_activation_identity_or_link_mismatch".clone_into(&mut status.reason);
        return status;
    }
    if results.len() != 1 {
        status.shadow_disposition = Some(super::StaleBaseShadowDisposition::Invalidated);
        if results.is_empty() {
            "stale_base_execution_missing_result"
        } else {
            "ambiguous_stale_base_execution_results"
        }
        .clone_into(&mut status.reason);
        return status;
    }
    let result_file = results[0];
    let Ok(result) = serde_json::from_slice::<ResultReceipt>(result_file.bytes) else {
        status.shadow_disposition = Some(super::StaleBaseShadowDisposition::Invalidated);
        "malformed_stale_base_result".clone_into(&mut status.reason);
        return status;
    };
    if validate_result(&activation.plan, &result).is_err() {
        status.shadow_disposition = Some(super::StaleBaseShadowDisposition::Invalidated);
        status.result_receipt = Some(result_file.name.to_owned());
        "stale_base_selected_result_mismatch".clone_into(&mut status.reason);
        return status;
    }
    status.state = TrialState::Terminal;
    status.result_receipt = Some(result_file.name.to_owned());
    status.shadow_disposition = Some(stale.disposition);
    status.reason = format!(
        "stale_base_{}_selected_pass",
        disposition_name(stale.disposition)
    );
    status
}

fn validate_stale_plan_selection(
    plan: &ActivationPlan,
    stale: &super::StaleBaseShadowReceipt,
) -> Result<(), &'static str> {
    let mut selection = stale
        .shadow_selection
        .as_deref()
        .cloned()
        .ok_or("missing_stale_shadow_selection")?;
    if selection.shadow_context_digest.as_deref()
        != Some(super::stale_base_context_digest(stale).as_str())
        || selection.repository != plan.repository
        || selection.pull_request != plan.pull_request
        || selection.target != plan.target
        || selection.pr_base_sha != plan.base_sha
        || selection.head_sha != plan.head_sha
        || selection.tree_sha != plan.tree_sha
        || selection.policy_digest.as_deref() != Some(plan.policy_digest.as_str())
        || selection.changed_paths_digest != plan.changed_paths_digest
        || selection.selection_tier != plan.selection_tier
        || selection.selected_tests.len() != plan.selected_count
        || selection.selected_build_targets.len() != plan.selected_build_target_count
    {
        return Err("stale_shadow_selection_plan_mismatch");
    }
    selection.shadow_context_digest = None;
    let selection_bytes =
        serde_json::to_vec(&selection).map_err(|_| "serialize_stale_shadow_selection_failed")?;
    let selected_tests = literal_file_bytes(&selection.selected_tests)?;
    let selected_build_targets = match plan.schema_version {
        1 => None,
        2 => Some(literal_file_bytes(&selection.selected_build_targets)?),
        _ => return Err("unsupported_stale_activation_schema"),
    };
    let selected_build_targets_digest = selected_build_targets
        .as_ref()
        .map(|targets| sha256(targets));
    if sha256(&selection_bytes) != plan.selection_receipt_digest
        || sha256(&selected_tests) != plan.selected_tests_digest
        || selected_build_targets_digest != plan.selected_build_targets_digest
    {
        return Err("stale_shadow_selection_digest_mismatch");
    }
    let payload = serde_json::to_vec(&ExecutionPayloadBinding {
        schema_version: plan.schema_version,
        repository: &plan.repository,
        pull_request: plan.pull_request,
        target: &plan.target,
        base_sha: &plan.base_sha,
        head_sha: &plan.head_sha,
        tree_sha: &plan.tree_sha,
        policy_digest: &plan.policy_digest,
        selection_receipt_digest: &plan.selection_receipt_digest,
        validation_contract_digest: &plan.validation_contract_digest,
        workflow_digest: &plan.workflow_digest,
        selected_tests_digest: &plan.selected_tests_digest,
        selected_tests: &selection.selected_tests,
        selected_build_targets_digest: plan.selected_build_targets_digest.as_deref(),
        selected_build_targets: selected_build_targets
            .as_ref()
            .map(|_| selection.selected_build_targets.as_slice()),
    })
    .map_err(|_| "serialize_stale_execution_payload_failed")?;
    if sha256(&payload) != plan.execution_payload_digest {
        return Err("stale_execution_payload_digest_mismatch");
    }
    Ok(())
}

fn literal_file_bytes(values: &[String]) -> Result<Vec<u8>, &'static str> {
    if values.is_empty()
        || values
            .iter()
            .any(|value| value.is_empty() || value.contains('\n'))
    {
        return Err("invalid_stale_literal_file_value");
    }
    let mut bytes = Vec::new();
    for value in values {
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(b'\n');
    }
    Ok(bytes)
}

/// Construct a stable rejection when the command boundary cannot safely read
/// the receipt directory or one of its entries.
#[must_use]
pub(crate) fn rejected_trial(
    identity: &TrialIdentity,
    activation_receipt: Option<String>,
    result_receipt_count: usize,
    result_receipt: Option<String>,
    reason: &str,
) -> TrialStatus {
    TrialStatus {
        schema_version: TRIAL_STATUS_SCHEMA_VERSION,
        state: TrialState::Rejected,
        repository: identity.repository.clone(),
        pull_request: identity.pull_request,
        target: identity.target.clone(),
        head_sha: identity.head_sha.clone(),
        activation_receipt,
        result_receipt_count,
        result_receipt,
        timing: None,
        shadow_disposition: None,
        reason: reason.to_owned(),
    }
}

fn disposition_name(disposition: super::StaleBaseShadowDisposition) -> &'static str {
    match disposition {
        super::StaleBaseShadowDisposition::Reused => "reused",
        super::StaleBaseShadowDisposition::Recomputed => "recomputed",
        super::StaleBaseShadowDisposition::Blocked => "blocked",
        super::StaleBaseShadowDisposition::Invalidated => "invalidated",
        super::StaleBaseShadowDisposition::FullRequired => "full_required",
    }
}

fn validate_identity(identity: &TrialIdentity) -> Result<(), &'static str> {
    if identity.pull_request == 0
        || identity.target.trim().is_empty()
        || !valid_repository(&identity.repository)
        || !valid_sha(&identity.head_sha)
    {
        return Err("invalid_trial_identity");
    }
    Ok(())
}

fn validate_activation(
    identity: &TrialIdentity,
    activation: &ActivationReceipt,
) -> Result<(), &'static str> {
    let plan = &activation.plan;
    if activation.schema_version != plan.schema_version
        || !matches!(plan.schema_version, 1 | 2)
        || activation.machine_mode != "shadow_compare"
    {
        return Err("invalid_shadow_activation_contract");
    }
    if plan.repository != identity.repository
        || plan.pull_request != identity.pull_request
        || plan.target != identity.target
        || plan.head_sha != identity.head_sha
    {
        return Err("shadow_activation_identity_mismatch");
    }
    if !valid_sha(&plan.base_sha)
        || !valid_sha(&plan.head_sha)
        || !valid_sha(&plan.tree_sha)
        || !valid_digest(&plan.policy_digest)
        || !valid_digest(&plan.validation_contract_digest)
        || !valid_digest(&plan.workflow_digest)
        || !valid_digest(&plan.selection_receipt_digest)
        || !valid_digest(&plan.selected_tests_digest)
        || !valid_optional_digest(plan.selected_build_targets_digest.as_deref())
        || !valid_digest(&plan.execution_payload_digest)
        || plan.selected_count == 0
    {
        return Err("invalid_shadow_activation_identity_or_digest");
    }
    if plan.schema_version == 1
        && (plan.stage != "test"
            || plan.selected_build_targets_digest.is_some()
            || plan.selected_build_target_count != 0)
    {
        return Err("invalid_shadow_activation_build_target_contract");
    }
    if plan.schema_version == 2
        && (plan.stage != "build_and_test"
            || plan.selected_build_targets_digest.is_none()
            || plan.selected_build_target_count == 0)
    {
        return Err("invalid_shadow_activation_build_target_contract");
    }
    Ok(())
}

fn validate_result(plan: &ActivationPlan, result: &ResultReceipt) -> Result<(), &'static str> {
    validate_result_binding(plan, result)?;
    if !result.full_authoritative {
        return Err("full_suite_not_authoritative");
    }
    if result.selected_returncode != 0
        || result.full_returncode != Some(0)
        || match plan.schema_version {
            1 => {
                result.selected_build_returncode.is_some() || result.full_build_returncode.is_some()
            }
            2 => {
                result.selected_build_returncode != Some(0)
                    || result.full_build_returncode != Some(0)
            }
            _ => true,
        }
    {
        return Err("shadow_result_nonzero_returncode");
    }
    if result.comparison_verdict != "matched_pass" || !result.graduation_eligible {
        return Err("shadow_result_not_matched_pass");
    }
    if plan.schema_version == 2 && timing(plan, result).is_none() {
        return Err("invalid_shadow_result_timing");
    }
    Ok(())
}

fn validate_result_binding(
    plan: &ActivationPlan,
    result: &ResultReceipt,
) -> Result<(), &'static str> {
    if result.schema_version != RESULT_RECEIPT_SCHEMA_VERSION {
        return Err("unsupported_shadow_result_schema");
    }
    if result.repository != plan.repository
        || result.pull_request != plan.pull_request
        || result.target != plan.target
        || result.base_sha != plan.base_sha
        || result.head_sha != plan.head_sha
        || result.tree_sha != plan.tree_sha
    {
        return Err("shadow_result_identity_mismatch");
    }
    if result.execution_payload_sha256 != plan.execution_payload_digest
        || result.policy_digest != plan.policy_digest
        || result.selection_receipt_digest != plan.selection_receipt_digest
        || result.validation_contract_digest != plan.validation_contract_digest
        || result.workflow_digest != plan.workflow_digest
        || result.selected_tests_digest != plan.selected_tests_digest
        || result.selected_build_targets_digest != plan.selected_build_targets_digest
        || result.selected_logical_count != plan.selected_count
        || result.selected_build_target_count != plan.selected_build_target_count
    {
        return Err("shadow_result_digest_or_count_mismatch");
    }
    Ok(())
}

fn timing(plan: &ActivationPlan, result: &ResultReceipt) -> Option<TrialTiming> {
    if plan.schema_version != 2 {
        return None;
    }
    let verification = valid_duration(result.verification_duration_seconds?)?;
    let selected_test = valid_duration(result.selected_duration_seconds?)?;
    let selected_build = valid_duration(result.selected_build_duration_seconds?)?;
    let full_test = valid_duration(result.full_duration_seconds?)?;
    let full_build_incremental = valid_duration(result.full_build_incremental_duration_seconds?)?;
    let reported_full_build = valid_duration(result.full_build_estimated_total_duration_seconds?)?;
    let full_build = valid_derived(selected_build + full_build_incremental)?;
    let tolerance = 1e-6_f64.max(full_build.abs() * 1e-9);
    if (reported_full_build - full_build).abs() > tolerance {
        return None;
    }
    let selected_total = valid_derived(verification + selected_build + selected_test)?;
    let full_total = valid_derived(full_build + full_test)?;
    let savings = valid_derived(full_total - selected_total)?;
    let speedup = if selected_total > 0.0 {
        Some(valid_derived(full_total / selected_total)?)
    } else {
        None
    };
    Some(TrialTiming {
        verification_duration_seconds: verification,
        selected_build_duration_seconds: selected_build,
        selected_test_duration_seconds: selected_test,
        selected_total_duration_seconds: selected_total,
        full_build_incremental_duration_seconds: full_build_incremental,
        full_build_estimated_total_duration_seconds: full_build,
        full_test_duration_seconds: full_test,
        full_estimated_total_duration_seconds: full_total,
        estimated_savings_seconds: savings,
        estimated_speedup_ratio: speedup,
    })
}

fn valid_duration(value: f64) -> Option<f64> {
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn valid_derived(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn reject(
    mut status: TrialStatus,
    result_receipt: Option<&str>,
    reason: &'static str,
) -> TrialStatus {
    status.state = TrialState::Rejected;
    status.result_receipt = result_receipt.map(str::to_owned);
    reason.clone_into(&mut status.reason);
    status
}

fn path_component(value: &str) -> String {
    let canonical: String = value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => byte as char,
            _ => '_',
        })
        .take(48)
        .collect();
    format!("{canonical}-{}", sha256(value.as_bytes()))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_optional_digest(value: Option<&str>) -> bool {
    value.is_none_or(valid_digest)
}

fn valid_repository(value: &str) -> bool {
    let mut parts = value.split('/');
    let (Some(owner), Some(repository), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !owner.is_empty()
        && !repository.is_empty()
        && owner
            .bytes()
            .chain(repository.bytes())
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DIGEST_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn identity() -> TrialIdentity {
        TrialIdentity {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            head_sha: SHA_A.to_owned(),
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
            },
            "original_test_command_sha256": DIGEST_C,
            "substituted_test_command_sha256": DIGEST_C
        })
    }

    fn result() -> Value {
        json!({
            "schema_version": 2,
            "recorded_at_unix_ns": 1,
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
            "selected_registration_count": 6,
            "full_registration_count": 20727,
            "verification_duration_seconds": 0.2,
            "selected_duration_seconds": 2.0,
            "selected_build_duration_seconds": 3.0,
            "full_duration_seconds": 20.0,
            "full_build_incremental_duration_seconds": 7.0,
            "full_build_estimated_total_duration_seconds": 10.0,
            "selected_build_target_count": 2,
            "selected_returncode": 0,
            "selected_build_returncode": 0,
            "full_returncode": 0,
            "full_build_returncode": 0,
            "full_authoritative": true,
            "failure_coverage": "no_failure_observed",
            "comparison_verdict": "matched_pass",
            "graduation_eligible": true
        })
    }

    fn evaluate(activation: Option<&Value>, results: &[Value]) -> TrialStatus {
        let activation_bytes = activation.map(|value| serde_json::to_vec(value).unwrap());
        let result_bytes = results
            .iter()
            .map(|value| serde_json::to_vec(value).unwrap())
            .collect::<Vec<_>>();
        let activation_file = activation_bytes.as_ref().map(|bytes| ReceiptFile {
            name: "activation-shadow_compare.json",
            bytes,
        });
        let names = (0..result_bytes.len())
            .map(|index| format!("result-{index}.json"))
            .collect::<Vec<_>>();
        let result_files = result_bytes
            .iter()
            .zip(&names)
            .map(|(bytes, name)| ReceiptFile { name, bytes })
            .collect::<Vec<_>>();
        evaluate_trial(&identity(), activation_file, &result_files)
    }

    #[test]
    fn missing_evidence_collects_without_claiming_readiness() {
        let missing_activation = evaluate(None, &[]);
        assert_eq!(missing_activation.state, TrialState::Collecting);
        assert_eq!(missing_activation.reason, "waiting_for_shadow_activation");

        let activation = activation();
        let missing_result = evaluate(Some(&activation), &[]);
        assert_eq!(missing_result.state, TrialState::Collecting);
        assert_eq!(missing_result.reason, "waiting_for_shadow_result");
    }

    #[test]
    fn exact_matched_pass_is_ready() {
        let status = evaluate(Some(&activation()), &[result()]);
        assert_eq!(status.state, TrialState::Ready);
        assert_eq!(status.reason, "matched_pass");
        assert_eq!(status.result_receipt.as_deref(), Some("result-0.json"));
        let timing = status.timing.expect("timing");
        assert!((timing.selected_total_duration_seconds - 5.2).abs() < f64::EPSILON);
        assert!((timing.full_estimated_total_duration_seconds - 30.0).abs() < f64::EPSILON);
        assert!((timing.estimated_savings_seconds - 24.8).abs() < f64::EPSILON);
        assert!(
            (timing.estimated_speedup_ratio.expect("speedup") - (30.0 / 5.2)).abs() < f64::EPSILON
        );
    }

    #[test]
    fn legacy_test_only_shadow_result_remains_verifiable() {
        let mut activation = activation();
        activation["schema_version"] = Value::from(1);
        activation["plan"]["schema_version"] = Value::from(1);
        activation["plan"]["stage"] = Value::String("test".to_owned());
        activation["plan"]["selected_build_targets_digest"] = Value::Null;
        activation["plan"]["selected_build_target_count"] = Value::from(0);
        let mut result = result();
        result["selected_build_targets_digest"] = Value::Null;
        result["selected_build_target_count"] = Value::from(0);
        result["selected_build_returncode"] = Value::Null;
        result["full_build_returncode"] = Value::Null;
        result["verification_duration_seconds"] = Value::Null;
        result["selected_duration_seconds"] = Value::Null;
        result["selected_build_duration_seconds"] = Value::Null;
        result["full_duration_seconds"] = Value::Null;
        result["full_build_incremental_duration_seconds"] = Value::Null;
        result["full_build_estimated_total_duration_seconds"] = Value::Null;
        assert_eq!(
            evaluate(Some(&activation), &[result]).state,
            TrialState::Ready
        );
    }

    #[test]
    fn identity_and_digest_drift_are_rejected() {
        let mut wrong_identity = result();
        wrong_identity["head_sha"] = Value::String(SHA_B.to_owned());
        assert_eq!(
            evaluate(Some(&activation()), &[wrong_identity]).reason,
            "shadow_result_identity_mismatch"
        );

        let mut wrong_digest = result();
        wrong_digest["workflow_digest"] = Value::String("d".repeat(64));
        assert_eq!(
            evaluate(Some(&activation()), &[wrong_digest]).reason,
            "shadow_result_digest_or_count_mismatch"
        );
    }

    #[test]
    fn selection_receipt_digest_not_unbound_path_duplicate_is_cross_receipt_authority() {
        let mut duplicate_drift = activation();
        duplicate_drift["plan"]["changed_paths_digest"] = Value::String("not-a-digest".to_owned());
        assert_eq!(
            evaluate(Some(&duplicate_drift), &[result()]).state,
            TrialState::Ready
        );

        let mut selection_drift = activation();
        selection_drift["plan"]["selection_receipt_digest"] = Value::String("d".repeat(64));
        assert_eq!(
            evaluate(Some(&selection_drift), &[result()]).reason,
            "shadow_result_digest_or_count_mismatch"
        );
    }

    #[test]
    fn selected_build_activation_requires_at_least_one_producer_target() {
        let mut empty_build = activation();
        empty_build["plan"]["selected_build_target_count"] = Value::from(0);
        assert_eq!(
            evaluate(Some(&empty_build), &[result()]).reason,
            "invalid_shadow_activation_build_target_contract"
        );
    }

    #[test]
    fn authority_returncodes_and_verdict_fail_closed() {
        let mut not_authoritative = result();
        not_authoritative["full_authoritative"] = Value::Bool(false);
        assert_eq!(
            evaluate(Some(&activation()), &[not_authoritative]).reason,
            "full_suite_not_authoritative"
        );

        let mut failed = result();
        failed["full_returncode"] = Value::from(1);
        assert_eq!(
            evaluate(Some(&activation()), &[failed]).reason,
            "shadow_result_nonzero_returncode"
        );

        let mut mismatch = result();
        mismatch["comparison_verdict"] = Value::String("mismatched_non_graduation".to_owned());
        mismatch["graduation_eligible"] = Value::Bool(false);
        assert_eq!(
            evaluate(Some(&activation()), &[mismatch]).reason,
            "shadow_result_not_matched_pass"
        );
    }

    #[test]
    fn selected_build_trial_requires_consistent_timing() {
        let mut missing = result();
        missing["selected_duration_seconds"] = Value::Null;
        assert_eq!(
            evaluate(Some(&activation()), &[missing]).reason,
            "invalid_shadow_result_timing"
        );

        let mut inconsistent = result();
        inconsistent["full_build_estimated_total_duration_seconds"] = Value::from(9.0);
        assert_eq!(
            evaluate(Some(&activation()), &[inconsistent]).reason,
            "invalid_shadow_result_timing"
        );

        let mut overflowing = result();
        overflowing["selected_build_duration_seconds"] = Value::from(f64::MAX);
        overflowing["full_build_incremental_duration_seconds"] = Value::from(f64::MAX);
        overflowing["full_build_estimated_total_duration_seconds"] = Value::from(f64::MAX);
        assert_eq!(
            evaluate(Some(&activation()), &[overflowing]).reason,
            "invalid_shadow_result_timing"
        );
    }

    #[test]
    fn multiple_append_only_receipts_are_ambiguous_even_when_equivalent() {
        let passing = result();
        let status = evaluate(Some(&activation()), &[passing.clone(), passing]);
        assert_eq!(status.state, TrialState::Rejected);
        assert_eq!(status.reason, "ambiguous_shadow_results");
        assert_eq!(status.result_receipt, None);
    }

    #[test]
    fn state_path_components_cannot_collide_after_sanitization() {
        let state = Path::new("/state");
        let mut first = identity();
        first.target = "mac/release".to_owned();
        let mut second = identity();
        second.target = "mac_release".to_owned();
        assert_ne!(
            result_directory(state, &first),
            result_directory(state, &second)
        );
    }
}
