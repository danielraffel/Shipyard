//! Lifecycle transitions, continuation contracts, and transactional wake publication.
#![allow(dead_code)] // Native lifecycle activation follows the shadow phase.

use super::registry::validated_route_exists;
use super::{
    OptionalExtension, Transaction, TransactionBehavior, WorkLedger, WorkLedgerError,
    WorkLedgerResult, configure_durable, digest, opaque_ref, params, validate_digest,
    validate_opaque_ref, validate_token, verify_integrity, verify_supported_schema,
};

/// Optional wake written in the same transaction as a state transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WakeIntent {
    /// Deterministic opaque wake identity.
    pub(super) wake_id: String,
    /// Exact ledger lifetime that published this wake.
    pub(super) ledger_incarnation_ref: String,
    /// Work generation fenced by the receiver.
    pub(super) work_generation: u64,
    /// Owner generation fenced by the receiver.
    pub(super) owner_generation: u64,
    /// Opaque protected route reference.
    pub(super) route_ref: String,
    /// Digest of a separately protected payload.
    pub(super) payload_digest: String,
}

impl WakeIntent {
    /// Build a wake whose identity is derived from its complete delivery fence.
    pub(super) fn new(
        ledger_incarnation_ref: String,
        work_id: &str,
        work_generation: u64,
        owner_generation: u64,
        route_ref: String,
        payload_digest: String,
    ) -> WorkLedgerResult<Self> {
        validate_opaque_ref("work_id", work_id, "wi")?;
        validate_opaque_ref("ledger_incarnation_ref", &ledger_incarnation_ref, "ledger")?;
        validate_opaque_ref("route_ref", &route_ref, "route")?;
        validate_digest("payload_digest", &payload_digest)?;
        let wake_id = deterministic_wake_id(
            &ledger_incarnation_ref,
            work_id,
            work_generation,
            owner_generation,
            &route_ref,
            &payload_digest,
        );
        Ok(Self {
            wake_id,
            ledger_incarnation_ref,
            work_generation,
            owner_generation,
            route_ref,
            payload_digest,
        })
    }
}

/// Closed lifecycle states used by native ledger transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    /// Legacy projection is inert and cannot dispatch.
    ShadowImported,
    /// Native record and continuation contract are published.
    Published,
    /// Policy permits deterministic stewardship.
    Ready,
    /// The daemon owns routine reconciliation.
    Managed,
    /// Waiting for external evidence.
    Waiting,
    /// Semantic repair is required.
    Actionable,
    /// A validated wake is being delivered.
    Dispatching,
    /// A code-capable agent owns the repair.
    AgentOwnedRepair,
    /// Repair evidence returned to deterministic stewardship.
    Returned,
    /// All lifecycle work is terminal.
    Terminal,
}

impl LifecycleState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ShadowImported => "shadow_imported",
            Self::Published => "published",
            Self::Ready => "ready",
            Self::Managed => "managed",
            Self::Waiting => "waiting",
            Self::Actionable => "actionable",
            Self::Dispatching => "dispatching",
            Self::AgentOwnedRepair => "agent_owned_repair",
            Self::Returned => "returned",
            Self::Terminal => "terminal",
        }
    }

    fn parse(value: &str) -> WorkLedgerResult<Self> {
        match value {
            "shadow_imported" => Ok(Self::ShadowImported),
            "published" => Ok(Self::Published),
            "ready" => Ok(Self::Ready),
            "managed" => Ok(Self::Managed),
            "waiting" => Ok(Self::Waiting),
            "actionable" => Ok(Self::Actionable),
            "dispatching" => Ok(Self::Dispatching),
            "agent_owned_repair" => Ok(Self::AgentOwnedRepair),
            "returned" => Ok(Self::Returned),
            "terminal" => Ok(Self::Terminal),
            _ => Err(WorkLedgerError::Refused(
                "work item has an unsupported lifecycle state".to_owned(),
            )),
        }
    }

    fn permits(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::ShadowImported, Self::Published)
                | (Self::Published, Self::Ready)
                | (Self::Ready, Self::Managed)
                | (
                    Self::Managed,
                    Self::Waiting | Self::Actionable | Self::Terminal
                )
                | (Self::Waiting, Self::Actionable | Self::Terminal)
                | (Self::Actionable, Self::Dispatching | Self::Terminal)
                | (
                    Self::Dispatching,
                    Self::Actionable | Self::AgentOwnedRepair | Self::Terminal
                )
                | (Self::AgentOwnedRepair, Self::Returned | Self::Terminal)
                | (Self::Returned, Self::Managed | Self::Terminal)
        )
    }
}

/// Structurally complete success and failure continuation contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContinuationSet {
    success_contract_digest: String,
    success_route_ref: Option<String>,
    failure_contract_digest: String,
    failure_route_ref: Option<String>,
}

impl ContinuationSet {
    /// Construct both outcomes together; a one-sided contract is impossible.
    pub fn new(
        success_contract_digest: String,
        success_route_ref: Option<String>,
        failure_contract_digest: String,
        failure_route_ref: Option<String>,
    ) -> WorkLedgerResult<Self> {
        validate_digest("success contract digest", &success_contract_digest)?;
        validate_digest("failure contract digest", &failure_contract_digest)?;
        for route in [success_route_ref.as_deref(), failure_route_ref.as_deref()]
            .into_iter()
            .flatten()
        {
            validate_opaque_ref("continuation route", route, "route")?;
        }
        Ok(Self {
            success_contract_digest,
            success_route_ref,
            failure_contract_digest,
            failure_route_ref,
        })
    }
}

impl WorkLedger {
    /// Construct a wake fenced to this exact durable ledger lifetime.
    pub fn wake_intent(
        &self,
        work_id: &str,
        work_generation: u64,
        owner_generation: u64,
        route_ref: String,
        payload_digest: String,
    ) -> WorkLedgerResult<WakeIntent> {
        WakeIntent::new(
            self.ledger_incarnation_ref.clone(),
            work_id,
            work_generation,
            owner_generation,
            route_ref,
            payload_digest,
        )
    }

    pub(super) fn transition_with_wake(
        &self,
        work_id: &str,
        expected_work_generation: u64,
        expected_owner_generation: u64,
        next: LifecycleState,
        wake: Option<&WakeIntent>,
    ) -> WorkLedgerResult<()> {
        validate_opaque_ref("work_id", work_id, "wi")?;
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
        let now = self.clock.observe(&transaction)?.timestamp.to_rfc3339();
        let current = validate_transition(
            &transaction,
            work_id,
            expected_work_generation,
            expected_owner_generation,
            next,
            wake.is_some(),
        )?;
        let changed = transaction.execute(
            "UPDATE work_items SET phase = ?1, work_generation = work_generation + 1,
                    updated_at = ?2
             WHERE id = ?3 AND work_generation = ?4 AND owner_generation = ?5",
            params![
                next.as_str(),
                now,
                work_id,
                expected_work_generation,
                expected_owner_generation
            ],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "work or owner generation no longer matches".to_owned(),
            ));
        }
        if let Some(wake) = wake {
            enqueue_wake(
                &transaction,
                &self.ledger_incarnation_ref,
                work_id,
                expected_work_generation,
                expected_owner_generation,
                wake,
                &now,
            )?;
        }
        let event_payload = wake.map_or_else(
            || digest(b"state-only-transition"),
            |intent| intent.payload_digest.clone(),
        );
        record_event(
            &transaction,
            &self.ledger_incarnation_ref,
            None,
            work_id,
            expected_work_generation + 1,
            expected_owner_generation,
            "lifecycle_transition",
            Some(current),
            next,
            &event_payload,
            &now,
        )?;
        super::clock::LedgerClock::finalize(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    /// Publish or revise both continuation outcomes under one revision fence.
    #[allow(dead_code)] // Kept private until scheduler activation; exercised by contract tests.
    pub(super) fn record_continuations(
        &self,
        work_id: &str,
        expected_revision: u64,
        continuations: &ContinuationSet,
    ) -> WorkLedgerResult<u64> {
        validate_opaque_ref("work_id", work_id, "wi")?;
        if expected_revision == u64::MAX {
            return Err(WorkLedgerError::Refused(
                "continuation revision is exhausted".to_owned(),
            ));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = self.clock.observe(&transaction)?.timestamp.to_rfc3339();
        let next_revision = expected_revision + 1;
        let changed = if expected_revision == 0 {
            transaction.execute(
                "INSERT OR IGNORE INTO continuation_contracts
                 (work_item_id, success_contract_digest, success_route_ref, success_state,
                  failure_contract_digest, failure_route_ref, failure_state, revision,
                  created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'pending', ?4, ?5, 'pending', ?6, ?7, ?7)",
                params![
                    work_id,
                    continuations.success_contract_digest,
                    continuations.success_route_ref,
                    continuations.failure_contract_digest,
                    continuations.failure_route_ref,
                    next_revision,
                    now,
                ],
            )?
        } else {
            transaction.execute(
                "UPDATE continuation_contracts SET success_contract_digest = ?1,
                 success_route_ref = ?2, failure_contract_digest = ?3,
                 failure_route_ref = ?4, success_state = 'pending',
                 failure_state = 'pending', revision = ?5, updated_at = ?6
                 WHERE work_item_id = ?7 AND revision = ?8",
                params![
                    continuations.success_contract_digest,
                    continuations.success_route_ref,
                    continuations.failure_contract_digest,
                    continuations.failure_route_ref,
                    next_revision,
                    now,
                    work_id,
                    expected_revision,
                ],
            )?
        };
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "continuation revision no longer matches".to_owned(),
            ));
        }
        super::clock::LedgerClock::finalize(&transaction)?;
        transaction.commit()?;
        Ok(next_revision)
    }
}

fn validate_transition(
    transaction: &Transaction<'_>,
    work_id: &str,
    work_generation: u64,
    owner_generation: u64,
    next: LifecycleState,
    has_wake: bool,
) -> WorkLedgerResult<LifecycleState> {
    let current_value: String = transaction
        .query_row(
            "SELECT phase FROM work_items
             WHERE id = ?1 AND work_generation = ?2 AND owner_generation = ?3",
            params![work_id, work_generation, owner_generation],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| {
            WorkLedgerError::Refused("work or owner generation no longer matches".to_owned())
        })?;
    let current = LifecycleState::parse(&current_value)?;
    if !current.permits(next) {
        return Err(WorkLedgerError::Refused(
            "illegal lifecycle transition".to_owned(),
        ));
    }
    if current == LifecycleState::Dispatching && next == LifecycleState::Actionable {
        return Err(WorkLedgerError::Refused(
            "dispatch failure rollback requires a definitive typed delivery receipt".to_owned(),
        ));
    }
    if current == LifecycleState::Dispatching && next == LifecycleState::AgentOwnedRepair {
        return Err(WorkLedgerError::Refused(
            "agent ownership requires an accepted typed delivery receipt".to_owned(),
        ));
    }
    if current == LifecycleState::Dispatching && next == LifecycleState::Terminal {
        return Err(WorkLedgerError::Refused(
            "terminating an active dispatch requires a typed delivery outcome".to_owned(),
        ));
    }
    if current == LifecycleState::ShadowImported {
        let complete: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM continuation_contracts WHERE work_item_id = ?1)",
            [work_id],
            |row| row.get(0),
        )?;
        if !complete {
            return Err(WorkLedgerError::Refused(
                "work item has no complete continuation contract".to_owned(),
            ));
        }
    }
    if (next == LifecycleState::Dispatching) != has_wake {
        return Err(WorkLedgerError::Refused(
            "dispatching requires exactly one validated wake".to_owned(),
        ));
    }
    Ok(current)
}

fn enqueue_wake(
    transaction: &Transaction<'_>,
    ledger_incarnation_ref: &str,
    work_id: &str,
    work_generation: u64,
    owner_generation: u64,
    wake: &WakeIntent,
    now: &str,
) -> WorkLedgerResult<()> {
    validate_wake(
        ledger_incarnation_ref,
        work_id,
        wake,
        work_generation + 1,
        owner_generation,
    )?;
    let route_matches = validated_route_exists(
        transaction,
        &wake.route_ref,
        work_id,
        work_generation,
        owner_generation,
    )?;
    if !route_matches {
        return Err(WorkLedgerError::Refused(
            "wake route is missing, stale, or belongs to different work".to_owned(),
        ));
    }
    transaction.execute(
        "INSERT INTO outbox (
           wake_id, ledger_incarnation_ref, work_item_id, work_generation, owner_generation, state,
           route_ref, payload_digest, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7, ?8, ?8)",
        params![
            wake.wake_id,
            ledger_incarnation_ref,
            work_id,
            wake.work_generation,
            wake.owner_generation,
            wake.route_ref,
            wake.payload_digest,
            now,
        ],
    )?;
    Ok(())
}

fn validate_wake(
    ledger_incarnation_ref: &str,
    work_id: &str,
    wake: &WakeIntent,
    expected_work_generation: u64,
    expected_owner_generation: u64,
) -> WorkLedgerResult<()> {
    for (name, value) in [
        ("wake_id", wake.wake_id.as_str()),
        ("route_ref", wake.route_ref.as_str()),
        ("payload_digest", wake.payload_digest.as_str()),
    ] {
        validate_token(name, value)?;
    }
    if wake.ledger_incarnation_ref != ledger_incarnation_ref
        || wake.work_generation != expected_work_generation
        || wake.owner_generation != expected_owner_generation
    {
        return Err(WorkLedgerError::Refused(
            "wake generation does not match transitioned work".to_owned(),
        ));
    }
    let expected_wake_id = deterministic_wake_id(
        ledger_incarnation_ref,
        work_id,
        wake.work_generation,
        wake.owner_generation,
        &wake.route_ref,
        &wake.payload_digest,
    );
    if wake.wake_id != expected_wake_id {
        return Err(WorkLedgerError::Refused(
            "wake identity does not match its delivery fence".to_owned(),
        ));
    }
    Ok(())
}

fn deterministic_wake_id(
    ledger_incarnation_ref: &str,
    work_id: &str,
    work_generation: u64,
    owner_generation: u64,
    route_ref: &str,
    payload_digest: &str,
) -> String {
    opaque_ref(
        "wake",
        &format!(
            "{ledger_incarnation_ref}\n{work_id}\n{work_generation}\n{owner_generation}\n{route_ref}\n{payload_digest}"
        ),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn record_event(
    transaction: &Transaction<'_>,
    ledger_incarnation_ref: &str,
    dispatcher_epoch_ref: Option<&str>,
    work_id: &str,
    work_generation: u64,
    owner_generation: u64,
    kind: &str,
    from: Option<LifecycleState>,
    to: LifecycleState,
    payload_digest: &str,
    created_at: &str,
) -> WorkLedgerResult<()> {
    validate_opaque_ref("work_id", work_id, "wi")?;
    validate_opaque_ref("ledger_incarnation_ref", ledger_incarnation_ref, "ledger")?;
    if let Some(dispatcher_epoch_ref) = dispatcher_epoch_ref {
        validate_opaque_ref("dispatcher_epoch_ref", dispatcher_epoch_ref, "dispatcher")?;
    }
    validate_digest("event payload digest", payload_digest)?;
    let from_state = from.map(LifecycleState::as_str);
    let identity = format!(
        "{ledger_incarnation_ref}\n{}\n{work_id}\n{work_generation}\n{owner_generation}\n{kind}\n{}\n{}\n{payload_digest}",
        dispatcher_epoch_ref.unwrap_or(""),
        from_state.unwrap_or(""),
        to.as_str()
    );
    let event_id = opaque_ref("event", &identity);
    transaction.execute(
        "INSERT OR IGNORE INTO events
         (event_id, ledger_incarnation_ref, dispatcher_epoch_ref, work_item_id, work_generation, owner_generation, kind,
          from_state, to_state, payload_digest, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            event_id,
            ledger_incarnation_ref,
            dispatcher_epoch_ref,
            work_id,
            work_generation,
            owner_generation,
            kind,
            from_state,
            to.as_str(),
            payload_digest,
            created_at,
        ],
    )?;
    let exact: bool = transaction.query_row(
        "SELECT ledger_incarnation_ref = ?2
                AND ifnull(dispatcher_epoch_ref, '') = ifnull(?3, '')
                AND work_item_id = ?4 AND work_generation = ?5 AND owner_generation = ?6
                AND kind = ?7 AND ifnull(from_state, '') = ifnull(?8, '')
                AND to_state = ?9 AND payload_digest = ?10
         FROM events WHERE event_id = ?1",
        params![
            event_id,
            ledger_incarnation_ref,
            dispatcher_epoch_ref,
            work_id,
            work_generation,
            owner_generation,
            kind,
            from_state,
            to.as_str(),
            payload_digest,
        ],
        |row| row.get(0),
    )?;
    if !exact {
        return Err(WorkLedgerError::Refused(
            "event identity collides with different audit evidence".to_owned(),
        ));
    }
    Ok(())
}
