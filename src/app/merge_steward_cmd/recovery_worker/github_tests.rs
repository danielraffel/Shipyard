use super::*;
use crate::recovery_worker::RecoveryRequiredCheck;

fn app_bound_request() -> RecoveryRequest {
    RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        vec![RecoveryRequiredCheck {
            context: "macos".to_owned(),
            app_id: Some(42),
        }],
        vec![RecoveryFailureFact::RequiredCheck {
            context: "macos".to_owned(),
            app_id: Some(42),
            conclusion: "FAILURE".to_owned(),
            run_id: None,
        }],
        "steward-policy",
        "worker-config",
    )
    .expect("request")
}

fn unbound_request() -> RecoveryRequest {
    RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        vec![RecoveryRequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
        }],
        vec![RecoveryFailureFact::RequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
            conclusion: "FAILURE".to_owned(),
            run_id: None,
        }],
        "steward-policy",
        "worker-config",
    )
    .expect("request")
}

fn pull_without_app_identity(request: &RecoveryRequest) -> Value {
    serde_json::json!({
        "state": "OPEN",
        "isDraft": false,
        "baseRefName": "main",
        "headRefOid": request.head_sha,
        "mergeStateStatus": "CLEAN",
        "labels": [
            {"name": super::super::super::MANAGED_LABEL},
            {"name": super::super::super::NEEDS_AGENT_LABEL}
        ],
        "statusCheckRollup": [{
            "__typename": "CheckRun",
            "name": "macos",
            "status": "COMPLETED",
            "conclusion": "FAILURE",
            "completedAt": "2026-08-21T08:00:00Z"
        }]
    })
}

#[test]
fn draft_or_configured_opt_out_supersedes_live_recovery() {
    let request = RecoveryRequest::new_with_steward_policy(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        true,
        "custom:no-recovery",
        "failure-fingerprint",
        "required check failed",
        vec![RecoveryRequiredCheck {
            context: "macos".to_owned(),
            app_id: Some(42),
        }],
        vec![RecoveryFailureFact::RequiredCheck {
            context: "macos".to_owned(),
            app_id: Some(42),
            conclusion: "FAILURE".to_owned(),
            run_id: None,
        }],
        "steward-policy",
        "worker-config",
    )
    .expect("request with exact opt-out policy");
    let mut pull = pull_without_app_identity(&request);
    let mut missing_draft = pull.clone();
    missing_draft
        .as_object_mut()
        .expect("pull object")
        .remove("isDraft");
    assert!(
        classify_pull_response(&missing_draft, &request, &[]).is_err(),
        "omitted draft state must fail closed"
    );

    pull["isDraft"] = Value::Bool(true);
    assert!(matches!(
        classify_pull_response(&pull, &request, &[]).expect("draft is deterministic"),
        RequestDisposition::Superseded(reason) if reason.contains("draft")
    ));

    pull["isDraft"] = Value::Bool(false);
    pull["labels"]
        .as_array_mut()
        .expect("labels")
        .push(serde_json::json!({"name": "CUSTOM:NO-RECOVERY"}));
    assert!(matches!(
        classify_pull_response(&pull, &request, &[]).expect("opt-out is deterministic"),
        RequestDisposition::Superseded(reason) if reason.contains("opt-out")
    ));

    let mut unmanaged = pull_without_app_identity(&request);
    unmanaged["labels"]
        .as_array_mut()
        .expect("labels")
        .retain(|label| {
            label.get("name").and_then(Value::as_str) != Some(super::super::super::MANAGED_LABEL)
        });
    assert!(matches!(
        classify_pull_response(&unmanaged, &request, &[])
            .expect("valid management revocation is deterministic"),
        RequestDisposition::Superseded(reason) if reason.contains("provenance label")
    ));
}

#[test]
fn valid_status_provenance_revocation_supersedes_but_malformed_status_errors() {
    let request = app_bound_request();
    let status = |id: u64, context: &str, state: &str| {
        serde_json::json!({
            "id": id,
            "context": context,
            "state": state,
            "created_at": "2026-08-21T08:00:00Z"
        })
    };
    let current = vec![
        status(1, super::super::super::HANDOFF_CONTEXT, "success"),
        status(2, super::super::super::RECOVERY_CONTEXT, "failure"),
    ];
    assert!(matches!(
        classify_status_provenance(&current, &request).expect("current provenance"),
        RequestDisposition::Current
    ));

    let revoked = vec![
        status(3, super::super::super::HANDOFF_CONTEXT, "failure"),
        status(4, super::super::super::RECOVERY_CONTEXT, "failure"),
    ];
    assert!(matches!(
        classify_status_provenance(&revoked, &request).expect("valid revocation"),
        RequestDisposition::Superseded(reason) if reason.contains("no longer has successful")
    ));
    assert!(matches!(
        classify_status_provenance(&revoked[1..], &request).expect("missing handoff revocation"),
        RequestDisposition::Superseded(_)
    ));

    let malformed = vec![serde_json::json!({
        "context": super::super::super::HANDOFF_CONTEXT,
        "state": "failure",
        "created_at": "2026-08-21T08:00:00Z"
    })];
    assert!(
        classify_status_provenance(&malformed, &request).is_err(),
        "malformed provenance must still fail closed"
    );
}

#[cfg(unix)]
#[test]
fn app_bound_failure_uses_bounded_rest_identity_hydration() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let binary = temp.path().join("gh");
    fs::write(
        &binary,
        r#"#!/bin/sh
set -eu
case "$*" in
  *"check-runs?per_page=100&page=1"*)
    printf '%s' '{"check_runs":[{"name":"macos","app":{"id":42},"status":"completed","conclusion":"failure","completed_at":"2026-08-21T08:00:00Z"}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    )
    .expect("fake gh");
    let mut permissions = fs::metadata(&binary).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&binary, permissions).expect("permissions");
    let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(binary);
    let request = app_bound_request();

    let hydrated =
        fetch_bounded_check_runs(&actions, &request, Instant::now() + Duration::from_secs(30))
            .expect("bounded identity hydration");
    assert_eq!(hydrated.len(), 1);
    assert_eq!(hydrated[0].app_id, Some(42));
    assert!(matches!(
        classify_pull_response(&pull_without_app_identity(&request), &request, &hydrated)
            .expect("hydrated failure remains current"),
        RequestDisposition::Current
    ));
}

#[cfg(unix)]
#[test]
fn truncated_rollup_uses_complete_rest_checks_and_statuses_without_app_binding() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let binary = temp.path().join("gh");
    let request = unbound_request();
    let pull = serde_json::json!({
        "state": "OPEN",
        "isDraft": false,
        "baseRefName": "main",
        "headRefOid": request.head_sha,
        "mergeStateStatus": "CLEAN",
        "labels": [
            {"name": super::super::super::MANAGED_LABEL},
            {"name": super::super::super::NEEDS_AGENT_LABEL}
        ],
        "statusCheckRollup": (0..STATUS_PAGE_SIZE).map(|index| {
            serde_json::json!({
                "__typename": "CheckRun",
                "name": format!("partial-{index}"),
                "status": "COMPLETED",
                "conclusion": "SUCCESS"
            })
        }).collect::<Vec<_>>()
    });
    let script = format!(
        r#"#!/bin/sh
set -eu
case "$*" in
  *"pr view"*)
    printf '%s' '{}' ;;
  *"check-runs?per_page=100&page=1"*)
    printf '%s' '{{"check_runs":[{{"name":"macos","app":{{"id":42}},"status":"completed","conclusion":"failure","completed_at":"2026-08-21T08:00:00Z"}}]}}' ;;
  *"statuses?per_page=100&page=1"*)
    printf '%s' '[{{"id":1,"context":"{}","state":"success","created_at":"2026-08-21T08:00:00Z"}},{{"id":2,"context":"{}","state":"failure","created_at":"2026-08-21T08:00:01Z"}}]' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
        serde_json::to_string(&pull).expect("pull JSON"),
        super::super::super::HANDOFF_CONTEXT,
        super::super::super::RECOVERY_CONTEXT,
    );
    fs::write(&binary, script).expect("fake gh");
    let mut permissions = fs::metadata(&binary).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&binary, permissions).expect("permissions");
    let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(binary);

    assert!(matches!(
        inspect_request(&actions, &request, Instant::now() + Duration::from_secs(30))
            .expect("complete bounded hydration"),
        RequestDisposition::Current
    ));
}

#[cfg(unix)]
#[test]
fn github_transport_rejects_stdout_and_stderr_past_explicit_byte_limits() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let binary = temp.path().join("gh");
    fs::write(
        &binary,
        r#"#!/bin/sh
set -eu
case "${1:-}" in
  stdout) printf '%065d' 0 ;;
  stderr) printf '%065d' 0 >&2; exit 2 ;;
  *) exit 3 ;;
esac
"#,
    )
    .expect("fake gh");
    let mut permissions = fs::metadata(&binary).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&binary, permissions).expect("permissions");
    let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(binary);

    for stream in ["stdout", "stderr"] {
        let error = actions
            .run_gh_with_timeout_bounded(&[stream.to_owned()], Duration::from_secs(30), 64, 64)
            .expect_err("oversized gh output must fail closed");
        assert!(
            error
                .to_string()
                .contains("exceeded bounded output capture"),
            "unexpected {stream} overflow error: {error}"
        );
    }
}

#[cfg(unix)]
#[test]
fn github_transport_does_not_wait_on_an_escaped_descendants_stdio() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let temp = tempfile::tempdir().expect("tempdir");
    let binary = temp.path().join("gh");
    let pid_path = temp.path().join("gh.pid");
    fs::write(
        &binary,
        r#"#!/bin/sh
set -eu
python3 -c 'import os,sys,time; os.setsid(); f=open(sys.argv[1],"w"); f.write(str(os.getpid())); f.flush(); os.fsync(f.fileno()); time.sleep(30)' "$1" &
while [ ! -s "$1" ]; do sleep 0.01; done
printf ready
"#,
    )
    .expect("fake gh");
    let mut permissions = fs::metadata(&binary).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&binary, permissions).expect("permissions");
    let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(binary);

    // The direct gh process exits only after its child has escaped the
    // supervised process group while retaining stdout/stderr. Pipe readers
    // would remain blocked until that child exits; regular-file capture is
    // immediately readable and owns no detached helper threads.
    let result = actions.run_gh_with_timeout_bounded(
        &[pid_path.to_string_lossy().into_owned()],
        Duration::from_secs(10),
        64,
        64,
    );
    if let Ok(pid) = fs::read_to_string(&pid_path) {
        let _ = Command::new("kill")
            .args(["-KILL", "--", pid.trim()])
            .status();
    }

    assert_eq!(
        result.expect("capture must not wait for inherited file handles"),
        "ready"
    );
}

#[test]
fn newly_failed_app_bound_required_check_supersedes_same_head_request() {
    let request = RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
        "failure-fingerprint",
        "required check failed",
        vec![
            RecoveryRequiredCheck {
                context: "macos".to_owned(),
                app_id: Some(42),
            },
            RecoveryRequiredCheck {
                context: "signing".to_owned(),
                app_id: Some(99),
            },
        ],
        vec![RecoveryFailureFact::RequiredCheck {
            context: "macos".to_owned(),
            app_id: Some(42),
            conclusion: "FAILURE".to_owned(),
            run_id: None,
        }],
        "steward-policy",
        "worker-config",
    )
    .expect("request");
    let pull = serde_json::json!({
        "state": "OPEN",
        "isDraft": false,
        "baseRefName": "main",
        "headRefOid": request.head_sha,
        "mergeStateStatus": "CLEAN",
        "labels": [
            {"name": super::super::super::MANAGED_LABEL},
            {"name": super::super::super::NEEDS_AGENT_LABEL}
        ],
        "statusCheckRollup": [
            {
                "__typename": "CheckRun",
                "name": "macos",
                "status": "COMPLETED",
                "conclusion": "FAILURE",
                "completedAt": "2026-08-21T08:00:00Z"
            },
            {
                "__typename": "CheckRun",
                "name": "signing",
                "status": "COMPLETED",
                "conclusion": "FAILURE",
                "completedAt": "2026-08-21T08:00:00Z"
            }
        ]
    });
    let hydrated = [
        serde_json::json!({
            "name": "macos",
            "app": {"id": 42},
            "status": "completed",
            "conclusion": "failure",
            "completed_at": "2026-08-21T08:00:00Z"
        }),
        serde_json::json!({
            "name": "signing",
            "app": {"id": 99},
            "status": "completed",
            "conclusion": "failure",
            "completed_at": "2026-08-21T08:00:00Z"
        }),
    ]
    .iter()
    .map(|value| {
        super::super::super::observation::parse_rest_check(value).expect("REST check fixture")
    })
    .collect::<Vec<_>>();

    assert!(matches!(
        classify_pull_response(&pull, &request, &hydrated).expect("typed freshness result"),
        RequestDisposition::Superseded(_)
    ));
}

#[test]
fn full_check_run_window_fails_closed() {
    let page = (0..CHECK_RUN_PAGE_SIZE)
        .map(|index| {
            serde_json::json!({
                "name": format!("check-{index}"),
                "app": {"id": 42},
                "status": "completed",
                "conclusion": "success"
            })
        })
        .collect::<Vec<_>>();
    let mut checks = Vec::new();
    for page_number in 1..MAX_CHECK_RUN_PAGES {
        assert!(!append_check_run_page(&mut checks, &page, page_number).expect("bounded page"));
    }
    let error = append_check_run_page(&mut checks, &page, MAX_CHECK_RUN_PAGES)
        .expect_err("full bounded window must fail closed");
    assert!(
        error
            .message()
            .contains("bounded 400-entry identity window")
    );
}

#[test]
fn full_commit_status_window_fails_closed() {
    let page = (0..STATUS_PAGE_SIZE)
        .map(|index| serde_json::json!({"context": format!("status-{index}")}))
        .collect::<Vec<_>>();
    let mut statuses = Vec::new();
    for page_number in 1..MAX_STATUS_PAGES {
        assert!(
            !append_complete_status_page(&mut statuses, &page, page_number).expect("bounded page")
        );
    }
    let error = append_complete_status_page(&mut statuses, &page, MAX_STATUS_PAGES)
        .expect_err("full bounded window must fail closed");
    assert!(
        error
            .message()
            .contains("bounded 400-entry identity window")
    );
}
