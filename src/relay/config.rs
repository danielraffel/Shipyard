//! `[relay]` in the machine-global config: host policy, never a repository's.
//!
//! ```toml
//! [relay]
//! enabled = true            # false is the rollback switch: the relay does nothing
//! mode = "shadow"           # "shadow" logs what it would send; "send" delivers
//! host_name = "m3"          # this host's short name in owner records
//! interval_secs = 120       # --follow cadence
//! rate_limit_minutes = 10   # one message per pull request per window
//! repos = ["Generous-Corp/pulp", "Generous-Corp/vellum"]
//! events = ["merged", "closed", "ejected", "verdict", "required_check",
//!           "dirty_after_base_move", "claim_taken", "stacked_base_merged", "overdue"]
//! sockets = ["/tmp/cmux-nightly.sock", ""]   # "" is cmux's default socket
//! comment_fallback = true
//! steward_tick_dir = "/Users/me/Library/Logs/shipyard-steward-tick"
//! steward_tick_max_age_secs = 600
//! admit_ready = "off"       # "report" or "apply": run the steward's admission
//!                           # when a verdict or green check makes a PR ready
//!
//! [relay.hosts.m5]
//! ssh = "m5"                # ssh destination; defaults to the host name
//! cmux = "/opt/homebrew/bin/cmux"
//!
//! [relay.repo."Generous-Corp/pulp"]
//! lead = "m3/surface:1000120036"
//! repair_lane = "m1/surface:1000010002"
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use toml::Table;
use toml::Value as TomlValue;

/// Delivery mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    /// Log what would be sent; send nothing.
    Shadow,
    /// Deliver.
    Send,
}

impl Mode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::Send => "send",
        }
    }
}

/// What the relay does when an event makes a pull request ready (approved at
/// its head, required checks green, open, not queued): nothing, a dry-run of
/// the steward's admission, or the admission itself. Enable `apply` only on
/// the host that runs the steward: the admission takes that host's steward
/// lock and mutation authority.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum AdmitReady {
    #[default]
    Off,
    Report,
    Apply,
}

/// How the relay reaches one host's cmux.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HostRoute {
    /// Short host name, as in owner records.
    pub(crate) name: String,
    /// `None` runs cmux on this host; `Some` runs it over ssh.
    pub(crate) ssh: Option<String>,
    /// The cmux executable on that host.
    pub(crate) cmux: String,
}

/// A surface on a host: `host/surface`.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Address {
    pub(crate) host: String,
    pub(crate) surface: String,
    /// The socket the record named, tried first.
    pub(crate) socket: Option<String>,
}

impl Address {
    /// Parse `host/surface`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let (host, surface) = text
            .split_once('/')
            .ok_or_else(|| format!("{text:?} is not host/surface"))?;
        if !crate::owner_registry::safe_value(host) || !crate::owner_registry::safe_value(surface) {
            return Err(format!("{text:?} has characters a transport cannot carry"));
        }
        Ok(Self {
            host: host.to_owned(),
            surface: surface.to_owned(),
            socket: None,
        })
    }
}

/// Fallbacks for one repository.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RepoFallbacks {
    pub(crate) lead: Option<Address>,
    pub(crate) repair_lane: Option<Address>,
}

/// The whole `[relay]` table, with defaults filled in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RelayConfig {
    pub(crate) enabled: bool,
    /// The machine config has a `[relay]` table. Without one the host is not
    /// part of a relay fleet and pushes register no owner.
    pub(crate) configured: bool,
    pub(crate) mode: Mode,
    pub(crate) host_name: Option<String>,
    pub(crate) interval_secs: u64,
    pub(crate) rate_limit_minutes: i64,
    pub(crate) repos: Vec<String>,
    pub(crate) events: BTreeSet<String>,
    pub(crate) sockets: Vec<String>,
    pub(crate) comment_fallback: bool,
    pub(crate) steward_tick_dir: Option<PathBuf>,
    pub(crate) steward_tick_max_age_secs: u64,
    pub(crate) admit_ready: AdmitReady,
    pub(crate) review_gates: BTreeSet<String>,
    pub(crate) hosts: BTreeMap<String, HostRoute>,
    pub(crate) repo: BTreeMap<String, RepoFallbacks>,
}

/// Events delivered when `relay.events` is absent.
pub(crate) const DEFAULT_EVENTS: [&str; 9] = [
    "merged",
    "closed",
    "ejected",
    "verdict",
    "required_check",
    "dirty_after_base_move",
    "claim_taken",
    "stacked_base_merged",
    "overdue",
];

/// Every event name the relay knows.
pub(crate) const KNOWN_EVENTS: [&str; 10] = [
    "merged",
    "closed",
    "ejected",
    "enqueued",
    "verdict",
    "required_check",
    "dirty_after_base_move",
    "claim_taken",
    "stacked_base_merged",
    "overdue",
];

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            configured: false,
            mode: Mode::Shadow,
            host_name: None,
            interval_secs: 120,
            rate_limit_minutes: 10,
            repos: Vec::new(),
            events: DEFAULT_EVENTS.iter().map(|e| (*e).to_owned()).collect(),
            sockets: vec!["/tmp/cmux-nightly.sock".to_owned(), String::new()],
            comment_fallback: true,
            steward_tick_dir: None,
            steward_tick_max_age_secs: 600,
            admit_ready: AdmitReady::Off,
            review_gates: super::DEFAULT_REVIEW_GATES
                .iter()
                .map(|g| (*g).to_owned())
                .collect(),
            hosts: BTreeMap::new(),
            repo: BTreeMap::new(),
        }
    }
}

fn strings(value: &TomlValue, key: &str) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("relay.{key} must be an array of strings"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("relay.{key} must be an array of strings"))
        })
        .collect()
}

fn string(table: &Table, key: &str, scope: &str) -> Result<Option<String>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(TomlValue::String(s)) => Ok(Some(s.trim().to_owned()).filter(|s| !s.is_empty())),
        Some(_) => Err(format!("{scope}.{key} must be a string")),
    }
}

fn unsigned(table: &Table, key: &str) -> Result<Option<u64>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(TomlValue::Integer(n)) if *n > 0 => Ok(u64::try_from(*n).ok()),
        Some(_) => Err(format!("relay.{key} must be a positive integer")),
    }
}

/// Parse `[relay]` from a machine-global config table. Absent is the
/// defaults; a malformed key is an error, never silently ignored.
#[allow(clippy::too_many_lines)]
pub(crate) fn parse(data: &Table) -> Result<RelayConfig, String> {
    let mut config = RelayConfig::default();
    let Some(value) = data.get("relay") else {
        return Ok(config);
    };
    let table = value
        .as_table()
        .ok_or_else(|| "relay must be a table".to_owned())?;
    config.configured = true;
    if let Some(enabled) = table.get("enabled") {
        config.enabled = enabled
            .as_bool()
            .ok_or_else(|| "relay.enabled must be a boolean".to_owned())?;
    }
    if let Some(mode) = string(table, "mode", "relay")? {
        config.mode = match mode.as_str() {
            "shadow" => Mode::Shadow,
            "send" => Mode::Send,
            other => {
                return Err(format!(
                    "relay.mode must be \"shadow\" or \"send\", not {other:?}"
                ));
            }
        };
    }
    config.host_name = string(table, "host_name", "relay")?;
    if let Some(n) = unsigned(table, "interval_secs")? {
        config.interval_secs = n;
    }
    if let Some(n) = unsigned(table, "rate_limit_minutes")? {
        config.rate_limit_minutes = i64::try_from(n).unwrap_or(i64::MAX);
    }
    if let Some(n) = unsigned(table, "steward_tick_max_age_secs")? {
        config.steward_tick_max_age_secs = n;
    }
    if let Some(value) = table.get("repos") {
        config.repos = strings(value, "repos")?;
    }
    if let Some(value) = table.get("events") {
        let events = strings(value, "events")?;
        if let Some(unknown) = events.iter().find(|e| !KNOWN_EVENTS.contains(&e.as_str())) {
            return Err(format!("relay.events: unknown event {unknown:?}"));
        }
        config.events = events.into_iter().collect();
    }
    if let Some(value) = table.get("sockets") {
        config.sockets = strings(value, "sockets")?;
        if config.sockets.is_empty() {
            return Err("relay.sockets must name at least one socket (\"\" is the default)".into());
        }
    }
    if let Some(value) = table.get("review_gates") {
        config.review_gates = strings(value, "review_gates")?.into_iter().collect();
    }
    if let Some(value) = table.get("comment_fallback") {
        config.comment_fallback = value
            .as_bool()
            .ok_or_else(|| "relay.comment_fallback must be a boolean".to_owned())?;
    }
    config.steward_tick_dir = string(table, "steward_tick_dir", "relay")?.map(PathBuf::from);
    if let Some(value) = string(table, "admit_ready", "relay")? {
        config.admit_ready = match value.as_str() {
            "off" => AdmitReady::Off,
            "report" => AdmitReady::Report,
            "apply" => AdmitReady::Apply,
            other => {
                return Err(format!(
                    "relay.admit_ready must be \"off\", \"report\" or \"apply\", not {other:?}"
                ));
            }
        };
    }
    if let Some(value) = table.get("hosts") {
        let hosts = value
            .as_table()
            .ok_or_else(|| "relay.hosts must be a table".to_owned())?;
        for (name, value) in hosts {
            let host = value
                .as_table()
                .ok_or_else(|| format!("relay.hosts.{name} must be a table"))?;
            let scope = format!("relay.hosts.{name}");
            let ssh = string(host, "ssh", &scope)?;
            let cmux = string(host, "cmux", &scope)?.unwrap_or_else(|| "cmux".to_owned());
            for value in ssh.iter().chain(std::iter::once(&cmux)) {
                if !crate::owner_registry::safe_value(value) {
                    return Err(format!("{scope}: {value:?} has unsafe characters"));
                }
            }
            config.hosts.insert(
                name.clone(),
                HostRoute {
                    name: name.clone(),
                    ssh,
                    cmux,
                },
            );
        }
    }
    if let Some(value) = table.get("repo") {
        let repos = value
            .as_table()
            .ok_or_else(|| "relay.repo must be a table".to_owned())?;
        for (repo, value) in repos {
            let entry = value
                .as_table()
                .ok_or_else(|| format!("relay.repo.{repo:?} must be a table"))?;
            let scope = format!("relay.repo.{repo:?}");
            let address = |key: &str| -> Result<Option<Address>, String> {
                string(entry, key, &scope)?
                    .map(|text| Address::parse(&text).map_err(|e| format!("{scope}.{key}: {e}")))
                    .transpose()
            };
            config.repo.insert(
                repo.clone(),
                RepoFallbacks {
                    lead: address("lead")?,
                    repair_lane: address("repair_lane")?,
                },
            );
        }
    }
    Ok(config)
}

impl RelayConfig {
    /// Whether the relay may reach `host` at all: this host, or one listed in
    /// `relay.hosts`. A host named only by a pull-request body marker (which
    /// anyone who can edit the body can write) is never an ssh destination.
    pub(crate) fn knows_host(&self, host: &str, local_host: &str) -> bool {
        host == local_host || self.hosts.contains_key(host)
    }

    /// How to reach `host` from `local_host`: locally when it is this host,
    /// else over ssh to its configured destination (or its own name).
    pub(crate) fn route(&self, host: &str, local_host: &str) -> HostRoute {
        let configured = self.hosts.get(host);
        let cmux = configured.map_or_else(|| "cmux".to_owned(), |h| h.cmux.clone());
        if host == local_host {
            return HostRoute {
                name: host.to_owned(),
                ssh: None,
                cmux,
            };
        }
        HostRoute {
            name: host.to_owned(),
            ssh: Some(
                configured
                    .and_then(|h| h.ssh.clone())
                    .unwrap_or_else(|| host.to_owned()),
            ),
            cmux,
        }
    }

    /// The fallbacks configured for `repo`.
    pub(crate) fn fallbacks(&self, repo: &str) -> RepoFallbacks {
        self.repo.get(repo).cloned().unwrap_or_default()
    }
}
