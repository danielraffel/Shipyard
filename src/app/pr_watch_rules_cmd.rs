//! `shipyard pr-watch rules` and `rules-replay`: CLI adapter over
//! [`crate::pr_watch::rules`].

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{DateTime, Duration, Utc};

use super::pr_watch_cmd::{GITHUB_READ_TIMEOUT, actions_for, cache_for, io_failure, parse_until};
use crate::app::cli::{PrWatchRulesArgs, PrWatchRulesReplayArgs, PrWatchRulesSource};
use crate::app::{CliFailure, WAIT_EXIT_INVALID};
use crate::config::LoadedConfig;
use crate::gate_cost::ReadCache;
use crate::paths::RuntimePaths;
use crate::pr_watch::fixtures::{FixtureReader, Recorder};
use crate::pr_watch::rules::{self, RuleConfig, RuleEvent, WatchInput, read};
use crate::pr_watch::scan::WatchConfig;
use crate::pr_watch::{WatchQuery, gather, parse_duration};

/// One `--repo` value: `OWNER/REPO` or `OWNER/REPO@workflow.yml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RepoSpec {
    pub(super) repo: String,
    pub(super) workflow: String,
}

pub(super) fn parse_repo_spec(text: &str, default_workflow: &str) -> Result<RepoSpec, String> {
    let (repo, workflow) = text
        .split_once('@')
        .map_or((text, default_workflow), |(repo, workflow)| {
            (repo, workflow)
        });
    let valid = repo
        .split_once('/')
        .is_some_and(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'));
    if !valid || workflow.is_empty() {
        return Err(format!(
            "--repo {text:?} must look like OWNER/REPO or OWNER/REPO@workflow.yml"
        ));
    }
    Ok(RepoSpec {
        repo: repo.to_owned(),
        workflow: workflow.to_owned(),
    })
}

fn invalid(error: String) -> CliFailure {
    CliFailure::new(WAIT_EXIT_INVALID, error)
}

fn rule_config(source: &PrWatchRulesSource) -> Result<RuleConfig, CliFailure> {
    Ok(RuleConfig {
        head_unreviewed_after: source
            .head_unreviewed_after
            .as_deref()
            .map(parse_duration)
            .transpose()
            .map_err(invalid)?,
        queue_stuck_after: parse_duration(&source.queue_stuck_after).map_err(invalid)?,
        approved_unarmed_after: parse_duration(&source.approved_unarmed_after).map_err(invalid)?,
    })
}

/// Where answers come from: GitHub (optionally recorded) or a recording.
enum Source<'a> {
    Live {
        cwd: &'a Path,
        config: &'a LoadedConfig,
        runtime_paths: &'a RuntimePaths,
        no_cache: bool,
        record: Option<&'a Path>,
    },
    Recorded(&'a Path),
}

fn slug_dir(root: &Path, repo: &str) -> PathBuf {
    root.join(repo.replace('/', "__"))
}

/// Run `read` with a GitHub reader bound to `repo`.
fn with_reader<T>(
    source: &Source<'_>,
    repo: &str,
    read: impl FnOnce(&crate::gate_cost::SyncGhReader<'_>, &ReadCache) -> Result<T, String>,
) -> Result<T, String> {
    match source {
        Source::Recorded(root) => {
            let fixtures = FixtureReader::load(&slug_dir(root, repo))?;
            let reader = |argv: &[String]| fixtures.read(argv);
            read(&reader, &ReadCache::disabled())
        }
        Source::Live {
            cwd,
            config,
            runtime_paths,
            no_cache,
            record,
        } => {
            let actions = actions_for(cwd, config, repo, true);
            let recorder = record
                .map(|root| Recorder::new(&slug_dir(root, repo)))
                .transpose()?;
            let binding = repo.to_owned();
            let reader = |argv: &[String]| {
                let answer = actions
                    .run_gh_with_timeout_env(
                        argv,
                        GITHUB_READ_TIMEOUT,
                        &[("GH_REPO", binding.as_str())],
                    )
                    .map_err(|error| error.to_string())?;
                if let Some(recorder) = &recorder {
                    recorder.record(argv, &answer);
                }
                Ok(answer)
            };
            // A recording must hold every answer, so it never reads the cache.
            let cache = cache_for(runtime_paths, repo, *no_cache || record.is_some());
            read(&reader, &cache)
        }
    }
}

/// Read everything the rules need for `from..to`.
fn read_input(
    source: &Source<'_>,
    args: &PrWatchRulesSource,
    watch: &WatchConfig,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<WatchInput, CliFailure> {
    let mut input = WatchInput::default();
    for text in &args.repos {
        let spec = parse_repo_spec(text, &watch.workflow).map_err(invalid)?;
        let query = WatchQuery {
            repo: spec.repo.clone(),
            base: watch.base.clone(),
            workflow: spec.workflow.clone(),
            required_checks: Vec::new(),
            from,
            to,
            required_checks_optional: true,
        };
        let (history, trails) = with_reader(source, &spec.repo, |gh, cache| {
            let history = gather(gh, cache, &query, &watch.thresholds)?;
            let numbers: BTreeSet<u64> = history
                .prs
                .values()
                .filter(|pr| pr.closed_at.is_none_or(|closed| closed >= from))
                .map(|pr| pr.number)
                .collect();
            let trails = read::read_trails(gh, &spec.repo, &numbers, from)?;
            Ok((history, trails))
        })
        .map_err(|error| CliFailure::new(1, format!("{}: {error}", spec.repo)))?;
        for gap in &history.gaps {
            eprintln!("warning: {}: {gap}", spec.repo);
        }
        input.trails.insert(spec.repo.clone(), trails);
        input.repos.push(history);
    }
    if let Some(repo) = &args.alarm_repo {
        input.alarms = with_reader(source, repo, |gh, _| read::read_alarms(gh, repo, from))
            .map_err(|error| CliFailure::new(1, format!("{repo}: {error}")))?;
    }
    for path in &args.input_events {
        input
            .inputs
            .extend(read::read_inputs(path).map_err(invalid)?);
    }
    Ok(input)
}

fn write_events<W: Write>(stdout: &mut W, events: &[RuleEvent]) -> Result<(), CliFailure> {
    for event in events {
        let line = serde_json::to_string(event).map_err(io_failure)?;
        writeln!(stdout, "{line}").map_err(io_failure)?;
    }
    Ok(())
}

/// `pr-watch rules`: one live pass.
pub(super) fn rules_command<W: Write>(
    args: &PrWatchRulesArgs,
    watch: &WatchConfig,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let rule_config = rule_config(&args.source)?;
    let now = Utc::now();
    let from = now - parse_duration(&args.lookback).map_err(invalid)?;
    let source = Source::Live {
        cwd,
        config,
        runtime_paths,
        no_cache: args.source.no_cache,
        record: None,
    };
    let input = read_input(&source, &args.source, watch, from, now)?;
    let repos: Vec<String> = input
        .repos
        .iter()
        .map(|history| history.repo.clone())
        .collect();
    let path = args
        .state_file
        .clone()
        .unwrap_or_else(|| rules::ledger_path(&runtime_paths.state_dir, &repos));
    let ledger = rules::load(&path).map_err(|error| CliFailure::new(1, error))?;
    let (ledger, outcome) = rules::pass(&input, ledger, now, &rule_config);
    if !args.dry_run {
        rules::save(&path, &ledger).map_err(|error| CliFailure::new(1, error))?;
    }
    write_events(stdout, &outcome.events)?;
    if outcome.primed {
        eprintln!(
            "pr-watch rules: no ledger at {}; recorded {} current condition(s) without emitting",
            path.display(),
            ledger.seen.len()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `pr-watch rules-replay`.
pub(super) fn rules_replay_command<W: Write>(
    args: &PrWatchRulesReplayArgs,
    watch: &WatchConfig,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let rule_config = rule_config(&args.source)?;
    let to = parse_until(args.until.as_deref())?;
    let start = to - parse_duration(&args.since).map_err(invalid)?;
    let from = start - parse_duration(&args.lookback).map_err(invalid)?;
    let tick = parse_duration(&args.tick).map_err(invalid)?;
    if tick <= Duration::zero() {
        return Err(invalid("--tick must be positive".to_owned()));
    }
    let source = match &args.fixtures {
        Some(dir) => Source::Recorded(dir),
        None => Source::Live {
            cwd,
            config,
            runtime_paths,
            no_cache: args.source.no_cache,
            record: args.record.as_deref(),
        },
    };
    let input = read_input(&source, &args.source, watch, from, to)?;
    let events = rules::replay(&input, start, to, tick, &rule_config);
    match &args.out {
        Some(path) => {
            let mut file = std::fs::File::create(path).map_err(io_failure)?;
            write_events(&mut file, &events)?;
        }
        None => write_events(stdout, &events)?,
    }
    eprintln!(
        "pr-watch rules-replay {}..{} every {}m: {} event(s): {}",
        start.format("%Y-%m-%dT%H:%MZ"),
        to.format("%Y-%m-%dT%H:%MZ"),
        tick.num_minutes(),
        events.len(),
        rules::tally(&events)
    );
    Ok(ExitCode::SUCCESS)
}
