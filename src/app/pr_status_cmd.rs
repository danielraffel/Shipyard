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
    status_command_with_reader(cwd, all, json, stdout, chrono::Utc::now(), |repository| {
        gh_json(cwd, repository)
    })
}

/// Fields read from the pull request list. `commits` is deliberately absent:
/// asking for it on the list multiplies every PR by its commits and their
/// authors, which GitHub refuses (over 500,000 possible nodes) for a busy
/// repository. Commits are read per pull request instead.
const LIST_FIELDS: &str =
    "number,headRefOid,updatedAt,body,author,mergeStateStatus,statusCheckRollup,comments";

/// The production GitHub read seam. Keeping this separate from row assembly
/// lets the ledger entry point be exercised against captured wire fixtures.
fn gh_json(cwd: &Path, repository: &str) -> Result<Value, CliFailure> {
    let mut rows = ghapp_json(
        cwd,
        &[
            "pr",
            "list",
            "--repo",
            repository,
            "--state",
            "open",
            "--limit",
            "100",
            "--json",
            LIST_FIELDS,
        ],
    )?;
    let items = rows
        .as_array_mut()
        .ok_or_else(|| CliFailure::new(1, "gh PR list response was not an array"))?;
    for item in items {
        let Some(number) = item.get("number").and_then(Value::as_u64) else {
            continue;
        };
        let view = ghapp_json(
            cwd,
            &[
                "pr",
                "view",
                &number.to_string(),
                "--repo",
                repository,
                "--json",
                "commits",
            ],
        )?;
        if let (Some(commits), Some(object)) = (view.get("commits"), item.as_object_mut()) {
            object.insert("commits".to_owned(), commits.clone());
        }
    }
    Ok(rows)
}

fn ghapp_json(cwd: &Path, args: &[&str]) -> Result<Value, CliFailure> {
    let output = Command::new("ghapp")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| {
            CliFailure::new(1, format!("ghapp {} failed: {error}", args[..2].join(" ")))
        })?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "ghapp {} failed: {}",
                args.join(" "),
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
    now: chrono::DateTime<chrono::Utc>,
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
        rows.extend(items.iter().map(|item| ledger_row(&repository, item, now)));
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

/// One ledger row for a pull request as GitHub returned it, judged at `now`.
fn ledger_row(repository: &str, item: &Value, now: chrono::DateTime<chrono::Utc>) -> LedgerRow {
    let movement = latest_movement(item);
    let age_seconds = parse_age(&movement, now);
    let merge_state = item
        .get("mergeStateStatus")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN");
    let queue_ready = matches!(merge_state, "CLEAN" | "BEHIND");
    LedgerRow {
        repository: repository.to_owned(),
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
        stalled: is_stalled(age_seconds, queue_ready),
    }
}

/// STALLED: no movement for two hours on a pull request that is not
/// queue-ready.
fn is_stalled(age_seconds: u64, queue_ready: bool) -> bool {
    !queue_ready && age_seconds >= 2 * 60 * 60
}

fn parse_age(updated: &str, now: chrono::DateTime<chrono::Utc>) -> u64 {
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(updated) else {
        return 0;
    };
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

fn owner_for(item: &serde_json::Value) -> String {
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
        .and_then(|authors| {
            authors
                .iter()
                .rev()
                .filter_map(|author| author.get("login").and_then(serde_json::Value::as_str))
                .find(|login| !login.is_empty())
        })
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
        let now = chrono::Utc::now();
        let result = status_command_with_reader(cwd, true, true, &mut output, now, |repository| {
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

    /// Open pulp pull requests as the production read returns them: the
    /// list fields plus each PR's `commits`, captured whole and unedited. A
    /// CLEAN row (10076), a DIRTY row (10057), and a BEHIND row with no
    /// whence block whose commits carry only login-less authors (10118).
    const LEDGER_PULP: &str =
        include_str!("../../tests/fixtures/github/pr_status_ledger/pulp.json");

    fn ledger_fixture_row(number: u64) -> Value {
        let rows: Value = serde_json::from_str(LEDGER_PULP).expect("ledger fixture");
        rows.as_array()
            .expect("array")
            .iter()
            .find(|row| row["number"] == number)
            .cloned()
            .unwrap_or_else(|| panic!("fixture row {number}"))
    }

    fn at(row: &Value, seconds_after_movement: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(&latest_movement(row))
            .expect("movement time")
            .with_timezone(&chrono::Utc)
            + chrono::Duration::seconds(seconds_after_movement)
    }

    #[test]
    fn stalled_threshold_is_exact_and_queue_ready_never_stalls() {
        assert!(!is_stalled(7_199, false));
        assert!(is_stalled(7_200, false));
        assert!(!is_stalled(99_999, true));

        let dirty = ledger_fixture_row(10057);
        let fresh = ledger_row("Generous-Corp/pulp", &dirty, at(&dirty, 7_199));
        assert_eq!(fresh.age_seconds, 7_199);
        assert!(!fresh.stalled, "under two hours is not STALLED");
        let stale = ledger_row("Generous-Corp/pulp", &dirty, at(&dirty, 7_200));
        assert!(stale.stalled, "two hours without movement is STALLED");
        assert_eq!(stale.next_action, "owner-wake");

        let clean = ledger_fixture_row(10076);
        let row = ledger_row("Generous-Corp/pulp", &clean, at(&clean, 99_999));
        assert_eq!(row.next_action, "observe");
        assert!(!row.stalled, "a queue-ready PR never stalls");
    }

    #[test]
    fn status_entry_point_judges_real_ledger_rows() {
        let mut output = Vec::new();
        let clean = ledger_fixture_row(10076);
        let now = at(&clean, 3 * 60 * 60);
        let result = status_command_with_reader(
            Path::new("."),
            true,
            true,
            &mut output,
            now,
            |repository| match repository {
                "Generous-Corp/pulp" => {
                    serde_json::from_str(LEDGER_PULP).map_err(|e| CliFailure::new(1, e.to_string()))
                }
                _ => Ok(Value::Array(Vec::new())),
            },
        );
        assert!(result.is_ok());
        let rows: Value = serde_json::from_slice(&output).expect("json ledger");
        let row = |number: u64| {
            rows.as_array()
                .expect("rows")
                .iter()
                .find(|row| row["number"] == number)
                .cloned()
                .unwrap_or_else(|| panic!("ledger row {number}: {rows}"))
        };
        let clean = row(10076);
        assert_eq!(clean["head"], "62fdd6a4f64655cea1277f795716dc196925b4cb");
        assert_eq!(clean["owner"], "01a12472-78d6-7d03-bca5-d27ce9afe730");
        assert_eq!(clean["next_action"], "observe");
        assert_eq!(clean["stalled"], false);
        let dirty = row(10057);
        assert_eq!(dirty["owner"], "01a10556-a83e-7803-b7c8-0a2a08bfa84e");
        assert_eq!(dirty["next_action"], "owner-wake");
        assert_eq!(dirty["stalled"], true);
        let behind = row(10118);
        assert_eq!(
            behind["owner"], "team-lead",
            "login-less authors fall through"
        );
        assert_eq!(behind["next_action"], "observe");
        assert_eq!(behind["stalled"], false);
    }

    #[test]
    fn fleet_repository_defaults_include_all_configured_repositories() {
        assert_eq!(
            FLEET_REPOSITORIES,
            &[
                "Generous-Corp/pulp",
                "danielraffel/tartci",
                "Generous-Corp/vellum"
            ]
        );
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
