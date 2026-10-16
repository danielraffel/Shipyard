//! Deterministic, read-only historical cause advice.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use super::super::CliFailure;
use super::super::cli::CauseAdviseArgs;
use crate::diagnose::cause::SCHEMA;

use super::record::needs_scrub;

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
        writeln!(out, "{}", serde_json::to_string(&result).unwrap())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if result.candidates.is_empty() {
        writeln!(
            out,
            "No historical cause matches ({} suppressed by outage windows).",
            result.suppressed
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        if result.suppressed > 0 {
            writeln!(
                out,
                "{} historical candidate(s) suppressed by declared outage windows.",
                result.suppressed
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        writeln!(out, "score\tjob\tsignature\tclass\tfix")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
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
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
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
            Ok(Window { start, end })
        })
        .collect()
}

fn read_causes(causes: &Path) -> Result<Vec<Value>, CliFailure> {
    let mut rows = Vec::new();
    for entry in fs::read_dir(causes)
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
    Ok(rows)
}

fn advise(
    args: &CauseAdviseArgs,
    windows: &[Window],
) -> Result<(Vec<Candidate>, usize), CliFailure> {
    let rows = read_causes(&args.dir.join("causes"))?;
    let fixlinks = read_fixlinks(&args.dir.join("fixlinks.jsonl"))?;
    let needle = tokens(&args.query);
    let mut out = Vec::new();
    let mut suppressed = 0;
    for row in rows {
        if !safe(&row)
            || row["repo"] != args.repo
            || row["head_sha"].as_str() == Some(args.head_sha.as_str())
            || args.pr.is_some_and(|pr| row["pr"].as_u64() == Some(pr))
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
            && windows.iter().any(|w| at >= w.start && at <= w.end)
        {
            suppressed += 1;
            continue;
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
        out.push(Candidate {
            fix_link: fixlinks
                .iter()
                .find(|f| {
                    let red_run_id = f["red_run_id"].as_u64();
                    let cause_run_id = row["run_id"].as_u64();
                    let red_attempt = f["red_attempt"].as_u64();
                    let cause_attempt = row["attempt"].as_u64();
                    f["repo"] == args.repo
                        && red_run_id.is_some()
                        && red_run_id == cause_run_id
                        && red_attempt.is_some()
                        && red_attempt == cause_attempt
                })
                .cloned(),
            cause: row,
            match_score: score,
            match_fields: matched,
        });
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
            // Multiple jobs and attempts can share a run and timestamp. The
            // complete row is a final key independent of archive file order.
            .then_with(|| a.cause.to_string().cmp(&b.cause.to_string()))
    });
    Ok((out, suppressed))
}

fn read_fixlinks(path: &Path) -> Result<Vec<Value>, CliFailure> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut links: Vec<Value> = fs::read_to_string(path)
        .map_err(|e| CliFailure::new(2, e.to_string()))?
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(safe)
        .collect();
    // If an archive contains several links for one red attempt, choose the
    // same link regardless of append order.
    links.sort_by_cached_key(Value::to_string);
    Ok(links)
}
static SENSITIVE_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:github_pat_|\bgh[oprsu]_[A-Za-z0-9]{20,}|(?:token|api[_-]?key|secret|password)\s*[:=])")
        .expect("sensitive marker pattern compiles")
});

fn safe(value: &Value) -> bool {
    let safe_text = |text: &str| !needs_scrub(text) && !SENSITIVE_MARKER.is_match(text);
    match value {
        Value::String(text) => safe_text(text),
        Value::Array(items) => items.iter().all(safe),
        Value::Object(fields) => fields
            .iter()
            .all(|(key, value)| safe_text(key) && safe(value)),
        _ => true,
    }
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
    fn pr_exclusion_only_applies_when_a_current_pr_is_given() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("causes")).unwrap();
        let numbered: Value = serde_json::from_str(&row(&"b".repeat(40), 9)).unwrap();
        let mut null = numbered.clone();
        null["pr"] = Value::Null;
        let mut missing = numbered.clone();
        missing.as_object_mut().unwrap().remove("pr");
        fs::write(
            dir.path().join("causes/archive.jsonl"),
            format!("{numbered}\n{null}\n{missing}\n"),
        )
        .unwrap();

        let mut query = args(dir.path());
        query.pr = None;
        assert_eq!(advise(&query, &[]).unwrap().0.len(), 3);
        query.pr = Some(9);
        let candidates = advise(&query, &[]).unwrap().0;
        assert_eq!(candidates.len(), 2);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.cause["pr"].is_null())
        );
    }

    #[test]
    fn current_head_and_current_pr_are_excluded_independently() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("causes")).unwrap();
        fs::write(
            dir.path().join("causes/archive.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                row(&"a".repeat(40), 8),
                row(&"b".repeat(40), 9),
                row(&"b".repeat(40), 8),
            ),
        )
        .unwrap();
        let candidates = advise(&args(dir.path()), &[]).unwrap().0;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].cause["head_sha"], "b".repeat(40));
        assert_eq!(candidates[0].cause["pr"], 8);
    }

    #[test]
    fn private_values_in_causes_and_fixlinks_are_never_returned() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("causes")).unwrap();
        let archive = dir.path().join("causes/archive.jsonl");
        let clean: Value = serde_json::from_str(&row(&"b".repeat(40), 8)).unwrap();
        for private in [
            "/Users/private/source.cpp",
            "/Volumes/private/source.cpp",
            "/private/tmp/private/source.cpp",
            "/tmp/private/source.cpp",
            "/home/private/source.cpp",
            "private@example.com",
            "10.0.0.5",
            "Daniels-Mac-Studio-m3-4.local",
            "macmini",
            "m5s",
            "tailscale",
            "ghp_abcdefghijklmnopqrstuv",
            "github_pat_abcdefghijklmnopqrstuv",
            "github_pat_short",
            "GHP_ABCDEFGHIJKLMNOPQRSTUV",
            "password=abcdefghij",
        ] {
            let mut unsafe_row = clean.clone();
            unsafe_row["extra"] = serde_json::json!({"nested": [private]});
            fs::write(&archive, unsafe_row.to_string()).unwrap();
            assert!(
                advise(&args(dir.path()), &[]).unwrap().0.is_empty(),
                "{private}"
            );

            fs::write(&archive, clean.to_string()).unwrap();
            let unsafe_link = serde_json::json!({
                "repo": "o/r", "red_run_id": 1, "red_attempt": 1,
                "extra": {"nested": [private]},
            });
            fs::write(dir.path().join("fixlinks.jsonl"), unsafe_link.to_string()).unwrap();
            let candidates = advise(&args(dir.path()), &[]).unwrap().0;
            assert_eq!(candidates.len(), 1);
            assert!(candidates[0].fix_link.is_none(), "{private}");
        }
    }

    #[test]
    fn scrubbed_text_and_public_provenance_urls_are_safe() {
        let clean = super::super::record::scrub(serde_json::json!({
            "detail": "/tmp/private/source.cpp private@example.com Daniels-Mac-Studio-m3.local",
            "run_url": "https://github.com/o/r/actions/runs/1",
            "job_url": "https://github.com/o/r/actions/runs/1/job/2",
        }));
        assert!(safe(&clean));
        // Advice checks even URL fields from externally supplied archives.
        assert!(!safe(
            &serde_json::json!({"job_url": "https://macmini/private"})
        ));
    }

    #[test]
    fn ranking_and_fixlink_selection_ignore_archive_order() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("causes")).unwrap();
        let mut rows: Vec<Value> = (1..=3)
            .map(|job_id| {
                let mut cause: Value = serde_json::from_str(&row(&"b".repeat(40), 8)).unwrap();
                cause["job_id"] = job_id.into();
                cause
            })
            .collect();
        let links = ["c", "d"].map(|sha| {
            serde_json::json!({
                "repo": "o/r", "red_run_id": 1, "red_attempt": 1,
                "fix_head_sha": sha.repeat(40),
            })
        });
        let mut query = args(dir.path());
        query.limit = 2;
        let mut expected = Vec::new();
        for reverse in [false, true] {
            if reverse {
                rows.reverse();
            }
            // Both files contain candidates tied on every original sort key.
            fs::write(dir.path().join("causes/a.jsonl"), rows[0].to_string()).unwrap();
            fs::write(
                dir.path().join("causes/z.jsonl"),
                format!("{}\n{}\n", rows[1], rows[2]),
            )
            .unwrap();
            let ordered = if reverse {
                [&links[1], &links[0]]
            } else {
                [&links[0], &links[1]]
            };
            fs::write(
                dir.path().join("fixlinks.jsonl"),
                format!("{}\n{}\n", ordered[0], ordered[1]),
            )
            .unwrap();
            let mut output = Vec::new();
            command(&query, true, &mut output).unwrap();
            if reverse {
                assert_eq!(output, expected);
            } else {
                let parsed: Value = serde_json::from_slice(&output).unwrap();
                assert_eq!(parsed["candidates"][0]["cause"]["job_id"], 1);
                assert_eq!(parsed["candidates"][1]["cause"]["job_id"], 2);
                assert_eq!(parsed["candidates"][0]["fix_link"], links[0]);
                expected = output;
            }
        }
    }

    #[test]
    fn fixlinks_without_explicit_run_identity_are_not_attached() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("causes")).unwrap();
        let mut cause: Value = serde_json::from_str(&row(&"b".repeat(40), 8)).unwrap();
        cause["run_id"] = Value::Null;
        cause["attempt"] = Value::Null;
        fs::write(dir.path().join("causes/archive.jsonl"), cause.to_string()).unwrap();
        fs::write(
            dir.path().join("fixlinks.jsonl"),
            serde_json::json!({"repo":"o/r"}).to_string(),
        )
        .unwrap();

        let candidates = advise(&args(dir.path()), &[]).unwrap().0;
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].fix_link.is_none());
    }

    #[test]
    fn malformed_head_is_rejected() {
        let mut a = args(Path::new("."));
        a.head_sha = "abc".into();
        let mut out = Vec::new();
        assert_eq!(command(&a, true, &mut out).unwrap_err().code, 2);
    }
}
