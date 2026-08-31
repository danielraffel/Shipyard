//! Shadow-only changed-surface planning when the protected base has advanced.
//!
//! This module deliberately cannot produce merge-authoritative evidence. It
//! classifies whether an exact stale PR head can still produce useful bounded
//! comparison evidence against a fail-closed integration-tree observation.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ChangedSurfacePolicy, ExactHeadInput, FallbackReason, IdentityError, ObservationStatus,
    PlannedSuite, SelectionReceipt, base_receipt, normalized_paths, plan_with_policy,
    policy_digest, validate_identity, validated_policy,
};

/// Authority retained by every stale-base shadow result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeAuthority {
    /// A current authenticated merge-group (or separately verified equivalent)
    /// is still required before any result can satisfy a merge gate.
    BlockedUntilCurrentMergeTree,
}

/// Terminal disposition for one stale-base shadow trial.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleBaseShadowDisposition {
    /// An identical immutable shadow receipt can be reused.
    Reused,
    /// A bounded shadow selection was recomputed from the integration surface.
    Recomputed,
    /// Required observations were unavailable or incomplete.
    Blocked,
    /// A supplied receipt disagreed with the current exact identity.
    Invalidated,
    /// A material or ambiguous change requires the full validation contract.
    FullRequired,
}

/// Previously persisted stale-base receipt offered for exact reuse.
#[derive(Clone, Copy, Debug)]
pub struct StaleBaseCandidate<'a> {
    /// Receipt bytes decoded by the caller after safe regular-file checks.
    pub receipt: &'a StaleBaseShadowReceipt,
}

/// Complete observation needed to classify a stale-base shadow trial.
#[derive(Clone, Debug)]
pub struct StaleBaseShadowInput<'a> {
    /// Selector policy loaded from the old authenticated PR base.
    pub old_policy: Result<ChangedSurfacePolicy, String>,
    /// Selector policy loaded from the current protected base.
    pub live_policy: Result<ChangedSurfacePolicy, String>,
    /// Digest of the old protected-base workflow/config bytes.
    pub old_workflow_digest: String,
    /// Digest of the live protected-base workflow/config bytes.
    pub live_workflow_digest: String,
    /// Digest of the unmodified target validation contract.
    pub validation_contract_digest: String,
    /// Paths changed by `old_base..live_base`.
    pub protected_base_delta_paths: Vec<String>,
    /// Completeness of the protected-base delta observation.
    pub protected_base_delta_status: ObservationStatus,
    /// Paths changed by `live_base..integration_tree`.
    pub integration_changed_paths: Vec<String>,
    /// Completeness of the integration-tree changed-path observation.
    pub integration_changed_paths_status: ObservationStatus,
    /// Tree produced by a conflict-free integration of live base and exact head.
    pub integration_tree_sha: String,
    /// Deterministic synthetic commit whose tree is the exact integration tree.
    pub integration_commit_sha: String,
    /// Whether integration reported conflicts or an otherwise unusable tree.
    pub integration_conflicted: bool,
    /// Tracked paths in the live protected-base tree.
    pub live_base_tracked_paths: Vec<String>,
    /// Completeness of the live protected-base inventory.
    pub live_base_tracked_paths_status: ObservationStatus,
    /// Optional prior immutable receipt for exact reuse.
    pub candidate: Option<StaleBaseCandidate<'a>>,
}

/// Immutable, shadow-only assessment for one stale PR head.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StaleBaseShadowReceipt {
    /// Receipt schema version.
    pub schema_version: u32,
    /// Terminal stale-base shadow disposition.
    pub disposition: StaleBaseShadowDisposition,
    /// Explicit authority fence; never merge-authoritative.
    pub merge_authority: MergeAuthority,
    /// Canonical repository identity.
    pub repository: String,
    /// Pull request identity.
    pub pull_request: u64,
    /// Target whose policy produced the selection.
    pub target: String,
    /// Exact stale PR head.
    pub head_sha: String,
    /// Exact stale PR head tree.
    pub head_tree_sha: String,
    /// Base SHA reported by the PR.
    pub old_protected_base_sha: String,
    /// Current protected-base SHA.
    pub live_protected_base_sha: String,
    /// Merge base proved for the stale head.
    pub merge_base_sha: String,
    /// Conflict-free synthesized integration tree, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration_tree_sha: Option<String>,
    /// Deterministic synthetic commit for isolated execution, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration_commit_sha: Option<String>,
    /// Digest of the exact PR changed-path set.
    pub changed_paths_digest: String,
    /// Digest of the complete old-to-live protected-base delta.
    pub protected_base_delta_digest: String,
    /// Old base-owned selector policy digest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_policy_digest: Option<String>,
    /// Live base-owned selector policy digest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_policy_digest: Option<String>,
    /// Old workflow/config digest.
    pub old_workflow_digest: String,
    /// Live workflow/config digest.
    pub live_workflow_digest: String,
    /// Exact target validation-contract digest.
    pub validation_contract_digest: String,
    /// Digest of the recomputed integration changed-path set.
    pub integration_changed_paths_digest: String,
    /// Bounded selection derived from the live policy and integration surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_selection: Option<Box<SelectionReceipt>>,
    /// Stable machine-readable reason.
    pub reason: String,
}

/// Classify and, when safe, recompute bounded shadow evidence for a stale base.
///
/// This function has no authoritative execution return type. Its output is
/// explicitly fenced until a separate current integration-tree authority path
/// validates the result.
pub fn plan_stale_base_shadow(
    exact: &ExactHeadInput,
    input: &StaleBaseShadowInput<'_>,
) -> Result<StaleBaseShadowReceipt, IdentityError> {
    validate_identity(exact)?;
    let mut receipt = base_stale_receipt(exact, input);
    if exact.pr_base_sha == exact.protected_ref_sha {
        receipt.disposition = StaleBaseShadowDisposition::Invalidated;
        "base_is_not_stale".clone_into(&mut receipt.reason);
        return Ok(receipt);
    }
    if exact.protected_ref_status != super::ProtectedRefStatus::Protected {
        receipt.disposition = StaleBaseShadowDisposition::Blocked;
        "protected_base_unavailable".clone_into(&mut receipt.reason);
        return Ok(receipt);
    }
    if !valid_digest(&input.old_workflow_digest)
        || !valid_digest(&input.live_workflow_digest)
        || !valid_digest(&input.validation_contract_digest)
    {
        receipt.disposition = StaleBaseShadowDisposition::Blocked;
        "contract_digest_unavailable".clone_into(&mut receipt.reason);
        return Ok(receipt);
    }
    if input.protected_base_delta_status != ObservationStatus::Complete
        || input.integration_changed_paths_status != ObservationStatus::Complete
        || input.live_base_tracked_paths_status != ObservationStatus::Complete
        || exact.remote_changed_paths_status != ObservationStatus::Complete
        || exact.local_changed_paths_status != ObservationStatus::Complete
        || input.live_base_tracked_paths.is_empty()
    {
        receipt.disposition = StaleBaseShadowDisposition::Blocked;
        "incomplete_stale_base_observation".clone_into(&mut receipt.reason);
        return Ok(receipt);
    }
    if !exact.merge_base_is_ancestor
        || exact.local_merge_base_sha != exact.remote_merge_base_sha
        || exact.local_merge_base_sha != exact.pr_base_sha
        || normalized_paths(&exact.local_changed_paths)
            != normalized_paths(&exact.remote_changed_paths)
    {
        receipt.disposition = StaleBaseShadowDisposition::FullRequired;
        "ambiguous_stale_base_lineage_or_diff".clone_into(&mut receipt.reason);
        return Ok(receipt);
    }
    if input.integration_conflicted
        || !valid_sha(&input.integration_tree_sha)
        || !valid_sha(&input.integration_commit_sha)
    {
        receipt.disposition = StaleBaseShadowDisposition::FullRequired;
        "conflicting_or_unavailable_integration_tree".clone_into(&mut receipt.reason);
        return Ok(receipt);
    }
    receipt.integration_tree_sha = Some(input.integration_tree_sha.clone());
    receipt.integration_commit_sha = Some(input.integration_commit_sha.clone());

    let Some((old_policy, live_policy)) = validated_stale_policies(input, &mut receipt) else {
        return Ok(receipt);
    };
    let old_policy_digest = policy_digest(&old_policy);
    let live_policy_digest = policy_digest(&live_policy);
    receipt.old_policy_digest = Some(old_policy_digest.clone());
    receipt.live_policy_digest = Some(live_policy_digest.clone());
    if old_policy_digest != live_policy_digest
        || input.old_workflow_digest != input.live_workflow_digest
    {
        receipt.disposition = StaleBaseShadowDisposition::FullRequired;
        "selector_policy_or_workflow_drift".clone_into(&mut receipt.reason);
        return Ok(receipt);
    }

    let selection = recompute_selection(exact, input, live_policy)?;
    if selection.planned_suite != PlannedSuite::Bounded {
        receipt.disposition = StaleBaseShadowDisposition::FullRequired;
        match selection.fallback_reason {
            Some(FallbackReason::SelectorPolicyChanged) => "selector_policy_drift",
            Some(FallbackReason::TestTopologyChanged) => "test_topology_drift",
            Some(FallbackReason::FullRequiredSurface) => "full_required_surface",
            Some(FallbackReason::UnmappedChangedPath) => "unmapped_integration_surface",
            Some(FallbackReason::AmbiguousDiff) => "ambiguous_integration_surface",
            _ => "integration_selection_requires_full",
        }
        .clone_into(&mut receipt.reason);
        return Ok(receipt);
    }
    let mut selection = selection;
    selection.shadow_context_digest = Some(stale_base_context_digest(&receipt));
    receipt.shadow_selection = Some(Box::new(selection));
    receipt.disposition = StaleBaseShadowDisposition::Recomputed;
    "bounded_shadow_recomputed".clone_into(&mut receipt.reason);

    if let Some(candidate) = input.candidate {
        if candidate_matches(&receipt, candidate.receipt) {
            receipt.disposition = StaleBaseShadowDisposition::Reused;
            "exact_bound_shadow_receipt_reused".clone_into(&mut receipt.reason);
        } else {
            receipt.disposition = StaleBaseShadowDisposition::Invalidated;
            receipt.shadow_selection = None;
            "stale_shadow_receipt_identity_or_contract_mismatch".clone_into(&mut receipt.reason);
        }
    }
    Ok(receipt)
}

fn recompute_selection(
    exact: &ExactHeadInput,
    input: &StaleBaseShadowInput<'_>,
    live_policy: ChangedSurfacePolicy,
) -> Result<SelectionReceipt, IdentityError> {
    // Recompute against the cumulative old-base -> live-base -> integration
    // surface. Looking only at live-base..integration would miss a family,
    // producer, or topology change that landed while the PR was waiting.
    let combined_paths = normalized_paths(
        &input
            .protected_base_delta_paths
            .iter()
            .chain(&input.integration_changed_paths)
            .cloned()
            .collect::<Vec<_>>(),
    );
    let synthetic = integration_exact_input(exact, input);
    plan_with_policy(
        base_receipt(&synthetic, combined_paths.clone()),
        Ok(live_policy),
        &combined_paths,
        &synthetic,
    )
}

pub(crate) fn integration_exact_input(
    exact: &ExactHeadInput,
    input: &StaleBaseShadowInput<'_>,
) -> ExactHeadInput {
    let combined_paths = normalized_paths(
        &input
            .protected_base_delta_paths
            .iter()
            .chain(&input.integration_changed_paths)
            .cloned()
            .collect::<Vec<_>>(),
    );
    let mut synthetic = exact.clone();
    synthetic.pr_base_sha.clone_from(&exact.protected_ref_sha);
    synthetic
        .local_merge_base_sha
        .clone_from(&exact.protected_ref_sha);
    synthetic
        .remote_merge_base_sha
        .clone_from(&exact.protected_ref_sha);
    synthetic.remote_changed_paths.clone_from(&combined_paths);
    synthetic.local_changed_paths.clone_from(&combined_paths);
    synthetic.base_tracked_paths = normalized_paths(&input.live_base_tracked_paths);
    synthetic.base_tracked_paths_status = ObservationStatus::Complete;
    synthetic
        .pr_head_sha
        .clone_from(&input.integration_commit_sha);
    synthetic
        .local_head_sha
        .clone_from(&input.integration_commit_sha);
    synthetic
        .remote_tree_sha
        .clone_from(&input.integration_tree_sha);
    synthetic
        .local_tree_sha
        .clone_from(&input.integration_tree_sha);
    // Exact-head secondary proofs cannot cross into a distinct integration
    // commit; affected families requiring one remain blocked until re-proved.
    synthetic.secondary_proofs.clear();
    synthetic
}

fn validated_stale_policies(
    input: &StaleBaseShadowInput<'_>,
    receipt: &mut StaleBaseShadowReceipt,
) -> Option<(ChangedSurfacePolicy, ChangedSurfacePolicy)> {
    let Ok(old_policy) = validated_policy(input.old_policy.clone()) else {
        receipt.disposition = StaleBaseShadowDisposition::FullRequired;
        "old_selector_policy_invalid".clone_into(&mut receipt.reason);
        return None;
    };
    let Ok(live_policy) = validated_policy(input.live_policy.clone()) else {
        receipt.disposition = StaleBaseShadowDisposition::FullRequired;
        "live_selector_policy_invalid".clone_into(&mut receipt.reason);
        return None;
    };
    Some((old_policy, live_policy))
}

fn base_stale_receipt(
    exact: &ExactHeadInput,
    input: &StaleBaseShadowInput<'_>,
) -> StaleBaseShadowReceipt {
    let changed_paths = normalized_paths(&exact.remote_changed_paths);
    let protected_delta = normalized_paths(&input.protected_base_delta_paths);
    let integration_paths = normalized_paths(&input.integration_changed_paths);
    StaleBaseShadowReceipt {
        schema_version: 1,
        disposition: StaleBaseShadowDisposition::Blocked,
        merge_authority: MergeAuthority::BlockedUntilCurrentMergeTree,
        repository: exact.repository.clone(),
        pull_request: exact.pull_request,
        target: exact.target.clone(),
        head_sha: exact.pr_head_sha.clone(),
        head_tree_sha: exact.remote_tree_sha.clone(),
        old_protected_base_sha: exact.pr_base_sha.clone(),
        live_protected_base_sha: exact.protected_ref_sha.clone(),
        merge_base_sha: exact.local_merge_base_sha.clone(),
        integration_tree_sha: None,
        integration_commit_sha: None,
        changed_paths_digest: paths_digest(&changed_paths),
        protected_base_delta_digest: paths_digest(&protected_delta),
        old_policy_digest: None,
        live_policy_digest: None,
        old_workflow_digest: input.old_workflow_digest.clone(),
        live_workflow_digest: input.live_workflow_digest.clone(),
        validation_contract_digest: input.validation_contract_digest.clone(),
        integration_changed_paths_digest: paths_digest(&integration_paths),
        shadow_selection: None,
        reason: "classification_pending".to_owned(),
    }
}

fn candidate_matches(current: &StaleBaseShadowReceipt, candidate: &StaleBaseShadowReceipt) -> bool {
    candidate.schema_version == current.schema_version
        && candidate.merge_authority == current.merge_authority
        && candidate.repository == current.repository
        && candidate.pull_request == current.pull_request
        && candidate.target == current.target
        && candidate.head_sha == current.head_sha
        && candidate.head_tree_sha == current.head_tree_sha
        && candidate.old_protected_base_sha == current.old_protected_base_sha
        && candidate.live_protected_base_sha == current.live_protected_base_sha
        && candidate.merge_base_sha == current.merge_base_sha
        && candidate.integration_tree_sha == current.integration_tree_sha
        && candidate.integration_commit_sha == current.integration_commit_sha
        && candidate.changed_paths_digest == current.changed_paths_digest
        && candidate.protected_base_delta_digest == current.protected_base_delta_digest
        && candidate.old_policy_digest == current.old_policy_digest
        && candidate.live_policy_digest == current.live_policy_digest
        && candidate.old_workflow_digest == current.old_workflow_digest
        && candidate.live_workflow_digest == current.live_workflow_digest
        && candidate.validation_contract_digest == current.validation_contract_digest
        && candidate.integration_changed_paths_digest == current.integration_changed_paths_digest
        && candidate.shadow_selection == current.shadow_selection
        && matches!(
            candidate.disposition,
            StaleBaseShadowDisposition::Recomputed | StaleBaseShadowDisposition::Reused
        )
}

fn paths_digest(paths: &[String]) -> String {
    let mut hasher = Sha256::new();
    for path in paths {
        hasher.update(path.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

pub(crate) fn stale_base_context_digest(receipt: &StaleBaseShadowReceipt) -> String {
    let mut context = receipt.clone();
    context.disposition = StaleBaseShadowDisposition::Recomputed;
    context.shadow_selection = None;
    context.reason.clear();
    let bytes = serde_json::to_vec(&context).expect("stale shadow context must serialize");
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;

    use super::*;
    use crate::changed_surface::trial::{
        ReceiptFile, TrialIdentity, TrialState, evaluate_stale_base_execution,
        evaluate_stale_base_terminal,
    };
    use crate::changed_surface::{
        BuildType, ChangedSurfaceExecutionPolicy, ExecutionCommandTransport, ExecutionDisposition,
        ExecutionMode, ProtectedRefStatus, RiskClass, TestFamily, plan_authoritative_execution,
    };

    const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const LIVE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const TREE: &str = "dddddddddddddddddddddddddddddddddddddddd";
    const INTEGRATION: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const DIGEST: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    fn policy() -> ChangedSurfacePolicy {
        ChangedSurfacePolicy {
            schema_version: 2,
            full_test_count: 100,
            build_type: BuildType::Debug,
            build_flags: vec!["-DCMAKE_BUILD_TYPE=Debug".to_owned()],
            baseline_tests: vec!["smoke".to_owned()],
            baseline_build_targets: vec!["smoke-target".to_owned()],
            baseline_only_paths: vec!["docs/**".to_owned()],
            ios_compile_skip_safe_paths: Vec::new(),
            full_required_paths: vec!["toolchain/**".to_owned()],
            policy_paths: vec!["policy/**".to_owned()],
            test_topology_paths: vec!["tests/CMakeLists.txt".to_owned()],
            families: vec![
                TestFamily {
                    name: "audio".to_owned(),
                    paths: vec!["src/audio/**".to_owned()],
                    tests: vec!["audio exact".to_owned()],
                    build_targets: vec!["audio-tests".to_owned()],
                    risk_class: RiskClass::Low,
                    extended_tests: Vec::new(),
                    supported_build_types: vec![BuildType::Debug],
                    required_secondary_target: None,
                    required_secondary_build_type: None,
                },
                TestFamily {
                    name: "ui".to_owned(),
                    paths: vec!["src/ui/**".to_owned()],
                    tests: vec!["ui exact".to_owned()],
                    build_targets: vec!["ui-tests".to_owned()],
                    risk_class: RiskClass::Low,
                    extended_tests: Vec::new(),
                    supported_build_types: vec![BuildType::Debug],
                    required_secondary_target: None,
                    required_secondary_build_type: None,
                },
            ],
            execution: None,
            secondary_contract_digests: BTreeMap::new(),
        }
    }

    fn exact() -> ExactHeadInput {
        ExactHeadInput {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            observed_at: Utc::now(),
            base_ref: "main".to_owned(),
            pr_base_sha: OLD.to_owned(),
            protected_ref_sha: LIVE.to_owned(),
            protected_ref_status: ProtectedRefStatus::Protected,
            pr_head_sha: HEAD.to_owned(),
            remote_tree_sha: TREE.to_owned(),
            local_head_sha: HEAD.to_owned(),
            local_tree_sha: TREE.to_owned(),
            local_merge_base_sha: OLD.to_owned(),
            remote_merge_base_sha: OLD.to_owned(),
            merge_base_is_ancestor: true,
            checkout_clean: true,
            remote_changed_paths: vec!["src/audio/change.cpp".to_owned()],
            remote_changed_paths_status: ObservationStatus::Complete,
            local_changed_paths: vec!["src/audio/change.cpp".to_owned()],
            local_changed_paths_status: ObservationStatus::Complete,
            base_tracked_paths: vec!["src/audio/change.cpp".to_owned()],
            base_tracked_paths_status: ObservationStatus::Complete,
            secondary_proofs: Vec::new(),
        }
    }

    fn context<'a>() -> StaleBaseShadowInput<'a> {
        StaleBaseShadowInput {
            old_policy: Ok(policy()),
            live_policy: Ok(policy()),
            old_workflow_digest: DIGEST.to_owned(),
            live_workflow_digest: DIGEST.to_owned(),
            validation_contract_digest: DIGEST.to_owned(),
            protected_base_delta_paths: vec!["docs/note.md".to_owned()],
            protected_base_delta_status: ObservationStatus::Complete,
            integration_changed_paths: vec!["src/audio/change.cpp".to_owned()],
            integration_changed_paths_status: ObservationStatus::Complete,
            integration_tree_sha: INTEGRATION.to_owned(),
            integration_commit_sha: "1".repeat(40),
            integration_conflicted: false,
            live_base_tracked_paths: vec![
                ".shipyard/config.toml".to_owned(),
                "src/audio/change.cpp".to_owned(),
                "src/ui/view.cpp".to_owned(),
                "tests/CMakeLists.txt".to_owned(),
            ],
            live_base_tracked_paths_status: ObservationStatus::Complete,
            candidate: None,
        }
    }

    #[test]
    fn harmless_base_movement_recomputes_shadow_only() {
        let receipt = plan_stale_base_shadow(&exact(), &context()).expect("assessment");
        assert_eq!(receipt.disposition, StaleBaseShadowDisposition::Recomputed);
        assert_eq!(
            receipt.merge_authority,
            MergeAuthority::BlockedUntilCurrentMergeTree
        );
        let selection = receipt.shadow_selection.as_deref().expect("selection");
        assert_eq!(selection.planned_suite, PlannedSuite::Bounded);
        assert_eq!(selection.selected_families, ["audio"]);
        assert_eq!(selection.pr_base_sha, LIVE);

        let bytes = serde_json::to_vec(&receipt).expect("receipt bytes");
        let status = evaluate_stale_base_terminal(
            &TrialIdentity {
                repository: "owner/repo".to_owned(),
                pull_request: 42,
                target: "mac".to_owned(),
                head_sha: HEAD.to_owned(),
            },
            ReceiptFile {
                name: "stale-base-shadow.json",
                bytes: &bytes,
            },
        );
        assert_eq!(status.state, TrialState::Terminal);
        assert_eq!(
            status.shadow_disposition,
            Some(StaleBaseShadowDisposition::Recomputed)
        );
    }

    #[test]
    fn affected_family_movement_recomputes_union_surface() {
        let mut input = context();
        input.protected_base_delta_paths = vec!["src/ui/base.cpp".to_owned()];
        let receipt = plan_stale_base_shadow(&exact(), &input).expect("assessment");
        let selection = receipt.shadow_selection.expect("selection");
        assert_eq!(selection.selected_families, ["audio", "ui"]);
        assert!(selection.selected_tests.contains(&"ui exact".to_owned()));
    }

    #[test]
    fn recomputed_selection_can_only_execute_as_exact_integration_identity() {
        let mut input = context();
        let mut live_policy = policy();
        live_policy.execution = Some(ChangedSurfaceExecutionPolicy {
            mode: ExecutionMode::Authoritative,
            stage: "test".to_owned(),
            command: Some(format!(
                "adapter {} {}",
                crate::changed_surface::execution::SELECTED_TESTS_PAYLOAD_PLACEHOLDER,
                crate::changed_surface::execution::SELECTED_TESTS_DIGEST_PLACEHOLDER
            )),
        });
        input.old_policy = Ok(live_policy.clone());
        input.live_policy = Ok(live_policy.clone());
        let receipt = plan_stale_base_shadow(&exact(), &input).expect("assessment");
        let mut selection = receipt.shadow_selection.as_deref().unwrap().clone();
        assert!(selection.shadow_context_digest.is_some());
        selection.shadow_context_digest = None;
        let integration_input = integration_exact_input(&exact(), &input);
        let ExecutionDisposition::Bounded(plan) = plan_authoritative_execution(
            &selection,
            &integration_input,
            &live_policy,
            true,
            ExecutionCommandTransport::PosixShell,
            DIGEST,
            DIGEST,
        )
        .expect("integration execution plan") else {
            panic!("expected bounded integration plan");
        };
        assert_eq!(plan.head_sha, input.integration_commit_sha);
        assert_eq!(plan.tree_sha, input.integration_tree_sha);
        assert_eq!(plan.base_sha, LIVE);
        assert_ne!(plan.head_sha, HEAD);

        let stale_bytes = serde_json::to_vec(&receipt).unwrap();
        let activation = serde_json::json!({
            "schema_version": plan.schema_version,
            "machine_mode": "shadow_compare",
            "merge_authority": "blocked_until_current_merge_tree",
            "stale_context_digest": stale_base_context_digest(&receipt),
            "stale_receipt_sha256": format!("{:x}", Sha256::digest(&stale_bytes)),
            "plan": plan,
        });
        let result = serde_json::json!({
            "schema_version": 2,
            "repository": plan.repository,
            "pull_request": plan.pull_request,
            "target": plan.target,
            "base_sha": plan.base_sha,
            "head_sha": plan.head_sha,
            "tree_sha": plan.tree_sha,
            "execution_payload_sha256": plan.execution_payload_digest,
            "policy_digest": plan.policy_digest,
            "selection_receipt_digest": plan.selection_receipt_digest,
            "validation_contract_digest": plan.validation_contract_digest,
            "workflow_digest": plan.workflow_digest,
            "selected_tests_digest": plan.selected_tests_digest,
            "selected_build_targets_digest": plan.selected_build_targets_digest,
            "selected_logical_count": plan.selected_count,
            "selected_build_target_count": plan.selected_build_target_count,
            "selected_returncode": 0,
            "selected_build_returncode": null,
            "full_returncode": null,
            "full_build_returncode": null,
            "full_authoritative": false,
            "comparison_verdict": "not_compared",
            "graduation_eligible": false
        });
        let activation_bytes = serde_json::to_vec(&activation).unwrap();
        let result_bytes = serde_json::to_vec(&result).unwrap();
        let status = evaluate_stale_base_execution(
            &TrialIdentity {
                repository: "owner/repo".to_owned(),
                pull_request: 42,
                target: "mac".to_owned(),
                head_sha: HEAD.to_owned(),
            },
            ReceiptFile {
                name: "stale-base-shadow.json",
                bytes: &stale_bytes,
            },
            ReceiptFile {
                name: "stale-activation-shadow_compare.json",
                bytes: &activation_bytes,
            },
            &[ReceiptFile {
                name: "result.json",
                bytes: &result_bytes,
            }],
        );
        assert_eq!(status.state, TrialState::Terminal);
        assert_eq!(status.reason, "stale_base_recomputed_selected_pass");
    }

    #[test]
    fn policy_topology_toolchain_and_unmapped_drift_require_full() {
        for path in [
            "policy/selector.toml",
            "tests/CMakeLists.txt",
            "toolchain/compiler.cmake",
            "unknown/new.surface",
        ] {
            let mut input = context();
            input.protected_base_delta_paths = vec![path.to_owned()];
            input.integration_changed_paths =
                vec!["src/audio/change.cpp".to_owned(), path.to_owned()];
            let receipt = plan_stale_base_shadow(&exact(), &input).expect("assessment");
            assert_eq!(
                receipt.disposition,
                StaleBaseShadowDisposition::FullRequired,
                "{path}"
            );
            assert!(receipt.shadow_selection.is_none(), "{path}");
        }

        let mut drift = context();
        drift
            .live_policy
            .as_mut()
            .unwrap()
            .build_flags
            .push("-DNEW=1".to_owned());
        assert_eq!(
            plan_stale_base_shadow(&exact(), &drift)
                .expect("assessment")
                .reason,
            "selector_policy_or_workflow_drift"
        );

        let mut producer_drift = context();
        producer_drift.live_policy.as_mut().unwrap().families[0]
            .build_targets
            .push("new-audio-producer".to_owned());
        assert_eq!(
            plan_stale_base_shadow(&exact(), &producer_drift)
                .expect("assessment")
                .reason,
            "selector_policy_or_workflow_drift"
        );

        let mut workflow_drift = context();
        workflow_drift.live_workflow_digest = "0".repeat(64);
        assert_eq!(
            plan_stale_base_shadow(&exact(), &workflow_drift)
                .expect("assessment")
                .reason,
            "selector_policy_or_workflow_drift"
        );
    }

    #[test]
    fn incomplete_or_conflicting_observations_fail_closed() {
        let mut incomplete = context();
        incomplete.protected_base_delta_status = ObservationStatus::Incomplete;
        assert_eq!(
            plan_stale_base_shadow(&exact(), &incomplete)
                .expect("assessment")
                .disposition,
            StaleBaseShadowDisposition::Blocked
        );

        let mut conflict = context();
        conflict.integration_conflicted = true;
        assert_eq!(
            plan_stale_base_shadow(&exact(), &conflict)
                .expect("assessment")
                .disposition,
            StaleBaseShadowDisposition::FullRequired
        );

        let mut divergent = exact();
        divergent.remote_merge_base_sha = LIVE.to_owned();
        assert_eq!(
            plan_stale_base_shadow(&divergent, &context())
                .expect("assessment")
                .reason,
            "ambiguous_stale_base_lineage_or_diff"
        );

        let mut truncated = exact();
        truncated.remote_changed_paths_status = ObservationStatus::Incomplete;
        assert_eq!(
            plan_stale_base_shadow(&truncated, &context())
                .expect("assessment")
                .disposition,
            StaleBaseShadowDisposition::Blocked
        );
    }

    #[test]
    fn exact_candidate_reuses_but_wrong_head_tree_or_contract_invalidates() {
        let first = plan_stale_base_shadow(&exact(), &context()).expect("first");
        let mut reuse = context();
        reuse.candidate = Some(StaleBaseCandidate { receipt: &first });
        assert_eq!(
            plan_stale_base_shadow(&exact(), &reuse)
                .expect("reuse")
                .disposition,
            StaleBaseShadowDisposition::Reused
        );

        for mutation in ["head", "tree", "contract"] {
            let mut replay = first.clone();
            match mutation {
                "head" => replay.head_sha = OLD.to_owned(),
                "tree" => replay.head_tree_sha = OLD.to_owned(),
                "contract" => replay.validation_contract_digest = "0".repeat(64),
                _ => unreachable!(),
            }
            let mut input = context();
            input.candidate = Some(StaleBaseCandidate { receipt: &replay });
            assert_eq!(
                plan_stale_base_shadow(&exact(), &input)
                    .expect("invalidated")
                    .disposition,
                StaleBaseShadowDisposition::Invalidated,
                "{mutation}"
            );
        }
    }

    #[test]
    fn integration_tree_surface_controls_selection_and_semantic_authority_stays_blocked() {
        let mut input = context();
        input.integration_changed_paths = vec!["src/ui/base.cpp".to_owned()];
        let receipt = plan_stale_base_shadow(&exact(), &input).expect("assessment");
        let selection = receipt.shadow_selection.expect("selection");
        assert_eq!(selection.selected_families, ["ui"]);
        assert_eq!(
            receipt.merge_authority,
            MergeAuthority::BlockedUntilCurrentMergeTree
        );
    }
}
