//! Tests for the bounded self-heal gate.
//!
//! The failure mode this module guards is **destructive and irreversible**, so
//! every check ships a planted negative control that must go red: for each
//! authorisation, the safe input is asserted to produce `Act` and a minimally
//! different unsafe input is asserted to produce `Escalate` or `Nothing`. The
//! pairs are kept adjacent and named for each other, because a gate that cannot
//! fail its own test authorises everything.
//!
//! Two controls are worth naming outright:
//!
//! * the **stop control** — the same fully-proven orphan, proposed as a stop
//!   rather than a destroy, must be refused as insufficient. A stop reclaims
//!   memory and leaves `no free clone id` exactly where it was;
//! * the **unreadable-provenance control** — the same clone, byte-for-byte,
//!   with only the provenance read failing, must escalate. Only the proof
//!   differs, which is the whole point.

use chrono::{Duration, TimeZone, Utc};

use super::*;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 4, 23, 44, 0).unwrap()
}

/// The orphan from the incident: VMID 200, an hour past a 30-minute TTL,
/// every idleness fact read and every one at rest.
fn proven_idle_orphan() -> CloneObservation {
    CloneObservation {
        vmid: 200,
        name: "pulp-build-vm-200".to_owned(),
        created_at: now() - Duration::minutes(60),
        provenance: Some(CloneProvenance {
            lane: LaneKind::Ordinary,
            job_state: JobState::Terminal,
            ttl_secs: 1800,
        }),
        provenance_boundary: None,
        runner_listener_running: Some(false),
        load_average: Some(0.0),
        registered_runner: Some(false),
    }
}

fn ordinary_safety() -> Safety {
    Safety::ordinary()
}

fn blind_supervisor_with_demand() -> SupervisorObservation {
    SupervisorObservation {
        lane: "pulp-gate-fast".to_owned(),
        consecutive_blind_cycles: 6,
        blind_cycles_before_restart: DEFAULT_BLIND_CYCLES_BEFORE_RESTART,
        queued_demand: 2,
    }
}

fn hop(name: &str, reachable: bool, failures: u32, budget: u32) -> RelayHop {
    RelayHop {
        name: name.to_owned(),
        reachable,
        connect_failures: failures,
        connect_budget: budget,
    }
}

// ---------------------------------------------------------------------------
// Reaping a clone that outlived its job: destroy, never stop.
// ---------------------------------------------------------------------------

#[test]
fn proven_idle_orphan_past_ttl_is_authorised_to_destroy() {
    let decision = decide_clone_reap(&proven_idle_orphan(), ReapMode::Destroy, now());

    assert!(decision.is_act(), "expected Act, got {decision:?}");
    assert_eq!(
        decision.action(),
        Some(&Action::DestroyClone {
            vmid: 200,
            name: "pulp-build-vm-200".to_owned(),
        }),
        "the authorised remedy must be a destroy, which is what releases the VMID"
    );
}

#[test]
fn control_stopping_the_same_orphan_is_refused_as_insufficient() {
    // Planted control for `proven_idle_orphan_past_ttl_is_authorised_to_destroy`:
    // identical clone, identical proof, only the proposed remedy differs.
    let decision = decide_clone_reap(&proven_idle_orphan(), ReapMode::Stop, now());

    let escalation = decision
        .escalation()
        .expect("a stop must be refused, not authorised");
    assert_eq!(escalation.refusal, Refusal::InsufficientRemedy);
    assert!(
        escalation.detail.contains("VMID"),
        "the refusal must cite the VMID, not memory: {}",
        escalation.detail
    );
    assert!(
        escalation.detail.contains("no free clone id"),
        "the refusal must name the failure a stop leaves in place: {}",
        escalation.detail
    );
    assert!(!decision.is_act());
    assert!(decision.action().is_none());
}

#[test]
fn no_approved_action_can_express_stopping_a_clone() {
    // The remedy lesson is encoded in the type, not only in the branch above:
    // whatever a caller proposes, the approval enum cannot carry a stop.
    for mode in [ReapMode::Stop, ReapMode::Destroy] {
        let decision = decide_clone_reap(&proven_idle_orphan(), mode, now());
        if let Some(action) = decision.action() {
            assert_eq!(action.kind(), "destroy_clone");
        }
    }
    assert!(ReapMode::Destroy.frees_vmid());
    assert!(
        !ReapMode::Stop.frees_vmid(),
        "a stopped clone still holds its pool ID"
    );
}

#[test]
fn control_same_orphan_with_unreadable_provenance_escalates() {
    // Planted control: byte-for-byte the authorised orphan, except the
    // provenance read failed. Only the proof differs.
    let mut observation = proven_idle_orphan();
    observation.provenance = None;
    observation.provenance_boundary = Some(Boundary::Transport);

    let decision = decide_clone_reap(&observation, ReapMode::Destroy, now());

    let escalation = decision
        .escalation()
        .expect("unreadable provenance must escalate, never act");
    assert_eq!(
        escalation.refusal,
        Refusal::NeverTouch(NeverTouch::UnreadableProvenance {
            boundary: Boundary::Transport
        })
    );
    assert_eq!(escalation.verdict, ServiceVerdict::Unknown);
    assert_eq!(escalation.boundary, Some(Boundary::Transport));
    assert!(!decision.is_act());
}

#[test]
fn clone_inside_its_ttl_is_left_alone() {
    // The live reaper logs `SKIP 200 — clone is only 88s old`. A self-heal that
    // touches fresh clones is a wrecking ball: every healthy job starts here.
    let mut observation = proven_idle_orphan();
    observation.created_at = now() - Duration::seconds(88);

    let decision = decide_clone_reap(&observation, ReapMode::Destroy, now());

    match &decision {
        SelfHealDecision::Nothing { reason } => {
            assert!(reason.contains("88s old"), "reason was: {reason}");
            assert!(reason.contains("SKIP 200"), "reason was: {reason}");
        }
        other => panic!("expected Nothing for a clone inside its TTL, got {other:?}"),
    }
    assert!(!decision.is_act());
}

#[test]
fn control_the_same_clone_one_second_past_ttl_is_authorised() {
    // Paired with the fresh-clone test: the TTL boundary is the only thing that
    // moved, so the skip above is a TTL decision and not an inert branch.
    let mut observation = proven_idle_orphan();
    observation.created_at = now() - Duration::seconds(1800);

    assert!(decide_clone_reap(&observation, ReapMode::Destroy, now()).is_act());
}

#[test]
fn partial_idle_proof_escalates_rather_than_acting() {
    let mut observation = proven_idle_orphan();
    observation.load_average = None;
    observation.registered_runner = None;

    let decision = decide_clone_reap(&observation, ReapMode::Destroy, now());

    let escalation = decision
        .escalation()
        .expect("a partial proof must escalate");
    assert_eq!(escalation.refusal, Refusal::IdleUnproven);
    assert!(escalation.detail.contains("load"), "{}", escalation.detail);
    assert!(
        escalation.detail.contains("runner_registration"),
        "{}",
        escalation.detail
    );
}

#[test]
fn a_busy_clone_past_its_ttl_escalates() {
    let mut observation = proven_idle_orphan();
    observation.runner_listener_running = Some(true);

    let decision = decide_clone_reap(&observation, ReapMode::Destroy, now());

    assert_eq!(
        decision.escalation().map(|e| e.refusal),
        Some(Refusal::IdleUnproven)
    );
}

#[test]
fn idle_proof_reads_each_fact_it_claims_to_read() {
    assert_eq!(prove_clone_idle(&proven_idle_orphan()), IdleProof::Proven);

    let mut busy_load = proven_idle_orphan();
    busy_load.load_average = Some(2.5);
    assert!(matches!(
        prove_clone_idle(&busy_load),
        IdleProof::Busy {
            fact: IdleFact::Load,
            ..
        }
    ));

    let mut still_registered = proven_idle_orphan();
    still_registered.registered_runner = Some(true);
    assert!(matches!(
        prove_clone_idle(&still_registered),
        IdleProof::Busy {
            fact: IdleFact::RunnerRegistration,
            ..
        }
    ));

    let mut unread = proven_idle_orphan();
    unread.runner_listener_running = None;
    assert_eq!(
        prove_clone_idle(&unread),
        IdleProof::Partial {
            unread: vec![IdleFact::RunnerListener]
        }
    );
}

// ---------------------------------------------------------------------------
// Never-touch: three hard refusals that no idle proof overrides.
// ---------------------------------------------------------------------------

#[test]
fn never_touch_running_release_build_even_when_fully_proven_idle() {
    // Every other precondition is satisfied: past TTL, destroy proposed, all
    // four idleness facts read and at rest. A release build parked waiting on
    // notarization gives exactly this reading.
    let mut observation = proven_idle_orphan();
    observation.provenance = Some(CloneProvenance {
        lane: LaneKind::ReleaseBuild,
        job_state: JobState::Active,
        ttl_secs: 1800,
    });

    assert_eq!(prove_clone_idle(&observation), IdleProof::Proven);

    let decision = decide_clone_reap(&observation, ReapMode::Destroy, now());
    let escalation = decision
        .escalation()
        .expect("a release build is never touched");
    assert_eq!(
        escalation.refusal,
        Refusal::NeverTouch(NeverTouch::RunningReleaseBuild)
    );
    assert!(
        escalation.detail.contains("No idle proof overrides this"),
        "{}",
        escalation.detail
    );
}

#[test]
fn a_release_build_that_reported_terminal_is_no_longer_in_flight() {
    // Paired with the test above, and the reason the two never-touch cases are
    // deliberately asymmetric: a release build is off-limits while it is
    // *running*, whereas a gate VM is off-limits full stop. Without this pair
    // the release-build refusal could be an unconditional lane ban that nobody
    // noticed was broader than intended.
    let mut observation = proven_idle_orphan();
    observation.provenance = Some(CloneProvenance {
        lane: LaneKind::ReleaseBuild,
        job_state: JobState::Terminal,
        ttl_secs: 1800,
    });

    assert_eq!(never_touch(&observation.safety()), None);
    assert!(decide_clone_reap(&observation, ReapMode::Destroy, now()).is_act());

    // …and an unread job state is treated as in-flight, not as terminal.
    observation.provenance = Some(CloneProvenance {
        lane: LaneKind::ReleaseBuild,
        job_state: JobState::Unknown,
        ttl_secs: 1800,
    });
    assert_eq!(
        never_touch(&observation.safety()),
        Some(NeverTouch::RunningReleaseBuild)
    );
}

#[test]
fn never_touch_required_gate_vm_even_when_fully_proven_idle_and_terminal() {
    // Strictly stronger than the release-build case: the job is terminal, the
    // TTL is blown and the proof is complete. It is still refused.
    let mut observation = proven_idle_orphan();
    observation.provenance = Some(CloneProvenance {
        lane: LaneKind::RequiredGate,
        job_state: JobState::Terminal,
        ttl_secs: 1800,
    });

    assert_eq!(prove_clone_idle(&observation), IdleProof::Proven);
    assert!(observation.is_past_ttl(now()));

    let decision = decide_clone_reap(&observation, ReapMode::Destroy, now());
    assert_eq!(
        decision.escalation().map(|e| e.refusal),
        Some(Refusal::NeverTouch(NeverTouch::RequiredGateVm))
    );
}

#[test]
fn never_touch_unreadable_provenance_even_when_every_other_fact_is_idle() {
    let mut observation = proven_idle_orphan();
    observation.provenance = None;
    observation.provenance_boundary = Some(Boundary::Permission);

    assert!(observation.runner_listener_running == Some(false));
    assert!(observation.registered_runner == Some(false));

    let decision = decide_clone_reap(&observation, ReapMode::Destroy, now());
    assert_eq!(
        decision.escalation().map(|e| e.refusal),
        Some(Refusal::NeverTouch(NeverTouch::UnreadableProvenance {
            boundary: Boundary::Permission
        }))
    );
}

#[test]
fn control_the_same_clone_with_an_ordinary_lane_is_authorised() {
    // Planted control for the three never-touch tests above: swap only the
    // lane back to ordinary and the identical input is authorised, so the
    // refusals are attributable to the never-touch list and nothing else.
    assert!(decide_clone_reap(&proven_idle_orphan(), ReapMode::Destroy, now()).is_act());
}

#[test]
fn never_touch_is_checked_before_the_fault_preconditions() {
    // A proposal aimed at a gate VM is a defect in the caller's target
    // selection. Swallowing it as `Nothing` because the TTL had not elapsed
    // would hide that defect until the day it has.
    let mut observation = proven_idle_orphan();
    observation.created_at = now() - Duration::seconds(10);
    observation.provenance = Some(CloneProvenance {
        lane: LaneKind::RequiredGate,
        job_state: JobState::Active,
        ttl_secs: 1800,
    });

    assert_eq!(
        decide_clone_reap(&observation, ReapMode::Destroy, now())
            .escalation()
            .map(|e| e.refusal),
        Some(Refusal::NeverTouch(NeverTouch::RequiredGateVm))
    );
}

#[test]
fn never_touch_classifies_each_case_from_safety_alone() {
    assert_eq!(never_touch(&Safety::ordinary()), None);
    assert_eq!(
        never_touch(&Safety {
            release_build_in_flight: true,
            ..Safety::ordinary()
        }),
        Some(NeverTouch::RunningReleaseBuild)
    );
    assert_eq!(
        never_touch(&Safety {
            serves_required_gate: true,
            ..Safety::ordinary()
        }),
        Some(NeverTouch::RequiredGateVm)
    );
    assert_eq!(
        never_touch(&Safety {
            provenance_unreadable: Some(Boundary::Scope),
            ..Safety::ordinary()
        }),
        Some(NeverTouch::UnreadableProvenance {
            boundary: Boundary::Scope
        })
    );
}

// ---------------------------------------------------------------------------
// Restarting a blind supervisor: blindness AND demand, never one alone.
// ---------------------------------------------------------------------------

#[test]
fn blind_supervisor_with_queued_demand_is_authorised_to_restart() {
    let decision = decide_supervisor_restart(
        &blind_supervisor_with_demand(),
        &ordinary_safety(),
        &IdleProof::Proven,
        now(),
    );

    assert_eq!(
        decision.action(),
        Some(&Action::RestartSupervisor {
            lane: "pulp-gate-fast".to_owned()
        })
    );
}

#[test]
fn control_the_same_blind_supervisor_with_no_demand_is_left_alone() {
    // Planted control for the test above: identical blindness, demand removed.
    // A blind supervisor nobody is waiting on is not urgent, and restarting it
    // is churn against the component whose restart orphans half-created clones.
    let mut observation = blind_supervisor_with_demand();
    observation.queued_demand = 0;

    let decision =
        decide_supervisor_restart(&observation, &ordinary_safety(), &IdleProof::Proven, now());

    match &decision {
        SelfHealDecision::Nothing { reason } => {
            assert!(reason.contains("nothing is queued"), "reason was: {reason}");
        }
        other => panic!("expected Nothing with no demand, got {other:?}"),
    }
    assert!(!decision.is_act());
}

#[test]
fn supervisor_blind_under_threshold_is_left_alone_even_with_demand() {
    let mut observation = blind_supervisor_with_demand();
    observation.consecutive_blind_cycles = 1;

    let decision =
        decide_supervisor_restart(&observation, &ordinary_safety(), &IdleProof::Proven, now());

    assert!(matches!(decision, SelfHealDecision::Nothing { .. }));
}

#[test]
fn supervisor_restart_escalates_without_an_idle_proof() {
    for proof in [
        IdleProof::Absent,
        IdleProof::Partial {
            unread: vec![IdleFact::RunnerListener],
        },
    ] {
        let decision = decide_supervisor_restart(
            &blind_supervisor_with_demand(),
            &ordinary_safety(),
            &proof,
            now(),
        );
        assert_eq!(
            decision.escalation().map(|e| e.refusal),
            Some(Refusal::IdleUnproven),
            "proof {} must escalate, not act",
            proof.kind()
        );
    }
}

#[test]
fn supervisor_restart_refuses_every_never_touch_case_despite_a_full_proof() {
    let cases = [
        (
            Safety {
                release_build_in_flight: true,
                ..Safety::ordinary()
            },
            NeverTouch::RunningReleaseBuild,
        ),
        (
            Safety {
                serves_required_gate: true,
                ..Safety::ordinary()
            },
            NeverTouch::RequiredGateVm,
        ),
        (
            Safety {
                provenance_unreadable: Some(Boundary::Identity),
                ..Safety::ordinary()
            },
            NeverTouch::UnreadableProvenance {
                boundary: Boundary::Identity,
            },
        ),
    ];

    for (safety, expected) in cases {
        let decision = decide_supervisor_restart(
            &blind_supervisor_with_demand(),
            &safety,
            &IdleProof::Proven,
            now(),
        );
        assert_eq!(
            decision.escalation().map(|e| e.refusal),
            Some(Refusal::NeverTouch(expected)),
            "idle proof must not override {expected:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Relay hops: never sever the chain.
// ---------------------------------------------------------------------------

#[test]
fn dropping_a_spent_hop_that_leaves_a_healthy_one_is_authorised() {
    let topology = RelayTopology {
        hops: vec![hop("relay-a", false, 9, 3), hop("relay-b", true, 0, 3)],
    };
    let change = RelayChange::DropHop {
        hop: "relay-a".to_owned(),
    };

    let decision = decide_relay_change(
        &topology,
        &change,
        &ordinary_safety(),
        &IdleProof::Proven,
        now(),
    );

    assert_eq!(
        decision.action(),
        Some(&Action::DropRelayHop {
            hop: "relay-a".to_owned()
        })
    );
}

#[test]
fn control_dropping_the_last_healthy_hop_is_refused() {
    // Planted control for the test above: the surviving hop is spent too, so
    // the same drop would leave the relay with nothing carrying it.
    let topology = RelayTopology {
        hops: vec![hop("relay-a", false, 9, 3), hop("relay-b", false, 5, 3)],
    };
    let change = RelayChange::DropHop {
        hop: "relay-a".to_owned(),
    };

    let decision = decide_relay_change(
        &topology,
        &change,
        &ordinary_safety(),
        &IdleProof::Proven,
        now(),
    );

    let escalation = decision
        .escalation()
        .expect("severing the relay must be refused");
    assert_eq!(escalation.refusal, Refusal::WouldSeverRelay);
    assert!(
        escalation.detail.contains("0 healthy hops"),
        "{}",
        escalation.detail
    );
    assert!(!decision.is_act());
}

#[test]
fn dropping_a_hop_inside_its_budget_is_not_a_fault() {
    let topology = RelayTopology {
        hops: vec![hop("relay-a", true, 1, 3), hop("relay-b", true, 0, 3)],
    };
    let change = RelayChange::DropHop {
        hop: "relay-a".to_owned(),
    };

    assert!(matches!(
        decide_relay_change(
            &topology,
            &change,
            &ordinary_safety(),
            &IdleProof::Proven,
            now()
        ),
        SelfHealDecision::Nothing { .. }
    ));
}

#[test]
fn reordering_never_severs_and_is_authorised_when_a_hop_is_spent() {
    let topology = RelayTopology {
        hops: vec![hop("relay-a", true, 9, 3), hop("relay-b", true, 0, 3)],
    };
    let change = RelayChange::Reorder {
        order: vec!["relay-b".to_owned(), "relay-a".to_owned()],
    };

    assert_eq!(
        decide_relay_change(
            &topology,
            &change,
            &ordinary_safety(),
            &IdleProof::Proven,
            now()
        )
        .action(),
        Some(&Action::ReorderRelayHops {
            order: vec!["relay-b".to_owned(), "relay-a".to_owned()]
        })
    );
}

#[test]
fn control_reordering_a_chain_with_no_healthy_hop_is_refused() {
    let topology = RelayTopology {
        hops: vec![hop("relay-a", true, 9, 3), hop("relay-b", false, 9, 3)],
    };
    let change = RelayChange::Reorder {
        order: vec!["relay-b".to_owned(), "relay-a".to_owned()],
    };

    assert_eq!(
        decide_relay_change(
            &topology,
            &change,
            &ordinary_safety(),
            &IdleProof::Proven,
            now()
        )
        .escalation()
        .map(|e| e.refusal),
        Some(Refusal::WouldSeverRelay)
    );
}

#[test]
fn a_relay_proposal_naming_an_unobserved_hop_is_refused() {
    let topology = RelayTopology {
        hops: vec![hop("relay-a", true, 9, 3), hop("relay-b", true, 0, 3)],
    };

    for change in [
        RelayChange::DropHop {
            hop: "relay-z".to_owned(),
        },
        RelayChange::Reorder {
            order: vec!["relay-a".to_owned(), "relay-z".to_owned()],
        },
    ] {
        assert_eq!(
            decide_relay_change(
                &topology,
                &change,
                &ordinary_safety(),
                &IdleProof::Proven,
                now()
            )
            .escalation()
            .map(|e| e.refusal),
            Some(Refusal::UnknownTarget)
        );
    }
}

#[test]
fn relay_change_escalates_without_an_idle_proof() {
    let topology = RelayTopology {
        hops: vec![hop("relay-a", false, 9, 3), hop("relay-b", true, 0, 3)],
    };
    let change = RelayChange::DropHop {
        hop: "relay-a".to_owned(),
    };

    assert_eq!(
        decide_relay_change(
            &topology,
            &change,
            &ordinary_safety(),
            &IdleProof::Absent,
            now()
        )
        .escalation()
        .map(|e| e.refusal),
        Some(Refusal::IdleUnproven)
    );
}

#[test]
fn relay_change_refuses_a_never_touch_target_despite_a_full_proof() {
    let topology = RelayTopology {
        hops: vec![hop("relay-a", false, 9, 3), hop("relay-b", true, 0, 3)],
    };
    let change = RelayChange::DropHop {
        hop: "relay-a".to_owned(),
    };
    let safety = Safety {
        serves_required_gate: true,
        ..Safety::ordinary()
    };

    assert_eq!(
        decide_relay_change(&topology, &change, &safety, &IdleProof::Proven, now())
            .escalation()
            .map(|e| e.refusal),
        Some(Refusal::NeverTouch(NeverTouch::RequiredGateVm))
    );
}

// ---------------------------------------------------------------------------
// The escalation payload has to be renderable without re-deriving anything.
// ---------------------------------------------------------------------------

#[test]
fn an_escalation_carries_the_proposal_the_refusal_and_the_human_action() {
    let decision = decide_clone_reap(&proven_idle_orphan(), ReapMode::Stop, now());
    let escalation = decision.escalation().expect("stop is refused");

    assert_eq!(
        escalation.proposed,
        Proposal::ReapClone {
            vmid: 200,
            mode: ReapMode::Stop
        }
    );
    assert_eq!(escalation.decided_at, now());
    assert!(escalation.human_action.contains("destroy"));
    assert!(!escalation.detail.is_empty());
}

#[test]
fn a_refusal_born_of_blindness_rolls_up_as_unknown_not_degraded() {
    // An instrument that could not read must never roll up as a healthy fleet
    // that merely declined to act.
    let mut unreadable = proven_idle_orphan();
    unreadable.provenance = None;
    unreadable.provenance_boundary = Some(Boundary::Scope);

    let blind = decide_clone_reap(&unreadable, ReapMode::Destroy, now());
    assert_eq!(
        blind.escalation().map(|e| e.verdict),
        Some(ServiceVerdict::Unknown)
    );

    // Control: a refusal that measured everything and still said no is
    // `Degraded`, so the `Unknown` above is attributable to the failed read.
    let seeing = decide_clone_reap(&proven_idle_orphan(), ReapMode::Stop, now());
    assert_eq!(
        seeing.escalation().map(|e| e.verdict),
        Some(ServiceVerdict::Degraded)
    );
}

#[test]
fn no_decision_path_ever_yields_an_action_the_caller_did_not_earn() {
    // The safety property in one assertion: across a sweep of inputs, `Act` is
    // returned only when the target is off the never-touch list, the fault
    // precondition holds, and idleness is proven.
    let mut acted = 0;
    for lane in [
        LaneKind::Ordinary,
        LaneKind::ReleaseBuild,
        LaneKind::RequiredGate,
    ] {
        for job_state in [JobState::Active, JobState::Terminal, JobState::Unknown] {
            for listener in [Some(false), Some(true), None] {
                for mode in [ReapMode::Stop, ReapMode::Destroy] {
                    let mut observation = proven_idle_orphan();
                    observation.provenance = Some(CloneProvenance {
                        lane,
                        job_state,
                        ttl_secs: 1800,
                    });
                    observation.runner_listener_running = listener;

                    if decide_clone_reap(&observation, mode, now()).is_act() {
                        acted += 1;
                        assert_eq!(never_touch(&observation.safety()), None);
                        assert_eq!(mode, ReapMode::Destroy);
                        assert_eq!(listener, Some(false));
                        assert_eq!(prove_clone_idle(&observation), IdleProof::Proven);
                        assert_ne!(lane, LaneKind::RequiredGate);
                    }
                }
            }
        }
    }
    // Ordinary in any job state (3), plus a release build that reported
    // terminal (1). Every required-gate combination, every stop, every unread
    // or busy listener, and every in-flight release build is excluded.
    assert_eq!(
        acted, 4,
        "only an off-list, destroy-proposed, provably idle clone may act"
    );
}
