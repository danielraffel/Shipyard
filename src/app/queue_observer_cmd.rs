//! Read-only `shipyard queue-observe` transport.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use fs2::FileExt;
use serde::Deserialize;
use serde_json::Value;

use super::CliFailure;
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::merge_queue_control::{authority_status, hold_status};
use crate::paths::RuntimePaths;
use crate::queue_observer::{
    ObserverState, OwnershipSnapshot, Transition, append_transition, default_paths, load_state,
    next_poll_seconds, observe, only_governance_graphql_errors, parse_snapshot_with_previous,
    render_markdown, save_state,
};

// One GraphQL request returns base SHA, bounded open PR heads/checks/labels,
// classic required contexts, and server-owned merge-queue order/group heads.
// This is intentionally a query literal; no mutation operation exists in this
// module or in the command surface.
const SNAPSHOT_QUERY: &str = r"query($owner:String!,$name:String!,$branch:String!,$qualified:String!){repository(owner:$owner,name:$name){url baseRef:ref(qualifiedName:$qualified){target{... on Commit{oid}} branchProtectionRule{requiredStatusCheckContexts requiredStatusChecks{context app{databaseId}}}} pullRequests(first:100,states:OPEN,baseRefName:$branch,orderBy:{field:UPDATED_AT,direction:DESC}){nodes{number url headRefOid mergeStateStatus autoMergeRequest{enabledAt} assignees(first:20){nodes{login} pageInfo{hasNextPage}} labels(first:40){nodes{name} pageInfo{hasNextPage}} statusCheckRollup{contexts(first:100){nodes{__typename ... on CheckRun{databaseId name status conclusion detailsUrl startedAt checkSuite{createdAt app{databaseId}}} ... on StatusContext{context state targetUrl createdAt}} pageInfo{hasNextPage}}}} pageInfo{hasNextPage}} mergeQueue(branch:$branch){entries(first:100){nodes{position enqueuedAt headCommit{oid statusCheckRollup{contexts(first:100){nodes{__typename ... on CheckRun{databaseId name status conclusion detailsUrl startedAt checkSuite{createdAt app{databaseId}}} ... on StatusContext{context state targetUrl createdAt}} pageInfo{hasNextPage}}}} pullRequest{number url headRefOid}} pageInfo{hasNextPage}}}}}";
const FAILURE_BACKOFF: [u64; 5] = [15, 30, 60, 120, 300];
const SNAPSHOT_ATTEMPT_TIMEOUT: Duration = Duration::from_mins(1);

pub(super) struct QueueObserverArgs {
    pub(super) repo: Option<String>,
    pub(super) base: String,
    pub(super) follow: bool,
    pub(super) state_file: Option<PathBuf>,
    pub(super) transition_log: Option<PathBuf>,
    pub(super) replay: Option<PathBuf>,
    pub(super) max_polls: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReplayFrame {
    #[allow(dead_code)]
    name: String,
    graphql: Value,
    #[serde(default)]
    ownership: OwnershipSnapshot,
}

pub(super) fn queue_observer_command<W: Write>(
    args: QueueObserverArgs,
    config: &LoadedConfig,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let explicit_repo = args.repo.is_some();
    let repo = super::runner_cmd::resolve_repo_slug(args.repo, cwd)?;
    let configured_required = configured_required_checks(config);
    let path_base = args.replay.as_ref().map_or_else(
        || args.base.clone(),
        |replay| format!("{}\0replay\0{}", args.base, replay.display()),
    );
    let path_repo = if args.replay.is_some() {
        format!("{repo}-replay")
    } else {
        repo.clone()
    };
    let (default_state, default_log) =
        default_paths(&runtime_paths.state_dir, &path_repo, &path_base);
    let state_path = args.state_file.unwrap_or(default_state);
    let log_path = args.transition_log.unwrap_or(default_log);
    validate_distinct_paths(&state_path, &log_path)?;
    let _lock = acquire_observer_lock(&state_path)?;
    let mut state = load_state(&state_path).map_err(|error| CliFailure::new(1, error))?;

    if let Some(replay) = args.replay {
        let frames = load_replay_frames(&replay)?;
        if frames.is_empty() {
            return Err(CliFailure::new(
                2,
                format!(
                    "queue observer replay {} contains no JSON frames",
                    replay.display()
                ),
            ));
        }
        for frame in frames {
            let snapshot = parse_snapshot_with_previous(
                &frame.graphql,
                &repo,
                &args.base,
                &configured_required,
                frame.ownership,
                state.as_ref().map(|state| &state.snapshot),
            )
            .map_err(|error| CliFailure::new(1, error))?;
            state = apply_and_emit(
                snapshot,
                state.as_ref(),
                &state_path,
                &log_path,
                json,
                stdout,
            )?;
        }
        return Ok(ExitCode::SUCCESS);
    }

    let actions = observer_actions(cwd, config, &repo, explicit_repo);
    let mut polls = 0;
    let mut failures = 0_usize;
    loop {
        let snapshot = fetch_snapshot(&actions, &repo, &args.base).and_then(|body| {
            let ownership = collect_runtime_ownership(runtime_paths, cwd, mode);
            parse_snapshot_with_previous(
                &body,
                &repo,
                &args.base,
                &configured_required,
                ownership,
                state.as_ref().map(|state| &state.snapshot),
            )
            .map_err(|error| CliFailure::new(1, error))
        });
        let snapshot = match snapshot {
            Ok(snapshot) => {
                failures = 0;
                snapshot
            }
            Err(error) if args.follow && failures < FAILURE_BACKOFF.len() => {
                let delay = FAILURE_BACKOFF[failures];
                failures += 1;
                let _ = crate::writer_domain_lease::write_stderr(format_args!(
                    "queue observer read failed; retrying in {delay}s: {}",
                    error.message
                ));
                thread::sleep(Duration::from_secs(delay));
                continue;
            }
            Err(error) => return Err(error),
        };
        state = apply_and_emit(
            snapshot,
            state.as_ref(),
            &state_path,
            &log_path,
            json,
            stdout,
        )?;
        polls += 1;
        if !args.follow || args.max_polls.is_some_and(|limit| polls >= limit) {
            break;
        }
        let delay = state.as_ref().map_or(15, next_poll_seconds);
        thread::sleep(Duration::from_secs(delay));
    }
    Ok(ExitCode::SUCCESS)
}

fn collect_runtime_ownership(
    runtime_paths: &RuntimePaths,
    cwd: &Path,
    mode: RuntimeMode,
) -> OwnershipSnapshot {
    collect_ownership(
        &runtime_paths.state_dir,
        &runtime_paths.global_dir,
        cwd,
        mode,
    )
}

fn observer_actions(
    cwd: &Path,
    config: &LoadedConfig,
    repo: &str,
    explicit_repo: bool,
) -> GitHubActions {
    let actions = GitHubActions::from_loaded_config(cwd, config);
    if explicit_repo {
        actions.with_repo_override(repo)
    } else {
        actions
    }
}

fn acquire_observer_lock(state_path: &Path) -> Result<fs::File, CliFailure> {
    let lock_path = observer_lock_path(state_path);
    let writer_domain = crate::writer_domain_lease::acquire_for_protected_creation(&lock_path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if let Some(parent) = state_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| {
            CliFailure::new(1, format!("create observer state directory: {error}"))
        })?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|error| CliFailure::new(1, format!("open observer lock: {error}")))?;
    drop(writer_domain);
    lock.try_lock_exclusive().map_err(|error| {
        CliFailure::new(
            1,
            format!("queue observer already active for this state path: {error}"),
        )
    })?;
    Ok(lock)
}

fn observer_lock_path(state_path: &Path) -> PathBuf {
    let mut lock_path = state_path.as_os_str().to_owned();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

fn validate_distinct_paths(state_path: &Path, log_path: &Path) -> Result<(), CliFailure> {
    let state_resolved = resolve_output_path(state_path)?;
    let log_resolved = resolve_output_path(log_path)?;
    if state_resolved == log_resolved || existing_files_match(state_path, log_path)? {
        return Err(CliFailure::new(
            2,
            "queue observer --state-file and --transition-log must be different paths",
        ));
    }
    Ok(())
}

fn resolve_output_path(path: &Path) -> Result<PathBuf, CliFailure> {
    match fs::symlink_metadata(path) {
        Ok(_) => fs::canonicalize(path).map_err(|error| {
            CliFailure::new(
                2,
                format!("resolve queue observer output {}: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let name = path.file_name().ok_or_else(|| {
                CliFailure::new(
                    2,
                    format!(
                        "queue observer output path {} has no file name",
                        path.display()
                    ),
                )
            })?;
            let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            fs::create_dir_all(parent).map_err(|error| {
                CliFailure::new(
                    1,
                    format!(
                        "create queue observer output directory {}: {error}",
                        parent.display()
                    ),
                )
            })?;
            fs::canonicalize(parent)
                .map(|parent| parent.join(name))
                .map_err(|error| {
                    CliFailure::new(
                        2,
                        format!(
                            "resolve queue observer output directory {}: {error}",
                            parent.display()
                        ),
                    )
                })
        }
        Err(error) => Err(CliFailure::new(
            2,
            format!("inspect queue observer output {}: {error}", path.display()),
        )),
    }
}

fn existing_files_match(left: &Path, right: &Path) -> Result<bool, CliFailure> {
    match same_file::is_same_file(left, right) {
        Ok(matches) => Ok(matches),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(CliFailure::new(
            2,
            format!("inspect queue observer output identity: {error}"),
        )),
    }
}

fn apply_and_emit<W: Write>(
    snapshot: crate::queue_observer::QueueStateSnapshot,
    previous: Option<&ObserverState>,
    state_path: &Path,
    log_path: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<Option<ObserverState>, CliFailure> {
    let result =
        observe(previous, snapshot).map_err(|error| CliFailure::new(1, error.to_string()))?;
    if let Some(transition) = &result.transition {
        // Append before advancing the cursor. A crash can therefore replay a
        // transition at least once, but can never durably skip it.
        append_transition(log_path, transition).map_err(|error| CliFailure::new(1, error))?;
        emit_transition(stdout, transition, json)?;
    }
    save_state(state_path, &result.state).map_err(|error| CliFailure::new(1, error))?;
    Ok(Some(result.state))
}

fn emit_transition<W: Write>(
    stdout: &mut W,
    transition: &Transition,
    json: bool,
) -> Result<(), CliFailure> {
    if json {
        serde_json::to_writer(&mut *stdout, transition)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        stdout
            .write_all(b"\n")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        stdout
            .write_all(render_markdown(transition).as_bytes())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn fetch_snapshot(actions: &GitHubActions, repo: &str, base: &str) -> Result<Value, CliFailure> {
    debug_assert!(!SNAPSHOT_QUERY.to_ascii_lowercase().contains("mutation"));
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| CliFailure::new(2, format!("invalid repository slug `{repo}`")))?;
    let args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={SNAPSHOT_QUERY}"),
        "-F".to_owned(),
        format!("owner={owner}"),
        "-F".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("branch={base}"),
        "-F".to_owned(),
        format!("qualified=refs/heads/{base}"),
    ];
    let output = actions
        .run_gh_with_timeout_output(&args, SNAPSHOT_ATTEMPT_TIMEOUT)
        .map_err(|error| CliFailure::new(1, format!("read queue snapshot: {error}")))?;
    let body = serde_json::from_slice(output.stdout()).map_err(|error| {
        if output.success() {
            CliFailure::new(1, format!("parse queue snapshot: {error}"))
        } else {
            CliFailure::new(
                1,
                format!("read queue snapshot: {}", output.command_error(&args)),
            )
        }
    })?;
    if !output.success() && !only_governance_graphql_errors(&body) {
        return Err(CliFailure::new(
            1,
            format!("read queue snapshot: {}", output.command_error(&args)),
        ));
    }
    Ok(body)
}

fn collect_ownership(
    state_root: &Path,
    global_dir: &Path,
    cwd: &Path,
    mode: RuntimeMode,
) -> OwnershipSnapshot {
    let authority = authority_status(state_root, cwd, mode, global_dir);
    let hold = hold_status(state_root);
    let mut snapshot = OwnershipSnapshot::default();
    match authority {
        Ok(value) => {
            snapshot.machine = optional_json_string(&value, "machine");
            snapshot.mutation_machine = optional_json_string(&value, "mutation_machine");
            snapshot.authority_matches = value
                .get("authority_matches")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        }
        Err(error) => snapshot.blocker = Some(format!("authority unreadable: {error}")),
    }
    apply_hold_observation(&mut snapshot, hold);
    snapshot
}

fn apply_hold_observation(snapshot: &mut OwnershipSnapshot, hold: Result<Option<Value>, String>) {
    match hold {
        Ok(Some(value)) => {
            snapshot.hold_active = true;
            snapshot.hold_reason =
                optional_json_string(&value, "reason").filter(|reason| !reason.trim().is_empty());
            snapshot.hold_machine = optional_json_string(&value, "machine");
            snapshot.held_at = optional_json_string(&value, "held_at");
            if snapshot.hold_reason.is_none() {
                add_ownership_blocker(snapshot, "hold record has no valid reason");
            }
        }
        Ok(None) => {}
        Err(error) => {
            snapshot.hold_active = true;
            add_ownership_blocker(snapshot, &format!("hold unreadable: {error}"));
        }
    }
}

fn add_ownership_blocker(snapshot: &mut OwnershipSnapshot, message: &str) {
    snapshot.blocker = Some(
        snapshot
            .blocker
            .take()
            .map_or(message.to_owned(), |current| {
                format!("{current}; {message}")
            }),
    );
}

fn configured_required_checks(config: &LoadedConfig) -> Vec<String> {
    config
        .get("governance.required_status_checks")
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn load_replay_frames(path: &Path) -> Result<Vec<ReplayFrame>, CliFailure> {
    if path.is_dir() {
        let mut paths = fs::read_dir(path)
            .map_err(|error| CliFailure::new(1, format!("read replay directory: {error}")))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect::<Vec<_>>();
        paths.sort();
        return paths
            .into_iter()
            .map(|path| load_replay_frame(&path))
            .collect();
    }
    let raw = fs::read_to_string(path)
        .map_err(|error| CliFailure::new(1, format!("read replay {}: {error}", path.display())))?;
    if let Ok(frames) = serde_json::from_str::<Vec<ReplayFrame>>(&raw) {
        return Ok(frames);
    }
    serde_json::from_str(&raw)
        .map(|frame| vec![frame])
        .map_err(|error| CliFailure::new(1, format!("parse replay {}: {error}", path.display())))
}

fn load_replay_frame(path: &Path) -> Result<ReplayFrame, CliFailure> {
    let raw = fs::read_to_string(path)
        .map_err(|error| CliFailure::new(1, format!("read replay {}: {error}", path.display())))?;
    serde_json::from_str(&raw)
        .map_err(|error| CliFailure::new(1, format!("parse replay {}: {error}", path.display())))
}

fn optional_json_string(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::config::LocalOverlaySource;
    use crate::queue_observer::parse_snapshot;

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, contents).expect("write script");
        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod script");
    }

    #[test]
    fn snapshot_transport_is_one_read_only_graphql_query() {
        let lower = SNAPSHOT_QUERY.to_ascii_lowercase();
        assert!(lower.starts_with("query("));
        assert!(!lower.contains("mutation"));
        assert_eq!(lower.matches("repository(").count(), 1);
        for field in [
            "baseref:ref",
            "pullrequests",
            "statuscheckrollup",
            "mergequeue",
            "branchprotectionrule",
        ] {
            assert!(lower.contains(field), "missing {field}");
        }
    }

    #[test]
    fn malformed_but_present_hold_remains_visible_and_blocking() {
        let mut snapshot = OwnershipSnapshot::default();
        apply_hold_observation(&mut snapshot, Ok(Some(serde_json::json!({}))));
        assert!(snapshot.hold_active);
        assert_eq!(snapshot.hold_reason, None);
        assert_eq!(
            snapshot.blocker.as_deref(),
            Some("hold record has no valid reason")
        );
    }

    #[test]
    fn distinct_output_paths_reject_parent_aliases() {
        let temp = tempfile::tempdir().expect("temp");
        let nested = temp.path().join("nested");
        fs::create_dir(&nested).expect("nested");
        let state = nested.join("..").join("observer.json");
        let log = temp.path().join("observer.json");
        let error = validate_distinct_paths(&state, &log).expect_err("same resolved path");
        assert_eq!(error.code, 2);
    }

    #[test]
    fn distinct_output_paths_reject_hard_link_aliases() {
        let temp = tempfile::tempdir().expect("temp");
        let state = temp.path().join("observer-state.json");
        let log = temp.path().join("observer-transitions.jsonl");
        fs::write(&state, "seed").expect("state");
        fs::hard_link(&state, &log).expect("hard link");

        let error = validate_distinct_paths(&state, &log).expect_err("same underlying file");
        assert_eq!(error.code, 2);
    }

    #[test]
    fn observer_lock_suffix_preserves_the_complete_state_name() {
        assert_eq!(
            observer_lock_path(Path::new("observer.json")),
            PathBuf::from("observer.json.lock")
        );
        assert_eq!(
            observer_lock_path(Path::new("observer.lock")),
            PathBuf::from("observer.lock.lock")
        );
    }

    #[test]
    fn replay_fixtures_miss_no_delivery_transition() {
        let fixture_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/queue-observer");
        let frames = load_replay_frames(&fixture_dir).expect("fixtures");
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.name.as_str())
                .collect::<Vec<_>>(),
            [
                "initial",
                "admission",
                "merge_group",
                "failure",
                "refresh",
                "ownership_hold",
                "merge",
            ]
        );
        let mut previous = None;
        let mut transitions = Vec::new();
        for frame in frames {
            let snapshot = parse_snapshot(
                &frame.graphql,
                "acme/pulp",
                "main",
                &["macos".to_owned()],
                frame.ownership,
            )
            .expect("snapshot");
            let result = observe(previous.as_ref(), snapshot).expect("observe");
            transitions.push(result.transition.expect("fixture transition"));
            previous = Some(result.state);
        }
        assert_eq!(transitions.len(), 7);
        assert_eq!(transitions[1].snapshot.queue[0].merge_group_sha, None);
        assert_eq!(
            transitions[2].snapshot.queue[0].merge_group_sha.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(
            transitions[3].snapshot.queue[0].checks[0]
                .conclusion
                .as_deref(),
            Some("failure")
        );
        assert_eq!(
            transitions[3].snapshot.required_contexts,
            ["macos".to_owned()]
        );
        assert_eq!(
            transitions[4].snapshot.pull_requests[0].head_sha,
            "cccccccccccccccccccccccccccccccccccccccc"
        );
        assert_eq!(
            transitions[5].snapshot.ownership.hold_reason.as_deref(),
            Some("queue monitor surface55 owns mutation")
        );
        assert!(transitions[5].snapshot.ownership.hold_active);
        assert_eq!(
            transitions[6].snapshot.main_sha,
            "dddddddddddddddddddddddddddddddddddddddd"
        );
        assert!(transitions[6].snapshot.pull_requests.is_empty());
    }

    #[test]
    fn replay_without_merge_queue_or_branch_rule_keeps_pr_head_and_checks() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/queue-observer-no-governance.json");
        let frame = load_replay_frames(&fixture)
            .expect("fixture")
            .pop()
            .expect("one frame");
        let snapshot = parse_snapshot(&frame.graphql, "acme/forge", "main", &[], frame.ownership)
            .expect("snapshot");

        assert_eq!(
            snapshot.main_sha,
            "2222222222222222222222222222222222222222"
        );
        assert!(snapshot.truncated);
        assert!(snapshot.required_contexts.is_empty());
        assert!(snapshot.queue.is_empty());
        assert_eq!(snapshot.pull_requests.len(), 1);
        assert_eq!(
            snapshot.pull_requests[0].head_sha,
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        );
        assert_eq!(snapshot.pull_requests[0].checks.len(), 1);
        assert_eq!(snapshot.pull_requests[0].checks[0].name, "build");
        assert!(!snapshot.pull_requests[0].checks[0].required);
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_graphql_with_only_governance_errors_preserves_partial_snapshot() {
        let temp = tempfile::tempdir().expect("temp");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/queue-observer-no-governance.json");
        let frame = load_replay_frames(&fixture)
            .expect("fixture")
            .pop()
            .expect("one frame");
        let body = temp.path().join("graphql.json");
        fs::write(
            &body,
            serde_json::to_vec(&frame.graphql).expect("encode graphql"),
        )
        .expect("write graphql");
        let gh = temp.path().join("gh");
        write_executable(
            &gh,
            &format!(
                "#!/bin/sh\ncat '{}'\nprintf '%s\\n' 'governance fields unavailable' >&2\nexit 1\n",
                body.display()
            ),
        );
        let config = LoadedConfig {
            data: toml::Table::new(),
            global_dir: temp.path().join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        };
        let actions =
            GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(&gh);

        let body = fetch_snapshot(&actions, "acme/forge", "main")
            .expect("governance-only error should preserve partial data");
        let snapshot = parse_snapshot(
            &body,
            "acme/forge",
            "main",
            &[],
            OwnershipSnapshot::default(),
        )
        .expect("snapshot");

        assert!(snapshot.queue.is_empty());
        assert!(snapshot.required_contexts.is_empty());
        assert_eq!(snapshot.pull_requests[0].number, 24);
        assert_eq!(snapshot.pull_requests[0].checks[0].name, "build");
    }
}
