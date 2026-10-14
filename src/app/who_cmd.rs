//! `shipyard who --pr N`: attribute a pull request's queue and merge events to
//! the host and agent session that caused them. See [`crate::who`].

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::CliFailure;
use crate::capacity::parse_host_classes;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::landability::gate;
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::who::{
    Attribution, LedgerEntry, TIMELINE_QUERY, attribute, parse_ledgers, timeline_events,
};

/// Each GitHub read may take this long.
const GITHUB_READ_TIMEOUT: Duration = Duration::from_mins(1);
/// Each host's ledgers must arrive within this.
const HOST_READ_TIMEOUT_SECS: u64 = 20;
/// Separates the two ledgers in a host's answer.
const MARK: &str = "@@shipyard-who-ledger ";

/// What the caller asked for.
pub(super) struct WhoArgs {
    pub pr: u64,
    pub repo: Option<String>,
    pub json: bool,
}

/// One host's read.
struct HostRead {
    class: String,
    result: Result<(String, String), String>,
}

/// The shell that prints both ledgers on a host, each after a marker line.
fn read_script() -> String {
    format!(
        "d=\"${{XDG_STATE_HOME:-$HOME/.local/state}}/shipyard\"; \
         for f in queue-arms queue-removals; do printf '{MARK}%s\\n' \"$f\"; \
         [ ! -f \"$d/$f.jsonl\" ] || cat \"$d/$f.jsonl\"; done"
    )
}

fn split_ledgers(text: &str) -> Result<(String, String), String> {
    let mut arms = None;
    let mut removals = None;
    let mut current: Option<&mut Option<String>> = None;
    for line in text.lines() {
        if let Some(name) = line.strip_prefix(MARK) {
            current = match name.trim() {
                "queue-arms" => Some(&mut arms),
                "queue-removals" => Some(&mut removals),
                _ => None,
            };
            if let Some(slot) = current.as_deref_mut() {
                *slot = Some(String::new());
            }
            continue;
        }
        if let Some(Some(buffer)) = current.as_deref_mut() {
            buffer.push_str(line);
            buffer.push('\n');
        }
    }
    match (arms, removals) {
        (Some(arms), Some(removals)) => Ok((arms, removals)),
        _ => Err("the host answered without both ledgers".to_owned()),
    }
}

fn ssh_binary() -> std::path::PathBuf {
    ["/usr/bin/ssh", "/bin/ssh"]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| std::path::PathBuf::from("ssh"))
}

fn read_host(class: String, ssh: Option<&str>) -> HostRead {
    let mut command = if let Some(host) = ssh {
        let mut command = Command::new(ssh_binary());
        command
            .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
            .arg(host)
            .arg(read_script());
        command
    } else {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &read_script()]);
        command
    };
    command.stdin(Stdio::null());
    let deadline = Instant::now() + Duration::from_secs(HOST_READ_TIMEOUT_SECS);
    let label = format!("ledger read on host class {class}");
    let result = crate::process::run_output_until(&mut command, deadline, &label)
        .map_err(|error| error.to_string())
        .and_then(|output| {
            if output.status.success() {
                split_ledgers(&String::from_utf8_lossy(&output.stdout))
            } else {
                Err(format!(
                    "exited {}: {}",
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        });
    HostRead { class, result }
}

/// Run `shipyard who`.
pub(super) fn who_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    args: WhoArgs,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let started = Instant::now();
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let repo = args
        .repo
        .filter(|value| !value.trim().is_empty())
        .or_else(|| gate::resolve_repo(&config, cwd))
        .ok_or_else(|| CliFailure::new(2, "No repo detected. Pass --repo OWNER/REPO."))?;
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| CliFailure::new(2, format!("invalid repository {repo:?}")))?;
    // Hosts are machine policy, as for fleet-update: never a repository's.
    let machine = LoadedConfig::load_machine_global_from_dir(runtime_paths.global_dir.clone())
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let mut hosts: Vec<(String, Option<String>)> = parse_host_classes(&machine.data)
        .map_err(|error| CliFailure::new(2, error))?
        .into_iter()
        .map(|class| (class.class, class.ssh))
        .collect();
    if !hosts.iter().any(|(_, ssh)| ssh.is_none()) {
        hosts.push(("local".to_owned(), None));
    }
    let readers: Vec<_> = hosts
        .into_iter()
        .map(|(class, ssh)| std::thread::spawn(move || read_host(class, ssh.as_deref())))
        .collect();

    let actions = crate::cloud::GitHubActions::from_loaded_config(cwd, &config);
    let timeline = actions
        .run_gh_with_timeout(
            &[
                "api".to_owned(),
                "graphql".to_owned(),
                "-f".to_owned(),
                format!("query={TIMELINE_QUERY}"),
                "-F".to_owned(),
                format!("owner={owner}"),
                "-F".to_owned(),
                format!("name={name}"),
                "-F".to_owned(),
                format!("number={}", args.pr),
            ],
            GITHUB_READ_TIMEOUT,
        )
        .map_err(|error| CliFailure::new(1, format!("timeline read failed: {error}")))?;
    let timeline: Value = serde_json::from_str(&timeline)
        .map_err(|error| CliFailure::new(1, format!("timeline answer is not JSON: {error}")))?;
    let events = timeline_events(&timeline).map_err(|error| CliFailure::new(1, error))?;

    let ledgers = collect(readers)?;
    let found = attribute(&repo, args.pr, &events, &ledgers.entries);
    let elapsed = started.elapsed();
    let report = Report {
        repo: &repo,
        pr: args.pr,
        found: &found,
        ledgers: &ledgers,
        elapsed,
    };
    if args.json {
        render_json(&report, stdout)
    } else {
        render_text(&report, stdout)
    }
}

/// Every host's ledgers, merged.
struct Ledgers {
    entries: Vec<LedgerEntry>,
    read_hosts: Vec<String>,
    unreachable: BTreeMap<String, String>,
    skipped: usize,
}

fn collect(readers: Vec<std::thread::JoinHandle<HostRead>>) -> Result<Ledgers, CliFailure> {
    let mut ledgers = Ledgers {
        entries: Vec::new(),
        read_hosts: Vec::new(),
        unreachable: BTreeMap::new(),
        skipped: 0,
    };
    for reader in readers {
        let read = reader
            .join()
            .map_err(|_| CliFailure::new(1, "a host reader panicked"))?;
        match read.result {
            Ok((arms, removals)) => {
                let (found, bad) = parse_ledgers(&read.class, &arms, &removals);
                ledgers.entries.extend(found);
                ledgers.skipped += bad;
                ledgers.read_hosts.push(read.class);
            }
            Err(error) => {
                ledgers.unreachable.insert(read.class, error);
            }
        }
    }
    Ok(ledgers)
}

struct Report<'a> {
    repo: &'a str,
    pr: u64,
    found: &'a [Attribution],
    ledgers: &'a Ledgers,
    elapsed: Duration,
}

fn render_json<W: Write>(report: &Report<'_>, stdout: &mut W) -> Result<ExitCode, CliFailure> {
    let (repo, found, ledgers, elapsed) =
        (report.repo, report.found, report.ledgers, report.elapsed);
    let (read_hosts, unreachable, skipped) =
        (&ledgers.read_hosts, &ledgers.unreachable, ledgers.skipped);
    let args_pr = report.pr;

    {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), serde_json::json!(repo));
        data.insert("pr".to_owned(), serde_json::json!(args_pr));
        data.insert(
            "events".to_owned(),
            serde_json::json!(
                found
                    .iter()
                    .map(|row| serde_json::json!({
                        "event": row.event,
                        "who": row.who(),
                        "entry": row.entry,
                        "head_match": row.head_match,
                    }))
                    .collect::<Vec<_>>()
            ),
        );
        data.insert("hosts_read".to_owned(), serde_json::json!(read_hosts));
        data.insert(
            "hosts_unreachable".to_owned(),
            serde_json::json!(unreachable),
        );
        data.insert(
            "malformed_ledger_lines".to_owned(),
            serde_json::json!(skipped),
        );
        data.insert(
            "time_to_answer_ms".to_owned(),
            serde_json::json!(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
        );
        write_json_envelope(stdout, "who", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn render_text<W: Write>(report: &Report<'_>, stdout: &mut W) -> Result<ExitCode, CliFailure> {
    let (repo, found, ledgers, elapsed) =
        (report.repo, report.found, report.ledgers, report.elapsed);
    let (read_hosts, unreachable, skipped) =
        (&ledgers.read_hosts, &ledgers.unreachable, ledgers.skipped);
    let io = |error: std::io::Error| CliFailure::new(1, error.to_string());
    writeln!(stdout, "{repo}#{}", report.pr).map_err(io)?;
    if found.is_empty() {
        writeln!(stdout, "  no queue or merge events on the timeline").map_err(io)?;
    }
    for row in found {
        let head = row
            .event
            .head
            .as_deref()
            .map_or("?", |head| head.get(..12).unwrap_or(head));
        let detail = row.entry.as_ref().map_or_else(
            || "no ledger entry on any host read".to_owned(),
            |entry| {
                format!(
                    "ledger {} {}{}",
                    entry.at.format("%H:%M:%SZ"),
                    entry.verdict,
                    match row.head_match {
                        Some(false) => ", HEAD DIFFERS",
                        None => ", head unverified",
                        Some(true) => "",
                    }
                )
            },
        );
        writeln!(
            stdout,
            "  {} {:<27} head {head}  {}  ({detail})",
            row.event.at.format("%Y-%m-%dT%H:%M:%SZ"),
            row.event.kind,
            row.who()
        )
        .map_err(io)?;
    }
    writeln!(stdout, "hosts read: {}", read_hosts.join(", ")).map_err(io)?;
    for (class, error) in unreachable {
        writeln!(stdout, "host unreachable: {class}: {error}").map_err(io)?;
    }
    if skipped > 0 {
        writeln!(stdout, "malformed ledger lines skipped: {skipped}").map_err(io)?;
    }
    writeln!(stdout, "answered in {:.1}s", elapsed.as_secs_f64()).map_err(io)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::{MARK, read_script, split_ledgers};

    #[test]
    fn a_hosts_answer_splits_into_its_two_ledgers() {
        let text = format!("{MARK}queue-arms\n{{\"a\":1}}\n{{\"a\":2}}\n{MARK}queue-removals\n");
        let (arms, removals) = split_ledgers(&text).expect("split");
        assert_eq!(arms.lines().count(), 2);
        assert!(removals.is_empty());
        assert!(
            split_ledgers("{\"a\":1}\n").is_err(),
            "no markers is not two empty ledgers"
        );
    }

    #[test]
    fn the_read_script_reads_both_ledgers_from_the_guards_state_dir() {
        // The script runs on remote hosts; its execution is proven live, and
        // here its text is pinned to the paths queue-arm-guard writes.
        let script = read_script();
        assert!(script.contains("${XDG_STATE_HOME:-$HOME/.local/state}/shipyard"));
        assert!(script.contains("for f in queue-arms queue-removals;"));
        assert!(script.contains(&format!("printf '{MARK}%s\\n'")));
        assert!(script.contains("cat \"$d/$f.jsonl\""));
    }
}
