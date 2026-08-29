//! Typed transfer from authoritative ledger intents into projection outboxes.

use std::path::Path;

use crate::transition_projection::{
    EnqueueOutcome, PROJECTION_CLAIM_LEASE_MS, ProjectionError, TransitionDraft,
};
use crate::work_ledger::{PendingProjectionIntent, WorkLedger, WorkLedgerResult};

use super::{CommittedTransitionIngress, MAX_COMMITTED_INTENTS_PER_DRAIN};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CommittedEnqueueDisposition {
    ActivelyClaimed,
    Contradiction,
    Transient,
}

#[derive(Debug)]
pub(crate) struct CommittedEnqueueError {
    disposition: CommittedEnqueueDisposition,
    detail: String,
}

impl CommittedEnqueueError {
    pub(super) fn contradiction(detail: impl Into<String>) -> Self {
        Self {
            disposition: CommittedEnqueueDisposition::Contradiction,
            detail: detail.into(),
        }
    }

    pub(super) fn transient(detail: impl Into<String>) -> Self {
        Self {
            disposition: CommittedEnqueueDisposition::Transient,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for CommittedEnqueueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for CommittedEnqueueError {}

impl From<ProjectionError> for CommittedEnqueueError {
    fn from(error: ProjectionError) -> Self {
        let disposition = match &error {
            ProjectionError::ActivelyClaimed => CommittedEnqueueDisposition::ActivelyClaimed,
            ProjectionError::Invalid(_) | ProjectionError::Contradiction(_) => {
                CommittedEnqueueDisposition::Contradiction
            }
            ProjectionError::Io(_)
            | ProjectionError::Clock
            | ProjectionError::Corrupt(_)
            | ProjectionError::Storage(_) => CommittedEnqueueDisposition::Transient,
        };
        Self {
            disposition,
            detail: error.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProjectionDrainFailureKind {
    LedgerOpen,
    LedgerRead,
    OutboxRetry,
    StateMutation,
}

impl ProjectionDrainFailureKind {
    const fn code(self) -> &'static str {
        match self {
            Self::LedgerOpen => "ledger-open",
            Self::LedgerRead => "ledger-read",
            Self::OutboxRetry => "outbox-retry",
            Self::StateMutation => "state-mutation",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectionDrainFailure {
    kind: ProjectionDrainFailureKind,
    intent_id: Option<String>,
    detail: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ProjectionIntentDrainReport {
    pub(super) scanned: usize,
    pub(super) projected: usize,
    pub(super) retried: usize,
    pub(super) quarantined: usize,
    pub(super) retained_disabled: usize,
    failures: Vec<ProjectionDrainFailure>,
}

impl ProjectionIntentDrainReport {
    pub(super) fn record_failure(
        &mut self,
        kind: ProjectionDrainFailureKind,
        intent_id: Option<&str>,
        detail: impl Into<String>,
    ) {
        self.failures.push(ProjectionDrainFailure {
            kind,
            intent_id: intent_id.map(str::to_owned),
            detail: detail.into(),
        });
    }

    pub(super) fn diagnostic_error(&self) -> Option<String> {
        self.failures.first().map_or_else(
            || {
                (self.quarantined > 0)
                    .then(|| "transition-projection-intent-contradiction".to_owned())
            },
            |failure| {
                Some(format!(
                    "transition-projection-intent-drain-{}",
                    failure.kind.code()
                ))
            },
        )
    }

    #[cfg(test)]
    fn failures(&self) -> &[ProjectionDrainFailure] {
        &self.failures
    }

    #[cfg(test)]
    pub(super) fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }
}

trait ProjectionIntentLedger {
    fn pending(
        &self,
        now_unix_ms: u64,
        limit: u64,
    ) -> WorkLedgerResult<Vec<PendingProjectionIntent>>;
    fn mark_projected(&self, intent_id: &str) -> WorkLedgerResult<()>;
    fn retry(
        &self,
        intent_id: &str,
        failure_class: &str,
        retry_at_unix_ms: u64,
    ) -> WorkLedgerResult<()>;
    fn quarantine(&self, intent_id: &str, failure_class: &str) -> WorkLedgerResult<()>;
}

impl ProjectionIntentLedger for WorkLedger {
    fn pending(
        &self,
        now_unix_ms: u64,
        limit: u64,
    ) -> WorkLedgerResult<Vec<PendingProjectionIntent>> {
        self.pending_projection_intents(now_unix_ms, limit)
    }

    fn mark_projected(&self, intent_id: &str) -> WorkLedgerResult<()> {
        self.mark_projection_intent_projected(intent_id)
    }

    fn retry(
        &self,
        intent_id: &str,
        failure_class: &str,
        retry_at_unix_ms: u64,
    ) -> WorkLedgerResult<()> {
        self.retry_projection_intent(intent_id, failure_class, retry_at_unix_ms)
    }

    fn quarantine(&self, intent_id: &str, failure_class: &str) -> WorkLedgerResult<()> {
        self.quarantine_projection_intent(intent_id, failure_class)
    }
}

trait ProjectionIntentIngress {
    fn enqueue_snapshot(
        &self,
        repository: &str,
        draft: TransitionDraft,
        receipt_snapshot: &[u8],
    ) -> Result<EnqueueOutcome, CommittedEnqueueError>;
}

impl ProjectionIntentIngress for CommittedTransitionIngress {
    fn enqueue_snapshot(
        &self,
        repository: &str,
        draft: TransitionDraft,
        receipt_snapshot: &[u8],
    ) -> Result<EnqueueOutcome, CommittedEnqueueError> {
        self.enqueue_committed_snapshot(repository, draft, receipt_snapshot)
    }
}

pub(super) fn drain_committed_projection_intents(
    state_dir: &Path,
    ingress: &CommittedTransitionIngress,
    now_unix_ms: u64,
) -> ProjectionIntentDrainReport {
    let ledger = match WorkLedger::open_existing(state_dir) {
        Ok(Some(ledger)) => ledger,
        Ok(None) => return ProjectionIntentDrainReport::default(),
        Err(error) => {
            let mut report = ProjectionIntentDrainReport::default();
            report.record_failure(
                ProjectionDrainFailureKind::LedgerOpen,
                None,
                error.to_string(),
            );
            return report;
        }
    };
    drain_projection_intents(&ledger, ingress, now_unix_ms)
}

fn drain_projection_intents<L: ProjectionIntentLedger, I: ProjectionIntentIngress>(
    ledger: &L,
    ingress: &I,
    now_unix_ms: u64,
) -> ProjectionIntentDrainReport {
    let mut report = ProjectionIntentDrainReport::default();
    let intents = match ledger.pending(now_unix_ms, MAX_COMMITTED_INTENTS_PER_DRAIN) {
        Ok(intents) => intents,
        Err(error) => {
            report.record_failure(
                ProjectionDrainFailureKind::LedgerRead,
                None,
                error.to_string(),
            );
            return report;
        }
    };
    for intent in intents {
        report.scanned += 1;
        let Ok(draft) = intent.reconstruct() else {
            match ledger.quarantine(&intent.intent_id, "receipt-contradiction") {
                Ok(()) => report.quarantined += 1,
                Err(error) => report.record_failure(
                    ProjectionDrainFailureKind::StateMutation,
                    Some(&intent.intent_id),
                    error.to_string(),
                ),
            }
            continue;
        };
        match ingress.enqueue_snapshot(&intent.repository, draft, &intent.receipt_snapshot) {
            Ok(EnqueueOutcome::Queued | EnqueueOutcome::AlreadyQueued) => {
                match ledger.mark_projected(&intent.intent_id) {
                    Ok(()) => report.projected += 1,
                    Err(error) => report.record_failure(
                        ProjectionDrainFailureKind::StateMutation,
                        Some(&intent.intent_id),
                        error.to_string(),
                    ),
                }
            }
            Ok(EnqueueOutcome::Disabled) => report.retained_disabled += 1,
            Err(error) if error.disposition == CommittedEnqueueDisposition::ActivelyClaimed => {
                let retry_at = now_unix_ms.saturating_add(PROJECTION_CLAIM_LEASE_MS);
                match ledger.retry(&intent.intent_id, "active-claim-supersession", retry_at) {
                    Ok(()) => report.retried += 1,
                    Err(state_error) => report.record_failure(
                        ProjectionDrainFailureKind::StateMutation,
                        Some(&intent.intent_id),
                        format!("{}; {}", error.detail, state_error),
                    ),
                }
            }
            Err(error) if error.disposition == CommittedEnqueueDisposition::Contradiction => {
                match ledger.quarantine(&intent.intent_id, "projection-contradiction") {
                    Ok(()) => report.quarantined += 1,
                    Err(state_error) => report.record_failure(
                        ProjectionDrainFailureKind::StateMutation,
                        Some(&intent.intent_id),
                        format!("{}; {}", error.detail, state_error),
                    ),
                }
            }
            Err(error) => {
                debug_assert_eq!(error.disposition, CommittedEnqueueDisposition::Transient);
                let exponent = intent.attempts.min(10);
                let delay = 1_000_u64.saturating_mul(1_u64 << exponent);
                match ledger.retry(
                    &intent.intent_id,
                    "projection-io-retry",
                    now_unix_ms.saturating_add(delay),
                ) {
                    Ok(()) => {
                        report.retried += 1;
                        report.record_failure(
                            ProjectionDrainFailureKind::OutboxRetry,
                            Some(&intent.intent_id),
                            error.detail,
                        );
                    }
                    Err(state_error) => report.record_failure(
                        ProjectionDrainFailureKind::StateMutation,
                        Some(&intent.intent_id),
                        format!("{}; {}", error.detail, state_error),
                    ),
                }
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_ledger::{
        RepoPolicy, WorkLedgerError, native_publication_test_policy as native_policy,
        native_publication_test_request as request,
    };

    fn ledger_with_projection_intent(state_dir: &Path) -> (WorkLedger, PendingProjectionIntent) {
        let request = request();
        let ledger = WorkLedger::open(state_dir).expect("ledger");
        ledger
            .set_repo_policy(
                &RepoPolicy {
                    repo: request.repository.clone(),
                    primary_platform: "macos".to_owned(),
                    compatibility_mode: "independent".to_owned(),
                    compatibility_lanes: vec!["linux".to_owned()],
                    blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                    declared_dependency_lanes: Vec::new(),
                    revision: 0,
                },
                0,
            )
            .expect("policy");
        WorkLedger::plan_or_apply_native_continuation(
            state_dir,
            &request,
            &native_policy(vec![request.repository.clone()]),
            true,
        )
        .expect("publication");
        let intent = ledger
            .pending_projection_intents(0, 1)
            .expect("pending")
            .pop()
            .expect("managed intent");
        (ledger, intent)
    }

    struct FixedIngress(Result<EnqueueOutcome, CommittedEnqueueDisposition>);

    impl ProjectionIntentIngress for FixedIngress {
        fn enqueue_snapshot(
            &self,
            _repository: &str,
            _draft: TransitionDraft,
            _receipt_snapshot: &[u8],
        ) -> Result<EnqueueOutcome, CommittedEnqueueError> {
            self.0.map_err(|disposition| CommittedEnqueueError {
                disposition,
                detail: "fixed test disposition".to_owned(),
            })
        }
    }

    struct FailingMutationLedger {
        intent: PendingProjectionIntent,
    }

    impl ProjectionIntentLedger for FailingMutationLedger {
        fn pending(
            &self,
            _now_unix_ms: u64,
            _limit: u64,
        ) -> WorkLedgerResult<Vec<PendingProjectionIntent>> {
            Ok(vec![self.intent.clone()])
        }

        fn mark_projected(&self, _intent_id: &str) -> WorkLedgerResult<()> {
            Err(WorkLedgerError::Refused("test projected write".to_owned()))
        }

        fn retry(
            &self,
            _intent_id: &str,
            _failure_class: &str,
            _retry_at_unix_ms: u64,
        ) -> WorkLedgerResult<()> {
            Err(WorkLedgerError::Refused("test retry write".to_owned()))
        }

        fn quarantine(&self, _intent_id: &str, _failure_class: &str) -> WorkLedgerResult<()> {
            Err(WorkLedgerError::Refused("test quarantine write".to_owned()))
        }
    }

    #[test]
    fn typed_enqueue_disposition_quarantines_contradiction_and_retries_transient() {
        assert_eq!(
            CommittedEnqueueError::from(ProjectionError::Invalid("sequence".to_owned()))
                .disposition,
            CommittedEnqueueDisposition::Contradiction
        );
        assert_eq!(
            CommittedEnqueueError::from(ProjectionError::Contradiction("collision".to_owned()))
                .disposition,
            CommittedEnqueueDisposition::Contradiction
        );
        assert_eq!(
            CommittedEnqueueError::from(ProjectionError::Corrupt("shared outbox".to_owned()))
                .disposition,
            CommittedEnqueueDisposition::Transient
        );
        assert_eq!(
            CommittedEnqueueError::from(ProjectionError::Storage("unsafe root".to_owned()))
                .disposition,
            CommittedEnqueueDisposition::Transient
        );
        for (disposition, expected_state, expected_quarantined, expected_retried) in [
            (
                CommittedEnqueueDisposition::Contradiction,
                "quarantined",
                1,
                0,
            ),
            (CommittedEnqueueDisposition::Transient, "pending", 0, 1),
        ] {
            let temp = tempfile::tempdir().expect("state");
            let (ledger, intent) = ledger_with_projection_intent(temp.path());
            let report = drain_projection_intents(&ledger, &FixedIngress(Err(disposition)), 10_000);
            assert_eq!(report.scanned, 1);
            assert_eq!(report.quarantined, expected_quarantined);
            assert_eq!(report.retried, expected_retried);
            assert_eq!(
                report.failures().len(),
                usize::from(disposition == CommittedEnqueueDisposition::Transient)
            );
            if disposition == CommittedEnqueueDisposition::Transient {
                assert_eq!(
                    report.diagnostic_error().as_deref(),
                    Some("transition-projection-intent-drain-outbox-retry")
                );
            }
            assert_eq!(
                ledger
                    .projection_intent_state(&intent.intent_id)
                    .expect("intent state"),
                (expected_state.to_owned(), 1),
            );
        }
    }

    #[test]
    fn reconstruction_and_state_update_failures_are_reported() {
        let temp = tempfile::tempdir().expect("state");
        let (_, mut corrupt) = ledger_with_projection_intent(temp.path());
        corrupt.receipt_snapshot = b"{}".to_vec();
        let report = drain_projection_intents(
            &FailingMutationLedger { intent: corrupt },
            &FixedIngress(Ok(EnqueueOutcome::Queued)),
            0,
        );
        assert_eq!(report.scanned, 1);
        assert_eq!(report.quarantined, 0);
        assert_eq!(report.failures().len(), 1);
        assert_eq!(
            report.failures()[0].kind,
            ProjectionDrainFailureKind::StateMutation
        );
        assert_eq!(
            report.diagnostic_error().as_deref(),
            Some("transition-projection-intent-drain-state-mutation")
        );

        let temp = tempfile::tempdir().expect("state");
        let (_, healthy) = ledger_with_projection_intent(temp.path());
        let report = drain_projection_intents(
            &FailingMutationLedger { intent: healthy },
            &FixedIngress(Ok(EnqueueOutcome::Queued)),
            0,
        );
        assert_eq!(report.projected, 0);
        assert_eq!(report.failures().len(), 1);
        assert_eq!(
            report.failures()[0].kind,
            ProjectionDrainFailureKind::StateMutation
        );
    }
}
