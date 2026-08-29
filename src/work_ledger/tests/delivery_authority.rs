use super::*;

struct Probe {
    github: Result<GitHubAuthorityObservation, DeliveryAuthorityRefusal>,
    terminal: Result<TerminalAuthorityObservation, DeliveryAuthorityRefusal>,
    terminal_calls: usize,
}

impl DeliveryAuthorityProbe for Probe {
    fn observe_github(
        &mut self,
        _expected: &DeliveryAuthorityExpectation,
    ) -> Result<GitHubAuthorityObservation, DeliveryAuthorityRefusal> {
        self.github.clone()
    }

    fn verify_terminal_once(
        &mut self,
        _expected: &DeliveryAuthorityExpectation,
    ) -> Result<TerminalAuthorityObservation, DeliveryAuthorityRefusal> {
        self.terminal_calls += 1;
        self.terminal.clone()
    }
}

fn expectation() -> DeliveryAuthorityExpectation {
    DeliveryAuthorityExpectation {
        installation_id: 42,
        repository: "danielraffel/pulp".to_owned(),
        pull_request: 148,
        head_sha: "a".repeat(40),
        base_ref: "main".to_owned(),
        base_sha: "c".repeat(40),
        requested_terminal_instance: "cmux:surface-a".to_owned(),
        requested_process: ProcessIncarnation {
            boot_id: "boot-a".to_owned(),
            pid: 101,
            start_identity: "start-a".to_owned(),
        },
        native_session_id: "codex-session-a".to_owned(),
        source_work_generation: 6,
        source_owner_generation: 2,
        target_work_generation: 6,
        target_owner_generation: 2,
    }
}

fn probe(now: DateTime<Utc>) -> Probe {
    Probe {
        github: Ok(GitHubAuthorityObservation {
            app_authenticated: true,
            installation_id: 42,
            repository: "danielraffel/pulp".to_owned(),
            pull_request: 148,
            head_sha: "a".repeat(40),
            base_ref: "main".to_owned(),
            base_sha: "c".repeat(40),
            observed_at: now,
        }),
        terminal: Ok(TerminalAuthorityObservation {
            requested_terminal_instance: "cmux:surface-a".to_owned(),
            actual_terminal_instance: "cmux:surface-a".to_owned(),
            process: ProcessIncarnation {
                boot_id: "boot-a".to_owned(),
                pid: 101,
                start_identity: "start-a".to_owned(),
            },
            native_session_id: "codex-session-a".to_owned(),
            mutation_endpoint: TerminalMutationEndpoint::Cmux {
                executable_path: "/test/cmux-a".to_owned(),
                socket_path: "/test/cmux-a.sock".to_owned(),
            },
            observed_at: now,
        }),
        terminal_calls: 0,
    }
}

#[test]
fn exact_current_route_produces_one_shot_authority() {
    let now = Utc::now();
    let mut first_probe = probe(now);
    let authority =
        verify_delivery_authority_at(&mut first_probe, &expectation(), now).expect("authority");
    assert_eq!(first_probe.terminal_calls, 1);
    assert_eq!(authority.terminal_instance(), "cmux:surface-a");
    assert_eq!(authority.receipt_digest().len(), 64);
}

#[test]
fn dead_original_occupant_does_not_block_read_only_reconciliation_authority() {
    let now = Utc::now();
    let mut probe = probe(now);
    probe.terminal = Err(DeliveryAuthorityRefusal::NoTerminalMatch);
    let endpoint = TerminalMutationEndpoint::Cmux {
        executable_path: "/Applications/cmux.app/Contents/MacOS/cmux".to_owned(),
        socket_path: "/tmp/cmux.sock".to_owned(),
    };
    let authority = verify_reconciliation_authority(
        &mut probe,
        &expectation(),
        endpoint.clone(),
        "f".repeat(64),
    )
    .expect("fresh App/head evidence authorizes only the exact read-only lookup");
    assert_eq!(probe.terminal_calls, 0);
    assert_eq!(authority.terminal_endpoint(), &endpoint);
    assert_eq!(authority.fence_digest(), "f".repeat(64));
}

#[test]
fn live_original_stays_in_place_and_dead_original_alone_authorizes_fresh_checkpoint() {
    let now = Utc::now();
    let endpoint = TerminalMutationEndpoint::Cmux {
        executable_path: "/Applications/cmux.app/Contents/MacOS/cmux".to_owned(),
        socket_path: "/tmp/cmux.sock".to_owned(),
    };
    let mut live = probe(now);
    let live_authority = verify_delivery_or_fresh_authority(
        &mut live,
        &expectation(),
        endpoint.clone(),
        &"f".repeat(64),
    )
    .expect("live original authority");
    assert!(!live_authority.is_fresh_checkpoint());
    assert_eq!(live_authority.terminal_instance(), "cmux:surface-a");

    for refusal in [
        DeliveryAuthorityRefusal::NoTerminalMatch,
        DeliveryAuthorityRefusal::ProcessIncarnationMismatch,
    ] {
        let mut dead = probe(now);
        dead.terminal = Err(refusal);
        let fresh = verify_delivery_or_fresh_authority(
            &mut dead,
            &expectation(),
            endpoint.clone(),
            &"f".repeat(64),
        )
        .expect("definitively dead original authorizes fresh checkpoint");
        assert!(fresh.is_fresh_checkpoint());
        assert_eq!(dead.terminal_calls, 1);
        assert_eq!(
            fresh
                .into_mutation_endpoint_for(6, 2)
                .expect("exact generation"),
            endpoint
        );
    }
}

#[test]
fn ambiguous_or_unobservable_original_never_authorizes_fresh_checkpoint() {
    let now = Utc::now();
    for refusal in [
        DeliveryAuthorityRefusal::TerminalAuthorityUnavailable,
        DeliveryAuthorityRefusal::MultipleTerminalMatches,
        DeliveryAuthorityRefusal::NativeSessionMismatch,
    ] {
        let mut ambiguous = probe(now);
        ambiguous.terminal = Err(refusal);
        assert_eq!(
            verify_delivery_or_fresh_authority(
                &mut ambiguous,
                &expectation(),
                TerminalMutationEndpoint::Cmux {
                    executable_path: "/test/cmux".to_owned(),
                    socket_path: "/test/cmux.sock".to_owned(),
                },
                &"f".repeat(64),
            ),
            Err(refusal)
        );
    }
}

#[test]
fn authorization_cannot_be_reused_with_altered_ledger_generations() {
    let now = Utc::now();
    let mut second_probe = probe(now);
    let authority =
        verify_delivery_authority_at(&mut second_probe, &expectation(), now).expect("authority");
    assert_eq!(
        authority.into_mutation_endpoint_for(7, 2),
        Err(DeliveryAuthorityRefusal::GenerationMismatch)
    );

    let mut second_probe = probe(now);
    let authority =
        verify_delivery_authority_at(&mut second_probe, &expectation(), now).expect("authority");
    assert_eq!(
        authority.into_mutation_endpoint_for(6, 3),
        Err(DeliveryAuthorityRefusal::GenerationMismatch)
    );
}

#[test]
fn head_or_base_drift_refuses_before_terminal_io() {
    let now = Utc::now();
    for drift in ["head", "base"] {
        let mut probe = probe(now);
        let github = probe.github.as_mut().expect("github");
        if drift == "head" {
            github.head_sha = "b".repeat(40);
        } else {
            github.base_ref = "release".to_owned();
        }
        let error = verify_delivery_authority_at(&mut probe, &expectation(), now)
            .expect_err("drift must refuse");
        assert!(matches!(
            error,
            DeliveryAuthorityRefusal::HeadMismatch | DeliveryAuthorityRefusal::BaseRefMismatch
        ));
        assert_eq!(probe.terminal_calls, 0);
    }
}

#[test]
fn moved_terminal_requires_separate_reconciliation_authority() {
    let now = Utc::now();
    let mut first_probe = probe(now);
    let terminal = first_probe.terminal.as_mut().expect("terminal");
    terminal.actual_terminal_instance = "cmux:surface-b".to_owned();
    assert_eq!(
        verify_delivery_authority_at(&mut first_probe, &expectation(), now),
        Err(DeliveryAuthorityRefusal::TerminalInstanceMismatch)
    );
}

#[test]
fn exact_terminal_match_never_waives_process_incarnation() {
    let now = Utc::now();
    let mut hostile = probe(now);
    let terminal = hostile.terminal.as_mut().expect("terminal");
    terminal.process.start_identity = "different-process".to_owned();
    assert_eq!(
        verify_delivery_authority_at(&mut hostile, &expectation(), now),
        Err(DeliveryAuthorityRefusal::ProcessIncarnationMismatch)
    );
}

#[test]
fn dead_or_reused_process_and_unavailable_adapter_fail_closed() {
    let now = Utc::now();
    let mut dead_probe = probe(now);
    dead_probe
        .terminal
        .as_mut()
        .expect("terminal")
        .process
        .start_identity = "reused-start".to_owned();
    assert_eq!(
        verify_delivery_authority_at(&mut dead_probe, &expectation(), now),
        Err(DeliveryAuthorityRefusal::ProcessIncarnationMismatch)
    );

    for refusal in [
        DeliveryAuthorityRefusal::MethodMissing,
        DeliveryAuthorityRefusal::NoTerminalMatch,
        DeliveryAuthorityRefusal::MultipleTerminalMatches,
    ] {
        let mut unavailable_probe = probe(now);
        unavailable_probe.terminal = Err(refusal);
        assert_eq!(
            verify_delivery_authority_at(&mut unavailable_probe, &expectation(), now),
            Err(refusal)
        );
    }
}
