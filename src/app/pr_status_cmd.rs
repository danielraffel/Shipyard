//! Read-only fleet PR ledger (`shipyard pr status --all`).

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::CliFailure;

/// Single source of truth for the daemon and read-only ledger fleet scope.
pub(crate) const FLEET_REPOSITORIES: &[&str] = &[
    "Generous-Corp/pulp",
    "danielraffel/tartci",
    "Generous-Corp/vellum",
];

#[derive(Debug, Serialize)]
struct LedgerRow {
    repository: String,
    number: u64,
    head: String,
    owner: String,
    next_action: String,
    age_seconds: u64,
    last_movement: String,
    stalled: bool,
}

/// Print an authoritative, read-only snapshot. Mutations are deliberately not
/// available on this path; the GitHub response is rendered as received.
pub(crate) fn status_command<W: Write>(
    cwd: &Path,
    all: bool,
    json: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    status_command_with_reader(cwd, all, json, stdout, |repository| {
        gh_json(cwd, repository)
    })
}

/// The production GitHub read seam. Keeping this separate from row assembly
/// lets the ledger entry point be exercised against captured wire fixtures.
fn gh_json(cwd: &Path, repository: &str) -> Result<Value, CliFailure> {
    let output = Command::new("ghapp")
        .args([
            "pr", "list", "--repo", repository, "--state", "open", "--limit", "100",
            "--json", "number,headRefOid,updatedAt,body,author,mergeStateStatus,statusCheckRollup,comments,commits",
        ])
        .current_dir(cwd)
        .output()
        .map_err(|error| CliFailure::new(1, format!("ghapp pr list failed: {error}")))?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "ghapp pr list failed for {repository}: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| CliFailure::new(1, format!("invalid gh PR JSON: {error}")))
}

fn status_command_with_reader<W, F>(
    cwd: &Path,
    all: bool,
    json: bool,
    stdout: &mut W,
    read: F,
) -> Result<std::process::ExitCode, CliFailure>
where
    W: Write,
    F: Fn(&str) -> Result<Value, CliFailure>,
{
    let repositories = if all {
        configured_repositories(cwd)
    } else {
        vec![current_repository(cwd)?]
    };
    let mut rows = Vec::new();
    for repository in repositories {
        let values = read(&repository)?;
        let items = values
            .as_array()
            .ok_or_else(|| CliFailure::new(1, "gh PR list response was not an array"))?;
        for item in items {
            let movement = latest_movement(item);
            let age_seconds = parse_age(&movement);
            let merge_state = item
                .get("mergeStateStatus")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN");
            let queue_ready = matches!(merge_state, "CLEAN" | "BEHIND");
            rows.push(LedgerRow {
                repository: repository.clone(),
                number: item
                    .get("number")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                head: item
                    .get("headRefOid")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                owner: owner_for(item),
                next_action: if queue_ready { "observe" } else { "owner-wake" }.to_owned(),
                age_seconds,
                last_movement: movement,
                stalled: !queue_ready && age_seconds >= 2 * 60 * 60,
            });
        }
    }
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &rows)
            .map_err(|error| CliFailure::new(1, format!("failed to render ledger: {error}")))?;
        writeln!(stdout).ok();
    } else {
        writeln!(
            stdout,
            "REPOSITORY\tPR\tOWNER\tNEXT\tAGE\tLAST MOVEMENT\tSTATE"
        )
        .ok();
        for row in rows {
            writeln!(
                stdout,
                "{}\t{}\t{}\t{}\t{}s\t{}\t{}",
                row.repository,
                row.number,
                row.owner,
                row.next_action,
                row.age_seconds,
                row.last_movement,
                if row.stalled { "STALLED" } else { "ACTIVE" }
            )
            .ok();
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn parse_age(updated: &str) -> u64 {
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(updated) else {
        return 0;
    };
    let now = chrono::Utc::now();
    (now - time.with_timezone(&chrono::Utc))
        .to_std()
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn latest_movement(item: &serde_json::Value) -> String {
    let mut times = Vec::new();
    if let Some(value) = item.get("updatedAt").and_then(serde_json::Value::as_str) {
        times.push(value);
    }
    if let Some(commits) = item.get("commits").and_then(serde_json::Value::as_array) {
        for commit in commits {
            if let Some(value) = commit
                .get("committedDate")
                .and_then(serde_json::Value::as_str)
            {
                times.push(value);
            }
        }
    }
    if let Some(comments) = item.get("comments").and_then(serde_json::Value::as_array) {
        for comment in comments {
            for key in ["createdAt", "updatedAt"] {
                if let Some(value) = comment.get(key).and_then(serde_json::Value::as_str) {
                    times.push(value);
                }
            }
        }
    }
    if let Some(checks) = item
        .get("statusCheckRollup")
        .and_then(serde_json::Value::as_array)
    {
        for check in checks {
            if let Some(value) = check.get("completedAt").and_then(serde_json::Value::as_str) {
                times.push(value);
            }
        }
    }
    times.into_iter().max().unwrap_or_default().to_owned()
}

pub(crate) fn owner_for(item: &serde_json::Value) -> String {
    if let Some(body) = item.get("body").and_then(serde_json::Value::as_str)
        && let Some(session) = crate::pr_fold::whence_session(body)
    {
        return session;
    }
    item.get("commits")
        .and_then(serde_json::Value::as_array)
        .and_then(|commits| commits.last())
        .and_then(|commit| commit.get("authors"))
        .and_then(serde_json::Value::as_array)
        .and_then(|authors| authors.last())
        .and_then(|author| author.get("login"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("team-lead")
        .to_owned()
}

fn current_repository(cwd: &Path) -> Result<String, CliFailure> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .map_err(|error| CliFailure::new(1, format!("git remote failed: {error}")))?;
    let remote = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    parse_repository_remote(&remote)
        .ok_or_else(|| CliFailure::new(1, "could not determine GitHub repository"))
}

fn parse_repository_remote(remote: &str) -> Option<String> {
    remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("http://github.com/"))
        .or_else(|| remote.strip_prefix("git@github.com:"))
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .map(|path| path.trim_end_matches(".git").to_owned())
}
fn configured_repositories(cwd: &Path) -> Vec<String> {
    let mut repos = FLEET_REPOSITORIES
        .iter()
        .map(|repo| (*repo).to_owned())
        .collect::<Vec<_>>();
    if let Ok(current) = current_repository(cwd)
        && !repos.contains(&current)
    {
        repos.push(current);
    }
    repos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_uses_latest_of_head_comment_and_check_movement() {
        let item = serde_json::json!({
            "updatedAt": "2026-10-11T00:00:00Z",
            "commits": [{"committedDate": "2026-10-11T01:00:00Z", "authors": [{"login": "alice"}]}],
            "comments": [{"createdAt": "2026-10-11T02:00:00Z", "updatedAt": "2026-10-11T02:00:00Z"}],
            "statusCheckRollup": [{"completedAt": "2026-10-11T03:00:00Z"}]
        });
        assert_eq!(latest_movement(&item), "2026-10-11T03:00:00Z");
    }

    #[test]
    fn parses_https_and_ssh_repository_remotes() {
        assert_eq!(
            parse_repository_remote("https://github.com/danielraffel/Shipyard.git"),
            Some("danielraffel/Shipyard".to_owned())
        );
        assert_eq!(
            parse_repository_remote("git@github.com:danielraffel/Shipyard.git"),
            Some("danielraffel/Shipyard".to_owned())
        );
    }

    #[test]
    fn status_entry_point_reads_committed_wire_fixtures() {
        let mut output = Vec::new();
        let cwd = Path::new(".");
        let result = status_command_with_reader(cwd, true, true, &mut output, |repository| {
            let fixture = match repository {
                "Generous-Corp/pulp" => {
                    include_str!("../../tests/fixtures/github/pr_list_wire/pulp.json")
                }
                "danielraffel/tartci" => {
                    include_str!("../../tests/fixtures/github/pr_list_wire/tartci.json")
                }
                "Generous-Corp/vellum" | "danielraffel/Shipyard" => {
                    include_str!("../../tests/fixtures/github/pr_list_wire/shipyard.json")
                }
                other => panic!("unexpected repository {other}"),
            };
            serde_json::from_str(fixture).map_err(|e| CliFailure::new(1, e.to_string()))
        });
        assert!(result.is_ok());
        let rendered = String::from_utf8(output).expect("json");
        assert!(rendered.contains("Generous-Corp/pulp"));
        assert!(rendered.contains("danielraffel/tartci"));
        assert!(rendered.contains("owner"));
    }

    #[test]
    fn owner_prefers_whence_then_last_committer_then_team_lead() {
        let whence = serde_json::json!({"body": "<!-- whence {\"prov\": {\"session\": \"sess-1\"}} -->", "commits": [{"authors": [{"login": "alice"}]}]});
        assert_eq!(owner_for(&whence), "sess-1");
        let committer = serde_json::json!({"body": "plain", "commits": [{"authors": [{"login": "alice"}]}, {"authors": [{"login": "bob"}]}]});
        assert_eq!(owner_for(&committer), "bob");
        assert_eq!(
            owner_for(&serde_json::json!({"body": "plain"})),
            "team-lead"
        );
    }
}
