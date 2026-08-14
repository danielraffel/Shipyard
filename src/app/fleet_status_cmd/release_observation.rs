use super::{
    DateTime, Engine, GitHubActions, MAX_RELEASE_COMMIT_LOOKUPS_PER_TICK, OBSERVATION_MAX_PAGES,
    OBSERVATION_PAGE_SIZE, ObservationReason, ReleaseProbe, Utc, Value, assess_release_liveness,
};

#[derive(Debug, Eq, PartialEq)]
pub(super) struct ReleasableCommitSummary {
    pub(super) count: u64,
    pub(super) truncated: bool,
    pub(super) oldest_committed_at: Option<String>,
}

pub(super) fn inspect_release_liveness(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    stale_threshold_secs: i64,
) -> Result<ReleaseProbe, String> {
    let raw = match actions.run_gh(&["api".to_owned(), format!("repos/{repo}/releases/latest")]) {
        Ok(raw) => raw,
        Err(error) if github_not_found(&error.to_string()) => {
            return Ok(ReleaseProbe {
                readable: true,
                source: "github (no releases)".to_owned(),
                reason_codes: Vec::new(),
                report: None,
            });
        }
        Err(error) => return Err(format!("inspect latest release failed: {error}")),
    };
    let release: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("could not parse latest release JSON: {error}"))?;
    let tag = release
        .get("tag_name")
        .and_then(Value::as_str)
        .filter(|tag| !tag.is_empty())
        .ok_or_else(|| "latest release response missing tag_name".to_owned())?;
    let published_at = release
        .get("published_at")
        .and_then(Value::as_str)
        .filter(|timestamp| !timestamp.is_empty())
        .ok_or_else(|| "latest release response missing published_at".to_owned())?;
    let raw = actions
        .run_gh(&["api".to_owned(), release_compare_path(repo, tag, base)])
        .map_err(|error| format!("compare latest release to {base} failed: {error}"))?;
    let comparison: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("could not parse release comparison JSON: {error}"))?;
    let commits_ahead = comparison
        .get("ahead_by")
        .and_then(Value::as_u64)
        .ok_or_else(|| "release comparison response missing ahead_by".to_owned())?;
    let releasable = count_releasable_commits(actions, repo, &comparison, commits_ahead)?;
    let base_version = fetch_base_version(actions, repo, base)?;
    let mut optional_reason_codes = Vec::new();
    let (open_release_incident_issues, issues_truncated) =
        if let Ok((count, truncated)) = fetch_release_incident_issue_count(actions, repo) {
            (Some(count), truncated)
        } else {
            optional_reason_codes.push(ObservationReason::AuxiliaryObservationUnavailable);
            (None, false)
        };
    if issues_truncated {
        optional_reason_codes.push(ObservationReason::AuxiliaryObservationUnavailable);
    }
    let latest_successful_release_workflow_at =
        if let Ok(value) = fetch_latest_successful_release_workflow(actions, repo, base) {
            value
        } else {
            optional_reason_codes.push(ObservationReason::AuxiliaryObservationUnavailable);
            None
        };
    let observation_truncated = releasable.truncated;
    Ok(ReleaseProbe {
        readable: true,
        source: "github".to_owned(),
        reason_codes: observation_truncated
            .then_some(ObservationReason::ObservationTruncated)
            .into_iter()
            .chain(optional_reason_codes)
            .collect(),
        report: Some(assess_release_liveness(
            tag.to_owned(),
            published_at.to_owned(),
            commits_ahead,
            releasable.count,
            releasable.oldest_committed_at,
            base_version,
            open_release_incident_issues,
            latest_successful_release_workflow_at,
            stale_threshold_secs,
            Utc::now(),
        )?),
    })
}

fn github_not_found(error: &str) -> bool {
    error.contains("HTTP 404") || error.contains("404 Not Found")
}

pub(super) fn count_releasable_commits(
    actions: &GitHubActions,
    repo: &str,
    comparison: &Value,
    commits_ahead: u64,
) -> Result<ReleasableCommitSummary, String> {
    if commits_ahead == 0 {
        return Ok(ReleasableCommitSummary {
            count: 0,
            truncated: false,
            oldest_committed_at: None,
        });
    }
    let commits = comparison
        .get("commits")
        .and_then(Value::as_array)
        .ok_or_else(|| "release comparison response missing commits".to_owned())?;
    if u64::try_from(commits.len()).ok() != Some(commits_ahead) {
        return Ok(ReleasableCommitSummary {
            count: commits_ahead,
            truncated: true,
            oldest_committed_at: None,
        });
    }
    let unskipped = commits
        .iter()
        .map(|commit| {
            let message = commit
                .pointer("/commit/message")
                .and_then(Value::as_str)
                .ok_or_else(|| "release comparison commit missing message".to_owned())?;
            Ok((!release_is_skipped(message)).then_some(commit))
        })
        .collect::<Result<Vec<_>, String>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let mut releasable = u64::try_from(
        unskipped
            .len()
            .saturating_sub(MAX_RELEASE_COMMIT_LOOKUPS_PER_TICK),
    )
    .expect("bounded commit count fits u64");
    let mut truncated = unskipped.len() > MAX_RELEASE_COMMIT_LOOKUPS_PER_TICK;
    let mut oldest = None::<(DateTime<Utc>, String)>;
    for commit in unskipped
        .into_iter()
        .take(MAX_RELEASE_COMMIT_LOOKUPS_PER_TICK)
    {
        let message = commit
            .pointer("/commit/message")
            .and_then(Value::as_str)
            .ok_or_else(|| "release comparison commit missing message".to_owned())?;
        debug_assert!(!release_is_skipped(message));
        let sha = commit
            .get("sha")
            .and_then(Value::as_str)
            .ok_or_else(|| "release comparison commit missing sha".to_owned())?;
        let detail = gh_json_value(
            actions,
            &["api".to_owned(), format!("repos/{repo}/commits/{sha}")],
            "inspect release commit",
        )?;
        let files = detail
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("release commit {sha} missing files"))?;
        let requires_release = if files.len() == 300 {
            truncated = true;
            true
        } else {
            files.is_empty() || files.iter().any(file_change_requires_release)
        };
        if requires_release {
            releasable += 1;
            let timestamp = commit
                .pointer("/commit/committer/date")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("release commit {sha} missing committer date"))?;
            let parsed = DateTime::parse_from_rfc3339(timestamp)
                .map_err(|error| {
                    format!("release commit {sha} has invalid committer date: {error}")
                })?
                .with_timezone(&Utc);
            if oldest.as_ref().is_none_or(|(current, _)| parsed < *current) {
                oldest = Some((parsed, timestamp.to_owned()));
            }
        }
    }
    Ok(ReleasableCommitSummary {
        count: releasable,
        truncated,
        oldest_committed_at: oldest.map(|(_, timestamp)| timestamp),
    })
}

fn gh_json_value(actions: &GitHubActions, args: &[String], purpose: &str) -> Result<Value, String> {
    let raw = actions
        .run_gh(args)
        .map_err(|error| format!("{purpose} failed: {error}"))?;
    serde_json::from_str(&raw)
        .map_err(|error| format!("{purpose} returned malformed JSON: {error}"))
}

pub(super) fn release_is_skipped(message: &str) -> bool {
    let mut lines = message.lines().collect::<Vec<_>>();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let Some(separator) = lines.iter().rposition(|line| line.trim().is_empty()) else {
        return false;
    };
    if !lines[..separator]
        .iter()
        .any(|line| !line.trim().is_empty())
    {
        return false;
    }
    let trailer_start = separator + 1;
    let mut trailers = Vec::new();
    for line in &lines[trailer_start..] {
        let trimmed = line.trim_start();
        if line.starts_with('#') {
            continue;
        }
        if trimmed.len() != line.len() {
            if trailers.is_empty() {
                return false;
            }
            continue;
        }
        let Some(trailer) = parse_trailer(line) else {
            return false;
        };
        trailers.push(trailer);
    }
    trailers.iter().any(|(key, value)| {
        key.eq_ignore_ascii_case("release")
            && value
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("skip"))
    })
}

fn parse_trailer(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    let key = key.trim();
    (!key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then_some((key, value.trim_start()))
}

pub(super) fn path_requires_release(path: &str) -> bool {
    let normalized = path.to_ascii_lowercase();
    !(normalized.starts_with("docs/")
        || normalized.starts_with(".claude-plugin/")
        || normalized.starts_with("commands/")
        || normalized.starts_with("skills/")
        || normalized.starts_with("agents/")
        || normalized.starts_with("hooks/")
        || matches!(
            normalized.as_str(),
            "changelog.md"
                | "readme.md"
                | "code_of_conduct.md"
                | "contributing.md"
                | "security.md"
                | "license"
                | "license.md"
        ))
}

pub(super) fn file_change_requires_release(file: &Value) -> bool {
    let Some(filename) = file.get("filename").and_then(Value::as_str) else {
        return true;
    };
    if path_requires_release(filename) {
        return true;
    }
    match file.get("previous_filename") {
        None => false,
        Some(Value::String(previous)) => path_requires_release(previous),
        Some(_) => true,
    }
}

pub(super) fn fetch_release_incident_issue_count(
    actions: &GitHubActions,
    repo: &str,
) -> Result<(u64, bool), String> {
    let mut count = 0u64;
    for page in 1..=OBSERVATION_MAX_PAGES {
        let raw = actions
            .run_gh(&[
                "api".to_owned(),
                format!(
                    "repos/{repo}/issues?state=open&per_page={OBSERVATION_PAGE_SIZE}&page={page}"
                ),
            ])
            .map_err(|error| format!("inspect open release incidents failed: {error}"))?;
        let issues: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("could not parse open issues JSON: {error}"))?;
        let issues = issues
            .as_array()
            .ok_or_else(|| "open issues response is not an array".to_owned())?;
        count += issues
            .iter()
            .filter(|issue| issue.get("pull_request").is_none())
            .filter(|issue| {
                let title = issue
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                title.contains("release")
                    && (title.contains("stuck")
                        || title.contains("blocked")
                        || title.contains("failed")
                        || title.contains("failure"))
            })
            .count() as u64;
        if issues.len() < OBSERVATION_PAGE_SIZE {
            return Ok((count, false));
        }
    }
    Ok((count, true))
}

pub(super) fn fetch_latest_successful_release_workflow(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Option<String>, String> {
    let raw = match actions.run_gh(&["api".to_owned(), release_workflow_runs_path(repo, base)]) {
        Ok(raw) => raw,
        Err(error) if error.to_string().contains("404") => return Ok(None),
        Err(error) => return Err(format!("inspect auto-release workflow failed: {error}")),
    };
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("could not parse auto-release workflow runs: {error}"))?;
    Ok(value
        .pointer("/workflow_runs/0/updated_at")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

pub(super) fn fetch_base_version(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Option<String>, String> {
    let raw = match actions.run_gh(&["api".to_owned(), base_version_path(repo, base)]) {
        Ok(raw) => raw,
        Err(error) if error.to_string().contains("404") => return Ok(None),
        Err(error) => return Err(format!("inspect base VERSION failed: {error}")),
    };
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("could not parse base VERSION response: {error}"))?;
    let encoded = value
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "base VERSION response missing content".to_owned())?
        .replace('\n', "");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("decode base VERSION failed: {error}"))?;
    let version = String::from_utf8(bytes)
        .map_err(|error| format!("base VERSION is not UTF-8: {error}"))?
        .trim()
        .to_owned();
    Ok((!version.is_empty()).then_some(version))
}

pub(super) fn release_compare_path(repo: &str, tag: &str, base: &str) -> String {
    format!(
        "repos/{repo}/compare/{}...{}",
        encode_api_component(tag),
        encode_api_component(base)
    )
}

pub(super) fn release_workflow_runs_path(repo: &str, base: &str) -> String {
    format!(
        "repos/{repo}/actions/workflows/auto-release.yml/runs?branch={}&status=success&per_page=1",
        encode_api_component(base)
    )
}

pub(super) fn base_version_path(repo: &str, base: &str) -> String {
    format!(
        "repos/{repo}/contents/VERSION?ref={}",
        encode_api_component(base)
    )
}

pub(super) fn encode_api_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}
