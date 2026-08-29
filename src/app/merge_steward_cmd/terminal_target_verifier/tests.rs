use super::*;
use std::collections::VecDeque;

type ProbeCall = (String, Vec<String>, BTreeMap<String, Option<String>>);

struct Probe {
    observed: VecDeque<Result<LocalProcessIdentityV1, VerificationFailure>>,
    outputs: VecDeque<Result<CommandOutput, VerificationFailure>>,
    calls: Vec<ProbeCall>,
}

impl TerminalProbe for Probe {
    fn observe_local_process(
        &mut self,
        _: u32,
    ) -> Result<LocalProcessIdentityV1, VerificationFailure> {
        self.observed.pop_front().expect("scripted OS observation")
    }

    fn herdr_selector_is_local(
        &mut self,
        selector: &HerdRSelectorV1,
    ) -> Result<bool, VerificationFailure> {
        Ok(!matches!(selector, HerdRSelectorV1::Session(value) if value == "remote"))
    }

    fn run(
        &mut self,
        program: &str,
        args: &[String],
        environment_overrides: &BTreeMap<String, Option<String>>,
    ) -> Result<CommandOutput, VerificationFailure> {
        self.calls.push((
            program.to_owned(),
            args.to_vec(),
            environment_overrides.clone(),
        ));
        self.outputs.pop_front().expect("scripted output")
    }
}

fn process() -> LocalProcessIdentityV1 {
    LocalProcessIdentityV1 {
        boot_id: "boot-a".into(),
        pid: 42,
        start_identity: "start-a".into(),
    }
}

#[allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    reason = "JSON fixtures are consumed into the probe's Result script"
)]
fn output(value: Value) -> Result<CommandOutput, VerificationFailure> {
    Ok(CommandOutput {
        success: true,
        stdout: value.to_string(),
        stderr: String::new(),
    })
}

#[test]
fn cmux_requires_exact_process_source_resolution_and_surface() {
    let process = process();
    let mut probe = Probe {
        observed: VecDeque::from([Ok(process.clone()), Ok(process.clone())]),
        outputs: VecDeque::from([output(serde_json::json!({"result": {
            "source": "pid", "pid_resolution": "controlling_tty", "pid": 42,
            "surface_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "workspace_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
        }}))]),
        calls: vec![],
    };
    let verified = verify_cmux(
        &mut probe,
        &CmuxExpectation {
            process: &process,
            socket_path: "/tmp/exact-cmux.sock",
            surface_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            captured_lifecycle_correlation: Some("life"),
        },
    )
    .expect("verified");
    assert!(matches!(verified.0.address, TerminalAddressV1::Cmux { .. }));
    assert_eq!(probe.calls[0].1[3], "agent.resolve_delivery_target");
    assert!(probe.calls[0].1[4].contains("controlling_tty"));
    assert!(
        probe.calls[0]
            .1
            .starts_with(&["--socket".into(), "/tmp/exact-cmux.sock".into()])
    );
    assert_eq!(probe.calls[0].2.get("CMUX_SOCKET_PATH"), Some(&None));
    assert_eq!(probe.calls[0].2.get("CMUX_SOCKET"), Some(&None));
}

#[test]
fn cmux_rejects_reused_label_and_method_missing() {
    let process = process();
    let mut reused = Probe {
        observed: VecDeque::from([Ok(process.clone()), Ok(process.clone())]),
        outputs: VecDeque::from([output(
            serde_json::json!({"source": "surface", "pid_resolution": "controlling_tty", "surface_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", "workspace_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"}),
        )]),
        calls: vec![],
    };
    let expected = CmuxExpectation {
        process: &process,
        socket_path: "/tmp/exact-cmux.sock",
        surface_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        captured_lifecycle_correlation: Some("life"),
    };
    assert_eq!(
        verify_cmux(&mut reused, &expected),
        Err(VerificationFailure::NoMatch)
    );

    let mut missing = Probe {
        observed: VecDeque::from([Ok(process.clone()), Ok(process.clone())]),
        outputs: VecDeque::from([Ok(CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: "method_not_found".into(),
        })]),
        calls: vec![],
    };
    assert_eq!(
        verify_cmux(&mut missing, &expected),
        Err(VerificationFailure::MethodMissing)
    );
}

#[test]
fn process_reuse_fails_before_adapter_query() {
    let process = process();
    let mut probe = Probe {
        observed: VecDeque::from([Ok(LocalProcessIdentityV1 {
            start_identity: "reused".into(),
            ..process.clone()
        })]),
        outputs: VecDeque::new(),
        calls: vec![],
    };
    assert_eq!(
        verify_cmux(
            &mut probe,
            &CmuxExpectation {
                process: &process,
                socket_path: "/tmp/exact-cmux.sock",
                surface_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                captured_lifecycle_correlation: Some("life")
            }
        ),
        Err(VerificationFailure::ProcessIdentityChanged)
    );
    assert!(probe.calls.is_empty());
}

#[test]
fn cmux_process_drift_during_query_fails_before_evidence_returns() {
    let process = process();
    let drifted = LocalProcessIdentityV1 {
        start_identity: "reused-after-query".into(),
        ..process.clone()
    };
    let mut probe = Probe {
        observed: VecDeque::from([Ok(process.clone()), Ok(drifted)]),
        outputs: VecDeque::from([output(serde_json::json!({"result": {
            "source": "pid", "pid_resolution": "controlling_tty", "pid": 42,
            "surface_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "workspace_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
        }}))]),
        calls: vec![],
    };
    assert_eq!(
        verify_cmux(
            &mut probe,
            &CmuxExpectation {
                process: &process,
                socket_path: "/tmp/exact-cmux.sock",
                surface_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                captured_lifecycle_correlation: Some("life"),
            }
        ),
        Err(VerificationFailure::ProcessIdentityChanged)
    );
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the fixture is consumed while constructing its scripted response"
)]
fn snapshot(panes: Value) -> Result<CommandOutput, VerificationFailure> {
    output(serde_json::json!({"result": {"snapshot": {"panes": panes}}}))
}

fn process_info(
    pane: &str,
    shell_pid: Option<u32>,
    pids: &[u32],
) -> Result<CommandOutput, VerificationFailure> {
    output(
        serde_json::json!({"result": {"process_info": {"pane_id": pane, "shell_pid": shell_pid, "foreground_processes": pids.iter().map(|pid| serde_json::json!({"pid": pid})).collect::<Vec<_>>()}}}),
    )
}

fn agent(pane: &str, terminal: &str, native: &str) -> Result<CommandOutput, VerificationFailure> {
    output(
        serde_json::json!({"result": {"agent": {"pane_id": pane, "terminal_id": terminal, "agent_session": {"value": native}}}}),
    )
}

#[test]
fn herdr_exact_terminal_process_and_native_session_are_required() {
    let process = process();
    let mut probe = Probe {
        observed: VecDeque::from([Ok(process.clone()), Ok(process.clone())]),
        outputs: VecDeque::from([
            snapshot(serde_json::json!([{"pane_id":"p1", "terminal_id":"t1"}])),
            process_info("p1", None, &[42]),
            agent("p1", "t1", "native-1"),
            process_info("p1", None, &[42]),
        ]),
        calls: vec![],
    };
    let verified = verify_herdr(
        &mut probe,
        &HerdRExpectation {
            process: &process,
            selector: &HerdRSelectorV1::Session("exact".into()),
            terminal_id: "t1",
            native_session_id: "native-1",
            allow_live_handoff_scan: false,
        },
    )
    .expect("verified");
    assert!(matches!(
        verified.0.address,
        TerminalAddressV1::HerdR { .. }
    ));
    assert!(probe.calls.iter().all(|(_, args, env)| {
        args.starts_with(&["--session".into(), "exact".into()])
            && env.get("HERDR_SESSION") == Some(&None)
            && env.get("HERDR_SOCKET_PATH") == Some(&None)
    }));
    assert!(probe.calls.iter().any(|(_, args, _)| {
        args.windows(3)
            .any(|window| window == ["process-info", "--pane", "p1"])
    }));
}

#[test]
fn herdr_remote_selector_fails_before_adapter_queries() {
    let process = process();
    let mut probe = Probe {
        observed: VecDeque::from([Ok(process.clone())]),
        outputs: VecDeque::new(),
        calls: vec![],
    };
    assert_eq!(
        verify_herdr(
            &mut probe,
            &HerdRExpectation {
                process: &process,
                selector: &HerdRSelectorV1::Session("remote".into()),
                terminal_id: "t1",
                native_session_id: "native-1",
                allow_live_handoff_scan: false,
            }
        ),
        Err(VerificationFailure::RemoteOrUnobservable)
    );
    assert!(probe.calls.is_empty());
}

#[test]
fn herdr_same_terminal_may_move_panes_and_shell_pid_may_bind() {
    let process = process();
    let mut probe = Probe {
        observed: VecDeque::from([Ok(process.clone()), Ok(process.clone())]),
        outputs: VecDeque::from([
            snapshot(serde_json::json!([{"pane_id":"new-pane", "terminal_id":"t1"}])),
            process_info("new-pane", Some(42), &[]),
            agent("new-pane", "t1", "native-1"),
            process_info("new-pane", Some(42), &[]),
        ]),
        calls: vec![],
    };
    let verified = verify_herdr(
        &mut probe,
        &HerdRExpectation {
            process: &process,
            selector: &HerdRSelectorV1::Session("exact".into()),
            terminal_id: "t1",
            native_session_id: "native-1",
            allow_live_handoff_scan: false,
        },
    )
    .expect("moved terminal remains the same instance");
    assert!(
        matches!(verified.0.address, TerminalAddressV1::HerdR { pane_id, .. } if pane_id == "new-pane")
    );
}

#[test]
fn herdr_expected_native_session_must_be_observed() {
    let process = process();
    let mut probe = Probe {
        observed: VecDeque::from([Ok(process.clone()), Ok(process.clone())]),
        outputs: VecDeque::from([
            snapshot(serde_json::json!([{"pane_id":"p1", "terminal_id":"t1"}])),
            process_info("p1", None, &[42]),
            output(serde_json::json!({"result": {"agent": {"pane_id":"p1", "terminal_id":"t1"}}})),
        ]),
        calls: vec![],
    };
    assert_eq!(
        verify_herdr(
            &mut probe,
            &HerdRExpectation {
                process: &process,
                selector: &HerdRSelectorV1::Session("exact".into()),
                terminal_id: "t1",
                native_session_id: "native-1",
                allow_live_handoff_scan: false,
            }
        ),
        Err(VerificationFailure::NativeSessionMismatch)
    );
}

#[test]
fn herdr_process_drift_during_queries_fails_before_evidence_returns() {
    let process = process();
    let mut probe = Probe {
        observed: VecDeque::from([
            Ok(process.clone()),
            Ok(LocalProcessIdentityV1 {
                start_identity: "reused-after-query".into(),
                ..process.clone()
            }),
        ]),
        outputs: VecDeque::from([
            snapshot(serde_json::json!([{"pane_id":"p1", "terminal_id":"t1"}])),
            process_info("p1", None, &[42]),
            agent("p1", "t1", "native-1"),
            process_info("p1", None, &[42]),
        ]),
        calls: vec![],
    };
    assert_eq!(
        verify_herdr(
            &mut probe,
            &HerdRExpectation {
                process: &process,
                selector: &HerdRSelectorV1::Session("exact".into()),
                terminal_id: "t1",
                native_session_id: "native-1",
                allow_live_handoff_scan: false,
            }
        ),
        Err(VerificationFailure::ProcessIdentityChanged)
    );
}

#[test]
fn herdr_handoff_scan_requires_one_exact_process() {
    let process = process();
    let expectation = HerdRExpectation {
        process: &process,
        selector: &HerdRSelectorV1::Session("exact".into()),
        terminal_id: "old-terminal",
        native_session_id: "native-1",
        allow_live_handoff_scan: true,
    };
    let mut multiple = Probe {
        observed: VecDeque::from([Ok(process.clone()), Ok(process.clone())]),
        outputs: VecDeque::from([
            snapshot(
                serde_json::json!([{"pane_id":"p1", "terminal_id":"t1"}, {"pane_id":"p2", "terminal_id":"t2"}]),
            ),
            process_info("p1", None, &[42]),
            process_info("p2", None, &[42]),
        ]),
        calls: vec![],
    };
    assert_eq!(
        verify_herdr(&mut multiple, &expectation),
        Err(VerificationFailure::MultipleMatches)
    );
}

#[test]
fn herdr_handoff_scan_binds_agent_to_the_unique_new_terminal() {
    let process = process();
    let mut probe = Probe {
        observed: VecDeque::from([Ok(process.clone()), Ok(process.clone())]),
        outputs: VecDeque::from([
            snapshot(serde_json::json!([{"pane_id":"p1", "terminal_id":"new-terminal"}])),
            process_info("p1", None, &[42]),
            agent("p1", "different-terminal", "native-1"),
        ]),
        calls: vec![],
    };
    assert_eq!(
        verify_herdr(
            &mut probe,
            &HerdRExpectation {
                process: &process,
                selector: &HerdRSelectorV1::Session("exact".into()),
                terminal_id: "gone",
                native_session_id: "native-1",
                allow_live_handoff_scan: true,
            }
        ),
        Err(VerificationFailure::NoMatch)
    );
}

#[test]
fn herdr_socket_selector_fails_closed_without_response_identity() {
    let process = process();
    let mut probe = Probe {
        observed: VecDeque::from([Ok(process.clone())]),
        outputs: VecDeque::new(),
        calls: vec![],
    };
    assert_eq!(
        verify_herdr(
            &mut probe,
            &HerdRExpectation {
                process: &process,
                selector: &HerdRSelectorV1::Socket("/tmp/exact.sock".into()),
                terminal_id: "t1",
                native_session_id: "native-1",
                allow_live_handoff_scan: false,
            }
        ),
        Err(VerificationFailure::Unsupported)
    );
    assert!(probe.calls.is_empty());
}

#[test]
fn demotion_retains_tombstone_and_unbound_never_publishes() {
    let instance = TerminalInstanceV1 {
        process: process(),
        address: TerminalAddressV1::HerdR {
            selector: HerdRSelectorV1::Session("exact".into()),
            terminal_id: "t1".into(),
            pane_id: "p1".into(),
        },
    };
    let demoted =
        TerminalBindingStateV1::bind_verified(VerifiedTerminalInstance(instance.clone())).demote();
    assert_eq!(
        demoted,
        TerminalBindingStateV1::Demoted {
            prior_instance: Some(instance)
        }
    );
    assert!(!demoted.publishes_terminal_adapter());
    let never_bound = TerminalBindingStateV1::LeaderLiveUnbound {
        prior_instance: None,
    }
    .demote();
    assert_eq!(
        never_bound,
        TerminalBindingStateV1::Demoted {
            prior_instance: None
        }
    );
    assert!(!never_bound.publishes_terminal_adapter());
}
