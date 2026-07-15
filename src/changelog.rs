use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::{Regex, RegexBuilder};

use crate::config::LoadedConfig;

const DEFAULT_SKIP_PATTERNS: [&str; 3] = [
    r"^chore: bump .*version",
    r"^chore\(release\): ",
    r"^bump .*to v?\d+\.\d+\.\d+$",
];

/// Parameters loaded from `[release.changelog]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangelogConfig {
    /// Whether generation is enabled.
    pub enabled: bool,
    /// GitHub repository URL used for release and PR links.
    pub repo_url: String,
    /// Changelog output path, relative to the repo when not absolute.
    pub path: String,
    /// Git tag glob passed to `git tag --list`.
    pub tag_filter: String,
    /// Human-facing product name.
    pub product: String,
    /// Regex patterns for merge subjects to hide.
    pub skip_commit_patterns: Vec<String>,
    /// Markdown title.
    pub title: String,
    /// Optional pre-entry markdown/html comment block.
    pub header_comment: Option<String>,
}

/// One rendered changelog entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangelogEntry {
    /// Bare version, e.g. `0.49.0`.
    pub version: String,
    /// Full git tag, e.g. `v0.49.0`.
    pub tag: String,
    /// Commit date in `YYYY-MM-DD` form.
    pub date: String,
    /// User-visible merged PRs as `(number, subject)`.
    pub prs: Vec<(u64, String)>,
}

/// Changelog generation failure.
#[derive(Debug)]
pub enum ChangelogError {
    /// Git command failed or could not start.
    Git {
        /// Git arguments excluding the `git` executable.
        args: Vec<String>,
        /// Failure detail from stderr or the process spawn error.
        message: String,
    },
    /// A configured skip pattern is not valid regex.
    InvalidPattern {
        /// Raw configured pattern.
        pattern: String,
        /// Regex parser message.
        message: String,
    },
}

impl Display for ChangelogError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git { args, message } => write!(f, "git {} failed: {message}", args.join(" ")),
            Self::InvalidPattern { pattern, message } => {
                write!(
                    f,
                    "invalid skip_commit_patterns entry {pattern:?}: {message}"
                )
            }
        }
    }
}

impl std::error::Error for ChangelogError {}

/// Extract changelog config from loaded Shipyard config.
#[must_use]
pub fn load_changelog_config(config: &LoadedConfig) -> ChangelogConfig {
    let Some(section) = config
        .get("release.changelog")
        .and_then(toml::Value::as_table)
    else {
        return disabled_config();
    };
    let skip_commit_patterns = section
        .get("skip_commit_patterns")
        .and_then(toml::Value::as_array)
        .filter(|items| !items.is_empty())
        .map_or_else(default_skip_patterns, |items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap_or_default().to_owned())
                .collect()
        });
    ChangelogConfig {
        enabled: section
            .get("enabled")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false),
        repo_url: table_str(section, "repo_url", ""),
        path: table_str(section, "path", "CHANGELOG.md"),
        tag_filter: table_str(section, "tag_filter", "v*"),
        product: table_str(section, "product", "this project"),
        skip_commit_patterns,
        title: table_str(section, "title", "Changelog"),
        header_comment: section
            .get("header_comment")
            .and_then(toml::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
    }
}

/// Return tags matching `tag_filter`, sorted newest semantic version first.
pub fn discover_tags(cfg: &ChangelogConfig, cwd: &Path) -> Result<Vec<String>, ChangelogError> {
    let output = run_git(
        cwd,
        &["tag", "--list", &cfg.tag_filter, "--sort=-v:refname"],
    )?;
    let prefix = cfg.tag_filter.trim_end_matches('*');
    let strict = Regex::new(&format!(r"^{}\d+\.\d+\.\d+$", regex::escape(prefix)))
        .expect("strict tag regex is valid");
    Ok(output
        .lines()
        .filter(|tag| strict.is_match(tag))
        .map(ToOwned::to_owned)
        .collect())
}

/// Build all user-visible entries from the configured tag graph.
pub fn build_entries(
    cfg: &ChangelogConfig,
    cwd: &Path,
) -> Result<Vec<ChangelogEntry>, ChangelogError> {
    let tags = discover_tags(cfg, cwd)?;
    let skip_patterns = compile_skip_patterns(cfg)?;
    let mut entries = Vec::new();
    for (index, tag) in tags.iter().enumerate() {
        let previous = tags.get(index + 1).map(String::as_str);
        let prs = merges_between(previous, tag, &skip_patterns, cwd)?;
        if prs.is_empty() {
            continue;
        }
        entries.push(ChangelogEntry {
            version: version_from_tag(tag, &cfg.tag_filter),
            tag: tag.clone(),
            date: tag_date(tag, cwd)?,
            prs,
        });
    }
    Ok(entries)
}

/// Render the full `CHANGELOG.md` body.
#[must_use]
pub fn render_changelog(entries: &[ChangelogEntry], cfg: &ChangelogConfig) -> String {
    let mut lines = vec![
        format!("# {}", cfg.title),
        String::new(),
        format!(
            "All notable changes to {} are documented here. Each entry links",
            cfg.product
        ),
        format!("to its [GitHub Release]({}/releases).", cfg.repo_url),
        String::new(),
    ];
    if let Some(header_comment) = &cfg.header_comment {
        lines.extend(header_comment.lines().map(ToOwned::to_owned));
        lines.push(String::new());
    }
    for entry in entries {
        lines.push(format!(r#"<a id="{}"></a>"#, anchor(entry)));
        lines.push(format!("## [{}] - {}", entry.version, entry.date));
        lines.push(String::new());
        for (number, subject) in &entry.prs {
            lines.push(format!(
                "- {subject} ([#{number}]({}/pull/{number}))",
                cfg.repo_url
            ));
        }
        lines.push(String::new());
    }
    for entry in entries {
        lines.push(format!(
            "[{}]: {}/releases/tag/{}",
            entry.version, cfg.repo_url, entry.tag
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Render release-page markdown for one entry.
#[must_use]
pub fn render_release_notes(
    entry: &ChangelogEntry,
    previous: Option<&ChangelogEntry>,
    cfg: &ChangelogConfig,
) -> String {
    let mut lines = vec![format!("## What's new in {}", entry.tag), String::new()];
    for (number, subject) in &entry.prs {
        lines.push(format!("- {subject} (#{number})"));
    }
    lines.push(String::new());
    lines.push(format!(
        "**Full changelog:** [CHANGELOG.md § {}]({}/blob/main/CHANGELOG.md#{})",
        entry.version,
        cfg.repo_url,
        anchor(entry)
    ));
    if let Some(previous) = previous {
        lines.push(format!(
            "**Previous release:** [{}]({}/releases/tag/{})",
            previous.tag, cfg.repo_url, previous.tag
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Resolve the configured changelog path under `cwd`.
#[must_use]
pub fn changelog_path(cfg: &ChangelogConfig, cwd: &Path) -> PathBuf {
    let path = Path::new(&cfg.path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn disabled_config() -> ChangelogConfig {
    ChangelogConfig {
        enabled: false,
        repo_url: String::new(),
        path: "CHANGELOG.md".to_owned(),
        tag_filter: "v*".to_owned(),
        product: "this project".to_owned(),
        skip_commit_patterns: default_skip_patterns(),
        title: "Changelog".to_owned(),
        header_comment: None,
    }
}

fn default_skip_patterns() -> Vec<String> {
    DEFAULT_SKIP_PATTERNS
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn table_str(table: &toml::Table, key: &str, default: &str) -> String {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or(default)
        .to_owned()
}

fn compile_skip_patterns(cfg: &ChangelogConfig) -> Result<Vec<Regex>, ChangelogError> {
    cfg.skip_commit_patterns
        .iter()
        .map(|pattern| {
            RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map_err(|error| ChangelogError::InvalidPattern {
                    pattern: pattern.clone(),
                    message: error.to_string(),
                })
        })
        .collect()
}

fn merges_between(
    previous: Option<&str>,
    current: &str,
    skip_patterns: &[Regex],
    cwd: &Path,
) -> Result<Vec<(u64, String)>, ChangelogError> {
    let range = previous.map_or_else(
        || current.to_owned(),
        |previous| format!("{previous}..{current}"),
    );
    let output = match run_git(
        cwd,
        &["log", &range, "--first-parent", "--pretty=format:%s"],
    ) {
        Ok(output) => output,
        Err(ChangelogError::Git { .. }) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let squash = Regex::new(r"\s*\(#(\d+)\)\s*$").expect("squash PR regex is valid");
    let merge =
        Regex::new(r"^Merge pull request #(\d+) from .+?/(.+)$").expect("merge PR regex is valid");
    let mut prs = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for line in output.lines() {
        let Some((number, subject)) = pr_from_subject(line, &squash, &merge) else {
            continue;
        };
        if seen.contains(&number)
            || skip_patterns
                .iter()
                .any(|pattern| pattern.is_match(&subject))
        {
            continue;
        }
        seen.insert(number);
        prs.push((number, subject));
    }
    Ok(prs)
}

fn pr_from_subject(line: &str, squash: &Regex, merge: &Regex) -> Option<(u64, String)> {
    if let Some(captures) = squash.captures(line) {
        let matched = captures.get(0)?;
        let number = captures.get(1)?.as_str().parse().ok()?;
        let subject = line[..matched.start()].trim_end().to_owned();
        return Some((number, subject));
    }
    let captures = merge.captures(line)?;
    let number = captures.get(1)?.as_str().parse().ok()?;
    let subject = captures
        .get(2)?
        .as_str()
        .replace('-', " ")
        .trim()
        .to_owned();
    Some((
        number,
        if subject.is_empty() {
            "Merge".to_owned()
        } else {
            subject
        },
    ))
}

fn tag_date(tag: &str, cwd: &Path) -> Result<String, ChangelogError> {
    run_git(cwd, &["log", "-1", "--format=%cI", tag]).map(|iso| iso.chars().take(10).collect())
}

fn version_from_tag(tag: &str, tag_filter: &str) -> String {
    let prefix = tag_filter.trim_end_matches('*');
    if !prefix.is_empty()
        && let Some(version) = tag.strip_prefix(prefix)
    {
        return version.to_owned();
    }
    tag.strip_prefix('v').unwrap_or(tag).to_owned()
}

fn anchor(entry: &ChangelogEntry) -> String {
    format!("v{}", entry.version.replace('.', ""))
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, ChangelogError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| ChangelogError::Git {
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(ChangelogError::Git {
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use crate::config::{LoadedConfig, LocalOverlaySource};

    use super::*;

    #[test]
    fn config_loader_uses_defaults_and_overrides() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join(".shipyard");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(
            project.join("config.toml"),
            r#"
[release.changelog]
enabled = true
repo_url = "https://github.com/owner/repo"
product = "Demo"
skip_commit_patterns = ["^docs:"]
"#,
        )
        .expect("config");
        let config = LoadedConfig::load(None, Some(project), None, LocalOverlaySource::None)
            .expect("config");

        let cfg = load_changelog_config(&config);

        assert!(cfg.enabled);
        assert_eq!(cfg.repo_url, "https://github.com/owner/repo");
        assert_eq!(cfg.path, "CHANGELOG.md");
        assert_eq!(cfg.skip_commit_patterns, vec!["^docs:"]);
    }

    #[test]
    fn renders_entries_and_release_notes_with_python_shape() {
        let cfg = ChangelogConfig {
            enabled: true,
            repo_url: "https://github.com/owner/repo".to_owned(),
            path: "CHANGELOG.md".to_owned(),
            tag_filter: "v*".to_owned(),
            product: "Demo".to_owned(),
            skip_commit_patterns: default_skip_patterns(),
            title: "Changelog".to_owned(),
            header_comment: None,
        };
        let previous = ChangelogEntry {
            version: "0.1.0".to_owned(),
            tag: "v0.1.0".to_owned(),
            date: "2026-04-24".to_owned(),
            prs: vec![(1, "feat: initial".to_owned())],
        };
        let current = ChangelogEntry {
            version: "0.2.0".to_owned(),
            tag: "v0.2.0".to_owned(),
            date: "2026-04-25".to_owned(),
            prs: vec![(2, "feat: next".to_owned())],
        };

        let changelog = render_changelog(&[current.clone(), previous.clone()], &cfg);
        let notes = render_release_notes(&current, Some(&previous), &cfg);

        assert!(changelog.contains("# Changelog"));
        assert!(changelog.contains(r#"<a id="v020"></a>"#));
        assert!(changelog.contains("- feat: next ([#2](https://github.com/owner/repo/pull/2))"));
        assert!(changelog.contains("[0.2.0]: https://github.com/owner/repo/releases/tag/v0.2.0"));
        assert!(notes.contains("## What's new in v0.2.0"));
        assert!(notes.contains("- feat: next (#2)"));
        assert!(notes.contains("**Previous release:** [v0.1.0]"));
    }

    #[test]
    fn builds_entries_from_git_tag_graph_and_skips_empty_versions() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_repo(temp.path());
        let cfg = ChangelogConfig {
            enabled: true,
            repo_url: "https://github.com/owner/repo".to_owned(),
            path: "CHANGELOG.md".to_owned(),
            tag_filter: "v*".to_owned(),
            product: "Demo".to_owned(),
            skip_commit_patterns: default_skip_patterns(),
            title: "Changelog".to_owned(),
            header_comment: None,
        };

        let entries = build_entries(&cfg, temp.path()).expect("entries");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].tag, "v0.2.0");
        assert_eq!(entries[0].prs, vec![(3, "feat: next".to_owned())]);
        assert_eq!(entries[1].tag, "v0.1.0");
        assert_eq!(entries[1].prs, vec![(1, "feat: initial".to_owned())]);
    }

    fn seed_repo(root: &Path) {
        git(root, &["init", "--quiet", "--initial-branch=main"]);
        commit(root, "one.txt", "one", "feat: initial (#1)");
        git(root, &["tag", "v0.1.0"]);
        commit(root, "bump.txt", "bump", "chore: bump package version (#2)");
        git(root, &["tag", "v0.1.1"]);
        commit(root, "two.txt", "two", "feat: next (#3)");
        git(root, &["tag", "v0.2.0"]);
    }

    fn commit(root: &Path, file: &str, contents: &str, message: &str) {
        std::fs::write(root.join(file), contents).expect("file");
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false"])
            .args(args)
            .current_dir(root)
            .status()
            .expect("git");
        assert!(status.success(), "git failed: {args:?}");
    }
}
