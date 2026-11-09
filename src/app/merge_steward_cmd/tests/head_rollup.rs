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
    app: u64,
}

struct Status {
    id: u64,
    context: &'static str,
    state: &'static str,
    at: &'static str,
    /// The Actions bot, or an ordinary user.
    bot: bool,
}

#[derive(Default)]
struct Head {
    number: u64,
    runs: Vec<Run>,
    statuses: Vec<Status>,
    /// Contexts the rollup reports as `EXPECTED`; REST has no such row.
    expected: Vec<&'static str>,
    /// Serve the check runs on a first rollup page and the statuses on a
    /// second.
    paged: bool,
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
        app: ACTIONS_APP,
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
            "app": {"id": run.app}
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

fn rollup_run_nodes(head: &Head) -> Vec<serde_json::Value> {
    head.runs
        .iter()
        .map(|run| serde_json::json!({
            "__typename": "CheckRun", "name": run.name, "status": run.status.to_ascii_uppercase(),
            "conclusion": run.conclusion.map(str::to_ascii_uppercase), "databaseId": run.id,
            "detailsUrl": format!("https://github.com/owner/repo/actions/runs/{}/job/{}", run.id / 10, run.id),
            "startedAt": "2026-11-08T01:00:00Z",
            "completedAt": run.conclusion.map(|_| "2026-11-08T02:00:00Z"),
            "checkSuite": {"app": {"databaseId": run.app}}
        }))
        .collect()
}

/// The newest status of each context, and each `EXPECTED` context.
fn rollup_status_nodes(head: &Head) -> Vec<serde_json::Value> {
    let mut newest: BTreeMap<&str, &Status> = BTreeMap::new();
    for status in &head.statuses {
        if newest
            .get(status.context)
            .is_none_or(|seen| seen.id < status.id)
        {
            newest.insert(status.context, status);
        }
    }
    let mut nodes = newest
        .values()
        .map(|status| {
            let creator = if status.bot {
                serde_json::json!({"__typename": "Bot", "login": "github-actions", "databaseId": 41_898_282})
            } else {
                serde_json::json!({"__typename": "User", "login": "alice", "databaseId": 7})
            };
            serde_json::json!({
                "__typename": "StatusContext", "context": status.context,
                "state": status.state.to_ascii_uppercase(),
                "targetUrl": format!("https://github.com/owner/repo/actions/runs/{}", status.id),
                "createdAt": status.at, "creator": creator
            })
        })
        .collect::<Vec<_>>();
    nodes.extend(head.expected.iter().map(|context| {
        serde_json::json!({
            "__typename": "StatusContext", "context": context, "state": "EXPECTED",
            "targetUrl": null, "createdAt": null, "creator": null
        })
    }));
    nodes
}

fn rollup_page(head: &Head, nodes: &[serde_json::Value], next: Option<&str>) -> serde_json::Value {
    let rollup = if nodes.is_empty() && next.is_none() && !head.paged {
        serde_json::Value::Null
    } else {
        serde_json::json!({"contexts": {
            "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}, "nodes": nodes}})
    };
    serde_json::json!({"__typename": "Commit", "oid": sha(head.number), "statusCheckRollup": rollup})
}

/// The rollup GitHub reports for the same head: each run, and the newest
/// status of each context; for a paged head, the statuses on page two.
fn rollup_pages(head: &Head) -> (serde_json::Value, Option<serde_json::Value>) {
    if head.paged {
        (
            rollup_page(head, &rollup_run_nodes(head), Some("PAGE2")),
            Some(rollup_page(head, &rollup_status_nodes(head), None)),
        )
    } else {
        let mut nodes = rollup_run_nodes(head);
        nodes.extend(rollup_status_nodes(head));
        (rollup_page(head, &nodes, None), None)
    }
}

/// A `gh` that answers the REST check-run and status reads and the rollup
/// query from the same per-head fixtures, and logs every call. A marker file
/// in the fixture directory changes the rollup's answer: `no-graphql` (a
/// gateway failure, so the REST census), `rate-limited`, `graphql-errors`
/// (errors beside partial data) or `null-node`.
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
    if [ -f "$dir/rate-limited" ]; then echo "gh: API rate limit exceeded for installation ID 1 (HTTP 403)" >&2; exit 1; fi
    afters='|'
    for arg in "$@"; do
      case "$arg" in after[0-9]*=*) key=${{arg%%=*}}; afters="$afters${{key#after}}|" ;; esac
    done
    out=''; sep=''
    for arg in "$@"; do
      case "$arg" in
        oid[0-9]*=*) key=${{arg%%=*}}; index=${{key#oid}}; oid=${{arg#*=}}
          file="$dir/$oid.rollup"
          case "$afters" in *"|$index|"*) file="$dir/$oid.rollup2" ;; esac
          if [ -f "$dir/null-node" ]; then node=null; else node=$(cat "$file"); fi
          out="$out$sep\"checkRollup$index\":$node"; sep=',' ;;
      esac
    done
    if [ -f "$dir/graphql-errors" ]; then
      printf '{{"data":{{"repository":{{%s}}}},"errors":[{{"type":"FORBIDDEN","path":["repository","checkRollup0"],"message":"Resource not accessible by integration"}}]}}' "$out"
    else
      printf '{{"data":{{"repository":{{%s}}}}}}' "$out"
    fi ;;
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
        let (first, second) = rollup_pages(head);
        fs::write(dir.join(format!("{sha}.rollup")), first.to_string()).expect("rollup");
        if let Some(second) = second {
            fs::write(dir.join(format!("{sha}.rollup2")), second.to_string()).expect("page 2");
        }
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
        ejection_guards: crate::merge_steward::EjectionGuards::default(),
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
            ..Head::default()
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
        ..Head::default()
    });
    heads.push(Head {
        number: 8,
        runs: Vec::new(),
        statuses: Vec::new(),
        ..Head::default()
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
            ..Head::default()
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

/// Heads whose shape the main fixture does not reach.
fn edge_heads() -> Vec<Head> {
    let handoff = |id| status(id, HANDOFF_CONTEXT, "success", "2026-11-08T00:00:00Z");
    let passing = |number: u64| {
        vec![
            handoff(number * 100),
            status(
                number * 100 + 1,
                "review-verdict",
                "success",
                "2026-11-08T03:00:00Z",
            ),
            status(
                number * 100 + 2,
                "legacy-ci",
                "success",
                "2026-11-08T03:00:00Z",
            ),
        ]
    };
    let with_macos = |number: u64, runs: Vec<Run>| Head {
        number,
        runs,
        statuses: passing(number),
        ..Head::default()
    };
    let mut heads = vec![
        // A rerun: the failed first attempt and the passing second.
        with_macos(
            11,
            vec![
                run("macos", 111, "completed", Some("failure")),
                run("macos", 112, "completed", Some("success")),
            ],
        ),
        // The same name from another app: the bound rule ignores it.
        with_macos(
            12,
            vec![
                run("macos", 121, "completed", Some("success")),
                Run {
                    app: 99,
                    ..run("macos", 129, "completed", Some("failure"))
                },
            ],
        ),
    ];
    for (offset, conclusion) in [
        "neutral",
        "skipped",
        "stale",
        "action_required",
        "startup_failure",
    ]
    .into_iter()
    .enumerate()
    {
        let number = 13 + offset as u64;
        heads.push(with_macos(
            number,
            vec![run("macos", number * 10 + 1, "completed", Some(conclusion))],
        ));
    }
    // An `EXPECTED` context the rollup synthesizes and REST never lists.
    let mut expected = with_macos(18, vec![run("macos", 181, "completed", Some("success"))]);
    expected.expected = vec!["deploy-preview"];
    heads.push(expected);
    // The statuses arrive on the rollup's second page.
    let mut paged = with_macos(19, vec![run("macos", 191, "completed", Some("success"))]);
    paged.paged = true;
    heads.push(paged);
    heads
}

fn rollup_census(
    temp: &tempfile::TempDir,
    actions: &GitHubActions,
    heads: &[Head],
    history: &Path,
) -> Result<Vec<ObservedPr>, String> {
    let _ = temp;
    let mut store = StatusHistory::load(history, "owner/repo");
    let mut prs = census_rows(heads);
    hydrate_census_check_identities_with_history(
        actions,
        "owner/repo",
        &required(),
        &mut prs,
        Some(&mut store),
    )?;
    Ok(prs)
}

#[cfg(unix)]
#[test]
fn rollup_census_matches_rest_on_reruns_apps_conclusions_expected_and_paged_statuses() {
    let heads = edge_heads();
    let rest = rest_census(&heads);
    let temp = tempfile::tempdir().expect("temp");
    let actions = fixture_gh(&temp, &heads);
    let history = temp.path().join("history.json");
    let cold = rollup_census(&temp, &actions, &heads, &history).expect("cold");
    let cold_calls = calls(&temp);
    let warm = rollup_census(&temp, &actions, &heads, &history).expect("warm");
    let warm_calls = calls(&temp)[cold_calls.len()..].to_vec();
    for (label, prs) in [("cold", &cold), ("warm", &warm)] {
        assert_eq!(sorted_facts(prs), sorted_facts(&rest), "{label} facts");
        assert_eq!(decisions(prs), decisions(&rest), "{label} decisions");
    }
    assert_eq!(count(&cold_calls, "/check-runs"), 0);
    // Only the head with an EXPECTED context reads REST again: it can never
    // be shown current.
    assert_eq!(
        warm_calls
            .iter()
            .filter(|line| line.contains("/statuses"))
            .collect::<Vec<_>>(),
        vec![&format!(
            "api repos/owner/repo/commits/{}/statuses?per_page=100&page=1",
            sha(18)
        )],
    );
    assert!(
        cold.iter()
            .find(|pr| pr.fact.number == 18)
            .is_some_and(|pr| pr
                .fact
                .checks
                .iter()
                .all(|check| check.name != "deploy-preview")),
        "an EXPECTED context is not a status REST knows"
    );
    assert!(
        count(&cold_calls, "after0=PAGE2") >= 1,
        "the second page was read: {cold_calls:?}"
    );
    let decided = decisions(&rest);
    let by_number = |number: u64| {
        &decided[heads
            .iter()
            .position(|head| head.number == number)
            .expect("head")]
    };
    for passing in [11, 12, 13, 14, 18, 19] {
        assert_eq!(
            by_number(passing),
            &StewardDecision::ArmMergeQueue,
            "head {passing}"
        );
    }
    for failing in [15, 16, 17] {
        assert!(
            matches!(by_number(failing), StewardDecision::RequiredFailed { .. }),
            "head {failing}: {:?}",
            by_number(failing)
        );
    }
}

#[cfg(unix)]
#[test]
fn a_refused_rollup_falls_back_to_rest_and_is_counted() {
    let heads = fixture_heads();
    let rest = rest_census(&heads);
    for (marker, reason) in [
        ("graphql-errors", "graphql_errors"),
        ("null-node", "missing_commit"),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let pass = std::sync::Arc::new(crate::gh_pass::GhPass::counting());
        let actions = fixture_gh(&temp, &heads).with_pass(pass.clone());
        fs::write(temp.path().join("fixtures").join(marker), "").expect("marker");
        let prs = rollup_census(&temp, &actions, &heads, &temp.path().join("history.json"))
            .expect("fallback census");
        assert_eq!(sorted_facts(&prs), sorted_facts(&rest), "{marker}");
        let report = pass.report();
        let batches = heads.len().div_ceil(HEADS_PER_QUERY);
        assert_eq!(
            report.fallbacks.len(),
            1,
            "{marker}: {:?}",
            report.fallbacks
        );
        assert_eq!(
            report.fallbacks[0].reason,
            format!("census check rollup -> REST: {reason}")
        );
        assert_eq!(
            report.fallbacks[0].count,
            u64::try_from(batches).expect("batches")
        );
        assert!(!report.fallbacks[0].last_detail.is_empty());
        let json = serde_json::to_value(&report).expect("report");
        assert!(json.get("fallbacks").is_some(), "the report carries it");
    }
    // A healthy census reports no fallback, and the field is left out.
    let temp = tempfile::tempdir().expect("temp");
    let pass = std::sync::Arc::new(crate::gh_pass::GhPass::counting());
    let actions = fixture_gh(&temp, &heads).with_pass(pass.clone());
    rollup_census(&temp, &actions, &heads, &temp.path().join("history.json")).expect("census");
    let report = pass.report();
    assert!(report.fallbacks.is_empty());
    assert!(
        serde_json::to_value(&report)
            .expect("report")
            .get("fallbacks")
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn a_rate_limited_rollup_fails_the_census_instead_of_spending_rest_reads() {
    let heads = fixture_heads();
    let temp = tempfile::tempdir().expect("temp");
    let actions = fixture_gh(&temp, &heads);
    fs::write(temp.path().join("fixtures/rate-limited"), "").expect("marker");
    let error = rollup_census(&temp, &actions, &heads, &temp.path().join("history.json"))
        .expect_err("rate limited");
    assert!(
        crate::app::merge_steward_cmd::pass_budget::is_rate_limited(&error),
        "{error}"
    );
    assert_eq!(count(&calls(&temp), "commits/"), 0, "no REST fallback");
}

/// A live Generous-Corp/pulp head (2026-11-08), its REST check runs and
/// status history and its rollup, trimmed to the fields both paths read.
#[test]
fn recorded_rest_and_rollup_answers_parse_alike() {
    let recorded: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/pulp_head_rest_and_rollup.json"))
            .expect("fixture");
    let nodes = recorded["rollup_nodes"].as_array().expect("nodes");
    let mut rollup_runs = nodes
        .iter()
        .filter(|node| node["__typename"] == "CheckRun")
        .map(|node| {
            crate::app::merge_steward_cmd::head_rollup::rollup_check_run(node).expect("run")
        })
        .collect::<Vec<_>>();
    let mut rest_runs = recorded["rest_check_runs"]
        .as_array()
        .expect("runs")
        .iter()
        .map(|row| crate::app::merge_steward_cmd::observation::parse_rest_check(row).expect("run"))
        .collect::<Vec<_>>();
    rollup_runs.sort_by_key(|check| check.check_id);
    rest_runs.sort_by_key(|check| check.check_id);
    assert_eq!(rollup_runs.len(), 2);
    assert_eq!(rollup_runs, rest_runs);
    // The rollup's newest statuses key exactly like the REST history's:
    // createdAt and targetUrl spelled the same, the bot's login gaining
    // REST's `[bot]` suffix, its databaseId the REST creator id.
    let history = recorded["rest_statuses"].as_array().expect("statuses");
    assert!(
        history
            .iter()
            .all(|row| row["creator"]["login"] == "github-actions[bot]")
    );
    let mut rollup = crate::app::merge_steward_cmd::head_rollup::HeadRollup::default();
    for node in nodes
        .iter()
        .filter(|node| node["__typename"] == "StatusContext")
    {
        assert_eq!(node["creator"]["login"], "github-actions");
        rollup.has_statuses = true;
        rollup.latest_statuses.insert(
            crate::app::merge_steward_cmd::head_rollup::rollup_status_key(node).expect("key"),
        );
    }
    assert_eq!(rollup.latest_statuses.len(), 2);
    assert!(rollup.history_is_current(history));
}

#[test]
fn a_dead_writers_temp_file_is_swept_on_load_and_a_live_one_is_kept() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("merge-steward.status-history.o~r.json");
    let stale = temp
        .path()
        .join("merge-steward.status-history.o~r.json.4242.tmp");
    let live = temp
        .path()
        .join("merge-steward.status-history.o~r.json.4343.tmp");
    let other = temp.path().join("merge-steward.json.4242.tmp");
    for file in [&stale, &live, &other] {
        fs::write(file, b"partial").expect("temp");
    }
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for file in [&stale, &other] {
        fs::File::options()
            .write(true)
            .open(file)
            .and_then(|file| file.set_modified(old))
            .expect("age");
    }
    let _ = StatusHistory::load(&path, "o/r");
    assert!(!stale.exists(), "a stale temp of this file is removed");
    assert!(live.exists(), "a recent temp may be a live writer's");
    assert!(other.exists(), "another file's temp is not ours");
}
