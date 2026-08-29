use super::*;
use crate::app::merge_steward_cmd::launch_profile::{
    CheckpointProvenanceV1, ContinuationBootstrapV1, LaunchProfileV1, ProviderMetadataV1,
    RecoveryPolicyV1, SessionProvenanceV1, WorktreeProvenanceV1,
};
use crate::provider_wrapper::ProviderReasoningEffortV1;
use crate::work_ledger::{
    ExactProtectedProfileResolver, ProviderAdapter, ProviderCapability, ProviderLaunchRequest,
    ProviderOutcome, WakeConsumerPolicy, WakeDeliveryResult,
};
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
        goal_managed: true,
        after_handoff: "continue".into(),
        transfer_agent_owner: false,
        apply: false,
    }
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
        },
    }
}

struct DeliveredAdapter;

impl ProviderAdapter for DeliveredAdapter {
    fn capability(&self, provider_id: &str) -> Option<ProviderCapability> {
        (provider_id == "codex").then(|| ProviderCapability {
            adapter_id: "cmux-workstream-v1".to_owned(),
            fresh_agent_launch: true,
            idempotent_launch: true,
        })
    }

    fn authorize(
        &mut self,
        fence: &crate::work_ledger::DeliveryFence,
        _operation: crate::work_ledger::ProviderAuthorizationOperation,
    ) -> Result<crate::work_ledger::DeliveryAuthorization, ProviderOutcome> {
        Ok(crate::work_ledger::DeliveryAuthorization::for_test(
            fence.work_generation,
            fence.owner_generation,
        ))
    }

    fn launch(
        &mut self,
        _request: ProviderLaunchRequest<'_>,
        _authority: crate::work_ledger::DeliveryAuthorization,
    ) -> ProviderOutcome {
        ProviderOutcome::Delivered {
            receipt: b"provider accepted continuation".to_vec(),
        }
    }

    fn reconcile(
        &mut self,
        _fence: &crate::work_ledger::DeliveryFence,
        _authority: crate::work_ledger::DeliveryAuthorization,
    ) -> ProviderOutcome {
        ProviderOutcome::Delivered {
            receipt: b"provider reconciled continuation".to_vec(),
        }
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

#[test]
fn managed_handoff_atomically_publishes_once_before_reporting_transfer() {
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
    assert!(!managed.wake_consumer_available);

    let interrupted = publish_managed_handoff_with_consumer(
        &paths,
        &actions,
        &path,
        managed,
        "owner/repo",
        args.pr,
        &args.head,
        &continuation_activation(),
        |_, _| {
            Err(CliFailure::new(
                1,
                "simulated stop after durable publication",
            ))
        },
    )
    .expect_err("interrupted acceptance remains pending");
    assert!(interrupted.message.contains("simulated stop"));
    let pending = load_handoff(&path)
        .expect("reload pending receipt")
        .expect("pending receipt");
    assert!(!pending.wake_consumer_available);
    let pending_publication = pending
        .native_publication
        .as_ref()
        .expect("durable pending publication");
    assert_eq!(pending_publication.state, NativePublicationStateV1::Pending);
    assert!(!pending_publication.work_id.is_empty());
    assert!(!pending_publication.route_ref.is_empty());
    assert!(!pending_publication.wake_id.is_empty());

    assert_pending_publication_fences_owner_replacement(&args, &pending);

    assert_publication_without_consumer_is_not_transfer(&paths, &actions, &args);

    deliver_pending_native_wake(&paths);

    let published = publish_managed_handoff(
        &paths,
        &actions,
        &path,
        pending,
        "owner/repo",
        args.pr,
        &args.head,
        &continuation_activation(),
    )
    .expect("publication");
    assert!(published.wake_consumer_available);
    assert!(published.native_publication.is_some());
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
    assert_eq!(status.provider_deliveries, 1);
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

fn assert_publication_without_consumer_is_not_transfer(
    paths: &RuntimePaths,
    actions: &GitHubActions,
    args: &StewardHandoffArgs,
) {
    let request = native_publication_request(paths, actions, "owner/repo", args.pr, &args.head)
        .expect("native request");
    let report = WorkLedger::plan_or_apply_native_continuation(
        &paths.state_dir,
        &request,
        &continuation_activation().config,
        true,
    )
    .expect("publish pending wake");
    let unavailable = wait_for_native_consumer_ownership_for(paths, &report, Duration::ZERO)
        .expect_err("publication without a daemon is not transfer");
    assert!(unavailable.message.contains("did not accept"));
}

fn deliver_pending_native_wake(paths: &RuntimePaths) {
    let ledger = WorkLedger::open_existing(&paths.state_dir)
        .expect("open ledger")
        .expect("published ledger");
    let mut resolver =
        ExactProtectedProfileResolver::new(&ledger, crate::app::decode_protected_launch_profile);
    let delivered = ledger
        .consume_one_wake(
            WakeConsumerPolicy {
                activation_enabled: true,
                dispatch_enabled: true,
                authorized_repositories: vec!["owner/repo".to_owned()],
            },
            &mut resolver,
            &mut DeliveredAdapter,
        )
        .expect("consumer delivery");
    assert_eq!(delivered, WakeDeliveryResult::Delivered);
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
        schema_version: 1,
        state: NativePublicationStateV1::Pending,
        work_id: "wi_exact".into(),
        route_ref: "route_exact".into(),
        wake_id: "wake_exact".into(),
        profile_digest,
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
