//! `shipyard overdue` and `shipyard next`: read-only views of the steward's
//! dwell-time limits and next-to-admit ranking.
//!
//! Both read the per-repository snapshot the steward's last `--apply` pass
//! wrote (`<state>/steward-dwell/<repo>.json`, see
//! `merge_steward_cmd::dwell_pass`). Neither reads GitHub: an agent asking
//! "what is overdue" or "what should land next" costs no API budget, and the
//! answer is the one the steward acted on. The snapshot's age is printed, so
//! a stale answer is visible.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::CliFailure;
use super::merge_steward_cmd::dwell_pass::DIR;
use crate::output::write_json_envelope;

/// Which view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum View {
    /// Pull requests past their dwell limit.
    Overdue,
    /// The pull requests closest to mergeable, with the one action each needs.
    Next,
}

/// Every snapshot to show: `repo`'s (any base), or every repository's. A
/// snapshot that cannot be read is skipped with a warning on stderr rather
/// than hiding the others.
fn snapshots(state_dir: &Path, repo: Option<&str>) -> Vec<Value> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(state_dir.join(DIR))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let parsed = std::fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|text| {
                    serde_json::from_str::<Value>(&text).map_err(|error| error.to_string())
                });
            match parsed {
                Ok(value) => Some(value),
                Err(error) => {
                    eprintln!("warning: skipping {}: {error}", path.display());
                    None
                }
            }
        })
        .filter(|value| {
            repo.is_none_or(|repo| {
                value
                    .get("repo")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.eq_ignore_ascii_case(repo))
            })
        })
        .collect()
}

fn age_seconds(snapshot: &Value, now: DateTime<Utc>) -> Option<i64> {
    let at = snapshot.get("at")?.as_str()?;
    let at = DateTime::parse_from_rfc3339(at).ok()?.with_timezone(&Utc);
    Some((now - at).num_seconds().max(0))
}

/// Run `shipyard overdue` or `shipyard next`.
pub(super) fn dwell_view_command<W: Write>(
    view: View,
    state_dir: &Path,
    repo: Option<&str>,
    json: bool,
    now: DateTime<Utc>,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let repo = repo.map(str::trim).filter(|repo| !repo.is_empty());
    let snapshots = snapshots(state_dir, repo);
    let key = match view {
        View::Overdue => "overdue",
        View::Next => "next_to_admit",
    };
    let io = |error: std::io::Error| CliFailure::new(1, error.to_string());
    if json {
        let repos: Vec<Value> = snapshots
            .iter()
            .map(|snapshot| {
                serde_json::json!({
                    "repo": snapshot.get("repo"),
                    "base": snapshot.get("base"),
                    "at": snapshot.get("at"),
                    "age_seconds": age_seconds(snapshot, now),
                    "act_mode": snapshot.get("act_mode"),
                    key: snapshot.get(key).cloned().unwrap_or(Value::Array(Vec::new())),
                })
            })
            .collect();
        let mut data = BTreeMap::new();
        data.insert("source".to_owned(), Value::from("steward-snapshot"));
        data.insert("repos".to_owned(), Value::Array(repos));
        let command = match view {
            View::Overdue => "overdue",
            View::Next => "next",
        };
        write_json_envelope(stdout, command, data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ExitCode::SUCCESS);
    }
    if snapshots.is_empty() {
        writeln!(
            stdout,
            "no steward dwell snapshot{} under {}: the steward writes one on each --apply pass",
            repo.map_or_else(String::new, |repo| format!(" for {repo}")),
            state_dir.join(DIR).display()
        )
        .map_err(io)?;
        return Ok(ExitCode::SUCCESS);
    }
    for snapshot in &snapshots {
        render_text(view, key, snapshot, now, stdout)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// One repository's snapshot as text.
fn render_text<W: Write>(
    view: View,
    key: &str,
    snapshot: &Value,
    now: DateTime<Utc>,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let io = |error: std::io::Error| CliFailure::new(1, error.to_string());
    let name = snapshot.get("repo").and_then(Value::as_str).unwrap_or("?");
    let age = age_seconds(snapshot, now).map_or_else(|| "unknown".to_owned(), |s| format!("{s}s"));
    let rows = snapshot
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    writeln!(
        stdout,
        "{name} (steward pass {age} ago): {} {key}",
        rows.len()
    )
    .map_err(io)?;
    for row in rows {
        let text = |field: &str| {
            row.get(field).map_or_else(String::new, |v| {
                v.as_str().map_or_else(|| v.to_string(), str::to_owned)
            })
        };
        match view {
            View::Overdue => {
                let route = row
                    .pointer("/owner/route")
                    .and_then(Value::as_str)
                    .unwrap_or("feed");
                let act = row
                    .pointer("/act/disposition")
                    .and_then(Value::as_str)
                    .unwrap_or("no_act");
                writeln!(
                    stdout,
                    "  #{} {} {}m (limit {}m, level {}) act={act} route={route}: {}",
                    text("pr"),
                    text("state"),
                    text("age_minutes"),
                    text("limit_minutes"),
                    text("level"),
                    row.pointer("/next_action/action")
                        .and_then(Value::as_str)
                        .unwrap_or("see blocked_on"),
                )
                .map_err(io)?;
            }
            View::Next => {
                writeln!(
                    stdout,
                    "  {}. #{} [{}] needs {} ({}): {}",
                    text("rank"),
                    text("number"),
                    text("tier"),
                    text("needs"),
                    text("actor"),
                    text("action"),
                )
                .map_err(io)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn write(state_dir: &Path, repo: &str, at: &str) {
        let path =
            super::super::merge_steward_cmd::dwell_pass::snapshot_path(state_dir, repo, "main");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            serde_json::json!({
                "schema": 1, "at": at, "repo": repo, "base": "main", "act_mode": "shadow",
                "overdue": [{"pr": 7, "state": "dirty", "age_minutes": 45, "limit_minutes": 30,
                             "level": 1, "owner": {"route": "feed"}, "act": {"disposition": "no_act"},
                             "next_action": {"action": "merge main into it"}}],
                "next_to_admit": [{"rank": 1, "number": 9, "tier": "approved_green",
                                   "needs": "admission by the steward", "actor": "steward",
                                   "action": "nothing to do"}],
            })
            .to_string(),
        )
        .unwrap();
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap()
    }

    #[test]
    fn overdue_and_next_read_the_snapshot_and_print_its_age() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "Owner/Repo", "2026-11-09T11:58:00Z");
        let mut out = Vec::new();
        dwell_view_command(
            View::Overdue,
            temp.path(),
            Some("owner/repo"),
            false,
            now(),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("120s ago"), "{text}");
        assert!(
            text.contains("#7 dirty 45m (limit 30m, level 1) act=no_act route=feed: merge main"),
            "{text}"
        );
        let mut out = Vec::new();
        dwell_view_command(View::Next, temp.path(), None, true, now(), &mut out).unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["command"], "next");
        assert_eq!(value["repos"][0]["next_to_admit"][0]["number"], 9);
        assert_eq!(value["repos"][0]["age_seconds"], 120);
        assert!(value["repos"][0].get("overdue").is_none());
    }

    #[test]
    fn an_unreadable_snapshot_is_skipped_not_fatal() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "Owner/Repo", "2026-11-09T11:58:00Z");
        std::fs::write(temp.path().join(DIR).join("broken.json"), "{").unwrap();
        let mut out = Vec::new();
        dwell_view_command(View::Next, temp.path(), None, true, now(), &mut out).unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["repos"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_missing_snapshot_says_so_without_failing() {
        let temp = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let code = dwell_view_command(
            View::Overdue,
            temp.path(),
            Some("owner/repo"),
            false,
            now(),
            &mut out,
        )
        .unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("no steward dwell snapshot for owner/repo")
        );
    }
}
