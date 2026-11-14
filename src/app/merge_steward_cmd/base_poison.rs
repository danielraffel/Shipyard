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
//!   names the poisoned context next to a fix word, when the base-poison
//!   detector names it as `candidate_fix_pr`. A head that passes the context
//!   after the newest failing group while at least two other open heads fail
//!   it ([`super::poison_pause::passing_head_fixes`]) is only exempt from the
//!   pause, never fast-tracked. When nothing names a fix, the steward opens one tracking issue per
//!   poisoned context ([`ensure_tracking_issue`]) and puts it first in the
//!   machine's priority file, so the repair lane starts on the root cause. For each one [`fast_track`]
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
//!
//! **Fix-first** ([`crate::base_health::fix_first`]). The streak alone never
//! holds or jumps anything. Each poisoned context also has the base tip's own
//! newest verdict read (one `GET` per poisoned context, only while the streak
//! fires); a context is **confirmed** when that verdict is a failure too. Only
//! a confirmed context holds admission (`held_base_poisoned:<context>`),
//! pauses the queue, or jumps the fix, and the hold lifts in the same pass
//! the tip goes green. While a context is confirmed, an open pull request
//! whose head is approved and passed the context after the tip went red is
//! a fix (`green head`), found from the census the pass already read. It
//! ranks below a label and the detector's naming and above title text, and it
//! is never held: it is enqueued with `jump: true` when `auto_jump = "on"`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{GitHubActions, MutationControl, RepoObservation, StewardPolicy};
use crate::base_health::fix_first::{self, TipReading};
use crate::base_health::merge_group::{self, MergeGroupHealth, PoisonedContext};
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
    /// Every candidate fix, named or measured, before [`choose_fix`]
    /// narrows them to one. The pause never removes any of them, so a fix
    /// already in the queue keeps its group even when the pick changes.
    #[serde(default)]
    pub(super) candidates: std::collections::BTreeSet<u64>,
    /// Why named fixes could not be listed, when they could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) fix_read_error: Option<String>,
    /// The base tip's own verdict for each poisoned context.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) tip: BTreeMap<String, TipReading>,
    /// Open heads that passed every confirmed context after the base went
    /// red (not drafts, none that rode a failing group): the only pull
    /// requests a confirmed episode admits, and the only fix it jumps.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub(super) covering: BTreeSet<u64>,
    /// Set when several pull requests qualified as the green-head fix and
    /// nothing singled one out. They are admitted plainly ([`Self::candidates`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) fix_ambiguity: Option<String>,
}

impl RepoPoison {
    /// Poisoned contexts the base tip also fails: the only ones that hold,
    /// pause or jump. An unread tip confirms nothing.
    pub(super) fn confirmed_contexts(&self) -> Vec<String> {
        self.health
            .poisoned_contexts()
            .into_iter()
            .filter(|context| self.tip.get(context).is_some_and(|tip| tip.red))
            .collect()
    }

    /// What this pass could not read, when anything: a tip that failed to
    /// read, or the open pull requests behind the named fixes. A decision
    /// built on it is reported as partial, never as clean.
    pub(super) fn partial(&self) -> Option<String> {
        let mut gaps = self
            .tip
            .iter()
            .filter_map(|(context, tip)| {
                tip.error
                    .as_ref()
                    .map(|error| format!("tip `{context}`: {error}"))
            })
            .collect::<Vec<_>>();
        gaps.extend(self.fix_read_error.clone());
        (!gaps.is_empty()).then(|| gaps.join("; "))
    }

    /// Whether every one of `contexts` was read green on the tip this pass:
    /// the episode for them is over even if the merge-group streak is not.
    pub(super) fn tip_green_for(&self, contexts: &str) -> bool {
        contexts.split(',').all(|context| {
            self.tip
                .get(context)
                .is_some_and(|tip| !tip.red && tip.error.is_none())
        })
    }
}

/// A tip reading that is red for each poisoned context of `health`, for
/// tests that exercise a confirmed episode.
#[cfg(test)]
pub(super) fn red_tip(health: &MergeGroupHealth) -> BTreeMap<String, TipReading> {
    health
        .poisoned_contexts()
        .into_iter()
        .map(|context| {
            (
                context,
                TipReading {
                    sha: None,
                    red: true,
                    red_since: None,
                    error: None,
                },
            )
        })
        .collect()
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
    let confirmed = poison.confirmed_contexts();
    let named = poison.fixes.contains_key(&pr);
    // A streak alone changes nothing at admission: one flaky group pair must
    // not stop the queue, and nothing is fast-tracked on it. Every pull
    // request, named fix or not, is admitted as usual.
    if confirmed.is_empty() {
        return Admission::Proceed;
    }
    // Confirmed: only a head that passed every confirmed context after the
    // base went red is admitted. Exemptions are never per context: a head
    // that passes one red context but not another forms a doomed group.
    if poison.covering.contains(&pr) {
        return if named {
            Admission::FastTrack {
                // A jump reorders the queue, so it is a mutation: dry-run
                // never jumps.
                jump: poison.policy.hold == PoisonHold::On
                    && poison.policy.auto_jump == AutoJump::On,
            }
        } else {
            // An ambiguous or measured fix: admitted plainly, never jumped.
            Admission::Proceed
        };
    }
    let contexts = confirmed.join(",");
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
    // Every required context is judged. Merge groups are read from their
    // check runs only, so a review status (which only waits on reviewers)
    // never poisons a group, while a crashing review check run still does.
    let judged = observation.required_checks.clone();
    let health = merge_group::read(&run_gh, &observation.repo, &observation.base, &judged, now);
    let poisoned = health.poisoned_contexts();
    let (mut fixes, fix_read_error) = if poisoned.is_empty() {
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
    let tip = poisoned
        .iter()
        .map(|context| {
            let rule = observation
                .required_checks
                .iter()
                .find(|rule| &rule.context == context);
            (
                context.clone(),
                read_tip(
                    &run_gh,
                    &observation.repo,
                    &observation.base,
                    context,
                    rule.and_then(|rule| rule.app_id),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let (green, covering) = green_head_fix(
        &observation.prs,
        &observation.required_checks,
        &policy.approval_context,
        &health,
        &tip,
    );
    if let GreenHead::One(pr, evidence) = &green {
        fixes.entry(*pr).or_default().push(evidence.clone());
    }
    let (fixes, mut candidates) = assemble_fixes(fixes, &observation.prs, &health);
    let fix_ambiguity = match green {
        GreenHead::Ambiguous(prs) => {
            candidates.extend(prs.iter().copied());
            Some(ambiguity_line(&prs))
        }
        _ => None,
    };
    Some(RepoPoison {
        candidates,
        policy: poison_policy,
        health,
        fixes,
        fix_read_error,
        tip,
        covering,
        fix_ambiguity,
    })
}

/// The report line for an ambiguous green-head reading.
pub(super) fn ambiguity_line(prs: &[u64]) -> String {
    format!(
        "fix ambiguous: {} all pass the red context on an approved head; none is jumped or held \
         (add `main-red-fix` or `fixes-main` to exactly one)",
        prs.iter()
            .map(|pr| format!("#{pr}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The base tip's verdict for one context: one read.
pub(super) fn read_tip(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    repo: &str,
    base: &str,
    context: &str,
    app_id: Option<u64>,
) -> TipReading {
    match run_gh(&[
        "api".to_owned(),
        fix_first::tip_check_path(repo, base, context),
    ])
    .and_then(|raw| serde_json::from_str::<Value>(&raw).map_err(|error| error.to_string()))
    {
        Ok(value) => fix_first::parse_tip(&value, context, app_id),
        Err(error) => TipReading {
            error: Some(error),
            ..TipReading::default()
        },
    }
}

/// What the green-head reading found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum GreenHead {
    /// No pull request qualifies.
    None,
    /// The fix, with its evidence line.
    One(u64, String),
    /// Several qualify and nothing singles one out: none is jumped or held.
    Ambiguous(Vec<u64>),
}

/// The one green-head fix across the confirmed contexts, with its evidence:
/// an open pull request whose current head is approved and passed every
/// confirmed context after the base tip went red. Also returns every open
/// head that passed every confirmed context, approved or not
/// ([`RepoPoison::covering`]). Read from the census; no call. The
/// facts belong to the head the census read; the enqueue that acts on them
/// pins that head (`expectedHeadOid`) and re-reads its exact-head approval,
/// so a head that moved in between is never jumped.
pub(super) fn green_head_fix(
    prs: &[super::ObservedPr],
    required: &[crate::merge_steward::RequiredCheck],
    approval_context: &str,
    health: &MergeGroupHealth,
    tip: &BTreeMap<String, TipReading>,
) -> (GreenHead, BTreeSet<u64>) {
    let time = |text: Option<&str>| {
        text.and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            .map(|at| at.with_timezone(&Utc))
    };
    let succeeded = |check: &crate::merge_steward::StewardCheck| {
        check.status.eq_ignore_ascii_case("COMPLETED")
            && check
                .conclusion
                .as_deref()
                .is_some_and(|conclusion| conclusion.eq_ignore_ascii_case("SUCCESS"))
    };
    // Per confirmed context: its red time and every open head's facts for
    // it. The decision itself is [`fix_first::decide`], shared with replay.
    let mut per_context: Vec<(String, Option<DateTime<Utc>>, Vec<fix_first::FixCandidate>)> =
        Vec::new();
    for poisoned in health.poisoned() {
        let Some(reading) = tip.get(&poisoned.context).filter(|tip| tip.red) else {
            continue;
        };
        let rule = required
            .iter()
            .find(|rule| rule.context == poisoned.context)
            .cloned()
            .unwrap_or_else(|| crate::merge_steward::RequiredCheck {
                context: poisoned.context.clone(),
                app_id: None,
            });
        let candidates = prs
            .iter()
            .map(|pr| fix_first::FixCandidate {
                number: pr.fact.number,
                draft: pr.fact.draft,
                labels: pr.fact.labels.clone(),
                approved_at: pr
                    .fact
                    .checks
                    .iter()
                    .filter(|check| {
                        check.name.eq_ignore_ascii_case(approval_context)
                            && check.source
                                == crate::merge_steward::StewardCheckSource::StatusContext
                            && succeeded(check)
                    })
                    .filter_map(|check| time(check.observed_at.as_deref()))
                    .min(),
                context_green_at: crate::merge_steward::selected_required_check(
                    &pr.fact.checks,
                    &rule,
                )
                .filter(|check| succeeded(check))
                .and_then(|check| time(check.observed_at.as_deref())),
                rode_failing_group: health.poisoned().iter().any(|poison| {
                    poison
                        .groups
                        .iter()
                        .any(|group| group.prs.contains(&pr.fact.number))
                }),
            })
            .collect::<Vec<_>>();
        per_context.push((poisoned.context.clone(), reading.red_since, candidates));
    }
    let facts = per_context
        .iter()
        .map(|(context, red_since, candidates)| fix_first::ContextFacts {
            context,
            red_since: *red_since,
            candidates,
        })
        .collect::<Vec<_>>();
    let mut decision = fix_first::decide(&facts);
    let green = match decision.pick {
        fix_first::Pick::None => GreenHead::None,
        fix_first::Pick::Ambiguous { prs } => GreenHead::Ambiguous(prs),
        fix_first::Pick::One(chosen) => GreenHead::One(
            chosen.pr,
            decision
                .evidence
                .remove(&chosen.pr)
                .unwrap_or_default()
                .join("; "),
        ),
    };
    (green, decision.covering)
}

/// The one named fix this pass fast-tracks ([`choose_fix`]), and every
/// candidate the pause must leave alone: the named ones plus the measured
/// ones ([`super::poison_pause::passing_head_fixes`]). Measured evidence only
/// exempts a pull request from the pause; it never fast-tracks, cancels or
/// jumps, because a path-filtered check can pass on an unrelated head.
pub(super) fn assemble_fixes(
    named: BTreeMap<u64, Vec<String>>,
    prs: &[super::ObservedPr],
    health: &MergeGroupHealth,
) -> (BTreeMap<u64, Vec<String>>, std::collections::BTreeSet<u64>) {
    let mut candidates = named
        .keys()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    candidates.extend(super::poison_pause::passing_head_fixes(prs, health).into_keys());
    // A pull request that rode a failing group demonstrably did not fix it.
    candidates.retain(|pr| {
        !health
            .poisoned()
            .iter()
            .any(|poison| poison.groups.iter().any(|group| group.prs.contains(pr)))
    });
    (choose_fix(named, health), candidates)
}

/// The one fix this pass fast-tracks, out of every named candidate.
///
/// A pull request that rode one of the streak's failing groups demonstrably
/// did not fix the context, so it is dropped. Of the rest, the strongest
/// evidence wins: the `shipyard:unblock` label (only someone with write
/// access can apply it), then the detector's naming, then a green head
/// ([`green_head_fix`]), then title or description text; ties go to the
/// lowest number. Exactly one fix keeps the
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
        } else if evidence.iter().any(|line| line.starts_with("green head")) {
            2
        } else {
            3
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
        "tip": poison.tip,
        "confirmed": poison.confirmed_contexts(),
        "fix_ambiguity": poison.fix_ambiguity,
        "partial": poison.partial(),
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
    if poison.fixes.is_empty() {
        return poison
            .health
            .poisoned()
            .iter()
            .map(|poisoned| {
                format!(
                    "`{}` would open or find the tracking issue `{}` and list it first in the priority file (no fix is named)",
                    poisoned.context,
                    issue_key("<repo>", "<base>", &poisoned.context)
                )
            })
            .collect();
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
    if contexts.is_empty() {
        return Vec::new();
    }
    // Shadow never opens an issue or touches the priority file, and neither
    // does a streak the base tip does not confirm.
    if poison.policy.hold != PoisonHold::On || poison.confirmed_contexts().is_empty() {
        return would_fast_track(poison);
    }
    let hooks = ReviewHooks::from_machine_global(&control.global_dir);
    if poison.fixes.is_empty() {
        let run_gh = |argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string());
        return poison
            .health
            .poisoned()
            .iter()
            .flat_map(|poisoned| {
                route_root_cause(
                    &run_gh,
                    &hooks,
                    &control.state_dir,
                    &observation.repo,
                    &observation.base,
                    poisoned,
                    Utc::now(),
                )
            })
            .collect();
    }
    let contexts = contexts.join(",");
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

/// Tracking-issue receipts, one line per issue opened or found.
const ISSUE_FILE: &str = "base-health/poison-issues.jsonl";
/// Marker that makes a tracking issue the steward's own.
pub(super) const ISSUE_MARKER: &str = "<!-- shipyard-base-poison: ";
/// How long a receipt is trusted before the open issues are listed again.
const ISSUE_RECEIPT_HOURS: i64 = 24;
/// How long a failed listing or open waits before it is tried again, so a
/// broken read does not page every open issue on every pass.
const ISSUE_RETRY_MINUTES: i64 = 30;

/// One tracking issue the steward opened or found for a poisoned context.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct IssueReceipt {
    /// `OWNER/REPO@BASE#CONTEXT`.
    pub(super) key: String,
    /// The issue.
    pub(super) issue: u64,
    /// RFC 3339.
    pub(super) at: String,
}

/// The marker key of the tracking issue for one poisoned context.
pub(super) fn issue_key(repo: &str, base: &str, context: &str) -> String {
    format!("{}@{base}#{context}", repo.to_ascii_lowercase())
}

/// The body of a new tracking issue.
pub(super) fn issue_body(repo: &str, base: &str, poisoned: &PoisonedContext) -> String {
    let groups = poisoned
        .groups
        .iter()
        .map(|group| {
            format!(
                "- merge group `{}` carrying {}",
                group.head_sha.get(..12).unwrap_or(&group.head_sha),
                group
                    .prs
                    .iter()
                    .map(|pr| format!("#{pr}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The required check `{context}` failed in {n} consecutive merge groups on `{base}` that \
carried different pull requests, so the base itself is broken, not a pull request.\n\n\
First error: `{error}`\n\n{groups}\n\n\
While this lasts the steward holds admission and pauses the queue (`merge_group_hold = \"on\"`). \
A pull request that fixes it is fast-tracked once it carries the `{label}` label or names \
`{context}` next to a fix word in its title or description. When a merge group passes \
`{context}`, the paused entries are restored in their queue order with no further step.\n\n\
{marker}{key} -->",
        context = poisoned.context,
        n = poisoned.groups.len(),
        error = poisoned
            .first_error
            .as_deref()
            .unwrap_or("(no annotation)")
            .replace('`', "'"),
        label = crate::queue_drain::UNBLOCK_LABEL,
        marker = ISSUE_MARKER,
        key = issue_key(repo, base, &poisoned.context),
    )
}

/// When nothing names a fix, open (or find) the context's tracking issue and
/// put it first in the priority file, with a kick, so the root cause is the
/// next thing worked. Returns report lines.
pub(super) fn route_root_cause(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    hooks: &ReviewHooks,
    state_dir: &Path,
    repo: &str,
    base: &str,
    poisoned: &PoisonedContext,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let issue = match ensure_tracking_issue(run_gh, state_dir, repo, base, poisoned, now) {
        Ok((issue, how)) => {
            lines.push(format!(
                "`{}` tracking issue #{issue}: {how}",
                poisoned.context
            ));
            issue
        }
        Err(error) => {
            lines.push(format!(
                "`{}` tracking issue: failed: {error}",
                poisoned.context
            ));
            return lines;
        }
    };
    let entry = format!("{repo}#{issue}");
    if let Some(file) = hooks.priority_file.as_ref() {
        lines.push(match prioritize_line(file, &entry) {
            Ok(true) => format!("{entry} listed first"),
            Ok(false) => format!("{entry} already first"),
            Err(error) => format!("{entry} priority: failed: {error}"),
        });
    }
    if !hooks.kick.is_empty() {
        let reason = format!(
            "base poisoned ({}); no fix is named; root cause tracked in {entry}",
            poisoned.context
        );
        let path = state_dir.join(FAST_TRACK_FILE);
        let kicked = load_receipts(&path).iter().any(|receipt| {
            receipt.repo.eq_ignore_ascii_case(repo)
                && receipt.pr == issue
                && receipt.action == "root_cause_kick"
                && !receipt.outcome.starts_with("failed")
        });
        if !kicked {
            let outcome = run_kick(&hooks.kick, &reason);
            lines.push(format!("{entry} root_cause_kick: {outcome}"));
            let receipt = FastTrackReceipt {
                repo: repo.to_owned(),
                pr: issue,
                head: String::new(),
                action: "root_cause_kick".to_owned(),
                at: now.to_rfc3339(),
                outcome,
            };
            if let Err(error) = append_receipt(&path, &receipt) {
                lines.push(format!(
                    "{entry} root_cause_kick: receipt not recorded: {error}"
                ));
            }
        }
    }
    lines
}

/// The tracking issue for one poisoned context: a fresh receipt, then an
/// open issue carrying the marker, then a new issue. Returns the number and
/// how it was found.
pub(super) fn ensure_tracking_issue(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    state_dir: &Path,
    repo: &str,
    base: &str,
    poisoned: &PoisonedContext,
    now: DateTime<Utc>,
) -> Result<(u64, &'static str), String> {
    let key = issue_key(repo, base, &poisoned.context);
    let path = state_dir.join(ISSUE_FILE);
    let fresh = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<IssueReceipt>(line).ok())
        .rfind(|receipt| {
            if receipt.key != key {
                return false;
            }
            let window = if receipt.issue == 0 {
                chrono::Duration::minutes(ISSUE_RETRY_MINUTES)
            } else {
                chrono::Duration::hours(ISSUE_RECEIPT_HOURS)
            };
            DateTime::parse_from_rfc3339(&receipt.at)
                .is_ok_and(|at| now - at.with_timezone(&Utc) < window)
        });
    if let Some(receipt) = fresh {
        return if receipt.issue == 0 {
            Err("an earlier attempt failed; retrying later".to_owned())
        } else {
            Ok((receipt.issue, "recorded"))
        };
    }
    let record = |issue: u64| {
        let receipt = IssueReceipt {
            key: key.clone(),
            issue,
            at: now.to_rfc3339(),
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(
                file,
                "{}",
                serde_json::to_string(&receipt).unwrap_or_default()
            );
        }
    };
    let failed = |error: String| {
        record(0);
        error
    };
    let open = run_gh(&crate::marked_issue::list_open_args(repo)).map_err(failed)?;
    if let Some(found) = crate::marked_issue::parse(&open, ISSUE_MARKER)
        .into_iter()
        .find(|issue| issue.key == key)
    {
        record(found.number);
        return Ok((found.number, "already open"));
    }
    let title = format!(
        "Base poisoned: `{}` fails every merge group on {base}",
        poisoned.context
    );
    let raw = run_gh(&[
        "api".to_owned(),
        "--method".to_owned(),
        "POST".to_owned(),
        format!("repos/{repo}/issues"),
        "-f".to_owned(),
        format!("title={title}"),
        "-f".to_owned(),
        format!("body={}", issue_body(repo, base, poisoned)),
    ])
    .map_err(failed)?;
    let number = serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|value| value.get("number").and_then(Value::as_u64))
        .ok_or_else(|| failed("the new issue's number was not returned".to_owned()))?;
    record(number);
    Ok((number, "opened"))
}

#[cfg(test)]
mod tests;
