//! Renewable exact-identity stewardship leases for acknowledged agent ownership.
//!
//! The lease is subordinate to `agent_ownership`; it cannot import workstream
//! state or become a second authority. Adoption either reattaches the exact
//! current holder or creates one successor generation after durable release,
//! expiry. There is deliberately no process-death adoption mode until a
//! custody-host issuer can produce a protected, durable, authenticated receipt.

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use super::{
    WorkLedger, WorkLedgerError, WorkLedgerResult, configure_durable, digest,
    is_canonical_repo_slug, opaque_ref, validate_digest, validate_opaque_ref,
    validate_workstream_handle, verify_integrity, verify_supported_schema,
};

const MAX_LEASE_DURATION: Duration = Duration::minutes(5);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnershipHolderMaterial {
    pub(crate) schema_version: u32,
    pub(crate) ownership_id: String,
    pub(crate) lease_generation: u64,
    pub(crate) holder_ref: String,
    pub(crate) incarnation_ref: String,
    pub(crate) credential: String,
}

impl OwnershipHolderMaterial {
    fn holder(&self) -> OwnershipLeaseHolder {
        OwnershipLeaseHolder {
            holder_ref: self.holder_ref.clone(),
            incarnation_ref: self.incarnation_ref.clone(),
            credential_digest: digest(self.credential.as_bytes()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct OwnershipLeaseFence {
    pub(crate) ownership_id: String,
    pub(crate) work_item_id: String,
    pub(crate) repository_provider: String,
    pub(crate) repository_id: String,
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) exact_head: String,
    pub(crate) workstream_handle: String,
    pub(crate) root_uuid: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct OwnershipLeaseHolder {
    pub(crate) holder_ref: String,
    pub(crate) incarnation_ref: String,
    pub(crate) credential_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct OwnershipLease {
    pub(crate) lease_id: String,
    pub(crate) fence: OwnershipLeaseFence,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) lease_generation: u64,
    pub(crate) holder: OwnershipLeaseHolder,
    pub(crate) state: String,
    pub(crate) predecessor_lease_id: Option<String>,
    pub(crate) transition_kind: String,
    pub(crate) proof_digest: String,
    pub(crate) release_digest: Option<String>,
    pub(crate) acquired_at: DateTime<Utc>,
    pub(crate) expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum OwnershipAdoptionProof {
    Expired { expected_expires_at: DateTime<Utc> },
    ExplicitRelease { release_digest: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OwnershipAdoptionResult {
    Attached(OwnershipLease),
    SuccessorCreated(OwnershipLease),
}

impl WorkLedger {
    pub(crate) fn ownership_holder_material(
        &self,
        work_item_id: &str,
        ownership_id: &str,
        lease_generation: u64,
    ) -> WorkLedgerResult<(String, Vec<u8>, OwnershipLeaseHolder)> {
        validate_opaque_ref("agent ownership", ownership_id, "ao")?;
        let connection = self.connect_read_only()?;
        let existing: Option<String> = connection
            .query_row(
                "SELECT object_ref FROM ownership_holder_materials
                  WHERE ownership_id = ?1 AND lease_generation = ?2 AND work_item_id = ?3",
                params![ownership_id, lease_generation, work_item_id],
                |row| row.get(0),
            )
            .optional()?;
        drop(connection);
        if let Some(object_ref) = existing {
            let (_, bytes) = self.open_protected_object(&object_ref)?;
            let material = validate_holder_material(&bytes, ownership_id, lease_generation)?;
            return Ok((object_ref, bytes, material.holder()));
        }
        let connection = self.connect_read_only()?;
        let credential: String =
            connection.query_row("SELECT lower(hex(randomblob(32)))", [], |row| row.get(0))?;
        drop(connection);
        let material = OwnershipHolderMaterial {
            schema_version: 1,
            ownership_id: ownership_id.to_owned(),
            lease_generation,
            holder_ref: opaque_ref("owner", &format!("{ownership_id}\nholder")),
            incarnation_ref: opaque_ref(
                "incarnation",
                &format!("{ownership_id}\n{lease_generation}\nincarnation"),
            ),
            credential,
        };
        let bytes = serde_json::to_vec(&material)
            .map_err(|_| refused("ownership holder material cannot be serialized"))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| refused("database has no parent"))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let connection = self.connect_read_only()?;
        let winner: Option<String> = connection
            .query_row(
                "SELECT object_ref FROM ownership_holder_materials
                  WHERE ownership_id = ?1 AND lease_generation = ?2 AND work_item_id = ?3",
                params![ownership_id, lease_generation, work_item_id],
                |row| row.get(0),
            )
            .optional()?;
        drop(connection);
        if let Some(object_ref) = winner {
            let (_, winner_bytes) = self.open_protected_object(&object_ref)?;
            let winner_material =
                validate_holder_material(&winner_bytes, ownership_id, lease_generation)?;
            return Ok((object_ref, winner_bytes, winner_material.holder()));
        }
        let record = self.put_protected_object_with_writer_domain(
            work_item_id,
            super::ProtectedObjectKind::AgentReceipt,
            None,
            &digest(&bytes),
            &bytes,
        )?;
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO ownership_holder_materials
             (ownership_id, lease_generation, work_item_id, object_ref, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                ownership_id,
                lease_generation,
                work_item_id,
                record.object_ref,
                Utc::now().to_rfc3339(),
            ],
        )?;
        transaction.commit()?;
        Ok((record.object_ref, bytes, material.holder()))
    }

    pub(crate) fn ownership_lease_fence(
        &self,
        ownership_id: &str,
    ) -> WorkLedgerResult<OwnershipLeaseFence> {
        let connection = self.connect_read_only()?;
        ownership_lease_fence_from_connection(&connection, ownership_id)
    }

    pub(crate) fn renew_ownership_lease_with_material(
        &self,
        ownership_id: &str,
        holder_bytes: &[u8],
        expected_lease_generation: u64,
        expires_at: DateTime<Utc>,
    ) -> WorkLedgerResult<(OwnershipLease, Vec<u8>)> {
        let (material_generation, holder) = holder_from_material(holder_bytes, ownership_id)?;
        if material_generation != expected_lease_generation {
            return Err(refused("ownership renewal holder generation is stale"));
        }
        let fence = self.ownership_lease_fence(ownership_id)?;
        let successor_generation = expected_lease_generation
            .checked_add(1)
            .ok_or_else(|| refused("ownership lease generation exhausted"))?;
        let (_object_ref, successor_bytes, successor) = self.ownership_holder_material(
            &fence.work_item_id,
            ownership_id,
            successor_generation,
        )?;
        let renewed = self.renew_ownership_lease(
            &fence,
            &holder,
            &successor,
            expected_lease_generation,
            expires_at,
        )?;
        Ok((renewed, successor_bytes))
    }

    pub(crate) fn bootstrap_legacy_ownership_with_protected_holder(
        &self,
        ownership_id: &str,
        expires_at: DateTime<Utc>,
    ) -> WorkLedgerResult<(OwnershipLease, Vec<u8>)> {
        let fence = self.ownership_lease_fence(ownership_id)?;
        let (_object_ref, material_bytes, holder) =
            self.ownership_holder_material(&fence.work_item_id, ownership_id, 1)?;
        let (_writer, mut connection) = self.lease_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let eligible: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ownership_lease_bootstrap_eligibility
              WHERE ownership_id = ?1)",
            [ownership_id],
            |row| row.get(0),
        )?;
        if !eligible {
            return Err(refused(
                "ownership is not eligible for legacy lease bootstrap",
            ));
        }
        let now = Utc::now();
        let lease =
            establish_ownership_lease_in_transaction(&tx, &fence, &holder, expires_at, now)?;
        tx.commit()?;
        Ok((lease, material_bytes))
    }

    pub(crate) fn release_ownership_lease_with_material(
        &self,
        ownership_id: &str,
        holder_bytes: &[u8],
        expected_lease_generation: u64,
    ) -> WorkLedgerResult<String> {
        let (material_generation, holder) = holder_from_material(holder_bytes, ownership_id)?;
        if material_generation != expected_lease_generation {
            return Err(refused("ownership release holder generation is stale"));
        }
        let fence = self.ownership_lease_fence(ownership_id)?;
        self.release_ownership_lease(&fence, &holder, expected_lease_generation)
    }

    pub(super) fn active_ownership_lease_with_material(
        &self,
        ownership_id: &str,
        holder_bytes: &[u8],
        expected_lease_generation: u64,
    ) -> WorkLedgerResult<OwnershipLease> {
        let (_material_generation, holder) = holder_from_material(holder_bytes, ownership_id)?;
        let mut connection = self.connect_read_write()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let lease = latest_lease(&tx, ownership_id)?
            .ok_or_else(|| refused("acknowledged ownership lease is missing"))?;
        if lease.state != "active"
            || lease.expires_at <= Utc::now()
            || lease.lease_generation != expected_lease_generation
            || lease.holder != holder
        {
            return Err(refused(
                "custody successor holder material is stale or unauthorized",
            ));
        }
        tx.commit()?;
        Ok(lease)
    }

    pub(crate) fn adopt_ownership_with_protected_holder(
        &self,
        ownership_id: &str,
        expected_lease_generation: u64,
        expires_at: DateTime<Utc>,
        proof_bytes: &[u8],
        existing_holder_bytes: Option<&[u8]>,
    ) -> WorkLedgerResult<(OwnershipAdoptionResult, Vec<u8>)> {
        let proof: OwnershipAdoptionProof = serde_json::from_slice(proof_bytes)
            .map_err(|_| refused("ownership adoption proof is malformed"))?;
        let fence = self.ownership_lease_fence(ownership_id)?;
        if let Some(existing_holder_bytes) = existing_holder_bytes {
            let (_material_generation, holder) =
                holder_from_material(existing_holder_bytes, ownership_id)?;
            let attached = self.adopt_ownership(
                &fence,
                &holder,
                expected_lease_generation,
                expires_at,
                proof,
            )?;
            return Ok((attached, existing_holder_bytes.to_vec()));
        }
        let successor_generation = expected_lease_generation
            .checked_add(1)
            .ok_or_else(|| refused("ownership lease generation exhausted"))?;
        let (_object_ref, material_bytes, successor) = self.ownership_holder_material(
            &fence.work_item_id,
            ownership_id,
            successor_generation,
        )?;
        let adopted = self.adopt_ownership(
            &fence,
            &successor,
            expected_lease_generation,
            expires_at,
            proof,
        )?;
        Ok((adopted, material_bytes))
    }

    #[cfg(test)]
    pub(crate) fn establish_ownership_lease(
        &self,
        fence: &OwnershipLeaseFence,
        holder: &OwnershipLeaseHolder,
        expires_at: DateTime<Utc>,
    ) -> WorkLedgerResult<OwnershipLease> {
        self.establish_ownership_lease_at(fence, holder, expires_at, Utc::now())
    }

    #[cfg(test)]
    fn establish_ownership_lease_at(
        &self,
        fence: &OwnershipLeaseFence,
        holder: &OwnershipLeaseHolder,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> WorkLedgerResult<OwnershipLease> {
        validate_fence(fence)?;
        validate_holder(holder)?;
        validate_expiry(now, expires_at)?;
        let (_writer, mut connection) = self.lease_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let lease = establish_ownership_lease_in_transaction(&tx, fence, holder, expires_at, now)?;
        tx.commit()?;
        Ok(lease)
    }

    pub(crate) fn renew_ownership_lease(
        &self,
        fence: &OwnershipLeaseFence,
        predecessor_holder: &OwnershipLeaseHolder,
        successor_holder: &OwnershipLeaseHolder,
        expected_lease_generation: u64,
        expires_at: DateTime<Utc>,
    ) -> WorkLedgerResult<OwnershipLease> {
        self.renew_ownership_lease_at(
            fence,
            predecessor_holder,
            successor_holder,
            expected_lease_generation,
            expires_at,
            Utc::now(),
        )
    }

    fn renew_ownership_lease_at(
        &self,
        fence: &OwnershipLeaseFence,
        predecessor_holder: &OwnershipLeaseHolder,
        successor_holder: &OwnershipLeaseHolder,
        expected_lease_generation: u64,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> WorkLedgerResult<OwnershipLease> {
        validate_fence(fence)?;
        validate_holder(predecessor_holder)?;
        validate_holder(successor_holder)?;
        validate_expiry(now, expires_at)?;
        let (_writer, mut connection) = self.lease_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (work_generation, _owner_generation) = validate_live_fence(&tx, fence)?;
        let current = latest_lease(&tx, &fence.ownership_id)?
            .ok_or_else(|| refused("ownership lease is absent"))?;
        if current.state == "active"
            && current.lease_generation == expected_lease_generation.saturating_add(1)
            && current.fence == *fence
            && current.holder == *successor_holder
            && current.transition_kind == "renewed"
            && current.expires_at == expires_at
            && predecessor_matches_holder(
                &tx,
                &current,
                expected_lease_generation,
                predecessor_holder,
            )?
        {
            return Ok(current);
        }
        require_exact_active(
            &current,
            fence,
            predecessor_holder,
            expected_lease_generation,
        )?;
        if predecessor_holder == successor_holder {
            return Err(refused(
                "ownership renewal must rotate protected holder material",
            ));
        }
        if current.expires_at <= now {
            return Err(refused("expired ownership lease cannot be renewed"));
        }
        require_no_live_prepared_custody_rebind(&tx, &current.lease_id, now)?;
        let generation = expected_lease_generation
            .checked_add(1)
            .ok_or_else(|| refused("ownership lease generation exhausted"))?;
        let proof_digest = digest(
            format!(
                "shipyard-ownership-renew-v1\n{}\n{}\n{}",
                current.lease_id,
                current.proof_digest,
                expires_at.to_rfc3339()
            )
            .as_bytes(),
        );
        supersede(&tx, &current.lease_id, now)?;
        let renewed = build_lease(
            fence,
            successor_holder,
            work_generation,
            current.owner_generation,
            generation,
            Some(current.lease_id),
            "renewed",
            proof_digest,
            now,
            expires_at,
        )?;
        insert_lease(&tx, &renewed)?;
        tx.commit()?;
        Ok(renewed)
    }

    pub(crate) fn release_ownership_lease(
        &self,
        fence: &OwnershipLeaseFence,
        holder: &OwnershipLeaseHolder,
        expected_lease_generation: u64,
    ) -> WorkLedgerResult<String> {
        self.release_ownership_lease_at(fence, holder, expected_lease_generation, Utc::now())
    }

    fn release_ownership_lease_at(
        &self,
        fence: &OwnershipLeaseFence,
        holder: &OwnershipLeaseHolder,
        expected_lease_generation: u64,
        now: DateTime<Utc>,
    ) -> WorkLedgerResult<String> {
        validate_fence(fence)?;
        validate_holder(holder)?;
        let (_writer, mut connection) = self.lease_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let release_digest = release_ownership_lease_in_transaction(
            &tx,
            fence,
            holder,
            expected_lease_generation,
            now,
        )?;
        tx.commit()?;
        Ok(release_digest)
    }

    pub(crate) fn adopt_ownership(
        &self,
        fence: &OwnershipLeaseFence,
        successor: &OwnershipLeaseHolder,
        expected_lease_generation: u64,
        expires_at: DateTime<Utc>,
        proof: OwnershipAdoptionProof,
    ) -> WorkLedgerResult<OwnershipAdoptionResult> {
        self.adopt_ownership_at(
            fence,
            successor,
            expected_lease_generation,
            expires_at,
            proof,
            Utc::now(),
        )
    }

    fn adopt_ownership_at(
        &self,
        fence: &OwnershipLeaseFence,
        successor: &OwnershipLeaseHolder,
        expected_lease_generation: u64,
        expires_at: DateTime<Utc>,
        proof: OwnershipAdoptionProof,
        now: DateTime<Utc>,
    ) -> WorkLedgerResult<OwnershipAdoptionResult> {
        validate_fence(fence)?;
        validate_holder(successor)?;
        validate_expiry(now, expires_at)?;
        let (_writer, mut connection) = self.lease_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (work_generation, owner_generation) = validate_live_fence(&tx, fence)?;
        let current = latest_lease(&tx, &fence.ownership_id)?
            .ok_or_else(|| refused("ownership lease is absent"))?;
        if current.fence != *fence {
            return Err(refused("ownership adoption fence is stale or ambiguous"));
        }
        if current.owner_generation != owner_generation {
            return Err(refused(
                "ownership adoption generation is not authoritative",
            ));
        }
        if current.state == "active"
            && current.lease_generation == expected_lease_generation
            && current.holder == *successor
            && current.expires_at > now
        {
            return Ok(OwnershipAdoptionResult::Attached(current));
        }
        if current.state == "active"
            && current.lease_generation == expected_lease_generation.saturating_add(1)
            && current.holder == *successor
            && current.expires_at == expires_at
            && current.expires_at > now
            && predecessor_generation_is(&tx, &current, expected_lease_generation)?
        {
            return Ok(OwnershipAdoptionResult::SuccessorCreated(current));
        }
        if current.lease_generation != expected_lease_generation {
            return Err(refused("ownership adoption lease generation is stale"));
        }
        let (transition_kind, proof_digest) = adoption_proof(&current, proof, now)?;
        if current.holder == *successor {
            return Err(refused(
                "successor proof cannot replace the same holder incarnation",
            ));
        }
        if current.state == "active" {
            require_no_live_prepared_custody_rebind(&tx, &current.lease_id, now)?;
            supersede(&tx, &current.lease_id, now)?;
        }
        let generation = current
            .lease_generation
            .checked_add(1)
            .ok_or_else(|| refused("ownership lease generation exhausted"))?;
        let successor_owner_generation = current
            .owner_generation
            .checked_add(1)
            .ok_or_else(|| refused("ownership owner generation exhausted"))?;
        let successor_lease = build_lease(
            fence,
            successor,
            work_generation,
            successor_owner_generation,
            generation,
            Some(current.lease_id),
            transition_kind,
            proof_digest,
            now,
            expires_at,
        )?;
        insert_lease(&tx, &successor_lease)?;
        let work_changed = tx.execute(
            "UPDATE work_items SET owner_generation = ?2, updated_at = ?3
              WHERE id = ?1 AND phase = 'agent_owned_repair' AND owner_generation = ?4",
            params![
                fence.work_item_id,
                successor_owner_generation,
                now.to_rfc3339(),
                owner_generation,
            ],
        )?;
        let ownership_changed = tx.execute(
            "UPDATE agent_ownership SET owner_generation = ?2, updated_at = ?3
              WHERE ownership_id = ?1 AND state = 'acknowledged' AND owner_generation = ?4",
            params![
                fence.ownership_id,
                successor_owner_generation,
                now.to_rfc3339(),
                owner_generation,
            ],
        )?;
        if work_changed != 1 || ownership_changed != 1 {
            return Err(refused(
                "ownership adoption lost its authoritative generation race",
            ));
        }
        tx.commit()?;
        Ok(OwnershipAdoptionResult::SuccessorCreated(successor_lease))
    }

    fn lease_write_connection(
        &self,
    ) -> WorkLedgerResult<(
        Option<crate::writer_domain_lease::ProductionWriterDomainLease>,
        Connection,
    )> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| refused("database has no parent"))?;
        let writer = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        Ok((writer, connection))
    }
}

fn adoption_proof(
    current: &OwnershipLease,
    proof: OwnershipAdoptionProof,
    now: DateTime<Utc>,
) -> WorkLedgerResult<(&'static str, String)> {
    match proof {
        OwnershipAdoptionProof::Expired {
            expected_expires_at,
        } => {
            if current.state != "active"
                || current.expires_at != expected_expires_at
                || current.expires_at > now
            {
                return Err(refused(
                    "ownership lease expiry is not authoritatively established",
                ));
            }
            Ok((
                "expired",
                digest(
                    format!(
                        "shipyard-ownership-expired-v1\n{}\n{}",
                        current.lease_id,
                        expected_expires_at.to_rfc3339()
                    )
                    .as_bytes(),
                ),
            ))
        }
        OwnershipAdoptionProof::ExplicitRelease { release_digest } => {
            validate_digest("ownership release receipt", &release_digest)?;
            if current.state != "released"
                || current.release_digest.as_deref() != Some(&release_digest)
            {
                return Err(refused(
                    "ownership release receipt does not match durable custody",
                ));
            }
            Ok(("released", release_digest))
        }
    }
}

fn validate_fence(fence: &OwnershipLeaseFence) -> WorkLedgerResult<()> {
    validate_opaque_ref("agent ownership", &fence.ownership_id, "ao")?;
    validate_opaque_ref("ownership work item", &fence.work_item_id, "wi")?;
    validate_workstream_handle(&fence.workstream_handle)?;
    if fence.repository_provider.is_empty()
        || fence.repository_provider.len() > 64
        || fence.repository_id.is_empty()
        || fence.repository_id.len() > 512
        || !is_canonical_repo_slug(&fence.repository)
        || fence.pull_request == 0
        || !is_exact_sha(&fence.exact_head)
        || !is_lower_uuid(&fence.root_uuid)
    {
        return Err(refused("ownership lease fence is incomplete or malformed"));
    }
    Ok(())
}

fn validate_holder(holder: &OwnershipLeaseHolder) -> WorkLedgerResult<()> {
    validate_opaque_ref("ownership holder", &holder.holder_ref, "owner")?;
    validate_opaque_ref(
        "ownership holder incarnation",
        &holder.incarnation_ref,
        "incarnation",
    )?;
    validate_digest("ownership holder credential", &holder.credential_digest)
}

fn validate_holder_material(
    bytes: &[u8],
    ownership_id: &str,
    lease_generation: u64,
) -> WorkLedgerResult<OwnershipHolderMaterial> {
    let material: OwnershipHolderMaterial = serde_json::from_slice(bytes)
        .map_err(|_| refused("ownership holder material is malformed"))?;
    if material.schema_version != 1
        || material.ownership_id != ownership_id
        || material.lease_generation != lease_generation
        || material.credential.len() != 64
        || material
            .credential
            .bytes()
            .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
    {
        return Err(refused("ownership holder material is not exact"));
    }
    validate_holder(&material.holder())?;
    Ok(material)
}

pub(super) fn holder_from_material(
    bytes: &[u8],
    ownership_id: &str,
) -> WorkLedgerResult<(u64, OwnershipLeaseHolder)> {
    let parsed: OwnershipHolderMaterial = serde_json::from_slice(bytes)
        .map_err(|_| refused("ownership holder material is malformed"))?;
    let material = validate_holder_material(bytes, ownership_id, parsed.lease_generation)?;
    Ok((material.lease_generation, material.holder()))
}

pub(super) fn ownership_lease_fence_from_connection(
    connection: &Connection,
    ownership_id: &str,
) -> WorkLedgerResult<OwnershipLeaseFence> {
    validate_opaque_ref("agent ownership", ownership_id, "ao")?;
    connection
        .query_row(
            "SELECT ownership.work_item_id, binding.repository_provider,
                    binding.repository_id, binding.repository, work.pr,
                    binding.exact_head, binding.workstream_handle, root.root_uuid
               FROM agent_ownership ownership
               JOIN work_items work ON work.id = ownership.work_item_id
               JOIN workstream_projection_bindings binding ON binding.work_item_id = work.id
               JOIN ownership_roots root ON root.work_item_id = work.id
              WHERE ownership.ownership_id = ?1",
            [ownership_id],
            |row| {
                Ok(OwnershipLeaseFence {
                    ownership_id: ownership_id.to_owned(),
                    work_item_id: row.get(0)?,
                    repository_provider: row.get(1)?,
                    repository_id: row.get(2)?,
                    repository: row.get(3)?,
                    pull_request: row.get(4)?,
                    exact_head: row.get(5)?,
                    workstream_handle: row.get(6)?,
                    root_uuid: row.get(7)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| refused("ownership lease fence is unavailable"))
}

fn validate_expiry(now: DateTime<Utc>, expires_at: DateTime<Utc>) -> WorkLedgerResult<()> {
    let duration = expires_at.signed_duration_since(now);
    if duration <= Duration::zero() || duration > MAX_LEASE_DURATION {
        return Err(refused(
            "ownership lease duration must be positive and at most five minutes",
        ));
    }
    Ok(())
}

fn validate_live_fence(
    tx: &Transaction<'_>,
    fence: &OwnershipLeaseFence,
) -> WorkLedgerResult<(u64, u64)> {
    let observed: Option<(u64, u64)> = tx
        .query_row(
            "SELECT ownership.work_generation, ownership.owner_generation
               FROM agent_ownership ownership
               JOIN work_items work ON work.id = ownership.work_item_id
               JOIN workstream_projection_bindings binding ON binding.work_item_id = work.id
               JOIN ownership_roots root ON root.work_item_id = work.id
              WHERE ownership.ownership_id = ?1 AND ownership.work_item_id = ?2
                AND ownership.state = 'acknowledged'
                AND work.phase = 'agent_owned_repair'
                AND work.work_generation = ownership.work_generation + 1
                AND work.owner_generation = ownership.owner_generation
                AND binding.repository_provider = ?3 AND binding.repository_id = ?4
                AND binding.repository = ?5 AND work.pr = ?6
                AND binding.exact_head = ?7 AND work.head_sha = ?7
                AND binding.workstream_handle = ?8 AND root.root_uuid = ?9",
            params![
                fence.ownership_id,
                fence.work_item_id,
                fence.repository_provider,
                fence.repository_id,
                fence.repository,
                fence.pull_request,
                fence.exact_head,
                fence.workstream_handle,
                fence.root_uuid,
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    observed.ok_or_else(|| {
        refused("ownership lease does not match live acknowledged repository custody")
    })
}

fn require_exact_active(
    current: &OwnershipLease,
    fence: &OwnershipLeaseFence,
    holder: &OwnershipLeaseHolder,
    generation: u64,
) -> WorkLedgerResult<()> {
    if current.state != "active"
        || current.fence != *fence
        || current.holder != *holder
        || current.lease_generation != generation
    {
        return Err(refused("ownership lease holder or generation changed"));
    }
    Ok(())
}

fn predecessor_generation_is(
    tx: &Transaction<'_>,
    lease: &OwnershipLease,
    expected_generation: u64,
) -> WorkLedgerResult<bool> {
    let Some(predecessor) = lease.predecessor_lease_id.as_deref() else {
        return Ok(false);
    };
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ownership_leases
          WHERE lease_id = ?1 AND ownership_id = ?2 AND lease_generation = ?3)",
        params![predecessor, lease.fence.ownership_id, expected_generation],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn predecessor_matches_holder(
    tx: &Transaction<'_>,
    lease: &OwnershipLease,
    expected_generation: u64,
    holder: &OwnershipLeaseHolder,
) -> WorkLedgerResult<bool> {
    let Some(predecessor) = lease.predecessor_lease_id.as_deref() else {
        return Ok(false);
    };
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ownership_leases
          WHERE lease_id = ?1 AND ownership_id = ?2 AND lease_generation = ?3
            AND holder_ref = ?4 AND holder_incarnation_ref = ?5
            AND holder_credential_digest = ?6)",
        params![
            predecessor,
            lease.fence.ownership_id,
            expected_generation,
            holder.holder_ref,
            holder.incarnation_ref,
            holder.credential_digest,
        ],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

pub(super) fn establish_ownership_lease_in_transaction(
    tx: &Transaction<'_>,
    fence: &OwnershipLeaseFence,
    holder: &OwnershipLeaseHolder,
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> WorkLedgerResult<OwnershipLease> {
    validate_fence(fence)?;
    validate_holder(holder)?;
    validate_expiry(now, expires_at)?;
    let (work_generation, owner_generation) = validate_live_fence(tx, fence)?;
    if let Some(existing) = latest_lease(tx, &fence.ownership_id)? {
        return if existing.state == "active"
            && existing.fence == *fence
            && existing.holder == *holder
            && existing.lease_generation == 1
            && existing.expires_at == expires_at
        {
            Ok(existing)
        } else {
            Err(refused("agent ownership already has a different lease"))
        };
    }
    let proof_digest = digest(
        format!(
            "shipyard-ownership-establish-v1\n{}\n{}\n{}",
            fence_digest(fence)?,
            holder.holder_ref,
            holder.incarnation_ref
        )
        .as_bytes(),
    );
    let lease = build_lease(
        fence,
        holder,
        work_generation,
        owner_generation,
        1,
        None,
        "established",
        proof_digest,
        now,
        expires_at,
    )?;
    insert_lease(tx, &lease)?;
    Ok(lease)
}

pub(super) fn release_ownership_lease_in_transaction(
    tx: &Transaction<'_>,
    fence: &OwnershipLeaseFence,
    holder: &OwnershipLeaseHolder,
    expected_lease_generation: u64,
    now: DateTime<Utc>,
) -> WorkLedgerResult<String> {
    validate_fence(fence)?;
    validate_holder(holder)?;
    validate_live_fence(tx, fence)?;
    let current = latest_lease(tx, &fence.ownership_id)?
        .ok_or_else(|| refused("ownership lease is absent"))?;
    if current.state == "released"
        && current.fence == *fence
        && current.holder == *holder
        && current.lease_generation == expected_lease_generation
    {
        return current
            .release_digest
            .ok_or_else(|| refused("released ownership lease lacks its receipt"));
    }
    require_exact_active(&current, fence, holder, expected_lease_generation)?;
    require_no_live_prepared_custody_rebind(tx, &current.lease_id, now)?;
    let release_digest = digest(
        format!(
            "shipyard-ownership-release-v1\n{}\n{}\n{}",
            current.lease_id,
            current.proof_digest,
            now.to_rfc3339()
        )
        .as_bytes(),
    );
    if tx.execute(
        "UPDATE ownership_leases SET state = 'released', release_digest = ?2, updated_at = ?3
          WHERE lease_id = ?1 AND state = 'active'",
        params![current.lease_id, release_digest, now.to_rfc3339()],
    )? != 1
    {
        return Err(refused("ownership lease changed during explicit release"));
    }
    Ok(release_digest)
}

#[allow(clippy::too_many_arguments)]
fn build_lease(
    fence: &OwnershipLeaseFence,
    holder: &OwnershipLeaseHolder,
    work_generation: u64,
    owner_generation: u64,
    lease_generation: u64,
    predecessor_lease_id: Option<String>,
    transition_kind: &str,
    proof_digest: String,
    acquired_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> WorkLedgerResult<OwnershipLease> {
    validate_digest("ownership lease proof", &proof_digest)?;
    let lease_id = opaque_ref(
        "ol",
        &format!(
            "shipyard-ownership-lease-v1\n{}\n{}\n{}\n{}\n{}",
            fence.ownership_id,
            lease_generation,
            holder.holder_ref,
            holder.incarnation_ref,
            holder.credential_digest
        ),
    );
    Ok(OwnershipLease {
        lease_id,
        fence: fence.clone(),
        work_generation,
        owner_generation,
        lease_generation,
        holder: holder.clone(),
        state: "active".to_owned(),
        predecessor_lease_id,
        transition_kind: transition_kind.to_owned(),
        proof_digest,
        release_digest: None,
        acquired_at,
        expires_at,
    })
}

fn insert_lease(tx: &Transaction<'_>, lease: &OwnershipLease) -> WorkLedgerResult<()> {
    tx.execute(
        "INSERT INTO ownership_leases
         (lease_id, ownership_id, work_item_id, work_generation, owner_generation,
          lease_generation, holder_ref, holder_incarnation_ref, holder_credential_digest,
          repository_provider, repository_id, repository, pull_request, exact_head, workstream_handle,
          root_uuid, state, predecessor_lease_id, transition_kind, proof_digest,
          acquired_at, expires_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, 'active', ?17, ?18, ?19, ?20, ?21, ?20)",
        params![
            lease.lease_id,
            lease.fence.ownership_id,
            lease.fence.work_item_id,
            lease.work_generation,
            lease.owner_generation,
            lease.lease_generation,
            lease.holder.holder_ref,
            lease.holder.incarnation_ref,
            lease.holder.credential_digest,
            lease.fence.repository_provider,
            lease.fence.repository_id,
            lease.fence.repository,
            lease.fence.pull_request,
            lease.fence.exact_head,
            lease.fence.workstream_handle,
            lease.fence.root_uuid,
            lease.predecessor_lease_id,
            lease.transition_kind,
            lease.proof_digest,
            lease.acquired_at.to_rfc3339(),
            lease.expires_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn supersede(tx: &Transaction<'_>, lease_id: &str, now: DateTime<Utc>) -> WorkLedgerResult<()> {
    if tx.execute(
        "UPDATE ownership_leases SET state = 'superseded', updated_at = ?2
          WHERE lease_id = ?1 AND state = 'active'",
        params![lease_id, now.to_rfc3339()],
    )? != 1
    {
        return Err(refused("ownership lease changed during successor adoption"));
    }
    Ok(())
}

fn latest_lease(
    tx: &Transaction<'_>,
    ownership_id: &str,
) -> WorkLedgerResult<Option<OwnershipLease>> {
    tx.query_row(
        "SELECT lease_id, work_item_id, work_generation, owner_generation, lease_generation,
                holder_ref, holder_incarnation_ref, holder_credential_digest,
                repository_provider, repository_id,
                repository, pull_request, exact_head, workstream_handle, root_uuid,
                state, predecessor_lease_id, transition_kind, proof_digest, release_digest,
                acquired_at, expires_at
           FROM ownership_leases WHERE ownership_id = ?1
          ORDER BY lease_generation DESC LIMIT 1",
        [ownership_id],
        |row| {
            let acquired_at: String = row.get(20)?;
            let expires_at: String = row.get(21)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, u64>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, String>(14)?,
                row.get::<_, String>(15)?,
                row.get::<_, Option<String>>(16)?,
                row.get::<_, String>(17)?,
                row.get::<_, String>(18)?,
                row.get::<_, Option<String>>(19)?,
                acquired_at,
                expires_at,
            ))
        },
    )
    .optional()?
    .map(|row| {
        Ok(OwnershipLease {
            lease_id: row.0,
            fence: OwnershipLeaseFence {
                ownership_id: ownership_id.to_owned(),
                work_item_id: row.1,
                repository_provider: row.8,
                repository_id: row.9,
                repository: row.10,
                pull_request: row.11,
                exact_head: row.12,
                workstream_handle: row.13,
                root_uuid: row.14,
            },
            work_generation: row.2,
            owner_generation: row.3,
            lease_generation: row.4,
            holder: OwnershipLeaseHolder {
                holder_ref: row.5,
                incarnation_ref: row.6,
                credential_digest: row.7,
            },
            state: row.15,
            predecessor_lease_id: row.16,
            transition_kind: row.17,
            proof_digest: row.18,
            release_digest: row.19,
            acquired_at: row
                .20
                .parse()
                .map_err(|_| refused("stored ownership lease timestamp is invalid"))?,
            expires_at: row
                .21
                .parse()
                .map_err(|_| refused("stored ownership lease timestamp is invalid"))?,
        })
    })
    .transpose()
}

pub(super) fn validate_active_successor_lease_for_custody(
    tx: &Transaction<'_>,
    lease: &OwnershipLease,
    work_item_id: &str,
    work_generation: u64,
    owner_generation: u64,
    workstream_handle: &str,
) -> WorkLedgerResult<()> {
    let persisted = latest_lease(tx, &lease.fence.ownership_id)?
        .ok_or_else(|| refused("custody successor ownership lease is absent"))?;
    let (live_work_generation, live_owner_generation) = validate_live_fence(tx, &lease.fence)?;
    if persisted != *lease
        || lease.state != "active"
        || lease.expires_at <= Utc::now()
        || lease.predecessor_lease_id.is_none()
        || lease.fence.work_item_id != work_item_id
        || lease.work_generation != work_generation
        || lease.work_generation != live_work_generation
        || lease.owner_generation != live_owner_generation
        || lease.owner_generation <= owner_generation
        || lease.fence.workstream_handle != workstream_handle
    {
        return Err(refused(
            "custody successor is not the exact active adopted ownership lease",
        ));
    }
    Ok(())
}

pub(super) fn validate_live_custody_lease_tuple(
    tx: &Transaction<'_>,
    rebind: &super::CustodySuccessorRebind,
    now: DateTime<Utc>,
) -> WorkLedgerResult<()> {
    let exact: bool = tx.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM ownership_leases lease
           JOIN ownership_roots root ON root.root_uuid = lease.root_uuid
           JOIN workstream_projection_bindings binding ON binding.work_item_id = lease.work_item_id
          WHERE lease.lease_id = ?1 AND lease.lease_generation = ?2
            AND lease.state = 'active' AND lease.expires_at = ?3 AND lease.expires_at > ?4
            AND root.root_uuid = ?5 AND lease.repository_provider = ?6
            AND lease.repository_id = ?7 AND lease.repository = ?8
            AND lease.pull_request = ?9 AND lease.exact_head = ?10
            AND lease.workstream_handle = ?11
            AND lease.proof_digest = ?12
            AND lease.holder_ref = ?13
            AND lease.holder_incarnation_ref = ?14
            AND binding.repository_provider = lease.repository_provider
            AND binding.repository_id = lease.repository_id
            AND binding.repository = lease.repository
            AND binding.exact_head = lease.exact_head
            AND binding.workstream_handle = lease.workstream_handle)",
        params![
            rebind.ownership_lease_id,
            rebind.ownership_lease_generation,
            rebind.ownership_lease_expires_at.to_rfc3339(),
            now.to_rfc3339(),
            rebind.ownership_root_uuid,
            rebind.repository_provider,
            rebind.repository_id,
            rebind.repository,
            rebind.pull_request,
            rebind.exact_head,
            rebind.workstream_handle,
            rebind.successor_proof_digest,
            rebind.successor_holder_ref,
            rebind.successor_session_incarnation_ref,
        ],
        |row| row.get(0),
    )?;
    if !exact {
        return Err(refused(
            "custody successor lease tuple is stale or superseded",
        ));
    }
    Ok(())
}

fn require_no_live_prepared_custody_rebind(
    tx: &Transaction<'_>,
    lease_id: &str,
    now: DateTime<Utc>,
) -> WorkLedgerResult<()> {
    let live: bool = tx.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM custody_successor_rebinds
            WHERE side = 'sender'
              AND (state = 'acknowledged'
                   OR (state = 'prepared'
                       AND json_extract(CAST(rebind_json AS TEXT), '$.ownership_lease_expires_at') > ?2))
              AND json_extract(CAST(rebind_json AS TEXT), '$.ownership_lease_id') = ?1
         )",
        params![lease_id, now.to_rfc3339()],
        |row| row.get(0),
    )?;
    if live {
        return Err(refused(
            "ownership lease is pinned by a live prepared custody successor rebind",
        ));
    }
    Ok(())
}

fn fence_digest(fence: &OwnershipLeaseFence) -> WorkLedgerResult<String> {
    Ok(digest(
        serde_json::to_vec(fence)
            .map_err(|_| refused("ownership lease fence cannot be serialized"))?
            .as_slice(),
    ))
}

fn is_exact_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_lower_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

fn refused(reason: &str) -> WorkLedgerError {
    WorkLedgerError::Refused(reason.to_owned())
}

pub(super) fn validate_persisted_ownership_leases(connection: &Connection) -> WorkLedgerResult<()> {
    let invalid_material: i64 = connection.query_row(
        "SELECT COUNT(*) FROM ownership_holder_materials material
           LEFT JOIN protected_objects object ON object.object_ref = material.object_ref
          WHERE object.object_ref IS NULL OR object.work_item_id != material.work_item_id
             OR object.kind != 'agent_receipt' OR object.profile_ref IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if invalid_material != 0 {
        return Err(refused(
            "ownership holder material is not bound to exact protected storage",
        ));
    }
    let invalid: i64 = connection.query_row(
        "SELECT COUNT(*) FROM ownership_leases lease
           LEFT JOIN agent_ownership ownership ON ownership.ownership_id = lease.ownership_id
           LEFT JOIN workstream_projection_bindings binding
             ON binding.work_item_id = lease.work_item_id
           LEFT JOIN ownership_roots root ON root.work_item_id = lease.work_item_id
           LEFT JOIN work_items work ON work.id = lease.work_item_id
          WHERE ownership.ownership_id IS NULL OR binding.work_item_id IS NULL
             OR root.work_item_id IS NULL OR root.root_uuid != lease.root_uuid
             OR work.id IS NULL OR ownership.work_item_id != lease.work_item_id
             OR ownership.work_generation != lease.work_generation
             OR ownership.owner_generation < lease.owner_generation
             OR (lease.state = 'active'
                 AND ownership.owner_generation != lease.owner_generation)
             OR binding.repository_provider != lease.repository_provider
             OR binding.repository_id != lease.repository_id
             OR binding.repository != lease.repository
             OR binding.workstream_handle != lease.workstream_handle
             OR work.pr != lease.pull_request
             OR (lease.state = 'active' AND (
                    ownership.state != 'acknowledged'
                    OR work.phase != 'agent_owned_repair'
                    OR binding.exact_head != lease.exact_head
                    OR work.head_sha != lease.exact_head
             ))
             OR (lease.predecessor_lease_id IS NOT NULL AND NOT EXISTS (
                 SELECT 1 FROM ownership_leases predecessor
                  WHERE predecessor.lease_id = lease.predecessor_lease_id
                    AND predecessor.ownership_id = lease.ownership_id
                    AND predecessor.lease_generation + 1 = lease.lease_generation
                    AND predecessor.work_item_id = lease.work_item_id
                    AND predecessor.work_generation = lease.work_generation
                    AND ((lease.transition_kind = 'renewed'
                          AND predecessor.owner_generation = lease.owner_generation)
                      OR (lease.transition_kind IN ('expired', 'released')
                          AND predecessor.owner_generation + 1 = lease.owner_generation))
                    AND predecessor.repository_provider = lease.repository_provider
                    AND predecessor.repository_id = lease.repository_id
                    AND predecessor.repository = lease.repository
                    AND predecessor.pull_request = lease.pull_request
                    AND predecessor.exact_head = lease.exact_head
                    AND predecessor.workstream_handle = lease.workstream_handle
                    AND predecessor.root_uuid = lease.root_uuid
                    AND ((lease.transition_kind = 'renewed'
                          AND predecessor.state = 'superseded'
                          AND predecessor.holder_ref = lease.holder_ref
                          AND predecessor.holder_incarnation_ref != lease.holder_incarnation_ref
                          AND predecessor.holder_credential_digest != lease.holder_credential_digest)
                      OR (lease.transition_kind = 'expired'
                          AND predecessor.state = 'superseded'
                          AND (predecessor.holder_ref != lease.holder_ref
                               OR predecessor.holder_incarnation_ref != lease.holder_incarnation_ref))
                      OR (lease.transition_kind = 'released'
                          AND predecessor.state = 'released'
                          AND predecessor.release_digest = lease.proof_digest
                          AND (predecessor.holder_ref != lease.holder_ref
                               OR predecessor.holder_incarnation_ref != lease.holder_incarnation_ref)))
             ))",
        [],
        |row| row.get(0),
    )?;
    if invalid != 0 {
        return Err(refused(
            "ownership lease history is not bound to exact ledger custody",
        ));
    }
    Ok(())
}

pub(super) fn require_no_active_ownership_lease(
    tx: &Transaction<'_>,
    ownership_id: &str,
) -> WorkLedgerResult<()> {
    let active: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ownership_leases
          WHERE ownership_id = ?1 AND state = 'active')",
        [ownership_id],
        |row| row.get(0),
    )?;
    if active {
        return Err(refused(
            "agent ownership return requires its stewardship lease to be explicitly released",
        ));
    }
    Ok(())
}

pub(super) fn require_exact_holder_lease(
    tx: &Transaction<'_>,
    ownership_id: &str,
    holder: &OwnershipLeaseHolder,
) -> WorkLedgerResult<()> {
    let lease = latest_lease(tx, ownership_id)?
        .ok_or_else(|| refused("acknowledged ownership lease is missing"))?;
    if lease.state != "active" || lease.holder != *holder || lease.expires_at <= Utc::now() {
        return Err(refused("acknowledged ownership holder material is stale"));
    }
    Ok(())
}

#[cfg(test)]
impl WorkLedger {
    pub(super) fn establish_ownership_lease_at_for_test(
        &self,
        fence: &OwnershipLeaseFence,
        holder: &OwnershipLeaseHolder,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> WorkLedgerResult<OwnershipLease> {
        self.establish_ownership_lease_at(fence, holder, expires_at, now)
    }

    pub(super) fn renew_ownership_lease_at_for_test(
        &self,
        fence: &OwnershipLeaseFence,
        predecessor_holder: &OwnershipLeaseHolder,
        successor_holder: &OwnershipLeaseHolder,
        expected_lease_generation: u64,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> WorkLedgerResult<OwnershipLease> {
        self.renew_ownership_lease_at(
            fence,
            predecessor_holder,
            successor_holder,
            expected_lease_generation,
            expires_at,
            now,
        )
    }

    pub(super) fn adopt_ownership_at_for_test(
        &self,
        fence: &OwnershipLeaseFence,
        successor: &OwnershipLeaseHolder,
        expected_lease_generation: u64,
        expires_at: DateTime<Utc>,
        proof: OwnershipAdoptionProof,
        now: DateTime<Utc>,
    ) -> WorkLedgerResult<OwnershipAdoptionResult> {
        self.adopt_ownership_at(
            fence,
            successor,
            expected_lease_generation,
            expires_at,
            proof,
            now,
        )
    }
}
