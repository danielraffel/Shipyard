//! A stable macOS privacy identity for the detached daemon.
//!
//! macOS privacy consent (TCC) is not granted to the process that touches a
//! protected resource but to its *responsible process*. A child inherits its
//! parent's responsible process; a launchd job is its own. For a command-line
//! binary the consent subject is the executable's real path, and every
//! Shipyard release installs under a fresh `auth-generations/<id>/` path. So a
//! daemon spawned by an unattended updater (itself a launchd job) is
//! attributed to a path macOS has never seen, and its first read of an
//! external volume raises a Removable Volumes prompt that nobody is there to
//! answer. The read blocks until someone does.
//!
//! The launcher breaks that chain. It is a copy of a signed Shipyard binary at
//! one stable path, started by a per-user launchd agent, and it stays resident
//! as the parent of the daemon (`shipyard daemon supervise`). The daemon and
//! everything it spawns are therefore attributed to the launcher path, whose
//! consent survives every update. The launcher itself must not `exec` the
//! release: the responsible process is a pid, and after `exec` that pid's path
//! is the new binary. Its child does exec: the release binary prepares the
//! daemon with its own spawn code and replaces itself with `daemon run`, so the
//! launcher copy freezes only a minimal hand-off, never the spawn invariants.
//!
//! The launcher is opt-in per host. `shipyard daemon launcher install` copies
//! the binary, runs one consent probe from launchd so the one-time prompt
//! appears while an operator is present, and only activates the launchd path
//! once the probe has read every requested volume. Without an activation
//! record the daemon is spawned directly, exactly as before.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::daemon_runtime::{
    DaemonSpawnFailedError, SpawnRequest, prepare_daemon_child, prepare_daemon_dirs,
};

/// Printed by `daemon supervise --contract`; names the argument contract the
/// launchd agent relies on.
pub const SUPERVISE_CONTRACT: &str = "shipyard-daemon-supervise-v1";
/// Activation record schema.
const RECORD_SCHEMA: u32 = 1;
/// File name of the launcher copy.
const LAUNCHER_FILE_NAME: &str = "shipyard-daemon-launcher";
/// Label prefix for the per-state-dir daemon agent.
const LABEL_PREFIX: &str = "com.danielraffel.shipyard.daemon";

/// Where the stable launcher copy lives for `home`.
#[must_use]
pub fn stable_launcher_path(home: &Path) -> PathBuf {
    home.join(".local/libexec/shipyard")
        .join(LAUNCHER_FILE_NAME)
}

/// Activation record path for a state root.
#[must_use]
pub fn record_path(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon").join("launcher.json")
}

/// The launchd label for the daemon agent of one state root. Derived from the
/// state root so a dev-mode or test state root never replaces the production
/// agent.
#[must_use]
pub fn launchd_label(state_dir: &Path) -> String {
    let digest = hex::encode(Sha256::digest(state_dir.as_os_str().as_encoded_bytes()));
    format!("{LABEL_PREFIX}.{}", &digest[..12])
}

/// Where launchd writes the launcher's own stdout/stderr. Outside the
/// protected state root, which only lease-holding writers may touch; the
/// daemon's log stays `daemon/daemon.log`.
#[must_use]
pub fn launcher_log_path(home: &Path) -> PathBuf {
    home.join("Library/Logs/shipyard-daemon-launcher.log")
}

/// The agent plist path for `label`.
#[must_use]
pub fn plist_path(home: &Path, label: &str) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{label}.plist"))
}

/// What `daemon launcher install` records once the consent probe succeeded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LauncherRecord {
    /// Record schema.
    pub schema: u32,
    /// Absolute path of the launcher copy.
    pub launcher_path: PathBuf,
    /// SHA-256 of the launcher copy when it was probed.
    pub launcher_sha256: String,
    /// launchd label of the daemon agent.
    pub label: String,
    /// Paths the consent probe read from launchd.
    pub probed_paths: Vec<PathBuf>,
    /// Shipyard version that installed the launcher.
    pub installed_by_version: String,
    /// RFC 3339 install time.
    pub installed_at: String,
}

/// A verified, active launcher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveLauncher {
    /// Launcher executable.
    pub path: PathBuf,
    /// launchd label of the daemon agent.
    pub label: String,
}

/// Read the activation record for `state_dir`.
#[must_use]
pub fn read_record(state_dir: &Path) -> Option<LauncherRecord> {
    let text = fs::read_to_string(record_path(state_dir)).ok()?;
    serde_json::from_str::<LauncherRecord>(&text)
        .ok()
        .filter(|record| record.schema == RECORD_SCHEMA)
}

/// The launcher to use for `state_dir`, if one is installed and still the
/// exact file that passed the consent probe. A replaced or missing file is
/// ignored rather than trusted, so the daemon falls back to a direct spawn.
#[must_use]
pub fn active_launcher(state_dir: &Path) -> Option<ActiveLauncher> {
    let record = read_record(state_dir)?;
    let metadata = fs::symlink_metadata(&record.launcher_path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    if sha256_file(&record.launcher_path).ok()? != record.launcher_sha256 {
        return None;
    }
    Some(ActiveLauncher {
        path: record.launcher_path,
        label: record.label,
    })
}

/// The launchd `ProgramArguments` that make `launcher` supervise the daemon
/// described by `request`. The daemon child receives the same argv a direct
/// spawn would, so fleet-update launch evidence is unchanged.
#[must_use]
pub fn launcher_arguments(launcher: &Path, request: &SpawnRequest) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = vec![
        launcher.into(),
        "--mode".into(),
        request.mode.as_str().into(),
    ];
    if let Some(global_dir) = &request.global_dir_override {
        arguments.push("--global-dir".into());
        arguments.push(global_dir.into());
    }
    if let Some(state_dir) = &request.state_dir_override {
        arguments.push("--state-dir".into());
        arguments.push(state_dir.into());
    }
    arguments.extend(["daemon".into(), "supervise".into(), "--exec".into()]);
    arguments.push(request.binary.clone().into());
    for repo in crate::daemon_runtime::normalize_repos(request.repos.clone()) {
        arguments.push("--repo".into());
        arguments.push(repo.into());
    }
    arguments
}

/// Render a launchd agent plist. `run_at_load` is only set for the one-shot
/// consent probe; the daemon agent is started explicitly with `kickstart`.
#[must_use]
pub fn render_plist(
    label: &str,
    program_arguments: &[OsString],
    log_path: &Path,
    home: &Path,
    path_env: &OsStr,
    run_at_load: bool,
) -> String {
    let mut out = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
        "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\">\n<dict>\n"
    ));
    let string = |out: &mut String, key: &str, value: &str| {
        let _ = write!(
            out,
            "\t<key>{}</key>\n\t<string>{}</string>\n",
            xml_escape(key),
            xml_escape(value)
        );
    };
    string(&mut out, "Label", label);
    out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for argument in program_arguments {
        let _ = writeln!(
            out,
            "\t\t<string>{}</string>",
            xml_escape(&argument.to_string_lossy())
        );
    }
    out.push_str("\t</array>\n");
    out.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
    for (key, value) in [("HOME", home.as_os_str()), ("PATH", path_env)] {
        let _ = write!(
            out,
            "\t\t<key>{key}</key>\n\t\t<string>{}</string>\n",
            xml_escape(&value.to_string_lossy())
        );
    }
    out.push_str("\t</dict>\n");
    string(&mut out, "WorkingDirectory", &home.to_string_lossy());
    string(&mut out, "StandardOutPath", &log_path.to_string_lossy());
    string(&mut out, "StandardErrorPath", &log_path.to_string_lossy());
    let boolean = |out: &mut String, key: &str, value: bool| {
        let _ = write!(
            out,
            "\t<key>{key}</key>\n\t<{}/>\n",
            if value { "true" } else { "false" }
        );
    };
    boolean(&mut out, "RunAtLoad", run_at_load);
    boolean(&mut out, "KeepAlive", false);
    // The daemon leads its own process group. If the launcher ever dies
    // first, launchd must not reap the daemon along with it.
    boolean(&mut out, "AbandonProcessGroup", true);
    out.push_str("</dict>\n</plist>\n");
    out
}

fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// `launchctl` operations the launcher needs, injectable for tests.
pub trait Launchctl {
    /// `launchctl bootout <domain>/<label>`; an absent job is not an error.
    fn bootout(&mut self, label: &str) -> Result<(), String>;
    /// `launchctl bootstrap <domain> <plist>`.
    fn bootstrap(&mut self, plist: &Path) -> Result<(), String>;
    /// `launchctl kickstart <domain>/<label>`.
    fn kickstart(&mut self, label: &str) -> Result<(), String>;
}

/// The real `launchctl`, targeting the invoking user's GUI domain.
pub struct SystemLaunchctl {
    domain: String,
}

impl SystemLaunchctl {
    /// The current user's `gui/<uid>` domain.
    #[must_use]
    pub fn for_current_user() -> Self {
        Self {
            domain: format!("gui/{}", nix::unistd::getuid().as_raw()),
        }
    }

    fn run(arguments: &[&OsStr]) -> Result<std::process::Output, String> {
        Command::new("/bin/launchctl")
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("failed to run launchctl: {error}"))
    }

    fn describe(action: &str, output: &std::process::Output) -> String {
        format!(
            "launchctl {action} exited {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

impl Launchctl for SystemLaunchctl {
    fn bootout(&mut self, label: &str) -> Result<(), String> {
        let target = format!("{}/{label}", self.domain);
        // Exit 3 / 113 mean the job is not loaded, which is the goal.
        let output = Self::run(&["bootout".as_ref(), target.as_ref()])?;
        match output.status.code() {
            Some(0 | 3 | 113) => Ok(()),
            _ => Err(Self::describe("bootout", &output)),
        }
    }

    fn bootstrap(&mut self, plist: &Path) -> Result<(), String> {
        let output = Self::run(&[
            "bootstrap".as_ref(),
            self.domain.as_ref(),
            plist.as_os_str(),
        ])?;
        output
            .status
            .success()
            .then_some(())
            .ok_or_else(|| Self::describe("bootstrap", &output))
    }

    fn kickstart(&mut self, label: &str) -> Result<(), String> {
        let target = format!("{}/{label}", self.domain);
        let output = Self::run(&["kickstart".as_ref(), target.as_ref()])?;
        output
            .status
            .success()
            .then_some(())
            .ok_or_else(|| Self::describe("kickstart", &output))
    }
}

/// Start the daemon for `request` through the launchd agent of `launcher`.
pub fn start_via_launchd(request: &SpawnRequest, launcher: &ActiveLauncher) -> Result<(), String> {
    start_via_launchd_with(
        request,
        launcher,
        &crate::paths::home_dir(),
        &mut SystemLaunchctl::for_current_user(),
    )
}

/// [`start_via_launchd`] with an explicit home and `launchctl`.
pub fn start_via_launchd_with(
    request: &SpawnRequest,
    launcher: &ActiveLauncher,
    home: &Path,
    launchctl: &mut dyn Launchctl,
) -> Result<(), String> {
    let plist = plist_path(home, &launcher.label);
    let rendered = render_plist(
        &launcher.label,
        &launcher_arguments(&launcher.path, request),
        &launcher_log_path(home),
        home,
        &crate::paths::unattended_tool_path(),
        false,
    );
    // Unload first: bootstrap refuses a label that is already loaded, and a
    // stale definition would restart the daemon with old arguments.
    launchctl.bootout(&launcher.label)?;
    write_file_atomically(&plist, rendered.as_bytes(), 0o644)
        .map_err(|error| format!("failed to write {}: {error}", plist.display()))?;
    launchctl.bootstrap(&plist)?;
    launchctl.kickstart(&launcher.label)
}

/// Arguments for the in-place step: `request.binary`, run by the resident
/// launcher, prepares the daemon with *its own* (current release) spawn code
/// and then replaces itself with `daemon run`. Only this minimal hand-off is
/// frozen in the launcher copy; log rotation, PATH, TMPDIR, and the stdio
/// fence always come from the release being started.
#[must_use]
pub fn in_place_arguments(request: &SpawnRequest) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = vec!["--mode".into(), request.mode.as_str().into()];
    if let Some(global_dir) = &request.global_dir_override {
        arguments.push("--global-dir".into());
        arguments.push(global_dir.into());
    }
    if let Some(state_dir) = &request.state_dir_override {
        arguments.push("--state-dir".into());
        arguments.push(state_dir.into());
    }
    arguments.extend([
        "daemon".into(),
        "supervise".into(),
        "--in-place".into(),
        "--exec".into(),
    ]);
    arguments.push(request.binary.clone().into());
    for repo in crate::daemon_runtime::normalize_repos(request.repos.clone()) {
        arguments.push("--repo".into());
        arguments.push(repo.into());
    }
    arguments
}

/// Replace this process with the daemon for `request`: the in-place step.
/// The pid, and therefore the privacy responsibility inherited from the
/// resident launcher, carries over to `daemon run`. Returns only on failure.
#[must_use]
pub fn exec_daemon_in_place(request: &SpawnRequest) -> DaemonSpawnFailedError {
    use std::os::unix::process::CommandExt;

    let (daemon_dir, temp_dir) = match prepare_daemon_dirs(&request.state_dir) {
        Ok(dirs) => dirs,
        Err(error) => return error,
    };
    match prepare_daemon_child(request, &daemon_dir, &temp_dir) {
        Ok(mut command) => {
            DaemonSpawnFailedError(format!("failed to exec daemon: {}", command.exec()))
        }
        Err(error) => error,
    }
}

/// Run the daemon for `request` as a child and wait for it. This is the
/// launchd agent's program: staying resident keeps this stable executable
/// the daemon's responsible process. The child is `request.binary` in its
/// in-place step (see [`in_place_arguments`]). SIGTERM, SIGINT, and SIGHUP
/// are forwarded to the daemon as SIGTERM. Returns the exit code to use.
pub fn supervise(request: &SpawnRequest) -> Result<i32, DaemonSpawnFailedError> {
    use nix::sys::signal::{SigSet, Signal, kill};
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    let mut command = Command::new(&request.binary);
    command
        .args(in_place_arguments(request))
        .env("PATH", crate::paths::unattended_tool_path())
        .stdin(Stdio::null());
    command.process_group(0);
    // std passes the parent's signal mask to the child, so spawn before
    // blocking anything: a daemon born with SIGTERM blocked could never be
    // stopped.
    let mut child = command
        .spawn()
        .map_err(|error| DaemonSpawnFailedError(format!("failed to spawn daemon: {error}")))?;
    let child_pid = nix::unistd::Pid::from_raw(
        i32::try_from(child.id())
            .map_err(|_| DaemonSpawnFailedError("child pid overflow".to_owned()))?,
    );
    let mut forwarded = SigSet::empty();
    for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
        forwarded.add(signal);
    }
    // Blocked on this (the only) thread before the forwarding thread exists,
    // so that thread inherits the mask and is the one `sigwait` hands them to.
    forwarded
        .thread_block()
        .map_err(|error| DaemonSpawnFailedError(format!("failed to block signals: {error}")))?;
    thread::spawn(move || {
        while forwarded.wait().is_ok() {
            let _ = kill(child_pid, Signal::SIGTERM);
        }
    });
    let status = child
        .wait()
        .map_err(|error| DaemonSpawnFailedError(format!("failed to wait for daemon: {error}")))?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
}

/// Outcome of the consent probe for one path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProbeEntry {
    /// Path that was read.
    pub path: PathBuf,
    /// Whether the directory could be listed.
    pub ok: bool,
    /// The error when it could not.
    pub error: Option<String>,
}

/// Read each of `paths` (list one entry) and write the outcome to `result`.
/// This runs as a launchd job so the read is attributed to the launcher path.
pub fn probe(paths: &[PathBuf], result: &Path) -> io::Result<bool> {
    let entries: Vec<ProbeEntry> = paths
        .iter()
        .map(
            |path| match fs::read_dir(path).and_then(|mut dir| dir.next().transpose()) {
                Ok(_) => ProbeEntry {
                    path: path.clone(),
                    ok: true,
                    error: None,
                },
                Err(error) => ProbeEntry {
                    path: path.clone(),
                    ok: false,
                    error: Some(error.to_string()),
                },
            },
        )
        .collect();
    let ok = !entries.is_empty() && entries.iter().all(|entry| entry.ok);
    let body = serde_json::to_vec(&entries).map_err(io::Error::other)?;
    write_file_atomically(result, &body, 0o600)?;
    Ok(ok)
}

/// Default consent-probe paths: the root of the external volume holding the
/// current directory, when it is under `/Volumes`.
#[must_use]
pub fn default_probe_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut components = cwd.components();
    let (Some(root), Some(volumes), Some(name)) =
        (components.next(), components.next(), components.next())
    else {
        return Vec::new();
    };
    if root.as_os_str() != "/" || volumes.as_os_str() != "Volumes" {
        return Vec::new();
    }
    let volume = Path::new("/Volumes").join(name.as_os_str());
    // `/Volumes/Macintosh HD` is a symlink to the boot volume.
    match fs::symlink_metadata(&volume) {
        Ok(metadata) if metadata.is_dir() => vec![volume],
        _ => Vec::new(),
    }
}

/// Everything `daemon launcher install` needs, injectable for tests.
pub struct InstallPlan<'a> {
    /// Binary to copy (the running Shipyard, resolved).
    pub source: PathBuf,
    /// Home directory.
    pub home: PathBuf,
    /// State root the daemon uses.
    pub state_dir: PathBuf,
    /// Arguments that select the runtime mode and directories, forwarded to
    /// the probe so it reads the same configuration as the daemon would.
    pub mode_arguments: Vec<OsString>,
    /// Volumes the probe must read.
    pub probe_paths: Vec<PathBuf>,
    /// How long to wait for the probe (and any consent prompt).
    pub wait: Duration,
    /// `launchctl`.
    pub launchctl: &'a mut dyn Launchctl,
}

/// Install the launcher: copy, probe from launchd, then activate.
pub fn install(
    plan: &mut InstallPlan<'_>,
    progress: &mut dyn Write,
) -> Result<LauncherRecord, String> {
    if plan.probe_paths.is_empty() {
        return Err(
            "no volume to probe: run from a checkout on the external volume or pass --probe-path /Volumes/<name>"
                .to_owned(),
        );
    }
    let launcher = stable_launcher_path(&plan.home);
    let source_sha = sha256_file(&plan.source)
        .map_err(|error| format!("failed to hash {}: {error}", plan.source.display()))?;
    let current_sha = sha256_file(&launcher).ok();
    if current_sha.as_deref() != Some(source_sha.as_str()) {
        copy_executable(&plan.source, &launcher)
            .map_err(|error| format!("failed to install {}: {error}", launcher.display()))?;
    }
    let launcher_sha = sha256_file(&launcher)
        .map_err(|error| format!("failed to hash {}: {error}", launcher.display()))?;

    let label = launchd_label(&plan.state_dir);
    let probe_label = format!("{label}.consent-probe");
    // The probe's plist and result live in a private scratch directory: a
    // RunAtLoad plist must never sit in LaunchAgents, and the state root is
    // reserved for lease-holding writers.
    let scratch = tempfile::Builder::new()
        .prefix("shipyard-launcher-probe-")
        .tempdir()
        .map_err(|error| format!("failed to create probe scratch directory: {error}"))?;
    let result_path = scratch.path().join("result.json");
    let probe_plist = scratch.path().join("probe.plist");
    let mut arguments: Vec<OsString> = vec![launcher.clone().into()];
    arguments.extend(plan.mode_arguments.iter().cloned());
    arguments.extend(["daemon".into(), "launcher-probe".into()]);
    for path in &plan.probe_paths {
        arguments.push("--path".into());
        arguments.push(path.into());
    }
    arguments.push("--result".into());
    arguments.push(result_path.clone().into());
    let rendered = render_plist(
        &probe_label,
        &arguments,
        &launcher_log_path(&plan.home),
        &plan.home,
        &crate::paths::unattended_tool_path(),
        true,
    );
    write_file_atomically(&probe_plist, rendered.as_bytes(), 0o600)
        .map_err(|error| format!("failed to write {}: {error}", probe_plist.display()))?;
    plan.launchctl.bootout(&probe_label)?;
    plan.launchctl.bootstrap(&probe_plist)?;
    let _ = writeln!(
        progress,
        "Probing {} from launchd as {}. If macOS shows a privacy prompt for that path, approve it; waiting up to {}s.",
        plan.probe_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        launcher.display(),
        plan.wait.as_secs()
    );
    let deadline = Instant::now() + plan.wait;
    let entries = loop {
        if let Some(entries) = read_probe_result(&result_path) {
            break Some(entries);
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(200));
    };
    let _ = plan.launchctl.bootout(&probe_label);
    drop(scratch);
    let Some(entries) = entries else {
        return Err(format!(
            "the launchd consent probe did not finish within {}s. A privacy prompt for {} may be waiting on this Mac's desktop; approve it and rerun `shipyard daemon launcher install`. The daemon keeps its current launch path until then.",
            plan.wait.as_secs(),
            launcher.display()
        ));
    };
    if let Some(failed) = entries.iter().find(|entry| !entry.ok) {
        return Err(format!(
            "the launchd consent probe could not read {}: {}. Allow {} under System Settings > Privacy & Security > Files and Folders (Removable Volumes) and rerun.",
            failed.path.display(),
            failed.error.as_deref().unwrap_or("unknown error"),
            launcher.display()
        ));
    }
    let record = LauncherRecord {
        schema: RECORD_SCHEMA,
        launcher_path: launcher,
        launcher_sha256: launcher_sha,
        label,
        probed_paths: plan.probe_paths.clone(),
        installed_by_version: env!("CARGO_PKG_VERSION").to_owned(),
        installed_at: chrono::Utc::now().to_rfc3339(),
    };
    let body = serde_json::to_vec_pretty(&record).map_err(|error| error.to_string())?;
    let record_file = record_path(&plan.state_dir);
    let _lease = crate::writer_domain_lease::acquire_for_protected_path(&record_file)
        .map_err(|error| format!("failed to acquire the state writer lease: {error}"))?;
    write_file_atomically(&record_file, &body, 0o600)
        .map_err(|error| format!("failed to write activation record: {error}"))?;
    Ok(record)
}

/// Deactivate the launcher: unload the daemon agent and remove its plist and
/// the activation record. The launcher copy is left in place so reinstalling
/// keeps the consent macOS already recorded for its path. A running daemon is
/// not stopped here; `daemon refresh` afterwards spawns it directly.
pub fn uninstall(state_dir: &Path, home: &Path) -> Result<bool, String> {
    let label =
        read_record(state_dir).map_or_else(|| launchd_label(state_dir), |record| record.label);
    let existed = record_path(state_dir).exists();
    // Removing the record first means a concurrent refresh can no longer pick
    // the launchd path.
    let _lease = crate::writer_domain_lease::acquire_for_protected_path(&record_path(state_dir))
        .map_err(|error| format!("failed to acquire the state writer lease: {error}"))?;
    match fs::remove_file(record_path(state_dir)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("failed to remove activation record: {error}")),
    }
    let plist = plist_path(home, &label);
    if plist.exists() {
        // bootout would terminate a running launcher and, through it, the
        // daemon; leave a loaded agent alone and only drop the definition.
        fs::remove_file(&plist)
            .map_err(|error| format!("failed to remove {}: {error}", plist.display()))?;
    }
    Ok(existed)
}

fn read_probe_result(path: &Path) -> Option<Vec<ProbeEntry>> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn copy_executable(source: &Path, destination: &Path) -> io::Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("launcher path has no parent"))?;
    fs::create_dir_all(parent)?;
    let staged = parent.join(format!(".{LAUNCHER_FILE_NAME}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&staged);
    fs::copy(source, &staged)?;
    set_mode(&staged, 0o700)?;
    fs::rename(&staged, destination)
}

fn write_file_atomically(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::other("path has no file name"))?;
    let staged = parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    {
        let mut file = fs::File::create(&staged)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    set_mode(&staged, mode)?;
    fs::rename(&staged, path)
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests;
