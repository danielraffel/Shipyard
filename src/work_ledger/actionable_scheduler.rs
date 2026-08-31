//! Exact-head steward decisions projected into the native lifecycle.
//!
//! This is the only producer allowed to turn an inert managed publication into
//! a continuation wake. The `actionable -> dispatching + outbox` boundary is
//! restart-completable and the latter transition is one `SQLite` transaction.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::{
    LifecycleState, WakeIntent, WorkLedger, WorkLedgerError, WorkLedgerResult, params,
    validate_digest,
};

pub(crate) const MAX_DISPATCH_PROBE_TARGETS: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DispatchProbeTargetRecord {
    pub(crate) target_key: String,
    pub(crate) repository_provider: String,
    pub(crate) repository_id: String,
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) head_sha: String,
    pub(crate) generation: u64,
    pub(crate) due_at: Option<String>,
    pub(crate) checkpoint_json: Vec<u8>,
}

/// A zero-model steward disposition for one exact managed PR head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Terminal dispositions are fed by the close/reconcile lane after integration.
pub(crate) enum NativeStewardDisposition {
    /// Required evidence is still pending.
    Waiting,
    /// Required evidence passed; deterministic merge stewardship continues.
    Passing,
    /// A semantic repair requires an agent.
    Actionable,
    /// The PR merged and no continuation is needed.
    Merged,
    /// The managed head was superseded or otherwise terminal.
    Superseded,
    /// Live GitHub evidence no longer describes the immutable managed head.
    StaleHead,
}

/// Stable result of applying one disposition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct NativeStewardApplyReport {
    pub(crate) matched: bool,
    pub(crate) changed: bool,
    pub(crate) wake_enqueued: bool,
    pub(crate) phase: Option<String>,
}

#[derive(Clone, Debug)]
struct NativeWork {
    id: String,
    base_ref: String,
    phase: String,
    work_generation: u64,
    owner_generation: u64,
    route_ref: String,
    profile_digest: String,
}

#[derive(Clone, Copy)]
struct NativeActionableAudit<'a> {
    expected_base_ref: &'a str,
    evidence_event: (&'a str, &'a str),
    identity_event: (&'a str, &'a str),
}

impl WorkLedger {
    pub(crate) fn replace_dispatch_probe_targets(
        &self,
        records: &[DispatchProbeTargetRecord],
    ) -> WorkLedgerResult<()> {
        if records.len() > MAX_DISPATCH_PROBE_TARGETS {
            return Err(WorkLedgerError::Refused(format!(
                "dispatch_probe_capacity_exhausted:{}>{MAX_DISPATCH_PROBE_TARGETS}",
                records.len()
            )));
        }
        if records
            .iter()
            .map(|record| record.target_key.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != records.len()
        {
            return Err(WorkLedgerError::Refused(
                "dispatch_probe_target_key_duplicated".to_owned(),
            ));
        }
        let mut connection = self.connect_read_write()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing = {
            let mut statement = transaction.prepare(
                "SELECT target_key, repository_provider, repository_id, repository, pull_request,
                        head_sha, generation, due_at, checkpoint_json
                   FROM dispatch_probe_targets",
            )?;
            statement
                .query_map([], |row| {
                    let record = DispatchProbeTargetRecord {
                        target_key: row.get(0)?,
                        repository_provider: row.get(1)?,
                        repository_id: row.get(2)?,
                        repository: row.get(3)?,
                        pull_request: row.get(4)?,
                        head_sha: row.get(5)?,
                        generation: row.get(6)?,
                        due_at: row.get(7)?,
                        checkpoint_json: row.get(8)?,
                    };
                    Ok((record.target_key.clone(), record))
                })?
                .collect::<Result<BTreeMap<_, _>, _>>()?
        };
        let desired_keys = records
            .iter()
            .map(|record| record.target_key.as_str())
            .collect::<BTreeSet<_>>();
        let now = chrono::Utc::now().to_rfc3339();
        for record in records {
            validate_dispatch_probe_record(record)?;
            if existing.get(&record.target_key) == Some(record) {
                continue;
            }
            transaction.execute(
                "INSERT INTO dispatch_probe_targets
                   (target_key, repository_provider, repository_id, repository, pull_request,
                    head_sha, generation, due_at, checkpoint_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(target_key) DO UPDATE SET
                   repository_provider = excluded.repository_provider,
                   repository_id = excluded.repository_id,
                   repository = excluded.repository,
                   pull_request = excluded.pull_request,
                   head_sha = excluded.head_sha,
                   generation = excluded.generation,
                   due_at = excluded.due_at,
                   checkpoint_json = excluded.checkpoint_json,
                   updated_at = excluded.updated_at",
                params![
                    record.target_key,
                    record.repository_provider,
                    record.repository_id,
                    record.repository,
                    record.pull_request,
                    record.head_sha,
                    record.generation,
                    record.due_at,
                    record.checkpoint_json,
                    now,
                ],
            )?;
        }
        for target_key in existing.keys() {
            if !desired_keys.contains(target_key.as_str()) {
                transaction.execute(
                    "DELETE FROM dispatch_probe_targets WHERE target_key = ?1",
                    [target_key],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn load_dispatch_probe_targets(
        &self,
    ) -> WorkLedgerResult<Vec<DispatchProbeTargetRecord>> {
        let connection = self.connect_read_only()?;
        let mut statement = connection.prepare(
            "SELECT target_key, repository_provider, repository_id, repository, pull_request,
                    head_sha, generation, due_at, checkpoint_json
               FROM dispatch_probe_targets
              ORDER BY target_key",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(DispatchProbeTargetRecord {
                target_key: row.get(0)?,
                repository_provider: row.get(1)?,
                repository_id: row.get(2)?,
                repository: row.get(3)?,
                pull_request: row.get(4)?,
                head_sha: row.get(5)?,
                generation: row.get(6)?,
                due_at: row.get(7)?,
                checkpoint_json: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Enumerate durable native handoffs that still require steward
    /// reconciliation. Unlike the GitHub shadow projection, this recovery
    /// inventory does not depend on repository polling policy being present.
    #[cfg(unix)]
    #[allow(clippy::type_complexity)] // Preserve the complete durable repository target key.
    pub(crate) fn native_steward_targets(
        &self,
    ) -> WorkLedgerResult<Vec<(Option<String>, Option<String>, String, u64, String)>> {
        let connection = self.connect_read_only()?;
        let mut statement = connection.prepare(
            "SELECT binding.repository_provider, binding.repository_id,
                    lower(work.repo), work.pr, lower(work.head_sha)
               FROM work_items work
               LEFT JOIN workstream_projection_bindings binding ON binding.work_item_id = work.id
              WHERE kind = 'terminal_handoff'
                AND phase IN ('managed', 'waiting', 'actionable', 'dispatching',
                              'agent_owned_repair', 'returned')
                AND pr IS NOT NULL AND pr > 0 AND head_sha IS NOT NULL
              ORDER BY lower(repo), pr, lower(head_sha)",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Resolve the exact base ref bound into one native target. The daemon
    /// never guesses `main` when invoking repository stewardship.
    #[cfg(unix)]
    #[allow(dead_code)] // Explicit compatibility boundary for legacy unbound callers.
    pub(crate) fn native_steward_base_ref(
        &self,
        repo: &str,
        pr: u64,
        head_sha: &str,
    ) -> WorkLedgerResult<Option<String>> {
        self.native_steward_base_ref_for_repository(None, None, repo, pr, head_sha)
    }

    #[cfg(unix)]
    pub(crate) fn native_steward_base_ref_for_repository(
        &self,
        repository_provider: Option<&str>,
        repository_id: Option<&str>,
        repo: &str,
        pr: u64,
        head_sha: &str,
    ) -> WorkLedgerResult<Option<String>> {
        validate_repository_identity(repository_provider, repository_id)?;
        let connection = self.connect_read_only()?;
        let mut statement = connection.prepare(
            "SELECT work.base_ref FROM work_items work
              LEFT JOIN workstream_projection_bindings binding ON binding.work_item_id = work.id
              WHERE work.kind = 'terminal_handoff' AND lower(work.repo) = lower(?1)
                AND work.pr = ?2 AND lower(work.head_sha) = lower(?3)
                AND phase IN ('managed', 'waiting', 'actionable', 'dispatching',
                              'agent_owned_repair', 'returned')",
        )?;
        if repository_provider.is_some() {
            let mut exact = connection.prepare(
                "SELECT work.base_ref FROM work_items work
                     JOIN workstream_projection_bindings binding ON binding.work_item_id = work.id
                    WHERE work.kind = 'terminal_handoff' AND lower(work.repo) = lower(?1)
                      AND work.pr = ?2 AND lower(work.head_sha) = lower(?3)
                      AND binding.repository_provider = ?4 AND binding.repository_id = ?5
                      AND work.phase IN ('managed', 'waiting', 'actionable', 'dispatching',
                                         'agent_owned_repair', 'returned')
                     ORDER BY work.id LIMIT 2",
            )?;
            let rows = exact
                .query_map(
                    params![repo, pr, head_sha, repository_provider, repository_id],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            return unique_base_ref(&rows);
        }
        let rows = statement
            .query_map(params![repo, pr, head_sha], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        unique_base_ref(&rows)
    }

    /// Apply a classified steward observation to one exact native publication.
    #[allow(dead_code)] // Explicit compatibility boundary for legacy unbound callers.
    pub(crate) fn apply_native_steward_disposition(
        &self,
        repo: &str,
        pr: u64,
        head_sha: &str,
        disposition: NativeStewardDisposition,
    ) -> WorkLedgerResult<NativeStewardApplyReport> {
        self.apply_native_steward_disposition_for_repository(
            None,
            None,
            repo,
            pr,
            head_sha,
            disposition,
        )
    }

    /// Publish one exact dispatch-wedge receipt through the existing native
    /// actionable transition and wake outbox. The evidence event, generation
    /// advance, and projection intent commit together; restart can therefore
    /// finish the ordinary `actionable -> dispatching` path without a second
    /// receipt store or a duplicate wake.
    #[allow(
        clippy::too_many_arguments,
        reason = "provider, immutable repository ID, exact head, and receipt digests are one authority fence"
    )]
    pub(crate) fn publish_dispatch_wedge(
        &self,
        repository_provider: Option<&str>,
        repository_id: Option<&str>,
        repo: &str,
        base_ref: &str,
        pr: u64,
        head_sha: &str,
        identity_digest: &str,
        evidence_digest: &str,
    ) -> WorkLedgerResult<NativeStewardApplyReport> {
        validate_digest("dispatch wedge identity", identity_digest)?;
        validate_digest("dispatch wedge evidence", evidence_digest)?;
        self.apply_native_steward_disposition_with(
            repository_provider,
            repository_id,
            repo,
            pr,
            head_sha,
            NativeStewardDisposition::Actionable,
            Some(NativeActionableAudit {
                expected_base_ref: base_ref,
                evidence_event: ("dispatch_wedge_detected", evidence_digest),
                identity_event: ("dispatch_wedge_identity", identity_digest),
            }),
            |_| {},
        )
    }

    pub(crate) fn apply_native_steward_disposition_for_repository(
        &self,
        repository_provider: Option<&str>,
        repository_id: Option<&str>,
        repo: &str,
        pr: u64,
        head_sha: &str,
        disposition: NativeStewardDisposition,
    ) -> WorkLedgerResult<NativeStewardApplyReport> {
        self.apply_native_steward_disposition_with(
            repository_provider,
            repository_id,
            repo,
            pr,
            head_sha,
            disposition,
            None,
            |_| {},
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // One exact identity fence and lifecycle reload path.
    fn apply_native_steward_disposition_with<F>(
        &self,
        repository_provider: Option<&str>,
        repository_id: Option<&str>,
        repo: &str,
        pr: u64,
        head_sha: &str,
        disposition: NativeStewardDisposition,
        actionable_audit: Option<NativeActionableAudit<'_>>,
        mut crash_hook: F,
    ) -> WorkLedgerResult<NativeStewardApplyReport>
    where
        F: FnMut(&str),
    {
        validate_target(repo, pr, head_sha)?;
        validate_repository_identity(repository_provider, repository_id)?;
        let Some(mut work) =
            self.native_work_for_target(repository_provider, repository_id, repo, pr, head_sha)?
        else {
            return Ok(NativeStewardApplyReport {
                matched: false,
                changed: false,
                wake_enqueued: false,
                phase: None,
            });
        };
        if actionable_audit.is_some_and(|audit| work.base_ref != audit.expected_base_ref) {
            return Err(WorkLedgerError::Refused(
                "native steward base ref no longer matches exact authority".to_owned(),
            ));
        }
        let mut changed = false;
        let mut wake_enqueued = false;

        match disposition {
            // Waiting/passing observations are deliberately state-preserving.
            // Advancing a generation here would invalidate the staged exact
            // route before a later actionable failure could consume it.
            NativeStewardDisposition::Waiting => {
                changed = self.stage_waiting_observation(
                    &work.id,
                    work.work_generation,
                    work.owner_generation,
                )?;
            }
            NativeStewardDisposition::Passing => {}
            NativeStewardDisposition::Merged
            | NativeStewardDisposition::Superseded
            | NativeStewardDisposition::StaleHead => {
                if matches!(
                    work.phase.as_str(),
                    "managed"
                        | "waiting"
                        | "actionable"
                        | "dispatching"
                        | "agent_owned_repair"
                        | "returned"
                ) {
                    let projection = match disposition {
                        NativeStewardDisposition::Merged => {
                            Some(super::projection_intents::ProjectionIntentKind::Merge)
                        }
                        NativeStewardDisposition::Superseded
                        | NativeStewardDisposition::StaleHead => {
                            Some(super::projection_intents::ProjectionIntentKind::ConfiguredClosure)
                        }
                        _ => unreachable!(),
                    };
                    let terminal_disposition = match disposition {
                        NativeStewardDisposition::Merged => "merged",
                        NativeStewardDisposition::Superseded => "superseded",
                        NativeStewardDisposition::StaleHead => "stale_head",
                        _ => unreachable!(),
                    };
                    let advanced = self.transition_with_wake_and_projection(
                        &work.id,
                        work.work_generation,
                        work.owner_generation,
                        LifecycleState::Terminal,
                        None,
                        projection,
                        Some(terminal_disposition),
                        None,
                        None,
                    )?;
                    changed = advanced;
                    work = self
                        .native_work_for_target(
                            repository_provider,
                            repository_id,
                            repo,
                            pr,
                            head_sha,
                        )?
                        .ok_or_else(|| {
                            WorkLedgerError::Refused(
                                "native work disappeared after terminal transition".to_owned(),
                            )
                        })?;
                }
            }
            NativeStewardDisposition::Actionable => {
                let actionable_audit_event = actionable_audit.map(|audit| audit.evidence_event);
                let actionable_identity_event = actionable_audit.map(|audit| audit.identity_event);
                if let Some((kind, payload_digest)) = actionable_identity_event
                    && !matches!(work.phase.as_str(), "managed" | "waiting")
                    && !self.has_actionable_audit_event(&work.id, kind, payload_digest)?
                {
                    return Err(WorkLedgerError::Refused(
                        "actionable work is not bound to this dispatch-wedge receipt".to_owned(),
                    ));
                }
                if matches!(work.phase.as_str(), "managed" | "waiting") {
                    let transition = self.transition_with_wake_and_projection(
                        &work.id,
                        work.work_generation,
                        work.owner_generation,
                        LifecycleState::Actionable,
                        None,
                        None,
                        None,
                        actionable_audit_event,
                        actionable_identity_event,
                    );
                    if transition.is_ok() {
                        changed = true;
                        crash_hook("after_actionable");
                    }
                    work = self
                        .native_work_for_target(
                            repository_provider,
                            repository_id,
                            repo,
                            pr,
                            head_sha,
                        )?
                        .ok_or_else(|| {
                            WorkLedgerError::Refused(
                                "native work disappeared after actionable transition".to_owned(),
                            )
                        })?;
                    if let Some((kind, payload_digest)) = actionable_identity_event
                        && !self.has_actionable_audit_event(&work.id, kind, payload_digest)?
                    {
                        return Err(WorkLedgerError::Refused(
                            "actionable work is not bound to this dispatch-wedge receipt"
                                .to_owned(),
                        ));
                    }
                    if transition.is_err()
                        && !matches!(
                            work.phase.as_str(),
                            "actionable"
                                | "dispatching"
                                | "agent_owned_repair"
                                | "returned"
                                | "terminal"
                        )
                    {
                        return transition.map(|_| unreachable!());
                    }
                }
                if work.phase == LifecycleState::Actionable.as_str() {
                    let wake = WakeIntent::new(
                        &work.id,
                        work.work_generation + 1,
                        work.owner_generation,
                        work.route_ref.clone(),
                        work.profile_digest.clone(),
                    )?;
                    let transition = self.transition_with_wake(
                        &work.id,
                        work.work_generation,
                        work.owner_generation,
                        LifecycleState::Dispatching,
                        Some(&wake),
                    );
                    if transition.is_ok() {
                        changed = true;
                        wake_enqueued = true;
                        crash_hook("after_dispatching");
                    }
                    work = self
                        .native_work_for_target(
                            repository_provider,
                            repository_id,
                            repo,
                            pr,
                            head_sha,
                        )?
                        .ok_or_else(|| {
                            WorkLedgerError::Refused(
                                "native work disappeared after dispatch transition".to_owned(),
                            )
                        })?;
                    if transition.is_err()
                        && !matches!(
                            work.phase.as_str(),
                            "dispatching" | "agent_owned_repair" | "returned" | "terminal"
                        )
                    {
                        return transition.map(|_| unreachable!());
                    }
                }
            }
        }

        Ok(NativeStewardApplyReport {
            matched: true,
            changed,
            wake_enqueued,
            phase: Some(work.phase),
        })
    }

    fn native_work_for_target(
        &self,
        repository_provider: Option<&str>,
        repository_id: Option<&str>,
        repo: &str,
        pr: u64,
        head_sha: &str,
    ) -> WorkLedgerResult<Option<NativeWork>> {
        let connection = self.connect_read_only()?;
        let mut statement = connection.prepare(
            "SELECT work.id, work.base_ref, work.phase, work.work_generation,
                        work.owner_generation, work.repair_route_ref,
                        object.content_digest
                   FROM work_items work
                   LEFT JOIN workstream_projection_bindings binding
                     ON binding.work_item_id = work.id
                   JOIN protected_objects object
                     ON object.work_item_id = work.id
                    AND object.kind = 'launch_profile'
                  WHERE work.kind = 'terminal_handoff'
                    AND lower(work.repo) = ?1 AND work.pr = ?2
                    AND lower(work.head_sha) = ?3
                    AND ((?4 IS NULL AND ?5 IS NULL)
                         OR (binding.repository_provider = ?4 AND binding.repository_id = ?5))
                  ORDER BY work.id LIMIT 2",
        )?;
        let rows = statement
            .query_map(
                params![repo, pr, head_sha, repository_provider, repository_id],
                |row| {
                    Ok(NativeWork {
                        id: row.get(0)?,
                        base_ref: row.get(1)?,
                        phase: row.get(2)?,
                        work_generation: row.get(3)?,
                        owner_generation: row.get(4)?,
                        route_ref: row.get(5)?,
                        profile_digest: row.get(6)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        match rows.as_slice() {
            [] => Ok(None),
            [work] => Ok(Some(work.clone())),
            _ => Err(WorkLedgerError::Refused(
                "native steward target repository identity is ambiguous".to_owned(),
            )),
        }
    }

    fn has_actionable_audit_event(
        &self,
        work_id: &str,
        kind: &str,
        payload_digest: &str,
    ) -> WorkLedgerResult<bool> {
        Ok(self.connect_read_only()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM events
               WHERE work_item_id = ?1 AND kind = ?2 AND to_state = 'actionable'
                 AND payload_digest = ?3)",
            params![work_id, kind, payload_digest],
            |row| row.get(0),
        )?)
    }
}

fn validate_repository_identity(
    repository_provider: Option<&str>,
    repository_id: Option<&str>,
) -> WorkLedgerResult<()> {
    match (repository_provider, repository_id) {
        (None, None) | (Some(_), Some(_)) => Ok(()),
        _ => Err(WorkLedgerError::Refused(
            "native steward repository identity is incomplete".to_owned(),
        )),
    }
}

fn validate_dispatch_probe_record(record: &DispatchProbeTargetRecord) -> WorkLedgerResult<()> {
    validate_target(&record.repository, record.pull_request, &record.head_sha)?;
    let expected_key = dispatch_probe_target_key(
        &record.repository_provider,
        &record.repository_id,
        &record.repository,
        record.pull_request,
        &record.head_sha,
    );
    if record.repository_provider.trim() != record.repository_provider
        || record.repository_provider.len() < 3
        || record.repository_id.trim() != record.repository_id
        || record.repository_id.is_empty()
        || record.target_key != expected_key
        || record.checkpoint_json.len() > 65_536
    {
        return Err(WorkLedgerError::Refused(
            "dispatch probe target identity or payload is invalid".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn dispatch_probe_target_key(
    repository_provider: &str,
    repository_id: &str,
    repository: &str,
    pull_request: u64,
    head_sha: &str,
) -> String {
    serde_json::to_string(&(
        repository_provider.to_ascii_lowercase(),
        repository_id,
        repository.to_ascii_lowercase(),
        pull_request,
        head_sha.to_ascii_lowercase(),
    ))
    .expect("dispatch scope tuple is serializable")
}

#[cfg_attr(not(unix), allow(dead_code))]
fn unique_base_ref(rows: &[String]) -> WorkLedgerResult<Option<String>> {
    match rows {
        [] => Ok(None),
        [base_ref] => Ok(Some(base_ref.clone())),
        _ => Err(WorkLedgerError::Refused(
            "native steward target repository identity is ambiguous".to_owned(),
        )),
    }
}

fn validate_target(repo: &str, pr: u64, head_sha: &str) -> WorkLedgerResult<()> {
    let canonical_repo =
        repo == repo.trim() && repo == repo.to_ascii_lowercase() && repo.split('/').count() == 2;
    let exact_head = head_sha.len() == 40
        && head_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if pr == 0 || !canonical_repo || !exact_head {
        return Err(WorkLedgerError::Refused(
            "native steward target is not canonical exact-head authority".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::work_ledger::native_publication::tests::{policy, request};

    fn dispatch_probe_record(index: u64) -> DispatchProbeTargetRecord {
        let head_sha = format!("{index:040x}");
        DispatchProbeTargetRecord {
            target_key: dispatch_probe_target_key(
                "github.com",
                "R_test_repository",
                "owner/repo",
                index,
                &head_sha,
            ),
            repository_provider: "github.com".to_owned(),
            repository_id: "R_test_repository".to_owned(),
            repository: "owner/repo".to_owned(),
            pull_request: index,
            head_sha,
            generation: 1,
            due_at: Some("2026-08-31T18:00:00Z".to_owned()),
            checkpoint_json: b"{}".to_vec(),
        }
    }

    fn dispatch_probe_updated_at(ledger: &WorkLedger) -> BTreeMap<String, String> {
        let connection = ledger.connect_read_only().expect("connection");
        let mut statement = connection
            .prepare(
                "SELECT target_key, updated_at FROM dispatch_probe_targets ORDER BY target_key",
            )
            .expect("statement");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    }

    #[test]
    fn dispatch_probe_sync_writes_only_changed_rows_and_prunes_exactly() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        let mut records = vec![dispatch_probe_record(1), dispatch_probe_record(2)];
        ledger
            .replace_dispatch_probe_targets(&records)
            .expect("initial sync");
        let initial = dispatch_probe_updated_at(&ledger);
        std::thread::sleep(std::time::Duration::from_millis(2));
        ledger
            .replace_dispatch_probe_targets(&records)
            .expect("no-op sync");
        assert_eq!(dispatch_probe_updated_at(&ledger), initial);

        records[1].generation = 2;
        std::thread::sleep(std::time::Duration::from_millis(2));
        ledger
            .replace_dispatch_probe_targets(&records)
            .expect("delta sync");
        let changed = dispatch_probe_updated_at(&ledger);
        assert_eq!(
            changed[&records[0].target_key],
            initial[&records[0].target_key]
        );
        assert_ne!(
            changed[&records[1].target_key],
            initial[&records[1].target_key]
        );

        ledger
            .replace_dispatch_probe_targets(&records[1..])
            .expect("exact prune");
        assert_eq!(ledger.load_dispatch_probe_targets().unwrap(), records[1..]);
    }

    #[test]
    fn dispatch_probe_capacity_refusal_is_typed_and_preserves_prior_rows() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        let prior = dispatch_probe_record(1);
        ledger
            .replace_dispatch_probe_targets(std::slice::from_ref(&prior))
            .expect("prior");
        let oversized = (1..=u64::try_from(MAX_DISPATCH_PROBE_TARGETS + 1).unwrap())
            .map(dispatch_probe_record)
            .collect::<Vec<_>>();
        let refused = ledger.replace_dispatch_probe_targets(&oversized);
        assert!(matches!(
            refused,
            Err(WorkLedgerError::Refused(message))
                if message.starts_with("dispatch_probe_capacity_exhausted:")
        ));
        assert_eq!(ledger.load_dispatch_probe_targets().unwrap(), vec![prior]);
    }

    fn published() -> (tempfile::TempDir, WorkLedger, String, String) {
        let state = tempfile::tempdir().expect("state");
        let request = request();
        WorkLedger::open(state.path())
            .expect("ledger")
            .set_repo_policy(
                &crate::work_ledger::RepoPolicy {
                    repo: request.repository.clone(),
                    primary_platform: "macos".to_owned(),
                    compatibility_mode: "independent".to_owned(),
                    compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
                    blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                    declared_dependency_lanes: Vec::new(),
                    revision: 0,
                },
                0,
            )
            .expect("repo policy");
        let report = WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &request,
            &policy(vec![request.repository.clone()]),
            true,
        )
        .expect("publish");
        let ledger = WorkLedger::open_existing(state.path())
            .expect("open")
            .expect("ledger");
        (state, ledger, request.repository, report.wake_id)
    }

    fn counts(ledger: &WorkLedger) -> (String, u64) {
        ledger
            .connect_read_only()
            .expect("connection")
            .query_row(
                "SELECT work.phase, (SELECT COUNT(*) FROM outbox)
                   FROM work_items work WHERE work.kind = 'terminal_handoff'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("counts")
    }

    #[test]
    fn publication_and_non_actionable_observations_create_no_wake() {
        let (_state, ledger, repo, _wake) = published();
        assert_eq!(counts(&ledger), ("managed".to_owned(), 0));
        for disposition in [
            NativeStewardDisposition::Waiting,
            NativeStewardDisposition::Passing,
            NativeStewardDisposition::StaleHead,
        ] {
            ledger
                .apply_native_steward_disposition(&repo, 43, &"a".repeat(40), disposition)
                .expect("non-actionable");
            assert_eq!(counts(&ledger).1, 0);
        }
    }

    #[test]
    fn exact_actionable_transition_enqueues_once_and_restarts_after_crash() {
        let (_state, ledger, repo, expected_wake) = published();
        let crashed = catch_unwind(AssertUnwindSafe(|| {
            ledger
                .apply_native_steward_disposition_with(
                    None,
                    None,
                    &repo,
                    43,
                    &"a".repeat(40),
                    NativeStewardDisposition::Actionable,
                    None,
                    |point| assert_ne!(point, "after_actionable", "crash"),
                )
                .ok();
        }));
        assert!(crashed.is_err());
        assert_eq!(counts(&ledger), ("actionable".to_owned(), 0));

        let applied = ledger
            .apply_native_steward_disposition(
                &repo,
                43,
                &"a".repeat(40),
                NativeStewardDisposition::Actionable,
            )
            .expect("resume");
        assert!(applied.wake_enqueued);
        assert_eq!(counts(&ledger), ("dispatching".to_owned(), 1));
        let wake: String = ledger
            .connect_read_only()
            .expect("connection")
            .query_row("SELECT wake_id FROM outbox", [], |row| row.get(0))
            .expect("wake");
        assert_eq!(wake, expected_wake);

        let replay = ledger
            .apply_native_steward_disposition(
                &repo,
                43,
                &"a".repeat(40),
                NativeStewardDisposition::Actionable,
            )
            .expect("replay");
        assert!(!replay.changed);
        assert!(!replay.wake_enqueued);
        assert_eq!(counts(&ledger).1, 1);
    }

    #[test]
    fn dispatch_wedge_receipt_restarts_after_actionable_commit_without_duplicate_wake() {
        let (_state, ledger, repo, _wake) = published();
        let receipt = "d".repeat(64);
        let evidence = "f".repeat(64);
        let crashed = catch_unwind(AssertUnwindSafe(|| {
            ledger
                .apply_native_steward_disposition_with(
                    None,
                    None,
                    &repo,
                    43,
                    &"a".repeat(40),
                    NativeStewardDisposition::Actionable,
                    Some(NativeActionableAudit {
                        expected_base_ref: "main",
                        evidence_event: ("dispatch_wedge_detected", &evidence),
                        identity_event: ("dispatch_wedge_identity", &receipt),
                    }),
                    |point| assert_ne!(point, "after_actionable", "crash"),
                )
                .ok();
        }));
        assert!(crashed.is_err());
        assert_eq!(counts(&ledger), ("actionable".to_owned(), 0));

        let resumed = ledger
            .publish_dispatch_wedge(
                None,
                None,
                &repo,
                "main",
                43,
                &"a".repeat(40),
                &receipt,
                &evidence,
            )
            .expect("resume receipt");
        assert!(resumed.wake_enqueued);
        let replay = ledger
            .publish_dispatch_wedge(
                None,
                None,
                &repo,
                "main",
                43,
                &"a".repeat(40),
                &receipt,
                &evidence,
            )
            .expect("replay receipt");
        assert!(!replay.changed);
        assert!(!replay.wake_enqueued);
        assert_eq!(counts(&ledger), ("dispatching".to_owned(), 1));
    }

    #[test]
    fn dispatch_wedge_receipt_refuses_unrelated_actionable_transition() {
        let (_state, ledger, repo, _wake) = published();
        let crashed = catch_unwind(AssertUnwindSafe(|| {
            ledger
                .apply_native_steward_disposition_with(
                    None,
                    None,
                    &repo,
                    43,
                    &"a".repeat(40),
                    NativeStewardDisposition::Actionable,
                    None,
                    |point| assert_ne!(point, "after_actionable", "crash"),
                )
                .ok();
        }));
        assert!(crashed.is_err());

        let refused = ledger.publish_dispatch_wedge(
            None,
            None,
            &repo,
            "main",
            43,
            &"a".repeat(40),
            &"e".repeat(64),
            &"f".repeat(64),
        );
        assert!(matches!(refused, Err(WorkLedgerError::Refused(_))));
        assert_eq!(counts(&ledger), ("actionable".to_owned(), 0));
    }

    #[test]
    fn concurrent_dispatch_wedge_receipts_cannot_publish_under_each_other() {
        let (_state, ledger, repo, _wake) = published();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for (identity, evidence) in [('d', 'f'), ('e', 'a')] {
            let ledger = ledger.clone();
            let repo = repo.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                ledger.publish_dispatch_wedge(
                    None,
                    None,
                    &repo,
                    "main",
                    43,
                    &"a".repeat(40),
                    &identity.to_string().repeat(64),
                    &evidence.to_string().repeat(64),
                )
            }));
        }
        barrier.wait();
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().expect("join"))
            .collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_err()).count(),
            1
        );
        assert_eq!(counts(&ledger), ("dispatching".to_owned(), 1));
        let connection = ledger.connect_read_only().expect("connection");
        let identities: u64 = connection
            .query_row(
                "SELECT count(*) FROM events WHERE kind = 'dispatch_wedge_identity'",
                [],
                |row| row.get(0),
            )
            .expect("identity events");
        let evidence: u64 = connection
            .query_row(
                "SELECT count(*) FROM events WHERE kind = 'dispatch_wedge_detected'",
                [],
                |row| row.get(0),
            )
            .expect("evidence events");
        assert_eq!((identities, evidence), (1, 1));
    }

    #[test]
    fn terminal_and_stale_head_paths_never_enqueue() {
        for disposition in [
            NativeStewardDisposition::Merged,
            NativeStewardDisposition::Superseded,
            NativeStewardDisposition::StaleHead,
        ] {
            let (_state, ledger, repo, _wake) = published();
            ledger
                .apply_native_steward_disposition(&repo, 43, &"a".repeat(40), disposition)
                .expect("terminal/no-op");
            assert_eq!(counts(&ledger).1, 0);
        }
    }

    #[test]
    fn stale_head_atomically_suppresses_an_unclaimed_actionable_wake() {
        let (_state, ledger, repo, _wake) = published();
        ledger
            .apply_native_steward_disposition(
                &repo,
                43,
                &"a".repeat(40),
                NativeStewardDisposition::Actionable,
            )
            .expect("actionable");
        assert_eq!(counts(&ledger), ("dispatching".to_owned(), 1));

        let stale = ledger
            .apply_native_steward_disposition(
                &repo,
                43,
                &"a".repeat(40),
                NativeStewardDisposition::StaleHead,
            )
            .expect("stale head");
        assert!(stale.changed);
        assert!(!stale.wake_enqueued);
        assert_eq!(stale.phase.as_deref(), Some("terminal"));
        let wake_state: String = ledger
            .connect_read_only()
            .expect("connection")
            .query_row("SELECT state FROM outbox", [], |row| row.get(0))
            .expect("wake state");
        assert_eq!(wake_state, "failed");
    }

    #[test]
    fn concurrent_daemon_and_steward_producers_dedupe_to_one_wake() {
        let (_state, ledger, repo, _wake) = published();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let ledger = ledger.clone();
            let repo = repo.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                ledger.apply_native_steward_disposition(
                    &repo,
                    43,
                    &"a".repeat(40),
                    NativeStewardDisposition::Actionable,
                )
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().expect("join").expect("deduped producer");
        }
        assert_eq!(counts(&ledger), ("dispatching".to_owned(), 1));
    }
}
