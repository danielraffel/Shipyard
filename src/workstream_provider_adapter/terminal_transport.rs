#[cfg(target_os = "macos")]
use std::ffi::OsStr;
#[cfg(target_os = "macos")]
use std::fs;
#[cfg(target_os = "macos")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "macos")]
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::process::run_output_until;
use crate::provider_wrapper::{CmuxEndpointV1, TerminalEndpointV1};

const COMMAND_DEADLINE: Duration = Duration::from_secs(15);
#[cfg(target_os = "macos")]
const CODESIGN: &str = "/usr/bin/codesign";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CommandResult {
    pub(super) success: bool,
    pub(super) stdout: Vec<u8>,
}

pub(super) trait TerminalTransport {
    fn bind(&mut self, endpoint: &TerminalEndpointV1) -> Result<(), RunnerFailure>;
    fn run(&mut self, args: &[String]) -> Result<CommandResult, RunnerFailure>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RunnerFailure {
    Unavailable,
    CapabilityUnproven,
    #[cfg(any(target_os = "macos", test))]
    Untrusted,
}

pub(super) struct ProductionCmuxTransport {
    endpoint: Option<CmuxEndpointV1>,
    trusted_signing_team_id: String,
}

impl ProductionCmuxTransport {
    pub(super) fn new(trusted_signing_team_id: String) -> Self {
        Self {
            endpoint: None,
            trusted_signing_team_id,
        }
    }
}

impl TerminalTransport for ProductionCmuxTransport {
    fn bind(&mut self, endpoint: &TerminalEndpointV1) -> Result<(), RunnerFailure> {
        match endpoint {
            TerminalEndpointV1::Cmux(endpoint) => {
                verify_authorized_cmux(endpoint, &self.trusted_signing_team_id)?;
                self.endpoint = Some(endpoint.clone());
                Ok(())
            }
            TerminalEndpointV1::HerdR { .. } => Err(RunnerFailure::CapabilityUnproven),
        }
    }

    fn run(&mut self, args: &[String]) -> Result<CommandResult, RunnerFailure> {
        let endpoint = self.endpoint.as_ref().ok_or(RunnerFailure::Unavailable)?;
        let mut command = Command::new(&endpoint.executable_path);
        command
            .args(["--socket", &endpoint.socket_path])
            .args(args)
            .env_clear();
        let output = run_output_until(
            &mut command,
            Instant::now() + COMMAND_DEADLINE,
            "cmux workstream provider",
        )
        .map_err(|_| RunnerFailure::Unavailable)?;
        Ok(CommandResult {
            success: output.status.success(),
            stdout: output.stdout,
        })
    }
}

#[cfg(target_os = "macos")]
fn verify_authorized_cmux(
    endpoint: &CmuxEndpointV1,
    trusted_signing_team_id: &str,
) -> Result<(), RunnerFailure> {
    use std::os::unix::fs::FileTypeExt;

    verify_cmux_signing_policy(endpoint, trusted_signing_team_id)?;
    let cli = Path::new(&endpoint.executable_path);
    let socket = Path::new(&endpoint.socket_path);
    let cli_metadata = fs::metadata(cli).map_err(|_| RunnerFailure::Unavailable)?;
    let socket_metadata = fs::symlink_metadata(socket).map_err(|_| RunnerFailure::Unavailable)?;
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
    {
        return Err(RunnerFailure::Untrusted);
    }
    let requirement = format!(
        "=anchor apple generic and certificate leaf[subject.OU] = \"{trusted_signing_team_id}\""
    );
    let output = Command::new(CODESIGN)
        .args([
            OsStr::new("--verify"),
            OsStr::new("--strict"),
            OsStr::new("-R"),
        ])
        .arg(requirement)
        .arg(cli)
        .output()
        .map_err(|_| RunnerFailure::Unavailable)?;
    if !output.status.success() {
        return Err(RunnerFailure::Untrusted);
    }
    // Darwin cannot execute a previously verified descriptor. The remaining
    // path race is inside Shipyard's explicit trusted-same-UID boundary, the
    // same authority that owns cmux.app and Shipyard's machine policy.
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn verify_authorized_cmux(
    _endpoint: &CmuxEndpointV1,
    _trusted_signing_team_id: &str,
) -> Result<(), RunnerFailure> {
    Err(RunnerFailure::Unavailable)
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn verify_cmux_signing_policy(
    endpoint: &CmuxEndpointV1,
    trusted_signing_team_id: &str,
) -> Result<(), RunnerFailure> {
    if endpoint.signing_team_id != trusted_signing_team_id {
        return Err(RunnerFailure::Untrusted);
    }
    Ok(())
}
