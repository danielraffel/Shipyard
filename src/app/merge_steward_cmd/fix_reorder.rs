//! Move a base fix that is already queued behind other entries to the front.
//!
//! [`super::base_poison`] enqueues the fix with `jump: true` when it is not
//! queued yet. A fix that was enqueued before the episode was confirmed (or
//! by hand) sits behind entries whose groups cannot pass. With
//! `[base_health] reorder_fix = "on"` the steward moves it with the same
//! steps `shipyard base-health --act` takes: dequeue, then enqueue its exact
//! head with `jump: true`, with a plain re-enqueue if the jump is refused
//! ([`crate::app::base_health_cmd::jump`]).
//!
//! It acts only while a context is confirmed red
//! ([`super::base_poison::RepoPoison::confirmed_contexts`]), only on a pass
//! that read everything (no `partial`), and only on the one fix the pass
//! named when that fix passed every confirmed context after the break
//! ([`super::base_poison::RepoPoison::covering`], the same set admission
//! uses). An entry at position 1 is left alone: its group
//! is the one already building. The jump is bound to the repository, the
//! base and the head the census read; a head that moved or a different base
//! is refused before anything changes, and the jump re-reads the exact-head
//! approval. One decision per episode (context and
//! the time the base went red) is recorded in
//! `<state>/base-health/fix-reorders.jsonl`; a failed attempt is recorded as
//! failed and retried by the next pass. `dry-run` records `would_reorder`
//! once per episode and changes nothing.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::base_poison::{RepoPoison, confirmed_red_since};
use super::{MutationControl, RepoObservation};
use crate::base_health::{JumpDecision, PoisonHold, already_recorded};

/// Decisions, one JSON line each, under the state directory.
const DECISIONS_FILE: &str = "base-health/fix-reorders.jsonl";

/// A queued fix the pass would move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Candidate {
    /// The fix.
    pub(super) pr: u64,
    /// The head the census read.
    pub(super) head: String,
    /// Its queue position (2 or later).
    pub(super) position: u64,
    /// The confirmed context.
    pub(super) context: String,
    /// The episode key: context and when the base went red.
    pub(super) episode: String,
}

/// The queued fix to move, or `Err` with the line that says why not, or
/// `Ok(None)` when there is nothing to say.
pub(super) fn candidate(
    poison: &RepoPoison,
    observation: &RepoObservation,
) -> Result<Option<Candidate>, String> {
    if poison.policy.acts.reorder_fix == PoisonHold::Off {
        return Ok(None);
    }
    let confirmed = confirmed_red_since(&poison.health, &poison.tip, &poison.red_everywhere);
    let Some((context, since)) = confirmed.iter().next() else {
        return Ok(None);
    };
    let Some(number) = poison.fixes.keys().next().copied() else {
        return Ok(None);
    };
    // A pass that could not read everything decides nothing that reorders.
    if let Some(gap) = poison.partial() {
        return Err(format!("#{number}: partial read ({gap}); not reordered"));
    }
    // The same rule as admission: only a fix that passed every confirmed
    // context after the break is moved; a fix for one context would form a
    // group that still fails another.
    if !poison.covering.contains(&number) {
        return Err(format!(
            "#{number}: does not pass every confirmed context; not reordered"
        ));
    }
    let Some(pr) = observation.prs.iter().find(|pr| pr.fact.number == number) else {
        return Err(format!("#{number}: not open on {}", observation.base));
    };
    match pr.fact.queue_position {
        None => Ok(None),
        Some(1) => Err(format!(
            "#{number}: already first; its group is the one building"
        )),
        Some(position) => Ok(Some(Candidate {
            pr: number,
            head: pr.fact.head_sha.clone(),
            position,
            context: context.clone(),
            episode: format!("{context}@{}", since.to_rfc3339()),
        })),
    }
}

fn decisions_path(state_dir: &Path) -> PathBuf {
    state_dir.join(DECISIONS_FILE)
}

/// Every recorded decision.
pub(super) fn load(state_dir: &Path) -> Vec<JumpDecision> {
    fs::read_to_string(decisions_path(state_dir))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn append(state_dir: &Path, decision: &JumpDecision) -> Result<(), String> {
    let path = decisions_path(state_dir);
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
        serde_json::to_string(decision).unwrap_or_default()
    )
    .map_err(|error| error.to_string())
}

/// What one pass does: the decision for `candidate`, made by `act` under
/// `on` and recorded once per episode. `act` returns the jump's outcome.
pub(super) fn decide(
    state_dir: &Path,
    repo: &str,
    mode: PoisonHold,
    candidate: &Candidate,
    now: DateTime<Utc>,
    act: impl FnOnce() -> String,
) -> String {
    let mut decision = JumpDecision {
        decided_at: now.to_rfc3339(),
        repo: repo.to_owned(),
        mode: if mode == PoisonHold::On {
            "on"
        } else {
            "dry_run"
        }
        .to_owned(),
        pr: candidate.pr,
        tests: vec![candidate.context.clone()],
        signal_run_id: 0,
        main_run_id: Some(candidate.episode.clone()),
        outcome: format!("would_reorder from position {}", candidate.position),
    };
    if already_recorded(&load(state_dir), &decision) {
        return format!(
            "#{} already decided for `{}` this episode",
            candidate.pr, candidate.context
        );
    }
    if mode == PoisonHold::On {
        decision.outcome = act();
    }
    let recorded = append(state_dir, &decision)
        .err()
        .map_or_else(String::new, |error| {
            format!(" (decision not recorded: {error})")
        });
    format!(
        "#{} from position {} (`{}` red): {}{recorded}",
        candidate.pr, candidate.position, candidate.context, decision.outcome
    )
}

/// The pass step. Returns the report lines, each prefixed `fix_reorder:`.
pub(super) fn run(
    actions: &crate::cloud::GitHubActions,
    control: Option<&MutationControl>,
    apply: bool,
    observation: &RepoObservation,
    poison: &RepoPoison,
) -> Vec<String> {
    let line = |text: String| vec![format!("fix_reorder: {text}")];
    let candidate = match candidate(poison, observation) {
        Ok(Some(candidate)) => candidate,
        Ok(None) => return Vec::new(),
        Err(why) => return line(why),
    };
    let mode = poison.policy.acts.reorder_fix;
    // A pass that does not apply records nothing and changes nothing.
    let (true, Some(control)) = (apply, control) else {
        return line(format!(
            "#{} would reorder from position {} (`{}` red; pass does not apply)",
            candidate.pr, candidate.position, candidate.context
        ));
    };
    line(decide(
        &control.state_dir,
        &observation.repo,
        mode,
        &candidate,
        Utc::now(),
        || {
            let Some(pr) = observation
                .prs
                .iter()
                .find(|pr| pr.fact.number == candidate.pr)
            else {
                return "failed: not open".to_owned();
            };
            let guard = match super::acquire_pr_mutation_guard(
                control,
                observation,
                pr,
                "runner steward move the base fix to the front of the merge queue",
            ) {
                Ok(guard) => guard,
                Err(error) => return format!("failed: mutation guard: {error}"),
            };
            let outcome = crate::app::base_health_cmd::jump(
                actions,
                &observation.repo,
                candidate.pr,
                Some(crate::app::base_health_cmd::Expected {
                    head: &candidate.head,
                    base: &observation.base,
                }),
            );
            let token = if outcome == "jumped" {
                "base_fix_reordered"
            } else {
                "base_fix_reorder_not_done"
            };
            match guard.finish_with_reason(token, &outcome) {
                Ok(()) => outcome,
                Err(audit) => format!("{outcome}; mutation audit failed: {audit}"),
            }
        },
    ))
}
