//! One local, read-only fleet page. No GitHub reads.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use wait_timeout::ChildExt;

use super::{CliFailure, dwell_cmd};
use crate::cloud::GitHubActions;
use crate::identity::RuntimeMode;

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
    let status = match child.wait_timeout(Duration::from_secs(3)) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return unavailable("sr status timed out");
        }
        Err(error) => return unavailable(error),
    };
    let mut stdout = Vec::new();
    let Some(mut pipe) = child.stdout.take() else {
        return unavailable("sr stdout unavailable");
    };
    if let Err(error) = pipe.read_to_end(&mut stdout) {
        return unavailable(error);
    }
    pool_result(status, &stdout)
}

fn pool_result(status: ExitStatus, stdout: &[u8]) -> Value {
    if !status.success() {
        return unavailable(format!("sr exited {status}"));
    }
    match serde_json::from_slice::<Value>(stdout) {
        Ok(status) => status
            .get("claude_pool")
            .cloned()
            .unwrap_or_else(|| unavailable("claude_pool field missing")),
        Err(error) => unavailable(format!("invalid sr JSON: {error}")),
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

#[allow(clippy::too_many_lines)] // Keep the bounded report assembly in one auditable order.
fn report_with_snapshot_source(
    snapshots: Vec<Value>,
    hosts_file: Option<&Path>,
    snapshot_source: &Value,
) -> Value {
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
            "overdue_rows": snapshot.get("overdue").cloned().unwrap_or_else(|| json!([])),
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
    let mut invariant_rows: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let add = |kind: &str, repo: &Value, row: &Value, map: &mut BTreeMap<String, Vec<String>>| {
        let repo = repo.as_str().unwrap_or("?");
        let number = row
            .get("number")
            .and_then(Value::as_u64)
            .or_else(|| row.get("pr").and_then(Value::as_u64))
            .map_or_else(|| "?".to_owned(), |n| n.to_string());
        let entries = map.entry(kind.to_owned()).or_default();
        let entry = format!("{repo}#{number}");
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    };
    for repo in &repos {
        for row in repo["rows"].as_array().into_iter().flatten() {
            let owner = row
                .get("owner")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if owner.is_empty() || owner.eq_ignore_ascii_case("unknown") {
                add(
                    "open_pr_without_owner",
                    &repo["repo"],
                    row,
                    &mut invariant_rows,
                );
            }
            if row.get("blocked_on").and_then(Value::as_str) == Some("empty_diff") {
                add("empty_diff", &repo["repo"], row, &mut invariant_rows);
            }
            if ["kind", "state", "blocked_on"].iter().any(|field| {
                row.get(*field)
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.eq_ignore_ascii_case("approved_stuck"))
            }) {
                add("approved_stuck", &repo["repo"], row, &mut invariant_rows);
            }
        }
        if let Some(overdue) = repo["overdue_rows"].as_array() {
            for row in overdue {
                let kind = row.get("kind").and_then(Value::as_str).unwrap_or("overdue");
                add(kind, &repo["repo"], row, &mut invariant_rows);
            }
        }
    }
    let invariants: Vec<Value> = invariant_rows
        .into_iter()
        .map(|(kind, prs)| json!({"kind": kind, "count": prs.len(), "prs": prs}))
        .collect();
    json!({"source": "steward-snapshot", "steward_snapshots": snapshot_source, "invariants": invariants, "summary": {
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
    let mut out = "# Fleet report\n\n## Invariants\n\n".to_owned();
    let invariants = data["invariants"].as_array().expect("invariants array");
    if invariants.is_empty() {
        out.push_str("All invariants hold\n\n");
    } else {
        for invariant in invariants {
            let _ = writeln!(
                out,
                "- **{}** ({}): {}",
                cell(invariant.get("kind")),
                invariant["count"],
                invariant["prs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "## Summary\n\n{} repos; {} open; {} approved and green; {} waiting review; {} red; {} overdue.\n\nSnapshot source: {}\n\nSteward tick: {}\n",
        summary["repos"],
        summary["open"],
        summary["approved_green"],
        summary["waiting_review"],
        summary["red"],
        summary["overdue"],
        source_line(&data["steward_snapshots"]),
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

fn parse_issue_target(target: &str) -> Result<(&str, u64), String> {
    let (repo, number) = target
        .rsplit_once('#')
        .ok_or_else(|| "issue must be OWNER/REPO#NUMBER".to_owned())?;
    if repo.split('/').count() != 2 || repo.split('/').any(str::is_empty) {
        return Err("issue must be OWNER/REPO#NUMBER".to_owned());
    }
    let number = number
        .parse()
        .map_err(|_| "issue number must be an integer".to_owned())?;
    Ok((repo, number))
}

fn publish_issue(
    actions: &GitHubActions,
    target: &str,
    body: &str,
    dry_run: bool,
) -> Result<bool, String> {
    if dry_run {
        return Ok(false);
    }
    let (repo, number) = parse_issue_target(target)?;
    let path = format!("repos/{repo}/issues/{number}");
    let current = actions
        .run_gh(&["api".to_owned(), path.clone()])
        .map_err(|e| e.to_string())?;
    let current: Value =
        serde_json::from_str(&current).map_err(|e| format!("issue response was not JSON: {e}"))?;
    if current.get("body").and_then(Value::as_str) == Some(body) {
        return Ok(false);
    }
    actions
        .run_gh(&[
            "api".to_owned(),
            "--method".to_owned(),
            "PATCH".to_owned(),
            path,
            "-f".to_owned(),
            format!("body={body}"),
        ])
        .map_err(|e| e.to_string())?;
    Ok(true)
}

#[cfg(test)]
pub(super) fn run<W: Write>(
    state_dir: &Path,
    format: &str,
    hosts_file: Option<&Path>,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    run_with_options(
        state_dir,
        format,
        hosts_file,
        stdout,
        None,
        RuntimeMode::Shipyard,
        None,
        false,
    )
}

#[allow(clippy::too_many_arguments)] // CLI boundary keeps publication options explicit.
pub(super) fn run_with_options<W: Write>(
    state_dir: &Path,
    format: &str,
    hosts_file: Option<&Path>,
    stdout: &mut W,
    cwd: Option<&Path>,
    mode: RuntimeMode,
    publish: Option<&str>,
    dry_run: bool,
) -> Result<ExitCode, CliFailure> {
    let (snapshots, snapshot_source) = match dwell_cmd::snapshots_for_report(state_dir, None) {
        Ok(snapshots) => (snapshots, json!("available")),
        Err(error) => (Vec::new(), unavailable(error)),
    };
    let data = report_with_snapshot_source(snapshots, hosts_file, &snapshot_source);
    let markdown_body = markdown(&data);
    if let Some(target) = publish {
        let cwd = cwd.ok_or_else(|| CliFailure::new(2, "publishing requires a checkout"))?;
        let actions = GitHubActions::from_cwd(mode, cwd);
        publish_issue(&actions, target, &markdown_body, dry_run)
            .map_err(|error| CliFailure::new(1, format!("publishing fleet report: {error}")))?;
    }
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

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn fixture_snapshots_group_rows_and_owner_actions() {
        let data = report_with_snapshot_source(
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
            &json!("available"),
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
    fn invariants_list_required_and_unknown_kinds_and_empty_state() {
        let data = report_with_snapshot_source(
            vec![json!({"repo":"o/a","at":"now","fleet_rows":[
                {"number":1,"owner":"unknown","blocked_on":"empty_diff"},
                {"number":2,"owner":"daniel","blocked_on":"approved_stuck"}],
                "overdue":[{"pr":3,"kind":"base_stalled"},{"pr":4,"kind":"future_kind"},
                    {"pr":5,"kind":"ci_billing_blocked"},{"pr":6,"kind":"ci_queued_forever"},
                    {"pr":7,"kind":"orphan"}]})],
            None,
            &json!("available"),
        );
        let kinds: Vec<&str> = data["invariants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["kind"].as_str().unwrap())
            .collect();
        for kind in [
            "open_pr_without_owner",
            "empty_diff",
            "approved_stuck",
            "base_stalled",
            "future_kind",
            "ci_billing_blocked",
            "ci_queued_forever",
            "orphan",
        ] {
            assert!(kinds.contains(&kind), "missing {kind}: {kinds:?}");
        }
        assert!(markdown(&data).starts_with("# Fleet report\n\n## Invariants\n"));
        let clean = report_with_snapshot_source(
            vec![json!({"repo":"o/a","fleet_rows":[
            {"number":1,"owner":"daniel","blocked_on":"ready"}],"overdue":[]})],
            None,
            &json!("available"),
        );
        assert!(markdown(&clean).contains("All invariants hold"));
    }

    #[cfg(unix)]
    #[test]
    fn publish_skips_unchanged_body_and_dry_run() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("gh.log");
        let gh = temp.path().join("gh");
        std::fs::write(
            &gh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '{{\"body\":\"same\"}}'\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let actions = GitHubActions::hermetic_for_tests(temp.path(), &gh);
        assert!(!publish_issue(&actions, "o/r#7", "same", false).unwrap());
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
        assert!(!publish_issue(&actions, "o/r#7", "changed", true).unwrap());
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn publish_changed_body_patches_issue_and_reports_patch_errors() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("gh.log");
        let mode = temp.path().join("mode");
        let gh = temp.path().join("gh");
        std::fs::write(&mode, "ok").unwrap();
        std::fs::write(
            &gh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  *'--method PATCH'*)\n    if [ \"$(cat '{}')\" = fail ]; then echo 'patch failed' >&2; exit 9; fi\n    printf '{{\"updated\":true}}'\n    ;;\n  *) printf '{{\"body\":\"old\"}}' ;;\nesac\n",
                log.display(),
                mode.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let actions = GitHubActions::hermetic_for_tests(temp.path(), &gh);

        assert!(publish_issue(&actions, "o/r#7", "new", false).unwrap());
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls
                .lines()
                .any(|line| { line == "api repos/o/r/issues/7" })
        );
        assert!(
            calls
                .lines()
                .any(|line| { line == "api --method PATCH repos/o/r/issues/7 -f body=new" })
        );

        std::fs::write(&mode, "fail").unwrap();
        let error = publish_issue(&actions, "o/r#7", "newer", false).unwrap_err();
        assert!(error.contains("PATCH repos/o/r/issues/7"));
        assert!(error.contains("patch failed"));
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

    #[test]
    fn unavailable_snapshots_are_rendered_without_claiming_zero_is_current() {
        let snapshot_source = unavailable("steward-dwell: directory is unreadable");
        let data = report_with_snapshot_source(Vec::new(), None, &snapshot_source);
        assert_eq!(
            data["steward_snapshots"]["unavailable"],
            "steward-dwell: directory is unreadable"
        );
        assert_eq!(data["summary"]["repos"], 0);
        assert!(
            markdown(&data)
                .contains("Snapshot source: (unavailable: steward-dwell: directory is unreadable)")
        );
    }

    #[test]
    fn completed_pool_child_output_is_read_after_timeout_wait() {
        let mut child = Command::new("sh")
            .args(["-c", "printf '%s' '{\"claude_pool\":{\"ready\":1}}'"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let status = child
            .wait_timeout(Duration::from_secs(3))
            .unwrap()
            .expect("child should complete");
        let mut stdout = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut stdout)
            .unwrap();
        assert_eq!(pool_result(status, &stdout)["ready"], 1);
    }
}
