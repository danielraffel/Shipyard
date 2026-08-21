//! Durable, host-independent rollout of one immutable Shipyard release.
//!
//! A semantic version is not a sufficient fleet identity: a rebuilt or
//! replaced release asset can carry the same version and different behavior.
//! This command therefore converges on `(version, installed binary sha256)`.
//! Hosts are probed and updated independently. Offline and busy hosts remain
//! pending in durable state while eligible peers continue; a controller-local
//! `LaunchAgent` retries the same state so an offline Mac converges on rejoin.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    CliFailure,
    cli::{
        FleetCommand, FleetReleaseApplyArgs, FleetReleaseCommand, FleetReleaseResumeArgs,
        FleetReleaseTargetArgs,
    },
};
use crate::{paths::RuntimePaths, process::ProcessTree};

const STATE_SCHEMA: u32 = 1;
const INVENTORY_SCHEMA: u32 = 1;
const RECONCILER_LABEL: &str = "com.shipyard.fleet-release";
const DEFAULT_TIMEOUT_SECS: u64 = 20;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
struct ReleaseIdentity {
    version: String,
    sha256: String,
}

impl ReleaseIdentity {
    fn new(version: &str, sha256: &str) -> Result<Self, CliFailure> {
        let raw = version.trim();
        let version = raw.strip_prefix('v').unwrap_or(raw).to_owned();
        let components = version.split('.').collect::<Vec<_>>();
        if components.len() != 3
            || !components
                .iter()
                .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
        {
            return Err(CliFailure::new(
                2,
                format!("invalid release version `{version}`; expected numeric X.Y.Z"),
            ));
        }
        let sha256 = sha256.trim().to_ascii_lowercase();
        if sha256.len() != 64 || !sha256.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(CliFailure::new(
                2,
                "--sha256 must be the 64-character SHA-256 of the installed binary",
            ));
        }
        Ok(Self { version, sha256 })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
struct FleetHost {
    name: String,
    #[serde(default)]
    ssh: Option<String>,
    #[serde(default)]
    local: bool,
    #[serde(default)]
    canary: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HostInventoryFile {
    Legacy(Vec<FleetHost>),
    Envelope {
        schema_version: u32,
        hosts: Vec<FleetHost>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum HostState {
    Pending,
    Offline,
    Busy,
    Converged,
    Failed,
    Unobservable,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum RolloutDirection {
    #[default]
    Forward,
    Rollback,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
struct HostObservation {
    reachable: bool,
    version: Option<String>,
    sha256: Option<String>,
    daemon_running: Option<bool>,
    daemon_version: Option<String>,
    participation: Option<bool>,
    busy: Option<bool>,
    detail: String,
}

impl HostObservation {
    fn disposition(&self, desired: &ReleaseIdentity) -> HostState {
        if !self.reachable {
            return HostState::Offline;
        }
        if self.version.is_none()
            || self.sha256.is_none()
            || self.daemon_running.is_none()
            || self.participation.is_none()
            || self.busy.is_none()
        {
            return HostState::Unobservable;
        }
        let daemon_ok = self.daemon_running == Some(false)
            || self.daemon_version.as_deref() == Some(desired.version.as_str());
        if self.version.as_deref() == Some(desired.version.as_str())
            && self.sha256.as_deref() == Some(desired.sha256.as_str())
            && daemon_ok
        {
            HostState::Converged
        } else if self.busy == Some(true) {
            HostState::Busy
        } else {
            HostState::Pending
        }
    }

    fn disposition_after_update(
        &self,
        desired: &ReleaseIdentity,
        daemon_was_running: bool,
    ) -> HostState {
        let state = self.disposition(desired);
        if state == HostState::Converged && daemon_was_running && self.daemon_running != Some(true)
        {
            HostState::Failed
        } else {
            state
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
struct HostReceipt {
    state: HostState,
    observed: HostObservation,
    #[serde(default)]
    expected_participation: Option<bool>,
    #[serde(default)]
    require_daemon_running: bool,
    updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
struct ReconcilerReceipt {
    installed: bool,
    loaded: bool,
    detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
struct RolloutState {
    schema_version: u32,
    generation: String,
    #[serde(default)]
    direction: RolloutDirection,
    desired: ReleaseIdentity,
    rollback: Option<ReleaseIdentity>,
    hosts_file: PathBuf,
    inventory_sha256: String,
    canary_host: Option<String>,
    canary_proven: bool,
    hosts: BTreeMap<String, HostReceipt>,
    reconciler: ReconcilerReceipt,
    updated_at: String,
}

#[derive(Clone, Debug, Serialize)]
struct FleetReleaseReport {
    command: String,
    generation: String,
    desired: ReleaseIdentity,
    rollback: Option<ReleaseIdentity>,
    canary_host: Option<String>,
    canary_proven: bool,
    wave: Vec<String>,
    hosts: BTreeMap<String, HostReceipt>,
    reconciler: ReconcilerReceipt,
    complete: bool,
}

trait HostExecutor: Sync {
    fn probe(&self, host: &FleetHost) -> HostObservation;
    fn install(
        &self,
        host: &FleetHost,
        desired: &ReleaseIdentity,
        before: &HostObservation,
    ) -> Result<(), String>;
}

#[derive(Clone, Copy)]
struct ProductionExecutor;

pub(super) fn fleet_command<W: Write>(
    command: FleetCommand,
    paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let FleetCommand::Release { command } = command;
    match command {
        FleetReleaseCommand::Status(args) => status_or_plan(&args, paths, false, json, stdout),
        FleetReleaseCommand::Plan(args) => status_or_plan(&args, paths, true, json, stdout),
        FleetReleaseCommand::Apply(args) => apply(&args, paths, json, stdout),
        FleetReleaseCommand::Rollback(args) => resume(&args, paths, true, json, stdout),
        FleetReleaseCommand::Reconcile(args) => resume(&args, paths, false, json, stdout),
    }
}

fn status_or_plan<W: Write>(
    args: &FleetReleaseTargetArgs,
    paths: &RuntimePaths,
    plan: bool,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let desired = ReleaseIdentity::new(&args.target, &args.sha256)?;
    let hosts_file = resolve_hosts_file(args.hosts_file.as_deref(), paths);
    let hosts = load_hosts(&hosts_file)?;
    let observations = probe_hosts(&ProductionExecutor, &hosts);
    let now = timestamp();
    let receipts = receipt_map(&hosts, observations, &desired, &now);
    let canary = select_canary(&hosts, &receipts, None, false);
    let canary_proven = receipts
        .get(&canary)
        .is_some_and(|receipt| receipt.state == HostState::Converged);
    let wave = next_wave(&hosts, &receipts, &canary, canary_proven);
    let report = FleetReleaseReport {
        command: if plan {
            "fleet:release:plan"
        } else {
            "fleet:release:status"
        }
        .to_owned(),
        generation: generation(&desired, &now),
        desired,
        rollback: None,
        canary_host: Some(canary),
        canary_proven,
        complete: receipts
            .values()
            .all(|receipt| receipt.state == HostState::Converged),
        wave,
        hosts: receipts,
        reconciler: inspect_reconciler(paths),
    };
    render_report(stdout, json, &report)?;
    Ok(if report.complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    })
}

fn apply<W: Write>(
    args: &FleetReleaseApplyArgs,
    paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let desired = ReleaseIdentity::new(&args.target.target, &args.target.sha256)?;
    let rollback = Some(ReleaseIdentity::new(
        &args.rollback_target,
        &args.rollback_sha256,
    )?);
    let hosts_file = resolve_hosts_file(args.target.hosts_file.as_deref(), paths);
    let state_file = resolve_state_file(args.target.state_file.as_deref(), paths);
    let _lock = StateLock::acquire(&fleet_lock_file(paths))?;
    let hosts = load_hosts(&hosts_file)?;
    let inventory_sha256 = file_sha256(&hosts_file)?;
    let now = timestamp();
    let observations = probe_hosts(&ProductionExecutor, &hosts);
    let receipts = receipt_map(&hosts, observations, &desired, &now);
    let canary = select_canary(&hosts, &receipts, None, false);
    let mut state = RolloutState {
        schema_version: STATE_SCHEMA,
        generation: generation(&desired, &now),
        direction: RolloutDirection::Forward,
        desired,
        rollback,
        hosts_file: canonical_or_original(&hosts_file),
        inventory_sha256,
        canary_host: Some(canary),
        canary_proven: false,
        hosts: receipts,
        reconciler: ReconcilerReceipt {
            installed: false,
            loaded: false,
            detail: "not requested".to_owned(),
        },
        updated_at: now,
    };
    write_state(&state_file, &state)?;
    if !args.no_reconciler {
        state.reconciler = install_reconciler(paths, &state_file, &state.hosts_file)?;
        write_state(&state_file, &state)?;
    }
    reconcile_once(&ProductionExecutor, &hosts, &state_file, &mut state)?;
    let report = report_from_state("fleet:release:apply", &hosts, &state);
    render_report(stdout, json, &report)?;
    Ok(if report.complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    })
}

fn resume<W: Write>(
    args: &FleetReleaseResumeArgs,
    paths: &RuntimePaths,
    rollback: bool,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let state_file = resolve_state_file(args.state_file.as_deref(), paths);
    let _lock = StateLock::acquire(&fleet_lock_file(paths))?;
    let mut state = read_state(&state_file)?;
    if let Some(override_file) = args.hosts_file.as_deref()
        && canonical_or_original(override_file) != canonical_or_original(&state.hosts_file)
    {
        return Err(CliFailure::new(
            2,
            "cannot change inventory during a persisted rollout; start a new apply generation",
        ));
    }
    let hosts_file = state.hosts_file.clone();
    if file_sha256(&hosts_file)? != state.inventory_sha256 {
        return Err(CliFailure::new(
            2,
            "fleet inventory changed during a persisted rollout; start a new apply generation",
        ));
    }
    let hosts = load_hosts(&hosts_file)?;
    if rollback {
        activate_rollback(&mut state)?;
        write_state(&state_file, &state)?;
    }
    reconcile_once(&ProductionExecutor, &hosts, &state_file, &mut state)?;
    let report = report_from_state(
        if rollback {
            "fleet:release:rollback"
        } else {
            "fleet:release:reconcile"
        },
        &hosts,
        &state,
    );
    render_report(stdout, json, &report)?;
    Ok(if report.complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(3)
    })
}

fn reconcile_once<E: HostExecutor>(
    executor: &E,
    hosts: &[FleetHost],
    state_file: &Path,
    state: &mut RolloutState,
) -> Result<(), CliFailure> {
    let now = timestamp();
    let observations = probe_hosts(executor, hosts);
    let prior_expectations = state
        .hosts
        .iter()
        .map(|(name, receipt)| {
            (
                name.clone(),
                (
                    receipt.expected_participation,
                    receipt.require_daemon_running,
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let terminal_failures = state
        .hosts
        .iter()
        .filter(|(_, receipt)| receipt.state == HostState::Failed)
        .map(|(name, receipt)| (name.clone(), receipt.clone()))
        .collect::<BTreeMap<_, _>>();
    state.hosts = receipt_map(hosts, observations, &state.desired, &now);
    for (name, receipt) in &mut state.hosts {
        let Some((expected_participation, require_daemon_running)) =
            prior_expectations.get(name).copied()
        else {
            continue;
        };
        receipt.expected_participation = expected_participation;
        receipt.require_daemon_running = require_daemon_running;
        let participation_changed = matches!(
            (expected_participation, receipt.observed.participation),
            (Some(expected), Some(actual)) if expected != actual
        );
        if participation_changed {
            receipt.state = HostState::Failed;
            "runner participation changed during release rollout"
                .clone_into(&mut receipt.observed.detail);
        } else {
            receipt.state = receipt
                .observed
                .disposition_after_update(&state.desired, require_daemon_running);
            if receipt.state == HostState::Failed && require_daemon_running {
                "previously running daemon did not remain running at the target version"
                    .clone_into(&mut receipt.observed.detail);
            }
        }
        if receipt.state == HostState::Converged {
            receipt.expected_participation = None;
            receipt.require_daemon_running = false;
        }
    }
    for (name, receipt) in terminal_failures {
        if hosts.iter().any(|host| host.name == name) {
            state.hosts.insert(name, receipt);
        }
    }
    let canary = select_canary(
        hosts,
        &state.hosts,
        state.canary_host.as_deref(),
        state.canary_proven,
    );
    state.canary_host = Some(canary.clone());
    let mut wave = next_wave(hosts, &state.hosts, &canary, state.canary_proven);
    persist_wave_expectations(state_file, state, &wave)?;
    let desired = state.desired.clone();
    apply_wave(executor, hosts, &mut state.hosts, &desired, &wave);

    if !state.canary_proven
        && state
            .hosts
            .get(&canary)
            .is_some_and(|receipt| receipt.state == HostState::Converged)
    {
        state.canary_proven = true;
        wave = next_wave(hosts, &state.hosts, &canary, true);
        persist_wave_expectations(state_file, state, &wave)?;
        apply_wave(executor, hosts, &mut state.hosts, &desired, &wave);
    }
    state.updated_at = timestamp();
    state.reconciler = inspect_reconciler_from_prior(&state.reconciler, state_file);
    write_state(state_file, state)
}

fn persist_wave_expectations(
    state_file: &Path,
    state: &mut RolloutState,
    wave: &[String],
) -> Result<(), CliFailure> {
    for name in wave {
        let Some(receipt) = state.hosts.get_mut(name) else {
            continue;
        };
        if receipt.expected_participation.is_none() {
            receipt.expected_participation = receipt.observed.participation;
        }
        receipt.require_daemon_running |= receipt.observed.daemon_running == Some(true);
        receipt.updated_at = timestamp();
    }
    state.updated_at = timestamp();
    write_state(state_file, state)
}

fn apply_wave<E: HostExecutor>(
    executor: &E,
    hosts: &[FleetHost],
    receipts: &mut BTreeMap<String, HostReceipt>,
    desired: &ReleaseIdentity,
    wave: &[String],
) {
    let selected = hosts
        .iter()
        .filter(|host| wave.contains(&host.name))
        .cloned()
        .collect::<Vec<_>>();
    let mut results = Vec::new();
    thread::scope(|scope| {
        let handles = selected
            .iter()
            .map(|host| {
                let before = receipts.get(&host.name).cloned();
                scope.spawn(move || apply_host(executor, host, before, desired))
            })
            .collect::<Vec<_>>();
        for handle in handles {
            if let Ok(result) = handle.join() {
                results.push(result);
            }
        }
    });
    let now = timestamp();
    for (
        name,
        observation,
        install_error,
        participation_changed,
        daemon_was_running,
        expected_participation,
    ) in results
    {
        let Some(observation) = observation else {
            continue;
        };
        let mut state = if participation_changed {
            HostState::Failed
        } else {
            observation.disposition_after_update(desired, daemon_was_running)
        };
        let mut observation = observation;
        if participation_changed {
            "runner participation changed during release rollout"
                .clone_into(&mut observation.detail);
        } else if state == HostState::Failed && daemon_was_running {
            "previously running daemon did not remain running at the target version"
                .clone_into(&mut observation.detail);
        } else if let Some(error) = install_error {
            if state == HostState::Converged {
                observation.detail = format!("converged despite transient install error: {error}");
            } else {
                observation.detail =
                    format!("install attempt was inconclusive and will retry: {error}");
                // Transport and installer failures are retryable. Only explicit
                // invariant violations above become terminal receipts.
                state = observation.disposition(desired);
            }
        }
        let keep_expectations = state != HostState::Converged;
        receipts.insert(
            name,
            HostReceipt {
                state,
                observed: observation,
                expected_participation: keep_expectations
                    .then_some(expected_participation)
                    .flatten(),
                require_daemon_running: keep_expectations && daemon_was_running,
                updated_at: now.clone(),
            },
        );
    }
}

type ApplyHostResult = (
    String,
    Option<HostObservation>,
    Option<String>,
    bool,
    bool,
    Option<bool>,
);

fn apply_host<E: HostExecutor>(
    executor: &E,
    host: &FleetHost,
    before: Option<HostReceipt>,
    desired: &ReleaseIdentity,
) -> ApplyHostResult {
    let Some(before) = before else {
        return (
            host.name.clone(),
            None,
            Some("missing pre-update probe".to_owned()),
            false,
            false,
            None,
        );
    };
    let participation_before = before
        .expected_participation
        .or(before.observed.participation);
    // The fleet-wide probe only plans the wave. Re-authorize this host
    // immediately before mutation because its runner can claim work while
    // another host is being inspected.
    let authorization = executor.probe(host);
    let daemon_was_running = before.require_daemon_running
        || before.observed.daemon_running == Some(true)
        || authorization.daemon_running == Some(true);
    let participation_changed = matches!(
        (participation_before, authorization.participation),
        (Some(before), Some(after)) if before != after
    );
    if participation_changed
        || !authorization.reachable
        || authorization.busy != Some(false)
        || authorization.disposition(desired) == HostState::Unobservable
    {
        return (
            host.name.clone(),
            Some(authorization),
            None,
            participation_changed,
            daemon_was_running,
            participation_before,
        );
    }
    let install_error = executor.install(host, desired, &authorization).err();
    let after = executor.probe(host);
    let participation_changed = matches!(
        (participation_before, after.participation),
        (Some(before), Some(after)) if before != after
    );
    (
        host.name.clone(),
        Some(after),
        install_error,
        participation_changed,
        daemon_was_running,
        participation_before,
    )
}

fn probe_hosts<E: HostExecutor>(
    executor: &E,
    hosts: &[FleetHost],
) -> Vec<(String, HostObservation)> {
    let mut observations = Vec::with_capacity(hosts.len());
    thread::scope(|scope| {
        let handles = hosts
            .iter()
            .map(|host| scope.spawn(move || (host.name.clone(), executor.probe(host))))
            .collect::<Vec<_>>();
        for handle in handles {
            if let Ok(observation) = handle.join() {
                observations.push(observation);
            }
        }
    });
    observations.sort_by(|left, right| left.0.cmp(&right.0));
    observations
}

fn receipt_map(
    hosts: &[FleetHost],
    observations: Vec<(String, HostObservation)>,
    desired: &ReleaseIdentity,
    now: &str,
) -> BTreeMap<String, HostReceipt> {
    let allowed = hosts.iter().map(|host| &host.name).collect::<BTreeSet<_>>();
    observations
        .into_iter()
        .filter(|(name, _)| allowed.contains(name))
        .map(|(name, observed)| {
            let state = observed.disposition(desired);
            (
                name,
                HostReceipt {
                    state,
                    observed,
                    expected_participation: None,
                    require_daemon_running: false,
                    updated_at: now.to_owned(),
                },
            )
        })
        .collect()
}

fn select_canary(
    hosts: &[FleetHost],
    receipts: &BTreeMap<String, HostReceipt>,
    prior: Option<&str>,
    prior_proven: bool,
) -> String {
    if let Some(prior) = prior
        && hosts.iter().any(|host| host.name == prior)
        && (prior_proven
            || receipts.get(prior).is_some_and(|receipt| {
                matches!(receipt.state, HostState::Pending | HostState::Converged)
            }))
    {
        return prior.to_owned();
    }
    // A declared canary wins when it can actually run. Otherwise choose any
    // reachable, idle host. This preserves a canary gate without letting one
    // offline laptop serialize the whole fleet.
    hosts
        .iter()
        .filter(|host| host.canary)
        .chain(hosts.iter())
        .find(|host| {
            receipts.get(&host.name).is_some_and(|receipt| {
                matches!(receipt.state, HostState::Pending | HostState::Converged)
            })
        })
        .or_else(|| hosts.first())
        .map_or_else(|| "unknown".to_owned(), |host| host.name.clone())
}

fn next_wave(
    hosts: &[FleetHost],
    receipts: &BTreeMap<String, HostReceipt>,
    canary: &str,
    canary_proven: bool,
) -> Vec<String> {
    hosts
        .iter()
        .filter(|host| canary_proven || host.name == canary)
        .filter(|host| {
            receipts
                .get(&host.name)
                .is_some_and(|receipt| receipt.state == HostState::Pending)
        })
        .map(|host| host.name.clone())
        .collect()
}

impl HostExecutor for ProductionExecutor {
    fn probe(&self, host: &FleetHost) -> HostObservation {
        let output = run_host_script(host, PROBE_SCRIPT, DEFAULT_TIMEOUT_SECS);
        let Ok(output) = output else {
            return HostObservation {
                reachable: false,
                version: None,
                sha256: None,
                daemon_running: None,
                daemon_version: None,
                participation: None,
                busy: None,
                detail: output
                    .err()
                    .unwrap_or_else(|| "host probe failed".to_owned()),
            };
        };
        if !output.status.success() {
            let transport_failed = !host.local && output.status.code() == Some(255);
            return HostObservation {
                reachable: !transport_failed,
                version: None,
                sha256: None,
                daemon_running: None,
                daemon_version: None,
                participation: None,
                busy: None,
                detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            };
        }
        parse_probe(&String::from_utf8_lossy(&output.stdout))
    }

    fn install(
        &self,
        host: &FleetHost,
        desired: &ReleaseIdentity,
        before: &HostObservation,
    ) -> Result<(), String> {
        let binary_matches = before.version.as_deref() == Some(desired.version.as_str())
            && before.sha256.as_deref() == Some(desired.sha256.as_str());
        let daemon_was_running = before.daemon_running == Some(true);
        let daemon_needs_refresh = daemon_was_running
            && before.daemon_version.as_deref() != Some(desired.version.as_str());
        let mutation_required = !binary_matches || daemon_needs_refresh;
        let installer_source = shell_quote(include_str!("../../install.sh"));
        let script = format!(
            r#"set -eu
bin="$HOME/.local/bin/shipyard"
pool="$HOME/.local/bin/tartci-pool"
restore_pool=0
restore_participation() {{
  if [ "$restore_pool" = 1 ]; then "$pool" on >/dev/null; fi
}}
trap restore_participation EXIT
if [ {mutate} = 1 ]; then
  if pgrep -f '[R]unner.Worker' >/dev/null 2>&1; then
    echo "runner became busy before Shipyard installation; deferring" >&2
    exit 75
  fi
  if [ {participating} = 1 ]; then
    "$pool" off >/dev/null
    restore_pool=1
    if pgrep -f '[R]unner.Worker' >/dev/null 2>&1; then
      echo "runner claimed work while entering the release drain; deferring" >&2
      exit 75
    fi
  fi
  if [ {install} = 1 ]; then
    printf '%s' {installer_source} | SHIPYARD_VERSION={version} SHIPYARD_EXPECTED_BINARY_SHA256={sha256} bash
  fi
  if [ {daemon} = 1 ]; then "$bin" daemon refresh >/dev/null; fi
  if [ "$restore_pool" = 1 ]; then "$pool" on >/dev/null; restore_pool=0; fi
fi
"$bin" --version >/dev/null
trap - EXIT
"#,
            version = shell_quote(&desired.version),
            sha256 = shell_quote(&desired.sha256),
            installer_source = installer_source,
            install = u8::from(!binary_matches),
            daemon = u8::from(daemon_needs_refresh),
            mutate = u8::from(mutation_required),
            participating = u8::from(before.participation == Some(true)),
        );
        let output = run_host_script(host, &script, 300);
        if mutation_required && before.participation == Some(true) {
            let restore = run_host_script(
                host,
                r#"pool="$HOME/.local/bin/tartci-pool"; [ -x "$pool" ] && "$pool" on >/dev/null"#,
                20,
            )?;
            if !restore.status.success() {
                return Err("failed to restore runner participation after rollout".to_owned());
            }
        }
        let output = output?;
        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            Err(if stderr.is_empty() {
                "Shipyard installer exited non-zero".to_owned()
            } else {
                stderr
            })
        }
    }
}

const PROBE_SCRIPT: &str = r#"set -u
bin="$HOME/.local/bin/shipyard"
pool="$HOME/.local/bin/tartci-pool"
if [ -x "$bin" ]; then
  version=$("$bin" --version 2>/dev/null | awk '{print $NF}' | sed 's/^v//')
  if command -v shasum >/dev/null 2>&1; then
    sha=$(shasum -a 256 "$bin" 2>/dev/null | awk '{print $1}')
  elif command -v sha256sum >/dev/null 2>&1; then
    sha=$(sha256sum "$bin" 2>/dev/null | awk '{print $1}')
  else
    sha=$(openssl dgst -sha256 "$bin" 2>/dev/null | awk '{print $NF}')
  fi
  daemon_json=$("$bin" --json daemon status 2>/dev/null || true)
  daemon_one=$(printf '%s' "$daemon_json" | tr '\n' ' ')
  daemon_running=$(printf '%s' "$daemon_one" | sed -n 's/.*"running"[[:space:]]*:[[:space:]]*true.*/true/p')
  if [ -z "$daemon_running" ]; then
    daemon_running=$(printf '%s' "$daemon_one" | sed -n 's/.*"running"[[:space:]]*:[[:space:]]*false.*/false/p')
  fi
  daemon_version=$(printf '%s' "$daemon_one" | sed -n 's/.*"shipyard_version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
else
  version=missing
  sha=missing
  daemon_running=false
  daemon_version=
fi
participation=unknown
if [ -x "$pool" ]; then
  first=$("$pool" status 2>/dev/null | head -1 || true)
  case "$first" in *"participate flag: true"*) participation=true;; *"participate flag: false"*) participation=false;; esac
fi
busy=false
if pgrep -f 'Runner.Worker spawnclient' >/dev/null 2>&1; then busy=true; fi
if ps -axww -o command= 2>/dev/null \
  | grep -E '[/ ](shipyard|sy)( +--json| +--mode +[^ ]+| +--state-dir +[^ ]+| +--global-dir +[^ ]+| +--cwd +[^ ]+)* +(ship|pr|run|rescue|auto-merge|watch)( |$)' \
  | grep -v '[g]rep -E' >/dev/null 2>&1; then busy=true; fi
printf 'version=%s\nsha256=%s\ndaemon_running=%s\ndaemon_version=%s\nparticipation=%s\nbusy=%s\n' \
  "$version" "$sha" "$daemon_running" "$daemon_version" "$participation" "$busy"
"#;

fn run_host_script(
    host: &FleetHost,
    script: &str,
    timeout_secs: u64,
) -> Result<std::process::Output, String> {
    let mut command = if host.local {
        let mut command = Command::new("/bin/sh");
        command.args(["-lc", script]);
        command
    } else {
        let ssh = host
            .ssh
            .as_deref()
            .ok_or_else(|| format!("host {} has neither local=true nor ssh", host.name))?;
        let mut command = Command::new("ssh");
        command.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=8",
            ssh,
            "/bin/sh",
            "-lc",
            &shell_quote(script),
        ]);
        command
    };
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    run_with_timeout(&mut command, Duration::from_secs(timeout_secs))
}

fn run_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut child = ProcessTree::spawn(command).map_err(|error| error.to_string())?;
    let stdout_reader = child.take_stdout().map(|mut pipe| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    });
    let stderr_reader = child.take_stderr().map(|mut pipe| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = child.wait();
                return Ok(std::process::Output {
                    status,
                    stdout: stdout_reader
                        .and_then(|reader| reader.join().ok())
                        .unwrap_or_default(),
                    stderr: stderr_reader
                        .and_then(|reader| reader.join().ok())
                        .unwrap_or_default(),
                });
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                child.terminate();
                return Err(format!(
                    "host command timed out after {}s",
                    timeout.as_secs()
                ));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn parse_probe(raw: &str) -> HostObservation {
    let values = raw
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect::<BTreeMap<_, _>>();
    let bool_value = |key: &str| match values.get(key).copied() {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    let string_value = |key: &str| {
        values
            .get(key)
            .filter(|value| !value.is_empty())
            .map(|value| (*value).to_owned())
    };
    HostObservation {
        reachable: true,
        version: string_value("version"),
        sha256: string_value("sha256").map(|sha| sha.to_ascii_lowercase()),
        daemon_running: bool_value("daemon_running"),
        daemon_version: string_value("daemon_version"),
        participation: bool_value("participation"),
        busy: bool_value("busy"),
        detail: "probe completed".to_owned(),
    }
}

fn load_hosts(path: &Path) -> Result<Vec<FleetHost>, CliFailure> {
    let raw = fs::read_to_string(path).map_err(|error| {
        CliFailure::new(
            2,
            format!("cannot read fleet hosts {}: {error}", path.display()),
        )
    })?;
    let inventory: HostInventoryFile = serde_json::from_str(&raw).map_err(|error| {
        CliFailure::new(
            2,
            format!("cannot parse fleet hosts {}: {error}", path.display()),
        )
    })?;
    let (mut hosts, implicit_local) = match inventory {
        HostInventoryFile::Legacy(hosts) => (hosts, true),
        HostInventoryFile::Envelope {
            schema_version,
            hosts,
        } => {
            if schema_version != INVENTORY_SCHEMA {
                return Err(CliFailure::new(
                    2,
                    format!("unsupported fleet-hosts schema {schema_version}"),
                ));
            }
            (hosts, false)
        }
    };
    if hosts.is_empty() {
        return Err(CliFailure::new(2, "fleet host inventory is empty"));
    }
    if implicit_local && !hosts.iter().any(|host| host.local) {
        hosts.insert(
            0,
            FleetHost {
                name: local_host_name(),
                ssh: None,
                local: true,
                canary: true,
            },
        );
    }
    let mut names = BTreeSet::new();
    let mut local_count = 0usize;
    for host in &hosts {
        if host.name.trim().is_empty() || !names.insert(host.name.to_ascii_lowercase()) {
            return Err(CliFailure::new(
                2,
                "fleet host names must be non-empty and unique",
            ));
        }
        if host.local {
            local_count += 1;
        } else if host.ssh.as_deref().is_none_or(str::is_empty) {
            return Err(CliFailure::new(
                2,
                format!("remote fleet host {} requires an ssh alias", host.name),
            ));
        } else if host
            .ssh
            .as_deref()
            .is_some_and(|ssh| ssh.trim_start().starts_with('-'))
        {
            return Err(CliFailure::new(
                2,
                format!("remote fleet host {} has an unsafe ssh alias", host.name),
            ));
        }
    }
    if local_count != 1 {
        return Err(CliFailure::new(
            2,
            "fleet inventory must resolve exactly one local host",
        ));
    }
    Ok(hosts)
}

fn local_host_name() -> String {
    Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "local".to_owned())
}

fn resolve_hosts_file(explicit: Option<&Path>, paths: &RuntimePaths) -> PathBuf {
    explicit.map_or_else(
        || paths.global_dir.join("fleet-hosts.json"),
        Path::to_path_buf,
    )
}

fn resolve_state_file(explicit: Option<&Path>, paths: &RuntimePaths) -> PathBuf {
    let path = explicit.map_or_else(
        || paths.state_dir.join("fleet-release").join("state.json"),
        Path::to_path_buf,
    );
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

fn fleet_lock_file(paths: &RuntimePaths) -> PathBuf {
    paths.state_dir.join("fleet-release").join("mutation.lock")
}

fn read_state(path: &Path) -> Result<RolloutState, CliFailure> {
    let raw = fs::read_to_string(path).map_err(|error| {
        CliFailure::new(
            2,
            format!("cannot read rollout state {}: {error}", path.display()),
        )
    })?;
    let state: RolloutState = serde_json::from_str(&raw).map_err(|error| {
        CliFailure::new(
            2,
            format!("cannot parse rollout state {}: {error}", path.display()),
        )
    })?;
    if state.schema_version != STATE_SCHEMA {
        return Err(CliFailure::new(
            2,
            format!(
                "unsupported fleet release state schema {}",
                state.schema_version
            ),
        ));
    }
    Ok(state)
}

fn write_state(path: &Path, state: &RolloutState) -> Result<(), CliFailure> {
    let parent = path
        .parent()
        .ok_or_else(|| CliFailure::new(2, "state path has no parent"))?;
    fs::create_dir_all(parent).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let bytes =
        serde_json::to_vec_pretty(state).map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::write(&temp, bytes).map_err(|error| CliFailure::new(1, error.to_string()))?;
    fs::rename(&temp, path).map_err(|error| CliFailure::new(1, error.to_string()))
}

struct StateLock {
    file: fs::File,
}

impl StateLock {
    fn acquire(lock_file: &Path) -> Result<Self, CliFailure> {
        let parent = lock_file
            .parent()
            .ok_or_else(|| CliFailure::new(2, "state path has no parent"))?;
        fs::create_dir_all(parent).map_err(|error| CliFailure::new(1, error.to_string()))?;
        let path = lock_file.to_path_buf();
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        file.try_lock_exclusive().map_err(|error| {
            CliFailure::new(
                3,
                format!(
                    "another fleet release reconciliation owns {}: {error}",
                    path.display()
                ),
            )
        })?;
        Ok(Self { file })
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn activate_rollback(state: &mut RolloutState) -> Result<(), CliFailure> {
    if state.direction == RolloutDirection::Rollback {
        return Ok(());
    }
    let rollback_identity = state.rollback.clone().ok_or_else(|| {
        CliFailure::new(
            2,
            "rollout has no immutable rollback identity; re-apply with --rollback-to and --rollback-sha256",
        )
    })?;
    let previous = std::mem::replace(&mut state.desired, rollback_identity);
    state.rollback = Some(previous);
    state.direction = RolloutDirection::Rollback;
    state.generation = generation(&state.desired, &timestamp());
    state.canary_host = None;
    state.canary_proven = false;
    state.hosts.clear();
    state.updated_at = timestamp();
    Ok(())
}

fn install_reconciler(
    paths: &RuntimePaths,
    state_file: &Path,
    hosts_file: &Path,
) -> Result<ReconcilerReceipt, CliFailure> {
    if !cfg!(target_os = "macos") {
        return Err(CliFailure::new(
            2,
            "durable fleet release reconciler currently requires macOS launchd",
        ));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| CliFailure::new(2, "HOME is not set"))?;
    let plist = home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{RECONCILER_LABEL}.plist"));
    let log_dir = paths.state_dir.join("fleet-release");
    fs::create_dir_all(plist.parent().expect("plist parent"))
        .and_then(|()| fs::create_dir_all(&log_dir))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let controller_dir = home.join(".local/libexec");
    fs::create_dir_all(&controller_dir).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let binary = controller_dir.join("shipyard-fleet-controller");
    let source_binary = std::env::current_exe()
        .map_err(|error| CliFailure::new(1, format!("cannot locate controller binary: {error}")))?;
    let staged_binary =
        controller_dir.join(format!(".shipyard-fleet-controller.{}", std::process::id()));
    fs::copy(&source_binary, &staged_binary)
        .and_then(|_| fs::rename(&staged_binary, &binary))
        .map_err(|error| CliFailure::new(1, format!("cannot install fleet controller: {error}")))?;
    let body = reconciler_plist(&binary, state_file, hosts_file, &log_dir);
    fs::write(&plist, body).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let domain = launchd_domain()?;
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{RECONCILER_LABEL}")])
        .status();
    let status = Command::new("launchctl")
        .args(["bootstrap", &domain])
        .arg(&plist)
        .status()
        .map_err(|error| CliFailure::new(1, format!("launchctl bootstrap failed: {error}")))?;
    if !status.success() {
        return Err(CliFailure::new(
            1,
            "launchctl bootstrap rejected fleet reconciler",
        ));
    }
    let receipt = inspect_reconciler(paths);
    if !receipt.installed || !receipt.loaded {
        return Err(CliFailure::new(
            1,
            format!("fleet reconciler did not load: {}", receipt.detail),
        ));
    }
    Ok(receipt)
}

fn inspect_reconciler(paths: &RuntimePaths) -> ReconcilerReceipt {
    if !cfg!(target_os = "macos") {
        return ReconcilerReceipt {
            installed: false,
            loaded: false,
            detail: "launchd reconciler unsupported on this platform".to_owned(),
        };
    }
    let installed = std::env::var_os("HOME").is_some_and(|home| {
        let home = PathBuf::from(home);
        home.join("Library/LaunchAgents")
            .join(format!("{RECONCILER_LABEL}.plist"))
            .is_file()
            && home
                .join(".local/libexec/shipyard-fleet-controller")
                .is_file()
    });
    let loaded = launchd_domain().is_ok_and(|domain| {
        Command::new("launchctl")
            .args(["print", &format!("{domain}/{RECONCILER_LABEL}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    });
    ReconcilerReceipt {
        installed,
        loaded,
        detail: format!(
            "plist={} state={}",
            installed,
            paths.state_dir.join("fleet-release/state.json").display()
        ),
    }
}

fn inspect_reconciler_from_prior(
    prior: &ReconcilerReceipt,
    state_file: &Path,
) -> ReconcilerReceipt {
    if !prior.installed {
        return prior.clone();
    }
    let controller_present = std::env::var_os("HOME").is_some_and(|home| {
        PathBuf::from(home)
            .join(".local/libexec/shipyard-fleet-controller")
            .is_file()
    });
    let loaded = if cfg!(target_os = "macos") && controller_present {
        launchd_domain().is_ok_and(|domain| {
            Command::new("launchctl")
                .args(["print", &format!("{domain}/{RECONCILER_LABEL}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    } else {
        false
    };
    ReconcilerReceipt {
        installed: controller_present,
        loaded,
        detail: format!("durable state={}", state_file.display()),
    }
}

fn reconciler_plist(binary: &Path, state: &Path, hosts: &Path, log_dir: &Path) -> String {
    let xml = |value: &Path| {
        value
            .display()
            .to_string()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{RECONCILER_LABEL}</string>
<key>ProgramArguments</key><array>
<string>{binary}</string><string>fleet</string><string>release</string><string>reconcile</string>
<string>--state-file</string><string>{state}</string>
<string>--hosts-file</string><string>{hosts}</string>
</array>
<key>StartInterval</key><integer>300</integer>
<key>ProcessType</key><string>Background</string>
<key>LowPriorityIO</key><true/><key>Nice</key><integer>10</integer>
<key>StandardOutPath</key><string>{stdout}</string>
<key>StandardErrorPath</key><string>{stderr}</string>
</dict></plist>
"#,
        binary = xml(binary),
        state = xml(state),
        hosts = xml(hosts),
        stdout = xml(&log_dir.join("reconcile.out.log")),
        stderr = xml(&log_dir.join("reconcile.err.log")),
    )
}

fn launchd_domain() -> Result<String, CliFailure> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|error| CliFailure::new(1, format!("cannot determine launchd uid: {error}")))?;
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || uid.is_empty() || !uid.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(CliFailure::new(1, "cannot determine numeric launchd uid"));
    }
    Ok(format!("gui/{uid}"))
}

fn report_from_state(
    command: &str,
    hosts: &[FleetHost],
    state: &RolloutState,
) -> FleetReleaseReport {
    let canary = state
        .canary_host
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    FleetReleaseReport {
        command: command.to_owned(),
        generation: state.generation.clone(),
        desired: state.desired.clone(),
        rollback: state.rollback.clone(),
        canary_host: state.canary_host.clone(),
        canary_proven: state.canary_proven,
        wave: next_wave(hosts, &state.hosts, &canary, state.canary_proven),
        complete: state.hosts.len() == hosts.len()
            && state
                .hosts
                .values()
                .all(|receipt| receipt.state == HostState::Converged),
        hosts: state.hosts.clone(),
        reconciler: state.reconciler.clone(),
    }
}

fn render_report<W: Write>(
    stdout: &mut W,
    json: bool,
    report: &FleetReleaseReport,
) -> Result<(), CliFailure> {
    if json {
        serde_json::to_writer_pretty(&mut *stdout, report)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    writeln!(
        stdout,
        "fleet release {} sha256={} canary={} proven={} complete={}",
        report.desired.version,
        report.desired.sha256,
        report.canary_host.as_deref().unwrap_or("none"),
        report.canary_proven,
        report.complete,
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    for (name, receipt) in &report.hosts {
        writeln!(
            stdout,
            "  {name:<20} {:?} version={} daemon={} participation={}",
            receipt.state,
            receipt.observed.version.as_deref().unwrap_or("unknown"),
            receipt.observed.daemon_version.as_deref().unwrap_or(
                if receipt.observed.daemon_running == Some(false) {
                    "stopped"
                } else {
                    "unknown"
                }
            ),
            receipt
                .observed
                .participation
                .map_or("unknown", |value| if value { "on" } else { "off" }),
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    if !report.wave.is_empty() {
        writeln!(stdout, "next wave: {}", report.wave.join(", "))
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn generation(identity: &ReleaseIdentity, now: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(identity.version.as_bytes());
    digest.update(identity.sha256.as_bytes());
    digest.update(now.as_bytes());
    format!("fr-{}", &format!("{:x}", digest.finalize())[..16])
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn file_sha256(path: &Path) -> Result<String, CliFailure> {
    let bytes = fs::read(path).map_err(|error| {
        CliFailure::new(
            2,
            format!("cannot read inventory {}: {error}", path.display()),
        )
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
#[path = "fleet_release_cmd/tests.rs"]
mod tests;
