use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};

use crate::app::cli::MetricsGateCostArgs;
use crate::app::{CliFailure, WAIT_EXIT_INVALID};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::gate_cost::{self, GateCostQuery, GateCostReport, GateProxies, ReadCache, scorecard};
use crate::identity::RuntimeMode;
use crate::output::write_pretty_json;

/// Per-request bound. Observation must never strand the invoking agent.
const GITHUB_READ_TIMEOUT: Duration = Duration::from_mins(1);
const CONFIG_SECTION: &str = "metrics.gate_cost";

pub(super) fn gate_cost_command<W: Write>(
    args: MetricsGateCostArgs,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json_output: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let now = Utc::now();
    let no_cache = args.no_cache;
    let scorecard = args.scorecard;
    let flakes = args.flakes;
    let causes = args.causes;
    let causes_max_jobs = args.causes_max_jobs;
    let required_contexts = args.required_contexts.clone();
    let selection_steps = args.selection_steps.clone();
    let export_cases = args.export_cases.clone();
    let query = resolve_query(args, &config, now)?;
    let cache = if no_cache {
        ReadCache::disabled()
    } else {
        ReadCache::open(&cache_dir(state_dir, &query.repo), SystemTime::now())
    };
    let actions = GitHubActions::from_loaded_config(cwd, &config);
    let reader = |gh_args: &[String]| {
        actions
            .run_gh_with_timeout(gh_args, GITHUB_READ_TIMEOUT)
            .map_err(|error| error.to_string())
    };
    if scorecard {
        let query = scorecard::ScorecardQuery {
            repo: query.repo,
            workflow: query.workflow,
            gate_job: query.gate_job,
            base_branch: query.base_branch,
            required_contexts,
            selection_steps,
            from: query.from,
            to: query.to,
        };
        let observation = scorecard::gather(&reader, &query, &cache)
            .map_err(|error| CliFailure::new(1, format!("scorecard read failed: {error}")))?;
        let mut report = scorecard::compute(&observation);
        let mut rows = scorecard::cases::cases(&observation);
        if causes || export_cases.is_some() {
            report.cause_naming = Some(read_causes(
                &reader,
                &cache,
                &observation,
                &mut rows,
                causes_max_jobs,
            ));
        }
        if let Some(path) = &export_cases {
            let receipt = write_cases(path, &observation, &rows)?;
            eprintln!(
                "export-cases: {} row(s) to {}, sha256 {} (receipt {})",
                receipt.rows,
                path.display(),
                receipt.sha256,
                receipt_path(path).display()
            );
            if !receipt.unread_kinds.is_empty() {
                eprintln!(
                    "export-cases: not read, so their count is unknown, not zero: {} \
                     (required contexts {})",
                    receipt.unread_kinds.join(", "),
                    receipt.required_source
                );
            }
        }
        if flakes {
            let read = scorecard::flakes::gather(&reader, &cache, &query)
                .map_err(|error| CliFailure::new(1, format!("flake read failed: {error}")))?;
            report.same_sha_flakes = Some(scorecard::flakes::compute(&query.repo, &read));
        }
        if json_output {
            write_pretty_json(stdout, &report)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        } else {
            write!(stdout, "{}", scorecard::render(&report))
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            if let Some(flakes) = &report.same_sha_flakes {
                write!(stdout, "{}", scorecard::flakes::render(flakes))
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
            if let Some(naming) = &report.cause_naming {
                write!(stdout, "{}", scorecard::causes::render(naming))
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
        }
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let observation = gate_cost::gather_cached(&reader, &query, now, &cache)
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

/// Read the red cases' causes, put them on `rows`, and score them.
fn read_causes(
    reader: &gate_cost::SyncGhReader<'_>,
    cache: &ReadCache,
    observation: &scorecard::ScorecardObservation,
    rows: &mut [scorecard::cases::Case],
    max_jobs: usize,
) -> scorecard::causes::CauseNaming {
    let jobs = scorecard::causes::red_jobs(observation);
    let read = scorecard::causes::gather(reader, cache, &observation.query.repo, &jobs, max_jobs);
    for unreadable in &read.unreadable {
        eprintln!(
            "causes: job {} unreadable: {}",
            unreadable.job_id, unreadable.reason
        );
    }
    scorecard::causes::attach(rows, &read);
    scorecard::causes::compute(rows, &read)
}

fn receipt_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".receipt.json");
    PathBuf::from(name)
}

/// Write the scorecard's cases and their receipt, each through a temporary
/// file renamed into place, so a reader never sees half a file. The receipt
/// is written last and is the commit marker: a crash between the two renames
/// leaves new rows beside an old receipt, whose row count and SHA-256 then
/// disagree with the file. Each temporary name is unique to the run, so two
/// exports to the same path never write the same temporary file.
fn write_cases(
    path: &Path,
    observation: &scorecard::ScorecardObservation,
    rows: &[scorecard::cases::Case],
) -> Result<scorecard::cases::Receipt, CliFailure> {
    let (bytes, receipt) = scorecard::cases::export(observation, rows)
        .map_err(|error| CliFailure::new(1, format!("export-cases: {error}")))?;
    let receipt_json = serde_json::to_vec_pretty(&receipt)
        .map_err(|error| CliFailure::new(1, format!("export-cases: {error}")))?;
    for (target, content) in [
        (path.to_path_buf(), bytes),
        (receipt_path(path), receipt_json),
    ] {
        replace_file(&target, &content).map_err(|error| {
            CliFailure::new(
                1,
                format!("export-cases: cannot write {}: {error}", target.display()),
            )
        })?;
    }
    Ok(receipt)
}

/// Write `content` to a temporary file unique to this call, beside `target`,
/// and rename it over `target`.
fn replace_file(target: &Path, content: &[u8]) -> std::io::Result<()> {
    let directory = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let prefix = target
        .file_name()
        .map_or_else(|| "export".into(), |name| name.to_string_lossy());
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{prefix}."))
        .suffix(".tmp")
        .tempfile_in(directory)?;
    std::io::Write::write_all(&mut temporary, content)?;
    temporary.persist(target).map_err(|error| error.error)?;
    Ok(())
}

/// One cache directory per repository, under Shipyard's state directory.
fn cache_dir(state_dir: &Path, repo: &str) -> PathBuf {
    state_dir
        .join("metrics")
        .join("gate-cost-cache")
        .join(repo.replace('/', "__").to_ascii_lowercase())
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

fn sample_note(evidence: &gate_cost::proxy::Evidence) -> String {
    if evidence.sufficient {
        format!("n={}", evidence.sample)
    } else {
        format!(
            "n={}, insufficient sample (needs {})",
            evidence.sample, evidence.min_sample
        )
    }
}

fn render_proxies(out: &mut String, proxies: &GateProxies) {
    let _ = writeln!(out, "proxies ({}):", proxies.basis);
    let runs = &proxies.runs_per_merged_pr;
    let _ = writeln!(
        out,
        "  gate runs per merged PR: {} PR-head, {} merge-group, {} wasted attempts ({} merged PRs)",
        opt(runs.pr_head_per_merged_pr),
        opt(runs.merge_group_per_merged_pr),
        opt(runs.wasted_per_merged_pr),
        sample_note(&runs.evidence),
    );
    let starvation = &proxies.starvation;
    let _ = writeln!(
        out,
        "  starvation: {} of {} gate attempts cancelled before a runner was assigned, {} of them withdrawn by a push ({}; {})",
        starvation.cancelled_before_runner,
        starvation.gate_attempts,
        starvation.superseded_by_push,
        opt(starvation.share),
        sample_note(&starvation.evidence),
    );
    let placement = &proxies.placement;
    let unserved: Vec<String> = placement
        .unserved_label_sets
        .iter()
        .map(|labels| format!("[{}]", labels.join(",")))
        .collect();
    let _ = writeln!(
        out,
        "  placement: {} of {} jobs on label sets nothing serves {}; placement-correct {} (census {}; {})",
        placement.unserved_label_jobs,
        placement.jobs,
        unserved.join(" "),
        opt(placement.placement_correct_share),
        placement.census,
        sample_note(&placement.evidence),
    );
    let wait = &proxies.queue_wait_per_job_ahead;
    let _ = writeln!(
        out,
        "  queue wait per job ahead: median {}s (raw median wait {}s is load-dependent; {})",
        opt(wait.median_seconds),
        opt(wait.raw_median_wait_seconds),
        sample_note(&wait.evidence),
    );
    let queue = &proxies.merge_queue;
    let causes: Vec<String> = queue
        .ejections_by_cause
        .iter()
        .map(|(cause, count)| format!("{cause}={count}"))
        .collect();
    let _ = writeln!(
        out,
        "  merge queue: {} attempts per merged PR; {} ejection(s) [{}] ({})",
        opt(queue.attempts_per_merged_pr),
        queue.ejections,
        causes.join(" "),
        sample_note(&queue.evidence),
    );
    let pushes = &proxies.push_cancellations;
    let _ = writeln!(
        out,
        "  push cancellations: {} of {} cancelled PR-head runs superseded by a push ({} of PR-head runs; {})",
        pushes.superseded_by_push,
        pushes.cancelled_pr_head_runs,
        opt(pushes.share_of_pr_head_runs),
        sample_note(&pushes.evidence),
    );
}

fn render(report: &GateCostReport) -> String {
    let mut out = format!(
        "{} {} job `{}` into {}, {} .. {}\n",
        report.repo, report.workflow, report.gate_job, report.base_branch, report.from, report.to
    );
    render_proxies(&mut out, &report.proxies);
    let _ = writeln!(
        out,
        "context (load-dependent): gate minutes track host load; judge changes by the proxies above"
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
    let _ = writeln!(
        out,
        "reads: {} sent to GitHub, {} served from the settled-answer cache",
        report.reads.github, report.reads.cached,
    );
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::gate_cost::{GateCostObservation, compute};

    /// The export and its receipt land next to each other, whole, and the
    /// receipt's hash is the file's.
    #[test]
    fn cases_and_their_receipt_are_written_together() {
        use sha2::Digest;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("week.jsonl");
        let observation = scorecard::ScorecardObservation {
            query: scorecard::ScorecardQuery {
                repo: "o/r".to_owned(),
                workflow: "build.yml".to_owned(),
                gate_job: "macos".to_owned(),
                base_branch: "main".to_owned(),
                required_contexts: Vec::new(),
                selection_steps: Vec::new(),
                from: Utc::now() - chrono::Duration::days(7),
                to: Utc::now(),
            },
            required: Ok((Vec::new(), "--required-context".to_owned())),
            prs: Vec::new(),
            merge_group_runs: Vec::new(),
            queue_events: Vec::new(),
            selection: None,
            gaps: Vec::new(),
        };
        let rows = scorecard::cases::cases(&observation);
        let receipt = write_cases(&path, &observation, &rows).expect("written");
        let bytes = std::fs::read(&path).expect("export");
        assert_eq!(receipt.rows, 0);
        assert_eq!(receipt.sha256, hex::encode(sha2::Sha256::digest(&bytes)));
        let saved: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("week.jsonl.receipt.json")).expect("receipt"),
        )
        .expect("json");
        assert_eq!(saved["sha256"], receipt.sha256);
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert!(
            !names
                .iter()
                .any(|name| Path::new(name).extension().is_some_and(|ext| ext == "tmp")),
            "{names:?}"
        );
    }

    /// The receipt is written last, so it is the commit marker: when the
    /// receipt cannot be written, the cases file already holds the new rows
    /// and the stale or missing receipt exposes the interrupted export.
    #[test]
    fn the_receipt_is_written_after_the_cases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("week.jsonl");
        std::fs::write(&path, b"old rows\n").expect("old export");
        std::fs::create_dir(dir.path().join("week.jsonl.receipt.json")).expect("blocker");
        let observation = scorecard::ScorecardObservation {
            query: scorecard::ScorecardQuery {
                repo: "o/r".to_owned(),
                workflow: "build.yml".to_owned(),
                gate_job: "macos".to_owned(),
                base_branch: "main".to_owned(),
                required_contexts: Vec::new(),
                selection_steps: Vec::new(),
                from: Utc::now() - chrono::Duration::days(7),
                to: Utc::now(),
            },
            required: Ok((Vec::new(), "--required-context".to_owned())),
            prs: Vec::new(),
            merge_group_runs: Vec::new(),
            queue_events: Vec::new(),
            selection: None,
            gaps: Vec::new(),
        };
        let failure =
            write_cases(&path, &observation, &[]).expect_err("the receipt cannot be written");
        assert!(
            failure.message.contains("receipt.json"),
            "{}",
            failure.message
        );
        assert_eq!(
            std::fs::read(&path).expect("cases"),
            b"",
            "the cases were written before the receipt"
        );
    }

    /// A temporary file another run left (or is writing) at a fixed name does
    /// not block or corrupt this run's write: each run's temporary is its own.
    #[test]
    fn a_temporary_at_the_old_fixed_name_does_not_block_the_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("week.jsonl");
        std::fs::create_dir(dir.path().join("week.jsonl.tmp")).expect("blocker");
        replace_file(&target, b"rows\n").expect("written past the blocker");
        assert_eq!(std::fs::read(&target).expect("read"), b"rows\n");
    }

    /// Concurrent exports to one path each land whole: the file is exactly one
    /// run's bytes, never a mix. Windows refuses to replace a file another
    /// rename is replacing at that moment, so there a run may fail outright,
    /// but never leaves a mix or a stray temporary.
    #[test]
    fn concurrent_writes_to_one_path_each_land_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("week.jsonl");
        let contents: Vec<Vec<u8>> = (0..8u8)
            .map(|index| vec![b'a' + index; 256 * 1024])
            .collect();
        let written = std::thread::scope(|scope| {
            let handles: Vec<_> = contents
                .iter()
                .map(|content| {
                    let target = &target;
                    scope.spawn(move || replace_file(target, content).is_ok())
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("joined"))
                .filter(|ok| *ok)
                .count()
        });
        if cfg!(windows) {
            assert!(written >= 1, "no run landed");
        } else {
            assert_eq!(
                written,
                contents.len(),
                "every run lands where renames replace"
            );
        }
        let landed = std::fs::read(&target).expect("read");
        assert!(contents.contains(&landed), "a mix of runs landed");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .map(|entry| entry.expect("entry").file_name())
            .filter(|name| name != "week.jsonl")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

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
            run_meta: BTreeMap::new(),
            placement_jobs: Vec::new(),
            runner_census: Err("not read".to_owned()),
            reads: gate_cost::ReadStats::default(),
            listing_gaps: Vec::new(),
            gate_absent: BTreeMap::new(),
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
        let proxy_line = text
            .lines()
            .position(|line| line.starts_with("proxies ("))
            .expect("proxy block");
        let minutes_line = text
            .lines()
            .position(|line| line.starts_with("gate-minutes per merged PR"))
            .expect("minutes line");
        assert!(proxy_line < minutes_line, "proxies must lead: {text}");
        for prefix in [
            "  starvation",
            "  placement",
            "  queue wait per job ahead",
            "  merge queue",
            "  push cancellations",
            "context (load-dependent)",
            "gate-minutes per merged PR",
            "  gate runs per merged PR",
            "  PR head",
            "  merge group",
            "batch fullness",
            "receipt reuse",
            "queue depth now",
            "gap queue_depth_history",
            "reads",
        ] {
            assert!(
                starts.contains(&prefix),
                "missing line {prefix:?} in {text}"
            );
        }
    }
}
