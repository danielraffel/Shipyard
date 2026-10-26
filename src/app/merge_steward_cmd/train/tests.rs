use std::cell::RefCell;
use std::collections::BTreeMap;

use super::{Host, Stage, TrainAction, armable_stage, train_command};
use crate::app::merge_steward_cmd::{ObservedPr, RepoObservation, parse_pr};
use crate::cloud::GitHubActions;
use crate::merge_steward::{CapacityPreemptionPolicy, RequiredCheck};

const REPO: &str = "owner/repo";
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A `gh` stand-in that logs every call and answers the base config with
/// `config`, or 404 when it is `None`. Anything else fails, so a test sees
/// every call the command made.
#[cfg(unix)]
fn fake_gh(temp: &tempfile::TempDir, config: Option<&str>) -> GitHubActions {
    let path = temp.path().join("gh");
    let log = temp.path().join("calls");
    let answer = config.map_or_else(
        || "echo 'gh: Not Found (HTTP 404)' >&2; exit 1".to_owned(),
        |text| format!("printf '%s' '{text}'"),
    );
    crate::test_support::write_executable_script(
        &path,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  \
             *contents/.shipyard/config.toml*) {answer} ;;\n  \
             *) echo \"unexpected: $*\" >&2; exit 2 ;;\nesac\n",
            log.display()
        ),
    );
    let loaded = crate::config::LoadedConfig {
        data: toml::Table::new(),
        global_dir: temp.path().join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: crate::config::LocalOverlaySource::None,
    };
    GitHubActions::from_loaded_config(temp.path(), &loaded).with_gh_binary_for_tests(path)
}

#[cfg(unix)]
fn calls(temp: &tempfile::TempDir) -> Vec<String> {
    std::fs::read_to_string(temp.path().join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[cfg(unix)]
fn status(temp: &tempfile::TempDir, actions: &GitHubActions, host: &Host) -> String {
    let mut out = Vec::new();
    train_command(
        actions,
        TrainAction::Status {
            repo: REPO.to_owned(),
            base: "main".to_owned(),
            record: true,
        },
        host,
        &temp.path().join("state"),
        false,
        &mut out,
    )
    .expect("status");
    String::from_utf8(out).expect("utf8")
}

#[cfg(unix)]
fn no_kill() -> Host {
    Host {
        kill_env: None,
        pause: None,
        operator: "agent@m3".to_owned(),
    }
}

/// OFF contract: a kill switch reads nothing from GitHub and records
/// nothing, whatever the base config says.
#[cfg(unix)]
#[test]
fn a_killed_train_makes_no_github_call_and_records_nothing() {
    let temp = tempfile::tempdir().expect("temp");
    let pause = temp.path().join("train.pause");
    std::fs::write(&pause, "anything at all").expect("pause");
    for (host, said) in [
        (
            Host {
                kill_env: Some("off".to_owned()),
                pause: None,
                operator: "agent@m3".to_owned(),
            },
            "killed by SHIPYARD_TRAIN=off",
        ),
        (
            Host {
                kill_env: Some(format!("off:{REPO}")),
                pause: None,
                operator: "agent@m3".to_owned(),
            },
            "killed by SHIPYARD_TRAIN=off:owner/repo",
        ),
        (
            Host {
                kill_env: None,
                pause: Some(pause.clone()),
                operator: "agent@m3".to_owned(),
            },
            "paused by the guards-dir train.pause",
        ),
    ] {
        let actions = fake_gh(&temp, Some("[train]\nenabled = true\n"));
        let text = status(&temp, &actions, &host);
        assert!(text.contains(said), "{text}");
        assert_eq!(calls(&temp), Vec::<String>::new(), "{said}");
        assert!(!temp.path().join("state/train").exists());
    }
}

/// OFF contract: with `[train]` absent, `enabled = false`, or no config at
/// all, status makes exactly the one base-config read and records nothing.
#[cfg(unix)]
#[test]
fn a_train_off_in_config_makes_one_read_and_records_nothing() {
    for config in [
        None,
        Some("[queue]\nx = 1\n"),
        Some("[train]\nenabled = false\n"),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(&temp, config);
        let text = status(&temp, &actions, &no_kill());
        assert!(
            text.contains("does not set `[train] enabled = true`"),
            "{config:?}: {text}"
        );
        let made = calls(&temp);
        assert_eq!(made.len(), 1, "{made:?}");
        assert!(made[0].contains("contents/.shipyard/config.toml?ref=main"));
        assert!(!temp.path().join("state/train").exists());
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_config_is_off_not_on() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, Some("[train\n"));
    let text = status(&temp, &actions, &no_kill());
    assert!(
        text.contains("off: the base config is unreadable"),
        "{text}"
    );
}

#[test]
fn pause_records_why_and_resume_removes_it() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("guards").join("train.pause");
    let host = |operator: &str, kill_env: Option<&str>| Host {
        kill_env: kill_env.map(str::to_owned),
        pause: Some(path.clone()),
        operator: operator.to_owned(),
    };
    let actions = GitHubActions::offline_for_tests(temp.path());
    let run = |host: &Host, action| {
        let mut out = Vec::new();
        train_command(&actions, action, host, temp.path(), false, &mut out)
            .map(|_| String::from_utf8(out).expect("utf8"))
            .map_err(|failure| failure.message)
    };
    let me = host("agent@m3", None);
    let resume = |reason: &str| TrainAction::Resume {
        reason: reason.to_owned(),
    };
    assert!(
        run(
            &me,
            TrainAction::Pause {
                reason: "  ".to_owned()
            }
        )
        .is_err()
    );
    run(
        &me,
        TrainAction::Pause {
            reason: "deploy freeze".to_owned(),
        },
    )
    .expect("pause");
    assert_eq!(
        crate::merge_train::read_pause(&path),
        Some("deploy freeze".to_owned())
    );
    // Only the pauser lifts it, with a reason, and not while the env kills.
    assert!(run(&me, resume(" ")).is_err());
    let other = run(&host("someone@m5", None), resume("done")).expect_err("refused");
    assert!(other.contains("paused by agent@m3"), "{other}");
    for kill in ["off", "0", "false", "off:owner/repo"] {
        let killed = run(&host("agent@m3", Some(kill)), resume("done")).expect_err("refused");
        assert!(killed.contains("still kills the train"), "{kill}: {killed}");
    }
    assert!(path.exists());
    assert!(
        run(&me, resume("freeze lifted"))
            .expect("resume")
            .contains("resumed")
    );
    assert!(!path.exists());
    assert!(
        run(&me, resume("again"))
            .expect("resume")
            .contains("was not paused")
    );
    // Both are audited beside the pause file.
    let audit =
        std::fs::read_to_string(temp.path().join("guards/train-pauses.jsonl")).expect("audit");
    let rows: Vec<serde_json::Value> = audit
        .lines()
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["action"], "pause");
    assert_eq!(rows[1]["action"], "resume");
    assert_eq!(rows[1]["reason"], "freeze lifted");
    assert_eq!(rows[1]["by"], "agent@m3");
}

/// A pause file that names no pauser is removed by hand, never by resume.
#[test]
fn a_pause_file_with_no_pauser_is_not_resumed() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("train.pause");
    std::fs::write(&path, "garbage").expect("write");
    let host = Host {
        kill_env: None,
        pause: Some(path.clone()),
        operator: "agent@m3".to_owned(),
    };
    let actions = GitHubActions::offline_for_tests(temp.path());
    let mut out = Vec::new();
    let failure = train_command(
        &actions,
        TrainAction::Resume {
            reason: "x".to_owned(),
        },
        &host,
        temp.path(),
        false,
        &mut out,
    )
    .expect_err("refused");
    assert!(
        failure.message.contains("names no pauser"),
        "{}",
        failure.message
    );
    assert!(path.exists());
}

fn observation(pr: ObservedPr) -> RepoObservation {
    RepoObservation {
        repo: REPO.to_owned(),
        base: "main".to_owned(),
        allow_auto_merge: true,
        merge_queue: true,
        required_checks: vec![RequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
        }],
        prs: vec![pr],
        runs: Vec::new(),
        merge_group_heads: BTreeMap::new(),
        merge_group_enqueued_at: BTreeMap::new(),
        capacity_preemption_policy: CapacityPreemptionPolicy::pulp(),
        preemption_error: None,
    }
}

fn green_pr() -> ObservedPr {
    parse_pr(
        &serde_json::json!({
            "id": "PR_node42", "number": 42, "state": "OPEN", "isDraft": false,
            "baseRefName": "main", "headRefOid": HEAD, "headRefName": "feature",
            "mergeStateStatus": "CLEAN", "autoMergeRequest": null, "labels": [],
            "author": {"login": "someone-else"},
        }),
        &BTreeMap::new(),
    )
    .expect("PR row")
}

const NEVER_ARMED: &str = r#"{"data":{"repository":{"pullRequest":{"number":42,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{"pageInfo":{"hasPreviousPage":false},"nodes":[]}}}}}"#;
const APPROVAL_CONFIG: &str =
    "[auto_merge]\narm_requires_head_approval = true\nreviewer_identities = [\"reviewer\"]\n";

/// The stage of the green PR when GitHub answers by substring from `routes`.
fn stage(routes: &[(&str, Result<String, String>)]) -> (Stage, String, Vec<String>) {
    let made = RefCell::new(Vec::new());
    let run_gh = |args: &[String]| -> Result<String, String> {
        let joined = args.join(" ");
        made.borrow_mut().push(joined.clone());
        routes
            .iter()
            .find(|(needle, _)| joined.contains(needle))
            .map_or_else(
                || Err(format!("unscripted gh call: {joined}")),
                |(_, answer)| answer.clone(),
            )
    };
    let observation = observation(green_pr());
    let policy = crate::head_approval::read_policy(&run_gh, REPO, "main");
    let (stage, reason) =
        armable_stage(&run_gh, &observation, &observation.prs[0], policy.as_ref());
    (stage, reason, made.into_inner())
}

fn approving_review() -> String {
    format!(
        r#"{{"login":"reviewer","type":"User","association":"MEMBER","at":"2026-10-26T10:00:00Z","state":"APPROVED","commit_id":"{HEAD}","body":"looks good"}}"#
    )
}

/// Approval is `head_approval`'s evaluation, never verdict-text matching:
/// an approving review of the head is ready; no approval needs review.
#[test]
fn approval_decides_ready_or_needs_review() {
    let ready = stage(&[
        ("isInMergeQueue", Ok(NEVER_ARMED.to_owned())),
        (
            "contents/.shipyard/config.toml",
            Ok(APPROVAL_CONFIG.to_owned()),
        ),
        ("/reviews", Ok(approving_review())),
        ("/comments", Ok(String::new())),
        ("--jq .user.login", Ok("someone-else\n".to_owned())),
    ]);
    assert_eq!(ready.0, Stage::Ready, "{ready:?}");
    assert!(
        ready.1.contains("approving review by reviewer"),
        "{ready:?}"
    );
    let unreviewed = stage(&[
        ("isInMergeQueue", Ok(NEVER_ARMED.to_owned())),
        (
            "contents/.shipyard/config.toml",
            Ok(APPROVAL_CONFIG.to_owned()),
        ),
        ("/reviews", Ok(String::new())),
        ("/comments", Ok(String::new())),
        ("--jq .user.login", Ok("someone-else\n".to_owned())),
        ("compare/", Err("gh: Not Found (HTTP 404)".to_owned())),
    ]);
    assert_eq!(unreviewed.0, Stage::NeedsReview, "{unreviewed:?}");
}

/// An approved head whose tiered check ran only its deferred lane is
/// tier-blocked, with the tier rule's own explanation.
#[test]
fn an_approved_deferred_head_is_tier_blocked() {
    let config = format!(
        "{APPROVAL_CONFIG}[queue]\ntiered_required_checks = [\"macos\"]\ntier_record_app = \"pulp-tier-records\"\n"
    );
    let summary = serde_json::json!({
        "schema": "shipyard-test-tier/v1", "check": "macos", "tier": "deferred",
        "run_id": 700, "run_attempt": 1, "full_lane_workflow": "build.yml"})
    .to_string();
    let listing = serde_json::json!([{"total_count": 2, "check_runs": [
        {"id": 9, "name": "macos", "status": "completed", "app": {"slug": "github-actions"}},
        {"id": 750, "name": "shipyard-test-tier/macos", "status": "completed",
         "app": {"slug": "pulp-tier-records"}, "output": {"summary": summary}}]}])
    .to_string();
    let (blocked, reason, _) = stage(&[
        ("isInMergeQueue", Ok(NEVER_ARMED.to_owned())),
        ("contents/.shipyard/config.toml", Ok(config)),
        ("/reviews", Ok(approving_review())),
        ("/comments", Ok(String::new())),
        ("--jq .user.login", Ok("someone-else\n".to_owned())),
        ("/check-runs?per_page=100", Ok(listing)),
        (
            "actions/jobs/9",
            Ok(r#"{"id":9,"run_id":700,"run_attempt":1}"#.to_owned()),
        ),
    ]);
    assert_eq!(blocked, Stage::TierBlocked, "{reason}");
    assert!(reason.contains("ran only the deferred tier"), "{reason}");
}

/// A head the queue ejected for failed checks, with no new head since, is
/// not ready, and its approval is never read.
#[test]
fn a_same_head_ejection_is_its_own_stage() {
    let ejected = r#"{"data":{"repository":{"pullRequest":{"number":42,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{"pageInfo":{"hasPreviousPage":false},"nodes":[{"__typename":"AddedToMergeQueueEvent","createdAt":"2026-09-01T00:00:00Z","actor":{"login":"a"}},{"__typename":"RemovedFromMergeQueueEvent","createdAt":"2026-09-02T00:00:00Z","reason":"failed_checks","actor":{"login":"a"}}]}}}}}"#;
    let (stage, _, made) = stage(&[
        ("isInMergeQueue", Ok(ejected.to_owned())),
        (
            "contents/.shipyard/config.toml",
            Ok(APPROVAL_CONFIG.to_owned()),
        ),
    ]);
    assert_eq!(stage, Stage::EjectedSameHead);
    assert!(
        !made.iter().any(|call| call.contains("/reviews")),
        "{made:?}"
    );
}

/// `--record` appends one sample per pass, with every merge since the
/// previous sample, and `metrics` reads them back.
#[test]
fn a_recorded_pass_appends_a_sample_that_metrics_read() {
    let temp = tempfile::tempdir().expect("temp");
    let searched = RefCell::new(Vec::new());
    let run_gh = |args: &[String]| -> Result<String, String> {
        let joined = args.join(" ");
        searched.borrow_mut().push(joined.clone());
        assert!(joined.contains("is:merged base:main"), "{joined}");
        Ok(
            r#"{"data":{"search":{"nodes":[{"number":41,"mergedAt":"2026-10-26T10:00:00Z"}]}}}"#
                .to_owned(),
        )
    };
    let rows = vec![super::StageRow {
        number: 42,
        head: HEAD.to_owned(),
        stage: Stage::Ready,
        reason: "approved".to_owned(),
        handoff: None,
    }];
    for _ in 0..2 {
        super::record_sample(&run_gh, REPO, "main", 0, &rows, temp.path()).expect("record");
    }
    let path = temp.path().join("train").join("owner__repo.jsonl");
    let samples = super::read_samples(&path);
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0].prs[&42].stage, Stage::Ready);
    assert_eq!(samples[0].merged[0].number, 41);
    // The second pass searches from the first sample's time.
    let second = &searched.borrow()[1];
    assert!(
        second.contains(&format!("merged:>={}", &samples[0].at[..19])),
        "{second}"
    );
    let measured = crate::merge_train::metrics(&samples);
    assert_eq!(measured.merges, 1);
    assert_eq!(measured.samples, 2);
}

/// The handoff status names one head, so a clean base merge leaves a
/// managed pull request `HandoffMissing`. The newest earlier handed-off head
/// carries to the current one exactly when the merge-only rule accepts the
/// commits since; a head with no earlier handoff, or one the rule refuses,
/// does not.
#[test]
fn a_handoff_carries_across_clean_base_merges_only() {
    const OLD: &str = "1111111111111111111111111111111111111111";
    const MID: &str = "2222222222222222222222222222222222222222";
    let routes = |handed_off: &'static str| {
        move |args: &[String]| -> Result<String, String> {
            let joined = args.join(" ");
            if joined.contains("pulls/42/commits") {
                return Ok(
                    serde_json::json!([{"sha": OLD}, {"sha": MID}, {"sha": HEAD}]).to_string(),
                );
            }
            if let Some(sha) = [OLD, MID]
                .iter()
                .find(|sha| joined.contains(&format!("commits/{sha}/status")))
            {
                let statuses = if *sha == handed_off {
                    serde_json::json!([{"context": "shipyard/steward-handoff", "state": "success"}])
                } else {
                    serde_json::json!([{"context": "ci", "state": "success"}])
                };
                return Ok(serde_json::json!({"statuses": statuses}).to_string());
            }
            Err(format!("unscripted gh call: {joined}"))
        }
    };
    let accepted = |_: &str, _: &str| Ok(());
    let refused = |_: &str, _: &str| Err("a commit since is not a merge".to_owned());
    // The newest handed-off head is the one tried, and it carries.
    let carried: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let recording = |from: &str, to: &str| {
        carried.borrow_mut().push(format!("{from}->{to}"));
        Ok(())
    };
    let run_gh = routes(OLD);
    assert_eq!(
        super::carried_handoff(&run_gh, REPO, 42, HEAD, &recording),
        Ok(Some(OLD.to_owned()))
    );
    assert_eq!(carried.borrow().as_slice(), [format!("{OLD}->{HEAD}")]);
    let run_gh = routes(MID);
    assert_eq!(
        super::carried_handoff(&run_gh, REPO, 42, HEAD, &accepted),
        Ok(Some(MID.to_owned()))
    );
    // The rule refusing the commits since: no carry, with the reason.
    assert_eq!(
        super::carried_handoff(&run_gh, REPO, 42, HEAD, &refused),
        Err("a commit since is not a merge".to_owned())
    );
    // No earlier head was ever handed off.
    let run_gh = routes("none");
    assert_eq!(
        super::carried_handoff(&run_gh, REPO, 42, HEAD, &accepted),
        Ok(None)
    );
}

/// `train status` never writes the carry log: a head whose approval carries
/// across a rebase (the rebase carry on, as on a host that switched it on) is
/// reported ready, and `carries.jsonl` stays byte-identical.
#[test]
fn a_report_of_a_carried_approval_writes_no_carry_log() {
    use crate::head_approval::tests::{History, by_default};
    let history = History::new(false);
    let head = history.rebase(|history| history.write("more.txt", b"more base\n"), |_| {});
    let guards = tempfile::tempdir().expect("guards");
    let log = guards.path().join("carries.jsonl");
    std::fs::write(&log, "{\"kind\":\"rebase\",\"earlier\":true}\n").expect("seed");
    let before = std::fs::read(&log).expect("log");
    let never_armed = NEVER_ARMED.replace(HEAD, &head);
    let run_gh = |args: &[String]| -> Result<String, String> {
        if args.join(" ").contains("isInMergeQueue") {
            return Ok(never_armed.clone());
        }
        history.gh(args)
    };
    let mut pr = green_pr();
    pr.fact.head_sha.clone_from(&head);
    pr.fact.author = None;
    pr.fact.number = 7;
    let mut observation = observation(pr);
    observation.repo = "danielraffel/tartci".to_owned();
    let mut policy = by_default("danielraffel/tartci");
    policy.rebase_carry = true;
    let (stage, reason) = crate::head_approval::with_guards_dir(guards.path(), || {
        crate::head_approval::with_host_review_default(true, || {
            armable_stage(&run_gh, &observation, &observation.prs[0], Ok(&policy))
        })
    });
    assert_eq!(stage, Stage::Ready, "{reason}");
    assert!(reason.contains("carried"), "{reason}");
    assert_eq!(std::fs::read(&log).expect("log"), before);
}
