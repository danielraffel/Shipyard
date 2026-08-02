//! Subprocess helpers that mark child processes as orchestrated by
//! Shipyard.
//!
//! When `shipyard pr` / `shipyard ship` / `shipyard auto-merge` /
//! `shipyard overflow` spawn `git` or `gh`, downstream consumers
//! (notably the Pulp pre-push hook in
//! [`danielraffel/pulp#1406`](https://github.com/danielraffel/pulp/pull/1406))
//! want to distinguish a supervised push from a raw `git push`. We
//! set `SHIPYARD_PR_RUNNING=1` on the child environment so the hook
//! can read it without the user having to remember a project-local
//! fallback like `PULP_VIA_SHIPYARD=1`.
//!
//! Scope discipline (see issue #266): only flows that participate in
//! the supervised PR / ship / merge pipeline route through these
//! helpers. Diagnostic subcommands (`doctor`, `pin`, `runner`,
//! `cleanup`, ad-hoc `cloud`) are intentionally left unmodified —
//! they are not "supervised pushes" and giving them the marker would
//! confuse the audit trail.

use std::path::Path;
use std::process::Command;

/// Environment-variable name set on every supervised subprocess.
///
/// Externally observable contract; do not rename without coordinating
/// with downstream consumers (see crate-level docs).
pub const SUPERVISED_ENV_VAR: &str = "SHIPYARD_PR_RUNNING";

/// Value set on every supervised subprocess.
pub const SUPERVISED_ENV_VALUE: &str = "1";

/// Default OpenSSH command for pushes whose local pre-push gate can run for
/// much longer than GitHub's idle SSH timeout.
pub const PUSH_SSH_KEEPALIVE_COMMAND: &str =
    "ssh -o ServerAliveInterval=30 -o ServerAliveCountMax=3";

/// Mark a `Command` as supervised. Returns the same command for
/// fluent chaining.
#[must_use]
pub fn supervised(mut command: Command) -> Command {
    command.env(SUPERVISED_ENV_VAR, SUPERVISED_ENV_VALUE);
    command
}

/// Build a `gh` invocation marked as supervised. Mirrors the
/// existing `gh(gh_command)` helper at `src/pr.rs:151` — pass
/// `Some(path)` from tests that inject a fake `gh` shim, `None`
/// for production code that should resolve `gh` on `$PATH`.
#[must_use]
pub fn gh_supervised(gh_command: Option<&Path>) -> Command {
    supervised(gh_command.map_or_else(|| Command::new("gh"), Command::new))
}

/// Build a `git` invocation marked as supervised.
#[must_use]
pub fn git_supervised() -> Command {
    supervised(Command::new("git"))
}

/// Build a supervised `git push` command with an SSH keepalive when the user
/// has not supplied a custom SSH command. Git opens the transport before it
/// invokes `pre-push`; without traffic, a long local gate can finish only to
/// discover that the remote closed the idle connection.
#[must_use]
pub fn git_push_supervised() -> Command {
    git_push_supervised_for(std::env::var_os("GIT_SSH_COMMAND").as_deref())
}

fn git_push_supervised_for(existing_ssh_command: Option<&std::ffi::OsStr>) -> Command {
    let mut command = git_supervised();
    if existing_ssh_command.is_none() {
        command.env("GIT_SSH_COMMAND", PUSH_SSH_KEEPALIVE_COMMAND);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn env_value(command: &Command, key: &str) -> Option<OsString> {
        command.get_envs().find_map(|(k, v)| {
            if k == key {
                v.map(std::ffi::OsStr::to_owned)
            } else {
                None
            }
        })
    }

    #[test]
    fn supervised_sets_marker_on_a_plain_command() {
        let cmd = supervised(Command::new("true"));
        assert_eq!(
            env_value(&cmd, SUPERVISED_ENV_VAR).as_deref(),
            Some(OsString::from("1").as_os_str())
        );
    }

    #[test]
    fn gh_supervised_uses_path_when_provided_and_sets_marker() {
        let fake = Path::new("/tmp/fake-gh");
        let cmd = gh_supervised(Some(fake));
        assert_eq!(cmd.get_program(), fake.as_os_str());
        assert_eq!(
            env_value(&cmd, SUPERVISED_ENV_VAR).as_deref(),
            Some(OsString::from("1").as_os_str())
        );
    }

    #[test]
    fn gh_supervised_falls_back_to_gh_on_path_when_none() {
        let cmd = gh_supervised(None);
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("gh"));
        assert_eq!(
            env_value(&cmd, SUPERVISED_ENV_VAR).as_deref(),
            Some(OsString::from("1").as_os_str())
        );
    }

    #[test]
    fn git_supervised_uses_git_and_sets_marker() {
        let cmd = git_supervised();
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("git"));
        assert_eq!(
            env_value(&cmd, SUPERVISED_ENV_VAR).as_deref(),
            Some(OsString::from("1").as_os_str())
        );
    }

    #[test]
    fn git_push_supervised_adds_idle_transport_keepalive() {
        let cmd = git_push_supervised_for(None);
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("git"));
        assert_eq!(
            env_value(&cmd, "GIT_SSH_COMMAND").as_deref(),
            Some(OsString::from(PUSH_SSH_KEEPALIVE_COMMAND).as_os_str())
        );
        assert_eq!(
            env_value(&cmd, SUPERVISED_ENV_VAR).as_deref(),
            Some(OsString::from(SUPERVISED_ENV_VALUE).as_os_str())
        );
    }

    #[test]
    fn git_push_supervised_preserves_explicit_ssh_policy() {
        let explicit = std::ffi::OsStr::new("ssh -i /tmp/operator-key");
        let cmd = git_push_supervised_for(Some(explicit));
        assert_eq!(env_value(&cmd, "GIT_SSH_COMMAND"), None);
    }

    // End-to-end: spawn a real subprocess and assert it sees the env.
    // Per-platform implementations because Windows runners may not have
    // `sh` on PATH and posix runners may not have `cmd`. Codex P1 on
    // shipyard PR #302 — the original unconditional `sh` spawn failed
    // the Windows CI matrix.
    #[cfg(unix)]
    #[test]
    fn marker_propagates_to_child_process_unix() {
        let output = supervised(Command::new("sh"))
            .args(["-c", "echo $SHIPYARD_PR_RUNNING"])
            .output()
            .expect("spawn sh");
        let stdout = String::from_utf8(output.stdout).expect("utf8");
        assert_eq!(stdout.trim(), SUPERVISED_ENV_VALUE);
    }

    #[cfg(windows)]
    #[test]
    fn marker_propagates_to_child_process_windows() {
        // `cmd /C` echoes the env var with `%VAR%` expansion.
        let output = supervised(Command::new("cmd"))
            .args(["/C", "echo %SHIPYARD_PR_RUNNING%"])
            .output()
            .expect("spawn cmd");
        let stdout = String::from_utf8(output.stdout).expect("utf8");
        assert_eq!(stdout.trim(), SUPERVISED_ENV_VALUE);
    }
}
