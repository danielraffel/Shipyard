//! Durable reviewer claim acquisition and validation.
//!
//! A reviewer must acquire a claim before posting a verdict. Acquisition
//! persists the exact-head claim first and then publishes one authenticated
//! `shipyard-reviewer-claim/v1` check-run receipt. The ghapp reviewer guard
//! calls `validate` immediately before a verdict write, so an expired,
//! revoked, replaced, or wrong-head claim fails closed.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use serde_json::Value;

use super::CliFailure;
use super::cli::ReviewerClaimCommand;
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::gh::GhClient;
use crate::head_approval::CONFIG_PATH;
use crate::identity::RuntimeMode;
use crate::landability::gate;
use crate::output::write_json_envelope;
use crate::reviewer_receipt::{
    CheckRunTransport, ClaimApp, GhAppCheckRunTransport, REVIEWER_CLAIM_CHECK_NAME,
    claim_app_from_base_config,
};
use crate::ship_state::{ReviewerClaim, ReviewerClaimRequest, ShipStateStore};

const GITHUB_TIMEOUT: Duration = Duration::from_mins(1);
const MAX_DEADLINE_SECONDS: i64 = 24 * 60 * 60;

/// Run one durable reviewer claim operation.
pub(super) fn reviewer_claim_command<W: Write>(
    command: ReviewerClaimCommand,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let actions = GitHubActions::from_loaded_config(cwd, &config);
    let store = ShipStateStore::new(state_dir.join("ship"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;

    match command {
        ReviewerClaimCommand::Acquire {
            pr,
            repo,
            head,
            reviewer,
            deadline_seconds,
            check_name,
        } => acquire(
            &store,
            &actions,
            &config,
            cwd,
            pr,
            repo,
            &head,
            &reviewer,
            deadline_seconds,
            &check_name,
            json_output,
            stdout,
        ),
        ReviewerClaimCommand::Validate {
            pr,
            repo,
            head,
            reviewer,
        } => validate(
            &store,
            &config,
            cwd,
            pr,
            repo,
            &head,
            &reviewer,
            json_output,
            stdout,
        ),
        ReviewerClaimCommand::Revoke { .. } => Err(CliFailure::new(
            1,
            "reviewer claim revoke is disabled until authenticated App receipt invalidation is implemented",
        )),
    }
}

fn resolve_repo(
    config: &LoadedConfig,
    cwd: &Path,
    repo: Option<String>,
) -> Result<String, CliFailure> {
    repo.filter(|value| !value.trim().is_empty())
        .or_else(|| gate::resolve_repo(config, cwd))
        .ok_or_else(|| CliFailure::new(2, "No repo detected. Pass --repo OWNER/REPO."))
}

fn validate_sha(head: &str) -> Result<(), CliFailure> {
    if head.len() != 40 || !head.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(CliFailure::new(
            2,
            format!("--head must be a full 40-character commit sha, not {head:?}"),
        ));
    }
    Ok(())
}

/// The pull request's live head, which must equal `requested`, and its base
/// branch.
fn live_pull(
    actions: &GitHubActions,
    repo: &str,
    pr: u64,
    requested: &str,
) -> Result<(String, String), CliFailure> {
    let raw = actions
        .run_gh_with_timeout(
            &["api".to_owned(), format!("repos/{repo}/pulls/{pr}")],
            GITHUB_TIMEOUT,
        )
        .map_err(|error| CliFailure::new(2, format!("read PR head: {error}")))?;
    let pull: Value = serde_json::from_str(&raw)
        .map_err(|error| CliFailure::new(2, format!("read PR head: {error}")))?;
    let live = pull
        .get("head")
        .and_then(|head| head.get("sha"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if live.len() != 40 || !live.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(CliFailure::new(2, format!("PR #{pr} has no readable head")));
    }
    let base = pull
        .get("base")
        .and_then(|base| base.get("ref"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .ok_or_else(|| CliFailure::new(2, format!("PR #{pr} has no readable base branch")))?;
    if !live.eq_ignore_ascii_case(requested) {
        return Err(CliFailure::new(
            1,
            format!("PR #{pr} moved: requested {requested}, live head is {live}"),
        ));
    }
    Ok((live.to_ascii_lowercase(), base.to_owned()))
}

/// Refuse a `--check-name` other than the one receipts are published under.
fn validate_check_name(check_name: &str) -> Result<(), CliFailure> {
    if check_name.trim() == REVIEWER_CLAIM_CHECK_NAME {
        Ok(())
    } else {
        Err(CliFailure::new(
            2,
            format!("--check-name is fixed to {REVIEWER_CLAIM_CHECK_NAME}"),
        ))
    }
}

/// The App the protected base names as the only receipt publisher, read from
/// the base branch's `.shipyard/config.toml`. An unreadable file, a disabled
/// or partial policy, or a shared App refuses the claim before anything is
/// persisted or published.
fn base_claim_app(actions: &GitHubActions, repo: &str, base: &str) -> Result<ClaimApp, CliFailure> {
    let text = actions
        .run_gh_with_timeout(
            &[
                "api".to_owned(),
                "-H".to_owned(),
                "Accept: application/vnd.github.raw+json".to_owned(),
                format!("repos/{repo}/contents/{CONFIG_PATH}?ref={base}"),
            ],
            GITHUB_TIMEOUT,
        )
        .map_err(|error| {
            CliFailure::new(
                2,
                format!("read the reviewer claim policy from {CONFIG_PATH} on {base}: {error}"),
            )
        })?;
    claim_app_from_base_config(Some(&text))
        .map_err(|error| CliFailure::new(2, format!("reviewer claim policy refused: {error}")))
}

/// Persist the claim, then publish its receipt through `transport`. A failed
/// publication revokes the claim before the error is returned.
fn publish_claim<T: CheckRunTransport>(
    store: &ShipStateStore,
    request: ReviewerClaimRequest<'_>,
    transport: &mut T,
) -> Result<ReviewerClaim, CliFailure> {
    let repo = request.repository.to_owned();
    let pr = request.pr;
    store
        .claim_reviewer_and_publish(request, |claim| {
            let receipt = crate::reviewer_receipt::request_from_claim(&repo, pr, claim);
            crate::reviewer_receipt::publish_or_reconcile(transport, &receipt)
                .map(|_| claim.clone())
        })
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

#[allow(clippy::too_many_arguments)] // The CLI boundary keeps every claim input explicit.
fn acquire<W: Write>(
    store: &ShipStateStore,
    actions: &GitHubActions,
    config: &LoadedConfig,
    cwd: &Path,
    pr: u64,
    requested_repo: Option<String>,
    head: &str,
    reviewer: &str,
    deadline_seconds: i64,
    check_name: &str,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    validate_sha(head)?;
    let reviewer = reviewer.trim();
    if reviewer.is_empty() {
        return Err(CliFailure::new(2, "--reviewer must not be empty"));
    }
    if !(1..=MAX_DEADLINE_SECONDS).contains(&deadline_seconds) {
        return Err(CliFailure::new(
            2,
            format!("--deadline-seconds must be between 1 and {MAX_DEADLINE_SECONDS}"),
        ));
    }
    validate_check_name(check_name)?;
    let repo = resolve_repo(config, cwd, requested_repo)?;
    let actions = actions.clone().with_repo_override(&repo);
    let (head, base) = live_pull(&actions, &repo, pr, head)?;
    let app = base_claim_app(&actions, &repo, &base)?;
    let claimed_at = Utc::now();
    let deadline_at = claimed_at + chrono::Duration::seconds(deadline_seconds);
    let request = ReviewerClaimRequest {
        repository: &repo,
        pr,
        head_sha: &head,
        reviewer,
        claimed_at,
        deadline_at,
        now: claimed_at,
    };
    let gh = GhClient::from_loaded_config(config)
        .map_err(|error| CliFailure::new(2, format!("GitHub auth config: {error}")))?;
    let mut transport = GhAppCheckRunTransport::new(gh, cwd, &repo, app)
        .map_err(|error| CliFailure::new(2, format!("GitHub App transport: {error}")))?;
    let claim = publish_claim(store, request, &mut transport)?;
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::String(repo.clone()));
        data.insert("pr".to_owned(), Value::from(pr));
        data.insert("head".to_owned(), Value::String(head));
        data.insert(
            "check_name".to_owned(),
            Value::String(REVIEWER_CLAIM_CHECK_NAME.to_owned()),
        );
        data.insert(
            "claim".to_owned(),
            serde_json::to_value(&claim).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        write_json_envelope(stdout, "reviewer-claim", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "reviewer claim acquired for {repo}#{pr} head {head} epoch {} until {}",
            claim.epoch,
            claim.deadline_at.to_rfc3339_opts(SecondsFormat::Secs, true)
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)] // The CLI boundary keeps every claim input explicit.
fn validate<W: Write>(
    store: &ShipStateStore,
    config: &LoadedConfig,
    cwd: &Path,
    pr: u64,
    requested_repo: Option<String>,
    head: &str,
    reviewer: &str,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    validate_sha(head)?;
    let repo = resolve_repo(config, cwd, requested_repo)?;
    let reviewer = reviewer.trim();
    let state = store.get_scoped(&repo, pr);
    let claim = state
        .as_ref()
        .and_then(|state| state.active_reviewer_claim.as_ref());
    let current = state.as_ref().is_some_and(|state| {
        state.reviewer_claim_is_current(head, state.reviewer_claim_epoch, reviewer, Utc::now())
    });
    if json_output {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::String(repo.clone()));
        data.insert("pr".to_owned(), Value::from(pr));
        data.insert("head".to_owned(), Value::String(head.to_ascii_lowercase()));
        data.insert("reviewer".to_owned(), Value::String(reviewer.to_owned()));
        data.insert("current".to_owned(), Value::Bool(current));
        data.insert(
            "claim_epoch".to_owned(),
            claim.map_or(Value::Null, |claim| Value::from(claim.epoch)),
        );
        data.insert(
            "external_id".to_owned(),
            claim.map_or(Value::Null, |claim| {
                Value::String(claim.receipt_id(&repo, pr))
            }),
        );
        write_json_envelope(stdout, "reviewer-claim", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "reviewer claim: {}",
            if current { "current" } else { "refused" }
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(if current {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use chrono::{Duration, Utc};
    use serde_json::{Value, json};

    use super::super::cli::{Cli, Command, ReviewerClaimCommand};
    use super::{REVIEWER_CLAIM_CHECK_NAME, base_claim_app, publish_claim, validate_check_name};
    use crate::reviewer_receipt::{ClaimApp, GhAppCheckRunTransport};
    use crate::ship_state::{ReviewerClaimRequest, ShipState, ShipStateStore};

    const REPO: &str = "Generous-Corp/pulp";

    fn head() -> String {
        "c".repeat(40)
    }

    /// Check runs on GitHub as the transport sees them, answering as the
    /// App whose identity `app` is.
    struct FakeGitHub {
        app: Value,
        runs: Vec<Value>,
        patches: Vec<(String, Value)>,
    }

    fn transport(github: &Rc<RefCell<FakeGitHub>>, policy: ClaimApp) -> GhAppCheckRunTransport {
        let github = Rc::clone(github);
        GhAppCheckRunTransport::with_runner(REPO, policy, move |args, input| {
            let mut github = github.borrow_mut();
            let body = || -> Value {
                serde_json::from_slice(input.expect("request body on stdin")).expect("JSON body")
            };
            if args.iter().any(|arg| arg == "POST") {
                let mut run = body();
                run["id"] = json!(99);
                run["app"] = github.app.clone();
                github.runs.push(run.clone());
                return Ok(run);
            }
            if args.iter().any(|arg| arg == "PATCH") {
                let patch = body();
                github.patches.push((args[1].clone(), patch.clone()));
                let id: u64 = args[1].rsplit('/').next().expect("id").parse().expect("id");
                let run = github
                    .runs
                    .iter_mut()
                    .find(|run| run["id"] == json!(id))
                    .expect("patched run exists");
                for (key, value) in patch.as_object().expect("patch object") {
                    run[key] = value.clone();
                }
                return Ok(run.clone());
            }
            Ok(json!([{ "check_runs": github.runs.clone() }]))
        })
    }

    fn store_with_state(temp: &tempfile::TempDir) -> ShipStateStore {
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store
            .save(&ShipState::new(
                11,
                REPO,
                "feature/x",
                "main",
                head(),
                "policy0001",
            ))
            .expect("save state");
        store
    }

    fn acquire_with(
        store: &ShipStateStore,
        github: &Rc<RefCell<FakeGitHub>>,
    ) -> Result<(), String> {
        let now = Utc::now();
        let head = head();
        let request = ReviewerClaimRequest {
            repository: REPO,
            pr: 11,
            head_sha: &head,
            reviewer: "pulp-review",
            claimed_at: now,
            deadline_at: now + Duration::minutes(30),
            now,
        };
        let mut transport = transport(github, ClaimApp::Id(4242));
        publish_claim(store, request, &mut transport)
            .map(|_| ())
            .map_err(|error| error.message)
    }

    #[test]
    fn a_receipt_from_the_configured_fleet_app_is_published() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = store_with_state(&temp);
        let github = Rc::new(RefCell::new(FakeGitHub {
            app: json!({"slug": "shipyard-review", "id": 4242}),
            runs: Vec::new(),
            patches: Vec::new(),
        }));
        acquire_with(&store, &github).expect("published");
        let github = github.borrow();
        assert_eq!(github.runs.len(), 1);
        assert_eq!(github.runs[0]["conclusion"], "success");
        assert_eq!(github.runs[0]["name"], REVIEWER_CLAIM_CHECK_NAME);
        assert!(github.patches.is_empty());
        let state = store.get_scoped(REPO, 11).expect("state");
        assert_eq!(state.active_reviewer_claim.expect("claim").epoch, 1);
    }

    #[test]
    fn a_receipt_from_the_shared_actions_app_is_refused_and_left_failed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = store_with_state(&temp);
        let github = Rc::new(RefCell::new(FakeGitHub {
            app: json!({"slug": "github-actions", "id": 15368}),
            runs: Vec::new(),
            patches: Vec::new(),
        }));
        let error = acquire_with(&store, &github).expect_err("refused");
        assert!(error.contains("marked failed"), "{error}");
        let github = github.borrow();
        assert_eq!(github.patches.len(), 1);
        assert_eq!(github.patches[0].0, format!("repos/{REPO}/check-runs/99"));
        assert_eq!(github.patches[0].1["conclusion"], "failure");
        assert!(
            github.runs.iter().all(|run| run["conclusion"] != "success"),
            "no success receipt is left behind: {:?}",
            github.runs
        );
        let state = store.get_scoped(REPO, 11).expect("state");
        assert!(state.active_reviewer_claim.is_none());
        assert_eq!(state.reviewer_claim_epoch, 2, "the refused claim is fenced");
    }

    #[test]
    fn the_default_check_name_is_the_one_acquire_requires() {
        let cli = Cli::parse_from([
            "shipyard",
            "reviewer-claim",
            "acquire",
            "--pr",
            "11",
            "--repo",
            REPO,
            "--head",
            &head(),
            "--reviewer",
            "pulp-review",
        ]);
        let Command::ReviewerClaim {
            command: ReviewerClaimCommand::Acquire { check_name, .. },
        } = cli.command
        else {
            panic!("parsed as another command");
        };
        assert_eq!(check_name, REVIEWER_CLAIM_CHECK_NAME);
        assert!(validate_check_name(&check_name).is_ok());
        assert!(validate_check_name("shipyard-reviewer-claim").is_err());
    }

    #[cfg(unix)]
    fn base_policy(answer: &str) -> Result<ClaimApp, String> {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = temp.path().join("gh");
        let log = temp.path().join("args.log");
        crate::test_support::write_executable_script(
            &gh,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{answer}\n",
                log.display()
            ),
        );
        let actions = crate::cloud::GitHubActions::hermetic_for_tests(temp.path(), &gh);
        let result = base_claim_app(&actions, REPO, "release").map_err(|error| error.message);
        let args = std::fs::read_to_string(log).expect("gh was called");
        assert!(
            args.contains(&format!(
                "repos/{REPO}/contents/.shipyard/config.toml?ref=release"
            )),
            "{args}"
        );
        result
    }

    #[cfg(unix)]
    #[test]
    fn acquire_reads_the_receipt_app_from_the_base_policy() {
        let enabled = "[auto_merge]\nreviewer_claim_receipt = \"reviewer-claim\"\n";
        assert_eq!(
            base_policy(&format!("printf '%s' '{enabled}reviewer_claim_app = 4242'")),
            Ok(ClaimApp::Id(4242))
        );
        let shared = base_policy(&format!(
            "printf '%s' '{enabled}reviewer_claim_app = 15368'"
        ))
        .expect_err("the shared id is refused");
        assert!(shared.contains("15368"), "{shared}");
        let partial = base_policy(&format!("printf '%s' '{enabled}'"))
            .expect_err("a partial policy is refused");
        assert!(partial.contains("reviewer_claim_app"), "{partial}");
        let absent = base_policy("echo 'gh: Not Found (HTTP 404)' >&2; exit 1")
            .expect_err("an absent policy is refused");
        assert!(absent.contains("reviewer claim policy"), "{absent}");
    }
}
