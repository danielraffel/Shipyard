//! Delivery over cmux: `cmux agent message` on this host, or over ssh on
//! another, with the body always on stdin (never inside a quoted argument a
//! remote shell could expand).
//!
//! A host can run cmux NIGHTLY (`/tmp/cmux-nightly.sock`) and stable cmux
//! (its default socket) at once, and a surface belongs to exactly one of
//! them. Existence is proven with `read-screen --surface`, which fails with
//! "Surface not found" on a socket that does not own the surface;
//! `identify` cannot prove it (it reports the focused surface). The socket
//! that worked is remembered and tried first next time.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::config::{Address, HostRoute, RelayConfig};

/// Sender name cmux shows on a relayed message.
pub(crate) const FROM: &str = "shipyard-relay";

/// What one cmux invocation returned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CmuxResult {
    pub(crate) ok: bool,
    pub(crate) output: String,
}

/// Runs cmux on a host. The production runner spawns processes; tests
/// script the answers.
pub(crate) trait CmuxRunner {
    /// Run `cmux <args>` on `route`, with `stdin` when given.
    fn run(&self, route: &HostRoute, args: &[String], stdin: Option<&str>) -> CmuxResult;
}

/// Quote `text` for a POSIX shell.
pub(crate) fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

fn ssh_binary() -> std::path::PathBuf {
    ["/usr/bin/ssh", "/bin/ssh"]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| std::path::PathBuf::from("ssh"))
}

/// The argv the production runner spawns for `args` on `route`.
pub(crate) fn command_line(route: &HostRoute, args: &[String]) -> Vec<String> {
    match &route.ssh {
        None => std::iter::once(route.cmux.clone())
            .chain(args.iter().cloned())
            .collect(),
        Some(destination) => {
            let remote = std::iter::once(route.cmux.as_str())
                .chain(args.iter().map(String::as_str))
                .map(sh_quote)
                .collect::<Vec<_>>()
                .join(" ");
            vec![
                ssh_binary().to_string_lossy().into_owned(),
                "-o".to_owned(),
                "BatchMode=yes".to_owned(),
                "-o".to_owned(),
                "ConnectTimeout=8".to_owned(),
                destination.clone(),
                remote,
            ]
        }
    }
}

/// Spawns cmux (locally or over ssh) under a deadline.
pub(crate) struct ProcessRunner {
    pub(crate) timeout: Duration,
}

impl CmuxRunner for ProcessRunner {
    fn run(&self, route: &HostRoute, args: &[String], stdin: Option<&str>) -> CmuxResult {
        let command_argv = command_line(route, args);
        let mut command = Command::new(&command_argv[0]);
        command.args(&command_argv[1..]);
        let deadline = Instant::now() + self.timeout;
        let label = format!("cmux on {}", route.name);
        let result = match stdin {
            #[cfg(unix)]
            Some(text) => crate::process::run_output_with_input_until(
                &mut command,
                text.as_bytes(),
                deadline,
                label,
            ),
            #[cfg(not(unix))]
            Some(_) => {
                return CmuxResult {
                    ok: false,
                    output: "stdin delivery needs a unix host".to_owned(),
                };
            }
            None => {
                command.stdin(Stdio::null());
                crate::process::run_output_until(&mut command, deadline, label)
            }
        };
        match result {
            Ok(output) => CmuxResult {
                ok: output.status.success(),
                output: format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
            },
            Err(error) => CmuxResult {
                ok: false,
                output: error.to_string(),
            },
        }
    }
}

fn socket_args(socket: &str) -> Vec<String> {
    if socket.is_empty() {
        Vec::new()
    } else {
        vec!["--socket".to_owned(), socket.to_owned()]
    }
}

/// `read-screen` for `surface` on `socket`: succeeds only where the surface
/// exists.
pub(crate) fn probe_args(socket: &str, surface: &str) -> Vec<String> {
    let mut args = socket_args(socket);
    args.extend(
        ["read-screen", "--surface", surface, "--lines", "1"]
            .iter()
            .map(|s| (*s).to_owned()),
    );
    args
}

/// `agent message` for `surface` on `socket`, body from stdin.
pub(crate) fn message_args(socket: &str, surface: &str) -> Vec<String> {
    let mut args = socket_args(socket);
    args.extend(
        ["agent", "message", surface, "--from", FROM, "--", "-"]
            .iter()
            .map(|s| (*s).to_owned()),
    );
    args
}

/// Remembered sockets, by `host surface`.
pub(crate) type Routes = BTreeMap<String, String>;

fn route_key(address: &Address) -> String {
    format!("{} {}", address.host, address.surface)
}

/// The socket on which `address`'s surface exists, trying the remembered
/// one, then the one its record named, then the configured order.
pub(crate) fn resolve(
    runner: &dyn CmuxRunner,
    config: &RelayConfig,
    local_host: &str,
    address: &Address,
    routes: &mut Routes,
) -> Option<String> {
    if !config.knows_host(&address.host, local_host) {
        return None;
    }
    let route = config.route(&address.host, local_host);
    let key = route_key(address);
    let mut order: Vec<String> = Vec::new();
    for socket in routes
        .get(&key)
        .into_iter()
        .chain(address.socket.iter())
        .chain(config.sockets.iter())
    {
        if !order.contains(socket) {
            order.push(socket.clone());
        }
    }
    for socket in order {
        if runner
            .run(&route, &probe_args(&socket, &address.surface), None)
            .ok
        {
            routes.insert(key, socket.clone());
            return Some(socket);
        }
    }
    routes.remove(&key);
    None
}

/// Human-readable name of a socket.
pub(crate) fn socket_label(socket: &str) -> String {
    if socket.is_empty() {
        "default socket".to_owned()
    } else {
        socket.to_owned()
    }
}

/// Send `text` to `address`. `Ok` names the transport that took it.
pub(crate) fn send(
    runner: &dyn CmuxRunner,
    config: &RelayConfig,
    local_host: &str,
    address: &Address,
    text: &str,
    routes: &mut Routes,
) -> Result<String, String> {
    if super::refuses_verdict_text(text) {
        return Err("refused: the message contains verdict text".to_owned());
    }
    if !crate::owner_registry::safe_value(&address.surface)
        || !crate::owner_registry::safe_value(&address.host)
    {
        return Err(format!(
            "unsafe address {}/{}",
            address.host, address.surface
        ));
    }
    if !config.knows_host(&address.host, local_host) {
        return Err(format!(
            "{}/{}: host {} is not this host and not in relay.hosts",
            address.host, address.surface, address.host
        ));
    }
    let Some(socket) = resolve(runner, config, local_host, address, routes) else {
        return Err(format!(
            "{}/{}: no cmux socket on {} knows this surface (session closed or moved)",
            address.host, address.surface, address.host
        ));
    };
    let route = config.route(&address.host, local_host);
    let result = runner.run(&route, &message_args(&socket, &address.surface), Some(text));
    if result.ok {
        Ok(format!(
            "cmux {} {} via {}{}",
            address.host,
            address.surface,
            socket_label(&socket),
            if route.ssh.is_some() { " over ssh" } else { "" }
        ))
    } else {
        routes.remove(&route_key(address));
        let tail: String = result
            .output
            .trim()
            .chars()
            .rev()
            .take(200)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Err(format!(
            "{}/{}: agent message failed: {tail}",
            address.host, address.surface
        ))
    }
}
