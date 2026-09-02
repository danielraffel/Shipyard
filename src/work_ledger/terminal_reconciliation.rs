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

const STRANDED_PUBLICATION_INVENTORY_SQL: &str =
    "SELECT work.id, work.repo, work.pr, work.head_sha, work.base_ref,
            work.phase, work.work_generation, work.owner_generation, work.source_digest,
            (SELECT COUNT(*) FROM imports imported WHERE imported.work_item_id = work.id),
            (SELECT COUNT(*) FROM imports imported WHERE imported.work_item_id = work.id
              AND imported.content_digest = work.source_digest),
            (SELECT COUNT(*) FROM continuation_contracts continuation
              WHERE continuation.work_item_id = work.id),
            (SELECT COUNT(*) FROM route_records route WHERE route.work_item_id = work.id),
            (SELECT COUNT(*) FROM protected_objects object WHERE object.work_item_id = work.id),
            (SELECT COUNT(*) FROM protected_objects profile
              WHERE profile.work_item_id = work.id AND profile.kind = 'launch_profile'),
            (SELECT COUNT(*) FROM outbox wake WHERE wake.work_item_id = work.id),
            (SELECT COUNT(*) FROM outbox wake
              WHERE wake.work_item_id = work.id AND wake.state = 'uncertain'),
            (SELECT COUNT(*) FROM provider_deliveries delivery WHERE delivery.wake_id IN (
              SELECT wake.wake_id FROM outbox wake WHERE wake.work_item_id = work.id)),
            (SELECT COUNT(*) FROM ownership_roots root WHERE root.work_item_id = work.id),
            (SELECT COUNT(*) FROM agent_ownership ownership WHERE ownership.work_item_id = work.id),
            (SELECT COUNT(*) FROM ownership_holder_materials material
              WHERE material.work_item_id = work.id),
            (SELECT COUNT(*) FROM ownership_lease_bootstrap_eligibility eligibility
              WHERE eligibility.ownership_id IN (SELECT ownership.ownership_id
                FROM agent_ownership ownership WHERE ownership.work_item_id = work.id)),
            (SELECT COUNT(*) FROM ownership_leases lease WHERE lease.ownership_id IN (
              SELECT ownership.ownership_id FROM agent_ownership ownership
               WHERE ownership.work_item_id = work.id)),
            (SELECT COUNT(*) FROM activation_epochs activation
              WHERE activation.work_item_id = work.id),
            (SELECT COUNT(*) FROM projection_intents intent WHERE intent.work_item_id = work.id),
            (SELECT COUNT(*) FROM custody_outbox custody WHERE custody.wake_id IN (
              SELECT wake.wake_id FROM outbox wake WHERE wake.work_item_id = work.id))
       FROM work_items work
       LEFT JOIN workstream_projection_bindings binding ON binding.work_item_id = work.id
      WHERE binding.work_item_id IS NULL AND work.kind = 'terminal_handoff'
      ORDER BY work.id LIMIT ?1";

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
    #[serde(skip_serializing)]
    pub(crate) delivery_id: Option<String>,
}

/// Bounded no-write inventory used before selecting one exact repair target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct TerminalReconciliationInventory {
    pub(crate) snapshot_sha256: String,
    pub(crate) complete: bool,
    pub(crate) limit: usize,
    pub(crate) items: Vec<TerminalReconciliationTarget>,
    pub(crate) stranded_publications: Vec<StrandedPublicationTarget>,
}

/// Redacted precursor created before native publication could bind its
/// workstream projection. These rows are observable but remain ineligible for
/// the completed-terminal repair path.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct StrandedPublicationTarget {
    pub(crate) work_id: String,
    pub(crate) repository: Option<String>,
    pub(crate) pull_request: Option<u64>,
    pub(crate) exact_head: Option<String>,
    pub(crate) base_ref: Option<String>,
    pub(crate) phase: String,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) classification: String,
    pub(crate) terminal_reconciliation_eligible: bool,
    pub(crate) blocking_reasons: Vec<String>,
    pub(crate) related: StrandedPublicationRelatedCounts,
    #[serde(skip_serializing)]
    pub(crate) source_digest: String,
}

/// Bounded related-state census used to explain why an unbound row is or is
/// not the narrow publication precursor. Counts are evidence only; they never
/// authorize reconciliation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct StrandedPublicationRelatedCounts {
    pub(crate) imports: u64,
    pub(crate) matching_imports: u64,
    pub(crate) continuation_contracts: u64,
    pub(crate) routes: u64,
    pub(crate) protected_objects: u64,
    pub(crate) launch_profiles: u64,
    pub(crate) wakes: u64,
    pub(crate) uncertain_wakes: u64,
    pub(crate) provider_deliveries: u64,
    pub(crate) ownership_roots: u64,
    pub(crate) agent_ownership: u64,
    pub(crate) ownership_holder_materials: u64,
    pub(crate) ownership_bootstrap_eligibility: u64,
    pub(crate) ownership_leases: u64,
    pub(crate) activation_epochs: u64,
    pub(crate) projection_intents: u64,
    pub(crate) custody_outbox: u64,
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
    pub(crate) delivery_id: Option<String>,
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

#[derive(Serialize)]
struct UncertainDispatchEvidenceV1<'a> {
    schema_version: u32,
    repository_provider: &'a str,
    repository_id: &'a str,
    repository: &'a str,
    pull_request_node_id: &'a str,
    pull_request: u64,
    exact_head: &'a str,
    base_ref: &'a str,
    merge_sha: &'a str,
    merged_at: &'a str,
    github_installation_id: u64,
    work_id: &'a str,
    work_generation: u64,
    owner_generation: u64,
    source_digest: &'a str,
    route_ref: &'a str,
    wake_id: &'a str,
    delivery_id: &'a str,
    profile_digest: &'a str,
}

impl WorkLedger {
    /// Zero-write preview for an uncertain dispatch. The preview projects the
    /// next terminal generation in memory, then describes the same immutable
    /// receipt/event/binding that `--apply` will commit.
    pub(crate) fn plan_uncertain_dispatch_reconciliation(
        &self,
        request: &TerminalReconciliationRequest,
    ) -> WorkLedgerResult<TerminalReconciliationReport> {
        validate_request(request)?;
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        self.verify_protected_object_storage(&connection)?;
        validate_uncertain_dispatch_authority(&connection, request)?;
        if projection_binding(&connection, &request.work_id)?.is_some()
            || ownership_root_exists(&connection, &request.work_id)?
        {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation dispatch target already has projection state".to_owned(),
            ));
        }
        let mut projected = request.clone();
        projected.work_generation = projected
            .work_generation
            .checked_add(1)
            .ok_or_else(|| WorkLedgerError::Refused("work generation exhausted".to_owned()))?;
        let evidence_digest = digest(&terminal_evidence_bytes(&projected)?);
        let plan_sha256 = reconciliation_plan_digest(&projected, &evidence_digest)?;
        Ok(TerminalReconciliationReport {
            applied: false,
            replay: false,
            work_id: projected.work_id,
            workstream_handle: projected.workstream_handle,
            repository: projected.repository,
            pull_request: projected.pull_request,
            exact_head: projected.head_sha,
            merge_sha: projected.merge_sha,
            merged_at: projected.merged_at,
            receipt_sha256: evidence_digest,
            plan_sha256,
        })
    }

    /// Close a dispatching handoff whose provider wake is durably uncertain
    /// after an exact merged-head proof. This is deliberately separate from
    /// projection reconciliation: it only advances the existing lifecycle
    /// generation to terminal and never creates ownership or routing state.
    /// Require the caller's final authenticated authority reread while writer
    /// custody is held, fence the complete request, and atomically append the
    /// terminalization event with the lifecycle transition.
    pub(crate) fn finalize_uncertain_dispatch_with_authority(
        &self,
        request: &TerminalReconciliationRequest,
        final_authority: impl FnOnce() -> WorkLedgerResult<()>,
    ) -> WorkLedgerResult<TerminalReconciliationRequest> {
        validate_request(request)?;
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
        // Hold the immediate transaction while obtaining the final remote
        // proof so no competing writer can invalidate the fenced snapshot.
        final_authority()?;
        validate_uncertain_dispatch_authority(&transaction, request)?;
        let projected_generation = request
            .work_generation
            .checked_add(1)
            .ok_or_else(|| WorkLedgerError::Refused("work generation exhausted".to_owned()))?;
        let now = Utc::now().to_rfc3339();
        let changed = transaction.execute(
            "UPDATE work_items SET phase = 'terminal', work_generation = work_generation + 1,
                    updated_at = ?1
              WHERE id = ?2 AND kind = 'terminal_handoff'
                AND repo = ?3 AND pr = ?4 AND head_sha = ?5
                AND base_ref = ?6 AND source_digest = ?7 AND repair_route_ref = ?8
                AND phase = 'dispatching' AND work_generation = ?9
                AND owner_generation = ?10",
            params![
                now,
                request.work_id,
                request.repository,
                request.pull_request,
                request.head_sha,
                request.base_ref,
                request.source_digest,
                request.route_ref,
                request.work_generation,
                request.owner_generation,
            ],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation work generation no longer matches".to_owned(),
            ));
        }
        let payload = uncertain_dispatch_evidence_digest(request)?;
        record_event(
            &transaction,
            &request.work_id,
            projected_generation,
            request.owner_generation,
            "terminal_dispatch_reconciled",
            Some(LifecycleState::Dispatching),
            LifecycleState::Terminal,
            &payload,
            &now,
        )?;
        transaction.commit()?;
        let mut projected = request.clone();
        projected.work_generation = projected_generation;
        Ok(projected)
    }

    /// List only incomplete terminal projection identities. The ordinary
    /// inventory remains fail-closed and never treats these rows as authority.
    pub(crate) fn terminal_reconciliation_inventory(
        &self,
    ) -> WorkLedgerResult<TerminalReconciliationInventory> {
        let mut connection = self.connect_read_only()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        verify_supported_schema(&transaction)?;
        verify_integrity(&transaction)?;
        self.verify_protected_object_storage(&transaction)?;

        let mut items = {
            let mut statement = transaction.prepare(
                "SELECT work.id, work.repo, work.pr, work.head_sha, work.base_ref,
                    work.phase, work.work_generation, work.owner_generation,
                    profile.content_digest, work.repair_route_ref, wake.wake_id,
                    work.source_digest, delivery.delivery_id
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
                     OR (wake.profile_ref IS NULL AND wake.state IN ('failed', 'uncertain')))
               LEFT JOIN provider_deliveries delivery ON delivery.wake_id = wake.wake_id
              WHERE binding.work_item_id IS NULL AND work.kind = 'terminal_handoff'
                AND work.phase = 'terminal'
              ORDER BY work.id LIMIT ?1",
            )?;
            statement
                .query_map([MAX_UNBOUND_TERMINAL_QUERY_ROWS], terminal_target_from_row)?
                .collect::<Result<Vec<_>, _>>()?
        };
        validate_terminal_target_identities(&items)?;
        let mut stranded_publications = stranded_publication_inventory(&transaction, &items)?;
        if items.len() + stranded_publications.len() > MAX_UNBOUND_TERMINAL_TARGETS {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation inventory exceeds its bound".to_owned(),
            ));
        }
        validate_unique_targets(&items)?;
        classify_stranded_identity_ambiguity(&mut stranded_publications);
        validate_cross_bucket_identity_uniqueness(&items, &stranded_publications)?;
        let snapshot_sha256 = inventory_digest(&items, &stranded_publications)?;
        transaction.commit()?;
        Ok(TerminalReconciliationInventory {
            snapshot_sha256,
            complete: true,
            limit: MAX_UNBOUND_TERMINAL_TARGETS,
            items: std::mem::take(&mut items),
            stranded_publications: std::mem::take(&mut stranded_publications),
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
        let mut connection = self.connect_read_only()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        verify_supported_schema(&transaction)?;
        verify_integrity(&transaction)?;
        self.verify_protected_object_storage(&transaction)?;
        let identity_count: u64 = transaction.query_row(
            "SELECT COUNT(*) FROM work_items work
              WHERE work.kind = 'terminal_handoff'
                AND lower(work.repo) = ?1 AND work.pr = ?2 AND lower(work.head_sha) = ?3",
            params![repository, pull_request, head_sha],
            |row| row.get(0),
        )?;
        if identity_count != 1 {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation target identity is absent or ambiguous".to_owned(),
            ));
        }
        let matches = {
            let mut statement = transaction.prepare(
                "SELECT work.id, work.repo, work.pr, work.head_sha, work.base_ref,
                    work.phase, work.work_generation, work.owner_generation,
                    profile.content_digest, work.repair_route_ref, wake.wake_id,
                    work.source_digest, delivery.delivery_id
               FROM work_items work
               JOIN protected_objects profile
                 ON profile.work_item_id = work.id AND profile.kind = 'launch_profile'
               JOIN outbox wake
                 ON wake.work_item_id = work.id
                AND wake.route_ref = work.repair_route_ref
                AND wake.payload_digest = profile.content_digest
                AND (wake.work_generation = work.work_generation - 1
                     OR (work.phase = 'dispatching' AND wake.work_generation = work.work_generation))
                AND wake.owner_generation = work.owner_generation
                AND (wake.profile_ref = profile.profile_ref
                     OR (wake.profile_ref IS NULL AND wake.state IN ('failed', 'uncertain')))
               LEFT JOIN provider_deliveries delivery ON delivery.wake_id = wake.wake_id
              WHERE work.kind = 'terminal_handoff' AND work.phase IN ('terminal', 'dispatching')
                AND work.repo = ?1 AND work.pr = ?2 AND work.head_sha = ?3
                AND (NOT EXISTS(
                       SELECT 1 FROM workstream_projection_bindings binding
                        WHERE binding.work_item_id = work.id)
                     OR EXISTS(
                       SELECT 1 FROM events event
                        WHERE event.work_item_id = work.id
                          AND event.kind = 'terminal_projection_reconciled'))
              ORDER BY work.id LIMIT 2",
            )?;
            statement
                .query_map(
                    params![repository, pull_request, head_sha],
                    terminal_target_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        transaction.commit()?;
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
        delivery_id: row.get(12)?,
    })
}

fn stranded_publication_inventory(
    connection: &rusqlite::Connection,
    terminal_targets: &[TerminalReconciliationTarget],
) -> WorkLedgerResult<Vec<StrandedPublicationTarget>> {
    let mut statement = connection.prepare(STRANDED_PUBLICATION_INVENTORY_SQL)?;
    let rows = statement.query_map(
        [MAX_UNBOUND_TERMINAL_QUERY_ROWS],
        stranded_publication_from_row,
    )?;
    let terminal_work_ids = terminal_targets
        .iter()
        .map(|item| item.work_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    Ok(rows
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|item| !terminal_work_ids.contains(item.work_id.as_str()))
        .collect())
}

fn stranded_publication_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StrandedPublicationTarget> {
    let base_ref = row.get::<_, Option<String>>(4)?;
    let repository = row.get::<_, Option<String>>(1)?;
    let pull_request = row.get::<_, Option<u64>>(2)?;
    let exact_head = row.get::<_, Option<String>>(3)?;
    let phase = row.get::<_, String>(5)?;
    let work_generation = row.get::<_, u64>(6)?;
    let owner_generation = row.get::<_, u64>(7)?;
    let related = stranded_related_counts(row)?;
    let downstream = [
        ("continuation_contract", related.continuation_contracts),
        ("route", related.routes),
        ("protected_object", related.protected_objects),
        ("wake", related.wakes),
        ("provider_delivery", related.provider_deliveries),
        ("ownership_root", related.ownership_roots),
        ("agent_ownership", related.agent_ownership),
        (
            "ownership_holder_material",
            related.ownership_holder_materials,
        ),
        (
            "ownership_bootstrap_eligibility",
            related.ownership_bootstrap_eligibility,
        ),
        ("ownership_lease", related.ownership_leases),
        ("activation_epoch", related.activation_epochs),
        ("projection_intent", related.projection_intents),
        ("custody_outbox", related.custody_outbox),
    ];
    let mut blocking_reasons = downstream
        .into_iter()
        .filter(|(_, count)| *count != 0)
        .map(|(name, _)| format!("unexpected_{name}"))
        .collect::<Vec<_>>();
    if phase != LifecycleState::ShadowImported.as_str() {
        blocking_reasons.push("unexpected_phase".to_owned());
    }
    if work_generation != 1 {
        blocking_reasons.push("unexpected_generation".to_owned());
    }
    if owner_generation != 1 {
        blocking_reasons.push("unexpected_owner_generation".to_owned());
    }
    if base_ref.is_none() {
        blocking_reasons.push("missing_base_ref".to_owned());
    }
    if repository.is_none() {
        blocking_reasons.push("missing_repository".to_owned());
    }
    if pull_request.is_none() {
        blocking_reasons.push("missing_pull_request".to_owned());
    }
    if exact_head.is_none() {
        blocking_reasons.push("missing_exact_head".to_owned());
    }
    if let (Some(repository), Some(pull_request), Some(exact_head)) =
        (&repository, pull_request, &exact_head)
        && validate_target(repository, pull_request, exact_head).is_err()
    {
        blocking_reasons.push("invalid_target_identity".to_owned());
    }
    if base_ref
        .as_deref()
        .is_some_and(|base_ref| base_ref.is_empty() || base_ref.len() > 255)
    {
        blocking_reasons.push("invalid_base_ref".to_owned());
    }
    if related.imports != 1 {
        blocking_reasons.push("unexpected_import_count".to_owned());
    }
    if related.matching_imports != 1 {
        blocking_reasons.push("source_import_mismatch".to_owned());
    }
    let precursor_shape_matches = blocking_reasons.is_empty();
    let classification = if precursor_shape_matches {
        "publication_precursor"
    } else if phase == LifecycleState::Terminal.as_str() || downstream_state_exists(&related) {
        "managed_unbound"
    } else {
        "blocked"
    };
    Ok(StrandedPublicationTarget {
        work_id: row.get(0)?,
        repository,
        pull_request,
        exact_head,
        base_ref,
        phase,
        work_generation,
        owner_generation,
        classification: classification.to_owned(),
        terminal_reconciliation_eligible: false,
        blocking_reasons,
        related,
        source_digest: row.get(8)?,
    })
}

fn stranded_related_counts(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StrandedPublicationRelatedCounts> {
    Ok(StrandedPublicationRelatedCounts {
        imports: row.get(9)?,
        matching_imports: row.get(10)?,
        continuation_contracts: row.get(11)?,
        routes: row.get(12)?,
        protected_objects: row.get(13)?,
        launch_profiles: row.get(14)?,
        wakes: row.get(15)?,
        uncertain_wakes: row.get(16)?,
        provider_deliveries: row.get(17)?,
        ownership_roots: row.get(18)?,
        agent_ownership: row.get(19)?,
        ownership_holder_materials: row.get(20)?,
        ownership_bootstrap_eligibility: row.get(21)?,
        ownership_leases: row.get(22)?,
        activation_epochs: row.get(23)?,
        projection_intents: row.get(24)?,
        custody_outbox: row.get(25)?,
    })
}

fn downstream_state_exists(related: &StrandedPublicationRelatedCounts) -> bool {
    related.continuation_contracts != 0
        || related.routes != 0
        || related.protected_objects != 0
        || related.wakes != 0
        || related.provider_deliveries != 0
        || related.ownership_roots != 0
        || related.agent_ownership != 0
        || related.ownership_holder_materials != 0
        || related.ownership_bootstrap_eligibility != 0
        || related.ownership_leases != 0
        || related.activation_epochs != 0
        || related.projection_intents != 0
        || related.custody_outbox != 0
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

fn validate_uncertain_dispatch_authority(
    connection: &rusqlite::Connection,
    request: &TerminalReconciliationRequest,
) -> WorkLedgerResult<()> {
    let Some(delivery_id) = request.delivery_id.as_deref() else {
        return Err(WorkLedgerError::Refused(
            "terminal reconciliation requires an exact uncertain delivery identity".to_owned(),
        ));
    };
    let profile_ref = OpaqueRef::derive("launch-profile", request.profile_digest.as_bytes())
        .as_str()
        .to_owned();
    let exact: bool = connection.query_row(
        "SELECT COUNT(*) = 1
           FROM work_items work
           JOIN route_records route
            ON route.route_ref = work.repair_route_ref
            AND route.work_item_id = work.id
            AND route.head_sha = work.head_sha
            AND route.owner_generation = work.owner_generation
           JOIN protected_objects profile
             ON profile.work_item_id = work.id AND profile.kind = 'launch_profile'
            AND profile.profile_ref = ?12 AND profile.content_digest = ?13
           JOIN continuation_contracts continuation ON continuation.work_item_id = work.id
           JOIN outbox wake ON wake.work_item_id = work.id
            AND wake.wake_id = ?10 AND wake.state = 'uncertain'
            AND wake.route_ref = route.route_ref
            AND wake.payload_digest = profile.content_digest
            AND (wake.profile_ref = profile.profile_ref OR wake.profile_ref IS NULL)
            AND wake.work_generation = work.work_generation
            AND wake.owner_generation = work.owner_generation
           JOIN wake_attempts attempt ON attempt.wake_id = wake.wake_id
            AND attempt.state = 'uncertain'
           JOIN provider_deliveries delivery ON delivery.wake_id = wake.wake_id
            AND delivery.delivery_id = ?11 AND delivery.attempt = attempt.attempt
            AND delivery.adapter_id = attempt.adapter_id
            AND delivery.state = 'uncertain'
           JOIN activation_epochs activation ON activation.activation_id = delivery.activation_id
            AND activation.work_item_id = work.id
            AND activation.work_generation = work.work_generation
            AND activation.owner_generation = work.owner_generation
            AND activation.state = 'released'
           JOIN protected_objects provider_request
             ON provider_request.object_ref = delivery.request_object_ref
            AND provider_request.work_item_id = work.id
            AND provider_request.kind = 'provider_request'
          WHERE work.id = ?1 AND work.kind = 'terminal_handoff'
            AND work.repo = ?2 AND work.pr = ?3 AND work.head_sha = ?4
            AND work.base_ref = ?5 AND work.phase = 'dispatching'
            AND work.work_generation = ?6 AND work.owner_generation = ?7
            AND work.repair_route_ref = ?8 AND work.source_digest = ?9
            AND continuation.success_contract_digest = ?14
            AND continuation.failure_contract_digest = ?15
            AND NOT EXISTS (
                SELECT 1 FROM provider_deliveries other
                 WHERE other.wake_id = wake.wake_id AND other.delivery_id != ?11)
            AND NOT EXISTS (
                SELECT 1 FROM workstream_projection_bindings binding
                 WHERE binding.work_item_id = work.id)
            AND NOT EXISTS (
                SELECT 1 FROM ownership_roots root WHERE root.work_item_id = work.id)",
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
            request.wake_id,
            delivery_id,
            profile_ref,
            request.profile_digest,
            request.success_continuation_digest,
            request.failure_continuation_digest,
        ],
        |row| row.get(0),
    )?;
    if !exact {
        return Err(WorkLedgerError::Refused(
            "terminal reconciliation uncertain dispatch authority is incomplete or changed"
                .to_owned(),
        ));
    }
    Ok(())
}

fn uncertain_dispatch_evidence_digest(
    request: &TerminalReconciliationRequest,
) -> WorkLedgerResult<String> {
    let delivery_id = request.delivery_id.as_deref().ok_or_else(|| {
        WorkLedgerError::Refused(
            "terminal reconciliation requires an exact uncertain delivery identity".to_owned(),
        )
    })?;
    let bytes = serde_json::to_vec(&UncertainDispatchEvidenceV1 {
        schema_version: 1,
        repository_provider: &request.repository_provider,
        repository_id: &request.repository_id,
        repository: &request.repository,
        pull_request_node_id: &request.pull_request_node_id,
        pull_request: request.pull_request,
        exact_head: &request.head_sha,
        base_ref: &request.base_ref,
        merge_sha: &request.merge_sha,
        merged_at: &request.merged_at,
        github_installation_id: request.github_installation_id,
        work_id: &request.work_id,
        work_generation: request.work_generation,
        owner_generation: request.owner_generation,
        source_digest: &request.source_digest,
        route_ref: &request.route_ref,
        wake_id: &request.wake_id,
        delivery_id,
        profile_digest: &request.profile_digest,
    })
    .map_err(|error| {
        WorkLedgerError::Refused(format!("encode terminal dispatch evidence: {error}"))
    })?;
    Ok(digest(&bytes))
}

#[allow(clippy::too_many_lines)] // Validate the complete legacy native authority in one snapshot.
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
    let wake_exact = if let [wake] = wakes.as_slice() {
        wake.0 == request.wake_id
            && wake.1 == request.route_ref
            && wake.3 == request.profile_digest
            && wake.5.checked_add(1) == Some(request.work_generation)
            && wake.6 == request.owner_generation
            && (wake.2.as_deref() == Some(profile_ref.as_str())
                || (wake.2.is_none() && (wake.4 == "failed" || wake.4 == "uncertain")))
    } else {
        false
    };
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
    let terminalization_exact = if let Some(delivery_id) = request.delivery_id.as_deref() {
        let mut dispatch_request = request.clone();
        dispatch_request.work_generation = dispatch_request
            .work_generation
            .checked_sub(1)
            .ok_or_else(|| WorkLedgerError::Refused("work generation underflow".to_owned()))?;
        let event_digest = uncertain_dispatch_evidence_digest(&dispatch_request)?;
        connection.query_row(
            "SELECT COUNT(*) = 1
               FROM events event
               JOIN provider_deliveries delivery ON delivery.wake_id = ?2
               JOIN wake_attempts attempt ON attempt.wake_id = delivery.wake_id
                AND attempt.attempt = delivery.attempt
              WHERE event.work_item_id = ?1
                AND event.work_generation = ?3 AND event.owner_generation = ?4
                AND event.kind = 'terminal_dispatch_reconciled'
                AND event.from_state = 'dispatching' AND event.to_state = 'terminal'
                AND event.payload_digest = ?5
                AND delivery.delivery_id = ?6 AND delivery.state = 'uncertain'
                AND attempt.state = 'uncertain'
                AND NOT EXISTS (SELECT 1 FROM provider_deliveries other
                                 WHERE other.wake_id = ?2 AND other.delivery_id != ?6)",
            params![
                request.work_id,
                request.wake_id,
                request.work_generation,
                request.owner_generation,
                event_digest,
                delivery_id,
            ],
            |row| row.get(0),
        )?
    } else {
        true
    };
    if !route_exact
        || !profile_exact
        || !wake_exact
        || !continuations_exact
        || !terminalization_exact
    {
        let missing = [
            (!route_exact).then_some("route"),
            (!profile_exact).then_some("profile"),
            (!wake_exact).then_some("wake"),
            (!continuations_exact).then_some("continuations"),
            (!terminalization_exact).then_some("dispatch terminalization"),
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
    if let Some(delivery_id) = request.delivery_id.as_deref() {
        validate_opaque_ref("delivery ID", delivery_id, "pd")?;
    }
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

fn validate_terminal_target_identities(
    items: &[TerminalReconciliationTarget],
) -> WorkLedgerResult<()> {
    for item in items {
        validate_target(&item.repository, item.pull_request, &item.exact_head)?;
        if item.base_ref.is_empty() || item.base_ref.len() > 255 {
            return Err(WorkLedgerError::Refused(
                "terminal reconciliation target base is invalid".to_owned(),
            ));
        }
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

fn validate_cross_bucket_identity_uniqueness(
    terminal_targets: &[TerminalReconciliationTarget],
    stranded: &[StrandedPublicationTarget],
) -> WorkLedgerResult<()> {
    let terminal_identities = terminal_targets
        .iter()
        .map(|item| {
            (
                item.repository.to_ascii_lowercase(),
                item.pull_request,
                item.exact_head.to_ascii_lowercase(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    if stranded.iter().any(|item| {
        item.repository
            .as_deref()
            .zip(item.pull_request)
            .zip(item.exact_head.as_deref())
            .is_some_and(|((repository, pull_request), exact_head)| {
                terminal_identities.contains(&(
                    repository.to_ascii_lowercase(),
                    pull_request,
                    exact_head.to_ascii_lowercase(),
                ))
            })
    }) {
        return Err(WorkLedgerError::Refused(
            "terminal reconciliation inventory contains a cross-class target collision".to_owned(),
        ));
    }
    Ok(())
}

fn inventory_digest(
    items: &[TerminalReconciliationTarget],
    stranded_publications: &[StrandedPublicationTarget],
) -> WorkLedgerResult<String> {
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
                &item.delivery_id,
            )
        })
        .collect::<Vec<_>>();
    let stranded = stranded_publications
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
                &item.classification,
                item.terminal_reconciliation_eligible,
                &item.blocking_reasons,
                &item.related,
                &item.source_digest,
            )
        })
        .collect::<Vec<_>>();
    serde_json::to_vec(&("terminal-reconciliation-inventory-v2", bound, stranded))
        .map(|bytes| digest(&bytes))
        .map_err(|error| WorkLedgerError::Refused(format!("encode terminal inventory: {error}")))
}

fn classify_stranded_identity_ambiguity(items: &mut [StrandedPublicationTarget]) {
    let counts = items
        .iter()
        .map(|item| {
            (
                item.repository
                    .as_ref()
                    .map(|value| value.to_ascii_lowercase()),
                item.pull_request,
                item.exact_head
                    .as_ref()
                    .map(|value| value.to_ascii_lowercase()),
            )
        })
        .fold(std::collections::BTreeMap::new(), |mut counts, identity| {
            *counts.entry(identity).or_insert(0_usize) += 1;
            counts
        });
    for item in items {
        let identity = (
            item.repository
                .as_ref()
                .map(|value| value.to_ascii_lowercase()),
            item.pull_request,
            item.exact_head
                .as_ref()
                .map(|value| value.to_ascii_lowercase()),
        );
        if counts.get(&identity).copied().unwrap_or_default() > 1 {
            "blocked".clone_into(&mut item.classification);
            item.blocking_reasons
                .push("ambiguous_target_identity".to_owned());
        }
    }
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
    let bytes = if let Some(delivery_id) = request.delivery_id.as_deref() {
        serde_json::to_vec(&(
            "terminal-reconciliation-plan-v2",
            &request.work_id,
            request.work_generation,
            request.owner_generation,
            &request.source_digest,
            &request.route_ref,
            &request.wake_id,
            delivery_id,
            &request.profile_digest,
            &request.workstream_handle,
            &request.plan_sha256,
            request.root_revision,
            request.issue_revision,
            request.projection_revision,
            request.material_event_revision,
            evidence_digest,
        ))
    } else {
        // Preserve byte-for-byte legacy plan identities and exact replay.
        serde_json::to_vec(&(
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
    }
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
        ContinuationSet, ImportCandidate, RepoPolicy, WakeIntent, digest,
        native_publication_test_policy, native_publication_test_request, opaque_ref,
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
            delivery_id: None,
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

    fn seed_stranded_publication_precursor(state_dir: &std::path::Path) -> (WorkLedger, String) {
        let ledger = WorkLedger::open(state_dir).expect("ledger");
        let repository = "owner/repo";
        let pull_request = 74;
        let exact_head = "a".repeat(40);
        let workstream = "GEN-37";
        let work_seed =
            format!("github.com\nrepository-node\n{pull_request}\n{exact_head}\n{workstream}");
        let work_id = opaque_ref(
            "wi",
            &format!("shipyard-native-continuation-v2\n{work_seed}"),
        );
        ledger
            .import_candidates(&[ImportCandidate {
                work_id: work_id.clone(),
                kind: "terminal_handoff".to_owned(),
                repo: Some(repository.to_owned()),
                pr: Some(pull_request),
                head_sha: Some(exact_head),
                base_ref: Some("main".to_owned()),
                goal_id: Some(opaque_ref("goal", workstream)),
                goal_generation: 1,
                lane: Some("fresh_agent_continuation".to_owned()),
                role: "root".to_owned(),
                owner_id: Some(opaque_ref("owner", "owner")),
                owner_generation: 1,
                terminal_adapter: Some("session_host".to_owned()),
                agent_adapter: Some("codex".to_owned()),
                provider_adapter: Some("codex".to_owned()),
                coordinator_route_ref: None,
                repair_route_ref: Some(opaque_ref("route", "pending-route")),
                pr_truth: "unknown".to_owned(),
                acceptance_truth: "unknown".to_owned(),
                continuation_truth: "pending".to_owned(),
                phase: LifecycleState::ShadowImported.as_str().to_owned(),
                source_ref: opaque_ref("src", "pending-publication"),
                content_digest: digest(b"pending publication authority"),
                source_updated_at: None,
            }])
            .expect("stranded precursor");
        (ledger, work_id)
    }

    #[test]
    fn terminal_reconciliation_inventory_types_stranded_publication_precursors() {
        let temp = TempDir::new().expect("temp");
        let (ledger, work_id) = seed_stranded_publication_precursor(temp.path());

        let inventory = ledger
            .terminal_reconciliation_inventory()
            .expect("typed inventory");
        assert!(inventory.items.is_empty());
        assert_eq!(inventory.stranded_publications.len(), 1);
        let stranded = &inventory.stranded_publications[0];
        assert_eq!(stranded.work_id, work_id);
        assert_eq!(stranded.classification, "publication_precursor");
        assert!(!stranded.terminal_reconciliation_eligible);
        assert!(stranded.blocking_reasons.is_empty());
        assert_eq!(stranded.related.imports, 1);
        assert_eq!(stranded.related.matching_imports, 1);
        let rendered = serde_json::to_string(&inventory).expect("inventory JSON");
        assert!(!rendered.contains(&stranded.source_digest));

        let mut other = stranded.clone();
        other.work_id = opaque_ref("wi", "ambiguous duplicate");
        other.repository = other.repository.map(|value| value.to_ascii_uppercase());
        other.exact_head = other.exact_head.map(|value| value.to_ascii_uppercase());
        let mut ambiguous = vec![stranded.clone(), other];
        classify_stranded_identity_ambiguity(&mut ambiguous);
        assert!(
            ambiguous
                .iter()
                .all(|item| item.classification == "blocked")
        );
        assert!(ambiguous.iter().all(|item| {
            item.blocking_reasons
                .contains(&"ambiguous_target_identity".to_owned())
        }));

        ledger
            .record_continuations(
                &work_id,
                0,
                &ContinuationSet::new(digest(b"success"), None, digest(b"failure"), None)
                    .expect("continuations"),
            )
            .expect("plant downstream state");
        let blocked = ledger
            .terminal_reconciliation_inventory()
            .expect("blocked inventory");
        assert_eq!(
            blocked.stranded_publications[0].classification,
            "managed_unbound"
        );
        assert!(!blocked.stranded_publications[0].terminal_reconciliation_eligible);
        assert_eq!(
            blocked.stranded_publications[0].blocking_reasons,
            ["unexpected_continuation_contract"]
        );
    }

    #[test]
    fn terminal_reconciliation_inventory_keeps_incomplete_identity_visible() {
        let temp = TempDir::new().expect("temp");
        let (ledger, work_id) = seed_stranded_publication_precursor(temp.path());
        ledger
            .connect_read_write()
            .expect("fixture connection")
            .execute("DELETE FROM imports WHERE work_item_id = ?1", [&work_id])
            .expect("remove import fixture");
        let missing_import = ledger
            .terminal_reconciliation_inventory()
            .expect("missing import remains observable");
        assert_eq!(missing_import.stranded_publications.len(), 1);
        assert_eq!(
            missing_import.stranded_publications[0].classification,
            "blocked"
        );
        assert_eq!(missing_import.stranded_publications[0].related.imports, 0);
        for reason in ["unexpected_import_count", "source_import_mismatch"] {
            assert!(
                missing_import.stranded_publications[0]
                    .blocking_reasons
                    .contains(&reason.to_owned())
            );
        }

        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute(
                "UPDATE work_items SET repo = NULL, pr = NULL, head_sha = NULL WHERE id = ?1",
                [&work_id],
            )
            .expect("remove target identity fixture");
        drop(connection);
        let incomplete = ledger
            .terminal_reconciliation_inventory()
            .expect("incomplete identity remains observable");
        let incomplete = &incomplete.stranded_publications[0];
        assert_eq!(incomplete.classification, "blocked");
        assert!(incomplete.repository.is_none());
        assert!(incomplete.pull_request.is_none());
        assert!(incomplete.exact_head.is_none());
        for reason in [
            "missing_repository",
            "missing_pull_request",
            "missing_exact_head",
        ] {
            assert!(incomplete.blocking_reasons.contains(&reason.to_owned()));
        }

        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute(
                "UPDATE work_items SET repo = 'OWNER/REPO', pr = 0,
                        head_sha = 'not-a-commit', base_ref = '' WHERE id = ?1",
                [&work_id],
            )
            .expect("malformed target identity fixture");
        drop(connection);
        let malformed = ledger
            .terminal_reconciliation_inventory()
            .expect("malformed identity remains observable");
        let malformed = &malformed.stranded_publications[0];
        assert_eq!(malformed.classification, "blocked");
        for reason in ["invalid_target_identity", "invalid_base_ref"] {
            assert!(malformed.blocking_reasons.contains(&reason.to_owned()));
        }
    }

    #[test]
    fn terminal_reconciliation_refuses_cross_class_identity_collisions() {
        let temp = TempDir::new().expect("temp");
        let (request, ledger) = seed_unbound_terminal(temp.path());
        let duplicate_work_id = opaque_ref("wi", "cross-class duplicate");
        ledger
            .import_candidates(&[ImportCandidate {
                work_id: duplicate_work_id.clone(),
                kind: "terminal_handoff".to_owned(),
                repo: Some(request.repository.clone()),
                pr: Some(request.pull_request),
                head_sha: Some(request.head_sha.clone()),
                base_ref: Some(request.base_ref.clone()),
                goal_id: Some(opaque_ref("goal", "cross-class duplicate")),
                goal_generation: 1,
                lane: Some("fresh_agent_continuation".to_owned()),
                role: "root".to_owned(),
                owner_id: Some(opaque_ref("owner", "cross-class duplicate")),
                owner_generation: 1,
                terminal_adapter: Some("session_host".to_owned()),
                agent_adapter: Some("codex".to_owned()),
                provider_adapter: Some("codex".to_owned()),
                coordinator_route_ref: None,
                repair_route_ref: Some(opaque_ref("route", "cross-class duplicate")),
                pr_truth: "unknown".to_owned(),
                acceptance_truth: "unknown".to_owned(),
                continuation_truth: "pending".to_owned(),
                phase: LifecycleState::ShadowImported.as_str().to_owned(),
                source_ref: opaque_ref("src", "cross-class duplicate"),
                content_digest: digest(b"cross-class duplicate"),
                source_updated_at: None,
            }])
            .expect("cross-class duplicate fixture");
        ledger
            .connect_read_write()
            .expect("fixture connection")
            .execute(
                "UPDATE work_items SET repo = upper(repo), head_sha = upper(head_sha)
                  WHERE id = ?1",
                [&duplicate_work_id],
            )
            .expect("noncanonical alias fixture");

        let inventory_error = ledger
            .terminal_reconciliation_inventory()
            .expect_err("cross-class inventory must refuse");
        assert!(
            inventory_error
                .to_string()
                .contains("cross-class target collision")
        );
        let target_error = ledger
            .terminal_reconciliation_target(
                &request.repository,
                request.pull_request,
                &request.head_sha,
            )
            .expect_err("targeted repair must refuse the duplicate identity");
        assert!(target_error.to_string().contains("absent or ambiguous"));
    }

    #[test]
    fn terminal_reconciliation_refuses_noncanonical_stored_terminal_identity() {
        let temp = TempDir::new().expect("temp");
        let (request, ledger) = seed_unbound_terminal(temp.path());
        ledger
            .connect_read_write()
            .expect("fixture connection")
            .execute(
                "UPDATE work_items SET repo = upper(repo), head_sha = upper(head_sha)
                  WHERE id = ?1",
                [&request.work_id],
            )
            .expect("noncanonical terminal identity fixture");

        let inventory_error = ledger
            .terminal_reconciliation_inventory()
            .expect_err("inventory must reject a noncanonical repair target");
        assert!(inventory_error.to_string().contains("target is invalid"));
        let target_error = ledger
            .terminal_reconciliation_target(
                &request.repository,
                request.pull_request,
                &request.head_sha,
            )
            .expect_err("targeted repair must reject normalized aliasing");
        assert!(target_error.to_string().contains("no exact terminal"));
    }

    fn seed_uncertain_dispatch(
        state_dir: &std::path::Path,
        null_profile_ref: bool,
        release_activation: bool,
    ) -> (TerminalReconciliationRequest, WorkLedger) {
        let (mut request, ledger) = seed_unbound_terminal(state_dir);
        request.work_generation -= 1;
        let delivery_id = format!("pd_{}", "3".repeat(64));
        let activation_id = format!("ae_{}", "4".repeat(64));
        let request_bytes = b"terminal reconciliation provider request";
        let request_digest = digest(request_bytes);
        let request_object = ledger
            .put_protected_object(
                &request.work_id,
                ProtectedObjectKind::ProviderRequest,
                None,
                &request_digest,
                request_bytes,
            )
            .expect("provider request fixture");
        let now = Utc::now().to_rfc3339();
        let connection = ledger.connect_read_write().expect("fixture connection");
        connection
            .execute(
                "UPDATE work_items SET phase = 'dispatching', work_generation = ?2
                  WHERE id = ?1",
                params![request.work_id, request.work_generation],
            )
            .expect("restore dispatching generation");
        connection
            .execute(
                "INSERT INTO activation_epochs
                 (activation_id, work_item_id, work_generation, owner_generation,
                  epoch, owner_ref, state, acquired_at, released_at)
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, 'active', ?6, NULL)",
                params![
                    activation_id,
                    request.work_id,
                    request.work_generation,
                    request.owner_generation,
                    format!("owner_{}", "5".repeat(64)),
                    now,
                ],
            )
            .expect("released activation fixture");
        connection
            .execute(
                "INSERT INTO wake_attempts
                 (wake_id, attempt, state, adapter_id, idempotent, outcome_digest,
                  started_at, finished_at)
                 VALUES (?1, 1, 'uncertain', 'test-provider-adapter', 0, ?2, ?3, ?3)",
                params![request.wake_id, digest(b"uncertain outcome"), now],
            )
            .expect("uncertain attempt fixture");
        connection
            .execute(
                "INSERT INTO provider_deliveries
                 (delivery_id, wake_id, attempt, activation_id, provider_id, adapter_id,
                  idempotency_key, request_object_ref, state, created_at, updated_at)
                 VALUES (?1, ?2, 1, ?3, 'test-provider', 'test-provider-adapter',
                         'terminal-reconciliation-test-key', ?4, 'uncertain', ?5, ?5)",
                params![
                    delivery_id,
                    request.wake_id,
                    activation_id,
                    request_object.object_ref,
                    now,
                ],
            )
            .expect("uncertain delivery fixture");
        if release_activation {
            connection
                .execute(
                    "UPDATE activation_epochs SET state = 'released', released_at = ?2
                      WHERE activation_id = ?1",
                    params![activation_id, now],
                )
                .expect("release uncertain activation fixture");
        }
        connection
            .execute(
                "UPDATE outbox SET state = 'uncertain',
                    profile_ref = CASE WHEN ?2 THEN NULL ELSE profile_ref END,
                    updated_at = ?3 WHERE wake_id = ?1",
                params![request.wake_id, null_profile_ref, now],
            )
            .expect("uncertain wake fixture");
        drop(connection);
        request.delivery_id = Some(delivery_id);
        (request, ledger)
    }

    #[derive(Debug, Eq, PartialEq)]
    struct DeliveryMutationSnapshot {
        delivery_id: String,
        wake_id: String,
        attempt: u64,
        activation_id: String,
        provider_id: String,
        adapter_id: String,
        idempotency_key: String,
        request_object_ref: String,
        receipt_object_ref: Option<String>,
        state: String,
        created_at: String,
        updated_at: String,
        delivered_at: Option<String>,
    }

    type WorkMutationSnapshot = (String, u64, u64, String, String, String, String);
    type EventMutationSnapshot = (
        String,
        u64,
        u64,
        String,
        Option<String>,
        String,
        String,
        String,
    );
    type WakeMutationSnapshot = (
        String,
        u64,
        u64,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        String,
    );
    type ActivationMutationSnapshot = (
        String,
        u64,
        u64,
        u64,
        String,
        String,
        String,
        Option<String>,
    );

    #[derive(Debug, Eq, PartialEq)]
    struct ReconciliationMutationSnapshot {
        work: WorkMutationSnapshot,
        events: Vec<EventMutationSnapshot>,
        wake: WakeMutationSnapshot,
        delivery: DeliveryMutationSnapshot,
        activation: ActivationMutationSnapshot,
    }

    #[allow(clippy::too_many_lines)] // Keep the immutable snapshot schema in one auditable query order.
    fn reconciliation_mutation_snapshot(
        ledger: &WorkLedger,
        request: &TerminalReconciliationRequest,
    ) -> ReconciliationMutationSnapshot {
        let connection = ledger.connect_read_only().expect("snapshot connection");
        let work = connection
            .query_row(
                "SELECT phase, work_generation, owner_generation, updated_at,
                        base_ref, source_digest, repair_route_ref
                   FROM work_items WHERE id = ?1",
                [&request.work_id],
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
            .expect("work snapshot");
        let events = {
            let mut statement = connection
                .prepare(
                    "SELECT event_id, work_generation, owner_generation, kind,
                            from_state, to_state, payload_digest, created_at
                       FROM events WHERE work_item_id = ?1 ORDER BY event_id",
                )
                .expect("event snapshot statement");
            statement
                .query_map([&request.work_id], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                })
                .expect("event snapshot rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("event snapshot")
        };
        let wake = connection
            .query_row(
                "SELECT state, work_generation, owner_generation, route_ref, profile_ref,
                        payload_digest, transport_receipt_digest, provider_delivery_id, updated_at
                   FROM outbox WHERE wake_id = ?1",
                [&request.wake_id],
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
                    ))
                },
            )
            .expect("wake snapshot");
        let delivery = connection
            .query_row(
                "SELECT delivery_id, wake_id, attempt, activation_id, provider_id,
                        adapter_id, idempotency_key, request_object_ref, receipt_object_ref,
                        state, created_at, updated_at, delivered_at
                   FROM provider_deliveries WHERE wake_id = ?1",
                [&request.wake_id],
                |row| {
                    Ok(DeliveryMutationSnapshot {
                        delivery_id: row.get(0)?,
                        wake_id: row.get(1)?,
                        attempt: row.get(2)?,
                        activation_id: row.get(3)?,
                        provider_id: row.get(4)?,
                        adapter_id: row.get(5)?,
                        idempotency_key: row.get(6)?,
                        request_object_ref: row.get(7)?,
                        receipt_object_ref: row.get(8)?,
                        state: row.get(9)?,
                        created_at: row.get(10)?,
                        updated_at: row.get(11)?,
                        delivered_at: row.get(12)?,
                    })
                },
            )
            .expect("delivery snapshot");
        let activation = connection
            .query_row(
                "SELECT activation.activation_id, activation.work_generation,
                        activation.owner_generation, activation.epoch, activation.owner_ref,
                        activation.state, activation.acquired_at, activation.released_at
                   FROM activation_epochs activation
                   JOIN provider_deliveries delivery
                     ON delivery.activation_id = activation.activation_id
                  WHERE delivery.wake_id = ?1",
                [&request.wake_id],
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
                    ))
                },
            )
            .expect("activation snapshot");
        ReconciliationMutationSnapshot {
            work,
            events,
            wake,
            delivery,
            activation,
        }
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
        assert!(absent.to_string().contains("absent or ambiguous"));

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
    fn terminal_reconciliation_inventory_types_nonterminal_rows_and_target_replays() {
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
        let nonterminal = ledger
            .terminal_reconciliation_inventory()
            .expect("nonterminal inventory");
        assert!(nonterminal.items.is_empty());
        assert_eq!(nonterminal.stranded_publications.len(), 1);
        assert_eq!(
            nonterminal.stranded_publications[0].classification,
            "managed_unbound"
        );
        assert!(!nonterminal.stranded_publications[0].terminal_reconciliation_eligible);
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

    #[test]
    fn uncertain_dispatch_dry_run_is_zero_write_and_apply_is_replayable() {
        let temp = TempDir::new().expect("temp");
        let (request, ledger) = seed_uncertain_dispatch(temp.path(), false, true);
        let mut unrelated = native_publication_test_request();
        unrelated.pull_request += 1;
        unrelated.head_sha = "b".repeat(40);
        unrelated.workstream_handle = "GEN-44".to_owned();
        let unrelated_report = WorkLedger::plan_or_apply_native_continuation(
            temp.path(),
            &unrelated,
            &native_publication_test_policy(vec![unrelated.repository.clone()]),
            true,
        )
        .expect("unrelated active work");
        let unrelated_before: (String, u64, u64) = ledger
            .connect_read_only()
            .expect("unrelated read")
            .query_row(
                "SELECT phase, work_generation, owner_generation FROM work_items WHERE id = ?1",
                [&unrelated_report.work_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("unrelated snapshot");
        let before = reconciliation_mutation_snapshot(&ledger, &request);
        let planned = ledger
            .plan_uncertain_dispatch_reconciliation(&request)
            .expect("uncertain dry run");
        assert!(!planned.applied);
        assert_eq!(
            reconciliation_mutation_snapshot(&ledger, &request),
            before,
            "dry-run changed exact reconciliation authority"
        );

        let projected = ledger
            .finalize_uncertain_dispatch_with_authority(&request, || Ok(()))
            .expect("fenced terminalization");
        assert_eq!(projected.work_generation, request.work_generation + 1);
        drop(ledger);
        let ledger = WorkLedger::open_existing(temp.path())
            .expect("restart open")
            .expect("restart ledger");
        let applied = WorkLedger::plan_or_apply_terminal_reconciliation(
            temp.path(),
            &projected,
            true,
            || Ok(()),
        )
        .expect("projection apply");
        assert!(applied.applied);
        assert_eq!(applied.plan_sha256, planned.plan_sha256);
        let replay = WorkLedger::plan_or_apply_terminal_reconciliation(
            temp.path(),
            &projected,
            true,
            || panic!("replay must not reread remote authority"),
        )
        .expect("response-loss replay");
        assert!(replay.replay);
        assert!(!replay.applied);
        assert_eq!(table_count(&ledger, "provider_deliveries"), 1);
        let unrelated_after: (String, u64, u64) = ledger
            .connect_read_only()
            .expect("unrelated read")
            .query_row(
                "SELECT phase, work_generation, owner_generation FROM work_items WHERE id = ?1",
                [&unrelated_report.work_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("unrelated snapshot");
        assert_eq!(unrelated_after, unrelated_before, "active work changed");
    }

    #[test]
    fn uncertain_dispatch_accepts_legacy_null_profile_and_refuses_local_drift() {
        let temp = TempDir::new().expect("temp");
        let (request, ledger) = seed_uncertain_dispatch(temp.path(), true, true);
        ledger
            .plan_uncertain_dispatch_reconciliation(&request)
            .expect("NULL profile is valid when exact protected profile remains bound");
        let before = reconciliation_mutation_snapshot(&ledger, &request);
        let error = ledger
            .finalize_uncertain_dispatch_with_authority(&request, || {
                Err(WorkLedgerError::Refused(
                    "simulated GitHub drift".to_owned(),
                ))
            })
            .expect_err("remote authority drift refuses");
        assert!(error.to_string().contains("simulated GitHub drift"));
        assert_eq!(
            reconciliation_mutation_snapshot(&ledger, &request),
            before,
            "remote-authority refusal changed exact reconciliation authority"
        );

        let connection = ledger.connect_read_write().expect("drift connection");
        connection
            .execute(
                "UPDATE work_items SET base_ref = 'different' WHERE id = ?1",
                [&request.work_id],
            )
            .expect("local drift");
        drop(connection);
        let before_local_refusal = reconciliation_mutation_snapshot(&ledger, &request);
        let error = ledger
            .finalize_uncertain_dispatch_with_authority(&request, || Ok(()))
            .expect_err("full local fence refuses");
        assert!(error.to_string().contains("authority"));
        assert_eq!(
            reconciliation_mutation_snapshot(&ledger, &request),
            before_local_refusal,
            "local-authority refusal changed exact reconciliation authority"
        );
    }

    #[test]
    fn uncertain_dispatch_refuses_non_uncertain_or_wrong_delivery_without_mutation() {
        for drift in ["wake-state", "delivery-id"] {
            let temp = TempDir::new().expect("temp");
            let (mut request, ledger) = seed_uncertain_dispatch(temp.path(), false, true);
            if drift == "wake-state" {
                ledger
                    .connect_read_write()
                    .expect("drift connection")
                    .execute(
                        "UPDATE outbox SET state = 'failed' WHERE wake_id = ?1",
                        [&request.wake_id],
                    )
                    .expect("non-uncertain wake");
            } else {
                request.delivery_id = Some(format!("pd_{}", "9".repeat(64)));
            }
            let before = reconciliation_mutation_snapshot(&ledger, &request);
            let error = ledger
                .finalize_uncertain_dispatch_with_authority(&request, || Ok(()))
                .expect_err("contradictory dispatch authority refuses");
            assert!(error.to_string().contains("authority"), "{drift}: {error}");
            assert_eq!(
                reconciliation_mutation_snapshot(&ledger, &request),
                before,
                "{drift} refusal changed exact reconciliation authority"
            );
        }
    }

    #[test]
    fn uncertain_dispatch_refuses_active_activation_without_mutation() {
        let temp = TempDir::new().expect("temp");
        let (request, ledger) = seed_uncertain_dispatch(temp.path(), false, false);
        let before = reconciliation_mutation_snapshot(&ledger, &request);

        let plan_error = ledger
            .plan_uncertain_dispatch_reconciliation(&request)
            .expect_err("dry-run must refuse a still-active activation");
        assert!(plan_error.to_string().contains("authority"));
        assert_eq!(
            reconciliation_mutation_snapshot(&ledger, &request),
            before,
            "dry-run mutated active work"
        );

        let apply_error = ledger
            .finalize_uncertain_dispatch_with_authority(&request, || Ok(()))
            .expect_err("apply must refuse a still-active activation");
        assert!(apply_error.to_string().contains("authority"));
        assert_eq!(
            reconciliation_mutation_snapshot(&ledger, &request),
            before,
            "apply mutated active work"
        );
    }
}
