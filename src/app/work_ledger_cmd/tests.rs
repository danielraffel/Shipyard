use std::collections::VecDeque;
use std::path::PathBuf;

use super::*;
use serde_json::Value;
#[cfg(unix)]
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

use crate::workstream_activation_loader::ReadyWorkstreamActivation;
use crate::workstream_continuation_config::{ProviderWrapperConfig, WorkstreamContinuationConfig};

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
            terminal_trust: Box::new(crate::workstream_continuation_config::TerminalTrustConfig {
                cmux_signing_team_id: "7WLXT3NR37".to_owned(),
            }),
        },
    }
}

#[test]
fn status_reports_enabled_configuration_and_redacted_runtime_truth() {
    let operational = operational_status(
        WorkstreamActivationState::Ready(ready_activation()),
        Some(serde_json::json!({
            "workstream_continuation": {
                "state": "idle",
                "reason_code": "provider_waiting",
                "route_ref": "private-route",
                "wake_id": "private-wake"
            }
        })),
    );
    assert!(operational.activation_enabled);
    assert!(operational.dispatch_enabled);
    assert_eq!(operational.activation_state, "enabled");
    assert_eq!(operational.dispatch_state, "enabled");
    assert_eq!(operational.continuation_runtime, "idle");
    assert_eq!(operational.activation_reason_code, None);
    assert_eq!(
        operational.runtime_reason_code.as_deref(),
        Some("provider_waiting")
    );

    let rendered =
        work_ledger_status_json(&absent_status(), &operational).expect("render operational status");
    assert_eq!(rendered["activation_enabled"], true);
    assert_eq!(rendered["dispatch_enabled"], true);
    assert_eq!(rendered["activation_state"], "enabled");
    assert_eq!(rendered["dispatch_state"], "enabled");
    assert_eq!(rendered["continuation_runtime"], "idle");
    assert!(rendered.get("activation_reason_code").is_none());
    assert_eq!(rendered["runtime_reason_code"], "provider_waiting");
    let bytes = serde_json::to_vec(&rendered).expect("serialize rendered status");
    assert!(
        !bytes
            .windows(b"private-route".len())
            .any(|part| part == b"private-route")
    );
    assert!(
        !bytes
            .windows(b"private-wake".len())
            .any(|part| part == b"private-wake")
    );
}

#[test]
fn status_distinguishes_disabled_refused_and_stopped_daemon() {
    let disabled = operational_status(WorkstreamActivationState::Disabled, None);
    assert!(!disabled.activation_enabled);
    assert!(!disabled.dispatch_enabled);
    assert_eq!(disabled.activation_state, "disabled");
    assert_eq!(disabled.dispatch_state, "disabled");
    assert_eq!(disabled.continuation_runtime, "daemon_not_running");
    assert_eq!(disabled.activation_reason_code, None);
    assert_eq!(disabled.runtime_reason_code, None);

    let old_daemon = operational_status(
        WorkstreamActivationState::Ready(ready_activation()),
        Some(serde_json::json!({
            "pid": 123,
            "shipyard_version": "0.126.2"
        })),
    );
    assert!(old_daemon.activation_enabled);
    assert!(old_daemon.dispatch_enabled);
    assert_eq!(old_daemon.continuation_runtime, "status_unavailable");
    assert_ne!(old_daemon.continuation_runtime, "daemon_not_running");

    let refused = operational_status(
        WorkstreamActivationState::Refused(
            crate::workstream_activation_loader::WorkstreamActivationRefusal::InvalidMachinePolicy,
        ),
        Some(serde_json::json!({
            "workstream_continuation": {
                "state": "refused",
                "reason_code": "stale_daemon_reason"
            }
        })),
    );
    assert!(!refused.activation_enabled);
    assert!(!refused.dispatch_enabled);
    assert_eq!(refused.activation_state, "refused");
    assert_eq!(refused.dispatch_state, "refused");
    assert_eq!(refused.continuation_runtime, "refused");
    assert_eq!(
        refused.activation_reason_code.as_deref(),
        Some("invalid_machine_policy")
    );
    assert_eq!(
        refused.runtime_reason_code.as_deref(),
        Some("stale_daemon_reason")
    );
    let rendered = work_ledger_status_json(&absent_status(), &refused)
        .expect("render simultaneous refusal status");
    assert_eq!(rendered["activation_reason_code"], "invalid_machine_policy");
    assert_eq!(rendered["runtime_reason_code"], "stale_daemon_reason");
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
fn inventory_command_emits_bounded_empty_json_without_creating_state() {
    let temp = TempDir::new().expect("temp");
    let state = temp.path().join("absent-state");
    let paths = RuntimePaths::current_with_overrides(
        crate::identity::RuntimeMode::Shipyard,
        Some(temp.path().join("global")),
        Some(state.clone()),
    );
    let mut output = Vec::new();

    work_ledger_command(
        &WorkLedgerCommand::Inventory,
        &paths,
        temp.path(),
        true,
        &mut output,
    )
    .expect("inventory command");

    let value: Value = serde_json::from_slice(&output).expect("inventory JSON");
    assert_eq!(value["complete"], true);
    assert_eq!(value["truncated"], false);
    assert_eq!(value["limit"], 256);
    assert_eq!(value["items"], serde_json::json!([]));
    assert!(!state.exists());
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)] // One end-to-end CLI fixture for both terminal dispositions.
fn run_terminal_reconciliation_cli_case(closed_unmerged: bool) {
    let temp = TempDir::new().expect("temp");
    let state = temp.path().join("state");
    let mut publication = crate::work_ledger::native_publication_test_request();
    let profile_bytes =
        crate::app::merge_steward_cmd::terminal_reconciliation_test_profile_bytes(&publication);
    publication.profile_digest = hex::encode(Sha256::digest(&profile_bytes));
    publication.protected_profile_bytes = profile_bytes;
    let (mut request, _ledger) =
        crate::work_ledger::terminal_reconciliation_test_fixture_with_request(&state, publication);
    if closed_unmerged {
        request.disposition = crate::work_ledger::TerminalReconciliationDisposition::ClosedUnmerged;
        request.merge_sha = None;
        request.merged_at = None;
        request.closed_at = Some("2026-09-01T13:00:00Z".to_owned());
    }
    let repository_json = serde_json::json!({
        "id": request.repository_id,
        "nameWithOwner": request.repository,
    })
    .to_string();
    let pull_json = if closed_unmerged {
        serde_json::json!({
            "id": request.pull_request_node_id,
            "state": "CLOSED",
            "headRefOid": request.head_sha,
            "baseRefName": request.base_ref,
            "mergeCommit": null,
            "mergedAt": null,
            "closedAt": request.closed_at,
        })
    } else {
        serde_json::json!({
            "id": request.pull_request_node_id,
            "state": "MERGED",
            "headRefOid": request.head_sha,
            "baseRefName": request.base_ref,
            "mergeCommit": {"oid": request.merge_sha},
            "mergedAt": request.merged_at,
            "closedAt": request.merged_at,
        })
    }
    .to_string();
    let source = format!(
        r#"
use std::io::Write as _;

fn main() {{
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let bytes = if args.first().map(String::as_str) == Some("repo") {{
        {repository_json:?}.as_bytes()
    }} else if args.first().map(String::as_str) == Some("pr") {{
        {pull_json:?}.as_bytes()
    }} else {{
        eprintln!("unexpected gh argv: {{}}", args.join(" "));
        std::process::exit(2);
    }};
    std::io::stdout().write_all(bytes).expect("write response");
}}
"#,
    );
    let gh = crate::test_support::compile_native_test_program(temp.path(), "terminal-gh", &source);
    let config = crate::config::LoadedConfig {
        data: "[github.auth]\nsource = 'gh-cli'"
            .parse()
            .expect("ambient test auth config"),
        global_dir: temp.path().join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: crate::config::LocalOverlaySource::None,
    };
    let actions = GitHubActions::from_loaded_config(temp.path(), &config)
        .with_repo_override(&request.repository)
        .with_gh_binary_for_tests(gh);

    let mut applied_output = Vec::new();
    reconcile_terminal_target(
        &state,
        &request.repository,
        request.pull_request,
        &request.head_sha,
        true,
        true,
        &mut applied_output,
        &actions,
    )
    .expect("targeted CLI apply");
    let applied: Value = serde_json::from_slice(&applied_output).expect("apply JSON");
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["replay"], true);

    let mut replay_output = Vec::new();
    reconcile_terminal_target(
        &state,
        &request.repository,
        request.pull_request,
        &request.head_sha,
        true,
        true,
        &mut replay_output,
        &actions,
    )
    .expect("targeted CLI exact replay");
    let replay: Value = serde_json::from_slice(&replay_output).expect("replay JSON");
    assert_eq!(replay["applied"], false);
    assert_eq!(replay["replay"], true);
}

#[cfg(unix)]
#[test]
fn terminal_reconciliation_cli_apply_is_reachable_as_exact_replay() {
    run_terminal_reconciliation_cli_case(false);
    run_terminal_reconciliation_cli_case(true);
}

#[cfg(unix)]
#[test]
fn inventory_human_output_names_immutable_repository_and_canonical_workstream() {
    let temp = TempDir::new().expect("temp");
    let state = temp.path().join("state");
    let paths = RuntimePaths::current_with_overrides(
        crate::identity::RuntimeMode::Shipyard,
        Some(temp.path().join("global")),
        Some(state.clone()),
    );
    let request = crate::work_ledger::native_publication_test_request();
    let policy =
        crate::work_ledger::native_publication_test_policy(vec![request.repository.clone()]);
    let ledger = WorkLedger::open(&state).expect("ledger");
    ledger
        .set_repo_policy(
            &RepoPolicy {
                repo: request.repository.clone(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: vec!["linux".to_owned()],
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                revision: 0,
            },
            0,
        )
        .expect("repository policy");
    WorkLedger::plan_or_apply_native_continuation(&state, &request, &policy, true)
        .expect("native publication");
    let mut output = Vec::new();

    work_ledger_command(
        &WorkLedgerCommand::Inventory,
        &paths,
        temp.path(),
        false,
        &mut output,
    )
    .expect("inventory command");

    let rendered = String::from_utf8(output).expect("human inventory");
    assert!(rendered.contains("github.com:R_test_repository:owner/repo#43"));
    assert!(rendered.contains("workstream=GEN-43"));
    assert_eq!(rendered.lines().count(), 2);
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
        repo_policy_revision: 1,
        schema11_reconciliation: None,
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

#[cfg(unix)]
#[test]
fn correlation_hints_are_private_bounded_and_client_only() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let temp = TempDir::new().expect("temp");
    let hints = temp.path().join("hints.json");
    std::fs::write(
        &hints,
        br#"{"linear_workspace_id":"ws_immutable","linear_root_uuid":"123e4567-e89b-12d3-a456-426614174000","provider_repository_id":"R_immutable"}"#,
    )
    .expect("write hints");
    std::fs::set_permissions(&hints, std::fs::Permissions::from_mode(0o600)).expect("private");
    let parsed = read_correlation_hints(&hints).expect("strict hints");
    assert_eq!(parsed.linear_workspace_id, "ws_immutable");

    std::fs::set_permissions(&hints, std::fs::Permissions::from_mode(0o644)).expect("public");
    assert!(read_correlation_hints(&hints).is_err());
    std::fs::set_permissions(&hints, std::fs::Permissions::from_mode(0o600)).expect("private");
    let link = temp.path().join("hints-link.json");
    symlink(&hints, &link).expect("symlink");
    assert!(read_correlation_hints(&link).is_err());
}
