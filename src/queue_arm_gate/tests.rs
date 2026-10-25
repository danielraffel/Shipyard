//! The gate runs the bundled queue-arm guard for real against a fake `gh`.
//!
//! Each case drives the steward's exact enqueue request through
//! [`crate::cloud::GitHubActions::run_gh_internal_queue_mutation`], the
//! transport the steward, the carrier and base-health jumps all use, and
//! asserts on what reached the fake `gh`: the mutation itself only when the
//! guard allowed it. They need a Unix `gh` script and Python; the portable
//! cases are in `portable_tests`.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::is_refusal;
use super::tests_support::with_real_guard;
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
            ["dequeuePullRequest", {"json": {"data": {"dequeuePullRequest": {"clientMutationId": null}}}}],
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
import json, os, sys
argv = ' '.join(sys.argv[1:])
open({calls:?}, 'a').write(argv + '\n')
open({calls:?} + '.env', 'a').write(' '.join(os.environ.get(name, '-') for name in (
    'SHIPYARD_INTERNAL_QUEUE_MUTATION', 'SHIPYARD_QUEUE_MUTATION_NONCE',
    'SHIPYARD_QUEUE_MUTATION_NONCE_DIR')) + '\n')
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

    /// `ship`'s arm (also what `--arm` asks for), through its plain
    /// transport, guard live.
    fn arm(&self) -> Result<String, String> {
        with_real_guard(&self.ledger_path(), &self.guards(), || {
            self.actions
                .run_gh(&crate::auto_arm::arm_mutation_args_at_head(
                    "PR_kwDOpulp4242",
                    HEAD,
                ))
                .map_err(|error| error.to_string())
        })
    }

    /// The marker, nonce and nonce directory each `gh` call carried.
    fn call_envs(&self) -> Vec<Vec<String>> {
        fs::read_to_string(self.temp.path().join("calls.env"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.split(' ').map(str::to_owned).collect())
            .collect()
    }

    fn armed(&self) -> bool {
        self.calls()
            .lines()
            .any(|line| line.contains("enablePullRequestAutoMerge"))
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

#[test]
fn steward_enqueue_bound_to_another_head_is_refused() {
    // The verdict names the live head, but the mutation would land another.
    let fixture = Fixture::new(&approve(HEAD, "reviewer: pulp-review\n"), SIGNED_BASE);
    let result = fixture.enqueue(OTHER_HEAD);
    let error = result.expect_err("refused");
    assert!(is_refusal(&error), "{error}");
    assert!(!fixture.mutated(), "{}", fixture.calls());
}

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
        super::check(&gh, &args, "shipyard test")
    })
    .expect_err("refused");
    assert!(is_refusal(&error), "{error}");
    let line = fs::read_to_string(&ledger).expect("ledger");
    let entry: Value = serde_json::from_str(line.trim()).expect("json");
    assert_eq!(entry["verdict"], "refused", "{entry}");
    assert_eq!(entry["caller"], "shipyard test", "{entry}");
}

/// The steward's transport, not just the gate in isolation: without the
/// real guard, the same unverdicted enqueue reaches GitHub. This is the
/// control for the refusals above.
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

/// `--arm` on a head with no verdict: the guard refuses it, nothing reaches
/// GitHub, and the refusal never points back at `--arm`.
#[test]
fn explicit_arm_without_a_verdict_is_refused() {
    let fixture = Fixture::new(&json!([]), SIGNED_BASE);
    let error = fixture.arm().expect_err("refused");
    assert!(is_refusal(&error), "{error}");
    assert!(
        error.contains("no reviewer verdict names head bca87fe519d6"),
        "{error}"
    );
    assert!(!error.contains("--arm"), "{error}");
    assert!(!fixture.armed(), "{}", fixture.calls());
    let entry = fixture.ledger().pop().expect("ledger line");
    assert_eq!(entry["verdict"], "refused", "{entry}");
}

/// `--arm` on a head with a signed exact-head APPROVE lands on the review.
#[test]
fn explicit_arm_with_a_verdict_passes() {
    let fixture = Fixture::new(&approve(HEAD, "reviewer: pulp-review\n"), SIGNED_BASE);
    fixture.arm().expect("allowed");
    assert!(fixture.armed(), "{}", fixture.calls());
    let entry = fixture.ledger().pop().expect("ledger line");
    assert_eq!(entry["verdict"], "allowed", "{entry}");
    assert_eq!(entry["signer"], "pulp-review", "{entry}");
}

/// An override on the landing command reaches neither the guard nor `gh`:
/// the unverdicted arm is refused all the same.
#[test]
fn an_override_on_the_landing_command_is_stripped() {
    let fixture = Fixture::new(&json!([]), SIGNED_BASE);
    let mut gh = std::process::Command::new(fixture.temp.path().join("gh"));
    gh.env("GHAPP_ALLOW_UNREVIEWED_ARM", "1")
        .env("GHAPP_ALLOW_QUEUE_REARM", "1");
    let error = with_real_guard(&fixture.ledger_path(), &fixture.guards(), || {
        super::check(
            &gh,
            &crate::auto_arm::arm_mutation_args_at_head("PR_kwDOpulp4242", HEAD),
            "shipyard ship",
        )
    })
    .expect_err("refused");
    assert!(
        error.contains("refusing to arm or merge an unreviewed head"),
        "{error}"
    );
    assert!(
        !error.contains("is not honoured"),
        "the override reached the guard: {error}"
    );
}

/// Each internal mutation carries the marker with a fresh nonce whose 0600
/// records sit in a 0700 directory; the guards' own `consume_nonce` honours
/// each record once, and only for its purpose.
#[test]
fn internal_mutations_carry_a_fresh_one_shot_nonce() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(&json!([]), SIGNED_BASE);
    let dequeue = [
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        "query=mutation($id:ID!){dequeuePullRequest(input:{id:$id}){clientMutationId}}".to_owned(),
    ];
    for _ in 0..2 {
        with_real_guard(&fixture.ledger_path(), &fixture.guards(), || {
            fixture.actions.run_gh_internal_queue_mutation(&dequeue)
        })
        .expect("ran");
    }
    let envs = fixture.call_envs();
    assert_eq!(envs.len(), 2, "{envs:?}");
    assert_eq!(envs[0][0], "1");
    assert_ne!(envs[0][1], envs[1][1], "each call gets its own nonce");
    let (nonce, directory) = (&envs[0][1], PathBuf::from(&envs[0][2]));
    assert_eq!(nonce.len(), 64, "{nonce}");
    assert_eq!(
        fs::metadata(&directory).expect("dir").permissions().mode() & 0o777,
        0o700
    );
    for purpose in [super::NONCE_INTERNAL_ARM, super::NONCE_INTERNAL_REMOVAL] {
        let record = directory.join(super::nonce_record_name(nonce, purpose));
        assert_eq!(
            fs::metadata(&record).expect("record").permissions().mode() & 0o777,
            0o600
        );
    }
    let consume = |purpose: &str| {
        let removal = crate::ghapp_guards::MANAGED_GUARDS
            .iter()
            .find(|guard| guard.name == "queue-removal-guard")
            .expect("bundled");
        let script = fixture.temp.path().join("removal_guard.py");
        fs::write(&script, removal.contents).expect("script");
        let output = std::process::Command::new("python3")
            .arg("-c")
            .arg(format!(
                "import importlib.machinery, importlib.util\n\
                 loader = importlib.machinery.SourceFileLoader('g', {:?})\n\
                 module = importlib.util.module_from_spec(importlib.util.spec_from_loader('g', loader))\n\
                 loader.exec_module(module)\n\
                 print(module.consume_nonce({purpose:?}))",
                script.display().to_string()
            ))
            .env(super::NONCE_ENV, nonce)
            .env(super::NONCE_DIR_ENV, &directory)
            .output()
            .expect("python3");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    assert_eq!(consume(super::NONCE_REVIEW_ONLY), "False", "wrong purpose");
    assert_eq!(consume(super::NONCE_INTERNAL_REMOVAL), "True");
    assert_eq!(consume(super::NONCE_INTERNAL_REMOVAL), "False", "replayed");
    assert_eq!(consume(super::NONCE_INTERNAL_ARM), "True");
}
