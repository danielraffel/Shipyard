use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::Value;

use super::{CliFailure, failure, gh_json, output_detail, prepared_gh, require_success};
use crate::dependency::{PulpDependencyConfig, PulpDependencyLock, sha256_hex};
use crate::gh::{GhClient, parse_github_remote_slug};

pub(super) fn consumer_repo_slug(
    client: &GhClient,
    repo_root: &Path,
) -> Result<String, CliFailure> {
    let output = client
        .prepare_privileged_git_command(repo_root)
        .map_err(|error| failure(error.to_string()))?
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .map_err(|error| failure(format!("failed to inspect origin remote: {error}")))?;
    require_success(&output, "git config remote.origin.url")?;
    let remote = String::from_utf8_lossy(&output.stdout);
    parse_github_remote_slug(remote.trim())
        .ok_or_else(|| failure("remote.origin.url is not a supported GitHub repository"))
}

pub(super) fn ensure_clean(client: &GhClient, repo_root: &Path) -> Result<(), CliFailure> {
    let output = client
        .prepare_privileged_git_command(repo_root)
        .map_err(|error| failure(error.to_string()))?
        .args(["status", "--porcelain=v1"])
        .output()
        .map_err(|error| failure(format!("failed to inspect consumer worktree: {error}")))?;
    require_success(&output, "git status")?;
    if !output.stdout.is_empty() {
        return Err(failure(
            "consumer worktree must be clean before Shipyard creates a dependency pin PR",
        ));
    }
    Ok(())
}

pub(super) fn fetch_base(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    base: &str,
) -> Result<String, CliFailure> {
    let sha = github_branch_sha(client, cwd, repo, base)?;
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(failure("consumer base did not resolve to a full Git SHA"));
    }
    Ok(sha.to_ascii_lowercase())
}

pub(super) struct TemporaryWorktree {
    _parent: tempfile::TempDir,
    checkout: PathBuf,
}

impl TemporaryWorktree {
    pub(super) fn create(
        client: &GhClient,
        repo: &str,
        base: &str,
        expected_sha: &str,
    ) -> Result<Self, CliFailure> {
        let parent = tempfile::tempdir().map_err(|error| {
            failure(format!("failed to create temporary worktree root: {error}"))
        })?;
        let checkout = parent.path().join("checkout");
        fs::create_dir(&checkout).map_err(|error| {
            failure(format!(
                "failed to create isolated dependency checkout: {error}"
            ))
        })?;
        let output = client
            .prepare_privileged_git_command(&checkout)
            .map_err(|error| failure(error.to_string()))?
            .args(["init", "--quiet", "--initial-branch=shipyard-dependency"])
            .output()
            .map_err(|error| failure(format!("failed to initialize isolated checkout: {error}")))?;
        require_success(&output, "trusted git init")?;

        let url = format!("https://github.com/{repo}.git");
        let refspec = format!("+refs/heads/{base}:refs/shipyard/dependency-base");
        let output = client
            .prepare_git_command(&checkout)
            .map_err(|error| failure(error.to_string()))?
            .args(["fetch", "--no-tags", &url, &refspec])
            .output()
            .map_err(|error| failure(format!("failed to fetch consumer base: {error}")))?;
        require_success(&output, "isolated GitHub App consumer base fetch")?;
        let fetched = trusted_git_output(
            client,
            &checkout,
            [
                "rev-parse",
                "--verify",
                "refs/shipyard/dependency-base^{commit}",
            ],
            "isolated consumer base",
        )?;
        if !fetched.trim().eq_ignore_ascii_case(expected_sha) {
            return Err(failure(format!(
                "consumer base moved from {expected_sha} to {} during isolated fetch",
                fetched.trim()
            )));
        }
        let output = client
            .prepare_privileged_git_command(&checkout)
            .map_err(|error| failure(error.to_string()))?
            .args(["checkout", "--quiet", "--detach", expected_sha])
            .output()
            .map_err(|error| failure(format!("failed to check out consumer base: {error}")))?;
        require_success(&output, "trusted git checkout")?;
        Ok(Self {
            _parent: parent,
            checkout,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.checkout
    }
}

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| error.to_string())?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("failed to stage {}: {error}", path.display()))?;
    temporary
        .write_all(bytes)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| format!("failed to persist {}: {}", path.display(), error.error))?;
    Ok(())
}

pub(super) fn dependency_branch(
    tag: &str,
    commit_sha: &str,
    consumer_base_sha: &str,
    lock_bytes: &[u8],
) -> String {
    let lock_digest = sha256_hex(lock_bytes);
    format!(
        "shipyard/pulp-{}-{}-{}-{}",
        tag.trim_start_matches('v'),
        &commit_sha[..12],
        &consumer_base_sha[..12],
        &lock_digest[..12]
    )
}

#[derive(Clone, Debug)]
pub(super) struct PinPr {
    pub(super) number: u64,
    pub(super) url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct GitHubAppIdentity {
    pub(super) login: String,
    pub(super) database_id: u64,
}

pub(super) struct PinPublication<'a> {
    pub(super) client: &'a GhClient,
    pub(super) cwd: &'a Path,
    pub(super) repo: &'a str,
    pub(super) config: &'a PulpDependencyConfig,
    pub(super) lock: &'a PulpDependencyLock,
    pub(super) branch: &'a str,
    pub(super) lock_bytes: &'a [u8],
    pub(super) base_sha: &'a str,
    pub(super) app: &'a GitHubAppIdentity,
}

pub(super) fn github_app_identity(
    client: &GhClient,
    cwd: &Path,
) -> Result<GitHubAppIdentity, CliFailure> {
    let value: Value = gh_json(
        client,
        cwd,
        [
            "api",
            "graphql",
            "-f",
            "query=query { viewer { login databaseId } }",
        ],
    )?;
    parse_github_app_identity(&value)
}

pub(super) fn parse_github_app_identity(value: &Value) -> Result<GitHubAppIdentity, CliFailure> {
    let login = value
        .pointer("/data/viewer/login")
        .and_then(Value::as_str)
        .filter(|login| login.ends_with("[bot]") && login.len() > "[bot]".len())
        .ok_or_else(|| failure("GitHub App token viewer is not an App bot identity"))?;
    let database_id = value
        .pointer("/data/viewer/databaseId")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| failure("GitHub App token viewer has no database id"))?;
    Ok(GitHubAppIdentity {
        login: login.to_owned(),
        database_id,
    })
}

pub(super) enum ExistingPin {
    Absent,
    Open(PinPr),
}

pub(super) fn existing_pin_pr(pin: &PinPublication<'_>) -> Result<ExistingPin, CliFailure> {
    let client = pin.client;
    let cwd = pin.cwd;
    let repo = pin.repo;
    let config = pin.config;
    let branch = pin.branch;
    let lock_bytes = pin.lock_bytes;
    let endpoint = format!(
        "repos/{repo}/contents/{}",
        config.lock_file.to_string_lossy()
    );
    let mut command = prepared_gh(client, cwd)?;
    command.args([
        "api",
        "--method",
        "GET",
        &endpoint,
        "-f",
        &format!("ref={branch}"),
    ]);
    let output = command
        .output()
        .map_err(|error| failure(format!("failed to inspect dependency branch: {error}")))?;
    if !output.status.success() {
        let detail = output_detail(&output);
        if detail.contains("HTTP 404") || detail.contains("Not Found") {
            return Ok(ExistingPin::Absent);
        }
        return Err(failure(format!(
            "dependency branch inspection failed: {detail}"
        )));
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| failure(format!("dependency branch returned invalid JSON: {error}")))?;
    let encoded = value
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| failure("dependency branch lock response has no content"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.replace('\n', ""))
        .map_err(|error| {
            failure(format!(
                "dependency branch lock is not valid base64: {error}"
            ))
        })?;
    if bytes != lock_bytes {
        return Err(failure(format!(
            "existing branch {branch} contains a different dependency lock; refusing to overwrite it"
        )));
    }
    let head_sha = validate_dependency_branch_envelope(pin)?;
    let owner = repo.split_once('/').map_or(repo, |(owner, _)| owner);
    let endpoint = format!("repos/{repo}/pulls");
    let pulls: Vec<Value> = gh_json(
        client,
        cwd,
        [
            "api",
            "--method",
            "GET",
            &endpoint,
            "-f",
            "state=open",
            "-f",
            &format!("head={owner}:{branch}"),
            "-f",
            &format!("base={}", config.base_branch),
        ],
    )?;
    let [pull] = pulls.as_slice() else {
        return Err(failure(if pulls.is_empty() {
            format!(
                "existing dependency branch {branch} has no trusted App-authored pull request; refusing to adopt it (delete the orphan branch and rerun)"
            )
        } else {
            format!("multiple open pull requests claim dependency branch {branch}")
        }));
    };
    validate_pin_pr(pull, pin, &head_sha)?;
    Ok(ExistingPin::Open(parse_pin_pr(pull)?))
}

fn validate_dependency_branch_envelope(pin: &PinPublication<'_>) -> Result<String, CliFailure> {
    let client = pin.client;
    let cwd = pin.cwd;
    let repo = pin.repo;
    let config = pin.config;
    let lock = pin.lock;
    let branch = pin.branch;
    let lock_bytes = pin.lock_bytes;
    let base_sha = pin.base_sha;
    let app = pin.app;
    let head_sha = github_branch_sha(client, cwd, repo, branch)?;
    let commit_endpoint = format!("repos/{repo}/commits/{head_sha}");
    let commit: Value = gh_json(client, cwd, ["api", &commit_endpoint])?;
    let parents = commit
        .get("parents")
        .and_then(Value::as_array)
        .ok_or_else(|| failure("dependency branch commit has no parent list"))?;
    let exact_parent =
        parents.len() == 1 && parents[0].get("sha").and_then(Value::as_str) == Some(base_sha);
    let expected_message = pin_commit_message(&lock.tag);
    if commit.get("sha").and_then(Value::as_str) != Some(head_sha.as_str())
        || !exact_parent
        || commit.pointer("/commit/message").and_then(Value::as_str)
            != Some(expected_message.as_str())
    {
        return Err(failure(format!(
            "existing dependency branch {branch} is not one exact pin commit on qualified base {base_sha}"
        )));
    }
    validate_app_actor(commit.get("author"), app, "dependency commit author")?;
    validate_app_actor(commit.get("committer"), app, "dependency commit committer")?;

    let expected_blob = git_blob_sha(client, cwd, lock_bytes)?;
    let tree_sha = commit
        .pointer("/commit/tree/sha")
        .and_then(Value::as_str)
        .ok_or_else(|| failure("dependency branch commit has no tree SHA"))?;
    validate_lock_tree_entry(
        client,
        cwd,
        repo,
        tree_sha,
        &config.lock_file,
        &expected_blob,
    )?;

    let endpoint = format!("repos/{repo}/compare/{base_sha}...{head_sha}");
    let comparison: Value = gh_json(client, cwd, ["api", &endpoint])?;
    let files = comparison
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| failure("GitHub compare response has no file list"))?;
    let expected = config.lock_file.to_string_lossy();
    let exact_lock_diff = files.len() == 1
        && files[0].get("filename").and_then(Value::as_str) == Some(expected.as_ref())
        && files[0].get("sha").and_then(Value::as_str) == Some(expected_blob.as_str())
        && matches!(
            files[0].get("status").and_then(Value::as_str),
            Some("added" | "modified")
        );
    let exact_comparison = comparison.get("ahead_by").and_then(Value::as_u64) == Some(1)
        && comparison.get("total_commits").and_then(Value::as_u64) == Some(1)
        && comparison
            .pointer("/base_commit/sha")
            .and_then(Value::as_str)
            == Some(base_sha)
        && comparison
            .pointer("/merge_base_commit/sha")
            .and_then(Value::as_str)
            == Some(base_sha);
    if !exact_lock_diff || !exact_comparison {
        return Err(failure(format!(
            "existing branch {branch} is not an exact one-commit lock diff from the qualified base"
        )));
    }
    Ok(head_sha)
}

fn validate_app_actor(
    value: Option<&Value>,
    app: &GitHubAppIdentity,
    label: &str,
) -> Result<(), CliFailure> {
    let matches = value.is_some_and(|value| {
        value.get("login").and_then(Value::as_str) == Some(app.login.as_str())
            && value.get("id").and_then(Value::as_u64) == Some(app.database_id)
            && value.get("type").and_then(Value::as_str) == Some("Bot")
    });
    if !matches {
        return Err(failure(format!(
            "{label} is not the pinned GitHub App actor {} ({})",
            app.login, app.database_id
        )));
    }
    Ok(())
}

fn git_blob_sha(client: &GhClient, cwd: &Path, bytes: &[u8]) -> Result<String, CliFailure> {
    let mut child = client
        .prepare_privileged_git_command(cwd)
        .map_err(|error| failure(error.to_string()))?
        .args(["hash-object", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| failure(format!("failed to start git hash-object: {error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| failure("git hash-object stdin was not available"))?
        .write_all(bytes)
        .map_err(|error| failure(format!("failed to hash dependency lock: {error}")))?;
    let output = child
        .wait_with_output()
        .map_err(|error| failure(format!("failed to wait for git hash-object: {error}")))?;
    require_success(&output, "git hash-object dependency lock")?;
    let sha = String::from_utf8(output.stdout)
        .map_err(|_| failure("dependency lock Git blob SHA is not valid UTF-8"))?;
    let sha = sha.trim();
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(failure(
            "dependency lock did not produce a full Git blob SHA",
        ));
    }
    Ok(sha.to_ascii_lowercase())
}

fn validate_lock_tree_entry(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    root_tree_sha: &str,
    lock_file: &Path,
    expected_blob_sha: &str,
) -> Result<(), CliFailure> {
    let components: Vec<_> = lock_file
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let mut tree_sha = root_tree_sha.to_owned();
    for (index, component) in components.iter().enumerate() {
        let endpoint = format!("repos/{repo}/git/trees/{tree_sha}");
        let tree: Value = gh_json(client, cwd, ["api", &endpoint])?;
        if tree.get("truncated").and_then(Value::as_bool) == Some(true) {
            return Err(failure("GitHub truncated the dependency commit tree"));
        }
        let entries = tree
            .get("tree")
            .and_then(Value::as_array)
            .ok_or_else(|| failure("dependency commit tree has no entries"))?;
        let entry = entries
            .iter()
            .find(|entry| entry.get("path").and_then(Value::as_str) == Some(component))
            .ok_or_else(|| {
                failure(format!(
                    "dependency commit tree is missing lock path component {component}"
                ))
            })?;
        let last = index + 1 == components.len();
        if last {
            let exact_blob = entry.get("type").and_then(Value::as_str) == Some("blob")
                && entry.get("mode").and_then(Value::as_str) == Some("100644")
                && entry.get("sha").and_then(Value::as_str) == Some(expected_blob_sha);
            if !exact_blob {
                return Err(failure(
                    "dependency lock tree entry is not the exact regular-file blob",
                ));
            }
        } else {
            if entry.get("type").and_then(Value::as_str) != Some("tree")
                || entry.get("mode").and_then(Value::as_str) != Some("040000")
            {
                return Err(failure(format!(
                    "dependency lock parent {component} is not a Git tree"
                )));
            }
            entry
                .get("sha")
                .and_then(Value::as_str)
                .ok_or_else(|| failure("dependency lock parent tree has no SHA"))?
                .clone_into(&mut tree_sha);
        }
    }
    Ok(())
}

fn github_branch_sha(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    branch: &str,
) -> Result<String, CliFailure> {
    let endpoint = format!("repos/{repo}/git/ref/heads/{branch}");
    let reference: Value = gh_json(client, cwd, ["api", &endpoint])?;
    reference
        .pointer("/object/sha")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| failure("GitHub branch ref has no commit SHA"))
}

pub(super) fn ensure_base_unchanged(
    client: &GhClient,
    cwd: &Path,
    repo: &str,
    branch: &str,
    expected_sha: &str,
) -> Result<(), CliFailure> {
    let current_sha = github_branch_sha(client, cwd, repo, branch)?;
    if !current_sha.eq_ignore_ascii_case(expected_sha) {
        return Err(failure(format!(
            "consumer base {branch} moved from {expected_sha} to {current_sha} during dependency qualification; rerun from the new reviewed base"
        )));
    }
    Ok(())
}

pub(super) fn commit_lock(
    client: &GhClient,
    checkout: &Path,
    lock_file: &Path,
    tag: &str,
    expected_lock: &[u8],
    expected_parent: &str,
    app: &GitHubAppIdentity,
) -> Result<(), CliFailure> {
    let add = client
        .prepare_privileged_git_command(checkout)
        .map_err(|error| failure(error.to_string()))?
        .arg("add")
        .arg("--")
        .arg(lock_file)
        .output()
        .map_err(|error| failure(format!("failed to stage dependency lock: {error}")))?;
    require_success(&add, "git add dependency lock")?;
    let message = pin_commit_message(tag);
    let email = app_noreply_email(app);
    let commit = client
        .prepare_privileged_git_command(checkout)
        .map_err(|error| failure(error.to_string()))?
        .args([
            "-c",
            &format!("user.name={}", app.login),
            "-c",
            &format!("user.email={email}"),
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--no-verify",
            "-m",
            &message,
        ])
        .output()
        .map_err(|error| failure(format!("failed to commit dependency lock: {error}")))?;
    require_success(&commit, "git commit dependency lock")?;
    validate_dependency_commit(
        client,
        checkout,
        lock_file,
        expected_lock,
        expected_parent,
        app,
    )
}

fn pin_commit_message(tag: &str) -> String {
    format!("chore(deps): pin Pulp {tag}")
}

fn app_noreply_email(app: &GitHubAppIdentity) -> String {
    format!("{}+{}@users.noreply.github.com", app.database_id, app.login)
}

fn validate_dependency_commit(
    client: &GhClient,
    checkout: &Path,
    lock_file: &Path,
    expected_lock: &[u8],
    expected_parent: &str,
    app: &GitHubAppIdentity,
) -> Result<(), CliFailure> {
    let parent = trusted_git_output(
        client,
        checkout,
        ["rev-parse", "HEAD^"],
        "dependency commit parent",
    )?;
    if !parent.trim().eq_ignore_ascii_case(expected_parent) {
        return Err(failure(
            "dependency commit is not based on the qualified consumer base",
        ));
    }
    let names = trusted_git_output(
        client,
        checkout,
        ["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
        "dependency commit file list",
    )?;
    let expected_name = lock_file.to_string_lossy();
    if names.lines().collect::<Vec<_>>() != [expected_name.as_ref()] {
        return Err(failure(
            "dependency commit changes files beyond the configured lock",
        ));
    }
    let object = format!("HEAD:{}", lock_file.to_string_lossy());
    let bytes = trusted_git_output_bytes(
        client,
        checkout,
        ["show", "--format=", "--no-textconv", &object],
        "committed dependency lock",
    )?;
    if bytes != expected_lock {
        return Err(failure(
            "committed dependency lock bytes differ from the qualified lock",
        ));
    }
    let identity = trusted_git_output(
        client,
        checkout,
        ["show", "-s", "--format=%an%x00%ae%x00%cn%x00%ce", "HEAD"],
        "dependency commit identity",
    )?;
    let fields: Vec<_> = identity.trim().split('\0').collect();
    let expected_email = app_noreply_email(app);
    if fields
        != [
            app.login.as_str(),
            expected_email.as_str(),
            app.login.as_str(),
            expected_email.as_str(),
        ]
    {
        return Err(failure(
            "dependency commit author or committer differs from the pinned GitHub App actor",
        ));
    }
    let status = trusted_git_output(
        client,
        checkout,
        ["status", "--porcelain=v1"],
        "dependency worktree status",
    )?;
    if !status.is_empty() {
        return Err(failure(
            "dependency worktree changed outside the verified commit",
        ));
    }
    Ok(())
}

fn trusted_git_output<const N: usize>(
    client: &GhClient,
    checkout: &Path,
    args: [&str; N],
    operation: &str,
) -> Result<String, CliFailure> {
    let bytes = trusted_git_output_bytes(client, checkout, args, operation)?;
    String::from_utf8(bytes).map_err(|_| failure(format!("{operation} is not valid UTF-8")))
}

fn trusted_git_output_bytes<I, S>(
    client: &GhClient,
    checkout: &Path,
    args: I,
    operation: &str,
) -> Result<Vec<u8>, CliFailure>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = client
        .prepare_privileged_git_command(checkout)
        .map_err(|error| failure(error.to_string()))?
        .args(args)
        .output()
        .map_err(|error| failure(format!("failed to inspect {operation}: {error}")))?;
    require_success(&output, operation)?;
    Ok(output.stdout)
}

pub(super) fn push_head(
    client: &GhClient,
    checkout: &Path,
    repo: &str,
    branch: &str,
) -> Result<(), CliFailure> {
    let mut command = client
        .prepare_git_command(checkout)
        .map_err(|error| failure(error.to_string()))?;
    let url = format!("https://github.com/{repo}.git");
    let refspec = format!("HEAD:refs/heads/{branch}");
    let lease = new_branch_lease(branch);
    command.args([
        "-c",
        "core.hooksPath=/dev/null",
        "push",
        "--no-verify",
        &lease,
        &url,
        &refspec,
    ]);
    let output = command
        .output()
        .map_err(|error| failure(format!("failed to push dependency branch: {error}")))?;
    require_success(&output, "GitHub App dependency branch push")
}

pub(super) fn new_branch_lease(branch: &str) -> String {
    format!("--force-with-lease=refs/heads/{branch}:")
}

pub(super) fn create_pin_pr(pin: &PinPublication<'_>) -> Result<PinPr, CliFailure> {
    let client = pin.client;
    let cwd = pin.cwd;
    let repo = pin.repo;
    let config = pin.config;
    let lock = pin.lock;
    let branch = pin.branch;
    let base_sha = pin.base_sha;
    let head_sha = validate_dependency_branch_envelope(pin)?;
    let endpoint = format!("repos/{repo}/pulls");
    let title = pin_pr_title(lock);
    let body = pin_pr_body(lock, base_sha);
    let value: Value = gh_json(
        client,
        cwd,
        [
            "api",
            "-X",
            "POST",
            &endpoint,
            "-f",
            &format!("title={title}"),
            "-f",
            &format!("head={branch}"),
            "-f",
            &format!("base={}", config.base_branch),
            "-f",
            &format!("body={body}"),
        ],
    )?;
    validate_pin_pr(&value, pin, &head_sha)?;
    parse_pin_pr(&value)
}

pub(super) fn validate_pin_pr(
    value: &Value,
    pin: &PinPublication<'_>,
    head_sha: &str,
) -> Result<(), CliFailure> {
    let repo = pin.repo;
    let config = pin.config;
    let lock = pin.lock;
    let branch = pin.branch;
    let base_sha = pin.base_sha;
    let app = pin.app;
    validate_app_actor(value.get("user"), app, "dependency pull request author")?;
    let expected_title = pin_pr_title(lock);
    let expected_body = pin_pr_body(lock, base_sha);
    let exact = value.get("state").and_then(Value::as_str) == Some("open")
        && value.get("draft").and_then(Value::as_bool) == Some(false)
        && value.get("title").and_then(Value::as_str) == Some(expected_title.as_str())
        && value.get("body").and_then(Value::as_str) == Some(expected_body.as_str())
        && value.pointer("/head/ref").and_then(Value::as_str) == Some(branch)
        && value.pointer("/head/sha").and_then(Value::as_str) == Some(head_sha)
        && value
            .pointer("/head/repo/full_name")
            .and_then(Value::as_str)
            .is_some_and(|actual| actual.eq_ignore_ascii_case(repo))
        && value.pointer("/base/ref").and_then(Value::as_str) == Some(config.base_branch.as_str())
        && value.pointer("/base/sha").and_then(Value::as_str) == Some(base_sha);
    if !exact {
        return Err(failure(
            "dependency pull request does not match the exact App-authored pin envelope",
        ));
    }
    Ok(())
}

pub(super) fn pin_pr_title(lock: &PulpDependencyLock) -> String {
    format!("chore(deps): pin Pulp {}", lock.tag)
}

fn parse_pin_pr(value: &Value) -> Result<PinPr, CliFailure> {
    Ok(PinPr {
        number: value
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| failure("GitHub pull request response has no number"))?,
        url: value
            .get("html_url")
            .and_then(Value::as_str)
            .ok_or_else(|| failure("GitHub pull request response has no html_url"))?
            .to_owned(),
    })
}

pub(super) fn pin_pr_body(lock: &PulpDependencyLock, qualified_base_sha: &str) -> String {
    let mut body = format!(
        "Shipyard qualified and materialized an immutable Pulp dependency pin.\n\n- channel: `{}`\n- qualified consumer base: `{qualified_base_sha}`\n- tag: `{}`\n- tag object: `{}`\n- peeled commit: `{}`\n- release id: `{}`\n- manifest SHA-256: `{}`\n- release-attestation statement SHA-256: `{}`\n",
        lock.channel.as_str(),
        lock.tag,
        lock.tag_ref_sha,
        lock.commit_sha,
        lock.release_id,
        lock.manifest.sha256,
        lock.release_attestation.statement_sha256,
    );
    for receipt in &lock.build_attestations {
        let _ = writeln!(
            body,
            "- `{}`: asset SHA-256 `{}`, provenance statement SHA-256 `{}`",
            receipt.asset, receipt.subject_sha256, receipt.statement_sha256
        );
    }
    body.push_str(
        "\nConsumer CI must run `shipyard dependency pulp verify` before merge. The consumer build remains authoritative for verifying the downloaded SDK bytes and its embedded `sdk-provenance.json` against this exact lock; Shipyard's machine cache is only a polling optimization.\n",
    );
    body
}
