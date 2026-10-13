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
        reviewers: source
            .reviewers
            .iter()
            .map(|text| {
                let (repo, name) = text
                    .split_once('=')
                    .filter(|(repo, name)| repo.contains('/') && !name.is_empty())
                    .ok_or_else(|| format!("--reviewer {text:?} must look like OWNER/REPO=NAME"))?;
                Ok((repo.to_ascii_lowercase(), name.to_owned()))
            })
            .collect::<Result<_, String>>()
            .map_err(invalid)?,
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
    /// A scripted GitHub, for tests.
    #[cfg(test)]
    Scripted(&'a (dyn Fn(&[String]) -> Result<String, String> + Sync)),
}

fn slug_dir(root: &Path, repo: &str) -> PathBuf {
    root.join(repo.replace('/', "__"))
}

/// Run `read` with a GitHub reader bound to `repo`.
/// GitHub reads made per repository, for the pass summary.
#[derive(Debug, Default)]
pub(super) struct ReadTally(std::sync::Mutex<std::collections::BTreeMap<String, u64>>);

impl ReadTally {
    fn add(&self, repo: &str) {
        if let Ok(mut counts) = self.0.lock() {
            *counts.entry(repo.to_owned()).or_default() += 1;
        }
    }

    /// `N GitHub reads (repo: n, ...)`.
    pub(super) fn line(&self) -> String {
        let counts = self
            .0
            .lock()
            .map(|counts| counts.clone())
            .unwrap_or_default();
        let total: u64 = counts.values().sum();
        let parts: Vec<String> = counts
            .iter()
            .map(|(repo, count)| format!("{repo}: {count}"))
            .collect();
        format!("{total} GitHub reads ({})", parts.join(", "))
    }
}

fn with_reader<T>(
    source: &Source<'_>,
    repo: &str,
    tally: &ReadTally,
    read: impl FnOnce(&crate::gate_cost::SyncGhReader<'_>, &ReadCache) -> Result<T, String>,
) -> Result<T, String> {
    match source {
        Source::Recorded(root) => {
            let fixtures = FixtureReader::load(&slug_dir(root, repo))?;
            let reader = |argv: &[String]| {
                tally.add(repo);
                fixtures.read(argv)
            };
            read(&reader, &ReadCache::disabled())
        }
        #[cfg(test)]
        Source::Scripted(gh) => {
            let reader = |argv: &[String]| {
                tally.add(repo);
                gh(argv)
            };
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
                tally.add(repo);
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
    tally: &ReadTally,
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
        let (history, trails) = with_reader(source, &spec.repo, tally, |gh, cache| {
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
        .map_err(|error| read_failure(&spec.repo, &error))?;
        for gap in &history.gaps {
            eprintln!("warning: {}: {gap}", spec.repo);
        }
        input.trails.insert(spec.repo.clone(), trails);
        input.repos.push(history);
    }
    if let Some(repo) = &args.alarm_repo {
        input.alarms = with_reader(source, repo, tally, |gh, _| {
            read::read_alarms(gh, repo, from)
        })
        .map_err(|error| read_failure(repo, &error))?;
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

/// Exit status of a pass skipped to protect the GitHub App's budget.
const EXIT_RATE_LIMITED: u8 = 75;

fn read_failure(repo: &str, error: &str) -> CliFailure {
    if read::rate_limited(error) {
        CliFailure::new(
            EXIT_RATE_LIMITED,
            format!("{repo}: GitHub rate limit reached mid-pass ({error}); nothing recorded"),
        )
    } else {
        CliFailure::new(1, format!("{repo}: {error}"))
    }
}

/// When the watcher may read again, kept beside its ledger.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Backoff {
    until: DateTime<Utc>,
    reason: String,
}

fn backoff_path(ledger: &Path) -> PathBuf {
    ledger.with_extension("backoff.json")
}

fn read_backoff(path: &Path) -> Option<Backoff> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_backoff(path: &Path, backoff: &Backoff) {
    if let Ok(body) = serde_json::to_string_pretty(backoff) {
        let _ = std::fs::write(path, body);
    }
}

/// The first watched repository whose App budget is under `floor`.
fn budget_refusal(
    source: &Source<'_>,
    repos: &[RepoSpec],
    tally: &ReadTally,
    floor: u64,
    now: DateTime<Utc>,
) -> Result<Option<Backoff>, CliFailure> {
    for spec in repos {
        let budget = with_reader(source, &spec.repo, tally, |gh, _| {
            read::read_rate_budget(gh)
        })
        .map_err(|error| read_failure(&spec.repo, &error))?;
        if budget.remaining() < floor {
            let until = budget.reset.unwrap_or(now + Duration::minutes(15));
            return Ok(Some(Backoff {
                until,
                reason: format!(
                    "{}: App budget core {} / graphql {} is under {floor}; resets {}",
                    spec.repo,
                    budget.core,
                    budget.graphql,
                    until.format("%Y-%m-%dT%H:%M:%SZ")
                ),
            }));
        }
    }
    Ok(None)
}

/// Kinds whose verdict could be older than the window: read the pull
/// request's whole trail before emitting them.
const TRAIL_DEPENDENT: [&str; 3] = ["ready_unreviewed", "head_unreviewed", "armed_unapproved"];

/// Replace the trail of every pull request a trail-dependent event names
/// with its whole trail.
fn complete_trails(
    source: &Source<'_>,
    input: &mut WatchInput,
    events: &[RuleEvent],
    tally: &ReadTally,
) -> Result<usize, CliFailure> {
    let mut wanted: BTreeSet<(String, u64)> = BTreeSet::new();
    for event in events {
        if let (true, Some(repo), Some(pr)) = (
            TRAIL_DEPENDENT.contains(&event.kind.as_str()),
            event.repo.as_ref(),
            event.pr,
        ) {
            wanted.insert((repo.clone(), pr));
        }
    }
    for (repo, pr) in &wanted {
        let trail = with_reader(source, repo, tally, |gh, _| {
            read::read_pr_trail(gh, repo, *pr)
        })
        .map_err(|error| read_failure(repo, &error))?;
        input
            .trails
            .entry(repo.clone())
            .or_default()
            .insert(*pr, trail);
    }
    Ok(wanted.len())
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
    let specs = args
        .source
        .repos
        .iter()
        .map(|text| parse_repo_spec(text, &watch.workflow))
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    let repos: Vec<String> = specs.iter().map(|spec| spec.repo.clone()).collect();
    let path = args
        .state_file
        .clone()
        .unwrap_or_else(|| rules::ledger_path(&runtime_paths.state_dir, &repos));
    let backoff = backoff_path(&path);
    if let Some(waiting) = read_backoff(&backoff).filter(|waiting| waiting.until > now) {
        eprintln!(
            "pr-watch rules: skipped, nothing read: backing off until {} ({}). It resumes by \
             itself; to force a pass, delete {}",
            waiting.until.format("%Y-%m-%dT%H:%M:%SZ"),
            waiting.reason,
            backoff.display()
        );
        return Ok(ExitCode::from(EXIT_RATE_LIMITED));
    }
    let source = Source::Live {
        cwd,
        config,
        runtime_paths,
        no_cache: args.source.no_cache,
        record: None,
    };
    let tally = ReadTally::default();
    if let Some(refusal) = budget_refusal(&source, &specs, &tally, args.min_rate_remaining, now)? {
        eprintln!(
            "pr-watch rules: skipped, nothing read past the budget check: {}. It resumes after \
             the reset; lower --min-rate-remaining to read anyway",
            refusal.reason
        );
        write_backoff(&backoff, &refusal);
        return Ok(ExitCode::from(EXIT_RATE_LIMITED));
    }
    let outcome = live_pass(&source, args, watch, &rule_config, &tally, &path, from, now);
    if let Err(failure) = &outcome
        && failure.code == EXIT_RATE_LIMITED
    {
        write_backoff(
            &backoff,
            &Backoff {
                until: now + Duration::minutes(15),
                reason: failure.message().to_owned(),
            },
        );
    }
    let (ledger, outcome, completed) = outcome?;
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
    eprintln!(
        "pr-watch rules: {} event(s); {}; {completed} pull request trail(s) read in full",
        outcome.events.len(),
        tally.line()
    );
    Ok(ExitCode::SUCCESS)
}

/// Read, evaluate, and when an event would fire on a possibly partial trail,
/// re-read those pull requests' whole trails and evaluate again.
#[allow(clippy::too_many_arguments)] // One pass's inputs, named at its one call.
fn live_pass(
    source: &Source<'_>,
    args: &PrWatchRulesArgs,
    watch: &WatchConfig,
    rule_config: &RuleConfig,
    tally: &ReadTally,
    path: &Path,
    from: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(rules::RuleLedger, rules::PassOutcome, usize), CliFailure> {
    let mut input = read_input(source, &args.source, watch, tally, from, now)?;
    let ledger = rules::load(path).map_err(|error| CliFailure::new(1, error))?;
    let (first_ledger, first) = rules::pass(&input, ledger.clone(), now, rule_config);
    if first.primed || first.events.is_empty() {
        return Ok((first_ledger, first, 0));
    }
    let completed = complete_trails(source, &mut input, &first.events, tally)?;
    if completed == 0 {
        return Ok((first_ledger, first, 0));
    }
    let (ledger, outcome) = rules::pass(&input, ledger, now, rule_config);
    Ok((ledger, outcome, completed))
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
    let tally = ReadTally::default();
    let input = read_input(&source, &args.source, watch, &tally, from, to)?;
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
    eprintln!("pr-watch rules-replay: {}", tally.line());
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn spec(repo: &str) -> RepoSpec {
        RepoSpec {
            repo: repo.to_owned(),
            workflow: "ci.yml".to_owned(),
        }
    }

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 13, 2, 0, 0).unwrap()
    }

    fn budget(core: u64) -> String {
        json!({"resources": {"core": {"remaining": core, "reset": 1_791_860_000},
                             "graphql": {"remaining": 5000, "reset": 1_791_861_000}}})
        .to_string()
    }

    #[test]
    fn a_pass_backs_off_while_the_app_budget_is_low_and_counts_its_reads() {
        let low = |argv: &[String]| -> Result<String, String> {
            assert_eq!(argv, ["api", "rate_limit"]);
            Ok(budget(900))
        };
        let tally = ReadTally::default();
        let refusal = budget_refusal(&Source::Scripted(&low), &[spec("o/r")], &tally, 1500, at())
            .expect("readable")
            .expect("refused");
        assert!(refusal.reason.contains("core 900"), "{}", refusal.reason);
        assert_eq!(refusal.until.timestamp(), 1_791_860_000);
        assert_eq!(tally.line(), "1 GitHub reads (o/r: 1)");
        let plenty = |_: &[String]| -> Result<String, String> { Ok(budget(4000)) };
        assert!(
            budget_refusal(
                &Source::Scripted(&plenty),
                &[spec("o/r")],
                &ReadTally::default(),
                1500,
                at()
            )
            .expect("readable")
            .is_none()
        );
    }

    #[test]
    fn a_rate_limited_read_fails_with_the_backoff_status() {
        let failure = read_failure("o/r", "gh: API rate limit exceeded (HTTP 403)");
        assert_eq!(failure.code, EXIT_RATE_LIMITED);
        assert_eq!(read_failure("o/r", "HTTP 502").code, 1);
    }

    #[test]
    fn a_backoff_survives_until_it_expires() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = backoff_path(&dir.path().join("rules-o__r.json"));
        write_backoff(
            &path,
            &Backoff {
                until: at(),
                reason: "budget".to_owned(),
            },
        );
        let read = read_backoff(&path).expect("written");
        assert_eq!(read.until, at());
        assert_eq!(read.reason, "budget");
    }

    /// The watcher's window saw no verdict on the head, so it would report
    /// the arm unapproved; the pull request's whole trail holds an older
    /// approval of that head, so the event must not fire.
    #[test]
    fn an_old_verdict_on_a_current_head_silences_the_event() {
        let comments = json!([[{"id": 1, "user": {"login": "shipyard-local[bot]"},
            "created_at": "2026-09-01T00:00:00Z",
            "body": format!("verdict: APPROVE head:{HEAD}")}]]);
        let gh = move |argv: &[String]| -> Result<String, String> {
            let joined = argv.join(" ");
            if joined.contains("repos/o/r/issues/7/comments") {
                Ok(comments.to_string())
            } else if joined.contains("graphql") {
                Ok(json!({"data": {"repository": {"p7": {"reviews": {"nodes": []}}}}}).to_string())
            } else {
                Err(format!("unscripted: {joined}"))
            }
        };
        let event = RuleEvent {
            schema: rules::EVENT_SCHEMA.to_owned(),
            kind: "armed_unapproved".to_owned(),
            key: "k".to_owned(),
            at: at(),
            since: None,
            repo: Some("o/r".to_owned()),
            pr: Some(7),
            head: Some(HEAD.to_owned()),
            summary: String::new(),
            detail: json!({}),
            fix: rules::fix::Fix::default(),
        };
        let mut input = WatchInput::default();
        let tally = ReadTally::default();
        let read = complete_trails(
            &Source::Scripted(&gh),
            &mut input,
            std::slice::from_ref(&event),
            &tally,
        )
        .expect("read");
        assert_eq!(read, 1);
        let trail = &input.trails["o/r"][&7];
        assert!(trail.iter().any(|entry| {
            entry
                .verdict
                .as_ref()
                .is_some_and(|verdict| verdict.approve && verdict.head == HEAD)
        }));
        // Events that do not depend on the trail read nothing.
        let other = RuleEvent {
            kind: "required_red".to_owned(),
            ..event
        };
        let read =
            complete_trails(&Source::Scripted(&gh), &mut input, &[other], &tally).expect("read");
        assert_eq!(read, 0);
    }

    #[test]
    fn a_reviewer_must_name_its_repository() {
        let source = |reviewers: Vec<&str>| PrWatchRulesSource {
            repos: vec!["o/r".to_owned()],
            alarm_repo: None,
            input_events: Vec::new(),
            head_unreviewed_after: None,
            queue_stuck_after: "90m".to_owned(),
            approved_unarmed_after: "15m".to_owned(),
            reviewers: reviewers.into_iter().map(str::to_owned).collect(),
            no_cache: false,
        };
        let config = rule_config(&source(vec!["O/R=fable-graph"])).expect("valid");
        assert_eq!(config.reviewers["o/r"], "fable-graph");
        assert!(rule_config(&source(vec!["fable-graph"])).is_err());
    }
}
