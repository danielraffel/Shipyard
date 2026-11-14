//! Tick-start invariants: the admission set-up facts a steward pass needs to
//! mean anything.
//!
//! A pass can run cleanly and still admit nothing for hours when the
//! repository around it is mis-wired: a required check no workflow produces,
//! an approval rule the gate never counts, a base with no automatic admission
//! path. None of those is a pull-request error, so `rc`, `errors` and
//! `pr_refused` stay quiet while every head waits. Each check here is a
//! deterministic read over what the pass already observed, and reports one
//! row `{id, ok, detail, fix}` in the repository's report:
//!
//! * `required_context_producer`: every required context on the base has a
//!   producer: a workflow on the base that names it, a check of that name on
//!   some open head, a binding to an app other than GitHub Actions, or a
//!   declared `[merge_steward.invariants] external_producers` entry.
//! * `approval_admission_config`: a base that admits by exact-head approval
//!   has the configuration that makes the steward admit at all: an app-bound
//!   approval context or a merge queue, the `[auto_merge]` signer and stamp
//!   lists, and an approval rule that counts.
//! * `required_never_ran`: no required context has gone unrun on every open
//!   head for longer than an hour.
//! * `reviewer_verdicts_counted`: approvals are being posted but none has
//!   counted for over an hour while heads wait on them.
//!
//! The fleet-level `config_clone` invariant (every fleet repository has a
//! steward config clone) runs before any pass, in [`clones`].
//!
//! Costs: the base config read is the pass's own, served from its cache; the
//! workflows are read from the checkout the pass runs in when that is a
//! checkout of the repository's base, and otherwise from the API once per pass
//! with each workflow blob cached on disk by its SHA. A failing invariant never
//! changes the pass's exit code; it is reported, dated from its first failing
//! pass, and (only when a host opts in) filed once per change as a
//! `fleet-followup` issue.

pub(crate) mod clones;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{GitHubActions, PrReport, RepoObservation};
use crate::merge_steward::RequiredCheck;

/// Every required context has a producer.
pub(crate) const PRODUCER: &str = "required_context_producer";
/// A base that needs exact-head approval can admit.
pub(crate) const ADMISSION: &str = "approval_admission_config";
/// No required context has gone unrun on every open head for too long.
pub(crate) const NEVER_RAN: &str = "required_never_ran";
/// Posted approvals are counted.
pub(crate) const VERDICTS: &str = "reviewer_verdicts_counted";

/// How long a required context may be absent from every open head.
pub(crate) const NEVER_RAN_LIMIT_MINUTES: i64 = 60;
/// How long a posted approval may go uncounted while no other counts.
pub(crate) const UNCOUNTED_LIMIT_MINUTES: i64 = 60;
/// The GitHub Actions app: a required check bound to it needs a workflow.
const GITHUB_ACTIONS_APP_ID: u64 = crate::reviewer_receipt::SHARED_ACTIONS_APP_ID;
/// Base-config table naming contexts another system posts.
const EXTERNAL_TABLE: &str = "merge_steward";
const EXTERNAL_SUBTABLE: &str = "invariants";
const EXTERNAL_KEY: &str = "external_producers";
/// Label every follow-up issue carries.
const FOLLOWUP_LABEL: &str = "fleet-followup";
/// The approval exclusions that mean the head moved, not that counting broke.
const MOVED_HEAD_EXCLUSIONS: [&str; 2] = ["stale_head", "predates_head"];

/// What the pass needs to run the invariants. `None` on the steward args
/// turns them off (`--no-invariants`).
#[derive(Clone, Debug, Default)]
pub(crate) struct Settings {
    /// The directory the pass runs in; its workflows are read when it is a
    /// checkout of the repository's base.
    pub checkout: Option<PathBuf>,
    /// `[merge_steward.invariants] followup_repo`: where a failing invariant
    /// is filed as a `fleet-followup` issue. `None` (the default) files none.
    pub followup_repo: Option<String>,
}

impl Settings {
    /// Read the follow-up switch from the machine-global config.
    ///
    /// # Errors
    ///
    /// When the config cannot be loaded or the keys have the wrong type.
    pub(crate) fn from_machine_global(
        global_dir: &Path,
        checkout: Option<PathBuf>,
    ) -> Result<Self, String> {
        let config =
            crate::config::LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
                .map_err(|error| format!("failed to load merge_steward.invariants: {error}"))?;
        let enabled = match config.get("merge_steward.invariants.followup_issues") {
            None => false,
            Some(value) => value.as_bool().ok_or_else(|| {
                "merge_steward.invariants.followup_issues must be a boolean".to_owned()
            })?,
        };
        let repo = match config.get("merge_steward.invariants.followup_repo") {
            None => "danielraffel/fleet-handbook".to_owned(),
            Some(value) => value
                .as_str()
                .filter(|repo| repo.split('/').count() == 2)
                .ok_or_else(|| {
                    "merge_steward.invariants.followup_repo must be OWNER/REPO".to_owned()
                })?
                .to_owned(),
        };
        Ok(Self {
            checkout,
            followup_repo: enabled.then_some(repo),
        })
    }
}

/// One invariant's result.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Invariant {
    /// Stable id (the constants above, or `config_clone`).
    pub id: String,
    /// Whether it holds. A skipped invariant reads `ok: true`.
    pub ok: bool,
    /// What was found.
    pub detail: String,
    /// The change that makes it hold; `None` when it holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// It could not be checked this pass; `detail` says why.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    /// When it first failed, across passes (apply passes record it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Self-heal actions taken this pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub healed: Vec<String>,
    /// What filing the `fleet-followup` issue did this pass, when it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub followup: Option<String>,
}

impl Invariant {
    pub(crate) fn holds(id: &str, detail: impl Into<String>) -> Self {
        Self {
            id: id.to_owned(),
            ok: true,
            detail: detail.into(),
            ..Self::default()
        }
    }

    pub(crate) fn fails(id: &str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            id: id.to_owned(),
            ok: false,
            detail: detail.into(),
            fix: Some(fix.into()),
            ..Self::default()
        }
    }

    pub(crate) fn skipped(id: &str, detail: impl Into<String>) -> Self {
        Self {
            id: id.to_owned(),
            ok: true,
            skipped: true,
            detail: detail.into(),
            ..Self::default()
        }
    }

    /// Whether this is an incident: it failed and was not skipped.
    #[must_use]
    pub(crate) fn incident(&self) -> bool {
        !self.ok && !self.skipped
    }

    /// What must stay the same for a repeated failure to be the same
    /// incident: the id and the fix, which name contexts and pull requests
    /// but never an age.
    fn fingerprint(&self) -> String {
        format!("{}|{}", self.id, self.fix.as_deref().unwrap_or_default())
    }
}

/// `ok`, or `FAIL:<id>,<id>` for the summary line.
#[must_use]
pub(crate) fn summary_token(invariants: &[Invariant]) -> String {
    let failed: Vec<&str> = invariants
        .iter()
        .filter(|invariant| invariant.incident())
        .map(|invariant| invariant.id.as_str())
        .collect();
    if failed.is_empty() {
        "ok".to_owned()
    } else {
        format!("FAIL:{}", failed.join(","))
    }
}

/// Everything the four repository invariants read. Pure input to
/// [`evaluate`].
pub(super) struct Facts<'a> {
    pub repo: &'a str,
    pub base: &'a str,
    pub merge_path: &'a str,
    pub approval_context: &'a str,
    /// The base's required checks as the pass read them; empty when the
    /// pass could not read an authoritative set.
    pub required: &'a [RequiredCheck],
    /// Lower-cased names of every check or status on any open head.
    pub head_checks: BTreeSet<String>,
    pub rows: &'a [PrReport],
    /// The base's `.shipyard/config.toml`: `Ok(None)` when it has none.
    pub base_config: &'a Result<Option<toml::Table>, String>,
    /// The text of every workflow file on the base.
    pub workflows: &'a Result<Vec<String>, String>,
    pub now: DateTime<Utc>,
}

/// The four repository invariants, in a fixed order.
#[must_use]
pub(super) fn evaluate(facts: &Facts<'_>) -> Vec<Invariant> {
    vec![
        producer(facts),
        admission(facts),
        never_ran(facts),
        verdicts(facts),
    ]
}

fn external_producers(config: &Result<Option<toml::Table>, String>) -> BTreeSet<String> {
    let Ok(Some(table)) = config else {
        return BTreeSet::new();
    };
    table
        .get(EXTERNAL_TABLE)
        .and_then(toml::Value::as_table)
        .and_then(|section| section.get(EXTERNAL_SUBTABLE))
        .and_then(toml::Value::as_table)
        .and_then(|section| section.get(EXTERNAL_KEY))
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a workflow's text names `context` (lower-cased) as a whole token
/// on a line that is not a comment: a job id, a job `name:`, or a status
/// context. A matrix job's check `build (macos)` is named by `build (`.
fn workflow_names(text: &str, context: &str) -> bool {
    let stem = context
        .split_once(" (")
        .map(|(stem, _)| format!("{stem} ("));
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .any(|line| {
            let line = line.to_ascii_lowercase();
            names_token(&line, context) || stem.as_deref().is_some_and(|stem| line.contains(stem))
        })
}

fn names_token(line: &str, token: &str) -> bool {
    let is_word = |character: char| character.is_ascii_alphanumeric() || "_-".contains(character);
    let mut from = 0;
    while let Some(offset) = line[from..].find(token) {
        let start = from + offset;
        let end = start + token.len();
        let before = line[..start].chars().next_back();
        let after = line[end..].chars().next();
        if !before.is_some_and(is_word) && !after.is_some_and(is_word) {
            return true;
        }
        from = start + token.len().max(1);
    }
    false
}

fn producer(facts: &Facts<'_>) -> Invariant {
    if facts.required.is_empty() {
        return Invariant::skipped(
            PRODUCER,
            "no authoritative required-check set was read this pass",
        );
    }
    let external = external_producers(facts.base_config);
    let mut missing = Vec::new();
    let mut unconfirmed = Vec::new();
    for check in facts.required {
        let name = check.context.to_ascii_lowercase();
        if check.app_id.is_some_and(|app| app != GITHUB_ACTIONS_APP_ID)
            || external.contains(&name)
            || facts.head_checks.contains(&name)
        {
            continue;
        }
        match facts.workflows {
            Ok(texts) if texts.iter().any(|text| workflow_names(text, &name)) => {}
            Ok(_) => missing.push(check.context.clone()),
            Err(_) => unconfirmed.push(check.context.clone()),
        }
    }
    if !missing.is_empty() {
        let names = backticked(&missing);
        return Invariant::fails(
            PRODUCER,
            format!(
                "required {names} on {base} has no producer: no workflow on {base} names it, \
                 no open head carries it, and no external producer is declared, so every pull \
                 request waits on it forever",
                base = facts.base
            ),
            format!(
                "add a workflow on {base} that produces {names} (job name or status context), \
                 or, when another system posts it, list it in `[{EXTERNAL_TABLE}.{EXTERNAL_SUBTABLE}] \
                 {EXTERNAL_KEY}` in .shipyard/config.toml, or drop it from the {base} ruleset's \
                 required checks ({repo})",
                base = facts.base,
                repo = facts.repo
            ),
        );
    }
    if let (Err(error), false) = (facts.workflows, unconfirmed.is_empty()) {
        return Invariant::skipped(
            PRODUCER,
            format!(
                "workflows unreadable ({error}); unconfirmed: {}",
                backticked(&unconfirmed)
            ),
        );
    }
    Invariant::holds(
        PRODUCER,
        format!(
            "{} required context(s) have a producer",
            facts.required.len()
        ),
    )
}

fn auto_merge_table(config: &Result<Option<toml::Table>, String>) -> Option<&toml::Table> {
    match config {
        Ok(Some(table)) => table
            .get(crate::head_approval::CONFIG_TABLE)
            .and_then(toml::Value::as_table),
        _ => None,
    }
}

fn non_empty_list(table: Option<&toml::Table>, key: &str) -> bool {
    table
        .and_then(|table| table.get(key))
        .and_then(toml::Value::as_array)
        .is_some_and(|items| !items.is_empty())
}

fn numbers(rows: &[&PrReport]) -> String {
    rows.iter()
        .map(|row| format!("#{}", row.number))
        .collect::<Vec<_>>()
        .join(",")
}

fn backticked(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Text the steward writes on a head the exact-head gate cannot admit
/// because the approval rule does not apply to the repository.
const APPROVAL_NOT_REQUIRED: &str = "head approval is not required here";

/// The `[auto_merge]` signer and stamp lists a base needs before any
/// reviewer verdict can admit one of its heads.
fn signer_config(
    facts: &Facts<'_>,
    auto: Option<&toml::Table>,
    problems: &mut Vec<String>,
    fixes: &mut Vec<String>,
) {
    use crate::head_approval::{SIGNERS_KEY, STAMP_SESSIONS_KEY};
    if let Err(error) = facts.base_config {
        problems.push(format!(
            "exact-head approval is required but .shipyard/config.toml on {} is unreadable: \
             {error}",
            facts.base
        ));
        return;
    }
    let absent: Vec<&str> = [SIGNERS_KEY, STAMP_SESSIONS_KEY]
        .into_iter()
        .filter(|key| !non_empty_list(auto, key))
        .collect();
    if absent.is_empty() {
        return;
    }
    let names = absent.join(" and ");
    problems.push(format!(
        "exact-head approval is required but .shipyard/config.toml on {} sets no \
         [auto_merge] {names}, so no reviewer verdict can admit a head",
        facts.base
    ));
    fixes.push(format!(
        "set [auto_merge] {names} in .shipyard/config.toml on {} as the other fleet \
         repositories do (reviewer_signers = [\"pulp-review\", \"infra-review2\"], \
         reviewer_stamp_sessions = [\"fleet-pass-reviewer-*\"])",
        facts.base
    ));
}

fn admission(facts: &Facts<'_>) -> Invariant {
    use crate::head_approval::{FLEET_REVIEW_REQUIRED_KEY, REQUIRED_KEY};
    let context = facts.approval_context;
    let auto = auto_merge_table(facts.base_config);
    let arm_requires = auto
        .and_then(|table| table.get(REQUIRED_KEY))
        .and_then(toml::Value::as_bool);
    // The base's `fleet_review_required = true` has the semantics of list
    // membership (see `ApprovalPolicy::from_table_for_repo`), and overrides an
    // explicit `arm_requires_head_approval = false` there too.
    let fleet_required = auto
        .and_then(|table| table.get(FLEET_REVIEW_REQUIRED_KEY))
        .and_then(toml::Value::as_bool)
        == Some(true);
    let listed = crate::head_approval::requires_review_by_default(facts.repo) || fleet_required;
    let ruleset_requires = facts
        .required
        .iter()
        .any(|check| check.context.eq_ignore_ascii_case(context));
    let needs_approval = ruleset_requires || listed || arm_requires == Some(true);
    let mut problems = Vec::new();
    let mut fixes = Vec::new();

    if facts.merge_path == "direct_merge_refused_server_enforcement_required" {
        let refused: Vec<&PrReport> = facts
            .rows
            .iter()
            .filter(|row| row.blocked.blocked_on == "direct_merge_refused")
            .collect();
        problems.push(format!(
            "no automatic admission on {base}: no merge queue and the ruleset does not require \
             an app-bound `{context}` check, so the steward refuses direct merge{of}",
            base = facts.base,
            of = if refused.is_empty() {
                String::new()
            } else {
                format!(" of {}", numbers(&refused))
            }
        ));
        fixes.push(format!(
            "require the `{context}` check, bound to the GitHub Actions app, in the {base} \
             ruleset (with a workflow that posts it), or enable a merge queue",
            base = facts.base
        ));
    }
    if needs_approval {
        signer_config(facts, auto, &mut problems, &mut fixes);
    }
    let uncounted: Vec<&PrReport> = facts
        .rows
        .iter()
        .filter(|row| {
            row.error
                .as_deref()
                .is_some_and(|error| error.contains(APPROVAL_NOT_REQUIRED))
        })
        .collect();
    let rule_off = ruleset_requires
        && !fleet_required
        && (arm_requires == Some(false) || (arm_requires.is_none() && !listed));
    if rule_off || !uncounted.is_empty() {
        problems.push(format!(
            "the {base} ruleset requires `{context}` but the approval rule does not apply to \
             {repo} (not in REVIEW_REQUIRED_REPOS, and neither [auto_merge] \
             {FLEET_REVIEW_REQUIRED_KEY} nor {REQUIRED_KEY} is true), so exact-head approvals \
             are never counted{on}",
            base = facts.base,
            repo = facts.repo,
            on = if uncounted.is_empty() {
                String::new()
            } else {
                format!(": {} wait approved but unmerged", numbers(&uncounted))
            }
        ));
        fixes.push(format!(
            "set [auto_merge] {FLEET_REVIEW_REQUIRED_KEY} = true in .shipyard/config.toml on \
             {base} (fleet App approvals then count, as for REVIEW_REQUIRED_REPOS; \
             {REQUIRED_KEY} = true is the different, strict reviewer_identities rule), or add \
             {repo} to REVIEW_REQUIRED_REPOS (src/head_approval.rs and \
             scripts/ghapp_queue_arm_guard.py)",
            base = facts.base,
            repo = facts.repo
        ));
    }
    if problems.is_empty() {
        return Invariant::holds(ADMISSION, format!("merge path {}", facts.merge_path));
    }
    if fixes.is_empty() {
        return Invariant::skipped(ADMISSION, problems.join("; "));
    }
    Invariant::fails(ADMISSION, problems.join("; "), fixes.join("; "))
}

fn age_minutes(since: Option<&str>, now: DateTime<Utc>) -> Option<i64> {
    let since = DateTime::parse_from_rfc3339(since?).ok()?;
    Some((now - since.with_timezone(&Utc)).num_minutes())
}

fn never_ran(facts: &Facts<'_>) -> Invariant {
    // context -> (oldest age, its pull request, how many wait on it)
    let mut waiting: BTreeMap<String, (i64, u64, usize)> = BTreeMap::new();
    for row in facts.rows {
        let Some(contexts) = row.blocked.blocked_on.strip_prefix("required_never_ran:") else {
            continue;
        };
        let Some(age) = age_minutes(row.blocked.since.as_deref(), facts.now) else {
            continue;
        };
        for context in contexts.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            if facts.head_checks.contains(&context.to_ascii_lowercase()) {
                continue;
            }
            let entry = waiting
                .entry(context.to_owned())
                .or_insert((age, row.number, 0));
            if age > entry.0 {
                entry.0 = age;
                entry.1 = row.number;
            }
            entry.2 += 1;
        }
    }
    let over: Vec<(&String, &(i64, u64, usize))> = waiting
        .iter()
        .filter(|(_, (age, _, _))| *age >= NEVER_RAN_LIMIT_MINUTES)
        .collect();
    if over.is_empty() {
        return Invariant::holds(
            NEVER_RAN,
            format!(
                "no required context unrun on every open head for {NEVER_RAN_LIMIT_MINUTES}+ min"
            ),
        );
    }
    let detail = over
        .iter()
        .map(|(context, (age, number, count))| {
            format!(
                "`{context}` has run on no open head; {count} pull request(s) wait on it, \
                 oldest #{number} for {age} min"
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let contexts: Vec<String> = over.iter().map(|(context, _)| (*context).clone()).collect();
    Invariant::fails(
        NEVER_RAN,
        detail,
        format!(
            "find what should post {names} on pull requests (see {PRODUCER}); if a workflow \
             exists, its `on:` triggers do not fire for pull requests on {base}: fix the \
             trigger, then re-run CI on the waiting heads",
            names = backticked(&contexts),
            base = facts.base
        ),
    )
}

/// Tokens that mean the approval gate counted an approval for that head
/// this pass.
fn counted(row: &PrReport) -> bool {
    let token = row.blocked.blocked_on.as_str();
    (token == "ready" && row.error.is_none())
        || token == "admitted"
        || [
            "waiting_code_owner:",
            "generated_path_batch:",
            "carries_unapproved_commits:",
            "carried_commits_over_cap:",
        ]
        .iter()
        .any(|prefix| token.starts_with(prefix))
}

fn verdicts(facts: &Facts<'_>) -> Invariant {
    let mut uncounted = Vec::new();
    for row in facts.rows {
        let Some(reason) = row.blocked.blocked_on.strip_prefix("approval_excluded:") else {
            continue;
        };
        if MOVED_HEAD_EXCLUSIONS.contains(&reason) {
            continue;
        }
        let Some(age) = age_minutes(row.blocked.since.as_deref(), facts.now) else {
            continue;
        };
        if age >= UNCOUNTED_LIMIT_MINUTES {
            uncounted.push((row.number, reason.to_owned(), age));
        }
    }
    let counted_now = facts.rows.iter().filter(|row| counted(row)).count();
    if uncounted.is_empty() || counted_now > 0 {
        return Invariant::holds(
            VERDICTS,
            format!(
                "{counted_now} head(s) counted an approval; {} approval(s) uncounted for \
                 {UNCOUNTED_LIMIT_MINUTES}+ min",
                uncounted.len()
            ),
        );
    }
    let mut reasons: Vec<String> = uncounted
        .iter()
        .map(|(_, reason, _)| reason.clone())
        .collect();
    reasons.sort();
    reasons.dedup();
    Invariant::fails(
        VERDICTS,
        format!(
            "no approval counted this pass while {} posted approval(s) have been excluded for \
             {UNCOUNTED_LIMIT_MINUTES}+ min: {}",
            uncounted.len(),
            uncounted
                .iter()
                .map(|(number, reason, age)| format!("#{number} {reason} {age} min"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        format!(
            "the reviewer's verdicts are posted but do not count ({}): check the posting path \
             on the reviewer host (stamp, signer list, key-holder socket; a pass-runner install \
             is the usual cause), then re-post the verdicts on the waiting heads",
            reasons.join(", "),
        ),
    )
}

// ---------------------------------------------------------------------------
// The pass's side: gather the facts, date failures, file follow-ups.
// ---------------------------------------------------------------------------

/// Run the repository invariants for one observed pass and return its rows.
#[allow(clippy::too_many_arguments)] // The pass's own facts, passed through as read.
pub(super) fn run(
    actions: &GitHubActions,
    settings: &Settings,
    observation: &RepoObservation,
    rows: &[PrReport],
    merge_path: &str,
    approval_context: &str,
    state_path: &Path,
    apply: bool,
    now: DateTime<Utc>,
) -> Vec<Invariant> {
    let repo = observation.repo.as_str();
    let base = observation.base.as_str();
    let base_config = base_config(actions, repo, base);
    let workflows = workflow_texts(actions, settings, repo, base, state_path);
    let head_checks = observation
        .prs
        .iter()
        .flat_map(|pr| pr.fact.checks.iter())
        .map(|check| check.name.to_ascii_lowercase())
        .collect();
    let facts = Facts {
        repo,
        base,
        merge_path,
        approval_context,
        required: &observation.required_checks,
        head_checks,
        rows,
        base_config: &base_config,
        workflows: &workflows,
        now,
    };
    let mut invariants = evaluate(&facts);
    if apply {
        let followup = settings
            .followup_repo
            .as_deref()
            .map(|followup_repo| Followup {
                actions,
                repo: followup_repo,
            });
        record(state_path, repo, &mut invariants, followup.as_ref(), now);
    }
    invariants
}

/// The base's config through the same argv every base-config read uses, so
/// the pass cache answers it.
fn base_config(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Option<toml::Table>, String> {
    let run_gh = |args: &[String]| actions.run_gh(args).map_err(|error| error.to_string());
    crate::rollup::base_config_text(&run_gh, repo, base)?
        .map(|text| {
            text.parse::<toml::Table>()
                .map_err(|error| format!(".shipyard/config.toml on {base} does not parse: {error}"))
        })
        .transpose()
}

/// Whether `dir` is a checkout of `repo` with `base` checked out.
fn checkout_of(dir: &Path, repo: &str, base: &str) -> bool {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let Some(url) = git(&["remote", "get-url", "origin"]) else {
        return false;
    };
    let url = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase();
    let slug = repo.to_ascii_lowercase();
    let names_repo = url
        .strip_suffix(&slug)
        .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('/') || prefix.ends_with(':'));
    names_repo && git(&["rev-parse", "--abbrev-ref", "HEAD"]).as_deref() == Some(base)
}

fn is_workflow_file(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml"))
}

/// Every workflow file's text on the base: from the local checkout when the
/// pass runs in one that holds them, else from the API with blobs cached by
/// SHA beside the state file.
fn workflow_texts(
    actions: &GitHubActions,
    settings: &Settings,
    repo: &str,
    base: &str,
    state_path: &Path,
) -> Result<Vec<String>, String> {
    if let Some(dir) = settings.checkout.as_deref() {
        let workflows = dir.join(".github/workflows");
        if workflows.is_dir() && checkout_of(dir, repo, base) {
            return local_workflows(&workflows);
        }
    }
    let listing = match actions.run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/contents/.github/workflows?ref={base}"),
        "--jq".to_owned(),
        ".[] | [.name, .sha] | @tsv".to_owned(),
    ]) {
        Ok(listing) => listing,
        Err(error) => {
            let detail = error.to_string();
            if detail.contains("404") || detail.contains("Not Found") {
                return Ok(Vec::new());
            }
            return Err(format!("workflow listing on {base}: {detail}"));
        }
    };
    let cache = state_path.with_extension("workflow-blobs");
    let mut texts = Vec::new();
    for line in listing.lines() {
        let Some((name, sha)) = line.split_once('\t') else {
            continue;
        };
        if !is_workflow_file(name) || !crate::merge_steward::is_full_sha(sha) {
            continue;
        }
        let cached = cache.join(sha);
        if let Ok(text) = std::fs::read_to_string(&cached) {
            texts.push(text);
            continue;
        }
        let text = actions
            .run_gh(&[
                "api".to_owned(),
                "-H".to_owned(),
                "Accept: application/vnd.github.raw+json".to_owned(),
                format!("repos/{repo}/git/blobs/{sha}"),
            ])
            .map_err(|error| format!("workflow {name}: {error}"))?;
        // A blob is content-addressed: a failed cache write only costs the
        // next pass one read.
        let _ = std::fs::create_dir_all(&cache).and_then(|()| std::fs::write(&cached, &text));
        texts.push(text);
    }
    Ok(texts)
}

fn local_workflows(dir: &Path) -> Result<Vec<String>, String> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|error| format!("{}: {error}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_workflow_file)
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| {
            std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
        })
        .collect()
}

/// Where the invariants' cross-pass state lives, beside the steward ledger.
pub(super) fn state_path(ledger_path: &Path) -> PathBuf {
    ledger_path.with_extension("invariants.json")
}

#[derive(Default, Deserialize, Serialize)]
struct State {
    #[serde(default)]
    failing: BTreeMap<String, Failing>,
}

#[derive(Deserialize, Serialize)]
struct Failing {
    since: DateTime<Utc>,
    fingerprint: String,
    /// The fingerprint last filed as a follow-up, so a repeat is not refiled.
    #[serde(default)]
    filed: Option<String>,
}

/// Where a failing invariant is filed, when the host opted in.
pub(crate) struct Followup<'a> {
    pub actions: &'a GitHubActions,
    pub repo: &'a str,
}

fn state_key(repo: &str, id: &str) -> String {
    format!("{}|{id}", repo.to_ascii_lowercase())
}

/// Date each failing invariant from its first failing pass, forget the ones
/// that hold again, and file a follow-up once per new or changed failure.
pub(crate) fn record(
    state_path: &Path,
    repo: &str,
    invariants: &mut [Invariant],
    followup: Option<&Followup<'_>>,
    now: DateTime<Utc>,
) {
    let mut state: State = std::fs::read(state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    for invariant in invariants.iter_mut() {
        let key = state_key(repo, &invariant.id);
        if !invariant.incident() {
            if !invariant.skipped {
                state.failing.remove(&key);
            }
            continue;
        }
        let fingerprint = invariant.fingerprint();
        let entry = state.failing.entry(key).or_insert_with(|| Failing {
            since: now,
            fingerprint: fingerprint.clone(),
            filed: None,
        });
        entry.fingerprint.clone_from(&fingerprint);
        invariant.since = Some(
            entry
                .since
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        if let Some(followup) = followup
            && entry.filed.as_deref() != Some(fingerprint.as_str())
        {
            match file_followup(followup, repo, invariant) {
                Ok(outcome) => {
                    entry.filed = Some(fingerprint);
                    invariant.followup = Some(outcome);
                }
                Err(error) => invariant.followup = Some(format!("not filed: {error}")),
            }
        }
    }
    if let Some(parent) = state_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = crate::durable_file::replace_json(state_path, &state, true) {
        for invariant in invariants
            .iter_mut()
            .filter(|invariant| invariant.incident())
        {
            let _ = write!(invariant.detail, " (state not saved: {error})");
        }
    }
}

/// The one issue title per invariant per repository.
pub(crate) fn followup_title(repo: &str, id: &str) -> String {
    format!("steward invariant {id} failing on {repo}")
}

fn file_followup(
    followup: &Followup<'_>,
    repo: &str,
    invariant: &Invariant,
) -> Result<String, String> {
    let scoped = followup.actions.clone().with_repo_override(followup.repo);
    let title = followup_title(repo, &invariant.id);
    let body = format!(
        "The Shipyard steward's `{id}` invariant fails on `{repo}`.\n\n**Found:** {detail}\n\n\
         **Fix:** {fix}\n\nFirst failing pass: {since}. The steward files this issue again \
         only when the finding changes.",
        id = invariant.id,
        detail = invariant.detail,
        fix = invariant.fix.as_deref().unwrap_or_default(),
        since = invariant.since.as_deref().unwrap_or("unknown"),
    );
    let listed = scoped
        .run_gh(&[
            "issue".to_owned(),
            "list".to_owned(),
            "--repo".to_owned(),
            followup.repo.to_owned(),
            "--state".to_owned(),
            "open".to_owned(),
            "--label".to_owned(),
            FOLLOWUP_LABEL.to_owned(),
            "--search".to_owned(),
            format!("\"{title}\" in:title"),
            "--json".to_owned(),
            "number,title".to_owned(),
            "--limit".to_owned(),
            "20".to_owned(),
        ])
        .map_err(|error| error.to_string())?;
    let existing = serde_json::from_str::<Value>(&listed)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .find(|issue| issue.get("title").and_then(Value::as_str) == Some(title.as_str()))
        .and_then(|issue| issue.get("number").and_then(Value::as_u64));
    if let Some(number) = existing {
        scoped
            .run_gh(&[
                "issue".to_owned(),
                "comment".to_owned(),
                number.to_string(),
                "--repo".to_owned(),
                followup.repo.to_owned(),
                "--body".to_owned(),
                body,
            ])
            .map_err(|error| error.to_string())?;
        return Ok(format!("commented on {}#{number}", followup.repo));
    }
    scoped
        .run_gh(&[
            "issue".to_owned(),
            "create".to_owned(),
            "--repo".to_owned(),
            followup.repo.to_owned(),
            "--title".to_owned(),
            title,
            "--label".to_owned(),
            FOLLOWUP_LABEL.to_owned(),
            "--body".to_owned(),
            body,
        ])
        .map(|url| format!("opened {}", url.trim()))
        .map_err(|error| error.to_string())
}
