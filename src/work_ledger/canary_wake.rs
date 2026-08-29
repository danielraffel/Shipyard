//! Admission and crash-safe publication of canary terminal wakes.
//!
//! A canary never constructs a route. Admission binds an exact already-published
//! native work generation, staged route, and protected launch profile. Terminal
//! delivery advances that work through the existing transactional outbox path
//! and writes a canary-specific audit event in the same `SQLite` transaction.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::registry::validated_route_exists;
use super::{
    LifecycleState, WakeIntent, WorkLedger, WorkLedgerError, WorkLedgerResult, configure_durable,
    validate_digest, validate_opaque_ref, verify_integrity, verify_supported_schema,
};
use crate::parallel_proof::Sha256Digest;
use crate::parallel_proof_canary_job::CanaryNativeContinuationBinding;

const BINDING_SCHEMA_VERSION: u32 = 1;

/// Durable proof that one exact canary terminal receipt reached native custody.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(
    windows,
    allow(
        dead_code,
        reason = "terminal wake publication is driven by the Unix daemon lane"
    )
)]
pub(crate) struct CanaryNativeWakeDelivery {
    pub(crate) wake_id: String,
    pub(crate) receipt_sha256: Sha256Digest,
}

impl WorkLedger {
    /// Verify the complete native authority before a canary job becomes durable.
    pub(crate) fn verify_canary_continuation_binding(
        &self,
        binding: &CanaryNativeContinuationBinding,
    ) -> WorkLedgerResult<()> {
        validate_binding_shape(binding)?;
        let route_generation = binding.work_generation.checked_add(1).ok_or_else(|| {
            WorkLedgerError::Refused("canary native work generation is exhausted".to_owned())
        })?;
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
        let work: Option<(String, u64, u64, Option<String>)> = transaction
            .query_row(
                "SELECT phase, work_generation, owner_generation, repair_route_ref
                   FROM work_items WHERE id = ?1",
                [&binding.work_item_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((phase, work_generation, owner_generation, repair_route_ref)) = work else {
            return Err(WorkLedgerError::Refused(
                "canary native work item is unavailable".to_owned(),
            ));
        };
        if !matches!(phase.as_str(), "managed" | "waiting")
            || work_generation != binding.work_generation
            || owner_generation != binding.owner_generation
            || repair_route_ref.as_deref() != Some(binding.route_ref.as_str())
        {
            return Err(WorkLedgerError::Refused(
                "canary native work authority is stale or contradictory".to_owned(),
            ));
        }
        if !validated_route_exists(
            &transaction,
            &binding.route_ref,
            &binding.work_item_id,
            route_generation,
            binding.owner_generation,
        )? {
            return Err(WorkLedgerError::Refused(
                "canary native route authority is missing or stale".to_owned(),
            ));
        }
        verify_route_profile(&transaction, binding)?;
        let prior_delivery: bool = transaction.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM events
                WHERE work_item_id = ?1 AND kind = 'canary_terminal_wake_delivery'
             )",
            [&binding.work_item_id],
            |row| row.get(0),
        )?;
        if prior_delivery {
            return Err(WorkLedgerError::Refused(
                "native work already has a canary terminal delivery".to_owned(),
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Publish or replay one exact terminal receipt into the native outbox.
    ///
    /// The returned receipt is safe to acknowledge in the canary store only
    /// after this method succeeds. A crash after the `SQLite` commit but before
    /// acknowledgement replays the same event and outbox row without dispatching
    /// a second wake.
    #[cfg_attr(
        windows,
        allow(
            dead_code,
            reason = "terminal wake publication is driven by the Unix daemon lane"
        )
    )]
    pub(crate) fn deliver_canary_terminal_wake(
        &self,
        binding: &CanaryNativeContinuationBinding,
        job_sha256: &Sha256Digest,
        terminal_receipt_sha256: &Sha256Digest,
    ) -> WorkLedgerResult<CanaryNativeWakeDelivery> {
        validate_binding_shape(binding)?;
        let actionable_generation = binding.work_generation.checked_add(1).ok_or_else(|| {
            WorkLedgerError::Refused("canary native work generation is exhausted".to_owned())
        })?;
        let dispatch_generation = actionable_generation.checked_add(1).ok_or_else(|| {
            WorkLedgerError::Refused("canary native work generation is exhausted".to_owned())
        })?;
        let wake = WakeIntent::new(
            &binding.work_item_id,
            dispatch_generation,
            binding.owner_generation,
            binding.route_ref.clone(),
            binding.payload_digest.clone(),
        )?;
        let receipt_sha256 = crate::parallel_proof_canary_job::native_wake_delivery_digest(
            binding,
            job_sha256,
            terminal_receipt_sha256,
            &wake.wake_id,
        )
        .map_err(|error| WorkLedgerError::Refused(error.to_string()))?;

        let (phase, generation, owner_generation) = self.canary_native_work_state(binding)?;
        if matches!(phase.as_str(), "managed" | "waiting") {
            if generation != binding.work_generation || owner_generation != binding.owner_generation
            {
                return Err(WorkLedgerError::Refused(
                    "canary native work generation changed before terminal delivery".to_owned(),
                ));
            }
            self.transition_with_wake(
                &binding.work_item_id,
                generation,
                owner_generation,
                LifecycleState::Actionable,
                None,
            )?;
        }

        let (phase, generation, owner_generation) = self.canary_native_work_state(binding)?;
        if phase == LifecycleState::Actionable.as_str() {
            if generation != actionable_generation || owner_generation != binding.owner_generation {
                return Err(WorkLedgerError::Refused(
                    "canary native actionable authority is contradictory".to_owned(),
                ));
            }
            self.transition_with_wake_and_delivery_receipt(
                &binding.work_item_id,
                generation,
                owner_generation,
                &wake,
                receipt_sha256.as_str(),
            )?;
        }

        self.verify_canary_delivery_receipt(
            binding,
            &wake,
            dispatch_generation,
            receipt_sha256.as_str(),
        )?;
        Ok(CanaryNativeWakeDelivery {
            wake_id: wake.wake_id,
            receipt_sha256,
        })
    }

    #[cfg_attr(
        windows,
        allow(
            dead_code,
            reason = "terminal wake publication is driven by the Unix daemon lane"
        )
    )]
    fn canary_native_work_state(
        &self,
        binding: &CanaryNativeContinuationBinding,
    ) -> WorkLedgerResult<(String, u64, u64)> {
        self.connect_read_only()?
            .query_row(
                "SELECT phase, work_generation, owner_generation
                   FROM work_items WHERE id = ?1 AND repair_route_ref = ?2",
                params![binding.work_item_id, binding.route_ref],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| {
                WorkLedgerError::Refused("canary native work authority disappeared".to_owned())
            })
    }

    #[cfg_attr(
        windows,
        allow(
            dead_code,
            reason = "terminal wake publication is driven by the Unix daemon lane"
        )
    )]
    fn verify_canary_delivery_receipt(
        &self,
        binding: &CanaryNativeContinuationBinding,
        wake: &WakeIntent,
        dispatch_generation: u64,
        receipt_sha256: &str,
    ) -> WorkLedgerResult<()> {
        let exact: Option<bool> = self
            .connect_read_only()?
            .query_row(
                "SELECT outbox.work_item_id = ?2
                    AND outbox.work_generation = ?3
                    AND outbox.owner_generation = ?4
                    AND outbox.route_ref = ?5
                    AND outbox.payload_digest = ?6
                    AND event.work_item_id = outbox.work_item_id
                    AND event.work_generation = outbox.work_generation
                    AND event.owner_generation = outbox.owner_generation
                    AND event.kind = 'canary_terminal_wake_delivery'
                    AND event.payload_digest = ?7
               FROM outbox
               JOIN events event ON event.work_item_id = outbox.work_item_id
                                AND event.work_generation = outbox.work_generation
              WHERE outbox.wake_id = ?1",
                params![
                    wake.wake_id,
                    binding.work_item_id,
                    dispatch_generation,
                    binding.owner_generation,
                    binding.route_ref,
                    binding.payload_digest,
                    receipt_sha256,
                ],
                |row| row.get(0),
            )
            .optional()?;
        if exact != Some(true) {
            return Err(WorkLedgerError::Refused(
                "canary native delivery receipt is missing or contradictory".to_owned(),
            ));
        }
        Ok(())
    }
}

fn validate_binding_shape(binding: &CanaryNativeContinuationBinding) -> WorkLedgerResult<()> {
    if binding.schema_version != BINDING_SCHEMA_VERSION
        || binding.work_generation == 0
        || binding.owner_generation == 0
    {
        return Err(WorkLedgerError::Refused(
            "canary native continuation binding is unsupported".to_owned(),
        ));
    }
    validate_opaque_ref("canary native work item", &binding.work_item_id, "wi")?;
    validate_opaque_ref("canary native route", &binding.route_ref, "route")?;
    let Some(profile_hash) = binding.profile_ref.strip_prefix("opaque:sha256:") else {
        return Err(WorkLedgerError::Refused(
            "invalid canary native launch profile".to_owned(),
        ));
    };
    validate_digest("canary native launch profile", profile_hash)?;
    validate_digest("canary native payload digest", &binding.payload_digest)
}

fn verify_route_profile(
    transaction: &rusqlite::Transaction<'_>,
    binding: &CanaryNativeContinuationBinding,
) -> WorkLedgerResult<()> {
    let route_payload: Vec<u8> = transaction.query_row(
        "SELECT payload_json FROM route_records WHERE route_ref = ?1",
        [&binding.route_ref],
        |row| row.get(0),
    )?;
    let route: serde_json::Value = serde_json::from_slice(&route_payload).map_err(|_| {
        WorkLedgerError::Refused("canary native route payload is malformed".to_owned())
    })?;
    if route
        .pointer("/launch_profile/profile_ref")
        .and_then(|value| value.as_str())
        != Some(binding.profile_ref.as_str())
    {
        return Err(WorkLedgerError::Refused(
            "canary native route selects a different launch profile".to_owned(),
        ));
    }
    let exact_profile: bool = transaction.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM protected_objects
            WHERE work_item_id = ?1 AND kind = 'launch_profile'
              AND profile_ref = ?2 AND content_digest = ?3
         )",
        params![
            binding.work_item_id,
            binding.profile_ref,
            binding.payload_digest
        ],
        |row| row.get(0),
    )?;
    if !exact_profile {
        return Err(WorkLedgerError::Refused(
            "canary native protected launch profile is missing or contradictory".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::work_ledger::{
        RepoPolicy, native_publication_test_policy, native_publication_test_request,
    };

    fn published() -> (
        tempfile::TempDir,
        WorkLedger,
        CanaryNativeContinuationBinding,
    ) {
        let state = tempfile::tempdir().expect("state");
        let request = native_publication_test_request();
        let ledger = WorkLedger::open(state.path()).expect("ledger");
        ledger
            .set_repo_policy(
                &RepoPolicy {
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
            .expect("policy");
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &request,
            &native_publication_test_policy(vec![request.repository.clone()]),
            true,
        )
        .expect("publish");
        let ledger = WorkLedger::open_existing(state.path())
            .expect("open")
            .expect("existing ledger");
        let binding = ledger
            .connect_read_only()
            .expect("connection")
            .query_row(
                "SELECT work.id, work.work_generation, work.owner_generation,
                        work.repair_route_ref, object.profile_ref, object.content_digest
                   FROM work_items work
                   JOIN protected_objects object ON object.work_item_id = work.id
                    AND object.kind = 'launch_profile'
                  WHERE work.kind = 'terminal_handoff'",
                [],
                |row| {
                    Ok(CanaryNativeContinuationBinding {
                        schema_version: 1,
                        work_item_id: row.get(0)?,
                        work_generation: row.get(1)?,
                        owner_generation: row.get(2)?,
                        route_ref: row.get(3)?,
                        profile_ref: row.get(4)?,
                        payload_digest: row.get(5)?,
                    })
                },
            )
            .expect("binding");
        (state, ledger, binding)
    }

    fn counts(ledger: &WorkLedger) -> (String, u64, u64) {
        ledger
            .connect_read_only()
            .expect("connection")
            .query_row(
                "SELECT phase,
                        (SELECT COUNT(*) FROM outbox),
                        (SELECT COUNT(*) FROM events
                          WHERE kind = 'canary_terminal_wake_delivery')
                   FROM work_items WHERE kind = 'terminal_handoff'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("counts")
    }

    #[test]
    fn admission_requires_exact_existing_generation_route_profile_and_payload() {
        let (_state, ledger, binding) = published();
        ledger
            .verify_canary_continuation_binding(&binding)
            .expect("exact binding");
        for drift in ["generation", "route", "profile", "payload"] {
            let mut changed = binding.clone();
            match drift {
                "generation" => changed.work_generation += 1,
                "route" => changed.route_ref = format!("route_{}", "a".repeat(64)),
                "profile" => {
                    changed.profile_ref = format!("opaque:sha256:{}", "b".repeat(64));
                }
                "payload" => changed.payload_digest = "c".repeat(64),
                _ => unreachable!(),
            }
            assert!(ledger.verify_canary_continuation_binding(&changed).is_err());
            assert_eq!(counts(&ledger), ("managed".to_owned(), 0, 0));
        }
    }

    #[test]
    fn crash_after_actionable_replays_one_transactional_native_delivery() {
        let (_state, ledger, binding) = published();
        ledger
            .verify_canary_continuation_binding(&binding)
            .expect("admit");
        ledger
            .transition_with_wake(
                &binding.work_item_id,
                binding.work_generation,
                binding.owner_generation,
                LifecycleState::Actionable,
                None,
            )
            .expect("simulated pre-delivery commit");

        let job = Sha256Digest::of_bytes(b"canary job");
        let terminal = Sha256Digest::of_bytes(b"terminal receipt");
        let first = ledger
            .deliver_canary_terminal_wake(&binding, &job, &terminal)
            .expect("deliver after restart");
        assert_eq!(counts(&ledger), ("dispatching".to_owned(), 1, 1));

        let replay = ledger
            .deliver_canary_terminal_wake(&binding, &job, &terminal)
            .expect("replay after delivery-before-ack crash");
        assert_eq!(replay, first);
        assert_eq!(counts(&ledger), ("dispatching".to_owned(), 1, 1));
    }

    #[test]
    fn contradictory_terminal_receipt_cannot_claim_an_existing_delivery() {
        let (_state, ledger, binding) = published();
        let job = Sha256Digest::of_bytes(b"canary job");
        ledger
            .deliver_canary_terminal_wake(
                &binding,
                &job,
                &Sha256Digest::of_bytes(b"terminal receipt"),
            )
            .expect("first delivery");
        assert!(
            ledger
                .deliver_canary_terminal_wake(
                    &binding,
                    &job,
                    &Sha256Digest::of_bytes(b"different terminal receipt"),
                )
                .is_err()
        );
        assert_eq!(counts(&ledger), ("dispatching".to_owned(), 1, 1));
    }
}
