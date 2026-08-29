//! Exact-head steward decisions projected into the native lifecycle.
//!
//! This is the only producer allowed to turn an inert managed publication into
//! a continuation wake. The `actionable -> dispatching + outbox` boundary is
//! restart-completable and the latter transition is one `SQLite` transaction.

use serde::Serialize;

use super::{
    LifecycleState, OptionalExtension, WakeIntent, WorkLedger, WorkLedgerError, WorkLedgerResult,
    params,
};

/// A zero-model steward disposition for one exact managed PR head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Terminal dispositions are fed by the close/reconcile lane after integration.
pub(crate) enum NativeStewardDisposition {
    /// Required evidence is still pending.
    Waiting,
    /// Required evidence passed; deterministic merge stewardship continues.
    Passing,
    /// A semantic repair requires an agent.
    Actionable,
    /// The PR merged and no continuation is needed.
    Merged,
    /// The managed head was superseded or otherwise terminal.
    Superseded,
    /// Live GitHub evidence no longer describes the immutable managed head.
    StaleHead,
}

/// Stable result of applying one disposition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct NativeStewardApplyReport {
    pub(crate) matched: bool,
    pub(crate) changed: bool,
    pub(crate) wake_enqueued: bool,
    pub(crate) phase: Option<String>,
}

#[derive(Clone, Debug)]
struct NativeWork {
    id: String,
    phase: String,
    work_generation: u64,
    owner_generation: u64,
    route_ref: String,
    profile_digest: String,
}

impl WorkLedger {
    /// Enumerate durable native handoffs that still require steward
    /// reconciliation. Unlike the GitHub shadow projection, this recovery
    /// inventory does not depend on repository polling policy being present.
    pub(crate) fn native_steward_targets(&self) -> WorkLedgerResult<Vec<(String, u64, String)>> {
        let connection = self.connect_read_only()?;
        let mut statement = connection.prepare(
            "SELECT lower(repo), pr, lower(head_sha) FROM work_items
              WHERE kind = 'terminal_handoff'
                AND phase IN ('managed', 'waiting', 'actionable', 'dispatching',
                              'agent_owned_repair', 'returned')
                AND pr IS NOT NULL AND pr > 0 AND head_sha IS NOT NULL
              ORDER BY lower(repo), pr, lower(head_sha)",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Apply a classified steward observation to one exact native publication.
    pub(crate) fn apply_native_steward_disposition(
        &self,
        repo: &str,
        pr: u64,
        head_sha: &str,
        disposition: NativeStewardDisposition,
    ) -> WorkLedgerResult<NativeStewardApplyReport> {
        self.apply_native_steward_disposition_with(repo, pr, head_sha, disposition, |_| {})
    }

    #[allow(clippy::too_many_lines)] // Kept together so every lifecycle arm shares one reload fence.
    fn apply_native_steward_disposition_with<F>(
        &self,
        repo: &str,
        pr: u64,
        head_sha: &str,
        disposition: NativeStewardDisposition,
        mut crash_hook: F,
    ) -> WorkLedgerResult<NativeStewardApplyReport>
    where
        F: FnMut(&str),
    {
        validate_target(repo, pr, head_sha)?;
        let Some(mut work) = self.native_work_for_target(repo, pr, head_sha)? else {
            return Ok(NativeStewardApplyReport {
                matched: false,
                changed: false,
                wake_enqueued: false,
                phase: None,
            });
        };
        let mut changed = false;
        let mut wake_enqueued = false;

        match disposition {
            // Waiting/passing observations are deliberately state-preserving.
            // Advancing a generation here would invalidate the staged exact
            // route before a later actionable failure could consume it.
            NativeStewardDisposition::Waiting | NativeStewardDisposition::Passing => {}
            NativeStewardDisposition::Merged
            | NativeStewardDisposition::Superseded
            | NativeStewardDisposition::StaleHead => {
                if matches!(
                    work.phase.as_str(),
                    "managed"
                        | "waiting"
                        | "actionable"
                        | "dispatching"
                        | "agent_owned_repair"
                        | "returned"
                ) {
                    let advanced = self.transition_with_wake(
                        &work.id,
                        work.work_generation,
                        work.owner_generation,
                        LifecycleState::Terminal,
                        None,
                    )?;
                    changed = advanced;
                    work = self
                        .native_work_for_target(repo, pr, head_sha)?
                        .ok_or_else(|| {
                            WorkLedgerError::Refused(
                                "native work disappeared after terminal transition".to_owned(),
                            )
                        })?;
                }
            }
            NativeStewardDisposition::Actionable => {
                if matches!(work.phase.as_str(), "managed" | "waiting") {
                    let transition = self.transition_with_wake(
                        &work.id,
                        work.work_generation,
                        work.owner_generation,
                        LifecycleState::Actionable,
                        None,
                    );
                    if transition.is_ok() {
                        changed = true;
                        crash_hook("after_actionable");
                    }
                    work = self
                        .native_work_for_target(repo, pr, head_sha)?
                        .ok_or_else(|| {
                            WorkLedgerError::Refused(
                                "native work disappeared after actionable transition".to_owned(),
                            )
                        })?;
                    if transition.is_err()
                        && !matches!(
                            work.phase.as_str(),
                            "actionable"
                                | "dispatching"
                                | "agent_owned_repair"
                                | "returned"
                                | "terminal"
                        )
                    {
                        return transition.map(|_| unreachable!());
                    }
                }
                if work.phase == LifecycleState::Actionable.as_str() {
                    let wake = WakeIntent::new(
                        &work.id,
                        work.work_generation + 1,
                        work.owner_generation,
                        work.route_ref.clone(),
                        work.profile_digest.clone(),
                    )?;
                    let transition = self.transition_with_wake(
                        &work.id,
                        work.work_generation,
                        work.owner_generation,
                        LifecycleState::Dispatching,
                        Some(&wake),
                    );
                    if transition.is_ok() {
                        changed = true;
                        wake_enqueued = true;
                        crash_hook("after_dispatching");
                    }
                    work = self
                        .native_work_for_target(repo, pr, head_sha)?
                        .ok_or_else(|| {
                            WorkLedgerError::Refused(
                                "native work disappeared after dispatch transition".to_owned(),
                            )
                        })?;
                    if transition.is_err()
                        && !matches!(
                            work.phase.as_str(),
                            "dispatching" | "agent_owned_repair" | "returned" | "terminal"
                        )
                    {
                        return transition.map(|_| unreachable!());
                    }
                }
            }
        }

        Ok(NativeStewardApplyReport {
            matched: true,
            changed,
            wake_enqueued,
            phase: Some(work.phase),
        })
    }

    fn native_work_for_target(
        &self,
        repo: &str,
        pr: u64,
        head_sha: &str,
    ) -> WorkLedgerResult<Option<NativeWork>> {
        let connection = self.connect_read_only()?;
        connection
            .query_row(
                "SELECT work.id, work.phase, work.work_generation,
                        work.owner_generation, work.repair_route_ref,
                        object.content_digest
                   FROM work_items work
                   JOIN protected_objects object
                     ON object.work_item_id = work.id
                    AND object.kind = 'launch_profile'
                  WHERE work.kind = 'terminal_handoff'
                    AND lower(work.repo) = ?1 AND work.pr = ?2
                    AND lower(work.head_sha) = ?3
                  LIMIT 1",
                params![repo, pr, head_sha],
                |row| {
                    Ok(NativeWork {
                        id: row.get(0)?,
                        phase: row.get(1)?,
                        work_generation: row.get(2)?,
                        owner_generation: row.get(3)?,
                        route_ref: row.get(4)?,
                        profile_digest: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }
}

fn validate_target(repo: &str, pr: u64, head_sha: &str) -> WorkLedgerResult<()> {
    let canonical_repo =
        repo == repo.trim() && repo == repo.to_ascii_lowercase() && repo.split('/').count() == 2;
    let exact_head = head_sha.len() == 40
        && head_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if pr == 0 || !canonical_repo || !exact_head {
        return Err(WorkLedgerError::Refused(
            "native steward target is not canonical exact-head authority".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::work_ledger::native_publication::tests::{policy, request};

    fn published() -> (tempfile::TempDir, WorkLedger, String, String) {
        let state = tempfile::tempdir().expect("state");
        let request = request();
        let report = WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &request,
            &policy(vec![request.repository.clone()]),
            true,
        )
        .expect("publish");
        let ledger = WorkLedger::open_existing(state.path())
            .expect("open")
            .expect("ledger");
        (state, ledger, request.repository, report.wake_id)
    }

    fn counts(ledger: &WorkLedger) -> (String, u64) {
        ledger
            .connect_read_only()
            .expect("connection")
            .query_row(
                "SELECT work.phase, (SELECT COUNT(*) FROM outbox)
                   FROM work_items work WHERE work.kind = 'terminal_handoff'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("counts")
    }

    #[test]
    fn publication_and_non_actionable_observations_create_no_wake() {
        let (_state, ledger, repo, _wake) = published();
        assert_eq!(counts(&ledger), ("managed".to_owned(), 0));
        for disposition in [
            NativeStewardDisposition::Waiting,
            NativeStewardDisposition::Passing,
            NativeStewardDisposition::StaleHead,
        ] {
            ledger
                .apply_native_steward_disposition(&repo, 43, &"a".repeat(40), disposition)
                .expect("non-actionable");
            assert_eq!(counts(&ledger).1, 0);
        }
    }

    #[test]
    fn exact_actionable_transition_enqueues_once_and_restarts_after_crash() {
        let (_state, ledger, repo, expected_wake) = published();
        let crashed = catch_unwind(AssertUnwindSafe(|| {
            ledger
                .apply_native_steward_disposition_with(
                    &repo,
                    43,
                    &"a".repeat(40),
                    NativeStewardDisposition::Actionable,
                    |point| assert_ne!(point, "after_actionable", "crash"),
                )
                .ok();
        }));
        assert!(crashed.is_err());
        assert_eq!(counts(&ledger), ("actionable".to_owned(), 0));

        let applied = ledger
            .apply_native_steward_disposition(
                &repo,
                43,
                &"a".repeat(40),
                NativeStewardDisposition::Actionable,
            )
            .expect("resume");
        assert!(applied.wake_enqueued);
        assert_eq!(counts(&ledger), ("dispatching".to_owned(), 1));
        let wake: String = ledger
            .connect_read_only()
            .expect("connection")
            .query_row("SELECT wake_id FROM outbox", [], |row| row.get(0))
            .expect("wake");
        assert_eq!(wake, expected_wake);

        let replay = ledger
            .apply_native_steward_disposition(
                &repo,
                43,
                &"a".repeat(40),
                NativeStewardDisposition::Actionable,
            )
            .expect("replay");
        assert!(!replay.changed);
        assert!(!replay.wake_enqueued);
        assert_eq!(counts(&ledger).1, 1);
    }

    #[test]
    fn terminal_and_stale_head_paths_never_enqueue() {
        for disposition in [
            NativeStewardDisposition::Merged,
            NativeStewardDisposition::Superseded,
            NativeStewardDisposition::StaleHead,
        ] {
            let (_state, ledger, repo, _wake) = published();
            ledger
                .apply_native_steward_disposition(&repo, 43, &"a".repeat(40), disposition)
                .expect("terminal/no-op");
            assert_eq!(counts(&ledger).1, 0);
        }
    }

    #[test]
    fn stale_head_atomically_suppresses_an_unclaimed_actionable_wake() {
        let (_state, ledger, repo, _wake) = published();
        ledger
            .apply_native_steward_disposition(
                &repo,
                43,
                &"a".repeat(40),
                NativeStewardDisposition::Actionable,
            )
            .expect("actionable");
        assert_eq!(counts(&ledger), ("dispatching".to_owned(), 1));

        let stale = ledger
            .apply_native_steward_disposition(
                &repo,
                43,
                &"a".repeat(40),
                NativeStewardDisposition::StaleHead,
            )
            .expect("stale head");
        assert!(stale.changed);
        assert!(!stale.wake_enqueued);
        assert_eq!(stale.phase.as_deref(), Some("terminal"));
        let wake_state: String = ledger
            .connect_read_only()
            .expect("connection")
            .query_row("SELECT state FROM outbox", [], |row| row.get(0))
            .expect("wake state");
        assert_eq!(wake_state, "failed");
    }

    #[test]
    fn concurrent_daemon_and_steward_producers_dedupe_to_one_wake() {
        let (_state, ledger, repo, _wake) = published();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let ledger = ledger.clone();
            let repo = repo.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                ledger.apply_native_steward_disposition(
                    &repo,
                    43,
                    &"a".repeat(40),
                    NativeStewardDisposition::Actionable,
                )
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().expect("join").expect("deduped producer");
        }
        assert_eq!(counts(&ledger), ("dispatching".to_owned(), 1));
    }
}
