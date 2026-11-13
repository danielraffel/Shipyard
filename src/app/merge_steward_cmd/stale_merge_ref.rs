//! Name a stale merge ref in the census, and retrigger it through
//! `shipyard advance` ([`crate::merge_steward::stale_merge_ref`]).
//!
//! After every row is classified, a row whose token says a required check is
//! red or never ran is re-read as `stale_merge_ref:<contexts>` when the
//! newest required run at its head is older than the base's movement allows,
//! every one of those contexts is green on the base tip (the same checks
//! pass on the base, so the red is the ref's, not the change's), and the
//! approval gate approves that exact head. Nothing is read unless such a row
//! exists: one base-config read (the pass cache answers it), one page of base
//! commits, one tip read per distinct red context (the read red-main triage
//! makes, [`super::base_poison::read_tip`]) and one gate read per stale
//! candidate. A failed read leaves the row as it was.
//!
//! The dwell pass then takes [`crate::merge_steward::dwell::ActKind::RetriggerStaleRef`]
//! on it, by running this binary's own `shipyard advance OWNER/REPO#N
//! --retrigger --head <sha> --json`: every refusal `advance` applies holds,
//! its writes go through `ghapp` and its guards, and it audits the act. The
//! steward never closes a pull request itself.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use wait_timeout::ChildExt as _;

use super::dwell_pass::ActOutcome;
use super::{GitHubActions, ObservedPr, PrReport, RepoObservation, StewardLedger, StewardPolicy};
use crate::merge_steward::stale_merge_ref::{
    self, RetriggerResult, StaleRefPolicy, Staleness, candidate_contexts, newest_required_run,
};

/// The agent id the steward's `advance` child carries, so `advance` can tell
/// it from the author and the approver.
pub(super) const STEWARD_AGENT_ID: &str = "shipyard-merge-steward";
/// How long one `advance` run may take.
const ADVANCE_TIMEOUT: Duration = Duration::from_mins(5);

/// Re-read stale rows of one pass. See the module documentation.
pub(super) fn annotate(
    actions: &GitHubActions,
    observation: &RepoObservation,
    policy: &StewardPolicy,
    ledger: &StewardLedger,
    reports: &mut [PrReport],
) {
    let read_policy = || {
        super::dwell_pass::read_policy(actions, &observation.repo, &observation.base)
            .stale_merge_ref
    };
    let read_commits = |count: u32| base_commit_times(actions, observation, count);
    let run_gh = |argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string());
    let tip_green = |context: &str| {
        let app_id = policy
            .required_checks
            .iter()
            .find(|rule| rule.context.eq_ignore_ascii_case(context))
            .and_then(|rule| rule.app_id);
        let tip = super::base_poison::read_tip(
            &run_gh,
            &observation.repo,
            &observation.base,
            context,
            app_id,
        );
        match tip.error {
            Some(error) => Err(error),
            None => Ok(!tip.red && tip.sha.is_some()),
        }
    };
    let approves = |pr: &ObservedPr| {
        matches!(
            super::pr_mutations::read_head_gate_for_report(actions, observation, pr, ledger),
            Ok((crate::head_approval::HeadGate::Approved { .. }, _, None))
        )
    };
    annotate_with(
        observation,
        policy,
        reports,
        &read_policy,
        &read_commits,
        &tip_green,
        &approves,
    );
}

/// [`annotate`] over injected reads.
pub(super) fn annotate_with(
    observation: &RepoObservation,
    policy: &StewardPolicy,
    reports: &mut [PrReport],
    read_policy: &dyn Fn() -> StaleRefPolicy,
    read_commits: &dyn Fn(u32) -> Result<Vec<DateTime<Utc>>, String>,
    tip_green: &dyn Fn(&str) -> Result<bool, String>,
    approves: &dyn Fn(&ObservedPr) -> bool,
) {
    // One tip read per context per pass, whatever number of rows name it.
    let mut tip_cache: std::collections::BTreeMap<String, Result<bool, String>> =
        std::collections::BTreeMap::new();
    let mut tip = |context: &str| {
        tip_cache
            .entry(context.to_owned())
            .or_insert_with(|| tip_green(context))
            .clone()
    };
    let candidates: Vec<(usize, String, DateTime<Utc>)> = reports
        .iter()
        .enumerate()
        .filter(|(_, report)| !super::pass_budget::is_deferred(report))
        .filter_map(|(index, report)| {
            let contexts = candidate_contexts(&report.blocked.blocked_on)?;
            let pr = find(observation, report)?;
            let runs_at = newest_required_run(&pr.fact.checks, &policy.required_checks)?;
            Some((index, contexts.to_owned(), runs_at))
        })
        .collect();
    if candidates.is_empty() {
        return;
    }
    let stale_policy = read_policy();
    if !stale_policy.enabled() {
        return;
    }
    let Ok(commits) = read_commits(stale_policy.commits_to_read()) else {
        return;
    };
    for (index, contexts, runs_at) in candidates {
        let Some(staleness) = stale_merge_ref::judge(stale_policy, runs_at, &commits) else {
            continue;
        };
        if !stale_merge_ref::all_green_on_tip(&contexts, &mut tip) {
            continue;
        }
        let report = &mut reports[index];
        let Some(pr) = find(observation, report) else {
            continue;
        };
        if !approves(pr) {
            continue;
        }
        rewrite(observation, policy, pr, report, &contexts, staleness);
    }
}

fn find<'a>(observation: &'a RepoObservation, report: &PrReport) -> Option<&'a ObservedPr> {
    observation
        .prs
        .iter()
        .find(|pr| pr.fact.number == report.number)
        .filter(|pr| pr.fact.head_sha.eq_ignore_ascii_case(&report.head_sha))
}

fn rewrite(
    observation: &RepoObservation,
    policy: &StewardPolicy,
    pr: &ObservedPr,
    report: &mut PrReport,
    contexts: &str,
    staleness: Staleness,
) {
    let blocked = &mut report.blocked;
    blocked.blocked_on = stale_merge_ref::token(contexts);
    blocked.blocked_detail = Some(format!(
        "approved; the newest required run at this head is from {}, {} has moved on {} \
         day(s) and at least {} commit(s) since, and the same checks pass on its tip, so the \
         merge ref those checks ran on is stale",
        staleness
            .runs_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        observation.base,
        staleness.lag_days,
        staleness.base_commits_since
    ));
    blocked.since = Some(
        staleness
            .runs_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    blocked.since_source = Some("required_run");
    blocked.next_action = Some(super::next_action::for_row(
        &super::next_action::RowContext {
            repo: &observation.repo,
            base: &observation.base,
            pr: &pr.fact,
            policy,
            error: report.error.as_deref(),
        },
        blocked,
    ));
}

/// The base's commit times, newest first: one page of `count`.
fn base_commit_times(
    actions: &GitHubActions,
    observation: &RepoObservation,
    count: u32,
) -> Result<Vec<DateTime<Utc>>, String> {
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            format!(
                "repos/{}/commits?sha={}&per_page={count}",
                observation.repo,
                crate::required_check_policy::encode_path_segment(&observation.base)
            ),
        ])
        .map_err(|error| error.to_string())?;
    parse_commit_times(&raw)
}

/// Commit times from a `repos/<repo>/commits` page, refusing a malformed one.
pub(super) fn parse_commit_times(raw: &str) -> Result<Vec<DateTime<Utc>>, String> {
    let value: Value = serde_json::from_str(raw.trim()).map_err(|error| error.to_string())?;
    value
        .as_array()
        .ok_or_else(|| "base commits were not an array".to_owned())?
        .iter()
        .map(|commit| {
            commit
                .pointer("/commit/committer/date")
                .and_then(Value::as_str)
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&Utc))
                .ok_or_else(|| "a base commit has no readable committer date".to_owned())
        })
        .collect()
}

/// Run `shipyard advance OWNER/REPO#N --retrigger --head <sha> --json` with
/// this binary, as the steward's own agent, and read what it came to.
pub(super) fn run_advance(repo: &str, pr: &ObservedPr) -> ActOutcome {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return ActOutcome::Deferred(format!("own executable unknown: {error}")),
    };
    let mut command = Command::new(exe);
    command
        .args(advance_args(repo, pr.fact.number, &pr.fact.head_sha))
        .env(crate::head_approval::AGENT_ENV, STEWARD_AGENT_ID)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::queue_arm_gate::strip_queue_authority(&mut command);
    let (exit, stdout, stderr) = match run_bounded(&mut command) {
        Ok(output) => output,
        Err(error) => return ActOutcome::Deferred(format!("advance did not run: {error}")),
    };
    outcome(stale_merge_ref::classify_advance(exit, &stdout, &stderr))
}

/// The `advance` argv, after the program name.
pub(super) fn advance_args(repo: &str, number: u64, head: &str) -> Vec<String> {
    vec![
        "advance".to_owned(),
        format!("{repo}#{number}"),
        "--retrigger".to_owned(),
        "--reason".to_owned(),
        "the steward found its required checks ran on a stale merge ref".to_owned(),
        "--head".to_owned(),
        head.to_owned(),
        "--json".to_owned(),
    ]
}

/// The dwell outcome for an `advance` result.
pub(super) fn outcome(result: RetriggerResult) -> ActOutcome {
    match result {
        RetriggerResult::Done(detail) => ActOutcome::Done(detail),
        RetriggerResult::Blocked(detail) => ActOutcome::Blocked(detail),
        RetriggerResult::Refused(detail) => ActOutcome::Refused(detail),
        RetriggerResult::Deferred(detail) => ActOutcome::Deferred(detail),
        RetriggerResult::Failed(detail) => ActOutcome::Failed(detail),
    }
}

fn run_bounded(command: &mut Command) -> Result<(Option<i32>, String, String), String> {
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let status = match child.wait_timeout(ADVANCE_TIMEOUT) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            // Killed mid-run: it may have closed the pull request.
            return Ok((
                None,
                String::new(),
                format!("timed out after {}s", ADVANCE_TIMEOUT.as_secs()),
            ));
        }
        Err(error) => return Err(error.to_string()),
    };
    Ok((
        status.code(),
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    ))
}
