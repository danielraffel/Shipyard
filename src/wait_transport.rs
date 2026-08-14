use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::wait::TruthResult;

/// Common result type for wait snapshot fetches and evaluator calls.
pub type WaitResult<T> = Result<T, Box<dyn std::error::Error>>;

/// Outcome reported by `shipyard wait`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Flat booleans are part of the CLI JSON contract.
pub struct WaitOutcome {
    /// Whether the condition matched.
    pub matched: bool,
    /// Last observed state passed back from the evaluator.
    pub observed: BTreeMap<String, Value>,
    /// Transport mode used to drive the wait.
    pub transport: String,
    /// Whether the transport fell back away from daemon live updates.
    pub fallback_used: bool,
    /// Number of live events processed.
    pub events_received: u64,
    /// Whether the overall wait timed out.
    pub timed_out: bool,
    /// Whether a daemon/live path was unavailable.
    pub daemon_unavailable: bool,
    /// Whether `--no-fallback` forced an early exit.
    pub fallback_disabled_hit: bool,
    /// Total elapsed wall-clock seconds.
    pub elapsed_seconds: f64,
}

impl WaitOutcome {
    #[must_use]
    fn daemon_default() -> Self {
        Self {
            transport: "daemon".to_owned(),
            ..Self::default()
        }
    }

    #[must_use]
    fn polling_default() -> Self {
        Self {
            transport: "polling".to_owned(),
            daemon_unavailable: true,
            ..Self::default()
        }
    }
}

#[cfg(unix)]
struct DaemonConnection {
    reader: BufReader<UnixStream>,
}

#[cfg(not(unix))]
struct DaemonConnection;

#[cfg_attr(not(unix), allow(dead_code))]
enum DaemonEventOutcome {
    Event(Value),
    Timeout,
    Disconnect,
}

#[cfg(unix)]
impl DaemonConnection {
    fn read_next_relevant_event<P>(
        &mut self,
        event_filter: &P,
        timeout: Duration,
    ) -> DaemonEventOutcome
    where
        P: Fn(&Value) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return DaemonEventOutcome::Timeout;
            }

            let _ = self
                .reader
                .get_mut()
                .set_read_timeout(Some(remaining.min(Duration::from_millis(250))));

            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Ok(0) | Err(_) => return DaemonEventOutcome::Disconnect,
                Ok(_) => {
                    let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
                    match message.get("type").and_then(Value::as_str) {
                        Some("event") if event_filter(&message) => {
                            return DaemonEventOutcome::Event(message);
                        }
                        Some("goodbye") => return DaemonEventOutcome::Disconnect,
                        _ => {}
                    }
                }
            }
        }
    }
}

#[cfg(not(unix))]
impl DaemonConnection {
    #[allow(clippy::unused_self)]
    fn read_next_relevant_event<P>(
        &mut self,
        _event_filter: &P,
        _timeout: Duration,
    ) -> DaemonEventOutcome
    where
        P: Fn(&Value) -> bool,
    {
        DaemonEventOutcome::Disconnect
    }
}

/// Run the canonical wait loop.
///
/// The transport mirrors the Python contract:
/// 1. best-effort daemon subscribe
/// 2. authoritative first snapshot
/// 3. daemon-driven re-evaluation when available
/// 4. polling fallback only when the daemon is unavailable or disconnects
pub fn wait_for_condition<F, E, P>(
    mut evaluator: E,
    mut fetch_snapshot: F,
    event_filter: P,
    timeout_seconds: f64,
    poll_interval_seconds: f64,
    no_fallback: bool,
    socket_path: &Path,
) -> WaitResult<WaitOutcome>
where
    F: FnMut() -> WaitResult<Option<Value>>,
    E: FnMut(Option<&Value>) -> WaitResult<TruthResult>,
    P: Fn(&Value) -> bool,
{
    let start = Instant::now();
    let timeout = Duration::from_secs_f64(timeout_seconds.max(0.0));
    let poll_interval = Duration::from_secs_f64(poll_interval_seconds.max(0.01));
    let mut connection = try_connect(socket_path);
    let mut outcome = if connection.is_some() {
        WaitOutcome::daemon_default()
    } else {
        WaitOutcome::polling_default()
    };

    let first_snapshot = fetch_snapshot()?;
    let first_result = evaluator(first_snapshot.as_ref())?;
    outcome.observed = first_result.observed;
    outcome.matched = first_result.matched;
    if first_result.matched {
        outcome.elapsed_seconds = start.elapsed().as_secs_f64();
        return Ok(outcome);
    }

    if connection.is_none() && no_fallback {
        outcome.fallback_disabled_hit = true;
        outcome.elapsed_seconds = start.elapsed().as_secs_f64();
        return Ok(outcome);
    }

    if let Some(mut connection) = connection.take() {
        loop {
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                outcome.timed_out = true;
                outcome.elapsed_seconds = start.elapsed().as_secs_f64();
                return Ok(outcome);
            }

            match connection.read_next_relevant_event(&event_filter, remaining) {
                DaemonEventOutcome::Event(_event) => {
                    outcome.events_received += 1;
                    let snapshot = fetch_snapshot()?;
                    let result = evaluator(snapshot.as_ref())?;
                    outcome.observed = result.observed;
                    if result.matched {
                        outcome.matched = true;
                        outcome.elapsed_seconds = start.elapsed().as_secs_f64();
                        return Ok(outcome);
                    }
                }
                DaemonEventOutcome::Timeout => {
                    outcome.timed_out = true;
                    outcome.elapsed_seconds = start.elapsed().as_secs_f64();
                    return Ok(outcome);
                }
                DaemonEventOutcome::Disconnect => {
                    outcome.daemon_unavailable = true;
                    if no_fallback {
                        outcome.fallback_disabled_hit = true;
                        outcome.elapsed_seconds = start.elapsed().as_secs_f64();
                        return Ok(outcome);
                    }

                    "polling".clone_into(&mut outcome.transport);
                    outcome.fallback_used = true;
                    break;
                }
            }
        }
    }

    while start.elapsed() < timeout {
        let remaining = timeout.saturating_sub(start.elapsed());
        thread::sleep(poll_interval.min(remaining));

        let snapshot = fetch_snapshot()?;
        let result = evaluator(snapshot.as_ref())?;
        outcome.observed = result.observed;
        if result.matched {
            outcome.matched = true;
            outcome.elapsed_seconds = start.elapsed().as_secs_f64();
            return Ok(outcome);
        }
    }

    outcome.timed_out = true;
    outcome.elapsed_seconds = start.elapsed().as_secs_f64();
    Ok(outcome)
}

/// Read a JSON snapshot file for tests and local development.
pub fn read_snapshot_file(path: &Path) -> WaitResult<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)?;
    let value = serde_json::from_str::<Value>(&contents)?;
    Ok((!value.is_null()).then_some(value))
}

/// Fetch a GitHub release snapshot.
pub fn fetch_release_snapshot(repo: &str, tag: &str, cwd: &Path) -> WaitResult<Option<Value>> {
    let client = gh_client(cwd)?;
    run_gh_json(
        &client,
        &[
            "api".to_owned(),
            format!("repos/{repo}/releases/tags/{tag}"),
            "-H".to_owned(),
            "Accept: application/vnd.github+json".to_owned(),
        ],
        cwd,
        15.0,
    )
}

/// Fetch a GitHub PR snapshot.
///
/// First tries `gh pr view --json …` (GraphQL under the hood). When GraphQL
/// is rate-limited, falls back to synthesising the same shape from REST:
/// `gh api repos/:r/pulls/:n` for the PR fields plus
/// `gh api repos/:r/commits/:sha/check-runs` for the check rollup. Matches
/// the same fallback pattern `src/pr.rs` and `src/app/auto_merge_cmd.rs` use.
pub fn fetch_pr_snapshot(repo: &str, pr_number: u64, cwd: &Path) -> WaitResult<Option<Value>> {
    let client = gh_client(cwd)?;
    match run_gh_capturing(
        &client,
        &[
            "pr".to_owned(),
            "view".to_owned(),
            pr_number.to_string(),
            "--repo".to_owned(),
            repo.to_owned(),
            "--json".to_owned(),
            PR_VIEW_JSON_FIELDS.to_owned(),
        ],
        cwd,
    )? {
        GhOutcome::Success(stdout) => {
            let value = serde_json::from_slice::<Value>(&stdout)?;
            Ok(value.is_object().then_some(value))
        }
        GhOutcome::GraphqlRateLimited => {
            crate::pr::report_rate_limit_fallback_with_client(&client, "gh pr snapshot", cwd);
            fetch_pr_snapshot_rest_with_client(&client, repo, pr_number, cwd)
        }
        GhOutcome::OtherFailure => Ok(None),
    }
}

// Keep this list limited to fields accepted by `gh pr view --json`. In
// particular, `merged` is not a supported field; a merged PR is represented by
// `state == "MERGED"` and normalized by the evaluator.
const PR_VIEW_JSON_FIELDS: &str =
    "number,headRefOid,state,mergeable,mergeStateStatus,statusCheckRollup";

/// REST fallback for `fetch_pr_snapshot`. Synthesises the GraphQL-shape value
/// `evaluate_pr_green` / `evaluate_pr_state` consume.
///
/// Note: REST `check-runs` does NOT carry per-check `isRequired`; we emit the
/// rollup without that field. `evaluate_pr_check_rollup` then falls back to
/// `entry.get("isRequired").as_bool().unwrap_or(true)`-equivalent semantics
/// — every check is treated as required. That's stricter than GraphQL but
/// safe: a green REST evaluation cannot incorrectly report green when
/// non-required checks fail.
pub fn fetch_pr_snapshot_rest(repo: &str, pr_number: u64, cwd: &Path) -> WaitResult<Option<Value>> {
    let client = gh_client(cwd)?;
    fetch_pr_snapshot_rest_with_client(&client, repo, pr_number, cwd)
}

fn fetch_pr_snapshot_rest_with_client(
    client: &GhClient,
    repo: &str,
    pr_number: u64,
    cwd: &Path,
) -> WaitResult<Option<Value>> {
    let pr_value = match run_gh_capturing(
        client,
        &[
            "api".to_owned(),
            format!("repos/{repo}/pulls/{pr_number}"),
            "-H".to_owned(),
            "Accept: application/vnd.github+json".to_owned(),
        ],
        cwd,
    )? {
        GhOutcome::Success(stdout) => serde_json::from_slice::<Value>(&stdout)?,
        GhOutcome::GraphqlRateLimited | GhOutcome::OtherFailure => return Ok(None),
    };

    let head_sha = pr_value
        .get("head")
        .and_then(|h| h.get("sha"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let check_runs = if head_sha.is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        match run_gh_capturing(
            client,
            &[
                "api".to_owned(),
                format!("repos/{repo}/commits/{head_sha}/check-runs?per_page=100"),
                "-H".to_owned(),
                "Accept: application/vnd.github+json".to_owned(),
            ],
            cwd,
        )? {
            GhOutcome::Success(stdout) => serde_json::from_slice::<Value>(&stdout)?,
            GhOutcome::GraphqlRateLimited | GhOutcome::OtherFailure => {
                Value::Object(serde_json::Map::new())
            }
        }
    };

    Ok(Some(synthesize_pr_snapshot_from_rest(
        pr_number,
        &pr_value,
        &check_runs,
    )))
}

/// Pure transform: combine `gh api repos/:r/pulls/:n` + `gh api repos/:r/commits/:sha/check-runs`
/// into the GraphQL `gh pr view --json` shape that `evaluate_pr_green` /
/// `evaluate_pr_state` consume. Carries a `_rest_fallback: true` marker so
/// debug output / tests can disambiguate the source.
pub fn synthesize_pr_snapshot_from_rest(pr_number: u64, pr: &Value, check_runs: &Value) -> Value {
    let head_sha = pr
        .get("head")
        .and_then(|h| h.get("sha"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let state = pr
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_uppercase();
    let merged = pr.get("merged").and_then(Value::as_bool).unwrap_or(false);
    let mergeable = pr.get("mergeable").cloned().unwrap_or(Value::Null);
    let mergeable_state = pr
        .get("mergeable_state")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN")
        .to_uppercase();
    let mut rollup: Vec<Value> = Vec::new();
    if let Some(runs) = check_runs.get("check_runs").and_then(Value::as_array) {
        for run in runs {
            let name = run.get("name").and_then(Value::as_str).unwrap_or("");
            let status = run.get("status").and_then(Value::as_str).unwrap_or("");
            let conclusion = run.get("conclusion").cloned().unwrap_or(Value::Null);
            rollup.push(serde_json::json!({
                "name": name,
                "state": status,
                "conclusion": conclusion,
            }));
        }
    }
    serde_json::json!({
        "number": pr_number,
        "headRefOid": head_sha,
        "state": state,
        "merged": merged,
        "mergeable": mergeable,
        "mergeStateStatus": mergeable_state,
        "statusCheckRollup": rollup,
        "_rest_fallback": true,
    })
}

/// Fetch a GitHub Actions workflow-run snapshot.
pub fn fetch_run_snapshot(repo: &str, run_id: &str, cwd: &Path) -> WaitResult<Option<Value>> {
    let client = gh_client(cwd)?;
    run_gh_json(
        &client,
        &[
            "run".to_owned(),
            "view".to_owned(),
            run_id.to_owned(),
            "--repo".to_owned(),
            repo.to_owned(),
            "--json".to_owned(),
            "databaseId,status,conclusion,headSha,workflowName,url".to_owned(),
        ],
        cwd,
        15.0,
    )
}

/// Forward events that plausibly concern a target PR.
pub fn pr_event_filter(pr_number: u64, repo: &str) -> impl Fn(&Value) -> bool {
    let repo = repo.to_owned();
    move |event| {
        let Some(kind) = event_kind(event) else {
            return false;
        };
        let Some(payload) = event_payload(event) else {
            return false;
        };
        match kind {
            "pull_request" => payload.get("number").and_then(Value::as_u64) == Some(pr_number),
            "check_run" | "check_suite" => {
                payload
                    .get("pull_request_numbers")
                    .and_then(Value::as_array)
                    .is_some_and(|numbers| {
                        numbers
                            .iter()
                            .any(|number| number.as_u64() == Some(pr_number))
                    })
                    || payload_repo(payload) == Some(repo.as_str())
            }
            "workflow_run" => payload_repo(payload) == Some(repo.as_str()),
            "reconcile_healed" => {
                payload.get("pr").and_then(Value::as_u64) == Some(pr_number)
                    && payload_repo(payload) == Some(repo.as_str())
            }
            _ => false,
        }
    }
}

/// Forward events that plausibly concern a target workflow run.
pub fn run_event_filter(run_id: &str, repo: &str) -> impl Fn(&Value) -> bool {
    let repo = repo.to_owned();
    let run_id = run_id.to_owned();
    move |event| {
        let Some(kind) = event_kind(event) else {
            return false;
        };
        let Some(payload) = event_payload(event) else {
            return false;
        };
        match kind {
            "workflow_run" => {
                value_matches_text(payload.get("run_id"), &run_id)
                    && payload_repo(payload) == Some(repo.as_str())
            }
            "workflow_job" => value_matches_text(payload.get("run_id"), &run_id),
            _ => false,
        }
    }
}

/// Forward events that plausibly concern a target release tag.
pub fn release_event_filter(tag: &str, repo: &str) -> impl Fn(&Value) -> bool {
    let repo = repo.to_owned();
    let tag = tag.to_owned();
    move |event| {
        let Some(kind) = event_kind(event) else {
            return false;
        };
        let Some(payload) = event_payload(event) else {
            return false;
        };
        kind == "release"
            && payload.get("tag_name").and_then(Value::as_str) == Some(tag.as_str())
            && payload_repo(payload) == Some(repo.as_str())
    }
}

fn gh_client(cwd: &Path) -> WaitResult<GhClient> {
    Ok(GhClient::from_cwd(RuntimeMode::Shipyard, cwd)?)
}

fn run_gh_json(
    client: &GhClient,
    args: &[String],
    cwd: &Path,
    timeout_seconds: f64,
) -> WaitResult<Option<Value>> {
    let output = client
        .prepare_command(cwd, None, GhSupervision::Supervised, GhAuthPolicy::Default)?
        .args(args)
        .output()?;

    let _ = timeout_seconds;

    if !output.status.success() {
        return Ok(None);
    }

    let value = serde_json::from_slice::<Value>(&output.stdout)?;
    Ok(value.is_object().then_some(value))
}

/// Outcome of a `gh` invocation, classified by whether stderr looks like a
/// GraphQL rate-limit (so callers can opt into a REST fallback).
enum GhOutcome {
    Success(Vec<u8>),
    GraphqlRateLimited,
    OtherFailure,
}

fn run_gh_capturing(client: &GhClient, args: &[String], cwd: &Path) -> WaitResult<GhOutcome> {
    let output = client
        .prepare_command(cwd, None, GhSupervision::Supervised, GhAuthPolicy::Default)?
        .args(args)
        .output()?;
    if output.status.success() {
        return Ok(GhOutcome::Success(output.stdout));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if crate::pr::is_graphql_rate_limited(&stderr) {
        return Ok(GhOutcome::GraphqlRateLimited);
    }
    Ok(GhOutcome::OtherFailure)
}

#[cfg(unix)]
fn try_connect(socket_path: &Path) -> Option<DaemonConnection> {
    if !socket_path.exists() {
        return None;
    }

    let mut stream = UnixStream::connect(socket_path).ok()?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    stream.write_all(br#"{"type":"subscribe"}"#).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.flush().ok()?;
    Some(DaemonConnection {
        reader: BufReader::new(stream),
    })
}

#[cfg(not(unix))]
fn try_connect(_socket_path: &Path) -> Option<DaemonConnection> {
    None
}

fn event_kind(event: &Value) -> Option<&str> {
    event.get("kind").and_then(Value::as_str)
}

fn event_payload(event: &Value) -> Option<&serde_json::Map<String, Value>> {
    event.get("payload").and_then(Value::as_object)
}

fn payload_repo(payload: &serde_json::Map<String, Value>) -> Option<&str> {
    payload.get("repo").and_then(Value::as_str)
}

fn value_matches_text(value: Option<&Value>, expected: &str) -> bool {
    value.is_some_and(|value| match value {
        Value::String(text) => text == expected,
        Value::Number(number) => number.to_string() == expected,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use serde_json::Value;
    #[cfg(unix)]
    use serde_json::json;

    use super::{
        PR_VIEW_JSON_FIELDS, WaitOutcome, pr_event_filter, read_snapshot_file,
        release_event_filter, run_event_filter, synthesize_pr_snapshot_from_rest,
        wait_for_condition,
    };
    #[cfg(unix)]
    use crate::daemon_ipc::{IpcServer, IpcState};
    use crate::wait::TruthResult;

    #[cfg(unix)]
    fn dummy_state() -> IpcState {
        IpcState {
            tunnel_backend: "tailscale".to_owned(),
            tunnel_url: None,
            tunnel_verified_at: None,
            subscribers: 0,
            last_event_at: None,
            registered_repos: Vec::new(),
            rate_limit: None,
            last_error: None,
        }
    }

    #[test]
    fn snapshot_match_returns_immediately() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let outcome = wait_for_condition(
            |snapshot| {
                Ok(TruthResult {
                    matched: snapshot
                        .and_then(|snapshot| snapshot.get("status"))
                        .and_then(Value::as_str)
                        == Some("completed"),
                    observed: [(
                        "status".to_owned(),
                        snapshot
                            .and_then(|snapshot| snapshot.get("status"))
                            .cloned()
                            .unwrap_or(Value::Null),
                    )]
                    .into_iter()
                    .collect(),
                })
            },
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Some(serde_json::json!({"status": "completed"})))
            },
            |_| true,
            5.0,
            0.05,
            true,
            &socket_path,
        )
        .expect("wait");

        assert!(outcome.matched);
        assert!(!outcome.fallback_disabled_hit);
        assert_eq!(outcome.transport, "polling");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn no_fallback_snapshot_miss_returns_early() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let outcome = wait_for_condition(
            |snapshot| {
                Ok(TruthResult {
                    matched: snapshot
                        .and_then(|snapshot| snapshot.get("status"))
                        .and_then(Value::as_str)
                        == Some("completed"),
                    observed: std::collections::BTreeMap::new(),
                })
            },
            || Ok(Some(serde_json::json!({"status": "pending"}))),
            |_| true,
            5.0,
            0.05,
            true,
            &socket_path,
        )
        .expect("wait");

        assert!(!outcome.matched);
        assert!(outcome.fallback_disabled_hit);
        assert!(!outcome.timed_out);
    }

    #[test]
    fn polling_can_match_after_multiple_fetches() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let outcome = wait_for_condition(
            |snapshot| {
                Ok(TruthResult {
                    matched: snapshot
                        .and_then(|snapshot| snapshot.get("status"))
                        .and_then(Value::as_str)
                        == Some("completed"),
                    observed: std::collections::BTreeMap::new(),
                })
            },
            move || {
                let count = counter.fetch_add(1, Ordering::SeqCst);
                let status = if count >= 2 { "completed" } else { "pending" };
                Ok(Some(serde_json::json!({"status": status})))
            },
            |_| true,
            1.0,
            0.01,
            false,
            &socket_path,
        )
        .expect("wait");

        assert!(outcome.matched);
        assert!(calls.load(Ordering::SeqCst) >= 3);
    }

    #[test]
    fn timeout_is_reported_when_condition_never_matches() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let outcome = wait_for_condition(
            |_| {
                Ok(TruthResult {
                    matched: false,
                    observed: std::collections::BTreeMap::new(),
                })
            },
            || Ok(Some(serde_json::json!({"status": "pending"}))),
            |_| true,
            0.03,
            0.01,
            false,
            &socket_path,
        )
        .expect("wait");

        assert_eq!(
            outcome,
            WaitOutcome {
                timed_out: true,
                transport: "polling".to_owned(),
                daemon_unavailable: true,
                elapsed_seconds: outcome.elapsed_seconds,
                ..WaitOutcome::default()
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_happy_path_live_event_triggers_re_evaluation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let mut server = IpcServer::new(socket_path.clone(), dummy_state);
        server.start().expect("start");

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let waiter = std::thread::spawn(move || {
            wait_for_condition(
                |snapshot| {
                    Ok(TruthResult {
                        matched: snapshot
                            .and_then(|snapshot| snapshot.get("status"))
                            .and_then(Value::as_str)
                            == Some("completed"),
                        observed: snapshot
                            .and_then(Value::as_object)
                            .map(|snapshot| {
                                snapshot
                                    .iter()
                                    .map(|(key, value)| (key.clone(), value.clone()))
                                    .collect::<std::collections::BTreeMap<_, _>>()
                            })
                            .unwrap_or_default(),
                    })
                },
                move || {
                    let count = counter.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(json!({
                        "status": if count == 0 { "pending" } else { "completed" }
                    })))
                },
                pr_event_filter(42, "o/r"),
                3.0,
                0.05,
                false,
                &socket_path,
            )
            .expect("wait")
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while server.subscriber_count() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        server.broadcast_event(json!({
            "kind": "pull_request",
            "payload": {"number": 42}
        }));

        let outcome = waiter.join().expect("join");
        server.stop().expect("stop");

        assert!(outcome.matched);
        assert_eq!(outcome.transport, "daemon");
        assert_eq!(outcome.events_received, 1);
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_disconnect_falls_back_to_polling() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let mut server = IpcServer::new(socket_path.clone(), dummy_state);
        server.start().expect("start");

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let waiter = std::thread::spawn(move || {
            wait_for_condition(
                |snapshot| {
                    Ok(TruthResult {
                        matched: snapshot
                            .and_then(|snapshot| snapshot.get("status"))
                            .and_then(Value::as_str)
                            == Some("completed"),
                        observed: std::collections::BTreeMap::new(),
                    })
                },
                move || {
                    let count = counter.fetch_add(1, Ordering::SeqCst);
                    let status = if count >= 2 { "completed" } else { "pending" };
                    Ok(Some(json!({"status": status})))
                },
                |_| true,
                2.0,
                0.02,
                false,
                &socket_path,
            )
            .expect("wait")
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while server.subscriber_count() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        server.stop().expect("stop");

        let outcome = waiter.join().expect("join");
        assert!(outcome.matched);
        assert_eq!(outcome.transport, "polling");
        assert!(outcome.fallback_used);
        assert!(outcome.daemon_unavailable);
    }

    #[test]
    fn pr_event_filter_drops_unrelated_events() {
        let filter = pr_event_filter(151, "o/r");
        assert!(filter(&serde_json::json!({
            "kind": "pull_request",
            "payload": {"number": 151}
        })));
        assert!(!filter(&serde_json::json!({
            "kind": "pull_request",
            "payload": {"number": 9999}
        })));
        assert!(filter(&serde_json::json!({
            "kind": "check_run",
            "payload": {"pull_request_numbers": [151], "repo": "o/r"}
        })));
        assert!(filter(&serde_json::json!({
            "kind": "reconcile_healed",
            "payload": {"pr": 151, "repo": "o/r"}
        })));
        assert!(!filter(&serde_json::json!({
            "kind": "workflow_job",
            "payload": {"repo": "o/r"}
        })));
    }

    #[test]
    fn run_and_release_event_filters_match_only_expected_payloads() {
        let run_filter = run_event_filter("24446948064", "o/r");
        assert!(run_filter(&serde_json::json!({
            "kind": "workflow_run",
            "payload": {"run_id": "24446948064", "repo": "o/r"}
        })));
        assert!(run_filter(&serde_json::json!({
            "kind": "workflow_job",
            "payload": {"run_id": 24_446_948_064_u64}
        })));
        assert!(!run_filter(&serde_json::json!({
            "kind": "workflow_run",
            "payload": {"run_id": "12", "repo": "o/r"}
        })));

        let release_filter = release_event_filter("v1.2.3", "o/r");
        assert!(release_filter(&serde_json::json!({
            "kind": "release",
            "payload": {"tag_name": "v1.2.3", "repo": "o/r"}
        })));
        assert!(!release_filter(&serde_json::json!({
            "kind": "release",
            "payload": {"tag_name": "v9.9.9", "repo": "o/r"}
        })));
    }

    #[test]
    fn rest_fallback_synthesis_matches_graphql_shape_for_green_pr() {
        let pr = serde_json::json!({
            "number": 287,
            "state": "open",
            "merged": false,
            "mergeable": true,
            "mergeable_state": "clean",
            "head": { "sha": "abc123" },
        });
        let check_runs = serde_json::json!({
            "total_count": 2,
            "check_runs": [
                {"name": "CI", "status": "completed", "conclusion": "success"},
                {"name": "Coverage >= 75%", "status": "completed", "conclusion": "success"},
            ],
        });
        let snapshot = synthesize_pr_snapshot_from_rest(287, &pr, &check_runs);
        assert_eq!(snapshot["number"], 287);
        assert_eq!(snapshot["headRefOid"], "abc123");
        assert_eq!(snapshot["state"], "OPEN");
        assert_eq!(snapshot["mergeStateStatus"], "CLEAN");
        assert_eq!(snapshot["merged"], false);
        assert_eq!(snapshot["mergeable"], true);
        assert_eq!(snapshot["_rest_fallback"], true);
        let rollup = snapshot["statusCheckRollup"].as_array().expect("rollup");
        assert_eq!(rollup.len(), 2);
        assert_eq!(rollup[0]["name"], "CI");
        assert_eq!(rollup[0]["state"], "completed");
        assert_eq!(rollup[0]["conclusion"], "success");
    }

    #[test]
    fn pr_view_query_uses_only_supported_gh_json_fields() {
        assert!(PR_VIEW_JSON_FIELDS.split(',').any(|field| field == "state"));
        assert!(
            !PR_VIEW_JSON_FIELDS
                .split(',')
                .any(|field| field == "merged")
        );
    }

    #[test]
    fn rest_fallback_synthesis_handles_missing_check_runs_array() {
        // If the check-runs call failed (GhOutcome::OtherFailure path) we pass
        // an empty object — the rollup should come out as an empty array, not
        // an error.
        let pr = serde_json::json!({
            "number": 1,
            "state": "open",
            "merged": false,
            "mergeable": null,
            "mergeable_state": "unknown",
            "head": { "sha": "deadbeef" },
        });
        let check_runs = serde_json::json!({});
        let snapshot = synthesize_pr_snapshot_from_rest(1, &pr, &check_runs);
        assert_eq!(snapshot["headRefOid"], "deadbeef");
        assert_eq!(snapshot["mergeStateStatus"], "UNKNOWN");
        assert_eq!(snapshot["statusCheckRollup"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn rest_fallback_synthesis_uppercases_state_and_mergeable_state() {
        // GraphQL emits these in SCREAMING_CASE; REST gives lowercase.
        // The evaluator's upper_entry_value already handles either case, but
        // synthesise to GraphQL's shape to minimise downstream surprise.
        let pr = serde_json::json!({
            "number": 9,
            "state": "closed",
            "merged": true,
            "mergeable": false,
            "mergeable_state": "behind",
            "head": { "sha": "h" },
        });
        let snapshot = synthesize_pr_snapshot_from_rest(9, &pr, &serde_json::json!({}));
        assert_eq!(snapshot["state"], "CLOSED");
        assert_eq!(snapshot["mergeStateStatus"], "BEHIND");
        assert_eq!(snapshot["merged"], true);
    }

    #[test]
    fn snapshot_file_loader_supports_missing_and_null() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(
            read_snapshot_file(&temp.path().join("missing.json"))
                .expect("read")
                .is_none()
        );

        let path = temp.path().join("snapshot.json");
        std::fs::write(&path, "null\n").expect("write");
        assert!(read_snapshot_file(&path).expect("read").is_none());

        std::fs::write(&path, "{\"status\":\"completed\"}\n").expect("write");
        assert_eq!(
            read_snapshot_file(&path).expect("read").expect("snapshot")["status"],
            "completed"
        );
    }
}
