use super::*;
#[cfg(unix)]
use crate::config::{LoadedConfig, LocalOverlaySource};
#[cfg(unix)]
use std::path::PathBuf;

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/rescue-superseded"
);
const REPO: &str = "Generous-Corp/pulp";
const SUPERSEDED: u64 = 35_942_471_884;
const LIVE: u64 = 35_942_990_001;
const PULL_REQUEST_CONTROL: u64 = 35_942_000_777;
const RELEASE_CONTROL: u64 = 35_942_000_888;
const GHOST: u64 = 32_218_602_754;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{FIXTURES}/{name}")).expect("read fixture")
}

#[allow(clippy::unnecessary_wraps)]
fn recorded_refs() -> Result<String, String> {
    Ok(fixture("ls_remote.txt"))
}

fn run(id: u64, event: &str, branch: &str, status: &str, workflow: &str, path: &str) -> QueuedRun {
    QueuedRun {
        database_id: id,
        name: workflow.to_owned(),
        head_branch: branch.to_owned(),
        event: event.to_owned(),
        created_at: "2026-09-24T01:20:23Z".to_owned(),
        run_started_at: None,
        workflow_name: workflow.to_owned(),
        url: None,
        path: path.to_owned(),
        status: status.to_owned(),
        conclusion: None,
    }
}

fn merge_group_run(id: u64, branch: &str) -> QueuedRun {
    run(
        id,
        "merge_group",
        branch,
        "in_progress",
        "Build and Test",
        ".github/workflows/build.yml",
    )
}

#[cfg(unix)]
struct Harness {
    _temp: tempfile::TempDir,
    actions: GitHubActions,
    log: PathBuf,
    state_root: PathBuf,
}

/// Fake `gh` that serves the recorded fixtures and logs every invocation.
/// `cancel_exit` / `cancel_stderr` script the cancel endpoint.
#[cfg(unix)]
fn harness(cancel_exit: u8, cancel_stderr: &str) -> Harness {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("tempdir");
    let write_exec = |path: &Path, body: &str| {
        std::fs::write(path, body).expect("write script");
        let mut perms = std::fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod");
    };
    let helper = temp.path().join("token-helper");
    write_exec(
        &helper,
        "#!/bin/sh\nprintf '{\"token\":\"ghs_test\",\"kind\":\"github-app-installation\",\"expires_at\":\"2099-01-01T00:00:00Z\"}'\n",
    );
    let config = LoadedConfig {
        data: format!(
            "[github.auth]\nsource = \"command\"\ntoken_command = [\"{}\"]\ncache_ttl_seconds = 300\n",
            helper.display()
        )
        .parse()
        .expect("config"),
        global_dir: temp.path().join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: LocalOverlaySource::None,
    };
    let log = temp.path().join("gh.log");
    let gh = temp.path().join("gh");
    write_exec(
        &gh,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
F='{fixtures}'
case "$*" in
  *"/cancel"*) printf '%s' '{cancel_stderr}' >&2; exit {cancel_exit} ;;
  *"force-cancel"*) exit 9 ;;
  "api repos/{repo}/actions/runs?status=in_progress"*) cat "$F/runs_in_progress.json" ;;
  "api repos/{repo}/actions/runs?status=queued"*) cat "$F/runs_queued.json" ;;
  "api repos/{repo}/actions/runs?status=waiting"*) cat "$F/runs_empty.json" ;;
  "run view "*) cat "$F/jobs_$3.json" 2>/dev/null || {{ echo "no jobs fixture" >&2; exit 1; }} ;;
  *) echo "unexpected gh call: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
            fixtures = FIXTURES,
            repo = REPO,
        ),
    );
    let actions =
        GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(&gh);
    let state_root = temp.path().join("state");
    std::fs::create_dir_all(&state_root).expect("state root");
    Harness {
        _temp: temp,
        actions,
        log,
        state_root,
    }
}

#[cfg(unix)]
fn calls(harness: &Harness) -> String {
    std::fs::read_to_string(&harness.log).unwrap_or_default()
}

#[cfg(unix)]
fn run_reap(
    harness: &Harness,
    apply: bool,
    refs: impl FnOnce() -> Result<String, String>,
) -> (Result<ExitCode, CliFailure>, Value) {
    let mut out = Vec::new();
    let result = reap_superseded_merge_groups(
        &ReapRequest {
            repo: REPO,
            apply,
            state_root: &harness.state_root,
        },
        &harness.actions,
        refs,
        "origin",
        true,
        &mut out,
    );
    let envelope = serde_json::from_slice::<Value>(&out).unwrap_or(Value::Null);
    (result, envelope)
}

#[cfg(unix)]
fn row_for(envelope: &Value, run_id: u64) -> Option<Value> {
    envelope
        .pointer("/data/runs")
        .or_else(|| envelope.get("runs"))
        .and_then(Value::as_array)?
        .iter()
        .find(|row| row.get("run_id").and_then(Value::as_u64) == Some(run_id))
        .cloned()
}

#[cfg(unix)]
fn status_of(envelope: &Value, run_id: u64) -> Option<String> {
    row_for(envelope, run_id)?
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

// ---- pure decision layer --------------------------------------------------

#[test]
fn ls_remote_listing_requires_head_control() {
    let confirmed = parse_queue_refs(recorded_refs());
    let QueueRefs::Confirmed(live) = confirmed else {
        panic!("recorded listing carries HEAD: {confirmed:?}");
    };
    assert_eq!(live.len(), 1);
    assert!(
        live.contains("gh-readonly-queue/main/pr-8726-0e3c1f6d2b4a5968778695a4b3c2d1e0f9a8b7c6")
    );

    // An empty queue is still a confirmed listing when HEAD is present.
    assert_eq!(
        parse_queue_refs(Ok("abc\tHEAD\n".to_owned())),
        QueueRefs::Confirmed(BTreeSet::new())
    );
    // A successful but empty listing measured nothing; it must not read as
    // "every ref is gone".
    assert!(matches!(
        parse_queue_refs(Ok(String::new())),
        QueueRefs::Unknown(_)
    ));
    assert!(matches!(
        parse_queue_refs(Err("fatal: could not read from remote".to_owned())),
        QueueRefs::Unknown(_)
    ));
}

#[test]
fn superseded_run_with_active_jobs_is_cancelled() {
    let refs = parse_queue_refs(recorded_refs());
    let run = merge_group_run(
        SUPERSEDED,
        "gh-readonly-queue/main/pr-8726-a49be6e6c8da2c3924e6f53cabd8e5c8bf786946",
    );
    assert_eq!(
        decide(&run, &refs, || Ok(2)),
        Decision::Cancel { active_jobs: 2 }
    );
}

#[test]
fn live_ref_is_kept_without_reading_jobs() {
    let refs = parse_queue_refs(recorded_refs());
    let run = merge_group_run(
        LIVE,
        "gh-readonly-queue/main/pr-8726-0e3c1f6d2b4a5968778695a4b3c2d1e0f9a8b7c6",
    );
    let decision = decide(&run, &refs, || {
        panic!("a live batch must not cost a jobs read")
    });
    assert!(matches!(decision, Decision::Keep(reason) if reason.contains("live")));
}

#[test]
fn unverified_refs_keep_the_run_without_reading_jobs() {
    let refs = parse_queue_refs(Err("network down".to_owned()));
    let run = merge_group_run(SUPERSEDED, "gh-readonly-queue/main/pr-8726-a49be6e6");
    let decision = decide(&run, &refs, || panic!("uncertainty must short-circuit"));
    assert!(matches!(decision, Decision::Keep(reason) if reason.contains("uncertainty")));
}

#[test]
fn zero_job_ghost_is_skipped_and_jobs_read_failure_keeps() {
    let refs = parse_queue_refs(recorded_refs());
    let ghost = merge_group_run(
        GHOST,
        "gh-readonly-queue/main/pr-7677-647fd209c059c55ad1b7893282b8b1105f4524a6",
    );
    assert!(matches!(
        decide(&ghost, &refs, || Ok(0)),
        Decision::SkipGhost(_)
    ));
    assert!(matches!(
        decide(&ghost, &refs, || Err("HTTP 502".to_owned())),
        Decision::Keep(reason) if reason.contains("could not be read")
    ));
}

#[test]
fn only_in_flight_unprotected_merge_group_runs_are_candidates() {
    let superseded = merge_group_run(SUPERSEDED, "gh-readonly-queue/main/pr-8726-a49be6e6");
    assert!(is_reap_candidate(&superseded));

    // Control: an ordinary PR run is never a candidate, even on a queue-shaped branch.
    let pull_request = run(
        PULL_REQUEST_CONTROL,
        "pull_request",
        "gh-readonly-queue/main/pr-8726-a49be6e6",
        "in_progress",
        "Build and Test",
        ".github/workflows/build.yml",
    );
    assert!(!is_reap_candidate(&pull_request));

    let release = run(
        RELEASE_CONTROL,
        "merge_group",
        "gh-readonly-queue/main/pr-8700-1111",
        "in_progress",
        "Release CLI",
        ".github/workflows/release-cli.yml",
    );
    assert!(!is_reap_candidate(&release));

    let mut completed = superseded.clone();
    completed.status = "completed".to_owned();
    assert!(!is_reap_candidate(&completed));

    let mut not_a_queue_branch = superseded;
    not_a_queue_branch.head_branch = "main".to_owned();
    assert!(!is_reap_candidate(&not_a_queue_branch));
}

// ---- end to end through a recorded-fixture gh -----------------------------

#[cfg(unix)]
#[test]
fn apply_cancels_only_the_superseded_run() {
    let harness = harness(0, "");
    let (result, envelope) = run_reap(&harness, true, recorded_refs);
    assert_eq!(result.expect("reap"), ExitCode::SUCCESS);

    let calls = calls(&harness);
    let cancels = calls
        .lines()
        .filter(|line| line.contains("/cancel"))
        .collect::<Vec<_>>();
    assert_eq!(
        cancels,
        vec![format!(
            "api -X POST repos/{REPO}/actions/runs/{SUPERSEDED}/cancel"
        )],
        "exactly the superseded run is cancelled: {calls}"
    );
    // Controls are never touched at all: no jobs read, no cancel.
    for untouched in [LIVE, PULL_REQUEST_CONTROL, RELEASE_CONTROL] {
        assert!(
            !calls.contains(&untouched.to_string()),
            "run {untouched} must not be touched: {calls}"
        );
    }
    assert_eq!(
        status_of(&envelope, SUPERSEDED).as_deref(),
        Some("cancelled")
    );
    assert_eq!(status_of(&envelope, LIVE).as_deref(), Some("keep"));
    assert_eq!(
        status_of(&envelope, GHOST).as_deref(),
        Some("skipped-ghost")
    );
    assert!(row_for(&envelope, PULL_REQUEST_CONTROL).is_none());
    assert!(row_for(&envelope, RELEASE_CONTROL).is_none());
    let evidence = row_for(&envelope, SUPERSEDED)
        .and_then(|row| {
            row.get("evidence")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    assert!(evidence.contains("absent from ls-remote") && evidence.contains("2 active job"));
}

#[cfg(unix)]
#[test]
fn dry_run_is_the_default_and_never_cancels() {
    let harness = harness(0, "");
    let (result, envelope) = run_reap(&harness, false, recorded_refs);
    assert_eq!(result.expect("reap"), ExitCode::SUCCESS);
    let calls = calls(&harness);
    assert!(
        !calls.contains("/cancel"),
        "dry-run must not mutate: {calls}"
    );
    assert_eq!(
        status_of(&envelope, SUPERSEDED).as_deref(),
        Some("would-cancel")
    );
}

#[cfg(unix)]
#[test]
fn ls_remote_failure_keeps_every_run_and_exits_nonzero() {
    let harness = harness(0, "");
    let (result, envelope) = run_reap(&harness, true, || {
        Err("git ls-remote origin exited 128: Connection reset".to_owned())
    });
    assert_eq!(result.expect("reap"), ExitCode::from(1));
    let calls = calls(&harness);
    assert!(
        !calls.contains("/cancel"),
        "no cancel on uncertainty: {calls}"
    );
    assert!(
        !calls.contains("run view"),
        "no jobs read on uncertainty: {calls}"
    );
    assert_eq!(status_of(&envelope, SUPERSEDED).as_deref(), Some("keep"));
    assert_eq!(status_of(&envelope, GHOST).as_deref(), Some("keep"));
}

#[cfg(unix)]
#[test]
fn cancel_409_is_left_alone_not_a_failure() {
    let harness = harness(
        1,
        "gh: Cannot cancel a workflow run that is completed. (HTTP 409)",
    );
    let (result, envelope) = run_reap(&harness, true, recorded_refs);
    assert_eq!(result.expect("reap"), ExitCode::SUCCESS);
    let calls = calls(&harness);
    assert!(!calls.contains("force-cancel"), "never escalate: {calls}");
    assert_eq!(
        status_of(&envelope, SUPERSEDED).as_deref(),
        Some("skipped-ghost")
    );
}

#[cfg(unix)]
#[test]
fn other_cancel_errors_are_failures() {
    let harness = harness(1, "gh: Server Error (HTTP 500)");
    let (result, envelope) = run_reap(&harness, true, recorded_refs);
    assert_eq!(result.expect("reap"), ExitCode::from(1));
    assert_eq!(status_of(&envelope, SUPERSEDED).as_deref(), Some("failed"));
}

#[cfg(unix)]
#[test]
fn merge_queue_hold_refuses_apply_before_any_github_call() {
    let harness = harness(0, "");
    let hold = harness
        .state_root
        .join(crate::merge_queue_control::HOLD_FILE);
    std::fs::create_dir_all(hold.parent().expect("parent")).expect("mkdir");
    std::fs::write(&hold, "{\"reason\":\"operator pause\"}\n").expect("hold");

    let (result, _) = run_reap(&harness, true, || panic!("refs must not be read"));
    let error = result.expect_err("held apply refuses");
    assert!(error.message().contains("held"), "{}", error.message());
    assert!(calls(&harness).is_empty(), "no GitHub call under hold");

    // Audit still works under hold.
    let (result, envelope) = run_reap(&harness, false, recorded_refs);
    assert_eq!(result.expect("audit"), ExitCode::SUCCESS);
    assert_eq!(
        status_of(&envelope, SUPERSEDED).as_deref(),
        Some("would-cancel")
    );
}

#[test]
fn human_render_prints_each_decision_with_evidence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let request = ReapRequest {
        repo: REPO,
        apply: false,
        state_root: temp.path(),
    };
    let refs = parse_queue_refs(recorded_refs());
    let superseded = merge_group_run(SUPERSEDED, "gh-readonly-queue/main/pr-8726-a49be6e6");
    let decision = Decision::Cancel { active_jobs: 2 };
    let rendered = render_human(
        &request,
        "origin",
        &refs,
        &[row(
            &superseded,
            &decision,
            &Outcome::WouldCancel,
            "queue ref absent from ls-remote; 2 active job(s) still hold capacity",
        )],
    );
    assert!(rendered.contains("dry-run; pass --apply"));
    assert!(rendered.contains("HEAD control present"));
    assert!(rendered.contains(&format!("run {SUPERSEDED}")));
    assert!(rendered.contains("would-cancel — queue ref absent"));
}
