//! Lossless log lifecycle primitives.
//!
//! Phase 1 rotates only before a writer opens (or while an append lock is
//! held). It deliberately does not rename a log behind a long-lived child
//! process: that would leave the child writing to the renamed inode and would
//! only make disk growth less visible.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::LoadedConfig;
use crate::job::{Job, JobStatus};

/// Marker that pins a log directory regardless of age or disk pressure.
pub const AUDIT_PIN_FILE: &str = ".shipyard-retain";
/// Machine-readable terminal classification stored beside target logs.
pub const TERMINAL_MANIFEST_FILE: &str = ".retention.json";

/// Operational defaults are intentionally conservative: ordinary successes
/// are short-lived, while failures and unclassified legacy evidence get a
/// substantially longer investigation window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogRetentionPolicy {
    /// Days to retain successful terminal job logs.
    pub success_days: i64,
    /// Days to retain failure, cancellation, and legacy-unclassified logs.
    pub failure_days: i64,
    /// Hours after terminal completion before gzip compaction is eligible.
    pub compress_after_hours: i64,
    /// Size at which a reopen-safe active file rotates before append.
    pub max_active_file_bytes: u64,
    /// Number of prior reopen segments to preserve.
    pub rotated_segments: usize,
    /// Log-tree size that activates pressure cleanup of successful jobs.
    pub high_watermark_bytes: u64,
    /// Target log-tree size after pressure cleanup.
    pub low_watermark_bytes: u64,
}

impl Default for LogRetentionPolicy {
    fn default() -> Self {
        Self {
            success_days: 7,
            failure_days: 30,
            compress_after_hours: 1,
            max_active_file_bytes: 64 * 1024 * 1024,
            rotated_segments: 4,
            high_watermark_bytes: 1024 * 1024 * 1024,
            low_watermark_bytes: 768 * 1024 * 1024,
        }
    }
}

impl LogRetentionPolicy {
    /// Read `[log_retention]` integer overrides from layered configuration.
    #[must_use]
    pub fn from_config(config: &LoadedConfig) -> Self {
        let defaults = Self::default();
        let mut policy = Self {
            success_days: integer(
                config,
                "log_retention.success_days",
                defaults.success_days,
                1,
                3650,
            ),
            failure_days: integer(
                config,
                "log_retention.failure_days",
                defaults.failure_days,
                1,
                3650,
            ),
            compress_after_hours: integer(
                config,
                "log_retention.compress_after_hours",
                defaults.compress_after_hours,
                0,
                87600,
            ),
            max_active_file_bytes: unsigned(
                config,
                "log_retention.max_active_file_bytes",
                defaults.max_active_file_bytes,
                1024 * 1024,
                u64::MAX,
            ),
            rotated_segments: usize::try_from(unsigned(
                config,
                "log_retention.rotated_segments",
                defaults.rotated_segments as u64,
                1,
                32,
            ))
            .unwrap_or(defaults.rotated_segments),
            high_watermark_bytes: unsigned(
                config,
                "log_retention.high_watermark_bytes",
                defaults.high_watermark_bytes,
                16 * 1024 * 1024,
                u64::MAX,
            ),
            low_watermark_bytes: unsigned(
                config,
                "log_retention.low_watermark_bytes",
                defaults.low_watermark_bytes,
                8 * 1024 * 1024,
                u64::MAX,
            ),
        };
        policy.low_watermark_bytes = policy.low_watermark_bytes.min(policy.high_watermark_bytes);
        policy
    }
}

fn integer(config: &LoadedConfig, key: &str, default: i64, min: i64, max: i64) -> i64 {
    config
        .get(key)
        .and_then(toml::Value::as_integer)
        .unwrap_or(default)
        .clamp(min, max)
}

fn unsigned(config: &LoadedConfig, key: &str, default: u64, min: u64, max: u64) -> u64 {
    config
        .get(key)
        .and_then(toml::Value::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(default)
        .clamp(min, max)
}

/// Durable reason for treating a terminal job as success or protected failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TerminalLogManifest {
    /// Manifest schema version.
    pub schema_version: u32,
    /// Queue job identifier.
    pub job_id: String,
    /// Durable terminal timestamp used for age decisions.
    pub terminal_at: DateTime<Utc>,
    /// Whether this evidence receives failure retention and pressure protection.
    pub failed: bool,
    /// Stable human/debug classification.
    pub reason: String,
}

impl TerminalLogManifest {
    /// Classify a terminal queue job without depending on later queue trimming.
    #[must_use]
    pub fn from_job(job: &Job) -> Self {
        let expected = job.target_names.iter().collect::<BTreeSet<_>>();
        let result_names = job.results.keys().collect::<BTreeSet<_>>();
        let complete_pass = job.status == JobStatus::Completed
            && job.completed_at.is_some()
            && !expected.is_empty()
            && expected.len() == job.target_names.len()
            && expected == result_names
            && job
                .results
                .iter()
                .all(|(name, result)| result.target_name == *name && result.passed());
        let failed = !complete_pass;
        let reason = if job.status == JobStatus::Cancelled {
            "cancelled"
        } else if failed {
            "target_failure"
        } else {
            "passed"
        };
        Self {
            schema_version: 1,
            job_id: job.id.clone(),
            terminal_at: job.completed_at.unwrap_or_else(Utc::now),
            failed,
            reason: reason.to_owned(),
        }
    }
}

/// Write the terminal manifest atomically if this job has a log directory.
pub fn write_terminal_manifest(state_dir: &Path, job: &Job) -> io::Result<()> {
    let job_dir = state_dir.join("logs").join(&job.id);
    if !job_dir.is_dir() {
        return Ok(());
    }
    let destination = job_dir.join(TERMINAL_MANIFEST_FILE);
    let bytes = serde_json::to_vec_pretty(&TerminalLogManifest::from_job(job))?;
    let mut temporary = tempfile::NamedTempFile::new_in(&job_dir)?;
    let file = temporary.as_file_mut();
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    temporary
        .persist(&destination)
        .map_err(|error| error.error)?;
    sync_parent_directory(&destination)
}

/// Remove a valid manifest before durably changing its terminal disposition.
/// A failed replacement can then only leave protected unclassified evidence,
/// never a stale success classification.
pub fn invalidate_conflicting_terminal_manifest(state_dir: &Path, job: &Job) -> io::Result<()> {
    let job_dir = state_dir.join("logs").join(&job.id);
    let Some(existing) = read_terminal_manifest(&job_dir) else {
        return Ok(());
    };
    if existing == TerminalLogManifest::from_job(job) {
        return Ok(());
    }
    fs::remove_file(job_dir.join(TERMINAL_MANIFEST_FILE))?;
    #[cfg(unix)]
    File::open(job_dir)?.sync_all()?;
    Ok(())
}

/// Load a valid terminal manifest. Invalid or future manifests are classified
/// by callers as protected legacy evidence.
#[must_use]
pub fn read_terminal_manifest(job_dir: &Path) -> Option<TerminalLogManifest> {
    let manifest = serde_json::from_slice::<TerminalLogManifest>(
        &fs::read(job_dir.join(TERMINAL_MANIFEST_FILE)).ok()?,
    )
    .ok()?;
    let directory_job_id = job_dir.file_name()?.to_str()?;
    (manifest.schema_version == 1 && manifest.job_id == directory_job_id).then_some(manifest)
}

/// Preserve an existing file, including a small bounded restart history,
/// before opening a new writer. The active file is never truncated in place.
pub fn rotate_before_open(path: &Path, segments: usize) -> io::Result<bool> {
    let segments = segments.max(1);
    prune_excess_segments(path, segments)?;
    sync_parent_directory(path)?;
    if !path.is_file() || path.metadata()?.len() == 0 {
        return Ok(false);
    }
    let oldest = rotated_path(path, segments);
    if oldest.exists() {
        fs::remove_file(oldest)?;
    }
    for index in (1..segments).rev() {
        let from = rotated_path(path, index);
        if from.exists() {
            fs::rename(from, rotated_path(path, index + 1))?;
        }
    }
    fs::rename(path, rotated_path(path, 1))?;
    sync_parent_directory(path)?;
    Ok(true)
}

/// Make directory-entry changes durable on platforms where std exposes a
/// directory handle suitable for syncing.
pub fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn prune_excess_segments(path: &Path, segments: usize) -> io::Result<()> {
    // Configuration has always capped this at 32, so enumerating the native
    // suffix paths covers every segment Shipyard can create without decoding
    // a potentially non-UTF-8 filename.
    for index in (segments + 1)..=32 {
        let candidate = rotated_path(path, index);
        if candidate.is_file() {
            fs::remove_file(candidate)?;
        }
    }
    Ok(())
}

/// Rotate only when the configured size threshold has been crossed.
pub fn rotate_if_oversize(path: &Path, policy: LogRetentionPolicy) -> io::Result<bool> {
    prune_excess_segments(path, policy.rotated_segments.max(1))?;
    sync_parent_directory(path)?;
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.len() >= policy.max_active_file_bytes)
    {
        rotate_before_open(path, policy.rotated_segments)
    } else {
        Ok(false)
    }
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    suffixed_path(path, &format!(".{index}"))
}

fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

/// Gzip one closed log with an atomic destination rename. The source remains
/// intact unless the complete gzip stream is durable and visible.
pub fn gzip_closed_log(path: &Path) -> io::Result<PathBuf> {
    let destination = suffixed_path(path, ".gz");
    if let Some(verification) = verify_gzip_for_retirement(path)? {
        // Two-pass retirement: a prior cleanup published the complete gzip
        // while retaining the source. Verify that durable prior generation
        // before removing the source on this later invocation.
        if retire_verified_gzip_source(path, &verification)? {
            return Ok(destination);
        }
    }
    let staging = path.parent().unwrap_or_else(|| Path::new("."));
    let prepared = prepare_gzip_derivative(path, staging)?;
    publish_gzip_derivative(path, prepared)
}

/// Stable metadata captured alongside an expensive gzip/source digest check.
pub struct GzipRetirementVerification {
    source: FileSignature,
    derivative: FileSignature,
    content_digest: [u8; 32],
}

#[derive(Eq, PartialEq)]
struct FileSignature {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial: u64,
    #[cfg(windows)]
    file_index: u64,
}

fn file_signature(path: &Path) -> io::Result<FileSignature> {
    let metadata = path.metadata()?;
    #[cfg(windows)]
    let windows_info = {
        let handle = winapi_util::Handle::from_path_any(path)?;
        winapi_util::file::information(&handle)?
    };
    Ok(FileSignature {
        len: metadata.len(),
        modified: metadata.modified()?,
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(windows)]
        volume_serial: windows_info.volume_serial_number(),
        #[cfg(windows)]
        file_index: windows_info.file_index(),
    })
}

/// Hash a published gzip and source outside the queue lock, returning a token
/// that permits a later constant-time, metadata-validated retirement.
pub fn verify_gzip_for_retirement(path: &Path) -> io::Result<Option<GzipRetirementVerification>> {
    let destination = suffixed_path(path, ".gz");
    if !destination.is_file() {
        return Ok(None);
    }
    let source = file_signature(path)?;
    let derivative = file_signature(&destination)?;
    let derivative_digest = reader_digest(flate2::read::GzDecoder::new(File::open(&destination)?))?;
    let source_digest = reader_digest(File::open(path)?)?;
    if derivative_digest != source_digest {
        return Ok(None);
    }
    Ok(Some(GzipRetirementVerification {
        source,
        derivative,
        content_digest: source_digest,
    }))
}

/// Retire a source only if neither it nor its verified gzip changed since the
/// expensive digest pass.
pub fn retire_verified_gzip_source(
    path: &Path,
    verification: &GzipRetirementVerification,
) -> io::Result<bool> {
    let destination = suffixed_path(path, ".gz");
    if file_signature(path)? != verification.source
        || file_signature(&destination)? != verification.derivative
    {
        return Ok(false);
    }
    let source_digest = reader_digest(File::open(path)?)?;
    let derivative_digest = reader_digest(flate2::read::GzDecoder::new(File::open(&destination)?))?;
    if source_digest != verification.content_digest
        || derivative_digest != verification.content_digest
        || file_signature(path)? != verification.source
        || file_signature(&destination)? != verification.derivative
    {
        return Ok(false);
    }
    fs::remove_file(path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(true)
}

/// Secure random staging file for a prepared gzip derivative.
pub struct PreparedGzip {
    temporary: tempfile::NamedTempFile,
}

/// Build and fsync a gzip derivative without publishing or removing evidence.
pub fn prepare_gzip_derivative(path: &Path, staging_dir: &Path) -> io::Result<PreparedGzip> {
    fs::create_dir_all(staging_dir)?;
    let mut temporary = tempfile::NamedTempFile::new_in(staging_dir)?;
    let source = File::open(path)?;
    let mut encoder = GzEncoder::new(BufWriter::new(temporary.as_file_mut()), Compression::fast());
    io::copy(&mut BufReader::new(source), &mut encoder)?;
    let mut output = encoder.finish()?;
    output.flush()?;
    output.get_ref().sync_all()?;
    drop(output);
    Ok(PreparedGzip { temporary })
}

/// Atomically publish a prepared derivative while retaining its source.
pub fn publish_gzip_derivative(path: &Path, prepared: PreparedGzip) -> io::Result<PathBuf> {
    let destination = suffixed_path(path, ".gz");
    if destination.exists() {
        fs::remove_file(&destination)?;
    }
    prepared
        .temporary
        .persist(&destination)
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    if let Some(parent) = destination.parent() {
        File::open(parent)?.sync_all()?;
    }
    // Keep the source through this cleanup pass. A later pass verifies the
    // published gzip before removing it, so a crash cannot lose both names.
    Ok(destination)
}

/// Whether a later gzip pass will retire this source because its published
/// gzip is complete and byte-for-byte equivalent.
pub fn gzip_source_will_retire(path: &Path) -> io::Result<bool> {
    verify_gzip_for_retirement(path).map(|value| value.is_some())
}

fn reader_digest(mut reader: impl io::Read) -> io::Result<[u8; 32]> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Priority, TargetResult, TargetStatus, ValidationMode};

    #[test]
    fn rotation_preserves_previous_evidence_and_bounds_segments() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("daemon.log");
        fs::write(&path, "one").expect("one");
        rotate_before_open(&path, 2).expect("rotate one");
        fs::write(&path, "two").expect("two");
        rotate_before_open(&path, 2).expect("rotate two");
        fs::write(&path, "three").expect("three");
        rotate_before_open(&path, 2).expect("rotate three");
        assert_eq!(
            fs::read_to_string(rotated_path(&path, 1)).expect("one"),
            "three"
        );
        assert_eq!(
            fs::read_to_string(rotated_path(&path, 2)).expect("two"),
            "two"
        );
        assert!(!rotated_path(&path, 3).exists());
        fs::write(rotated_path(&path, 3), "old-three").expect("old three");
        fs::write(rotated_path(&path, 4), "old-four").expect("old four");
        rotate_if_oversize(
            &path,
            LogRetentionPolicy {
                rotated_segments: 2,
                max_active_file_bytes: u64::MAX,
                ..LogRetentionPolicy::default()
            },
        )
        .expect("lower segment bound with absent active file");
        assert!(!rotated_path(&path, 3).exists());
        assert!(!rotated_path(&path, 4).exists());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn rotation_preserves_non_utf8_path_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let temp = tempfile::tempdir().expect("temp");
        let path = temp
            .path()
            .join(OsString::from_vec(b"target-\xff.log".to_vec()));
        fs::write(&path, "evidence").expect("log");
        rotate_before_open(&path, 1).expect("rotate");
        assert_eq!(
            fs::read_to_string(suffixed_path(&path, ".1")).expect("segment"),
            "evidence"
        );
    }

    #[test]
    fn terminal_manifest_pins_failure_classification() {
        let mut job = Job::create(
            "sha",
            "branch",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        job.status = JobStatus::Completed;
        job.results.insert(
            "macos".to_owned(),
            TargetResult::new("macos", "macos", TargetStatus::Fail, "local"),
        );
        let manifest = TerminalLogManifest::from_job(&job);
        assert!(manifest.failed);
        assert_eq!(manifest.reason, "target_failure");
    }

    #[test]
    fn terminal_manifest_must_match_its_directory() {
        let temp = tempfile::tempdir().expect("temp");
        let job_dir = temp.path().join("actual-job");
        fs::create_dir_all(&job_dir).expect("job dir");
        let manifest = TerminalLogManifest {
            schema_version: 1,
            job_id: "other-job".to_owned(),
            terminal_at: Utc::now(),
            failed: false,
            reason: "passed".to_owned(),
        };
        fs::write(
            job_dir.join(TERMINAL_MANIFEST_FILE),
            serde_json::to_vec(&manifest).expect("json"),
        )
        .expect("manifest");
        assert!(read_terminal_manifest(&job_dir).is_none());
    }

    #[test]
    fn incomplete_completed_job_is_failure_protected() {
        let mut job = Job::create(
            "sha",
            "branch",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        job.status = JobStatus::Completed;
        job.completed_at = Some(Utc::now());
        assert!(TerminalLogManifest::from_job(&job).failed);
        job.results.insert(
            "macos".to_owned(),
            TargetResult::new("macos", "macos", TargetStatus::Pass, "local"),
        );
        job.completed_at = None;
        assert!(
            TerminalLogManifest::from_job(&job).failed,
            "a timestamp-less completion remains unclassified failure evidence"
        );
    }

    #[test]
    fn mismatched_result_key_is_failure_protected() {
        let mut job = Job::create(
            "sha",
            "branch",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        job.status = JobStatus::Completed;
        job.completed_at = Some(Utc::now());
        job.results.insert(
            "linux".to_owned(),
            TargetResult::new("linux", "linux", TargetStatus::Pass, "local"),
        );
        assert!(TerminalLogManifest::from_job(&job).failed);
    }

    #[test]
    fn normal_queue_terminal_update_writes_manifest() {
        use crate::queue::Queue;
        let temp = tempfile::tempdir().expect("temp");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let mut job = Job::create(
            "sha",
            "branch",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        queue.enqueue(job.clone()).expect("enqueue");
        fs::create_dir_all(temp.path().join("logs").join(&job.id)).expect("logs");
        job.status = JobStatus::Completed;
        job.completed_at = Some(Utc::now());
        job.results.insert(
            "macos".to_owned(),
            TargetResult::new("macos", "macos", TargetStatus::Pass, "local"),
        );
        queue.update(&job).expect("terminal update");
        let manifest =
            read_terminal_manifest(&temp.path().join("logs").join(&job.id)).expect("manifest");
        assert!(!manifest.failed);
    }

    #[test]
    #[cfg(unix)]
    fn fixed_manifest_temporary_symlink_is_never_followed() {
        use crate::queue::Queue;
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("temp");
        let external = temp.path().join("external");
        fs::write(&external, "outside evidence\n").expect("external");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let mut job = Job::create(
            "sha",
            "branch",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        queue.enqueue(job.clone()).expect("enqueue");
        let log_dir = temp.path().join("logs").join(&job.id);
        fs::create_dir_all(&log_dir).expect("log dir");
        symlink(
            &external,
            log_dir.join(format!("{TERMINAL_MANIFEST_FILE}.tmp")),
        )
        .expect("adversarial fixed temporary symlink");
        job.status = JobStatus::Completed;
        job.completed_at = Some(Utc::now());
        job.results.insert(
            "macos".to_owned(),
            TargetResult::new("macos", "macos", TargetStatus::Pass, "local"),
        );
        queue.update(&job).expect("queue outcome remains durable");
        assert_eq!(
            queue.get(&job.id).expect("read queue").expect("job").status,
            JobStatus::Completed
        );
        assert!(read_terminal_manifest(&log_dir).is_some());
        assert_eq!(
            fs::read_to_string(external).expect("external"),
            "outside evidence\n"
        );
    }

    #[test]
    fn fixed_temporary_collision_cannot_block_failure_reclassification() {
        use crate::queue::Queue;
        let temp = tempfile::tempdir().expect("temp");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let mut job = Job::create(
            "sha",
            "branch",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        queue.enqueue(job.clone()).expect("enqueue");
        let log_dir = temp.path().join("logs").join(&job.id);
        fs::create_dir_all(&log_dir).expect("log dir");
        job.status = JobStatus::Completed;
        job.completed_at = Some(Utc::now());
        job.results.insert(
            "macos".to_owned(),
            TargetResult::new("macos", "macos", TargetStatus::Pass, "local"),
        );
        queue.update(&job).expect("successful outcome");
        assert!(
            !read_terminal_manifest(&log_dir)
                .expect("success manifest")
                .failed
        );

        fs::create_dir(log_dir.join(format!("{TERMINAL_MANIFEST_FILE}.tmp")))
            .expect("block replacement");
        job.results.insert(
            "macos".to_owned(),
            TargetResult::new("macos", "macos", TargetStatus::Error, "local"),
        );
        queue
            .update(&job)
            .expect("failed reclassification is durable");
        assert!(
            read_terminal_manifest(&log_dir)
                .expect("failure manifest")
                .failed
        );
        assert!(!queue.get(&job.id).expect("queue").expect("job").passed());
    }

    #[test]
    fn stale_recovered_publication_cannot_overwrite_newer_failure_manifest() {
        use crate::queue::Queue;
        let temp = tempfile::tempdir().expect("temp");
        let mut queue = Queue::new(temp.path()).expect("queue");
        let mut success = Job::create(
            "sha",
            "branch",
            vec!["macos".to_owned()],
            ValidationMode::Full,
            Priority::Normal,
        );
        queue.enqueue(success.clone()).expect("enqueue");
        let log_dir = temp.path().join("logs").join(&success.id);
        fs::create_dir_all(&log_dir).expect("log dir");
        success.status = JobStatus::Completed;
        success.completed_at = Some(Utc::now());
        success.results.insert(
            "macos".to_owned(),
            TargetResult::new("macos", "macos", TargetStatus::Pass, "local"),
        );
        queue.update(&success).expect("success");
        let mut failure = success.clone();
        failure.results.insert(
            "macos".to_owned(),
            TargetResult::new("macos", "macos", TargetStatus::Error, "local"),
        );
        queue.update(&failure).expect("failure");
        queue
            .publish_terminal_manifest_if_current(&success)
            .expect("stale publish ignored");
        assert!(read_terminal_manifest(&log_dir).expect("manifest").failed);
    }

    #[test]
    fn gzip_is_atomic_and_round_trip_readable() {
        use flate2::read::GzDecoder;
        use std::io::Read;
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("target.log");
        fs::write(&path, "diagnostic evidence\n").expect("write");
        let compressed = gzip_closed_log(&path).expect("publish gzip");
        assert!(path.exists());
        let mut text = String::new();
        GzDecoder::new(File::open(compressed).expect("open gzip"))
            .read_to_string(&mut text)
            .expect("decode");
        assert_eq!(text, "diagnostic evidence\n");
        fs::write(&path, "DIAGNOSTIC EVIDENCE\n").expect("same-length source replacement");
        assert!(
            verify_gzip_for_retirement(&path)
                .expect("verify mismatch")
                .is_none(),
            "a valid gzip stream must not retire different current source content"
        );
        fs::write(&path, "diagnostic evidence\nlate line\n").expect("append replacement");
        let compressed = gzip_closed_log(&path).expect("republish changed source");
        assert!(path.exists());
        let mut changed = String::new();
        GzDecoder::new(File::open(compressed).expect("open changed gzip"))
            .read_to_string(&mut changed)
            .expect("decode changed");
        assert_eq!(changed, "diagnostic evidence\nlate line\n");
        gzip_closed_log(&path).expect("retire matching source");
        assert!(!path.exists());
    }

    #[test]
    fn gzip_retirement_rejects_same_content_source_replacement() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("target.log");
        fs::write(&path, "diagnostic evidence\n").expect("write source");
        gzip_closed_log(&path).expect("publish gzip");
        let verification = verify_gzip_for_retirement(&path)
            .expect("verify gzip")
            .expect("matching derivative");

        fs::rename(&path, temp.path().join("original.log")).expect("move verified source");
        fs::write(&path, "diagnostic evidence\n").expect("replace with equal content");

        assert!(
            !retire_verified_gzip_source(&path, &verification).expect("reject replacement"),
            "path identity must prevent retiring a source replaced after verification"
        );
        assert!(path.exists());
    }
}
