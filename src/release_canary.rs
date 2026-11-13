//! Daily, read-only release-readiness canary: the pure half.
//!
//! A repo opts in with `[release.canary]` in `.shipyard/config.toml`, naming a
//! command that rehearses the release of the *next* version. Rules that only
//! apply to the real next version (a parity waiver, a pretag expression, a test
//! that hard-codes a version) are found the day they break, not on release day.
//!
//! This module holds config parsing, next-version arithmetic, issue wording and
//! the open/update/close decision. The side effects live in
//! `app::release_canary_cmd`.

use crate::config::LoadedConfig;
use crate::marked_issue::MarkedIssue;

/// Marker prefix embedded in the body of the one issue the canary owns.
pub const MARKER_PREFIX: &str = "<!-- shipyard-release-canary: ";

/// Longest `timeout_secs` accepted. The steward tick's outer limit is an hour:
/// a clone (10 minutes) plus this must fit inside it.
pub const MAX_TIMEOUT_SECS: u64 = 1800;

/// An in-progress stamp older than this is from a killed run.
const RUNNING_STALE_SECS: u64 = 4000;

/// Largest number of output bytes quoted in an issue body.
const TAIL_BYTES: usize = 6000;

/// How the next version is derived from the latest tag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bump {
    /// `1.2.3` becomes `1.2.4`.
    Patch,
    /// `1.2.3` becomes `1.3.0`.
    Minor,
}

/// Parameters loaded from `[release.canary]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanaryConfig {
    /// Shell command; `{next_version}` and `{next_tag}` are substituted.
    pub command: String,
    /// Wall-clock limit for the command.
    pub timeout_secs: u64,
    /// Which part of the latest tag advances.
    pub bump: Bump,
    /// Tag prefix, `v` by default.
    pub tag_prefix: String,
}

/// Read `[release.canary]`. An absent section, or `enabled = false`, is `None`.
///
/// # Errors
///
/// Returns a message when the section is present and enabled but invalid.
pub fn load_config(config: &LoadedConfig) -> Result<Option<CanaryConfig>, String> {
    let Some(section) = config.get("release.canary").and_then(toml::Value::as_table) else {
        return Ok(None);
    };
    if section
        .get("enabled")
        .and_then(toml::Value::as_bool)
        .is_some_and(|enabled| !enabled)
    {
        return Ok(None);
    }
    let command = section
        .get("command")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("release.canary.command is required")?
        .to_owned();
    let timeout_secs = match section.get("timeout_secs") {
        None => 900,
        Some(value) => value
            .as_integer()
            .and_then(|n| u64::try_from(n).ok())
            .filter(|n| (1..=MAX_TIMEOUT_SECS).contains(n))
            .ok_or_else(|| {
                format!(
                    "release.canary.timeout_secs must be an integer from 1 to {MAX_TIMEOUT_SECS}"
                )
            })?,
    };
    let bump = match section.get("bump").and_then(toml::Value::as_str) {
        None | Some("patch") => Bump::Patch,
        Some("minor") => Bump::Minor,
        Some(other) => {
            return Err(format!(
                "invalid release.canary.bump {other:?}; expected `patch` or `minor`"
            ));
        }
    };
    let tag_prefix = section
        .get("tag_prefix")
        .and_then(toml::Value::as_str)
        .unwrap_or("v")
        .to_owned();
    Ok(Some(CanaryConfig {
        command,
        timeout_secs,
        bump,
        tag_prefix,
    }))
}

fn parse_semver(tag: &str, prefix: &str) -> Option<(u64, u64, u64)> {
    let rest = tag.strip_prefix(prefix)?;
    let mut parts = rest.split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// The bare next version (`0.2.1`) from the highest stable tag in `tags`.
#[must_use]
pub fn next_version(tags: &[String], prefix: &str, bump: Bump) -> Option<String> {
    let (major, minor, patch) = tags
        .iter()
        .filter_map(|tag| parse_semver(tag.trim(), prefix))
        .max()?;
    Some(match bump {
        Bump::Patch => format!("{major}.{minor}.{}", patch + 1),
        Bump::Minor => format!("{major}.{}.0", minor + 1),
    })
}

const PREFIXES: [&str; 5] = ["GH_", "GITHUB_", "SHIPYARD_", "AWS_", "GIT_ASKPASS"];
const FRAGMENTS: [&str; 9] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PRIVATE_KEY",
    "APP_KEY",
    "API_KEY",
    "CREDENTIAL",
    "_PEM",
];

/// Whether an environment variable must not reach the canary command: tokens,
/// keys and agent sockets, GitHub and App variables, and Shipyard's own.
#[must_use]
pub fn is_secret_env(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    PREFIXES.iter().any(|prefix| key.starts_with(prefix))
        || key.starts_with("SSH_")
        || FRAGMENTS.iter().any(|fragment| key.contains(fragment))
}

/// Substitute `{next_version}` (bare) and `{next_tag}` (with prefix).
#[must_use]
pub fn render_command(template: &str, version: &str, prefix: &str) -> String {
    template
        .replace("{next_version}", version)
        .replace("{next_tag}", &format!("{prefix}{version}"))
}

/// The last `TAIL_BYTES` of `output`, cut at a line start.
#[must_use]
pub fn output_tail(output: &str) -> String {
    if output.len() <= TAIL_BYTES {
        return output.trim_end().to_owned();
    }
    let mut start = output.len() - TAIL_BYTES;
    while !output.is_char_boundary(start) {
        start += 1;
    }
    let cut = &output[start..];
    let cut = cut.split_once('\n').map_or(cut, |(_, rest)| rest);
    format!("... (earlier output omitted)\n{}", cut.trim_end())
}

/// What one canary run found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The command exited 0.
    Pass,
    /// The command failed or timed out; `detail` says how and quotes the tail.
    Fail {
        /// One line: exit code or timeout.
        reason: String,
        /// Output tail.
        tail: String,
    },
}

/// The mutation to carry out on the one tracking issue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IssueAction {
    /// Nothing to do.
    Nothing,
    /// Open the issue.
    Open {
        /// Title.
        title: String,
        /// Body.
        body: String,
    },
    /// Rewrite the open issue.
    Update {
        /// Issue number.
        number: u64,
        /// Title.
        title: String,
        /// Body.
        body: String,
    },
    /// Comment and close the open issue.
    Close {
        /// Issue number.
        number: u64,
        /// Closing comment.
        comment: String,
    },
}

/// Issue title for `tag`.
#[must_use]
pub fn issue_title(tag: &str) -> String {
    format!("release canary: {tag} would fail")
}

/// Issue body, carrying the marker for `repo`.
#[must_use]
pub fn issue_body(repo: &str, tag: &str, command: &str, reason: &str, tail: &str) -> String {
    format!(
        "{MARKER_PREFIX}{repo} -->\n\
         The daily release canary rehearsed `{tag}` on a clean checkout of main and it failed.\n\n\
         Command: `{command}`\n\
         Result: {reason}\n\n\
         Output tail:\n\n```\n{}\n```\n\n\
         This issue is opened and edited by `shipyard release-canary`; it closes itself when the \
         canary passes again. Nothing was tagged, pushed or edited.\n",
        tail.replace("```", "'''")
    )
}

/// Decide the issue mutation from the open marked issues and this run's result.
#[must_use]
pub fn decide(
    existing: &[MarkedIssue],
    repo: &str,
    tag: &str,
    command: &str,
    outcome: &Outcome,
) -> IssueAction {
    let current = existing.iter().min_by_key(|issue| issue.number);
    match (outcome, current) {
        (Outcome::Pass, None) => IssueAction::Nothing,
        (Outcome::Pass, Some(issue)) => IssueAction::Close {
            number: issue.number,
            comment: format!("The release canary for `{tag}` passes again; closing."),
        },
        (Outcome::Fail { reason, tail }, None) => IssueAction::Open {
            title: issue_title(tag),
            body: issue_body(repo, tag, command, reason, tail),
        },
        (Outcome::Fail { reason, tail }, Some(issue)) => IssueAction::Update {
            number: issue.number,
            title: issue_title(tag),
            body: issue_body(repo, tag, command, reason, tail),
        },
    }
}

/// Whether a daily run is due, from the stamp file's content and age.
///
/// The stamp reads `YYYY-MM-DD status`. A pass, fail or skip stamps the day; an
/// infrastructure `error` is retried after an hour; a `running` stamp (written
/// before the run) blocks reruns until it is too old to be a live run.
#[must_use]
pub fn daily_due(stamp: Option<(&str, u64)>, today: &str) -> bool {
    let Some((content, age_secs)) = stamp else {
        return true;
    };
    let mut parts = content.split_whitespace();
    let (Some(day), Some(status)) = (parts.next(), parts.next()) else {
        return true;
    };
    if status == "error" {
        return age_secs >= 3600;
    }
    if status == "running" {
        return age_secs >= RUNNING_STALE_SECS;
    }
    day != today
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from_toml(contents: &str) -> LoadedConfig {
        LoadedConfig {
            data: contents.parse::<toml::Table>().expect("config TOML"),
            global_dir: std::path::PathBuf::from("/nonexistent/global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: crate::config::LocalOverlaySource::None,
        }
    }

    #[test]
    fn config_absent_disabled_valid_and_invalid() {
        assert_eq!(load_config(&config_from_toml("")), Ok(None));
        assert_eq!(
            load_config(&config_from_toml(
                "[release.canary]\nenabled = false\ncommand = \"x\"\n"
            )),
            Ok(None)
        );
        let cfg = load_config(&config_from_toml(
            "[release.canary]\ncommand = \"run {next_tag}\"\ntimeout_secs = 60\nbump = \"minor\"\n",
        ))
        .expect("valid")
        .expect("enabled");
        assert_eq!(cfg.timeout_secs, 60);
        assert_eq!(cfg.bump, Bump::Minor);
        assert_eq!(cfg.tag_prefix, "v");
        assert!(load_config(&config_from_toml("[release.canary]\n")).is_err());
        assert!(
            load_config(&config_from_toml(
                "[release.canary]\ncommand = \"x\"\nbump = \"major\"\n"
            ))
            .is_err()
        );
    }

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|tag| (*tag).to_owned()).collect()
    }

    #[test]
    fn next_version_bumps_the_highest_stable_tag() {
        let all = tags(&["v0.1.9", "v0.2.0", "v0.10.0", "v0.2.0-rc.1", "nightly"]);
        assert_eq!(
            next_version(&all, "v", Bump::Patch).as_deref(),
            Some("0.10.1")
        );
        assert_eq!(
            next_version(&all, "v", Bump::Minor).as_deref(),
            Some("0.11.0")
        );
        assert_eq!(next_version(&tags(&["nightly"]), "v", Bump::Patch), None);
    }

    #[test]
    fn secret_env_filter_strips_tokens_keys_and_sockets_but_keeps_path() {
        for key in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "gh_app_private_key",
            "SHIPYARD_APP_KEY_PATH",
            "SSH_AUTH_SOCK",
            "AWS_SECRET_ACCESS_KEY",
            "NPM_TOKEN",
            "GIT_ASKPASS",
        ] {
            assert!(is_secret_env(key), "{key}");
        }
        for key in ["PATH", "HOME", "LANG", "TMPDIR", "USER"] {
            assert!(!is_secret_env(key), "{key}");
        }
    }

    #[test]
    fn timeout_is_capped_at_load() {
        let too_long = "[release.canary]\ncommand = \"x\"\ntimeout_secs = 99999\n";
        assert!(load_config(&config_from_toml(too_long)).is_err());
        let ok = "[release.canary]\ncommand = \"x\"\ntimeout_secs = 1800\n";
        assert!(load_config(&config_from_toml(ok)).is_ok());
    }

    #[test]
    fn command_placeholders_are_substituted() {
        assert_eq!(
            render_command("x --tag {next_tag} --v {next_version}", "0.2.1", "v"),
            "x --tag v0.2.1 --v 0.2.1"
        );
    }

    #[test]
    fn tail_keeps_the_end_and_cuts_on_a_line() {
        let long = "line\n".repeat(5000);
        let tail = output_tail(&long);
        assert!(tail.starts_with("... (earlier output omitted)\nline"));
        assert!(tail.len() < TAIL_BYTES + 60);
        assert_eq!(output_tail("short\n"), "short");
    }

    fn issue(number: u64) -> MarkedIssue {
        MarkedIssue {
            number,
            key: "o/r".to_owned(),
            body: String::new(),
        }
    }

    fn fail() -> Outcome {
        Outcome::Fail {
            reason: "exit 1".to_owned(),
            tail: "boom".to_owned(),
        }
    }

    #[test]
    fn decisions_open_update_close_and_stay_quiet() {
        assert_eq!(
            decide(&[], "o/r", "v1.0.1", "c", &Outcome::Pass),
            IssueAction::Nothing
        );
        match decide(&[], "o/r", "v1.0.1", "c", &fail()) {
            IssueAction::Open { title, body } => {
                assert_eq!(title, "release canary: v1.0.1 would fail");
                assert!(body.starts_with("<!-- shipyard-release-canary: o/r -->"));
                assert!(body.contains("boom"));
            }
            other => panic!("expected open, got {other:?}"),
        }
        assert!(matches!(
            decide(&[issue(9), issue(4)], "o/r", "v1.0.1", "c", &fail()),
            IssueAction::Update { number: 4, .. }
        ));
        assert!(matches!(
            decide(&[issue(4)], "o/r", "v1.0.1", "c", &Outcome::Pass),
            IssueAction::Close { number: 4, .. }
        ));
    }

    #[test]
    fn daily_stamp_gates_once_per_day_and_retries_errors_hourly() {
        assert!(daily_due(None, "2026-11-13"));
        assert!(!daily_due(Some(("2026-11-13 pass", 10)), "2026-11-13"));
        assert!(!daily_due(Some(("2026-11-13 fail", 10)), "2026-11-13"));
        assert!(daily_due(Some(("2026-11-12 pass", 10)), "2026-11-13"));
        assert!(!daily_due(Some(("2026-11-13 error", 60)), "2026-11-13"));
        assert!(daily_due(Some(("2026-11-13 error", 3600)), "2026-11-13"));
        assert!(daily_due(Some(("garbage", 1)), "2026-11-13"));
        assert!(!daily_due(
            Some(("2026-11-13 running 99", 60)),
            "2026-11-13"
        ));
        assert!(!daily_due(
            Some(("2026-11-12 running 99", 60)),
            "2026-11-13"
        ));
        assert!(daily_due(
            Some(("2026-11-13 running 99", 4000)),
            "2026-11-13"
        ));
        assert!(!daily_due(Some(("2026-11-13 skipped", 5)), "2026-11-13"));
    }
}
