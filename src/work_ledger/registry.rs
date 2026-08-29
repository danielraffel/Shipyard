//! Protected adapter and route registry.
#![allow(dead_code)] // Native registry writers activate after shadow cutover.

use super::{
    AdapterBindingRecord, OptionalExtension, RouteProvenanceRecord, Transaction,
    TransactionBehavior, Utc, WorkLedger, WorkLedgerError, WorkLedgerResult, configure_durable,
    digest, is_lower_hex, params, validate_opaque_ref, verify_supported_schema,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RouteRegistration {
    pub(super) route_ref: String,
    pub(super) work_id: String,
    pub(super) head_sha: String,
    pub(super) work_generation: u64,
    pub(super) owner_ref: String,
    pub(super) owner_generation: u64,
    pub(super) revision: u64,
    pub(super) origin_machine_ref: String,
    pub(super) provenance: RouteProvenanceRecord,
    pub(super) envelope_integrity: String,
}

#[allow(dead_code)] // Native route writers are activated after shadow cutover.
impl RouteRegistration {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        route_ref: String,
        work_id: String,
        head_sha: String,
        work_generation: u64,
        owner_ref: String,
        owner_generation: u64,
        revision: u64,
        origin_machine_ref: String,
        provenance: RouteProvenanceRecord,
    ) -> WorkLedgerResult<Self> {
        validate_opaque_ref("route_ref", &route_ref, "route")?;
        validate_opaque_ref("work_id", &work_id, "wi")?;
        validate_opaque_ref("owner_ref", &owner_ref, "owner")?;
        validate_opaque_ref("origin_machine_ref", &origin_machine_ref, "machine")?;
        if !is_lower_hex(&head_sha, 40) && !is_lower_hex(&head_sha, 64) {
            return Err(WorkLedgerError::Refused(
                "invalid route head SHA".to_owned(),
            ));
        }
        if work_generation == 0 || owner_generation == 0 || revision == 0 {
            return Err(WorkLedgerError::Refused(
                "route generations and revision must be positive".to_owned(),
            ));
        }
        provenance
            .validate()
            .map_err(|_| WorkLedgerError::Refused("route provenance is invalid".to_owned()))?;
        if provenance.launch_generation() != owner_generation {
            return Err(WorkLedgerError::Refused(
                "route launch generation does not match owner generation".to_owned(),
            ));
        }
        let mut registration = Self {
            route_ref,
            work_id,
            head_sha,
            work_generation,
            owner_ref,
            owner_generation,
            revision,
            origin_machine_ref,
            provenance,
            envelope_integrity: String::new(),
        };
        registration.envelope_integrity = registration.compute_envelope_integrity();
        Ok(registration)
    }

    pub(super) fn compute_envelope_integrity(&self) -> String {
        digest(
            format!(
                "shipyard-route-envelope-v1\0{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
                self.route_ref,
                self.work_id,
                self.head_sha,
                self.work_generation,
                self.owner_ref,
                self.owner_generation,
                self.revision,
                self.origin_machine_ref,
                self.provenance.integrity(),
            )
            .as_bytes(),
        )
    }
}

impl WorkLedger {
    pub(super) fn register_adapter(&self, adapter: &AdapterBindingRecord) -> WorkLedgerResult<()> {
        adapter.validate().map_err(|_| {
            WorkLedgerError::Refused("adapter registry identity is invalid".to_owned())
        })?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        let now = Utc::now().to_rfc3339();
        let changed = connection.execute(
            "INSERT OR IGNORE INTO adapter_registry
             (registry_ref, axis, name, generation, revision, implementation_digest,
              configuration_digest, capabilities_digest, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9, ?9)",
            params![
                adapter.registry_ref.as_str(),
                adapter.axis.as_str(),
                adapter.name,
                adapter.generation,
                adapter.revision,
                adapter.implementation_sha256.as_str(),
                adapter.configuration_sha256.as_str(),
                adapter.capabilities_sha256.as_str(),
                now,
            ],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "adapter registry identity already exists".to_owned(),
            ));
        }
        Ok(())
    }

    /// Register one complete route under exact work, owner, and revision fences.
    #[allow(dead_code)] // Native route writers are activated after shadow cutover.
    pub(super) fn register_route(&self, route: &RouteRegistration) -> WorkLedgerResult<()> {
        self.register_route_inner(route, false)
    }

    /// Stage the immutable route for the next generation of a managed native
    /// publication. Only the actionable producer may consume that generation.
    pub(super) fn register_staged_route(&self, route: &RouteRegistration) -> WorkLedgerResult<()> {
        self.register_route_inner(route, true)
    }

    fn register_route_inner(
        &self,
        route: &RouteRegistration,
        permit_managed_next_generation: bool,
    ) -> WorkLedgerResult<()> {
        route
            .provenance
            .validate()
            .map_err(|_| WorkLedgerError::Refused("route provenance is invalid".to_owned()))?;
        if route.compute_envelope_integrity() != route.envelope_integrity {
            return Err(WorkLedgerError::Refused(
                "route envelope integrity is invalid".to_owned(),
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
        let current: (String, u64, u64, Option<String>, String) = transaction.query_row(
            "SELECT head_sha, work_generation, owner_generation, owner_id, phase
             FROM work_items WHERE id = ?1",
            params![route.work_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let generation_matches = route.work_generation == current.1
            || (permit_managed_next_generation
                && current.4 == "managed"
                && current.1.checked_add(1) == Some(route.work_generation));
        if current.0 != route.head_sha
            || !generation_matches
            || current.2 != route.owner_generation
            || current.3.as_deref() != Some(route.owner_ref.as_str())
        {
            return Err(WorkLedgerError::Refused(
                "route does not match current work provenance".to_owned(),
            ));
        }
        if !registered_adapters_present(&transaction, &route.provenance)? {
            return Err(WorkLedgerError::Refused(
                "route references an absent, retired, or changed adapter registration".to_owned(),
            ));
        }
        let payload = serde_json::to_vec(&route.provenance).map_err(|_| {
            WorkLedgerError::Refused("route provenance cannot be serialized".to_owned())
        })?;
        let payload_digest = digest(&payload);
        let now = Utc::now().to_rfc3339();
        let changed = transaction.execute(
            "INSERT OR IGNORE INTO route_records
             (route_ref, work_item_id, head_sha, work_generation, owner_ref,
              owner_generation, revision, origin_machine_ref, terminal_kind,
              agent_kind, provider_kind, payload_json, payload_digest, integrity_hash,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, ?15, ?15)",
            params![
                route.route_ref,
                route.work_id,
                route.head_sha,
                route.work_generation,
                route.owner_ref,
                route.owner_generation,
                route.revision,
                route.origin_machine_ref,
                route.provenance.terminal_kind(),
                route.provenance.agent_kind(),
                route.provenance.provider_kind(),
                payload,
                payload_digest,
                route.envelope_integrity,
                now,
            ],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "route reference or revision already exists".to_owned(),
            ));
        }
        transaction.commit()?;
        Ok(())
    }
}

pub(super) fn validated_route_exists(
    transaction: &Transaction<'_>,
    route_ref: &str,
    work_id: &str,
    work_generation: u64,
    owner_generation: u64,
) -> WorkLedgerResult<bool> {
    type StoredRoute = (
        String,
        String,
        u64,
        String,
        u64,
        u64,
        String,
        Vec<u8>,
        String,
        String,
        String,
        String,
        String,
    );
    let stored: Option<StoredRoute> = transaction
        .query_row(
            "SELECT work_item_id, head_sha, work_generation, owner_ref, owner_generation,
                    revision, origin_machine_ref, payload_json, integrity_hash,
                    terminal_kind, agent_kind, provider_kind, payload_digest
             FROM route_records WHERE route_ref = ?1",
            [route_ref],
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
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                ))
            },
        )
        .optional()?;
    let Some((
        stored_work,
        head,
        stored_generation,
        owner,
        stored_owner_generation,
        revision,
        machine,
        payload,
        integrity,
        terminal_kind,
        agent_kind,
        provider_kind,
        payload_digest,
    )) = stored
    else {
        return Ok(false);
    };
    if stored_work != work_id
        || stored_generation != work_generation
        || stored_owner_generation != owner_generation
    {
        return Ok(false);
    }
    let provenance: RouteProvenanceRecord = serde_json::from_slice(&payload)
        .map_err(|_| WorkLedgerError::Refused("stored route payload is malformed".to_owned()))?;
    if digest(&payload) != payload_digest
        || provenance.terminal_kind() != terminal_kind
        || provenance.agent_kind() != agent_kind
        || provenance.provider_kind() != provider_kind
    {
        return Err(WorkLedgerError::Refused(
            "stored route metadata disagrees with its payload".to_owned(),
        ));
    }
    if !registered_adapters_present(transaction, &provenance)? {
        return Err(WorkLedgerError::Refused(
            "stored route adapter registration is absent, retired, or changed".to_owned(),
        ));
    }
    let registration = RouteRegistration::new(
        route_ref.to_owned(),
        stored_work,
        head,
        stored_generation,
        owner,
        stored_owner_generation,
        revision,
        machine,
        provenance,
    )?;
    if registration.envelope_integrity != integrity {
        return Err(WorkLedgerError::Refused(
            "stored route envelope integrity mismatch".to_owned(),
        ));
    }
    Ok(true)
}

/// Revalidate a route and return whether its protected launch-profile and
/// provider identities match the exact profile selected for this wake.
pub(super) fn validated_route_matches_launch(
    transaction: &Transaction<'_>,
    route_ref: &str,
    work_id: &str,
    work_generation: u64,
    owner_generation: u64,
    profile_ref: &str,
    provider_kind: &str,
) -> WorkLedgerResult<bool> {
    if !validated_route_exists(
        transaction,
        route_ref,
        work_id,
        work_generation,
        owner_generation,
    )? {
        return Ok(false);
    }
    let payload: Vec<u8> = transaction.query_row(
        "SELECT payload_json FROM route_records WHERE route_ref = ?1",
        [route_ref],
        |row| row.get(0),
    )?;
    let provenance: RouteProvenanceRecord = serde_json::from_slice(&payload)
        .map_err(|_| WorkLedgerError::Refused("stored route payload is malformed".to_owned()))?;
    Ok(
        provenance.launch_profile.profile_ref.as_str() == profile_ref
            && provenance.launch_profile.execution_provider_kind() == provider_kind
            && provenance.provider_kind() == provenance.launch_profile.provider_kind,
    )
}

fn registered_adapters_present(
    transaction: &Transaction<'_>,
    provenance: &RouteProvenanceRecord,
) -> WorkLedgerResult<bool> {
    for binding in provenance.adapter_bindings() {
        let exact: Option<bool> = transaction
            .query_row(
                "SELECT axis = ?2 AND name = ?3 AND generation = ?4 AND revision = ?5
                        AND implementation_digest = ?6 AND configuration_digest = ?7
                        AND capabilities_digest = ?8 AND state = 'active'
                 FROM adapter_registry WHERE registry_ref = ?1",
                params![
                    binding.registry_ref.as_str(),
                    binding.axis.as_str(),
                    binding.name,
                    binding.generation,
                    binding.revision,
                    binding.implementation_sha256.as_str(),
                    binding.configuration_sha256.as_str(),
                    binding.capabilities_sha256.as_str(),
                ],
                |row| row.get(0),
            )
            .optional()?;
        if exact != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}
