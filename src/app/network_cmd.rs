use std::collections::BTreeMap;
use std::io::Write;
use std::process::ExitCode;

use serde_json::{Value, json};

use super::{
    CliFailure,
    cli::{NetworkCommand, NetworkTailscaleCommand},
};
use crate::output::write_json_envelope;
use crate::tunnel::{TailscaleStatus, probe_tailscale};

pub(super) fn network_command<W: Write>(
    command: NetworkCommand,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        NetworkCommand::Tailscale { command } => match command {
            NetworkTailscaleCommand::Status => tailscale_status(json_mode, stdout)?,
        },
    }
    Ok(ExitCode::SUCCESS)
}

fn tailscale_status<W: Write>(json_mode: bool, stdout: &mut W) -> Result<(), CliFailure> {
    let status = probe_tailscale();
    write_tailscale_status(&status, json_mode, stdout)
}

fn write_tailscale_status<W: Write>(
    status: &TailscaleStatus,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let reachable = status.is_tailnet_reachable();
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("available".to_owned(), json!(status.binary_path.is_some()));
        data.insert(
            "binary_path".to_owned(),
            status
                .binary_path
                .as_ref()
                .map_or(Value::Null, |path| json!(path.to_string_lossy())),
        );
        data.insert("backend_state".to_owned(), json!(&status.backend_state));
        data.insert("dns_name".to_owned(), json!(status.magic_dns_name()));
        data.insert("tailscale_ips".to_owned(), json!(&status.tailscale_ips));
        data.insert("online".to_owned(), json!(&status.online));
        data.insert("tailnet_reachable".to_owned(), json!(reachable));
        data.insert("funnel_ready".to_owned(), json!(status.is_ready()));
        return write_json_envelope(stdout, "network.tailscale.status", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }

    writeln!(stdout, "Tailscale").map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(
        stdout,
        "  binary: {}",
        status
            .binary_path
            .as_ref()
            .map_or_else(|| "not found".to_owned(), |path| path.display().to_string())
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(
        stdout,
        "  backend: {}",
        status.backend_state.as_deref().unwrap_or("unknown")
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(
        stdout,
        "  tailnet: {}",
        if reachable {
            "reachable"
        } else {
            "not reachable"
        }
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if let Some(dns_name) = status.magic_dns_name() {
        writeln!(stdout, "  dns: {dns_name}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    if !status.tailscale_ips.is_empty() {
        writeln!(stdout, "  ips: {}", status.tailscale_ips.join(", "))
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    if let Some(online) = status.online {
        writeln!(stdout, "  online: {online}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    writeln!(
        stdout,
        "  funnel: {}",
        if status.is_ready() {
            "ready"
        } else {
            "not ready"
        }
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::Value;

    use super::*;

    fn sample_status() -> TailscaleStatus {
        TailscaleStatus {
            binary_path: Some(PathBuf::from(
                "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
            )),
            backend_state: Some("Running".to_owned()),
            dns_name: Some("mac-studio.example.ts.net.".to_owned()),
            tailscale_ips: vec!["100.64.0.1".to_owned(), "fd7a:115c:a1e0::1".to_owned()],
            online: Some(true),
            funnel_permitted: false,
        }
    }

    #[test]
    fn tailscale_status_json_reports_tailnet_separate_from_funnel() {
        let mut stdout = Vec::new();
        write_tailscale_status(&sample_status(), true, &mut stdout).expect("write status");

        let body: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(body["command"], "network.tailscale.status");
        assert_eq!(body["available"], true);
        assert_eq!(body["backend_state"], "Running");
        assert_eq!(body["dns_name"], "mac-studio.example.ts.net");
        assert_eq!(body["tailscale_ips"][0], "100.64.0.1");
        assert_eq!(body["tailnet_reachable"], true);
        assert_eq!(body["funnel_ready"], false);
    }

    #[test]
    fn tailscale_status_human_reports_tailnet_separate_from_funnel() {
        let mut stdout = Vec::new();
        write_tailscale_status(&sample_status(), false, &mut stdout).expect("write status");
        let rendered = String::from_utf8(stdout).expect("utf8");

        assert!(rendered.contains("tailnet: reachable"));
        assert!(rendered.contains("dns: mac-studio.example.ts.net"));
        assert!(rendered.contains("funnel: not ready"));
    }
}
