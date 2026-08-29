//! Inert transactional route-change records. Adapter verification activates these APIs later.
#![allow(dead_code)]

use super::lifecycle::record_event;
use super::registry::{RouteRegistration, insert_route_record, load_validated_route};
use super::route::AgentRoute;
use super::{
    LifecycleState, TransactionBehavior, WorkLedger, WorkLedgerError, WorkLedgerResult,
    configure_durable, digest, opaque_ref, params, validate_digest, validate_opaque_ref,
    verify_integrity, verify_supported_schema,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SameSessionRebindChallenge {
    pub change_id: String,
    pub source: RouteRegistration,
    pub target_work_generation: u64,
    pub target_route_ref: String,
    integrity: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct VerifiedTerminalRebindReceipt {
    change_id: String,
    target_route_ref: String,
    evidence_digest: String,
    integrity: String,
}

#[cfg(test)]
impl VerifiedTerminalRebindReceipt {
    pub(super) fn verified(
        change_id: String,
        target_route_ref: String,
        evidence_digest: String,
    ) -> Self {
        let integrity = receipt_digest(&change_id, &target_route_ref, &evidence_digest);
        Self {
            change_id,
            target_route_ref,
            evidence_digest,
            integrity,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AppliedRouteChangeReceipt {
    pub change_id: String,
    pub target_route_ref: String,
    pub receipt_digest: String,
}

fn challenge_integrity(
    change_id: &str,
    source: &RouteRegistration,
    target_work: u64,
    target: &str,
) -> String {
    digest(format!("shipyard-route-change-challenge-v1\0{change_id}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{target_work}\n{target}", source.work_id, source.head_sha, source.work_generation, source.owner_ref, source.owner_generation, source.route_ref, source.revision, source.origin_machine_ref, source.envelope_integrity).as_bytes())
}

fn receipt_digest(change_id: &str, target: &str, evidence: &str) -> String {
    digest(
        format!("shipyard-route-change-receipt-v1\0{change_id}\n{target}\n{evidence}").as_bytes(),
    )
}

impl WorkLedger {
    pub(super) fn prepare_same_session_rebind(
        &self,
        source: &RouteRegistration,
        target_route_ref: String,
    ) -> WorkLedgerResult<SameSessionRebindChallenge> {
        validate_opaque_ref("target_route_ref", &target_route_ref, "route")?;
        let target_work = source
            .work_generation
            .checked_add(1)
            .ok_or_else(|| WorkLedgerError::Refused("work generation overflow".to_owned()))?;
        let change_id = opaque_ref(
            "route-change",
            &format!(
                "{}\n{}\n{}\n{}",
                self.ledger_incarnation_ref,
                source.work_id,
                source.work_generation,
                source.owner_generation
            ),
        );
        let integrity = challenge_integrity(&change_id, source, target_work, &target_route_ref);
        let _lease = crate::writer_domain_lease::acquire_for_protected_path(
            self.path
                .parent()
                .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?,
        )?;
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let clock = self.clock.observe(&tx)?;
        if clock.durable_wall_regressed {
            return Err(WorkLedgerError::Refused(
                "wall clock regressed below durable floor; new route changes are refused"
                    .to_owned(),
            ));
        }
        let current = load_validated_route(&tx, &source.route_ref)?
            .ok_or_else(|| WorkLedgerError::Refused("source route is absent".to_owned()))?;
        if current != *source {
            return Err(WorkLedgerError::Refused("source route changed".to_owned()));
        }
        let dead_recovery: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM route_changes WHERE recovery_route_ref=?1
                    AND state='failed' AND ledger_incarnation_ref=?2)",
            params![source.route_ref, self.ledger_incarnation_ref],
            |row| row.get(0),
        )?;
        if dead_recovery {
            return Err(WorkLedgerError::Refused(
                "a dead-session recovery fence cannot use same-session rebind".to_owned(),
            ));
        }
        let matches: bool = tx.query_row("SELECT head_sha=?2 AND work_generation=?3 AND owner_id=?4 AND owner_generation=?5 AND repair_route_ref=?6 FROM work_items WHERE id=?1", params![source.work_id, source.head_sha, source.work_generation, source.owner_ref, source.owner_generation, source.route_ref], |r| r.get(0))?;
        if !matches {
            return Err(WorkLedgerError::Refused(
                "source work fence changed".to_owned(),
            ));
        }
        let now = clock.timestamp.to_rfc3339();
        let changed = tx.execute("INSERT OR IGNORE INTO route_changes (change_id,ledger_incarnation_ref,work_item_id,head_sha,kind,state,source_work_generation,source_owner_ref,source_owner_generation,source_route_ref,intermediate_work_generation,target_work_generation,target_owner_ref,target_owner_generation,target_route_ref,claim_integrity,change_integrity,created_at,updated_at) VALUES (?1,?2,?3,?4,'same_session_rebind','prepared',?5,?6,?7,?8,?5,?9,?6,?7,?10,?11,?11,?12,?12)", params![change_id,self.ledger_incarnation_ref,source.work_id,source.head_sha,source.work_generation,source.owner_ref,source.owner_generation,source.route_ref,target_work,target_route_ref,integrity,now])?;
        if changed == 1 {
            record_event(
                &tx,
                &self.ledger_incarnation_ref,
                None,
                &source.work_id,
                source.work_generation,
                source.owner_generation,
                "same_session_rebind_prepared",
                Some(LifecycleState::Actionable),
                LifecycleState::Actionable,
                &integrity,
                &now,
            )?;
        } else {
            let stored: Option<(String,String,String,String)> = tx.query_row("SELECT change_id,target_route_ref,claim_integrity,state FROM route_changes WHERE work_item_id=?1 AND source_work_generation=?2 AND source_owner_generation=?3 AND ledger_incarnation_ref=?4",params![source.work_id,source.work_generation,source.owner_generation,self.ledger_incarnation_ref],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
            if stored
                != Some((
                    change_id.clone(),
                    target_route_ref.clone(),
                    integrity.clone(),
                    "prepared".to_owned(),
                ))
            {
                return Err(WorkLedgerError::Refused(
                    "a different route change already exists for source generation".to_owned(),
                ));
            }
        }
        super::clock::LedgerClock::finalize(&tx)?;
        tx.commit()?;
        Ok(SameSessionRebindChallenge {
            change_id,
            source: source.clone(),
            target_work_generation: target_work,
            target_route_ref,
            integrity,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn apply_same_session_rebind(
        &self,
        challenge: &SameSessionRebindChallenge,
        verified: &VerifiedTerminalRebindReceipt,
        target: &RouteRegistration,
    ) -> WorkLedgerResult<AppliedRouteChangeReceipt> {
        if challenge_integrity(
            &challenge.change_id,
            &challenge.source,
            challenge.target_work_generation,
            &challenge.target_route_ref,
        ) != challenge.integrity
        {
            return Err(WorkLedgerError::Refused(
                "challenge integrity mismatch".to_owned(),
            ));
        }
        validate_digest("adapter evidence", &verified.evidence_digest)?;
        let expected_receipt = receipt_digest(
            &verified.change_id,
            &verified.target_route_ref,
            &verified.evidence_digest,
        );
        if verified.change_id != challenge.change_id
            || verified.target_route_ref != challenge.target_route_ref
            || verified.integrity != expected_receipt
        {
            return Err(WorkLedgerError::Refused(
                "terminal rebind receipt mismatch".to_owned(),
            ));
        }
        validate_same_session_target(
            &challenge.source,
            target,
            challenge.target_work_generation,
            &challenge.target_route_ref,
        )?;
        let _lease = crate::writer_domain_lease::acquire_for_protected_path(
            self.path
                .parent()
                .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?,
        )?;
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.clock.observe(&tx)?.timestamp.to_rfc3339();
        if load_validated_route(&tx, &challenge.source.route_ref)?.as_ref()
            != Some(&challenge.source)
        {
            return Err(WorkLedgerError::Refused(
                "same-session challenge source no longer matches persisted route".to_owned(),
            ));
        }
        let stored: Option<(String,Option<String>,Option<String>)> = tx.query_row("SELECT state,receipt_digest,target_route_ref FROM route_changes WHERE change_id=?1 AND ledger_incarnation_ref=?2", params![challenge.change_id,self.ledger_incarnation_ref], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((state, Some(saved), Some(route))) = stored.clone()
            && state == "applied"
            && saved == expected_receipt
            && route == target.route_ref
        {
            let persisted = load_validated_route(&tx, &route)?.ok_or_else(|| {
                WorkLedgerError::Refused("applied target route is absent".to_owned())
            })?;
            let current:bool=tx.query_row("SELECT work_generation=?2 AND owner_id=?3 AND owner_generation=?4 AND repair_route_ref=?5 FROM work_items WHERE id=?1",params![target.work_id,target.work_generation,target.owner_ref,target.owner_generation,target.route_ref],|r|r.get(0))?;
            if persisted != *target || !current {
                return Err(WorkLedgerError::Refused(
                    "applied replay target fence is corrupt".to_owned(),
                ));
            }
            return Ok(AppliedRouteChangeReceipt {
                change_id: challenge.change_id.clone(),
                target_route_ref: route,
                receipt_digest: saved,
            });
        }
        if !matches!(stored,Some((ref s,None,_)) if s=="prepared") {
            return Err(WorkLedgerError::Refused(
                "route change is not prepared".to_owned(),
            ));
        }
        insert_route_record(&tx, target, &now)?;
        let changed=tx.execute("UPDATE work_items SET work_generation=?1,repair_route_ref=?2,updated_at=?3 WHERE id=?4 AND head_sha=?5 AND work_generation=?6 AND owner_id=?7 AND owner_generation=?8 AND repair_route_ref=?9",params![target.work_generation,target.route_ref,now,target.work_id,target.head_sha,challenge.source.work_generation,target.owner_ref,target.owner_generation,challenge.source.route_ref])?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "source work fence changed".to_owned(),
            ));
        }
        tx.execute("UPDATE route_changes SET state='applied',receipt_kind='accepted',receipt_evidence_digest=?1,receipt_digest=?2,updated_at=?3,completed_at=?3 WHERE change_id=?4 AND state='prepared' AND ledger_incarnation_ref=?5",params![verified.evidence_digest,expected_receipt,now,challenge.change_id,self.ledger_incarnation_ref])?;
        record_event(
            &tx,
            &self.ledger_incarnation_ref,
            None,
            &target.work_id,
            target.work_generation,
            target.owner_generation,
            "same_session_rebound",
            Some(LifecycleState::Actionable),
            LifecycleState::Actionable,
            &expected_receipt,
            &now,
        )?;
        super::clock::LedgerClock::finalize(&tx)?;
        tx.commit()?;
        Ok(AppliedRouteChangeReceipt {
            change_id: challenge.change_id.clone(),
            target_route_ref: target.route_ref.clone(),
            receipt_digest: expected_receipt,
        })
    }
}

fn validate_same_session_target(
    source: &RouteRegistration,
    target: &RouteRegistration,
    target_work: u64,
    target_ref: &str,
) -> WorkLedgerResult<()> {
    if target.route_ref != target_ref
        || target.work_id != source.work_id
        || target.head_sha != source.head_sha
        || target.work_generation != target_work
        || target.owner_ref != source.owner_ref
        || target.owner_generation != source.owner_generation
        || target.revision
            != source
                .revision
                .checked_add(1)
                .ok_or_else(|| WorkLedgerError::Refused("route revision overflow".to_owned()))?
        || target.origin_machine_ref != source.origin_machine_ref
    {
        return Err(WorkLedgerError::Refused(
            "same-session target envelope changed protected facts".to_owned(),
        ));
    }
    source
        .provenance
        .validate()
        .map_err(|_| WorkLedgerError::Refused("source provenance is invalid".to_owned()))?;
    target
        .provenance
        .validate()
        .map_err(|_| WorkLedgerError::Refused("target provenance is invalid".to_owned()))?;
    let mut expected = source.provenance.clone();
    expected.terminal = target.provenance.terminal.clone();
    expected.integrity_sha256 = target.provenance.integrity_sha256.clone();
    if expected != target.provenance {
        return Err(WorkLedgerError::Refused(
            "same-session rebind changed protected provenance".to_owned(),
        ));
    }
    Ok(())
}

use rusqlite::OptionalExtension;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DeadNativeSessionReceipt {
    native_session_ref: String,
    evidence_digest: String,
    integrity: String,
}

#[cfg(test)]
impl DeadNativeSessionReceipt {
    pub(super) fn verified(native_session_ref: String, evidence_digest: String) -> Self {
        let integrity = digest(
            format!("shipyard-dead-native-session-v1\0{native_session_ref}\n{evidence_digest}")
                .as_bytes(),
        );
        Self {
            native_session_ref,
            evidence_digest,
            integrity,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FreshOwnerTransferClaim {
    pub change_id: String,
    pub source: RouteRegistration,
    pub intermediate_work_generation: u64,
    pub target_work_generation: u64,
    pub target_owner_ref: String,
    pub target_owner_generation: u64,
    pub target_route_ref: String,
    pub recovery_route_ref: String,
    pub checkpoint_digest: String,
    dead_session_evidence_digest: String,
    integrity: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StartedFreshOwnerTransfer {
    pub claim: FreshOwnerTransferClaim,
    pub adapter_evidence_digest: String,
    pub start_integrity: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FreshTransferReceiptKind {
    Accepted,
    DefinitiveNotDelivered,
    Uncertain,
}

impl FreshTransferReceiptKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::DefinitiveNotDelivered => "definitive_not_delivered",
            Self::Uncertain => "uncertain",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FreshOwnerTransferReceipt {
    change_id: String,
    kind: FreshTransferReceiptKind,
    evidence_digest: String,
    target_route_ref: String,
    integrity: String,
}

#[cfg(test)]
impl FreshOwnerTransferReceipt {
    pub(super) fn verified(
        change_id: String,
        kind: FreshTransferReceiptKind,
        evidence_digest: String,
        target_route_ref: String,
    ) -> Self {
        let integrity =
            fresh_receipt_integrity(&change_id, kind, &evidence_digest, &target_route_ref);
        Self {
            change_id,
            kind,
            evidence_digest,
            target_route_ref,
            integrity,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum FreshTransferDisposition {
    Applied(AppliedRouteChangeReceipt),
    NotDelivered(String),
    Uncertain(String),
}

pub(super) fn native_session(route: &RouteRegistration) -> &str {
    use super::route::AgentRoute;
    match &route.provenance.agent.route {
        AgentRoute::Codex { session }
        | AgentRoute::Claude { session }
        | AgentRoute::Named { session, .. } => session.native_session_ref.as_str(),
    }
}
#[allow(clippy::too_many_arguments)]
fn fresh_claim_integrity(
    id: &str,
    source: &RouteRegistration,
    iw: u64,
    tw: u64,
    owner: &str,
    og: u64,
    route: &str,
    recovery_route: &str,
    dead: &str,
    checkpoint: &str,
) -> String {
    digest(format!("shipyard-fresh-owner-claim-v1\0{id}\n{}\n{}\n{}\n{}\n{iw}\n{tw}\n{owner}\n{og}\n{route}\n{recovery_route}\n{dead}\n{checkpoint}",source.work_id,source.work_generation,source.owner_generation,source.envelope_integrity).as_bytes())
}
fn fresh_receipt_integrity(
    id: &str,
    kind: FreshTransferReceiptKind,
    evidence: &str,
    target: &str,
) -> String {
    digest(
        format!(
            "shipyard-fresh-owner-receipt-v1\0{id}\n{}\n{evidence}\n{target}",
            kind.as_str()
        )
        .as_bytes(),
    )
}

impl WorkLedger {
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(super) fn prepare_fresh_owner_transfer(
        &self,
        source: &RouteRegistration,
        dead: &DeadNativeSessionReceipt,
        checkpoint_digest: &str,
        target_owner_ref: &str,
        target_route_ref: &str,
    ) -> WorkLedgerResult<FreshOwnerTransferClaim> {
        validate_digest("dead-session evidence", &dead.evidence_digest)?;
        validate_digest("checkpoint", checkpoint_digest)?;
        validate_opaque_ref("target owner", target_owner_ref, "owner")?;
        validate_opaque_ref("target route", target_route_ref, "route")?;
        let expected_dead = digest(
            format!(
                "shipyard-dead-native-session-v1\0{}\n{}",
                dead.native_session_ref, dead.evidence_digest
            )
            .as_bytes(),
        );
        if dead.native_session_ref != native_session(source) || dead.integrity != expected_dead {
            return Err(WorkLedgerError::Refused(
                "native-session-dead receipt mismatch".to_owned(),
            ));
        }
        let iw = source
            .work_generation
            .checked_add(1)
            .ok_or_else(|| WorkLedgerError::Refused("work generation overflow".to_owned()))?;
        let tw = iw
            .checked_add(1)
            .ok_or_else(|| WorkLedgerError::Refused("work generation overflow".to_owned()))?;
        let og = source
            .owner_generation
            .checked_add(1)
            .ok_or_else(|| WorkLedgerError::Refused("owner generation overflow".to_owned()))?;
        if target_owner_ref == source.owner_ref {
            return Err(WorkLedgerError::Refused(
                "fresh transfer requires a new owner".to_owned(),
            ));
        }
        let id = opaque_ref(
            "route-change",
            &format!(
                "{}\n{}\n{}\n{}",
                self.ledger_incarnation_ref,
                source.work_id,
                source.work_generation,
                source.owner_generation
            ),
        );
        let recovery_route_ref = opaque_ref("route", &format!("{id}\nsource-recovery"));
        let integrity = fresh_claim_integrity(
            &id,
            source,
            iw,
            tw,
            target_owner_ref,
            og,
            target_route_ref,
            &recovery_route_ref,
            &dead.evidence_digest,
            checkpoint_digest,
        );
        let claim = FreshOwnerTransferClaim {
            change_id: id.clone(),
            source: source.clone(),
            intermediate_work_generation: iw,
            target_work_generation: tw,
            target_owner_ref: target_owner_ref.to_owned(),
            target_owner_generation: og,
            target_route_ref: target_route_ref.to_owned(),
            recovery_route_ref: recovery_route_ref.clone(),
            checkpoint_digest: checkpoint_digest.to_owned(),
            dead_session_evidence_digest: dead.evidence_digest.clone(),
            integrity: integrity.clone(),
        };
        let _lease = crate::writer_domain_lease::acquire_for_protected_path(
            self.path
                .parent()
                .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?,
        )?;
        let mut c = self.connect_read_write()?;
        configure_durable(&c)?;
        verify_supported_schema(&c)?;
        verify_integrity(&c)?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let clock = self.clock.observe(&tx)?;
        if load_validated_route(&tx, &claim.source.route_ref)?.as_ref() != Some(&claim.source) {
            return Err(WorkLedgerError::Refused(
                "fresh transfer claim source no longer matches persisted route".to_owned(),
            ));
        }
        if clock.durable_wall_regressed {
            return Err(WorkLedgerError::Refused(
                "wall clock regression refuses fresh transfer preparation".to_owned(),
            ));
        }
        let now = clock.timestamp.to_rfc3339();
        if load_validated_route(&tx, &source.route_ref)?.as_ref() != Some(source) {
            return Err(WorkLedgerError::Refused("source route changed".to_owned()));
        }
        let replay: Option<(String, String)> = tx
            .query_row(
                "SELECT state, claim_integrity FROM route_changes
                 WHERE work_item_id=?1 AND source_work_generation=?2 AND source_owner_generation=?3 AND ledger_incarnation_ref=?4",
                params![source.work_id, source.work_generation, source.owner_generation,self.ledger_incarnation_ref],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if replay == Some(("prepared".to_owned(), integrity.clone())) {
            let current: bool = tx.query_row(
                "SELECT phase='dispatching' AND work_generation=?2 AND owner_id=?3
                        AND owner_generation=?4 AND repair_route_ref=?5
                 FROM work_items WHERE id=?1 AND head_sha=?6",
                params![
                    source.work_id,
                    iw,
                    source.owner_ref,
                    source.owner_generation,
                    source.route_ref,
                    source.head_sha
                ],
                |row| row.get(0),
            )?;
            if current {
                return Ok(claim);
            }
            return Err(WorkLedgerError::Refused(
                "prepared fresh transfer lost its work fence".to_owned(),
            ));
        } else if replay.is_some() {
            return Err(WorkLedgerError::Refused(
                "a different route change already exists for source generation".to_owned(),
            ));
        }
        let changed=tx.execute("UPDATE work_items SET phase='dispatching',work_generation=?1,updated_at=?2 WHERE id=?3 AND head_sha=?4 AND phase='actionable' AND work_generation=?5 AND owner_id=?6 AND owner_generation=?7 AND repair_route_ref=?8",params![iw,now,source.work_id,source.head_sha,source.work_generation,source.owner_ref,source.owner_generation,source.route_ref])?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "fresh transfer source fence changed".to_owned(),
            ));
        }
        tx.execute("INSERT INTO route_changes (change_id,ledger_incarnation_ref,work_item_id,head_sha,kind,state,source_work_generation,source_owner_ref,source_owner_generation,source_route_ref,intermediate_work_generation,target_work_generation,target_owner_ref,target_owner_generation,target_route_ref,recovery_route_ref,dead_session_evidence_digest,checkpoint_digest,claim_integrity,change_integrity,created_at,updated_at) VALUES (?1,?2,?3,?4,'fresh_owner_transfer','prepared',?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?17,?18,?18)",params![id,self.ledger_incarnation_ref,source.work_id,source.head_sha,source.work_generation,source.owner_ref,source.owner_generation,source.route_ref,iw,tw,target_owner_ref,og,target_route_ref,recovery_route_ref,dead.evidence_digest,checkpoint_digest,integrity,now])?;
        record_event(
            &tx,
            &self.ledger_incarnation_ref,
            None,
            &source.work_id,
            iw,
            source.owner_generation,
            "fresh_owner_transfer_prepared",
            Some(LifecycleState::Actionable),
            LifecycleState::Dispatching,
            &integrity,
            &now,
        )?;
        super::clock::LedgerClock::finalize(&tx)?;
        tx.commit()?;
        Ok(claim)
    }

    pub(super) fn mark_fresh_owner_transfer_started(
        &self,
        claim: &FreshOwnerTransferClaim,
        adapter_evidence_digest: String,
    ) -> WorkLedgerResult<StartedFreshOwnerTransfer> {
        validate_digest("adapter evidence", &adapter_evidence_digest)?;
        if fresh_claim_integrity(
            &claim.change_id,
            &claim.source,
            claim.intermediate_work_generation,
            claim.target_work_generation,
            &claim.target_owner_ref,
            claim.target_owner_generation,
            &claim.target_route_ref,
            &claim.recovery_route_ref,
            &claim.dead_session_evidence_digest,
            &claim.checkpoint_digest,
        ) != claim.integrity
        {
            return Err(WorkLedgerError::Refused(
                "fresh transfer claim integrity mismatch".to_owned(),
            ));
        }
        let start = digest(
            format!(
                "shipyard-fresh-owner-start-v1\0{}\n{}\n{}",
                claim.change_id, claim.integrity, adapter_evidence_digest
            )
            .as_bytes(),
        );
        let _lease = crate::writer_domain_lease::acquire_for_protected_path(
            self.path
                .parent()
                .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?,
        )?;
        let mut c = self.connect_read_write()?;
        configure_durable(&c)?;
        verify_supported_schema(&c)?;
        verify_integrity(&c)?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let clock = self.clock.observe(&tx)?;
        if load_validated_route(&tx, &claim.source.route_ref)?.as_ref() != Some(&claim.source) {
            return Err(WorkLedgerError::Refused(
                "fresh transfer start source no longer matches persisted route".to_owned(),
            ));
        }
        let replay: Option<(String,Option<String>,Option<String>)> = tx.query_row(
            "SELECT state,adapter_evidence_digest,start_integrity FROM route_changes WHERE change_id=?1 AND ledger_incarnation_ref=?2",
            params![claim.change_id,self.ledger_incarnation_ref], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        if replay
            == Some((
                "delivery_started".to_owned(),
                Some(adapter_evidence_digest.clone()),
                Some(start.clone()),
            ))
        {
            return Ok(StartedFreshOwnerTransfer {
                claim: claim.clone(),
                adapter_evidence_digest,
                start_integrity: start,
            });
        }
        if clock.durable_wall_regressed {
            return Err(WorkLedgerError::Refused(
                "wall clock regression refuses fresh transfer start".to_owned(),
            ));
        }
        let now = clock.timestamp.to_rfc3339();
        let work_fence=tx.execute("UPDATE work_items SET updated_at=updated_at WHERE id=?1 AND head_sha=?2 AND phase='dispatching' AND work_generation=?3 AND owner_id=?4 AND owner_generation=?5 AND repair_route_ref=?6",params![claim.source.work_id,claim.source.head_sha,claim.intermediate_work_generation,claim.source.owner_ref,claim.source.owner_generation,claim.source.route_ref])?;
        if work_fence != 1 {
            return Err(WorkLedgerError::Refused(
                "fresh transfer work fence changed before delivery start".to_owned(),
            ));
        }
        let changed=tx.execute("UPDATE route_changes SET state='delivery_started',delivery_started_at=?1,adapter_evidence_digest=?2,start_integrity=?3,updated_at=?1 WHERE change_id=?4 AND state='prepared' AND claim_integrity=?5 AND ledger_incarnation_ref=?6",params![now,adapter_evidence_digest,start,claim.change_id,claim.integrity,self.ledger_incarnation_ref])?;
        if changed == 1 {
            record_event(
                &tx,
                &self.ledger_incarnation_ref,
                None,
                &claim.source.work_id,
                claim.intermediate_work_generation,
                claim.source.owner_generation,
                "fresh_owner_transfer_started",
                Some(LifecycleState::Dispatching),
                LifecycleState::Dispatching,
                &start,
                &now,
            )?;
        } else {
            return Err(WorkLedgerError::Refused(
                "fresh transfer is not prepared".to_owned(),
            ));
        }
        super::clock::LedgerClock::finalize(&tx)?;
        tx.commit()?;
        Ok(StartedFreshOwnerTransfer {
            claim: claim.clone(),
            adapter_evidence_digest,
            start_integrity: start,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn reconcile_fresh_owner_transfer(
        &self,
        started: &StartedFreshOwnerTransfer,
        receipt: &FreshOwnerTransferReceipt,
        target: Option<&RouteRegistration>,
    ) -> WorkLedgerResult<FreshTransferDisposition> {
        let expected_start = digest(
            format!(
                "shipyard-fresh-owner-start-v1\0{}\n{}\n{}",
                started.claim.change_id, started.claim.integrity, started.adapter_evidence_digest
            )
            .as_bytes(),
        );
        if expected_start != started.start_integrity {
            return Err(WorkLedgerError::Refused(
                "fresh transfer start integrity mismatch".to_owned(),
            ));
        }
        validate_digest("receipt evidence", &receipt.evidence_digest)?;
        let rd = fresh_receipt_integrity(
            &receipt.change_id,
            receipt.kind,
            &receipt.evidence_digest,
            &receipt.target_route_ref,
        );
        if receipt.change_id != started.claim.change_id
            || receipt.integrity != rd
            || receipt.target_route_ref != started.claim.target_route_ref
        {
            return Err(WorkLedgerError::Refused(
                "fresh transfer receipt mismatch".to_owned(),
            ));
        }
        if receipt.kind == FreshTransferReceiptKind::Accepted {
            validate_fresh_target(
                &started.claim,
                target.ok_or_else(|| {
                    WorkLedgerError::Refused("accepted receipt requires target route".to_owned())
                })?,
            )?;
        } else if target.is_some() {
            return Err(WorkLedgerError::Refused(
                "non-accepted receipt cannot carry target route".to_owned(),
            ));
        }
        let _lease = crate::writer_domain_lease::acquire_for_protected_path(
            self.path
                .parent()
                .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?,
        )?;
        let mut c = self.connect_read_write()?;
        configure_durable(&c)?;
        verify_supported_schema(&c)?;
        verify_integrity(&c)?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.clock.observe(&tx)?.timestamp.to_rfc3339();
        if load_validated_route(&tx, &started.claim.source.route_ref)?.as_ref()
            != Some(&started.claim.source)
        {
            return Err(WorkLedgerError::Refused(
                "fresh transfer source no longer matches persisted route".to_owned(),
            ));
        }
        let stored: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT state,receipt_digest FROM route_changes WHERE change_id=?1 AND ledger_incarnation_ref=?2",
                params![receipt.change_id,self.ledger_incarnation_ref],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((state, Some(saved))) = stored.clone()
            && saved == rd
        {
            if state == "applied" {
                let target = target.ok_or_else(|| {
                    WorkLedgerError::Refused("applied replay requires target route".to_owned())
                })?;
                let persisted = load_validated_route(&tx, &target.route_ref)?.ok_or_else(|| {
                    WorkLedgerError::Refused("applied target route is absent".to_owned())
                })?;
                let current:bool=tx.query_row("SELECT phase='agent_owned_repair' AND work_generation=?2 AND owner_id=?3 AND owner_generation=?4 AND repair_route_ref=?5 FROM work_items WHERE id=?1",params![target.work_id,target.work_generation,target.owner_ref,target.owner_generation,target.route_ref],|r|r.get(0))?;
                if persisted != *target || !current {
                    return Err(WorkLedgerError::Refused(
                        "applied fresh-transfer replay fence is corrupt".to_owned(),
                    ));
                }
            } else if state == "failed" {
                let persisted = load_validated_route(&tx, &started.claim.recovery_route_ref)?
                    .ok_or_else(|| {
                        WorkLedgerError::Refused(
                            "failed-transfer recovery route is absent".to_owned(),
                        )
                    })?;
                let current:bool=tx.query_row("SELECT phase='actionable' AND work_generation=?2 AND owner_id=?3 AND owner_generation=?4 AND repair_route_ref=?5 FROM work_items WHERE id=?1 AND head_sha=?6",params![started.claim.source.work_id,started.claim.target_work_generation,started.claim.source.owner_ref,started.claim.source.owner_generation,started.claim.recovery_route_ref,started.claim.source.head_sha],|r|r.get(0))?;
                if persisted.work_generation != started.claim.target_work_generation
                    || persisted.owner_ref != started.claim.source.owner_ref
                    || !current
                {
                    return Err(WorkLedgerError::Refused(
                        "failed fresh-transfer replay fence is no longer current".to_owned(),
                    ));
                }
            } else if state == "uncertain" {
                let current:bool=tx.query_row("SELECT phase='dispatching' AND work_generation=?2 AND owner_id=?3 AND owner_generation=?4 AND repair_route_ref=?5 FROM work_items WHERE id=?1 AND head_sha=?6",params![started.claim.source.work_id,started.claim.intermediate_work_generation,started.claim.source.owner_ref,started.claim.source.owner_generation,started.claim.source.route_ref,started.claim.source.head_sha],|r|r.get(0))?;
                if !current {
                    return Err(WorkLedgerError::Refused(
                        "uncertain fresh-transfer replay fence is no longer current".to_owned(),
                    ));
                }
            }
            return Ok(match state.as_str() {
                "applied" => FreshTransferDisposition::Applied(AppliedRouteChangeReceipt {
                    change_id: receipt.change_id.clone(),
                    target_route_ref: receipt.target_route_ref.clone(),
                    receipt_digest: saved,
                }),
                "failed" => FreshTransferDisposition::NotDelivered(saved),
                "uncertain" => FreshTransferDisposition::Uncertain(saved),
                _ => {
                    return Err(WorkLedgerError::Refused(
                        "stored receipt has invalid state".to_owned(),
                    ));
                }
            });
        }
        if !matches!(stored,Some((ref s,None)) if s=="delivery_started") {
            return Err(WorkLedgerError::Refused(
                "fresh transfer is not delivery-started".to_owned(),
            ));
        }
        let start_matches:bool=tx.query_row("SELECT start_integrity=?2 AND adapter_evidence_digest=?3 AND ledger_incarnation_ref=?4 FROM route_changes WHERE change_id=?1",params![started.claim.change_id,started.start_integrity,started.adapter_evidence_digest,self.ledger_incarnation_ref],|r|r.get(0))?;
        if !start_matches {
            return Err(WorkLedgerError::Refused(
                "stored fresh transfer start changed".to_owned(),
            ));
        }
        let disposition = match receipt.kind {
            FreshTransferReceiptKind::Accepted => {
                let target = target.ok_or_else(|| {
                    WorkLedgerError::Refused("accepted receipt requires target route".to_owned())
                })?;
                insert_route_record(&tx, target, &now)?;
                let n=tx.execute("UPDATE work_items SET phase='agent_owned_repair',work_generation=?1,owner_id=?2,owner_generation=?3,repair_route_ref=?4,updated_at=?5 WHERE id=?6 AND head_sha=?7 AND phase='dispatching' AND work_generation=?8 AND owner_id=?9 AND owner_generation=?10 AND repair_route_ref=?11",params![target.work_generation,target.owner_ref,target.owner_generation,target.route_ref,now,target.work_id,target.head_sha,started.claim.intermediate_work_generation,started.claim.source.owner_ref,started.claim.source.owner_generation,started.claim.source.route_ref])?;
                if n != 1 {
                    return Err(WorkLedgerError::Refused(
                        "late or stale fresh-owner acknowledgment".to_owned(),
                    ));
                }
                FreshTransferDisposition::Applied(AppliedRouteChangeReceipt {
                    change_id: receipt.change_id.clone(),
                    target_route_ref: target.route_ref.clone(),
                    receipt_digest: rd.clone(),
                })
            }
            FreshTransferReceiptKind::DefinitiveNotDelivered => {
                // This immutable route is a generation-current source fence for another
                // fresh-owner transfer, not permission to resume the proven-dead session.
                // `prepare_same_session_rebind` explicitly rejects recovery-route sources.
                let recovery = RouteRegistration::new(
                    started.claim.recovery_route_ref.clone(),
                    started.claim.source.work_id.clone(),
                    started.claim.source.head_sha.clone(),
                    started.claim.target_work_generation,
                    started.claim.source.owner_ref.clone(),
                    started.claim.source.owner_generation,
                    started
                        .claim
                        .source
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| {
                            WorkLedgerError::Refused("route revision overflow".to_owned())
                        })?,
                    started.claim.source.origin_machine_ref.clone(),
                    started.claim.source.provenance.clone(),
                )?;
                insert_route_record(&tx, &recovery, &now)?;
                let n=tx.execute("UPDATE work_items SET phase='actionable',work_generation=?1,repair_route_ref=?2,updated_at=?3 WHERE id=?4 AND phase='dispatching' AND work_generation=?5 AND owner_id=?6 AND owner_generation=?7 AND repair_route_ref=?8",params![started.claim.target_work_generation,recovery.route_ref,now,started.claim.source.work_id,started.claim.intermediate_work_generation,started.claim.source.owner_ref,started.claim.source.owner_generation,started.claim.source.route_ref])?;
                if n != 1 {
                    return Err(WorkLedgerError::Refused(
                        "fresh transfer source fence changed".to_owned(),
                    ));
                }
                FreshTransferDisposition::NotDelivered(rd.clone())
            }
            FreshTransferReceiptKind::Uncertain => FreshTransferDisposition::Uncertain(rd.clone()),
        };
        let state = match receipt.kind {
            FreshTransferReceiptKind::Accepted => "applied",
            FreshTransferReceiptKind::DefinitiveNotDelivered => "failed",
            FreshTransferReceiptKind::Uncertain => "uncertain",
        };
        tx.execute("UPDATE route_changes SET state=?1,receipt_kind=?2,receipt_evidence_digest=?3,receipt_digest=?4,updated_at=?5,completed_at=?5 WHERE change_id=?6 AND state='delivery_started' AND ledger_incarnation_ref=?7",params![state,receipt.kind.as_str(),receipt.evidence_digest,rd,now,receipt.change_id,self.ledger_incarnation_ref])?;
        let (from, to) = match receipt.kind {
            FreshTransferReceiptKind::Accepted => (
                LifecycleState::Dispatching,
                LifecycleState::AgentOwnedRepair,
            ),
            FreshTransferReceiptKind::DefinitiveNotDelivered => {
                (LifecycleState::Dispatching, LifecycleState::Actionable)
            }
            FreshTransferReceiptKind::Uncertain => {
                (LifecycleState::Dispatching, LifecycleState::Dispatching)
            }
        };
        record_event(
            &tx,
            &self.ledger_incarnation_ref,
            None,
            &started.claim.source.work_id,
            match receipt.kind {
                FreshTransferReceiptKind::Uncertain => started.claim.intermediate_work_generation,
                _ => started.claim.target_work_generation,
            },
            match receipt.kind {
                FreshTransferReceiptKind::Accepted => started.claim.target_owner_generation,
                _ => started.claim.source.owner_generation,
            },
            "fresh_owner_transfer_reconciled",
            Some(from),
            to,
            &rd,
            &now,
        )?;
        super::clock::LedgerClock::finalize(&tx)?;
        tx.commit()?;
        Ok(disposition)
    }
}

fn validate_fresh_target(
    claim: &FreshOwnerTransferClaim,
    target: &RouteRegistration,
) -> WorkLedgerResult<()> {
    if target.route_ref != claim.target_route_ref
        || target.work_id != claim.source.work_id
        || target.head_sha != claim.source.head_sha
        || target.work_generation != claim.target_work_generation
        || target.owner_ref != claim.target_owner_ref
        || target.owner_generation != claim.target_owner_generation
        || target.revision
            != claim
                .source
                .revision
                .checked_add(1)
                .ok_or_else(|| WorkLedgerError::Refused("route revision overflow".to_owned()))?
        || native_session(target) == native_session(&claim.source)
    {
        return Err(WorkLedgerError::Refused(
            "fresh-owner target fence is invalid".to_owned(),
        ));
    }
    let (sa, ta) = (&claim.source.provenance.agent, &target.provenance.agent);
    if sa.adapter != ta.adapter {
        return Err(WorkLedgerError::Refused(
            "fresh transfer changed agent adapter".to_owned(),
        ));
    }
    if let (
        AgentRoute::Named {
            name: source_name, ..
        },
        AgentRoute::Named {
            name: target_name, ..
        },
    ) = (&sa.route, &ta.route)
        && source_name != target_name
    {
        return Err(WorkLedgerError::Refused(
            "fresh transfer changed named-agent identity".to_owned(),
        ));
    }
    let ((AgentRoute::Codex { session: ss }, AgentRoute::Codex { session: ts })
    | (AgentRoute::Claude { session: ss }, AgentRoute::Claude { session: ts })
    | (
        AgentRoute::Named {
            name: _,
            session: ss,
        },
        AgentRoute::Named {
            name: _,
            session: ts,
        },
    )) = (&sa.route, &ta.route)
    else {
        return Err(WorkLedgerError::Refused(
            "fresh transfer changed agent kind".to_owned(),
        ));
    };
    if ss.account_ref != ts.account_ref
        || ss.model_ref != ts.model_ref
        || ss.wrapper_ref != ts.wrapper_ref
        || ss.session_headers_ref != ts.session_headers_ref
        || ss.session_headers_sha256 != ts.session_headers_sha256
    {
        return Err(WorkLedgerError::Refused(
            "fresh transfer changed protected session routing".to_owned(),
        ));
    }
    let sl = &claim.source.provenance.launch_profile;
    let tl = &target.provenance.launch_profile;
    if claim.source.provenance.provider != target.provenance.provider
        || sl.profile_ref != tl.profile_ref
        || sl.executable_sha256 != tl.executable_sha256
        || sl.wrapper_ref != tl.wrapper_ref
        || sl.configuration_sha256 != tl.configuration_sha256
        || sl.provider_kind != tl.provider_kind
        || tl.generation != claim.target_owner_generation
        || tl.revision
            != sl
                .revision
                .checked_add(1)
                .ok_or_else(|| WorkLedgerError::Refused("launch revision overflow".to_owned()))?
    {
        return Err(WorkLedgerError::Refused(
            "fresh transfer changed provider or launch profile".to_owned(),
        ));
    }
    Ok(())
}
