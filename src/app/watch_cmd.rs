use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use super::{CliFailure, WATCH_EXIT_INDETERMINATE};
use crate::diagnostics::{DiagnosticsFetcher, GhDiagnosticsFetcher};
use crate::evidence::EvidenceStore;
use crate::ship_state::ShipStateStore;
use crate::watch::{
    WatchDiagnosticsCache, active_pr_for_current_branch, collect_watch_diagnostics,
    emit_watch_event, emit_watch_snapshot_with_diagnostics, reused_evidence_map,
    ship_terminal_verdict, watch_event_signature,
};

#[derive(Clone, Copy)]
pub(super) struct WatchCommandContext<'a> {
    pub(super) store: &'a ShipStateStore,
    pub(super) evidence_store: &'a EvidenceStore,
    pub(super) cwd: &'a Path,
}

#[derive(Clone, Copy)]
pub(super) struct WatchCommandOptions {
    pub(super) pr: Option<u64>,
    pub(super) follow: bool,
    pub(super) interval: f64,
    pub(super) json: bool,
}

pub(super) fn watch<W: Write>(
    context: WatchCommandContext<'_>,
    options: WatchCommandOptions,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let fetcher = GhDiagnosticsFetcher::new(context.cwd);
    watch_with_fetcher(context, options, &fetcher, stdout)
}

/// Run the watch loop with an injectable diagnostics fetcher.
///
/// Production callers use [`watch`], which passes [`GhDiagnosticsFetcher`].
/// Tests substitute a fake fetcher so the terminal-failure diagnostics path
/// can be exercised without a real GitHub API surface.
pub(super) fn watch_with_fetcher<W: Write, F: DiagnosticsFetcher + ?Sized>(
    context: WatchCommandContext<'_>,
    options: WatchCommandOptions,
    fetcher: &F,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let target_pr = options
        .pr
        .or_else(|| active_pr_for_current_branch(context.store, context.cwd));
    let Some(target_pr) = target_pr else {
        let message = "No active ship state for current branch.";
        emit_watch_event("no-active-ship", None, Some(message), options.json, stdout)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ExitCode::from(2));
    };

    let mut last_signature = None;
    let mut observed_any_state = false;
    let mut diagnostics_cache = WatchDiagnosticsCache::new();

    loop {
        let state = crate::watch::repository_for_cwd(context.cwd).map_or_else(
            || context.store.get(target_pr),
            |repository| context.store.get_scoped(&repository, target_pr),
        );
        let Some(state) = state else {
            if !observed_any_state {
                let message = format!(
                    "PR #{target_pr}: no ship state found (typo, wrong repo, or never shipped)."
                );
                emit_watch_event(
                    "pr-not-found",
                    Some(target_pr),
                    Some(&message),
                    options.json,
                    stdout,
                )
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
                return Ok(ExitCode::from(2));
            }
            let (message, exit_code) = archived_handback(target_pr);
            emit_watch_event(
                "state-archived",
                Some(target_pr),
                Some(&message),
                options.json,
                stdout,
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            return Ok(exit_code);
        };

        observed_any_state = true;
        let reuse_map = reused_evidence_map(context.evidence_store, &state);
        let signature = watch_event_signature(&state, &reuse_map);
        if last_signature.as_deref() != Some(signature.as_str()) {
            // Phase 2 (issue #303): on each new state signature, resolve
            // diagnostics for any target that has just entered a terminal
            // failure state. The cache key `(target, run_id)` guarantees we
            // fetch at most once per terminal-failure transition for the
            // lifetime of this `shipyard watch` invocation.
            let diagnostics = collect_watch_diagnostics(&state, fetcher, &mut diagnostics_cache);
            emit_watch_snapshot_with_diagnostics(
                &state,
                &reuse_map,
                &diagnostics,
                options.json,
                stdout,
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            last_signature = Some(signature);
        }

        if let Some(verdict) = ship_terminal_verdict(&state) {
            return Ok(if verdict {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            });
        }

        if !options.follow {
            return Ok(ExitCode::from(3));
        }

        thread::sleep(Duration::from_secs_f64(options.interval.max(1.0)));
    }
}

/// Message for a ship state that is gone by the time the watcher looks again.
///
/// Kept as its own function so the wording and the exit code can be asserted
/// without driving the whole watch loop — the absence of any test over this
/// path is why it returned success for years.
///
/// The archive records that a state ended, never *how*: "archived" covers
/// merged, discarded and pruned alike. Reporting success would tell a caller
/// that a pull request closed **without** merging had landed, so the handback
/// says the outcome is undetermined and names the authority that knows.
pub(super) fn archived_handback(target_pr: u64) -> (String, ExitCode) {
    let message = format!(
        "PR #{target_pr}: ship state archived, so the outcome is UNDETERMINED — archived covers \
         merged, discarded and pruned alike, and the archive does not record which. Ask the \
         authority: `gh pr view {target_pr} --json state,mergedAt`."
    );
    (message, ExitCode::from(WATCH_EXIT_INDETERMINATE))
}

#[cfg(test)]
mod tests {
    use super::archived_handback;
    use std::process::ExitCode;

    use chrono::Utc;
    use serde_json::Value;

    use super::{
        WATCH_EXIT_INDETERMINATE, WatchCommandContext, WatchCommandOptions, watch,
        watch_with_fetcher,
    };
    use crate::diagnostics::{DiagnosticsError, DiagnosticsFetcher};
    use crate::evidence::EvidenceStore;
    use crate::ship_state::{DispatchedRun, ShipState, ShipStateStore};

    fn stores(temp: &tempfile::TempDir) -> (ShipStateStore, EvidenceStore) {
        let ship = ShipStateStore::new(temp.path().join("ship-state")).expect("ship store");
        let evidence = EvidenceStore::new(temp.path().join("evidence")).expect("evidence store");
        (ship, evidence)
    }

    fn context<'a>(
        store: &'a ShipStateStore,
        evidence_store: &'a EvidenceStore,
        cwd: &'a std::path::Path,
    ) -> WatchCommandContext<'a> {
        WatchCommandContext {
            store,
            evidence_store,
            cwd,
        }
    }

    fn options(pr: Option<u64>, follow: bool, json: bool) -> WatchCommandOptions {
        WatchCommandOptions {
            pr,
            follow,
            interval: 0.01,
            json,
        }
    }

    fn sample_state(pr: u64) -> ShipState {
        let mut state = ShipState::new(
            pr,
            "danielraffel/pulp",
            "feature/test",
            "main",
            "abcdef0123456789abcdef0123456789abcdef01",
            "policy",
        );
        state.pr_title = "Test PR".to_owned();
        state.dispatched_runs.push(sample_run("linux", true));
        state
    }

    fn sample_run(target: &str, required: bool) -> DispatchedRun {
        let now = Utc::now();
        DispatchedRun {
            target: target.to_owned(),
            provider: "local".to_owned(),
            run_id: format!("run-{target}"),
            status: "in_progress".to_owned(),
            started_at: now,
            updated_at: now,
            attempt: 1,
            last_heartbeat_at: None,
            phase: Some("test".to_owned()),
            required,
        }
    }

    /// An archived ship state means merged, discarded, or pruned. Reporting
    /// success there tells a caller that a pull request closed WITHOUT merging
    /// had landed — the false-success terminal an adversarial review called out
    /// as strictly worse than never handing back at all.
    #[test]
    fn negative_control_an_archived_state_is_not_a_success() {
        let (_, code) = archived_handback(7996);

        assert_ne!(
            code,
            ExitCode::SUCCESS,
            "absence must never be reported as success"
        );
        assert_eq!(code, ExitCode::from(4));
    }

    /// The handback has to say the outcome is unknown AND where to get it. An
    /// alert that only says something ended makes its reader start from
    /// nothing.
    #[test]
    fn the_archived_handback_names_the_ambiguity_and_the_authority() {
        let (message, _) = archived_handback(7996);

        assert!(message.contains("UNDETERMINED"), "{message}");
        assert!(
            message.contains("merged, discarded and pruned"),
            "it must name what it cannot distinguish: {message}"
        );
        assert!(
            message.contains("gh pr view 7996"),
            "it must name the authority, scoped to this PR: {message}"
        );
    }

    /// Planted control for the test above: the message is per-PR, so a caller
    /// cannot be sent to look up the wrong subject. A wrong terminal is worse
    /// than none.
    #[test]
    fn control_the_handback_is_scoped_to_its_own_pr() {
        let (a, _) = archived_handback(7996);
        let (b, _) = archived_handback(1234);

        assert_ne!(a, b);
        assert!(!a.contains("1234"), "{a}");
        assert!(!b.contains("7996"), "{b}");
    }

    #[test]
    fn watch_reports_no_active_state_for_current_branch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (store, evidence) = stores(&temp);
        let mut out = Vec::new();

        let code = watch(
            context(&store, &evidence, temp.path()),
            options(None, false, true),
            &mut out,
        )
        .expect("watch");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(code, ExitCode::from(2));
        assert_eq!(payload["command"], "watch");
        assert_eq!(payload["event"], "no-active-ship");
        assert_eq!(
            payload["message"],
            "No active ship state for current branch."
        );
    }

    #[test]
    fn watch_reports_explicit_pr_not_found() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (store, evidence) = stores(&temp);
        let mut out = Vec::new();

        let code = watch(
            context(&store, &evidence, temp.path()),
            options(Some(404), false, false),
            &mut out,
        )
        .expect("watch");

        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(code, ExitCode::from(2));
        assert!(text.contains("PR #404: no ship state found"));
    }

    #[test]
    fn watch_no_follow_emits_snapshot_and_exits_in_flight_code() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (store, evidence) = stores(&temp);
        store.save(&sample_state(42)).expect("state");
        let mut out = Vec::new();

        let code = watch(
            context(&store, &evidence, temp.path()),
            options(Some(42), false, true),
            &mut out,
        )
        .expect("watch");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(code, ExitCode::from(3));
        assert_eq!(payload["command"], "watch");
        assert_eq!(payload["event"], "update");
        assert_eq!(payload["pr"], 42);
        assert_eq!(payload["dispatched_runs"][0]["target"], "linux");
    }

    #[test]
    fn watch_terminal_pass_returns_success_after_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (store, evidence) = stores(&temp);
        let mut state = sample_state(42);
        state
            .evidence_snapshot
            .insert("linux".to_owned(), "pass".to_owned());
        store.save(&state).expect("state");
        let mut out = Vec::new();

        let code = watch(
            context(&store, &evidence, temp.path()),
            options(Some(42), false, true),
            &mut out,
        )
        .expect("watch");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(payload["evidence"]["linux"], "pass");
    }

    #[test]
    fn watch_terminal_fail_returns_failure_after_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (store, evidence) = stores(&temp);
        let mut state = sample_state(42);
        state
            .evidence_snapshot
            .insert("linux".to_owned(), "fail".to_owned());
        store.save(&state).expect("state");
        let mut out = Vec::new();

        let code = watch(
            context(&store, &evidence, temp.path()),
            options(Some(42), false, false),
            &mut out,
        )
        .expect("watch");

        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(code, ExitCode::from(1));
        assert!(text.contains("PR #42"));
        assert!(text.contains("linux"));
        assert!(text.contains("fail"));
    }

    // ----- Phase 2 (issue #303) ------------------------------------------

    struct CountingFakeFetcher {
        jobs_json: String,
        log: String,
        jobs_calls: std::cell::Cell<usize>,
        log_calls: std::cell::Cell<usize>,
    }

    impl CountingFakeFetcher {
        fn new(jobs_json: String, log: String) -> Self {
            Self {
                jobs_json,
                log,
                jobs_calls: std::cell::Cell::new(0),
                log_calls: std::cell::Cell::new(0),
            }
        }
    }

    impl DiagnosticsFetcher for CountingFakeFetcher {
        fn fetch_jobs_json(&self, _repo: &str, _run_id: u64) -> Result<String, DiagnosticsError> {
            self.jobs_calls.set(self.jobs_calls.get() + 1);
            Ok(self.jobs_json.clone())
        }

        fn fetch_job_log(&self, _repo: &str, _job_id: u64) -> Result<String, DiagnosticsError> {
            self.log_calls.set(self.log_calls.get() + 1);
            Ok(self.log.clone())
        }
    }

    fn jobs_payload() -> String {
        serde_json::json!({
            "total_count": 1,
            "jobs": [{
                "id": 76_630_095_261u64,
                "name": "macOS (ARM64) [namespace]",
                "html_url": "https://github.com/danielraffel/pulp/actions/runs/26063806409/job/76630095261",
                "conclusion": "failure",
                "steps": [{"name": "Test (non-Windows)", "conclusion": "failure"}],
                "labels": ["namespace-profile-generouscorp-macos"]
            }]
        })
        .to_string()
    }

    fn ctest_log() -> String {
        "noise\nThe following tests FAILED:\n\t1236 - FontResolver: animation respects LRU cache cap (Failed)\nErrors while running CTest\n".to_owned()
    }

    fn cloud_failed_state(pr: u64, run_id: u64) -> ShipState {
        let mut state = ShipState::new(
            pr,
            "danielraffel/pulp",
            "feature/test",
            "main",
            "abcdef0123456789abcdef0123456789abcdef01",
            "policy",
        );
        let now = Utc::now();
        state.dispatched_runs.push(DispatchedRun {
            target: "mac".to_owned(),
            provider: "namespace".to_owned(),
            run_id: run_id.to_string(),
            status: "failed".to_owned(),
            started_at: now,
            updated_at: now,
            attempt: 1,
            last_heartbeat_at: None,
            phase: Some("test".to_owned()),
            required: true,
        });
        state
            .evidence_snapshot
            .insert("mac".to_owned(), "fail".to_owned());
        state
    }

    #[test]
    fn watch_human_render_includes_phase2_diagnostics_block() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (store, evidence) = stores(&temp);
        store
            .save(&cloud_failed_state(42, 26_063_806_409))
            .expect("state");
        let fetcher = CountingFakeFetcher::new(jobs_payload(), ctest_log());
        let mut out = Vec::new();

        let code = watch_with_fetcher(
            context(&store, &evidence, temp.path()),
            options(Some(42), false, false),
            &fetcher,
            &mut out,
        )
        .expect("watch");

        assert_eq!(code, ExitCode::from(1));
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(
                "https://github.com/danielraffel/pulp/actions/runs/26063806409/job/76630095261"
            ),
            "human render must include failing job URL:\n{text}"
        );
        assert!(
            text.contains("Test (non-Windows)"),
            "human render must surface the failing step:\n{text}"
        );
        assert!(
            text.contains("FontResolver"),
            "human render must show parsed CTest footer:\n{text}"
        );
    }

    #[test]
    fn watch_json_render_includes_phase2_diagnostics_block() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (store, evidence) = stores(&temp);
        store
            .save(&cloud_failed_state(42, 26_063_806_409))
            .expect("state");
        let fetcher = CountingFakeFetcher::new(jobs_payload(), ctest_log());
        let mut out = Vec::new();

        let code = watch_with_fetcher(
            context(&store, &evidence, temp.path()),
            options(Some(42), false, true),
            &fetcher,
            &mut out,
        )
        .expect("watch");

        assert_eq!(code, ExitCode::from(1));
        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        let diag = &payload["diagnostics"][0];
        assert_eq!(diag["failed_target"], "mac");
        assert_eq!(diag["run_id"], 26_063_806_409u64);
        assert_eq!(diag["provider"], "namespace");
        assert_eq!(diag["kind"], "failed");
        assert_eq!(diag["cloud_job_id"], 76_630_095_261u64);
        assert_eq!(diag["failed_step"], "Test (non-Windows)");
    }
}
