//! Durable multi-host node registry and invite state.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Directory name under the Shipyard state root for multi-host state.
pub const MULTI_HOST_DIR: &str = "multi-host";

/// Durable registry file name.
const NODES_FILE: &str = "nodes.json";
/// Durable invite file name.
const INVITES_FILE: &str = "invites.json";

/// Supported node role.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    /// Controller node, the single writer for shared state.
    Controller,
    /// Client that can submit work to a controller.
    Client,
    /// Worker capacity node.
    Worker,
}

/// Supported controller endpoint kind.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeEndpointKind {
    /// Tailscale `MagicDNS` HTTPS endpoint.
    TailscaleDns,
    /// Tailscale IP HTTPS endpoint.
    TailscaleIp,
    /// Pinned HTTPS endpoint on a local network.
    LanHttps,
    /// SSH transport fallback.
    Ssh,
}

/// One reachable controller endpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeEndpoint {
    /// Endpoint kind.
    pub kind: NodeEndpointKind,
    /// URL or SSH host URL.
    pub url: String,
    /// Optional pinned certificate fingerprint for LAN HTTPS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_sha256: Option<String>,
}

impl NodeEndpoint {
    /// Validate that the endpoint does not permit plaintext LAN RPC.
    pub fn validate(&self) -> Result<(), NodeRegistryError> {
        match self.kind {
            NodeEndpointKind::TailscaleDns
            | NodeEndpointKind::TailscaleIp
            | NodeEndpointKind::LanHttps => {
                if !self.url.starts_with("https://") {
                    return Err(NodeRegistryError::new(
                        "controller HTTP endpoints must use https://; plaintext LAN HTTP is not supported",
                    ));
                }
                if self.kind == NodeEndpointKind::LanHttps && self.cert_sha256.is_none() {
                    return Err(NodeRegistryError::new(
                        "LAN HTTPS endpoints must include a pinned cert_sha256 fingerprint",
                    ));
                }
            }
            NodeEndpointKind::Ssh => {
                if !self.url.starts_with("ssh://") {
                    return Err(NodeRegistryError::new("SSH endpoints must use ssh://"));
                }
            }
        }
        Ok(())
    }
}

/// One registered machine.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeRecord {
    /// Stable machine id.
    pub machine_id: String,
    /// Human-readable node name.
    pub name: String,
    /// Node role.
    pub role: NodeRole,
    /// Capability labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Ordered reachable endpoints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<NodeEndpoint>,
    /// SHA-256 hash of this node's bearer token, when paired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_hash: Option<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last heartbeat seen by the controller.
    pub last_seen_at: DateTime<Utc>,
    /// Revocation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
}

/// One pending pairing invite.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeInvite {
    /// Invite id.
    pub invite_id: String,
    /// Intended node display name.
    pub name: String,
    /// SHA-256 hash of one-time join token.
    pub token_hash: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Expiry time.
    pub expires_at: DateTime<Utc>,
}

/// Registry command error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeRegistryError {
    message: String,
}

impl NodeRegistryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for NodeRegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for NodeRegistryError {}

impl From<io::Error> for NodeRegistryError {
    fn from(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<serde_json::Error> for NodeRegistryError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(error.to_string())
    }
}

/// JSON-backed registry store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeRegistryStore {
    state_dir: PathBuf,
}

impl NodeRegistryStore {
    /// Create a registry store under a Shipyard state root.
    #[must_use]
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    /// Registry path.
    #[must_use]
    pub fn nodes_path(&self) -> PathBuf {
        self.state_dir.join(MULTI_HOST_DIR).join(NODES_FILE)
    }

    /// Invite path.
    #[must_use]
    pub fn invites_path(&self) -> PathBuf {
        self.state_dir.join(MULTI_HOST_DIR).join(INVITES_FILE)
    }

    /// List all node records.
    pub fn list_nodes(&self) -> Result<Vec<NodeRecord>, NodeRegistryError> {
        read_json_array(&self.nodes_path())
    }

    /// Upsert one node record.
    pub fn upsert_node(&self, mut node: NodeRecord) -> Result<NodeRecord, NodeRegistryError> {
        for endpoint in &node.endpoints {
            endpoint.validate()?;
        }
        let mut nodes = self.list_nodes()?;
        node.last_seen_at = Utc::now();
        match nodes
            .iter()
            .position(|existing| existing.machine_id == node.machine_id)
        {
            Some(index) => {
                let created_at = nodes[index].created_at;
                node.created_at = created_at;
                nodes[index] = node.clone();
            }
            None => nodes.push(node.clone()),
        }
        write_json_array(&self.nodes_path(), &nodes)?;
        Ok(node)
    }

    /// Mark a node revoked. Returns true when a record was found.
    pub fn revoke_node(&self, machine_id: &str) -> Result<bool, NodeRegistryError> {
        let mut nodes = self.list_nodes()?;
        let Some(node) = nodes.iter_mut().find(|node| node.machine_id == machine_id) else {
            return Ok(false);
        };
        node.revoked_at = Some(Utc::now());
        node.token_hash = None;
        write_json_array(&self.nodes_path(), &nodes)?;
        Ok(true)
    }

    /// Create a one-time invite. The returned token is shown once.
    pub fn create_invite(
        &self,
        name: &str,
        ttl_minutes: i64,
    ) -> Result<(NodeInvite, String), NodeRegistryError> {
        let token = generate_secret("syjoin")?;
        let invite_id = generate_secret("syinvite")?;
        let now = Utc::now();
        let invite = NodeInvite {
            invite_id,
            name: name.to_owned(),
            token_hash: token_hash(&token),
            created_at: now,
            expires_at: now + Duration::minutes(ttl_minutes.max(1)),
        };
        let mut invites: Vec<NodeInvite> = read_json_array(&self.invites_path())?;
        invites.retain(|invite| invite.expires_at > now);
        invites.push(invite.clone());
        write_json_array(&self.invites_path(), &invites)?;
        Ok((invite, token))
    }
}

fn read_json_array<T>(path: &Path) -> Result<Vec<T>, NodeRegistryError>
where
    T: for<'de> Deserialize<'de>,
{
    match fs::read_to_string(path) {
        Ok(raw) => {
            if raw.trim().is_empty() {
                Ok(Vec::new())
            } else {
                Ok(serde_json::from_str(&raw)?)
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn write_json_array<T>(path: &Path, values: &[T]) -> Result<(), NodeRegistryError>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_string_pretty(values)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

/// Return the SHA-256 hash for a bearer or invite token.
#[must_use]
pub fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

fn generate_secret(prefix: &str) -> Result<String, NodeRegistryError> {
    let mut bytes = [0u8; 24];
    fill_random(&mut bytes)?;
    Ok(format!("{prefix}_{}", hex::encode(bytes)))
}

fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    crate::random::fill_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_upserts_and_revokes_nodes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = NodeRegistryStore::new(temp.path());
        let now = Utc::now();

        let node = store
            .upsert_node(NodeRecord {
                machine_id: "sy_node_abc".to_owned(),
                name: "mac-studio".to_owned(),
                role: NodeRole::Controller,
                capabilities: vec!["macos".to_owned()],
                endpoints: vec![NodeEndpoint {
                    kind: NodeEndpointKind::Ssh,
                    url: "ssh://mac-studio".to_owned(),
                    cert_sha256: None,
                }],
                token_hash: None,
                created_at: now,
                last_seen_at: now,
                revoked_at: None,
            })
            .expect("upsert");

        assert_eq!(node.name, "mac-studio");
        assert_eq!(store.list_nodes().expect("nodes").len(), 1);
        assert!(store.revoke_node("sy_node_abc").expect("revoke"));
        let nodes = store.list_nodes().expect("nodes");
        assert!(nodes[0].revoked_at.is_some());
        assert_eq!(nodes[0].token_hash, None);
    }

    #[test]
    fn registry_rejects_plaintext_lan_http() {
        let endpoint = NodeEndpoint {
            kind: NodeEndpointKind::LanHttps,
            url: "http://192.168.1.10:8765".to_owned(),
            cert_sha256: Some("abc".to_owned()),
        };

        let error = endpoint.validate().expect_err("plaintext");

        assert!(error.to_string().contains("https://"));
    }

    #[test]
    fn registry_requires_lan_cert_pin() {
        let endpoint = NodeEndpoint {
            kind: NodeEndpointKind::LanHttps,
            url: "https://192.168.1.10:8765".to_owned(),
            cert_sha256: None,
        };

        let error = endpoint.validate().expect_err("pin");

        assert!(error.to_string().contains("cert_sha256"));
    }

    #[test]
    fn invite_stores_only_token_hash() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = NodeRegistryStore::new(temp.path());

        let (invite, token) = store.create_invite("m5", 15).expect("invite");
        let raw = fs::read_to_string(store.invites_path()).expect("invites");

        assert!(token.starts_with("syjoin_"));
        assert_eq!(invite.token_hash, token_hash(&token));
        assert!(!raw.contains(&token));
    }
}
