//! Pull request shell boundary used by `ship`.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::config::LoadedConfig;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};

/// GitHub pull request metadata needed by ship orchestration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrInfo {
    /// Pull request number.
    pub number: u64,
    /// Pull request URL.
    pub url: String,
    /// Pull request title.
    pub title: String,
    /// Pull request state.
    pub state: String,
    /// Head branch name.
    pub branch: String,
    /// Base branch name.
    pub base: String,
}

/// Error returned by PR shell helpers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrError {
    message: String,
}

impl PrError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn message(&self) -> &str {
        &self.message
    }
}

impl Display for PrError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for PrError {}

/// Push the current branch before PR lookup/create.
pub fn push_branch(cwd: &Path, branch: &str) -> Result<(), PrError> {
    // Supervised git push — sets SHIPYARD_PR_RUNNING=1 so downstream
    // pre-push hooks know this push originated from `shipyard pr`.
    let output = crate::supervised::git_push_supervised()
        .args(["push", "-u", "origin", branch])
        .current_dir(cwd)
        .output()
        .map_err(|error| PrError::new(format!("git push failed to start: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(PrError::new(format!(
        "git push failed: {}",
        stderr_or_stdout(&output)
    )))
}

/// Find the first open PR for `branch`.
pub fn find_pr_for_branch(
    config: &LoadedConfig,
    cwd: &Path,
    gh_command: Option<&Path>,
    branch: &str,
) -> Result<Option<PrInfo>, PrError> {
    let client = gh_client(config)?;
    let output = gh(&client, cwd, gh_command)?
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "open",
            "--limit",
            "1",
            "--json",
            PR_JSON_FIELDS,
        ])
        .current_dir(cwd)
        .output()
        .map_err(|error| PrError::new(format!("gh pr list failed to start: {error}")))?;
    if !output.status.success() {
        let message = stderr_or_stdout(&output);
        if is_graphql_rate_limited(&message) {
            report_rate_limit_fallback_with_client(&client, "gh pr list", cwd);
            return find_pr_for_branch_rest(&client, cwd, gh_command, branch);
        }
        return Err(PrError::new(format!("gh pr list failed: {message}")));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_pr_list(&text)
}

/// Create a PR and normalize its metadata through `gh pr view`.
pub fn create_pr(
    config: &LoadedConfig,
    cwd: &Path,
    gh_command: Option<&Path>,
    branch: &str,
    base: &str,
    title: &str,
    body: &str,
) -> Result<PrInfo, PrError> {
    let client = gh_client(config)?;
    let output = gh(&client, cwd, gh_command)?
        .args([
            "pr", "create", "--head", branch, "--base", base, "--title", title, "--body", body,
        ])
        .output()
        .map_err(|error| PrError::new(format!("gh pr create failed to start: {error}")))?;
    if !output.status.success() {
        let message = stderr_or_stdout(&output);
        if is_graphql_rate_limited(&message) {
            report_graphql_pr_create_fallback_with_client(&client, &message, cwd);
            return create_pr_rest(&client, cwd, gh_command, branch, base, title, body);
        }
        if is_graphql_pr_create_integration_blocked(&message) {
            report_graphql_pr_create_fallback_with_client(&client, &message, cwd);
            return match create_pr_rest(&client, cwd, gh_command, branch, base, title, body) {
                Ok(info) => Ok(info),
                Err(error) if is_integration_blocked(error.message()) => {
                    create_pr_with_ambient_gh(&client, cwd, gh_command, branch, base, title, body)
                }
                Err(error) => Err(error),
            };
        }
        return Err(PrError::new(format!("gh pr create failed: {message}")));
    }
    let selector = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if selector.is_empty() {
        return Err(PrError::new("gh pr create did not print a PR URL"));
    }
    get_pr_status_with_client(&client, cwd, gh_command, &selector)
}

/// Return normalized PR metadata for a PR selector.
pub fn get_pr_status(
    config: &LoadedConfig,
    cwd: &Path,
    gh_command: Option<&Path>,
    selector: &str,
) -> Result<PrInfo, PrError> {
    let client = gh_client(config)?;
    get_pr_status_with_client(&client, cwd, gh_command, selector)
}

fn get_pr_status_with_client(
    client: &GhClient,
    cwd: &Path,
    gh_command: Option<&Path>,
    selector: &str,
) -> Result<PrInfo, PrError> {
    let output = gh(client, cwd, gh_command)?
        .args(["pr", "view", selector, "--json", PR_JSON_FIELDS])
        .output()
        .map_err(|error| PrError::new(format!("gh pr view failed to start: {error}")))?;
    if !output.status.success() {
        let message = stderr_or_stdout(&output);
        if is_graphql_rate_limited(&message) {
            report_rate_limit_fallback_with_client(client, "gh pr view", cwd);
            return get_pr_status_rest(client, cwd, gh_command, selector);
        }
        return Err(PrError::new(format!("gh pr view failed: {message}")));
    }
    parse_pr_info(&String::from_utf8_lossy(&output.stdout))
}

const PR_JSON_FIELDS: &str = "number,url,title,state,headRefName,baseRefName";

fn gh_client(config: &LoadedConfig) -> Result<GhClient, PrError> {
    // Build the gh client from the caller's already-resolved config (not a
    // fresh `from_cwd` read of the real global dir) so PR creation honors the
    // same `[github.auth]` the rest of the ship used — and so tests that pass an
    // isolated config don't pick up the operator's global GitHub App auth.
    GhClient::from_loaded_config(config)
        .map_err(|error| PrError::new(format!("github auth config failed: {error}")))
}

fn gh(client: &GhClient, cwd: &Path, gh_command: Option<&Path>) -> Result<Command, PrError> {
    gh_with_policy(client, cwd, gh_command, GhAuthPolicy::Default)
}

fn gh_with_policy(
    client: &GhClient,
    cwd: &Path,
    gh_command: Option<&Path>,
    auth_policy: GhAuthPolicy,
) -> Result<Command, PrError> {
    client
        .prepare_command(cwd, gh_command, GhSupervision::Supervised, auth_policy)
        .map_err(|error| PrError::new(format!("gh command preparation failed: {error}")))
}

fn stderr_or_stdout(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !stderr.is_empty() {
        return stderr;
    }
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn parse_pr_list(text: &str) -> Result<Option<PrInfo>, PrError> {
    let value = serde_json::from_str::<Value>(text)
        .map_err(|error| PrError::new(format!("failed to parse gh pr list JSON: {error}")))?;
    let Some(items) = value.as_array() else {
        return Err(PrError::new("gh pr list JSON was not an array"));
    };
    items.first().map(parse_pr_value).transpose()
}

fn find_pr_for_branch_rest(
    client: &GhClient,
    cwd: &Path,
    gh_command: Option<&Path>,
    branch: &str,
) -> Result<Option<PrInfo>, PrError> {
    let repo = repo_slug(cwd)?;
    let owner = repo
        .split('/')
        .next()
        .filter(|owner| !owner.is_empty())
        .ok_or_else(|| PrError::new(format!("could not derive owner from repo {repo:?}")))?;
    // Build a query-string URL so `gh api` issues a GET. Passing `-f`
    // without `-X GET` makes gh send the parameters as a request body
    // and switch to POST against `repos/:r/pulls` — which is the
    // *create-PR* endpoint. GitHub then 422s on missing `base`, which
    // is the symptom captured in #282. Force GET semantics explicitly.
    let head = format!("{owner}:{branch}");
    let endpoint = format!(
        "repos/{repo}/pulls?head={}&state=open&per_page=1",
        url_encode(&head)
    );
    let args = vec![
        "api".to_owned(),
        "-X".to_owned(),
        "GET".to_owned(),
        endpoint,
    ];
    let output = gh(client, cwd, gh_command)?
        .args(args)
        .output()
        .map_err(|error| PrError::new(format!("gh REST PR lookup failed to start: {error}")))?;
    if !output.status.success() {
        return Err(PrError::new(format!(
            "gh REST PR lookup failed after GraphQL rate limit: {}",
            stderr_or_stdout(&output)
        )));
    }
    parse_pr_rest_list(&String::from_utf8_lossy(&output.stdout))
}

fn create_pr_rest(
    client: &GhClient,
    cwd: &Path,
    gh_command: Option<&Path>,
    branch: &str,
    base: &str,
    title: &str,
    body: &str,
) -> Result<PrInfo, PrError> {
    let repo = repo_slug(cwd)?;
    let endpoint = format!("repos/{repo}/pulls");
    let args = vec![
        "api".to_owned(),
        "-X".to_owned(),
        "POST".to_owned(),
        endpoint,
        "-f".to_owned(),
        format!("title={title}"),
        "-f".to_owned(),
        format!("head={branch}"),
        "-f".to_owned(),
        format!("base={base}"),
        "-f".to_owned(),
        format!("body={body}"),
    ];
    let output = gh(client, cwd, gh_command)?
        .args(args)
        .output()
        .map_err(|error| PrError::new(format!("gh REST PR create failed to start: {error}")))?;
    if !output.status.success() {
        return Err(PrError::new(format!(
            "gh pr create hit GraphQL rate limit, then REST fallback failed: {}",
            stderr_or_stdout(&output)
        )));
    }
    parse_pr_rest_info(&String::from_utf8_lossy(&output.stdout))
}

fn create_pr_with_ambient_gh(
    client: &GhClient,
    cwd: &Path,
    gh_command: Option<&Path>,
    branch: &str,
    base: &str,
    title: &str,
    body: &str,
) -> Result<PrInfo, PrError> {
    eprintln!(
        "shipyard: GitHub App token cannot create this pull request through GraphQL or REST. Falling back to ambient gh auth for PR creation only."
    );
    let output = gh_with_policy(client, cwd, gh_command, GhAuthPolicy::AmbientOnly)?
        .args([
            "pr", "create", "--head", branch, "--base", base, "--title", title, "--body", body,
        ])
        .output()
        .map_err(|error| PrError::new(format!("ambient gh pr create failed to start: {error}")))?;
    if !output.status.success() {
        return Err(PrError::new(format!(
            "ambient gh pr create failed after GitHub App fallback was blocked: {}",
            stderr_or_stdout(&output)
        )));
    }
    let selector = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if selector.is_empty() {
        return Err(PrError::new("ambient gh pr create did not print a PR URL"));
    }
    get_pr_status_with_client(client, cwd, gh_command, &selector)
}

fn get_pr_status_rest(
    client: &GhClient,
    cwd: &Path,
    gh_command: Option<&Path>,
    selector: &str,
) -> Result<PrInfo, PrError> {
    let repo = repo_slug(cwd)?;
    let number = selector_pr_number(selector)
        .ok_or_else(|| PrError::new(format!("could not parse PR selector {selector:?}")))?;
    let endpoint = format!("repos/{repo}/pulls/{number}");
    let output = gh(client, cwd, gh_command)?
        .args(["api", &endpoint])
        .output()
        .map_err(|error| PrError::new(format!("gh REST PR view failed to start: {error}")))?;
    if !output.status.success() {
        return Err(PrError::new(format!(
            "gh REST PR view failed after GraphQL rate limit: {}",
            stderr_or_stdout(&output)
        )));
    }
    parse_pr_rest_info(&String::from_utf8_lossy(&output.stdout))
}

fn repo_slug(cwd: &Path) -> Result<String, PrError> {
    let output = crate::supervised::git_supervised()
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd)
        .output()
        .map_err(|error| PrError::new(format!("git remote probe failed: {error}")))?;
    if !output.status.success() {
        return Err(PrError::new(format!(
            "git remote probe failed: {}",
            stderr_or_stdout(&output)
        )));
    }
    let remote = String::from_utf8_lossy(&output.stdout);
    parse_github_remote_slug(remote.trim()).ok_or_else(|| {
        PrError::new(format!(
            "remote.origin.url is not a supported GitHub remote: {}",
            remote.trim()
        ))
    })
}

/// Minimal percent-encoder for query-string values used by REST fallbacks.
/// Encodes the characters `gh api` needs us to escape (`:` `/` `?` `&` `=` `#`
/// and spaces) so query parameters survive without pulling in a dependency.
fn url_encode(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
            _ => {
                let mut buf = [0u8; 4];
                for byte in ch.encode_utf8(&mut buf).as_bytes() {
                    write!(&mut out, "%{byte:02X}").expect("write to String");
                }
            }
        }
    }
    out
}

fn parse_github_remote_slug(remote: &str) -> Option<String> {
    crate::gh::parse_github_remote_slug(remote)
}

fn selector_pr_number(selector: &str) -> Option<u64> {
    selector
        .trim()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|part| part.parse::<u64>().ok())
}

/// Detect the surface-level marker of a GraphQL rate-limit exhaustion in
/// `gh` stderr. Used by `src/pr.rs` (existing) and `src/app/auto_merge_cmd.rs`
/// to opt into a REST fallback when GraphQL is at 0/5000 but REST still has
/// budget.
#[must_use]
pub(crate) fn is_graphql_rate_limited(message: &str) -> bool {
    crate::gh::is_graphql_rate_limited(message)
}

fn is_graphql_pr_create_integration_blocked(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("graphql")
        && lower.contains("resource not accessible by integration")
        && lower.contains("createpullrequest")
}

fn is_integration_blocked(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("resource not accessible by integration")
}

fn report_graphql_pr_create_fallback_with_client(client: &GhClient, message: &str, cwd: &Path) {
    if is_graphql_rate_limited(message) {
        report_rate_limit_fallback_with_client(client, "gh pr create", cwd);
        return;
    }
    eprintln!(
        "shipyard: GraphQL PR creation is unavailable for this GitHub identity. Falling back to REST."
    );
}

/// Print a one-line user-facing notice that GraphQL is exhausted and
/// the operation is falling back to REST. Best-effort: probes
/// `gh api rate_limit` (a separate quota that does not consume
/// GraphQL budget) to surface the reset time; silently omits the
/// time if the probe fails (network glitch, gh not on PATH, etc.).
///
/// Issue #266: prior to v0.56.x the fallback happened silently so
/// users couldn't tell GraphQL had bailed. Surfacing this on stderr
/// keeps the operation succeeding while making the cost visible.
pub(crate) fn report_rate_limit_fallback_with_client(
    client: &GhClient,
    operation: &str,
    cwd: &Path,
) {
    let reset_suffix = fetch_graphql_reset_unix(client, cwd)
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map(|dt| format!("; reset at {} UTC", dt.format("%H:%M:%S")))
        .unwrap_or_default();
    eprintln!(
        "shipyard: GraphQL rate limit hit for {operation}{reset_suffix}. Falling back to REST."
    );
}

/// Probe `gh api rate_limit --jq .resources.graphql.reset` for the
/// unix timestamp at which the GraphQL bucket refills. Returns None
/// if the probe fails for any reason — caller falls back to omitting
/// the reset time from its user-facing message.
///
/// `rate_limit` is in its own GitHub API quota bucket (the "shared"
/// REST core bucket), so this probe does NOT itself consume GraphQL
/// budget and is safe to call from inside a GraphQL-rate-limited
/// recovery path.
fn fetch_graphql_reset_unix(client: &GhClient, cwd: &Path) -> Option<i64> {
    let output = gh(client, cwd, None)
        .ok()?
        .args(["api", "rate_limit", "--jq", ".resources.graphql.reset"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim().parse::<i64>().ok()
}

fn parse_pr_rest_list(text: &str) -> Result<Option<PrInfo>, PrError> {
    let value = serde_json::from_str::<Value>(text)
        .map_err(|error| PrError::new(format!("failed to parse REST PR list JSON: {error}")))?;
    let Some(items) = value.as_array() else {
        return Err(PrError::new("REST PR list JSON was not an array"));
    };
    items.first().map(parse_pr_rest_value).transpose()
}

fn parse_pr_rest_info(text: &str) -> Result<PrInfo, PrError> {
    let value = serde_json::from_str::<Value>(text)
        .map_err(|error| PrError::new(format!("failed to parse REST PR JSON: {error}")))?;
    parse_pr_rest_value(&value)
}

fn parse_pr_rest_value(value: &Value) -> Result<PrInfo, PrError> {
    Ok(PrInfo {
        number: value
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| PrError::new("REST PR JSON missing number"))?,
        url: string_field(value, "html_url")?,
        title: string_field(value, "title")?,
        state: string_field(value, "state")?.to_ascii_uppercase(),
        branch: nested_string_field(value, &["head", "ref"])?,
        base: nested_string_field(value, &["base", "ref"])?,
    })
}

fn parse_pr_info(text: &str) -> Result<PrInfo, PrError> {
    let value = serde_json::from_str::<Value>(text)
        .map_err(|error| PrError::new(format!("failed to parse gh pr view JSON: {error}")))?;
    parse_pr_value(&value)
}

fn parse_pr_value(value: &Value) -> Result<PrInfo, PrError> {
    Ok(PrInfo {
        number: value
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| PrError::new("PR JSON missing number"))?,
        url: string_field(value, "url")?,
        title: string_field(value, "title")?,
        state: string_field(value, "state")?,
        branch: string_field(value, "headRefName")?,
        base: string_field(value, "baseRefName")?,
    })
}

fn string_field(value: &Value, field: &str) -> Result<String, PrError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| PrError::new(format!("PR JSON missing {field}")))
}

fn nested_string_field(value: &Value, path: &[&str]) -> Result<String, PrError> {
    let mut current = value;
    for field in path {
        current = current
            .get(field)
            .ok_or_else(|| PrError::new(format!("REST PR JSON missing {}", path.join("."))))?;
    }
    current
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| PrError::new(format!("REST PR JSON missing {}", path.join("."))))
}

#[cfg(test)]
mod tests {
    use super::{
        PrInfo, is_graphql_pr_create_integration_blocked, is_graphql_rate_limited,
        is_integration_blocked, parse_github_remote_slug, parse_pr_info, parse_pr_list,
        parse_pr_rest_info, parse_pr_rest_list, selector_pr_number, url_encode,
    };

    #[test]
    fn parses_pr_view_payload() {
        let info = parse_pr_info(
            r#"{
                "number": 42,
                "url": "https://github.com/o/r/pull/42",
                "title": "Fix thing",
                "state": "OPEN",
                "headRefName": "feature/test",
                "baseRefName": "main"
            }"#,
        )
        .expect("pr info");

        assert_eq!(
            info,
            PrInfo {
                number: 42,
                url: "https://github.com/o/r/pull/42".to_owned(),
                title: "Fix thing".to_owned(),
                state: "OPEN".to_owned(),
                branch: "feature/test".to_owned(),
                base: "main".to_owned(),
            }
        );
    }

    #[test]
    fn parses_empty_and_non_empty_pr_lists() {
        assert_eq!(parse_pr_list("[]").expect("empty"), None);
        assert_eq!(
            parse_pr_list(
                r#"[{
                    "number": 7,
                    "url": "https://github.com/o/r/pull/7",
                    "title": "Ship it",
                    "state": "OPEN",
                    "headRefName": "feature/a",
                    "baseRefName": "develop"
                }]"#,
            )
            .expect("list")
            .expect("first")
            .number,
            7
        );
    }

    #[test]
    fn detects_graphql_rate_limit_errors() {
        assert!(is_graphql_rate_limited(
            "GraphQL: API rate limit already exceeded for user ID 123"
        ));
        assert!(!is_graphql_rate_limited("HTTP 500: something else failed"));
    }

    #[test]
    fn detects_graphql_pr_create_app_integration_block() {
        assert!(is_graphql_pr_create_integration_blocked(
            "pull request create failed: GraphQL: Resource not accessible by integration (createPullRequest)"
        ));
        assert!(!is_graphql_pr_create_integration_blocked(
            "GraphQL: Resource not accessible by integration (mergePullRequest)"
        ));
        assert!(!is_graphql_pr_create_integration_blocked(
            "REST: Resource not accessible by integration (createPullRequest)"
        ));
        assert!(is_integration_blocked(
            "gh: Resource not accessible by integration (HTTP 403)"
        ));
    }

    #[test]
    fn url_encode_escapes_query_string_specials() {
        // `:` and `/` appear in `owner:branch` and `refs/heads/...` query
        // values; both must survive in the URL or the GET hits the wrong
        // endpoint. Issue #282 saw `gh api repos/.../pulls -f head=owner:branch`
        // get turned into POST → 422 missing base. Forcing GET + encoding
        // the value keeps it a real GET.
        assert_eq!(url_encode("owner:branch"), "owner%3Abranch");
        assert_eq!(url_encode("refs/heads/main"), "refs%2Fheads%2Fmain");
        assert_eq!(
            url_encode("danielraffel:feat/298-waitpr-rest-fallback"),
            "danielraffel%3Afeat%2F298-waitpr-rest-fallback"
        );
        // Letters / digits / unreserved punctuation pass through unchanged.
        assert_eq!(url_encode("AbC-_.~123"), "AbC-_.~123");
    }

    #[test]
    fn parses_github_remote_slugs() {
        assert_eq!(
            parse_github_remote_slug("git@github.com:danielraffel/Shipyard.git"),
            Some("danielraffel/Shipyard".to_owned())
        );
        assert_eq!(
            parse_github_remote_slug("https://github.com/danielraffel/pulp"),
            Some("danielraffel/pulp".to_owned())
        );
        assert_eq!(parse_github_remote_slug("https://example.com/nope"), None);
    }

    #[test]
    fn parses_pr_numbers_from_urls_or_plain_selectors() {
        assert_eq!(
            selector_pr_number("https://github.com/danielraffel/Shipyard/pull/273"),
            Some(273)
        );
        assert_eq!(selector_pr_number("274"), Some(274));
        assert_eq!(selector_pr_number("not-a-pr"), None);
    }

    #[test]
    fn parses_rest_pr_payloads() {
        let info = parse_pr_rest_info(
            r#"{
                "number": 273,
                "html_url": "https://github.com/danielraffel/Shipyard/pull/273",
                "title": "REST fallback",
                "state": "open",
                "head": {"ref": "feature/rest-fallback"},
                "base": {"ref": "main"}
            }"#,
        )
        .expect("rest pr");

        assert_eq!(
            info,
            PrInfo {
                number: 273,
                url: "https://github.com/danielraffel/Shipyard/pull/273".to_owned(),
                title: "REST fallback".to_owned(),
                state: "OPEN".to_owned(),
                branch: "feature/rest-fallback".to_owned(),
                base: "main".to_owned(),
            }
        );
        assert_eq!(parse_pr_rest_list("[]").expect("empty"), None);
        assert_eq!(
            parse_pr_rest_list(
                r#"[{
                    "number": 274,
                    "html_url": "https://github.com/o/r/pull/274",
                    "title": "Focus profile",
                    "state": "open",
                    "head": {"ref": "feature/focus"},
                    "base": {"ref": "main"}
                }]"#
            )
            .expect("list")
            .expect("first")
            .number,
            274
        );
    }
}
