use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::gh::{GhAuthPolicy, GhClient, GhPrepareError, GhSupervision};
use crate::identity::RuntimeMode;
use crate::merge_steward::RequiredCheck;
use crate::required_check_policy::{
    classic_required_checks, encode_path_segment, evaluated_required_checks,
    normalize_required_checks,
};
use crate::wait::TruthResult;

const SNAPSHOT_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// Number of transient GitHub snapshot/auth failures retried in-process.
    pub transient_errors: u64,
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

enum SnapshotFetch {
    Snapshot(Option<Value>),
    TimedOut,
}

fn fetch_snapshot_resilient<F>(
    fetch_snapshot: &mut F,
    start: &Instant,
    timeout: Duration,
    poll_interval: Duration,
    transient_errors: &mut u64,
) -> WaitResult<SnapshotFetch>
where
    F: FnMut(Duration) -> WaitResult<Option<Value>>,
{
    let retry_interval = poll_interval
        .max(Duration::from_millis(250))
        .min(Duration::from_secs(5));
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Ok(SnapshotFetch::TimedOut);
        }
        let fetched = fetch_snapshot(remaining);
        let timed_out = timeout.saturating_sub(start.elapsed()).is_zero();
        match fetched {
            Err(error) if is_transient_snapshot_error(error.as_ref()) => {
                *transient_errors += 1;
                if timed_out {
                    return Ok(SnapshotFetch::TimedOut);
                }
                let remaining = timeout.saturating_sub(start.elapsed());
                thread::sleep(retry_interval.min(remaining));
            }
            _ if timed_out => return Ok(SnapshotFetch::TimedOut),
            Ok(snapshot) => return Ok(SnapshotFetch::Snapshot(snapshot)),
            Err(error) => return Err(error),
        }
    }
}

fn is_transient_snapshot_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<GhPrepareError>()
        .is_some_and(GhPrepareError::is_transient)
        || error.downcast_ref::<SnapshotCommandTimeout>().is_some()
}

fn fetch_and_evaluate<F, E>(
    fetch_snapshot: &mut F,
    evaluator: &mut E,
    start: &Instant,
    timeout: Duration,
    poll_interval: Duration,
    transient_errors: &mut u64,
) -> WaitResult<Option<TruthResult>>
where
    F: FnMut(Duration) -> WaitResult<Option<Value>>,
    E: FnMut(Option<&Value>) -> WaitResult<TruthResult>,
{
    match fetch_snapshot_resilient(
        fetch_snapshot,
        start,
        timeout,
        poll_interval,
        transient_errors,
    )? {
        SnapshotFetch::Snapshot(snapshot) => evaluator(snapshot.as_ref()).map(Some),
        SnapshotFetch::TimedOut => Ok(None),
    }
}

fn mark_timed_out(outcome: &mut WaitOutcome, start: &Instant) {
    outcome.timed_out = true;
    outcome.elapsed_seconds = start.elapsed().as_secs_f64();
}

fn record_evaluation(outcome: &mut WaitOutcome, result: TruthResult, start: &Instant) -> bool {
    outcome.observed = result.observed;
    outcome.matched = result.matched;
    if result.matched {
        outcome.elapsed_seconds = start.elapsed().as_secs_f64();
    }
    result.matched
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
/// 3. daemon-driven re-evaluation plus periodic authoritative reconciliation
/// 4. polling fallback only when the daemon is unavailable or disconnects
pub fn wait_for_condition<F, E, P>(
    evaluator: E,
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
    wait_for_condition_with_timeout(
        evaluator,
        move |_| fetch_snapshot(),
        event_filter,
        timeout_seconds,
        poll_interval_seconds,
        no_fallback,
        socket_path,
    )
}

pub(crate) fn wait_for_condition_with_timeout<F, E, P>(
    mut evaluator: E,
    mut fetch_snapshot: F,
    event_filter: P,
    timeout_seconds: f64,
    poll_interval_seconds: f64,
    no_fallback: bool,
    socket_path: &Path,
) -> WaitResult<WaitOutcome>
where
    F: FnMut(Duration) -> WaitResult<Option<Value>>,
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

    let Some(first_result) = fetch_and_evaluate(
        &mut fetch_snapshot,
        &mut evaluator,
        &start,
        timeout,
        poll_interval,
        &mut outcome.transient_errors,
    )?
    else {
        mark_timed_out(&mut outcome, &start);
        return Ok(outcome);
    };
    if record_evaluation(&mut outcome, first_result, &start) {
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

            match connection.read_next_relevant_event(&event_filter, remaining.min(poll_interval)) {
                DaemonEventOutcome::Event(_event) => {
                    outcome.events_received += 1;
                }
                DaemonEventOutcome::Timeout => {}
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

            let Some(result) = fetch_and_evaluate(
                &mut fetch_snapshot,
                &mut evaluator,
                &start,
                timeout,
                poll_interval,
                &mut outcome.transient_errors,
            )?
            else {
                mark_timed_out(&mut outcome, &start);
                return Ok(outcome);
            };
            if record_evaluation(&mut outcome, result, &start) {
                return Ok(outcome);
            }
        }
    }

    while start.elapsed() < timeout {
        let remaining = timeout.saturating_sub(start.elapsed());
        thread::sleep(poll_interval.min(remaining));

        let Some(result) = fetch_and_evaluate(
            &mut fetch_snapshot,
            &mut evaluator,
            &start,
            timeout,
            poll_interval,
            &mut outcome.transient_errors,
        )?
        else {
            mark_timed_out(&mut outcome, &start);
            return Ok(outcome);
        };
        if record_evaluation(&mut outcome, result, &start) {
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
    fetch_release_snapshot_with_timeout(repo, tag, cwd, Duration::from_secs(15))
}

pub(crate) fn fetch_release_snapshot_with_timeout(
    repo: &str,
    tag: &str,
    cwd: &Path,
    timeout: Duration,
) -> WaitResult<Option<Value>> {
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
        timeout,
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
    fetch_pr_snapshot_with_timeout(repo, pr_number, cwd, Duration::from_secs(15))
}

pub(crate) fn fetch_pr_snapshot_with_timeout(
    repo: &str,
    pr_number: u64,
    cwd: &Path,
    timeout: Duration,
) -> WaitResult<Option<Value>> {
    let client = gh_client(cwd)?;
    fetch_pr_snapshot_with_client(&client, repo, pr_number, cwd, timeout)
}

pub(crate) fn fetch_pr_snapshot_with_client(
    client: &GhClient,
    repo: &str,
    pr_number: u64,
    cwd: &Path,
    timeout: Duration,
) -> WaitResult<Option<Value>> {
    let started = Instant::now();
    match run_gh_capturing(
        client,
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
        timeout,
    )? {
        GhOutcome::Success(stdout) => {
            let mut value = serde_json::from_slice::<Value>(&stdout)?;
            if !value.is_object() {
                return Ok(None);
            }
            let base = value
                .get("baseRefName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let remaining = remaining_snapshot_timeout(timeout, started.elapsed());
            let policy = fetch_required_check_policy(client, repo, &base, cwd, remaining)?;
            match policy {
                Some(required) => {
                    let remaining = remaining_snapshot_timeout(timeout, started.elapsed());
                    let materialized = fetch_materialized_required_checks(
                        client, repo, pr_number, cwd, remaining,
                    )?;
                    annotate_required_checks(
                        &mut value,
                        &required,
                        materialized.as_deref().unwrap_or_default(),
                    );
                }
                None => {
                    value["_required_checks_known"] = Value::Bool(false);
                }
            }
            Ok(Some(value))
        }
        GhOutcome::GraphqlRateLimited => {
            // The shared reporter performs a best-effort reset-time probe. A
            // waiter cannot make that extra unbounded call without violating
            // its overall deadline, so retain the notice and skip the probe.
            let _ = crate::writer_domain_lease::write_stderr(format_args!(
                "shipyard: GraphQL rate limit hit for gh pr snapshot. Falling back to REST."
            ));
            let remaining = timeout.saturating_sub(started.elapsed());
            fetch_pr_snapshot_rest_with_client(client, repo, pr_number, cwd, remaining)
        }
        GhOutcome::OtherFailure => Ok(None),
    }
}

// Keep this list limited to fields accepted by `gh pr view --json`. In
// particular, `merged` is not a supported field; a merged PR is represented by
// `state == "MERGED"` and normalized by the evaluator.
const PR_VIEW_JSON_FIELDS: &str =
    "number,headRefName,headRefOid,baseRefName,state,mergeable,mergeStateStatus,statusCheckRollup";
const PR_CHECKS_JSON_FIELDS: &str = "name,state,bucket,link";

fn fetch_required_check_policy(
    client: &GhClient,
    repo: &str,
    base: &str,
    cwd: &Path,
    timeout: Duration,
) -> WaitResult<Option<Vec<RequiredCheck>>> {
    if base.is_empty() {
        return Ok(None);
    }
    let started = Instant::now();
    let encoded_base = encode_path_segment(base);
    let evaluated_endpoint = format!("repos/{repo}/rules/branches/{encoded_base}");
    let evaluated_output = run_gh_output(
        client,
        &["api", "--paginate", "--slurp", evaluated_endpoint.as_str()],
        cwd,
        timeout,
    )?;
    if !evaluated_output.status.success() {
        return Ok(None);
    }
    let evaluated_value = serde_json::from_slice::<Value>(&evaluated_output.stdout)?;
    let mut required = evaluated_required_checks(&evaluated_value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;

    let classic_endpoint =
        format!("repos/{repo}/branches/{encoded_base}/protection/required_status_checks");
    let classic_output = run_gh_output(
        client,
        &["api", classic_endpoint.as_str()],
        cwd,
        remaining_snapshot_timeout(timeout, started.elapsed()),
    )?;
    if classic_output.status.success() {
        let classic_value = serde_json::from_slice::<Value>(&classic_output.stdout)?;
        required.extend(
            classic_required_checks(&classic_value)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?,
        );
    } else {
        let stderr = String::from_utf8_lossy(&classic_output.stderr);
        if !stderr.contains("HTTP 404") {
            return Ok(None);
        }
    }
    Ok(Some(normalize_required_checks(required)))
}

fn fetch_materialized_required_checks(
    client: &GhClient,
    repo: &str,
    pr_number: u64,
    cwd: &Path,
    timeout: Duration,
) -> WaitResult<Option<Vec<Value>>> {
    let pr_number = pr_number.to_string();
    let output = run_gh_output(
        client,
        &[
            "pr",
            "checks",
            pr_number.as_str(),
            "--repo",
            repo,
            "--required",
            "--json",
            PR_CHECKS_JSON_FIELDS,
        ],
        cwd,
        timeout,
    )?;

    if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout)
        && let Some(required) = value.as_array()
    {
        return Ok(Some(required.clone()));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("no required checks reported") {
        return Ok(Some(Vec::new()));
    }
    Ok(None)
}

fn annotate_required_checks(
    snapshot: &mut Value,
    policy: &[RequiredCheck],
    materialized: &[Value],
) {
    let Some(snapshot) = snapshot.as_object_mut() else {
        return;
    };
    snapshot.insert("_required_checks_known".to_owned(), Value::Bool(true));

    let rollup = snapshot
        .entry("statusCheckRollup")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(rollup) = rollup.as_array_mut() else {
        return;
    };

    for entry in rollup.iter_mut().filter_map(Value::as_object_mut) {
        entry.insert("isRequired".to_owned(), Value::Bool(false));
    }

    let mut materialized_by_name = BTreeMap::<String, Vec<Value>>::new();
    for entry in materialized {
        if let Some(name) = entry.get("name").and_then(Value::as_str) {
            materialized_by_name
                .entry(name.to_owned())
                .or_default()
                .push(entry.clone());
        }
    }

    for required in policy {
        let observed = materialized_by_name
            .get_mut(&required.context)
            .and_then(Vec::pop);
        let state = observed
            .as_ref()
            .and_then(|entry| entry.get("state"))
            .and_then(Value::as_str)
            .filter(|state| !state.is_empty())
            .unwrap_or("PENDING");
        rollup.push(required_check_rollup_entry(&required.context, state));
    }

    // A materialized check that GitHub classifies as required but which was
    // absent from both policy APIs is still a blocker. Retaining it fails
    // closed across transient policy-surface drift.
    for (name, entries) in materialized_by_name {
        for entry in entries {
            let state = entry
                .get("state")
                .and_then(Value::as_str)
                .filter(|state| !state.is_empty())
                .unwrap_or("PENDING");
            rollup.push(required_check_rollup_entry(&name, state));
        }
    }
}

fn required_check_rollup_entry(name: &str, state: &str) -> Value {
    let upper = state.to_ascii_uppercase();
    let conclusion = if matches!(upper.as_str(), "QUEUED" | "IN_PROGRESS" | "PENDING") {
        Value::Null
    } else {
        Value::String(upper.clone())
    };
    serde_json::json!({
        "name": name,
        "state": upper,
        "conclusion": conclusion,
        "isRequired": true,
    })
}

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
    fetch_pr_snapshot_rest_with_timeout(repo, pr_number, cwd, Duration::from_secs(15))
}

pub(crate) fn fetch_pr_snapshot_rest_with_timeout(
    repo: &str,
    pr_number: u64,
    cwd: &Path,
    timeout: Duration,
) -> WaitResult<Option<Value>> {
    let client = gh_client(cwd)?;
    fetch_pr_snapshot_rest_with_client(&client, repo, pr_number, cwd, timeout)
}

fn fetch_pr_snapshot_rest_with_client(
    client: &GhClient,
    repo: &str,
    pr_number: u64,
    cwd: &Path,
    timeout: Duration,
) -> WaitResult<Option<Value>> {
    let started = Instant::now();
    let pr_value = match run_gh_capturing(
        client,
        &[
            "api".to_owned(),
            format!("repos/{repo}/pulls/{pr_number}"),
            "-H".to_owned(),
            "Accept: application/vnd.github+json".to_owned(),
        ],
        cwd,
        timeout,
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
            remaining_snapshot_timeout(timeout, started.elapsed()),
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
    let head_ref = pr
        .get("head")
        .and_then(|h| h.get("ref"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let base_ref = pr
        .get("base")
        .and_then(|b| b.get("ref"))
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
                "isRequired": true,
            }));
        }
    }
    serde_json::json!({
        "number": pr_number,
        "headRefName": head_ref,
        "headRefOid": head_sha,
        "baseRefName": base_ref,
        "state": state,
        "merged": merged,
        "mergeable": mergeable,
        "mergeStateStatus": mergeable_state,
        "statusCheckRollup": rollup,
        "_required_checks_known": false,
        "_rest_fallback": true,
    })
}

/// Fetch a GitHub Actions workflow-run snapshot.
pub fn fetch_run_snapshot(repo: &str, run_id: &str, cwd: &Path) -> WaitResult<Option<Value>> {
    fetch_run_snapshot_with_timeout(repo, run_id, cwd, Duration::from_secs(15))
}

pub(crate) fn fetch_run_snapshot_with_timeout(
    repo: &str,
    run_id: &str,
    cwd: &Path,
    timeout: Duration,
) -> WaitResult<Option<Value>> {
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
        timeout,
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
    timeout: Duration,
) -> WaitResult<Option<Value>> {
    let string_args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = run_gh_output(client, &string_args, cwd, timeout)?;

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

fn run_gh_capturing(
    client: &GhClient,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> WaitResult<GhOutcome> {
    let string_args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = run_gh_output(client, &string_args, cwd, timeout)?;
    if output.status.success() {
        return Ok(GhOutcome::Success(output.stdout));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if crate::pr::is_graphql_rate_limited(&stderr) {
        return Ok(GhOutcome::GraphqlRateLimited);
    }
    Ok(GhOutcome::OtherFailure)
}

fn snapshot_command_timeout(remaining: Duration) -> Duration {
    remaining.min(SNAPSHOT_COMMAND_TIMEOUT)
}

fn remaining_snapshot_timeout(timeout: Duration, elapsed: Duration) -> Duration {
    timeout.saturating_sub(elapsed)
}

#[derive(Debug)]
struct SnapshotCommandTimeout {
    timeout: Duration,
}

impl std::fmt::Display for SnapshotCommandTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "GitHub snapshot command timed out after {}ms",
            self.timeout.as_millis()
        )
    }
}

impl std::error::Error for SnapshotCommandTimeout {}

fn run_gh_output(
    client: &GhClient,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> WaitResult<Output> {
    let timeout = snapshot_command_timeout(timeout);
    if timeout.is_zero() {
        return Err(Box::new(SnapshotCommandTimeout { timeout }));
    }

    let started = Instant::now();
    let mut command = client.prepare_command_with_auth_timeout(
        cwd,
        None,
        GhSupervision::Supervised,
        GhAuthPolicy::Default,
        timeout,
    )?;
    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(Box::new(SnapshotCommandTimeout { timeout }));
    }

    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    let mut process = crate::process::ProcessTree::spawn(&mut command)?;
    let Some(status) = process.wait_timeout(remaining)? else {
        process.terminate();
        return Err(Box::new(SnapshotCommandTimeout { timeout }));
    };
    process.terminate();

    let read_output = |file: &mut std::fs::File| -> std::io::Result<Vec<u8>> {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    Ok(Output {
        status,
        stdout: read_output(&mut stdout)?,
        stderr: read_output(&mut stderr)?,
    })
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
    use std::time::Duration;
    #[cfg(unix)]
    use std::time::Instant;

    use serde_json::Value;
    #[cfg(unix)]
    use serde_json::json;

    use super::{
        PR_CHECKS_JSON_FIELDS, PR_VIEW_JSON_FIELDS, WaitOutcome, annotate_required_checks,
        pr_event_filter, read_snapshot_file, release_event_filter, remaining_snapshot_timeout,
        run_event_filter, snapshot_command_timeout, synthesize_pr_snapshot_from_rest,
        wait_for_condition_with_timeout,
    };
    #[cfg(unix)]
    use crate::daemon_ipc::{IpcServer, IpcState};
    use crate::gh::GhPrepareError;
    use crate::merge_steward::RequiredCheck;
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
            configured_repos: Vec::new(),
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
        let outcome = wait_for_condition_with_timeout(
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
            move |_| {
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
    fn snapshot_command_timeout_is_capped_at_fifteen_seconds() {
        assert_eq!(
            snapshot_command_timeout(Duration::from_secs(30)),
            Duration::from_secs(15)
        );
        assert_eq!(
            snapshot_command_timeout(Duration::from_secs(7)),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn multi_step_snapshot_timeout_uses_remaining_overall_budget() {
        let overall = Duration::from_secs(40);

        assert_eq!(
            remaining_snapshot_timeout(overall, Duration::from_secs(10)),
            Duration::from_secs(30)
        );
        assert_eq!(
            snapshot_command_timeout(remaining_snapshot_timeout(overall, Duration::from_secs(10))),
            Duration::from_secs(15)
        );
        assert_eq!(
            remaining_snapshot_timeout(overall, Duration::from_secs(32)),
            Duration::from_secs(8)
        );
        assert_eq!(
            remaining_snapshot_timeout(overall, Duration::from_secs(41)),
            Duration::ZERO
        );
    }

    #[test]
    fn no_fallback_snapshot_miss_returns_early() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let outcome = wait_for_condition_with_timeout(
            |snapshot| {
                Ok(TruthResult {
                    matched: snapshot
                        .and_then(|snapshot| snapshot.get("status"))
                        .and_then(Value::as_str)
                        == Some("completed"),
                    observed: std::collections::BTreeMap::new(),
                })
            },
            |_| Ok(Some(serde_json::json!({"status": "pending"}))),
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
        let outcome = wait_for_condition_with_timeout(
            |snapshot| {
                Ok(TruthResult {
                    matched: snapshot
                        .and_then(|snapshot| snapshot.get("status"))
                        .and_then(Value::as_str)
                        == Some("completed"),
                    observed: std::collections::BTreeMap::new(),
                })
            },
            move |_| {
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
    fn transient_token_helper_failure_is_retried_until_snapshot_matches() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let outcome = wait_for_condition_with_timeout(
            |snapshot| {
                Ok(TruthResult {
                    matched: snapshot
                        .and_then(|snapshot| snapshot.get("status"))
                        .and_then(Value::as_str)
                        == Some("completed"),
                    observed: std::collections::BTreeMap::new(),
                })
            },
            move |_| {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(Box::new(GhPrepareError::HelperFailed {
                        program: "helper".to_owned(),
                        status: Some(1),
                        stderr: "GitHub API request failed: connection reset by peer".to_owned(),
                    }) as Box<dyn std::error::Error>);
                }
                Ok(Some(serde_json::json!({"status": "completed"})))
            },
            |_| true,
            1.0,
            0.01,
            true,
            &socket_path,
        )
        .expect("transient helper failure should recover");

        assert!(outcome.matched);
        assert_eq!(outcome.transient_errors, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn repeated_transient_token_helper_failures_consume_only_overall_timeout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let outcome = wait_for_condition_with_timeout(
            |_| {
                Ok(TruthResult {
                    matched: false,
                    observed: std::collections::BTreeMap::new(),
                })
            },
            |_| {
                Err(Box::new(GhPrepareError::HelperFailed {
                    program: "helper".to_owned(),
                    status: Some(1),
                    stderr: "service unavailable".to_owned(),
                }) as Box<dyn std::error::Error>)
            },
            |_| true,
            0.03,
            0.01,
            false,
            &socket_path,
        )
        .expect("transient failures should time out normally");

        assert!(outcome.timed_out);
        assert!(!outcome.matched);
        assert!(outcome.transient_errors >= 1);
    }

    #[test]
    fn retry_does_not_start_another_snapshot_after_the_deadline() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let outcome = wait_for_condition_with_timeout(
            |_| {
                Ok(TruthResult {
                    matched: false,
                    observed: std::collections::BTreeMap::new(),
                })
            },
            move |remaining| {
                counter.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
                Err(Box::new(GhPrepareError::HelperFailed {
                    program: "helper".to_owned(),
                    status: Some(1),
                    stderr: "service unavailable".to_owned(),
                }) as Box<dyn std::error::Error>)
            },
            |_| true,
            0.01,
            0.01,
            false,
            &socket_path,
        )
        .expect("transient failure should consume the deadline");

        assert!(outcome.timed_out);
        assert_eq!(outcome.transient_errors, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn snapshot_returned_after_its_budget_times_out_without_evaluation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let evaluations = Arc::new(AtomicUsize::new(0));
        let evaluation_count = Arc::clone(&evaluations);
        let outcome = wait_for_condition_with_timeout(
            move |snapshot| {
                evaluation_count.fetch_add(1, Ordering::SeqCst);
                Ok(TruthResult {
                    matched: snapshot
                        .and_then(|snapshot| snapshot.get("status"))
                        .and_then(Value::as_str)
                        == Some("completed"),
                    observed: std::collections::BTreeMap::new(),
                })
            },
            |remaining| {
                std::thread::sleep(remaining + Duration::from_millis(10));
                Ok(Some(serde_json::json!({"status": "completed"})))
            },
            |_| true,
            0.01,
            0.01,
            false,
            &socket_path,
        )
        .expect("late snapshot should become an overall timeout");

        assert!(outcome.timed_out);
        assert!(!outcome.matched);
        assert_eq!(evaluations.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn permanent_token_helper_failure_is_not_hidden_until_timeout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let result = wait_for_condition_with_timeout(
            |_| {
                Ok(TruthResult {
                    matched: false,
                    observed: std::collections::BTreeMap::new(),
                })
            },
            |_| {
                Err(Box::new(GhPrepareError::HelperFailed {
                    program: "helper".to_owned(),
                    status: Some(1),
                    stderr: "HTTP 401: bad credentials".to_owned(),
                }) as Box<dyn std::error::Error>)
            },
            |_| true,
            1.0,
            0.01,
            false,
            &socket_path,
        );

        let error = result.expect_err("permanent helper failure should surface");
        assert!(error.downcast_ref::<GhPrepareError>().is_some());
    }

    #[test]
    fn timeout_is_reported_when_condition_never_matches() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let outcome = wait_for_condition_with_timeout(
            |_| {
                Ok(TruthResult {
                    matched: false,
                    observed: std::collections::BTreeMap::new(),
                })
            },
            |_| Ok(Some(serde_json::json!({"status": "pending"}))),
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
            wait_for_condition_with_timeout(
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
                move |_| {
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
    fn daemon_reconciles_snapshot_when_matching_event_is_missed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("daemon.sock");
        let mut server = IpcServer::new(socket_path.clone(), dummy_state);
        server.start().expect("start");

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let outcome = wait_for_condition_with_timeout(
            |snapshot| {
                Ok(TruthResult {
                    matched: snapshot
                        .and_then(|snapshot| snapshot.get("status"))
                        .and_then(Value::as_str)
                        == Some("completed"),
                    observed: std::collections::BTreeMap::new(),
                })
            },
            move |_| {
                let count = counter.fetch_add(1, Ordering::SeqCst);
                Ok(Some(json!({
                    "status": if count == 0 { "pending" } else { "completed" }
                })))
            },
            pr_event_filter(42, "o/r"),
            2.0,
            0.02,
            true,
            &socket_path,
        )
        .expect("wait");
        server.stop().expect("stop");

        assert!(outcome.matched);
        assert_eq!(outcome.transport, "daemon");
        assert!(!outcome.fallback_used);
        assert!(!outcome.daemon_unavailable);
        assert!(!outcome.fallback_disabled_hit);
        assert_eq!(outcome.events_received, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
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
            wait_for_condition_with_timeout(
                |snapshot| {
                    Ok(TruthResult {
                        matched: snapshot
                            .and_then(|snapshot| snapshot.get("status"))
                            .and_then(Value::as_str)
                            == Some("completed"),
                        observed: std::collections::BTreeMap::new(),
                    })
                },
                move |_| {
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
        assert_eq!(snapshot["_required_checks_known"], false);
        let rollup = snapshot["statusCheckRollup"].as_array().expect("rollup");
        assert_eq!(rollup.len(), 2);
        assert_eq!(rollup[0]["name"], "CI");
        assert_eq!(rollup[0]["state"], "completed");
        assert_eq!(rollup[0]["conclusion"], "success");
        assert_eq!(rollup[0]["isRequired"], true);
    }

    #[test]
    fn pr_view_query_uses_only_supported_gh_json_fields() {
        assert!(PR_VIEW_JSON_FIELDS.split(',').any(|field| field == "state"));
        assert!(
            PR_VIEW_JSON_FIELDS
                .split(',')
                .any(|field| field == "baseRefName")
        );
        assert!(
            !PR_VIEW_JSON_FIELDS
                .split(',')
                .any(|field| field == "merged")
        );
    }

    #[test]
    fn required_check_query_and_annotation_preserve_advisory_failures() {
        assert_eq!(PR_CHECKS_JSON_FIELDS, "name,state,bucket,link");
        let mut snapshot = serde_json::json!({
            "statusCheckRollup": [
                {"name": "macos", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"name": "coverage", "status": "COMPLETED", "conclusion": "FAILURE"}
            ]
        });
        annotate_required_checks(
            &mut snapshot,
            &[RequiredCheck {
                context: "macos".to_owned(),
                app_id: Some(42),
            }],
            &[serde_json::json!({
                "name": "macos",
                "state": "SUCCESS",
                "bucket": "pass",
                "link": "https://github.test/check/1"
            })],
        );
        assert_eq!(snapshot["_required_checks_known"], true);
        assert_eq!(snapshot["statusCheckRollup"][0]["isRequired"], false);
        assert_eq!(snapshot["statusCheckRollup"][1]["isRequired"], false);
        assert_eq!(snapshot["statusCheckRollup"][2]["name"], "macos");
        assert_eq!(snapshot["statusCheckRollup"][2]["isRequired"], true);
        assert_eq!(snapshot["statusCheckRollup"][2]["conclusion"], "SUCCESS");
    }

    #[test]
    fn required_check_annotation_adds_missing_status_context() {
        let mut snapshot = serde_json::json!({"statusCheckRollup": []});
        annotate_required_checks(
            &mut snapshot,
            &[RequiredCheck {
                context: "required-status".to_owned(),
                app_id: None,
            }],
            &[serde_json::json!({
                "name": "required-status",
                "state": "FAILURE",
                "bucket": "fail",
                "link": "https://github.test/status/1"
            })],
        );
        assert_eq!(snapshot["statusCheckRollup"][0]["name"], "required-status");
        assert_eq!(snapshot["statusCheckRollup"][0]["conclusion"], "FAILURE");
        assert_eq!(snapshot["statusCheckRollup"][0]["isRequired"], true);
    }

    #[test]
    fn required_policy_materializes_missing_context_as_pending() {
        let mut snapshot = serde_json::json!({
            "statusCheckRollup": [
                {"name": "advisory", "status": "COMPLETED", "conclusion": "SUCCESS"}
            ]
        });
        annotate_required_checks(
            &mut snapshot,
            &[
                RequiredCheck {
                    context: "present".to_owned(),
                    app_id: Some(42),
                },
                RequiredCheck {
                    context: "missing".to_owned(),
                    app_id: Some(42),
                },
            ],
            &[serde_json::json!({
                "name": "present",
                "state": "SUCCESS",
                "bucket": "pass",
                "link": "https://github.test/check/1"
            })],
        );
        assert_eq!(snapshot["statusCheckRollup"][0]["isRequired"], false);
        assert_eq!(snapshot["statusCheckRollup"][1]["name"], "present");
        assert_eq!(snapshot["statusCheckRollup"][1]["conclusion"], "SUCCESS");
        assert_eq!(snapshot["statusCheckRollup"][2]["name"], "missing");
        assert_eq!(snapshot["statusCheckRollup"][2]["state"], "PENDING");
        assert_eq!(snapshot["statusCheckRollup"][2]["conclusion"], Value::Null);
    }

    #[test]
    fn same_name_advisory_cannot_substitute_for_missing_required_producer() {
        let mut snapshot = serde_json::json!({
            "statusCheckRollup": [
                {"name": "macos", "status": "COMPLETED", "conclusion": "SUCCESS"}
            ]
        });
        annotate_required_checks(
            &mut snapshot,
            &[RequiredCheck {
                context: "macos".to_owned(),
                app_id: Some(42),
            }],
            &[],
        );
        assert_eq!(snapshot["statusCheckRollup"][0]["isRequired"], false);
        assert_eq!(snapshot["statusCheckRollup"][1]["name"], "macos");
        assert_eq!(snapshot["statusCheckRollup"][1]["state"], "PENDING");
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
