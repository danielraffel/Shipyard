use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use crate::cloud::GitHubActions;
use crate::custody_transport::{
    MAX_CUSTODY_WIRE_BYTES, handle_incoming_request, incoming_peer_evidence_from_environment,
    load_custody_transport_policy,
};
use crate::daemon_ipc::read_daemon_status;
use crate::output::write_pretty_json;
use crate::paths::RuntimePaths;
use crate::work_ledger::{
    AgentReturnExpectation, CustodyStatus, NativePublicationReport, RepoPolicy, WorkLedger,
    absent_status, apply_legacy_snapshot, local_work_inventory, plan_legacy_snapshot,
    validate_repo_policy,
};
use crate::workstream_activation_loader::{WorkstreamActivationLoader, WorkstreamActivationState};

use super::CliFailure;
use super::cli::{WorkLedgerCommand, WorkLedgerPolicyCommand};
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
            let status = WorkLedger::open_existing(state_dir)
                .map_err(failure)?
                .map_or_else(|| Ok(absent_status()), |ledger| ledger.status())
                .map_err(failure)?;
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
        WorkLedgerCommand::AcknowledgeContext { wake, receipt } => {
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
            let ownership = ledger
                .acknowledge_agent_context(wake, &bytes)
                .map_err(failure)?;
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
        } => {
            if expectation == Path::new("-") && receipt == Path::new("-") {
                return Err(CliFailure::new(
                    1,
                    "expectation and receipt cannot both read from stdin",
                ));
            }
            let mut activation = ProductionHandshakeActivation::new(runtime_paths)?;
            let ((expectation_bytes, receipt_bytes), ready) =
                read_then_revalidate(&mut activation, || {
                    Ok((
                        read_private_input(expectation)?,
                        read_private_input(receipt)?,
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
                .return_agent_ownership(
                    ownership,
                    &expected.delivery_id,
                    expected.work_generation,
                    &expected,
                    &receipt_bytes,
                )
                .map_err(failure)?;
            write_agent_transition(stdout, json, "ownership_returned", &returned, None)?;
        }
        WorkLedgerCommand::Policy { command } => {
            policy_command(command, state_dir, json, stdout)?;
        }
    }
    Ok(ExitCode::SUCCESS)
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
        writeln!(stdout, "Profile digest: {}", report.profile_digest).map_err(failure)
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
