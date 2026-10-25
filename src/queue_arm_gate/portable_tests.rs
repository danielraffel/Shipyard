//! Cases that need neither a `gh` script nor Python.

use std::ffi::OsString;

use super::is_landing_request;

const HEAD: &str = "bca87fe519d60da1c42f476fb8e839b1bc247b67";
const ENQUEUE: &str = "mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head}){mergeQueueEntry{position}}}";

#[test]
fn only_requests_that_can_land_are_gated() {
    let argv = |words: &[&str]| {
        words
            .iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>()
    };
    for landing in [
        argv(&["api", "graphql", "-f", &format!("query={ENQUEUE}")]),
        argv(&[
            "api",
            "graphql",
            "-f",
            "query=mutation($id:ID!){enablePullRequestAutoMerge(input:{pullRequestId:$id}){pullRequest{id}}}",
        ]),
        argv(&["pr", "merge", "7", "--match-head-commit", HEAD, "--merge"]),
        argv(&["api", "-X", "PUT", "repos/o/r/pulls/7/merge", "-f", "sha=x"]),
    ] {
        assert!(is_landing_request(&landing), "{landing:?}");
    }
    for read in [
        argv(&["api", "repos/o/r/pulls/7"]),
        argv(&["api", "-X", "GET", "repos/o/r/pulls/7/merge"]),
        argv(&["pr", "view", "7"]),
        argv(&["pr", "merge", "7", "--disable-auto"]),
        argv(&[
            "api",
            "graphql",
            "-f",
            "query=mutation($id:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$id}){pullRequest{id}}}",
        ]),
        argv(&["api", "graphql", "-f", "query=query{viewer{login}}"]),
    ] {
        assert!(!is_landing_request(&read), "{read:?}");
    }
}

#[test]
fn queue_authority_variables_are_the_markers_nonces_and_overrides() {
    for name in [
        "SHIPYARD_INTERNAL_QUEUE_MUTATION",
        "SHIPYARD_QUEUE_ARM_REVIEW_ONLY",
        "SHIPYARD_QUEUE_MUTATION_NONCE",
        "SHIPYARD_QUEUE_MUTATION_NONCE_DIR",
        "GHAPP_ALLOW_UNREVIEWED_ARM",
        "GHAPP_ALLOW_QUEUE_REARM",
        "GHAPP_ALLOW_QUEUE_REMOVAL",
    ] {
        assert!(super::is_queue_authority_variable(name), "{name}");
    }
    for name in [
        "PATH",
        "GH_TOKEN",
        "SHIPYARD_QUEUE_ARMS_LOG",
        "GHAPP_REAL_GH",
    ] {
        assert!(!super::is_queue_authority_variable(name), "{name}");
    }
}

/// A child Shipyard builds carries no queue authority it was handed.
#[test]
fn stripping_removes_every_queue_authority_variable() {
    let mut command = std::process::Command::new("child");
    command
        .env("GHAPP_ALLOW_UNREVIEWED_ARM", "1")
        .env("GHAPP_ALLOW_SOMETHING_NEW", "1")
        .env("SHIPYARD_INTERNAL_QUEUE_MUTATION", "1")
        .env("GH_TOKEN", "kept");
    super::strip_queue_authority(&mut command);
    let envs = command
        .get_envs()
        .map(|(key, value)| (key.to_string_lossy().into_owned(), value.is_some()))
        .collect::<std::collections::BTreeMap<_, _>>();
    for name in [
        "GHAPP_ALLOW_UNREVIEWED_ARM",
        "GHAPP_ALLOW_SOMETHING_NEW",
        "SHIPYARD_INTERNAL_QUEUE_MUTATION",
        "SHIPYARD_QUEUE_ARM_REVIEW_ONLY",
        "SHIPYARD_QUEUE_MUTATION_NONCE",
        "SHIPYARD_QUEUE_MUTATION_NONCE_DIR",
    ] {
        assert_eq!(envs.get(name), Some(&false), "{name} removed");
    }
    assert_eq!(envs.get("GH_TOKEN"), Some(&true));
}

/// A Shipyard process started with queue authority re-executes itself
/// without it, so nothing it spawns (runners, validation commands, hooks)
/// inherits it; one started without any carries on.
#[test]
fn an_inherited_marker_relaunches_without_it() {
    let clean = ["PATH", "HOME", "GH_TOKEN"].map(OsString::from);
    assert!(super::relaunch_command(clean.into_iter()).is_none());
    let inherited = [
        "PATH",
        "SHIPYARD_INTERNAL_QUEUE_MUTATION",
        "SHIPYARD_QUEUE_ARM_REVIEW_ONLY",
        "GHAPP_ALLOW_QUEUE_REARM",
    ]
    .map(OsString::from);
    let command = super::relaunch_command(inherited.into_iter()).expect("relaunch");
    let removed = command
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        removed,
        [
            "GHAPP_ALLOW_QUEUE_REARM",
            "SHIPYARD_INTERNAL_QUEUE_MUTATION",
            "SHIPYARD_QUEUE_ARM_REVIEW_ONLY"
        ]
    );
    assert!(!command.get_envs().any(|(key, _)| key == "PATH"));
}

#[test]
fn nonce_records_are_named_as_the_guards_name_them() {
    // sha256("abc:internal-arm"), as `nonce_record_name` in the removal guard.
    assert_eq!(
        super::nonce_record_name("abc", "internal-arm"),
        "ab52110613b2955772a8e3c60bd5b65b0d826b70e2a06894b44d055a7a871e2d"
    );
}

/// The review-only guard run never carries the landing's own authority: its
/// marker, its nonce and any override are dropped, and it gets the
/// review-only switch with a nonce of its own.
#[test]
fn the_review_only_run_drops_the_landings_authority() {
    let temp = tempfile::tempdir().expect("temp");
    let ledger = temp.path().join(super::ARMS_LEDGER);
    let mut gh = std::process::Command::new("gh");
    super::mark_internal_queue_mutation(&mut gh).expect("marked");
    gh.env("GHAPP_ALLOW_UNREVIEWED_ARM", "1")
        .env("GH_TOKEN", "token");
    let landing_nonce = env_of(&gh, super::NONCE_ENV).expect("landing nonce");
    let mut guard = std::process::Command::new("python3");
    super::review_only_environment(&gh, &mut guard, &ledger).expect("environment");
    assert_eq!(env_of(&guard, "SHIPYARD_INTERNAL_QUEUE_MUTATION"), None);
    assert_eq!(env_of(&guard, "GHAPP_ALLOW_UNREVIEWED_ARM"), None);
    assert_eq!(env_of(&guard, "GH_TOKEN").as_deref(), Some("token"));
    assert_eq!(env_of(&guard, super::REVIEW_ONLY_ENV).as_deref(), Some("1"));
    let nonce = env_of(&guard, super::NONCE_ENV).expect("its own nonce");
    assert_ne!(nonce, landing_nonce);
    let directory = std::path::PathBuf::from(env_of(&guard, super::NONCE_DIR_ENV).expect("dir"));
    assert!(
        directory
            .join(super::nonce_record_name(&nonce, super::NONCE_REVIEW_ONLY))
            .is_file()
    );
}

/// The value `command` sets for `key`; `None` when unset or removed.
fn env_of(command: &std::process::Command, key: &str) -> Option<String> {
    command
        .get_envs()
        .find(|(name, _)| *name == key)
        .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
}
