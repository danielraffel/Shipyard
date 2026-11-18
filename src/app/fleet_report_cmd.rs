//! One local, read-only fleet page. No GitHub reads.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use wait_timeout::ChildExt;

use super::{CliFailure, dwell_cmd};

fn unavailable(reason: impl std::fmt::Display) -> Value {
    json!({"unavailable": reason.to_string()})
}

fn steward_summary() -> Value {
    let Some(home) = std::env::var_os("HOME") else {
        return unavailable("HOME is unset");
    };
    let path = Path::new(&home).join("Library/Logs/shipyard-steward-tick/summary.log");
    match std::fs::read_to_string(&path) {
        Ok(text) => text
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map_or_else(|| unavailable("summary log is empty"), |line| json!(line)),
        Err(error) => unavailable(error),
    }
}

fn pool() -> Value {
    let child = Command::new("sr")
        .args(["status", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return unavailable(child.unwrap_err());
    };
    match child.wait_timeout(Duration::from_secs(3)) {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return unavailable("sr status timed out");
        }
        Err(error) => return unavailable(error),
    }
    match child.wait_with_output() {
        Ok(output) if output.status.success() => {
            match serde_json::from_slice::<Value>(&output.stdout) {
                Ok(status) => status
                    .get("claude_pool")
                    .cloned()
                    .unwrap_or_else(|| unavailable("claude_pool field missing")),
                Err(error) => unavailable(format!("invalid sr JSON: {error}")),
            }
        }
        Ok(output) => unavailable(format!("sr exited {}", output.status)),
        Err(error) => unavailable(error),
    }
}

fn hosts(path: Option<&Path>) -> Value {
    let Some(path) = path else {
        return unavailable("--hosts-file not provided");
    };
    let result = std::fs::read_to_string(path)
        .map_err(|error| error.to_string())
        .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|error| error.to_string()));
    match result {
        Ok(Value::Array(rows))
            if rows.iter().all(|row| {
                row.get("host").and_then(Value::as_str).is_some()
                    && row.get("status").and_then(Value::as_str).is_some()
            }) =>
        {
            Value::Array(rows)
        }
        Ok(_) => unavailable("expected a JSON array of {host, status} objects"),
        Err(error) => unavailable(error),
    }
}

fn owner_action(row: &Value) -> bool {
    if row.pointer("/next_action/actor").and_then(Value::as_str) == Some("owner")
        || row.get("actor").and_then(Value::as_str) == Some("owner")
    {
        return true;
    }
    let token = row
        .get("blocked_on")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    [
        "owner_decision",
        "owner-decision",
        "secret",
        "setting",
        "sudo",
    ]
    .iter()
    .any(|needle| token.contains(needle))
}

fn count_rows(rows: &[Value], overdue: &BTreeSet<u64>) -> Value {
    let count =
        |predicate: &dyn Fn(&Value) -> bool| rows.iter().filter(|row| predicate(row)).count();
    json!({
        "open": rows.len(),
        "approved_green": count(&|row| row.get("approved_green").and_then(Value::as_bool)
            .unwrap_or_else(|| matches!(row.get("tier").and_then(Value::as_str), Some("approved_green")))),
        "waiting_review": count(&|row| {
            matches!(row.get("tier").and_then(Value::as_str), Some("green_needs_review"))
                || matches!(row.get("blocked_on").and_then(Value::as_str), Some("no_verdict" | "awaiting_review"))
        }),
        "red": count(&|row| {
            let token = row.get("blocked_on").and_then(Value::as_str).unwrap_or("");
            row.get("red").and_then(Value::as_bool).unwrap_or(false)
                || token.starts_with("required_failed") || token.starts_with("held:")
                || token.starts_with("merge_state:DIRTY")
        }),
        "overdue": overdue.len(),
    })
}

fn report(snapshots: Vec<Value>, hosts_file: Option<&Path>) -> Value {
    let repos: Vec<Value> = snapshots
        .into_iter()
        .map(|snapshot| {
            let mut rows = snapshot
                .get("fleet_rows")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_else(|| {
                    // Schema 1 snapshots from older stewards still produce a useful, bounded page.
                    let mut rows = snapshot
                        .get("next_to_admit")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    for overdue in snapshot
                        .get("overdue")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let number = overdue.get("pr").and_then(Value::as_u64);
                        if !rows
                            .iter()
                            .any(|row| row.get("number").and_then(Value::as_u64) == number)
                        {
                            let mut row = overdue.clone();
                            row["number"] = overdue.get("pr").cloned().unwrap_or(Value::Null);
                            rows.push(row);
                        }
                    }
                    rows
                });
            for row in &mut rows {
                if let Some(title) = row.get("title").and_then(Value::as_str)
                    && title.chars().count() > 70
                {
                    row["title"] =
                        json!(format!("{}…", title.chars().take(69).collect::<String>()));
                }
            }
            let overdue: BTreeSet<u64> = snapshot
                .get("overdue")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|row| row.get("pr").and_then(Value::as_u64))
                .collect();
            let counts = count_rows(&rows, &overdue);
            json!({"repo": snapshot.get("repo"), "base": snapshot.get("base"),
            "at": snapshot.get("at"), "counts": counts, "rows": rows,
            "complete": snapshot.get("fleet_rows").is_some()})
        })
        .collect();
    let owner_actions: Vec<Value> = repos
        .iter()
        .flat_map(|repo| {
            repo["rows"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|row| owner_action(row))
                .map(|row| {
                    let mut row = row.clone();
                    row["repo"] = repo["repo"].clone();
                    row
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let sum = |key: &str| {
        repos
            .iter()
            .map(|repo| repo["counts"][key].as_u64().unwrap_or(0))
            .sum::<u64>()
    };
    json!({"source": "steward-snapshot", "summary": {
        "repos": repos.len(), "open": sum("open"), "approved_green": sum("approved_green"),
        "waiting_review": sum("waiting_review"), "red": sum("red"), "overdue": sum("overdue")},
        "owner_actions": owner_actions, "repos": repos,
        "steward_tick": steward_summary(), "pool": pool(), "hosts": hosts(hosts_file)})
}

fn cell(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .map_or_else(
            || value.map_or_else(|| "—".to_owned(), Value::to_string),
            str::to_owned,
        )
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
}

fn row_line(row: &Value, include_repo: bool) -> String {
    let mut cells = Vec::new();
    if include_repo {
        cells.push(cell(row.get("repo")));
    }
    cells.push(format!(
        "#{}",
        row.get("number")
            .and_then(Value::as_u64)
            .map_or_else(|| "?".to_owned(), |n| n.to_string())
    ));
    let title = row.get("title").and_then(Value::as_str).unwrap_or("—");
    cells.push(cell(Some(&json!(
        title.chars().take(70).collect::<String>()
    ))));
    cells.push(cell(row.get("blocked_on")));
    cells.push(cell(row.get("owner")));
    cells.push(
        row.get("age_minutes")
            .and_then(Value::as_i64)
            .map_or_else(|| "—".to_owned(), |n| format!("{n}m")),
    );
    cells.push(cell(
        row.pointer("/next_action/action")
            .or_else(|| row.get("action")),
    ));
    cells.push(cell(
        row.pointer("/next_action/actor")
            .or_else(|| row.get("actor")),
    ));
    format!("| {} |", cells.join(" | "))
}

fn source_line(value: &Value) -> String {
    value
        .get("unavailable")
        .and_then(Value::as_str)
        .map_or_else(
            || cell(Some(value)),
            |reason| format!("(unavailable: {reason})"),
        )
}

fn markdown(data: &Value) -> String {
    let summary = &data["summary"];
    let mut out = format!(
        "# Fleet report\n\n## Summary\n\n{} repos; {} open; {} approved and green; {} waiting review; {} red; {} overdue.\n\nSteward tick: {}\n\n",
        summary["repos"],
        summary["open"],
        summary["approved_green"],
        summary["waiting_review"],
        summary["red"],
        summary["overdue"],
        source_line(&data["steward_tick"])
    );
    out.push_str("## Owner actions\n\n");
    let owner = data["owner_actions"]
        .as_array()
        .expect("owner actions array");
    if owner.is_empty() {
        out.push_str("None.\n\n");
    } else {
        out.push_str("| Repo | PR | Title | Blocked on | Owner | Age | Next action | Who acts |\n| --- | --- | --- | --- | --- | --- | --- | --- |\n");
        for row in owner {
            out.push_str(&row_line(row, true));
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str("## Per repo\n\n");
    for repo in data["repos"].as_array().expect("repos array") {
        let counts = &repo["counts"];
        let _ = write!(
            out,
            "### {}\n\n{} open; {} approved and green; {} waiting review; {} red; {} overdue. Snapshot: {}.\n\n",
            cell(repo.get("repo")),
            counts["open"],
            counts["approved_green"],
            counts["waiting_review"],
            counts["red"],
            counts["overdue"],
            cell(repo.get("at"))
        );
        if repo["complete"] == false {
            out.push_str("(unavailable: this steward snapshot predates full fleet rows; counts cover displayed rows only)\n\n");
        }
        out.push_str("| PR | Title | Blocked on | Owner | Age | Next action | Who acts |\n| --- | --- | --- | --- | --- | --- | --- |\n");
        for row in repo["rows"].as_array().expect("rows array") {
            out.push_str(&row_line(row, false));
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str("## Pool\n\n");
    let _ = write!(
        out,
        "Claude pool: {}\n\nHosts:\n",
        source_line(&data["pool"])
    );
    if let Some(hosts) = data["hosts"].as_array() {
        for host in hosts {
            let _ = writeln!(
                out,
                "- {}: {}",
                cell(host.get("host")),
                cell(host.get("status"))
            );
        }
    } else {
        let _ = writeln!(out, "{}", source_line(&data["hosts"]));
    }
    out
}

pub(super) fn run<W: Write>(
    state_dir: &Path,
    format: &str,
    hosts_file: Option<&Path>,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let data = report(dwell_cmd::snapshots(state_dir, None), hosts_file);
    let rendered = if format == "json" {
        serde_json::to_string_pretty(&data).expect("JSON value")
    } else {
        markdown(&data)
    };
    writeln!(stdout, "{rendered}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_snapshots_group_rows_and_owner_actions() {
        let data = report(
            vec![
                json!({"repo":"o/a","at":"now","fleet_rows":[
            {"number":1,"title":"Owner choice","blocked_on":"owner_decision","owner":"daniel","age_minutes":42,
             "next_action":{"actor":"owner","action":"decide"},"tier":"rest"},
            {"number":2,"title":"Ready","blocked_on":"ready","tier":"approved_green"}],
            "overdue":[{"pr":1}]}),
                json!({"repo":"o/b","at":"now","fleet_rows":[
            {"number":3,"title":"Review","blocked_on":"no_verdict","tier":"green_needs_review"}],"overdue":[]}),
            ],
            None,
        );
        assert_eq!(data["summary"]["open"], 3);
        assert_eq!(data["summary"]["approved_green"], 1);
        assert_eq!(data["summary"]["waiting_review"], 1);
        assert_eq!(data["owner_actions"][0]["repo"], "o/a");
        let md = markdown(&data);
        assert!(md.contains("## Owner actions\n\n| Repo"));
        assert!(md.contains("### o/a"));
        assert!(md.contains("### o/b"));
        assert!(md.contains("(unavailable:"));
    }

    #[test]
    fn unavailable_hosts_do_not_fail_command() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot =
            super::super::merge_steward_cmd::dwell_pass::snapshot_path(temp.path(), "o/a", "main");
        std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        std::fs::write(
            &snapshot,
            json!({"repo":"o/a","base":"main","at":"now",
            "fleet_rows":[{"number":1,"title":"Review","blocked_on":"no_verdict",
                "tier":"green_needs_review"}],"overdue":[]})
            .to_string(),
        )
        .unwrap();
        let mut output = Vec::new();
        run(
            temp.path(),
            "md",
            Some(&temp.path().join("missing.json")),
            &mut output,
        )
        .unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Hosts:\n(unavailable:")
        );
        let mut output = Vec::new();
        run(temp.path(), "json", None, &mut output).unwrap();
        let report: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(report["repos"][0]["rows"][0]["number"], 1);
    }
}
