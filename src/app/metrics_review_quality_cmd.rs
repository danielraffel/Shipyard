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
    lines: u64,
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
    let since = Utc::now() - chrono::Duration::days(args.days.cast_signed());
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
            let key = format!("review-quality:{path}");
            if let Some(value) = cache.get(&key) {
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
            cache.put(&key, &value);
            Ok(value)
        };
        let prs = list_merged(&mut read, &repo, since)?;
        for pr in prs {
            let comments = read(format!(
                "repos/{repo}/issues/{}/comments?per_page=100",
                pr.number
            ))?;
            let verdicts = parse_verdicts(&comments);
            accumulate(&mut report, &verdicts, pr.lines);
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
            let Some(merged_at) = item
                .get("merged_at")
                .and_then(Value::as_str)
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                .map(|date| date.with_timezone(&Utc))
            else {
                continue;
            };
            if merged_at < since {
                return Ok(result);
            }
            result.push(PullRequest {
                number: item
                    .get("number")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                lines: item
                    .get("additions")
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
                    + item
                        .get("deletions")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
            });
        }
        if items.len() < 100 {
            break;
        }
    }
    Ok(result)
}

fn parse_verdicts(value: &Value) -> Vec<Verdict> {
    let verdict_re = Regex::new(r"(?im)^\s*verdict:\s*(HOLD|APPROVE)\b").expect("verdict regex");
    let reviewer_re = Regex::new(r"(?im)^\s*reviewer:\s*([^\s]+)").expect("reviewer regex");
    let session_re = Regex::new(r"(?i)<!--\s*session:\s*([^>]+)-->").expect("session regex");
    let finding_re = Regex::new(r"(?i)([^\s`]+):(\d+)\s*[-–—:]?\s*(.+)").expect("finding regex");
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let body = item.get("body").and_then(Value::as_str).unwrap_or_default();
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
    for (index, verdict) in verdicts.iter().enumerate() {
        let entry = report.reviewers.entry(verdict.signer.clone()).or_default();
        if index == 0 {
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
            entry.later_rounds += held.saturating_sub(1);
        }
    }
    report.totals.first_verdicts += 1;
    report.totals.first_verdict_holds += u64::from(verdicts[0].hold);
    report.totals.holds += held;
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
        let comments = json!([
            {"body":"reviewer: alice\nverdict: HOLD\n- src/lib.rs:10 use thing"},
            {"body":"reviewer: alice\nverdict: HOLD\n- src/lib.rs:10 use thing\n- src/main.rs:20 new"},
            {"body":"reviewer: alice\nverdict: APPROVE"}
        ]);
        let verdicts = parse_verdicts(&comments);
        let mut report = ReviewQualityReport {
            schema: "x",
            ..Default::default()
        };
        accumulate(&mut report, &verdicts, 1200);
        let m = report.reviewers.get("alice").expect("signer");
        assert_eq!((m.holds, m.hold_after_hold, m.repeat_findings), (2, 1, 1));
        assert_eq!(report.rounds_by_size["over 1,000"].holds, 2);
    }

    #[test]
    fn session_marker_supplies_signer_and_size_buckets_are_exact() {
        let comments =
            json!([{"body":"<!-- session: host/agent-b -->\nverdict: HOLD\n- a.rs:1 id 7-r1-f1"}]);
        let verdicts = parse_verdicts(&comments);
        let mut report = ReviewQualityReport {
            schema: "x",
            ..Default::default()
        };
        accumulate(&mut report, &verdicts, 401);
        assert!(report.reviewers.contains_key("agent-b"));
        assert!(report.rounds_by_size.contains_key("401-1,000"));
    }
}
