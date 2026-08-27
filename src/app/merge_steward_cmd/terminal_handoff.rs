use super::{
    CliFailure, Path, StewardLedger, TerminalHandoff, TerminalHandoffOutcome, TerminalHandoffPhase,
    TerminalProvenanceKind, Utc,
    handoff::TerminalOwnerRoute,
    ledger::{load_existing_ledger, save_ledger},
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const MAX_TERMINAL_HANDOFFS: usize = 1_000;

fn key(
    repo: &str,
    base: &str,
    pr_number: u64,
    head_sha: &str,
    outcome: TerminalHandoffOutcome,
    trigger_fingerprint: Option<&str>,
) -> String {
    let suffix = match outcome {
        TerminalHandoffOutcome::SuccessContinuation => "success".to_owned(),
        TerminalHandoffOutcome::ActionableFailure => trigger_fingerprint
            .map_or_else(|| "failure".to_owned(), |value| format!("failure:{value}")),
    };
    format!(
        "{}@{}#{pr_number}:{}:{suffix}",
        repo.to_ascii_lowercase(),
        base,
        head_sha.to_ascii_lowercase()
    )
}

pub(super) fn persist_success_continuation(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    repo: &str,
    base: &str,
    pr_number: u64,
    head_sha: &str,
    owner: Option<TerminalOwnerRoute>,
) -> Result<(), CliFailure> {
    let dedupe_key = key(
        repo,
        base,
        pr_number,
        head_sha,
        TerminalHandoffOutcome::SuccessContinuation,
        None,
    );
    let owner_disposition = owner
        .as_ref()
        .map_or("route_registry_required", |owner| &owner.owner_disposition);
    persist(
        ledger_path,
        ledger,
        TerminalHandoff {
            dedupe_key,
            repo: repo.to_ascii_lowercase(),
            base: base.to_owned(),
            pr_number,
            head_sha: head_sha.to_ascii_lowercase(),
            outcome: TerminalHandoffOutcome::SuccessContinuation,
            trigger: "required_checks_terminal_success".to_owned(),
            next_action: "arm_merge_queue_exact_head".to_owned(),
            origin_machine: owner.as_ref().map(|owner| owner.origin_machine.clone()),
            owner_id: owner.as_ref().map(|owner| owner.owner_id.clone()),
            ownership_generation: owner.as_ref().map(|owner| owner.ownership_generation),
            owner_disposition: owner_disposition.to_owned(),
            owner_route_id: owner.as_ref().and_then(|owner| owner.route_id.clone()),
            owner_provider: owner.as_ref().and_then(|owner| owner.provider.clone()),
            resume_transport: owner
                .as_ref()
                .and_then(|owner| owner.resume_transport.clone()),
            owner_terminal_provenance: owner.and_then(|owner| owner.terminal_provenance),
            wake_consumer_available: false,
            failure_contexts: Vec::new(),
            phase: TerminalHandoffPhase::Pending,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        },
        true,
    )
}

#[allow(clippy::too_many_arguments)] // Exact repository/base/PR/head identity stays explicit at publication.
pub(super) fn persist_actionable_failure(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    repo: &str,
    base: &str,
    pr_number: u64,
    head_sha: &str,
    owner: Option<TerminalOwnerRoute>,
    mut failure_contexts: Vec<String>,
) -> Result<(), CliFailure> {
    failure_contexts.sort();
    failure_contexts.dedup();
    let trigger_fingerprint = failure_fingerprint(&failure_contexts);
    let dedupe_key = key(
        repo,
        base,
        pr_number,
        head_sha,
        TerminalHandoffOutcome::ActionableFailure,
        Some(&trigger_fingerprint),
    );
    let owner_disposition = owner
        .as_ref()
        .map_or("route_registry_required", |owner| &owner.owner_disposition);
    persist(
        ledger_path,
        ledger,
        TerminalHandoff {
            dedupe_key,
            repo: repo.to_ascii_lowercase(),
            base: base.to_owned(),
            pr_number,
            head_sha: head_sha.to_ascii_lowercase(),
            outcome: TerminalHandoffOutcome::ActionableFailure,
            trigger: "actionable_terminal_failure".to_owned(),
            next_action: "wake_exact_owner_for_causal_repair".to_owned(),
            origin_machine: owner.as_ref().map(|owner| owner.origin_machine.clone()),
            owner_id: owner.as_ref().map(|owner| owner.owner_id.clone()),
            ownership_generation: owner.as_ref().map(|owner| owner.ownership_generation),
            owner_disposition: owner_disposition.to_owned(),
            owner_route_id: owner.as_ref().and_then(|owner| owner.route_id.clone()),
            owner_provider: owner.as_ref().and_then(|owner| owner.provider.clone()),
            resume_transport: owner
                .as_ref()
                .and_then(|owner| owner.resume_transport.clone()),
            owner_terminal_provenance: owner.and_then(|owner| owner.terminal_provenance),
            wake_consumer_available: false,
            failure_contexts,
            phase: TerminalHandoffPhase::Recorded,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        },
        false,
    )
}

pub(super) fn mark_success_continuation_applied(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    repo: &str,
    base: &str,
    pr_number: u64,
    head_sha: &str,
) -> Result<(), CliFailure> {
    let key = key(
        repo,
        base,
        pr_number,
        head_sha,
        TerminalHandoffOutcome::SuccessContinuation,
        None,
    );
    if !ledger.terminal_handoffs.contains_key(&key) {
        return Err(CliFailure::new(
            1,
            "success continuation disappeared before completion",
        ));
    }
    update_handoffs(ledger_path, ledger, |handoffs| {
        let record = handoffs.get_mut(&key).expect("checked continuation key");
        record.phase = TerminalHandoffPhase::Applied;
        record.updated_at = Utc::now().to_rfc3339();
        true
    })?;
    Ok(())
}

pub(super) fn reconcile_queued_success_continuation(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    repo: &str,
    base: &str,
    pr_number: u64,
    head_sha: &str,
) -> Result<bool, CliFailure> {
    let key = key(
        repo,
        base,
        pr_number,
        head_sha,
        TerminalHandoffOutcome::SuccessContinuation,
        None,
    );
    update_handoffs(ledger_path, ledger, |handoffs| {
        let Some(record) = handoffs.get_mut(&key) else {
            return false;
        };
        if record.phase == TerminalHandoffPhase::Applied {
            return false;
        }
        record.phase = TerminalHandoffPhase::Applied;
        record.updated_at = Utc::now().to_rfc3339();
        true
    })
}

pub(super) fn resolve_success_continuation(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    repo: &str,
    base: &str,
    pr_number: u64,
    head_sha: &str,
) -> Result<(), CliFailure> {
    let key = key(
        repo,
        base,
        pr_number,
        head_sha,
        TerminalHandoffOutcome::SuccessContinuation,
        None,
    );
    update_handoffs(ledger_path, ledger, |handoffs| {
        let Some(record) = handoffs.get_mut(&key) else {
            return false;
        };
        record.phase = TerminalHandoffPhase::Resolved;
        record.updated_at = Utc::now().to_rfc3339();
        true
    })?;
    Ok(())
}

pub(super) fn resolve_terminal_handoffs(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    repo: &str,
    base: &str,
    pr_number: u64,
    head_sha: &str,
) -> Result<(), CliFailure> {
    update_handoffs(ledger_path, ledger, |handoffs| {
        let mut changed = false;
        for record in handoffs.values_mut().filter(|record| {
            record.repo.eq_ignore_ascii_case(repo)
                && record.base == base
                && record.pr_number == pr_number
                && record.head_sha.eq_ignore_ascii_case(head_sha)
                && !matches!(
                    record.phase,
                    TerminalHandoffPhase::Applied | TerminalHandoffPhase::Resolved
                )
        }) {
            record.phase = TerminalHandoffPhase::Resolved;
            record.updated_at = Utc::now().to_rfc3339();
            changed = true;
        }
        changed
    })?;
    Ok(())
}

pub(super) fn resolve_superseded_terminal_handoffs(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    repo: &str,
    base: &str,
    current_pr_heads: &BTreeMap<u64, String>,
) -> Result<(), CliFailure> {
    // `observation::pull_requests` is an exact base-scoped open-PR snapshot and
    // refuses a possibly partial 100-row result. Within that proven domain,
    // absence means the PR closed, merged, or changed base; a different head
    // proves exact-head supersession. Records for every other base are kept.
    update_handoffs(ledger_path, ledger, |handoffs| {
        let mut changed = false;
        for record in handoffs.values_mut().filter(|record| {
            record.repo.eq_ignore_ascii_case(repo)
                && record.base == base
                && !matches!(
                    record.phase,
                    TerminalHandoffPhase::Applied | TerminalHandoffPhase::Resolved
                )
                && current_pr_heads
                    .get(&record.pr_number)
                    .is_none_or(|head| !head.eq_ignore_ascii_case(&record.head_sha))
        }) {
            record.phase = TerminalHandoffPhase::Resolved;
            record.updated_at = Utc::now().to_rfc3339();
            changed = true;
        }
        changed
    })?;
    Ok(())
}

fn update_handoffs(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    update: impl FnOnce(&mut BTreeMap<String, TerminalHandoff>) -> bool,
) -> Result<bool, CliFailure> {
    let original = ledger.terminal_handoffs.clone();
    if !update(&mut ledger.terminal_handoffs) {
        return Ok(false);
    }
    if let Err(error) = save_ledger(ledger_path, ledger) {
        reconcile_after_ambiguous_save(ledger_path, ledger, original);
        return Err(error);
    }
    Ok(true)
}

fn persist(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    incoming: TerminalHandoff,
    rearm_applied: bool,
) -> Result<(), CliFailure> {
    let original = ledger.terminal_handoffs.clone();
    let changed = match persist_inner(ledger, incoming, rearm_applied) {
        Ok(changed) => changed,
        Err(error) => {
            ledger.terminal_handoffs = original;
            return Err(error);
        }
    };
    if !changed {
        return Ok(());
    }
    if let Err(error) = save_ledger(ledger_path, ledger) {
        reconcile_after_ambiguous_save(ledger_path, ledger, original);
        return Err(error);
    }
    Ok(())
}

fn persist_inner(
    ledger: &mut StewardLedger,
    incoming: TerminalHandoff,
    rearm_applied: bool,
) -> Result<bool, CliFailure> {
    let superseded_success = resolve_conflicting_success(ledger, &incoming);
    let key = incoming.dedupe_key.clone();
    if let Some(existing) = ledger.terminal_handoffs.get(&key) {
        let existing_phase = existing.phase;
        let rearm_actionable = incoming.outcome == TerminalHandoffOutcome::ActionableFailure
            && existing_phase == TerminalHandoffPhase::Resolved;
        let route_can_resolve = (existing.owner_disposition == "route_registry_required"
            && incoming.owner_disposition != "route_registry_required")
            || (existing.owner_disposition == "unroutable_private_route"
                && incoming.owner_disposition == "original_owner"
                && incoming.ownership_generation >= existing.ownership_generation);
        let ownership_can_transfer = matches!(
            existing.owner_disposition.as_str(),
            "original_owner" | "fresh_agent_only"
        ) && incoming.owner_disposition == "original_owner"
            && incoming.ownership_generation > existing.ownership_generation;
        let route_degraded = existing.owner_disposition == "original_owner"
            && incoming.owner_disposition == "unroutable_private_route";
        let owner_can_change = route_can_resolve || ownership_can_transfer;
        let owner_may_differ = owner_can_change || route_degraded;
        if existing.repo != incoming.repo
            || existing.pr_number != incoming.pr_number
            || existing.base != incoming.base
            || existing.head_sha != incoming.head_sha
            || existing.outcome != incoming.outcome
            || existing.dedupe_key != incoming.dedupe_key
            || existing.trigger != incoming.trigger
            || existing.next_action != incoming.next_action
            || (!owner_may_differ && existing.origin_machine != incoming.origin_machine)
            || (!owner_may_differ && existing.owner_id != incoming.owner_id)
            || (!owner_may_differ && existing.ownership_generation != incoming.ownership_generation)
            || (!owner_may_differ && existing.owner_disposition != incoming.owner_disposition)
            || (!owner_may_differ && existing.owner_route_id != incoming.owner_route_id)
            || (!owner_may_differ && existing.owner_provider != incoming.owner_provider)
            || (!owner_may_differ && existing.resume_transport != incoming.resume_transport)
            || (!owner_may_differ
                && !same_terminal_provenance(
                    existing.owner_terminal_provenance,
                    incoming.owner_terminal_provenance,
                ))
            || existing.wake_consumer_available != incoming.wake_consumer_available
            || existing.failure_contexts != incoming.failure_contexts
        {
            return Err(CliFailure::new(
                1,
                "terminal handoff identity changed; refusing to replace durable continuation",
            ));
        }
        let record_changed = owner_can_change
            || route_degraded
            || (rearm_applied && existing_phase != TerminalHandoffPhase::Pending)
            || rearm_actionable;
        if record_changed {
            let record = ledger
                .terminal_handoffs
                .get_mut(&key)
                .expect("existing terminal handoff key");
            if route_degraded {
                "unroutable_private_route".clone_into(&mut record.owner_disposition);
                record.owner_route_id = None;
                record.owner_provider = None;
                record.resume_transport = None;
                record.owner_terminal_provenance = None;
            } else if owner_can_change {
                record.origin_machine = incoming.origin_machine;
                record.owner_id = incoming.owner_id;
                record.ownership_generation = incoming.ownership_generation;
                record.owner_disposition = incoming.owner_disposition;
                record.owner_route_id = incoming.owner_route_id;
                record.owner_provider = incoming.owner_provider;
                record.resume_transport = incoming.resume_transport;
                record.owner_terminal_provenance = incoming.owner_terminal_provenance;
            }
            if rearm_applied && existing_phase != TerminalHandoffPhase::Pending {
                record.phase = TerminalHandoffPhase::Pending;
            } else if rearm_actionable {
                record.phase = TerminalHandoffPhase::Recorded;
            }
            record.updated_at = Utc::now().to_rfc3339();
        }
        return Ok(record_changed || superseded_success);
    }
    if incoming.outcome == TerminalHandoffOutcome::ActionableFailure {
        ledger.terminal_handoffs.retain(|_, record| {
            record.outcome != TerminalHandoffOutcome::ActionableFailure
                || record.repo != incoming.repo
                || record.base != incoming.base
                || record.pr_number != incoming.pr_number
                || record.head_sha != incoming.head_sha
        });
    }
    make_capacity_for_terminal_handoff(ledger)?;
    ledger.terminal_handoffs.insert(key, incoming);
    Ok(true)
}

fn same_terminal_provenance(
    existing: Option<TerminalProvenanceKind>,
    incoming: Option<TerminalProvenanceKind>,
) -> bool {
    existing.unwrap_or_default() == incoming.unwrap_or_default()
}

fn resolve_conflicting_success(ledger: &mut StewardLedger, incoming: &TerminalHandoff) -> bool {
    if incoming.outcome != TerminalHandoffOutcome::ActionableFailure {
        return false;
    }
    let mut changed = false;
    let now = Utc::now().to_rfc3339();
    for record in ledger.terminal_handoffs.values_mut().filter(|record| {
        record.outcome == TerminalHandoffOutcome::SuccessContinuation
            && record.repo == incoming.repo
            && record.base == incoming.base
            && record.pr_number == incoming.pr_number
            && record.head_sha == incoming.head_sha
            && !matches!(
                record.phase,
                TerminalHandoffPhase::Applied | TerminalHandoffPhase::Resolved
            )
    }) {
        record.phase = TerminalHandoffPhase::Resolved;
        record.updated_at.clone_from(&now);
        changed = true;
    }
    changed
}

fn reconcile_after_ambiguous_save(
    ledger_path: &Path,
    ledger: &mut StewardLedger,
    fallback_handoffs: BTreeMap<String, TerminalHandoff>,
) {
    // `save_ledger` may fail after the atomic rename if only directory sync
    // failed. Reloading distinguishes the published old/new image and prevents
    // a later final save from overwriting it with a guessed in-memory state.
    match load_existing_ledger(ledger_path) {
        Ok(Some(published)) => *ledger = published,
        Ok(None) | Err(_) => ledger.terminal_handoffs = fallback_handoffs,
    }
}

fn make_capacity_for_terminal_handoff(ledger: &mut StewardLedger) -> Result<(), CliFailure> {
    while ledger.terminal_handoffs.len() >= MAX_TERMINAL_HANDOFFS {
        let oldest_terminal = ledger
            .terminal_handoffs
            .iter()
            .filter(|(_, record)| {
                matches!(
                    record.phase,
                    TerminalHandoffPhase::Applied | TerminalHandoffPhase::Resolved
                )
            })
            .min_by_key(|(_, record)| record.updated_at.as_str())
            .map(|(key, _)| key.clone());
        let Some(oldest_terminal) = oldest_terminal else {
            return Err(CliFailure::new(
                1,
                "terminal handoff ledger is full of unresolved obligations; refusing to discard or create another",
            ));
        };
        ledger.terminal_handoffs.remove(&oldest_terminal);
    }
    Ok(())
}

fn failure_fingerprint(failure_contexts: &[String]) -> String {
    let mut digest = Sha256::new();
    for context in failure_contexts {
        digest.update(context.len().to_be_bytes());
        digest.update(context.as_bytes());
    }
    hex::encode(digest.finalize())
}

#[cfg(test)]
#[path = "terminal_handoff/tests.rs"]
mod tests;
