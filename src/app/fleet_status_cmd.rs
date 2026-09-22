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
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::CliFailure;
use crate::capacity::{
    HostCapacity, HostClassConfig, REMOTE_OBSERVER_PATH, any_unreadable,
    observer_ssh_probe_options, parse_host_classes, probe_host_capacity_until,
    remote_observer_command, total_free,
};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::executor::ssh::shlex_quote;
use crate::fleet_service::{Boundary, RegisteredRunner, RunnerScope};
use crate::fleet_slot::{
    QueuedJobObservation, QueuedJobReport, WedgeThresholds, assess_queued_job,
};
use crate::merge_queue_liveness::{
    ActiveRunObservation, CheckObservation, JobObservation, MergeQueueLivenessInputs,
    MergeQueueLivenessReport, assess_merge_queue_liveness, assess_release_liveness,
    parse_check_observations, parse_merge_queue_entries,
};
use crate::process::{BoundedOutputError, run_output_until};

mod assessment;
mod observation;
mod policy;
mod readability;
mod release_observation;
mod render;

pub(in crate::app) use assessment::FleetAssessment;
use assessment::{
    AttestationProbe, BrokenRunner, DoctorProbe, ExpectedHostConfig, ExpectedHostStatus,
    HostFleetStatus, MergeQueueProbe, ObservationReason, QueuedSummary, ReleaseProbe,
    RepositoryRunner, RoutingMismatch, RunnerInventory, StorageProbe, WedgedQueuedJobs,
};
use observation::{
    classify_observation_error, fetch_observed_workflow_runs, inspect_merge_queue_liveness,
    observation_reason_codes, queued_macos_summary, required_status_checks,
};
#[cfg(test)]
use observation::{enrollment_snapshot_path, select_bounded_runs};
#[cfg(all(test, unix))]
use observation::{
    fetch_check_observations, fetch_merge_queue_entries, reconcile_enrollment_snapshot,
};
#[cfg(test)]
pub(in crate::app) use policy::FleetLivenessPolicy;
pub(in crate::app) use policy::fleet_liveness_policy;
use readability::{
    DegradedObservation, LaneCorroboration, ReadBoundary, RoutingConfidence, classify_read_boundary,
};
use release_observation::inspect_release_liveness;
#[cfg(all(test, unix))]
use release_observation::{ReleasableCommitSummary, count_releasable_commits};
#[cfg(test)]
use release_observation::{
    base_version_path, file_change_requires_release, path_requires_release, release_compare_path,
    release_is_skipped, release_workflow_runs_path,
};
pub(in crate::app) use render::{render_fleet_assessment, render_fleet_watch_event};

const FLEET_LANE_TARGET: &str = "macos";
const DEFAULT_DISK_FLOOR_KIBIBYTE: u64 = 25 * 1024 * 1024;
const FLEET_HOST_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const FLEET_GITHUB_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);

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

    let host_probes = probe_hosts_concurrently(&classes);
    let capacities = host_probes
        .iter()
        .map(|probe| probe.capacity.clone())
        .collect::<Vec<_>>();
    // One fleet tick is an observation, not a batch job. Share one absolute
    // GitHub deadline across runner inventory, active-run detail, merge-queue,
    // and release reads so a large or slow API surface fails closed with a
    // rendered assessment instead of wedging the overseer indefinitely.
    let actions = fleet_github_actions_with_timeout(actions, FLEET_GITHUB_OBSERVATION_TIMEOUT);
    // Observe repository runners once through the controller's authenticated
    // GitHub client. Host-local doctor probes can share an unauthenticated IP
    // rate limit, which must not make otherwise healthy capacity unroutable.
    let runners = fetch_repository_runners(&actions, &repo);
    let expected_host_configs =
        parse_expected_hosts(&config.data).map_err(|error| CliFailure::new(2, error))?;
    let expected_hosts = assess_expected_hosts(&expected_host_configs, &runners, &args.target);
    // What the controller itself could see of the lane. A host's own failed
    // GitHub read is weighed against this rather than taken as a capacity fact.
    let corroboration = LaneCorroboration {
        inventory_readable: runners.readable,
        online_lane_runners: runners.online_lane_runners(FLEET_LANE_TARGET),
    };
    let mut hosts = Vec::new();
    for probe in host_probes {
        // `--target` is a GitHub job-name substring, not a TartCI routing
        // label. FleetStatus is the macOS VM fleet command, so host health is
        // always scoped to the macOS lane even for custom job names such as
        // `required-apple-tests`.
        hosts.push(analyze_host(
            probe.capacity,
            probe.doctor,
            probe.storage,
            probe.attestation,
            FLEET_LANE_TARGET,
            corroboration,
        ));
    }

    let queue_run_limit = args.queue_run_limit.clamp(1, MAX_DETAILED_WORKFLOW_RUNS);
    let observed_runs = fetch_observed_workflow_runs(&actions, &repo, queue_run_limit);
    let routing_mismatches = observed_runs.as_ref().map_or_else(
        |_| Vec::new(),
        |observed| detect_routing_mismatches(&observed.runs, &runners),
    );
    let wedged_queued = observed_runs.as_ref().map_or_else(
        |_| WedgedQueuedJobs::default(),
        |observed| {
            detect_wedged_queued_jobs(
                &observed.runs,
                &runners,
                WedgeThresholds::default(),
                Utc::now(),
            )
        },
    );
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
    let routing_degraded_reasons = routing_degraded_reasons(&hosts);
    let routing_confidence = if routing_degraded_reasons.is_empty() {
        RoutingConfidence::Confirmed
    } else {
        RoutingConfidence::Degraded
    };
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
                &actions,
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
        &actions,
        &repo,
        &args.base,
        args.release_stale_threshold_secs,
    )
    .unwrap_or_else(|reason| ReleaseProbe {
        readable: false,
        source: reason.clone(),
        report: None,
        reason_codes: vec![classify_observation_error(&reason)],
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
        || !runners.readable
        || !expected_hosts_needing_attention(&expected_hosts).is_empty()
        || !routing_mismatches.is_empty()
        || !wedged_queued.raising.is_empty()
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
        routing_confidence,
        routing_degraded_reasons,
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
        runners,
        expected_hosts,
        routing_mismatches,
        wedged_queued,
        observation_reason_codes,
        observation_incomplete,
        should_fail,
    })
}

fn fleet_github_actions_with_timeout(actions: &GitHubActions, timeout: Duration) -> GitHubActions {
    actions
        .clone()
        .with_absolute_deadline(Instant::now() + timeout)
}

struct HostProbeBundle {
    capacity: HostCapacity,
    doctor: DoctorProbe,
    storage: StorageProbe,
    attestation: AttestationProbe,
}

fn probe_hosts_concurrently(classes: &[HostClassConfig]) -> Vec<HostProbeBundle> {
    probe_hosts_concurrently_with_timeout(classes, FLEET_HOST_PROBE_TIMEOUT)
}

fn probe_hosts_concurrently_with_timeout(
    classes: &[HostClassConfig],
    timeout: Duration,
) -> Vec<HostProbeBundle> {
    probe_hosts_concurrently_with_timeout_using(classes, timeout, probe_storage_until)
}

fn probe_hosts_concurrently_with_timeout_using(
    classes: &[HostClassConfig],
    timeout: Duration,
    storage_probe: fn(&HostClassConfig, Instant) -> StorageProbe,
) -> Vec<HostProbeBundle> {
    let deadline = Instant::now() + timeout;
    thread::scope(|scope| {
        let handles = classes
            .iter()
            .map(|class| {
                scope.spawn(move || probe_host_until_using(class, deadline, storage_probe))
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .zip(classes)
            .map(|(handle, class)| {
                handle
                    .join()
                    .unwrap_or_else(|_| unreadable_host_bundle(class, "host probe panicked"))
            })
            .collect()
    })
}

fn probe_host_until_using(
    class: &HostClassConfig,
    deadline: Instant,
    storage_probe: fn(&HostClassConfig, Instant) -> StorageProbe,
) -> HostProbeBundle {
    thread::scope(|scope| {
        let capacity = scope.spawn(|| probe_host_capacity_until(class, deadline));
        let doctor = scope.spawn(|| probe_doctor_until(class, deadline));
        let storage = scope.spawn(|| storage_probe(class, deadline));
        let attestation = scope.spawn(|| probe_attestation_until(class, deadline));
        HostProbeBundle {
            capacity: capacity.join().unwrap_or_else(|_| HostCapacity {
                class: class.class.clone(),
                ssh: class.ssh.clone(),
                cap: class.cap,
                running: None,
                source: "capacity probe panicked".to_owned(),
            }),
            doctor: doctor.join().unwrap_or_else(|_| DoctorProbe {
                readable: false,
                source: "doctor probe panicked".to_owned(),
                digest: None,
            }),
            storage: storage.join().unwrap_or_else(|_| StorageProbe {
                source: "storage probe panicked".to_owned(),
                disk_path: class.tart_home.clone().unwrap_or_else(|| ".".to_owned()),
                disk_floor_kibibyte: DEFAULT_DISK_FLOOR_KIBIBYTE,
                ..StorageProbe::default()
            }),
            attestation: attestation.join().unwrap_or_else(|_| AttestationProbe {
                readable: false,
                boundary: Some(Boundary::Transport.as_str().to_owned()),
                source: "attestation probe panicked".to_owned(),
                ..AttestationProbe::default()
            }),
        }
    })
}

fn unreadable_host_bundle(class: &HostClassConfig, reason: &str) -> HostProbeBundle {
    HostProbeBundle {
        capacity: HostCapacity {
            class: class.class.clone(),
            ssh: class.ssh.clone(),
            cap: class.cap,
            running: None,
            source: reason.to_owned(),
        },
        doctor: DoctorProbe {
            readable: false,
            source: reason.to_owned(),
            digest: None,
        },
        storage: StorageProbe {
            source: reason.to_owned(),
            disk_path: class.tart_home.clone().unwrap_or_else(|| ".".to_owned()),
            disk_floor_kibibyte: DEFAULT_DISK_FLOOR_KIBIBYTE,
            ..StorageProbe::default()
        },
        attestation: AttestationProbe {
            readable: false,
            boundary: Some(Boundary::Transport.as_str().to_owned()),
            source: reason.to_owned(),
            ..AttestationProbe::default()
        },
    }
}

fn parse_expected_hosts(data: &toml::Table) -> Result<Vec<ExpectedHostConfig>, String> {
    let Some(value) = data
        .get("runner")
        .and_then(toml::Value::as_table)
        .and_then(|runner| runner.get("fleet"))
        .and_then(toml::Value::as_table)
        .and_then(|fleet| fleet.get("expected_host"))
    else {
        return Ok(Vec::new());
    };
    let hosts = value
        .as_table()
        .ok_or_else(|| "runner.fleet.expected_host must be a table".to_owned())?;
    let mut parsed = Vec::with_capacity(hosts.len());
    for (name, value) in hosts {
        let host = value
            .as_table()
            .ok_or_else(|| format!("runner.fleet.expected_host.{name} must be a table"))?;
        let active = match host.get("active") {
            None => true,
            Some(toml::Value::Boolean(value)) => *value,
            Some(_) => {
                return Err(format!(
                    "runner.fleet.expected_host.{name}.active must be a boolean"
                ));
            }
        };
        let min_online = match host.get("min_online") {
            None => 1,
            Some(toml::Value::Integer(value)) if *value >= 0 => {
                u32::try_from(*value).map_err(|_| {
                    format!("runner.fleet.expected_host.{name}.min_online is too large")
                })?
            }
            Some(_) => {
                return Err(format!(
                    "runner.fleet.expected_host.{name}.min_online must be a non-negative integer"
                ));
            }
        };
        let labels = match host.get("labels") {
            Some(toml::Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        format!("runner.fleet.expected_host.{name}.labels must be strings")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(format!(
                    "runner.fleet.expected_host.{name}.labels must be an array"
                ));
            }
            None => {
                return Err(format!(
                    "runner.fleet.expected_host.{name}.labels is required"
                ));
            }
        };
        if labels.is_empty() {
            return Err(format!(
                "runner.fleet.expected_host.{name}.labels must not be empty"
            ));
        }
        parsed.push(ExpectedHostConfig {
            name: name.clone(),
            active,
            min_online,
            labels,
        });
    }
    Ok(parsed)
}

/// Labels that declare a host's operating system.
const PLATFORM_LABELS: [(&str, &str); 4] = [
    ("linux", "linux"),
    ("macos", "macos"),
    ("darwin", "macos"),
    ("windows", "windows"),
];

/// Architecture labels the macOS VM lane cannot use.
///
/// The fleet this command aggregates runs Apple Silicon Tart VMs, so an
/// Intel-declared Mac is as unable to take a lane job as a Linux box is.
const NON_APPLE_SILICON_LABELS: [&str; 3] = ["x64", "x86_64", "x86"];

/// Whether a declared host's labels can serve the lane being reported on.
///
/// Declared labels are read as constraints, never as a requirement to declare:
/// a host that names no platform is not excluded, because silence is not a
/// contradiction. A host that names one is held to it.
fn expected_host_serves_target(labels: &[String], target: &str) -> bool {
    let target = normalized_target(target);
    let lowered = labels
        .iter()
        .map(|label| label.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    let declared = lowered
        .iter()
        .filter_map(|label| {
            PLATFORM_LABELS
                .iter()
                .find(|(name, _)| *name == label)
                .map(|(_, platform)| *platform)
        })
        .collect::<Vec<_>>();
    if !declared.is_empty() && !declared.iter().any(|platform| *platform == target) {
        return false;
    }
    if target == "macos"
        && lowered
            .iter()
            .any(|label| NON_APPLE_SILICON_LABELS.contains(&label.as_str()))
    {
        return false;
    }
    true
}

/// Declared-host problems that belong to the lane being reported on.
///
/// A Linux or Intel host short of runners is real inventory news and stays in
/// the output, but it is not a fault in the macOS lane the reader asked about.
/// Letting it raise the top-level verdict is what made a healthy fleet read as
/// needing attention for machines that never serve it.
fn expected_hosts_needing_attention(hosts: &[ExpectedHostStatus]) -> Vec<&ExpectedHostStatus> {
    hosts
        .iter()
        .filter(|host| host.serves_target && host.problem.is_some())
        .collect()
}

fn assess_expected_hosts(
    expected: &[ExpectedHostConfig],
    inventory: &RunnerInventory,
    target: &str,
) -> Vec<ExpectedHostStatus> {
    expected
        .iter()
        .map(|host| {
            let required_labels = host
                .labels
                .iter()
                .map(|label| label.to_ascii_lowercase())
                .collect::<BTreeSet<_>>();
            let matching = inventory
                .runners
                .iter()
                .filter(|runner| {
                    let actual = runner
                        .labels
                        .iter()
                        .map(|label| label.to_ascii_lowercase())
                        .collect::<BTreeSet<_>>();
                    required_labels.is_subset(&actual)
                })
                .collect::<Vec<_>>();
            let online = matching
                .iter()
                .filter(|runner| runner.status.eq_ignore_ascii_case("online"))
                .count();
            let idle = matching
                .iter()
                .filter(|runner| runner.status.eq_ignore_ascii_case("online") && !runner.busy)
                .count();
            let problem = if host.active && !inventory.readable {
                Some("runner_inventory_unreadable".to_owned())
            } else if host.active && online < host.min_online as usize {
                Some(format!(
                    "expected_host_unavailable:online={online} min_online={}",
                    host.min_online
                ))
            } else {
                None
            };
            ExpectedHostStatus {
                name: host.name.clone(),
                active: host.active,
                min_online: host.min_online,
                labels: host.labels.clone(),
                matching_runners: matching.iter().map(|runner| runner.name.clone()).collect(),
                online,
                idle,
                serves_target: expected_host_serves_target(&host.labels, target),
                problem,
            }
        })
        .collect()
}

fn probe_doctor_until(class: &HostClassConfig, deadline: Instant) -> DoctorProbe {
    let output = if let Some(host) = &class.ssh {
        let mut command = Command::new("ssh");
        let remote = remote_observer_command(&remote_tartci_command(class), deadline);
        command
            .args(observer_ssh_probe_options())
            .arg(host)
            .arg(remote);
        run_output_until(&mut command, deadline, "ssh tartci doctor")
    } else {
        let mut command = Command::new(&class.tartci_bin);
        if let Some(github_cli) = &class.github_cli {
            command.env("TARTCI_GH_CLI", github_cli);
        }
        if let Some(tart_home) = &class.tart_home {
            command.env("TART_HOME", tart_home);
        }
        command.args(["doctor", "--reap", "--json"]);
        run_output_until(&mut command, deadline, "tartci doctor")
    };

    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return DoctorProbe {
                readable: false,
                source: match error {
                    BoundedOutputError::TimedOut { .. } if class.ssh.is_some() => {
                        "ssh tartci doctor timed out".to_owned()
                    }
                    BoundedOutputError::TimedOut { .. } => "tartci doctor timed out".to_owned(),
                    _ if class.ssh.is_some() => {
                        format!("ssh tartci doctor unreadable: {error}")
                    }
                    _ => {
                        format!("`{}` tartci doctor unreadable: {error}", class.tartci_bin)
                    }
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

fn analyze_host(
    capacity: HostCapacity,
    doctor: DoctorProbe,
    storage: StorageProbe,
    attestation: AttestationProbe,
    target: &str,
    corroboration: LaneCorroboration,
) -> HostFleetStatus {
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
    let reported_problems = digest
        .and_then(|value| value.get("problems"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|problem| problem_matches_target(problem, &all_supervisors, target))
        .cloned()
        .collect::<Vec<_>>();
    let (problems, degraded_observations) =
        partition_observation_gaps(reported_problems, corroboration);
    let storage_problems = storage_problems(&storage);
    let attestation_problems = attestation_problems(&attestation);
    let problem_count = problems.len() + storage_problems.len() + attestation_problems.len();
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
        && storage.readable
        // An unreadable attestation is not named here on purpose: it raises an
        // attestation problem, so `problem_count` below already carries it. A
        // second conjunct would be a guard no test could break.
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
        degraded_observations,
        supervisors,
        storage,
        storage_problems,
        attestation,
        attestation_problems,
    }
}

fn storage_problems(storage: &StorageProbe) -> Vec<String> {
    if !storage.readable {
        return vec![format!("storage_unreadable:{}", storage.source)];
    }
    let mut problems = Vec::new();
    if storage
        .disk_available_kibibyte
        .is_some_and(|available| available < storage.disk_floor_kibibyte)
    {
        problems.push(format!(
            "disk_floor_unmet:path={} available_kibibyte={} floor_kibibyte={}",
            storage.disk_path,
            storage.disk_available_kibibyte.unwrap_or_default(),
            storage.disk_floor_kibibyte
        ));
    }
    if storage
        .ccache_size_kibibyte
        .zip(storage.ccache_max_kibibyte)
        .is_some_and(|(size, maximum)| size > maximum)
    {
        problems.push(format!(
            "ccache_over_limit:size_kibibyte={} max_kibibyte={}",
            storage.ccache_size_kibibyte.unwrap_or_default(),
            storage.ccache_max_kibibyte.unwrap_or_default()
        ));
    }
    problems
}

fn probe_storage_until(class: &HostClassConfig, deadline: Instant) -> StorageProbe {
    let disk_path = class.tart_home.as_deref().unwrap_or(".");
    let script = storage_probe_script(disk_path);
    let output = if let Some(host) = &class.ssh {
        let mut command = Command::new("ssh");
        let remote = remote_observer_command(
            &format!(
                "env PATH={REMOTE_OBSERVER_PATH} sh -c {}",
                shlex_quote(&script)
            ),
            deadline,
        );
        command
            .args(observer_ssh_probe_options())
            .arg(host)
            .arg(remote);
        run_output_until(&mut command, deadline, "ssh storage probe")
    } else {
        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        run_output_until(&mut command, deadline, "storage probe")
    };
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return StorageProbe {
                source: match error {
                    BoundedOutputError::TimedOut { .. } if class.ssh.is_some() => {
                        "ssh storage probe timed out".to_owned()
                    }
                    BoundedOutputError::TimedOut { .. } => "storage probe timed out".to_owned(),
                    _ if class.ssh.is_some() => {
                        format!("ssh storage probe unreadable: {error}")
                    }
                    _ => {
                        format!("storage probe unreadable: {error}")
                    }
                },
                disk_path: disk_path.to_owned(),
                disk_floor_kibibyte: DEFAULT_DISK_FLOOR_KIBIBYTE,
                ..StorageProbe::default()
            };
        }
    };
    storage_probe_from_output(&output, disk_path)
}

/// The attester declares its own cadence; a reading older than this many
/// intervals means the writer stopped, not that the host is quiet.
const ATTESTATION_STALENESS_FACTOR: i64 = 2;

/// Used only when the artifact omits its own interval, so a missing cadence
/// cannot silently disable the staleness gate.
const ATTESTATION_DEFAULT_INTERVAL_SECS: u64 = 300;

/// Seconds a host's clock may run ahead of ours before its artifact is refused.
/// Jitter between NTP-disciplined fleet hosts is sub-second; a full minute is
/// still absorbed, and anything past it is a real clock fault rather than noise.
const ATTESTATION_MAX_CLOCK_SKEW_SECS: i64 = 60;

/// `$HOME` is expanded on the far side on purpose: the artifact lives under the
/// attester's own home directory, which differs per host and is not knowable
/// here. Quoting the path would defeat that and read a literal `~`.
fn attestation_probe_script() -> String {
    "p=\"$HOME\"/.tartci/state/host-attestation.json; if [ -r \"$p\" ]; then cat \"$p\"; else \
     echo 'host attestation artifact absent' >&2; exit 66; fi"
        .to_owned()
}

fn probe_attestation_until(class: &HostClassConfig, deadline: Instant) -> AttestationProbe {
    probe_attestation_until_at(class, deadline, Utc::now())
}

fn probe_attestation_until_at(
    class: &HostClassConfig,
    deadline: Instant,
    now: DateTime<Utc>,
) -> AttestationProbe {
    let script = attestation_probe_script();
    let output = if let Some(host) = &class.ssh {
        let mut command = Command::new("ssh");
        let remote = remote_observer_command(
            &format!(
                "env PATH={REMOTE_OBSERVER_PATH} sh -c {}",
                shlex_quote(&script)
            ),
            deadline,
        );
        command
            .args(observer_ssh_probe_options())
            .arg(host)
            .arg(remote);
        run_output_until(&mut command, deadline, "ssh attestation probe")
    } else {
        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        run_output_until(&mut command, deadline, "attestation probe")
    };
    let base_source = if class.ssh.is_some() { "ssh" } else { "local" };
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return AttestationProbe {
                readable: false,
                boundary: Some(Boundary::Transport.as_str().to_owned()),
                source: match error {
                    BoundedOutputError::TimedOut { .. } => {
                        format!("{base_source} attestation probe timed out")
                    }
                    _ => format!("{base_source} attestation probe unreadable: {error}"),
                },
                ..AttestationProbe::default()
            };
        }
    };
    attestation_probe_from_output(&output, base_source, now)
}

/// Collect the runners the attester itself called broken.
///
/// A runner absent from `persistent_runners` and one in a document that omits
/// the array both yield nothing here; the caller has already refused any
/// document it could not read, so an empty census means an empty census.
fn broken_runners_from_document(document: &Value) -> Vec<BrokenRunner> {
    document
        .get("persistent_runners")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |entries| {
            entries
                .iter()
                .filter(|entry| entry.get("verdict").and_then(Value::as_str) == Some("broken"))
                .map(|entry| BrokenRunner {
                    label: entry
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    loaded: entry
                        .get("loaded")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    crash_loop: entry
                        .get("crash_loop")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    runs: entry.get("runs").and_then(Value::as_u64).unwrap_or(0),
                    registered: entry
                        .get("registered")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
                .collect()
        })
}

/// Parse one host's attestation artifact into a verdict.
///
/// Every refusal path sets `readable = false` and names a boundary, because a
/// reading that could not be taken must never be indistinguishable from one
/// that found nothing wrong.
fn attestation_probe_from_output(
    output: &Output,
    base_source: &str,
    now: DateTime<Utc>,
) -> AttestationProbe {
    let unreadable = |boundary: Boundary, detail: String| AttestationProbe {
        readable: false,
        boundary: Some(boundary.as_str().to_owned()),
        source: detail,
        ..AttestationProbe::default()
    };

    if !output.status.success() {
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |c| c.to_string());
        return unreadable(
            Boundary::Transport,
            format!("{base_source} attestation exit {code}"),
        );
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let Ok(document) = serde_json::from_str::<Value>(&raw) else {
        return unreadable(
            Boundary::Parse,
            format!("{base_source} attestation is not JSON"),
        );
    };

    attestation_probe_from_document(&document, base_source, now)
}

/// Judge a parsed attestation document.
///
/// Split from the transport half above so that reading bytes off a host and
/// deciding what those bytes assert stay separately testable — and so neither
/// grows without the other being looked at.
fn attestation_probe_from_document(
    document: &Value,
    base_source: &str,
    now: DateTime<Utc>,
) -> AttestationProbe {
    let unreadable = |boundary: Boundary, detail: String| AttestationProbe {
        readable: false,
        boundary: Some(boundary.as_str().to_owned()),
        source: detail,
        ..AttestationProbe::default()
    };

    // A launchd LaunchAgent lives in the per-user GUI domain, so a
    // non-interactive session cannot enumerate it. The attester runs inside
    // that domain and reports whether it could read it; when it could not, the
    // runner census below is blind rather than empty.
    let launchd_readable = document.get("launchd_readable").and_then(Value::as_bool);
    // Absent, null or non-boolean is not a claim that the domain was readable.
    // Treating it as one would let a partially written or older artifact take
    // the believe-the-census path, which is the blind-reads-as-empty failure
    // this arm exists to refuse.
    let Some(launchd_domain_readable) = launchd_readable else {
        return unreadable(
            Boundary::Parse,
            format!(
                "{base_source} attestation did not report launchd readability, so a blind \
                 runner census cannot be told from an empty one"
            ),
        );
    };
    if !launchd_domain_readable {
        return AttestationProbe {
            readable: false,
            boundary: Some(Boundary::Scope.as_str().to_owned()),
            source: format!("{base_source} attestation could not read the launchd GUI domain"),
            launchd_readable,
            ..AttestationProbe::default()
        };
    }

    let interval_secs = document
        .get("interval_secs")
        .and_then(Value::as_u64)
        .unwrap_or(ATTESTATION_DEFAULT_INTERVAL_SECS);

    // A stale artifact repeats its last word with total confidence, so age is
    // checked before any field inside it is believed.
    let Some(written_raw) = document.get("written_at").and_then(Value::as_str) else {
        return unreadable(
            Boundary::Parse,
            format!(
                "{base_source} attestation carried no written_at, so its age cannot be bounded"
            ),
        );
    };
    let Ok(written_at) = DateTime::parse_from_rfc3339(written_raw) else {
        return unreadable(
            Boundary::Parse,
            format!("{base_source} attestation written_at is not RFC3339: {written_raw}"),
        );
    };
    let written_at = written_at.with_timezone(&Utc);
    let age_secs = now.signed_duration_since(written_at).num_seconds();
    // `age_secs` is signed and the ceiling below is a one-sided test, so an
    // artifact stamped in the future reads as fresh no matter how long its
    // writer has been dead — the staleness gate silently defeating itself on
    // exactly the host that needs it. A clock too far ahead is refused here,
    // and named as the clock fault it is rather than flattened into "the
    // attester stopped writing", which is a different and untrue claim.
    if age_secs < -ATTESTATION_MAX_CLOCK_SKEW_SECS {
        let ahead = age_secs.saturating_neg();
        return AttestationProbe {
            readable: false,
            boundary: Some(Boundary::Parse.as_str().to_owned()),
            source: format!(
                "{base_source} attestation was written {ahead}s in the future — the writer's \
                 clock disagrees with ours, so its age cannot be bounded"
            ),
            written_at: Some(written_raw.to_owned()),
            age_secs: Some(age_secs),
            interval_secs: Some(interval_secs),
            launchd_readable,
            ..AttestationProbe::default()
        };
    }
    let ceiling = ATTESTATION_STALENESS_FACTOR
        .saturating_mul(i64::try_from(interval_secs).unwrap_or(i64::MAX));
    if age_secs > ceiling {
        return AttestationProbe {
            readable: false,
            boundary: Some(Boundary::Transport.as_str().to_owned()),
            source: format!(
                "{base_source} attestation last wrote {age_secs}s ago, past the {ceiling}s ceiling \
                 — the attester stopped writing"
            ),
            written_at: Some(written_raw.to_owned()),
            age_secs: Some(age_secs),
            interval_secs: Some(interval_secs),
            launchd_readable,
            ..AttestationProbe::default()
        };
    }

    readable_attestation(
        document,
        base_source,
        written_raw,
        age_secs,
        interval_secs,
        launchd_readable,
    )
}

/// Build the probe for a document that survived every refusal above.
///
/// Reached only once the artifact has been proved current and its census
/// proved sighted, so every count here is an assertion rather than a default.
fn readable_attestation(
    document: &Value,
    base_source: &str,
    written_raw: &str,
    age_secs: i64,
    interval_secs: u64,
    launchd_readable: Option<bool>,
) -> AttestationProbe {
    AttestationProbe {
        readable: true,
        boundary: None,
        source: format!("{base_source} attestation"),
        written_at: Some(written_raw.to_owned()),
        age_secs: Some(age_secs),
        interval_secs: Some(interval_secs),
        launchd_readable,
        writer_sha256: document
            .get("generation")
            .and_then(|g| g.get("writer_sha256"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        persistent_runner_count: document
            .get("persistent_runners")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        jit_lane_count: document
            .get("jit_lanes")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        broken: broken_runners_from_document(document),
    }
}

/// Turn one host's attestation into the problems that should raise.
///
/// An unreadable attestation is a problem in its own right: the check exists to
/// notice a host that stopped serving, and a probe that cannot see is exactly
/// the state in which that failure hides.
fn attestation_problems(attestation: &AttestationProbe) -> Vec<String> {
    if !attestation.readable {
        let boundary = attestation.boundary.as_deref().unwrap_or("unknown");
        return vec![format!(
            "attestation_unreadable:{boundary}:{}",
            attestation.source
        )];
    }
    attestation
        .crash_looping()
        .map(|runner| {
            format!(
                "runner_crash_loop:{}:{} spawns, registered={}",
                runner.label, runner.runs, runner.registered
            )
        })
        .collect()
}

/// Problem identifiers a host raises when its own GitHub read did not answer.
///
/// `github_runners_scope_unreadable` belongs here as much as `github_unreadable`
/// does: both say the host could not see GitHub, neither says the host cannot
/// run work. Only the first of the two carries a reason, which is why an
/// unclassified boundary has to be a first-class outcome rather than an error.
const HOST_GITHUB_OBSERVATION_IDS: [&str; 2] =
    ["github_unreadable", "github_runners_scope_unreadable"];

/// The host problem's identifier and detail, for a string or object problem.
fn host_problem_parts(problem: &Value) -> Option<(String, String)> {
    if let Some(text) = problem.as_str() {
        let (id, detail) = text.split_once(':').unwrap_or((text, ""));
        return Some((id.to_owned(), detail.to_owned()));
    }
    let id = problem.get("id").and_then(Value::as_str)?;
    let detail = problem
        .get("detail")
        .or_else(|| problem.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some((id.to_owned(), detail.to_owned()))
}

/// How a host's failed GitHub read should be read, or `None` if the problem is
/// not a GitHub read at all.
fn host_github_observation_boundary(problem: &Value) -> Option<ReadBoundary> {
    let (id, detail) = host_problem_parts(problem)?;
    if !HOST_GITHUB_OBSERVATION_IDS.contains(&id.as_str()) {
        return None;
    }
    // A scope-unreadable problem names only the scope, never the cause, so its
    // detail classifies as unclassified rather than as a denial — which is the
    // correct reading: nothing about that string says GitHub refused.
    Some(classify_read_boundary(&detail))
}

/// Split a host's problems into the ones that keep its health verdict and the
/// ones the controller's own census already answered.
///
/// A failed GitHub read is demoted only when both halves hold: it did not
/// report a denial, and the repo-scope census independently found online lane
/// runners. Without corroboration every problem is retained, so a tick where
/// nothing was readable still fails closed.
fn partition_observation_gaps(
    problems: Vec<Value>,
    corroboration: LaneCorroboration,
) -> (Vec<Value>, Vec<DegradedObservation>) {
    let mut retained = Vec::new();
    let mut degraded = Vec::new();
    for problem in problems {
        let Some(boundary) = host_github_observation_boundary(&problem) else {
            retained.push(problem);
            continue;
        };
        if boundary.is_fleet_fact() || !corroboration.corroborates() {
            retained.push(problem);
            continue;
        }
        degraded.push(DegradedObservation {
            problem: problem
                .as_str()
                .map_or_else(|| problem.to_string(), str::to_owned),
            boundary,
            corroborated_by: corroboration.detail(),
        });
    }
    (retained, degraded)
}

/// Name every host observation the routable-slot count had to work around.
fn routing_degraded_reasons(hosts: &[HostFleetStatus]) -> Vec<String> {
    hosts
        .iter()
        .flat_map(|host| {
            let class = host.capacity.class.clone();
            host.degraded_observations.iter().map(move |gap| {
                format!(
                    "{class}: {} [{}] — {}",
                    gap.problem,
                    gap.boundary.as_str(),
                    gap.corroborated_by
                )
            })
        })
        .collect()
}

fn storage_probe_script(disk_path: &str) -> String {
    format!(
        "disk_path={}; printf 'disk_path\\t%s\\n' \"$disk_path\"; \
         df -Pk \"$disk_path\" 2>/dev/null | awk 'END {{print \"disk_available_kibibyte\\t\" $4}}'; \
         if command -v ccache >/dev/null 2>&1; then \
           ccache --print-stats 2>/dev/null | \
             awk -F '\\t' '$1 == \"cache_size_kibibyte\" || $1 == \"max_cache_size_kibibyte\" {{print}}'; \
         fi",
        shlex_quote(disk_path)
    )
}

fn storage_probe_from_output(output: &Output, fallback_path: &str) -> StorageProbe {
    let mut probe = StorageProbe {
        source: if output.status.success() {
            "host".to_owned()
        } else {
            format!("host exit {}", output.status.code().unwrap_or(-1))
        },
        disk_path: fallback_path.to_owned(),
        disk_floor_kibibyte: DEFAULT_DISK_FLOOR_KIBIBYTE,
        ..StorageProbe::default()
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((key, value)) = line.split_once('\t') else {
            continue;
        };
        match key {
            "disk_path" => value.clone_into(&mut probe.disk_path),
            "disk_available_kibibyte" => {
                probe.disk_available_kibibyte = value.trim().parse().ok();
            }
            "cache_size_kibibyte" => probe.ccache_size_kibibyte = value.trim().parse().ok(),
            "max_cache_size_kibibyte" => {
                probe.ccache_max_kibibyte = value.trim().parse().ok();
            }
            _ => {}
        }
    }
    probe.readable = output.status.success() && probe.disk_available_kibibyte.is_some();
    probe
}

/// Backoff before each retry of the repo-scope runner census.
///
/// The census is the one read every host's routability verdict leans on, so a
/// single blip must not decide it. Two retries is the largest budget that still
/// fits: the whole tick shares one [`FLEET_GITHUB_OBSERVATION_TIMEOUT`]
/// deadline with the active-run, merge-queue and release reads, and `run_gh`
/// clamps each call to whatever remains — so 1.25s of waiting plus a third
/// attempt is affordable where a longer ladder would starve the reads that
/// follow. Retries stop early on their own once the shared budget is spent, and
/// a denial is never retried at all.
const RUNNER_CENSUS_BACKOFF: [Duration; 2] =
    [Duration::from_millis(250), Duration::from_millis(1000)];

fn fetch_repository_runners(actions: &GitHubActions, repo: &str) -> RunnerInventory {
    fetch_repository_runners_with_backoff(actions, repo, &RUNNER_CENSUS_BACKOFF)
}

/// Read the census, retrying only failures a retry can fix.
///
/// The returned inventory always carries the boundary and the attempt count on
/// failure, so "one 403" and "three exhausted timeouts" never render the same.
fn fetch_repository_runners_with_backoff(
    actions: &GitHubActions,
    repo: &str,
    backoff: &[Duration],
) -> RunnerInventory {
    let mut attempt = 0_usize;
    loop {
        match fetch_repository_runners_once(actions, repo) {
            Ok(inventory) => {
                return RunnerInventory {
                    attempts: u32::try_from(attempt + 1).unwrap_or(u32::MAX),
                    ..inventory
                };
            }
            Err(failure) => {
                let boundary = classify_read_boundary(&failure);
                if !boundary.worth_retrying() || attempt >= backoff.len() {
                    return RunnerInventory {
                        source: format!("{failure} [{}]", boundary.as_str()),
                        boundary: Some(boundary),
                        attempts: u32::try_from(attempt + 1).unwrap_or(u32::MAX),
                        ..RunnerInventory::default()
                    };
                }
                thread::sleep(backoff[attempt]);
                attempt += 1;
            }
        }
    }
}

fn fetch_repository_runners_once(
    actions: &GitHubActions,
    repo: &str,
) -> Result<RunnerInventory, String> {
    let raw = actions.run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        format!("repos/{repo}/actions/runners?per_page=100"),
        "--jq".to_owned(),
        ".runners[]".to_owned(),
    ]);
    let raw = match raw {
        Ok(raw) => raw,
        Err(error) => {
            return Err(format!("github runners unreadable: {error}"));
        }
    };
    let mut runners = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                return Err(format!("github runner JSON malformed: {error}"));
            }
        };
        runners.push(RepositoryRunner {
            id: value.get("id").and_then(Value::as_u64).unwrap_or_default(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            status: value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            busy: value.get("busy").and_then(Value::as_bool).unwrap_or(false),
            labels: value
                .get("labels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|label| label.get("name").and_then(Value::as_str))
                .map(str::to_owned)
                .collect(),
        });
    }
    Ok(RunnerInventory {
        readable: true,
        source: "github".to_owned(),
        boundary: None,
        attempts: 1,
        runners,
    })
}

fn detect_routing_mismatches(
    runs: &[ActiveRunObservation],
    inventory: &RunnerInventory,
) -> Vec<RoutingMismatch> {
    if !inventory.readable {
        return Vec::new();
    }
    let candidates = inventory
        .runners
        .iter()
        .filter(|runner| runner.status.eq_ignore_ascii_case("online") && !runner.busy)
        .filter(|runner| {
            let labels = runner
                .labels
                .iter()
                .map(|label| label.to_ascii_lowercase())
                .collect::<BTreeSet<_>>();
            labels.contains("self-hosted") && labels.contains("linux") && labels.contains("x64")
        })
        .map(|runner| runner.name.clone())
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Vec::new();
    }
    runs.iter()
        .filter(|run| {
            run.workflow == "Build and Test" && run.head_branch.starts_with("gh-readonly-queue/")
        })
        .flat_map(|run| {
            let candidates = candidates.clone();
            run.jobs.iter().filter_map(move |job| {
                let active = matches!(job.status.as_str(), "queued" | "in_progress");
                let hosted_linux = job
                    .labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case("ubuntu-latest"));
                (active && hosted_linux && job.name.to_ascii_lowercase().contains("linux"))
                    .then(|| RoutingMismatch {
                        run_id: run.run_id,
                        workflow: run.workflow.clone(),
                        job: job.name.clone(),
                        requested_labels: job.labels.clone(),
                        idle_candidates: candidates.clone(),
                        reason: "merge-group Linux build uses ubuntu-latest while self-hosted Linux x64 runners are idle".to_owned(),
                    })
            })
        })
        .collect()
}

/// Classify every queued job the observation window reached, and return the
/// ones whose verdict raises.
///
/// This is the caller side of [`assess_queued_job`]: the pure classifier had no
/// live caller, so a run GitHub would neither schedule nor cancel raised
/// nothing. Demand is a precondition of the assertion by construction — its
/// input *is* a queued job — so an idle lane with nothing queued produces no
/// report and cannot be mistaken for a fault.
///
/// Two deliberate narrowings, both of which lose real wedges rather than
/// manufacture false ones:
///
/// * Only a wholly queued run is classified. A job can enter the queue long
///   after its workflow starts, and the run's `created_at` is the only age
///   proxy available, so using it on a partially started run would read
///   upstream runtime as queue age.
/// * A job requesting no labels is skipped. `advertises_all` is a superset
///   test and is vacuously true on an empty slice, so an unlabelled job would
///   match every registered runner and could be reported as wedged on the
///   strength of a runner that could never have run it.
///
/// The census behind `inventory` is repository-scoped, while the lane-service
/// vocabulary asks for both scopes. A missed org runner yields
/// `capable_runner_online = false`, which classifies as `NoCapableRunner` and
/// verdict `Served` — so the gap can hide a real wedge and cannot invent one.
fn detect_wedged_queued_jobs(
    runs: &[ActiveRunObservation],
    inventory: &RunnerInventory,
    thresholds: WedgeThresholds,
    now: DateTime<Utc>,
) -> WedgedQueuedJobs {
    // A census that could not be read is not a census that found nothing.
    if !inventory.readable {
        return WedgedQueuedJobs::default();
    }
    let registered = inventory
        .runners
        .iter()
        .map(|runner| RegisteredRunner {
            name: runner.name.clone(),
            scope: RunnerScope::Repo,
            online: runner.status.eq_ignore_ascii_case("online"),
            busy: runner.busy,
            labels: runner.labels.clone(),
        })
        .collect::<Vec<_>>();

    let mut examined = 0usize;
    let mut raising: Vec<QueuedJobReport> = Vec::new();
    for run in runs {
        if run.status != "queued" {
            continue;
        }
        let Some(queued_since) = run
            .created_at
            .as_deref()
            .and_then(|created_at| DateTime::parse_from_rfc3339(created_at).ok())
            .map(|ts| ts.with_timezone(&Utc))
        else {
            continue;
        };
        for job in &run.jobs {
            if job.status != "queued" || job.labels.is_empty() {
                continue;
            }
            examined += 1;
            let capable = registered
                .iter()
                .filter(|runner| runner.online && runner.advertises_all(&job.labels))
                .collect::<Vec<_>>();
            let report = assess_queued_job(
                &QueuedJobObservation {
                    run_id: run.run_id.to_string(),
                    name: job.name.clone(),
                    queued_since,
                    capable_runner_online: !capable.is_empty(),
                    capable_runner_idle: capable.iter().any(|runner| !runner.busy),
                    // Nothing in the observation model carries a
                    // cancellation-request timestamp, so the unclearable state
                    // is unreachable from this caller rather than inferred.
                    cancel_requested_at: None,
                },
                thresholds,
                now,
            );
            if report.verdict.is_raise() {
                raising.push(report);
            }
        }
    }
    WedgedQueuedJobs { examined, raising }
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
    let mut parts = vec!["env".to_owned(), format!("PATH={REMOTE_OBSERVER_PATH}")];
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

#[cfg(test)]
mod tests;
