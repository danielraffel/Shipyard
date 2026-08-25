//! Pure one-host build-once consumption proof for the first Pulp M3 canary.
//!
//! This module does not run CMake, CTest, transfer artifacts, dispatch work, or
//! publish merge evidence. It binds one successful configure/build observation
//! to the content-addressed artifact already present in a parallel-proof
//! manifest, then reconciles one controller-owned execution observation against
//! the exact canonical CTest inventory. The existing full gate remains the only
//! authoritative result. A matching pure assessment is neither an operational
//! canary nor evidence of a build speedup.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::parallel_proof::{
    ArtifactIdentity, BuildIdentity, MAX_RECORD_BYTES, ParallelProofContext, ParallelProofError,
    Sha256Digest, SourceIdentity,
};

const SCHEMA_VERSION: u32 = 1;
const PULP_REPOSITORY: &str = "generous-corp/pulp";
const PULP_REPOSITORY_ID: u64 = 1_203_111_607;
const PULP_TARGET: &str = "mac";
const M3_HOST: &str = "m3";

/// Machine-global opt-in for the bounded one-host shadow canary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OneHostM3Policy {
    /// Explicit opt-in. Repository content cannot turn this on by default.
    pub enabled: bool,
    /// Exact repository admitted by this canary.
    pub repository: String,
    /// Exact Shipyard target admitted by this canary.
    pub target: String,
    /// Exact host that both builds and consumes the artifact.
    pub host_id: String,
}

impl Default for OneHostM3Policy {
    fn default() -> Self {
        Self {
            enabled: false,
            repository: PULP_REPOSITORY.to_owned(),
            target: PULP_TARGET.to_owned(),
            host_id: M3_HOST.to_owned(),
        }
    }
}

/// Controller-owned receipt for exactly one successful configure and build.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureBuildReceipt {
    /// Receipt schema version.
    pub schema_version: u32,
    /// This receipt is permanently shadow-only.
    pub shadow_only: bool,
    /// Exact builder host.
    pub builder_host_id: String,
    /// Nonzero authenticated builder-session generation.
    pub host_session_generation: u64,
    /// Complete parallel-proof manifest digest.
    pub manifest_sha256: Sha256Digest,
    /// Exact source identity configured and built.
    pub source: SourceIdentity,
    /// Exact build/toolchain contract executed.
    pub build: BuildIdentity,
    /// Exact canonical `CTest` inventory configured into the build tree.
    pub inventory_sha256: Sha256Digest,
    /// Controller digest of the exact configure command and environment.
    pub configure_command_sha256: Sha256Digest,
    /// Controller digest of the exact build command and environment.
    pub build_command_sha256: Sha256Digest,
    /// Number of configure invocations in this receipt.
    pub configure_invocations: u32,
    /// Number of build invocations in this receipt.
    pub build_invocations: u32,
    /// Configure process exit code.
    pub configure_exit_code: i32,
    /// Build process exit code.
    pub build_exit_code: i32,
    /// Minimal content-addressed artifact produced by the build.
    pub artifact: ArtifactIdentity,
}

impl ConfigureBuildReceipt {
    /// Construct the exact successful receipt for one configure/build pair.
    pub fn successful(
        proof: ParallelProofContext<'_>,
        host_session_generation: u64,
        configure_command_sha256: Sha256Digest,
        build_command_sha256: Sha256Digest,
    ) -> Result<Self, ParallelProofError> {
        let proof = ParallelProofContext::new(proof.manifest, proof.inventory, proof.plan)?;
        let receipt = Self {
            schema_version: SCHEMA_VERSION,
            shadow_only: true,
            builder_host_id: M3_HOST.to_owned(),
            host_session_generation,
            manifest_sha256: proof.manifest.digest(proof.inventory, proof.plan)?,
            source: proof.manifest.source.clone(),
            build: proof.manifest.build.clone(),
            inventory_sha256: proof.inventory.digest()?,
            configure_command_sha256,
            build_command_sha256,
            configure_invocations: 1,
            build_invocations: 1,
            configure_exit_code: 0,
            build_exit_code: 0,
            artifact: proof.manifest.artifact.clone(),
        };
        receipt.validate(proof)?;
        Ok(receipt)
    }

    /// Validate the receipt against the inseparable source/build/artifact proof.
    pub fn validate(&self, proof: ParallelProofContext<'_>) -> Result<(), ParallelProofError> {
        let proof = ParallelProofContext::new(proof.manifest, proof.inventory, proof.plan)?;
        if self.schema_version != SCHEMA_VERSION {
            return Err(ParallelProofError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if !self.shadow_only {
            return Err(ParallelProofError::InvalidField("one-host shadow_only"));
        }
        if self.builder_host_id != M3_HOST || self.host_session_generation == 0 {
            return Err(ParallelProofError::InvalidField(
                "one-host builder identity",
            ));
        }
        if self.configure_invocations != 1 || self.build_invocations != 1 {
            return Err(ParallelProofError::InvalidField(
                "one-host configure/build invocation count",
            ));
        }
        if self.configure_exit_code != 0 || self.build_exit_code != 0 {
            return Err(ParallelProofError::InvalidField(
                "one-host configure/build exit code",
            ));
        }
        if self.manifest_sha256 != proof.manifest.digest(proof.inventory, proof.plan)? {
            return Err(ParallelProofError::BindingMismatch(
                "one-host proof manifest",
            ));
        }
        if self.source != proof.manifest.source {
            return Err(ParallelProofError::BindingMismatch("one-host source"));
        }
        if self.build != proof.manifest.build {
            return Err(ParallelProofError::BindingMismatch("one-host build"));
        }
        if self.inventory_sha256 != proof.inventory.digest()? {
            return Err(ParallelProofError::BindingMismatch(
                "one-host CTest inventory",
            ));
        }
        if self.artifact != proof.manifest.artifact {
            return Err(ParallelProofError::BindingMismatch("one-host artifact"));
        }
        ensure_record_bound("one-host configure/build receipt", self)
    }

    /// Domain-separated digest binding this exact immutable receipt.
    pub fn digest(
        &self,
        proof: ParallelProofContext<'_>,
    ) -> Result<Sha256Digest, ParallelProofError> {
        self.validate(proof)?;
        canonical_digest("shipyard.parallel-proof.one-host-build.v1", self)
    }

    /// A v1 one-host receipt never satisfies merge readiness.
    #[must_use]
    pub const fn satisfies_merge_readiness(&self) -> bool {
        false
    }
}

/// Controller-owned observation of consuming the exact built artifact on M3.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OneHostConsumptionObservation {
    /// Observation schema version.
    pub schema_version: u32,
    /// Exact consuming host; v1 requires the same M3 builder.
    pub consumer_host_id: String,
    /// Authenticated consumer-session generation.
    pub host_session_generation: u64,
    /// Digest of the exact configure/build receipt consumed.
    pub configure_build_receipt_sha256: Sha256Digest,
    /// Payload digest verified immediately before execution.
    pub artifact_payload_sha256: Sha256Digest,
    /// Layout digest verified immediately before execution.
    pub artifact_layout_sha256: Sha256Digest,
    /// Artifact byte length verified immediately before execution.
    pub artifact_size_bytes: u64,
    /// Configure invocations observed while consuming the artifact; must be zero.
    pub configure_invocations_during_consume: u32,
    /// Build invocations observed while consuming the artifact; must be zero.
    pub build_invocations_during_consume: u32,
    /// Exact sorted, unique `CTest` identifiers observed as executed.
    pub executed_test_ids: Vec<String>,
}

/// Result of the default-off one-host shadow assessment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OneHostM3Decision {
    /// Machine-global policy has not explicitly opted in.
    Disabled,
    /// Exact build-once consumption and executed-set reconciliation succeeded.
    ShadowMatched {
        /// Exact configure/build receipt consumed.
        configure_build_receipt_sha256: Sha256Digest,
        /// Content address of the consumed artifact.
        artifact_payload_sha256: Sha256Digest,
        /// Exact canonical `CTest` inventory reconciled.
        inventory_sha256: Sha256Digest,
        /// Number of exact tests observed in both declared and executed sets.
        executed_test_count: u32,
        /// Remains true for every v1 outcome.
        full_gate_authoritative: bool,
        /// Remains false for this one-host foundation step.
        cross_host_dispatch: bool,
    },
}

impl OneHostM3Decision {
    /// A one-host shadow match never satisfies merge readiness.
    #[must_use]
    pub const fn satisfies_merge_readiness(&self) -> bool {
        false
    }
}

/// Assess one exact same-host artifact consumption without running any process.
pub fn assess_one_host_m3(
    proof: ParallelProofContext<'_>,
    policy: &OneHostM3Policy,
    receipt: &ConfigureBuildReceipt,
    observation: &OneHostConsumptionObservation,
) -> Result<OneHostM3Decision, ParallelProofError> {
    let proof = ParallelProofContext::new(proof.manifest, proof.inventory, proof.plan)?;
    if !policy.enabled {
        return Ok(OneHostM3Decision::Disabled);
    }
    validate_scope(proof, policy)?;
    receipt.validate(proof)?;
    validate_consumption(proof, receipt, observation)?;

    Ok(OneHostM3Decision::ShadowMatched {
        configure_build_receipt_sha256: receipt.digest(proof)?,
        artifact_payload_sha256: receipt.artifact.payload_sha256.clone(),
        inventory_sha256: proof.inventory.digest()?,
        executed_test_count: u32::try_from(proof.inventory.tests.len())
            .map_err(|_| ParallelProofError::InvalidField("one-host executed test count"))?,
        full_gate_authoritative: true,
        cross_host_dispatch: false,
    })
}

fn validate_scope(
    proof: ParallelProofContext<'_>,
    policy: &OneHostM3Policy,
) -> Result<(), ParallelProofError> {
    if policy.repository != PULP_REPOSITORY
        || policy.target != PULP_TARGET
        || policy.host_id != M3_HOST
        || proof.manifest.source.repository != PULP_REPOSITORY
        || proof.manifest.source.repository_id != PULP_REPOSITORY_ID
        || proof.manifest.build.target_triple != "aarch64-apple-darwin"
    {
        return Err(ParallelProofError::InvalidField("one-host canary scope"));
    }
    Ok(())
}

fn validate_consumption(
    proof: ParallelProofContext<'_>,
    receipt: &ConfigureBuildReceipt,
    observation: &OneHostConsumptionObservation,
) -> Result<(), ParallelProofError> {
    if observation.schema_version != SCHEMA_VERSION {
        return Err(ParallelProofError::UnsupportedSchemaVersion(
            observation.schema_version,
        ));
    }
    if observation.consumer_host_id != receipt.builder_host_id
        || observation.consumer_host_id != M3_HOST
        || observation.host_session_generation != receipt.host_session_generation
    {
        return Err(ParallelProofError::BindingMismatch("one-host session"));
    }
    if observation.configure_build_receipt_sha256 != receipt.digest(proof)? {
        return Err(ParallelProofError::BindingMismatch(
            "one-host configure/build receipt",
        ));
    }
    if observation.artifact_payload_sha256 != receipt.artifact.payload_sha256
        || observation.artifact_layout_sha256 != receipt.artifact.layout_sha256
        || observation.artifact_size_bytes != receipt.artifact.size_bytes
    {
        return Err(ParallelProofError::BindingMismatch(
            "one-host consumed artifact",
        ));
    }
    if observation.configure_invocations_during_consume != 0
        || observation.build_invocations_during_consume != 0
    {
        return Err(ParallelProofError::InvalidField(
            "one-host consumption rebuilt artifact",
        ));
    }
    let expected = proof
        .inventory
        .tests
        .iter()
        .map(|test| test.id.as_str())
        .collect::<Vec<_>>();
    if !strictly_sorted_unique(&observation.executed_test_ids) {
        return Err(ParallelProofError::NonCanonical(
            "one-host executed test ids",
        ));
    }
    if observation
        .executed_test_ids
        .iter()
        .map(String::as_str)
        .ne(expected)
    {
        return Err(ParallelProofError::BindingMismatch(
            "one-host executed CTest set",
        ));
    }
    ensure_record_bound("one-host consumption observation", observation)
}

fn strictly_sorted_unique(values: &[String]) -> bool {
    values
        .windows(2)
        .all(|pair| pair[0].as_str() < pair[1].as_str())
}

fn ensure_record_bound(
    field: &'static str,
    value: &impl Serialize,
) -> Result<(), ParallelProofError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| ParallelProofError::Json(error.to_string()))?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(ParallelProofError::LimitExceeded {
            field,
            max: MAX_RECORD_BYTES,
            found: bytes.len(),
        });
    }
    Ok(())
}

fn canonical_digest(
    domain: &str,
    value: &impl Serialize,
) -> Result<Sha256Digest, ParallelProofError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| ParallelProofError::Json(error.to_string()))?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(ParallelProofError::LimitExceeded {
            field: "one-host digest input",
            max: MAX_RECORD_BYTES,
            found: bytes.len(),
        });
    }
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_be_bytes());
    hasher.update(domain.as_bytes());
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    Sha256Digest::parse(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parallel_proof::{
        ArtifactTrustClass, ExecutionBoundary, ParallelProofManifest, ProofSubject, ShardPlan,
        TestCase, TestInventory, TrustIdentity,
    };

    struct Fixture {
        inventory: TestInventory,
        plan: ShardPlan,
        manifest: ParallelProofManifest,
    }

    fn sha(byte: u8) -> Sha256Digest {
        Sha256Digest::of_bytes(&[byte])
    }

    fn fixture() -> Fixture {
        let inventory = TestInventory::new(vec![
            TestCase {
                id: "audio".to_owned(),
                dependencies: Vec::new(),
                fixture_setup: Vec::new(),
                fixture_required: Vec::new(),
                fixture_cleanup: Vec::new(),
                run_serial: false,
                resource_locks: Vec::new(),
                required_capabilities: vec!["macos-arm64".to_owned()],
            },
            TestCase {
                id: "render".to_owned(),
                dependencies: Vec::new(),
                fixture_setup: Vec::new(),
                fixture_required: Vec::new(),
                fixture_cleanup: Vec::new(),
                run_serial: false,
                resource_locks: Vec::new(),
                required_capabilities: vec!["macos-arm64".to_owned()],
            },
        ])
        .expect("inventory");
        let plan = ShardPlan::deterministic_balanced(&inventory, 2).expect("plan");
        let source = SourceIdentity {
            repository_id: PULP_REPOSITORY_ID,
            repository: PULP_REPOSITORY.to_owned(),
            subject: ProofSubject::PullRequest { number: 1 },
            head_sha: "a".repeat(40),
            tree_sha: "b".repeat(40),
        };
        let build = BuildIdentity {
            contract_sha256: sha(1),
            toolchain_sha256: sha(2),
            target_triple: "aarch64-apple-darwin".to_owned(),
            profile: "Release".to_owned(),
        };
        let artifact = ArtifactIdentity {
            source_tree_sha: source.tree_sha.clone(),
            build_contract_sha256: build.contract_sha256.clone(),
            payload_sha256: sha(3),
            layout_sha256: sha(4),
            size_bytes: 4096,
        };
        let manifest = ParallelProofManifest::new(
            source,
            build,
            artifact,
            TrustIdentity {
                producer_identity_sha256: sha(5),
                image_sha256: sha(6),
                policy_sha256: sha(7),
                artifact_class: ArtifactTrustClass::TrustedController,
                execution_boundary: ExecutionBoundary::TrustedHost,
                network_enabled: false,
                writable_host_mounts: false,
            },
            &inventory,
            &plan,
        )
        .expect("manifest");
        Fixture {
            inventory,
            plan,
            manifest,
        }
    }

    fn proof(fixture: &Fixture) -> ParallelProofContext<'_> {
        ParallelProofContext::new(&fixture.manifest, &fixture.inventory, &fixture.plan)
            .expect("proof")
    }

    fn receipt(fixture: &Fixture) -> ConfigureBuildReceipt {
        ConfigureBuildReceipt::successful(proof(fixture), 7, sha(8), sha(9)).expect("receipt")
    }

    fn observation(
        fixture: &Fixture,
        receipt: &ConfigureBuildReceipt,
    ) -> OneHostConsumptionObservation {
        OneHostConsumptionObservation {
            schema_version: SCHEMA_VERSION,
            consumer_host_id: M3_HOST.to_owned(),
            host_session_generation: receipt.host_session_generation,
            configure_build_receipt_sha256: receipt.digest(proof(fixture)).expect("receipt digest"),
            artifact_payload_sha256: receipt.artifact.payload_sha256.clone(),
            artifact_layout_sha256: receipt.artifact.layout_sha256.clone(),
            artifact_size_bytes: receipt.artifact.size_bytes,
            configure_invocations_during_consume: 0,
            build_invocations_during_consume: 0,
            executed_test_ids: fixture
                .inventory
                .tests
                .iter()
                .map(|test| test.id.clone())
                .collect(),
        }
    }

    fn enabled_policy() -> OneHostM3Policy {
        OneHostM3Policy {
            enabled: true,
            ..OneHostM3Policy::default()
        }
    }

    #[test]
    fn default_is_disabled_and_full_gate_stays_authoritative() {
        let fixture = fixture();
        let receipt = receipt(&fixture);
        let decision = assess_one_host_m3(
            proof(&fixture),
            &OneHostM3Policy::default(),
            &receipt,
            &observation(&fixture, &receipt),
        )
        .expect("disabled");
        assert_eq!(decision, OneHostM3Decision::Disabled);
        assert!(!decision.satisfies_merge_readiness());
        assert!(!receipt.satisfies_merge_readiness());
    }

    #[test]
    fn exact_m3_artifact_is_consumed_once_without_rebuilding() {
        let fixture = fixture();
        let receipt = receipt(&fixture);
        let policy = enabled_policy();
        let decision = assess_one_host_m3(
            proof(&fixture),
            &policy,
            &receipt,
            &observation(&fixture, &receipt),
        )
        .expect("shadow match");
        assert!(matches!(
            decision,
            OneHostM3Decision::ShadowMatched {
                executed_test_count: 2,
                full_gate_authoritative: true,
                cross_host_dispatch: false,
                ..
            }
        ));
    }

    #[test]
    fn rejects_reconfigure_or_rebuild_during_consumption() {
        let fixture = fixture();
        let receipt = receipt(&fixture);
        let policy = enabled_policy();
        let mut observed = observation(&fixture, &receipt);
        observed.build_invocations_during_consume = 1;
        assert!(matches!(
            assess_one_host_m3(proof(&fixture), &policy, &receipt, &observed),
            Err(ParallelProofError::InvalidField(
                "one-host consumption rebuilt artifact"
            ))
        ));
        observed.build_invocations_during_consume = 0;
        observed.configure_invocations_during_consume = 1;
        assert!(matches!(
            assess_one_host_m3(proof(&fixture), &policy, &receipt, &observed),
            Err(ParallelProofError::InvalidField(
                "one-host consumption rebuilt artifact"
            ))
        ));
    }

    #[test]
    fn rejects_source_build_artifact_and_receipt_mutation() {
        let fixture = fixture();
        let policy = enabled_policy();

        let valid_receipt = receipt(&fixture);
        let valid_observation = observation(&fixture, &valid_receipt);

        let mut changed_source = valid_receipt.clone();
        changed_source.source.head_sha = "c".repeat(40);
        assert!(
            assess_one_host_m3(
                proof(&fixture),
                &policy,
                &changed_source,
                &valid_observation
            )
            .is_err()
        );

        let mut changed_build = valid_receipt.clone();
        changed_build.build.contract_sha256 = sha(10);
        assert!(
            assess_one_host_m3(proof(&fixture), &policy, &changed_build, &valid_observation)
                .is_err()
        );

        let mut changed_artifact = valid_observation.clone();
        changed_artifact.artifact_payload_sha256 = sha(11);
        assert!(matches!(
            assess_one_host_m3(proof(&fixture), &policy, &valid_receipt, &changed_artifact),
            Err(ParallelProofError::BindingMismatch(
                "one-host consumed artifact"
            ))
        ));

        let mut changed_receipt = valid_observation;
        changed_receipt.configure_build_receipt_sha256 = sha(12);
        assert!(matches!(
            assess_one_host_m3(proof(&fixture), &policy, &valid_receipt, &changed_receipt),
            Err(ParallelProofError::BindingMismatch(
                "one-host configure/build receipt"
            ))
        ));
    }

    #[test]
    fn rejects_missing_extra_duplicate_or_reordered_executed_tests() {
        let fixture = fixture();
        let receipt = receipt(&fixture);
        let policy = enabled_policy();

        for ids in [
            vec!["audio".to_owned()],
            vec!["audio".to_owned(), "extra".to_owned(), "render".to_owned()],
            vec!["audio".to_owned(), "audio".to_owned()],
            vec!["render".to_owned(), "audio".to_owned()],
        ] {
            let mut observed = observation(&fixture, &receipt);
            observed.executed_test_ids = ids;
            assert!(assess_one_host_m3(proof(&fixture), &policy, &receipt, &observed).is_err());
        }
    }

    #[test]
    fn rejects_cross_host_session_and_scope_drift() {
        let fixture = fixture();
        let receipt = receipt(&fixture);
        let policy = enabled_policy();
        let mut observed = observation(&fixture, &receipt);
        observed.consumer_host_id = "m1".to_owned();
        assert!(matches!(
            assess_one_host_m3(proof(&fixture), &policy, &receipt, &observed),
            Err(ParallelProofError::BindingMismatch("one-host session"))
        ));

        let mut wrong_scope = policy;
        wrong_scope.target = "linux".to_owned();
        assert!(matches!(
            assess_one_host_m3(
                proof(&fixture),
                &wrong_scope,
                &receipt,
                &observation(&fixture, &receipt)
            ),
            Err(ParallelProofError::InvalidField("one-host canary scope"))
        ));
    }
}
