//! Shared subprocess streaming helpers for validation executors.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::process::ProcessTree;

const OUTPUT_TAIL_BYTES_CAP: usize = 1_048_576;
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_STUCK_IDLE: Duration = Duration::from_secs(90);

/// Progress event emitted while a command runs.
#[derive(Clone, Debug, PartialEq)]
pub struct ProgressEvent {
    /// Current validation phase, when known.
    pub phase: Option<String>,
    /// Timestamp of the last decoded output line.
    pub last_output_at: Option<DateTime<Utc>>,
    /// Timestamp of the latest output or idle heartbeat.
    pub last_heartbeat_at: DateTime<Utc>,
    /// Seconds since the last decoded output line.
    pub quiet_for_secs: f64,
    /// Liveness label: `active`, `quiet`, or `stuck`.
    pub liveness: String,
}

impl ProgressEvent {
    /// Build a phase-only active event.
    #[must_use]
    pub fn phase(phase: impl Into<String>) -> Self {
        Self {
            phase: Some(phase.into()),
            last_output_at: None,
            last_heartbeat_at: Utc::now(),
            quiet_for_secs: 0.0,
            liveness: "active".to_owned(),
        }
    }
}

/// Command shape accepted by the streaming runner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamingCommandSpec {
    /// Run through the platform shell.
    Shell(String),
    /// Run an executable with explicit argv.
    Args(Vec<String>),
}

/// Action requested by a decoded output-line observer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamLineAction {
    /// Continue streaming the process.
    Continue,
    /// Stop the process and return a completed streaming result.
    Terminate(String),
}

/// Action requested by a validation progress observer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgressAction {
    /// Continue running the validation process.
    Continue,
    /// Terminate the validation process tree and preserve the reason.
    Terminate(String),
}

/// Streaming command request.
pub struct StreamingCommand<'a> {
    /// Command or argv to execute.
    pub command: StreamingCommandSpec,
    /// Optional working directory.
    pub cwd: Option<PathBuf>,
    /// Optional log file path.
    pub log_path: Option<PathBuf>,
    /// Append to the log instead of replacing it.
    pub append: bool,
    /// Optional wall-clock timeout.
    pub timeout: Option<Duration>,
    /// Initial phase.
    pub phase: Option<String>,
    /// Idle heartbeat cadence.
    pub heartbeat_interval: Duration,
    /// Idle duration after which liveness becomes `stuck`.
    pub stuck_idle: Duration,
    /// Contract markers to scan as substrings in decoded output lines.
    pub required_contract_markers: Vec<String>,
    /// Optional progress callback.
    pub progress_callback: Option<&'a mut dyn FnMut(ProgressEvent) -> ProgressAction>,
    /// Optional decoded output-line callback.
    pub line_callback: Option<&'a mut dyn FnMut(&str) -> StreamLineAction>,
}

impl StreamingCommand<'_> {
    /// Construct a shell command with Python-compatible defaults.
    #[must_use]
    pub fn shell(command: impl Into<String>) -> Self {
        Self {
            command: StreamingCommandSpec::Shell(command.into()),
            cwd: None,
            log_path: None,
            append: false,
            timeout: None,
            phase: None,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            stuck_idle: DEFAULT_STUCK_IDLE,
            required_contract_markers: Vec::new(),
            progress_callback: None,
            line_callback: None,
        }
    }
}

/// Result of a streamed subprocess execution.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamingCommandResult {
    /// Process exit code.
    pub returncode: i32,
    /// Bounded output tail.
    pub output: String,
    /// Process start timestamp.
    pub started_at: DateTime<Utc>,
    /// Process completion timestamp.
    pub completed_at: DateTime<Utc>,
    /// Wall-clock duration in seconds.
    pub duration_secs: f64,
    /// Timestamp of the last decoded output line.
    pub last_output_at: Option<DateTime<Utc>>,
    /// Last observed phase.
    pub phase: Option<String>,
    /// Contract markers seen in first-observed order.
    pub contract_markers_seen: Vec<String>,
    /// Timestamp of latest output or idle heartbeat.
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// Reason the process was stopped before natural exit, when applicable.
    pub termination_reason: Option<String>,
}

/// Streaming command failure.
#[derive(Debug)]
pub enum StreamingError {
    /// The argv form had no program.
    MissingProgram,
    /// Process I/O failed.
    Io(io::Error),
    /// Wall-clock timeout expired.
    Timeout {
        /// Timeout budget.
        timeout: Duration,
        /// Elapsed runtime.
        elapsed: Duration,
    },
}

impl std::fmt::Display for StreamingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingProgram => formatter.write_str("streaming command has no program"),
            Self::Io(error) => write!(formatter, "streaming command I/O failed: {error}"),
            Self::Timeout { timeout, .. } => {
                write!(formatter, "streaming command timed out after {timeout:?}")
            }
        }
    }
}

impl std::error::Error for StreamingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::MissingProgram | Self::Timeout { .. } => None,
        }
    }
}

impl From<io::Error> for StreamingError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Run a command while streaming output to disk and progress callbacks.
pub fn run_streaming_command(
    mut request: StreamingCommand<'_>,
) -> Result<StreamingCommandResult, StreamingError> {
    let started_at = Utc::now();
    let start = Instant::now();
    let mut child = spawn_command(&request)?;
    let stdout = child.take_stdout();
    let stderr = child.take_stderr();
    let (sender, receiver) = mpsc::channel();
    let mut readers = spawn_readers(sender, stdout, stderr);
    let mut log = open_log(&request)?;
    let mut state = StreamState::new(request.phase.take());
    let mut termination_reason = None;

    loop {
        if let Some(timeout) = request.timeout
            && start.elapsed() > timeout
        {
            kill_child(&mut child);
            join_readers(&mut readers);
            return Err(StreamingError::Timeout {
                timeout,
                elapsed: start.elapsed(),
            });
        }

        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(StreamMessage::Line(line)) => {
                match state.record_line(&line, &mut log, &mut request.progress_callback)? {
                    ProgressAction::Continue => {}
                    ProgressAction::Terminate(reason) => {
                        termination_reason = Some(reason);
                        kill_child(&mut child);
                        break;
                    }
                }
                state.scan_markers(&line, &request.required_contract_markers);
                if let Some(callback) = request.line_callback.as_mut() {
                    match callback(&line) {
                        StreamLineAction::Continue => {}
                        StreamLineAction::Terminate(reason) => {
                            termination_reason = Some(reason);
                            kill_child(&mut child);
                            break;
                        }
                    }
                }
            }
            Ok(StreamMessage::Eof) => {
                state.active_readers = state.active_readers.saturating_sub(1);
                if state.active_readers == 0 {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if child.try_wait()?.is_some() && state.active_readers == 0 {
                    break;
                }
                if let ProgressAction::Terminate(reason) = state.emit_idle_heartbeat(
                    &mut request.progress_callback,
                    start,
                    request.heartbeat_interval,
                    request.stuck_idle,
                ) {
                    termination_reason = Some(reason);
                    kill_child(&mut child);
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let status = child.wait()?;
    join_readers(&mut readers);
    let completed_at = Utc::now();
    Ok(StreamingCommandResult {
        returncode: status.code().unwrap_or(-1),
        output: state.output_tail(),
        started_at,
        completed_at,
        duration_secs: start.elapsed().as_secs_f64(),
        last_output_at: state.last_output_at,
        phase: state.phase,
        contract_markers_seen: state.seen_markers,
        last_heartbeat_at: state.last_heartbeat_at,
        termination_reason,
    })
}

#[derive(Debug)]
enum StreamMessage {
    Line(String),
    Eof,
}

struct StreamState {
    active_readers: usize,
    output_tail: VecDeque<String>,
    output_tail_bytes: usize,
    last_output_at: Option<DateTime<Utc>>,
    last_output_instant: Instant,
    last_heartbeat_instant: Instant,
    last_heartbeat_at: Option<DateTime<Utc>>,
    phase: Option<String>,
    seen_markers: Vec<String>,
}

impl StreamState {
    fn new(phase: Option<String>) -> Self {
        let now = Instant::now();
        Self {
            active_readers: 2,
            output_tail: VecDeque::new(),
            output_tail_bytes: 0,
            last_output_at: None,
            last_output_instant: now,
            last_heartbeat_instant: now,
            last_heartbeat_at: None,
            phase,
            seen_markers: Vec::new(),
        }
    }

    fn record_line(
        &mut self,
        line: &str,
        log: &mut Option<std::fs::File>,
        progress_callback: &mut Option<&mut dyn FnMut(ProgressEvent) -> ProgressAction>,
    ) -> io::Result<ProgressAction> {
        self.output_tail.push_back(line.to_owned());
        self.output_tail_bytes += line.len();
        while self.output_tail_bytes > OUTPUT_TAIL_BYTES_CAP && self.output_tail.len() > 1 {
            if let Some(removed) = self.output_tail.pop_front() {
                self.output_tail_bytes -= removed.len();
            }
        }

        if let Some(log) = log {
            log.write_all(line.as_bytes())?;
            log.flush()?;
        }

        if let Some(phase) = parse_phase_marker(line.trim()) {
            self.phase = Some(phase);
        }
        let now = Utc::now();
        self.last_output_at = Some(now);
        self.last_heartbeat_at = Some(now);
        self.last_output_instant = Instant::now();
        self.last_heartbeat_instant = self.last_output_instant;
        Ok(emit_progress(
            progress_callback,
            ProgressEvent {
                phase: self.phase.clone(),
                last_output_at: self.last_output_at,
                last_heartbeat_at: now,
                quiet_for_secs: 0.0,
                liveness: "active".to_owned(),
            },
        ))
    }

    fn scan_markers(&mut self, line: &str, required_markers: &[String]) {
        for marker in required_markers {
            if line.contains(marker) && !self.seen_markers.contains(marker) {
                self.seen_markers.push(marker.clone());
            }
        }
    }

    fn emit_idle_heartbeat(
        &mut self,
        progress_callback: &mut Option<&mut dyn FnMut(ProgressEvent) -> ProgressAction>,
        start: Instant,
        heartbeat_interval: Duration,
        stuck_idle: Duration,
    ) -> ProgressAction {
        if self.last_heartbeat_instant.elapsed() < heartbeat_interval {
            return ProgressAction::Continue;
        }
        let now = Utc::now();
        let quiet_duration = if self.last_output_at.is_some() {
            self.last_output_instant.elapsed()
        } else {
            start.elapsed()
        };
        let liveness = if quiet_duration >= stuck_idle {
            "stuck"
        } else {
            "quiet"
        };
        self.last_heartbeat_at = Some(now);
        self.last_heartbeat_instant = Instant::now();
        emit_progress(
            progress_callback,
            ProgressEvent {
                phase: self.phase.clone(),
                last_output_at: self.last_output_at,
                last_heartbeat_at: now,
                quiet_for_secs: quiet_duration.as_secs_f64(),
                liveness: liveness.to_owned(),
            },
        )
    }

    fn output_tail(&self) -> String {
        self.output_tail.iter().cloned().collect()
    }
}

fn spawn_command(request: &StreamingCommand<'_>) -> Result<ProcessTree, StreamingError> {
    let mut command = match &request.command {
        StreamingCommandSpec::Shell(command) => shell_command(command),
        StreamingCommandSpec::Args(argv) => argv_command(argv)?,
    };
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    Ok(ProcessTree::spawn(&mut command)?)
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("cmd");
    shell.args(["/C", command]);
    shell
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("sh");
    shell.args(["-c", command]);
    shell
}

fn argv_command(argv: &[String]) -> Result<Command, StreamingError> {
    let Some((program, arguments)) = argv.split_first() else {
        return Err(StreamingError::MissingProgram);
    };
    let mut command = Command::new(program);
    command.args(arguments);
    Ok(command)
}

fn open_log(request: &StreamingCommand<'_>) -> io::Result<Option<std::fs::File>> {
    let Some(path) = &request.log_path else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(Some(
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(request.append)
            .truncate(!request.append)
            .open(path)?,
    ))
}

fn spawn_readers(
    sender: Sender<StreamMessage>,
    stdout: Option<impl Read + Send + 'static>,
    stderr: Option<impl Read + Send + 'static>,
) -> Vec<JoinHandle<()>> {
    let mut readers = Vec::new();
    if let Some(stdout) = stdout {
        readers.push(spawn_reader(stdout, sender.clone()));
    }
    if let Some(stderr) = stderr {
        readers.push(spawn_reader(stderr, sender));
    }
    readers
}

fn spawn_reader<R: Read + Send + 'static>(
    stream: R,
    sender: Sender<StreamMessage>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&bytes).into_owned();
                    if sender.send(StreamMessage::Line(line)).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = sender.send(StreamMessage::Eof);
    })
}

fn kill_child(child: &mut ProcessTree) {
    child.terminate();
}

fn join_readers(readers: &mut Vec<JoinHandle<()>>) {
    for reader in readers.drain(..) {
        let _ = reader.join();
    }
}

fn emit_progress(
    progress_callback: &mut Option<&mut dyn FnMut(ProgressEvent) -> ProgressAction>,
    event: ProgressEvent,
) -> ProgressAction {
    if let Some(callback) = progress_callback {
        callback(event)
    } else {
        ProgressAction::Continue
    }
}

fn parse_phase_marker(line: &str) -> Option<String> {
    for prefix in ["__SHIPYARD_PHASE__:", "__PULP_PHASE__:"] {
        if let Some(value) = line.strip_prefix(prefix) {
            return Some(value.trim().to_owned());
        }
    }

    let inner = line.strip_prefix("===")?.strip_suffix("===")?.trim();
    if inner.is_empty() {
        return None;
    }
    inner
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        .then(|| inner.to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        ProgressAction, ProgressEvent, StreamLineAction, StreamingCommand, StreamingCommandSpec,
        StreamingError, run_streaming_command,
    };

    #[test]
    fn streaming_command_parses_phase_markers_and_writes_log() {
        let temp = tempfile::NamedTempFile::new().expect("log");
        let mut events = Vec::<ProgressEvent>::new();
        let mut request = StreamingCommand::shell("echo __SHIPYARD_PHASE__:build && echo building");
        request.log_path = Some(temp.path().to_path_buf());
        let mut callback = |event| {
            events.push(event);
            ProgressAction::Continue
        };
        request.progress_callback = Some(&mut callback);

        let result = run_streaming_command(request).expect("run");

        assert_eq!(result.returncode, 0);
        assert_eq!(result.phase.as_deref(), Some("build"));
        assert!(result.output.contains("building"));
        assert!(
            std::fs::read_to_string(temp.path())
                .expect("log")
                .contains("building")
        );
        assert!(
            events
                .iter()
                .any(|event| event.phase.as_deref() == Some("build"))
        );
    }

    #[test]
    fn streaming_command_collects_contract_markers_in_order() {
        let mut request =
            StreamingCommand::shell("echo before MARKER_TWO && echo MARKER_ONE && echo MARKER_TWO");
        request.required_contract_markers = vec!["MARKER_ONE".to_owned(), "MARKER_TWO".to_owned()];

        let result = run_streaming_command(request).expect("run");

        assert_eq!(
            result.contract_markers_seen,
            vec!["MARKER_TWO".to_owned(), "MARKER_ONE".to_owned()]
        );
    }

    #[test]
    fn streaming_command_line_callback_can_stop_process() {
        let mut request = StreamingCommand::shell("printf 'start\\nSTOP\\nafter\\n'; sleep 3");
        let mut callback = |line: &str| {
            if line.contains("STOP") {
                StreamLineAction::Terminate("matched STOP".to_owned())
            } else {
                StreamLineAction::Continue
            }
        };
        request.line_callback = Some(&mut callback);

        let result = run_streaming_command(request).expect("run");

        assert_eq!(result.termination_reason.as_deref(), Some("matched STOP"));
        assert!(result.output.contains("STOP"));
    }

    #[cfg(not(windows))]
    #[test]
    fn progress_callback_terminates_the_running_process_tree() {
        let mut request = StreamingCommand::shell("echo started; sleep 30");
        let mut callback =
            |_event| ProgressAction::Terminate("durable queue cancellation".to_owned());
        request.progress_callback = Some(&mut callback);
        let started = std::time::Instant::now();

        let result = run_streaming_command(request).expect("run");

        assert_eq!(
            result.termination_reason.as_deref(),
            Some("durable queue cancellation")
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the descendant sleep process must not retain the output pipes"
        );
    }

    #[test]
    fn argv_command_rejects_empty_argv() {
        let request = StreamingCommand {
            command: StreamingCommandSpec::Args(Vec::new()),
            cwd: None,
            log_path: None,
            append: false,
            timeout: None,
            phase: None,
            heartbeat_interval: Duration::from_secs(30),
            stuck_idle: Duration::from_secs(90),
            required_contract_markers: Vec::new(),
            progress_callback: None,
            line_callback: None,
        };

        assert!(matches!(
            run_streaming_command(request).expect_err("missing program"),
            StreamingError::MissingProgram
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn streaming_command_times_out() {
        let mut request = StreamingCommand::shell("sleep 2");
        request.timeout = Some(Duration::from_millis(50));

        assert!(matches!(
            run_streaming_command(request).expect_err("timeout"),
            StreamingError::Timeout { .. }
        ));
    }

    #[test]
    fn streaming_command_emits_stuck_heartbeat() {
        let mut events = Vec::<ProgressEvent>::new();
        let mut request = StreamingCommand::shell("echo done");
        request.heartbeat_interval = Duration::from_millis(1);
        request.stuck_idle = Duration::from_millis(1);
        let mut callback = |event| {
            events.push(event);
            ProgressAction::Continue
        };
        request.progress_callback = Some(&mut callback);

        let result = run_streaming_command(request).expect("run");

        assert_eq!(result.returncode, 0);
        assert!(events.iter().any(|event| event.liveness == "active"));
    }
}
