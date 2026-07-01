use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use toml::{Table, Value as TomlValue};

use super::{
    CliFailure,
    cli::{TargetBackend, TargetsCommand, TargetsPoolCommand, TargetsWarmCommand},
};
use crate::config::LoadedConfig;
use crate::executor::dispatch::{
    ExecutorDispatcher, ResolvedBackend, ResolvedTarget, resolve_targets,
    resolve_targets_from_table,
};
use crate::host_pool::{
    HostPoolConfig, HostPoolLease, HostPoolLeaseStore, HostPoolMemberConfig, default_lease_path,
    parse_host_pools,
};
use crate::identity::RuntimeMode;
use crate::job::ValidationMode;
use crate::output::write_json_envelope;
use crate::warm_pool::{WarmPool, default_pool_path, now_epoch_secs};

pub(super) fn targets_command<W: Write>(
    command: Option<TargetsCommand>,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    match command.unwrap_or(TargetsCommand::List) {
        TargetsCommand::List => targets_list(&config, json_mode, stdout)?,
        TargetsCommand::Test { name } => return targets_test(&config, &name, json_mode, stdout),
        TargetsCommand::Add {
            name,
            backend,
            platform,
            host,
            repo_path,
        } => {
            let request = TargetAddRequest {
                name,
                backend,
                config: NewTargetConfig {
                    backend: backend.as_str().to_owned(),
                    platform,
                    host,
                    repo_path,
                },
            };
            targets_add(&config, &request, json_mode, stdout)?;
        }
        TargetsCommand::Remove { name } => targets_remove(&config, &name, json_mode, stdout)?,
        TargetsCommand::Warm { command } => {
            targets_warm(command, state_dir, json_mode, stdout)?;
        }
        TargetsCommand::Pool { command } => {
            targets_pool(command, &config, state_dir, json_mode, stdout)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn targets_list<W: Write>(
    config: &LoadedConfig,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if target_tables(config).is_none_or(Table::is_empty) {
        if json_mode {
            write_targets_list_json(stdout, Vec::new())?;
        } else {
            writeln!(stdout, "No targets configured. Run `shipyard init`.")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        return Ok(());
    }

    let targets = resolve_targets(config, ValidationMode::Full)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let rows = target_rows(&targets);
    if json_mode {
        write_targets_list_json(stdout, rows)?;
    } else {
        write_targets_list_human(stdout, &rows)?;
    }
    Ok(())
}

fn targets_test<W: Write>(
    config: &LoadedConfig,
    name: &str,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if !target_tables(config).is_some_and(|targets| targets.contains_key(name)) {
        return Err(CliFailure::new(
            1,
            format!("Target '{name}' not configured"),
        ));
    }
    let targets = resolve_targets(config, ValidationMode::Full)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let target = targets
        .iter()
        .find(|target| target.name == name)
        .ok_or_else(|| CliFailure::new(1, format!("Target '{name}' not configured")))?;
    let dispatcher = ExecutorDispatcher::new(None);
    let (reachable, active_backend) = probe_target(&dispatcher, target);
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("name".to_owned(), Value::String(name.to_owned()));
        data.insert("reachable".to_owned(), Value::Bool(reachable));
        data.insert(
            "active_backend".to_owned(),
            active_backend
                .as_ref()
                .map_or(Value::Null, |backend| Value::String(backend.clone())),
        );
        write_json_envelope(stdout, "targets.test", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ExitCode::SUCCESS);
    }

    if reachable {
        writeln!(
            stdout,
            "{name}: reachable via {}",
            active_backend.unwrap_or_else(|| target.backend_name.clone())
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        Ok(ExitCode::SUCCESS)
    } else {
        writeln!(stdout, "{name}: unreachable")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        Ok(ExitCode::FAILURE)
    }
}

fn targets_add<W: Write>(
    config: &LoadedConfig,
    request: &TargetAddRequest,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let name = request.name.as_str();
    let project_dir = config.project_dir.as_ref().ok_or_else(|| {
        CliFailure::new(
            1,
            "No .shipyard/config.toml found. Run `shipyard init` first.",
        )
    })?;
    if target_tables(config).is_some_and(|targets| targets.contains_key(name)) {
        return Err(CliFailure::new(
            1,
            format!("Target '{name}' already exists. Remove it first or pick another name."),
        ));
    }
    if matches!(
        request.backend,
        TargetBackend::Ssh | TargetBackend::SshWindows
    ) && request.config.host.is_none()
    {
        return Err(CliFailure::new(
            1,
            format!(
                "--host is required for backend={}",
                request.backend.as_str()
            ),
        ));
    }

    if matches!(
        request.backend,
        TargetBackend::Ssh | TargetBackend::SshWindows
    ) && !probe_new_target(name, &request.config)?
        && !json_mode
        && let Some(host) = request.config.host.as_deref()
    {
        writeln!(
            stdout,
            "warning: {host} is not reachable right now. Adding anyway."
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }

    let config_path = project_dir.join("config.toml");
    append_target_section(&config_path, name, &request.config)?;
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("name".to_owned(), Value::String(name.to_owned()));
        data.insert(
            "config".to_owned(),
            serde_json::to_value(&request.config)
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        write_json_envelope(stdout, "targets.add", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "Added target '{name}' to {}", config_path.display())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn targets_remove<W: Write>(
    config: &LoadedConfig,
    name: &str,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let project_dir = config
        .project_dir
        .as_ref()
        .ok_or_else(|| CliFailure::new(1, "No .shipyard/config.toml found."))?;
    if !target_tables(config).is_some_and(|targets| targets.contains_key(name)) {
        return Err(CliFailure::new(1, format!("Target '{name}' not found")));
    }
    let config_path = project_dir.join("config.toml");
    remove_target_section(&config_path, name)?;
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("name".to_owned(), Value::String(name.to_owned()));
        write_json_envelope(stdout, "targets.remove", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "Removed target '{name}' from {}",
            config_path.display()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn targets_warm<W: Write>(
    command: Option<TargetsWarmCommand>,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    match command.unwrap_or(TargetsWarmCommand::Status) {
        TargetsWarmCommand::Status => targets_warm_status(state_dir, json_mode, stdout),
        TargetsWarmCommand::Drain { yes } => targets_warm_drain(state_dir, yes, json_mode, stdout),
    }
}

fn targets_warm_status<W: Write>(
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let now = now_epoch_secs();
    let pool = WarmPool::new(default_pool_path(state_dir));
    pool.prune_expired(now)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let rows = pool
        .all_entries()
        .into_iter()
        .map(|entry| WarmEntryRow {
            target: entry.target,
            host: entry.host,
            backend: entry.backend,
            workdir: entry.workdir,
            sha: entry.sha,
            ttl_remaining_secs: round_one((entry.expires_at - now).max(0.0)),
            expires_at: isoformat_epoch(entry.expires_at),
            created_at: isoformat_epoch(entry.created_at),
        })
        .collect::<Vec<_>>();

    if json_mode {
        let mut data = BTreeMap::new();
        data.insert(
            "entries".to_owned(),
            serde_json::to_value(&rows).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        write_json_envelope(stdout, "targets.warm.status", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }

    if rows.is_empty() {
        writeln!(stdout, "Warm pool is empty.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }

    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "Warm pool").map_err(|error| CliFailure::new(1, error.to_string()))?;
    for row in rows {
        writeln!(
            stdout,
            "  {:<16} {:<20} sha={} ttl={:>6.0}s workdir={}",
            row.target,
            row.host,
            short_sha(&row.sha),
            row.ttl_remaining_secs,
            row.workdir
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn targets_warm_drain<W: Write>(
    state_dir: &Path,
    yes: bool,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let pool = WarmPool::new(default_pool_path(state_dir));
    let existing = pool.all_entries().len();
    if existing == 0 {
        if json_mode {
            write_warm_drain_json(stdout, 0)?;
        } else {
            writeln!(stdout, "Warm pool already empty.")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        return Ok(());
    }
    if !yes && !json_mode {
        writeln!(stdout, "Aborted.").map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    let drained = pool
        .drain()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if json_mode {
        write_warm_drain_json(stdout, drained)?;
    } else {
        writeln!(stdout, "Drained {drained} warm-pool entries.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn targets_pool<W: Write>(
    command: Option<TargetsPoolCommand>,
    config: &LoadedConfig,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    match command.unwrap_or(TargetsPoolCommand::Status) {
        TargetsPoolCommand::Status => targets_pool_status(config, state_dir, json_mode, stdout),
        TargetsPoolCommand::Cleanup { dry_run, fix } => {
            targets_pool_cleanup(state_dir, dry_run || !fix, json_mode, stdout)
        }
    }
}

fn targets_pool_status<W: Write>(
    config: &LoadedConfig,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let pools =
        parse_host_pools(&config.data).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let store = HostPoolLeaseStore::new(default_lease_path(state_dir));
    let leases = store
        .leases()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let rows = host_pool_rows(&pools, &leases, Utc::now());

    if json_mode {
        let mut data = BTreeMap::new();
        data.insert(
            "pools".to_owned(),
            serde_json::to_value(&rows).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        write_json_envelope(stdout, "targets.pool.status", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }

    write_host_pool_status_human(stdout, &rows)
}

fn targets_pool_cleanup<W: Write>(
    state_dir: &Path,
    dry_run: bool,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let now = Utc::now();
    let store = HostPoolLeaseStore::new(default_lease_path(state_dir));
    let leases = store
        .leases()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let stale_leases = leases.iter().filter(|lease| lease.is_stale(now)).count();
    let removed = if dry_run {
        0
    } else {
        store
            .prune_stale(now)
            .map_err(|error| CliFailure::new(1, error.to_string()))?
    };

    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("dry_run".to_owned(), json!(dry_run));
        data.insert("stale_leases".to_owned(), json!(stale_leases));
        data.insert("removed".to_owned(), json!(removed));
        write_json_envelope(stdout, "targets.pool.cleanup", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }

    if dry_run {
        writeln!(
            stdout,
            "Would remove {stale_leases} stale host-pool lease record(s)."
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "Removed {removed} stale host-pool lease record(s).")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct TargetRow {
    name: String,
    backend: String,
    platform: String,
    reachable: bool,
    active_backend: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct TargetAddRequest {
    name: String,
    backend: TargetBackend,
    config: NewTargetConfig,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct NewTargetConfig {
    backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_path: Option<String>,
}

#[derive(Debug, PartialEq, Serialize)]
struct WarmEntryRow {
    target: String,
    host: String,
    backend: String,
    workdir: String,
    sha: String,
    ttl_remaining_secs: f64,
    expires_at: String,
    created_at: String,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct HostPoolStatusRow {
    name: String,
    strategy: String,
    lease_stale_seconds: u64,
    heartbeat_interval_seconds: u64,
    members: Vec<HostPoolMemberStatusRow>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct HostPoolMemberStatusRow {
    id: String,
    #[serde(rename = "type")]
    backend_type: String,
    host: Option<String>,
    repo_path: Option<String>,
    cwd: Option<String>,
    max_concurrency: u32,
    available_slots: u32,
    capabilities: Vec<String>,
    state: String,
    active_leases: usize,
    stale_leases: usize,
    leases: Vec<HostPoolLeaseStatusRow>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct HostPoolLeaseStatusRow {
    lease_id: String,
    target: String,
    backend: String,
    host: Option<String>,
    job_id: Option<String>,
    branch: String,
    sha: String,
    short_sha: String,
    owner_pid: u32,
    acquired_at: DateTime<Utc>,
    heartbeat_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    stale: bool,
    age_seconds: i64,
    heartbeat_age_seconds: i64,
}

fn target_tables(config: &LoadedConfig) -> Option<&Table> {
    config.get("targets").and_then(TomlValue::as_table)
}

fn target_rows(targets: &[ResolvedTarget]) -> Vec<TargetRow> {
    let dispatcher = ExecutorDispatcher::new(None);
    targets
        .iter()
        .map(|target| {
            let (reachable, active_backend) = probe_target(&dispatcher, target);
            TargetRow {
                name: target.name.clone(),
                backend: target.backend_name.clone(),
                platform: target.platform.clone(),
                reachable,
                active_backend,
            }
        })
        .collect()
}

fn probe_target(
    dispatcher: &ExecutorDispatcher,
    target: &ResolvedTarget,
) -> (bool, Option<String>) {
    if let ResolvedBackend::Fallback(chain) = &target.backend {
        for backend in &chain.backends {
            if dispatcher.probe(&backend.target) {
                return (true, Some(backend.target.backend_name.clone()));
            }
        }
        return (false, None);
    }
    if dispatcher.probe(target) {
        (true, Some(target.backend_name.clone()))
    } else {
        (false, None)
    }
}

fn probe_new_target(name: &str, target: &NewTargetConfig) -> Result<bool, CliFailure> {
    let mut target_table = Table::new();
    target_table.insert(
        "backend".to_owned(),
        TomlValue::String(target.backend.clone()),
    );
    if let Some(platform) = &target.platform {
        target_table.insert("platform".to_owned(), TomlValue::String(platform.clone()));
    }
    if let Some(host) = &target.host {
        target_table.insert("host".to_owned(), TomlValue::String(host.clone()));
    }
    if let Some(repo_path) = &target.repo_path {
        target_table.insert("repo_path".to_owned(), TomlValue::String(repo_path.clone()));
    }
    let mut targets = Table::new();
    targets.insert(name.to_owned(), TomlValue::Table(target_table));
    let mut root = Table::new();
    root.insert("targets".to_owned(), TomlValue::Table(targets));
    let resolved = resolve_targets_from_table(&root, ValidationMode::Full)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(resolved
        .first()
        .is_some_and(|target| ExecutorDispatcher::new(None).probe(target)))
}

fn write_targets_list_json<W: Write>(
    stdout: &mut W,
    rows: Vec<TargetRow>,
) -> Result<(), CliFailure> {
    let mut data = BTreeMap::new();
    data.insert(
        "targets".to_owned(),
        serde_json::to_value(rows).map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    write_json_envelope(stdout, "targets.list", data)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn write_targets_list_human<W: Write>(
    stdout: &mut W,
    rows: &[TargetRow],
) -> Result<(), CliFailure> {
    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "Targets").map_err(|error| CliFailure::new(1, error.to_string()))?;
    for row in rows {
        let status = if row.reachable {
            "reachable"
        } else {
            "unreachable"
        };
        writeln!(
            stdout,
            "  {:<16} {:<12} {:<16} {status}",
            row.name, row.backend, row.platform
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn host_pool_rows(
    pools: &[HostPoolConfig],
    leases: &[HostPoolLease],
    now: DateTime<Utc>,
) -> Vec<HostPoolStatusRow> {
    pools
        .iter()
        .map(|pool| HostPoolStatusRow {
            name: pool.name.clone(),
            strategy: pool.strategy.clone(),
            lease_stale_seconds: pool.lease_stale_seconds,
            heartbeat_interval_seconds: pool.heartbeat_interval_seconds,
            members: pool
                .members
                .iter()
                .map(|member| host_pool_member_row(pool, member, leases, now))
                .collect(),
        })
        .collect()
}

fn host_pool_member_row(
    pool: &HostPoolConfig,
    member: &HostPoolMemberConfig,
    leases: &[HostPoolLease],
    now: DateTime<Utc>,
) -> HostPoolMemberStatusRow {
    let member_leases = leases
        .iter()
        .filter(|lease| lease.pool_name == pool.name && lease.member_id == member.id)
        .collect::<Vec<_>>();
    let active_leases = member_leases
        .iter()
        .filter(|lease| !lease.is_stale(now))
        .count();
    let stale_leases = member_leases.len() - active_leases;
    let available_slots = member
        .max_concurrency
        .saturating_sub(u32::try_from(active_leases).unwrap_or(u32::MAX));
    HostPoolMemberStatusRow {
        id: member.id.clone(),
        backend_type: member.backend_type.clone(),
        host: member.host.clone(),
        repo_path: member.repo_path.clone(),
        cwd: member.cwd.as_ref().map(|path| path.display().to_string()),
        max_concurrency: member.max_concurrency,
        available_slots,
        capabilities: member.capabilities.clone(),
        state: if active_leases > 0 { "busy" } else { "idle" }.to_owned(),
        active_leases,
        stale_leases,
        leases: member_leases
            .into_iter()
            .map(|lease| host_pool_lease_row(lease, now))
            .collect(),
    }
}

fn host_pool_lease_row(lease: &HostPoolLease, now: DateTime<Utc>) -> HostPoolLeaseStatusRow {
    HostPoolLeaseStatusRow {
        lease_id: lease.lease_id.clone(),
        target: lease.target_name.clone(),
        backend: lease.backend.clone(),
        host: lease.host.clone(),
        job_id: lease.job_id.clone(),
        branch: lease.branch.clone(),
        sha: lease.sha.clone(),
        short_sha: short_sha(&lease.sha).to_owned(),
        owner_pid: lease.owner_pid,
        acquired_at: lease.acquired_at,
        heartbeat_at: lease.heartbeat_at,
        expires_at: lease.expires_at,
        stale: lease.is_stale(now),
        age_seconds: seconds_since(now, lease.acquired_at),
        heartbeat_age_seconds: seconds_since(now, lease.heartbeat_at),
    }
}

fn write_host_pool_status_human<W: Write>(
    stdout: &mut W,
    rows: &[HostPoolStatusRow],
) -> Result<(), CliFailure> {
    if rows.is_empty() {
        writeln!(stdout, "No host pools configured.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }

    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "Host pools").map_err(|error| CliFailure::new(1, error.to_string()))?;
    for pool in rows {
        writeln!(
            stdout,
            "  {} strategy={} stale={}s heartbeat={}s",
            pool.name, pool.strategy, pool.lease_stale_seconds, pool.heartbeat_interval_seconds
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for member in &pool.members {
            writeln!(
                stdout,
                "    {:<16} {:<5} {:<4} active={} stale={} slots={}/{} {} caps={}",
                member.id,
                member.backend_type,
                member.state,
                member.active_leases,
                member.stale_leases,
                member.available_slots,
                member.max_concurrency,
                host_pool_member_location(member),
                display_pool_capabilities(&member.capabilities)
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
            for lease in &member.leases {
                writeln!(
                    stdout,
                    "      lease={} target={} sha={} branch={} age={}s heartbeat={}s{}{}",
                    lease.lease_id,
                    lease.target,
                    lease.short_sha,
                    lease.branch,
                    lease.age_seconds,
                    lease.heartbeat_age_seconds,
                    lease
                        .job_id
                        .as_ref()
                        .map(|job| format!(" job={job}"))
                        .unwrap_or_default(),
                    if lease.stale { " stale" } else { "" }
                )
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            }
        }
    }
    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn host_pool_member_location(member: &HostPoolMemberStatusRow) -> String {
    if let Some(host) = &member.host {
        return format!("host={host}");
    }
    if let Some(cwd) = &member.cwd {
        return format!("cwd={cwd}");
    }
    "-".to_owned()
}

fn display_pool_capabilities(capabilities: &[String]) -> String {
    if capabilities.is_empty() {
        "-".to_owned()
    } else {
        capabilities.join(",")
    }
}

fn append_target_section(
    config_path: &Path,
    name: &str,
    target: &NewTargetConfig,
) -> Result<(), CliFailure> {
    let mut text =
        fs::read_to_string(config_path).map_err(|error| CliFailure::new(1, error.to_string()))?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.ends_with("\n\n") {
        text.push('\n');
    }
    text.push_str(&render_target_section(name, target));
    fs::write(config_path, text).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn render_target_section(name: &str, target: &NewTargetConfig) -> String {
    let mut section = format!(
        "[targets.{name}]\nbackend = {}\n",
        toml_quote(&target.backend)
    );
    if let Some(platform) = &target.platform {
        let _ = writeln!(section, "platform = {}", toml_quote(platform));
    }
    if let Some(host) = &target.host {
        let _ = writeln!(section, "host = {}", toml_quote(host));
    }
    if let Some(repo_path) = &target.repo_path {
        let _ = writeln!(section, "repo_path = {}", toml_quote(repo_path));
    }
    section
}

fn remove_target_section(config_path: &Path, name: &str) -> Result<(), CliFailure> {
    let text =
        fs::read_to_string(config_path).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut output = String::new();
    let mut skipping = false;
    let section_marker = format!("[targets.{name}]");
    for line in text.split_inclusive('\n') {
        let stripped = line.trim();
        if stripped == section_marker {
            skipping = true;
            continue;
        }
        if skipping && stripped.starts_with('[') && stripped.ends_with(']') {
            skipping = false;
            output.push_str(line);
            continue;
        }
        if !skipping {
            output.push_str(line);
        }
    }
    fs::write(config_path, output).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn write_warm_drain_json<W: Write>(stdout: &mut W, drained: usize) -> Result<(), CliFailure> {
    let mut data = BTreeMap::new();
    data.insert("drained".to_owned(), json!(drained));
    write_json_envelope(stdout, "targets.warm.drain", data)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn toml_quote(value: &str) -> String {
    serde_json::to_string(value).expect("strings serialize")
}

fn isoformat_epoch(epoch: f64) -> String {
    let system_time = UNIX_EPOCH + Duration::from_secs_f64(epoch.max(0.0));
    DateTime::<Utc>::from(system_time).to_rfc3339()
}

fn round_one(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

fn seconds_since(now: DateTime<Utc>, then: DateTime<Utc>) -> i64 {
    now.signed_duration_since(then).num_seconds().max(0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Duration as ChronoDuration;
    use serde_json::Value;
    use tempfile::TempDir;
    use toml::Table;

    use super::{LoadedConfig, targets_pool_cleanup, targets_pool_status};
    use crate::config::LocalOverlaySource;
    use crate::host_pool::{HostPoolLeaseRequest, HostPoolLeaseStore, default_lease_path};

    fn table(input: &str) -> Table {
        input.parse::<Table>().expect("toml")
    }

    fn loaded_config(data: Table) -> LoadedConfig {
        LoadedConfig {
            data,
            global_dir: PathBuf::from("/tmp/global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    #[test]
    fn pool_status_reports_no_configured_pools() {
        let temp = TempDir::new().expect("tempdir");
        let config = loaded_config(Table::new());
        let mut output = Vec::new();

        targets_pool_status(&config, temp.path(), false, &mut output).expect("status");

        assert_eq!(
            String::from_utf8(output).expect("utf8"),
            "No host pools configured.\n"
        );
    }

    #[test]
    fn pool_status_json_reports_active_and_stale_leases() {
        let temp = TempDir::new().expect("tempdir");
        let config = loaded_config(table(
            r#"
            [host_pools.local_macs]

            [[host_pools.local_macs.members]]
            id = "mac-studio"
            type = "ssh"
            host = "mac-studio"
            max_concurrency = 1
            capabilities = ["macos", "arm64"]

            [[host_pools.local_macs.members]]
            id = "local"
            type = "local"
            cwd = "/repo"
            max_concurrency = 1
            capabilities = ["macos", "arm64"]
            "#,
        ));
        let store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        let active = store
            .acquire(&lease_request("mac-studio", 180))
            .expect("acquire")
            .expect("active lease");
        let stale = store
            .acquire(&lease_request("local", 1))
            .expect("acquire stale candidate")
            .expect("stale lease");
        let mut leases = store.leases().expect("leases");
        for lease in &mut leases {
            if lease.lease_id == stale.lease_id {
                lease.expires_at = lease.acquired_at - ChronoDuration::seconds(1);
            }
        }
        std::fs::write(
            default_lease_path(temp.path()),
            serde_json::to_string_pretty(&serde_json::json!({ "leases": leases })).expect("json"),
        )
        .expect("write leases");
        let mut output = Vec::new();

        targets_pool_status(&config, temp.path(), true, &mut output).expect("status");

        let value: Value = serde_json::from_slice(&output).expect("json");
        assert_eq!(value["command"], "targets.pool.status");
        let members = value["pools"][0]["members"].as_array().expect("members");
        assert_eq!(members[0]["id"], "mac-studio");
        assert_eq!(members[0]["state"], "busy");
        assert_eq!(members[0]["active_leases"], 1);
        assert_eq!(members[1]["id"], "local");
        assert_eq!(members[1]["state"], "idle");
        assert_eq!(members[1]["stale_leases"], 1);
        assert_eq!(members[0]["leases"][0]["lease_id"], active.lease_id);
    }

    #[test]
    fn pool_cleanup_dry_run_then_fix_prunes_stale_leases() {
        let temp = TempDir::new().expect("tempdir");
        let store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        let stale = store
            .acquire(&lease_request("local", 1))
            .expect("acquire stale candidate")
            .expect("stale lease");
        let mut leases = store.leases().expect("leases");
        for lease in &mut leases {
            if lease.lease_id == stale.lease_id {
                lease.expires_at = lease.acquired_at - ChronoDuration::seconds(1);
            }
        }
        std::fs::write(
            default_lease_path(temp.path()),
            serde_json::to_string_pretty(&serde_json::json!({ "leases": leases })).expect("json"),
        )
        .expect("write leases");
        let mut dry_run_output = Vec::new();

        targets_pool_cleanup(temp.path(), true, true, &mut dry_run_output).expect("dry run");

        let dry_run: Value = serde_json::from_slice(&dry_run_output).expect("json");
        assert_eq!(dry_run["command"], "targets.pool.cleanup");
        assert_eq!(dry_run["dry_run"], true);
        assert_eq!(dry_run["stale_leases"], 1);
        assert_eq!(dry_run["removed"], 0);
        assert_eq!(store.leases().expect("leases").len(), 1);

        let mut fix_output = Vec::new();
        targets_pool_cleanup(temp.path(), false, true, &mut fix_output).expect("fix");

        let fix: Value = serde_json::from_slice(&fix_output).expect("json");
        assert_eq!(fix["dry_run"], false);
        assert_eq!(fix["stale_leases"], 1);
        assert_eq!(fix["removed"], 1);
        assert!(store.leases().expect("leases").is_empty());
    }

    fn lease_request(member_id: &str, lease_stale_seconds: u64) -> HostPoolLeaseRequest {
        HostPoolLeaseRequest {
            pool_name: "local_macs".to_owned(),
            member_id: member_id.to_owned(),
            target_name: "mac".to_owned(),
            backend: "ssh".to_owned(),
            host: Some(member_id.to_owned()),
            job_id: Some("job-1".to_owned()),
            branch: "main".to_owned(),
            sha: "abcdef123456".to_owned(),
            max_concurrency: 1,
            lease_stale_seconds,
        }
    }
}
