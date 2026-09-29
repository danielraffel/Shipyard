//! `shipyard pr-watch`: CLI adapter over [`crate::pr_watch`].

use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration as StdDuration, SystemTime};

use chrono::{DateTime, Utc};

use crate::app::cli::{
    PrWatchCommand, PrWatchControl, PrWatchDigestArgs, PrWatchReplayArgs, PrWatchScanArgs,
};
use crate::app::{CliFailure, WAIT_EXIT_INVALID};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::gate_cost::ReadCache;
use crate::output::write_pretty_json;
use crate::paths::RuntimePaths;
use crate::pr_watch::digest::{self, DigestOutcome};
use crate::pr_watch::fixtures::{FixtureReader, Recorder};
use crate::pr_watch::replay::{Expectation, ReplayOptions, ReplayReport, replay};
use crate::pr_watch::scan::{
    AttributorCommand, ScanReport, ScanRequest, WatchConfig, run_digest_command, scan,
};
use crate::pr_watch::{WatchQuery, gather, ledger};

/// Per-request bound. Observation must never strand the invoking agent.
const GITHUB_READ_TIMEOUT: StdDuration = StdDuration::from_secs(120);

pub(super) fn pr_watch_command<W: Write>(
    command: PrWatchCommand,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let watch = WatchConfig::from_config(config)
        .map_err(|error| CliFailure::new(WAIT_EXIT_INVALID, format!("[pr_watch]: {error}")))?;
    match command {
        PrWatchCommand::Scan(args) => {
            scan_command(args, watch, config, cwd, runtime_paths, json, stdout)
        }
        PrWatchCommand::Replay(args) => {
            replay_command(args, watch, config, cwd, runtime_paths, json, stdout)
        }
        PrWatchCommand::Digest(args) => {
            digest_command(args, &watch, cwd, runtime_paths, json, stdout)
        }
    }
}

fn io_failure(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(1, error.to_string())
}

fn actions_for(cwd: &Path, config: &LoadedConfig, repo: &str, explicit: bool) -> GitHubActions {
    let actions = GitHubActions::from_loaded_config(cwd, config);
    if explicit {
        actions.with_repo_override(repo)
    } else {
        actions
    }
}

fn cache_for(runtime_paths: &RuntimePaths, repo: &str, no_cache: bool) -> ReadCache {
    if no_cache {
        return ReadCache::disabled();
    }
    ReadCache::open(
        &runtime_paths
            .state_dir
            .join("pr-watch")
            .join("cache")
            .join(repo.replace('/', "__").to_ascii_lowercase()),
        SystemTime::now(),
    )
}

fn scan_command<W: Write>(
    args: PrWatchScanArgs,
    mut watch: WatchConfig,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let explicit = args.repo.is_some();
    let repo = super::runner_cmd::resolve_repo_slug(args.repo, cwd)?;
    if let Some(base) = args.base {
        watch.base = base;
    }
    if args.digest && watch.digest_command.is_empty() {
        return Err(CliFailure::new(
            WAIT_EXIT_INVALID,
            "pr-watch scan --digest needs [pr_watch.digest] command = [\"...\"]",
        ));
    }
    let state_path = args
        .state_file
        .unwrap_or_else(|| ledger::default_path(&runtime_paths.state_dir, &repo, &watch.base));
    let actions = actions_for(cwd, config, &repo, explicit);
    let reader = |argv: &[String]| {
        actions
            .run_gh_with_timeout(argv, GITHUB_READ_TIMEOUT)
            .map_err(|error| error.to_string())
    };
    // The only mutating requests this command can send are the sticky
    // comment's POST/PATCH, and only with --post-comments.
    let writer = |argv: &[String]| {
        actions
            .run_gh_with_timeout(argv, GITHUB_READ_TIMEOUT)
            .map_err(|error| error.to_string())
    };
    let digest_argv = watch.digest_command.clone();
    let mut sender = |payload: &str| run_digest_command(&digest_argv, payload);
    let cache = cache_for(runtime_paths, &repo, args.no_cache);
    let request = ScanRequest {
        repo,
        config: watch,
        state_path,
        post_comments: args.post_comments,
        post_digest: args.digest,
        plan_comments: true,
    };
    // The repository's batch attributor (`[queue.attribution] command`), when
    // this checkout has one, can clear a flag-3 ejection for a neighbour.
    let attributor_command = AttributorCommand::discover(config, cwd);
    let ask = |pr: u64, run_id: u64| {
        attributor_command
            .as_ref()
            .and_then(|command| command.ask(&request.repo, pr, run_id))
    };
    let attributor: Option<&crate::pr_watch::scan::Attributor<'_>> =
        attributor_command.is_some().then_some(&ask);
    let report = scan(
        &reader,
        &writer,
        &mut sender,
        attributor,
        &cache,
        &request,
        Utc::now(),
    )
    .map_err(|error| CliFailure::new(1, format!("pr-watch scan failed: {error}")))?;
    if json {
        write_pretty_json(stdout, &report).map_err(io_failure)?;
    } else {
        write!(stdout, "{}", render_scan(&report)).map_err(io_failure)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn render_scan(report: &ScanReport) -> String {
    let mut out = format!(
        "{} at {}: {} flag(s); required checks: {}\n",
        report.repo,
        report.at.format("%Y-%m-%dT%H:%M:%SZ"),
        report.flags.len(),
        report.required_checks.join(", ")
    );
    for flag in &report.flags {
        let _ = writeln!(
            out,
            "  #{} flag {} {}: {} — {}",
            flag.pr,
            flag.kind.number(),
            flag.kind.as_str(),
            flag.verdict,
            flag.evidence
        );
    }
    let verb = if report.comments_posted {
        "sent"
    } else {
        "would send (dry run; --post-comments to send)"
    };
    for action in &report.comment_actions {
        let (pr, what) = match action {
            crate::pr_watch::comment::CommentAction::Create { pr, .. } => (pr, "create"),
            crate::pr_watch::comment::CommentAction::Update { pr, .. } => (pr, "update"),
        };
        let _ = writeln!(out, "comment #{pr}: {what} ({verb})");
    }
    for error in &report.comment_errors {
        let _ = writeln!(out, "comment error: {error}");
    }
    let _ = writeln!(out, "digest: {}", render_digest_outcome(&report.digest));
    for gap in &report.gaps {
        let _ = writeln!(out, "gap: {gap}");
    }
    let _ = writeln!(
        out,
        "reads: {} sent to GitHub, {} served from cache",
        report.reads.github, report.reads.cached
    );
    out
}

fn render_digest_outcome(outcome: &DigestOutcome) -> String {
    match outcome {
        DigestOutcome::Skipped => {
            "skipped (nothing aged in, or sent within the interval)".to_owned()
        }
        DigestOutcome::WouldSend { lines } => format!("would send {lines} line(s) (dry run)"),
        DigestOutcome::Sent { lines } => format!("sent {lines} line(s)"),
    }
}

fn parse_until(text: Option<&str>) -> Result<DateTime<Utc>, CliFailure> {
    match text {
        None => Ok(Utc::now()),
        Some(text) => DateTime::parse_from_rfc3339(text)
            .map(|time| time.with_timezone(&Utc))
            .map_err(|error| {
                CliFailure::new(
                    WAIT_EXIT_INVALID,
                    format!("invalid --until {text:?}: {error}"),
                )
            }),
    }
}

#[allow(clippy::too_many_lines)]
fn replay_command<W: Write>(
    args: PrWatchReplayArgs,
    mut watch: WatchConfig,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let explicit = args.repo.is_some();
    let repo = super::runner_cmd::resolve_repo_slug(args.repo, cwd)?;
    if let Some(base) = args.base {
        watch.base = base;
    }
    let invalid = |error: String| CliFailure::new(WAIT_EXIT_INVALID, error);
    let to = parse_until(args.until.as_deref())?;
    let from = to - crate::pr_watch::parse_duration(&args.since).map_err(invalid)?;
    let tick = crate::pr_watch::parse_duration(&args.tick).map_err(invalid)?;
    let expectations = args
        .expect
        .iter()
        .map(|text| text.parse::<Expectation>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    let query = WatchQuery {
        repo: repo.clone(),
        base: watch.base.clone(),
        workflow: watch.workflow.clone(),
        required_checks: watch.required_checks.clone(),
        from,
        to,
    };
    let history = if let Some(dir) = &args.fixtures {
        let fixtures = FixtureReader::load(dir).map_err(io_failure)?;
        let reader = |argv: &[String]| fixtures.read(argv);
        gather(&reader, &ReadCache::disabled(), &query, &watch.thresholds)
    } else {
        let actions = actions_for(cwd, config, &repo, explicit);
        let recorder = args
            .record
            .as_deref()
            .map(Recorder::new)
            .transpose()
            .map_err(io_failure)?;
        let reader = |argv: &[String]| {
            let answer = actions
                .run_gh_with_timeout(argv, GITHUB_READ_TIMEOUT)
                .map_err(|error| error.to_string())?;
            if let Some(recorder) = &recorder {
                recorder.record(argv, &answer);
            }
            Ok(answer)
        };
        // A recording must hold every answer, so it never reads the cache.
        let cache = cache_for(runtime_paths, &repo, args.no_cache || args.record.is_some());
        gather(&reader, &cache, &query, &watch.thresholds)
    }
    .map_err(|error| CliFailure::new(1, format!("pr-watch replay read failed: {error}")))?;
    let options = ReplayOptions {
        tick,
        thresholds: watch.thresholds.clone(),
        digest: watch.digest_policy,
        expectations,
        control_merged_clean: args.control == Some(PrWatchControl::MergedClean),
    };
    let report = replay(&history, &options);
    if let Some(path) = &args.state_file {
        write_throwaway_state(path, &history, &options, runtime_paths)?;
    }
    if json {
        write_pretty_json(stdout, &report).map_err(io_failure)?;
    } else {
        write!(stdout, "{}", render_replay(&report)).map_err(io_failure)?;
    }
    Ok(if report.pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Replay's simulated ledger goes only to an explicit path, never to the live
/// ledger.
fn write_throwaway_state(
    path: &Path,
    history: &crate::pr_watch::RepoHistory,
    options: &ReplayOptions,
    runtime_paths: &RuntimePaths,
) -> Result<(), CliFailure> {
    let live = ledger::default_path(&runtime_paths.state_dir, &history.repo, &history.base);
    if same_path(path, &live) {
        return Err(CliFailure::new(
            WAIT_EXIT_INVALID,
            "replay --state-file must not be the live pr-watch ledger",
        ));
    }
    let mut simulated = ledger::Ledger::new(&history.repo, &history.base);
    let flags = crate::pr_watch::evaluate(history, history.to, &options.thresholds);
    let prs = crate::pr_watch::scan::pr_now(history, history.to);
    ledger::reconcile(&mut simulated, &flags, &prs, history.to);
    ledger::save(path, &simulated).map_err(io_failure)
}

fn same_path(a: &Path, b: &Path) -> bool {
    let canonical =
        |path: &Path| -> PathBuf { path.canonicalize().unwrap_or_else(|_| path.to_path_buf()) };
    canonical(a) == canonical(b)
}

fn render_digest_stats(out: &mut String, stats: &crate::pr_watch::replay::DigestStats) {
    let _ = writeln!(
        out,
        "digests: {} sent; {} PRs got a line ({} per-PR lines), {} shared-failure lines; max {} lines in one digest",
        stats.digests, stats.prs_digested, stats.pr_lines, stats.shared_lines, stats.max_lines
    );
    let prs: Vec<String> = stats
        .per_pr
        .iter()
        .map(|(pr, lines)| format!("#{pr}x{lines}"))
        .collect();
    let _ = writeln!(out, "  PRs digested (lines): {}", prs.join(" "));
    for (test, times) in &stats.shared_tests {
        let _ = writeln!(out, "  shared failure: {test} announced {times}x");
    }
}

/// One line per (pull request, flag) across all episodes.
fn render_replay_summary(out: &mut String, report: &ReplayReport) {
    let _ = writeln!(
        out,
        "summary by pull request (flag: episodes, first seen, digested?):"
    );
    let mut summary: std::collections::BTreeMap<(u64, u8), Vec<&crate::pr_watch::replay::Episode>> =
        std::collections::BTreeMap::new();
    for episode in &report.episodes {
        summary
            .entry((episode.pr, episode.flag))
            .or_default()
            .push(episode);
    }
    for ((pr, flag), episodes) in &summary {
        let first = episodes[0];
        let digested = episodes.iter().any(|episode| episode.digested_at.is_some());
        let _ = writeln!(
            out,
            "  #{pr} flag {flag} {}: {} episode(s), first {}, {} — {}",
            first.kind,
            episodes.len(),
            first.first_seen_at.format("%m-%d %H:%MZ"),
            if digested { "digested" } else { "not digested" },
            first.first_evidence
        );
    }
}

fn render_replay(report: &ReplayReport) -> String {
    let mut out = format!(
        "{} replay {} .. {} every {}m: {} ticks, {} PRs, required checks: {}\n",
        report.repo,
        report.from.format("%Y-%m-%dT%H:%MZ"),
        report.to.format("%Y-%m-%dT%H:%MZ"),
        report.tick_minutes,
        report.ticks,
        report.prs,
        report.required_checks.join(", "),
    );
    for expectation in &report.expectations {
        let _ = writeln!(
            out,
            "expect #{} flags {:?}: raised {:?} -> {}",
            expectation.pr,
            expectation.expected,
            expectation.raised,
            if expectation.pass {
                "PASS".to_owned()
            } else {
                format!("FAIL (missing {:?})", expectation.missing)
            }
        );
    }
    if let Some(control) = &report.control {
        let _ = writeln!(
            out,
            "control merged-clean: {} clean merged PRs, {} flagged -> {}",
            control.clean_merged.len(),
            control.violations.len(),
            if control.pass { "PASS" } else { "FAIL" }
        );
        for episode in &control.violations {
            let _ = writeln!(
                out,
                "  violation #{} flag {}: {}",
                episode.pr, episode.flag, episode.first_evidence
            );
        }
    }
    let _ = writeln!(
        out,
        "episodes: {} ({} would have reached a digest; {} digests)",
        report.episodes.len(),
        report
            .episodes
            .iter()
            .filter(|episode| episode.digested_at.is_some())
            .count(),
        report.digests
    );
    render_digest_stats(&mut out, &report.digest_stats);
    render_replay_summary(&mut out, report);
    let _ = writeln!(out, "episodes:");
    for episode in &report.episodes {
        let _ = writeln!(
            out,
            "  #{} flag {} {} [{}] {} .. {} ({} ticks{}{}): {}",
            episode.pr,
            episode.flag,
            episode.kind,
            episode.verdict,
            episode.first_seen_at.format("%m-%d %H:%MZ"),
            episode.last_seen_at.format("%m-%d %H:%MZ"),
            episode.ticks,
            episode
                .digested_at
                .map(|at| format!(", digest {}", at.format("%m-%d %H:%MZ")))
                .unwrap_or_default(),
            episode
                .addressed_reason
                .as_deref()
                .map(|reason| format!(", ended: {reason}"))
                .unwrap_or_default(),
            episode.last_evidence
        );
    }
    for gap in &report.gaps {
        let _ = writeln!(out, "gap: {gap}");
    }
    let _ = writeln!(out, "result: {}", if report.pass { "PASS" } else { "FAIL" });
    out
}

fn digest_command<W: Write>(
    args: PrWatchDigestArgs,
    watch: &WatchConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let repo = super::runner_cmd::resolve_repo_slug(args.repo, cwd)?;
    let base = args.base.unwrap_or_else(|| watch.base.clone());
    let state_path = args
        .state_file
        .unwrap_or_else(|| ledger::default_path(&runtime_paths.state_dir, &repo, &base));
    if args.post && watch.digest_command.is_empty() {
        return Err(CliFailure::new(
            WAIT_EXIT_INVALID,
            "pr-watch digest --post needs [pr_watch.digest] command = [\"...\"]",
        ));
    }
    let _lock = ledger::lock(&state_path).map_err(io_failure)?;
    let mut state = ledger::load(&state_path, &repo, &base).map_err(io_failure)?;
    let mut persist = |ledger: &ledger::Ledger| ledger::save(&state_path, ledger);
    let digest_argv = watch.digest_command.clone();
    let mut send = |payload: &str| run_digest_command(&digest_argv, payload);
    let (outcome, payload) = digest::run(
        &mut state,
        Utc::now(),
        watch.digest_policy,
        args.post,
        &mut persist,
        &mut send,
    )
    .map_err(|error| CliFailure::new(1, error))?;
    if json {
        write_pretty_json(
            stdout,
            &serde_json::json!({"outcome": outcome, "payload": payload}),
        )
        .map_err(io_failure)?;
    } else {
        let _ = writeln!(stdout, "digest: {}", render_digest_outcome(&outcome));
        if let Some(payload) = payload {
            let text = serde_json::to_string_pretty(&payload).map_err(io_failure)?;
            writeln!(stdout, "{text}").map_err(io_failure)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}
