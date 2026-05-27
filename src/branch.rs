//! Remote branch creation plus branch-protection application.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

use crate::governance::{BranchProtectionRules, GovernanceGh, put_branch_protection};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_PUSH_TIMEOUT: Duration = Duration::from_mins(1);

/// Outcome status for `shipyard branch apply`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BranchApplyStatus {
    /// The branch was created on origin.
    Created,
    /// The branch already existed on origin.
    AlreadyExists,
    /// Governance rules were applied to the branch.
    RulesApplied,
    /// Branch creation succeeded or was unnecessary, but rule application failed.
    RulesFailed,
    /// A git operation failed before rules could be applied.
    GitFailed,
}

impl BranchApplyStatus {
    /// Python-compatible status string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::AlreadyExists => "already_exists",
            Self::RulesApplied => "rules_applied",
            Self::RulesFailed => "rules_failed",
            Self::GitFailed => "git_failed",
        }
    }
}

/// Result returned by branch creation/application flows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchApplyResult {
    /// Branch that was targeted.
    pub branch: String,
    /// Final operation status.
    pub status: BranchApplyStatus,
    /// Human-readable detail suitable for CLI output.
    pub message: String,
}

impl BranchApplyResult {
    /// Whether the operation reached an acceptable terminal state.
    #[must_use]
    pub const fn ok(&self) -> bool {
        matches!(
            self.status,
            BranchApplyStatus::Created | BranchApplyStatus::RulesApplied
        )
    }
}

/// Create `branch` on `origin` from the remote SHA of `base_branch`.
///
/// The base SHA is resolved via `git ls-remote`, not local tracking refs, so
/// shallow/single-branch clones and stale worktrees cannot create the branch at
/// the wrong commit.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn create_branch_on_remote(
    cwd: &Path,
    branch: &str,
    base_branch: &str,
    git_command: Option<&Path>,
) -> BranchApplyResult {
    let check = run_git(
        cwd,
        git_command,
        &["ls-remote", "--exit-code", "--heads", "origin", branch],
        GIT_TIMEOUT,
    );
    match check {
        Ok(output) if output.success() => {
            return BranchApplyResult {
                branch: branch.to_owned(),
                status: BranchApplyStatus::AlreadyExists,
                message: format!("Branch '{branch}' already exists on origin"),
            };
        }
        Ok(output) if output.code == Some(2) => {}
        Ok(output) => {
            return BranchApplyResult {
                branch: branch.to_owned(),
                status: BranchApplyStatus::GitFailed,
                message: format!(
                    "ls-remote failed for {branch}: {}",
                    output.detail_or("no detail")
                ),
            };
        }
        Err(error) => {
            return BranchApplyResult {
                branch: branch.to_owned(),
                status: BranchApplyStatus::GitFailed,
                message: format!("ls-remote failed for {branch}: {error}"),
            };
        }
    }

    let base_ref = format!("refs/heads/{base_branch}");
    let base_lookup = match run_git(
        cwd,
        git_command,
        &["ls-remote", "--exit-code", "origin", &base_ref],
        GIT_TIMEOUT,
    ) {
        Ok(output) if output.success() => output,
        Ok(output) => {
            return BranchApplyResult {
                branch: branch.to_owned(),
                status: BranchApplyStatus::GitFailed,
                message: format!(
                    "ls-remote failed to resolve base branch '{base_branch}': {}",
                    output.detail_or("no detail")
                ),
            };
        }
        Err(error) => {
            return BranchApplyResult {
                branch: branch.to_owned(),
                status: BranchApplyStatus::GitFailed,
                message: format!(
                    "ls-remote failed to resolve base branch '{base_branch}': {error}"
                ),
            };
        }
    };

    let Some(base_sha) = first_sha(&base_lookup.stdout) else {
        return BranchApplyResult {
            branch: branch.to_owned(),
            status: BranchApplyStatus::GitFailed,
            message: format!(
                "ls-remote returned no SHA for origin/{base_branch} - does the base branch exist on the remote?"
            ),
        };
    };

    let refspec = format!("{base_sha}:refs/heads/{branch}");
    let push = match run_git(
        cwd,
        git_command,
        &["push", "origin", &refspec],
        GIT_PUSH_TIMEOUT,
    ) {
        Ok(output) if output.success() => output,
        Ok(output) => {
            return BranchApplyResult {
                branch: branch.to_owned(),
                status: BranchApplyStatus::GitFailed,
                message: format!(
                    "git push failed creating {branch} from {base_branch} ({}): {}",
                    short_sha(&base_sha),
                    output.detail_or("no detail")
                ),
            };
        }
        Err(error) => {
            return BranchApplyResult {
                branch: branch.to_owned(),
                status: BranchApplyStatus::GitFailed,
                message: format!(
                    "git push failed creating {branch} from {base_branch} ({}): {error}",
                    short_sha(&base_sha)
                ),
            };
        }
    };
    debug_assert!(push.success());

    BranchApplyResult {
        branch: branch.to_owned(),
        status: BranchApplyStatus::Created,
        message: format!(
            "Created '{branch}' from '{base_branch}' ({}) on origin",
            short_sha(&base_sha)
        ),
    }
}

/// Create the branch when missing, then apply its governance rules.
#[must_use]
pub fn create_branch_and_apply_rules(
    cwd: &Path,
    repo: &str,
    branch: &str,
    base_branch: &str,
    rules: &BranchProtectionRules,
    git_command: Option<&Path>,
    gh: &GovernanceGh,
) -> BranchApplyResult {
    let create_result = create_branch_on_remote(cwd, branch, base_branch, git_command);
    if create_result.status == BranchApplyStatus::GitFailed {
        return create_result;
    }

    match put_branch_protection(repo, branch, rules, gh) {
        Ok(()) => BranchApplyResult {
            branch: branch.to_owned(),
            status: BranchApplyStatus::RulesApplied,
            message: if create_result.status == BranchApplyStatus::Created {
                format!("Created '{branch}' from '{base_branch}' and applied governance rules")
            } else {
                format!("Branch '{branch}' already existed; reapplied governance rules")
            },
        },
        Err(error) => BranchApplyResult {
            branch: branch.to_owned(),
            status: BranchApplyStatus::RulesFailed,
            message: format!(
                "Branch exists but rule apply failed: {error}. Re-run `shipyard governance apply --branch {branch}` to retry."
            ),
        },
    }
}

/// Apply governance rules to an existing branch.
#[must_use]
pub fn apply_branch_rules(
    repo: &str,
    branch: &str,
    rules: &BranchProtectionRules,
    gh: &GovernanceGh,
) -> BranchApplyResult {
    match put_branch_protection(repo, branch, rules, gh) {
        Ok(()) => BranchApplyResult {
            branch: branch.to_owned(),
            status: BranchApplyStatus::RulesApplied,
            message: format!("Applied governance rules to '{branch}'"),
        },
        Err(error) => BranchApplyResult {
            branch: branch.to_owned(),
            status: BranchApplyStatus::RulesFailed,
            message: format!("Rule apply failed for '{branch}': {error}"),
        },
    }
}

#[derive(Debug)]
struct ShellOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl ShellOutput {
    fn success(&self) -> bool {
        self.code == Some(0)
    }

    fn detail_or<'a>(&'a self, fallback: &'a str) -> &'a str {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr;
        }
        let stdout = self.stdout.trim();
        if !stdout.is_empty() {
            return stdout;
        }
        fallback
    }
}

fn run_git(
    cwd: &Path,
    git_command: Option<&Path>,
    args: &[&str],
    timeout: Duration,
) -> Result<ShellOutput, String> {
    // Supervised git invocation — sets SHIPYARD_PR_RUNNING=1 even when
    // a test injects an alternative `git_command` shim, so coverage of
    // the env-var contract extends through every code path.
    let mut command = crate::supervised::supervised(
        git_command.map_or_else(|| Command::new("git"), Command::new),
    );
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to run git: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture git stdout".to_owned())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture git stderr".to_owned())?;
    let status = child
        .wait_timeout(timeout)
        .map_err(|error| format!("failed waiting for git: {error}"))?
        .ok_or_else(|| {
            let _ = child.kill();
            let _ = child.wait();
            format!("timed out after {}s", timeout.as_secs())
        })?;
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    stdout
        .read_to_string(&mut stdout_text)
        .map_err(|error| format!("failed reading git stdout: {error}"))?;
    stderr
        .read_to_string(&mut stderr_text)
        .map_err(|error| format!("failed reading git stderr: {error}"))?;
    Ok(ShellOutput {
        code: status.code(),
        stdout: stdout_text,
        stderr: stderr_text,
    })
}

fn first_sha(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|line| line.split_whitespace().next())
        .filter(|sha| !sha.is_empty())
        .map(str::to_owned)
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tempfile::TempDir;
    use toml::Table;

    use super::{
        BranchApplyStatus, apply_branch_rules, create_branch_and_apply_rules,
        create_branch_on_remote,
    };
    use crate::governance::{GovernanceGh, resolve_branch_rules};

    #[test]
    fn create_branch_on_remote_is_idempotent_when_remote_branch_exists() {
        let temp = TempDir::new().expect("tempdir");
        let git = write_script(
            temp.path(),
            "git-existing",
            r#"#!/bin/sh
if [ "$1" = "ls-remote" ] && [ "$3" = "--heads" ]; then
  exit 0
fi
exit 99
"#,
        );

        let result = create_branch_on_remote(temp.path(), "develop/demo", "main", Some(&git));

        assert_eq!(result.status, BranchApplyStatus::AlreadyExists);
        assert_eq!(result.branch, "develop/demo");
        assert!(!result.ok());
    }

    #[test]
    fn create_branch_on_remote_uses_remote_base_sha_for_push_refspec() {
        let temp = TempDir::new().expect("tempdir");
        let trace = temp.path().join("trace");
        let base_sha = "abcdef1234567890abcdef1234567890abcdef12";
        let git = write_script(
            temp.path(),
            "git-create",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
"ls-remote --exit-code --heads origin release/1.0")
  exit 2
  ;;
"ls-remote --exit-code origin refs/heads/main")
  printf '{}\trefs/heads/main\n'
  exit 0
  ;;
"push origin {}:refs/heads/release/1.0")
  exit 0
  ;;
esac
exit 99
"#,
                trace.display(),
                base_sha,
                base_sha
            ),
        );

        let result = create_branch_on_remote(temp.path(), "release/1.0", "main", Some(&git));

        assert_eq!(
            result.status,
            BranchApplyStatus::Created,
            "{}",
            result.message
        );
        assert!(result.message.contains("abcdef12"));
        let trace = fs::read_to_string(trace).expect("trace");
        assert!(trace.contains(&format!("push origin {base_sha}:refs/heads/release/1.0")));
    }

    #[test]
    fn create_branch_and_apply_rules_runs_gh_after_existing_branch() {
        let temp = TempDir::new().expect("tempdir");
        let payload = temp.path().join("payload.json");
        let git = write_script(
            temp.path(),
            "git-existing",
            r#"#!/bin/sh
if [ "$1" = "ls-remote" ] && [ "$3" = "--heads" ]; then
  exit 0
fi
exit 99
"#,
        );
        let gh = write_script(
            temp.path(),
            "gh",
            &format!(
                r"#!/bin/sh
cat > '{}'
exit 0
",
                payload.display()
            ),
        );
        let rules = rules();
        let gh = GovernanceGh::ambient(temp.path(), Some(&gh));

        let result = create_branch_and_apply_rules(
            temp.path(),
            "owner/repo",
            "develop/demo",
            "main",
            &rules,
            Some(&git),
            &gh,
        );

        assert_eq!(result.status, BranchApplyStatus::RulesApplied);
        assert!(result.ok());
        let payload = fs::read_to_string(payload).expect("payload");
        assert!(payload.contains("\"required_status_checks\""));
    }

    #[test]
    fn apply_branch_rules_reports_gh_failures() {
        let temp = TempDir::new().expect("tempdir");
        let gh = write_script(
            temp.path(),
            "gh-fail",
            r#"#!/bin/sh
cat >/dev/null
echo "forbidden" >&2
exit 1
"#,
        );
        let gh = GovernanceGh::ambient(temp.path(), Some(&gh));

        let result = apply_branch_rules("owner/repo", "main", &rules(), &gh);

        assert_eq!(result.status, BranchApplyStatus::RulesFailed);
        assert!(result.message.contains("forbidden"));
        assert!(!result.ok());
    }

    fn rules() -> crate::governance::BranchProtectionRules {
        let config = r#"
            [project]
            profile = "multi"

            [governance]
            required_status_checks = ["ci"]
        "#
        .parse::<Table>()
        .expect("toml");
        resolve_branch_rules(&config, "main").expect("rules")
    }

    fn write_script(dir: &Path, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).expect("write script");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod");
        path
    }
}
