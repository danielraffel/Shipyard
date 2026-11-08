use super::*;

fn managed_pr(head: &str) -> ObservedPr {
    ObservedPr {
        node_id: "PR_managed".to_owned(),
        owner: Some("team-lead".to_owned()),
        fact: StewardPullRequest {
            number: 42,
            head_sha: head.to_owned(),
            author: None,
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
                check_id: None,
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

/// A fake GitHub whose single managed pull request's head is chosen by
/// `head_script` on every `pr list` read.
fn moving_head_actions(temp: &tempfile::TempDir, head_script: &str) -> GitHubActions {
    fake_gh(
        temp,
        &format!(
            r#"
case "$*" in
  "api repos/owner/repo") printf '%s' '{{"full_name":"owner/repo","allow_auto_merge":true}}' ;;
  *"rules/branches/main --paginate --slurp"*) printf '%s' '[[]]' ;;
  *"branches/main/protection/required_status_checks"*) printf '%s' '{{"contexts":[],"checks":[]}}' ;;
  "api graphql "*) printf '%s' '{{"data":{{"repository":{{"mergeQueue":null}}}}}}' ;;
  "pr list "*)
    {head_script}
    printf '%s' '[{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"HEAD","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{{"name":"shipyard:managed"}}],"statusCheckRollup":[{{"__typename":"StatusContext","context":"shipyard/steward-handoff","state":"SUCCESS","createdAt":"2026-08-30T00:00:00Z"}}]}}]' | sed "s/HEAD/$head/" ;;
  *"actions/runs?status=in_progress"*)
    printf '%s' '{{"workflow_runs":[{{"id":9001,"workflow_id":77,"run_attempt":1,"name":"Build and Test","head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","head_branch":"feature","status":"in_progress","event":"pull_request","pull_requests":[{{"number":42}}],"created_at":"2026-08-30T00:00:00Z"}}]}}' ;;
  *"actions/runs?status="*) printf '%s' '{{"workflow_runs":[]}}' ;;
  *"actions/runs/9001/jobs"*)
    printf '%s' '{{"jobs":[{{"name":"macOS","status":"queued","conclusion":null,"labels":["self-hosted","macOS","ARM64"],"runner_name":null}}]}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#
        ),
    )
    .with_repo_override("owner/repo")
}

#[test]
fn final_authority_fence_detects_head_advance_with_old_claimable_work() {
    let temp = tempfile::tempdir().expect("temp");
    // The observation saw head b; the fence's fresh read sees c.
    let actions = moving_head_actions(&temp, "head=cccccccccccccccccccccccccccccccccccccccc");
    let mut observation = admission_observation();
    observation.runs[0].status = "in_progress".to_owned();
    let labels = ["arm64", "macos", "self-hosted"].map(str::to_owned);
    let (cancellable, blocking) =
        admission_candidates(&actions, &observation, &labels).expect("candidate classification");
    assert!(cancellable.is_empty(), "running workflow must not cancel");
    assert_eq!(blocking, vec![9001]);

    let error = fence_admission_authority(&actions, &observation)
        .expect_err("the fence alone must reject head drift");
    assert_eq!(error, ADMISSION_AUTHORITY_CHANGED);
}

#[test]
fn a_plan_made_under_moved_authority_is_remade_from_fresh_reads() {
    let temp = tempfile::tempdir().expect("temp");
    // One push lands during the first inspection; the authority then holds.
    let actions = moving_head_actions(
        &temp,
        "test -e .pr-authority-seen && head=cccccccccccccccccccccccccccccccccccccccc || { : > .pr-authority-seen; head=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb; }",
    );
    let labels = ["arm64", "macos", "self-hosted"].map(str::to_owned);
    let (observation, _, _) =
        observe_admission_candidates(&actions, "owner/repo", "main", &labels, true)
            .expect("the second attempt observes a still authority");
    assert_eq!(
        observation.prs[0].fact.head_sha,
        "c".repeat(40),
        "the plan is made on the head that held, not the one that moved"
    );
}

#[test]
fn an_authority_that_never_holds_is_still_an_error() {
    let temp = tempfile::tempdir().expect("temp");
    // Every read sees a new head.
    let actions = moving_head_actions(
        &temp,
        "n=$(cat .reads 2>/dev/null || echo 0); n=$((n+1)); echo $n > .reads; head=$(printf '%040d' $n)",
    );
    let labels = ["arm64", "macos", "self-hosted"].map(str::to_owned);
    let error = observe_admission_candidates(&actions, "owner/repo", "main", &labels, true)
        .expect_err("a moving authority never yields a plan");
    assert!(error.starts_with(ADMISSION_AUTHORITY_CHANGED), "{error}");
    assert!(error.contains("3 consecutive attempts"), "{error}");
}

#[test]
fn only_the_authority_change_is_retried() {
    let mut calls = 0;
    let error = with_stable_admission_authority(|| -> Result<(), String> {
        calls += 1;
        Err("open PR list failed".to_owned())
    })
    .expect_err("other errors return");
    assert_eq!(error, "open PR list failed");
    assert_eq!(calls, 1);

    let mut calls = 0;
    let value = with_stable_admission_authority(|| {
        calls += 1;
        if calls < 3 {
            Err(ADMISSION_AUTHORITY_CHANGED.to_owned())
        } else {
            Ok(calls)
        }
    })
    .expect("third attempt holds");
    assert_eq!(value, 3);
}

/// The subprocess lock holder exits with one of two codes so that giving up on the parent's
/// release marker cannot be mistaken for obeying it. Dropping the lock early would leave the
/// parent measuring an uncontended state while still asserting a contention verdict.
#[cfg(unix)]
const LOCK_CHILD_RELEASED_EXIT: i32 = 87;
#[cfg(unix)]
const LOCK_CHILD_TIMED_OUT_EXIT: i32 = 86;

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
    // The lock has to outlive the parent's contention assertions. The bound only exists so a
    // panicking parent cannot orphan the holder, so it is far longer than any progress the
    // parent makes here rather than a guess at how long that progress takes.
    let deadline = Instant::now() + Duration::from_mins(1);
    while !release.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::process::exit(if release.exists() {
        LOCK_CHILD_RELEASED_EXIT
    } else {
        LOCK_CHILD_TIMED_OUT_EXIT
    });
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn cross_process_contender_defers_and_owner_death_forces_fresh_observation() {
    let _process_fixture = crate::test_support::lock_process_tree_for_test();
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
    assert_eq!(
        child.wait().expect("child exit").code(),
        Some(LOCK_CHILD_RELEASED_EXIT),
        "lock holder timed out instead of obeying the release marker, so the contention verdict \
         above was measured against an already-released lock"
    );
    let actions = clean_admission_actions(&temp, &calls);
    output.clear();
    let exit = run(&args, &actions, &mut output).expect("fresh successor observation");
    assert_eq!(exit, ExitCode::SUCCESS);
    let verdict: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(verdict["verdict"], "admit");
    let calls_text = fs::read_to_string(&calls).unwrap();
    assert!(calls_text.contains("api repos/owner/repo"));
    assert_eq!(
        calls_text
            .lines()
            .filter(|line| line.starts_with("pr list "))
            .count(),
        1,
        "read-only admission should avoid a second full PR census"
    );

    let hang_temp = tempfile::tempdir().expect("hang temp");
    let hanging = fake_gh(&hang_temp, "sleep 5")
        .with_repo_override("owner/repo")
        .with_absolute_deadline(Instant::now() + Duration::from_millis(100));
    let timeout_started = Instant::now();
    output.clear();
    let exit = run(&args, &hanging, &mut output).expect("typed timeout verdict");
    // The fake `gh` sleeps five seconds against a hundred-millisecond deadline, so any wall
    // clock well short of that sleep proves the deadline aborted the call. The margin is
    // deliberately loose rather than tight to the deadline: under `cargo llvm-cov` this binary
    // is slow enough that a near-deadline bound measures the instrumentation instead.
    assert!(
        timeout_started.elapsed() < Duration::from_secs(3),
        "observation deadline did not abort the hanging call"
    );
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
    // A read-only admission reads the ledger snapshot and does not wait on a
    // running steward pass.
    let exit = run(&args, &actions, &mut output).expect("read-only verdict under steward");
    typed_verdict(exit, &output, 0, "clean");

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

/// A steward apply pass holds the ledger lock for its whole multi-repo pass,
/// which on a busy host is most of the time. Admission that only reads must
/// not be starved by it, but a pending cancellation (a real queue hold) must
/// still defer, and the path that writes must still require the lock.
#[cfg(unix)]
#[test]
fn running_steward_blocks_only_admission_that_must_write() {
    let temp = tempfile::tempdir().expect("temp");
    let control = mutation_control(&temp, "studio", "studio");
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(control.global_dir),
        Some(control.state_dir),
    );
    let calls = temp.path().join("github-calls");
    let actions = clean_admission_actions(&temp, &calls);
    let mut args = AdmissionCleanArgs {
        repo: "owner/repo".to_owned(),
        base: "main".to_owned(),
        labels: vec!["self-hosted".to_owned(), "ARM64".to_owned()],
        apply: true,
    };
    let run = |args: &AdmissionCleanArgs, output: &mut Vec<u8>| {
        admission_clean_command(
            args,
            temp.path(),
            RuntimeMode::Isolated,
            &paths,
            &actions,
            true,
            output,
        )
    };
    let ledger_path = paths.state_dir.join("merge-steward.json");
    let steward = acquire_ledger_lock(&ledger_path).expect("active steward lock");

    // Clean queue, no pending cancellation: the apply path TartCI calls admits
    // while the steward pass is still running, after its fresh revalidation.
    let mut output = Vec::new();
    let exit = run(&args, &mut output).expect("clean apply verdict under steward");
    let verdict = typed_verdict(exit, &output, 0, "clean");
    assert_eq!(verdict["verdict"], "admit");

    // A pending cancellation the steward published is a queue hold. The
    // lock-free snapshot still sees it, so report-only admission defers.
    let mut ledger = StewardLedger::default();
    ledger
        .pending_cancellations
        .insert("pending".to_owned(), pending_cancellation_record());
    save_ledger(&ledger_path, &ledger).expect("seed pending");
    args.apply = false;
    output.clear();
    let exit = run(&args, &mut output).expect("report-only pending verdict");
    let verdict = typed_verdict(exit, &output, 3, "cancellation_pending");
    assert_eq!(verdict["blocker_run_ids"], serde_json::json!([100]));

    // Resuming that cancellation writes the ledger, so the apply path still
    // needs the lock and defers while the steward holds it.
    args.apply = true;
    output.clear();
    let exit = run(&args, &mut output).expect("apply pending verdict under steward");
    let verdict = typed_verdict(exit, &output, 3, "stewardship_in_progress");
    assert!(
        verdict["error"]
            .as_str()
            .unwrap()
            .contains("already running")
    );
    let unchanged = load_ledger(&ledger_path).expect("ledger");
    assert!(
        unchanged.pending_cancellations.contains_key("pending"),
        "a deferred admission must not touch the steward's ledger"
    );
    drop(steward);
}

/// The #609 shape, deterministically: a child forked while a steward holds its
/// ledger or observation lock keeps a duplicate of the locked descriptor until
/// its exec. Releasing must not depend on that duplicate closing first.
#[test]
fn steward_locks_release_while_a_forked_duplicate_descriptor_lives() {
    let temp = tempfile::tempdir().expect("temp");
    let ledger = temp.path().join("merge-steward.json");
    let held = acquire_ledger_lock(&ledger).expect("ledger lock");
    let duplicate = held.try_clone().expect("duplicate descriptor");
    drop(held);
    acquire_ledger_lock(&ledger).expect("the ledger lock must be free after release");
    drop(duplicate);

    let labels = vec!["pulp-build-vm".to_owned()];
    let observation =
        try_acquire_admission_observation_lock(temp.path(), "Owner/Repo", "main", &labels)
            .expect("observation lock")
            .expect("free");
    let duplicate = observation.try_clone().expect("duplicate descriptor");
    drop(observation);
    assert!(
        try_acquire_admission_observation_lock(temp.path(), "Owner/Repo", "main", &labels)
            .expect("observation lock")
            .is_some(),
        "the observation lock must be free after release"
    );
    drop(duplicate);
}

/// A fake `gh` for a repository with one managed, labelled pull request whose
/// rollup is absent, which is how the production census sees it: every
/// admission observation must then read that head's check runs and statuses.
#[cfg(unix)]
fn managed_head_admission_actions(
    dir: &tempfile::TempDir,
    calls: &Path,
    head: &str,
) -> GitHubActions {
    fake_gh(
        dir,
        &format!(
            r#"
printf '%s\n' "$*" >> '{calls}'
case "$*" in
  "api repos/owner/repo") printf '%s' '{{"full_name":"owner/repo","allow_auto_merge":true}}' ;;
  *"rules/branches/main --paginate --slurp"*) printf '%s' '[[]]' ;;
  *"branches/main/protection/required_status_checks"*) printf '%s' '{{"contexts":[],"checks":[]}}' ;;
  "api graphql "*) printf '%s' '{{"data":{{"repository":{{"mergeQueue":null}}}}}}' ;;
  "pr list "*) printf '%s' '[{{"id":"PR_kw","number":42,"isDraft":false,"baseRefName":"main","headRefOid":"{head}","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{{"name":"shipyard:managed"}}],"author":{{"login":"someone"}}}}]' ;;
  *"commits/{head}/check-runs"*) printf '%s' '{{"check_runs":[]}}' ;;
  *"commits/{head}/statuses"*) printf '%s' '[{{"context":"shipyard/steward-handoff","state":"success","updated_at":"2026-08-30T00:00:00Z"}}]' ;;
  *"actions/runs?status="*) printf '%s' '{{"workflow_runs":[]}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            calls = calls.display(),
        ),
    )
    .with_repo_override("owner/repo")
}

/// Admission's clean verdict is made entirely by its read client: when that
/// client carries a dedicated credential, the shared one is never spent. The
/// read cache then collapses the repeated per-head reads of one verdict, and of
/// the next verdict inside the TTL, to a single read each.
#[cfg(unix)]
#[test]
fn clean_admission_reads_use_the_read_client_and_reuse_per_head_reads() {
    let temp = tempfile::tempdir().expect("temp");
    let control = mutation_control(&temp, "studio", "studio");
    let paths = RuntimePaths::current_with_overrides(
        RuntimeMode::Isolated,
        Some(control.global_dir),
        Some(control.state_dir),
    );
    let head = "c".repeat(40);
    let shared_calls = temp.path().join("shared-calls");
    let read_calls = temp.path().join("read-calls");
    let shared_dir = tempfile::tempdir().expect("shared gh");
    let reads_dir = tempfile::tempdir().expect("reads gh");
    let shared = managed_head_admission_actions(&shared_dir, &shared_calls, &head);
    let cache = crate::cloud::ReadCache::new(
        paths.state_dir.join("admission-read-cache"),
        Duration::from_mins(3),
    );
    let reads =
        managed_head_admission_actions(&reads_dir, &read_calls, &head).with_read_cache(cache);
    let args = AdmissionCleanArgs {
        repo: "owner/repo".to_owned(),
        base: "main".to_owned(),
        labels: vec!["self-hosted".to_owned(), "ARM64".to_owned()],
        apply: true,
    };
    let run = |output: &mut Vec<u8>| {
        admission_clean_command_with_reads(
            &args,
            temp.path(),
            RuntimeMode::Isolated,
            &paths,
            &shared,
            &reads,
            true,
            output,
        )
    };
    let count = |calls: &Path, needle: &str| {
        fs::read_to_string(calls)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(needle))
            .count()
    };

    let mut output = Vec::new();
    let exit = run(&mut output).expect("clean verdict");
    typed_verdict(exit, &output, 0, "clean");
    assert!(
        !shared_calls.exists(),
        "a clean verdict must not spend the shared credential: {}",
        fs::read_to_string(&shared_calls).unwrap_or_default()
    );
    // Observation, its fence, revalidation and its fence each consult the
    // head; the cache answers all but the first.
    assert_eq!(count(&read_calls, "/statuses"), 1);
    assert_eq!(count(&read_calls, "/check-runs"), 1);
    let settings_reads = |calls: &Path| {
        fs::read_to_string(calls)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "api repos/owner/repo")
            .count()
    };
    assert_eq!(settings_reads(&read_calls), 1);
    assert_eq!(count(&read_calls, "rules/branches"), 1);
    // The PR census, the merge queue and the run census are never cached.
    assert_eq!(count(&read_calls, "pr list "), 4);
    assert_eq!(count(&read_calls, "status=queued"), 2);

    fs::remove_file(&read_calls).unwrap();
    output.clear();
    let exit = run(&mut output).expect("second clean verdict");
    typed_verdict(exit, &output, 0, "clean");
    assert_eq!(count(&read_calls, "/statuses"), 0);
    assert_eq!(count(&read_calls, "/check-runs"), 0);
    assert_eq!(count(&read_calls, "rules/branches"), 0);
    assert_eq!(settings_reads(&read_calls), 0);
    assert_eq!(count(&read_calls, "pr list "), 4);
    assert_eq!(count(&read_calls, "status=queued"), 2);
}
