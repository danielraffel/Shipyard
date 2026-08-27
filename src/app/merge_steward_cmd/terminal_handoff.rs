use super::{
    CliFailure, Path, StewardLedger, TerminalHandoff, TerminalHandoffOutcome, TerminalHandoffPhase,
    Utc,
    handoff::TerminalOwnerRoute,
    ledger::{load_ledger, save_ledger},
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
            resume_transport: owner.and_then(|owner| owner.resume_transport),
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
            resume_transport: owner.and_then(|owner| owner.resume_transport),
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
            } else if owner_can_change {
                record.origin_machine = incoming.origin_machine;
                record.owner_id = incoming.owner_id;
                record.ownership_generation = incoming.ownership_generation;
                record.owner_disposition = incoming.owner_disposition;
                record.owner_route_id = incoming.owner_route_id;
                record.owner_provider = incoming.owner_provider;
                record.resume_transport = incoming.resume_transport;
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
    match load_ledger(ledger_path) {
        Ok(published) => *ledger = published,
        Err(_) => ledger.terminal_handoffs = fallback_handoffs,
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
mod tests {
    use super::*;
    use crate::app::merge_steward_cmd::ledger::load_ledger;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn owner(route: &str) -> Option<TerminalOwnerRoute> {
        Some(TerminalOwnerRoute {
            origin_machine: "m3".to_owned(),
            owner_id: "owner-exact".to_owned(),
            ownership_generation: 1,
            owner_disposition: "original_owner".to_owned(),
            route_id: Some(route.to_owned()),
            provider: Some("codex".to_owned()),
            resume_transport: Some("codex_queue".to_owned()),
        })
    }

    fn fresh_agent_owner() -> Option<TerminalOwnerRoute> {
        Some(TerminalOwnerRoute {
            origin_machine: "m3".to_owned(),
            owner_id: "fresh-agent-only".to_owned(),
            ownership_generation: 1,
            owner_disposition: "fresh_agent_only".to_owned(),
            route_id: None,
            provider: None,
            resume_transport: None,
        })
    }

    #[test]
    fn replay_is_idempotent_but_owner_drift_fails_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("merge-steward.json");
        let mut ledger = StewardLedger::default();
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
            vec!["windows@app=9".to_owned()],
        )
        .expect("first");
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
            vec!["windows@app=9".to_owned()],
        )
        .expect("replay");
        let error = persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-b"),
            vec!["windows@app=9".to_owned()],
        )
        .expect_err("owner drift");
        assert!(error.message().contains("identity changed"));

        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
            vec!["macos@app=42".to_owned()],
        )
        .expect("a distinct exact failure trigger supersedes stale same-head evidence");
        assert_eq!(ledger.terminal_handoffs.len(), 1);
        assert_eq!(
            ledger
                .terminal_handoffs
                .values()
                .next()
                .expect("current failure")
                .failure_contexts,
            vec!["macos@app=42"]
        );
    }

    #[test]
    fn route_resolution_and_owner_transfer_are_monotonic() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("merge-steward.json");
        let mut ledger = StewardLedger::default();
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            None,
            vec!["macos@app=42".to_owned()],
        )
        .expect("unresolved route");
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            fresh_agent_owner(),
            vec!["macos@app=42".to_owned()],
        )
        .expect("resolve known route-less fallback");
        let mut generation_two = owner("route-generation-2").expect("owner");
        generation_two.owner_id = "replacement-owner".to_owned();
        generation_two.ownership_generation = 2;
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            Some(generation_two),
            vec!["macos@app=42".to_owned()],
        )
        .expect("transfer route");
        let record = ledger.terminal_handoffs.values().next().expect("record");
        assert_eq!(record.owner_id.as_deref(), Some("replacement-owner"));
        assert_eq!(record.ownership_generation, Some(2));
        assert_eq!(record.owner_route_id.as_deref(), Some("route-generation-2"));

        let error = persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-generation-1"),
            vec!["macos@app=42".to_owned()],
        )
        .expect_err("stale generation");
        assert!(error.message().contains("identity changed"));

        let mut unroutable = owner("discarded-route").expect("owner");
        unroutable.owner_disposition = "unroutable_private_route".to_owned();
        unroutable.route_id = None;
        unroutable.provider = None;
        unroutable.resume_transport = None;
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            8,
            HEAD,
            Some(unroutable.clone()),
            vec!["windows@app=9".to_owned()],
        )
        .expect("unroutable snapshot");
        unroutable.owner_id = "tampered-owner".to_owned();
        let error = persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            8,
            HEAD,
            Some(unroutable),
            vec!["windows@app=9".to_owned()],
        )
        .expect_err("unroutable replay cannot rewrite owner");
        assert!(error.message().contains("identity changed"));
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            8,
            HEAD,
            owner("validated-route"),
            vec!["windows@app=9".to_owned()],
        )
        .expect("validated route resolves unroutable snapshot");

        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            9,
            HEAD,
            owner("trusted-route"),
            vec!["linux@app=3".to_owned()],
        )
        .expect("trusted route");
        let mut degraded = owner("discarded-route").expect("owner");
        degraded.owner_id = "untrusted-fallback-owner".to_owned();
        degraded.owner_disposition = "unroutable_private_route".to_owned();
        degraded.route_id = None;
        degraded.provider = None;
        degraded.resume_transport = None;
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            9,
            HEAD,
            Some(degraded),
            vec!["linux@app=3".to_owned()],
        )
        .expect("route degradation preserves obligation");
        let degraded_record = ledger
            .terminal_handoffs
            .values()
            .find(|record| record.pr_number == 9)
            .expect("degraded record");
        assert_eq!(degraded_record.owner_id.as_deref(), Some("owner-exact"));
        assert_eq!(
            degraded_record.owner_disposition,
            "unroutable_private_route"
        );
        assert_eq!(degraded_record.owner_route_id, None);
    }

    #[test]
    fn base_scoped_observation_resolves_only_proven_head_supersession() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("merge-steward.json");
        let mut ledger = StewardLedger::default();
        persist_success_continuation(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
        )
        .expect("success");
        persist_success_continuation(
            &path,
            &mut ledger,
            "owner/repo",
            "MAIN",
            7,
            HEAD,
            owner("route-case-distinct"),
        )
        .expect("case-distinct base");
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            8,
            HEAD,
            owner("route-b"),
            vec!["macos@app=42".to_owned()],
        )
        .expect("failure");
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "release",
            9,
            HEAD,
            owner("route-release"),
            vec!["windows@app=9".to_owned()],
        )
        .expect("other base failure");
        let current_heads =
            BTreeMap::from([(7, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned())]);
        resolve_superseded_terminal_handoffs(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            &current_heads,
        )
        .expect("resolve stale");
        assert_eq!(
            ledger
                .terminal_handoffs
                .values()
                .find(|record| record.pr_number == 7 && record.base == "main")
                .expect("superseded head")
                .phase,
            TerminalHandoffPhase::Resolved
        );
        assert_eq!(
            ledger
                .terminal_handoffs
                .values()
                .find(|record| record.pr_number == 7 && record.base == "MAIN")
                .expect("case-distinct base")
                .phase,
            TerminalHandoffPhase::Pending
        );
        assert_eq!(
            ledger
                .terminal_handoffs
                .values()
                .find(|record| record.pr_number == 8)
                .expect("closed main PR")
                .phase,
            TerminalHandoffPhase::Resolved
        );
        assert_eq!(
            ledger
                .terminal_handoffs
                .values()
                .find(|record| record.pr_number == 9)
                .expect("other base")
                .phase,
            TerminalHandoffPhase::Recorded
        );
    }

    #[test]
    fn deterministic_convergence_resolves_recorded_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("merge-steward.json");
        let mut ledger = StewardLedger::default();
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
            vec!["macos@app=42".to_owned()],
        )
        .expect("failure");
        persist_success_continuation(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
        )
        .expect("pending success");
        resolve_terminal_handoffs(&path, &mut ledger, "owner/repo", "main", 7, HEAD)
            .expect("resolve");
        assert!(
            ledger
                .terminal_handoffs
                .values()
                .all(|record| record.phase == TerminalHandoffPhase::Resolved)
        );
        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
            vec!["macos@app=42".to_owned()],
        )
        .expect("recur");
        let restarted = load_ledger(&path).expect("restart");
        assert_eq!(
            restarted
                .terminal_handoffs
                .values()
                .find(|record| record.outcome == TerminalHandoffOutcome::ActionableFailure)
                .expect("record")
                .phase,
            TerminalHandoffPhase::Recorded
        );
    }

    #[test]
    fn same_head_failure_supersedes_ambiguous_pending_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("merge-steward.json");
        let mut ledger = StewardLedger::default();
        persist_success_continuation(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
        )
        .expect("durable intent precedes ambiguous enqueue response");
        assert_eq!(
            ledger
                .terminal_handoffs
                .values()
                .next()
                .expect("pending continuation")
                .phase,
            TerminalHandoffPhase::Pending
        );

        let blocked_parent = temp.path().join("not-a-directory");
        std::fs::write(&blocked_parent, "occupied").expect("blocked parent");
        persist_actionable_failure(
            &blocked_parent.join("ledger.json"),
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
            vec!["macos@app=42".to_owned()],
        )
        .expect_err("failed publication rolls back both outcome changes");
        assert_eq!(ledger.terminal_handoffs.len(), 1);
        assert_eq!(
            ledger
                .terminal_handoffs
                .values()
                .next()
                .expect("original continuation")
                .phase,
            TerminalHandoffPhase::Pending
        );

        persist_actionable_failure(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
            vec!["macos@app=42".to_owned()],
        )
        .expect("same-head required failure");

        let restarted = load_ledger(&path).expect("restart");
        let unresolved = restarted
            .terminal_handoffs
            .values()
            .filter(|record| {
                !matches!(
                    record.phase,
                    TerminalHandoffPhase::Applied | TerminalHandoffPhase::Resolved
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(
            unresolved[0].outcome,
            TerminalHandoffOutcome::ActionableFailure
        );
        assert_eq!(unresolved[0].phase, TerminalHandoffPhase::Recorded);
        assert!(
            restarted.terminal_handoffs.values().any(|record| {
                record.outcome == TerminalHandoffOutcome::SuccessContinuation
                    && record.phase == TerminalHandoffPhase::Resolved
            }),
            "the ambiguous success intent remains as resolved audit history"
        );
    }

    #[test]
    fn queued_reconciliation_completes_uncertain_success_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("merge-steward.json");
        let mut ledger = StewardLedger::default();
        assert!(
            !reconcile_queued_success_continuation(
                &path,
                &mut ledger,
                "owner/repo",
                "main",
                7,
                HEAD,
            )
            .expect("no intent")
        );
        persist_success_continuation(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
        )
        .expect("intent");
        assert!(
            reconcile_queued_success_continuation(
                &path,
                &mut ledger,
                "owner/repo",
                "main",
                7,
                HEAD,
            )
            .expect("first reconciliation")
        );
        assert!(
            !reconcile_queued_success_continuation(
                &path,
                &mut ledger,
                "owner/repo",
                "main",
                7,
                HEAD,
            )
            .expect("deduplicated reconciliation")
        );
        persist_success_continuation(
            &path,
            &mut ledger,
            "owner/repo",
            "main",
            7,
            HEAD,
            owner("route-a"),
        )
        .expect("rearm exact-head continuation");
        assert_eq!(
            ledger
                .terminal_handoffs
                .values()
                .next()
                .expect("rearmed")
                .phase,
            TerminalHandoffPhase::Pending
        );
        assert!(
            reconcile_queued_success_continuation(
                &path,
                &mut ledger,
                "owner/repo",
                "main",
                7,
                HEAD,
            )
            .expect("reconciled re-enqueue")
        );
    }

    #[test]
    fn retention_discards_only_applied_records_and_fails_closed_on_pending_capacity() {
        let mut ledger = StewardLedger::default();
        for pr_number in 1..=MAX_TERMINAL_HANDOFFS as u64 {
            let dedupe_key = format!("owner/repo#{pr_number}:{HEAD}:success");
            ledger.terminal_handoffs.insert(
                dedupe_key.clone(),
                TerminalHandoff {
                    dedupe_key,
                    repo: "owner/repo".to_owned(),
                    base: "main".to_owned(),
                    pr_number,
                    head_sha: HEAD.to_owned(),
                    outcome: TerminalHandoffOutcome::SuccessContinuation,
                    trigger: "required_checks_terminal_success".to_owned(),
                    next_action: "arm_merge_queue_exact_head".to_owned(),
                    origin_machine: None,
                    owner_id: None,
                    ownership_generation: None,
                    owner_disposition: "route_registry_required".to_owned(),
                    owner_route_id: None,
                    owner_provider: None,
                    resume_transport: None,
                    wake_consumer_available: false,
                    failure_contexts: Vec::new(),
                    phase: TerminalHandoffPhase::Pending,
                    created_at: "2026-08-27T00:00:00Z".to_owned(),
                    updated_at: format!("2026-08-27T00:{:02}:00Z", pr_number % 60),
                },
            );
        }
        assert!(make_capacity_for_terminal_handoff(&mut ledger).is_err());
        ledger
            .terminal_handoffs
            .values_mut()
            .next()
            .expect("record")
            .phase = TerminalHandoffPhase::Applied;
        make_capacity_for_terminal_handoff(&mut ledger).expect("evict applied record");
        assert_eq!(ledger.terminal_handoffs.len(), MAX_TERMINAL_HANDOFFS - 1);
    }
}
