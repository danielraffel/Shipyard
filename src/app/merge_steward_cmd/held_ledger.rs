//! Heads the steward has seen held, kept where pull-request code cannot reach.
//!
//! A pull request's own workflow token can delete or edit any comment in its
//! repository (merge-queue-rehearsal#47). With an APPROVE of a head and a
//! later HOLD of the same head, deleting the HOLD, or editing its verdict
//! line away, would leave the gate reading only the APPROVE. So once a
//! steward pass judges a head held, the head is written to the steward's own
//! ledger, and the gate refuses it from then on whatever the comments say:
//! a HOLD makes its head un-approvable, and only a new head can be approved.
//!
//! This is interim hardening. It remembers only HOLDs a pass saw: one deleted
//! before the next pass judged its head is not remembered. The durable fix
//! takes verdicts from an App-posted status instead of comments.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use super::{ApprovalBlockers, StewardLedger};
use crate::head_approval::{ApprovalBlocker, HeadGate};

/// Held heads per pull request: `repo#pr` (lowercase repository) to each
/// held head (lowercase) and when a pass first saw it held (RFC 3339).
pub(super) type HeldHeads = BTreeMap<String, BTreeMap<String, String>>;

/// How long a held head is remembered. A pull request open longer than this
/// with an unchanged held head is far past any landing it could race.
const RETENTION_DAYS: i64 = 30;

fn key(repo: &str, pr: u64) -> String {
    format!("{}#{pr}", repo.to_ascii_lowercase())
}

/// Remember every head this pass's gate reads found held.
pub(super) fn record(
    ledger: &mut StewardLedger,
    repo: &str,
    blockers: &ApprovalBlockers,
    now: DateTime<Utc>,
) {
    for (number, gate) in blockers {
        if matches!(gate.blocker, Some(ApprovalBlocker::Held { .. })) {
            ledger
                .held_heads
                .entry(key(repo, *number))
                .or_default()
                .entry(gate.head.to_ascii_lowercase())
                .or_insert_with(|| now.to_rfc3339());
        }
    }
    let floor = now - Duration::days(RETENTION_DAYS);
    for heads in ledger.held_heads.values_mut() {
        heads.retain(|_, since| {
            DateTime::parse_from_rfc3339(since).map_or(true, |at| at.with_timezone(&Utc) > floor)
        });
    }
    ledger.held_heads.retain(|_, heads| !heads.is_empty());
}

/// When a pass first saw `head` of `repo#pr` held, if it ever did.
pub(super) fn held_since<'a>(
    ledger: &'a StewardLedger,
    repo: &str,
    pr: u64,
    head: &str,
) -> Option<&'a str> {
    ledger
        .held_heads
        .get(&key(repo, pr))?
        .get(&head.to_ascii_lowercase())
        .map(String::as_str)
}

/// `gate`, refused when the ledger holds `head`: an APPROVE never outranks a
/// HOLD the steward once saw on the same head, even after the HOLD comment
/// was deleted or edited.
pub(super) fn apply(
    ledger: &StewardLedger,
    repo: &str,
    pr: u64,
    head: &str,
    outcome: (HeadGate, Vec<String>, Option<ApprovalBlocker>),
) -> (HeadGate, Vec<String>, Option<ApprovalBlocker>) {
    let (gate, mut events, blocker) = outcome;
    let Some(since) = held_since(ledger, repo, pr, head).filter(|_| gate.allows()) else {
        return (gate, events, blocker);
    };
    events.push("head_held:held by the steward ledger".to_owned());
    (
        HeadGate::NotApproved {
            head: head.to_owned(),
        },
        events,
        Some(ApprovalBlocker::Held {
            holder: "the steward ledger (a HOLD it saw on this head)".to_owned(),
            record: "ledger",
            id: None,
            at: Some(since.to_owned()),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::head_approval::Approval;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn approved() -> (HeadGate, Vec<String>, Option<ApprovalBlocker>) {
        (
            HeadGate::Approved {
                approval: Approval::Marker {
                    login: "shipyard-local[bot]".to_owned(),
                    comment_id: Some(1),
                    performed_via_github_app_id: None,
                },
            },
            Vec::new(),
            None,
        )
    }

    /// A pass's gate read that found `head` held by a comment HOLD.
    fn held_gate(head: &str) -> super::super::blocked_on::GateRecord {
        super::super::blocked_on::GateRecord::new(
            head,
            Some(ApprovalBlocker::Held {
                holder: "pulp-review".to_owned(),
                record: "comment",
                id: Some(2),
                at: Some("2026-11-24T01:00:00Z".to_owned()),
            }),
            &serde_json::Value::Null,
            false,
        )
    }

    /// APPROVE(H), then HOLD(H) seen by a pass, then the HOLD deleted: the
    /// comments alone now read approved, and the ledger still refuses.
    #[test]
    fn a_hold_the_steward_saw_outlives_its_deleted_comment() {
        let mut ledger = StewardLedger::default();
        let now = Utc::now();
        // Control: nothing recorded, the approval stands.
        let (gate, _, _) = apply(&ledger, "Owner/Repo", 7, HEAD, approved());
        assert!(gate.allows());
        record(
            &mut ledger,
            "Owner/Repo",
            &ApprovalBlockers::from([(7, held_gate(HEAD))]),
            now,
        );
        let (gate, events, blocker) = apply(&ledger, "owner/repo", 7, HEAD, approved());
        assert!(!gate.allows(), "{gate:?}");
        assert!(matches!(blocker, Some(ApprovalBlocker::Held { .. })));
        assert!(events.iter().any(|event| event.starts_with("head_held:")));
        // Another head of the same pull request, or another pull request, is
        // not held by it.
        assert!(
            apply(&ledger, "owner/repo", 7, &"b".repeat(40), approved())
                .0
                .allows()
        );
        assert!(apply(&ledger, "owner/repo", 8, HEAD, approved()).0.allows());
    }

    #[test]
    fn a_held_head_is_forgotten_after_the_retention_window() {
        let mut ledger = StewardLedger::default();
        let then = Utc::now() - Duration::days(RETENTION_DAYS + 1);
        record(
            &mut ledger,
            "o/r",
            &ApprovalBlockers::from([(7, held_gate(HEAD))]),
            then,
        );
        assert!(held_since(&ledger, "o/r", 7, HEAD).is_some());
        record(&mut ledger, "o/r", &ApprovalBlockers::new(), Utc::now());
        assert!(held_since(&ledger, "o/r", 7, HEAD).is_none());
    }
}
