use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::identity::{ProductIdentity, RuntimeMode};
use crate::platform::Platform;

/// Resolved runtime paths for a selected mode.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimePaths {
    /// Human-readable runtime mode.
    pub mode: &'static str,
    /// CLI binary name.
    pub binary_name: &'static str,
    /// Tracked per-repo configuration directory name.
    pub tracked_project_dir_name: &'static str,
    /// Private per-repo overlay directory name.
    pub local_overlay_dir_name: &'static str,
    /// Machine-global configuration directory.
    pub global_dir: PathBuf,
    /// Machine-global state directory.
    pub state_dir: PathBuf,
    /// Daemon runtime directory.
    pub daemon_dir: PathBuf,
    /// Daemon IPC socket path.
    pub daemon_socket: PathBuf,
    /// Daemon PID file path.
    pub daemon_pid_file: PathBuf,
    /// Daemon log file path.
    pub daemon_log_file: PathBuf,
}

impl RuntimePaths {
    /// Resolve runtime paths for the current platform and selected mode.
    #[must_use]
    pub fn current(mode: RuntimeMode) -> Self {
        Self::current_with_overrides(mode, None, None)
    }

    /// Resolve runtime paths for the current platform with optional overrides.
    #[must_use]
    pub fn current_with_overrides(
        mode: RuntimeMode,
        global_dir_override: Option<PathBuf>,
        state_dir_override: Option<PathBuf>,
    ) -> Self {
        let home_dir = home_dir();
        let mut paths = Self::for_platform(Platform::current(), &home_dir, mode);
        if let Some(global_dir_override) = global_dir_override {
            paths.global_dir = global_dir_override;
        }
        if let Some(state_dir_override) = state_dir_override {
            paths.state_dir = state_dir_override;
            paths.daemon_dir = paths.state_dir.join("daemon");
            paths.daemon_socket = paths.daemon_dir.join("daemon.sock");
            paths.daemon_pid_file = paths.daemon_dir.join("daemon.pid");
            paths.daemon_log_file = paths.daemon_dir.join("daemon.log");
        }
        paths
    }

    /// Pure path-resolution helper used by tests.
    #[must_use]
    pub fn for_platform(platform: Platform, home_dir: &Path, mode: RuntimeMode) -> Self {
        let identity = ProductIdentity::for_mode(mode);

        let global_dir = match platform {
            Platform::MacOs => home_dir
                .join("Library")
                .join("Application Support")
                .join(identity.config_stem),
            Platform::Linux => home_dir.join(".config").join(identity.config_stem),
            Platform::Windows => home_dir
                .join("AppData")
                .join("Local")
                .join(identity.config_stem),
        };

        let state_dir = match platform {
            Platform::MacOs => home_dir
                .join("Library")
                .join("Application Support")
                .join(identity.state_stem),
            Platform::Linux => home_dir
                .join(".local")
                .join("state")
                .join(identity.state_stem),
            Platform::Windows => home_dir
                .join("AppData")
                .join("Local")
                .join(identity.state_stem),
        };

        let daemon_dir = state_dir.join("daemon");
        let daemon_socket = daemon_dir.join("daemon.sock");
        let daemon_pid_file = daemon_dir.join("daemon.pid");
        let daemon_log_file = daemon_dir.join("daemon.log");

        Self {
            mode: mode.as_str(),
            binary_name: identity.binary_name,
            tracked_project_dir_name: identity.tracked_project_dir_name,
            local_overlay_dir_name: identity.local_overlay_dir_name,
            global_dir,
            state_dir,
            daemon_dir,
            daemon_socket,
            daemon_pid_file,
            daemon_log_file,
        }
    }
}

pub(crate) fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A deterministic PATH for unattended Shipyard children.
///
/// Non-login SSH and launchd commonly omit Homebrew and `~/.local/bin`, even
/// though those locations contain the configured `ghapp`, Tart, and Shipyard
/// binaries. Use only canonical locations: inherited entries may point into a
/// repository or caller-controlled wrapper directory. Callers no longer need
/// to hand-assemble PATH before refreshing a daemon.
#[must_use]
pub(crate) fn unattended_tool_path() -> OsString {
    let entries = unattended_tool_entries();
    env::join_paths(entries).unwrap_or_default()
}

fn unattended_tool_entries() -> Vec<PathBuf> {
    let mut entries = Vec::new();
    #[cfg(unix)]
    {
        entries.extend([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
            home_dir().join(".local/bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
            PathBuf::from("/usr/sbin"),
            PathBuf::from("/sbin"),
        ]);
    }
    #[cfg(windows)]
    {
        if let Some(system_root) = env::var_os("SystemRoot").map(PathBuf::from) {
            entries.push(system_root.join("System32"));
        }
        if let Some(program_files) = env::var_os("ProgramFiles").map(PathBuf::from) {
            entries.extend([
                program_files.join("Git/bin"),
                program_files.join("Git/usr/bin"),
                program_files.join("GitHub CLI"),
            ]);
        }
        entries.push(home_dir().join(".local/bin"));
    }
    let mut seen = BTreeSet::new();
    entries.retain(|entry| seen.insert(entry.clone()));
    entries
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::RuntimePaths;
    use crate::identity::RuntimeMode;
    use crate::platform::Platform;

    #[cfg(unix)]
    #[test]
    fn unattended_path_restores_canonical_non_login_tool_locations() {
        let path = super::unattended_tool_path();
        let entries = std::env::split_paths(&path).collect::<Vec<_>>();
        assert!(entries.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(entries.contains(&super::home_dir().join(".local/bin")));
        assert!(entries.contains(&PathBuf::from("/usr/bin")));
        assert_eq!(entries, super::unattended_tool_entries());
        assert!(!entries.contains(&PathBuf::from("/tmp/repository/bin")));
    }

    #[test]
    fn isolated_macos_paths_do_not_collide_with_shipyard_defaults() {
        let home = PathBuf::from("/Users/daniel");
        let paths = RuntimePaths::for_platform(Platform::MacOs, &home, RuntimeMode::Isolated);

        assert_eq!(paths.binary_name, "shipyard");
        assert_eq!(paths.tracked_project_dir_name, ".shipyard");
        assert_eq!(paths.local_overlay_dir_name, ".shipyard-dev.local");
        assert_eq!(
            paths.global_dir,
            PathBuf::from("/Users/daniel/Library/Application Support/shipyard-dev")
        );
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/Users/daniel/Library/Application Support/shipyard-dev")
        );
        assert_eq!(
            paths.daemon_socket,
            PathBuf::from(
                "/Users/daniel/Library/Application Support/shipyard-dev/daemon/daemon.sock"
            )
        );
    }

    #[test]
    fn shipyard_mode_matches_current_macos_contract() {
        let home = PathBuf::from("/Users/daniel");
        let paths = RuntimePaths::for_platform(Platform::MacOs, &home, RuntimeMode::Shipyard);

        assert_eq!(paths.binary_name, "shipyard");
        assert_eq!(paths.local_overlay_dir_name, ".shipyard.local");
        assert_eq!(
            paths.global_dir,
            PathBuf::from("/Users/daniel/Library/Application Support/shipyard")
        );
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/Users/daniel/Library/Application Support/shipyard")
        );
        assert_eq!(
            paths.daemon_socket,
            PathBuf::from("/Users/daniel/Library/Application Support/shipyard/daemon/daemon.sock")
        );
    }

    #[test]
    fn linux_paths_split_config_from_state() {
        let home = PathBuf::from("/home/daniel");
        let paths = RuntimePaths::for_platform(Platform::Linux, &home, RuntimeMode::Shipyard);

        assert_eq!(
            paths.global_dir,
            PathBuf::from("/home/daniel/.config/shipyard")
        );
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/home/daniel/.local/state/shipyard")
        );
    }

    #[test]
    fn windows_paths_use_local_appdata_for_both_roots() {
        let home = PathBuf::from("C:/Users/daniel");
        let paths = RuntimePaths::for_platform(Platform::Windows, &home, RuntimeMode::Shipyard);

        assert_eq!(
            paths.global_dir,
            PathBuf::from("C:/Users/daniel/AppData/Local/shipyard")
        );
        assert_eq!(
            paths.state_dir,
            PathBuf::from("C:/Users/daniel/AppData/Local/shipyard")
        );
    }
}
