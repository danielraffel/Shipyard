//! The steward's dwell-time limits and next-to-admit ranking, for one
//! repository's pass ([`crate::merge_steward::dwell`],
//! [`crate::merge_steward::next_admit`]).
//!
//! Runs after every row is classified, on the rows and reads the pass already
//! has. The only extra GitHub traffic is an act's own revalidation and
//! mutation, at most once per pull request, state and head: no polling, and
//! nothing proportional to the number of open pull requests. Stacked pull
//! requests are seen through the open-pull-request listing the carried-commit
//! check already read this pass, when it read one.
//!
//! What it writes:
//!
//! * the dwell state, in the steward ledger (`dwell`), so ages survive ticks;
//! * each act's intent, saved to the ledger before the side effect, so a
//!   crash can never repeat it;
//! * each new escalation, appended to the overdue feed
//!   (`<state>/steward-dwell/overdue.jsonl`) for fleet-loop or a model pass,
//!   and named in the tick report so `shipyard relay` delivers it to the
//!   owning session when an owner record exists;
//! * a per-repository snapshot (`<state>/steward-dwell/<repo>.json`) that
//!   `shipyard overdue` and `shipyard next` read without touching GitHub.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use super::carried_commits::OpenPr;
use super::next_action::NextAction;
use super::{
    GitHubActions, MutationControl, ObservedPr, PrReport, RepoObservation, StewardLedger,
    StewardPolicy, StewardPullRequest,
};
use crate::merge_steward::dwell::{
    self, ActKind, ActMode, ActRecord, Actable, Disposition, DwellPolicy, DwellRepoState,
    DwellState, Planned, Sighting,
};
use crate::merge_steward::next_admit::{self, OthersState, RankInput, Ranked};
use crate::merge_steward::selected_required_check;

/// The feed and snapshot directory under the state directory.
pub(crate) const DIR: &str = "steward-dwell";
/// The append-only overdue feed.
pub(crate) const FEED: &str = "overdue.jsonl";
/// Feed size past which it is rotated to `overdue.jsonl.1`.
const FEED_ROTATE_BYTES: u64 = 4 * 1024 * 1024;

/// The ledger key of one repository and base: a steward run for another
/// base never touches this one's dwell state.
pub(super) fn dwell_key(repo: &str, base: &str) -> String {
    format!("{}@{}", repo.to_ascii_lowercase(), base)
}

/// The dwell section of one repository's tick report.
#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct DwellRepoReport {
    /// Whether GitHub-writing acts run (`[auto_merge.dwell] act`).
    pub act_mode: ActMode,
    /// Pull requests with a tracked dwell.
    pub tracked: usize,
    /// Every pull request past its limit, worst first.
    pub overdue: Vec<OverdueRecord>,
    /// The closest to mergeable, with the single thing each needs.
    pub next_to_admit: Vec<Ranked>,
    /// Every ranked pull request, closest first: the order the next pass
    /// evaluates admission candidates in.
    pub ranking: Vec<u64>,
    /// `reused` when the pass's open-pull-request listing showed stacked
    /// pull requests; `not_read_this_pass` otherwise (stacked entries hold).
    pub stack_listing: &'static str,
    /// New escalations this pass emitted.
    pub escalations_new: usize,
    /// Escalations owed but deferred by the per-pass cap.
    pub escalations_deferred: usize,
    /// Why the base config could not be read (the defaults applied).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_read_error: Option<String>,
    /// Dwell failures that did not stop the pass.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// One overdue pull request, as the tick report, the feed and `shipyard
/// overdue` show it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct OverdueRecord {
    /// Always `overdue`.
    pub kind: &'static str,
    /// `OWNER/REPO`.
    pub repo: String,
    /// Pull request number.
    pub pr: u64,
    /// The dwell state.
    pub state: DwellState,
    /// The `blocked_on` token behind it.
    pub blocked_on: String,
    /// The head.
    pub head: String,
    /// When it entered the state.
    pub entered_at: DateTime<Utc>,
    /// Minutes in the state.
    pub age_minutes: i64,
    /// The configured limit.
    pub limit_minutes: u32,
    /// Minutes past the limit.
    pub over_minutes: i64,
    /// Escalation level (1 at the limit, doubling).
    pub level: u32,
    /// Highest level the steward has emitted for this state and head: the
    /// relay delivers a level once it is emitted, even from a later tick.
    pub escalated_level: u32,
    /// The steward's next action for the row, when the census classified it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<NextAction>,
    /// Who should move it.
    pub owner: OwnerRef,
    /// The mechanical act, if any, and what became of it.
    pub act: Disposition,
    /// The act's outcome this pass, when it ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub act_outcome: Option<String>,
    /// Whether this pass emitted a new escalation for it.
    pub escalated: bool,
    /// An escalation owed but deferred by the per-pass cap.
    pub escalation_deferred: bool,
    /// When this record was written.
    pub at: DateTime<Utc>,
}

/// Who an overdue record addresses.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct OwnerRef {
    /// `relay` when an owner record names a session, else `feed`.
    pub route: &'static str,
    /// The role that acts when no session is named (`author`, `reviewer`,
    /// `operator`, `steward`).
    pub role: &'static str,
    /// Owning session's host, from the owner registry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Its cmux surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    /// Its session id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// GitHub author of the pull request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

/// What an act did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ActOutcome {
    /// The side effect happened.
    Done(String),
    /// A live revalidation refused it; nothing was written.
    Refused(String),
    /// It was attempted and GitHub refused it.
    Failed(String),
    /// A precondition could not be read; nothing was attempted.
    Deferred(String),
}

impl ActOutcome {
    fn status(&self) -> &'static str {
        match self {
            Self::Done(_) => "done",
            Self::Refused(_) => "refused",
            Self::Failed(_) => "failed",
            Self::Deferred(_) => "deferred",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Done(detail)
            | Self::Refused(detail)
            | Self::Failed(detail)
            | Self::Deferred(detail) => detail,
        }
    }
}

/// The GitHub side effects dwell acts may take. Deliberately has no arm,
/// enqueue, merge or branch-update method.
pub(super) trait DwellActs {
    /// Re-run the newest red approval-context run, after the approval gate,
    /// read live, admits the exact head.
    fn rerun_verdict(&self, pr: &ObservedPr, ledger: &StewardLedger) -> ActOutcome;
    /// Dispatch `workflows` on the pull request's head branch.
    fn dispatch(&self, pr: &ObservedPr, workflows: &[String]) -> ActOutcome;
    /// Retarget stacked `pr` to the steward's base once its parent merged.
    fn retarget(&self, pr: &OpenPr) -> ActOutcome;
}

/// Everything one repository's dwell step reads.
pub(super) struct DwellInput<'a> {
    pub observation: &'a RepoObservation,
    pub policy: &'a StewardPolicy,
    pub dwell_policy: &'a DwellPolicy,
    /// The pass's open-pull-request listing, when the carried-commit check
    /// read one.
    pub listing: Option<&'a [OpenPr]>,
    /// Steward `--apply`: persist and act.
    pub apply: bool,
    pub ledger_path: &'a Path,
    /// Where owner records and the feed live (`None` in a dry run without a
    /// state directory).
    pub state_dir: Option<&'a Path>,
    pub now: DateTime<Utc>,
    /// Past this instant no GitHub-writing act is taken (`--pass-budget-secs`).
    pub deadline: Option<std::time::Instant>,
}

/// Read `[auto_merge.dwell]` from the base's `.shipyard/config.toml`, through
/// the same argv every base-config read uses, so the pass cache answers it.
pub(super) fn read_policy(actions: &GitHubActions, repo: &str, base: &str) -> DwellPolicy {
    let argv = [
        "api".to_owned(),
        "-H".to_owned(),
        "Accept: application/vnd.github.raw+json".to_owned(),
        format!("repos/{repo}/contents/.shipyard/config.toml?ref={base}"),
    ];
    match actions.run_gh(&argv) {
        Ok(raw) => match raw.parse::<toml::Table>() {
            Ok(table) => DwellPolicy::from_table(&table),
            Err(error) => DwellPolicy {
                read_error: Some(format!(
                    ".shipyard/config.toml on {base} does not parse: {error}"
                )),
                ..DwellPolicy::default()
            },
        },
        Err(error) => {
            let detail = error.to_string();
            if detail.contains("404") || detail.contains("Not Found") {
                DwellPolicy::default()
            } else {
                DwellPolicy {
                    read_error: Some(format!(".shipyard/config.toml on {base}: {detail}")),
                    ..DwellPolicy::default()
                }
            }
        }
    }
}

/// Run the dwell step for one repository: track, rank, act, escalate.
#[allow(clippy::too_many_lines)] // One pass, in order; each step is a call.
pub(super) fn run(
    input: &DwellInput<'_>,
    reports: &[PrReport],
    ledger: &mut StewardLedger,
    acts: &dyn DwellActs,
) -> DwellRepoReport {
    let observation = input.observation;
    let repo_key = dwell_key(&observation.repo, &observation.base);
    let mut report = DwellRepoReport {
        act_mode: input.dwell_policy.act,
        stack_listing: if input.listing.is_some() {
            "reused"
        } else {
            "not_read_this_pass"
        },
        policy_read_error: input.dwell_policy.read_error.clone(),
        ..DwellRepoReport::default()
    };
    let stacked = input
        .listing
        .map(|listing| stacked_on_merged_base(listing, observation))
        .unwrap_or_default();
    let (sightings, skipped) = sightings(reports, &stacked);
    let mut observed: BTreeSet<u64> = observation.prs.iter().map(|pr| pr.fact.number).collect();
    if let Some(listing) = input.listing {
        observed.extend(listing.iter().map(|pr| pr.number));
    }
    let mut state = ledger.dwell.remove(&repo_key).unwrap_or_default();
    dwell::track(
        &mut state,
        &sightings,
        &observed,
        &skipped,
        input.listing.is_some(),
        input.dwell_policy,
        &input.policy.approval_context,
        input.now,
    );
    report.tracked = state.entries.len();

    let ranked = next_admit::rank(&rank_inputs(input, reports, &state));
    report.ranking = ranked.iter().map(|row| row.number).collect();
    report.next_to_admit = ranked
        .iter()
        .take(next_admit::NEXT_LISTED)
        .cloned()
        .collect();
    state.admission_rank.clone_from(&report.ranking);

    let rows = dwell::overdue(&state, input.dwell_policy, input.now);
    let stacked_now: BTreeSet<u64> = stacked.iter().map(|pr| pr.number).collect();
    let actable = |row: &dwell::Overdue, kind: ActKind| {
        if skipped.contains(&row.number) {
            return Actable::NotNow(
                "deferred by the pass budget; not classified this pass".to_owned(),
            );
        }
        if kind.writes_github() && super::pass_budget::deadline_passed(input.deadline) {
            return Actable::NotNow("the pass time budget is spent".to_owned());
        }
        match kind {
            ActKind::DispatchNeverRan
                if workflows_for(&row.token, &input.dwell_policy.dispatch).is_empty() =>
            {
                Actable::Never
            }
            ActKind::RetargetStacked if !stacked_now.contains(&row.number) => {
                Actable::NotNow("stacked pull requests were not listed this pass".to_owned())
            }
            _ => Actable::Yes,
        }
    };
    let mut planned = dwell::plan(&state, rows, input.dwell_policy, input.now, &actable);
    let mut escalations_left = input
        .dwell_policy
        .max_escalations_per_pass
        .saturating_sub(planned.iter().filter(|row| row.escalate).count());
    state.readmit_first.clear();
    let mut outcomes: BTreeMap<u64, String> = BTreeMap::new();
    for row in &mut planned {
        let Disposition::Act { kind } = row.act else {
            continue;
        };
        match take_act(
            input, row, kind, &stacked, &mut state, ledger, acts, &repo_key,
        ) {
            Ok((text, done)) => {
                outcomes.insert(row.overdue.number, text);
                // An act that did not happen is escalated now, not a level
                // later.
                if !done && !row.escalate {
                    let already = state
                        .entries
                        .get(&row.overdue.number)
                        .map_or(0, |entry| entry.escalated_level);
                    let owed = row.overdue.level > already;
                    // Within the same per-pass cap as every other escalation.
                    if owed && escalations_left > 0 {
                        escalations_left -= 1;
                        row.escalate = true;
                    } else if owed {
                        row.escalation_deferred = true;
                    }
                }
            }
            Err(error) => report.errors.push(error),
        }
    }
    if input.apply {
        dwell::mark_escalated(&mut state, &planned);
    }
    report.escalations_new = planned.iter().filter(|row| row.escalate).count();
    report.escalations_deferred = planned.iter().filter(|row| row.escalation_deferred).count();
    let by_number: BTreeMap<u64, &PrReport> = reports
        .iter()
        .map(|report| (report.number, report))
        .collect();
    report.overdue = planned
        .iter()
        .map(|row| {
            // A seeded level was never emitted, so the relay must not see it.
            let escalated_level = state.entries.get(&row.overdue.number).map_or(0, |entry| {
                if entry.escalated_level > entry.seeded_level {
                    entry.escalated_level
                } else {
                    0
                }
            });
            overdue_record(
                input,
                by_number.get(&row.overdue.number).copied(),
                row,
                escalated_level,
                outcomes.get(&row.overdue.number).cloned(),
            )
        })
        .collect();
    ledger.dwell.insert(repo_key, state);
    if input.apply
        && let Some(state_dir) = input.state_dir
        && let Err(error) =
            append_feed(state_dir, report.overdue.iter().filter(|row| row.escalated))
    {
        report.errors.push(error);
    }
    report
}

/// Pull requests stacked on a base whose pull request is no longer open: an
/// open pull request on another branch, labelled `stacked:<n>`, whose parent
/// `#n` is not open and whose base branch no open pull request heads.
fn stacked_on_merged_base(listing: &[OpenPr], observation: &RepoObservation) -> Vec<OpenPr> {
    let base = observation.base.as_str();
    let census = observation.prs.iter().map(|pr| &pr.fact);
    let open: BTreeSet<u64> = listing
        .iter()
        .map(|pr| pr.number)
        .chain(census.clone().map(|pr| pr.number))
        .collect();
    let heads: BTreeSet<&str> = listing
        .iter()
        .map(|pr| pr.head_ref.as_str())
        .chain(census.map(|pr| pr.head_branch.as_str()))
        .collect();
    listing
        .iter()
        .filter(|pr| !pr.base_ref.eq_ignore_ascii_case(base) && !pr.cross_repository)
        .filter(|pr| pr.stacked_on.is_some_and(|parent| !open.contains(&parent)))
        .filter(|pr| !heads.contains(pr.base_ref.as_str()))
        .cloned()
        .collect()
}

/// The pass's sightings, and the rows it saw but did not classify (deferred
/// by the pass budget), whose entries hold.
fn sightings(reports: &[PrReport], stacked: &[OpenPr]) -> (Vec<Sighting>, BTreeSet<u64>) {
    let mut skipped = BTreeSet::new();
    let mut out = Vec::new();
    for report in reports {
        if super::pass_budget::is_deferred(report) || report.blocked.blocked_on.is_empty() {
            skipped.insert(report.number);
            continue;
        }
        out.push(Sighting {
            number: report.number,
            head: report.head_sha.clone(),
            token: report.blocked.blocked_on.clone(),
            since: report
                .blocked
                .since
                .as_deref()
                .and_then(|since| DateTime::parse_from_rfc3339(since).ok())
                .map(|since| since.with_timezone(&Utc)),
        });
    }
    out.extend(stacked.iter().map(|pr| Sighting {
        number: pr.number,
        head: pr.head_sha.clone(),
        token: dwell::STACKED_BASE_MERGED.to_owned(),
        since: None,
    }));
    (out, skipped)
}

/// Required contexts a `required_never_ran:<a,b>` token names that have a
/// mapped workflow, as their distinct workflow files.
fn workflows_for(token: &str, dispatch: &BTreeMap<String, String>) -> Vec<String> {
    let Some(contexts) = token.strip_prefix("required_never_ran:") else {
        return Vec::new();
    };
    let mut workflows: Vec<String> = contexts
        .split(',')
        .filter_map(|context| dispatch.get(context.trim()).cloned())
        .collect();
    workflows.sort();
    workflows.dedup();
    workflows
}

/// Take one planned act: record the intent durably, then the side effect,
/// then the outcome. Returns the outcome text and whether the act happened,
/// or an error that left nothing done.
#[allow(clippy::too_many_arguments)]
fn take_act(
    input: &DwellInput<'_>,
    row: &Planned,
    kind: ActKind,
    stacked: &[OpenPr],
    state: &mut DwellRepoState,
    ledger: &mut StewardLedger,
    acts: &dyn DwellActs,
    repo_key: &str,
) -> Result<(String, bool), String> {
    let number = row.overdue.number;
    let record = |status: &str, detail: Option<String>| ActRecord {
        kind,
        state: row.overdue.state,
        head: row.overdue.head.clone(),
        status: status.to_owned(),
        at: input.now,
        detail,
        attempts: 1,
    };
    if kind == ActKind::Readmit {
        // Reordering the steward's own next pass: the admission path keeps
        // every gate it has, and nothing here arms or enqueues.
        state.readmit_first.push(number);
        state.record_act(
            number,
            record(
                "done",
                Some("evaluated first in the next admission pass".to_owned()),
            ),
        );
        return Ok(("readmit: evaluated first next pass".to_owned(), true));
    }
    if !input.apply {
        return Ok((format!("{}: not taken (dry run)", kind.key()), true));
    }
    // Each act re-checks the pass budget: a gate read and a live read each
    // can take a while.
    if super::pass_budget::deadline_passed(input.deadline) {
        return Ok((
            format!("{}: not taken (the pass time budget is spent)", kind.key()),
            true,
        ));
    }
    // Write-ahead: the intent is durable before the side effect, so a crash
    // after it can never take the act a second time. If it cannot be saved,
    // nothing is recorded and the act is retried next pass.
    let mut staged = state.clone();
    staged.record_act(number, record("intent", None));
    ledger.dwell.insert(repo_key.to_owned(), staged.clone());
    let saved = super::save_ledger(input.ledger_path, ledger);
    ledger.dwell.remove(repo_key);
    saved.map_err(|error| {
        format!(
            "#{number} {}: intent not persisted, act skipped: {}",
            kind.key(),
            error.message
        )
    })?;
    *state = staged;
    let outcome = match kind {
        ActKind::RerunVerdictCheck | ActKind::DispatchNeverRan => {
            match input
                .observation
                .prs
                .iter()
                .find(|pr| pr.fact.number == number)
                .filter(|pr| pr.fact.head_sha.eq_ignore_ascii_case(&row.overdue.head))
            {
                None => ActOutcome::Refused("head moved since it was observed".to_owned()),
                Some(pr) if kind == ActKind::RerunVerdictCheck => acts.rerun_verdict(pr, ledger),
                // The token the entry was tracked under, never this pass's
                // report row (a deferred row has none).
                Some(pr) => acts.dispatch(
                    pr,
                    &workflows_for(&row.overdue.token, &input.dwell_policy.dispatch),
                ),
            }
        }
        ActKind::RetargetStacked => match stacked.iter().find(|pr| pr.number == number) {
            Some(pr) if pr.head_sha.eq_ignore_ascii_case(&row.overdue.head) => acts.retarget(pr),
            _ => ActOutcome::Deferred("not seen stacked on a merged base this pass".to_owned()),
        },
        ActKind::Readmit => unreachable!("handled above"),
    };
    let text = format!(
        "{}: {} ({})",
        kind.key(),
        outcome.status(),
        outcome.detail()
    );
    // `done` is final at this head. Anything else is retried a bounded number
    // of times with backoff (`dwell::retry_due`) and escalates meanwhile.
    state.record_act(
        number,
        record(outcome.status(), Some(outcome.detail().to_owned())),
    );
    Ok((text, matches!(outcome, ActOutcome::Done(_))))
}

fn overdue_record(
    input: &DwellInput<'_>,
    report: Option<&PrReport>,
    row: &Planned,
    escalated_level: u32,
    act_outcome: Option<String>,
) -> OverdueRecord {
    let number = row.overdue.number;
    let role = row.overdue.state.escalation_role();
    let author = input
        .observation
        .prs
        .iter()
        .find(|pr| pr.fact.number == number)
        .and_then(|pr| pr.fact.author.clone());
    // Only the author's own states go to the owning (author) session; review,
    // CI and steward states go to the feed, where the party that can act
    // reads them.
    let registered = (role == "author")
        .then(|| {
            input.state_dir.and_then(|dir| {
                crate::owner_registry::load(dir, &input.observation.repo, number)
                    .filter(|record| !record.released)
            })
        })
        .flatten();
    let owner = OwnerRef {
        route: if registered.is_some() {
            "relay"
        } else {
            "feed"
        },
        role,
        host: registered.as_ref().map(|record| record.host.clone()),
        surface: registered
            .as_ref()
            .and_then(|record| record.surface.clone()),
        session: registered
            .as_ref()
            .and_then(|record| record.session.clone()),
        author,
    };
    OverdueRecord {
        kind: "overdue",
        repo: input.observation.repo.clone(),
        pr: number,
        state: row.overdue.state,
        blocked_on: row.overdue.token.clone(),
        head: row.overdue.head.clone(),
        entered_at: row.overdue.entered_at,
        age_minutes: row.overdue.age_minutes,
        limit_minutes: row.overdue.limit_minutes,
        over_minutes: row.overdue.over_minutes,
        level: row.overdue.level,
        escalated_level,
        next_action: report.and_then(|report| report.blocked.next_action.clone()),
        owner,
        act: row.act.clone(),
        act_outcome,
        escalated: row.escalate,
        escalation_deferred: row.escalation_deferred,
        at: input.now,
    }
}

/// The ranking inputs: every census row, read from facts the pass has.
fn rank_inputs(
    input: &DwellInput<'_>,
    reports: &[PrReport],
    state: &DwellRepoState,
) -> Vec<RankInput> {
    let edges = dependency_edges(input, reports);
    let unblocks = next_admit::unblock_counts(&edges);
    let census: BTreeMap<u64, &ObservedPr> = input
        .observation
        .prs
        .iter()
        .map(|pr| (pr.fact.number, pr))
        .collect();
    reports
        .iter()
        .filter_map(|report| {
            let pr = *census.get(&report.number)?;
            let token = report.blocked.blocked_on.as_str();
            let age_minutes = state
                .entries
                .get(&report.number)
                .map(|entry| entry.entered_at)
                .or_else(|| {
                    report
                        .blocked
                        .since
                        .as_deref()
                        .and_then(|since| DateTime::parse_from_rfc3339(since).ok())
                        .map(|since| since.with_timezone(&Utc))
                })
                .map(|since| (input.now - since).num_minutes().max(0));
            let next = report.blocked.next_action.as_ref();
            Some(RankInput {
                number: report.number,
                token: token.to_owned(),
                others: others_state(&pr.fact, input.policy),
                approved: token == "ready" || approval_green(&pr.fact, input.policy),
                blocked_hard: pr.fact.merge_state.eq_ignore_ascii_case("DIRTY")
                    || off_admission_path(token),
                excluded: excluded(token, &pr.fact),
                unblocks: unblocks.get(&report.number).copied().unwrap_or(0),
                age_minutes,
                actor: next.map_or_else(String::new, |next| {
                    serde_json::to_value(next.actor)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_default()
                }),
                action: next.map_or_else(String::new, |next| next.action.clone()),
            })
        })
        .collect()
}

/// A token that keeps a pull request out of tiers 1 to 3 however green and
/// approved it looks: a conflict, a HOLD, an ejected head, an approval that
/// does not count, or a row the steward does not admit at all.
fn off_admission_path(token: &str) -> bool {
    let kind = token.split_once(':').map_or(token, |(kind, _)| kind);
    match kind {
        "merge_state" => token.ends_with("DIRTY"),
        "approval_excluded" => !matches!(
            token,
            "approval_excluded:stale_head" | "approval_excluded:predates_head"
        ),
        _ => matches!(
            kind,
            "held"
                | "ejected_same_head"
                | "ejection_cap"
                | "unmanaged"
                | "handoff_missing"
                | "invalid_head"
                | "required_policy_unknown"
                | "direct_merge_refused"
                | "admission_refused"
                | "steward_error"
                | "carries_unapproved_commits"
                | "carried_commits_over_cap"
                | "carried_commits_unreadable"
        ),
    }
}

fn excluded(token: &str, pr: &StewardPullRequest) -> bool {
    pr.queue_position.is_some()
        || pr.draft
        || matches!(token, "queued" | "admitted" | "draft" | "owner_decision")
        || token.starts_with("excluded_label:")
        || token.starts_with("provenance_blocked")
}

/// `(dependent, depends_on)` edges: `stacked:<n>` labels and carried
/// commits on the census, and base-branch chains from the listing.
fn dependency_edges(input: &DwellInput<'_>, reports: &[PrReport]) -> Vec<(u64, u64)> {
    let mut edges = Vec::new();
    for pr in &input.observation.prs {
        for label in &pr.fact.labels {
            if let Some(parent) = label
                .strip_prefix(super::STACKED_LABEL_PREFIX)
                .and_then(|n| n.trim().trim_start_matches('#').parse::<u64>().ok())
            {
                edges.push((pr.fact.number, parent));
            }
        }
    }
    for report in reports {
        if let Some(carried) = report
            .blocked
            .blocked_on
            .strip_prefix("carries_unapproved_commits:#")
            .and_then(|n| n.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse::<u64>().ok())
        {
            edges.push((report.number, carried));
        }
    }
    if let Some(listing) = input.listing {
        let by_head: BTreeMap<&str, u64> = listing
            .iter()
            .map(|pr| (pr.head_ref.as_str(), pr.number))
            .collect();
        for pr in listing {
            if let Some(parent) = pr.stacked_on {
                edges.push((pr.number, parent));
            }
            if let Some(parent) = by_head.get(pr.base_ref.as_str()) {
                edges.push((pr.number, *parent));
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    edges
}

fn green(conclusion: Option<&str>) -> bool {
    conclusion.is_some_and(|conclusion| {
        ["SUCCESS", "NEUTRAL", "SKIPPED"]
            .iter()
            .any(|ok| conclusion.eq_ignore_ascii_case(ok))
    })
}

fn is_approval(policy: &StewardPolicy, context: &str) -> bool {
    let approval = policy.approval_context.trim();
    !approval.is_empty() && context.eq_ignore_ascii_case(approval)
}

/// The approval context is green at the head.
fn approval_green(pr: &StewardPullRequest, policy: &StewardPolicy) -> bool {
    policy
        .required_checks
        .iter()
        .filter(|required| is_approval(policy, &required.context))
        .filter_map(|required| selected_required_check(&pr.checks, required))
        .any(|check| {
            check.status.eq_ignore_ascii_case("COMPLETED") && green(check.conclusion.as_deref())
        })
}

/// The required checks other than the approval context, at the head.
fn others_state(pr: &StewardPullRequest, policy: &StewardPolicy) -> OthersState {
    let mut state = OthersState::Green;
    let mut any = false;
    for required in policy
        .required_checks
        .iter()
        .filter(|required| !is_approval(policy, &required.context))
    {
        any = true;
        match selected_required_check(&pr.checks, required) {
            None => state = OthersState::Pending,
            Some(check) if !check.status.eq_ignore_ascii_case("COMPLETED") => {
                if state == OthersState::Green {
                    state = OthersState::Pending;
                }
            }
            Some(check) if green(check.conclusion.as_deref()) => {}
            Some(_) => return OthersState::Red,
        }
    }
    if any { state } else { OthersState::Pending }
}

/// Append the new escalations to the overdue feed.
fn append_feed<'a>(
    state_dir: &Path,
    rows: impl Iterator<Item = &'a OverdueRecord>,
) -> Result<(), String> {
    use std::io::Write as _;
    let lines: Vec<String> = rows
        .filter_map(|row| serde_json::to_string(row).ok())
        .collect();
    if lines.is_empty() {
        return Ok(());
    }
    let dir = state_dir.join(DIR);
    std::fs::create_dir_all(&dir).map_err(|error| format!("overdue feed: {error}"))?;
    let path = dir.join(FEED);
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > FEED_ROTATE_BYTES) {
        let _ = std::fs::rename(&path, dir.join(format!("{FEED}.1")));
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("overdue feed {}: {error}", path.display()))?;
    let mut text = lines.join("\n");
    text.push('\n');
    file.write_all(text.as_bytes())
        .map_err(|error| format!("overdue feed {}: {error}", path.display()))
}

/// Where `repo`'s snapshot for `base` lives.
pub(crate) fn snapshot_path(state_dir: &Path, repo: &str, base: &str) -> PathBuf {
    let slug: String = dwell_key(repo, base)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect();
    state_dir.join(DIR).join(format!("{slug}.json"))
}

/// Write `repo`'s snapshot for `shipyard overdue` and `shipyard next`.
pub(super) fn write_snapshot(
    state_dir: &Path,
    repo: &str,
    base: &str,
    report: &DwellRepoReport,
    api_calls: Option<&crate::gh_pass::GhPassReport>,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let snapshot = serde_json::json!({
        "schema": 1,
        "at": now.to_rfc3339(),
        "repo": repo,
        "base": base,
        "act_mode": report.act_mode,
        "tracked": report.tracked,
        "overdue": report.overdue,
        "next_to_admit": report.next_to_admit,
        "ranking": report.ranking,
        "stack_listing": report.stack_listing,
        "api_calls": api_calls.map_or(Value::Null, |api| serde_json::json!({
            "rest": api.rest, "graphql": api.graphql, "cache_hits": api.cache_hits,
        })),
    });
    let path = snapshot_path(state_dir, repo, base);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("dwell snapshot: {error}"))?;
    }
    crate::durable_file::replace_json(&path, &snapshot, true)
        .map_err(|error| format!("dwell snapshot {}: {error}", path.display()))
}

/// The newest observation of the approval context on the head, when it is a
/// completed red run that a re-run could flip: the check the required rule
/// selects (app binding included), with no later or still-running check of
/// the same name. Pure.
pub(super) fn rerun_target(
    checks: &[crate::merge_steward::StewardCheck],
    required: &crate::merge_steward::RequiredCheck,
) -> Result<u64, String> {
    let selected = selected_required_check(checks, required)
        .ok_or_else(|| format!("no {} check on the head", required.context))?;
    if checks.iter().any(|check| {
        check.name.eq_ignore_ascii_case(&required.context)
            && !check.status.eq_ignore_ascii_case("COMPLETED")
    }) {
        return Err(format!("a {} run is still running", required.context));
    }
    if !selected.status.eq_ignore_ascii_case("COMPLETED") || green(selected.conclusion.as_deref()) {
        return Err(format!("the {} check is not red", required.context));
    }
    selected
        .run_id
        .ok_or_else(|| format!("the {} check names no workflow run", required.context))
}

/// Whether the approval gate's live answer allows re-running the verdict
/// check: only an exact-head approval does. `NotRequired` does not (the
/// ruleset may still require the context, and a re-run cannot make it
/// green), and any refusal (a HOLD above all) does not. Pure.
pub(super) fn gate_allows_rerun(
    gate: &crate::head_approval::HeadGate,
    blocker: Option<&crate::head_approval::ApprovalBlocker>,
) -> Result<(), String> {
    if let Some(blocker) = blocker {
        return Err(format!(
            "the approval gate refuses the head ({}); a re-run cannot change it",
            blocker.token()
        ));
    }
    match gate {
        crate::head_approval::HeadGate::Approved { .. } => Ok(()),
        _ => Err("the approval gate does not approve the exact head".to_owned()),
    }
}

/// What a live read of a pull request must show before an act on it: still
/// open, the head the act was planned for, and a head branch in the base
/// repository (a fork's branch name would dispatch on a base-repository
/// branch of the same name). Pure.
pub(super) fn live_pr_matches(live: &Value, repo: &str, head: &str) -> Result<(), String> {
    let text = |pointer: &str| {
        live.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    if text("/state") != "open" {
        return Err("no longer open".to_owned());
    }
    if !text("/head/sha").eq_ignore_ascii_case(head) {
        return Err("head moved since it was observed".to_owned());
    }
    if !text("/head/repo/full_name").eq_ignore_ascii_case(repo) {
        return Err("its head branch is in a fork".to_owned());
    }
    Ok(())
}

/// Every fact a retarget needs, read live. Pure: refuses unless the stacked
/// pull request is unchanged, its `stacked:<n>` parent merged into the
/// steward's base from the very branch it is based on, and that branch still
/// points at the parent's merged head (so it carries nothing the parent did
/// not land).
pub(super) fn retarget_allowed(
    stacked: &OpenPr,
    live: &Value,
    parent: &Value,
    base_tip: Option<&str>,
    repo: &str,
    base: &str,
) -> Result<(), String> {
    let text = |value: &Value, pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    live_pr_matches(live, repo, &stacked.head_sha)?;
    if text(live, "/base/ref") != stacked.base_ref {
        return Err("its base changed since it was observed".to_owned());
    }
    let number = parent.get("number").and_then(Value::as_u64).unwrap_or(0);
    if parent.get("merged_at").is_none_or(Value::is_null) {
        return Err(format!(
            "#{number} closed without merging: retargeting would carry its commits"
        ));
    }
    if text(parent, "/head/ref") != stacked.base_ref
        || !text(parent, "/head/repo/full_name").eq_ignore_ascii_case(repo)
    {
        return Err(format!(
            "#{number} was not the pull request of `{}`",
            stacked.base_ref
        ));
    }
    if text(parent, "/base/ref") != base {
        return Err(format!(
            "#{number} merged into `{}`, not `{base}`",
            text(parent, "/base/ref")
        ));
    }
    match base_tip {
        None => Err(format!(
            "`{}` is gone; GitHub retargets on branch deletion",
            stacked.base_ref
        )),
        Some(tip) if tip.eq_ignore_ascii_case(&text(parent, "/head/sha")) => Ok(()),
        Some(_) => Err(format!(
            "`{}` moved after #{number} merged; retargeting would carry unlanded commits",
            stacked.base_ref
        )),
    }
}

/// [`DwellActs`] against GitHub, under the steward's mutation guard. Every
/// act re-reads the pull request live immediately before its side effect.
pub(super) struct GitHubDwellActs<'a> {
    pub actions: &'a GitHubActions,
    pub observation: &'a RepoObservation,
    pub control: Option<&'a MutationControl>,
    /// The policy's approval context and the required rule naming it.
    pub approval: Option<&'a crate::merge_steward::RequiredCheck>,
}

impl GitHubDwellActs<'_> {
    fn guarded(
        &self,
        pr: &ObservedPr,
        action: &str,
        effect: impl FnOnce() -> Result<String, String>,
    ) -> ActOutcome {
        let Some(control) = self.control else {
            return ActOutcome::Deferred("no mutation control (dry run)".to_owned());
        };
        let guard = match super::acquire_pr_mutation_guard(control, self.observation, pr, action) {
            Ok(guard) => guard,
            Err(error) => return ActOutcome::Deferred(format!("mutation guard: {error}")),
        };
        match effect() {
            Ok(detail) => match guard.finish("dwell_act_accepted") {
                Ok(()) => ActOutcome::Done(detail),
                Err(error) => ActOutcome::Done(format!("{detail}; audit failed: {error}")),
            },
            Err(error) => {
                let _ = guard.finish("dwell_act_failed");
                ActOutcome::Failed(error)
            }
        }
    }

    fn gh_json(&self, argv: &[String]) -> Result<Value, String> {
        let raw = self
            .actions
            .run_gh(argv)
            .map_err(|error| error.to_string())?;
        serde_json::from_str(&raw).map_err(|error| error.to_string())
    }

    fn live_pr(&self, number: u64) -> Result<Value, String> {
        self.gh_json(&[
            "api".to_owned(),
            format!("repos/{}/pulls/{number}", self.observation.repo),
        ])
    }

    /// Live revalidation of a census pull request before an act on it.
    fn revalidate(&self, pr: &ObservedPr) -> Option<ActOutcome> {
        match self.live_pr(pr.fact.number) {
            Err(error) => Some(ActOutcome::Deferred(format!(
                "pull request unreadable: {error}"
            ))),
            Ok(live) => live_pr_matches(&live, &self.observation.repo, &pr.fact.head_sha)
                .err()
                .map(ActOutcome::Refused),
        }
    }
}

impl DwellActs for GitHubDwellActs<'_> {
    fn rerun_verdict(&self, pr: &ObservedPr, ledger: &StewardLedger) -> ActOutcome {
        let Some(required) = self.approval else {
            return ActOutcome::Refused("no required approval context".to_owned());
        };
        let run_id = match rerun_target(&pr.fact.checks, required) {
            Ok(run_id) => run_id,
            Err(why) => return ActOutcome::Refused(why),
        };
        if let Some(outcome) = self.revalidate(pr) {
            return outcome;
        }
        match super::pr_mutations::read_head_gate_for_report(
            self.actions,
            self.observation,
            pr,
            ledger,
        ) {
            Err(error) => {
                return ActOutcome::Deferred(format!("approval gate unreadable: {error}"));
            }
            Ok((gate, _, blocker)) => {
                if let Err(why) = gate_allows_rerun(&gate, blocker.as_ref()) {
                    return ActOutcome::Refused(why);
                }
            }
        }
        self.guarded(
            pr,
            &format!(
                "runner steward dwell rerun stale {} run {run_id}",
                required.context
            ),
            || {
                self.actions
                    .rerun_failed_run(&self.observation.repo, run_id)
                    .map(|()| format!("re-ran run {run_id}"))
                    .map_err(|error| error.to_string())
            },
        )
    }

    fn dispatch(&self, pr: &ObservedPr, workflows: &[String]) -> ActOutcome {
        if workflows.is_empty() {
            return ActOutcome::Refused("no mapped workflow for the missing context".to_owned());
        }
        if let Some(outcome) = self.revalidate(pr) {
            return outcome;
        }
        self.guarded(
            pr,
            &format!("runner steward dwell dispatch {}", workflows.join(",")),
            || {
                for workflow in workflows {
                    self.actions
                        .run_gh(&[
                            "workflow".to_owned(),
                            "run".to_owned(),
                            workflow.clone(),
                            "--ref".to_owned(),
                            pr.fact.head_branch.clone(),
                            "--repo".to_owned(),
                            self.observation.repo.clone(),
                        ])
                        .map_err(|error| format!("{workflow}: {error}"))?;
                }
                Ok(format!(
                    "dispatched {} on {}",
                    workflows.join(","),
                    pr.fact.head_branch
                ))
            },
        )
    }

    fn retarget(&self, stacked: &OpenPr) -> ActOutcome {
        let repo = &self.observation.repo;
        let base = &self.observation.base;
        let Some(parent_number) = stacked.stacked_on else {
            return ActOutcome::Refused("no stacked:<n> label".to_owned());
        };
        let live = match self.live_pr(stacked.number) {
            Ok(live) => live,
            Err(error) => {
                return ActOutcome::Deferred(format!("#{} unreadable: {error}", stacked.number));
            }
        };
        let parent = match self.live_pr(parent_number) {
            Ok(parent) => parent,
            Err(error) => {
                return ActOutcome::Deferred(format!("#{parent_number} unreadable: {error}"));
            }
        };
        let base_tip = match self.gh_json(&[
            "api".to_owned(),
            format!(
                "repos/{repo}/branches/{}",
                super::observation::encode_path_segment(&stacked.base_ref)
            ),
        ]) {
            Ok(branch) => branch
                .pointer("/commit/sha")
                .and_then(Value::as_str)
                .map(str::to_owned),
            Err(error) if error.contains("404") || error.contains("Not Found") => None,
            Err(error) => {
                return ActOutcome::Deferred(format!("`{}` unreadable: {error}", stacked.base_ref));
            }
        };
        if let Err(why) = retarget_allowed(stacked, &live, &parent, base_tip.as_deref(), repo, base)
        {
            return ActOutcome::Refused(why);
        }
        let observed = ObservedPr {
            node_id: String::new(),
            fact: StewardPullRequest {
                number: stacked.number,
                head_sha: stacked.head_sha.clone(),
                head_branch: stacked.head_ref.clone(),
                author: stacked.author.clone(),
                draft: false,
                merge_state: String::new(),
                auto_merge_active: false,
                queue_position: None,
                labels: Vec::new(),
                checks: Vec::new(),
            },
            owner: None,
            check_rollup_maybe_truncated: false,
        };
        self.guarded(
            &observed,
            &format!("runner steward dwell retarget to {base} after #{parent_number} merged"),
            || {
                let updated = self.gh_json(&[
                    "api".to_owned(),
                    "-X".to_owned(),
                    "PATCH".to_owned(),
                    format!("repos/{repo}/pulls/{}", stacked.number),
                    "-f".to_owned(),
                    format!("base={base}"),
                ])?;
                // Post-action proof: GitHub's answer names the new base.
                let now_base = updated
                    .pointer("/base/ref")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if now_base == base {
                    Ok(format!("retargeted to {base}; #{parent_number} merged"))
                } else {
                    Err(format!("readback base is {now_base:?}, not {base}"))
                }
            },
        )
    }
}
