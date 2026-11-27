//! Apply CI's expected derived files to a pull request (`shipyard
//! apply-derived`), and decide when a red pull request is red only on
//! derived-file drift.
//!
//! A *derived file* is regenerated from the tree, such as Pulp's
//! `test/ctest_script_inputs/` fragments. Its drift check fails when a pull
//! request changes what the file is derived from without regenerating it, and
//! the check uploads the files it expected as a workflow artifact. Applying
//! that artifact fixes the red without a configure, a build or any of the
//! repository's own code.
//!
//! Everything in the artifact is attacker-influenced: the pull request's code
//! produced it. So this module never trusts it:
//!
//! - [`safe_zip`] parses the archive in memory, never through a shell
//!   extractor, with size limits enforced while inflating.
//! - Only regular files are accepted: no symlinks, devices or directories with
//!   content, and no setuid, setgid or sticky bits. Incoming permission bits
//!   are ignored.
//! - Every entry name is normalized by [`normalize_path`], which refuses `..`,
//!   `.`, empty segments, absolute paths, drive letters, backslashes, NUL,
//!   control characters and any `.git` segment. After mapping onto its
//!   artifact's root it must fall inside the base branch's allowlist
//!   ([`DerivedFilesConfig::allows`]).
//!
//! The configuration is read from the **base** branch's tracked
//! `.shipyard/config.toml`, never from the pull request:
//!
//! ```toml
//! [derived_files]
//! # Allowlist: a trailing `/` admits everything under a directory, anything
//! # else is one exact file.
//! paths = ["test/ctest_script_inputs/", ".shipyard/changed-surface-families.toml"]
//! # The ctest names of the drift checks. A pull request is red only on drift
//! # when every failing required check failed only these tests.
//! drift_tests = ["script-test-inputs-drift", "changed-surface-script-families-drift"]
//! # Trailer keys copied from the previous tip commit, because the trailer
//! # gates read the tip.
//! carry_trailers = ["Version-Bump", "Skill-Update", "Release"]
//! # Trailer lines added to every apply commit.
//! commit_trailers = []
//!
//! # Each artifact a drift check uploads, and the directory its entries are
//! # relative to (`""` for the repository root).
//! [[derived_files.artifacts]]
//! name = "script-inputs-expected"
//! root = "test/ctest_script_inputs"
//! ```

pub mod safe_zip;
#[cfg(test)]
pub(crate) mod test_zip;

use serde::Serialize;

/// The base config table.
pub const CONFIG_TABLE: &str = "derived_files";
/// The longest accepted path, in bytes.
pub const MAX_PATH_BYTES: usize = 512;
/// The commit subject, before the run id.
pub const COMMIT_SUBJECT_PREFIX: &str = "chore: apply CI's expected derived files from run ";

/// One artifact a drift check uploads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArtifactMapping {
    /// The artifact's exact name.
    pub name: String,
    /// Repository-relative directory its entries are relative to; empty for
    /// the repository root.
    pub root: String,
}

/// The base branch's `[derived_files]` table.
///
/// [`Default`] is unconfigured: nothing applies and the steward never names
/// the command.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DerivedFilesConfig {
    /// Allowlisted paths: `dir/` for a directory, anything else exact.
    pub paths: Vec<String>,
    /// The artifacts that carry expected files.
    pub artifacts: Vec<ArtifactMapping>,
    /// The ctest names of the drift checks.
    pub drift_tests: Vec<String>,
    /// Trailer keys carried from the previous tip commit.
    pub carry_trailers: Vec<String>,
    /// Trailer lines added to every apply commit.
    pub commit_trailers: Vec<String>,
    /// Why the table is unusable, when it is. A table with an error is
    /// treated as unconfigured (fail closed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DerivedFilesConfig {
    /// Parse `[derived_files]` from a whole base config. An absent table is
    /// [`Default`]; a malformed one keeps what parsed and sets
    /// [`Self::error`].
    #[must_use]
    pub fn from_table(table: &toml::Table) -> Self {
        let Some(value) = table.get(CONFIG_TABLE) else {
            return Self::default();
        };
        let Some(section) = value.as_table() else {
            return Self::failed(format!("{CONFIG_TABLE} must be a table"));
        };
        let mut config = Self::default();
        let mut errors = Vec::new();
        config.paths = strings(section, "paths", &mut errors)
            .into_iter()
            .filter_map(|raw| match normalize_allowlist_entry(&raw) {
                Ok(path) => Some(path),
                Err(error) => {
                    errors.push(format!("{CONFIG_TABLE}.paths `{raw}`: {error}"));
                    None
                }
            })
            .collect();
        config.drift_tests = strings(section, "drift_tests", &mut errors);
        config.carry_trailers = strings(section, "carry_trailers", &mut errors);
        config.commit_trailers = strings(section, "commit_trailers", &mut errors);
        for line in &config.commit_trailers {
            if !is_trailer_line(line) {
                errors.push(format!(
                    "{CONFIG_TABLE}.commit_trailers `{line}` is not a `Key: value` line"
                ));
            }
        }
        match section.get("artifacts") {
            None => {}
            Some(toml::Value::Array(items)) => {
                for item in items {
                    match parse_mapping(item) {
                        Ok(mapping) => config.artifacts.push(mapping),
                        Err(error) => errors.push(format!("{CONFIG_TABLE}.artifacts: {error}")),
                    }
                }
            }
            Some(_) => errors.push(format!("{CONFIG_TABLE}.artifacts must be an array")),
        }
        if !errors.is_empty() {
            config.error = Some(errors.join("; "));
        }
        config
    }

    /// Parse the whole base config text.
    #[must_use]
    pub fn from_config_text(text: &str) -> Self {
        match text.parse::<toml::Table>() {
            Ok(table) => Self::from_table(&table),
            Err(error) => Self::failed(format!(".shipyard/config.toml does not parse: {error}")),
        }
    }

    fn failed(error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::default()
        }
    }

    /// Whether the command may apply anything: an allowlist and at least one
    /// artifact, with no parse error.
    #[must_use]
    pub fn applies(&self) -> bool {
        self.error.is_none() && !self.paths.is_empty() && !self.artifacts.is_empty()
    }

    /// Whether the steward may name the command: [`Self::applies`] and drift
    /// test names to recognize the red by.
    #[must_use]
    pub fn advises(&self) -> bool {
        self.applies() && !self.drift_tests.is_empty()
    }

    /// Whether the normalized repository path `path` is allowlisted.
    #[must_use]
    pub fn allows(&self, path: &str) -> bool {
        self.paths.iter().any(|allowed| {
            allowed.strip_suffix('/').map_or_else(
                || path == allowed,
                |dir| {
                    path.strip_prefix(dir)
                        .and_then(|rest| rest.strip_prefix('/'))
                        .is_some_and(|rest| !rest.is_empty())
                },
            )
        })
    }

    /// The artifact mapping named `name`.
    #[must_use]
    pub fn mapping(&self, name: &str) -> Option<&ArtifactMapping> {
        self.artifacts.iter().find(|mapping| mapping.name == name)
    }
}

fn strings(section: &toml::Table, key: &str, errors: &mut Vec<String>) -> Vec<String> {
    match section.get(key) {
        None => Vec::new(),
        Some(toml::Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                match item.as_str().map(str::trim) {
                    Some(text) if !text.is_empty() => out.push(text.to_owned()),
                    _ => errors.push(format!(
                        "{CONFIG_TABLE}.{key} must be an array of non-empty strings"
                    )),
                }
            }
            out
        }
        Some(_) => {
            errors.push(format!("{CONFIG_TABLE}.{key} must be an array"));
            Vec::new()
        }
    }
}

fn parse_mapping(item: &toml::Value) -> Result<ArtifactMapping, String> {
    let table = item.as_table().ok_or("each entry must be a table")?;
    let name = table
        .get("name")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or("each entry needs a non-empty `name`")?;
    let root = table
        .get("root")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("`{name}` needs a `root` (\"\" for the repository root)"))?;
    let root = root.trim().trim_end_matches('/');
    let root = if root.is_empty() || root == "." {
        String::new()
    } else {
        normalize_path(root).map_err(|error| format!("`{name}` root `{root}`: {error}"))?
    };
    Ok(ArtifactMapping {
        name: name.to_owned(),
        root,
    })
}

fn normalize_allowlist_entry(raw: &str) -> Result<String, String> {
    match raw.strip_suffix('/') {
        Some(dir) => normalize_path(dir).map(|dir| format!("{dir}/")),
        None => normalize_path(raw),
    }
}

/// Normalize one relative path from the artifact or the config, refusing
/// anything that could name a path outside the checkout or inside `.git`.
///
/// Accepted: `/`-separated segments of ASCII letters, digits, `.`, `_` and
/// `-` only, with no `.` or `..` segment, no empty segment and no segment
/// starting with `.git` in any case (`.git`, `.gitattributes`,
/// `.gitmodules`, `.github`), at most [`MAX_PATH_BYTES`] long. ASCII only
/// means no Unicode normalization pairs (NFC and NFD of one name collapse to
/// one file on APFS), no bidi overrides and no control characters. The path
/// is returned unchanged.
pub fn normalize_path(path: &str) -> Result<String, String> {
    if path.is_empty() {
        return Err("empty path".to_owned());
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(format!("path longer than {MAX_PATH_BYTES} bytes"));
    }
    if path.starts_with('/') {
        return Err("absolute path".to_owned());
    }
    for segment in path.split('/') {
        match segment {
            "" => return Err("empty path segment".to_owned()),
            "." | ".." => return Err(format!("`{segment}` path segment")),
            _ if segment
                .get(..4)
                .is_some_and(|start| start.eq_ignore_ascii_case(".git")) =>
            {
                return Err(format!("`{segment}`: a segment starting with `.git`"));
            }
            _ if !segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) =>
            {
                return Err(
                    "path segments may hold only ASCII letters, digits, `.`, `_` and `-` \
                     (no backslash, drive letter, NUL, control, bidi or non-ASCII character)"
                        .to_owned(),
                );
            }
            _ => {}
        }
    }
    Ok(path.to_owned())
}

/// Map an artifact entry onto the repository: its normalized name under the
/// mapping's root, which must be allowlisted.
pub fn destination(
    config: &DerivedFilesConfig,
    mapping: &ArtifactMapping,
    entry: &str,
) -> Result<String, String> {
    let entry = normalize_path(entry).map_err(|error| format!("entry `{entry}`: {error}"))?;
    let path = if mapping.root.is_empty() {
        entry
    } else {
        format!("{}/{entry}", mapping.root)
    };
    let path = normalize_path(&path)?;
    if config.allows(&path) {
        Ok(path)
    } else {
        Err(format!(
            "`{path}` (artifact `{}`) is outside the derived-file allowlist ({})",
            mapping.name,
            config.paths.join(", ")
        ))
    }
}

/// Whether `line` is a single `Key: value` trailer line.
#[must_use]
pub fn is_trailer_line(line: &str) -> bool {
    !line.contains('\n')
        && line.split_once(": ").is_some_and(|(key, value)| {
            !key.is_empty()
                && !value.trim().is_empty()
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// The apply commit's message: the fixed subject, then the previous tip's
/// trailers whose key is in `carry` and the configured `fixed` lines, in that
/// order, without repeats.
#[must_use]
pub fn commit_message(
    run_id: u64,
    previous_trailers: &[String],
    config: &DerivedFilesConfig,
) -> String {
    let mut trailers: Vec<&str> = Vec::new();
    for line in previous_trailers {
        let line = line.trim();
        let Some((key, _)) = line.split_once(": ") else {
            continue;
        };
        if is_trailer_line(line)
            && config
                .carry_trailers
                .iter()
                .any(|carry| carry.eq_ignore_ascii_case(key))
            && !trailers.contains(&line)
        {
            trailers.push(line);
        }
    }
    for line in &config.commit_trailers {
        if !trailers.contains(&line.as_str()) {
            trailers.push(line);
        }
    }
    let mut message = format!("{COMMIT_SUBJECT_PREFIX}{run_id}\n");
    if !trailers.is_empty() {
        message.push('\n');
        for line in trailers {
            message.push_str(line);
            message.push('\n');
        }
    }
    message
}

/// One failing required check, as the steward read it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailingCheck {
    /// The check's context.
    pub context: String,
    /// The ctest names its log's `The following tests FAILED:` blocks list;
    /// `None` when the log could not be read.
    pub failed_tests: Option<Vec<String>>,
}

/// Whether a pull request is red only on derived-file drift: the config
/// advises, an expected-files artifact exists at the head, and every failing
/// required check failed at least one test, all of them drift tests.
///
/// Fails closed: an unread log, a check that failed no named test (a compile
/// error, a crashed step) or any other failing test is not drift-only.
#[must_use]
pub fn drift_only(config: &DerivedFilesConfig, failing: &[FailingCheck], artifact: bool) -> bool {
    config.advises()
        && artifact
        && !failing.is_empty()
        && failing.iter().all(|check| {
            check.failed_tests.as_ref().is_some_and(|tests| {
                !tests.is_empty()
                    && tests
                        .iter()
                        .all(|test| config.drift_tests.iter().any(|drift| drift == test))
            })
        })
}

/// One workflow run at a head, as the runs API reports it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunRecord {
    /// Run id.
    pub id: u64,
    /// `pull_request`, `push`, ...
    pub event: String,
    /// The run's conclusion, once completed.
    pub conclusion: Option<String>,
    /// The commit it ran on.
    pub head_sha: String,
    /// The branch it ran for.
    pub head_branch: String,
    /// `OWNER/REPO` of the head.
    pub head_repo: String,
    /// The pull requests GitHub attributes the run to.
    pub pull_requests: Vec<u64>,
}

/// The `workflow_runs` of one page of `actions/runs`.
#[must_use]
pub fn parse_runs(value: &serde_json::Value) -> Vec<RunRecord> {
    use serde_json::Value;
    let text = |run: &Value, pointer: &str| {
        run.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    value
        .get("workflow_runs")
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|run| {
                    Some(RunRecord {
                        id: run.get("id")?.as_u64()?,
                        event: text(run, "/event"),
                        conclusion: run
                            .get("conclusion")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        head_sha: text(run, "/head_sha"),
                        head_branch: text(run, "/head_branch"),
                        head_repo: text(run, "/head_repository/full_name"),
                        pull_requests: run
                            .get("pull_requests")
                            .and_then(Value::as_array)
                            .map(|prs| {
                                prs.iter()
                                    .filter_map(|pr| pr.get("number").and_then(Value::as_u64))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `run` is a failed run of *this* pull request at `head`: its head
/// is `head` in `repo`, and it is a `pull_request` run GitHub attributes to
/// `number`, or a `push` run of the pull request's head branch. A run another
/// pull request from the same commit triggered (with a workflow from a base
/// its author chose) never matches.
#[must_use]
pub fn run_belongs(run: &RunRecord, repo: &str, number: u64, head: &str, head_ref: &str) -> bool {
    run.conclusion.as_deref() == Some("failure")
        && run.head_sha.eq_ignore_ascii_case(head)
        && run.head_repo.eq_ignore_ascii_case(repo)
        && match run.event.as_str() {
            "pull_request" => run.pull_requests.contains(&number),
            "push" => run.head_branch == head_ref,
            _ => false,
        }
}

/// Pages of `actions/runs?head_sha=` read, 100 runs each.
pub const MAX_RUN_PAGES: u32 = 3;

/// Every run at `head`, paging through `read` (one `gh api` path per page).
pub fn runs_at_head(
    read: &dyn Fn(&str) -> Result<serde_json::Value, String>,
    repo: &str,
    head: &str,
) -> Result<Vec<RunRecord>, String> {
    let mut runs = Vec::new();
    for page in 1..=MAX_RUN_PAGES {
        let value = read(&format!(
            "repos/{repo}/actions/runs?head_sha={head}&per_page=100&page={page}"
        ))?;
        let batch = parse_runs(&value);
        let done = batch.len() < 100;
        runs.extend(batch);
        if done {
            break;
        }
    }
    Ok(runs)
}

#[cfg(test)]
mod tests;
