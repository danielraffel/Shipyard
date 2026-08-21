use std::process::ExitCode;

#[cfg(unix)]
use super::test_support::{fake_gh, seed_repo_with_local_origin};
use super::test_support::{
    git, git_capture, loaded_config, local_and_unreachable_config, seed_repo,
    unreachable_ssh_config,
};
use super::{
    SHIP_EXIT_VALIDATION_STATE_MISSING, ShipCommandArgs, ShipInvocation, finish_background_ship,
    ship_command,
};
use crate::app::cli::MergeResult;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::job::{Job, JobKind, Priority, TargetResult, TargetStatus, ValidationMode};
use crate::paths::RuntimePaths;
use crate::queue_request::{
    ExecutionProvenance, QueueRequestStore, QueuedExecutionEnvelope, QueuedExecutionOwner,
    QueuedShipDispositionKind,
};
use crate::ship::ShipExecutionRequest;
use crate::ship_state::{ShipState, ShipStateStore};

#[test]
fn auto_create_base_default_matches_python_patterns() {
    assert!(super::should_auto_create_base("develop/next", None));
    assert!(super::should_auto_create_base("release/1.2", None));
    assert!(!super::should_auto_create_base("develop", None));
    assert!(!super::should_auto_create_base("main", None));
    assert!(super::should_auto_create_base("main", Some(true)));
    assert!(!super::should_auto_create_base("develop/next", Some(false)));
}

#[test]
fn daemon_ship_rejects_env_auth_before_remote_resolution() {
    let error = super::validate_daemon_ship_submission(true, false, Some("env"))
        .expect_err("env auth must be rejected");
    assert_eq!(error.code, 2);
    assert!(error.message().contains("source = command"));
    assert!(super::validate_daemon_ship_submission(true, false, Some("command")).is_ok());
}

#[test]
fn ship_command_runs_local_target_merges_and_archives_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    // The issue #321 merge preflight verifies the live PR head matches
    // the validated SHA. Pin the live head to the seeded repo's real HEAD
    // so the happy-path merge proceeds.
    let head = git_capture(&["rev-parse", "HEAD"], &repo);
    let snapshot = temp.path().join("pr.json");
    std::fs::write(&snapshot, format!(r#"{{"headRefOid":"{head}"}}"#)).expect("write snapshot");
    let mut stdout = Vec::new();

    let code = ship_command(
        ShipCommandArgs {
            pr: Some(42),
            base: "main".to_owned(),
            auto_create_base: None,
            no_warm: true,
            resume_from: None,
            merge_command: None,
            merge_result: Some(MergeResult::Success),
            gh_command: None,
            pr_snapshot_file: Some(snapshot),
            allow_unreachable_targets: false,
            allow_fleet_epoch_drift: false,
            skip_targets: Vec::new(),
            adopt_head: false,
            steward_handoff: None,
            invocation: ShipInvocation::Direct,
            foreground: true,
        },
        &loaded_config(temp.path()),
        &repo,
        &paths,
        true,
        &mut stdout,
    )
    .expect("ship command");

    assert_eq!(code, ExitCode::SUCCESS);
    let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
    assert_eq!(output["command"], "ship");
    assert_eq!(output["pr"], 42);
    assert_eq!(output["merged"], true);
    assert_eq!(output["run"]["overall"], "pass");
    assert_eq!(output["ship_state"]["repo"], "danielraffel/pulp");
    assert_eq!(output["ship_state"]["evidence_snapshot"]["mac"], "pass");
    assert!(!paths.state_dir.join("ship").join("42.json").exists());
    assert_eq!(
        std::fs::read_dir(paths.state_dir.join("ship").join("archive"))
            .expect("archive")
            .count(),
        1
    );
}

// Regression coverage for Shipyard issue #296. The synthetic
// `MergeResult::Failure` injects `Err("simulated merge failure")` in
// `merge_pr`. `execute_auto_merge` then evaluates
// `merge_error_confirms_merged(error) || pr_is_merged(...)` as a
// "did the merge actually succeed despite the error?" escape hatch.
// `pr_is_merged` shells out to `gh pr view <pr> --json state` against
// the temp repo's `origin` remote (https://github.com/danielraffel/pulp).
// PR #43 *is* merged in that upstream repo, so on hosts with a fresh
// GraphQL budget `pr_is_merged` returns true and the failure path
// archives the state and returns `Merged` — producing the observed
// `merged: true`. Pinning `--pr-snapshot-file` (via the new
// `pr_snapshot_file` field on `ShipCommandArgs`) keeps `pr_is_merged`
// offline and deterministic.
#[test]
fn ship_command_green_merge_failure_keeps_active_state_and_exits_success() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    // `state:OPEN` keeps the failure-path `pr_is_merged` escape hatch
    // closed; `headRefOid` matching the seeded HEAD lets the issue #321
    // preflight pass so the injected `MergeResult::Failure` is the thing
    // under test.
    let head = git_capture(&["rev-parse", "HEAD"], &repo);
    let snapshot = temp.path().join("pr.json");
    std::fs::write(
        &snapshot,
        format!(r#"{{"state":"OPEN","headRefOid":"{head}"}}"#),
    )
    .expect("write snapshot");
    let mut stdout = Vec::new();

    let code = ship_command(
        ShipCommandArgs {
            pr: Some(43),
            base: "main".to_owned(),
            auto_create_base: None,
            no_warm: true,
            resume_from: None,
            merge_command: None,
            merge_result: Some(MergeResult::Failure),
            gh_command: None,
            pr_snapshot_file: Some(snapshot),
            allow_unreachable_targets: false,
            allow_fleet_epoch_drift: false,
            skip_targets: Vec::new(),
            adopt_head: false,
            steward_handoff: None,
            invocation: ShipInvocation::Direct,
            foreground: true,
        },
        &loaded_config(temp.path()),
        &repo,
        &paths,
        true,
        &mut stdout,
    )
    .expect("ship command");

    assert_eq!(code, ExitCode::SUCCESS);
    let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
    assert_eq!(output["merged"], false);
    assert_eq!(output["run"]["overall"], "pass");
    assert!(
        crate::ship_state::ShipStateStore::new(paths.state_dir.join("ship"))
            .expect("ship-state store")
            .get(43)
            .is_some()
    );
    assert_eq!(
        std::fs::read_dir(paths.state_dir.join("ship").join("archive"))
            .expect("archive")
            .count(),
        0
    );
}

#[test]
fn daemon_finish_selects_repository_when_pr_numbers_collide() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);
    let global_dir = temp.path().join("global");
    let state_dir = temp.path().join("state");
    let config = LoadedConfig::load_from_cwd_with_global_dir(
        RuntimeMode::Isolated,
        &repo,
        global_dir.clone(),
    )
    .expect("load config");
    let head = git_capture(&["rev-parse", "HEAD"], &repo);
    let request = ShipExecutionRequest {
        pr: 42,
        repo: "danielraffel/pulp".to_owned(),
        branch: "feature/test".to_owned(),
        base_branch: "main".to_owned(),
        sha: head.clone(),
        commit_subject: "test collision".to_owned(),
        pr_url: None,
        pr_title: None,
        mode: ValidationMode::Full,
        priority: Priority::Normal,
        warm_disabled: true,
        fail_fast: false,
        resume_from: None,
        advisory_targets: std::collections::BTreeSet::new(),
        adopt_head: false,
        pr_snapshot_file: None,
        targets: Vec::new(),
    };
    let job = Job::create(
        &head,
        &request.branch,
        vec!["mac".to_owned()],
        ValidationMode::Full,
        Priority::Normal,
    )
    .with_kind(JobKind::Ship)
    .start()
    .expect("start job")
    .with_result(TargetResult::new(
        "mac",
        "macos-arm64",
        TargetStatus::Fail,
        "local",
    ))
    .complete()
    .expect("complete job");
    let mut envelope = QueuedExecutionEnvelope::from_ship_request(&job.id, &repo, &request);
    envelope.execution_owner = QueuedExecutionOwner::Daemon;
    envelope.provenance =
        ExecutionProvenance::capture_with_config(&repo, Some(&request.repo), &head, &config);
    QueueRequestStore::new(&state_dir)
        .expect("request store")
        .save(&envelope)
        .expect("save request");

    let store = ShipStateStore::new(state_dir.join("ship")).expect("ship-state store");
    store
        .save(&ShipState::new(
            42,
            "danielraffel/pulp",
            "feature/test",
            "main",
            &head,
            "policy-pulp",
        ))
        .expect("save Pulp state");
    store
        .save(&ShipState::new(
            42,
            "Generous-Corp/forge",
            "feature/modular",
            "main",
            "forge-head",
            "policy-forge",
        ))
        .expect("save Forge state");
    assert!(store.get(42).is_none(), "unscoped lookup must be ambiguous");

    let (code, terminal_state, disposition) = finish_background_ship(
        &request,
        &job,
        RuntimeMode::Isolated,
        &global_dir,
        &state_dir,
    )
    .expect("finish Pulp ship despite colliding Forge PR number");

    assert_eq!(code, ExitCode::from(1));
    assert_eq!(
        disposition.kind,
        QueuedShipDispositionKind::ValidationFailed
    );
    let terminal_state = terminal_state.expect("captured Pulp ship state");
    assert_eq!(terminal_state.repo, "danielraffel/pulp");
    assert_eq!(terminal_state.head_sha, head);
    assert!(store.get_scoped("Generous-Corp/forge", 42).is_some());
}

#[test]
fn daemon_finish_preserves_green_validation_when_ship_state_is_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);
    let global_dir = temp.path().join("global");
    let state_dir = temp.path().join("state");
    let config = LoadedConfig::load_from_cwd_with_global_dir(
        RuntimeMode::Isolated,
        &repo,
        global_dir.clone(),
    )
    .expect("load config");
    let head = git_capture(&["rev-parse", "HEAD"], &repo);
    let snapshot = temp.path().join("pr-open.json");
    std::fs::write(&snapshot, r#"{"state":"OPEN"}"#).expect("write PR snapshot");
    let request = ShipExecutionRequest {
        pr: 7751,
        repo: "danielraffel/pulp".to_owned(),
        branch: "feature/validated".to_owned(),
        base_branch: "main".to_owned(),
        sha: head.clone(),
        commit_subject: "validated locally".to_owned(),
        pr_url: None,
        pr_title: None,
        mode: ValidationMode::Full,
        priority: Priority::Normal,
        warm_disabled: true,
        fail_fast: false,
        resume_from: None,
        advisory_targets: std::collections::BTreeSet::new(),
        adopt_head: false,
        pr_snapshot_file: Some(snapshot),
        targets: Vec::new(),
    };
    let job = Job::create(
        &head,
        &request.branch,
        vec!["mac".to_owned()],
        ValidationMode::Full,
        Priority::Normal,
    )
    .with_kind(JobKind::Ship)
    .start()
    .expect("start job")
    .with_result(TargetResult::new(
        "mac",
        "macos-arm64",
        TargetStatus::Pass,
        "local",
    ))
    .complete()
    .expect("complete job");
    let mut envelope = QueuedExecutionEnvelope::from_ship_request(&job.id, &repo, &request);
    envelope.execution_owner = QueuedExecutionOwner::Daemon;
    envelope.provenance =
        ExecutionProvenance::capture_with_config(&repo, Some(&request.repo), &head, &config);
    QueueRequestStore::new(&state_dir)
        .expect("request store")
        .save(&envelope)
        .expect("save request");

    let (code, terminal_state, disposition) = finish_background_ship(
        &request,
        &job,
        RuntimeMode::Isolated,
        &global_dir,
        &state_dir,
    )
    .expect("missing state is a typed post-validation result");

    assert_eq!(code, ExitCode::from(SHIP_EXIT_VALIDATION_STATE_MISSING));
    assert_eq!(
        disposition.kind,
        QueuedShipDispositionKind::GreenValidationStateMissing
    );
    assert_eq!(disposition.exit_code, SHIP_EXIT_VALIDATION_STATE_MISSING);
    assert!(
        disposition
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("state is missing"))
    );
    assert!(terminal_state.is_none());
    assert!(job.passed(), "local validation proof must remain green");
}

#[test]
fn daemon_finish_refuses_same_head_replacement_validation_identity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);
    let global_dir = temp.path().join("global");
    let state_dir = temp.path().join("state");
    let config = LoadedConfig::load_from_cwd_with_global_dir(
        RuntimeMode::Isolated,
        &repo,
        global_dir.clone(),
    )
    .expect("load config");
    let head = git_capture(&["rev-parse", "HEAD"], &repo);
    let request = ShipExecutionRequest {
        pr: 7751,
        repo: "danielraffel/pulp".to_owned(),
        branch: "feature/validated".to_owned(),
        base_branch: "main".to_owned(),
        sha: head.clone(),
        commit_subject: "validated locally".to_owned(),
        pr_url: None,
        pr_title: None,
        mode: ValidationMode::Full,
        priority: Priority::Normal,
        warm_disabled: true,
        fail_fast: false,
        resume_from: None,
        advisory_targets: std::collections::BTreeSet::new(),
        adopt_head: false,
        pr_snapshot_file: None,
        targets: Vec::new(),
    };
    let job = Job::create(
        &head,
        &request.branch,
        vec!["mac".to_owned()],
        ValidationMode::Full,
        Priority::Normal,
    )
    .with_kind(JobKind::Ship)
    .start()
    .expect("start job")
    .with_result(TargetResult::new(
        "mac",
        "macos-arm64",
        TargetStatus::Pass,
        "local",
    ))
    .complete()
    .expect("complete job A");
    let mut envelope = QueuedExecutionEnvelope::from_ship_request(&job.id, &repo, &request);
    envelope.execution_owner = QueuedExecutionOwner::Daemon;
    envelope.provenance =
        ExecutionProvenance::capture_with_config(&repo, Some(&request.repo), &head, &config);
    QueueRequestStore::new(&state_dir)
        .expect("request store")
        .save(&envelope)
        .expect("save request");

    // Job B reactivated the same PR/head under a different validation policy
    // before job A entered post-validation handling.
    let store = ShipStateStore::new(state_dir.join("ship")).expect("ship-state store");
    let mut replacement = ShipState::new(
        request.pr,
        &request.repo,
        &request.branch,
        &request.base_branch,
        &request.sha,
        "replacement-policy",
    );
    replacement.update_evidence("replacement", "pass");
    store.save(&replacement).expect("replacement state");

    let (code, terminal_state, disposition) = finish_background_ship(
        &request,
        &job,
        RuntimeMode::Isolated,
        &global_dir,
        &state_dir,
    )
    .expect("stale merge phase is a typed green outcome");

    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(disposition.kind, QueuedShipDispositionKind::GreenNotMerged);
    assert!(
        disposition
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("validation policy"))
    );
    assert_eq!(
        terminal_state
            .expect("captured replacement")
            .policy_signature,
        "replacement-policy"
    );
    assert_eq!(
        store
            .get_scoped(&request.repo, request.pr)
            .expect("replacement remains active")
            .policy_signature,
        "replacement-policy"
    );
    assert!(store.list_archived().is_empty());
}

#[test]
fn ship_command_preflight_failure_happens_before_state_mutation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    let mut stdout = Vec::new();

    let error = ship_command(
        ShipCommandArgs {
            pr: Some(44),
            base: "main".to_owned(),
            auto_create_base: None,
            no_warm: true,
            resume_from: None,
            merge_command: None,
            merge_result: Some(MergeResult::Success),
            gh_command: None,
            pr_snapshot_file: None,
            allow_unreachable_targets: false,
            allow_fleet_epoch_drift: false,
            skip_targets: Vec::new(),
            adopt_head: false,
            steward_handoff: None,
            invocation: ShipInvocation::Direct,
            foreground: true,
        },
        &unreachable_ssh_config(temp.path()),
        &repo,
        &paths,
        true,
        &mut stdout,
    )
    .expect_err("preflight should fail");

    assert_eq!(error.code, crate::preflight::EXIT_BACKEND_UNREACHABLE);
    assert!(
        error
            .message
            .contains("Target 'linux' (ssh) is unreachable.")
    );
    assert!(error.message.contains("target has no host configured"));
    assert!(stdout.is_empty());
    assert!(!paths.state_dir.join("queue.json").exists());
    assert!(!paths.state_dir.join("ship").exists());
}

#[test]
fn ship_command_skip_target_excludes_unreachable_target_before_preflight() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    seed_repo(&repo);
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    let head = git_capture(&["rev-parse", "HEAD"], &repo);
    let snapshot = temp.path().join("pr.json");
    std::fs::write(
        &snapshot,
        format!(r#"{{"headRefOid":"{head}","baseRefName":"main"}}"#),
    )
    .expect("write snapshot");
    let mut stdout = Vec::new();

    let code = ship_command(
        ShipCommandArgs {
            pr: Some(45),
            base: "main".to_owned(),
            auto_create_base: None,
            no_warm: true,
            resume_from: None,
            merge_command: None,
            merge_result: Some(MergeResult::Success),
            gh_command: None,
            pr_snapshot_file: Some(snapshot),
            allow_unreachable_targets: false,
            allow_fleet_epoch_drift: false,
            skip_targets: vec!["linux".to_owned()],
            adopt_head: false,
            steward_handoff: None,
            invocation: ShipInvocation::Direct,
            foreground: true,
        },
        &local_and_unreachable_config(temp.path()),
        &repo,
        &paths,
        true,
        &mut stdout,
    )
    .expect("ship command");

    assert_eq!(code, ExitCode::SUCCESS);
    let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
    let evidence = output["ship_state"]["evidence_snapshot"]
        .as_object()
        .expect("evidence");
    assert_eq!(evidence["mac"], "pass");
    assert!(!evidence.contains_key("linux"));
}

#[test]
#[cfg(unix)]
fn ship_command_without_pr_finds_existing_pr_after_preflight_and_push() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let remote = temp.path().join("remote.git");
    seed_repo_with_local_origin(&repo, &remote);
    let gh = temp.path().join("gh");
    let gh_log = temp.path().join("gh.log");
    fake_gh(
        &gh,
        &format!(
            r#"
echo "$@" >> "{}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{{"number":88,"url":"https://github.com/o/r/pull/88","title":"Existing PR","state":"OPEN","headRefName":"feature/test","baseRefName":"main"}}]'
  exit 0
fi
echo "unexpected gh args: $@" >&2
exit 2
"#,
            gh_log.display()
        ),
    );
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    let mut stdout = Vec::new();

    let code = ship_command(
        ShipCommandArgs {
            pr: None,
            base: "main".to_owned(),
            auto_create_base: None,
            no_warm: true,
            resume_from: None,
            merge_command: None,
            merge_result: Some(MergeResult::Success),
            gh_command: Some(gh),
            pr_snapshot_file: None,
            allow_unreachable_targets: false,
            allow_fleet_epoch_drift: false,
            skip_targets: Vec::new(),
            adopt_head: false,
            steward_handoff: None,
            invocation: ShipInvocation::Direct,
            foreground: true,
        },
        &loaded_config(temp.path()),
        &repo,
        &paths,
        true,
        &mut stdout,
    )
    .expect("ship command");

    assert_eq!(code, ExitCode::SUCCESS);
    let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
    assert_eq!(output["pr"], 88);
    assert_eq!(
        output["ship_state"]["pr_url"],
        "https://github.com/o/r/pull/88"
    );
    assert_eq!(output["ship_state"]["pr_title"], "Existing PR");
    assert!(
        String::from_utf8_lossy(
            &crate::supervised::git_supervised()
                .args(["show-ref", "refs/heads/feature/test"])
                .current_dir(&remote)
                .output()
                .expect("show-ref")
                .stdout
        )
        .contains("refs/heads/feature/test")
    );
    assert!(
        std::fs::read_to_string(gh_log)
            .expect("gh log")
            .contains("pr list")
    );
}

#[test]
#[cfg(unix)]
fn ship_command_without_pr_creates_pr_when_none_exists() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    let remote = temp.path().join("remote.git");
    seed_repo_with_local_origin(&repo, &remote);
    std::fs::write(repo.join("feature.txt"), "feature\n").expect("feature");
    git(&["add", "."], &repo);
    git(
        &[
            "commit",
            "-q",
            "-m",
            "Add autopilot",
            "-m",
            "Context\n\nLane-Policy: mac=advisory",
        ],
        &repo,
    );
    let gh = temp.path().join("gh");
    let gh_log = temp.path().join("gh.log");
    fake_gh(
        &gh,
        &format!(
            r#"
echo "$@" >> "{}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  echo 'https://github.com/o/r/pull/89'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo '{{"number":89,"url":"https://github.com/o/r/pull/89","title":"Add autopilot","state":"OPEN","headRefName":"feature/test","baseRefName":"develop/test"}}'
  exit 0
fi
echo "unexpected gh args: $@" >&2
exit 2
"#,
            gh_log.display()
        ),
    );
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(temp.path().join("global")),
        Some(temp.path().join("state")),
    );
    let mut stdout = Vec::new();

    let code = ship_command(
        ShipCommandArgs {
            pr: None,
            base: "develop/test".to_owned(),
            auto_create_base: None,
            no_warm: true,
            resume_from: None,
            merge_command: None,
            merge_result: Some(MergeResult::Success),
            gh_command: Some(gh),
            pr_snapshot_file: None,
            allow_unreachable_targets: false,
            allow_fleet_epoch_drift: false,
            skip_targets: Vec::new(),
            adopt_head: false,
            steward_handoff: None,
            invocation: ShipInvocation::Direct,
            foreground: true,
        },
        &loaded_config(temp.path()),
        &repo,
        &paths,
        true,
        &mut stdout,
    )
    .expect("ship command");

    assert_eq!(code, ExitCode::SUCCESS);
    let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
    assert_eq!(output["pr"], 89);
    assert_eq!(output["ship_state"]["base_branch"], "develop/test");
    assert_eq!(output["ship_state"]["pr_title"], "Add autopilot");
    assert!(
        String::from_utf8_lossy(
            &crate::supervised::git_supervised()
                .args(["show-ref", "refs/heads/develop/test"])
                .current_dir(&remote)
                .output()
                .expect("show-ref")
                .stdout
        )
        .contains("refs/heads/develop/test")
    );
    let log = std::fs::read_to_string(gh_log).expect("gh log");
    assert!(log.contains("pr list"));
    assert!(log.contains("pr create"));
    assert!(log.contains("pr view"));
    assert!(log.contains("Lane-Policy: mac=advisory"));
    assert!(log.contains("## Advisory lanes"));
    assert!(log.contains("`mac` (overridden via Lane-Policy trailer)"));
}
