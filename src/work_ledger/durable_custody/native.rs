use super::{
    CustodyEnvelope, CustodyRelation, InboxAuthority, InboxClaim, ProcessedReceipt, WorkLedger,
    WorkLedgerError, WorkLedgerResult, digest, params, positive_u64, validate_digest,
    validate_opaque_ref,
};

impl WorkLedger {
    /// Custody activation is a cutover, not a takeover of an in-flight local
    /// provider call. Claimed or uncertain wakes retain their original host
    /// custody and require an explicit drain or reconciliation first.
    pub(crate) fn require_native_custody_cutover_ready(&self) -> WorkLedgerResult<()> {
        let connection = self.custody_read_connection()?;
        let blocked: bool = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM outbox wake
                 JOIN work_items work ON work.id = wake.work_item_id
                WHERE work.kind = 'terminal_handoff'
                  AND wake.state IN ('claimed', 'uncertain')
            )",
            [],
            |row| row.get(0),
        )?;
        if blocked {
            return Err(WorkLedgerError::Refused(
                "cross-machine custody cutover found an in-flight local wake".to_owned(),
            ));
        }
        Ok(())
    }

    /// Build immutable custody envelopes for native wakes that have not yet
    /// been assigned to the elected cross-machine authority.
    pub(crate) fn native_custody_stage_candidates(
        &self,
        source_machine_ref: &str,
        source_incarnation_ref: &str,
        limit: usize,
    ) -> WorkLedgerResult<Vec<CustodyEnvelope>> {
        validate_opaque_ref("custody source machine", source_machine_ref, "machine")?;
        validate_opaque_ref(
            "custody source incarnation",
            source_incarnation_ref,
            "incarnation",
        )?;
        let connection = self.custody_read_connection()?;
        let mut statement = connection.prepare(
            "SELECT wake.wake_id, wake.work_item_id, wake.work_generation,
                    wake.owner_generation, wake.payload_digest, work.source_digest,
                    binding.workstream_handle, binding.projection_revision
               FROM outbox wake
               JOIN work_items work ON work.id = wake.work_item_id
               JOIN workstream_projection_bindings binding
                 ON binding.work_item_id = wake.work_item_id
              WHERE wake.state = 'pending' AND work.phase = 'dispatching'
                AND NOT EXISTS (
                    SELECT 1 FROM custody_outbox custody
                     WHERE custody.wake_id = wake.wake_id
                )
              ORDER BY wake.created_at, wake.wake_id LIMIT ?1",
        )?;
        let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?;
        rows.map(|row| {
            let (
                wake_id,
                work_item_id,
                work_generation,
                owner_generation,
                content_digest,
                work_authority_digest,
                workstream_handle,
                workstream_revision,
            ) = row?;
            CustodyEnvelope::new(
                wake_id,
                work_item_id,
                positive_u64("native custody work generation", work_generation)?,
                positive_u64("native custody owner generation", owner_generation)?,
                content_digest,
                work_authority_digest,
                workstream_handle,
                positive_u64("native custody workstream revision", workstream_revision)?,
                source_machine_ref.to_owned(),
                source_incarnation_ref.to_owned(),
                CustodyRelation::wake(),
            )
        })
        .collect()
    }

    /// Received native obligations are claimed in stable order. Processing
    /// rows remain eligible after their lease expires, which makes a daemon
    /// crash between claim and effect restart-safe.
    pub(crate) fn native_custody_inbox_candidates(
        &self,
        limit: usize,
    ) -> WorkLedgerResult<Vec<String>> {
        let connection = self.custody_read_connection()?;
        let mut statement = connection.prepare(
            "SELECT message_id FROM custody_inbox
              WHERE state IN ('received', 'processing')
              ORDER BY received_at, message_id LIMIT ?1",
        )?;
        let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Commit receiver processing only when this host already has the exact
    /// protected native wake named by the authenticated envelope. Custody does
    /// not smuggle profile bytes or credentials between disjoint host WALs;
    /// missing local publication authority remains retryable and unprocessed.
    pub(crate) fn apply_native_custody_obligation(
        &self,
        claim: &InboxClaim,
        target_incarnation_ref: &str,
        authority_digest: &str,
    ) -> WorkLedgerResult<ProcessedReceipt> {
        validate_opaque_ref(
            "native custody target incarnation",
            target_incarnation_ref,
            "incarnation",
        )?;
        validate_digest("native custody authority", authority_digest)?;
        let connection = self.custody_read_connection()?;
        let identity_json: Vec<u8> = connection.query_row(
            "SELECT identity_json FROM custody_inbox WHERE message_id = ?1",
            [&claim.message_id],
            |row| row.get(0),
        )?;
        let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json).map_err(|_| {
            WorkLedgerError::Refused("stored custody envelope is invalid".to_owned())
        })?;
        envelope.validate()?;
        self.protected_launch_profile_bytes(&envelope.work_item_id, &envelope.content_digest)?;
        let authority = InboxAuthority::new(
            envelope.workstream_revision,
            target_incarnation_ref.to_owned(),
            authority_digest.to_owned(),
        )?;
        let effect_digest = digest(
            format!(
                "shipyard-native-custody-admission-v1\n{}\n{}\n{}",
                envelope.message_id, envelope.identity_digest, authority_digest
            )
            .as_bytes(),
        );
        self.apply_custody_effect(claim, &authority, &effect_digest, |tx| {
            let exact: bool = tx.query_row(
                "SELECT EXISTS(
                    SELECT 1
                      FROM outbox wake
                      JOIN work_items work ON work.id = wake.work_item_id
                      JOIN workstream_projection_bindings binding
                        ON binding.work_item_id = wake.work_item_id
                      JOIN protected_objects profile
                        ON profile.work_item_id = wake.work_item_id
                       AND profile.kind = 'launch_profile'
                       AND profile.content_digest = wake.payload_digest
                     WHERE wake.wake_id = ?1 AND wake.work_item_id = ?2
                       AND wake.work_generation = ?3 AND wake.owner_generation = ?4
                       AND wake.payload_digest = ?5
                       AND work.source_digest = ?6
                       AND binding.workstream_handle = ?7
                       AND binding.projection_revision = ?8
                       AND (
                           (wake.state IN ('pending', 'claimed', 'uncertain')
                            AND work.phase IN ('dispatching', 'agent_owned_repair'))
                           OR
                           (wake.state IN ('delivered', 'acknowledged')
                            AND work.phase IN ('agent_owned_repair', 'returned',
                                               'managed', 'terminal'))
                       )
                )",
                params![
                    envelope.wake_id,
                    envelope.work_item_id,
                    envelope.work_generation,
                    envelope.owner_generation,
                    envelope.content_digest,
                    envelope.work_authority_digest,
                    envelope.workstream_handle,
                    envelope.workstream_revision,
                ],
                |row| row.get(0),
            )?;
            if !exact {
                return Err(WorkLedgerError::Refused(
                    "custody destination native obligation is unavailable or contradictory"
                        .to_owned(),
                ));
            }
            Ok(())
        })
    }
}
