//! Live, read-only terminal authority for continuation delivery.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalProcessIncarnation {
    pub(crate) boot_id: String,
    pub(crate) pid: u32,
    pub(crate) start_identity: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "adapter", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TerminalCapabilityRequest {
    Cmux {
        cli_path: String,
        socket_path: String,
        surface_id: String,
        workspace_id: String,
        native_session_id: String,
        provider_kind: String,
        process: LocalProcessIncarnation,
    },
    HerdR {
        selector: String,
        terminal_id: Option<String>,
        native_session_id: String,
        provider_kind: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VerifiedTerminalEvidence {
    pub(crate) terminal_instance: String,
    pub(crate) workspace_id: String,
    pub(crate) native_session_id: String,
    pub(crate) process: LocalProcessIncarnation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalCapabilityRefusal {
    Unsupported,
    Unobservable,
    MethodMissing,
    InvalidResponse,
    NoMatch,
    MultipleMatches,
    ProcessIncarnationChanged,
    NativeSessionMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderProcessPresence {
    Absent,
    Present,
}

#[cfg(target_os = "macos")]
pub(crate) fn observe_provider_on_cmux_surface(
    cli_path: &str,
    socket_path: &str,
    surface_id: &str,
    native_session_id: &str,
    provider_kind: &str,
) -> Result<ProviderProcessPresence, TerminalCapabilityRefusal> {
    validate_cmux_inputs(
        cli_path,
        socket_path,
        surface_id,
        native_session_id,
        provider_kind,
    )?;
    for pid in candidate_agent_pids(provider_kind)? {
        let target = match resolve_pid(cli_path, socket_path, pid) {
            Ok(target) => target,
            Err(TerminalCapabilityRefusal::NoMatch) => continue,
            Err(_) => return Err(TerminalCapabilityRefusal::Unobservable),
        };
        if target.surface_id == surface_id && process_has_argument(pid, native_session_id)? {
            return Ok(ProviderProcessPresence::Present);
        }
    }
    Ok(ProviderProcessPresence::Absent)
}

#[cfg(target_os = "macos")]
fn process_has_argument(pid: u32, expected: &str) -> Result<bool, TerminalCapabilityRefusal> {
    let mut command = std::process::Command::new("/bin/ps");
    command
        .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
        .env_clear();
    let output = crate::process::run_output_until(
        &mut command,
        std::time::Instant::now() + std::time::Duration::from_secs(2),
        "terminal provider argv",
    )
    .map_err(|_| TerminalCapabilityRefusal::Unobservable)?;
    if !output.status.success() || output.stdout.len() > 64 * 1024 {
        return Err(TerminalCapabilityRefusal::Unobservable);
    }
    let command = std::str::from_utf8(&output.stdout)
        .map_err(|_| TerminalCapabilityRefusal::InvalidResponse)?;
    Ok(command
        .split_ascii_whitespace()
        .any(|value| value == expected))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn observe_provider_on_cmux_surface(
    _: &str,
    _: &str,
    _: &str,
    _: &str,
    _: &str,
) -> Result<ProviderProcessPresence, TerminalCapabilityRefusal> {
    Err(TerminalCapabilityRefusal::Unsupported)
}

pub(crate) trait TerminalEvidenceAdapter {
    fn capture_cmux(
        &mut self,
        cli_path: &str,
        socket_path: &str,
        surface_id: &str,
        native_session_id: &str,
        provider_kind: &str,
    ) -> Result<TerminalCapabilityRequest, TerminalCapabilityRefusal>;

    fn verify_once(
        &mut self,
        request: &TerminalCapabilityRequest,
    ) -> Result<VerifiedTerminalEvidence, TerminalCapabilityRefusal>;
}

pub(crate) struct ProductionTerminalEvidenceAdapter;

impl TerminalEvidenceAdapter for ProductionTerminalEvidenceAdapter {
    fn capture_cmux(
        &mut self,
        cli_path: &str,
        socket_path: &str,
        surface_id: &str,
        native_session_id: &str,
        provider_kind: &str,
    ) -> Result<TerminalCapabilityRequest, TerminalCapabilityRefusal> {
        capture_cmux(
            cli_path,
            socket_path,
            surface_id,
            native_session_id,
            provider_kind,
        )
    }

    fn verify_once(
        &mut self,
        request: &TerminalCapabilityRequest,
    ) -> Result<VerifiedTerminalEvidence, TerminalCapabilityRefusal> {
        match request {
            TerminalCapabilityRequest::Cmux {
                cli_path,
                socket_path,
                surface_id,
                native_session_id,
                provider_kind,
                process,
                ..
            } => verify_cmux(
                cli_path,
                socket_path,
                surface_id,
                native_session_id,
                provider_kind,
                process,
            ),
            TerminalCapabilityRequest::HerdR { .. } => Err(TerminalCapabilityRefusal::Unsupported),
        }
    }
}

#[cfg(target_os = "macos")]
fn capture_cmux(
    cli_path: &str,
    socket_path: &str,
    surface_id: &str,
    native_session_id: &str,
    provider_kind: &str,
) -> Result<TerminalCapabilityRequest, TerminalCapabilityRefusal> {
    validate_cmux_inputs(
        cli_path,
        socket_path,
        surface_id,
        native_session_id,
        provider_kind,
    )?;
    let mut matches = Vec::new();
    for pid in candidate_agent_pids(provider_kind)? {
        let Ok(process) = observe_process(pid) else {
            continue;
        };
        let Ok(target) = resolve_pid(cli_path, socket_path, pid) else {
            continue;
        };
        if target.surface_id == surface_id {
            matches.push((process, target.workspace_id));
        }
    }
    let [(process, workspace_id)] = matches.as_slice() else {
        return Err(if matches.is_empty() {
            TerminalCapabilityRefusal::NoMatch
        } else {
            TerminalCapabilityRefusal::MultipleMatches
        });
    };
    require_native_session(
        cli_path,
        socket_path,
        surface_id,
        native_session_id,
        provider_kind,
    )?;
    require_unique_native_session(cli_path, socket_path, native_session_id)?;
    require_same_process(process)?;
    Ok(TerminalCapabilityRequest::Cmux {
        cli_path: cli_path.to_owned(),
        socket_path: socket_path.to_owned(),
        surface_id: surface_id.to_owned(),
        workspace_id: workspace_id.clone(),
        native_session_id: native_session_id.to_owned(),
        provider_kind: provider_kind.to_owned(),
        process: process.clone(),
    })
}

#[cfg(not(target_os = "macos"))]
fn capture_cmux(
    _: &str,
    _: &str,
    _: &str,
    _: &str,
    _: &str,
) -> Result<TerminalCapabilityRequest, TerminalCapabilityRefusal> {
    Err(TerminalCapabilityRefusal::Unsupported)
}

#[cfg(target_os = "macos")]
fn verify_cmux(
    cli_path: &str,
    socket_path: &str,
    surface_id: &str,
    native_session_id: &str,
    provider_kind: &str,
    process: &LocalProcessIncarnation,
) -> Result<VerifiedTerminalEvidence, TerminalCapabilityRefusal> {
    validate_cmux_inputs(
        cli_path,
        socket_path,
        surface_id,
        native_session_id,
        provider_kind,
    )?;
    require_same_process(process)?;
    let target = resolve_pid(cli_path, socket_path, process.pid)?;
    if target.surface_id != surface_id {
        return Err(TerminalCapabilityRefusal::NoMatch);
    }
    require_native_session(
        cli_path,
        socket_path,
        surface_id,
        native_session_id,
        provider_kind,
    )?;
    require_unique_native_session(cli_path, socket_path, native_session_id)?;
    require_same_process(process)?;
    Ok(VerifiedTerminalEvidence {
        terminal_instance: target.surface_id,
        workspace_id: target.workspace_id,
        native_session_id: native_session_id.to_owned(),
        process: process.clone(),
    })
}

#[cfg(not(target_os = "macos"))]
fn verify_cmux(
    _: &str,
    _: &str,
    _: &str,
    _: &str,
    _: &str,
    _: &LocalProcessIncarnation,
) -> Result<VerifiedTerminalEvidence, TerminalCapabilityRefusal> {
    Err(TerminalCapabilityRefusal::Unsupported)
}

#[cfg(target_os = "macos")]
#[derive(Deserialize)]
struct CmuxTarget {
    source: String,
    pid_resolution: String,
    #[serde(default)]
    pid: Option<u32>,
    surface_id: String,
    workspace_id: String,
}

#[cfg(target_os = "macos")]
fn resolve_pid(
    cli_path: &str,
    socket_path: &str,
    pid: u32,
) -> Result<CmuxTarget, TerminalCapabilityRefusal> {
    let params = serde_json::json!({"pid": pid, "pid_resolution": "controlling_tty"}).to_string();
    let value = cmux_json(
        cli_path,
        socket_path,
        &["rpc", "agent.resolve_delivery_target", &params],
    )?;
    let target: CmuxTarget = serde_json::from_value(value.get("result").cloned().unwrap_or(value))
        .map_err(|_| TerminalCapabilityRefusal::InvalidResponse)?;
    // v0.64.22 is request-specific and reports source=pid but predates the
    // additive PID echo. If a newer server supplies it, enforce it.
    if target.source != "pid"
        || target.pid_resolution != "controlling_tty"
        || target.pid.is_some_and(|observed| observed != pid)
        || !is_uuid(&target.surface_id)
        || !is_uuid(&target.workspace_id)
    {
        return Err(TerminalCapabilityRefusal::NoMatch);
    }
    Ok(target)
}

#[cfg(target_os = "macos")]
fn require_native_session(
    cli_path: &str,
    socket_path: &str,
    surface_id: &str,
    native_session_id: &str,
    provider_kind: &str,
) -> Result<(), TerminalCapabilityRefusal> {
    let value = cmux_json(
        cli_path,
        socket_path,
        &[
            "--json",
            "--id-format",
            "uuids",
            "surface",
            "resume",
            "show",
            "--surface",
            surface_id,
        ],
    )?;
    if native_session_matches(&value, native_session_id, provider_kind) {
        Ok(())
    } else {
        Err(TerminalCapabilityRefusal::NativeSessionMismatch)
    }
}

#[cfg(target_os = "macos")]
fn native_session_matches(
    value: &serde_json::Value,
    native_session_id: &str,
    provider_kind: &str,
) -> bool {
    let Some(binding) = value.get("resume_binding") else {
        return false;
    };
    let observed = observed_checkpoint(value);
    observed == Some(native_session_id)
        && binding.get("kind").and_then(serde_json::Value::as_str) == Some(provider_kind)
        && binding.get("source").and_then(serde_json::Value::as_str) == Some("agent-hook")
        && binding
            .get("execution_location")
            .and_then(serde_json::Value::as_str)
            == Some("local")
        && [
            "remote_pty_session_id",
            "remote_surface_id",
            "remote_workspace_id",
        ]
        .into_iter()
        .all(|field| binding.get(field).is_none_or(serde_json::Value::is_null))
}

#[cfg(target_os = "macos")]
fn observed_checkpoint(value: &serde_json::Value) -> Option<&str> {
    value
        .pointer("/resume_binding/checkpoint_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            value
                .pointer("/restore_record/checkpoint_id")
                .and_then(serde_json::Value::as_str)
        })
}

#[cfg(target_os = "macos")]
fn require_unique_native_session(
    cli_path: &str,
    socket_path: &str,
    native_session_id: &str,
) -> Result<(), TerminalCapabilityRefusal> {
    let tree = cmux_json(
        cli_path,
        socket_path,
        &["--json", "--id-format", "uuids", "tree", "--all"],
    )?;
    let mut surfaces = Vec::new();
    collect_surface_ids(&tree, &mut surfaces)?;
    if surfaces.len() > 256 {
        return Err(TerminalCapabilityRefusal::Unobservable);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut matches = 0;
    for surface in surfaces {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(TerminalCapabilityRefusal::Unobservable)?;
        let value = cmux_json_with_timeout(
            cli_path,
            socket_path,
            &[
                "--json",
                "--id-format",
                "uuids",
                "surface",
                "resume",
                "show",
                "--surface",
                &surface,
            ],
            remaining.min(std::time::Duration::from_secs(5)),
        )?;
        if observed_checkpoint(&value) == Some(native_session_id) {
            matches += 1;
        }
    }
    match matches {
        1 => Ok(()),
        0 => Err(TerminalCapabilityRefusal::NoMatch),
        _ => Err(TerminalCapabilityRefusal::MultipleMatches),
    }
}

#[cfg(target_os = "macos")]
fn collect_surface_ids(
    value: &serde_json::Value,
    output: &mut Vec<String>,
) -> Result<(), TerminalCapabilityRefusal> {
    match value {
        serde_json::Value::Object(fields) => {
            match (fields.get("surface_ids"), fields.get("surfaces")) {
                (Some(ids), Some(surfaces)) => {
                    let ids = ids
                        .as_array()
                        .ok_or(TerminalCapabilityRefusal::InvalidResponse)?;
                    let surfaces = surfaces
                        .as_array()
                        .ok_or(TerminalCapabilityRefusal::InvalidResponse)?;
                    if ids.len() != surfaces.len() {
                        return Err(TerminalCapabilityRefusal::InvalidResponse);
                    }
                    for (id, surface) in ids.iter().zip(surfaces) {
                        let id = id
                            .as_str()
                            .filter(|id| is_uuid(id))
                            .ok_or(TerminalCapabilityRefusal::InvalidResponse)?;
                        let object_id = surface
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .filter(|object_id| is_uuid(object_id))
                            .ok_or(TerminalCapabilityRefusal::InvalidResponse)?;
                        if id != object_id || output.iter().any(|existing| existing == id) {
                            return Err(TerminalCapabilityRefusal::InvalidResponse);
                        }
                        output.push(id.to_owned());
                    }
                }
                (None, None) => {}
                _ => return Err(TerminalCapabilityRefusal::InvalidResponse),
            }
            for (key, value) in fields {
                if !matches!(key.as_str(), "surface_ids" | "surfaces") {
                    collect_surface_ids(value, output)?;
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_surface_ids(value, output)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn cmux_json(
    cli_path: &str,
    socket_path: &str,
    args: &[&str],
) -> Result<serde_json::Value, TerminalCapabilityRefusal> {
    cmux_json_with_timeout(
        cli_path,
        socket_path,
        args,
        std::time::Duration::from_secs(5),
    )
}

#[cfg(target_os = "macos")]
fn cmux_json_with_timeout(
    cli_path: &str,
    socket_path: &str,
    args: &[&str],
    timeout: std::time::Duration,
) -> Result<serde_json::Value, TerminalCapabilityRefusal> {
    use std::io::{Read, Seek, SeekFrom};
    use std::process::{Command, Stdio};
    let mut stdout = tempfile::tempfile().map_err(|_| TerminalCapabilityRefusal::Unobservable)?;
    let mut stderr = tempfile::tempfile().map_err(|_| TerminalCapabilityRefusal::Unobservable)?;
    let mut command = Command::new(cli_path);
    command
        .args(["--socket", socket_path])
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            stdout
                .try_clone()
                .map_err(|_| TerminalCapabilityRefusal::Unobservable)?,
        ))
        .stderr(Stdio::from(
            stderr
                .try_clone()
                .map_err(|_| TerminalCapabilityRefusal::Unobservable)?,
        ));
    let mut process = crate::process::ProcessTree::spawn(&mut command)
        .map_err(|_| TerminalCapabilityRefusal::Unobservable)?;
    let Some(status) = process
        .wait_timeout(timeout)
        .map_err(|_| TerminalCapabilityRefusal::Unobservable)?
    else {
        process.terminate();
        return Err(TerminalCapabilityRefusal::Unobservable);
    };
    process.terminate();
    let read = |file: &mut std::fs::File| {
        file.seek(SeekFrom::Start(0))
            .and_then(|_| {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).map(|_| bytes)
            })
            .map_err(|_| TerminalCapabilityRefusal::Unobservable)
    };
    let output = std::process::Output {
        status,
        stdout: read(&mut stdout)?,
        stderr: read(&mut stderr)?,
    };
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        return Err(
            if error.contains("method_not_found") || error.contains("unrecognized_method") {
                TerminalCapabilityRefusal::MethodMissing
            } else {
                TerminalCapabilityRefusal::NoMatch
            },
        );
    }
    serde_json::from_slice(&output.stdout).map_err(|_| TerminalCapabilityRefusal::InvalidResponse)
}

#[cfg(target_os = "macos")]
fn candidate_agent_pids(provider_kind: &str) -> Result<Vec<u32>, TerminalCapabilityRefusal> {
    use std::process::Command;
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,uid=,comm="])
        .env_clear()
        .output()
        .map_err(|_| TerminalCapabilityRefusal::Unobservable)?;
    if !output.status.success() {
        return Err(TerminalCapabilityRefusal::Unobservable);
    }
    let uid = nix::unistd::Uid::effective().as_raw();
    let mut pids = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let process_uid = fields.next()?.parse::<u32>().ok()?;
            let command = fields.next()?;
            let name = command.rsplit('/').next()?;
            (process_uid == uid && name == provider_kind).then_some(pid)
        })
        .collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    if pids.len() > 256 {
        return Err(TerminalCapabilityRefusal::Unobservable);
    }
    Ok(pids)
}

#[cfg(target_os = "macos")]
fn observe_process(pid: u32) -> Result<LocalProcessIncarnation, TerminalCapabilityRefusal> {
    use std::process::Command;
    if pid == 0 {
        return Err(TerminalCapabilityRefusal::Unobservable);
    }
    let boot = Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.boottime"])
        .env_clear()
        .output()
        .map_err(|_| TerminalCapabilityRefusal::Unobservable)?;
    let start = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .env_clear()
        .output()
        .map_err(|_| TerminalCapabilityRefusal::Unobservable)?;
    if !boot.status.success() || !start.status.success() {
        return Err(TerminalCapabilityRefusal::Unobservable);
    }
    let boot_id = String::from_utf8_lossy(&boot.stdout).trim().to_owned();
    let start_identity = String::from_utf8_lossy(&start.stdout).trim().to_owned();
    if boot_id.is_empty() || start_identity.is_empty() {
        return Err(TerminalCapabilityRefusal::Unobservable);
    }
    Ok(LocalProcessIncarnation {
        boot_id,
        pid,
        start_identity,
    })
}

#[cfg(target_os = "macos")]
fn require_same_process(
    expected: &LocalProcessIncarnation,
) -> Result<(), TerminalCapabilityRefusal> {
    if observe_process(expected.pid)? == *expected {
        Ok(())
    } else {
        Err(TerminalCapabilityRefusal::ProcessIncarnationChanged)
    }
}

#[cfg(target_os = "macos")]
fn validate_cmux_inputs(
    cli_path: &str,
    socket_path: &str,
    surface_id: &str,
    native_session_id: &str,
    provider_kind: &str,
) -> Result<(), TerminalCapabilityRefusal> {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;
    let cli = Path::new(cli_path);
    let socket = Path::new(socket_path);
    let cli_metadata = cli
        .metadata()
        .map_err(|_| TerminalCapabilityRefusal::Unobservable)?;
    let socket_metadata = socket
        .symlink_metadata()
        .map_err(|_| TerminalCapabilityRefusal::Unobservable)?;
    let effective_uid = nix::unistd::Uid::effective().as_raw();
    if !cli.is_absolute()
        || !socket.is_absolute()
        || !cli_metadata.is_file()
        || (cli_metadata.uid() != 0 && cli_metadata.uid() != effective_uid)
        || cli_metadata.permissions().mode() & 0o022 != 0
        || cli_metadata.permissions().mode() & 0o111 == 0
        || !socket_metadata.file_type().is_socket()
        || (socket_metadata.uid() != 0 && socket_metadata.uid() != effective_uid)
        || socket_metadata.permissions().mode() & 0o022 != 0
        || !is_uuid(surface_id)
        || native_session_id.trim().is_empty()
        || provider_kind.is_empty()
        || provider_kind.len() > 128
        || !provider_kind.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
    {
        return Err(TerminalCapabilityRefusal::Unobservable);
    }
    Ok(())
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(index, ch)| {
            matches!(index, 8 | 13 | 18 | 23) && ch == '-'
                || !matches!(index, 8 | 13 | 18 | 23) && ch.is_ascii_hexdigit()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAdapter {
        capture: Result<TerminalCapabilityRequest, TerminalCapabilityRefusal>,
        verify: Result<VerifiedTerminalEvidence, TerminalCapabilityRefusal>,
    }

    impl TerminalEvidenceAdapter for FakeAdapter {
        fn capture_cmux(
            &mut self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<TerminalCapabilityRequest, TerminalCapabilityRefusal> {
            self.capture.clone()
        }

        fn verify_once(
            &mut self,
            _: &TerminalCapabilityRequest,
        ) -> Result<VerifiedTerminalEvidence, TerminalCapabilityRefusal> {
            self.verify.clone()
        }
    }

    fn request() -> TerminalCapabilityRequest {
        TerminalCapabilityRequest::Cmux {
            cli_path: "/test/cmux".into(),
            socket_path: "/test/cmux.sock".into(),
            surface_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into(),
            workspace_id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into(),
            native_session_id: "native".into(),
            provider_kind: "codex".into(),
            process: LocalProcessIncarnation {
                boot_id: "boot".into(),
                pid: 42,
                start_identity: "start".into(),
            },
        }
    }

    #[test]
    fn herdr_is_an_explicit_but_refused_capability() {
        let mut adapter = ProductionTerminalEvidenceAdapter;
        assert_eq!(
            adapter.verify_once(&TerminalCapabilityRequest::HerdR {
                selector: "named".into(),
                terminal_id: Some("terminal".into()),
                native_session_id: "native".into(),
                provider_kind: "codex".into(),
            }),
            Err(TerminalCapabilityRefusal::Unsupported)
        );
    }

    #[test]
    fn workspace_rename_does_not_replace_surface_identity() {
        let evidence = VerifiedTerminalEvidence {
            terminal_instance: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into(),
            workspace_id: "cccccccc-cccc-cccc-cccc-cccccccccccc".into(),
            native_session_id: "native".into(),
            process: LocalProcessIncarnation {
                boot_id: "boot".into(),
                pid: 42,
                start_identity: "start".into(),
            },
        };
        assert_eq!(
            evidence.terminal_instance,
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
        );
        assert_ne!(
            evidence.workspace_id,
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
        );
    }

    #[test]
    fn zero_multiple_and_pid_reuse_refusals_are_not_promoted_to_evidence() {
        for refusal in [
            TerminalCapabilityRefusal::NoMatch,
            TerminalCapabilityRefusal::MultipleMatches,
            TerminalCapabilityRefusal::ProcessIncarnationChanged,
        ] {
            let mut adapter = FakeAdapter {
                capture: Err(refusal),
                verify: Err(refusal),
            };
            assert_eq!(adapter.capture_cmux("", "", "", "", "codex"), Err(refusal));
            assert_eq!(adapter.verify_once(&request()), Err(refusal));
        }
    }

    #[test]
    fn cmux_request_can_never_cross_promote_to_herdr() {
        let mut adapter = ProductionTerminalEvidenceAdapter;
        let herdr = TerminalCapabilityRequest::HerdR {
            selector: "named".into(),
            terminal_id: None,
            native_session_id: "native".into(),
            provider_kind: "codex".into(),
        };
        assert_eq!(
            adapter.verify_once(&herdr),
            Err(TerminalCapabilityRefusal::Unsupported)
        );
        assert!(matches!(request(), TerminalCapabilityRequest::Cmux { .. }));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn malformed_or_duplicate_surface_census_fails_closed() {
        for tree in [
            serde_json::json!({"surface_ids": "not-an-array"}),
            serde_json::json!({"surface_ids": [42]}),
            serde_json::json!({"surface_ids": ["not-a-uuid"]}),
            serde_json::json!({
                "surface_ids": [
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
                ]
            }),
        ] {
            assert_eq!(
                collect_surface_ids(&tree, &mut Vec::new()),
                Err(TerminalCapabilityRefusal::InvalidResponse)
            );
        }
        let mut valid = Vec::new();
        collect_surface_ids(
            &serde_json::json!({
                "surface_ids": ["aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"],
                "surfaces": [{"id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"}]
            }),
            &mut valid,
        )
        .expect("paired live tree representations");
        assert_eq!(valid, ["aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"]);
        assert_eq!(
            collect_surface_ids(
                &serde_json::json!({
                    "surface_ids": ["aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"],
                    "surfaces": [{"id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"}]
                }),
                &mut Vec::new(),
            ),
            Err(TerminalCapabilityRefusal::InvalidResponse)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_binding_kind_must_match_execution_provider() {
        let mut value = serde_json::json!({
            "resume_binding": {
                "checkpoint_id": "native",
                "kind": "codex",
                "source": "agent-hook",
                "execution_location": "local",
                "remote_pty_session_id": null,
                "remote_surface_id": null,
                "remote_workspace_id": null
            }
        });
        assert!(native_session_matches(&value, "native", "codex"));
        value["resume_binding"]["kind"] = serde_json::json!("claude");
        assert!(!native_session_matches(&value, "native", "codex"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn provider_process_presence_requires_the_exact_native_session_argument() {
        let mut child = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "while :; do sleep 1; done",
                "shipyard-route-test",
                "native-session-exact",
            ])
            .spawn()
            .expect("spawn argument fixture");
        assert!(process_has_argument(child.id(), "native-session-exact").unwrap());
        assert!(!process_has_argument(child.id(), "other-session").unwrap());
        child.kill().expect("stop argument fixture");
        child.wait().expect("reap argument fixture");
    }
}
