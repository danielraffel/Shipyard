use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::app::cli::{
    MetricsCommand, MetricsFreshnessArgs, MetricsGroupBy, MetricsImportCommand,
    MetricsImportGithubArgs, MetricsImportTartciArgs, MetricsRecordArgs,
};
use crate::app::{CliFailure, WAIT_EXIT_INVALID};
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::metrics::freshness::{
    DEFAULT_STALE_AFTER_HOURS, Freshness, FreshnessStatus, parse_stale_after,
};
use crate::metrics::github_import::{self, GithubImportRequest};
#[cfg(test)]
use crate::metrics::github_import::{
    github_jobs_api_path, github_runs_api_path, workflow_run_single_pr,
};
use crate::metrics::proxy::{Basis, ProxyValue, WALL_CONTEXT_LABEL};
use crate::metrics::{
    GateClass, MetricRecordInput, MetricsFinding, MetricsJobRow, MetricsStore, MetricsSummaryRow,
    StewardshipScorecard, SummaryGroupBy, parse_duration_ms,
};
use crate::output::write_pretty_json;

const GITHUB_METRICS_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);

/// Exit code for `--fail-on-stale` when the store is STALE or EMPTY.
pub(super) const METRICS_EXIT_STALE: u8 = 3;

#[derive(Debug, Serialize)]
struct MetricsRecordOutput {
    database: String,
    job_id: i64,
}

#[derive(Debug, Serialize)]
struct MetricsImportOutput {
    database: String,
    source: String,
    imported: usize,
}

#[derive(Debug, Serialize)]
struct MetricsRowsOutput<T> {
    database: String,
    rows: Vec<T>,
}

#[derive(Debug, Serialize)]
struct MetricsTrendOutput {
    database: String,
    basis: &'static str,
    rows: Vec<MetricsJobRow>,
    trend: Vec<MetricsFinding>,
}

#[derive(Debug, Serialize)]
struct MetricsSummaryOutput {
    database: String,
    group_by: SummaryGroupBy,
    rows: Vec<MetricsSummaryRow>,
    freshness: Freshness,
}

#[derive(Debug, Serialize)]
struct MetricsFindingsOutput {
    database: String,
    project: String,
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    required: Option<RequiredChecks>,
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness: Option<Freshness>,
    findings: Vec<MetricsFinding>,
}

/// `scorecard --json`: the scorecard's own keys plus `freshness`.
#[derive(Debug, Serialize)]
struct MetricsScorecardOutput<'a> {
    #[serde(flatten)]
    scorecard: &'a StewardshipScorecard,
    freshness: &'a Freshness,
}

/// `--stale-after` wins; otherwise `[metrics] stale_after`; otherwise 24h.
fn resolve_stale_after(
    flag: Option<&str>,
    config_cwd: Option<(RuntimeMode, &Path)>,
) -> Result<chrono::Duration, CliFailure> {
    let configured = || {
        config_cwd
            .and_then(|(mode, cwd)| LoadedConfig::load_from_cwd(mode, cwd).ok())
            .and_then(|config| config.get_str("metrics.stale_after").map(str::to_owned))
    };
    match flag.map(str::to_owned).or_else(configured) {
        Some(text) => {
            parse_stale_after(&text).map_err(|error| CliFailure::new(WAIT_EXIT_INVALID, error))
        }
        None => Ok(chrono::Duration::hours(DEFAULT_STALE_AFTER_HOURS)),
    }
}

fn freshness_for(
    store: &MetricsStore,
    project: Option<&str>,
    args: &MetricsFreshnessArgs,
    config_cwd: Option<(RuntimeMode, &Path)>,
) -> Result<Freshness, CliFailure> {
    let threshold = resolve_stale_after(args.stale_after.as_deref(), config_cwd)?;
    store
        .freshness(project, threshold, Utc::now())
        .map_err(|error| CliFailure::new(1, format!("metrics freshness failed: {error}")))
}

/// Lead a stale verdict with a finding saying so, and stop "insufficient
/// sample" findings from blaming lanes for a missing import.
fn annotate_findings(findings: &mut Vec<MetricsFinding>, freshness: &Freshness) {
    if !freshness.is_degraded() {
        return;
    }
    for finding in findings.iter_mut() {
        if finding.signal.starts_with("insufficient") {
            finding.message = format!(
                "{} The store is {}: the shortage is likely the missing import, not the lane.",
                finding.message,
                freshness.status.as_str().to_ascii_uppercase()
            );
        }
    }
    findings.insert(
        0,
        MetricsFinding {
            severity: freshness.status.as_str().to_owned(),
            lane: "*".to_owned(),
            signal: format!("{}_data", freshness.status.as_str()),
            message: freshness.message.clone(),
            sample_count: freshness.sources.iter().map(|source| source.samples).sum(),
            suggested_poll_interval_secs: 600,
            recommended_actions: vec![
                "Run `shipyard metrics import github --repo <owner/repo>` now.".to_owned(),
                "Enable the daemon's scheduled import: [metrics.import] enabled = true in the \
                 machine-global config."
                    .to_owned(),
            ],
            basis: "freshness",
            comparison: None,
            denominator: None,
            gate: None,
        },
    );
}

fn stale_exit(freshness: &Freshness, fail_on_stale: bool) -> std::process::ExitCode {
    if fail_on_stale && freshness.status != FreshnessStatus::Fresh {
        std::process::ExitCode::from(METRICS_EXIT_STALE)
    } else {
        std::process::ExitCode::SUCCESS
    }
}

/// The required status checks `watch` classified lanes against, and where
/// they came from.
#[derive(Clone, Debug, Serialize)]
struct RequiredChecks {
    /// `flag`, `config` (`[governance] required_status_checks`), or `none`.
    source: &'static str,
    checks: Vec<String>,
}

/// `--required` wins; otherwise the repo config's required status checks.
fn resolve_required(
    flags: Vec<String>,
    config_cwd: Option<(RuntimeMode, &Path)>,
) -> RequiredChecks {
    if !flags.is_empty() {
        return RequiredChecks {
            source: "flag",
            checks: flags,
        };
    }
    let from_config: Vec<String> = config_cwd
        .and_then(|(mode, cwd)| LoadedConfig::load_from_cwd(mode, cwd).ok())
        .and_then(|config| {
            config
                .get("governance.required_status_checks")
                .and_then(toml::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
        })
        .unwrap_or_default();
    if from_config.is_empty() {
        RequiredChecks {
            source: "none",
            checks: Vec::new(),
        }
    } else {
        RequiredChecks {
            source: "config",
            checks: from_config,
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn metrics_command<W: Write>(
    command: MetricsCommand,
    state_dir: &Path,
    config_cwd: Option<(RuntimeMode, &Path)>,
    json_output: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let store = MetricsStore::open(state_dir)
        .map_err(|error| CliFailure::new(2, format!("metrics store error: {error}")))?;
    match command {
        MetricsCommand::Record(args) => {
            let input = record_input(*args)?;
            let job_id = store
                .record(&input)
                .map_err(|error| CliFailure::new(1, format!("metrics record failed: {error}")))?;
            let output = MetricsRecordOutput {
                database: store.path().display().to_string(),
                job_id,
            };
            write_output(stdout, json_output, &output, || {
                format!("recorded job {job_id}")
            })?;
        }
        MetricsCommand::Import { source } => match source {
            MetricsImportCommand::Tartci(args) => {
                let imported = import_tartci(&store, &args)?;
                let output = MetricsImportOutput {
                    database: store.path().display().to_string(),
                    source: "tartci".to_owned(),
                    imported,
                };
                write_output(stdout, json_output, &output, || {
                    format!("imported {imported} tartci metric rows")
                })?;
            }
            MetricsImportCommand::Github(args) => {
                let imported = import_github(&store, &args)?;
                let output = MetricsImportOutput {
                    database: store.path().display().to_string(),
                    source: "github".to_owned(),
                    imported,
                };
                write_output(stdout, json_output, &output, || {
                    format!("imported {imported} GitHub job rows")
                })?;
            }
        },
        MetricsCommand::List(args) => {
            let rows = store
                .list(args.project.as_deref(), args.limit)
                .map_err(|error| CliFailure::new(1, format!("metrics list failed: {error}")))?;
            write_rows(stdout, json_output, store.path(), rows)?;
        }
        MetricsCommand::Trend(args) => {
            let since_days = parse_days(&args.since)?;
            let basis = Basis::from(args.basis);
            let rows = store
                .list(args.project.as_deref(), args.limit)
                .map_err(|error| CliFailure::new(1, format!("metrics trend failed: {error}")))?;
            let trend = store
                .trend(args.project.as_deref(), since_days, basis)
                .map_err(|error| CliFailure::new(1, format!("metrics trend failed: {error}")))?;
            write_trend(stdout, json_output, store.path(), basis, rows, trend)?;
        }
        MetricsCommand::Summary(args) => {
            let group_by = match args.group_by {
                MetricsGroupBy::Runner => SummaryGroupBy::Runner,
                MetricsGroupBy::Host => SummaryGroupBy::Host,
            };
            let rows = store
                .summary_grouped(args.project.as_deref(), group_by)
                .map_err(|error| CliFailure::new(1, format!("metrics summary failed: {error}")))?;
            let freshness =
                freshness_for(&store, args.project.as_deref(), &args.freshness, config_cwd)?;
            let exit = stale_exit(&freshness, args.freshness.fail_on_stale);
            write_summary(stdout, json_output, store.path(), group_by, rows, freshness)?;
            return Ok(exit);
        }
        MetricsCommand::Slowest(args) => {
            let rows = store
                .slowest(args.project.as_deref(), args.limit)
                .map_err(|error| CliFailure::new(1, format!("metrics slowest failed: {error}")))?;
            write_rows(stdout, json_output, store.path(), rows)?;
        }
        MetricsCommand::Compare(args) => {
            let _lane = args.lane.as_deref();
            let _before_days = args.before.as_deref().map(parse_days).transpose()?;
            let split_days_ago = args
                .after
                .as_deref()
                .map(parse_days)
                .transpose()?
                .unwrap_or(args.split_days_ago);
            let findings = store
                .compare(&args.project, split_days_ago, args.basis.into())
                .map_err(|error| CliFailure::new(1, format!("metrics compare failed: {error}")))?;
            write_findings(
                stdout,
                json_output,
                store.path(),
                (args.project, None),
                (None, None),
                findings,
            )?;
        }
        MetricsCommand::Watch(args) => {
            let since_days = parse_days(&args.since)?;
            let required = resolve_required(args.required, config_cwd);
            let freshness =
                freshness_for(&store, Some(&args.project), &args.freshness, config_cwd)?;
            let exit = stale_exit(&freshness, args.freshness.fail_on_stale);
            let mut findings = store
                .watch_with_required(
                    &args.project,
                    since_days,
                    args.basis.into(),
                    &required.checks,
                )
                .map_err(|error| CliFailure::new(1, format!("metrics watch failed: {error}")))?;
            annotate_findings(&mut findings, &freshness);
            write_findings(
                stdout,
                json_output,
                store.path(),
                (args.project, None),
                (Some(required), Some(freshness)),
                findings,
            )?;
            return Ok(exit);
        }
        MetricsCommand::Advise(args) => {
            let freshness =
                freshness_for(&store, Some(&args.project), &args.freshness, config_cwd)?;
            let exit = stale_exit(&freshness, args.freshness.fail_on_stale);
            let mut findings = store
                .advise(&args.project)
                .map_err(|error| CliFailure::new(1, format!("metrics advise failed: {error}")))?;
            annotate_findings(&mut findings, &freshness);
            write_findings(
                stdout,
                json_output,
                store.path(),
                (args.project, args.profile),
                (None, Some(freshness)),
                findings,
            )?;
            return Ok(exit);
        }
        MetricsCommand::Scorecard(args) => {
            let since_days = parse_days(&args.since)?;
            let scorecard = store
                .stewardship_scorecard_with_basis(&args.project, since_days, args.basis.into())
                .map_err(|error| {
                    CliFailure::new(1, format!("metrics scorecard failed: {error}"))
                })?;
            let freshness =
                freshness_for(&store, Some(&args.project), &args.freshness, config_cwd)?;
            let exit = stale_exit(&freshness, args.freshness.fail_on_stale);
            let output = MetricsScorecardOutput {
                scorecard: &scorecard,
                freshness: &freshness,
            };
            write_output(stdout, json_output, &output, || {
                let mut text = if freshness.is_degraded() {
                    format!("{}\n", freshness.message)
                } else {
                    String::new()
                };
                text.push_str(&scorecard_proxy_lines(&scorecard));
                let coverage = format!(
                    "{}: {} jobs, {:.2} worker-minutes, {} PRs over {}d; worker-minutes coverage={} ({}); PR coverage={} ({}); submit-to-receipt={} ({}); model-tokens={} ({})",
                    scorecard.project,
                    scorecard.job_samples,
                    scorecard.worker_minutes,
                    scorecard.distinct_pull_requests,
                    scorecard.since_days,
                    scorecard.worker_minutes_coverage.status,
                    scorecard.worker_minutes_coverage.reason,
                    scorecard.pull_request_throughput.status,
                    scorecard.pull_request_throughput.reason,
                    scorecard.submit_to_receipt.status,
                    scorecard.submit_to_receipt.reason,
                    scorecard.model_token_use.status,
                    scorecard.model_token_use.reason,
                );
                text.push_str(&coverage);
                text
            })?;
            return Ok(exit);
        }
        MetricsCommand::GateCost(_) => {
            return Err(CliFailure::new(
                2,
                "metrics gate-cost is dispatched before the metrics store opens",
            ));
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn record_input(args: MetricsRecordArgs) -> Result<MetricRecordInput, CliFailure> {
    let duration_ms = match (args.duration_ms, args.duration.as_deref()) {
        (Some(value), None) => value,
        (None, Some(value)) => {
            parse_duration_ms(value).map_err(|error| CliFailure::new(WAIT_EXIT_INVALID, error))?
        }
        (None, None) => {
            return Err(CliFailure::new(
                WAIT_EXIT_INVALID,
                "metrics record requires --duration-ms or --duration",
            ));
        }
        (Some(_), Some(_)) => unreachable!("clap conflicts duration flags"),
    };
    Ok(MetricRecordInput {
        project: args.project,
        repo: args.repo,
        branch: args.branch,
        sha: args.sha,
        pr: args.pr,
        workflow: args.workflow,
        profile: args.profile,
        routing_decision: args.routing_decision,
        job: args.job,
        target: args.target,
        platform: args.platform,
        backend: args.backend,
        provider: args.provider,
        runner: args.runner,
        host: args.host,
        step: args.step,
        duration_ms,
        status: args.status,
        exit_code: args.exit_code,
        failure_class: args.failure_class,
        external_id: args.external_id,
        queued_at: parse_rfc3339(args.queued_at.as_deref())?,
        cache_hit: None,
        runner_assigned: args.runner_assigned,
        labels: (!args.labels.is_empty()).then_some(args.labels),
        started_at: parse_rfc3339(args.started_at.as_deref())?,
        completed_at: parse_rfc3339(args.completed_at.as_deref())?,
    })
}

fn parse_rfc3339(value: Option<&str>) -> Result<Option<DateTime<Utc>>, CliFailure> {
    value
        .map(|text| {
            DateTime::parse_from_rfc3339(text)
                .map(|timestamp| timestamp.with_timezone(&Utc))
                .map_err(|error| {
                    CliFailure::new(
                        WAIT_EXIT_INVALID,
                        format!("invalid timestamp {text:?}: {error}"),
                    )
                })
        })
        .transpose()
}

fn parse_days(value: &str) -> Result<i64, CliFailure> {
    let trimmed = value.trim();
    let days = trimmed.strip_suffix('d').unwrap_or(trimmed);
    days.parse::<i64>().map_err(|_| {
        CliFailure::new(
            WAIT_EXIT_INVALID,
            format!("invalid day window {value:?}; use a number or Nd, for example 14d"),
        )
    })
}

fn import_tartci(
    store: &MetricsStore,
    args: &MetricsImportTartciArgs,
) -> Result<usize, CliFailure> {
    let mut text = String::new();
    match args.file.as_ref().and_then(|path| path.to_str()) {
        None | Some("-") => {
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|error| CliFailure::new(1, format!("stdin read failed: {error}")))?;
        }
        Some(_) => {
            let path = args.file.as_ref().expect("file path");
            text = std::fs::read_to_string(path).map_err(|error| {
                CliFailure::new(1, format!("could not read {}: {error}", path.display()))
            })?;
        }
    }
    store
        .import_tartci(&text)
        .map_err(|error| CliFailure::new(1, format!("tartci import failed: {error}")))
}

fn import_github(
    store: &MetricsStore,
    args: &MetricsImportGithubArgs,
) -> Result<usize, CliFailure> {
    let request = GithubImportRequest {
        repo: args.repo.clone(),
        project: args.project.clone(),
        workflow: args.workflow.clone(),
        branch: args.branch.clone(),
        limit: args.limit,
    };
    let mut gh = |argv: &[String]| gh_json(argv);
    github_import::import_github(store, &request, &mut gh, &|message| {
        CliFailure::new(1, message)
    })
}

fn gh_json(args: &[String]) -> Result<Value, CliFailure> {
    gh_json_with_timeout("gh", args, GITHUB_METRICS_OBSERVATION_TIMEOUT)
}

fn gh_json_with_timeout(
    program: impl AsRef<Path>,
    args: &[String],
    timeout: Duration,
) -> Result<Value, CliFailure> {
    // Metrics import is observational and must never strand the invoking
    // agent behind an unbounded gh subprocess. Capture to regular files so an
    // escaped descendant cannot keep a pipe reader blocked, and supervise the
    // complete process tree under one fixed deadline.
    let mut command = Command::new(program.as_ref());
    command.args(args);
    let deadline = Instant::now() + timeout;
    let output =
        crate::process::run_output_until(&mut command, deadline, "metrics GitHub observation")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if !output.status.success() {
        return Err(CliFailure::new(
            u8::try_from(output.status.code().unwrap_or(1)).unwrap_or(1),
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| CliFailure::new(1, format!("gh JSON parse failed: {error}")))
}

fn write_rows<W: Write>(
    stdout: &mut W,
    json_output: bool,
    db_path: &Path,
    rows: Vec<MetricsJobRow>,
) -> Result<(), CliFailure> {
    if json_output {
        return write_output(
            stdout,
            true,
            &MetricsRowsOutput {
                database: db_path.display().to_string(),
                rows,
            },
            String::new,
        );
    }
    writeln!(
        stdout,
        "project\tjob\ttarget\tbackend\tprovider\thost\tstatus\ttotal_ms"
    )
    .map_err(io_error)?;
    for row in rows {
        writeln!(
            stdout,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.project,
            row.job,
            row.target.unwrap_or_default(),
            row.backend.unwrap_or_default(),
            row.provider.unwrap_or_default(),
            row.host.unwrap_or_default(),
            row.status,
            row.total_ms
                .map_or(String::new(), |value| value.to_string())
        )
        .map_err(io_error)?;
    }
    Ok(())
}

fn write_summary<W: Write>(
    stdout: &mut W,
    json_output: bool,
    db_path: &Path,
    group_by: SummaryGroupBy,
    rows: Vec<MetricsSummaryRow>,
    freshness: Freshness,
) -> Result<(), CliFailure> {
    if json_output {
        return write_output(
            stdout,
            true,
            &MetricsSummaryOutput {
                database: db_path.display().to_string(),
                group_by,
                rows,
                freshness,
            },
            String::new,
        );
    }
    if freshness.is_degraded() {
        writeln!(stdout, "{}", freshness.message).map_err(io_error)?;
    }
    let machine = match group_by {
        SummaryGroupBy::Runner => "runner",
        SummaryGroupBy::Host => "host",
    };
    writeln!(
        stdout,
        "project\ttarget\tbackend\t{machine}\tprovider\tcount\tfail_rate\tp50_ms\tp90_ms"
    )
    .map_err(io_error)?;
    for row in rows {
        writeln!(
            stdout,
            "{}\t{}\t{}\t{}\t{}\t{}\t{:.2}\t{}\t{}",
            row.project,
            row.target,
            row.backend,
            row.host,
            row.provider,
            row.count,
            row.failure_rate,
            row.p50_ms.map_or(String::new(), |value| value.to_string()),
            row.p90_ms.map_or(String::new(), |value| value.to_string())
        )
        .map_err(io_error)?;
    }
    Ok(())
}

fn write_findings<W: Write>(
    stdout: &mut W,
    json_output: bool,
    db_path: &Path,
    (project, profile): (String, Option<String>),
    (required, freshness): (Option<RequiredChecks>, Option<Freshness>),
    findings: Vec<MetricsFinding>,
) -> Result<(), CliFailure> {
    if json_output {
        return write_output(
            stdout,
            true,
            &MetricsFindingsOutput {
                database: db_path.display().to_string(),
                project,
                profile,
                required,
                freshness,
                findings,
            },
            String::new,
        );
    }
    if let Some(freshness) = freshness
        .as_ref()
        .filter(|freshness| freshness.is_degraded())
    {
        writeln!(stdout, "{}", freshness.message).map_err(io_error)?;
    }
    if let Some(required) = &required {
        if required.checks.is_empty() {
            writeln!(
                stdout,
                "required checks: none known (pass --required or set [governance] \
                 required_status_checks); every lane is unclassified"
            )
            .map_err(io_error)?;
        } else {
            writeln!(
                stdout,
                "required checks ({}): {}",
                required.source,
                required.checks.join(", ")
            )
            .map_err(io_error)?;
        }
    }
    // The banner above already carries the freshness finding.
    let findings: Vec<MetricsFinding> = findings
        .into_iter()
        .filter(|finding| finding.basis != "freshness")
        .collect();
    if findings.is_empty() {
        writeln!(stdout, "No material findings.").map_err(io_error)?;
        return Ok(());
    }
    if let Some(basis) = findings
        .iter()
        .find(|finding| finding.comparison.is_some())
        .map(|finding| finding.basis)
    {
        writeln!(stdout, "basis: {basis}; wall time is {WALL_CONTEXT_LABEL}").map_err(io_error)?;
    }
    let mut section = None;
    for finding in findings {
        if finding.gate.is_some() && finding.gate != section {
            section = finding.gate;
            let title = match finding.gate {
                Some(GateClass::Required) => "required gates:",
                Some(GateClass::Advisory) => "advisory jobs:",
                _ => "unclassified lanes:",
            };
            writeln!(stdout, "{title}").map_err(io_error)?;
        }
        writeln!(
            stdout,
            "{}\t{}\t{}\t{}",
            finding.severity, finding.lane, finding.signal, finding.message
        )
        .map_err(io_error)?;
        if let Some(denominator) = &finding.denominator {
            writeln!(
                stdout,
                "  denominator: {} named {} (each job's own conclusion, never the workflow \
                 run's): previous n={} ({} success/failure), current n={} ({} success/failure)",
                denominator.unit,
                denominator.job_names.join(" | "),
                denominator.previous_jobs,
                denominator.previous_decided,
                denominator.current_jobs,
                denominator.current_decided,
            )
            .map_err(io_error)?;
        }
    }
    Ok(())
}

fn proxy_value_text(value: &ProxyValue) -> String {
    let measured = value
        .value
        .map_or_else(|| "n/a".to_owned(), |number| format!("{number:.3}"));
    let marker = if value.sufficient {
        ""
    } else {
        ", insufficient sample"
    };
    format!("{}={measured} (n={}{marker})", value.name, value.sample)
}

fn scorecard_proxy_lines(scorecard: &StewardshipScorecard) -> String {
    let proxies: Vec<String> = scorecard.proxies.iter().map(proxy_value_text).collect();
    format!(
        "{} over {}d vs the previous {}d: {}\n  proxies: {}\n  {WALL_CONTEXT_LABEL}: duration p50 {} ms, p90 {} ms; queue p50 {} ms, p90 {} ms\n",
        scorecard.project,
        scorecard.since_days,
        scorecard.since_days,
        scorecard.comparison.summary(),
        proxies.join(", "),
        opt_ms(scorecard.duration_p50_ms),
        opt_ms(scorecard.duration_p90_ms),
        opt_ms(scorecard.queue_p50_ms),
        opt_ms(scorecard.queue_p90_ms),
    )
}

fn opt_ms(value: Option<i64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| value.to_string())
}

fn write_trend<W: Write>(
    stdout: &mut W,
    json_output: bool,
    db_path: &Path,
    basis: Basis,
    rows: Vec<MetricsJobRow>,
    trend: Vec<MetricsFinding>,
) -> Result<(), CliFailure> {
    if json_output {
        return write_output(
            stdout,
            true,
            &MetricsTrendOutput {
                database: db_path.display().to_string(),
                basis: basis.as_str(),
                rows,
                trend,
            },
            String::new,
        );
    }
    writeln!(
        stdout,
        "trend verdicts (basis {}; wall time is {WALL_CONTEXT_LABEL}):",
        basis.as_str()
    )
    .map_err(io_error)?;
    for item in &trend {
        writeln!(stdout, "  {}\t{}", item.lane, item.message).map_err(io_error)?;
    }
    write_rows(stdout, false, db_path, rows)
}

fn write_output<W: Write, T: Serialize, F: FnOnce() -> String>(
    stdout: &mut W,
    json_output: bool,
    value: &T,
    table: F,
) -> Result<(), CliFailure> {
    if json_output {
        write_pretty_json(stdout, value).map_err(|error| CliFailure::new(1, error.to_string()))
    } else {
        writeln!(stdout, "{}", table()).map_err(io_error)
    }
}

#[allow(clippy::needless_pass_by_value)]
fn io_error(error: std::io::Error) -> CliFailure {
    CliFailure::new(1, error.to_string())
}

#[allow(dead_code)]
fn _json_debug(value: &impl Serialize) -> Value {
    json!(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_freshness_flags() -> MetricsFreshnessArgs {
        MetricsFreshnessArgs {
            stale_after: None,
            fail_on_stale: false,
        }
    }

    #[cfg(unix)]
    #[test]
    fn github_metrics_observation_times_out_escaped_helper() {
        let temp = tempfile::tempdir().expect("tempdir");
        let helper = temp.path().join("gh");
        crate::test_support::write_executable_script_with_mode(
            &helper,
            "#!/bin/sh\nsleep 2\n",
            0o700,
        );

        let error = gh_json_with_timeout(&helper, &[], Duration::from_millis(50))
            .expect_err("hung helper must time out");
        assert!(error.message().contains("timed out"));
    }

    #[test]
    fn github_api_paths_are_absolute() {
        assert_eq!(
            github_runs_api_path("danielraffel/pulp", None),
            "/repos/danielraffel/pulp/actions/runs"
        );
        assert_eq!(
            github_runs_api_path("danielraffel/pulp", Some("build.yml")),
            "/repos/danielraffel/pulp/actions/workflows/build.yml/runs"
        );
        assert_eq!(
            github_jobs_api_path("danielraffel/pulp", 123),
            "/repos/danielraffel/pulp/actions/runs/123/jobs"
        );
    }

    #[test]
    fn workflow_run_pr_identity_requires_exactly_one_pr() {
        assert_eq!(
            workflow_run_single_pr(&json!({"pull_requests": [{"number": 538}]})),
            Some(538)
        );
        assert_eq!(
            workflow_run_single_pr(&json!({"pull_requests": [{"number": 538}, {"number": 539}]})),
            None
        );
        assert_eq!(workflow_run_single_pr(&json!({"pull_requests": []})), None);
    }

    #[test]
    fn json_outputs_keep_their_existing_keys_and_add_proxy_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let run = |command: MetricsCommand| {
            let mut output = Vec::new();
            metrics_command(command, temp.path(), None, true, &mut output).expect("command");
            serde_json::from_slice::<Value>(&output).expect("json")
        };
        let scorecard = run(MetricsCommand::Scorecard(
            crate::app::cli::MetricsWatchArgs {
                project: "shipyard".to_owned(),
                since: "14d".to_owned(),
                required: Vec::new(),
                basis: crate::app::cli::MetricsBasis::Proxy,
                freshness: no_freshness_flags(),
            },
        ));
        for key in [
            "project",
            "job_samples",
            "worker_minutes",
            "duration_p50_ms",
            "duration_p90_ms",
            "queue_p50_ms",
            "cache_hit_rate",
            "submit_to_receipt",
            "model_token_use",
            "basis",
            "proxies",
            "comparison",
        ] {
            assert!(scorecard.get(key).is_some(), "scorecard lost {key}");
        }
        assert_eq!(scorecard["basis"], "proxy");
        assert_eq!(scorecard["comparison"]["verdict"], "insufficient_sample");
        assert_eq!(
            scorecard["comparison"]["context"]["label"],
            "context (load-dependent)"
        );
        let trend = run(MetricsCommand::Trend(crate::app::cli::MetricsTrendArgs {
            project: Some("shipyard".to_owned()),
            limit: 5,
            since: "14d".to_owned(),
            basis: crate::app::cli::MetricsBasis::WallTime,
        }));
        assert!(trend["rows"].is_array());
        assert!(trend["trend"].is_array());
        assert_eq!(trend["basis"], "wall_time");
    }

    #[test]
    fn scorecard_human_output_surfaces_coverage_gaps() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut output = Vec::new();
        metrics_command(
            MetricsCommand::Scorecard(crate::app::cli::MetricsWatchArgs {
                project: "shipyard".to_owned(),
                since: "14d".to_owned(),
                required: Vec::new(),
                basis: crate::app::cli::MetricsBasis::Proxy,
                freshness: no_freshness_flags(),
            }),
            temp.path(),
            None,
            false,
            &mut output,
        )
        .expect("scorecard command");
        let output = String::from_utf8(output).expect("UTF-8 output");

        let mut lines = output.lines();
        assert!(
            lines
                .next()
                .unwrap_or_default()
                .starts_with("EMPTY: no metrics rows for project shipyard"),
            "an empty store must say so first: {output}"
        );
        let first = lines.next().unwrap_or_default();
        assert!(
            first.contains("insufficient_sample (basis proxy"),
            "proxy verdict must lead: {output}"
        );
        assert!(output.contains("context (load-dependent): duration p50"));

        assert!(output.contains("PR coverage=unavailable"));
        assert!(output.contains("worker-minutes coverage=unavailable"));
        assert!(output.contains("submit-to-receipt=unavailable"));
        assert!(output.contains("model-tokens=unavailable"));
    }

    #[test]
    fn watch_human_output_names_its_required_set_and_job_denominator() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        for (days_ago, status, count) in [(10, "success", 12), (2, "success", 6), (2, "failure", 6)]
        {
            for index in 0..count {
                let completed = Utc::now() - chrono::Duration::days(days_ago)
                    + chrono::Duration::minutes(index);
                store
                    .record(&MetricRecordInput {
                        project: "p".to_owned(),
                        job: "macos".to_owned(),
                        target: Some("macos-gate/merge_group".to_owned()),
                        duration_ms: 60_000,
                        status: status.to_owned(),
                        completed_at: Some(completed),
                        ..MetricRecordInput::default()
                    })
                    .expect("record");
            }
        }
        let watch = |required: Vec<String>, json: bool| {
            let mut output = Vec::new();
            metrics_command(
                MetricsCommand::Watch(crate::app::cli::MetricsWatchArgs {
                    project: "p".to_owned(),
                    since: "7d".to_owned(),
                    required,
                    basis: crate::app::cli::MetricsBasis::Proxy,
                    // The fixture's newest row is two days old.
                    freshness: MetricsFreshnessArgs {
                        stale_after: Some("30d".to_owned()),
                        fail_on_stale: false,
                    },
                }),
                temp.path(),
                None,
                json,
                &mut output,
            )
            .expect("watch");
            String::from_utf8(output).expect("utf-8")
        };
        let text = watch(vec!["macos".to_owned()], false);
        assert!(
            text.starts_with("required checks (flag): macos\n"),
            "{text}"
        );
        assert!(text.contains("required gates:\n"), "{text}");
        assert!(text.contains("(n=12/12 jobs (success+failure))"), "{text}");
        assert!(
            text.contains("denominator: jobs named macos (each job's own conclusion"),
            "{text}"
        );
        assert!(
            text.contains("previous n=12 (12 success/failure), current n=12"),
            "{text}"
        );

        let unknown = watch(Vec::new(), false);
        assert!(
            unknown.starts_with("required checks: none known"),
            "{unknown}"
        );
        assert!(unknown.contains("unclassified lanes:"), "{unknown}");

        let json: Value =
            serde_json::from_str(&watch(vec!["macos".to_owned()], true)).expect("json");
        assert_eq!(json["required"]["source"], "flag");
        assert_eq!(json["findings"][0]["gate"], "required");
        assert_eq!(json["findings"][0]["denominator"]["unit"], "jobs");
        assert_eq!(json["findings"][0]["denominator"]["current_decided"], 12);
    }

    /// A store written before slugs were accepted (`project = "pulp"`,
    /// `repo = "Generous-Corp/pulp"`) whose last GitHub import is three days
    /// old: every verdict command must find it by slug and call it STALE.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn stale_legacy_store_is_found_by_slug_and_reported_stale() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = MetricsStore::open(temp.path()).expect("store");
        let completed = Utc::now() - chrono::Duration::days(3);
        let conn = rusqlite::Connection::open(store.path()).expect("db");
        conn.execute(
            "INSERT INTO runs (ts, project, repo, status) VALUES (?1, 'pulp', 'Generous-Corp/pulp', 'success')",
            [completed.to_rfc3339()],
        )
        .expect("run");
        conn.execute(
            "INSERT INTO jobs (run_id, job, target, backend, provider, completed_at, total_ms, status, external_id)
             VALUES (1, 'macos', 'macos', 'local', 'self-hosted', ?1, 60000, 'success', 'github:1/1/1')",
            [completed.to_rfc3339()],
        )
        .expect("job");
        conn.execute(
            "INSERT INTO steps (job_id, step, duration_ms, status) VALUES (1, 'github_job', 60000, 'success')",
            [],
        )
        .expect("step");
        drop(conn);

        let run = |command: MetricsCommand, json: bool| {
            let mut output = Vec::new();
            let exit =
                metrics_command(command, temp.path(), None, json, &mut output).expect("command");
            (exit, String::from_utf8(output).expect("utf-8"))
        };
        let watch = |fail_on_stale: bool| {
            MetricsCommand::Watch(crate::app::cli::MetricsWatchArgs {
                project: "Generous-Corp/pulp".to_owned(),
                since: "14d".to_owned(),
                required: Vec::new(),
                basis: crate::app::cli::MetricsBasis::WallTime,
                freshness: MetricsFreshnessArgs {
                    stale_after: None,
                    fail_on_stale,
                },
            })
        };

        let (exit, text) = run(watch(false), false);
        assert_eq!(exit, std::process::ExitCode::SUCCESS);
        assert!(
            text.starts_with("STALE: last github import "),
            "stale must lead: {text}"
        );
        assert!(
            text.contains("(3d ago; threshold 1d) for project Generous-Corp/pulp"),
            "{text}"
        );
        assert!(
            text.contains("insufficient_samples")
                && text.contains("the missing import, not the lane"),
            "{text}"
        );

        let (_, json) = run(watch(false), true);
        let json: Value = serde_json::from_str(&json).expect("json");
        assert_eq!(json["freshness"]["status"], "stale");
        assert_eq!(json["freshness"]["stale_sources"][0], "github");
        assert_eq!(json["findings"][0]["signal"], "stale_data");

        let (exit, _) = run(watch(true), false);
        assert_eq!(exit, std::process::ExitCode::from(METRICS_EXIT_STALE));

        let (_, advise) = run(
            MetricsCommand::Advise(crate::app::cli::MetricsAdviseArgs {
                project: "Generous-Corp/pulp".to_owned(),
                profile: None,
                freshness: no_freshness_flags(),
            }),
            true,
        );
        let advise: Value = serde_json::from_str(&advise).expect("json");
        assert_eq!(advise["freshness"]["status"], "stale");

        let (_, summary) = run(
            MetricsCommand::Summary(crate::app::cli::MetricsSummaryArgs {
                project: Some("Generous-Corp/pulp".to_owned()),
                group_by: MetricsGroupBy::Runner,
                freshness: no_freshness_flags(),
            }),
            true,
        );
        let summary: Value = serde_json::from_str(&summary).expect("json");
        assert_eq!(
            summary["rows"].as_array().map(Vec::len),
            Some(1),
            "slug must find the pulp row"
        );
        assert_eq!(summary["freshness"]["status"], "stale");

        // A generous threshold turns the same store fresh, and exit 0.
        let (exit, text) = run(
            MetricsCommand::Watch(crate::app::cli::MetricsWatchArgs {
                project: "pulp".to_owned(),
                since: "14d".to_owned(),
                required: Vec::new(),
                basis: crate::app::cli::MetricsBasis::WallTime,
                freshness: MetricsFreshnessArgs {
                    stale_after: Some("7d".to_owned()),
                    fail_on_stale: true,
                },
            }),
            false,
        );
        assert_eq!(exit, std::process::ExitCode::SUCCESS);
        assert!(!text.contains("STALE"), "{text}");
    }
}
