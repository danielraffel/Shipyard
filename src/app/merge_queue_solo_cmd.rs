//! `shipyard merge-queue solo` and `solo-release`: make and end a solo
//! merge-queue reservation ([`crate::merge_queue_solo`]).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::CliFailure;
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::head_approval::{self, HeadGate, RunGh};
use crate::identity::RuntimeMode;
use crate::merge_queue_control::authority_status;
use crate::merge_queue_solo::{self, Reservation, SoloReason};
use crate::output::write_json_envelope;

/// Exit code for a refusal: the request was understood and declined.
const REFUSED: u8 = 3;

/// What the operator asked for.
pub(super) struct SoloRequest<'a> {
    pub(super) target: &'a str,
    pub(super) reason: &'a str,
    pub(super) note: &'a str,
    pub(super) ttl_minutes: i64,
}

/// `owner/name#number`.
fn parse_target(target: &str) -> Result<(String, u64), String> {
    let (repo, number) = target
        .trim()
        .split_once('#')
        .ok_or_else(|| format!("`{target}` is not owner/name#number"))?;
    merge_queue_solo::reservation_path(Path::new("."), repo)?;
    let number = number
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| format!("`{target}` is not owner/name#number"))?;
    Ok((repo.to_owned(), number))
}

/// What the approval check found: the base, the approved head, the approval,
/// and whether the pull request is already in the queue.
pub(super) struct ApprovedHead {
    base: String,
    head: String,
    approval: String,
    in_queue: bool,
}

/// The pull request's base and head when its current head carries a
/// verified exact-head approval; anything else refuses.
pub(super) fn approved_head(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
) -> Result<ApprovedHead, String> {
    let pull: Value = serde_json::from_str(&run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/pulls/{pr}"),
    ])?)
    .map_err(|error| format!("pull request {repo}#{pr} unreadable: {error}"))?;
    let field = |pointer: &str| {
        pull.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("pull request {repo}#{pr} has no {pointer}"))
    };
    if field("/state")? != "open" {
        return Err(format!("{repo}#{pr} is not open"));
    }
    let base = field("/base/ref")?;
    let head = field("/head/sha")?;
    let author = field("/user/login").ok();
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("`{repo}` is not owner/name"))?;
    let queue: Value = serde_json::from_str(&run_gh(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={}", crate::pr_queue_state::PR_QUEUE_STATE_QUERY),
        "-F".to_owned(),
        format!("owner={owner}"),
        "-F".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("number={pr}"),
    ])?)
    .map_err(|error| format!("merge-queue state of {repo}#{pr} unreadable: {error}"))?;
    let live_head = queue
        .pointer("/data/repository/pullRequest/headRefOid")
        .and_then(Value::as_str);
    if live_head != Some(head.as_str()) {
        return Err(format!(
            "{repo}#{pr} changed head while it was read; run the command again"
        ));
    }
    let policy = head_approval::read_policy_required(run_gh, repo, &base)
        .map_err(|error| format!("head approval policy unreadable: {error}"))?;
    let (gate, _events) = head_approval::evaluate_with_policy_for_author_with_events(
        run_gh,
        repo,
        pr,
        &head,
        &policy,
        &queue,
        author.as_deref(),
    )?;
    match gate {
        HeadGate::Approved { approval } => Ok(ApprovedHead {
            in_queue: queue
                .pointer("/data/repository/pullRequest/isInMergeQueue")
                .and_then(Value::as_bool)
                == Some(true),
            base,
            head,
            approval: approval.describe(),
        }),
        // A base that requires no approval has no exact-head approval to
        // reserve on, and a reservation needs one.
        other => Err(format!(
            "{repo}#{pr} at {head} is not approved at its exact head: {}",
            other.explain()
        )),
    }
}

/// Make the reservation `request` describes, after the approval check.
pub(super) fn reserve(
    state_root: &Path,
    run_gh: RunGh<'_>,
    request: &SoloRequest<'_>,
    now: DateTime<Utc>,
) -> Result<(Reservation, std::path::PathBuf), CliFailure> {
    let (repo, pr) = parse_target(request.target).map_err(|error| CliFailure::new(2, error))?;
    let reason = SoloReason::parse(request.reason).map_err(|error| CliFailure::new(2, error))?;
    if request.note.trim().is_empty() {
        return Err(CliFailure::new(2, "--note must say what is landing alone"));
    }
    let approved =
        approved_head(run_gh, &repo, pr).map_err(|error| CliFailure::new(REFUSED, error))?;
    let mut reservation = Reservation::new(
        &repo,
        &approved.base,
        pr,
        &approved.head,
        reason,
        request.note,
        &approved.approval,
        request.ttl_minutes,
        now,
    )
    .map_err(|error| CliFailure::new(2, error))?;
    // Already queued: leaving the queue ends it from the first pass on.
    reservation.seen_in_queue = approved.in_queue;
    let path = merge_queue_solo::reserve(state_root, &reservation)
        .map_err(|error| CliFailure::new(REFUSED, error))?;
    Ok((reservation, path))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn solo_command<W: Write>(
    state_root: &Path,
    global_dir: &Path,
    cwd: &Path,
    mode: RuntimeMode,
    request: &SoloRequest<'_>,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    // Only the mutation machine's steward reads the reservation; one made
    // anywhere else would pause nothing.
    let authority = authority_status(state_root, cwd, mode, global_dir)
        .map_err(|error| CliFailure::new(1, error))?;
    if authority["authority_matches"].as_bool() != Some(true) {
        return Err(CliFailure::new(
            REFUSED,
            format!(
                "this host ({}) is not the configured merge-queue mutation machine ({}); \
                 make the reservation there",
                authority["machine"].as_str().unwrap_or("unknown"),
                authority["mutation_machine"]
                    .as_str()
                    .unwrap_or("unconfigured")
            ),
        ));
    }
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, format!("cannot load config: {error}")))?;
    let actions = GitHubActions::from_loaded_config(cwd, &config);
    let run_gh = |argv: &[String]| actions.run_gh(argv).map_err(|error| error.to_string());
    let (reservation, path) = reserve(state_root, &run_gh, request, Utc::now())?;
    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "reservation".to_owned(),
            serde_json::to_value(&reservation).unwrap_or(Value::Null),
        );
        data.insert("path".to_owned(), Value::String(path.display().to_string()));
        write_json_envelope(stdout, "merge-queue.solo", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "{}#{} holds the {} queue alone until {} ({})",
            reservation.repo,
            reservation.pr,
            reservation.base,
            reservation.expires_at.to_rfc3339(),
            path.display()
        )
        .ok();
    }
    Ok(())
}

pub(super) fn solo_release_command<W: Write>(
    state_root: &Path,
    repo: &str,
    reason: &str,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if reason.trim().is_empty() {
        return Err(CliFailure::new(2, "--reason must not be empty"));
    }
    let removed = merge_queue_solo::release(state_root, repo.trim(), reason.trim())
        .map_err(|error| CliFailure::new(1, error))?;
    if json {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::String(repo.to_owned()));
        data.insert("changed".to_owned(), Value::Bool(removed));
        write_json_envelope(stdout, "merge-queue.solo-release", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if removed {
        writeln!(stdout, "solo reservation for {repo} ended").ok();
    } else {
        writeln!(stdout, "{repo} had no solo reservation").ok();
    }
    Ok(())
}

/// `merge-queue solo` and `solo-release`.
pub(super) fn dispatch<W: Write>(
    command: super::cli::MergeQueueCommand,
    state_root: &Path,
    global_dir: &Path,
    cwd: &Path,
    mode: RuntimeMode,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    match command {
        super::cli::MergeQueueCommand::Solo {
            target,
            reason,
            note,
            ttl_minutes,
        } => solo_command(
            state_root,
            global_dir,
            cwd,
            mode,
            &SoloRequest {
                target: &target,
                reason: &reason,
                note: &note,
                ttl_minutes,
            },
            json,
            stdout,
        ),
        super::cli::MergeQueueCommand::SoloRelease { repo, reason } => {
            solo_release_command(state_root, &repo, &reason, json, stdout)
        }
        _ => Err(CliFailure::new(2, "not a solo reservation command")),
    }
}

/// One human line per reservation, for `merge-queue status`.
pub(super) fn write_status_lines<W: Write>(state_root: &Path, stdout: &mut W) {
    for (repo, state) in merge_queue_solo::list(state_root, Utc::now()) {
        writeln!(stdout, "solo {repo}: {}", state.describe()).ok();
    }
}

/// Every reservation on this host, for `merge-queue status`.
pub(super) fn status_report(state_root: &Path) -> Vec<Value> {
    merge_queue_solo::list(state_root, Utc::now())
        .into_iter()
        .map(|(repo, state)| {
            let mut report = state.report();
            if let Some(object) = report.as_object_mut() {
                object.insert("repo".to_owned(), Value::String(repo));
            }
            report
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// A fake GitHub for pulp#451 at `HEAD`, with `comments` on the issue.
    fn github(comments: &'static str) -> impl Fn(&[String]) -> Result<String, String> {
        move |argv: &[String]| {
            let joined = argv.join(" ");
            if joined == "api repos/Generous-Corp/pulp/pulls/451" {
                return Ok(format!(
                    r#"{{"state":"open","base":{{"ref":"main"}},"head":{{"sha":"{HEAD}"}},"user":{{"login":"release-bot"}}}}"#
                ));
            }
            if joined.starts_with("api repos/Generous-Corp/pulp/pulls/451 --jq") {
                return Ok(r#"{"base_ref":"main","head_ref":"release/coalesce","head_owner":"Generous-Corp"}"#.to_owned());
            }
            if joined.contains("contents/.shipyard/config.toml") {
                return Ok("[auto_merge]\narm_requires_head_approval = true\nreviewer_identities = [\"shipyard-review[bot]\"]\n".to_owned());
            }
            if joined.contains("isInMergeQueue") {
                return Ok(format!(
                    r#"{{"data":{{"repository":{{"pullRequest":{{"number":451,"state":"OPEN","headRefOid":"{HEAD}","isInMergeQueue":true,"mergeQueueEntry":{{"state":"QUEUED","position":2}},"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}"#
                ));
            }
            if joined.ends_with("--jq .commit.committer.date") {
                return Ok("2026-11-11T01:00:00Z".to_owned());
            }
            if joined.contains("pulls/451/reviews") {
                return Ok(String::new());
            }
            if joined.contains("issues/451/comments") {
                return Ok(comments.to_owned());
            }
            if joined.starts_with("api repos/Generous-Corp/pulp/pulls/451 --jq [.head.ref") {
                return Ok("release/coalesce\nmain".to_owned());
            }
            Err(format!("unexpected: {joined}"))
        }
    }

    const APPROVAL: &str = r#"{"id":6390564321,"login":"shipyard-review[bot]","type":"Bot","association":"NONE","at":"2026-11-11T01:53:36Z","performed_via_github_app_id":null,"body":"verdict: APPROVE head:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
    const HOLD: &str = r#"{"id":6390564322,"login":"shipyard-review[bot]","type":"Bot","association":"NONE","at":"2026-11-11T01:53:36Z","performed_via_github_app_id":null,"body":"verdict: HOLD head:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;

    fn request(target: &str) -> SoloRequest<'_> {
        SoloRequest {
            target,
            reason: "release",
            note: "v1.4 release coalesce lands alone",
            ttl_minutes: merge_queue_solo::DEFAULT_TTL_MINUTES,
        }
    }

    #[test]
    fn a_reservation_is_made_for_a_pull_request_approved_at_its_exact_head() {
        let temp = tempfile::tempdir().expect("temp");
        let run_gh = github(APPROVAL);
        let (reservation, _path) = head_approval::with_host_review_default(false, || {
            reserve(
                temp.path(),
                &run_gh,
                &request("Generous-Corp/pulp#451"),
                Utc::now(),
            )
        })
        .expect("reserved");
        assert_eq!(reservation.pr, 451);
        assert_eq!(reservation.head, HEAD);
        assert_eq!(reservation.base, "main");
        // The fake shows it queued, so leaving the queue ends it at once.
        assert!(reservation.seen_in_queue);
        assert!(matches!(
            merge_queue_solo::read(temp.path(), "Generous-Corp/pulp", Utc::now()),
            merge_queue_solo::SoloState::Active(_)
        ));
    }

    #[test]
    fn a_reservation_is_refused_for_a_pull_request_not_approved_at_its_exact_head() {
        for comments in ["", HOLD] {
            let temp = tempfile::tempdir().expect("temp");
            let run_gh = github(comments);
            let refusal = head_approval::with_host_review_default(false, || {
                reserve(
                    temp.path(),
                    &run_gh,
                    &request("Generous-Corp/pulp#451"),
                    Utc::now(),
                )
            })
            .expect_err("refused");
            assert_eq!(refusal.code, REFUSED, "{}", refusal.message);
            assert!(
                refusal.message.contains("not approved"),
                "{}",
                refusal.message
            );
            assert_eq!(
                merge_queue_solo::read(temp.path(), "Generous-Corp/pulp", Utc::now()),
                merge_queue_solo::SoloState::Absent
            );
        }
    }

    #[test]
    fn a_reservation_is_refused_on_a_host_that_is_not_the_mutation_machine() {
        let temp = tempfile::tempdir().expect("temp");
        let global = temp.path().join("global");
        let state = temp.path().join("state");
        std::fs::create_dir_all(&global).expect("global");
        std::fs::create_dir_all(&state).expect("state");
        std::fs::write(
            global.join("config.toml"),
            "[merge_queue]\nmutation_machine = \"studio\"\n",
        )
        .expect("config");
        std::fs::write(state.join("machine-tag"), "laptop\n").expect("tag");
        let mut out = Vec::new();
        let refusal = solo_command(
            &state,
            &global,
            temp.path(),
            RuntimeMode::Isolated,
            &request("Generous-Corp/pulp#451"),
            true,
            &mut out,
        )
        .expect_err("refused");
        assert_eq!(refusal.code, REFUSED, "{}", refusal.message);
        assert!(
            refusal.message.contains("mutation machine"),
            "{}",
            refusal.message
        );
        assert!(out.is_empty());
        assert_eq!(
            merge_queue_solo::read(&state, "Generous-Corp/pulp", Utc::now()),
            merge_queue_solo::SoloState::Absent
        );
    }

    #[test]
    fn a_reservation_needs_a_known_reason_class_and_a_target() {
        let temp = tempfile::tempdir().expect("temp");
        let run_gh = github(APPROVAL);
        let mut bad = request("Generous-Corp/pulp#451");
        bad.reason = "reorder";
        assert_eq!(
            reserve(temp.path(), &run_gh, &bad, Utc::now())
                .expect_err("reason")
                .code,
            2
        );
        assert!(reserve(temp.path(), &run_gh, &request("pulp#451"), Utc::now()).is_err());
        assert!(
            reserve(
                temp.path(),
                &run_gh,
                &request("Generous-Corp/pulp"),
                Utc::now()
            )
            .is_err()
        );
    }
}
