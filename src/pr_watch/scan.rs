//! One live pass: gather, evaluate at `now`, fold into the ledger, plan (and
//! optionally post) sticky comments, and optionally run the digest. Shared by
//! `shipyard pr-watch scan` and the daemon's periodic job.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;

use super::comment::{self, CommentAction};
use super::digest::{self, DigestOutcome, DigestPayload, DigestPolicy};
use super::flags::{Flag, FlagKind, Thresholds, evaluate};
use super::gather::{WatchQuery, gather};
use super::handback::{self, HandbackConfig, HandbackMode, HandbackReport};
use super::ledger::{self, PrNow};
use super::{ACK_LABEL, head_at, open_at};
use crate::config::LoadedConfig;
use crate::gate_cost::{ReadCache, SyncGhReader};

/// A `gh` request that may mutate: used only for comment endpoints.
pub type GhWriter<'a> = dyn Fn(&[String]) -> Result<String, String> + 'a;
/// Delivers a digest payload.
pub type DigestSender<'a> = dyn FnMut(&str) -> Result<(), String> + 'a;
/// Asks the repository's batch attributor about one failed merge group:
/// `(pull request, run id)` to its verdict, `None` when it did not rule.
pub type Attributor<'a> = dyn Fn(u64, u64) -> Option<super::Attribution> + 'a;

/// Attributor calls per pass, most recent failed groups first. Each call reads
/// GitHub itself, so the pass bounds them.
pub const MAX_ATTRIBUTIONS_PER_PASS: usize = 8;

/// `[queue.attribution] command` and the checkout it runs from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributorCommand {
    /// Argv; Shipyard appends `--repo R --pr N --run-id ID`.
    pub argv: Vec<String>,
    /// Repository root (the directory holding `.shipyard/config.toml`).
    pub root: PathBuf,
}

impl AttributorCommand {
    /// The configured attributor, when the config names one and it is present
    /// in the checkout at `cwd` (a relative script path must exist under the
    /// root). Absent or unusable means "no attribution", never an error.
    #[must_use]
    pub fn discover(config: &LoadedConfig, cwd: &std::path::Path) -> Option<Self> {
        let argv: Vec<String> = config
            .get("queue.attribution.command")
            .and_then(toml::Value::as_array)?
            .iter()
            .map(|part| part.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?;
        if argv.is_empty() || argv.iter().any(String::is_empty) {
            return None;
        }
        let root = cwd
            .ancestors()
            .find(|dir| dir.join(".shipyard").join("config.toml").is_file())?
            .to_path_buf();
        let scripts_present = argv.iter().skip(1).all(|part| {
            let is_script = std::path::Path::new(part)
                .extension()
                .is_some_and(|ext| ext == "py" || ext == "sh");
            !is_script || part.starts_with('-') || root.join(part).is_file()
        });
        scripts_present.then_some(Self { argv, root })
    }

    /// Run it for one failed group. Exit 0 and a JSON verdict naming this run
    /// are required; anything else is "did not rule".
    #[must_use]
    pub fn ask(&self, repo: &str, pr: u64, run_id: u64) -> Option<super::Attribution> {
        #[cfg(unix)]
        {
            let (program, rest) = self.argv.split_first()?;
            let mut command = std::process::Command::new(program);
            command
                .args(rest)
                .args([
                    "--repo",
                    repo,
                    "--pr",
                    &pr.to_string(),
                    "--run-id",
                    &run_id.to_string(),
                ])
                .current_dir(&self.root);
            let deadline = Instant::now() + StdDuration::from_secs(180);
            let output =
                crate::process::run_output_until(&mut command, deadline, "queue attribution")
                    .ok()?;
            if !output.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
            if value.get("run_id").and_then(serde_json::Value::as_u64) != Some(run_id) {
                return None;
            }
            super::Attribution::parse(&stdout)
        }
        #[cfg(not(unix))]
        {
            let _ = (repo, pr, run_id);
            None
        }
    }
}

/// Fill in attributions for the failed merge groups that make an open pull
/// request's flag 3, most recent first, at most [`MAX_ATTRIBUTIONS_PER_PASS`]
/// calls. Decisive verdicts are cached per run.
fn attribute(
    history: &mut super::RepoHistory,
    attributor: &Attributor<'_>,
    cache: &ReadCache,
    now: DateTime<Utc>,
    thresholds: &Thresholds,
) {
    let mut failed_by_pr: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (index, run) in history.group_runs.iter().enumerate() {
        let Some(pr) = run.pr else { continue };
        let open = history
            .prs
            .get(&pr)
            .is_some_and(|entry| open_at(entry, now));
        let failed = run
            .required_jobs
            .iter()
            .any(|job| job.failed() && history.required_checks.contains(&job.name));
        if open && failed {
            failed_by_pr.entry(pr).or_default().push(index);
        }
    }
    let mut wanted: Vec<(DateTime<Utc>, u64, usize)> = Vec::new();
    for (pr, runs) in failed_by_pr {
        if runs.len() >= thresholds.failed_groups {
            wanted.extend(
                runs.into_iter()
                    .map(|index| (history.group_runs[index].created_at, pr, index)),
            );
        }
    }
    wanted.sort_by_key(|item| std::cmp::Reverse(item.0));
    let mut asked = 0;
    for (_, pr, index) in wanted {
        let run_id = history.group_runs[index].id;
        let key = format!("pr-watch:attribution:{}:{run_id}:{pr}", history.repo);
        if let Some(value) = cache.get(&key)
            && let Ok(found) = serde_json::from_value::<super::Attribution>(value)
        {
            history.group_runs[index].attribution = Some(found);
            continue;
        }
        if asked >= MAX_ATTRIBUTIONS_PER_PASS {
            continue;
        }
        asked += 1;
        if let Some(found) = attributor(pr, run_id) {
            if found.implicates_head.is_some()
                && let Ok(value) = serde_json::to_value(&found)
            {
                cache.put(&key, &value);
            }
            history.group_runs[index].attribution = Some(found);
        }
    }
}

/// `[pr_watch]` settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchConfig {
    /// Daemon job enabled.
    pub enabled: bool,
    /// Repositories the daemon job watches.
    pub repos: Vec<String>,
    /// Base branch.
    pub base: String,
    /// Gate workflow file.
    pub workflow: String,
    /// Required checks override.
    pub required_checks: Vec<String>,
    /// History window read per pass.
    pub lookback: Duration,
    /// Post sticky comments.
    pub post_comments: bool,
    /// Run the digest.
    pub digest: bool,
    /// Bot login whose marker comments are ours.
    pub comment_author: Option<String>,
    /// Digest command argv.
    pub digest_command: Vec<String>,
    /// Digest timing.
    pub digest_policy: DigestPolicy,
    /// Rule thresholds.
    pub thresholds: Thresholds,
    /// `[pr_watch.handback]`.
    pub handback: HandbackConfig,
    /// Config problems that do not stop a pass but must not pass silently,
    /// such as a `[pr_watch.digest]` table with no `enabled` key.
    pub warnings: Vec<String>,
    /// How long a scanning host may go without a completed pass before
    /// `pr-watch liveness` and `doctor` call it stale.
    pub stale_after: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repos: Vec::new(),
            base: "main".to_owned(),
            workflow: "build.yml".to_owned(),
            required_checks: Vec::new(),
            lookback: Duration::days(7),
            post_comments: false,
            digest: false,
            comment_author: None,
            digest_command: Vec::new(),
            digest_policy: DigestPolicy::default(),
            thresholds: Thresholds::default(),
            handback: HandbackConfig::default(),
            warnings: Vec::new(),
            stale_after: Duration::minutes(super::liveness::DEFAULT_STALE_AFTER_MINUTES),
        }
    }
}

fn strings(config: &LoadedConfig, key: &str) -> Option<Vec<String>> {
    config
        .get(key)
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
}

fn integer(config: &LoadedConfig, key: &str) -> Option<i64> {
    config.get(key).and_then(toml::Value::as_integer)
}

fn boolean(config: &LoadedConfig, key: &str) -> Option<bool> {
    config.get(key).and_then(toml::Value::as_bool)
}

/// Resolve the daemon digest toggle.
///
/// TOML cannot hold `pr_watch.digest` as both a boolean and a table, so the
/// toggle lives inside the table as `[pr_watch.digest] enabled`. A bare
/// `[pr_watch] digest = true` (no table) is still honoured. A table without
/// `enabled`, or a value of the wrong type, resolves to off with a warning
/// rather than silently reading as `digest = false`.
fn digest_toggle(config: &LoadedConfig) -> (bool, Option<String>) {
    match config.get("pr_watch.digest") {
        None => (false, None),
        Some(toml::Value::Boolean(enabled)) => (*enabled, None),
        Some(toml::Value::Table(table)) => match table.get("enabled") {
            Some(toml::Value::Boolean(enabled)) => (*enabled, None),
            Some(other) => (
                false,
                Some(format!(
                    "[pr_watch.digest] enabled must be true or false, got {}; digest stays off",
                    other.type_str()
                )),
            ),
            None => (
                false,
                Some(
                    "[pr_watch.digest] is configured but has no `enabled = true`; \
                     the daemon digest stays off"
                        .to_owned(),
                ),
            ),
        },
        Some(other) => (
            false,
            Some(format!(
                "[pr_watch] digest must be a boolean or a [pr_watch.digest] table, got {}; \
                 digest stays off",
                other.type_str()
            )),
        ),
    }
}

impl WatchConfig {
    /// Read `[pr_watch]`, `[pr_watch.thresholds]` and `[pr_watch.digest]`.
    /// Every key is optional; `enabled`, `post_comments` and the digest
    /// toggle (`[pr_watch.digest] enabled`, or a bare `[pr_watch] digest`
    /// boolean) default to `false`.
    ///
    /// # Errors
    /// When `lookback` is not a valid window.
    pub fn from_config(config: &LoadedConfig) -> Result<Self, String> {
        let (digest, digest_warning) = digest_toggle(config);
        let mut out = Self {
            enabled: boolean(config, "pr_watch.enabled").unwrap_or(false),
            post_comments: boolean(config, "pr_watch.post_comments").unwrap_or(false),
            digest,
            warnings: digest_warning.into_iter().collect(),
            repos: strings(config, "pr_watch.repos").unwrap_or_default(),
            handback: HandbackConfig::from_config(config),
            ..Self::default()
        };
        if let Some(base) = config.get_str("pr_watch.base") {
            base.clone_into(&mut out.base);
        }
        if let Some(workflow) = config
            .get_str("pr_watch.workflow")
            .or_else(|| config.get_str("metrics.gate_cost.workflow"))
        {
            workflow.clone_into(&mut out.workflow);
        }
        out.required_checks = strings(config, "pr_watch.required_checks").unwrap_or_default();
        if let Some(lookback) = config.get_str("pr_watch.lookback") {
            out.lookback = super::parse_duration(lookback)?;
        }
        out.comment_author = config.get_str("pr_watch.comment_author").map(str::to_owned);
        out.digest_command = strings(config, "pr_watch.digest.command").unwrap_or_default();
        if let Some(minutes) = integer(config, "pr_watch.digest.interval_minutes") {
            out.digest_policy.interval = Duration::minutes(minutes.max(1));
        }
        if let Some(minutes) = integer(config, "pr_watch.stale_after_minutes") {
            out.stale_after = Duration::minutes(minutes.max(1));
        }
        if let Some(minutes) = integer(config, "pr_watch.digest.min_age_minutes") {
            out.digest_policy.min_age = Duration::minutes(minutes.max(0));
        }
        let t = &mut out.thresholds;
        let count = |key: &str, slot: &mut usize| {
            if let Some(value) = integer(config, &format!("pr_watch.thresholds.{key}")) {
                *slot = usize::try_from(value.max(1)).unwrap_or(1);
            }
        };
        count("repeat_failures", &mut t.repeat_failures);
        count("pre_existing_other_prs", &mut t.pre_existing_other_prs);
        count("failed_groups", &mut t.failed_groups);
        count("replacements", &mut t.replacements);
        let span = |key: &str, slot: &mut i64| {
            if let Some(value) = integer(config, &format!("pr_watch.thresholds.{key}")) {
                *slot = value.max(1);
            }
        };
        span(
            "pre_existing_window_hours",
            &mut t.pre_existing_window_hours,
        );
        span("red_minutes", &mut t.red_minutes);
        span("replacement_window_hours", &mut t.replacement_window_hours);
        span("split_days", &mut t.split_days);
        span("green_unarmed_minutes", &mut t.green_unarmed_minutes);
        if let Some(value) = integer(config, "pr_watch.thresholds.split_files") {
            t.split_files = u64::try_from(value.max(1)).unwrap_or(1);
        }
        if let Some(value) = integer(config, "pr_watch.thresholds.split_commits") {
            t.split_commits = u64::try_from(value.max(1)).unwrap_or(1);
        }
        Ok(out)
    }
}

/// What to do in one pass.
#[derive(Clone, Debug)]
pub struct ScanRequest {
    /// `OWNER/REPO`.
    pub repo: String,
    /// Settings.
    pub config: WatchConfig,
    /// Ledger path.
    pub state_path: PathBuf,
    /// Send comment requests.
    pub post_comments: bool,
    /// Deliver the digest through the sender.
    pub post_digest: bool,
    /// Plan comments (reads comment lists of flagged pull requests that have
    /// no recorded comment). A dry-run CLI plans to show what it would send;
    /// the daemon plans only when it will post.
    pub plan_comments: bool,
    /// Hand-back: off, plan (dry run), or deliver.
    pub handback: HandbackMode,
}

/// What one pass found and did.
#[derive(Clone, Debug, Serialize)]
pub struct ScanReport {
    /// `OWNER/REPO`.
    pub repo: String,
    /// Evaluation instant.
    pub at: DateTime<Utc>,
    /// Required checks used.
    pub required_checks: Vec<String>,
    /// Flags holding now.
    pub flags: Vec<Flag>,
    /// Comment requests planned.
    pub comment_actions: Vec<CommentAction>,
    /// Whether they were sent.
    pub comments_posted: bool,
    /// Comment failures.
    pub comment_errors: Vec<String>,
    /// Digest outcome.
    pub digest: DigestOutcome,
    /// Digest payload built (sent or would-send).
    pub digest_payload: Option<DigestPayload>,
    /// Ledger changes this pass.
    pub ledger_changes: usize,
    /// Read gaps.
    pub gaps: Vec<String>,
    /// GitHub reads sent / served from cache.
    pub reads: crate::gate_cost::ReadStats,
    /// Hand-back plan or deliveries, when requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handback: Option<HandbackReport>,
    /// Where every open handed pull request stands.
    pub coverage: super::coverage::CoverageSummary,
}

/// Current pull-request facts for the ledger.
#[must_use]
pub fn pr_now(history: &super::RepoHistory, at: DateTime<Utc>) -> BTreeMap<u64, PrNow> {
    history
        .prs
        .values()
        .map(|pr| {
            (
                pr.number,
                PrNow {
                    open: open_at(pr, at),
                    merged: pr.merged_at.is_some(),
                    head_sha: head_at(pr, at)
                        .map_or_else(|| pr.head_sha.clone(), |head| head.sha.clone()),
                    acknowledged: pr.labels.iter().any(|label| label == ACK_LABEL),
                    title: pr.title.clone(),
                    url: pr.url.clone(),
                },
            )
        })
        .collect()
}

/// Run one pass. `handback` supplies the host runner and local identity the
/// hand-back needs; it runs only when both it and `request.handback` ask.
///
/// # Errors
/// When the history cannot be read or the ledger cannot be locked, loaded,
/// or saved. Comment and digest delivery failures are reported, not raised,
/// except a digest failure after its claim is rolled back.
#[allow(clippy::too_many_arguments)]
pub fn scan(
    reader: &SyncGhReader<'_>,
    writer: &GhWriter<'_>,
    sender: &mut DigestSender<'_>,
    attributor: Option<&Attributor<'_>>,
    cache: &ReadCache,
    request: &ScanRequest,
    now: DateTime<Utc>,
    handback: Option<&mut handback::Deps<'_>>,
) -> Result<ScanReport, String> {
    let config = &request.config;
    let query = WatchQuery {
        repo: request.repo.clone(),
        base: config.base.clone(),
        workflow: config.workflow.clone(),
        required_checks: config.required_checks.clone(),
        from: now - config.lookback,
        to: now,
    };
    let mut history = gather(reader, cache, &query, &config.thresholds)?;
    if let Some(attributor) = attributor {
        attribute(&mut history, attributor, cache, now, &config.thresholds);
    }
    let mut flags = evaluate(&history, now, &config.thresholds);
    let (screen_gaps, screened_held) = screen_green_unarmed(reader, &request.repo, &mut flags);
    let mut coverage_rows = super::coverage::coverage(&history, &flags, now, &config.thresholds);
    for row in &mut coverage_rows {
        if screened_held.contains(&row.pr) {
            row.state = super::coverage::CoverageState::Held;
            "draft, or a `shipyard:hold` line in the body or a comment".clone_into(&mut row.reason);
        }
    }
    let coverage = super::coverage::summarize(&coverage_rows, now);
    let prs = pr_now(&history, now);
    let open_prs: Vec<u64> = prs
        .iter()
        .filter(|(_, pr)| pr.open)
        .map(|(number, _)| *number)
        .collect();
    let _lock = ledger::lock(&request.state_path)?;
    let mut ledger = ledger::load(&request.state_path, &request.repo, &config.base)?;
    let events = ledger::reconcile(&mut ledger, &flags, &prs, now);
    ledger.coverage = Some(coverage.clone());
    ledger::append_events(&request.state_path, &events)?;
    // Acknowledged pull requests get no comment either.
    let commentable: Vec<Flag> = flags
        .iter()
        .filter(|flag| !prs.get(&flag.pr).is_some_and(|pr| pr.acknowledged))
        .cloned()
        .collect();
    let (actions, mut gaps) = if request.plan_comments || request.post_comments {
        comment::plan(
            reader,
            &mut ledger,
            &commentable,
            &open_prs,
            config.comment_author.as_deref(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let comment_errors = if request.post_comments {
        comment::apply(&mut ledger, &actions, writer)
    } else {
        Vec::new()
    };
    let handback_report = match handback {
        Some(deps) if request.handback != HandbackMode::Off => Some(handback::run(
            &mut ledger,
            &history,
            now,
            &config.handback,
            request.handback,
            reader,
            writer,
            deps,
        )),
        _ => None,
    };
    if let Some(report) = &handback_report {
        ledger::append_events(&request.state_path, &report.events)?;
    }
    ledger::save(&request.state_path, &ledger)?;
    let mut persist = |ledger: &ledger::Ledger| ledger::save(&request.state_path, ledger);
    let (digest, digest_payload) = digest::run(
        &mut ledger,
        now,
        config.digest_policy,
        request.post_digest,
        &mut persist,
        sender,
    )?;
    gaps.extend(history.gaps.iter().cloned());
    gaps.extend(screen_gaps);
    Ok(ScanReport {
        repo: request.repo.clone(),
        at: now,
        required_checks: history.required_checks.clone(),
        flags,
        comment_actions: actions,
        comments_posted: request.post_comments,
        comment_errors,
        digest,
        digest_payload,
        ledger_changes: events.len(),
        gaps,
        reads: cache.stats(),
        handback: handback_report,
        coverage,
    })
}

/// At most this many pull requests are screened per pass.
const SCREEN_LIMIT: usize = 20;

/// Screen flag-6 candidates against what the history does not carry: drop a
/// draft, and a pull request whose body or comments hold a `shipyard:hold`
/// line; name the latest comment that promised someone would arm it. A
/// candidate that cannot be read keeps its flag, with a gap saying so.
/// Replay does not screen, like the attributor.
fn screen_green_unarmed(
    reader: &SyncGhReader<'_>,
    repo: &str,
    flags: &mut Vec<Flag>,
) -> (Vec<String>, Vec<u64>) {
    let mut gaps = Vec::new();
    let candidates: Vec<u64> = flags
        .iter()
        .filter(|flag| flag.kind == FlagKind::GreenUnarmed)
        .map(|flag| flag.pr)
        .collect();
    let mut drop: Vec<u64> = Vec::new();
    for (index, pr) in candidates.into_iter().enumerate() {
        if index >= SCREEN_LIMIT {
            gaps.push(format!(
                "#{pr}: flag 6 not screened (more than {SCREEN_LIMIT} candidates this pass)"
            ));
            continue;
        }
        match read_screen(reader, repo, pr) {
            Ok(screen) if screen.draft || screen.held => drop.push(pr),
            Ok(screen) => {
                if let (Some(note), Some(flag)) = (
                    screen.arm_note,
                    flags
                        .iter_mut()
                        .find(|flag| flag.pr == pr && flag.kind == FlagKind::GreenUnarmed),
                ) {
                    let _ = write!(flag.evidence, "; {note}");
                }
            }
            Err(error) => gaps.push(format!("#{pr}: flag 6 screen: {error}")),
        }
    }
    flags.retain(|flag| !(flag.kind == FlagKind::GreenUnarmed && drop.contains(&flag.pr)));
    (gaps, drop)
}

/// What a flag-6 screen read.
#[derive(Debug, Default, PartialEq, Eq)]
struct Screen {
    draft: bool,
    held: bool,
    arm_note: Option<String>,
}

fn read_screen(reader: &SyncGhReader<'_>, repo: &str, pr: u64) -> Result<Screen, String> {
    let raw = reader(&["api".to_owned(), format!("repos/{repo}/pulls/{pr}")])?;
    let pull: Value = serde_json::from_str(&raw).map_err(|e| format!("pull request JSON: {e}"))?;
    let raw = reader(&[
        "api".to_owned(),
        format!("repos/{repo}/issues/{pr}/comments?per_page=100"),
    ])?;
    let comments: Value = serde_json::from_str(&raw).map_err(|e| format!("comments JSON: {e}"))?;
    Ok(screen_of(&pull, &comments))
}

/// A line that is exactly `shipyard:hold` (any case, optional trailing
/// reason after whitespace or a colon) marks a deliberate hold.
fn holds(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.strip_prefix("shipyard:hold")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', ':', '\t']))
    })
}

/// The sentence of `text` that promises an arm (`... arms ...`, `... will
/// arm ...`), clipped.
fn arm_promise(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let at = [" arms ", " arms.", " arms,", " will arm ", " to arm "]
        .iter()
        .filter_map(|needle| lower.find(needle))
        .min()?;
    let start = text[..at].rfind(['.', '\n', '*']).map_or(0, |i| i + 1);
    let end = text[at + 1..]
        .find(['.', '\n'])
        .map_or(text.len(), |i| at + 1 + i + 1);
    let sentence: String = text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let clipped: String = sentence.chars().take(140).collect();
    (!clipped.is_empty()).then_some(clipped)
}

fn screen_of(pull: &Value, comments: &Value) -> Screen {
    let body = pull.get("body").and_then(Value::as_str).unwrap_or_default();
    let comments: Vec<&Value> = comments
        .as_array()
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    let held = holds(body)
        || comments
            .iter()
            .any(|c| holds(c.get("body").and_then(Value::as_str).unwrap_or_default()));
    let arm_note = comments.iter().rev().find_map(|comment| {
        let body = comment.get("body").and_then(Value::as_str)?;
        // pr-watch's own sticky comment quotes the note; never re-quote it.
        if body.contains(super::COMMENT_MARKER) {
            return None;
        }
        let note = arm_promise(body)?;
        let who = comment
            .pointer("/user/login")
            .and_then(Value::as_str)
            .unwrap_or("someone");
        let when = comment
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Some(format!("waiting on \"{note}\" ({who}, {when})"))
    });
    Screen {
        draft: pull.get("draft").and_then(Value::as_bool).unwrap_or(false),
        held,
        arm_note,
    }
}

/// Deliver a digest by running `argv` with the payload on stdin. Exit 0 is
/// delivered.
///
/// # Errors
/// When the command is empty, cannot run, times out, or exits non-zero.
pub fn run_digest_command(command_argv: &[String], payload: &str) -> Result<(), String> {
    let Some((program, program_args)) = command_argv.split_first() else {
        return Err("[pr_watch.digest] command is not configured".to_owned());
    };
    #[cfg(unix)]
    {
        let mut command = std::process::Command::new(program);
        command.args(program_args);
        let deadline = Instant::now() + StdDuration::from_secs(120);
        let output = crate::process::run_output_with_input_until(
            &mut command,
            payload.as_bytes(),
            deadline,
            "pr-watch digest command",
        )
        .map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "digest command exited {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (
            program,
            program_args,
            payload,
            Instant::now(),
            StdDuration::ZERO,
        );
        Err("the digest command is supported on unix hosts only".to_owned())
    }
}

/// Summary of one daemon pass over the configured repositories.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DaemonPass {
    /// `false` when `[pr_watch] enabled` is off (nothing was read).
    pub enabled: bool,
    /// Per-repository flag counts.
    pub flags: BTreeMap<String, usize>,
    /// Per-repository failures.
    pub errors: Vec<String>,
    /// Config warnings (for example a digest table that is not enabled).
    pub warnings: Vec<String>,
}

/// One daemon pass: re-read machine-global `[pr_watch]` config (so a toggle
/// needs no restart) and scan each repository. `[pr_watch] repos` wins over
/// the daemon's advertised repositories. Comments and digest follow
/// `post_comments` / `digest`, both off by default.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn daemon_pass(
    global_dir: &std::path::Path,
    state_dir: &std::path::Path,
    daemon_repos: &[String],
    now: DateTime<Utc>,
) -> DaemonPass {
    let config = match LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf()) {
        Ok(config) => config,
        Err(error) => {
            return DaemonPass {
                errors: vec![format!("config: {error}")],
                ..DaemonPass::default()
            };
        }
    };
    let watch = match WatchConfig::from_config(&config) {
        Ok(watch) => watch,
        Err(error) => {
            return DaemonPass {
                errors: vec![format!("[pr_watch]: {error}")],
                ..DaemonPass::default()
            };
        }
    };
    if !watch.enabled {
        return DaemonPass::default();
    }
    let repos = if watch.repos.is_empty() {
        daemon_repos.to_vec()
    } else {
        watch.repos.clone()
    };
    let mut pass = DaemonPass {
        enabled: true,
        warnings: watch.warnings.clone(),
        ..DaemonPass::default()
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for repo in repos {
        let actions = crate::cloud::GitHubActions::from_loaded_config(&cwd, &config)
            .with_repo_override(&repo);
        let timeout = StdDuration::from_secs(120);
        let reader = |argv: &[String]| {
            actions
                .run_gh_with_timeout_env(argv, timeout, &[("GH_REPO", repo.as_str())])
                .map_err(|error| error.to_string())
        };
        let writer = |argv: &[String]| {
            actions
                .run_gh_with_timeout_env(argv, timeout, &[("GH_REPO", repo.as_str())])
                .map_err(|error| error.to_string())
        };
        let argv = watch.digest_command.clone();
        let mut sender = |payload: &str| run_digest_command(&argv, payload);
        let cache = ReadCache::open(
            &state_dir
                .join("pr-watch")
                .join("cache")
                .join(repo.replace('/', "__").to_ascii_lowercase()),
            std::time::SystemTime::now(),
        );
        let request = ScanRequest {
            repo: repo.clone(),
            config: watch.clone(),
            state_path: ledger::default_path(state_dir, &repo, &watch.base),
            post_comments: watch.post_comments,
            post_digest: watch.digest && !watch.digest_command.is_empty(),
            plan_comments: watch.post_comments,
            handback: if watch.handback.enabled {
                HandbackMode::Deliver
            } else {
                HandbackMode::Off
            },
        };
        if watch.digest && watch.digest_command.is_empty() {
            pass.errors.push(format!(
                "{repo}: the digest is enabled but [pr_watch.digest] command is empty; not sending"
            ));
        }
        // The daemon runs outside any checkout, so it has no repository
        // attributor; flag 3 stays on the named-failed-groups rule there.
        let mut runner = handback::host::ProcessHostRunner {
            cmux_path: watch.handback.cmux_path.clone(),
            timeout: StdDuration::from_secs(watch.handback.timeout_seconds),
            inbox_dir: handback::host::default_inbox_dir(),
        };
        let mut deps = handback::Deps {
            runner: &mut runner,
            state_dir: state_dir.to_path_buf(),
            local_names: handback::local_host_names(),
            local_machine: handback::owner::local_machine_identity(state_dir),
        };
        match scan(
            &reader,
            &writer,
            &mut sender,
            None,
            &cache,
            &request,
            now,
            Some(&mut deps),
        ) {
            Ok(report) => {
                pass.flags.insert(repo.clone(), report.flags.len());
                pass.errors.extend(
                    report
                        .comment_errors
                        .into_iter()
                        .map(|e| format!("{repo}: {e}")),
                );
            }
            Err(error) => pass.errors.push(format!("{repo}: {error}")),
        }
    }
    pass
}

#[cfg(test)]
mod screen_tests {
    use serde_json::json;

    use super::*;
    use crate::pr_watch::flags::DigestRoute;

    fn flag(pr: u64, kind: FlagKind) -> Flag {
        Flag {
            pr,
            kind,
            key: "abc".to_owned(),
            verdict: "v".to_owned(),
            evidence: "green".to_owned(),
            head_sha: "abc".to_owned(),
            route: DigestRoute::PerPr,
            shared_tests: Vec::new(),
            related_prs: Vec::new(),
        }
    }

    #[test]
    fn a_hold_line_is_exact() {
        assert!(holds("shipyard:hold"));
        assert!(holds(
            "Notes\n  Shipyard:Hold: waiting on the examples lane\n"
        ));
        assert!(holds("shipyard:hold until Friday"));
        assert!(!holds("shipyard:holder"));
        assert!(!holds("please do not shipyard:hold this"));
    }

    #[test]
    fn an_arm_promise_quotes_its_sentence() {
        assert_eq!(
            arm_promise("Approved at 3e7ee2ca. Unarmed; team-lead arms. Read against the code.")
                .as_deref(),
            Some("Unarmed; team-lead arms.")
        );
        assert_eq!(
            arm_promise(
                "**Approved at this head; team-lead arms with MERGE once the required checks are green.**"
            )
            .as_deref(),
            Some("Approved at this head; team-lead arms with MERGE once the required checks are green.")
        );
        assert_eq!(arm_promise("Unarmed and not queued."), None);
    }

    #[test]
    fn the_screen_drops_drafts_and_holds_and_names_the_arm_promise() {
        let calls = std::sync::Mutex::new(Vec::new());
        let reader = |argv: &[String]| -> Result<String, String> {
            calls.lock().unwrap().push(argv.join(" "));
            let path = argv[1].as_str();
            let comment = |body: &str| json!({"body": body, "user": {"login": "lead[bot]"}, "created_at": "2026-10-06T09:16:26Z"});
            match path {
                "repos/o/r/pulls/1" => Ok(json!({"draft": true, "body": ""}).to_string()),
                "repos/o/r/pulls/2" => {
                    Ok(json!({"draft": false, "body": "Fix.\n\nshipyard:hold"}).to_string())
                }
                "repos/o/r/pulls/3" | "repos/o/r/pulls/4" | "repos/o/r/pulls/5" => {
                    Ok(json!({"draft": false, "body": "Fix."}).to_string())
                }
                "repos/o/r/issues/3/comments?per_page=100" => Ok(json!([
                    comment("Approved at abc. Unarmed; team-lead arms."),
                    comment(&format!(
                        "{}\n- green but nobody armed it; waiting on \"x arms.\"",
                        crate::pr_watch::COMMENT_MARKER
                    )),
                ])
                .to_string()),
                "repos/o/r/issues/4/comments?per_page=100" => Ok(json!([comment(
                    "Holding.\nshipyard:hold: examples lane first"
                )])
                .to_string()),
                p if p.ends_with("/comments?per_page=100") => Ok("[]".to_owned()),
                _ => Err("boom".to_owned()),
            }
        };
        let mut flags = vec![
            flag(1, FlagKind::GreenUnarmed),
            flag(2, FlagKind::GreenUnarmed),
            flag(3, FlagKind::GreenUnarmed),
            flag(4, FlagKind::GreenUnarmed),
            flag(5, FlagKind::GreenUnarmed),
            flag(6, FlagKind::GreenUnarmed),
            flag(7, FlagKind::RedWhileArmed),
        ];
        let (gaps, held) = screen_green_unarmed(&reader, "o/r", &mut flags);
        assert_eq!(held, vec![1, 2, 4]);
        let left: Vec<u64> = flags.iter().map(|f| f.pr).collect();
        // Draft (1), body hold (2), comment hold (4) are dropped; an
        // unreadable candidate (6) keeps its flag with a gap; other kinds are
        // never read.
        assert_eq!(left, vec![3, 5, 6, 7]);
        assert_eq!(
            flags[0].evidence,
            "green; waiting on \"Unarmed; team-lead arms.\" (lead[bot], 2026-10-06T09:16:26Z)"
        );
        assert_eq!(flags[1].evidence, "green");
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert!(gaps[0].starts_with("#6: flag 6 screen"), "{gaps:?}");
        assert!(!calls.lock().unwrap().iter().any(|c| c.contains("/7")));
    }
}
