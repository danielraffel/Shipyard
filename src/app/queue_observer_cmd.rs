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
    next_poll_seconds, observe, parse_snapshot, render_markdown, save_state,
};

// One GraphQL request returns base SHA, bounded open PR heads/checks/labels,
// classic required contexts, and server-owned merge-queue order/group heads.
// This is intentionally a query literal; no mutation operation exists in this
// module or in the command surface.
const SNAPSHOT_QUERY: &str = r"query($owner:String!,$name:String!,$branch:String!,$qualified:String!){repository(owner:$owner,name:$name){url baseRef:ref(qualifiedName:$qualified){target{... on Commit{oid}} branchProtectionRule{requiredStatusCheckContexts}} pullRequests(first:100,states:OPEN,baseRefName:$branch,orderBy:{field:UPDATED_AT,direction:DESC}){nodes{number url headRefOid mergeStateStatus autoMergeRequest{enabledAt} assignees(first:20){nodes{login} pageInfo{hasNextPage}} labels(first:40){nodes{name} pageInfo{hasNextPage}} statusCheckRollup{contexts(first:100){nodes{__typename ... on CheckRun{databaseId name status conclusion detailsUrl startedAt checkSuite{createdAt}} ... on StatusContext{context state targetUrl createdAt}} pageInfo{hasNextPage}}}} pageInfo{hasNextPage}} mergeQueue(branch:$branch){entries(first:100){nodes{position enqueuedAt headCommit{oid statusCheckRollup{contexts(first:100){nodes{__typename ... on CheckRun{databaseId name status conclusion detailsUrl startedAt checkSuite{createdAt}} ... on StatusContext{context state targetUrl createdAt}} pageInfo{hasNextPage}}}} pullRequest{number url headRefOid}} pageInfo{hasNextPage}}}}}";
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
            let snapshot = parse_snapshot(
                &frame.graphql,
                &repo,
                &args.base,
                &configured_required,
                frame.ownership,
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

    let actions = GitHubActions::from_loaded_config(cwd, config);
    let mut polls = 0;
    let mut failures = 0_usize;
    loop {
        let snapshot = fetch_snapshot(&actions, &repo, &args.base).and_then(|body| {
            let ownership = collect_ownership(
                &runtime_paths.state_dir,
                &runtime_paths.global_dir,
                cwd,
                mode,
            );
            parse_snapshot(&body, &repo, &args.base, &configured_required, ownership)
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
                eprintln!(
                    "queue observer read failed; retrying in {delay}s: {}",
                    error.message
                );
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

fn acquire_observer_lock(state_path: &Path) -> Result<fs::File, CliFailure> {
    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            CliFailure::new(1, format!("create observer state directory: {error}"))
        })?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state_path.with_extension("lock"))
        .map_err(|error| CliFailure::new(1, format!("open observer lock: {error}")))?;
    lock.try_lock_exclusive().map_err(|error| {
        CliFailure::new(
            1,
            format!("queue observer already active for this state path: {error}"),
        )
    })?;
    Ok(lock)
}

fn validate_distinct_paths(state_path: &Path, log_path: &Path) -> Result<(), CliFailure> {
    if state_path == log_path {
        return Err(CliFailure::new(
            2,
            "queue observer --state-file and --transition-log must be different paths",
        ));
    }
    Ok(())
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
    }
    if let Some(transition) = &result.transition {
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
    let raw = actions
        .run_gh_with_timeout(&args, SNAPSHOT_ATTEMPT_TIMEOUT)
        .map_err(|error| CliFailure::new(1, format!("read queue snapshot: {error}")))?;
    serde_json::from_str(&raw)
        .map_err(|error| CliFailure::new(1, format!("parse queue snapshot: {error}")))
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
    match hold {
        Ok(Some(value)) => {
            snapshot.hold_reason = optional_json_string(&value, "reason");
            snapshot.hold_machine = optional_json_string(&value, "machine");
            snapshot.held_at = optional_json_string(&value, "held_at");
        }
        Ok(None) => {}
        Err(error) => {
            let message = format!("hold unreadable: {error}");
            snapshot.blocker = Some(
                snapshot
                    .blocker
                    .map_or(message.clone(), |current| format!("{current}; {message}")),
            );
        }
    }
    snapshot
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
        assert_eq!(
            transitions[6].snapshot.main_sha,
            "dddddddddddddddddddddddddddddddddddddddd"
        );
        assert!(transitions[6].snapshot.pull_requests.is_empty());
    }
}
