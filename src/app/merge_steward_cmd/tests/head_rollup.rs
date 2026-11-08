//! The census reads current-head checks through batched rollups and kept
//! status histories, and must hydrate exactly what the per-head REST reads
//! hydrate.

use super::*;
use crate::app::merge_steward_cmd::head_rollup::{HEADS_PER_QUERY, StatusHistory};
use crate::app::merge_steward_cmd::observation::hydrate_census_check_identities_with_history;
use crate::merge_steward::{StewardDecision, StewardPolicy, classify_pr};

const ACTIONS_APP: u64 = 15_368;

struct Run {
    name: &'static str,
    id: u64,
    status: &'static str,
    conclusion: Option<&'static str>,
}

struct Status {
    id: u64,
    context: &'static str,
    state: &'static str,
    at: &'static str,
    /// The Actions bot, or an ordinary user.
    bot: bool,
}

struct Head {
    number: u64,
    runs: Vec<Run>,
    statuses: Vec<Status>,
}

fn sha(number: u64) -> String {
    format!("{number:040}")
}

fn run(name: &'static str, id: u64, status: &'static str, conclusion: Option<&'static str>) -> Run {
    Run {
        name,
        id,
        status,
        conclusion,
    }
}

fn status(id: u64, context: &'static str, state: &'static str, at: &'static str) -> Status {
    Status {
        id,
        context,
        state,
        at,
        bot: true,
    }
}

fn rest_runs(head: &Head) -> serde_json::Value {
    serde_json::json!({
        "total_count": head.runs.len(),
        "check_runs": head.runs.iter().map(|run| serde_json::json!({
            "id": run.id, "name": run.name, "status": run.status, "conclusion": run.conclusion,
            "details_url": format!("https://github.com/owner/repo/actions/runs/{}/job/{}", run.id / 10, run.id),
            "started_at": "2026-11-08T01:00:00Z",
            "completed_at": run.conclusion.map(|_| "2026-11-08T02:00:00Z"),
            "app": {"id": ACTIONS_APP}
        })).collect::<Vec<_>>()
    })
}

fn creator_rest(status: &Status) -> serde_json::Value {
    if status.bot {
        serde_json::json!({"login": "github-actions[bot]", "id": 41_898_282})
    } else {
        serde_json::json!({"login": "alice", "id": 7})
    }
}

fn rest_statuses(head: &Head) -> serde_json::Value {
    let mut rows = head.statuses.iter().collect::<Vec<_>>();
    rows.sort_by_key(|status| std::cmp::Reverse(status.id));
    serde_json::Value::Array(
        rows.into_iter()
            .map(|status| {
                serde_json::json!({
                    "id": status.id, "context": status.context, "state": status.state,
                    "target_url": format!("https://github.com/owner/repo/actions/runs/{}", status.id),
                    "created_at": status.at, "updated_at": status.at,
                    "creator": creator_rest(status), "description": "",
                })
            })
            .collect(),
    )
}

/// The rollup GitHub reports for the same head: each run, and the newest
/// status of each context.
fn rollup(head: &Head) -> serde_json::Value {
    let mut nodes = head
        .runs
        .iter()
        .map(|run| serde_json::json!({
            "__typename": "CheckRun", "name": run.name, "status": run.status.to_ascii_uppercase(),
            "conclusion": run.conclusion.map(str::to_ascii_uppercase), "databaseId": run.id,
            "detailsUrl": format!("https://github.com/owner/repo/actions/runs/{}/job/{}", run.id / 10, run.id),
            "startedAt": "2026-11-08T01:00:00Z",
            "completedAt": run.conclusion.map(|_| "2026-11-08T02:00:00Z"),
            "checkSuite": {"app": {"databaseId": ACTIONS_APP}}
        }))
        .collect::<Vec<_>>();
    let mut newest: BTreeMap<&str, &Status> = BTreeMap::new();
    for status in &head.statuses {
        if newest
            .get(status.context)
            .is_none_or(|seen| seen.id < status.id)
        {
            newest.insert(status.context, status);
        }
    }
    for status in newest.values() {
        let creator = if status.bot {
            serde_json::json!({"__typename": "Bot", "login": "github-actions", "databaseId": 41_898_282})
        } else {
            serde_json::json!({"__typename": "User", "login": "alice", "databaseId": 7})
        };
        nodes.push(serde_json::json!({
            "__typename": "StatusContext", "context": status.context,
            "state": status.state.to_ascii_uppercase(),
            "targetUrl": format!("https://github.com/owner/repo/actions/runs/{}", status.id),
            "createdAt": status.at, "creator": creator
        }));
    }
    let rollup = if nodes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({"contexts": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": nodes}})
    };
    serde_json::json!({"__typename": "Commit", "oid": sha(head.number), "statusCheckRollup": rollup})
}

/// A `gh` that answers the REST check-run and status reads and the rollup
/// query from the same per-head fixtures, and logs every call. Writing
/// `no-graphql` in `dir` makes the rollup unreadable: the REST census.
fn fixture_gh(temp: &tempfile::TempDir, heads: &[Head]) -> GitHubActions {
    let dir = temp.path().join("fixtures");
    fs::create_dir_all(&dir).expect("fixtures");
    write_heads(&dir, heads);
    fake_gh(
        temp,
        &format!(
            r#"dir='{dir}'
printf '%s\n' "$*" >> "$dir/calls"
head=''
for arg in "$@"; do
  case "$arg" in
    repos/*/commits/*) rest=${{arg#*commits/}}; head=${{rest%%/*}} ;;
  esac
done
case "$*" in
  *statusCheckRollup*)
    if [ -f "$dir/no-graphql" ]; then echo "HTTP 502" >&2; exit 1; fi
    out=''; sep=''
    for arg in "$@"; do
      case "$arg" in
        oid[0-9]*=*) key=${{arg%%=*}}; index=${{key#oid}}; oid=${{arg#*=}}
          out="$out$sep\"checkRollup$index\":$(cat "$dir/$oid.rollup")"; sep=',' ;;
      esac
    done
    printf '{{"data":{{"repository":{{%s}}}}}}' "$out" ;;
  *"/check-runs?per_page=100&page=1"*) cat "$dir/$head.runs" ;;
  *"/statuses?per_page=100&page=1"*) cat "$dir/$head.statuses" ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            dir = dir.display()
        ),
    )
}

fn write_heads(dir: &Path, heads: &[Head]) {
    for head in heads {
        let sha = sha(head.number);
        fs::write(dir.join(format!("{sha}.runs")), rest_runs(head).to_string()).expect("runs");
        fs::write(
            dir.join(format!("{sha}.statuses")),
            rest_statuses(head).to_string(),
        )
        .expect("statuses");
        fs::write(dir.join(format!("{sha}.rollup")), rollup(head).to_string()).expect("rollup");
    }
}

fn census_rows(heads: &[Head]) -> Vec<ObservedPr> {
    heads
        .iter()
        .map(|head| {
            parse_pr(
                &serde_json::json!({
                    "id": format!("PR_{}", head.number), "number": head.number, "isDraft": false,
                    "baseRefName": "main", "headRefOid": sha(head.number),
                    "headRefName": format!("feature-{}", head.number), "author": {"login": "alice"},
                    "mergeStateStatus": "CLEAN", "autoMergeRequest": null,
                    "labels": [{"name": MANAGED_LABEL}]
                }),
                &BTreeMap::new(),
            )
            .expect("row")
        })
        .collect()
}

fn required() -> Vec<RequiredCheck> {
    vec![
        RequiredCheck {
            context: "macos".to_owned(),
            app_id: Some(ACTIONS_APP),
        },
        RequiredCheck {
            context: "review-verdict".to_owned(),
            app_id: Some(ACTIONS_APP),
        },
        RequiredCheck {
            context: "legacy-ci".to_owned(),
            app_id: None,
        },
    ]
}

fn policy() -> StewardPolicy {
    StewardPolicy {
        merge_queue: true,
        native_auto_merge: true,
        approval_context: crate::merge_steward::DEFAULT_APPROVAL_CONTEXT.to_owned(),
        required_checks: required(),
        opt_out_label: "steward:skip".to_owned(),
        provenance_blocking_labels: Vec::new(),
        managed_label: Some(MANAGED_LABEL.to_owned()),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 0,
    }
}

/// Every head shape the census must read the same way.
fn fixture_heads() -> Vec<Head> {
    let handoff = |id| status(id, HANDOFF_CONTEXT, "success", "2026-11-08T00:00:00Z");
    let verdict = |id| status(id, "review-verdict", "success", "2026-11-08T03:00:00Z");
    // `legacy-ci` is a status-only required context, with a stale pending
    // entry under its success.
    let legacy = |id| {
        [
            status(id, "legacy-ci", "pending", "2026-11-08T00:30:00Z"),
            status(id + 1, "legacy-ci", "success", "2026-11-08T00:40:00Z"),
        ]
    };
    let green = |number, macos: Run| {
        let mut statuses = vec![handoff(number * 100), verdict(number * 100 + 1)];
        statuses.extend(legacy(number * 100 + 2));
        Head {
            number,
            runs: vec![
                macos,
                run("lint", number * 10 + 2, "completed", Some("cancelled")),
            ],
            statuses,
        }
    };
    let mut heads = vec![
        green(1, run("macos", 11, "completed", Some("success"))),
        green(2, run("macos", 21, "completed", Some("failure"))),
        green(3, run("macos", 31, "in_progress", None)),
    ];
    // Status-only context failed after succeeding: its history matters.
    let mut red_legacy = green(4, run("macos", 41, "completed", Some("success")));
    red_legacy
        .statuses
        .push(status(499, "legacy-ci", "failure", "2026-11-08T04:00:00Z"));
    heads.push(red_legacy);
    // The newest review-verdict status is a user's; the Actions one under it
    // is what the app-bound rule selects. Only the history carries it.
    let mut user_verdict = green(5, run("macos", 51, "completed", Some("success")));
    user_verdict.statuses.push(Status {
        id: 599,
        context: "review-verdict",
        state: "failure",
        at: "2026-11-08T05:00:00Z",
        bot: false,
    });
    heads.push(user_verdict);
    // The handoff succeeded once and then failed: the handoff check asks
    // whether any handoff status succeeded.
    let mut handoff_history = green(6, run("macos", 61, "completed", Some("success")));
    handoff_history.statuses.push(status(
        699,
        HANDOFF_CONTEXT,
        "failure",
        "2026-11-08T06:00:00Z",
    ));
    heads.push(handoff_history);
    // No status at all, and nothing at all.
    heads.push(Head {
        number: 7,
        runs: vec![run("macos", 71, "completed", Some("success"))],
        statuses: Vec::new(),
    });
    heads.push(Head {
        number: 8,
        runs: Vec::new(),
        statuses: Vec::new(),
    });
    heads
}

fn calls(temp: &tempfile::TempDir) -> Vec<String> {
    fs::read_to_string(temp.path().join("fixtures/calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn count(calls: &[String], needle: &str) -> usize {
    calls.iter().filter(|line| line.contains(needle)).count()
}

fn sorted_facts(prs: &[ObservedPr]) -> Vec<crate::merge_steward::StewardPullRequest> {
    prs.iter()
        .map(|pr| {
            let mut fact = pr.fact.clone();
            fact.checks.sort_by_key(|check| format!("{check:?}"));
            fact
        })
        .collect()
}

fn decisions(prs: &[ObservedPr]) -> Vec<StewardDecision> {
    prs.iter()
        .map(|pr| classify_pr(&pr.fact, &policy(), &BTreeMap::new()))
        .collect()
}

/// The census as it read before rollups: per-head REST reads only.
fn rest_census(heads: &[Head]) -> Vec<ObservedPr> {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fixture_gh(&temp, heads);
    fs::write(temp.path().join("fixtures/no-graphql"), "").expect("disable rollup");
    let mut prs = census_rows(heads);
    hydrate_census_check_identities_with_history(
        &actions,
        "owner/repo",
        &required(),
        &mut prs,
        None,
    )
    .expect("REST census");
    prs
}

#[cfg(unix)]
#[test]
fn rollup_census_hydrates_the_same_checks_and_decisions_as_the_rest_census() {
    let heads = fixture_heads();
    let rest = rest_census(&heads);

    let temp = tempfile::tempdir().expect("temp");
    let actions = fixture_gh(&temp, &heads);
    let mut store = StatusHistory::load(&temp.path().join("history.json"), "owner/repo");
    let mut cold = census_rows(&heads);
    hydrate_census_check_identities_with_history(
        &actions,
        "owner/repo",
        &required(),
        &mut cold,
        Some(&mut store),
    )
    .expect("cold census");
    let cold_calls = calls(&temp);
    assert_eq!(
        count(&cold_calls, "/check-runs"),
        0,
        "check runs come from the rollup"
    );

    let mut store = StatusHistory::load(&temp.path().join("history.json"), "owner/repo");
    let mut warm = census_rows(&heads);
    hydrate_census_check_identities_with_history(
        &actions,
        "owner/repo",
        &required(),
        &mut warm,
        Some(&mut store),
    )
    .expect("warm census");
    let warm_calls = calls(&temp)[cold_calls.len()..].to_vec();
    assert_eq!(
        count(&warm_calls, "/statuses"),
        0,
        "unchanged histories are reused: {warm_calls:?}"
    );

    for (label, prs) in [("cold", &cold), ("warm", &warm)] {
        assert_eq!(sorted_facts(prs), sorted_facts(&rest), "{label} facts");
        assert_eq!(decisions(prs), decisions(&rest), "{label} decisions");
        assert!(prs.iter().all(|pr| !pr.check_rollup_maybe_truncated));
    }
    // The fixture covers green, failed and pending required checks and a
    // status-only required context.
    let decisions = decisions(&rest);
    assert_eq!(decisions[0], StewardDecision::ArmMergeQueue);
    assert!(
        matches!(&decisions[1], StewardDecision::RequiredFailed { contexts } if contexts == &["macos (app_id=15368)".to_owned()]),
        "{:?}",
        decisions[1]
    );
    assert!(matches!(
        decisions[2],
        StewardDecision::WaitingRequired { .. }
    ));
    assert!(
        matches!(&decisions[3], StewardDecision::RequiredFailed { contexts } if contexts == &["legacy-ci".to_owned()]),
        "{:?}",
        decisions[3]
    );
    assert_eq!(
        decisions[4],
        StewardDecision::ArmMergeQueue,
        "older app status"
    );
    assert_ne!(
        decisions[5],
        StewardDecision::HandoffMissing,
        "a handoff once succeeded"
    );
    // Those two need the history: with only the newest status per context,
    // which is all a rollup carries, they would decide differently.
    let latest_only = rest
        .iter()
        .cloned()
        .map(|mut pr| {
            let all = pr.fact.checks.clone();
            pr.fact.checks.retain(|check| {
                check.source != crate::merge_steward::StewardCheckSource::StatusContext
                    || !all.iter().any(|other| {
                        other.source == check.source
                            && other.name == check.name
                            && other.check_id > check.check_id
                    })
            });
            pr
        })
        .collect::<Vec<_>>();
    let without_history = self::decisions(&latest_only);
    assert_ne!(without_history[4], decisions[4]);
    assert_eq!(without_history[5], StewardDecision::HandoffMissing);
}

#[cfg(unix)]
#[test]
fn a_status_posted_since_the_last_pass_is_read_again() {
    let mut heads = fixture_heads();
    let temp = tempfile::tempdir().expect("temp");
    let actions = fixture_gh(&temp, &heads);
    let history = temp.path().join("history.json");
    let mut store = StatusHistory::load(&history, "owner/repo");
    let mut prs = census_rows(&heads);
    hydrate_census_check_identities_with_history(
        &actions,
        "owner/repo",
        &required(),
        &mut prs,
        Some(&mut store),
    )
    .expect("first census");
    let before = calls(&temp).len();

    // A new verdict lands on head 1 and turns it red.
    heads[0].statuses.push(status(
        9_001,
        "review-verdict",
        "failure",
        "2026-11-08T09:00:00Z",
    ));
    write_heads(&temp.path().join("fixtures"), &heads);
    let mut store = StatusHistory::load(&history, "owner/repo");
    let mut prs = census_rows(&heads);
    hydrate_census_check_identities_with_history(
        &actions,
        "owner/repo",
        &required(),
        &mut prs,
        Some(&mut store),
    )
    .expect("second census");
    let second = calls(&temp)[before..].to_vec();
    assert_eq!(count(&second, "/statuses"), 1, "{second:?}");
    assert!(
        second
            .iter()
            .any(|line| line.contains(&format!("commits/{}/statuses", sha(1))))
    );
    assert_eq!(sorted_facts(&prs), sorted_facts(&rest_census(&heads)));
    assert!(matches!(
        decisions(&prs)[0],
        StewardDecision::RequiredFailed { .. }
    ));
}

#[cfg(unix)]
#[test]
fn rollup_census_cuts_check_reads_per_pull_request() {
    const PULL_REQUESTS: usize = 24;
    let heads = (1..=PULL_REQUESTS as u64)
        .map(|number| Head {
            number,
            runs: vec![
                run("macos", number * 10 + 1, "completed", Some("success")),
                run("lint", number * 10 + 2, "completed", Some("success")),
            ],
            statuses: vec![
                status(
                    number * 100,
                    "review-verdict",
                    "pending",
                    "2026-11-08T01:00:00Z",
                ),
                status(
                    number * 100 + 1,
                    "review-verdict",
                    "success",
                    "2026-11-08T02:00:00Z",
                ),
                status(
                    number * 100 + 2,
                    "legacy-ci",
                    "success",
                    "2026-11-08T02:00:00Z",
                ),
            ],
        })
        .collect::<Vec<_>>();
    let census = |store: Option<&mut StatusHistory>, disable_rollup: bool| {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fixture_gh(&temp, &heads);
        if disable_rollup {
            fs::write(temp.path().join("fixtures/no-graphql"), "").expect("disable");
        }
        let mut prs = census_rows(&heads);
        hydrate_census_check_identities_with_history(
            &actions,
            "owner/repo",
            &required(),
            &mut prs,
            store,
        )
        .expect("census");
        let log = calls(&temp);
        (
            count(&log, "commits/"),
            count(&log, "statusCheckRollup"),
            log.len(),
        )
    };
    // Before: two REST reads per head. (The disabled rollup is still tried
    // once per batch; only the REST reads are the old path's cost.)
    let (before_rest, _, _) = census(None, true);
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("history.json");
    let (cold_rest, cold_graphql, cold_total) =
        census(Some(&mut StatusHistory::load(&path, "owner/repo")), false);
    let (warm_rest, warm_graphql, warm_total) =
        census(Some(&mut StatusHistory::load(&path, "owner/repo")), false);
    let batches = PULL_REQUESTS.div_ceil(HEADS_PER_QUERY);
    eprintln!(
        "{PULL_REQUESTS} PRs: before {before_rest} REST; cold {cold_rest} REST + {cold_graphql} GraphQL; warm {warm_rest} REST + {warm_graphql} GraphQL"
    );
    assert_eq!(before_rest, 2 * PULL_REQUESTS);
    assert_eq!((cold_rest, cold_graphql), (PULL_REQUESTS, batches));
    assert_eq!((warm_rest, warm_graphql), (0, batches));
    assert_eq!(cold_total, cold_rest + cold_graphql);
    assert!(
        warm_total * 3 <= before_rest,
        "a steady-state census costs at most a third of the REST census"
    );
}

#[cfg(unix)]
#[test]
fn a_rollup_past_one_page_is_followed_and_a_repeated_cursor_is_refused() {
    let page = |id: u64, next: Option<&str>| {
        serde_json::json!({"data": {"repository": {"checkRollup0": {
            "__typename": "Commit", "oid": sha(1),
            "statusCheckRollup": {"contexts": {
                "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next},
                "nodes": [{"__typename": "CheckRun", "name": format!("job-{id}"), "status": "COMPLETED",
                           "conclusion": "SUCCESS", "databaseId": id, "detailsUrl": null,
                           "startedAt": null, "completedAt": "2026-11-08T00:00:00Z",
                           "checkSuite": {"app": {"databaseId": ACTIONS_APP}}}]}}}}}})
        .to_string()
    };
    for (second, expect_ok) in [(page(2, None), true), (page(2, Some("CURSOR")), false)] {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(
            &temp,
            &format!(
                r#"case "$*" in
  *"after0=CURSOR"*) printf '%s' '{second}' ;;
  *statusCheckRollup*) printf '%s' '{first}' ;;
  *) exit 2 ;;
esac
"#,
                first = page(1, Some("CURSOR")),
            ),
        );
        let read = crate::app::merge_steward_cmd::head_rollup::read_rollups(
            &actions,
            "owner/repo",
            &[sha(1)],
            None,
        );
        if expect_ok {
            let rollups = read.expect("two pages");
            let names = rollups[&sha(1)]
                .check_runs
                .iter()
                .map(|check| check.name.clone())
                .collect::<Vec<_>>();
            assert_eq!(names, ["job-1", "job-2"]);
        } else {
            assert!(
                read.expect_err("repeated cursor")
                    .contains("repeated a cursor")
            );
        }
    }
}

#[test]
fn each_repository_keeps_its_own_status_history_beside_the_ledger() {
    let ledger = Path::new("/state/merge-steward.json");
    let pulp = status_history_path(ledger, "Generous-Corp/pulp");
    assert_eq!(
        pulp,
        Path::new("/state/merge-steward.status-history.generous-corp~pulp.json")
    );
    assert_ne!(pulp, status_history_path(ledger, "Generous-Corp/vellum"));
}
