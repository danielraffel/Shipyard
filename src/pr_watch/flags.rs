//! The five flag rules, evaluated at one instant.
//!
//! [`evaluate`] is pure: it reads a [`RepoHistory`] and a time and considers
//! only facts that were already true at that time. A live scan passes `now`;
//! a replay passes every tick of a past window.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::{
    CheckFact, FailureRecord, HeadFact, PrHistory, QueueEventKind, RepoHistory, failures_by_pr,
    head_at, open_at, outcomes_for, short,
};

/// Which rule raised a flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagKind {
    /// Flag 1: the same failing test on at least two runs.
    RepeatTestFailure,
    /// Flag 2: armed, out of the queue, a required check red too long.
    RedWhileArmed,
    /// Flag 3: at least two failed merge groups named for the pull request.
    RepeatedEjection,
    /// Flag 4: head replaced repeatedly while the base moved.
    RebaseTreadmill,
    /// Flag 5 (advisory): long-lived or large.
    SplitCandidate,
}

impl FlagKind {
    /// The spec's flag number.
    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            Self::RepeatTestFailure => 1,
            Self::RedWhileArmed => 2,
            Self::RepeatedEjection => 3,
            Self::RebaseTreadmill => 4,
            Self::SplitCandidate => 5,
        }
    }

    /// The kind for a spec flag number.
    #[must_use]
    pub fn from_number(number: u8) -> Option<Self> {
        match number {
            1 => Some(Self::RepeatTestFailure),
            2 => Some(Self::RedWhileArmed),
            3 => Some(Self::RepeatedEjection),
            4 => Some(Self::RebaseTreadmill),
            5 => Some(Self::SplitCandidate),
            _ => None,
        }
    }

    /// Whether a new head addresses this flag (restarts its clock). A new
    /// head is the fix attempt for a failure, but it *is* the symptom of a
    /// rebase treadmill, and it does not change a pull request's size.
    #[must_use]
    pub fn restarts_on_new_head(self) -> bool {
        !matches!(self, Self::RebaseTreadmill | Self::SplitCandidate)
    }

    /// Digest severity: the per-PR digest line names the highest.
    #[must_use]
    pub fn severity(self) -> u8 {
        match self {
            Self::RedWhileArmed => 5,
            Self::RepeatedEjection => 4,
            Self::RepeatTestFailure => 3,
            Self::RebaseTreadmill => 2,
            Self::SplitCandidate => 1,
        }
    }

    /// Stable snake-case name (the digest contract's `kind`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RepeatTestFailure => "repeat_test_failure",
            Self::RedWhileArmed => "red_while_armed",
            Self::RepeatedEjection => "repeated_ejection",
            Self::RebaseTreadmill => "rebase_treadmill",
            Self::SplitCandidate => "split_candidate",
        }
    }
}

/// One flag that holds at an instant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flag {
    /// Pull request.
    pub pr: u64,
    /// Rule.
    pub kind: FlagKind,
    /// Distinguishes flags of one kind on one pull request (the failing test,
    /// the red check). Empty when a kind holds at most once per pull request.
    pub key: String,
    /// Short verdict.
    pub verdict: String,
    /// One-line evidence, the line a comment or digest would carry.
    pub evidence: String,
    /// Head the flag was evaluated against.
    pub head_sha: String,
    /// Where the digest carries it. The sticky comment carries every flag.
    #[serde(default)]
    pub route: DigestRoute,
    /// For [`DigestRoute::Shared`]: the tests failing across pull requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_tests: Vec<String>,
    /// For [`DigestRoute::Shared`]: the other pull requests failing them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_prs: Vec<u64>,
}

/// How the digest treats a flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestRoute {
    /// One line per pull request, owner action.
    #[default]
    PerPr,
    /// A failure shared across pull requests (likely main): at most one
    /// "shared failure" line per test, not an owner action.
    Shared,
    /// Sticky comment only (for example, an ejection the batch attributor
    /// pinned on a neighbour).
    CommentOnly,
}

impl Flag {
    /// Ledger identity: `pr:kind:key`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}:{}:{}", self.pr, self.kind.as_str(), self.key)
    }
}

/// Rule thresholds. Defaults are the spec's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thresholds {
    /// Flag 1: runs of the same failing signature.
    pub repeat_failures: usize,
    /// Flag 1: other pull requests failing the same signature within
    /// [`Thresholds::pre_existing_window_hours`] that make it pre-existing.
    pub pre_existing_other_prs: usize,
    /// Flag 1: look-back for the pre-existing check.
    pub pre_existing_window_hours: i64,
    /// Flag 2: minutes a required check must stay red.
    pub red_minutes: i64,
    /// Flag 3: failed merge groups named for the pull request.
    pub failed_groups: usize,
    /// Flag 4: qualifying head replacements.
    pub replacements: usize,
    /// Flag 4: rolling window, hours.
    pub replacement_window_hours: i64,
    /// Flag 5: days open.
    pub split_days: i64,
    /// Flag 5: changed files.
    pub split_files: u64,
    /// Flag 5: commits.
    pub split_commits: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            repeat_failures: 2,
            pre_existing_other_prs: 2,
            pre_existing_window_hours: 24,
            red_minutes: 30,
            failed_groups: 2,
            replacements: 3,
            replacement_window_hours: 24,
            split_days: 3,
            split_files: 60,
            split_commits: 30,
        }
    }
}

/// Every flag that holds at `at`, for pull requests open at `at`, ordered by
/// pull request then kind.
#[must_use]
pub fn evaluate(history: &RepoHistory, at: DateTime<Utc>, thresholds: &Thresholds) -> Vec<Flag> {
    let failures = failures_by_pr(history);
    let mut flags = Vec::new();
    for pr in history.prs.values() {
        if !open_at(pr, at) {
            continue;
        }
        let head = head_at(pr, at).map_or_else(|| pr.head_sha.clone(), |head| head.sha.clone());
        let mut found = Vec::new();
        found.extend(repeat_test_failure(
            history, &failures, pr, &head, at, thresholds,
        ));
        found.extend(red_while_armed(pr, history, at, thresholds));
        found.extend(repeated_ejection(history, pr, &head, at, thresholds));
        found.extend(rebase_treadmill(pr, &head, at, thresholds));
        if !found.is_empty()
            && let Some(split) = split_candidate(pr, &head, at, thresholds)
        {
            found.push(split);
        }
        found.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.key.cmp(&b.key)));
        flags.extend(found);
    }
    flags
}

fn completed_by(check: &CheckFact, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    check.completed_at.filter(|completed| *completed <= at)
}

/// Flag 1: one flag per (pull request, required check), listing every
/// failing test that repeated on that check.
#[allow(clippy::too_many_lines)]
fn repeat_test_failure(
    history: &RepoHistory,
    failures: &BTreeMap<u64, Vec<FailureRecord<'_>>>,
    pr: &PrHistory,
    head: &str,
    at: DateTime<Utc>,
    thresholds: &Thresholds,
) -> Vec<Flag> {
    let Some(own) = failures.get(&pr.number) else {
        return Vec::new();
    };
    // check -> signature -> the settled failing runs that carried it.
    let mut by_check: BTreeMap<&str, BTreeMap<&str, Vec<&FailureRecord<'_>>>> = BTreeMap::new();
    for record in own {
        if completed_by(record.check, at).is_none() {
            continue;
        }
        for signature in &record.check.signatures {
            by_check
                .entry(record.check.name.as_str())
                .or_default()
                .entry(signature.as_str())
                .or_default()
                .push(record);
        }
    }
    if by_check.is_empty() {
        return Vec::new();
    }
    let mut flags = Vec::new();
    for (name, signatures) in by_check {
        if !still_red(history, pr.number, name, at) {
            continue;
        }
        // (signature, runs, lanes, other PRs) for every repeated signature.
        let mut own_code = Vec::new();
        let mut pre_existing = Vec::new();
        let mut lanes: Vec<String> = Vec::new();
        for (signature, records) in signatures {
            let runs: BTreeSet<u64> = records.iter().map(|record| record.check.id).collect();
            if runs.len() < thresholds.repeat_failures {
                continue;
            }
            for record in &records {
                if !lanes.contains(&record.lane) {
                    lanes.push(record.lane.clone());
                }
            }
            let others = other_prs_failing(failures, pr.number, name, signature, at, thresholds);
            if others.len() >= thresholds.pre_existing_other_prs {
                pre_existing.push((signature, runs.len(), others));
            } else {
                own_code.push((signature, runs.len()));
            }
        }
        if own_code.is_empty() && pre_existing.is_empty() {
            continue;
        }
        let shared = own_code.is_empty();
        let (shared_tests, related_prs): (Vec<String>, Vec<u64>) = if shared {
            let mut related: BTreeSet<u64> = BTreeSet::new();
            for (_, _, prs) in &pre_existing {
                related.extend(prs);
            }
            (
                pre_existing
                    .iter()
                    .map(|(signature, _, _)| (*signature).to_owned())
                    .collect(),
                related.into_iter().collect(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let (verdict, mut evidence) = if shared {
            let mut others: BTreeSet<u64> = BTreeSet::new();
            for (_, _, prs) in &pre_existing {
                others.extend(prs);
            }
            let tests = pre_existing
                .iter()
                .map(|(signature, runs, _)| format!("`{signature}` ({runs} runs)"))
                .collect();
            (
                "failing on main/pre-existing",
                format!(
                    "`{name}` failed {} repeatedly, but the same test(s) also failed on {} in the last {}h",
                    listed(tests),
                    listed(others.iter().map(|pr| format!("#{pr}")).collect()),
                    thresholds.pre_existing_window_hours
                ),
            )
        } else {
            let tests = own_code
                .iter()
                .map(|(signature, runs)| format!("`{signature}` ({runs} runs)"))
                .collect();
            let mut text = format!("`{name}` failed {} repeatedly", listed(tests));
            if !pre_existing.is_empty() {
                let _ = write!(
                    text,
                    "; {} more failing test(s) are also failing on other PRs",
                    pre_existing.len()
                );
            }
            ("code failure, not flake", text)
        };
        let _ = write!(evidence, " on {}", listed(lanes));
        flags.push(Flag {
            pr: pr.number,
            kind: FlagKind::RepeatTestFailure,
            key: name.to_owned(),
            verdict: verdict.to_owned(),
            evidence,
            head_sha: head.to_owned(),
            // A failure shared with other pull requests is a main-health
            // signal, not an owner action: it gets a shared digest line.
            route: if shared {
                DigestRoute::Shared
            } else {
                DigestRoute::PerPr
            },
            shared_tests,
            related_prs,
        });
    }
    flags
}

/// Whether the pull request is still red on `name` at `at`: its latest
/// settled pass-or-fail outcome for the check is a failure.
fn still_red(history: &RepoHistory, pr: u64, name: &str, at: DateTime<Utc>) -> bool {
    outcomes_for(history, pr)
        .iter()
        .filter(|record| record.check.name == name)
        .filter(|record| record.check.failed() || record.check.succeeded())
        .filter_map(|record| completed_by(record.check, at).map(|time| (time, record)))
        .max_by_key(|(time, record)| (*time, record.check.id))
        .is_some_and(|(_, record)| record.check.failed())
}

/// Other pull requests whose `name` check failed with `signature` within the
/// pre-existing window before `at`.
fn other_prs_failing(
    failures: &BTreeMap<u64, Vec<FailureRecord<'_>>>,
    pr: u64,
    name: &str,
    signature: &str,
    at: DateTime<Utc>,
    thresholds: &Thresholds,
) -> BTreeSet<u64> {
    let window_start = at - Duration::hours(thresholds.pre_existing_window_hours);
    failures
        .iter()
        .filter(|(number, _)| **number != pr)
        .filter(|(_, records)| {
            records.iter().any(|record| {
                record.check.name == name
                    && record.check.signatures.iter().any(|s| s == signature)
                    && completed_by(record.check, at).is_some_and(|time| time >= window_start)
            })
        })
        .map(|(number, _)| *number)
        .collect()
}

/// The first three items, then "and N more".
fn listed(items: Vec<String>) -> String {
    const SHOWN: usize = 3;
    let more = items.len().saturating_sub(SHOWN);
    let mut text = items.into_iter().take(SHOWN).collect::<Vec<_>>().join(", ");
    if more > 0 {
        let _ = write!(text, " and {more} more");
    }
    text
}

/// Arming state reconstructed from the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arming {
    Idle,
    Armed,
    Queued,
    EjectedFailedChecks,
    Done,
}

fn arming_at(pr: &PrHistory, at: DateTime<Utc>) -> Arming {
    let mut state = Arming::Idle;
    for event in pr.events.iter().filter(|event| event.at <= at) {
        state = match &event.kind {
            QueueEventKind::Armed => Arming::Armed,
            // GitHub consumes auto-merge on enqueue; a disable after an
            // ejection keeps the ejection (the pull request is still out of
            // the queue for its checks), any other disable unarms.
            QueueEventKind::Disarmed { .. } => match state {
                Arming::EjectedFailedChecks => Arming::EjectedFailedChecks,
                Arming::Done => Arming::Done,
                _ => Arming::Idle,
            },
            QueueEventKind::Enqueued => Arming::Queued,
            QueueEventKind::Removed { reason } => {
                if reason.eq_ignore_ascii_case("failed_checks") {
                    Arming::EjectedFailedChecks
                } else if reason.eq_ignore_ascii_case("merged") {
                    Arming::Done
                } else {
                    Arming::Idle
                }
            }
            QueueEventKind::Merged | QueueEventKind::Closed => Arming::Done,
            QueueEventKind::Reopened => Arming::Idle,
            QueueEventKind::ForcePushed { .. } => state,
        };
    }
    state
}

/// The latest attempt of `name` on `head` that had started by `at`.
fn latest_attempt<'a>(head: &'a HeadFact, name: &str, at: DateTime<Utc>) -> Option<&'a CheckFact> {
    head.checks
        .iter()
        .filter(|check| check.name == name)
        .filter(|check| {
            check
                .started_at
                .or(check.completed_at)
                .is_some_and(|start| start <= at)
        })
        .max_by_key(|check| (check.started_at.or(check.completed_at), check.id))
}

/// Flag 2.
fn red_while_armed(
    pr: &PrHistory,
    history: &RepoHistory,
    at: DateTime<Utc>,
    thresholds: &Thresholds,
) -> Vec<Flag> {
    let arming = arming_at(pr, at);
    if !matches!(arming, Arming::Armed | Arming::EjectedFailedChecks) {
        return Vec::new();
    }
    let Some(head) = head_at(pr, at) else {
        return Vec::new();
    };
    let mut flags = Vec::new();
    for name in &history.required_checks {
        let Some(check) = latest_attempt(head, name, at) else {
            continue;
        };
        let Some(completed) = completed_by(check, at) else {
            continue;
        };
        if !check.failed() {
            continue;
        }
        let red_for = at - completed;
        if red_for <= Duration::minutes(thresholds.red_minutes) {
            continue;
        }
        let state = if arming == Arming::Armed {
            "auto-merge armed"
        } else {
            "ejected (failed_checks), not re-armed"
        };
        flags.push(Flag {
            pr: pr.number,
            kind: FlagKind::RedWhileArmed,
            key: format!("{}|{name}", short(&head.sha)),
            verdict: "armed but blocked on a red required check".to_owned(),
            evidence: format!(
                "{state}, not queued; required `{name}` on head {} red since {} (> {} min), no push since",
                short(&head.sha),
                completed.format("%Y-%m-%d %H:%MZ"),
                thresholds.red_minutes
            ),
            head_sha: head.sha.clone(),
            route: DigestRoute::PerPr,
            shared_tests: Vec::new(),
            related_prs: Vec::new(),
        });
    }
    flags
}

/// Flag 3.
fn repeated_ejection(
    history: &RepoHistory,
    pr: &PrHistory,
    head: &str,
    at: DateTime<Utc>,
    thresholds: &Thresholds,
) -> Option<Flag> {
    let mut named: Vec<&super::GroupRun> = history
        .group_runs
        .iter()
        .filter(|run| run.pr == Some(pr.number))
        .collect();
    named.sort_by_key(|run| (run.created_at, run.id));
    let failed: Vec<&super::GroupRun> = named
        .iter()
        .copied()
        .filter(|run| group_failed(history, run) && run.settled_at() <= at)
        .collect();
    if failed.len() < thresholds.failed_groups {
        return None;
    }
    let last_failure = failed.iter().map(|run| run.settled_at()).max()?;
    // A later passing group named for the pull request clears it.
    let passed_since = named
        .iter()
        .any(|run| run.gate_passed() && run.created_at > last_failure && run.created_at <= at);
    if passed_since {
        return None;
    }
    let parts: Vec<String> = failed
        .iter()
        .map(|run| {
            let jobs: Vec<&str> = run
                .required_jobs
                .iter()
                .filter(|job| job.failed() && history.required_checks.contains(&job.name))
                .map(|job| job.name.as_str())
                .collect();
            format!(
                "{} `{}` failed ({})",
                run.settled_at().format("%m-%d %H:%MZ"),
                jobs.join("`,`"),
                parent_status(history, run)
            )
        })
        .collect();
    let mut evidence = format!(
        "{} merge groups named for #{} failed a required job (named for, not proved culprit): {}",
        failed.len(),
        pr.number,
        parts.join("; ")
    );
    // The batch attributor, when it ruled on every failed group, can clear
    // the named pull request: the flag stays on the comment, labelled, and
    // leaves the digest.
    let cleared = failed.iter().all(|run| {
        run.attribution
            .as_ref()
            .is_some_and(|attribution| attribution.clears(pr.number))
    });
    let (verdict, route) = if cleared {
        let blamed: BTreeSet<u64> = failed
            .iter()
            .filter_map(|run| run.attribution.as_ref()?.implicated_pr)
            .collect();
        let label = if blamed.is_empty() {
            "neighbour or infrastructure (attributor)".to_owned()
        } else {
            let blamed_list: Vec<String> = blamed.iter().map(|n| format!("#{n}")).collect();
            format!("neighbour of {}", blamed_list.join(", "))
        };
        let _ = write!(evidence, "; attributor: {label}");
        (label, DigestRoute::CommentOnly)
    } else {
        (
            "repeatedly ejected from the merge queue".to_owned(),
            DigestRoute::PerPr,
        )
    };
    Some(Flag {
        pr: pr.number,
        kind: FlagKind::RepeatedEjection,
        key: String::new(),
        verdict,
        evidence,
        head_sha: head.to_owned(),
        route,
        shared_tests: Vec::new(),
        related_prs: Vec::new(),
    })
}

/// Whether a merge group failed a job named in the required checks. An
/// advisory job's failure never counts.
fn group_failed(history: &RepoHistory, run: &super::GroupRun) -> bool {
    run.required_jobs
        .iter()
        .any(|job| job.failed() && history.required_checks.contains(&job.name))
}

fn parent_status(history: &RepoHistory, run: &super::GroupRun) -> String {
    let Some(parent) = run.parent_sha.as_deref() else {
        return "parent unknown".to_owned();
    };
    match history
        .group_runs
        .iter()
        .find(|candidate| candidate.head_sha == parent)
    {
        Some(group) => {
            let who = group
                .pr
                .map_or_else(|| "?".to_owned(), |pr| format!("#{pr}"));
            let status = if group_failed(history, group) {
                "failed"
            } else if group.gate_passed() {
                "passed"
            } else {
                group.conclusion.as_deref().unwrap_or("pending")
            };
            format!("parent group {who} {status}")
        }
        None => format!("parent {} is a base commit", short(parent)),
    }
}

/// Flag 4.
fn rebase_treadmill(
    pr: &PrHistory,
    head: &str,
    at: DateTime<Utc>,
    thresholds: &Thresholds,
) -> Option<Flag> {
    let window_start = at - Duration::hours(thresholds.replacement_window_hours);
    let qualifying = qualifying_replacements(pr)
        .into_iter()
        .filter(|(time, _, _)| *time > window_start && *time <= at)
        .collect::<Vec<_>>();
    if qualifying.len() < thresholds.replacements {
        return None;
    }
    let chain: Vec<String> = qualifying
        .iter()
        .map(|(time, previous, next)| {
            format!(
                "{}→{} at {}",
                short(previous),
                short(next),
                time.format("%H:%MZ")
            )
        })
        .collect();
    Some(Flag {
        pr: pr.number,
        kind: FlagKind::RebaseTreadmill,
        key: String::new(),
        verdict: "hand-rebase treadmill (inferred)".to_owned(),
        evidence: format!(
            "head replaced {} times in {}h, each cancelling the previous head's gate run while the merge base advanced (inferred from merge bases): {}",
            qualifying.len(),
            thresholds.replacement_window_hours,
            chain.join(", ")
        ),
        head_sha: head.to_owned(),
        route: DigestRoute::PerPr,
        shared_tests: Vec::new(),
        related_prs: Vec::new(),
    })
}

/// Head replacements `(when, previous, next)` where the previous head's gate
/// run was cancelled without a settled required outcome and the merge base
/// moved. Used by flag 4 and by the gatherer to decide which merge bases to
/// read.
#[must_use]
pub fn qualifying_replacements(pr: &PrHistory) -> Vec<(DateTime<Utc>, String, String)> {
    let mut out = Vec::new();
    for pair in pr.heads.windows(2) {
        let (previous, next) = (&pair[0], &pair[1]);
        if !gate_cancelled(previous) {
            continue;
        }
        match (&previous.merge_base, &next.merge_base) {
            (Some(before), Some(after)) if before != after => {
                out.push((next.first_seen_at, previous.sha.clone(), next.sha.clone()));
            }
            _ => {}
        }
    }
    out
}

/// Whether a head's gate run was cancelled: its latest gate-workflow run
/// concluded `cancelled`.
#[must_use]
pub fn gate_cancelled(head: &HeadFact) -> bool {
    head.gate_runs
        .iter()
        .max_by_key(|run| (run.created_at, run.id))
        .is_some_and(|run| run.conclusion.as_deref() == Some("cancelled"))
}

/// Flag 5 (only called when another flag holds).
fn split_candidate(
    pr: &PrHistory,
    head: &str,
    at: DateTime<Utc>,
    thresholds: &Thresholds,
) -> Option<Flag> {
    let mut reasons = Vec::new();
    if let Some(created) = pr.created_at {
        let open_for = at - created;
        if open_for > Duration::days(thresholds.split_days) {
            reasons.push(format!("open since {}", created.format("%Y-%m-%d")));
        }
    }
    if pr.changed_files > thresholds.split_files {
        reasons.push(format!("{} files", pr.changed_files));
    }
    if pr.commits > thresholds.split_commits {
        reasons.push(format!("{} commits", pr.commits));
    }
    if reasons.is_empty() {
        return None;
    }
    Some(Flag {
        pr: pr.number,
        kind: FlagKind::SplitCandidate,
        key: String::new(),
        verdict: "split candidate (advisory)".to_owned(),
        evidence: format!(
            "{} (thresholds: > {} days, > {} files, > {} commits)",
            reasons.join(", "),
            thresholds.split_days,
            thresholds.split_files,
            thresholds.split_commits
        ),
        head_sha: head.to_owned(),
        route: DigestRoute::PerPr,
        shared_tests: Vec::new(),
        related_prs: Vec::new(),
    })
}
