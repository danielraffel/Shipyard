//! Ship-state reconciliation against GitHub's current pull-request checks.
//!
//! Webhook delivery is best-effort. This module provides the deterministic
//! heal path that re-fetches `statusCheckRollup`, updates stale dispatched-run
//! statuses, mirrors terminal status into the GUI-facing evidence snapshot, and
//! reports transitions for daemon IPC subscribers.

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::Value;
use wait_timeout::ChildExt;

use crate::config::LoadedConfig;
use crate::evidence::canonical_repository;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::ship_state::{DispatchedRun, ShipState, ShipStateStore};

/// How often the daemon should run reconciliation after startup.
pub const RECONCILE_INTERVAL_SECONDS: u64 = 30;
/// Freshness window before terminal states are eligible for budgeted skips.
pub const RECONCILE_FRESH_WINDOW_SECONDS: i64 = 3_600;
/// Forced reconcile window for aged terminal states.
pub const RECONCILE_FORCED_WINDOW_SECONDS: i64 = 86_400;
/// Maximum time allowed for one `gh pr view` reconcile fetch.
pub const RECONCILE_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

const TERMINAL_RUN_STATUSES: &[&str] = &["completed", "passed", "failed", "cancelled", "canceled"];
const TERMINAL_EVIDENCE_STATUSES: &[&str] = &["pass", "fail", "reused", "skipped"];

/// In-memory forced-reconcile bookkeeping carried by the daemon process.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReconcileWindow {
    last_forced: BTreeMap<(String, u64), DateTime<Utc>>,
}

impl ReconcileWindow {
    /// Return the last successful forced-reconcile timestamp for a PR.
    #[must_use]
    pub fn last_forced_at(&self, repo: &str, pr: u64) -> Option<DateTime<Utc>> {
        self.last_forced
            .get(&(canonical_repository(repo), pr))
            .copied()
    }

    fn stamp(&mut self, repo: &str, pr: u64, now: DateTime<Utc>) {
        self.last_forced
            .insert((canonical_repository(repo), pr), now);
    }
}

/// One target status transition observed by reconcile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconcileTransition {
    /// Pull request number.
    pub pr: u64,
    /// Repository slug.
    pub repo: String,
    /// Target name.
    pub target: String,
    /// Status recorded before reconcile.
    pub from_status: String,
    /// Status recorded after reconcile.
    pub to_status: String,
}

/// Summary of one reconcile pass across active ship-state files.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReconcileReport {
    /// Number of active ship-state files rewritten.
    pub healed: usize,
    /// Per-target status transitions for daemon subscribers.
    pub transitions: Vec<ReconcileTransition>,
    /// Aged-terminal states skipped due to the forced-window budget.
    pub skipped_terminal: usize,
    /// Fetch or parse failures skipped without mutating local state.
    pub fetch_errors: usize,
}

/// Result of reconciling one ship-state value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciledShipState {
    /// Updated ship-state.
    pub state: ShipState,
    /// Per-target run-status transitions.
    pub transitions: Vec<ReconcileTransition>,
    /// Human-readable changes useful for diagnostics and tests.
    pub changes: Vec<String>,
}

/// Error returned while fetching GitHub check rollup data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReconcileFetchError {
    /// `gh` could not be started, so no GitHub request was attempted.
    Spawn(String),
    /// A started `gh` process could not be waited on or captured.
    Io(String),
    /// `gh` did not finish inside the reconcile timeout.
    Timeout(String),
    /// `gh` exited non-zero.
    Command(String),
    /// `gh` returned JSON that did not match the expected object shape.
    Parse(String),
    /// A configured GitHub auth boundary could not prepare a `gh` command.
    Prepare(String),
}

impl Display for ReconcileFetchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(message)
            | Self::Io(message)
            | Self::Timeout(message)
            | Self::Command(message)
            | Self::Parse(message)
            | Self::Prepare(message) => formatter.write_str(message),
        }
    }
}

/// Exhaustive producer-provenanced check snapshot plus exact API cost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvenancedCheckRollup {
    /// Live pull-request head.
    pub head_sha: String,
    /// Complete check/status nodes across all fetched pages.
    pub checks: Vec<Value>,
    /// GraphQL requests that crossed the command boundary.
    pub api_requests: usize,
}

/// Provenanced fetch failure with requests spent before failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvenancedFetchError {
    /// Stable typed fetch failure.
    pub error: ReconcileFetchError,
    /// GraphQL requests that crossed the command boundary.
    pub api_requests: usize,
}

impl Error for ReconcileFetchError {}

/// Reconcile all active ship-state files using the real `gh` shell boundary.
#[must_use]
pub fn reconcile_active_ship_states(
    state_dir: &Path,
    window: &mut ReconcileWindow,
) -> ReconcileReport {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let gh_client = GhClient::from_cwd(RuntimeMode::Shipyard, &cwd).map_err(|error| {
        format!("failed to load GitHub auth config during active reconcile: {error}")
    });
    reconcile_active_ship_states_with(state_dir, window, Utc::now(), |state| {
        let gh_client = gh_client
            .as_ref()
            .map_err(|error| ReconcileFetchError::Prepare(error.clone()))?;
        fetch_status_check_rollup_with_client(gh_client, &cwd, &state.repo, state.pr)
    })
}

/// Reconcile all active ship-state files with an injected fetcher.
#[must_use]
pub fn reconcile_active_ship_states_with<F>(
    state_dir: &Path,
    window: &mut ReconcileWindow,
    now: DateTime<Utc>,
    mut fetch: F,
) -> ReconcileReport
where
    F: FnMut(&ShipState) -> Result<Vec<Value>, ReconcileFetchError>,
{
    let Ok(store) = ShipStateStore::new(state_dir.join("ship")) else {
        return ReconcileReport::default();
    };
    let mut report = ReconcileReport::default();

    for state in store.list_active() {
        if is_aged_terminal(&state, window, now) {
            report.skipped_terminal += 1;
            continue;
        }
        let was_aged_candidate = is_aged_terminal_candidate(&state, now);
        let Ok(rollup) = fetch(&state) else {
            report.fetch_errors += 1;
            continue;
        };
        let mut reconciled_changes = Vec::new();
        let mut reconciled_transitions = Vec::new();
        let saved = store
            .with_pr_state_scoped_locked(&state.repo, state.pr, |current| {
                let Some(current_state) = current.as_ref() else {
                    return Ok(());
                };
                let reconciled = reconcile_ship_state(current_state, &rollup, now);
                if reconciled.changes.is_empty() {
                    return Ok(());
                }
                reconciled_changes = reconciled.changes;
                reconciled_transitions = reconciled.transitions;
                *current = Some(reconciled.state);
                Ok(())
            })
            .is_ok();
        if saved && !reconciled_changes.is_empty() {
            report.healed += 1;
            report.transitions.extend(reconciled_transitions);
        }
        if was_aged_candidate {
            window.stamp(&state.repo, state.pr, now);
        }
    }

    report
}

/// Reconcile one ship-state value against a GitHub check rollup.
#[must_use]
pub fn reconcile_ship_state(
    state: &ShipState,
    status_check_rollup: &[Value],
    now: DateTime<Utc>,
) -> ReconciledShipState {
    let mut next_state = state.clone();
    let mut transitions = Vec::new();
    let mut changes = Vec::new();
    let mut next_runs = Vec::with_capacity(state.dispatched_runs.len());

    for run in &state.dispatched_runs {
        // GitHub's check rollup can only authoritatively heal runs that were
        // dispatched through GitHub Actions. Local/SSH runs carry Shipyard's
        // own job id here; matching those by a short target name (for example
        // `mac`) can otherwise overwrite a failed local proof with an
        // unrelated green hosted check.
        if !run_is_github_actions_backed(run) {
            next_runs.push(run.clone());
            continue;
        }
        let Some(check) = match_check(run, status_check_rollup) else {
            next_runs.push(run.clone());
            continue;
        };
        let Some(new_status) = conclusion_to_run_status(check) else {
            next_runs.push(run.clone());
            continue;
        };

        if new_status == run.status {
            next_runs.push(run.clone());
        } else {
            changes.push(format!(
                "target={:?}: {:?} -> {:?} (matched check {:?})",
                run.target,
                run.status,
                new_status,
                check_name(check)
            ));
            transitions.push(ReconcileTransition {
                pr: state.pr,
                repo: state.repo.clone(),
                target: run.target.clone(),
                from_status: run.status.clone(),
                to_status: new_status.clone(),
            });
            next_runs.push(DispatchedRun {
                status: new_status.clone(),
                updated_at: now,
                ..run.clone()
            });
        }

        if let Some(evidence_status) = run_status_to_evidence(&new_status) {
            let current = next_state.evidence_snapshot.get(&run.target);
            if current.map(String::as_str) != Some(evidence_status) {
                changes.push(format!(
                    "evidence[{target:?}]: {before:?} -> {after:?}",
                    target = run.target,
                    before = current,
                    after = evidence_status
                ));
                next_state
                    .evidence_snapshot
                    .insert(run.target.clone(), evidence_status.to_owned());
            }
        }
    }

    next_state.dispatched_runs = next_runs;
    ReconciledShipState {
        state: next_state,
        transitions,
        changes,
    }
}

fn run_is_github_actions_backed(run: &DispatchedRun) -> bool {
    // Cloud target results persist GitHub's numeric workflow-run database id.
    // Local and SSH targets retain Shipyard's `sy-*` job id instead.
    run.run_id.parse::<u64>().is_ok()
}

/// Fetch `statusCheckRollup` for a PR through the GitHub CLI.
pub fn fetch_status_check_rollup(repo: &str, pr: u64) -> Result<Vec<Value>, ReconcileFetchError> {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    fetch_status_check_rollup_with_cwd(RuntimeMode::Shipyard, &cwd, repo, pr)
}

/// Fetch `statusCheckRollup` for a PR through the configured GitHub CLI boundary.
pub fn fetch_status_check_rollup_with_cwd(
    mode: RuntimeMode,
    cwd: &Path,
    repo: &str,
    pr: u64,
) -> Result<Vec<Value>, ReconcileFetchError> {
    let gh_client = GhClient::from_cwd(mode, cwd).map_err(|error| {
        ReconcileFetchError::Prepare(format!(
            "failed to load GitHub auth config while reconciling PR #{pr} ({repo}): {error}"
        ))
    })?;
    fetch_status_check_rollup_with_client(&gh_client, cwd, repo, pr)
}

fn fetch_status_check_rollup_with_client(
    gh_client: &GhClient,
    cwd: &Path,
    repo: &str,
    pr: u64,
) -> Result<Vec<Value>, ReconcileFetchError> {
    let (_head, rollup) = fetch_head_and_status_check_rollup_with_client(
        gh_client,
        cwd,
        repo,
        pr,
        "statusCheckRollup",
    )?;
    Ok(rollup)
}

/// Fetch the PR's live `headRefOid` alongside its `statusCheckRollup`, so a
/// caller can prove the rollup describes the exact SHA it validated before
/// acting on it. Returns `(head_ref_oid, rollup_entries)`.
pub fn fetch_head_and_status_check_rollup_with_cwd(
    mode: RuntimeMode,
    cwd: &Path,
    repo: &str,
    pr: u64,
) -> Result<(String, Vec<Value>), ReconcileFetchError> {
    let gh_client = GhClient::from_cwd(mode, cwd).map_err(|error| {
        ReconcileFetchError::Prepare(format!(
            "failed to load GitHub auth config while reconciling PR #{pr} ({repo}): {error}"
        ))
    })?;
    fetch_head_and_status_check_rollup_with_client(
        &gh_client,
        cwd,
        repo,
        pr,
        "headRefOid,statusCheckRollup",
    )
}

/// Fetch exact PR head/check state using the caller's already-loaded config.
/// Delayed workers need this to preserve command-based auth in a stripped
/// daemon environment instead of rediscovering credentials from ambient cwd.
pub fn fetch_head_and_status_check_rollup_with_config(
    config: &LoadedConfig,
    cwd: &Path,
    repo: &str,
    pr: u64,
) -> Result<(String, Vec<Value>), ReconcileFetchError> {
    let gh_client = GhClient::from_loaded_config(config).map_err(|error| {
        ReconcileFetchError::Prepare(format!(
            "failed to load GitHub auth config while reconciling PR #{pr} ({repo}): {error}"
        ))
    })?;
    fetch_head_and_status_check_rollup_with_client(
        &gh_client,
        cwd,
        repo,
        pr,
        "headRefOid,statusCheckRollup",
    )
}

/// Fetch an exact PR head plus complete hosted-check producer provenance.
///
/// Metadata-only authority must bind configured contexts to the GitHub App or
/// status creator that produced them. The ordinary `gh pr view` rollup omits
/// that identity, so this stricter query paginates a bounded exhaustive set.
pub fn fetch_head_and_provenanced_status_check_rollup_with_config(
    config: &LoadedConfig,
    cwd: &Path,
    repo: &str,
    pr: u64,
) -> Result<(String, Vec<Value>), ReconcileFetchError> {
    let gh_client = GhClient::from_loaded_config(config).map_err(|error| {
        ReconcileFetchError::Prepare(format!(
            "failed to load GitHub auth config while reconciling PR #{pr} ({repo}): {error}"
        ))
    })?;
    fetch_head_and_provenanced_status_check_rollup_with_client(
        &gh_client, cwd, repo, pr, None, None,
    )
    .map(|rollup| (rollup.head_sha, rollup.checks))
    .map_err(|failure| failure.error)
}

/// Fetch complete hosted-check provenance using exact repository auth routing
/// and a bounded token-helper deadline.
pub fn fetch_head_and_provenanced_status_check_rollup_for_repo_with_config(
    config: &LoadedConfig,
    cwd: &Path,
    repo: &str,
    pr: u64,
    auth_timeout: Duration,
) -> Result<ProvenancedCheckRollup, ProvenancedFetchError> {
    let gh_client =
        GhClient::from_loaded_config(config).map_err(|error| ProvenancedFetchError {
            error: ReconcileFetchError::Prepare(format!(
                "failed to load GitHub auth config while reconciling PR #{pr} ({repo}): {error}"
            )),
            api_requests: 0,
        })?;
    fetch_head_and_provenanced_status_check_rollup_for_repo_with_client(
        &gh_client,
        cwd,
        repo,
        pr,
        auth_timeout,
        Duration::from_mins(1),
    )
}

/// Fetch complete provenance from a reusable GitHub client. Each target gets
/// an exact repository override and pins one bounded App installation token
/// before pagination so concurrent repositories cannot evict each other.
pub fn fetch_head_and_provenanced_status_check_rollup_for_repo_with_client(
    gh_client: &GhClient,
    cwd: &Path,
    repo: &str,
    pr: u64,
    auth_timeout: Duration,
    observation_timeout: Duration,
) -> Result<ProvenancedCheckRollup, ProvenancedFetchError> {
    let deadline = Instant::now() + observation_timeout;
    let mut repo_client = gh_client
        .clone()
        .with_repo_override(repo)
        .map_err(|error| ProvenancedFetchError {
            error: ReconcileFetchError::Prepare(error.to_string()),
            api_requests: 0,
        })?;
    let summary = repo_client
        .pin_command_auth_with_timeout(
            cwd,
            auth_timeout.min(deadline.saturating_duration_since(Instant::now())),
        )
        .map_err(|error| ProvenancedFetchError {
            error: ReconcileFetchError::Prepare(error.to_string()),
            api_requests: 0,
        })?;
    if summary.token_kind.as_deref() != Some("github-app-installation") {
        return Err(ProvenancedFetchError {
            error: ReconcileFetchError::Prepare(
                "shadow observation requires repository-scoped GitHub App installation auth"
                    .to_owned(),
            ),
            api_requests: 0,
        });
    }
    fetch_head_and_provenanced_status_check_rollup_with_client(
        &repo_client,
        cwd,
        repo,
        pr,
        Some(auth_timeout),
        Some(deadline),
    )
}

fn fetch_head_and_provenanced_status_check_rollup_with_client(
    gh_client: &GhClient,
    cwd: &Path,
    repo: &str,
    pr: u64,
    auth_timeout: Option<Duration>,
    deadline: Option<Instant>,
) -> Result<ProvenancedCheckRollup, ProvenancedFetchError> {
    const MAX_PAGES: usize = 10;
    const QUERY: &str = r"query($owner:String!,$name:String!,$number:Int!,$cursor:String){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid statusCheckRollup{contexts(first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{__typename ... on CheckRun{name status conclusion checkSuite{app{databaseId slug}}} ... on StatusContext{context state creator{__typename login ... on User{databaseId} ... on Bot{databaseId} ... on Organization{databaseId}}}}}}}}}";
    let (owner, name) = repo.split_once('/').ok_or_else(|| ProvenancedFetchError {
        error: ReconcileFetchError::Prepare(format!("invalid repository identity '{repo}'")),
        api_requests: 0,
    })?;
    let mut api_requests = 0;
    let mut cursor = None::<String>;
    let mut expected_head = None::<String>;
    let mut checks = Vec::new();
    for _ in 0..MAX_PAGES {
        let parsed = fetch_provenanced_page(
            gh_client,
            cwd,
            owner,
            name,
            repo,
            pr,
            cursor.as_deref(),
            auth_timeout,
            QUERY,
            api_requests,
            deadline,
        )?;
        api_requests += 1;
        if expected_head
            .as_ref()
            .is_some_and(|head| head != &parsed.head_sha)
        {
            return Err(ProvenancedFetchError {
                error: ReconcileFetchError::Parse(
                    "pull-request head changed during provenanced pagination".to_owned(),
                ),
                api_requests,
            });
        }
        expected_head.get_or_insert(parsed.head_sha);
        checks.extend(parsed.checks);
        if !parsed.has_next_page {
            return Ok(ProvenancedCheckRollup {
                head_sha: expected_head.unwrap_or_default(),
                checks,
                api_requests,
            });
        }
        cursor = parsed.end_cursor;
        if cursor.is_none() {
            return Err(ProvenancedFetchError {
                error: ReconcileFetchError::Parse(
                    "provenanced hosted-check page omitted end cursor".to_owned(),
                ),
                api_requests,
            });
        }
    }
    Err(ProvenancedFetchError {
        error: ReconcileFetchError::Parse(
            "provenanced hosted-check observation exceeded bounded 1000-context budget".to_owned(),
        ),
        api_requests,
    })
}

#[allow(clippy::too_many_arguments)]
fn fetch_provenanced_page(
    gh_client: &GhClient,
    cwd: &Path,
    owner: &str,
    name: &str,
    repo: &str,
    pr: u64,
    cursor: Option<&str>,
    auth_timeout: Option<Duration>,
    query: &str,
    api_requests: usize,
    deadline: Option<Instant>,
) -> Result<ProvenancedPage, ProvenancedFetchError> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(ProvenancedFetchError {
            error: ReconcileFetchError::Timeout(
                "shadow observation exceeded its complete-pass deadline".to_owned(),
            ),
            api_requests,
        });
    }
    let prepared = match auth_timeout {
        Some(timeout) => gh_client.prepare_privileged_command_with_auth_timeout(
            cwd,
            GhSupervision::Unsupervised,
            deadline.map_or(timeout, |deadline| {
                timeout.min(deadline.saturating_duration_since(Instant::now()))
            }),
        ),
        None => gh_client.prepare_command(
            cwd,
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
        ),
    };
    let mut command = prepared.map_err(|error| ProvenancedFetchError {
        error: ReconcileFetchError::Prepare(error.to_string()),
        api_requests,
    })?;
    command.args([
        "api",
        "graphql",
        "-f",
        &format!("query={query}"),
        "-F",
        &format!("owner={owner}"),
        "-F",
        &format!("name={name}"),
        "-F",
        &format!("number={pr}"),
    ]);
    if let Some(cursor) = cursor {
        command.args(["-f", &format!("cursor={cursor}")]);
    }
    let request_timeout = deadline.map_or(RECONCILE_FETCH_TIMEOUT, |deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .min(RECONCILE_FETCH_TIMEOUT)
    });
    if request_timeout.is_zero() {
        return Err(ProvenancedFetchError {
            error: ReconcileFetchError::Timeout(
                "shadow observation exceeded its complete-pass deadline".to_owned(),
            ),
            api_requests,
        });
    }
    let capture = run_capture(command, request_timeout).map_err(|error| {
        let crossed_boundary = !matches!(error, ReconcileFetchError::Spawn(_));
        ProvenancedFetchError {
            error,
            api_requests: api_requests + usize::from(crossed_boundary),
        }
    })?;
    parse_provenanced_page(&capture, repo, pr).map_err(|error| ProvenancedFetchError {
        error,
        api_requests: api_requests + 1,
    })
}

#[derive(Debug)]
struct ProvenancedPage {
    head_sha: String,
    checks: Vec<Value>,
    has_next_page: bool,
    end_cursor: Option<String>,
}

fn parse_provenanced_page(
    capture: &CommandCapture,
    repo: &str,
    pr: u64,
) -> Result<ProvenancedPage, ReconcileFetchError> {
    if capture.timed_out {
        return Err(ReconcileFetchError::Timeout(format!(
            "gh api graphql timed out while observing PR #{pr} ({repo})"
        )));
    }
    if capture.returncode != Some(0) {
        return Err(ReconcileFetchError::Command(format!(
            "gh api graphql failed while observing PR #{pr} ({repo}): {}",
            capture.stderr_or_stdout()
        )));
    }
    let value = serde_json::from_str::<Value>(&capture.stdout).map_err(|error| {
        ReconcileFetchError::Parse(format!("failed to parse gh api graphql JSON: {error}"))
    })?;
    if value
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        return Err(ReconcileFetchError::Parse(
            "gh api graphql returned partial-data errors".to_owned(),
        ));
    }
    let pull = value
        .pointer("/data/repository/pullRequest")
        .ok_or_else(|| ReconcileFetchError::Parse("missing pullRequest object".to_owned()))?;
    let head_sha = pull
        .get("headRefOid")
        .and_then(Value::as_str)
        .filter(|head| !head.is_empty())
        .ok_or_else(|| ReconcileFetchError::Parse("missing pull-request head".to_owned()))?
        .to_owned();
    if pull.get("statusCheckRollup").is_none_or(Value::is_null) {
        return Ok(ProvenancedPage {
            head_sha,
            checks: Vec::new(),
            has_next_page: false,
            end_cursor: None,
        });
    }
    let contexts = pull
        .pointer("/statusCheckRollup/contexts")
        .ok_or_else(|| ReconcileFetchError::Parse("missing status-check contexts".to_owned()))?;
    let checks = contexts
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| ReconcileFetchError::Parse("missing status-check nodes".to_owned()))?
        .clone();
    let page_info = contexts
        .get("pageInfo")
        .ok_or_else(|| ReconcileFetchError::Parse("missing status-check pageInfo".to_owned()))?;
    let has_next_page = page_info
        .get("hasNextPage")
        .and_then(Value::as_bool)
        .ok_or_else(|| ReconcileFetchError::Parse("missing hasNextPage".to_owned()))?;
    let end_cursor = page_info
        .get("endCursor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(ProvenancedPage {
        head_sha,
        checks,
        has_next_page,
        end_cursor,
    })
}

fn fetch_head_and_status_check_rollup_with_client(
    gh_client: &GhClient,
    cwd: &Path,
    repo: &str,
    pr: u64,
    json_fields: &str,
) -> Result<(String, Vec<Value>), ReconcileFetchError> {
    let mut command = gh_client
        .prepare_command(
            cwd,
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
        )
        .map_err(|error| {
            ReconcileFetchError::Prepare(format!(
                "failed to prepare gh pr view while reconciling PR #{pr} ({repo}): {error}"
            ))
        })?;
    command.args([
        "pr",
        "view",
        &pr.to_string(),
        "--repo",
        repo,
        "--json",
        json_fields,
    ]);
    let capture = run_capture(command, RECONCILE_FETCH_TIMEOUT)?;
    if capture.timed_out {
        return Err(ReconcileFetchError::Timeout(format!(
            "gh pr view timed out while reconciling PR #{pr} ({repo})"
        )));
    }
    if capture.returncode != Some(0) {
        return Err(ReconcileFetchError::Command(format!(
            "gh pr view failed while reconciling PR #{pr} ({repo}): {}",
            capture.stderr_or_stdout()
        )));
    }
    let value = serde_json::from_str::<Value>(&capture.stdout).map_err(|error| {
        ReconcileFetchError::Parse(format!("failed to parse gh pr view JSON: {error}"))
    })?;
    let head = value
        .get("headRefOid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let rollup = value
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok((head, rollup))
}

fn is_aged_terminal(state: &ShipState, window: &ReconcileWindow, now: DateTime<Utc>) -> bool {
    if !is_aged_terminal_candidate(state, now) {
        return false;
    }
    window
        .last_forced_at(&state.repo, state.pr)
        .is_some_and(|last_forced| {
            (now - last_forced).num_seconds() <= RECONCILE_FORCED_WINDOW_SECONDS
        })
}

fn is_aged_terminal_candidate(state: &ShipState, now: DateTime<Utc>) -> bool {
    if !all_runs_or_evidence_terminal(state) {
        return false;
    }
    (now - state.updated_at).num_seconds() > RECONCILE_FRESH_WINDOW_SECONDS
}

fn all_runs_or_evidence_terminal(state: &ShipState) -> bool {
    if state.dispatched_runs.is_empty() {
        return !state.evidence_snapshot.is_empty()
            && state
                .evidence_snapshot
                .values()
                .all(|status| TERMINAL_EVIDENCE_STATUSES.contains(&status.as_str()));
    }
    state
        .dispatched_runs
        .iter()
        .all(|run| TERMINAL_RUN_STATUSES.contains(&run.status.to_ascii_lowercase().as_str()))
}

fn match_check<'a>(run: &DispatchedRun, checks: &'a [Value]) -> Option<&'a Value> {
    let target_lc = run.target.to_ascii_lowercase();
    let mut exact = Vec::new();
    let mut word_boundary = Vec::new();
    let mut substring = Vec::new();

    for check in checks {
        let name_lc = check_name(check).to_ascii_lowercase();
        if name_lc == target_lc {
            exact.push(check);
            continue;
        }
        let padded_name = padded_check_name(&name_lc);
        if padded_name.contains(&format!(" {target_lc} ")) {
            word_boundary.push(check);
            continue;
        }
        if name_lc.contains(&target_lc) {
            substring.push(check);
        }
    }

    let pool = if !exact.is_empty() {
        exact
    } else if !word_boundary.is_empty() {
        word_boundary
    } else {
        substring
    };
    pool.into_iter().max_by_key(|check| check_timestamp(check))
}

fn padded_check_name(name_lc: &str) -> String {
    let normalized = name_lc
        .chars()
        .map(|ch| {
            if matches!(ch, '/' | '(' | ')') {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>();
    format!(" {normalized} ")
}

fn conclusion_to_run_status(check: &Value) -> Option<String> {
    let state = uppercase_field(check, "state");
    let conclusion = uppercase_field(check, "conclusion");
    if matches!(state.as_str(), "QUEUED" | "PENDING") {
        return Some("pending".to_owned());
    }
    if state == "IN_PROGRESS" {
        return Some("in_progress".to_owned());
    }
    if state != "COMPLETED" && conclusion.is_empty() {
        return None;
    }
    if matches!(conclusion.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED") {
        return Some("completed".to_owned());
    }
    if conclusion == "CANCELLED" {
        return Some("cancelled".to_owned());
    }
    Some("failed".to_owned())
}

fn run_status_to_evidence(run_status: &str) -> Option<&'static str> {
    match run_status {
        "completed" => Some("pass"),
        "failed" | "cancelled" => Some("fail"),
        _ => None,
    }
}

fn uppercase_field(check: &Value, field: &str) -> String {
    check
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase()
}

fn check_name(check: &Value) -> &str {
    check
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn check_timestamp(check: &Value) -> &str {
    check
        .get("completedAt")
        .or_else(|| check.get("startedAt"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandCapture {
    returncode: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

impl CommandCapture {
    fn stderr_or_stdout(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_owned();
        }
        self.stdout.trim().to_owned()
    }
}

fn run_capture(
    mut command: Command,
    timeout: Duration,
) -> Result<CommandCapture, ReconcileFetchError> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ReconcileFetchError::Spawn(format!("failed to start gh: {error}")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ReconcileFetchError::Io("failed to capture gh stdout".to_owned()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ReconcileFetchError::Io("failed to capture gh stderr".to_owned()))?;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let timed_out = match child.wait_timeout(timeout) {
        Ok(status) => status.is_none(),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(ReconcileFetchError::Io(format!(
                "failed to wait for gh: {error}"
            )));
        }
    };
    if timed_out {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|error| ReconcileFetchError::Io(format!("failed to reap gh: {error}")))?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| ReconcileFetchError::Io("gh stdout reader panicked".to_owned()))?
        .map_err(|error| ReconcileFetchError::Io(format!("failed to read gh stdout: {error}")))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| ReconcileFetchError::Io("gh stderr reader panicked".to_owned()))?
        .map_err(|error| ReconcileFetchError::Io(format!("failed to read gh stderr: {error}")))?;
    Ok(CommandCapture {
        returncode: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::{
        CommandCapture, ReconcileFetchError, ReconcileTransition, ReconcileWindow,
        fetch_provenanced_page, parse_provenanced_page, reconcile_active_ship_states_with,
        reconcile_ship_state, run_capture,
    };
    use crate::ship_state::{DispatchedRun, ShipState, ShipStateStore};

    fn sample_time() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(2026, 4, 25, 7, 0, 0)
            .single()
            .expect("valid time")
    }

    #[test]
    fn provenanced_page_requires_cursor_metadata_and_preserves_nodes() {
        let capture = CommandCapture {
            returncode: Some(0),
            stdout: serde_json::json!({
                "data": {"repository": {"pullRequest": {
                    "headRefOid": "a".repeat(40),
                    "statusCheckRollup": {"contexts": {
                        "pageInfo": {"hasNextPage": true, "endCursor": "cursor-100"},
                        "nodes": [{"__typename": "CheckRun", "name": "macos"}]
                    }}
                }}}
            })
            .to_string(),
            stderr: String::new(),
            timed_out: false,
        };
        let page = parse_provenanced_page(&capture, "owner/repo", 42).expect("page");
        assert_eq!(page.head_sha, "a".repeat(40));
        assert!(page.has_next_page);
        assert_eq!(page.end_cursor.as_deref(), Some("cursor-100"));
        assert_eq!(page.checks.len(), 1);
    }

    #[test]
    fn provenanced_page_accepts_a_null_rollup_as_no_checks() {
        let capture = CommandCapture {
            returncode: Some(0),
            stdout: serde_json::json!({
                "data": {"repository": {"pullRequest": {
                    "headRefOid": "b".repeat(40),
                    "statusCheckRollup": null
                }}}
            })
            .to_string(),
            stderr: String::new(),
            timed_out: false,
        };
        let page = parse_provenanced_page(&capture, "owner/repo", 43).expect("page");
        assert_eq!(page.head_sha, "b".repeat(40));
        assert!(page.checks.is_empty());
        assert!(!page.has_next_page);
    }

    #[test]
    fn provenanced_page_rejects_partial_graphql_data() {
        let capture = CommandCapture {
            returncode: Some(0),
            stdout: serde_json::json!({
                "errors": [{"message": "field forbidden"}],
                "data": {"repository": {"pullRequest": {
                    "headRefOid": "b".repeat(40),
                    "statusCheckRollup": null
                }}}
            })
            .to_string(),
            stderr: String::new(),
            timed_out: false,
        };
        assert!(matches!(
            parse_provenanced_page(&capture, "owner/repo", 43),
            Err(ReconcileFetchError::Parse(_))
        ));
    }

    #[test]
    fn expired_complete_pass_deadline_attempts_no_request() {
        let client = crate::gh::GhClient::ambient();
        let failure = fetch_provenanced_page(
            &client,
            std::path::Path::new("."),
            "owner",
            "repo",
            "owner/repo",
            43,
            None,
            None,
            "query",
            7,
            Some(std::time::Instant::now()),
        )
        .expect_err("deadline");
        assert!(matches!(failure.error, ReconcileFetchError::Timeout(_)));
        assert_eq!(failure.api_requests, 7);
    }

    #[cfg(unix)]
    #[test]
    fn command_capture_drains_output_larger_than_a_pipe_buffer() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "head -c 200000 /dev/zero"]);
        let capture = run_capture(command, std::time::Duration::from_secs(5)).expect("capture");
        assert!(!capture.timed_out);
        assert_eq!(capture.returncode, Some(0));
        assert_eq!(capture.stdout.len(), 200_000);
    }

    fn run(target: &str, status: &str) -> DispatchedRun {
        let now = sample_time();
        DispatchedRun {
            target: target.to_owned(),
            provider: "namespace".to_owned(),
            run_id: "123456789".to_owned(),
            status: status.to_owned(),
            started_at: now,
            updated_at: now,
            attempt: 1,
            last_heartbeat_at: None,
            phase: None,
            required: true,
        }
    }

    fn state_with_run(pr: u64, target: &str, status: &str) -> ShipState {
        let mut state = ShipState::new(pr, "owner/repo", "feature/x", "main", "abc", "policy");
        state.created_at = sample_time();
        state.updated_at = sample_time();
        state.dispatched_runs.push(run(target, status));
        state
    }

    #[test]
    fn reconciles_run_status_and_terminal_evidence() {
        let mut state = state_with_run(42, "macos", "failed");
        state
            .evidence_snapshot
            .insert("macos".to_owned(), "fail".to_owned());
        let now = sample_time() + Duration::minutes(5);
        let rollup = vec![serde_json::json!({
            "name": "Build and Test / macos (pull_request)",
            "state": "COMPLETED",
            "conclusion": "SUCCESS",
            "completedAt": "2026-04-25T07:04:00Z"
        })];

        let reconciled = reconcile_ship_state(&state, &rollup, now);

        assert_eq!(reconciled.state.dispatched_runs[0].status, "completed");
        assert_eq!(reconciled.state.dispatched_runs[0].updated_at, now);
        assert_eq!(reconciled.state.evidence_snapshot["macos"], "pass");
        assert_eq!(
            reconciled.transitions,
            vec![ReconcileTransition {
                pr: 42,
                repo: "owner/repo".to_owned(),
                target: "macos".to_owned(),
                from_status: "failed".to_owned(),
                to_status: "completed".to_owned(),
            }]
        );
    }

    #[test]
    fn green_github_check_cannot_overwrite_failed_local_validation() {
        let mut state = state_with_run(7_792, "mac", "failed");
        state.dispatched_runs[0].provider = "local".to_owned();
        state.dispatched_runs[0].run_id = "sy-20260824-5f5628".to_owned();
        state
            .evidence_snapshot
            .insert("mac".to_owned(), "fail".to_owned());
        let rollup = vec![serde_json::json!({
            "name": "Build and Test / mac (pull_request)",
            "state": "COMPLETED",
            "conclusion": "SUCCESS",
            "completedAt": "2026-08-24T15:08:00Z"
        })];

        let reconciled =
            reconcile_ship_state(&state, &rollup, sample_time() + Duration::minutes(5));

        assert!(reconciled.changes.is_empty());
        assert!(reconciled.transitions.is_empty());
        assert_eq!(reconciled.state, state);
        assert_eq!(reconciled.state.evidence_snapshot["mac"], "fail");
    }

    #[test]
    fn check_matching_prefers_exact_pool_before_newer_fuzzy_matches() {
        let state = state_with_run(42, "mac", "in_progress");
        let now = sample_time() + Duration::minutes(5);
        let rollup = vec![
            serde_json::json!({
                "name": "Build / mac",
                "state": "COMPLETED",
                "conclusion": "SUCCESS",
                "completedAt": "2026-04-25T07:05:00Z"
            }),
            serde_json::json!({
                "name": "mac",
                "state": "COMPLETED",
                "conclusion": "FAILURE",
                "completedAt": "2026-04-25T07:00:00Z"
            }),
        ];

        let reconciled = reconcile_ship_state(&state, &rollup, now);

        assert_eq!(reconciled.state.dispatched_runs[0].status, "failed");
    }

    #[test]
    fn unknown_or_unmatched_checks_do_not_guess() {
        let state = state_with_run(42, "macos", "in_progress");
        let rollup = vec![
            serde_json::json!({"name": "linux", "state": "COMPLETED", "conclusion": "SUCCESS"}),
            serde_json::json!({"name": "macos", "state": "WAITING"}),
        ];

        let reconciled = reconcile_ship_state(&state, &rollup, sample_time());

        assert!(reconciled.changes.is_empty());
        assert_eq!(reconciled.state, state);
    }

    #[test]
    fn active_reconcile_saves_healed_state_and_transitions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let state = state_with_run(42, "macos", "in_progress");
        store.save(&state).expect("save");
        let mut window = ReconcileWindow::default();
        let now = sample_time() + Duration::minutes(5);

        let report = reconcile_active_ship_states_with(temp.path(), &mut window, now, |_| {
            Ok(vec![serde_json::json!({
                "name": "macos",
                "state": "COMPLETED",
                "conclusion": "SUCCESS"
            })])
        });

        assert_eq!(report.healed, 1);
        assert_eq!(report.fetch_errors, 0);
        assert_eq!(report.transitions[0].to_status, "completed");
        assert_eq!(
            store.get(42).expect("saved").dispatched_runs[0].status,
            "completed"
        );
    }

    #[test]
    fn recently_forced_aged_terminal_states_are_skipped() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = state_with_run(42, "macos", "completed");
        state.updated_at = sample_time() - Duration::hours(2);
        store.save(&state).expect("save");
        let mut window = ReconcileWindow::default();
        window.stamp("owner/repo", 42, sample_time() - Duration::hours(1));
        let mut fetch_calls = 0;

        let report =
            reconcile_active_ship_states_with(temp.path(), &mut window, sample_time(), |_| {
                fetch_calls += 1;
                Ok(Vec::new())
            });

        assert_eq!(report.skipped_terminal, 1);
        assert_eq!(fetch_calls, 0);
    }

    #[test]
    fn forced_window_is_not_stamped_on_fetch_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = state_with_run(42, "macos", "completed");
        state.updated_at = sample_time() - Duration::hours(2);
        store.save(&state).expect("save");
        let mut window = ReconcileWindow::default();

        let report =
            reconcile_active_ship_states_with(temp.path(), &mut window, sample_time(), |_| {
                Err(ReconcileFetchError::Command("gh failed".to_owned()))
            });

        assert_eq!(report.fetch_errors, 1);
        assert_eq!(window.last_forced_at("owner/repo", 42), None);
    }

    #[test]
    fn forced_window_is_stamped_after_successful_aged_terminal_attempt() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state = state_with_run(42, "macos", "completed");
        state.updated_at = sample_time() - Duration::hours(2);
        store.save(&state).expect("save");
        let mut window = ReconcileWindow::default();
        let now = sample_time();

        let report =
            reconcile_active_ship_states_with(temp.path(), &mut window, now, |_| Ok(Vec::new()));

        assert_eq!(report.fetch_errors, 0);
        assert_eq!(window.last_forced_at("owner/repo", 42), Some(now));
    }

    #[test]
    fn forced_window_does_not_alias_same_pr_number_across_repositories() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut pulp = state_with_run(42, "macos", "completed");
        pulp.repo = "owner/pulp".to_owned();
        pulp.updated_at = sample_time() - Duration::hours(2);
        store.save(&pulp).expect("pulp state");
        let mut forge = pulp.clone();
        forge.repo = "owner/forge".to_owned();
        store.save(&forge).expect("forge state");
        let mut window = ReconcileWindow::default();
        let now = sample_time();
        let mut fetch_calls = 0;

        let report = reconcile_active_ship_states_with(temp.path(), &mut window, now, |_| {
            fetch_calls += 1;
            Ok(Vec::new())
        });

        assert_eq!(report.skipped_terminal, 0);
        assert_eq!(fetch_calls, 2);
        assert_eq!(window.last_forced_at("OWNER/PULP", 42), Some(now));
        assert_eq!(window.last_forced_at("owner/forge", 42), Some(now));
    }
}
