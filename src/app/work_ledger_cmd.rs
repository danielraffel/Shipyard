use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use serde::Deserialize;
use serde_json::Value;

use crate::cloud::GitHubActions;
use crate::custody_transport::{
    MAX_CUSTODY_WIRE_BYTES, handle_incoming_request, incoming_peer_evidence_from_environment,
    load_custody_transport_policy, remote_custody_inventory,
};
use crate::daemon_ipc::read_daemon_status;
use crate::output::write_pretty_json;
use crate::paths::RuntimePaths;
use crate::work_ledger::{
    AgentReturnExpectation, CustodyStatus, NativePublicationReport, RepoPolicy, WorkLedger,
    absent_status, apply_legacy_snapshot, immutable_legacy_status, local_work_inventory,
    plan_legacy_snapshot, validate_repo_policy,
};
use crate::workstream_activation_loader::{WorkstreamActivationLoader, WorkstreamActivationState};

use super::CliFailure;
use super::cli::{OwnershipLeaseCommand, WorkLedgerCommand, WorkLedgerPolicyCommand};
use super::merge_steward_cmd::native_publication_request;

const MAX_AGENT_RECEIPT_BYTES: u64 = 64 * 1024;

#[allow(clippy::too_many_lines)]
pub(super) fn work_ledger_command<W: Write>(
    command: &WorkLedgerCommand,
    runtime_paths: &RuntimePaths,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let state_dir = &runtime_paths.state_dir;
    match command {
        WorkLedgerCommand::Status => {
            let status = match immutable_legacy_status(state_dir).map_err(failure)? {
                Some(status) => status,
                None => WorkLedger::open_existing(state_dir)
                    .map_err(failure)?
                    .map_or_else(|| Ok(absent_status()), |ledger| ledger.status())
                    .map_err(failure)?,
            };
            let operational = work_ledger_operational_status(runtime_paths);
            if json {
                let rendered = work_ledger_status_json(&status, &operational)?;
                write_pretty_json(stdout, &rendered).map_err(failure)?;
            } else {
                writeln!(
                    stdout,
                    "Work ledger: {}",
                    if status.exists { "present" } else { "absent" }
                )
                .map_err(failure)?;
                writeln!(stdout, "Schema: {}", status.schema_version).map_err(failure)?;
                writeln!(stdout, "Integrity: {}", status.integrity).map_err(failure)?;
                writeln!(stdout, "Journal: {}", status.journal_mode).map_err(failure)?;
                writeln!(stdout, "Synchronous: {}", status.synchronous).map_err(failure)?;
                writeln!(stdout, "Foreign keys: {}", status.foreign_keys).map_err(failure)?;
                writeln!(stdout, "Work items: {}", status.work_items).map_err(failure)?;
                writeln!(stdout, "Pending wakes: {}", status.pending_wakes).map_err(failure)?;
                writeln!(stdout, "Uncertain wakes: {}", status.uncertain_wakes).map_err(failure)?;
                writeln!(
                    stdout,
                    "Pending projection intents: {}",
                    status.pending_projection_intents
                )
                .map_err(failure)?;
                writeln!(
                    stdout,
                    "Quarantined projection intents: {}",
                    status.quarantined_projection_intents
                )
                .map_err(failure)?;
                writeln!(stdout, "Activation: {}", operational.activation_state)
                    .map_err(failure)?;
                writeln!(stdout, "Dispatch: {}", operational.dispatch_state).map_err(failure)?;
                writeln!(
                    stdout,
                    "Continuation runtime: {}",
                    operational.continuation_runtime
                )
                .map_err(failure)?;
                if let Some(reason_code) = &operational.activation_reason_code {
                    writeln!(stdout, "Activation reason: {reason_code}").map_err(failure)?;
                }
                if let Some(reason_code) = &operational.runtime_reason_code {
                    writeln!(stdout, "Continuation runtime reason: {reason_code}")
                        .map_err(failure)?;
                }
            }
        }
        WorkLedgerCommand::Inventory => {
            let inventory = local_work_inventory(state_dir).map_err(failure)?;
            if json {
                write_pretty_json(stdout, &inventory).map_err(failure)?;
            } else {
                for item in &inventory.items {
                    let ownership = match (
                        item.ownership_id.as_deref(),
                        item.ownership_state.as_deref(),
                        item.ownership_work_generation,
                        item.ownership_owner_generation,
                    ) {
                        (Some(id), Some(state), Some(work), Some(owner)) => {
                            format!("{id}:{state}:{work}/{owner}")
                        }
                        _ => "none".to_owned(),
                    };
                    writeln!(
                        stdout,
                        "{}:{}:{}#{} {} {} workstream={} work={} generation={}/{} owner={} ownership={}",
                        item.repository_provider.as_deref().unwrap_or("unknown-provider"),
                        item.repository_id.as_deref().unwrap_or("unknown-repository-id"),
                        item.repository,
                        item.pull_request,
                        item.exact_head,
                        item.state,
                        item.workstream_handle,
                        item.work_item_id,
                        item.work_generation,
                        item.owner_generation,
                        item.owner_id.as_deref().unwrap_or("none"),
                        ownership,
                    )
                    .map_err(failure)?;
                }
                writeln!(
                    stdout,
                    "Inventory: {} item(s), limit={}, complete={}",
                    inventory.items.len(),
                    inventory.limit,
                    inventory.complete
                )
                .map_err(failure)?;
            }
        }
        WorkLedgerCommand::CustodyInventory(arguments) => {
            let message = &arguments.message;
            let correlation_hints = &arguments.correlation_hints;
            let production_paths = RuntimePaths::current(crate::identity::RuntimeMode::Shipyard);
            if runtime_paths != &production_paths {
                return Err(CliFailure::new(
                    1,
                    "custody inventory is available only against canonical production roots",
                ));
            }
            let policy = load_custody_transport_policy(
                crate::identity::RuntimeMode::Shipyard,
                runtime_paths.global_dir.clone(),
            )
            .map_err(|error| CliFailure::new(1, error))?
            .ok_or_else(|| CliFailure::new(1, "custody transport is disabled"))?;
            let hints = correlation_hints
                .as_deref()
                .map(read_correlation_hints)
                .transpose()?;
            let result = remote_custody_inventory(&policy, state_dir, message);
            if json {
                let mut rendered = serde_json::to_value(&result).map_err(failure)?;
                if let (Some(hints), Some(object)) = (hints, rendered.as_object_mut()) {
                    object.insert(
                        "correlation_hints".to_owned(),
                        serde_json::to_value(hints).map_err(failure)?,
                    );
                }
                write_pretty_json(stdout, &rendered).map_err(failure)?;
            } else {
                let rendered = serde_json::to_value(&result).map_err(failure)?;
                writeln!(
                    stdout,
                    "Custody inventory: {}",
                    rendered
                        .get("outcome")
                        .and_then(Value::as_str)
                        .unwrap_or("refused")
                )
                .map_err(failure)?;
            }
        }
        WorkLedgerCommand::CustodyStatus => {
            let status = WorkLedger::open_existing(state_dir)
                .map_err(failure)?
                .map_or_else(
                    || Ok(CustodyStatus::default()),
                    |ledger| ledger.custody_status(),
                )
                .map_err(failure)?;
            if json {
                write_pretty_json(stdout, &status).map_err(failure)?;
            } else {
                writeln!(stdout, "Outgoing pending: {}", status.outgoing_pending)
                    .map_err(failure)?;
                writeln!(stdout, "Outgoing claimed: {}", status.outgoing_claimed)
                    .map_err(failure)?;
                writeln!(stdout, "Outgoing accepted: {}", status.outgoing_accepted)
                    .map_err(failure)?;
                writeln!(stdout, "Outgoing processed: {}", status.outgoing_processed)
                    .map_err(failure)?;
                writeln!(stdout, "Outgoing cancelled: {}", status.outgoing_cancelled)
                    .map_err(failure)?;
                writeln!(
                    stdout,
                    "Outgoing superseded: {}",
                    status.outgoing_superseded
                )
                .map_err(failure)?;
                writeln!(stdout, "Incoming received: {}", status.incoming_received)
                    .map_err(failure)?;
                writeln!(
                    stdout,
                    "Incoming processing: {}",
                    status.incoming_processing
                )
                .map_err(failure)?;
                writeln!(stdout, "Incoming processed: {}", status.incoming_processed)
                    .map_err(failure)?;
                writeln!(stdout, "Incoming cancelled: {}", status.incoming_cancelled)
                    .map_err(failure)?;
                writeln!(
                    stdout,
                    "Incoming superseded: {}",
                    status.incoming_superseded
                )
                .map_err(failure)?;
                writeln!(stdout, "Pending controls: {}", status.pending_controls)
                    .map_err(failure)?;
                writeln!(
                    stdout,
                    "Pending successor rebinds: {}",
                    status.pending_rebinds
                )
                .map_err(failure)?;
            }
        }
        WorkLedgerCommand::CustodyReceive => {
            let production_paths = RuntimePaths::current(crate::identity::RuntimeMode::Shipyard);
            if runtime_paths != &production_paths {
                return Err(CliFailure::new(
                    1,
                    "custody receive is available only against canonical production roots",
                ));
            }
            let policy = load_custody_transport_policy(
                crate::identity::RuntimeMode::Shipyard,
                runtime_paths.global_dir.clone(),
            )
            .map_err(|error| CliFailure::new(1, error))?
            .ok_or_else(|| CliFailure::new(1, "custody transport is disabled"))?;
            let evidence = incoming_peer_evidence_from_environment(&policy)
                .map_err(|error| CliFailure::new(1, error))?;
            let mut input = Vec::new();
            std::io::stdin()
                .take(MAX_CUSTODY_WIRE_BYTES + 1)
                .read_to_end(&mut input)
                .map_err(failure)?;
            if input.len() as u64 > MAX_CUSTODY_WIRE_BYTES {
                return Err(CliFailure::new(1, "custody request exceeds the wire limit"));
            }
            let response = handle_incoming_request(&policy, state_dir, &evidence, &input);
            serde_json::to_writer(&mut *stdout, &response).map_err(failure)?;
            writeln!(stdout).map_err(failure)?;
        }
        WorkLedgerCommand::Import { apply } => {
            let report = if *apply {
                apply_legacy_snapshot(state_dir).map_err(failure)?
            } else {
                plan_legacy_snapshot(state_dir).map_err(failure)?
            };
            if json {
                write_pretty_json(stdout, &report).map_err(failure)?;
            } else {
                writeln!(
                    stdout,
                    "Legacy import: {} (shadow only)",
                    if report.applied { "applied" } else { "dry-run" }
                )
                .map_err(failure)?;
                writeln!(stdout, "Candidates: {}", report.candidates).map_err(failure)?;
                writeln!(stdout, "Inserted: {}", report.inserted).map_err(failure)?;
                writeln!(stdout, "Updated: {}", report.updated).map_err(failure)?;
                writeln!(stdout, "Unchanged: {}", report.unchanged).map_err(failure)?;
                for (kind, count) in &report.by_kind {
                    writeln!(stdout, "  {kind}: {count}").map_err(failure)?;
                }
                writeln!(stdout, "Plan digest: {}", report.plan_digest).map_err(failure)?;
                writeln!(stdout, "Activation: disabled").map_err(failure)?;
                writeln!(stdout, "Dispatch: disabled").map_err(failure)?;
            }
        }
        WorkLedgerCommand::Publish {
            repo,
            pr,
            head,
            apply,
        } => {
            let production_paths = RuntimePaths::current(crate::identity::RuntimeMode::Shipyard);
            if runtime_paths != &production_paths {
                return Err(CliFailure::new(
                    1,
                    "native publication is available only against canonical production roots",
                ));
            }
            let actions = GitHubActions::new(cwd).with_repo_override(repo);
            let request = native_publication_request(runtime_paths, &actions, repo, *pr, head)?;
            let mut loader = WorkstreamActivationLoader::production();
            let ready = match loader.revalidate_for_tick() {
                WorkstreamActivationState::Ready(ready) => ready,
                WorkstreamActivationState::Disabled => {
                    return Err(CliFailure::new(
                        1,
                        "workstream continuation activation is disabled",
                    ));
                }
                WorkstreamActivationState::Refused(reason) => {
                    return Err(CliFailure::new(
                        1,
                        format!(
                            "workstream continuation activation refused: {}",
                            reason.code()
                        ),
                    ));
                }
            };
            if ready.machine_tag != request.origin_machine {
                return Err(CliFailure::new(
                    1,
                    "durable handoff belongs to a different origin machine",
                ));
            }
            let report = WorkLedger::plan_or_apply_native_continuation(
                state_dir,
                &request,
                &ready.config,
                *apply,
            )
            .map_err(failure)?;
            write_publication_report(stdout, &report, json)?;
        }
        WorkLedgerCommand::ContextChallenge { wake } => {
            let mut activation = ProductionHandshakeActivation::new(runtime_paths)?;
            let ready = activation.revalidate()?;
            let ledger = required_ledger(state_dir)?;
            let challenge = ledger
                .agent_context_challenge(wake, &ready.config.repositories)
                .map_err(failure)?;
            if json {
                write_pretty_json(stdout, &challenge).map_err(failure)?;
            } else {
                writeln!(stdout, "Context challenge: ready").map_err(failure)?;
                writeln!(stdout, "Wake: {}", challenge.wake_id).map_err(failure)?;
                writeln!(stdout, "Workstream: {}", challenge.workstream_handle).map_err(failure)?;
                writeln!(stdout, "Repository: {}", challenge.repository).map_err(failure)?;
                writeln!(
                    stdout,
                    "Checkpoint generation: {}",
                    challenge.checkpoint_generation
                )
                .map_err(failure)?;
            }
        }
        WorkLedgerCommand::AcknowledgeContext {
            wake,
            receipt,
            holder_output,
        } => {
            let mut holder_file = reserve_private_output(holder_output)?;
            let mut activation = ProductionHandshakeActivation::new(runtime_paths)?;
            let (bytes, ready) =
                read_then_revalidate(&mut activation, || read_private_input(receipt))?;
            let ledger = required_ledger(state_dir)?;
            ledger
                .agent_context_challenge(wake, &ready.config.repositories)
                .map_err(failure)?;
            let ready = activation.revalidate()?;
            ledger
                .agent_context_challenge(wake, &ready.config.repositories)
                .map_err(failure)?;
            activation.revalidate()?;
            let (ownership, holder_material) = ledger
                .acknowledge_agent_context_with_lease(wake, &bytes)
                .map_err(failure)?;
            write_private_output(&mut holder_file, &holder_material)?;
            let return_challenge = ledger
                .agent_return_challenge(&ownership.ownership_id, &ready.config.repositories)
                .map_err(failure)?;
            write_agent_transition(
                stdout,
                json,
                "context_acknowledged",
                &ownership,
                Some(&return_challenge),
            )?;
        }
        WorkLedgerCommand::ReturnChallenge { ownership } => {
            let mut activation = ProductionHandshakeActivation::new(runtime_paths)?;
            let ready = activation.revalidate()?;
            let ledger = required_ledger(state_dir)?;
            let challenge = ledger
                .agent_return_challenge(ownership, &ready.config.repositories)
                .map_err(failure)?;
            if json {
                write_pretty_json(stdout, &challenge).map_err(failure)?;
            } else {
                writeln!(stdout, "Return challenge: ready").map_err(failure)?;
                writeln!(stdout, "Ownership: {}", challenge.ownership_id).map_err(failure)?;
                writeln!(stdout, "Repository: {}", challenge.repository).map_err(failure)?;
                writeln!(
                    stdout,
                    "Checkpoint floor: {}",
                    challenge.checkpoint_generation
                )
                .map_err(failure)?;
            }
        }
        WorkLedgerCommand::ReturnOwnership {
            ownership,
            expectation,
            receipt,
            holder,
        } => {
            let stdin_count = [expectation, receipt, holder]
                .into_iter()
                .filter(|path| path.as_path() == Path::new("-"))
                .count();
            if stdin_count > 1 {
                return Err(CliFailure::new(
                    1,
                    "only one return input may read from stdin",
                ));
            }
            let mut activation = ProductionHandshakeActivation::new(runtime_paths)?;
            let ((expectation_bytes, receipt_bytes, holder_bytes), ready) =
                read_then_revalidate(&mut activation, || {
                    Ok((
                        read_private_input(expectation)?,
                        read_private_input(receipt)?,
                        read_private_input(holder)?,
                    ))
                })?;
            let expected: AgentReturnExpectation = serde_json::from_slice(&expectation_bytes)
                .map_err(|_| {
                    CliFailure::new(1, "return expectation is not exact schema-v1 JSON")
                })?;
            if expected.ownership_id != *ownership {
                return Err(CliFailure::new(
                    1,
                    "return expectation belongs to a different ownership",
                ));
            }
            let ledger = required_ledger(state_dir)?;
            ledger
                .agent_return_challenge(ownership, &ready.config.repositories)
                .map_err(failure)?;
            let ready = activation.revalidate()?;
            ledger
                .agent_return_challenge(ownership, &ready.config.repositories)
                .map_err(failure)?;
            activation.revalidate()?;
            let returned = ledger
                .return_agent_ownership_with_holder(
                    ownership,
                    &expected.delivery_id,
                    expected.work_generation,
                    &expected,
                    &receipt_bytes,
                    &holder_bytes,
                )
                .map_err(failure)?;
            write_agent_transition(stdout, json, "ownership_returned", &returned, None)?;
        }
        WorkLedgerCommand::Ownership { command } => match command.as_ref() {
            OwnershipLeaseCommand::Bootstrap { .. } => ownership_bootstrap_command(
                command.as_ref(),
                runtime_paths,
                state_dir,
                json,
                stdout,
            )?,
            OwnershipLeaseCommand::Renew { .. } => {
                ownership_renew_command(command.as_ref(), runtime_paths, state_dir, json, stdout)?;
            }
            OwnershipLeaseCommand::Release { .. } => {
                ownership_release_command(
                    command.as_ref(),
                    runtime_paths,
                    state_dir,
                    json,
                    stdout,
                )?;
            }
            OwnershipLeaseCommand::Adopt { .. } => {
                ownership_adopt_command(command.as_ref(), runtime_paths, state_dir, json, stdout)?;
            }
            OwnershipLeaseCommand::CustodyPrepare { .. } => ownership_custody_prepare_command(
                command.as_ref(),
                runtime_paths,
                state_dir,
                json,
                stdout,
            )?,
        },
        WorkLedgerCommand::Policy { command } => {
            policy_command(command.as_ref(), state_dir, json, stdout)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn parse_lease_expiry(value: &str) -> Result<chrono::DateTime<chrono::Utc>, CliFailure> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
        .map_err(|_| CliFailure::new(1, "lease expiry must be exact RFC3339"))
}

fn ownership_bootstrap_command<W: Write>(
    command: &OwnershipLeaseCommand,
    runtime_paths: &RuntimePaths,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let OwnershipLeaseCommand::Bootstrap { request } = command else {
        unreachable!("bootstrap helper requires bootstrap command")
    };
    let expires_at = parse_lease_expiry(&request.expires_at)?;
    let mut holder_file = reserve_private_output(&request.holder_output)?;
    let mut activation = ProductionHandshakeActivation::new(runtime_paths)?;
    let ledger = required_ledger(state_dir)?;
    activation.revalidate()?;
    let (lease, holder_material) = ledger
        .bootstrap_legacy_ownership_with_protected_holder(&request.ownership, expires_at)
        .map_err(failure)?;
    write_private_output(&mut holder_file, &holder_material)?;
    if json {
        write_pretty_json(stdout, &lease).map_err(failure)?;
    } else {
        writeln!(stdout, "Ownership lease: bootstrapped").map_err(failure)?;
        writeln!(stdout, "Lease: {}", lease.lease_id).map_err(failure)?;
        writeln!(stdout, "Generation: {}", lease.lease_generation).map_err(failure)?;
    }
    Ok(())
}

fn ownership_renew_command<W: Write>(
    command: &OwnershipLeaseCommand,
    runtime_paths: &RuntimePaths,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let OwnershipLeaseCommand::Renew { request } = command else {
        unreachable!("renew helper requires renew command")
    };
    let expires_at = parse_lease_expiry(&request.expires_at)?;
    let mut successor_file = reserve_private_output(&request.holder_output)?;
    let mut activation = ProductionHandshakeActivation::new(runtime_paths)?;
    let (holder_bytes, _) =
        read_then_revalidate(&mut activation, || read_private_input(&request.holder))?;
    let ledger = required_ledger(state_dir)?;
    activation.revalidate()?;
    let (renewed, successor_material) = ledger
        .renew_ownership_lease_with_material(
            &request.ownership,
            &holder_bytes,
            request.expected_generation,
            expires_at,
        )
        .map_err(failure)?;
    write_private_output(&mut successor_file, &successor_material)?;
    if json {
        write_pretty_json(stdout, &renewed).map_err(failure)?;
    } else {
        writeln!(stdout, "Ownership lease: renewed").map_err(failure)?;
        writeln!(stdout, "Lease: {}", renewed.lease_id).map_err(failure)?;
        writeln!(stdout, "Generation: {}", renewed.lease_generation).map_err(failure)?;
    }
    Ok(())
}

fn ownership_release_command<W: Write>(
    command: &OwnershipLeaseCommand,
    runtime_paths: &RuntimePaths,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let OwnershipLeaseCommand::Release { request } = command else {
        unreachable!("release helper requires release command")
    };
    let mut activation = ProductionHandshakeActivation::new(runtime_paths)?;
    let (holder_bytes, _) =
        read_then_revalidate(&mut activation, || read_private_input(&request.holder))?;
    let ledger = required_ledger(state_dir)?;
    activation.revalidate()?;
    let release_digest = ledger
        .release_ownership_lease_with_material(
            &request.ownership,
            &holder_bytes,
            request.expected_generation,
        )
        .map_err(failure)?;
    if json {
        write_pretty_json(
            stdout,
            &serde_json::json!({
                "ownership_id": request.ownership,
                "lease_generation": request.expected_generation,
                "release_digest": release_digest,
            }),
        )
        .map_err(failure)?;
    } else {
        writeln!(stdout, "Ownership lease: released").map_err(failure)?;
        writeln!(stdout, "Release digest: {release_digest}").map_err(failure)?;
    }
    Ok(())
}

fn ownership_adopt_command<W: Write>(
    command: &OwnershipLeaseCommand,
    runtime_paths: &RuntimePaths,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let OwnershipLeaseCommand::Adopt { request } = command else {
        unreachable!("adopt helper requires adopt command")
    };
    let expires_at = parse_lease_expiry(&request.expires_at)?;
    let mut holder_file = reserve_private_output(&request.holder_output)?;
    let mut activation = ProductionHandshakeActivation::new(runtime_paths)?;
    let (proof_bytes, _) =
        read_then_revalidate(&mut activation, || read_private_input(&request.proof))?;
    let holder_bytes = request
        .holder
        .as_ref()
        .map(|holder| read_then_revalidate(&mut activation, || read_private_input(holder)))
        .transpose()?
        .map(|(bytes, _)| bytes);
    let ledger = required_ledger(state_dir)?;
    activation.revalidate()?;
    let (adopted, holder_material) = ledger
        .adopt_ownership_with_protected_holder(
            &request.ownership,
            request.expected_generation,
            expires_at,
            &proof_bytes,
            holder_bytes.as_deref(),
        )
        .map_err(failure)?;
    write_private_output(&mut holder_file, &holder_material)?;
    if json {
        write_pretty_json(stdout, &adopted).map_err(failure)?;
    } else {
        let lease = match &adopted {
            crate::work_ledger::OwnershipAdoptionResult::Attached(lease)
            | crate::work_ledger::OwnershipAdoptionResult::SuccessorCreated(lease) => lease,
        };
        writeln!(stdout, "Ownership lease: adopted").map_err(failure)?;
        writeln!(stdout, "Lease: {}", lease.lease_id).map_err(failure)?;
        writeln!(stdout, "Generation: {}", lease.lease_generation).map_err(failure)?;
    }
    Ok(())
}

fn ownership_custody_prepare_command<W: Write>(
    command: &OwnershipLeaseCommand,
    runtime_paths: &RuntimePaths,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let OwnershipLeaseCommand::CustodyPrepare { request } = command else {
        unreachable!("custody prepare helper requires custody prepare command")
    };
    let mut activation = ProductionHandshakeActivation::new(runtime_paths)?;
    let (holder_bytes, _) =
        read_then_revalidate(&mut activation, || read_private_input(&request.holder))?;
    let ledger = required_ledger(state_dir)?;
    activation.revalidate()?;
    let rebind = ledger
        .prepare_custody_successor_rebind_with_holder(
            &request.message,
            &request.expected_old_incarnation,
            &request.new_target_incarnation,
            &request.new_target_route,
            &request.terminal_adapter,
            &request.new_authority_digest,
            &request.ownership,
            request.expected_generation,
            &holder_bytes,
        )
        .map_err(failure)?;
    if json {
        write_pretty_json(stdout, &rebind).map_err(failure)?;
    } else {
        writeln!(stdout, "Custody successor: prepared").map_err(failure)?;
        writeln!(stdout, "Rebind: {}", rebind.rebind_id).map_err(failure)?;
        writeln!(stdout, "Lease: {}", rebind.ownership_lease_id).map_err(failure)?;
        writeln!(stdout, "Generation: {}", rebind.ownership_lease_generation).map_err(failure)?;
    }
    Ok(())
}

#[cfg(unix)]
const MAX_CORRELATION_HINT_BYTES: u64 = 16 * 1024;

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct CustodyCorrelationHints {
    linear_workspace_id: String,
    linear_root_uuid: String,
    provider_repository_id: String,
}

#[cfg(not(unix))]
fn read_correlation_hints(_path: &Path) -> Result<CustodyCorrelationHints, CliFailure> {
    Err(CliFailure::new(
        1,
        "correlation hints owner-only access cannot be proven on this platform",
    ))
}

#[cfg(unix)]
fn read_correlation_hints(path: &Path) -> Result<CustodyCorrelationHints, CliFailure> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    let metadata = std::fs::symlink_metadata(path).map_err(failure)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_CORRELATION_HINT_BYTES
    {
        return Err(CliFailure::new(
            1,
            "correlation hints must be a bounded regular file",
        ));
    }
    if metadata.mode() & 0o077 != 0
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.nlink() != 1
    {
        return Err(CliFailure::new(1, "correlation hints must be owner-only"));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    options.custom_flags(nix::libc::O_NOFOLLOW);
    let mut file = options.open(path).map_err(failure)?;
    let opened = file.metadata().map_err(failure)?;
    if !opened.is_file()
        || opened.len() != metadata.len()
        || opened.dev() != metadata.dev()
        || opened.ino() != metadata.ino()
        || opened.uid() != nix::unistd::Uid::effective().as_raw()
        || opened.nlink() != 1
        || opened.mode() & 0o077 != 0
    {
        return Err(CliFailure::new(
            1,
            "correlation hints changed while opening",
        ));
    }
    let mut encoded = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(MAX_CORRELATION_HINT_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(failure)?;
    if encoded.len() as u64 > MAX_CORRELATION_HINT_BYTES {
        return Err(CliFailure::new(
            1,
            "correlation hints exceed the size bound",
        ));
    }
    let after = file.metadata().map_err(failure)?;
    if after.dev() != opened.dev()
        || after.ino() != opened.ino()
        || after.uid() != opened.uid()
        || after.nlink() != opened.nlink()
        || after.mode() != opened.mode()
        || after.len() != opened.len()
        || after.mtime() != opened.mtime()
        || after.mtime_nsec() != opened.mtime_nsec()
        || after.ctime() != opened.ctime()
        || after.ctime_nsec() != opened.ctime_nsec()
    {
        return Err(CliFailure::new(
            1,
            "correlation hints changed while reading",
        ));
    }
    let hints: CustodyCorrelationHints = serde_json::from_slice(&encoded)
        .map_err(|_| CliFailure::new(1, "correlation hints are malformed"))?;
    for value in [&hints.linear_workspace_id, &hints.provider_repository_id] {
        if value.is_empty()
            || value.len() > 512
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'/' | b'\\'))
        {
            return Err(CliFailure::new(1, "correlation hint identity is invalid"));
        }
    }
    let root = hints.linear_root_uuid.as_bytes();
    if root.len() != 36
        || root.iter().enumerate().any(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte != b'-',
            _ => !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase(),
        })
    {
        return Err(CliFailure::new(1, "correlation hint root UUID is invalid"));
    }
    Ok(hints)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkLedgerOperationalStatus {
    activation_enabled: bool,
    dispatch_enabled: bool,
    activation_state: String,
    dispatch_state: String,
    continuation_runtime: String,
    activation_reason_code: Option<String>,
    runtime_reason_code: Option<String>,
}

fn work_ledger_operational_status(runtime_paths: &RuntimePaths) -> WorkLedgerOperationalStatus {
    let production_paths = RuntimePaths::current(crate::identity::RuntimeMode::Shipyard);
    if runtime_paths != &production_paths {
        return operational_status(
            WorkstreamActivationState::Refused(
                crate::workstream_activation_loader::WorkstreamActivationRefusal::NonProductionRuntime,
            ),
            read_daemon_status(&runtime_paths.state_dir),
        );
    }
    let mut loader = WorkstreamActivationLoader::production();
    operational_status(
        loader.revalidate_for_tick(),
        read_daemon_status(&runtime_paths.state_dir),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn operational_status(
    activation: WorkstreamActivationState,
    daemon_status: Option<Value>,
) -> WorkLedgerOperationalStatus {
    let runtime_state = daemon_status
        .as_ref()
        .and_then(|status| status.get("workstream_continuation"))
        .and_then(|continuation| continuation.get("state"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let runtime_reason = daemon_status
        .as_ref()
        .and_then(|status| status.get("workstream_continuation"))
        .and_then(|continuation| continuation.get("reason_code"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let continuation_runtime = match (daemon_status.is_some(), runtime_state) {
        (false, _) => "daemon_not_running".to_owned(),
        (true, Some(state)) => state,
        (true, None) => "status_unavailable".to_owned(),
    };

    match activation {
        WorkstreamActivationState::Ready(_) => WorkLedgerOperationalStatus {
            activation_enabled: true,
            dispatch_enabled: true,
            activation_state: "enabled".to_owned(),
            dispatch_state: "enabled".to_owned(),
            continuation_runtime,
            activation_reason_code: None,
            runtime_reason_code: runtime_reason,
        },
        WorkstreamActivationState::Disabled => WorkLedgerOperationalStatus {
            activation_enabled: false,
            dispatch_enabled: false,
            activation_state: "disabled".to_owned(),
            dispatch_state: "disabled".to_owned(),
            continuation_runtime,
            activation_reason_code: None,
            runtime_reason_code: runtime_reason,
        },
        WorkstreamActivationState::Refused(reason) => WorkLedgerOperationalStatus {
            activation_enabled: false,
            dispatch_enabled: false,
            activation_state: "refused".to_owned(),
            dispatch_state: "refused".to_owned(),
            continuation_runtime,
            activation_reason_code: Some(reason.code().to_owned()),
            runtime_reason_code: runtime_reason,
        },
    }
}

fn work_ledger_status_json(
    ledger: &crate::work_ledger::LedgerStatus,
    operational: &WorkLedgerOperationalStatus,
) -> Result<Value, CliFailure> {
    let mut rendered = serde_json::to_value(ledger).map_err(failure)?;
    let Value::Object(fields) = &mut rendered else {
        return Err(CliFailure::new(1, "work ledger status must be an object"));
    };
    fields.insert(
        "activation_enabled".to_owned(),
        Value::Bool(operational.activation_enabled),
    );
    fields.insert(
        "dispatch_enabled".to_owned(),
        Value::Bool(operational.dispatch_enabled),
    );
    fields.insert(
        "activation_state".to_owned(),
        Value::String(operational.activation_state.clone()),
    );
    fields.insert(
        "dispatch_state".to_owned(),
        Value::String(operational.dispatch_state.clone()),
    );
    fields.insert(
        "continuation_runtime".to_owned(),
        Value::String(operational.continuation_runtime.clone()),
    );
    if let Some(reason_code) = &operational.activation_reason_code {
        fields.insert(
            "activation_reason_code".to_owned(),
            Value::String(reason_code.clone()),
        );
    }
    if let Some(reason_code) = &operational.runtime_reason_code {
        fields.insert(
            "runtime_reason_code".to_owned(),
            Value::String(reason_code.clone()),
        );
    }
    Ok(rendered)
}

trait HandshakeActivation {
    fn revalidate(
        &mut self,
    ) -> Result<crate::workstream_activation_loader::ReadyWorkstreamActivation, CliFailure>;
}

struct ProductionHandshakeActivation {
    loader: WorkstreamActivationLoader,
}

impl ProductionHandshakeActivation {
    fn new(runtime_paths: &RuntimePaths) -> Result<Self, CliFailure> {
        let production_paths = RuntimePaths::current(crate::identity::RuntimeMode::Shipyard);
        if runtime_paths != &production_paths {
            return Err(CliFailure::new(
                1,
                "agent handshake is available only against canonical production roots",
            ));
        }
        Ok(Self {
            loader: WorkstreamActivationLoader::production(),
        })
    }
}

impl HandshakeActivation for ProductionHandshakeActivation {
    fn revalidate(
        &mut self,
    ) -> Result<crate::workstream_activation_loader::ReadyWorkstreamActivation, CliFailure> {
        match self.loader.revalidate_for_tick() {
            WorkstreamActivationState::Ready(ready) => Ok(ready),
            WorkstreamActivationState::Disabled => Err(CliFailure::new(
                1,
                "workstream continuation activation is disabled",
            )),
            WorkstreamActivationState::Refused(reason) => Err(CliFailure::new(
                1,
                format!(
                    "workstream continuation activation refused: {}",
                    reason.code()
                ),
            )),
        }
    }
}

fn read_then_revalidate<A, T>(
    activation: &mut A,
    read: impl FnOnce() -> Result<T, CliFailure>,
) -> Result<
    (
        T,
        crate::workstream_activation_loader::ReadyWorkstreamActivation,
    ),
    CliFailure,
>
where
    A: HandshakeActivation,
{
    activation.revalidate()?;
    let value = read()?;
    let ready = activation.revalidate()?;
    Ok((value, ready))
}

fn required_ledger(state_dir: &Path) -> Result<WorkLedger, CliFailure> {
    WorkLedger::open_existing(state_dir)
        .map_err(failure)?
        .ok_or_else(|| CliFailure::new(1, "work ledger is absent"))
}

fn read_private_input(path: &Path) -> Result<Vec<u8>, CliFailure> {
    if path == Path::new("-") {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take(MAX_AGENT_RECEIPT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| CliFailure::new(1, "stdin receipt is unreadable"))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_AGENT_RECEIPT_BYTES {
            return Err(CliFailure::new(1, "stdin receipt exceeds 64 KiB"));
        }
        return Ok(bytes);
    }
    read_private_file(path)
}

fn reserve_private_output(path: &Path) -> Result<std::fs::File, CliFailure> {
    if path == Path::new("-") {
        return Err(CliFailure::new(
            1,
            "protected holder material cannot be written to stdout",
        ));
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .map_err(|_| CliFailure::new(1, "holder output must be a new owner-only file"))
}

fn write_private_output(file: &mut std::fs::File, bytes: &[u8]) -> Result<(), CliFailure> {
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| CliFailure::new(1, "holder output could not be durably written"))
}

fn read_private_file(path: &Path) -> Result<Vec<u8>, CliFailure> {
    let before = std::fs::symlink_metadata(path)
        .map_err(|_| CliFailure::new(1, "receipt file is unreadable"))?;
    if before.file_type().is_symlink() || !before.is_file() {
        return Err(CliFailure::new(1, "receipt path is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if before.permissions().mode() & 0o077 != 0 {
            return Err(CliFailure::new(
                1,
                "receipt file must be private (mode 0600)",
            ));
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    let file = options
        .open(path)
        .map_err(|_| CliFailure::new(1, "receipt file is unreadable"))?;
    let opened = file
        .metadata()
        .map_err(|_| CliFailure::new(1, "receipt file is unreadable"))?;
    if !opened.is_file() || opened.len() > MAX_AGENT_RECEIPT_BYTES {
        return Err(CliFailure::new(1, "receipt file exceeds 64 KiB"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if before.dev() != opened.dev() || before.ino() != opened.ino() {
            return Err(CliFailure::new(1, "receipt file changed while opening"));
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_AGENT_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CliFailure::new(1, "receipt file is unreadable"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_AGENT_RECEIPT_BYTES {
        return Err(CliFailure::new(1, "receipt file exceeds 64 KiB"));
    }
    Ok(bytes)
}

fn write_agent_transition<W: Write>(
    stdout: &mut W,
    json: bool,
    state: &str,
    ownership: &crate::work_ledger::AgentOwnershipReceipt,
    return_challenge: Option<&crate::work_ledger::AgentReturnChallenge>,
) -> Result<(), CliFailure> {
    if json {
        write_pretty_json(
            stdout,
            &serde_json::json!({
                "state": state,
                "ownership_id": ownership.ownership_id,
                "receipt_digest": ownership.receipt_digest,
                "return_challenge": return_challenge,
            }),
        )
        .map_err(failure)
    } else {
        writeln!(stdout, "Agent ownership: {state}").map_err(failure)?;
        writeln!(stdout, "Ownership: {}", ownership.ownership_id).map_err(failure)?;
        writeln!(stdout, "Receipt digest: {}", ownership.receipt_digest).map_err(failure)
    }
}

fn write_publication_report<W: Write>(
    stdout: &mut W,
    report: &NativePublicationReport,
    json: bool,
) -> Result<(), CliFailure> {
    if json {
        write_pretty_json(stdout, report).map_err(failure)
    } else {
        writeln!(
            stdout,
            "Native publication: {}",
            if report.replay {
                "exact replay"
            } else if report.applied {
                "applied"
            } else {
                "dry-run"
            }
        )
        .map_err(failure)?;
        writeln!(stdout, "Work item: {}", report.work_id).map_err(failure)?;
        writeln!(stdout, "Route: {}", report.route_ref).map_err(failure)?;
        writeln!(stdout, "Wake: {}", report.wake_id).map_err(failure)?;
        writeln!(stdout, "Profile digest: {}", report.profile_digest).map_err(failure)?;
        if let Some(reconciliation) = &report.schema11_reconciliation {
            writeln!(
                stdout,
                "Schema reconciliation: {} -> {} (snapshot={})",
                reconciliation.schema_before,
                reconciliation.schema_after,
                reconciliation.snapshot_sha256,
            )
            .map_err(failure)?;
            for item in &reconciliation.items {
                writeln!(
                    stdout,
                    "  {} {}#{} {} workstream={} work={}",
                    item.disposition.as_str(),
                    item.repository,
                    item.pull_request,
                    item.exact_head,
                    item.workstream_handle,
                    item.work_id,
                )
                .map_err(failure)?;
            }
        }
        Ok(())
    }
}

fn policy_command<W: Write>(
    command: &WorkLedgerPolicyCommand,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    match command {
        WorkLedgerPolicyCommand::List => policy_list(state_dir, json, stdout),
        WorkLedgerPolicyCommand::Set { .. } => policy_set(command, state_dir, json, stdout),
    }
}

fn policy_list<W: Write>(state_dir: &Path, json: bool, stdout: &mut W) -> Result<(), CliFailure> {
    let policies = WorkLedger::open_existing(state_dir)
        .map_err(failure)?
        .map_or_else(|| Ok(Vec::new()), |ledger| ledger.repo_policies())
        .map_err(failure)?;
    if json {
        write_pretty_json(
            stdout,
            &serde_json::json!({
                "mode": "shadow",
                "activation_enabled": false,
                "dispatch_enabled": false,
                "policies": policies,
            }),
        )
        .map_err(failure)?;
    } else {
        if policies.is_empty() {
            writeln!(stdout, "No repository policies in the shadow ledger.").map_err(failure)?;
        }
        for policy in policies {
            writeln!(
                stdout,
                "{}: primary={} compatibility={} lanes={:?} blocking={} dependencies={:?} revision={}",
                policy.repo,
                policy.primary_platform,
                policy.compatibility_mode,
                policy.compatibility_lanes,
                policy.blocking_rule,
                policy.declared_dependency_lanes,
                policy.revision
            )
            .map_err(failure)?;
        }
        writeln!(stdout, "Scheduler activation: disabled (shadow only)").map_err(failure)?;
        writeln!(stdout, "Dispatch: disabled (shadow only)").map_err(failure)?;
    }
    Ok(())
}

fn policy_set<W: Write>(
    command: &WorkLedgerPolicyCommand,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let WorkLedgerPolicyCommand::Set {
        repo,
        primary_platform,
        compatibility_mode,
        compatibility_lanes,
        blocking_rule,
        declared_dependency_lanes,
        expected_revision,
        apply,
    } = command
    else {
        unreachable!("policy_set only receives set commands")
    };
    let planned = RepoPolicy {
        repo: repo.clone(),
        primary_platform: primary_platform.clone(),
        compatibility_mode: compatibility_mode.clone(),
        compatibility_lanes: {
            let mut lanes = compatibility_lanes.clone();
            lanes.sort();
            lanes
        },
        blocking_rule: blocking_rule.clone(),
        declared_dependency_lanes: {
            let mut lanes = declared_dependency_lanes.clone();
            lanes.sort();
            lanes
        },
        revision: *expected_revision,
    };
    validate_repo_policy(&planned, *expected_revision).map_err(failure)?;
    let policy = if *apply {
        WorkLedger::open(state_dir)
            .and_then(|ledger| ledger.set_repo_policy(&planned, *expected_revision))
            .map_err(failure)?
    } else {
        WorkLedger::open_existing(state_dir)
            .map_err(failure)?
            .map_or_else(
                || {
                    if *expected_revision != 0 {
                        return Err(failure("repository policy revision no longer matches"));
                    }
                    let mut next = planned.clone();
                    next.revision = 1;
                    Ok(next)
                },
                |ledger| {
                    ledger
                        .plan_repo_policy(&planned, *expected_revision)
                        .map_err(failure)
                },
            )?
    };
    if json {
        write_pretty_json(
            stdout,
            &serde_json::json!({
                "mode": "shadow",
                "activation_enabled": false,
                "dispatch_enabled": false,
                "policy": policy,
            }),
        )
        .map_err(failure)?;
    } else {
        writeln!(
            stdout,
            "{} policy {}: primary={} compatibility={} lanes={:?} blocking={} dependencies={:?} revision={}",
            policy.repo,
            if *apply { "applied" } else { "planned" },
            policy.primary_platform,
            policy.compatibility_mode,
            policy.compatibility_lanes,
            policy.blocking_rule,
            policy.declared_dependency_lanes,
            policy.revision
        )
        .map_err(failure)?;
        writeln!(stdout, "Scheduler activation: disabled (shadow only)").map_err(failure)?;
        writeln!(stdout, "Dispatch: disabled (shadow only)").map_err(failure)?;
    }
    Ok(())
}

fn failure(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(1, error.to_string())
}

#[cfg(test)]
mod tests;
