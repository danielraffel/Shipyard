#[cfg(test)]
mod tests {
    use super::*;

    fn digest(seed: &str) -> Sha256Digest {
        Sha256Digest::of_bytes(seed.as_bytes())
    }

    fn job() -> ApprovedCanaryJob {
        ApprovedCanaryJob {
            schema_version: LEGACY_JOB_SCHEMA_VERSION,
            job_id: "canary-job-1".to_owned(),
            correlation_id: "canary-correlation-1".to_owned(),
            owner: CanaryJobOwner {
                controller_id: "shipyard-controller".to_owned(),
                controller_incarnation: "incarnation-7".to_owned(),
                approval_sha256: digest("approval"),
            },
            operation: ApprovedCanaryOperation::ParallelProofDistributedShadow {
                repository_id: 42,
                repository: "Generous-Corp/pulp".to_owned(),
                target: "macos".to_owned(),
                target_triple: "aarch64-apple-darwin".to_owned(),
                builder_host_id: "m3".to_owned(),
                worker_host_id: "m1".to_owned(),
                manifest_sha256: digest("manifest"),
                request_sha256: digest("request"),
                release_sha256: digest("release"),
                builder_session_generation: 11,
                worker_session_generation: 12,
                cache_authority_sha256: digest("cache-authority"),
                storage_authority_sha256: digest("storage-authority"),
                artifact_bytes_total: 1_024,
                invocation_authority_sha256: digest("authority"),
                adapter_executable_sha256: digest("adapter"),
                worker_executable_sha256: digest("shipyard-worker"),
            },
            approved_at_ms: 1_000,
            deadline_at_ms: 10_000,
            heartbeat_interval_ms: 100,
            heartbeat_timeout_ms: 500,
            max_heartbeat_receipts: 4,
            success: CanarySuccessPredicate {
                required_exit_code: 0,
                artifact_schema_version: 1,
                max_artifact_bytes: 4096,
            },
            cancellation: CanaryCancellationPolicy {
                grace_ms: 250,
                cancel_at_deadline: true,
            },
            wake: CanaryWakePredicate {
                on_success: true,
                on_actionable_failure: true,
            },
            native_continuation: None,
            logs: CanaryLogPolicy {
                segment_bytes: 1024,
                max_segments: 3,
            },
        }
    }

    fn process(job: &ApprovedCanaryJob) -> CanaryProcessTreeIdentity {
        let nonce = domain_digest(
            "shipyard.canary-job.launch-nonce.v1",
            &(job.digest().unwrap(), &job.owner.controller_incarnation),
        )
        .unwrap();
        let ApprovedCanaryOperation::ParallelProofDistributedShadow {
            worker_executable_sha256,
            ..
        } = &job.operation;
        CanaryProcessTreeIdentity {
            pid: 42,
            tree_id: "pgrp:42".to_owned(),
            birth_token: "birth-1".to_owned(),
            os_start_identity_sha256: Sha256Digest::of_bytes(b"test-start"),
            launch_nonce_sha256: nonce,
            executable_sha256: worker_executable_sha256.clone(),
            launched_at_ms: 1_100,
        }
    }

    fn response(job: &ApprovedCanaryJob) -> CanaryJobResponse {
        let launch_nonce_sha256 = domain_digest(
            "shipyard.canary-job.launch-nonce.v1",
            &(job.digest().unwrap(), &job.owner.controller_incarnation),
        )
        .unwrap();
        CanaryJobResponse {
            schema_version: job.success.artifact_schema_version,
            operation_sha256: job.operation.digest().unwrap(),
            job_sha256: job.digest().unwrap(),
            launch_nonce_sha256,
            observation: DistributedExecutionObservation {
                delivery: ArtifactDeliveryObservation {
                    mode: ArtifactDeliveryMode::FullTransfer,
                    artifact_bytes_total: 1_024,
                    artifact_bytes_reused: 0,
                    artifact_bytes_transferred: 1_024,
                    interruption: None,
                },
                setup_ms: 20,
                transfer_ms: 40,
                verification_ms: 10,
                dispatch_ms: 5,
                shard_execution_ms: 100,
                worker_active_ms: 180,
                submit_to_receipt_ms: 200,
                caches: Vec::new(),
            },
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        launches: u32,
        launch: Option<Result<CanaryProcessTreeIdentity, String>>,
        discovery: Option<Result<CanaryProcessObservation, String>>,
        observations: Vec<Result<CanaryProcessObservation, String>>,
        cancellation: Option<Result<CanaryCancellationObservation, String>>,
    }

    impl CanaryJobBackend for FakeBackend {
        fn launch(
            &mut self,
            _job: &ApprovedCanaryJob,
            _launch_nonce_sha256: &Sha256Digest,
            claimed_at_ms: u64,
        ) -> Result<CanaryProcessTreeIdentity, String> {
            assert_eq!(claimed_at_ms, 1_100);
            self.launches += 1;
            self.launch.take().expect("launch configured")
        }

        fn discover(
            &mut self,
            _job: &ApprovedCanaryJob,
            _launch_nonce_sha256: &Sha256Digest,
        ) -> Result<CanaryProcessObservation, String> {
            self.discovery.take().expect("discovery configured")
        }

        fn observe(
            &mut self,
            _job: &ApprovedCanaryJob,
            _process: &CanaryProcessTreeIdentity,
        ) -> Result<CanaryProcessObservation, String> {
            self.observations.remove(0)
        }

        fn cancel(
            &mut self,
            _job: &ApprovedCanaryJob,
            _process: &CanaryProcessTreeIdentity,
            grace_ms: u64,
        ) -> Result<CanaryCancellationObservation, String> {
            assert_eq!(grace_ms, 250);
            self.cancellation.take().expect("cancellation configured")
        }
    }

    #[test]
    fn exact_replay_never_redispatches() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let mut backend = FakeBackend {
            launch: Some(Ok(process(&job))),
            ..FakeBackend::default()
        };

        let first = launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        let replay = launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        assert!(first.launched);
        assert!(!replay.launched);
        assert_eq!(backend.launches, 1);
        assert_eq!(first.snapshot, replay.snapshot);
    }

    #[test]
    fn existing_launch_claim_is_the_only_spawn_authority() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        let (_, outcome) = store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        assert_eq!(outcome, StoreWriteOutcome::Created);
        let mut contender = FakeBackend::default();

        let result = launch_canary_job(&store, &job, 1_100, &mut contender).unwrap();

        assert!(!result.launched);
        assert_eq!(contender.launches, 0);
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Launching { .. }
        ));
    }

    #[test]
    fn durable_prelaunch_cancellation_wins_without_spawning() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let request = CanaryCancellationRequest {
            job_sha256: job.digest().unwrap(),
            controller_id: job.owner.controller_id.clone(),
            approval_sha256: job.owner.approval_sha256.clone(),
            requested_at_ms: 1_050,
        };
        store.request_cancel(&job.job_id, &request).unwrap();
        assert_eq!(
            store.request_cancel(&job.job_id, &request).unwrap(),
            StoreWriteOutcome::AlreadyPresent
        );
        let mut backend = FakeBackend::default();

        let result = launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        assert_eq!(backend.launches, 0);
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::CancelledBeforeLaunch,
                process: None,
                ..
            }
        ));
    }

    #[test]
    fn cancellation_during_launch_gap_is_applied_after_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        store
            .request_cancel(
                &job.job_id,
                &CanaryCancellationRequest {
                    job_sha256: job.digest().unwrap(),
                    controller_id: job.owner.controller_id.clone(),
                    approval_sha256: job.owner.approval_sha256.clone(),
                    requested_at_ms: 1_150,
                },
            )
            .unwrap();
        assert!(matches!(
            store.load(&job.job_id).unwrap().latest().receipt,
            CanaryJobReceiptState::CancellationRequestedBeforeIdentity { .. }
        ));
        let mut backend = FakeBackend {
            discovery: Some(Ok(CanaryProcessObservation::Alive(process(&job)))),
            cancellation: Some(Ok(CanaryCancellationObservation::Terminated)),
            ..FakeBackend::default()
        };

        let result = reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();

        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn crash_after_spawn_is_discovered_not_redispatched() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("jobs");
        let store = CanaryJobStore::open(&root).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        drop(store);

        let reopened = CanaryJobStore::open(&root).unwrap();
        let expected = process(&job);
        let mut backend = FakeBackend {
            discovery: Some(Ok(CanaryProcessObservation::Alive(expected.clone()))),
            ..FakeBackend::default()
        };
        let reconciled = reconcile_canary_job(&reopened, &job.job_id, 1_200, &mut backend).unwrap();

        assert!(!reconciled.launched);
        assert_eq!(backend.launches, 0);
        assert!(matches!(
            reconciled.snapshot.latest().receipt,
            CanaryJobReceiptState::Running { ref process } if process == &expected
        ));
    }

    #[test]
    fn ambiguous_launch_error_remains_discoverable() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let expected = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Err("response lost after spawn token=secret".to_owned())),
            discovery: Some(Ok(CanaryProcessObservation::Alive(expected.clone()))),
            ..FakeBackend::default()
        };

        let ambiguous = launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        assert!(ambiguous.retryable_failure_sha256.is_some());
        assert!(matches!(
            ambiguous.snapshot.latest().receipt,
            CanaryJobReceiptState::Launching { .. }
        ));
        let recovered = reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();
        assert!(matches!(
            recovered.snapshot.latest().receipt,
            CanaryJobReceiptState::Running { ref process } if process == &expected
        ));
    }

    #[test]
    fn future_dated_process_identity_is_never_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let mut future = process(&job);
        future.launched_at_ms = 9_000;
        let mut backend = FakeBackend {
            launch: Some(Ok(future)),
            ..FakeBackend::default()
        };

        assert!(matches!(
            launch_canary_job(&store, &job, 1_100, &mut backend),
            Err(ParallelProofError::CorruptRecord(message))
                if message == "canary process launch claim time"
        ));
        assert!(matches!(
            store.load(&job.job_id).unwrap().latest().receipt,
            CanaryJobReceiptState::Launching { .. }
        ));
    }

    #[test]
    fn missing_process_is_terminal_loss_not_success() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        let mut backend = FakeBackend {
            discovery: Some(Ok(CanaryProcessObservation::Missing)),
            ..FakeBackend::default()
        };

        let result = reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();

        assert!(result.wake);
        assert_eq!(result.wake_receipt_sequence, Some(2));
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Lost,
                artifact: None,
                ..
            }
        ));
        let replay = reconcile_canary_job(&store, &job.job_id, 2_100, &mut backend).unwrap();
        assert!(replay.wake);
        assert_eq!(replay.wake_receipt_sequence, Some(2));
        store
            .acknowledge_wake(
                &job.job_id,
                &CanaryWakeAcknowledgement {
                    job_sha256: job.digest().unwrap(),
                    receipt_sha256: replay.snapshot.latest().digest().unwrap(),
                    controller_id: job.owner.controller_id.clone(),
                    approval_sha256: job.owner.approval_sha256.clone(),
                    native_wake_id: None,
                    native_delivery_sha256: None,
                    acknowledged_at_ms: 2_200,
                },
            )
            .unwrap();
        let acknowledged = reconcile_canary_job(&store, &job.job_id, 2_300, &mut backend).unwrap();
        assert!(!acknowledged.wake);
        assert_eq!(acknowledged.wake_receipt_sequence, None);
    }

    #[test]
    fn schema_v2_wake_ack_requires_native_delivery_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let mut job = job();
        job.schema_version = CURRENT_JOB_SCHEMA_VERSION;
        job.native_continuation = Some(CanaryNativeContinuationBinding {
            schema_version: 1,
            work_item_id: format!("wi_{}", "a".repeat(64)),
            work_generation: 4,
            owner_generation: 1,
            route_ref: format!("route_{}", "b".repeat(64)),
            profile_ref: format!("opaque:sha256:{}", "c".repeat(64)),
            payload_digest: "d".repeat(64),
        });
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        let mut backend = FakeBackend {
            discovery: Some(Ok(CanaryProcessObservation::Missing)),
            ..FakeBackend::default()
        };
        let terminal = reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();
        let base = CanaryWakeAcknowledgement {
            job_sha256: job.digest().unwrap(),
            receipt_sha256: terminal.snapshot.latest().digest().unwrap(),
            controller_id: job.owner.controller_id.clone(),
            approval_sha256: job.owner.approval_sha256.clone(),
            native_wake_id: None,
            native_delivery_sha256: None,
            acknowledged_at_ms: 1_300,
        };
        assert!(matches!(
            store.acknowledge_wake(&job.job_id, &base),
            Err(ParallelProofError::AuthenticationFailed)
        ));
        store
            .acknowledge_wake(
                &job.job_id,
                &CanaryWakeAcknowledgement {
                    native_wake_id: Some(
                        native_wake_id(job.native_continuation.as_ref().unwrap()).unwrap(),
                    ),
                    native_delivery_sha256: Some(
                        native_wake_delivery_digest(
                            job.native_continuation.as_ref().unwrap(),
                            &base.job_sha256,
                            &base.receipt_sha256,
                            &native_wake_id(job.native_continuation.as_ref().unwrap()).unwrap(),
                        )
                        .unwrap(),
                    ),
                    ..base
                },
            )
            .unwrap();
        assert!(
            !reconcile_canary_job(&store, &job.job_id, 1_400, &mut backend)
                .unwrap()
                .wake
        );
    }

    #[test]
    fn transient_observation_error_preserves_retryable_custody() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![Err(
                "temporary transport outage with token=secret".to_owned()
            )],
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        let before = store.load(&job.job_id).unwrap();

        let retryable = reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();

        assert!(retryable.retryable_failure_sha256.is_some());
        assert!(!format!("{retryable:?}").contains("temporary transport"));
        assert_eq!(store.load(&job.job_id).unwrap(), before);
        backend
            .observations
            .push(Ok(CanaryProcessObservation::Alive(process)));
        let retried = reconcile_canary_job(&store, &job.job_id, 1_300, &mut backend).unwrap();
        assert!(matches!(
            retried.snapshot.latest().receipt,
            CanaryJobReceiptState::Heartbeat { .. }
        ));
    }

    #[test]
    fn exit_requires_exact_artifact_predicate() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        let artifact = store.record_artifact(&job.job_id, &response(&job)).unwrap();
        backend
            .observations
            .push(Ok(CanaryProcessObservation::Exited {
                process,
                exit_code: Some(0),
                exited_at_ms: 1_500,
                artifact: Some(artifact),
            }));

        let result = reconcile_canary_job(&store, &job.job_id, 2_000, &mut backend).unwrap();

        assert!(result.wake);
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Succeeded,
                artifact: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn crash_before_running_receipt_can_recover_terminal_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        let artifact = store.record_artifact(&job.job_id, &response(&job)).unwrap();
        let mut backend = FakeBackend {
            discovery: Some(Ok(CanaryProcessObservation::Exited {
                process: process(&job),
                exit_code: Some(0),
                exited_at_ms: 1_500,
                artifact: Some(artifact),
            })),
            ..FakeBackend::default()
        };

        let recovered = reconcile_canary_job(&store, &job.job_id, 2_000, &mut backend).unwrap();

        assert!(recovered.wake);
        assert!(matches!(
            recovered.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Succeeded,
                ..
            }
        ));
    }

    #[test]
    fn exit_after_deadline_cannot_be_certified_as_success() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        let artifact = store.record_artifact(&job.job_id, &response(&job)).unwrap();
        backend
            .observations
            .push(Ok(CanaryProcessObservation::Exited {
                process,
                exit_code: Some(0),
                exited_at_ms: 10_001,
                artifact: Some(artifact),
            }));

        let result = reconcile_canary_job(&store, &job.job_id, 11_000, &mut backend).unwrap();

        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Failed,
                artifact: None,
                ..
            }
        ));
    }

    #[test]
    fn cancellation_cannot_erase_an_earlier_authenticated_exit() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        let artifact = store.record_artifact(&job.job_id, &response(&job)).unwrap();
        store
            .request_cancel(
                &job.job_id,
                &CanaryCancellationRequest {
                    job_sha256: job.digest().unwrap(),
                    controller_id: job.owner.controller_id.clone(),
                    approval_sha256: job.owner.approval_sha256.clone(),
                    requested_at_ms: 2_000,
                },
            )
            .unwrap();
        backend
            .observations
            .push(Ok(CanaryProcessObservation::Exited {
                process,
                exit_code: Some(0),
                exited_at_ms: 1_500,
                artifact: Some(artifact),
            }));

        let result = reconcile_canary_job(&store, &job.job_id, 2_100, &mut backend).unwrap();

        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Succeeded,
                ..
            }
        ));
    }

    #[test]
    fn zero_exit_without_artifact_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![Ok(CanaryProcessObservation::Exited {
                process,
                exit_code: Some(0),
                exited_at_ms: 1_500,
                artifact: None,
            })],
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        let result = reconcile_canary_job(&store, &job.job_id, 2_000, &mut backend).unwrap();
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Failed,
                artifact: None,
                ..
            }
        ));
    }

    #[test]
    fn malformed_typed_response_cannot_be_certified_as_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        assert!(matches!(
            store.record_artifact(&job.job_id, &response(&job)),
            Err(ParallelProofError::InvalidAttemptSequence(message))
                if message == "canary artifact requires launch custody"
        ));
        let prepared = store.load(&job.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();
        let mut malformed = response(&job);
        malformed.observation.delivery.artifact_bytes_transferred = 1_023;

        assert!(matches!(
            store.record_artifact(&job.job_id, &malformed),
            Err(ParallelProofError::BindingMismatch(
                "canary typed response counters"
            ))
        ));
        assert!(matches!(
            store.artifacts.load(&artifact_key(&job.job_id)),
            Err(ImmutableStoreError::Missing(_))
        ));
    }

    #[test]
    fn artifact_from_same_operation_cannot_cross_job_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let first = job();
        let stale_response = response(&first);
        let mut second = first.clone();
        second.job_id = "canary-job-2".to_owned();
        second.correlation_id = "canary-correlation-2".to_owned();
        store.submit(&second).unwrap();
        let prepared = store.load(&second.job_id).unwrap();
        let CanaryJobReceiptState::Prepared {
            launch_nonce_sha256,
        } = &prepared.latest().receipt
        else {
            panic!("expected prepared receipt");
        };
        store
            .claim_launch(&prepared, launch_nonce_sha256.clone(), 1_100)
            .unwrap();

        assert!(matches!(
            store.record_artifact(&second.job_id, &stale_response),
            Err(ParallelProofError::BindingMismatch(
                "canary response identity"
            ))
        ));
    }

    #[test]
    fn deadline_cancellation_is_bounded_and_proven() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![Ok(CanaryProcessObservation::Alive(process))],
            cancellation: Some(Ok(CanaryCancellationObservation::Terminated)),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        let result = reconcile_canary_job(&store, &job.job_id, 10_001, &mut backend).unwrap();
        assert!(!result.wake);
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn stale_heartbeat_cancels_even_when_process_is_still_alive() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![Ok(CanaryProcessObservation::Alive(process))],
            cancellation: Some(Ok(CanaryCancellationObservation::Terminated)),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        let result = reconcile_canary_job(&store, &job.job_id, 1_601, &mut backend).unwrap();
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn rapid_poll_does_not_consume_heartbeat_budget() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![
                Ok(CanaryProcessObservation::Alive(process.clone())),
                Ok(CanaryProcessObservation::Alive(process)),
            ],
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();

        let early = reconcile_canary_job(&store, &job.job_id, 1_150, &mut backend).unwrap();
        assert!(matches!(
            early.snapshot.latest().receipt,
            CanaryJobReceiptState::Running { .. }
        ));
        assert_eq!(heartbeat_count(&early.snapshot).unwrap(), 0);
        let due = reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();
        assert_eq!(heartbeat_count(&due.snapshot).unwrap(), 1);
    }

    #[test]
    fn heartbeat_limit_cancels_tree_before_terminal_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let mut job = job();
        job.max_heartbeat_receipts = 1;
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![
                Ok(CanaryProcessObservation::Alive(process.clone())),
                Ok(CanaryProcessObservation::Alive(process)),
            ],
            cancellation: Some(Ok(CanaryCancellationObservation::Terminated)),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        reconcile_canary_job(&store, &job.job_id, 1_200, &mut backend).unwrap();

        let result = reconcile_canary_job(&store, &job.job_id, 1_300, &mut backend).unwrap();

        assert!(backend.cancellation.is_none());
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::HeartbeatLimit,
                ..
            }
        ));
    }

    #[test]
    fn cancellation_missing_is_uncertain_and_actionable() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        let process = process(&job);
        let mut backend = FakeBackend {
            launch: Some(Ok(process.clone())),
            observations: vec![Ok(CanaryProcessObservation::Alive(process))],
            cancellation: Some(Ok(CanaryCancellationObservation::Missing)),
            ..FakeBackend::default()
        };
        launch_canary_job(&store, &job, 1_100, &mut backend).unwrap();
        store
            .request_cancel(
                &job.job_id,
                &CanaryCancellationRequest {
                    job_sha256: job.digest().unwrap(),
                    controller_id: job.owner.controller_id.clone(),
                    approval_sha256: job.owner.approval_sha256.clone(),
                    requested_at_ms: 2_000,
                },
            )
            .unwrap();

        let result = reconcile_canary_job(&store, &job.job_id, 2_100, &mut backend).unwrap();
        assert!(result.wake);
        assert!(matches!(
            result.snapshot.latest().receipt,
            CanaryJobReceiptState::Terminal {
                outcome: CanaryJobTerminalOutcome::CancellationUncertain,
                ..
            }
        ));
    }

    #[test]
    fn logs_are_redacted_immutable_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();

        store
            .record_log_segment(
                &job.job_id,
                0,
                b"phase=transfer\nauthorization: Bearer abc\ntoken=hunter2\npassword hunter3\nGH_PAT=ghp_abc\nAWS_ACCESS_KEY_ID=AKIA123\n-----BEGIN PRIVATE KEY-----\nurl=https://host/path?sig=abc\nok\x01\n",
            )
            .unwrap();
        let log = String::from_utf8(store.load_log_segment(&job.job_id, 0).unwrap()).unwrap();
        assert_eq!(log.matches("[REDACTED]").count(), 8);
        assert!(!log.contains("authorization"));
        assert!(!log.contains("token"));
        assert!(!log.contains("Bearer abc"));
        assert!(!log.contains("hunter2"));
        assert!(!log.contains("hunter3"));
        assert!(!log.contains("ghp_abc"));
        assert!(!log.contains("AKIA123"));
        assert!(!log.contains("PRIVATE KEY"));
        assert!(!log.contains("sig=abc"));
        assert!(!log.contains('\x01'));
        assert_eq!(
            store
                .record_log_segment(&job.job_id, 0, b"different")
                .unwrap_err()
                .to_string(),
            ParallelProofError::ImmutableConflict("job-canary-job-1-log-000".to_owned())
                .to_string()
        );
        assert!(matches!(
            store.record_log_segment(&job.job_id, 3, b"overflow"),
            Err(ParallelProofError::LimitExceeded { .. })
        ));
        assert!(matches!(
            redact_log(b"phase=transfer", b"phase=transfer".len()),
            Err(ParallelProofError::LimitExceeded {
                field: "redacted canary log segment bytes",
                ..
            })
        ));
    }

    #[test]
    fn contradictory_envelope_replay_fails_immutable() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let mut contradiction = job.clone();
        contradiction.deadline_at_ms += 1;

        assert!(matches!(
            store.submit(&contradiction),
            Err(ParallelProofError::ImmutableConflict(_))
        ));
    }

    #[test]
    fn immutable_input_and_pending_index_survive_store_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path()).unwrap();
        let mut exact_job = job();
        let bytes = b"exact-private-invocation";
        let ApprovedCanaryOperation::ParallelProofDistributedShadow { request_sha256, .. } =
            &mut exact_job.operation;
        *request_sha256 = Sha256Digest::of_bytes(bytes);
        store.record_input(&exact_job, bytes).unwrap();
        store.submit(&exact_job).unwrap();
        drop(store);

        let reopened = CanaryJobStore::open(temp.path()).unwrap();
        assert_eq!(reopened.load_input(&exact_job).unwrap(), bytes);
        assert_eq!(
            reopened.pending_job_ids().unwrap(),
            vec![exact_job.job_id.clone()]
        );

        let mut contradiction = exact_job.clone();
        let ApprovedCanaryOperation::ParallelProofDistributedShadow { request_sha256, .. } =
            &mut contradiction.operation;
        *request_sha256 = digest("different-request");
        assert!(matches!(
            reopened.load_input(&contradiction),
            Err(ParallelProofError::BindingMismatch(
                "canary job request bytes"
            ))
        ));

        let large = vec![b'x'; 2 * 1024 * 1024];
        let mut large_job = job();
        large_job.job_id = "canary-large-input".to_owned();
        let ApprovedCanaryOperation::ParallelProofDistributedShadow { request_sha256, .. } =
            &mut large_job.operation;
        *request_sha256 = Sha256Digest::of_bytes(&large);
        reopened.record_input(&large_job, &large).unwrap();
        assert_eq!(reopened.load_input(&large_job).unwrap(), large);
    }

    #[test]
    fn corrupt_record_does_not_starve_unrelated_pending_job() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path()).unwrap();
        let exact_job = job();
        store.submit(&exact_job).unwrap();
        store.records.put("corrupt-envelope", b"{").unwrap();

        let (pending, errors) = store.pending_job_scan().unwrap();
        assert_eq!(pending, vec![exact_job.job_id]);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn read_only_status_open_never_creates_missing_storage() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("absent-jobs");
        assert!(CanaryJobStore::load_read_only(&missing, "job-1").is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn partial_submission_recovers_after_owner_process_death() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("jobs");
        let store = CanaryJobStore::open(&root).unwrap();
        let job = job();
        store
            .records
            .put(
                &envelope_key(&job.job_id),
                &serde_json::to_vec(&job).unwrap(),
            )
            .unwrap();
        drop(store);

        let reopened_by_fresh_controller = CanaryJobStore::open(&root).unwrap();
        assert_eq!(
            reopened_by_fresh_controller.submit(&job).unwrap(),
            StoreWriteOutcome::Created
        );
        assert_eq!(
            reopened_by_fresh_controller
                .load(&job.job_id)
                .unwrap()
                .job
                .operation,
            job.operation
        );
    }

    #[test]
    fn malformed_heartbeat_cannot_skip_running_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let prepared = store.load(&job.job_id).unwrap().latest().clone();
        let heartbeat = CanaryJobReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            job_sha256: job.digest().unwrap(),
            sequence: 1,
            previous_receipt_sha256: Some(prepared.digest().unwrap()),
            receipt: CanaryJobReceiptState::Heartbeat {
                process: process(&job),
                observed_at_ms: 1_200,
            },
        };
        store.put_receipt(&job.job_id, &heartbeat).unwrap();

        assert!(matches!(
            store.load(&job.job_id),
            Err(ParallelProofError::CorruptRecord(message))
                if message == "canary heartbeat transition"
        ));
    }

    #[test]
    fn contradictory_durable_cancel_authority_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = CanaryJobStore::open(temp.path().join("jobs")).unwrap();
        let job = job();
        store.submit(&job).unwrap();
        let request = CanaryCancellationRequest {
            job_sha256: job.digest().unwrap(),
            controller_id: "different-controller".to_owned(),
            approval_sha256: job.owner.approval_sha256.clone(),
            requested_at_ms: 2_000,
        };
        assert!(matches!(
            store.request_cancel(&job.job_id, &request),
            Err(ParallelProofError::AuthenticationFailed)
        ));
        assert!(matches!(
            store.load(&job.job_id).unwrap().latest().receipt,
            CanaryJobReceiptState::Prepared { .. }
        ));
    }
}
