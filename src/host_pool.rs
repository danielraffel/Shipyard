//! Local host-pool configuration and lease state.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use toml::{Table, Value as TomlValue};

/// Default host-pool lease staleness window.
pub const DEFAULT_LEASE_STALE_SECONDS: u64 = 180;
/// Default host-pool heartbeat interval.
pub const DEFAULT_HEARTBEAT_INTERVAL_SECONDS: u64 = 15;

/// Parsed host-pool config.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostPoolConfig {
    /// Pool name from `[host_pools.<name>]`.
    pub name: String,
    /// Member selection strategy. Only `ordered` has semantics in the first slice.
    pub strategy: String,
    /// Seconds after which a lease is stale.
    pub lease_stale_seconds: u64,
    /// Suggested heartbeat interval for active leases.
    pub heartbeat_interval_seconds: u64,
    /// Concrete pool members.
    pub members: Vec<HostPoolMemberConfig>,
}

/// One configured host-pool member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostPoolMemberConfig {
    /// Stable member id.
    pub id: String,
    /// Backend type, currently `ssh` or `local`.
    pub backend_type: String,
    /// SSH host for `ssh` members.
    pub host: Option<String>,
    /// Remote repo path for `ssh` members.
    pub repo_path: Option<String>,
    /// Local checkout path for `local` members.
    pub cwd: Option<PathBuf>,
    /// Max concurrent leases. Phase 2a still runs one active queue job.
    pub max_concurrency: u32,
    /// Member capabilities.
    pub capabilities: Vec<String>,
}

/// Host-pool config parse error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostPoolConfigError {
    message: String,
}

impl HostPoolConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for HostPoolConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid host pool config: {}", self.message)
    }
}

impl Error for HostPoolConfigError {}

/// Parse `[host_pools]` from merged config.
pub fn parse_host_pools(data: &Table) -> Result<Vec<HostPoolConfig>, HostPoolConfigError> {
    let Some(pools) = data.get("host_pools").and_then(TomlValue::as_table) else {
        return Ok(Vec::new());
    };
    pools
        .iter()
        .map(|(name, value)| {
            let table = value.as_table().ok_or_else(|| {
                HostPoolConfigError::new(format!("host_pools.{name} must be a table"))
            })?;
            parse_host_pool(name, table)
        })
        .collect()
}

fn parse_host_pool(name: &str, table: &Table) -> Result<HostPoolConfig, HostPoolConfigError> {
    let strategy = string_value(table, "strategy")
        .unwrap_or("ordered")
        .to_owned();
    if strategy != "ordered" {
        return Err(HostPoolConfigError::new(format!(
            "host_pools.{name}.strategy {strategy:?} is unsupported; expected \"ordered\""
        )));
    }
    let members = table
        .get("members")
        .and_then(TomlValue::as_array)
        .ok_or_else(|| {
            HostPoolConfigError::new(format!("host_pools.{name}.members must be an array"))
        })?;
    let members = members
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let member = value.as_table().ok_or_else(|| {
                HostPoolConfigError::new(format!(
                    "host_pools.{name}.members[{index}] must be a table"
                ))
            })?;
            parse_member(name, index, member)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if members.is_empty() {
        return Err(HostPoolConfigError::new(format!(
            "host_pools.{name}.members must not be empty"
        )));
    }
    Ok(HostPoolConfig {
        name: name.to_owned(),
        strategy,
        lease_stale_seconds: u64_value(table, "lease_stale_seconds")
            .unwrap_or(DEFAULT_LEASE_STALE_SECONDS),
        heartbeat_interval_seconds: u64_value(table, "heartbeat_interval_seconds")
            .unwrap_or(DEFAULT_HEARTBEAT_INTERVAL_SECONDS),
        members,
    })
}

fn parse_member(
    pool_name: &str,
    index: usize,
    table: &Table,
) -> Result<HostPoolMemberConfig, HostPoolConfigError> {
    let id = string_value(table, "id")
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            HostPoolConfigError::new(format!(
                "host_pools.{pool_name}.members[{index}].id is required"
            ))
        })?
        .to_owned();
    let backend_type = string_value(table, "type")
        .unwrap_or("ssh")
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-");
    if !matches!(backend_type.as_str(), "ssh" | "local") {
        return Err(HostPoolConfigError::new(format!(
            "host_pools.{pool_name}.members[{index}].type {backend_type:?} is unsupported"
        )));
    }
    if backend_type == "ssh" && string_value(table, "host").is_none() {
        return Err(HostPoolConfigError::new(format!(
            "host_pools.{pool_name}.members[{index}].host is required for ssh members"
        )));
    }
    Ok(HostPoolMemberConfig {
        id,
        backend_type,
        host: string_value(table, "host").map(ToOwned::to_owned),
        repo_path: string_value(table, "repo_path").map(ToOwned::to_owned),
        cwd: string_value(table, "cwd").map(PathBuf::from),
        max_concurrency: u32_value(table, "max_concurrency").unwrap_or(1).max(1),
        capabilities: string_array(table, "capabilities"),
    })
}

/// One active or stale host-pool lease.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostPoolLease {
    /// Stable lease id.
    pub lease_id: String,
    /// Pool name.
    pub pool_name: String,
    /// Member id.
    pub member_id: String,
    /// Logical target name.
    pub target_name: String,
    /// Concrete backend label.
    pub backend: String,
    /// Concrete host when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Queue job id when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// Branch under validation.
    pub branch: String,
    /// SHA under validation.
    pub sha: String,
    /// Owning process id.
    pub owner_pid: u32,
    /// Acquisition timestamp.
    pub acquired_at: DateTime<Utc>,
    /// Last heartbeat timestamp.
    pub heartbeat_at: DateTime<Utc>,
    /// Stale cutoff timestamp.
    pub expires_at: DateTime<Utc>,
}

impl HostPoolLease {
    /// Return true when the lease is stale at `now`.
    #[must_use]
    pub fn is_stale(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }
}

/// Request to acquire one host-pool lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostPoolLeaseRequest {
    /// Pool name.
    pub pool_name: String,
    /// Member id.
    pub member_id: String,
    /// Logical target name.
    pub target_name: String,
    /// Concrete backend label.
    pub backend: String,
    /// Concrete host when available.
    pub host: Option<String>,
    /// Queue job id when available.
    pub job_id: Option<String>,
    /// Branch under validation.
    pub branch: String,
    /// SHA under validation.
    pub sha: String,
    /// Max concurrent leases for this member.
    pub max_concurrency: u32,
    /// Lease stale window.
    pub lease_stale_seconds: u64,
}

/// JSON-backed host-pool lease store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostPoolLeaseStore {
    path: PathBuf,
}

impl HostPoolLeaseStore {
    /// Create a lease store rooted at a concrete JSON path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Return the backing JSON path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return every recorded lease, including stale leases.
    pub fn leases(&self) -> HostPoolLeaseResult<Vec<HostPoolLease>> {
        self.with_lock(|_| self.read_leases())
    }

    /// Acquire a lease if member capacity is available.
    pub fn acquire(
        &self,
        request: &HostPoolLeaseRequest,
    ) -> HostPoolLeaseResult<Option<HostPoolLease>> {
        self.with_lock(|_| {
            let now = Utc::now();
            let mut leases = self.read_leases()?;
            leases.retain(|lease| !lease.is_stale(now));
            let active = leases
                .iter()
                .filter(|lease| {
                    lease.pool_name == request.pool_name && lease.member_id == request.member_id
                })
                .count();
            if active >= request.max_concurrency as usize {
                self.save_leases(&leases)?;
                return Ok(None);
            }
            let lease = HostPoolLease {
                lease_id: new_lease_id(now),
                pool_name: request.pool_name.clone(),
                member_id: request.member_id.clone(),
                target_name: request.target_name.clone(),
                backend: request.backend.clone(),
                host: request.host.clone(),
                job_id: request.job_id.clone(),
                branch: request.branch.clone(),
                sha: request.sha.clone(),
                owner_pid: process::id(),
                acquired_at: now,
                heartbeat_at: now,
                expires_at: now
                    + chrono::Duration::seconds(
                        i64::try_from(request.lease_stale_seconds).unwrap_or(i64::MAX),
                    ),
            };
            leases.push(lease.clone());
            self.save_leases(&leases)?;
            Ok(Some(lease))
        })
    }

    /// Refresh one lease heartbeat.
    pub fn heartbeat(&self, lease_id: &str, lease_stale_seconds: u64) -> HostPoolLeaseResult<bool> {
        self.with_lock(|_| {
            let now = Utc::now();
            let mut leases = self.read_leases()?;
            let mut updated = false;
            for lease in &mut leases {
                if lease.lease_id == lease_id {
                    lease.heartbeat_at = now;
                    lease.expires_at = now
                        + chrono::Duration::seconds(
                            i64::try_from(lease_stale_seconds).unwrap_or(i64::MAX),
                        );
                    updated = true;
                }
            }
            if updated {
                self.save_leases(&leases)?;
            }
            Ok(updated)
        })
    }

    /// Release one lease by id.
    pub fn release(&self, lease_id: &str) -> HostPoolLeaseResult<bool> {
        self.with_lock(|_| {
            let leases = self.read_leases()?;
            let original_len = leases.len();
            let leases = leases
                .into_iter()
                .filter(|lease| lease.lease_id != lease_id)
                .collect::<Vec<_>>();
            let removed = leases.len() != original_len;
            if removed {
                self.save_leases(&leases)?;
            }
            Ok(removed)
        })
    }

    /// Release every lease owned by one queue job.
    ///
    /// A daemon supervisor uses this after proving that the worker's complete
    /// process tree is dead. The operation is idempotent so restart recovery
    /// can safely repeat it before acknowledging terminal queue state.
    pub(crate) fn release_for_job(&self, job_id: &str) -> HostPoolLeaseResult<usize> {
        self.with_lock(|_| {
            let leases = self.read_leases()?;
            let original_len = leases.len();
            let leases = leases
                .into_iter()
                .filter(|lease| lease.job_id.as_deref() != Some(job_id))
                .collect::<Vec<_>>();
            let removed = original_len - leases.len();
            if removed > 0 {
                self.save_leases(&leases)?;
            }
            Ok(removed)
        })
    }

    /// Drop stale leases and return the count removed.
    pub fn prune_stale(&self, now: DateTime<Utc>) -> HostPoolLeaseResult<usize> {
        self.with_lock(|_| {
            let leases = self.read_leases()?;
            let original_len = leases.len();
            let leases = leases
                .into_iter()
                .filter(|lease| !lease.is_stale(now))
                .collect::<Vec<_>>();
            let removed = original_len - leases.len();
            if removed > 0 {
                self.save_leases(&leases)?;
            }
            Ok(removed)
        })
    }

    fn with_lock<T>(
        &self,
        f: impl FnOnce(&File) -> HostPoolLeaseResult<T>,
    ) -> HostPoolLeaseResult<T> {
        let lock_path = lock_path_for(&self.path);
        let writer_domain = crate::writer_domain_lease::acquire_for_protected_creation(&lock_path)?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)?;
        drop(writer_domain);
        lock.lock_exclusive()?;
        let result = f(&lock);
        let unlock_result = lock.unlock();
        match (result, unlock_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
        }
    }

    fn read_leases(&self) -> HostPoolLeaseResult<Vec<HostPoolLease>> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let payload: LeasePayload = serde_json::from_str(&raw)?;
        Ok(payload.leases)
    }

    fn save_leases(&self, leases: &[HostPoolLease]) -> HostPoolLeaseResult<()> {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&self.path)?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = serde_json::to_string_pretty(&LeasePayload {
            leases: leases.to_vec(),
        })? + "\n";
        let tmp = self.path.with_extension("json.tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(payload.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
}

/// Host-pool lease store result.
pub type HostPoolLeaseResult<T> = Result<T, HostPoolLeaseError>;

/// Host-pool lease store error.
#[derive(Debug)]
pub enum HostPoolLeaseError {
    /// Filesystem error.
    Io(io::Error),
    /// JSON error.
    Json(serde_json::Error),
}

impl Display for HostPoolLeaseError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "host-pool lease I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "host-pool lease JSON failed: {error}"),
        }
    }
}

impl Error for HostPoolLeaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
        }
    }
}

impl From<io::Error> for HostPoolLeaseError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for HostPoolLeaseError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct LeasePayload {
    #[serde(default)]
    leases: Vec<HostPoolLease>,
}

/// Canonical host-pool lease path.
#[must_use]
pub fn default_lease_path(state_dir: &Path) -> PathBuf {
    state_dir.join("host_pool").join("leases.json")
}

fn lock_path_for(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

fn new_lease_id(now: DateTime<Utc>) -> String {
    format!(
        "{}-{}",
        process::id(),
        now.timestamp_nanos_opt().unwrap_or_else(|| now.timestamp())
    )
}

fn string_value<'a>(table: &'a Table, key: &str) -> Option<&'a str> {
    table
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn u64_value(table: &Table, key: &str) -> Option<u64> {
    table
        .get(key)
        .and_then(TomlValue::as_integer)
        .and_then(|value| u64::try_from(value).ok())
}

fn u32_value(table: &Table, key: &str) -> Option<u32> {
    u64_value(table, key).and_then(|value| u32::try_from(value).ok())
}

fn string_array(table: &Table, key: &str) -> Vec<String> {
    table
        .get(key)
        .and_then(TomlValue::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(TomlValue::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use tempfile::TempDir;
    use toml::Table;

    use super::{HostPoolLeaseRequest, HostPoolLeaseStore, default_lease_path, parse_host_pools};

    fn table(input: &str) -> Table {
        input.parse::<Table>().expect("toml")
    }

    #[test]
    fn parses_host_pool_config() {
        let config = table(
            r#"
            [host_pools.local_macs]
            strategy = "ordered"
            lease_stale_seconds = 120

            [[host_pools.local_macs.members]]
            id = "mac-studio"
            type = "ssh"
            host = "mac-studio"
            repo_path = "/Users/shipyard/work/shipyard"
            max_concurrency = 1
            capabilities = ["macos", "arm64"]

            [[host_pools.local_macs.members]]
            id = "local"
            type = "local"
            cwd = "/repo"
            capabilities = ["macos", "arm64"]
            "#,
        );

        let pools = parse_host_pools(&config).expect("pools");

        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].name, "local_macs");
        assert_eq!(pools[0].lease_stale_seconds, 120);
        assert_eq!(pools[0].members[0].host.as_deref(), Some("mac-studio"));
        assert_eq!(pools[0].members[1].backend_type, "local");
    }

    #[test]
    fn rejects_ssh_member_without_host() {
        let config = table(
            r#"
            [host_pools.local_macs]
            [[host_pools.local_macs.members]]
            id = "mac-studio"
            type = "ssh"
            "#,
        );

        let error = parse_host_pools(&config).expect_err("invalid");

        assert!(error.to_string().contains("host is required"));
    }

    #[test]
    fn lease_store_respects_capacity_and_release() {
        let temp = TempDir::new().expect("tempdir");
        let store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        let request = lease_request("mac-studio");

        let lease = store.acquire(&request).expect("acquire").expect("lease");
        assert!(store.acquire(&request).expect("busy").is_none());
        assert!(store.release(&lease.lease_id).expect("release"));
        assert!(store.acquire(&request).expect("second").is_some());
    }

    #[test]
    fn lease_store_prunes_stale_leases() {
        let temp = TempDir::new().expect("tempdir");
        let store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        let mut request = lease_request("mac-studio");
        request.lease_stale_seconds = 1;
        let lease = store.acquire(&request).expect("acquire").expect("lease");

        let removed = store
            .prune_stale(lease.expires_at + Duration::seconds(1))
            .expect("prune");

        assert_eq!(removed, 1);
        assert!(store.leases().expect("leases").is_empty());
    }

    #[test]
    fn lease_store_releases_all_job_leases_idempotently() {
        let temp = TempDir::new().expect("tempdir");
        let store = HostPoolLeaseStore::new(default_lease_path(temp.path()));
        let job_lease = store
            .acquire(&lease_request("mac-studio"))
            .expect("acquire job lease")
            .expect("job lease");
        let mut other_request = lease_request("other-mac");
        other_request.job_id = Some("job-2".to_owned());
        let other_lease = store
            .acquire(&other_request)
            .expect("acquire other lease")
            .expect("other lease");

        assert_eq!(store.release_for_job("job-1").expect("release"), 1);
        assert_eq!(store.release_for_job("job-1").expect("repeat release"), 0);
        assert_eq!(store.leases().expect("leases"), vec![other_lease]);
        assert_ne!(
            job_lease.lease_id,
            store.leases().expect("leases")[0].lease_id
        );
    }

    fn lease_request(member_id: &str) -> HostPoolLeaseRequest {
        HostPoolLeaseRequest {
            pool_name: "local_macs".to_owned(),
            member_id: member_id.to_owned(),
            target_name: "mac".to_owned(),
            backend: "ssh".to_owned(),
            host: Some(member_id.to_owned()),
            job_id: Some("job-1".to_owned()),
            branch: "main".to_owned(),
            sha: "abc123".to_owned(),
            max_concurrency: 1,
            lease_stale_seconds: 180,
        }
    }
}
