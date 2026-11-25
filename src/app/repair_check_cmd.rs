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
    if let Some(id) = edited_latest_verdict(&comments) {
        return Err(CliFailure::new(
            2,
            format!(
                "the newest bot verdict (comment {id}) was edited after it was posted, so its \
                 text is not the reviewer's; refusing to judge the HOLD from it"
            ),
        ));
    }
    let Some((hold_head, hold_body)) = latest_hold(&comments) else {
        return render_no_hold(json_output, stdout);
    };
    let compare = read(format!("repos/{repo}/compare/{hold_head}...{current_head}"))?;
    let changed = compare_files(&compare)?;
    let answers = parse_answers(&args.answers)?;
    let results = check_hold(&hold_body, &changed, &answers)?;
    render(&results, json_output, args.comment_body, stdout)
}

/// Parse a HOLD body and evaluate it; a HOLD with no parseable findings is an
/// error so an unreadable HOLD can never read as repaired.
fn check_hold(
    hold_body: &str,
    changed: &[ChangedFile],
    answers: &BTreeMap<String, String>,
) -> Result<Vec<FindingResult>, CliFailure> {
    let findings = repair_check::parse_findings(hold_body);
    if findings.is_empty() {
        return Err(CliFailure::new(
            2,
            "latest HOLD has no parseable findings; refusing to report it as repaired",
        ));
    }
    Ok(repair_check::evaluate(&findings, changed, answers))
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

/// The id of the newest bot verdict comment unless it is provably unedited:
/// its `created_at` and `updated_at` are both readable RFC 3339 times and
/// equal. Any token with `pull-requests: write` can edit a comment and keep its
/// author, so an edited HOLD can hide or rewrite findings and an edited APPROVE
/// can read as no HOLD. Missing, null, non-string or unparseable edit metadata
/// cannot show the text is the reviewer's, so it is refused the same way; a bot
/// verdict with no readable `created_at` is refused too, because the newest
/// verdict cannot be chosen past it.
fn edited_latest_verdict(comments: &[Value]) -> Option<u64> {
    let id = |comment: &Value| {
        comment
            .get("id")
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    let time = |comment: &Value, key: &str| {
        comment
            .get(key)
            .and_then(Value::as_str)
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
    };
    let verdicts: Vec<&Value> = comments
        .iter()
        .filter(|comment| {
            comment.pointer("/user/login").and_then(Value::as_str) == Some("shipyard-local[bot]")
                && comment
                    .get("body")
                    .and_then(Value::as_str)
                    .and_then(|body| body.lines().rev().find(|line| !line.trim().is_empty()))
                    .is_some_and(|last| last.trim().starts_with("verdict: "))
        })
        .collect();
    if let Some(unreadable) = verdicts.iter().find(|c| time(c, "created_at").is_none()) {
        return Some(id(unreadable));
    }
    // The same choice `latest_hold` makes: the newest by `created_at`.
    let newest = verdicts.into_iter().max_by(|a, b| {
        let at = |c: &Value| {
            c.get("created_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        at(a).cmp(&at(b))
    })?;
    let created = time(newest, "created_at");
    let updated = time(newest, "updated_at");
    (created.is_none() || updated != created).then(|| id(newest))
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

    /// Any token with `pull-requests: write` can edit a comment and keep its
    /// author (merge-queue-rehearsal#47), so an edited newest verdict is not
    /// judged: an APPROVE edited over a HOLD would read as no HOLD.
    #[test]
    fn an_edited_newest_verdict_is_refused() {
        let hold = serde_json::json!({"id": 1, "user":{"login":"shipyard-local[bot]"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z",
            "body":"Findings:\n- [blocking] a.rs 3 r p\nverdict: HOLD head:old"});
        assert_eq!(edited_latest_verdict(std::slice::from_ref(&hold)), None);
        let mut forged = hold.clone();
        forged["body"] = serde_json::json!("ok\nverdict: APPROVE head:old");
        forged["updated_at"] = serde_json::json!("2026-01-01T00:05:00Z");
        assert_eq!(
            latest_hold(std::slice::from_ref(&forged)),
            None,
            "it reads as no HOLD"
        );
        assert_eq!(edited_latest_verdict(&[forged.clone()]), Some(1));
        // The newest verdict decides: an older edited one does not refuse a
        // newer unedited one, and a newer edited one refuses an older one.
        let mut newer = hold.clone();
        newer["id"] = serde_json::json!(2);
        newer["created_at"] = serde_json::json!("2026-01-02T00:00:00Z");
        newer["updated_at"] = serde_json::json!("2026-01-02T00:00:00Z");
        assert_eq!(
            edited_latest_verdict(&[forged.clone(), newer.clone()]),
            None
        );
        let mut newer_edited = newer;
        newer_edited["updated_at"] = serde_json::json!("2026-01-02T00:05:00Z");
        assert_eq!(edited_latest_verdict(&[hold, newer_edited]), Some(2));
    }

    /// Edit metadata that cannot prove the newest verdict is unedited refuses
    /// it: a forged APPROVE whose `updated_at` is missing, null, not a string or
    /// not a time must not read as a closed HOLD.
    #[test]
    fn incomplete_edit_metadata_refuses_the_newest_verdict() {
        let forged = serde_json::json!({"id": 7, "user":{"login":"shipyard-local[bot]"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z",
            "body":"ok\nverdict: APPROVE head:old"});
        assert_eq!(
            edited_latest_verdict(std::slice::from_ref(&forged)),
            None,
            "control"
        );
        for updated in [
            serde_json::Value::Null,
            serde_json::json!(5),
            serde_json::json!("not a time"),
            serde_json::json!(""),
        ] {
            let mut bad = forged.clone();
            bad["updated_at"] = updated.clone();
            assert_eq!(edited_latest_verdict(&[bad]), Some(7), "{updated}");
        }
        let mut missing = forged.clone();
        missing
            .as_object_mut()
            .expect("object")
            .remove("updated_at");
        assert_eq!(edited_latest_verdict(&[missing]), Some(7), "missing");
        // An unreadable created_at on any bot verdict refuses: the newest
        // verdict cannot be chosen past it.
        let mut undated = forged.clone();
        undated["id"] = serde_json::json!(8);
        undated["created_at"] = serde_json::Value::Null;
        assert_eq!(edited_latest_verdict(&[forged, undated]), Some(8));
    }

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
    fn unparseable_hold_is_an_error_not_repaired() {
        let error = check_hold(
            "HOLD, prose only\nverdict: HOLD head:x",
            &[],
            &BTreeMap::new(),
        )
        .expect_err("must fail");
        assert_eq!(error.code, 2);
    }

    #[test]
    fn parseable_hold_is_evaluated() {
        let results =
            check_hold("FINDINGS:\n- garbled bullet", &[], &BTreeMap::new()).expect("parsed");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, Status::Open);
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
