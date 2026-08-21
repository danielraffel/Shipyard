//! Fail-closed promotion of an exact-head changed-surface plan into a bounded
//! test-stage command.
//!
//! Test identities are never joined into a regex or interpolated directly into
//! a shell command. Shipyard substitutes a bounded URL-safe base64 payload and
//! its SHA-256 into a protected-base command template; the repository adapter
//! owns private literal-file materialization at consumption time.

use std::fmt::{Display, Formatter};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ChangedSurfacePolicy, ExactHeadInput, PlannedSuite, SelectionReceipt, SelectionTier,
    plan_selection, policy_digest, verify_receipt_identity,
};

/// Stable URL-safe payload placeholder accepted in a protected-base command.
pub const SELECTED_TESTS_PAYLOAD_PLACEHOLDER: &str = "{selected_tests_b64}";
/// Stable payload-digest placeholder accepted in a protected-base command.
pub const SELECTED_TESTS_DIGEST_PLACEHOLDER: &str = "{selected_tests_digest}";
/// Current bounded-execution planning receipt schema.
pub const AUTHORITATIVE_EXECUTION_PLAN_SCHEMA_VERSION: u32 = 1;
/// Stay below the Windows `cmd.exe` command-line ceiling after base64 expansion;
/// larger selections fail closed to the ordinary full suite.
pub const MAX_SELECTED_TEST_BYTES: usize = 4 * 1024;

/// Protected-base execution posture.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Continue to execute the configured full suite.
    Shadow,
    /// Permit an eligible exact-head bounded plan to replace only the test stage.
    Authoritative,
}

/// Protected-base declaration controlling bounded execution for one target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangedSurfaceExecutionPolicy {
    /// Promotion posture. A machine-global kill switch is checked separately.
    pub mode: ExecutionMode,
    /// Only the canonical test stage may be replaced during the first rollout.
    #[serde(default = "default_test_stage")]
    pub stage: String,
    /// Base-owned command containing exactly one selected-file placeholder.
    #[serde(default)]
    pub command: Option<String>,
}

impl ChangedSurfaceExecutionPolicy {
    pub(super) fn validate(&self, schema_version: u32) -> Result<(), String> {
        if schema_version < 2 {
            return Err("changed-surface execution requires schema_version = 2".to_owned());
        }
        if self.stage != "test" {
            return Err("changed-surface execution may replace only the test stage".to_owned());
        }
        match self.mode {
            ExecutionMode::Shadow if self.command.is_some() => {
                Err("shadow changed-surface execution must not declare a command".to_owned())
            }
            ExecutionMode::Shadow => Ok(()),
            ExecutionMode::Authoritative => {
                let command = self.command.as_deref().unwrap_or_default();
                if command.trim().is_empty() {
                    return Err(
                        "authoritative changed-surface execution requires a command".to_owned()
                    );
                }
                if command.matches(SELECTED_TESTS_PAYLOAD_PLACEHOLDER).count() != 1
                    || command.matches(SELECTED_TESTS_DIGEST_PLACEHOLDER).count() != 1
                {
                    return Err(format!(
                        "authoritative command must contain exactly one {SELECTED_TESTS_PAYLOAD_PLACEHOLDER} and one {SELECTED_TESTS_DIGEST_PLACEHOLDER} placeholder"
                    ));
                }
                Ok(())
            }
        }
    }
}

fn default_test_stage() -> String {
    "test".to_owned()
}

/// Why the normal full suite remains authoritative.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FullExecutionReason {
    /// The protected-base policy remains shadow-only.
    ShadowPolicy,
    /// Trusted machine policy disabled bounded execution.
    MachineKillSwitch,
    /// The exact-head planner selected the full suite.
    PlannerSelectedFull,
}

/// Result of applying promotion policy to one exact-head selection receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "disposition")]
pub enum ExecutionDisposition {
    /// Execute the configured full validation contract unchanged.
    Full {
        /// Stable reason retained in orchestration telemetry.
        reason: FullExecutionReason,
    },
    /// The planner is waiting on a typed exact-head secondary proof.
    Blocked {
        /// Bounded diagnostic copied from the exact planner receipt.
        reason: String,
    },
    /// Replace only the target's test stage with this exact-bound command.
    Bounded(Box<AuthoritativeExecutionPlan>),
}

/// Immutable plan that must be carried into the eventual execution evidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AuthoritativeExecutionPlan {
    /// Plan schema version.
    pub schema_version: u32,
    /// Repository identity.
    pub repository: String,
    /// Pull request identity.
    pub pull_request: u64,
    /// Target whose protected-base policy authorized selection.
    pub target: String,
    /// Exact protected base SHA.
    pub base_sha: String,
    /// Exact PR head SHA.
    pub head_sha: String,
    /// Exact PR head tree SHA.
    pub tree_sha: String,
    /// Digest of the protected-base selector policy.
    pub policy_digest: String,
    /// Digest of the authenticated changed-path set.
    pub changed_paths_digest: String,
    /// Digest of the unmodified target validation contract.
    pub validation_contract_digest: String,
    /// Digest of the protected-base workflow contract.
    pub workflow_digest: String,
    /// Digest of the complete selection receipt used for promotion.
    pub selection_receipt_digest: String,
    /// Digest of the exact ordered literal test file.
    pub selected_tests_digest: String,
    /// Selected risk tier.
    pub selection_tier: SelectionTier,
    /// Number of literal test names written to the file.
    pub selected_count: usize,
    /// Canonical stage replaced by this plan.
    pub stage: String,
    /// Protected-base command with only the file path substituted.
    pub command: String,
}

/// Promotion error. Callers must fail closed to the full suite and retain the
/// diagnostic; they must never treat this as bounded success.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionPlanError(pub String);

impl Display for ExecutionPlanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ExecutionPlanError {}

/// Bind an eligible exact-head planner receipt to the command that may replace
/// one local test stage. This function performs no I/O.
pub fn plan_authoritative_execution(
    receipt: &SelectionReceipt,
    input: &ExactHeadInput,
    policy: &ChangedSurfacePolicy,
    machine_enabled: bool,
    validation_contract_digest: &str,
    workflow_digest: &str,
) -> Result<ExecutionDisposition, ExecutionPlanError> {
    let derived = rederive_receipt(receipt, input, policy)?;
    let receipt = &derived;
    if !receipt.exact_head_verified {
        return Err(error("selection receipt is not exact-head verified"));
    }
    if receipt.policy_digest.as_deref() != Some(&policy_digest(policy)) {
        return Err(error(
            "selection receipt policy digest does not match protected-base policy",
        ));
    }
    if !receipt.shadow_only {
        return Err(error(
            "selection receipt is not an original shadow planner receipt",
        ));
    }
    // A blocked plan means the ordinary full suite cannot prove an affected
    // family. Preserve that safety disposition independently of whether this
    // host or policy currently permits bounded execution.
    if receipt.planned_suite == PlannedSuite::Blocked {
        return Ok(ExecutionDisposition::Blocked {
            reason: receipt
                .fallback_detail
                .clone()
                .unwrap_or_else(|| "required exact-head secondary proof is unavailable".to_owned()),
        });
    }
    let Some(execution) = policy.execution.as_ref() else {
        return Ok(ExecutionDisposition::Full {
            reason: FullExecutionReason::ShadowPolicy,
        });
    };
    execution
        .validate(policy.schema_version)
        .map_err(ExecutionPlanError)?;
    if execution.mode == ExecutionMode::Shadow {
        return Ok(ExecutionDisposition::Full {
            reason: FullExecutionReason::ShadowPolicy,
        });
    }
    if !machine_enabled {
        return Ok(ExecutionDisposition::Full {
            reason: FullExecutionReason::MachineKillSwitch,
        });
    }
    match receipt.planned_suite {
        PlannedSuite::Full => {
            return Ok(ExecutionDisposition::Full {
                reason: FullExecutionReason::PlannerSelectedFull,
            });
        }
        PlannedSuite::Blocked => unreachable!("blocked plans return before policy fallbacks"),
        PlannedSuite::Bounded => {}
    }
    if receipt.authoritative_suite != PlannedSuite::Full {
        return Err(error(
            "planner receipt no longer records the full shadow authority",
        ));
    }
    bounded_execution_plan(
        receipt,
        execution,
        validation_contract_digest,
        workflow_digest,
    )
}

fn rederive_receipt(
    receipt: &SelectionReceipt,
    input: &ExactHeadInput,
    policy: &ChangedSurfacePolicy,
) -> Result<SelectionReceipt, ExecutionPlanError> {
    verify_receipt_identity(receipt, input)
        .map_err(|failure| error(format!("selection receipt identity is stale: {failure}")))?;
    let derived = plan_selection(input, Ok(policy.clone()))
        .map_err(|failure| error(format!("rederive exact-head selection: {failure}")))?;
    let mut supplied = receipt.clone();
    supplied.elapsed_ms = 0;
    if supplied != derived {
        return Err(error(
            "selection receipt fields differ from the rederived protected-base plan",
        ));
    }
    Ok(derived)
}

fn bounded_execution_plan(
    receipt: &SelectionReceipt,
    execution: &ChangedSurfaceExecutionPolicy,
    validation_contract_digest: &str,
    workflow_digest: &str,
) -> Result<ExecutionDisposition, ExecutionPlanError> {
    validate_digest("validation contract", validation_contract_digest)?;
    validate_digest("workflow", workflow_digest)?;
    let selected = literal_file_bytes(&receipt.selected_tests)?;
    if selected.len() > MAX_SELECTED_TEST_BYTES {
        return Err(error(
            "bounded selection payload exceeds the safe command limit",
        ));
    }
    let selection_receipt = serde_json::to_vec(receipt)
        .map_err(|failure| error(format!("serialize selection receipt: {failure}")))?;
    let selection_receipt_digest = sha256_hex(&selection_receipt);
    let selected_tests_digest = sha256_hex(&selected);
    let selected_tests_payload = URL_SAFE_NO_PAD.encode(&selected);
    let template = execution
        .command
        .as_deref()
        .ok_or_else(|| error("authoritative command is missing"))?;
    let command = template.replacen(
        SELECTED_TESTS_PAYLOAD_PLACEHOLDER,
        &selected_tests_payload,
        1,
    );
    let command = command.replacen(SELECTED_TESTS_DIGEST_PLACEHOLDER, &selected_tests_digest, 1);
    Ok(ExecutionDisposition::Bounded(Box::new(
        AuthoritativeExecutionPlan {
            schema_version: AUTHORITATIVE_EXECUTION_PLAN_SCHEMA_VERSION,
            repository: receipt.repository.clone(),
            pull_request: receipt.pull_request,
            target: receipt.target.clone(),
            base_sha: receipt.pr_base_sha.clone(),
            head_sha: receipt.head_sha.clone(),
            tree_sha: receipt.tree_sha.clone(),
            policy_digest: receipt
                .policy_digest
                .clone()
                .expect("matched policy digest"),
            changed_paths_digest: receipt.changed_paths_digest.clone(),
            validation_contract_digest: validation_contract_digest.to_owned(),
            workflow_digest: workflow_digest.to_owned(),
            selection_receipt_digest,
            selected_tests_digest,
            selection_tier: receipt.selection_tier,
            selected_count: receipt.selected_tests.len(),
            stage: execution.stage.clone(),
            command,
        },
    )))
}

fn literal_file_bytes(tests: &[String]) -> Result<Vec<u8>, ExecutionPlanError> {
    if tests.is_empty() {
        return Err(error(
            "bounded execution requires at least one literal test",
        ));
    }
    let mut bytes = Vec::new();
    for test in tests {
        if test.trim().is_empty() || test.contains(['\n', '\r', '\0']) {
            return Err(error("literal test names must be nonempty single lines"));
        }
        bytes.extend_from_slice(test.as_bytes());
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn validate_digest(label: &str, digest: &str) -> Result<(), ExecutionPlanError> {
    if digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(error(format!("{label} digest is missing or malformed")))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn error(detail: impl Into<String>) -> ExecutionPlanError {
    ExecutionPlanError(detail.into())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;

    use super::*;
    use crate::changed_surface::{
        BuildType, ChangedSurfacePolicy, ExactHeadInput, ObservationStatus, ProtectedRefStatus,
        TestFamily,
    };

    const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TREE: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const DIGEST: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn fixture_policy(mode: ExecutionMode) -> ChangedSurfacePolicy {
        ChangedSurfacePolicy {
            schema_version: 2,
            full_test_count: 10,
            build_type: BuildType::Debug,
            build_flags: vec!["-DCMAKE_BUILD_TYPE=Debug".to_owned()],
            baseline_tests: vec!["smoke".to_owned()],
            baseline_only_paths: vec!["docs/**".to_owned()],
            full_required_paths: vec!["CMakeLists.txt".to_owned()],
            policy_paths: vec!["policy.json".to_owned()],
            test_topology_paths: vec!["tests/**".to_owned()],
            families: vec![TestFamily {
                name: "core".to_owned(),
                paths: vec!["src/**".to_owned()],
                tests: vec!["core exact".to_owned()],
                risk_class: crate::changed_surface::RiskClass::Low,
                extended_tests: Vec::new(),
                supported_build_types: vec![BuildType::Debug],
                required_secondary_target: None,
                required_secondary_build_type: None,
            }],
            execution: Some(ChangedSurfaceExecutionPolicy {
                mode,
                stage: "test".to_owned(),
                command: (mode == ExecutionMode::Authoritative)
                    .then(|| {
                        "tools/run-selected --selection {selected_tests_b64} --sha256 {selected_tests_digest}"
                            .to_owned()
                    }),
            }),
            secondary_contract_digests: BTreeMap::new(),
        }
    }

    fn fixture_input(path: &str) -> ExactHeadInput {
        ExactHeadInput {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            observed_at: Utc::now(),
            base_ref: "main".to_owned(),
            pr_base_sha: BASE.to_owned(),
            protected_ref_sha: BASE.to_owned(),
            protected_ref_status: ProtectedRefStatus::Protected,
            pr_head_sha: HEAD.to_owned(),
            remote_tree_sha: TREE.to_owned(),
            local_head_sha: HEAD.to_owned(),
            local_tree_sha: TREE.to_owned(),
            local_merge_base_sha: BASE.to_owned(),
            remote_merge_base_sha: BASE.to_owned(),
            merge_base_is_ancestor: true,
            checkout_clean: true,
            remote_changed_paths: vec![path.to_owned()],
            remote_changed_paths_status: ObservationStatus::Complete,
            local_changed_paths: vec![path.to_owned()],
            local_changed_paths_status: ObservationStatus::Complete,
            base_tracked_paths: vec![
                "src/a.rs".to_owned(),
                "docs/guide.md".to_owned(),
                "CMakeLists.txt".to_owned(),
                ".shipyard/config.toml".to_owned(),
            ],
            base_tracked_paths_status: ObservationStatus::Complete,
            secondary_proofs: Vec::new(),
        }
    }

    fn fixture_receipt(policy: &ChangedSurfacePolicy, input: &ExactHeadInput) -> SelectionReceipt {
        plan_selection(input, Ok(policy.clone())).expect("fixture plan")
    }

    #[test]
    fn default_shadow_and_machine_kill_switch_keep_full_execution() {
        let input = fixture_input("src/a.rs");
        let shadow = fixture_policy(ExecutionMode::Shadow);
        let shadow_receipt = fixture_receipt(&shadow, &input);
        assert_eq!(
            plan_authoritative_execution(&shadow_receipt, &input, &shadow, true, DIGEST, DIGEST,)
                .expect("shadow"),
            ExecutionDisposition::Full {
                reason: FullExecutionReason::ShadowPolicy,
            }
        );
        let live = fixture_policy(ExecutionMode::Authoritative);
        let live_receipt = fixture_receipt(&live, &input);
        assert_eq!(
            plan_authoritative_execution(&live_receipt, &input, &live, false, DIGEST, DIGEST,)
                .expect("kill switch"),
            ExecutionDisposition::Full {
                reason: FullExecutionReason::MachineKillSwitch,
            }
        );
    }

    #[test]
    fn blocked_receipt_survives_shadow_policy_and_machine_kill_switch() {
        for (mode, machine_enabled) in [
            (ExecutionMode::Shadow, true),
            (ExecutionMode::Authoritative, false),
        ] {
            let mut policy = fixture_policy(mode);
            policy.families[0].supported_build_types = vec![BuildType::Release];
            policy.families[0].required_secondary_target = Some("release".to_owned());
            policy.families[0].required_secondary_build_type = Some(BuildType::Release);
            let input = fixture_input("src/a.rs");
            let receipt = fixture_receipt(&policy, &input);
            assert_eq!(receipt.planned_suite, PlannedSuite::Blocked);
            assert_eq!(
                plan_authoritative_execution(
                    &receipt,
                    &input,
                    &policy,
                    machine_enabled,
                    DIGEST,
                    DIGEST,
                )
                .expect("blocked"),
                ExecutionDisposition::Blocked {
                    reason: receipt.fallback_detail.expect("blocked detail"),
                }
            );
        }
    }

    #[test]
    fn bounded_plan_binds_all_contracts_and_never_embeds_test_names() {
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        let ExecutionDisposition::Bounded(plan) =
            plan_authoritative_execution(&receipt, &input, &policy, true, DIGEST, DIGEST)
                .expect("bounded")
        else {
            panic!("expected bounded plan");
        };
        assert_eq!(plan.head_sha, HEAD);
        assert_eq!(plan.tree_sha, TREE);
        assert_eq!(plan.validation_contract_digest, DIGEST);
        assert_eq!(plan.workflow_digest, DIGEST);
        assert_eq!(plan.selected_count, 2);
        assert!(!plan.command.contains("core exact"));
        assert!(!plan.command.contains("smoke"));
        assert!(!plan.command.contains(SELECTED_TESTS_PAYLOAD_PLACEHOLDER));
        assert!(!plan.command.contains(SELECTED_TESTS_DIGEST_PLACEHOLDER));
        assert!(plan.command.contains(&plan.selected_tests_digest));
    }

    #[test]
    fn literal_payload_is_cross_shell_safe_and_bound_to_the_plan_digest() {
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        let ExecutionDisposition::Bounded(plan) =
            plan_authoritative_execution(&receipt, &input, &policy, true, DIGEST, DIGEST)
                .expect("bounded")
        else {
            panic!("expected bounded plan");
        };
        let payload = plan
            .command
            .split_whitespace()
            .skip_while(|token| *token != "--selection")
            .nth(1)
            .expect("payload token");
        assert!(
            payload
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "payload must be safe as one token in POSIX and cmd shells"
        );
        let bytes = URL_SAFE_NO_PAD.decode(payload).expect("decode payload");
        assert_eq!(
            bytes,
            literal_file_bytes(&receipt.selected_tests).expect("literal bytes")
        );
        assert_eq!(sha256_hex(&bytes), plan.selected_tests_digest);
    }

    #[test]
    fn malformed_policy_and_receipt_fail_closed() {
        let mut policy = fixture_policy(ExecutionMode::Authoritative);
        policy.execution.as_mut().expect("execution").command =
            Some("run {selected_tests_b64} {selected_tests_b64}".to_owned());
        assert!(
            policy
                .execution
                .as_ref()
                .expect("execution")
                .validate(2)
                .is_err()
        );

        let policy = fixture_policy(ExecutionMode::Authoritative);
        let input = fixture_input("src/a.rs");
        let mut receipt = fixture_receipt(&policy, &input);
        receipt.policy_digest = Some(DIGEST.to_owned());
        assert!(
            plan_authoritative_execution(&receipt, &input, &policy, true, DIGEST, DIGEST,).is_err()
        );
        let mut receipt = fixture_receipt(&policy, &input);
        receipt.selected_tests.push("bad\nname".to_owned());
        assert!(
            plan_authoritative_execution(&receipt, &input, &policy, true, DIGEST, DIGEST,).is_err()
        );
    }

    #[test]
    fn planner_full_and_blocked_are_not_promoted() {
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let full_input = fixture_input("CMakeLists.txt");
        let full = fixture_receipt(&policy, &full_input);
        assert_eq!(full.planned_suite, PlannedSuite::Full);
        assert_eq!(
            plan_authoritative_execution(&full, &full_input, &policy, true, DIGEST, DIGEST,)
                .expect("full"),
            ExecutionDisposition::Full {
                reason: FullExecutionReason::PlannerSelectedFull,
            }
        );
        let mut blocked_policy = policy;
        blocked_policy.families[0].supported_build_types = vec![BuildType::Release];
        blocked_policy.families[0].required_secondary_target = Some("release".to_owned());
        blocked_policy.families[0].required_secondary_build_type = Some(BuildType::Release);
        let blocked_input = fixture_input("src/a.rs");
        let blocked = fixture_receipt(&blocked_policy, &blocked_input);
        assert_eq!(blocked.planned_suite, PlannedSuite::Blocked);
        let blocked_detail = blocked.fallback_detail.clone().expect("blocked detail");
        assert_eq!(
            plan_authoritative_execution(
                &blocked,
                &blocked_input,
                &blocked_policy,
                true,
                DIGEST,
                DIGEST,
            )
            .expect("blocked"),
            ExecutionDisposition::Blocked {
                reason: blocked_detail,
            }
        );
    }

    #[test]
    fn mutated_receipt_fields_and_identity_are_never_promoted() {
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        for mutate in [
            |receipt: &mut SelectionReceipt| receipt.selected_tests.clear(),
            |receipt: &mut SelectionReceipt| receipt.planned_suite = PlannedSuite::Full,
            |receipt: &mut SelectionReceipt| receipt.head_sha = "/tmp/escape".to_owned(),
        ] {
            let mut mutated = receipt.clone();
            mutate(&mut mutated);
            assert!(
                plan_authoritative_execution(&mutated, &input, &policy, true, DIGEST, DIGEST,)
                    .is_err()
            );
        }
    }

    #[test]
    fn oversized_cross_shell_payload_falls_back_before_command_construction() {
        let mut policy = fixture_policy(ExecutionMode::Authoritative);
        policy.baseline_tests = vec!["x".repeat(MAX_SELECTED_TEST_BYTES)];
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        assert_eq!(receipt.planned_suite, PlannedSuite::Bounded);
        assert!(
            plan_authoritative_execution(&receipt, &input, &policy, true, DIGEST, DIGEST,).is_err()
        );
    }
}
