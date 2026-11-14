//! Cancel merge-group runs whose queue branch no longer exists.
//!
//! When the queue is rebuilt (a reorder, a dequeue, an ejection), GitHub
//! deletes the old `gh-readonly-queue/<base>/pr-<n>-<sha>` branch, but a run
//! already started on it keeps its runner until it finishes. On 2026-11-13
//! Vellum run 43410542522 on `gh-readonly-queue/main/pr-408-8c606bc7` kept a
//! macOS GPU slot after the 11:02Z reorder until it was cancelled by hand.
//!
//! With `[base_health] cancel_orphaned_groups = "on"` the steward cancels such
//! a run. Only a run whose event is `merge_group`, that has not completed,
//! whose branch is a queue branch of this base, and whose head is not a
//! current queue entry's group head is a suspect; the ref is then read in the
//! same pass, and only a `404` (confirmed absent) makes it an orphan. A ref
//! that exists or could not be read leaves the run alone and says so. `push`
//! and `pull_request` runs are never touched.
//!
//! Each cancel runs under the run mutation guard and is recorded in
//! `<state>/base-health/orphan-cancels.jsonl`; a run already cancelled is not
//! cancelled again, and a failed cancel is retried next pass. `dry-run`
//! reports `would cancel` and changes nothing.
//!
//! Reads: none unless a suspect exists; then one ref read per suspect branch.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{MutationControl, RepoObservation};
use crate::base_health::PoisonHold;
use crate::merge_steward::StewardRun;

/// Cancel receipts, one JSON line each.
const RECEIPTS_FILE: &str = "base-health/orphan-cancels.jsonl";

/// One cancel decision.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Receipt {
    /// `OWNER/REPO`.
    pub(super) repo: String,
    /// The workflow run.
    pub(super) run_id: u64,
    /// Its queue branch.
    pub(super) branch: String,
    /// Its head.
    pub(super) head_sha: String,
    /// RFC 3339.
    pub(super) at: String,
    /// `cancelled` or `failed: ...`.
    pub(super) outcome: String,
}

/// What the fresh ref read said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RefState {
    /// `404`: the branch is gone.
    Absent,
    /// The branch exists.
    Present,
    /// The read failed some other way.
    Unread(String),
}

/// Classify a ref read: only GitHub's `404` is absence.
pub(super) fn ref_state(result: &Result<String, String>) -> RefState {
    match result {
        Ok(_) => RefState::Present,
        Err(error) if error.to_ascii_lowercase().contains("http 404") => RefState::Absent,
        Err(error) => RefState::Unread(error.clone()),
    }
}

/// Runs that may be orphaned: unfinished `merge_group` runs on this base's
/// queue branches whose head is no current entry's group head.
pub(super) fn suspects(observation: &RepoObservation) -> Vec<&StewardRun> {
    let prefix = format!("gh-readonly-queue/{}/", observation.base);
    let live = observation
        .merge_group_heads
        .values()
        .map(|head| head.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    observation
        .runs
        .iter()
        .filter(|run| run.event == "merge_group")
        .filter(|run| !run.status.eq_ignore_ascii_case("completed"))
        .filter(|run| run.head_branch.starts_with(&prefix))
        .filter(|run| !live.contains(&run.head_sha.to_ascii_lowercase()))
        .collect()
}

fn load(state_dir: &Path) -> Vec<Receipt> {
    fs::read_to_string(state_dir.join(RECEIPTS_FILE))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn append(state_dir: &Path, receipt: &Receipt) -> Result<(), String> {
    let path = state_dir.join(RECEIPTS_FILE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| error.to_string())?;
    writeln!(
        file,
        "{}",
        serde_json::to_string(receipt).unwrap_or_default()
    )
    .map_err(|error| error.to_string())
}

/// The acts a pass can take, kept apart so the decision is testable.
pub(super) trait Acts {
    /// Read the queue branch's ref.
    fn read_ref(&self, branch: &str) -> Result<String, String>;
    /// Cancel the run under the mutation guard; `Ok` once GitHub accepted.
    fn cancel(&self, run: &StewardRun) -> Result<(), String>;
}

/// Decide and, under `on`, act on every suspect. Returns the report lines.
pub(super) fn decide(
    mode: PoisonHold,
    state_dir: Option<&Path>,
    observation: &RepoObservation,
    acts: &dyn Acts,
    now: DateTime<Utc>,
) -> Vec<String> {
    if mode == PoisonHold::Off {
        return Vec::new();
    }
    let receipts = state_dir.map(load).unwrap_or_default();
    let mut lines = Vec::new();
    let mut refs: std::collections::BTreeMap<String, RefState> = std::collections::BTreeMap::new();
    for run in suspects(observation) {
        if receipts
            .iter()
            .any(|receipt| receipt.run_id == run.id && receipt.outcome == "cancelled")
        {
            lines.push(format!(
                "orphan_cancel: run {} on {} already cancelled",
                run.id, run.head_branch
            ));
            continue;
        }
        let state = refs
            .entry(run.head_branch.clone())
            .or_insert_with(|| ref_state(&acts.read_ref(&run.head_branch)))
            .clone();
        match state {
            RefState::Present => continue,
            RefState::Unread(error) => {
                lines.push(format!(
                    "orphan_cancel: run {} on {}: ref unread, left running: {error}",
                    run.id, run.head_branch
                ));
                continue;
            }
            RefState::Absent => {}
        }
        let (true, Some(state_dir)) = (mode == PoisonHold::On, state_dir) else {
            lines.push(format!(
                "orphan_cancel: would cancel run {} ({}) on {}: its queue branch is gone",
                run.id, run.workflow, run.head_branch
            ));
            continue;
        };
        let outcome = match acts.cancel(run) {
            Ok(()) => "cancelled".to_owned(),
            Err(error) => format!("failed: {error}"),
        };
        let receipt = Receipt {
            repo: observation.repo.clone(),
            run_id: run.id,
            branch: run.head_branch.clone(),
            head_sha: run.head_sha.clone(),
            at: now.to_rfc3339(),
            outcome: outcome.clone(),
        };
        let recorded = append(state_dir, &receipt)
            .err()
            .map_or_else(String::new, |error| {
                format!(" (receipt not recorded: {error})")
            });
        lines.push(format!(
            "orphan_cancel: run {} ({}) on {}: {outcome}{recorded}",
            run.id, run.workflow, run.head_branch
        ));
    }
    lines
}

struct GitHubActs<'a> {
    actions: &'a crate::cloud::GitHubActions,
    control: Option<&'a MutationControl>,
    observation: &'a RepoObservation,
}

impl Acts for GitHubActs<'_> {
    fn read_ref(&self, branch: &str) -> Result<String, String> {
        self.actions
            .run_gh(&[
                "api".to_owned(),
                format!("repos/{}/git/ref/heads/{branch}", self.observation.repo),
            ])
            .map_err(|error| error.to_string())
    }

    fn cancel(&self, run: &StewardRun) -> Result<(), String> {
        let control = self
            .control
            .ok_or_else(|| "cancel requires mutation control".to_owned())?;
        let guard = super::acquire_run_mutation_guard(
            control,
            self.observation,
            run,
            "runner steward cancel a merge-group run whose queue branch is gone",
        )?;
        let result = self
            .actions
            .cancel_workflow_run(&self.observation.repo, run.id)
            .map_err(|error| error.to_string());
        let (token, reason) = match &result {
            Ok(()) => (
                "orphan_merge_group_cancelled",
                "queue branch absent".to_owned(),
            ),
            Err(error) => ("orphan_merge_group_cancel_failed", error.clone()),
        };
        guard
            .finish_with_reason(token, &reason)
            .map_err(|audit| format!("mutation audit failed: {audit}"))?;
        result
    }
}

/// The pass step.
pub(super) fn run(
    mode: PoisonHold,
    actions: &crate::cloud::GitHubActions,
    control: Option<&MutationControl>,
    apply: bool,
    observation: &RepoObservation,
) -> Vec<String> {
    if !observation.merge_queue {
        return Vec::new();
    }
    // A pass that does not apply never writes or cancels.
    let mode = if apply { mode } else { mode.min_dry_run() };
    decide(
        mode,
        if apply {
            control.map(|control| control.state_dir.as_path())
        } else {
            None
        },
        observation,
        &GitHubActs {
            actions,
            control,
            observation,
        },
        Utc::now(),
    )
}
