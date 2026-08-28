use super::*;
use crate::work_ledger::dispatch::{
    DeliveryFence, FreshAgentLaunchProfile, ProviderAdapter, ProviderCapability,
    ProviderLaunchRequest, ProviderOutcome, WakeConsumerPolicy, WakeDeliveryResult, WakeEnvelope,
    WakeProfileResolver,
};

#[derive(Clone)]
struct TestProfile {
    provider: String,
    argv: Vec<String>,
    digest: String,
    permits_fresh: bool,
    route_profile_ref: Option<String>,
}

impl FreshAgentLaunchProfile for TestProfile {
    fn provider_id(&self) -> &str {
        &self.provider
    }

    fn launch_argv(&self) -> &[String] {
        &self.argv
    }

    fn profile_digest(&self) -> WorkLedgerResult<String> {
        Ok(self.digest.clone())
    }

    fn permits_fresh_agent(&self) -> bool {
        self.permits_fresh
    }

    fn route_profile_ref(&self) -> WorkLedgerResult<String> {
        self.route_profile_ref.clone().map_or_else(
            || {
                Ok(OpaqueRef::derive("launch-profile", self.digest.as_bytes())
                    .as_str()
                    .to_owned())
            },
            Ok,
        )
    }
}

struct Resolver {
    profile: TestProfile,
    calls: usize,
}

impl WakeProfileResolver for Resolver {
    type Profile = TestProfile;

    fn resolve(&mut self, _wake: &WakeEnvelope) -> WorkLedgerResult<Self::Profile> {
        self.calls += 1;
        Ok(self.profile.clone())
    }
}

struct Adapter {
    capability: Option<ProviderCapability>,
    launch_outcomes: Vec<ProviderOutcome>,
    reconcile_outcome: ProviderOutcome,
    launched_argv: Vec<Vec<String>>,
    launch_fences: Vec<DeliveryFence>,
    reconcile_fences: Vec<DeliveryFence>,
    panic_after_claim: bool,
}

impl Adapter {
    fn successful(idempotent: bool) -> Self {
        Self {
            capability: Some(ProviderCapability {
                adapter_id: "test-provider-adapter".to_owned(),
                fresh_agent_launch: true,
                idempotent_launch: idempotent,
            }),
            launch_outcomes: vec![ProviderOutcome::Acknowledged {
                receipt_digest: digest(b"launch receipt"),
            }],
            reconcile_outcome: ProviderOutcome::Acknowledged {
                receipt_digest: digest(b"reconciled receipt"),
            },
            launched_argv: Vec::new(),
            launch_fences: Vec::new(),
            reconcile_fences: Vec::new(),
            panic_after_claim: false,
        }
    }
}

impl ProviderAdapter for Adapter {
    fn capability(&self, provider_id: &str) -> Option<ProviderCapability> {
        (provider_id == "subrouter")
            .then(|| self.capability.clone())
            .flatten()
    }

    fn launch(&mut self, request: ProviderLaunchRequest<'_>) -> ProviderOutcome {
        self.launched_argv.push(request.argv.to_vec());
        self.launch_fences.push(request.fence.clone());
        assert!(
            !self.panic_after_claim,
            "simulated process death after durable claim"
        );
        self.launch_outcomes.remove(0)
    }

    fn reconcile(&mut self, fence: &DeliveryFence) -> ProviderOutcome {
        self.reconcile_fences.push(fence.clone());
        self.reconcile_outcome.clone()
    }
}

fn active_policy() -> WakeConsumerPolicy {
    WakeConsumerPolicy {
        activation_enabled: true,
        dispatch_enabled: true,
    }
}

fn setup_wake() -> (tempfile::TempDir, WorkLedger, TestProfile, String, String) {
    let temp = tempfile::TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let candidate = sample_candidate();
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("import");
    ledger
        .record_continuations(
            &work_id,
            0,
            &ContinuationSet::new(digest(b"success"), None, digest(b"failure"), None)
                .expect("continuations"),
        )
        .expect("record continuations");
    for (generation, state) in [
        (1, LifecycleState::Published),
        (2, LifecycleState::Ready),
        (3, LifecycleState::Managed),
        (4, LifecycleState::Actionable),
    ] {
        ledger
            .transition_with_wake(&work_id, generation, 3, state, None)
            .expect("transition");
    }
    let (route, agent_adapter) = sample_route(&work_id, 5);
    ledger.register_adapter(&agent_adapter).expect("adapter");
    ledger.register_route(&route).expect("route");
    let profile = TestProfile {
        provider: "subrouter".to_owned(),
        argv: vec![
            "/absolute/provider-wrapper".to_owned(),
            "agent".to_owned(),
            "--prompt=value with spaces;$(never-a-shell)".to_owned(),
        ],
        digest: digest(b"profile"),
        permits_fresh: true,
        route_profile_ref: None,
    };
    let wake = WakeIntent::new(
        &work_id,
        6,
        3,
        route.route_ref.clone(),
        profile.digest.clone(),
    )
    .expect("wake");
    let wake_id = wake.wake_id.clone();
    ledger
        .transition_with_wake(&work_id, 5, 3, LifecycleState::Dispatching, Some(&wake))
        .expect("dispatch transition");
    (temp, ledger, profile, work_id, wake_id)
}

fn outbox_state(ledger: &WorkLedger, wake_id: &str) -> String {
    ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [wake_id],
            |row| row.get(0),
        )
        .expect("outbox state")
}

#[test]
fn success_passes_exact_launch_profile_argv_and_atomically_acknowledges() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut resolver = Resolver {
        profile: profile.clone(),
        calls: 0,
    };
    let mut adapter = Adapter::successful(true);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("consume"),
        WakeDeliveryResult::Acknowledged
    );
    assert_eq!(adapter.launched_argv, vec![profile.argv]);
    assert_eq!(adapter.launch_fences.len(), 1);
    assert_eq!(outbox_state(&ledger, &wake_id), "acknowledged");
    let connection = ledger.connect_read_only().expect("connection");
    let work: (String, u64) = connection
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("work state");
    assert_eq!(work, ("agent_owned_repair".to_owned(), 7));
    let attempt: (String, bool) = connection
        .query_row(
            "SELECT state, finished_at IS NOT NULL FROM wake_attempts WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("attempt");
    assert_eq!(attempt, ("acknowledged".to_owned(), true));
}

#[test]
fn retry_is_durable_and_uses_a_new_attempt_without_changing_generation() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    adapter.launch_outcomes = vec![
        ProviderOutcome::Retryable {
            error_digest: digest(b"temporary provider refusal"),
        },
        ProviderOutcome::Acknowledged {
            receipt_digest: digest(b"second attempt receipt"),
        },
    ];
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("first"),
        WakeDeliveryResult::Retrying
    );
    assert_eq!(outbox_state(&ledger, &wake_id), "pending");
    let generation: u64 = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("generation");
    assert_eq!(generation, 6);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("second"),
        WakeDeliveryResult::Acknowledged
    );
    let attempts: Vec<(u64, String)> = {
        let connection = ledger.connect_read_only().expect("connection");
        let mut query = connection
            .prepare("SELECT attempt, state FROM wake_attempts ORDER BY attempt")
            .expect("query");
        query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    assert_eq!(
        attempts,
        vec![(1, "retry".to_owned()), (2, "acknowledged".to_owned())]
    );
}

#[test]
fn restart_reconciles_idempotent_claim_without_duplicate_launch() {
    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let mut first_resolver = Resolver {
        profile: profile.clone(),
        calls: 0,
    };
    let mut interrupted = Adapter::successful(true);
    interrupted.panic_after_claim = true;
    let death = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = ledger.consume_one_wake(active_policy(), &mut first_resolver, &mut interrupted);
    }));
    assert!(death.is_err());
    assert_eq!(outbox_state(&ledger, &wake_id), "claimed");

    let mut resolver = Resolver { profile, calls: 0 };
    let mut restarted = Adapter::successful(true);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut restarted)
            .expect("reconcile"),
        WakeDeliveryResult::Acknowledged
    );
    assert!(restarted.launched_argv.is_empty());
    assert_eq!(restarted.reconcile_fences.len(), 1);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut restarted)
            .expect("duplicate tick"),
        WakeDeliveryResult::Empty
    );
}

#[test]
fn non_idempotent_restart_becomes_uncertain_without_launch_or_reconcile() {
    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let mut first_resolver = Resolver {
        profile: profile.clone(),
        calls: 0,
    };
    let mut interrupted = Adapter::successful(false);
    interrupted.panic_after_claim = true;
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = ledger.consume_one_wake(active_policy(), &mut first_resolver, &mut interrupted);
    }));

    let mut resolver = Resolver { profile, calls: 0 };
    let mut restarted = Adapter::successful(false);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut restarted)
            .expect("uncertain"),
        WakeDeliveryResult::Uncertain
    );
    assert!(restarted.launched_argv.is_empty());
    assert!(restarted.reconcile_fences.is_empty());
    assert_eq!(outbox_state(&ledger, &wake_id), "uncertain");
}

#[test]
fn pre_v3_claim_without_attempt_proof_becomes_uncertain() {
    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    ledger
        .connect_read_write()
        .expect("connection")
        .execute(
            "UPDATE outbox SET state = 'claimed' WHERE wake_id = ?1",
            [&wake_id],
        )
        .expect("simulate migrated v2 claim");
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("conservative recovery"),
        WakeDeliveryResult::Uncertain
    );
    assert!(adapter.launched_argv.is_empty());
    assert!(adapter.reconcile_fences.is_empty());
    assert_eq!(outbox_state(&ledger, &wake_id), "uncertain");
}

#[test]
fn stale_generation_refuses_before_provider_launch() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    ledger
        .connect_read_write()
        .expect("connection")
        .execute(
            "UPDATE work_items SET work_generation = work_generation + 1 WHERE id = ?1",
            [&work_id],
        )
        .expect("simulate newer owner work");
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    let error = ledger
        .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
        .expect_err("stale generation");
    assert!(error.to_string().contains("generation is stale"));
    assert!(adapter.launched_argv.is_empty());
    assert_eq!(outbox_state(&ledger, &wake_id), "pending");
}

#[test]
fn missing_provider_capability_fails_durably_without_launch() {
    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    adapter.capability = None;
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("missing capability"),
        WakeDeliveryResult::Failed
    );
    assert!(adapter.launched_argv.is_empty());
    assert_eq!(outbox_state(&ledger, &wake_id), "failed");
}

#[test]
fn route_profile_identity_mismatch_refuses_before_claim_or_launch() {
    let (_temp, ledger, mut profile, _work_id, wake_id) = setup_wake();
    profile.route_profile_ref = Some(
        OpaqueRef::derive("launch-profile", b"different protected profile")
            .as_str()
            .to_owned(),
    );
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    let error = ledger
        .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
        .expect_err("route/profile mismatch");
    assert!(error.to_string().contains("wake route is missing, stale"));
    assert!(adapter.launched_argv.is_empty());
    assert_eq!(outbox_state(&ledger, &wake_id), "pending");
}

#[test]
fn route_provider_identity_mismatch_refuses_before_claim_or_launch() {
    let (_temp, ledger, mut profile, _work_id, wake_id) = setup_wake();
    profile.provider = "direct".to_owned();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    let error = ledger
        .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
        .expect_err("route/provider mismatch");
    assert!(error.to_string().contains("wake route is missing, stale"));
    assert!(adapter.launched_argv.is_empty());
    assert_eq!(outbox_state(&ledger, &wake_id), "pending");
}

#[test]
fn live_consumer_lease_fences_second_and_third_consumers_during_provider_call() {
    use std::sync::mpsc;

    struct BlockingAdapter {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }

    impl ProviderAdapter for BlockingAdapter {
        fn capability(&self, provider_id: &str) -> Option<ProviderCapability> {
            (provider_id == "subrouter").then(|| ProviderCapability {
                adapter_id: "test-provider-adapter".to_owned(),
                fresh_agent_launch: true,
                idempotent_launch: true,
            })
        }

        fn launch(&mut self, _request: ProviderLaunchRequest<'_>) -> ProviderOutcome {
            self.entered.send(()).expect("announce provider entry");
            self.release.recv().expect("release provider");
            ProviderOutcome::Acknowledged {
                receipt_digest: digest(b"barrier launch receipt"),
            }
        }

        fn reconcile(&mut self, _fence: &DeliveryFence) -> ProviderOutcome {
            panic!("a concurrent live owner must not be treated as restart recovery");
        }
    }

    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let first_ledger = ledger.clone();
    let first_profile = profile.clone();
    let first = std::thread::spawn(move || {
        let mut resolver = Resolver {
            profile: first_profile,
            calls: 0,
        };
        let mut adapter = BlockingAdapter {
            entered: entered_tx,
            release: release_rx,
        };
        first_ledger.consume_one_wake(active_policy(), &mut resolver, &mut adapter)
    });
    entered_rx.recv().expect("provider entered");

    for _ in 0..2 {
        let mut resolver = Resolver {
            profile: profile.clone(),
            calls: 0,
        };
        let mut adapter = Adapter::successful(true);
        let error = ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect_err("live consumer owns wake");
        assert!(error.to_string().contains("another live wake consumer"));
        assert_eq!(resolver.calls, 0);
        assert!(adapter.launched_argv.is_empty());
    }
    assert_eq!(outbox_state(&ledger, &wake_id), "claimed");
    release_tx.send(()).expect("release launch");
    assert_eq!(
        first.join().expect("consumer thread").expect("consume"),
        WakeDeliveryResult::Acknowledged
    );

    let claims: Vec<(u64, String)> = {
        let connection = ledger.connect_read_only().expect("connection");
        let mut query = connection
            .prepare("SELECT epoch, kind FROM wake_claim_epochs ORDER BY epoch")
            .expect("query");
        query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    assert_eq!(claims, vec![(1, "claim".to_owned())]);
}

#[test]
fn default_off_policy_refuses_before_profile_lookup_or_mutation() {
    let (temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    let error = ledger
        .consume_one_wake(WakeConsumerPolicy::default(), &mut resolver, &mut adapter)
        .expect_err("activation remains off");
    assert!(error.to_string().contains("explicitly enabled"));
    assert_eq!(resolver.calls, 0);
    assert!(adapter.launched_argv.is_empty());
    assert_eq!(outbox_state(&ledger, &wake_id), "pending");
    assert!(!temp.path().join("wake-consumer.lock").exists());
}
