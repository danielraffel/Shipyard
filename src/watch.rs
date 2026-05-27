use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::diagnostics::{
    DiagnosticsFetcher, FailureDiagnostics, FailureKind, GhDiagnosticsFetcher,
    fetch_failed_job_diagnostics, select_parser,
};
use crate::evidence::EvidenceStore;
use crate::output::write_json_envelope;
use crate::ship_state::{DispatchedRun, ShipState, ShipStateStore};

const QUEUED_STATUSES: [&str; 3] = ["queued", "pending", "waiting"];

/// Statuses on a `DispatchedRun` that indicate a terminal failure for which
/// `shipyard watch` should fetch and render Phase 1 diagnostics on the
/// transition that first observes them.
const TERMINAL_FAILURE_RUN_STATUSES: [&str; 6] = [
    "failed",
    "failure",
    "completed_failure",
    "cancelled",
    "canceled",
    "timed_out",
];

/// Determine the active PR for the current git branch, if any.
#[must_use]
pub fn active_pr_for_current_branch(store: &ShipStateStore, cwd: &Path) -> Option<u64> {
    let branch = git_branch(cwd)?;
    store
        .list_active()
        .into_iter()
        .filter(|state| state.branch == branch)
        .max_by_key(|state| state.updated_at)
        .map(|state| state.pr)
}

/// Return a stable signature for the visible watch state.
#[must_use]
pub fn watch_signature(state: &ShipState) -> String {
    let now = Utc::now();
    let stuck_threshold = stuck_queued_threshold_secs();
    let runs = state
        .dispatched_runs
        .iter()
        .map(|run| {
            format!(
                "{}:{}:{}:{}:{}:sq={}",
                run.target,
                run.status,
                run.run_id,
                run.phase.as_deref().unwrap_or(""),
                run.last_heartbeat_at
                    .as_ref()
                    .map(DateTime::<Utc>::to_rfc3339)
                    .unwrap_or_default(),
                if is_stuck_queued(run, now, stuck_threshold) {
                    "1"
                } else {
                    "0"
                }
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    let evidence = state
        .evidence_snapshot
        .iter()
        .map(|(target, status)| format!("{target}:{status}"))
        .collect::<Vec<_>>()
        .join(",");

    format!(
        "pr={}|sha={}|attempt={}|evidence={}|runs={}",
        state.pr, state.head_sha, state.attempt, evidence, runs
    )
}

/// Return reused evidence provenance for the current ship state keyed by target.
#[must_use]
pub fn reused_evidence_map(
    evidence_store: &EvidenceStore,
    state: &ShipState,
) -> BTreeMap<String, String> {
    evidence_store
        .get_branch(&state.branch)
        .into_iter()
        .filter_map(|(target, record)| {
            if record.sha == state.head_sha {
                record.reused_from.map(|reused_from| (target, reused_from))
            } else {
                None
            }
        })
        .collect()
}

/// Return the full watch-event signature including reuse provenance.
#[must_use]
pub fn watch_event_signature(state: &ShipState, reuse_map: &BTreeMap<String, String>) -> String {
    let reuse = reuse_map
        .iter()
        .map(|(target, reused_from)| format!("{target}:{}", short_sha(reused_from, 12)))
        .collect::<Vec<_>>()
        .join(",");
    format!("{}|reused={reuse}", watch_signature(state))
}

/// Return `Some(true)` for terminal success, `Some(false)` for terminal failure,
/// or `None` while the ship is still in flight.
#[must_use]
pub fn ship_terminal_verdict(state: &ShipState) -> Option<bool> {
    if state.evidence_snapshot.is_empty() {
        return None;
    }

    if state
        .evidence_snapshot
        .values()
        .any(|status| status != "pass" && status != "fail")
    {
        return None;
    }

    let advisory_targets = state
        .dispatched_runs
        .iter()
        .filter(|run| !run.required)
        .map(|run| run.target.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let required_targets = state
        .dispatched_runs
        .iter()
        .filter(|run| run.required)
        .map(|run| run.target.as_str())
        .collect::<std::collections::BTreeSet<_>>();

    let seen_targets = state
        .evidence_snapshot
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if required_targets.difference(&seen_targets).next().is_some() {
        return None;
    }

    Some(
        state
            .evidence_snapshot
            .iter()
            .all(|(target, status)| status == "pass" || advisory_targets.contains(target.as_str())),
    )
}

/// Phase 1 diagnostics resolved for a single failed target during a
/// `shipyard watch` poll. Built by [`collect_watch_diagnostics`] and rendered
/// by [`emit_watch_snapshot_with_diagnostics`].
#[derive(Clone, Debug)]
pub struct WatchTargetDiagnostics {
    /// Logical Shipyard target name (e.g. `mac`).
    pub target_name: String,
    /// Provider label captured on the dispatched run (e.g. `namespace`,
    /// `github-hosted`).
    pub provider: String,
    /// GitHub Actions workflow run ID this evidence refers to.
    pub run_id: u64,
    /// Failure-cause classification used to pick a verb in the human render
    /// and to label the JSON event.
    pub kind: FailureKind,
    /// Resolved failing-job metadata + parsed footer.
    pub diagnostics: FailureDiagnostics,
}

/// Per-watch-session cache keyed by `(target, run_id)` so a target that has
/// already been reported as terminally failed is not refetched on every poll
/// (issue #303 Phase 2). The cache is intentionally process-local — it lives
/// for the duration of a single `shipyard watch` invocation.
#[derive(Clone, Debug, Default)]
pub struct WatchDiagnosticsCache {
    seen: BTreeSet<(String, u64)>,
}

impl WatchDiagnosticsCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the cache has already resolved diagnostics for this
    /// `(target, run_id)` pair.
    #[must_use]
    pub fn contains(&self, target: &str, run_id: u64) -> bool {
        self.seen.contains(&(target.to_owned(), run_id))
    }

    /// Mark `(target, run_id)` as resolved so subsequent polls skip refetching.
    pub fn mark(&mut self, target: &str, run_id: u64) {
        self.seen.insert((target.to_owned(), run_id));
    }
}

/// Resolve Phase 1 diagnostics for any `DispatchedRun` that has just entered
/// a terminal failure state and has not yet been recorded in `cache`.
///
/// At most one log fetch is performed per terminal-failure transition; the
/// cache key `(target, run_id)` guarantees that subsequent polls observing
/// the same state are no-ops. `run_id` must parse as a `u64` (cloud workflow
/// run IDs) — local / SSH / Windows targets carry non-numeric Shipyard job
/// IDs in their dispatched-run record and are skipped here because the GHA
/// API surface does not know how to address them.
#[must_use]
pub fn collect_watch_diagnostics<F: DiagnosticsFetcher + ?Sized>(
    state: &ShipState,
    fetcher: &F,
    cache: &mut WatchDiagnosticsCache,
) -> Vec<WatchTargetDiagnostics> {
    let mut out = Vec::new();
    if state.repo.is_empty() {
        return out;
    }
    for run in &state.dispatched_runs {
        if !is_terminal_failure_status(&run.status) {
            continue;
        }
        let Ok(run_id) = run.run_id.parse::<u64>() else {
            // Non-numeric run IDs come from local / SSH / Windows backends
            // (the Shipyard internal job ID); skip — the GHA API can't
            // address them.
            continue;
        };
        if cache.contains(&run.target, run_id) {
            continue;
        }
        let parser = select_parser(None);
        let resolved = fetch_failed_job_diagnostics(
            fetcher,
            &state.repo,
            run_id,
            &run.target,
            parser.as_ref(),
        );
        let kind = match run.status.as_str() {
            "cancelled" | "canceled" => FailureKind::Cancelled,
            "timed_out" => FailureKind::TimedOut,
            _ => FailureKind::Failed,
        };
        cache.mark(&run.target, run_id);
        out.push(WatchTargetDiagnostics {
            target_name: run.target.clone(),
            provider: run.provider.clone(),
            run_id,
            kind,
            diagnostics: resolved,
        });
    }
    out
}

/// Convenience overload of [`collect_watch_diagnostics`] that uses the
/// production [`GhDiagnosticsFetcher`].
#[must_use]
pub fn collect_watch_diagnostics_gh(
    state: &ShipState,
    cache: &mut WatchDiagnosticsCache,
) -> Vec<WatchTargetDiagnostics> {
    let fetcher = GhDiagnosticsFetcher::default();
    collect_watch_diagnostics(state, &fetcher, cache)
}

fn is_terminal_failure_status(status: &str) -> bool {
    TERMINAL_FAILURE_RUN_STATUSES.contains(&status)
}

fn failure_kind_label(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Cancelled => "cancelled",
        FailureKind::TimedOut => "timed_out",
        FailureKind::Failed => "failed",
    }
}

/// Emit one watch snapshot, either as a JSON update event or human-readable text.
pub fn emit_watch_snapshot<W: Write>(
    state: &ShipState,
    reuse_map: &BTreeMap<String, String>,
    json: bool,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    emit_watch_snapshot_with_diagnostics(state, reuse_map, &[], json, stdout)
}

/// Emit one watch snapshot, optionally appending the Phase 2 diagnostics block
/// for any target that has just entered a terminal-failure state.
pub fn emit_watch_snapshot_with_diagnostics<W: Write>(
    state: &ShipState,
    reuse_map: &BTreeMap<String, String>,
    diagnostics: &[WatchTargetDiagnostics],
    json: bool,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = Utc::now();
    if json {
        return emit_watch_json_snapshot(state, reuse_map, diagnostics, now, stdout);
    }
    emit_watch_human_snapshot(state, reuse_map, diagnostics, now, stdout)
}

fn emit_watch_json_snapshot<W: Write>(
    state: &ShipState,
    reuse_map: &BTreeMap<String, String>,
    diagnostics: &[WatchTargetDiagnostics],
    now: DateTime<Utc>,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut data = BTreeMap::new();
    data.insert("event".to_owned(), Value::from("update"));
    data.insert("pr".to_owned(), Value::from(state.pr));
    data.insert("head_sha".to_owned(), Value::from(state.head_sha.clone()));
    data.insert("attempt".to_owned(), Value::from(state.attempt));
    data.insert(
        "evidence".to_owned(),
        Value::Object(json_evidence_snapshot(state, reuse_map)),
    );
    let stuck_threshold = stuck_queued_threshold_secs();
    data.insert(
        "dispatched_runs".to_owned(),
        Value::Array(
            state
                .dispatched_runs
                .iter()
                .map(|run| json_run(run, now, stuck_threshold))
                .collect::<Result<Vec<_>, _>>()?,
        ),
    );
    if !diagnostics.is_empty() {
        data.insert(
            "diagnostics".to_owned(),
            Value::Array(diagnostics.iter().map(json_diagnostics).collect()),
        );
    }
    data.insert(
        "updated_at".to_owned(),
        Value::from(state.updated_at.to_rfc3339()),
    );
    write_json_envelope(stdout, "watch", data)?;
    Ok(())
}

fn json_diagnostics(entry: &WatchTargetDiagnostics) -> Value {
    let details = &entry.diagnostics;
    let job = details.job.as_ref();
    serde_json::json!({
        "failed_target": entry.target_name,
        "provider": entry.provider,
        "run_id": entry.run_id,
        "kind": failure_kind_label(entry.kind),
        "cloud_job_id": job.map(|info| info.job_id),
        "cloud_job_name": job.map(|info| info.name.clone()),
        "cloud_job_url": job.map(|info| info.html_url.clone()),
        "failed_step": job.and_then(|info| info.failed_step.clone()),
        "details": details,
    })
}

fn json_evidence_snapshot(
    state: &ShipState,
    reuse_map: &BTreeMap<String, String>,
) -> serde_json::Map<String, Value> {
    state
        .evidence_snapshot
        .iter()
        .map(|(target, status)| {
            let value = if status == "pass" {
                reuse_map.get(target).map_or_else(
                    || Value::from(status.clone()),
                    |reused_from| {
                        serde_json::json!({
                            "status": "reused",
                            "reused_from": reused_from,
                        })
                    },
                )
            } else {
                Value::from(status.clone())
            };
            (target.clone(), value)
        })
        .collect()
}

fn emit_watch_human_snapshot<W: Write>(
    state: &ShipState,
    reuse_map: &BTreeMap<String, String>,
    diagnostics: &[WatchTargetDiagnostics],
    now: DateTime<Utc>,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let age_secs = now
        .signed_duration_since(state.updated_at)
        .num_seconds()
        .max(0);
    writeln!(
        stdout,
        "PR #{}  sha={}  attempt={}  age={}s",
        state.pr,
        abbreviate_sha(&state.head_sha),
        state.attempt,
        age_secs
    )?;

    let (complete, in_flight, total) = progress_summary(state);
    if total > 0 {
        writeln!(
            stdout,
            "  {complete}/{total} targets complete · {in_flight} in flight"
        )?;
    }

    let advisory_targets = state
        .dispatched_runs
        .iter()
        .filter(|run| !run.required)
        .map(|run| run.target.as_str())
        .collect::<std::collections::BTreeSet<_>>();

    for (target, status) in &state.evidence_snapshot {
        let advisory_tag = if advisory_targets.contains(target.as_str()) {
            " (advisory)"
        } else {
            ""
        };
        if status == "pass"
            && let Some(reused_from) = reuse_map.get(target)
        {
            writeln!(
                stdout,
                "  evidence: {target}=reused (from {}){advisory_tag}",
                short_sha(reused_from, 7)
            )?;
        } else {
            writeln!(stdout, "  evidence: {target}={status}{advisory_tag}")?;
        }
    }

    for run in &state.dispatched_runs {
        let phase = run.phase.as_deref().unwrap_or("-");
        let advisory_tag = if run.required { "" } else { " (advisory)" };
        let stuck_queued = queued_for_secs(run, now)
            .filter(|queued_for_secs| *queued_for_secs >= stuck_queued_threshold_secs())
            .map(|queued_for_secs| {
                format!(
                    " stuck-queued {}",
                    format_stuck_queued_duration(queued_for_secs)
                )
            })
            .unwrap_or_default();
        writeln!(
            stdout,
            "  run: {} ({}) id={} status={} phase={} elapsed={}s last_seen={}{}{}",
            run.target,
            run.provider,
            run.run_id,
            run.status,
            phase,
            run_elapsed_secs(run, now),
            heartbeat_age_label(run, now),
            stuck_queued,
            advisory_tag
        )?;
    }

    render_watch_diagnostics(diagnostics, stdout)?;

    Ok(())
}

/// Render the Phase 1 diagnostics block, mirroring the shape used by
/// `render_validation_failed` in `src/app/ship_cmd.rs` so users see the same
/// "Job / URL / Step / Tests" layout regardless of which command surfaced the
/// failure.
fn render_watch_diagnostics<W: Write>(
    diagnostics: &[WatchTargetDiagnostics],
    stdout: &mut W,
) -> std::io::Result<()> {
    if diagnostics.is_empty() {
        return Ok(());
    }
    writeln!(
        stdout,
        "  \u{2717} {} terminal-failure transition{} this poll:",
        diagnostics.len(),
        if diagnostics.len() == 1 { "" } else { "s" }
    )?;
    for entry in diagnostics {
        match entry.kind {
            FailureKind::Cancelled => {
                writeln!(
                    stdout,
                    "    \u{223C} Validation cancelled (concurrency-replaced or skipped); not a failure"
                )?;
                writeln!(stdout, "      Target:  {}", entry.target_name)?;
            }
            FailureKind::TimedOut => {
                writeln!(stdout, "    \u{2717} Validation timed out")?;
                writeln!(stdout, "      Target:  {}", entry.target_name)?;
            }
            FailureKind::Failed => {
                writeln!(
                    stdout,
                    "      Target:  {} (cloud={})",
                    entry.target_name, entry.provider
                )?;
                if let Some(job) = entry.diagnostics.job.as_ref() {
                    writeln!(stdout, "      Job:     {}", job.name)?;
                    if !job.html_url.is_empty() {
                        writeln!(stdout, "      URL:     {}", job.html_url)?;
                    }
                    if let Some(step) = job.failed_step.as_deref() {
                        writeln!(stdout, "      Step:    \"{step}\"")?;
                    }
                } else if let Some(run_id) = entry.diagnostics.run_id {
                    writeln!(
                        stdout,
                        "      Run ID:  {run_id} (failed-job lookup unavailable)"
                    )?;
                }
                if !entry.diagnostics.failure_summary.is_empty() {
                    writeln!(stdout, "      Tests:")?;
                    for line in &entry.diagnostics.failure_summary {
                        writeln!(stdout, "        {line}")?;
                    }
                    if entry.diagnostics.failure_summary_truncated {
                        writeln!(stdout, "        (truncated; see job log for full list)")?;
                    }
                } else if entry.diagnostics.log_tail.is_some() {
                    writeln!(stdout, "      Tests:   (no recognised footer; see job URL)")?;
                }
            }
        }
    }
    Ok(())
}

/// Emit a non-update watch event such as `pr-not-found`.
pub fn emit_watch_event<W: Write>(
    event: &str,
    pr: Option<u64>,
    message: Option<&str>,
    json: bool,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from(event));
        if let Some(pr) = pr {
            data.insert("pr".to_owned(), Value::from(pr));
        }
        if let Some(message) = message {
            data.insert("message".to_owned(), Value::from(message));
        }
        write_json_envelope(stdout, "watch", data)?;
        return Ok(());
    }

    if let Some(message) = message {
        writeln!(stdout, "{message}")?;
    }
    Ok(())
}

fn git_branch(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!branch.is_empty()).then_some(branch)
}

fn json_run(
    run: &DispatchedRun,
    now: DateTime<Utc>,
    stuck_threshold_secs: i64,
) -> Result<Value, serde_json::Error> {
    let mut value = serde_json::to_value(run)?;
    let Value::Object(ref mut map) = value else {
        unreachable!("DispatchedRun must serialize as an object");
    };
    map.insert(
        "elapsed_seconds".to_owned(),
        Value::from(run_elapsed_secs(run, now)),
    );
    map.insert(
        "queued_for_secs".to_owned(),
        queued_for_secs(run, now).map_or(Value::Null, Value::from),
    );
    map.insert(
        "stuck_queued".to_owned(),
        Value::from(is_stuck_queued(run, now, stuck_threshold_secs)),
    );
    Ok(value)
}

fn run_elapsed_secs(run: &DispatchedRun, now: DateTime<Utc>) -> i64 {
    now.signed_duration_since(run.started_at)
        .num_seconds()
        .max(0)
}

fn stuck_queued_threshold_secs() -> i64 {
    std::env::var("SHIPYARD_STUCK_QUEUED_THRESHOLD_SECS")
        .ok()
        .and_then(|raw| raw.parse::<i64>().ok())
        .map_or(300, |threshold| threshold.max(0))
}

fn queued_for_secs(run: &DispatchedRun, now: DateTime<Utc>) -> Option<i64> {
    QUEUED_STATUSES
        .contains(&run.status.to_lowercase().as_str())
        .then(|| run_elapsed_secs(run, now))
}

fn is_stuck_queued(run: &DispatchedRun, now: DateTime<Utc>, threshold_secs: i64) -> bool {
    queued_for_secs(run, now).is_some_and(|queued_for_secs| queued_for_secs >= threshold_secs)
}

fn format_stuck_queued_duration(secs: i64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3_600 {
        return format!("{}m", secs / 60);
    }
    let hours = secs / 3_600;
    let minutes = (secs % 3_600) / 60;
    format!("{hours}h{minutes}m")
}

fn heartbeat_age_label(run: &DispatchedRun, now: DateTime<Utc>) -> String {
    let terminal = matches!(
        run.status.as_str(),
        "pass"
            | "success"
            | "completed"
            | "completed_success"
            | "fail"
            | "failed"
            | "failure"
            | "completed_failure"
            | "cancelled"
            | "canceled"
    );
    if terminal {
        return "-".to_owned();
    }
    let Some(last_heartbeat_at) = run.last_heartbeat_at else {
        return "-".to_owned();
    };
    let age = now
        .signed_duration_since(last_heartbeat_at)
        .num_seconds()
        .max(0);
    if age < 60 {
        format!("{age}s_ago")
    } else {
        let minutes = age / 60;
        let seconds = age % 60;
        format!("{minutes}m{seconds:02}s_ago")
    }
}

fn progress_summary(state: &ShipState) -> (usize, usize, usize) {
    let terminal_statuses = [
        "pass",
        "success",
        "completed",
        "completed_success",
        "fail",
        "failed",
        "failure",
        "completed_failure",
        "cancelled",
        "canceled",
    ];

    let complete = state
        .dispatched_runs
        .iter()
        .filter(|run| terminal_statuses.contains(&run.status.as_str()))
        .count();
    let total = state.dispatched_runs.len();
    let in_flight = total.saturating_sub(complete);
    (complete, in_flight, total)
}

fn abbreviate_sha(sha: &str) -> &str {
    short_sha(sha, 12)
}

fn short_sha(sha: &str, width: usize) -> &str {
    let max = sha
        .char_indices()
        .nth(width)
        .map_or_else(|| sha.len(), |(index, _)| index);
    &sha[..max]
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::{
        WatchDiagnosticsCache, collect_watch_diagnostics, emit_watch_snapshot,
        emit_watch_snapshot_with_diagnostics, format_stuck_queued_duration, is_stuck_queued,
        queued_for_secs, reused_evidence_map, ship_terminal_verdict, watch_event_signature,
        watch_signature,
    };
    use crate::diagnostics::{DiagnosticsError, DiagnosticsFetcher};
    use crate::evidence::{EvidenceRecord, EvidenceStore};
    use crate::ship_state::{DispatchedRun, ShipState};

    fn state(pr: u64, evidence: &[(&str, &str)], runs: Vec<DispatchedRun>) -> ShipState {
        let mut state = ShipState::new(pr, "owner/repo", "feature/x", "main", "a".repeat(40), "p1");
        state.evidence_snapshot = evidence
            .iter()
            .map(|(target, status)| ((*target).to_owned(), (*status).to_owned()))
            .collect();
        state.dispatched_runs = runs;
        state
    }

    fn run(target: &str, status: &str, run_id: &str, required: bool) -> DispatchedRun {
        let now = Utc::now();
        DispatchedRun {
            target: target.to_owned(),
            provider: "local".to_owned(),
            run_id: run_id.to_owned(),
            status: status.to_owned(),
            started_at: now,
            updated_at: now,
            attempt: 1,
            last_heartbeat_at: None,
            phase: None,
            required,
        }
    }

    fn run_with_age(target: &str, status: &str, age_secs: i64) -> DispatchedRun {
        let mut run = run(target, status, "1", true);
        let started_at = Utc::now() - Duration::seconds(age_secs);
        run.started_at = started_at;
        run.updated_at = started_at;
        run
    }

    #[test]
    fn watch_signature_ignores_updated_at_only_changes() {
        let mut state = state(42, &[("macos", "pass")], vec![]);
        let before = watch_signature(&state);
        state.updated_at += Duration::seconds(5);
        let after = watch_signature(&state);
        assert_eq!(before, after);
    }

    #[test]
    fn watch_event_signature_changes_when_reuse_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("store");
        let state = state(
            42,
            &[("macos", "pass")],
            vec![run("macos", "completed", "1", true)],
        );
        let base = watch_event_signature(&state, &reused_evidence_map(&evidence, &state));

        evidence
            .record(&EvidenceRecord {
                sha: state.head_sha.clone(),
                branch: state.branch.clone(),
                target_name: "macos".to_owned(),
                platform: "macos-arm64".to_owned(),
                status: "pass".to_owned(),
                backend: "reused".to_owned(),
                completed_at: Utc::now(),
                duration_secs: None,
                host: None,
                primary_backend: None,
                failover_reason: None,
                provider: None,
                runner_profile: None,
                failure_class: None,
                reused_from: Some("b".repeat(40)),
                contract_digest: None,
                stages_signature: None,
            })
            .expect("record");

        let updated = watch_event_signature(&state, &reused_evidence_map(&evidence, &state));
        assert_ne!(base, updated);
        assert!(updated.contains("reused=macos:bbbbbbbbbbbb"));
    }

    #[test]
    fn queued_for_secs_only_applies_to_queued_family_statuses() {
        let now = Utc::now();
        for status in ["queued", "pending", "waiting"] {
            assert!(queued_for_secs(&run_with_age("macos", status, 120), now).is_some());
        }
        for status in ["in_progress", "completed", "failed"] {
            assert!(queued_for_secs(&run_with_age("macos", status, 120), now).is_none());
        }
    }

    #[test]
    fn stuck_queued_uses_threshold_and_human_duration_shape() {
        let now = Utc::now();
        let run = run_with_age("macos", "queued", 400);

        assert!(is_stuck_queued(&run, now, 300));
        assert!(!is_stuck_queued(&run, now, 600));
        assert_eq!(format_stuck_queued_duration(45), "45s");
        assert_eq!(format_stuck_queued_duration(125), "2m");
        assert_eq!(format_stuck_queued_duration(3_900), "1h5m");
    }

    #[test]
    fn watch_signature_flips_when_run_crosses_default_stuck_threshold() {
        let before = state(42, &[], vec![run_with_age("macos", "queued", 100)]);
        let after = state(42, &[], vec![run_with_age("macos", "queued", 400)]);

        let sig_before = watch_signature(&before);
        let sig_after = watch_signature(&after);

        assert_ne!(sig_before, sig_after);
        assert!(sig_before.contains("sq=0"));
        assert!(sig_after.contains("sq=1"));
    }

    #[test]
    fn watch_json_includes_stuck_queued_fields() {
        let state = state(42, &[], vec![run_with_age("macos", "queued", 400)]);
        let mut stdout = Vec::new();

        emit_watch_snapshot(
            &state,
            &std::collections::BTreeMap::new(),
            true,
            &mut stdout,
        )
        .expect("watch snapshot");
        let value: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        let run = &value["dispatched_runs"][0];

        assert!(run["queued_for_secs"].as_i64().expect("queued secs") >= 300);
        assert_eq!(run["stuck_queued"], serde_json::Value::Bool(true));
    }

    #[test]
    fn verdict_returns_success_when_all_required_targets_pass() {
        let state = state(
            42,
            &[("macos", "pass"), ("linux", "pass")],
            vec![
                run("macos", "completed", "1", true),
                run("linux", "completed", "2", true),
            ],
        );
        assert_eq!(ship_terminal_verdict(&state), Some(true));
    }

    #[test]
    fn verdict_returns_failure_when_required_target_fails() {
        let state = state(
            42,
            &[("macos", "fail")],
            vec![run("macos", "failed", "1", true)],
        );
        assert_eq!(ship_terminal_verdict(&state), Some(false));
    }

    #[test]
    fn verdict_ignores_advisory_failures() {
        let state = state(
            42,
            &[("mac", "pass"), ("windows", "fail")],
            vec![
                run("mac", "completed", "1", true),
                run("windows", "failed", "2", false),
            ],
        );
        assert_eq!(ship_terminal_verdict(&state), Some(true));
    }

    #[test]
    fn verdict_requires_terminal_evidence_for_all_required_targets() {
        let state = state(
            42,
            &[("mac", "pass")],
            vec![
                run("mac", "completed", "1", true),
                run("linux", "completed", "2", true),
            ],
        );
        assert_eq!(ship_terminal_verdict(&state), None);
    }

    // ----- Phase 2 (issue #303) -----

    /// Fake [`DiagnosticsFetcher`] that returns canned responses and counts
    /// the number of jobs / log fetches it received, so tests can assert that
    /// the watch cache really prevents refetching across polls.
    struct FakeFetcher {
        jobs_json: String,
        log: String,
        jobs_calls: std::cell::Cell<usize>,
        log_calls: std::cell::Cell<usize>,
    }

    impl FakeFetcher {
        fn new(jobs_json: String, log: String) -> Self {
            Self {
                jobs_json,
                log,
                jobs_calls: std::cell::Cell::new(0),
                log_calls: std::cell::Cell::new(0),
            }
        }
    }

    impl DiagnosticsFetcher for FakeFetcher {
        fn fetch_jobs_json(&self, _repo: &str, _run_id: u64) -> Result<String, DiagnosticsError> {
            self.jobs_calls.set(self.jobs_calls.get() + 1);
            Ok(self.jobs_json.clone())
        }

        fn fetch_job_log(&self, _repo: &str, _job_id: u64) -> Result<String, DiagnosticsError> {
            self.log_calls.set(self.log_calls.get() + 1);
            Ok(self.log.clone())
        }
    }

    fn failed_jobs_payload() -> String {
        serde_json::json!({
            "total_count": 1,
            "jobs": [{
                "id": 12_345,
                "name": "macOS (ARM64) [namespace]",
                "html_url": "https://github.com/owner/repo/actions/runs/9999/job/12345",
                "conclusion": "failure",
                "steps": [{"name": "Test", "conclusion": "failure"}],
                "labels": ["namespace-profile-foo"]
            }]
        })
        .to_string()
    }

    fn failed_ctest_log() -> String {
        "noise\nThe following tests FAILED:\n\t10 - synth_test (Failed)\nErrors while running CTest\n".to_owned()
    }

    fn failed_run(target: &str, provider: &str, run_id: u64) -> DispatchedRun {
        let mut run = run(target, "failed", &run_id.to_string(), true);
        run.provider = provider.to_owned();
        run
    }

    #[test]
    fn collect_watch_diagnostics_fetches_for_terminal_failure() {
        let state = state(
            42,
            &[("mac", "fail")],
            vec![failed_run("mac", "namespace", 9_999)],
        );
        let fetcher = FakeFetcher::new(failed_jobs_payload(), failed_ctest_log());
        let mut cache = WatchDiagnosticsCache::new();

        let diagnostics = collect_watch_diagnostics(&state, &fetcher, &mut cache);

        assert_eq!(diagnostics.len(), 1);
        let entry = &diagnostics[0];
        assert_eq!(entry.target_name, "mac");
        assert_eq!(entry.provider, "namespace");
        assert_eq!(entry.run_id, 9_999);
        let job = entry
            .diagnostics
            .job
            .as_ref()
            .expect("job metadata present");
        assert_eq!(job.job_id, 12_345);
        assert!(job.html_url.contains("/runs/9999/job/12345"));
        assert_eq!(entry.diagnostics.failure_summary.len(), 1);
        assert!(entry.diagnostics.failure_summary[0].contains("synth_test"));
    }

    #[test]
    fn collect_watch_diagnostics_caches_repeat_polls() {
        let state = state(
            42,
            &[("mac", "fail")],
            vec![failed_run("mac", "namespace", 9_999)],
        );
        let fetcher = FakeFetcher::new(failed_jobs_payload(), failed_ctest_log());
        let mut cache = WatchDiagnosticsCache::new();

        let first = collect_watch_diagnostics(&state, &fetcher, &mut cache);
        let second = collect_watch_diagnostics(&state, &fetcher, &mut cache);

        assert_eq!(first.len(), 1, "first poll fetches");
        assert!(
            second.is_empty(),
            "second poll on identical terminal-fail state must not refetch"
        );
        assert_eq!(
            fetcher.jobs_calls.get(),
            1,
            "expected exactly one jobs fetch across two polls"
        );
        assert_eq!(
            fetcher.log_calls.get(),
            1,
            "expected exactly one log fetch across two polls"
        );
    }

    #[test]
    fn collect_watch_diagnostics_skips_local_run_ids() {
        // Local/SSH/Windows backends store the Shipyard internal job ID, which
        // is not numeric. We must not call GHA for those.
        let state = state(
            42,
            &[("mac", "fail")],
            vec![{
                let mut r = run("mac", "failed", "shipyard-internal-id-xyz", true);
                r.provider = "local".to_owned();
                r
            }],
        );
        let fetcher = FakeFetcher::new(failed_jobs_payload(), failed_ctest_log());
        let mut cache = WatchDiagnosticsCache::new();

        let diagnostics = collect_watch_diagnostics(&state, &fetcher, &mut cache);

        assert!(diagnostics.is_empty());
        assert_eq!(fetcher.jobs_calls.get(), 0);
        assert_eq!(fetcher.log_calls.get(), 0);
    }

    #[test]
    fn collect_watch_diagnostics_skips_non_terminal_runs() {
        let state = state(
            42,
            &[],
            vec![{
                let mut r = run("mac", "in_progress", "9999", true);
                r.provider = "namespace".to_owned();
                r
            }],
        );
        let fetcher = FakeFetcher::new(failed_jobs_payload(), failed_ctest_log());
        let mut cache = WatchDiagnosticsCache::new();

        let diagnostics = collect_watch_diagnostics(&state, &fetcher, &mut cache);

        assert!(diagnostics.is_empty());
        assert_eq!(fetcher.jobs_calls.get(), 0);
    }

    #[test]
    fn watch_human_render_contains_failing_job_url() {
        let state = state(
            42,
            &[("mac", "fail")],
            vec![failed_run("mac", "namespace", 9_999)],
        );
        let fetcher = FakeFetcher::new(failed_jobs_payload(), failed_ctest_log());
        let mut cache = WatchDiagnosticsCache::new();
        let diagnostics = collect_watch_diagnostics(&state, &fetcher, &mut cache);
        let mut stdout = Vec::new();

        emit_watch_snapshot_with_diagnostics(
            &state,
            &std::collections::BTreeMap::new(),
            &diagnostics,
            false,
            &mut stdout,
        )
        .expect("watch snapshot");
        let text = String::from_utf8(stdout).expect("utf8");

        assert!(
            text.contains("https://github.com/owner/repo/actions/runs/9999/job/12345"),
            "human render must include failing job URL:\n{text}"
        );
        assert!(
            text.contains("macOS (ARM64) [namespace]"),
            "human render must name the failing job:\n{text}"
        );
        assert!(
            text.contains("synth_test"),
            "human render must include parsed CTest footer:\n{text}"
        );
    }

    #[test]
    fn watch_json_render_includes_diagnostics_block() {
        let state = state(
            42,
            &[("mac", "fail")],
            vec![failed_run("mac", "namespace", 9_999)],
        );
        let fetcher = FakeFetcher::new(failed_jobs_payload(), failed_ctest_log());
        let mut cache = WatchDiagnosticsCache::new();
        let diagnostics = collect_watch_diagnostics(&state, &fetcher, &mut cache);
        let mut stdout = Vec::new();

        emit_watch_snapshot_with_diagnostics(
            &state,
            &std::collections::BTreeMap::new(),
            &diagnostics,
            true,
            &mut stdout,
        )
        .expect("json snapshot");
        let value: serde_json::Value = serde_json::from_slice(&stdout).expect("json");

        let diag_array = value["diagnostics"]
            .as_array()
            .expect("diagnostics array on transition payload");
        assert_eq!(diag_array.len(), 1);
        let diag = &diag_array[0];
        assert_eq!(diag["failed_target"], "mac");
        assert_eq!(diag["run_id"], 9_999);
        assert_eq!(diag["provider"], "namespace");
        assert_eq!(diag["kind"], "failed");
        assert_eq!(diag["cloud_job_id"], 12_345);
        assert!(
            diag["cloud_job_url"]
                .as_str()
                .expect("job url string")
                .contains("/runs/9999/job/12345")
        );
        assert_eq!(diag["failed_step"], "Test");
        assert_eq!(
            diag["details"]["failed_target"], "mac",
            "details should round-trip through Phase 1's FailureDiagnostics schema"
        );
    }

    #[test]
    fn watch_json_render_omits_diagnostics_when_empty() {
        let state = state(42, &[], vec![]);
        let mut stdout = Vec::new();

        emit_watch_snapshot(
            &state,
            &std::collections::BTreeMap::new(),
            true,
            &mut stdout,
        )
        .expect("json snapshot");
        let value: serde_json::Value = serde_json::from_slice(&stdout).expect("json");

        assert!(
            value.get("diagnostics").is_none(),
            "in-flight watch payloads must not include an empty diagnostics array"
        );
    }

    #[test]
    fn collect_watch_diagnostics_classifies_cancelled_and_timed_out() {
        let mut state = state(
            42,
            &[("mac", "fail")],
            vec![{
                let mut r = run("mac", "cancelled", "9999", true);
                r.provider = "namespace".to_owned();
                r
            }],
        );
        let fetcher = FakeFetcher::new(failed_jobs_payload(), failed_ctest_log());
        let mut cache = WatchDiagnosticsCache::new();
        let diagnostics = collect_watch_diagnostics(&state, &fetcher, &mut cache);
        assert_eq!(diagnostics.len(), 1);
        assert!(matches!(
            diagnostics[0].kind,
            crate::diagnostics::FailureKind::Cancelled
        ));

        // Now flip to timed_out and force a new run_id so the cache lets it through.
        state.dispatched_runs[0].status = "timed_out".to_owned();
        state.dispatched_runs[0].run_id = "10000".to_owned();
        let diagnostics = collect_watch_diagnostics(&state, &fetcher, &mut cache);
        assert_eq!(diagnostics.len(), 1);
        assert!(matches!(
            diagnostics[0].kind,
            crate::diagnostics::FailureKind::TimedOut
        ));
    }
}
