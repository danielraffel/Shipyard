//! `shipyard repair-check`: refuse a repaired marker while the latest HOLD is open.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::Value;

use super::CliFailure;
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::repair_check::{self, ChangedFile, FindingResult, Status};

const READ_TIMEOUT: Duration = Duration::from_mins(1);

pub(super) struct RepairCheckArgs {
    pub(super) pr: u64,
    pub(super) repo: Option<String>,
    pub(super) answers: Vec<String>,
    pub(super) comment_body: bool,
}

pub(super) fn repair_check_command<W: Write>(
    args: RepairCheckArgs,
    mode: RuntimeMode,
    cwd: &Path,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let repo = args
        .repo
        .filter(|value| !value.trim().is_empty())
        .or_else(|| crate::landability::gate::resolve_repo(&config, cwd))
        .ok_or_else(|| CliFailure::new(2, "No repo detected. Pass --repo OWNER/REPO."))?;
    let actions = GitHubActions::from_loaded_config(cwd, &config).with_repo_override(&repo);
    let read = |path: String| -> Result<Value, CliFailure> {
        let text = actions
            .run_gh_with_timeout(&["api".to_owned(), path.clone()], READ_TIMEOUT)
            .map_err(|error| {
                CliFailure::new(2, format!("repair-check read failed for {path}: {error}"))
            })?;
        serde_json::from_str(&text).map_err(|error| {
            CliFailure::new(2, format!("repair-check JSON failed for {path}: {error}"))
        })
    };
    let pull = read(format!("repos/{repo}/pulls/{}", args.pr))?;
    let current_head = pull
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .ok_or_else(|| CliFailure::new(2, "PR has no readable head sha"))?
        .to_owned();
    let comments = read_comments(&read, &repo, args.pr)?;
    let Some((hold_head, hold_body)) = latest_hold(&comments) else {
        return render_no_hold(json_output, stdout);
    };
    let compare = read(format!("repos/{repo}/compare/{hold_head}...{current_head}"))?;
    let changed = compare_files(&compare)?;
    let answers = parse_answers(&args.answers)?;
    let findings = repair_check::parse_findings(&hold_body);
    if findings.is_empty() {
        return Err(CliFailure::new(
            2,
            "latest HOLD has no parseable findings; refusing to report it as repaired",
        ));
    }
    let results = repair_check::evaluate(&findings, &changed, &answers);
    render(&results, json_output, args.comment_body, stdout)
}

fn read_comments(
    read: &impl Fn(String) -> Result<Value, CliFailure>,
    repo: &str,
    pr: u64,
) -> Result<Vec<Value>, CliFailure> {
    let mut all = Vec::new();
    for page in 1..=100 {
        let value = read(format!(
            "repos/{repo}/issues/{pr}/comments?per_page=100&page={page}"
        ))?;
        let items = value
            .as_array()
            .ok_or_else(|| CliFailure::new(2, "HOLD comments response was not an array"))?;
        all.extend(items.iter().cloned());
        if items.len() < 100 {
            return Ok(all);
        }
    }
    Err(CliFailure::new(
        2,
        "HOLD comments exceeded the 10000-comment safety bound",
    ))
}

/// The newest bot verdict decides: a HOLD followed by an APPROVE (or any later
/// verdict) is not open. The verdict is read from the comment's last non-blank
/// line, the trailer the runner appends, so a verdict-shaped line quoted inside
/// the summary or a finding is ignored.
fn latest_hold(comments: &[Value]) -> Option<(String, String)> {
    let (_, verdict, head, body) = comments
        .iter()
        .filter_map(|comment| {
            if comment.pointer("/user/login").and_then(Value::as_str) != Some("shipyard-local[bot]")
            {
                return None;
            }
            let body = comment.get("body").and_then(Value::as_str)?;
            let last = body.lines().rev().find(|line| !line.trim().is_empty())?;
            let rest = last.trim().strip_prefix("verdict: ")?;
            let (verdict, head) = rest.split_once(" head:")?;
            let head = head.split_whitespace().next()?.to_owned();
            let created = comment
                .get("created_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Some((created, verdict.trim().to_owned(), head, body.to_owned()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))?;
    (verdict == "HOLD").then_some((head, body))
}

fn compare_files(value: &Value) -> Result<Vec<ChangedFile>, CliFailure> {
    let files = value
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| CliFailure::new(2, "compare response has no readable files array"))?;
    files
        .iter()
        .map(|file| {
            let name = file
                .get("filename")
                .and_then(Value::as_str)
                .ok_or_else(|| CliFailure::new(2, "compare file has no filename"))?;
            let patch = file
                .get("patch")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Ok(ChangedFile {
                name: name.to_owned(),
                hunks: repair_check::parse_hunks(patch),
            })
        })
        .collect()
}

fn parse_answers(values: &[String]) -> Result<BTreeMap<String, String>, CliFailure> {
    values
        .iter()
        .map(|value| {
            value
                .split_once('=')
                .map(|(id, reason)| (id.to_owned(), reason.to_owned()))
                .ok_or_else(|| {
                    CliFailure::new(2, format!("--answer must be ID=REASON, got {value:?}"))
                })
                .and_then(|(id, reason)| {
                    if id.is_empty() || reason.trim().is_empty() {
                        Err(CliFailure::new(
                            2,
                            "--answer requires a nonempty id and reason",
                        ))
                    } else {
                        Ok((id, reason))
                    }
                })
        })
        .collect()
}

fn render_no_hold<W: Write>(json_output: bool, stdout: &mut W) -> Result<ExitCode, CliFailure> {
    if json_output {
        serde_json::to_writer(
            &mut *stdout,
            &serde_json::json!({"findings": [], "message": "no open HOLD"}),
        )
        .map_err(|e| CliFailure::new(2, e.to_string()))?;
        writeln!(stdout).ok();
    } else {
        writeln!(stdout, "no open HOLD").map_err(|e| CliFailure::new(2, e.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn render<W: Write>(
    results: &[FindingResult],
    json_output: bool,
    comment_body: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let open = results.iter().any(|result| result.status == Status::Open);
    if json_output {
        serde_json::to_writer(
            &mut *stdout,
            &serde_json::json!({"findings": results, "open": open}),
        )
        .map_err(|e| CliFailure::new(2, e.to_string()))?;
        writeln!(stdout).ok();
    } else {
        if comment_body {
            writeln!(stdout, "repaired:").ok();
        }
        writeln!(stdout, "| id | file:line | status | evidence |").ok();
        writeln!(stdout, "| --- | --- | --- | --- |").ok();
        for result in results {
            writeln!(
                stdout,
                "| {} | {}{} | {} | {} |",
                result.id,
                result.file,
                result.line.map_or(String::new(), |line| format!(":{line}")),
                status_label(&result.status),
                result.evidence
            )
            .map_err(|e| CliFailure::new(2, e.to_string()))?;
        }
    }
    Ok(if open {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn status_label(status: &Status) -> &'static str {
    match status {
        Status::Addressed => "ADDRESSED",
        Status::Answered => "ANSWERED",
        Status::Open => "OPEN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_hold_uses_the_newest_trusted_bot_comment() {
        let comments = vec![
            serde_json::json!({"user":{"login":"shipyard-local[bot]"},"created_at":"2026-01-01T00:00:00Z","body":"verdict: HOLD head:old"}),
            serde_json::json!({"user":{"login":"someone"},"created_at":"2026-01-03T00:00:00Z","body":"verdict: HOLD head:forged"}),
            serde_json::json!({"user":{"login":"shipyard-local[bot]"},"created_at":"2026-01-02T00:00:00Z","body":"verdict: HOLD head:new"}),
        ];
        assert_eq!(
            latest_hold(&comments).map(|(head, _)| head),
            Some("new".to_owned())
        );
    }

    #[test]
    fn a_later_approve_closes_the_hold() {
        let comments = vec![
            serde_json::json!({"user":{"login":"shipyard-local[bot]"},"created_at":"2026-01-01T00:00:00Z","body":"Findings:\n- [blocking] a.rs 3 r p\nverdict: HOLD head:old"}),
            serde_json::json!({"user":{"login":"shipyard-local[bot]"},"created_at":"2026-01-02T00:00:00Z","body":"ok\nverdict: APPROVE head:new"}),
        ];
        assert_eq!(latest_hold(&comments), None);
    }

    #[test]
    fn a_quoted_verdict_line_is_not_the_verdict() {
        let comments = vec![
            serde_json::json!({"user":{"login":"shipyard-local[bot]"},"created_at":"2026-01-01T00:00:00Z","body":"summary quotes verdict: HOLD head:fake\nverdict: HOLD head:fake2\nmore text after"}),
        ];
        assert_eq!(latest_hold(&comments), None);
    }

    #[test]
    fn malformed_compare_is_an_error() {
        assert_eq!(
            compare_files(&serde_json::json!({}))
                .expect_err("must fail")
                .code,
            2
        );
    }

    #[test]
    fn open_unparsed_finding_exits_nonzero() {
        let findings = repair_check::parse_findings("FINDINGS:\n- garbled bullet");
        let results = repair_check::evaluate(&findings, &[], &BTreeMap::new());
        let mut output = Vec::new();
        assert_eq!(
            render(&results, false, false, &mut output).expect("render"),
            ExitCode::from(1)
        );
    }

    #[test]
    fn no_hold_is_success() {
        let mut output = Vec::new();
        assert_eq!(
            render_no_hold(false, &mut output).expect("render"),
            ExitCode::SUCCESS
        );
        assert!(String::from_utf8_lossy(&output).contains("no open HOLD"));
    }
}
