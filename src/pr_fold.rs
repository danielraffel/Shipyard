//! Same-session sibling pull requests that could ship as one.
//!
//! Each pull request pays its own PR-head gate run and its own merge-group
//! run. When one agent session opens several pull requests within a few hours
//! over the same part of the tree, those runs are mostly redundant: over one
//! week of Generous-Corp/pulp, 85 of 177 session-stamped pull requests had an
//! open sibling from the same session, opened within six hours, touching a
//! shared directory family.
//!
//! The session comes from the `whence` provenance block that stamps a pull
//! request body (`<!-- whence {"prov": {"session": ...}} -->`) and, for the
//! pull request about to be opened, from the same environment variables
//! `whence` reads. Everything here is advisory: it names candidates, it never
//! folds anything by itself.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;

/// How far back a sibling may have been opened.
pub const SIBLING_WINDOW: Duration = Duration::hours(6);

/// Paths every change touches because a gate forces it (version files, skill
/// files, the planning pointer). Sharing them says nothing about relatedness.
pub const DEFAULT_NOISE_PATHS: &[&str] =
    &[".claude-plugin", ".agents/skills", "skills", "planning"];

/// Config key overriding [`DEFAULT_NOISE_PATHS`].
pub const NOISE_CONFIG_KEY: &str = "pr.fold.noise_paths";

/// Environment variables naming the current agent session, in the order
/// `whence` consults them.
pub const SESSION_ENV: &[&str] = &[
    "WHENCE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_SESSION_ID",
    "CODEX_ROLLOUT_ID",
];

/// The current session id, from the first non-empty [`SESSION_ENV`] variable.
#[must_use]
pub fn current_session(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    SESSION_ENV
        .iter()
        .filter_map(|name| lookup(name))
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
}

/// `prov.session` from a pull request body's `whence` block.
#[must_use]
pub fn whence_session(body: &str) -> Option<String> {
    let start = body.find("<!-- whence ")? + "<!-- whence ".len();
    let end = body[start..].find(" -->")? + start;
    let value: Value = serde_json::from_str(&body[start..end]).ok()?;
    value
        .pointer("/prov/session")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|session| !session.is_empty())
        .map(str::to_owned)
}

/// Directory families a change touches: the first two path components of each
/// file, skipping root files and `noise` prefixes.
#[must_use]
pub fn families<'a>(
    files: impl IntoIterator<Item = &'a str>,
    noise: &[String],
) -> BTreeSet<String> {
    files
        .into_iter()
        .filter(|file| file.contains('/'))
        .filter(|file| {
            !noise
                .iter()
                .any(|prefix| *file == prefix.as_str() || file.starts_with(&format!("{prefix}/")))
        })
        .map(|file| file.split('/').take(2).collect::<Vec<_>>().join("/"))
        .collect()
}

/// One open pull request, as read from `repos/{repo}/pulls?state=open`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenPr {
    /// Number.
    pub number: u64,
    /// Head branch.
    pub branch: String,
    /// When it was opened.
    pub created_at: DateTime<Utc>,
    /// Its `whence` session, when stamped.
    pub session: Option<String>,
}

/// Parse one page of `repos/{repo}/pulls` into [`OpenPr`]s.
#[must_use]
pub fn parse_open_prs(value: &Value) -> Vec<OpenPr> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pr| {
            Some(OpenPr {
                number: pr.get("number")?.as_u64()?,
                branch: pr.pointer("/head/ref")?.as_str()?.to_owned(),
                created_at: DateTime::parse_from_rfc3339(pr.get("created_at")?.as_str()?)
                    .ok()?
                    .with_timezone(&Utc),
                session: pr
                    .get("body")
                    .and_then(Value::as_str)
                    .and_then(whence_session),
            })
        })
        .collect()
}

/// Open pull requests from `session`, opened within [`SIBLING_WINDOW`] of
/// `now`, excluding the current branch. Their files are not yet known.
#[must_use]
pub fn session_candidates<'a>(
    open: &'a [OpenPr],
    session: &str,
    current_branch: &str,
    now: DateTime<Utc>,
) -> Vec<&'a OpenPr> {
    open.iter()
        .filter(|pr| pr.session.as_deref() == Some(session))
        .filter(|pr| pr.branch != current_branch)
        .filter(|pr| now - pr.created_at <= SIBLING_WINDOW)
        .collect()
}

/// A candidate that shares at least one directory family with this change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FoldSuggestion {
    /// Pull request number.
    pub number: u64,
    /// Head branch, the argument `--fold` takes.
    pub branch: String,
    /// Families both changes touch.
    pub shared: Vec<String>,
}

/// Keep the candidates whose families overlap `here`.
#[must_use]
pub fn suggestions(
    here: &BTreeSet<String>,
    candidates: &[(&OpenPr, BTreeSet<String>)],
) -> Vec<FoldSuggestion> {
    candidates
        .iter()
        .filter_map(|(pr, theirs)| {
            let shared: Vec<String> = here.intersection(theirs).cloned().collect();
            (!shared.is_empty()).then(|| FoldSuggestion {
                number: pr.number,
                branch: pr.branch.clone(),
                shared,
            })
        })
        .collect()
}

/// The advisory lines for a person.
#[must_use]
pub fn render(suggestions: &[FoldSuggestion]) -> Vec<String> {
    if suggestions.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![
        "▸ Fold suggestion (advisory): open PRs from this session in the last 6 h touch the same families:".to_owned(),
    ];
    for suggestion in suggestions {
        lines.push(format!(
            "    #{} {} — shared: {}",
            suggestion.number,
            suggestion.branch,
            suggestion.shared.join(", ")
        ));
    }
    let branches: Vec<&str> = suggestions.iter().map(|s| s.branch.as_str()).collect();
    lines.push(format!(
        "  One PR per family saves a PR-head and a merge-group gate run each. To fold: shipyard pr --fold {}",
        branches.join(" --fold ")
    ));
    lines.push(
        "  Keep separate: an urgent fix, an unfinished branch, or one that may need reverting alone."
            .to_owned(),
    );
    lines
}

#[cfg(test)]
mod tests;
