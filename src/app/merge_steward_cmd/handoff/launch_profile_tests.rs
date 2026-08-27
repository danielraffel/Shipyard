use super::*;
use crate::app::merge_steward_cmd::launch_profile::{
    CheckpointProvenanceV1, LaunchProfileV1, ProviderMetadataV1, RecoveryPolicyV1,
    SessionProvenanceV1, WorktreeProvenanceV1,
};

fn handoff_args() -> StewardHandoffArgs {
    StewardHandoffArgs {
        repo: Some("owner/repo".into()),
        pr: 7,
        head: "a".repeat(40),
        workstream_id: "SY-LF-TEST".into(),
        context_url: Some("https://linear.example/SY-LF-TEST".into()),
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
        provider: ProviderMetadataV1 {
            provider: "opaque-provider".into(),
            account: Some("subscription-a".into()),
            model: Some("model-tier-a".into()),
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
            path: std::env::temp_dir()
                .join("worktrees")
                .join("exact-head")
                .to_string_lossy()
                .into_owned(),
            head_sha: "a".repeat(40),
            lineage_id: "lineage-current-main-7".into(),
        },
        recovery_policy: RecoveryPolicyV1::ExactSessionThenFreshCheckpoint,
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
fn exact_launch_profile_survives_receipt_restart_without_translation() {
    let temp = tempfile::tempdir().expect("temp");
    let args = handoff_args();
    let profile = prepare_launch_profile_candidate(profile("-r"), "owner/repo", &args.head)
        .expect("valid profile");
    let receipt = prepare_handoff_receipt_with_profile(
        None,
        &args,
        "owner/repo",
        "m3",
        Some(route(&args)),
        Some(profile.clone()),
    )
    .expect("receipt");
    let path = temp.path().join("handoff.json");
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
            .expect("profile")
            .profile
            .resume_argv,
        vec!["/opt/provider-router", "agent", "-r", "provider-session-7"]
    );
    assert!(!restarted.wake_consumer_available);
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
fn public_receipt_render_never_projects_private_profile_fields() {
    let args = handoff_args();
    let route = route(&args);
    let mut output = Vec::new();
    render(&args, "owner/repo", Some(&route), "m3", true, &mut output).expect("render");
    let rendered = String::from_utf8(output).expect("UTF-8");
    for private in [
        "/opt/provider-router",
        "provider-session-7",
        "worktrees",
        "subscription-a",
        "model-tier-a",
    ] {
        assert!(
            !rendered.contains(private),
            "projected private field {private}"
        );
    }
}
