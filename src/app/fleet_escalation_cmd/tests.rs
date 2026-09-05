//! Tests for the escalation I/O layer.
//!
//! These assert on the **exact `gh` calls made and not made**, which is the
//! house pattern for proving a mutation boundary: a fake `gh` records its argv,
//! and the test checks both that the intended endpoint was hit and that the
//! unintended ones were not. For a module whose whole job is to write to a
//! shared repository, "did not call" is the more important half.

use super::*;
use crate::fleet_escalation::EscalationAction;

#[cfg(unix)]
fn fake_gh(temp: &tempfile::TempDir, body: &str) -> GitHubActions {
    use std::os::unix::fs::PermissionsExt;
    let path = temp.path().join("gh");
    // Write to a staging name and rename into place. Writing the script and
    // exec'ing it directly races: with tests running in parallel, another
    // thread can fork while this one still holds the file open for writing,
    // the child inherits that descriptor, and the exec fails ETXTBSY ("Text
    // file busy"). Rename is atomic and publishes an inode nobody holds a
    // write handle to, so the exec cannot observe the half-written state.
    // Seen only on Linux under llvm-cov, where instrumentation widens the
    // window enough to hit it.
    let staging = temp.path().join("gh.staging");
    std::fs::write(&staging, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write fake gh");
    let mut permissions = std::fs::metadata(&staging)
        .expect("fake gh metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&staging, permissions).expect("chmod fake gh");
    std::fs::rename(&staging, &path).expect("publish fake gh");
    // An empty config keeps the fake off the ambient auth path, which
    // otherwise demands a real GitHub remote on the temp dir.
    let config = crate::config::LoadedConfig {
        data: toml::Table::new(),
        global_dir: temp.path().join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: crate::config::LocalOverlaySource::None,
    };
    GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(path)
}

/// A fake `gh` that logs every argv line to `calls` and prints `payload`.
#[cfg(unix)]
fn recording_gh(temp: &tempfile::TempDir, payload: &str) -> (GitHubActions, std::path::PathBuf) {
    let calls = temp.path().join("calls");
    let actions = fake_gh(
        temp,
        &format!(
            "printf '%s\\n' \"$*\" >> '{calls}'\nprintf '%s' '{payload}'",
            calls = calls.display()
        ),
    );
    (actions, calls)
}

#[cfg(unix)]
fn calls_of(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

// Only the `#[cfg(unix)]` tests build an action, because the fake-`gh` harness
// needs a `#!/bin/sh` shim. Without the same gate this is dead code on Windows,
// where `-D warnings` turns that into a build failure.
#[cfg(unix)]
fn open_action() -> EscalationAction {
    EscalationAction::Open {
        key: "host=macpro lane=linux".to_owned(),
        title: "Fleet: linux on macpro is unserved".to_owned(),
        body: format!("{}\n\nbody text", subject_marker("host=macpro lane=linux")),
    }
}

// ---------------------------------------------------------------------------
// Dry-run is the default, and it must touch nothing
// ---------------------------------------------------------------------------

/// The planted control for the most dangerous property of this module: with
/// `apply` false it must make **no mutating call at all**. A dry run that
/// quietly writes is worse than no dry run, because it is trusted.
#[cfg(unix)]
#[test]
fn negative_control_a_dry_run_makes_no_mutating_call() {
    let temp = tempfile::tempdir().expect("temp");
    let (actions, calls) = recording_gh(&temp, "{}");

    for action in [
        open_action(),
        EscalationAction::Update {
            number: 7,
            body: "b".to_owned(),
        },
        EscalationAction::Close {
            number: 7,
            comment: "c".to_owned(),
        },
    ] {
        let record = apply_escalation(&actions, "o/r", &action, false).expect("dry run");
        assert!(!record.applied, "{record:?}");
    }

    let logged = calls_of(&calls);
    assert!(
        logged.is_empty(),
        "a dry run must not invoke gh at all, got: {logged}"
    );
}

/// Pairing control: with `apply` true the same open DOES call the API.
/// Without this, "makes no call" would also pass for a module that never works.
#[cfg(unix)]
#[test]
fn control_applying_an_open_posts_to_the_issues_endpoint() {
    let temp = tempfile::tempdir().expect("temp");
    let (actions, calls) = recording_gh(&temp, "{\"number\":123}");

    let record = apply_escalation(&actions, "o/r", &open_action(), true).expect("open");

    assert!(record.applied);
    assert_eq!(record.kind, "open");
    assert!(record.detail.contains("123"), "{}", record.detail);

    let logged = calls_of(&calls);
    assert!(logged.contains("--method POST"), "{logged}");
    assert!(logged.contains("repos/o/r/issues"), "{logged}");
    assert!(
        !logged.contains("state=closed"),
        "opening must not close anything: {logged}"
    );
}

// ---------------------------------------------------------------------------
// Each action hits its own endpoint, and only its own
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn an_update_patches_the_issue_and_does_not_open_a_new_one() {
    let temp = tempfile::tempdir().expect("temp");
    let (actions, calls) = recording_gh(&temp, "{}");

    apply_escalation(
        &actions,
        "o/r",
        &EscalationAction::Update {
            number: 42,
            body: "fresh".to_owned(),
        },
        true,
    )
    .expect("update");

    let logged = calls_of(&calls);
    assert!(logged.contains("--method PATCH"), "{logged}");
    assert!(logged.contains("repos/o/r/issues/42"), "{logged}");
    assert!(
        !logged.contains("--method POST"),
        "an edit must never create: {logged}"
    );
}

/// The comment must be posted before the state change. If the close fails, the
/// reader is left with an open issue that explains the recovery; reversed, a
/// failure would leave a silently closed issue and no explanation.
#[cfg(unix)]
#[test]
fn a_close_comments_before_it_closes() {
    let temp = tempfile::tempdir().expect("temp");
    let (actions, calls) = recording_gh(&temp, "{}");

    apply_escalation(
        &actions,
        "o/r",
        &EscalationAction::Close {
            number: 9,
            comment: "recovered".to_owned(),
        },
        true,
    )
    .expect("close");

    let logged = calls_of(&calls);
    let comment_at = logged.find("issues/9/comments").expect("comment call");
    let close_at = logged.find("state=closed").expect("close call");
    assert!(
        comment_at < close_at,
        "the comment must precede the close: {logged}"
    );
}

#[cfg(unix)]
#[test]
fn nothing_touches_the_api_even_when_applying() {
    let temp = tempfile::tempdir().expect("temp");
    let (actions, calls) = recording_gh(&temp, "{}");

    let record = apply_escalation(
        &actions,
        "o/r",
        &EscalationAction::Nothing {
            reason: "under threshold".to_owned(),
        },
        true,
    )
    .expect("nothing");

    assert!(!record.applied);
    assert!(calls_of(&calls).is_empty());
}

// ---------------------------------------------------------------------------
// Reading the open issues back
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn tracking_issues_are_matched_by_marker_not_title() {
    let temp = tempfile::tempdir().expect("temp");
    let marked = format!(
        "{}\\n\\nstuff",
        subject_marker("host=m5 lane=release").replace('"', "")
    );
    let payload = format!(
        "{{\"number\":5,\"title\":\"a human renamed this\",\"body\":\"{marked}\"}}\n\
         {{\"number\":6,\"title\":\"Fleet: looks like ours\",\"body\":\"no marker here\"}}"
    );
    let actions = fake_gh(&temp, &format!("printf '%s' '{payload}'"));

    let issues = fetch_tracking_issues(&actions, "o/r").expect("issues");

    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].number, 5);
    assert_eq!(issues[0].key, "host=m5 lane=release");
}

/// Planted control: an issue we did not write must never be adopted, however
/// much its title looks like ours. Editing somebody else's report is worse than
/// opening a duplicate.
#[cfg(unix)]
#[test]
fn negative_control_an_unmarked_issue_is_never_adopted() {
    let temp = tempfile::tempdir().expect("temp");
    let payload = "{\"number\":6,\"title\":\"Fleet: linux on macpro is unserved\",\"body\":\"filed by a human\"}";
    let actions = fake_gh(&temp, &format!("printf '%s' '{payload}'"));

    assert!(
        fetch_tracking_issues(&actions, "o/r")
            .expect("issues")
            .is_empty()
    );
}

/// The issues endpoint returns pull requests. Closing one as "recovered" would
/// be a memorable way to lose somebody's work.
#[cfg(unix)]
#[test]
fn pull_requests_are_excluded_even_when_marked() {
    let temp = tempfile::tempdir().expect("temp");
    let marked = subject_marker("host=m5 lane=release");
    let payload =
        format!("{{\"number\":7,\"body\":\"{marked}\",\"pull_request\":{{\"url\":\"x\"}}}}");
    let actions = fake_gh(&temp, &format!("printf '%s' '{payload}'"));

    assert!(
        fetch_tracking_issues(&actions, "o/r")
            .expect("issues")
            .is_empty()
    );
}

/// An unreadable list must be an error, not an empty vector. Reading "nothing
/// is open" from a failed call is how a duplicate gets opened next to the issue
/// that already exists.
#[cfg(unix)]
#[test]
fn negative_control_an_unreadable_list_errors_rather_than_reading_empty() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, "echo 'boom' >&2; exit 1");

    assert!(fetch_tracking_issues(&actions, "o/r").is_err());
}

#[test]
fn a_marker_round_trips() {
    let key = "host=macpro lane=PULP_AUTO_LINUX_RUNS_ON_JSON";
    let body = format!("{}\n\nrest", marker_for(key));
    assert_eq!(parse_marker(&body).as_deref(), Some(key));
    assert_eq!(parse_marker("no marker"), None);
    assert_eq!(parse_marker("<!-- shipyard-fleet-subject:  -->"), None);
}

// ---------------------------------------------------------------------------
// Batch behaviour
// ---------------------------------------------------------------------------

/// A failed mutation invalidates the snapshot the whole batch was decided
/// against, so the batch stops rather than pressing on and risking a duplicate.
#[cfg(unix)]
#[test]
fn a_batch_stops_at_the_first_failure() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            "printf '%s\\n' \"$*\" >> '{calls}'\n\
             case \"$*\" in\n\
               *'issues/42'*) echo 'nope' >&2; exit 1 ;;\n\
               *) printf '%s' '{{\"number\":1}}' ;;\n\
             esac",
            calls = calls.display()
        ),
    );

    let decisions = vec![
        open_action(),
        EscalationAction::Update {
            number: 42,
            body: "b".to_owned(),
        },
        EscalationAction::Update {
            number: 99,
            body: "b".to_owned(),
        },
    ];
    let (applied, error) = apply_all(&actions, "o/r", &decisions, true);

    assert_eq!(applied.len(), 1, "{applied:?}");
    assert!(error.is_some());
    let logged = calls_of(&calls);
    assert!(
        !logged.contains("issues/99"),
        "the batch must not continue past a failure: {logged}"
    );
}

#[cfg(unix)]
#[test]
fn a_clean_batch_reports_every_action() {
    let temp = tempfile::tempdir().expect("temp");
    let (actions, _calls) = recording_gh(&temp, "{\"number\":1}");
    let decisions = vec![
        open_action(),
        EscalationAction::Nothing {
            reason: "quiet".to_owned(),
        },
    ];
    let (applied, error) = apply_all(&actions, "o/r", &decisions, false);
    assert!(error.is_none());
    assert_eq!(applied.len(), 2);
    assert!(applied.iter().all(|record| !record.applied));
}
