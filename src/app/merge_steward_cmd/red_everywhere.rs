//! Red everywhere: the same required check failing the same way on many open
//! pull requests at once is a base-level cause, not a fault in each of them.
//!
//! Each pass groups the open pull requests whose current head has a failed
//! required check by (check, failing test name). The test names come from the
//! diagnosis the infra re-run sweep already read for that head
//! ([`super::infra_rerun::Record::tests`]); a check whose failing tests were
//! not read groups by the check alone. A group of [`MIN_PRS`] or more distinct
//! pull requests is a base-level episode: the pass reports it, and each
//! member's row says to wait for the base fix (or merge the base once the base
//! is green) instead of repairing the pull request.
//!
//! The mode is the base config's `[auto_merge] red_everywhere`:
//!
//! * `off`: nothing is grouped or reported.
//! * `shadow` (the default): episodes are reported and each member row is
//!   marked; nothing is held.
//! * `enforce`: as shadow, and a member whose head has not changed since the
//!   episode began is not enqueued (`held_red_everywhere:<check>`), and its
//!   row tells the repair lane to skip it (`repair_skip`). Nothing is ever
//!   dequeued or closed.
//!
//! Reads: none. The grouping uses the census's current-head checks and the
//! sweep's ledger records. Episodes are computed before any pull request is
//! mutated ([`observe`]) and member rows are marked after ([`Pass::annotate`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::infra_rerun::{self, record_key};
use super::{PrReport, RepoObservation, StewardLedger};

/// Distinct pull requests failing the same way that make an episode.
pub(crate) const MIN_PRS: usize = 3;

pub(crate) use crate::merge_steward::ejection_guards::RedEverywhereMode as Mode;

/// One episode in the ledger: when it began and which heads were in it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct Episode {
    pub(super) first_epoch: i64,
    pub(super) last_seen_epoch: i64,
    /// Member pull request -> the head it failed on.
    #[serde(default)]
    pub(super) heads: BTreeMap<u64, String>,
}

/// One reported episode.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(super) struct Group {
    pub(super) check: String,
    /// The shared failing test, or `null` when grouped by the check alone.
    pub(super) test: Option<String>,
    pub(super) prs: Vec<u64>,
    /// When this episode was first seen (RFC 3339).
    pub(super) first_seen: String,
    /// First seen by this pass.
    pub(super) new: bool,
}

/// The repository report section.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct RepoStatus {
    pub(super) mode: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) groups: Vec<Group>,
}

/// The mark on a member's row.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(super) struct RowMark {
    pub(super) check: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) test: Option<String>,
    /// Open pull requests in the episode.
    pub(super) prs: usize,
    /// The repair lane should not repair this pull request (enforce only).
    pub(super) repair_skip: bool,
}

/// One red required check on a current head, with its failing tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Red {
    pub(super) pr: u64,
    pub(super) head: String,
    pub(super) check: String,
    /// Failing test names from the diagnosis; empty when none were read.
    pub(super) tests: Vec<String>,
}

/// Group key: (check, test or `None` for the check alone).
type Key = (String, Option<String>);

/// The episodes in `reds`: groups of [`MIN_PRS`] or more distinct pull
/// requests failing the same check with the same test (or the same check, for
/// heads whose tests were not read). Reads nothing.
pub(super) fn group(reds: &[Red]) -> BTreeMap<Key, BTreeMap<u64, String>> {
    let mut groups: BTreeMap<Key, BTreeMap<u64, String>> = BTreeMap::new();
    for red in reds {
        let tests: BTreeSet<&String> = red.tests.iter().collect();
        if tests.is_empty() {
            groups
                .entry((red.check.clone(), None))
                .or_default()
                .insert(red.pr, red.head.clone());
        }
        for test in tests {
            groups
                .entry((red.check.clone(), Some(test.clone())))
                .or_default()
                .insert(red.pr, red.head.clone());
        }
    }
    groups.retain(|_, members| members.len() >= MIN_PRS);
    groups
}

fn episode_key(repo: &str, check: &str, test: Option<&str>) -> String {
    format!(
        "{}|{check}|{}",
        repo.to_ascii_lowercase(),
        test.unwrap_or("")
    )
}

fn repo_prefix(repo: &str) -> String {
    format!("{}|", repo.to_ascii_lowercase())
}

/// The red required checks of every open pull request's current head, with
/// the failing tests the ledger holds for that exact head and check.
///
/// Every failed required check counts, a commit status or a check run without
/// Actions ids included ([`infra_rerun::failed_required`]); such a red has no
/// diagnosis and groups by the check alone. The ledger's tests are reused only
/// when the record judged this very observation (`judged_check_id` equals the
/// check's id): a newer run of the same check on the same head groups by the
/// check alone until it is read, never by an older run's tests.
pub(super) fn reds(
    observation: &RepoObservation,
    ledger: &StewardLedger,
    approval_context: &str,
    excluded_labels: &[String],
) -> Vec<Red> {
    infra_rerun::failed_required(observation, approval_context, excluded_labels)
        .into_iter()
        .map(|(pr, check)| {
            let tests = check
                .check_id
                .and_then(|check_id| {
                    ledger
                        .infra_reruns
                        .get(&record_key(
                            &observation.repo,
                            pr.fact.number,
                            &pr.fact.head_sha,
                            &check.name,
                        ))
                        .filter(|record| record.judged_check_id == Some(check_id))
                })
                .map(|record| record.tests.clone())
                .unwrap_or_default();
            Red {
                pr: pr.fact.number,
                head: pr.fact.head_sha.clone(),
                check: check.name.clone(),
                tests,
            }
        })
        .collect()
}

/// One pass's episodes: the report section, and the marks still to put on
/// member rows once the rows exist ([`Self::annotate`]).
#[derive(Debug)]
pub(super) struct Pass {
    status: RepoStatus,
    marks: BTreeMap<u64, RowMark>,
}

/// The pass step that runs before any pull request is mutated: group,
/// remember each episode in the ledger (its first sighting and member heads;
/// an episode no longer seen is dropped, and a member no longer red the same
/// way leaves it), and return what to report and mark. Running it before the
/// enqueue path means [`held`] sees this pass's episodes: a newly detected
/// one holds at once, and a head that recovered is never held on last pass's
/// record.
pub(super) fn observe(
    mode: Mode,
    observation: &RepoObservation,
    reds: &[Red],
    ledger: &mut StewardLedger,
    now_epoch: i64,
) -> Pass {
    let prefix = repo_prefix(&observation.repo);
    let mut pass = Pass {
        status: RepoStatus {
            mode: mode.label().to_owned(),
            groups: Vec::new(),
        },
        marks: BTreeMap::new(),
    };
    if mode == Mode::Off {
        ledger
            .red_everywhere
            .retain(|key, _| !key.starts_with(&prefix));
        return pass;
    }
    let groups = group(reds);
    let live: BTreeSet<String> = groups
        .keys()
        .map(|(check, test)| episode_key(&observation.repo, check, test.as_deref()))
        .collect();
    ledger
        .red_everywhere
        .retain(|key, _| !key.starts_with(&prefix) || live.contains(key));
    for ((check, test), members) in groups {
        let key = episode_key(&observation.repo, &check, test.as_deref());
        let new = !ledger.red_everywhere.contains_key(&key);
        let episode = ledger.red_everywhere.entry(key).or_insert(Episode {
            first_epoch: now_epoch,
            last_seen_epoch: now_epoch,
            heads: BTreeMap::new(),
        });
        episode.last_seen_epoch = now_epoch;
        // A member keeps the head it first failed on. A member that pushed a
        // new head has left the episode, even when the new head fails the same
        // way: it is not counted, marked or held. Its first head stays on
        // record so it does not rejoin as a new member while the episode lasts.
        episode.heads.retain(|pr, _| members.contains_key(pr));
        let mut members = members;
        members.retain(|pr, head| {
            episode
                .heads
                .entry(*pr)
                .or_insert_with(|| head.clone())
                .eq_ignore_ascii_case(head)
        });
        if members.len() < MIN_PRS {
            let key = episode_key(&observation.repo, &check, test.as_deref());
            ledger.red_everywhere.remove(&key);
            continue;
        }
        let count = members.len();
        for pr in members.keys() {
            pass.marks.entry(*pr).or_insert_with(|| RowMark {
                check: check.clone(),
                test: test.clone(),
                prs: count,
                repair_skip: mode == Mode::Enforce,
            });
        }
        pass.status.groups.push(Group {
            check,
            test,
            prs: members.into_keys().collect(),
            first_seen: chrono::DateTime::from_timestamp(episode.first_epoch, 0)
                .map(|at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_default(),
            new,
        });
    }
    pass
}

impl Pass {
    /// Mark each member's row and return the report section.
    pub(super) fn annotate(mut self, reports: &mut [PrReport]) -> RepoStatus {
        for report in reports.iter_mut() {
            let Some(mark) = self.marks.remove(&report.number) else {
                continue;
            };
            if let Some(next) = report.blocked.next_action.as_mut() {
                next.action = advice(&mark);
            }
            report.blocked.red_everywhere = Some(mark);
        }
        self.status
    }
}

/// [`observe`] then [`Pass::annotate`], for tests that need no mutation in
/// between.
#[cfg(test)]
pub(super) fn run(
    mode: Mode,
    observation: &RepoObservation,
    reds: &[Red],
    ledger: &mut StewardLedger,
    reports: &mut [PrReport],
    now_epoch: i64,
) -> RepoStatus {
    observe(mode, observation, reds, ledger, now_epoch).annotate(reports)
}

/// The row's next action while it is in an episode.
pub(super) fn advice(mark: &RowMark) -> String {
    let what = mark.test.as_deref().map_or_else(
        || mark.check.clone(),
        |test| format!("{} ({test})", mark.check),
    );
    format!(
        "Failing on {} PRs with the same test: {what}. Likely a base-level cause; do not repair \
         this PR, wait for the base fix or merge main once main is green.",
        mark.prs
    )
}

/// Under enforce, the check whose episode holds this exact head, if any:
/// a member whose head has not changed since the episode began.
pub(super) fn held(
    mode: Mode,
    ledger: &StewardLedger,
    repo: &str,
    pr: u64,
    head: &str,
) -> Option<String> {
    if mode != Mode::Enforce {
        return None;
    }
    let prefix = repo_prefix(repo);
    ledger.red_everywhere.iter().find_map(|(key, episode)| {
        (key.starts_with(&prefix)
            && episode
                .heads
                .get(&pr)
                .is_some_and(|member| member.eq_ignore_ascii_case(head)))
        .then(|| {
            key[prefix.len()..]
                .split('|')
                .next()
                .unwrap_or_default()
                .to_owned()
        })
    })
}

#[cfg(test)]
mod tests;
