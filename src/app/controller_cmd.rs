use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Utc;
use serde_json::Value;
use toml::{Table, Value as TomlValue};

use super::{
    CliFailure,
    cli::{ControllerCommand, NodeCommand},
};
use crate::config::{LoadedConfig, LocalOverlaySource};
use crate::identity::{ProductIdentity, RuntimeMode};
use crate::machine_identity::get_or_create_machine_id;
use crate::node_registry::{
    NodeEndpoint, NodeEndpointKind, NodeRecord, NodeRegistryStore, NodeRole,
};
use crate::output::write_json_envelope;

pub(super) fn controller_command<W: Write>(
    command: ControllerCommand,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        ControllerCommand::Init { name, endpoints } => {
            controller_init(mode, cwd, state_dir, name, &endpoints, json_mode, stdout)?;
        }
        ControllerCommand::Invite { name, ttl_minutes } => {
            controller_invite(state_dir, &name, ttl_minutes, json_mode, stdout)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn node_command<W: Write>(
    command: NodeCommand,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        NodeCommand::List => node_list(state_dir, json_mode, stdout)?,
        NodeCommand::Remove { machine_id } => {
            node_remove(state_dir, &machine_id, json_mode, stdout)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn controller_init<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    name: Option<String>,
    endpoint_specs: &[String],
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let machine_id = get_or_create_machine_id(state_dir)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let name = name.unwrap_or_else(default_node_name);
    let endpoints = endpoint_specs
        .iter()
        .map(|spec| parse_endpoint_spec(spec))
        .collect::<Result<Vec<_>, _>>()?;
    let now = Utc::now();
    let store = NodeRegistryStore::new(state_dir);
    let node = store
        .upsert_node(NodeRecord {
            machine_id: machine_id.clone(),
            name: name.clone(),
            role: NodeRole::Controller,
            capabilities: local_capabilities(),
            endpoints: endpoints.clone(),
            token_hash: None,
            created_at: now,
            last_seen_at: now,
            revoked_at: None,
        })
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let config_path = write_controller_local_config(mode, cwd, &name)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;

    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("machine_id".to_owned(), Value::String(machine_id));
        data.insert("name".to_owned(), Value::String(name));
        data.insert(
            "config_path".to_owned(),
            Value::String(config_path.display().to_string()),
        );
        data.insert(
            "node".to_owned(),
            serde_json::to_value(&node).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        return write_json_envelope(stdout, "controller.init", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }

    writeln!(stdout, "Initialized controller {name} ({machine_id})")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "Config: {}", config_path.display())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if endpoints.is_empty() {
        writeln!(
            stdout,
            "No endpoints registered yet. Add Tailscale, pinned LAN HTTPS, or SSH endpoints before pairing nodes."
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn controller_invite<W: Write>(
    state_dir: &Path,
    name: &str,
    ttl_minutes: i64,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let store = NodeRegistryStore::new(state_dir);
    let (invite, token) = store
        .create_invite(name, ttl_minutes)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert(
            "invite".to_owned(),
            serde_json::to_value(&invite).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert("token".to_owned(), Value::String(token));
        data.insert(
            "expires_at".to_owned(),
            Value::String(invite.expires_at.to_rfc3339()),
        );
        return write_json_envelope(stdout, "controller.invite", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(stdout, "Invite for {name} expires at {}", invite.expires_at)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "Token: {token}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(
        stdout,
        "Use this token once with the future controller join command. The stored invite contains only a token hash."
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn node_list<W: Write>(
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let nodes = NodeRegistryStore::new(state_dir)
        .list_nodes()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert(
            "nodes".to_owned(),
            serde_json::to_value(&nodes).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        return write_json_envelope(stdout, "node.list", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    if nodes.is_empty() {
        writeln!(stdout, "No nodes registered.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    writeln!(stdout, "Nodes").map_err(|error| CliFailure::new(1, error.to_string()))?;
    for node in nodes {
        let revoked = if node.revoked_at.is_some() {
            " revoked"
        } else {
            ""
        };
        writeln!(
            stdout,
            "  {} {:<12} role={:?} endpoints={}{}",
            node.machine_id,
            node.name,
            node.role,
            node.endpoints.len(),
            revoked
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn node_remove<W: Write>(
    state_dir: &Path,
    machine_id: &str,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let removed = NodeRegistryStore::new(state_dir)
        .revoke_node(machine_id)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert(
            "machine_id".to_owned(),
            Value::String(machine_id.to_owned()),
        );
        data.insert("removed".to_owned(), Value::Bool(removed));
        return write_json_envelope(stdout, "node.remove", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    if removed {
        writeln!(stdout, "Revoked node {machine_id}")
    } else {
        writeln!(stdout, "Node {machine_id} was not registered")
    }
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn parse_endpoint_spec(spec: &str) -> Result<NodeEndpoint, CliFailure> {
    let Some((kind, raw_url)) = spec.split_once('=') else {
        return Err(CliFailure::new(
            1,
            "endpoint must be kind=url, for example tailscale-dns=https://mac.ts.net:8765",
        ));
    };
    let (url, cert_sha256) = match raw_url.split_once("#sha256=") {
        Some((url, fingerprint)) => (url.to_owned(), Some(fingerprint.to_owned())),
        None => (raw_url.to_owned(), None),
    };
    let kind = match kind {
        "tailscale-dns" => NodeEndpointKind::TailscaleDns,
        "tailscale-ip" => NodeEndpointKind::TailscaleIp,
        "lan-https" => NodeEndpointKind::LanHttps,
        "ssh" => NodeEndpointKind::Ssh,
        other => {
            return Err(CliFailure::new(
                1,
                format!("unsupported endpoint kind {other:?}"),
            ));
        }
    };
    let endpoint = NodeEndpoint {
        kind,
        url,
        cert_sha256,
    };
    endpoint
        .validate()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(endpoint)
}

fn write_controller_local_config(
    mode: RuntimeMode,
    cwd: &Path,
    name: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)?;
    let identity = ProductIdentity::for_mode(mode);
    let local_dir = match config.local_overlay_source {
        LocalOverlaySource::Direct => config
            .local_dir
            .unwrap_or_else(|| cwd.join(identity.local_overlay_dir_name)),
        LocalOverlaySource::WorktreeFallback | LocalOverlaySource::None => {
            cwd.join(identity.local_overlay_dir_name)
        }
    };
    let path = local_dir.join("config.toml");
    let mut table = if path.exists() {
        fs::read_to_string(&path)?.parse::<Table>()?
    } else {
        Table::new()
    };
    let multi_host = table
        .entry("multi_host".to_owned())
        .or_insert_with(|| TomlValue::Table(Table::new()))
        .as_table_mut()
        .ok_or("multi_host config section must be a table")?;
    let controller = multi_host
        .entry("controller".to_owned())
        .or_insert_with(|| TomlValue::Table(Table::new()))
        .as_table_mut()
        .ok_or("multi_host.controller config section must be a table")?;
    controller.insert("enabled".to_owned(), TomlValue::Boolean(true));
    controller.insert("name".to_owned(), TomlValue::String(name.to_owned()));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, format!("{table}\n"))?;
    Ok(path)
}

fn default_node_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "shipyard-controller".to_owned())
}

fn local_capabilities() -> Vec<String> {
    let mut capabilities = Vec::new();
    if cfg!(target_os = "macos") {
        capabilities.push("macos".to_owned());
    } else if cfg!(target_os = "linux") {
        capabilities.push("linux".to_owned());
    } else if cfg!(target_os = "windows") {
        capabilities.push("windows".to_owned());
    }
    if cfg!(target_arch = "aarch64") {
        capabilities.push("arm64".to_owned());
    } else if cfg!(target_arch = "x86_64") {
        capabilities.push("x64".to_owned());
    }
    capabilities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parser_rejects_plain_http_lan() {
        let error = parse_endpoint_spec("lan-https=http://192.168.1.2:8765#sha256=abc")
            .expect_err("plain http");

        assert!(error.message.contains("https://"));
    }

    #[test]
    fn endpoint_parser_accepts_pinned_lan_https() {
        let endpoint =
            parse_endpoint_spec("lan-https=https://192.168.1.2:8765#sha256=abc").expect("endpoint");

        assert_eq!(endpoint.kind, NodeEndpointKind::LanHttps);
        assert_eq!(endpoint.cert_sha256.as_deref(), Some("abc"));
    }
}
