//! Revision-fenced repository scheduling policy.

use super::{
    OptionalExtension, Serialize, TransactionBehavior, Utc, WorkLedger, WorkLedgerError,
    WorkLedgerResult, configure_durable, is_canonical_repo_slug, params, validate_token,
    verify_integrity, verify_supported_schema,
};

const SUPPORTED_POLICY_PLATFORMS: &[&str] = &["linux", "macos", "windows"];

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
    /// Evaluate whether one compatibility lane may block the primary lane.
    #[must_use]
    pub fn compatibility_lane_may_block(
        &self,
        lane: &str,
        shared_integrity_evidence: bool,
    ) -> bool {
        if self
            .compatibility_lanes
            .binary_search_by(|known| known.as_str().cmp(lane))
            .is_err()
        {
            return false;
        }
        match self.blocking_rule.as_str() {
            "all" => self.compatibility_mode == "blocking",
            "declared_dependency_or_shared_integrity" => {
                self.declared_dependency_lanes
                    .iter()
                    .any(|declared| declared == lane)
                    || shared_integrity_evidence
            }
            _ => false,
        }
    }
}

impl WorkLedger {
    /// List repository policies in deterministic repository order.
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
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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
        let changed = if expected_revision == 0 {
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
        let matches = match current {
            None => expected_revision == 0,
            Some(revision) => revision == expected_revision && expected_revision != 0,
        };
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
