//! Reviewer-quality measurements from merged pull-request comment threads.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::app::CliFailure;
use crate::app::cli::MetricsReviewQualityArgs;
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::gate_cost::ReadCache;
use crate::identity::RuntimeMode;
use crate::output::write_pretty_json;

const READ_TIMEOUT: Duration = Duration::from_mins(1);

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct ReviewerQualityMetrics {
    pub first_verdict_holds: u64,
    pub first_verdicts: u64,
    pub hold_after_hold: u64,
    pub holds: u64,
    pub findings: u64,
    pub repeat_findings: u64,
    pub held_prs: u64,
    pub later_rounds: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct ReviewQualityReport {
    pub schema: &'static str,
    pub days: u64,
    pub repositories: Vec<String>,
    pub reviewers: BTreeMap<String, ReviewerQualityMetrics>,
    pub totals: ReviewerQualityMetrics,
    pub repo_totals: BTreeMap<String, ReviewerQualityMetrics>,
    pub rounds_by_size: BTreeMap<String, ReviewerQualityMetrics>,
    pub reads: ReadCounts,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct ReadCounts {
    pub github: u64,
    pub cached: u64,
}

#[derive(Clone, Debug)]
struct PullRequest {
    number: u64,
}

#[derive(Clone, Debug)]
struct Verdict {
    hold: bool,
    signer: String,
    findings: Vec<Finding>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Finding {
    identity: String,
}

pub(super) fn review_quality_command<W: Write>(
    args: MetricsReviewQualityArgs,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json_output: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let mut report = ReviewQualityReport {
        schema: "shipyard-review-quality/v1",
        days: args.days,
        repositories: args.repos.clone(),
        ..ReviewQualityReport::default()
    };
    let since = i64::try_from(args.days)
        .ok()
        .and_then(chrono::Duration::try_days)
        .and_then(|window| Utc::now().checked_sub_signed(window))
        .ok_or_else(|| CliFailure::new(2, format!("--days {} is out of range", args.days)))?;
    for repo in args.repos {
        let actions = GitHubActions::from_loaded_config(cwd, &config).with_repo_override(&repo);
        let cache = ReadCache::open(
            &state_dir
                .join("review-quality-cache")
                .join(repo.replace('/', "_")),
            SystemTime::now(),
        );
        let mut live_reads = 0_u64;
        let before = report.totals.clone();
        let mut read = |path: String| -> Result<Value, CliFailure> {
            // Only a merged PR's detail is immutable; the closed-PR listing and
            // comment threads change as PRs merge and verdicts are posted, so
            // those are always read live.
            let cacheable = path.split('?').next().is_some_and(|p| {
                p.rsplit('/')
                    .next()
                    .is_some_and(|n| n.parse::<u64>().is_ok())
                    && p.contains("/pulls/")
            });
            let key = format!("review-quality:{path}");
            if cacheable && let Some(value) = cache.get(&key) {
                return Ok(value);
            }
            let args = vec!["api".to_owned(), path.clone()];
            live_reads += 1;
            let text = actions
                .run_gh_with_timeout(&args, READ_TIMEOUT)
                .map_err(|error| {
                    CliFailure::new(1, format!("review-quality read failed: {error}"))
                })?;
            let value: Value = serde_json::from_str(&text).map_err(|error| {
                CliFailure::new(1, format!("review-quality JSON failed: {error}"))
            })?;
            if cacheable {
                cache.put(&key, &value);
            }
            Ok(value)
        };
        let prs = list_merged(&mut read, &repo, since)?;
        for pr in prs {
            let comments = read_all_comments(&mut read, &repo, pr.number)?;
            let verdicts = parse_verdicts(&comments);
            if verdicts.is_empty() {
                continue;
            }
            // The pulls list endpoint omits additions/deletions; only the
            // single-PR GET returns them.
            let detail = read(format!("repos/{repo}/pulls/{}", pr.number))?;
            accumulate(&mut report, &verdicts, changed_lines(&detail));
        }
        let stats = cache.stats();
        report.reads.github += live_reads;
        report.reads.cached += stats.cached;
        report
            .repo_totals
            .insert(repo, delta(&report.totals, &before));
    }
    if json_output {
        write_pretty_json(stdout, &report).map_err(|error| {
            CliFailure::new(1, format!("review-quality output failed: {error}"))
        })?;
    } else {
        writeln!(
            stdout,
            "review quality: {} repositories, {} days",
            report.repositories.len(),
            report.days
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for (signer, metrics) in &report.reviewers {
            writeln!(stdout, "{signer}: first HOLD {}/{}; HOLD-after-HOLD {}/{}; findings/HOLD {:.2}; repeats {}; later rounds {}",
                metrics.first_verdict_holds, metrics.first_verdicts, metrics.hold_after_hold,
                metrics.holds, ratio(metrics.findings, metrics.holds), metrics.repeat_findings, metrics.later_rounds)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn delta(
    after: &ReviewerQualityMetrics,
    before: &ReviewerQualityMetrics,
) -> ReviewerQualityMetrics {
    ReviewerQualityMetrics {
        first_verdict_holds: after
            .first_verdict_holds
            .saturating_sub(before.first_verdict_holds),
        first_verdicts: after.first_verdicts.saturating_sub(before.first_verdicts),
        hold_after_hold: after.hold_after_hold.saturating_sub(before.hold_after_hold),
        holds: after.holds.saturating_sub(before.holds),
        findings: after.findings.saturating_sub(before.findings),
        repeat_findings: after.repeat_findings.saturating_sub(before.repeat_findings),
        held_prs: after.held_prs.saturating_sub(before.held_prs),
        later_rounds: after.later_rounds.saturating_sub(before.later_rounds),
    }
}

fn changed_lines(detail: &Value) -> u64 {
    ["additions", "deletions"]
        .iter()
        .map(|key| detail.get(key).and_then(Value::as_u64).unwrap_or_default())
        .sum()
}

/// Read every page of a PR's issue comments (100 per page) as one array.
fn read_all_comments(
    read: &mut impl FnMut(String) -> Result<Value, CliFailure>,
    repo: &str,
    number: u64,
) -> Result<Value, CliFailure> {
    let mut all = Vec::new();
    for page in 1..=100 {
        let value = read(format!(
            "repos/{repo}/issues/{number}/comments?per_page=100&page={page}"
        ))?;
        let Some(items) = value.as_array() else { break };
        all.extend(items.iter().cloned());
        if items.len() < 100 {
            return Ok(Value::Array(all));
        }
    }
    Err(CliFailure::new(
        1,
        format!(
            "review-quality: PR {number} has more than 10000 comments; refusing a partial read"
        ),
    ))
}

fn list_merged(
    read: &mut impl FnMut(String) -> Result<Value, CliFailure>,
    repo: &str,
    since: DateTime<Utc>,
) -> Result<Vec<PullRequest>, CliFailure> {
    let mut result = Vec::new();
    for page in 1..=100 {
        let value = read(format!(
            "repos/{repo}/pulls?state=closed&sort=updated&direction=desc&per_page=100&page={page}"
        ))?;
        let Some(items) = value.as_array() else { break };
        for item in items {
            // Sorted by updated desc, and merged_at <= updated_at, so once an
            // item was last updated before the window nothing later can have
            // merged inside it.
            if let Some(updated_at) = item
                .get("updated_at")
                .and_then(Value::as_str)
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                && updated_at.with_timezone(&Utc) < since
            {
                return Ok(result);
            }
            let Some(merged_at) = item
                .get("merged_at")
                .and_then(Value::as_str)
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                .map(|date| date.with_timezone(&Utc))
            else {
                continue;
            };
            if merged_at < since {
                continue;
            }
            result.push(PullRequest {
                number: item
                    .get("number")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            });
        }
        if items.len() < 100 {
            return Ok(result);
        }
    }
    Err(CliFailure::new(
        1,
        format!(
            "review-quality: {repo} has more than 10000 closed PRs in the window; refusing a partial read"
        ),
    ))
}

fn parse_verdicts(value: &Value) -> Vec<Verdict> {
    let verdict_re = Regex::new(r"(?im)^\s*verdict:\s*(HOLD|APPROVE)\b").expect("verdict regex");
    let reviewer_re = Regex::new(r"(?im)^\s*reviewer:\s*([^\s]+)").expect("reviewer regex");
    let session_re = Regex::new(r"(?i)<!--\s*session:\s*([^>]+)-->").expect("session regex");
    let finding_re = Regex::new(
        r"(?i)^\s*[-*]\s+(?:\[[^\]]*\]\s+)?`?([\w./-]+\.\w+)`?[:\s]\s*(\d+)\b\s*[-–—:]?\s*(.*)$",
    )
    .expect("finding regex");
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let body = item.get("body").and_then(Value::as_str).unwrap_or_default();
        // Verdicts are posted by the fleet's bot signer; ignore text from
        // any other commenter so it cannot forge reviewer metrics.
        let is_bot = item
            .pointer("/user/login")
            .and_then(Value::as_str)
            .is_some_and(|login| login.ends_with("[bot]"));
        if !is_bot {
            continue;
        }
        let Some(caps) = verdict_re.captures(body) else {
            continue;
        };
        let signer = reviewer_re
            .captures(body)
            .map(|c| c[1].to_owned())
            .or_else(|| {
                session_re.captures(body).map(|c| {
                    c[1].trim()
                        .split('/')
                        .next_back()
                        .unwrap_or("unknown")
                        .to_owned()
                })
            })
            .unwrap_or_else(|| "unknown".to_owned());
        let findings = body
            .lines()
            .filter_map(|line| finding_re.captures(line))
            .map(|c| Finding {
                identity: normalize_finding(&c[1], &c[2], &c[3]),
            })
            .collect();
        out.push(Verdict {
            hold: caps[1].eq_ignore_ascii_case("HOLD"),
            signer,
            findings,
        });
    }
    out
}

fn normalize_finding(path: &str, line: &str, text: &str) -> String {
    let id = Regex::new(r"\b\d+-r\d+-f\d+\b").expect("id regex");
    if let Some(found) = id.find(text) {
        return found.as_str().to_owned();
    }
    format!(
        "{}:{}:{}",
        path.trim(),
        line,
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
    )
}

fn accumulate(report: &mut ReviewQualityReport, verdicts: &[Verdict], lines: u64) {
    if verdicts.is_empty() {
        return;
    }
    let size = size_bucket(lines).to_owned();
    let mut seen = BTreeSet::new();
    let mut previous_hold = false;
    let held = verdicts.iter().filter(|v| v.hold).count() as u64;
    let mut first_seen = BTreeSet::new();
    for verdict in verdicts {
        let entry = report.reviewers.entry(verdict.signer.clone()).or_default();
        if first_seen.insert(verdict.signer.clone()) {
            entry.first_verdicts += 1;
            if verdict.hold {
                entry.first_verdict_holds += 1;
            }
        }
        if verdict.hold {
            entry.holds += 1;
            if previous_hold {
                entry.hold_after_hold += 1;
            }
            entry.findings += verdict.findings.len() as u64;
            entry.repeat_findings += verdict
                .findings
                .iter()
                .filter(|f| !seen.insert(f.identity.clone()))
                .count() as u64;
            previous_hold = true;
        } else {
            previous_hold = false;
        }
    }
    if held > 0 {
        for signer in verdicts
            .iter()
            .filter(|v| v.hold)
            .map(|v| &v.signer)
            .collect::<BTreeSet<_>>()
        {
            let entry = report.reviewers.entry(signer.clone()).or_default();
            entry.held_prs += 1;
            let own = verdicts
                .iter()
                .filter(|v| v.hold && &v.signer == signer)
                .count() as u64;
            entry.later_rounds += own.saturating_sub(1);
        }
    }
    report.totals.first_verdicts += 1;
    report.totals.first_verdict_holds += u64::from(verdicts[0].hold);
    report.totals.holds += held;
    report.totals.hold_after_hold += verdicts
        .windows(2)
        .filter(|pair| pair[0].hold && pair[1].hold)
        .count() as u64;
    report.totals.findings += verdicts
        .iter()
        .filter(|v| v.hold)
        .map(|v| v.findings.len() as u64)
        .sum::<u64>();
    let mut total_seen = BTreeSet::new();
    report.totals.repeat_findings += verdicts
        .iter()
        .filter(|v| v.hold)
        .flat_map(|v| v.findings.iter())
        .fold(0_u64, |count, finding| {
            count + u64::from(!total_seen.insert(finding.identity.clone()))
        });
    report.totals.held_prs += u64::from(held > 0);
    report.totals.later_rounds += held.saturating_sub(1);
    let bucket = report.rounds_by_size.entry(size).or_default();
    bucket.holds += held;
    bucket.hold_after_hold += verdicts
        .windows(2)
        .filter(|pair| pair[0].hold && pair[1].hold)
        .count() as u64;
    bucket.findings += verdicts
        .iter()
        .filter(|verdict| verdict.hold)
        .map(|verdict| verdict.findings.len() as u64)
        .sum::<u64>();
    let mut bucket_seen = BTreeSet::new();
    bucket.repeat_findings += verdicts
        .iter()
        .filter(|verdict| verdict.hold)
        .flat_map(|verdict| verdict.findings.iter())
        .fold(0_u64, |count, finding| {
            count + u64::from(!bucket_seen.insert(finding.identity.clone()))
        });
    bucket.held_prs += u64::from(held > 0);
    bucket.later_rounds += held.saturating_sub(1);
    bucket.first_verdicts += 1;
    bucket.first_verdict_holds += u64::from(verdicts[0].hold);
}

fn size_bucket(lines: u64) -> &'static str {
    match lines {
        0..=100 => "0-100",
        101..=400 => "101-400",
        401..=1_000 => "401-1,000",
        _ => "over 1,000",
    }
}

#[allow(clippy::cast_precision_loss)]
fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_two_round_hold_and_repeat_id() {
        let comments: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/review-quality/two-round-comments.json"
        ))
        .expect("fixture");
        let verdicts = parse_verdicts(&comments);
        let mut report = ReviewQualityReport {
            schema: "x",
            ..Default::default()
        };
        accumulate(&mut report, &verdicts, 1200);
        let m = report.reviewers.get("alice").expect("signer");
        assert_eq!((m.holds, m.hold_after_hold, m.repeat_findings), (2, 1, 1));
        assert_eq!(report.rounds_by_size["over 1,000"].holds, 2);
        assert_eq!(report.totals.hold_after_hold, 1);
    }

    #[test]
    fn session_marker_supplies_signer_and_size_buckets_are_exact() {
        let comments = json!([{"user":{"login":"shipyard-local[bot]"},"body":"<!-- session: host/agent-b -->\nverdict: HOLD\n- a.rs:1 id 7-r1-f1"}]);
        let verdicts = parse_verdicts(&comments);
        let mut report = ReviewQualityReport {
            schema: "x",
            ..Default::default()
        };
        accumulate(&mut report, &verdicts, 401);
        assert!(report.reviewers.contains_key("agent-b"));
        assert!(report.rounds_by_size.contains_key("401-1,000"));
    }

    #[test]
    fn verdict_head_line_and_timestamps_are_not_findings() {
        let comments = json!([{"user":{"login":"shipyard-local[bot]"},"body":"Findings:\n- [blocking] src/a.rs 281 real finding\n- src/b.rs:7 other\nat 10:30:45 today\nreviewer: r\nverdict: HOLD head:1234567890abcdef1234567890abcdef12345678"}]);
        let verdicts = parse_verdicts(&comments);
        assert_eq!(verdicts[0].findings.len(), 2);
    }

    #[test]
    fn changed_lines_comes_from_pr_detail() {
        assert_eq!(
            changed_lines(&json!({"additions": 300, "deletions": 150})),
            450
        );
        assert_eq!(changed_lines(&json!({"number": 1})), 0);
    }

    #[test]
    fn old_merge_recently_updated_does_not_end_the_scan() {
        let since = DateTime::parse_from_rfc3339("2026-11-10T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut read = |_path: String| -> Result<Value, CliFailure> {
            Ok(json!([
                {"number": 1, "merged_at": "2026-10-01T00:00:00Z", "updated_at": "2026-11-19T00:00:00Z"},
                {"number": 2, "merged_at": "2026-11-15T00:00:00Z", "updated_at": "2026-11-15T00:00:00Z"},
                {"number": 3, "merged_at": null, "updated_at": "2026-11-14T00:00:00Z"},
                {"number": 5, "merged_at": "2026-11-13T00:00:00Z", "updated_at": "2026-11-13T00:00:00Z"},
                {"number": 4, "merged_at": "2026-10-02T00:00:00Z", "updated_at": "2026-10-02T00:00:00Z"},
                {"number": 6, "merged_at": "2026-11-12T00:00:00Z", "updated_at": "2026-11-12T00:00:00Z"}
            ]))
        };
        let prs = list_merged(&mut read, "o/r", since).unwrap();
        assert_eq!(prs.iter().map(|p| p.number).collect::<Vec<_>>(), vec![2, 5]);
    }

    #[test]
    fn comments_are_paginated() {
        let mut calls = Vec::new();
        let mut read = |path: String| -> Result<Value, CliFailure> {
            calls.push(path.clone());
            let n = if path.ends_with("page=1") { 100 } else { 3 };
            Ok(Value::Array(
                (0..n).map(|i| json!({"body": format!("c{i}")})).collect(),
            ))
        };
        let all = read_all_comments(&mut read, "o/r", 9).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 103);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn non_bot_commenters_cannot_forge_verdicts() {
        let comments = json!([{"user":{"login":"mallory"},"body":"reviewer: x\nverdict: HOLD"}]);
        assert!(parse_verdicts(&comments).is_empty());
    }

    #[test]
    fn page_cap_fails_closed() {
        let mut read = |_path: String| -> Result<Value, CliFailure> {
            Ok(Value::Array(
                (0..100).map(|_| json!({"body": "x"})).collect(),
            ))
        };
        assert!(read_all_comments(&mut read, "o/r", 9).is_err());
    }

    #[test]
    fn rounds_are_attributed_to_each_signers_own_sequence() {
        let comments = json!([
            {"user":{"login":"b[bot]"},"body":"reviewer: a\nverdict: HOLD"},
            {"user":{"login":"b[bot]"},"body":"reviewer: b\nverdict: HOLD"}
        ]);
        let verdicts = parse_verdicts(&comments);
        let mut report = ReviewQualityReport {
            schema: "x",
            ..Default::default()
        };
        accumulate(&mut report, &verdicts, 10);
        let b = &report.reviewers["b"];
        assert_eq!(
            (b.first_verdicts, b.first_verdict_holds, b.later_rounds),
            (1, 1, 0)
        );
    }
}
