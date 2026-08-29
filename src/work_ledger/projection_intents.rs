//! Transactional production of immutable external-projection intents.
//!
//! These rows are part of the authoritative ledger transaction. External
//! projection remains default-off and asynchronous; the receipt snapshot here
//! is the content-addressed source of truth used for later reconstruction.

use serde::{Deserialize, Serialize};

use crate::transition_projection::{ProjectionEvidence, TransitionDraft, TransitionKind};

use super::{
    Transaction, Utc, WorkLedger, WorkLedgerError, WorkLedgerResult, digest, params,
    validate_digest,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectionIntentKind {
    Handoff,
    Waiting,
    Actionable,
    NewHead,
    Merge,
    ConfiguredClosure,
}

impl ProjectionIntentKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Handoff => "handoff",
            Self::Waiting => "waiting",
            Self::Actionable => "actionable",
            Self::NewHead => "new_head",
            Self::Merge => "merge",
            Self::ConfiguredClosure => "configured_closure",
        }
    }

    fn transition_kind(self) -> TransitionKind {
        match self {
            Self::Handoff => TransitionKind::Handoff,
            Self::Waiting => TransitionKind::Waiting,
            Self::Actionable => TransitionKind::Actionable,
            Self::NewHead => TransitionKind::NewHead,
            Self::Merge => TransitionKind::Merge,
            Self::ConfiguredClosure => TransitionKind::ConfiguredClosure,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectionReceiptSnapshotV1 {
    schema_version: u32,
    work_item_id: String,
    workstream_handle: String,
    sequence: u64,
    kind: ProjectionIntentKind,
    source_revision: String,
    exact_head: Option<String>,
    work_generation: u64,
    owner_generation: u64,
    event_kind: String,
    from_state: Option<String>,
    to_state: String,
    authority_digest: String,
    terminal_disposition: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct PendingProjectionIntent {
    pub(crate) intent_id: String,
    pub(crate) repository: String,
    pub(crate) receipt_snapshot: Vec<u8>,
    pub(crate) receipt_sha256: String,
    pub(crate) transition_id: String,
    pub(crate) supersedes_transition_id: Option<String>,
    pub(crate) attempts: u32,
}

impl PendingProjectionIntent {
    pub(crate) fn reconstruct(&self) -> WorkLedgerResult<TransitionDraft> {
        if digest(&self.receipt_snapshot) != self.receipt_sha256 {
            return Err(WorkLedgerError::Refused(
                "projection receipt snapshot digest mismatch".to_owned(),
            ));
        }
        let snapshot: ProjectionReceiptSnapshotV1 = serde_json::from_slice(&self.receipt_snapshot)
            .map_err(|_| {
                WorkLedgerError::Refused("projection receipt snapshot is malformed".to_owned())
            })?;
        let draft = TransitionDraft {
            workstream_id: snapshot.workstream_handle,
            sequence: snapshot.sequence,
            kind: snapshot.kind.transition_kind(),
            evidence: ProjectionEvidence {
                source_revision: snapshot.source_revision,
                exact_head: snapshot.exact_head,
                receipt_sha256: self.receipt_sha256.clone(),
            },
            supersedes_transition_id: self.supersedes_transition_id.clone(),
            note: snapshot.terminal_disposition,
        };
        let sealed = draft.clone().seal().map_err(|error| {
            WorkLedgerError::Refused(format!("projection intent reconstruction failed: {error}"))
        })?;
        if sealed.transition_id != self.transition_id {
            return Err(WorkLedgerError::Refused(
                "projection intent reconstruction contradicted stored identity".to_owned(),
            ));
        }
        Ok(draft)
    }
}

impl WorkLedger {
    pub(super) fn stage_waiting_observation(
        &self,
        work_item_id: &str,
        work_generation: u64,
        owner_generation: u64,
    ) -> WorkLedgerResult<bool> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let mut connection = self.connect_read_write()?;
        super::configure_durable(&connection)?;
        super::verify_supported_schema(&connection)?;
        let transaction =
            connection.transaction_with_behavior(super::TransactionBehavior::Immediate)?;
        let current: (String, u64, u64) = transaction.query_row(
            "SELECT phase, work_generation, owner_generation FROM work_items WHERE id = ?1",
            [work_item_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if current.1 != work_generation || current.2 != owner_generation {
            return Err(WorkLedgerError::Refused(
                "waiting observation authority is stale".to_owned(),
            ));
        }
        let projection_bound: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM workstream_projection_bindings WHERE work_item_id = ?1)",
            [work_item_id],
            |row| row.get(0),
        )?;
        if !projection_bound {
            transaction.commit()?;
            return Ok(false);
        }
        let already_latest: bool = transaction.query_row(
            "SELECT coalesce((SELECT kind = 'waiting' FROM projection_intents
                               WHERE work_item_id = ?1 ORDER BY sequence DESC LIMIT 1), 0)",
            [work_item_id],
            |row| row.get(0),
        )?;
        if already_latest {
            transaction.commit()?;
            return Ok(false);
        }
        let authority_digest = digest(
            format!("shipyard-waiting-observation-v1\n{work_item_id}\n{work_generation}\n{owner_generation}\n{}", current.0).as_bytes(),
        );
        let now = Utc::now().to_rfc3339();
        Self::stage_projection_intent(
            &transaction,
            work_item_id,
            work_generation,
            owner_generation,
            ProjectionIntentKind::Waiting,
            "steward_waiting_observation",
            Some(&current.0),
            &current.0,
            &authority_digest,
            None,
            &now,
        )?;
        transaction.commit()?;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn bind_workstream_projection(
        &self,
        work_item_id: &str,
        workstream_handle: &str,
        plan_sha256: &str,
        root_revision: u64,
        issue_revision: u64,
        projection_revision: u64,
        material_event_revision: u64,
        repository: &str,
        exact_head: &str,
    ) -> WorkLedgerResult<()> {
        validate_digest("projection plan digest", plan_sha256)?;
        if workstream_handle.is_empty() || workstream_handle.len() > 128 || projection_revision == 0
        {
            return Err(WorkLedgerError::Refused(
                "workstream projection binding is incomplete".to_owned(),
            ));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let mut connection = self.connect_read_write()?;
        super::configure_durable(&connection)?;
        super::verify_supported_schema(&connection)?;
        let transaction =
            connection.transaction_with_behavior(super::TransactionBehavior::Immediate)?;
        let now = Utc::now().to_rfc3339();
        transaction.execute(
            "INSERT OR IGNORE INTO workstream_projection_bindings
             (work_item_id, workstream_handle, plan_sha256, root_revision, issue_revision,
              projection_revision, material_event_revision, repository, exact_head, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                work_item_id,
                workstream_handle,
                plan_sha256,
                root_revision,
                issue_revision,
                projection_revision,
                material_event_revision,
                repository,
                exact_head,
                now,
            ],
        )?;
        let exact: bool = transaction.query_row(
            "SELECT workstream_handle = ?2 AND plan_sha256 = ?3 AND root_revision = ?4
                    AND issue_revision = ?5 AND projection_revision = ?6
                    AND material_event_revision = ?7 AND repository = ?8 AND exact_head = ?9
               FROM workstream_projection_bindings WHERE work_item_id = ?1",
            params![
                work_item_id,
                workstream_handle,
                plan_sha256,
                root_revision,
                issue_revision,
                projection_revision,
                material_event_revision,
                repository,
                exact_head,
            ],
            |row| row.get(0),
        )?;
        if !exact {
            return Err(WorkLedgerError::Refused(
                "workstream projection binding changed".to_owned(),
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(super) fn stage_projection_intent(
        transaction: &Transaction<'_>,
        work_item_id: &str,
        work_generation: u64,
        owner_generation: u64,
        kind: ProjectionIntentKind,
        event_kind: &str,
        from_state: Option<&str>,
        to_state: &str,
        authority_digest: &str,
        terminal_disposition: Option<&str>,
        now: &str,
    ) -> WorkLedgerResult<String> {
        validate_digest("projection authority digest", authority_digest)?;
        let binding: (String, String, String, String) = transaction
            .query_row(
                "SELECT workstream_handle, plan_sha256, repository, exact_head
               FROM workstream_projection_bindings WHERE work_item_id = ?1",
                [work_item_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => WorkLedgerError::Refused(
                    "authoritative transition lacks an authenticated workstream projection binding"
                        .to_owned(),
                ),
                other => WorkLedgerError::Sql(other),
            })?;
        let sequence: u64 = transaction.query_row(
            "SELECT coalesce(max(sequence), 0) + 1 FROM projection_intents
              WHERE workstream_handle = ?1",
            [&binding.0],
            |row| row.get(0),
        )?;
        let supersedes_transition_id: Option<String> = transaction
            .query_row(
                "SELECT intent.transition_id
                   FROM projection_intents intent
                   JOIN workstream_projection_bindings prior_binding
                     ON prior_binding.work_item_id = intent.work_item_id
                  WHERE intent.workstream_handle = ?1 AND prior_binding.repository = ?2
                  ORDER BY intent.sequence DESC LIMIT 1",
                params![binding.0, binding.2],
                |row| row.get(0),
            )
            .optional()?;
        let exact_head = Some(binding.3.clone());
        let snapshot = ProjectionReceiptSnapshotV1 {
            schema_version: 1,
            work_item_id: work_item_id.to_owned(),
            workstream_handle: binding.0.clone(),
            sequence,
            kind,
            source_revision: binding.1.clone(),
            exact_head: exact_head.clone(),
            work_generation,
            owner_generation,
            event_kind: event_kind.to_owned(),
            from_state: from_state.map(str::to_owned),
            to_state: to_state.to_owned(),
            authority_digest: authority_digest.to_owned(),
            terminal_disposition: terminal_disposition.map(str::to_owned),
        };
        let receipt_snapshot = serde_json::to_vec(&snapshot).map_err(|_| {
            WorkLedgerError::Refused("projection receipt snapshot could not be encoded".to_owned())
        })?;
        let receipt_sha256 = digest(&receipt_snapshot);
        let note = terminal_disposition.map(ToOwned::to_owned);
        let draft = TransitionDraft {
            workstream_id: binding.0.clone(),
            sequence,
            kind: kind.transition_kind(),
            evidence: ProjectionEvidence {
                source_revision: binding.1.clone(),
                exact_head,
                receipt_sha256: receipt_sha256.clone(),
            },
            supersedes_transition_id: supersedes_transition_id.clone(),
            note,
        };
        let transition = draft.seal().map_err(|error| {
            WorkLedgerError::Refused(format!("projection intent is invalid: {error}"))
        })?;
        let intent_id = digest(
            format!(
                "shipyard-projection-intent-v1\n{}",
                transition.transition_id
            )
            .as_bytes(),
        );
        transaction.execute(
            "INSERT INTO projection_intents
             (intent_id, work_item_id, workstream_handle, sequence, kind, source_revision,
              exact_head, receipt_snapshot, receipt_sha256, transition_id,
              supersedes_transition_id, state, attempts, retry_at_unix_ms,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     'pending', 0, 0, ?12, ?12)",
            params![
                intent_id,
                work_item_id,
                binding.0,
                sequence,
                kind.as_str(),
                binding.1,
                transition.evidence.exact_head,
                receipt_snapshot,
                receipt_sha256,
                transition.transition_id,
                supersedes_transition_id,
                now,
            ],
        )?;
        Ok(intent_id)
    }

    pub(crate) fn pending_projection_intents(
        &self,
        now_unix_ms: u64,
        limit: u64,
    ) -> WorkLedgerResult<Vec<PendingProjectionIntent>> {
        let connection = self.connect_read_only()?;
        super::verify_supported_schema(&connection)?;
        let mut statement = connection.prepare(
            "WITH eligible AS (
               SELECT intent.*, binding.repository,
                      row_number() OVER (
                        PARTITION BY intent.workstream_handle ORDER BY intent.sequence
                      ) AS workstream_rank
                 FROM projection_intents intent
                 JOIN workstream_projection_bindings binding
                   ON binding.work_item_id = intent.work_item_id
                WHERE intent.state = 'pending'
             )
             SELECT intent_id, repository, receipt_snapshot, receipt_sha256, transition_id,
                    supersedes_transition_id, attempts
               FROM eligible WHERE workstream_rank = 1 AND retry_at_unix_ms <= ?1
              ORDER BY sequence, workstream_handle LIMIT ?2",
        )?;
        let rows = statement.query_map(params![now_unix_ms, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, u32>(6)?,
            ))
        })?;
        rows.map(|row| {
            let (
                intent_id,
                repository,
                bytes,
                receipt_sha256,
                transition_id,
                supersedes_transition_id,
                attempts,
            ) = row?;
            Ok(PendingProjectionIntent {
                intent_id,
                repository,
                receipt_snapshot: bytes,
                receipt_sha256,
                transition_id,
                supersedes_transition_id,
                attempts,
            })
        })
        .collect()
    }

    pub(crate) fn mark_projection_intent_projected(&self, intent_id: &str) -> WorkLedgerResult<()> {
        self.update_projection_intent_state(intent_id, "projected", None, 0)
    }

    #[cfg(test)]
    pub(crate) fn projection_intent_state(
        &self,
        intent_id: &str,
    ) -> WorkLedgerResult<(String, u64)> {
        Ok(self.connect_read_only()?.query_row(
            "SELECT state, attempts FROM projection_intents WHERE intent_id = ?1",
            [intent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    #[cfg(test)]
    pub(crate) fn corrupt_projection_receipt_for_test(
        &self,
        intent_id: &str,
    ) -> WorkLedgerResult<()> {
        let connection = self.connect_read_write()?;
        connection.execute_batch("DROP TRIGGER projection_intent_identity_immutable")?;
        connection.execute(
            "UPDATE projection_intents SET receipt_snapshot = x'7b7d' WHERE intent_id = ?1",
            [intent_id],
        )?;
        Ok(())
    }

    pub(crate) fn retry_projection_intent(
        &self,
        intent_id: &str,
        failure_class: &str,
        retry_at_unix_ms: u64,
    ) -> WorkLedgerResult<()> {
        self.update_projection_intent_state(
            intent_id,
            "pending",
            Some(failure_class),
            retry_at_unix_ms,
        )
    }

    pub(crate) fn quarantine_projection_intent(
        &self,
        intent_id: &str,
        failure_class: &str,
    ) -> WorkLedgerResult<()> {
        self.update_projection_intent_state(intent_id, "quarantined", Some(failure_class), 0)
    }

    fn update_projection_intent_state(
        &self,
        intent_id: &str,
        state: &str,
        failure_class: Option<&str>,
        retry_at_unix_ms: u64,
    ) -> WorkLedgerResult<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let connection = self.connect_read_write()?;
        super::configure_durable(&connection)?;
        let changed = connection.execute(
            "UPDATE projection_intents
                SET state = ?1, attempts = attempts + 1, retry_at_unix_ms = ?2,
                    failure_class = ?3, updated_at = ?4
              WHERE intent_id = ?5 AND state = 'pending'",
            params![
                state,
                retry_at_unix_ms,
                failure_class,
                Utc::now().to_rfc3339(),
                intent_id
            ],
        )?;
        if changed > 1 {
            return Err(WorkLedgerError::Refused(
                "projection intent update was not singular".to_owned(),
            ));
        }
        Ok(())
    }
}

use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_ledger::RepoPolicy;
    use crate::work_ledger::actionable_scheduler::NativeStewardDisposition;
    use crate::work_ledger::native_publication::tests::{policy, request};
    use std::sync::{Arc, Barrier};

    fn published() -> (
        tempfile::TempDir,
        WorkLedger,
        super::super::NativePublicationRequest,
    ) {
        let state = tempfile::tempdir().expect("state");
        let request = request();
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        ledger
            .set_repo_policy(
                &RepoPolicy {
                    repo: request.repository.clone(),
                    primary_platform: "macos".to_owned(),
                    compatibility_mode: "independent".to_owned(),
                    compatibility_lanes: vec!["linux".to_owned()],
                    blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                    declared_dependency_lanes: Vec::new(),
                    revision: 0,
                },
                0,
            )
            .expect("policy");
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &request,
            &policy(vec![request.repository.clone()]),
            true,
        )
        .expect("publication");
        (state, ledger, request)
    }

    #[test]
    fn schema_v11_installs_immutable_projection_tables() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        let connection = ledger.connect_read_only().expect("connection");
        let version: u64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 11);
        for table in ["workstream_projection_bindings", "projection_intents"] {
            let exists: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
                    [table],
                    |row| row.get(0),
                )
                .expect("table");
            assert!(exists, "missing {table}");
        }
    }

    #[test]
    fn schema_v10_migrates_atomically_to_v11() {
        let state = tempfile::tempdir().expect("state");
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        let connection = ledger.connect_read_write().expect("connection");
        connection
            .execute_batch(
                "DROP TRIGGER projection_intent_no_delete;
                 DROP TRIGGER projection_intent_identity_immutable;
                 DROP TABLE projection_intents;
                 DROP TRIGGER workstream_projection_binding_no_delete;
                 DROP TRIGGER workstream_projection_binding_identity_immutable;
                 DROP TABLE workstream_projection_bindings;
                 PRAGMA user_version = 10;",
            )
            .expect("v10 fixture");
        drop(connection);
        let reopened = WorkLedger::open(state.path()).expect("migrated ledger");
        assert_eq!(reopened.status().expect("status").schema_version, 11);
    }

    #[test]
    fn publication_binds_bootstrap_and_stages_managed_receipt_in_one_ledger() {
        let (_state, ledger, request) = published();
        let connection = ledger.connect_read_only().expect("connection");
        let binding: (String, String, u64, u64, u64, u64, String) = connection
            .query_row(
                "SELECT workstream_handle, plan_sha256, root_revision, issue_revision,
                        projection_revision, material_event_revision, exact_head
                   FROM workstream_projection_bindings",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .expect("binding");
        assert_eq!(binding.0, request.workstream_handle);
        assert_eq!(binding.1, request.plan_sha256);
        assert_eq!((binding.2, binding.3, binding.4, binding.5), (1, 1, 1, 1));
        assert_eq!(binding.6, request.head_sha);

        let intents = ledger.pending_projection_intents(0, 32).expect("intents");
        assert_eq!(intents.len(), 1);
        let first = intents[0].reconstruct().expect("reconstruct");
        let second = intents[0].reconstruct().expect("reconstruct again");
        assert_eq!(first, second);
        assert_eq!(first.kind, TransitionKind::Handoff);
        assert_eq!(first.evidence.source_revision, request.plan_sha256);
        assert_eq!(
            first.evidence.exact_head.as_deref(),
            Some(request.head_sha.as_str())
        );
        assert_eq!(
            digest(&intents[0].receipt_snapshot),
            first.evidence.receipt_sha256
        );
    }

    #[test]
    fn producer_sequence_is_strict_and_supersedes_prior_transition() {
        let (_state, ledger, request) = published();
        ledger
            .apply_native_steward_disposition(
                &request.repository,
                request.pull_request,
                &request.head_sha,
                NativeStewardDisposition::Waiting,
            )
            .expect("waiting");
        ledger
            .apply_native_steward_disposition(
                &request.repository,
                request.pull_request,
                &request.head_sha,
                NativeStewardDisposition::Actionable,
            )
            .expect("actionable");
        let connection = ledger.connect_read_only().expect("connection");
        let mut statement = connection
            .prepare(
                "SELECT sequence, kind, transition_id, supersedes_transition_id
                   FROM projection_intents ORDER BY sequence",
            )
            .expect("statement");
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .expect("rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");
        assert_eq!(
            rows.iter().map(|row| row.0).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            rows.iter().map(|row| row.1.as_str()).collect::<Vec<_>>(),
            vec!["handoff", "waiting", "actionable", "handoff"]
        );
        for pair in rows.windows(2) {
            assert_eq!(pair[1].3.as_deref(), Some(pair[0].2.as_str()));
        }
    }

    #[test]
    fn merged_and_nonmerged_closure_remain_distinct() {
        for (disposition, expected_kind, expected_note) in [
            (NativeStewardDisposition::Merged, "merge", "merged"),
            (
                NativeStewardDisposition::Superseded,
                "configured_closure",
                "superseded",
            ),
            (
                NativeStewardDisposition::StaleHead,
                "configured_closure",
                "stale_head",
            ),
        ] {
            let (_state, ledger, request) = published();
            ledger
                .apply_native_steward_disposition(
                    &request.repository,
                    request.pull_request,
                    &request.head_sha,
                    disposition,
                )
                .expect("closure");
            let (kind, bytes): (String, Vec<u8>) = ledger
                .connect_read_only()
                .expect("connection")
                .query_row(
                    "SELECT kind, receipt_snapshot FROM projection_intents
                      ORDER BY sequence DESC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("closure intent");
            let snapshot: ProjectionReceiptSnapshotV1 =
                serde_json::from_slice(&bytes).expect("snapshot");
            assert_eq!(kind, expected_kind);
            assert_eq!(
                snapshot.terminal_disposition.as_deref(),
                Some(expected_note)
            );
        }
    }

    #[test]
    fn waiting_observation_replay_is_idempotent() {
        let (_state, ledger, request) = published();
        for _ in 0..2 {
            ledger
                .apply_native_steward_disposition(
                    &request.repository,
                    request.pull_request,
                    &request.head_sha,
                    NativeStewardDisposition::Waiting,
                )
                .expect("waiting");
        }
        let count: u64 = ledger
            .connect_read_only()
            .expect("connection")
            .query_row("SELECT COUNT(*) FROM projection_intents", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(count, 2);
    }

    #[test]
    fn concurrent_waiting_producers_allocate_one_next_sequence() {
        let (_state, ledger, request) = published();
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let ledger = ledger.clone();
            let request = request.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                ledger.apply_native_steward_disposition(
                    &request.repository,
                    request.pull_request,
                    &request.head_sha,
                    NativeStewardDisposition::Waiting,
                )
            }));
        }
        barrier.wait();
        let reports = workers
            .into_iter()
            .map(|worker| worker.join().expect("join").expect("waiting"))
            .collect::<Vec<_>>();
        assert_eq!(reports.iter().filter(|report| report.changed).count(), 1);
        let sequences: String = ledger
            .connect_read_only()
            .expect("connection")
            .query_row(
                "SELECT group_concat(sequence, ',') FROM
                   (SELECT sequence FROM projection_intents ORDER BY sequence)",
                [],
                |row| row.get(0),
            )
            .expect("sequences");
        assert_eq!(sequences, "1,2");
    }
}
