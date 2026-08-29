use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::*;

fn draft(sequence: u64, kind: TransitionKind) -> TransitionDraft {
    TransitionDraft {
        workstream_id: "GEN-14".to_owned(),
        sequence,
        kind,
        evidence: ProjectionEvidence {
            source_revision: "a".repeat(64),
            exact_head: Some("b".repeat(40)),
            receipt_sha256: format!("{sequence:064x}"),
        },
        supersedes_transition_id: None,
        note: Some("ready".to_owned()),
    }
}

#[derive(Default)]
struct Adapter {
    calls: Arc<AtomicUsize>,
    failures: usize,
    wrong_readback: bool,
    last_evidence_identity: Option<String>,
}

impl TransitionProjectionAdapter for Adapter {
    fn submit(
        &mut self,
        transition: &ProjectedTransition,
    ) -> Result<SubmitReceipt, AdapterFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.failures > 0 {
            self.failures -= 1;
            return Err(AdapterFailure {
                reason: "token=secret temporary outage".to_owned(),
                retryable: true,
            });
        }
        self.last_evidence_identity = Some(transition.evidence_identity.clone());
        Ok(SubmitReceipt {
            external_id: "linear-comment-1".to_owned(),
            idempotency_key: transition.transition_id.clone(),
        })
    }

    fn readback(&mut self, receipt: &SubmitReceipt) -> Result<ProjectionReadback, AdapterFailure> {
        Ok(ProjectionReadback {
            transition_id: receipt.idempotency_key.clone(),
            evidence_identity: if self.wrong_readback {
                "0".repeat(64)
            } else {
                self.last_evidence_identity
                    .clone()
                    .expect("submitted evidence")
            },
        })
    }
}

#[test]
fn disabled_mode_has_zero_effect_and_never_calls_adapter() {
    let outbox = TransitionOutbox::disabled();
    let invalid = TransitionDraft {
        workstream_id: String::new(),
        ..draft(1, TransitionKind::Waiting)
    };
    assert_eq!(outbox.enqueue(invalid).unwrap(), EnqueueOutcome::Disabled);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut adapter = Adapter {
        calls: Arc::clone(&calls),
        ..Adapter::default()
    };
    assert_eq!(
        outbox.reconcile_one(&mut adapter, 0).unwrap(),
        ReconcileOutcome::Disabled
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn identities_are_stable_and_notes_are_redacted() {
    let mut input = draft(1, TransitionKind::Handoff);
    input.note = Some("token=abc ghp_secret Bearer raw-secret safe".to_owned());
    let first = input.clone().seal().unwrap();
    let second = input.seal().unwrap();
    assert_eq!(first.transition_id, second.transition_id);
    assert_eq!(first.evidence_identity, second.evidence_identity);
    assert_eq!(
        first.note.as_deref(),
        Some("[REDACTED] [REDACTED] [REDACTED] [REDACTED] safe")
    );
    assert!(!serde_json::to_string(&first).unwrap().contains("secret"));
}

#[test]
fn every_transition_kind_has_a_stable_wire_name() {
    let names = [
        (TransitionKind::Handoff, "\"handoff\""),
        (TransitionKind::Waiting, "\"waiting\""),
        (TransitionKind::Actionable, "\"actionable\""),
        (TransitionKind::NewHead, "\"new_head\""),
        (TransitionKind::Merge, "\"merge\""),
        (TransitionKind::ConfiguredClosure, "\"configured_closure\""),
    ];
    for (kind, name) in names {
        assert_eq!(serde_json::to_string(&kind).unwrap(), name);
    }
}

#[test]
fn restart_replays_pending_and_exact_readback_ack_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let transition = draft(1, TransitionKind::Handoff).seal().unwrap();
    {
        let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
        assert_eq!(
            outbox.enqueue(draft(1, TransitionKind::Handoff)).unwrap(),
            EnqueueOutcome::Queued
        );
    }
    let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
    let mut adapter = Adapter::default();
    assert_eq!(
        outbox.reconcile_one(&mut adapter, 1).unwrap(),
        ReconcileOutcome::Acknowledged {
            transition_id: transition.transition_id.clone()
        }
    );
    assert_eq!(
        outbox.reconcile_one(&mut adapter, 1).unwrap(),
        ReconcileOutcome::Idle
    );
    assert!(outbox.snapshot().unwrap()[0].acknowledged);
}

#[test]
fn retry_and_readback_mismatch_remain_queued_with_backoff() {
    let temp = tempfile::tempdir().unwrap();
    let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
    outbox.enqueue(draft(1, TransitionKind::Waiting)).unwrap();
    let mut adapter = Adapter {
        failures: 1,
        ..Adapter::default()
    };
    assert!(matches!(
        outbox.reconcile_one(&mut adapter, 100).unwrap(),
        ReconcileOutcome::RetryQueued {
            retry_at_unix_ms: 1_100,
            ..
        }
    ));
    assert_eq!(
        outbox.reconcile_one(&mut adapter, 1_099).unwrap(),
        ReconcileOutcome::Idle
    );
    adapter.wrong_readback = true;
    assert!(matches!(
        outbox.reconcile_one(&mut adapter, 1_100).unwrap(),
        ReconcileOutcome::RetryQueued {
            retry_at_unix_ms: 3_100,
            ..
        }
    ));
    let bytes = fs::read(temp.path().join("outbox/transitions.ndjson")).unwrap();
    assert!(!String::from_utf8(bytes).unwrap().contains("secret"));
}

#[test]
fn ordering_and_supersession_skip_obsolete_pending_state() {
    let temp = tempfile::tempdir().unwrap();
    let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
    let first = draft(1, TransitionKind::Waiting).seal().unwrap();
    outbox.enqueue(draft(1, TransitionKind::Waiting)).unwrap();
    assert!(
        outbox
            .enqueue(draft(1, TransitionKind::Actionable))
            .is_err()
    );
    let mut next = draft(2, TransitionKind::Actionable);
    next.supersedes_transition_id = Some(first.transition_id);
    outbox.enqueue(next).unwrap();
    let snapshot = outbox.snapshot().unwrap();
    assert!(snapshot[0].superseded);
    let mut adapter = Adapter::default();
    let result = outbox.reconcile_one(&mut adapter, 0).unwrap();
    assert!(matches!(result, ReconcileOutcome::Acknowledged { .. }));
    assert_eq!(outbox.snapshot().unwrap()[1].transition.sequence, 2);
}

#[test]
fn active_claim_blocks_concurrent_supersession_until_exact_ack() {
    struct SupersedingAdapter<'a> {
        outbox: &'a TransitionOutbox,
        blocked: bool,
        evidence_identity: Option<String>,
    }

    impl TransitionProjectionAdapter for SupersedingAdapter<'_> {
        fn submit(
            &mut self,
            transition: &ProjectedTransition,
        ) -> Result<SubmitReceipt, AdapterFailure> {
            let mut newer = draft(2, TransitionKind::Actionable);
            newer.supersedes_transition_id = Some(transition.transition_id.clone());
            self.blocked = self.outbox.enqueue(newer).is_err();
            self.evidence_identity = Some(transition.evidence_identity.clone());
            Ok(SubmitReceipt {
                external_id: "external-claimed".to_owned(),
                idempotency_key: transition.transition_id.clone(),
            })
        }

        fn readback(
            &mut self,
            receipt: &SubmitReceipt,
        ) -> Result<ProjectionReadback, AdapterFailure> {
            Ok(ProjectionReadback {
                transition_id: receipt.idempotency_key.clone(),
                evidence_identity: self.evidence_identity.clone().unwrap(),
            })
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
    outbox.enqueue(draft(1, TransitionKind::Waiting)).unwrap();
    let mut adapter = SupersedingAdapter {
        outbox: &outbox,
        blocked: false,
        evidence_identity: None,
    };
    assert!(matches!(
        outbox.reconcile_one(&mut adapter, 0).unwrap(),
        ReconcileOutcome::Acknowledged { .. }
    ));
    assert!(adapter.blocked);
}

#[test]
fn expired_claim_is_reclaimed_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("outbox");
    let outbox = TransitionOutbox::open(&root).unwrap();
    let transition = draft(1, TransitionKind::Waiting).seal().unwrap();
    outbox.enqueue(draft(1, TransitionKind::Waiting)).unwrap();
    let storage = outbox.storage.as_ref().unwrap();
    storage
        .with_exclusive_log(|log| {
            append_record(
                log,
                &LogRecord::Claim {
                    transition_id: transition.transition_id,
                    claim_id: "c".repeat(64),
                    claim_until_unix_ms: 1,
                },
            )
        })
        .unwrap();
    drop(outbox);

    let reopened = TransitionOutbox::open(root).unwrap();
    let mut adapter = Adapter::default();
    assert!(matches!(
        reopened.reconcile_one(&mut adapter, 2).unwrap(),
        ReconcileOutcome::Acknowledged { .. }
    ));
}

#[test]
fn active_claim_does_not_starve_an_unrelated_ready_transition() {
    let temp = tempfile::tempdir().unwrap();
    let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
    let first = draft(1, TransitionKind::Waiting).seal().unwrap();
    outbox.enqueue(draft(1, TransitionKind::Waiting)).unwrap();
    let mut second_draft = draft(1, TransitionKind::Actionable);
    second_draft.workstream_id = "GEN-15".to_owned();
    let second = second_draft.clone().seal().unwrap();
    outbox.enqueue(second_draft).unwrap();
    let storage = outbox.storage.as_ref().unwrap();
    storage
        .with_exclusive_log(|log| {
            append_record(
                log,
                &LogRecord::Claim {
                    transition_id: first.transition_id,
                    claim_id: "d".repeat(64),
                    claim_until_unix_ms: current_unix_ms()?.saturating_add(10_000),
                },
            )
        })
        .unwrap();
    let mut adapter = Adapter::default();
    assert_eq!(
        outbox.reconcile_one(&mut adapter, 0).unwrap(),
        ReconcileOutcome::Acknowledged {
            transition_id: second.transition_id
        }
    );
}

#[test]
fn replay_rejects_individually_valid_but_reordered_transitions() {
    let temp = tempfile::tempdir().unwrap();
    let outbox = TransitionOutbox::open(temp.path().join("outbox")).unwrap();
    outbox.enqueue(draft(2, TransitionKind::NewHead)).unwrap();
    let storage = outbox.storage.as_ref().unwrap();
    storage
        .with_exclusive_log(|log| {
            append_record(
                log,
                &LogRecord::Enqueue {
                    transition: draft(1, TransitionKind::Waiting).seal().unwrap(),
                },
            )
        })
        .unwrap();
    assert!(outbox.snapshot().is_err());
}

#[test]
fn partial_tail_is_removed_without_losing_committed_records() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("outbox");
    let outbox = TransitionOutbox::open(&root).unwrap();
    outbox.enqueue(draft(1, TransitionKind::NewHead)).unwrap();
    let path = root.join("transitions.ndjson");
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"record\":\"enqueue\"").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let reopened = TransitionOutbox::open(&root).unwrap();
    assert_eq!(reopened.snapshot().unwrap().len(), 1);
    assert_eq!(fs::read(&path).unwrap().last(), Some(&b'\n'));
}

#[test]
fn complete_malformed_record_fails_closed_without_truncation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("outbox");
    let outbox = TransitionOutbox::open(&root).unwrap();
    outbox.enqueue(draft(1, TransitionKind::NewHead)).unwrap();
    let path = root.join("transitions.ndjson");
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{not-json}\n").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let before = fs::read(&path).unwrap();
    let reopened = TransitionOutbox::open(&root).unwrap();
    assert!(reopened.snapshot().is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn concurrent_writers_serialize_without_lost_transitions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("outbox");
    TransitionOutbox::open(&root).unwrap();
    let mut threads = Vec::new();
    for index in 0..8_u64 {
        let root = root.clone();
        threads.push(thread::spawn(move || {
            let outbox = TransitionOutbox::open(root).unwrap();
            let mut item = draft(index + 1, TransitionKind::NewHead);
            item.workstream_id = format!("GEN-{index}");
            outbox.enqueue(item).unwrap();
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
    let outbox = TransitionOutbox::open(root).unwrap();
    assert_eq!(outbox.snapshot().unwrap().len(), 8);
}

#[cfg(unix)]
#[test]
fn symlink_storage_is_rejected() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    fs::create_dir(&real).unwrap();
    let link = temp.path().join("link");
    symlink(&real, &link).unwrap();
    assert!(TransitionOutbox::open(link).is_err());

    let root = temp.path().join("outbox");
    fs::create_dir(&root).unwrap();
    symlink(root.join("elsewhere"), root.join("transitions.ndjson")).unwrap();
    assert!(TransitionOutbox::open(root).is_err());
}
