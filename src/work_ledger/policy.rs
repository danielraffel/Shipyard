//! Revision-fenced repository scheduling policy.

use super::{
    OptionalExtension, Serialize, TransactionBehavior, Utc, WorkLedger, WorkLedgerError,
    WorkLedgerResult, configure_durable, is_canonical_repo_slug, params, validate_token,
    verify_integrity, verify_supported_schema,
};

const SUPPORTED_POLICY_PLATFORMS: &[&str] = &["linux", "macos", "windows"];
const MACOS_FIRST_REPOSITORIES: &[&str] = &[
    "generous-corp/forge",
    "generous-corp/pulp",
    "generous-corp/vellum",
];
const BUILTIN_POLICY_REVISION: u64 = 1;

/// Reviewed evidence that a compatibility failure also affects the primary lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformEscalationKind {
    /// A shared persisted artifact or state format is invalid across lanes.
    SharedPersistedData,
    /// The same source fails to compile on more than one platform family.
    CrossPlatformCompilation,
    /// The same correctness invariant fails on more than one platform family.
    CrossPlatformCorrectness,
}

/// Content-addressed evidence authorizing a cross-lane escalation review.
#[allow(dead_code)] // Activated only after a protected receipt verifier lands.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(crate) struct PlatformEscalationEvidence {
    /// Narrow reason that the compatibility failure is relevant to the primary lane.
    pub(crate) kind: PlatformEscalationKind,
    /// SHA-256 of the reviewed evidence receipt; raw logs are never embedded.
    pub(crate) evidence_digest: String,
}

/// Zero-model disposition for one failed compatibility platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityFailureDisposition {
    /// Retain the failure for asynchronous repair without blocking the primary lane.
    CapturedAsynchronously,
    /// Surface claimed shared evidence for review without granting block authority.
    SharedEvidenceReviewRequired,
}

/// Typed, non-executing classification of one compatibility-lane failure.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompatibilityFailureClassification {
    /// Canonical repository whose policy produced the decision.
    pub repo: String,
    /// Exact repository-policy revision.
    pub policy_revision: u64,
    /// Primary platform protected from routine compatibility failures.
    pub primary_platform: String,
    /// Failed compatibility platform.
    pub lane: String,
    /// Number of failed checks represented by this classification.
    pub failed_checks: u64,
    /// Deterministic disposition.
    pub disposition: CompatibilityFailureDisposition,
    /// Always false until a later protected receipt verifier grants authority.
    pub blocks_primary: bool,
    /// Routine classification never authorizes a CI rerun.
    pub ci_rerun_allowed: bool,
    /// Routine classification never invokes a model.
    pub model_calls: u64,
    /// Reviewed escalation kind, absent on the routine asynchronous path.
    pub escalation_kind: Option<PlatformEscalationKind>,
    /// Digest of reviewed evidence, absent on the routine asynchronous path.
    pub escalation_evidence_digest: Option<String>,
}

/// Revision-fenced repository scheduling policy stored in the shadow ledger.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepoPolicy {
    /// Canonical lowercase owner/repository slug.
    pub repo: String,
    /// Preferred platform lane, such as `macos`.
    pub primary_platform: String,
    /// Whether compatibility lanes are independent or globally blocking.
    pub compatibility_mode: String,
    /// Complete known compatibility-lane inventory for this repository.
    pub compatibility_lanes: Vec<String>,
    /// Exact rule that permits a compatibility lane to block another lane.
    pub blocking_rule: String,
    /// Compatibility lanes with an explicitly declared artifact dependency.
    pub declared_dependency_lanes: Vec<String>,
    /// Monotonic policy revision.
    pub revision: u64,
}

impl RepoPolicy {
    /// Return the built-in macOS-first shadow policy for Pulp, Forge, or Vellum.
    ///
    /// An explicit revision-fenced policy row always overrides this default, so
    /// each repository can change independently without changing fleet policy.
    #[must_use]
    pub fn macos_first_default(repo: &str) -> Option<Self> {
        MACOS_FIRST_REPOSITORIES
            .binary_search(&repo)
            .ok()
            .map(|_| Self {
                repo: repo.to_owned(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                revision: BUILTIN_POLICY_REVISION,
            })
    }

    /// Classify one compatibility-platform failure without dispatching work.
    ///
    /// Missing evidence always takes the asynchronous path. The three typed
    /// shared-evidence classes surface a review candidate, but remain
    /// non-blocking until a later protected verifier authenticates the receipt.
    #[allow(dead_code)] // Activated only after a protected receipt verifier lands.
    pub(crate) fn classify_compatibility_failure(
        &self,
        lane: &str,
        failed_checks: u64,
        escalation: Option<&PlatformEscalationEvidence>,
    ) -> WorkLedgerResult<CompatibilityFailureClassification> {
        let mut classification = self
            .capture_compatibility_failure(lane, failed_checks)
            .ok_or_else(|| {
                WorkLedgerError::Refused(
                    "compatibility failure requires a known lane and positive failed-check count"
                        .to_owned(),
                )
            })?;
        if let Some(evidence) = escalation {
            validate_evidence_digest(&evidence.evidence_digest)?;
            classification.disposition =
                CompatibilityFailureDisposition::SharedEvidenceReviewRequired;
            classification.escalation_kind = Some(evidence.kind);
            classification.escalation_evidence_digest = Some(evidence.evidence_digest.clone());
        }
        Ok(classification)
    }

    pub(crate) fn capture_compatibility_failure(
        &self,
        lane: &str,
        failed_checks: u64,
    ) -> Option<CompatibilityFailureClassification> {
        if failed_checks == 0
            || self
                .compatibility_lanes
                .binary_search_by(|known| known.as_str().cmp(lane))
                .is_err()
        {
            return None;
        }
        Some(CompatibilityFailureClassification {
            repo: self.repo.clone(),
            policy_revision: self.revision,
            primary_platform: self.primary_platform.clone(),
            lane: lane.to_owned(),
            failed_checks,
            disposition: CompatibilityFailureDisposition::CapturedAsynchronously,
            blocks_primary: false,
            ci_rerun_allowed: false,
            model_calls: 0,
            escalation_kind: None,
            escalation_evidence_digest: None,
        })
    }
}

fn validate_evidence_digest(digest: &str) -> WorkLedgerResult<()> {
    if digest.len() != 64
        || digest != digest.to_ascii_lowercase()
        || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(WorkLedgerError::Refused(
            "platform escalation evidence requires a lowercase SHA-256 digest".to_owned(),
        ));
    }
    Ok(())
}

impl WorkLedger {
    /// List effective repository policies in deterministic repository order.
    ///
    /// Explicit rows override the built-in Pulp, Forge, and Vellum defaults.
    pub fn repo_policies(&self) -> WorkLedgerResult<Vec<RepoPolicy>> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let mut statement = connection.prepare(
            "SELECT repo, primary_platform, compatibility_mode, compatibility_lanes_json,
                    blocking_rule, declared_dependency_lanes_json, revision
             FROM repo_policies ORDER BY repo",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(RepoPolicy {
                repo: row.get(0)?,
                primary_platform: row.get(1)?,
                compatibility_mode: row.get(2)?,
                compatibility_lanes: serde_json::from_str(&row.get::<_, String>(3)?).map_err(
                    |error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    },
                )?,
                blocking_rule: row.get(4)?,
                declared_dependency_lanes: serde_json::from_str(&row.get::<_, String>(5)?)
                    .map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                revision: row.get(6)?,
            })
        })?;
        let mut policies = default_repo_policies()
            .into_iter()
            .map(|policy| (policy.repo.clone(), policy))
            .collect::<std::collections::BTreeMap<_, _>>();
        for policy in rows.collect::<Result<Vec<_>, _>>()? {
            policies.insert(policy.repo.clone(), policy);
        }
        Ok(policies.into_values().collect())
    }

    /// Insert or revise one repository policy under an exact revision fence.
    pub fn set_repo_policy(
        &self,
        policy: &RepoPolicy,
        expected_revision: u64,
    ) -> WorkLedgerResult<RepoPolicy> {
        validate_repo_policy(policy, expected_revision)?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next_revision = expected_revision + 1;
        let current: Option<u64> = transaction
            .query_row(
                "SELECT revision FROM repo_policies WHERE repo = ?1",
                [&policy.repo],
                |row| row.get(0),
            )
            .optional()?;
        let insert = current.is_none() && expected_revision == absent_policy_revision(&policy.repo);
        let update = current == Some(expected_revision);
        if !insert && !update {
            return Err(WorkLedgerError::Refused(
                "repository policy revision no longer matches".to_owned(),
            ));
        }
        let changed = if insert {
            transaction.execute(
                "INSERT OR IGNORE INTO repo_policies
                 (repo, primary_platform, compatibility_mode, compatibility_lanes_json,
                  blocking_rule, declared_dependency_lanes_json, revision, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    policy.repo,
                    policy.primary_platform,
                    policy.compatibility_mode,
                    serde_json::to_string(&policy.compatibility_lanes).map_err(|_| {
                        WorkLedgerError::Refused(
                            "repository compatibility lanes cannot be serialized".to_owned(),
                        )
                    })?,
                    policy.blocking_rule,
                    serde_json::to_string(&policy.declared_dependency_lanes).map_err(|_| {
                        WorkLedgerError::Refused(
                            "repository dependency lanes cannot be serialized".to_owned(),
                        )
                    })?,
                    next_revision,
                    Utc::now().to_rfc3339(),
                ],
            )?
        } else {
            transaction.execute(
                "UPDATE repo_policies SET primary_platform = ?1, compatibility_mode = ?2,
                        compatibility_lanes_json = ?3, blocking_rule = ?4,
                        declared_dependency_lanes_json = ?5, revision = ?6, updated_at = ?7
                 WHERE repo = ?8 AND revision = ?9",
                params![
                    policy.primary_platform,
                    policy.compatibility_mode,
                    serde_json::to_string(&policy.compatibility_lanes).map_err(|_| {
                        WorkLedgerError::Refused(
                            "repository compatibility lanes cannot be serialized".to_owned(),
                        )
                    })?,
                    policy.blocking_rule,
                    serde_json::to_string(&policy.declared_dependency_lanes).map_err(|_| {
                        WorkLedgerError::Refused(
                            "repository dependency lanes cannot be serialized".to_owned(),
                        )
                    })?,
                    next_revision,
                    Utc::now().to_rfc3339(),
                    policy.repo,
                    expected_revision,
                ],
            )?
        };
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "repository policy revision no longer matches".to_owned(),
            ));
        }
        transaction.commit()?;
        let mut applied = policy.clone();
        applied.revision = next_revision;
        Ok(applied)
    }

    /// Validate and classify a repository policy update without mutating it.
    pub fn plan_repo_policy(
        &self,
        policy: &RepoPolicy,
        expected_revision: u64,
    ) -> WorkLedgerResult<RepoPolicy> {
        validate_repo_policy(policy, expected_revision)?;
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let current: Option<u64> = connection
            .query_row(
                "SELECT revision FROM repo_policies WHERE repo = ?1",
                [&policy.repo],
                |row| row.get(0),
            )
            .optional()?;
        let matches = current.map_or_else(
            || expected_revision == absent_policy_revision(&policy.repo),
            |revision| revision == expected_revision,
        );
        if !matches {
            return Err(WorkLedgerError::Refused(
                "repository policy revision no longer matches".to_owned(),
            ));
        }
        let mut planned = policy.clone();
        planned.revision = expected_revision + 1;
        Ok(planned)
    }
}

pub(crate) fn absent_policy_revision(repo: &str) -> u64 {
    if RepoPolicy::macos_first_default(repo).is_some() {
        BUILTIN_POLICY_REVISION
    } else {
        0
    }
}

pub(crate) fn default_repo_policies() -> Vec<RepoPolicy> {
    MACOS_FIRST_REPOSITORIES
        .iter()
        .filter_map(|repo| RepoPolicy::macos_first_default(repo))
        .collect()
}

pub(crate) fn validate_repo_policy(
    policy: &RepoPolicy,
    expected_revision: u64,
) -> WorkLedgerResult<()> {
    if !is_canonical_repo_slug(&policy.repo) {
        return Err(WorkLedgerError::Refused(
            "repository policy requires a lowercase owner/repo slug".to_owned(),
        ));
    }
    validate_token("primary_platform", &policy.primary_platform)?;
    if policy.primary_platform != policy.primary_platform.to_ascii_lowercase()
        || policy.primary_platform.trim() != policy.primary_platform
    {
        return Err(WorkLedgerError::Refused(
            "primary platform must be canonical lowercase".to_owned(),
        ));
    }
    if SUPPORTED_POLICY_PLATFORMS
        .binary_search(&policy.primary_platform.as_str())
        .is_err()
    {
        return Err(WorkLedgerError::Refused(
            "unsupported primary platform".to_owned(),
        ));
    }
    if policy.compatibility_lanes.len() > 32 {
        return Err(WorkLedgerError::Refused(
            "too many compatibility lanes".to_owned(),
        ));
    }
    validate_sorted_policy_lanes(
        "compatibility lanes",
        &policy.compatibility_lanes,
        &policy.primary_platform,
    )?;
    if policy.declared_dependency_lanes.len() > 32 {
        return Err(WorkLedgerError::Refused(
            "too many declared dependency lanes".to_owned(),
        ));
    }
    validate_sorted_policy_lanes(
        "declared dependency lanes",
        &policy.declared_dependency_lanes,
        &policy.primary_platform,
    )?;
    if policy
        .declared_dependency_lanes
        .iter()
        .any(|lane| policy.compatibility_lanes.binary_search(lane).is_err())
    {
        return Err(WorkLedgerError::Refused(
            "declared dependency lane is not in the compatibility-lane inventory".to_owned(),
        ));
    }
    if !matches!(
        policy.compatibility_mode.as_str(),
        "independent" | "blocking"
    ) {
        return Err(WorkLedgerError::Refused(
            "unsupported compatibility mode".to_owned(),
        ));
    }
    if !matches!(
        policy.blocking_rule.as_str(),
        "declared_dependency_or_shared_integrity" | "all"
    ) {
        return Err(WorkLedgerError::Refused(
            "unsupported compatibility blocking rule".to_owned(),
        ));
    }
    if policy.revision != expected_revision {
        return Err(WorkLedgerError::Refused(
            "planned policy revision does not match expected revision".to_owned(),
        ));
    }
    if expected_revision == u64::MAX {
        return Err(WorkLedgerError::Refused(
            "repository policy revision is exhausted".to_owned(),
        ));
    }
    Ok(())
}

fn validate_sorted_policy_lanes(
    label: &str,
    lanes: &[String],
    primary_platform: &str,
) -> WorkLedgerResult<()> {
    let mut prior: Option<&String> = None;
    for lane in lanes {
        validate_token(label, lane)?;
        if lane != &lane.to_ascii_lowercase()
            || lane.trim() != lane
            || lane == primary_platform
            || SUPPORTED_POLICY_PLATFORMS
                .binary_search(&lane.as_str())
                .is_err()
            || prior.is_some_and(|value| value >= lane)
        {
            return Err(WorkLedgerError::Refused(format!(
                "{label} must be unique sorted lowercase non-primary lanes"
            )));
        }
        prior = Some(lane);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_pulp_forge_and_vellum_receive_macos_first_defaults() {
        for repo in MACOS_FIRST_REPOSITORIES {
            let policy = RepoPolicy::macos_first_default(repo).expect("default policy");
            assert_eq!(policy.repo, *repo);
            assert_eq!(policy.primary_platform, "macos");
            assert_eq!(policy.compatibility_mode, "independent");
            assert_eq!(policy.compatibility_lanes, ["linux", "windows"]);
            assert!(policy.declared_dependency_lanes.is_empty());
            assert_eq!(policy.revision, BUILTIN_POLICY_REVISION);
        }
        assert!(RepoPolicy::macos_first_default("other/repo").is_none());
        assert!(RepoPolicy::macos_first_default("Generous-Corp/Pulp").is_none());
    }

    #[test]
    fn every_reviewed_escalation_kind_is_explicit_and_non_rerunning() {
        let policy = RepoPolicy::macos_first_default("generous-corp/pulp").expect("policy");
        for kind in [
            PlatformEscalationKind::SharedPersistedData,
            PlatformEscalationKind::CrossPlatformCompilation,
            PlatformEscalationKind::CrossPlatformCorrectness,
        ] {
            let evidence = PlatformEscalationEvidence {
                kind,
                evidence_digest: "b".repeat(64),
            };
            let classification = policy
                .classify_compatibility_failure("linux", 3, Some(&evidence))
                .expect("classification");
            assert!(!classification.blocks_primary);
            assert!(!classification.ci_rerun_allowed);
            assert_eq!(classification.model_calls, 0);
            assert_eq!(classification.escalation_kind, Some(kind));
        }
    }
}
