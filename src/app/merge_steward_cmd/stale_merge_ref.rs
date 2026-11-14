//! Name a stale merge ref in the census, and retrigger it through
//! `shipyard advance` ([`crate::merge_steward::stale_merge_ref`]).
//!
//! After every row is classified, a `required_failed:<contexts>` row is
//! re-read as `stale_merge_ref:<contexts>` only on evidence that the red was
//! measured on a test merge commit that no longer exists: every red
//! context's failing run is a `pull_request` run of this head whose tested
//! base is not the base of the pull request's current `merge_commit_sha`.
//! Then every one of those contexts must be green on the base tip (the read
//! red-main triage makes, [`super::base_poison::read_tip`]) and the approval
//! gate must approve the exact head. A red on the current merge ref is never
//! stale, however old.
//!
//! Reads: for a red row with an Actions run behind each red context, one
//! pull-request read, one commit read and one run read per red run, at most
//! [`MAX_FRESH_PER_PASS`] pull requests per pass; the verdict is kept in the
//! ledger ([`StaleMemo`]) so the same red is not read again (a "not stale"
//! verdict is re-read after six hours, since GitHub may rebuild the ref).
//! A stale verdict then costs one tip read per context and one gate read.
//! A failed read leaves the row as it was.
//!
//! The dwell pass then takes [`crate::merge_steward::dwell::ActKind::RetriggerStaleRef`]
//! on it, by running this binary's own `shipyard advance OWNER/REPO#N
//! --retrigger --head <sha> --json`: every refusal `advance` applies holds,
//! its writes go through `ghapp` and its guards, and it audits the act. The
//! steward never closes a pull request itself.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use wait_timeout::ChildExt as _;

use super::dwell_pass::ActOutcome;
use super::{GitHubActions, ObservedPr, PrReport, RepoObservation, StewardLedger, StewardPolicy};
use crate::merge_steward::stale_merge_ref::{
    self, MAX_FRESH_PER_PASS, RetriggerResult, StaleMemo, candidate_contexts, red_runs,
};

/// The agent id the steward's `advance` child carries, so `advance` can tell
/// it from the author and the approver.
pub(super) const STEWARD_AGENT_ID: &str = "shipyard-merge-steward";
/// How long one `advance` run may take.
const ADVANCE_TIMEOUT: Duration = Duration::from_mins(5);

/// A read answering a sha, `None` when there is none to answer.
type ShaRead = Result<Option<String>, String>;

/// The reads [`annotate_with`] makes.
pub(super) struct StaleReads<'a> {
    /// The base of the pull request's current test merge commit, or `None`
    /// when it has none at the observed head.
    pub merge_base: &'a dyn Fn(&ObservedPr) -> ShaRead,
    /// The base one run was tested on, or `None` when it is not a
    /// `pull_request` run of this pull request at this head.
    pub run_base: &'a dyn Fn(u64, &ObservedPr) -> ShaRead,
    /// Whether a context is green on the base tip.
    pub tip_green: &'a dyn Fn(&str) -> Result<bool, String>,
    /// Whether the approval gate approves the exact head.
    pub approves: &'a dyn Fn(&ObservedPr) -> bool,
}

/// Re-read stale rows of one pass. See the module documentation.
pub(super) fn annotate(
    actions: &GitHubActions,
    observation: &RepoObservation,
    policy: &StewardPolicy,
    ledger: &mut StewardLedger,
    reports: &mut [PrReport],
) {
    let key = super::dwell_pass::dwell_key(&observation.repo, &observation.base);
    let mut memo = ledger
        .dwell
        .get_mut(&key)
        .map(|state| std::mem::take(&mut state.stale_ref))
        .unwrap_or_default();
    let repo = observation.repo.as_str();
    let json = |path: String| -> Result<Value, String> {
        let raw = actions
            .run_gh(&["api".to_owned(), path])
            .map_err(|error| error.to_string())?;
        serde_json::from_str(raw.trim()).map_err(|error| error.to_string())
    };
    let merge_base = |pr: &ObservedPr| -> Result<Option<String>, String> {
        let live = json(format!("repos/{repo}/pulls/{}", pr.fact.number))?;
        let head = &pr.fact.head_sha;
        if !live
            .pointer("/head/sha")
            .and_then(Value::as_str)
            .is_some_and(|sha| sha.eq_ignore_ascii_case(head))
        {
            return Ok(None);
        }
        let Some(merge) = live.get("merge_commit_sha").and_then(Value::as_str) else {
            return Ok(None);
        };
        let commit = json(format!("repos/{repo}/commits/{merge}"))?;
        Ok(stale_merge_ref::merge_base_of(&commit, head))
    };
    let run_base = |run: u64, pr: &ObservedPr| -> Result<Option<String>, String> {
        let value = json(format!("repos/{repo}/actions/runs/{run}"))?;
        Ok(stale_merge_ref::run_tested_base(
            &value,
            pr.fact.number,
            &pr.fact.head_sha,
        ))
    };
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
    let gate_ledger: &StewardLedger = ledger;
    let approves = |pr: &ObservedPr| {
        matches!(
            super::pr_mutations::read_head_gate_for_report(actions, observation, pr, gate_ledger),
            Ok((crate::head_approval::HeadGate::Approved { .. }, _, None))
        )
    };
    annotate_with(
        observation,
        policy,
        &mut memo,
        Utc::now(),
        reports,
        &StaleReads {
            merge_base: &merge_base,
            run_base: &run_base,
            tip_green: &tip_green,
            approves: &approves,
        },
    );
    ledger.dwell.entry(key).or_default().stale_ref = memo;
}

/// [`annotate`] over injected reads and an explicit memo and time.
pub(super) fn annotate_with(
    observation: &RepoObservation,
    policy: &StewardPolicy,
    memo: &mut BTreeMap<u64, StaleMemo>,
    now: DateTime<Utc>,
    reports: &mut [PrReport],
    reads: &StaleReads<'_>,
) {
    memo.retain(|number, _| observation.prs.iter().any(|pr| pr.fact.number == *number));
    let mut fresh_left = MAX_FRESH_PER_PASS;
    // One tip read per context per pass, whatever number of rows name it.
    let mut tip_cache: BTreeMap<String, Result<bool, String>> = BTreeMap::new();
    let mut tip = |context: &str| {
        tip_cache
            .entry(context.to_owned())
            .or_insert_with(|| (reads.tip_green)(context))
            .clone()
    };
    for report in reports.iter_mut() {
        if super::pass_budget::is_deferred(report) {
            continue;
        }
        let Some(contexts) = candidate_contexts(&report.blocked.blocked_on).map(str::to_owned)
        else {
            continue;
        };
        let Some(pr) = find(observation, report) else {
            continue;
        };
        let Some(runs) = red_runs(&contexts, &pr.fact.checks, &policy.required_checks) else {
            continue;
        };
        let reused = memo
            .get(&pr.fact.number)
            .and_then(|memo| memo.reuse(&pr.fact.head_sha, &runs, now));
        let stale = match reused {
            Some(stale) => stale,
            None if fresh_left == 0 => continue,
            None => {
                fresh_left -= 1;
                let Some(stale) = judge_live(pr, &runs, reads) else {
                    continue;
                };
                memo.insert(
                    pr.fact.number,
                    StaleMemo {
                        head: pr.fact.head_sha.clone(),
                        runs: runs.clone(),
                        stale,
                        at: now,
                    },
                );
                stale
            }
        };
        if stale && stale_merge_ref::all_green_on_tip(&contexts, &mut tip) && (reads.approves)(pr) {
            rewrite(observation, policy, pr, report, &contexts);
        }
    }
}

/// Read whether `runs` were measured on a merge commit that no longer
/// exists; `None` when a read failed (nothing is remembered then).
fn judge_live(pr: &ObservedPr, runs: &[u64], reads: &StaleReads<'_>) -> Option<bool> {
    let Some(current) = (reads.merge_base)(pr).ok()? else {
        return Some(false);
    };
    let tested = runs
        .iter()
        .map(|run| (reads.run_base)(*run, pr))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(stale_merge_ref::measured_on_old_merge(&current, &tested))
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
) {
    let blocked = &mut report.blocked;
    blocked.blocked_on = stale_merge_ref::token(contexts);
    blocked.blocked_detail = Some(format!(
        "approved; {contexts} went red on a test merge commit that no longer exists (the \
         pull request's merge ref has since been rebuilt on a newer {}), and the same checks \
         pass on its tip",
        observation.base
    ));
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
