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
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use super::{ApprovalBlockers, StewardLedger};
use crate::head_approval::{Approval, ApprovalBlocker, HeadGate};

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

/// `reports` without any pull request whose head the ledger holds: the
/// native-arm backstop is never offered one.
pub(super) fn backstop_candidates(
    ledger: &StewardLedger,
    repo: &str,
    reports: Vec<super::PrReport>,
) -> Vec<super::PrReport> {
    reports
        .into_iter()
        .filter(|report| held_since(ledger, repo, report.number, &report.head_sha).is_none())
        .collect()
}

/// The heads whose hold refuses `gate`: the head it judges, and every head
/// its approval was carried from. A held head's approval never carries: with
/// the HOLD comment deleted, a merge of the base would otherwise carry it to
/// the new head.
fn judged_heads(gate: &HeadGate, head: &str) -> Vec<String> {
    let mut heads = vec![head.to_ascii_lowercase()];
    if let HeadGate::Approved { approval } = gate {
        let mut approval = approval;
        while let Approval::Carried {
            from_head,
            approval: inner,
            ..
        } = approval
        {
            heads.push(from_head.to_ascii_lowercase());
            approval = inner;
        }
    }
    heads
}

/// The hold hook's record of every HOLD posted through `ghapp` on this host
/// (`scripts/ghapp_verdict_hold_hook.py`), in the guards directory; the
/// queue-arm guard reads the same file.
const HOLDS_LOG: &str = "holds.jsonl";

/// When this host's hold record names `head` of `repo#pr` as held.
///
/// No file (or no guards directory) is no record.
///
/// # Errors
///
/// When the file exists but cannot be read: the gate is refused rather than
/// judged without it.
fn recorded_hold(
    dir: Option<&Path>,
    repo: &str,
    pr: u64,
    head: &str,
) -> Result<Option<String>, String> {
    let Some(dir) = dir else {
        return Ok(None);
    };
    let path = dir.join(HOLDS_LOG);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "the hold record {} is unreadable: {error}",
                path.display()
            ));
        }
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|row| {
            row.get("repo")
                .and_then(Value::as_str)
                .is_some_and(|name| name.eq_ignore_ascii_case(repo))
                && row.get("pr").and_then(Value::as_u64) == Some(pr)
                && row
                    .get("hold_head")
                    .and_then(Value::as_str)
                    .is_some_and(|held| held.eq_ignore_ascii_case(head))
        })
        .map(|row| {
            row.get("at")
                .and_then(Value::as_str)
                .unwrap_or("an unrecorded time")
                .to_owned()
        }))
}

/// `gate`, refused when the steward ledger or this host's hold record holds
/// `head`, or a head its approval was carried from: an APPROVE never
/// outranks a HOLD once seen on the same head, even after the HOLD comment
/// was deleted or edited.
///
/// # Errors
///
/// When the host's hold record exists but cannot be read.
pub(super) fn apply(
    ledger: &StewardLedger,
    repo: &str,
    pr: u64,
    head: &str,
    outcome: (HeadGate, Vec<String>, Option<ApprovalBlocker>),
) -> Result<(HeadGate, Vec<String>, Option<ApprovalBlocker>), String> {
    apply_with_holds(
        ledger,
        crate::head_approval::guards_dir().as_deref(),
        repo,
        pr,
        head,
        outcome,
    )
}

/// [`apply`] with the guards directory given.
pub(super) fn apply_with_holds(
    ledger: &StewardLedger,
    guards: Option<&Path>,
    repo: &str,
    pr: u64,
    head: &str,
    outcome: (HeadGate, Vec<String>, Option<ApprovalBlocker>),
) -> Result<(HeadGate, Vec<String>, Option<ApprovalBlocker>), String> {
    let (gate, mut events, blocker) = outcome;
    if !gate.allows() {
        return Ok((gate, events, blocker));
    }
    let mut found = None;
    for judged in judged_heads(&gate, head) {
        if let Some(since) = held_since(ledger, repo, pr, &judged) {
            found = Some(("the steward ledger", judged, since.to_owned()));
            break;
        }
        if let Some(since) = recorded_hold(guards, repo, pr, &judged)? {
            found = Some(("this host's hold record", judged, since));
            break;
        }
    }
    let Some((source, held_head, since)) = found else {
        return Ok((gate, events, blocker));
    };
    let carried = !held_head.eq_ignore_ascii_case(head);
    events.push(format!("head_held:held by {source}"));
    Ok((
        HeadGate::NotApproved {
            head: head.to_owned(),
        },
        events,
        Some(ApprovalBlocker::Held {
            holder: if carried {
                format!(
                    "{source} (a HOLD it saw on head {}, whose approval this head carries)",
                    held_head.get(..12).unwrap_or(&held_head)
                )
            } else {
                format!("{source} (a HOLD it saw on this head)")
            },
            record: "ledger",
            id: None,
            at: Some(since),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (gate, _, _) =
            apply_with_holds(&ledger, None, "Owner/Repo", 7, HEAD, approved()).unwrap();
        assert!(gate.allows());
        record(
            &mut ledger,
            "Owner/Repo",
            &ApprovalBlockers::from([(7, held_gate(HEAD))]),
            now,
        );
        let (gate, events, blocker) =
            apply_with_holds(&ledger, None, "owner/repo", 7, HEAD, approved()).unwrap();
        assert!(!gate.allows(), "{gate:?}");
        assert!(matches!(blocker, Some(ApprovalBlocker::Held { .. })));
        assert!(events.iter().any(|event| event.starts_with("head_held:")));
        // Another head of the same pull request, or another pull request, is
        // not held by it.
        assert!(
            apply_with_holds(&ledger, None, "owner/repo", 7, &"b".repeat(40), approved())
                .unwrap()
                .0
                .allows()
        );
        assert!(
            apply_with_holds(&ledger, None, "owner/repo", 8, HEAD, approved())
                .unwrap()
                .0
                .allows()
        );
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

    /// `approved()` carried to `head` from `from`.
    fn carried(from: &str) -> (HeadGate, Vec<String>, Option<ApprovalBlocker>) {
        let (HeadGate::Approved { approval }, events, blocker) = approved() else {
            unreachable!()
        };
        (
            HeadGate::Approved {
                approval: Approval::Carried {
                    from_head: from.to_owned(),
                    approval: Box::new(approval),
                    rebase: false,
                },
            },
            events,
            blocker,
        )
    }

    /// The delta review's D1: APPROVE(H), HOLD(H) seen and recorded, the
    /// HOLD deleted, the base merged in (head H2). The approval of H carries
    /// to H2 by merge-only updates; a held head's approval never carries.
    #[test]
    fn a_carry_from_a_held_head_is_refused() {
        let mut ledger = StewardLedger::default();
        let h2 = "b".repeat(40);
        assert!(
            apply_with_holds(&ledger, None, "o/r", 7, &h2, carried(HEAD))
                .unwrap()
                .0
                .allows(),
            "control: nothing held, the carry stands"
        );
        record(
            &mut ledger,
            "o/r",
            &ApprovalBlockers::from([(7, held_gate(HEAD))]),
            Utc::now(),
        );
        let (gate, _, blocker) =
            apply_with_holds(&ledger, None, "o/r", 7, &h2, carried(HEAD)).unwrap();
        assert!(!gate.allows(), "{gate:?}");
        assert!(matches!(blocker, Some(ApprovalBlocker::Held { .. })));
        assert!(
            apply_with_holds(&ledger, None, "o/r", 7, &h2, carried(&"c".repeat(40)))
                .unwrap()
                .0
                .allows(),
            "a carry from an unheld head stands"
        );
    }

    /// The host's hold record holds the live head and a carried-from head
    /// with an empty ledger; an unreadable record is an error, never a pass.
    #[test]
    fn the_hosts_hold_record_holds_and_fails_closed() {
        let ledger = StewardLedger::default();
        let dir = tempfile::tempdir().expect("guards");
        let h2 = "b".repeat(40);
        assert!(
            apply_with_holds(&ledger, Some(dir.path()), "o/r", 7, HEAD, approved())
                .unwrap()
                .0
                .allows(),
            "control: no file is no record"
        );
        std::fs::write(
            dir.path().join(HOLDS_LOG),
            format!(
                "not json\n{}\n",
                serde_json::json!({"repo": "O/R", "pr": 7, "hold_head": HEAD,
                                   "at": "2026-11-24T01:00:00Z"})
            ),
        )
        .expect("record");
        assert!(
            !apply_with_holds(&ledger, Some(dir.path()), "o/r", 7, HEAD, approved())
                .unwrap()
                .0
                .allows()
        );
        assert!(
            !apply_with_holds(&ledger, Some(dir.path()), "o/r", 7, &h2, carried(HEAD))
                .unwrap()
                .0
                .allows()
        );
        assert!(
            apply_with_holds(&ledger, Some(dir.path()), "o/r", 8, HEAD, approved())
                .unwrap()
                .0
                .allows(),
            "another pull request"
        );
        // A directory where the file should be cannot be read as a file.
        let unreadable = tempfile::tempdir().expect("guards");
        std::fs::create_dir(unreadable.path().join(HOLDS_LOG)).expect("dir");
        assert!(
            apply_with_holds(&ledger, Some(unreadable.path()), "o/r", 7, HEAD, approved()).is_err()
        );
    }

    #[test]
    fn the_native_arm_backstop_is_never_offered_a_held_head() {
        let row = |number: u64, head: &str| super::super::PrReport {
            number,
            head_sha: head.to_owned(),
            decision: super::super::StewardDecision::Unmanaged,
            mutation: None,
            error: None,
            blocked: super::super::blocked_on::Stall::default(),
            required_checks: Vec::new(),
        };
        let mut ledger = StewardLedger::default();
        let rows = || vec![row(7, HEAD), row(8, HEAD)];
        assert_eq!(
            backstop_candidates(&ledger, "o/r", rows()).len(),
            2,
            "control"
        );
        record(
            &mut ledger,
            "o/r",
            &ApprovalBlockers::from([(7, held_gate(HEAD))]),
            Utc::now(),
        );
        let offered = backstop_candidates(&ledger, "o/r", rows());
        assert_eq!(
            offered.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![8]
        );
    }
}
