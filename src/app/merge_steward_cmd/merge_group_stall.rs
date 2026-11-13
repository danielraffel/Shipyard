//! Merge groups that can never finish.
//!
//! A merge group waits until every required context reports on its head.
//! When the workflow that should post one fails before posting it, nothing
//! will ever post it: the entry sits `AWAITING_CHECKS` until the ruleset's
//! `check_response_timeout` (six hours on Generous-Corp/vellum) and blocks
//! every entry behind it. On 2026-11-13 Vellum #406's group `df1ddd39` had
//! its "Framework trusted verify" workflow fail at 13:50Z in the
//! `framework-trusted-judge` job, the required `framework-trusted-verify`
//! status was never posted, and #411 and #398 waited 3 h 15 min until the
//! entry was dequeued by hand.
//!
//! **Detection** (always on, report only). For each queue entry with a merge
//! group head ([`RepoObservation::merge_group_heads`]) and no active run on
//! that head in the pass's run census, the group is read: its workflow runs,
//! its check runs and its commit statuses ([`read_group`], three reads). A
//! required context is present when a check run or status of that name
//! exists from the app its rule names ([`present`]). When every run has
//! completed and a required context is still absent ([`judge`]):
//!
//! * the entry is **doomed** at once when a completed run that produces the
//!   context failed. A run produces a context when its workflow name or file
//!   name, normalized, is the context, or when `[base_health.control_workflows]`
//!   names its file for the context;
//! * otherwise it is **stalled** once [`STALL_MINUTES`] have passed since the
//!   last run completed.
//!
//! Either way the row's `blocked_on` becomes `merge_group_stalled:<context>`
//! (counted in the pass summary) and the repository report gets a
//! `merge-group-stalled ...` line. A run still going, or any read that
//! failed, decides nothing; a failed read is reported as unread.
//!
//! **Act** (`[merge_queue] dequeue_doomed_groups`, `off | dry-run | on`,
//! default `dry-run`). A doomed entry only is removed from the queue through
//! the guarded steward dequeue (audit `defect-fix`; a steward removal never
//! counts toward the head's ejection cap), and one comment on the pull
//! request names the context, the failed run and its conclusion. Right before
//! the dequeue the entry is read again: a different PR head, a different group
//! head, or no entry at all refuses the act. Once per (pull request, group
//! head): the decision is kept in the steward ledger. Dequeue only, never
//! close. `dry-run` reports `would_dequeue_doomed_group`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{MutationApplyContext, ObservedPr, PrReport, RepoObservation, StewardLedger};
use crate::base_health::PoisonHold;
use crate::merge_steward::{RequiredCheck, StewardDecision};

/// Minutes after the last run completed before an absent context with no
/// failed producer is reported.
pub(super) const STALL_MINUTES: i64 = 30;
/// The audit action, and the reason the comment gives.
pub(super) const DEQUEUE_AUDIT: &str = "defect-fix";
/// Conclusions that fail a run.
const FAILED: [&str; 4] = ["failure", "timed_out", "startup_failure", "action_required"];

/// One workflow run on a group head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GroupRun {
    pub(super) id: u64,
    /// Workflow name.
    pub(super) name: String,
    /// Workflow file path.
    pub(super) path: String,
    pub(super) status: String,
    pub(super) conclusion: Option<String>,
    pub(super) updated_at: Option<DateTime<Utc>>,
    pub(super) url: Option<String>,
}

/// What a group head carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct GroupFacts {
    pub(super) runs: Vec<GroupRun>,
    /// Check runs: name and producing app.
    pub(super) checks: Vec<(String, Option<u64>)>,
    /// Commit statuses: context and the app its creator acts for.
    pub(super) statuses: Vec<(String, Option<u64>)>,
}

/// The run that should have produced a missing context, and failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct FailedProducer {
    pub(super) context: String,
    pub(super) run_id: u64,
    pub(super) workflow: String,
    pub(super) conclusion: String,
    pub(super) url: Option<String>,
}

/// The verdict on one group head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum Verdict {
    /// A run is still going: nothing to say.
    Running,
    /// Every required context is present.
    Complete,
    /// A context is absent but the wait is still short.
    Watching {
        missing: Vec<String>,
        since: Option<DateTime<Utc>>,
    },
    /// Every run completed and a context is still absent.
    Stalled {
        missing: Vec<String>,
        since: Option<DateTime<Utc>>,
        /// Set when the run that produces a missing context failed.
        #[serde(skip_serializing_if = "Option::is_none")]
        doomed: Option<FailedProducer>,
    },
}

/// Lowercase, every run of non-alphanumerics one `-`.
fn normalize(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

/// Whether `run` produces `context`: its workflow name or file stem,
/// normalized, is the context, or the policy names its file for it.
pub(super) fn produces(
    run: &GroupRun,
    context: &str,
    producers: &BTreeMap<String, String>,
) -> bool {
    let file = run.path.rsplit('/').next().unwrap_or(&run.path);
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    let want = normalize(context);
    normalize(&run.name) == want
        || normalize(stem) == want
        || producers.get(context).is_some_and(|named| named == file)
}

/// Whether `rule`'s context is present on the head, from its app.
pub(super) fn present(facts: &GroupFacts, rule: &RequiredCheck) -> bool {
    let matches = |(name, app): &(String, Option<u64>)| {
        name == &rule.context && rule.app_id.is_none_or(|want| *app == Some(want))
    };
    facts.checks.iter().any(matches) || facts.statuses.iter().any(matches)
}

/// Judge one group head from facts read this pass.
pub(super) fn judge(
    facts: &GroupFacts,
    required: &[RequiredCheck],
    producers: &BTreeMap<String, String>,
    enqueued_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Verdict {
    if facts
        .runs
        .iter()
        .any(|run| !run.status.eq_ignore_ascii_case("completed"))
    {
        return Verdict::Running;
    }
    let missing = required
        .iter()
        .filter(|rule| !present(facts, rule))
        .map(|rule| rule.context.clone())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Verdict::Complete;
    }
    let since = facts
        .runs
        .iter()
        .filter_map(|run| run.updated_at)
        .max()
        .or(enqueued_at);
    let doomed = missing.iter().find_map(|context| {
        facts
            .runs
            .iter()
            .filter(|run| {
                run.conclusion
                    .as_deref()
                    .is_some_and(|conclusion| FAILED.contains(&conclusion))
            })
            .find(|run| produces(run, context, producers))
            .map(|run| FailedProducer {
                context: context.clone(),
                run_id: run.id,
                workflow: run.name.clone(),
                conclusion: run.conclusion.clone().unwrap_or_default(),
                url: run.url.clone(),
            })
    });
    let waited = since.is_some_and(|since| now - since >= chrono::Duration::minutes(STALL_MINUTES));
    if doomed.is_some() || waited {
        Verdict::Stalled {
            missing,
            since,
            doomed,
        }
    } else {
        Verdict::Watching { missing, since }
    }
}

fn read_json(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    path: &str,
) -> Result<Value, String> {
    let raw = run_gh(&["api".to_owned(), path.to_owned()])?;
    serde_json::from_str(&raw).map_err(|error| format!("malformed JSON from `{path}`: {error}"))
}

fn time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|at| at.with_timezone(&Utc))
}

/// The three reads for one group head. Any failure, or a page that may not
/// hold everything, fails the whole reading: a partial reading decides
/// nothing.
pub(super) fn read_group(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    repo: &str,
    sha: &str,
) -> Result<GroupFacts, String> {
    let runs = read_json(
        run_gh,
        &format!("repos/{repo}/actions/runs?head_sha={sha}&per_page=100"),
    )?;
    let rows = runs
        .get("workflow_runs")
        .and_then(Value::as_array)
        .ok_or_else(|| "workflow runs answer had no workflow_runs".to_owned())?;
    if runs.get("total_count").and_then(Value::as_u64).unwrap_or(0) > rows.len() as u64 {
        return Err("more than one page of workflow runs; refusing a partial reading".to_owned());
    }
    let mut facts = GroupFacts::default();
    for row in rows {
        // The filter is GitHub's; re-check it so another commit's run never
        // speaks for this head.
        if !row
            .get("head_sha")
            .and_then(Value::as_str)
            .is_some_and(|head| head.eq_ignore_ascii_case(sha))
        {
            continue;
        }
        let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
        facts.runs.push(GroupRun {
            id: row
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| "a workflow run had no id".to_owned())?,
            name: text("name").unwrap_or_default(),
            path: text("path").unwrap_or_default(),
            status: text("status").unwrap_or_default(),
            conclusion: text("conclusion"),
            updated_at: time(row.get("updated_at")),
            url: text("html_url"),
        });
    }
    let checks = read_json(
        run_gh,
        &format!("repos/{repo}/commits/{sha}/check-runs?per_page=100"),
    )?;
    let rows = checks
        .get("check_runs")
        .and_then(Value::as_array)
        .ok_or_else(|| "check runs answer had no check_runs".to_owned())?;
    if checks
        .get("total_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > rows.len() as u64
    {
        return Err("more than one page of check runs; refusing a partial reading".to_owned());
    }
    facts.checks = rows
        .iter()
        .filter_map(|row| {
            Some((
                row.get("name")?.as_str()?.to_owned(),
                row.pointer("/app/id").and_then(Value::as_u64),
            ))
        })
        .collect();
    let statuses = read_json(
        run_gh,
        &format!("repos/{repo}/commits/{sha}/statuses?per_page=100"),
    )?;
    let rows = statuses
        .as_array()
        .ok_or_else(|| "statuses answer was not a list".to_owned())?;
    if rows.len() >= 100 {
        return Err("a full page of statuses; refusing a partial reading".to_owned());
    }
    facts.statuses = rows
        .iter()
        .filter_map(|row| {
            Some((
                row.get("context")?.as_str()?.to_owned(),
                crate::merge_steward::status_creator_app_id(
                    row.pointer("/creator/login").and_then(Value::as_str),
                    row.pointer("/creator/id").and_then(Value::as_u64),
                ),
            ))
        })
        .collect();
    Ok(facts)
}

/// One queue entry the pass judged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Entry {
    pub(super) pr: u64,
    /// The pull request's head when judged.
    pub(super) pr_head: String,
    /// The merge group head judged.
    pub(super) group_sha: String,
    pub(super) verdict: Verdict,
}

/// This pass's readings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct StallPass {
    pub(super) mode: PoisonHold,
    pub(super) entries: BTreeMap<u64, Entry>,
    /// Entries whose group could not be read, with why.
    pub(super) unread: BTreeMap<u64, String>,
    /// The queue-removals ledger read this pass
    /// ([`super::defect_fix_removals`]).
    pub(super) defect_fix: super::defect_fix_removals::Reading,
}

impl StallPass {
    /// The stalled entry for `pr`, if any.
    pub(super) fn stalled(&self, pr: u64) -> Option<&Entry> {
        self.entries
            .get(&pr)
            .filter(|entry| matches!(entry.verdict, Verdict::Stalled { .. }))
    }

    /// The report lines: one `merge-group-stalled` line per stalled entry,
    /// one per unread entry.
    pub(super) fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for entry in self.entries.values() {
            if let Verdict::Stalled {
                missing,
                since,
                doomed,
            } = &entry.verdict
            {
                lines.push(format!(
                    "merge-group-stalled #{} group {} missing {} since {}{}",
                    entry.pr,
                    short(&entry.group_sha),
                    missing.join(","),
                    since.map_or_else(|| "unknown".to_owned(), |at| at.to_rfc3339()),
                    doomed.as_ref().map_or_else(String::new, |failed| format!(
                        "; doomed: run {} ({}) {}",
                        failed.run_id, failed.workflow, failed.conclusion
                    ))
                ));
            }
        }
        for (pr, error) in &self.unread {
            lines.push(format!(
                "merge-group-stalled #{pr}: group unread, nothing decided: {error}"
            ));
        }
        lines
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Judge every queue entry whose group head has no active run in the census.
/// Reads three times per such entry.
pub(super) fn observe(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    observation: &RepoObservation,
    mode: PoisonHold,
    producers: &BTreeMap<String, String>,
    now: DateTime<Utc>,
) -> StallPass {
    let mut pass = StallPass {
        mode,
        ..StallPass::default()
    };
    if !observation.merge_queue {
        return pass;
    }
    for pr in observation
        .prs
        .iter()
        .filter(|pr| pr.fact.queue_position.is_some())
    {
        let Some(group_sha) = observation.merge_group_heads.get(&pr.fact.number) else {
            continue;
        };
        if observation
            .runs
            .iter()
            .any(|run| run.head_sha.eq_ignore_ascii_case(group_sha))
        {
            continue;
        }
        let enqueued_at = observation
            .merge_group_enqueued_at
            .get(&pr.fact.number)
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc));
        match read_group(run_gh, &observation.repo, group_sha) {
            Ok(facts) => {
                let verdict = judge(
                    &facts,
                    &observation.required_checks,
                    producers,
                    enqueued_at,
                    now,
                );
                pass.entries.insert(
                    pr.fact.number,
                    Entry {
                        pr: pr.fact.number,
                        pr_head: pr.fact.head_sha.clone(),
                        group_sha: group_sha.clone(),
                        verdict,
                    },
                );
            }
            Err(error) => {
                pass.unread.insert(pr.fact.number, error);
            }
        }
    }
    pass
}

/// Mark rows the steward refused because a `defect-fix` removal blocks their
/// head: `blocked_on` `defect_fix_removed:<reason>`, repair lane next.
pub(super) fn annotate_defect_fix(
    reading: &super::defect_fix_removals::Reading,
    repo: &str,
    reports: &mut [PrReport],
) {
    for report in reports.iter_mut() {
        let refused = report.mutation.as_deref().is_some_and(|mutation| {
            mutation.split(',').any(|part| {
                part.starts_with("defect_fix_removed:") || part == "defect_fix_ledger_unjudged"
            })
        });
        // A dry-run pass mutates nothing, so its arm-ready rows are checked
        // directly.
        let would_refuse = matches!(report.decision, StewardDecision::ArmMergeQueue)
            && reading
                .blocking(repo, report.number, &report.head_sha)
                .is_some();
        if !refused && !would_refuse {
            continue;
        }
        let (token, detail) = match reading.blocking(repo, report.number, &report.head_sha) {
            Some(removal) => super::defect_fix_removals::refusal(removal),
            None => (
                "defect_fix_removed:unjudged".to_owned(),
                "the queue-removals ledger could not be read, and this head left the queue after its last good read: held until the ledger reads again".to_owned(),
            ),
        };
        report.blocked.blocked_on = token;
        report.blocked.blocked_detail = Some(detail);
        report.blocked.next_action = Some(super::next_action::NextAction {
            actor: super::next_action::Actor::Repair,
            action:
                "fix the defect and push a new head; this head is not put back in the merge queue"
                    .to_owned(),
            stop: false,
        });
    }
}

/// Mark stalled rows: `blocked_on` `merge_group_stalled:<context>`.
pub(super) fn annotate(pass: &StallPass, reports: &mut [PrReport]) {
    for report in reports.iter_mut() {
        let Some(entry) = pass.stalled(report.number) else {
            continue;
        };
        let Verdict::Stalled {
            missing,
            since,
            doomed,
        } = &entry.verdict
        else {
            continue;
        };
        report.blocked.blocked_on = format!("merge_group_stalled:{}", missing.join(","));
        report.blocked.since = since.map(|at| at.to_rfc3339());
        report.blocked.since_source = Some("merge_group_runs_completed");
        report.blocked.blocked_detail = Some(match doomed {
            Some(failed) => format!(
                "doomed: run {} ({}) concluded {} and never posted `{}` on group {}",
                failed.run_id,
                failed.workflow,
                failed.conclusion,
                failed.context,
                short(&entry.group_sha)
            ),
            None => format!(
                "every run on group {} completed and `{}` never reported",
                short(&entry.group_sha),
                missing.join(",")
            ),
        });
    }
}

/// One dequeue decision, kept so it happens once per (PR, group head).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Receipt {
    /// RFC 3339.
    pub(super) at: String,
    /// `dequeued`, `refused: ...` or `failed: ...`.
    pub(super) outcome: String,
    /// Whether the comment was posted.
    #[serde(default)]
    pub(super) commented: bool,
}

/// The ledger key for one decision.
pub(super) fn receipt_key(repo: &str, pr: u64, group_sha: &str) -> String {
    format!(
        "{}#{pr}@{}",
        repo.to_ascii_lowercase(),
        group_sha.to_ascii_lowercase()
    )
}

/// The comment posted on a dequeued pull request.
pub(super) fn comment_body(entry: &Entry, failed: &FailedProducer) -> String {
    format!(
        "Shipyard removed this pull request from the merge queue (`{DEQUEUE_AUDIT}`): its merge group `{group}` can never finish.\n\n\
The required context `{context}` was never reported on the group, and the workflow that produces it, `{workflow}`, concluded `{conclusion}`{url}. Every other run on the group had completed, so the entry would have waited until the ruleset's check timeout and blocked every entry behind it.\n\n\
Nothing else changed: the pull request is still open and its head `{head}` is unchanged. Fix the failing workflow (or re-run it) and the pull request is admitted again as usual.",
        group = short(&entry.group_sha),
        context = failed.context,
        workflow = failed.workflow,
        conclusion = failed.conclusion,
        url = failed
            .url
            .as_deref()
            .map_or_else(String::new, |url| format!(" ({url})")),
        head = short(&entry.pr_head),
    )
}

/// The acts the dequeue needs, kept apart so the decision is testable.
pub(super) trait Acts {
    /// Re-read the entry: (PR head, merge-group head) as GitHub sees them
    /// now, `None` for the group head when the PR is not queued.
    fn reread(&self, pr: &ObservedPr) -> Result<(String, Option<String>), String>;
    /// Dequeue under the mutation guard, with the audit. `Ok(Some(..))`
    /// when the dequeue landed but something after it (the audit) failed.
    fn dequeue(
        &self,
        pr: &ObservedPr,
        ledger: &mut StewardLedger,
    ) -> Result<Option<String>, String>;
    /// Post the comment.
    fn comment(&self, pr: u64, body: &str) -> Result<(), String>;
}

/// Decide and, under `on`, dequeue a doomed entry. `None` when the act does
/// not apply to `pr` (so the pass evaluates it as usual).
#[allow(clippy::too_many_arguments)] // One entry's facts, the mode and its one actor.
pub(super) fn decide(
    pass: &StallPass,
    apply: bool,
    repo: &str,
    pr: &ObservedPr,
    decision: &StewardDecision,
    ledger: &mut StewardLedger,
    acts: &dyn Acts,
    now: DateTime<Utc>,
) -> Option<PrReport> {
    if !matches!(decision, StewardDecision::Queued { .. }) || pass.mode == PoisonHold::Off {
        return None;
    }
    let entry = pass.stalled(pr.fact.number)?;
    let Verdict::Stalled {
        doomed: Some(failed),
        ..
    } = &entry.verdict
    else {
        return None;
    };
    let report = |mutation: Option<String>, error: Option<String>| PrReport {
        number: pr.fact.number,
        head_sha: pr.fact.head_sha.clone(),
        decision: decision.clone(),
        mutation,
        error,
        blocked: super::blocked_on::Stall::default(),
    };
    let key = receipt_key(repo, pr.fact.number, &entry.group_sha);
    if let Some(done) = ledger.doomed_dequeues.get(&key)
        && !done.outcome.starts_with("failed")
    {
        return Some(report(
            Some(format!("doomed_group_already_decided:{}", done.outcome)),
            None,
        ));
    }
    if pass.mode != PoisonHold::On || !apply {
        return Some(report(
            Some(format!(
                "would_dequeue_doomed_group:{}:{}",
                failed.context,
                short(&entry.group_sha)
            )),
            None,
        ));
    }
    // Bound to the exact PR head and group head this pass judged.
    let (outcome, error) = match acts.reread(pr) {
        Err(error) => (format!("failed: re-read: {error}"), Some(error)),
        Ok((head, _)) if !head.eq_ignore_ascii_case(&entry.pr_head) => {
            (format!("refused: head moved to {}", short(&head)), None)
        }
        Ok((_, group))
            if !group
                .as_deref()
                .is_some_and(|group| group.eq_ignore_ascii_case(&entry.group_sha)) =>
        {
            (
                format!(
                    "refused: merge group is now {}",
                    group.as_deref().map_or("absent", short)
                ),
                None,
            )
        }
        Ok(_) => match acts.dequeue(pr, ledger) {
            Ok(warning) => ("dequeued".to_owned(), warning),
            Err(error) => (format!("failed: {error}"), Some(error)),
        },
    };
    let mut receipt = Receipt {
        at: now.to_rfc3339(),
        outcome: outcome.clone(),
        commented: false,
    };
    let mut error = error;
    if outcome == "dequeued" {
        match acts.comment(pr.fact.number, &comment_body(entry, failed)) {
            Ok(()) => receipt.commented = true,
            Err(comment_error) => {
                let line =
                    format!("dequeued; comment not posted (retried next pass): {comment_error}");
                error = Some(error.map_or(line.clone(), |earlier| format!("{earlier}; {line}")));
            }
        }
    }
    ledger.doomed_dequeues.insert(key, receipt);
    Some(report(
        Some(format!(
            "doomed_group_{}:{}:{}",
            if outcome == "dequeued" {
                "dequeued"
            } else {
                "not_dequeued"
            },
            failed.context,
            short(&entry.group_sha)
        )),
        error.or_else(|| (outcome != "dequeued").then_some(outcome)),
    ))
}

/// Retry the comment of a dequeue whose comment failed, once per pass.
/// Returns report lines.
pub(super) fn retry_comments(
    ledger: &mut StewardLedger,
    repo: &str,
    comment: &dyn Fn(u64, &str) -> Result<(), String>,
) -> Vec<String> {
    let prefix = format!("{}#", repo.to_ascii_lowercase());
    let mut lines = Vec::new();
    for (key, receipt) in ledger.doomed_dequeues.iter_mut().filter(|(key, receipt)| {
        key.starts_with(&prefix) && receipt.outcome == "dequeued" && !receipt.commented
    }) {
        let Some(pr) = key
            .strip_prefix(&prefix)
            .and_then(|rest| rest.split_once('@'))
            .and_then(|(pr, _)| pr.parse::<u64>().ok())
        else {
            continue;
        };
        let body = format!(
            "Shipyard removed this pull request from the merge queue (`{DEQUEUE_AUDIT}`): its merge group could never finish because a required context was never reported. See the steward report for the run."
        );
        match comment(pr, &body) {
            Ok(()) => {
                receipt.commented = true;
                lines.push(format!(
                    "merge-group-stalled #{pr}: comment posted on retry"
                ));
            }
            Err(error) => lines.push(format!(
                "merge-group-stalled #{pr}: comment still not posted: {error}"
            )),
        }
    }
    lines
}

/// No acts: a pass without mutation control decides, never acts.
pub(super) struct NoActs;

impl Acts for NoActs {
    fn reread(&self, _pr: &ObservedPr) -> Result<(String, Option<String>), String> {
        Err("no mutation control".to_owned())
    }
    fn dequeue(
        &self,
        _pr: &ObservedPr,
        _ledger: &mut StewardLedger,
    ) -> Result<Option<String>, String> {
        Err("no mutation control".to_owned())
    }
    fn comment(&self, _pr: u64, _body: &str) -> Result<(), String> {
        Err("no mutation control".to_owned())
    }
}

/// The steward's GitHub acts.
pub(super) struct GitHubActs<'a, 'b> {
    pub(super) context: &'a MutationApplyContext<'b>,
}

impl Acts for GitHubActs<'_, '_> {
    fn reread(&self, pr: &ObservedPr) -> Result<(String, Option<String>), String> {
        let repo = &self.context.observation.repo;
        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| format!("repository `{repo}` is not OWNER/REPO"))?;
        let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid mergeQueueEntry{headCommit{oid}}}}}";
        let raw = self
            .context
            .actions
            .run_gh(&[
                "api".to_owned(),
                "graphql".to_owned(),
                "-f".to_owned(),
                format!("query={query}"),
                "-f".to_owned(),
                format!("owner={owner}"),
                "-f".to_owned(),
                format!("name={name}"),
                "-F".to_owned(),
                format!("number={}", pr.fact.number),
            ])
            .map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
        let node = value
            .pointer("/data/repository/pullRequest")
            .ok_or_else(|| "pull request not in the answer".to_owned())?;
        let head = node
            .get("headRefOid")
            .and_then(Value::as_str)
            .ok_or_else(|| "no headRefOid".to_owned())?
            .to_owned();
        let group = node
            .pointer("/mergeQueueEntry/headCommit/oid")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok((head, group))
    }

    fn dequeue(
        &self,
        pr: &ObservedPr,
        ledger: &mut StewardLedger,
    ) -> Result<Option<String>, String> {
        let guard = super::acquire_pr_mutation_guard(
            self.context.mutation_control,
            self.context.observation,
            pr,
            "runner steward dequeue a merge group that can never finish",
        )?;
        let result = super::unapproved_admission::dequeue_under_guard(
            self.context,
            pr,
            &pr.fact.head_sha,
            ledger,
            DEQUEUE_AUDIT,
        );
        // Record the removal where every steward (and `shipyard who`) reads
        // it, so this head is not put back until it changes.
        let appended = if result.is_ok() {
            super::defect_fix_removals::append(
                &super::defect_fix_removals::ledger_path(),
                &self.context.observation.repo,
                pr.fact.number,
                &pr.fact.head_sha,
                "merge group can never finish: a required context's producing run failed",
                Utc::now(),
            )
            .err()
        } else {
            None
        };
        let outcome = if result.is_ok() {
            "doomed_group_dequeued"
        } else {
            "doomed_group_dequeue_failed"
        };
        let appended = appended.map(|error| format!("queue-removals ledger not written: {error}"));
        match (guard.finish(outcome), result) {
            (Ok(()), result) => result.map(|()| appended),
            (Err(audit), Ok(())) => Ok(Some(format!(
                "dequeued, but the mutation audit failed: {audit}{}",
                appended.map_or_else(String::new, |line| format!("; {line}"))
            ))),
            (Err(audit), Err(error)) => {
                Err(format!("{error}; mutation audit also failed: {audit}"))
            }
        }
    }

    fn comment(&self, pr: u64, body: &str) -> Result<(), String> {
        post_comment(
            self.context.actions,
            &self.context.observation.repo,
            pr,
            body,
        )
    }
}

/// Post one comment.
pub(super) fn post_comment(
    actions: &crate::cloud::GitHubActions,
    repo: &str,
    pr: u64,
    body: &str,
) -> Result<(), String> {
    actions
        .run_gh(&[
            "api".to_owned(),
            "--method".to_owned(),
            "POST".to_owned(),
            format!("repos/{repo}/issues/{pr}/comments"),
            "-f".to_owned(),
            format!("body={body}"),
        ])
        .map(|_| ())
        .map_err(|error| error.to_string())
}
