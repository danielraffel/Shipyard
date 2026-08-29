use super::*;
use crate::app::merge_steward_cmd::launch_profile::{
    CheckpointProvenanceV1, ContinuationBootstrapV1, LaunchProfileV1, ProviderMetadataV1,
    RecoveryPolicyV1, SessionProvenanceV1, WorktreeProvenanceV1,
};
use crate::provider_wrapper::ProviderReasoningEffortV1;
use crate::work_ledger::WorkLedger;
use crate::workstream_continuation_config::{ProviderWrapperConfig, WorkstreamContinuationConfig};
use std::process::Command;
use std::sync::OnceLock;

fn publication_actions(temp: &tempfile::TempDir, head: &str) -> GitHubActions {
    let response = serde_json::json!({
        "state": "OPEN",
        "headRefOid": head,
        "baseRefName": "main",
        "baseRefOid": "b".repeat(40),
    })
    .to_string();
    let source = format!("fn main() {{ print!(\"{{}}\", {response:?}); }}");
    let binary = crate::test_support::compile_native_test_program(temp.path(), "gh", &source);
    GitHubActions::new(temp.path()).with_gh_binary_for_tests(binary)
}

fn seed_repo_policy(paths: &RuntimePaths) {
    WorkLedger::open(&paths.state_dir)
        .expect("ledger")
        .set_repo_policy(
            &crate::work_ledger::RepoPolicy {
                repo: "owner/repo".to_owned(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                revision: 0,
            },
            0,
        )
        .expect("repo policy");
}

struct ActiveWorktreeFixture {
    temp: tempfile::TempDir,
    path: String,
    head: String,
    branch: String,
}

fn active_worktree() -> &'static ActiveWorktreeFixture {
    static FIXTURE: OnceLock<ActiveWorktreeFixture> = OnceLock::new();
    FIXTURE.get_or_init(make_worktree_fixture)
}

fn make_worktree_fixture() -> ActiveWorktreeFixture {
    let temp = tempfile::tempdir().expect("temp Git repository");
    let path = temp.path().canonicalize().expect("canonical temp path");
    let run = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(args)
            .output()
            .expect("run Git fixture command");
        assert!(
            output.status.success(),
            "Git fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("Git fixture output")
            .trim()
            .to_owned()
    };
    run(&["init", "-b", "launch-profile-test"]);
    run(&["config", "user.name", "Shipyard Test"]);
    run(&["config", "user.email", "shipyard-test@example.invalid"]);
    run(&[
        "remote",
        "add",
        "origin",
        "https://github.com/owner/repo.git",
    ]);
    std::fs::write(path.join("fixture"), "launch profile\n").expect("fixture file");
    run(&["add", "fixture"]);
    run(&["commit", "-m", "fixture"]);
    let head = run(&["rev-parse", "HEAD"]);
    let branch = run(&["branch", "--show-current"]);
    let prefix = format!("branch.{branch}.pulpWorktree");
    run(&["config", &format!("{prefix}Status"), "active"]);
    run(&["config", &format!("{prefix}DurableSha"), &head]);
    run(&[
        "config",
        &format!("{prefix}LastPath"),
        path.to_str().expect("UTF-8 fixture path"),
    ]);
    ActiveWorktreeFixture {
        temp,
        path: path.to_string_lossy().into_owned(),
        head,
        branch,
    }
}

fn handoff_args() -> StewardHandoffArgs {
    let fixture = active_worktree();
    StewardHandoffArgs {
        repo: Some("owner/repo".into()),
        pr: 7,
        head: fixture.head.clone(),
        workstream_id: "GEN-43".into(),
        context_url: Some("https://linear.example/GEN-43".into()),
        agent_provider: Some("codex".into()),
        agent_session_id: Some("provider-session-7".into()),
        agent_parent_session_id: None,
        agent_surface_id: None,
        launch_profile: None,
        task_graph: None,
        goal_managed: true,
        after_handoff: "continue".into(),
        transfer_agent_owner: false,
        apply: false,
    }
}

fn pause_task_graph(temp: &tempfile::TempDir) -> std::path::PathBuf {
    let path = temp.path().join("task-graph.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "workstream_id": "GEN-43",
            "revision": 7,
            "handoff_task_id": "steward-pr-7",
            "nodes": [
                {"id": "steward-pr-7", "state": "handed_off"},
                {"id": "land-dependent", "state": "blocked", "depends_on": ["steward-pr-7"]}
            ]
        }))
        .expect("serialize task graph"),
    )
    .expect("write task graph");
    path
}

fn profile(resume_flag: &str) -> LaunchProfileV1 {
    let fixture = active_worktree();
    LaunchProfileV1 {
        schema_version: 1,
        launch_argv: vec![
            "/opt/provider-router".into(),
            "agent".into(),
            "--new".into(),
        ],
        resume_argv: vec![
            "/opt/provider-router".into(),
            "agent".into(),
            resume_flag.into(),
            "provider-session-7".into(),
        ],
        subrouter_executable_sha256: Some("9".repeat(64)),
        route_environment: std::collections::BTreeMap::from([(
            "SUBROUTER_OPAQUE_PROVIDER_ACCOUNT_ID".into(),
            "subscription-a".into(),
        )]),
        provider: ProviderMetadataV1 {
            provider: "opaque-provider".into(),
            account: Some("subscription-a".into()),
            model: Some("model-tier-a".into()),
            reasoning_effort: None,
        },
        session: Some(SessionProvenanceV1 {
            agent_provider: "codex".into(),
            provider_session_id: "provider-session-7".into(),
        }),
        checkpoint: CheckpointProvenanceV1 {
            checkpoint_id: "checkpoint-7".into(),
            generation: 4,
            digest: "b".repeat(64),
        },
        worktree: WorktreeProvenanceV1 {
            repository: "owner/repo".into(),
            path: fixture.path.clone(),
            head_sha: fixture.head.clone(),
            lineage_id: fixture.branch.clone(),
        },
        continuation_bootstrap: Some(ContinuationBootstrapV1 {
            workstream_handle: "GEN-43".into(),
            context_url: Some("https://linear.example/GEN-43".into()),
            plan_sha256: "a".repeat(64),
            root_revision: 0,
            issue_revision: 0,
            projection_revision: 1,
            material_event_revision: 0,
            checkpoint_id: "checkpoint-7".into(),
            checkpoint_generation: 4,
            checkpoint_digest: "b".repeat(64),
            repository: "owner/repo".into(),
            head_sha: fixture.head.clone(),
            expected_resume_context_digest: "c".repeat(64),
            success_continuation_digest: "d".repeat(64),
            failure_continuation_digest: "e".repeat(64),
        }),
        recovery_policy: RecoveryPolicyV1::ExactSessionThenFreshCheckpoint,
    }
}

fn native_profile() -> LaunchProfileV1 {
    let mut profile = profile("-r");
    profile.provider.provider = "codex".into();
    profile.route_environment = std::collections::BTreeMap::from([(
        "SUBROUTER_CODEX_ACCOUNT_ID".into(),
        "subscription-a".into(),
    )]);
    profile.provider.reasoning_effort = Some(ProviderReasoningEffortV1::Medium);
    profile.launch_argv = vec![
        "/opt/subrouter".into(),
        "codex".into(),
        "--model".into(),
        "model-tier-a".into(),
        "-c".into(),
        "model_reasoning_effort=\"medium\"".into(),
    ];
    profile.resume_argv = vec![
        "/opt/subrouter".into(),
        "codex".into(),
        "resume".into(),
        "--model".into(),
        "model-tier-a".into(),
        "-c".into(),
        "model_reasoning_effort=\"medium\"".into(),
        "provider-session-7".into(),
    ];
    profile
}

fn continuation_activation() -> ReadyWorkstreamActivation {
    ReadyWorkstreamActivation {
        machine_tag: "m3".into(),
        config: WorkstreamContinuationConfig {
            origin_machine: "m3".into(),
            repositories: vec!["owner/repo".into()],
            provider_wrapper: ProviderWrapperConfig {
                executable_path: "/opt/shipyard-workstream-provider".into(),
                executable_sha256: "f".repeat(64),
                provider_id: "codex".into(),
                adapter_id: "cmux-workstream-v1".into(),
                deadline_seconds: 30,
                max_stdout_bytes: 65_536,
                max_stderr_bytes: 65_536,
            },
            terminal_trust: Box::new(crate::workstream_continuation_config::TerminalTrustConfig {
                cmux_signing_team_id: "7WLXT3NR37".to_owned(),
            }),
        },
    }
}

fn route(args: &StewardHandoffArgs) -> AgentRouteReference {
    route_at(args, "m3")
}

fn route_at(args: &StewardHandoffArgs, origin: &str) -> AgentRouteReference {
    let agent = resolve_agent_context_with_environment(args, &AgentEnvironment::default())
        .expect("resolve agent")
        .expect("agent route");
    agent_route_reference(&agent, origin)
}

#[test]
fn nonexistent_worktree_provenance_fails_before_publication() {
    let args = handoff_args();
    let mut candidate = profile("-r");
    candidate.worktree.path = active_worktree()
        .temp
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let error = prepare_launch_profile_candidate(candidate, "owner/repo", &args.head)
        .expect_err("nonexistent worktree must fail closed");
    assert!(error.message().contains("path is unavailable"));
}

#[test]
fn equivalent_path_forms_are_compared_after_filesystem_canonicalization() {
    let fixture = active_worktree();
    let nested = fixture.temp.path().join("normalization-probe");
    std::fs::create_dir_all(&nested).expect("normalization probe directory");
    let mut candidate = profile("-r");
    candidate.worktree.path = nested.join("..").to_string_lossy().into_owned();
    prepare_launch_profile_candidate(candidate, "owner/repo", &fixture.head)
        .expect("equivalent path syntax must resolve to the verified worktree root");
}

#[test]
fn claimed_repository_and_head_require_matching_live_git_evidence() {
    let mut wrong_repo = profile("-r");
    wrong_repo.worktree.repository = "owner/forged".into();
    let head = wrong_repo.worktree.head_sha.clone();
    let error = prepare_launch_profile_candidate(wrong_repo, "owner/forged", &head)
        .expect_err("claimed repository cannot override the live origin");
    assert!(error.message().contains("origin does not match"));

    let mut wrong_head = profile("-r");
    wrong_head.worktree.head_sha = "a".repeat(40);
    let error = prepare_launch_profile_candidate(wrong_head, "owner/repo", &"a".repeat(40))
        .expect_err("claimed head cannot override the live worktree HEAD");
    assert!(error.message().contains("HEAD does not match"));
}

#[test]
fn superseded_worktree_provenance_fails_before_publication() {
    let fixture = make_worktree_fixture();
    let prefix = format!("branch.{}.pulpWorktreeStatus", fixture.branch);
    let status = Command::new("git")
        .arg("-C")
        .arg(&fixture.path)
        .args(["config", &prefix, "superseded"])
        .status()
        .expect("mark fixture superseded");
    assert!(status.success());
    let mut candidate = profile("-r");
    candidate.worktree.path = fixture.path;
    candidate.worktree.head_sha = fixture.head;
    candidate.worktree.lineage_id = fixture.branch;
    let expected_head = candidate.worktree.head_sha.clone();
    let error = prepare_launch_profile_candidate(candidate, "owner/repo", &expected_head)
        .expect_err("superseded worktree must fail closed");
    assert!(error.message().contains("lineage is not active"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn exact_launch_profile_survives_receipt_restart_without_translation() {
    let temp = tempfile::tempdir().expect("temp");
    let args = handoff_args();
    let profile = prepare_launch_profile_candidate(native_profile(), "owner/repo", &args.head)
        .expect("valid profile");
    let agent = resolve_agent_context_with_environment(&args, &AgentEnvironment::default())
        .expect("resolve agent")
        .expect("agent route");
    let route = agent_route_reference(&agent, "m3");
    let route_path = temp
        .path()
        .join("merge-steward")
        .join("agent-routes")
        .join(format!("{}.json", route.route_id));
    persist_agent_route(&route_path, &route, &agent).expect("persist private route");
    let receipt = prepare_handoff_receipt_with_profile(
        None,
        &args,
        "owner/repo",
        "m3",
        Some(route),
        Some(profile.clone()),
    )
    .expect("receipt");
    let path = temp
        .path()
        .join("merge-steward")
        .join("handoffs")
        .join(encode_path_segment("owner/repo"))
        .join(format!("pr-{}", args.pr))
        .join(format!("{}.json", args.head));
    persist_handoff(&path, receipt, HandoffPhase::Managed).expect("durable receipt");

    let restarted = load_handoff(&path)
        .expect("reload after restart")
        .expect("receipt remains");
    validate_handoff_receipt_integrity(&restarted, "owner/repo", args.pr, &args.head)
        .expect("restarted receipt integrity");
    let stored = restarted.launch_profile.as_ref().expect("stored profile");
    assert_eq!(stored.profile, profile.profile);
    assert_eq!(stored.generation, 1);
    assert_eq!(stored.revision, 1);
    assert_eq!(
        restarted
            .launch_profile
            .as_ref()
            .expect("profile")
            .profile
            .resume_argv,
        vec![
            "/opt/subrouter",
            "codex",
            "resume",
            "--model",
            "model-tier-a",
            "-c",
            "model_reasoning_effort=\"medium\"",
            "provider-session-7"
        ]
    );
    assert_eq!(
        stored.profile.route_environment,
        std::collections::BTreeMap::from([(
            "SUBROUTER_CODEX_ACCOUNT_ID".to_owned(),
            "subscription-a".to_owned(),
        )])
    );
    assert!(!restarted.wake_consumer_available);
    let paths = RuntimePaths::current_with_overrides(
        crate::identity::RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().to_path_buf()),
    );
    seed_repo_policy(&paths);
    let actions = publication_actions(&temp, &args.head);
    let publication =
        native_publication_request(&paths, &actions, "owner/repo", args.pr, &args.head)
            .expect("normalize native publication");
    assert_eq!(publication.repository, "owner/repo");
    assert_eq!(publication.workstream_handle, "GEN-43");
    assert_eq!(publication.origin_machine, "m3");
    assert_eq!(publication.agent_provider, "codex");
    assert_eq!(publication.agent_session_id, "provider-session-7");
    assert_eq!(publication.profile_provider, "codex");
    assert_eq!(publication.profile_digest, stored.profile_digest);
    assert_eq!(
        hex::encode(Sha256::digest(&publication.protected_profile_bytes)),
        publication.profile_digest
    );
    let terminal = terminal_owner_route(temp.path(), "owner/repo", args.pr, &args.head)
        .expect("valid terminal owner")
        .expect("managed owner");
    let provider_route = terminal.provider_route.expect("provider route");
    assert_eq!(provider_route.profile_digest, stored.profile_digest);
    assert_eq!(provider_route.integrity_hash, stored.integrity_hash);
    assert_eq!(provider_route.generation, 1);
    assert_eq!(provider_route.revision, 1);
    assert_eq!(provider_route.provider, "codex");
    assert_eq!(provider_route.account.as_deref(), Some("subscription-a"));
    assert_eq!(provider_route.model.as_deref(), Some("model-tier-a"));

    std::fs::remove_file(&route_path).expect("remove private agent route");
    assert!(
        native_publication_request(&paths, &actions, "owner/repo", args.pr, &args.head).is_err(),
        "native publication must fail closed after private route loss"
    );
    let unresolved =
        terminal_owner_route_or_unresolved(temp.path(), "owner/repo", args.pr, &args.head)
            .expect("validated public receipt remains an unresolved obligation");
    assert_eq!(unresolved.owner_disposition, "unroutable_private_route");
    assert_eq!(unresolved.route_id, None);
    assert_eq!(unresolved.provider, None);
    assert_eq!(unresolved.terminal_provenance, None);
    assert_eq!(
        unresolved.provider_route,
        Some(ProviderRouteReferenceV1 {
            profile_digest: stored.profile_digest.clone(),
            integrity_hash: stored.integrity_hash.clone(),
            generation: stored.generation,
            revision: stored.revision,
            provider: "codex".to_owned(),
            account: Some("subscription-a".to_owned()),
            model: Some("model-tier-a".to_owned()),
        })
    );
}

fn assert_legacy_publication_migrates(
    paths: &RuntimePaths,
    args: &StewardHandoffArgs,
    path: &Path,
    mut legacy: DurableStewardHandoff,
) {
    legacy.schema_version = 3;
    let publication = legacy.native_publication.as_mut().expect("publication");
    publication.schema_version = 1;
    publication.repo_policy_revision = 0;
    persist_handoff(path, legacy, HandoffPhase::Managed).expect("legacy fixture");
    std::fs::remove_dir_all(
        paths
            .state_dir
            .join("work-ledger")
            .join("native-policy-bindings"),
    )
    .expect("remove new binding fixture");
    migrate_legacy_native_policy_authority(&paths.state_dir, "owner/repo", args.pr, &args.head)
        .expect("migrate legacy exact publication");
    crate::work_ledger::verify_native_policy_binding(
        &paths.state_dir,
        "owner/repo",
        args.pr,
        &args.head,
    )
    .expect("migrated binding");
    let upgraded = load_handoff(path).expect("load").expect("receipt");
    assert_eq!(upgraded.schema_version, 4);
    assert_eq!(
        upgraded
            .native_publication
            .expect("publication")
            .schema_version,
        2
    );
}

#[test]
fn managed_handoff_publishes_inert_authority_without_creating_a_wake() {
    let temp = tempfile::tempdir().expect("temp");
    let paths = RuntimePaths::current_with_overrides(
        crate::identity::RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    seed_repo_policy(&paths);
    let args = handoff_args();
    let actions = publication_actions(&temp, &args.head);
    let agent = resolve_agent_context_with_environment(&args, &AgentEnvironment::default())
        .expect("resolve agent")
        .expect("agent");
    let route = agent_route_reference(&agent, "m3");
    persist_agent_route(&agent_route_path(&paths, &route.route_id), &route, &agent)
        .expect("private route");
    let profile = prepare_launch_profile_candidate(native_profile(), "owner/repo", &args.head)
        .expect("native profile");
    let receipt = prepare_handoff_receipt_with_profile(
        None,
        &args,
        "owner/repo",
        "m3",
        Some(route),
        Some(profile),
    )
    .expect("handoff");
    let path = handoff_path(
        &handoff_directory(&paths, "owner/repo", args.pr),
        &args.head,
    );
    let managed = persist_handoff(&path, receipt, HandoffPhase::Managed).expect("managed");
    assert!(!managed.wake_consumer_available);

    let published = publish_managed_handoff_with_consumer(
        &paths,
        &actions,
        &path,
        managed,
        "owner/repo",
        args.pr,
        &args.head,
        &continuation_activation(),
        |_, _| panic!("ordinary handoff must not await a wake consumer"),
    )
    .expect("inert publication");
    let accepted = load_handoff(&path)
        .expect("reload accepted receipt")
        .expect("accepted receipt");
    assert!(accepted.wake_consumer_available);
    let accepted_publication = accepted
        .native_publication
        .as_ref()
        .expect("durable accepted publication");
    assert_eq!(
        accepted_publication.state,
        NativePublicationStateV1::Accepted
    );
    assert!(!accepted_publication.work_id.is_empty());
    assert!(!accepted_publication.route_ref.is_empty());
    assert!(!accepted_publication.wake_id.is_empty());

    assert_pending_publication_fences_owner_replacement(&args, &accepted);

    assert_eq!(published, accepted);
    assert!(published.wake_consumer_available);
    assert_eq!(published.agent_disposition, AgentDisposition::Continue);
    assert!(!published.pause_required);
    validate_handoff_receipt_integrity(&published, "owner/repo", args.pr, &args.head)
        .expect("published receipt integrity");
    let first_revision = published.revision;

    let replay = publish_managed_handoff(
        &paths,
        &actions,
        &path,
        published,
        "owner/repo",
        args.pr,
        &args.head,
        &continuation_activation(),
    )
    .expect("idempotent replay");
    assert_eq!(replay.revision, first_revision);
    let status = WorkLedger::open_existing(&paths.state_dir)
        .expect("ledger")
        .expect("present")
        .status()
        .expect("status");
    assert_eq!(status.pending_wakes, 0);
    assert_eq!(status.provider_deliveries, 0);

    assert_legacy_publication_migrates(&paths, &args, &path, replay);
}

#[test]
fn provider_outcome_cannot_falsely_pause_a_continue_handoff() {
    let temp = tempfile::tempdir().expect("temp");
    let paths = RuntimePaths::current_with_overrides(
        crate::identity::RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    seed_repo_policy(&paths);
    let args = handoff_args();
    let actions = publication_actions(&temp, &args.head);
    let agent = resolve_agent_context_with_environment(&args, &AgentEnvironment::default())
        .expect("resolve agent")
        .expect("agent");
    let route = agent_route_reference(&agent, "m3");
    persist_agent_route(&agent_route_path(&paths, &route.route_id), &route, &agent)
        .expect("private route");
    let profile = prepare_launch_profile_candidate(native_profile(), "owner/repo", &args.head)
        .expect("native profile");
    let receipt = prepare_handoff_receipt_with_profile(
        None,
        &args,
        "owner/repo",
        "m3",
        Some(route),
        Some(profile),
    )
    .expect("handoff");
    let path = handoff_path(
        &handoff_directory(&paths, "owner/repo", args.pr),
        &args.head,
    );
    let managed = persist_handoff(&path, receipt, HandoffPhase::Managed).expect("managed");
    let published = publish_managed_handoff_with_consumer(
        &paths,
        &actions,
        &path,
        managed,
        "owner/repo",
        args.pr,
        &args.head,
        &continuation_activation(),
        |_, _| Err(CliFailure::new(1, "provider would claim pause")),
    )
    .expect("provider callback is outside disposition authority");
    assert!(published.wake_consumer_available);
    assert_eq!(published.agent_disposition, AgentDisposition::Continue);
    assert!(!published.pause_required);
}

#[test]
fn pause_disposition_recovers_from_pending_publication_exactly_once() {
    let temp = tempfile::tempdir().expect("temp");
    let paths = RuntimePaths::current_with_overrides(
        crate::identity::RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    seed_repo_policy(&paths);
    let mut args = handoff_args();
    args.after_handoff = "pause".into();
    let actions = publication_actions(&temp, &args.head);
    let agent = resolve_agent_context_with_environment(&args, &AgentEnvironment::default())
        .expect("resolve agent")
        .expect("agent");
    let agent_route = agent_route_reference(&agent, "m3");
    persist_agent_route(
        &agent_route_path(&paths, &agent_route.route_id),
        &agent_route,
        &agent,
    )
    .expect("private route");
    let proof = load_pause_proof(&pause_task_graph(&temp), &args.workstream_id)
        .expect("valid dependency boundary");
    let profile = prepare_launch_profile_candidate(native_profile(), "owner/repo", &args.head)
        .expect("native profile");
    let receipt = prepare_handoff_receipt_with_profile_and_disposition(
        None,
        &args,
        "owner/repo",
        "m3",
        Some(agent_route.clone()),
        Some(profile),
        Some(proof.clone()),
    )
    .expect("pause handoff");
    let path = handoff_path(
        &handoff_directory(&paths, "owner/repo", args.pr),
        &args.head,
    );
    let managed = persist_handoff(&path, receipt, HandoffPhase::Managed).expect("managed");
    let request = native_publication_request(&paths, &actions, "owner/repo", args.pr, &args.head)
        .expect("publication request");
    let planned = WorkLedger::plan_or_apply_native_continuation(
        &paths.state_dir,
        &request,
        &continuation_activation().config,
        false,
    )
    .expect("plan publication");
    let pending = bind_native_publication_pending(&path, managed, &planned)
        .expect("crash-window pending receipt");

    let restarted = prepare_handoff_receipt_with_profile_and_disposition(
        Some(pending),
        &args,
        "owner/repo",
        "m3",
        Some(agent_route),
        None,
        None,
    )
    .expect("restart reuses durable profile and proof");
    assert_eq!(restarted.disposition_proof.as_ref(), Some(&proof));
    let accepted = publish_managed_handoff(
        &paths,
        &actions,
        &path,
        restarted,
        "owner/repo",
        args.pr,
        &args.head,
        &continuation_activation(),
    )
    .expect("restart completes publication");
    assert!(accepted.wake_consumer_available);
    assert_eq!(accepted.agent_disposition, AgentDisposition::Pause);
    assert!(accepted.pause_required);
    let revision = accepted.revision;
    let replay = publish_managed_handoff(
        &paths,
        &actions,
        &path,
        accepted,
        "owner/repo",
        args.pr,
        &args.head,
        &continuation_activation(),
    )
    .expect("accepted replay");
    assert_eq!(replay.revision, revision);
    assert_eq!(
        WorkLedger::open_existing(&paths.state_dir)
            .expect("ledger")
            .expect("present")
            .status()
            .expect("status")
            .pending_wakes,
        0
    );
}

fn assert_pending_publication_fences_owner_replacement(
    args: &StewardHandoffArgs,
    pending: &DurableStewardHandoff,
) {
    let mut replacement = args.clone();
    replacement.agent_session_id = Some("replacement-session-8".into());
    replacement.transfer_agent_owner = true;
    let mut replacement_profile = native_profile();
    replacement_profile
        .session
        .as_mut()
        .expect("session")
        .provider_session_id = "replacement-session-8".into();
    *replacement_profile
        .resume_argv
        .last_mut()
        .expect("session argv") = "replacement-session-8".into();
    let replacement_profile =
        prepare_launch_profile_candidate(replacement_profile, "owner/repo", &replacement.head)
            .expect("replacement profile");
    let transfer_error = prepare_handoff_receipt_with_profile(
        Some(pending.clone()),
        &replacement,
        "owner/repo",
        "m5",
        Some(route_at(&replacement, "m5")),
        Some(replacement_profile),
    )
    .expect_err("pending publication fences owner replacement");
    assert!(transfer_error.message.contains("cannot be transferred"));
}

#[test]
fn failed_publication_never_claims_monitoring_transfer() {
    let temp = tempfile::tempdir().expect("temp");
    let paths = RuntimePaths::current_with_overrides(
        crate::identity::RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    let args = handoff_args();
    let actions = publication_actions(&temp, &args.head);
    let agent = resolve_agent_context_with_environment(&args, &AgentEnvironment::default())
        .expect("resolve agent")
        .expect("agent");
    let route = agent_route_reference(&agent, "m3");
    persist_agent_route(&agent_route_path(&paths, &route.route_id), &route, &agent)
        .expect("private route");
    let profile = prepare_launch_profile_candidate(native_profile(), "owner/repo", &args.head)
        .expect("native profile");
    let receipt = prepare_handoff_receipt_with_profile(
        None,
        &args,
        "owner/repo",
        "m3",
        Some(route),
        Some(profile),
    )
    .expect("handoff");
    let path = handoff_path(
        &handoff_directory(&paths, "owner/repo", args.pr),
        &args.head,
    );
    let managed = persist_handoff(&path, receipt, HandoffPhase::Managed).expect("managed");
    let mut wrong_policy = continuation_activation();
    wrong_policy.config.repositories = vec!["owner/other".into()];
    assert!(
        publish_managed_handoff(
            &paths,
            &actions,
            &path,
            managed,
            "owner/repo",
            args.pr,
            &args.head,
            &wrong_policy,
        )
        .is_err()
    );
    let stored = load_handoff(&path)
        .expect("read receipt")
        .expect("managed receipt");
    assert!(!stored.wake_consumer_available);
    assert!(stored.native_publication.is_none());
}

#[test]
fn native_publication_rejects_prompt_bearing_launch_profile() {
    let temp = tempfile::tempdir().expect("temp");
    let args = handoff_args();
    let mut unsafe_profile = native_profile();
    unsafe_profile
        .launch_argv
        .push("raw prompt containing a secret".into());
    let profile = prepare_launch_profile_candidate(unsafe_profile, "owner/repo", &args.head)
        .expect("legacy profile storage remains supported");
    let agent = resolve_agent_context_with_environment(&args, &AgentEnvironment::default())
        .expect("resolve agent")
        .expect("agent route");
    let route = agent_route_reference(&agent, "m3");
    let route_path = temp
        .path()
        .join("merge-steward")
        .join("agent-routes")
        .join(format!("{}.json", route.route_id));
    persist_agent_route(&route_path, &route, &agent).expect("persist private route");
    let receipt = prepare_handoff_receipt_with_profile(
        None,
        &args,
        "owner/repo",
        "m3",
        Some(route),
        Some(profile),
    )
    .expect("receipt");
    let path = temp
        .path()
        .join("merge-steward")
        .join("handoffs")
        .join(encode_path_segment("owner/repo"))
        .join(format!("pr-{}", args.pr))
        .join(format!("{}.json", args.head));
    persist_handoff(&path, receipt, HandoffPhase::Managed).expect("durable receipt");
    let paths = RuntimePaths::current_with_overrides(
        crate::identity::RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().to_path_buf()),
    );
    seed_repo_policy(&paths);
    let actions = publication_actions(&temp, &args.head);
    let error = native_publication_request(&paths, &actions, "owner/repo", args.pr, &args.head)
        .expect_err("native publication must reject raw prompts");
    assert!(error.message().contains("prompt"));
}

#[test]
fn same_owner_restart_reuses_profile_but_rejects_translation() {
    let args = handoff_args();
    let stored =
        prepare_launch_profile_candidate(profile("-r"), "owner/repo", &args.head).expect("profile");
    let mut receipt = prepare_handoff_receipt_with_profile(
        None,
        &args,
        "owner/repo",
        "m3",
        Some(route(&args)),
        Some(stored.clone()),
    )
    .expect("initial receipt");
    receipt.revision = 1;

    let replayed = prepare_handoff_receipt_with_profile(
        Some(receipt.clone()),
        &args,
        "owner/repo",
        "m3",
        Some(route(&args)),
        None,
    )
    .expect("restart replay must use the durable profile without its source file");
    assert_eq!(replayed.launch_profile, receipt.launch_profile);

    let translated =
        prepare_launch_profile_candidate(profile("--resume"), "owner/repo", &args.head)
            .expect("translated candidate");
    let changed = prepare_handoff_receipt_with_profile(
        Some(receipt),
        &args,
        "owner/repo",
        "m3",
        Some(route(&args)),
        Some(translated),
    )
    .expect_err("argv translation must require a new owner generation");
    assert!(changed.message().contains("cannot change or omit"));
}

#[test]
fn hostile_profile_route_fences_and_provenance_fail_integrity() {
    let args = handoff_args();
    let stored =
        prepare_launch_profile_candidate(profile("-r"), "owner/repo", &args.head).expect("profile");
    let mut receipt = prepare_handoff_receipt_with_profile(
        None,
        &args,
        "owner/repo",
        "m3",
        Some(route(&args)),
        Some(stored),
    )
    .expect("receipt");
    receipt.revision = 1;
    validate_handoff_receipt_integrity(&receipt, "owner/repo", args.pr, &args.head)
        .expect("baseline integrity");

    let invalid = |candidate: &DurableStewardHandoff| {
        validate_handoff_receipt_integrity(candidate, "owner/repo", args.pr, &args.head).is_err()
    };
    let mut tampered = receipt.clone();
    tampered
        .launch_profile
        .as_mut()
        .expect("profile")
        .integrity_hash = "0".repeat(64);
    assert!(invalid(&tampered));

    tampered = receipt.clone();
    tampered
        .launch_profile
        .as_mut()
        .expect("profile")
        .generation = 2;
    assert!(invalid(&tampered));

    tampered = receipt.clone();
    tampered.launch_profile.as_mut().expect("profile").revision = 2;
    assert!(invalid(&tampered));

    tampered = receipt.clone();
    tampered.agent_route.as_mut().expect("route").route_id = "route-diverged".into();
    assert!(invalid(&tampered));

    tampered = receipt.clone();
    tampered
        .launch_profile
        .as_mut()
        .expect("profile")
        .profile
        .checkpoint
        .generation = 5;
    assert!(invalid(&tampered));

    tampered = receipt.clone();
    tampered
        .launch_profile
        .as_mut()
        .expect("profile")
        .profile
        .worktree
        .lineage_id = "other-lineage".into();
    assert!(invalid(&tampered));

    tampered = receipt;
    tampered.wake_consumer_available = true;
    assert!(invalid(&tampered));
}

#[test]
fn exact_session_recovery_without_an_agent_route_fails_closed() {
    let mut args = handoff_args();
    args.agent_provider = None;
    args.agent_session_id = None;
    let stored =
        prepare_launch_profile_candidate(profile("-r"), "owner/repo", &args.head).expect("profile");
    let error =
        prepare_handoff_receipt_with_profile(None, &args, "owner/repo", "m3", None, Some(stored))
            .expect_err("exact-session policy needs an exact route");
    assert!(error.message().contains("requires a durable agent route"));

    let mut fresh = profile("--checkpoint");
    fresh.session = None;
    fresh.recovery_policy = RecoveryPolicyV1::FreshCheckpointOnly;
    let fresh =
        prepare_launch_profile_candidate(fresh, "owner/repo", &args.head).expect("fresh profile");
    let mut receipt =
        prepare_handoff_receipt_with_profile(None, &args, "owner/repo", "m3", None, Some(fresh))
            .expect("fresh-only profile needs no route");
    receipt.revision = 1;
    validate_handoff_receipt_integrity(&receipt, "owner/repo", args.pr, &args.head)
        .expect("fresh-only integrity");
}

#[test]
fn provider_session_provenance_must_match_the_exact_agent_route() {
    let args = handoff_args();
    let mut mismatched = profile("-r");
    mismatched
        .session
        .as_mut()
        .expect("session")
        .provider_session_id = "different-session".into();
    let candidate =
        prepare_launch_profile_candidate(mismatched, "owner/repo", &args.head).expect("profile");
    let error = validate_launch_profile_route(Some(&candidate), Some(&route(&args)))
        .expect_err("session mismatch must fail closed");
    assert!(error.message().contains("does not match"));
}

#[test]
fn explicit_owner_transfer_advances_profile_generation_and_revision() {
    let first = handoff_args();
    let first_profile = prepare_launch_profile_candidate(profile("-r"), "owner/repo", &first.head)
        .expect("profile");
    let mut receipt = prepare_handoff_receipt_with_profile(
        None,
        &first,
        "owner/repo",
        "m3",
        Some(route(&first)),
        Some(first_profile),
    )
    .expect("initial receipt");
    receipt.revision = 1;

    let mut replacement = first.clone();
    replacement.agent_session_id = Some("replacement-session-8".into());
    replacement.transfer_agent_owner = true;
    let replacement_profile = prepare_launch_profile_candidate(
        {
            let mut profile = profile("--resume");
            profile
                .session
                .as_mut()
                .expect("session")
                .provider_session_id = "replacement-session-8".into();
            profile.resume_argv[3] = "replacement-session-8".into();
            profile
        },
        "owner/repo",
        &replacement.head,
    )
    .expect("replacement profile");
    let transferred = prepare_handoff_receipt_with_profile(
        Some(receipt),
        &replacement,
        "owner/repo",
        "m5",
        Some(route_at(&replacement, "m5")),
        Some(replacement_profile),
    )
    .expect("explicit transfer");
    assert_eq!(transferred.ownership_generation, 2);
    let stored = transferred.launch_profile.as_ref().expect("profile");
    assert_eq!(stored.generation, 2);
    assert_eq!(stored.revision, 2);
    validate_handoff_receipt_integrity(
        &transferred,
        "owner/repo",
        replacement.pr,
        &replacement.head,
    )
    .expect("transferred profile remains bound to the replacement route");

    let replayed = prepare_handoff_receipt_with_profile(
        Some(transferred.clone()),
        &replacement,
        "owner/repo",
        "m5",
        Some(route_at(&replacement, "m5")),
        None,
    )
    .expect("transferred owner restart reuses the durable replacement profile");
    assert_eq!(replayed, transferred);
}

#[test]
fn explicit_owner_transfer_can_add_the_first_profile_to_a_legacy_receipt() {
    let first = handoff_args();
    let mut legacy = prepare_handoff_receipt_with_profile(
        None,
        &first,
        "owner/repo",
        "m3",
        Some(route(&first)),
        None,
    )
    .expect("legacy profile-less receipt");
    legacy.revision = 1;

    let mut replacement = first;
    replacement.agent_session_id = Some("replacement-session-8".into());
    replacement.transfer_agent_owner = true;
    let mut profile = profile("--resume");
    profile
        .session
        .as_mut()
        .expect("session")
        .provider_session_id = "replacement-session-8".into();
    profile.resume_argv[3] = "replacement-session-8".into();
    let profile = prepare_launch_profile_candidate(profile, "owner/repo", &replacement.head)
        .expect("first profile");
    let upgraded = prepare_handoff_receipt_with_profile(
        Some(legacy),
        &replacement,
        "owner/repo",
        "m5",
        Some(route_at(&replacement, "m5")),
        Some(profile),
    )
    .expect("fenced transfer upgrades the legacy receipt");
    assert_eq!(upgraded.ownership_generation, 2);
    let stored = upgraded.launch_profile.as_ref().expect("profile");
    assert_eq!(stored.generation, 2);
    assert_eq!(stored.revision, 1);
    validate_handoff_receipt_integrity(&upgraded, "owner/repo", replacement.pr, &replacement.head)
        .expect("upgraded receipt integrity");
}

#[test]
fn pending_native_publication_refuses_owner_transfer() {
    let first = handoff_args();
    let first_profile = prepare_launch_profile_candidate(profile("-r"), "owner/repo", &first.head)
        .expect("profile");
    let mut published = prepare_handoff_receipt_with_profile(
        None,
        &first,
        "owner/repo",
        "m3",
        Some(route(&first)),
        Some(first_profile),
    )
    .expect("initial receipt");
    published.revision = 1;
    let profile_digest = published
        .launch_profile
        .as_ref()
        .expect("stored profile")
        .profile_digest
        .clone();
    published.wake_consumer_available = false;
    published.native_publication = Some(NativePublicationReceiptV1 {
        schema_version: 2,
        state: NativePublicationStateV1::Pending,
        work_id: "wi_exact".into(),
        route_ref: "route_exact".into(),
        wake_id: "wake_exact".into(),
        profile_digest,
        repo_policy_revision: 1,
    });

    let mut replacement = first.clone();
    replacement.agent_session_id = Some("replacement-session-8".into());
    replacement.transfer_agent_owner = true;
    let mut replacement_profile = profile("--resume");
    replacement_profile
        .session
        .as_mut()
        .expect("session")
        .provider_session_id = "replacement-session-8".into();
    replacement_profile.resume_argv[3] = "replacement-session-8".into();
    let replacement_profile =
        prepare_launch_profile_candidate(replacement_profile, "owner/repo", &replacement.head)
            .expect("replacement profile");

    let error = prepare_handoff_receipt_with_profile(
        Some(published),
        &replacement,
        "owner/repo",
        "m5",
        Some(route_at(&replacement, "m5")),
        Some(replacement_profile),
    )
    .expect_err("published ownership is immutable");
    assert!(error.message.contains("cannot be transferred"));
}

#[test]
fn public_receipt_render_never_projects_private_profile_fields() {
    let args = handoff_args();
    let route = route(&args);
    let mut output = Vec::new();
    render(
        &args,
        "owner/repo",
        Some(&route),
        "m3",
        true,
        false,
        AgentDisposition::Continue,
        false,
        &mut output,
    )
    .expect("render");
    let rendered = String::from_utf8(output).expect("UTF-8");
    for private in [
        "/opt/provider-router",
        "provider-session-7",
        "worktrees",
        "subscription-a",
        "model-tier-a",
        "SUBROUTER_OPAQUE_PROVIDER_ACCOUNT_ID",
    ] {
        assert!(
            !rendered.contains(private),
            "projected private field {private}"
        );
    }
}
