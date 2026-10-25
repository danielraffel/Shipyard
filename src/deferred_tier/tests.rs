use std::cell::RefCell;
use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{
    Blocker, DeferredCheck, TierPolicy, checks_scope, deferred_tier_checks, explain_all,
    tier_policy_from_config,
};

const REPO: &str = "owner/repo";
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TIERED: &str =
    "[queue]\ntiered_required_checks = [\"macos\"]\ntier_record_app = \"pulp-tier-records\"\n";

fn listing_path() -> String {
    format!("repos/{REPO}/commits/{HEAD}/check-runs?per_page=100")
}

fn config_path() -> String {
    format!("repos/{REPO}/contents/.shipyard/config.toml?ref=main")
}

/// A tier record check run, as the base's named record App creates it.
fn record(tier: &str, run_id: u64, run_attempt: u64) -> Value {
    json!({
        "id": 50 + run_id, "name": "shipyard-test-tier/macos", "status": "completed",
        "conclusion": "neutral", "app": {"slug": "pulp-tier-records"},
        "output": {"title": tier, "summary": json!({
            "schema": "shipyard-test-tier/v1", "check": "macos", "tier": tier,
            "run_id": run_id, "run_attempt": run_attempt, "selector": "pr-fast+pr-guard",
            "trigger": "pull_request", "actor": "author", "full_lane_workflow": "build.yml", "full_suite_runs_in": "merge_group"}).to_string()}
    })
}

/// The `macos` job: an older failed run superseded by run 9, the job of
/// workflow run 700 attempt 1.
fn macos(status: &str) -> Vec<Value> {
    vec![
        json!({"id": 5, "name": "macos", "status": "completed", "conclusion": "failure",
               "app": {"slug": "github-actions"}}),
        json!({"id": 9, "name": "macos", "status": status, "conclusion": "success",
               "app": {"slug": "github-actions"},
               // Annotations are never read: code under test can print them.
               "output": {"annotations_count": 3}}),
    ]
}

/// The "Set up job" lines a runner prints, with the job's `Checks` scope.
fn job_log(checks: &str) -> String {
    format!(
        "2026-10-24T10:00:00.0000000Z Current runner version: '2.329.0'\n\
         2026-10-24T10:00:00.1000000Z ##[group]GITHUB_TOKEN Permissions\n\
         2026-10-24T10:00:00.1000000Z Actions: read\n\
         2026-10-24T10:00:00.1000000Z Checks: {checks}\n\
         2026-10-24T10:00:00.1000000Z Contents: read\n\
         2026-10-24T10:00:00.1000000Z Metadata: read\n\
         2026-10-24T10:00:00.1000000Z ##[endgroup]\n\
         2026-10-24T10:00:00.2000000Z Secret source: Actions\n\
         2026-10-24T10:05:00.0000000Z ##[notice]Checks: write\n"
    )
}

fn log_path() -> String {
    format!("repos/{REPO}/actions/jobs/9/logs")
}

fn answers(runs: &[Value]) -> BTreeMap<String, Result<String, String>> {
    let total = runs.len();
    BTreeMap::from([
        (config_path(), Ok(TIERED.to_owned())),
        (
            listing_path(),
            Ok(json!([{"total_count": total, "check_runs": runs}]).to_string()),
        ),
        (
            format!("repos/{REPO}/actions/jobs/9"),
            Ok(json!({"id": 9, "run_id": 700, "run_attempt": 1}).to_string()),
        ),
        (log_path(), Ok(job_log("read"))),
    ])
}

fn with(mut runs: Vec<Value>, extra: Vec<Value>) -> Vec<Value> {
    runs.extend(extra);
    runs
}

fn read(
    answers: &BTreeMap<String, Result<String, String>>,
) -> (Result<Vec<DeferredCheck>, String>, Vec<String>) {
    let calls = RefCell::new(Vec::new());
    let run_gh = |args: &[String]| -> Result<String, String> {
        let path = args.last().cloned().unwrap_or_default();
        calls.borrow_mut().push(path.clone());
        answers
            .get(&path)
            .cloned()
            .unwrap_or_else(|| panic!("unexpected gh call {args:?}"))
    };
    let result = deferred_tier_checks(&run_gh, REPO, "main", HEAD);
    (result, calls.into_inner())
}

fn blockers(answers: &BTreeMap<String, Result<String, String>>) -> Vec<Blocker> {
    read(answers)
        .0
        .expect("readable")
        .into_iter()
        .map(|check| check.blocker)
        .collect()
}

#[test]
fn a_deferred_record_refuses_and_names_the_label() {
    let checks = read(&answers(&with(
        macos("completed"),
        vec![record("deferred", 700, 1)],
    )))
    .0
    .expect("readable");
    assert!(matches!(checks[0].blocker, Blocker::Deferred { .. }));
    assert_eq!(
        checks[0].fix(),
        "dispatch `build.yml` on head aaaaaaaaaaaa as the fleet App and wait for the full lane"
    );
    let detail = explain_all(&checks).expect("a refusal");
    assert!(
        detail.contains(
            "required check `macos` on aaaaaaaaaaaa ran only the deferred tier \
             (selector pr-fast+pr-guard; full suite runs in merge_group)"
        ),
        "{detail}"
    );
}

/// Pulp's build.yml no longer runs on `labeled`: the full lane is a
/// fleet-App dispatch of the workflow on the head. A record naming no
/// workflow still says to dispatch, and a stray `label` is never used.
#[test]
fn the_fix_dispatches_the_full_lane_and_never_names_a_label() {
    let mut deferred = record("deferred", 700, 1);
    let mut summary: Value =
        serde_json::from_str(deferred["output"]["summary"].as_str().expect("summary"))
            .expect("json");
    summary
        .as_object_mut()
        .expect("object")
        .remove("full_lane_workflow");
    summary["label"] = json!("macos-ci");
    deferred["output"]["summary"] = json!(summary.to_string());
    let checks = read(&answers(&with(macos("completed"), vec![deferred])))
        .0
        .expect("readable");
    assert_eq!(
        checks[0].fix(),
        "dispatch the workflow of `macos` on head aaaaaaaaaaaa as the fleet App and wait for \
         the full lane"
    );
    assert!(!checks[0].explain().contains("macos-ci"));
}

#[test]
fn a_fast_record_bound_to_the_run_allows() {
    let answers = answers(&with(macos("completed"), vec![record("fast", 700, 1)]));
    assert_eq!(blockers(&answers), Vec::new());
}

/// A record that cannot be bound to this attempt never speaks for it, and no
/// record at all refuses: a missing marker is not a full tier.
#[test]
fn a_missing_or_unbound_record_refuses() {
    for extra in [
        vec![],
        vec![record("fast", 700, 2)],
        vec![record("fast", 699, 1)],
        vec![{
            let mut forged = record("fast", 700, 1);
            forged["app"]["slug"] = json!("some-app");
            forged
        }],
        vec![{
            let mut malformed = record("fast", 700, 1);
            malformed["output"]["summary"] = json!("tier: fast");
            malformed
        }],
    ] {
        assert_eq!(
            blockers(&answers(&with(macos("completed"), extra.clone()))),
            vec![Blocker::MissingRecord {
                run_id: 700,
                run_attempt: 1
            }],
            "{extra:?}"
        );
    }
}

/// A pull request job acts as `github-actions` and may hold `checks: write`,
/// so it can PATCH any record that app created, from `deferred` to `full`,
/// leaving nothing on the check run to show it. Such a record never counts,
/// alone or beside the real one.
#[test]
fn a_record_a_pull_request_job_could_rewrite_never_counts() {
    let patched = |tier: &str| {
        let mut record = record(tier, 700, 1);
        record["app"]["slug"] = json!("github-actions");
        record
    };
    assert_eq!(
        blockers(&answers(&with(macos("completed"), vec![patched("full")]))),
        vec![Blocker::MissingRecord {
            run_id: 700,
            run_attempt: 1
        }]
    );
    let found = blockers(&answers(&with(
        macos("completed"),
        vec![record("deferred", 700, 1), patched("full")],
    )));
    assert!(matches!(found[..], [Blocker::Deferred { .. }]), "{found:?}");
}

/// A later record cannot widen a deferred one: forging only refuses.
#[test]
fn any_deferred_record_for_the_run_refuses() {
    for records in [
        vec![record("deferred", 700, 1), record("full", 700, 1)],
        vec![record("full", 700, 1), record("deferred", 700, 1)],
    ] {
        let found = blockers(&answers(&with(macos("completed"), records)));
        assert!(matches!(found[..], [Blocker::Deferred { .. }]), "{found:?}");
    }
}

#[test]
fn a_check_that_has_not_completed_or_not_run_refuses() {
    let pending = blockers(&answers(&macos("in_progress")));
    assert_eq!(
        pending,
        vec![Blocker::Pending {
            status: "in_progress".to_owned()
        }]
    );
    let missing = blockers(&answers(&[]));
    assert_eq!(
        missing,
        vec![Blocker::Pending {
            status: "missing".to_owned()
        }]
    );
    assert!(
        DeferredCheck {
            check: "macos".to_owned(),
            head: HEAD.to_owned(),
            blocker: missing[0].clone(),
        }
        .explain()
        .contains("has not run on aaaaaaaaaaaa")
    );
}

#[test]
fn a_repository_that_lists_no_tiered_check_reads_only_its_config() {
    for config in [
        Err("gh: Not Found (HTTP 404)".to_owned()),
        Ok("[queue.environment_requeue]\nenabled = true\n".to_owned()),
    ] {
        let mut answers = answers(&with(macos("completed"), vec![record("deferred", 700, 1)]));
        answers.insert(config_path(), config);
        let (result, calls) = read(&answers);
        assert_eq!(result.expect("readable"), Vec::new());
        assert_eq!(calls, vec![config_path()]);
    }
}

#[test]
fn an_unreadable_config_listing_or_job_fails_closed() {
    let mut bad_config = answers(&macos("completed"));
    bad_config.insert(config_path(), Err("HTTP 502".to_owned()));
    assert!(read(&bad_config).0.is_err());
    let mut bad_key = answers(&macos("completed"));
    bad_key.insert(
        config_path(),
        Ok("[queue]\ntiered_required_checks = \"macos\"\n".to_owned()),
    );
    assert!(read(&bad_key).0.is_err());
    for listing in [
        Err("gh: HTTP 502".to_owned()),
        Ok(json!([{"total_count": 3, "check_runs": []}]).to_string()),
        Ok("{}".to_owned()),
    ] {
        let mut answers = answers(&macos("completed"));
        answers.insert(listing_path(), listing.clone());
        assert!(read(&answers).0.is_err(), "{listing:?}");
    }
    let mut no_job = answers(&with(macos("completed"), vec![record("fast", 700, 1)]));
    no_job.insert(
        format!("repos/{REPO}/actions/jobs/9"),
        Err("HTTP 404".to_owned()),
    );
    assert!(matches!(
        blockers(&no_job)[..],
        [Blocker::Unreadable { .. }]
    ));
}

#[test]
fn a_tiered_check_that_is_not_an_actions_job_refuses() {
    let mut runs = macos("completed");
    runs[1]["app"]["slug"] = json!("buildkite");
    assert!(matches!(
        blockers(&answers(&with(runs, vec![record("fast", 700, 1)])))[..],
        [Blocker::Unreadable { .. }]
    ));
}

#[test]
fn every_page_of_check_runs_is_read() {
    let mut answers = answers(&[]);
    let runs = with(macos("completed"), vec![record("deferred", 700, 1)]);
    answers.insert(
        listing_path(),
        Ok(json!([
            {"total_count": 3, "check_runs": runs[..1].to_vec()},
            {"total_count": 3, "check_runs": runs[1..].to_vec()},
        ])
        .to_string()),
    );
    assert!(matches!(blockers(&answers)[..], [Blocker::Deferred { .. }]));
}

#[test]
fn the_config_names_checks_and_the_one_app_whose_records_count() {
    assert_eq!(
        tier_policy_from_config(TIERED),
        Ok(TierPolicy {
            checks: vec!["macos".to_owned()],
            record_app: "pulp-tier-records".to_owned(),
        })
    );
    assert_eq!(tier_policy_from_config(""), Ok(TierPolicy::default()));
    for bad in [
        "[queue]\ntiered_required_checks = [1]\ntier_record_app = \"pulp-tier-records\"\n",
        // Checks without an app, or with an app pull request code or agents
        // can write as, refuse every landing.
        "[queue]\ntiered_required_checks = [\"macos\"]\n",
        "[queue]\ntiered_required_checks = [\"macos\"]\ntier_record_app = \"github-actions\"\n",
        "[queue]\ntiered_required_checks = [\"macos\"]\ntier_record_app = \"shipyard-local\"\n",
    ] {
        assert!(tier_policy_from_config(bad).is_err(), "{bad}");
    }
}

/// A tiered job whose token could write check runs could have rewritten its
/// own `deferred` record to `full`, with nothing on the record to show it, so
/// a full record from such a job never allows.
#[test]
fn a_full_record_from_a_job_that_could_rewrite_it_refuses() {
    let mut answers = answers(&with(macos("completed"), vec![record("full", 700, 1)]));
    answers.insert(log_path(), Ok(job_log("write")));
    assert_eq!(
        blockers(&answers),
        vec![Blocker::ChecksWritable {
            level: "write".to_owned()
        }]
    );
    // Control on the same fixture: the same record from a read-only job allows.
    answers.insert(log_path(), Ok(job_log("read")));
    assert_eq!(blockers(&answers), Vec::new());
}

/// A refusal needs no log: only a head the records would allow pays for it.
#[test]
fn the_job_log_is_read_only_when_the_records_would_allow() {
    let mut answers = answers(&with(macos("completed"), vec![record("deferred", 700, 1)]));
    answers.insert(log_path(), Err("never read".to_owned()));
    let (result, calls) = read(&answers);
    assert!(matches!(
        result.expect("readable")[0].blocker,
        Blocker::Deferred { .. }
    ));
    assert!(!calls.contains(&log_path()), "{calls:?}");
}

#[test]
fn an_unreadable_or_unparseable_token_header_refuses() {
    for log in [
        Err("HTTP 410: logs expired".to_owned()),
        Ok("2026-10-24T10:00:00.0000000Z Secret source: Actions\n".to_owned()),
        Ok(job_log("read").replace("##[endgroup]", "Secret source: Actions")),
        Ok(job_log("read").replace("Contents: read", "Contents read")),
        Ok(job_log("read").replace(
            "Contents: read",
            "Checks: read\n2026-10-24T10:00:00Z Checks: write",
        )),
    ] {
        let mut answers = answers(&with(macos("completed"), vec![record("full", 700, 1)]));
        answers.insert(log_path(), log.clone());
        assert!(
            matches!(blockers(&answers)[..], [Blocker::Unreadable { .. }]),
            "{log:?}"
        );
    }
}

#[test]
fn the_checks_scope_is_read_from_the_header_group_only() {
    assert_eq!(checks_scope(&job_log("read")), Ok("read".to_owned()));
    assert_eq!(checks_scope(&job_log("none")), Ok("none".to_owned()));
    assert_eq!(checks_scope(&job_log("write")), Ok("write".to_owned()));
    // Not listed is no access. The `##[notice]Checks: write` printed later
    // in the log is outside the group and never read.
    let unlisted = job_log("read").replace("2026-10-24T10:00:00.1000000Z Checks: read\n", "");
    assert!(!unlisted.contains("Checks: read"), "{unlisted}");
    assert_eq!(checks_scope(&unlisted), Ok("none".to_owned()));
}
