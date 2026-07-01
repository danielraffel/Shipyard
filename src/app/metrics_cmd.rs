use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::app::cli::{
    MetricsCommand, MetricsImportCommand, MetricsImportGithubArgs, MetricsImportTartciArgs,
    MetricsRecordArgs,
};
use crate::app::{CliFailure, WAIT_EXIT_INVALID};
use crate::metrics::{
    GitHubRunJob, MetricRecordInput, MetricsFinding, MetricsJobRow, MetricsStore,
    MetricsSummaryRow, github_job_to_record, parse_duration_ms,
};
use crate::output::write_pretty_json;

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
struct MetricsFindingsOutput {
    database: String,
    project: String,
    profile: Option<String>,
    findings: Vec<MetricsFinding>,
}

#[allow(clippy::too_many_lines)]
pub(super) fn metrics_command<W: Write>(
    command: MetricsCommand,
    state_dir: &Path,
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
        MetricsCommand::List(args) | MetricsCommand::Trend(args) => {
            let rows = store
                .list(args.project.as_deref(), args.limit)
                .map_err(|error| CliFailure::new(1, format!("metrics list failed: {error}")))?;
            write_rows(stdout, json_output, store.path(), rows)?;
        }
        MetricsCommand::Summary(args) => {
            let rows = store
                .summary(args.project.as_deref())
                .map_err(|error| CliFailure::new(1, format!("metrics summary failed: {error}")))?;
            write_summary(stdout, json_output, store.path(), rows)?;
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
                .compare(&args.project, split_days_ago)
                .map_err(|error| CliFailure::new(1, format!("metrics compare failed: {error}")))?;
            write_findings(
                stdout,
                json_output,
                store.path(),
                args.project,
                None,
                findings,
            )?;
        }
        MetricsCommand::Watch(args) => {
            let since_days = parse_days(&args.since)?;
            let findings = store
                .watch(&args.project, since_days)
                .map_err(|error| CliFailure::new(1, format!("metrics watch failed: {error}")))?;
            write_findings(
                stdout,
                json_output,
                store.path(),
                args.project,
                None,
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
                args.project,
                args.profile,
                findings,
            )?;
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
        .filter_map(|run| run.get("id").and_then(Value::as_i64))
        .collect::<Vec<_>>();
    let project = args.project.clone().unwrap_or_else(|| {
        args.repo
            .rsplit('/')
            .next()
            .unwrap_or(&args.repo)
            .to_owned()
    });
    let mut imported = 0;
    for run_id in run_ids {
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
            let input = github_job_to_record(&args.repo, args.workflow.as_deref(), &project, &job);
            store.record(&input).map_err(|error| {
                CliFailure::new(1, format!("GitHub metrics record failed: {error}"))
            })?;
            imported += 1;
        }
    }
    Ok(imported)
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
    let output = Command::new("gh")
        .args(args)
        .output()
        .map_err(|error| CliFailure::new(1, format!("gh spawn failed: {error}")))?;
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
    rows: Vec<MetricsSummaryRow>,
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
        "project\ttarget\tbackend\thost\tprovider\tcount\tfail_rate\tp50_ms\tp90_ms"
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
    project: String,
    profile: Option<String>,
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
                findings,
            },
            String::new,
        );
    }
    if findings.is_empty() {
        writeln!(stdout, "No material findings.").map_err(io_error)?;
        return Ok(());
    }
    for finding in findings {
        writeln!(
            stdout,
            "{}\t{}\t{}\t{}",
            finding.severity, finding.lane, finding.signal, finding.message
        )
        .map_err(io_error)?;
    }
    Ok(())
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
}
