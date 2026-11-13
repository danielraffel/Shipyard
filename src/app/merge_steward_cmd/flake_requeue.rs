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
//! * no signature check is a context the base itself is confirmed red on
//!   (that is `base_poison`'s case, not a flake);
//! * no earlier flake re-enqueue of this exact head is on record. The receipt
//!   is saved before the enqueue, so a crash cannot buy a second one.
//!
//! The re-enqueue is separate from the classifier's one free retry: a head
//! the classifier refused still gets it. It does not lift the head-approval
//! ejection cap ([`crate::head_approval::EJECTION_CAP`]); a head ejected
//! again after its flake re-enqueue is left for repair.
//!
//! ## The root cause
//!
//! Each flake signature is reported in the tick as a `flake_root_cause`
//! record (signature, checks, tests, pull requests, ejection window, issue).
//! Under `apply` the steward also files one issue per signature, found again
//! by a marker in its body: a new pull request joining the signature adds a
//! comment to the same issue instead of a second issue. A failed or refused
//! write is reported and retried after [`ISSUE_RETRY_HOURS`], never routed
//! around.
//!
//! Mode: `[auto_merge] flake_requeue` ([`crate::merge_steward::flake_requeue`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::StewardLedger;
use crate::environment_requeue::{EnvironmentRequeue, FailureAttribution};
use crate::merge_steward::flake_requeue::{FlakeMode, FlakePolicy};

/// How long a sighting is kept after the steward last saw its head ejected.
pub(super) const RETENTION_DAYS: i64 = 7;
/// How long a re-enqueue receipt is kept: longer than any head waits.
pub(super) const RECEIPT_DAYS: i64 = 30;
/// How long after a failed issue write the next attempt waits.
pub(super) const ISSUE_RETRY_HOURS: i64 = 6;
/// The marker that finds a flake issue again.
pub(super) const ISSUE_MARKER: &str = "<!-- shipyard-flake-signature: ";

/// What the steward ledger keeps between passes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct State {
    /// Ejections the classifier read a signature for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) sightings: Vec<Sighting>,
    /// Flake re-enqueues taken, keyed `repo#pr@head`: one per head, ever.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) requeued: BTreeMap<String, Receipt>,
    /// The root-cause issue per signature, keyed [`issue_key`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) issues: BTreeMap<String, IssueRecord>,
    /// This pass's decisions, reported and dropped. Never persisted.
    #[serde(skip)]
    pub(super) pass: Vec<Decision>,
}

impl State {
    /// Whether there is nothing to persist.
    pub(super) fn is_empty(&self) -> bool {
        self.sightings.is_empty() && self.requeued.is_empty() && self.issues.is_empty()
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

/// One flake re-enqueue.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Receipt {
    pub(super) signature: String,
    /// The ejection it answered (RFC 3339).
    pub(super) ejection_at: String,
    /// When it was taken (RFC 3339).
    pub(super) at: String,
}

/// The root-cause issue of one signature.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct IssueRecord {
    /// The issue number; 0 after a failed write.
    pub(super) number: u64,
    /// The pull requests the issue names.
    #[serde(default)]
    pub(super) prs: Vec<u64>,
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
    /// `requeued`, `would_requeue`, `already_requeued`, `defect_fix_removed`,
    /// `base_red` or `unrecorded`.
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
    /// The issue, when one is filed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) issue: Option<u64>,
    /// `opened`, `found`, `updated`, `recorded`, `would_file`, `retry_later`
    /// or `failed`.
    pub(super) issue_action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) issue_error: Option<String>,
}

/// The repository report section.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct RepoStatus {
    pub(super) mode: String,
    pub(super) window_hours: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) decisions: Vec<Decision>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) flake_root_cause: Vec<RootCause>,
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
    let mut by_check: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for check in &verdict.attribution {
        let steps = verdict
            .evidence
            .iter()
            .filter(|step| step.check == check.check)
            .collect::<Vec<_>>();
        if steps.is_empty() || steps.iter().any(|step| step.failed_tests.is_empty()) {
            return None;
        }
        by_check.entry(check.check.as_str()).or_default().extend(
            steps
                .iter()
                .flat_map(|step| step.failed_tests.iter())
                .map(String::as_str),
        );
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

fn time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

pub(super) fn receipt_key(repo: &str, pr: u64, head: &str) -> String {
    format!(
        "{}#{pr}@{}",
        repo.to_ascii_lowercase(),
        head.to_ascii_lowercase()
    )
}

/// The issue key of one signature: the repository and a digest of the
/// signature, so the marker never carries a test name.
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
    /// Contexts the base is confirmed red on.
    pub(super) base_red: &'a [String],
}

/// What the enqueue path should do with a flake-classified ejection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Re-enqueue the head now; the receipt is saved.
    Requeue(String),
    /// Keep the refusal, with this added to the retry detail.
    Keep(String),
}

/// Record the ejection and classify it. `None` when the flake class is off,
/// the classifier already allows a retry, or the ejection has no signature
/// or no corroborating pull request (a single ejection is never a flake).
///
/// Under `apply`, an eligible head's receipt is recorded and the ledger
/// saved to `ledger_path` before this returns [`Verdict::Requeue`]; a save
/// that fails keeps the refusal.
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
    let window = Duration::hours(i64::from(ejection.policy.window_hours));
    let with = corroborating(&ledger.flake_requeue, &this, window);
    if with.is_empty() {
        return None;
    }
    let class = format!("flake:{}", this.signature.text);
    let key = receipt_key(ejection.repo, ejection.pr, ejection.head);
    let (mut outcome, detail) = decide(ejection, &ledger.flake_requeue, &this, &with, &key);
    let verdict = if outcome == "requeued" {
        ledger.flake_requeue.requeued.insert(
            key.clone(),
            Receipt {
                signature: this.signature.text.clone(),
                ejection_at: this.at.clone(),
                at: now.to_rfc3339(),
            },
        );
        let saved = ledger_path.map_or(Ok(()), |path| {
            super::ledger::save_ledger(path, ledger).map_err(|error| error.message)
        });
        match saved {
            Ok(()) => Verdict::Requeue(detail),
            Err(error) => {
                ledger.flake_requeue.requeued.remove(&key);
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

/// What to do with a corroborated flake ejection: `(outcome, detail)`.
/// `requeued` is the only outcome that re-enqueues.
fn decide(
    ejection: &Ejection<'_>,
    state: &State,
    this: &Sighting,
    with: &[u64],
    key: &str,
) -> (&'static str, String) {
    let class = format!("flake:{}", this.signature.text);
    let others = with
        .iter()
        .map(|pr| format!("#{pr}"))
        .collect::<Vec<_>>()
        .join(", ");
    if ejection.defect_fix_removed {
        return (
            "defect_fix_removed",
            format!("{class}: a defect-fix removal names this head, so it is not re-enqueued"),
        );
    }
    if let Some(context) = ejection
        .base_red
        .iter()
        .find(|context| this.signature.checks.contains(context))
    {
        return (
            "base_red",
            format!(
                "{class}: the base is confirmed red on `{context}`, which is the base's \
                 failure, not a flake; not re-enqueued"
            ),
        );
    }
    if let Some(receipt) = state.requeued.get(key) {
        return (
            "already_requeued",
            format!(
                "{class}: this head's one flake re-enqueue was spent at {}",
                receipt.at
            ),
        );
    }
    if ejection.policy.mode == FlakeMode::Shadow {
        return (
            "would_requeue",
            format!(
                "{class}: the same test ejected {others} too; would re-enqueue this head once \
                 (flake_requeue = \"shadow\")"
            ),
        );
    }
    (
        "requeued",
        format!(
            "{class}: the same test ejected {others} too, so this unchanged, approved, green \
             head is re-enqueued once"
        ),
    )
}

fn prune(state: &mut State, now: DateTime<Utc>) {
    let seen_floor = (now - Duration::days(RETENTION_DAYS)).timestamp();
    state
        .sightings
        .retain(|sighting| sighting.seen_epoch >= seen_floor);
    let receipt_floor = now - Duration::days(RECEIPT_DAYS);
    state
        .requeued
        .retain(|_, receipt| time(&receipt.at).is_some_and(|at| at >= receipt_floor));
    state
        .issues
        .retain(|_, issue| time(&issue.at).is_some_and(|at| at >= receipt_floor));
}

/// One flake signature in this repository: its sightings, every one
/// corroborated by another pull request's.
struct Class {
    signature: Signature,
    sightings: Vec<Sighting>,
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
        decisions,
        flake_root_cause: Vec::new(),
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

/// Open issues carrying [`ISSUE_MARKER`], listed at most once per pass.
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
    if let Some(record) = &recorded
        && record.number == 0
        && time(&record.at).is_some_and(|at| now - at < Duration::hours(ISSUE_RETRY_HOURS))
    {
        cause.issue_action = "retry_later";
        cause.issue_error.clone_from(&record.error);
        return;
    }
    let filed = recorded.as_ref().filter(|record| record.number > 0);
    let result = write_issue(run_gh, open_issues, repo, base, class, prs, filed, &key);
    let (number, action, error) = match result {
        Ok((number, action)) => (number, action, None),
        Err(error) => (0, "failed", Some(error)),
    };
    issues.insert(
        key,
        IssueRecord {
            number,
            // A failed write keeps the pull requests the issue already names.
            prs: if number > 0 {
                prs.to_vec()
            } else {
                recorded.map(|record| record.prs).unwrap_or_default()
            },
            at: now.to_rfc3339(),
            error: error.clone(),
        },
    );
    cause.issue = (number > 0).then_some(number);
    cause.issue_action = action;
    cause.issue_error = error;
}

/// The issue write itself: the recorded issue, else one an open issue's
/// marker names, else a new one. A recorded issue gains one comment naming
/// the pull requests it did not name yet. Returns the number and what was
/// done.
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
        if new.is_empty() {
            return Ok((record.number, "recorded"));
        }
        return run_gh(&[
            "api".to_owned(),
            "--method".to_owned(),
            "POST".to_owned(),
            format!("repos/{repo}/issues/{}/comments", record.number),
            "-f".to_owned(),
            format!("body={}", update_body(&class.signature, &new, prs)),
        ])
        .map(|_| (record.number, "updated"));
    }
    let listed = open_issues
        .get_or_insert_with(|| {
            run_gh(&crate::marked_issue::list_open_args(repo))
                .map(|raw| crate::marked_issue::parse(&raw, ISSUE_MARKER))
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

fn issue_title(signature: &Signature) -> String {
    let mut title = format!(
        "Flaky merge-group test ejects approved PRs: {}",
        signature.text
    );
    if title.chars().count() > 200 {
        title = title.chars().take(197).collect::<String>() + "...";
    }
    title
}

fn sighting_line(sighting: &Sighting) -> String {
    format!(
        "- #{} at head `{}`, merge group `{}`, ejected {}",
        sighting.pr,
        sighting.head.get(..12).unwrap_or(&sighting.head),
        sighting
            .merge_group
            .get(..12)
            .unwrap_or(&sighting.merge_group),
        sighting.at
    )
}

fn issue_body(repo: &str, base: &str, class: &Class, key: &str) -> String {
    let lines = class
        .sightings
        .iter()
        .map(sighting_line)
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The merge queue on `{repo}` (`{base}`) ejected approved, green heads of different pull \
requests for the same failing test, from different merge groups, so the Shipyard steward \
classifies it as a flake.\n\n**Signature:** `{signature}`\n\n{lines}\n\n\
Each such head is re-enqueued once, unchanged (`[auto_merge] flake_requeue`). Every batch the \
test fails still costs a rebuild and an ejection: fix or quarantine the test, or list it in \
`[auto_merge] known_flaky_tests` while it is fixed. Later ejections with the same signature are \
added here as comments.\n\n{ISSUE_MARKER}{key} -->",
        signature = class.signature.text.replace('`', "'"),
    )
}

fn update_body(signature: &Signature, new: &[&Sighting], prs: &[u64]) -> String {
    format!(
        "The same signature (`{}`) ejected more pull requests; {} in all.\n\n{}",
        signature.text.replace('`', "'"),
        prs.len(),
        new.iter()
            .map(|sighting| sighting_line(sighting))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

#[cfg(test)]
mod tests;
