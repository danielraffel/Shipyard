//! Shadow-only invariants for build-once, sharded test proof.
//!
//! This module deliberately has no queue, transport, runner-dispatch, GitHub,
//! or merge-readiness integration. It defines the records those later layers
//! must preserve and a crash-durable, immutable local record store.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};

/// Current schema version for every parallel-proof record.
pub const PARALLEL_PROOF_SCHEMA_VERSION: u32 = 1;
/// Maximum number of tests accepted in one canonical inventory.
pub const MAX_TESTS: usize = 100_000;
/// Maximum number of shards accepted in one plan.
pub const MAX_SHARDS: usize = 4_096;
/// Maximum UTF-8 byte length of an identifier or capability.
pub const MAX_IDENTIFIER_BYTES: usize = 512;
/// Maximum number of authenticated capabilities on one worker assignment.
pub const MAX_CAPABILITIES: usize = 256;
/// Maximum immutable attempts accepted for one shard.
pub const MAX_ATTEMPTS_PER_SHARD: usize = 32;
/// Maximum assignment, disposition, or receipt records accepted by one aggregation.
pub const MAX_ATTEMPT_RECORDS: usize = 16_384;
/// Maximum encoded size of any worker report or durable record.
pub const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
const MAX_RELATIONS: usize = 1_000_000;
const MAX_PAYLOAD_BYTES: usize = MAX_RECORD_BYTES - 4_096;
const HMAC_BLOCK_BYTES: usize = 64;

/// A validated lowercase SHA-256 digest.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    /// Parse exactly 64 lowercase hexadecimal characters.
    pub fn parse(value: impl Into<String>) -> Result<Self, ParallelProofError> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(ParallelProofError::InvalidDigest)
        }
    }

    /// Hash bytes with SHA-256.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }

    /// Return the lowercase hexadecimal representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Fail-closed validation, authentication, aggregation, or storage error.
#[derive(Debug)]
pub enum ParallelProofError {
    /// A record uses an unsupported schema version.
    UnsupportedSchemaVersion(u32),
    /// A SHA-256 digest is malformed.
    InvalidDigest,
    /// A field is empty, malformed, or internally inconsistent.
    InvalidField(&'static str),
    /// A bounded collection, string, or record exceeded its limit.
    LimitExceeded {
        /// Name of the bounded input.
        field: &'static str,
        /// Maximum accepted size.
        max: usize,
        /// Observed size.
        found: usize,
    },
    /// Canonical ordering or uniqueness was violated.
    NonCanonical(&'static str),
    /// A test identifier is absent from the declared inventory.
    UnknownTest(String),
    /// The shard allocation is not exhaustive and disjoint.
    InvalidPartition(&'static str),
    /// `CTest` topology cannot be preserved by the proposed plan.
    TopologyViolation(String),
    /// A content binding does not match the authoritative record.
    BindingMismatch(&'static str),
    /// A controller authentication code is invalid.
    AuthenticationFailed,
    /// An assignment attempt or fencing sequence is stale or incomplete.
    InvalidAttemptSequence(String),
    /// Two immutable records claim the same logical identity with different bytes.
    ImmutableConflict(String),
    /// A durable record was not found.
    MissingRecord(String),
    /// A durable record failed its envelope or semantic integrity checks.
    CorruptRecord(String),
    /// A test-only crash point interrupted persistence.
    CrashInjected(&'static str),
    /// JSON encoding or decoding failed.
    Json(String),
    /// Filesystem persistence failed.
    Io(std::io::Error),
}

impl fmt::Display for ParallelProofError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "unsupported parallel-proof schema version {version}"
                )
            }
            Self::InvalidDigest => formatter.write_str("invalid SHA-256 digest"),
            Self::InvalidField(field) => write!(formatter, "invalid {field}"),
            Self::LimitExceeded { field, max, found } => {
                write!(formatter, "{field} exceeds limit {max}: found {found}")
            }
            Self::NonCanonical(field) => write!(formatter, "non-canonical {field}"),
            Self::UnknownTest(test) => write!(formatter, "unknown test {test}"),
            Self::InvalidPartition(reason) => {
                write!(formatter, "invalid shard partition: {reason}")
            }
            Self::TopologyViolation(reason) => write!(formatter, "unsafe test topology: {reason}"),
            Self::BindingMismatch(field) => write!(formatter, "binding mismatch for {field}"),
            Self::AuthenticationFailed => formatter.write_str("controller authentication failed"),
            Self::InvalidAttemptSequence(reason) => {
                write!(formatter, "invalid assignment attempt sequence: {reason}")
            }
            Self::ImmutableConflict(key) => {
                write!(formatter, "conflicting immutable record for {key}")
            }
            Self::MissingRecord(key) => write!(formatter, "missing durable record {key}"),
            Self::CorruptRecord(reason) => write!(formatter, "corrupt durable record: {reason}"),
            Self::CrashInjected(point) => write!(formatter, "injected crash at {point}"),
            Self::Json(error) => write!(formatter, "JSON error: {error}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
        }
    }
}

impl std::error::Error for ParallelProofError {}

impl From<std::io::Error> for ParallelProofError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ParallelProofError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

/// Scope at which a `CTest` resource lock must exclude overlapping work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLockScope {
    /// The lock excludes work only on the same physical host.
    Host,
    /// The lock excludes work across the entire proof fleet.
    Fleet,
}

/// One explicitly classified `CTest` resource-lock claim.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLock {
    /// `CTest` resource-lock name.
    pub name: String,
    /// Exclusion scope used by the controller and aggregator.
    pub scope: ResourceLockScope,
}

/// Canonical metadata for one `CTest` test.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestCase {
    /// Exact `CTest` test name.
    pub id: String,
    /// Tests named by `CTest` `DEPENDS`.
    pub dependencies: Vec<String>,
    /// Fixtures created by this test.
    pub fixture_setup: Vec<String>,
    /// Fixtures required by this test.
    pub fixture_required: Vec<String>,
    /// Fixtures cleaned up by this test.
    pub fixture_cleanup: Vec<String>,
    /// Whether `CTest` marks this test `RUN_SERIAL`.
    pub run_serial: bool,
    /// Resource locks, each with an explicit host or fleet scope.
    pub resource_locks: Vec<ResourceLock>,
    /// Capabilities a selected worker must advertise.
    pub required_capabilities: Vec<String>,
}

/// Canonical, bounded inventory from which a shard plan is derived.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestInventory {
    /// Schema version.
    pub schema_version: u32,
    /// Tests sorted by exact identifier.
    pub tests: Vec<TestCase>,
}

impl TestInventory {
    /// Canonicalize and validate a complete test inventory.
    pub fn new(mut tests: Vec<TestCase>) -> Result<Self, ParallelProofError> {
        ensure_count("tests", tests.len(), 1, MAX_TESTS)?;
        ensure_test_relation_bound(&tests)?;
        ensure_serialized_bound("inventory input bytes", &tests)?;
        for test in &mut tests {
            canonicalize_test(test)?;
        }
        tests.sort_by(|left, right| left.id.cmp(&right.id));
        reject_adjacent_duplicates(tests.iter().map(|test| test.id.as_str()), "test ids")?;
        let inventory = Self {
            schema_version: PARALLEL_PROOF_SCHEMA_VERSION,
            tests,
        };
        inventory.validate()?;
        Ok(inventory)
    }

    /// Validate schema, bounds, ordering, dependency graph, and fixture references.
    pub fn validate(&self) -> Result<(), ParallelProofError> {
        validate_version(self.schema_version)?;
        ensure_count("tests", self.tests.len(), 1, MAX_TESTS)?;
        ensure_test_relation_bound(&self.tests)?;
        ensure_serialized_bound("inventory bytes", self)?;
        if !strictly_sorted(self.tests.iter().map(|test| test.id.as_str())) {
            return Err(ParallelProofError::NonCanonical("test ids"));
        }
        let ids = self
            .tests
            .iter()
            .map(|test| test.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut fixture_setups: BTreeMap<&str, usize> = BTreeMap::new();
        let mut resource_scopes: BTreeMap<&str, ResourceLockScope> = BTreeMap::new();
        for test in &self.tests {
            validate_canonical_test(test)?;
            for dependency in &test.dependencies {
                if dependency == &test.id {
                    return Err(ParallelProofError::TopologyViolation(format!(
                        "test {} depends on itself",
                        test.id
                    )));
                }
                if !ids.contains(dependency.as_str()) {
                    return Err(ParallelProofError::UnknownTest(dependency.clone()));
                }
            }
            for fixture in &test.fixture_setup {
                *fixture_setups.entry(fixture).or_default() += 1;
            }
            for lock in &test.resource_locks {
                match resource_scopes.insert(lock.name.as_str(), lock.scope) {
                    Some(previous) if previous != lock.scope => {
                        return Err(ParallelProofError::TopologyViolation(format!(
                            "resource lock {} has inconsistent scope",
                            lock.name
                        )));
                    }
                    _ => {}
                }
            }
        }
        for test in &self.tests {
            for fixture in test
                .fixture_required
                .iter()
                .chain(test.fixture_cleanup.iter())
            {
                if !fixture_setups.contains_key(fixture.as_str()) {
                    return Err(ParallelProofError::TopologyViolation(format!(
                        "fixture {fixture} has no setup test"
                    )));
                }
            }
        }
        validate_dependency_acyclic(self)
    }

    /// Domain-separated digest of the canonical inventory.
    pub fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
        self.validate()?;
        canonical_digest("shipyard.parallel-proof.inventory.v1", self)
    }
}

/// Scheduling behavior required for one shard.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShardExecutionMode {
    /// May overlap other shards, subject to resource-lock exclusions.
    Parallel,
    /// Must not overlap any other shard in the proof.
    FleetExclusive,
}

/// One deterministic shard in an exhaustive test plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestShard {
    /// Zero-based contiguous shard identifier.
    pub id: u32,
    /// Execution mode derived from test topology.
    pub execution_mode: ShardExecutionMode,
    /// Exact test identifiers, sorted lexicographically.
    pub test_ids: Vec<String>,
}

/// Exhaustive, disjoint, topology-preserving shard plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShardPlan {
    /// Schema version.
    pub schema_version: u32,
    /// Digest of the exact canonical inventory.
    pub inventory_digest: Sha256Digest,
    /// Total number of tests assigned exactly once.
    pub total_tests: u32,
    /// Shards sorted by contiguous identifier.
    pub shards: Vec<TestShard>,
}

impl ShardPlan {
    /// Build a stable balanced plan while keeping dependency and fixture components together.
    pub fn deterministic_balanced(
        inventory: &TestInventory,
        shard_count: usize,
    ) -> Result<Self, ParallelProofError> {
        inventory.validate()?;
        ensure_count("shards", shard_count, 1, MAX_SHARDS)?;
        let components = topology_components(inventory)?;
        if shard_count > components.len() {
            return Err(ParallelProofError::InvalidPartition(
                "requested more non-empty shards than topology components",
            ));
        }

        let mut serial = Vec::new();
        let mut regular = Vec::new();
        for component in components {
            let contains_serial = component
                .iter()
                .any(|index| inventory.tests[*index].run_serial);
            if contains_serial {
                if component.len() != 1 {
                    return Err(ParallelProofError::TopologyViolation(format!(
                        "RUN_SERIAL test {} is coupled to other tests",
                        inventory.tests[component[0]].id
                    )));
                }
                serial.push(component);
            } else {
                regular.push(component);
            }
        }
        if serial.len() > shard_count || (!regular.is_empty() && serial.len() == shard_count) {
            return Err(ParallelProofError::InvalidPartition(
                "not enough shards to isolate RUN_SERIAL tests",
            ));
        }
        let regular_shards = shard_count - serial.len();
        if regular_shards > regular.len() {
            return Err(ParallelProofError::InvalidPartition(
                "not enough independent topology components for requested shards",
            ));
        }

        let mut assignments = serial
            .into_iter()
            .map(|component| {
                component
                    .into_iter()
                    .map(|index| inventory.tests[index].id.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut parallel_assignments = vec![Vec::new(); regular_shards];
        let mut loads = vec![0_usize; regular_shards];
        regular.sort_by(|left, right| {
            inventory.tests[left[0]]
                .id
                .cmp(&inventory.tests[right[0]].id)
        });
        for component in regular {
            let target = loads
                .iter()
                .enumerate()
                .min_by_key(|(index, load)| (**load, *index))
                .map(|(index, _)| index)
                .ok_or(ParallelProofError::InvalidPartition("no parallel shard"))?;
            loads[target] += component.len();
            parallel_assignments[target].extend(
                component
                    .into_iter()
                    .map(|index| inventory.tests[index].id.clone()),
            );
        }
        assignments.extend(parallel_assignments);
        Self::from_assignments(inventory, assignments)
    }

    /// Validate an explicit allocation and derive all shard modes.
    pub fn from_assignments(
        inventory: &TestInventory,
        assignments: Vec<Vec<String>>,
    ) -> Result<Self, ParallelProofError> {
        inventory.validate()?;
        ensure_count("shards", assignments.len(), 1, MAX_SHARDS)?;
        let membership_count = assignments.iter().try_fold(0_usize, |total, test_ids| {
            total
                .checked_add(test_ids.len())
                .ok_or(ParallelProofError::LimitExceeded {
                    field: "shard test memberships",
                    max: MAX_TESTS,
                    found: usize::MAX,
                })
        })?;
        ensure_count("shard test memberships", membership_count, 1, MAX_TESTS)?;
        if membership_count != inventory.tests.len() {
            return Err(ParallelProofError::InvalidPartition(
                "shard membership count does not match inventory",
            ));
        }
        for test_ids in &assignments {
            if test_ids.is_empty() {
                return Err(ParallelProofError::InvalidPartition("empty shard"));
            }
            for test_id in test_ids {
                validate_identifier("shard test id", test_id)?;
            }
        }
        ensure_serialized_bound("shard assignments bytes", &assignments)?;
        let lookup = inventory_lookup(inventory);
        let mut shards = Vec::with_capacity(assignments.len());
        for (index, mut test_ids) in assignments.into_iter().enumerate() {
            test_ids.sort();
            reject_adjacent_duplicates(test_ids.iter().map(String::as_str), "shard test ids")?;
            let has_serial = test_ids
                .iter()
                .any(|id| lookup.get(id.as_str()).is_some_and(|test| test.run_serial));
            let mode = if has_serial {
                ShardExecutionMode::FleetExclusive
            } else {
                ShardExecutionMode::Parallel
            };
            shards.push(TestShard {
                id: u32::try_from(index)
                    .map_err(|_| ParallelProofError::InvalidField("shard id"))?,
                execution_mode: mode,
                test_ids,
            });
        }
        let plan = Self {
            schema_version: PARALLEL_PROOF_SCHEMA_VERSION,
            inventory_digest: inventory.digest()?,
            total_tests: u32::try_from(inventory.tests.len()).map_err(|_| {
                ParallelProofError::LimitExceeded {
                    field: "tests",
                    max: u32::MAX as usize,
                    found: inventory.tests.len(),
                }
            })?,
            shards,
        };
        plan.validate_against(inventory)?;
        Ok(plan)
    }

    /// Validate binding, exhaustive/disjoint membership, and topology preservation.
    pub fn validate_against(&self, inventory: &TestInventory) -> Result<(), ParallelProofError> {
        validate_version(self.schema_version)?;
        inventory.validate()?;
        ensure_count("shards", self.shards.len(), 1, MAX_SHARDS)?;
        self.validate_input_bounds(inventory.tests.len())?;
        if self.inventory_digest != inventory.digest()? {
            return Err(ParallelProofError::BindingMismatch("inventory digest"));
        }
        if usize::try_from(self.total_tests).ok() != Some(inventory.tests.len()) {
            return Err(ParallelProofError::BindingMismatch("test count"));
        }
        let lookup = inventory_lookup(inventory);
        let mut membership = BTreeMap::new();
        for (index, shard) in self.shards.iter().enumerate() {
            if usize::try_from(shard.id).ok() != Some(index) {
                return Err(ParallelProofError::NonCanonical("shard ids"));
            }
            if shard.test_ids.is_empty()
                || !strictly_sorted(shard.test_ids.iter().map(String::as_str))
            {
                return Err(ParallelProofError::NonCanonical("shard test ids"));
            }
            let mut contains_serial = false;
            for test_id in &shard.test_ids {
                let Some(test) = lookup.get(test_id.as_str()) else {
                    return Err(ParallelProofError::UnknownTest(test_id.clone()));
                };
                if membership.insert(test_id.as_str(), shard.id).is_some() {
                    return Err(ParallelProofError::InvalidPartition(
                        "test appears in more than one shard",
                    ));
                }
                contains_serial |= test.run_serial;
            }
            let expected_mode = if contains_serial {
                ShardExecutionMode::FleetExclusive
            } else {
                ShardExecutionMode::Parallel
            };
            if shard.execution_mode != expected_mode {
                return Err(ParallelProofError::BindingMismatch("shard execution mode"));
            }
            if contains_serial && shard.test_ids.len() != 1 {
                return Err(ParallelProofError::TopologyViolation(format!(
                    "RUN_SERIAL shard {} contains multiple tests",
                    shard.id
                )));
            }
        }
        if membership.len() != inventory.tests.len() {
            return Err(ParallelProofError::InvalidPartition(
                "one or more tests are unassigned",
            ));
        }
        for test in &inventory.tests {
            let shard = membership[test.id.as_str()];
            for dependency in &test.dependencies {
                if membership[dependency.as_str()] != shard {
                    return Err(ParallelProofError::TopologyViolation(format!(
                        "dependency {} -> {} crosses shards",
                        test.id, dependency
                    )));
                }
            }
        }
        let mut fixture_shards: BTreeMap<&str, u32> = BTreeMap::new();
        for test in &inventory.tests {
            let shard = membership[test.id.as_str()];
            for fixture in test
                .fixture_setup
                .iter()
                .chain(test.fixture_required.iter())
                .chain(test.fixture_cleanup.iter())
            {
                match fixture_shards.insert(fixture, shard) {
                    Some(previous) if previous != shard => {
                        return Err(ParallelProofError::TopologyViolation(format!(
                            "fixture {fixture} crosses shards"
                        )));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn validate_input_bounds(&self, expected_memberships: usize) -> Result<(), ParallelProofError> {
        ensure_serialized_bound("shard plan bytes", self)?;
        let membership_count = self.shards.iter().try_fold(0_usize, |total, shard| {
            total
                .checked_add(shard.test_ids.len())
                .ok_or(ParallelProofError::LimitExceeded {
                    field: "shard test memberships",
                    max: MAX_TESTS,
                    found: usize::MAX,
                })
        })?;
        ensure_count("shard test memberships", membership_count, 1, MAX_TESTS)?;
        if membership_count != expected_memberships {
            return Err(ParallelProofError::InvalidPartition(
                "shard membership count does not match inventory",
            ));
        }
        for shard in &self.shards {
            for test_id in &shard.test_ids {
                validate_identifier("shard test id", test_id)?;
            }
        }
        Ok(())
    }

    /// Domain-separated digest of the validated plan.
    pub fn digest(&self, inventory: &TestInventory) -> Result<Sha256Digest, ParallelProofError> {
        self.validate_against(inventory)?;
        canonical_digest("shipyard.parallel-proof.plan.v1", self)
    }

    fn shard(&self, shard_id: u32) -> Option<&TestShard> {
        self.shards.get(usize::try_from(shard_id).ok()?)
    }
}

/// Kind of immutable GitHub subject being proved.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProofSubject {
    /// Exact pull-request head.
    PullRequest {
        /// GitHub pull-request number.
        number: u64,
    },
    /// Exact merge-group head.
    MergeGroup {
        /// GitHub merge-group identifier.
        id: String,
    },
}

/// Immutable repository, subject, commit, and tree identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentity {
    /// Numeric GitHub repository identity.
    pub repository_id: u64,
    /// Canonical lowercase `owner/name` slug.
    pub repository: String,
    /// Pull request or merge group being proved.
    pub subject: ProofSubject,
    /// Exact subject commit object ID.
    pub head_sha: String,
    /// Exact Git tree object ID.
    pub tree_sha: String,
}

/// Immutable full-build contract and toolchain identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildIdentity {
    /// Digest of the full build recipe, flags, environment, and external inputs.
    pub contract_sha256: Sha256Digest,
    /// Digest of the compiler, SDK, and toolchain closure.
    pub toolchain_sha256: Sha256Digest,
    /// Exact target triple.
    pub target_triple: String,
    /// Exact build profile name.
    pub profile: String,
}

/// Immutable build artifact identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIdentity {
    /// Exact Git tree object ID used to build the artifact.
    pub source_tree_sha: String,
    /// Digest of the full build contract used to build the artifact.
    pub build_contract_sha256: Sha256Digest,
    /// Digest of the complete artifact payload.
    pub payload_sha256: Sha256Digest,
    /// Digest of the canonical artifact-member inventory.
    pub layout_sha256: Sha256Digest,
    /// Exact encoded artifact byte length.
    pub size_bytes: u64,
}

/// Provenance class of the build artifact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactTrustClass {
    /// Produced by trusted controller-owned inputs.
    TrustedController,
    /// Contains contributor-controlled code or build outputs.
    UntrustedContributor,
}

/// Runtime boundary in which the artifact may be executed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionBoundary {
    /// Execution may occur on a trusted host under the declared policy.
    TrustedHost,
    /// Execution is confined to a disposable guest that is destroyed afterward.
    DisposableGuest,
}

/// Immutable producer and execution trust identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrustIdentity {
    /// Authenticated producer-host identity digest.
    pub producer_identity_sha256: Sha256Digest,
    /// Immutable VM or runner image digest.
    pub image_sha256: Sha256Digest,
    /// Digest of the sandbox, mount, network, and teardown policy.
    pub policy_sha256: Sha256Digest,
    /// Artifact provenance class.
    pub artifact_class: ArtifactTrustClass,
    /// Required execution boundary.
    pub execution_boundary: ExecutionBoundary,
    /// Whether artifact execution has network access.
    pub network_enabled: bool,
    /// Whether artifact execution has writable maintainer-host mounts.
    pub writable_host_mounts: bool,
}

/// Immutable identity of a build-once/shard-many shadow proof.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParallelProofManifest {
    /// Schema version.
    pub schema_version: u32,
    /// Must remain true in this schema; no v1 record has merge authority.
    pub shadow_only: bool,
    /// Repository, subject, commit, and tree identity.
    pub source: SourceIdentity,
    /// Full build and toolchain contract.
    pub build: BuildIdentity,
    /// Content-addressed artifact identity.
    pub artifact: ArtifactIdentity,
    /// Producer and artifact execution trust identity.
    pub trust: TrustIdentity,
    /// Exact canonical test inventory digest.
    pub inventory_digest: Sha256Digest,
    /// Exact exhaustive shard-plan digest.
    pub plan_digest: Sha256Digest,
}

impl ParallelProofManifest {
    /// Construct a validated, permanently shadow-only manifest.
    pub fn new(
        source: SourceIdentity,
        build: BuildIdentity,
        artifact: ArtifactIdentity,
        trust: TrustIdentity,
        inventory: &TestInventory,
        plan: &ShardPlan,
    ) -> Result<Self, ParallelProofError> {
        let manifest = Self {
            schema_version: PARALLEL_PROOF_SCHEMA_VERSION,
            shadow_only: true,
            source,
            build,
            artifact,
            trust,
            inventory_digest: inventory.digest()?,
            plan_digest: plan.digest(inventory)?,
        };
        manifest.validate(inventory, plan)?;
        Ok(manifest)
    }

    /// Validate all immutable source, build, artifact, trust, inventory, and plan bindings.
    pub fn validate(
        &self,
        inventory: &TestInventory,
        plan: &ShardPlan,
    ) -> Result<(), ParallelProofError> {
        validate_version(self.schema_version)?;
        if !self.shadow_only {
            return Err(ParallelProofError::InvalidField("shadow_only"));
        }
        validate_source(&self.source)?;
        validate_build(&self.build)?;
        validate_artifact(&self.artifact)?;
        validate_trust(&self.trust)?;
        if self.artifact.source_tree_sha != self.source.tree_sha {
            return Err(ParallelProofError::BindingMismatch("artifact source tree"));
        }
        if self.artifact.build_contract_sha256 != self.build.contract_sha256 {
            return Err(ParallelProofError::BindingMismatch(
                "artifact build contract",
            ));
        }
        if self.inventory_digest != inventory.digest()? {
            return Err(ParallelProofError::BindingMismatch("manifest inventory"));
        }
        if self.plan_digest != plan.digest(inventory)? {
            return Err(ParallelProofError::BindingMismatch("manifest plan"));
        }
        Ok(())
    }

    /// Domain-separated digest of the complete immutable manifest.
    pub fn digest(
        &self,
        inventory: &TestInventory,
        plan: &ShardPlan,
    ) -> Result<Sha256Digest, ParallelProofError> {
        self.validate(inventory, plan)?;
        canonical_digest("shipyard.parallel-proof.manifest.v1", self)
    }

    /// V1 shadow proof can never satisfy a protected merge-readiness check.
    #[must_use]
    pub const fn satisfies_merge_readiness(&self) -> bool {
        false
    }
}

/// Borrowed, validated manifest/inventory/plan tuple used by proof operations.
#[derive(Clone, Copy, Debug)]
pub struct ParallelProofContext<'a> {
    /// Immutable proof manifest.
    pub manifest: &'a ParallelProofManifest,
    /// Canonical test inventory bound by the manifest.
    pub inventory: &'a TestInventory,
    /// Exhaustive shard plan bound by the manifest.
    pub plan: &'a ShardPlan,
}

impl<'a> ParallelProofContext<'a> {
    /// Construct and validate one inseparable proof context.
    pub fn new(
        manifest: &'a ParallelProofManifest,
        inventory: &'a TestInventory,
        plan: &'a ShardPlan,
    ) -> Result<Self, ParallelProofError> {
        manifest.validate(inventory, plan)?;
        Ok(Self {
            manifest,
            inventory,
            plan,
        })
    }

    fn validate(self) -> Result<(), ParallelProofError> {
        self.manifest.validate(self.inventory, self.plan)
    }
}

struct ValidatedShardBinding<'a> {
    digest: Sha256Digest,
    required_capabilities: BTreeSet<&'a str>,
    execution_mode: ShardExecutionMode,
}

struct ValidatedProofBindings<'a> {
    manifest_digest: Sha256Digest,
    inventory_digest: Sha256Digest,
    plan_digest: Sha256Digest,
    shards: Vec<ValidatedShardBinding<'a>>,
}

impl<'a> ValidatedProofBindings<'a> {
    fn from_validated(proof: ParallelProofContext<'a>) -> Result<Self, ParallelProofError> {
        let shards = proof
            .plan
            .shards
            .iter()
            .map(|shard| {
                Ok(ValidatedShardBinding {
                    digest: canonical_digest("shipyard.parallel-proof.shard.v1", shard)?,
                    required_capabilities: required_capabilities(proof.inventory, shard),
                    execution_mode: shard.execution_mode,
                })
            })
            .collect::<Result<Vec<_>, ParallelProofError>>()?;
        Ok(Self {
            manifest_digest: canonical_digest(
                "shipyard.parallel-proof.manifest.v1",
                proof.manifest,
            )?,
            inventory_digest: canonical_digest(
                "shipyard.parallel-proof.inventory.v1",
                proof.inventory,
            )?,
            plan_digest: canonical_digest("shipyard.parallel-proof.plan.v1", proof.plan)?,
            shards,
        })
    }

    fn shard(&self, shard_id: u32) -> Option<&ValidatedShardBinding<'a>> {
        self.shards.get(usize::try_from(shard_id).ok()?)
    }
}

/// Authenticated worker session attributes supplied by a transport boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedWorker {
    /// Stable controller host identifier.
    pub host_id: String,
    /// Authenticated machine or guest identity digest.
    pub identity_sha256: Sha256Digest,
    /// Sorted, unique capability names authenticated for this session.
    pub capabilities: Vec<String>,
    /// Monotonic host-session generation used to fence reconnects.
    pub session_generation: u64,
}

impl AuthenticatedWorker {
    /// Validate the transport-authenticated identity and capability set.
    pub fn validate(&self) -> Result<(), ParallelProofError> {
        validate_identifier("host id", &self.host_id)?;
        ensure_count(
            "worker capabilities",
            self.capabilities.len(),
            0,
            MAX_CAPABILITIES,
        )?;
        validate_sorted_identifiers("worker capabilities", &self.capabilities)?;
        if self.session_generation == 0 {
            return Err(ParallelProofError::InvalidField("session generation"));
        }
        Ok(())
    }
}

/// Secret controller key used only to mint and verify assignments.
#[derive(Clone)]
pub struct ControllerKey {
    id: String,
    secret: Vec<u8>,
}

impl fmt::Debug for ControllerKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControllerKey")
            .field("id", &self.id)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

impl ControllerKey {
    /// Create a controller key from 32 through 64 bytes of secret material.
    pub fn new(id: impl Into<String>, secret: &[u8]) -> Result<Self, ParallelProofError> {
        let id = id.into();
        validate_identifier("controller key id", &id)?;
        if !(32..=64).contains(&secret.len()) {
            return Err(ParallelProofError::InvalidField("controller key length"));
        }
        Ok(Self {
            id,
            secret: secret.to_vec(),
        })
    }

    /// Stable public identifier for this controller key.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Authenticated and fenced controller assignment claims.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssignmentClaims {
    /// Schema version.
    pub schema_version: u32,
    /// Complete proof-manifest digest.
    pub manifest_digest: Sha256Digest,
    /// Exact inventory digest.
    pub inventory_digest: Sha256Digest,
    /// Exact shard-plan digest.
    pub plan_digest: Sha256Digest,
    /// Exact artifact payload digest.
    pub artifact_sha256: Sha256Digest,
    /// Shard identifier.
    pub shard_id: u32,
    /// Digest of the exact shard description.
    pub shard_digest: Sha256Digest,
    /// Immutable attempt number, contiguous from one.
    pub attempt: u32,
    /// Monotonically increasing controller fencing token.
    pub fence: u64,
    /// Assigned host identifier.
    pub host_id: String,
    /// Authenticated worker identity digest.
    pub worker_identity_sha256: Sha256Digest,
    /// Sorted capability set authenticated for the assigned session.
    pub capabilities: Vec<String>,
    /// Exact authenticated session generation.
    pub session_generation: u64,
    /// Topology-derived execution mode.
    pub execution_mode: ShardExecutionMode,
    /// Required execution boundary for the artifact.
    pub execution_boundary: ExecutionBoundary,
}

/// Controller-minted assignment with an HMAC-SHA256 authenticator.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerAssignment {
    /// Immutable assignment claims.
    pub claims: AssignmentClaims,
    /// Public controller-key identifier.
    pub controller_key_id: String,
    /// HMAC-SHA256 over the canonical assignment claims.
    pub authentication: Sha256Digest,
}

impl ControllerAssignment {
    /// Mint one assignment bound to a proof, shard, attempt, worker, and fence.
    pub fn mint(
        key: &ControllerKey,
        proof: ParallelProofContext<'_>,
        shard_id: u32,
        attempt: u32,
        fence: u64,
        worker: &AuthenticatedWorker,
    ) -> Result<Self, ParallelProofError> {
        proof.validate()?;
        worker.validate()?;
        let shard = proof
            .plan
            .shard(shard_id)
            .ok_or(ParallelProofError::InvalidField("shard id"))?;
        if attempt == 0 || fence == 0 {
            return Err(ParallelProofError::InvalidField("attempt or fence"));
        }
        let required = required_capabilities(proof.inventory, shard);
        let available = worker.capabilities.iter().map(String::as_str).collect();
        if !required.is_subset(&available) {
            return Err(ParallelProofError::BindingMismatch("worker capabilities"));
        }
        let claims = AssignmentClaims {
            schema_version: PARALLEL_PROOF_SCHEMA_VERSION,
            manifest_digest: proof.manifest.digest(proof.inventory, proof.plan)?,
            inventory_digest: proof.inventory.digest()?,
            plan_digest: proof.plan.digest(proof.inventory)?,
            artifact_sha256: proof.manifest.artifact.payload_sha256.clone(),
            shard_id,
            shard_digest: canonical_digest("shipyard.parallel-proof.shard.v1", shard)?,
            attempt,
            fence,
            host_id: worker.host_id.clone(),
            worker_identity_sha256: worker.identity_sha256.clone(),
            capabilities: worker.capabilities.clone(),
            session_generation: worker.session_generation,
            execution_mode: shard.execution_mode,
            execution_boundary: proof.manifest.trust.execution_boundary,
        };
        let authentication = assignment_authentication(key, &claims)?;
        let assignment = Self {
            claims,
            controller_key_id: key.id.clone(),
            authentication,
        };
        assignment.verify(key)?;
        Ok(assignment)
    }

    /// Verify the assignment authenticator and structural claims.
    pub fn verify(&self, key: &ControllerKey) -> Result<(), ParallelProofError> {
        validate_version(self.claims.schema_version)?;
        if self.controller_key_id != key.id {
            return Err(ParallelProofError::AuthenticationFailed);
        }
        validate_identifier("host id", &self.claims.host_id)?;
        ensure_count(
            "assignment capabilities",
            self.claims.capabilities.len(),
            0,
            MAX_CAPABILITIES,
        )?;
        validate_sorted_identifiers("assignment capabilities", &self.claims.capabilities)?;
        if self.claims.attempt == 0 || self.claims.fence == 0 || self.claims.session_generation == 0
        {
            return Err(ParallelProofError::InvalidField("assignment counters"));
        }
        ensure_count(
            "assignment attempt",
            self.claims.attempt as usize,
            1,
            MAX_ATTEMPTS_PER_SHARD,
        )?;
        ensure_serialized_bound("assignment bytes", self)?;
        let expected = assignment_authentication(key, &self.claims)?;
        if !constant_time_eq(
            expected.as_str().as_bytes(),
            self.authentication.as_str().as_bytes(),
        ) {
            return Err(ParallelProofError::AuthenticationFailed);
        }
        Ok(())
    }

    /// Domain-separated digest of the authenticated assignment.
    pub fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
        canonical_digest("shipyard.parallel-proof.assignment.v1", self)
    }

    /// Stable logical identity used by the immutable store.
    #[must_use]
    pub fn logical_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.claims.manifest_digest.as_str(),
            self.claims.shard_id,
            self.claims.attempt
        )
    }
}

/// Terminal result of one declared test.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcomeStatus {
    /// Test completed successfully.
    Passed,
    /// Test completed unsuccessfully.
    Failed,
    /// Test did not execute to completion.
    NotRun,
}

/// Bounded outcome for one exact test identifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestOutcome {
    /// Exact canonical test identifier.
    pub test_id: String,
    /// Terminal test status.
    pub status: TestOutcomeStatus,
    /// Bounded runtime reported by the worker.
    pub duration_ms: u64,
}

/// Untrusted worker input accepted only through an authenticated assignment/session boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerReport {
    /// Schema version.
    pub schema_version: u32,
    /// Digest of the exact controller assignment being answered.
    pub assignment_digest: Sha256Digest,
    /// Artifact payload digest observed before execution.
    pub observed_artifact_sha256: Sha256Digest,
    /// Build contract digest observed by the worker.
    pub observed_build_contract_sha256: Sha256Digest,
    /// Subject commit observed by the worker.
    pub observed_head_sha: String,
    /// Source tree observed by the worker.
    pub observed_tree_sha: String,
    /// Outcomes sorted by test identifier and covering the shard exactly once.
    pub outcomes: Vec<TestOutcome>,
    /// Digest of the complete captured log.
    pub log_sha256: Sha256Digest,
}

/// Controller-owned observation of execution, separate from untrusted worker JSON.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControllerExecutionObservation {
    /// Inclusive execution start in controller-comparable milliseconds.
    pub started_at_ms: u64,
    /// Exclusive execution end in controller-comparable milliseconds.
    pub completed_at_ms: u64,
    /// Whether the controller-side harness verified the full artifact.
    pub artifact_verified: bool,
    /// Execution boundary observed by the controller-side harness.
    pub execution_boundary: ExecutionBoundary,
    /// Whether the controller observed successful disposable-guest teardown.
    pub guest_teardown_confirmed: bool,
}

/// Controller-owned terminal disposition for an issued assignment attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttemptDispositionKind {
    /// The assignment was durably fenced before execution began.
    FencedBeforeStart,
    /// The assignment executed during a controller-observed interval.
    Executed {
        /// Inclusive controller-observed execution start.
        started_at_ms: u64,
        /// Exclusive controller-observed execution end.
        completed_at_ms: u64,
        /// Execution boundary observed by the controller-side harness.
        execution_boundary: ExecutionBoundary,
        /// Whether disposable-guest teardown completed.
        guest_teardown_confirmed: bool,
    },
}

/// Authenticated terminal disposition for one exact assignment attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptDisposition {
    /// Schema version.
    pub schema_version: u32,
    /// Complete proof-manifest digest.
    pub manifest_digest: Sha256Digest,
    /// Exact authenticated assignment digest.
    pub assignment_digest: Sha256Digest,
    /// Shard identifier.
    pub shard_id: u32,
    /// Immutable attempt number.
    pub attempt: u32,
    /// Controller fencing token.
    pub fence: u64,
    /// Terminal attempt disposition.
    pub kind: AttemptDispositionKind,
    /// Public controller-key identifier.
    pub controller_key_id: String,
    /// HMAC-SHA256 proving controller ownership of the disposition.
    pub authentication: Sha256Digest,
}

impl AttemptDisposition {
    /// Close an assignment as durably fenced before it started.
    pub fn fenced_before_start(
        key: &ControllerKey,
        assignment: &ControllerAssignment,
    ) -> Result<Self, ParallelProofError> {
        assignment.verify(key)?;
        Self::mint(key, assignment, AttemptDispositionKind::FencedBeforeStart)
    }

    /// Close an assignment with a controller-owned execution observation.
    pub fn executed(
        key: &ControllerKey,
        assignment: &ControllerAssignment,
        observation: &ControllerExecutionObservation,
    ) -> Result<Self, ParallelProofError> {
        assignment.verify(key)?;
        validate_controller_observation(assignment, observation)?;
        if observation.execution_boundary == ExecutionBoundary::DisposableGuest
            && !observation.guest_teardown_confirmed
        {
            return Err(ParallelProofError::InvalidField("guest teardown"));
        }
        Self::mint(
            key,
            assignment,
            AttemptDispositionKind::Executed {
                started_at_ms: observation.started_at_ms,
                completed_at_ms: observation.completed_at_ms,
                execution_boundary: observation.execution_boundary,
                guest_teardown_confirmed: observation.guest_teardown_confirmed,
            },
        )
    }

    fn mint(
        key: &ControllerKey,
        assignment: &ControllerAssignment,
        kind: AttemptDispositionKind,
    ) -> Result<Self, ParallelProofError> {
        let mut disposition = Self {
            schema_version: PARALLEL_PROOF_SCHEMA_VERSION,
            manifest_digest: assignment.claims.manifest_digest.clone(),
            assignment_digest: assignment.digest()?,
            shard_id: assignment.claims.shard_id,
            attempt: assignment.claims.attempt,
            fence: assignment.claims.fence,
            kind,
            controller_key_id: key.id.clone(),
            authentication: digest_placeholder(),
        };
        disposition.authentication = disposition_authentication(key, &disposition)?;
        disposition.verify(key, assignment)?;
        Ok(disposition)
    }

    /// Verify controller authentication and exact assignment binding.
    pub fn verify(
        &self,
        key: &ControllerKey,
        assignment: &ControllerAssignment,
    ) -> Result<(), ParallelProofError> {
        validate_version(self.schema_version)?;
        assignment.verify(key)?;
        if self.controller_key_id != key.id
            || self.manifest_digest != assignment.claims.manifest_digest
            || self.assignment_digest != assignment.digest()?
            || self.shard_id != assignment.claims.shard_id
            || self.attempt != assignment.claims.attempt
            || self.fence != assignment.claims.fence
        {
            return Err(ParallelProofError::BindingMismatch("attempt disposition"));
        }
        match self.kind {
            AttemptDispositionKind::FencedBeforeStart => {}
            AttemptDispositionKind::Executed {
                started_at_ms,
                completed_at_ms,
                execution_boundary,
                guest_teardown_confirmed,
            } => {
                if started_at_ms >= completed_at_ms
                    || execution_boundary != assignment.claims.execution_boundary
                    || (execution_boundary == ExecutionBoundary::DisposableGuest
                        && !guest_teardown_confirmed)
                {
                    return Err(ParallelProofError::InvalidField(
                        "attempt execution disposition",
                    ));
                }
            }
        }
        ensure_serialized_bound("attempt disposition bytes", self)?;
        let expected = disposition_authentication(key, self)?;
        if !constant_time_eq(
            expected.as_str().as_bytes(),
            self.authentication.as_str().as_bytes(),
        ) {
            return Err(ParallelProofError::AuthenticationFailed);
        }
        Ok(())
    }

    /// Domain-separated digest of this authenticated disposition.
    pub fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
        canonical_digest("shipyard.parallel-proof.attempt-disposition.v1", self)
    }

    /// Stable logical identity used by the immutable store.
    #[must_use]
    pub fn logical_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.manifest_digest.as_str(),
            self.shard_id,
            self.attempt
        )
    }
}

/// Accepted terminal state of one shard attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShardReceiptStatus {
    /// Every declared test passed and artifact/boundary checks held.
    Passed,
    /// One or more declared checks failed or did not run.
    Failed,
}

/// Controller-accepted immutable receipt for one authenticated assignment attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShardReceipt {
    /// Schema version.
    pub schema_version: u32,
    /// Complete proof-manifest digest.
    pub manifest_digest: Sha256Digest,
    /// Exact inventory digest.
    pub inventory_digest: Sha256Digest,
    /// Exact shard-plan digest.
    pub plan_digest: Sha256Digest,
    /// Digest of the exact authenticated assignment.
    pub assignment_digest: Sha256Digest,
    /// Digest of the controller-owned executed-attempt disposition.
    pub disposition_digest: Sha256Digest,
    /// Exact artifact payload digest.
    pub artifact_sha256: Sha256Digest,
    /// Shard identifier.
    pub shard_id: u32,
    /// Immutable attempt number.
    pub attempt: u32,
    /// Controller fencing token.
    pub fence: u64,
    /// Authenticated host identifier.
    pub host_id: String,
    /// Authenticated worker identity digest.
    pub worker_identity_sha256: Sha256Digest,
    /// Exact authenticated host-session generation.
    pub session_generation: u64,
    /// Topology-derived execution mode.
    pub execution_mode: ShardExecutionMode,
    /// Required and observed execution boundary.
    pub execution_boundary: ExecutionBoundary,
    /// Canonically sorted outcomes covering the shard exactly once.
    pub outcomes: Vec<TestOutcome>,
    /// Digest of the canonical outcome vector.
    pub outcomes_digest: Sha256Digest,
    /// Digest of the complete captured log.
    pub log_sha256: Sha256Digest,
    /// Inclusive execution start in controller-comparable milliseconds.
    pub started_at_ms: u64,
    /// Exclusive execution end in controller-comparable milliseconds.
    pub completed_at_ms: u64,
    /// Terminal shard status derived from all checks.
    pub status: ShardReceiptStatus,
    /// Whether the worker verified the complete artifact before execution.
    pub artifact_verified: bool,
    /// Whether a disposable guest was confirmed destroyed after execution.
    pub guest_teardown_confirmed: bool,
    /// Public controller-key identifier used to accept this receipt.
    pub controller_key_id: String,
    /// HMAC-SHA256 proving this receipt passed the controller acceptance boundary.
    pub acceptance_authentication: Sha256Digest,
}

impl ShardReceipt {
    /// Domain-separated digest of this immutable receipt.
    pub fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
        self.validate_structural()?;
        canonical_digest("shipyard.parallel-proof.receipt.v1", self)
    }

    /// Verify the controller acceptance authenticator and structural invariants.
    pub fn verify(&self, key: &ControllerKey) -> Result<(), ParallelProofError> {
        self.validate_structural()?;
        if self.controller_key_id != key.id {
            return Err(ParallelProofError::AuthenticationFailed);
        }
        let expected = receipt_authentication(key, self)?;
        if !constant_time_eq(
            expected.as_str().as_bytes(),
            self.acceptance_authentication.as_str().as_bytes(),
        ) {
            return Err(ParallelProofError::AuthenticationFailed);
        }
        Ok(())
    }

    /// Stable logical identity used by the immutable store.
    #[must_use]
    pub fn logical_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.manifest_digest.as_str(),
            self.shard_id,
            self.attempt
        )
    }

    fn validate_structural(&self) -> Result<(), ParallelProofError> {
        validate_version(self.schema_version)?;
        validate_identifier("host id", &self.host_id)?;
        if self.attempt == 0 || self.fence == 0 || self.session_generation == 0 {
            return Err(ParallelProofError::InvalidField("receipt counters"));
        }
        if self.started_at_ms >= self.completed_at_ms {
            return Err(ParallelProofError::InvalidField("receipt interval"));
        }
        ensure_count("receipt outcomes", self.outcomes.len(), 1, MAX_TESTS)?;
        if !strictly_sorted(self.outcomes.iter().map(|outcome| outcome.test_id.as_str())) {
            return Err(ParallelProofError::NonCanonical("receipt outcomes"));
        }
        for outcome in &self.outcomes {
            validate_identifier("outcome test id", &outcome.test_id)?;
        }
        if self.outcomes_digest
            != canonical_digest("shipyard.parallel-proof.outcomes.v1", &self.outcomes)?
        {
            return Err(ParallelProofError::BindingMismatch("outcomes digest"));
        }
        let expected_status = if self.artifact_verified
            && self
                .outcomes
                .iter()
                .all(|outcome| outcome.status == TestOutcomeStatus::Passed)
            && (self.execution_boundary != ExecutionBoundary::DisposableGuest
                || self.guest_teardown_confirmed)
        {
            ShardReceiptStatus::Passed
        } else {
            ShardReceiptStatus::Failed
        };
        if self.status != expected_status {
            return Err(ParallelProofError::BindingMismatch("receipt status"));
        }
        ensure_serialized_bound("receipt bytes", self)
    }
}

/// Decode one strictly bounded worker report with unknown fields rejected.
pub fn decode_worker_report(bytes: &[u8]) -> Result<WorkerReport, ParallelProofError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(ParallelProofError::LimitExceeded {
            field: "worker report bytes",
            max: MAX_RECORD_BYTES,
            found: bytes.len(),
        });
    }
    Ok(serde_json::from_slice(bytes)?)
}

/// Validate untrusted worker input and issue a controller-accepted receipt.
pub fn accept_worker_report(
    key: &ControllerKey,
    proof: ParallelProofContext<'_>,
    assignment: &ControllerAssignment,
    worker: &AuthenticatedWorker,
    observation: &ControllerExecutionObservation,
    report: WorkerReport,
) -> Result<ShardReceipt, ParallelProofError> {
    proof.validate()?;
    assignment.verify(key)?;
    worker.validate()?;
    validate_assignment_binding(proof.manifest, proof.inventory, proof.plan, assignment)?;
    let claims = &assignment.claims;
    if worker.host_id != claims.host_id
        || worker.identity_sha256 != claims.worker_identity_sha256
        || worker.capabilities != claims.capabilities
        || worker.session_generation != claims.session_generation
    {
        return Err(ParallelProofError::AuthenticationFailed);
    }
    let shard = proof
        .plan
        .shard(claims.shard_id)
        .ok_or(ParallelProofError::InvalidField("shard id"))?;
    validate_worker_report_binding(proof.manifest, assignment, &report)?;
    validate_controller_observation(assignment, observation)?;
    validate_report_outcomes(shard, &report.outcomes)?;
    let disposition = AttemptDisposition::executed(key, assignment, observation)?;
    let boundary_passed = observation_boundary_passed(proof.manifest, observation);
    let status = if observation.artifact_verified
        && boundary_passed
        && report
            .outcomes
            .iter()
            .all(|outcome| outcome.status == TestOutcomeStatus::Passed)
    {
        ShardReceiptStatus::Passed
    } else {
        ShardReceiptStatus::Failed
    };
    let mut receipt = ShardReceipt {
        schema_version: PARALLEL_PROOF_SCHEMA_VERSION,
        manifest_digest: claims.manifest_digest.clone(),
        inventory_digest: claims.inventory_digest.clone(),
        plan_digest: claims.plan_digest.clone(),
        assignment_digest: assignment.digest()?,
        disposition_digest: disposition.digest()?,
        artifact_sha256: claims.artifact_sha256.clone(),
        shard_id: claims.shard_id,
        attempt: claims.attempt,
        fence: claims.fence,
        host_id: claims.host_id.clone(),
        worker_identity_sha256: claims.worker_identity_sha256.clone(),
        session_generation: claims.session_generation,
        execution_mode: claims.execution_mode,
        execution_boundary: observation.execution_boundary,
        outcomes_digest: canonical_digest("shipyard.parallel-proof.outcomes.v1", &report.outcomes)?,
        outcomes: report.outcomes,
        log_sha256: report.log_sha256,
        started_at_ms: observation.started_at_ms,
        completed_at_ms: observation.completed_at_ms,
        status,
        artifact_verified: observation.artifact_verified,
        guest_teardown_confirmed: observation.guest_teardown_confirmed,
        controller_key_id: key.id.clone(),
        acceptance_authentication: digest_placeholder(),
    };
    receipt.acceptance_authentication = receipt_authentication(key, &receipt)?;
    receipt.validate_structural()?;
    Ok(receipt)
}

fn validate_worker_report_binding(
    manifest: &ParallelProofManifest,
    assignment: &ControllerAssignment,
    report: &WorkerReport,
) -> Result<(), ParallelProofError> {
    validate_version(report.schema_version)?;
    ensure_serialized_bound("worker report bytes", report)?;
    if report.assignment_digest != assignment.digest()? {
        return Err(ParallelProofError::BindingMismatch("assignment digest"));
    }
    if report.observed_artifact_sha256 != manifest.artifact.payload_sha256 {
        return Err(ParallelProofError::BindingMismatch("observed artifact"));
    }
    if report.observed_build_contract_sha256 != manifest.build.contract_sha256 {
        return Err(ParallelProofError::BindingMismatch(
            "observed build contract",
        ));
    }
    if report.observed_head_sha != manifest.source.head_sha {
        return Err(ParallelProofError::BindingMismatch("observed head"));
    }
    if report.observed_tree_sha != manifest.source.tree_sha {
        return Err(ParallelProofError::BindingMismatch("observed tree"));
    }
    Ok(())
}

fn validate_controller_observation(
    assignment: &ControllerAssignment,
    observation: &ControllerExecutionObservation,
) -> Result<(), ParallelProofError> {
    if observation.execution_boundary != assignment.claims.execution_boundary {
        return Err(ParallelProofError::BindingMismatch("execution boundary"));
    }
    if observation.started_at_ms >= observation.completed_at_ms {
        return Err(ParallelProofError::InvalidField("controller interval"));
    }
    Ok(())
}

fn validate_report_outcomes(
    shard: &TestShard,
    outcomes: &[TestOutcome],
) -> Result<(), ParallelProofError> {
    ensure_count("worker outcomes", outcomes.len(), 1, MAX_TESTS)?;
    if !strictly_sorted(outcomes.iter().map(|outcome| outcome.test_id.as_str())) {
        return Err(ParallelProofError::NonCanonical("worker outcomes"));
    }
    for outcome in outcomes {
        validate_identifier("outcome test id", &outcome.test_id)?;
    }
    if outcomes
        .iter()
        .map(|outcome| outcome.test_id.as_str())
        .ne(shard.test_ids.iter().map(String::as_str))
    {
        return Err(ParallelProofError::BindingMismatch("executed test set"));
    }
    Ok(())
}

fn observation_boundary_passed(
    manifest: &ParallelProofManifest,
    observation: &ControllerExecutionObservation,
) -> bool {
    match manifest.trust.artifact_class {
        ArtifactTrustClass::TrustedController => {
            observation.execution_boundary == manifest.trust.execution_boundary
        }
        ArtifactTrustClass::UntrustedContributor => {
            observation.execution_boundary == ExecutionBoundary::DisposableGuest
                && observation.guest_teardown_confirmed
        }
    }
}

/// Deterministic shadow-proof terminal state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ShadowProofStatus {
    /// All active attempts produced complete passing receipts.
    Passed,
    /// At least one active assignment has no receipt.
    Incomplete {
        /// Sorted identifiers of missing shards.
        missing_shards: Vec<u32>,
    },
    /// At least one complete active receipt failed.
    Failed {
        /// Sorted identifiers of failed shards.
        failed_shards: Vec<u32>,
    },
    /// At least one shard is incomplete and at least one complete shard failed.
    IncompleteAndFailed {
        /// Sorted identifiers of missing shards.
        missing_shards: Vec<u32>,
        /// Sorted identifiers of failed shards.
        failed_shards: Vec<u32>,
    },
}

/// Deterministic aggregation over the active attempt for every declared shard.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowAggregate {
    /// Schema version.
    pub schema_version: u32,
    /// Complete proof-manifest digest.
    pub manifest_digest: Sha256Digest,
    /// Exact inventory digest.
    pub inventory_digest: Sha256Digest,
    /// Exact plan digest.
    pub plan_digest: Sha256Digest,
    /// Active assignment digests sorted by shard identifier.
    pub active_assignment_digests: Vec<Sha256Digest>,
    /// All terminal disposition digests sorted by shard and attempt.
    pub attempt_disposition_digests: Vec<Sha256Digest>,
    /// Active receipt digests sorted by shard identifier; missing shards are absent.
    pub receipt_digests: Vec<Sha256Digest>,
    /// Deterministic shadow status.
    pub status: ShadowProofStatus,
}

impl ShadowAggregate {
    /// Domain-separated digest of the deterministic aggregate.
    pub fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
        canonical_digest("shipyard.parallel-proof.aggregate.v1", self)
    }

    /// V1 aggregate can never satisfy a protected merge-readiness check.
    #[must_use]
    pub const fn satisfies_merge_readiness(&self) -> bool {
        false
    }
}

/// Aggregate only the highest contiguous authenticated attempt for each declared shard.
pub fn aggregate_shadow_proof(
    key: &ControllerKey,
    proof: ParallelProofContext<'_>,
    assignments: &[ControllerAssignment],
    dispositions: &[AttemptDisposition],
    receipts: &[ShardReceipt],
) -> Result<ShadowAggregate, ParallelProofError> {
    proof.validate()?;
    validate_aggregate_input_bounds(proof.plan.shards.len(), assignments, dispositions, receipts)?;
    let bindings = ValidatedProofBindings::from_validated(proof)?;
    let active = validate_attempt_chains(key, proof.manifest, &bindings, assignments)?;
    if active.len() != proof.plan.shards.len() {
        return Err(ParallelProofError::InvalidAttemptSequence(
            "every declared shard must have an assignment".to_owned(),
        ));
    }

    let assignment_by_attempt = assignments
        .iter()
        .map(|assignment| {
            (
                (assignment.claims.shard_id, assignment.claims.attempt),
                assignment,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let dispositions_by_attempt = index_dispositions(key, &assignment_by_attempt, dispositions)?;
    let receipts_by_attempt = index_receipts(
        key,
        proof,
        &assignment_by_attempt,
        &dispositions_by_attempt,
        receipts,
    )?;
    let observed_executions = dispositions_by_attempt
        .iter()
        .filter_map(|(attempt_key, disposition)| {
            let assignment = assignment_by_attempt[attempt_key];
            observed_execution(assignment, disposition)
        })
        .collect::<Vec<_>>();
    validate_execution_overlap(proof.inventory, proof.plan, &observed_executions)?;

    let mut active_assignments = Vec::with_capacity(proof.plan.shards.len());
    let mut active_receipts = Vec::new();
    let mut missing = assignments
        .iter()
        .filter(|assignment| {
            !dispositions_by_attempt
                .contains_key(&(assignment.claims.shard_id, assignment.claims.attempt))
        })
        .map(|assignment| assignment.claims.shard_id)
        .collect::<Vec<_>>();
    let mut failed = Vec::new();
    for shard in &proof.plan.shards {
        let assignment = active
            .get(&shard.id)
            .ok_or_else(|| ParallelProofError::MissingRecord(format!("shard {}", shard.id)))?;
        active_assignments.push(assignment.digest()?);
        let attempt_key = (shard.id, assignment.claims.attempt);
        match (
            dispositions_by_attempt.get(&attempt_key),
            receipts_by_attempt.get(&attempt_key),
        ) {
            (Some(_), Some(receipt)) => {
                active_receipts.push((*receipt).to_owned());
                if receipt.status != ShardReceiptStatus::Passed {
                    failed.push(shard.id);
                }
            }
            _ => missing.push(shard.id),
        }
    }
    missing.sort_unstable();
    missing.dedup();
    let receipt_digests = active_receipts
        .iter()
        .map(ShardReceipt::digest)
        .collect::<Result<Vec<_>, _>>()?;
    let attempt_disposition_digests = dispositions_by_attempt
        .values()
        .map(|disposition| disposition.digest())
        .collect::<Result<Vec<_>, _>>()?;
    let status = if !missing.is_empty() && !failed.is_empty() {
        ShadowProofStatus::IncompleteAndFailed {
            missing_shards: missing,
            failed_shards: failed,
        }
    } else if !missing.is_empty() {
        ShadowProofStatus::Incomplete {
            missing_shards: missing,
        }
    } else if !failed.is_empty() {
        ShadowProofStatus::Failed {
            failed_shards: failed,
        }
    } else {
        ShadowProofStatus::Passed
    };
    Ok(ShadowAggregate {
        schema_version: PARALLEL_PROOF_SCHEMA_VERSION,
        manifest_digest: bindings.manifest_digest,
        inventory_digest: bindings.inventory_digest,
        plan_digest: bindings.plan_digest,
        active_assignment_digests: active_assignments,
        attempt_disposition_digests,
        receipt_digests,
        status,
    })
}

fn validate_aggregate_input_bounds(
    shard_count: usize,
    assignments: &[ControllerAssignment],
    dispositions: &[AttemptDisposition],
    receipts: &[ShardReceipt],
) -> Result<(), ParallelProofError> {
    ensure_count(
        "assignments",
        assignments.len(),
        shard_count,
        MAX_ATTEMPT_RECORDS,
    )?;
    let aggregate_record_count = assignments
        .len()
        .checked_add(dispositions.len())
        .and_then(|count| count.checked_add(receipts.len()))
        .unwrap_or(usize::MAX);
    ensure_count(
        "aggregate attempt records",
        aggregate_record_count,
        assignments.len(),
        MAX_ATTEMPT_RECORDS,
    )?;
    ensure_serialized_bound(
        "aggregate input corpus bytes",
        &(assignments, dispositions, receipts),
    )
}

fn index_dispositions<'a>(
    key: &ControllerKey,
    assignments: &BTreeMap<(u32, u32), &ControllerAssignment>,
    dispositions: &'a [AttemptDisposition],
) -> Result<BTreeMap<(u32, u32), &'a AttemptDisposition>, ParallelProofError> {
    let mut indexed = BTreeMap::new();
    for disposition in dispositions {
        let attempt_key = (disposition.shard_id, disposition.attempt);
        let Some(assignment) = assignments.get(&attempt_key) else {
            return Err(ParallelProofError::ImmutableConflict(format!(
                "unauthorized disposition {}:{}",
                disposition.shard_id, disposition.attempt
            )));
        };
        disposition.verify(key, assignment)?;
        match indexed.insert(attempt_key, disposition) {
            Some(previous) if previous != disposition => {
                return Err(ParallelProofError::ImmutableConflict(format!(
                    "attempt disposition {}:{}",
                    disposition.shard_id, disposition.attempt
                )));
            }
            _ => {}
        }
    }
    Ok(indexed)
}

fn index_receipts<'a>(
    key: &ControllerKey,
    proof: ParallelProofContext<'_>,
    assignments: &BTreeMap<(u32, u32), &ControllerAssignment>,
    dispositions: &BTreeMap<(u32, u32), &AttemptDisposition>,
    receipts: &'a [ShardReceipt],
) -> Result<BTreeMap<(u32, u32), &'a ShardReceipt>, ParallelProofError> {
    let mut indexed = BTreeMap::new();
    for receipt in receipts {
        receipt.verify(key)?;
        let attempt_key = (receipt.shard_id, receipt.attempt);
        let Some(assignment) = assignments.get(&attempt_key) else {
            return Err(ParallelProofError::ImmutableConflict(format!(
                "unauthorized receipt {}:{}",
                receipt.shard_id, receipt.attempt
            )));
        };
        validate_receipt_binding(proof.manifest, proof.plan, assignment, receipt)?;
        let disposition = dispositions.get(&attempt_key).ok_or_else(|| {
            ParallelProofError::MissingRecord(format!(
                "attempt disposition {}:{}",
                receipt.shard_id, receipt.attempt
            ))
        })?;
        validate_receipt_disposition(disposition, receipt)?;
        match indexed.insert(attempt_key, receipt) {
            Some(previous) if previous != receipt => {
                return Err(ParallelProofError::ImmutableConflict(format!(
                    "receipt {}:{}",
                    receipt.shard_id, receipt.attempt
                )));
            }
            _ => {}
        }
    }
    Ok(indexed)
}

/// Outcome of an immutable durable-store write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreWriteOutcome {
    /// A new immutable record was published and directory-synced.
    Created,
    /// The identical immutable bytes already existed and were retained.
    AlreadyPresent,
}

/// Crash-durable, no-overwrite local store for parallel-proof records.
#[derive(Clone, Debug)]
pub struct ParallelProofStore {
    root: PathBuf,
}

impl ParallelProofStore {
    /// Create or reopen a controller-owned record-store root.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ParallelProofError> {
        let root = root.into();
        if root.file_name().is_none()
            || root
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(ParallelProofError::InvalidField("store root"));
        }
        let parent = store_root_parent(&root)?;
        let parent_metadata = fs::symlink_metadata(parent).map_err(|parent_error| {
            if parent_error.kind() == std::io::ErrorKind::NotFound {
                ParallelProofError::InvalidField("store root parent")
            } else {
                ParallelProofError::Io(parent_error)
            }
        })?;
        if !parent_metadata.file_type().is_dir() {
            return Err(ParallelProofError::InvalidField("store root parent"));
        }
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(ParallelProofError::CorruptRecord(format!(
                    "{} is not a real directory",
                    root.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&root) {
                    Ok(()) => {}
                    Err(create_error)
                        if create_error.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        let metadata = fs::symlink_metadata(&root)?;
                        if !metadata.file_type().is_dir() {
                            return Err(ParallelProofError::CorruptRecord(format!(
                                "{} is not a real directory",
                                root.display()
                            )));
                        }
                    }
                    Err(create_error) => return Err(ParallelProofError::Io(create_error)),
                }
            }
            Err(error) => return Err(ParallelProofError::Io(error)),
        }
        sync_directory(parent)?;
        sync_directory(&root)?;
        Ok(Self { root })
    }

    /// Persist a validated manifest under its content digest.
    pub fn record_manifest(
        &self,
        manifest: &ParallelProofManifest,
        inventory: &TestInventory,
        plan: &ShardPlan,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        let key = manifest.digest(inventory, plan)?.as_str().to_owned();
        self.put(RecordKind::Manifest, &key, manifest, CrashPoint::None)
    }

    /// Persist a validated canonical inventory under its content digest.
    pub fn record_inventory(
        &self,
        inventory: &TestInventory,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        let key = inventory.digest()?.as_str().to_owned();
        self.put(RecordKind::Inventory, &key, inventory, CrashPoint::None)
    }

    /// Persist a validated exhaustive shard plan under its content digest.
    pub fn record_plan(
        &self,
        inventory: &TestInventory,
        plan: &ShardPlan,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        let key = plan.digest(inventory)?.as_str().to_owned();
        self.put(RecordKind::Plan, &key, plan, CrashPoint::None)
    }

    /// Persist a verified immutable controller assignment.
    pub fn record_assignment(
        &self,
        key: &ControllerKey,
        assignment: &ControllerAssignment,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        assignment.verify(key)?;
        self.put(
            RecordKind::Assignment,
            &assignment.logical_key(),
            assignment,
            CrashPoint::None,
        )
    }

    /// Persist a verified immutable terminal attempt disposition.
    pub fn record_disposition(
        &self,
        key: &ControllerKey,
        assignment: &ControllerAssignment,
        disposition: &AttemptDisposition,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        disposition.verify(key, assignment)?;
        self.put(
            RecordKind::Disposition,
            &disposition.logical_key(),
            disposition,
            CrashPoint::None,
        )
    }

    /// Persist a structurally valid immutable shard receipt.
    pub fn record_receipt(
        &self,
        key: &ControllerKey,
        receipt: &ShardReceipt,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        receipt.verify(key)?;
        self.put(
            RecordKind::Receipt,
            &receipt.logical_key(),
            receipt,
            CrashPoint::None,
        )
    }

    /// Load and integrity-check an immutable shard receipt.
    pub fn load_receipt(
        &self,
        key: &ControllerKey,
        logical_key: &str,
    ) -> Result<ShardReceipt, ParallelProofError> {
        let receipt: ShardReceipt = self.load(RecordKind::Receipt, logical_key)?;
        receipt.verify(key)?;
        if receipt.logical_key() != logical_key {
            return Err(ParallelProofError::CorruptRecord(
                "receipt logical key mismatch".to_owned(),
            ));
        }
        Ok(receipt)
    }

    /// Load and verify an immutable terminal attempt disposition.
    pub fn load_disposition(
        &self,
        key: &ControllerKey,
        assignment: &ControllerAssignment,
        logical_key: &str,
    ) -> Result<AttemptDisposition, ParallelProofError> {
        let disposition: AttemptDisposition = self.load(RecordKind::Disposition, logical_key)?;
        disposition.verify(key, assignment)?;
        if disposition.logical_key() != logical_key {
            return Err(ParallelProofError::CorruptRecord(
                "disposition logical key mismatch".to_owned(),
            ));
        }
        Ok(disposition)
    }

    fn put<T: Serialize>(
        &self,
        kind: RecordKind,
        logical_key: &str,
        value: &T,
        crash: CrashPoint,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        validate_identifier("record key", logical_key)?;
        let payload = serde_json::to_value(value)?;
        let payload_bytes = serde_json::to_vec(&payload)?;
        if payload_bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(ParallelProofError::LimitExceeded {
                field: "durable payload bytes",
                max: MAX_PAYLOAD_BYTES,
                found: payload_bytes.len(),
            });
        }
        let envelope = StoredEnvelope {
            schema_version: PARALLEL_PROOF_SCHEMA_VERSION,
            kind,
            logical_key: logical_key.to_owned(),
            payload_sha256: canonical_digest("shipyard.parallel-proof.store-payload.v1", &payload)?,
            payload,
        };
        let bytes = serde_json::to_vec(&envelope)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(ParallelProofError::LimitExceeded {
                field: "durable record bytes",
                max: MAX_RECORD_BYTES,
                found: bytes.len(),
            });
        }
        self.publish(kind, logical_key, &bytes, crash)
    }

    fn publish(
        &self,
        kind: RecordKind,
        logical_key: &str,
        bytes: &[u8],
        crash: CrashPoint,
    ) -> Result<StoreWriteOutcome, ParallelProofError> {
        let directory = self.root.join(kind.directory());
        ensure_store_child_directory(&self.root, &directory)?;
        let file_stem = store_file_stem(kind, logical_key);
        let destination = directory.join(format!("{file_stem}.json"));
        let lock_path = directory.join(format!("{file_stem}.lock"));
        reject_non_regular_if_present(&destination)?;
        reject_non_regular_if_present(&lock_path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        lock.lock_exclusive()?;
        let result = (|| {
            if destination.exists() {
                let existing = read_bounded(&destination)?;
                if existing == bytes {
                    sync_directory(&directory)?;
                    return Ok(StoreWriteOutcome::AlreadyPresent);
                }
                return Err(ParallelProofError::ImmutableConflict(
                    logical_key.to_owned(),
                ));
            }

            let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
            temporary.write_all(bytes)?;
            temporary.as_file_mut().sync_all()?;
            if crash == CrashPoint::AfterTempSync {
                return Err(ParallelProofError::CrashInjected("after_temp_sync"));
            }
            match temporary.persist_noclobber(&destination) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let existing = read_bounded(&destination)?;
                    if existing == bytes {
                        sync_directory(&directory)?;
                        return Ok(StoreWriteOutcome::AlreadyPresent);
                    }
                    return Err(ParallelProofError::ImmutableConflict(
                        logical_key.to_owned(),
                    ));
                }
                Err(error) => return Err(ParallelProofError::Io(error.error)),
            }
            if crash == CrashPoint::AfterPublish {
                return Err(ParallelProofError::CrashInjected("after_publish"));
            }
            sync_directory(&directory)?;
            Ok(StoreWriteOutcome::Created)
        })();
        FileExt::unlock(&lock)?;
        result
    }

    fn load<T: DeserializeOwned>(
        &self,
        kind: RecordKind,
        logical_key: &str,
    ) -> Result<T, ParallelProofError> {
        validate_identifier("record key", logical_key)?;
        let path = self
            .root
            .join(kind.directory())
            .join(format!("{}.json", store_file_stem(kind, logical_key)));
        reject_non_regular_if_present(&path)?;
        if !path.exists() {
            return Err(ParallelProofError::MissingRecord(logical_key.to_owned()));
        }
        let bytes = read_bounded(&path)?;
        let envelope: StoredEnvelope = serde_json::from_slice(&bytes)
            .map_err(|error| ParallelProofError::CorruptRecord(error.to_string()))?;
        if envelope.schema_version != PARALLEL_PROOF_SCHEMA_VERSION
            || envelope.kind != kind
            || envelope.logical_key != logical_key
        {
            return Err(ParallelProofError::CorruptRecord(
                "envelope identity mismatch".to_owned(),
            ));
        }
        let expected = canonical_digest(
            "shipyard.parallel-proof.store-payload.v1",
            &envelope.payload,
        )?;
        if expected != envelope.payload_sha256 {
            return Err(ParallelProofError::CorruptRecord(
                "payload digest mismatch".to_owned(),
            ));
        }
        serde_json::from_value(envelope.payload)
            .map_err(|error| ParallelProofError::CorruptRecord(error.to_string()))
    }
}

fn store_root_parent(root: &Path) -> Result<&Path, ParallelProofError> {
    match root.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Ok(Path::new(".")),
        Some(parent) => Ok(parent),
        None => Err(ParallelProofError::InvalidField("store root parent")),
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RecordKind {
    Manifest,
    Inventory,
    Plan,
    Assignment,
    Disposition,
    Receipt,
}

impl RecordKind {
    const fn directory(self) -> &'static str {
        match self {
            Self::Manifest => "manifests",
            Self::Inventory => "inventories",
            Self::Plan => "plans",
            Self::Assignment => "assignments",
            Self::Disposition => "dispositions",
            Self::Receipt => "receipts",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredEnvelope {
    schema_version: u32,
    kind: RecordKind,
    logical_key: String,
    payload_sha256: Sha256Digest,
    payload: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CrashPoint {
    None,
    AfterTempSync,
    AfterPublish,
}

fn validate_version(version: u32) -> Result<(), ParallelProofError> {
    if version == PARALLEL_PROOF_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ParallelProofError::UnsupportedSchemaVersion(version))
    }
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), ParallelProofError> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > MAX_IDENTIFIER_BYTES
        || value.chars().any(char::is_control)
    {
        Err(ParallelProofError::InvalidField(field))
    } else {
        Ok(())
    }
}

fn validate_git_sha(field: &'static str, value: &str) -> Result<(), ParallelProofError> {
    if matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(ParallelProofError::InvalidField(field))
    }
}

fn ensure_count(
    field: &'static str,
    found: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), ParallelProofError> {
    if found < minimum {
        Err(ParallelProofError::InvalidField(field))
    } else if found > maximum {
        Err(ParallelProofError::LimitExceeded {
            field,
            max: maximum,
            found,
        })
    } else {
        Ok(())
    }
}

fn ensure_serialized_bound<T: Serialize + ?Sized>(
    field: &'static str,
    value: &T,
) -> Result<(), ParallelProofError> {
    let mut writer = BoundedSizeWriter {
        written: 0,
        maximum: MAX_PAYLOAD_BYTES,
        exceeded: false,
    };
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(()),
        Err(_) if writer.exceeded => Err(ParallelProofError::LimitExceeded {
            field,
            max: MAX_PAYLOAD_BYTES,
            found: MAX_PAYLOAD_BYTES + 1,
        }),
        Err(error) => Err(error.into()),
    }
}

struct BoundedSizeWriter {
    written: usize,
    maximum: usize,
    exceeded: bool,
}

impl Write for BoundedSizeWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.maximum.saturating_sub(self.written) {
            self.exceeded = true;
            return Err(std::io::Error::other("serialized value exceeds bound"));
        }
        self.written += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn strictly_sorted<'a>(mut values: impl Iterator<Item = &'a str>) -> bool {
    let Some(mut previous) = values.next() else {
        return true;
    };
    for value in values {
        if value <= previous {
            return false;
        }
        previous = value;
    }
    true
}

fn reject_adjacent_duplicates<'a>(
    values: impl Iterator<Item = &'a str>,
    field: &'static str,
) -> Result<(), ParallelProofError> {
    let mut previous = None;
    for value in values {
        if previous == Some(value) {
            return Err(ParallelProofError::NonCanonical(field));
        }
        previous = Some(value);
    }
    Ok(())
}

fn sort_unique_strings(
    values: &mut [String],
    field: &'static str,
) -> Result<(), ParallelProofError> {
    for value in values.iter() {
        validate_identifier(field, value)?;
    }
    values.sort();
    reject_adjacent_duplicates(values.iter().map(String::as_str), field)
}

fn ensure_test_relation_bound(tests: &[TestCase]) -> Result<(), ParallelProofError> {
    let mut relation_count = 0_usize;
    for test in tests {
        for count in [
            test.dependencies.len(),
            test.fixture_setup.len(),
            test.fixture_required.len(),
            test.fixture_cleanup.len(),
            test.resource_locks.len(),
            test.required_capabilities.len(),
        ] {
            relation_count =
                relation_count
                    .checked_add(count)
                    .ok_or(ParallelProofError::LimitExceeded {
                        field: "test relations",
                        max: MAX_RELATIONS,
                        found: usize::MAX,
                    })?;
            if relation_count > MAX_RELATIONS {
                return Err(ParallelProofError::LimitExceeded {
                    field: "test relations",
                    max: MAX_RELATIONS,
                    found: relation_count,
                });
            }
        }
    }
    Ok(())
}

fn canonicalize_test(test: &mut TestCase) -> Result<(), ParallelProofError> {
    validate_identifier("test id", &test.id)?;
    sort_unique_strings(&mut test.dependencies, "dependencies")?;
    sort_unique_strings(&mut test.fixture_setup, "fixture setup")?;
    sort_unique_strings(&mut test.fixture_required, "fixture required")?;
    sort_unique_strings(&mut test.fixture_cleanup, "fixture cleanup")?;
    sort_unique_strings(&mut test.required_capabilities, "required capabilities")?;
    for lock in &test.resource_locks {
        validate_identifier("resource lock", &lock.name)?;
    }
    test.resource_locks.sort();
    if test
        .resource_locks
        .windows(2)
        .any(|pair| pair[0] == pair[1])
    {
        return Err(ParallelProofError::NonCanonical("resource locks"));
    }
    Ok(())
}

fn validate_canonical_test(test: &TestCase) -> Result<(), ParallelProofError> {
    let mut canonical = test.clone();
    canonicalize_test(&mut canonical)?;
    if &canonical == test {
        Ok(())
    } else {
        Err(ParallelProofError::NonCanonical("test metadata"))
    }
}

fn validate_sorted_identifiers(
    field: &'static str,
    values: &[String],
) -> Result<(), ParallelProofError> {
    if values.len() > MAX_RELATIONS {
        return Err(ParallelProofError::LimitExceeded {
            field,
            max: MAX_RELATIONS,
            found: values.len(),
        });
    }
    for value in values {
        validate_identifier(field, value)?;
    }
    if strictly_sorted(values.iter().map(String::as_str)) {
        Ok(())
    } else {
        Err(ParallelProofError::NonCanonical(field))
    }
}

fn validate_dependency_acyclic(inventory: &TestInventory) -> Result<(), ParallelProofError> {
    let mut indegree = inventory
        .tests
        .iter()
        .map(|test| (test.id.as_str(), test.dependencies.len()))
        .collect::<BTreeMap<_, _>>();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for test in &inventory.tests {
        for dependency in &test.dependencies {
            dependents
                .entry(dependency.as_str())
                .or_default()
                .push(test.id.as_str());
        }
    }
    let mut ready = indegree
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(*id))
        .collect::<BTreeSet<_>>();
    let mut visited = 0_usize;
    while let Some(id) = ready.pop_first() {
        visited += 1;
        if let Some(children) = dependents.get(id) {
            for child in children {
                let count = indegree
                    .get_mut(child)
                    .ok_or_else(|| ParallelProofError::UnknownTest((*child).to_owned()))?;
                *count -= 1;
                if *count == 0 {
                    ready.insert(child);
                }
            }
        }
    }
    if visited == inventory.tests.len() {
        Ok(())
    } else {
        Err(ParallelProofError::TopologyViolation(
            "dependency graph contains a cycle".to_owned(),
        ))
    }
}

fn inventory_lookup(inventory: &TestInventory) -> BTreeMap<&str, &TestCase> {
    inventory
        .tests
        .iter()
        .map(|test| (test.id.as_str(), test))
        .collect()
}

fn topology_components(inventory: &TestInventory) -> Result<Vec<Vec<usize>>, ParallelProofError> {
    let indexes = inventory
        .tests
        .iter()
        .enumerate()
        .map(|(index, test)| (test.id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let mut parent = (0..inventory.tests.len()).collect::<Vec<_>>();
    for (index, test) in inventory.tests.iter().enumerate() {
        for dependency in &test.dependencies {
            union(
                &mut parent,
                index,
                *indexes
                    .get(dependency.as_str())
                    .ok_or_else(|| ParallelProofError::UnknownTest(dependency.clone()))?,
            );
        }
    }
    let mut fixture_owner = BTreeMap::new();
    for (index, test) in inventory.tests.iter().enumerate() {
        for fixture in test
            .fixture_setup
            .iter()
            .chain(test.fixture_required.iter())
            .chain(test.fixture_cleanup.iter())
        {
            if let Some(previous) = fixture_owner.insert(fixture.as_str(), index) {
                union(&mut parent, index, previous);
            }
        }
    }
    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..inventory.tests.len() {
        let root = find(&mut parent, index);
        components.entry(root).or_default().push(index);
    }
    let mut components = components.into_values().collect::<Vec<_>>();
    components.sort_by(|left, right| {
        inventory.tests[left[0]]
            .id
            .cmp(&inventory.tests[right[0]].id)
    });
    Ok(components)
}

fn find(parent: &mut [usize], index: usize) -> usize {
    let mut root = index;
    while parent[root] != root {
        root = parent[root];
    }
    let mut current = index;
    while parent[current] != current {
        let next = parent[current];
        parent[current] = root;
        current = next;
    }
    root
}

fn union(parent: &mut [usize], left: usize, right: usize) {
    let left_root = find(parent, left);
    let right_root = find(parent, right);
    if left_root != right_root {
        parent[right_root] = left_root;
    }
}

fn validate_source(source: &SourceIdentity) -> Result<(), ParallelProofError> {
    if source.repository_id == 0 {
        return Err(ParallelProofError::InvalidField("repository id"));
    }
    validate_identifier("repository", &source.repository)?;
    let mut repository = source.repository.split('/');
    let valid_repository = repository.next().is_some_and(valid_repository_component)
        && repository.next().is_some_and(valid_repository_component)
        && repository.next().is_none();
    if !valid_repository {
        return Err(ParallelProofError::InvalidField("repository"));
    }
    match &source.subject {
        ProofSubject::PullRequest { number } if *number > 0 => {}
        ProofSubject::MergeGroup { id } => validate_identifier("merge group id", id)?,
        ProofSubject::PullRequest { .. } => {
            return Err(ParallelProofError::InvalidField("pull request number"));
        }
    }
    validate_git_sha("head sha", &source.head_sha)?;
    validate_git_sha("tree sha", &source.tree_sha)?;
    if source.head_sha.len() != source.tree_sha.len() {
        return Err(ParallelProofError::InvalidField("git object format"));
    }
    Ok(())
}

fn valid_repository_component(component: &str) -> bool {
    !component.is_empty()
        && component.bytes().any(|byte| byte.is_ascii_alphanumeric())
        && component.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn validate_build(build: &BuildIdentity) -> Result<(), ParallelProofError> {
    validate_identifier("target triple", &build.target_triple)?;
    validate_identifier("build profile", &build.profile)
}

fn validate_artifact(artifact: &ArtifactIdentity) -> Result<(), ParallelProofError> {
    validate_git_sha("artifact source tree", &artifact.source_tree_sha)?;
    if artifact.size_bytes == 0 {
        Err(ParallelProofError::InvalidField("artifact size"))
    } else {
        Ok(())
    }
}

fn validate_trust(trust: &TrustIdentity) -> Result<(), ParallelProofError> {
    if trust.artifact_class == ArtifactTrustClass::UntrustedContributor
        && (trust.execution_boundary != ExecutionBoundary::DisposableGuest
            || trust.network_enabled
            || trust.writable_host_mounts)
    {
        return Err(ParallelProofError::InvalidField(
            "untrusted artifact boundary",
        ));
    }
    Ok(())
}

fn required_capabilities<'a>(
    inventory: &'a TestInventory,
    shard: &'a TestShard,
) -> BTreeSet<&'a str> {
    let lookup = inventory_lookup(inventory);
    shard
        .test_ids
        .iter()
        .flat_map(|id| lookup[id.as_str()].required_capabilities.iter())
        .map(String::as_str)
        .collect()
}

fn validate_assignment_binding(
    manifest: &ParallelProofManifest,
    inventory: &TestInventory,
    plan: &ShardPlan,
    assignment: &ControllerAssignment,
) -> Result<(), ParallelProofError> {
    let proof = ParallelProofContext {
        manifest,
        inventory,
        plan,
    };
    proof.validate()?;
    let bindings = ValidatedProofBindings::from_validated(proof)?;
    validate_assignment_binding_cached(manifest, &bindings, assignment)
}

fn validate_assignment_binding_cached(
    manifest: &ParallelProofManifest,
    bindings: &ValidatedProofBindings<'_>,
    assignment: &ControllerAssignment,
) -> Result<(), ParallelProofError> {
    let claims = &assignment.claims;
    let shard = bindings
        .shard(claims.shard_id)
        .ok_or(ParallelProofError::InvalidField("shard id"))?;
    if claims.manifest_digest != bindings.manifest_digest
        || claims.inventory_digest != bindings.inventory_digest
        || claims.plan_digest != bindings.plan_digest
        || claims.artifact_sha256 != manifest.artifact.payload_sha256
        || claims.shard_digest != shard.digest
        || claims.execution_mode != shard.execution_mode
        || claims.execution_boundary != manifest.trust.execution_boundary
    {
        return Err(ParallelProofError::BindingMismatch("assignment claims"));
    }
    let available = claims.capabilities.iter().map(String::as_str).collect();
    if shard.required_capabilities.is_subset(&available) {
        Ok(())
    } else {
        Err(ParallelProofError::BindingMismatch(
            "assignment capabilities",
        ))
    }
}

fn validate_receipt_binding(
    manifest: &ParallelProofManifest,
    plan: &ShardPlan,
    assignment: &ControllerAssignment,
    receipt: &ShardReceipt,
) -> Result<(), ParallelProofError> {
    let claims = &assignment.claims;
    let shard = plan
        .shard(claims.shard_id)
        .ok_or(ParallelProofError::InvalidField("shard id"))?;
    if receipt.manifest_digest != claims.manifest_digest
        || receipt.inventory_digest != claims.inventory_digest
        || receipt.plan_digest != claims.plan_digest
        || receipt.assignment_digest != assignment.digest()?
        || receipt.artifact_sha256 != manifest.artifact.payload_sha256
        || receipt.shard_id != claims.shard_id
        || receipt.attempt != claims.attempt
        || receipt.fence != claims.fence
        || receipt.host_id != claims.host_id
        || receipt.worker_identity_sha256 != claims.worker_identity_sha256
        || receipt.session_generation != claims.session_generation
        || receipt.execution_mode != claims.execution_mode
        || receipt.execution_boundary != claims.execution_boundary
        || receipt
            .outcomes
            .iter()
            .map(|outcome| outcome.test_id.as_str())
            .ne(shard.test_ids.iter().map(String::as_str))
    {
        return Err(ParallelProofError::BindingMismatch("receipt claims"));
    }
    Ok(())
}

fn validate_receipt_disposition(
    disposition: &AttemptDisposition,
    receipt: &ShardReceipt,
) -> Result<(), ParallelProofError> {
    let AttemptDispositionKind::Executed {
        started_at_ms,
        completed_at_ms,
        execution_boundary,
        guest_teardown_confirmed,
    } = disposition.kind
    else {
        return Err(ParallelProofError::BindingMismatch(
            "receipt for unstarted attempt",
        ));
    };
    if receipt.disposition_digest != disposition.digest()?
        || receipt.started_at_ms != started_at_ms
        || receipt.completed_at_ms != completed_at_ms
        || receipt.execution_boundary != execution_boundary
        || receipt.guest_teardown_confirmed != guest_teardown_confirmed
    {
        return Err(ParallelProofError::BindingMismatch("receipt disposition"));
    }
    Ok(())
}

fn validate_attempt_chains<'a>(
    key: &ControllerKey,
    manifest: &ParallelProofManifest,
    bindings: &ValidatedProofBindings<'_>,
    assignments: &'a [ControllerAssignment],
) -> Result<BTreeMap<u32, &'a ControllerAssignment>, ParallelProofError> {
    let mut by_shard: BTreeMap<u32, BTreeMap<u32, &'a ControllerAssignment>> = BTreeMap::new();
    let mut fences = BTreeSet::new();
    for assignment in assignments {
        assignment.verify(key)?;
        validate_assignment_binding_cached(manifest, bindings, assignment)?;
        if !fences.insert(assignment.claims.fence) {
            return Err(ParallelProofError::InvalidAttemptSequence(format!(
                "duplicate fence {}",
                assignment.claims.fence
            )));
        }
        let attempts = by_shard.entry(assignment.claims.shard_id).or_default();
        if let Some(previous) = attempts.insert(assignment.claims.attempt, assignment)
            && previous != assignment
        {
            return Err(ParallelProofError::ImmutableConflict(format!(
                "assignment {}:{}",
                assignment.claims.shard_id, assignment.claims.attempt
            )));
        }
    }
    let mut active = BTreeMap::new();
    for (shard, attempts) in by_shard {
        let mut previous_fence = 0_u64;
        for (index, (attempt, assignment)) in attempts.iter().enumerate() {
            let expected = u32::try_from(index + 1)
                .map_err(|_| ParallelProofError::InvalidField("attempt"))?;
            if *attempt != expected || assignment.claims.fence <= previous_fence {
                return Err(ParallelProofError::InvalidAttemptSequence(format!(
                    "shard {shard} must have contiguous attempts and increasing fences"
                )));
            }
            previous_fence = assignment.claims.fence;
        }
        let assignment = attempts
            .last_key_value()
            .map(|(_, assignment)| *assignment)
            .ok_or_else(|| {
                ParallelProofError::InvalidAttemptSequence(format!(
                    "shard {shard} has no assignment"
                ))
            })?;
        active.insert(shard, assignment);
    }
    Ok(active)
}

struct ObservedExecution {
    shard_id: u32,
    execution_mode: ShardExecutionMode,
    host_id: String,
    started_at_ms: u64,
    completed_at_ms: u64,
}

fn observed_execution(
    assignment: &ControllerAssignment,
    disposition: &AttemptDisposition,
) -> Option<ObservedExecution> {
    let AttemptDispositionKind::Executed {
        started_at_ms,
        completed_at_ms,
        ..
    } = disposition.kind
    else {
        return None;
    };
    Some(ObservedExecution {
        shard_id: assignment.claims.shard_id,
        execution_mode: assignment.claims.execution_mode,
        host_id: assignment.claims.host_id.clone(),
        started_at_ms,
        completed_at_ms,
    })
}

fn validate_execution_overlap(
    inventory: &TestInventory,
    plan: &ShardPlan,
    executions: &[ObservedExecution],
) -> Result<(), ParallelProofError> {
    let lookup = inventory_lookup(inventory);
    let shard_locks = plan
        .shards
        .iter()
        .map(|shard| {
            shard
                .test_ids
                .iter()
                .flat_map(|id| lookup[id.as_str()].resource_locks.iter())
                .map(|lock| (lock.name.as_str(), lock.scope))
                .collect::<BTreeSet<_>>()
        })
        .collect::<Vec<_>>();
    let mut ordered = executions.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        (
            left.started_at_ms,
            left.completed_at_ms,
            left.shard_id,
            &left.host_id,
        )
            .cmp(&(
                right.started_at_ms,
                right.completed_at_ms,
                right.shard_id,
                &right.host_id,
            ))
    });

    validate_same_shard_overlap(&ordered)?;
    validate_fleet_exclusive_overlap(&ordered)?;
    validate_fleet_lock_overlap(&ordered, &shard_locks)?;
    validate_host_lock_overlap(&ordered, &shard_locks)
}

fn validate_same_shard_overlap(
    executions: &[&ObservedExecution],
) -> Result<(), ParallelProofError> {
    let mut ordered = executions.to_vec();
    ordered.sort_by_key(|execution| {
        (
            execution.shard_id,
            execution.started_at_ms,
            execution.completed_at_ms,
        )
    });
    let mut current_shard = None;
    let mut furthest: Option<&ObservedExecution> = None;
    for execution in ordered {
        if current_shard != Some(execution.shard_id) {
            current_shard = Some(execution.shard_id);
            furthest = None;
        }
        if let Some(previous) =
            furthest.filter(|previous| previous.completed_at_ms > execution.started_at_ms)
        {
            return Err(ParallelProofError::TopologyViolation(format!(
                "shard {} attempts overlapped intervals ending at {} and starting at {}",
                execution.shard_id, previous.completed_at_ms, execution.started_at_ms
            )));
        }
        if furthest.is_none_or(|previous| execution.completed_at_ms > previous.completed_at_ms) {
            furthest = Some(execution);
        }
    }
    Ok(())
}

fn validate_fleet_exclusive_overlap(
    ordered: &[&ObservedExecution],
) -> Result<(), ParallelProofError> {
    let mut furthest_any: Option<&ObservedExecution> = None;
    let mut furthest_exclusive: Option<&ObservedExecution> = None;
    for execution in ordered {
        let conflicting = if execution.execution_mode == ShardExecutionMode::FleetExclusive {
            furthest_any.filter(|previous| previous.completed_at_ms > execution.started_at_ms)
        } else {
            furthest_exclusive.filter(|previous| previous.completed_at_ms > execution.started_at_ms)
        };
        if let Some(previous) = conflicting {
            return Err(ParallelProofError::TopologyViolation(format!(
                "fleet-exclusive shard overlapped {} and {}",
                previous.shard_id, execution.shard_id
            )));
        }
        if furthest_any.is_none_or(|previous| execution.completed_at_ms > previous.completed_at_ms)
        {
            furthest_any = Some(execution);
        }
        if execution.execution_mode == ShardExecutionMode::FleetExclusive
            && furthest_exclusive
                .is_none_or(|previous| execution.completed_at_ms > previous.completed_at_ms)
        {
            furthest_exclusive = Some(execution);
        }
    }
    Ok(())
}

fn validate_fleet_lock_overlap(
    ordered: &[&ObservedExecution],
    shard_locks: &[BTreeSet<(&str, ResourceLockScope)>],
) -> Result<(), ParallelProofError> {
    let mut furthest_by_fleet_lock: BTreeMap<&str, &ObservedExecution> = BTreeMap::new();
    for execution in ordered {
        let locks = shard_locks
            .get(
                usize::try_from(execution.shard_id)
                    .map_err(|_| ParallelProofError::InvalidField("shard id"))?,
            )
            .ok_or(ParallelProofError::InvalidField("shard id"))?;
        for (lock_name, _) in locks
            .iter()
            .filter(|(_, scope)| *scope == ResourceLockScope::Fleet)
        {
            if let Some(previous) = furthest_by_fleet_lock
                .get(lock_name)
                .filter(|previous| previous.completed_at_ms > execution.started_at_ms)
            {
                return Err(ParallelProofError::TopologyViolation(format!(
                    "resource lock {lock_name} overlapped shards {} and {}",
                    previous.shard_id, execution.shard_id
                )));
            }
            if furthest_by_fleet_lock
                .get(lock_name)
                .is_none_or(|previous| execution.completed_at_ms > previous.completed_at_ms)
            {
                furthest_by_fleet_lock.insert(lock_name, execution);
            }
        }
    }
    Ok(())
}

fn validate_host_lock_overlap(
    ordered: &[&ObservedExecution],
    shard_locks: &[BTreeSet<(&str, ResourceLockScope)>],
) -> Result<(), ParallelProofError> {
    let mut ordered_by_host = ordered.to_vec();
    ordered_by_host.sort_by(|left, right| {
        (
            &left.host_id,
            left.started_at_ms,
            left.completed_at_ms,
            left.shard_id,
        )
            .cmp(&(
                &right.host_id,
                right.started_at_ms,
                right.completed_at_ms,
                right.shard_id,
            ))
    });
    let mut current_host = None;
    let mut furthest_by_host_lock: BTreeMap<&str, &ObservedExecution> = BTreeMap::new();
    for execution in ordered_by_host {
        if current_host != Some(execution.host_id.as_str()) {
            current_host = Some(execution.host_id.as_str());
            furthest_by_host_lock.clear();
        }
        let locks = shard_locks
            .get(
                usize::try_from(execution.shard_id)
                    .map_err(|_| ParallelProofError::InvalidField("shard id"))?,
            )
            .ok_or(ParallelProofError::InvalidField("shard id"))?;
        for (lock_name, _) in locks
            .iter()
            .filter(|(_, scope)| *scope == ResourceLockScope::Host)
        {
            if let Some(previous) = furthest_by_host_lock
                .get(lock_name)
                .filter(|previous| previous.completed_at_ms > execution.started_at_ms)
            {
                return Err(ParallelProofError::TopologyViolation(format!(
                    "resource lock {lock_name} overlapped shards {} and {}",
                    previous.shard_id, execution.shard_id
                )));
            }
            if furthest_by_host_lock
                .get(lock_name)
                .is_none_or(|previous| execution.completed_at_ms > previous.completed_at_ms)
            {
                furthest_by_host_lock.insert(lock_name, execution);
            }
        }
    }
    Ok(())
}

fn assignment_authentication(
    key: &ControllerKey,
    claims: &AssignmentClaims,
) -> Result<Sha256Digest, ParallelProofError> {
    let bytes = serde_json::to_vec(claims)?;
    Ok(Sha256Digest(hex::encode(hmac_sha256(
        &key.secret,
        b"shipyard.parallel-proof.assignment-auth.v1",
        &bytes,
    ))))
}

fn receipt_authentication(
    key: &ControllerKey,
    receipt: &ShardReceipt,
) -> Result<Sha256Digest, ParallelProofError> {
    let mut value = serde_json::to_value(receipt)?;
    let fields = value
        .as_object_mut()
        .ok_or(ParallelProofError::InvalidField("receipt authentication"))?;
    if fields.remove("acceptance_authentication").is_none() {
        return Err(ParallelProofError::InvalidField("receipt authentication"));
    }
    let bytes = serde_json::to_vec(&value)?;
    Ok(Sha256Digest(hex::encode(hmac_sha256(
        &key.secret,
        b"shipyard.parallel-proof.receipt-auth.v1",
        &bytes,
    ))))
}

fn disposition_authentication(
    key: &ControllerKey,
    disposition: &AttemptDisposition,
) -> Result<Sha256Digest, ParallelProofError> {
    let mut value = serde_json::to_value(disposition)?;
    let fields = value
        .as_object_mut()
        .ok_or(ParallelProofError::InvalidField(
            "disposition authentication",
        ))?;
    if fields.remove("authentication").is_none() {
        return Err(ParallelProofError::InvalidField(
            "disposition authentication",
        ));
    }
    let bytes = serde_json::to_vec(&value)?;
    Ok(Sha256Digest(hex::encode(hmac_sha256(
        &key.secret,
        b"shipyard.parallel-proof.disposition-auth.v1",
        &bytes,
    ))))
}

fn digest_placeholder() -> Sha256Digest {
    Sha256Digest::of_bytes(b"pending controller receipt authentication")
}

fn hmac_sha256(key: &[u8], domain: &[u8], message: &[u8]) -> [u8; 32] {
    let mut normalized = [0_u8; HMAC_BLOCK_BYTES];
    if key.len() > HMAC_BLOCK_BYTES {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; HMAC_BLOCK_BYTES];
    let mut outer_pad = [0x5c_u8; HMAC_BLOCK_BYTES];
    for index in 0..HMAC_BLOCK_BYTES {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update((domain.len() as u64).to_be_bytes());
    inner.update(domain);
    inner.update((message.len() as u64).to_be_bytes());
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn canonical_digest<T: Serialize + ?Sized>(
    domain: &str,
    value: &T,
) -> Result<Sha256Digest, ParallelProofError> {
    let bytes = serde_json::to_vec(value)?;
    let mut hash = Sha256::new();
    hash.update((domain.len() as u64).to_be_bytes());
    hash.update(domain.as_bytes());
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
    Ok(Sha256Digest(hex::encode(hash.finalize())))
}

fn store_file_stem(kind: RecordKind, logical_key: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"shipyard.parallel-proof.store-key.v1");
    hash.update(kind.directory().as_bytes());
    hash.update(logical_key.as_bytes());
    hex::encode(hash.finalize())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, ParallelProofError> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(ParallelProofError::LimitExceeded {
            field: "durable record bytes",
            max: MAX_RECORD_BYTES,
            found: bytes.len(),
        });
    }
    Ok(bytes)
}

fn reject_non_regular_if_present(path: &Path) -> Result<(), ParallelProofError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => Err(ParallelProofError::CorruptRecord(
            format!("{} is not a regular file", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ParallelProofError::Io(error)),
    }
}

fn ensure_store_child_directory(root: &Path, directory: &Path) -> Result<(), ParallelProofError> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() => sync_directory(root),
        Ok(_) => Err(ParallelProofError::CorruptRecord(format!(
            "{} is not a real directory",
            directory.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::create_dir(directory) {
                Ok(()) => {}
                Err(create_error) if create_error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(directory)?;
                    if !metadata.file_type().is_dir() {
                        return Err(ParallelProofError::CorruptRecord(format!(
                            "{} is not a real directory",
                            directory.display()
                        )));
                    }
                }
                Err(create_error) => return Err(ParallelProofError::Io(create_error)),
            }
            sync_directory(root)
        }
        Err(error) => Err(ParallelProofError::Io(error)),
    }
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> Result<(), ParallelProofError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "keep one fallible durability contract where Windows has no directory fsync"
)]
fn sync_directory(_path: &Path) -> Result<(), ParallelProofError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    struct Fixture {
        inventory: TestInventory,
        plan: ShardPlan,
        manifest: ParallelProofManifest,
        key: ControllerKey,
        assignments: Vec<ControllerAssignment>,
    }

    fn digest(label: &str) -> Sha256Digest {
        Sha256Digest::of_bytes(label.as_bytes())
    }

    fn git_id(character: char) -> String {
        std::iter::repeat_n(character, 64).collect()
    }

    fn test_case(id: impl Into<String>) -> TestCase {
        TestCase {
            id: id.into(),
            dependencies: Vec::new(),
            fixture_setup: Vec::new(),
            fixture_required: Vec::new(),
            fixture_cleanup: Vec::new(),
            run_serial: false,
            resource_locks: Vec::new(),
            required_capabilities: vec!["macos-arm64".to_owned()],
        }
    }

    fn numbered_inventory(count: usize) -> TestInventory {
        TestInventory::new(
            (0..count)
                .map(|index| test_case(format!("test-{index:05}")))
                .collect(),
        )
        .expect("inventory")
    }

    fn trust(class: ArtifactTrustClass) -> TrustIdentity {
        let untrusted = class == ArtifactTrustClass::UntrustedContributor;
        TrustIdentity {
            producer_identity_sha256: digest("producer"),
            image_sha256: digest("image"),
            policy_sha256: digest("policy"),
            artifact_class: class,
            execution_boundary: if untrusted {
                ExecutionBoundary::DisposableGuest
            } else {
                ExecutionBoundary::TrustedHost
            },
            network_enabled: false,
            writable_host_mounts: false,
        }
    }

    fn worker(host_id: impl Into<String>) -> AuthenticatedWorker {
        let host_id = host_id.into();
        AuthenticatedWorker {
            identity_sha256: digest(&format!("identity-{host_id}")),
            host_id,
            capabilities: vec!["macos-arm64".to_owned()],
            session_generation: 1,
        }
    }

    fn fixture(test_count: usize, shard_count: usize, class: ArtifactTrustClass) -> Fixture {
        let inventory = numbered_inventory(test_count);
        let plan = ShardPlan::deterministic_balanced(&inventory, shard_count).expect("plan");
        let tree = git_id('b');
        let build = BuildIdentity {
            contract_sha256: digest("build-contract"),
            toolchain_sha256: digest("toolchain"),
            target_triple: "aarch64-apple-darwin".to_owned(),
            profile: "release".to_owned(),
        };
        let artifact = ArtifactIdentity {
            source_tree_sha: tree.clone(),
            build_contract_sha256: build.contract_sha256.clone(),
            payload_sha256: digest("artifact"),
            layout_sha256: digest("artifact-layout"),
            size_bytes: 42,
        };
        let manifest = ParallelProofManifest::new(
            SourceIdentity {
                repository_id: 7,
                repository: "generous-corp/pulp".to_owned(),
                subject: ProofSubject::PullRequest { number: 7705 },
                head_sha: git_id('a'),
                tree_sha: tree,
            },
            build,
            artifact,
            trust(class),
            &inventory,
            &plan,
        )
        .expect("manifest");
        let key = ControllerKey::new("controller-2026-08", &[0x5a; 32]).expect("key");
        let proof_context =
            ParallelProofContext::new(&manifest, &inventory, &plan).expect("proof context");
        let assignments = plan
            .shards
            .iter()
            .map(|shard| {
                ControllerAssignment::mint(
                    &key,
                    proof_context,
                    shard.id,
                    1,
                    u64::from(shard.id) + 1,
                    &worker(format!("host-{}", shard.id)),
                )
                .expect("assignment")
            })
            .collect();
        Fixture {
            inventory,
            plan,
            manifest,
            key,
            assignments,
        }
    }

    fn proof(fixture: &Fixture) -> ParallelProofContext<'_> {
        ParallelProofContext::new(&fixture.manifest, &fixture.inventory, &fixture.plan)
            .expect("proof context")
    }

    fn worker_for_assignment(assignment: &ControllerAssignment) -> AuthenticatedWorker {
        AuthenticatedWorker {
            host_id: assignment.claims.host_id.clone(),
            identity_sha256: assignment.claims.worker_identity_sha256.clone(),
            capabilities: assignment.claims.capabilities.clone(),
            session_generation: assignment.claims.session_generation,
        }
    }

    fn report_for(
        fixture: &Fixture,
        assignment: &ControllerAssignment,
        status: TestOutcomeStatus,
    ) -> WorkerReport {
        let shard = fixture
            .plan
            .shard(assignment.claims.shard_id)
            .expect("shard");
        WorkerReport {
            schema_version: PARALLEL_PROOF_SCHEMA_VERSION,
            assignment_digest: assignment.digest().expect("assignment digest"),
            observed_artifact_sha256: fixture.manifest.artifact.payload_sha256.clone(),
            observed_build_contract_sha256: fixture.manifest.build.contract_sha256.clone(),
            observed_head_sha: fixture.manifest.source.head_sha.clone(),
            observed_tree_sha: fixture.manifest.source.tree_sha.clone(),
            outcomes: shard
                .test_ids
                .iter()
                .map(|test_id| TestOutcome {
                    test_id: test_id.clone(),
                    status,
                    duration_ms: 3,
                })
                .collect(),
            log_sha256: digest(&format!(
                "log-{}-{}",
                assignment.claims.shard_id, assignment.claims.attempt
            )),
        }
    }

    fn observation_for(
        fixture: &Fixture,
        started_at_ms: u64,
        completed_at_ms: u64,
    ) -> ControllerExecutionObservation {
        ControllerExecutionObservation {
            started_at_ms,
            completed_at_ms,
            artifact_verified: true,
            execution_boundary: fixture.manifest.trust.execution_boundary,
            guest_teardown_confirmed: fixture.manifest.trust.execution_boundary
                == ExecutionBoundary::DisposableGuest,
        }
    }

    fn receipt_for(
        fixture: &Fixture,
        assignment: &ControllerAssignment,
        started_at_ms: u64,
        completed_at_ms: u64,
        status: TestOutcomeStatus,
    ) -> ShardReceipt {
        accept_worker_report(
            &fixture.key,
            proof(fixture),
            assignment,
            &worker_for_assignment(assignment),
            &observation_for(fixture, started_at_ms, completed_at_ms),
            report_for(fixture, assignment, status),
        )
        .expect("receipt")
    }

    fn sequential_receipts(fixture: &Fixture) -> Vec<ShardReceipt> {
        fixture
            .assignments
            .iter()
            .map(|assignment| {
                let start = u64::from(assignment.claims.shard_id) * 100 + 1;
                receipt_for(
                    fixture,
                    assignment,
                    start,
                    start + 50,
                    TestOutcomeStatus::Passed,
                )
            })
            .collect()
    }

    fn resign_receipt(key: &ControllerKey, receipt: &mut ShardReceipt) {
        receipt.acceptance_authentication =
            receipt_authentication(key, receipt).expect("receipt authentication");
    }

    fn disposition_for_receipt(
        key: &ControllerKey,
        assignment: &ControllerAssignment,
        receipt: &ShardReceipt,
    ) -> AttemptDisposition {
        let disposition = AttemptDisposition::executed(
            key,
            assignment,
            &ControllerExecutionObservation {
                started_at_ms: receipt.started_at_ms,
                completed_at_ms: receipt.completed_at_ms,
                artifact_verified: receipt.artifact_verified,
                execution_boundary: receipt.execution_boundary,
                guest_teardown_confirmed: receipt.guest_teardown_confirmed,
            },
        )
        .expect("attempt disposition");
        assert_eq!(
            disposition.digest().expect("disposition digest"),
            receipt.disposition_digest
        );
        disposition
    }

    fn aggregate_fixture(
        fixture: &Fixture,
        assignments: &[ControllerAssignment],
        receipts: &[ShardReceipt],
    ) -> Result<ShadowAggregate, ParallelProofError> {
        let dispositions = receipts
            .iter()
            .filter_map(|receipt| {
                assignments
                    .iter()
                    .find(|assignment| {
                        assignment.claims.shard_id == receipt.shard_id
                            && assignment.claims.attempt == receipt.attempt
                    })
                    .map(|assignment| disposition_for_receipt(&fixture.key, assignment, receipt))
            })
            .collect::<Vec<_>>();
        aggregate_shadow_proof(
            &fixture.key,
            proof(fixture),
            assignments,
            &dispositions,
            receipts,
        )
    }

    #[test]
    fn partitions_20_702_tests_exhaustively_disjointly_and_deterministically() {
        let inventory = numbered_inventory(20_702);
        let first = ShardPlan::deterministic_balanced(&inventory, 6).expect("first plan");
        let second = ShardPlan::deterministic_balanced(&inventory, 6).expect("second plan");
        assert_eq!(first, second);
        assert_eq!(first.total_tests, 20_702);
        assert_eq!(first.shards.len(), 6);
        let memberships = first
            .shards
            .iter()
            .flat_map(|shard| shard.test_ids.iter())
            .collect::<BTreeSet<_>>();
        assert_eq!(memberships.len(), 20_702);
        assert_eq!(
            first.digest(&inventory).expect("first digest"),
            second.digest(&inventory).expect("second digest")
        );
        let sizes = first
            .shards
            .iter()
            .map(|shard| shard.test_ids.len())
            .collect::<Vec<_>>();
        assert!(sizes.iter().max().expect("max") - sizes.iter().min().expect("min") <= 1);
    }

    #[test]
    fn relation_limit_precedes_canonicalization() {
        let mut oversized = test_case("oversized");
        oversized.dependencies = vec![String::new(); MAX_RELATIONS + 1];
        assert!(matches!(
            TestInventory::new(vec![oversized]),
            Err(ParallelProofError::LimitExceeded {
                field: "test relations",
                ..
            })
        ));
    }

    #[test]
    fn inventory_byte_limit_precedes_canonicalization() {
        let mut oversized = test_case("oversized");
        oversized.dependencies =
            vec!["x".repeat(MAX_IDENTIFIER_BYTES); (MAX_PAYLOAD_BYTES / MAX_IDENTIFIER_BYTES) + 1];
        assert!(matches!(
            TestInventory::new(vec![oversized]),
            Err(ParallelProofError::LimitExceeded {
                field: "inventory input bytes",
                ..
            })
        ));
    }

    #[test]
    fn shard_membership_limit_precedes_canonicalization() {
        let inventory = numbered_inventory(1);
        let oversized = vec![vec![String::new(); MAX_TESTS + 1]];
        assert!(matches!(
            ShardPlan::from_assignments(&inventory, oversized),
            Err(ParallelProofError::LimitExceeded {
                field: "shard test memberships",
                ..
            })
        ));
    }

    #[test]
    fn plan_rejects_missing_duplicate_unknown_and_digest_tampering() {
        let inventory = numbered_inventory(4);
        let valid = ShardPlan::from_assignments(
            &inventory,
            vec![
                vec!["test-00000".to_owned(), "test-00001".to_owned()],
                vec!["test-00002".to_owned(), "test-00003".to_owned()],
            ],
        )
        .expect("valid plan");

        let mut missing = valid.clone();
        missing.shards[1].test_ids.pop();
        assert!(matches!(
            missing.validate_against(&inventory),
            Err(ParallelProofError::InvalidPartition(_))
        ));

        let mut duplicate = valid.clone();
        duplicate.shards[1].test_ids[0] = "test-00001".to_owned();
        assert!(matches!(
            duplicate.validate_against(&inventory),
            Err(ParallelProofError::InvalidPartition(_))
        ));

        let mut unknown = valid.clone();
        unknown.shards[0].test_ids[0] = "not-declared".to_owned();
        assert!(matches!(
            unknown.validate_against(&inventory),
            Err(ParallelProofError::UnknownTest(_))
        ));

        let mut tampered = valid;
        tampered.inventory_digest = digest("wrong inventory");
        assert!(matches!(
            tampered.validate_against(&inventory),
            Err(ParallelProofError::BindingMismatch("inventory digest"))
        ));
    }

    #[test]
    fn dependency_and_fixture_edges_must_not_cross_shards() {
        let mut setup = test_case("setup");
        setup.fixture_setup = vec!["database".to_owned()];
        let mut consumer = test_case("consumer");
        consumer.dependencies = vec!["setup".to_owned()];
        consumer.fixture_required = vec!["database".to_owned()];
        let mut cleanup = test_case("cleanup");
        cleanup.fixture_cleanup = vec!["database".to_owned()];
        let inventory = TestInventory::new(vec![setup, consumer, cleanup]).expect("inventory");
        assert!(matches!(
            ShardPlan::from_assignments(
                &inventory,
                vec![
                    vec!["setup".to_owned()],
                    vec!["cleanup".to_owned(), "consumer".to_owned()]
                ]
            ),
            Err(ParallelProofError::TopologyViolation(_))
        ));
        let plan = ShardPlan::deterministic_balanced(&inventory, 1).expect("co-located plan");
        assert_eq!(plan.shards[0].test_ids.len(), 3);
    }

    #[test]
    fn dependency_cycles_and_unresolved_fixtures_fail_closed() {
        let mut left = test_case("left");
        left.dependencies = vec!["right".to_owned()];
        let mut right = test_case("right");
        right.dependencies = vec!["left".to_owned()];
        assert!(matches!(
            TestInventory::new(vec![left, right]),
            Err(ParallelProofError::TopologyViolation(_))
        ));

        let mut orphan = test_case("orphan");
        orphan.fixture_required = vec!["missing".to_owned()];
        assert!(matches!(
            TestInventory::new(vec![orphan]),
            Err(ParallelProofError::TopologyViolation(_))
        ));
    }

    #[test]
    fn run_serial_is_a_single_test_fleet_exclusive_shard() {
        let mut serial = test_case("serial");
        serial.run_serial = true;
        let inventory = TestInventory::new(vec![test_case("parallel"), serial]).expect("inventory");
        let plan = ShardPlan::deterministic_balanced(&inventory, 2).expect("plan");
        let serial_shard = plan
            .shards
            .iter()
            .find(|shard| shard.test_ids == ["serial"])
            .expect("serial shard");
        assert_eq!(
            serial_shard.execution_mode,
            ShardExecutionMode::FleetExclusive
        );
        assert!(matches!(
            ShardPlan::from_assignments(
                &inventory,
                vec![vec!["parallel".to_owned(), "serial".to_owned()]]
            ),
            Err(ParallelProofError::TopologyViolation(_))
        ));
    }

    #[test]
    fn resource_lock_scope_must_be_consistent() {
        let mut left = test_case("left");
        left.resource_locks.push(ResourceLock {
            name: "device".to_owned(),
            scope: ResourceLockScope::Host,
        });
        let mut right = test_case("right");
        right.resource_locks.push(ResourceLock {
            name: "device".to_owned(),
            scope: ResourceLockScope::Fleet,
        });
        assert!(matches!(
            TestInventory::new(vec![left, right]),
            Err(ParallelProofError::TopologyViolation(_))
        ));
    }

    #[test]
    fn manifest_binds_every_identity_and_remains_shadow_only() {
        let fixture = fixture(2, 2, ArtifactTrustClass::TrustedController);
        assert!(!fixture.manifest.satisfies_merge_readiness());
        let mut malformed_repository = fixture.manifest.source.clone();
        malformed_repository.repository = "generous corp/pulp".to_owned();
        assert!(matches!(
            validate_source(&malformed_repository),
            Err(ParallelProofError::InvalidField("repository"))
        ));
        let mut mixed_object_format = fixture.manifest.source.clone();
        mixed_object_format.tree_sha = "b".repeat(40);
        assert!(matches!(
            validate_source(&mixed_object_format),
            Err(ParallelProofError::InvalidField("git object format"))
        ));
        let mut tampered = fixture.manifest.clone();
        tampered.artifact.payload_sha256 = digest("substitution");
        assert_ne!(
            fixture
                .manifest
                .digest(&fixture.inventory, &fixture.plan)
                .expect("original"),
            tampered
                .digest(&fixture.inventory, &fixture.plan)
                .expect("tampered")
        );
        tampered.shadow_only = false;
        assert!(matches!(
            tampered.validate(&fixture.inventory, &fixture.plan),
            Err(ParallelProofError::InvalidField("shadow_only"))
        ));
    }

    #[test]
    fn untrusted_artifacts_require_an_isolated_disposable_guest() {
        let fixture = fixture(1, 1, ArtifactTrustClass::UntrustedContributor);
        let receipt = receipt_for(
            &fixture,
            &fixture.assignments[0],
            1,
            2,
            TestOutcomeStatus::Passed,
        );
        assert_eq!(receipt.status, ShardReceiptStatus::Passed);

        let mut unsafe_trust = fixture.manifest.trust.clone();
        unsafe_trust.execution_boundary = ExecutionBoundary::TrustedHost;
        assert!(matches!(
            validate_trust(&unsafe_trust),
            Err(ParallelProofError::InvalidField(
                "untrusted artifact boundary"
            ))
        ));

        let mut observation = observation_for(&fixture, 1, 2);
        observation.guest_teardown_confirmed = false;
        assert!(matches!(
            accept_worker_report(
                &fixture.key,
                proof(&fixture),
                &fixture.assignments[0],
                &worker_for_assignment(&fixture.assignments[0]),
                &observation,
                report_for(&fixture, &fixture.assignments[0], TestOutcomeStatus::Passed),
            ),
            Err(ParallelProofError::InvalidField("guest teardown"))
        ));
    }

    #[test]
    fn assignment_authentication_and_worker_capabilities_fail_closed() {
        let fixture = fixture(1, 1, ArtifactTrustClass::TrustedController);
        let mut tampered = fixture.assignments[0].clone();
        tampered.claims.host_id = "attacker".to_owned();
        assert!(matches!(
            tampered.verify(&fixture.key),
            Err(ParallelProofError::AuthenticationFailed)
        ));

        let incapable_worker = AuthenticatedWorker {
            host_id: "incapable".to_owned(),
            identity_sha256: digest("incapable"),
            capabilities: vec!["linux".to_owned()],
            session_generation: 1,
        };
        assert!(matches!(
            ControllerAssignment::mint(&fixture.key, proof(&fixture), 0, 1, 50, &incapable_worker),
            Err(ParallelProofError::BindingMismatch("worker capabilities"))
        ));

        let oversized_worker = AuthenticatedWorker {
            host_id: "too-many-capabilities".to_owned(),
            identity_sha256: digest("oversized worker"),
            capabilities: (0..=MAX_CAPABILITIES)
                .map(|index| format!("capability-{index:03}"))
                .collect(),
            session_generation: 1,
        };
        assert!(matches!(
            ControllerAssignment::mint(&fixture.key, proof(&fixture), 0, 1, 51, &oversized_worker),
            Err(ParallelProofError::LimitExceeded {
                field: "worker capabilities",
                ..
            })
        ));

        assert!(matches!(
            ControllerAssignment::mint(
                &fixture.key,
                proof(&fixture),
                0,
                u32::try_from(MAX_ATTEMPTS_PER_SHARD + 1).expect("attempt limit"),
                52,
                &worker("over-retried")
            ),
            Err(ParallelProofError::LimitExceeded {
                field: "assignment attempt",
                ..
            })
        ));
    }

    #[test]
    fn worker_report_rejects_artifact_source_session_and_test_set_tampering() {
        let fixture = fixture(2, 1, ArtifactTrustClass::TrustedController);
        let assignment = &fixture.assignments[0];
        let authenticated = worker_for_assignment(assignment);
        let observation = observation_for(&fixture, 1, 2);
        let mut report = report_for(&fixture, assignment, TestOutcomeStatus::Passed);
        report.observed_artifact_sha256 = digest("wrong");
        assert!(matches!(
            accept_worker_report(
                &fixture.key,
                proof(&fixture),
                assignment,
                &authenticated,
                &observation,
                report
            ),
            Err(ParallelProofError::BindingMismatch("observed artifact"))
        ));

        let mut report = report_for(&fixture, assignment, TestOutcomeStatus::Passed);
        report.observed_head_sha = git_id('c');
        assert!(matches!(
            accept_worker_report(
                &fixture.key,
                proof(&fixture),
                assignment,
                &authenticated,
                &observation,
                report
            ),
            Err(ParallelProofError::BindingMismatch("observed head"))
        ));

        let mut stale_worker = authenticated.clone();
        stale_worker.session_generation += 1;
        assert!(matches!(
            accept_worker_report(
                &fixture.key,
                proof(&fixture),
                assignment,
                &stale_worker,
                &observation,
                report_for(&fixture, assignment, TestOutcomeStatus::Passed)
            ),
            Err(ParallelProofError::AuthenticationFailed)
        ));

        let mut report = report_for(&fixture, assignment, TestOutcomeStatus::Passed);
        report.outcomes.pop();
        assert!(matches!(
            accept_worker_report(
                &fixture.key,
                proof(&fixture),
                assignment,
                &authenticated,
                &observation,
                report
            ),
            Err(ParallelProofError::BindingMismatch("executed test set"))
        ));
    }

    #[test]
    fn worker_report_json_is_strict_and_bounded() {
        let fixture = fixture(1, 1, ArtifactTrustClass::TrustedController);
        let report = report_for(&fixture, &fixture.assignments[0], TestOutcomeStatus::Passed);
        let mut value = serde_json::to_value(report).expect("json");
        value
            .as_object_mut()
            .expect("object")
            .insert("unexpected".to_owned(), serde_json::json!(true));
        assert!(matches!(
            decode_worker_report(&serde_json::to_vec(&value).expect("bytes")),
            Err(ParallelProofError::Json(_))
        ));
        assert!(matches!(
            decode_worker_report(&vec![b'x'; MAX_RECORD_BYTES + 1]),
            Err(ParallelProofError::LimitExceeded {
                field: "worker report bytes",
                ..
            })
        ));
    }

    #[test]
    fn aggregation_is_order_independent_and_only_complete_passes() {
        let fixture = fixture(8, 3, ArtifactTrustClass::TrustedController);
        let receipts = sequential_receipts(&fixture);
        let forward = aggregate_fixture(&fixture, &fixture.assignments, &receipts)
            .expect("forward aggregate");
        assert_eq!(forward.status, ShadowProofStatus::Passed);
        assert!(!forward.satisfies_merge_readiness());

        let mut reversed_assignments = fixture.assignments.clone();
        reversed_assignments.reverse();
        let mut reversed_receipts = receipts.clone();
        reversed_receipts.reverse();
        let reversed = aggregate_fixture(&fixture, &reversed_assignments, &reversed_receipts)
            .expect("reversed aggregate");
        assert_eq!(forward, reversed);
        assert_eq!(
            forward.digest().expect("forward digest"),
            reversed.digest().expect("reversed digest")
        );

        let incomplete = aggregate_fixture(&fixture, &fixture.assignments, &receipts[..2])
            .expect("incomplete aggregate");
        assert_eq!(
            incomplete.status,
            ShadowProofStatus::Incomplete {
                missing_shards: vec![2]
            }
        );

        let mut failed = receipts;
        failed[1] = receipt_for(
            &fixture,
            &fixture.assignments[1],
            101,
            151,
            TestOutcomeStatus::Failed,
        );
        assert_eq!(
            aggregate_fixture(&fixture, &fixture.assignments, &failed)
                .expect("failed aggregate")
                .status,
            ShadowProofStatus::Failed {
                failed_shards: vec![1]
            }
        );
        assert_eq!(
            aggregate_fixture(&fixture, &fixture.assignments, &failed[..2])
                .expect("unknown execution interval and failure are both retained")
                .status,
            ShadowProofStatus::IncompleteAndFailed {
                missing_shards: vec![2],
                failed_shards: vec![1]
            }
        );
    }

    #[test]
    fn retry_ordering_fences_stale_success_and_rejects_attempt_gaps() {
        let fixture = fixture(1, 1, ArtifactTrustClass::TrustedController);
        let first = fixture.assignments[0].clone();
        let second = ControllerAssignment::mint(
            &fixture.key,
            proof(&fixture),
            0,
            2,
            99,
            &worker("retry-host"),
        )
        .expect("retry assignment");
        let stale_receipt = receipt_for(&fixture, &first, 1, 2, TestOutcomeStatus::Passed);
        let incomplete = aggregate_fixture(
            &fixture,
            &[first.clone(), second.clone()],
            std::slice::from_ref(&stale_receipt),
        )
        .expect("stale receipt ignored");
        assert_eq!(
            incomplete.status,
            ShadowProofStatus::Incomplete {
                missing_shards: vec![0]
            }
        );
        let active_receipt = receipt_for(&fixture, &second, 3, 4, TestOutcomeStatus::Passed);
        assert_eq!(
            aggregate_fixture(
                &fixture,
                &[first, second.clone()],
                &[stale_receipt, active_receipt]
            )
            .expect("active retry passes")
            .status,
            ShadowProofStatus::Passed
        );
        assert!(matches!(
            aggregate_fixture(&fixture, &[second], &[]),
            Err(ParallelProofError::InvalidAttemptSequence(_))
        ));
    }

    #[test]
    fn retry_attempts_for_one_shard_must_not_overlap() {
        let fixture = fixture(1, 1, ArtifactTrustClass::TrustedController);
        let first = fixture.assignments[0].clone();
        let second = ControllerAssignment::mint(
            &fixture.key,
            proof(&fixture),
            0,
            2,
            99,
            &worker("retry-host"),
        )
        .expect("retry assignment");
        let receipts = [
            receipt_for(&fixture, &first, 1, 10, TestOutcomeStatus::Passed),
            receipt_for(&fixture, &second, 5, 15, TestOutcomeStatus::Passed),
        ];
        assert!(matches!(
            aggregate_fixture(&fixture, &[first, second], &receipts),
            Err(ParallelProofError::TopologyViolation(_))
        ));
    }

    #[test]
    fn aggregate_attempt_record_limit_is_collective() {
        let fixture = fixture(1, 1, ArtifactTrustClass::TrustedController);
        let assignment = fixture.assignments[0].clone();
        let disposition = AttemptDisposition::fenced_before_start(&fixture.key, &assignment)
            .expect("fenced disposition");
        let count_per_kind = (MAX_ATTEMPT_RECORDS / 2) + 1;
        let assignments = vec![assignment; count_per_kind];
        let dispositions = vec![disposition; count_per_kind];

        assert!(matches!(
            aggregate_shadow_proof(
                &fixture.key,
                proof(&fixture),
                &assignments,
                &dispositions,
                &[]
            ),
            Err(ParallelProofError::LimitExceeded {
                field: "aggregate attempt records",
                ..
            })
        ));
    }

    #[test]
    fn mixed_unknown_and_conflicting_receipts_are_rejected() {
        let fixture = fixture(2, 1, ArtifactTrustClass::TrustedController);
        let receipt = receipt_for(
            &fixture,
            &fixture.assignments[0],
            1,
            2,
            TestOutcomeStatus::Passed,
        );
        assert!(matches!(
            aggregate_shadow_proof(
                &fixture.key,
                proof(&fixture),
                &fixture.assignments,
                &[],
                std::slice::from_ref(&receipt)
            ),
            Err(ParallelProofError::MissingRecord(_))
        ));
        let mut unauthorized = receipt.clone();
        unauthorized.attempt = 2;
        unauthorized.fence = 2;
        resign_receipt(&fixture.key, &mut unauthorized);
        assert!(matches!(
            aggregate_fixture(&fixture, &fixture.assignments, &[unauthorized]),
            Err(ParallelProofError::ImmutableConflict(_))
        ));

        let mut conflicting = receipt.clone();
        conflicting.log_sha256 = digest("different log");
        resign_receipt(&fixture.key, &mut conflicting);
        assert!(matches!(
            aggregate_fixture(&fixture, &fixture.assignments, &[receipt, conflicting]),
            Err(ParallelProofError::ImmutableConflict(_))
        ));
    }

    #[test]
    fn synthesized_receipts_cannot_bypass_controller_acceptance() {
        let fixture = fixture(1, 1, ArtifactTrustClass::TrustedController);
        let receipt = receipt_for(
            &fixture,
            &fixture.assignments[0],
            1,
            2,
            TestOutcomeStatus::Passed,
        );
        let mut forged = receipt;
        forged.log_sha256 = digest("worker-forged-pass");
        assert!(matches!(
            aggregate_fixture(&fixture, &fixture.assignments, &[forged]),
            Err(ParallelProofError::AuthenticationFailed)
        ));
    }

    fn lock_fixture(scope: ResourceLockScope) -> Fixture {
        let mut left = test_case("left");
        left.resource_locks.push(ResourceLock {
            name: "shared-device".to_owned(),
            scope,
        });
        let mut right = test_case("right");
        right.resource_locks.push(ResourceLock {
            name: "shared-device".to_owned(),
            scope,
        });
        let inventory = TestInventory::new(vec![left, right]).expect("lock inventory");
        let plan = ShardPlan::deterministic_balanced(&inventory, 2).expect("lock plan");
        let base = fixture(2, 2, ArtifactTrustClass::TrustedController);
        let manifest = ParallelProofManifest::new(
            base.manifest.source,
            base.manifest.build,
            base.manifest.artifact,
            base.manifest.trust,
            &inventory,
            &plan,
        )
        .expect("lock manifest");
        let key = base.key;
        let proof_context =
            ParallelProofContext::new(&manifest, &inventory, &plan).expect("proof context");
        let assignments = plan
            .shards
            .iter()
            .map(|shard| {
                ControllerAssignment::mint(
                    &key,
                    proof_context,
                    shard.id,
                    1,
                    u64::from(shard.id) + 1,
                    &worker(format!("host-{}", shard.id)),
                )
                .expect("lock assignment")
            })
            .collect();
        Fixture {
            inventory,
            plan,
            manifest,
            key,
            assignments,
        }
    }

    #[test]
    fn stale_attempts_require_terminal_dispositions_and_enforce_their_intervals() {
        let fixture = lock_fixture(ResourceLockScope::Fleet);
        let retry = ControllerAssignment::mint(
            &fixture.key,
            proof(&fixture),
            0,
            2,
            99,
            &worker("retry-host"),
        )
        .expect("retry assignment");
        let mut assignments = fixture.assignments.clone();
        assignments.push(retry.clone());
        let receipts = vec![
            receipt_for(&fixture, &retry, 20, 30, TestOutcomeStatus::Passed),
            receipt_for(
                &fixture,
                &fixture.assignments[1],
                1,
                10,
                TestOutcomeStatus::Passed,
            ),
        ];
        let active_dispositions = vec![
            disposition_for_receipt(&fixture.key, &retry, &receipts[0]),
            disposition_for_receipt(&fixture.key, &fixture.assignments[1], &receipts[1]),
        ];
        let stale_executed = AttemptDisposition::executed(
            &fixture.key,
            &fixture.assignments[0],
            &observation_for(&fixture, 1, 10),
        )
        .expect("stale execution disposition");
        let mut overlapping_dispositions = active_dispositions.clone();
        overlapping_dispositions.push(stale_executed);
        assert!(matches!(
            aggregate_shadow_proof(
                &fixture.key,
                proof(&fixture),
                &assignments,
                &overlapping_dispositions,
                &receipts
            ),
            Err(ParallelProofError::TopologyViolation(_))
        ));

        assert_eq!(
            aggregate_shadow_proof(
                &fixture.key,
                proof(&fixture),
                &assignments,
                &active_dispositions,
                &receipts
            )
            .expect("unclosed stale attempt is incomplete")
            .status,
            ShadowProofStatus::Incomplete {
                missing_shards: vec![0]
            }
        );

        let mut fenced_dispositions = active_dispositions;
        fenced_dispositions.push(
            AttemptDisposition::fenced_before_start(&fixture.key, &fixture.assignments[0])
                .expect("fenced stale assignment"),
        );
        assert_eq!(
            aggregate_shadow_proof(
                &fixture.key,
                proof(&fixture),
                &assignments,
                &fenced_dispositions,
                &receipts
            )
            .expect("fenced stale attempt permits pass")
            .status,
            ShadowProofStatus::Passed
        );
    }

    #[test]
    fn resource_and_run_serial_overlaps_fail_closed() {
        let fleet = lock_fixture(ResourceLockScope::Fleet);
        let fleet_receipts = fleet
            .assignments
            .iter()
            .map(|assignment| receipt_for(&fleet, assignment, 1, 10, TestOutcomeStatus::Passed))
            .collect::<Vec<_>>();
        assert!(matches!(
            aggregate_fixture(&fleet, &fleet.assignments, &fleet_receipts),
            Err(ParallelProofError::TopologyViolation(_))
        ));

        let host = lock_fixture(ResourceLockScope::Host);
        let host_receipts = host
            .assignments
            .iter()
            .map(|assignment| receipt_for(&host, assignment, 1, 10, TestOutcomeStatus::Passed))
            .collect::<Vec<_>>();
        assert_eq!(
            aggregate_fixture(&host, &host.assignments, &host_receipts)
                .expect("different hosts may overlap host lock")
                .status,
            ShadowProofStatus::Passed
        );

        let mut serial = test_case("serial");
        serial.run_serial = true;
        let inventory = TestInventory::new(vec![serial, test_case("parallel")]).expect("inventory");
        let plan = ShardPlan::deterministic_balanced(&inventory, 2).expect("plan");
        let base = fixture(2, 2, ArtifactTrustClass::TrustedController);
        let manifest = ParallelProofManifest::new(
            base.manifest.source,
            base.manifest.build,
            base.manifest.artifact,
            base.manifest.trust,
            &inventory,
            &plan,
        )
        .expect("manifest");
        let key = base.key;
        let proof_context =
            ParallelProofContext::new(&manifest, &inventory, &plan).expect("proof context");
        let assignments = plan
            .shards
            .iter()
            .map(|shard| {
                ControllerAssignment::mint(
                    &key,
                    proof_context,
                    shard.id,
                    1,
                    u64::from(shard.id) + 1,
                    &worker(format!("host-{}", shard.id)),
                )
                .expect("assignment")
            })
            .collect::<Vec<_>>();
        let serial_fixture = Fixture {
            inventory,
            plan,
            manifest,
            key,
            assignments,
        };
        let receipts = serial_fixture
            .assignments
            .iter()
            .map(|assignment| {
                receipt_for(
                    &serial_fixture,
                    assignment,
                    1,
                    10,
                    TestOutcomeStatus::Passed,
                )
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            aggregate_fixture(&serial_fixture, &serial_fixture.assignments, &receipts),
            Err(ParallelProofError::TopologyViolation(_))
        ));
    }

    #[test]
    fn overlap_sweep_tracks_furthest_interval_and_scales_to_the_corpus_bound() {
        let multi_shard = fixture(3, 3, ArtifactTrustClass::TrustedController);
        let nested = vec![
            ObservedExecution {
                shard_id: 0,
                execution_mode: ShardExecutionMode::Parallel,
                host_id: "host-0".to_owned(),
                started_at_ms: 0,
                completed_at_ms: 100,
            },
            ObservedExecution {
                shard_id: 1,
                execution_mode: ShardExecutionMode::Parallel,
                host_id: "host-1".to_owned(),
                started_at_ms: 10,
                completed_at_ms: 20,
            },
            ObservedExecution {
                shard_id: 2,
                execution_mode: ShardExecutionMode::FleetExclusive,
                host_id: "host-2".to_owned(),
                started_at_ms: 30,
                completed_at_ms: 40,
            },
        ];
        assert!(matches!(
            validate_execution_overlap(&multi_shard.inventory, &multi_shard.plan, &nested),
            Err(ParallelProofError::TopologyViolation(_))
        ));

        let single_shard = fixture(1, 1, ArtifactTrustClass::TrustedController);
        let maximum_executions = MAX_ATTEMPT_RECORDS / 2;
        let sequential = (0..maximum_executions)
            .map(|index| ObservedExecution {
                shard_id: 0,
                execution_mode: ShardExecutionMode::Parallel,
                host_id: "host-0".to_owned(),
                started_at_ms: (index as u64) * 2,
                completed_at_ms: (index as u64) * 2 + 1,
            })
            .collect::<Vec<_>>();
        validate_execution_overlap(&single_shard.inventory, &single_shard.plan, &sequential)
            .expect("maximum feasible execution corpus remains non-overlapping");
    }

    #[test]
    fn immutable_store_is_idempotent_and_rejects_conflicts() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let store = ParallelProofStore::open(temporary.path()).expect("store");
        let fixture = fixture(2, 1, ArtifactTrustClass::TrustedController);
        let receipt = receipt_for(
            &fixture,
            &fixture.assignments[0],
            1,
            2,
            TestOutcomeStatus::Passed,
        );
        let disposition = disposition_for_receipt(&fixture.key, &fixture.assignments[0], &receipt);
        assert_eq!(
            store
                .record_disposition(&fixture.key, &fixture.assignments[0], &disposition)
                .expect("disposition created"),
            StoreWriteOutcome::Created
        );
        assert_eq!(
            store
                .load_disposition(
                    &fixture.key,
                    &fixture.assignments[0],
                    &disposition.logical_key()
                )
                .expect("disposition loaded"),
            disposition
        );
        assert_eq!(
            store
                .record_receipt(&fixture.key, &receipt)
                .expect("created"),
            StoreWriteOutcome::Created
        );
        assert_eq!(
            store
                .record_receipt(&fixture.key, &receipt)
                .expect("idempotent"),
            StoreWriteOutcome::AlreadyPresent
        );
        assert_eq!(
            store
                .load_receipt(&fixture.key, &receipt.logical_key())
                .expect("loaded"),
            receipt
        );
        let mut conflict = receipt.clone();
        conflict.log_sha256 = digest("conflicting immutable log");
        resign_receipt(&fixture.key, &mut conflict);
        assert!(matches!(
            store.record_receipt(&fixture.key, &conflict),
            Err(ParallelProofError::ImmutableConflict(_))
        ));
    }

    #[test]
    fn store_creates_only_one_root_level_under_an_existing_parent() {
        assert_eq!(
            store_root_parent(Path::new("proof-store")).expect("implicit current directory"),
            Path::new(".")
        );
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("proof-store");
        ParallelProofStore::open(&root).expect("create final root");
        assert!(root.is_dir());
        ParallelProofStore::open(&root).expect("reopen final root");

        let missing_parent_root = temporary.path().join("missing").join("nested-store");
        assert!(matches!(
            ParallelProofStore::open(&missing_parent_root),
            Err(ParallelProofError::InvalidField("store root parent"))
        ));
        assert!(!temporary.path().join("missing").exists());

        for invalid_root in [
            temporary.path().join("proofs").join(".."),
            PathBuf::from("./proof-store"),
        ] {
            assert!(matches!(
                ParallelProofStore::open(invalid_root),
                Err(ParallelProofError::InvalidField("store root"))
            ));
        }
    }

    #[test]
    fn concurrent_identical_and_conflicting_writes_never_overwrite() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(ParallelProofStore::open(temporary.path()).expect("store"));
        let fixture = fixture(2, 1, ArtifactTrustClass::TrustedController);
        let receipt = Arc::new(receipt_for(
            &fixture,
            &fixture.assignments[0],
            1,
            2,
            TestOutcomeStatus::Passed,
        ));
        let key = Arc::new(fixture.key.clone());
        let barrier = Arc::new(Barrier::new(8));
        let handles = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                let receipt = Arc::clone(&receipt);
                let key = Arc::clone(&key);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    store.record_receipt(&key, &receipt)
                })
            })
            .collect::<Vec<_>>();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread").expect("write"))
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == StoreWriteOutcome::Created)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == StoreWriteOutcome::AlreadyPresent)
                .count(),
            7
        );

        let second_root = tempfile::tempdir().expect("second tempdir");
        let store = Arc::new(ParallelProofStore::open(second_root.path()).expect("second store"));
        let mut conflict = (*receipt).clone();
        conflict.log_sha256 = digest("other bytes");
        resign_receipt(&key, &mut conflict);
        let variants = [(*receipt).clone(), conflict];
        let barrier = Arc::new(Barrier::new(2));
        let handles = variants
            .into_iter()
            .map(|variant| {
                let store = Arc::clone(&store);
                let key = Arc::clone(&key);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    store.record_receipt(&key, &variant)
                })
            })
            .collect::<Vec<_>>();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().expect("conflict thread"))
            .collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Err(ParallelProofError::ImmutableConflict(_))))
                .count(),
            1
        );
    }

    #[test]
    fn crash_points_leave_no_partial_record_and_restart_recovers() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let store = ParallelProofStore::open(temporary.path()).expect("store");
        let fixture = fixture(1, 1, ArtifactTrustClass::TrustedController);
        let receipt = receipt_for(
            &fixture,
            &fixture.assignments[0],
            1,
            2,
            TestOutcomeStatus::Passed,
        );
        let logical_key = receipt.logical_key();
        assert!(matches!(
            store.put(
                RecordKind::Receipt,
                &logical_key,
                &receipt,
                CrashPoint::AfterTempSync
            ),
            Err(ParallelProofError::CrashInjected("after_temp_sync"))
        ));
        assert!(matches!(
            store.load_receipt(&fixture.key, &logical_key),
            Err(ParallelProofError::MissingRecord(_))
        ));
        assert!(matches!(
            store.put(
                RecordKind::Receipt,
                &logical_key,
                &receipt,
                CrashPoint::AfterPublish
            ),
            Err(ParallelProofError::CrashInjected("after_publish"))
        ));
        let reopened = ParallelProofStore::open(temporary.path()).expect("reopen");
        assert_eq!(
            reopened
                .load_receipt(&fixture.key, &logical_key)
                .expect("recovered"),
            receipt
        );
        assert_eq!(
            reopened
                .record_receipt(&fixture.key, &receipt)
                .expect("sync retry"),
            StoreWriteOutcome::AlreadyPresent
        );
    }

    #[test]
    fn durable_record_tampering_is_detected() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let store = ParallelProofStore::open(temporary.path()).expect("store");
        let fixture = fixture(1, 1, ArtifactTrustClass::TrustedController);
        let receipt = receipt_for(
            &fixture,
            &fixture.assignments[0],
            1,
            2,
            TestOutcomeStatus::Passed,
        );
        store
            .record_receipt(&fixture.key, &receipt)
            .expect("record");
        let path = temporary.path().join("receipts").join(format!(
            "{}.json",
            store_file_stem(RecordKind::Receipt, &receipt.logical_key())
        ));
        let mut bytes = fs::read(&path).expect("read");
        let index = bytes.len() / 2;
        bytes[index] ^= 1;
        fs::write(&path, bytes).expect("tamper");
        assert!(matches!(
            store.load_receipt(&fixture.key, &receipt.logical_key()),
            Err(ParallelProofError::CorruptRecord(_))
        ));
    }

    #[test]
    fn durable_payload_identity_must_match_the_requested_key() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let store = ParallelProofStore::open(temporary.path()).expect("store");
        let fixture = fixture(1, 1, ArtifactTrustClass::TrustedController);
        let receipt = receipt_for(
            &fixture,
            &fixture.assignments[0],
            1,
            2,
            TestOutcomeStatus::Passed,
        );
        store
            .record_receipt(&fixture.key, &receipt)
            .expect("record");
        let original_path = temporary.path().join("receipts").join(format!(
            "{}.json",
            store_file_stem(RecordKind::Receipt, &receipt.logical_key())
        ));
        let mut envelope: StoredEnvelope =
            serde_json::from_slice(&fs::read(original_path).expect("read envelope"))
                .expect("decode envelope");
        let wrong_key = format!("{}:99:99", receipt.manifest_digest.as_str());
        envelope.logical_key.clone_from(&wrong_key);
        let wrong_path = temporary.path().join("receipts").join(format!(
            "{}.json",
            store_file_stem(RecordKind::Receipt, &wrong_key)
        ));
        fs::write(
            wrong_path,
            serde_json::to_vec(&envelope).expect("encode copied envelope"),
        )
        .expect("write copied envelope");

        assert!(matches!(
            store.load_receipt(&fixture.key, &wrong_key),
            Err(ParallelProofError::CorruptRecord(_))
        ));
    }
}
