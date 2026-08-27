//! Fail-closed authorization for metadata-only pull-request validation.
//!
//! This module is deliberately pure: callers must independently observe the
//! exact pull-request identity, Git tree, changed paths, and hosted checks.
//! An authorized receipt may replace native targets only while every identity
//! and policy field remains byte-for-byte unchanged.

use std::collections::{BTreeMap, BTreeSet};

use glob::{MatchOptions, Pattern};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::LoadedConfig;

/// Current metadata-only authority policy schema.
pub const METADATA_AUTHORITY_POLICY_SCHEMA_VERSION: u32 = 1;
/// Current immutable metadata-only authority receipt schema.
pub const METADATA_AUTHORITY_RECEIPT_SCHEMA_VERSION: u32 = 1;

const PATH_MATCH_OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// Trusted, repository-scoped metadata-only policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataAuthorityPolicy {
    /// Policy schema.
    pub schema_version: u32,
    /// Canonical `owner/repository` identity.
    pub repository: String,
    /// Protected base branch this policy covers.
    pub base_ref: String,
    /// Complete repository-relative glob allowlist.
    pub allowed_paths: Vec<String>,
    /// Hosted checks that must be present once and terminal-successful.
    pub required_checks: Vec<String>,
}

/// One independently observed hosted check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostedCheckObservation {
    /// Check context/name.
    pub name: String,
    /// GitHub status (`COMPLETED` for `CheckRun`, terminal state for contexts).
    pub status: String,
    /// GitHub conclusion/state.
    pub conclusion: String,
    /// Stable GitHub producer identity (`app:<database-id>` or `actor:<type>:<database-id>`).
    pub producer: String,
}

/// Exact observation supplied to the pure authority decision.
#[allow(clippy::struct_excessive_bools)] // Flat facts preserve the auditable GitHub/git snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MetadataAuthorityObservation {
    /// Canonical `owner/repository` identity.
    pub repository: String,
    /// Pull request number.
    pub pull_request: u64,
    /// Pull request base branch.
    pub base_ref: String,
    /// Pull request base SHA.
    pub base_sha: String,
    /// Current protected base-ref SHA.
    pub protected_ref_sha: String,
    /// Whether the base ref is protected.
    pub protected_ref_protected: bool,
    /// Pull request head SHA from GitHub.
    pub remote_head_sha: String,
    /// Head tree SHA from GitHub.
    pub remote_tree_sha: String,
    /// Local checkout HEAD.
    pub local_head_sha: String,
    /// Local checkout tree.
    pub local_tree_sha: String,
    /// Local merge base between base and head.
    pub local_merge_base_sha: String,
    /// GitHub-observed merge base between base and head.
    pub remote_merge_base_sha: String,
    /// Whether the merge base is an ancestor of the head.
    pub merge_base_is_ancestor: bool,
    /// Whether the checkout has no tracked or untracked changes.
    pub checkout_clean: bool,
    /// Complete changed-path observation from GitHub.
    pub remote_changed_paths: Vec<String>,
    /// Whether the GitHub changed-path observation was exhaustive.
    pub remote_changed_paths_complete: bool,
    /// Complete changed-path observation from the local merge base.
    pub local_changed_paths: Vec<String>,
    /// Whether the local changed-path observation was exhaustive.
    pub local_changed_paths_complete: bool,
    /// Fresh exact-head hosted check observations.
    pub hosted_checks: Vec<HostedCheckObservation>,
}

/// Immutable authorization carried by a zero-native ship request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataAuthorityReceipt {
    /// Receipt schema.
    pub schema_version: u32,
    /// Canonical repository.
    pub repository: String,
    /// Pull request number.
    pub pull_request: u64,
    /// Protected base branch.
    pub base_ref: String,
    /// Exact base SHA.
    pub base_sha: String,
    /// Exact head SHA.
    pub head_sha: String,
    /// Exact head tree SHA.
    pub tree_sha: String,
    /// Ordinary target used only to reproduce the independent identity/path observation.
    pub observation_target: String,
    /// SHA-256 of the canonical trusted policy.
    pub policy_digest: String,
    /// SHA-256 of the exact sorted changed-path set.
    pub changed_paths_digest: String,
    /// SHA-256 of the exact successful required-check observations.
    pub required_checks_digest: String,
    /// Exact sorted changed paths authorized by the policy.
    pub changed_paths: Vec<String>,
    /// Exact sorted required check names observed successful.
    pub required_checks: Vec<String>,
    /// Exact successful hosted-check observations bound by the digest.
    pub hosted_checks: Vec<HostedCheckObservation>,
}

/// Conservative decision: authorize zero-native execution or preserve full.
#[allow(clippy::large_enum_variant)] // The successful value is an immutable receipt, not a hot collection element.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataAuthorityDecision {
    /// Exact metadata-only authority is proven.
    Authorized(MetadataAuthorityReceipt),
    /// Ordinary full validation must remain authoritative.
    Full {
        /// Stable fail-closed reason suitable for an audit receipt.
        reason: String,
    },
}

/// Normalize GitHub `CheckRun` and `StatusContext` nodes without promoting a
/// pending context to terminal success.
#[must_use]
pub fn parse_hosted_checks(values: &[Value]) -> Vec<HostedCheckObservation> {
    values
        .iter()
        .filter_map(|value| {
            let name = value
                .get("name")
                .or_else(|| value.get("context"))?
                .as_str()?
                .to_owned();
            if let Some(status) = value.get("status").and_then(Value::as_str) {
                let producer = value
                    .pointer("/checkSuite/app/databaseId")
                    .and_then(Value::as_u64)
                    .map(|id| format!("app:{id}"))?;
                return Some(HostedCheckObservation {
                    name,
                    status: status.to_owned(),
                    conclusion: value
                        .get("conclusion")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    producer,
                });
            }
            let state = value.get("state").and_then(Value::as_str)?;
            let actor_type = value.pointer("/creator/__typename")?.as_str()?;
            let producer = value
                .pointer("/creator/databaseId")
                .and_then(Value::as_u64)
                .map(|id| format!("actor:{actor_type}:{id}"))?;
            Some(HostedCheckObservation {
                name,
                status: if state.eq_ignore_ascii_case("pending") {
                    "PENDING".to_owned()
                } else {
                    "COMPLETED".to_owned()
                },
                conclusion: state.to_owned(),
                producer,
            })
        })
        .collect()
}

/// Recheck required hosted contexts at worker start. Unrelated checks may be
/// present, but every configured context must remain unique and successful.
pub fn verify_live_checks(
    policy: &MetadataAuthorityPolicy,
    checks: &[HostedCheckObservation],
) -> Result<(), String> {
    let mut by_name = BTreeMap::<String, Vec<&HostedCheckObservation>>::new();
    for check in checks {
        by_name
            .entry(check.name.to_ascii_lowercase())
            .or_default()
            .push(check);
    }
    for required in &policy.required_checks {
        let (required_name, required_producer) = required.split_once('@').ok_or_else(|| {
            "configured hosted check must include producer as name@app:<id> or name@user:<id>"
                .to_owned()
        })?;
        let observed = by_name
            .get(&required_name.to_ascii_lowercase())
            .ok_or_else(|| "a configured hosted check is missing".to_owned())?;
        if observed.len() != 1 {
            return Err(
                "a configured hosted check has ambiguous duplicate observations".to_owned(),
            );
        }
        let check = observed[0];
        if check.producer != required_producer {
            return Err("a configured hosted check has the wrong producer".to_owned());
        }
        if !check.status.eq_ignore_ascii_case("completed")
            || !check.conclusion.eq_ignore_ascii_case("success")
        {
            return Err("a configured hosted check is pending or unsuccessful".to_owned());
        }
    }
    Ok(())
}

/// Load one exact repository policy from trusted machine-global config.
///
/// Project/worktree overlays cannot activate or widen this authority.
pub fn trusted_policy(
    config: &LoadedConfig,
    repository: &str,
) -> Result<Option<MetadataAuthorityPolicy>, String> {
    let trusted = LoadedConfig::load_machine_global_from_dir(config.global_dir.clone())
        .map_err(|error| format!("load trusted metadata authority policy: {error}"))?;
    match trusted.get_str("metadata_authority.mode") {
        None | Some("off") => return Ok(None),
        Some("authoritative") => {}
        Some(value) => return Err(format!("invalid metadata_authority.mode '{value}'")),
    }
    let repositories = trusted
        .get("metadata_authority.repositories")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "metadata_authority.repositories must be a nonempty table".to_owned())?;
    if repositories.is_empty() {
        return Err("metadata_authority.repositories must be a nonempty table".to_owned());
    }
    let canonical = canonical_repository(repository);
    let mut matched = None;
    for (configured_repo, value) in repositories {
        if canonical_repository(configured_repo) != canonical {
            continue;
        }
        if matched.is_some() {
            return Err("metadata authority repository identity is duplicated".to_owned());
        }
        let mut policy: MetadataAuthorityPolicy = value
            .clone()
            .try_into()
            .map_err(|error| format!("decode metadata authority policy: {error}"))?;
        policy.repository = canonical_repository(&policy.repository);
        validate_policy(&policy)?;
        if policy.repository != canonical {
            return Err("metadata authority table key and policy repository differ".to_owned());
        }
        matched = Some(policy);
    }
    Ok(matched)
}

/// Decide whether one exact observation may use zero-native authority.
#[must_use]
#[allow(clippy::too_many_lines)] // Keep the fail-closed decision chain in one auditable order.
pub fn authorize_metadata_only(
    policy: &MetadataAuthorityPolicy,
    observation: &MetadataAuthorityObservation,
    observation_target: &str,
) -> MetadataAuthorityDecision {
    let full = |reason: &str| MetadataAuthorityDecision::Full {
        reason: reason.to_owned(),
    };
    if let Err(reason) = validate_policy(policy) {
        return full(&reason);
    }
    if canonical_repository(&policy.repository) != canonical_repository(&observation.repository) {
        return full("repository identity is outside the trusted metadata policy");
    }
    if observation.pull_request == 0
        || observation.base_ref != policy.base_ref
        || observation_target.trim().is_empty()
    {
        return full("pull-request/base identity is outside the trusted metadata policy");
    }
    if !valid_sha(&observation.base_sha)
        || !valid_sha(&observation.protected_ref_sha)
        || !valid_sha(&observation.remote_head_sha)
        || !valid_sha(&observation.remote_tree_sha)
    {
        return full("remote pull-request identity is incomplete or malformed");
    }
    if !observation.protected_ref_protected
        || observation.base_sha != observation.protected_ref_sha
        || observation.local_merge_base_sha != observation.remote_merge_base_sha
        || observation.local_merge_base_sha != observation.base_sha
        || !observation.merge_base_is_ancestor
    {
        return full("protected base or merge-base identity is stale or ambiguous");
    }
    if !observation.checkout_clean
        || observation.local_head_sha != observation.remote_head_sha
        || observation.local_tree_sha != observation.remote_tree_sha
    {
        return full("local checkout does not exactly match the clean remote head/tree");
    }
    if !observation.remote_changed_paths_complete || !observation.local_changed_paths_complete {
        return full("changed-path observation is incomplete");
    }
    let Some(remote_paths) = canonical_paths(&observation.remote_changed_paths) else {
        return full("remote changed paths are malformed or ambiguous");
    };
    let Some(local_paths) = canonical_paths(&observation.local_changed_paths) else {
        return full("local changed paths are malformed or ambiguous");
    };
    if remote_paths.is_empty() || remote_paths != local_paths {
        return full("local and remote changed-path sets do not exactly match");
    }
    let patterns = policy
        .allowed_paths
        .iter()
        .map(|value| Pattern::new(value))
        .collect::<Result<Vec<_>, _>>()
        .expect("validated policy patterns");
    if remote_paths.iter().any(|path| {
        !patterns
            .iter()
            .any(|pattern| pattern.matches_with(path, PATH_MATCH_OPTIONS))
    }) {
        return full("one or more changed paths are outside the metadata allowlist");
    }

    if let Err(reason) = verify_live_checks(policy, &observation.hosted_checks) {
        return full(&reason);
    }
    let mut checks_by_name = BTreeMap::<String, Vec<&HostedCheckObservation>>::new();
    for check in &observation.hosted_checks {
        checks_by_name
            .entry(check.name.to_ascii_lowercase())
            .or_default()
            .push(check);
    }
    let mut accepted_checks = Vec::new();
    let mut accepted_observations = Vec::new();
    for required in &policy.required_checks {
        let Some((required_name, _)) = required.split_once('@') else {
            return full("a configured hosted check omits its producer identity");
        };
        let Some(observed) = checks_by_name.get(&required_name.to_ascii_lowercase()) else {
            return full("a configured hosted check is missing");
        };
        if observed.len() != 1 {
            return full("a configured hosted check has ambiguous duplicate observations");
        }
        let check = observed[0];
        if !check.status.eq_ignore_ascii_case("completed")
            || !check.conclusion.eq_ignore_ascii_case("success")
        {
            return full("a configured hosted check is pending or unsuccessful");
        }
        accepted_checks.push(required.clone());
        accepted_observations.push(check.clone());
    }
    accepted_checks.sort_by_key(|value| value.to_ascii_lowercase());
    accepted_observations.sort_by_key(|value| value.name.to_ascii_lowercase());

    MetadataAuthorityDecision::Authorized(MetadataAuthorityReceipt {
        schema_version: METADATA_AUTHORITY_RECEIPT_SCHEMA_VERSION,
        repository: canonical_repository(&observation.repository),
        pull_request: observation.pull_request,
        base_ref: observation.base_ref.clone(),
        base_sha: observation.base_sha.clone(),
        head_sha: observation.remote_head_sha.clone(),
        tree_sha: observation.remote_tree_sha.clone(),
        observation_target: observation_target.to_owned(),
        policy_digest: digest_json(policy),
        changed_paths_digest: digest_json(&remote_paths),
        required_checks_digest: digest_json(&accepted_observations),
        changed_paths: remote_paths,
        required_checks: accepted_checks,
        hosted_checks: accepted_observations,
    })
}

/// Revalidate the immutable receipt against the request and trusted policy.
#[allow(clippy::too_many_arguments)] // Every exact identity component is deliberately explicit.
pub fn verify_receipt(
    receipt: &MetadataAuthorityReceipt,
    policy: &MetadataAuthorityPolicy,
    repository: &str,
    pull_request: u64,
    base_ref: &str,
    head_sha: &str,
    tree_sha: &str,
    observation_target: &str,
) -> Result<(), String> {
    validate_policy(policy)?;
    if receipt.schema_version != METADATA_AUTHORITY_RECEIPT_SCHEMA_VERSION {
        return Err("unsupported metadata authority receipt schema".to_owned());
    }
    if receipt.repository != canonical_repository(repository)
        || receipt.pull_request != pull_request
        || receipt.base_ref != base_ref
        || !valid_sha(&receipt.base_sha)
        || receipt.head_sha != head_sha
        || receipt.tree_sha != tree_sha
        || receipt.observation_target != observation_target
    {
        return Err("metadata authority receipt identity drifted".to_owned());
    }
    if receipt.policy_digest != digest_json(policy)
        || receipt.changed_paths_digest != digest_json(&receipt.changed_paths)
        || receipt.required_checks_digest != digest_json(&receipt.hosted_checks)
    {
        return Err("metadata authority receipt or policy digest drifted".to_owned());
    }
    let Some(paths) = canonical_paths(&receipt.changed_paths) else {
        return Err("metadata authority receipt paths are malformed".to_owned());
    };
    if paths.is_empty() {
        return Err("metadata authority receipt has no changed paths".to_owned());
    }
    let patterns = policy
        .allowed_paths
        .iter()
        .map(|value| Pattern::new(value))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "metadata authority policy path is malformed".to_owned())?;
    if paths.iter().any(|path| {
        !patterns
            .iter()
            .any(|pattern| pattern.matches_with(path, PATH_MATCH_OPTIONS))
    }) {
        return Err("metadata authority receipt path is outside policy".to_owned());
    }
    let mut observed_names = Vec::new();
    let mut unique_names = BTreeSet::new();
    for check in &receipt.hosted_checks {
        if !unique_names.insert(check.name.to_ascii_lowercase())
            || !check.status.eq_ignore_ascii_case("completed")
            || !check.conclusion.eq_ignore_ascii_case("success")
        {
            return Err("metadata authority receipt hosted checks are invalid".to_owned());
        }
        observed_names.push(format!("{}@{}", check.name, check.producer));
    }
    observed_names.sort_by_key(|value| value.to_ascii_lowercase());
    let mut required = receipt.required_checks.clone();
    required.sort_by_key(|value| value.to_ascii_lowercase());
    let mut policy_required = policy.required_checks.clone();
    policy_required.sort_by_key(|value| value.to_ascii_lowercase());
    let lower = |values: &[String]| {
        values
            .iter()
            .map(|value| value.to_ascii_lowercase())
            .collect::<Vec<_>>()
    };
    if lower(&required) != lower(&observed_names) || lower(&required) != lower(&policy_required) {
        return Err("metadata authority receipt required checks drifted".to_owned());
    }
    Ok(())
}

fn validate_policy(policy: &MetadataAuthorityPolicy) -> Result<(), String> {
    if policy.schema_version != METADATA_AUTHORITY_POLICY_SCHEMA_VERSION {
        return Err("unsupported metadata authority policy schema".to_owned());
    }
    if canonical_repository(&policy.repository) != policy.repository.to_ascii_lowercase()
        || policy.base_ref.is_empty()
        || policy.allowed_paths.is_empty()
        || policy.required_checks.is_empty()
    {
        return Err("metadata authority policy is incomplete".to_owned());
    }
    let mut checks = BTreeSet::new();
    for check in &policy.required_checks {
        let valid_identity = check.split_once('@').is_some_and(|(name, producer)| {
            let id = producer
                .strip_prefix("app:")
                .or_else(|| producer.strip_prefix("actor:"));
            !name.is_empty() && id.is_some_and(|id| !id.is_empty())
        });
        if check.trim() != check || !valid_identity || !checks.insert(check.to_ascii_lowercase()) {
            return Err(
                "metadata authority required checks are malformed or duplicated".to_owned(),
            );
        }
    }
    for pattern in &policy.allowed_paths {
        if pattern.trim() != pattern
            || pattern.starts_with('/')
            || pattern.contains("..")
            || pattern.starts_with('*')
            || pattern.starts_with('?')
            || Pattern::new(pattern).is_err()
        {
            return Err("metadata authority allowed path is malformed".to_owned());
        }
    }
    Ok(())
}

fn canonical_paths(paths: &[String]) -> Option<Vec<String>> {
    let mut canonical = BTreeSet::new();
    for path in paths {
        if path.is_empty()
            || path.starts_with('/')
            || path.split('/').any(|part| part.is_empty() || part == "..")
            || !canonical.insert(path.clone())
        {
            return None;
        }
    }
    Some(canonical.into_iter().collect())
}

fn canonical_repository(repository: &str) -> String {
    repository.to_ascii_lowercase()
}

fn valid_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn digest_json<T: Serialize + ?Sized>(value: &T) -> String {
    let encoded = serde_json::to_vec(value).expect("serializable metadata authority value");
    format!("{:x}", Sha256::digest(encoded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LocalOverlaySource;

    fn policy() -> MetadataAuthorityPolicy {
        MetadataAuthorityPolicy {
            schema_version: 1,
            repository: "owner/repo".to_owned(),
            base_ref: "main".to_owned(),
            allowed_paths: vec!["docs/**".to_owned()],
            required_checks: vec!["docs@app:15368".to_owned(), "CodeQL@app:15368".to_owned()],
        }
    }

    fn observation() -> MetadataAuthorityObservation {
        MetadataAuthorityObservation {
            repository: "OWNER/REPO".to_owned(),
            pull_request: 7,
            base_ref: "main".to_owned(),
            base_sha: "a".repeat(40),
            protected_ref_sha: "a".repeat(40),
            protected_ref_protected: true,
            remote_head_sha: "b".repeat(40),
            remote_tree_sha: "c".repeat(40),
            local_head_sha: "b".repeat(40),
            local_tree_sha: "c".repeat(40),
            local_merge_base_sha: "a".repeat(40),
            remote_merge_base_sha: "a".repeat(40),
            merge_base_is_ancestor: true,
            checkout_clean: true,
            remote_changed_paths: vec!["docs/validation/fast.md".to_owned()],
            remote_changed_paths_complete: true,
            local_changed_paths: vec!["docs/validation/fast.md".to_owned()],
            local_changed_paths_complete: true,
            hosted_checks: vec![
                HostedCheckObservation {
                    name: "CodeQL".to_owned(),
                    status: "COMPLETED".to_owned(),
                    conclusion: "SUCCESS".to_owned(),
                    producer: "app:15368".to_owned(),
                },
                HostedCheckObservation {
                    name: "docs".to_owned(),
                    status: "COMPLETED".to_owned(),
                    conclusion: "SUCCESS".to_owned(),
                    producer: "app:15368".to_owned(),
                },
            ],
        }
    }

    #[test]
    fn exact_metadata_observation_authorizes_immutable_receipt() {
        let MetadataAuthorityDecision::Authorized(receipt) =
            authorize_metadata_only(&policy(), &observation(), "mac")
        else {
            panic!("expected metadata authority");
        };
        assert_eq!(receipt.changed_paths, vec!["docs/validation/fast.md"]);
        assert_eq!(
            receipt.required_checks,
            vec!["CodeQL@app:15368", "docs@app:15368"]
        );
        assert!(
            verify_receipt(
                &receipt,
                &policy(),
                "owner/repo",
                7,
                "main",
                &"b".repeat(40),
                &"c".repeat(40),
                "mac",
            )
            .is_ok()
        );
    }

    #[test]
    fn unknown_path_and_identity_or_check_drift_preserve_full() {
        let mut cases = Vec::new();
        let mut value = observation();
        value.remote_changed_paths = vec!["src/lib.rs".to_owned()];
        value.local_changed_paths = value.remote_changed_paths.clone();
        cases.push(value);
        let mut value = observation();
        value.local_tree_sha = "d".repeat(40);
        cases.push(value);
        let mut value = observation();
        value.hosted_checks[0].conclusion = "FAILURE".to_owned();
        cases.push(value);
        let mut value = observation();
        value.hosted_checks[0].conclusion = "SKIPPED".to_owned();
        cases.push(value);
        let mut value = observation();
        value.hosted_checks[0].conclusion = "NEUTRAL".to_owned();
        cases.push(value);
        let mut value = observation();
        value.hosted_checks.pop();
        cases.push(value);
        let mut value = observation();
        value.hosted_checks.push(value.hosted_checks[0].clone());
        cases.push(value);
        let mut value = observation();
        value.protected_ref_protected = false;
        cases.push(value);
        let mut value = observation();
        value.protected_ref_sha = "d".repeat(40);
        cases.push(value);
        let mut value = observation();
        value.remote_merge_base_sha = "d".repeat(40);
        cases.push(value);
        for case in cases {
            assert!(matches!(
                authorize_metadata_only(&policy(), &case, "mac"),
                MetadataAuthorityDecision::Full { .. }
            ));
        }
    }

    #[test]
    fn single_star_allowlist_does_not_cross_directory_boundaries() {
        let mut narrow = policy();
        narrow.allowed_paths = vec!["metadata/*.json".to_owned()];
        let mut nested = observation();
        nested.remote_changed_paths = vec!["metadata/private/native/config.json".to_owned()];
        nested.local_changed_paths = nested.remote_changed_paths.clone();
        assert!(matches!(
            authorize_metadata_only(&narrow, &nested, "mac"),
            MetadataAuthorityDecision::Full { .. }
        ));
    }

    #[test]
    fn incomplete_or_ambiguous_path_observation_preserves_full() {
        let mut incomplete = observation();
        incomplete.remote_changed_paths_complete = false;
        assert!(matches!(
            authorize_metadata_only(&policy(), &incomplete, "mac"),
            MetadataAuthorityDecision::Full { .. }
        ));
        let mut duplicate = observation();
        duplicate
            .local_changed_paths
            .push("docs/validation/fast.md".to_owned());
        assert!(matches!(
            authorize_metadata_only(&policy(), &duplicate, "mac"),
            MetadataAuthorityDecision::Full { .. }
        ));
    }

    #[test]
    fn receipt_rejects_policy_or_identity_mutation() {
        let MetadataAuthorityDecision::Authorized(receipt) =
            authorize_metadata_only(&policy(), &observation(), "mac")
        else {
            panic!("expected metadata authority");
        };
        let mut changed = policy();
        changed.allowed_paths = vec!["**".to_owned()];
        assert!(
            verify_receipt(
                &receipt,
                &changed,
                "owner/repo",
                7,
                "main",
                &"b".repeat(40),
                &"c".repeat(40),
                "mac",
            )
            .is_err()
        );
        let mut changed_receipt = receipt.clone();
        changed_receipt.changed_paths = vec!["src/lib.rs".to_owned()];
        changed_receipt.changed_paths_digest = digest_json(&changed_receipt.changed_paths);
        assert!(
            verify_receipt(
                &changed_receipt,
                &policy(),
                "owner/repo",
                7,
                "main",
                &"b".repeat(40),
                &"c".repeat(40),
                "mac",
            )
            .is_err()
        );
        assert!(
            verify_receipt(
                &receipt,
                &policy(),
                "owner/repo",
                7,
                "main",
                &"e".repeat(40),
                &"c".repeat(40),
                "mac",
            )
            .is_err()
        );
    }

    #[test]
    fn only_machine_global_config_can_activate_repository_policy() {
        let sandbox = tempfile::tempdir().expect("tempdir");
        let global = sandbox.path().join("global");
        let project = sandbox.path().join("project");
        std::fs::create_dir_all(&global).expect("global dir");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(
            global.join("config.toml"),
            r#"
[metadata_authority]
mode = "authoritative"
[metadata_authority.repositories."owner/repo"]
schema_version = 1
repository = "owner/repo"
base_ref = "main"
allowed_paths = ["docs/**"]
required_checks = ["docs@app:15368", "CodeQL@app:15368"]
"#,
        )
        .expect("global config");
        std::fs::write(
            project.join("config.toml"),
            r#"
[metadata_authority.repositories."owner/repo"]
allowed_paths = ["**"]
"#,
        )
        .expect("hostile project overlay");
        let config =
            LoadedConfig::load(Some(global), Some(project), None, LocalOverlaySource::None)
                .expect("merged config");
        let policy = trusted_policy(&config, "OWNER/REPO")
            .expect("trusted policy")
            .expect("active policy");
        assert_eq!(policy.allowed_paths, vec!["docs/**"]);
    }

    #[test]
    fn universal_path_policy_is_rejected() {
        let mut value = policy();
        value.allowed_paths = vec!["**".to_owned()];
        assert!(validate_policy(&value).is_err());
    }

    #[test]
    fn parses_check_runs_and_status_contexts_without_promoting_pending() {
        let checks = parse_hosted_checks(&[
            serde_json::json!({"name":"docs","status":"COMPLETED","conclusion":"SUCCESS","checkSuite":{"app":{"databaseId":15368,"slug":"github-actions"}}}),
            serde_json::json!({"context":"legacy","state":"SUCCESS","creator":{"__typename":"Bot","databaseId":7,"login":"bot"}}),
            serde_json::json!({"context":"waiting","state":"PENDING","creator":{"__typename":"Bot","databaseId":7,"login":"bot"}}),
        ]);
        assert_eq!(checks[0].status, "COMPLETED");
        assert_eq!(checks[1].conclusion, "SUCCESS");
        assert_eq!(checks[2].status, "PENDING");
        assert!(verify_live_checks(&policy(), &checks).is_err());
    }
}
