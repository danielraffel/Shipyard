//! `shipyard relay`: run [`crate::relay::pass`] once, or every interval.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::Utc;
use fs2::FileExt;
use serde_json::{Value, json};

use super::CliFailure;
use super::owner_cmd::{local_host, machine_relay_config};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::gh_pass::GhPass;
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::relay::config::{AdmitReady, Mode, RelayConfig};
use crate::relay::pass::{PassInput, ReadyAdmission, RelayState, run_pass};
use crate::relay::transport::ProcessRunner;

/// Each GitHub read may take this long.
const GITHUB_TIMEOUT: Duration = Duration::from_mins(1);
/// Each cmux invocation may take this long.
const CMUX_TIMEOUT: Duration = Duration::from_secs(40);

/// What the caller asked for.
pub(super) struct RelayArgs {
    pub follow: bool,
    pub interval: Option<u64>,
    pub repos: Vec<String>,
    pub send: bool,
    pub shadow: bool,
    pub max_passes: Option<u64>,
}

fn io_failure(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(1, error.to_string())
}

/// Where a mode keeps its state and delivery log. Shadow and send never
/// share one: a shadow day must not mark events as delivered.
pub(crate) fn relay_paths(state_dir: &Path, mode: Mode) -> (PathBuf, PathBuf, PathBuf) {
    let dir = state_dir.join("relay");
    let prefix = match mode {
        Mode::Shadow => "shadow-",
        Mode::Send => "",
    };
    (
        dir.join(format!("{prefix}state.json")),
        dir.join(format!("{prefix}deliveries.jsonl")),
        dir.join("relay.lock"),
    )
}

/// The steward's newest tick report for `repo` in `dir`, when no older than
/// `max_age`.
fn newest_steward_report(dir: Option<&Path>, repo: &str, max_age: Duration) -> Option<Value> {
    let dir = dir?;
    let prefix = format!("{}-", repo.replace('/', "_"));
    let newest = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix))
                && path.extension().is_some_and(|ext| ext == "json")
        })
        .max()?;
    let fresh = std::fs::metadata(&newest)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age <= max_age);
    if !fresh {
        return None;
    }
    std::fs::read_to_string(&newest)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
}

/// The steward's newest tick report for `repo` in `dir`, when no older than
/// `max_age`, reduced to per-PR next actions.
pub(crate) fn steward_actions(
    dir: Option<&Path>,
    repo: &str,
    max_age: Duration,
) -> BTreeMap<u64, (String, String)> {
    newest_steward_report(dir, repo, max_age)
        .map(|report| crate::relay::steward_next_actions(&report))
        .unwrap_or_default()
}

/// The same report's overdue rows ([`crate::relay::steward_overdue`]).
pub(crate) fn steward_overdue(
    dir: Option<&Path>,
    repo: &str,
    max_age: Duration,
) -> BTreeMap<u64, Vec<crate::relay::RelayEvent>> {
    newest_steward_report(dir, repo, max_age)
        .map(|report| crate::relay::steward_overdue(&report))
        .unwrap_or_default()
}

fn load_state(path: &Path) -> Result<RelayState, CliFailure> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "relay state {} is unreadable ({error}); move it aside to start over",
                    path.display()
                ),
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RelayState::default()),
        Err(error) => Err(io_failure(error)),
    }
}

fn append_log(path: &Path, records: &[Value]) -> Result<(), CliFailure> {
    if records.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_failure)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(io_failure)?;
    let mut text = String::new();
    for record in records {
        text.push_str(&record.to_string());
        text.push('\n');
    }
    file.write_all(text.as_bytes()).map_err(io_failure)?;
    file.sync_all().map_err(io_failure)
}

/// The ready admission a relay in `mode` runs: a shadow relay mutates
/// nothing, so `apply` there only reports.
pub(crate) fn effective_admit_ready(configured: AdmitReady, mode: Mode) -> AdmitReady {
    match (configured, mode) {
        (AdmitReady::Apply, Mode::Shadow) => AdmitReady::Report,
        (admit, _) => admit,
    }
}

/// Run `shipyard relay`.
#[allow(clippy::too_many_lines)]
pub(super) fn relay_command<W: Write>(
    args: &RelayArgs,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config =
        LoadedConfig::load_from_cwd_with_global_dir(mode, cwd, runtime_paths.global_dir.clone())
            .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let runtime_mode = mode;
    let relay: RelayConfig = machine_relay_config(runtime_paths)?;
    if !relay.enabled {
        writeln!(
            stdout,
            "▸ relay disabled (relay.enabled = false); nothing done"
        )
        .map_err(io_failure)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mode = if args.send {
        Mode::Send
    } else if args.shadow {
        Mode::Shadow
    } else {
        relay.mode
    };
    let repos = if args.repos.is_empty() {
        relay.repos.clone()
    } else {
        args.repos.clone()
    };
    if repos.is_empty() {
        return Err(CliFailure::new(
            2,
            "no repositories to relay: pass --repo OWNER/REPO or set relay.repos",
        ));
    }
    let here = local_host(&relay);
    let (state_path, log_path, lock_path) = relay_paths(&runtime_paths.state_dir, mode);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(io_failure)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(io_failure)?;
    lock.try_lock_exclusive().map_err(|_| {
        CliFailure::new(
            1,
            format!(
                "another relay holds {}; one relay per host",
                lock_path.display()
            ),
        )
    })?;
    let _lock = crate::file_lock::LockedFile::new(lock);
    let runner = ProcessRunner {
        timeout: CMUX_TIMEOUT,
    };
    let interval = Duration::from_secs(args.interval.unwrap_or(relay.interval_secs));
    let tick_dir = relay.steward_tick_dir.clone();
    let tick_age = Duration::from_secs(relay.steward_tick_max_age_secs);
    // `relay.admit_ready`: when an event makes a pull request ready, run the
    // steward's own admission for it (see `merge_steward_cmd::ready_event`).
    // A shadow relay mutates nothing, so there it only reports.
    let admit_ready = effective_admit_ready(relay.admit_ready, mode);
    let steward_reads = GitHubActions::from_loaded_config(cwd, &config).with_inproc_reads(
        crate::github_read::for_steward(&config, &runtime_paths.state_dir),
    );
    let ready = |repo: &str, pr: u64, head: &str, base: &str| -> Value {
        let outcome = super::merge_steward_cmd::ready_event::admit_ready(
            &steward_reads,
            cwd,
            runtime_mode,
            runtime_paths,
            &super::merge_steward_cmd::ready_event::AdmitReadyArgs {
                repo: repo.to_owned(),
                base: base.to_owned(),
                pr,
                head: head.to_owned(),
                min_interval: super::merge_steward_cmd::ready_event::DEFAULT_MIN_INTERVAL,
                apply: admit_ready == AdmitReady::Apply,
            },
        );
        serde_json::to_value(outcome).unwrap_or(Value::Null)
    };
    let ready_hook: Option<&ReadyAdmission<'_>> =
        (admit_ready != AdmitReady::Off).then_some(&ready);
    let mut passes = 0_u64;
    let mut unhealthy;
    loop {
        passes += 1;
        let mut state = load_state(&state_path)?;
        let mut repo_reports = Vec::new();
        let mut api_calls = BTreeMap::new();
        let mut log = Vec::new();
        for repo in &repos {
            let pass = Arc::new(GhPass::caching());
            let actions = GitHubActions::from_loaded_config(cwd, &config)
                .with_repo_override(repo)
                .with_pass(Arc::clone(&pass));
            let gh = |gh_args: &[String]| {
                actions
                    .run_gh_with_timeout(gh_args, GITHUB_TIMEOUT)
                    .map_err(|error| error.to_string())
            };
            let steward = |repo: &str| steward_actions(tick_dir.as_deref(), repo, tick_age);
            let steward_overdue = |repo: &str| steward_overdue(tick_dir.as_deref(), repo, tick_age);
            let one = std::slice::from_ref(repo);
            // State first, then the record: a crash between them loses a log
            // line, never sends a message twice.
            let persist = |state: &RelayState, record: &Value| -> Result<(), String> {
                crate::durable_file::replace_json(&state_path, state, true)
                    .map_err(|error| format!("save relay state: {error}"))?;
                append_log(&log_path, std::slice::from_ref(record))
                    .map_err(|error| format!("append delivery log: {}", error.message))
            };
            let input = PassInput {
                config: &relay,
                repos: one,
                local_host: &here,
                mode,
                state_dir: &runtime_paths.state_dir,
                now: Utc::now(),
                gh: &gh,
                runner: &runner,
                steward: &steward,
                steward_overdue: &steward_overdue,
                persist: Some(&persist),
                ready: ready_hook,
            };
            let report = run_pass(&input, &mut state);
            log.extend(report.log);
            repo_reports.extend(report.repos);
            api_calls.insert(repo.clone(), pass.report());
        }
        crate::durable_file::replace_json(&state_path, &state, true).map_err(io_failure)?;
        append_log(&log_path, &log)?;
        unhealthy = repo_reports.iter().any(|r| !r.errors.is_empty());
        if json_output {
            let mut data = BTreeMap::new();
            data.insert("mode".to_owned(), json!(mode.as_str()));
            data.insert("host".to_owned(), json!(here));
            data.insert("pass".to_owned(), json!(passes));
            data.insert("repos".to_owned(), json!(repo_reports));
            data.insert("api_calls".to_owned(), json!(api_calls));
            data.insert("log".to_owned(), json!(log_path.display().to_string()));
            write_json_envelope(stdout, "relay", data).map_err(io_failure)?;
        } else {
            for report in &repo_reports {
                let calls = api_calls.get(&report.repo);
                writeln!(
                    stdout,
                    "▸ relay {} [{}] {}: {} tracked, {} event(s), {} delivered, {} undelivered; \
                     GitHub {} GraphQL / {} REST",
                    Utc::now().format("%H:%M:%SZ"),
                    mode.as_str(),
                    report.repo,
                    report.tracked,
                    report.events,
                    report.deliveries,
                    report.undelivered,
                    calls.map_or(0, |c| c.graphql),
                    calls.map_or(0, |c| c.rest),
                )
                .map_err(io_failure)?;
                for error in &report.errors {
                    writeln!(stdout, "  error: {error}").map_err(io_failure)?;
                }
            }
        }
        stdout.flush().map_err(io_failure)?;
        if !args.follow || args.max_passes.is_some_and(|max| passes >= max) {
            break;
        }
        std::thread::sleep(interval);
    }
    Ok(if unhealthy {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_and_send_never_share_state_or_log() {
        let dir = Path::new("/s");
        let (shadow_state, shadow_log, shadow_lock) = relay_paths(dir, Mode::Shadow);
        let (state, log, lock) = relay_paths(dir, Mode::Send);
        assert_ne!(shadow_state, state);
        assert_ne!(shadow_log, log);
        assert_eq!(shadow_lock, lock, "one relay per host, whatever the mode");
        assert_eq!(state, Path::new("/s/relay/state.json"));
    }

    #[test]
    fn only_a_fresh_steward_report_for_the_repository_is_reused() {
        let temp = tempfile::tempdir().expect("temp");
        let report = serde_json::json!({"repos": [{"prs": [
            {"number": 5, "head_sha": "abc", "next_action": {"action": "wait"}}]}]});
        std::fs::write(
            temp.path().join("Generous-Corp_pulp-20261109T120000Z.json"),
            report.to_string(),
        )
        .expect("write");
        std::fs::write(
            temp.path().join("Generous-Corp_pulp-20261109T110000Z.json"),
            "{}",
        )
        .expect("older");
        std::fs::write(
            temp.path()
                .join("Generous-Corp_pulp-20261109T130000Z.json.err"),
            "",
        )
        .expect("err file");
        let fresh = steward_actions(
            Some(temp.path()),
            "Generous-Corp/pulp",
            Duration::from_secs(600),
        );
        assert_eq!(fresh.get(&5).map(|(_, a)| a.as_str()), Some("wait"));
        assert!(
            steward_actions(
                Some(temp.path()),
                "Generous-Corp/vellum",
                Duration::from_secs(600)
            )
            .is_empty()
        );
        assert!(
            steward_actions(Some(temp.path()), "Generous-Corp/pulp", Duration::ZERO).is_empty(),
            "a stale report is not reused"
        );
        assert!(steward_actions(None, "Generous-Corp/pulp", Duration::from_secs(600)).is_empty());
    }

    #[test]
    fn a_shadow_relay_only_reports_the_ready_admission() {
        assert_eq!(
            effective_admit_ready(AdmitReady::Apply, Mode::Shadow),
            AdmitReady::Report
        );
        assert_eq!(
            effective_admit_ready(AdmitReady::Apply, Mode::Send),
            AdmitReady::Apply
        );
        assert_eq!(
            effective_admit_ready(AdmitReady::Report, Mode::Send),
            AdmitReady::Report
        );
        assert_eq!(
            effective_admit_ready(AdmitReady::Off, Mode::Send),
            AdmitReady::Off
        );
    }

    #[test]
    fn a_corrupt_state_file_stops_the_relay_instead_of_resetting_it() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("state.json");
        assert_eq!(
            load_state(&path).expect("missing is empty"),
            RelayState::default()
        );
        std::fs::write(&path, "{not json").expect("write");
        assert!(load_state(&path).is_err());
    }
}
