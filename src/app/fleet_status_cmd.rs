//! CLI handler for `shipyard runner fleet-status`.
//!
//! Read-only fleet aggregation for macOS VM CI: combine per-host capacity,
//! host-local `tartci doctor --reap --json` digests, and queued macOS age. The
//! command never deletes VMs or retargets runs; destructive cleanup stays inside
//! `tartci doctor --reap --fix` on each host.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Output};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::CliFailure;
use crate::capacity::{
    HostCapacity, HostClassConfig, any_unreadable, gather_configured_host_capacities,
    parse_host_classes, total_free,
};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::executor::ssh::shlex_quote;
use crate::output::write_json_envelope;

const FLEET_LANE_TARGET: &str = "macos";

pub(super) struct FleetStatusArgs {
    pub(super) repo: Option<String>,
    pub(super) target: String,
    pub(super) queued_age_threshold_secs: i64,
    pub(super) queue_run_limit: u32,
}

struct DoctorProbe {
    readable: bool,
    source: String,
    digest: Option<Value>,
}

struct HostFleetStatus {
    capacity: HostCapacity,
    doctor: DoctorProbe,
    supervisor_count: usize,
    fresh_supervisor_count: usize,
    stale_supervisor_count: usize,
    problem_count: usize,
    github_runner_count: usize,
    stale_vm_count: usize,
    routable: bool,
    problems: Vec<Value>,
    supervisors: Vec<Value>,
}

pub(super) fn fleet_status_command<W: Write>(
    args: FleetStatusArgs,
    config: &LoadedConfig,
    cwd: &Path,
    actions: &GitHubActions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let repo = super::runner_cmd::resolve_repo_slug(args.repo, cwd)?;
    let classes = parse_host_classes(&config.data).map_err(|e| CliFailure::new(2, e))?;
    if classes.is_empty() {
        return Err(CliFailure::new(
            1,
            "No [host_class.<name>] configured — fleet-status needs capacity hosts.",
        ));
    }

    let capacities =
        gather_configured_host_capacities(&config.data).map_err(|e| CliFailure::new(2, e))?;
    let mut hosts = Vec::new();
    for class in &classes {
        let capacity = capacities
            .iter()
            .find(|host| host.class == class.class)
            .cloned()
            .unwrap_or_else(|| HostCapacity {
                class: class.class.clone(),
                ssh: class.ssh.clone(),
                cap: class.cap,
                running: None,
                source: "capacity missing for host class".to_owned(),
            });
        let doctor = probe_doctor(class);
        // `--target` is a GitHub job-name substring, not a TartCI routing
        // label. FleetStatus is the macOS VM fleet command, so host health is
        // always scoped to the macOS lane even for custom job names such as
        // `required-apple-tests`.
        hosts.push(analyze_host(capacity, doctor, FLEET_LANE_TARGET));
    }

    let queue_run_limit = args.queue_run_limit.clamp(1, 100);
    let queue = queued_macos_summary(actions, &repo, &args.target, queue_run_limit).unwrap_or_else(
        |reason| QueuedSummary {
            readable: false,
            source: reason,
            count: 0,
            oldest_age_secs: None,
        },
    );

    let free = total_free(&capacities);
    let capacity_unreadable = any_unreadable(&capacities);
    let doctor_unreadable = hosts.iter().any(|host| !host.doctor.readable);
    let supervisor_unhealthy = hosts.iter().any(|host| {
        host.capacity.free() > 0 && host.doctor.readable && host.fresh_supervisor_count == 0
    });
    let problem_hosts = hosts.iter().any(|host| host.problem_count > 0);
    let routable_free_slots: u32 = hosts
        .iter()
        .filter(|host| host.routable)
        .map(|host| host.capacity.free())
        .sum();
    let queued_age_threshold_secs = args.queued_age_threshold_secs.max(0);
    let queued_age_with_capacity = queue
        .oldest_age_secs
        .is_some_and(|age| age >= queued_age_threshold_secs)
        && routable_free_slots > 0;
    let should_fail = capacity_unreadable
        || doctor_unreadable
        || supervisor_unhealthy
        || problem_hosts
        || !queue.readable
        || queued_age_with_capacity;

    if json {
        write_fleet_json(
            stdout,
            &FleetJsonView {
                repo: &repo,
                target: &args.target,
                free,
                routable_free_slots,
                capacity_unreadable,
                doctor_unreadable,
                supervisor_unhealthy,
                problem_hosts,
                queued_age_threshold_secs,
                queue_run_limit,
                queued_age_with_capacity,
                queue: &queue,
                hosts: &hosts,
            },
        )?;
    } else {
        write_fleet_text(
            stdout,
            &FleetTextView {
                repo: &repo,
                target: &args.target,
                free,
                routable_free_slots,
                queued_age_threshold_secs,
                should_fail,
                queue: &queue,
                hosts: &hosts,
            },
        );
    }

    Ok(if should_fail {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[allow(clippy::struct_excessive_bools)]
struct FleetJsonView<'a> {
    repo: &'a str,
    target: &'a str,
    free: u32,
    routable_free_slots: u32,
    capacity_unreadable: bool,
    doctor_unreadable: bool,
    supervisor_unhealthy: bool,
    problem_hosts: bool,
    queued_age_threshold_secs: i64,
    queue_run_limit: u32,
    queued_age_with_capacity: bool,
    queue: &'a QueuedSummary,
    hosts: &'a [HostFleetStatus],
}

fn write_fleet_json<W: Write>(stdout: &mut W, view: &FleetJsonView<'_>) -> Result<(), CliFailure> {
    let mut data = BTreeMap::new();
    data.insert("repo".to_owned(), Value::from(view.repo));
    data.insert("target".to_owned(), Value::from(view.target));
    data.insert("free_slots".to_owned(), Value::from(view.free));
    data.insert(
        "routable_free_slots".to_owned(),
        Value::from(view.routable_free_slots),
    );
    data.insert(
        "any_unreadable".to_owned(),
        Value::from(view.capacity_unreadable || view.doctor_unreadable || !view.queue.readable),
    );
    data.insert(
        "supervisor_unhealthy".to_owned(),
        Value::from(view.supervisor_unhealthy),
    );
    data.insert("problem_hosts".to_owned(), Value::from(view.problem_hosts));
    data.insert(
        "queued_age_threshold_secs".to_owned(),
        Value::from(view.queued_age_threshold_secs),
    );
    data.insert(
        "queue_run_limit".to_owned(),
        Value::from(view.queue_run_limit),
    );
    data.insert(
        "queued_age_with_capacity".to_owned(),
        Value::from(view.queued_age_with_capacity),
    );
    data.insert("queue".to_owned(), queue_to_json(view.queue));
    data.insert(
        "hosts".to_owned(),
        Value::from(view.hosts.iter().map(host_to_json).collect::<Vec<_>>()),
    );
    write_json_envelope(stdout, "runner.fleet-status", data)
        .map_err(|e| CliFailure::new(1, format!("failed to write JSON: {e}")))
}

struct FleetTextView<'a> {
    repo: &'a str,
    target: &'a str,
    free: u32,
    routable_free_slots: u32,
    queued_age_threshold_secs: i64,
    should_fail: bool,
    queue: &'a QueuedSummary,
    hosts: &'a [HostFleetStatus],
}

fn write_fleet_text<W: Write>(stdout: &mut W, view: &FleetTextView<'_>) {
    writeln!(
        stdout,
        "fleet-status repo={repo} target={} free={free} routable_free={routable_free_slots}",
        view.target,
        repo = view.repo,
        free = view.free,
        routable_free_slots = view.routable_free_slots
    )
    .ok();
    for host in view.hosts {
        let running = host
            .capacity
            .running
            .map_or_else(|| "?".to_owned(), |value| value.to_string());
        writeln!(
            stdout,
            "  {:<10} cap={} running={} free={} routable={} supervisors={}/{} stale={} problems={} source={}",
            host.capacity.class,
            host.capacity.cap,
            running,
            host.capacity.free(),
            host.routable,
            host.fresh_supervisor_count,
            host.supervisor_count,
            host.stale_supervisor_count,
            host.problem_count,
            host.doctor.source
        )
        .ok();
    }
    writeln!(
        stdout,
        "  queued macOS: count={} oldest_age_secs={} threshold={} readable={}",
        view.queue.count,
        view.queue
            .oldest_age_secs
            .map_or_else(|| "-".to_owned(), |age| age.to_string()),
        view.queued_age_threshold_secs,
        view.queue.readable
    )
    .ok();
    if view.should_fail {
        writeln!(
            stdout,
            "fleet-status: attention required (see fields above)"
        )
        .ok();
    }
}

fn probe_doctor(class: &HostClassConfig) -> DoctorProbe {
    let output = if let Some(host) = &class.ssh {
        Command::new("ssh")
            .args(ssh_probe_options())
            .arg(host)
            .arg(remote_tartci_command(class))
            .output()
    } else {
        let mut command = Command::new(&class.tartci_bin);
        if let Some(github_cli) = &class.github_cli {
            command.env("TARTCI_GH_CLI", github_cli);
        }
        if let Some(tart_home) = &class.tart_home {
            command.env("TART_HOME", tart_home);
        }
        command.args(["doctor", "--reap", "--json"]).output()
    };

    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return DoctorProbe {
                readable: false,
                source: if class.ssh.is_some() {
                    format!("ssh spawn failed: {error}")
                } else {
                    format!("`{}` spawn failed: {error}", class.tartci_bin)
                },
                digest: None,
            };
        }
    };

    doctor_probe_from_output(&output, if class.ssh.is_some() { "ssh" } else { "local" })
}

fn doctor_probe_from_output(output: &Output, base_source: &str) -> DoctorProbe {
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(digest) => {
            let source = if output.status.success() {
                base_source.to_owned()
            } else {
                let status = output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |code| format!("exit {code}"));
                format!("{base_source} (doctor {status})")
            };
            DoctorProbe {
                readable: true,
                source,
                digest: Some(digest),
            }
        }
        Err(error) => {
            if output.status.success() {
                return DoctorProbe {
                    readable: false,
                    source: format!("could not parse tartci doctor JSON: {error}"),
                    digest: None,
                };
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            let source = stderr
                .lines()
                .next()
                .filter(|line| !line.trim().is_empty())
                .map_or_else(
                    || format!("tartci doctor failed; JSON parse error: {error}"),
                    str::to_owned,
                );
            DoctorProbe {
                readable: false,
                source,
                digest: None,
            }
        }
    }
}

fn analyze_host(capacity: HostCapacity, doctor: DoctorProbe, target: &str) -> HostFleetStatus {
    let digest = doctor.digest.as_ref();
    let all_supervisors = digest
        .and_then(|value| value.get("supervisors"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let supervisors = all_supervisors
        .iter()
        .filter(|supervisor| item_matches_target(supervisor, target))
        .cloned()
        .collect::<Vec<_>>();
    let heartbeat_stale_secs = digest
        .and_then(|value| value.pointer("/config/heartbeat_stale_secs"))
        .and_then(Value::as_i64)
        .unwrap_or(900);
    let fresh_supervisor_count = supervisors
        .iter()
        .filter(|supervisor| supervisor_is_fresh(supervisor, heartbeat_stale_secs))
        .count();
    let supervisor_count = supervisors.len();
    let stale_supervisor_count = supervisor_count.saturating_sub(fresh_supervisor_count);
    let problems = digest
        .and_then(|value| value.get("problems"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|problem| problem_matches_target(problem, &all_supervisors, target))
        .cloned()
        .collect::<Vec<_>>();
    let problem_count = problems.len();
    let github_runner_count = digest
        .and_then(|value| value.get("github_runners"))
        .and_then(Value::as_array)
        .map_or(0, |runners| {
            runners
                .iter()
                .filter(|runner| item_matches_target(runner, target))
                .count()
        });
    let stale_vm_count = digest
        .and_then(|value| value.get("vms"))
        .and_then(Value::as_array)
        .map_or(0, |vms| {
            vms.iter()
                .filter(|vm| vm.get("stale").and_then(Value::as_bool).unwrap_or(false))
                .filter(|vm| {
                    item_or_related_supervisor_matches_target(vm, &all_supervisors, target)
                })
                .count()
        });
    let routable = capacity.readable()
        && capacity.free() > 0
        && doctor.readable
        && problem_count == 0
        && fresh_supervisor_count > 0;
    HostFleetStatus {
        capacity,
        doctor,
        supervisor_count,
        fresh_supervisor_count,
        stale_supervisor_count,
        problem_count,
        github_runner_count,
        stale_vm_count,
        routable,
        problems,
        supervisors,
    }
}

fn normalized_target(target: &str) -> String {
    let normalized = target.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "mac" | "darwin" => "macos".to_owned(),
        "win" => "windows".to_owned(),
        _ if normalized.starts_with("macos-") || normalized.starts_with("darwin-") => {
            "macos".to_owned()
        }
        _ if normalized.starts_with("windows-") => "windows".to_owned(),
        _ if normalized.starts_with("linux-") => "linux".to_owned(),
        _ => normalized,
    }
}

fn labels_match_target(labels: &Value, target: &str) -> bool {
    let target = normalized_target(target);
    let matches = |label: &str| {
        let label = label.trim().to_ascii_lowercase();
        label == target || (target == "macos" && label == "darwin")
    };
    if let Some(labels) = labels.as_str() {
        return labels.split(',').any(matches);
    }
    labels
        .as_array()
        .is_some_and(|labels| labels.iter().filter_map(Value::as_str).any(matches))
}

fn item_matches_target(item: &Value, target: &str) -> bool {
    item.get("labels")
        .is_some_and(|labels| labels_match_target(labels, target))
}

fn item_identity(item: &Value) -> Option<&str> {
    item.get("name")
        .or_else(|| item.get("vm"))
        .or_else(|| item.get("runner"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
}

fn item_or_related_supervisor_matches_target(
    item: &Value,
    supervisors: &[Value],
    target: &str,
) -> bool {
    if item_matches_target(item, target) {
        return true;
    }
    let Some(identity) = item_identity(item) else {
        return true;
    };
    let related = supervisors.iter().filter(|supervisor| {
        ["name", "vm", "runner"]
            .iter()
            .any(|key| supervisor.get(*key).and_then(Value::as_str) == Some(identity))
    });
    let mut found = false;
    for supervisor in related {
        found = true;
        if item_matches_target(supervisor, target) {
            return true;
        }
    }
    !found
}

fn problem_matches_target(problem: &Value, supervisors: &[Value], target: &str) -> bool {
    let Some(problem) = problem.as_str() else {
        return true;
    };
    let Some((_, identity)) = problem.split_once(':') else {
        return true;
    };
    let item = serde_json::json!({"name": identity});
    item_or_related_supervisor_matches_target(&item, supervisors, target)
}

fn supervisor_is_fresh(supervisor: &Value, stale_after_secs: i64) -> bool {
    let owner_alive = supervisor
        .get("owner_pid_alive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let heartbeat_age = supervisor
        .get("heartbeat_age_secs")
        .and_then(Value::as_i64)
        .unwrap_or(i64::MAX);
    owner_alive && heartbeat_age <= stale_after_secs
}

fn remote_tartci_command(class: &HostClassConfig) -> String {
    let mut parts = vec!["env".to_owned(), format!("PATH={REMOTE_PROBE_PATH}")];
    if let Some(tart_home) = &class.tart_home {
        parts.push(format!("TART_HOME={}", shlex_quote(tart_home)));
    }
    if let Some(github_cli) = &class.github_cli {
        parts.push(format!("TARTCI_GH_CLI={}", shlex_quote(github_cli)));
    }
    parts.push(shlex_quote(&class.tartci_bin));
    parts.extend(
        ["doctor", "--reap", "--json"]
            .iter()
            .map(|arg| shlex_quote(arg)),
    );
    parts.join(" ")
}

const REMOTE_PROBE_PATH: &str =
    "/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

fn ssh_probe_options() -> Vec<String> {
    [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

#[derive(Debug)]
struct QueuedSummary {
    readable: bool,
    source: String,
    count: usize,
    oldest_age_secs: Option<i64>,
}

fn queued_macos_summary(
    actions: &GitHubActions,
    repo: &str,
    target: &str,
    run_limit: u32,
) -> Result<QueuedSummary, String> {
    let per_page = run_limit.clamp(1, 100);
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/actions/runs?status=queued&per_page={per_page}"),
        ])
        .map_err(|error| format!("list queued runs failed: {error}"))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("could not parse queued runs JSON: {error}"))?;
    let Some(runs) = value.get("workflow_runs").and_then(Value::as_array) else {
        return Err("queued runs response missing workflow_runs".to_owned());
    };
    let mut count = 0usize;
    let mut oldest_age_secs: Option<i64> = None;
    let now = Utc::now();
    for run in runs {
        let Some(run_id) = run.get("id").and_then(Value::as_u64) else {
            continue;
        };
        if !run_has_queued_target_job(actions, repo, run_id, target) {
            continue;
        }
        count += 1;
        if let Some(created_at) = run.get("created_at").and_then(Value::as_str)
            && let Ok(ts) = DateTime::parse_from_rfc3339(created_at)
        {
            let age = (now - ts.with_timezone(&Utc)).num_seconds().max(0);
            oldest_age_secs = Some(oldest_age_secs.map_or(age, |oldest| oldest.max(age)));
        }
    }
    Ok(QueuedSummary {
        readable: true,
        source: "github".to_owned(),
        count,
        oldest_age_secs,
    })
}

fn run_has_queued_target_job(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
    target: &str,
) -> bool {
    let Ok(raw) = actions.run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/actions/runs/{run_id}/jobs"),
    ]) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    let target = target.to_ascii_lowercase();
    value
        .get("jobs")
        .and_then(Value::as_array)
        .is_some_and(|jobs| {
            jobs.iter().any(|job| {
                job.get("status").and_then(Value::as_str) == Some("queued")
                    && job
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.to_ascii_lowercase().contains(&target))
            })
        })
}

fn host_to_json(host: &HostFleetStatus) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("class".to_owned(), Value::from(host.capacity.class.clone()));
    m.insert(
        "ssh".to_owned(),
        host.capacity.ssh.clone().map_or(Value::Null, Value::from),
    );
    m.insert("cap".to_owned(), Value::from(host.capacity.cap));
    m.insert(
        "running".to_owned(),
        host.capacity.running.map_or(Value::Null, Value::from),
    );
    m.insert("free".to_owned(), Value::from(host.capacity.free()));
    m.insert(
        "capacity_readable".to_owned(),
        Value::from(host.capacity.readable()),
    );
    m.insert(
        "doctor_readable".to_owned(),
        Value::from(host.doctor.readable),
    );
    m.insert("source".to_owned(), Value::from(host.doctor.source.clone()));
    m.insert("routable".to_owned(), Value::from(host.routable));
    m.insert(
        "supervisor_count".to_owned(),
        Value::from(host.supervisor_count),
    );
    m.insert(
        "fresh_supervisor_count".to_owned(),
        Value::from(host.fresh_supervisor_count),
    );
    m.insert(
        "stale_supervisor_count".to_owned(),
        Value::from(host.stale_supervisor_count),
    );
    m.insert("problem_count".to_owned(), Value::from(host.problem_count));
    m.insert(
        "github_runner_count".to_owned(),
        Value::from(host.github_runner_count),
    );
    m.insert(
        "stale_vm_count".to_owned(),
        Value::from(host.stale_vm_count),
    );
    if host.doctor.digest.is_some() {
        m.insert("problems".to_owned(), Value::from(host.problems.clone()));
        m.insert(
            "supervisors".to_owned(),
            Value::from(host.supervisors.clone()),
        );
    }
    Value::Object(m)
}

fn queue_to_json(queue: &QueuedSummary) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("readable".to_owned(), Value::from(queue.readable));
    m.insert("source".to_owned(), Value::from(queue.source.clone()));
    m.insert("count".to_owned(), Value::from(queue.count));
    m.insert(
        "oldest_age_secs".to_owned(),
        queue.oldest_age_secs.map_or(Value::Null, Value::from),
    );
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_fresh_requires_alive_owner_and_recent_heartbeat() {
        let supervisor = serde_json::json!({
            "owner_pid_alive": true,
            "heartbeat_age_secs": 42
        });
        assert!(supervisor_is_fresh(&supervisor, 900));
        assert!(!supervisor_is_fresh(&supervisor, 10));
        let dead = serde_json::json!({
            "owner_pid_alive": false,
            "heartbeat_age_secs": 1
        });
        assert!(!supervisor_is_fresh(&dead, 900));
    }

    #[test]
    fn remote_tartci_command_sets_tart_home_and_quotes_binary() {
        let class = HostClassConfig {
            class: "m5".to_owned(),
            ssh: Some("m5-ci".to_owned()),
            cap: 2,
            tart_bin: "/opt/homebrew/bin/tart".to_owned(),
            tartci_bin: "/Users/ci user/.local/bin/tartci".to_owned(),
            github_cli: Some("ghapp".to_owned()),
            tart_home: Some("/Users/ci user/VMs".to_owned()),
            labels: Vec::new(),
        };
        assert_eq!(
            remote_tartci_command(&class),
            "env PATH=/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin TART_HOME='/Users/ci user/VMs' TARTCI_GH_CLI=ghapp '/Users/ci user/.local/bin/tartci' doctor --reap --json"
        );
    }

    #[test]
    fn remote_tartci_command_leaves_github_cli_unset_by_default() {
        let class = HostClassConfig {
            class: "studio".to_owned(),
            ssh: Some("studio".to_owned()),
            cap: 2,
            tart_bin: "tart".to_owned(),
            tartci_bin: "tartci".to_owned(),
            github_cli: None,
            tart_home: None,
            labels: Vec::new(),
        };

        assert!(!remote_tartci_command(&class).contains("TARTCI_GH_CLI"));
    }

    #[test]
    fn composite_platform_target_matches_lane_labels() {
        let labels = serde_json::json!(["self-hosted", "macOS", "ARM64"]);

        assert!(labels_match_target(&labels, "macos-arm64"));
        assert!(labels_match_target(&labels, "darwin-arm64"));
        assert!(!labels_match_target(&labels, "linux-arm64"));
    }

    #[test]
    fn fleet_lane_is_independent_of_custom_queue_job_name() {
        let custom_queue_target = "required-apple-tests";
        let labels = serde_json::json!(["self-hosted", "macOS", "ARM64"]);

        assert!(!labels_match_target(&labels, custom_queue_target));
        assert!(labels_match_target(&labels, FLEET_LANE_TARGET));
    }

    #[test]
    fn analyze_host_scopes_health_to_requested_target() {
        let doctor = DoctorProbe {
            readable: true,
            source: "test".to_owned(),
            digest: Some(serde_json::json!({
                "config": {"heartbeat_stale_secs": 900},
                "problems": ["suspect_live_owner_stale_heartbeat:linux-ephr-1"],
                "supervisors": [
                    {"runner":"pulp-vm-01", "vm":"pulp-vm-01-x", "labels":"self-hosted,macOS,ARM64", "owner_pid_alive":true, "heartbeat_age_secs":5},
                    {"runner":"linux-ephr-1", "vm":"linux-ephr-1", "labels":"self-hosted,Linux,ARM64", "owner_pid_alive":true, "heartbeat_age_secs":5000}
                ],
                "vms": [
                    {"name":"linux-ephr-1", "stale":true}
                ],
                "github_runners": [
                    {"name":"pulp-vm-01", "labels":["self-hosted", "macOS", "ARM64"]},
                    {"name":"linux-ephr-1", "labels":["self-hosted", "Linux", "ARM64"]}
                ]
            })),
        };
        let host = analyze_host(
            HostCapacity {
                class: "studio".to_owned(),
                ssh: None,
                cap: 2,
                running: Some(0),
                source: "test".to_owned(),
            },
            doctor,
            "macos",
        );
        assert!(host.routable);
        assert_eq!(host.problem_count, 0);
        assert_eq!(host.supervisor_count, 1);
        assert_eq!(host.github_runner_count, 1);
        assert_eq!(host.stale_vm_count, 0);
    }

    #[test]
    fn doctor_probe_parses_json_even_when_doctor_exits_nonzero() {
        let output = Command::new("sh")
            .args([
                "-c",
                "printf '%s' '{\"problems\":[{\"id\":\"stale_vm\"}]}' ; exit 1",
            ])
            .output()
            .expect("sh");
        let probe = doctor_probe_from_output(&output, "ssh");

        assert!(probe.readable);
        assert_eq!(probe.source, "ssh (doctor exit 1)");
        assert_eq!(
            probe
                .digest
                .as_ref()
                .and_then(|digest| digest.get("problems"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
            1
        );
    }
}
