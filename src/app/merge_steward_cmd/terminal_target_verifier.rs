//! Fail-closed terminal adapter binding for a future transactional route-change gate.
//!
//! This module deliberately does not publish or mutate a route. It turns fresh,
//! independently observed runtime evidence into a typed candidate that a later
//! source+target-generation CAS may commit.

#![allow(
    dead_code,
    reason = "activation is intentionally deferred until the transactional route-change gate"
)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MAX_HERDR_HANDOFF_PANES: usize = 256;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct LocalProcessIdentityV1 {
    pub(super) boot_id: String,
    pub(super) pid: u32,
    pub(super) start_identity: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum TerminalAddressV1 {
    Cmux {
        surface_id: String,
        workspace_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        captured_lifecycle_correlation: Option<String>,
    },
    HerdR {
        selector: HerdRSelectorV1,
        terminal_id: String,
        pane_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(super) enum HerdRSelectorV1 {
    Session(String),
    Socket(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct TerminalInstanceV1 {
    process: LocalProcessIdentityV1,
    address: TerminalAddressV1,
}

/// Opaque evidence returned only after both adapter and final OS verification.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct VerifiedTerminalInstance(TerminalInstanceV1);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum TerminalBindingStateV1 {
    LeaderLiveUnbound {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prior_instance: Option<TerminalInstanceV1>,
    },
    AdapterBound {
        instance: TerminalInstanceV1,
    },
    Demoted {
        #[serde(skip_serializing_if = "Option::is_none")]
        prior_instance: Option<TerminalInstanceV1>,
    },
}

impl TerminalBindingStateV1 {
    fn bind_verified(verified: VerifiedTerminalInstance) -> Self {
        Self::AdapterBound {
            instance: verified.0,
        }
    }

    /// Demotion is monotonic: the last authority is retained as a tombstone.
    pub(super) fn demote(self) -> Self {
        match self {
            Self::AdapterBound { instance }
            | Self::Demoted {
                prior_instance: Some(instance),
            } => Self::Demoted {
                prior_instance: Some(instance),
            },
            Self::Demoted {
                prior_instance: None,
            } => Self::Demoted {
                prior_instance: None,
            },
            Self::LeaderLiveUnbound { prior_instance } => match prior_instance {
                Some(instance) => Self::Demoted {
                    prior_instance: Some(instance),
                },
                None => Self::Demoted {
                    prior_instance: None,
                },
            },
        }
    }

    pub(super) const fn publishes_terminal_adapter(&self) -> bool {
        matches!(self, Self::AdapterBound { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CmuxExpectation<'a> {
    pub(super) process: &'a LocalProcessIdentityV1,
    pub(super) socket_path: &'a str,
    pub(super) surface_id: &'a str,
    pub(super) captured_lifecycle_correlation: Option<&'a str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HerdRExpectation<'a> {
    pub(super) process: &'a LocalProcessIdentityV1,
    pub(super) selector: &'a HerdRSelectorV1,
    pub(super) terminal_id: &'a str,
    pub(super) native_session_id: &'a str,
    pub(super) allow_live_handoff_scan: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum VerificationFailure {
    Unsupported,
    RemoteOrUnobservable,
    ProcessIdentityChanged,
    MethodMissing,
    InvalidResponse,
    NoMatch,
    MultipleMatches,
    NativeSessionMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CommandOutput {
    pub(super) success: bool,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

pub(super) trait TerminalProbe {
    /// Re-observe this PID from the local OS. Remote observations are forbidden.
    fn observe_local_process(
        &mut self,
        pid: u32,
    ) -> Result<LocalProcessIdentityV1, VerificationFailure>;

    /// Prove the selected `HerdR` server observes this local host, not a forward.
    fn herdr_selector_is_local(
        &mut self,
        selector: &HerdRSelectorV1,
    ) -> Result<bool, VerificationFailure>;

    /// Run a bounded adapter query. Implementations must not use ambient route selectors.
    fn run(
        &mut self,
        program: &str,
        args: &[String],
        // `None` means the inherited variable must be removed.
        environment_overrides: &BTreeMap<String, Option<String>>,
    ) -> Result<CommandOutput, VerificationFailure>;
}

pub(super) fn verify_cmux<P: TerminalProbe>(
    probe: &mut P,
    expected: &CmuxExpectation<'_>,
) -> Result<VerifiedTerminalInstance, VerificationFailure> {
    require_same_process(probe, expected.process)?;
    if !expected.socket_path.starts_with('/')
        || !is_uuid(expected.surface_id)
        || expected
            .captured_lifecycle_correlation
            .is_some_and(|value| value.trim().is_empty())
    {
        return Err(VerificationFailure::InvalidResponse);
    }
    let params = serde_json::json!({
        "pid": expected.process.pid,
        "pid_resolution": "controlling_tty",
    })
    .to_string();
    let output = probe.run(
        "cmux",
        &[
            "--socket".into(),
            expected.socket_path.into(),
            "rpc".into(),
            "agent.resolve_delivery_target".into(),
            params,
        ],
        &BTreeMap::from([
            ("CMUX_SOCKET_PATH".into(), None),
            ("CMUX_SOCKET".into(), None),
        ]),
    )?;
    if !output.success {
        return Err(classify_command_failure(&output));
    }
    let value: Value =
        serde_json::from_str(&output.stdout).map_err(|_| VerificationFailure::InvalidResponse)?;
    let payload = value.get("result").unwrap_or(&value);
    let surface_id = required_string(payload, "surface_id")?;
    let workspace_id = required_string(payload, "workspace_id")?;
    if payload.get("source").and_then(Value::as_str) != Some("pid")
        || payload.get("pid_resolution").and_then(Value::as_str) != Some("controlling_tty")
        || payload.get("pid").and_then(Value::as_u64) != Some(u64::from(expected.process.pid))
        || surface_id != expected.surface_id
        || !is_uuid(surface_id)
        || !is_uuid(workspace_id)
    {
        return Err(VerificationFailure::NoMatch);
    }
    require_same_process(probe, expected.process)?;
    Ok(VerifiedTerminalInstance(TerminalInstanceV1 {
        process: expected.process.clone(),
        address: TerminalAddressV1::Cmux {
            surface_id: surface_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            captured_lifecycle_correlation: expected
                .captured_lifecycle_correlation
                .map(str::to_owned),
        },
    }))
}

pub(super) fn verify_herdr<P: TerminalProbe>(
    probe: &mut P,
    expected: &HerdRExpectation<'_>,
) -> Result<VerifiedTerminalInstance, VerificationFailure> {
    require_same_process(probe, expected.process)?;
    validate_selector(expected.selector)?;
    if !probe.herdr_selector_is_local(expected.selector)? {
        return Err(VerificationFailure::RemoteOrUnobservable);
    }
    let snapshot = herdr_json(probe, expected.selector, &["api", "snapshot"])?;
    let panes = result_field(&snapshot, "snapshot")
        .unwrap_or_else(|| snapshot.get("result").unwrap_or(&snapshot))
        .get("panes")
        .and_then(Value::as_array)
        .ok_or(VerificationFailure::InvalidResponse)?;
    let exact = panes
        .iter()
        .filter(|pane| {
            pane.get("terminal_id").and_then(Value::as_str) == Some(expected.terminal_id)
        })
        .collect::<Vec<_>>();
    if exact.len() > 1 {
        return Err(VerificationFailure::MultipleMatches);
    }

    let (pane_id, bound_terminal_id) = if let Some(pane) = exact.first() {
        let pane_id = required_string(pane, "pane_id")?;
        require_herdr_process(probe, expected.selector, pane_id, expected.process.pid)?;
        (pane_id.to_owned(), expected.terminal_id.to_owned())
    } else {
        if !expected.allow_live_handoff_scan {
            return Err(VerificationFailure::NoMatch);
        }
        if panes.len() > MAX_HERDR_HANDOFF_PANES {
            return Err(VerificationFailure::RemoteOrUnobservable);
        }
        let mut matches = Vec::new();
        let mut seen = BTreeSet::new();
        for pane in panes {
            let pane_id = required_string(pane, "pane_id")?;
            if !seen.insert(pane_id) {
                return Err(VerificationFailure::MultipleMatches);
            }
            if herdr_process_matches(probe, expected.selector, pane_id, expected.process.pid)? {
                matches.push((
                    pane_id.to_owned(),
                    required_string(pane, "terminal_id")?.to_owned(),
                ));
            }
        }
        match matches.as_slice() {
            [(pane_id, terminal_id)] => (pane_id.clone(), terminal_id.clone()),
            [] => return Err(VerificationFailure::NoMatch),
            _ => return Err(VerificationFailure::MultipleMatches),
        }
    };

    let agent = herdr_json(probe, expected.selector, &["agent", "get", &pane_id])?;
    let agent =
        result_field(&agent, "agent").unwrap_or_else(|| agent.get("result").unwrap_or(&agent));
    if agent.get("terminal_id").and_then(Value::as_str) != Some(bound_terminal_id.as_str())
        || agent.get("pane_id").and_then(Value::as_str) != Some(pane_id.as_str())
    {
        return Err(VerificationFailure::NoMatch);
    }
    let observed = agent
        .get("agent_session")
        .and_then(|session| session.get("value"))
        .and_then(Value::as_str);
    if expected.native_session_id.is_empty() || observed != Some(expected.native_session_id) {
        return Err(VerificationFailure::NativeSessionMismatch);
    }
    require_herdr_process(probe, expected.selector, &pane_id, expected.process.pid)?;
    require_same_process(probe, expected.process)?;
    Ok(VerifiedTerminalInstance(TerminalInstanceV1 {
        process: expected.process.clone(),
        address: TerminalAddressV1::HerdR {
            selector: expected.selector.clone(),
            terminal_id: bound_terminal_id,
            pane_id,
        },
    }))
}

fn require_same_process<P: TerminalProbe>(
    probe: &mut P,
    expected: &LocalProcessIdentityV1,
) -> Result<(), VerificationFailure> {
    if expected.pid == 0 || expected.boot_id.is_empty() || expected.start_identity.is_empty() {
        return Err(VerificationFailure::RemoteOrUnobservable);
    }
    let observed = probe.observe_local_process(expected.pid)?;
    if observed == *expected {
        Ok(())
    } else {
        Err(VerificationFailure::ProcessIdentityChanged)
    }
}

fn validate_selector(selector: &HerdRSelectorV1) -> Result<(), VerificationFailure> {
    if matches!(selector, HerdRSelectorV1::Socket(_)) {
        // HerdR 0.8.2 selects sockets only through an environment variable and
        // does not echo server identity in responses. That cannot prove which
        // instance answered, so socket activation remains unsupported.
        return Err(VerificationFailure::Unsupported);
    }
    let value = match selector {
        HerdRSelectorV1::Session(value) | HerdRSelectorV1::Socket(value) => value,
    };
    if value.trim().is_empty()
        || matches!(selector, HerdRSelectorV1::Socket(_)) && !value.starts_with('/')
    {
        Err(VerificationFailure::RemoteOrUnobservable)
    } else {
        Ok(())
    }
}

fn herdr_json<P: TerminalProbe>(
    probe: &mut P,
    selector: &HerdRSelectorV1,
    command: &[&str],
) -> Result<Value, VerificationFailure> {
    let env = match selector {
        HerdRSelectorV1::Session(_) => BTreeMap::from([
            ("HERDR_SESSION".into(), None),
            ("HERDR_SOCKET_PATH".into(), None),
        ]),
        HerdRSelectorV1::Socket(socket) => BTreeMap::from([
            ("HERDR_SESSION".into(), None),
            ("HERDR_SOCKET_PATH".into(), Some(socket.clone())),
        ]),
    };
    let mut args = match selector {
        HerdRSelectorV1::Session(session) => vec!["--session".into(), session.clone()],
        HerdRSelectorV1::Socket(_) => Vec::new(),
    };
    args.extend(command.iter().map(|arg| (*arg).to_owned()));
    let output = probe.run("herdr", &args, &env)?;
    if !output.success {
        return Err(classify_command_failure(&output));
    }
    serde_json::from_str(&output.stdout).map_err(|_| VerificationFailure::InvalidResponse)
}

fn require_herdr_process<P: TerminalProbe>(
    probe: &mut P,
    selector: &HerdRSelectorV1,
    pane_id: &str,
    pid: u32,
) -> Result<(), VerificationFailure> {
    if herdr_process_matches(probe, selector, pane_id, pid)? {
        Ok(())
    } else {
        Err(VerificationFailure::NoMatch)
    }
}

fn herdr_process_matches<P: TerminalProbe>(
    probe: &mut P,
    selector: &HerdRSelectorV1,
    pane_id: &str,
    pid: u32,
) -> Result<bool, VerificationFailure> {
    let value = herdr_json(
        probe,
        selector,
        &["pane", "process-info", "--pane", pane_id],
    )?;
    let info = result_field(&value, "process_info").ok_or(VerificationFailure::InvalidResponse)?;
    if info.get("pane_id").and_then(Value::as_str) != Some(pane_id) {
        return Err(VerificationFailure::InvalidResponse);
    }
    let shell_matches = info.get("shell_pid").and_then(Value::as_u64) == Some(u64::from(pid));
    let foreground_matches = info
        .get("foreground_processes")
        .and_then(Value::as_array)
        .map_or(0, |processes| {
            processes
                .iter()
                .filter(|process| {
                    process.get("pid").and_then(Value::as_u64) == Some(u64::from(pid))
                })
                .count()
        });
    if foreground_matches > 1 {
        return Err(VerificationFailure::MultipleMatches);
    }
    Ok(shell_matches || foreground_matches == 1)
}

fn result_field<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    value.get("result")?.get(field)
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, VerificationFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(VerificationFailure::InvalidResponse)
}

fn classify_command_failure(output: &CommandOutput) -> VerificationFailure {
    let combined = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    if combined.contains("method_not_found") || combined.contains("unrecognized_method") {
        VerificationFailure::MethodMissing
    } else if combined.contains("not supported") || combined.contains("unsupported") {
        VerificationFailure::Unsupported
    } else {
        VerificationFailure::NoMatch
    }
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(index, ch)| {
            matches!(index, 8 | 13 | 18 | 23) && ch == '-'
                || !matches!(index, 8 | 13 | 18 | 23) && ch.is_ascii_hexdigit()
        })
}

#[cfg(test)]
#[path = "terminal_target_verifier/tests.rs"]
mod tests;
