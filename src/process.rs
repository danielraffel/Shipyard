//! Cross-platform child-process tree supervision.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(not(windows))]
use std::process::Child;
#[cfg(unix)]
const TERMINATION_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(not(windows))]
const TERMINATION_REAP_TIMEOUT: Duration = Duration::from_secs(2);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_BOUNDED_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
#[cfg(unix)]
const DEADLINE_TEARDOWN_BUDGET: Duration = Duration::from_millis(500);

/// Failure from a descendant-safe command observation bounded by one deadline.
#[derive(Debug)]
pub(crate) enum BoundedOutputError {
    /// The shared deadline elapsed before the command completed.
    TimedOut { label: String },
    /// The command could not be spawned, waited, or captured.
    Unreadable {
        label: String,
        operation: &'static str,
        source: io::Error,
    },
    /// A probe exceeded the observer's fixed capture budget.
    OutputLimit { label: String, stream: &'static str },
}

impl fmt::Display for BoundedOutputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut { label } => write!(formatter, "{label} timed out"),
            Self::Unreadable {
                label,
                operation,
                source,
            } => write!(formatter, "{label} {operation} failed: {source}"),
            Self::OutputLimit { label, stream } => write!(
                formatter,
                "{label} {stream} exceeded {MAX_BOUNDED_OUTPUT_BYTES} byte capture limit"
            ),
        }
    }
}

/// Capture a command under an absolute deadline without pipe-reader wedges.
///
/// Regular-file capture keeps an escaped descendant that inherited stdout or
/// stderr from blocking this process. The complete supervised tree is reaped
/// on success, timeout, and error.
pub(crate) fn run_output_until(
    command: &mut Command,
    deadline: Instant,
    label: impl Into<String>,
) -> Result<Output, BoundedOutputError> {
    run_output_with_optional_input_until(command, None, deadline, label)
}

/// Capture one command with exact bounded stdin under the same descendant-safe
/// deadline contract as [`run_output_until`].
#[cfg(unix)]
pub(crate) fn run_output_with_input_until(
    command: &mut Command,
    input: &[u8],
    deadline: Instant,
    label: impl Into<String>,
) -> Result<Output, BoundedOutputError> {
    run_output_with_optional_input_until(command, Some(input), deadline, label)
}

#[allow(clippy::too_many_lines)]
fn run_output_with_optional_input_until(
    command: &mut Command,
    input: Option<&[u8]>,
    deadline: Instant,
    label: impl Into<String>,
) -> Result<Output, BoundedOutputError> {
    let label = label.into();
    if Instant::now() >= deadline {
        return Err(BoundedOutputError::TimedOut { label });
    }
    let mut stdout = tempfile::tempfile().map_err(|source| BoundedOutputError::Unreadable {
        label: label.clone(),
        operation: "stdout capture",
        source,
    })?;
    let mut stderr = tempfile::tempfile().map_err(|source| BoundedOutputError::Unreadable {
        label: label.clone(),
        operation: "stderr capture",
        source,
    })?;
    let mut stdin = input
        .map(|bytes| {
            let mut file = tempfile::tempfile()?;
            file.write_all(bytes)?;
            file.seek(SeekFrom::Start(0))?;
            Ok::<_, io::Error>(file)
        })
        .transpose()
        .map_err(|source| BoundedOutputError::Unreadable {
            label: label.clone(),
            operation: "stdin capture",
            source,
        })?;
    command
        .stdin(match stdin.as_mut() {
            Some(file) => {
                Stdio::from(
                    file.try_clone()
                        .map_err(|source| BoundedOutputError::Unreadable {
                            label: label.clone(),
                            operation: "stdin clone",
                            source,
                        })?,
                )
            }
            None => Stdio::null(),
        })
        .stdout(Stdio::from(stdout.try_clone().map_err(|source| {
            BoundedOutputError::Unreadable {
                label: label.clone(),
                operation: "stdout clone",
                source,
            }
        })?))
        .stderr(Stdio::from(stderr.try_clone().map_err(|source| {
            BoundedOutputError::Unreadable {
                label: label.clone(),
                operation: "stderr clone",
                source,
            }
        })?));
    let mut tree =
        ProcessTree::spawn(command).map_err(|source| BoundedOutputError::Unreadable {
            label: label.clone(),
            operation: "spawn",
            source,
        })?;
    #[cfg(unix)]
    let execution_deadline = {
        let remaining = deadline.saturating_duration_since(Instant::now());
        deadline
            .checked_sub(DEADLINE_TEARDOWN_BUDGET.min(remaining / 4))
            .unwrap_or(deadline)
    };
    #[cfg(not(unix))]
    let execution_deadline = deadline;
    let status = loop {
        for (stream, file) in [("stdout", &stdout), ("stderr", &stderr)] {
            let length = match file.metadata() {
                Ok(metadata) => metadata.len(),
                Err(source) => {
                    tree.terminate_until(deadline);
                    return Err(BoundedOutputError::Unreadable {
                        label,
                        operation: "capture metadata",
                        source,
                    });
                }
            };
            if length > MAX_BOUNDED_OUTPUT_BYTES {
                tree.terminate_until(deadline);
                return Err(BoundedOutputError::OutputLimit { label, stream });
            }
        }
        match tree.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < execution_deadline => {
                std::thread::sleep(
                    WAIT_POLL_INTERVAL
                        .min(execution_deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) => {
                tree.terminate_until(deadline);
                return Err(BoundedOutputError::TimedOut { label });
            }
            Err(source) => {
                tree.terminate_until(deadline);
                return Err(BoundedOutputError::Unreadable {
                    label,
                    operation: "wait",
                    source,
                });
            }
        }
    };
    // The leader can exit while a grandchild retains its stdio. Always reap
    // the process group before reading the regular captures.
    tree.terminate_until(deadline);
    let stdout = read_bounded_capture(&mut stdout, &label, "stdout")?;
    let stderr = read_bounded_capture(&mut stderr, &label, "stderr")?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn read_bounded_capture(
    file: &mut std::fs::File,
    label: &str,
    stream: &'static str,
) -> Result<Vec<u8>, BoundedOutputError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|source| BoundedOutputError::Unreadable {
            label: label.to_owned(),
            operation: "capture seek",
            source,
        })?;
    let mut bytes = Vec::new();
    file.take(MAX_BOUNDED_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| BoundedOutputError::Unreadable {
            label: label.to_owned(),
            operation: "capture read",
            source,
        })?;
    if bytes.len() as u64 > MAX_BOUNDED_OUTPUT_BYTES {
        return Err(BoundedOutputError::OutputLimit {
            label: label.to_owned(),
            stream,
        });
    }
    Ok(bytes)
}

/// A supervised child process tree.
///
/// Unix callers get a process-group leader. Windows callers get a process
/// created suspended, assigned to a Job Object, and then resumed. The retained
/// wrapper can terminate the Job even after the direct child has exited while
/// a descendant still holds an inherited stdout or stderr pipe open.
pub(crate) struct ProcessTree {
    #[cfg(windows)]
    child: Box<dyn process_wrap::std::ChildWrapper>,
    #[cfg(not(windows))]
    child: Child,
    terminated: bool,
}

impl ProcessTree {
    /// Spawn `command` as a supervised process-tree leader.
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(windows)]
        {
            use process_wrap::std::{CommandWrap, JobObject};

            // `JobObject` adds CREATE_SUSPENDED before spawning, assigns the
            // child to the Job without a descendant-creation race, then resumes
            // the child before returning.
            let command = std::mem::replace(command, Command::new(""));
            let mut wrapped = CommandWrap::from(command);
            wrapped.wrap(JobObject);
            wrapped.spawn().map(|child| Self {
                child,
                terminated: false,
            })
        }
        #[cfg(not(windows))]
        {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            command.spawn().map(|child| Self {
                child,
                terminated: false,
            })
        }
    }

    /// Take the direct child's captured stdout pipe.
    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        #[cfg(windows)]
        {
            self.child.stdout().take()
        }
        #[cfg(not(windows))]
        {
            self.child.stdout.take()
        }
    }

    /// Take the direct child's captured stderr pipe.
    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        #[cfg(windows)]
        {
            self.child.stderr().take()
        }
        #[cfg(not(windows))]
        {
            self.child.stderr.take()
        }
    }

    /// Return the direct child's status if it has exited.
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Wait for normal completion and disarm drop-time termination.
    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait()?;
        self.terminated = true;
        Ok(status)
    }

    /// Wait up to `timeout` for the direct child to exit.
    pub(crate) fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(Some(status));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep(WAIT_POLL_INTERVAL.min(deadline - now));
        }
    }

    /// Best-effort termination of the complete supervised tree.
    ///
    /// Every platform bounds both the termination request and direct-child
    /// reaping. A child stuck in an uninterruptible kernel wait must not hold a
    /// caller's higher-level queue or model lease forever.
    pub(crate) fn terminate(&mut self) {
        if self.terminated {
            return;
        }
        #[cfg(windows)]
        {
            // The wrapper retains the Job Object even if `try_wait` reaped the
            // direct child, so this still reaches surviving descendants. Use
            // the non-blocking request: synchronous Job termination can wait
            // indefinitely inside Windows while a descendant retains an I/O
            // handle, defeating every caller's outer timeout.
            if self.child.start_kill().is_ok() {
                self.terminated = true;
            }
        }
        #[cfg(not(windows))]
        {
            self.terminated = true;
        }
        #[cfg(unix)]
        {
            let Ok(mut terminator) = termination_command(self.child.id()).spawn() else {
                let _ = self.child.kill();
                let _ = self.wait_timeout(TERMINATION_REAP_TIMEOUT);
                return;
            };
            if !matches!(
                wait_child_until(
                    &mut terminator,
                    Instant::now() + TERMINATION_COMMAND_TIMEOUT,
                ),
                Ok(Some(_))
            ) {
                let _ = terminator.kill();
                let _ =
                    wait_child_until(&mut terminator, Instant::now() + TERMINATION_REAP_TIMEOUT);
            }
            let _ = self.child.kill();
            let _ = self.wait_timeout(TERMINATION_REAP_TIMEOUT);
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = self.child.kill();
            let _ = self.wait_timeout(TERMINATION_REAP_TIMEOUT);
        }
    }

    /// Best-effort complete-tree termination without waiting past `deadline`.
    pub(crate) fn terminate_until(&mut self, deadline: Instant) {
        if self.terminated {
            return;
        }
        #[cfg(windows)]
        {
            let _ = deadline;
            if self.child.start_kill().is_ok() {
                self.terminated = true;
            }
        }
        #[cfg(not(windows))]
        {
            self.terminated = true;
        }
        #[cfg(unix)]
        {
            if let Ok(mut terminator) = termination_command(self.child.id()).spawn() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if !remaining.is_zero()
                    && !matches!(wait_child_until(&mut terminator, deadline), Ok(Some(_)))
                {
                    let _ = terminator.kill();
                }
            } else {
                let _ = self.child.kill();
            }
            let _ = self.child.kill();
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                let _ = self.wait_timeout(remaining);
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = self.child.kill();
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                let _ = self.wait_timeout(remaining);
            }
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        // Fail closed on early returns between spawn and explicit cleanup.
        if !self.terminated {
            let _ = self.child.start_kill();
        }
    }
}

#[cfg(not(windows))]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        // Match the Windows Job boundary when a caller returns before its
        // normal explicit cleanup path.
        self.terminate();
    }
}

#[cfg(unix)]
fn wait_child_until(child: &mut Child, deadline: Instant) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(WAIT_POLL_INTERVAL.min(deadline - now));
    }
}

#[cfg(unix)]
fn termination_command(pid: u32) -> Command {
    use std::process::Stdio;

    let mut command = Command::new("kill");
    command
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::ffi::OsStr;

    #[cfg(unix)]
    use super::termination_command;

    #[cfg(unix)]
    #[test]
    fn bounded_output_times_out_hanging_leaf() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let started = Instant::now();
        let result = super::run_output_until(
            Command::new("sh").args(["-c", "sleep 30"]),
            Instant::now() + Duration::from_millis(150),
            "local tart probe",
        );

        assert!(matches!(
            result,
            Err(super::BoundedOutputError::TimedOut { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_output_rejects_post_exit_capture_over_limit() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let result = super::run_output_until(
            Command::new("sh").args(["-c", "head -c 8388609 /dev/zero"]),
            Instant::now() + Duration::from_secs(3),
            "oversize probe",
        );

        assert!(matches!(
            result,
            Err(super::BoundedOutputError::OutputLimit {
                stream: "stdout",
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_output_reaps_descendant_that_retains_capture() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().expect("tempdir");
        let pid_path = temp.path().join("descendant.pid");
        let output = super::run_output_until(
            Command::new("sh")
                .args([
                    "-c",
                    "sleep 30 & echo $! > \"$SHIPYARD_DESCENDANT_PID\"; printf ok",
                ])
                .env("SHIPYARD_DESCENDANT_PID", &pid_path),
            Instant::now() + Duration::from_secs(2),
            "descendant capture probe",
        )
        .expect("leader should finish without waiting on inherited stdout");
        assert_eq!(output.stdout, b"ok");
        let pid = std::fs::read_to_string(pid_path).expect("descendant pid");
        let process_is_running = || {
            Command::new("kill")
                .args(["-0", "--", pid.trim()])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_is_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_is_running(),
            "descendant retaining stdout survived probe teardown"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_output_stops_ssh_connect_then_stall() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().expect("tempdir");
        let connected = temp.path().join("connected");
        let started = Instant::now();
        let result = super::run_output_until(
            Command::new("sh")
                .args(["-c", "touch \"$SHIPYARD_SSH_CONNECTED\"; sleep 30"])
                .env("SHIPYARD_SSH_CONNECTED", &connected),
            Instant::now() + Duration::from_millis(200),
            "ssh capacity probe",
        );

        assert!(connected.exists(), "fixture must reach connected state");
        assert!(matches!(
            result,
            Err(super::BoundedOutputError::TimedOut { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_bounded_output_timeouts_keep_reaping_independent() {
        use std::process::Command;
        use std::sync::{Arc, Barrier};
        use std::time::{Duration, Instant};

        let barrier = Arc::new(Barrier::new(9));
        let fixtures = (0..8)
            .map(|_| {
                let temp = tempfile::tempdir().expect("tempdir");
                let pid_path = temp.path().join("descendant.pid");
                (temp, pid_path)
            })
            .collect::<Vec<_>>();
        let workers = fixtures
            .iter()
            .map(|(_, pid_path)| {
                let barrier = Arc::clone(&barrier);
                let pid_path = pid_path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    super::run_output_until(
                        Command::new("sh")
                            .args([
                                "-c",
                                "sleep 30 & echo $! > \"$SHIPYARD_DESCENDANT_PID\"; wait",
                            ])
                            .env("SHIPYARD_DESCENDANT_PID", &pid_path),
                        Instant::now() + Duration::from_secs(2),
                        "concurrent timeout probe",
                    )
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();

        let readiness_deadline = Instant::now() + Duration::from_secs(1);
        let descendant_pids = loop {
            let pids = fixtures
                .iter()
                .map(|(_, pid_path)| {
                    std::fs::read_to_string(pid_path)
                        .ok()
                        .filter(|pid| pid.trim().parse::<u32>().is_ok_and(|pid| pid > 0))
                })
                .collect::<Option<Vec<_>>>();
            if let Some(pids) = pids {
                break pids;
            }
            assert!(
                Instant::now() < readiness_deadline,
                "concurrent timeout fixtures did not all publish descendant identities"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let teardown_started = Instant::now();

        for worker in workers {
            assert!(matches!(
                worker.join().expect("timeout worker must not panic"),
                Err(super::BoundedOutputError::TimedOut { .. })
            ));
        }
        assert!(
            teardown_started.elapsed() < Duration::from_secs(5),
            "concurrent teardown serialized beyond its shared bounded window"
        );

        for ((_, _), pid) in fixtures.into_iter().zip(descendant_pids) {
            let process_is_running = || {
                Command::new("kill")
                    .args(["-0", "--", pid.trim()])
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while process_is_running() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                !process_is_running(),
                "concurrent timeout descendant {} survived teardown",
                pid.trim()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_termination_targets_the_child_process_group() {
        let command = termination_command(42);
        assert_eq!(command.get_program(), OsStr::new("kill"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-KILL", "--", "-42"].map(OsStr::new)
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_drop_terminates_the_child_process_group() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().expect("tempdir");
        let pid_path = temp.path().join("descendant.pid");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sleep 30 & echo $! > \"$SHIPYARD_DROP_TEST_PID\"; wait",
            ])
            .env("SHIPYARD_DROP_TEST_PID", &pid_path);
        let tree = super::ProcessTree::spawn(&mut command).expect("spawn process tree");
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_path)
                && let Ok(pid) = contents.trim().parse::<u32>()
                && pid > 0
            {
                break pid.to_string();
            }
            assert!(
                Instant::now() < deadline,
                "descendant pid was not recorded completely"
            );
            std::thread::sleep(Duration::from_millis(10));
        };

        drop(tree);

        let process_is_running = || {
            Command::new("kill")
                .args(["-0", "--", &pid])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_is_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_is_running(),
            "process-tree descendant {pid} survived drop"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_job_remains_usable_after_tree_leader_exits() {
        use std::io::Read;
        use std::process::{Command, Stdio};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let temp = tempfile::TempDir::new().expect("tempdir");
        let release = temp.path().join("release-root-helper");
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "process::tests::windows_process_tree_root_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("SHIPYARD_PROCESS_TREE_RELEASE", &release)
            .stdout(Stdio::piped());
        let mut tree = super::ProcessTree::spawn(&mut command).expect("spawn process tree");
        let mut stdout = tree.take_stdout().expect("root stdout");
        let (reader_sender, reader_receiver) = mpsc::sync_channel(1);
        let _reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
            let _ = reader_sender.send(result);
        });

        std::fs::write(&release, b"go").expect("release root helper");
        let root_deadline = Instant::now() + Duration::from_secs(5);
        let root_status = loop {
            if let Some(status) = tree.try_wait().expect("poll tree leader") {
                break status;
            }
            assert!(
                Instant::now() < root_deadline,
                "tree leader should exit while its descendant remains"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(root_status.success());
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            matches!(reader_receiver.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "the descendant should still hold the inherited pipe open"
        );

        tree.terminate();
        reader_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("terminating the retained Job Object should close the descendant pipe")
            .expect("read descendant stdout");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "subprocess helper for windows_job_remains_usable_after_tree_leader_exits"]
    #[allow(clippy::zombie_processes)]
    fn windows_process_tree_root_helper() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let release = std::env::var_os("SHIPYARD_PROCESS_TREE_RELEASE")
            .map(std::path::PathBuf::from)
            .expect("release path");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !release.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(release.exists(), "parent did not release root helper");
        // Deliberately leave the leaf running when this root exits: the parent
        // test proves the retained Job Object can still terminate that leaf.
        Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "process::tests::windows_process_tree_leaf_helper",
                "--ignored",
                "--nocapture",
            ])
            .spawn()
            .expect("spawn leaf helper");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "subprocess helper for windows_job_remains_usable_after_tree_leader_exits"]
    fn windows_process_tree_leaf_helper() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}
