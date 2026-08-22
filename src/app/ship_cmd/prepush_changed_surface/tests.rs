use super::*;
use crate::changed_surface::{BuildType, RiskClass, TestFamily};
use crate::pr::push_branch_with_env;
use chrono::TimeZone as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

fn loaded_config(global_dir: &Path, merged: toml::Table) -> LoadedConfig {
    LoadedConfig {
        data: merged,
        global_dir: global_dir.to_path_buf(),
        project_dir: None,
        local_dir: None,
        local_overlay_source: crate::config::LocalOverlaySource::None,
    }
}

fn policy() -> ChangedSurfacePolicy {
    ChangedSurfacePolicy {
        schema_version: 2,
        full_test_count: 8,
        build_type: BuildType::Debug,
        build_flags: vec!["PULP_TESTS=ON".to_owned()],
        baseline_tests: vec!["baseline".to_owned()],
        baseline_only_paths: vec!["docs/**".to_owned()],
        full_required_paths: vec!["cmake/**".to_owned()],
        policy_paths: vec![".shipyard/config.toml".to_owned()],
        test_topology_paths: vec!["tests/CMakeLists.txt".to_owned()],
        families: vec![TestFamily {
            name: "dsp".to_owned(),
            paths: vec!["src/**".to_owned()],
            tests: vec!["dsp unit".to_owned()],
            risk_class: RiskClass::Low,
            extended_tests: Vec::new(),
            supported_build_types: vec![BuildType::Debug],
            required_secondary_target: None,
            required_secondary_build_type: None,
        }],
        execution: None,
        secondary_contract_digests: std::collections::BTreeMap::default(),
    }
}

fn input(pull_request: u64) -> ExactHeadInput {
    let base = "1111111111111111111111111111111111111111";
    let head = "2222222222222222222222222222222222222222";
    let tree = "3333333333333333333333333333333333333333";
    ExactHeadInput {
        repository: "owner/repo".to_owned(),
        pull_request,
        target: "mac".to_owned(),
        observed_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        base_ref: "main".to_owned(),
        pr_base_sha: base.to_owned(),
        protected_ref_sha: base.to_owned(),
        protected_ref_status: ProtectedRefStatus::Protected,
        pr_head_sha: head.to_owned(),
        remote_tree_sha: tree.to_owned(),
        local_head_sha: head.to_owned(),
        local_tree_sha: tree.to_owned(),
        local_merge_base_sha: base.to_owned(),
        remote_merge_base_sha: base.to_owned(),
        merge_base_is_ancestor: true,
        checkout_clean: true,
        remote_changed_paths: vec!["src/dsp.cpp".to_owned()],
        remote_changed_paths_status: ObservationStatus::Complete,
        local_changed_paths: vec!["src/dsp.cpp".to_owned()],
        local_changed_paths_status: ObservationStatus::Complete,
        base_tracked_paths: vec!["src/dsp.cpp".to_owned()],
        base_tracked_paths_status: ObservationStatus::Complete,
        secondary_proofs: Vec::new(),
    }
}

fn prospective(state_dir: &Path) -> ProspectivePush {
    let policy = policy();
    let selection =
        plan_selection(&input(PROSPECTIVE_PR_SENTINEL), Ok(policy.clone())).expect("selection");
    let receipt = ProspectiveReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repository: "owner/repo".to_owned(),
        target: "mac".to_owned(),
        protected_base_ref: "main".to_owned(),
        protected_base_sha: "1111111111111111111111111111111111111111".to_owned(),
        head_ref: "refs/heads/feature/prepush".to_owned(),
        head_sha: "2222222222222222222222222222222222222222".to_owned(),
        tree_sha: "3333333333333333333333333333333333333333".to_owned(),
        merge_base_sha: "1111111111111111111111111111111111111111".to_owned(),
        changed_paths_digest: selection.changed_paths_digest.clone(),
        policy_digest: policy_digest(&policy),
        planner_digest: canonical_selection_digest(&selection).expect("planner digest"),
        coverage_contract_digest: policy_digest(&policy),
        inventory_digest: test_inventory_digest(&policy),
        selected_tests_digest: digest_nul(&selection.selected_tests),
        hook_path: ".githooks/pre-push".to_owned(),
        hook_sha256: sha256(b"fixture hook"),
        transaction_nonce: String::new(),
        result_dir: PathBuf::new(),
        selection,
    };
    persist_prospective(state_dir, receipt).expect("persist prospective")
}

fn passing_hook_result(push: &ProspectivePush) -> HookResult {
    HookResult {
        schema_version: HOOK_RESULT_SCHEMA_VERSION,
        transaction_nonce: push.receipt.transaction_nonce.clone(),
        prospective_receipt_sha256: push.receipt_digest.clone(),
        update_count: 1,
        update_ref: push.receipt.head_ref.clone(),
        head_sha: push.receipt.head_sha.clone(),
        tree_sha: push.receipt.tree_sha.clone(),
        selected_tests_digest: push.receipt.selected_tests_digest.clone(),
        hook_sha256: push.receipt.hook_sha256.clone(),
    }
}

#[test]
fn malformed_oversized_and_missing_hook_results_fail_closed() {
    let temp = tempfile::tempdir().expect("temp");
    let push = prospective(temp.path());
    assert!(load_hook_result(&push.receipt.result_dir).is_err());

    fs::write(push.receipt.result_dir.join("hook-result.json"), b"{bad").expect("malformed");
    assert!(load_hook_result(&push.receipt.result_dir).is_err());

    fs::write(
        push.receipt.result_dir.join("hook-result.json"),
        vec![b'x'; usize::try_from(MAX_HOOK_RESULT_BYTES + 1).unwrap()],
    )
    .expect("oversized");
    assert!(load_hook_result(&push.receipt.result_dir).is_err());

    #[cfg(unix)]
    {
        let result = push.receipt.result_dir.join("hook-result.json");
        fs::remove_file(&result).expect("remove oversized result");
        let target = push.receipt.result_dir.join("target.json");
        fs::write(
            &target,
            serde_json::to_vec(&passing_hook_result(&push)).unwrap(),
        )
        .expect("target");
        std::os::unix::fs::symlink(&target, &result).expect("symlink");
        assert!(load_hook_result(&push.receipt.result_dir).is_err());
    }
}

#[test]
fn repository_or_local_config_cannot_enable_trusted_prepush_mode() {
    let temp = tempfile::tempdir().expect("temp");
    let merged = "[changed_surface_prepush]\nmode = 'shadow_compare'\n"
        .parse::<toml::Table>()
        .expect("merged config");
    let config = loaded_config(temp.path(), merged);
    assert_eq!(
        trusted_mode(&config).expect("default mode"),
        PrepushMode::Off
    );

    fs::write(
        temp.path().join("config.toml"),
        "[changed_surface_prepush]\nmode = 'shadow_compare'\n",
    )
    .expect("global config");
    assert_eq!(
        trusted_mode(&config).expect("trusted mode"),
        PrepushMode::ShadowCompare
    );
}

#[test]
fn hook_result_mutations_never_create_a_dedupe_authority() {
    let temp = tempfile::tempdir().expect("temp");
    let push = prospective(temp.path());
    let mut mutations = Vec::new();
    let mut stale = passing_hook_result(&push);
    stale.head_sha = "4444444444444444444444444444444444444444".to_owned();
    mutations.push(stale);
    let mut multi_ref = passing_hook_result(&push);
    multi_ref.update_count = 2;
    mutations.push(multi_ref);
    let mut wrong_ref = passing_hook_result(&push);
    wrong_ref.update_ref = "refs/tags/v1".to_owned();
    mutations.push(wrong_ref);
    let mut wrong_nonce = passing_hook_result(&push);
    wrong_nonce.transaction_nonce = "attacker".to_owned();
    mutations.push(wrong_nonce);
    let mut wrong_digest = passing_hook_result(&push);
    wrong_digest.prospective_receipt_sha256 = sha256(b"different");
    mutations.push(wrong_digest);
    let mut wrong_hook = passing_hook_result(&push);
    wrong_hook.hook_sha256 = sha256(b"branch-controlled hook");
    mutations.push(wrong_hook);

    for mutation in mutations {
        assert!(verify_hook_result(&push, &mutation).is_err());
    }
}

#[test]
fn optional_receipt_persistence_failure_declines_without_error() {
    let temp = tempfile::tempdir().expect("temp");
    let blocked_state = temp.path().join("state-file");
    fs::write(&blocked_state, b"not a directory").expect("blocking file");
    let policy = policy();
    let selection =
        plan_selection(&input(PROSPECTIVE_PR_SENTINEL), Ok(policy.clone())).expect("selection");
    let receipt = ProspectiveReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        repository: "owner/repo".to_owned(),
        target: "mac".to_owned(),
        protected_base_ref: "main".to_owned(),
        protected_base_sha: "1111111111111111111111111111111111111111".to_owned(),
        head_ref: "refs/heads/feature/prepush".to_owned(),
        head_sha: "2222222222222222222222222222222222222222".to_owned(),
        tree_sha: "3333333333333333333333333333333333333333".to_owned(),
        merge_base_sha: "1111111111111111111111111111111111111111".to_owned(),
        changed_paths_digest: selection.changed_paths_digest.clone(),
        policy_digest: policy_digest(&policy),
        planner_digest: canonical_selection_digest(&selection).expect("planner digest"),
        coverage_contract_digest: policy_digest(&policy),
        inventory_digest: test_inventory_digest(&policy),
        selected_tests_digest: digest_nul(&selection.selected_tests),
        hook_path: ".githooks/pre-push".to_owned(),
        hook_sha256: sha256(b"fixture hook"),
        transaction_nonce: String::new(),
        result_dir: PathBuf::new(),
        selection,
    };
    assert!(persist_or_decline(&blocked_state, receipt).is_none());
}

#[test]
fn abandoned_reaper_removes_only_owned_transaction_directories() {
    let temp = tempfile::tempdir().expect("temp");
    let transactions = temp.path().join("transactions");
    fs::create_dir(&transactions).expect("transactions");
    let owned = transactions.join("transaction-old");
    let unrelated = transactions.join("keep-me");
    fs::create_dir(&owned).expect("owned");
    fs::create_dir(&unrelated).expect("unrelated");
    fs::write(transactions.join("transaction-file"), b"keep").expect("file");
    reap_abandoned_transactions(&transactions, Duration::ZERO).expect("reap");
    assert!(!owned.exists());
    assert!(unrelated.exists());
    assert!(transactions.join("transaction-file").exists());
}

#[test]
fn postpush_policy_path_and_test_drift_fail_closed() {
    let temp = tempfile::tempdir().expect("temp");
    let push = prospective(temp.path());
    let policy = policy();
    let observed = plan_selection(&input(42), Ok(policy.clone())).expect("post plan");
    verify_postpush_identity(&push.receipt, &observed, Some(&policy)).expect("equivalent");

    let mut path_drift = input(42);
    path_drift.remote_changed_paths = vec!["src/other.cpp".to_owned()];
    path_drift.local_changed_paths = path_drift.remote_changed_paths.clone();
    let path_receipt = plan_selection(&path_drift, Ok(policy.clone())).expect("path plan");
    assert!(verify_postpush_identity(&push.receipt, &path_receipt, Some(&policy)).is_err());

    let mut changed_policy = policy;
    changed_policy.baseline_tests = vec!["different baseline".to_owned()];
    let policy_receipt =
        plan_selection(&input(42), Ok(changed_policy.clone())).expect("policy plan");
    assert!(
        verify_postpush_identity(&push.receipt, &policy_receipt, Some(&changed_policy)).is_err()
    );
}

#[test]
fn unknown_and_full_required_paths_never_enter_prepush_transport() {
    let policy = policy();
    let unknown = plan_selection(
        &ExactHeadInput {
            remote_changed_paths: vec!["unmapped/new.cpp".to_owned()],
            local_changed_paths: vec!["unmapped/new.cpp".to_owned()],
            ..input(PROSPECTIVE_PR_SENTINEL)
        },
        Ok(policy.clone()),
    )
    .expect("unknown fallback");
    assert_eq!(unknown.planned_suite, PlannedSuite::Full);
    assert!(!selection_is_transportable(&unknown));

    let full_required = plan_selection(
        &ExactHeadInput {
            remote_changed_paths: vec!["cmake/toolchain.cmake".to_owned()],
            local_changed_paths: vec!["cmake/toolchain.cmake".to_owned()],
            ..input(PROSPECTIVE_PR_SENTINEL)
        },
        Ok(policy),
    )
    .expect("full-required fallback");
    assert_eq!(full_required.planned_suite, PlannedSuite::Full);
    assert!(!selection_is_transportable(&full_required));
}

#[test]
fn target_discovery_refuses_missing_and_multiple_selector_targets() {
    let targets = Vec::<ResolvedTarget>::new();
    assert_eq!(
        unique_policy_target("[targets.mac.changed_surface_selection]", &targets),
        None
    );
    // The resolved-target intersection is the caller-input firewall: a
    // protected config cannot make an arbitrary target appear here.
    assert_eq!(
        unique_policy_target(
            "[targets.a.changed_surface_selection]\n[targets.b.changed_surface_selection]",
            &targets
        ),
        None
    );
}

#[test]
fn hook_must_be_covered_by_protected_policy_or_topology() {
    let policy = policy();
    assert!(!policy_covers_hook(&policy, ".githooks/pre-push"));
    assert!(policy_covers_hook(&policy, "tests/CMakeLists.txt"));
    assert!(policy_covers_hook(&policy, ".shipyard/config.toml"));
}

#[test]
fn protected_base_hook_bytes_are_authenticated_before_and_after_push() {
    let temp = tempfile::tempdir().expect("temp");
    let checkout = temp.path().join("checkout");
    run(Command::new("git").args(["init"]).arg(&checkout));
    run(Command::new("git").current_dir(&checkout).args([
        "config",
        "user.email",
        "test@example.com",
    ]));
    run(Command::new("git")
        .current_dir(&checkout)
        .args(["config", "user.name", "Test"]));
    run(Command::new("git")
        .current_dir(&checkout)
        .args(["config", "core.hooksPath", ".githooks"]));
    fs::create_dir(checkout.join(".githooks")).expect("hooks");
    let hook = checkout.join(".githooks/pre-push");
    fs::write(&hook, b"#!/bin/sh\nexit 0\n").expect("hook");
    #[cfg(unix)]
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");
    run(Command::new("git")
        .current_dir(&checkout)
        .args(["add", ".githooks/pre-push"]));
    run(Command::new("git")
        .current_dir(&checkout)
        .args(["commit", "-m", "protected hook"]));
    let base = git(&checkout, &["rev-parse", "HEAD"]).expect("base");
    let mut protected_policy = policy();
    protected_policy
        .policy_paths
        .push(".githooks/pre-push".to_owned());
    let (path, digest) = observe_hook_implementation(&checkout, &base, &protected_policy)
        .expect("authenticated hook");
    assert_eq!(path, ".githooks/pre-push");
    assert_eq!(digest, sha256(b"#!/bin/sh\nexit 0\n"));

    fs::write(&hook, b"#!/bin/sh\n# forged\nexit 0\n").expect("mutated hook");
    assert!(observe_hook_implementation(&checkout, &base, &protected_policy).is_none());
}

#[cfg(unix)]
#[test]
fn push_hook_receipt_and_postpush_equivalence_create_one_verified_snapshot() {
    let temp = tempfile::tempdir().expect("temp");
    let state = temp.path().join("state");
    let mut push = prospective(&state);
    let remote = temp.path().join("remote.git");
    let checkout = temp.path().join("checkout");
    run(Command::new("git").args(["init", "--bare"]).arg(&remote));
    run(Command::new("git").args(["init"]).arg(&checkout));
    run(Command::new("git").current_dir(&checkout).args([
        "config",
        "user.email",
        "test@example.com",
    ]));
    run(Command::new("git")
        .current_dir(&checkout)
        .args(["config", "user.name", "Test"]));
    fs::write(checkout.join("README.md"), "fixture\n").expect("fixture");
    run(Command::new("git")
        .current_dir(&checkout)
        .args(["add", "."]));
    run(Command::new("git")
        .current_dir(&checkout)
        .args(["commit", "-m", "fixture"]));
    run(Command::new("git")
        .current_dir(&checkout)
        .args(["branch", "-M", "feature/prepush"]));
    run(Command::new("git")
        .current_dir(&checkout)
        .args(["remote", "add", "origin"])
        .arg(&remote));
    let hooks = checkout.join(".git/hooks");
    let hook = hooks.join("pre-push");
    fs::write(
            &hook,
            r#"#!/bin/sh
test "$SHIPYARD_PR_RUNNING" = 1 || exit 40
python3 - <<'PY'
import json, os
with open(os.environ['SHIPYARD_CHANGED_SURFACE_PREPUSH_RECEIPT_PATH']) as f:
    receipt = json.load(f)
result = {
    'schema_version': 1,
    'transaction_nonce': os.environ['SHIPYARD_CHANGED_SURFACE_PREPUSH_TRANSACTION_NONCE'],
    'prospective_receipt_sha256': os.environ['SHIPYARD_CHANGED_SURFACE_PREPUSH_RECEIPT_SHA256'],
    'update_count': 1,
    'update_ref': receipt['head_ref'],
    'head_sha': receipt['head_sha'],
    'tree_sha': receipt['tree_sha'],
    'selected_tests_digest': receipt['selected_tests_digest'],
    'hook_sha256': receipt['hook_sha256'],
}
with open(os.path.join(os.environ['SHIPYARD_CHANGED_SURFACE_RESULT_DIR'], 'hook-result.json'), 'w') as f:
    json.dump(result, f, sort_keys=True, separators=(',', ':'))
PY
"#,
        )
        .expect("hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");

    assert!(persist_verified_snapshot(&push, &state, "owner/repo", 42).is_err());
    push_branch_with_env(&checkout, "feature/prepush", push.environment())
        .expect("supervised push");
    push.mark_supervised_push_succeeded();
    let hook_result = load_hook_result(&push.receipt.result_dir).expect("hook receipt");
    verify_hook_result(&push, &hook_result).expect("hook identity");
    let policy = policy();
    let observed = plan_selection(&input(42), Ok(policy.clone())).expect("post plan");
    verify_postpush_identity(&push.receipt, &observed, Some(&policy)).expect("post identity");
    persist_verified_snapshot(&push, &state, "owner/repo", 42).expect("snapshot");
    let snapshot = snapshot_path(
        &state,
        "owner/repo",
        42,
        &push.receipt.head_sha,
        "mac",
        &push.receipt.transaction_nonce,
    );
    let bytes = fs::read(&snapshot).expect("snapshot bytes");
    assert!(String::from_utf8_lossy(&bytes).contains("full_only_due_exact_prepush_shadow"));
    persist_verified_snapshot(&push, &state, "owner/repo", 42).expect("idempotent");

    let mut repeated = prospective(&state);
    repeated.mark_supervised_push_succeeded();
    persist_verified_snapshot(&repeated, &state, "owner/repo", 42).expect("repeat snapshot");
    let repeated_path = snapshot_path(
        &state,
        "owner/repo",
        42,
        &repeated.receipt.head_sha,
        "mac",
        &repeated.receipt.transaction_nonce,
    );
    assert_ne!(snapshot, repeated_path);
    assert!(repeated_path.exists());
}

fn run(command: &mut Command) {
    let output = command.output().expect("spawn fixture command");
    assert!(
        output.status.success(),
        "fixture command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
