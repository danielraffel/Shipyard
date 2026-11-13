//! Route launchd jobs through `ShipyardLauncher.app` once a host activates it.
//!
//! macOS privacy consent (TCC) belongs to a process's responsible process, and
//! a launchd job is its own. A job whose program is the Shipyard release
//! binary is identified by that binary's path, which changes with every
//! release (`auth-generations/<id>/shipyard`), so every install makes it a new
//! identity that raises a new "access removable volume" prompt. The launcher
//! bundle (`tools/shipyard-launcher`) is one identity across releases: it
//! verifies the current release and spawns it as a child.
//!
//! Routing happens only after `shipyard-launcher-check --activate` confirmed
//! the launcher holds Full Disk Access and wrote the activation record. A
//! launcher without a grant would raise its own prompt, so without the record
//! nothing changes.
//!
//! Two entry points use it:
//! - [`route_launchd_job`]: a launchd job whose program is this binary (for
//!   example `com.danielraffel.shipyard.fleet-reconcile`) replaces itself with
//!   the launcher. `exec` keeps the pid, and the pid is the job's responsible
//!   process, so the launcher bundle becomes the job's identity.
//! - The daemon agent plist (`daemon_launcher::start_via_launchd_with`) names
//!   the launcher as its program.

use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

/// Set by the launcher in its child's environment; its presence means this
/// process already runs under the launcher.
pub const LAUNCHER_ENV: &str = "SHIPYARD_LAUNCHER_APP";
/// Turns routing off for one process (`off`).
pub const ROUTING_ENV: &str = "SHIPYARD_LAUNCHER_ROUTING";
/// How long `--launcher-verify` may take before routing gives up on it.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);

/// The installed bundle for `home`.
#[must_use]
pub fn app_path(home: &Path) -> PathBuf {
    home.join(".local/libexec/ShipyardLauncher.app")
}

/// The bundle's executable.
#[must_use]
pub fn executable_path(home: &Path) -> PathBuf {
    app_path(home).join("Contents/MacOS/shipyard-launcher")
}

/// The activation record `shipyard-launcher-check --activate` writes.
#[must_use]
pub fn activation_path(home: &Path) -> PathBuf {
    home.join(".local/share/shipyard/launcher/activation.json")
}

/// Where both the launcher and this routing log one line per launch.
#[must_use]
pub fn log_path(home: &Path) -> PathBuf {
    home.join("Library/Logs/shipyard-launcher.log")
}

/// Why a launchd job or the daemon ran Shipyard directly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Direct {
    /// No activation record: the host never confirmed the launcher's grant.
    NotActivated,
    /// Activated, but the bundle executable is missing or not a plain file.
    Missing,
    /// The launcher refused the current release or could not run.
    VerifyFailed(String),
}

impl Direct {
    /// A short `reason=` token for logs.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::NotActivated => "not_activated".to_owned(),
            Self::Missing => "launcher_missing".to_owned(),
            Self::VerifyFailed(detail) => format!("verify_failed: {detail}"),
        }
    }
}

/// The launcher executable to route through, or why not. Activation is a
/// schema-1 record; the executable must be a regular file (not a symlink);
/// `verify` must accept it (in production, `--launcher-verify`).
pub fn active_executable(
    home: &Path,
    verify: &dyn Fn(&Path) -> Result<(), String>,
) -> Result<PathBuf, Direct> {
    let record = fs::read_to_string(activation_path(home)).map_err(|_| Direct::NotActivated)?;
    let schema = serde_json::from_str::<serde_json::Value>(&record)
        .ok()
        .and_then(|value| value.get("schema").and_then(serde_json::Value::as_u64));
    if schema != Some(1) {
        return Err(Direct::NotActivated);
    }
    let executable = executable_path(home);
    match fs::symlink_metadata(&executable) {
        Ok(metadata) if metadata.is_file() => {}
        _ => return Err(Direct::Missing),
    }
    verify(&executable).map_err(Direct::VerifyFailed)?;
    Ok(executable)
}

/// Run `<launcher> --launcher-verify`: the bundle's own signature, the
/// current release's resolution and its Developer ID signature, without
/// running the release.
pub fn launcher_verify(executable: &Path) -> Result<(), String> {
    let mut child = Command::new(executable)
        .arg("--launcher-verify")
        .env_remove(LAUNCHER_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", executable.display()))?;
    let Some(status) = child
        .wait_timeout(VERIFY_TIMEOUT)
        .map_err(|error| error.to_string())?
    else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("timed out after {}s", VERIFY_TIMEOUT.as_secs()));
    };
    if status.success() {
        return Ok(());
    }
    let mut detail = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut stderr, &mut detail);
    }
    Err(format!(
        "exit {}: {}",
        status.code().unwrap_or(-1),
        detail.trim()
    ))
}

/// What a starting process knows about itself.
#[derive(Clone, Debug, Default)]
pub struct JobFacts {
    /// Parent pid.
    pub ppid: i32,
    /// `XPC_SERVICE_NAME`: launchd sets it to the job label.
    pub xpc_service_name: Option<String>,
    /// [`LAUNCHER_ENV`] is set.
    pub under_launcher: bool,
    /// [`ROUTING_ENV`] is `off`.
    pub routing_off: bool,
    /// This executable, canonical.
    pub current_exe: Option<PathBuf>,
    /// What `~/.local/bin/shipyard` resolves to, canonical: the only binary
    /// the launcher runs.
    pub pointer_target: Option<PathBuf>,
    /// This is the daemon itself (`daemon run`) or its in-place hand-off. A
    /// daemon orphaned before its first instruction has parent 1 and an
    /// inherited `XPC_SERVICE_NAME`, yet must keep the pid its spawner waits on.
    pub daemon_internal: bool,
}

impl JobFacts {
    /// Facts for this process.
    #[must_use]
    pub fn current(home: &Path) -> Self {
        Self {
            ppid: nix::unistd::getppid().as_raw(),
            xpc_service_name: std::env::var("XPC_SERVICE_NAME").ok(),
            under_launcher: std::env::var_os(LAUNCHER_ENV).is_some(),
            routing_off: std::env::var(ROUTING_ENV).is_ok_and(|value| value == "off"),
            current_exe: std::env::current_exe()
                .ok()
                .and_then(|path| fs::canonicalize(path).ok()),
            pointer_target: fs::canonicalize(home.join(".local/bin/shipyard")).ok(),
            daemon_internal: is_daemon_internal(std::env::args_os().skip(1)),
        }
    }

    /// This process is the program launchd started for a job: its parent is
    /// launchd and launchd named the job. Children of a job keep the name but
    /// not the parent; they already inherit the job's responsible process.
    #[must_use]
    pub fn is_launchd_job(&self) -> bool {
        self.ppid == 1
            && self
                .xpc_service_name
                .as_deref()
                .is_some_and(|name| !name.is_empty() && name != "0")
    }

    /// Whether routing applies: a launchd job, not already under the launcher,
    /// not switched off, and running the very binary the launcher would run
    /// (so routing never swaps one Shipyard for another).
    #[must_use]
    pub fn wants_routing(&self) -> bool {
        self.is_launchd_job()
            && !self.under_launcher
            && !self.routing_off
            && self.current_exe.is_some()
            && !self.daemon_internal
            && self.current_exe == self.pointer_target
    }
}

/// `daemon run`, or `daemon supervise --in-place`.
#[must_use]
pub fn is_daemon_internal(arguments: impl Iterator<Item = OsString>) -> bool {
    let arguments: Vec<OsString> = arguments.collect();
    arguments.iter().any(|argument| argument == "--in-place")
        || arguments
            .windows(2)
            .any(|pair| pair[0] == "daemon" && pair[1] == "run")
}

/// When this process is a launchd job's own Shipyard program and the host
/// activated the launcher, replace this process with the launcher, same
/// arguments. Returns only when it ran nothing: routing does not apply, or
/// the launcher is unavailable (logged, and Shipyard continues directly).
pub fn route_launchd_job() {
    // Every invocation passes here; only a launchd job's own program (parent
    // pid 1) goes on to touch the file system.
    if nix::unistd::getppid().as_raw() != 1 {
        return;
    }
    let home = crate::paths::home_dir();
    let facts = JobFacts::current(&home);
    if !facts.wants_routing() {
        return;
    }
    let label = facts.xpc_service_name.clone().unwrap_or_default();
    match active_executable(&home, &launcher_verify) {
        Ok(executable) => {
            log(&home, &format!("job={label} launcher=app"));
            let error = exec_launcher(&executable);
            log(
                &home,
                &format!("job={label} launcher=direct reason=exec_failed: {error}"),
            );
        }
        Err(Direct::NotActivated) => {}
        Err(direct) => log(
            &home,
            &format!("job={label} launcher=direct reason={}", direct.reason()),
        ),
    }
}

fn exec_launcher(executable: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    Command::new(executable).args(arguments).exec()
}

/// Append one line to the launcher log. Best effort.
pub fn log(home: &Path, line: &str) {
    let path = log_path(home);
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = writeln!(
        file,
        "{} pid={} shipyard {line}",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        std::process::id()
    );
}

#[cfg(test)]
mod tests;
