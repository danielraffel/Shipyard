//! Rollup planning for a base without a merge queue, when it is backed up.
//!
//! A user-owned repository has no merge queue, so the steward merges each
//! approved head on its own ([`super::exact_head_merge`]). When several are
//! ready at once they land one after another, and nothing ever tests them
//! together. A rollup ([`crate::rollup`]) is the batch shape for such a base:
//! one branch that merges each member's exact approved head onto the base,
//! tested once and landed as one pull request on the members' own approvals.
//!
//! This pass decides, from the steward's own `ready` rows, which pull
//! requests would form a rollup now. It is configured in the base branch's
//! tracked `.shipyard/config.toml`, read through the GitHub API, never from a
//! head:
//!
//! ```toml
//! [auto_merge.rollup]
//! mode = "shadow"        # off | shadow (default) | apply
//! min_ready = 3          # rollups start at this many ready pull requests
//! max_members = 4        # 2 to 4
//! sensitive_paths = []   # globs that keep a pull request out, on top of the defaults
//! max_ready = 12         # more ready than this: skip the plan (skipped_too_many)
//! interval_minutes = 30  # plan at most this often per repository; 0 plans every pass
//! ```
//!
//! A pull request joins only when all of these hold, judged at this pass:
//!
//! * the steward decided it is ready (exact-head merge path, required checks
//!   green, `CLEAN`), it is not a draft, and it carries no `owner-decision`,
//!   `superseded` or `PARKED/*` label;
//! * the live exact-head approval gate ([`crate::head_approval`], dry run)
//!   approves its current head, by a verdict that names that head (a carried
//!   approval does not count);
//! * its changed files (renames' sources included, GitHub's 300-file compare
//!   cap refuses) touch no [`DEFAULT_SENSITIVE_PATHS`] glob, no configured
//!   `sensitive_paths` glob, no `[train] exclude_paths` glob and no carry
//!   exclusion of the base ([`crate::head_approval::DEFAULT_CARRY_EXCLUDED_PATHS`]
//!   unless the base sets its own);
//! * it shares no file with an older member, and no `[train] hot_files` glob.
//!
//! Members are taken oldest first, at most `max_members`; fewer than two is no
//! rollup.
//!
//! **Read cost is bounded.** A plan is made at most once per
//! `interval_minutes` per repository: an applied pass stamps the plan under
//! the steward's state directory (`rollup-plan/`), and passes in between
//! report that plan with `cached_at` and read nothing. More than `max_ready`
//! ready pull requests skips the plan. Reading stops once `max_members`
//! members are chosen, and at the pass's time budget (`deadline`, which is
//! never stamped). Rows the pass merged or enqueued this tick are not ready.
//!
//! **Nothing here writes.** It reads the base config, the approval policy,
//! each candidate's gate and files, and returns a report. `shadow` reports
//! the batch it would form. `apply` reports the same and says it is not
//! available: forming a rollup means pushing a `shipyard-train/rollup-*`
//! branch and opening its pull request, which the steward is not authorised
//! to do, and landing one needs the base to take rollups (`[train] enabled =
//! true`, `mode = "rollup"`, `landing = "direct"`). Until both exist, `apply`
//! fails closed: every ready pull request still merges on its own, exactly as
//! with `off`.

use std::collections::BTreeSet;

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{PrReport, RepoObservation, StewardDecision};
use crate::head_approval::HeadGate;
use crate::rollup;

/// The `[auto_merge]` sub-table the keys live in.
pub(super) const CONFIG_TABLE: &str = "rollup";

/// Paths that keep a pull request out of a rollup whatever the base config
/// says: workflows, the `ghapp` guards and wrappers, guard scripts, the
/// exact-head approval gate and the rollup and exact-head merge code itself.
/// The base's carry exclusions apply on top.
pub(super) const DEFAULT_SENSITIVE_PATHS: &[&str] = &[
    ".github/workflows/**",
    "scripts/ghapp*",
    "scripts/*guard*",
    "src/head_approval*",
    "src/head_approval/**",
    "src/rollup*",
    "src/rollup/**",
    "src/app/rollup_cmd*",
    "src/app/merge_steward_cmd/exact_head_merge*",
    "src/app/merge_steward_cmd/rollup_pass*",
];

/// Default `min_ready`.
pub(super) const DEFAULT_MIN_READY: usize = 3;
/// Default `max_ready`.
pub(super) const DEFAULT_MAX_READY: usize = 12;
/// Default `interval_minutes`.
pub(super) const DEFAULT_INTERVAL_MINUTES: usize = 30;
/// The steward state subdirectory that holds each repository's last plan.
pub(super) const CACHE_DIR: &str = "rollup-plan";

/// What the pass does.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Mode {
    /// Nothing, and no reads.
    Off,
    /// Report the batch it would form. The default.
    #[default]
    Shadow,
    /// Form and land it. Not available yet: reported, and fails closed.
    Apply,
}

/// The base's `[auto_merge.rollup]` table.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct Policy {
    pub mode: Mode,
    pub min_ready: usize,
    pub max_members: usize,
    pub max_ready: usize,
    pub interval_minutes: usize,
    /// [`DEFAULT_SENSITIVE_PATHS`] then the configured `sensitive_paths`.
    pub sensitive_paths: Vec<String>,
    /// Why a value was ignored, when one was. A bad value never widens
    /// anything: an unknown mode is `shadow`, a bad number its default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            min_ready: DEFAULT_MIN_READY,
            max_members: rollup::MEMBER_CAP,
            max_ready: DEFAULT_MAX_READY,
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            sensitive_paths: DEFAULT_SENSITIVE_PATHS
                .iter()
                .map(|path| (*path).to_owned())
                .collect(),
            error: None,
        }
    }
}

impl Policy {
    /// Read `[auto_merge.rollup]` from a parsed base config; absent is the
    /// default (shadow).
    pub(super) fn from_table(table: &toml::Table) -> Self {
        let mut policy = Self::default();
        let Some(section) = table
            .get(crate::merge_steward::ejection_guards::CONFIG_TABLE)
            .and_then(toml::Value::as_table)
            .and_then(|auto_merge| auto_merge.get(CONFIG_TABLE))
        else {
            return policy;
        };
        let Some(section) = section.as_table() else {
            policy.error = Some("[auto_merge.rollup] is not a table".to_owned());
            return policy;
        };
        let mut errors = Vec::new();
        match section.get("mode").map(toml::Value::as_str) {
            None => {}
            Some(Some("off")) => policy.mode = Mode::Off,
            Some(Some("shadow")) => policy.mode = Mode::Shadow,
            Some(Some("apply")) => policy.mode = Mode::Apply,
            Some(other) => errors.push(format!(
                "mode {} is not off, shadow or apply; using shadow",
                other.map_or_else(|| "(not a string)".to_owned(), |text| format!("{text:?}"))
            )),
        }
        let mut count = |key: &str, range: std::ops::RangeInclusive<usize>, slot: &mut usize| {
            if let Some(value) = section.get(key) {
                match value
                    .as_integer()
                    .and_then(|count| usize::try_from(count).ok())
                    .filter(|count| range.contains(count))
                {
                    Some(count) => *slot = count,
                    None => errors.push(format!(
                        "{key} must be a whole number from {} to {}; using {slot}",
                        range.start(),
                        range.end()
                    )),
                }
            }
        };
        count("min_ready", 2..=100, &mut policy.min_ready);
        count(
            "max_members",
            2..=rollup::MEMBER_CAP,
            &mut policy.max_members,
        );
        count("max_ready", 2..=100, &mut policy.max_ready);
        count("interval_minutes", 0..=1440, &mut policy.interval_minutes);
        if let Some(value) = section.get("sensitive_paths") {
            if let Some(extra) = value.as_array().and_then(|items| {
                items
                    .iter()
                    .map(|item| item.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            }) {
                policy.sensitive_paths.extend(extra);
            } else {
                // A list that cannot be read could have named a path to keep
                // out: keep everything out instead.
                errors.push(
                    "sensitive_paths is not a list of strings; every path is treated as \
                     sensitive"
                        .to_owned(),
                );
                policy.sensitive_paths.push("**".to_owned());
            }
        }
        if !errors.is_empty() {
            policy.error = Some(format!("[auto_merge.rollup] {}", errors.join("; ")));
        }
        policy
    }
}

/// A ready pull request, as the steward's pass saw it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Ready {
    pub pr: u64,
    pub head: String,
    pub draft: bool,
    pub labels: Vec<String>,
}

/// One member of the rollup the pass would form.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct Member {
    pub pr: u64,
    pub head: String,
}

/// A ready pull request left out, and why.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct Skipped {
    pub pr: u64,
    pub why: String,
}

/// What the pass did.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Action {
    /// Fewer than `min_ready` ready: nothing more was read.
    BelowThreshold,
    /// More than `max_ready` ready: nothing more was read.
    SkippedTooMany,
    /// Fewer than two eligible members.
    NoBatch,
    /// Would form `members`.
    Shadow,
    /// Would form `members`, but merged nothing as a batch.
    ApplyUnavailable,
    /// The pass time budget ran out mid-plan; `members` is partial.
    Deadline,
    /// A needed read failed; nothing was planned.
    Unreadable,
}

/// What the pass decided for one repository.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct Report {
    pub mode: Mode,
    /// Ready pull requests this pass, oldest first.
    pub ready: Vec<u64>,
    pub min_ready: usize,
    pub action: Action,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Member>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When this is an earlier pass's plan, reused within `interval_minutes`:
    /// the time that pass made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_at: Option<String>,
}

/// What [`plan`] reads, injected so the decision is testable on its own.
pub(super) struct Readers<'a> {
    /// The live exact-head gate of `(pr, head)`.
    pub gate: &'a dyn Fn(u64, &str) -> Result<HeadGate, String>,
    /// The files `head` changes against the base.
    pub files: &'a dyn Fn(&str) -> Result<Vec<String>, String>,
    /// Whether the pass time budget is spent; checked before each candidate.
    pub expired: &'a dyn Fn() -> bool,
}

/// The ready rows of a server-enforced exact-head merge pass, oldest first:
/// decided ready at the observed head. Rows the apply pass merged or enqueued
/// this tick stay in: the plan is the pre-mutation snapshot, so an applied
/// pass still reports the rollup it did not form.
pub(super) fn ready_rows(observation: &RepoObservation, prs: &[PrReport]) -> Vec<Ready> {
    let mut ready: Vec<Ready> = prs
        .iter()
        .filter(|row| matches!(row.decision, StewardDecision::ArmMergeQueue))
        .filter_map(|row| {
            let pr = observation
                .prs
                .iter()
                .find(|pr| pr.fact.number == row.number)?;
            // A row whose head is not the observed head is stale: skip it.
            (pr.fact.head_sha.eq_ignore_ascii_case(&row.head_sha)).then(|| Ready {
                pr: row.number,
                head: row.head_sha.to_ascii_lowercase(),
                draft: pr.fact.draft,
                labels: pr.fact.labels.clone(),
            })
        })
        .collect();
    ready.sort_by_key(|ready| ready.pr);
    ready
}

/// Why `ready` is out before any read: a draft or a fleet hold label.
fn static_refusal(ready: &Ready) -> Option<String> {
    if ready.draft {
        return Some("a draft".to_owned());
    }
    crate::merge_steward::excluded_admission_label(&ready.labels)
        .map(|label| format!("carries the {label} label"))
}

/// One candidate, read through `readers`: the gate first, then (only when
/// the gate admits it) its files.
fn read_candidate(item: &Ready, readers: &Readers<'_>) -> rollup::Candidate {
    let blocked = static_refusal(item).or_else(|| match (readers.gate)(item.pr, &item.head) {
        // The rollup gate requires a verdict that names this head; a base
        // that requires no approval has nothing to roll up on.
        Ok(HeadGate::NotRequired) => {
            Some("head approval is not required here, so no approval names it".to_owned())
        }
        Ok(gate) => rollup::member_refusal(item.pr, &gate, true),
        Err(error) => Some(format!("its review cannot be read: {error}")),
    });
    let (files, blocked) = match blocked {
        Some(why) => (Vec::new(), Some(why)),
        None => match (readers.files)(&item.head) {
            Ok(files) => (files, None),
            Err(error) => (
                Vec::new(),
                Some(format!("its files cannot be read: {error}")),
            ),
        },
    };
    rollup::Candidate {
        pr: item.pr,
        head: item.head.clone(),
        files,
        blocked,
    }
}

/// The `[train]`-aware formation config for `policy`.
fn formation(policy: &Policy, train: &rollup::Config) -> rollup::Config {
    rollup::Config {
        max_members: policy.max_members,
        exclude_paths: policy
            .sensitive_paths
            .iter()
            .chain(&train.exclude_paths)
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        hot_files: train.hot_files.clone(),
        require_counted_approval: true,
        ..rollup::Config::default()
    }
}

/// Decide the rollup for one pass. Reads only through `readers`, only when
/// `ready` is from `policy.min_ready` to `policy.max_ready`, and only until
/// `policy.max_members` members are chosen or the budget is spent.
pub(super) fn plan(
    policy: &Policy,
    train: &rollup::Config,
    carry_excluded: &[String],
    ready: &[Ready],
    readers: &Readers<'_>,
) -> Report {
    let mut report = Report {
        mode: policy.mode,
        ready: ready.iter().map(|ready| ready.pr).collect(),
        min_ready: policy.min_ready,
        action: Action::BelowThreshold,
        members: Vec::new(),
        skipped: Vec::new(),
        detail: policy.error.clone(),
        cached_at: None,
    };
    if ready.len() < policy.min_ready {
        return report;
    }
    if ready.len() > policy.max_ready {
        report.action = Action::SkippedTooMany;
        report.detail = Some(
            report
                .detail
                .take()
                .into_iter()
                .chain([format!(
                    "{} ready is more than max_ready ({}); no plan read",
                    ready.len(),
                    policy.max_ready
                )])
                .collect::<Vec<_>>()
                .join("; "),
        );
        return report;
    }
    let config = formation(policy, train);
    let mut candidates = Vec::new();
    let (mut chosen, mut skipped) = (Vec::new(), Vec::new());
    let mut unread: Option<(usize, String)> = None;
    for (index, item) in ready.iter().enumerate() {
        if chosen.len() == policy.max_members {
            unread = Some((
                index,
                format!(
                    "not read: the rollup is full ({} members)",
                    policy.max_members
                ),
            ));
            break;
        }
        if (readers.expired)() {
            unread = Some((index, "not read: the pass time budget is spent".to_owned()));
            report.action = Action::Deadline;
            break;
        }
        candidates.push(read_candidate(item, readers));
        (chosen, skipped) = rollup::form(&candidates, &config, carry_excluded);
    }
    report.skipped = skipped
        .into_iter()
        .map(|(pr, why)| Skipped { pr, why })
        .collect();
    if let Some((index, why)) = unread {
        report
            .skipped
            .extend(ready[index..].iter().map(|item| Skipped {
                pr: item.pr,
                why: why.clone(),
            }));
    }
    report.members = chosen
        .iter()
        .filter_map(|pr| {
            candidates
                .iter()
                .find(|candidate| candidate.pr == *pr)
                .map(|candidate| Member {
                    pr: candidate.pr,
                    head: candidate.head.clone(),
                })
        })
        .collect();
    if report.action == Action::Deadline {
        return report;
    }
    if report.members.len() < 2 {
        report.members.clear();
        report.action = Action::NoBatch;
    } else if policy.mode == Mode::Apply {
        report.action = Action::ApplyUnavailable;
        report.detail = Some(apply_unavailable(train, report.detail.take()));
    } else {
        report.action = Action::Shadow;
    }
    report
}

/// Why `apply` formed nothing, after any config error in `prior`.
fn apply_unavailable(train: &rollup::Config, prior: Option<String>) -> String {
    let mut why: Vec<String> = prior.into_iter().collect();
    why.push(
        "the steward does not push rollup branches or open rollup pull requests; each ready \
         pull request merges on its own"
            .to_owned(),
    );
    if !(train.rolls_up() && train.landing == rollup::Landing::Direct) {
        why.push(
            "the base does not take direct rollups ([train] enabled = true, mode = \"rollup\", \
             landing = \"direct\")"
                .to_owned(),
        );
    }
    why.join("; ")
}

/// A stamped plan: when it was made, for how long it stands, and the plan.
#[derive(Serialize, Deserialize)]
struct Stamp {
    at: DateTime<Utc>,
    interval_minutes: usize,
    report: Report,
}

/// The stamp file of `repo`'s `base` under `dir`.
fn stamp_path(dir: &Path, repo: &str, base: &str) -> PathBuf {
    let slug = |text: &str| {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    dir.join(format!("{}__{}.json", slug(repo), slug(base)))
}

/// The stamped plan, when one is still within its interval at `now`.
fn fresh_stamp(path: &Path, now: DateTime<Utc>) -> Option<Report> {
    let stamp: Stamp = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let age = now.signed_duration_since(stamp.at);
    let interval = chrono::Duration::minutes(i64::try_from(stamp.interval_minutes).ok()?);
    (age >= chrono::Duration::zero() && age < interval).then(|| Report {
        cached_at: Some(stamp.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        ..stamp.report
    })
}

/// Stamp `report`, best effort: a failed write only costs a re-plan.
fn write_stamp(path: &Path, report: &Report, interval_minutes: usize, now: DateTime<Utc>) {
    if interval_minutes == 0 {
        return;
    }
    let Ok(body) = serde_json::to_vec(&Stamp {
        at: now,
        interval_minutes,
        report: report.clone(),
    }) else {
        return;
    };
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_ok()
    {
        let staging = path.with_extension("json.tmp");
        if std::fs::write(&staging, body).is_ok() {
            let _ = std::fs::rename(&staging, path);
        }
    }
}

/// Where and whether [`run`] keeps its plan between passes.
pub(super) struct Throttle<'a> {
    /// The stamp directory (`<state>/rollup-plan`); `None` never caches.
    pub dir: Option<&'a Path>,
    /// Whether this pass may write a stamp (an applied pass).
    pub write: bool,
    pub now: DateTime<Utc>,
    /// Whether the pass time budget is spent.
    pub expired: &'a dyn Fn() -> bool,
}

/// Run the pass for one observed repository after its plan was applied.
/// `None` when it does not apply (a merge-queue base, fewer than two ready
/// rows, or `mode = "off"`). Reads GitHub only; its one local write is the
/// plan stamp under `throttle.dir`, on an applied pass.
pub(super) fn run(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    observation: &RepoObservation,
    merge_path: &str,
    prs: &[PrReport],
    throttle: &Throttle<'_>,
) -> Option<Report> {
    if observation.merge_queue || merge_path != "server_enforced_exact_head_merge" {
        return None;
    }
    let ready = ready_rows(observation, prs);
    if ready.len() < 2 {
        return None;
    }
    let (repo, base) = (observation.repo.as_str(), observation.base.as_str());
    let stamp = throttle.dir.map(|dir| stamp_path(dir, repo, base));
    if let Some(cached) = stamp
        .as_deref()
        .and_then(|path| fresh_stamp(path, throttle.now))
    {
        return Some(cached);
    }
    let (report, interval) = plan_live(run_gh, repo, base, &ready, throttle.expired)?;
    if let (Some(path), Some(interval), true) = (stamp.as_deref(), interval, throttle.write)
        && report.action != Action::Deadline
    {
        write_stamp(path, &report, interval, throttle.now);
    }
    Some(report)
}

/// [`run`]'s live plan, and the interval to stamp it for (`None`: do not
/// stamp an unreadable result).
fn plan_live(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    repo: &str,
    base: &str,
    ready: &[Ready],
    expired: &dyn Fn() -> bool,
) -> Option<(Report, Option<usize>)> {
    let unreadable = |detail: String| {
        Some((
            Report {
                mode: Mode::Shadow,
                ready: ready.iter().map(|ready| ready.pr).collect(),
                min_ready: DEFAULT_MIN_READY,
                action: Action::Unreadable,
                members: Vec::new(),
                skipped: Vec::new(),
                detail: Some(detail),
                cached_at: None,
            },
            None,
        ))
    };
    let text = match rollup::base_config_text(run_gh, repo, base) {
        Ok(text) => text,
        Err(error) => return unreadable(error),
    };
    let table = match text.as_deref().map(str::parse::<toml::Table>).transpose() {
        Ok(table) => table.unwrap_or_default(),
        Err(error) => {
            return unreadable(format!(
                "{} on {base} does not parse: {error}",
                crate::head_approval::CONFIG_PATH
            ));
        }
    };
    let policy = Policy::from_table(&table);
    if policy.mode == Mode::Off {
        return None;
    }
    // A `[train]` table that does not parse adds no exclusions it might have
    // named: refuse the plan rather than guess.
    let train = match rollup::Config::from_table(&table) {
        Ok(train) => train,
        Err(error) => return unreadable(error),
    };
    let interval = Some(policy.interval_minutes);
    if ready.len() < policy.min_ready || ready.len() > policy.max_ready {
        let none = Readers {
            gate: &|_, _| Err("not read".to_owned()),
            files: &|_| Err("not read".to_owned()),
            expired,
        };
        return Some((plan(&policy, &train, &[], ready, &none), interval));
    }
    let approval = match crate::head_approval::read_policy(run_gh, repo, base) {
        Ok(approval) => approval,
        Err(error) => return unreadable(format!("approval policy unreadable: {error}")),
    };
    let gate = |pr: u64, head: &str| {
        crate::head_approval::evaluate_member(run_gh, repo, pr, head, &approval)
    };
    let files = |head: &str| rollup::member_files(run_gh, repo, base, head);
    let report = plan(
        &policy,
        &train,
        &approval.carry_excluded_paths(),
        ready,
        &Readers {
            gate: &gate,
            files: &files,
            expired,
        },
    );
    Some((report, interval))
}

#[cfg(test)]
mod tests;
