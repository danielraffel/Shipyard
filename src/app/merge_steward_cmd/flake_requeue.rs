//! The flake class: one re-enqueue of an approved head that a flaky test
//! ejected, when the same test ejected another pull request's head too.
//!
//! ## Why
//!
//! On 2026-11-13 a flaky `CoreMIDI` test failed three Pulp merge groups in a
//! row. Each failure ejected a different approved, green pull request. Under
//! the retry discipline none of them earned the classifier's free same-head
//! retry (no infrastructure signature, and the test was not in
//! `known_flaky_tests`), so each sat at `ejected_same_head:failed_checks:
//! needs_new_head` until a human noticed. One pull request's merge-group
//! failure says nothing about who broke it; the same test failing the merge
//! groups of several unrelated pull requests says the test did.
//!
//! ## The signature
//!
//! No new log scraping. When the steward meets an ejected head it already
//! runs the same-head retry classifier ([`crate::environment_requeue`]),
//! which reads every failing required check of the ejecting merge group and
//! each failing step's ctest `The following tests FAILED:` block. The
//! signature is that reading: each failing required check with the tests it
//! named, sorted (`macos: MidiCoreTest.SendsSysex`). It exists only when every
//! failing required check was attributed and every failing step named at
//! least one test; a failure that names no test (a compile error, a crashed
//! step) never has a signature, so it can never be classified as a flake.
//!
//! ## The rules
//!
//! An ejection is `flake:<signature>` when another pull request's head was
//! ejected with the same signature by a **different** merge group (one group
//! ejecting several of its own members is one failure, not two) within
//! `flake_window_hours` of it. The steward then re-enqueues the same head,
//! once, when all of these hold:
//!
//! * the head is unchanged, still approved and green: the enqueue path has
//!   already re-read the exact-head approval gate and the head's required
//!   checks live before it reaches this point;
//! * no `defect-fix` removal names the head (the defect-fix block runs first
//!   and is checked again here);
//! * this pass read the base **green** for every signature check
//!   ([`base_read`]): the merge-group reading has no gaps, and the check's
//!   newest completed run on the base tip, read live, is a `success` (not
//!   `neutral` or `skipped`). A red base is `base_red`; an unread, partial or
//!   unavailable reading, or a tip with no completed run, is `base_unknown`;
//! * the signature has not tripped its breaker: a head the flake class
//!   re-enqueued that the queue ejected **again** with the same signature
//!   says the failure is not a flake (a break the base has not shown red
//!   yet, such as a runner image, a certificate or a date), so the signature
//!   is `signature_tripped` and re-enqueues nothing more. Such a head has two
//!   removals, so the approval gate stops it at the ejection cap before this
//!   class runs; the breaker is therefore read by [`sweep`], before any
//!   admission, from each re-enqueued head's own timeline. A second removal
//!   whose signature cannot be read trips too, and a timeline that cannot be
//!   read holds the signature for the pass (`breaker_unread`). A head that
//!   moved is not followed (its new head has its own signature);
//! * the signature has spent fewer than `flake_max_requeues` re-enqueues
//!   within the window (`signature_capped`);
//! * no earlier flake re-enqueue of this exact head is on record.
//!
//! The receipt is saved `pending` before the enqueue and marked `done` once
//! GitHub accepts it, so a crash cannot buy a second re-enqueue. A head the
//! enqueue path then deferred (a generated-path batch, a refusal from
//! GitHub) keeps a `pending` receipt; after one steward tick
//! ([`PENDING_RETRY_MINUTES`]) it may be retried once, and never again.
//!
//! The flake class never approves anything. It runs only after the
//! steward's own live exact-head approval read has admitted the head, and
//! its ledger (sightings and receipts) can only refuse a re-enqueue, never
//! stand in for approval. Any read it depends on that fails (the merge
//! group's checks or a job log, the removal time, the base health, the
//! receipt save) leaves the ordinary refusal in place.
//!
//! The re-enqueue is separate from the classifier's one free retry: a head
//! the classifier refused still gets it. It does not lift the head-approval
//! ejection cap ([`crate::head_approval::EJECTION_CAP`]); a head ejected
//! again after its flake re-enqueue is left for repair.
//!
//! ## The root cause
//!
//! Each flake signature is reported in the tick as a `flake_root_cause`
//! record (signature, checks, tests, pull requests, ejection window,
//! breaker state, issue). Under `apply` the steward also files one issue per
//! signature, found again by a marker on its last line: a new pull request
//! joining the signature, or the breaker tripping, adds a comment to the
//! same issue instead of a second issue, and a closed issue is recorded and
//! left alone. Test names are sanitised before they reach an issue. A failed
//! or refused write is reported and retried after [`ISSUE_RETRY_HOURS`],
//! never routed around.
//!
//! Mode: `[auto_merge] flake_requeue` ([`crate::merge_steward::flake_requeue`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::StewardLedger;
use super::base_poison::RepoPoison;
use crate::base_health::merge_group::MergeGroupHealth;
use crate::environment_requeue::{EnvironmentRequeue, FailureAttribution, StepEvidence};
use crate::merge_steward::flake_requeue::{FlakeMode, FlakePolicy};
use crate::pr_queue_state::PrQueueReport;

/// How long a sighting is kept after the steward last saw its head ejected.
pub(super) const RETENTION_DAYS: i64 = 7;
/// How long a re-enqueue receipt, a breaker trip and an issue record are
/// kept: longer than any head waits.
pub(super) const RECEIPT_DAYS: i64 = 30;
/// How long after a failed issue write the next attempt waits.
pub(super) const ISSUE_RETRY_HOURS: i64 = 6;
/// One steward tick: a `pending` receipt older than this whose head was not
/// enqueued may be retried once.
pub(super) const PENDING_RETRY_MINUTES: i64 = 5;
/// Attempts one head's flake re-enqueue may take: the first, and the one
/// retry of a `pending` receipt.
pub(super) const MAX_ATTEMPTS: u32 = 2;
/// The marker that finds a flake issue again: the last line of its body.
pub(super) const ISSUE_MARKER: &str = "<!-- shipyard-flake-signature: ";
/// Longest test or check name an issue shows.
const NAME_CHARS: usize = 120;
/// Most test names an issue shows per signature.
const NAMES_SHOWN: usize = 10;
/// Most ejections an issue body or comment lists.
const LINES_SHOWN: usize = 30;

/// What the steward ledger keeps between passes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct State {
    /// Ejections the classifier read a signature for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) sightings: Vec<Sighting>,
    /// Flake re-enqueues taken, keyed `repo#pr@head`: one per head, ever.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) requeued: BTreeMap<String, Receipt>,
    /// Signatures whose breaker tripped, keyed [`issue_key`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) tripped: BTreeMap<String, Trip>,
    /// The root-cause issue per signature, keyed [`issue_key`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) issues: BTreeMap<String, IssueRecord>,
    /// This pass's decisions, reported and dropped. Never persisted.
    #[serde(skip)]
    pub(super) pass: Vec<Decision>,
    /// This pass only: signatures (keyed [`issue_key`]) with a re-enqueued
    /// head whose second-ejection check could not be read ([`sweep`]), and
    /// why. They re-enqueue nothing this pass. Never persisted.
    #[serde(skip)]
    pub(super) unswept: BTreeMap<String, String>,
}

impl State {
    /// Whether there is nothing to persist.
    pub(super) fn is_empty(&self) -> bool {
        self.sightings.is_empty()
            && self.requeued.is_empty()
            && self.tripped.is_empty()
            && self.issues.is_empty()
    }
}

/// One ejection of one head with a signature.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Sighting {
    pub(super) repo: String,
    pub(super) pr: u64,
    /// Lowercase head.
    pub(super) head: String,
    pub(super) signature: Signature,
    /// The ejecting merge-group commit.
    pub(super) merge_group: String,
    /// When the queue removed the head (RFC 3339).
    pub(super) at: String,
    /// When the steward last saw this ejection (epoch seconds).
    pub(super) seen_epoch: i64,
}

/// The failing required checks of one ejection, with their failing tests.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(super) struct Signature {
    /// `check: test, test + check: test`, sorted.
    pub(super) text: String,
    pub(super) checks: Vec<String>,
    pub(super) tests: Vec<String>,
}

/// Where one head's flake re-enqueue stands.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ReceiptState {
    /// Recorded before the enqueue; GitHub has not accepted it yet.
    Pending,
    /// GitHub accepted the enqueue, or the queue has since ejected the head
    /// again (so it was enqueued). A receipt written without a state reads
    /// as `done`, so it is spent and never retried.
    #[default]
    Done,
}

/// One head's flake re-enqueue.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Receipt {
    /// Lowercase repository.
    #[serde(default)]
    pub(super) repo: String,
    pub(super) signature: String,
    /// The ejection it answered (RFC 3339).
    pub(super) ejection_at: String,
    /// When it was last attempted (RFC 3339).
    pub(super) at: String,
    #[serde(default)]
    pub(super) state: ReceiptState,
    /// Attempts taken, at most [`MAX_ATTEMPTS`].
    #[serde(default = "one")]
    pub(super) attempts: u32,
    /// The breaker sweep saw this head ejected again and judged it
    /// ([`sweep`]); it is not read again.
    #[serde(default)]
    pub(super) swept: bool,
}

const fn one() -> u32 {
    1
}

/// A signature's tripped breaker: the head that was ejected again.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Trip {
    pub(super) signature: String,
    pub(super) pr: u64,
    pub(super) head: String,
    /// The second ejection (RFC 3339).
    pub(super) at: String,
    /// The second ejection's signature could not be read: tripped closed.
    #[serde(default)]
    pub(super) unread: bool,
}

impl Trip {
    fn describe(&self) -> String {
        format!(
            "#{} at head {} was ejected again {} at {} after its flake re-enqueue",
            self.pr,
            short(&self.head),
            if self.unread {
                "(its signature could not be read)"
            } else {
                "with the same signature"
            },
            self.at
        )
    }
}

/// The root-cause issue of one signature.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct IssueRecord {
    /// The issue number; 0 after a failed write.
    pub(super) number: u64,
    /// The pull requests the issue names.
    #[serde(default)]
    pub(super) prs: Vec<u64>,
    /// The issue says the breaker tripped.
    #[serde(default)]
    pub(super) tripped: bool,
    /// The issue was found closed: it is left alone.
    #[serde(default)]
    pub(super) closed: bool,
    /// When it was last written or attempted (RFC 3339).
    pub(super) at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

/// What the steward did with one flake-classified ejection this pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Decision {
    #[serde(skip)]
    pub(super) repo: String,
    pub(super) pr: u64,
    pub(super) head: String,
    /// `flake:<signature>`.
    pub(super) class: String,
    /// `requeued`, `requeue_retried`, `would_requeue`, `requeue_pending`,
    /// `already_requeued`, `defect_fix_removed`, `base_red`, `base_unknown`,
    /// `signature_tripped`, `signature_capped` or `unrecorded`.
    pub(super) outcome: &'static str,
    /// The other pull requests the same signature ejected.
    pub(super) with: Vec<u64>,
}

/// One flake signature's root cause, for the coordinator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct RootCause {
    pub(super) signature: String,
    pub(super) checks: Vec<String>,
    pub(super) tests: Vec<String>,
    pub(super) prs: Vec<u64>,
    pub(super) first_ejection: String,
    pub(super) last_ejection: String,
    /// `active`, or `signature_tripped` once the breaker tripped.
    pub(super) state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tripped_by: Option<String>,
    /// Flake re-enqueues this signature spent within the window.
    pub(super) requeues: usize,
    /// The issue, when one is filed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) issue: Option<u64>,
    /// `opened`, `found`, `updated`, `recorded`, `closed`, `would_file`,
    /// `retry_later` or `failed`.
    pub(super) issue_action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) issue_error: Option<String>,
}

/// The repository report section.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct RepoStatus {
    pub(super) mode: String,
    pub(super) window_hours: u32,
    pub(super) max_requeues: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) decisions: Vec<Decision>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) flake_root_cause: Vec<RootCause>,
    /// Signatures the breaker sweep could not check this pass, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) breaker_unread: Vec<String>,
}

/// The signature of a classifier verdict, when it has one: every failing
/// required check was attributed, none is the head's own fault, and every
/// failing step of each named at least one test.
pub(super) fn signature(verdict: &EnvironmentRequeue) -> Option<Signature> {
    if verdict.attribution.is_empty()
        || verdict
            .attribution
            .iter()
            .any(|check| matches!(check.attribution, FailureAttribution::HeadCaused(_)))
    {
        return None;
    }
    signature_of(
        &verdict.evidence,
        verdict.attribution.iter().map(|check| check.check.as_str()),
    )
}

/// The signature the step evidence of `checks` reads, when every one of
/// them has failing steps and each step named at least one test.
fn signature_of<'a>(
    evidence: &[StepEvidence],
    checks: impl IntoIterator<Item = &'a str>,
) -> Option<Signature> {
    let mut by_check: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for check in checks {
        let steps = evidence
            .iter()
            .filter(|step| step.check == check)
            .collect::<Vec<_>>();
        if steps.is_empty() || steps.iter().any(|step| step.failed_tests.is_empty()) {
            return None;
        }
        by_check.entry(check).or_default().extend(
            steps
                .iter()
                .flat_map(|step| step.failed_tests.iter())
                .map(String::as_str),
        );
    }
    if by_check.is_empty() {
        return None;
    }
    let text = by_check
        .iter()
        .map(|(check, tests)| {
            format!(
                "{check}: {}",
                tests.iter().copied().collect::<Vec<_>>().join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join(" + ");
    let tests = by_check
        .values()
        .flatten()
        .map(|test| (*test).to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Some(Signature {
        text,
        checks: by_check.keys().map(|check| (*check).to_owned()).collect(),
        tests,
    })
}

/// The signature of a re-ejection, read from its step evidence alone (the
/// classifier attributes nothing after a head's first ejection).
fn evidence_signature(verdict: &EnvironmentRequeue) -> Option<Signature> {
    let checks = verdict
        .evidence
        .iter()
        .map(|step| step.check.as_str())
        .collect::<BTreeSet<_>>();
    signature_of(&verdict.evidence, checks)
}

/// What this pass knows about the base for a signature's checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BaseRead {
    /// Every check's newest completed run on the base tip is a `success`,
    /// and the merge-group reading has no gaps.
    Green,
    /// The base is red on this check: confirmed by base poison, or its
    /// newest completed run on the tip failed.
    Red(String),
    /// Not known: why.
    Unknown(String),
}

/// Reads one check's newest completed pass-or-fail conclusion on the base
/// tip (`Ok(None)` when it has none).
pub(super) type TipVerdict<'a> = &'a dyn Fn(&str) -> Result<Option<String>, String>;

/// The base reading for `checks`. Only a positive green reading is
/// [`BaseRead::Green`]: every check's newest completed run on the base tip
/// is a `success` (not `neutral` or `skipped`), read this pass. Base poison
/// off, an unread or partial merge-group reading, no checks, a tip with no
/// completed run (pending, cancelled, never run on a push), a `neutral` or
/// `skipped` newest run, or a tip that could not be read is
/// [`BaseRead::Unknown`].
pub(super) fn base_read(
    poison: Option<&RepoPoison>,
    checks: &[String],
    tip: TipVerdict<'_>,
) -> BaseRead {
    let Some(poison) = poison else {
        return BaseRead::Unknown(
            "the base's merge-group health was not read this pass (base poison off or \
             unavailable)"
                .to_owned(),
        );
    };
    let confirmed = poison.confirmed_contexts();
    if let Some(check) = checks.iter().find(|check| confirmed.contains(check)) {
        return BaseRead::Red(check.clone());
    }
    if let Some(gap) = poison.partial() {
        return BaseRead::Unknown(format!("the base reading is partial: {gap}"));
    }
    if let MergeGroupHealth::Unknown { detail } = &poison.health {
        return BaseRead::Unknown(format!("the merge groups were not read: {detail}"));
    }
    if checks.is_empty() {
        return BaseRead::Unknown("the signature names no check".to_owned());
    }
    for check in checks {
        match tip(check) {
            Ok(Some(conclusion)) if conclusion == "success" => {}
            Ok(Some(conclusion)) if FAILING.contains(&conclusion.as_str()) => {
                return BaseRead::Red(check.clone());
            }
            Ok(Some(conclusion)) => {
                return BaseRead::Unknown(format!(
                    "`{check}`'s newest completed run on the base tip is `{conclusion}`, not \
                     `success`"
                ));
            }
            Ok(None) => {
                return BaseRead::Unknown(format!(
                    "`{check}` has no completed run on the base tip"
                ));
            }
            Err(error) => {
                return BaseRead::Unknown(format!(
                    "`{check}` on the base tip could not be read: {error}"
                ));
            }
        }
    }
    BaseRead::Green
}

/// Conclusions that pass a check, as the base-health readers count them.
const PASSING: [&str; 3] = ["success", "neutral", "skipped"];
/// Conclusions that fail a check.
const FAILING: [&str; 3] = ["failure", "timed_out", "action_required"];

/// The newest completed pass-or-fail conclusion of `check` in a
/// [`crate::base_health::fix_first::tip_check_path`] answer, from `app_id`
/// when the rule pins one. Cancelled, stale and running runs say nothing;
/// `None` when no run says anything.
pub(super) fn newest_tip_verdict(
    value: &Value,
    check: &str,
    app_id: Option<u64>,
) -> Option<String> {
    value
        .get("check_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|run| run.get("name").and_then(Value::as_str) == Some(check))
        .filter(|run| {
            app_id
                .is_none_or(|app_id| run.pointer("/app/id").and_then(Value::as_u64) == Some(app_id))
        })
        .filter(|run| run.get("status").and_then(Value::as_str) == Some("completed"))
        .filter_map(|run| {
            let conclusion = run
                .get("conclusion")
                .and_then(Value::as_str)?
                .to_ascii_lowercase();
            (PASSING.contains(&conclusion.as_str()) || FAILING.contains(&conclusion.as_str()))
                .then_some((run.get("id").and_then(Value::as_u64)?, conclusion))
        })
        .max_by_key(|(id, _)| *id)
        .map(|(_, conclusion)| conclusion)
}

fn time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

pub(super) fn receipt_key(repo: &str, pr: u64, head: &str) -> String {
    format!(
        "{}#{pr}@{}",
        repo.to_ascii_lowercase(),
        head.to_ascii_lowercase()
    )
}

/// The key of one signature (issue and breaker): the repository and a
/// digest of the signature, so the marker never carries a test name.
pub(super) fn issue_key(repo: &str, signature: &str) -> String {
    let digest = Sha256::digest(signature.as_bytes());
    let hex = digest.iter().take(8).fold(String::new(), |mut hex, byte| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    format!("{}|{hex}", repo.to_ascii_lowercase())
}

/// The other pull requests whose heads the same signature ejected, from a
/// different merge group, within the window of `this`.
pub(super) fn corroborating(state: &State, this: &Sighting, window: Duration) -> Vec<u64> {
    let Some(at) = time(&this.at) else {
        return Vec::new();
    };
    state
        .sightings
        .iter()
        .filter(|other| {
            other.repo.eq_ignore_ascii_case(&this.repo)
                && other.pr != this.pr
                && other.signature.text == this.signature.text
                && !other.merge_group.eq_ignore_ascii_case(&this.merge_group)
                && time(&other.at).is_some_and(|other_at| (other_at - at).abs() <= window)
        })
        .map(|other| other.pr)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Flake re-enqueues `signature` spent in `repo` within `window` of `now`,
/// leaving out the receipt keyed `except`.
fn requeues_in_window(
    state: &State,
    repo: &str,
    signature: &str,
    window: Duration,
    now: DateTime<Utc>,
    except: Option<&str>,
) -> usize {
    let prefix = format!("{}#", repo.to_ascii_lowercase());
    state
        .requeued
        .iter()
        .filter(|(key, receipt)| {
            key.starts_with(&prefix)
                && Some(key.as_str()) != except
                && receipt.signature == signature
                && time(&receipt.at).is_some_and(|at| now - at <= window)
        })
        .count()
}

/// One ejection the enqueue path refused, as the steward read it.
pub(super) struct Ejection<'a> {
    pub(super) policy: &'a FlakePolicy,
    pub(super) repo: &'a str,
    pub(super) pr: u64,
    pub(super) head: &'a str,
    /// When the queue removed the head.
    pub(super) at: Option<&'a str>,
    /// The classifier's verdict on it.
    pub(super) verdict: &'a EnvironmentRequeue,
    /// A `defect-fix` removal names this head.
    pub(super) defect_fix_removed: bool,
    /// This pass's base-poison reading, when one was made.
    pub(super) poison: Option<&'a RepoPoison>,
    /// Reads a check's newest completed verdict on the base tip.
    pub(super) tip: TipVerdict<'a>,
}

/// What the enqueue path should do with a flake-classified ejection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Re-enqueue the head now; the `pending` receipt is saved.
    Requeue(String),
    /// Keep the refusal, with this added to the retry detail.
    Keep(String),
}

/// Record the ejection and classify it. `None` when the flake class is off,
/// the classifier already allows a retry, or the ejection has no signature
/// or no corroborating pull request (a single ejection is never a flake).
///
/// Under `apply`, an eligible head's `pending` receipt is recorded and the
/// ledger saved to `ledger_path` before this returns [`Verdict::Requeue`];
/// a save that fails keeps the refusal.
pub(super) fn consider(
    ejection: &Ejection<'_>,
    ledger: &mut StewardLedger,
    ledger_path: Option<&Path>,
    now: DateTime<Utc>,
) -> Option<Verdict> {
    if ejection.policy.mode == FlakeMode::Off || ejection.verdict.allowed {
        return None;
    }
    let this = record(ejection, &mut ledger.flake_requeue, now)?;
    let key = receipt_key(ejection.repo, ejection.pr, ejection.head);
    trip_on_second_ejection(&mut ledger.flake_requeue, &this, &key);
    let window = Duration::hours(i64::from(ejection.policy.window_hours));
    let with = corroborating(&ledger.flake_requeue, &this, window);
    if with.is_empty() {
        return None;
    }
    let class = format!("flake:{}", this.signature.text);
    let (mut outcome, detail) = decide(ejection, &ledger.flake_requeue, &this, &with, &key, now);
    let verdict = if matches!(outcome, "requeued" | "requeue_retried") {
        let previous = ledger.flake_requeue.requeued.get(&key).cloned();
        ledger.flake_requeue.requeued.insert(
            key.clone(),
            Receipt {
                repo: ejection.repo.to_ascii_lowercase(),
                signature: this.signature.text.clone(),
                ejection_at: this.at.clone(),
                at: now.to_rfc3339(),
                state: ReceiptState::Pending,
                attempts: previous.as_ref().map_or(1, |receipt| receipt.attempts + 1),
                swept: false,
            },
        );
        let saved = ledger_path.map_or(Ok(()), |path| {
            super::ledger::save_ledger(path, ledger).map_err(|error| error.message)
        });
        match saved {
            Ok(()) => Verdict::Requeue(detail),
            Err(error) => {
                match previous {
                    Some(previous) => {
                        ledger.flake_requeue.requeued.insert(key.clone(), previous);
                    }
                    None => {
                        ledger.flake_requeue.requeued.remove(&key);
                    }
                }
                outcome = "unrecorded";
                Verdict::Keep(format!(
                    "{class}: the re-enqueue receipt could not be saved ({error}), so the head \
                     is not re-enqueued"
                ))
            }
        }
    } else {
        Verdict::Keep(detail)
    };
    ledger.flake_requeue.pass.push(Decision {
        repo: ejection.repo.to_owned(),
        pr: ejection.pr,
        head: this.head,
        class,
        outcome,
        with,
    });
    Some(verdict)
}

/// Record the ejection's sighting (once per head and merge group). `None`
/// when it has no signature, removal time or merge-group commit.
fn record(ejection: &Ejection<'_>, state: &mut State, now: DateTime<Utc>) -> Option<Sighting> {
    let signature = signature(ejection.verdict)?;
    let at = ejection.at.filter(|at| time(at).is_some())?;
    let merge_group = ejection.verdict.merge_group_commit.clone()?;
    prune(state, now);
    let this = Sighting {
        repo: ejection.repo.to_owned(),
        pr: ejection.pr,
        head: ejection.head.to_ascii_lowercase(),
        signature,
        merge_group,
        at: at.to_owned(),
        seen_epoch: now.timestamp(),
    };
    match state.sightings.iter_mut().find(|seen| {
        seen.repo.eq_ignore_ascii_case(&this.repo)
            && seen.pr == this.pr
            && seen.head == this.head
            && seen.merge_group.eq_ignore_ascii_case(&this.merge_group)
    }) {
        Some(seen) => seen.clone_from(&this),
        None => state.sightings.push(this.clone()),
    }
    Some(this)
}

/// The breaker: a head this class re-enqueued that the queue removed again,
/// after the re-enqueue, with the same signature. The re-enqueue reached the
/// queue (so the receipt is `done`), and the signature trips.
fn trip_on_second_ejection(state: &mut State, this: &Sighting, key: &str) {
    let Some(receipt) = state.requeued.get_mut(key) else {
        return;
    };
    let after = time(&this.at)
        .zip(time(&receipt.at))
        .is_some_and(|(ejected, requeued)| ejected > requeued);
    if !after {
        return;
    }
    receipt.state = ReceiptState::Done;
    if receipt.signature != this.signature.text {
        return;
    }
    state
        .tripped
        .entry(issue_key(&this.repo, &this.signature.text))
        .or_insert_with(|| Trip {
            signature: this.signature.text.clone(),
            pr: this.pr,
            head: this.head.clone(),
            at: this.at.clone(),
            unread: false,
        });
}

/// What the breaker sweep reads for one re-enqueued head.
pub(super) struct SweepReads<'a> {
    /// The pull request's merge-queue timeline (a queue-state answer).
    pub(super) queue: &'a dyn Fn(u64) -> Result<Value, String>,
    /// The classifier's reading of the removal the timeline ends with.
    pub(super) assess: &'a dyn Fn(&PrQueueReport) -> Option<EnvironmentRequeue>,
}

/// Return the removal that proves this receipt's head was ejected again.
///
/// A queue response without the required removal, live head, or timestamps is
/// incomplete evidence. Treat it as unreadable so the receipt remains
/// unswept and the signature stays blocked until a later pass can read it.
fn post_requeue_ejection<'a>(
    report: &'a PrQueueReport,
    head: &str,
    requeued_at: &str,
) -> Result<&'a crate::pr_queue_state::Ejection, String> {
    let removal = report
        .last_ejection
        .as_ref()
        .ok_or_else(|| "missing last queue removal".to_owned())?;
    let live_head = report
        .head_oid
        .as_deref()
        .ok_or_else(|| "missing live head oid".to_owned())?;
    if !live_head.eq_ignore_ascii_case(head) {
        return Err(format!(
            "live head {live_head} does not match receipt head {head}"
        ));
    }
    let removed_at = removal
        .at
        .as_deref()
        .and_then(time)
        .ok_or_else(|| "missing or invalid removal timestamp".to_owned())?;
    let requeued_at =
        time(requeued_at).ok_or_else(|| "missing or invalid re-enqueue timestamp".to_owned())?;
    if removed_at <= requeued_at {
        return Err(format!(
            "removal timestamp {} is not after re-enqueue timestamp {}",
            removal.at.as_deref().unwrap_or("<missing>"),
            requeued_at.to_rfc3339(),
        ));
    }
    Ok(removal)
}

/// The breaker, outside the admission path. A head this class re-enqueued
/// that the queue ejects again has two removals, so the approval gate stops
/// it at the ejection cap before [`consider`] ever sees it. Each pass, every
/// re-enqueued head that is still the open pull request's head and is not
/// queued has its timeline read: a `failed_checks` removal after the
/// re-enqueue whose evidence reads the same signature trips the signature,
/// and so does one whose signature cannot be read (fail closed). A removal
/// with another signature, or for another reason, is judged and not read
/// again. A read that fails marks the signature `breaker_unread` for this
/// pass: nothing on it is re-enqueued until the sweep can read it. A head
/// that moved is not followed: its new head would need a new signature.
pub(super) fn sweep(
    policy: &FlakePolicy,
    observation: &super::RepoObservation,
    ledger: &mut StewardLedger,
    reads: &SweepReads<'_>,
) {
    let repo = &observation.repo;
    let state = &mut ledger.flake_requeue;
    let signature_prefix = format!("{}|", repo.to_ascii_lowercase());
    state
        .unswept
        .retain(|key, _| !key.starts_with(&signature_prefix));
    if policy.mode == FlakeMode::Off {
        return;
    }
    let prefix = format!("{}#", repo.to_ascii_lowercase());
    let keys = state
        .requeued
        .iter()
        .filter(|(key, receipt)| key.starts_with(&prefix) && !receipt.swept)
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in keys {
        let Some((pr, head)) = key[prefix.len()..]
            .split_once('@')
            .and_then(|(pr, head)| Some((pr.parse::<u64>().ok()?, head.to_owned())))
        else {
            continue;
        };
        let Some(open) = observation
            .prs
            .iter()
            .find(|open| open.fact.number == pr)
            .filter(|open| open.fact.head_sha.eq_ignore_ascii_case(&head))
        else {
            continue;
        };
        if open.fact.queue_position.is_some() {
            continue;
        }
        let receipt = state.requeued[&key].clone();
        let signature_key = issue_key(repo, &receipt.signature);
        if state.tripped.contains_key(&signature_key) {
            if let Some(receipt) = state.requeued.get_mut(&key) {
                receipt.swept = true;
            }
            continue;
        }
        let queue = match (reads.queue)(pr) {
            Ok(queue) => queue,
            Err(error) => {
                state
                    .unswept
                    .insert(signature_key, format!("#{pr} timeline unread: {error}"));
                continue;
            }
        };
        let report = crate::pr_queue_state::explain_pr_queue_state(&queue);
        let removal = match post_requeue_ejection(&report, &head, &receipt.at) {
            Ok(removal) => removal,
            Err(detail) => {
                state.unswept.insert(
                    signature_key,
                    format!("#{pr} queue report unread: {detail}"),
                );
                continue;
            }
        };
        if let Some(receipt) = state.requeued.get_mut(&key) {
            receipt.state = ReceiptState::Done;
            receipt.swept = true;
        }
        if removal.reason != "failed_checks" {
            continue;
        }
        let read = (reads.assess)(&report)
            .as_ref()
            .and_then(evidence_signature);
        let unread = read.is_none();
        if read.is_none_or(|read| read.text == receipt.signature) {
            state.tripped.entry(signature_key).or_insert_with(|| Trip {
                signature: receipt.signature.clone(),
                pr,
                head: head.clone(),
                at: removal.at.clone().unwrap_or_default(),
                unread,
            });
        }
    }
}

/// What to do with a corroborated flake ejection: `(outcome, detail)`.
/// `requeued` and `requeue_retried` are the only outcomes that re-enqueue.
fn decide(
    ejection: &Ejection<'_>,
    state: &State,
    this: &Sighting,
    with: &[u64],
    key: &str,
    now: DateTime<Utc>,
) -> (&'static str, String) {
    let class = format!("flake:{}", this.signature.text);
    let keep = |outcome: &'static str, why: String| (outcome, format!("{class}: {why}"));
    if ejection.defect_fix_removed {
        return keep(
            "defect_fix_removed",
            "a defect-fix removal names this head, so it is not re-enqueued".to_owned(),
        );
    }
    let signature_key = issue_key(ejection.repo, &this.signature.text);
    if let Some(trip) = state.tripped.get(&signature_key) {
        return keep(
            "signature_tripped",
            format!(
                "the breaker tripped ({}); this signature is not a flake and re-enqueues \
                 nothing more",
                trip.describe()
            ),
        );
    }
    if let Some(why) = state.unswept.get(&signature_key) {
        return keep(
            "breaker_unread",
            format!(
                "an earlier flake re-enqueue on this signature could not be checked for a \
                 second ejection this pass ({why}); not re-enqueued"
            ),
        );
    }
    let retry = match receipt_gate(state, key, now) {
        Ok(retry) => retry,
        Err((outcome, why)) => return keep(outcome, why),
    };
    let window = Duration::hours(i64::from(ejection.policy.window_hours));
    let spent = requeues_in_window(
        state,
        ejection.repo,
        &this.signature.text,
        window,
        now,
        Some(key),
    );
    if spent >= ejection.policy.max_requeues as usize {
        return keep(
            "signature_capped",
            format!(
                "this signature spent {spent} flake re-enqueues within {}h (cap {}); not \
                 re-enqueued",
                ejection.policy.window_hours, ejection.policy.max_requeues
            ),
        );
    }
    // Last, so a refusal above costs no read.
    match base_read(ejection.poison, &this.signature.checks, ejection.tip) {
        BaseRead::Green => {}
        BaseRead::Red(context) => {
            return keep(
                "base_red",
                format!(
                    "the base is red on `{context}`, which is the base's failure, not a flake; \
                     not re-enqueued"
                ),
            );
        }
        BaseRead::Unknown(why) => {
            return keep(
                "base_unknown",
                format!("{why}; a flake re-enqueue needs the base read green, so it waits"),
            );
        }
    }
    let others = with
        .iter()
        .map(|pr| format!("#{pr}"))
        .collect::<Vec<_>>()
        .join(", ");
    if ejection.policy.mode == FlakeMode::Shadow {
        return keep(
            "would_requeue",
            format!(
                "the same test ejected {others} too; would re-enqueue this head once \
                 (flake_requeue = \"shadow\")"
            ),
        );
    }
    if retry {
        return keep(
            "requeue_retried",
            "the earlier flake re-enqueue never reached the queue; retried once".to_owned(),
        );
    }
    keep(
        "requeued",
        format!(
            "the same test ejected {others} too, so this unchanged, approved, green head is \
             re-enqueued once"
        ),
    )
}

/// This head's receipt: `Ok(false)` when there is none, `Ok(true)` when a
/// `pending` one older than a tick may take its one retry, else the refusal.
fn receipt_gate(
    state: &State,
    key: &str,
    now: DateTime<Utc>,
) -> Result<bool, (&'static str, String)> {
    let Some(receipt) = state.requeued.get(key) else {
        return Ok(false);
    };
    let age = time(&receipt.at).map(|at| now - at);
    match receipt.state {
        ReceiptState::Pending
            if age.is_some_and(|age| age < Duration::minutes(PENDING_RETRY_MINUTES)) =>
        {
            Err((
                "requeue_pending",
                format!(
                    "this head's flake re-enqueue from {} is still pending",
                    receipt.at
                ),
            ))
        }
        ReceiptState::Pending if receipt.attempts < MAX_ATTEMPTS && age.is_some() => Ok(true),
        _ => Err((
            "already_requeued",
            format!(
                "this head's one flake re-enqueue was spent at {}",
                receipt.at
            ),
        )),
    }
}

/// After the enqueue path ran: a `pending` receipt for this exact head whose
/// enqueue GitHub accepted (`mutation` has an `enqueued` part) is `done`,
/// and saved. Anything else leaves it `pending`.
pub(super) fn settle(
    ledger: &mut StewardLedger,
    ledger_path: Option<&Path>,
    repo: &str,
    pr: u64,
    head: &str,
    mutation: Option<&str>,
) {
    let enqueued = mutation
        .is_some_and(|mutation| mutation.split(',').any(|part| part.starts_with("enqueued")));
    if !enqueued {
        return;
    }
    let Some(receipt) = ledger
        .flake_requeue
        .requeued
        .get_mut(&receipt_key(repo, pr, head))
        .filter(|receipt| receipt.state == ReceiptState::Pending)
    else {
        return;
    };
    receipt.state = ReceiptState::Done;
    // Best effort: the pass saves the ledger at its end too, and a receipt
    // left `pending` by a crash here is still spent once the queue ejects
    // the head again (`trip_on_second_ejection`), and cannot be retried
    // while the head sits in the queue.
    if let Some(path) = ledger_path {
        let _ = super::ledger::save_ledger(path, ledger);
    }
}

fn prune(state: &mut State, now: DateTime<Utc>) {
    let seen_floor = (now - Duration::days(RETENTION_DAYS)).timestamp();
    state
        .sightings
        .retain(|sighting| sighting.seen_epoch >= seen_floor);
    let floor = now - Duration::days(RECEIPT_DAYS);
    state
        .requeued
        .retain(|_, receipt| time(&receipt.at).is_some_and(|at| at >= floor));
    state
        .tripped
        .retain(|_, trip| time(&trip.at).is_some_and(|at| at >= floor));
    state
        .issues
        .retain(|_, issue| time(&issue.at).is_some_and(|at| at >= floor));
}

/// One flake signature in this repository: its sightings, every one
/// corroborated by another pull request's.
struct Class {
    signature: Signature,
    sightings: Vec<Sighting>,
    trip: Option<Trip>,
}

fn classes(state: &State, repo: &str, window: Duration) -> Vec<Class> {
    let mut by_signature: BTreeMap<String, Class> = BTreeMap::new();
    for sighting in state
        .sightings
        .iter()
        .filter(|sighting| sighting.repo.eq_ignore_ascii_case(repo))
        .filter(|sighting| !corroborating(state, sighting, window).is_empty())
    {
        by_signature
            .entry(sighting.signature.text.clone())
            .or_insert_with(|| Class {
                signature: sighting.signature.clone(),
                sightings: Vec::new(),
                trip: state
                    .tripped
                    .get(&issue_key(repo, &sighting.signature.text))
                    .cloned(),
            })
            .sightings
            .push(sighting.clone());
    }
    by_signature.into_values().collect()
}

/// The `gh` transport: stdout, or a message.
pub(super) type RunGh<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

/// The end-of-pass step: report this pass's decisions and every flake
/// signature in the repository, and under `apply` (on an applying pass)
/// file or update each signature's root-cause issue.
pub(super) fn finish(
    run_gh: RunGh<'_>,
    policy: &FlakePolicy,
    repo: &str,
    base: &str,
    ledger: &mut StewardLedger,
    apply: bool,
    now: DateTime<Utc>,
) -> RepoStatus {
    let state = &mut ledger.flake_requeue;
    let (decisions, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut state.pass)
        .into_iter()
        .partition(|decision| decision.repo.eq_ignore_ascii_case(repo));
    state.pass = rest;
    let mut status = RepoStatus {
        mode: policy.mode.label().to_owned(),
        window_hours: policy.window_hours,
        max_requeues: policy.max_requeues,
        decisions,
        flake_root_cause: Vec::new(),
        breaker_unread: state
            .unswept
            .iter()
            .filter(|(key, _)| key.starts_with(&format!("{}|", repo.to_ascii_lowercase())))
            .map(|(_, why)| why.clone())
            .collect(),
    };
    if policy.mode == FlakeMode::Off {
        return status;
    }
    prune(state, now);
    let window = Duration::hours(i64::from(policy.window_hours));
    let mut open_issues: OpenIssues = None;
    for class in classes(state, repo, window) {
        let prs = class
            .sightings
            .iter()
            .map(|sighting| sighting.pr)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut times = class
            .sightings
            .iter()
            .map(|sighting| sighting.at.clone())
            .collect::<Vec<_>>();
        times.sort();
        let mut cause = RootCause {
            signature: class.signature.text.clone(),
            checks: class.signature.checks.clone(),
            tests: class.signature.tests.clone(),
            prs: prs.clone(),
            first_ejection: times.first().cloned().unwrap_or_default(),
            last_ejection: times.last().cloned().unwrap_or_default(),
            state: if class.trip.is_some() {
                "signature_tripped"
            } else {
                "active"
            },
            tripped_by: class.trip.as_ref().map(Trip::describe),
            requeues: requeues_in_window(state, repo, &class.signature.text, window, now, None),
            issue: None,
            issue_action: "would_file",
            issue_error: None,
        };
        if policy.mode == FlakeMode::Apply && apply {
            ensure_issue(
                run_gh,
                &mut open_issues,
                repo,
                base,
                &class,
                &prs,
                &mut state.issues,
                &mut cause,
                now,
            );
        }
        status.flake_root_cause.push(cause);
    }
    status
}

/// Open issues carrying [`ISSUE_MARKER`] on their last line, listed at most
/// once per pass.
type OpenIssues = Option<Result<Vec<crate::marked_issue::MarkedIssue>, String>>;

/// File, find or update the root-cause issue of one signature, recording
/// the outcome in `cause` and `issues`.
#[allow(clippy::too_many_arguments)]
fn ensure_issue(
    run_gh: RunGh<'_>,
    open_issues: &mut OpenIssues,
    repo: &str,
    base: &str,
    class: &Class,
    prs: &[u64],
    issues: &mut BTreeMap<String, IssueRecord>,
    cause: &mut RootCause,
    now: DateTime<Utc>,
) {
    let key = issue_key(repo, &class.signature.text);
    let recorded = issues.get(&key).cloned();
    if let Some(record) = &recorded {
        if record.closed {
            cause.issue = Some(record.number);
            cause.issue_action = "closed";
            return;
        }
        if record.number == 0
            && time(&record.at).is_some_and(|at| now - at < Duration::hours(ISSUE_RETRY_HOURS))
        {
            cause.issue_action = "retry_later";
            cause.issue_error.clone_from(&record.error);
            return;
        }
    }
    let filed = recorded.as_ref().filter(|record| record.number > 0);
    let result = write_issue(run_gh, open_issues, repo, base, class, prs, filed, &key);
    let (record, action, error) = match result {
        Ok((number, action)) => (
            IssueRecord {
                number,
                prs: prs.to_vec(),
                tripped: class.trip.is_some(),
                closed: action == "closed",
                at: now.to_rfc3339(),
                error: None,
            },
            action,
            None,
        ),
        // A failed write keeps what the issue already says.
        Err(error) => (
            IssueRecord {
                number: 0,
                prs: recorded
                    .as_ref()
                    .map(|record| record.prs.clone())
                    .unwrap_or_default(),
                tripped: recorded.as_ref().is_some_and(|record| record.tripped),
                closed: false,
                at: now.to_rfc3339(),
                error: Some(error.clone()),
            },
            "failed",
            Some(error),
        ),
    };
    cause.issue = (record.number > 0).then_some(record.number);
    cause.issue_action = action;
    cause.issue_error = error;
    issues.insert(key, record);
}

/// The issue write itself: the recorded issue, else one an open issue's
/// marker names, else a new one. A recorded issue that is still open gains
/// one comment naming what it does not say yet (new pull requests, a tripped
/// breaker); a closed one is recorded and left alone. Returns the number
/// and what was done.
#[allow(clippy::too_many_arguments)]
fn write_issue(
    run_gh: RunGh<'_>,
    open_issues: &mut OpenIssues,
    repo: &str,
    base: &str,
    class: &Class,
    prs: &[u64],
    filed: Option<&IssueRecord>,
    key: &str,
) -> Result<(u64, &'static str), String> {
    if let Some(record) = filed {
        let known = record.prs.iter().copied().collect::<BTreeSet<_>>();
        let new = class
            .sightings
            .iter()
            .filter(|sighting| !known.contains(&sighting.pr))
            .collect::<Vec<_>>();
        let trip = class.trip.as_ref().filter(|_| !record.tripped);
        if new.is_empty() && trip.is_none() {
            return Ok((record.number, "recorded"));
        }
        let raw = run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/issues/{}", record.number),
        ])?;
        let state = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|value| {
                value
                    .get("state")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .ok_or_else(|| format!("issue #{} state unreadable", record.number))?;
        if state != "open" {
            return Ok((record.number, "closed"));
        }
        return run_gh(&[
            "api".to_owned(),
            "--method".to_owned(),
            "POST".to_owned(),
            format!("repos/{repo}/issues/{}/comments", record.number),
            "-f".to_owned(),
            format!("body={}", update_body(&class.signature, &new, prs, trip)),
        ])
        .map(|_| (record.number, "updated"));
    }
    let listed = open_issues
        .get_or_insert_with(|| {
            run_gh(&crate::marked_issue::list_open_args(repo)).map(|raw| {
                crate::marked_issue::parse(&raw, ISSUE_MARKER)
                    .into_iter()
                    .filter(|issue| marker_on_last_line(&issue.body) == Some(issue.key.as_str()))
                    .collect()
            })
        })
        .as_ref()
        .map_err(|error| format!("open issues could not be listed: {error}"))?;
    // Found by its marker after the ledger lost it: record it as it is,
    // without guessing which pull requests it already names.
    if let Some(issue) = listed.iter().find(|issue| issue.key == key) {
        return Ok((issue.number, "found"));
    }
    let raw = run_gh(&[
        "api".to_owned(),
        "--method".to_owned(),
        "POST".to_owned(),
        format!("repos/{repo}/issues"),
        "-f".to_owned(),
        format!("title={}", issue_title(&class.signature)),
        "-f".to_owned(),
        format!("body={}", issue_body(repo, base, class, key)),
    ])?;
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|value| value.get("number").and_then(Value::as_u64))
        .map(|number| (number, "opened"))
        .ok_or_else(|| "the new issue's number was not returned".to_owned())
}

/// The key of a body whose marker is its last line and appears once. A
/// marker anywhere else (quoted, or carried in by text) is not one.
pub(super) fn marker_on_last_line(body: &str) -> Option<&str> {
    if body.matches(ISSUE_MARKER).count() != 1 {
        return None;
    }
    let key = body
        .trim_end()
        .lines()
        .last()?
        .trim()
        .strip_prefix(ISSUE_MARKER)?
        .strip_suffix("-->")?
        .trim();
    let (repo, digest) = key.split_once('|')?;
    (!repo.is_empty()
        && !repo.contains(char::is_whitespace)
        && digest.len() == 16
        && digest.chars().all(|c| c.is_ascii_hexdigit()))
    .then_some(key)
}

/// A name as an issue may show it: no mention, markup, comment or code
/// delimiters, no control characters, at most [`NAME_CHARS`] characters.
pub(super) fn clean(name: &str) -> String {
    let mut text = name.replace("<!--", "").replace("-->", "");
    text.retain(|c| !matches!(c, '@' | '`' | '<' | '>' | '[' | ']') && !c.is_control());
    let text = text.trim();
    if text.chars().count() > NAME_CHARS {
        text.chars().take(NAME_CHARS - 3).collect::<String>() + "..."
    } else {
        text.to_owned()
    }
}

/// A signature as an issue shows it: cleaned names, at most
/// [`NAMES_SHOWN`] tests.
fn shown(signature: &Signature) -> String {
    let tests = signature
        .tests
        .iter()
        .take(NAMES_SHOWN)
        .map(|test| clean(test))
        .collect::<Vec<_>>()
        .join(", ");
    let more = signature.tests.len().saturating_sub(NAMES_SHOWN);
    format!(
        "{}: {tests}{}",
        signature
            .checks
            .iter()
            .map(|check| clean(check))
            .collect::<Vec<_>>()
            .join(" + "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    )
}

fn issue_title(signature: &Signature) -> String {
    let title = format!(
        "Flaky merge-group test ejects approved PRs: {}",
        shown(signature)
    );
    if title.chars().count() > 200 {
        title.chars().take(197).collect::<String>() + "..."
    } else {
        title
    }
}

fn sighting_lines(sightings: &[&Sighting]) -> String {
    let mut lines = sightings
        .iter()
        .take(LINES_SHOWN)
        .map(|sighting| {
            format!(
                "- #{} at head {}, merge group {}, ejected {}",
                sighting.pr,
                clean(short(&sighting.head)),
                clean(short(&sighting.merge_group)),
                clean(&sighting.at)
            )
        })
        .collect::<Vec<_>>();
    if sightings.len() > LINES_SHOWN {
        lines.push(format!("- and {} more", sightings.len() - LINES_SHOWN));
    }
    lines.join("\n")
}

fn issue_body(repo: &str, base: &str, class: &Class, key: &str) -> String {
    let sightings = class.sightings.iter().collect::<Vec<_>>();
    let trip = class.trip.as_ref().map_or_else(String::new, |trip| {
        format!(
            "\n\n**Breaker tripped:** {}. This signature is not treated as a flake any more.",
            trip.describe()
        )
    });
    format!(
        "The merge queue on {repo} ({base}) ejected approved, green heads of different pull \
requests for the same failing test, from different merge groups, so the Shipyard steward \
classifies it as a flake.\n\n**Signature:** {signature}\n\n{lines}{trip}\n\n\
Each such head is re-enqueued once, unchanged ([auto_merge] flake_requeue). Every batch the \
test fails still costs a rebuild and an ejection: fix or quarantine the test, or list it in \
[auto_merge] known_flaky_tests while it is fixed. Later ejections with the same signature are \
added here as comments.\n\n{ISSUE_MARKER}{key} -->",
        repo = clean(repo),
        base = clean(base),
        signature = shown(&class.signature),
        lines = sighting_lines(&sightings),
    )
}

fn update_body(
    signature: &Signature,
    new: &[&Sighting],
    prs: &[u64],
    trip: Option<&Trip>,
) -> String {
    let mut parts = Vec::new();
    if !new.is_empty() {
        parts.push(format!(
            "The same signature ({}) ejected more pull requests; {} in all.\n\n{}",
            shown(signature),
            prs.len(),
            sighting_lines(new)
        ));
    }
    if let Some(trip) = trip {
        parts.push(format!(
            "**Breaker tripped:** {}. The steward re-enqueues nothing more for this signature.",
            trip.describe()
        ));
    }
    parts.join("\n\n")
}

/// A merge-group reading with no gaps that shows `checks` passing; the
/// base tip is read separately ([`TipVerdict`]).
#[cfg(test)]
pub(super) fn green_base(checks: &[&str]) -> RepoPoison {
    RepoPoison {
        policy: crate::base_health::BasePoisonPolicy::for_base(true),
        health: MergeGroupHealth::Clear {
            detail: "newest merge group passed".to_owned(),
            passed: checks.iter().map(|check| (*check).to_owned()).collect(),
            passed_by: BTreeMap::new(),
        },
        fixes: BTreeMap::new(),
        candidates: BTreeSet::new(),
        fix_read_error: None,
        tip: BTreeMap::new(),
        covering: BTreeSet::new(),
        fix_ambiguity: None,
        red_everywhere: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests;
