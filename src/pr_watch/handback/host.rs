//! Every process the hand-back can start, and the allowlist that refuses
//! anything else.
//!
//! The hand-back never types into a session. The only commands it can build
//! are, on the owner's host (directly, or through `ssh <alias>`):
//!
//! - `cmux sessions list --json --session <id>` (read-only liveness);
//! - `cmux notify --surface <uuid> --title <t> --body <b>`;
//! - `cmux set-status shipyard-pr-<n> <v> --workspace <uuid>` and the
//!   matching `cmux clear-status`;
//! - one fixed `sh -c` script that appends to
//!   `~/.local/state/shipyard/inbox/<session>.jsonl` from stdin.
//!
//! [`check_argv`] is the single gate: the production runner calls it before
//! every spawn, and tests assert every planned argv passes it and that
//! `cmux send`, `send-key`, agent CLIs and arbitrary shell do not.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};

use serde::Serialize;
use serde_json::Value;

use super::owner::{Route, safe_alias, safe_session_id, safe_uuid};

/// Default cmux CLI path. A non-interactive `ssh` does not put the app's
/// `bin` directory on `PATH`, so the absolute path is used everywhere.
pub const DEFAULT_CMUX_PATH: &str = "/Applications/cmux.app/Contents/Resources/bin/cmux";
/// Prefix of every sidebar status key this tool sets.
pub const STATUS_KEY_PREFIX: &str = "shipyard-pr-";
/// `ssh -o ConnectTimeout=` value.
pub const SSH_CONNECT_TIMEOUT: &str = "ConnectTimeout=10";
/// The remote inbox append, verbatim. `$1` is the (validated) session id;
/// the entry lines arrive on stdin.
pub const INBOX_SCRIPT: &str =
    "umask 077; d=\"$HOME/.local/state/shipyard/inbox\"; mkdir -p \"$d\" && cat >> \"$d/$1.jsonl\"";

/// One command on the owner's host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum HostCommand {
    /// `cmux sessions list --json --session <id>`.
    SessionsList {
        /// Session id.
        session: String,
    },
    /// `cmux notify --surface <uuid> --title <t> --body <b>`.
    Notify {
        /// Surface UUID.
        surface: String,
        /// Title.
        title: String,
        /// Body.
        body: String,
    },
    /// `cmux set-status <key> <value> --workspace <uuid>`.
    SetStatus {
        /// Workspace UUID.
        workspace: String,
        /// Key ([`STATUS_KEY_PREFIX`] + pull request).
        key: String,
        /// Pill text.
        value: String,
    },
    /// `cmux clear-status <key> --workspace <uuid>`.
    ClearStatus {
        /// Workspace UUID.
        workspace: String,
        /// Key.
        key: String,
    },
    /// Append stdin to the session's inbox file.
    InboxAppend {
        /// Session id.
        session: String,
    },
}

impl HostCommand {
    fn cmux_args(&self) -> Option<Vec<String>> {
        let own = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        Some(match self {
            Self::SessionsList { session } => {
                own(&["sessions", "list", "--json", "--session", session])
            }
            Self::Notify {
                surface,
                title,
                body,
            } => own(&[
                "notify",
                "--surface",
                surface,
                "--title",
                title,
                "--body",
                body,
            ]),
            Self::SetStatus {
                workspace,
                key,
                value,
            } => own(&["set-status", key, value, "--workspace", workspace]),
            Self::ClearStatus { workspace, key } => {
                own(&["clear-status", key, "--workspace", workspace])
            }
            Self::InboxAppend { .. } => return None,
        })
    }
}

/// A process to start: argv plus optional stdin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Invocation {
    /// Program and arguments.
    pub argv: Vec<String>,
    /// Bytes for stdin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
}

/// Single-quote one word for a POSIX shell.
#[must_use]
pub fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Split a string made only of [`shell_quote`]d words separated by single
/// spaces. Anything outside quotes other than `\'` and a separating space is
/// refused, so a remote command string cannot carry shell syntax.
///
/// # Errors
/// On anything that [`shell_quote`] would not have produced.
pub fn shell_unquote(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(inner) => word.push(inner),
                        None => return Err("unterminated quote".to_owned()),
                    }
                }
            }
            '\\' if chars.clone().next() == Some('\'') => {
                chars.next();
                in_word = true;
                word.push('\'');
            }
            ' ' if in_word => {
                words.push(std::mem::take(&mut word));
                in_word = false;
            }
            other => return Err(format!("unquoted {other:?} in remote command")),
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// The invocation for `command` on `route`. `None` for a local inbox append,
/// which is a file write, not a process.
#[must_use]
pub fn invocation(
    route: &Route,
    command: &HostCommand,
    cmux_path: &str,
    stdin: Option<String>,
) -> Option<Invocation> {
    let words: Vec<String> = if let Some(args) = command.cmux_args() {
        std::iter::once(cmux_path.to_owned()).chain(args).collect()
    } else {
        let HostCommand::InboxAppend { session } = command else {
            return None;
        };
        match route {
            Route::Local => return None,
            Route::Ssh(_) => vec![
                "sh".to_owned(),
                "-c".to_owned(),
                INBOX_SCRIPT.to_owned(),
                "sh".to_owned(),
                session.clone(),
            ],
        }
    };
    let argv = match route {
        Route::Local => words,
        Route::Ssh(alias) => vec![
            "ssh".to_owned(),
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            SSH_CONNECT_TIMEOUT.to_owned(),
            "--".to_owned(),
            alias.clone(),
            words
                .iter()
                .map(|word| shell_quote(word))
                .collect::<Vec<_>>()
                .join(" "),
        ],
    };
    Some(Invocation { argv, stdin })
}

fn cmux_args_allowed(args: &[String]) -> Result<(), String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let ok = match a.as_slice() {
        ["sessions", "list", "--json", "--session", session] => safe_session_id(session),
        [
            "notify",
            "--surface",
            surface,
            "--title",
            title,
            "--body",
            _body,
        ] => safe_uuid(surface) && !title.starts_with('-'),
        ["set-status", key, value, "--workspace", workspace] => {
            status_key_ok(key) && !value.starts_with('-') && safe_uuid(workspace)
        }
        ["clear-status", key, "--workspace", workspace] => {
            status_key_ok(key) && safe_uuid(workspace)
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "cmux {} is not an allowed hand-back command",
            a.first().copied().unwrap_or("")
        ))
    }
}

fn status_key_ok(key: &str) -> bool {
    key.strip_prefix(STATUS_KEY_PREFIX)
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

/// The allowlist. `Ok` only for the command shapes in the module docs, run
/// directly (program = `cmux_path`) or as `ssh -o BatchMode=yes -o
/// ConnectTimeout=10 -- <alias> <quoted words>`.
///
/// # Errors
/// Naming why the argv is refused.
pub fn check_argv(argv: &[String], cmux_path: &str) -> Result<(), String> {
    let Some((program, rest)) = argv.split_first() else {
        return Err("empty argv".to_owned());
    };
    if program == cmux_path {
        return cmux_args_allowed(rest);
    }
    if program != "ssh" {
        return Err(format!("program {program:?} is not allowed"));
    }
    let [o1, batch, o2, timeout, dashes, alias, remote] = rest else {
        return Err("ssh argv has the wrong shape".to_owned());
    };
    if o1 != "-o"
        || batch != "BatchMode=yes"
        || o2 != "-o"
        || timeout != SSH_CONNECT_TIMEOUT
        || dashes != "--"
        || !safe_alias(alias)
    {
        return Err("ssh options are not the allowed ones".to_owned());
    }
    let words = shell_unquote(remote)?;
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    match w.as_slice() {
        ["sh", "-c", script, "sh", session] if *script == INBOX_SCRIPT => {
            if safe_session_id(session) {
                Ok(())
            } else {
                Err("inbox session id is not safe".to_owned())
            }
        }
        [program, cmux_rest @ ..] if *program == cmux_path => cmux_args_allowed(
            &cmux_rest
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>(),
        ),
        _ => Err("remote command is not an allowed hand-back command".to_owned()),
    }
}

/// Whether the owner session is running, from `cmux sessions list --json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum Liveness {
    /// A cmux record for the session is `running` or `idle` with a live pid.
    Live {
        /// Surface holding it now.
        surface: Option<String>,
        /// Workspace holding it now.
        workspace: Option<String>,
    },
    /// cmux knows the session and it is not running (or has no record).
    Dead(String),
    /// cmux could not answer.
    Unknown(String),
    /// The host could not be reached.
    Unreachable(String),
}

impl Liveness {
    /// Stable name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Live { .. } => "live",
            Self::Dead(_) => "dead",
            Self::Unknown(_) => "unknown",
            Self::Unreachable(_) => "unreachable",
        }
    }
}

/// Decide liveness from `cmux sessions list --json --session <id>` output.
/// A row counts only when its `session_id` matches exactly; it is live only
/// when `agent_lifecycle` is `running` or `idle` and `stored_pid_exists` is
/// `true`. `idle` is an agent that finished its turn and waits for input with
/// its process still alive: the usual state of an owner whose pull request
/// went red after it stopped, and exactly the one a hand-back is for. A row
/// on the expected surface is preferred.
#[must_use]
pub fn parse_sessions(stdout: &str, session: &str, expected_surface: Option<&str>) -> Liveness {
    let Ok(value) = serde_json::from_str::<Value>(stdout) else {
        return Liveness::Unknown("cmux sessions list output is not JSON".to_owned());
    };
    let Some(rows) = value.get("sessions").and_then(Value::as_array) else {
        return Liveness::Unknown("cmux sessions list has no sessions array".to_owned());
    };
    let rows: Vec<&Value> = rows
        .iter()
        .filter(|row| row.get("session_id").and_then(Value::as_str) == Some(session))
        .collect();
    if rows.is_empty() {
        return Liveness::Dead("no cmux record for the session".to_owned());
    }
    let text = |row: &Value, key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
    let live: Vec<&&Value> = rows
        .iter()
        .filter(|row| {
            matches!(
                row.get("agent_lifecycle").and_then(Value::as_str),
                Some("running" | "idle")
            ) && row.get("stored_pid_exists").and_then(Value::as_bool) == Some(true)
        })
        .collect();
    let chosen = live
        .iter()
        .find(|row| {
            expected_surface.is_some_and(|surface| {
                text(row, "surface_id").is_some_and(|s| s.eq_ignore_ascii_case(surface))
            })
        })
        .or_else(|| live.first());
    match chosen {
        Some(row) => Liveness::Live {
            surface: text(row, "surface_id").filter(|s| safe_uuid(s)),
            workspace: text(row, "workspace_id").filter(|s| safe_uuid(s)),
        },
        None => Liveness::Dead(format!(
            "cmux lifecycle {}",
            rows.iter()
                .find_map(|row| row.get("agent_lifecycle").and_then(Value::as_str))
                .unwrap_or("unknown")
        )),
    }
}

/// Why a host command did not succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    /// The host could not be reached (ssh exit 255, timeout, spawn failure).
    Unreachable(String),
    /// The command ran and failed, or was refused by the allowlist.
    Failed(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "unreachable: {e}"),
            Self::Failed(e) => write!(f, "failed: {e}"),
        }
    }
}

/// Runs hand-back commands. Tests substitute a recorder.
pub trait HostRunner {
    /// Run an invocation that already passed [`check_argv`]; stdout on
    /// success.
    ///
    /// # Errors
    /// See [`RunError`].
    fn run(&mut self, invocation: &Invocation) -> Result<String, RunError>;

    /// Append lines to this machine's inbox file for `session`.
    ///
    /// # Errors
    /// When the file cannot be written.
    fn append_local_inbox(&mut self, session: &str, lines: &str) -> Result<(), String>;
}

/// Default local inbox directory: `$SHIPYARD_INBOX_DIR`, else
/// `~/.local/state/shipyard/inbox` (the same path on every platform, so the
/// session hook and a remote writer agree).
#[must_use]
pub fn default_inbox_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SHIPYARD_INBOX_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".local/state/shipyard/inbox"))
}

/// Append `lines` to `<dir>/<session>.jsonl` (owner-only permissions).
///
/// # Errors
/// When the session id is unsafe or the file cannot be written.
pub fn append_inbox_file(dir: &Path, session: &str, lines: &str) -> Result<(), String> {
    if !safe_session_id(session) {
        return Err("inbox session id is not safe".to_owned());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{session}.jsonl"));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.write_all(lines.as_bytes())
        .map_err(|e| format!("append {}: {e}", path.display()))
}

/// The real runner: [`check_argv`] then a bounded spawn.
pub struct ProcessHostRunner {
    /// Allowed cmux program.
    pub cmux_path: String,
    /// Per-command deadline.
    pub timeout: StdDuration,
    /// Local inbox directory.
    pub inbox_dir: Option<PathBuf>,
}

impl HostRunner for ProcessHostRunner {
    fn run(&mut self, invocation: &Invocation) -> Result<String, RunError> {
        check_argv(&invocation.argv, &self.cmux_path).map_err(RunError::Failed)?;
        #[cfg(unix)]
        {
            let (program, args) = invocation
                .argv
                .split_first()
                .ok_or_else(|| RunError::Failed("empty argv".to_owned()))?;
            let mut command = std::process::Command::new(program);
            command.args(args);
            let deadline = Instant::now() + self.timeout;
            let over_ssh = program == "ssh";
            let result = match &invocation.stdin {
                Some(input) => crate::process::run_output_with_input_until(
                    &mut command,
                    input.as_bytes(),
                    deadline,
                    "pr-watch hand-back",
                ),
                None => {
                    crate::process::run_output_until(&mut command, deadline, "pr-watch hand-back")
                }
            };
            let output = result.map_err(|e| {
                if over_ssh {
                    RunError::Unreachable(e.to_string())
                } else {
                    RunError::Failed(e.to_string())
                }
            })?;
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
            let detail = format!(
                "exit {:?}: {}",
                output.status.code(),
                stderr.chars().take(200).collect::<String>()
            );
            if over_ssh && output.status.code() == Some(255) {
                Err(RunError::Unreachable(detail))
            } else {
                Err(RunError::Failed(detail))
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (Instant::now(), &self.timeout);
            Err(RunError::Failed(
                "hand-back is supported on unix hosts only".to_owned(),
            ))
        }
    }

    fn append_local_inbox(&mut self, session: &str, lines: &str) -> Result<(), String> {
        let dir = self
            .inbox_dir
            .clone()
            .ok_or_else(|| "no inbox directory (HOME unset)".to_owned())?;
        append_inbox_file(&dir, session, lines)
    }
}
