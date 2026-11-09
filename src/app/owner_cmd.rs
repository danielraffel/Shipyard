//! `shipyard owner register|release` and `shipyard who --contact`: the
//! command side of [`crate::owner_registry`].

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{Value, json};

use super::CliFailure;
use super::cli::OwnerCommand;
use crate::capacity::parse_host_classes;
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::landability::gate;
use crate::output::write_json_envelope;
use crate::owner_registry::{self, OwnerRecord, PrBody, SessionIdentity, Source};
use crate::paths::RuntimePaths;
use crate::relay::config::{Address, RelayConfig};
use crate::relay::transport::{self, ProcessRunner, Routes};

/// Each GitHub read or write may take this long.
const GITHUB_TIMEOUT: Duration = Duration::from_mins(1);
/// Each remote host's record read must answer within this.
const HOST_READ_TIMEOUT: Duration = Duration::from_secs(20);
/// Each cmux probe may take this long.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

fn io_failure(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(1, error.to_string())
}

/// `[relay]` from this host's machine config.
pub(super) fn machine_relay_config(
    runtime_paths: &RuntimePaths,
) -> Result<RelayConfig, CliFailure> {
    let machine = LoadedConfig::load_machine_global_from_dir(runtime_paths.global_dir.clone())
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    crate::relay::config::parse(&machine.data)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))
}

/// This host's short name in owner records.
pub(super) fn local_host(config: &RelayConfig) -> String {
    owner_registry::resolve_host_name(config.host_name.as_deref(), &|name| {
        std::env::var(name).ok()
    })
}

fn this_session(config: &RelayConfig) -> SessionIdentity {
    SessionIdentity::from_env(local_host(config), &|name| std::env::var(name).ok())
}

/// A `gh` runner scoped to `repo`, through the configured GitHub client.
fn gh_for(cwd: &Path, config: &LoadedConfig, repo: &str) -> GitHubActions {
    GitHubActions::from_loaded_config(cwd, config).with_repo_override(repo)
}

fn run_gh(actions: &GitHubActions) -> impl Fn(&[String]) -> Result<String, String> + '_ {
    move |args: &[String]| {
        actions
            .run_gh_with_timeout(args, GITHUB_TIMEOUT)
            .map_err(|error| error.to_string())
    }
}

fn resolve_repo(
    repo: Option<String>,
    config: &LoadedConfig,
    cwd: &Path,
) -> Result<String, CliFailure> {
    repo.filter(|value| !value.trim().is_empty())
        .or_else(|| gate::resolve_repo(config, cwd))
        .ok_or_else(|| CliFailure::new(2, "No repo detected. Pass --repo OWNER/REPO."))
}

fn current_branch(cwd: &Path) -> Result<String, CliFailure> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(io_failure)?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || branch.is_empty() || branch == "HEAD" {
        return Err(CliFailure::new(
            2,
            "not on a branch; pass --pr to name the pull request",
        ));
    }
    Ok(branch)
}

/// Environment switch: `SHIPYARD_OWNER_REGISTER=0` stops `shipyard pr` and
/// `shipyard ship` from registering the pushing session (for automation that
/// pushes on another session's behalf).
pub(super) const REGISTER_ENV: &str = "SHIPYARD_OWNER_REGISTER";

/// Whether a push registers its session ([`REGISTER_ENV`]).
pub(super) fn registration_enabled(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env(REGISTER_ENV).is_none_or(|value| value.trim() != "0")
}

/// Register the session that just pushed `repo#number` through
/// `shipyard pr` / `shipyard ship`. Never fails the caller: the answer is a
/// line to print.
pub(super) fn register_after_push(
    cwd: &Path,
    config: &LoadedConfig,
    runtime_paths: &RuntimePaths,
    repo: &str,
    number: u64,
) -> String {
    let relay = match machine_relay_config(runtime_paths) {
        Ok(relay) => relay,
        Err(error) => return format!("⚠︎ Owner not registered: {}", error.message),
    };
    let identity = this_session(&relay);
    let actions = gh_for(cwd, config, repo);
    let gh = run_gh(&actions);
    match owner_registry::read_pr_body(&gh, repo, number) {
        Ok(pr) => {
            let record = identity.record(repo, number, Source::Ship, Utc::now());
            owner_registry::register(&gh, &runtime_paths.state_dir, &pr, record).line()
        }
        Err(error) => format!("⚠︎ Owner of {repo}#{number} not registered: {error}"),
    }
}

/// Run `shipyard owner ...`.
#[allow(clippy::too_many_lines)]
pub(super) fn owner_command<W: Write>(
    command: OwnerCommand,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config =
        LoadedConfig::load_from_cwd_with_global_dir(mode, cwd, runtime_paths.global_dir.clone())
            .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let relay = machine_relay_config(runtime_paths)?;
    let identity = this_session(&relay);
    let now = Utc::now();
    let mut registrations = Vec::new();
    match command {
        OwnerCommand::Register { pr, repo, all } => {
            if all {
                for record in owner_registry::load_all(&runtime_paths.state_dir) {
                    if !same_session(&record, &identity) {
                        continue;
                    }
                    let actions = gh_for(cwd, &config, &record.repo);
                    let gh = run_gh(&actions);
                    match owner_registry::read_pr_body(&gh, &record.repo, record.pr) {
                        Ok(body) if body.state == "OPEN" => {
                            let next =
                                identity.record(&record.repo, record.pr, Source::Manual, now);
                            registrations.push(Ok(owner_registry::register(
                                &gh,
                                &runtime_paths.state_dir,
                                &body,
                                next,
                            )));
                        }
                        Ok(_) => {}
                        Err(error) => registrations
                            .push(Err(format!("{}#{}: {error}", record.repo, record.pr))),
                    }
                }
            } else {
                let repo = resolve_repo(repo, &config, cwd)?;
                let actions = gh_for(cwd, &config, &repo);
                let gh = run_gh(&actions);
                let body = if let Some(number) = pr {
                    owner_registry::read_pr_body(&gh, &repo, number)
                } else {
                    let branch = current_branch(cwd)?;
                    owner_registry::find_pr_for_branch(&gh, &repo, &branch).and_then(|found| {
                        found.ok_or_else(|| format!("no open pull request has head {branch}"))
                    })
                }
                .map_err(|error| CliFailure::new(1, error))?;
                let record = identity.record(&repo, body.number, Source::Manual, now);
                registrations.push(Ok(owner_registry::register(
                    &gh,
                    &runtime_paths.state_dir,
                    &body,
                    record,
                )));
            }
        }
        OwnerCommand::Release { pr, repo } => {
            let repo = resolve_repo(repo, &config, cwd)?;
            let actions = gh_for(cwd, &config, &repo);
            let gh = run_gh(&actions);
            let body = owner_registry::read_pr_body(&gh, &repo, pr)
                .map_err(|error| CliFailure::new(1, error))?;
            let current = owner_registry::load(&runtime_paths.state_dir, &repo, pr)
                .or_else(|| owner_registry::parse_marker(&repo, pr, &body.body))
                .unwrap_or_else(|| identity.record(&repo, pr, Source::Manual, now));
            let released = OwnerRecord {
                released: true,
                registered_at: now,
                source: Source::Manual,
                ..current
            };
            registrations.push(Ok(owner_registry::register(
                &gh,
                &runtime_paths.state_dir,
                &body,
                released,
            )));
        }
    }
    let failed = registrations.iter().any(|r| match r {
        Ok(reg) => reg.stored_at.is_err(),
        Err(_) => true,
    });
    if json_output {
        let mut data = BTreeMap::new();
        data.insert(
            "registrations".to_owned(),
            Value::Array(
                registrations
                    .iter()
                    .map(|r| match r {
                        Ok(reg) => json!({
                            "record": reg.record,
                            "stored_at": reg.stored_at.as_ref().map(|p| p.display().to_string()).ok(),
                            "store_error": reg.stored_at.as_ref().err(),
                            "marker": match &reg.marker {
                                Ok(owner_registry::MarkerWrite::Written) => "written",
                                Ok(owner_registry::MarkerWrite::Unchanged) => "unchanged",
                                Err(_) => "failed",
                            },
                            "marker_error": reg.marker.as_ref().err(),
                        }),
                        Err(error) => json!({ "error": error }),
                    })
                    .collect(),
            ),
        );
        data.insert("addressable".to_owned(), json!(identity.addressable()));
        write_json_envelope(stdout, "owner", data).map_err(io_failure)?;
    } else {
        if registrations.is_empty() {
            writeln!(stdout, "▸ No pull request to register").map_err(io_failure)?;
        }
        for registration in &registrations {
            let line = match registration {
                Ok(reg) => reg.line(),
                Err(error) => format!("⚠︎ {error}"),
            };
            writeln!(stdout, "{line}").map_err(io_failure)?;
        }
        if !identity.addressable() {
            writeln!(
                stdout,
                "⚠︎ This session has no cmux surface (CMUX_SURFACE_ID); the relay will use the \
                 repository's fallbacks."
            )
            .map_err(io_failure)?;
        }
    }
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Whether `record` was written by the session `identity` describes.
fn same_session(record: &OwnerRecord, identity: &SessionIdentity) -> bool {
    (identity.session.is_some() && record.session == identity.session)
        || (identity.surface.is_some() && record.surface == identity.surface)
}

/// What `shipyard who --contact` was asked.
pub(super) struct ContactArgs {
    pub pr: u64,
    pub repo: Option<String>,
    pub json: bool,
}

/// GraphQL: the body and the newest comments, for the marker and the last
/// stamped verdict author.
const CONTACT_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){id number state body comments(last:30){nodes{body createdAt}}}}}";

/// The last verdict stamp's `host/session` (and agent) in `comments`.
pub(super) fn last_stamped_actor(comments: &[Value]) -> Option<String> {
    let stamp = regex::Regex::new(r"<!--\s*session:\s*([^\s>]+)(?:\s+agent:\s*([^\s>]+))?\s*-->")
        .expect("regex");
    comments
        .iter()
        .filter_map(|c| c.get("body").and_then(Value::as_str))
        .filter_map(|body| stamp.captures_iter(body).last())
        .next_back()
        .map(|found| match found.get(2) {
            Some(agent) => format!("{} agent {}", &found[1], agent.as_str()),
            None => found[1].to_owned(),
        })
}

fn remote_record(
    ssh: &str,
    state_dir: &str,
    repo: &str,
    pr: u64,
) -> Result<Option<OwnerRecord>, String> {
    let path = owner_registry::record_path(Path::new(state_dir), repo, pr)
        .ok_or_else(|| format!("invalid repository {repo:?}"))?;
    let script = format!(
        "f={}; [ ! -f \"$f\" ] || cat \"$f\"",
        transport::sh_quote(&path.to_string_lossy())
    );
    let mut command = Command::new("/usr/bin/ssh");
    command
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            ssh,
            &script,
        ])
        .stdin(Stdio::null());
    let output = crate::process::run_output_until(
        &mut command,
        Instant::now() + HOST_READ_TIMEOUT,
        format!("owner record read on {ssh}"),
    )
    .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "exited {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| format!("unreadable record: {error}"))
}

/// Run `shipyard who --contact`.
#[allow(clippy::too_many_lines)]
pub(super) fn who_contact_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    args: ContactArgs,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let started = Instant::now();
    let config =
        LoadedConfig::load_from_cwd_with_global_dir(mode, cwd, runtime_paths.global_dir.clone())
            .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let repo = resolve_repo(args.repo, &config, cwd)?;
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| CliFailure::new(2, format!("invalid repository {repo:?}")))?;
    let relay = machine_relay_config(runtime_paths)?;
    let here = local_host(&relay);

    // Every source, newest record wins.
    let mut sources: Vec<(String, OwnerRecord)> = Vec::new();
    let mut unreachable: BTreeMap<String, String> = BTreeMap::new();
    if let Some(record) = owner_registry::load(&runtime_paths.state_dir, &repo, args.pr) {
        sources.push((format!("store on {here}"), record));
    }
    let machine = LoadedConfig::load_machine_global_from_dir(runtime_paths.global_dir.clone())
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let readers: Vec<_> = parse_host_classes(&machine.data)
        .map_err(|error| CliFailure::new(2, error))?
        .into_iter()
        .filter_map(|class| {
            let ssh = class.ssh?;
            let state_dir = class.shipyard_state_dir?;
            let repo = repo.clone();
            let pr = args.pr;
            Some(std::thread::spawn(move || {
                (class.class, remote_record(&ssh, &state_dir, &repo, pr))
            }))
        })
        .collect();
    for reader in readers {
        match reader.join() {
            Ok((class, Ok(Some(record)))) => sources.push((format!("store on {class}"), record)),
            Ok((_, Ok(None))) => {}
            Ok((class, Err(error))) => {
                unreachable.insert(class, error);
            }
            Err(_) => return Err(CliFailure::new(1, "a host reader panicked")),
        }
    }
    let actions = gh_for(cwd, &config, &repo);
    let raw = actions
        .run_gh_with_timeout(
            &[
                "api".to_owned(),
                "graphql".to_owned(),
                "-f".to_owned(),
                format!("query={CONTACT_QUERY}"),
                "-f".to_owned(),
                format!("owner={owner}"),
                "-f".to_owned(),
                format!("name={name}"),
                "-F".to_owned(),
                format!("number={}", args.pr),
            ],
            GITHUB_TIMEOUT,
        )
        .map_err(|error| CliFailure::new(1, format!("pull request read failed: {error}")))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| CliFailure::new(1, format!("answer is not JSON: {error}")))?;
    let pr_node = value
        .pointer("/data/repository/pullRequest")
        .filter(|v| !v.is_null())
        .ok_or_else(|| CliFailure::new(1, format!("{repo}#{} not found", args.pr)))?;
    let body = PrBody {
        id: String::new(),
        number: args.pr,
        state: pr_node
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        body: pr_node
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
    };
    if let Some(record) = owner_registry::parse_marker(&repo, args.pr, &body.body) {
        sources.push(("pull request body marker".to_owned(), record));
    }
    let comments = pr_node
        .pointer("/comments/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let stamped = last_stamped_actor(&comments);

    let chosen = sources
        .iter()
        .max_by_key(|(_, record)| record.registered_at)
        .cloned();
    let runner = ProcessRunner {
        timeout: PROBE_TIMEOUT,
    };
    let mut routes = Routes::new();
    let probe = |address: &Address, routes: &mut Routes| {
        transport::resolve(&runner, &relay, &here, address, routes)
    };
    let owner_probe = chosen.as_ref().and_then(|(_, record)| {
        let surface = record.surface.clone()?;
        let address = Address {
            host: record.host.clone(),
            surface,
            socket: record.cmux_socket.clone(),
        };
        Some((address.clone(), probe(&address, &mut routes)))
    });
    let fallbacks = relay.fallbacks(&repo);
    let fallback_probes: Vec<(&str, Address, Option<String>)> = [
        ("lead", fallbacks.lead.clone()),
        ("repair_lane", fallbacks.repair_lane.clone()),
    ]
    .into_iter()
    .filter_map(|(tier, address)| {
        let address = address?;
        let socket = probe(&address, &mut routes);
        Some((tier, address, socket))
    })
    .collect();
    let elapsed = started.elapsed();

    if args.json {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), json!(repo));
        data.insert("pr".to_owned(), json!(args.pr));
        data.insert("state".to_owned(), json!(body.state));
        data.insert(
            "owner".to_owned(),
            chosen.as_ref().map_or(
                Value::Null,
                |(source, record)| json!({ "source": source, "record": record }),
            ),
        );
        data.insert(
            "probe".to_owned(),
            owner_probe
                .as_ref()
                .map_or(Value::Null, |(address, socket)| {
                    json!({
                        "host": address.host,
                        "surface": address.surface,
                        "reachable": socket.is_some(),
                        "socket": socket.as_deref().map(transport::socket_label),
                    })
                }),
        );
        data.insert(
            "fallbacks".to_owned(),
            Value::Array(
                fallback_probes
                    .iter()
                    .map(|(tier, address, socket)| {
                        json!({
                            "tier": tier,
                            "host": address.host,
                            "surface": address.surface,
                            "reachable": socket.is_some(),
                        })
                    })
                    .collect(),
            ),
        );
        data.insert("comment_fallback".to_owned(), json!(relay.comment_fallback));
        data.insert("last_stamped_actor".to_owned(), json!(stamped));
        data.insert(
            "sources".to_owned(),
            Value::Array(
                sources
                    .iter()
                    .map(|(source, record)| json!({ "source": source, "record": record }))
                    .collect(),
            ),
        );
        data.insert("hosts_unreachable".to_owned(), json!(unreachable));
        data.insert(
            "time_to_answer_ms".to_owned(),
            json!(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
        );
        write_json_envelope(stdout, "who.contact", data).map_err(io_failure)?;
        return Ok(ExitCode::SUCCESS);
    }
    writeln!(stdout, "{repo}#{} ({})", args.pr, body.state).map_err(io_failure)?;
    match &chosen {
        Some((source, record)) => {
            writeln!(
                stdout,
                "  owner: {} {} surface {} session {} (from {source}, registered {}){}",
                record.host,
                record.client.as_str(),
                record.surface.as_deref().unwrap_or("none"),
                record.session.as_deref().unwrap_or("?"),
                record.registered_at.format("%Y-%m-%dT%H:%M:%SZ"),
                if record.released {
                    "; RELEASED to the repair lane"
                } else {
                    ""
                }
            )
            .map_err(io_failure)?;
        }
        None => writeln!(
            stdout,
            "  owner: no record on any host read and no body marker"
        )
        .map_err(io_failure)?,
    }
    match &owner_probe {
        Some((address, Some(socket))) => writeln!(
            stdout,
            "  reachable: cmux {} {} via {}",
            address.host,
            address.surface,
            transport::socket_label(socket)
        ),
        Some((address, None)) => writeln!(
            stdout,
            "  UNREACHABLE: no cmux socket on {} knows {}",
            address.host, address.surface
        ),
        None => writeln!(stdout, "  no cmux address to probe"),
    }
    .map_err(io_failure)?;
    for (tier, address, socket) in &fallback_probes {
        writeln!(
            stdout,
            "  fallback {tier}: {}/{} {}",
            address.host,
            address.surface,
            if socket.is_some() {
                "reachable"
            } else {
                "UNREACHABLE"
            }
        )
        .map_err(io_failure)?;
    }
    writeln!(
        stdout,
        "  fallback comment: {}",
        if relay.comment_fallback { "on" } else { "off" }
    )
    .map_err(io_failure)?;
    if let Some(actor) = stamped {
        writeln!(stdout, "  last stamped verdict author: {actor}").map_err(io_failure)?;
    }
    for (class, error) in &unreachable {
        writeln!(stdout, "  host unreachable: {class}: {error}").map_err(io_failure)?;
    }
    writeln!(stdout, "answered in {:.1}s", elapsed.as_secs_f64()).map_err(io_failure)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    fn record(session: Option<&str>, surface: Option<&str>) -> OwnerRecord {
        OwnerRecord {
            schema: 1,
            repo: "o/r".to_owned(),
            pr: 1,
            host: "m5".to_owned(),
            client: owner_registry::Client::Codex,
            cmux_socket: None,
            surface: surface.map(str::to_owned),
            agent: None,
            session: session.map(str::to_owned),
            registered_at: Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap(),
            source: Source::Ship,
            released: false,
        }
    }

    #[test]
    fn register_all_matches_the_session_or_the_surface_never_an_absent_one() {
        let me = SessionIdentity {
            host: "m5".to_owned(),
            client: None,
            cmux_socket: None,
            surface: Some("S1".to_owned()),
            agent: None,
            session: Some("sess".to_owned()),
        };
        assert!(same_session(&record(Some("sess"), None), &me));
        assert!(same_session(&record(None, Some("S1")), &me));
        assert!(!same_session(&record(Some("other"), Some("S2")), &me));
        let anonymous = SessionIdentity::default();
        assert!(
            !same_session(&record(None, None), &anonymous),
            "two unknown sessions are not the same session"
        );
    }

    #[test]
    fn a_push_registers_unless_the_environment_opts_out() {
        assert!(registration_enabled(&|_| None));
        assert!(registration_enabled(&|_| Some("1".to_owned())));
        assert!(!registration_enabled(
            &|name| (name == REGISTER_ENV).then(|| "0".to_owned())
        ));
    }

    #[test]
    fn the_last_stamped_verdict_author_is_read_from_the_newest_stamp() {
        let comments = vec![
            json!({"body": "a\n<!-- session: m1/old -->\nverdict: HOLD head:x"}),
            json!({"body": "plain comment"}),
            json!({"body": "b\n<!-- session: m3/new agent: infra-review2 -->\nverdict: APPROVE head:y"}),
        ];
        assert_eq!(
            last_stamped_actor(&comments).as_deref(),
            Some("m3/new agent infra-review2")
        );
        assert_eq!(last_stamped_actor(&[json!({"body": "none"})]), None);
    }
}
