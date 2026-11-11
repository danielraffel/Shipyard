//! Base poison read from merge groups.
//!
//! A base can be broken in a way the default branch's own CI never shows: a
//! time-based check that starts failing on a date, or a required context that
//! only runs on `merge_group` events. Then every batch the queue forms fails
//! the same required context, each costs a full gate, and each ejects pull
//! requests that did nothing wrong. The main-branch detector
//! ([`super::read_latest`]) sees nothing, because nothing ran on the base.
//!
//! This module reads the merge groups themselves. A required context is
//! **poisoned** when it failed in at least [`MIN_STREAK`] consecutive merge
//! groups (newest first, skipping groups where the context produced no
//! verdict) and those groups carried different pull requests with none common
//! to all of them, so no single pull request riding along explains the
//! failure. The poison **clears** as soon as a newer merge group passes the
//! context: the streak is counted from the newest verdict, so one pass ends
//! it. A streak whose newest failure is older than [`super::MAX_SIGNAL_AGE`]
//! is reported as stale and not acted on, so a base the queue has stopped
//! testing is re-tested by the next admission rather than held forever.
//!
//! Decisions are made here from already-read facts ([`detect`]); [`read`]
//! is the GitHub adapter and stays bounded: at most [`GROUP_SAMPLE`] merge
//! groups, and it stops reading groups as soon as every required context has
//! passed in the newest group that reported it.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::merge_steward::RequiredCheck;
use crate::validation_signals::GhReader;

/// Merge groups examined, newest first.
pub const GROUP_SAMPLE: usize = 6;
/// Consecutive failing merge groups that make a context poisoned.
pub const MIN_STREAK: usize = 2;
/// Merge-group workflow runs listed to find [`GROUP_SAMPLE`] distinct heads.
const RUN_LIST_PAGE: u32 = 40;

/// One required context's newest check on a merge-group head.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GroupCheck {
    /// Check-run name, which is the required context.
    pub context: String,
    /// GitHub conclusion, lowercased; empty while the check runs.
    pub conclusion: String,
    /// Check-run id, for reading its annotations.
    pub check_id: Option<u64>,
    /// When the check completed.
    pub completed_at: Option<DateTime<Utc>>,
    /// The check's own output title, a fallback first error line.
    pub title: Option<String>,
}

/// One merge group: its head and the pull requests it carried.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MergeGroup {
    /// Merge-group head commit.
    pub head_sha: String,
    /// Pull requests in the group. At least the one GitHub names in the
    /// `gh-readonly-queue/<base>/pr-<n>-<sha>` branch.
    pub prs: BTreeSet<u64>,
    /// Checks on the head, one per context.
    pub checks: Vec<GroupCheck>,
}

/// How one check bears on a context's health.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Pass,
    Fail,
    /// Running, cancelled, missing: says nothing about the base.
    NoVerdict,
}

fn outcome(check: Option<&GroupCheck>) -> Outcome {
    match check.map(|check| check.conclusion.as_str()) {
        Some("success" | "neutral" | "skipped") => Outcome::Pass,
        Some("failure" | "action_required" | "timed_out") => Outcome::Fail,
        _ => Outcome::NoVerdict,
    }
}

impl MergeGroup {
    fn check(&self, context: &str) -> Option<&GroupCheck> {
        self.checks.iter().find(|check| check.context == context)
    }
}

/// A merge group named in a poison finding.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GroupRef {
    /// Merge-group head commit.
    pub head_sha: String,
    /// Pull requests it carried.
    pub prs: Vec<u64>,
}

/// One poisoned required context.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PoisonedContext {
    /// The required context that keeps failing.
    pub context: String,
    /// First line of its newest failure, when one could be read.
    pub first_error: Option<String>,
    /// The consecutive failing groups, newest first.
    pub groups: Vec<GroupRef>,
    /// When the newest failure completed.
    pub latest_failure_at: Option<DateTime<Utc>>,
    /// Check run of the newest failure.
    pub check_id: Option<u64>,
    /// Shadow only: whether every group in the streak failed with the same
    /// first error line (a stricter predicate under measurement; it changes
    /// no decision). `None` when it was not read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_first_error: Option<bool>,
}

/// What the merge groups say about the base.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MergeGroupHealth {
    /// At least one required context is poisoned.
    Poisoned {
        /// Every poisoned context, sorted by name.
        contexts: Vec<PoisonedContext>,
    },
    /// No required context is poisoned.
    Clear {
        /// Why, for a person.
        detail: String,
        /// The required contexts whose newest verdict in the sample is a
        /// pass: the only evidence that the base is healthy again for them.
        /// A stale streak or an empty sample passes nothing.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        passed: Vec<String>,
    },
    /// The merge groups could not be read. Callers keep their normal
    /// behaviour: an unread base is never treated as poisoned.
    Unknown {
        /// What failed.
        detail: String,
    },
}

impl MergeGroupHealth {
    /// The poisoned contexts, empty unless [`Self::Poisoned`].
    #[must_use]
    pub fn poisoned(&self) -> &[PoisonedContext] {
        match self {
            Self::Poisoned { contexts } => contexts,
            _ => &[],
        }
    }

    /// Names of the poisoned contexts.
    #[must_use]
    pub fn poisoned_contexts(&self) -> Vec<String> {
        self.poisoned()
            .iter()
            .map(|poison| poison.context.clone())
            .collect()
    }

    /// One line for a person.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Poisoned { contexts } => contexts
                .iter()
                .map(|poison| {
                    format!(
                        "base poisoned: `{}` failed in {} consecutive merge groups{}",
                        poison.context,
                        poison.groups.len(),
                        poison
                            .first_error
                            .as_ref()
                            .map_or_else(String::new, |error| format!(": {error}"))
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
            Self::Clear { detail, .. } => format!("merge groups clear: {detail}"),
            Self::Unknown { detail } => format!("merge groups unread: {detail}"),
        }
    }
}

/// The newest consecutive failures of `context`, newest first, skipping
/// groups with no verdict and stopping at the first pass.
fn streak<'a>(groups: &'a [MergeGroup], context: &str) -> Vec<&'a MergeGroup> {
    let mut out = Vec::new();
    for group in groups {
        match outcome(group.check(context)) {
            Outcome::Pass => break,
            Outcome::Fail => out.push(group),
            Outcome::NoVerdict => {}
        }
    }
    out
}

/// Indices of the groups in any streak of [`MIN_STREAK`] or more: the only
/// groups whose pull-request sets the verdict needs, so the adapter reads
/// those and no others.
#[must_use]
pub fn groups_needing_members(groups: &[MergeGroup], required: &[String]) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    for context in required {
        let failing = streak(groups, context);
        if failing.len() >= MIN_STREAK {
            for group in failing {
                if let Some(index) = groups
                    .iter()
                    .position(|candidate| candidate.head_sha == group.head_sha)
                {
                    out.insert(index);
                }
            }
        }
    }
    out
}

/// Judge the base from merge groups ordered newest first.
#[must_use]
pub fn detect(groups: &[MergeGroup], required: &[String], now: DateTime<Utc>) -> MergeGroupHealth {
    if groups.is_empty() {
        return MergeGroupHealth::Clear {
            detail: "no recent merge group".to_owned(),
            passed: Vec::new(),
        };
    }
    let mut poisoned = Vec::new();
    let mut notes = Vec::new();
    let mut contexts = required.to_vec();
    contexts.sort();
    contexts.dedup();
    for context in &contexts {
        let failing = streak(groups, context);
        if failing.len() < MIN_STREAK {
            continue;
        }
        let distinct = failing
            .iter()
            .map(|group| &group.prs)
            .collect::<BTreeSet<_>>();
        let common = failing
            .iter()
            .skip(1)
            .fold(failing[0].prs.clone(), |common, group| {
                common.intersection(&group.prs).copied().collect()
            });
        if distinct.len() < 2 || !common.is_empty() {
            notes.push(format!(
                "`{context}` failed in {} groups that all carried PR {}",
                failing.len(),
                common
                    .iter()
                    .map(|pr| format!("#{pr}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            continue;
        }
        let newest = failing[0];
        let newest_check = newest.check(context);
        let latest_failure_at = newest_check.and_then(|check| check.completed_at);
        if latest_failure_at.is_some_and(|at| now - at > super::MAX_SIGNAL_AGE) {
            notes.push(format!(
                "`{context}` streak is stale (newest failure older than {} h)",
                super::MAX_SIGNAL_AGE.num_hours()
            ));
            continue;
        }
        poisoned.push(PoisonedContext {
            context: context.clone(),
            first_error: newest_check.and_then(|check| check.title.clone()),
            groups: failing
                .iter()
                .map(|group| GroupRef {
                    head_sha: group.head_sha.clone(),
                    prs: group.prs.iter().copied().collect(),
                })
                .collect(),
            latest_failure_at,
            check_id: newest_check.and_then(|check| check.check_id),
            same_first_error: None,
        });
    }
    if poisoned.is_empty() {
        MergeGroupHealth::Clear {
            detail: if notes.is_empty() {
                "no required context failed in two consecutive merge groups".to_owned()
            } else {
                notes.join("; ")
            },
            passed: contexts
                .iter()
                .filter(|context| {
                    groups
                        .iter()
                        .map(|group| outcome(group.check(context)))
                        .find(|outcome| *outcome != Outcome::NoVerdict)
                        == Some(Outcome::Pass)
                })
                .cloned()
                .collect(),
        }
    } else {
        MergeGroupHealth::Poisoned { contexts: poisoned }
    }
}

/// Whether reading more groups can change the verdict: false once every
/// required context has passed in the newest group that reported it.
fn undecided(groups: &[MergeGroup], required: &[String]) -> bool {
    required.iter().any(|context| {
        groups
            .iter()
            .map(|group| outcome(group.check(context)))
            .find(|outcome| *outcome != Outcome::NoVerdict)
            != Some(Outcome::Pass)
    })
}

fn read_json(gh: &GhReader<'_>, path: &str) -> Result<Value, String> {
    let raw = gh(&["api".to_owned(), path.to_owned()])?;
    serde_json::from_str(&raw).map_err(|error| format!("malformed JSON from `{path}`: {error}"))
}

fn parse_time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&Utc))
}

/// The stricter predicate under shadow measurement: every failing group in
/// the streak carries a readable first error line, and they are all equal.
#[must_use]
pub fn same_first_error(errors: &[Option<String>]) -> bool {
    let Some(Some(first)) = errors.first() else {
        return false;
    };
    errors.len() >= MIN_STREAK && errors.iter().all(|error| error.as_ref() == Some(first))
}

/// Pull requests named by `Merge pull request #<n>` commit subjects.
#[must_use]
pub fn merged_prs(messages: &[&str]) -> BTreeSet<u64> {
    const PREFIX: &str = "Merge pull request #";
    messages
        .iter()
        .filter_map(|message| {
            let rest = message.lines().next()?.strip_prefix(PREFIX)?;
            let digits = rest
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            digits.parse().ok()
        })
        .collect()
}

/// The newest check per required context on one head.
fn checks_for_head(value: &Value, required: &[RequiredCheck]) -> Vec<GroupCheck> {
    let mut out: Vec<GroupCheck> = Vec::new();
    for run in value
        .get("check_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = run.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(rule) = required.iter().find(|rule| rule.context == name) else {
            continue;
        };
        if rule
            .app_id
            .is_some_and(|app_id| run.pointer("/app/id").and_then(Value::as_u64) != Some(app_id))
        {
            continue;
        }
        let id = run.get("id").and_then(Value::as_u64);
        let check = GroupCheck {
            context: name.to_owned(),
            conclusion: run
                .get("conclusion")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase(),
            check_id: id,
            completed_at: parse_time(run.get("completed_at")),
            title: run
                .pointer("/output/title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_owned),
        };
        // A re-run is a new check run with a larger id: keep the newest.
        match out.iter_mut().find(|existing| existing.context == name) {
            Some(existing) if existing.check_id < id => *existing = check,
            Some(_) => {}
            None => out.push(check),
        }
    }
    out
}

/// First line of the first failure annotation on a check run.
fn first_error_line(gh: &GhReader<'_>, repo: &str, check_id: u64) -> Option<String> {
    let annotations = read_json(
        gh,
        &format!("repos/{repo}/check-runs/{check_id}/annotations"),
    )
    .ok()?;
    annotations
        .as_array()?
        .iter()
        .filter(|annotation| {
            annotation.get("annotation_level").and_then(Value::as_str) == Some("failure")
                && !annotation
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| message.starts_with("Process completed with exit code"))
        })
        .find_map(|annotation| {
            annotation
                .get("message")
                .and_then(Value::as_str)?
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_owned)
        })
}

/// Read the newest merge groups on `base` and judge them.
///
/// Reads one page of `merge_group` workflow runs, then each distinct head's
/// check runs (newest first, stopping once the verdict is settled), then the
/// commits of only those groups that sit in a failing streak, and finally the
/// failure annotations of a poisoned context's newest check.
#[must_use]
pub fn read(
    gh: &GhReader<'_>,
    repo: &str,
    base: &str,
    required: &[RequiredCheck],
    now: DateTime<Utc>,
) -> MergeGroupHealth {
    if required.is_empty() {
        return MergeGroupHealth::Unknown {
            detail: "no required contexts to judge".to_owned(),
        };
    }
    let contexts = required
        .iter()
        .map(|rule| rule.context.clone())
        .collect::<Vec<_>>();
    let runs = match read_json(
        gh,
        &format!("repos/{repo}/actions/runs?event=merge_group&per_page={RUN_LIST_PAGE}"),
    ) {
        Ok(value) => value,
        Err(detail) => return MergeGroupHealth::Unknown { detail },
    };
    let prefix = format!("gh-readonly-queue/{base}/");
    let mut heads: Vec<(String, Option<u64>)> = Vec::new();
    for run in runs
        .get("workflow_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let branch = run.get("head_branch").and_then(Value::as_str).unwrap_or("");
        let Some(sha) = run.get("head_sha").and_then(Value::as_str) else {
            continue;
        };
        if !branch.starts_with(&prefix) || heads.iter().any(|(head, _)| head == sha) {
            continue;
        }
        heads.push((sha.to_owned(), crate::merge_steward::merge_group_pr(branch)));
        if heads.len() == GROUP_SAMPLE {
            break;
        }
    }
    let mut groups = Vec::new();
    for (sha, top) in heads {
        let checks = match read_json(
            gh,
            &format!("repos/{repo}/commits/{sha}/check-runs?per_page=100"),
        ) {
            Ok(value) => checks_for_head(&value, required),
            Err(detail) => return MergeGroupHealth::Unknown { detail },
        };
        groups.push(MergeGroup {
            head_sha: sha,
            prs: top.into_iter().collect(),
            checks,
        });
        if !undecided(&groups, &contexts) {
            break;
        }
    }
    for index in groups_needing_members(&groups, &contexts) {
        let group = &mut groups[index];
        // A group's own merge commits name every pull request it carried;
        // without them the branch's one named pull request stands alone.
        let compare = match read_json(
            gh,
            &format!("repos/{repo}/compare/{base}...{}", group.head_sha),
        ) {
            Ok(compare) => compare,
            Err(detail) => return MergeGroupHealth::Unknown { detail },
        };
        let Some(commits) = compare.get("commits").and_then(Value::as_array) else {
            return MergeGroupHealth::Unknown {
                detail: format!(
                    "merge-group membership compare for {} had no commits array",
                    group.head_sha
                ),
            };
        };
        let messages = commits
            .iter()
            .filter_map(|commit| commit.pointer("/commit/message").and_then(Value::as_str))
            .collect::<Vec<_>>();
        group.prs.extend(merged_prs(&messages));
    }
    let mut health = detect(&groups, &contexts, now);
    if let MergeGroupHealth::Poisoned { contexts } = &mut health {
        for poison in contexts {
            // The two newest groups of the streak only: one annotation read
            // more than before, and only while poisoned.
            let errors = poison
                .groups
                .iter()
                .take(MIN_STREAK)
                .map(|member| {
                    groups
                        .iter()
                        .find(|group| group.head_sha == member.head_sha)
                        .and_then(|group| group.check(&poison.context))
                        .and_then(|check| check.check_id)
                        .and_then(|check_id| first_error_line(gh, repo, check_id))
                })
                .collect::<Vec<_>>();
            if let Some(Some(line)) = errors.first() {
                poison.first_error = Some(line.clone());
            }
            poison.same_first_error = Some(same_first_error(&errors));
        }
    }
    health
}

#[cfg(test)]
mod tests;
