//! Fail-closed promotion of an exact-head changed-surface plan into a bounded
//! test-stage command.
//!
//! Test identities are never joined into a regex or shell command. Shipyard
//! writes one literal name per line and substitutes only the generated file's
//! safely quoted absolute path into a protected-base command template.

use std::fmt::{Display, Formatter};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ChangedSurfacePolicy, PlannedSuite, SelectionReceipt, SelectionTier, policy_digest};

/// Stable placeholder accepted in a protected-base command template.
pub const SELECTED_TESTS_FILE_PLACEHOLDER: &str = "{selected_tests_file}";
/// Current bounded-execution planning receipt schema.
pub const AUTHORITATIVE_EXECUTION_PLAN_SCHEMA_VERSION: u32 = 1;

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
                if command.matches(SELECTED_TESTS_FILE_PLACEHOLDER).count() != 1 {
                    return Err(format!(
                        "authoritative command must contain exactly one {SELECTED_TESTS_FILE_PLACEHOLDER} placeholder"
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
    /// Generated absolute literal-test file.
    pub selected_tests_file: PathBuf,
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
    policy: &ChangedSurfacePolicy,
    machine_enabled: bool,
    state_dir: &Path,
    validation_contract_digest: &str,
    workflow_digest: &str,
) -> Result<ExecutionDisposition, ExecutionPlanError> {
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
    validate_digest("validation contract", validation_contract_digest)?;
    validate_digest("workflow", workflow_digest)?;
    let selected = literal_file_bytes(&receipt.selected_tests)?;
    let selection_receipt = serde_json::to_vec(receipt)
        .map_err(|failure| error(format!("serialize selection receipt: {failure}")))?;
    let selection_receipt_digest = sha256_hex(&selection_receipt);
    let selected_tests_file = selected_tests_path(state_dir, receipt, &selection_receipt_digest)?;
    let template = execution
        .command
        .as_deref()
        .ok_or_else(|| error("authoritative command is missing"))?;
    let command = template.replacen(
        SELECTED_TESTS_FILE_PLACEHOLDER,
        &posix_shell_quote(&selected_tests_file)?,
        1,
    );
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
            selected_tests_digest: sha256_hex(&selected),
            selection_tier: receipt.selection_tier,
            selected_count: receipt.selected_tests.len(),
            selected_tests_file,
            stage: execution.stage.clone(),
            command,
        },
    )))
}

/// Atomically publish the literal test file after the plan has been accepted.
pub fn materialize_selected_tests(
    plan: &AuthoritativeExecutionPlan,
    selected_tests: &[String],
) -> Result<(), ExecutionPlanError> {
    let bytes = literal_file_bytes(selected_tests)?;
    if selected_tests.len() != plan.selected_count
        || sha256_hex(&bytes) != plan.selected_tests_digest
    {
        return Err(error(
            "selected tests do not match the immutable execution plan",
        ));
    }
    let parent = plan
        .selected_tests_file
        .parent()
        .ok_or_else(|| error("selected-test file has no parent"))?;
    if plan.selected_tests_file.exists() {
        let existing = fs::read(&plan.selected_tests_file)
            .map_err(|failure| error(format!("read existing selected-test file: {failure}")))?;
        if existing == bytes {
            return Ok(());
        }
        return Err(error(
            "immutable selected-test path already contains different bytes",
        ));
    }
    fs::create_dir_all(parent)
        .map_err(|failure| error(format!("create selected-test directory: {failure}")))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|failure| error(format!("create selected-test temporary file: {failure}")))?;
    temporary
        .write_all(&bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|failure| error(format!("write selected-test file: {failure}")))?;
    match temporary.persist_noclobber(&plan.selected_tests_file) {
        Ok(_) => Ok(()),
        Err(failure) if failure.error.kind() == ErrorKind::AlreadyExists => {
            let existing = fs::read(&plan.selected_tests_file).map_err(|read_failure| {
                error(format!(
                    "read concurrently published selected-test file: {read_failure}"
                ))
            })?;
            if existing == bytes {
                Ok(())
            } else {
                Err(error(
                    "immutable selected-test path was concurrently published with different bytes",
                ))
            }
        }
        Err(failure) => Err(error(format!(
            "persist selected-test file: {}",
            failure.error
        ))),
    }
}

fn selected_tests_path(
    state_dir: &Path,
    receipt: &SelectionReceipt,
    selection_receipt_digest: &str,
) -> Result<PathBuf, ExecutionPlanError> {
    if !state_dir.is_absolute() {
        return Err(error(
            "state directory must be absolute for authoritative execution",
        ));
    }
    Ok(state_dir
        .join("changed-surface-execution")
        .join(percent_encode_component(&receipt.repository))
        .join(receipt.pull_request.to_string())
        .join(&receipt.head_sha)
        .join(percent_encode_component(&receipt.target))
        .join(selection_receipt_digest)
        .join("selected-tests.txt"))
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

fn posix_shell_quote(path: &Path) -> Result<String, ExecutionPlanError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(error("selected-test path contains a parent traversal"));
    }
    let value = path
        .to_str()
        .ok_or_else(|| error("selected-test path is not UTF-8"))?;
    if value.contains(['\n', '\r', '\0']) {
        return Err(error("selected-test path contains a shell line boundary"));
    }
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

fn percent_encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
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

    use super::*;
    use crate::changed_surface::{BuildType, ChangedSurfacePolicy, SelectionOutcomes, TestFamily};

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TREE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DIGEST: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

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
                    .then(|| "tools/run-selected --file {selected_tests_file}".to_owned()),
            }),
            secondary_contract_digests: BTreeMap::new(),
        }
    }

    fn fixture_receipt(policy: &ChangedSurfacePolicy) -> SelectionReceipt {
        SelectionReceipt {
            schema_version: 2,
            exact_head_verified: true,
            shadow_only: true,
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac".to_owned(),
            protected_ref: "main".to_owned(),
            pr_base_sha: SHA.to_owned(),
            protected_ref_sha: SHA.to_owned(),
            merge_base_sha: SHA.to_owned(),
            head_sha: SHA.to_owned(),
            tree_sha: TREE.to_owned(),
            changed_paths_digest: DIGEST.to_owned(),
            policy_digest: Some(policy_digest(policy)),
            build_type: Some(BuildType::Debug),
            build_flags: vec!["-DCMAKE_BUILD_TYPE=Debug".to_owned()],
            changed_paths: vec!["src/a.rs".to_owned()],
            selected_families: vec!["core".to_owned()],
            selected_tests: vec!["core exact".to_owned(), "smoke".to_owned()],
            baseline_tests: vec!["smoke".to_owned()],
            family_coverage: BTreeMap::from([("core".to_owned(), 1)]),
            secondary_proofs: Vec::new(),
            planned_suite: PlannedSuite::Bounded,
            selection_tier: SelectionTier::Affected,
            authoritative_suite: PlannedSuite::Full,
            outcomes: SelectionOutcomes {
                planner: "bounded".to_owned(),
                authoritative_execution: "not_observed_by_shadow_planner".to_owned(),
            },
            selected_count: Some(2),
            full_count: Some(10),
            fallback_reason: None,
            fallback_detail: None,
            elapsed_ms: 1,
        }
    }

    #[test]
    fn default_shadow_and_machine_kill_switch_keep_full_execution() {
        let shadow = fixture_policy(ExecutionMode::Shadow);
        assert_eq!(
            plan_authoritative_execution(
                &fixture_receipt(&shadow),
                &shadow,
                true,
                Path::new("/state"),
                DIGEST,
                DIGEST,
            )
            .expect("shadow"),
            ExecutionDisposition::Full {
                reason: FullExecutionReason::ShadowPolicy,
            }
        );
        let live = fixture_policy(ExecutionMode::Authoritative);
        assert_eq!(
            plan_authoritative_execution(
                &fixture_receipt(&live),
                &live,
                false,
                Path::new("/state"),
                DIGEST,
                DIGEST,
            )
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
            let policy = fixture_policy(mode);
            let mut receipt = fixture_receipt(&policy);
            receipt.planned_suite = PlannedSuite::Blocked;
            receipt.fallback_detail = Some("secondary proof missing".to_owned());
            assert_eq!(
                plan_authoritative_execution(
                    &receipt,
                    &policy,
                    machine_enabled,
                    Path::new("/state"),
                    DIGEST,
                    DIGEST,
                )
                .expect("blocked"),
                ExecutionDisposition::Blocked {
                    reason: "secondary proof missing".to_owned(),
                }
            );
        }
    }

    #[test]
    fn bounded_plan_binds_all_contracts_and_never_embeds_test_names() {
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let receipt = fixture_receipt(&policy);
        let ExecutionDisposition::Bounded(plan) = plan_authoritative_execution(
            &receipt,
            &policy,
            true,
            Path::new("/state with quote's"),
            DIGEST,
            DIGEST,
        )
        .expect("bounded") else {
            panic!("expected bounded plan");
        };
        assert_eq!(plan.head_sha, SHA);
        assert_eq!(plan.tree_sha, TREE);
        assert_eq!(plan.validation_contract_digest, DIGEST);
        assert_eq!(plan.workflow_digest, DIGEST);
        assert_eq!(plan.selected_count, 2);
        assert!(!plan.command.contains("core exact"));
        assert!(!plan.command.contains("smoke"));
        assert!(plan.command.contains("'\"'\"'"));
    }

    #[test]
    fn literal_file_is_atomic_and_bound_to_the_plan_digest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let receipt = fixture_receipt(&policy);
        let ExecutionDisposition::Bounded(plan) =
            plan_authoritative_execution(&receipt, &policy, true, temp.path(), DIGEST, DIGEST)
                .expect("bounded")
        else {
            panic!("expected bounded plan");
        };
        materialize_selected_tests(&plan, &receipt.selected_tests).expect("materialize");
        assert_eq!(
            fs::read_to_string(&plan.selected_tests_file).expect("read"),
            "core exact\nsmoke\n"
        );
        assert!(
            materialize_selected_tests(&plan, &["different".to_owned()]).is_err(),
            "a different test set must not overwrite the exact-bound file"
        );
        materialize_selected_tests(&plan, &receipt.selected_tests)
            .expect("identical publication is idempotent");
        fs::write(&plan.selected_tests_file, b"conflicting\n").expect("replace fixture");
        assert!(
            materialize_selected_tests(&plan, &receipt.selected_tests).is_err(),
            "an existing conflicting immutable file must be rejected"
        );
        assert_eq!(
            fs::read(&plan.selected_tests_file).expect("read conflicting fixture"),
            b"conflicting\n",
            "conflicting bytes must never be overwritten"
        );
    }

    #[test]
    fn malformed_policy_and_receipt_fail_closed() {
        let mut policy = fixture_policy(ExecutionMode::Authoritative);
        policy.execution.as_mut().expect("execution").command =
            Some("run {selected_tests_file} {selected_tests_file}".to_owned());
        assert!(
            policy
                .execution
                .as_ref()
                .expect("execution")
                .validate(2)
                .is_err()
        );

        let policy = fixture_policy(ExecutionMode::Authoritative);
        let mut receipt = fixture_receipt(&policy);
        receipt.policy_digest = Some(DIGEST.to_owned());
        assert!(
            plan_authoritative_execution(
                &receipt,
                &policy,
                true,
                Path::new("/state"),
                DIGEST,
                DIGEST,
            )
            .is_err()
        );
        let mut receipt = fixture_receipt(&policy);
        receipt.selected_tests.push("bad\nname".to_owned());
        assert!(
            plan_authoritative_execution(
                &receipt,
                &policy,
                true,
                Path::new("/state"),
                DIGEST,
                DIGEST,
            )
            .is_err()
        );
    }

    #[test]
    fn planner_full_and_blocked_are_not_promoted() {
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let mut full = fixture_receipt(&policy);
        full.planned_suite = PlannedSuite::Full;
        assert_eq!(
            plan_authoritative_execution(
                &full,
                &policy,
                true,
                Path::new("/state"),
                DIGEST,
                DIGEST,
            )
            .expect("full"),
            ExecutionDisposition::Full {
                reason: FullExecutionReason::PlannerSelectedFull,
            }
        );
        let mut blocked = fixture_receipt(&policy);
        blocked.planned_suite = PlannedSuite::Blocked;
        blocked.fallback_detail = Some("release proof missing".to_owned());
        assert_eq!(
            plan_authoritative_execution(
                &blocked,
                &policy,
                true,
                Path::new("/state"),
                DIGEST,
                DIGEST,
            )
            .expect("blocked"),
            ExecutionDisposition::Blocked {
                reason: "release proof missing".to_owned(),
            }
        );
    }
}
