//! Backend-aware executor dispatch.
//!
//! This module is the Rust equivalent of Python Shipyard's
//! `ExecutorDispatcher`: it resolves merged TOML config into typed
//! executor requests and keeps backend selection out of CLI code.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration as StdDuration;

use chrono::Utc;
use toml::{Table, Value};

use crate::config::LoadedConfig;
use crate::executor::cloud::{CloudExecutor, CloudTargetConfig, CloudValidationRequest};
use crate::executor::contract::ContractConfig;
use crate::executor::local::{
    LocalExecutor, LocalTargetConfig, LocalValidationConfig, LocalValidationRequest,
};
use crate::executor::ssh::{
    self, ProbeOutcome, SshExecutor, SshTargetConfig, SshValidation, SshValidationRequest,
    format_ssh_diagnosis,
};
use crate::executor::ssh_windows::{
    self, WindowsExecutor, WindowsTargetConfig, WindowsValidation, WindowsValidationRequest,
};
use crate::executor::streaming::ProgressEvent;
use crate::host_pool::{
    HostPoolLeaseRequest, HostPoolLeaseStore, HostPoolMemberConfig, default_lease_path,
    parse_host_pools,
};
use crate::job::{TargetResult, ValidationMode};
use crate::prepared_state::PreparedStateStore;
use crate::warm_pool::extract_warm_keepalive_seconds;

const STAGE_ORDER: [&str; 4] = ["setup", "configure", "build", "test"];

/// A target resolved from merged Shipyard configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedTarget {
    /// Logical target name.
    pub name: String,
    /// Platform label.
    pub platform: String,
    /// Normalized backend label.
    pub backend_name: String,
    /// Configured warm-pool keepalive in seconds.
    pub warm_keepalive_seconds: u32,
    /// Host key input, when this target has a remote host.
    pub host: Option<String>,
    /// Backend-specific target settings.
    pub backend: ResolvedBackend,
    /// Backend-specific validation settings.
    pub validation: ResolvedValidation,
    /// Issue #303: optional failure parser selection from
    /// `[targets.<name>] failure_parser = "ctest|catch2|pytest|go|auto"`.
    /// Defaults to `auto` when absent.
    pub failure_parser: Option<String>,
}

impl ResolvedTarget {
    /// Return the workdir/repo path that the executor will enter.
    #[must_use]
    pub fn workdir(&self) -> Option<String> {
        match &self.backend {
            ResolvedBackend::Local(target) => target
                .cwd
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            ResolvedBackend::Ssh(target) => Some(target.repo_path.clone()),
            ResolvedBackend::Windows(target) => Some(target.repo_path.clone()),
            ResolvedBackend::Cloud(_) => None,
            ResolvedBackend::HostPool(pool) => pool
                .members
                .first()
                .and_then(|member| member.target.workdir()),
            ResolvedBackend::Fallback(chain) => chain
                .backends
                .first()
                .and_then(|backend| backend.target.workdir()),
        }
    }

    /// Return a copy with the backend workdir overridden.
    ///
    /// Warm-pool orchestration uses this after a same-SHA hit.
    #[must_use]
    pub fn with_workdir(mut self, workdir: impl Into<String>) -> Self {
        let workdir = workdir.into();
        match &mut self.backend {
            ResolvedBackend::Local(target) => {
                target.cwd = Some(PathBuf::from(workdir));
            }
            ResolvedBackend::Ssh(target) => {
                target.repo_path = workdir;
            }
            ResolvedBackend::Windows(target) => {
                target.repo_path = workdir;
            }
            ResolvedBackend::Cloud(_) => {}
            ResolvedBackend::HostPool(pool) => {
                if let Some(primary) = pool.members.first_mut() {
                    let updated = primary.target.as_ref().clone().with_workdir(workdir);
                    *primary.target = updated;
                }
            }
            ResolvedBackend::Fallback(chain) => {
                if let Some(primary) = chain.backends.first_mut() {
                    let updated = primary.target.as_ref().clone().with_workdir(workdir);
                    *primary.target = updated;
                }
            }
        }
        self
    }
}

/// Backend-specific target settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedBackend {
    /// Local process execution.
    Local(LocalTargetConfig),
    /// POSIX SSH execution.
    Ssh(SshTargetConfig),
    /// Windows SSH execution.
    Windows(WindowsTargetConfig),
    /// GitHub Actions cloud execution.
    Cloud(CloudTargetConfig),
    /// Ordered local host-pool execution.
    HostPool(ResolvedHostPoolConfig),
    /// Ordered fallback chain.
    Fallback(FallbackTargetConfig),
}

/// Backend-specific validation settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedValidation {
    /// Local validation settings.
    Local(LocalValidationConfig),
    /// POSIX SSH validation settings and optional contract.
    Ssh {
        /// Command/stage shape.
        validation: SshValidation,
        /// Optional validation contract.
        contract: Option<ContractConfig>,
    },
    /// Windows SSH validation settings and optional contract.
    Windows {
        /// Command/stage shape.
        validation: WindowsValidation,
        /// Optional validation contract.
        contract: Option<ContractConfig>,
    },
    /// Cloud validation settings.
    Cloud,
    /// Host-pool validation settings. Concrete member targets own the real
    /// local or SSH validation config.
    HostPool,
    /// Fallback validation settings.
    Fallback,
}

/// Ordered fallback chain resolved from one logical target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FallbackTargetConfig {
    /// Backends in attempt order.
    pub backends: Vec<FallbackBackend>,
    /// Required capabilities for locality routing.
    pub requires: Vec<String>,
    /// Heartbeat age that demotes non-passing stale results.
    pub heartbeat_stale_secs: u64,
}

/// One backend entry in a fallback chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FallbackBackend {
    /// Fully resolved backend target.
    pub target: Box<ResolvedTarget>,
    /// User-facing backend label.
    pub label: String,
    /// User-facing profile label for capability mismatch errors.
    pub profile_label: String,
    /// Inline backend capabilities.
    pub capabilities: Vec<String>,
}

/// One resolved host-pool target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedHostPoolConfig {
    /// Pool name from `[host_pools.<name>]`.
    pub pool_name: String,
    /// Member selection strategy.
    pub strategy: String,
    /// Seconds after which an unrefreshed lease is stale.
    pub lease_stale_seconds: u64,
    /// Seconds between lease heartbeat refreshes.
    pub heartbeat_interval_seconds: u64,
    /// Capabilities required by the target.
    pub requires: Vec<String>,
    /// Resolved concrete member targets in selection order.
    pub members: Vec<ResolvedHostPoolMember>,
}

/// One resolved member in a host pool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedHostPoolMember {
    /// Stable member id.
    pub id: String,
    /// Concrete target used by the existing local or SSH executor.
    pub target: Box<ResolvedTarget>,
    /// User-facing member label.
    pub label: String,
    /// User-facing profile label for capability mismatch errors.
    pub profile_label: String,
    /// Max concurrent leases for this member.
    pub max_concurrency: u32,
    /// Member capabilities.
    pub capabilities: Vec<String>,
}

/// Errors while resolving or dispatching executor config.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchError {
    /// The config has no `[targets]` table.
    MissingTargets,
    /// A target table has the wrong shape.
    InvalidTargetConfig {
        /// Target name.
        target: String,
        /// Human-readable reason.
        reason: String,
    },
    /// A validation table has the wrong shape.
    InvalidValidationConfig {
        /// Target name.
        target: String,
        /// Human-readable reason.
        reason: String,
    },
    /// The target uses an unknown backend.
    UnsupportedBackend {
        /// Target name.
        target: String,
        /// Normalized backend name.
        backend: String,
    },
}

impl Display for DispatchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTargets => write!(formatter, "missing [targets] table"),
            Self::InvalidTargetConfig { target, reason } => {
                write!(formatter, "invalid target {target:?}: {reason}")
            }
            Self::InvalidValidationConfig { target, reason } => {
                write!(
                    formatter,
                    "invalid validation for target {target:?}: {reason}"
                )
            }
            Self::UnsupportedBackend { target, backend } => {
                write!(
                    formatter,
                    "target {target:?} uses unsupported backend {backend:?}"
                )
            }
        }
    }
}

impl Error for DispatchError {}

/// Input to one backend dispatch.
pub struct DispatchValidationRequest<'target, 'callback> {
    /// Queue job id when the target is run from a queued job.
    pub job_id: Option<String>,
    /// Return scheduler-owned deferral results for transient host-pool lease
    /// contention instead of final busy target results.
    pub defer_host_pool_lease_unavailable: bool,
    /// Commit SHA under validation.
    pub sha: String,
    /// Branch under validation.
    pub branch: String,
    /// Resolved target.
    pub target: &'target ResolvedTarget,
    /// Local log file path.
    pub log_path: PathBuf,
    /// Optional stage to resume from.
    pub resume_from: Option<String>,
    /// Validation mode.
    pub mode: ValidationMode,
    /// Optional progress callback.
    pub progress_callback: Option<&'callback mut dyn FnMut(ProgressEvent)>,
}

/// Backend reachability diagnosis used by submission preflight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReachabilityDiagnostic {
    /// Whether the backend accepted a lightweight probe.
    pub reachable: bool,
    /// Human-readable diagnosis when unreachable.
    pub message: Option<String>,
    /// Stable backend failure category.
    pub category: Option<String>,
}

impl ReachabilityDiagnostic {
    fn reachable() -> Self {
        Self {
            reachable: true,
            message: None,
            category: None,
        }
    }

    fn from_probe(outcome: &ProbeOutcome) -> Self {
        Self {
            reachable: outcome.reachable,
            message: (!outcome.reachable).then(|| format_ssh_diagnosis(&outcome.diagnostic)),
            category: outcome
                .diagnostic
                .category
                .map(|category| category.as_str().to_owned()),
        }
    }
}

/// Concrete executor dispatcher.
#[derive(Clone, Debug)]
pub struct ExecutorDispatcher {
    local: LocalExecutor,
    ssh: SshExecutor,
    windows: WindowsExecutor,
    cloud: CloudExecutor,
    host_pool_store: Option<HostPoolLeaseStore>,
}

impl ExecutorDispatcher {
    /// Construct a dispatcher with production executors.
    #[must_use]
    pub fn new(prepared_state_store: Option<PreparedStateStore>) -> Self {
        Self::new_with_host_pool_store(prepared_state_store, None)
    }

    /// Construct a dispatcher that can lease host-pool members from `state_dir`.
    #[must_use]
    pub fn new_with_state_dir(
        prepared_state_store: Option<PreparedStateStore>,
        state_dir: &std::path::Path,
    ) -> Self {
        Self::new_with_host_pool_store(
            prepared_state_store,
            Some(HostPoolLeaseStore::new(default_lease_path(state_dir))),
        )
    }

    /// Construct a dispatcher with an explicit host-pool store.
    #[must_use]
    pub fn new_with_host_pool_store(
        prepared_state_store: Option<PreparedStateStore>,
        host_pool_store: Option<HostPoolLeaseStore>,
    ) -> Self {
        Self {
            local: LocalExecutor::new(prepared_state_store),
            ssh: SshExecutor::new(),
            windows: WindowsExecutor::new(),
            cloud: CloudExecutor::new(
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            ),
            host_pool_store,
        }
    }

    /// Dispatch one resolved target to its backend executor.
    #[must_use]
    pub fn validate(&self, mut request: DispatchValidationRequest<'_, '_>) -> TargetResult {
        match (&request.target.backend, &request.target.validation) {
            (ResolvedBackend::Local(target), ResolvedValidation::Local(validation)) => {
                let mut local_request =
                    LocalValidationRequest::new(request.log_path, validation.clone());
                local_request.sha = request.sha;
                local_request.branch = request.branch;
                local_request.target = target.clone();
                local_request.resume_from = request.resume_from;
                local_request.mode = mode_label(request.mode);
                local_request.progress_callback = request.progress_callback.take();
                self.local.validate(local_request)
            }
            (
                ResolvedBackend::Ssh(target),
                ResolvedValidation::Ssh {
                    validation,
                    contract,
                },
            ) => {
                let mut ssh_request =
                    SshValidationRequest::new(request.log_path, validation.clone());
                ssh_request.sha = request.sha;
                ssh_request.branch = request.branch;
                ssh_request.target = target.clone();
                ssh_request.contract.clone_from(contract);
                ssh_request.resume_from = request.resume_from;
                ssh_request.mode = mode_label(request.mode);
                ssh_request.progress_callback = request.progress_callback.take();
                self.ssh.validate(ssh_request)
            }
            (
                ResolvedBackend::Windows(target),
                ResolvedValidation::Windows {
                    validation,
                    contract,
                },
            ) => {
                let mut windows_request =
                    WindowsValidationRequest::new(request.log_path, validation.clone());
                windows_request.sha = request.sha;
                windows_request.branch = request.branch;
                windows_request.target = target.clone();
                windows_request.contract.clone_from(contract);
                windows_request.resume_from = request.resume_from;
                windows_request.mode = mode_label(request.mode);
                windows_request.progress_callback = request.progress_callback.take();
                self.windows.validate(windows_request)
            }
            (ResolvedBackend::Cloud(target), ResolvedValidation::Cloud) => {
                let mut cloud_request =
                    CloudValidationRequest::new(request.log_path, target.clone());
                cloud_request.sha = request.sha;
                cloud_request.branch = request.branch;
                cloud_request.progress_callback = request.progress_callback.take();
                self.cloud.validate(cloud_request)
            }
            (ResolvedBackend::HostPool(pool), ResolvedValidation::HostPool) => {
                self.validate_host_pool(&mut request, pool)
            }
            (ResolvedBackend::Fallback(chain), ResolvedValidation::Fallback) => {
                self.validate_fallback(&mut request, chain)
            }
            _ => unreachable!("ResolvedTarget pairs backend and validation by construction"),
        }
    }

    /// Return true when a target is reachable enough to submit work.
    #[must_use]
    pub fn probe(&self, target: &ResolvedTarget) -> bool {
        self.diagnose(target).reachable
    }

    /// Diagnose target reachability without running validation.
    #[must_use]
    pub fn diagnose(&self, target: &ResolvedTarget) -> ReachabilityDiagnostic {
        match &target.backend {
            ResolvedBackend::Local(_) => ReachabilityDiagnostic::reachable(),
            ResolvedBackend::Ssh(target) => {
                ReachabilityDiagnostic::from_probe(&ssh::diagnose_target(target))
            }
            ResolvedBackend::Windows(target) => {
                ReachabilityDiagnostic::from_probe(&ssh_windows::diagnose_target(target))
            }
            ResolvedBackend::Cloud(_) => {
                if self.cloud.probe() {
                    ReachabilityDiagnostic::reachable()
                } else {
                    ReachabilityDiagnostic {
                        reachable: false,
                        message: Some("gh auth status failed".to_owned()),
                        category: Some("gh_auth".to_owned()),
                    }
                }
            }
            ResolvedBackend::HostPool(pool) => self.diagnose_host_pool(pool),
            ResolvedBackend::Fallback(chain) => chain
                .backends
                .first()
                .map_or_else(ReachabilityDiagnostic::reachable, |backend| {
                    self.diagnose(&backend.target)
                }),
        }
    }

    fn validate_fallback(
        &self,
        request: &mut DispatchValidationRequest<'_, '_>,
        chain: &FallbackTargetConfig,
    ) -> TargetResult {
        let filtered = filter_backends_by_requires(chain);
        let Some(primary) = filtered.first() else {
            return capability_mismatch_result(request.target, chain);
        };
        let primary_label = primary.label.clone();
        let mut last_result = None;

        for (attempt, backend) in filtered.into_iter().enumerate() {
            let attempt_log = attempt_log_path(&request.log_path, attempt);
            if !self.probe(&backend.target) {
                last_result = Some(probe_failed_result(
                    &backend.target,
                    &backend.label,
                    &attempt_log,
                ));
                continue;
            }

            let result = self.validate(DispatchValidationRequest {
                job_id: request.job_id.clone(),
                defer_host_pool_lease_unavailable: request.defer_host_pool_lease_unavailable,
                sha: request.sha.clone(),
                branch: request.branch.clone(),
                target: &backend.target,
                log_path: attempt_log,
                resume_from: request.resume_from.clone(),
                mode: request.mode,
                progress_callback: request
                    .progress_callback
                    .as_mut()
                    .map(|callback| &mut **callback as &mut dyn FnMut(ProgressEvent)),
            });
            let result = demote_stale_result(result, chain.heartbeat_stale_secs);

            if result.status == crate::job::TargetStatus::Fail {
                return result;
            }
            if result.status == crate::job::TargetStatus::Pass {
                if attempt == 0 {
                    return result;
                }
                return failover_pass_result(
                    result,
                    &backend.label,
                    &primary_label,
                    last_result.as_ref(),
                );
            }
            last_result = Some(result);
        }

        exhausted_result(request.target, &primary_label, last_result)
    }

    #[allow(clippy::too_many_lines)]
    fn validate_host_pool(
        &self,
        request: &mut DispatchValidationRequest<'_, '_>,
        pool: &ResolvedHostPoolConfig,
    ) -> TargetResult {
        let filtered = filter_host_pool_members_by_requires(pool);
        let Some(primary) = filtered.first() else {
            return host_pool_capability_mismatch_result(request.target, pool);
        };
        let Some(store) = self.host_pool_store.clone() else {
            return host_pool_error_result(
                request.target,
                &primary.label,
                &request.log_path,
                "host-pool lease store is unavailable",
            );
        };
        let primary_label = primary.label.clone();
        let mut last_result = None;

        for (attempt, member) in filtered.into_iter().enumerate() {
            let attempt_log = attempt_log_path(&request.log_path, attempt);
            if !self.probe(&member.target) {
                last_result = Some(probe_failed_result(
                    &member.target,
                    &member.label,
                    &attempt_log,
                ));
                continue;
            }

            let lease_request = HostPoolLeaseRequest {
                pool_name: pool.pool_name.clone(),
                member_id: member.id.clone(),
                target_name: request.target.name.clone(),
                backend: member.target.backend_name.clone(),
                host: member.target.host.clone(),
                job_id: request.job_id.clone(),
                branch: request.branch.clone(),
                sha: request.sha.clone(),
                max_concurrency: member.max_concurrency,
                lease_stale_seconds: pool.lease_stale_seconds.max(1),
            };
            let lease = match store.acquire(&lease_request) {
                Ok(Some(lease)) => lease,
                Ok(None) => {
                    last_result = Some(if request.defer_host_pool_lease_unavailable {
                        host_pool_lease_unavailable_result(
                            &member.target,
                            &member.label,
                            &attempt_log,
                        )
                    } else {
                        host_pool_busy_result(&member.target, &member.label, &attempt_log)
                    });
                    continue;
                }
                Err(error) => {
                    last_result = Some(host_pool_error_result(
                        &member.target,
                        &member.label,
                        &attempt_log,
                        &error.to_string(),
                    ));
                    continue;
                }
            };
            let _lease_guard = HostPoolLeaseGuard::new(
                store.clone(),
                lease.lease_id,
                pool.heartbeat_interval_seconds.max(1),
                pool.lease_stale_seconds.max(1),
            );

            let mut result = self.validate(DispatchValidationRequest {
                job_id: request.job_id.clone(),
                defer_host_pool_lease_unavailable: request.defer_host_pool_lease_unavailable,
                sha: request.sha.clone(),
                branch: request.branch.clone(),
                target: &member.target,
                log_path: attempt_log,
                resume_from: request.resume_from.clone(),
                mode: request.mode,
                progress_callback: request
                    .progress_callback
                    .as_mut()
                    .map(|callback| &mut **callback as &mut dyn FnMut(ProgressEvent)),
            });

            if result.status == crate::job::TargetStatus::Fail {
                result.backend.clone_from(&member.label);
                return result;
            }
            if result.status == crate::job::TargetStatus::Pass {
                if attempt == 0 {
                    result.backend.clone_from(&member.label);
                    return result;
                }
                return failover_pass_result(
                    result,
                    &member.label,
                    &primary_label,
                    last_result.as_ref(),
                );
            }
            result.backend.clone_from(&member.label);
            last_result = Some(result);
        }

        if last_result
            .as_ref()
            .is_some_and(crate::job::TargetResult::is_scheduler_deferred)
        {
            return last_result.expect("checked present");
        }

        exhausted_result(request.target, &primary_label, last_result)
    }

    fn diagnose_host_pool(&self, pool: &ResolvedHostPoolConfig) -> ReachabilityDiagnostic {
        let filtered = filter_host_pool_members_by_requires(pool);
        if filtered.is_empty() {
            return ReachabilityDiagnostic {
                reachable: false,
                message: Some("no host-pool member satisfies target requirements".to_owned()),
                category: Some("configuration".to_owned()),
            };
        }
        let mut last = None;
        for member in filtered {
            let diagnostic = self.diagnose(&member.target);
            if diagnostic.reachable {
                return diagnostic;
            }
            last = Some(diagnostic);
        }
        last.unwrap_or_else(|| ReachabilityDiagnostic {
            reachable: false,
            message: Some("host pool has no reachable members".to_owned()),
            category: Some("configuration".to_owned()),
        })
    }
}

/// Resolve every configured target for a validation mode.
pub fn resolve_targets(
    config: &LoadedConfig,
    mode: ValidationMode,
) -> Result<Vec<ResolvedTarget>, DispatchError> {
    resolve_targets_from_table(&config.data, mode)
}

/// Resolve every configured target from a merged TOML table.
pub fn resolve_targets_from_table(
    data: &Table,
    mode: ValidationMode,
) -> Result<Vec<ResolvedTarget>, DispatchError> {
    let targets = data
        .get("targets")
        .and_then(Value::as_table)
        .ok_or(DispatchError::MissingTargets)?;
    let base_validation = resolve_validation_table(data, mode);
    targets
        .iter()
        .map(|(name, value)| {
            let table = value.as_table().ok_or_else(|| {
                invalid_target(name, "target entry must be a TOML table".to_owned())
            })?;
            resolve_target(data, name, table, &base_validation)
        })
        .collect()
}

fn resolve_target(
    data: &Table,
    name: &str,
    table: &Table,
    base_validation: &Table,
) -> Result<ResolvedTarget, DispatchError> {
    let platform = table_str(table, "platform").unwrap_or("unknown").to_owned();
    let backend_name = normalize_backend_name(table);
    let validation_table = resolve_target_validation_table(data, table, base_validation);
    if table
        .get("fallback")
        .and_then(Value::as_array)
        .is_some_and(|fallback| !fallback.is_empty())
    {
        return resolved_fallback(data, name, &platform, table, base_validation, &backend_name);
    }
    resolve_backend_target(
        data,
        name,
        &platform,
        table,
        &validation_table,
        &backend_name,
    )
}

fn resolve_backend_target(
    data: &Table,
    name: &str,
    platform: &str,
    table: &Table,
    validation_table: &Table,
    backend_name: &str,
) -> Result<ResolvedTarget, DispatchError> {
    match backend_name {
        "local" => resolved_local(name, platform, table, validation_table),
        "ssh" => resolved_ssh(name, platform, backend_name, table, validation_table),
        "ssh-windows" => resolved_windows(name, platform, backend_name, table, validation_table),
        "cloud" => resolved_cloud(data, name, platform, table),
        "host-pool" => resolved_host_pool(data, name, platform, table, validation_table),
        backend => Err(DispatchError::UnsupportedBackend {
            target: name.to_owned(),
            backend: backend.to_owned(),
        }),
    }
}

fn resolved_local(
    name: &str,
    platform: &str,
    table: &Table,
    validation_table: &Table,
) -> Result<ResolvedTarget, DispatchError> {
    let contract = parse_contract(name, validation_table.get("contract"))?;
    let target = LocalTargetConfig {
        name: name.to_owned(),
        platform: platform.to_owned(),
        cwd: optional_path(table, "cwd"),
        timeout_secs: u64_value(table, "timeout_secs").unwrap_or(1_800),
    };
    let validation = LocalValidationConfig {
        command: optional_string(validation_table, "command"),
        stages: stage_map(validation_table),
        contract,
        prepared_state_enabled: prepared_state_enabled(validation_table),
        allow_tree_drift: bool_value(validation_table, "_allow_tree_drift").unwrap_or(false),
    };
    Ok(ResolvedTarget {
        name: name.to_owned(),
        platform: platform.to_owned(),
        backend_name: "local".to_owned(),
        warm_keepalive_seconds: extract_warm_keepalive_seconds(table.get("warm_keepalive_seconds")),
        host: None,
        backend: ResolvedBackend::Local(target),
        validation: ResolvedValidation::Local(validation),
        failure_parser: parse_failure_parser_field(name, table)?,
    })
}

fn resolved_ssh(
    name: &str,
    platform: &str,
    backend_name: &str,
    table: &Table,
    validation_table: &Table,
) -> Result<ResolvedTarget, DispatchError> {
    let target = SshTargetConfig {
        name: name.to_owned(),
        platform: platform.to_owned(),
        host: optional_string(table, "host"),
        repo_path: table_str(table, "repo_path").unwrap_or("~/repo").to_owned(),
        ssh_options: string_array(table, "ssh_options"),
        identity_file: optional_string(table, "identity_file"),
        remote_bundle_path: table_str(table, "remote_bundle_path")
            .unwrap_or("/tmp/shipyard.bundle")
            .to_owned(),
        local_repo_dir: optional_path(table, "local_repo_dir"),
        timeout_secs: u64_value(table, "timeout_secs").unwrap_or(1_800),
        bundle_upload_timeout_secs: u64_value(table, "bundle_upload_timeout_secs").unwrap_or(1_800),
        bundle_apply_timeout_secs: u64_value(table, "bundle_apply_timeout_secs").unwrap_or(1_800),
    };
    Ok(ResolvedTarget {
        name: name.to_owned(),
        platform: platform.to_owned(),
        backend_name: backend_name.to_owned(),
        warm_keepalive_seconds: extract_warm_keepalive_seconds(table.get("warm_keepalive_seconds")),
        host: target.host.clone(),
        backend: ResolvedBackend::Ssh(target),
        validation: ResolvedValidation::Ssh {
            validation: ssh_validation(validation_table),
            contract: parse_contract(name, validation_table.get("contract"))?,
        },
        failure_parser: parse_failure_parser_field(name, table)?,
    })
}

fn resolved_windows(
    name: &str,
    platform: &str,
    backend_name: &str,
    table: &Table,
    validation_table: &Table,
) -> Result<ResolvedTarget, DispatchError> {
    let target = WindowsTargetConfig {
        name: name.to_owned(),
        platform: platform.to_owned(),
        host: optional_string(table, "host"),
        repo_path: table_str(table, "repo_path")
            .unwrap_or(r"C:\repo")
            .to_owned(),
        ssh_options: string_array(table, "ssh_options"),
        identity_file: optional_string(table, "identity_file"),
        remote_bundle_path: table_str(table, "remote_bundle_path")
            .unwrap_or("shipyard.bundle")
            .to_owned(),
        local_repo_dir: optional_path(table, "local_repo_dir"),
        timeout_secs: u64_value(table, "timeout_secs").unwrap_or(1_800),
        bundle_upload_timeout_secs: u64_value(table, "bundle_upload_timeout_secs").unwrap_or(1_800),
        bundle_apply_timeout_secs: u64_value(table, "bundle_apply_timeout_secs").unwrap_or(1_800),
        windows_vs_detect: bool_value(table, "windows_vs_detect").unwrap_or(true),
        windows_host_mutex: bool_value(table, "windows_host_mutex").unwrap_or(true),
        windows_host_mutex_name: table_str(table, "windows_host_mutex_name")
            .unwrap_or(r"Global\ShipyardValidate")
            .to_owned(),
    };
    Ok(ResolvedTarget {
        name: name.to_owned(),
        platform: platform.to_owned(),
        backend_name: backend_name.to_owned(),
        warm_keepalive_seconds: extract_warm_keepalive_seconds(table.get("warm_keepalive_seconds")),
        host: target.host.clone(),
        backend: ResolvedBackend::Windows(target),
        validation: ResolvedValidation::Windows {
            validation: windows_validation(validation_table),
            contract: parse_contract(name, validation_table.get("contract"))?,
        },
        failure_parser: parse_failure_parser_field(name, table)?,
    })
}

fn resolved_cloud(
    data: &Table,
    name: &str,
    platform: &str,
    table: &Table,
) -> Result<ResolvedTarget, DispatchError> {
    let provider = optional_string(table, "runner_provider")
        .or_else(|| optional_string(table, "provider"))
        .or_else(|| dotted_string(data, "cloud.provider"))
        .unwrap_or_else(|| "github-hosted".to_owned());
    let provider_config = table_at(data, &format!("cloud.providers.{provider}"));
    let mut target = CloudTargetConfig::new(name, platform);
    target.workflow = optional_string(table, "workflow")
        .or_else(|| dotted_string(data, "cloud.workflow"))
        .unwrap_or_else(|| "ci.yml".to_owned());
    target.repository =
        optional_string(table, "repository").or_else(|| dotted_string(data, "cloud.repository"));
    target.runner_provider = Some(provider);
    target.runner_selector = optional_string(table, "runner_selector")
        .or_else(|| provider_config.and_then(|config| optional_string(config, "runner_selector")));
    target.runner_overrides = string_map(provider_config, "runner_overrides");
    target
        .runner_overrides
        .extend(string_map(Some(table), "runner_overrides"));
    target.poll_interval_secs = u64_value(table, "poll_interval_secs")
        .or_else(|| dotted_u64(data, "cloud.poll_interval_secs"))
        .unwrap_or(15);
    target.dispatch_settle_secs = u64_value(table, "dispatch_settle_secs")
        .or_else(|| dotted_u64(data, "cloud.dispatch_settle_secs"))
        .unwrap_or(30);
    target.max_poll_secs = u64_value(table, "cloud_max_poll_secs")
        .or_else(|| u64_value(table, "max_poll_secs"))
        .or_else(|| dotted_u64(data, "cloud.max_poll_secs"))
        .unwrap_or(3_600);
    target.failure_parser = parse_failure_parser_field(name, table)?;

    Ok(ResolvedTarget {
        name: name.to_owned(),
        platform: platform.to_owned(),
        backend_name: "cloud".to_owned(),
        warm_keepalive_seconds: extract_warm_keepalive_seconds(table.get("warm_keepalive_seconds")),
        host: None,
        backend: ResolvedBackend::Cloud(target),
        validation: ResolvedValidation::Cloud,
        failure_parser: parse_failure_parser_field(name, table)?,
    })
}

fn resolved_host_pool(
    data: &Table,
    name: &str,
    platform: &str,
    table: &Table,
    validation_table: &Table,
) -> Result<ResolvedTarget, DispatchError> {
    let pool_name = optional_string(table, "pool").ok_or_else(|| {
        invalid_target(
            name,
            "`pool` is required when backend = \"host-pool\"".to_owned(),
        )
    })?;
    let pools = parse_host_pools(data).map_err(|error| invalid_target(name, error.to_string()))?;
    let pool = pools
        .iter()
        .find(|pool| pool.name == pool_name)
        .ok_or_else(|| {
            invalid_target(
                name,
                format!("host pool {pool_name:?} is not configured under [host_pools]"),
            )
        })?;
    let members = pool
        .members
        .iter()
        .map(|member| resolved_host_pool_member(name, platform, table, validation_table, member))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ResolvedTarget {
        name: name.to_owned(),
        platform: platform.to_owned(),
        backend_name: "host-pool".to_owned(),
        warm_keepalive_seconds: extract_warm_keepalive_seconds(table.get("warm_keepalive_seconds")),
        host: None,
        backend: ResolvedBackend::HostPool(ResolvedHostPoolConfig {
            pool_name,
            strategy: pool.strategy.clone(),
            lease_stale_seconds: pool.lease_stale_seconds,
            heartbeat_interval_seconds: pool.heartbeat_interval_seconds,
            requires: string_array(table, "requires"),
            members,
        }),
        validation: ResolvedValidation::HostPool,
        failure_parser: parse_failure_parser_field(name, table)?,
    })
}

fn resolved_host_pool_member(
    name: &str,
    platform: &str,
    table: &Table,
    validation_table: &Table,
    member: &HostPoolMemberConfig,
) -> Result<ResolvedHostPoolMember, DispatchError> {
    let mut member_table = table.clone();
    member_table.remove("fallback");
    member_table.remove("host");
    member_table.remove("repo_path");
    member_table.remove("cwd");
    member_table.insert(
        "type".to_owned(),
        Value::String(member.backend_type.clone()),
    );
    member_table.insert(
        "backend".to_owned(),
        Value::String(member.backend_type.clone()),
    );
    if let Some(host) = &member.host {
        member_table.insert("host".to_owned(), Value::String(host.clone()));
    }
    if let Some(repo_path) = &member.repo_path {
        member_table.insert("repo_path".to_owned(), Value::String(repo_path.clone()));
    }
    if let Some(cwd) = &member.cwd {
        member_table.insert(
            "cwd".to_owned(),
            Value::String(cwd.to_string_lossy().into_owned()),
        );
    }

    let target = match member.backend_type.as_str() {
        "local" => resolved_local(name, platform, &member_table, validation_table)?,
        "ssh" => resolved_ssh(name, platform, "ssh", &member_table, validation_table)?,
        backend => {
            return Err(invalid_target(
                name,
                format!("host-pool member backend {backend:?} is unsupported"),
            ));
        }
    };
    let label = format!(
        "host-pool:{}/{}",
        table_str(table, "pool").unwrap_or("?"),
        member.id
    );
    Ok(ResolvedHostPoolMember {
        id: member.id.clone(),
        target: Box::new(target),
        profile_label: label.clone(),
        label,
        max_concurrency: member.max_concurrency,
        capabilities: member.capabilities.clone(),
    })
}

fn resolved_fallback(
    data: &Table,
    name: &str,
    platform: &str,
    table: &Table,
    base_validation: &Table,
    primary_backend_name: &str,
) -> Result<ResolvedTarget, DispatchError> {
    let mut primary_table = table.clone();
    primary_table.remove("fallback");
    primary_table.insert(
        "type".to_owned(),
        Value::String(primary_backend_name.to_owned()),
    );

    let primary = resolve_fallback_backend(data, name, platform, &primary_table, base_validation)?
        .ok_or_else(|| {
            invalid_target(
                name,
                "primary fallback backend must be executable".to_owned(),
            )
        })?;
    let mut backends = vec![primary];

    let fallback = table
        .get("fallback")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_target(name, "`fallback` must be an array".to_owned()))?;
    for value in fallback {
        let fallback_table = value
            .as_table()
            .ok_or_else(|| invalid_target(name, "`fallback` entries must be tables".to_owned()))?;
        let merged = merged_fallback_table(table, fallback_table);
        if normalize_backend_name(&merged) == "vm" {
            continue;
        }
        if let Some(backend) =
            resolve_fallback_backend(data, name, platform, &merged, base_validation)?
        {
            backends.push(backend);
        }
    }

    let primary_target = backends
        .first()
        .expect("fallback chain has primary")
        .target
        .as_ref();
    Ok(ResolvedTarget {
        name: name.to_owned(),
        platform: platform.to_owned(),
        backend_name: primary_target.backend_name.clone(),
        warm_keepalive_seconds: extract_warm_keepalive_seconds(table.get("warm_keepalive_seconds")),
        host: primary_target.host.clone(),
        backend: ResolvedBackend::Fallback(FallbackTargetConfig {
            backends,
            requires: string_array(table, "requires"),
            heartbeat_stale_secs: u64_value(table, "heartbeat_stale_secs").unwrap_or(90),
        }),
        validation: ResolvedValidation::Fallback,
        failure_parser: parse_failure_parser_field(name, table)?,
    })
}

fn resolve_fallback_backend(
    data: &Table,
    name: &str,
    platform: &str,
    table: &Table,
    base_validation: &Table,
) -> Result<Option<FallbackBackend>, DispatchError> {
    let backend_name = normalize_backend_name(table);
    if backend_name == "vm" {
        return Ok(None);
    }
    let validation_table = resolve_target_validation_table(data, table, base_validation);
    let target = resolve_backend_target(
        data,
        name,
        platform,
        table,
        &validation_table,
        &backend_name,
    )?;
    let label = backend_label(&target);
    Ok(Some(FallbackBackend {
        profile_label: profile_label(&target),
        capabilities: string_array(table, "capabilities"),
        target: Box::new(target),
        label,
    }))
}

fn merged_fallback_table(base: &Table, fallback: &Table) -> Table {
    let mut merged = base.clone();
    merged.remove("fallback");
    for (key, value) in fallback {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

fn backend_label(target: &ResolvedTarget) -> String {
    match &target.backend {
        ResolvedBackend::Local(_) => "local".to_owned(),
        ResolvedBackend::Ssh(target) => {
            format!("ssh:{}", target.host.as_deref().unwrap_or("?"))
        }
        ResolvedBackend::Windows(target) => {
            format!("ssh-windows:{}", target.host.as_deref().unwrap_or("?"))
        }
        ResolvedBackend::Cloud(target) => {
            format!("cloud:{}", target.runner_provider.as_deref().unwrap_or("?"))
        }
        ResolvedBackend::HostPool(pool) => format!("host-pool:{}", pool.pool_name),
        ResolvedBackend::Fallback(_) => target.backend_name.clone(),
    }
}

fn profile_label(target: &ResolvedTarget) -> String {
    match &target.backend {
        ResolvedBackend::Cloud(target) => format!(
            "{}.{}",
            target.runner_provider.as_deref().unwrap_or("?"),
            target.runner_selector.as_deref().unwrap_or("default")
        ),
        ResolvedBackend::HostPool(pool) => format!("host-pool:{}", pool.pool_name),
        _ => backend_label(target),
    }
}

fn resolve_validation_table(data: &Table, mode: ValidationMode) -> Table {
    let Some(validation) = data.get("validation").and_then(Value::as_table) else {
        return Table::new();
    };
    let preferred = match mode {
        ValidationMode::Full => "default",
        ValidationMode::Smoke => "smoke",
    };
    let fallback = if mode == ValidationMode::Smoke {
        validation.get("default").and_then(Value::as_table)
    } else {
        None
    };
    let mut result = validation
        .get(preferred)
        .and_then(Value::as_table)
        .or(fallback)
        .cloned()
        .unwrap_or_else(|| validation.clone());
    for peer in ["contract", "prepared_state"] {
        if !result.contains_key(peer)
            && let Some(value) = validation.get(peer)
        {
            result.insert(peer.to_owned(), value.clone());
        }
    }
    result
}

fn resolve_target_validation_table(data: &Table, target: &Table, base: &Table) -> Table {
    let mut result = base.clone();
    let platform_os = target
        .get("platform")
        .and_then(Value::as_str)
        .and_then(|platform| platform.split('-').next())
        .unwrap_or_default();

    if let Some(override_table) = data
        .get("validation")
        .and_then(Value::as_table)
        .and_then(|validation| validation.get("overrides"))
        .and_then(Value::as_table)
        .and_then(|overrides| overrides.get(platform_os))
        .and_then(Value::as_table)
    {
        merge_shallow(&mut result, override_table);
    }
    if let Some(override_table) = base
        .get("overrides")
        .and_then(Value::as_table)
        .and_then(|overrides| overrides.get(platform_os))
        .and_then(Value::as_table)
    {
        merge_shallow(&mut result, override_table);
    }
    result.remove("overrides");
    if let Some(target_validation) = target.get("validation").and_then(Value::as_table) {
        merge_shallow(&mut result, target_validation);
    }
    result
}

fn merge_shallow(base: &mut Table, overlay: &Table) {
    base.extend(
        overlay
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
}

fn parse_contract(
    target: &str,
    value: Option<&Value>,
) -> Result<Option<ContractConfig>, DispatchError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let table = value
        .as_table()
        .ok_or_else(|| invalid_validation(target, "`contract` must be a TOML table".to_owned()))?;
    let markers = string_array(table, "markers");
    if markers.is_empty() {
        return Ok(None);
    }
    Ok(Some(ContractConfig {
        markers,
        require_at_least_one: bool_value(table, "require_at_least_one").unwrap_or(true),
        enforce: bool_value(table, "enforce").unwrap_or(true),
    }))
}

fn ssh_validation(table: &Table) -> SshValidation {
    optional_string(table, "command").map_or_else(
        || SshValidation::Stages(stage_map(table)),
        SshValidation::Command,
    )
}

fn windows_validation(table: &Table) -> WindowsValidation {
    optional_string(table, "command").map_or_else(
        || WindowsValidation::Stages(stage_map(table)),
        WindowsValidation::Command,
    )
}

fn stage_map(table: &Table) -> BTreeMap<String, String> {
    STAGE_ORDER
        .iter()
        .filter_map(|stage| {
            optional_string(table, stage).map(|command| ((*stage).to_owned(), command))
        })
        .collect()
}

fn prepared_state_enabled(table: &Table) -> bool {
    table
        .get("prepared_state")
        .and_then(Value::as_table)
        .and_then(|section| bool_value(section, "enabled"))
        .unwrap_or(false)
}

fn normalize_backend_name(table: &Table) -> String {
    let backend = table
        .get("type")
        .or_else(|| table.get("backend"))
        .and_then(Value::as_str)
        .unwrap_or("local")
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-");
    if backend == "ssh"
        && table
            .get("platform")
            .and_then(Value::as_str)
            .is_some_and(|platform| platform.starts_with("windows"))
    {
        "ssh-windows".to_owned()
    } else {
        backend
    }
}

fn mode_label(mode: ValidationMode) -> String {
    match mode {
        ValidationMode::Full => "full",
        ValidationMode::Smoke => "smoke",
    }
    .to_owned()
}

fn table_str<'a>(table: &'a Table, key: &str) -> Option<&'a str> {
    table.get(key)?.as_str()
}

fn optional_string(table: &Table, key: &str) -> Option<String> {
    table_str(table, key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn optional_path(table: &Table, key: &str) -> Option<PathBuf> {
    optional_string(table, key).map(PathBuf::from)
}

fn u64_value(table: &Table, key: &str) -> Option<u64> {
    table
        .get(key)
        .and_then(Value::as_integer)
        .and_then(|value| u64::try_from(value).ok())
}

fn bool_value(table: &Table, key: &str) -> Option<bool> {
    table.get(key).and_then(Value::as_bool)
}

fn string_array(table: &Table, key: &str) -> Vec<String> {
    table
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn table_at<'a>(data: &'a Table, dotted_key: &str) -> Option<&'a Table> {
    let mut current = data;
    let mut parts = dotted_key.split('.').peekable();
    while let Some(part) = parts.next() {
        let value = current.get(part)?;
        if parts.peek().is_none() {
            return value.as_table();
        }
        current = value.as_table()?;
    }
    None
}

fn dotted_string(data: &Table, dotted_key: &str) -> Option<String> {
    let (table_key, key) = dotted_key.rsplit_once('.')?;
    table_at(data, table_key).and_then(|table| optional_string(table, key))
}

fn dotted_u64(data: &Table, dotted_key: &str) -> Option<u64> {
    let (table_key, key) = dotted_key.rsplit_once('.')?;
    table_at(data, table_key).and_then(|table| u64_value(table, key))
}

fn string_map(table: Option<&Table>, key: &str) -> BTreeMap<String, String> {
    table
        .and_then(|table| table.get(key))
        .and_then(Value::as_table)
        .map(|items| {
            items
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn filter_backends_by_requires(chain: &FallbackTargetConfig) -> Vec<&FallbackBackend> {
    let requires = chain
        .requires
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if requires.is_empty() {
        return chain.backends.iter().collect();
    }
    chain
        .backends
        .iter()
        .filter(|backend| {
            requires.iter().all(|required| {
                backend
                    .capabilities
                    .iter()
                    .any(|capability| capability.trim() == *required)
            })
        })
        .collect()
}

fn filter_host_pool_members_by_requires(
    pool: &ResolvedHostPoolConfig,
) -> Vec<&ResolvedHostPoolMember> {
    let requires = pool
        .requires
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if requires.is_empty() {
        return pool.members.iter().collect();
    }
    pool.members
        .iter()
        .filter(|member| {
            requires.iter().all(|required| {
                member
                    .capabilities
                    .iter()
                    .any(|capability| capability.trim() == *required)
            })
        })
        .collect()
}

fn capability_mismatch_result(
    target: &ResolvedTarget,
    chain: &FallbackTargetConfig,
) -> TargetResult {
    let mut requires = chain
        .requires
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    requires.sort();
    let tried = chain
        .backends
        .iter()
        .map(|backend| backend.profile_label.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let backend = chain
        .backends
        .first()
        .map_or_else(|| "none".to_owned(), |backend| backend.label.clone());
    let mut result = TargetResult::new(
        &target.name,
        &target.platform,
        crate::job::TargetStatus::Error,
        backend,
    );
    result.error_message = Some(format!(
        "no provider satisfies requires={requires:?}: tried [{tried}]"
    ));
    result.failure_class = Some(crate::classify::FailureClass::Infra.as_str().to_owned());
    result
}

fn host_pool_capability_mismatch_result(
    target: &ResolvedTarget,
    pool: &ResolvedHostPoolConfig,
) -> TargetResult {
    let mut requires = pool
        .requires
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    requires.sort();
    let tried = pool
        .members
        .iter()
        .map(|member| member.profile_label.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let backend = pool
        .members
        .first()
        .map_or_else(|| "none".to_owned(), |member| member.label.clone());
    let mut result = TargetResult::new(
        &target.name,
        &target.platform,
        crate::job::TargetStatus::Error,
        backend,
    );
    result.error_message = Some(format!(
        "no host-pool member satisfies requires={requires:?}: tried [{tried}]"
    ));
    result.failure_class = Some(crate::classify::FailureClass::Infra.as_str().to_owned());
    result
}

fn probe_failed_result(
    target: &ResolvedTarget,
    label: &str,
    log_path: &std::path::Path,
) -> TargetResult {
    let now = Utc::now();
    let mut result = TargetResult::new(
        &target.name,
        &target.platform,
        crate::job::TargetStatus::Unreachable,
        label,
    );
    result.started_at = Some(now);
    result.completed_at = Some(now);
    result.log_path = Some(log_path.to_string_lossy().into_owned());
    result.error_message = Some(format!("Probe failed for {label}"));
    result.failure_class = Some(crate::classify::FailureClass::Infra.as_str().to_owned());
    result
}

fn host_pool_busy_result(
    target: &ResolvedTarget,
    label: &str,
    log_path: &std::path::Path,
) -> TargetResult {
    let now = Utc::now();
    let mut result = TargetResult::new(
        &target.name,
        &target.platform,
        crate::job::TargetStatus::Unreachable,
        label,
    );
    result.started_at = Some(now);
    result.completed_at = Some(now);
    result.log_path = Some(log_path.to_string_lossy().into_owned());
    result.error_message = Some(format!("No host-pool lease available for {label}"));
    result.failure_class = Some(crate::classify::FailureClass::Infra.as_str().to_owned());
    result
}

fn host_pool_lease_unavailable_result(
    target: &ResolvedTarget,
    label: &str,
    log_path: &std::path::Path,
) -> TargetResult {
    let now = Utc::now();
    let mut result = TargetResult::new(
        &target.name,
        &target.platform,
        crate::job::TargetStatus::Pending,
        label,
    );
    result.started_at = Some(now);
    result.log_path = Some(log_path.to_string_lossy().into_owned());
    result.error_message = Some(format!(
        "Host-pool lease unavailable for {label}; defer job"
    ));
    result.scheduler_defer_reason = Some("host_pool_lease_unavailable".to_owned());
    result
}

fn host_pool_error_result(
    target: &ResolvedTarget,
    label: &str,
    log_path: &std::path::Path,
    message: &str,
) -> TargetResult {
    let now = Utc::now();
    let mut result = TargetResult::new(
        &target.name,
        &target.platform,
        crate::job::TargetStatus::Error,
        label,
    );
    result.started_at = Some(now);
    result.completed_at = Some(now);
    result.log_path = Some(log_path.to_string_lossy().into_owned());
    result.error_message = Some(message.to_owned());
    result.failure_class = Some(crate::classify::FailureClass::Infra.as_str().to_owned());
    result
}

fn demote_stale_result(mut result: TargetResult, heartbeat_stale_secs: u64) -> TargetResult {
    if result.status != crate::job::TargetStatus::Pass
        && should_evict_for_heartbeat(&result, heartbeat_stale_secs)
    {
        result.status = crate::job::TargetStatus::Unreachable;
        if result.error_message.is_none() {
            result.error_message = Some(format!("Runner went silent for >{heartbeat_stale_secs}s"));
        }
        if result.failure_class.is_none() {
            result.failure_class = Some(crate::classify::FailureClass::Infra.as_str().to_owned());
        }
    }
    result
}

fn should_evict_for_heartbeat(result: &TargetResult, heartbeat_stale_secs: u64) -> bool {
    if result.liveness.as_deref() == Some("stuck") {
        return true;
    }
    let Some(last_heartbeat_at) = result.last_heartbeat_at else {
        return false;
    };
    let reference = result.completed_at.unwrap_or_else(Utc::now);
    let age = reference.signed_duration_since(last_heartbeat_at);
    age.num_milliseconds()
        >= i64::try_from(heartbeat_stale_secs.saturating_mul(1_000)).unwrap_or(i64::MAX)
}

fn failover_pass_result(
    mut result: TargetResult,
    backend_label: &str,
    primary_label: &str,
    last_result: Option<&TargetResult>,
) -> TargetResult {
    result.backend = format!("{backend_label}-failover");
    result.primary_backend = Some(primary_label.to_owned());
    result.failover_reason = Some(
        last_result
            .and_then(|result| result.error_message.clone())
            .unwrap_or_else(|| "unknown".to_owned()),
    );
    result
}

fn exhausted_result(
    target: &ResolvedTarget,
    primary_label: &str,
    last_result: Option<TargetResult>,
) -> TargetResult {
    if let Some(mut result) = last_result {
        result.backend = format!("{primary_label}-exhausted");
        result.primary_backend = Some(primary_label.to_owned());
        result.failover_reason = Some("All backends exhausted".to_owned());
        if result.failure_class.is_none() {
            result.failure_class = Some(crate::classify::FailureClass::Infra.as_str().to_owned());
        }
        return result;
    }
    let mut result = TargetResult::new(
        &target.name,
        &target.platform,
        crate::job::TargetStatus::Error,
        format!("{primary_label}-exhausted"),
    );
    result.primary_backend = Some(primary_label.to_owned());
    result.failover_reason = Some("No usable executors found".to_owned());
    result.error_message = Some("All backends skipped (no matching executors)".to_owned());
    result.failure_class = Some(crate::classify::FailureClass::Unknown.as_str().to_owned());
    result
}

fn attempt_log_path(base: &std::path::Path, attempt: usize) -> PathBuf {
    if attempt == 0 {
        return base.to_path_buf();
    }
    PathBuf::from(format!("{}.attempt-{attempt}", base.to_string_lossy()))
}

fn invalid_target(target: &str, reason: String) -> DispatchError {
    DispatchError::InvalidTargetConfig {
        target: target.to_owned(),
        reason,
    }
}

struct HostPoolLeaseGuard {
    store: HostPoolLeaseStore,
    lease_id: String,
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl HostPoolLeaseGuard {
    fn new(
        store: HostPoolLeaseStore,
        lease_id: String,
        heartbeat_interval_seconds: u64,
        lease_stale_seconds: u64,
    ) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel();
        let heartbeat_store = store.clone();
        let heartbeat_lease_id = lease_id.clone();
        let interval = StdDuration::from_secs(heartbeat_interval_seconds.max(1));
        let handle = thread::spawn(move || {
            loop {
                match stop_rx.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        let _ = heartbeat_store
                            .heartbeat(&heartbeat_lease_id, lease_stale_seconds.max(1));
                    }
                }
            }
        });
        Self {
            store,
            lease_id,
            stop: Some(stop_tx),
            handle: Some(handle),
        }
    }
}

impl Drop for HostPoolLeaseGuard {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = self.store.release(&self.lease_id);
    }
}

/// Parse `[targets.<name>] failure_parser = "..."` against Shipyard's
/// allow-list. See `crate::diagnostics::ALLOWED_PARSERS`. Returns `Ok(None)`
/// when unset and `Err(_)` when set to a value not in the registry.
fn parse_failure_parser_field(name: &str, table: &Table) -> Result<Option<String>, DispatchError> {
    let Some(raw) = optional_string(table, "failure_parser") else {
        return Ok(None);
    };
    crate::diagnostics::validate_parser_name(&raw)
        .map(Some)
        .map_err(|error| invalid_target(name, error.to_string()))
}

fn invalid_validation(target: &str, reason: String) -> DispatchError {
    DispatchError::InvalidValidationConfig {
        target: target.to_owned(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{Duration, Utc};
    use toml::Table;

    use super::{ResolvedBackend, ResolvedValidation, resolve_targets_from_table};
    use crate::executor::ssh::SshValidation;
    use crate::executor::ssh_windows::WindowsValidation;
    use crate::host_pool::{HostPoolLeaseRequest, HostPoolLeaseStore, default_lease_path};
    use crate::job::{TargetResult, TargetStatus, ValidationMode};

    fn table(input: &str) -> Table {
        input.parse::<Table>().expect("valid TOML")
    }

    #[test]
    fn resolves_local_target_with_contract_and_prepared_state() {
        let config = table(
            r#"
            [validation.default]
            build = "cargo build"
            test = "cargo test"

            [validation.contract]
            markers = ["SMOKE", "FULL"]
            enforce = false

            [validation.prepared_state]
            enabled = true

            [targets.mac]
            backend = "local"
            platform = "macos-arm64"
            cwd = "/repo"
            warm_keepalive_seconds = 600
            "#,
        );
        let targets = resolve_targets_from_table(&config, ValidationMode::Full).expect("targets");
        let mac = &targets[0];

        assert_eq!(mac.name, "mac");
        assert_eq!(mac.backend_name, "local");
        assert_eq!(mac.warm_keepalive_seconds, 600);
        assert_eq!(mac.workdir(), Some("/repo".to_owned()));
        assert!(
            matches!(&mac.backend, ResolvedBackend::Local(target) if target.cwd == Some(PathBuf::from("/repo")))
        );
        let ResolvedValidation::Local(validation) = &mac.validation else {
            panic!("local validation");
        };
        assert!(validation.prepared_state_enabled);
        assert_eq!(validation.stages["build"], "cargo build");
        assert_eq!(
            validation.contract.as_ref().expect("contract").markers,
            ["SMOKE", "FULL"]
        );
        assert!(!validation.contract.as_ref().expect("contract").enforce);
    }

    #[test]
    fn platform_override_and_target_override_match_python_order() {
        let config = table(
            r#"
            [validation.default]
            build = "base build"
            test = "base test"

            [validation.overrides.windows]
            build = "legacy windows build"

            [validation.default.overrides.windows]
            build = "nested windows build"

            [targets.windows]
            backend = "ssh"
            platform = "windows-x64"
            host = "win"

            [targets.windows.validation]
            test = "target test"
            "#,
        );
        let targets = resolve_targets_from_table(&config, ValidationMode::Full).expect("targets");
        let windows = &targets[0];

        assert_eq!(windows.backend_name, "ssh-windows");
        assert!(
            matches!(&windows.backend, ResolvedBackend::Windows(target) if target.host.as_deref() == Some("win"))
        );
        let ResolvedValidation::Windows { validation, .. } = &windows.validation else {
            panic!("windows validation");
        };
        let WindowsValidation::Stages(stages) = validation else {
            panic!("stages");
        };
        assert_eq!(stages["build"], "nested windows build");
        assert_eq!(stages["test"], "target test");
    }

    #[test]
    fn command_overrides_stages_for_ssh() {
        let config = table(
            r#"
            [validation.default]
            command = "make ci"
            build = "ignored"

            [targets.ubuntu]
            backend = "ssh"
            host = "ubuntu"
            repo_path = "/srv/repo"
            ssh_options = ["-p", "2222"]
            identity_file = "~/.ssh/id_ed25519"
            "#,
        );
        let targets = resolve_targets_from_table(&config, ValidationMode::Full).expect("targets");
        let ubuntu = &targets[0];

        assert_eq!(ubuntu.backend_name, "ssh");
        assert_eq!(ubuntu.host.as_deref(), Some("ubuntu"));
        assert!(
            matches!(&ubuntu.backend, ResolvedBackend::Ssh(target) if target.repo_path == "/srv/repo" && target.ssh_options == ["-p", "2222"])
        );
        let ResolvedValidation::Ssh { validation, .. } = &ubuntu.validation else {
            panic!("ssh validation");
        };
        assert!(matches!(validation, SshValidation::Command(command) if command == "make ci"));
    }

    #[test]
    fn resolves_fallback_chain_and_skips_vm_entries() {
        let fallback = table(
            r#"
            [cloud]
            provider = "namespace"
            repository = "owner/repo"

            [targets.ubuntu]
            backend = "ssh"
            platform = "linux-x64"
            host = "linux"
            capabilities = ["x64"]
            requires = ["x64"]
            heartbeat_stale_secs = 12
            fallback = [
              { type = "vm", vm_name = "legacy" },
              { type = "cloud", provider = "namespace", capabilities = ["x64", "gpu"] },
            ]
            "#,
        );
        let targets = resolve_targets_from_table(&fallback, ValidationMode::Full).expect("targets");
        let ubuntu = &targets[0];

        assert_eq!(ubuntu.backend_name, "ssh");
        assert_eq!(ubuntu.host.as_deref(), Some("linux"));
        assert!(matches!(&ubuntu.validation, ResolvedValidation::Fallback));
        let ResolvedBackend::Fallback(chain) = &ubuntu.backend else {
            panic!("fallback backend");
        };
        assert_eq!(chain.requires, ["x64"]);
        assert_eq!(chain.heartbeat_stale_secs, 12);
        assert_eq!(chain.backends.len(), 2);
        assert_eq!(chain.backends[0].label, "ssh:linux");
        assert_eq!(chain.backends[0].capabilities, ["x64"]);
        assert_eq!(chain.backends[1].label, "cloud:namespace");
        assert_eq!(chain.backends[1].capabilities, ["x64", "gpu"]);
    }

    #[test]
    fn resolves_host_pool_target_members() {
        let config = table(
            r#"
            [validation.default]
            command = "make ci"

            [host_pools.local_macs]
            lease_stale_seconds = 120
            heartbeat_interval_seconds = 5

            [[host_pools.local_macs.members]]
            id = "mac-studio"
            type = "ssh"
            host = "mac-studio"
            repo_path = "/Users/shipyard/work/shipyard"
            capabilities = ["macos", "arm64"]

            [[host_pools.local_macs.members]]
            id = "local"
            type = "local"
            cwd = "/repo"
            capabilities = ["macos", "arm64"]

            [targets.mac]
            backend = "host-pool"
            pool = "local_macs"
            platform = "macos-arm64"
            requires = ["macos", "arm64"]
            "#,
        );

        let targets = resolve_targets_from_table(&config, ValidationMode::Full).expect("targets");
        let mac = &targets[0];

        assert_eq!(mac.backend_name, "host-pool");
        assert!(matches!(mac.validation, ResolvedValidation::HostPool));
        let ResolvedBackend::HostPool(pool) = &mac.backend else {
            panic!("host-pool backend");
        };
        assert_eq!(pool.pool_name, "local_macs");
        assert_eq!(pool.lease_stale_seconds, 120);
        assert_eq!(pool.heartbeat_interval_seconds, 5);
        assert_eq!(pool.requires, ["macos", "arm64"]);
        assert_eq!(pool.members[0].id, "mac-studio");
        assert_eq!(pool.members[0].label, "host-pool:local_macs/mac-studio");
        assert!(
            matches!(&pool.members[0].target.backend, ResolvedBackend::Ssh(target) if target.host.as_deref() == Some("mac-studio") && target.repo_path == "/Users/shipyard/work/shipyard")
        );
        assert!(
            matches!(&pool.members[1].target.backend, ResolvedBackend::Local(target) if target.cwd == Some(PathBuf::from("/repo")))
        );
    }

    #[test]
    fn fallback_helpers_preserve_python_result_provenance() {
        let pass = TargetResult::new("linux", "linux-x64", TargetStatus::Pass, "cloud");
        let mut last =
            TargetResult::new("linux", "linux-x64", TargetStatus::Unreachable, "ssh:host");
        last.error_message = Some("Probe failed for ssh:host".to_owned());

        let pass = super::failover_pass_result(pass, "cloud:namespace", "ssh:host", Some(&last));

        assert_eq!(pass.status, TargetStatus::Pass);
        assert_eq!(pass.backend, "cloud:namespace-failover");
        assert_eq!(pass.primary_backend.as_deref(), Some("ssh:host"));
        assert_eq!(
            pass.failover_reason.as_deref(),
            Some("Probe failed for ssh:host")
        );

        last.failure_class = None;
        let exhausted =
            super::exhausted_result(&resolved_local_target("linux"), "ssh:host", Some(last));
        assert_eq!(exhausted.backend, "ssh:host-exhausted");
        assert_eq!(exhausted.primary_backend.as_deref(), Some("ssh:host"));
        assert_eq!(
            exhausted.failover_reason.as_deref(),
            Some("All backends exhausted")
        );
        assert_eq!(exhausted.failure_class.as_deref(), Some("INFRA"));
    }

    #[test]
    fn fallback_capability_mismatch_lists_requires_and_tried_profiles() {
        let target = resolved_local_target("linux");
        let chain = super::FallbackTargetConfig {
            requires: vec!["gpu".to_owned(), "arm64".to_owned()],
            heartbeat_stale_secs: 90,
            backends: vec![super::FallbackBackend {
                target: Box::new(target.clone()),
                label: "local".to_owned(),
                profile_label: "local".to_owned(),
                capabilities: vec!["x64".to_owned()],
            }],
        };

        assert!(super::filter_backends_by_requires(&chain).is_empty());
        let result = super::capability_mismatch_result(&target, &chain);

        assert_eq!(result.status, TargetStatus::Error);
        assert_eq!(result.failure_class.as_deref(), Some("INFRA"));
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("requires=[\"arm64\", \"gpu\"]"))
        );
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("tried [local]"))
        );
    }

    #[test]
    fn fallback_stale_heartbeat_demotes_non_passing_result() {
        let mut result = TargetResult::new("linux", "linux-x64", TargetStatus::Error, "cloud");
        let now = Utc::now();
        result.completed_at = Some(now);
        result.last_heartbeat_at = Some(now - Duration::seconds(120));

        let result = super::demote_stale_result(result, 90);

        assert_eq!(result.status, TargetStatus::Unreachable);
        assert_eq!(result.failure_class.as_deref(), Some("INFRA"));
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("Runner went silent"))
        );
    }

    #[test]
    fn fallback_dispatch_forwards_progress_callback_to_attempts() {
        let config = table(
            r#"
            [validation.default]
            build = "echo fallback-build"

            [targets.mac]
            backend = "local"
            platform = "macos-arm64"
            fallback = [
              { type = "local" },
            ]
            "#,
        );
        let target = resolve_targets_from_table(&config, ValidationMode::Full)
            .expect("targets")
            .remove(0);
        let temp = tempfile::tempdir().expect("tempdir");
        let mut phases = Vec::new();
        let status = {
            let mut callback = |event: crate::executor::streaming::ProgressEvent| {
                if let Some(phase) = event.phase {
                    phases.push(phase);
                }
            };
            super::ExecutorDispatcher::new(None)
                .validate(super::DispatchValidationRequest {
                    job_id: None,
                    defer_host_pool_lease_unavailable: false,
                    sha: "abc1234".to_owned(),
                    branch: "main".to_owned(),
                    target: &target,
                    log_path: temp.path().join("fallback.log"),
                    resume_from: None,
                    mode: ValidationMode::Full,
                    progress_callback: Some(&mut callback),
                })
                .status
        };

        assert_eq!(status, TargetStatus::Pass);
        assert!(phases.iter().any(|phase| phase == "build"));
    }

    #[test]
    fn host_pool_dispatch_acquires_and_releases_local_lease() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let config = table(&format!(
            r#"
            [validation.default]
            command = "grep -q '\\\"job_id\\\": \\\"job-host-pool\\\"' '{}'"

            [host_pools.local_macs]

            [[host_pools.local_macs.members]]
            id = "local"
            type = "local"
            cwd = "{}"
            capabilities = ["macos", "arm64"]

            [targets.mac]
            backend = "host-pool"
            pool = "local_macs"
            platform = "macos-arm64"
            requires = ["macos", "arm64"]
            "#,
            default_lease_path(temp.path()).display(),
            repo.display()
        ));
        let target = resolve_targets_from_table(&config, ValidationMode::Full)
            .expect("targets")
            .remove(0);
        let dispatcher = super::ExecutorDispatcher::new_with_state_dir(None, temp.path());

        let result = dispatcher.validate(super::DispatchValidationRequest {
            job_id: Some("job-host-pool".to_owned()),
            defer_host_pool_lease_unavailable: false,
            sha: "abc1234".to_owned(),
            branch: "main".to_owned(),
            target: &target,
            log_path: temp.path().join("host-pool.log"),
            resume_from: None,
            mode: ValidationMode::Full,
            progress_callback: None,
        });

        assert_eq!(result.status, TargetStatus::Pass);
        assert_eq!(result.backend, "host-pool:local_macs/local");
        let store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        assert!(store.leases().expect("leases").is_empty());
    }

    #[test]
    fn host_pool_dispatch_skips_busy_member() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let config = table(&format!(
            r#"
            [validation.default]
            command = "true"

            [host_pools.local_macs]

            [[host_pools.local_macs.members]]
            id = "mac-studio"
            type = "local"
            cwd = "{}"
            capabilities = ["macos", "arm64"]

            [[host_pools.local_macs.members]]
            id = "local"
            type = "local"
            cwd = "{}"
            capabilities = ["macos", "arm64"]

            [targets.mac]
            backend = "host-pool"
            pool = "local_macs"
            platform = "macos-arm64"
            requires = ["macos", "arm64"]
            "#,
            repo.display(),
            repo.display()
        ));
        let store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        let busy = store
            .acquire(&HostPoolLeaseRequest {
                pool_name: "local_macs".to_owned(),
                member_id: "mac-studio".to_owned(),
                target_name: "mac".to_owned(),
                backend: "local".to_owned(),
                host: None,
                job_id: Some("existing".to_owned()),
                branch: "main".to_owned(),
                sha: "busy".to_owned(),
                max_concurrency: 1,
                lease_stale_seconds: 180,
            })
            .expect("acquire")
            .expect("busy lease");
        let target = resolve_targets_from_table(&config, ValidationMode::Full)
            .expect("targets")
            .remove(0);
        let dispatcher =
            super::ExecutorDispatcher::new_with_host_pool_store(None, Some(store.clone()));

        let result = dispatcher.validate(super::DispatchValidationRequest {
            job_id: None,
            defer_host_pool_lease_unavailable: false,
            sha: "abc1234".to_owned(),
            branch: "main".to_owned(),
            target: &target,
            log_path: temp.path().join("host-pool-busy.log"),
            resume_from: None,
            mode: ValidationMode::Full,
            progress_callback: None,
        });

        assert_eq!(result.status, TargetStatus::Pass);
        assert_eq!(result.backend, "host-pool:local_macs/local-failover");
        assert_eq!(
            result.primary_backend.as_deref(),
            Some("host-pool:local_macs/mac-studio")
        );
        let leases = store.leases().expect("leases");
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].lease_id, busy.lease_id);
        store.release(&busy.lease_id).expect("release busy");
    }

    #[test]
    fn scheduler_host_pool_dispatch_defers_when_lease_unavailable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let config = table(&format!(
            r#"
            [validation.default]
            command = "true"

            [host_pools.local_macs]

            [[host_pools.local_macs.members]]
            id = "local"
            type = "local"
            cwd = "{}"
            capabilities = ["macos", "arm64"]

            [targets.mac]
            backend = "host-pool"
            pool = "local_macs"
            platform = "macos-arm64"
            requires = ["macos", "arm64"]
            "#,
            repo.display()
        ));
        let store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        let busy = store
            .acquire(&HostPoolLeaseRequest {
                pool_name: "local_macs".to_owned(),
                member_id: "local".to_owned(),
                target_name: "mac".to_owned(),
                backend: "local".to_owned(),
                host: None,
                job_id: Some("existing".to_owned()),
                branch: "main".to_owned(),
                sha: "busy".to_owned(),
                max_concurrency: 1,
                lease_stale_seconds: 180,
            })
            .expect("acquire")
            .expect("busy lease");
        let target = resolve_targets_from_table(&config, ValidationMode::Full)
            .expect("targets")
            .remove(0);
        let dispatcher =
            super::ExecutorDispatcher::new_with_host_pool_store(None, Some(store.clone()));

        let result = dispatcher.validate(super::DispatchValidationRequest {
            job_id: Some("job-waiting".to_owned()),
            defer_host_pool_lease_unavailable: true,
            sha: "abc1234".to_owned(),
            branch: "main".to_owned(),
            target: &target,
            log_path: temp.path().join("host-pool-deferred.log"),
            resume_from: None,
            mode: ValidationMode::Full,
            progress_callback: None,
        });

        assert_eq!(result.status, TargetStatus::Pending);
        assert_eq!(
            result.scheduler_defer_reason.as_deref(),
            Some("host_pool_lease_unavailable")
        );
        assert!(!result.is_terminal());
        assert!(result.failure_class.is_none());
        assert_eq!(store.leases().expect("leases")[0].lease_id, busy.lease_id);
        store.release(&busy.lease_id).expect("release busy");
    }

    #[test]
    fn resolves_cloud_target_with_namespace_provider() {
        let cloud = table(
            r#"
            [cloud]
            provider = "namespace"
            repository = "owner/repo"
            poll_interval_secs = 1
            dispatch_settle_secs = 2
            max_poll_secs = 3

            [cloud.providers.namespace]
            runner_selector = "namespace-profile-generouscorp"

            [cloud.providers.namespace.runner_overrides]
            linux-x64 = "namespace-profile-generouscorp"
            macos-arm64 = "namespace-profile-generouscorp-macos"

            [targets.windows]
            backend = "cloud"
            platform = "windows-x64"
            workflow = "ci.yml"

            [targets.windows.runner_overrides]
            windows-x64 = "namespace-profile-generouscorp-windows"
            "#,
        );
        let targets = resolve_targets_from_table(&cloud, ValidationMode::Full).expect("targets");
        let windows = &targets[0];

        assert_eq!(windows.backend_name, "cloud");
        assert_eq!(windows.host, None);
        assert_eq!(windows.workdir(), None);
        assert!(matches!(&windows.validation, ResolvedValidation::Cloud));
        let ResolvedBackend::Cloud(target) = &windows.backend else {
            panic!("cloud backend");
        };
        assert_eq!(target.workflow, "ci.yml");
        assert_eq!(target.repository.as_deref(), Some("owner/repo"));
        assert_eq!(target.runner_provider.as_deref(), Some("namespace"));
        assert_eq!(
            target.runner_selector.as_deref(),
            Some("namespace-profile-generouscorp")
        );
        assert_eq!(
            target.runner_overrides["linux-x64"],
            "namespace-profile-generouscorp"
        );
        assert_eq!(
            target.runner_overrides["windows-x64"],
            "namespace-profile-generouscorp-windows"
        );
        assert_eq!(target.poll_interval_secs, 1);
        assert_eq!(target.dispatch_settle_secs, 2);
        assert_eq!(target.max_poll_secs, 3);
    }

    #[test]
    fn with_workdir_overrides_backend_specific_path() {
        let config = table(
            r#"
            [targets.windows]
            backend = "ssh-windows"
            repo_path = "C:\\repo"
            "#,
        );
        let target = resolve_targets_from_table(&config, ValidationMode::Full)
            .expect("targets")
            .remove(0)
            .with_workdir(r"D:\warm");

        assert_eq!(target.workdir(), Some(r"D:\warm".to_owned()));
    }

    fn resolved_local_target(name: &str) -> super::ResolvedTarget {
        resolve_targets_from_table(
            &table(&format!(
                r#"
                [targets.{name}]
                backend = "local"
                platform = "linux-x64"
                "#
            )),
            ValidationMode::Full,
        )
        .expect("target")
        .remove(0)
    }
}
