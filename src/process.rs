//! Cross-platform child-process tree supervision.

use std::io;
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus};
use std::time::{Duration, Instant};

#[cfg(not(windows))]
use std::process::Child;
#[cfg(unix)]
use wait_timeout::ChildExt;

#[cfg(unix)]
const TERMINATION_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

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
    /// Unix reaps synchronously behind a bounded signal-command wait. Windows
    /// requests Job Object termination without entering its unbounded wait;
    /// pipe readers provide the bounded observation that descendants exited.
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
                let _ = self.child.wait();
                return;
            };
            if !matches!(
                terminator.wait_timeout(TERMINATION_COMMAND_TIMEOUT),
                Ok(Some(_))
            ) {
                let _ = terminator.kill();
                let _ = terminator.wait();
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
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
        while !pid_path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(pid_path.exists(), "descendant pid was not recorded");
        let pid = std::fs::read_to_string(&pid_path)
            .expect("descendant pid")
            .trim()
            .to_owned();

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
