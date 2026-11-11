//! Admission while the base is poisoned.
//!
//! When the merge groups show a required context failing on the base itself
//! ([`crate::base_health::merge_group`]), every pull request the steward
//! enqueues forms a group that cannot pass: it costs a full gate and ejects
//! everything in it. Two things then matter, in this order: stop forming
//! doomed groups, and land the fix as fast as possible.
//!
//! Each pass on a merge-queue base whose `[base_health] merge_group_hold` is
//! not `off` reads the merge groups once ([`observe`]). While a context is
//! poisoned:
//!
//! * **Hold.** An approved, green pull request that is not a named fix is not
//!   enqueued (`held_base_poisoned:<context>`); under `dry-run` it is
//!   enqueued as before and reported `would_hold_base_poisoned:<context>`.
//! * **Fast-track the fix.** A pull request is a named fix when it carries
//!   [`crate::queue_drain::UNBLOCK_LABEL`], when its title or description
//!   names the poisoned context next to a fix word, or when the base-poison
//!   detector names it as `candidate_fix_pr`. For each one [`fast_track`]
//!   puts its review first (the machine's reviewer priority file and kick
//!   command, once per head) and moves its waiting CI jobs to the front of
//!   the runner queue, excusing only the approval context's red, which every
//!   head carries before review. Once it is approved and green the steward
//!   enqueues it as usual, with `jump: true` when `auto_jump = "on"`; a
//!   refused jump falls back to a plain enqueue in the same pass.
//!
//! The approval is never excused at admission: the fix is enqueued only
//! through the same exact-head approval gate as everything else.
//!
//! The hold lifts by itself once a merge group passes the context, because
//! the streak is counted from the newest verdict. An unreadable merge-group
//! history never holds anything.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{GitHubActions, MutationControl, RepoObservation, StewardPolicy};
use crate::base_health::merge_group::{self, MergeGroupHealth};
use crate::base_health::{
    self, AutoJump, BasePoisonPolicy, FIX_REVIEW_KICK_KEY, FIX_REVIEW_PRIORITY_FILE_KEY, PoisonHold,
};

/// Open pull requests listed to find named fixes.
const FIX_LIST_LIMIT: usize = 300;
/// How long a reviewer kick command may run.
const KICK_TIMEOUT: Duration = Duration::from_secs(30);
/// Fast-track receipts, one line per action, under the state directory.
const FAST_TRACK_FILE: &str = "base-health/fast-track.jsonl";

/// What one pass learned about the base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct RepoPoison {
    /// The base config's `[base_health]` settings.
    pub(super) policy: BasePoisonPolicy,
    /// The merge groups' verdict.
    pub(super) health: MergeGroupHealth,
    /// Named fixes for a poisoned context, with why each counts.
    pub(super) fixes: BTreeMap<u64, Vec<String>>,
    /// Why named fixes could not be listed, when they could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) fix_read_error: Option<String>,
}

/// What admission does with one approved, green pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Admission {
    /// Nothing poisoned, or nothing read: admit as usual.
    Proceed,
    /// Poisoned, hold in dry-run: admit, and report what would have held.
    WouldHold {
        /// The poisoned contexts.
        contexts: String,
    },
    /// Poisoned, hold on: not admitted.
    Hold {
        /// The poisoned contexts.
        contexts: String,
    },
    /// A named fix for the poison: admitted, at the front when `jump`.
    FastTrack {
        /// Enqueue with `jump: true`.
        jump: bool,
    },
}

impl Admission {
    /// The report token for this admission, when it has one.
    pub(super) fn token(&self) -> Option<String> {
        match self {
            Self::Proceed => None,
            Self::WouldHold { contexts } => Some(format!("would_hold_base_poisoned:{contexts}")),
            Self::Hold { contexts } => Some(format!("held_base_poisoned:{contexts}")),
            Self::FastTrack { jump: true } => Some("fast_track_fix_jump".to_owned()),
            Self::FastTrack { jump: false } => Some("fast_track_fix".to_owned()),
        }
    }
}

/// Decide one pull request's admission from the pass's reading.
pub(super) fn admission(poison: Option<&RepoPoison>, pr: u64) -> Admission {
    let Some(poison) = poison else {
        return Admission::Proceed;
    };
    let contexts = poison.health.poisoned_contexts();
    if contexts.is_empty() {
        return Admission::Proceed;
    }
    if poison.fixes.contains_key(&pr) {
        return Admission::FastTrack {
            // A jump reorders the queue, so it is a mutation: dry-run never jumps.
            jump: poison.policy.hold == PoisonHold::On && poison.policy.auto_jump == AutoJump::On,
        };
    }
    let contexts = contexts.join(",");
    match poison.policy.hold {
        PoisonHold::On => Admission::Hold { contexts },
        PoisonHold::DryRun => Admission::WouldHold { contexts },
        PoisonHold::Off => Admission::Proceed,
    }
}

/// Read the merge groups for this pass, and the named fixes when poisoned.
/// `None` when the base does not ask for it.
pub(super) fn observe(
    actions: &GitHubActions,
    observation: &RepoObservation,
    policy: &StewardPolicy,
    now: DateTime<Utc>,
) -> Option<RepoPoison> {
    let poison_policy = policy.ejection_guards.base_poison.clone();
    if !observation.merge_queue || poison_policy.hold == PoisonHold::Off {
        return None;
    }
    let run_gh = |argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string());
    let health = merge_group::read(
        &run_gh,
        &observation.repo,
        &observation.base,
        &observation.required_checks,
        now,
    );
    let poisoned = health.poisoned_contexts();
    let (fixes, fix_read_error) = if poisoned.is_empty() {
        (BTreeMap::new(), None)
    } else {
        named_fixes(
            &run_gh,
            &observation.repo,
            &observation.base,
            &poisoned,
            now,
        )
    };
    let fixes = choose_fix(fixes, &health);
    Some(RepoPoison {
        policy: poison_policy,
        health,
        fixes,
        fix_read_error,
    })
}

/// The one fix this pass fast-tracks, out of every named candidate.
///
/// A pull request that rode one of the streak's failing groups demonstrably
/// did not fix the context, so it is dropped. Of the rest, the strongest
/// evidence wins: the `shipyard:unblock` label (only someone with write
/// access can apply it), then the detector's naming, then title or
/// description text; ties go to the lowest number. Exactly one fix keeps the
/// per-pass reads and cancellations bounded and stops a pull request whose
/// text merely mentions the context from being fast-tracked beside it.
pub(super) fn choose_fix(
    fixes: BTreeMap<u64, Vec<String>>,
    health: &MergeGroupHealth,
) -> BTreeMap<u64, Vec<String>> {
    let rode_a_failing_group = |pr: u64| {
        health
            .poisoned()
            .iter()
            .any(|poison| poison.groups.iter().any(|group| group.prs.contains(&pr)))
    };
    let rank = |evidence: &[String]| {
        if evidence.iter().any(|line| line.starts_with("label ")) {
            0
        } else if evidence
            .iter()
            .any(|line| line.starts_with("base-poison detector"))
        {
            1
        } else {
            2
        }
    };
    fixes
        .into_iter()
        .filter(|(pr, _)| !rode_a_failing_group(*pr))
        .min_by_key(|(pr, evidence)| (rank(evidence), *pr))
        .into_iter()
        .collect()
}

/// Open, non-draft pull requests named as the fix for one of `contexts`,
/// with the evidence for each.
pub(super) fn named_fixes(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    repo: &str,
    base: &str,
    contexts: &[String],
    now: DateTime<Utc>,
) -> (BTreeMap<u64, Vec<String>>, Option<String>) {
    let mut fixes = BTreeMap::new();
    let mut errors = Vec::new();
    match run_gh(&[
        "pr".to_owned(),
        "list".to_owned(),
        "--repo".to_owned(),
        repo.to_owned(),
        "--base".to_owned(),
        base.to_owned(),
        "--state".to_owned(),
        "open".to_owned(),
        "--limit".to_owned(),
        FIX_LIST_LIMIT.to_string(),
        "--json".to_owned(),
        "number,title,body,labels,isDraft".to_owned(),
    ])
    .and_then(|raw| serde_json::from_str::<Value>(&raw).map_err(|error| error.to_string()))
    {
        Ok(rows) => {
            for row in rows.as_array().into_iter().flatten() {
                let Some(number) = row.get("number").and_then(Value::as_u64) else {
                    continue;
                };
                if row.get("isDraft").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let text = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or("");
                let labels = row
                    .get("labels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|label| label.get("name").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let evidence = crate::queue_drain::fix_evidence(
                    text("title"),
                    text("body"),
                    &labels,
                    contexts,
                );
                if !evidence.is_empty() {
                    fixes.insert(number, evidence);
                }
            }
        }
        Err(error) => errors.push(format!("open pull requests: {error}")),
    }
    // The main-branch detector may also name the fix; a fresh poisoned
    // signal with a candidate counts.
    let finding = base_health::read_latest(run_gh, repo, base_health::DEFAULT_WORKFLOW);
    if let Some(advice) = base_health::jump_advice(repo, &finding, now) {
        fixes
            .entry(advice.pr)
            .or_insert_with(Vec::new)
            .push(format!(
                "base-poison detector run {} names it",
                advice.signal_run_id
            ));
    }
    (fixes, (!errors.is_empty()).then(|| errors.join("; ")))
}

/// The report for this pass.
pub(super) fn report(poison: &RepoPoison, fast_track: &[String]) -> Value {
    json!({
        "summary": poison.health.summary(),
        "hold": poison.policy.hold,
        "auto_jump": poison.policy.auto_jump,
        "health": poison.health,
        "fixes": poison.fixes,
        "fix_read_error": poison.fix_read_error,
        "config_error": poison.policy.read_error,
        "fast_track": fast_track,
    })
}

/// One fast-track action, recorded so it happens once per head.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct FastTrackReceipt {
    /// `OWNER/REPO`.
    pub(super) repo: String,
    /// The fix pull request.
    pub(super) pr: u64,
    /// Its head when acted on.
    pub(super) head: String,
    /// `review_priority` or `review_kick`.
    pub(super) action: String,
    /// RFC 3339.
    pub(super) at: String,
    /// What happened.
    pub(super) outcome: String,
}

fn receipts_path(state_dir: &Path) -> PathBuf {
    state_dir.join(FAST_TRACK_FILE)
}

fn load_receipts(path: &Path) -> Vec<FastTrackReceipt> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn append_receipt(path: &Path, receipt: &FastTrackReceipt) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "{}",
        serde_json::to_string(receipt).unwrap_or_default()
    )
    .map_err(|error| error.to_string())
}

/// The reviewer hooks this machine configures. Read from the machine-global
/// layer only: a repository must never name a program the steward runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ReviewHooks {
    /// argv for the kick; the reason is appended.
    pub(super) kick: Vec<String>,
    /// Reviewer priority file.
    pub(super) priority_file: Option<PathBuf>,
}

impl ReviewHooks {
    pub(super) fn from_machine_global(global_dir: &Path) -> Self {
        let Ok(config) =
            crate::config::LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
        else {
            return Self::default();
        };
        let kick = config
            .get(FIX_REVIEW_KICK_KEY)
            .and_then(toml::Value::as_array)
            .map(|argv| {
                argv.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let priority_file = config
            .get_str(FIX_REVIEW_PRIORITY_FILE_KEY)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(expand_home);
        Self {
            kick,
            priority_file,
        }
    }
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// Put `entry` first in a priority file, keeping every other line once.
/// Returns whether the file changed.
pub(super) fn prioritize_line(path: &Path, entry: &str) -> Result<bool, String> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let first = existing
        .lines()
        .find(|line| !line.trim().is_empty() && !line.starts_with('#'));
    if first.is_some_and(|line| line.split_whitespace().next() == Some(entry)) {
        return Ok(false);
    }
    let mut lines = vec![entry.to_owned()];
    lines.extend(
        existing
            .lines()
            .filter(|line| line.split_whitespace().next() != Some(entry))
            .map(str::to_owned),
    );
    let tmp = path.with_extension("base-poison.tmp");
    fs::write(&tmp, format!("{}\n", lines.join("\n"))).map_err(|error| error.to_string())?;
    fs::rename(&tmp, path).map_err(|error| error.to_string())?;
    Ok(true)
}

fn run_kick(argv: &[String], reason: &str) -> String {
    let Some((program, program_args)) = argv.split_first() else {
        return "not_configured".to_owned();
    };
    // The kick may leave a follow-up process behind on purpose, so only the
    // direct child is waited on and, past the timeout, killed.
    let child = Command::new(program)
        .args(program_args)
        .arg(reason)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return format!("failed: {error}"),
    };
    let deadline = Instant::now() + KICK_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return "kicked".to_owned(),
            Ok(Some(status)) => return format!("failed: exit {status}"),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return "failed: timed out".to_owned();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => return format!("failed: {error}"),
        }
    }
}

/// Put one fix's review first: the priority file and the kick, each once per
/// head. Returns report lines.
pub(super) fn prioritize_review(
    hooks: &ReviewHooks,
    state_dir: &Path,
    repo: &str,
    pr: u64,
    head: &str,
    contexts: &str,
) -> Vec<String> {
    let path = receipts_path(state_dir);
    let receipts = load_receipts(&path);
    let done = |action: &str| {
        receipts.iter().any(|receipt| {
            receipt.repo.eq_ignore_ascii_case(repo)
                && receipt.pr == pr
                && receipt.head == head
                && receipt.action == action
                && !receipt.outcome.starts_with("failed")
        })
    };
    let mut lines = Vec::new();
    let mut record = |action: &str, outcome: String| {
        lines.push(format!("#{pr} {action}: {outcome}"));
        let receipt = FastTrackReceipt {
            repo: repo.to_owned(),
            pr,
            head: head.to_owned(),
            action: action.to_owned(),
            at: Utc::now().to_rfc3339(),
            outcome,
        };
        if let Err(error) = append_receipt(&path, &receipt) {
            lines.push(format!("#{pr} {action}: receipt not recorded: {error}"));
        }
    };
    let entry = format!("{repo}#{pr}");
    if let Some(file) = hooks.priority_file.as_ref()
        && !done("review_priority")
    {
        let outcome = match prioritize_line(file, &entry) {
            Ok(true) => "listed first".to_owned(),
            Ok(false) => "already first".to_owned(),
            Err(error) => format!("failed: {error}"),
        };
        record("review_priority", outcome);
    }
    if !hooks.kick.is_empty() && !done("review_kick") {
        let short = head.get(..12).unwrap_or(head);
        let reason = format!("base poisoned ({contexts}); fix {entry} at {short} needs review");
        record("review_kick", run_kick(&hooks.kick, &reason));
    }
    lines
}

/// Whether GitHub definitively refused an enqueue with `jump: true`, so a
/// plain enqueue of the same head is worth one more call. A queue-arm guard
/// refusal never reached GitHub and is not retried; a transport failure is
/// not an answer and is left to the uncertain-admission reconciliation.
pub(super) fn jump_was_refused<E: std::fmt::Display>(result: &Result<String, E>) -> bool {
    let text = match result {
        Ok(raw) => raw.clone(),
        Err(error) => error.to_string(),
    };
    !crate::queue_arm_gate::is_refusal(&text)
        && super::mutation_outcome::definitive_refusal(&text).is_some()
}

/// What [`fast_track`] would do, for a pass that does not mutate (a dry
/// run, or `merge_group_hold = "dry-run"`).
pub(super) fn would_fast_track(poison: &RepoPoison) -> Vec<String> {
    if poison.health.poisoned_contexts().is_empty() {
        return Vec::new();
    }
    poison
        .fixes
        .iter()
        .map(|(pr, evidence)| {
            format!(
                "#{pr} would fast-track (review first, CI to the front): {}",
                evidence.join("; ")
            )
        })
        .collect()
}

/// Fast-track every named fix this pass: review first, then its CI.
pub(super) fn fast_track(
    actions: &GitHubActions,
    control: &MutationControl,
    observation: &RepoObservation,
    policy: &StewardPolicy,
    poison: &RepoPoison,
) -> Vec<String> {
    let contexts = poison.health.poisoned_contexts();
    if contexts.is_empty() || poison.fixes.is_empty() {
        return Vec::new();
    }
    let contexts = contexts.join(",");
    let hooks = ReviewHooks::from_machine_global(&control.global_dir);
    let runtime = super::queue_drain::QueueDrainRuntime {
        cwd: &control.cwd,
        mode: control.mode,
        state_dir: &control.state_dir,
        global_dir: &control.global_dir,
        actions,
    };
    let mut lines = Vec::new();
    for number in poison.fixes.keys() {
        let Some(pr) = observation.prs.iter().find(|pr| pr.fact.number == *number) else {
            lines.push(format!("#{number}: not open on {}", observation.base));
            continue;
        };
        if pr.fact.queue_position.is_some() {
            lines.push(format!("#{number}: already in the merge queue"));
            continue;
        }
        let approved =
            crate::merge_steward::has_successful_status(&pr.fact, &policy.approval_context);
        if !approved {
            lines.extend(prioritize_review(
                &hooks,
                &control.state_dir,
                &observation.repo,
                *number,
                &pr.fact.head_sha,
                &contexts,
            ));
        }
        // Only a fix with a required check still to run has CI to move; a
        // settled head would cost the full queue-drain read for nothing.
        let settled = policy
            .required_checks
            .iter()
            .filter(|rule| rule.context != policy.approval_context)
            .all(|rule| {
                crate::merge_steward::selected_required_check(&pr.fact.checks, rule)
                    .is_some_and(|check| check.status.eq_ignore_ascii_case("COMPLETED"))
            });
        if settled {
            lines.push(format!("#{number} ci: required checks already ran"));
            continue;
        }
        lines.push(
            match super::queue_drain::prioritize_named_fix(
                &runtime,
                &observation.repo,
                &observation.base,
                *number,
                std::slice::from_ref(&policy.approval_context),
            ) {
                Ok(line) => format!("#{number} ci: {line}"),
                Err(error) => format!("#{number} ci: failed: {error}"),
            },
        );
    }
    lines
}

#[cfg(test)]
mod tests;
