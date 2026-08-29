//! Singleton daemon bridge from exact shadow evidence to native wake custody.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::merge_steward::{StewardDecision, classify_shadow_summary};
use crate::shadow_scheduler::{ShadowObservation, ShadowTransitionEvidence};
use crate::work_ledger::{NativeStewardApplyReport, NativeStewardDisposition, WorkLedger};

/// Redacted producer state exposed by daemon status and persisted across restart.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ActionableWakeProducerStatus {
    pub(crate) state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pull_request: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason_code: Option<String>,
    pub(crate) wake_enqueued: bool,
    pub(crate) model_calls: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) updated_at: Option<String>,
}

impl Default for ActionableWakeProducerStatus {
    fn default() -> Self {
        Self {
            state: "idle".to_owned(),
            repository: None,
            pull_request: None,
            head_sha: None,
            reason_code: None,
            wake_enqueued: false,
            model_calls: 0,
            updated_at: None,
        }
    }
}

/// One daemon-owned, single-threaded producer. Repository failures are
/// reported per evidence item and never prevent later repositories running.
pub(crate) struct ActionableWakeProducer {
    state_dir: PathBuf,
    status: ActionableWakeProducerStatus,
}

impl ActionableWakeProducer {
    pub(crate) fn new(state_dir: PathBuf) -> Self {
        let status = load_status(&state_dir).unwrap_or_default();
        Self { state_dir, status }
    }

    pub(crate) fn status(&self) -> ActionableWakeProducerStatus {
        self.status.clone()
    }

    /// Reapply terminal stewardship recorded while the daemon was offline.
    /// The daemon calls this before every continuation consumer tick so a
    /// persisted pending wake cannot outrun a durable terminal decision after
    /// restart. One malformed repository is recorded and skipped without
    /// preventing later exact targets from being reconciled.
    pub(crate) fn reconcile_durable_terminals(&mut self) {
        let Ok(targets) = WorkLedger::open_existing(&self.state_dir).and_then(|ledger| {
            ledger.map_or_else(|| Ok(Vec::new()), |ledger| ledger.native_steward_targets())
        }) else {
            self.record(
                String::new(),
                0,
                String::new(),
                "error".to_owned(),
                Some("durable_terminal_reconciliation_unavailable".to_owned()),
                false,
            );
            return;
        };
        for (repository, pull_request, head_sha) in targets {
            match crate::app::exact_steward_transition(
                &self.state_dir,
                &repository,
                pull_request,
                &head_sha,
            ) {
                Ok(crate::app::ExactStewardTransition::Terminal) => {
                    self.apply(
                        &repository,
                        pull_request,
                        &head_sha,
                        NativeStewardDisposition::Superseded,
                        "steward_terminal_reconstructed",
                    );
                }
                Ok(_) => {}
                Err(_) => {
                    self.record(
                        repository,
                        pull_request,
                        head_sha,
                        "error".to_owned(),
                        Some("steward_authority_unavailable".to_owned()),
                        false,
                    );
                }
            }
        }
    }

    pub(crate) fn process(
        &mut self,
        evidence: &ShadowTransitionEvidence,
    ) -> ActionableWakeProducerStatus {
        let transition = &evidence.transition;
        let Some(observation) = transition.observation.as_ref() else {
            // Fetch failure is not terminal authority. Only the steward's
            // durable resolved transition may suppress already queued work;
            // transient GitHub failures remain state-preserving.
            return match crate::app::exact_steward_transition(
                &self.state_dir,
                &transition.repo,
                transition.pr,
                &transition.expected_head_sha,
            ) {
                Ok(crate::app::ExactStewardTransition::Terminal) => self.apply(
                    &transition.repo,
                    transition.pr,
                    &transition.expected_head_sha,
                    NativeStewardDisposition::Superseded,
                    "steward_terminal",
                ),
                Ok(_) => self.record(
                    transition.repo.clone(),
                    transition.pr,
                    transition.expected_head_sha.clone(),
                    "observed".to_owned(),
                    Some("fetch_transition".to_owned()),
                    false,
                ),
                Err(_) => self.record(
                    transition.repo.clone(),
                    transition.pr,
                    transition.expected_head_sha.clone(),
                    "error".to_owned(),
                    Some("steward_authority_unavailable".to_owned()),
                    false,
                ),
            };
        };
        self.process_observation(observation)
    }

    pub(crate) fn process_observation(
        &mut self,
        observation: &ShadowObservation,
    ) -> ActionableWakeProducerStatus {
        let decision = classify_shadow_summary(
            observation.exact_head,
            observation.pending_checks,
            observation.passed_checks,
            observation.failed_checks,
        );
        let Ok(steward) = crate::app::exact_steward_transition(
            &self.state_dir,
            &observation.repo,
            observation.pr,
            &observation.expected_head_sha,
        ) else {
            return self.record(
                observation.repo.clone(),
                observation.pr,
                observation.expected_head_sha.clone(),
                "error".to_owned(),
                Some("steward_authority_unavailable".to_owned()),
                false,
            );
        };
        let (disposition, reason) = if steward == crate::app::ExactStewardTransition::Terminal {
            (NativeStewardDisposition::Superseded, "steward_terminal")
        } else {
            match decision {
                StewardDecision::RequiredFailed { .. } => {
                    if steward == crate::app::ExactStewardTransition::Actionable {
                        (NativeStewardDisposition::Actionable, "required_failed")
                    } else {
                        (
                            NativeStewardDisposition::Waiting,
                            "failure_not_steward_actionable",
                        )
                    }
                }
                StewardDecision::WaitingRequired { .. } => {
                    (NativeStewardDisposition::Waiting, "waiting_required")
                }
                StewardDecision::ArmMergeQueue => (NativeStewardDisposition::Passing, "passing"),
                StewardDecision::NeedsUpdate { .. } => {
                    (NativeStewardDisposition::StaleHead, "stale_head")
                }
                _ => (NativeStewardDisposition::StaleHead, "non_actionable"),
            }
        };
        self.apply(
            &observation.repo,
            observation.pr,
            &observation.expected_head_sha,
            disposition,
            reason,
        )
    }

    fn apply(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        disposition: NativeStewardDisposition,
        reason: &str,
    ) -> ActionableWakeProducerStatus {
        let result = WorkLedger::open_existing(&self.state_dir).and_then(|ledger| {
            ledger.map_or_else(
                || {
                    Ok(NativeStewardApplyReport {
                        matched: false,
                        changed: false,
                        wake_enqueued: false,
                        phase: None,
                    })
                },
                |ledger| {
                    ledger.apply_native_steward_disposition(
                        repository,
                        pull_request,
                        head_sha,
                        disposition,
                    )
                },
            )
        });
        match result {
            Ok(report) => self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                if report.matched { "ready" } else { "unmatched" }.to_owned(),
                Some(reason.to_owned()),
                report.wake_enqueued,
            ),
            Err(_) => self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "error".to_owned(),
                Some("ledger_refused".to_owned()),
                false,
            ),
        }
    }

    fn record(
        &mut self,
        repository: String,
        pull_request: u64,
        head_sha: String,
        state: String,
        reason_code: Option<String>,
        wake_enqueued: bool,
    ) -> ActionableWakeProducerStatus {
        self.status = ActionableWakeProducerStatus {
            state,
            repository: Some(repository),
            pull_request: Some(pull_request),
            head_sha: Some(head_sha),
            reason_code,
            wake_enqueued,
            model_calls: 0,
            updated_at: Some(Utc::now().to_rfc3339()),
        };
        if save_status(&self.state_dir, &self.status).is_err() {
            self.status.state.clear();
            self.status.state.push_str("status_persistence_error");
            self.status.reason_code = Some("status_persistence_refused".to_owned());
            self.status.wake_enqueued = false;
        }
        self.status.clone()
    }
}

fn status_path(state_dir: &Path) -> PathBuf {
    state_dir
        .join("daemon")
        .join("actionable-wake-producer.json")
}

fn load_status(state_dir: &Path) -> Option<ActionableWakeProducerStatus> {
    const MAX_STATUS_BYTES: u64 = 64 * 1024;
    let path = status_path(state_dir);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.nlink() != 1
        || metadata.len() > MAX_STATUS_BYTES
        || metadata.mode() & 0o077 != 0
    {
        return None;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).ok()?);
    file.take(MAX_STATUS_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_STATUS_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn save_status(state_dir: &Path, status: &ActionableWakeProducerStatus) -> std::io::Result<()> {
    let directory = state_dir.join("daemon");
    crate::writer_domain_lease::ensure_protected_dir_all(&directory)?;
    let _writer = crate::writer_domain_lease::acquire_for_protected_path(&directory)?;
    let path = status_path(state_dir);
    let bytes = serde_json::to_vec(status).map_err(std::io::Error::other)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".actionable-wake-producer-")
        .suffix(".tmp")
        .tempfile_in(&directory)?;
    temporary.as_file_mut().set_len(0)?;
    temporary.as_file_mut().write_all(&bytes)?;
    temporary.as_file_mut().sync_all()?;
    crate::queue::replace_file_with_windows_retry(temporary.path(), &path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shadow_scheduler::{
        ShadowObservation, ShadowObservationTransition, ShadowObservationTransitionKind,
        ShadowTrigger,
    };
    use crate::work_ledger::{
        native_publication_test_policy as policy, native_publication_test_request as request,
    };

    fn evidence(failed_checks: u64) -> ShadowTransitionEvidence {
        let publication = request();
        let observation = ShadowObservation {
            repo: publication.repository.clone(),
            pr: publication.pull_request,
            expected_head_sha: publication.head_sha.clone(),
            work_items: 1,
            observed_head_sha: publication.head_sha.clone(),
            exact_head: true,
            snapshot_digest: "1".repeat(64),
            ledger_digest: "2".repeat(64),
            github_digest: "3".repeat(64),
            pending_checks: 0,
            passed_checks: u64::from(failed_checks == 0),
            failed_checks,
            policy_revision: 1,
            primary_platform: "macos".to_owned(),
            compatibility_mode: "independent".to_owned(),
            blocking_rule: "primary_only".to_owned(),
        };
        ShadowTransitionEvidence {
            transition: ShadowObservationTransition {
                kind: ShadowObservationTransitionKind::SnapshotChanged,
                repo: observation.repo.clone(),
                pr: observation.pr,
                expected_head_sha: observation.expected_head_sha.clone(),
                policy_revision: observation.policy_revision,
                observation: Some(observation),
                previous_snapshot_digest: None,
                failure_class: None,
            },
            trigger: ShadowTrigger::Webhook,
            api_requests: 1,
            fetch_errors: 0,
            elapsed_ms: 1,
        }
    }

    fn record_steward_transition(state_dir: &Path, phase: &str) {
        let publication = request();
        let record = serde_json::json!({
            "terminal_handoffs": {
                "exact-actionable": {
                    "dedupe_key": "exact-actionable",
                    "repo": publication.repository,
                    "base": publication.base_ref,
                    "pr_number": publication.pull_request,
                    "head_sha": publication.head_sha,
                    "outcome": "actionable_failure",
                    "trigger": "actionable_terminal_failure",
                    "next_action": "wake_exact_owner_for_causal_repair",
                    "owner_disposition": "exact_session",
                    "wake_consumer_available": false,
                    "failure_contexts": ["required_check:macos:app_id=1:conclusion=FAILURE:run_id=1"],
                    "phase": phase,
                    "created_at": "2026-08-29T00:00:00Z",
                    "updated_at": "2026-08-29T00:00:00Z"
                }
            }
        });
        std::fs::write(
            state_dir.join("merge-steward.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .expect("record actionable steward transition");
    }

    fn record_actionable(state_dir: &Path) {
        record_steward_transition(state_dir, "recorded");
    }

    #[test]
    fn daemon_producer_is_zero_model_durable_and_idempotent() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("publish managed handoff");
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());

        let aggregate_only = producer.process(&evidence(1));
        assert!(!aggregate_only.wake_enqueued);
        assert_eq!(
            aggregate_only.reason_code.as_deref(),
            Some("failure_not_steward_actionable")
        );
        record_actionable(state.path());
        let first = producer.process(&evidence(1));
        assert!(first.wake_enqueued);
        assert_eq!(first.model_calls, 0);
        let replay = producer.process(&evidence(1));
        assert!(!replay.wake_enqueued);
        assert_eq!(
            WorkLedger::open_existing(state.path())
                .unwrap()
                .unwrap()
                .status()
                .unwrap()
                .pending_wakes,
            1
        );

        record_steward_transition(state.path(), "resolved");
        let terminal = producer.process(&evidence(1));
        assert_eq!(terminal.reason_code.as_deref(), Some("steward_terminal"));
        let ledger = WorkLedger::open_existing(state.path()).unwrap().unwrap();
        assert_eq!(ledger.status().unwrap().pending_wakes, 0);

        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        assert_eq!(restarted.status(), terminal);
    }

    #[test]
    fn restart_reconstructs_durable_terminal_before_any_shadow_observation() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("publish managed handoff");
        record_actionable(state.path());
        let mut original = ActionableWakeProducer::new(state.path().to_path_buf());
        assert!(original.process(&evidence(1)).wake_enqueued);
        assert_eq!(
            WorkLedger::open_existing(state.path())
                .unwrap()
                .unwrap()
                .status()
                .unwrap()
                .pending_wakes,
            1
        );

        record_steward_transition(state.path(), "resolved");
        let mut restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        restarted.reconcile_durable_terminals();
        let ledger = WorkLedger::open_existing(state.path()).unwrap().unwrap();
        assert_eq!(
            ledger.status().unwrap().pending_wakes,
            0,
            "producer status: {:?}",
            restarted.status()
        );
        assert!(ledger.native_steward_targets().unwrap().is_empty());
        assert_eq!(
            restarted.status().reason_code.as_deref(),
            Some("steward_terminal_reconstructed")
        );
    }

    #[test]
    fn one_repository_error_does_not_disable_later_processing() {
        let state = tempfile::tempdir().expect("state");
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        let missing = producer.process(&evidence(1));
        assert_eq!(missing.state, "unmatched");

        let publication = request();
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("publish after unrelated miss");
        record_actionable(state.path());
        assert!(producer.process(&evidence(1)).wake_enqueued);
    }
}
