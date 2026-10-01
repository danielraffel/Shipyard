//! Who owns a pull request, and how to reach the host it lives on.
//!
//! Lookup order: the merge steward's exact-head handoff record (written by
//! `shipyard pr` when `[merge_steward] auto_handoff` is on, local to the host
//! that ran it), then the `whence` provenance marker every agent-opened pull
//! request body carries (`<!-- whence {"labels":[...],"prov":{...}} -->`),
//! then nobody. Both are advisory evidence of who opened the pull request;
//! neither is proof the session still exists, which [`super::host`] checks.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where an owner record came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerSource {
    /// `shipyard runner steward-handoff` record for the exact head.
    Steward,
    /// `whence` marker in the pull-request body.
    Whence,
}

/// The session believed to own a pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    /// Record it came from.
    pub source: OwnerSource,
    /// `claude`, `codex`, or whatever the record named.
    pub agent: String,
    /// Host name as the record stamped it (`m3`, `Daniels-M5-Studio`), when
    /// known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// `true` when the steward record proves the owner is this machine.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
    /// Native agent session id.
    pub session: String,
    /// cmux surface UUID, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    /// Resume hint for a person (never executed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// Worktree path the session worked in, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

const MARKER_OPEN: &str = "<!-- whence ";
const MARKER_CLOSE: &str = "-->";

/// Whether `value` is a plausible agent session id: 1-128 characters of
/// ASCII letters, digits, `-`, `_`, `.`, `:`. Anything else could smuggle
/// shell or path syntax, so it is refused.
#[must_use]
pub fn safe_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with(['-', '.'])
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// Whether `value` is a cmux surface/workspace UUID (8-4-4-4-12 hex).
#[must_use]
pub fn safe_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn bounded(text: &str, max: usize) -> String {
    let mut out: String = text.chars().filter(|c| !c.is_control()).take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

/// Parse the `whence` marker out of a pull-request body.
///
/// `Ok(None)` when the body carries no marker. `Err` when a marker is present
/// but unusable (not JSON, no `prov.session`, or a session/surface that does
/// not look like one), so a caller reports it instead of treating the pull
/// request as unowned-by-design.
///
/// # Errors
/// On a malformed marker, described.
pub fn parse_whence(body: &str) -> Result<Option<Owner>, String> {
    let Some(start) = body.find(MARKER_OPEN) else {
        return Ok(None);
    };
    let rest = &body[start + MARKER_OPEN.len()..];
    let Some(end) = rest.find(MARKER_CLOSE) else {
        return Err("whence marker is not closed".to_owned());
    };
    let text = rest[..end].trim();
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("whence marker is not JSON: {error}"))?;
    let prov = value
        .get("prov")
        .and_then(Value::as_object)
        .ok_or_else(|| "whence marker has no prov object".to_owned())?;
    let field = |name: &str| {
        prov.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    let session = field("session").ok_or_else(|| "whence marker has no prov.session".to_owned())?;
    if !safe_session_id(session) {
        return Err(format!(
            "whence prov.session {:?} is not a session id",
            bounded(session, 40)
        ));
    }
    let surface = match field("terminal_address") {
        Some(surface) if safe_uuid(surface) => Some(surface.to_ascii_uppercase()),
        Some(surface) => {
            return Err(format!(
                "whence prov.terminal_address {:?} is not a surface UUID",
                bounded(surface, 40)
            ));
        }
        None => None,
    };
    let agent = field("agent").map_or_else(|| "other".to_owned(), |agent| bounded(agent, 20));
    Ok(Some(Owner {
        source: OwnerSource::Whence,
        agent,
        host: field("host").map(|host| bounded(host, 64)),
        local: false,
        session: session.to_owned(),
        surface,
        resume: field("resume").map(|resume| bounded(resume, 200)),
        path: field("path").map(|path| bounded(path, 200)),
    }))
}

/// The merge steward's exact-head handoff record for `(repo, pr, head)`, read
/// from this machine's state directory. `None` when there is none or it names
/// no agent route. Parsed loosely (as JSON values) so this read never couples
/// to the steward's private types; any shape surprise is "no record".
#[must_use]
pub fn steward_owner(
    state_dir: &Path,
    repo: &str,
    pr: u64,
    head: &str,
    local_machine: Option<&str>,
) -> Option<Owner> {
    let steward = state_dir.join("merge-steward");
    let path = steward
        .join("handoffs")
        .join(crate::required_check_policy::encode_path_segment(
            &repo.to_ascii_lowercase(),
        ))
        .join(format!("pr-{pr}"))
        .join(format!("{}.json", head.to_ascii_lowercase()));
    let receipt: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if receipt.get("pr").and_then(Value::as_u64) != Some(pr)
        || !receipt
            .get("head_sha")
            .and_then(Value::as_str)
            .is_some_and(|sha| sha.eq_ignore_ascii_case(head))
    {
        return None;
    }
    let route_id = receipt.pointer("/agent_route/route_id")?.as_str()?;
    if !safe_session_id(route_id) {
        return None;
    }
    let origin = receipt
        .get("origin_machine")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let route: Value = serde_json::from_str(
        &std::fs::read_to_string(
            steward
                .join("agent-routes")
                .join(format!("{route_id}.json")),
        )
        .ok()?,
    )
    .ok()?;
    let agent = route.get("agent")?;
    let session = agent.get("session_id")?.as_str()?;
    if !safe_session_id(session) {
        return None;
    }
    let surface = agent
        .get("surface_id")
        .and_then(Value::as_str)
        .or_else(|| agent.pointer("/terminal_provenance/surface_id")?.as_str())
        .filter(|surface| safe_uuid(surface))
        .map(str::to_ascii_uppercase);
    let provider = agent
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("other");
    Some(Owner {
        source: OwnerSource::Steward,
        agent: bounded(provider, 20),
        host: None,
        local: local_machine.is_some_and(|id| !id.is_empty() && id == origin),
        session: session.to_owned(),
        surface,
        resume: None,
        path: None,
    })
}

/// This machine's steward identity (`machine-identity.json` `id`), if any.
#[must_use]
pub fn local_machine_identity(state_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(state_dir.join("machine-identity.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value.get("id")?.as_str().map(str::to_owned)
}

/// Combine the two sources. The steward record wins; when it cannot place
/// the owner on a host, a whence marker naming the same session lends its
/// host, surface, resume hint and path.
#[must_use]
pub fn resolve(steward: Option<Owner>, whence: Option<Owner>) -> Option<Owner> {
    match (steward, whence) {
        (Some(mut owner), Some(marker)) if marker.session == owner.session => {
            if !owner.local {
                owner.host = marker.host;
            }
            owner.surface = owner.surface.or(marker.surface);
            owner.resume = marker.resume;
            owner.path = marker.path;
            Some(owner)
        }
        (Some(owner), _) => Some(owner),
        (None, marker) => marker,
    }
}

/// How to run a command on the owner's host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "via", content = "alias", rename_all = "snake_case")]
pub enum Route {
    /// This machine.
    Local,
    /// `ssh <alias>`.
    Ssh(String),
}

/// Whether an ssh alias is safe to pass as an argv element: letters, digits,
/// `.`, `_`, `-`, not starting with `-`.
#[must_use]
pub fn safe_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 64
        && !alias.starts_with('-')
        && alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Route to an owner's host. `hosts` is `[pr_watch.handback.hosts]`: stamped
/// host name (case-insensitive) to an ssh alias, or `"local"`. A host absent
/// from the map is local only when it equals one of `local_names` (this
/// machine's short host name); otherwise it is unroutable (`Err`).
///
/// # Errors
/// When the owner has no host, the host has no mapping, or the mapping is not
/// a safe alias.
pub fn route_for(
    owner: &Owner,
    hosts: &BTreeMap<String, String>,
    local_names: &[String],
) -> Result<Route, String> {
    if owner.local {
        return Ok(Route::Local);
    }
    let Some(host) = owner.host.as_deref() else {
        return Err("owner host unknown".to_owned());
    };
    let mapped = hosts
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(host))
        .map(|(_, alias)| alias.as_str());
    match mapped {
        Some(alias) if alias.eq_ignore_ascii_case("local") => Ok(Route::Local),
        Some(alias) if safe_alias(alias) => Ok(Route::Ssh(alias.to_owned())),
        Some(alias) => Err(format!(
            "[pr_watch.handback.hosts] {host} = {alias:?} is not an ssh alias"
        )),
        None if local_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(host)) =>
        {
            Ok(Route::Local)
        }
        None => Err(format!(
            "no [pr_watch.handback.hosts] entry for host {host:?}"
        )),
    }
}
