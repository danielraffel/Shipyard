//! `shipyard runner steward-read-probe`: the steward's hottest reads, timed
//! through `gh` and in process side by side.
//!
//! Each round makes every read once per transport, alternating which goes
//! first so neither always meets a warm server-side cache. Both answers are
//! parsed and compared; a difference is reported by the top-level keys that
//! differ (a check can legitimately move between two reads seconds apart).
//! Nothing a read returns is printed.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::gh_pass::{ApiCostReport, GhPass, LatencyReport};
use crate::github_read::InprocReads;

use super::CliFailure;

/// The per-read timeout the steward's bounded reads use.
const READ_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Serialize)]
struct ProbeReport {
    repo: String,
    pr: u64,
    rounds: u32,
    latency: std::collections::BTreeMap<String, LatencyReport>,
    api_cost: Option<ApiCostReport>,
    per_read: Vec<ReadReport>,
    parity: Parity,
}

#[derive(Serialize)]
struct ReadReport {
    read: &'static str,
    gh_ms: Vec<u64>,
    inproc_ms: Vec<u64>,
}

#[derive(Default, Serialize)]
struct Parity {
    compared: u32,
    equal: u32,
    mismatches: Vec<Mismatch>,
    errors: Vec<String>,
}

#[derive(Serialize)]
struct Mismatch {
    read: &'static str,
    round: u32,
    keys: Vec<String>,
}

#[allow(clippy::too_many_lines)]
pub(in crate::app) fn read_probe_command<W: Write>(
    config: &LoadedConfig,
    cwd: &Path,
    repo: &str,
    pr: u64,
    base: &str,
    rounds: u32,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let Some(slug) = crate::github_read::argv::exact_slug(repo) else {
        return Err(CliFailure::new(2, "--repo must be an exact OWNER/REPO"));
    };
    let (owner, name) = slug.split_once('/').unwrap_or_default();
    let client = crate::gh::GhClient::from_loaded_config(config)
        .map_err(|error| CliFailure::new(1, format!("github.auth: {error}")))?;
    let reads = client
        .command_token_argv()
        .and_then(|argv| InprocReads::for_token_command(argv, None))
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "github.auth.token_command is not the canonical App command; nothing to probe",
            )
        })?;
    let base_actions = GitHubActions::from_loaded_config(cwd, config).with_repo_override(&slug);
    let gh_pass = Arc::new(GhPass::counting());
    let inproc_pass = Arc::new(GhPass::counting());
    let gh = base_actions.clone().with_pass(Arc::clone(&gh_pass));
    let inproc = base_actions
        .with_pass(Arc::clone(&inproc_pass))
        .with_inproc_reads(Some(reads));

    let head = inproc
        .run_gh_with_timeout(
            &strings(&[
                "pr",
                "view",
                &pr.to_string(),
                "--repo",
                &slug,
                "--json",
                "state,headRefOid",
            ]),
            READ_TIMEOUT,
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.get("headRefOid")?.as_str().map(str::to_owned))
        .ok_or_else(|| CliFailure::new(1, format!("could not read the head of #{pr}")))?;

    let probes: Vec<(&'static str, Vec<String>)> = vec![
        (
            "pr view (census fields)",
            strings(&[
                "pr",
                "view",
                &pr.to_string(),
                "--repo",
                &slug,
                "--json",
                "id,number,state,isDraft,baseRefName,headRefOid,headRefName,mergeStateStatus,autoMergeRequest,labels,author,changedFiles,mergedAt",
            ]),
        ),
        (
            "pr view (live fields)",
            strings(&[
                "pr",
                "view",
                &pr.to_string(),
                "--repo",
                &slug,
                "--json",
                super::cancellation_revalidation::LIVE_PR_VIEW_FIELDS,
            ]),
        ),
        (
            "merge-queue policy (GraphQL)",
            strings(&[
                "api",
                "graphql",
                "-f",
                &format!("query={}", super::observation::MERGE_QUEUE_QUERY),
                "-F",
                &format!("owner={owner}"),
                "-F",
                &format!("name={name}"),
                "-F",
                &format!("branch={base}"),
            ]),
        ),
        (
            "commit statuses (REST)",
            strings(&[
                "api",
                &format!("repos/{slug}/commits/{head}/statuses?per_page=100&page=1"),
            ]),
        ),
    ];

    let mut parity = Parity::default();
    let mut per_read = probes
        .iter()
        .map(|(read, _)| ReadReport {
            read,
            gh_ms: Vec::new(),
            inproc_ms: Vec::new(),
        })
        .collect::<Vec<_>>();
    for round in 0..rounds {
        for ((read, args), timings) in probes.iter().zip(per_read.iter_mut()) {
            let time = |actions: &GitHubActions| {
                let started = std::time::Instant::now();
                let answer = actions.run_gh_with_timeout(args, READ_TIMEOUT);
                (
                    answer,
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                )
            };
            let ((gh_answer, gh_ms), (inproc_answer, inproc_ms)) = if round % 2 == 0 {
                let first = time(&gh);
                (first, time(&inproc))
            } else {
                let second = time(&inproc);
                (time(&gh), second)
            };
            timings.gh_ms.push(gh_ms);
            timings.inproc_ms.push(inproc_ms);
            match (gh_answer, inproc_answer) {
                (Ok(gh_raw), Ok(inproc_raw)) => {
                    parity.compared += 1;
                    let keys = differing_keys(&gh_raw, &inproc_raw);
                    if keys.is_empty() {
                        parity.equal += 1;
                    } else {
                        parity.mismatches.push(Mismatch { read, round, keys });
                    }
                }
                (gh_answer, inproc_answer) => parity.errors.push(format!(
                    "{read} round {round}: gh {} / inproc {}",
                    gh_answer.map_or_else(|error| short(error.message()), |_| "ok".to_owned()),
                    inproc_answer.map_or_else(|error| short(error.message()), |_| "ok".to_owned()),
                )),
            }
        }
    }
    let gh_report = gh_pass.report();
    let inproc_report = inproc_pass.report();
    let mut latency = gh_report.latency;
    latency.extend(inproc_report.latency);
    let report = ProbeReport {
        repo: slug,
        pr,
        rounds,
        latency,
        api_cost: inproc_report.api_cost,
        per_read,
        parity,
    };
    serde_json::to_writer_pretty(&mut *stdout, &report)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

/// An error's last 160 characters: the cause, not the argv echo `gh` errors
/// open with.
fn short(message: &str) -> String {
    let chars = message.chars().collect::<Vec<_>>();
    chars[chars.len().saturating_sub(160)..].iter().collect()
}

/// Top-level keys whose values differ, or `<document>` when either side is
/// not JSON or the documents differ below a non-object root.
fn differing_keys(left: &str, right: &str) -> Vec<String> {
    let (Ok(left), Ok(right)) = (
        serde_json::from_str::<Value>(left),
        serde_json::from_str::<Value>(right),
    ) else {
        return vec!["<document>".to_owned()];
    };
    if left == right {
        return Vec::new();
    }
    match (left.as_object(), right.as_object()) {
        (Some(left), Some(right)) => {
            let mut keys = left
                .keys()
                .chain(right.keys())
                .filter(|key| left.get(*key) != right.get(*key))
                .cloned()
                .collect::<Vec<_>>();
            keys.sort();
            keys.dedup();
            keys
        }
        _ => vec!["<document>".to_owned()],
    }
}

#[cfg(test)]
mod tests {
    use super::differing_keys;

    #[test]
    fn differing_keys_names_only_what_changed() {
        assert!(differing_keys(r#"{"a":1,"b":[2]}"#, r#"{"b":[2],"a":1}"#).is_empty());
        assert_eq!(
            differing_keys(r#"{"a":1,"b":2}"#, r#"{"a":1,"b":3,"c":4}"#),
            ["b", "c"]
        );
        assert_eq!(differing_keys("[1]", "[2]"), ["<document>"]);
        assert_eq!(differing_keys("x", "[2]"), ["<document>"]);
    }
}
