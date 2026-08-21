use std::collections::BTreeMap;
use std::io::Write;

use chrono::Utc;
use serde_json::Value;

use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::reconcile::{
    ReconcileFetchError, fetch_status_check_rollup_with_cwd, reconcile_ship_state,
};
use crate::ship_liveness::LivenessContext;
use crate::ship_state::{ShipState, ShipStateStore};

pub(super) fn ship_state_list<W: Write>(
    store: &ShipStateStore,
    liveness: &LivenessContext<'_>,
    json: bool,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let states = store.list_active();
    let now = Utc::now();
    if json {
        let orphaned = states
            .iter()
            .filter_map(|state| {
                liveness.classify(state, now).map(|report| {
                    serde_json::json!({
                        "repo": state.repo,
                        "pr": state.pr,
                        "stalled_minutes": report.stalled_minutes,
                        "evidence": report.evidence.as_str(),
                    })
                })
            })
            .collect::<Vec<_>>();
        let mut data = BTreeMap::new();
        data.insert("states".to_owned(), serde_json::to_value(&states)?);
        data.insert("orphaned".to_owned(), Value::Array(orphaned));
        write_json_envelope(stdout, "ship-state:list", data)?;
        return Ok(());
    }
    if states.is_empty() {
        writeln!(stdout, "No active ship state.")?;
        return Ok(());
    }
    for state in &states {
        let age = now
            .signed_duration_since(state.updated_at)
            .num_minutes()
            .max(0);
        let title = if !state.pr_title.is_empty() {
            state.pr_title.clone()
        } else if !state.commit_subject.is_empty() {
            state.commit_subject.clone()
        } else {
            "(no title)".to_owned()
        };
        writeln!(
            stdout,
            "PR #{}  sha={}  attempt={}  runs={}  age={}m  {}",
            state.pr,
            abbreviate_sha(&state.head_sha),
            state.attempt,
            state.dispatched_runs.len(),
            age,
            title
        )?;
        if let Some(report) = liveness.classify(state, now) {
            writeln!(
                stdout,
                "    ORPHANED? [{}]: in flight, {} ({}m stalled); auto-merge will not fire \
                 until it reaches a verdict — re-run `shipyard ship`, or `ship-state discard` \
                 if it is truly dead.",
                report.evidence.as_str(),
                report.evidence.cause(),
                report.stalled_minutes,
            )?;
        }
        if !state.pr_url.is_empty() {
            writeln!(stdout, "    {}", state.pr_url)?;
        }
    }
    Ok(())
}

pub(super) fn ship_state_show<W: Write>(
    store: &ShipStateStore,
    repository: Option<&str>,
    pr: u64,
    json: bool,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(state) = repository.map_or_else(|| store.get(pr), |repo| store.get_scoped(repo, pr))
    else {
        return Err(format!("No ship state for PR #{pr}").into());
    };

    if json {
        let mut data = BTreeMap::new();
        let value = serde_json::to_value(state)?;
        let Value::Object(map) = value else {
            return Err("ship-state must serialize as an object".into());
        };
        for (key, value) in map {
            data.insert(key, value);
        }
        write_json_envelope(stdout, "ship-state:show", data)?;
        return Ok(());
    }

    writeln!(stdout, "PR #{}  attempt {}", state.pr, state.attempt)?;
    if !state.pr_title.is_empty() {
        writeln!(stdout, "  title:          {}", state.pr_title)?;
    }
    if !state.pr_url.is_empty() {
        writeln!(stdout, "  url:            {}", state.pr_url)?;
    }
    if !state.commit_subject.is_empty() {
        writeln!(stdout, "  commit:         {}", state.commit_subject)?;
    }
    writeln!(stdout, "  repo:           {}", state.repo)?;
    writeln!(
        stdout,
        "  branch:         {} -> {}",
        state.branch, state.base_branch
    )?;
    writeln!(stdout, "  head_sha:       {}", state.head_sha)?;
    writeln!(stdout, "  policy:         {}", state.policy_signature)?;
    writeln!(stdout, "  evidence:       {:?}", state.evidence_snapshot)?;
    writeln!(
        stdout,
        "  dispatched:     {} run(s)",
        state.dispatched_runs.len()
    )?;
    for run in state.dispatched_runs {
        writeln!(
            stdout,
            "    - {} ({}) run_id={} status={}",
            run.target, run.provider, run.run_id, run.status
        )?;
    }
    writeln!(
        stdout,
        "  created_at:     {}",
        state.created_at.to_rfc3339()
    )?;
    writeln!(
        stdout,
        "  updated_at:     {}",
        state.updated_at.to_rfc3339()
    )?;
    Ok(())
}

pub(super) fn ship_state_discard<W: Write>(
    store: &ShipStateStore,
    repository: Option<&str>,
    pr: u64,
    json: bool,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(state) = repository.map_or_else(|| store.get(pr), |repo| store.get_scoped(repo, pr))
    else {
        return Err(format!("No ship state for PR #{pr}").into());
    };
    let archived = store.archive_scoped(&state.repo, pr)?;
    if json {
        let mut data = BTreeMap::new();
        data.insert("pr".to_owned(), Value::from(pr));
        data.insert(
            "archived_to".to_owned(),
            archived.map_or(Value::Null, |path| {
                Value::String(path.to_string_lossy().into_owned())
            }),
        );
        write_json_envelope(stdout, "ship-state:discard", data)?;
    } else {
        writeln!(stdout, "Archived ship state for PR #{pr}.")?;
    }
    Ok(())
}

pub(super) fn ship_state_reconcile<W: Write>(
    store: &ShipStateStore,
    mode: RuntimeMode,
    cwd: &std::path::Path,
    pr: Option<u64>,
    reconcile_all: bool,
    json: bool,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let repository = (mode == RuntimeMode::Shipyard)
        .then(|| super::branch_cmd::detect_repo_from_remote(cwd, None))
        .flatten();
    ship_state_reconcile_with(
        store,
        repository.as_deref(),
        pr,
        reconcile_all,
        json,
        stdout,
        |state| fetch_status_check_rollup_with_cwd(mode, cwd, &state.repo, state.pr),
    )
}

fn ship_state_reconcile_with<W: Write, F>(
    store: &ShipStateStore,
    repository: Option<&str>,
    pr: Option<u64>,
    reconcile_all: bool,
    json: bool,
    stdout: &mut W,
    mut fetch: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnMut(&ShipState) -> Result<Vec<Value>, ReconcileFetchError>,
{
    let targets = if reconcile_all {
        store.list_active()
    } else if let Some(pr) = pr {
        repository
            .map_or_else(|| store.get(pr), |repo| store.get_scoped(repo, pr))
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };

    if targets.is_empty() {
        if json {
            let mut data = BTreeMap::new();
            data.insert("results".to_owned(), Value::Array(Vec::new()));
            write_json_envelope(stdout, "ship-state:reconcile", data)?;
        } else if let Some(pr) = pr {
            writeln!(stdout, "No active ship state for PR #{pr}.")?;
        } else {
            writeln!(stdout, "No active ship state.")?;
        }
        return Ok(());
    }

    let now = Utc::now();
    let mut results = Vec::new();
    for state in targets {
        match fetch(&state) {
            Ok(rollup) => {
                let mut changes = Vec::new();
                store.with_pr_state_scoped_locked(&state.repo, state.pr, |current| {
                    let Some(current_state) = current.as_ref() else {
                        return Ok(());
                    };
                    let reconciled = reconcile_ship_state(current_state, &rollup, now);
                    if !reconciled.changes.is_empty() {
                        changes = reconciled.changes;
                        *current = Some(reconciled.state);
                    }
                    Ok(())
                })?;
                results.push(reconcile_success(state.pr, changes));
            }
            Err(error) => {
                results.push(reconcile_error(state.pr, error.to_string()));
            }
        }
    }

    if json {
        let mut data = BTreeMap::new();
        data.insert("results".to_owned(), Value::Array(results));
        write_json_envelope(stdout, "ship-state:reconcile", data)?;
        return Ok(());
    }

    for result in results {
        let pr = result.get("pr").and_then(Value::as_u64).unwrap_or_default();
        if result.get("ok").and_then(Value::as_bool) == Some(false) {
            let error = result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            writeln!(stdout, "PR #{pr}: {error}")?;
            continue;
        }
        let changes = result
            .get("changes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if changes.is_empty() {
            writeln!(stdout, "PR #{pr}: already in sync with GitHub")?;
        } else {
            writeln!(stdout, "PR #{pr}: applied {} change(s)", changes.len())?;
            for change in changes {
                if let Some(change) = change.as_str() {
                    writeln!(stdout, "  · {change}")?;
                }
            }
        }
    }
    Ok(())
}

fn reconcile_success(pr: u64, changes: Vec<String>) -> Value {
    let mut result = serde_json::Map::new();
    result.insert("pr".to_owned(), Value::from(pr));
    result.insert("ok".to_owned(), Value::Bool(true));
    result.insert(
        "changes".to_owned(),
        Value::Array(changes.into_iter().map(Value::String).collect()),
    );
    Value::Object(result)
}

fn reconcile_error(pr: u64, error: String) -> Value {
    let mut result = serde_json::Map::new();
    result.insert("pr".to_owned(), Value::from(pr));
    result.insert("ok".to_owned(), Value::Bool(false));
    result.insert("error".to_owned(), Value::String(error));
    Value::Object(result)
}

fn abbreviate_sha(sha: &str) -> &str {
    let max = sha
        .char_indices()
        .nth(12)
        .map_or_else(|| sha.len(), |(index, _)| index);
    &sha[..max]
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use serde_json::Value;
    use tempfile::TempDir;

    use super::{
        abbreviate_sha, ship_state_discard, ship_state_list, ship_state_reconcile_with,
        ship_state_show,
    };
    use crate::reconcile::ReconcileFetchError;
    use crate::ship_liveness::{DEFAULT_ORPHAN_STALE_MINUTES, LivenessContext};
    use crate::ship_state::{DispatchedRun, ShipState, ShipStateStore};

    fn store(temp: &TempDir) -> ShipStateStore {
        ShipStateStore::new(temp.path().to_path_buf()).expect("state store should open")
    }

    /// Queue-free liveness context: pure `updated_at` staleness (`time_fallback`
    /// evidence). The queue-backed classification paths are covered in
    /// `crate::ship_liveness` unit tests.
    fn time_ctx() -> LivenessContext<'static> {
        LivenessContext::time_only(Duration::minutes(DEFAULT_ORPHAN_STALE_MINUTES))
    }

    fn sample_state(pr: u64, sha: &str) -> ShipState {
        let mut state = ShipState::new(
            pr,
            "danielraffel/pulp",
            format!("shipyard-pr-{pr}"),
            "main",
            sha,
            "policy0001",
        );
        state.pr_url = format!("https://github.com/danielraffel/pulp/pull/{pr}");
        state.pr_title = format!("Ship PR {pr}");
        state.commit_subject = format!("Commit subject {pr}");
        state
    }

    fn sample_run(target: &str, run_id: &str) -> DispatchedRun {
        let now = Utc::now();
        DispatchedRun {
            target: target.to_owned(),
            provider: "namespace".to_owned(),
            run_id: run_id.to_owned(),
            status: "success".to_owned(),
            started_at: now,
            updated_at: now,
            attempt: 1,
            last_heartbeat_at: Some(now),
            phase: Some("complete".to_owned()),
            required: true,
        }
    }

    #[test]
    fn list_human_reports_empty_store() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut out = Vec::new();

        ship_state_list(&store, &time_ctx(), false, &mut out).expect("list should render");

        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "No active ship state.\n"
        );
    }

    #[test]
    fn list_human_renders_state_summary() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut state = sample_state(42, "abcdef0123456789abcdef0123456789abcdef01");
        state.attempt = 3;
        state.updated_at = Utc::now() - Duration::minutes(9);
        state.dispatched_runs.push(sample_run("linux", "run-42"));
        store.save(&state).expect("state should save");
        let mut out = Vec::new();

        ship_state_list(&store, &time_ctx(), false, &mut out).expect("list should render");

        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("PR #42"));
        assert!(text.contains("sha=abcdef012345"));
        assert!(text.contains("attempt=3"));
        assert!(text.contains("runs=1"));
        assert!(text.contains("Ship PR 42"));
        assert!(text.contains("https://github.com/danielraffel/pulp/pull/42"));
    }

    #[test]
    fn list_json_uses_envelope_and_sorted_states() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        store
            .save(&sample_state(9, "9999999999999999999999999999999999999999"))
            .expect("state should save");
        store
            .save(&sample_state(2, "2222222222222222222222222222222222222222"))
            .expect("state should save");
        let mut out = Vec::new();

        ship_state_list(&store, &time_ctx(), true, &mut out).expect("list should render");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(payload["command"], "ship-state:list");
        assert_eq!(payload["schema_version"], 1);
        assert_eq!(payload["states"][0]["pr"], 2);
        assert_eq!(payload["states"][1]["pr"], 9);
    }

    #[test]
    fn list_json_reports_orphaned_prs() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        // Stale in-flight → orphaned via the time fallback (no queue context).
        let mut orphan = sample_state(3, "3333333333333333333333333333333333333333");
        orphan.updated_at = Utc::now() - Duration::minutes(DEFAULT_ORPHAN_STALE_MINUTES + 10);
        store.save(&orphan).expect("state should save");
        // Terminal verdict → not orphaned even though also old.
        let mut done = sample_state(4, "4444444444444444444444444444444444444444");
        done.update_evidence("linux", "pass");
        done.dispatched_runs.push(sample_run("linux", "run-4"));
        done.updated_at = Utc::now() - Duration::hours(3);
        store.save(&done).expect("state should save");
        let mut out = Vec::new();

        ship_state_list(&store, &time_ctx(), true, &mut out).expect("list should render");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        let orphaned = payload["orphaned"].as_array().expect("orphaned array");
        assert_eq!(orphaned.len(), 1, "only the stale in-flight PR is orphaned");
        assert_eq!(orphaned[0]["repo"], "danielraffel/pulp");
        assert_eq!(orphaned[0]["pr"], 3);
        assert_eq!(orphaned[0]["evidence"], "time_fallback");
        assert!(orphaned[0]["stalled_minutes"].as_i64().unwrap() >= DEFAULT_ORPHAN_STALE_MINUTES);
    }

    #[test]
    fn list_human_marks_orphaned_state() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut state = sample_state(5, "5555555555555555555555555555555555555555");
        state.updated_at = Utc::now() - Duration::minutes(DEFAULT_ORPHAN_STALE_MINUTES + 30);
        store.save(&state).expect("state should save");
        let mut out = Vec::new();

        ship_state_list(&store, &time_ctx(), false, &mut out).expect("list should render");

        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("PR #5"));
        assert!(
            text.contains("ORPHANED? [time_fallback]"),
            "text was: {text}"
        );
        assert!(text.contains("re-run `shipyard ship`"));
    }

    #[test]
    fn show_human_renders_full_state_details() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut state = sample_state(7, "7777777777777777777777777777777777777777");
        state.update_evidence("linux", "PASS");
        state.dispatched_runs.push(sample_run("linux", "run-7"));
        store.save(&state).expect("state should save");
        let mut out = Vec::new();

        ship_state_show(&store, None, 7, false, &mut out).expect("show should render");

        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("PR #7  attempt 1"));
        assert!(text.contains("title:          Ship PR 7"));
        assert!(text.contains("url:            https://github.com/danielraffel/pulp/pull/7"));
        assert!(text.contains("repo:           danielraffel/pulp"));
        assert!(text.contains("branch:         shipyard-pr-7 -> main"));
        assert!(text.contains("policy:         policy0001"));
        assert!(text.contains("\"linux\": \"PASS\""));
        assert!(text.contains("- linux (namespace) run_id=run-7 status=success"));
    }

    #[test]
    fn show_json_flattens_state_into_envelope() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut state = sample_state(12, "1212121212121212121212121212121212121212");
        state.dispatched_runs.push(sample_run("macos", "run-12"));
        store.save(&state).expect("state should save");
        let mut out = Vec::new();

        ship_state_show(&store, None, 12, true, &mut out).expect("show should render");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(payload["command"], "ship-state:show");
        assert_eq!(payload["pr"], 12);
        assert_eq!(payload["repo"], "danielraffel/pulp");
        assert_eq!(payload["dispatched_runs"][0]["target"], "macos");
        assert_eq!(payload["dispatched_runs"][0]["run_id"], "run-12");
    }

    #[test]
    fn show_missing_state_returns_clear_error() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut out = Vec::new();

        let err = ship_state_show(&store, None, 404, false, &mut out).expect_err("missing state");

        assert_eq!(err.to_string(), "No ship state for PR #404");
        assert!(out.is_empty());
    }

    #[test]
    fn show_scoped_selects_current_repository_when_pr_numbers_collide() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        store
            .save(&ShipState::new(
                7,
                "owner/pulp",
                "pulp",
                "main",
                "p",
                "policy",
            ))
            .expect("pulp state");
        store
            .save(&ShipState::new(
                7,
                "owner/forge",
                "forge",
                "main",
                "f",
                "policy",
            ))
            .expect("forge state");
        let mut out = Vec::new();

        ship_state_show(&store, Some("OWNER/FORGE"), 7, true, &mut out).expect("scoped show");

        let value: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(value["repo"], "owner/forge");
        assert_eq!(value["head_sha"], "f");
    }

    #[test]
    fn discard_json_archives_state_and_reports_path() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        store
            .save(&sample_state(
                33,
                "3333333333333333333333333333333333333333",
            ))
            .expect("state should save");
        let mut out = Vec::new();

        ship_state_discard(&store, None, 33, true, &mut out).expect("discard should render");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(payload["command"], "ship-state:discard");
        assert_eq!(payload["schema_version"], 1);
        assert_eq!(payload["pr"], 33);
        let archived_to = payload["archived_to"].as_str().expect("archive path");
        assert!(archived_to.contains("33-"));
        assert!(store.get(33).is_none());
        assert_eq!(store.list_archived().len(), 1);
    }

    #[test]
    fn discard_human_reports_archived_state() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        store
            .save(&sample_state(
                34,
                "3434343434343434343434343434343434343434",
            ))
            .expect("state should save");
        let mut out = Vec::new();

        ship_state_discard(&store, None, 34, false, &mut out).expect("discard should render");

        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "Archived ship state for PR #34.\n"
        );
        assert!(store.get(34).is_none());
    }

    #[test]
    fn discard_missing_state_returns_clear_error() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut out = Vec::new();

        let err = ship_state_discard(&store, None, 404, true, &mut out).expect_err("missing state");

        assert_eq!(err.to_string(), "No ship state for PR #404");
        assert!(out.is_empty());
    }

    #[test]
    fn reconcile_json_heals_matching_run_and_persists_state() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut state = sample_state(42, "4242424242424242424242424242424242424242");
        state.dispatched_runs.push(DispatchedRun {
            status: "in_progress".to_owned(),
            ..sample_run("macos", "run-42")
        });
        store.save(&state).expect("state should save");
        let mut out = Vec::new();

        ship_state_reconcile_with(&store, None, Some(42), false, true, &mut out, |_| {
            Ok(vec![serde_json::json!({
                "name": "Build / macos",
                "state": "COMPLETED",
                "conclusion": "SUCCESS",
                "completedAt": "2026-04-25T07:04:00Z"
            })])
        })
        .expect("reconcile should render");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(payload["command"], "ship-state:reconcile");
        assert_eq!(payload["results"][0]["pr"], 42);
        assert_eq!(payload["results"][0]["ok"], true);
        assert!(
            payload["results"][0]["changes"][0]
                .as_str()
                .expect("change")
                .contains("in_progress")
        );
        let saved = store.get(42).expect("saved state");
        assert_eq!(saved.dispatched_runs[0].status, "completed");
        assert_eq!(saved.evidence_snapshot["macos"], "pass");
    }

    #[test]
    fn reconcile_all_reports_fetch_errors_without_mutating_other_states() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        store
            .save(&sample_state(5, "5555555555555555555555555555555555555555"))
            .expect("state should save");
        let mut out = Vec::new();

        ship_state_reconcile_with(&store, None, None, true, false, &mut out, |state| {
            Err(ReconcileFetchError::Command(format!(
                "gh failed for PR #{}",
                state.pr
            )))
        })
        .expect("reconcile should render");

        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("PR #5: gh failed for PR #5"));
        assert!(store.get(5).is_some());
    }

    #[test]
    fn reconcile_empty_store_is_nonsilent() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut out = Vec::new();

        ship_state_reconcile_with(&store, None, Some(404), false, false, &mut out, |_| {
            Ok(Vec::new())
        })
        .expect("reconcile should render");

        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "No active ship state for PR #404.\n"
        );
    }

    #[test]
    fn abbreviate_sha_respects_utf8_boundaries() {
        assert_eq!(abbreviate_sha("abcdef0123456789"), "abcdef012345");
        assert_eq!(abbreviate_sha("short"), "short");
        assert_eq!(abbreviate_sha("abcdef01234é6789"), "abcdef01234é");
    }
}
