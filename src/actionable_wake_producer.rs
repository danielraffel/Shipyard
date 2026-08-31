//! Singleton daemon bridge from exact shadow evidence to native wake custody.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::dispatch_wedge::{
    DispatchWedgeInputs, DispatchWedgeObservation, DispatchWedgeState, assess_dispatch_wedge,
    dispatch_wedge_observation_digest, publish_dispatch_wedge,
};
use crate::merge_steward::{StewardDecision, classify_shadow_summary};
use crate::shadow_scheduler::{ShadowObservation, ShadowTransitionEvidence};
use crate::work_ledger::MAX_DISPATCH_PROBE_TARGETS;
use crate::work_ledger::{NativeStewardApplyReport, NativeStewardDisposition, WorkLedger};

const MAX_STATUS_BYTES: usize = 64 * 1024;

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
    #[serde(default)]
    pub(crate) repositories: BTreeMap<String, ActionableRepositoryStatus>,
    /// One bounded durable record per exact native-steward target. Observations,
    /// the next probe deadline, and the monotonic observer generation are
    /// published together so none can be silently evicted or observed mixed.
    #[serde(default, skip_serializing)]
    pub(crate) dispatch_targets: BTreeMap<String, DispatchTargetCheckpoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) updated_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ActionableRepositoryStatus {
    pub(crate) state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pull_request: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason_code: Option<String>,
    pub(crate) wake_enqueued: bool,
    pub(crate) updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DispatchProbeSchedule {
    #[serde(rename = "p")]
    pub(crate) repository_provider: String,
    #[serde(rename = "i")]
    pub(crate) repository_id: String,
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) head_sha: String,
    pub(crate) due_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DispatchObservationCheckpoint {
    pub(crate) digest: String,
    pub(crate) not_before: String,
    pub(crate) boot_epoch: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DispatchTargetCheckpoint {
    #[serde(rename = "p")]
    pub(crate) repository_provider: String,
    #[serde(rename = "i")]
    pub(crate) repository_id: String,
    pub(crate) repository: String,
    pub(crate) pull_request: u64,
    pub(crate) head_sha: String,
    #[serde(default)]
    pub(crate) generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) schedule: Option<DispatchProbeSchedule>,
    #[serde(default)]
    pub(crate) observations: BTreeMap<String, DispatchObservationCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pending_publication: Option<DispatchPendingPublication>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct DispatchTargetInventoryIdentity {
    repository_provider: String,
    repository_id: String,
    repository: String,
    pull_request: u64,
    head_sha: String,
}

impl DispatchTargetInventoryIdentity {
    pub(crate) fn new(
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
    ) -> Self {
        Self {
            repository_provider: repository_provider.to_owned(),
            repository_id: repository_id.to_owned(),
            repository: repository.to_ascii_lowercase(),
            pull_request,
            head_sha: head_sha.to_ascii_lowercase(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DispatchPendingPublication {
    #[serde(rename = "p")]
    pub(crate) repository_provider: String,
    #[serde(rename = "i")]
    pub(crate) repository_id: String,
    pub(crate) base_ref: String,
    pub(crate) observation_digest: String,
    pub(crate) dedupe_key: String,
    pub(crate) evidence_digest: String,
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
            repositories: BTreeMap::new(),
            dispatch_targets: BTreeMap::new(),
            updated_at: None,
        }
    }
}

/// One daemon-owned, single-threaded producer. Repository failures are
/// reported per evidence item and never prevent later repositories running.
pub(crate) struct ActionableWakeProducer {
    state_dir: PathBuf,
    status: ActionableWakeProducerStatus,
    boot_epoch: String,
    dispatch_state_available: bool,
    dispatch_state_digest: Option<String>,
}

impl ActionableWakeProducer {
    pub(crate) fn new(state_dir: PathBuf) -> Self {
        let mut status = load_status(&state_dir).unwrap_or_default();
        let legacy_dispatch_targets =
            canonicalize_dispatch_target_keys(std::mem::take(&mut status.dispatch_targets));
        let dispatch_state = legacy_dispatch_targets.and_then(|legacy_dispatch_targets| {
            WorkLedger::open(&state_dir)
                .map_err(|error| error.to_string())
                .and_then(|ledger| {
                    let records = ledger
                        .load_dispatch_probe_targets()
                        .map_err(|error| error.to_string())?;
                    if records.is_empty() && !legacy_dispatch_targets.is_empty() {
                        persist_dispatch_targets(&state_dir, &legacy_dispatch_targets)
                            .map_err(|error| error.to_string())?;
                        save_aggregate_status(&state_dir, &status)
                            .map_err(|error| error.to_string())?;
                        Ok(legacy_dispatch_targets)
                    } else {
                        let replacements = records
                            .iter()
                            .filter_map(|record| {
                                let canonical = dispatch_scope_prefix(
                                    &record.repository_provider,
                                    &record.repository_id,
                                    &record.repository,
                                    record.pull_request,
                                    &record.head_sha,
                                );
                                (record.target_key != canonical)
                                    .then(|| (record.target_key.clone(), canonical))
                            })
                            .collect::<BTreeMap<_, _>>();
                        let targets = decode_dispatch_target_records(records)?;
                        if !replacements.is_empty() {
                            ledger
                                .rekey_dispatch_probe_targets(&replacements)
                                .map_err(|error| error.to_string())?;
                        }
                        Ok(targets)
                    }
                })
        });
        let dispatch_state_available = if let Ok(targets) = dispatch_state {
            status.dispatch_targets = targets;
            true
        } else {
            "uncertain".clone_into(&mut status.state);
            status.reason_code = Some("dispatch_probe_state_unreadable".to_owned());
            status.dispatch_targets.clear();
            false
        };
        let dispatch_state_digest = dispatch_state_available
            .then(|| dispatch_targets_digest(&status.dispatch_targets))
            .transpose()
            .ok()
            .flatten();
        let seed = format!(
            "{}:{}:{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            state_dir.display()
        );
        let boot_epoch = format!("{:x}", sha2::Sha256::digest(seed.as_bytes()));
        Self {
            state_dir,
            status,
            boot_epoch,
            dispatch_state_available,
            dispatch_state_digest,
        }
    }

    pub(crate) fn status(&self) -> ActionableWakeProducerStatus {
        self.status.clone()
    }

    pub(crate) fn mark_in_flight(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
    ) -> ActionableWakeProducerStatus {
        self.record(
            repository.to_owned(),
            pull_request,
            head_sha.to_owned(),
            "in_flight",
            Some("daemon_exact_steward".to_owned()),
            false,
        )
    }

    pub(crate) fn mark_uncertain(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
    ) -> ActionableWakeProducerStatus {
        self.record(
            repository.to_owned(),
            pull_request,
            head_sha.to_owned(),
            "uncertain",
            Some("steward_result_uncertain".to_owned()),
            false,
        )
    }

    pub(crate) fn mark_ready(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
    ) -> ActionableWakeProducerStatus {
        self.record(
            repository.to_owned(),
            pull_request,
            head_sha.to_owned(),
            "ready",
            Some("steward_cycle_complete".to_owned()),
            false,
        )
    }

    pub(crate) fn mark_disabled(
        &mut self,
        repository: &str,
        reason: &str,
    ) -> ActionableWakeProducerStatus {
        self.record(
            repository.to_owned(),
            0,
            String::new(),
            "disabled",
            Some(reason.to_owned()),
            false,
        )
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
                "error",
                Some("durable_terminal_reconciliation_unavailable".to_owned()),
                false,
            );
            return;
        };
        for (repository_provider, repository_id, repository, pull_request, head_sha) in targets {
            match crate::app::exact_steward_transition(
                &self.state_dir,
                repository_provider.as_deref(),
                repository_id.as_deref(),
                &repository,
                pull_request,
                &head_sha,
            ) {
                Ok(crate::app::ExactStewardTransition::Terminal) => {
                    self.apply(
                        repository_provider.as_deref(),
                        repository_id.as_deref(),
                        &repository,
                        pull_request,
                        &head_sha,
                        NativeStewardDisposition::Superseded,
                        "steward_terminal_reconstructed",
                    );
                }
                Ok(crate::app::ExactStewardTransition::Actionable) => {
                    self.apply(
                        repository_provider.as_deref(),
                        repository_id.as_deref(),
                        &repository,
                        pull_request,
                        &head_sha,
                        NativeStewardDisposition::Actionable,
                        "steward_actionable_reconstructed",
                    );
                }
                Ok(_) => {}
                Err(_) => {
                    self.record(
                        repository,
                        pull_request,
                        head_sha,
                        "error",
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
            if transition.failure_class.as_deref() == Some("repository_identity") {
                return self.record(
                    transition.repo.clone(),
                    transition.pr,
                    transition.expected_head_sha.clone(),
                    "error",
                    Some("repository_identity_mismatch".to_owned()),
                    false,
                );
            }
            // Fetch failure is not terminal authority. Only the steward's
            // durable resolved transition may suppress already queued work;
            // transient GitHub failures remain state-preserving.
            return match crate::app::exact_steward_transition(
                &self.state_dir,
                transition.repository_provider.as_deref(),
                transition.repository_id.as_deref(),
                &transition.repo,
                transition.pr,
                &transition.expected_head_sha,
            ) {
                Ok(crate::app::ExactStewardTransition::Terminal) => self.apply(
                    transition.repository_provider.as_deref(),
                    transition.repository_id.as_deref(),
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
                    "observed",
                    Some("fetch_transition".to_owned()),
                    false,
                ),
                Err(_) => self.record(
                    transition.repo.clone(),
                    transition.pr,
                    transition.expected_head_sha.clone(),
                    "error",
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
            observation.repository_provider.as_deref(),
            observation.repository_id.as_deref(),
            &observation.repo,
            observation.pr,
            &observation.expected_head_sha,
        ) else {
            return self.record(
                observation.repo.clone(),
                observation.pr,
                observation.expected_head_sha.clone(),
                "error",
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
            observation.repository_provider.as_deref(),
            observation.repository_id.as_deref(),
            &observation.repo,
            observation.pr,
            &observation.expected_head_sha,
            disposition,
            reason,
        )
    }

    /// Classify one exact daemon observer result and, only after two matching
    /// cycles, publish its wake through the existing `WorkLedger` transaction.
    #[allow(
        clippy::too_many_lines,
        reason = "checkpoint and WorkLedger publication remain one auditable fail-closed boundary"
    )]
    #[cfg(test)]
    pub(crate) fn process_dispatch_wedge_observation(
        &mut self,
        observation: &DispatchWedgeObservation,
        assignment_threshold_secs: i64,
    ) -> ActionableWakeProducerStatus {
        self.process_dispatch_wedge_observation_for_repository(
            test_repository_provider(),
            test_repository_id(),
            observation,
            assignment_threshold_secs,
        )
    }

    #[allow(
        clippy::too_many_lines,
        reason = "checkpoint and WorkLedger publication remain one auditable fail-closed boundary"
    )]
    fn process_dispatch_wedge_observation_for_repository(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        observation: &DispatchWedgeObservation,
        assignment_threshold_secs: i64,
    ) -> ActionableWakeProducerStatus {
        let authority = &observation.authority;
        if !self.dispatch_state_available {
            return self.record(
                authority.repository.clone(),
                authority.pull_request,
                authority.pull_request_head.clone(),
                "uncertain",
                Some("dispatch_probe_state_unreadable".to_owned()),
                false,
            );
        }
        let key = dispatch_observation_key(authority);
        let scope = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            &authority.repository,
            authority.pull_request,
            &authority.pull_request_head,
        );
        if !self.status.dispatch_targets.contains_key(&scope)
            && self.status.dispatch_targets.len() >= MAX_DISPATCH_PROBE_TARGETS
        {
            return self.record(
                authority.repository.clone(),
                authority.pull_request,
                authority.pull_request_head.clone(),
                "uncertain",
                Some("dispatch_probe_capacity_exhausted".to_owned()),
                false,
            );
        }
        let digest = dispatch_wedge_observation_digest(authority, &observation.runners);
        let now = Utc::now();
        let pending_digest = self
            .status
            .dispatch_targets
            .get(&scope)
            .and_then(|target| target.pending_publication.as_ref())
            .filter(|pending| pending.observation_digest == digest)
            .map(|pending| pending.observation_digest.clone());
        let checkpoint_digest = self
            .status
            .dispatch_targets
            .get(&scope)
            .and_then(|target| target.observations.get(&key))
            .filter(|checkpoint| checkpoint.boot_epoch == self.boot_epoch)
            .filter(|checkpoint| {
                chrono::DateTime::parse_from_rfc3339(&checkpoint.not_before)
                    .is_ok_and(|due| due.with_timezone(&Utc) <= now)
            })
            .map(|checkpoint| checkpoint.digest.clone());
        let previous = pending_digest.clone().or_else(|| checkpoint_digest.clone());
        let assessment = assess_dispatch_wedge(&DispatchWedgeInputs {
            authority,
            runners: &observation.runners,
            observation_complete: observation.observation_complete,
            previous_observation_digest: previous.as_deref(),
            assignment_threshold_secs,
            now,
        });
        if assessment.state == DispatchWedgeState::DispatchWedge {
            let Some(evidence) = assessment.evidence.clone() else {
                return self.record(
                    authority.repository.clone(),
                    authority.pull_request,
                    authority.pull_request_head.clone(),
                    "uncertain",
                    Some("dispatch_wedge_evidence_missing".to_owned()),
                    false,
                );
            };
            if pending_digest.is_some()
                && checkpoint_digest.is_none()
                && self
                    .status
                    .dispatch_targets
                    .get(&scope)
                    .and_then(|target| target.pending_publication.as_ref())
                    .is_some_and(|pending| {
                        pending.dedupe_key != evidence.dedupe_key
                            || pending.evidence_digest != evidence.evidence_digest
                    })
            {
                if let Some(target) = self.status.dispatch_targets.get_mut(&scope) {
                    target.pending_publication = None;
                }
                return self.record(
                    authority.repository.clone(),
                    authority.pull_request,
                    authority.pull_request_head.clone(),
                    "uncertain",
                    Some("dispatch_wedge_pending_publication_mismatch".to_owned()),
                    false,
                );
            }
            let Some(target) = self.status.dispatch_targets.get_mut(&scope) else {
                return self.record(
                    authority.repository.clone(),
                    authority.pull_request,
                    authority.pull_request_head.clone(),
                    "uncertain",
                    Some("dispatch_wedge_checkpoint_missing".to_owned()),
                    false,
                );
            };
            let repository_provider = target.repository_provider.clone();
            let repository_id = target.repository_id.clone();
            let previous_pending = target.pending_publication.clone();
            target.pending_publication = Some(DispatchPendingPublication {
                repository_provider: repository_provider.clone(),
                repository_id: repository_id.clone(),
                base_ref: authority.base_ref.clone(),
                observation_digest: evidence.observation_digest,
                dedupe_key: evidence.dedupe_key,
                evidence_digest: evidence.evidence_digest,
            });
            if self.persist_status().is_err() {
                if let Some(target) = self.status.dispatch_targets.get_mut(&scope) {
                    target.pending_publication = previous_pending;
                }
                return self.record(
                    authority.repository.clone(),
                    authority.pull_request,
                    authority.pull_request_head.clone(),
                    "uncertain",
                    Some("dispatch_wedge_pending_publication_failed".to_owned()),
                    false,
                );
            }
            let publication = WorkLedger::open_existing(&self.state_dir).and_then(|ledger| {
                ledger.map_or_else(
                    || {
                        Err(crate::work_ledger::WorkLedgerError::Refused(
                            "dispatch wedge has no existing work ledger".to_owned(),
                        ))
                    },
                    |ledger| {
                        publish_dispatch_wedge(
                            &ledger,
                            nonempty_identity(&repository_provider),
                            nonempty_identity(&repository_id),
                            &assessment,
                        )
                        .map_err(crate::work_ledger::WorkLedgerError::Refused)
                    },
                )
            });
            return match publication {
                Ok(Some(receipt)) if receipt.matched => {
                    let pending = self.status.dispatch_targets.remove(&scope);
                    if self.persist_status().is_err() {
                        if let Some(pending) = pending {
                            self.status.dispatch_targets.insert(scope, pending);
                        }
                        self.record(
                            authority.repository.clone(),
                            authority.pull_request,
                            authority.pull_request_head.clone(),
                            "uncertain",
                            Some("dispatch_wedge_cleanup_failed".to_owned()),
                            false,
                        )
                    } else {
                        self.record(
                            authority.repository.clone(),
                            authority.pull_request,
                            authority.pull_request_head.clone(),
                            "ready",
                            Some("dispatch_wedge".to_owned()),
                            receipt.wake_enqueued,
                        )
                    }
                }
                Ok(_) => self.record(
                    authority.repository.clone(),
                    authority.pull_request,
                    authority.pull_request_head.clone(),
                    "uncertain",
                    Some("dispatch_wedge_unmatched".to_owned()),
                    false,
                ),
                Err(_) => self.record(
                    authority.repository.clone(),
                    authority.pull_request,
                    authority.pull_request_head.clone(),
                    "uncertain",
                    Some("dispatch_wedge_publication_refused".to_owned()),
                    false,
                ),
            };
        }
        if observation.observation_complete {
            let target = self
                .status
                .dispatch_targets
                .entry(scope.clone())
                .or_insert_with(|| DispatchTargetCheckpoint {
                    repository_provider: repository_provider.to_owned(),
                    repository_id: repository_id.to_owned(),
                    repository: authority.repository.clone(),
                    pull_request: authority.pull_request,
                    head_sha: authority.pull_request_head.clone(),
                    generation: 0,
                    schedule: None,
                    observations: BTreeMap::new(),
                    pending_publication: None,
                });
            let not_before = target
                .observations
                .get(&key)
                .filter(|checkpoint| checkpoint.digest == digest)
                .map_or_else(
                    || {
                        (now + chrono::Duration::seconds(
                            assignment_threshold_secs.max(stability_delay_seconds()),
                        ))
                        .to_rfc3339()
                    },
                    |checkpoint| checkpoint.not_before.clone(),
                );
            target.observations.insert(
                key.clone(),
                DispatchObservationCheckpoint {
                    digest,
                    not_before,
                    boot_epoch: self.boot_epoch.clone(),
                },
            );
            if self.persist_status().is_err() {
                if let Some(target) = self.status.dispatch_targets.get_mut(&scope) {
                    target.observations.remove(&key);
                }
                self.prune_empty_dispatch_target(&scope);
                return self.record(
                    authority.repository.clone(),
                    authority.pull_request,
                    authority.pull_request_head.clone(),
                    "uncertain",
                    Some("dispatch_wedge_checkpoint_failed".to_owned()),
                    false,
                );
            }
        }
        let state = match assessment.state {
            DispatchWedgeState::Indeterminate => "uncertain",
            DispatchWedgeState::Waiting => "in_flight",
            _ => "ready",
        };
        self.record(
            authority.repository.clone(),
            authority.pull_request,
            authority.pull_request_head.clone(),
            state,
            Some(assessment.reason),
            false,
        )
    }

    /// Apply one complete target-scoped observer cycle. Keys absent from this
    /// read are invalidated before any present observation can match, so
    /// `A -> empty/replacement -> A` never counts as two consecutive reads.
    #[cfg(test)]
    pub(crate) fn process_dispatch_wedge_cycle(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        observations: &[DispatchWedgeObservation],
        assignment_threshold_secs: i64,
    ) -> ActionableWakeProducerStatus {
        let Some(generation) = self.begin_dispatch_wedge_cycle(repository, pull_request, head_sha)
        else {
            return self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_wedge_cycle_start_failed".to_owned()),
                false,
            );
        };
        let status = self.process_dispatch_wedge_cycle_at_generation_for_repository(
            test_repository_provider(),
            test_repository_id(),
            repository,
            pull_request,
            head_sha,
            generation,
            observations,
            assignment_threshold_secs,
        );
        if status.reason_code.as_deref() == Some("dispatch_wedge_candidate_absent") {
            self.finish_dispatch_cycle_without_followup_for_repository(
                test_repository_provider(),
                test_repository_id(),
                repository,
                pull_request,
                head_sha,
            );
        }
        status
    }

    #[cfg(test)]
    pub(crate) fn begin_dispatch_wedge_cycle(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
    ) -> Option<u64> {
        self.begin_dispatch_wedge_cycle_for_repository(
            test_repository_provider(),
            test_repository_id(),
            repository,
            pull_request,
            head_sha,
        )
    }

    pub(crate) fn begin_dispatch_wedge_cycle_for_repository(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
    ) -> Option<u64> {
        if !self.dispatch_state_available {
            self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_probe_state_unreadable".to_owned()),
                false,
            );
            return None;
        }
        let key = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
        );
        if !self.status.dispatch_targets.contains_key(&key)
            && self.status.dispatch_targets.len() >= MAX_DISPATCH_PROBE_TARGETS
        {
            self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_probe_capacity_exhausted".to_owned()),
                false,
            );
            return None;
        }
        let previous = self.status.dispatch_targets.get(&key).cloned();
        let target = self
            .status
            .dispatch_targets
            .entry(key.clone())
            .or_insert_with(|| DispatchTargetCheckpoint {
                repository_provider: repository_provider.to_owned(),
                repository_id: repository_id.to_owned(),
                repository: repository.to_owned(),
                pull_request,
                head_sha: head_sha.to_owned(),
                generation: 0,
                schedule: None,
                observations: BTreeMap::new(),
                pending_publication: None,
            });
        if target.repository_provider != repository_provider
            || target.repository_id != repository_id
        {
            return None;
        }
        let generation = target.generation.checked_add(1)?;
        target.generation = generation;
        if self.persist_status().is_err() {
            if let Some(previous) = previous {
                self.status.dispatch_targets.insert(key, previous);
            } else {
                self.status.dispatch_targets.remove(&key);
            }
            return None;
        }
        Some(generation)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "repository identity and exact target generation are independent mutation fences"
    )]
    pub(crate) fn process_dispatch_wedge_cycle_at_generation_for_repository(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        generation: u64,
        observations: &[DispatchWedgeObservation],
        assignment_threshold_secs: i64,
    ) -> ActionableWakeProducerStatus {
        let prefix = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
        );
        if self
            .status
            .dispatch_targets
            .get(&prefix)
            .is_none_or(|target| target.generation != generation)
        {
            return self.status();
        }
        if self
            .status
            .dispatch_targets
            .get(&prefix)
            .is_some_and(|target| target.pending_publication.is_some())
        {
            return self.publish_pending_dispatch_wedge(
                repository_provider,
                repository_id,
                repository,
                pull_request,
                head_sha,
            );
        }
        let present = observations
            .iter()
            .map(|observation| dispatch_observation_key(&observation.authority))
            .collect::<std::collections::BTreeSet<_>>();
        if let Some(target) = self.status.dispatch_targets.get_mut(&prefix) {
            target.observations.retain(|key, _| present.contains(key));
        }
        let mut status = None;
        let mut processed = std::collections::BTreeSet::new();
        for observation in observations {
            if !processed.insert(dispatch_observation_key(&observation.authority)) {
                continue;
            }
            let observed = self.process_dispatch_wedge_observation_for_repository(
                repository_provider,
                repository_id,
                observation,
                assignment_threshold_secs,
            );
            let terminal_dispatch = matches!(
                observed.reason_code.as_deref(),
                Some(
                    "dispatch_wedge"
                        | "dispatch_wedge_unmatched"
                        | "dispatch_wedge_publication_refused"
                        | "dispatch_wedge_cleanup_failed"
                )
            );
            let observed_priority = dispatch_cycle_status_priority(&observed);
            if status
                .as_ref()
                .is_none_or(|current| observed_priority > dispatch_cycle_status_priority(current))
            {
                status = Some(observed);
            }
            if terminal_dispatch {
                break;
            }
        }
        status.unwrap_or_else(|| {
            self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "ready",
                Some("dispatch_wedge_candidate_absent".to_owned()),
                false,
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn process_dispatch_wedge_cycle_at_generation(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        generation: u64,
        observations: &[DispatchWedgeObservation],
        assignment_threshold_secs: i64,
    ) -> ActionableWakeProducerStatus {
        self.process_dispatch_wedge_cycle_at_generation_for_repository(
            test_repository_provider(),
            test_repository_id(),
            repository,
            pull_request,
            head_sha,
            generation,
            observations,
            assignment_threshold_secs,
        )
    }

    pub(crate) fn invalidate_dispatch_wedge_scope_for_repository(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        reason: &str,
    ) -> ActionableWakeProducerStatus {
        if !self.dispatch_state_available {
            return self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_probe_state_unreadable".to_owned()),
                false,
            );
        }
        let prefix = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
        );
        if self
            .status
            .dispatch_targets
            .get(&prefix)
            .is_some_and(|target| target.pending_publication.is_some())
        {
            return self.publish_pending_dispatch_wedge(
                repository_provider,
                repository_id,
                repository,
                pull_request,
                head_sha,
            );
        }
        let previous = self.status.dispatch_targets.remove(&prefix);
        let status = self.record(
            repository.to_owned(),
            pull_request,
            head_sha.to_owned(),
            "uncertain",
            Some(reason.to_owned()),
            false,
        );
        if status.state == "status_persistence_error"
            && let Some(previous) = previous
        {
            self.status.dispatch_targets.insert(prefix, previous);
        }
        status
    }

    #[cfg(test)]
    pub(crate) fn invalidate_dispatch_wedge_scope(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        reason: &str,
    ) -> ActionableWakeProducerStatus {
        self.invalidate_dispatch_wedge_scope_for_repository(
            test_repository_provider(),
            test_repository_id(),
            repository,
            pull_request,
            head_sha,
            reason,
        )
    }

    fn publish_pending_dispatch_wedge(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
    ) -> ActionableWakeProducerStatus {
        let key = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
        );
        let Some(pending) = self
            .status
            .dispatch_targets
            .get(&key)
            .and_then(|target| target.pending_publication.clone())
        else {
            return self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_wedge_pending_publication_missing".to_owned()),
                false,
            );
        };
        if !pending
            .repository_provider
            .eq_ignore_ascii_case(repository_provider)
            || pending.repository_id != repository_id
        {
            return self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_wedge_pending_publication_identity_mismatch".to_owned()),
                false,
            );
        }
        let publication = WorkLedger::open_existing(&self.state_dir).and_then(|ledger| {
            ledger.map_or_else(
                || {
                    Err(crate::work_ledger::WorkLedgerError::Refused(
                        "dispatch wedge has no existing work ledger".to_owned(),
                    ))
                },
                |ledger| {
                    ledger.publish_dispatch_wedge(
                        nonempty_identity(&pending.repository_provider),
                        nonempty_identity(&pending.repository_id),
                        repository,
                        &pending.base_ref,
                        pull_request,
                        head_sha,
                        &pending.dedupe_key,
                        &pending.evidence_digest,
                    )
                },
            )
        });
        match publication {
            Ok(receipt) if receipt.matched => {
                let target = self.status.dispatch_targets.remove(&key);
                if self.persist_status().is_err() {
                    if let Some(target) = target {
                        self.status.dispatch_targets.insert(key, target);
                    }
                    self.record(
                        repository.to_owned(),
                        pull_request,
                        head_sha.to_owned(),
                        "uncertain",
                        Some("dispatch_wedge_cleanup_failed".to_owned()),
                        false,
                    )
                } else {
                    self.record(
                        repository.to_owned(),
                        pull_request,
                        head_sha.to_owned(),
                        "ready",
                        Some("dispatch_wedge".to_owned()),
                        receipt.wake_enqueued,
                    )
                }
            }
            Ok(_) => self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_wedge_unmatched".to_owned()),
                false,
            ),
            Err(_) => self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_wedge_publication_refused".to_owned()),
                false,
            ),
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "repository identity and exact target generation are independent mutation fences"
    )]
    pub(crate) fn invalidate_dispatch_wedge_scope_at_generation_for_repository(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        generation: u64,
        reason: &str,
    ) -> ActionableWakeProducerStatus {
        let key = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
        );
        if self
            .status
            .dispatch_targets
            .get(&key)
            .is_some_and(|target| target.generation == generation)
        {
            return self.invalidate_dispatch_wedge_scope_for_repository(
                repository_provider,
                repository_id,
                repository,
                pull_request,
                head_sha,
                reason,
            );
        }
        self.status()
    }

    #[cfg(test)]
    pub(crate) fn invalidate_dispatch_wedge_scope_at_generation(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        generation: u64,
        reason: &str,
    ) -> ActionableWakeProducerStatus {
        self.invalidate_dispatch_wedge_scope_at_generation_for_repository(
            test_repository_provider(),
            test_repository_id(),
            repository,
            pull_request,
            head_sha,
            generation,
            reason,
        )
    }

    pub(crate) fn dispatch_cycle_generation_current_for_repository(
        &self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        generation: u64,
    ) -> bool {
        let key = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
        );
        self.status
            .dispatch_targets
            .get(&key)
            .is_some_and(|target| target.generation == generation)
    }

    #[cfg(test)]
    pub(crate) fn dispatch_cycle_generation_current(
        &self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        generation: u64,
    ) -> bool {
        self.dispatch_cycle_generation_current_for_repository(
            test_repository_provider(),
            test_repository_id(),
            repository,
            pull_request,
            head_sha,
            generation,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reschedule_dispatch_probe_after_failure_at_generation(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        generation: u64,
        due_at: chrono::DateTime<Utc>,
        reason: &str,
    ) -> ActionableWakeProducerStatus {
        self.reschedule_dispatch_probe_at_generation(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
            generation,
            due_at,
            "uncertain",
            reason,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reschedule_dispatch_probe_after_auxiliary_failure_at_generation(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        generation: u64,
        due_at: chrono::DateTime<Utc>,
    ) -> ActionableWakeProducerStatus {
        self.reschedule_dispatch_probe_at_generation(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
            generation,
            due_at,
            "ready",
            "dispatch_wedge_observation_retry_scheduled",
        )
    }

    pub(crate) fn retain_dispatch_scope_after_steward_failure_at_generation_for_repository(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        generation: u64,
    ) -> ActionableWakeProducerStatus {
        if !self.dispatch_state_available {
            return self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_probe_state_unreadable".to_owned()),
                false,
            );
        }
        let key = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
        );
        let previous = self.status.dispatch_targets.get(&key).cloned();
        let Some(target) = self.status.dispatch_targets.get_mut(&key) else {
            return self.status();
        };
        if target.generation != generation {
            return self.status();
        }
        if target.pending_publication.is_some() {
            return self.publish_pending_dispatch_wedge(
                repository_provider,
                repository_id,
                repository,
                pull_request,
                head_sha,
            );
        }
        target.observations.clear();
        target.schedule = None;
        let status = self.record(
            repository.to_owned(),
            pull_request,
            head_sha.to_owned(),
            "uncertain",
            Some("steward_retry_scheduled".to_owned()),
            false,
        );
        if status.state == "status_persistence_error"
            && let Some(previous) = previous
        {
            self.status.dispatch_targets.insert(key, previous);
        }
        status
    }

    #[cfg(test)]
    pub(crate) fn retain_dispatch_scope_after_steward_failure_at_generation(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        generation: u64,
    ) -> ActionableWakeProducerStatus {
        self.retain_dispatch_scope_after_steward_failure_at_generation_for_repository(
            test_repository_provider(),
            test_repository_id(),
            repository,
            pull_request,
            head_sha,
            generation,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn reschedule_dispatch_probe_at_generation(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        generation: u64,
        due_at: chrono::DateTime<Utc>,
        state: &str,
        reason: &str,
    ) -> ActionableWakeProducerStatus {
        if !self.dispatch_state_available {
            return self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_probe_state_unreadable".to_owned()),
                false,
            );
        }
        let key = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
        );
        let previous = self.status.dispatch_targets.get(&key).cloned();
        let Some(target) = self.status.dispatch_targets.get_mut(&key) else {
            return self.status();
        };
        if target.generation != generation
            || target.repository_provider != repository_provider
            || target.repository_id != repository_id
        {
            return self.status();
        }
        if target.pending_publication.is_some() {
            return self.publish_pending_dispatch_wedge(
                repository_provider,
                repository_id,
                repository,
                pull_request,
                head_sha,
            );
        }
        target.observations.clear();
        target.schedule = Some(DispatchProbeSchedule {
            repository_provider: repository_provider.to_owned(),
            repository_id: repository_id.to_owned(),
            repository: repository.to_owned(),
            pull_request,
            head_sha: head_sha.to_owned(),
            due_at: due_at.to_rfc3339(),
        });
        let status = self.record(
            repository.to_owned(),
            pull_request,
            head_sha.to_owned(),
            state,
            Some(reason.to_owned()),
            false,
        );
        if status.state == "status_persistence_error"
            && let Some(previous) = previous
        {
            self.status.dispatch_targets.insert(key, previous);
        }
        status
    }

    #[cfg(test)]
    pub(crate) fn schedule_dispatch_probe(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        due_at: chrono::DateTime<Utc>,
    ) -> ActionableWakeProducerStatus {
        self.schedule_dispatch_probe_for_repository(
            test_repository_provider(),
            test_repository_id(),
            repository,
            pull_request,
            head_sha,
            due_at,
        )
    }

    pub(crate) fn schedule_dispatch_probe_for_repository(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
        due_at: chrono::DateTime<Utc>,
    ) -> ActionableWakeProducerStatus {
        if !self.dispatch_state_available {
            return self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_probe_state_unreadable".to_owned()),
                false,
            );
        }
        let key = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
        );
        if !self.status.dispatch_targets.contains_key(&key)
            && self.status.dispatch_targets.len() >= MAX_DISPATCH_PROBE_TARGETS
        {
            return self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "uncertain",
                Some("dispatch_probe_capacity_exhausted".to_owned()),
                false,
            );
        }
        let previous = self.status.dispatch_targets.get(&key).cloned();
        let target = self
            .status
            .dispatch_targets
            .entry(key.clone())
            .or_insert_with(|| DispatchTargetCheckpoint {
                repository_provider: repository_provider.to_owned(),
                repository_id: repository_id.to_owned(),
                repository: repository.to_owned(),
                pull_request,
                head_sha: head_sha.to_owned(),
                generation: 0,
                schedule: None,
                observations: BTreeMap::new(),
                pending_publication: None,
            });
        if target.repository_provider != repository_provider
            || target.repository_id != repository_id
        {
            return self.status();
        }
        target.schedule = Some(DispatchProbeSchedule {
            repository_provider: repository_provider.to_owned(),
            repository_id: repository_id.to_owned(),
            repository: repository.to_owned(),
            pull_request,
            head_sha: head_sha.to_owned(),
            due_at: due_at.to_rfc3339(),
        });
        let status = self.record(
            repository.to_owned(),
            pull_request,
            head_sha.to_owned(),
            "in_flight",
            Some("dispatch_wedge_second_read_scheduled".to_owned()),
            false,
        );
        if status.state == "status_persistence_error" {
            if let Some(previous) = previous {
                self.status.dispatch_targets.insert(key, previous);
            } else {
                self.status.dispatch_targets.remove(&key);
            }
        }
        status
    }

    pub(crate) fn finish_dispatch_cycle_without_followup_for_repository(
        &mut self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
    ) -> bool {
        if !self.dispatch_state_available {
            return false;
        }
        let key = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
        );
        let previous = self.status.dispatch_targets.remove(&key);
        if self.persist_status().is_ok() {
            return true;
        }
        if let Some(previous) = previous {
            self.status.dispatch_targets.insert(key, previous);
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn finish_dispatch_cycle_without_followup(
        &mut self,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
    ) -> bool {
        self.finish_dispatch_cycle_without_followup_for_repository(
            test_repository_provider(),
            test_repository_id(),
            repository,
            pull_request,
            head_sha,
        )
    }

    pub(crate) fn due_dispatch_probes(
        &self,
        now: chrono::DateTime<Utc>,
        limit: usize,
    ) -> Vec<DispatchProbeSchedule> {
        if !self.dispatch_state_available {
            return Vec::new();
        }
        let mut due = self
            .status
            .dispatch_targets
            .values()
            .filter_map(|target| target.schedule.as_ref())
            .filter(|schedule| {
                chrono::DateTime::parse_from_rfc3339(&schedule.due_at)
                    .is_ok_and(|due| due.with_timezone(&Utc) <= now)
            })
            .cloned()
            .collect::<Vec<_>>();
        due.sort_by(|left, right| {
            left.due_at.cmp(&right.due_at).then_with(|| {
                dispatch_scope_prefix(
                    &left.repository_provider,
                    &left.repository_id,
                    &left.repository,
                    left.pull_request,
                    &left.head_sha,
                )
                .cmp(&dispatch_scope_prefix(
                    &right.repository_provider,
                    &right.repository_id,
                    &right.repository,
                    right.pull_request,
                    &right.head_sha,
                ))
            })
        });
        due.truncate(limit);
        due
    }

    pub(crate) fn dispatch_second_read_due_at_for_repository(
        &self,
        repository_provider: &str,
        repository_id: &str,
        repository: &str,
        pull_request: u64,
        head_sha: &str,
    ) -> Option<chrono::DateTime<Utc>> {
        let key = dispatch_scope_prefix(
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
        );
        self.status
            .dispatch_targets
            .get(&key)?
            .observations
            .values()
            .filter_map(|checkpoint| {
                chrono::DateTime::parse_from_rfc3339(&checkpoint.not_before)
                    .ok()
                    .map(|due| due.with_timezone(&Utc))
            })
            .min()
    }

    pub(crate) fn retain_dispatch_targets(
        &mut self,
        active: &std::collections::BTreeSet<DispatchTargetInventoryIdentity>,
    ) -> ActionableWakeProducerStatus {
        if !self.dispatch_state_available {
            return self.status();
        }
        let stale_keys = self
            .status
            .dispatch_targets
            .iter()
            .filter(|(_, target)| {
                !active.contains(&DispatchTargetInventoryIdentity::new(
                    &target.repository_provider,
                    &target.repository_id,
                    &target.repository,
                    target.pull_request,
                    &target.head_sha,
                ))
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        if stale_keys.is_empty() {
            return self.status();
        }
        let removed = stale_keys
            .into_iter()
            .filter_map(|key| {
                self.status
                    .dispatch_targets
                    .remove(&key)
                    .map(|target| (key, target))
            })
            .collect::<Vec<_>>();
        if self.persist_status().is_err() {
            self.status.dispatch_targets.extend(removed);
            "status_persistence_error".clone_into(&mut self.status.state);
            self.status.reason_code = Some("status_persistence_refused".to_owned());
            self.status.wake_enqueued = false;
        }
        self.status()
    }

    fn prune_empty_dispatch_target(&mut self, key: &str) {
        if self.status.dispatch_targets.get(key).is_some_and(|target| {
            target.schedule.is_none()
                && target.observations.is_empty()
                && target.pending_publication.is_none()
                && target.generation == 0
        }) {
            self.status.dispatch_targets.remove(key);
        }
    }

    #[allow(clippy::too_many_arguments)] // Exact repository identity is part of the mutation fence.
    fn apply(
        &mut self,
        repository_provider: Option<&str>,
        repository_id: Option<&str>,
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
                    ledger.apply_native_steward_disposition_for_repository(
                        repository_provider,
                        repository_id,
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
                if report.matched { "ready" } else { "unmatched" },
                Some(reason.to_owned()),
                report.wake_enqueued,
            ),
            Err(_) => self.record(
                repository.to_owned(),
                pull_request,
                head_sha.to_owned(),
                "error",
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
        state: &str,
        reason_code: Option<String>,
        wake_enqueued: bool,
    ) -> ActionableWakeProducerStatus {
        const MAX_REPOSITORIES: usize = 256;
        let (state, reason_code, wake_enqueued) = if self.dispatch_state_available {
            (state, reason_code, wake_enqueued)
        } else {
            (
                "uncertain",
                Some("dispatch_probe_state_unreadable".to_owned()),
                false,
            )
        };
        let updated_at = Utc::now().to_rfc3339();
        if !self.status.repositories.contains_key(&repository)
            && self.status.repositories.len() >= MAX_REPOSITORIES
        {
            let oldest = self
                .status
                .repositories
                .iter()
                .min_by_key(|(_, value)| &value.updated_at)
                .map(|(repo, _)| repo.clone());
            if let Some(oldest) = oldest {
                self.status.repositories.remove(&oldest);
            }
        }
        if repository.is_empty() {
            normalize_state(state).clone_into(&mut self.status.state);
            self.status.repository = None;
        } else {
            self.status.repositories.insert(
                repository.clone(),
                ActionableRepositoryStatus {
                    state: normalize_state(state).to_owned(),
                    pull_request: (pull_request > 0).then_some(pull_request),
                    head_sha: (!head_sha.is_empty()).then_some(head_sha.clone()),
                    reason_code: reason_code.clone(),
                    wake_enqueued,
                    updated_at: updated_at.clone(),
                },
            );
            self.status.state = aggregate_state(&self.status.repositories).to_owned();
            self.status.repository = Some(repository);
        }
        self.status.pull_request = (pull_request > 0).then_some(pull_request);
        self.status.head_sha = (!head_sha.is_empty()).then_some(head_sha);
        self.status.reason_code = reason_code;
        self.status.wake_enqueued = wake_enqueued;
        self.status.model_calls = 0;
        self.status.updated_at = Some(updated_at);
        if self.persist_status().is_err() {
            self.status.state.clear();
            self.status.state.push_str("status_persistence_error");
            self.status.reason_code = Some("status_persistence_refused".to_owned());
            self.status.wake_enqueued = false;
        }
        self.status.clone()
    }

    fn persist_status(&mut self) -> std::io::Result<()> {
        save_aggregate_status(&self.state_dir, &self.status)?;
        if !self.dispatch_state_available {
            return Ok(());
        }
        let digest = dispatch_targets_digest(&self.status.dispatch_targets)?;
        if self.dispatch_state_digest.as_deref() == Some(&digest) {
            return Ok(());
        }
        persist_dispatch_targets(&self.state_dir, &self.status.dispatch_targets)?;
        self.dispatch_state_digest = Some(digest);
        Ok(())
    }
}

fn dispatch_observation_key(authority: &crate::dispatch_wedge::DispatchJobAuthority) -> String {
    format!(
        "{}/{}/{}/{}/{}/{}",
        authority.repository.to_ascii_lowercase(),
        authority.pull_request,
        authority.pull_request_head.to_ascii_lowercase(),
        authority.merge_group_head.to_ascii_lowercase(),
        authority.workflow_run_id,
        authority.job_id
    )
}

fn dispatch_cycle_status_priority(status: &ActionableWakeProducerStatus) -> u8 {
    match status.reason_code.as_deref() {
        Some(
            "dispatch_wedge"
            | "dispatch_wedge_unmatched"
            | "dispatch_wedge_publication_refused"
            | "dispatch_wedge_cleanup_failed",
        ) => 3,
        Some(
            "matching_second_read_required"
            | "dispatch_wedge_checkpoint_failed"
            | "dispatch_wedge_pending_publication_failed"
            | "dispatch_wedge_pending_publication_mismatch"
            | "status_persistence_refused",
        ) => 2,
        Some("assignment_threshold_not_reached") => 1,
        _ => 0,
    }
}

fn dispatch_scope_prefix(
    repository_provider: &str,
    repository_id: &str,
    repository: &str,
    pull_request: u64,
    head_sha: &str,
) -> String {
    crate::work_ledger::dispatch_probe_target_key(
        repository_provider,
        repository_id,
        repository,
        pull_request,
        head_sha,
    )
}

fn canonicalize_dispatch_target_keys(
    targets: BTreeMap<String, DispatchTargetCheckpoint>,
) -> Result<BTreeMap<String, DispatchTargetCheckpoint>, String> {
    let mut canonical = BTreeMap::new();
    for target in targets.into_values() {
        let key = dispatch_scope_prefix(
            &target.repository_provider,
            &target.repository_id,
            &target.repository,
            target.pull_request,
            &target.head_sha,
        );
        if canonical.insert(key, target).is_some() {
            return Err("duplicate canonical dispatch target identity".to_owned());
        }
    }
    Ok(canonical)
}

fn nonempty_identity(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
const fn test_repository_provider() -> &'static str {
    "github.com"
}

#[cfg(test)]
const fn test_repository_id() -> &'static str {
    "R_test_repository"
}

const fn stability_delay_seconds() -> i64 {
    #[cfg(test)]
    {
        0
    }
    #[cfg(not(test))]
    {
        20
    }
}

fn normalize_state(state: &str) -> &str {
    match state {
        "ready" => "ready",
        "in_flight" => "in_flight",
        "uncertain" => "uncertain",
        "disabled" => "disabled",
        _ => "refused",
    }
}

fn aggregate_state(repositories: &BTreeMap<String, ActionableRepositoryStatus>) -> &str {
    if repositories.is_empty() {
        return "idle";
    }
    for candidate in ["refused", "uncertain", "in_flight"] {
        if repositories
            .values()
            .any(|status| status.state == candidate)
        {
            return candidate;
        }
    }
    if repositories
        .values()
        .all(|status| status.state == "disabled")
    {
        "disabled"
    } else {
        "ready"
    }
}

fn status_path(state_dir: &Path) -> PathBuf {
    state_dir
        .join("daemon")
        .join("actionable-wake-producer.json")
}

fn load_status(state_dir: &Path) -> Option<ActionableWakeProducerStatus> {
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
        || metadata.len() > MAX_STATUS_BYTES as u64
        || metadata.mode() & 0o077 != 0
    {
        return None;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).ok()?);
    file.take(MAX_STATUS_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_STATUS_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
fn save_status(state_dir: &Path, status: &ActionableWakeProducerStatus) -> std::io::Result<()> {
    save_aggregate_status(state_dir, status)
        .and_then(|()| persist_dispatch_targets(state_dir, &status.dispatch_targets))
}

fn save_aggregate_status(
    state_dir: &Path,
    status: &ActionableWakeProducerStatus,
) -> std::io::Result<()> {
    let directory = state_dir.join("daemon");
    crate::writer_domain_lease::ensure_protected_dir_all(&directory)?;
    let _writer = crate::writer_domain_lease::acquire_for_protected_path(&directory)?;
    let path = status_path(state_dir);
    let bytes = serde_json::to_vec(status).map_err(std::io::Error::other)?;
    if bytes.len() > MAX_STATUS_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "actionable wake producer status exceeds durable reload limit",
        ));
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(".actionable-wake-producer-")
        .suffix(".tmp")
        .tempfile_in(&directory)?;
    temporary.as_file_mut().set_len(0)?;
    temporary.as_file_mut().write_all(&bytes)?;
    temporary.as_file_mut().sync_all()?;
    crate::queue::replace_file_with_windows_retry(temporary.path(), &path)
}

fn persist_dispatch_targets(
    state_dir: &Path,
    targets: &BTreeMap<String, DispatchTargetCheckpoint>,
) -> std::io::Result<()> {
    let records = targets
        .iter()
        .map(|(target_key, checkpoint)| {
            Ok(crate::work_ledger::DispatchProbeTargetRecord {
                target_key: target_key.clone(),
                repository_provider: checkpoint.repository_provider.clone(),
                repository_id: checkpoint.repository_id.clone(),
                repository: checkpoint.repository.clone(),
                pull_request: checkpoint.pull_request,
                head_sha: checkpoint.head_sha.clone(),
                generation: checkpoint.generation,
                due_at: checkpoint
                    .schedule
                    .as_ref()
                    .map(|schedule| schedule.due_at.clone()),
                checkpoint_json: serde_json::to_vec(checkpoint).map_err(std::io::Error::other)?,
            })
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let ledger = WorkLedger::open_existing(state_dir)
        .map_err(std::io::Error::other)?
        .ok_or_else(|| std::io::Error::other("dispatch target WorkLedger is unavailable"))?;
    ledger
        .replace_dispatch_probe_targets(&records)
        .map_err(std::io::Error::other)
}

fn dispatch_targets_digest(
    targets: &BTreeMap<String, DispatchTargetCheckpoint>,
) -> std::io::Result<String> {
    let bytes = serde_json::to_vec(targets).map_err(std::io::Error::other)?;
    Ok(format!("{:x}", sha2::Sha256::digest(bytes)))
}

fn decode_dispatch_target_records(
    records: Vec<crate::work_ledger::DispatchProbeTargetRecord>,
) -> Result<BTreeMap<String, DispatchTargetCheckpoint>, String> {
    if records.len() > MAX_DISPATCH_PROBE_TARGETS {
        return Err("dispatch_probe_capacity_exhausted".to_owned());
    }
    let mut targets = BTreeMap::new();
    for record in records {
        let checkpoint: DispatchTargetCheckpoint =
            serde_json::from_slice(&record.checkpoint_json).map_err(|error| error.to_string())?;
        if checkpoint.repository_provider != record.repository_provider
            || checkpoint.repository_id != record.repository_id
            || checkpoint.repository != record.repository
            || checkpoint.pull_request != record.pull_request
            || checkpoint.head_sha != record.head_sha
            || checkpoint.generation != record.generation
            || checkpoint
                .schedule
                .as_ref()
                .map(|schedule| &schedule.due_at)
                != record.due_at.as_ref()
        {
            return Err("dispatch target row and checkpoint payload disagree".to_owned());
        }
        if checkpoint
            .pending_publication
            .as_ref()
            .is_some_and(|pending| {
                !pending
                    .repository_provider
                    .eq_ignore_ascii_case(&checkpoint.repository_provider)
                    || pending.repository_id != checkpoint.repository_id
            })
        {
            return Err("dispatch pending publication identity disagrees with target".to_owned());
        }
        let key = dispatch_scope_prefix(
            &checkpoint.repository_provider,
            &checkpoint.repository_id,
            &checkpoint.repository,
            checkpoint.pull_request,
            &checkpoint.head_sha,
        );
        if targets.insert(key, checkpoint).is_some() {
            return Err("duplicate canonical dispatch target identity".to_owned());
        }
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch_wedge::{DispatchJobAuthority, DispatchRunnerObservation};
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
            repository_provider: Some(publication.repository_provider.clone()),
            repository_id: Some(publication.repository_id.clone()),
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
                repository_provider: observation.repository_provider.clone(),
                repository_id: observation.repository_id.clone(),
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

    fn seed_repo_policy(state_dir: &Path, repository: &str) {
        WorkLedger::open(state_dir)
            .expect("ledger")
            .set_repo_policy(
                &crate::work_ledger::RepoPolicy {
                    repo: repository.to_owned(),
                    primary_platform: "macos".to_owned(),
                    compatibility_mode: "independent".to_owned(),
                    compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
                    blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                    declared_dependency_lanes: Vec::new(),
                    revision: 0,
                },
                0,
            )
            .expect("repo policy");
    }

    fn dispatch_observation() -> DispatchWedgeObservation {
        let publication = request();
        DispatchWedgeObservation {
            authority: DispatchJobAuthority {
                repository: publication.repository,
                base_ref: publication.base_ref,
                pull_request: publication.pull_request,
                pull_request_head: publication.head_sha,
                queue_position: 1,
                merge_group_head: "b".repeat(40),
                workflow_run_id: 101,
                workflow_id: 202,
                run_attempt: 1,
                run_event: "merge_group".to_owned(),
                run_head: "b".repeat(40),
                job_id: 303,
                job_name: "macos".to_owned(),
                job_status: "queued".to_owned(),
                job_conclusion: None,
                runner_name: None,
                labels: vec!["self-hosted".to_owned(), "macos".to_owned()],
                queued_at: (Utc::now() - chrono::Duration::minutes(10)).to_rfc3339(),
                required_context: "macos".to_owned(),
                required_app_id: Some(42),
                producer_app_id: Some(42),
            },
            runners: vec![DispatchRunnerObservation {
                runner_id: 404,
                name: "compatible-idle".to_owned(),
                status: "online".to_owned(),
                busy: false,
                labels: vec![
                    "self-hosted".to_owned(),
                    "macos".to_owned(),
                    "extra".to_owned(),
                ],
            }],
            observation_complete: true,
        }
    }

    fn make_dispatch_observation_due(
        producer: &mut ActionableWakeProducer,
        observation: &DispatchWedgeObservation,
    ) {
        let scope = dispatch_scope_prefix(
            test_repository_provider(),
            test_repository_id(),
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
        );
        producer
            .status
            .dispatch_targets
            .get_mut(&scope)
            .expect("dispatch target")
            .observations
            .values_mut()
            .for_each(|checkpoint| {
                checkpoint.not_before = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
            });
        producer.persist_status().expect("persist due checkpoint");
    }

    #[test]
    fn dispatch_wedge_requires_durable_second_cycle_and_wakes_once() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("publish managed handoff");
        let observation = dispatch_observation();
        let mut first = ActionableWakeProducer::new(state.path().to_path_buf());
        let waiting = first.process_dispatch_wedge_observation(&observation, 300);
        assert!(!waiting.wake_enqueued);
        assert_eq!(
            waiting.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
        make_dispatch_observation_due(&mut first, &observation);

        let mut restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        let post_restart_first = restarted.process_dispatch_wedge_observation(&observation, 300);
        assert!(!post_restart_first.wake_enqueued);
        assert_eq!(
            post_restart_first.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
        let published = restarted.process_dispatch_wedge_observation(&observation, 300);
        assert_eq!(published.reason_code.as_deref(), Some("dispatch_wedge"));
        assert!(published.wake_enqueued);
        let replay = restarted.process_dispatch_wedge_observation(&observation, 300);
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
    }

    #[test]
    fn replacement_merge_group_requires_two_fresh_cycles() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("publish managed handoff");
        let original = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        assert!(
            !producer
                .process_dispatch_wedge_cycle(
                    &original.authority.repository,
                    original.authority.pull_request,
                    &original.authority.pull_request_head,
                    std::slice::from_ref(&original),
                    300,
                )
                .wake_enqueued
        );

        let mut replacement = original.clone();
        replacement.authority.merge_group_head = "c".repeat(40);
        replacement.authority.run_head = "c".repeat(40);
        replacement.authority.workflow_run_id += 1;
        replacement.authority.job_id += 1;
        let first_replacement = producer.process_dispatch_wedge_cycle(
            &replacement.authority.repository,
            replacement.authority.pull_request,
            &replacement.authority.pull_request_head,
            std::slice::from_ref(&replacement),
            300,
        );
        assert!(!first_replacement.wake_enqueued);
        assert_eq!(
            first_replacement.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
        let returned_original = producer.process_dispatch_wedge_cycle(
            &original.authority.repository,
            original.authority.pull_request,
            &original.authority.pull_request_head,
            std::slice::from_ref(&original),
            300,
        );
        assert!(!returned_original.wake_enqueued);
        assert_eq!(
            returned_original.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
    }

    #[test]
    fn absent_cycle_invalidates_prior_candidate_digest() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("publish managed handoff");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            Utc::now(),
        );
        let absent = producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            &[],
            300,
        );
        assert_eq!(
            absent.reason_code.as_deref(),
            Some("dispatch_wedge_candidate_absent")
        );
        assert!(producer.finish_dispatch_cycle_without_followup(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
        ));
        assert!(producer.status.dispatch_targets.is_empty());
        let returned = producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        assert!(!returned.wake_enqueued);
        assert_eq!(
            returned.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
    }

    #[test]
    fn failed_observer_cycle_invalidates_prior_candidate_digest() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("publish managed handoff");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        producer.invalidate_dispatch_wedge_scope(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            "dispatch_wedge_observation_failed",
        );
        let returned = producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        assert!(!returned.wake_enqueued);
        assert_eq!(
            returned.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
    }

    #[test]
    fn failed_first_checkpoint_cannot_authorize_second_read() {
        let temp = tempfile::tempdir().expect("temp");
        let blocker = temp.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker");
        let state_dir = blocker.join("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state_dir.clone());
        let failed = producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        assert_eq!(failed.state, "status_persistence_error");
        assert!(producer.status.dispatch_targets.is_empty());

        std::fs::remove_file(&blocker).expect("remove blocker");
        std::fs::create_dir_all(&state_dir).expect("state");
        let mut producer = ActionableWakeProducer::new(state_dir.clone());
        let retried = producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        assert!(!retried.wake_enqueued);
        assert_eq!(
            retried.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
    }

    #[test]
    fn dispatch_probe_deadline_survives_restart() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            Utc::now() - chrono::Duration::seconds(1),
        );
        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        let due = restarted.due_dispatch_probes(Utc::now(), usize::MAX);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].repository, observation.authority.repository);
        assert_eq!(due[0].pull_request, observation.authority.pull_request);
        assert_eq!(due[0].head_sha, observation.authority.pull_request_head);
    }

    #[test]
    fn failed_probe_reschedule_preserves_prior_durable_deadline() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        let original_due = Utc::now() + chrono::Duration::minutes(5);
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            original_due,
        );
        let daemon_dir = state.path().join("daemon");
        let saved_dir = state.path().join("daemon-saved");
        std::fs::rename(&daemon_dir, &saved_dir).expect("move daemon state");
        std::fs::write(&daemon_dir, b"block directory recreation").expect("block daemon dir");
        let refused = producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            Utc::now(),
        );
        assert_eq!(refused.state, "status_persistence_error");
        std::fs::remove_file(&daemon_dir).expect("remove blocker");
        std::fs::rename(&saved_dir, &daemon_dir).expect("restore daemon state");

        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        let target = restarted
            .status
            .dispatch_targets
            .values()
            .next()
            .expect("preserved target");
        assert_eq!(
            target.schedule.as_ref().expect("preserved schedule").due_at,
            original_due.to_rfc3339()
        );
    }

    #[test]
    fn one_hundred_twenty_eight_dispatch_targets_restart_without_json_growth_or_eviction() {
        let state = tempfile::tempdir().expect("state");
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        for pull_request in 1..=128 {
            producer.schedule_dispatch_probe(
                "owner/repo",
                pull_request,
                &format!("{pull_request:040x}"),
                Utc::now() - chrono::Duration::seconds(1),
            );
        }
        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        assert_eq!(
            restarted.due_dispatch_probes(Utc::now(), usize::MAX).len(),
            128
        );
        assert_eq!(
            WorkLedger::open_existing(state.path())
                .unwrap()
                .unwrap()
                .load_dispatch_probe_targets()
                .unwrap()
                .len(),
            128
        );
        assert!(std::fs::metadata(status_path(state.path())).unwrap().len() < 16 * 1024);
    }

    #[test]
    fn dispatch_target_capacity_is_durable_typed_backpressure_without_eviction() {
        let state = tempfile::tempdir().expect("state");
        let mut status = ActionableWakeProducerStatus::default();
        let due_at = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        for pull_request in 1..=u64::try_from(MAX_DISPATCH_PROBE_TARGETS).unwrap() {
            let head_sha = format!("{pull_request:040x}");
            let key = dispatch_scope_prefix(
                test_repository_provider(),
                test_repository_id(),
                "owner/repo",
                pull_request,
                &head_sha,
            );
            status.dispatch_targets.insert(
                key,
                DispatchTargetCheckpoint {
                    repository_provider: test_repository_provider().to_owned(),
                    repository_id: test_repository_id().to_owned(),
                    repository: "owner/repo".to_owned(),
                    pull_request,
                    head_sha: head_sha.clone(),
                    generation: 1,
                    schedule: Some(DispatchProbeSchedule {
                        repository_provider: test_repository_provider().to_owned(),
                        repository_id: test_repository_id().to_owned(),
                        repository: "owner/repo".to_owned(),
                        pull_request,
                        head_sha,
                        due_at: due_at.clone(),
                    }),
                    observations: BTreeMap::new(),
                    pending_publication: None,
                },
            );
        }
        WorkLedger::open(state.path()).expect("ledger");
        save_status(state.path(), &status).expect("capacity fixture");

        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        let refused = producer.schedule_dispatch_probe(
            "owner/repo",
            u64::try_from(MAX_DISPATCH_PROBE_TARGETS).unwrap() + 1,
            &"f".repeat(40),
            Utc::now(),
        );
        assert_eq!(
            refused.reason_code.as_deref(),
            Some("dispatch_probe_capacity_exhausted")
        );
        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        assert_eq!(
            restarted.status.dispatch_targets.len(),
            MAX_DISPATCH_PROBE_TARGETS
        );
        assert_eq!(
            restarted.status.reason_code.as_deref(),
            Some("dispatch_probe_capacity_exhausted")
        );
    }

    #[test]
    fn full_capacity_inventory_noop_uses_exact_keys_without_persistence() {
        let state = tempfile::tempdir().expect("state");
        let mut status = ActionableWakeProducerStatus::default();
        let mut active = std::collections::BTreeSet::new();
        for pull_request in 1..=u64::try_from(MAX_DISPATCH_PROBE_TARGETS).unwrap() {
            let head_sha = format!("{pull_request:040x}");
            let key = dispatch_scope_prefix(
                test_repository_provider(),
                test_repository_id(),
                "Owner/Repo",
                pull_request,
                &head_sha,
            );
            active.insert(DispatchTargetInventoryIdentity::new(
                test_repository_provider(),
                test_repository_id(),
                "Owner/Repo",
                pull_request,
                &head_sha,
            ));
            status.dispatch_targets.insert(
                key,
                DispatchTargetCheckpoint {
                    repository_provider: test_repository_provider().to_owned(),
                    repository_id: test_repository_id().to_owned(),
                    repository: "owner/repo".to_owned(),
                    pull_request,
                    head_sha,
                    generation: 1,
                    schedule: None,
                    observations: BTreeMap::new(),
                    pending_publication: None,
                },
            );
        }
        WorkLedger::open(state.path()).expect("ledger");
        save_status(state.path(), &status).expect("capacity fixture");
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());

        let daemon_dir = state.path().join("daemon");
        let saved_dir = state.path().join("daemon-saved");
        std::fs::rename(&daemon_dir, &saved_dir).expect("move daemon state");
        std::fs::write(&daemon_dir, b"block aggregate persistence").expect("block daemon dir");

        let retained = producer.retain_dispatch_targets(&active);
        assert_ne!(retained.state, "status_persistence_error");
        assert_eq!(
            producer.status.dispatch_targets.len(),
            MAX_DISPATCH_PROBE_TARGETS
        );

        std::fs::remove_file(&daemon_dir).expect("remove blocker");
        std::fs::rename(&saved_dir, &daemon_dir).expect("restore daemon state");
    }

    #[test]
    fn inventory_prune_refuses_same_slug_head_with_different_repository_identity() {
        let state = tempfile::tempdir().expect("state");
        let head_sha = "a".repeat(40);
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer
            .begin_dispatch_wedge_cycle_for_repository(
                test_repository_provider(),
                test_repository_id(),
                "owner/repo",
                42,
                &head_sha,
            )
            .expect("identity-bound target");
        let active = std::collections::BTreeSet::from([DispatchTargetInventoryIdentity::new(
            test_repository_provider(),
            "different-immutable-repository-id",
            "OWNER/REPO",
            42,
            &head_sha.to_ascii_uppercase(),
        )]);

        producer.retain_dispatch_targets(&active);

        assert!(producer.status.dispatch_targets.is_empty());
    }

    #[test]
    fn same_slug_head_with_distinct_repository_identities_cycle_and_clean_independently() {
        let state = tempfile::tempdir().expect("state");
        let repository = "owner/repo";
        let head_sha = "a".repeat(40);
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        let first_generation = producer
            .begin_dispatch_wedge_cycle_for_repository(
                "github.com",
                "R_first",
                repository,
                42,
                &head_sha,
            )
            .expect("first identity generation");
        let second_generation = producer
            .begin_dispatch_wedge_cycle_for_repository(
                "enterprise.example",
                "R_second",
                repository,
                42,
                &head_sha,
            )
            .expect("second identity generation");
        assert_eq!(producer.status.dispatch_targets.len(), 2);

        let first = producer.process_dispatch_wedge_cycle_at_generation_for_repository(
            "github.com",
            "R_first",
            repository,
            42,
            &head_sha,
            first_generation,
            &[],
            300,
        );
        assert_eq!(
            first.reason_code.as_deref(),
            Some("dispatch_wedge_candidate_absent")
        );
        assert!(
            producer.finish_dispatch_cycle_without_followup_for_repository(
                "github.com",
                "R_first",
                repository,
                42,
                &head_sha,
            )
        );
        assert_eq!(producer.status.dispatch_targets.len(), 1);
        assert!(producer.dispatch_cycle_generation_current_for_repository(
            "enterprise.example",
            "R_second",
            repository,
            42,
            &head_sha,
            second_generation,
        ));

        producer.invalidate_dispatch_wedge_scope_at_generation_for_repository(
            "enterprise.example",
            "R_second",
            repository,
            42,
            &head_sha,
            second_generation,
            "test_cleanup",
        );
        assert!(producer.status.dispatch_targets.is_empty());
    }

    #[test]
    fn unreadable_dispatch_target_state_fails_closed_without_deleting_rows() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            Utc::now(),
        );
        rusqlite::Connection::open(state.path().join("work-ledger").join("work-items.sqlite3"))
            .unwrap()
            .execute(
                "UPDATE dispatch_probe_targets SET checkpoint_json = ?1",
                [b"[]".as_slice()],
            )
            .expect("corrupt payload control");

        let mut restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        assert_eq!(
            restarted.status.reason_code.as_deref(),
            Some("dispatch_probe_state_unreadable")
        );
        assert!(
            restarted
                .due_dispatch_probes(Utc::now(), usize::MAX)
                .is_empty()
        );
        restarted.mark_ready("owner/repo", 43, &"a".repeat(40));
        assert_eq!(
            WorkLedger::open_existing(state.path())
                .unwrap()
                .unwrap()
                .load_dispatch_probe_targets()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn legacy_json_dispatch_targets_migrate_once_into_work_ledger() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            Utc::now() - chrono::Duration::seconds(1),
        );
        let mut legacy = serde_json::to_value(&producer.status).expect("legacy status");
        legacy.as_object_mut().unwrap().insert(
            "dispatch_targets".to_owned(),
            serde_json::to_value(&producer.status.dispatch_targets).unwrap(),
        );
        WorkLedger::open_existing(state.path())
            .unwrap()
            .unwrap()
            .replace_dispatch_probe_targets(&[])
            .expect("pre-migration empty ledger");
        std::fs::write(
            status_path(state.path()),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .expect("legacy JSON fixture");

        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        assert_eq!(
            restarted.due_dispatch_probes(Utc::now(), usize::MAX).len(),
            1
        );
        assert_eq!(
            WorkLedger::open_existing(state.path())
                .unwrap()
                .unwrap()
                .load_dispatch_probe_targets()
                .unwrap()
                .len(),
            1
        );
        assert!(
            !std::fs::read_to_string(status_path(state.path()))
                .unwrap()
                .contains("dispatch_targets")
        );
    }

    #[test]
    fn legacy_json_duplicate_canonical_identity_fails_closed() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            Utc::now() - chrono::Duration::seconds(1),
        );
        let checkpoint = producer
            .status
            .dispatch_targets
            .values()
            .next()
            .expect("scheduled checkpoint")
            .clone();
        let mut duplicate_targets = BTreeMap::new();
        duplicate_targets.insert("legacy-key-one".to_owned(), checkpoint.clone());
        duplicate_targets.insert("legacy-key-two".to_owned(), checkpoint);
        let mut legacy = serde_json::to_value(&producer.status).expect("legacy status");
        legacy.as_object_mut().unwrap().insert(
            "dispatch_targets".to_owned(),
            serde_json::to_value(duplicate_targets).unwrap(),
        );
        WorkLedger::open_existing(state.path())
            .unwrap()
            .unwrap()
            .replace_dispatch_probe_targets(&[])
            .expect("pre-migration empty ledger");
        std::fs::write(
            status_path(state.path()),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .expect("legacy JSON fixture");

        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        assert_eq!(
            restarted.status.reason_code.as_deref(),
            Some("dispatch_probe_state_unreadable")
        );
        assert!(restarted.status.dispatch_targets.is_empty());
        assert!(
            WorkLedger::open_existing(state.path())
                .unwrap()
                .unwrap()
                .load_dispatch_probe_targets()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn predecessor_sqlite_key_rekeys_atomically_without_losing_target_state() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            Utc::now() - chrono::Duration::seconds(1),
        );
        let target = producer
            .status
            .dispatch_targets
            .values_mut()
            .next()
            .expect("scheduled target");
        target.observations.insert(
            "durable-observation".to_owned(),
            DispatchObservationCheckpoint {
                digest: "evidence-digest".to_owned(),
                not_before: Utc::now().to_rfc3339(),
                boot_epoch: "prior-boot".to_owned(),
            },
        );
        target.pending_publication = Some(DispatchPendingPublication {
            repository_provider: target.repository_provider.clone(),
            repository_id: target.repository_id.clone(),
            base_ref: "main".to_owned(),
            observation_digest: "observation-digest".to_owned(),
            dedupe_key: "dedupe-key".to_owned(),
            evidence_digest: "evidence-digest".to_owned(),
        });
        persist_dispatch_targets(state.path(), &producer.status.dispatch_targets)
            .expect("persist complete predecessor state");
        let expected = producer.status.dispatch_targets.clone();
        let predecessor_key = format!(
            "{}/{}/{}/",
            observation.authority.repository.to_ascii_lowercase(),
            observation.authority.pull_request,
            observation.authority.pull_request_head.to_ascii_lowercase()
        );
        rusqlite::Connection::open(state.path().join("work-ledger/work-items.sqlite3"))
            .unwrap()
            .execute(
                "UPDATE dispatch_probe_targets SET target_key = ?1",
                [&predecessor_key],
            )
            .expect("install exact predecessor key");

        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        assert!(restarted.dispatch_state_available);
        assert_eq!(restarted.status.dispatch_targets, expected);
        let rows = WorkLedger::open_existing(state.path())
            .unwrap()
            .unwrap()
            .load_dispatch_probe_targets()
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].target_key,
            dispatch_scope_prefix(
                &rows[0].repository_provider,
                &rows[0].repository_id,
                &rows[0].repository,
                rows[0].pull_request,
                &rows[0].head_sha,
            )
        );
        let second_restart = ActionableWakeProducer::new(state.path().to_path_buf());
        assert_eq!(second_restart.status.dispatch_targets, expected);
    }

    #[test]
    fn restart_refuses_pending_publication_for_different_repository_identity() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            Utc::now() - chrono::Duration::seconds(1),
        );
        let target = producer
            .status
            .dispatch_targets
            .values_mut()
            .next()
            .expect("scheduled target");
        target.pending_publication = Some(DispatchPendingPublication {
            repository_provider: "enterprise.example".to_owned(),
            repository_id: "R_other".to_owned(),
            base_ref: "main".to_owned(),
            observation_digest: "observation-digest".to_owned(),
            dedupe_key: "dedupe-key".to_owned(),
            evidence_digest: "evidence-digest".to_owned(),
        });
        persist_dispatch_targets(state.path(), &producer.status.dispatch_targets)
            .expect("persist malformed control");

        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        assert!(!restarted.dispatch_state_available);
        assert_eq!(
            restarted.status.reason_code.as_deref(),
            Some("dispatch_probe_state_unreadable")
        );
        assert!(restarted.status.dispatch_targets.is_empty());
    }

    #[test]
    fn sixty_five_targets_make_fair_restart_progress_and_prune() {
        let state = tempfile::tempdir().expect("state");
        let mut observations = Vec::new();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        for pull_request in 1..=65 {
            let mut observation = dispatch_observation();
            observation.authority.pull_request = pull_request;
            observation.authority.pull_request_head = format!("{pull_request:040x}");
            observation.authority.merge_group_head = format!("{:040x}", pull_request + 100);
            observation.authority.run_head = observation.authority.merge_group_head.clone();
            observation.authority.workflow_run_id += pull_request;
            observation.authority.job_id += pull_request;
            let first = producer.process_dispatch_wedge_cycle(
                &observation.authority.repository,
                pull_request,
                &observation.authority.pull_request_head,
                std::slice::from_ref(&observation),
                300,
            );
            assert_eq!(
                first.reason_code.as_deref(),
                Some("matching_second_read_required")
            );
            producer.schedule_dispatch_probe(
                &observation.authority.repository,
                pull_request,
                &observation.authority.pull_request_head,
                Utc::now() - chrono::Duration::seconds(1),
            );
            observations.push(observation);
        }

        let mut restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        assert_eq!(restarted.due_dispatch_probes(Utc::now(), 2).len(), 2);
        assert_eq!(
            restarted.due_dispatch_probes(Utc::now(), usize::MAX).len(),
            65
        );
        for observation in observations {
            let first_generation = restarted
                .begin_dispatch_wedge_cycle(
                    &observation.authority.repository,
                    observation.authority.pull_request,
                    &observation.authority.pull_request_head,
                )
                .expect("first restart generation");
            let first = restarted.process_dispatch_wedge_cycle_at_generation(
                &observation.authority.repository,
                observation.authority.pull_request,
                &observation.authority.pull_request_head,
                first_generation,
                std::slice::from_ref(&observation),
                300,
            );
            assert_eq!(
                first.reason_code.as_deref(),
                Some("matching_second_read_required")
            );
            make_dispatch_observation_due(&mut restarted, &observation);
            let second_generation = restarted
                .begin_dispatch_wedge_cycle(
                    &observation.authority.repository,
                    observation.authority.pull_request,
                    &observation.authority.pull_request_head,
                )
                .expect("second restart generation");
            let terminal = restarted.process_dispatch_wedge_cycle_at_generation(
                &observation.authority.repository,
                observation.authority.pull_request,
                &observation.authority.pull_request_head,
                second_generation,
                std::slice::from_ref(&observation),
                300,
            );
            assert_eq!(
                terminal.reason_code.as_deref(),
                Some("dispatch_wedge_unmatched")
            );
        }
        assert_eq!(restarted.status.dispatch_targets.len(), 65);
        assert!(
            restarted.status.dispatch_targets.values().all(|target| {
                target.pending_publication.is_some() && target.schedule.is_some()
            })
        );
        restarted.retain_dispatch_targets(&std::collections::BTreeSet::new());
        assert!(restarted.status.dispatch_targets.is_empty());
    }

    #[test]
    fn inventory_prune_persistence_failure_rolls_back_and_survives_restart() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let due_at = Utc::now() - chrono::Duration::seconds(1);
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        make_dispatch_observation_due(&mut producer, &observation);
        producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            due_at,
        );

        let daemon_dir = state.path().join("daemon");
        let saved_dir = state.path().join("daemon-saved");
        std::fs::rename(&daemon_dir, &saved_dir).expect("move daemon state");
        std::fs::write(&daemon_dir, b"block aggregate persistence").expect("block daemon dir");
        let refused = producer.retain_dispatch_targets(&std::collections::BTreeSet::new());
        assert_eq!(refused.state, "status_persistence_error");
        assert_eq!(producer.status.dispatch_targets.len(), 1);
        let retained = producer.status.dispatch_targets.values().next().unwrap();
        assert!(retained.schedule.is_some());
        assert!(!retained.observations.is_empty());
        assert!(retained.pending_publication.is_some());

        std::fs::remove_file(&daemon_dir).expect("remove blocker");
        std::fs::rename(&saved_dir, &daemon_dir).expect("restore daemon state");
        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        let due = restarted.due_dispatch_probes(Utc::now(), usize::MAX);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].pull_request, observation.authority.pull_request);
        let retained = restarted.status.dispatch_targets.values().next().unwrap();
        assert!(!retained.observations.is_empty());
        assert!(retained.pending_publication.is_some());
    }

    #[test]
    fn pending_publication_survives_restart_and_assigned_or_absent_probe() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        let first = producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        assert_eq!(
            first.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
        make_dispatch_observation_due(&mut producer, &observation);
        let refused = producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        assert_eq!(
            refused.reason_code.as_deref(),
            Some("dispatch_wedge_unmatched")
        );
        assert!(
            producer
                .status
                .dispatch_targets
                .values()
                .any(|target| target.pending_publication.is_some())
        );

        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("recover ledger");
        let mut restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        let recovered = restarted.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            &[],
            300,
        );
        assert_eq!(recovered.reason_code.as_deref(), Some("dispatch_wedge"));
        assert!(recovered.wake_enqueued);
        assert!(restarted.status.dispatch_targets.is_empty());
    }

    #[test]
    fn pending_publication_survives_failed_probe_and_wakes_once() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        make_dispatch_observation_due(&mut producer, &observation);
        let refused = producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            std::slice::from_ref(&observation),
            300,
        );
        assert_eq!(
            refused.reason_code.as_deref(),
            Some("dispatch_wedge_unmatched")
        );

        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("recover ledger");
        let mut restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        let generation = restarted
            .begin_dispatch_wedge_cycle(
                &observation.authority.repository,
                observation.authority.pull_request,
                &observation.authority.pull_request_head,
            )
            .expect("error probe generation");
        let recovered = restarted.invalidate_dispatch_wedge_scope_at_generation(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            generation,
            "dispatch_wedge_observation_failed",
        );
        assert_eq!(recovered.reason_code.as_deref(), Some("dispatch_wedge"));
        assert!(recovered.wake_enqueued);
        let duplicate = restarted.invalidate_dispatch_wedge_scope(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            "dispatch_wedge_observation_failed",
        );
        assert!(!duplicate.wake_enqueued);
    }

    #[test]
    fn oversized_status_is_refused_before_replacing_restartable_state() {
        let state = tempfile::tempdir().expect("state");
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.mark_ready("owner/repo", 1, &"a".repeat(40));
        let prior = std::fs::read(status_path(state.path())).expect("prior state");
        assert!(prior.len() <= MAX_STATUS_BYTES);

        let refused = producer.schedule_dispatch_probe(
            &format!("owner/{}", "r".repeat(MAX_STATUS_BYTES)),
            2,
            &"b".repeat(40),
            Utc::now(),
        );
        assert_eq!(refused.state, "status_persistence_error");
        assert_eq!(
            std::fs::read(status_path(state.path())).expect("preserved state"),
            prior
        );
        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        assert_eq!(
            restarted.status.reason_code.as_deref(),
            Some("steward_cycle_complete")
        );
    }

    #[test]
    fn failed_pending_publication_save_cannot_authorize_a_later_cycle() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("publish managed handoff");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        assert_eq!(
            producer
                .process_dispatch_wedge_observation(&observation, 300)
                .reason_code
                .as_deref(),
            Some("matching_second_read_required")
        );
        make_dispatch_observation_due(&mut producer, &observation);

        let scope = dispatch_scope_prefix(
            test_repository_provider(),
            test_repository_id(),
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
        );
        let original_repository_id = producer.status.dispatch_targets[&scope]
            .repository_id
            .clone();
        producer
            .status
            .dispatch_targets
            .get_mut(&scope)
            .expect("checkpoint")
            .repository_id = "x".repeat(70_000);

        let failed = producer.process_dispatch_wedge_observation(&observation, 300);
        assert!(matches!(
            failed.reason_code.as_deref(),
            Some("dispatch_wedge_pending_publication_failed" | "status_persistence_refused")
        ));
        assert!(
            producer
                .status
                .dispatch_targets
                .values()
                .all(|target| target.pending_publication.is_none())
        );
        assert_eq!(
            WorkLedger::open_existing(state.path())
                .unwrap()
                .unwrap()
                .status()
                .unwrap()
                .pending_wakes,
            0
        );

        producer
            .status
            .dispatch_targets
            .get_mut(&scope)
            .expect("rolled-back checkpoint")
            .repository_id = original_repository_id;
        save_status(state.path(), &producer.status).expect("persist rolled-back checkpoint");
        let mut restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        let fresh_first = restarted.process_dispatch_wedge_observation(&observation, 300);
        assert!(!fresh_first.wake_enqueued);
        assert_eq!(
            fresh_first.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
        assert!(
            restarted
                .process_dispatch_wedge_observation(&observation, 300)
                .wake_enqueued
        );
    }

    #[test]
    fn older_probe_cannot_publish_after_newer_absence() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        let old_generation = producer
            .begin_dispatch_wedge_cycle(
                &observation.authority.repository,
                observation.authority.pull_request,
                &observation.authority.pull_request_head,
            )
            .expect("old generation");
        let current_generation = producer
            .begin_dispatch_wedge_cycle(
                &observation.authority.repository,
                observation.authority.pull_request,
                &observation.authority.pull_request_head,
            )
            .expect("current generation");
        let before_superseded = producer.status();
        let superseded = producer.process_dispatch_wedge_cycle_at_generation(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            old_generation,
            std::slice::from_ref(&observation),
            300,
        );
        assert_eq!(superseded, before_superseded);
        assert_eq!(producer.status(), before_superseded);
        let absent = producer.process_dispatch_wedge_cycle_at_generation(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            current_generation,
            &[],
            300,
        );
        assert_eq!(
            absent.reason_code.as_deref(),
            Some("dispatch_wedge_candidate_absent")
        );
        assert!(producer.finish_dispatch_cycle_without_followup(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
        ));
        assert!(producer.status.dispatch_targets.is_empty());
    }

    #[test]
    fn older_probe_cannot_override_newer_merge_group_generation() {
        let state = tempfile::tempdir().expect("state");
        let old = dispatch_observation();
        let mut replacement = old.clone();
        replacement.authority.merge_group_head = "c".repeat(40);
        replacement.authority.run_head = "c".repeat(40);
        replacement.authority.workflow_run_id += 1;
        replacement.authority.job_id += 1;
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        let old_generation = producer
            .begin_dispatch_wedge_cycle(
                &old.authority.repository,
                old.authority.pull_request,
                &old.authority.pull_request_head,
            )
            .expect("old generation");
        let replacement_generation = producer
            .begin_dispatch_wedge_cycle(
                &replacement.authority.repository,
                replacement.authority.pull_request,
                &replacement.authority.pull_request_head,
            )
            .expect("replacement generation");
        let replacement_first = producer.process_dispatch_wedge_cycle_at_generation(
            &replacement.authority.repository,
            replacement.authority.pull_request,
            &replacement.authority.pull_request_head,
            replacement_generation,
            std::slice::from_ref(&replacement),
            300,
        );
        assert_eq!(
            replacement_first.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
        let before_superseded = producer.status();
        let superseded = producer.process_dispatch_wedge_cycle_at_generation(
            &old.authority.repository,
            old.authority.pull_request,
            &old.authority.pull_request_head,
            old_generation,
            std::slice::from_ref(&old),
            300,
        );
        assert_eq!(superseded, before_superseded);
        assert_eq!(producer.status(), before_superseded);
    }

    #[test]
    fn pre_threshold_checkpoint_becomes_actionable_after_deadline() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("publish managed handoff");
        let mut observation = dispatch_observation();
        observation.authority.queued_at = Utc::now().to_rfc3339();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        let early = producer.process_dispatch_wedge_observation(&observation, 1);
        assert_eq!(
            early.reason_code.as_deref(),
            Some("assignment_threshold_not_reached")
        );
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        let mature = producer.process_dispatch_wedge_observation(&observation, 1);
        assert_eq!(mature.reason_code.as_deref(), Some("dispatch_wedge"));
        assert!(mature.wake_enqueued);
    }

    #[test]
    fn old_workflow_clock_cannot_skip_first_observed_job_threshold() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("publish managed handoff");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());

        let first = producer.process_dispatch_wedge_observation(&observation, 1);
        assert_eq!(
            first.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
        let immediate_second = producer.process_dispatch_wedge_observation(&observation, 1);
        assert!(!immediate_second.wake_enqueued);
        assert_eq!(
            immediate_second.reason_code.as_deref(),
            Some("matching_second_read_required")
        );

        std::thread::sleep(std::time::Duration::from_millis(1_100));
        let mature = producer.process_dispatch_wedge_observation(&observation, 1);
        assert_eq!(mature.reason_code.as_deref(), Some("dispatch_wedge"));
        assert!(mature.wake_enqueued);
    }

    #[test]
    fn duplicate_job_rows_in_one_cycle_cannot_satisfy_two_read_gate() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        let status = producer.process_dispatch_wedge_cycle(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            &[observation.clone(), observation.clone()],
            300,
        );
        assert!(!status.wake_enqueued);
        assert_eq!(
            status.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
    }

    #[test]
    fn later_non_followup_job_cannot_discard_an_earlier_second_read_requirement() {
        let state = tempfile::tempdir().expect("state");
        let mut no_capacity = dispatch_observation();
        no_capacity.authority.workflow_run_id += 1;
        no_capacity.authority.job_id += 1;
        no_capacity.authority.job_name = "macos-secondary".to_owned();
        no_capacity.authority.required_context = "macos-secondary".to_owned();
        no_capacity.runners.clear();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        let primed = producer.process_dispatch_wedge_cycle(
            &no_capacity.authority.repository,
            no_capacity.authority.pull_request,
            &no_capacity.authority.pull_request_head,
            std::slice::from_ref(&no_capacity),
            300,
        );
        assert_eq!(
            primed.reason_code.as_deref(),
            Some("matching_second_read_required")
        );

        let fresh_candidate = dispatch_observation();
        let aggregate = producer.process_dispatch_wedge_cycle(
            &fresh_candidate.authority.repository,
            fresh_candidate.authority.pull_request,
            &fresh_candidate.authority.pull_request_head,
            &[fresh_candidate.clone(), no_capacity],
            300,
        );
        assert_eq!(
            aggregate.reason_code.as_deref(),
            Some("matching_second_read_required")
        );
    }

    #[test]
    fn transient_probe_failure_invalidates_evidence_and_restarts_with_backoff() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            Utc::now(),
        );
        let generation = producer
            .begin_dispatch_wedge_cycle_for_repository(
                test_repository_provider(),
                test_repository_id(),
                &observation.authority.repository,
                observation.authority.pull_request,
                &observation.authority.pull_request_head,
            )
            .expect("generation");
        let first = producer.process_dispatch_wedge_cycle_at_generation(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            generation,
            std::slice::from_ref(&observation),
            300,
        );
        assert_eq!(
            first.reason_code.as_deref(),
            Some("matching_second_read_required")
        );

        let retry_at = Utc::now() + chrono::Duration::seconds(60);
        let failed = producer.reschedule_dispatch_probe_after_failure_at_generation(
            test_repository_provider(),
            test_repository_id(),
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            generation,
            retry_at,
            "dispatch_wedge_observation_failed",
        );
        assert_eq!(
            failed.reason_code.as_deref(),
            Some("dispatch_wedge_observation_failed")
        );
        let checkpoint = producer
            .status
            .dispatch_targets
            .values()
            .next()
            .expect("target retained");
        assert!(checkpoint.observations.is_empty());
        assert!(checkpoint.schedule.is_some());

        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        let due = restarted.due_dispatch_probes(retry_at + chrono::Duration::seconds(1), 10);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].pull_request, observation.authority.pull_request);
        assert_eq!(due[0].repository_id, test_repository_id());
    }

    #[test]
    fn successful_steward_auxiliary_probe_failure_retries_without_marking_steward_uncertain() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            Utc::now(),
        );
        let generation = producer
            .begin_dispatch_wedge_cycle_for_repository(
                test_repository_provider(),
                test_repository_id(),
                &observation.authority.repository,
                observation.authority.pull_request,
                &observation.authority.pull_request_head,
            )
            .expect("generation");
        let retry_at = Utc::now() + chrono::Duration::seconds(60);
        let status = producer.reschedule_dispatch_probe_after_auxiliary_failure_at_generation(
            test_repository_provider(),
            test_repository_id(),
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            generation,
            retry_at,
        );
        assert_eq!(status.state, "ready");
        assert_eq!(
            status.reason_code.as_deref(),
            Some("dispatch_wedge_observation_retry_scheduled")
        );

        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        let due = restarted.due_dispatch_probes(retry_at + chrono::Duration::seconds(1), 10);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].pull_request, observation.authority.pull_request);
    }

    #[test]
    fn failed_steward_retains_scope_for_restart_without_entering_observer_lane() {
        let state = tempfile::tempdir().expect("state");
        let observation = dispatch_observation();
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        producer.schedule_dispatch_probe(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            Utc::now(),
        );
        let generation = producer
            .begin_dispatch_wedge_cycle_for_repository(
                test_repository_provider(),
                test_repository_id(),
                &observation.authority.repository,
                observation.authority.pull_request,
                &observation.authority.pull_request_head,
            )
            .expect("generation");
        let status = producer.retain_dispatch_scope_after_steward_failure_at_generation(
            &observation.authority.repository,
            observation.authority.pull_request,
            &observation.authority.pull_request_head,
            generation,
        );
        assert_eq!(status.state, "uncertain");
        assert_eq!(
            status.reason_code.as_deref(),
            Some("steward_retry_scheduled")
        );
        let target = producer
            .status
            .dispatch_targets
            .values()
            .next()
            .expect("retained target");
        assert!(target.schedule.is_none());
        assert!(target.observations.is_empty());

        let restarted = ActionableWakeProducer::new(state.path().to_path_buf());
        assert_eq!(restarted.status.dispatch_targets.len(), 1);
        assert!(
            restarted
                .due_dispatch_probes(Utc::now(), usize::MAX)
                .is_empty()
        );
    }

    #[test]
    fn daemon_producer_is_zero_model_durable_and_idempotent() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
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
    fn repository_identity_failure_never_reaches_terminal_lookup_or_mutation() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
        WorkLedger::plan_or_apply_native_continuation(
            state.path(),
            &publication,
            &policy(vec![publication.repository.clone()]),
            true,
        )
        .expect("publish managed handoff");
        record_steward_transition(state.path(), "resolved");
        let mut mismatched = evidence(1);
        mismatched.transition.observation = None;
        mismatched.transition.failure_class = Some("repository_identity".to_owned());
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        let status = producer.process(&mismatched);
        assert_eq!(
            status.reason_code.as_deref(),
            Some("repository_identity_mismatch")
        );
        let ledger = WorkLedger::open_existing(state.path()).unwrap().unwrap();
        assert_eq!(ledger.status().unwrap().pending_wakes, 0);
        assert_eq!(ledger.native_steward_targets().unwrap().len(), 1);
    }

    #[test]
    fn restart_reconstructs_durable_terminal_before_any_shadow_observation() {
        let state = tempfile::tempdir().expect("state");
        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
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
        assert_eq!(missing.state, "refused");

        let publication = request();
        seed_repo_policy(state.path(), &publication.repository);
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

    #[test]
    fn aggregate_status_is_truthful_and_repository_inventory_is_bounded() {
        let mut repositories = BTreeMap::new();
        for index in 0..300 {
            repositories.insert(
                format!("owner/repo-{index:03}"),
                ActionableRepositoryStatus {
                    state: "ready".to_owned(),
                    pull_request: Some(1),
                    head_sha: Some("a".repeat(40)),
                    reason_code: None,
                    wake_enqueued: false,
                    updated_at: format!("2026-08-29T00:{:02}:00Z", index % 60),
                },
            );
        }
        assert_eq!(aggregate_state(&repositories), "ready");
        repositories.get_mut("owner/repo-001").unwrap().state = "uncertain".to_owned();
        assert_eq!(aggregate_state(&repositories), "uncertain");
        repositories.get_mut("owner/repo-002").unwrap().state = "refused".to_owned();
        assert_eq!(aggregate_state(&repositories), "refused");

        let state = tempfile::tempdir().expect("state");
        let mut producer = ActionableWakeProducer::new(state.path().to_path_buf());
        for index in 0..300 {
            producer.mark_ready(&format!("owner/repo-{index:03}"), 1, &"a".repeat(40));
        }
        assert_eq!(producer.status().repositories.len(), 256);
    }
}
