//! Bounded, cached reads of GitHub Actions job logs.
//!
//! `GET repos/{repo}/actions/jobs/{id}/logs` is one REST request per job, and
//! the passes that read logs (pr-watch failure signatures above all) walk
//! every failed job in a multi-day window. Unbounded, one such pass is
//! hundreds of requests on the App installation's shared hourly budget, and a
//! pass that repeats it every fifteen minutes can exhaust that budget for
//! every other client on the fleet.
//!
//! Three guards keep a log read cheap and bounded:
//!
//! 1. **A disk cache keyed by job id.** A completed job's log never changes,
//!    so a log read once is served from disk afterwards. The cache has a total
//!    size bound and evicts least-recently-used entries past it. A job whose
//!    log is permanently gone (HTTP 404 or 410) is remembered for
//!    [`JobLogPolicy::missing_ttl`] so it is not re-asked every pass.
//! 2. **A per-pass fetch budget.** One [`JobLogPass`] sends at most
//!    [`JobLogPolicy::max_fetches_per_pass`] requests. Reads past it fail fast
//!    with a [`DEFERRED`] error the caller records as a gap. Because every log
//!    that was fetched is cached, the next pass serves those from disk and
//!    spends its budget on the next ones: a backlog is drained a bounded slice
//!    at a time and resumes where the previous pass stopped.
//! 3. **A per-minute ceiling for the whole process**, shared by every pass in
//!    it, so a long-lived daemon cannot exceed
//!    [`JobLogPolicy::max_fetches_per_minute`] however many passes overlap. A
//!    read past it waits up to [`JobLogPolicy::max_wait`] for a slot, so the
//!    ceiling paces a pass rather than cutting it short.
//!
//! Only callers that read logs of completed jobs may use the cache: every
//! caller here reads the log of a failed (so completed) job.
//!
//! Writes go into Shipyard's protected state tree and so hold the writer-domain
//! lease, with a short bound. A write that cannot take the lease is skipped on
//! its own; unlike a whole-run latch, it does not stop later writes, because a
//! skipped cache write is exactly what turns the next pass into a refetch.

use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::LoadedConfig;
use crate::writer_domain_lease::ProductionWriterDomainLease;

/// Prefix of the error a read returns when a budget, not GitHub, refused it.
pub const DEFERRED: &str = "job-log fetch deferred";

/// Config table for the policy.
const TABLE: &str = "github.job_logs";
/// First line of a cached log, followed by the cache key.
const LOG_MAGIC: &str = "shipyard-job-log/v1";
/// First line of a remembered permanent miss.
const MISSING_MAGIC: &str = "shipyard-job-log-missing/v1";
/// Bound on taking the writer-domain lease for one cache write.
const LEASE_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest a 404 is remembered (see [`JobLogPolicy::missing_ttl`]).
pub const NOT_FOUND_TTL: Duration = Duration::from_hours(1);

/// Whether `error` came from a budget rather than from GitHub.
#[must_use]
pub fn is_deferred(error: &str) -> bool {
    error.starts_with(DEFERRED)
}

/// Limits for job-log reads, from `[github.job_logs]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JobLogPolicy {
    /// Requests one pass may send. `0` disables log fetching (cache only).
    pub max_fetches_per_pass: usize,
    /// Requests the whole process may send in any sixty-second window.
    pub max_fetches_per_minute: usize,
    /// Total bytes the disk cache may hold before evicting.
    pub cache_max_bytes: u64,
    /// A log larger than this is returned but not cached.
    pub max_entry_bytes: u64,
    /// How long a permanent miss (HTTP 410) is remembered. A 404 is
    /// remembered for at most [`NOT_FOUND_TTL`]: GitHub answers 404 for a
    /// short while after a job completes, before its log is uploaded.
    pub missing_ttl: Duration,
    /// How long a read may wait for a per-minute slot before it is deferred.
    pub max_wait: Duration,
    /// Read Generous-Corp logs with the `[github.admission_auth]` credential.
    pub admission_auth: bool,
}

impl Default for JobLogPolicy {
    fn default() -> Self {
        Self {
            max_fetches_per_pass: 100,
            max_fetches_per_minute: 30,
            cache_max_bytes: 512 * 1024 * 1024,
            max_entry_bytes: 16 * 1024 * 1024,
            missing_ttl: Duration::from_hours(24),
            max_wait: Duration::from_secs(90),
            admission_auth: false,
        }
    }
}

impl JobLogPolicy {
    /// No budgets: every read that misses the cache is sent. For replaying
    /// recorded fixtures and for recording them, where a deferral would drop
    /// an answer, and for callers that bound their reads themselves.
    #[must_use]
    pub fn unbounded() -> Self {
        Self {
            max_fetches_per_pass: usize::MAX,
            max_fetches_per_minute: usize::MAX,
            max_wait: Duration::ZERO,
            ..Self::default()
        }
    }

    /// The policy from machine-global config. Values of the wrong type or out
    /// of range keep their default and are reported as warnings.
    #[must_use]
    pub fn from_config(config: &LoadedConfig) -> (Self, Vec<String>) {
        let mut policy = Self::default();
        let mut warnings = Vec::new();
        let mut count = |key: &str, slot: &mut u64| {
            let dotted = format!("{TABLE}.{key}");
            match config.get(&dotted) {
                None => {}
                Some(value) => match value.as_integer().and_then(|v| u64::try_from(v).ok()) {
                    Some(parsed) => *slot = parsed,
                    None => warnings.push(format!(
                        "[{TABLE}] {key} must be a non-negative integer; using {slot}"
                    )),
                },
            }
        };
        let mut per_pass = policy.max_fetches_per_pass as u64;
        let mut per_minute = policy.max_fetches_per_minute as u64;
        let mut cache_mb = policy.cache_max_bytes / (1024 * 1024);
        let mut entry_mb = policy.max_entry_bytes / (1024 * 1024);
        let mut missing_hours = policy.missing_ttl.as_secs() / 3600;
        let mut wait_secs = policy.max_wait.as_secs();
        count("max_fetches_per_pass", &mut per_pass);
        count("max_fetches_per_minute", &mut per_minute);
        count("cache_max_mb", &mut cache_mb);
        count("max_entry_mb", &mut entry_mb);
        count("missing_ttl_hours", &mut missing_hours);
        count("max_wait_seconds", &mut wait_secs);
        let defaults = Self::default();
        if cache_mb == 0 {
            warnings.push(format!(
                "[{TABLE}] cache_max_mb = 0 would evict every entry it writes; using {}",
                defaults.cache_max_bytes / (1024 * 1024)
            ));
            cache_mb = defaults.cache_max_bytes / (1024 * 1024);
        }
        if missing_hours.checked_mul(3600).is_none() {
            warnings.push(format!(
                "[{TABLE}] missing_ttl_hours is too large; using 24"
            ));
            missing_hours = 24;
        }
        policy.max_fetches_per_pass = usize::try_from(per_pass).unwrap_or(usize::MAX);
        policy.max_fetches_per_minute = usize::try_from(per_minute).unwrap_or(usize::MAX);
        policy.cache_max_bytes = cache_mb.saturating_mul(1024 * 1024);
        policy.max_entry_bytes = entry_mb.saturating_mul(1024 * 1024);
        policy.missing_ttl = Duration::from_secs(missing_hours * 3600);
        policy.max_wait = Duration::from_secs(wait_secs);
        match config.get(&format!("{TABLE}.admission_auth")) {
            None => {}
            Some(value) => match value.as_bool() {
                Some(flag) => policy.admission_auth = flag,
                None => warnings.push(format!(
                    "[{TABLE}] admission_auth must be a boolean; using false"
                )),
            },
        }
        (policy, warnings)
    }
}

/// A sliding sixty-second window of request times.
#[derive(Debug, Default)]
pub struct RateWindow {
    stamps: Mutex<VecDeque<Instant>>,
}

impl RateWindow {
    /// The window every pass in this process shares.
    #[must_use]
    pub fn process() -> &'static Self {
        static WINDOW: OnceLock<RateWindow> = OnceLock::new();
        WINDOW.get_or_init(Self::default)
    }

    /// Take a slot, waiting up to `max_wait` for one to free.
    pub fn take(&self, limit: usize, max_wait: Duration) -> bool {
        let deadline = Instant::now() + max_wait;
        loop {
            let now = Instant::now();
            if self.try_take(now, limit) {
                return true;
            }
            if now >= deadline {
                return false;
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(250)),
            );
        }
    }

    /// Take one slot at `now` when fewer than `limit` were taken in the
    /// sixty seconds before it.
    pub fn try_take(&self, now: Instant, limit: usize) -> bool {
        let mut stamps = self
            .stamps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while stamps
            .front()
            .is_some_and(|first| now.saturating_duration_since(*first) >= Duration::from_mins(1))
        {
            stamps.pop_front();
        }
        if stamps.len() >= limit {
            return false;
        }
        stamps.push_back(now);
        true
    }
}

/// Takes the writer-domain lease for a write under `path`.
type LeaseFn = fn(&Path) -> io::Result<Option<ProductionWriterDomainLease>>;

fn production_lease(path: &Path) -> io::Result<Option<ProductionWriterDomainLease>> {
    crate::writer_domain_lease::acquire_for_protected_path_with_timeout(path, LEASE_TIMEOUT)
}

/// What the cache holds for one job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cached {
    /// The job's log.
    Log(String),
    /// GitHub said the log is gone; the reason it gave.
    Missing(String),
}

/// Disk cache of completed jobs' logs, one file per job.
#[derive(Debug)]
pub struct JobLogStore {
    dir: PathBuf,
    max_bytes: u64,
    max_entry_bytes: u64,
    missing_ttl: Duration,
    lease: LeaseFn,
    /// Bytes the directory holds, kept current across writes and evictions.
    bytes: AtomicU64,
    evicting: Mutex<()>,
}

impl JobLogStore {
    /// Open (creating if needed) the cache at `dir`. `None` when the directory
    /// cannot be created: the cache is an optimisation and never fails a read.
    #[must_use]
    pub fn open(dir: &Path, policy: &JobLogPolicy) -> Option<Self> {
        Self::open_with_lease(dir, policy, production_lease)
    }

    fn open_with_lease(dir: &Path, policy: &JobLogPolicy, lease: LeaseFn) -> Option<Self> {
        if !dir.is_dir() {
            let _lease = lease(dir).ok()?;
            fs::create_dir_all(dir).ok()?;
        }
        let store = Self {
            dir: dir.to_path_buf(),
            max_bytes: policy.cache_max_bytes,
            max_entry_bytes: policy.max_entry_bytes,
            missing_ttl: policy.missing_ttl,
            lease,
            bytes: AtomicU64::new(0),
            evicting: Mutex::new(()),
        };
        let total = store.entries().iter().map(|entry| entry.1).sum();
        store.bytes.store(total, Ordering::Relaxed);
        Some(store)
    }

    fn key(repo: &str, job_id: u64) -> String {
        format!("{}:{job_id}", repo.to_ascii_lowercase())
    }

    fn path(&self, key: &str) -> PathBuf {
        let digest = hex::encode(Sha256::digest(key.as_bytes()));
        self.dir.join(format!("{digest}.log"))
    }

    /// The cached answer for one job. A hit refreshes the entry's age, so
    /// eviction drops the least recently used entries first.
    #[must_use]
    pub fn get(&self, repo: &str, job_id: u64) -> Option<Cached> {
        let key = Self::key(repo, job_id);
        let path = self.path(&key);
        let text = fs::read_to_string(&path).ok()?;
        let (header, body) = text.split_once('\n')?;
        let cached = if let Some(rest) = header.strip_prefix(LOG_MAGIC) {
            // The key is stored beside the body so a digest collision or a
            // foreign file is a miss rather than another job's log, and the
            // body's length so a write torn by a crash is a miss rather than
            // a truncated log. (A cache entry is not fsynced: losing one costs
            // a refetch, never a wrong answer.)
            let (stored_key, length) = rest.trim().rsplit_once(' ')?;
            if stored_key != key || length.parse::<usize>().ok()? != body.len() {
                return None;
            }
            Cached::Log(body.to_owned())
        } else {
            let rest = header.strip_prefix(MISSING_MAGIC)?;
            let (rest, ttl) = rest.trim().rsplit_once(' ')?;
            let (stored_key, at) = rest.rsplit_once(' ')?;
            if stored_key != key {
                return None;
            }
            let at = at.parse::<u64>().ok()?;
            let ttl = ttl.parse::<u64>().ok()?.min(self.missing_ttl.as_secs());
            let age = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()?
                .as_secs()
                .saturating_sub(at);
            if age >= ttl {
                return None;
            }
            Cached::Missing(body.to_owned())
        };
        // Refreshing the age is a write into the protected tree; without the
        // lease the entry keeps its age and is evicted a little sooner.
        if let Ok(_lease) = (self.lease)(&self.dir)
            && let Ok(file) = fs::File::options().write(true).open(&path)
        {
            let _ = file.set_modified(SystemTime::now());
        }
        Some(cached)
    }

    /// Store a completed job's log.
    pub fn put_log(&self, repo: &str, job_id: u64, log: &str) {
        if log.len() as u64 > self.max_entry_bytes {
            return;
        }
        let key = Self::key(repo, job_id);
        self.write(&key, &format!("{LOG_MAGIC} {key} {}\n{log}", log.len()));
    }

    /// Remember for `ttl` (capped by the policy) that GitHub has no log for
    /// this job.
    pub fn put_missing(&self, repo: &str, job_id: u64, reason: &str, ttl: Duration) {
        let key = Self::key(repo, job_id);
        let at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let ttl = ttl.as_secs();
        self.write(&key, &format!("{MISSING_MAGIC} {key} {at} {ttl}\n{reason}"));
    }

    fn write(&self, key: &str, body: &str) {
        let path = self.path(key);
        let total = {
            let Ok(_lease) = (self.lease)(&self.dir) else {
                return;
            };
            let previous = fs::metadata(&path).map_or(0, |metadata| metadata.len());
            let Ok(mut file) = tempfile::NamedTempFile::new_in(&self.dir) else {
                return;
            };
            if file.write_all(body.as_bytes()).is_err() || file.persist(&path).is_err() {
                return;
            }
            let written = body.len() as u64;
            let mut current = self.bytes.load(Ordering::Relaxed);
            loop {
                let next = current.saturating_add(written).saturating_sub(previous);
                match self.bytes.compare_exchange_weak(
                    current,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break next,
                    Err(actual) => current = actual,
                }
            }
        };
        if total > self.max_bytes {
            self.evict();
        }
    }

    /// Every entry with its size and age, oldest first.
    fn entries(&self) -> Vec<(PathBuf, u64, SystemTime)> {
        let Ok(read) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut entries: Vec<(PathBuf, u64, SystemTime)> = read
            .flatten()
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("log"))
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                Some((
                    entry.path(),
                    metadata.len(),
                    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                ))
            })
            .collect();
        entries.sort_by_key(|entry| entry.2);
        entries
    }

    /// Remove least recently used entries until the cache fits.
    fn evict(&self) {
        let Ok(_one_evictor) = self.evicting.try_lock() else {
            return;
        };
        let Ok(_lease) = (self.lease)(&self.dir) else {
            return;
        };
        let entries = self.entries();
        let mut total: u64 = entries.iter().map(|entry| entry.1).sum();
        for (path, size, _) in entries {
            if total <= self.max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
        self.bytes.store(total, Ordering::Relaxed);
    }

    /// Bytes the cache holds.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// Where one pass's log reads came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct JobLogStats {
    /// Requests sent to GitHub, retries and fallbacks included.
    pub fetched: u64,
    /// Answers served from the disk cache.
    pub cached: u64,
    /// Reads a budget refused.
    pub deferred: u64,
}

/// An alternative reader for log requests (a dedicated credential).
pub type LogReader = Box<dyn Fn(&[String]) -> Result<String, String> + Send + Sync>;

/// The caller's reader. It need not be `Sync`: a read runs on the caller's
/// thread.
pub type Reader<'a> = dyn Fn(&[String]) -> Result<String, String> + 'a;

/// The log reads of one pass: the cache, the pass budget, and the process
/// per-minute ceiling.
pub struct JobLogPass {
    store: Option<JobLogStore>,
    policy: JobLogPolicy,
    window: &'static RateWindow,
    reader: Option<LogReader>,
    fetched: AtomicUsize,
    requests: AtomicU64,
    cached: AtomicU64,
    deferred: AtomicU64,
}

impl fmt::Debug for JobLogPass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JobLogPass")
            .field("store", &self.store)
            .field("policy", &self.policy)
            .field("dedicated_reader", &self.reader.is_some())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl Default for JobLogPass {
    /// No disk cache and no budgets: what a caller that has not configured
    /// job-log reads got before they were budgeted. Passes that walk many
    /// logs install a budgeted pass with [`ReadCache::with_job_logs`].
    ///
    /// [`ReadCache::with_job_logs`]: crate::gate_cost::ReadCache::with_job_logs
    fn default() -> Self {
        Self::new(None, JobLogPolicy::unbounded())
    }
}

impl JobLogPass {
    /// A pass over `store` (or no cache) with `policy`, sharing the process
    /// per-minute window.
    #[must_use]
    pub fn new(store: Option<JobLogStore>, policy: JobLogPolicy) -> Self {
        Self::with_window(store, policy, RateWindow::process())
    }

    /// A pass with its own per-minute window (tests).
    #[must_use]
    pub fn with_window(
        store: Option<JobLogStore>,
        policy: JobLogPolicy,
        window: &'static RateWindow,
    ) -> Self {
        Self {
            store,
            policy,
            window,
            reader: None,
            fetched: AtomicUsize::new(0),
            requests: AtomicU64::new(0),
            cached: AtomicU64::new(0),
            deferred: AtomicU64::new(0),
        }
    }

    /// Send log requests through `reader` instead of the caller's reader.
    #[must_use]
    pub fn with_reader(mut self, reader: LogReader) -> Self {
        self.reader = Some(reader);
        self
    }

    /// The limits this pass applies.
    #[must_use]
    pub fn policy(&self) -> &JobLogPolicy {
        &self.policy
    }

    /// Counts so far.
    #[must_use]
    pub fn stats(&self) -> JobLogStats {
        JobLogStats {
            fetched: self.requests.load(Ordering::Relaxed),
            cached: self.cached.load(Ordering::Relaxed),
            deferred: self.deferred.load(Ordering::Relaxed),
        }
    }

    /// Claim one request from the pass budget and the per-minute window.
    fn admit(&self) -> Result<(), String> {
        let claimed = self.fetched.fetch_add(1, Ordering::Relaxed);
        if claimed >= self.policy.max_fetches_per_pass {
            self.fetched.fetch_sub(1, Ordering::Relaxed);
            self.deferred.fetch_add(1, Ordering::Relaxed);
            return Err(format!(
                "{DEFERRED}: this pass already sent its {} log requests ([github.job_logs] max_fetches_per_pass)",
                self.policy.max_fetches_per_pass
            ));
        }
        if self.policy.max_fetches_per_minute != usize::MAX
            && !self
                .window
                .take(self.policy.max_fetches_per_minute, self.policy.max_wait)
        {
            self.fetched.fetch_sub(1, Ordering::Relaxed);
            self.deferred.fetch_add(1, Ordering::Relaxed);
            return Err(format!(
                "{DEFERRED}: {} log requests already sent in the last minute and no slot freed within {}s ([github.job_logs] max_fetches_per_minute)",
                self.policy.max_fetches_per_minute,
                self.policy.max_wait.as_secs()
            ));
        }
        Ok(())
    }

    /// The pass for a process using Shipyard's state directory: logs cached
    /// under `<state_dir>/job-logs`, budgets from `[github.job_logs]`.
    /// Returns config warnings beside it.
    #[must_use]
    pub fn for_state_dir(state_dir: &Path, config: &LoadedConfig) -> (Self, Vec<String>) {
        let (policy, mut warnings) = JobLogPolicy::from_config(config);
        let dir = state_dir.join("job-logs");
        let store = JobLogStore::open(&dir, &policy);
        if store.is_none() {
            warnings.push(format!(
                "job-log cache {} could not be opened; logs are read uncached",
                dir.display()
            ));
        }
        (Self::new(store, policy), warnings)
    }

    /// The cached answer for one job, without sending a request.
    #[must_use]
    pub fn cached(&self, repo: &str, job_id: u64) -> Option<Result<String, String>> {
        let answer = match self.store.as_ref()?.get(repo, job_id)? {
            Cached::Log(log) => Ok(log),
            Cached::Missing(reason) => Err(reason),
        };
        self.cached.fetch_add(1, Ordering::Relaxed);
        Some(answer)
    }

    /// Requests this pass may still send.
    #[must_use]
    pub fn fetches_left(&self) -> usize {
        self.policy
            .max_fetches_per_pass
            .saturating_sub(self.fetched.load(Ordering::Relaxed))
    }

    /// Count `count` reads the caller deferred itself, past [`Self::fetches_left`].
    pub fn note_deferred(&self, count: usize) {
        self.deferred.fetch_add(count as u64, Ordering::Relaxed);
    }

    /// The log of completed job `job_id` in `repo`: from the cache when it is
    /// there, otherwise one request (when a budget allows) whose answer is
    /// cached. A permanent miss is cached as a miss.
    ///
    /// # Errors
    /// The request failed, or a budget deferred it ([`is_deferred`]).
    pub fn read(&self, gh: &Reader<'_>, repo: &str, job_id: u64) -> Result<String, String> {
        if let Some(answer) = self.cached(repo, job_id) {
            return answer;
        }
        self.admit()?;
        let counted = |argv: &[String]| {
            self.requests.fetch_add(1, Ordering::Relaxed);
            gh(argv)
        };
        let result = match &self.reader {
            Some(reader) => {
                let dedicated = |argv: &[String]| {
                    self.requests.fetch_add(1, Ordering::Relaxed);
                    reader(argv)
                };
                match crate::gate_cost::fetch_job_log(&dedicated, repo, job_id) {
                    // The dedicated credential cannot read this log: the
                    // caller's reader still can.
                    Err(error) if credential_refused(&error) => {
                        crate::gate_cost::fetch_job_log(&counted, repo, job_id)
                    }
                    other => other,
                }
            }
            None => crate::gate_cost::fetch_job_log(&counted, repo, job_id),
        };
        if let Some(store) = &self.store {
            match &result {
                Ok(log) => store.put_log(repo, job_id, log),
                Err(error) if error.contains("HTTP 410") => {
                    store.put_missing(repo, job_id, error, self.policy.missing_ttl);
                }
                Err(error) if error.contains("HTTP 404") => {
                    store.put_missing(repo, job_id, error, NOT_FOUND_TTL);
                }
                Err(_) => {}
            }
        }
        result
    }
}

/// The dedicated credential was refused (not allowed, not authenticated, or
/// out of budget), so the request is worth one retry on the caller's reader.
fn credential_refused(error: &str) -> bool {
    error.contains("HTTP 401")
        || error.contains("HTTP 429")
        || (error.contains("HTTP 403")
            && (error.contains("not accessible by integration") || error.contains("rate limit")))
}

#[cfg(test)]
mod tests;
