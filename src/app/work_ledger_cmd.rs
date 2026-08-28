use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

use crate::output::write_pretty_json;
use crate::paths::RuntimePaths;
use crate::work_ledger::{
    AgentReturnExpectation, NativePublicationReport, RepoPolicy, WorkLedger, absent_status,
    apply_legacy_snapshot, plan_legacy_snapshot, validate_repo_policy,
};
use crate::workstream_activation_loader::{WorkstreamActivationLoader, WorkstreamActivationState};

use super::CliFailure;
use super::cli::{WorkLedgerCommand, WorkLedgerPolicyCommand};
use super::merge_steward_cmd::native_publication_request;

const MAX_AGENT_RECEIPT_BYTES: u64 = 64 * 1024;

pub(super) fn work_ledger_command<W: Write>(
    command: &WorkLedgerCommand,
    runtime_paths: &RuntimePaths,
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
            if json {
                write_pretty_json(stdout, &status).map_err(failure)?;
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
                writeln!(stdout, "Activation: disabled (shadow only)").map_err(failure)?;
                writeln!(stdout, "Dispatch: disabled (shadow only)").map_err(failure)?;
            }
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
            let request = native_publication_request(runtime_paths, repo, *pr, head)?;
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
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use super::*;
    use serde_json::Value;
    use tempfile::TempDir;

    use crate::workstream_activation_loader::ReadyWorkstreamActivation;
    use crate::workstream_continuation_config::{
        ProviderWrapperConfig, WorkstreamContinuationConfig,
    };

    struct SequenceActivation(VecDeque<Result<ReadyWorkstreamActivation, &'static str>>);

    impl HandshakeActivation for SequenceActivation {
        fn revalidate(&mut self) -> Result<ReadyWorkstreamActivation, CliFailure> {
            self.0
                .pop_front()
                .expect("activation sequence")
                .map_err(|code| CliFailure::new(1, code))
        }
    }

    fn ready_activation() -> ReadyWorkstreamActivation {
        ReadyWorkstreamActivation {
            machine_tag: "m5".to_owned(),
            config: WorkstreamContinuationConfig {
                origin_machine: "m5".to_owned(),
                repositories: vec!["generous-corp/shipyard".to_owned()],
                provider_wrapper: ProviderWrapperConfig {
                    executable_path: PathBuf::from("/opt/wrapper"),
                    executable_sha256: "a".repeat(64),
                    provider_id: "codex".to_owned(),
                    adapter_id: "cmux-workstream-v1".to_owned(),
                    deadline_seconds: 30,
                    max_stdout_bytes: 1024,
                    max_stderr_bytes: 1024,
                },
            },
        }
    }

    #[test]
    fn planted_blocking_input_activation_drift_refuses_before_mutation_authority() {
        let mut activation = SequenceActivation(VecDeque::from([
            Ok(ready_activation()),
            Err("activation_drift"),
        ]));
        let mut input_completed = false;
        let error = read_then_revalidate(&mut activation, || {
            input_completed = true;
            Ok(vec![b'{', b'}'])
        })
        .expect_err("drift after blocked input");
        assert!(input_completed);
        assert_eq!(error.message(), "activation_drift");
        assert!(activation.0.is_empty());
    }

    #[test]
    fn policy_json_always_reports_shadow_activation_and_dispatch() {
        let temp = TempDir::new().expect("temp");
        for command in [
            WorkLedgerPolicyCommand::List,
            WorkLedgerPolicyCommand::Set {
                repo: "generous-corp/forge".to_owned(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                expected_revision: 0,
                apply: false,
            },
        ] {
            let mut output = Vec::new();
            policy_command(&command, temp.path(), true, &mut output).expect("policy json");
            let value: Value = serde_json::from_slice(&output).expect("valid json");
            assert_eq!(value["mode"], "shadow");
            assert_eq!(value["activation_enabled"], false);
            assert_eq!(value["dispatch_enabled"], false);
        }
    }

    #[test]
    fn publication_json_is_stable_and_exposes_no_private_profile() {
        let report = NativePublicationReport {
            applied: false,
            replay: false,
            work_id: "wi:test".to_owned(),
            route_ref: "route:test".to_owned(),
            wake_id: "wake:test".to_owned(),
            profile_digest: "a".repeat(64),
        };
        let mut output = Vec::new();
        write_publication_report(&mut output, &report, true).expect("publication json");
        let value: Value = serde_json::from_slice(&output).expect("valid json");
        assert_eq!(value["applied"], false);
        assert_eq!(value["replay"], false);
        assert_eq!(value["work_id"], "wi:test");
        assert!(value.get("protected_profile_bytes").is_none());
        assert!(value.get("agent_session_id").is_none());
    }

    #[test]
    fn agent_transition_json_omits_protected_object_location() {
        let ownership = crate::work_ledger::AgentOwnershipReceipt {
            ownership_id: "ao:test".to_owned(),
            receipt_object_ref: "secret-object-location".to_owned(),
            receipt_digest: "a".repeat(64),
        };
        let mut output = Vec::new();
        write_agent_transition(&mut output, true, "context_acknowledged", &ownership, None)
            .expect("transition JSON");
        let value: Value = serde_json::from_slice(&output).expect("valid JSON");
        assert_eq!(value["ownership_id"], "ao:test");
        assert!(value.get("receipt_object_ref").is_none());
        assert!(
            !String::from_utf8(output)
                .expect("UTF-8")
                .contains("secret-object-location")
        );
    }

    #[cfg(unix)]
    #[test]
    fn receipt_reader_requires_private_regular_no_follow_file() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let temp = TempDir::new().expect("temp");
        let receipt = temp.path().join("receipt.json");
        std::fs::write(&receipt, br#"{"schema_version":1}"#).expect("write");
        let mut permissions = std::fs::metadata(&receipt).expect("metadata").permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&receipt, permissions).expect("public mode");
        assert!(read_private_file(&receipt).is_err());

        let mut permissions = std::fs::metadata(&receipt).expect("metadata").permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&receipt, permissions).expect("private mode");
        assert_eq!(
            read_private_file(&receipt).expect("private receipt"),
            br#"{"schema_version":1}"#
        );

        let link = temp.path().join("receipt-link.json");
        symlink(&receipt, &link).expect("symlink");
        assert!(read_private_file(&link).is_err());
    }
}
