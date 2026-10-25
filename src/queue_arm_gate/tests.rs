//! The gate runs the bundled queue-arm guard for real against a fake `gh`.
//!
//! Each case drives the steward's exact enqueue request through
//! [`crate::cloud::GitHubActions::run_gh_internal_queue_mutation`], the
//! transport the steward, the carrier and base-health jumps all use, and
//! asserts on what reached the fake `gh`: the mutation itself only when the
//! guard allowed it.

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::tests_support::with_real_guard;
use super::{is_landing_request, is_refusal};
use crate::cloud::GitHubActions;

const REPO: &str = "Generous-Corp/pulp";
const HEAD: &str = "bca87fe519d60da1c42f476fb8e839b1bc247b67";
const OTHER_HEAD: &str = "0d7179d1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const COMMENT_ID: u64 = 6_390_564_321;
const ENQUEUE: &str = "mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head}){mergeQueueEntry{position}}}";

struct Fixture {
    temp: tempfile::TempDir,
    actions: GitHubActions,
}

impl Fixture {
    /// A fake `gh` answering the guard's reads for PR 4242 with `comments`,
    /// and the enqueue itself, logging every call.
    fn new(comments: &Value, base_config: &str) -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let answers = json!([
            ["enqueuePullRequest", {"json": {"data": {"enqueuePullRequest": {"mergeQueueEntry": {"position": 1}}}}}],
            ["enablePullRequestAutoMerge", {"json": {"data": {"enablePullRequestAutoMerge": {"pullRequest": {"number": 4242}}}}}],
            ["contents/.shipyard/config.toml", {"text": base_config}],
            ["/reviews", {"pages": [[]]}],
            ["/comments", {"pages": [comments]}],
            ["node(id:", {"json": {"data": {"node": {
                "number": 4242, "state": "OPEN", "headRefOid": HEAD, "baseRefName": "main",
                "isInMergeQueue": false, "mergeQueueEntry": null, "autoMergeRequest": null,
                "repository": {"nameWithOwner": REPO},
                "timelineItems": {"pageInfo": {"hasPreviousPage": false}, "nodes": []}}}}}],
            ["/pulls/4242", {"json": {"number": 4242, "base": {"ref": "main"},
                "head": {"sha": HEAD}, "user": {"login": "shipyard-local[bot]", "type": "Bot"}}}],
        ]);
        let table = temp.path().join("answers.json");
        fs::write(&table, answers.to_string()).expect("answers");
        let calls = temp.path().join("calls");
        let gh = temp.path().join("gh");
        crate::test_support::write_executable_script(
            &gh,
            &format!(
                r"#!/usr/bin/env python3
import json, sys
argv = ' '.join(sys.argv[1:])
open({calls:?}, 'a').write(argv + '\n')
for needle, answer in json.load(open({table:?})):
    if needle not in argv:
        continue
    if 'pages' in answer:
        sys.stdout.write(json.dumps(answer['pages'] if '--slurp' in sys.argv else answer['pages'][0]))
    elif 'text' in answer:
        sys.stdout.write(answer['text'])
    else:
        sys.stdout.write(json.dumps(answer['json']))
    sys.exit(0)
sys.stderr.write('unexpected gh call: ' + argv + '\n')
sys.exit(3)
",
                calls = calls.display().to_string(),
                table = table.display().to_string(),
            ),
        );
        fs::create_dir_all(temp.path().join("guards")).expect("guards dir");
        let actions = GitHubActions::hermetic_for_tests(temp.path(), gh);
        Self { temp, actions }
    }

    fn guards(&self) -> PathBuf {
        self.temp.path().join("guards")
    }

    fn ledger_path(&self) -> PathBuf {
        self.temp.path().join("state").join(super::ARMS_LEDGER)
    }

    /// The steward's enqueue, through the steward's transport, guard live.
    fn enqueue(&self, head: &str) -> Result<String, String> {
        with_real_guard(&self.ledger_path(), &self.guards(), || {
            self.actions
                .run_gh_internal_queue_mutation(&[
                    "api".to_owned(),
                    "graphql".to_owned(),
                    "-f".to_owned(),
                    format!("query={ENQUEUE}"),
                    "-F".to_owned(),
                    "id=PR_kwDOpulp4242".to_owned(),
                    "-F".to_owned(),
                    format!("head={head}"),
                ])
                .map_err(|error| error.to_string())
        })
    }

    /// An operator's `--arm` (native auto-merge), through the transport
    /// `ship` uses for it, guard live.
    fn arm(&self, reason: Option<&str>) -> Result<String, String> {
        let requested = super::OperatorOverride {
            operator: "operator@host".to_owned(),
            reason: reason.map(str::to_owned),
        };
        with_real_guard(&self.ledger_path(), &self.guards(), || {
            self.actions
                .run_gh_with_operator_override(
                    &crate::auto_arm::arm_mutation_args_at_head("PR_kwDOpulp4242", HEAD),
                    &requested,
                )
                .map_err(|error| error.to_string())
        })
    }

    fn armed(&self) -> bool {
        self.calls()
            .lines()
            .any(|line| line.contains("enablePullRequestAutoMerge"))
    }

    fn overrides(&self) -> Vec<Value> {
        fs::read_to_string(self.guards().join("overrides.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("override line"))
            .collect()
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.temp.path().join("calls")).unwrap_or_default()
    }

    fn mutated(&self) -> bool {
        self.calls()
            .lines()
            .any(|line| line.contains("enqueuePullRequest"))
    }

    fn ledger(&self) -> Vec<Value> {
        fs::read_to_string(self.ledger_path())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("ledger line"))
            .collect()
    }
}

fn verdict(body: &str) -> Value {
    json!([{
        "id": COMMENT_ID,
        "user": {"login": "shipyard-local[bot]", "type": "Bot"},
        "author_association": "CONTRIBUTOR",
        "created_at": "2026-10-24T22:53:36Z",
        "body": body,
    }])
}

fn approve(head: &str, extra: &str) -> Value {
    verdict(&format!(
        "Reviewed.\n{extra}<!-- session: Daniels-Mac-Studio-m3/08fb81d8 -->\nverdict: APPROVE head:{head}"
    ))
}

const SIGNED_BASE: &str = "[auto_merge]\nreviewer_signers = [\"pulp-review\", \"infra-review2\"]\n";

fn assert_refused(fixture: &Fixture, result: &Result<String, String>, expected: &str) {
    let error = result.as_ref().expect_err("the guard refuses");
    assert!(is_refusal(error), "{error}");
    assert!(error.contains(expected), "{error}");
    assert!(
        !fixture.mutated(),
        "the enqueue reached GitHub: {}",
        fixture.calls()
    );
    let ledger = fixture.ledger();
    let entry = ledger
        .last()
        .expect("a ledger line for the refused attempt");
    assert_eq!(entry["verdict"], "refused", "{entry}");
    assert_eq!(entry["repo"], REPO, "{entry}");
    assert_eq!(entry["pr"], 4242, "{entry}");
    assert_eq!(entry["head"], HEAD, "{entry}");
    assert!(
        entry["caller"]
            .as_str()
            .is_some_and(|caller| caller.starts_with("shipyard")),
        "{entry}"
    );
}

#[cfg(unix)]
#[test]
fn steward_enqueue_of_an_unverdicted_head_is_refused() {
    let fixture = Fixture::new(&json!([]), SIGNED_BASE);
    let result = fixture.enqueue(HEAD);
    assert_refused(
        &fixture,
        &result,
        "no reviewer verdict names head bca87fe519d6",
    );
}

#[cfg(unix)]
#[test]
fn steward_enqueue_with_a_verdict_naming_another_head_is_refused() {
    let fixture = Fixture::new(&approve(OTHER_HEAD, "reviewer: pulp-review\n"), SIGNED_BASE);
    let result = fixture.enqueue(HEAD);
    assert_refused(
        &fixture,
        &result,
        "no reviewer verdict names head bca87fe519d6",
    );
}

#[cfg(unix)]
#[test]
fn steward_enqueue_bound_to_another_head_is_refused() {
    // The verdict names the live head, but the mutation would land another.
    let fixture = Fixture::new(&approve(HEAD, "reviewer: pulp-review\n"), SIGNED_BASE);
    let result = fixture.enqueue(OTHER_HEAD);
    let error = result.expect_err("refused");
    assert!(is_refusal(&error), "{error}");
    assert!(!fixture.mutated(), "{}", fixture.calls());
}

#[cfg(unix)]
#[test]
fn steward_enqueue_with_a_signer_not_on_the_list_is_refused() {
    let fixture = Fixture::new(&approve(HEAD, "reviewer: impostor\n"), SIGNED_BASE);
    let result = fixture.enqueue(HEAD);
    assert_refused(
        &fixture,
        &result,
        "its signer impostor is not one of pulp-review, infra-review2",
    );
}

#[cfg(unix)]
#[test]
fn steward_enqueue_with_an_unattributed_session_stamp_is_refused() {
    let fixture = Fixture::new(
        &verdict(&format!(
            "reviewer: pulp-review\n<!-- session: Daniels-M5-Studio/unattributed -->\nverdict: APPROVE head:{HEAD}"
        )),
        SIGNED_BASE,
    );
    let result = fixture.enqueue(HEAD);
    assert_refused(&fixture, &result, "its session stamp is unattributed");
}

#[cfg(unix)]
#[test]
fn steward_enqueue_with_a_session_line_off_the_hosts_allowlist_is_refused() {
    let fixture = Fixture::new(
        &approve(HEAD, "reviewer: pulp-review\nsession: someone (host m9)\n"),
        SIGNED_BASE,
    );
    fs::write(
        fixture.guards().join("reviewer-signers.json"),
        json!({"default": ["pulp-review"], "sessions": ["pulp-review (host m3)"]}).to_string(),
    )
    .expect("signers");
    let result = fixture.enqueue(HEAD);
    assert_refused(&fixture, &result, "is not on this host's allowlist");
}

#[cfg(unix)]
#[test]
fn steward_enqueue_of_a_signed_exact_head_approval_is_allowed_and_recorded() {
    let fixture = Fixture::new(&approve(HEAD, "reviewer: pulp-review\n"), SIGNED_BASE);
    let result = fixture.enqueue(HEAD);
    let raw = result.expect("allowed");
    assert!(raw.contains("mergeQueueEntry"), "{raw}");
    assert!(fixture.mutated(), "{}", fixture.calls());
    let ledger = fixture.ledger();
    assert_eq!(ledger.len(), 1, "{ledger:?}");
    let entry = &ledger[0];
    assert_eq!(entry["verdict"], "allowed", "{entry}");
    assert_eq!(entry["verb"], "arm", "{entry}");
    assert_eq!(entry["head"], HEAD, "{entry}");
    assert_eq!(entry["pr"], 4242, "{entry}");
    assert_eq!(entry["verdict_comment_id"], COMMENT_ID, "{entry}");
    assert_eq!(entry["signer"], "pulp-review", "{entry}");
    assert!(entry["caller"].is_string(), "{entry}");
}

#[cfg(unix)]
#[test]
fn every_attempt_appends_one_ledger_line() {
    let fixture = Fixture::new(&approve(HEAD, "reviewer: pulp-review\n"), SIGNED_BASE);
    assert!(fixture.enqueue(OTHER_HEAD).is_err());
    assert!(fixture.enqueue(HEAD).is_ok());
    let verdicts = fixture
        .ledger()
        .iter()
        .map(|entry| entry["verdict"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(verdicts, ["refused", "allowed"]);
}

#[cfg(unix)]
#[test]
fn a_guard_that_cannot_run_refuses_and_is_recorded() {
    let temp = tempfile::tempdir().expect("temp");
    let ledger = temp.path().join(super::ARMS_LEDGER);
    let mut gh = std::process::Command::new(temp.path().join("gh"));
    // python3 is looked up on PATH; a working directory that does not exist
    // makes the spawn itself fail.
    gh.current_dir(temp.path().join("missing"));
    let args = [
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={ENQUEUE}"),
    ];
    let error = with_real_guard(&ledger, temp.path(), || {
        super::check(&gh, &args, "shipyard test", None)
    })
    .expect_err("refused");
    assert!(is_refusal(&error), "{error}");
    let line = fs::read_to_string(&ledger).expect("ledger");
    let entry: Value = serde_json::from_str(line.trim()).expect("json");
    assert_eq!(entry["verdict"], "refused", "{entry}");
    assert_eq!(entry["caller"], "shipyard test", "{entry}");
}

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

/// The steward's transport, not just the gate in isolation: without the
/// real guard, the same unverdicted enqueue reaches GitHub. This is the
/// control for the refusals above.
#[cfg(unix)]
#[test]
fn without_the_guard_the_same_request_reaches_github() {
    let fixture = Fixture::new(&json!([]), SIGNED_BASE);
    let result = fixture.actions.run_gh_internal_queue_mutation(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={ENQUEUE}"),
        "-F".to_owned(),
        "id=PR_kwDOpulp4242".to_owned(),
        "-F".to_owned(),
        format!("head={HEAD}"),
    ]);
    assert!(result.is_ok(), "{result:?}");
    assert!(fixture.mutated());
}

/// `--arm` on a head with no verdict: the guard judges it and, with no stated
/// reason, refuses the override, so nothing reaches GitHub. Its refusal never
/// points back at `--arm`.
#[cfg(unix)]
#[test]
fn explicit_arm_without_a_verdict_is_refused() {
    let fixture = Fixture::new(&json!([]), SIGNED_BASE);
    let error = fixture.arm(None).expect_err("refused");
    assert!(is_refusal(&error), "{error}");
    assert!(
        error.contains("no reviewer verdict names head bca87fe519d6"),
        "{error}"
    );
    assert!(!error.contains("--arm"), "{error}");
    assert!(!fixture.armed(), "{}", fixture.calls());
    let entry = fixture.ledger().pop().expect("ledger line");
    assert_eq!(entry["verdict"], "refused", "{entry}");
    let audit = fixture
        .overrides()
        .pop()
        .expect("the declined override is audited");
    assert_eq!(audit["honoured"], false, "{audit}");
    assert_eq!(audit["operator"], "operator@host", "{audit}");
}

/// `--arm` on a head with a signed exact-head APPROVE lands on the review
/// itself; the override is never needed.
#[cfg(unix)]
#[test]
fn explicit_arm_with_a_verdict_passes() {
    let fixture = Fixture::new(&approve(HEAD, "reviewer: pulp-review\n"), SIGNED_BASE);
    fixture.arm(None).expect("allowed");
    assert!(fixture.armed(), "{}", fixture.calls());
    let entry = fixture.ledger().pop().expect("ledger line");
    assert_eq!(entry["verdict"], "allowed", "{entry}");
    assert_eq!(entry["signer"], "pulp-review", "{entry}");
    assert!(fixture.overrides().is_empty());
}

/// Even with a reason, an agent's environment cannot use the override.
#[cfg(unix)]
#[test]
fn explicit_arm_with_a_reason_is_refused_in_an_agent_context() {
    let fixture = Fixture::new(&json!([]), SIGNED_BASE);
    let mut gh = std::process::Command::new(fixture.temp.path().join("gh"));
    gh.env("CLAUDECODE", "1");
    let requested = super::OperatorOverride {
        operator: "operator@host".to_owned(),
        reason: Some("read it myself".to_owned()),
    };
    let error = with_real_guard(&fixture.ledger_path(), &fixture.guards(), || {
        super::check(
            &gh,
            &crate::auto_arm::arm_mutation_args_at_head("PR_kwDOpulp4242", HEAD),
            "shipyard ship",
            Some(&requested),
        )
    })
    .expect_err("refused");
    assert!(
        error.contains("refusing to arm or merge an unreviewed head"),
        "{error}"
    );
    let audit = fixture.overrides().pop().expect("audited");
    assert_eq!(audit["honoured"], false, "{audit}");
    assert_eq!(audit["reason"], "read it myself", "{audit}");
    assert!(
        audit["declined"]
            .as_str()
            .is_some_and(|why| why.contains("CLAUDECODE")),
        "{audit}"
    );
}

/// An override in the environment that nobody requested is stripped.
#[test]
fn an_unrequested_override_is_stripped() {
    let mut command = std::process::Command::new("gh");
    command.env(super::UNREVIEWED_OVERRIDE_ENV, "1");
    super::apply_override(&mut command, None);
    assert!(
        command
            .get_envs()
            .any(|(key, value)| { key == super::UNREVIEWED_OVERRIDE_ENV && value.is_none() })
    );
}
