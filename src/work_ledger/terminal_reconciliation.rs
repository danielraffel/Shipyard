//! Fenced reconciliation of a terminal native handoff that predates its
//! immutable workstream projection binding.
//!
//! This path is intentionally terminal-only. It never creates or revises a
//! route, continuation, wake, ownership authority, ownership lease, activation
//! epoch, or projection intent. The accepted projection-binding insert mints
//! the schema-required inert ownership-root identity, but no ownership row,
//! holder material, bootstrap eligibility, or lease can be created by this
//! path. The other accepted mutation is an audit event for a row that is
//! already terminal.

use serde::Serialize;

use super::lifecycle::record_event;
use super::route::OpaqueRef;
use super::{
    LifecycleState, OptionalExtension, ProtectedObjectKind, TransactionBehavior, Utc, WorkLedger,
    WorkLedgerError, WorkLedgerResult, configure_durable, digest, params, validate_digest,
    validate_opaque_ref, validate_token, verify_integrity, verify_supported_schema,
};

const MAX_UNBOUND_TERMINAL_TARGETS: usize = 32;
const MAX_UNBOUND_TERMINAL_QUERY_ROWS: i64 = 33;

/// Redacted exact target discovered without taking mutation authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct TerminalReconciliationTarget {
    pub(crate) work_id: String,
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) exact_head: String,
    pub(crate) base_ref: String,
    pub(crate) phase: String,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) profile_digest: String,
    #[serde(skip_serializing)]
    pub(crate) route_ref: String,
    #[serde(skip_serializing)]
    pub(crate) wake_id: String,
    #[serde(skip_serializing)]
    pub(crate) source_digest: String,
}

/// Bounded no-write inventory used before selecting one exact repair target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct TerminalReconciliationInventory {
    pub(crate) snapshot_sha256: String,
    pub(crate) complete: bool,
    pub(crate) limit: usize,
    pub(crate) items: Vec<TerminalReconciliationTarget>,
}

/// Complete public authority required to bind an already-terminal target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalReconciliationRequest {
    pub(crate) repository_provider: String,
    pub(crate) repository_id: String,
    pub(crate) repository: String,
    pub(crate) pull_request_node_id: String,
    pub(crate) pull_request: u64,
    pub(crate) head_sha: String,
    pub(crate) base_ref: String,
    pub(crate) merge_sha: String,
    pub(crate) merged_at: String,
    pub(crate) github_installation_id: u64,
    pub(crate) work_id: String,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) source_digest: String,
    pub(crate) route_ref: String,
    pub(crate) wake_id: String,
    pub(crate) profile_digest: String,
    pub(crate) workstream_handle: String,
    pub(crate) plan_sha256: String,
    pub(crate) root_revision: u64,
    pub(crate) issue_revision: u64,
    pub(crate) projection_revision: u64,
    pub(crate) material_event_revision: u64,
    pub(crate) success_continuation_digest: String,
    pub(crate) failure_continuation_digest: String,
}

/// Stable plan/apply receipt. No protected profile or route material is exposed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct TerminalReconciliationReport {
    pub(crate) applied: bool,
    pub(crate) replay: bool,
    pub(crate) work_id: String,
    pub(crate) workstream_handle: String,
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) exact_head: String,
    pub(crate) merge_sha: String,
    pub(crate) merged_at: String,
    pub(crate) receipt_sha256: String,
    pub(crate) plan_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct TerminalEvidenceV1<'a> {
    schema_version: u32,
    repository_provider: &'a str,
    repository_id: &'a str,
    repository: &'a str,
    pull_request_node_id: &'a str,
    pull_request: u64,
    exact_head: &'a str,
    base_ref: &'a str,
    disposition: &'static str,
    merge_sha: &'a str,
    merged_at: &'a str,
    github_installation_id: u64,
    work_id: &'a str,
    work_generation: u64,
    owner_generation: u64,
    source_digest: &'a str,
    workstream_handle: &'a str,
}

impl WorkLedger {
    /// List only incomplete terminal projection identities. The ordinary
    /// inventory remains fail-closed and never treats these rows as authority.
    pub(crate) fn terminal_reconciliation_inventory(
        &self,
    ) -> WorkLedgerResult<TerminalReconciliationInventory> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        self.verify_protected_object_storage(&connection)?;

        let mut statement = connection.prepare(
            "SELECT work.id, lower(work.repo), work.pr, lower(work.head_sha), work.base_ref,
                    work.phase, work.work_generation, work.owner_generation,
                    profile.content_digest, work.repair_route_ref, wake.wake_id,
                    work.source_digest
               FROM work_items work
               LEFT JOIN workstream_projection_bindings binding
                 ON binding.work_item_id = work.id
               JOIN protected_objects profile
                 ON profile.work_item_id = work.id AND profile.kind = 'launch_profile'
               JOIN outbox wake
                 ON wake.work_item_id = work.id
                AND wake.route_ref = work.repair_route_ref
                AND wake.payload_digest = profile.content_digest
                AND wake.work_generation = work.work_generation - 1
                AND wake.owner_generation = work.owner_generation
                AND (wake.profile_ref = profile.profile_ref
                     OR (wake.profile_ref IS NULL AND wake.state = 'failed'))
              WHERE binding.work_item_id IS NULL AND work.kind = 'terminal_handoff'
                AND work.phase = 'terminal'
              ORDER BY work.id LIMIT ?1",
        )?;
        let rows =
            statement.query_map([MAX_UNBOUND_TERMINAL_QUERY_ROWS], terminal_target_from_row)?;
        let mut items = rows.collect::<Result<Vec<_>, _>>()?;
        if items.len() > MAX_UNBOUND_TERMINAL_TARGETS {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation inventory exceeds its bound".to_owned(),
            ));
        }
        validate_unique_targets(&items)?;
        let snapshot_sha256 = inventory_digest(&items)?;
        Ok(TerminalReconciliationInventory {
            snapshot_sha256,
            complete: true,
            limit: MAX_UNBOUND_TERMINAL_TARGETS,
            items: std::mem::take(&mut items),
        })
    }

    /// Resolve one exact target for dry-run/apply or exact replay. Unlike the
    /// no-argument inventory this deliberately includes an already-bound row;
    /// the planner then proves its immutable binding, receipt, and event.
    pub(crate) fn terminal_reconciliation_target(
        &self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
    ) -> WorkLedgerResult<TerminalReconciliationTarget> {
        validate_target(repository, pull_request, head_sha)?;
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        self.verify_protected_object_storage(&connection)?;
        let mut statement = connection.prepare(
            "SELECT work.id, lower(work.repo), work.pr, lower(work.head_sha), work.base_ref,
                    work.phase, work.work_generation, work.owner_generation,
                    profile.content_digest, work.repair_route_ref, wake.wake_id,
                    work.source_digest
               FROM work_items work
               JOIN protected_objects profile
                 ON profile.work_item_id = work.id AND profile.kind = 'launch_profile'
               JOIN outbox wake
                 ON wake.work_item_id = work.id
                AND wake.route_ref = work.repair_route_ref
                AND wake.payload_digest = profile.content_digest
                AND wake.work_generation = work.work_generation - 1
                AND wake.owner_generation = work.owner_generation
                AND (wake.profile_ref = profile.profile_ref
                     OR (wake.profile_ref IS NULL AND wake.state = 'failed'))
              WHERE work.kind = 'terminal_handoff' AND work.phase = 'terminal'
                AND lower(work.repo) = ?1 AND work.pr = ?2 AND lower(work.head_sha) = ?3
                AND (NOT EXISTS(
                       SELECT 1 FROM workstream_projection_bindings binding
                        WHERE binding.work_item_id = work.id)
                     OR EXISTS(
                       SELECT 1 FROM events event
                        WHERE event.work_item_id = work.id
                          AND event.kind = 'terminal_projection_reconciled'))
              ORDER BY work.id LIMIT 2",
        )?;
        let matches = statement
            .query_map(
                params![repository, pull_request, head_sha],
                terminal_target_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        match matches.as_slice() {
            [target] => Ok(target.clone()),
            [] => Err(WorkLedgerError::Refused(
                "no exact terminal reconciliation target exists".to_owned(),
            )),
            _ => Err(WorkLedgerError::Refused(
                "terminal reconciliation target is ambiguous".to_owned(),
            )),
        }
    }

    pub(crate) fn plan_or_apply_terminal_reconciliation(
        state_dir: &std::path::Path,
        request: &TerminalReconciliationRequest,
        apply: bool,
        pre_apply_hook: impl FnOnce() -> WorkLedgerResult<()>,
    ) -> WorkLedgerResult<TerminalReconciliationReport> {
        validate_request(request)?;
        let ledger = Self::open_existing(state_dir)?.ok_or_else(|| {
            WorkLedgerError::Refused("native work ledger is unavailable".to_owned())
        })?;
        if !apply {
            return ledger.plan_terminal_reconciliation(request);
        }

        let directory = ledger
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?
            .to_path_buf();
        drop(ledger);
        let _writer_domain =
            crate::writer_domain_lease::acquire_exclusive_for_protected_path(&directory)?;
        let ledger = Self::open_under_writer_domain(state_dir)?;
        let evidence = terminal_evidence_bytes(request)?;
        let evidence_digest = digest(&evidence);
        ledger.discard_exact_unregistered_protected_object_with_writer_domain(
            &request.work_id,
            ProtectedObjectKind::ProviderReceipt,
            None,
            &evidence_digest,
            &evidence,
        )?;
        let planned = ledger.plan_terminal_reconciliation(request)?;
        if planned.replay {
            return Ok(planned);
        }
        pre_apply_hook()?;
        let current = ledger.plan_terminal_reconciliation(request)?;
        if current != planned {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation snapshot changed before apply".to_owned(),
            ));
        }
        let staged = ledger.stage_protected_object_with_writer_domain(
            &request.work_id,
            ProtectedObjectKind::ProviderReceipt,
            None,
            &evidence_digest,
            &evidence,
        )?;
        if let Err(error) = ledger.apply_terminal_reconciliation(request, &evidence_digest, &staged)
        {
            if !staged.already_registered {
                ledger.discard_exact_unregistered_protected_object_with_writer_domain(
                    &request.work_id,
                    ProtectedObjectKind::ProviderReceipt,
                    None,
                    &evidence_digest,
                    &evidence,
                )?;
            }
            return Err(error);
        }
        let mut replay = ledger.plan_terminal_reconciliation(request)?;
        if !replay.replay {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation did not replay exactly after apply".to_owned(),
            ));
        }
        replay.applied = true;
        Ok(replay)
    }

    fn plan_terminal_reconciliation(
        &self,
        request: &TerminalReconciliationRequest,
    ) -> WorkLedgerResult<TerminalReconciliationReport> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        self.verify_protected_object_storage(&connection)?;
        validate_existing_native_authority(&connection, request)?;
        let binding = projection_binding(&connection, &request.work_id)?;
        if binding.is_none() && ownership_root_exists(&connection, &request.work_id)? {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation found an ownership root without its projection binding"
                    .to_owned(),
            ));
        }
        let evidence_digest = digest(&terminal_evidence_bytes(request)?);
        let event_exists = reconciliation_event_exists(&connection, request, &evidence_digest)?;
        let receipt_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM protected_objects
                            WHERE work_item_id = ?1 AND kind = 'provider_receipt'
                              AND content_digest = ?2)",
            params![request.work_id, evidence_digest],
            |row| row.get(0),
        )?;
        let replay = match binding {
            None if event_exists => {
                return Err(WorkLedgerError::Refused(
                    "terminal reconciliation event exists without its binding".to_owned(),
                ));
            }
            None => false,
            Some(binding) if binding == expected_binding(request) => {
                if !event_exists || !receipt_exists {
                    return Err(WorkLedgerError::Refused(
                        "terminal reconciliation binding lacks its exact durable receipt"
                            .to_owned(),
                    ));
                }
                true
            }
            Some(_) => {
                return Err(WorkLedgerError::Refused(
                    "terminal reconciliation binding disagrees".to_owned(),
                ));
            }
        };
        let plan_sha256 = reconciliation_plan_digest(request, &evidence_digest)?;
        Ok(TerminalReconciliationReport {
            applied: false,
            replay,
            work_id: request.work_id.clone(),
            workstream_handle: request.workstream_handle.clone(),
            repository: request.repository.clone(),
            pull_request: request.pull_request,
            exact_head: request.head_sha.clone(),
            merge_sha: request.merge_sha.clone(),
            merged_at: request.merged_at.clone(),
            receipt_sha256: evidence_digest,
            plan_sha256,
        })
    }

    fn apply_terminal_reconciliation(
        &self,
        request: &TerminalReconciliationRequest,
        evidence_digest: &str,
        staged: &super::protected_objects::StagedProtectedObject,
    ) -> WorkLedgerResult<()> {
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_existing_native_authority(&transaction, request)?;
        if projection_binding(&transaction, &request.work_id)?.is_some() {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation binding appeared before commit".to_owned(),
            ));
        }
        if ownership_root_exists(&transaction, &request.work_id)? {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation found an ownership root without its projection binding"
                    .to_owned(),
            ));
        }
        let registered: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM protected_objects WHERE object_ref = ?1)",
            [&staged.record.object_ref],
            |row| row.get(0),
        )?;
        if registered != staged.already_registered {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation receipt registration changed before commit".to_owned(),
            ));
        }
        if !registered {
            transaction.execute(
                "INSERT INTO protected_objects
                 (object_ref, work_item_id, kind, profile_ref, storage_name,
                  content_digest, byte_length, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    staged.record.object_ref,
                    staged.record.work_item_id,
                    staged.record.kind,
                    staged.record.profile_ref,
                    staged.storage_name,
                    staged.record.content_digest,
                    staged.record.byte_length,
                    Utc::now().to_rfc3339(),
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO workstream_projection_bindings
             (work_item_id, workstream_handle, plan_sha256, root_revision, issue_revision,
              projection_revision, material_event_revision, repository_provider,
              repository_id, repository, exact_head, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                request.work_id,
                request.workstream_handle,
                request.plan_sha256,
                request.root_revision,
                request.issue_revision,
                request.projection_revision,
                request.material_event_revision,
                request.repository_provider,
                request.repository_id,
                request.repository,
                request.head_sha,
                request.merged_at,
            ],
        )?;
        record_event(
            &transaction,
            &request.work_id,
            request.work_generation,
            request.owner_generation,
            "terminal_projection_reconciled",
            Some(LifecycleState::Terminal),
            LifecycleState::Terminal,
            evidence_digest,
            &request.merged_at,
        )?;
        transaction.commit()?;
        Ok(())
    }
}

fn terminal_target_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<TerminalReconciliationTarget> {
    Ok(TerminalReconciliationTarget {
        work_id: row.get(0)?,
        repository: row.get(1)?,
        pull_request: row.get(2)?,
        exact_head: row.get(3)?,
        base_ref: row.get(4)?,
        phase: row.get(5)?,
        work_generation: row.get(6)?,
        owner_generation: row.get(7)?,
        profile_digest: row.get(8)?,
        route_ref: row.get(9)?,
        wake_id: row.get(10)?,
        source_digest: row.get(11)?,
    })
}

type ProjectionBinding = (
    String,
    String,
    u64,
    u64,
    u64,
    u64,
    String,
    String,
    String,
    String,
);

fn projection_binding(
    connection: &rusqlite::Connection,
    work_id: &str,
) -> WorkLedgerResult<Option<ProjectionBinding>> {
    connection
        .query_row(
            "SELECT workstream_handle, plan_sha256, root_revision, issue_revision,
                    projection_revision, material_event_revision, repository_provider,
                    repository_id, repository, exact_head
               FROM workstream_projection_bindings WHERE work_item_id = ?1",
            [work_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            },
        )
        .optional()
        .map_err(Into::into)
}

fn expected_binding(request: &TerminalReconciliationRequest) -> ProjectionBinding {
    (
        request.workstream_handle.clone(),
        request.plan_sha256.clone(),
        request.root_revision,
        request.issue_revision,
        request.projection_revision,
        request.material_event_revision,
        request.repository_provider.clone(),
        request.repository_id.clone(),
        request.repository.clone(),
        request.head_sha.clone(),
    )
}

fn validate_existing_native_authority(
    connection: &rusqlite::Connection,
    request: &TerminalReconciliationRequest,
) -> WorkLedgerResult<()> {
    let exact: bool = connection.query_row(
        "SELECT kind = 'terminal_handoff' AND lower(repo) = ?2 AND pr = ?3
                AND lower(head_sha) = ?4 AND base_ref = ?5 AND phase = 'terminal'
                AND work_generation = ?6 AND owner_generation = ?7
                AND repair_route_ref = ?8 AND source_digest = ?9
           FROM work_items WHERE id = ?1",
        params![
            request.work_id,
            request.repository,
            request.pull_request,
            request.head_sha,
            request.base_ref,
            request.work_generation,
            request.owner_generation,
            request.route_ref,
            request.source_digest,
        ],
        |row| row.get(0),
    )?;
    if !exact {
        return Err(WorkLedgerError::Refused(
            "terminal reconciliation local authority changed".to_owned(),
        ));
    }
    let profile_ref = OpaqueRef::derive("launch-profile", request.profile_digest.as_bytes())
        .as_str()
        .to_owned();
    let route_exact: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM route_records
                        WHERE route_ref = ?1 AND work_item_id = ?2 AND head_sha = ?3
                          AND owner_generation = ?4)",
        params![
            request.route_ref,
            request.work_id,
            request.head_sha,
            request.owner_generation,
        ],
        |row| row.get(0),
    )?;
    let profile_exact: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM protected_objects
                        WHERE work_item_id = ?1 AND kind = 'launch_profile'
                          AND profile_ref = ?2 AND content_digest = ?3)",
        params![request.work_id, profile_ref, request.profile_digest],
        |row| row.get(0),
    )?;
    let wakes = connection
        .prepare(
            "SELECT wake_id, route_ref, profile_ref, payload_digest, state,
                    work_generation, owner_generation
               FROM outbox WHERE work_item_id = ?1 AND wake_id = ?2 LIMIT 2",
        )?
        .query_map(params![request.work_id, request.wake_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, u64>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let wake_exact = matches!(wakes.as_slice(), [wake] if
        wake.0 == request.wake_id
            && wake.1 == request.route_ref
            && wake.3 == request.profile_digest
            && wake.5.checked_add(1) == Some(request.work_generation)
            && wake.6 == request.owner_generation
            && (wake.2.as_deref() == Some(profile_ref.as_str())
                || (wake.2.is_none() && wake.4 == "failed")));
    let continuations_exact: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM continuation_contracts
                        WHERE work_item_id = ?1 AND success_contract_digest = ?2
                          AND failure_contract_digest = ?3)",
        params![
            request.work_id,
            request.success_continuation_digest,
            request.failure_continuation_digest,
        ],
        |row| row.get(0),
    )?;
    if !route_exact || !profile_exact || !wake_exact || !continuations_exact {
        let missing = [
            (!route_exact).then_some("route"),
            (!profile_exact).then_some("profile"),
            (!wake_exact).then_some("wake"),
            (!continuations_exact).then_some("continuations"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(",");
        return Err(WorkLedgerError::Refused(format!(
            "terminal reconciliation native authority is incomplete or changed: {missing}"
        )));
    }
    Ok(())
}

fn ownership_root_exists(
    connection: &rusqlite::Connection,
    work_id: &str,
) -> WorkLedgerResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM ownership_roots WHERE work_item_id = ?1)",
            [work_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn reconciliation_event_exists(
    connection: &rusqlite::Connection,
    request: &TerminalReconciliationRequest,
    evidence_digest: &str,
) -> WorkLedgerResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM events
                            WHERE work_item_id = ?1 AND work_generation = ?2
                              AND owner_generation = ?3
                              AND kind = 'terminal_projection_reconciled'
                              AND from_state = 'terminal' AND to_state = 'terminal'
                              AND payload_digest = ?4)",
            params![
                request.work_id,
                request.work_generation,
                request.owner_generation,
                evidence_digest,
            ],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn validate_request(request: &TerminalReconciliationRequest) -> WorkLedgerResult<()> {
    validate_target(&request.repository, request.pull_request, &request.head_sha)?;
    validate_token("repository identity", &request.repository_id)?;
    validate_token("pull request node identity", &request.pull_request_node_id)?;
    validate_opaque_ref("work ID", &request.work_id, "wi")?;
    validate_opaque_ref("route reference", &request.route_ref, "route")?;
    validate_opaque_ref("wake ID", &request.wake_id, "wake")?;
    super::validate_workstream_handle(&request.workstream_handle)?;
    for (name, value) in [
        ("profile digest", &request.profile_digest),
        ("source digest", &request.source_digest),
        ("plan digest", &request.plan_sha256),
        (
            "success continuation digest",
            &request.success_continuation_digest,
        ),
        (
            "failure continuation digest",
            &request.failure_continuation_digest,
        ),
    ] {
        validate_digest(name, value)?;
    }
    validate_commit_sha("head SHA", &request.head_sha)?;
    validate_commit_sha("merge SHA", &request.merge_sha)?;
    if request.repository_provider != "github.com"
        || request.github_installation_id == 0
        || request.work_generation == 0
        || request.owner_generation == 0
        || request.projection_revision == 0
        || request.base_ref.is_empty()
        || request.base_ref.len() > 255
        || chrono::DateTime::parse_from_rfc3339(&request.merged_at).is_err()
    {
        return Err(WorkLedgerError::Refused(
            "terminal reconciliation authority is incomplete".to_owned(),
        ));
    }
    Ok(())
}

fn validate_target(repository: &str, pull_request: u64, head_sha: &str) -> WorkLedgerResult<()> {
    if !super::is_canonical_repo_slug(repository)
        || repository != repository.to_ascii_lowercase()
        || pull_request == 0
        || head_sha.len() != 40
        || head_sha != head_sha.to_ascii_lowercase()
        || head_sha.bytes().any(|byte| !byte.is_ascii_hexdigit())
    {
        return Err(WorkLedgerError::Refused(
            "terminal reconciliation target is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn validate_unique_targets(items: &[TerminalReconciliationTarget]) -> WorkLedgerResult<()> {
    let unique = items
        .iter()
        .map(|item| (&item.repository, item.pull_request, &item.exact_head))
        .collect::<std::collections::BTreeSet<_>>();
    if unique.len() != items.len() {
        return Err(WorkLedgerError::Refused(
            "terminal reconciliation inventory contains ambiguous targets".to_owned(),
        ));
    }
    Ok(())
}

fn inventory_digest(items: &[TerminalReconciliationTarget]) -> WorkLedgerResult<String> {
    let bound = items
        .iter()
        .map(|item| {
            (
                &item.work_id,
                &item.repository,
                item.pull_request,
                &item.exact_head,
                &item.base_ref,
                &item.phase,
                item.work_generation,
                item.owner_generation,
                &item.profile_digest,
                &item.route_ref,
                &item.wake_id,
                &item.source_digest,
            )
        })
        .collect::<Vec<_>>();
    serde_json::to_vec(&("terminal-reconciliation-inventory-v1", bound))
        .map(|bytes| digest(&bytes))
        .map_err(|error| WorkLedgerError::Refused(format!("encode terminal inventory: {error}")))
}

fn terminal_evidence_bytes(request: &TerminalReconciliationRequest) -> WorkLedgerResult<Vec<u8>> {
    serde_json::to_vec(&TerminalEvidenceV1 {
        schema_version: 1,
        repository_provider: &request.repository_provider,
        repository_id: &request.repository_id,
        repository: &request.repository,
        pull_request_node_id: &request.pull_request_node_id,
        pull_request: request.pull_request,
        exact_head: &request.head_sha,
        base_ref: &request.base_ref,
        disposition: "merged",
        merge_sha: &request.merge_sha,
        merged_at: &request.merged_at,
        github_installation_id: request.github_installation_id,
        work_id: &request.work_id,
        work_generation: request.work_generation,
        owner_generation: request.owner_generation,
        source_digest: &request.source_digest,
        workstream_handle: &request.workstream_handle,
    })
    .map_err(|error| WorkLedgerError::Refused(format!("encode terminal evidence: {error}")))
}

fn reconciliation_plan_digest(
    request: &TerminalReconciliationRequest,
    evidence_digest: &str,
) -> WorkLedgerResult<String> {
    let bytes = serde_json::to_vec(&(
        "terminal-reconciliation-plan-v1",
        &request.work_id,
        request.work_generation,
        request.owner_generation,
        &request.source_digest,
        &request.route_ref,
        &request.wake_id,
        &request.profile_digest,
        &request.workstream_handle,
        &request.plan_sha256,
        request.root_revision,
        request.issue_revision,
        request.projection_revision,
        request.material_event_revision,
        evidence_digest,
    ))
    .map_err(|error| WorkLedgerError::Refused(format!("encode reconciliation plan: {error}")))?;
    Ok(digest(&bytes))
}

fn validate_commit_sha(name: &str, value: &str) -> WorkLedgerResult<()> {
    if value.len() != 40
        || value != value.to_ascii_lowercase()
        || value.bytes().any(|byte| !byte.is_ascii_hexdigit())
    {
        return Err(WorkLedgerError::Refused(format!(
            "terminal reconciliation {name} is invalid"
        )));
    }
    Ok(())
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::work_ledger::{
        RepoPolicy, WakeIntent, native_publication_test_policy, native_publication_test_request,
    };
    use tempfile::TempDir;

    fn ownership_snapshot(ledger: &WorkLedger, work_id: &str) -> (i64, i64, i64, i64, i64) {
        let connection = ledger.connect_read_only().expect("ownership snapshot");
        connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM ownership_roots WHERE work_item_id = ?1),
                   (SELECT COUNT(*) FROM agent_ownership WHERE work_item_id = ?1),
                   (SELECT COUNT(*) FROM ownership_holder_materials WHERE work_item_id = ?1),
                   (SELECT COUNT(*) FROM ownership_lease_bootstrap_eligibility eligibility
                     JOIN agent_ownership ownership
                       ON ownership.ownership_id = eligibility.ownership_id
                    WHERE ownership.work_item_id = ?1),
                   (SELECT COUNT(*) FROM ownership_leases WHERE work_item_id = ?1)",
                [work_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("ownership counts")
    }

    #[allow(clippy::too_many_lines)] // One end-to-end legacy terminal authority fixture.
    pub(crate) fn seed_unbound_terminal_with_request(
        state_dir: &std::path::Path,
        publication: crate::work_ledger::NativePublicationRequest,
    ) -> (TerminalReconciliationRequest, WorkLedger) {
        let ledger = WorkLedger::open(state_dir).expect("ledger");
        ledger
            .set_repo_policy(
                &RepoPolicy {
                    repo: publication.repository.clone(),
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
            state_dir,
            &publication,
            &native_publication_test_policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("native publication");
        let connection = ledger.connect_read_write().expect("fixture connection");
        let (managed_generation, phase): (u64, String) = connection
            .query_row(
                "SELECT work_generation, phase FROM work_items WHERE id = ?1",
                [&report.work_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("managed generation");
        assert_eq!(phase, LifecycleState::Managed.as_str());
        drop(connection);
        ledger
            .transition_with_wake(
                &report.work_id,
                managed_generation,
                publication.owner_generation,
                LifecycleState::Actionable,
                None,
            )
            .expect("fixture actionable transition");
        let dispatch_generation = managed_generation + 1;
        let wake = WakeIntent::new(
            &report.work_id,
            dispatch_generation + 1,
            publication.owner_generation,
            report.route_ref.clone(),
            report.profile_digest.clone(),
        )
        .expect("fixture wake");
        assert_eq!(wake.wake_id, report.wake_id, "publication wake identity");
        ledger
            .transition_with_wake(
                &report.work_id,
                dispatch_generation,
                publication.owner_generation,
                LifecycleState::Dispatching,
                Some(&wake),
            )
            .expect("fixture dispatch transition");
        ledger
            .transition_with_wake(
                &report.work_id,
                dispatch_generation + 1,
                publication.owner_generation,
                LifecycleState::Terminal,
                None,
            )
            .expect("fixture terminal transition");
        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute_batch(
                "DROP TRIGGER workstream_projection_binding_no_delete;
                 DROP TRIGGER ownership_root_no_delete;",
            )
            .expect("fixture removes production delete guard");
        connection
            .execute(
                "DELETE FROM ownership_roots WHERE work_item_id = ?1",
                [&report.work_id],
            )
            .expect("fixture removes post-binding ownership root");
        connection
            .execute(
                "DELETE FROM workstream_projection_bindings WHERE work_item_id = ?1",
                [&report.work_id],
            )
            .expect("fixture removes projection binding");
        let (work_generation, source_digest) = connection
            .query_row(
                "SELECT work_generation, source_digest FROM work_items WHERE id = ?1",
                [&report.work_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("work generation");
        drop(connection);
        let request = TerminalReconciliationRequest {
            repository_provider: publication.repository_provider.clone(),
            repository_id: publication.repository_id.clone(),
            repository: publication.repository.clone(),
            pull_request_node_id: "PR_test_terminal".to_owned(),
            pull_request: publication.pull_request,
            head_sha: publication.head_sha.clone(),
            base_ref: publication.base_ref.clone(),
            merge_sha: "c".repeat(40),
            merged_at: "2026-09-01T12:00:00Z".to_owned(),
            github_installation_id: publication.github_installation_id,
            work_id: report.work_id,
            work_generation,
            owner_generation: publication.owner_generation,
            source_digest,
            route_ref: report.route_ref,
            wake_id: report.wake_id,
            profile_digest: report.profile_digest,
            workstream_handle: publication.workstream_handle,
            plan_sha256: publication.plan_sha256,
            root_revision: publication.root_revision,
            issue_revision: publication.issue_revision,
            projection_revision: publication.projection_revision,
            material_event_revision: publication.material_event_revision,
            success_continuation_digest: publication.success_continuation_digest,
            failure_continuation_digest: publication.failure_continuation_digest,
        };
        (request, ledger)
    }

    pub(crate) fn seed_unbound_terminal(
        state_dir: &std::path::Path,
    ) -> (TerminalReconciliationRequest, WorkLedger) {
        seed_unbound_terminal_with_request(state_dir, native_publication_test_request())
    }

    fn table_count(ledger: &WorkLedger, table: &str) -> i64 {
        ledger
            .connect_read_only()
            .expect("read connection")
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("table count")
    }

    fn outbox_snapshot(
        ledger: &WorkLedger,
        work_id: &str,
    ) -> Vec<(String, String, String, String)> {
        let connection = ledger.connect_read_only().expect("read connection");
        let mut statement = connection
            .prepare(
                "SELECT wake_id, state, route_ref, payload_digest
                   FROM outbox WHERE work_item_id = ?1 ORDER BY wake_id",
            )
            .expect("outbox snapshot statement");
        statement
            .query_map([work_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .expect("outbox snapshot rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("outbox snapshot")
    }

    #[test]
    fn terminal_reconciliation_is_bounded_dry_run_apply_and_exact_replay() {
        let temp = TempDir::new().expect("temp");
        let (request, ledger) = seed_unbound_terminal(temp.path());
        let inventory = ledger
            .terminal_reconciliation_inventory()
            .expect("bounded inventory");
        assert!(inventory.complete);
        assert_eq!(inventory.items.len(), 1);
        assert_eq!(inventory.items[0].work_id, request.work_id);
        let inventory_json = serde_json::to_string(&inventory).expect("redacted inventory JSON");
        assert!(!inventory_json.contains(&request.route_ref));
        assert!(!inventory_json.contains(&request.wake_id));
        assert!(!inventory_json.contains("source_digest"));
        let before = [
            ("outbox", table_count(&ledger, "outbox")),
            ("route_records", table_count(&ledger, "route_records")),
            (
                "continuation_contracts",
                table_count(&ledger, "continuation_contracts"),
            ),
            ("events", table_count(&ledger, "events")),
            (
                "protected_objects",
                table_count(&ledger, "protected_objects"),
            ),
        ];
        let planned =
            WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, false, || {
                panic!("dry-run must not invoke final authority read")
            })
            .expect("dry-run");
        assert!(!planned.applied);
        assert!(!planned.replay);
        for (table, count) in before {
            assert_eq!(
                table_count(&ledger, table),
                count,
                "dry-run changed {table}"
            );
        }

        let applied =
            WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, true, || {
                Ok(())
            })
            .expect("apply");
        assert!(applied.applied);
        assert!(applied.replay);
        assert_eq!(applied.plan_sha256, planned.plan_sha256);
        assert_eq!(table_count(&ledger, "outbox"), before[0].1);
        assert_eq!(table_count(&ledger, "route_records"), before[1].1);
        assert_eq!(table_count(&ledger, "continuation_contracts"), before[2].1);
        assert_eq!(table_count(&ledger, "events"), before[3].1 + 1);
        assert_eq!(table_count(&ledger, "protected_objects"), before[4].1 + 1);

        let replay =
            WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, true, || {
                panic!("exact replay must not invoke final authority read")
            })
            .expect("replay");
        assert!(!replay.applied);
        assert!(replay.replay);
        assert_eq!(replay.plan_sha256, planned.plan_sha256);
        assert!(
            ledger
                .terminal_reconciliation_inventory()
                .expect("post-reconciliation inventory")
                .items
                .is_empty()
        );
        assert!(
            ledger
                .native_steward_targets()
                .expect("terminal scheduler inventory")
                .is_empty(),
            "a repaired terminal row must remain inert to the scheduler"
        );
    }

    #[test]
    fn terminal_reconciliation_refuses_changed_authority_and_pre_apply_drift() {
        let temp = TempDir::new().expect("temp");
        let (request, _ledger) = seed_unbound_terminal(temp.path());
        let mut wrong_merge = request.clone();
        wrong_merge.merge_sha = "d".repeat(40);
        let wrong = WorkLedger::plan_or_apply_terminal_reconciliation(
            temp.path(),
            &wrong_merge,
            false,
            || Ok(()),
        )
        .expect("a distinct authoritative merge may be planned");
        let original =
            WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, false, || {
                Ok(())
            })
            .expect("original plan");
        assert_ne!(wrong.plan_sha256, original.plan_sha256);

        let error =
            WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, true, || {
                Err(WorkLedgerError::Refused(
                    "simulated final GitHub authority drift".to_owned(),
                ))
            })
            .expect_err("final authority drift must refuse");
        assert!(
            error
                .to_string()
                .contains("simulated final GitHub authority drift")
        );
        let ledger = WorkLedger::open_existing(temp.path())
            .expect("open")
            .expect("ledger");
        assert!(
            projection_binding(&ledger.connect_read_only().expect("read"), &request.work_id)
                .expect("binding query")
                .is_none()
        );
    }

    #[test]
    fn terminal_reconciliation_refuses_absent_mismatched_and_owned_targets() {
        let temp = TempDir::new().expect("temp");
        let (request, ledger) = seed_unbound_terminal(temp.path());
        let absent = ledger
            .terminal_reconciliation_target(
                &request.repository,
                request.pull_request + 1,
                &request.head_sha,
            )
            .expect_err("absent exact target must refuse");
        assert!(absent.to_string().contains("no exact terminal"));

        for (label, changed) in [
            ("repository", {
                let mut changed = request.clone();
                changed.repository = "different/repository".to_owned();
                changed
            }),
            ("pull request", {
                let mut changed = request.clone();
                changed.pull_request += 1;
                changed
            }),
            ("head", {
                let mut changed = request.clone();
                changed.head_sha = "d".repeat(40);
                changed
            }),
            ("base", {
                let mut changed = request.clone();
                changed.base_ref = "release".to_owned();
                changed
            }),
            ("source digest", {
                let mut changed = request.clone();
                changed.source_digest = digest(b"wrong source authority");
                changed
            }),
            ("wake", {
                let mut changed = request.clone();
                let replacement = if changed.wake_id.ends_with('0') {
                    "1"
                } else {
                    "0"
                };
                changed.wake_id.replace_range(68..69, replacement);
                changed
            }),
            ("route", {
                let mut changed = request.clone();
                let replacement = if changed.route_ref.ends_with('0') {
                    "1"
                } else {
                    "0"
                };
                changed.route_ref.replace_range(69..70, replacement);
                changed
            }),
        ] {
            let error = WorkLedger::plan_or_apply_terminal_reconciliation(
                temp.path(),
                &changed,
                false,
                || Ok(()),
            )
            .expect_err(label);
            assert!(
                error.to_string().contains("authority")
                    || error.to_string().contains("target is invalid"),
                "{label}: {error}"
            );
        }

        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute(
                "INSERT INTO ownership_roots(root_uuid, work_item_id, created_at)
                 VALUES (?1, ?2, ?3)",
                params![
                    "01234567-89ab-cdef-0123-456789abcdef",
                    request.work_id,
                    request.merged_at,
                ],
            )
            .expect("fixture orphan ownership root");
        drop(connection);
        let protected_before = table_count(&ledger, "protected_objects");
        let owned =
            WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, true, || {
                Ok(())
            })
            .expect_err("orphan ownership root must refuse before receipt staging");
        assert!(owned.to_string().contains("ownership root"));
        assert_eq!(
            table_count(&ledger, "protected_objects"),
            protected_before,
            "known-invalid authority must not stage a receipt"
        );
    }

    #[test]
    fn terminal_reconciliation_recovers_after_binding_transaction_rollback() {
        let temp = TempDir::new().expect("temp");
        let (request, ledger) = seed_unbound_terminal(temp.path());
        let protected_before = table_count(&ledger, "protected_objects");
        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute_batch(
                "CREATE TRIGGER test_refuse_terminal_reconciliation
                 BEFORE INSERT ON workstream_projection_bindings
                 BEGIN SELECT RAISE(ABORT, 'simulated binding transaction failure'); END;",
            )
            .expect("failure-capable binding trigger");
        drop(connection);

        let failed =
            WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, true, || {
                Ok(())
            })
            .expect_err("binding transaction must fail after file staging");
        assert!(
            failed
                .to_string()
                .contains("simulated binding transaction failure")
        );
        assert_eq!(table_count(&ledger, "protected_objects"), protected_before);
        assert!(
            projection_binding(&ledger.connect_read_only().expect("read"), &request.work_id)
                .expect("binding query")
                .is_none()
        );
        assert!(
            !reconciliation_event_exists(
                &ledger.connect_read_only().expect("read"),
                &request,
                &digest(&terminal_evidence_bytes(&request).expect("evidence")),
            )
            .expect("event query")
        );

        ledger
            .connect_read_write()
            .expect("fixture connection")
            .execute_batch("DROP TRIGGER test_refuse_terminal_reconciliation;")
            .expect("remove failure trigger");
        let recovered =
            WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, true, || {
                Ok(())
            })
            .expect("exact retry reuses staged receipt and commits binding");
        assert!(recovered.applied);
        assert!(recovered.replay);
        assert_eq!(
            table_count(&ledger, "protected_objects"),
            protected_before + 1,
            "recovery must register exactly one immutable receipt"
        );
    }

    #[test]
    fn terminal_reconciliation_recovers_exact_crash_orphan_before_replanning() {
        let temp = TempDir::new().expect("temp");
        let (request, ledger) = seed_unbound_terminal(temp.path());
        let protected_before = table_count(&ledger, "protected_objects");
        let evidence = terminal_evidence_bytes(&request).expect("evidence");
        let evidence_digest = digest(&evidence);
        let staged = ledger
            .stage_protected_object_with_writer_domain(
                &request.work_id,
                ProtectedObjectKind::ProviderReceipt,
                None,
                &evidence_digest,
                &evidence,
            )
            .expect("simulate crash after file publication");
        assert!(!staged.already_registered);
        assert_eq!(table_count(&ledger, "protected_objects"), protected_before);

        let recovered =
            WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, true, || {
                Ok(())
            })
            .expect("exact apply removes crash orphan and commits atomically");
        assert!(recovered.applied);
        assert!(recovered.replay);
        assert_eq!(
            table_count(&ledger, "protected_objects"),
            protected_before + 1
        );
    }

    #[test]
    fn terminal_reconciliation_ignores_and_preserves_unrelated_historical_wakes() {
        let temp = TempDir::new().expect("temp");
        let (request, ledger) = seed_unbound_terminal(temp.path());
        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute(
                "INSERT INTO outbox
                 (wake_id, work_item_id, work_generation, owner_generation, state,
                  route_ref, profile_ref, payload_digest, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'failed', ?5, NULL, ?6, ?7, ?7)",
                params![
                    format!("wake_{}", "1".repeat(64)),
                    request.work_id,
                    request.work_generation - 1,
                    request.owner_generation,
                    format!("route_{}", "2".repeat(64)),
                    digest(b"unrelated historical profile"),
                    request.merged_at,
                ],
            )
            .expect("unrelated historical wake");
        drop(connection);
        let before = outbox_snapshot(&ledger, &request.work_id);
        assert_eq!(before.len(), 2);
        let inventory = ledger
            .terminal_reconciliation_inventory()
            .expect("inventory ignores unrelated wake");
        assert_eq!(inventory.items.len(), 1);
        assert_eq!(inventory.items[0].wake_id, request.wake_id);

        WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, true, || Ok(()))
            .expect("reconcile with historical wake");
        assert_eq!(outbox_snapshot(&ledger, &request.work_id), before);
    }

    #[test]
    fn terminal_reconciliation_inventory_omits_nonterminal_rows_and_target_replays() {
        let temp = TempDir::new().expect("temp");
        let (request, ledger) = seed_unbound_terminal(temp.path());
        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute(
                "UPDATE work_items SET phase = 'waiting' WHERE id = ?1",
                [&request.work_id],
            )
            .expect("fixture nonterminal phase");
        drop(connection);
        assert!(
            ledger
                .terminal_reconciliation_inventory()
                .expect("nonterminal inventory")
                .items
                .is_empty()
        );
        ledger
            .connect_read_write()
            .expect("fixture connection")
            .execute(
                "UPDATE work_items SET phase = 'terminal' WHERE id = ?1",
                [&request.work_id],
            )
            .expect("restore terminal phase");

        let ownership_before = ownership_snapshot(&ledger, &request.work_id);
        assert_eq!(
            ownership_before.0, 0,
            "legacy row starts without root identity"
        );
        WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, true, || Ok(()))
            .expect("initial reconciliation");
        let ownership_after = ownership_snapshot(&ledger, &request.work_id);
        assert_eq!(
            ownership_after.0, 1,
            "binding mints one inert root identity"
        );
        assert_eq!(
            (
                ownership_after.1,
                ownership_after.2,
                ownership_after.3,
                ownership_after.4
            ),
            (
                ownership_before.1,
                ownership_before.2,
                ownership_before.3,
                ownership_before.4
            ),
            "terminal repair must not mint ownership authority or lease state"
        );
        assert!(
            ledger
                .terminal_reconciliation_inventory()
                .expect("unbound-only inventory")
                .items
                .is_empty()
        );
        let replay_target = ledger
            .terminal_reconciliation_target(
                &request.repository,
                request.pull_request,
                &request.head_sha,
            )
            .expect("targeted CLI resolver reaches exact replay");
        assert_eq!(replay_target.work_id, request.work_id);
        let replay =
            WorkLedger::plan_or_apply_terminal_reconciliation(temp.path(), &request, true, || {
                panic!("exact replay must not re-read final authority")
            })
            .expect("exact replay");
        assert!(replay.replay);
        assert!(!replay.applied);
    }
}
