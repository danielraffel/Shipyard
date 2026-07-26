//! CLI handler for `shipyard runner fleet-status`.
//!
//! Read-only fleet aggregation for macOS VM CI: combine per-host capacity,
//! host-local `tartci doctor --reap --json` digests, and queued macOS age. The
//! command never deletes VMs or retargets runs; destructive cleanup stays inside
//! `tartci doctor --reap --fix` on each host.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::CliFailure;
use crate::capacity::{
    HostCapacity, HostClassConfig, any_unreadable, gather_configured_host_capacities,
    parse_host_classes, total_free,
};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::executor::ssh::shlex_quote;
use crate::merge_queue_liveness::{
    ActiveRunObservation, CheckObservation, JobObservation, MergeQueueLivenessInputs,
    MergeQueueLivenessReport, ReleaseLivenessReport, assess_merge_queue_liveness,
    assess_release_liveness, parse_check_observations, parse_merge_queue_entries,
};
use crate::output::write_json_envelope;

const FLEET_LANE_TARGET: &str = "macos";

pub(super) struct FleetStatusArgs {
    pub(super) repo: Option<String>,
    pub(super) base: String,
    pub(super) target: String,
    pub(super) queued_age_threshold_secs: i64,
    pub(super) queue_run_limit: u32,
    pub(super) merge_queue_stall_threshold_secs: i64,
    pub(super) release_stale_threshold_secs: i64,
}

const OBSERVATION_PAGE_SIZE: usize = 100;
const OBSERVATION_MAX_PAGES: u32 = 5;
// GitHub exposes jobs only per workflow run. Keep each recurring fleet tick to
// a predictable REST budget: two run-list requests plus at most this many job
// requests. A larger active set is reported as truncated rather than silently
// driving the watchdog into its own rate-limit wedge.
const MAX_DETAILED_WORKFLOW_RUNS: u32 = 50;
const MAX_ENROLLMENT_LOOKUPS_PER_TICK: usize = 25;
const MERGE_QUEUE_QUERY: &str = "query($owner:String!,$name:String!,$branch:String!,$cursor:String){repository(owner:$owner,name:$name){mergeQueue(branch:$branch){entries(first:100,after:$cursor){nodes{position enqueuedAt headCommit{oid} pullRequest{number}} pageInfo{hasNextPage endCursor}}}}}";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ObservationReason {
    GitHubAuthFailed,
    GitHubRateLimited,
    GitHubObservationFailed,
    ObservationTruncated,
    AuxiliaryObservationUnavailable,
    ReleaseStale,
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

#[allow(clippy::too_many_lines)]
pub(super) fn fleet_status_command<W: Write>(
    args: FleetStatusArgs,
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
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

    let queue_run_limit = args.queue_run_limit.clamp(1, MAX_DETAILED_WORKFLOW_RUNS);
    let observed_runs = fetch_observed_workflow_runs(actions, &repo, queue_run_limit);
    let queue = observed_runs.as_ref().map_or_else(
        |reason| QueuedSummary {
            readable: false,
            source: reason.clone(),
            count: 0,
            oldest_age_secs: None,
        },
        |observed| queued_macos_summary(&observed.runs, &args.target),
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
    let eligible_host_classes = classes
        .iter()
        .map(|class| class.class.clone())
        .collect::<Vec<_>>();
    let required_contexts = required_status_checks(config);
    let merge_queue = observed_runs
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|observed| {
            inspect_merge_queue_liveness(
                actions,
                &repo,
                &args.base,
                state_dir,
                &required_contexts,
                &eligible_host_classes,
                routable_free_slots,
                args.merge_queue_stall_threshold_secs,
                &observed.runs,
                observed.truncated,
            )
        })
        .unwrap_or_else(|reason| MergeQueueProbe {
            readable: false,
            reason_codes: vec![classify_observation_error(&reason)],
            source: reason,
            report: None,
        });
    let release = inspect_release_liveness(
        actions,
        &repo,
        &args.base,
        args.release_stale_threshold_secs,
    )
    .unwrap_or_else(|reason| {
        let no_releases = reason.contains("HTTP 404") || reason.contains("404 Not Found");
        ReleaseProbe {
            readable: no_releases,
            source: if no_releases {
                "github (no releases)".to_owned()
            } else {
                reason.clone()
            },
            report: None,
            reason_codes: if no_releases {
                Vec::new()
            } else {
                vec![classify_observation_error(&reason)]
            },
        }
    });
    let queued_age_threshold_secs = args.queued_age_threshold_secs.max(0);
    let queued_age_with_capacity = queue
        .oldest_age_secs
        .is_some_and(|age| age >= queued_age_threshold_secs)
        && routable_free_slots > 0;
    let observation_reason_codes = observation_reason_codes(&merge_queue, &release);
    let observation_incomplete = !merge_queue.readable
        || !release.readable
        || observation_reason_codes.iter().any(|reason| {
            matches!(
                reason,
                ObservationReason::GitHubAuthFailed
                    | ObservationReason::GitHubRateLimited
                    | ObservationReason::GitHubObservationFailed
                    | ObservationReason::ObservationTruncated
            )
        });
    let should_fail = capacity_unreadable
        || doctor_unreadable
        || supervisor_unhealthy
        || problem_hosts
        || !queue.readable
        || queued_age_with_capacity
        || !merge_queue.readable
        || !release.readable
        || observation_incomplete
        || merge_queue
            .report
            .as_ref()
            .is_some_and(MergeQueueLivenessReport::needs_attention)
        || release
            .report
            .as_ref()
            .is_some_and(|report| report.stale_with_unreleased_commits);

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
                base: &args.base,
                merge_queue_stall_threshold_secs: args.merge_queue_stall_threshold_secs.max(0),
                merge_queue: &merge_queue,
                release_stale_threshold_secs: args.release_stale_threshold_secs.max(0),
                release: &release,
                hosts: &hosts,
                observation_reason_codes: &observation_reason_codes,
                observation_incomplete,
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
                base: &args.base,
                merge_queue_stall_threshold_secs: args.merge_queue_stall_threshold_secs.max(0),
                merge_queue: &merge_queue,
                release_stale_threshold_secs: args.release_stale_threshold_secs.max(0),
                release: &release,
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
    base: &'a str,
    merge_queue_stall_threshold_secs: i64,
    merge_queue: &'a MergeQueueProbe,
    release_stale_threshold_secs: i64,
    release: &'a ReleaseProbe,
    hosts: &'a [HostFleetStatus],
    observation_reason_codes: &'a [ObservationReason],
    observation_incomplete: bool,
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
        Value::from(
            view.capacity_unreadable
                || view.doctor_unreadable
                || !view.queue.readable
                || view.observation_incomplete,
        ),
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
    data.insert("base".to_owned(), Value::from(view.base));
    data.insert(
        "merge_queue_stall_threshold_secs".to_owned(),
        Value::from(view.merge_queue_stall_threshold_secs),
    );
    data.insert(
        "merge_queue".to_owned(),
        merge_queue_to_json(view.merge_queue),
    );
    data.insert(
        "release_stale_threshold_secs".to_owned(),
        Value::from(view.release_stale_threshold_secs),
    );
    data.insert("release".to_owned(), release_to_json(view.release));
    data.insert(
        "observation_reason_codes".to_owned(),
        serde_json::to_value(view.observation_reason_codes).expect("observation reasons serialize"),
    );
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
    base: &'a str,
    merge_queue_stall_threshold_secs: i64,
    merge_queue: &'a MergeQueueProbe,
    release_stale_threshold_secs: i64,
    release: &'a ReleaseProbe,
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
    write_merge_queue_text(stdout, view);
    write_release_text(stdout, view);
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

struct MergeQueueProbe {
    readable: bool,
    source: String,
    report: Option<MergeQueueLivenessReport>,
    reason_codes: Vec<ObservationReason>,
}

struct ReleaseProbe {
    readable: bool,
    source: String,
    report: Option<ReleaseLivenessReport>,
    reason_codes: Vec<ObservationReason>,
}

struct ObservedRuns {
    runs: Vec<ActiveRunObservation>,
    truncated: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct EnrollmentSnapshot {
    entries: Vec<EnrollmentSnapshotEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct EnrollmentSnapshotEntry {
    pr: u64,
    head_sha: Option<String>,
    observed_at: String,
    #[serde(default)]
    auto_merge_cleared: bool,
    #[serde(default)]
    last_checked_at: Option<String>,
}

fn required_status_checks(config: &LoadedConfig) -> Vec<String> {
    config
        .get("governance.required_status_checks")
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn fetch_merge_queue_entries(
    actions: &GitHubActions,
    owner: &str,
    name: &str,
    base: &str,
    max_pages: u32,
) -> Result<(Vec<crate::merge_queue_liveness::MergeQueueEntry>, bool), String> {
    let mut cursor: Option<String> = None;
    let mut entries = Vec::new();
    for page in 1..=max_pages.max(1) {
        let mut args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={MERGE_QUEUE_QUERY}"),
            "-F".to_owned(),
            format!("owner={owner}"),
            "-F".to_owned(),
            format!("name={name}"),
            "-F".to_owned(),
            format!("branch={base}"),
        ];
        if let Some(cursor) = &cursor {
            args.extend(["-F".to_owned(), format!("cursor={cursor}")]);
        }
        let raw = actions
            .run_gh(&args)
            .map_err(|error| format!("inspect merge queue page {page} failed: {error}"))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("could not parse merge-queue page {page}: {error}"))?;
        entries.extend(parse_merge_queue_entries(&value)?);
        let page_info = value.pointer("/data/repository/mergeQueue/entries/pageInfo");
        let has_next = page_info
            .and_then(|info| info.get("hasNextPage"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !has_next {
            entries.sort_by_key(|entry| entry.position);
            return Ok((entries, false));
        }
        cursor = page_info
            .and_then(|info| info.get("endCursor"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            return Err("merge-queue pagination says more pages but has no endCursor".to_owned());
        }
    }
    entries.sort_by_key(|entry| entry.position);
    Ok((entries, true))
}

fn enrollment_snapshot_path(state_dir: &Path, repo: &str, base: &str) -> PathBuf {
    let digest = format!("{:x}", Sha256::digest(format!("{repo}\0{base}").as_bytes()));
    let key = format!("{}-{}", repo.replace('/', "-"), &digest[..24]);
    state_dir.join("fleet-liveness").join(format!("{key}.json"))
}

#[allow(clippy::too_many_lines)]
fn reconcile_enrollment_snapshot(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    state_dir: &Path,
    entries: &[crate::merge_queue_liveness::MergeQueueEntry],
) -> Result<(Vec<u64>, bool), String> {
    let path = enrollment_snapshot_path(state_dir, repo, base);
    let previous = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<EnrollmentSnapshot>(&raw)
            .map_err(|error| format!("parse fleet enrollment snapshot failed: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => EnrollmentSnapshot::default(),
        Err(error) => return Err(format!("read fleet enrollment snapshot failed: {error}")),
    };
    let current = entries
        .iter()
        .map(|entry| entry.pr)
        .collect::<BTreeSet<_>>();
    let mut cleared = Vec::new();
    let mut retained = Vec::new();
    let mut candidates = previous
        .entries
        .into_iter()
        .filter(|entry| !current.contains(&entry.pr))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.last_checked_at
            .as_deref()
            .unwrap_or(&left.observed_at)
            .cmp(
                right
                    .last_checked_at
                    .as_deref()
                    .unwrap_or(&right.observed_at),
            )
    });
    let mut truncated = false;
    for (index, previous_entry) in candidates.into_iter().enumerate() {
        if index >= MAX_ENROLLMENT_LOOKUPS_PER_TICK {
            if previous_entry.auto_merge_cleared {
                cleared.push(previous_entry.pr);
            }
            retained.push(previous_entry);
            truncated = true;
            continue;
        }
        let raw = actions
            .run_gh(&[
                "api".to_owned(),
                format!("repos/{repo}/pulls/{}", previous_entry.pr),
            ])
            .map_err(|error| {
                format!(
                    "inspect prior queue PR #{} enrollment failed: {error}",
                    previous_entry.pr
                )
            })?;
        let pull: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("could not parse prior queue PR JSON: {error}"))?;
        if pull.get("state").and_then(Value::as_str) == Some("open") {
            let pull_base = pull
                .pointer("/base/ref")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "prior queue PR #{} response missing base.ref",
                        previous_entry.pr
                    )
                })?;
            if pull_base != base {
                continue;
            }
            retained.push(EnrollmentSnapshotEntry {
                pr: previous_entry.pr,
                head_sha: previous_entry.head_sha,
                observed_at: previous_entry.observed_at,
                auto_merge_cleared: pull.get("auto_merge").is_none_or(Value::is_null),
                last_checked_at: Some(Utc::now().to_rfc3339()),
            });
            if pull.get("auto_merge").is_none_or(Value::is_null) {
                cleared.push(previous_entry.pr);
            }
        }
    }
    let snapshot = EnrollmentSnapshot {
        entries: entries
            .iter()
            .map(|entry| EnrollmentSnapshotEntry {
                pr: entry.pr,
                head_sha: entry.head_sha.clone(),
                observed_at: Utc::now().to_rfc3339(),
                auto_merge_cleared: false,
                last_checked_at: None,
            })
            .chain(retained)
            .collect(),
    };
    let parent = path
        .parent()
        .ok_or_else(|| "fleet snapshot path has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create fleet snapshot directory failed: {error}"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("create fleet snapshot temp file failed: {error}"))?;
    serde_json::to_writer(&mut temp, &snapshot)
        .map_err(|error| format!("serialize fleet snapshot failed: {error}"))?;
    temp.persist(&path)
        .map_err(|error| format!("persist fleet snapshot failed: {error}"))?;
    cleared.sort_unstable();
    Ok((cleared, truncated))
}

fn classify_observation_error(reason: &str) -> ObservationReason {
    let reason = reason.to_ascii_lowercase();
    if reason.contains("rate limit") || reason.contains("secondary rate") {
        ObservationReason::GitHubRateLimited
    } else if reason.contains("authentication")
        || reason.contains("bad credentials")
        || reason.contains("http 401")
        || reason.contains("http 403")
    {
        ObservationReason::GitHubAuthFailed
    } else {
        ObservationReason::GitHubObservationFailed
    }
}

fn observation_reason_codes(
    merge_queue: &MergeQueueProbe,
    release: &ReleaseProbe,
) -> Vec<ObservationReason> {
    let mut reasons = merge_queue
        .reason_codes
        .iter()
        .chain(release.reason_codes.iter())
        .copied()
        .collect::<Vec<_>>();
    if release
        .report
        .as_ref()
        .is_some_and(|report| report.stale_with_unreleased_commits)
    {
        reasons.push(ObservationReason::ReleaseStale);
    }
    reasons.sort_by_key(|reason| format!("{reason:?}"));
    reasons.dedup();
    reasons
}

#[allow(clippy::too_many_arguments)]
fn inspect_merge_queue_liveness(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    state_dir: &Path,
    required_contexts: &[String],
    eligible_host_classes: &[String],
    routable_free_slots: u32,
    stall_threshold_secs: i64,
    active_runs: &[ActiveRunObservation],
    observation_truncated: bool,
) -> Result<MergeQueueProbe, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository slug `{repo}`"))?;
    let (entries, queue_truncated) =
        fetch_merge_queue_entries(actions, owner, name, base, OBSERVATION_MAX_PAGES)?;
    let (enrollment_cleared_prs, enrollment_truncated) =
        reconcile_enrollment_snapshot(actions, repo, base, state_dir, &entries)?;
    let mut observation_truncated =
        observation_truncated || queue_truncated || enrollment_truncated;
    let Some(front) = entries.first() else {
        return Ok(MergeQueueProbe {
            readable: true,
            source: "github (queue empty or not configured)".to_owned(),
            reason_codes: observation_truncated
                .then_some(ObservationReason::ObservationTruncated)
                .into_iter()
                .collect(),
            report: Some(assess_merge_queue_liveness(MergeQueueLivenessInputs {
                entries: &[],
                checks: &[],
                active_runs,
                required_contexts,
                eligible_host_classes,
                routable_free_slots,
                stall_threshold_secs,
                now: Utc::now(),
                enrollment_cleared_prs: &enrollment_cleared_prs,
                observation_truncated,
            })),
        });
    };

    let checks = match front.head_sha.as_deref() {
        Some(sha) => {
            let (checks, truncated) = fetch_check_observations(actions, repo, sha)?;
            observation_truncated |= truncated;
            checks
        }
        None => Vec::new(),
    };
    Ok(MergeQueueProbe {
        readable: true,
        source: "github".to_owned(),
        reason_codes: observation_truncated
            .then_some(ObservationReason::ObservationTruncated)
            .into_iter()
            .collect(),
        report: Some(assess_merge_queue_liveness(MergeQueueLivenessInputs {
            entries: &entries,
            checks: &checks,
            active_runs,
            required_contexts,
            eligible_host_classes,
            routable_free_slots,
            stall_threshold_secs,
            now: Utc::now(),
            enrollment_cleared_prs: &enrollment_cleared_prs,
            observation_truncated,
        })),
    })
}

fn fetch_check_observations(
    actions: &GitHubActions,
    repo: &str,
    sha: &str,
) -> Result<(Vec<CheckObservation>, bool), String> {
    let mut checks = Vec::new();
    for page in 1..=OBSERVATION_MAX_PAGES {
        let raw = actions
            .run_gh(&[
                "api".to_owned(),
                format!(
                    "repos/{repo}/commits/{sha}/check-runs?per_page={OBSERVATION_PAGE_SIZE}&page={page}"
                ),
            ])
            .map_err(|error| format!("inspect front merge-group checks failed: {error}"))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("could not parse front check-runs JSON: {error}"))?;
        let page_checks = parse_check_observations(&value)?;
        let page_len = page_checks.len();
        checks.extend(page_checks);
        if page_len < OBSERVATION_PAGE_SIZE {
            return Ok((checks, false));
        }
    }
    Ok((checks, true))
}

fn fetch_observed_workflow_runs(
    actions: &GitHubActions,
    repo: &str,
    run_limit: u32,
) -> Result<ObservedRuns, String> {
    let limit = usize::try_from(run_limit.clamp(1, MAX_DETAILED_WORKFLOW_RUNS))
        .expect("u32 run limit fits usize");
    let mut runs_by_status = Vec::new();
    let mut truncated = false;
    for status in ["in_progress", "queued"] {
        let raw = actions
            .run_gh(&[
                "api".to_owned(),
                format!(
                    "repos/{repo}/actions/runs?status={status}&per_page={OBSERVATION_PAGE_SIZE}&page=1"
                ),
            ])
            .map_err(|error| format!("list {status} workflow runs failed: {error}"))?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("could not parse {status} workflow runs JSON: {error}"))?;
        let runs = value
            .get("workflow_runs")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{status} workflow runs response missing workflow_runs"))?;
        truncated |= runs.len() == OBSERVATION_PAGE_SIZE;
        runs_by_status.push(runs.clone());
    }
    let raw_runs = select_bounded_runs(&runs_by_status, limit);
    truncated |= runs_by_status.iter().map(Vec::len).sum::<usize>() > raw_runs.len();
    let mut observations = Vec::new();
    for run in &raw_runs {
        let Some(head_branch) = run.get("head_branch").and_then(Value::as_str) else {
            continue;
        };
        let Some(run_id) = run.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let (jobs, jobs_truncated) = fetch_run_jobs(actions, repo, run_id)?;
        truncated |= jobs_truncated;
        observations.push(ActiveRunObservation {
            run_id,
            workflow: run
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            head_branch: head_branch.to_owned(),
            head_sha: run
                .get("head_sha")
                .and_then(Value::as_str)
                .map(str::to_owned),
            status: run
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            created_at: run
                .get("created_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
            pull_requests: run
                .get("pull_requests")
                .and_then(Value::as_array)
                .map(|pull_requests| {
                    pull_requests
                        .iter()
                        .filter_map(|pr| pr.get("number").and_then(Value::as_u64))
                        .collect()
                })
                .unwrap_or_default(),
            url: run
                .get("html_url")
                .and_then(Value::as_str)
                .map(str::to_owned),
            jobs,
        });
    }
    Ok(ObservedRuns {
        runs: observations,
        truncated,
    })
}

fn fetch_run_jobs(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
) -> Result<(Vec<JobObservation>, bool), String> {
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            format!(
                "repos/{repo}/actions/runs/{run_id}/jobs?per_page={OBSERVATION_PAGE_SIZE}&page=1"
            ),
        ])
        .map_err(|error| format!("list jobs for workflow run {run_id} failed: {error}"))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("could not parse jobs for workflow run {run_id}: {error}"))?;
    let jobs = value
        .get("jobs")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("workflow run {run_id} response missing jobs"))?;
    Ok((
        parse_job_observations(jobs),
        jobs.len() == OBSERVATION_PAGE_SIZE,
    ))
}

fn select_bounded_runs(runs_by_status: &[Vec<Value>], limit: usize) -> Vec<Value> {
    let mut selected = Vec::with_capacity(limit);
    let mut indices = vec![0usize; runs_by_status.len()];
    while selected.len() < limit {
        let mut progressed = false;
        for (status_index, runs) in runs_by_status.iter().enumerate() {
            let index = &mut indices[status_index];
            if *index < runs.len() && selected.len() < limit {
                selected.push(runs[*index].clone());
                *index += 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    selected
}

fn parse_job_observations(jobs: &[Value]) -> Vec<JobObservation> {
    jobs.iter()
        .filter_map(|job| {
            Some(JobObservation {
                name: job.get("name")?.as_str()?.to_owned(),
                status: job.get("status")?.as_str()?.to_owned(),
                runner_name: job
                    .get("runner_name")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
                labels: job
                    .get("labels")
                    .and_then(Value::as_array)
                    .map(|labels| {
                        labels
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

fn inspect_release_liveness(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    stale_threshold_secs: i64,
) -> Result<ReleaseProbe, String> {
    let raw = actions
        .run_gh(&["api".to_owned(), format!("repos/{repo}/releases/latest")])
        .map_err(|error| format!("inspect latest release failed: {error}"))?;
    let release: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("could not parse latest release JSON: {error}"))?;
    let tag = release
        .get("tag_name")
        .and_then(Value::as_str)
        .filter(|tag| !tag.is_empty())
        .ok_or_else(|| "latest release response missing tag_name".to_owned())?;
    let published_at = release
        .get("published_at")
        .and_then(Value::as_str)
        .filter(|timestamp| !timestamp.is_empty())
        .ok_or_else(|| "latest release response missing published_at".to_owned())?;
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/compare/{tag}...{base}"),
        ])
        .map_err(|error| format!("compare latest release to {base} failed: {error}"))?;
    let comparison: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("could not parse release comparison JSON: {error}"))?;
    let commits_ahead = comparison
        .get("ahead_by")
        .and_then(Value::as_u64)
        .ok_or_else(|| "release comparison response missing ahead_by".to_owned())?;
    let changed_files = comparison.get("files").and_then(Value::as_array);
    // GitHub caps compare-file output at 300 paths. If that bound is reached,
    // treat the comparison as release-relevant rather than allowing omitted
    // paths to create a false healthy signal.
    let comparison_truncated = changed_files.is_some_and(|files| files.len() == 300);
    let releasable_commits_ahead = if commits_ahead == 0 {
        0
    } else if comparison_truncated {
        commits_ahead
    } else {
        changed_files.map_or(commits_ahead, |files| {
            if files.is_empty()
                || files
                    .iter()
                    .filter_map(|file| file.get("filename").and_then(Value::as_str))
                    .any(path_requires_release)
            {
                commits_ahead
            } else {
                0
            }
        })
    };
    let base_version = fetch_base_version(actions, repo, base)?;
    let mut optional_reason_codes = Vec::new();
    let (open_release_incident_issues, issues_truncated) =
        if let Ok((count, truncated)) = fetch_release_incident_issue_count(actions, repo) {
            (Some(count), truncated)
        } else {
            optional_reason_codes.push(ObservationReason::AuxiliaryObservationUnavailable);
            (None, false)
        };
    if issues_truncated {
        optional_reason_codes.push(ObservationReason::AuxiliaryObservationUnavailable);
    }
    let latest_successful_release_workflow_at =
        match fetch_latest_successful_release_workflow(actions, repo, base) {
            Ok(value) => value,
            Err(error) => {
                optional_reason_codes.push(classify_observation_error(&error));
                None
            }
        };
    let observation_truncated = comparison_truncated;
    Ok(ReleaseProbe {
        readable: true,
        source: "github".to_owned(),
        reason_codes: observation_truncated
            .then_some(ObservationReason::ObservationTruncated)
            .into_iter()
            .chain(optional_reason_codes)
            .collect(),
        report: Some(assess_release_liveness(
            tag.to_owned(),
            published_at.to_owned(),
            commits_ahead,
            releasable_commits_ahead,
            base_version,
            open_release_incident_issues,
            latest_successful_release_workflow_at,
            stale_threshold_secs,
            Utc::now(),
        )?),
    })
}

fn path_requires_release(path: &str) -> bool {
    let normalized = path.to_ascii_lowercase();
    !(normalized.starts_with("docs/")
        || normalized.starts_with(".claude-plugin/")
        || normalized.starts_with("commands/")
        || normalized.starts_with("skills/")
        || normalized.starts_with("agents/")
        || normalized.starts_with("hooks/")
        || matches!(
            normalized.as_str(),
            "changelog.md"
                | "readme.md"
                | "code_of_conduct.md"
                | "contributing.md"
                | "security.md"
                | "license"
                | "license.md"
        ))
}

fn fetch_release_incident_issue_count(
    actions: &GitHubActions,
    repo: &str,
) -> Result<(u64, bool), String> {
    let mut count = 0u64;
    for page in 1..=OBSERVATION_MAX_PAGES {
        let raw = actions
            .run_gh(&[
                "api".to_owned(),
                format!(
                    "repos/{repo}/issues?state=open&per_page={OBSERVATION_PAGE_SIZE}&page={page}"
                ),
            ])
            .map_err(|error| format!("inspect open release incidents failed: {error}"))?;
        let issues: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("could not parse open issues JSON: {error}"))?;
        let issues = issues
            .as_array()
            .ok_or_else(|| "open issues response is not an array".to_owned())?;
        count += issues
            .iter()
            .filter(|issue| issue.get("pull_request").is_none())
            .filter(|issue| {
                let title = issue
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                title.contains("release")
                    && (title.contains("stuck")
                        || title.contains("blocked")
                        || title.contains("failed")
                        || title.contains("failure"))
            })
            .count() as u64;
        if issues.len() < OBSERVATION_PAGE_SIZE {
            return Ok((count, false));
        }
    }
    Ok((count, true))
}

fn fetch_latest_successful_release_workflow(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Option<String>, String> {
    let raw = match actions
        .run_gh(&[
            "api".to_owned(),
            format!(
                "repos/{repo}/actions/workflows/auto-release.yml/runs?branch={base}&status=success&per_page=1"
            ),
        ]) {
        Ok(raw) => raw,
        Err(error) if error.to_string().contains("404") => return Ok(None),
        Err(error) => return Err(format!("inspect auto-release workflow failed: {error}")),
    };
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("could not parse auto-release workflow runs: {error}"))?;
    Ok(value
        .pointer("/workflow_runs/0/updated_at")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

fn fetch_base_version(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Option<String>, String> {
    let raw = match actions.run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/contents/VERSION?ref={base}"),
    ]) {
        Ok(raw) => raw,
        Err(error) if error.to_string().contains("404") => return Ok(None),
        Err(error) => return Err(format!("inspect base VERSION failed: {error}")),
    };
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("could not parse base VERSION response: {error}"))?;
    let encoded = value
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "base VERSION response missing content".to_owned())?
        .replace('\n', "");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("decode base VERSION failed: {error}"))?;
    let version = String::from_utf8(bytes)
        .map_err(|error| format!("base VERSION is not UTF-8: {error}"))?
        .trim()
        .to_owned();
    Ok((!version.is_empty()).then_some(version))
}

fn queued_macos_summary(runs: &[ActiveRunObservation], target: &str) -> QueuedSummary {
    let mut count = 0usize;
    let mut oldest_age_secs: Option<i64> = None;
    let now = Utc::now();
    for run in runs {
        if !run.jobs.iter().any(|job| {
            job.status == "queued"
                && job
                    .name
                    .to_ascii_lowercase()
                    .contains(&target.to_ascii_lowercase())
        }) {
            continue;
        }
        count += 1;
        // A downstream job can become queued long after its workflow starts.
        // Without a job-level queued timestamp, only a wholly queued workflow
        // has an authoritative age proxy. Still count downstream work, but do
        // not turn upstream runtime into a false queue-age alert.
        if run.status != "queued" {
            continue;
        }
        if let Some(created_at) = run.created_at.as_deref()
            && let Ok(ts) = DateTime::parse_from_rfc3339(created_at)
        {
            let age = (now - ts.with_timezone(&Utc)).num_seconds().max(0);
            oldest_age_secs = Some(oldest_age_secs.map_or(age, |oldest| oldest.max(age)));
        }
    }
    QueuedSummary {
        readable: true,
        source: "github".to_owned(),
        count,
        oldest_age_secs,
    }
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

fn merge_queue_to_json(probe: &MergeQueueProbe) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("readable".to_owned(), Value::from(probe.readable));
    m.insert("source".to_owned(), Value::from(probe.source.clone()));
    m.insert(
        "observation_reason_codes".to_owned(),
        serde_json::to_value(&probe.reason_codes).expect("merge observation reasons serialize"),
    );
    m.insert(
        "report".to_owned(),
        probe.report.as_ref().map_or(Value::Null, |report| {
            serde_json::to_value(report).expect("merge-queue liveness serialization")
        }),
    );
    Value::Object(m)
}

fn release_to_json(probe: &ReleaseProbe) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("readable".to_owned(), Value::from(probe.readable));
    m.insert("source".to_owned(), Value::from(probe.source.clone()));
    m.insert(
        "observation_reason_codes".to_owned(),
        serde_json::to_value(&probe.reason_codes).expect("release observation reasons serialize"),
    );
    m.insert(
        "report".to_owned(),
        probe.report.as_ref().map_or(Value::Null, |report| {
            serde_json::to_value(report).expect("release liveness serialization")
        }),
    );
    Value::Object(m)
}

fn write_merge_queue_text<W: Write>(stdout: &mut W, view: &FleetTextView<'_>) {
    let Some(report) = &view.merge_queue.report else {
        writeln!(
            stdout,
            "  merge queue {}: unreadable ({})",
            view.base, view.merge_queue.source
        )
        .ok();
        return;
    };
    let front = report
        .front
        .as_ref()
        .map_or_else(|| "-".to_owned(), |front| format!("#{}", front.pr));
    writeln!(
        stdout,
        "  merge queue {}: front={} required_materialized={} required_progressed={} stalled_with_idle_capacity={} threshold={} readable={}",
        view.base,
        front,
        report.materialized_required_checks,
        report.progressed_required_checks,
        report.front_stalled_with_idle_capacity,
        view.merge_queue_stall_threshold_secs,
        view.merge_queue.readable
    )
    .ok();
    if !view.merge_queue.reason_codes.is_empty() {
        writeln!(
            stdout,
            "    observation_reasons={:?}",
            view.merge_queue.reason_codes
        )
        .ok();
    }
    if !report.enrollment_cleared_prs.is_empty() {
        let prs = report
            .enrollment_cleared_prs
            .iter()
            .map(|pr| format!("#{pr}"))
            .collect::<Vec<_>>()
            .join(",");
        writeln!(stdout, "    auto_merge_enrollment_cleared={prs}").ok();
    }
    for occupier in &report.capacity_occupiers {
        writeln!(
            stdout,
            "    {:?}: run={} pr={} job={} runner={} url={}",
            occupier.kind,
            occupier.run_id,
            occupier
                .pr
                .map_or_else(|| "-".to_owned(), |pr| format!("#{pr}")),
            occupier.job,
            occupier.runner_name,
            occupier.url.as_deref().unwrap_or("-")
        )
        .ok();
    }
}

fn write_release_text<W: Write>(stdout: &mut W, view: &FleetTextView<'_>) {
    let Some(report) = &view.release.report else {
        writeln!(stdout, "  release: unavailable ({})", view.release.source).ok();
        return;
    };
    writeln!(
        stdout,
        "  release: tag={} age_secs={} commits_ahead={} stale={} threshold={} readable={}",
        report.tag,
        report.age_secs,
        report.commits_ahead,
        report.stale_with_unreleased_commits,
        view.release_stale_threshold_secs,
        view.release.readable
    )
    .ok();
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

    #[cfg(unix)]
    fn fake_gh(temp: &tempfile::TempDir, body: &str) -> GitHubActions {
        use std::os::unix::fs::PermissionsExt;

        let path = temp.path().join("gh");
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write fake gh");
        let mut permissions = fs::metadata(&path).expect("fake gh metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod fake gh");
        GitHubActions::new(temp.path()).with_gh_binary_for_tests(path)
    }

    #[cfg(unix)]
    #[test]
    fn transport_keeps_optional_runs_and_finds_queued_job_inside_in_progress_run() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            r#"
case "$*" in
  *"actions/runs?status=in_progress"*)
    printf '%s' '{"workflow_runs":[
      {"id":10,"name":"Required","head_branch":"gh-readonly-queue/main/pr-11-a","head_sha":"aaa","status":"in_progress","created_at":"2026-07-26T00:00:00Z","pull_requests":[{"number":11}]},
      {"id":20,"name":"Examples","head_branch":"feature/demo","head_sha":"bbb","status":"in_progress","created_at":"2026-07-26T00:00:00Z","pull_requests":[{"number":22}]}
    ]}' ;;
  *"actions/runs?status=queued"*) printf '%s' '{"workflow_runs":[]}' ;;
  *"actions/runs/10/jobs"*)
    printf '%s' '{"jobs":[{"name":"macOS required","status":"queued","runner_name":"","labels":["self-hosted","pulp-build-m5"]}]}' ;;
  *"actions/runs/20/jobs"*)
    printf '%s' '{"jobs":[{"name":"Validate examples (macOS)","status":"in_progress","runner_name":"pulp-vm-m1-01","labels":["self-hosted","pulp-build-m1"]}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        );
        let observed =
            fetch_observed_workflow_runs(&actions, "owner/repo", 100).expect("observe runs");
        assert_eq!(observed.runs.len(), 2);
        assert_eq!(observed.runs[1].head_branch, "feature/demo");
        let queued = queued_macos_summary(&observed.runs, "macos");
        assert_eq!(queued.count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn transport_paginates_merge_queue_instead_of_misclassifying_followers() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            r#"
case "$*" in
  *"cursor=NEXT"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[{"position":100,"enqueuedAt":"2026-07-26T00:00:00Z","headCommit":{"oid":"bbb"},"pullRequest":{"number":222}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}' ;;
  *)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[{"position":0,"enqueuedAt":"2026-07-26T00:00:00Z","headCommit":{"oid":"aaa"},"pullRequest":{"number":111}}],"pageInfo":{"hasNextPage":true,"endCursor":"NEXT"}}}}}}' ;;
esac
"#,
        );
        let (entries, truncated) =
            fetch_merge_queue_entries(&actions, "owner", "repo", "main", 5).expect("queue");
        assert!(!truncated);
        assert_eq!(
            entries.iter().map(|entry| entry.pr).collect::<Vec<_>>(),
            [111, 222]
        );
    }

    #[cfg(unix)]
    #[test]
    fn durable_snapshot_detects_open_pr_whose_auto_merge_was_cleared() {
        let temp = tempfile::tempdir().expect("temp");
        let calls = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!(
                "printf x >> '{}'\nprintf '%s' '{{\"state\":\"open\",\"base\":{{\"ref\":\"main\"}},\"auto_merge\":null}}'",
                calls.display()
            ),
        );
        let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
        fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
        fs::write(
            &path,
            r#"{"entries":[{"pr":11,"head_sha":"aaa","observed_at":"2026-07-26T00:00:00Z"}]}"#,
        )
        .expect("snapshot");
        let (cleared, truncated) =
            reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &[])
                .expect("reconcile");
        assert_eq!(cleared, [11]);
        assert!(!truncated);
        let (still_cleared, still_truncated) =
            reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &[])
                .expect("reconcile again");
        assert_eq!(still_cleared, [11]);
        assert!(!still_truncated);
        assert_eq!(fs::read_to_string(calls).expect("calls"), "xx");
    }

    #[cfg(unix)]
    #[test]
    fn retained_enrollment_alert_is_revalidated_and_clears_when_pr_closes() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            r#"printf '%s' '{"state":"closed","base":{"ref":"main"},"auto_merge":null}'"#,
        );
        let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
        fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
        fs::write(
            &path,
            r#"{"entries":[{"pr":11,"head_sha":"aaa","observed_at":"2026-07-26T00:00:00Z","auto_merge_cleared":true}]}"#,
        )
        .expect("snapshot");
        let (cleared, truncated) =
            reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &[])
                .expect("reconcile");
        assert!(cleared.is_empty());
        assert!(!truncated);
    }

    #[cfg(unix)]
    #[test]
    fn retargeted_pr_is_not_reported_as_cleared_enrollment() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            r#"printf '%s' '{"state":"open","base":{"ref":"release"},"auto_merge":null}'"#,
        );
        let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
        fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
        fs::write(
            &path,
            r#"{"entries":[{"pr":11,"head_sha":"aaa","observed_at":"2026-07-26T00:00:00Z"}]}"#,
        )
        .expect("snapshot");
        let (cleared, truncated) =
            reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &[])
                .expect("reconcile");
        assert!(cleared.is_empty());
        assert!(!truncated);
    }

    #[cfg(unix)]
    #[test]
    fn malformed_enrollment_snapshot_fails_closed_without_overwrite() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(&temp, "exit 99");
        let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
        fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
        fs::write(&path, "not json").expect("snapshot");
        let error = reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &[])
            .expect_err("corrupt history must be visible");
        assert!(error.contains("parse fleet enrollment snapshot failed"));
        assert_eq!(fs::read_to_string(path).expect("snapshot"), "not json");
    }

    #[cfg(unix)]
    #[test]
    fn enrollment_reconciliation_has_a_fixed_per_tick_api_budget() {
        let temp = tempfile::tempdir().expect("temp");
        let calls = temp.path().join("calls");
        let actions = fake_gh(
            &temp,
            &format!(
                "printf x >> '{}'\nprintf '%s' '{{\"state\":\"open\",\"base\":{{\"ref\":\"main\"}},\"auto_merge\":{{}}}}'",
                calls.display()
            ),
        );
        let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
        fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
        let entries = (1..=MAX_ENROLLMENT_LOOKUPS_PER_TICK + 1)
            .map(|pr| {
                serde_json::json!({
                    "pr": pr,
                    "head_sha": null,
                    "observed_at": "2026-07-26T00:00:00Z"
                })
            })
            .collect::<Vec<_>>();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"entries": entries})).expect("snapshot JSON"),
        )
        .expect("snapshot");
        let (cleared, truncated) =
            reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &[])
                .expect("reconcile");
        assert!(cleared.is_empty());
        assert!(truncated);
        assert_eq!(
            fs::read_to_string(calls).expect("calls").len(),
            MAX_ENROLLMENT_LOOKUPS_PER_TICK
        );
    }

    #[test]
    fn observation_failures_have_stable_auth_and_rate_limit_reasons() {
        assert_eq!(
            classify_observation_error("HTTP 403: API rate limit exceeded"),
            ObservationReason::GitHubRateLimited
        );
        assert_eq!(
            classify_observation_error("HTTP 401: Bad credentials"),
            ObservationReason::GitHubAuthFailed
        );
    }

    #[test]
    fn enrollment_snapshot_keys_do_not_alias_punctuation_variants() {
        let root = Path::new("/tmp/state");
        assert_ne!(
            enrollment_snapshot_path(root, "foo/bar-baz", "release/x"),
            enrollment_snapshot_path(root, "foo-bar/baz", "release-x")
        );
    }

    #[test]
    fn initial_merge_queue_cursor_is_nullable() {
        assert!(MERGE_QUEUE_QUERY.contains("$cursor:String)"));
        assert!(!MERGE_QUEUE_QUERY.contains("$cursor:String!"));
    }

    #[test]
    fn active_run_selection_is_globally_bounded_and_fair_across_statuses() {
        let in_progress = (0..80)
            .map(|id| serde_json::json!({"id": id}))
            .collect::<Vec<_>>();
        let queued = (100..180)
            .map(|id| serde_json::json!({"id": id}))
            .collect::<Vec<_>>();
        let selected = select_bounded_runs(&[in_progress, queued], 50);
        assert_eq!(selected.len(), 50);
        assert_eq!(
            selected
                .iter()
                .filter(|run| run["id"].as_u64().is_some_and(|id| id < 100))
                .count(),
            25
        );
        assert_eq!(
            selected
                .iter()
                .filter(|run| run["id"].as_u64().is_some_and(|id| id >= 100))
                .count(),
            25
        );
    }

    #[test]
    fn downstream_queued_job_does_not_inherit_in_progress_workflow_age() {
        let job = JobObservation {
            name: "macOS required".to_owned(),
            status: "queued".to_owned(),
            runner_name: None,
            labels: Vec::new(),
        };
        let run = |status: &str| ActiveRunObservation {
            run_id: 1,
            workflow: "Build".to_owned(),
            head_branch: "feature".to_owned(),
            head_sha: None,
            status: status.to_owned(),
            created_at: Some("1970-01-01T00:00:00Z".to_owned()),
            pull_requests: Vec::new(),
            url: None,
            jobs: vec![job.clone()],
        };
        let downstream = queued_macos_summary(&[run("in_progress")], "macos");
        assert_eq!(downstream.count, 1);
        assert_eq!(downstream.oldest_age_secs, None);
        let wholly_queued = queued_macos_summary(&[run("queued")], "macos");
        assert!(wholly_queued.oldest_age_secs.is_some());
    }

    #[test]
    fn release_classification_uses_changed_paths_not_commit_labels() {
        assert!(path_requires_release("src/installer.rs"));
        assert!(!path_requires_release("skills/ci/SKILL.md"));
        assert!(!path_requires_release(".claude-plugin/plugin.json"));
        assert!(!path_requires_release("docs/installer.md"));
        assert!(!path_requires_release("CHANGELOG.md"));
    }
}
