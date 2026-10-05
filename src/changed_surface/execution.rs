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
pub const SELECTED_TESTS_PAYLOAD_PLACEHOLDER: &str = "{selection_receipt_b64}";
/// Stable payload-digest placeholder accepted in a protected-base command.
pub const SELECTED_TESTS_DIGEST_PLACEHOLDER: &str = "{selection_receipt_digest}";
/// Current bounded-execution planning receipt schema. Schema 3 names a keyed
/// plan's executable-reuse binding by digest (`executable_reuse_sha256`); the
/// binding is [`EXECUTABLE_REUSE_BINDING_FILE`] beside the activation.
pub const AUTHORITATIVE_EXECUTION_PLAN_SCHEMA_VERSION: u32 = 3;
/// The largest decoded execution payload, the same bytes and the same limit
/// the repository adapter enforces. A payload of P bytes costs about 4P/3
/// base64 units plus the fixed command parts, so 5,632 B stays under
/// [`MAX_EXECUTION_COMMAND_UNITS`] with margin; that command check remains the
/// authoritative shell bound. A larger selection is not executed bounded: the
/// configured stages run as usual, with a planning diagnostic.
pub const MAX_SELECTED_TEST_BYTES: usize = 5632;
/// File a keyed plan's executable-reuse binding is written to, in the plan's
/// evidence directory; the payload carries the sha256 of exactly its bytes.
pub const EXECUTABLE_REUSE_BINDING_FILE: &str = "executable-reuse-binding.json";
/// Conservative ceiling below `cmd.exe`'s 8,191 UTF-16-code-unit limit.
pub const MAX_EXECUTION_COMMAND_UNITS: usize = 8_000;

/// Protected-base execution posture.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Continue to execute the configured full suite.
    Shadow,
    /// Permit an eligible exact-head bounded plan to replace only the test stage.
    Authoritative,
}

/// Shell/transport contract that will consume the expanded protected command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionCommandTransport {
    /// A local or SSH POSIX shell whose final command uses the declared bound.
    PosixShell,
    /// Windows PowerShell/OpenSSH re-encodes the command and is not yet eligible.
    WindowsEncoded,
    /// A backend whose final command representation is not proven here.
    Unsupported,
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
        let expected_stage = if schema_version >= 3 {
            "build_and_test"
        } else {
            "test"
        };
        if self.stage != expected_stage {
            return Err(format!(
                "changed-surface schema {schema_version} requires stage = {expected_stage}"
            ));
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
    /// The eventual backend command encoding is not eligible for bounded execution.
    UnsupportedTransport,
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
    /// Exact base the selection was planned against: the protected base, or
    /// the head's merge base when the head is behind its recorded base.
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
    /// Digest of the exact ordered `CMake` producer-target file, when selected builds apply.
    pub selected_build_targets_digest: Option<String>,
    /// Digest of the exact identity-bound payload consumed by the adapter.
    pub execution_payload_digest: String,
    /// Selected risk tier.
    pub selection_tier: SelectionTier,
    /// Number of literal test names written to the file.
    pub selected_count: usize,
    /// Number of selected `CMake` producer targets.
    pub selected_build_target_count: usize,
    /// Canonical stage replaced by this plan.
    pub stage: String,
    /// Protected-base command with only the file path substituted.
    pub command: String,
    /// `bounded`, `keyed_bounded_shadow` or `keyed_full_shadow`.
    pub disposition: String,
    /// The exact payload bytes `execution_payload_digest` covers, kept so a
    /// keyed run's binding can be read back after the run.
    #[serde(skip)]
    pub execution_payload: Vec<u8>,
    /// A keyed plan's binding, as the exact bytes to write to
    /// [`EXECUTABLE_REUSE_BINDING_FILE`]; empty when the plan is unkeyed.
    #[serde(skip)]
    pub executable_reuse_binding: Vec<u8>,
}

#[derive(Serialize)]
struct AuthoritativeExecutionPayload<'a> {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_tests_digest: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_tests: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_build_targets_digest: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_build_targets: Option<&'a [String]>,
    /// `full` for a keyed-full plan; absent on every bounded payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    disposition: Option<&'a str>,
    /// sha256 of the keyed binding file the runner derives against.
    #[serde(skip_serializing_if = "Option::is_none")]
    executable_reuse_sha256: Option<&'a str>,
}

/// Serialize a bounded payload, naming `binding_digest` when keyed, and hold
/// it to [`MAX_SELECTED_TEST_BYTES`] as sent: the binding itself is out of
/// band, so a keyed payload is only the digest field larger.
fn capped_payload<'a>(
    payload: &mut AuthoritativeExecutionPayload<'a>,
    binding_digest: Option<&'a str>,
) -> Result<Vec<u8>, ExecutionPlanError> {
    payload.executable_reuse_sha256 = binding_digest;
    let bytes = serde_json::to_vec(&*payload)
        .map_err(|failure| error(format!("serialize authoritative payload: {failure}")))?;
    if bytes.len() > MAX_SELECTED_TEST_BYTES {
        return Err(ExecutionPlanError::over_cap(bytes.len()));
    }
    Ok(bytes)
}

/// The canonical bytes of a keyed binding and their sha256.
fn binding_file(
    binding: &super::executable_reuse::ExecutableReuseBinding,
) -> Result<(Vec<u8>, String), ExecutionPlanError> {
    let bytes = serde_json::to_vec(binding)
        .map_err(|failure| error(format!("serialize executable-reuse binding: {failure}")))?;
    let digest = sha256_hex(&bytes);
    Ok((bytes, digest))
}

/// An ordinary bounded selection.
pub const BOUNDED: &str = "bounded";
/// A bounded selection that also derives executable keys (shadow).
pub const KEYED_BOUNDED_SHADOW: &str = "keyed_bounded_shadow";
/// The full suite, run by the adapter so it can derive executable keys
/// first (shadow): the configured build and test, exactly, nothing skipped.
pub const KEYED_FULL_SHADOW: &str = "keyed_full_shadow";

/// Promotion error. Callers must fail closed to the full suite and retain the
/// diagnostic; they must never treat this as bounded success.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionPlanError(pub String);

/// The reason a plan falls back when its payload is over
/// [`MAX_SELECTED_TEST_BYTES`]; recorded with the byte count so the cap's
/// effect can be counted from the fallback diagnostics.
pub const SELECTION_PAYLOAD_OVER_CAP: &str = "selection_payload_over_cap";

impl ExecutionPlanError {
    fn over_cap(bytes: usize) -> Self {
        error(format!(
            "{SELECTION_PAYLOAD_OVER_CAP}: the bounded selection payload is {bytes} bytes, \
             over the {MAX_SELECTED_TEST_BYTES}-byte cap"
        ))
    }

    /// The payload's size when this plan was refused for exceeding
    /// [`MAX_SELECTED_TEST_BYTES`], `None` for any other refusal.
    #[must_use]
    pub fn selection_payload_over_cap(&self) -> Option<usize> {
        self.0
            .strip_prefix(SELECTION_PAYLOAD_OVER_CAP)?
            .strip_prefix(": the bounded selection payload is ")?
            .split(' ')
            .next()?
            .parse()
            .ok()
    }
}

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
    command_transport: ExecutionCommandTransport,
    validation_contract_digest: &str,
    workflow_digest: &str,
) -> Result<ExecutionDisposition, ExecutionPlanError> {
    let derived = rederive_receipt(receipt, input, policy)?;
    let receipt = &derived;
    if !receipt.exact_head_verified {
        return Err(error("selection receipt is not exact-head verified"));
    }
    match receipt.policy_digest.as_deref() {
        None => {
            return Err(error(format!(
                "selection receipt carries no policy digest (fallback: {:?})",
                receipt.fallback_reason
            )));
        }
        Some(digest) if digest != policy_digest(policy) => {
            return Err(error(
                "selection receipt policy digest does not match protected-base policy",
            ));
        }
        Some(_) => {}
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
    if command_transport != ExecutionCommandTransport::PosixShell {
        return Ok(ExecutionDisposition::Full {
            reason: FullExecutionReason::UnsupportedTransport,
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
        policy.schema_version,
        validation_contract_digest,
        workflow_digest,
        None,
    )
}

/// Bind a keyed shadow plan: the adapter derives executable keys against
/// `binding` before building, and every outcome still validates in full. A
/// bounded receipt the policy may execute keeps its bounded selection
/// (`keyed_bounded_shadow`); anything else becomes a keyed full run
/// (`keyed_full_shadow`), whose payload carries `disposition: "full"` and no
/// selected tests. `Ok(None)` means nothing is keyed here (a blocked plan, no
/// execution template, another transport, or the machine kill switch), and
/// the configured stages run as usual. Performs no I/O.
#[allow(clippy::too_many_arguments)]
pub fn plan_keyed_execution(
    receipt: &SelectionReceipt,
    input: &ExactHeadInput,
    policy: &ChangedSurfacePolicy,
    binding: &super::executable_reuse::ExecutableReuseBinding,
    machine_enabled: bool,
    command_transport: ExecutionCommandTransport,
    validation_contract_digest: &str,
    workflow_digest: &str,
) -> Result<Option<AuthoritativeExecutionPlan>, ExecutionPlanError> {
    let derived = rederive_receipt(receipt, input, policy)?;
    let receipt = &derived;
    if !receipt.exact_head_verified || !receipt.shadow_only {
        return Err(error(
            "selection receipt is not an exact-head shadow planner receipt",
        ));
    }
    let policy_digest_value = policy_digest(policy);
    if receipt.policy_digest.as_deref() != Some(policy_digest_value.as_str()) {
        return Err(error(
            "selection receipt policy digest does not match protected-base policy",
        ));
    }
    let Some(execution) = policy.execution.as_ref() else {
        return Ok(None);
    };
    execution
        .validate(policy.schema_version)
        .map_err(ExecutionPlanError)?;
    // A keyed run replaces the configured build as well as the tests, so
    // only a build-and-test stage can carry one.
    if execution.command.is_none()
        || execution.stage != "build_and_test"
        || receipt.planned_suite == PlannedSuite::Blocked
        || !machine_enabled
        || command_transport != ExecutionCommandTransport::PosixShell
    {
        return Ok(None);
    }
    validate_digest("validation contract", validation_contract_digest)?;
    validate_digest("workflow", workflow_digest)?;
    if receipt.planned_suite == PlannedSuite::Bounded && execution.mode != ExecutionMode::Shadow {
        return match bounded_execution_plan(
            receipt,
            execution,
            policy.schema_version,
            validation_contract_digest,
            workflow_digest,
            Some(binding),
        )? {
            ExecutionDisposition::Bounded(plan) => Ok(Some(*plan)),
            _ => Ok(None),
        };
    }
    let selection_receipt = serde_json::to_vec(receipt)
        .map_err(|failure| error(format!("serialize selection receipt: {failure}")))?;
    let selection_receipt_digest = sha256_hex(&selection_receipt);
    let execution_schema_version = AUTHORITATIVE_EXECUTION_PLAN_SCHEMA_VERSION;
    let (binding_bytes, binding_digest) = binding_file(binding)?;
    let execution_payload = serde_json::to_vec(&AuthoritativeExecutionPayload {
        schema_version: execution_schema_version,
        repository: &receipt.repository,
        pull_request: receipt.pull_request,
        target: &receipt.target,
        base_sha: receipt.planned_base(),
        head_sha: &receipt.head_sha,
        tree_sha: &receipt.tree_sha,
        policy_digest: &policy_digest_value,
        selection_receipt_digest: &selection_receipt_digest,
        validation_contract_digest,
        workflow_digest,
        selected_tests_digest: None,
        selected_tests: None,
        selected_build_targets_digest: None,
        selected_build_targets: None,
        disposition: Some("full"),
        executable_reuse_sha256: Some(&binding_digest),
    })
    .map_err(|failure| error(format!("serialize keyed payload: {failure}")))?;
    let execution_payload_digest = sha256_hex(&execution_payload);
    let command = execution_command(
        execution,
        &execution_payload,
        &execution_payload_digest,
        "keyed",
    )?;
    Ok(Some(AuthoritativeExecutionPlan {
        schema_version: execution_schema_version,
        repository: receipt.repository.clone(),
        pull_request: receipt.pull_request,
        target: receipt.target.clone(),
        base_sha: receipt.planned_base().to_owned(),
        head_sha: receipt.head_sha.clone(),
        tree_sha: receipt.tree_sha.clone(),
        policy_digest: policy_digest_value,
        changed_paths_digest: receipt.changed_paths_digest.clone(),
        validation_contract_digest: validation_contract_digest.to_owned(),
        workflow_digest: workflow_digest.to_owned(),
        selection_receipt_digest,
        selected_tests_digest: String::new(),
        selected_build_targets_digest: None,
        execution_payload_digest,
        selection_tier: receipt.selection_tier,
        selected_count: 0,
        selected_build_target_count: 0,
        stage: execution.stage.clone(),
        command,
        disposition: KEYED_FULL_SHADOW.to_owned(),
        execution_payload,
        executable_reuse_binding: binding_bytes,
    }))
}

/// The protected-base command with the payload and its digest substituted,
/// refused when it would exceed the smallest supported shell limit.
fn execution_command(
    execution: &ChangedSurfaceExecutionPolicy,
    payload: &[u8],
    payload_digest: &str,
    kind: &str,
) -> Result<String, ExecutionPlanError> {
    let template = execution
        .command
        .as_deref()
        .ok_or_else(|| error("authoritative command is missing"))?;
    let command = template
        .replacen(
            SELECTED_TESTS_PAYLOAD_PLACEHOLDER,
            &URL_SAFE_NO_PAD.encode(payload),
            1,
        )
        .replacen(SELECTED_TESTS_DIGEST_PLACEHOLDER, payload_digest, 1);
    if command.encode_utf16().count() > MAX_EXECUTION_COMMAND_UNITS {
        return Err(error(format!(
            "{kind} execution command exceeds the smallest supported shell limit"
        )));
    }
    Ok(command)
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
    policy_schema_version: u32,
    validation_contract_digest: &str,
    workflow_digest: &str,
    keyed: Option<&super::executable_reuse::ExecutableReuseBinding>,
) -> Result<ExecutionDisposition, ExecutionPlanError> {
    validate_digest("validation contract", validation_contract_digest)?;
    validate_digest("workflow", workflow_digest)?;
    let selected = literal_file_bytes(&receipt.selected_tests)?;
    let selection_receipt = serde_json::to_vec(receipt)
        .map_err(|failure| error(format!("serialize selection receipt: {failure}")))?;
    let selection_receipt_digest = sha256_hex(&selection_receipt);
    let selected_tests_digest = sha256_hex(&selected);
    let execution_schema_version = if policy_schema_version >= 3 {
        AUTHORITATIVE_EXECUTION_PLAN_SCHEMA_VERSION
    } else {
        1
    };
    let selected_build_targets = if policy_schema_version >= 3 {
        Some(
            literal_file_bytes(&receipt.selected_build_targets).map_err(|_| {
                error("schema-v3 bounded execution requires selected CMake producer targets")
            })?,
        )
    } else {
        None
    };
    let selected_build_targets_digest = selected_build_targets
        .as_ref()
        .map(|targets| sha256_hex(targets));
    let policy_digest = receipt
        .policy_digest
        .as_deref()
        .expect("matched policy digest");
    let binding = keyed.map(binding_file).transpose()?;
    let mut payload = AuthoritativeExecutionPayload {
        schema_version: execution_schema_version,
        repository: &receipt.repository,
        pull_request: receipt.pull_request,
        target: &receipt.target,
        base_sha: receipt.planned_base(),
        head_sha: &receipt.head_sha,
        tree_sha: &receipt.tree_sha,
        policy_digest,
        selection_receipt_digest: &selection_receipt_digest,
        validation_contract_digest,
        workflow_digest,
        selected_tests_digest: Some(&selected_tests_digest),
        selected_tests: Some(&receipt.selected_tests),
        selected_build_targets_digest: selected_build_targets_digest.as_deref(),
        selected_build_targets: (policy_schema_version >= 3)
            .then_some(receipt.selected_build_targets.as_slice()),
        disposition: None,
        executable_reuse_sha256: None,
    };
    let execution_payload = capped_payload(
        &mut payload,
        binding.as_ref().map(|(_, digest)| digest.as_str()),
    )?;
    let execution_payload_digest = sha256_hex(&execution_payload);
    let command = execution_command(
        execution,
        &execution_payload,
        &execution_payload_digest,
        "bounded",
    )?;
    Ok(ExecutionDisposition::Bounded(Box::new(
        AuthoritativeExecutionPlan {
            schema_version: execution_schema_version,
            repository: receipt.repository.clone(),
            pull_request: receipt.pull_request,
            target: receipt.target.clone(),
            base_sha: receipt.planned_base().to_owned(),
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
            selected_build_targets_digest,
            execution_payload_digest,
            selection_tier: receipt.selection_tier,
            selected_count: receipt.selected_tests.len(),
            selected_build_target_count: receipt.selected_build_targets.len(),
            stage: execution.stage.clone(),
            command,
            disposition: if keyed.is_some() {
                KEYED_BOUNDED_SHADOW
            } else {
                BOUNDED
            }
            .to_owned(),
            execution_payload,
            executable_reuse_binding: binding.map(|(bytes, _)| bytes).unwrap_or_default(),
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

/// A bounded schema-3 `build_and_test` plan, for tests elsewhere in the crate
/// that need the shape the planner actually emits.
#[cfg(test)]
#[must_use]
pub fn schema_v3_build_and_test_plan_for_tests() -> AuthoritativeExecutionPlan {
    tests::schema_v3_build_and_test_plan()
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
    const DIGEST_TWO: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    fn fixture_policy(mode: ExecutionMode) -> ChangedSurfacePolicy {
        ChangedSurfacePolicy {
            schema_version: 2,
            full_test_count: 10,
            build_type: BuildType::Debug,
            build_flags: vec!["-DCMAKE_BUILD_TYPE=Debug".to_owned()],
            baseline_tests: vec!["smoke".to_owned()],
            baseline_build_targets: Vec::new(),
            baseline_only_paths: vec!["docs/**".to_owned()],
            ios_compile_skip_safe_paths: Vec::new(),
            full_required_paths: vec!["CMakeLists.txt".to_owned()],
            policy_paths: vec!["policy.json".to_owned()],
            test_topology_paths: vec!["tests/**".to_owned()],
            families: vec![TestFamily {
                name: "core".to_owned(),
                paths: vec!["src/**".to_owned()],
                tests: vec!["core exact".to_owned()],
                build_targets: Vec::new(),
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
                        "tools/run-selected --receipt {selection_receipt_b64} --receipt-sha256 {selection_receipt_digest}"
                            .to_owned()
                    }),
            }),
            secondary_contract_digests: BTreeMap::new(),
            executable_reuse: None,
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
            merge_base_plan: None,
        }
    }

    fn fixture_receipt(policy: &ChangedSurfacePolicy, input: &ExactHeadInput) -> SelectionReceipt {
        plan_selection(input, Ok(policy.clone())).expect("fixture plan")
    }

    fn fixture_execution(
        receipt: &SelectionReceipt,
        input: &ExactHeadInput,
        policy: &ChangedSurfacePolicy,
        machine_enabled: bool,
    ) -> Result<ExecutionDisposition, ExecutionPlanError> {
        plan_authoritative_execution(
            receipt,
            input,
            policy,
            machine_enabled,
            ExecutionCommandTransport::PosixShell,
            DIGEST,
            DIGEST,
        )
    }

    #[test]
    fn default_shadow_and_machine_kill_switch_keep_full_execution() {
        let input = fixture_input("src/a.rs");
        let shadow = fixture_policy(ExecutionMode::Shadow);
        let shadow_receipt = fixture_receipt(&shadow, &input);
        assert_eq!(
            fixture_execution(&shadow_receipt, &input, &shadow, true).expect("shadow"),
            ExecutionDisposition::Full {
                reason: FullExecutionReason::ShadowPolicy,
            }
        );
        let live = fixture_policy(ExecutionMode::Authoritative);
        let live_receipt = fixture_receipt(&live, &input);
        assert_eq!(
            fixture_execution(&live_receipt, &input, &live, false).expect("kill switch"),
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
                    ExecutionCommandTransport::PosixShell,
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
            fixture_execution(&receipt, &input, &policy, true).expect("bounded")
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
        assert!(plan.command.contains(&plan.execution_payload_digest));
    }

    pub(super) fn schema_v3_build_and_test_plan() -> AuthoritativeExecutionPlan {
        let mut policy = fixture_policy(ExecutionMode::Authoritative);
        policy.schema_version = 3;
        policy.baseline_build_targets = vec!["pulp-test-build-check".to_owned()];
        policy.families[0].build_targets = vec!["pulp-cli".to_owned()];
        let execution = policy.execution.as_mut().expect("execution");
        execution.stage = "build_and_test".to_owned();
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        let ExecutionDisposition::Bounded(plan) =
            fixture_execution(&receipt, &input, &policy, true).expect("bounded")
        else {
            panic!("expected bounded plan");
        };
        *plan
    }

    #[test]
    fn schema_v3_binds_selected_build_targets_and_replaces_build_and_test() {
        let mut policy = fixture_policy(ExecutionMode::Authoritative);
        policy.schema_version = 3;
        policy.baseline_build_targets = vec!["pulp-test-build-check".to_owned()];
        policy.families[0].build_targets = vec!["pulp-cli".to_owned()];
        let execution = policy.execution.as_mut().expect("execution");
        execution.stage = "build_and_test".to_owned();
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        assert_eq!(
            receipt.selected_build_targets,
            ["pulp-cli", "pulp-test-build-check"]
        );
        let ExecutionDisposition::Bounded(plan) =
            fixture_execution(&receipt, &input, &policy, true).expect("bounded")
        else {
            panic!("expected bounded plan");
        };
        assert_eq!(plan.schema_version, 3);
        assert_eq!(plan.stage, "build_and_test");
        assert_eq!(plan.selected_build_target_count, 2);
        assert!(plan.selected_build_targets_digest.is_some());
        let payload = plan
            .command
            .split_whitespace()
            .skip_while(|token| *token != "--receipt")
            .nth(1)
            .expect("payload token");
        let bytes = URL_SAFE_NO_PAD.decode(payload).expect("decode payload");
        let decoded: serde_json::Value = serde_json::from_slice(&bytes).expect("payload json");
        assert_eq!(decoded["schema_version"], 3);
        assert_eq!(
            decoded["selected_build_targets"],
            serde_json::json!(["pulp-cli", "pulp-test-build-check"])
        );
        assert_eq!(
            decoded["selected_build_targets_digest"],
            plan.selected_build_targets_digest
                .as_deref()
                .expect("digest")
        );
    }

    #[test]
    fn schema_v3_refuses_test_only_stage_or_missing_producer_targets() {
        let mut policy = fixture_policy(ExecutionMode::Authoritative);
        policy.schema_version = 3;
        assert!(
            policy
                .execution
                .as_ref()
                .expect("execution")
                .validate(3)
                .is_err()
        );
        policy.execution.as_mut().expect("execution").stage = "build_and_test".to_owned();
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        assert!(fixture_execution(&receipt, &input, &policy, true).is_err());
    }

    #[test]
    fn payload_authorization_changes_with_validation_and_workflow_contracts() {
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        let ExecutionDisposition::Bounded(first) = plan_authoritative_execution(
            &receipt,
            &input,
            &policy,
            true,
            ExecutionCommandTransport::PosixShell,
            DIGEST,
            DIGEST,
        )
        .expect("first") else {
            panic!("expected bounded plan");
        };
        let ExecutionDisposition::Bounded(second) = plan_authoritative_execution(
            &receipt,
            &input,
            &policy,
            true,
            ExecutionCommandTransport::PosixShell,
            DIGEST_TWO,
            DIGEST,
        )
        .expect("second") else {
            panic!("expected bounded plan");
        };
        let ExecutionDisposition::Bounded(third) = plan_authoritative_execution(
            &receipt,
            &input,
            &policy,
            true,
            ExecutionCommandTransport::PosixShell,
            DIGEST,
            DIGEST_TWO,
        )
        .expect("third") else {
            panic!("expected bounded plan");
        };
        assert_ne!(
            first.execution_payload_digest,
            second.execution_payload_digest
        );
        assert_ne!(
            first.execution_payload_digest,
            third.execution_payload_digest
        );
        assert_ne!(first.command, second.command);
        assert_ne!(first.command, third.command);
    }

    #[test]
    fn unproven_command_transports_keep_the_full_suite() {
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        for transport in [
            ExecutionCommandTransport::WindowsEncoded,
            ExecutionCommandTransport::Unsupported,
        ] {
            assert_eq!(
                plan_authoritative_execution(
                    &receipt, &input, &policy, true, transport, DIGEST, DIGEST,
                )
                .expect("unsupported transport falls back"),
                ExecutionDisposition::Full {
                    reason: FullExecutionReason::UnsupportedTransport,
                }
            );
        }
    }

    #[test]
    fn literal_payload_is_cross_shell_safe_and_bound_to_the_plan_digest() {
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        let ExecutionDisposition::Bounded(plan) =
            fixture_execution(&receipt, &input, &policy, true).expect("bounded")
        else {
            panic!("expected bounded plan");
        };
        let payload = plan
            .command
            .split_whitespace()
            .skip_while(|token| *token != "--receipt")
            .nth(1)
            .expect("payload token");
        assert!(
            payload
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "payload must be safe as one token in POSIX and cmd shells"
        );
        let bytes = URL_SAFE_NO_PAD.decode(payload).expect("decode payload");
        let decoded: serde_json::Value = serde_json::from_slice(&bytes).expect("payload json");
        assert_eq!(decoded["head_sha"], HEAD);
        assert_eq!(decoded["tree_sha"], TREE);
        assert_eq!(decoded["policy_digest"], policy_digest(&policy));
        assert_eq!(
            decoded["selection_receipt_digest"],
            plan.selection_receipt_digest
        );
        assert_eq!(decoded["validation_contract_digest"], DIGEST);
        assert_eq!(decoded["workflow_digest"], DIGEST);
        assert_eq!(
            decoded["selected_tests"],
            serde_json::json!(receipt.selected_tests)
        );
        assert_eq!(sha256_hex(&bytes), plan.execution_payload_digest);
    }

    #[test]
    fn malformed_policy_and_receipt_fail_closed() {
        let mut policy = fixture_policy(ExecutionMode::Authoritative);
        policy.execution.as_mut().expect("execution").command =
            Some("run {selection_receipt_b64} {selection_receipt_b64}".to_owned());
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
        assert!(fixture_execution(&receipt, &input, &policy, true).is_err());
        let mut receipt = fixture_receipt(&policy, &input);
        receipt.selected_tests.push("bad\nname".to_owned());
        assert!(fixture_execution(&receipt, &input, &policy, true).is_err());
    }

    #[test]
    fn stale_base_receipt_keeps_the_full_suite_instead_of_a_digest_refusal() {
        // The shape every production stale-base run had: the PR's recorded
        // base lags the protected tip, so the planner stops at provenance.
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let mut input = fixture_input("src/a.rs");
        input.protected_ref_sha = "ffffffffffffffffffffffffffffffffffffffff".to_owned();
        let receipt = fixture_receipt(&policy, &input);
        assert_eq!(
            receipt.fallback_reason,
            Some(crate::changed_surface::FallbackReason::StaleBase)
        );
        assert_eq!(receipt.policy_digest, Some(policy_digest(&policy)));
        assert_eq!(
            fixture_execution(&receipt, &input, &policy, true).expect("stale base plans full"),
            ExecutionDisposition::Full {
                reason: FullExecutionReason::PlannerSelectedFull,
            }
        );

        let mut foreign = fixture_policy(ExecutionMode::Authoritative);
        foreign.full_test_count += 1;
        let refused = fixture_execution(&receipt, &input, &foreign, true)
            .expect_err("a different policy stays fail-closed");
        assert!(refused.0.contains("rederived"), "{refused}");
    }

    #[test]
    fn receipt_without_a_policy_digest_names_its_fallback() {
        // An invalid base policy never yields a digest; the refusal must say
        // so rather than claim a mismatch against a policy it never bound.
        let mut policy = fixture_policy(ExecutionMode::Authoritative);
        policy.full_test_count = 0;
        let mut input = fixture_input("src/a.rs");
        input.protected_ref_sha = "ffffffffffffffffffffffffffffffffffffffff".to_owned();
        let receipt = fixture_receipt(&policy, &input);
        assert_eq!(receipt.policy_digest, None);
        let refused = fixture_execution(&receipt, &input, &policy, true)
            .expect_err("no digest stays fail-closed");
        assert!(refused.0.contains("carries no policy digest"), "{refused}");
        assert!(refused.0.contains("StaleBase"), "{refused}");
    }

    #[test]
    fn a_merge_base_plan_rederives_and_promotes_like_an_up_to_date_one() {
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let mut input = fixture_input("src/a.rs");
        let recorded = "ffffffffffffffffffffffffffffffffffffffff";
        input.pr_base_sha = recorded.to_owned();
        input.protected_ref_sha = recorded.to_owned();
        input.merge_base_plan = Some(crate::changed_surface::MergeBasePlan {
            recorded_base_policy_digest: Some(policy_digest(&policy)),
        });
        let receipt = fixture_receipt(&policy, &input);
        assert_eq!(receipt.planned_base_sha.as_deref(), Some(BASE));
        let ExecutionDisposition::Bounded(plan) =
            fixture_execution(&receipt, &input, &policy, true).expect("bounded")
        else {
            panic!("expected a bounded plan");
        };
        // The adapter projects the base the selection was planned against;
        // the recorded tip is not an ancestor-equal merge base of this head.
        assert_eq!(plan.base_sha, BASE);
        let payload: serde_json::Value = plan
            .command
            .split_whitespace()
            .filter_map(|word| URL_SAFE_NO_PAD.decode(word).ok())
            .find_map(|bytes| serde_json::from_slice(&bytes).ok())
            .expect("payload in command");
        assert_eq!(payload["base_sha"], BASE);

        // A receipt claiming a merge-base plan the input does not carry is stale.
        let mut forged = receipt;
        forged.planned_base_sha = None;
        assert!(fixture_execution(&forged, &input, &policy, true).is_err());
    }

    #[test]
    fn planner_full_and_blocked_are_not_promoted() {
        let policy = fixture_policy(ExecutionMode::Authoritative);
        let full_input = fixture_input("CMakeLists.txt");
        let full = fixture_receipt(&policy, &full_input);
        assert_eq!(full.planned_suite, PlannedSuite::Full);
        assert_eq!(
            fixture_execution(&full, &full_input, &policy, true).expect("full"),
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
                ExecutionCommandTransport::PosixShell,
                DIGEST,
                DIGEST,
            )
            .expect("blocked"),
            ExecutionDisposition::Blocked {
                reason: blocked_detail,
            }
        );
    }

    fn binding() -> crate::changed_surface::executable_reuse::ExecutableReuseBinding {
        crate::changed_surface::executable_reuse::ExecutableReuseBinding {
            candidates: vec![crate::changed_surface::executable_reuse::BaseCandidate {
                run_id: "c1-1-2".to_owned(),
                record_sha256: DIGEST.to_owned(),
                record_path: "/state/reuse-records/o__r/records/c1/c1-1-2".to_owned(),
                commit: BASE.to_owned(),
            }],
            rules_digest: DIGEST.to_owned(),
            derivation_code_dir: "/state/derivation/abc".to_owned(),
            derivation_code_sha256: DIGEST.to_owned(),
            sample_seed: DIGEST.to_owned(),
            sample_percent: 5,
            build_dir: "build".to_owned(),
        }
    }

    fn build_and_test_policy(mode: ExecutionMode) -> ChangedSurfacePolicy {
        let mut policy = fixture_policy(mode);
        policy.schema_version = 3;
        policy.baseline_build_targets = vec!["pulp-test-build-check".to_owned()];
        policy.families[0].build_targets = vec!["pulp-cli".to_owned()];
        policy.execution.as_mut().expect("execution").stage = "build_and_test".to_owned();
        policy
    }

    fn keyed(
        receipt: &SelectionReceipt,
        input: &ExactHeadInput,
        policy: &ChangedSurfacePolicy,
        machine_enabled: bool,
    ) -> Option<AuthoritativeExecutionPlan> {
        plan_keyed_execution(
            receipt,
            input,
            policy,
            &binding(),
            machine_enabled,
            ExecutionCommandTransport::PosixShell,
            DIGEST,
            DIGEST,
        )
        .expect("keyed plan")
    }

    fn payload_of(plan: &AuthoritativeExecutionPlan) -> serde_json::Value {
        let encoded = plan
            .command
            .split_whitespace()
            .nth(2)
            .expect("payload argument");
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).expect("base64")).expect("json")
    }

    #[test]
    fn a_full_plan_becomes_a_keyed_full_run_with_no_selection() {
        let policy = build_and_test_policy(ExecutionMode::Authoritative);
        let input = fixture_input("CMakeLists.txt");
        let receipt = fixture_receipt(&policy, &input);
        assert_eq!(receipt.planned_suite, PlannedSuite::Full);
        let plan = keyed(&receipt, &input, &policy, true).expect("keyed full");
        assert_eq!(plan.disposition, KEYED_FULL_SHADOW);
        assert_eq!(
            (plan.selected_count, plan.selected_tests_digest.as_str()),
            (0, "")
        );
        let payload = payload_of(&plan);
        assert_eq!(payload["disposition"], "full");
        assert!(
            payload.get("executable_reuse").is_none(),
            "the binding is a file, named by digest"
        );
        assert_eq!(
            plan.executable_reuse_binding,
            serde_json::to_vec(&binding()).expect("json")
        );
        assert_eq!(
            payload["executable_reuse_sha256"],
            sha256_hex(&plan.executable_reuse_binding),
            "the digest is over the exact file bytes"
        );
        for absent in [
            "selected_tests",
            "selected_tests_digest",
            "selected_build_targets",
            "selected_build_targets_digest",
        ] {
            assert!(
                payload.get(absent).is_none(),
                "{absent} must not be in a full payload"
            );
        }
        let raw = URL_SAFE_NO_PAD
            .decode(
                plan.command
                    .split_whitespace()
                    .nth(2)
                    .expect("payload argument"),
            )
            .expect("base64");
        assert_eq!(
            sha256_hex(&raw),
            plan.execution_payload_digest,
            "the digest is over the exact bytes sent"
        );
    }

    #[test]
    fn a_bounded_plan_keeps_its_selection_and_binds_the_keys() {
        let policy = build_and_test_policy(ExecutionMode::Authoritative);
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        let plan = keyed(&receipt, &input, &policy, true).expect("keyed bounded");
        assert_eq!(plan.disposition, KEYED_BOUNDED_SHADOW);
        let payload = payload_of(&plan);
        assert!(
            payload.get("disposition").is_none(),
            "a bounded payload never says disposition"
        );
        assert!(payload.get("selected_tests").is_some());
        assert!(payload.get("executable_reuse").is_none());
        let bound: serde_json::Value =
            serde_json::from_slice(&plan.executable_reuse_binding).expect("binding file");
        assert_eq!(bound["candidates"][0]["commit"], BASE);
        assert_eq!(
            payload["executable_reuse_sha256"],
            sha256_hex(&plan.executable_reuse_binding)
        );
        let ExecutionDisposition::Bounded(unkeyed) =
            fixture_execution(&receipt, &input, &policy, true).expect("bounded")
        else {
            panic!("expected bounded");
        };
        assert_eq!(unkeyed.disposition, BOUNDED);
        assert!(unkeyed.executable_reuse_binding.is_empty());
        assert_ne!(
            unkeyed.execution_payload_digest, plan.execution_payload_digest,
            "the binding is bound"
        );
    }

    #[test]
    fn nothing_is_keyed_when_the_plan_cannot_run_the_adapter() {
        let policy = build_and_test_policy(ExecutionMode::Authoritative);
        let input = fixture_input("CMakeLists.txt");
        let receipt = fixture_receipt(&policy, &input);
        assert!(keyed(&receipt, &input, &policy, true).is_some(), "control");
        assert!(
            keyed(&receipt, &input, &policy, false).is_none(),
            "machine kill switch"
        );
        let tests_only = fixture_policy(ExecutionMode::Authoritative);
        let receipt_tests_only = fixture_receipt(&tests_only, &input);
        assert!(
            keyed(&receipt_tests_only, &input, &tests_only, true).is_none(),
            "a test-only stage cannot replace the build"
        );
        let mut shadow = build_and_test_policy(ExecutionMode::Shadow);
        shadow.execution.as_mut().expect("execution").command = None;
        let receipt = fixture_receipt(&shadow, &input);
        assert!(
            keyed(&receipt, &input, &shadow, true).is_none(),
            "no command template"
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
            assert!(fixture_execution(&mutated, &input, &policy, true).is_err());
        }
    }

    #[test]
    fn oversized_cross_shell_payload_falls_back_before_command_construction() {
        let mut policy = fixture_policy(ExecutionMode::Authoritative);
        policy.baseline_tests = vec!["x".repeat(MAX_SELECTED_TEST_BYTES)];
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        assert_eq!(receipt.planned_suite, PlannedSuite::Bounded);
        assert!(fixture_execution(&receipt, &input, &policy, true).is_err());
    }

    #[test]
    fn final_command_includes_template_overhead_in_shell_limit() {
        let mut policy = fixture_policy(ExecutionMode::Authoritative);
        policy.execution.as_mut().expect("execution").command = Some(format!(
            "{} {{selection_receipt_b64}} {{selection_receipt_digest}}",
            "x".repeat(MAX_EXECUTION_COMMAND_UNITS)
        ));
        let input = fixture_input("src/a.rs");
        let receipt = fixture_receipt(&policy, &input);
        assert!(fixture_execution(&receipt, &input, &policy, true).is_err());
    }

    /// A binding with `candidates` candidates of real record-path length.
    fn real_binding(
        candidates: usize,
    ) -> crate::changed_surface::executable_reuse::ExecutableReuseBinding {
        let record = "/Users/danielraffel/Library/Application Support/shipyard/reuse-records/\
                      Generous-Corp__pulp/records";
        let mut bound = binding();
        bound.derivation_code_dir = "/Users/danielraffel/Library/Application Support/shipyard/\
                                     executable-reuse/derivation/312a68e352fadad3c4be462636bd51bbff0d12866d371701dd1ba6b5f599a25b"
            .to_owned();
        bound.candidates = (0..candidates)
            .map(|n| {
                let commit = format!("{n:040x}");
                let run_id = format!("{commit}-1791170114519872000-69131");
                crate::changed_surface::executable_reuse::BaseCandidate {
                    record_path: format!("{record}/{commit}/{run_id}"),
                    run_id,
                    record_sha256: DIGEST.to_owned(),
                    commit,
                }
            })
            .collect();
        bound
    }

    /// The bounded receipt with `count` literal test names of `width` bytes.
    fn selection_of(count: usize, width: usize) -> (ChangedSurfacePolicy, SelectionReceipt) {
        let policy = build_and_test_policy(ExecutionMode::Authoritative);
        let input = fixture_input("src/a.rs");
        let mut receipt = fixture_receipt(&policy, &input);
        receipt.selected_tests = (0..count)
            .map(|n| format!("{n:04}-{}", "t".repeat(width - 5)))
            .collect();
        (policy, receipt)
    }

    fn keyed_bounded(
        policy: &ChangedSurfacePolicy,
        receipt: &SelectionReceipt,
        bound: &crate::changed_surface::executable_reuse::ExecutableReuseBinding,
    ) -> Result<ExecutionDisposition, ExecutionPlanError> {
        bounded_execution_plan(
            receipt,
            policy.execution.as_ref().expect("execution"),
            policy.schema_version,
            DIGEST,
            DIGEST,
            Some(bound),
        )
    }

    #[test]
    fn proof_b_sized_selection_plans_keyed_where_the_inline_binding_was_refused() {
        // Proof B: 96 tests, a 3,071-byte list, one candidate.
        let (policy, receipt) = selection_of(96, 30);
        let list = serde_json::to_vec(&receipt.selected_tests).expect("list");
        assert!((3_000..3_200).contains(&list.len()), "{}", list.len());
        let bound = real_binding(1);
        let ExecutionDisposition::Bounded(plan) =
            keyed_bounded(&policy, &receipt, &bound).expect("keyed bounded plan")
        else {
            panic!("expected a bounded plan");
        };
        assert_eq!(plan.disposition, KEYED_BOUNDED_SHADOW);
        // Control: the same payload with the binding inline, as schema 2 sent
        // it, is over the pre-fix 4 KiB payload cap; that is the refusal
        // proof B hit.
        let mut inline = payload_of(&plan);
        inline
            .as_object_mut()
            .expect("payload")
            .remove("executable_reuse_sha256");
        inline["executable_reuse"] = serde_json::to_value(&bound).expect("binding");
        assert!(
            serde_json::to_vec(&inline).expect("inline").len() > 4 * 1024,
            "the inline binding did not fit the old cap"
        );
    }

    #[test]
    fn the_command_does_not_grow_with_the_candidate_count() {
        // A selection near the payload cap, with the most candidates a plan
        // binds, still makes a command under the shell bound, and the same
        // command length as one candidate.
        let (policy, receipt) = selection_of(140, 30);
        let max = crate::changed_surface::executable_reuse::MAX_CANDIDATES;
        let plans: Vec<AuthoritativeExecutionPlan> = [1, max]
            .iter()
            .map(|&n| {
                let ExecutionDisposition::Bounded(plan) =
                    keyed_bounded(&policy, &receipt, &real_binding(n)).expect("keyed plan")
                else {
                    panic!("expected a bounded plan");
                };
                *plan
            })
            .collect();
        let commands: Vec<usize> = plans
            .iter()
            .map(|plan| plan.command.encode_utf16().count())
            .collect();
        assert!(commands[1] < MAX_EXECUTION_COMMAND_UNITS, "{commands:?}");
        assert_eq!(commands[0], commands[1], "{commands:?}");
        // Control: the same eight candidates inline would push the command
        // past the shell bound.
        let mut inline = payload_of(&plans[1]);
        inline
            .as_object_mut()
            .expect("payload")
            .remove("executable_reuse_sha256");
        inline["executable_reuse"] = serde_json::to_value(real_binding(max)).expect("binding");
        let inline_units = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&inline).expect("inline"))
            .len();
        assert!(inline_units > MAX_EXECUTION_COMMAND_UNITS, "{inline_units}");
    }

    #[test]
    fn the_selection_cap_still_refuses_an_oversized_list() {
        // Refused by the payload cap itself, before the command check.
        let (policy, receipt) = selection_of(200, 30);
        let Err(refused) = keyed_bounded(&policy, &receipt, &real_binding(1)) else {
            panic!("an oversized selection must not plan");
        };
        let bytes = refused
            .selection_payload_over_cap()
            .unwrap_or_else(|| panic!("not the payload cap: {refused}"));
        assert!(bytes > MAX_SELECTED_TEST_BYTES, "{bytes}");
        assert!(refused.to_string().starts_with(SELECTION_PAYLOAD_OVER_CAP));
        // Any other refusal names no byte count.
        assert_eq!(error("other").selection_payload_over_cap(), None);
    }

    #[test]
    fn the_command_names_the_binding_file_by_the_digest_of_its_bytes() {
        let (policy, receipt) = selection_of(10, 30);
        let ExecutionDisposition::Bounded(plan) =
            keyed_bounded(&policy, &receipt, &real_binding(2)).expect("keyed plan")
        else {
            panic!("expected a bounded plan");
        };
        let payload = payload_of(&plan);
        assert_eq!(
            payload["executable_reuse_sha256"],
            sha256_hex(&plan.executable_reuse_binding)
        );
        let parsed: crate::changed_surface::executable_reuse::ExecutableReuseBinding =
            serde_json::from_slice(&plan.executable_reuse_binding).expect("binding file");
        assert_eq!(parsed, real_binding(2));
    }
}
