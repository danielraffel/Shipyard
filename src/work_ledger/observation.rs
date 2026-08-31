//! Read-only scheduler projections from the canonical work ledger.

use super::{RepoPolicy, WorkLedger, WorkLedgerResult, verify_integrity, verify_supported_schema};

/// One exact pull-request head that the shadow scheduler may observe.
///
/// Multiple lifecycle records can refer to the same immutable PR head. The
/// projection collapses them so a catch-up pass spends one GitHub request per
/// exact `(repo, pr, head)` rather than one request per imported record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShadowPrTarget {
    /// Source provider for the immutable repository identity, when projection-bound.
    pub repository_provider: Option<String>,
    /// Provider-scoped immutable repository identity, when projection-bound.
    pub repository_id: Option<String>,
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
    /// A repository is deliberately invisible to the scheduler until it has
    /// an explicit policy row. This keeps Pulp, Forge, and Vellum easy to
    /// revise independently and prevents an imported repository from silently
    /// inheriting another project's platform or blocking policy.
    pub fn shadow_pr_targets(&self) -> WorkLedgerResult<Vec<ShadowPrTarget>> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;

        let mut statement = connection.prepare(
            "SELECT binding.repository_provider, binding.repository_id,
                    work_items.repo, work_items.pr, work_items.head_sha, COUNT(*),
                    repo_policies.primary_platform, repo_policies.compatibility_mode,
                    repo_policies.compatibility_lanes_json, repo_policies.blocking_rule,
                    repo_policies.declared_dependency_lanes_json, repo_policies.revision
             FROM work_items
             JOIN repo_policies ON repo_policies.repo = work_items.repo
             LEFT JOIN workstream_projection_bindings binding
               ON binding.work_item_id = work_items.id
             WHERE work_items.phase IN (
                       'published', 'ready', 'managed', 'waiting', 'actionable',
                       'dispatching', 'agent_owned_repair', 'returned'
                   )
               AND work_items.pr IS NOT NULL AND work_items.pr > 0
               AND work_items.head_sha IS NOT NULL
               AND binding.repository_provider = 'github.com'
               AND binding.repository_id IS NOT NULL
             GROUP BY binding.repository_provider, binding.repository_id,
                      work_items.repo, work_items.pr, work_items.head_sha,
                      repo_policies.primary_platform, repo_policies.compatibility_mode,
                      repo_policies.compatibility_lanes_json, repo_policies.blocking_rule,
                      repo_policies.declared_dependency_lanes_json, repo_policies.revision
             ORDER BY work_items.repo, work_items.pr, work_items.head_sha",
        )?;
        let rows = statement.query_map([], |row| {
            let repo = row.get::<_, String>(2)?;
            Ok(ShadowPrTarget {
                repository_provider: row.get(0)?,
                repository_id: row.get(1)?,
                policy: RepoPolicy {
                    repo: repo.clone(),
                    primary_platform: row.get(6)?,
                    compatibility_mode: row.get(7)?,
                    compatibility_lanes: serde_json::from_str(&row.get::<_, String>(8)?).map_err(
                        |error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        },
                    )?,
                    blocking_rule: row.get(9)?,
                    declared_dependency_lanes: serde_json::from_str(&row.get::<_, String>(10)?)
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                10,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                    revision: row.get(11)?,
                },
                repo,
                pr: row.get(3)?,
                head_sha: row.get(4)?,
                work_items: row.get(5)?,
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
        RepoPolicy {
            repo: repo.to_owned(),
            primary_platform: "macos".to_owned(),
            compatibility_mode: "independent".to_owned(),
            compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
            blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
            declared_dependency_lanes: Vec::new(),
            revision: 0,
        }
    }

    #[test]
    fn targets_are_exact_deduplicated_and_require_explicit_repo_policy() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        let head = "a".repeat(40);
        let pulp_ship = candidate("generous-corp/pulp", 42, &head, "pulp-ship");
        let pulp_handoff = candidate("generous-corp/pulp", 42, &head, "pulp-handoff");
        ledger
            .import(&[
                pulp_ship.clone(),
                pulp_handoff.clone(),
                candidate("other/repo", 7, &"b".repeat(40), "other"),
            ])
            .expect("import");
        for (candidate, handle) in [(&pulp_ship, "GEN-42"), (&pulp_handoff, "GEN-43")] {
            ledger
                .bind_workstream_projection(
                    &candidate.work_id,
                    handle,
                    &digest(format!("plan:{handle}").as_bytes()),
                    1,
                    1,
                    1,
                    1,
                    "github.com",
                    "R_pulp",
                    "generous-corp/pulp",
                    &head,
                )
                .expect("authenticated projection binding");
        }
        let connection = ledger.connect_read_write().expect("test connection");
        connection
            .execute(
                "UPDATE work_items SET phase = 'managed' WHERE repo = 'generous-corp/pulp'",
                [],
            )
            .expect("native test phase");
        ledger
            .set_repo_policy(&macos_policy("generous-corp/pulp"), 0)
            .expect("policy");

        let targets = ledger.shadow_pr_targets().expect("targets");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].repo, "generous-corp/pulp");
        assert_eq!(targets[0].pr, 42);
        assert_eq!(targets[0].head_sha, head);
        assert_eq!(targets[0].work_items, 2);
        assert_eq!(
            targets[0].repository_provider.as_deref(),
            Some("github.com")
        );
        assert_eq!(targets[0].repository_id.as_deref(), Some("R_pulp"));
        assert_eq!(targets[0].policy.primary_platform, "macos");
        assert_eq!(targets[0].policy.compatibility_mode, "independent");
        assert!(
            !targets[0]
                .policy
                .compatibility_lane_may_block("linux", false)
        );
        assert!(
            targets[0]
                .policy
                .compatibility_lane_may_block("linux", true)
        );
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
        ledger
            .set_repo_policy(&macos_policy("generous-corp/pulp"), 0)
            .expect("policy");
        assert!(ledger.shadow_pr_targets().expect("targets").is_empty());
    }
}
