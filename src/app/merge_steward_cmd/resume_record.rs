use super::{
    CliFailure, ResumeAdapterV1, ResumeRecordPhase, ResumeRecordV1, ResumeRoutingDisposition,
    StewardLedger, TerminalHandoff, TerminalHandoffOutcome, TerminalHandoffPhase, Utc,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const MAX_RESUME_RECORDS: usize = 1_000;

/// Rebuild the inert resume-intent projection from authoritative terminal handoffs.
///
/// The caller publishes both maps in one crash-safe steward-ledger replacement.
/// Phase 1 deliberately has no transition that can enable dispatch.
pub(super) fn reconcile_resume_records(ledger: &mut StewardLedger) -> Result<bool, CliFailure> {
    let actionable = ledger
        .terminal_handoffs
        .values()
        .filter(|handoff| handoff.outcome == TerminalHandoffOutcome::ActionableFailure)
        .cloned()
        .collect::<Vec<_>>();
    let authoritative_keys = actionable
        .iter()
        .map(|handoff| handoff.dedupe_key.clone())
        .collect::<BTreeSet<_>>();
    let mut changed = false;

    for handoff in &actionable {
        let incoming = record_for(handoff)?;
        changed |= upsert(&mut ledger.resume_records, incoming)?;
    }

    let now = Utc::now().to_rfc3339();
    for record in ledger
        .resume_records
        .values_mut()
        .filter(|record| !authoritative_keys.contains(&record.terminal_handoff_key))
    {
        if record.phase != ResumeRecordPhase::Resolved {
            record.phase = ResumeRecordPhase::Resolved;
            record.updated_at.clone_from(&now);
            changed = true;
        }
    }

    changed |= make_capacity(&mut ledger.resume_records)?;
    Ok(changed)
}

fn record_for(handoff: &TerminalHandoff) -> Result<ResumeRecordV1, CliFailure> {
    let (routing_disposition, adapter) = route(handoff)?;
    let resume_id = resume_id(handoff, adapter.as_ref());
    let now = Utc::now().to_rfc3339();
    Ok(ResumeRecordV1 {
        schema_version: 1,
        resume_id,
        terminal_handoff_key: handoff.dedupe_key.clone(),
        repo: handoff.repo.clone(),
        base: handoff.base.clone(),
        pr_number: handoff.pr_number,
        head_sha: handoff.head_sha.clone(),
        owner_id: handoff.owner_id.clone(),
        ownership_generation: handoff.ownership_generation,
        routing_disposition,
        adapter,
        dispatch_enabled: false,
        phase: if handoff.phase == TerminalHandoffPhase::Resolved {
            ResumeRecordPhase::Resolved
        } else {
            ResumeRecordPhase::Recorded
        },
        created_at: now.clone(),
        updated_at: now,
    })
}

fn route(
    handoff: &TerminalHandoff,
) -> Result<(ResumeRoutingDisposition, Option<ResumeAdapterV1>), CliFailure> {
    let disposition = match handoff.owner_disposition.as_str() {
        "original_owner" => ResumeRoutingDisposition::OriginalOwner,
        "fresh_agent_only" => ResumeRoutingDisposition::FreshCheckpointRequired,
        "route_registry_required" => ResumeRoutingDisposition::RouteRegistryRequired,
        "unroutable_private_route" => ResumeRoutingDisposition::UnroutablePrivateRoute,
        _ => {
            return Err(CliFailure::new(
                1,
                "terminal handoff has unsupported resume routing disposition",
            ));
        }
    };
    if disposition != ResumeRoutingDisposition::OriginalOwner {
        return Ok((disposition, None));
    }

    let Some(route_id) = handoff.owner_route_id.as_deref() else {
        return Ok((ResumeRoutingDisposition::UnroutablePrivateRoute, None));
    };
    let native = match (
        handoff.owner_provider.as_deref(),
        handoff.resume_transport.as_deref(),
    ) {
        (Some("codex"), Some("codex_queue")) | (Some("claude"), Some("claude_resume")) => {
            Some(ResumeAdapterV1::ProviderNative {
                provider: handoff.owner_provider.clone().expect("matched provider"),
                transport: handoff.resume_transport.clone().expect("matched transport"),
                route_id: route_id.to_owned(),
            })
        }
        _ => None,
    };
    if native.is_some() {
        return Ok((disposition, native));
    }
    if handoff.owner_terminal_provenance == Some(super::TerminalProvenanceKind::Cmux) {
        return Ok((
            disposition,
            Some(ResumeAdapterV1::Cmux {
                route_id: route_id.to_owned(),
            }),
        ));
    }
    Ok((ResumeRoutingDisposition::UnroutablePrivateRoute, None))
}

fn resume_id(handoff: &TerminalHandoff, adapter: Option<&ResumeAdapterV1>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"shipyard-resume-record-v1\0");
    for field in [
        handoff.dedupe_key.as_str(),
        handoff.owner_id.as_deref().unwrap_or_default(),
        handoff.owner_route_id.as_deref().unwrap_or_default(),
    ] {
        digest.update(field.len().to_be_bytes());
        digest.update(field.as_bytes());
    }
    digest.update(
        handoff
            .ownership_generation
            .unwrap_or_default()
            .to_be_bytes(),
    );
    match adapter {
        Some(ResumeAdapterV1::ProviderNative {
            provider,
            transport,
            route_id,
        }) => {
            digest.update(b"provider_native\0");
            for field in [provider, transport, route_id] {
                digest.update(field.len().to_be_bytes());
                digest.update(field.as_bytes());
            }
        }
        Some(ResumeAdapterV1::Cmux { route_id }) => {
            digest.update(b"cmux\0");
            digest.update(route_id.len().to_be_bytes());
            digest.update(route_id.as_bytes());
        }
        None => digest.update(b"unroutable\0"),
    }
    format!("resume-{}", hex::encode(digest.finalize()))
}

fn upsert(
    records: &mut BTreeMap<String, ResumeRecordV1>,
    incoming: ResumeRecordV1,
) -> Result<bool, CliFailure> {
    let key = incoming.terminal_handoff_key.clone();
    let Some(existing) = records.get_mut(&key) else {
        records.insert(key, incoming);
        return Ok(true);
    };
    if existing.schema_version != 1 || existing.terminal_handoff_key != key {
        return Err(CliFailure::new(
            1,
            "resume record identity is corrupt; refusing to replace it",
        ));
    }
    if same_payload(existing, &incoming) {
        return Ok(false);
    }
    if existing.resume_id == incoming.resume_id {
        let created_at = existing.created_at.clone();
        let updated_at = incoming.updated_at.clone();
        *existing = incoming;
        existing.created_at = created_at;
        existing.updated_at = updated_at;
        return Ok(true);
    }
    // A different deterministic ID is legal only when the exact terminal
    // record has already fenced a newer owner generation or route.
    if incoming.ownership_generation < existing.ownership_generation {
        return Err(CliFailure::new(
            1,
            "resume record ownership generation regressed",
        ));
    }
    *existing = incoming;
    Ok(true)
}

fn same_payload(existing: &ResumeRecordV1, incoming: &ResumeRecordV1) -> bool {
    existing.schema_version == incoming.schema_version
        && existing.resume_id == incoming.resume_id
        && existing.terminal_handoff_key == incoming.terminal_handoff_key
        && existing.repo == incoming.repo
        && existing.base == incoming.base
        && existing.pr_number == incoming.pr_number
        && existing.head_sha == incoming.head_sha
        && existing.owner_id == incoming.owner_id
        && existing.ownership_generation == incoming.ownership_generation
        && existing.routing_disposition == incoming.routing_disposition
        && existing.adapter == incoming.adapter
        && existing.dispatch_enabled == incoming.dispatch_enabled
        && existing.phase == incoming.phase
}

fn make_capacity(records: &mut BTreeMap<String, ResumeRecordV1>) -> Result<bool, CliFailure> {
    let mut changed = false;
    while records.len() > MAX_RESUME_RECORDS {
        let oldest_resolved = records
            .iter()
            .filter(|(_, record)| record.phase == ResumeRecordPhase::Resolved)
            .min_by_key(|(_, record)| record.updated_at.as_str())
            .map(|(key, _)| key.clone());
        let Some(oldest_resolved) = oldest_resolved else {
            return Err(CliFailure::new(
                1,
                "resume ledger is full of unresolved obligations; refusing to discard one",
            ));
        };
        records.remove(&oldest_resolved);
        changed = true;
    }
    Ok(changed)
}

#[cfg(test)]
#[path = "resume_record/tests.rs"]
mod tests;
