use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;

/// Location of a consumer repository's Shipyard pin file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsumerPin {
    /// `tools/shipyard.toml` path.
    pub pin_file: PathBuf,
    /// Repository root containing `tools/`.
    pub repo_root: PathBuf,
}

/// Parsed semantic version used by safety guards.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VersionTuple(pub u64, pub u64, pub u64);

/// Find `tools/shipyard.toml` by walking upward from `cwd`.
#[must_use]
pub fn detect_consumer_pin(cwd: &Path) -> Option<ConsumerPin> {
    for parent in cwd.canonicalize().ok()?.ancestors() {
        let candidate = parent.join("tools").join("shipyard.toml");
        if candidate.exists() {
            return Some(ConsumerPin {
                pin_file: candidate,
                repo_root: parent.to_path_buf(),
            });
        }
    }
    None
}

/// Return true when `cwd` appears to be inside the Shipyard source repo.
#[must_use]
pub fn is_shipyard_repo(cwd: &Path) -> bool {
    let Ok(cwd) = cwd.canonicalize() else {
        return false;
    };
    for parent in cwd.ancestors() {
        let cargo = parent.join("Cargo.toml");
        if cargo.exists() {
            let Ok(text) = fs::read_to_string(cargo) else {
                return false;
            };
            if text
                .lines()
                .any(|line| line.trim() == "name = \"shipyard\"")
            {
                return true;
            }
        }
        let pyproject = parent.join("pyproject.toml");
        if !pyproject.exists() {
            continue;
        }
        let Ok(text) = fs::read_to_string(pyproject) else {
            return false;
        };
        return text
            .lines()
            .any(|line| line.trim() == "name = \"shipyard\"");
    }
    false
}

/// Read the pinned version string from `tools/shipyard.toml`.
#[must_use]
pub fn read_pinned_version(pin_file: &Path) -> Option<String> {
    let text = fs::read_to_string(pin_file).ok()?;
    parse_pinned_version(&text)
}

/// Parse the first `version = "..."`
#[must_use]
pub fn parse_pinned_version(text: &str) -> Option<String> {
    text.lines().find_map(parse_version_line)
}

/// Rewrite the first `version = "..."`
pub fn rewrite_pinned_version(
    pin_file: &Path,
    new_version: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let text = fs::read_to_string(pin_file)?;
    let Some(new_text) = rewrite_pinned_text(&text, new_version) else {
        return Err(format!("Failed to rewrite version in {}", pin_file.display()).into());
    };
    fs::write(pin_file, new_text)?;
    Ok(())
}

/// Rewrite a pin file's first version assignment while preserving comments.
#[must_use]
pub fn rewrite_pinned_text(text: &str, new_version: &str) -> Option<String> {
    let mut rewritten = Vec::new();
    let mut changed = false;
    for line in text.lines() {
        if !changed && let Some(prefix_end) = version_assignment_prefix_end(line) {
            let prefix = &line[..prefix_end];
            let suffix = trailing_after_version(line, prefix_end).unwrap_or_default();
            rewritten.push(format!("{prefix}\"{new_version}\"{suffix}"));
            changed = true;
        } else {
            rewritten.push(line.to_owned());
        }
    }
    changed.then(|| {
        let mut out = rewritten.join("\n");
        if text.ends_with('\n') {
            out.push('\n');
        }
        out
    })
}

/// Normalize `0.47.0` to `v0.47.0`.
#[must_use]
pub fn normalize_target_version(target: &str) -> String {
    let target = target.trim();
    if target.starts_with('v') {
        target.to_owned()
    } else {
        format!("v{target}")
    }
}

/// Parse `v0.47.0` / `0.47.0` to comparable components.
#[must_use]
pub fn parse_version_tuple(version: &str) -> Option<VersionTuple> {
    let version = version.trim().strip_prefix('v').unwrap_or(version.trim());
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(VersionTuple(major, minor, patch))
}

/// Return true when applying `target` would downgrade `installed`.
#[must_use]
pub fn would_downgrade_installed(target: &str, installed: &str) -> bool {
    let Some(target) = parse_version_tuple(target) else {
        return false;
    };
    let Some(installed) = parse_version_tuple(installed) else {
        return false;
    };
    target < installed
}

/// Return true when `origin/main` already pins a version at least `target`.
#[must_use]
pub fn origin_main_satisfies_target(main_pin: &str, target: &str) -> bool {
    let Some(main_pin) = parse_version_tuple(main_pin) else {
        return false;
    };
    let Some(target) = parse_version_tuple(target) else {
        return false;
    };
    main_pin >= target
}

/// Return the latest published Shipyard release tag via `gh`.
#[must_use]
pub fn latest_shipyard_release() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    latest_shipyard_release_from_cwd(RuntimeMode::Shipyard, &cwd)
}

/// Return the latest published Shipyard release tag via the configured `gh`.
#[must_use]
pub fn latest_shipyard_release_from_cwd(mode: RuntimeMode, cwd: &Path) -> Option<String> {
    let output = GhClient::from_cwd(mode, cwd)
        .ok()?
        .prepare_command(
            cwd,
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
        )
        .ok()?
        .args([
            "release",
            "list",
            "--repo",
            "danielraffel/Shipyard",
            "--limit",
            "1",
            "--json",
            "tagName",
            "--jq",
            ".[0].tagName",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let tag = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!tag.is_empty()).then_some(tag)
}

/// Return the globally installed `shipyard --version` version string.
#[must_use]
pub fn current_global_shipyard_version() -> Option<String> {
    let output = Command::new("shipyard")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_shipyard_version_output(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `shipyard --version` output.
#[must_use]
pub fn parse_shipyard_version_output(output: &str) -> Option<String> {
    let mut tokens = output.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "version" {
            return tokens
                .next()
                .map(|value| value.trim_matches(',').to_owned());
        }
    }
    None
}

/// Best-effort read of `origin/main:tools/shipyard.toml`.
#[must_use]
pub fn main_pinned_version_at_origin(repo_root: &Path) -> Option<String> {
    let fetch = Command::new("git")
        .args(["fetch", "--quiet", "origin", "main"])
        .current_dir(repo_root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !fetch.success() {
        return None;
    }
    let output = Command::new("git")
        .args(["show", "origin/main:tools/shipyard.toml"])
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_pinned_version(&String::from_utf8_lossy(&output.stdout))
}

fn parse_version_line(line: &str) -> Option<String> {
    let prefix_end = version_assignment_prefix_end(line)?;
    let rest = &line[prefix_end..];
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

fn version_assignment_prefix_end(line: &str) -> Option<usize> {
    let trimmed_start = line.trim_start();
    if !trimmed_start.starts_with("version") {
        return None;
    }
    let leading = line.len() - trimmed_start.len();
    let after_key = leading + "version".len();
    let after_key_text = &line[after_key..];
    let equals_relative = after_key_text.find('=')?;
    let prefix_end = after_key + equals_relative + 1;
    Some(prefix_end + line[prefix_end..].find('"')?)
}

fn trailing_after_version(line: &str, prefix_end: usize) -> Option<&str> {
    let rest = line[prefix_end..].strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[end + 1..])
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::{
        detect_consumer_pin, is_shipyard_repo, main_pinned_version_at_origin,
        normalize_target_version, origin_main_satisfies_target, parse_pinned_version,
        parse_shipyard_version_output, parse_version_tuple, rewrite_pinned_text,
        would_downgrade_installed,
    };

    #[test]
    fn parses_and_rewrites_pin_text_without_dropping_comments() {
        let text = "# pin\n[shipyard]\nversion = \"v0.40.0\" # keep\nrepo = \"x\"\n";
        assert_eq!(parse_pinned_version(text), Some("v0.40.0".to_owned()));

        let rewritten = rewrite_pinned_text(text, "v0.47.0").expect("rewrite");
        assert!(rewritten.contains("# pin"));
        assert!(rewritten.contains("version = \"v0.47.0\" # keep"));
        assert!(rewritten.ends_with('\n'));
    }

    #[test]
    fn detects_consumer_pin_from_subdirectory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tools = temp.path().join("tools");
        let subdir = temp.path().join("nested").join("dir");
        std::fs::create_dir_all(&tools).expect("tools");
        std::fs::create_dir_all(&subdir).expect("subdir");
        std::fs::write(tools.join("shipyard.toml"), "version = \"v0.1.0\"\n").expect("pin");

        let pin = detect_consumer_pin(&subdir).expect("pin");
        assert_eq!(
            pin.repo_root,
            temp.path().canonicalize().expect("canonical")
        );
    }

    #[test]
    fn detects_shipyard_repo_by_cargo_package_name() {
        let temp = tempfile::tempdir().expect("tempdir");
        let subdir = temp.path().join("src");
        std::fs::create_dir_all(&subdir).expect("subdir");
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"shipyard\"\nversion = \"0.51.0\"\n",
        )
        .expect("cargo");

        assert!(is_shipyard_repo(&subdir));
    }

    #[test]
    fn parses_version_tuples_and_global_version_output() {
        assert_eq!(normalize_target_version("0.47.0"), "v0.47.0");
        assert!(parse_version_tuple("v0.47.0") > parse_version_tuple("v0.46.9"));
        assert_eq!(
            parse_shipyard_version_output("shipyard, version 0.47.0\n"),
            Some("0.47.0".to_owned())
        );
        assert!(parse_version_tuple("v0.47.0-dev").is_none());
    }

    #[test]
    fn safety_guard_comparisons_skip_unparseable_versions() {
        assert!(would_downgrade_installed("v0.26.0", "0.47.0"));
        assert!(!would_downgrade_installed("v0.48.0", "0.47.0"));
        assert!(!would_downgrade_installed("v0.26.0-dev", "0.47.0"));

        assert!(origin_main_satisfies_target("v0.47.0", "v0.47.0"));
        assert!(origin_main_satisfies_target("v0.48.0", "v0.47.0"));
        assert!(!origin_main_satisfies_target("v0.46.0", "v0.47.0"));
        assert!(!origin_main_satisfies_target("latest", "v0.47.0"));
    }

    #[test]
    fn origin_main_pin_fails_open_when_fetch_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        git(temp.path(), &["init", "--quiet", "--initial-branch=main"]);
        std::fs::create_dir_all(temp.path().join("tools")).expect("tools");
        std::fs::write(
            temp.path().join("tools").join("shipyard.toml"),
            "version = \"v0.47.0\"\n",
        )
        .expect("pin");
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-q", "-m", "seed"]);
        git(
            temp.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(temp.path(), &["remote", "add", "origin", "/does/not/exist"]);

        assert_eq!(main_pinned_version_at_origin(temp.path()), None);
    }

    fn git(cwd: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("git runs");
        assert!(status.success(), "git failed: {args:?}");
    }
}
