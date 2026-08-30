//! Bounded, private release-asset streaming for fleet authority resolution.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::{CHECKSUM_ASSET, ObservedAsset};

const MAX_PLATFORM_ASSET_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CHECKSUM_ASSET_BYTES: u64 = 1024 * 1024;
const MAX_DOWNLOAD_STDERR_BYTES: u64 = 1024 * 1024;
const DOWNLOAD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const DOWNLOAD_TEARDOWN_BUDGET: Duration = Duration::from_millis(500);
const CAPTURE_DRAIN_BUDGET: Duration = Duration::from_millis(100);
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(super) struct DownloadedAsset {
    _directory: tempfile::TempDir,
    path: PathBuf,
    size: u64,
}

impl DownloadedAsset {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn read_all(&self) -> Result<Vec<u8>, String> {
        let capacity = usize::try_from(self.size)
            .map_err(|_| "release asset size exceeded this platform's address space".to_owned())?;
        let mut bytes = Vec::with_capacity(capacity);
        File::open(&self.path)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|error| format!("could not read downloaded release asset: {error}"))?;
        if bytes.len() as u64 != self.size {
            return Err("downloaded release asset changed after verification".to_owned());
        }
        Ok(bytes)
    }
}

pub(super) fn download_asset_to_private_file(
    command: &mut Command,
    asset: &ObservedAsset,
    timeout: Duration,
    staging_parent: Option<&Path>,
) -> Result<DownloadedAsset, String> {
    let maximum = if asset.name == CHECKSUM_ASSET {
        MAX_CHECKSUM_ASSET_BYTES
    } else {
        MAX_PLATFORM_ASSET_BYTES
    };
    if asset.size == 0 || asset.size > maximum {
        return Err(format!(
            "release asset {} size {} exceeded the supported range 1..={maximum}",
            asset.name, asset.size
        ));
    }
    let mut builder = tempfile::Builder::new();
    builder.prefix("shipyard-fleet-release-");
    let directory = match staging_parent {
        Some(parent) => builder.tempdir_in(parent),
        None => builder.tempdir(),
    }
    .map_err(|error| format!("could not create release asset staging directory: {error}"))?;
    set_private_directory(directory.path())?;
    let path = directory.path().join("asset");
    let writer = open_private_asset(&path)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let deadline = Instant::now() + timeout;
    let execution_deadline = deadline
        .checked_sub(DOWNLOAD_TEARDOWN_BUDGET.min(timeout / 4))
        .unwrap_or(deadline);
    let label = format!("download release asset {}", asset.name);
    let mut tree = crate::process::ProcessTree::spawn(command)
        .map_err(|error| format!("{label} spawn failed: {error}"))?;
    let (captures, events_rx) =
        start_bounded_captures(&mut tree, writer, asset.size, deadline, &label)?;
    let status = wait_for_download(&mut tree, &events_rx, deadline, execution_deadline, &label);
    // The direct gh process may exit while a descendant still owns the file.
    // Close the complete supervised tree before observing final bytes.
    tree.terminate_until(deadline);
    let (downloaded, detail) = captures.finish(&label);
    let status = status?;
    let downloaded = downloaded?;
    let detail = detail?;
    if !status.success() {
        let detail = String::from_utf8_lossy(&detail).trim().to_owned();
        return Err(if detail.is_empty() {
            format!("{label} exited {}", status.code().unwrap_or(-1))
        } else {
            format!("{label} failed: {detail}")
        });
    }
    if downloaded != asset.size {
        return Err(format!(
            "{label} was truncated: expected {} bytes, downloaded {downloaded}",
            asset.size
        ));
    }
    let actual = sha256_file(&path, asset.size)?;
    if actual != asset.sha256 {
        return Err(format!(
            "release asset {} changed during authority resolution: metadata {}, downloaded {}",
            asset.name, asset.sha256, actual
        ));
    }
    Ok(DownloadedAsset {
        _directory: directory,
        path,
        size: asset.size,
    })
}

struct CaptureThreads {
    asset: JoinHandle<Result<u64, String>>,
    diagnostic: JoinHandle<Result<Vec<u8>, String>>,
    stop: Arc<AtomicBool>,
}

impl CaptureThreads {
    fn finish(self, label: &str) -> (Result<u64, String>, Result<Vec<u8>, String>) {
        self.stop.store(true, Ordering::Release);
        (
            join_capture(self.asset, label, "asset"),
            join_capture(self.diagnostic, label, "diagnostic"),
        )
    }
}

fn start_bounded_captures(
    tree: &mut crate::process::ProcessTree,
    writer: File,
    expected_size: u64,
    deadline: Instant,
    label: &str,
) -> Result<(CaptureThreads, Receiver<String>), String> {
    let stdout = tree.take_stdout().ok_or_else(|| {
        tree.terminate_until(deadline);
        format!("{label} did not expose its asset stream")
    })?;
    let stderr = tree.take_stderr().ok_or_else(|| {
        tree.terminate_until(deadline);
        format!("{label} did not expose its diagnostic stream")
    })?;
    set_nonblocking(&stdout).map_err(|error| {
        tree.terminate_until(deadline);
        format!("{label} could not bound its asset stream: {error}")
    })?;
    set_nonblocking(&stderr).map_err(|error| {
        tree.terminate_until(deadline);
        format!("{label} could not bound its diagnostic stream: {error}")
    })?;
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stdout_events = events_tx.clone();
    let stdout_stop = Arc::clone(&stop);
    let asset = std::thread::Builder::new()
        .name("fleet-release-asset".to_owned())
        .spawn(move || {
            copy_asset_bounded(
                stdout,
                writer,
                expected_size,
                deadline,
                &stdout_stop,
                &stdout_events,
            )
        })
        .map_err(|error| {
            tree.terminate_until(deadline);
            format!("{label} could not start its bounded asset capture: {error}")
        })?;
    let diagnostic = match std::thread::Builder::new()
        .name("fleet-release-stderr".to_owned())
        .spawn({
            let stop = Arc::clone(&stop);
            move || capture_stderr_bounded(stderr, deadline, &stop, &events_tx)
        }) {
        Ok(diagnostic) => diagnostic,
        Err(error) => {
            tree.terminate_until(deadline);
            stop.store(true, Ordering::Release);
            let _ = asset.join();
            return Err(format!(
                "{label} could not start its bounded diagnostic capture: {error}"
            ));
        }
    };
    Ok((
        CaptureThreads {
            asset,
            diagnostic,
            stop,
        },
        events_rx,
    ))
}

fn wait_for_download(
    tree: &mut crate::process::ProcessTree,
    events: &Receiver<String>,
    deadline: Instant,
    execution_deadline: Instant,
    label: &str,
) -> Result<ExitStatus, String> {
    loop {
        match events.try_recv() {
            Ok(error) => {
                tree.terminate_until(deadline);
                return Err(format!("{label} {error}"));
            }
            Err(TryRecvError::Disconnected | TryRecvError::Empty) => {}
        }
        match tree.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < execution_deadline => {
                std::thread::sleep(
                    DOWNLOAD_POLL_INTERVAL
                        .min(execution_deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) => {
                tree.terminate_until(deadline);
                return Err(format!("{label} timed out"));
            }
            Err(error) => {
                tree.terminate_until(deadline);
                return Err(format!("{label} wait failed: {error}"));
            }
        }
    }
}

fn copy_asset_bounded<R: Read>(
    mut reader: R,
    mut writer: File,
    limit: u64,
    deadline: Instant,
    stop: &AtomicBool,
    events: &Sender<String>,
) -> Result<u64, String> {
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES].into_boxed_slice();
    let mut drain_deadline = None;
    loop {
        if capture_must_stop(stop, deadline, &mut drain_deadline) {
            writer.flush().map_err(|error| {
                signal_capture_failure(events, format!("asset flush failed: {error}"))
            })?;
            return Ok(copied);
        }
        let count = match reader.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    writer.flush().map_err(|error| {
                        signal_capture_failure(events, format!("asset flush failed: {error}"))
                    })?;
                    return Ok(copied);
                }
                if Instant::now() >= deadline {
                    return Err(signal_capture_failure(
                        events,
                        "asset capture timed out".to_owned(),
                    ));
                }
                std::thread::sleep(DOWNLOAD_POLL_INTERVAL);
                continue;
            }
            Err(error) => {
                return Err(signal_capture_failure(
                    events,
                    format!("asset read failed: {error}"),
                ));
            }
        };
        if count == 0 {
            writer.flush().map_err(|error| {
                signal_capture_failure(events, format!("asset flush failed: {error}"))
            })?;
            return Ok(copied);
        }
        let remaining = usize::try_from(limit.saturating_sub(copied)).map_err(|_| {
            signal_capture_failure(events, "asset size exceeded this platform".to_owned())
        })?;
        let accepted = count.min(remaining);
        writer.write_all(&buffer[..accepted]).map_err(|error| {
            signal_capture_failure(events, format!("asset write failed: {error}"))
        })?;
        copied += u64::try_from(accepted)
            .map_err(|_| signal_capture_failure(events, "asset chunk exceeded u64".to_owned()))?;
        if accepted != count {
            return Err(signal_capture_failure(
                events,
                format!("exceeded its declared {limit} byte size"),
            ));
        }
    }
}

fn capture_stderr_bounded<R: Read>(
    mut reader: R,
    deadline: Instant,
    stop: &AtomicBool,
    events: &Sender<String>,
) -> Result<Vec<u8>, String> {
    let mut captured = Vec::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES].into_boxed_slice();
    let mut drain_deadline = None;
    let limit = usize::try_from(MAX_DOWNLOAD_STDERR_BYTES).map_err(|_| {
        signal_capture_failure(events, "stderr limit exceeded this platform".to_owned())
    })?;
    loop {
        if capture_must_stop(stop, deadline, &mut drain_deadline) {
            return Ok(captured);
        }
        let count = match reader.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    return Ok(captured);
                }
                if Instant::now() >= deadline {
                    return Err(signal_capture_failure(
                        events,
                        "stderr capture timed out".to_owned(),
                    ));
                }
                std::thread::sleep(DOWNLOAD_POLL_INTERVAL);
                continue;
            }
            Err(error) => {
                return Err(signal_capture_failure(
                    events,
                    format!("stderr read failed: {error}"),
                ));
            }
        };
        if count == 0 {
            return Ok(captured);
        }
        let remaining = limit.saturating_sub(captured.len());
        let accepted = count.min(remaining);
        captured.extend_from_slice(&buffer[..accepted]);
        if accepted != count {
            return Err(signal_capture_failure(
                events,
                format!("stderr exceeded {MAX_DOWNLOAD_STDERR_BYTES} byte capture limit"),
            ));
        }
    }
}

fn capture_must_stop(
    stop: &AtomicBool,
    deadline: Instant,
    drain_deadline: &mut Option<Instant>,
) -> bool {
    let now = Instant::now();
    if now >= deadline {
        return true;
    }
    if stop.load(Ordering::Acquire) {
        let drain = drain_deadline.get_or_insert_with(|| {
            now.checked_add(CAPTURE_DRAIN_BUDGET)
                .map_or(deadline, |drain| drain.min(deadline))
        });
        now >= *drain
    } else {
        false
    }
}

#[cfg(unix)]
fn set_nonblocking<T: std::os::fd::AsFd>(stream: &T) -> Result<(), String> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};

    let flags = fcntl(stream, FcntlArg::F_GETFL)
        .map(OFlag::from_bits_truncate)
        .map_err(|error| error.to_string())?;
    fcntl(stream, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the cross-platform call site shares the Unix fallible contract"
)]
fn set_nonblocking<T>(_stream: &T) -> Result<(), String> {
    // Windows children are placed in a Job Object by ProcessTree. Terminating
    // that complete tree closes every inherited pipe handle, so the bounded
    // capture threads cannot be stranded by a detached descendant. Unix needs
    // O_NONBLOCK because process groups cannot contain a double-fork escapee.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn set_nonblocking<T>(_stream: &T) -> Result<(), String> {
    Err("fleet release streaming requires bounded process-tree pipes".to_owned())
}

fn signal_capture_failure(events: &Sender<String>, error: String) -> String {
    let _ = events.send(error.clone());
    error
}

fn join_capture<T>(
    capture: JoinHandle<Result<T, String>>,
    label: &str,
    stream: &str,
) -> Result<T, String> {
    capture
        .join()
        .map_err(|_| format!("{label} {stream} capture panicked"))?
        .map_err(|error| format!("{label} {error}"))
}

fn open_private_asset(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| format!("could not create private release asset staging file: {error}"))
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not protect release asset staging directory: {error}"))
}

#[cfg(windows)]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the cross-platform call site shares the Unix fallible contract"
)]
fn set_private_directory(_path: &Path) -> Result<(), String> {
    // tempfile creates the directory beneath the caller's protected temporary
    // root using the platform's inherited ACL. The asset itself is create_new,
    // so no pre-existing path can be followed or replaced during creation.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn set_private_directory(_path: &Path) -> Result<(), String> {
    Err("fleet release staging requires private-directory permissions".to_owned())
}

pub(super) fn sha256_file(path: &Path, expected_size: u64) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("could not open downloaded release asset: {error}"))?;
    let mut digest = Sha256::new();
    let copied = std::io::copy(&mut file, &mut digest)
        .map_err(|error| format!("could not hash downloaded release asset: {error}"))?;
    if copied != expected_size {
        return Err(format!(
            "downloaded release asset changed while hashing: expected {expected_size} bytes, read {copied}"
        ));
    }
    Ok(hex::encode(digest.finalize()))
}
