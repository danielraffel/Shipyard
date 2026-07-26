//! CLI handler for `shipyard runner fleet-status`.
//!
//! Read-only fleet aggregation for macOS VM CI: combine per-host capacity,
//! host-local `tartci doctor --reap --json` digests, and queued macOS age. The
//! command never deletes VMs or retargets runs; destructive cleanup stays inside
//! `tartci doctor --reap --fix` on each host.

use std::collections::BTreeSet;
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
    MergeQueueLivenessReport, assess_merge_queue_liveness, assess_release_liveness,
    parse_check_observations, parse_merge_queue_entries,
};

mod assessment;
mod observation;
mod policy;
mod render;

pub(in crate::app) use assessment::FleetAssessment;
use assessment::{
    DoctorProbe, HostFleetStatus, MergeQueueProbe, ObservationReason, QueuedSummary, ReleaseProbe,
};
#[cfg(test)]
use observation::{
    base_version_path, path_requires_release, release_compare_path, release_workflow_runs_path,
    select_bounded_runs,
};
use observation::{
    classify_observation_error, fetch_observed_workflow_runs, inspect_merge_queue_liveness,
    inspect_release_liveness, observation_reason_codes, queued_macos_summary,
    required_status_checks,
};
#[cfg(all(test, unix))]
use observation::{
    count_releasable_commits, enrollment_snapshot_path, fetch_check_observations,
    fetch_merge_queue_entries, reconcile_enrollment_snapshot,
};
#[cfg(test)]
pub(in crate::app) use policy::FleetLivenessPolicy;
pub(in crate::app) use policy::fleet_liveness_policy;
pub(in crate::app) use render::{render_fleet_assessment, render_fleet_watch_event};

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
const MAX_RELEASE_COMMIT_LOOKUPS_PER_TICK: usize = 25;
const MERGE_QUEUE_QUERY: &str = "query($owner:String!,$name:String!,$branch:String!,$cursor:String){repository(owner:$owner,name:$name){mergeQueue(branch:$branch){entries(first:100,after:$cursor){nodes{position enqueuedAt headCommit{oid} pullRequest{number}} pageInfo{hasNextPage endCursor}}}}}";

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
    let assessment = collect_fleet_assessment(args, config, cwd, state_dir, actions)?;
    render_fleet_assessment(&assessment, json, stdout)?;
    Ok(assessment.exit_code())
}

#[allow(clippy::too_many_lines)]
pub(super) fn collect_fleet_assessment(
    args: FleetStatusArgs,
    config: &LoadedConfig,
    cwd: &Path,
    state_dir: &Path,
    actions: &GitHubActions,
) -> Result<FleetAssessment, CliFailure> {
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

    Ok(FleetAssessment {
        repo,
        target: args.target,
        free,
        routable_free_slots,
        capacity_unreadable,
        doctor_unreadable,
        supervisor_unhealthy,
        problem_hosts,
        queued_age_threshold_secs,
        queue_run_limit,
        queued_age_with_capacity,
        queue,
        base: args.base,
        merge_queue_stall_threshold_secs: args.merge_queue_stall_threshold_secs.max(0),
        merge_queue,
        release_stale_threshold_secs: args.release_stale_threshold_secs.max(0),
        release,
        hosts,
        observation_reason_codes,
        observation_incomplete,
        should_fail,
    })
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

#[cfg(test)]
mod tests;
