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
            source_work_generation: 6,
            source_owner_generation: 2,
            target_work_generation: 6,
            target_owner_generation: 2,
            transactionally_rebound: false,
            observed_at: now,
        }),
        terminal_calls: 0,
    }
}

#[test]
fn exact_current_route_produces_one_shot_authority() {
    let now = Utc::now();
    let mut probe = probe(now);
    let authority =
        verify_delivery_authority_at(&mut probe, &expectation(), now).expect("authority");
    assert_eq!(probe.terminal_calls, 1);
    assert_eq!(authority.terminal_instance(), "cmux:surface-a");
    assert_eq!(authority.receipt_digest().len(), 64);
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
fn moved_terminal_requires_verified_rebind_and_exact_generations() {
    let now = Utc::now();
    let mut first_probe = probe(now);
    let terminal = first_probe.terminal.as_mut().expect("terminal");
    terminal.actual_terminal_instance = "cmux:surface-b".to_owned();
    assert_eq!(
        verify_delivery_authority_at(&mut first_probe, &expectation(), now),
        Err(DeliveryAuthorityRefusal::TerminalInstanceMismatch)
    );

    let mut rebound_probe = probe(now);
    let terminal = rebound_probe.terminal.as_mut().expect("terminal");
    terminal.actual_terminal_instance = "cmux:surface-b".to_owned();
    terminal.transactionally_rebound = true;
    terminal.target_owner_generation += 1;
    assert_eq!(
        verify_delivery_authority_at(&mut rebound_probe, &expectation(), now),
        Err(DeliveryAuthorityRefusal::GenerationMismatch)
    );
}

#[test]
fn claimed_rebind_never_waives_process_incarnation() {
    let now = Utc::now();
    let mut hostile = probe(now);
    let terminal = hostile.terminal.as_mut().expect("terminal");
    terminal.actual_terminal_instance = "cmux:surface-b".to_owned();
    terminal.transactionally_rebound = true;
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
