use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::app::cli::{
    MetricsCommand, MetricsGroupBy, MetricsImportCommand, MetricsImportGithubArgs,
    MetricsImportTartciArgs, MetricsRecordArgs,
};
use crate::app::{CliFailure, WAIT_EXIT_INVALID};
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::metrics::proxy::{Basis, ProxyValue, WALL_CONTEXT_LABEL};
use crate::metrics::{
    GateClass, GitHubRunJob, MetricRecordInput, MetricsFinding, MetricsJobRow, MetricsStore,
    MetricsSummaryRow, StewardshipScorecard, SummaryGroupBy, github_job_to_record,
    parse_duration_ms,
};
use crate::output::write_pretty_json;

const GITHUB_METRICS_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);

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
}

#[derive(Debug, Serialize)]
struct MetricsFindingsOutput {
    database: String,
    project: String,
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    required: Option<RequiredChecks>,
    findings: Vec<MetricsFinding>,
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
            write_summary(stdout, json_output, store.path(), group_by, rows)?;
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
                None,
                findings,
            )?;
        }
        MetricsCommand::Watch(args) => {
            let since_days = parse_days(&args.since)?;
            let required = resolve_required(args.required, config_cwd);
            let findings = store
                .watch_with_required(
                    &args.project,
                    since_days,
                    args.basis.into(),
                    &required.checks,
                )
                .map_err(|error| CliFailure::new(1, format!("metrics watch failed: {error}")))?;
            write_findings(
                stdout,
                json_output,
                store.path(),
                (args.project, None),
                Some(required),
                findings,
            )?;
        }
        MetricsCommand::Advise(args) => {
            let findings = store
                .advise(&args.project)
                .map_err(|error| CliFailure::new(1, format!("metrics advise failed: {error}")))?;
            write_findings(
                stdout,
                json_output,
                store.path(),
                (args.project, args.profile),
                None,
                findings,
            )?;
        }
        MetricsCommand::Scorecard(args) => {
            let since_days = parse_days(&args.since)?;
            let scorecard = store
                .stewardship_scorecard_with_basis(&args.project, since_days, args.basis.into())
                .map_err(|error| {
                    CliFailure::new(1, format!("metrics scorecard failed: {error}"))
                })?;
            write_output(stdout, json_output, &scorecard, || {
                let mut text = scorecard_proxy_lines(&scorecard);
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
    let mut run_args = vec![
        "api".to_owned(),
        "-X".to_owned(),
        "GET".to_owned(),
        github_runs_api_path(&args.repo, args.workflow.as_deref()),
        "-f".to_owned(),
        format!("per_page={}", args.limit),
    ];
    if let Some(branch) = &args.branch {
        run_args.push("-f".to_owned());
        run_args.push(format!("branch={branch}"));
    }
    let runs = gh_json(&run_args)?;
    let run_ids = runs
        .get("workflow_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|run| {
            let run_id = run.get("id").and_then(Value::as_i64)?;
            let pr = workflow_run_single_pr(run);
            Some((run_id, pr))
        })
        .collect::<Vec<_>>();
    let project = args.project.clone().unwrap_or_else(|| {
        args.repo
            .rsplit('/')
            .next()
            .unwrap_or(&args.repo)
            .to_owned()
    });
    let mut imported = 0;
    for (run_id, pr) in run_ids {
        let jobs = gh_json(&[
            "api".to_owned(),
            "-X".to_owned(),
            "GET".to_owned(),
            github_jobs_api_path(&args.repo, run_id),
            "-f".to_owned(),
            "per_page=100".to_owned(),
        ])?;
        let Some(job_values) = jobs.get("jobs").and_then(Value::as_array) else {
            continue;
        };
        for value in job_values {
            let mut job: GitHubRunJob = serde_json::from_value(value.clone())
                .map_err(|error| CliFailure::new(1, format!("GitHub job parse failed: {error}")))?;
            job.run_id.get_or_insert(run_id);
            if job.completed_at.is_none() {
                continue;
            }
            let input =
                github_job_to_record(&args.repo, args.workflow.as_deref(), &project, pr, &job);
            store.record_terminal_observation(&input).map_err(|error| {
                CliFailure::new(1, format!("GitHub metrics record failed: {error}"))
            })?;
            imported += 1;
        }
    }
    Ok(imported)
}

fn workflow_run_single_pr(run: &Value) -> Option<i64> {
    let pull_requests = run.get("pull_requests")?.as_array()?;
    let [pull_request] = pull_requests.as_slice() else {
        return None;
    };
    pull_request.get("number").and_then(Value::as_i64)
}

fn github_runs_api_path(repo: &str, workflow: Option<&str>) -> String {
    workflow.map_or_else(
        || format!("/repos/{repo}/actions/runs"),
        |workflow| format!("/repos/{repo}/actions/workflows/{workflow}/runs"),
    )
}

fn github_jobs_api_path(repo: &str, run_id: i64) -> String {
    format!("/repos/{repo}/actions/runs/{run_id}/jobs")
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
) -> Result<(), CliFailure> {
    if json_output {
        return write_output(
            stdout,
            true,
            &MetricsSummaryOutput {
                database: db_path.display().to_string(),
                group_by,
                rows,
            },
            String::new,
        );
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
    required: Option<RequiredChecks>,
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
                findings,
            },
            String::new,
        );
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

    #[cfg(unix)]
    #[test]
    fn github_metrics_observation_times_out_escaped_helper() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let helper = temp.path().join("gh");
        std::fs::write(&helper, "#!/bin/sh\nsleep 2\n").expect("write helper");
        let mut permissions = std::fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&helper, permissions).expect("helper permissions");

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
            }),
            temp.path(),
            None,
            false,
            &mut output,
        )
        .expect("scorecard command");
        let output = String::from_utf8(output).expect("UTF-8 output");

        let first = output.lines().next().unwrap_or_default();
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
}
