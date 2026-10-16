//! Deterministic, read-only historical cause advice.

use std::fs;
use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use super::super::CliFailure;
use super::super::cli::CauseAdviseArgs;
use crate::diagnose::cause::SCHEMA;

const ADVICE_SCHEMA: &str = "shipyard.cause-advice/v1";

#[derive(Debug, Serialize)]
struct Output {
    schema: &'static str,
    repo: String,
    head_sha: String,
    query: String,
    candidates: Vec<Candidate>,
    suppressed: usize,
    advisory_only: bool,
}
#[derive(Debug, Serialize)]
struct Candidate {
    cause: Value,
    fix_link: Option<Value>,
    match_score: usize,
    match_fields: Vec<&'static str>,
}
#[derive(Clone)]
struct Window {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    #[allow(dead_code)]
    reason: String,
}

pub(super) fn command<W: Write>(
    args: &CauseAdviseArgs,
    json: bool,
    out: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    if !valid_sha(&args.head_sha) {
        return Err(CliFailure::new(
            2,
            "--head-sha must be a full 40-hex commit SHA",
        ));
    }
    if args.query.trim().is_empty() {
        return Err(CliFailure::new(2, "--query must not be empty"));
    }
    let windows = read_windows(args.outage_windows.as_deref())?;
    let (mut candidates, suppressed) = advise(args, &windows)?;
    candidates.truncate(args.limit);
    let result = Output {
        schema: ADVICE_SCHEMA,
        repo: args.repo.clone(),
        head_sha: args.head_sha.clone(),
        query: args.query.clone(),
        candidates,
        suppressed,
        advisory_only: true,
    };
    if json {
        writeln!(out, "{}", serde_json::to_string(&result).unwrap()).map_err(ioerr)?;
    } else if result.candidates.is_empty() {
        writeln!(
            out,
            "No historical cause matches ({} suppressed by outage windows).",
            result.suppressed
        )
        .map_err(ioerr)?;
    } else {
        if result.suppressed > 0 {
            writeln!(
                out,
                "{} historical candidate(s) suppressed by declared outage windows.",
                result.suppressed
            )
            .map_err(ioerr)?;
        }
        writeln!(out, "score\tjob\tsignature\tclass\tfix").map_err(ioerr)?;
        for c in &result.candidates {
            writeln!(
                out,
                "{}\t{}\t{}\t{}\t{}",
                c.match_score,
                c.cause["job"].as_str().unwrap_or(""),
                c.cause["signature"].as_str().unwrap_or(""),
                c.cause["failure_class"].as_str().unwrap_or(""),
                if c.fix_link.is_some() {
                    "available"
                } else {
                    "none"
                }
            )
            .map_err(ioerr)?;
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn ioerr(e: std::io::Error) -> CliFailure {
    CliFailure::new(1, e.to_string())
}
fn valid_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn tokens(s: &str) -> Vec<String> {
    s.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|x| x.len() >= 2)
        .map(str::to_owned)
        .collect()
}

fn read_windows(path: Option<&Path>) -> Result<Vec<Window>, CliFailure> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let text = fs::read_to_string(path)
        .map_err(|e| CliFailure::new(2, format!("cannot read outage windows: {e}")))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let v: Value = serde_json::from_str(line)
                .map_err(|e| CliFailure::new(2, format!("invalid outage window: {e}")))?;
            let parse = |k| {
                v[k].as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc))
            };
            let (Some(start), Some(end)) = (parse("start"), parse("end")) else {
                return Err(CliFailure::new(
                    2,
                    "outage window needs RFC3339 start and end",
                ));
            };
            if end < start {
                return Err(CliFailure::new(2, "outage window end precedes start"));
            }
            Ok(Window {
                start,
                end,
                reason: v["reason"].as_str().unwrap_or("declared outage").to_owned(),
            })
        })
        .collect()
}

fn advise(
    args: &CauseAdviseArgs,
    windows: &[Window],
) -> Result<(Vec<Candidate>, usize), CliFailure> {
    let causes = args.dir.join("causes");
    let mut rows = Vec::new();
    for entry in fs::read_dir(&causes)
        .map_err(|e| CliFailure::new(2, format!("cannot read causes archive: {e}")))?
    {
        let path = entry.map_err(|e| CliFailure::new(2, e.to_string()))?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let text = fs::read_to_string(path).map_err(|e| CliFailure::new(2, e.to_string()))?;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let row: Value = serde_json::from_str(line)
                .map_err(|e| CliFailure::new(2, format!("invalid cause row: {e}")))?;
            if row["schema"] == SCHEMA {
                rows.push(row);
            }
        }
    }
    let fixlinks = read_fixlinks(&args.dir.join("fixlinks.jsonl"))?;
    let needle = tokens(&args.query);
    let mut out = Vec::new();
    let mut suppressed = 0;
    for row in rows {
        if !safe(&row)
            || row["repo"] != args.repo
            || row["head_sha"].as_str() == Some(args.head_sha.as_str())
            || row["pr"].as_u64() == args.pr
            || row["failure_class"] == "unknown"
            || !matches!(row["confidence"].as_str(), Some("high" | "medium"))
        {
            continue;
        }
        if let Some(at) = row["completed_at"]
            .as_str()
            .or(row["named_at"].as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
        {
            if windows.iter().any(|w| at >= w.start && at <= w.end) {
                suppressed += 1;
                continue;
            }
        }
        let fields = [
            ("signature", row["signature"].as_str().unwrap_or("")),
            ("failed_step", row["failed_step"].as_str().unwrap_or("")),
            ("job", row["job"].as_str().unwrap_or("")),
            ("detail", row["detail"].as_str().unwrap_or("")),
        ];
        let mut score = 0;
        let mut matched = Vec::new();
        for (name, value) in fields {
            let hay = tokens(value);
            let overlap = needle.iter().filter(|t| hay.contains(t)).count();
            if overlap > 0 {
                score += overlap
                    * if name == "signature" {
                        4
                    } else if name == "failed_step" {
                        3
                    } else {
                        1
                    };
                matched.push(name);
            }
        }
        if score == 0 {
            continue;
        }
        let id = format!("{}", row["run_id"]);
        out.push(Candidate {
            fix_link: fixlinks
                .iter()
                .find(|f| {
                    f["repo"] == args.repo
                        && f["red_run_id"] == row["run_id"]
                        && f["red_attempt"].as_u64().unwrap_or(1)
                            == row["attempt"].as_u64().unwrap_or(1)
                })
                .cloned(),
            cause: row,
            match_score: score,
            match_fields: matched,
        });
        let _ = id;
    }
    out.sort_by(|a, b| {
        b.match_score
            .cmp(&a.match_score)
            .then_with(|| {
                b.cause["named_at"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(a.cause["named_at"].as_str().unwrap_or(""))
            })
            .then_with(|| {
                a.cause["run_id"]
                    .to_string()
                    .cmp(&b.cause["run_id"].to_string())
            })
    });
    Ok((out, suppressed))
}

fn read_fixlinks(path: &Path) -> Result<Vec<Value>, CliFailure> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(fs::read_to_string(path)
        .map_err(|e| CliFailure::new(2, e.to_string()))?
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &Value| safe(v))
        .collect())
}
fn safe(v: &Value) -> bool {
    let s = v.to_string();
    !Regex::new(r"(?i)(?:/Users/|/home/|/private/tmp/|\b(?:\d{1,3}\.){3}\d{1,3}\b|github_pat_|\bgh[oprsu]_[A-Za-z0-9]{20,}|(?:token|api[_-]?key|secret|password)\s*[:=])").unwrap().is_match(&s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn args(dir: &Path) -> CauseAdviseArgs {
        CauseAdviseArgs {
            dir: dir.to_path_buf(),
            repo: "o/r".into(),
            head_sha: "a".repeat(40),
            query: "compile".into(),
            pr: Some(9),
            outage_windows: None,
            limit: 5,
        }
    }
    fn row(sha: &str, pr: u64) -> String {
        serde_json::json!({"schema":SCHEMA,"repo":"o/r","head_sha":sha,"pr":pr,"run_id":1,"attempt":1,"job_id":2,"job":"linux","signature":"compile failure","failure_class":"build","confidence":"high","named_at":"2026-10-13T00:00:00Z","completed_at":"2026-10-13T00:00:00Z"}).to_string()
    }
    #[test]
    fn exact_head_and_pr_are_excluded_and_order_is_stable() {
        let d = tempdir().unwrap();
        fs::create_dir(d.path().join("causes")).unwrap();
        fs::write(
            d.path().join("causes/2026-10.jsonl"),
            format!("{}\n{}\n", row(&"b".repeat(40), 8), row(&"a".repeat(40), 9)),
        )
        .unwrap();
        let (got, suppressed) = advise(&args(d.path()), &[]).unwrap();
        assert_eq!(suppressed, 0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].cause["pr"], 8);
    }
    #[test]
    fn unsafe_archive_rows_are_suppressed() {
        let d = tempdir().unwrap();
        fs::create_dir(d.path().join("causes")).unwrap();
        fs::write(
            d.path().join("causes/x.jsonl"),
            row(&"b".repeat(40), 8)
                .replace("compile failure", "/Users/private/token=secret compile"),
        )
        .unwrap();
        assert!(advise(&args(d.path()), &[]).unwrap().0.is_empty());
    }
    #[test]
    fn malformed_head_is_rejected() {
        let mut a = args(Path::new("."));
        a.head_sha = "abc".into();
        let mut out = Vec::new();
        assert_eq!(command(&a, true, &mut out).unwrap_err().code, 2);
    }
}
