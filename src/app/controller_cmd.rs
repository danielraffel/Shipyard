use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use chrono::Utc;
use serde_json::Value;
use toml::{Table, Value as TomlValue};

use super::watch_cmd::{WatchCommandContext, WatchCommandOptions, watch};
use super::{
    CliFailure,
    cli::{ControllerCommand, NodeCommand},
    queue_cmd::{evidence_command, logs_command, queue_command, status_command},
};
use crate::config::{LoadedConfig, LocalOverlaySource};
use crate::evidence::EvidenceStore;
use crate::executor::ssh::shlex_quote;
use crate::identity::{ProductIdentity, RuntimeMode};
use crate::machine_identity::get_or_create_machine_id;
use crate::node_registry::{
    NodeEndpoint, NodeEndpointKind, NodeJoin, NodeRecord, NodeRegistryStore, NodeRole,
};
use crate::output::write_json_envelope;
use crate::ship_state::ShipStateStore;
use wait_timeout::ChildExt;

const CONTROLLER_SSH_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn controller_command<W: Write>(
    command: ControllerCommand,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        ControllerCommand::Status => {
            controller_status(mode, cwd, state_dir, json_mode, stdout)?;
        }
        ControllerCommand::Init { name, endpoints } => {
            controller_init(mode, cwd, state_dir, name, &endpoints, json_mode, stdout)?;
        }
        ControllerCommand::Invite { name, ttl_minutes } => {
            controller_invite(state_dir, &name, ttl_minutes, json_mode, stdout)?;
        }
        ControllerCommand::Join {
            name,
            controller,
            token,
        } => {
            controller_join(
                mode,
                cwd,
                state_dir,
                name,
                &controller,
                &token,
                json_mode,
                stdout,
            )?;
        }
        ControllerCommand::AcceptJoin {
            name,
            machine_id,
            token,
            token_stdin,
            capabilities,
        } => {
            let token = read_token_argument(token.as_deref(), token_stdin)?;
            controller_accept_join(
                state_dir,
                &name,
                &machine_id,
                &token,
                capabilities,
                json_mode,
                stdout,
            )?;
        }
        rpc @ (ControllerCommand::RpcStatus { .. }
        | ControllerCommand::RpcQueue { .. }
        | ControllerCommand::RpcLogs { .. }
        | ControllerCommand::RpcEvidence { .. }
        | ControllerCommand::RpcNodeList { .. }
        | ControllerCommand::RpcWatch { .. }) => {
            controller_rpc_command(rpc, mode, cwd, state_dir, json_mode, stdout)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn controller_rpc_command<W: Write>(
    command: ControllerCommand,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    match command {
        ControllerCommand::RpcStatus {
            machine_id,
            token_stdin,
        } => controller_rpc_status(
            mode,
            cwd,
            state_dir,
            &machine_id,
            token_stdin,
            json_mode,
            stdout,
        ),
        ControllerCommand::RpcQueue {
            machine_id,
            token_stdin,
        } => controller_rpc_queue(state_dir, &machine_id, token_stdin, json_mode, stdout),
        ControllerCommand::RpcLogs {
            machine_id,
            job_id,
            target,
            token_stdin,
        } => controller_rpc_logs(state_dir, &machine_id, token_stdin, &job_id, target, stdout),
        ControllerCommand::RpcEvidence {
            machine_id,
            branch,
            token_stdin,
        } => controller_rpc_evidence(
            cwd,
            state_dir,
            &machine_id,
            token_stdin,
            branch,
            json_mode,
            stdout,
        ),
        ControllerCommand::RpcNodeList {
            machine_id,
            token_stdin,
        } => controller_rpc_node_list(state_dir, &machine_id, token_stdin, json_mode, stdout),
        ControllerCommand::RpcWatch {
            machine_id,
            pr,
            branch,
            token_stdin,
        } => controller_rpc_watch(
            cwd,
            state_dir,
            &machine_id,
            token_stdin,
            pr,
            branch.as_deref(),
            json_mode,
            stdout,
        ),
        ControllerCommand::Status
        | ControllerCommand::Init { .. }
        | ControllerCommand::Invite { .. }
        | ControllerCommand::Join { .. }
        | ControllerCommand::AcceptJoin { .. } => {
            unreachable!("non-RPC controller command passed to RPC helper")
        }
    }
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

pub(super) fn leave_command<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let path = mutate_local_config(mode, cwd, |table| {
        let Some(multi_host) = table
            .get_mut("multi_host")
            .and_then(TomlValue::as_table_mut)
        else {
            return Ok(());
        };
        multi_host.remove("client");
        Ok(())
    })?;
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("path".to_owned(), Value::String(path.display().to_string()));
        return write_json_envelope(stdout, "leave", data)
            .map(|()| ExitCode::SUCCESS)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "Removed local controller client config from {}",
        path.display()
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
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

fn controller_status<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let machine_id = crate::machine_identity::existing_machine_id(state_dir)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .unwrap_or_else(|| "uninitialized".to_owned());
    let controller = config
        .data
        .get("multi_host")
        .and_then(TomlValue::as_table)
        .and_then(|multi| multi.get("controller"))
        .and_then(TomlValue::as_table);
    let client = config
        .data
        .get("multi_host")
        .and_then(TomlValue::as_table)
        .and_then(|multi| multi.get("client"))
        .and_then(TomlValue::as_table);
    let nodes = NodeRegistryStore::new(state_dir)
        .list_nodes()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("machine_id".to_owned(), Value::String(machine_id));
        data.insert(
            "controller_enabled".to_owned(),
            Value::Bool(
                controller
                    .and_then(|table| table.get("enabled"))
                    .and_then(TomlValue::as_bool)
                    == Some(true),
            ),
        );
        data.insert(
            "client_enabled".to_owned(),
            Value::Bool(
                client
                    .and_then(|table| table.get("enabled"))
                    .and_then(TomlValue::as_bool)
                    == Some(true),
            ),
        );
        data.insert(
            "client_controller".to_owned(),
            client
                .and_then(|table| table.get("controller"))
                .and_then(TomlValue::as_str)
                .map_or(Value::Null, |value| Value::String(value.to_owned())),
        );
        data.insert(
            "nodes".to_owned(),
            serde_json::to_value(nodes).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        return write_json_envelope(stdout, "controller.status", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(stdout, "Controller").map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "  machine_id: {machine_id}")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(
        stdout,
        "  controller_enabled: {}",
        controller
            .and_then(|table| table.get("enabled"))
            .and_then(TomlValue::as_bool)
            .unwrap_or(false)
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if let Some(endpoint) = client
        .and_then(|table| table.get("controller"))
        .and_then(TomlValue::as_str)
    {
        writeln!(stdout, "  client_controller: {endpoint}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    writeln!(stdout, "  registered_nodes: {}", nodes.len())
        .map_err(|error| CliFailure::new(1, error.to_string()))
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

#[allow(clippy::too_many_arguments)]
fn controller_join<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    name: Option<String>,
    controller: &str,
    token: &str,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let endpoint = parse_join_controller(controller)?;
    if endpoint.kind != NodeEndpointKind::Ssh {
        return Err(CliFailure::new(
            1,
            "controller join currently supports ssh:// endpoints only; HTTPS controller RPC will land after the pinned-TLS server is implemented",
        ));
    }
    let machine_id = get_or_create_machine_id(state_dir)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let name = name.unwrap_or_else(default_node_name);
    let capabilities = local_capabilities();
    let join = ssh_accept_join(&endpoint.url, &name, &machine_id, token, &capabilities)?;
    let config_path = write_client_local_config(mode, cwd, &endpoint.url, &join.bearer_token)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;

    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("machine_id".to_owned(), Value::String(machine_id));
        data.insert("controller".to_owned(), Value::String(endpoint.url));
        data.insert(
            "config_path".to_owned(),
            Value::String(config_path.display().to_string()),
        );
        data.insert(
            "node".to_owned(),
            serde_json::to_value(join.node)
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        return write_json_envelope(stdout, "controller.join", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(stdout, "Joined controller {}", endpoint.url)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "Config: {}", config_path.display())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(())
}

fn controller_accept_join<W: Write>(
    state_dir: &Path,
    name: &str,
    machine_id: &str,
    token: &str,
    capabilities: Vec<String>,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let join = NodeRegistryStore::new(state_dir)
        .accept_join(token, machine_id, name, capabilities)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert(
            "node".to_owned(),
            serde_json::to_value(&join.node)
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert("bearer_token".to_owned(), Value::String(join.bearer_token));
        return write_json_envelope(stdout, "controller.accept_join", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(stdout, "Registered node {} ({machine_id})", join.node.name)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn controller_rpc_status<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    machine_id: &str,
    token_stdin: bool,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    authenticate_rpc_node(state_dir, machine_id, token_stdin)?;
    status_command(mode, cwd, state_dir, json_mode, stdout)?;
    Ok(())
}

fn controller_rpc_queue<W: Write>(
    state_dir: &Path,
    machine_id: &str,
    token_stdin: bool,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    authenticate_rpc_node(state_dir, machine_id, token_stdin)?;
    queue_command(state_dir, json_mode, stdout)?;
    Ok(())
}

fn controller_rpc_logs<W: Write>(
    state_dir: &Path,
    machine_id: &str,
    token_stdin: bool,
    job_id: &str,
    target: Option<String>,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    authenticate_rpc_node(state_dir, machine_id, token_stdin)?;
    logs_command(job_id, target, state_dir, stdout)?;
    Ok(())
}

fn controller_rpc_evidence<W: Write>(
    cwd: &Path,
    state_dir: &Path,
    machine_id: &str,
    token_stdin: bool,
    branch: Option<String>,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    authenticate_rpc_node(state_dir, machine_id, token_stdin)?;
    evidence_command(branch, cwd, state_dir, json_mode, stdout)?;
    Ok(())
}

fn controller_rpc_node_list<W: Write>(
    state_dir: &Path,
    machine_id: &str,
    token_stdin: bool,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    authenticate_rpc_node(state_dir, machine_id, token_stdin)?;
    node_list(state_dir, json_mode, stdout)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn controller_rpc_watch<W: Write>(
    cwd: &Path,
    state_dir: &Path,
    machine_id: &str,
    token_stdin: bool,
    pr: Option<u64>,
    branch: Option<&str>,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    authenticate_rpc_node(state_dir, machine_id, token_stdin)?;
    let store = ShipStateStore::new(state_dir.join("ship"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let evidence = EvidenceStore::new(state_dir.join("evidence"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let pr = match (pr, branch) {
        (Some(pr), _) => Some(pr),
        (None, Some(branch)) => active_pr_for_branch(&store, branch),
        (None, None) => None,
    };
    watch(
        WatchCommandContext {
            store: &store,
            evidence_store: &evidence,
            cwd,
        },
        WatchCommandOptions {
            pr,
            follow: false,
            interval: 5.0,
            json: json_mode,
        },
        stdout,
    )?;
    Ok(())
}

fn active_pr_for_branch(store: &ShipStateStore, branch: &str) -> Option<u64> {
    store
        .list_active()
        .into_iter()
        .filter(|state| state.branch == branch)
        .max_by_key(|state| state.updated_at)
        .map(|state| state.pr)
}

fn authenticate_rpc_node(
    state_dir: &Path,
    machine_id: &str,
    token_stdin: bool,
) -> Result<(), CliFailure> {
    let token = read_rpc_token(token_stdin)?;
    NodeRegistryStore::new(state_dir)
        .authenticate_node(machine_id, &token)
        .map_err(|error| CliFailure::new(1, format!("auth denied: {error}")))?;
    Ok(())
}

fn read_rpc_token(token_stdin: bool) -> Result<String, CliFailure> {
    if token_stdin {
        return read_secret_from_stdin("controller RPC token");
    }
    std::env::var("SHIPYARD_NODE_TOKEN")
        .map_err(|_| CliFailure::new(1, "missing SHIPYARD_NODE_TOKEN for controller RPC"))
}

fn read_token_argument(token: Option<&str>, token_stdin: bool) -> Result<String, CliFailure> {
    match (token, token_stdin) {
        (Some(_), true) => Err(CliFailure::new(
            1,
            "use either --token or --token-stdin, not both",
        )),
        (Some(token), false) => Ok(token.to_owned()),
        (None, true) => read_secret_from_stdin("join token"),
        (None, false) => Err(CliFailure::new(1, "missing --token for accept-join")),
    }
}

fn read_secret_from_stdin(label: &str) -> Result<String, CliFailure> {
    let mut token = String::new();
    std::io::stdin()
        .read_to_string(&mut token)
        .map_err(|error| {
            CliFailure::new(1, format!("failed to read {label} from stdin: {error}"))
        })?;
    let token = token.trim_end_matches(['\r', '\n']).to_owned();
    if token.is_empty() {
        return Err(CliFailure::new(1, format!("{label} from stdin is empty")));
    }
    Ok(token)
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

fn parse_join_controller(controller: &str) -> Result<NodeEndpoint, CliFailure> {
    if controller.starts_with("ssh://") {
        return parse_endpoint_spec(&format!("ssh={controller}"));
    }
    if controller.starts_with("https://") {
        return parse_endpoint_spec(&format!("tailscale-dns={controller}"));
    }
    Err(CliFailure::new(
        1,
        "controller must be ssh://host or https://host:port",
    ))
}

fn write_controller_local_config(
    mode: RuntimeMode,
    cwd: &Path,
    name: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    mutate_local_config_boxed(mode, cwd, |table| {
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
        Ok(())
    })
}

fn write_client_local_config(
    mode: RuntimeMode,
    cwd: &Path,
    controller_endpoint: &str,
    bearer_token: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    mutate_local_config_boxed(mode, cwd, |table| {
        let multi_host = table
            .entry("multi_host".to_owned())
            .or_insert_with(|| TomlValue::Table(Table::new()))
            .as_table_mut()
            .ok_or("multi_host config section must be a table")?;
        let client = multi_host
            .entry("client".to_owned())
            .or_insert_with(|| TomlValue::Table(Table::new()))
            .as_table_mut()
            .ok_or("multi_host.client config section must be a table")?;
        client.insert("enabled".to_owned(), TomlValue::Boolean(true));
        client.insert(
            "controller".to_owned(),
            TomlValue::String(controller_endpoint.to_owned()),
        );
        client.insert(
            "node_token".to_owned(),
            TomlValue::String(bearer_token.to_owned()),
        );
        Ok(())
    })
}

fn mutate_local_config(
    mode: RuntimeMode,
    cwd: &Path,
    mutate: impl FnOnce(&mut Table) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<PathBuf, CliFailure> {
    mutate_local_config_boxed(mode, cwd, mutate)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn mutate_local_config_boxed(
    mode: RuntimeMode,
    cwd: &Path,
    mutate: impl FnOnce(&mut Table) -> Result<(), Box<dyn std::error::Error>>,
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
    mutate(&mut table)?;
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

fn ssh_accept_join(
    endpoint: &str,
    name: &str,
    machine_id: &str,
    token: &str,
    capabilities: &[String],
) -> Result<NodeJoin, CliFailure> {
    let host = endpoint
        .strip_prefix("ssh://")
        .ok_or_else(|| CliFailure::new(1, "SSH endpoint must start with ssh://"))?;
    let mut remote = vec![
        "shipyard".to_owned(),
        "--local-state".to_owned(),
        "controller".to_owned(),
        "accept-join".to_owned(),
        "--name".to_owned(),
        shlex_quote(name),
        "--machine-id".to_owned(),
        shlex_quote(machine_id),
        "--token-stdin".to_owned(),
        "--json".to_owned(),
    ];
    for capability in capabilities {
        remote.push("--capability".to_owned());
        remote.push(shlex_quote(capability));
    }
    let output = run_controller_ssh(host, &remote.join(" "), Some(token), "ssh join")?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "ssh join failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| CliFailure::new(1, format!("invalid join response: {error}")))?;
    let node = serde_json::from_value(value.get("node").cloned().unwrap_or(Value::Null))
        .map_err(|error| CliFailure::new(1, format!("invalid join node: {error}")))?;
    let bearer_token = value
        .get("bearer_token")
        .and_then(Value::as_str)
        .ok_or_else(|| CliFailure::new(1, "join response missing bearer_token"))?
        .to_owned();
    Ok(NodeJoin { node, bearer_token })
}

pub(super) fn remote_status_command<W: Write>(
    config: &LoadedConfig,
    machine_id: &str,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    remote_controller_command(
        config,
        machine_id,
        "rpc-status",
        json_mode,
        "status",
        stdout,
    )
}

pub(super) fn remote_queue_command<W: Write>(
    config: &LoadedConfig,
    machine_id: &str,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    remote_controller_command(config, machine_id, "rpc-queue", json_mode, "queue", stdout)
}

pub(super) fn remote_logs_command<W: Write>(
    config: &LoadedConfig,
    machine_id: &str,
    job_id: &str,
    target: Option<&str>,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let client = configured_client(config)?;
    let Some(endpoint) = client.controller.strip_prefix("ssh://") else {
        return Err(CliFailure::new(
            1,
            "configured controller is not reachable through the implemented SSH transport; use --local-state for local logs",
        ));
    };
    let remote = remote_controller_logs_shell_command(machine_id, job_id, target);
    let output = run_controller_ssh(
        endpoint,
        &remote,
        Some(&client.node_token),
        "controller logs",
    )?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "controller_unreachable: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    stdout
        .write_all(&output.stdout)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

pub(super) fn remote_evidence_command<W: Write>(
    config: &LoadedConfig,
    machine_id: &str,
    branch: Option<&str>,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let client = configured_client(config)?;
    let Some(endpoint) = client.controller.strip_prefix("ssh://") else {
        return Err(CliFailure::new(
            1,
            "configured controller is not reachable through the implemented SSH transport; use --local-state for local evidence",
        ));
    };
    let remote = remote_controller_evidence_shell_command(machine_id, branch, json_mode);
    let output = run_controller_ssh(
        endpoint,
        &remote,
        Some(&client.node_token),
        "controller evidence",
    )?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "controller_unreachable: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    stdout
        .write_all(&output.stdout)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

pub(super) fn remote_node_list_command<W: Write>(
    config: &LoadedConfig,
    machine_id: &str,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    remote_controller_command(
        config,
        machine_id,
        "rpc-node-list",
        json_mode,
        "node list",
        stdout,
    )
}

pub(super) fn remote_watch_command<W: Write>(
    config: &LoadedConfig,
    machine_id: &str,
    pr: Option<u64>,
    branch: Option<&str>,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let client = configured_client(config)?;
    let Some(endpoint) = client.controller.strip_prefix("ssh://") else {
        return Err(CliFailure::new(
            1,
            "configured controller is not reachable through the implemented SSH transport; use --local-state for local watch",
        ));
    };
    let remote = remote_controller_watch_shell_command(machine_id, pr, branch, json_mode);
    let output = run_controller_ssh(
        endpoint,
        &remote,
        Some(&client.node_token),
        "controller watch",
    )?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "controller_unreachable: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    stdout
        .write_all(&output.stdout)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

fn remote_controller_command<W: Write>(
    config: &LoadedConfig,
    machine_id: &str,
    rpc_command: &str,
    json_mode: bool,
    local_command_hint: &str,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    // This SSH command transport is intentionally limited to read-only,
    // no-payload RPCs. Mutating controller RPCs need an idempotent request
    // envelope before they can safely reuse this boundary.
    let client = configured_client(config)?;
    let Some(endpoint) = client.controller.strip_prefix("ssh://") else {
        return Err(CliFailure::new(
            1,
            format!(
                "configured controller is not reachable through the implemented SSH transport; use --local-state for local {local_command_hint}"
            ),
        ));
    };
    let remote = remote_controller_shell_command(rpc_command, machine_id, json_mode);
    let output = run_controller_ssh(
        endpoint,
        &remote,
        Some(&client.node_token),
        "controller RPC",
    )?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "controller_unreachable: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    stdout
        .write_all(&output.stdout)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

struct ControllerSshOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_controller_ssh(
    endpoint: &str,
    remote_command: &str,
    stdin_payload: Option<&str>,
    action: &str,
) -> Result<ControllerSshOutput, CliFailure> {
    let mut command = crate::supervised::supervised(Command::new("ssh"));
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(endpoint)
        .arg(remote_command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin_payload.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|error| CliFailure::new(1, format!("{action} failed to start: {error}")))?;
    if let Some(payload) = stdin_payload
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(payload.as_bytes()).map_err(|error| {
            CliFailure::new(1, format!("{action} failed to send token: {error}"))
        })?;
    }
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = thread::spawn(move || read_child_pipe(stdout));
    let stderr_reader = thread::spawn(move || read_child_pipe(stderr));
    let Some(status) = child
        .wait_timeout(CONTROLLER_SSH_TIMEOUT)
        .map_err(|error| CliFailure::new(1, format!("{action} wait failed: {error}")))?
    else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        return Err(CliFailure::new(
            1,
            format!(
                "{action} timed out after {}s",
                CONTROLLER_SSH_TIMEOUT.as_secs()
            ),
        ));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| CliFailure::new(1, format!("{action} stdout reader panicked")))?
        .map_err(|error| CliFailure::new(1, format!("{action} stdout read failed: {error}")))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| CliFailure::new(1, format!("{action} stderr reader panicked")))?
        .map_err(|error| CliFailure::new(1, format!("{action} stderr read failed: {error}")))?;
    Ok(ControllerSshOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_child_pipe(pipe: Option<impl Read>) -> Result<Vec<u8>, std::io::Error> {
    let mut output = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut output)?;
    }
    Ok(output)
}

fn remote_controller_logs_shell_command(
    machine_id: &str,
    job_id: &str,
    target: Option<&str>,
) -> String {
    let mut remote = format!(
        "shipyard --local-state controller rpc-logs --machine-id {} --token-stdin {}",
        shlex_quote(machine_id),
        shlex_quote(job_id),
    );
    if let Some(target) = target {
        remote.push_str(" --target ");
        remote.push_str(&shlex_quote(target));
    }
    remote
}

fn remote_controller_evidence_shell_command(
    machine_id: &str,
    branch: Option<&str>,
    json_mode: bool,
) -> String {
    let mut remote = format!(
        "shipyard --local-state controller rpc-evidence --machine-id {} --token-stdin",
        shlex_quote(machine_id),
    );
    if json_mode {
        remote.push_str(" --json");
    }
    if let Some(branch) = branch {
        remote.push(' ');
        remote.push_str(&shlex_quote(branch));
    }
    remote
}

fn remote_controller_watch_shell_command(
    machine_id: &str,
    pr: Option<u64>,
    branch: Option<&str>,
    json_mode: bool,
) -> String {
    let mut remote = format!(
        "shipyard --local-state controller rpc-watch --machine-id {} --token-stdin",
        shlex_quote(machine_id),
    );
    if json_mode {
        remote.push_str(" --json");
    }
    if let Some(pr) = pr {
        remote.push_str(" --pr ");
        remote.push_str(&pr.to_string());
    }
    if let Some(branch) = branch {
        remote.push_str(" --branch ");
        remote.push_str(&shlex_quote(branch));
    }
    remote
}

fn remote_controller_shell_command(rpc_command: &str, machine_id: &str, json_mode: bool) -> String {
    format!(
        "shipyard --local-state controller {} --machine-id {} --token-stdin {}",
        shlex_quote(rpc_command),
        shlex_quote(machine_id),
        if json_mode { "--json" } else { "" }
    )
}

struct ConfiguredClient {
    controller: String,
    node_token: String,
}

pub(super) fn configured_client_enabled(config: &LoadedConfig) -> bool {
    config
        .data
        .get("multi_host")
        .and_then(TomlValue::as_table)
        .and_then(|multi| multi.get("client"))
        .and_then(TomlValue::as_table)
        .and_then(|client| client.get("enabled"))
        .and_then(TomlValue::as_bool)
        == Some(true)
}

fn configured_client(config: &LoadedConfig) -> Result<ConfiguredClient, CliFailure> {
    let client = config
        .data
        .get("multi_host")
        .and_then(TomlValue::as_table)
        .and_then(|multi| multi.get("client"))
        .and_then(TomlValue::as_table)
        .ok_or_else(|| CliFailure::new(1, "multi_host.client is not configured"))?;
    let controller = client
        .get("controller")
        .and_then(TomlValue::as_str)
        .ok_or_else(|| CliFailure::new(1, "multi_host.client.controller is missing"))?
        .to_owned();
    let node_token = client
        .get("node_token")
        .and_then(TomlValue::as_str)
        .ok_or_else(|| CliFailure::new(1, "multi_host.client.node_token is missing"))?
        .to_owned();
    Ok(ConfiguredClient {
        controller,
        node_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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

    #[test]
    fn client_config_marks_controller_enabled_and_leave_removes_it() {
        let temp = TempDir::new().expect("tempdir");
        write_client_local_config(
            RuntimeMode::Shipyard,
            temp.path(),
            "ssh://mac-studio",
            "synode_secret",
        )
        .expect("client config");
        let loaded =
            LoadedConfig::load_from_cwd(RuntimeMode::Shipyard, temp.path()).expect("config");
        assert!(configured_client_enabled(&loaded));

        let mut stdout = Vec::new();
        leave_command(RuntimeMode::Shipyard, temp.path(), true, &mut stdout).expect("leave");
        let loaded =
            LoadedConfig::load_from_cwd(RuntimeMode::Shipyard, temp.path()).expect("config");

        assert!(!configured_client_enabled(&loaded));
        assert!(serde_json::from_slice::<Value>(&stdout).is_ok());
    }

    #[test]
    fn accept_join_registers_client_and_returns_token_once() {
        let temp = TempDir::new().expect("tempdir");
        let store = NodeRegistryStore::new(temp.path());
        let (_invite, token) = store.create_invite("m5", 15).expect("invite");
        let mut stdout = Vec::new();

        controller_accept_join(
            temp.path(),
            "m5",
            "sy_node_client",
            &token,
            vec!["macos".to_owned()],
            true,
            &mut stdout,
        )
        .expect("accept join");
        let payload: Value = serde_json::from_slice(&stdout).expect("json");
        let bearer = payload["bearer_token"].as_str().expect("bearer");

        assert!(bearer.starts_with("synode_"));
        assert_eq!(payload["node"]["machine_id"], "sy_node_client");
        assert!(
            store
                .authenticate_node("sy_node_client", bearer)
                .expect("auth")
                .token_hash
                .is_some()
        );
    }

    #[test]
    fn remote_controller_shell_command_targets_authenticated_queue_rpc() {
        let command = remote_controller_shell_command("rpc-queue", "sy_node_client", true);

        assert!(command.contains("shipyard --local-state controller rpc-queue"));
        assert!(command.contains("--machine-id sy_node_client"));
        assert!(command.contains("--token-stdin"));
        assert!(command.ends_with("--json"));
        assert!(!command.contains("synode_secret"));
    }

    #[test]
    fn remote_controller_logs_shell_command_targets_authenticated_logs_rpc() {
        let command =
            remote_controller_logs_shell_command("sy_node_client", "sy-job", Some("mac target"));

        assert!(command.contains("shipyard --local-state controller rpc-logs"));
        assert!(command.contains("--machine-id sy_node_client"));
        assert!(command.contains("--token-stdin"));
        assert!(command.contains(" sy-job --target 'mac target'"));
        assert!(!command.contains("synode secret"));
    }

    #[test]
    fn remote_controller_evidence_shell_command_targets_authenticated_evidence_rpc() {
        let command = remote_controller_evidence_shell_command(
            "sy_node_client",
            Some("feature/test branch"),
            true,
        );

        assert!(command.contains("shipyard --local-state controller rpc-evidence"));
        assert!(command.contains("--machine-id sy_node_client"));
        assert!(command.contains("--token-stdin"));
        assert!(command.contains("--json 'feature/test branch'"));
        assert!(!command.contains("synode secret"));
    }

    #[test]
    fn remote_controller_shell_command_targets_authenticated_node_list_rpc() {
        let command = remote_controller_shell_command("rpc-node-list", "sy_node_client", true);

        assert!(command.contains("shipyard --local-state controller rpc-node-list"));
        assert!(command.contains("--machine-id sy_node_client"));
        assert!(command.contains("--token-stdin"));
        assert!(command.ends_with("--json"));
        assert!(!command.contains("synode_secret"));
    }

    #[test]
    fn remote_controller_watch_shell_command_targets_authenticated_watch_rpc() {
        let command = remote_controller_watch_shell_command(
            "sy_node_client",
            Some(319),
            Some("feature/test branch"),
            true,
        );

        assert!(command.contains("shipyard --local-state controller rpc-watch"));
        assert!(command.contains("--machine-id sy_node_client"));
        assert!(command.contains("--token-stdin"));
        assert!(command.contains("--json"));
        assert!(command.contains("--pr 319"));
        assert!(command.contains("--branch 'feature/test branch'"));
        assert!(!command.contains("synode_secret"));
    }
}
