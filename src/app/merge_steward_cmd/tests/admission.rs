use super::*;

fn managed_pr(head: &str) -> ObservedPr {
    ObservedPr {
        node_id: "PR_managed".to_owned(),
        fact: StewardPullRequest {
            number: 42,
            head_sha: head.to_owned(),
            head_branch: "feature".to_owned(),
            draft: false,
            merge_state: "BLOCKED".to_owned(),
            auto_merge_active: false,
            queue_position: None,
            labels: vec![MANAGED_LABEL.to_owned()],
            checks: vec![StewardCheck {
                name: HANDOFF_CONTEXT.to_owned(),
                source: crate::merge_steward::StewardCheckSource::StatusContext,
                app_id: None,
                status: "COMPLETED".to_owned(),
                conclusion: Some("SUCCESS".to_owned()),
                run_id: None,
                observed_at: Some("2026-08-15T17:00:00Z".to_owned()),
            }],
        },
        check_rollup_maybe_truncated: false,
    }
}

fn admission_observation() -> RepoObservation {
    let mut observation = observation_for(managed_pr(&"b".repeat(40)), true);
    observation.runs = vec![StewardRun {
        id: 9001,
        workflow_id: 77,
        run_attempt: 1,
        workflow: "Build and Test".to_owned(),
        head_sha: "a".repeat(40),
        head_branch: "feature".to_owned(),
        status: "queued".to_owned(),
        event: "pull_request".to_owned(),
        pull_request_number: Some(42),
        created_at: "2026-08-15T16:00:00Z".to_owned(),
        jobs: Vec::new(),
    }];
    observation
}

fn clean_admission_actions(temp: &tempfile::TempDir, calls: &Path) -> GitHubActions {
    fake_gh(
        temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  "api repos/owner/repo") printf '%s' '{{"full_name":"owner/repo","allow_auto_merge":true}}' ;;
  *"rules/branches/main --paginate --slurp"*) printf '%s' '[[]]' ;;
  *"branches/main/protection/required_status_checks"*) printf '%s' '{{"contexts":[],"checks":[]}}' ;;
  "api graphql "*) printf '%s' '{{"data":{{"repository":{{"mergeQueue":null}}}}}}' ;;
  "pr list "*) printf '%s' '[]' ;;
  *"actions/runs?status="*) printf '%s' '{{"workflow_runs":[]}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            calls.display()
        ),
    )
    .with_repo_override("owner/repo")
}

fn typed_verdict(exit: ExitCode, output: &[u8], code: u8, reason: &str) -> Value {
    assert_eq!(exit, ExitCode::from(code));
    let verdict: Value = serde_json::from_slice(output).expect("flat JSON");
    assert_eq!(verdict["reason"], reason);
    verdict
}

#[test]
fn admission_target_normalizes_case_and_rejects_ambiguous_labels() {
    let normalized = normalize_admission_target(&AdmissionCleanArgs {
        repo: "Owner/repo".to_owned(),
        base: "main".to_owned(),
        labels: vec![
            "self-hosted".to_owned(),
            "ARM64".to_owned(),
            "macOS".to_owned(),
        ],
        apply: false,
    })
    .expect("valid target");
    assert_eq!(normalized.2, vec!["arm64", "macos", "self-hosted"]);

    for labels in [
        vec!["ARM64".to_owned()],
        vec!["self-hosted".to_owned(), "SELF-HOSTED".to_owned()],
        vec!["self-hosted".to_owned(), String::new()],
    ] {
        assert!(
            normalize_admission_target(&AdmissionCleanArgs {
                repo: "owner/repo".to_owned(),
                base: "main".to_owned(),
                labels,
                apply: false,
            })
            .is_err()
        );
    }
}

#[test]
fn admission_candidates_filter_superseded_runs_by_exact_runner_labels() {
    let temp = tempfile::tempdir().expect("temp");
    let matching = fake_gh(
        &temp,
        r#"
case "$*" in
  *"actions/runs/9001/jobs"*)
    printf '%s' '{"jobs":[{"name":"macOS","status":"queued","conclusion":null,"labels":["self-hosted","macOS","ARM64","pulp-build-vm","shipyard-recovery-pool"],"runner_name":null}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    )
    .with_repo_override("owner/repo");
    let labels = [
        "arm64",
        "macos",
        "pulp-build-vm",
        "self-hosted",
        "shipyard-recovery-pool",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let (candidates, blocking_only) =
        admission_candidates(&matching, &admission_observation(), &labels)
            .expect("matching candidate");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].run_id, 9001);
    assert!(blocking_only.is_empty());

    let nonmatching = fake_gh(
        &temp,
        r#"
case "$*" in
  *"actions/runs/9001/jobs"*)
    printf '%s' '{"jobs":[{"name":"other","status":"queued","conclusion":null,"labels":["self-hosted","Linux"],"runner_name":null}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    )
    .with_repo_override("owner/repo");
    let (candidates, blocking_only) =
        admission_candidates(&nonmatching, &admission_observation(), &labels)
            .expect("nonmatching candidate");
    assert!(candidates.is_empty());
    assert!(blocking_only.is_empty());
}

#[test]
fn typed_admission_output_matches_tartci_flat_contract() {
    let mut output = Vec::new();
    let exit = emit_admission_verdict(
        &mut output,
        true,
        AdmissionVerdict::Defer,
        "stale_compatible_runs",
        "Generous-Corp/pulp",
        "main",
        &["arm64".to_owned(), "self-hosted".to_owned()],
        &[9001],
        None,
    )
    .expect("render verdict");
    assert_eq!(exit, ExitCode::from(3));
    let value: Value = serde_json::from_slice(&output).expect("flat JSON");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["command"], "runner:admission-clean");
    assert_eq!(value["verdict"], "defer");
    assert_eq!(value["reason"], "stale_compatible_runs");
    assert_eq!(value["blocker_run_ids"], json!([9001]));
    assert!(value.get("data").is_none(), "must not use generic envelope");
    let error = bounded_admission_error(&"x".repeat(5_000));
    assert!(error.len() <= 4 * 1024 + 14 && error.ends_with("...[truncated]"));
}

#[test]
fn final_authority_fence_detects_head_advance_with_old_claimable_work() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  "api repos/owner/repo") printf '%s' '{"full_name":"owner/repo","allow_auto_merge":true}' ;;
  *"rules/branches/main --paginate --slurp"*) printf '%s' '[[]]' ;;
  *"branches/main/protection/required_status_checks"*) printf '%s' '{"contexts":[],"checks":[]}' ;;
  "api graphql "*) printf '%s' '{"data":{"repository":{"mergeQueue":null}}}' ;;
  "pr list "*)
    test -e .pr-authority-seen && head=cccccccccccccccccccccccccccccccccccccccc || { : > .pr-authority-seen; head=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb; }
    printf '%s' '[{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"HEAD","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{"name":"shipyard:managed"}],"statusCheckRollup":[{"__typename":"StatusContext","context":"shipyard/steward-handoff","state":"SUCCESS","createdAt":"2026-08-30T00:00:00Z"}]}]' | sed "s/HEAD/$head/" ;;
  *"actions/runs?status=in_progress"*)
    printf '%s' '{"workflow_runs":[{"id":9001,"workflow_id":77,"run_attempt":1,"name":"Build and Test","head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","head_branch":"feature","status":"in_progress","event":"pull_request","pull_requests":[{"number":42}],"created_at":"2026-08-30T00:00:00Z"}]}' ;;
  *"actions/runs?status="*) printf '%s' '{"workflow_runs":[]}' ;;
  *"actions/runs/9001/jobs"*)
    printf '%s' '{"jobs":[{"name":"macOS","status":"queued","conclusion":null,"labels":["self-hosted","macOS","ARM64"],"runner_name":null}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    )
    .with_repo_override("owner/repo");
    let mut observation = admission_observation();
    observation.runs[0].status = "in_progress".to_owned();
    let labels = ["arm64", "macos", "self-hosted"].map(str::to_owned);
    let (cancellable, blocking) =
        admission_candidates(&actions, &observation, &labels).expect("candidate classification");
    assert!(cancellable.is_empty(), "running workflow must not cancel");
    assert_eq!(blocking, vec![9001]);

    let error = observe_admission_candidates(&actions, "owner/repo", "main", &labels)
        .expect_err("fenced observation must reject head drift");
    assert!(error.contains("authority changed"), "{error}");
}

#[cfg(unix)]
#[test]
#[ignore = "subprocess helper for admission observation lock"]
fn admission_observation_lock_child() {
    let Ok(state) = std::env::var("SHIPYARD_ADMISSION_LOCK_CHILD_STATE") else {
        return;
    };
    let ready = PathBuf::from(std::env::var("SHIPYARD_ADMISSION_LOCK_CHILD_READY").unwrap());
    let release = PathBuf::from(std::env::var("SHIPYARD_ADMISSION_LOCK_CHILD_RELEASE").unwrap());
    let labels = vec!["arm64".to_owned(), "self-hosted".to_owned()];
    let _lock =
        try_acquire_admission_observation_lock(Path::new(&state), "Owner/Repo", "main", &labels)
            .expect("child lock")
            .expect("child owns lock");
    fs::write(ready, b"ready").expect("ready marker");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !release.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::process::exit(87);
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn cross_process_contender_defers_and_owner_death_forces_fresh_observation() {
    let temp = tempfile::tempdir().expect("temp");
    let control = mutation_control(&temp, "studio", "studio");
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(control.global_dir),
        Some(control.state_dir),
    );
    let ready = temp.path().join("ready");
    let release = temp.path().join("release");
    let calls = temp.path().join("github-called");
    let test_module = module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, name)| name);
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{test_module}::admission_observation_lock_child"),
            "--ignored",
        ])
        .env("SHIPYARD_ADMISSION_LOCK_CHILD_STATE", &paths.state_dir)
        .env("SHIPYARD_ADMISSION_LOCK_CHILD_READY", &ready)
        .env("SHIPYARD_ADMISSION_LOCK_CHILD_RELEASE", &release)
        .spawn()
        .expect("spawn lock owner");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() && Instant::now() < deadline {
        assert!(child.try_wait().unwrap().is_none(), "child exited early");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "child did not acquire lock");

    let actions = fake_gh(&temp, &format!(": > '{}'; exit 2", calls.display()));
    let mut args = AdmissionCleanArgs {
        repo: "owner/repo".to_owned(),
        base: "main".to_owned(),
        labels: vec!["self-hosted".to_owned(), "ARM64".to_owned()],
        apply: false,
    };
    let run = |args: &AdmissionCleanArgs, actions: &GitHubActions, output: &mut Vec<u8>| {
        admission_clean_command(
            args,
            temp.path(),
            RuntimeMode::Isolated,
            &paths,
            actions,
            true,
            output,
        )
    };
    let mut output = Vec::new();
    let exit = run(&args, &actions, &mut output).expect("typed contention verdict");
    typed_verdict(exit, &output, 3, "observation_in_progress");
    assert!(!calls.exists(), "contender must not call GitHub");

    fs::write(&release, b"release").expect("release child");
    assert_eq!(child.wait().expect("child exit").code(), Some(87));
    let actions = clean_admission_actions(&temp, &calls);
    output.clear();
    let exit = run(&args, &actions, &mut output).expect("fresh successor observation");
    assert_eq!(exit, ExitCode::SUCCESS);
    let verdict: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(verdict["verdict"], "admit");
    assert!(
        fs::read_to_string(&calls)
            .unwrap()
            .contains("api repos/owner/repo")
    );

    let hang_temp = tempfile::tempdir().expect("hang temp");
    let hanging = fake_gh(&hang_temp, "sleep 5")
        .with_repo_override("owner/repo")
        .with_absolute_deadline(Instant::now() + Duration::from_millis(100));
    let timeout_started = Instant::now();
    output.clear();
    let exit = run(&args, &hanging, &mut output).expect("typed timeout verdict");
    assert!(timeout_started.elapsed() < Duration::from_secs(1));
    let verdict = typed_verdict(exit, &output, 1, "observation_failed");
    assert!(verdict["error"].as_str().unwrap().contains("timed out"));
    fs::remove_file(&calls).expect("reset successor calls");
    output.clear();
    assert_eq!(
        run(&args, &actions, &mut output).unwrap(),
        ExitCode::SUCCESS
    );
    assert!(calls.exists(), "successor must reacquire and call GitHub");

    let steward = acquire_ledger_lock(&paths.state_dir.join("merge-steward.json"))
        .expect("active steward lock");
    output.clear();
    let exit = run(&args, &actions, &mut output).expect("typed ledger contention verdict");
    let verdict = typed_verdict(exit, &output, 3, "stewardship_in_progress");
    assert!(
        verdict["error"]
            .as_str()
            .unwrap()
            .contains("already running")
    );

    let mut ledger = StewardLedger::default();
    ledger
        .pending_cancellations
        .insert("pending".to_owned(), pending_cancellation_record());
    save_ledger(&paths.state_dir.join("merge-steward.json"), &ledger).expect("seed pending");
    args.apply = true;
    output.clear();
    let exit = run(&args, &actions, &mut output).expect("typed apply contention verdict");
    typed_verdict(exit, &output, 3, "stewardship_in_progress");
    drop(steward);
    let lock_path = paths.state_dir.join("merge-steward.json.lock");
    fs::remove_file(&lock_path).expect("remove lock file");
    fs::create_dir(&lock_path).expect("replace lock with directory");
    output.clear();
    let exit = run(&args, &actions, &mut output).expect("typed ledger failure verdict");
    let verdict = typed_verdict(exit, &output, 1, "mutation_failed");
    assert!(
        verdict["error"]
            .as_str()
            .unwrap()
            .contains("could not open")
    );
}
