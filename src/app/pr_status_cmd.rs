//! Read-only fleet PR ledger (`shipyard pr status --all`).

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use super::CliFailure;

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
    let repositories = if all {
        configured_repositories(cwd)
    } else {
        vec![current_repository(cwd)?]
    };
    let mut rows = Vec::new();
    for repository in repositories {
        let output = Command::new("gh")
            .args([
                "pr", "list", "--repo", &repository, "--state", "open", "--limit", "100",
                "--json", "number,headRefOid,updatedAt,author,mergeStateStatus,statusCheckRollup",
            ])
            .current_dir(cwd)
            .output()
            .map_err(|error| CliFailure::new(1, format!("gh pr list failed: {error}")))?;
        if !output.status.success() {
            return Err(CliFailure::new(
                1,
                format!("gh pr list failed for {repository}: {}", String::from_utf8_lossy(&output.stderr)),
            ));
        }
        let values: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| CliFailure::new(1, format!("invalid gh PR JSON: {error}")))?;
        let items = values
            .as_array()
            .ok_or_else(|| CliFailure::new(1, "gh PR list response was not an array"))?;
        for item in items {
            let updated = item.get("updatedAt").and_then(serde_json::Value::as_str).unwrap_or("");
            let age_seconds = parse_age(updated);
            let merge_state = item
                .get("mergeStateStatus")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("UNKNOWN");
            let queue_ready = matches!(merge_state, "CLEAN" | "BEHIND");
            rows.push(LedgerRow {
                repository: repository.clone(),
                number: item.get("number").and_then(serde_json::Value::as_u64).unwrap_or_default(),
                head: item.get("headRefOid").and_then(serde_json::Value::as_str).unwrap_or("").to_owned(),
                owner: item
                    .get("author")
                    .and_then(|author| author.get("login"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("team-lead")
                    .to_owned(),
                next_action: if queue_ready { "observe" } else { "owner-wake" }.to_owned(),
                age_seconds,
                last_movement: updated.to_owned(),
                stalled: !queue_ready && age_seconds >= 2 * 60 * 60,
            });
        }
    }
    if json {
        serde_json::to_writer_pretty(&mut *stdout, &rows)
            .map_err(|error| CliFailure::new(1, format!("failed to render ledger: {error}")))?;
        writeln!(stdout).ok();
    } else {
        writeln!(stdout, "REPOSITORY\tPR\tOWNER\tNEXT\tAGE\tLAST MOVEMENT\tSTATE").ok();
        for row in rows {
            writeln!(stdout, "{}\t{}\t{}\t{}\t{}s\t{}\t{}", row.repository, row.number, row.owner, row.next_action, row.age_seconds, row.last_movement, if row.stalled { "STALLED" } else { "ACTIVE" }).ok();
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn parse_age(updated: &str) -> u64 {
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(updated) else { return 0 };
    let now = chrono::Utc::now();
    (now - time.with_timezone(&chrono::Utc)).to_std().unwrap_or(Duration::ZERO).as_secs()
}

fn current_repository(cwd: &Path) -> Result<String, CliFailure> {
    let output = Command::new("git").args(["remote", "get-url", "origin"]).current_dir(cwd).output()
        .map_err(|error| CliFailure::new(1, format!("git remote failed: {error}")))?;
    let remote = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    remote.trim_end_matches(".git").rsplit_once(':').map(|(_, repo)| repo.to_owned())
        .or_else(|| remote.split_once("github.com/").map(|(_, repo)| repo.to_owned()))
        .ok_or_else(|| CliFailure::new(1, "could not determine GitHub repository"))
}

fn configured_repositories(cwd: &Path) -> Vec<String> {
    let mut repos = vec!["Generous-Corp/pulp".to_owned(), "danielraffel/tartci".to_owned(), "Generous-Corp/vellum".to_owned()];
    if let Ok(current) = current_repository(cwd) { if !repos.contains(&current) { repos.push(current); } }
    repos
}
