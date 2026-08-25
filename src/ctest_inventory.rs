//! Pure CTest JSON-v1 inventory production for shadow parallel proof.
//!
//! This module parses already-captured `ctest --show-only=json-v1` output. It
//! does not execute CTest, dispatch workers, publish evidence, or satisfy merge
//! readiness. Resource-lock scopes and runtime capabilities are controller
//! policy, so callers must supply them explicitly rather than trusting mutable
//! test-side labels.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::parallel_proof::{
    MAX_CAPABILITIES, MAX_IDENTIFIER_BYTES, MAX_RECORD_BYTES, MAX_RELATIONS, MAX_TESTS,
    ParallelProofError, ResourceLock, ResourceLockScope, Sha256Digest, TestCase, TestInventory,
};

/// Schema version for controller-owned `CTest` classification.
pub const CTEST_INVENTORY_CLASSIFICATION_SCHEMA_VERSION: u32 = 1;

/// Maximum encoded bytes accepted before decoding a `CTest` observation.
///
/// The observation cannot be larger than the durable inventory it produces;
/// this also bounds allocation amplification while decoding repository-owned
/// property metadata.
pub const MAX_CTEST_JSON_BYTES: usize = MAX_RECORD_BYTES;

/// Controller-owned metadata that `CTest` cannot classify safely by itself.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CtestInventoryClassification {
    /// Classification schema version.
    pub schema_version: u32,
    /// Capabilities required by every test in this target inventory.
    pub target_required_capabilities: Vec<String>,
    /// Independently expected number of configured tests before any filtering.
    pub expected_test_count: u32,
    /// Digest of the independently expected sorted exact test identifiers.
    pub expected_test_ids_sha256: Sha256Digest,
    /// Exact `CTest -C` configuration, or an empty string for no configuration.
    pub expected_config: String,
    /// Additional capabilities for exact test identifiers.
    #[serde(default)]
    pub test_required_capabilities: BTreeMap<String, Vec<String>>,
    /// Explicit exclusion scope for every observed `CTest` `RESOURCE_LOCK`.
    #[serde(default)]
    pub resource_lock_scopes: BTreeMap<String, ResourceLockScope>,
}

/// Fail-closed `CTest` inventory-production error.
#[derive(Debug)]
pub enum CtestInventoryError {
    /// The raw `CTest` JSON exceeded the parser input bound.
    InputTooLarge {
        /// Observed input bytes.
        found: usize,
        /// Maximum accepted input bytes.
        max: usize,
    },
    /// JSON could not be decoded.
    InvalidJson(String),
    /// The document is not the supported `CTest` JSON-v1 shape.
    UnsupportedDocument(String),
    /// Relevant `CTest` metadata is missing, duplicated, or has an unsafe type.
    AmbiguousMetadata(String),
    /// A resource lock has no explicit controller-owned scope.
    UnclassifiedResourceLock(String),
    /// Controller classification names a test or lock absent from the document.
    UnusedClassification(String),
    /// Canonical inventory validation rejected the translated result.
    InvalidInventory(ParallelProofError),
}

impl fmt::Display for CtestInventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { found, max } => {
                write!(formatter, "CTest JSON exceeds limit {max}: found {found}")
            }
            Self::InvalidJson(error) => write!(formatter, "invalid CTest JSON: {error}"),
            Self::UnsupportedDocument(reason) => {
                write!(formatter, "unsupported CTest JSON document: {reason}")
            }
            Self::AmbiguousMetadata(reason) => {
                write!(formatter, "ambiguous CTest metadata: {reason}")
            }
            Self::UnclassifiedResourceLock(lock) => {
                write!(formatter, "resource lock {lock} has no explicit scope")
            }
            Self::UnusedClassification(entry) => {
                write!(
                    formatter,
                    "classification entry does not match CTest metadata: {entry}"
                )
            }
            Self::InvalidInventory(error) => write!(formatter, "invalid test inventory: {error}"),
        }
    }
}

impl std::error::Error for CtestInventoryError {}

impl From<ParallelProofError> for CtestInventoryError {
    fn from(error: ParallelProofError) -> Self {
        Self::InvalidInventory(error)
    }
}

#[derive(Debug, Deserialize)]
struct CtestDocument {
    kind: String,
    version: CtestVersion,
    tests: Vec<CtestTest>,
}

#[derive(Debug, Deserialize)]
struct CtestVersion {
    major: u32,
    minor: u32,
}

#[derive(Debug, Deserialize)]
struct CtestTest {
    name: String,
    #[serde(default)]
    config: CtestConfig,
    command: Option<Vec<String>>,
    #[serde(default)]
    properties: Vec<CtestProperty>,
}

#[derive(Debug, Deserialize)]
struct CtestProperty {
    name: String,
    value: Value,
}

#[derive(Debug, Default)]
enum CtestConfig {
    #[default]
    Missing,
    Value(String),
    Invalid,
}

impl<'de> Deserialize<'de> for CtestConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Value::deserialize(deserializer)? {
            Value::String(value) => Self::Value(value),
            _ => Self::Invalid,
        })
    }
}

/// Translate one bounded `CTest` JSON-v1 observation into the canonical
/// shadow-only parallel-proof inventory.
pub fn inventory_from_ctest_json_v1(
    bytes: &[u8],
    classification: &CtestInventoryClassification,
) -> Result<TestInventory, CtestInventoryError> {
    ensure_input_bound(bytes.len())?;
    validate_classification(classification)?;
    let document: CtestDocument = serde_json::from_slice(bytes)
        .map_err(|error| CtestInventoryError::InvalidJson(error.to_string()))?;
    if document.kind != "ctestInfo" {
        return Err(CtestInventoryError::UnsupportedDocument(format!(
            "kind must be ctestInfo, found {}",
            document.kind
        )));
    }
    if document.version.major != 1 || document.version.minor != 0 {
        return Err(CtestInventoryError::UnsupportedDocument(format!(
            "version must be 1.0, found {}.{}",
            document.version.major, document.version.minor
        )));
    }
    if document.tests.is_empty() || document.tests.len() > MAX_TESTS {
        return Err(CtestInventoryError::UnsupportedDocument(format!(
            "test count must be 1..={MAX_TESTS}, found {}",
            document.tests.len()
        )));
    }

    let mut observed_test_ids = document
        .tests
        .iter()
        .map(|test| test.name.clone())
        .collect::<Vec<_>>();
    for test_id in &observed_test_ids {
        validate_identifier("test name", test_id)?;
    }
    observed_test_ids.sort();
    if observed_test_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(CtestInventoryError::AmbiguousMetadata(
            "duplicate test identifiers".to_owned(),
        ));
    }
    if observed_test_ids.len() != classification.expected_test_count as usize {
        return Err(CtestInventoryError::AmbiguousMetadata(format!(
            "observed test count {} does not match independently expected count {}",
            observed_test_ids.len(),
            classification.expected_test_count
        )));
    }
    if ctest_test_ids_digest(&observed_test_ids)? != classification.expected_test_ids_sha256 {
        return Err(CtestInventoryError::AmbiguousMetadata(
            "observed test identifiers do not match independently expected configured graph"
                .to_owned(),
        ));
    }
    validate_capability_expansion(classification, &observed_test_ids)?;
    let declared_tests = observed_test_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for test_id in classification.test_required_capabilities.keys() {
        if !declared_tests.contains(test_id.as_str()) {
            return Err(CtestInventoryError::UnusedClassification(format!(
                "test capability override {test_id}"
            )));
        }
    }

    let mut observed_locks = BTreeSet::new();
    let mut tests = Vec::with_capacity(document.tests.len());
    for test in document.tests {
        tests.push(translate_test(test, classification, &mut observed_locks)?);
    }
    for lock in classification.resource_lock_scopes.keys() {
        if !observed_locks.contains(lock) {
            return Err(CtestInventoryError::UnusedClassification(format!(
                "resource lock scope {lock}"
            )));
        }
    }
    TestInventory::new(tests).map_err(Into::into)
}

fn ensure_input_bound(found: usize) -> Result<(), CtestInventoryError> {
    if found > MAX_CTEST_JSON_BYTES {
        return Err(CtestInventoryError::InputTooLarge {
            found,
            max: MAX_CTEST_JSON_BYTES,
        });
    }
    Ok(())
}

fn validate_classification(
    classification: &CtestInventoryClassification,
) -> Result<(), CtestInventoryError> {
    if classification.schema_version != CTEST_INVENTORY_CLASSIFICATION_SCHEMA_VERSION {
        return Err(CtestInventoryError::UnsupportedDocument(format!(
            "classification schema must be {}, found {}",
            CTEST_INVENTORY_CLASSIFICATION_SCHEMA_VERSION, classification.schema_version
        )));
    }
    if classification.expected_test_count == 0
        || classification.expected_test_count as usize > MAX_TESTS
    {
        return Err(CtestInventoryError::AmbiguousMetadata(format!(
            "expected test count must be 1..={MAX_TESTS}"
        )));
    }
    if classification.expected_config.len() > MAX_IDENTIFIER_BYTES
        || classification.expected_config.chars().any(char::is_control)
    {
        return Err(CtestInventoryError::AmbiguousMetadata(
            "invalid expected CTest configuration".to_owned(),
        ));
    }
    validate_capabilities(
        "target required capability",
        &classification.target_required_capabilities,
    )?;
    for (test, capabilities) in &classification.test_required_capabilities {
        validate_identifier("test capability override", test)?;
        validate_capabilities("test required capability", capabilities)?;
    }
    for lock in classification.resource_lock_scopes.keys() {
        validate_identifier("resource lock scope", lock)?;
    }
    Ok(())
}

/// Compute the domain-separated digest used to bind the independently
/// configured exhaustive test set.
pub fn ctest_test_ids_digest(test_ids: &[String]) -> Result<Sha256Digest, CtestInventoryError> {
    if test_ids.is_empty() || test_ids.len() > MAX_TESTS {
        return Err(CtestInventoryError::AmbiguousMetadata(format!(
            "test id count must be 1..={MAX_TESTS}"
        )));
    }
    let mut previous: Option<&str> = None;
    let mut hasher = Sha256::new();
    hasher.update(b"shipyard.ctest-inventory.test-ids.v1");
    for test_id in test_ids {
        validate_identifier("test id", test_id)?;
        if previous.is_some_and(|value| value >= test_id.as_str()) {
            return Err(CtestInventoryError::AmbiguousMetadata(
                "test ids must be sorted and unique".to_owned(),
            ));
        }
        hasher.update((test_id.len() as u64).to_be_bytes());
        hasher.update(test_id.as_bytes());
        previous = Some(test_id);
    }
    Sha256Digest::parse(hex::encode(hasher.finalize())).map_err(Into::into)
}

fn validate_capabilities(field: &str, capabilities: &[String]) -> Result<(), CtestInventoryError> {
    let mut previous: Option<&str> = None;
    for capability in capabilities {
        validate_identifier(field, capability)?;
        if previous.is_some_and(|value| value >= capability.as_str()) {
            return Err(CtestInventoryError::AmbiguousMetadata(format!(
                "{field} values must be sorted and unique"
            )));
        }
        previous = Some(capability);
    }
    Ok(())
}

fn validate_capability_expansion(
    classification: &CtestInventoryClassification,
    test_ids: &[String],
) -> Result<(), CtestInventoryError> {
    if classification.target_required_capabilities.len() > MAX_CAPABILITIES {
        return Err(CtestInventoryError::AmbiguousMetadata(format!(
            "target requires more than {MAX_CAPABILITIES} capabilities"
        )));
    }
    let target_bytes = classification
        .target_required_capabilities
        .iter()
        .map(String::len)
        .sum::<usize>();
    let mut relation_count = classification
        .target_required_capabilities
        .len()
        .checked_mul(test_ids.len())
        .ok_or_else(|| {
            CtestInventoryError::AmbiguousMetadata("capability relation count overflow".to_owned())
        })?;
    let mut expanded_bytes = target_bytes.checked_mul(test_ids.len()).ok_or_else(|| {
        CtestInventoryError::AmbiguousMetadata("capability byte count overflow".to_owned())
    })?;
    for test_id in test_ids {
        let Some(extra) = classification.test_required_capabilities.get(test_id) else {
            continue;
        };
        if classification.target_required_capabilities.len() + extra.len() > MAX_CAPABILITIES {
            return Err(CtestInventoryError::AmbiguousMetadata(format!(
                "test {test_id} requires more than {MAX_CAPABILITIES} capabilities"
            )));
        }
        if extra.iter().any(|capability| {
            classification
                .target_required_capabilities
                .binary_search(capability)
                .is_ok()
        }) {
            return Err(CtestInventoryError::AmbiguousMetadata(format!(
                "test {test_id} repeats a target-required capability"
            )));
        }
        relation_count = relation_count.checked_add(extra.len()).ok_or_else(|| {
            CtestInventoryError::AmbiguousMetadata("capability relation count overflow".to_owned())
        })?;
        expanded_bytes = expanded_bytes
            .checked_add(extra.iter().map(String::len).sum::<usize>())
            .ok_or_else(|| {
                CtestInventoryError::AmbiguousMetadata("capability byte count overflow".to_owned())
            })?;
    }
    if relation_count > MAX_RELATIONS {
        return Err(CtestInventoryError::AmbiguousMetadata(format!(
            "capability relations exceed limit {MAX_RELATIONS}: found {relation_count}"
        )));
    }
    if expanded_bytes > MAX_RECORD_BYTES {
        return Err(CtestInventoryError::AmbiguousMetadata(format!(
            "expanded capability bytes exceed limit {MAX_RECORD_BYTES}: found {expanded_bytes}"
        )));
    }
    Ok(())
}

fn validate_identifier(field: &str, value: &str) -> Result<(), CtestInventoryError> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES || value.chars().any(char::is_control)
    {
        return Err(CtestInventoryError::AmbiguousMetadata(format!(
            "invalid {field}"
        )));
    }
    Ok(())
}

fn translate_test(
    test: CtestTest,
    classification: &CtestInventoryClassification,
    observed_locks: &mut BTreeSet<String>,
) -> Result<TestCase, CtestInventoryError> {
    validate_identifier("test name", &test.name)?;
    let observed_config = match &test.config {
        CtestConfig::Missing => "",
        CtestConfig::Value(value) => value,
        CtestConfig::Invalid => {
            return Err(CtestInventoryError::AmbiguousMetadata(format!(
                "test {} configuration must be an omitted field or string",
                test.name
            )));
        }
    };
    if observed_config != classification.expected_config {
        return Err(CtestInventoryError::AmbiguousMetadata(format!(
            "test {} configuration {:?} does not match expected {:?}",
            test.name, observed_config, classification.expected_config
        )));
    }
    let mut properties = BTreeMap::new();
    for property in test.properties {
        validate_identifier("property name", &property.name)?;
        if properties
            .insert(property.name.clone(), property.value)
            .is_some()
        {
            return Err(CtestInventoryError::AmbiguousMetadata(format!(
                "test {} repeats property {}",
                test.name, property.name
            )));
        }
    }
    if properties.contains_key("RESOURCE_GROUPS") {
        return Err(CtestInventoryError::UnsupportedDocument(format!(
            "test {} uses RESOURCE_GROUPS",
            test.name
        )));
    }
    if boolean_property(&test.name, "DISABLED", properties.get("DISABLED"))? {
        return Err(CtestInventoryError::UnsupportedDocument(format!(
            "test {} is disabled and cannot enter an executable inventory",
            test.name
        )));
    }
    let required_files = string_list(
        &test.name,
        "REQUIRED_FILES",
        properties.get("REQUIRED_FILES"),
    )?;
    if !required_files.is_empty() {
        return Err(CtestInventoryError::UnsupportedDocument(format!(
            "test {} uses REQUIRED_FILES without controller filesystem attestation",
            test.name
        )));
    }
    match test.command.as_deref() {
        Some([executable, ..])
            if !executable.is_empty() && !executable.chars().any(char::is_control) => {}
        _ => {
            return Err(CtestInventoryError::UnsupportedDocument(format!(
                "test {} has no executable command",
                test.name
            )));
        }
    }

    let dependencies = string_list(&test.name, "DEPENDS", properties.get("DEPENDS"))?;
    let fixture_setup = string_list(
        &test.name,
        "FIXTURES_SETUP",
        properties.get("FIXTURES_SETUP"),
    )?;
    let fixture_required = string_list(
        &test.name,
        "FIXTURES_REQUIRED",
        properties.get("FIXTURES_REQUIRED"),
    )?;
    let fixture_cleanup = string_list(
        &test.name,
        "FIXTURES_CLEANUP",
        properties.get("FIXTURES_CLEANUP"),
    )?;
    let run_serial = boolean_property(&test.name, "RUN_SERIAL", properties.get("RUN_SERIAL"))?;
    let lock_names = string_list(&test.name, "RESOURCE_LOCK", properties.get("RESOURCE_LOCK"))?;
    let resource_locks = classify_resource_locks(lock_names, classification, observed_locks)?;

    let mut required_capabilities = classification.target_required_capabilities.clone();
    if let Some(extra) = classification.test_required_capabilities.get(&test.name) {
        required_capabilities.extend(extra.iter().cloned());
    }

    Ok(TestCase {
        id: test.name,
        dependencies,
        fixture_setup,
        fixture_required,
        fixture_cleanup,
        run_serial,
        resource_locks,
        required_capabilities,
    })
}

fn classify_resource_locks(
    lock_names: Vec<String>,
    classification: &CtestInventoryClassification,
    observed_locks: &mut BTreeSet<String>,
) -> Result<Vec<ResourceLock>, CtestInventoryError> {
    lock_names
        .into_iter()
        .map(|name| {
            let scope = classification
                .resource_lock_scopes
                .get(&name)
                .copied()
                .ok_or_else(|| CtestInventoryError::UnclassifiedResourceLock(name.clone()))?;
            observed_locks.insert(name.clone());
            Ok(ResourceLock { name, scope })
        })
        .collect()
}

fn string_list(
    test: &str,
    property: &str,
    value: Option<&Value>,
) -> Result<Vec<String>, CtestInventoryError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value.as_array().ok_or_else(|| {
        CtestInventoryError::AmbiguousMetadata(format!(
            "test {test} property {property} must be an array of strings"
        ))
    })?;
    values
        .iter()
        .map(|value| {
            let value = value.as_str().ok_or_else(|| {
                CtestInventoryError::AmbiguousMetadata(format!(
                    "test {test} property {property} contains a non-string"
                ))
            })?;
            validate_identifier(property, value)?;
            Ok(value.to_owned())
        })
        .collect()
}

fn boolean_property(
    test: &str,
    property: &str,
    value: Option<&Value>,
) -> Result<bool, CtestInventoryError> {
    match value {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(CtestInventoryError::AmbiguousMetadata(format!(
            "test {test} property {property} must be a boolean"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classification() -> CtestInventoryClassification {
        let test_ids = vec![
            "cleanup".to_owned(),
            "consumer".to_owned(),
            "setup".to_owned(),
        ];
        CtestInventoryClassification {
            schema_version: 1,
            target_required_capabilities: vec!["macos-arm64".to_owned()],
            expected_test_count: u32::try_from(test_ids.len()).expect("test count"),
            expected_test_ids_sha256: ctest_test_ids_digest(&test_ids).expect("test id digest"),
            expected_config: String::new(),
            test_required_capabilities: BTreeMap::from([(
                "consumer".to_owned(),
                vec!["coreaudio-device".to_owned()],
            )]),
            resource_lock_scopes: BTreeMap::from([
                ("audio-device".to_owned(), ResourceLockScope::Host),
                ("release-signing".to_owned(), ResourceLockScope::Fleet),
            ]),
        }
    }

    fn complete_document() -> Vec<u8> {
        br#"{
          "kind":"ctestInfo",
          "version":{"major":1,"minor":0},
          "backtraceGraph":{"commands":[],"files":[],"nodes":[]},
          "tests":[
            {"name":"cleanup","command":["cleanup"],"properties":[
              {"name":"FIXTURES_CLEANUP","value":["audio"]},
              {"name":"RESOURCE_LOCK","value":["release-signing"]}
            ]},
            {"name":"consumer","command":["consumer"],"properties":[
              {"name":"DEPENDS","value":["setup"]},
              {"name":"FIXTURES_REQUIRED","value":["audio"]},
              {"name":"RUN_SERIAL","value":true},
              {"name":"RESOURCE_LOCK","value":["audio-device"]},
              {"name":"TIMEOUT","value":120.0}
            ]},
            {"name":"setup","command":["setup"],"properties":[
              {"name":"FIXTURES_SETUP","value":["audio"]}
            ]}
          ]
        }"#
        .to_vec()
    }

    fn empty_policy() -> CtestInventoryClassification {
        let test_ids = vec!["test".to_owned()];
        CtestInventoryClassification {
            schema_version: 1,
            target_required_capabilities: Vec::new(),
            expected_test_count: 1,
            expected_test_ids_sha256: ctest_test_ids_digest(&test_ids).expect("test id digest"),
            expected_config: String::new(),
            test_required_capabilities: BTreeMap::new(),
            resource_lock_scopes: BTreeMap::new(),
        }
    }

    #[test]
    fn translates_complete_topology_and_controller_classification() {
        let inventory = inventory_from_ctest_json_v1(&complete_document(), &classification())
            .expect("inventory");

        assert_eq!(
            inventory
                .tests
                .iter()
                .map(|test| test.id.as_str())
                .collect::<Vec<_>>(),
            vec!["cleanup", "consumer", "setup"]
        );
        let consumer = &inventory.tests[1];
        assert_eq!(consumer.dependencies, ["setup"]);
        assert_eq!(consumer.fixture_required, ["audio"]);
        assert!(consumer.run_serial);
        assert_eq!(consumer.resource_locks[0].name, "audio-device");
        assert_eq!(consumer.resource_locks[0].scope, ResourceLockScope::Host);
        assert_eq!(
            consumer.required_capabilities,
            ["coreaudio-device", "macos-arm64"]
        );
        assert_eq!(
            inventory.tests[0].resource_locks[0].scope,
            ResourceLockScope::Fleet
        );
        inventory.digest().expect("canonical digest");
    }

    #[test]
    fn translation_is_deterministic_across_document_order() {
        let first = inventory_from_ctest_json_v1(&complete_document(), &classification())
            .expect("first inventory");
        let mut reordered: Value =
            serde_json::from_slice(&complete_document()).expect("fixture JSON");
        reordered["tests"]
            .as_array_mut()
            .expect("test array")
            .reverse();
        let second = inventory_from_ctest_json_v1(
            &serde_json::to_vec(&reordered).expect("reordered JSON"),
            &classification(),
        )
        .expect("second inventory");
        assert_eq!(first, second);
        assert_eq!(
            first.digest().expect("first digest"),
            second.digest().expect("second digest")
        );
    }

    #[test]
    fn rejects_a_filtered_subset_of_the_configured_graph() {
        let filtered = br#"{
          "kind":"ctestInfo","version":{"major":1,"minor":0},
          "tests":[{"name":"setup","command":["setup"],"properties":[]}]
        }"#;
        assert!(matches!(
            inventory_from_ctest_json_v1(filtered, &classification()),
            Err(CtestInventoryError::AmbiguousMetadata(reason)) if reason.contains("count")
        ));
    }

    #[test]
    fn rejects_a_different_ctest_configuration() {
        let mut document: Value =
            serde_json::from_slice(&complete_document()).expect("fixture JSON");
        for test in document["tests"].as_array_mut().expect("test array") {
            test["config"] = Value::String("Release".to_owned());
        }
        assert!(matches!(
            inventory_from_ctest_json_v1(
                &serde_json::to_vec(&document).expect("configured JSON"),
                &classification(),
            ),
            Err(CtestInventoryError::AmbiguousMetadata(reason)) if reason.contains("configuration")
        ));
    }

    #[test]
    fn rejects_a_null_ctest_configuration() {
        let mut document: Value =
            serde_json::from_slice(&complete_document()).expect("fixture JSON");
        document["tests"][0]["config"] = Value::Null;
        assert!(matches!(
            inventory_from_ctest_json_v1(
                &serde_json::to_vec(&document).expect("null-config JSON"),
                &classification(),
            ),
            Err(CtestInventoryError::AmbiguousMetadata(reason)) if reason.contains("configuration")
        ));
    }

    #[test]
    fn rejects_unclassified_or_unused_resource_locks() {
        let mut missing = classification();
        missing.resource_lock_scopes.remove("audio-device");
        assert!(matches!(
            inventory_from_ctest_json_v1(&complete_document(), &missing),
            Err(CtestInventoryError::UnclassifiedResourceLock(lock)) if lock == "audio-device"
        ));

        let mut unused = classification();
        unused
            .resource_lock_scopes
            .insert("typo".to_owned(), ResourceLockScope::Host);
        assert!(matches!(
            inventory_from_ctest_json_v1(&complete_document(), &unused),
            Err(CtestInventoryError::UnusedClassification(entry)) if entry.contains("typo")
        ));
    }

    #[test]
    fn rejects_unknown_test_capability_override() {
        let mut policy = classification();
        policy
            .test_required_capabilities
            .insert("missing".to_owned(), vec!["macos-arm64".to_owned()]);
        assert!(matches!(
            inventory_from_ctest_json_v1(&complete_document(), &policy),
            Err(CtestInventoryError::UnusedClassification(entry)) if entry.contains("missing")
        ));
    }

    #[test]
    fn rejects_unsupported_ctest_version_and_resource_groups() {
        let wrong_version = String::from_utf8(complete_document())
            .expect("utf8")
            .replace("\"minor\":0", "\"minor\":1");
        assert!(matches!(
            inventory_from_ctest_json_v1(wrong_version.as_bytes(), &classification()),
            Err(CtestInventoryError::UnsupportedDocument(reason)) if reason.contains("version")
        ));

        let document = br#"{
          "kind":"ctestInfo","version":{"major":1,"minor":0},
          "tests":[{"name":"test","properties":[
            {"name":"RESOURCE_GROUPS","value":["gpus:1"]}
          ]}]
        }"#;
        assert!(matches!(
            inventory_from_ctest_json_v1(document, &empty_policy()),
            Err(CtestInventoryError::UnsupportedDocument(reason))
                if reason.contains("RESOURCE_GROUPS")
        ));

        let disabled = br#"{
          "kind":"ctestInfo","version":{"major":1,"minor":0},
          "tests":[{"name":"test","command":["test"],"properties":[
            {"name":"DISABLED","value":true}
          ]}]
        }"#;
        assert!(matches!(
            inventory_from_ctest_json_v1(disabled, &empty_policy()),
            Err(CtestInventoryError::UnsupportedDocument(reason)) if reason.contains("disabled")
        ));

        let no_command = br#"{
          "kind":"ctestInfo","version":{"major":1,"minor":0},
          "tests":[{"name":"test","properties":[]}]
        }"#;
        assert!(matches!(
            inventory_from_ctest_json_v1(no_command, &empty_policy()),
            Err(CtestInventoryError::UnsupportedDocument(reason)) if reason.contains("command")
        ));

        let required_files = br#"{
          "kind":"ctestInfo","version":{"major":1,"minor":0},
          "tests":[{"name":"test","command":["test"],"properties":[
            {"name":"REQUIRED_FILES","value":["fixture.dat"]}
          ]}]
        }"#;
        assert!(matches!(
            inventory_from_ctest_json_v1(required_files, &empty_policy()),
            Err(CtestInventoryError::UnsupportedDocument(reason))
                if reason.contains("REQUIRED_FILES")
        ));
    }

    #[test]
    fn rejects_capability_expansion_before_cloning() {
        let mut policy = empty_policy();
        policy.target_required_capabilities = (0..=MAX_CAPABILITIES)
            .map(|index| format!("capability-{index:03}"))
            .collect();
        let document = br#"{
          "kind":"ctestInfo","version":{"major":1,"minor":0},
          "tests":[{"name":"test","command":["test"],"properties":[]}]
        }"#;
        assert!(matches!(
            inventory_from_ctest_json_v1(document, &policy),
            Err(CtestInventoryError::AmbiguousMetadata(reason))
                if reason.contains("capabilities")
        ));
    }

    #[test]
    fn rejects_ambiguous_property_types_and_duplicates() {
        let wrong_type = br#"{
          "kind":"ctestInfo","version":{"major":1,"minor":0},
          "tests":[{"name":"test","command":["test"],"properties":[
            {"name":"RUN_SERIAL","value":"TRUE"}
          ]}]
        }"#;
        assert!(matches!(
            inventory_from_ctest_json_v1(wrong_type, &empty_policy()),
            Err(CtestInventoryError::AmbiguousMetadata(reason)) if reason.contains("boolean")
        ));

        let duplicate = br#"{
          "kind":"ctestInfo","version":{"major":1,"minor":0},
          "tests":[{"name":"test","properties":[
            {"name":"DEPENDS","value":[]},
            {"name":"DEPENDS","value":[]}
          ]}]
        }"#;
        assert!(matches!(
            inventory_from_ctest_json_v1(duplicate, &empty_policy()),
            Err(CtestInventoryError::AmbiguousMetadata(reason)) if reason.contains("repeats")
        ));
    }

    #[test]
    fn input_bound_fails_before_json_decode() {
        let error = ensure_input_bound(MAX_CTEST_JSON_BYTES + 1).expect_err("oversized input");
        assert!(matches!(error, CtestInventoryError::InputTooLarge { .. }));
    }
}
