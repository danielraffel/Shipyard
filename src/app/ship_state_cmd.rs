use std::collections::BTreeMap;
use std::io::Write;

use chrono::Utc;
use serde_json::Value;

use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::reconcile::{
    ReconcileFetchError, fetch_status_check_rollup_with_cwd, reconcile_ship_state,
};
use crate::ship_liveness::{LivenessContext, LivenessFinding, PrLifecycle, reconcile_finding};
use crate::ship_state::{ShipState, ShipStateStore};

/// Resolves a pull request's current lifecycle. Production passes a `gh`-backed
/// reader; tests inject a fixture so no test ever touches the network or the
/// live record store.
pub(super) type PrLifecycleReader<'a> = &'a mut dyn FnMut(&str, u64) -> PrLifecycle;

/// Classify every active ship-state once, resolving the PR lifecycle **only**
/// for records the queue already flagged.
///
/// The lookup is deliberately not run for every record: a healthy store holds
/// hundreds of finished records, and asking GitHub about each one would turn a
/// local diagnostic into a rate-limit hazard. Flagged records are a handful.
/// Hard ceiling on PR-lifecycle lookups per `ship-state list`.
///
/// Each lookup is one `gh pr view`, so an uncapped loop turns a diagnostic that
/// used to cost nothing into one API call per flagged record. That is fine at
/// the observed scale (8 flagged out of 157 active), but a mass-orphan event —
/// a daemon crash with dozens in flight — would fan out into a burst, and
/// GitHub throttles bursts independently of the core quota. Beyond the budget
/// the remaining records simply keep `Unknown`, which fails closed: they stay
/// flagged and say their PR state could not be read, which is the truth.
const MAX_PR_LIFECYCLE_LOOKUPS: usize = 25;

fn classify_states(
    states: &[ShipState],
    liveness: &LivenessContext<'_>,
    lifecycle_of: PrLifecycleReader<'_>,
    now: chrono::DateTime<Utc>,
) -> Vec<Option<LivenessFinding>> {
    let mut budget = MAX_PR_LIFECYCLE_LOOKUPS;
    states
        .iter()
        .map(|state| {
            let report = liveness.classify(state, now)?;
            let lifecycle = if budget == 0 {
                PrLifecycle::Unknown
            } else {
                budget -= 1;
                lifecycle_of(&state.repo, state.pr)
            };
            Some(reconcile_finding(report, lifecycle))
        })
        .collect()
}

/// Emits the machine-readable `ship-state:list` envelope. `orphaned` keeps only
/// records that can still be waiting on Shipyard; a record whose PR already
/// reached a terminal state moves to `resolved`, so a consumer counting
/// `orphaned` is not misled by leftovers.
fn write_list_json<W: Write>(
    states: &[ShipState],
    findings: &[Option<LivenessFinding>],
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut orphaned = Vec::new();
    let mut resolved = Vec::new();
    for (state, finding) in states.iter().zip(findings) {
        let (bucket, evidence, stalled_minutes, lifecycle) = match finding {
            Some(LivenessFinding::Orphaned { report, lifecycle }) => (
                &mut orphaned,
                report.evidence,
                report.stalled_minutes,
                *lifecycle,
            ),
            Some(LivenessFinding::Resolved(report)) => (
                &mut resolved,
                report.evidence,
                report.stalled_minutes,
                report.lifecycle,
            ),
            None => continue,
        };
        bucket.push(serde_json::json!({
            "repo": state.repo,
            "pr": state.pr,
            "stalled_minutes": stalled_minutes,
            "evidence": evidence.as_str(),
            "pr_lifecycle": lifecycle.as_str(),
        }));
    }
    let mut data = BTreeMap::new();
    data.insert("states".to_owned(), serde_json::to_value(states)?);
    data.insert("orphaned".to_owned(), Value::Array(orphaned));
    data.insert("resolved".to_owned(), Value::Array(resolved));
    write_json_envelope(stdout, "ship-state:list", data)
}

/// Writes the operator-facing note under one record.
///
/// The orphan note deliberately does NOT claim "auto-merge will not fire". That
/// wording was measurably false: GitHub-native auto-merge lands PRs whose
/// ship-state never reached a verdict, so most flagged records on a real store
/// belonged to PRs that had already merged. State only what is known, and say
/// so plainly when the PR's own state could not be read.
fn write_liveness_note<W: Write>(
    state: &ShipState,
    finding: LivenessFinding,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    match finding {
        LivenessFinding::Orphaned { report, lifecycle } => {
            let consequence = if lifecycle == PrLifecycle::Open {
                "the PR is still open and Shipyard will not reach a verdict on its own"
            } else {
                "the PR state could not be read, so this record may already be resolved"
            };
            writeln!(
                stdout,
                "    ORPHANED? [{}]: in flight, {} ({}m stalled); {} — re-run \
                 `shipyard ship`, or `ship-state discard` if it is truly dead.",
                report.evidence.as_str(),
                report.evidence.cause(),
                report.stalled_minutes,
                consequence,
            )?;
        }
        LivenessFinding::Resolved(report) => {
            let landed = if report.lifecycle == PrLifecycle::Merged {
                "the PR already merged"
            } else {
                "the PR was closed without merging"
            };
            writeln!(
                stdout,
                "    RESOLVED [{}]: {}, so no verdict is owed; this record never \
                 finalized and is {}m stale. It blocks nothing — clear it with \
                 `shipyard ship-state discard {}`.",
                report.lifecycle.as_str(),
                landed,
                report.stalled_minutes,
                state.pr,
            )?;
        }
    }
    Ok(())
}

pub(super) fn ship_state_list<W: Write>(
    store: &ShipStateStore,
    liveness: &LivenessContext<'_>,
    lifecycle_of: PrLifecycleReader<'_>,
    json: bool,
    stdout: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let states = store.list_active();
    let now = Utc::now();
    let findings = classify_states(&states, liveness, lifecycle_of, now);
    if json {
        return write_list_json(&states, &findings, stdout);
    }
    if states.is_empty() {
        writeln!(stdout, "No active ship state.")?;
        return Ok(());
    }
    for (state, finding) in states.iter().zip(&findings) {
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
            "{} PR #{}  sha={}  attempt={}  runs={}  age={}m  {}",
            state.repo,
            state.pr,
            abbreviate_sha(&state.head_sha),
            state.attempt,
            state.dispatched_runs.len(),
            age,
            title
        )?;
        if let Some(finding) = finding {
            write_liveness_note(state, *finding, stdout)?;
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
    let repository = super::branch_cmd::detect_repo_from_remote(cwd, None);
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
        MAX_PR_LIFECYCLE_LOOKUPS, abbreviate_sha, ship_state_discard, ship_state_list,
        ship_state_reconcile_with, ship_state_show,
    };
    use crate::reconcile::ReconcileFetchError;
    use crate::ship_liveness::{DEFAULT_ORPHAN_STALE_MINUTES, LivenessContext, PrLifecycle};
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

    /// The pre-existing default for tests that predate PR-lifecycle
    /// reconciliation: GitHub is never consulted, so every flagged record keeps
    /// the (fail-closed) orphan verdict.
    fn unknown_lifecycle() -> impl FnMut(&str, u64) -> PrLifecycle {
        |_repo: &str, _pr: u64| PrLifecycle::Unknown
    }

    /// A fixture lifecycle reader that records which PRs it was asked about, so
    /// a test can prove the lookup is bounded to flagged records only.
    fn fixed_lifecycle(
        lifecycle: PrLifecycle,
        asked: &mut Vec<u64>,
    ) -> impl FnMut(&str, u64) -> PrLifecycle + '_ {
        move |_repo: &str, pr: u64| {
            asked.push(pr);
            lifecycle
        }
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

        ship_state_list(
            &store,
            &time_ctx(),
            &mut unknown_lifecycle(),
            false,
            &mut out,
        )
        .expect("list should render");

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

        ship_state_list(
            &store,
            &time_ctx(),
            &mut unknown_lifecycle(),
            false,
            &mut out,
        )
        .expect("list should render");

        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("danielraffel/pulp PR #42"));
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

        ship_state_list(
            &store,
            &time_ctx(),
            &mut unknown_lifecycle(),
            true,
            &mut out,
        )
        .expect("list should render");

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

        ship_state_list(
            &store,
            &time_ctx(),
            &mut unknown_lifecycle(),
            true,
            &mut out,
        )
        .expect("list should render");

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

        ship_state_list(
            &store,
            &time_ctx(),
            &mut unknown_lifecycle(),
            false,
            &mut out,
        )
        .expect("list should render");

        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("PR #5"));
        assert!(
            text.contains("ORPHANED? [time_fallback]"),
            "text was: {text}"
        );
        assert!(text.contains("re-run `shipyard ship`"));
    }

    /// Plants the measured defect: a record left in flight against a PR that
    /// has already **merged**. Before PR-lifecycle reconciliation this rendered
    /// as `ORPHANED? … auto-merge will not fire until it reaches a verdict`,
    /// which was false — the PR merged anyway. On a live store 5 of the 8
    /// flagged records were merged PRs, so the detector was 62% stale and an
    /// operator learned to ignore it.
    #[test]
    fn merged_pr_record_is_reported_resolved_not_orphaned() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut state = sample_state(8148, "838ec05b2f370000000000000000000000000000");
        state.updated_at = Utc::now() - Duration::minutes(138);
        store.save(&state).expect("state should save");
        let mut asked = Vec::new();
        let mut out = Vec::new();

        ship_state_list(
            &store,
            &time_ctx(),
            &mut fixed_lifecycle(PrLifecycle::Merged, &mut asked),
            false,
            &mut out,
        )
        .expect("list should render");

        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(asked, vec![8148], "only the flagged record is looked up");
        assert!(
            text.contains("RESOLVED [merged]"),
            "a merged PR's leftover record must read as resolved; text was: {text}"
        );
        assert!(
            text.contains("shipyard ship-state discard 8148"),
            "the operator needs the exact reaping command; text was: {text}"
        );
        assert!(
            !text.contains("ORPHANED?"),
            "a merged PR is not an orphan blocking a merge; text was: {text}"
        );
        assert!(
            !text.contains("auto-merge will not fire"),
            "this claim was measurably false for merged PRs; text was: {text}"
        );
    }

    /// Positive control for the test above. Same record, same age, same code
    /// path — only the PR lifecycle differs. If this stopped reporting an
    /// orphan, the fix would be suppressing real stalls rather than
    /// reconciling them, and the merged-PR assertion above would pass for the
    /// wrong reason.
    #[test]
    fn open_pr_record_is_still_reported_orphaned() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut state = sample_state(8146, "c6f51d12d4050000000000000000000000000000");
        state.updated_at = Utc::now() - Duration::minutes(282);
        store.save(&state).expect("state should save");
        let mut asked = Vec::new();
        let mut out = Vec::new();

        ship_state_list(
            &store,
            &time_ctx(),
            &mut fixed_lifecycle(PrLifecycle::Open, &mut asked),
            false,
            &mut out,
        )
        .expect("list should render");

        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(asked, vec![8146]);
        assert!(
            text.contains("ORPHANED? [time_fallback]"),
            "text was: {text}"
        );
        assert!(
            text.contains("the PR is still open"),
            "an open PR's consequence must be stated plainly; text was: {text}"
        );
        assert!(!text.contains("RESOLVED"), "text was: {text}");
        assert!(
            !text.contains("auto-merge will not fire"),
            "the false claim must be gone from every branch; text was: {text}"
        );
    }

    /// Fail-closed control: an unreadable PR state must keep the orphan
    /// verdict, so a GitHub outage can never quietly hide a real stall.
    #[test]
    fn unreadable_pr_state_keeps_the_orphan_verdict() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut state = sample_state(8145, "b3211f5994980000000000000000000000000000");
        state.updated_at = Utc::now() - Duration::minutes(333);
        store.save(&state).expect("state should save");
        let mut out = Vec::new();

        ship_state_list(
            &store,
            &time_ctx(),
            &mut unknown_lifecycle(),
            false,
            &mut out,
        )
        .expect("list should render");

        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("ORPHANED? [time_fallback]"),
            "text was: {text}"
        );
        assert!(
            text.contains("could not be read"),
            "an unverified PR must say so rather than assert a consequence; text was: {text}"
        );
    }

    /// The lookup budget must be a hard ceiling, not a hope. An uncapped loop
    /// turns `ship-state list` into one `gh pr view` per flagged record, and a
    /// mass-orphan event would fan that into exactly the burst GitHub throttles
    /// independently of the core quota. Records past the budget fail closed.
    #[test]
    fn pr_lifecycle_lookups_are_capped_and_the_remainder_fails_closed() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let flagged = MAX_PR_LIFECYCLE_LOOKUPS + 7;
        for pr in 0..flagged {
            let pr = u64::try_from(pr).expect("small") + 1;
            let mut state = sample_state(pr, &format!("{pr:040}"));
            state.updated_at = Utc::now() - Duration::minutes(DEFAULT_ORPHAN_STALE_MINUTES + 10);
            store.save(&state).expect("state should save");
        }
        let mut asked = Vec::new();
        let mut out = Vec::new();

        ship_state_list(
            &store,
            &time_ctx(),
            &mut fixed_lifecycle(PrLifecycle::Merged, &mut asked),
            true,
            &mut out,
        )
        .expect("list should render");

        assert_eq!(
            asked.len(),
            MAX_PR_LIFECYCLE_LOOKUPS,
            "every flagged record past the budget must be answered without an API call"
        );
        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        let resolved = payload["resolved"].as_array().expect("resolved array");
        let orphaned = payload["orphaned"].as_array().expect("orphaned array");
        assert_eq!(resolved.len(), MAX_PR_LIFECYCLE_LOOKUPS);
        assert_eq!(orphaned.len(), flagged - MAX_PR_LIFECYCLE_LOOKUPS);
        // Fail closed: unlooked-up records stay flagged rather than being
        // guessed resolved.
        assert!(
            orphaned
                .iter()
                .all(|entry| entry["pr_lifecycle"] == "unknown"),
            "{payload}"
        );
    }

    /// A closed-without-merging PR is also terminal: no verdict can be owed.
    #[test]
    fn closed_pr_record_is_reported_resolved() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut state = sample_state(9001, "9001000000000000000000000000000000000000");
        state.updated_at = Utc::now() - Duration::minutes(600);
        store.save(&state).expect("state should save");
        let mut asked = Vec::new();
        let mut out = Vec::new();

        ship_state_list(
            &store,
            &time_ctx(),
            &mut fixed_lifecycle(PrLifecycle::Closed, &mut asked),
            false,
            &mut out,
        )
        .expect("list should render");

        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("RESOLVED [closed]"), "text was: {text}");
        assert!(text.contains("closed without merging"), "text was: {text}");
        assert!(!text.contains("ORPHANED?"), "text was: {text}");
    }

    /// The JSON surface is what tooling reads. A merged PR's record must leave
    /// `orphaned` entirely — a consumer counting that array is exactly who was
    /// being misled.
    #[test]
    fn list_json_moves_merged_records_out_of_orphaned() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        let mut merged = sample_state(8134, "4841cb30a7790000000000000000000000000000");
        merged.updated_at = Utc::now() - Duration::minutes(647);
        store.save(&merged).expect("state should save");
        let mut asked = Vec::new();
        let mut out = Vec::new();

        ship_state_list(
            &store,
            &time_ctx(),
            &mut fixed_lifecycle(PrLifecycle::Merged, &mut asked),
            true,
            &mut out,
        )
        .expect("list should render");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert!(
            payload["orphaned"]
                .as_array()
                .expect("orphaned array")
                .is_empty(),
            "a merged PR must not be counted as orphaned: {payload}"
        );
        let resolved = payload["resolved"].as_array().expect("resolved array");
        assert_eq!(resolved.len(), 1, "{payload}");
        assert_eq!(resolved[0]["pr"], 8134);
        assert_eq!(resolved[0]["pr_lifecycle"], "merged");
        assert_eq!(resolved[0]["evidence"], "time_fallback");
    }

    /// The lifecycle lookup costs a GitHub call, so it must never run for the
    /// hundreds of already-finished records a healthy store holds. Only records
    /// the queue already flagged are looked up.
    #[test]
    fn lifecycle_lookup_skips_records_the_queue_never_flagged() {
        let temp = TempDir::new().expect("tempdir");
        let store = store(&temp);
        // Terminal verdict → never flagged, however old.
        let mut done = sample_state(100, "1000000000000000000000000000000000000000");
        done.update_evidence("linux", "pass");
        done.dispatched_runs.push(sample_run("linux", "run-100"));
        done.updated_at = Utc::now() - Duration::days(20);
        store.save(&done).expect("state should save");
        // Fresh in-flight → inside the staleness gate, so also never flagged.
        let fresh = sample_state(101, "1010000000000000000000000000000000000000");
        store.save(&fresh).expect("state should save");
        let mut asked = Vec::new();
        let mut out = Vec::new();

        ship_state_list(
            &store,
            &time_ctx(),
            &mut fixed_lifecycle(PrLifecycle::Merged, &mut asked),
            false,
            &mut out,
        )
        .expect("list should render");

        assert!(
            asked.is_empty(),
            "unflagged records must cost no GitHub calls, asked about: {asked:?}"
        );
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
            ..sample_run("macos", "42424242")
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
