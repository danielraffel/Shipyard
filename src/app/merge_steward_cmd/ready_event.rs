//! Ready-on-event admission: when the event that made a pull request
//! `ready` arrives (its exact-head approval posts, or its last required check
//! goes green), run the steward's own admission for that one pull request
//! instead of waiting for the next tick.
//!
//! The event is only a trigger. Nothing in it is trusted: the pull request,
//! its head, its checks and its approval are read again, and the head is
//! admitted through the tick's [`mutate_pr_with_recovery`] (the live
//! exact-head approval gate, the final revalidation and the
//! `expectedHeadOid`-pinned enqueue or merge), under the steward's ledger
//! lock. Anything this path cannot read, or any state it does not own, is
//! left for the tick:
//!
//! - one attempt per head per window (`claim_attempt`), recorded before any
//!   GitHub read;
//! - a head that moved, a pull request that is not open, not on the default
//!   branch, already queued or armed, or that the policy does not classify
//!   as ready, is skipped;
//! - a base with a poison reading other than clear, a paused queue entry, or
//!   a pending run cancellation is skipped;
//! - a ledger lock another pass holds is skipped, never waited on.
//!
//! See `docs/scheduling-loop.md`, "Ready on event".

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::Serialize;
use serde_json::Value;

use super::{
    GitHubActions, MutationApplyContext, MutationControl, RepoObservation, StewardDecision,
    attempts_for, classify_pr, load_ledger, observation, pr_mutations::mutate_pr_with_recovery,
    save_ledger, single_repo_args, try_acquire_ledger_lock,
};
use crate::base_health::merge_group::MergeGroupHealth;

/// The default minimum time between two admission attempts on one head.
pub(crate) const DEFAULT_MIN_INTERVAL: Duration = Duration::from_mins(1);

/// How long an attempt record is kept.
const ATTEMPT_RETENTION_SECS: i64 = 24 * 60 * 60;

/// The pull request an event names, at the head it names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReadyEvent {
    pub(crate) repo: String,
    pub(crate) base: String,
    pub(crate) number: u64,
    pub(crate) head: String,
}

/// What one event did.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ReadyEventOutcome {
    pub(crate) repo: String,
    pub(crate) base: String,
    pub(crate) pr: u64,
    pub(crate) head: String,
    pub(crate) apply: bool,
    /// `admitted`: the steward's admission ran and enqueued or merged the
    /// head. `refused`: it ran and refused the head. `would_admit`: a dry run
    /// found the head ready. `skipped`: nothing was mutated; the tick decides.
    /// `error`: the admission ran and failed.
    pub(crate) outcome: String,
    pub(crate) reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mutation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

impl ReadyEventOutcome {
    fn new(event: &ReadyEvent, apply: bool, outcome: &str, reason: impl Into<String>) -> Self {
        Self {
            repo: event.repo.clone(),
            base: event.base.clone(),
            pr: event.number,
            head: event.head.clone(),
            apply,
            outcome: outcome.to_owned(),
            reason: reason.into(),
            mutation: None,
            error: None,
        }
    }

    fn skipped(event: &ReadyEvent, apply: bool, reason: impl Into<String>) -> Self {
        Self::new(event, apply, "skipped", reason)
    }
}

/// Where the per-head attempt records live: beside the steward ledger.
pub(super) fn attempts_path(ledger_path: &Path) -> PathBuf {
    ledger_path.with_extension("ready-events.json")
}

fn attempt_key(event: &ReadyEvent) -> String {
    format!(
        "{}#{}@{}",
        event.repo.to_ascii_lowercase(),
        event.number,
        event.head.to_ascii_lowercase()
    )
}

/// Record an attempt on `key` unless one was recorded less than `window`
/// ago. `Ok(false)` is a duplicate. An unreadable record file is an error:
/// the caller does nothing rather than risk a second attempt.
pub(super) fn claim_attempt(
    path: &Path,
    key: &str,
    now: DateTime<Utc>,
    window: Duration,
) -> Result<bool, String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let lock_path = path.with_extension("json.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("could not open {}: {error}", lock_path.display()))?;
    lock.lock_exclusive()
        .map_err(|error| format!("could not lock {}: {error}", lock_path.display()))?;
    let _lock = crate::file_lock::LockedFile::new(lock);
    let mut attempts: BTreeMap<String, i64> = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| format!("unreadable {}: {error}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let now_epoch = now.timestamp();
    let window = i64::try_from(window.as_secs()).unwrap_or(i64::MAX);
    if let Some(at) = attempts.get(key)
        && now_epoch.saturating_sub(*at) < window
        && *at <= now_epoch
    {
        return Ok(false);
    }
    attempts.retain(|_, at| now_epoch.saturating_sub(*at) < ATTEMPT_RETENTION_SECS);
    attempts.insert(key.to_owned(), now_epoch);
    crate::durable_file::replace_json(path, &attempts, true)
        .map_err(|error| format!("could not record {}: {error}", path.display()))?;
    Ok(true)
}

/// Read the one pull request the event names, as a one-row observation: the
/// same reads `shipyard wait pr --state actionable` makes. A pull request
/// that is not open, not on `base`, or not on the repository's default
/// branch is an error, so nothing acts on it.
pub(super) fn observe_event_pr(
    actions: &GitHubActions,
    event: &ReadyEvent,
) -> Result<RepoObservation, String> {
    let row = observation::gh_json(
        actions,
        &[
            "pr".to_owned(),
            "view".to_owned(),
            event.number.to_string(),
            "--repo".to_owned(),
            event.repo.clone(),
            "--json".to_owned(),
            observation::PR_VIEW_FIELDS.to_owned(),
        ],
        "pull request",
    )?;
    let settings = observation::gh_json(
        actions,
        &["api".to_owned(), format!("repos/{}", event.repo)],
        "repository settings",
    )?;
    let text = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or_default();
    if !text("state").eq_ignore_ascii_case("OPEN") {
        return Err(format!("not open ({})", text("state")));
    }
    let base = text("baseRefName");
    let default_branch = settings
        .get("default_branch")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !base.eq_ignore_ascii_case(&event.base) || !base.eq_ignore_ascii_case(default_branch) {
        return Err(format!(
            "based on {base:?}, not the default branch {default_branch:?}"
        ));
    }
    if row.get("changedFiles").and_then(Value::as_u64) == Some(0) {
        return Err("empty diff".to_owned());
    }
    observation::observe_pull_request(actions, &settings, base, &row)
}

/// Run the steward's admission for the one pull request `event` names.
///
/// `observe` reads that pull request ([`observe_event_pr`] in production).
/// Without `apply`, nothing is locked, recorded or mutated.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn admit_ready_event(
    actions: &GitHubActions,
    control: &MutationControl,
    ledger_path: &Path,
    event: &ReadyEvent,
    apply: bool,
    min_interval: Duration,
    observe: &dyn Fn() -> Result<RepoObservation, String>,
    now: DateTime<Utc>,
) -> ReadyEventOutcome {
    if !crate::merge_steward::is_full_sha(&event.head) {
        return ReadyEventOutcome::skipped(event, apply, "invalid_head");
    }
    if apply {
        match claim_attempt(
            &attempts_path(ledger_path),
            &attempt_key(event),
            now,
            min_interval,
        ) {
            Ok(true) => {}
            Ok(false) => return ReadyEventOutcome::skipped(event, apply, "duplicate_event"),
            Err(error) => {
                return ReadyEventOutcome::skipped(
                    event,
                    apply,
                    format!("attempts_unread:{error}"),
                );
            }
        }
    }
    // A cheap filter before the lock: most events do not find a ready head,
    // and those never hold the lock the tick needs.
    let args = single_repo_args(&event.repo, &event.base, apply);
    let unlocked_ledger = match load_ledger(ledger_path) {
        Ok(ledger) => ledger,
        Err(error) => {
            return ReadyEventOutcome::skipped(
                event,
                apply,
                format!("ledger_unread:{}", error.message()),
            );
        }
    };
    let observation = match observe() {
        Ok(observation) => observation,
        Err(error) => {
            return ReadyEventOutcome::skipped(event, apply, format!("unread:{error}"));
        }
    };
    let pr = match observation.prs.as_slice() {
        [pr] if pr.fact.number == event.number => pr,
        _ => return ReadyEventOutcome::skipped(event, apply, "unread:no single row"),
    };
    if !pr.fact.head_sha.eq_ignore_ascii_case(&event.head) {
        return ReadyEventOutcome::skipped(
            event,
            apply,
            format!("head_moved:{}", pr.fact.head_sha),
        );
    }
    if pr.fact.queue_position.is_some() || pr.fact.auto_merge_active {
        return ReadyEventOutcome::skipped(event, apply, "already_queued_or_armed");
    }
    let policy = super::cancellation::pass_policy(actions, &args, &observation);
    let decision = classify_pr(
        &pr.fact,
        &policy,
        &attempts_for(&unlocked_ledger, &observation.repo, &pr.fact),
    );
    if decision != StewardDecision::ArmMergeQueue {
        return ReadyEventOutcome::skipped(
            event,
            apply,
            format!("not_ready:{}", decision_name(&decision)),
        );
    }
    // A base the merge groups show poisoned, or that could not be read, is
    // the tick's: it pauses, fast-tracks and restores the queue as a whole.
    let base_poison = super::base_poison::observe(actions, &observation, &policy, now);
    if let Some(poison) = base_poison.as_ref()
        && !matches!(poison.health, MergeGroupHealth::Clear { .. })
    {
        return ReadyEventOutcome::skipped(event, apply, "base_poison_not_clear");
    }
    if !apply {
        let mut outcome = ReadyEventOutcome::new(event, apply, "would_admit", "ready");
        outcome.mutation = Some("would_enqueue".to_owned());
        return outcome;
    }
    // Serialized with the tick and with `admission-clean`: the same lock the
    // tick holds for its whole pass. Busy means a pass is running and will
    // see this head itself.
    let _lock = match try_acquire_ledger_lock(ledger_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => return ReadyEventOutcome::skipped(event, apply, "steward_busy"),
        Err(error) => {
            return ReadyEventOutcome::skipped(
                event,
                apply,
                format!("lock_unavailable:{}", error.message()),
            );
        }
    };
    let mut ledger = match load_ledger(ledger_path) {
        Ok(ledger) => ledger,
        Err(error) => {
            return ReadyEventOutcome::skipped(
                event,
                apply,
                format!("ledger_unread:{}", error.message()),
            );
        }
    };
    if !ledger.pending_cancellations.is_empty() {
        return ReadyEventOutcome::skipped(event, apply, "pending_cancellations");
    }
    // The decision again, on the ledger the lock protects.
    let decision = classify_pr(
        &pr.fact,
        &policy,
        &attempts_for(&ledger, &observation.repo, &pr.fact),
    );
    if decision != StewardDecision::ArmMergeQueue {
        return ReadyEventOutcome::skipped(
            event,
            apply,
            format!("not_ready:{}", decision_name(&decision)),
        );
    }
    if super::poison_pause::waiting_report(&ledger, &observation, pr, &decision).is_some() {
        return ReadyEventOutcome::skipped(event, apply, "poison_paused");
    }
    // The queue-removals ledger, read the way the tick reads it; a head a
    // defect-fix removal blocks is refused inside the admission. The cached
    // last good read is the tick's to update.
    let mut removal_cache = ledger.removal_ledger.clone();
    let defect_fix = super::defect_fix_removals::read(
        &super::defect_fix_removals::ledger_path(),
        &mut removal_cache,
        now,
    );
    let context = MutationApplyContext {
        actions,
        observation: &observation,
        ledger_path,
        mutation_control: control,
        approval_blockers: std::cell::RefCell::default(),
        carried_commits: std::cell::RefCell::default(),
        changed_files: std::cell::RefCell::default(),
        ejection_guards: policy.ejection_guards.clone(),
        base_poison: base_poison.as_ref(),
        defect_fix: Some(&defect_fix),
    };
    let (mutation, error) = mutate_pr_with_recovery(
        &context,
        pr,
        &policy,
        &decision,
        &mut ledger,
        args.recover_hosted_setup_eviction_priority,
    );
    let saved = save_ledger(ledger_path, &ledger).err();
    let admitted = mutation.as_deref().is_some_and(|mutation| {
        mutation
            .split(',')
            .any(|part| part == "enqueued" || part.starts_with("merged_exact_head"))
    });
    // The admission names a refusal with a token (`waiting_head_approval`,
    // `skipped_after_live_revalidation`, ...) and its detail; a failure with
    // no token, or a ledger it could not save, is an error.
    let mut outcome = match (admitted, mutation.as_deref(), &error) {
        (true, _, _) => ReadyEventOutcome::new(event, apply, "admitted", "ready"),
        (false, Some(token), _) => ReadyEventOutcome::new(event, apply, "refused", token),
        (false, None, Some(_)) => ReadyEventOutcome::new(event, apply, "error", "admission failed"),
        (false, None, None) => ReadyEventOutcome::new(event, apply, "refused", "no admission"),
    };
    outcome.mutation = mutation;
    outcome.error = error;
    if let Some(saved) = saved {
        "error".clone_into(&mut outcome.outcome);
        let note = format!("steward ledger not saved: {}", saved.message());
        outcome.error = Some(match outcome.error.take() {
            Some(error) => format!("{error}; {note}"),
            None => note,
        });
    }
    outcome
}

fn decision_name(decision: &StewardDecision) -> String {
    serde_json::to_value(decision)
        .ok()
        .and_then(|value| {
            value
                .get("action")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The arguments of `shipyard runner steward-admit-ready`.
pub(crate) struct AdmitReadyArgs {
    pub(crate) repo: String,
    pub(crate) base: String,
    pub(crate) pr: u64,
    pub(crate) head: String,
    pub(crate) min_interval: Duration,
    pub(crate) apply: bool,
}

/// One event's admission with the production reads and the steward's state
/// under `runtime_paths`: what `runner steward-admit-ready` and the relay
/// run.
pub(crate) fn admit_ready(
    actions: &GitHubActions,
    cwd: &Path,
    mode: crate::identity::RuntimeMode,
    runtime_paths: &crate::paths::RuntimePaths,
    args: &AdmitReadyArgs,
) -> ReadyEventOutcome {
    let event = ReadyEvent {
        repo: args.repo.clone(),
        base: args.base.clone(),
        number: args.pr,
        head: args.head.clone(),
    };
    let control = match crate::ship_state::ShipStateStore::new(runtime_paths.state_dir.join("ship"))
    {
        Ok(store) => MutationControl {
            store,
            cwd: cwd.to_path_buf(),
            mode,
            global_dir: runtime_paths.global_dir.clone(),
            state_dir: runtime_paths.state_dir.clone(),
        },
        Err(error) => {
            return ReadyEventOutcome::skipped(
                &event,
                args.apply,
                format!("state_unavailable:{error}"),
            );
        }
    };
    let scoped = actions.clone().with_repo_override(&args.repo);
    let ledger_path = runtime_paths.state_dir.join("merge-steward.json");
    admit_ready_event(
        &scoped,
        &control,
        &ledger_path,
        &event,
        args.apply,
        args.min_interval,
        &|| observe_event_pr(&scoped, &event),
        Utc::now(),
    )
}

/// `shipyard runner steward-admit-ready`.
pub(crate) fn admit_ready_command<W: std::io::Write>(
    args: &AdmitReadyArgs,
    cwd: &Path,
    mode: crate::identity::RuntimeMode,
    runtime_paths: &crate::paths::RuntimePaths,
    actions: &GitHubActions,
    json_output: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, super::CliFailure> {
    if !crate::merge_steward::is_full_sha(&args.head) {
        return Err(super::CliFailure::new(
            2,
            "--head must be the full 40-character head SHA",
        ));
    }
    let outcome = admit_ready(actions, cwd, mode, runtime_paths, args);
    let io = |error: std::io::Error| super::CliFailure::new(1, error.to_string());
    if json_output {
        let mut data = BTreeMap::new();
        data.insert(
            "admission".to_owned(),
            serde_json::to_value(&outcome).unwrap_or(Value::Null),
        );
        crate::output::write_json_envelope(stdout, "steward-admit-ready", data)
            .map_err(|error| super::CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "{}#{} {} {}: {}{}",
            outcome.repo,
            outcome.pr,
            outcome.head.get(..10).unwrap_or(&outcome.head),
            outcome.outcome,
            outcome.reason,
            outcome
                .error
                .as_deref()
                .map(|error| format!(" ({error})"))
                .unwrap_or_default()
        )
        .map_err(io)?;
    }
    Ok(if outcome.outcome == "error" {
        std::process::ExitCode::from(1)
    } else {
        std::process::ExitCode::SUCCESS
    })
}
