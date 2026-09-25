use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::app::cli::MetricsGateCostArgs;
use crate::app::{CliFailure, WAIT_EXIT_INVALID};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::gate_cost::{self, GateCostQuery, GateCostReport};
use crate::identity::RuntimeMode;
use crate::output::write_pretty_json;

/// Per-request bound. Observation must never strand the invoking agent.
const GITHUB_READ_TIMEOUT: Duration = Duration::from_secs(60);
const CONFIG_SECTION: &str = "metrics.gate_cost";

pub(super) fn gate_cost_command<W: Write>(
    args: MetricsGateCostArgs,
    mode: RuntimeMode,
    cwd: &Path,
    json_output: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let now = Utc::now();
    let query = resolve_query(args, &config, now)?;
    let actions = GitHubActions::from_loaded_config(cwd, &config);
    let reader = |gh_args: &[String]| {
        actions
            .run_gh_with_timeout(gh_args, GITHUB_READ_TIMEOUT)
            .map_err(|error| error.to_string())
    };
    let observation = gate_cost::gather(&reader, &query, now)
        .map_err(|error| CliFailure::new(1, format!("gate-cost read failed: {error}")))?;
    let report = gate_cost::compute(&observation);
    if json_output {
        write_pretty_json(stdout, &report)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        write!(stdout, "{}", render(&report))
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn config_str(config: &LoadedConfig, key: &str) -> Option<String> {
    config
        .get_str(&format!("{CONFIG_SECTION}.{key}"))
        .map(str::to_owned)
}

fn required(value: Option<String>, flag: &str, key: &str) -> Result<String, CliFailure> {
    value.filter(|text| !text.is_empty()).ok_or_else(|| {
        CliFailure::new(
            WAIT_EXIT_INVALID,
            format!("gate-cost needs --{flag} (or `{key}` in [{CONFIG_SECTION}])"),
        )
    })
}

fn parse_time(text: &str, flag: &str) -> Result<DateTime<Utc>, CliFailure> {
    DateTime::parse_from_rfc3339(text)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| {
            CliFailure::new(
                WAIT_EXIT_INVALID,
                format!("invalid --{flag} {text:?}: {error}"),
            )
        })
}

fn resolve_query(
    args: MetricsGateCostArgs,
    config: &LoadedConfig,
    now: DateTime<Utc>,
) -> Result<GateCostQuery, CliFailure> {
    let repo = required(
        args.repo.or_else(|| config_str(config, "repo")),
        "repo",
        "repo",
    )?;
    let workflow = required(
        args.workflow.or_else(|| config_str(config, "workflow")),
        "workflow",
        "workflow",
    )?;
    let gate_job = required(
        args.gate_job.or_else(|| config_str(config, "gate_job")),
        "gate-job",
        "gate_job",
    )?;
    let base_branch = args
        .base
        .or_else(|| config_str(config, "base_branch"))
        .unwrap_or_else(|| "main".to_owned());
    let mut events = args.events;
    if events.is_empty() {
        events = config
            .get(&format!("{CONFIG_SECTION}.events"))
            .and_then(toml::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
    }
    if events.is_empty() {
        events = vec![
            "pull_request".to_owned(),
            gate_cost::MERGE_GROUP_EVENT.to_owned(),
        ];
    }
    let receipt_job = args
        .receipt_job
        .or_else(|| config_str(config, "receipt_job"));
    let receipt_target = args
        .receipt_target
        .or_else(|| config_str(config, "receipt_target"))
        .unwrap_or_else(|| gate_job.clone());
    let to = args
        .to
        .as_deref()
        .map(|text| parse_time(text, "to"))
        .transpose()?
        .unwrap_or(now);
    let from = match args.from.as_deref() {
        Some(text) => parse_time(text, "from")?,
        None => {
            to - gate_cost::parse_window(&args.since)
                .map_err(|error| CliFailure::new(WAIT_EXIT_INVALID, error))?
        }
    };
    if from >= to {
        return Err(CliFailure::new(
            WAIT_EXIT_INVALID,
            "gate-cost window is empty: --from must be before --to",
        ));
    }
    Ok(GateCostQuery {
        repo,
        workflow,
        gate_job,
        base_branch,
        events,
        receipt_job,
        receipt_target,
        from,
        to,
    })
}

fn opt(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| format!("{value:.2}"))
}

fn render(report: &GateCostReport) -> String {
    let mut out = format!(
        "{} {} job `{}` into {}, {} .. {}\n",
        report.repo, report.workflow, report.gate_job, report.base_branch, report.from, report.to
    );
    let _ = writeln!(
        out,
        "gate-minutes per merged PR: {} ({:.2} gate-min / {} merged PRs; {:.2} wasted)",
        opt(report.gate_minutes_per_merged_pr),
        report.gate_minutes,
        report
            .merged_prs
            .map_or_else(|| "n/a".to_owned(), |count| count.to_string()),
        report.wasted_gate_minutes,
    );
    let _ = writeln!(
        out,
        "  gate runs per merged PR: {} PR-head, {} merge-group",
        opt(report.pr_head_runs_per_merged_pr),
        opt(report.merge_group_runs_per_merged_pr),
    );
    for (label, lane) in [
        ("PR head", &report.pr_head),
        ("merge group", &report.merge_group),
    ] {
        let _ = writeln!(
            out,
            "  {label}: {} runs, {} jobs ran, {:.2} min, median {} (p25 {}, p75 {})",
            lane.runs,
            lane.jobs_ran,
            lane.gate_minutes,
            opt(lane.median_minutes),
            opt(lane.p25_minutes),
            opt(lane.p75_minutes),
        );
    }
    let batches = &report.batches;
    let distribution: Vec<String> = batches
        .distribution
        .iter()
        .map(|(size, count)| format!("{size}x{count}"))
        .collect();
    let _ = writeln!(
        out,
        "batch fullness: mean {} PRs/batch of max {} ({}), {} batches [{}]",
        opt(batches.mean_prs_per_batch),
        batches
            .max_entries_to_merge
            .map_or_else(|| "n/a".to_owned(), |max| max.to_string()),
        opt(batches.mean_fullness),
        batches.batches,
        distribution.join(" "),
    );
    let _ = writeln!(
        out,
        "receipt reuse: {} of {} merge-group runs ({}) for `{}`; {} refused, {} no decision",
        report.reuse.reused,
        report.reuse.merge_group_runs,
        opt(report.reuse.rate),
        report.reuse.target,
        report.reuse.refused,
        report.reuse.no_decision + report.reuse.unreadable,
    );
    if let Some(depth) = report.current_queue_depth {
        let _ = writeln!(out, "queue depth now: {depth}");
    }
    for gap in &report.telemetry_gaps {
        let _ = writeln!(out, "gap {}: {}", gap.signal, gap.reason);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::gate_cost::{GateCostObservation, compute};

    #[test]
    fn human_output_puts_each_signal_on_its_own_line() {
        let now = Utc::now();
        let report = compute(&GateCostObservation {
            query: GateCostQuery {
                repo: "o/r".to_owned(),
                workflow: "build.yml".to_owned(),
                gate_job: "macos".to_owned(),
                base_branch: "main".to_owned(),
                events: vec!["merge_group".to_owned()],
                receipt_job: None,
                receipt_target: "macos".to_owned(),
                from: now - chrono::Duration::hours(1),
                to: now,
            },
            runs_by_event: BTreeMap::new(),
            gate_jobs: Vec::new(),
            merged_prs: Ok(0),
            batches: Ok(Vec::new()),
            max_entries_to_merge: Some(5),
            max_entries_to_build: None,
            merge_method: None,
            reuse: BTreeMap::new(),
            current_queue_depth: Ok(0),
            ruleset_error: None,
        });
        let text = render(&report);
        let starts: Vec<&str> = text
            .lines()
            .map(|line| line.split(':').next().unwrap_or(""))
            .collect();
        assert!(
            text.lines()
                .next()
                .unwrap_or("")
                .starts_with("o/r build.yml")
        );
        for prefix in [
            "gate-minutes per merged PR",
            "  gate runs per merged PR",
            "  PR head",
            "  merge group",
            "batch fullness",
            "receipt reuse",
            "queue depth now",
            "gap queue_depth_history",
        ] {
            assert!(
                starts.contains(&prefix),
                "missing line {prefix:?} in {text}"
            );
        }
    }
}
