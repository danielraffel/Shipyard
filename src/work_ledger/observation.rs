//! Read-only scheduler projections from the canonical work ledger.

use super::{RepoPolicy, WorkLedger, WorkLedgerResult, verify_integrity, verify_supported_schema};

/// One exact pull-request head that the shadow scheduler may observe.
///
/// Multiple lifecycle records can refer to the same immutable PR head. The
/// projection collapses them so a catch-up pass spends one GitHub request per
/// exact `(repo, pr, head)` rather than one request per imported record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShadowPrTarget {
    /// Canonical lowercase `owner/repo` identity.
    pub repo: String,
    /// Pull-request number.
    pub pr: u64,
    /// Exact immutable head recorded by the ledger.
    pub head_sha: String,
    /// Number of nonterminal work items represented by this target.
    pub work_items: u64,
    /// Exact revision-fenced repository policy used by later decisions.
    pub policy: RepoPolicy,
}

impl WorkLedger {
    /// Enumerate policy-covered nonterminal PR heads without mutating storage.
    ///
    /// Pulp, Forge, and Vellum use the built-in macOS-first policy until an
    /// explicit revision-fenced row overrides it. Every other repository is
    /// deliberately invisible until it has an explicit policy, preventing an
    /// imported repository from inheriting another project's priorities.
    pub fn shadow_pr_targets(&self) -> WorkLedgerResult<Vec<ShadowPrTarget>> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;

        let mut statement = connection.prepare(
            "SELECT work_items.repo, work_items.pr, work_items.head_sha, COUNT(*),
                    COALESCE(repo_policies.primary_platform, 'macos'),
                    COALESCE(repo_policies.compatibility_mode, 'independent'),
                    COALESCE(repo_policies.compatibility_lanes_json, '[\"linux\",\"windows\"]'),
                    COALESCE(repo_policies.blocking_rule,
                             'declared_dependency_or_shared_integrity'),
                    COALESCE(repo_policies.declared_dependency_lanes_json, '[]'),
                    COALESCE(repo_policies.revision, 1)
             FROM work_items
             LEFT JOIN repo_policies ON repo_policies.repo = work_items.repo
             WHERE work_items.phase IN (
                       'published', 'ready', 'managed', 'waiting', 'actionable',
                       'dispatching', 'agent_owned_repair', 'returned'
                   )
               AND work_items.pr IS NOT NULL AND work_items.pr > 0
               AND work_items.head_sha IS NOT NULL
               AND (repo_policies.repo IS NOT NULL OR work_items.repo IN (
                       'generous-corp/forge', 'generous-corp/pulp', 'generous-corp/vellum'
                   ))
             GROUP BY work_items.repo, work_items.pr, work_items.head_sha,
                      repo_policies.primary_platform, repo_policies.compatibility_mode,
                      repo_policies.compatibility_lanes_json, repo_policies.blocking_rule,
                      repo_policies.declared_dependency_lanes_json, repo_policies.revision
             ORDER BY work_items.repo, work_items.pr, work_items.head_sha",
        )?;
        let rows = statement.query_map([], |row| {
            let repo = row.get::<_, String>(0)?;
            Ok(ShadowPrTarget {
                policy: RepoPolicy {
                    repo: repo.clone(),
                    primary_platform: row.get(4)?,
                    compatibility_mode: row.get(5)?,
                    compatibility_lanes: serde_json::from_str(&row.get::<_, String>(6)?).map_err(
                        |error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                6,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        },
                    )?,
                    blocking_rule: row.get(7)?,
                    declared_dependency_lanes: serde_json::from_str(&row.get::<_, String>(8)?)
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                    revision: row.get(9)?,
                },
                repo,
                pr: row.get(1)?,
                head_sha: row.get(2)?,
                work_items: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_ledger::{ImportCandidate, LifecycleState, digest, opaque_ref};

    fn candidate(repo: &str, pr: u64, head_sha: &str, source: &str) -> ImportCandidate {
        ImportCandidate {
            work_id: opaque_ref("wi", source),
            kind: "ship_state".to_owned(),
            repo: Some(repo.to_owned()),
            pr: Some(pr),
            head_sha: Some(head_sha.to_owned()),
            base_ref: Some("main".to_owned()),
            goal_id: None,
            goal_generation: 1,
            lane: Some("macos".to_owned()),
            role: "root".to_owned(),
            owner_id: None,
            owner_generation: 1,
            terminal_adapter: None,
            agent_adapter: None,
            provider_adapter: None,
            coordinator_route_ref: None,
            repair_route_ref: None,
            pr_truth: "unknown".to_owned(),
            acceptance_truth: "unknown".to_owned(),
            continuation_truth: "unknown".to_owned(),
            phase: LifecycleState::ShadowImported.as_str().to_owned(),
            source_ref: opaque_ref("src", source),
            content_digest: digest(source.as_bytes()),
            source_updated_at: None,
        }
    }

    fn macos_policy(repo: &str) -> RepoPolicy {
        RepoPolicy::macos_first_default(repo).expect("built-in policy")
    }

    #[test]
    fn macos_first_defaults_are_exact_deduplicated_and_other_repos_require_policy() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        let head = "a".repeat(40);
        ledger
            .import(&[
                candidate("generous-corp/pulp", 42, &head, "pulp-ship"),
                candidate("generous-corp/pulp", 42, &head, "pulp-handoff"),
                candidate("other/repo", 7, &"b".repeat(40), "other"),
            ])
            .expect("import");
        let connection = ledger.connect_read_write().expect("test connection");
        connection
            .execute(
                "UPDATE work_items SET phase = 'managed' WHERE repo = 'generous-corp/pulp'",
                [],
            )
            .expect("native test phase");
        let targets = ledger.shadow_pr_targets().expect("targets");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].repo, "generous-corp/pulp");
        assert_eq!(targets[0].pr, 42);
        assert_eq!(targets[0].head_sha, head);
        assert_eq!(targets[0].work_items, 2);
        assert_eq!(
            targets[0].policy,
            RepoPolicy::macos_first_default("generous-corp/pulp").expect("default policy")
        );
        let failure = targets[0]
            .policy
            .classify_compatibility_failure("linux", 1, None)
            .expect("routine compatibility failure");
        assert!(!failure.blocks_primary);
        assert!(!failure.ci_rerun_allowed);
        assert_eq!(failure.model_calls, 0);
    }

    #[test]
    fn inert_shadow_imports_are_not_scheduler_targets() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        ledger
            .import(&[candidate(
                "generous-corp/pulp",
                42,
                &"a".repeat(40),
                "archived-import",
            )])
            .expect("import");
        assert!(ledger.shadow_pr_targets().expect("targets").is_empty());
    }

    #[test]
    fn explicit_policy_overrides_one_builtin_without_changing_the_others() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        let mut forge = macos_policy("generous-corp/forge");
        forge.primary_platform = "linux".to_owned();
        forge.compatibility_lanes = vec!["macos".to_owned(), "windows".to_owned()];
        ledger.set_repo_policy(&forge, 1).expect("override");
        ledger
            .import(&[
                candidate("generous-corp/forge", 1, &"a".repeat(40), "forge"),
                candidate("generous-corp/vellum", 2, &"b".repeat(40), "vellum"),
            ])
            .expect("import");
        let connection = ledger.connect_read_write().expect("test connection");
        connection
            .execute("UPDATE work_items SET phase = 'managed'", [])
            .expect("native test phase");

        let targets = ledger.shadow_pr_targets().expect("targets");

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].policy.primary_platform, "linux");
        assert_eq!(targets[0].policy.revision, 2);
        assert_eq!(targets[1].policy.primary_platform, "macos");
        assert_eq!(targets[1].policy.revision, 1);
    }
}
