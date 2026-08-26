//! Fail-closed changed-surface test planning for exact pull-request heads.
//!
//! This module deliberately plans only. During the shadow phase the configured
//! full suite remains authoritative; an eligible bounded selection is emitted
//! as telemetry so it can be compared with that full-suite result.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use glob::Pattern;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod execution;
pub mod trial;
pub use execution::{
    AuthoritativeExecutionPlan, ChangedSurfaceExecutionPolicy, ExecutionCommandTransport,
    ExecutionDisposition, ExecutionMode, ExecutionPlanError, FullExecutionReason,
    plan_authoritative_execution,
};

/// Oldest changed-surface declaration schema understood by this release.
pub const MIN_CHANGED_SURFACE_SCHEMA_VERSION: u32 = 1;
/// Newest changed-surface declaration schema understood by this release.
pub const CHANGED_SURFACE_SCHEMA_VERSION: u32 = 3;
/// Current exact-head selection receipt schema.
pub const SELECTION_RECEIPT_SCHEMA_VERSION: u32 = 3;
/// Maximum age accepted for a required secondary execution proof.
pub const SECONDARY_PROOF_MAX_AGE_HOURS: i64 = 24;

/// A complete reviewed test family and the changed paths that affect it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestFamily {
    /// Stable family name used in receipts.
    pub name: String,
    /// Repository-relative glob patterns that activate this family.
    pub paths: Vec<String>,
    /// Complete literal test names for this family. These are not regexes.
    pub tests: Vec<String>,
    /// `CMake` producer targets needed to materialize this family's tests.
    #[serde(default)]
    pub build_targets: Vec<String>,
    /// Risk class reviewed for this family. Schema v1 defaults to affected.
    #[serde(default)]
    pub risk_class: RiskClass,
    /// Deterministic one-hop/co-failure neighbors added for medium-risk changes.
    #[serde(default)]
    pub extended_tests: Vec<String>,
    /// Build types on which these literal tests are valid.
    pub supported_build_types: Vec<BuildType>,
    /// Required non-advisory target that proves this family when the current
    /// target's build type is unsupported.
    #[serde(default)]
    pub required_secondary_target: Option<String>,
    /// Build type produced by the required secondary target.
    #[serde(default)]
    pub required_secondary_build_type: Option<BuildType>,
}

/// Reviewed risk class for an affected test family.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    /// Run the mandatory kernel plus the directly affected family.
    #[default]
    Low,
    /// Also run reviewed integration/co-failure neighbors.
    Medium,
    /// The family is not eligible for bounded execution.
    High,
}

/// Typed build configuration used by selector compatibility policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildType {
    /// Unoptimized/debug validation.
    Debug,
    /// Optimized production validation.
    Release,
    /// Optimized validation retaining debug information.
    RelWithDebInfo,
    /// Size-optimized validation.
    MinSizeRel,
}

/// Base-owned selector policy declared under
/// `[targets.<name>.changed_surface_selection]`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangedSurfacePolicy {
    /// Policy schema version.
    pub schema_version: u32,
    /// Number of tests in the authoritative full suite.
    pub full_test_count: usize,
    /// Build type of this target's test stage.
    pub build_type: BuildType,
    /// Reviewed build flags bound into the receipt.
    #[serde(default)]
    pub build_flags: Vec<String>,
    /// Literal tests that run for every eligible bounded selection.
    pub baseline_tests: Vec<String>,
    /// `CMake` producer targets needed by the mandatory baseline tests.
    #[serde(default)]
    pub baseline_build_targets: Vec<String>,
    /// Paths reviewed as safe to require baseline smoke only (for example docs).
    #[serde(default)]
    pub baseline_only_paths: Vec<String>,
    /// Paths independently reviewed by the repository as safe to omit its
    /// optional iOS/AUv3 compile lane. Shipyard binds this repository-owned
    /// projection into the policy digest but does not infer test selection
    /// from it.
    #[serde(default)]
    pub ios_compile_skip_safe_paths: Vec<String>,
    /// Reviewed high-risk surfaces that always require the full suite.
    #[serde(default)]
    pub full_required_paths: Vec<String>,
    /// Policy/schema paths whose head-side modification forces full-suite fallback.
    #[serde(default)]
    pub policy_paths: Vec<String>,
    /// Test-registration/topology paths whose modification forces full-suite fallback.
    pub test_topology_paths: Vec<String>,
    /// Complete reviewed family declarations.
    pub families: Vec<TestFamily>,
    /// Optional protected-base promotion policy. Absence remains shadow-only.
    #[serde(default)]
    pub execution: Option<ChangedSurfaceExecutionPolicy>,
    /// Expected target-validation digests computed from the authenticated base.
    #[serde(skip)]
    pub secondary_contract_digests: BTreeMap<String, String>,
}

/// Authenticated GitHub provenance plus independently observed local git facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactHeadInput {
    /// Canonical `owner/repo` identity.
    pub repository: String,
    /// Pull request number.
    pub pull_request: u64,
    /// Exact target whose base-owned policy produced the plan.
    pub target: String,
    /// Time at which this planning observation began.
    pub observed_at: DateTime<Utc>,
    /// Base ref reported by the pull request.
    pub base_ref: String,
    /// Base SHA reported by the pull request.
    pub pr_base_sha: String,
    /// Current SHA resolved from the protected base ref.
    pub protected_ref_sha: String,
    /// Protection/resolution status from GitHub's branch API.
    pub protected_ref_status: ProtectedRefStatus,
    /// Head SHA reported by the pull request.
    pub pr_head_sha: String,
    /// Head tree SHA reported by GitHub's commit API.
    pub remote_tree_sha: String,
    /// HEAD observed in the local validation checkout.
    pub local_head_sha: String,
    /// HEAD tree observed in the local validation checkout.
    pub local_tree_sha: String,
    /// Merge base computed by local git.
    pub local_merge_base_sha: String,
    /// Merge base reported by GitHub's compare API.
    pub remote_merge_base_sha: String,
    /// Whether local git proves the merge base is an ancestor of the head.
    pub merge_base_is_ancestor: bool,
    /// Whether the checkout has tracked, staged, or untracked changes.
    pub checkout_clean: bool,
    /// Paths returned by the authenticated PR-files API.
    pub remote_changed_paths: Vec<String>,
    /// Completeness of the authenticated PR-files query.
    pub remote_changed_paths_status: ObservationStatus,
    /// Paths computed locally for `merge-base..HEAD`.
    pub local_changed_paths: Vec<String>,
    /// Completeness of local changed-path computation.
    pub local_changed_paths_status: ObservationStatus,
    /// Tracked paths read from the authenticated base tree.
    pub base_tracked_paths: Vec<String>,
    /// Completeness of the authenticated base-tree observation.
    pub base_tracked_paths_status: ObservationStatus,
    /// Exact-head target evidence available to satisfy typed secondary legs.
    pub secondary_proofs: Vec<SecondaryProof>,
}

/// Exact-head target evidence offered for a typed secondary validation leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecondaryProof {
    /// Required target name from base-owned policy.
    pub target: String,
    /// Build type this target proves.
    pub build_type: BuildType,
    /// Exact validated head SHA.
    pub head_sha: String,
    /// Exact git tree observed by the target before execution.
    pub tree_sha: String,
    /// Whether the concrete target ran without stage resume or reuse.
    pub full_execution: bool,
    /// Evidence status.
    pub passed: bool,
    /// Reused ancestor evidence is never accepted for a required secondary leg.
    pub reused: bool,
    /// Completion time of the concrete target execution.
    pub completed_at: DateTime<Utc>,
    /// Contract digest recorded by the concrete target execution, when present.
    pub contract_digest: Option<String>,
}

/// Authenticated protected-ref query state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtectedRefStatus {
    /// GitHub resolved the base branch and reports it protected.
    Protected,
    /// GitHub resolved the base branch and reports it unprotected.
    Unprotected,
    /// The protected-ref query was unavailable or malformed.
    Unresolved,
}

/// Whether an observed input is complete enough for selector policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationStatus {
    /// The observation completed without truncation.
    Complete,
    /// The observation failed or was truncated.
    Incomplete,
}

/// A hard identity failure. No successful or reusable receipt may be emitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdentityError {
    /// A required identity field was missing or malformed.
    Unresolved(String),
    /// The local checkout does not contain the authenticated PR head.
    HeadMismatch {
        /// Authenticated PR head.
        expected: String,
        /// Locally observed HEAD.
        observed: String,
    },
    /// The local tree does not match GitHub's tree for the authenticated head.
    TreeMismatch {
        /// Authenticated GitHub tree.
        expected: String,
        /// Locally observed tree.
        observed: String,
    },
    /// The validation checkout contains changes outside the authenticated tree.
    DirtyCheckout,
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unresolved(detail) => {
                write!(formatter, "unresolved exact-head identity: {detail}")
            }
            Self::HeadMismatch { expected, observed } => write!(
                formatter,
                "exact-head mismatch: authenticated PR head {expected}, local HEAD {observed}"
            ),
            Self::TreeMismatch { expected, observed } => write!(
                formatter,
                "head tree mismatch: authenticated tree {expected}, local tree {observed}"
            ),
            Self::DirtyCheckout => write!(
                formatter,
                "validation checkout is dirty and does not represent only the authenticated head tree"
            ),
        }
    }
}

impl std::error::Error for IdentityError {}

/// Why an exact-head plan conservatively selected the full suite.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    /// The protected-ref query was missing or malformed.
    BaseRefUnresolved,
    /// GitHub did not report the PR base as a protected ref.
    BaseRefNotProtected,
    /// The protected ref advanced away from the PR base used for provenance.
    StaleBase,
    /// Base and head are the same commit.
    BaseEqualsHead,
    /// Local ancestry could not be proven.
    AncestryMismatch,
    /// GitHub and local git disagreed about the applicable merge base.
    MergeBaseMismatch,
    /// The applicable merge base is not the authenticated protected base.
    BasePolicyMismatch,
    /// GitHub and local git reported different changed-path sets.
    ChangedPathsMismatch,
    /// The base-owned selector declaration was missing or invalid.
    InvalidPolicy,
    /// This head changes selector configuration or schema.
    SelectorPolicyChanged,
    /// This head changes test registration/topology.
    TestTopologyChanged,
    /// A protected-base policy surface requires full validation.
    FullRequiredSurface,
    /// An affected family is explicitly classified as high risk.
    HighRiskFamily,
    /// A changed path is not covered by a family or baseline-only declaration.
    UnmappedChangedPath,
    /// Path evaluation itself was ambiguous or invalid.
    AmbiguousDiff,
}

/// Risk tier selected for shadow comparison.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionTier {
    /// Mandatory kernel only.
    Mandatory,
    /// Mandatory kernel plus directly affected tests.
    Affected,
    /// Affected tests plus reviewed integration/co-failure neighbors.
    Extended,
    /// Complete authoritative suite.
    Full,
}

/// Planner decision retained in the exact-head receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedSuite {
    /// A bounded suite was computed for shadow comparison.
    Bounded,
    /// Safety policy selected the full authoritative suite.
    Full,
    /// A known incompatible family requires an exact-head secondary proof.
    Blocked,
}

/// Secondary proof bound into a completed selection receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SecondaryProofReceipt {
    /// Required target.
    pub target: String,
    /// Typed build configuration.
    pub build_type: BuildType,
    /// Exact head proven by the target.
    pub head_sha: String,
    /// Exact git tree observed by the target before execution.
    pub tree_sha: String,
    /// Whether the concrete target ran without stage resume or reuse.
    pub full_execution: bool,
    /// Completion time of the accepted fresh execution.
    pub completed_at: DateTime<Utc>,
    /// Contract digest recorded by the execution, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_digest: Option<String>,
    /// Families covered by this proof.
    pub families: Vec<String>,
    /// Complete literal tests covered by this proof.
    pub tests: Vec<String>,
}

/// Outcomes carried by a shadow receipt. This phase never claims target proof.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionOutcomes {
    /// Planner result after exact-head verification.
    pub planner: String,
    /// Full-suite result is owned by the existing target execution path.
    pub authoritative_execution: String,
}

/// Exact-head shadow-planning receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionReceipt {
    /// Receipt schema version.
    pub schema_version: u32,
    /// Whether this receipt passed exact-head identity verification.
    pub exact_head_verified: bool,
    /// This phase never executes the bounded selection authoritatively.
    pub shadow_only: bool,
    /// Repository identity.
    pub repository: String,
    /// Pull request identity.
    pub pull_request: u64,
    /// Exact target whose base-owned policy produced this receipt.
    pub target: String,
    /// Authenticated protected target ref.
    pub protected_ref: String,
    /// Base SHA reported by PR metadata.
    pub pr_base_sha: String,
    /// SHA resolved from the protected ref.
    pub protected_ref_sha: String,
    /// Applicable merge-base SHA.
    pub merge_base_sha: String,
    /// Exact authenticated head SHA.
    pub head_sha: String,
    /// Exact authenticated head tree SHA.
    pub tree_sha: String,
    /// Digest of the authenticated changed-path set.
    pub changed_paths_digest: String,
    /// Digest of the selector policy loaded from the authenticated base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_digest: Option<String>,
    /// Current target build type from base-owned policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_type: Option<BuildType>,
    /// Reviewed target build flags from base-owned policy.
    pub build_flags: Vec<String>,
    /// Authenticated changed paths.
    pub changed_paths: Vec<String>,
    /// Selected affected family names.
    pub selected_families: Vec<String>,
    /// Literal bounded test names (baseline union complete affected families).
    pub selected_tests: Vec<String>,
    /// Protected-base `CMake` target union needed by the selected tests.
    pub selected_build_targets: Vec<String>,
    /// Mandatory baseline tests included in every eligible bound.
    pub baseline_tests: Vec<String>,
    /// Test counts by affected family.
    pub family_coverage: BTreeMap<String, usize>,
    /// Required exact-head secondary legs satisfying incompatible families.
    pub secondary_proofs: Vec<SecondaryProofReceipt>,
    /// Planned suite for comparison; execution remains full during shadow mode.
    pub planned_suite: PlannedSuite,
    /// Risk tier represented by `selected_tests` or the conservative fallback.
    pub selection_tier: SelectionTier,
    /// Authoritative suite executed in this phase.
    pub authoritative_suite: PlannedSuite,
    /// Explicit planner/authoritative-execution outcomes.
    pub outcomes: SelectionOutcomes,
    /// Selected bounded count, or the full count on fallback when known.
    pub selected_count: Option<usize>,
    /// Declared full-suite count when policy parsing succeeded.
    pub full_count: Option<usize>,
    /// Stable fallback reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<FallbackReason>,
    /// Bounded detail for diagnostics; never interpreted as policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_detail: Option<String>,
    /// Planner elapsed time, populated by the command boundary.
    pub elapsed_ms: u64,
}

/// Parse and validate a base-owned selector declaration from tracked TOML.
pub fn policy_from_toml(contents: &str, target: &str) -> Result<ChangedSurfacePolicy, String> {
    let root = contents
        .parse::<toml::Table>()
        .map_err(|error| format!("parse base config: {error}"))?;
    let value = root
        .get("targets")
        .and_then(toml::Value::as_table)
        .and_then(|targets| targets.get(target))
        .and_then(toml::Value::as_table)
        .and_then(|target| target.get("changed_surface_selection"))
        .cloned()
        .ok_or_else(|| {
            format!(
                "authenticated base has no [targets.{target}.changed_surface_selection] declaration"
            )
        })?;
    let mut policy: ChangedSurfacePolicy = value
        .try_into()
        .map_err(|error| format!("invalid selector declaration: {error}"))?;
    validate_policy(&policy)?;
    validate_secondary_targets(&root, target, &policy)?;
    policy.secondary_contract_digests = secondary_contract_digests(&root, &policy)?;
    Ok(policy)
}

fn secondary_contract_digests(
    root: &toml::Table,
    policy: &ChangedSurfacePolicy,
) -> Result<BTreeMap<String, String>, String> {
    let required = policy
        .families
        .iter()
        .filter_map(|family| family.required_secondary_target.as_deref())
        .collect::<BTreeSet<_>>();
    if required.is_empty() {
        return Ok(BTreeMap::new());
    }
    let resolved = crate::executor::dispatch::resolve_targets_from_table(
        root,
        crate::job::ValidationMode::Full,
    )
    .map_err(|error| format!("resolve authenticated-base target contracts: {error}"))?;
    required
        .into_iter()
        .map(|name| {
            let target = resolved
                .iter()
                .find(|target| target.name == name)
                .ok_or_else(|| format!("required secondary target {name:?} did not resolve"))?;
            match &target.validation {
                crate::executor::dispatch::ResolvedValidation::Local(validation)
                    if !validation.prepared_state_enabled => {}
                crate::executor::dispatch::ResolvedValidation::Local(_) => {
                    return Err(format!(
                        "required secondary target {name:?} must disable prepared-state reuse"
                    ));
                }
                _ => {
                    return Err(format!(
                        "required secondary target {name:?} must use a concrete local validation contract"
                    ));
                }
            }
            let digest =
                crate::queue_request::validation_contract_digest(target).ok_or_else(|| {
                    format!("required secondary target {name:?} has no typed validation contract")
                })?;
            Ok((name.to_owned(), digest))
        })
        .collect()
}

fn validate_secondary_targets(
    root: &toml::Table,
    current_target: &str,
    policy: &ChangedSurfacePolicy,
) -> Result<(), String> {
    let targets = root
        .get("targets")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "base config has no targets table".to_owned())?;
    for family in &policy.families {
        let Some(required) = family.required_secondary_target.as_deref() else {
            continue;
        };
        if required == current_target {
            return Err(format!(
                "family {} secondary target must differ from the current target",
                family.name
            ));
        }
        let table = targets
            .get(required)
            .and_then(toml::Value::as_table)
            .ok_or_else(|| {
                format!(
                    "family {} required secondary target {required:?} is not declared",
                    family.name
                )
            })?;
        if crate::lane_policy::resolve_lane_policy_from_table(root, None).is_advisory(required) {
            return Err(format!(
                "family {} required secondary target {required:?} must not be advisory",
                family.name
            ));
        }
        let declared_build_type = table
            .get("validation_build_type")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "family {} required secondary target {required:?} has no validation_build_type",
                    family.name
                )
            })?;
        let required_build_type = family.required_secondary_build_type.ok_or_else(|| {
            format!(
                "family {} has no typed secondary build requirement",
                family.name
            )
        })?;
        if declared_build_type != build_type_name(required_build_type) {
            return Err(format!(
                "family {} required secondary target {required:?} declares validation_build_type {declared_build_type:?}, expected {:?}",
                family.name, required_build_type
            ));
        }
    }
    let current_build_type = targets
        .get(current_target)
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("validation_build_type"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("target {current_target:?} has no validation_build_type"))?;
    if current_build_type != build_type_name(policy.build_type) {
        return Err(format!(
            "target {current_target:?} declares validation_build_type {current_build_type:?}, expected {:?}",
            policy.build_type
        ));
    }
    Ok(())
}

fn build_type_name(build_type: BuildType) -> &'static str {
    match build_type {
        BuildType::Debug => "debug",
        BuildType::Release => "release",
        BuildType::RelWithDebInfo => "rel_with_deb_info",
        BuildType::MinSizeRel => "min_size_rel",
    }
}

/// Validate an exact-head input and compute a shadow-only selection receipt.
pub fn plan_selection(
    input: &ExactHeadInput,
    policy: Result<ChangedSurfacePolicy, String>,
) -> Result<SelectionReceipt, IdentityError> {
    validate_identity(input)?;
    let changed_paths = normalized_paths(&input.remote_changed_paths);
    let receipt = base_receipt(input, changed_paths.clone());
    if let Some(reason) = provenance_fallback(input, &changed_paths) {
        return Ok(fallback(receipt, None, reason, None));
    }
    plan_with_policy(receipt, policy, &changed_paths, input)
}

fn plan_with_policy(
    mut receipt: SelectionReceipt,
    policy: Result<ChangedSurfacePolicy, String>,
    changed_paths: &[String],
    input: &ExactHeadInput,
) -> Result<SelectionReceipt, IdentityError> {
    let policy = match validated_policy(policy) {
        Ok(policy) => policy,
        Err(detail) => {
            return Ok(fallback(
                receipt,
                None,
                FallbackReason::InvalidPolicy,
                Some(detail),
            ));
        }
    };
    receipt.policy_digest = Some(policy_digest(&policy));
    receipt.full_count = Some(policy.full_test_count);
    receipt.build_type = Some(policy.build_type);
    receipt.build_flags.clone_from(&policy.build_flags);
    receipt.baseline_tests = sorted_unique(&policy.baseline_tests);
    receipt.selected_build_targets = sorted_unique(&policy.baseline_build_targets);

    if input.base_tracked_paths_status == ObservationStatus::Incomplete
        || input.base_tracked_paths.is_empty()
    {
        return Ok(fallback(
            receipt,
            Some(&policy),
            FallbackReason::AmbiguousDiff,
            Some("authenticated base-tree inventory is incomplete".to_owned()),
        ));
    }
    if baseline_only_covers_base(&policy, &input.base_tracked_paths)? {
        return Ok(fallback(
            receipt,
            Some(&policy),
            FallbackReason::InvalidPolicy,
            Some("baseline_only_paths collectively cover the authenticated base tree".to_owned()),
        ));
    }

    let policy_patterns = std::iter::once(".shipyard/config.toml".to_owned())
        .chain(policy.policy_paths.iter().cloned())
        .collect::<Vec<_>>();
    let forced_fallback = if paths_match_any(changed_paths, &policy_patterns)? {
        Some((FallbackReason::SelectorPolicyChanged, None))
    } else if paths_match_any(changed_paths, &policy.test_topology_paths)? {
        Some((FallbackReason::TestTopologyChanged, None))
    } else if paths_match_any(changed_paths, &policy.full_required_paths)? {
        Some((FallbackReason::FullRequiredSurface, None))
    } else {
        None
    };

    select_policy_families(receipt, &policy, changed_paths, input, forced_fallback)
}

fn select_policy_families(
    receipt: SelectionReceipt,
    policy: &ChangedSurfacePolicy,
    changed_paths: &[String],
    input: &ExactHeadInput,
    mut forced_fallback: Option<(FallbackReason, Option<String>)>,
) -> Result<SelectionReceipt, IdentityError> {
    let mut selected_tests = literal_set(&receipt.baseline_tests);
    let mut selected_build_targets = literal_set(&receipt.selected_build_targets);
    let mut selected_families = Vec::new();
    let mut family_coverage = BTreeMap::new();
    let mut secondary = BTreeMap::<(String, BuildType, String), SecondaryProofReceipt>::new();
    let mut mapped_paths = forced_full_paths(policy, changed_paths)?;
    let mut selection_tier = SelectionTier::Mandatory;
    let globally_forced_full = forced_fallback.is_some();
    for family in &policy.families {
        let affected = matching_paths(changed_paths, &family.paths)?;
        let directly_affected = !affected.is_empty();
        let requires_global_secondary =
            globally_forced_full && !family.supported_build_types.contains(&policy.build_type);
        if !directly_affected && !requires_global_secondary {
            continue;
        }
        mapped_paths.extend(affected);
        selected_families.push(family.name.clone());
        let (family_tier, tests) = selected_family_tests(family);
        selection_tier = selection_tier.max(family_tier);
        family_coverage.insert(family.name.clone(), tests.len());
        if family_tier == SelectionTier::Full {
            forced_fallback.get_or_insert_with(|| {
                (
                    FallbackReason::HighRiskFamily,
                    Some(format!("affected high-risk family: {}", family.name)),
                )
            });
        }
        if !family.supported_build_types.contains(&policy.build_type) {
            if let Err(detail) =
                record_required_secondary(family, &tests, input, policy, &mut secondary)
            {
                return Ok(blocked(receipt, policy, detail));
            }
            continue;
        }
        if directly_affected && family_tier != SelectionTier::Full {
            selected_tests.extend(tests);
            selected_build_targets.extend(family.build_targets.iter().cloned());
        }
    }
    mapped_paths.extend(matching_paths(changed_paths, &policy.baseline_only_paths)?);
    if let Some((reason, detail)) = forced_fallback {
        return Ok(finalize_full_receipt(
            receipt,
            policy,
            selected_families,
            family_coverage,
            secondary,
            reason,
            detail,
        ));
    }
    let unmapped = unmapped_changed_paths(changed_paths, &mapped_paths);
    if !unmapped.is_empty() {
        return Ok(fallback(
            receipt,
            Some(policy),
            FallbackReason::UnmappedChangedPath,
            Some(format!("unmapped paths: {}", unmapped.join(", "))),
        ));
    }

    Ok(finalize_bounded_receipt(
        receipt,
        selected_tests,
        selected_build_targets,
        selected_families,
        family_coverage,
        secondary,
        selection_tier,
    ))
}

fn record_required_secondary(
    family: &TestFamily,
    tests: &[String],
    input: &ExactHeadInput,
    policy: &ChangedSurfacePolicy,
    secondary: &mut BTreeMap<(String, BuildType, String), SecondaryProofReceipt>,
) -> Result<(), String> {
    let required_target = family.required_secondary_target.as_ref().ok_or_else(|| {
        format!(
            "family {:?} is incompatible with {:?} and has no required secondary target",
            family.name, policy.build_type
        )
    })?;
    let required_build_type = family.required_secondary_build_type.ok_or_else(|| {
        format!(
            "family {:?} has no typed secondary build requirement",
            family.name
        )
    })?;
    let proof = required_secondary_proof(input, policy, required_target, required_build_type)
        .ok_or_else(|| {
            format!(
                "family {:?} requires fresh exact-head evidence from target {:?} at {:?}",
                family.name, required_target, required_build_type
            )
        })?;
    let key = (
        required_target.clone(),
        proof.build_type,
        proof.head_sha.clone(),
    );
    let entry = secondary
        .entry(key)
        .or_insert_with(|| SecondaryProofReceipt {
            target: required_target.clone(),
            build_type: proof.build_type,
            head_sha: proof.head_sha.clone(),
            tree_sha: proof.tree_sha.clone(),
            full_execution: proof.full_execution,
            completed_at: proof.completed_at,
            contract_digest: proof.contract_digest.clone(),
            families: Vec::new(),
            tests: Vec::new(),
        });
    entry.families.push(family.name.clone());
    entry.tests.extend(tests.iter().cloned());
    Ok(())
}

fn unmapped_changed_paths(paths: &[String], mapped: &BTreeSet<String>) -> Vec<String> {
    paths
        .iter()
        .filter(|path| !mapped.contains(*path))
        .cloned()
        .collect()
}

fn forced_full_paths(
    policy: &ChangedSurfacePolicy,
    changed_paths: &[String],
) -> Result<BTreeSet<String>, IdentityError> {
    let patterns = std::iter::once(".shipyard/config.toml".to_owned())
        .chain(policy.policy_paths.iter().cloned())
        .chain(policy.test_topology_paths.iter().cloned())
        .chain(policy.full_required_paths.iter().cloned())
        .collect::<Vec<_>>();
    matching_paths(changed_paths, &patterns)
}

fn literal_set(tests: &[String]) -> BTreeSet<String> {
    tests.iter().cloned().collect()
}

fn selected_family_tests(family: &TestFamily) -> (SelectionTier, Vec<String>) {
    let tier = match family.risk_class {
        RiskClass::Low => SelectionTier::Affected,
        RiskClass::Medium => SelectionTier::Extended,
        RiskClass::High => SelectionTier::Full,
    };
    let tests = family
        .tests
        .iter()
        .chain(
            (family.risk_class == RiskClass::Medium)
                .then_some(family.extended_tests.iter())
                .into_iter()
                .flatten(),
        )
        .cloned()
        .collect::<Vec<_>>();
    (tier, sorted_unique(&tests))
}

fn required_secondary_proof<'a>(
    input: &'a ExactHeadInput,
    policy: &ChangedSurfacePolicy,
    target: &str,
    build_type: BuildType,
) -> Option<&'a SecondaryProof> {
    let expected_contract = policy.secondary_contract_digests.get(target)?;
    input.secondary_proofs.iter().find(|proof| {
        proof.target == target
            && proof.head_sha == input.pr_head_sha
            && proof.tree_sha == input.remote_tree_sha
            && proof.full_execution
            && proof.build_type == build_type
            && proof.passed
            && !proof.reused
            && proof.contract_digest.as_ref() == Some(expected_contract)
            && proof.completed_at <= input.observed_at
            && proof.completed_at
                >= input.observed_at - chrono::Duration::hours(SECONDARY_PROOF_MAX_AGE_HOURS)
    })
}

fn finalize_bounded_receipt(
    mut receipt: SelectionReceipt,
    selected_tests: BTreeSet<String>,
    selected_build_targets: BTreeSet<String>,
    selected_families: Vec<String>,
    family_coverage: BTreeMap<String, usize>,
    secondary: BTreeMap<(String, BuildType, String), SecondaryProofReceipt>,
    selection_tier: SelectionTier,
) -> SelectionReceipt {
    receipt.selected_families = selected_families;
    receipt.selected_tests = selected_tests.into_iter().collect();
    receipt.selected_build_targets = selected_build_targets.into_iter().collect();
    receipt.family_coverage = family_coverage;
    receipt.secondary_proofs = finalized_secondary_proofs(secondary);
    receipt.selected_count = Some(receipt.selected_tests.len());
    receipt.planned_suite = PlannedSuite::Bounded;
    receipt.selection_tier = selection_tier;
    receipt
}

fn finalize_full_receipt(
    mut receipt: SelectionReceipt,
    policy: &ChangedSurfacePolicy,
    selected_families: Vec<String>,
    family_coverage: BTreeMap<String, usize>,
    secondary: BTreeMap<(String, BuildType, String), SecondaryProofReceipt>,
    reason: FallbackReason,
    detail: Option<String>,
) -> SelectionReceipt {
    receipt = fallback(receipt, Some(policy), reason, detail);
    receipt.selected_families = selected_families;
    receipt.family_coverage = family_coverage;
    receipt.secondary_proofs = finalized_secondary_proofs(secondary);
    receipt
}

fn finalized_secondary_proofs(
    secondary: BTreeMap<(String, BuildType, String), SecondaryProofReceipt>,
) -> Vec<SecondaryProofReceipt> {
    secondary
        .into_values()
        .map(|mut proof| {
            proof.families.sort();
            proof.families.dedup();
            proof.tests.sort();
            proof.tests.dedup();
            proof
        })
        .collect()
}

fn validated_policy(
    policy: Result<ChangedSurfacePolicy, String>,
) -> Result<ChangedSurfacePolicy, String> {
    let policy = policy?;
    validate_policy(&policy)?;
    Ok(policy)
}

fn blocked(
    mut receipt: SelectionReceipt,
    policy: &ChangedSurfacePolicy,
    detail: String,
) -> SelectionReceipt {
    receipt.policy_digest = Some(policy_digest(policy));
    receipt.full_count = Some(policy.full_test_count);
    receipt.build_type = Some(policy.build_type);
    receipt.build_flags.clone_from(&policy.build_flags);
    receipt.baseline_tests = sorted_unique(&policy.baseline_tests);
    receipt.planned_suite = PlannedSuite::Blocked;
    receipt.selection_tier = SelectionTier::Full;
    receipt.selected_count = None;
    receipt.fallback_reason = None;
    receipt.fallback_detail = Some(detail);
    "blocked_required_secondary_proof".clone_into(&mut receipt.outcomes.planner);
    receipt
}

fn baseline_only_covers_base(
    policy: &ChangedSurfacePolicy,
    base_paths: &[String],
) -> Result<bool, IdentityError> {
    let matched = matching_paths(base_paths, &policy.baseline_only_paths)?;
    Ok(!base_paths.is_empty() && matched.len() == normalized_paths(base_paths).len())
}

fn provenance_fallback(input: &ExactHeadInput, changed_paths: &[String]) -> Option<FallbackReason> {
    if input.protected_ref_status == ProtectedRefStatus::Unresolved
        || !valid_sha(&input.protected_ref_sha)
    {
        return Some(FallbackReason::BaseRefUnresolved);
    }
    if input.protected_ref_status == ProtectedRefStatus::Unprotected {
        return Some(FallbackReason::BaseRefNotProtected);
    }
    if input.pr_base_sha != input.protected_ref_sha {
        return Some(FallbackReason::StaleBase);
    }
    if input.pr_base_sha == input.pr_head_sha {
        return Some(FallbackReason::BaseEqualsHead);
    }
    if !input.merge_base_is_ancestor {
        return Some(FallbackReason::AncestryMismatch);
    }
    if !valid_sha(&input.local_merge_base_sha)
        || !valid_sha(&input.remote_merge_base_sha)
        || input.local_merge_base_sha != input.remote_merge_base_sha
    {
        return Some(FallbackReason::MergeBaseMismatch);
    }
    if input.local_merge_base_sha != input.pr_base_sha {
        return Some(FallbackReason::BasePolicyMismatch);
    }
    if input.remote_changed_paths_status == ObservationStatus::Incomplete
        || input.local_changed_paths_status == ObservationStatus::Incomplete
    {
        return Some(FallbackReason::AmbiguousDiff);
    }
    (changed_paths != normalized_paths(&input.local_changed_paths))
        .then_some(FallbackReason::ChangedPathsMismatch)
}

/// Reject a receipt whose exact identity no longer matches the validation input.
pub fn verify_receipt_identity(
    receipt: &SelectionReceipt,
    input: &ExactHeadInput,
) -> Result<(), IdentityError> {
    validate_identity(input)?;
    let mismatches = [
        (
            "repository",
            receipt.repository.as_str(),
            input.repository.as_str(),
        ),
        ("target", receipt.target.as_str(), input.target.as_str()),
        (
            "base_ref",
            receipt.protected_ref.as_str(),
            input.base_ref.as_str(),
        ),
        (
            "base_sha",
            receipt.pr_base_sha.as_str(),
            input.pr_base_sha.as_str(),
        ),
        (
            "protected_ref_sha",
            receipt.protected_ref_sha.as_str(),
            input.protected_ref_sha.as_str(),
        ),
        (
            "merge_base_sha",
            receipt.merge_base_sha.as_str(),
            input.local_merge_base_sha.as_str(),
        ),
        (
            "head_sha",
            receipt.head_sha.as_str(),
            input.pr_head_sha.as_str(),
        ),
        (
            "tree_sha",
            receipt.tree_sha.as_str(),
            input.remote_tree_sha.as_str(),
        ),
    ]
    .into_iter()
    .filter(|(_, receipt_value, input_value)| receipt_value != input_value)
    .map(|(name, _, _)| name)
    .collect::<Vec<_>>();
    if receipt.pull_request != input.pull_request {
        return Err(IdentityError::Unresolved(
            "receipt pull-request identity mismatch".to_owned(),
        ));
    }
    if !mismatches.is_empty() {
        return Err(IdentityError::Unresolved(format!(
            "receipt identity mismatch: {}",
            mismatches.join(", ")
        )));
    }
    if receipt.changed_paths_digest != digest_lines(&normalized_paths(&input.remote_changed_paths))
    {
        return Err(IdentityError::Unresolved(
            "receipt changed-path digest mismatch".to_owned(),
        ));
    }
    if receipt.secondary_proofs.iter().any(|required| {
        !input.secondary_proofs.iter().any(|proof| {
            proof.target == required.target
                && proof.build_type == required.build_type
                && proof.head_sha == required.head_sha
                && proof.tree_sha == required.tree_sha
                && proof.full_execution == required.full_execution
                && proof.passed
                && !proof.reused
                && proof.completed_at == required.completed_at
                && proof.contract_digest == required.contract_digest
                && proof.completed_at <= input.observed_at
                && proof.completed_at
                    >= input.observed_at - chrono::Duration::hours(SECONDARY_PROOF_MAX_AGE_HOURS)
        })
    }) {
        return Err(IdentityError::Unresolved(
            "receipt required-secondary-proof identity mismatch".to_owned(),
        ));
    }
    Ok(())
}

fn validate_identity(input: &ExactHeadInput) -> Result<(), IdentityError> {
    if input.pull_request == 0
        || !valid_repo(&input.repository)
        || input.target.is_empty()
        || input.base_ref.trim().is_empty()
    {
        return Err(IdentityError::Unresolved(
            "repository, PR number, or base ref is missing".to_owned(),
        ));
    }
    for (name, sha) in [
        ("PR base", &input.pr_base_sha),
        ("PR head", &input.pr_head_sha),
        ("remote tree", &input.remote_tree_sha),
        ("local HEAD", &input.local_head_sha),
        ("local tree", &input.local_tree_sha),
    ] {
        if !valid_sha(sha) {
            return Err(IdentityError::Unresolved(format!(
                "{name} SHA is missing or malformed"
            )));
        }
    }
    if input.local_head_sha != input.pr_head_sha {
        return Err(IdentityError::HeadMismatch {
            expected: input.pr_head_sha.clone(),
            observed: input.local_head_sha.clone(),
        });
    }
    if input.local_tree_sha != input.remote_tree_sha {
        return Err(IdentityError::TreeMismatch {
            expected: input.remote_tree_sha.clone(),
            observed: input.local_tree_sha.clone(),
        });
    }
    if !input.checkout_clean {
        return Err(IdentityError::DirtyCheckout);
    }
    Ok(())
}

fn validate_policy(policy: &ChangedSurfacePolicy) -> Result<(), String> {
    if !(MIN_CHANGED_SURFACE_SCHEMA_VERSION..=CHANGED_SURFACE_SCHEMA_VERSION)
        .contains(&policy.schema_version)
    {
        return Err(format!(
            "unsupported changed-surface schema version {}",
            policy.schema_version
        ));
    }
    validate_risk_policy(policy)?;
    if let Some(execution) = &policy.execution {
        execution.validate(policy.schema_version)?;
    }
    if policy.full_test_count == 0 {
        return Err("full_test_count must be nonzero".to_owned());
    }
    validate_literal_tests("baseline_tests", &policy.baseline_tests)?;
    if policy.schema_version >= 3 {
        validate_build_targets("baseline_build_targets", &policy.baseline_build_targets)?;
    }
    if policy.families.is_empty() {
        return Err("at least one test family is required".to_owned());
    }
    if policy.test_topology_paths.is_empty() {
        return Err("test_topology_paths must be nonempty".to_owned());
    }
    let mut names = BTreeSet::new();
    for family in &policy.families {
        if family.name.trim().is_empty() || !names.insert(family.name.as_str()) {
            return Err("family names must be nonempty and unique".to_owned());
        }
        validate_patterns(&family.paths)?;
        validate_literal_tests(&format!("family {} tests", family.name), &family.tests)?;
        if policy.schema_version >= 3 {
            validate_build_targets(
                &format!("family {} build_targets", family.name),
                &family.build_targets,
            )?;
        }
        if family.supported_build_types.is_empty() {
            return Err(format!(
                "family {} supported_build_types must be nonempty",
                family.name
            ));
        }
        if !family.supported_build_types.contains(&policy.build_type)
            && (family
                .required_secondary_target
                .as_deref()
                .is_none_or(str::is_empty)
                || family.required_secondary_build_type.is_none())
        {
            return Err(format!(
                "family {} is incompatible with the target build type and requires required_secondary_target",
                family.name
            ));
        }
        if let Some(build_type) = family.required_secondary_build_type
            && !family.supported_build_types.contains(&build_type)
        {
            return Err(format!(
                "family {} secondary build type must be supported by the family",
                family.name
            ));
        }
    }
    validate_patterns(&policy.baseline_only_paths)?;
    validate_patterns(&policy.ios_compile_skip_safe_paths)?;
    validate_patterns(&policy.policy_paths)?;
    validate_patterns(&policy.test_topology_paths)?;
    if policy.baseline_only_paths.iter().any(|pattern| {
        pattern.split('/').next().is_none_or(|first| {
            first
                .chars()
                .any(|character| matches!(character, '*' | '?' | '['))
        })
    }) {
        return Err(
            "baseline_only_paths must start with a literal top-level path component".to_owned(),
        );
    }
    let declared = policy
        .baseline_tests
        .iter()
        .chain(
            policy
                .families
                .iter()
                .flat_map(|family| family.tests.iter().chain(family.extended_tests.iter())),
        )
        .collect::<BTreeSet<_>>()
        .len();
    if declared > policy.full_test_count {
        return Err("declared literal tests exceed full_test_count".to_owned());
    }
    Ok(())
}

fn validate_risk_policy(policy: &ChangedSurfacePolicy) -> Result<(), String> {
    if policy.schema_version == 1
        && (!policy.full_required_paths.is_empty()
            || policy.families.iter().any(|family| {
                family.risk_class != RiskClass::Low || !family.extended_tests.is_empty()
            }))
    {
        return Err("risk tiers and full_required_paths require schema_version = 2".to_owned());
    }
    validate_patterns(&policy.full_required_paths)?;
    for family in &policy.families {
        if family.risk_class == RiskClass::Medium {
            validate_literal_tests(
                &format!("family {} extended_tests", family.name),
                &family.extended_tests,
            )?;
        } else if !family.extended_tests.is_empty() {
            return Err(format!(
                "family {} extended_tests require risk_class = medium",
                family.name
            ));
        }
    }
    Ok(())
}

fn validate_literal_tests(label: &str, tests: &[String]) -> Result<(), String> {
    if tests.is_empty() || tests.iter().any(|test| test.trim().is_empty()) {
        return Err(format!("{label} must contain nonempty literal test names"));
    }
    if tests.iter().collect::<BTreeSet<_>>().len() != tests.len() {
        return Err(format!("{label} contains duplicate test names"));
    }
    Ok(())
}

fn validate_build_targets(label: &str, targets: &[String]) -> Result<(), String> {
    validate_literal_tests(label, targets)?;
    if targets.iter().any(|target| {
        target.starts_with('-')
            || !target.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-' | b':')
            })
    }) {
        return Err(format!(
            "{label} must contain only canonical CMake target names"
        ));
    }
    Ok(())
}

fn validate_patterns(patterns: &[String]) -> Result<(), String> {
    for pattern in patterns {
        if pattern.trim().is_empty()
            || Path::new(pattern).is_absolute()
            || pattern.split('/').any(|part| part == "..")
        {
            return Err(format!(
                "invalid repository-relative path pattern {pattern:?}"
            ));
        }
        Pattern::new(pattern)
            .map_err(|error| format!("invalid path pattern {pattern:?}: {error}"))?;
    }
    Ok(())
}

fn base_receipt(input: &ExactHeadInput, changed_paths: Vec<String>) -> SelectionReceipt {
    SelectionReceipt {
        schema_version: SELECTION_RECEIPT_SCHEMA_VERSION,
        exact_head_verified: true,
        shadow_only: true,
        repository: input.repository.clone(),
        pull_request: input.pull_request,
        target: input.target.clone(),
        protected_ref: input.base_ref.clone(),
        pr_base_sha: input.pr_base_sha.clone(),
        protected_ref_sha: input.protected_ref_sha.clone(),
        merge_base_sha: input.local_merge_base_sha.clone(),
        head_sha: input.pr_head_sha.clone(),
        tree_sha: input.remote_tree_sha.clone(),
        changed_paths_digest: digest_lines(&changed_paths),
        policy_digest: None,
        build_type: None,
        build_flags: Vec::new(),
        changed_paths,
        selected_families: Vec::new(),
        selected_tests: Vec::new(),
        selected_build_targets: Vec::new(),
        baseline_tests: Vec::new(),
        family_coverage: BTreeMap::new(),
        secondary_proofs: Vec::new(),
        planned_suite: PlannedSuite::Full,
        selection_tier: SelectionTier::Full,
        authoritative_suite: PlannedSuite::Full,
        outcomes: SelectionOutcomes {
            planner: "planned".to_owned(),
            authoritative_execution: "not_observed_by_shadow_planner".to_owned(),
        },
        selected_count: None,
        full_count: None,
        fallback_reason: None,
        fallback_detail: None,
        elapsed_ms: 0,
    }
}

fn fallback(
    mut receipt: SelectionReceipt,
    policy: Option<&ChangedSurfacePolicy>,
    reason: FallbackReason,
    detail: Option<String>,
) -> SelectionReceipt {
    if let Some(policy) = policy {
        receipt.full_count = Some(policy.full_test_count);
        receipt.selected_count = Some(policy.full_test_count);
        receipt.build_type = Some(policy.build_type);
        receipt.build_flags.clone_from(&policy.build_flags);
        receipt.baseline_tests = sorted_unique(&policy.baseline_tests);
        receipt
            .policy_digest
            .get_or_insert_with(|| policy_digest(policy));
    }
    receipt.planned_suite = PlannedSuite::Full;
    receipt.selection_tier = SelectionTier::Full;
    receipt.authoritative_suite = PlannedSuite::Full;
    receipt.fallback_reason = Some(reason);
    receipt.fallback_detail = detail;
    receipt
}

fn matching_paths(
    paths: &[String],
    patterns: &[String],
) -> Result<BTreeSet<String>, IdentityError> {
    let compiled = patterns
        .iter()
        .map(|pattern| {
            Pattern::new(pattern).map_err(|error| {
                IdentityError::Unresolved(format!("invalid base-owned path pattern: {error}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(paths
        .iter()
        .filter(|path| compiled.iter().any(|pattern| pattern.matches(path)))
        .cloned()
        .collect())
}

fn paths_match_any(paths: &[String], patterns: &[String]) -> Result<bool, IdentityError> {
    matching_paths(paths, patterns).map(|matches| !matches.is_empty())
}

fn normalized_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|path| !path.is_empty())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn sorted_unique(values: &[String]) -> Vec<String> {
    values
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn policy_digest(policy: &ChangedSurfacePolicy) -> String {
    let bytes = serde_json::to_vec(policy).expect("policy serialization should not fail");
    hex::encode(Sha256::digest(bytes))
}

fn digest_lines(lines: &[String]) -> String {
    let mut hasher = Sha256::new();
    for line in lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\0");
    }
    hex::encode(hasher.finalize())
}

fn valid_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_repo(value: &str) -> bool {
    let mut parts = value.split('/');
    parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_some_and(|part| !part.is_empty())
        && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn policy() -> ChangedSurfacePolicy {
        ChangedSurfacePolicy {
            schema_version: 1,
            full_test_count: 100,
            build_type: BuildType::Debug,
            build_flags: vec!["-DCMAKE_BUILD_TYPE=Debug".to_owned()],
            baseline_tests: vec!["smoke boots".to_owned(), "smoke config".to_owned()],
            baseline_build_targets: Vec::new(),
            baseline_only_paths: vec!["docs/**".to_owned()],
            ios_compile_skip_safe_paths: Vec::new(),
            full_required_paths: Vec::new(),
            policy_paths: vec!["schema/changed-surface.json".to_owned()],
            test_topology_paths: vec![
                "tests/CMakeLists.txt".to_owned(),
                "tests/**/registry.rs".to_owned(),
            ],
            families: vec![
                TestFamily {
                    name: "audio".to_owned(),
                    paths: vec!["src/audio/**".to_owned()],
                    tests: vec!["audio alpha".to_owned(), "audio beta".to_owned()],
                    build_targets: Vec::new(),
                    risk_class: RiskClass::Low,
                    extended_tests: Vec::new(),
                    supported_build_types: vec![BuildType::Debug, BuildType::Release],
                    required_secondary_target: None,
                    required_secondary_build_type: None,
                },
                TestFamily {
                    name: "registry".to_owned(),
                    paths: vec![
                        "src/registry/**".to_owned(),
                        "include/registry/**".to_owned(),
                    ],
                    tests: vec!["registry one".to_owned(), "registry two".to_owned()],
                    build_targets: Vec::new(),
                    risk_class: RiskClass::Low,
                    extended_tests: Vec::new(),
                    supported_build_types: vec![BuildType::Debug, BuildType::Release],
                    required_secondary_target: None,
                    required_secondary_build_type: None,
                },
            ],
            execution: None,
            secondary_contract_digests: BTreeMap::new(),
        }
    }

    fn input(paths: &[&str]) -> ExactHeadInput {
        ExactHeadInput {
            repository: "owner/repo".to_owned(),
            pull_request: 42,
            target: "mac-debug".to_owned(),
            observed_at: Utc::now(),
            base_ref: "main".to_owned(),
            pr_base_sha: A.to_owned(),
            protected_ref_sha: A.to_owned(),
            protected_ref_status: ProtectedRefStatus::Protected,
            pr_head_sha: B.to_owned(),
            remote_tree_sha: C.to_owned(),
            local_head_sha: B.to_owned(),
            local_tree_sha: C.to_owned(),
            local_merge_base_sha: A.to_owned(),
            remote_merge_base_sha: A.to_owned(),
            merge_base_is_ancestor: true,
            checkout_clean: true,
            remote_changed_paths: paths.iter().map(ToString::to_string).collect(),
            remote_changed_paths_status: ObservationStatus::Complete,
            local_changed_paths: paths.iter().map(ToString::to_string).collect(),
            local_changed_paths_status: ObservationStatus::Complete,
            base_tracked_paths: vec![
                "src/audio/processor.rs".to_owned(),
                "src/registry/index.rs".to_owned(),
                "docs/guide.md".to_owned(),
                ".shipyard/config.toml".to_owned(),
            ],
            base_tracked_paths_status: ObservationStatus::Complete,
            secondary_proofs: Vec::new(),
        }
    }

    #[test]
    fn exact_head_selects_baseline_and_complete_affected_families() {
        let receipt = plan_selection(
            &input(&["src/audio/processor.rs", "include/registry/api.hpp"]),
            Ok(policy()),
        )
        .expect("plan");
        assert_eq!(receipt.planned_suite, PlannedSuite::Bounded);
        assert_eq!(receipt.authoritative_suite, PlannedSuite::Full);
        assert_eq!(receipt.selected_families, ["audio", "registry"]);
        assert_eq!(receipt.baseline_tests.len(), 2);
        assert_eq!(receipt.selected_tests.len(), 6);
        assert_eq!(receipt.family_coverage["audio"], 2);
        assert_eq!(receipt.family_coverage["registry"], 2);
        assert_eq!(receipt.selection_tier, SelectionTier::Affected);
    }

    #[test]
    fn baseline_runs_when_only_reviewed_docs_change() {
        let receipt = plan_selection(&input(&["docs/guide.md"]), Ok(policy())).expect("plan");
        assert_eq!(receipt.planned_suite, PlannedSuite::Bounded);
        assert!(receipt.selected_families.is_empty());
        assert_eq!(receipt.selected_tests, receipt.baseline_tests);
        assert_eq!(receipt.selected_count, Some(2));
        assert_eq!(receipt.selection_tier, SelectionTier::Mandatory);
    }

    #[test]
    fn schema_v2_selects_extended_neighbors_only_for_medium_risk() {
        let mut risk_policy = policy();
        risk_policy.schema_version = 2;
        risk_policy.families[0].risk_class = RiskClass::Medium;
        risk_policy.families[0].extended_tests = vec![
            "audio integration consumer".to_owned(),
            "audio prior co-failure".to_owned(),
        ];
        let receipt =
            plan_selection(&input(&["src/audio/processor.rs"]), Ok(risk_policy)).expect("plan");
        assert_eq!(receipt.planned_suite, PlannedSuite::Bounded);
        assert_eq!(receipt.selection_tier, SelectionTier::Extended);
        assert!(
            receipt
                .selected_tests
                .contains(&"audio integration consumer".to_owned())
        );
        assert_eq!(receipt.family_coverage["audio"], 4);
    }

    #[test]
    fn schema_v2_high_risk_and_full_required_surfaces_fail_closed() {
        let mut high_risk = policy();
        high_risk.schema_version = 2;
        high_risk.families[0].risk_class = RiskClass::High;
        let receipt = plan_selection(&input(&["src/audio/processor.rs"]), Ok(high_risk))
            .expect("full fallback");
        assert_eq!(receipt.selection_tier, SelectionTier::Full);
        assert_eq!(
            receipt.fallback_reason,
            Some(FallbackReason::HighRiskFamily)
        );

        let mut full_surface = policy();
        full_surface.schema_version = 2;
        full_surface.full_required_paths = vec!["src/registry/**".to_owned()];
        let receipt = plan_selection(&input(&["src/registry/index.rs"]), Ok(full_surface))
            .expect("full fallback");
        assert_eq!(receipt.selection_tier, SelectionTier::Full);
        assert_eq!(
            receipt.fallback_reason,
            Some(FallbackReason::FullRequiredSurface)
        );
    }

    #[test]
    fn schema_v1_rejects_v2_risk_fields() {
        let mut invalid = policy();
        invalid.full_required_paths = vec!["src/audio/**".to_owned()];
        assert!(validate_policy(&invalid).is_err());

        invalid.full_required_paths.clear();
        invalid.families[0].risk_class = RiskClass::Medium;
        invalid.families[0].extended_tests = vec!["audio neighbor".to_owned()];
        assert!(validate_policy(&invalid).is_err());
    }

    #[test]
    fn identity_and_tree_mismatches_hard_fail_without_a_receipt() {
        let mut head = input(&["src/audio/a.rs"]);
        head.local_head_sha = C.to_owned();
        assert!(matches!(
            plan_selection(&head, Ok(policy())),
            Err(IdentityError::HeadMismatch { .. })
        ));
        let mut tree = input(&["src/audio/a.rs"]);
        tree.local_tree_sha = A.to_owned();
        assert!(matches!(
            plan_selection(&tree, Ok(policy())),
            Err(IdentityError::TreeMismatch { .. })
        ));
        let mut dirty = input(&["src/audio/a.rs"]);
        dirty.checkout_clean = false;
        assert_eq!(
            plan_selection(&dirty, Ok(policy())),
            Err(IdentityError::DirtyCheckout)
        );
    }

    #[test]
    fn policy_ambiguities_fall_back_only_after_identity_verification() {
        let cases = [
            ("unprotected", FallbackReason::BaseRefNotProtected),
            ("stale", FallbackReason::StaleBase),
            ("equal", FallbackReason::BaseEqualsHead),
            ("ancestry", FallbackReason::AncestryMismatch),
            ("merge", FallbackReason::MergeBaseMismatch),
            ("base-policy", FallbackReason::BasePolicyMismatch),
            ("paths", FallbackReason::ChangedPathsMismatch),
        ];
        for (case, reason) in cases {
            let mut value = input(&["src/audio/a.rs"]);
            match case {
                "unprotected" => value.protected_ref_status = ProtectedRefStatus::Unprotected,
                "stale" => value.protected_ref_sha = C.to_owned(),
                "equal" => value.pr_head_sha = A.to_owned(),
                "ancestry" => value.merge_base_is_ancestor = false,
                "merge" => value.remote_merge_base_sha = C.to_owned(),
                "base-policy" => {
                    value.local_merge_base_sha = C.to_owned();
                    value.remote_merge_base_sha = C.to_owned();
                }
                "paths" => value.local_changed_paths = vec!["different".to_owned()],
                _ => unreachable!(),
            }
            if case == "equal" {
                value.local_head_sha = A.to_owned();
            }
            let receipt = plan_selection(&value, Ok(policy())).expect("fallback receipt");
            assert_eq!(receipt.fallback_reason, Some(reason), "{case}");
            assert_eq!(receipt.planned_suite, PlannedSuite::Full, "{case}");
        }
    }

    #[test]
    fn head_side_policy_and_topology_changes_cannot_validate_themselves() {
        let policy_change =
            plan_selection(&input(&[".shipyard/config.toml"]), Ok(policy())).expect("fallback");
        assert_eq!(
            policy_change.fallback_reason,
            Some(FallbackReason::SelectorPolicyChanged)
        );
        let schema_change = plan_selection(&input(&["schema/changed-surface.json"]), Ok(policy()))
            .expect("fallback");
        assert_eq!(
            schema_change.fallback_reason,
            Some(FallbackReason::SelectorPolicyChanged)
        );
        let topology =
            plan_selection(&input(&["tests/CMakeLists.txt"]), Ok(policy())).expect("fallback");
        assert_eq!(
            topology.fallback_reason,
            Some(FallbackReason::TestTopologyChanged)
        );
        let policy_plus_unmapped = plan_selection(
            &input(&[".shipyard/config.toml", "unknown/new.rs"]),
            Ok(policy()),
        )
        .expect("policy fallback wins");
        assert_eq!(
            policy_plus_unmapped.fallback_reason,
            Some(FallbackReason::SelectorPolicyChanged)
        );
    }

    #[test]
    fn missing_invalid_unmapped_and_empty_family_inputs_fall_back_or_reject() {
        let missing = plan_selection(&input(&["src/audio/a.rs"]), Err("missing".to_owned()))
            .expect("fallback");
        assert_eq!(missing.fallback_reason, Some(FallbackReason::InvalidPolicy));

        let unmapped = plan_selection(&input(&["src/new/a.rs"]), Ok(policy())).expect("fallback");
        assert_eq!(
            unmapped.fallback_reason,
            Some(FallbackReason::UnmappedChangedPath)
        );

        let mut invalid = policy();
        invalid.families[0].tests.clear();
        assert!(validate_policy(&invalid).is_err());
        let malformed = plan_selection(&input(&["src/audio/a.rs"]), Ok(invalid))
            .expect("invalid policy falls back");
        assert_eq!(
            malformed.fallback_reason,
            Some(FallbackReason::InvalidPolicy)
        );
        let encoded = toml::to_string(&toml::toml! {
            [targets.mac.changed_surface_selection]
            schema_version = 1
            full_test_count = 1
            baseline_tests = ["smoke"]
            test_topology_paths = ["tests/**"]
            unexpected_regex = ".*"
        })
        .expect("toml");
        assert!(policy_from_toml(&encoded, "mac").is_err());
    }

    #[test]
    fn repository_mobile_skip_projection_is_parsed_and_bound_into_policy_digest() {
        let original = policy();
        let mut projected = original.clone();
        projected.ios_compile_skip_safe_paths = vec!["docs/**".to_owned()];
        let encoded = toml::to_string(&projected).expect("encode policy");
        let decoded: ChangedSurfacePolicy = toml::from_str(&encoded).expect("decode policy");
        assert_eq!(decoded.ios_compile_skip_safe_paths, vec!["docs/**"]);
        assert_ne!(policy_digest(&original), policy_digest(&decoded));
    }

    #[test]
    fn receipt_identity_mismatch_hard_fails() {
        let original = input(&["src/audio/a.rs"]);
        let receipt = plan_selection(&original, Ok(policy())).expect("receipt");
        verify_receipt_identity(&receipt, &original).expect("same identity");
        let mut changed = original;
        changed.pr_head_sha = C.to_owned();
        changed.local_head_sha = C.to_owned();
        assert!(verify_receipt_identity(&receipt, &changed).is_err());

        let mut changed_base = input(&["src/audio/a.rs"]);
        changed_base.protected_ref_sha = C.to_owned();
        assert!(verify_receipt_identity(&receipt, &changed_base).is_err());

        let mut changed_merge_base = input(&["src/audio/a.rs"]);
        changed_merge_base.local_merge_base_sha = C.to_owned();
        assert!(verify_receipt_identity(&receipt, &changed_merge_base).is_err());

        let mut changed_target = input(&["src/audio/a.rs"]);
        changed_target.target = "other-target".to_owned();
        assert!(verify_receipt_identity(&receipt, &changed_target).is_err());
    }

    #[test]
    fn policy_rejects_test_free_success_and_repository_wide_baseline_only() {
        let mut no_baseline = policy();
        no_baseline.baseline_tests.clear();
        assert!(validate_policy(&no_baseline).is_err());
        let mut no_family_tests = policy();
        no_family_tests.families[1].tests.clear();
        assert!(validate_policy(&no_family_tests).is_err());
        let mut bypass = policy();
        bypass.baseline_only_paths = vec!["**".to_owned()];
        assert!(validate_policy(&bypass).is_err());
        bypass.baseline_only_paths = vec!["?*".to_owned()];
        assert!(validate_policy(&bypass).is_err());
        bypass.baseline_only_paths = vec!["d*/**".to_owned()];
        assert!(validate_policy(&bypass).is_err());
    }

    #[test]
    fn incomplete_diff_and_unresolved_protected_ref_fall_back() {
        let mut incomplete = input(&["src/audio/a.rs"]);
        incomplete.remote_changed_paths_status = ObservationStatus::Incomplete;
        let receipt = plan_selection(&incomplete, Ok(policy())).expect("fallback");
        assert_eq!(receipt.fallback_reason, Some(FallbackReason::AmbiguousDiff));

        let mut unresolved = input(&["src/audio/a.rs"]);
        unresolved.protected_ref_sha.clear();
        unresolved.protected_ref_status = ProtectedRefStatus::Unresolved;
        let receipt = plan_selection(&unresolved, Ok(policy())).expect("fallback");
        assert_eq!(
            receipt.fallback_reason,
            Some(FallbackReason::BaseRefUnresolved)
        );
    }

    #[test]
    fn authenticated_path_identity_is_not_rewritten() {
        let paths = vec!["docs\\literal ".to_owned(), "docs/literal".to_owned()];
        let normalized = normalized_paths(&paths);
        assert_eq!(normalized, ["docs/literal", "docs\\literal "]);
        assert_ne!(normalized[0], normalized[1]);
        assert!(normalized.iter().any(|path| path.ends_with(' ')));
    }

    #[test]
    fn rename_source_path_can_force_policy_fallback() {
        let receipt = plan_selection(
            &input(&["schema/changed-surface.json", "docs/moved.json"]),
            Ok(policy()),
        )
        .expect("fallback");
        assert_eq!(
            receipt.fallback_reason,
            Some(FallbackReason::SelectorPolicyChanged)
        );
    }

    #[test]
    fn baseline_only_union_cannot_cover_the_authenticated_base_tree() {
        let mut bypass = policy();
        bypass.baseline_only_paths = vec![
            "src/**".to_owned(),
            "docs/**".to_owned(),
            ".shipyard/**".to_owned(),
        ];
        let receipt =
            plan_selection(&input(&["src/audio/processor.rs"]), Ok(bypass)).expect("full fallback");
        assert_eq!(receipt.planned_suite, PlannedSuite::Full);
        assert_eq!(receipt.fallback_reason, Some(FallbackReason::InvalidPolicy));
    }

    #[test]
    fn debug_excludes_release_only_family_and_requires_fresh_exact_head_release_proof() {
        let mut release_policy = policy();
        release_policy.families.push(TestFamily {
            name: "installed-sdk".to_owned(),
            paths: vec!["sdk/**".to_owned()],
            tests: vec!["agent capability installed SDK".to_owned()],
            build_targets: Vec::new(),
            risk_class: RiskClass::Low,
            extended_tests: Vec::new(),
            supported_build_types: vec![BuildType::Release],
            required_secondary_target: Some("release-installed-sdk".to_owned()),
            required_secondary_build_type: Some(BuildType::Release),
        });
        release_policy.secondary_contract_digests.insert(
            "release-installed-sdk".to_owned(),
            "release-contract".to_owned(),
        );
        let mut debug = input(&["sdk/agent-capability.cpp"]);
        debug
            .base_tracked_paths
            .push("sdk/agent-capability.cpp".to_owned());

        let blocked = plan_selection(&debug, Ok(release_policy.clone())).expect("blocked plan");
        assert_eq!(blocked.planned_suite, PlannedSuite::Blocked);
        assert!(
            !blocked
                .selected_tests
                .contains(&"agent capability installed SDK".to_owned())
        );
        assert!(blocked.secondary_proofs.is_empty());

        debug.secondary_proofs.push(SecondaryProof {
            target: "release-installed-sdk".to_owned(),
            build_type: BuildType::Release,
            head_sha: B.to_owned(),
            tree_sha: C.to_owned(),
            full_execution: true,
            passed: true,
            reused: false,
            completed_at: debug.observed_at,
            contract_digest: Some("release-contract".to_owned()),
        });
        let eligible = plan_selection(&debug, Ok(release_policy.clone())).expect("bounded plan");
        assert_eq!(eligible.planned_suite, PlannedSuite::Bounded);
        assert!(
            !eligible
                .selected_tests
                .contains(&"agent capability installed SDK".to_owned())
        );
        assert_eq!(eligible.secondary_proofs.len(), 1);
        assert_eq!(
            eligible.secondary_proofs[0].tests,
            ["agent capability installed SDK"]
        );

        let mut high_risk_policy = release_policy.clone();
        high_risk_policy.schema_version = 2;
        high_risk_policy.families[2].risk_class = RiskClass::High;
        let full = plan_selection(&debug, Ok(high_risk_policy.clone()))
            .expect("full plus secondary proof");
        assert_eq!(full.planned_suite, PlannedSuite::Full);
        assert_eq!(full.fallback_reason, Some(FallbackReason::HighRiskFamily));
        assert_eq!(full.secondary_proofs.len(), 1);

        let mut missing_high_risk_proof = debug.clone();
        missing_high_risk_proof.secondary_proofs.clear();
        let blocked = plan_selection(&missing_high_risk_proof, Ok(high_risk_policy))
            .expect("secondary proof remains mandatory");
        assert_eq!(blocked.planned_suite, PlannedSuite::Blocked);

        let mut full_surface_policy = release_policy.clone();
        full_surface_policy.schema_version = 2;
        full_surface_policy.full_required_paths = vec!["sdk/**".to_owned()];
        let blocked = plan_selection(&missing_high_risk_proof, Ok(full_surface_policy))
            .expect("full-required surface still needs secondary proof");
        assert_eq!(blocked.planned_suite, PlannedSuite::Blocked);

        let mut global_full_policy = release_policy.clone();
        global_full_policy.schema_version = 2;
        global_full_policy.full_required_paths = vec!["CMakeLists.txt".to_owned()];
        let global_input = input(&["CMakeLists.txt"]);
        let blocked = plan_selection(&global_input, Ok(global_full_policy.clone()))
            .expect("global full requires every incompatible family proof");
        assert_eq!(blocked.planned_suite, PlannedSuite::Blocked);
        let mut proven_global_input = global_input;
        proven_global_input.secondary_proofs = debug.secondary_proofs.clone();
        let full = plan_selection(&proven_global_input, Ok(global_full_policy))
            .expect("global full with secondary proof");
        assert_eq!(full.planned_suite, PlannedSuite::Full);
        assert_eq!(
            full.fallback_reason,
            Some(FallbackReason::FullRequiredSurface)
        );
        assert_eq!(full.secondary_proofs.len(), 1);

        debug.secondary_proofs[0].reused = true;
        let reused = plan_selection(&debug, Ok(release_policy.clone())).expect("blocked plan");
        assert_eq!(reused.planned_suite, PlannedSuite::Blocked);

        debug.secondary_proofs[0].reused = false;
        debug.secondary_proofs[0].completed_at =
            debug.observed_at - chrono::Duration::hours(SECONDARY_PROOF_MAX_AGE_HOURS + 1);
        let stale = plan_selection(&debug, Ok(release_policy.clone())).expect("blocked plan");
        assert_eq!(stale.planned_suite, PlannedSuite::Blocked);

        debug.secondary_proofs[0].completed_at = debug.observed_at;
        debug.secondary_proofs[0].build_type = BuildType::Debug;
        let wrong_build = plan_selection(&debug, Ok(release_policy)).expect("blocked plan");
        assert_eq!(wrong_build.planned_suite, PlannedSuite::Blocked);
    }

    #[test]
    fn base_config_authenticates_secondary_build_type_and_profile_requirement() {
        let config = r#"
            [project]
            profile = "mac-only"

            [profiles.mac-only]
            focus_platforms = ["macos"]

            [targets.debug]
            platform = "macos-arm64"
            validation_build_type = "debug"

            [targets.debug.changed_surface_selection]
            schema_version = 1
            full_test_count = 2
            build_type = "debug"
            baseline_tests = ["smoke"]
            test_topology_paths = ["tests/**"]

            [[targets.debug.changed_surface_selection.families]]
            name = "sdk"
            paths = ["sdk/**"]
            tests = ["installed SDK"]
            supported_build_types = ["release"]
            required_secondary_target = "release-sdk"
            required_secondary_build_type = "release"

            [targets.release-sdk]
            platform = "linux-x64"
            validation_build_type = "release"
        "#;
        assert!(policy_from_toml(config, "debug").is_err());

        let required = config.replace("platform = \"linux-x64\"", "platform = \"macos-arm64\"");
        assert!(policy_from_toml(&required, "debug").is_ok());

        let wrong_build = required.replace(
            "[targets.release-sdk]\n            platform = \"macos-arm64\"\n            validation_build_type = \"release\"",
            "[targets.release-sdk]\n            platform = \"macos-arm64\"\n            validation_build_type = \"debug\"",
        );
        assert!(policy_from_toml(&wrong_build, "debug").is_err());

        let cloud = required.replace(
            "[targets.release-sdk]\n            platform",
            "[targets.release-sdk]\n            backend = \"cloud\"\n            workflow = \"release.yml\"\n            platform",
        );
        assert!(policy_from_toml(&cloud, "debug").is_err());

        let prepared = required.replace(
            "[project]\n            profile = \"mac-only\"",
            "[project]\n            profile = \"mac-only\"\n\n            [validation.prepared_state]\n            enabled = true",
        );
        assert!(policy_from_toml(&prepared, "debug").is_err());
    }
}
