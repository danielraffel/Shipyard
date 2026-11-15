//! The steward's side of a solo merge-queue reservation
//! ([`crate::merge_queue_solo`]).
//!
//! Each applying pass reads the repository's reservation once, ends it when
//! its pull request merged, closed, changed head, left the queue after being
//! seen in it, or ran out its TTL, and reports it. While it is active every
//! admission of another pull request into that queue is held with
//! `held_solo_reservation:#<n>`; the reserved pull request goes through every
//! gate unchanged. An unreadable reservation holds every admission and is
//! reported as an error, so a human sees it.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{GitHubActions, MutationApplyContext, ObservedPr, RepoObservation};
use crate::merge_queue_solo::{self, LiveReserved, Reconcile, SoloState};

/// The hold token for `pr`, or `None` when it may go through its own gates.
pub(super) fn hold(context: &MutationApplyContext<'_>, pr: &ObservedPr) -> Option<String> {
    merge_queue_solo::read(
        &context.mutation_control.state_dir,
        &context.observation.repo,
        Utc::now(),
    )
    .admission_hold(&context.observation.base, pr.fact.number)
}

/// What one pass found and did.
pub(super) struct PassReport {
    /// The repository report's `solo_reservation`, absent when there is none.
    pub(super) report: Option<Value>,
    /// An error for the repository: an unreadable reservation, or one the
    /// pass could not reconcile.
    pub(super) error: Option<String>,
}

/// Reconcile and report `observation.repo`'s reservation. Only an applying
/// pass ends or latches one; a report pass only says what it would do.
pub(super) fn reconcile(
    actions: &GitHubActions,
    state_dir: &Path,
    observation: &RepoObservation,
    apply: bool,
    now: DateTime<Utc>,
) -> PassReport {
    reconcile_with(state_dir, &observation.repo, apply, now, &|number| {
        super::pr_mutations::read_pr_queue_state(actions, observation, number)
            .ok()
            .and_then(|response| LiveReserved::from_queue_state(&response))
    })
}

/// [`reconcile`] with the live read injected.
pub(super) fn reconcile_with(
    state_dir: &Path,
    repo: &str,
    apply: bool,
    now: DateTime<Utc>,
    read_live: &dyn Fn(u64) -> Option<LiveReserved>,
) -> PassReport {
    let state = merge_queue_solo::read(state_dir, repo, now);
    let reservation = match &state {
        SoloState::Absent => {
            return PassReport {
                report: None,
                error: None,
            };
        }
        SoloState::Unreadable { .. } => {
            return PassReport {
                report: Some(state.report()),
                error: Some(state.describe()),
            };
        }
        SoloState::Active(reservation) | SoloState::Expired(reservation) => reservation.clone(),
    };
    // Expiry needs no read; anything else needs the pull request as it is now.
    let live = if now >= reservation.expires_at {
        None
    } else {
        read_live(reservation.pr)
    };
    let decision = merge_queue_solo::reconcile_decision(&reservation, live.as_ref(), now);
    let action = match decision {
        Reconcile::Keep => "kept".to_owned(),
        Reconcile::Latch => "seen_in_queue".to_owned(),
        Reconcile::Clear(why) => format!("ended:{why}"),
    };
    let unread = live.is_none() && now < reservation.expires_at;
    if !apply {
        let mut report = state.report();
        if let Some(object) = report.as_object_mut() {
            object.insert("would".to_owned(), Value::String(action));
            object.insert("live_read".to_owned(), Value::Bool(!unread));
        }
        return PassReport {
            report: Some(report),
            error: None,
        };
    }
    match merge_queue_solo::apply_decision(state_dir, &reservation, &decision, now) {
        Ok(after) => {
            let mut report = after.report();
            if let Some(object) = report.as_object_mut() {
                object.insert("action".to_owned(), Value::String(action));
                object.insert("live_read".to_owned(), Value::Bool(!unread));
            }
            let error = matches!(after, SoloState::Unreadable { .. }).then(|| after.describe());
            PassReport {
                report: Some(report),
                error,
            }
        }
        Err(error) => PassReport {
            report: Some(state.report()),
            error: Some(format!("solo reservation reconcile failed: {error}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge_queue_solo::{DEFAULT_TTL_MINUTES, Reservation, SoloReason};

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const REPO: &str = "Generous-Corp/vellum";

    fn reserve(state: &Path, now: DateTime<Utc>) -> Reservation {
        let reservation = Reservation::new(
            REPO,
            "main",
            451,
            HEAD,
            SoloReason::Release,
            "release coalesce lands alone",
            "verdict comment 1",
            DEFAULT_TTL_MINUTES,
            now,
        )
        .expect("reservation");
        merge_queue_solo::reserve(state, &reservation).expect("reserve");
        reservation
    }

    #[allow(clippy::unnecessary_wraps)] // The live-read signature.
    fn merged(_: u64) -> Option<LiveReserved> {
        Some(LiveReserved {
            state: "MERGED".to_owned(),
            head: HEAD.to_owned(),
            in_queue: false,
        })
    }

    #[test]
    fn an_applying_pass_ends_the_reservation_once_its_pull_request_merged() {
        let temp = tempfile::tempdir().expect("temp");
        let now = Utc::now();
        reserve(temp.path(), now);
        let report = reconcile_with(temp.path(), REPO, false, now, &merged);
        assert_eq!(report.error, None);
        let value = report.report.expect("report");
        assert_eq!(value["would"], "ended:merged", "{value}");
        assert!(matches!(
            merge_queue_solo::read(temp.path(), REPO, now),
            SoloState::Active(_)
        ));
        let report = reconcile_with(temp.path(), REPO, true, now, &merged);
        assert_eq!(report.error, None);
        let value = report.report.expect("report");
        assert_eq!(value["action"], "ended:merged", "{value}");
        assert_eq!(value["state"], "absent", "{value}");
        assert_eq!(
            merge_queue_solo::read(temp.path(), REPO, now),
            SoloState::Absent
        );
    }

    #[test]
    fn an_unread_pull_request_keeps_the_reservation() {
        let temp = tempfile::tempdir().expect("temp");
        let now = Utc::now();
        reserve(temp.path(), now);
        let report = reconcile_with(temp.path(), REPO, true, now, &|_| None);
        let value = report.report.expect("report");
        assert_eq!(value["action"], "kept", "{value}");
        assert_eq!(value["live_read"], false, "{value}");
        assert!(matches!(
            merge_queue_solo::read(temp.path(), REPO, now),
            SoloState::Active(_)
        ));
    }

    #[test]
    fn an_expired_reservation_is_ended_without_reading_github() {
        let temp = tempfile::tempdir().expect("temp");
        let then = Utc::now() - chrono::Duration::minutes(DEFAULT_TTL_MINUTES + 1);
        reserve(temp.path(), then);
        let report = reconcile_with(temp.path(), REPO, true, Utc::now(), &|_| {
            panic!("an expired reservation needs no read")
        });
        assert_eq!(report.report.expect("report")["action"], "ended:expired");
        assert_eq!(
            merge_queue_solo::read(temp.path(), REPO, Utc::now()),
            SoloState::Absent
        );
    }

    #[test]
    fn an_unreadable_reservation_is_an_error_the_tick_reports() {
        let temp = tempfile::tempdir().expect("temp");
        let path = merge_queue_solo::reservation_path(temp.path(), REPO).expect("path");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
        std::fs::write(&path, "not json").expect("write");
        let report = reconcile_with(temp.path(), REPO, true, Utc::now(), &merged);
        let error = report.error.expect("error");
        assert!(error.contains("unreadable"), "{error}");
        assert_eq!(report.report.expect("report")["state"], "unreadable");
        // Fail closed: nothing ended it.
        assert!(path.exists());
    }
}
