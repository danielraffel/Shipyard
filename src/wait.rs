use std::collections::BTreeMap;

use serde::Serialize;

/// Passing conclusions for `wait pr --state green`.
pub const PASSING_CONCLUSIONS: &[&str] = &["SUCCESS", "NEUTRAL", "SKIPPED"];
/// Still-waiting states for GitHub checks.
pub const STILL_WAITING_STATES: &[&str] = &["QUEUED", "IN_PROGRESS", "PENDING"];
/// Terminal run statuses for `wait run`.
pub const RUN_TERMINAL_STATUSES: &[&str] = &["completed"];

/// Error raised when a scope is unsupported, such as rulesets or merge queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsupportedScopeError(pub String);

impl std::fmt::Display for UnsupportedScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UnsupportedScopeError {}

/// Error raised when the input is missing or malformed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidInputError(pub String);

impl std::fmt::Display for InvalidInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidInputError {}

/// Error raised when `wait run --success` sees a terminal failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunFailedFastError {
    /// Observed run snapshot.
    pub observed: BTreeMap<String, serde_json::Value>,
}

impl std::fmt::Display for RunFailedFastError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "run terminal conclusion: {}",
            self.observed
                .get("conclusion")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<unknown>")
        )
    }
}

impl std::error::Error for RunFailedFastError {}

/// Truth-condition evaluation result for `shipyard wait`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TruthResult {
    /// Whether the condition matched.
    pub matched: bool,
    /// JSON-serializable observed state.
    pub observed: BTreeMap<String, serde_json::Value>,
}

/// Evaluate whether a release satisfies the manifest condition.
pub fn evaluate_release(
    snapshot: Option<&serde_json::Value>,
    manifest: Option<&[String]>,
) -> Result<TruthResult, InvalidInputError> {
    let Some(snapshot) = snapshot else {
        return Ok(TruthResult {
            matched: false,
            observed: BTreeMap::from([("exists".to_owned(), serde_json::Value::Bool(false))]),
        });
    };

    let draft = snapshot
        .get("draft")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if draft {
        return Ok(TruthResult {
            matched: false,
            observed: BTreeMap::from([
                ("exists".to_owned(), serde_json::Value::Bool(true)),
                ("draft".to_owned(), serde_json::Value::Bool(true)),
            ]),
        });
    }

    let assets = snapshot
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let observed_assets = assets
        .iter()
        .filter_map(|asset| asset.as_object())
        .map(|asset| {
            serde_json::json!({
                "name": asset.get("name").and_then(serde_json::Value::as_str).unwrap_or(""),
                "state": asset.get("state").and_then(serde_json::Value::as_str).unwrap_or(""),
                "size": asset.get("size").and_then(serde_json::Value::as_i64).unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();

    let mut observed = BTreeMap::from([
        ("exists".to_owned(), serde_json::Value::Bool(true)),
        ("draft".to_owned(), serde_json::Value::Bool(false)),
        (
            "tag_name".to_owned(),
            snapshot
                .get("tag_name")
                .cloned()
                .unwrap_or(serde_json::Value::String(String::new())),
        ),
        (
            "assets".to_owned(),
            serde_json::Value::Array(observed_assets.clone()),
        ),
    ]);

    if let Some(manifest) = manifest.filter(|items| !items.is_empty()) {
        let by_name = observed_assets
            .iter()
            .filter_map(|asset| {
                let name = asset.get("name")?.as_str()?.to_owned();
                Some((name, asset))
            })
            .collect::<BTreeMap<_, _>>();

        let mut missing = Vec::new();
        let mut not_uploaded = Vec::new();
        for name in manifest {
            match by_name.get(name) {
                None => missing.push(serde_json::Value::String(name.clone())),
                Some(asset)
                    if asset.get("state").and_then(serde_json::Value::as_str)
                        != Some("uploaded") =>
                {
                    not_uploaded.push(serde_json::Value::String(name.clone()));
                }
                Some(_) => {}
            }
        }
        observed.insert(
            "manifest".to_owned(),
            serde_json::Value::Array(
                manifest
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        observed.insert(
            "missing".to_owned(),
            serde_json::Value::Array(missing.clone()),
        );
        observed.insert(
            "not_uploaded".to_owned(),
            serde_json::Value::Array(not_uploaded.clone()),
        );
        return Ok(TruthResult {
            matched: missing.is_empty() && not_uploaded.is_empty(),
            observed,
        });
    }

    let matched = observed_assets
        .iter()
        .any(|asset| asset.get("state").and_then(serde_json::Value::as_str) == Some("uploaded"));
    Ok(TruthResult { matched, observed })
}

/// Evaluate whether a PR is green under classic required-check semantics.
pub fn evaluate_pr_green(
    snapshot: Option<&serde_json::Value>,
) -> Result<TruthResult, Box<dyn std::error::Error>> {
    let snapshot = snapshot.ok_or_else(|| InvalidInputError("PR not found".to_owned()))?;
    let merge_state = snapshot
        .get("mergeStateStatus")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    if merge_state.contains("RULESET") || merge_queue_required(snapshot) {
        return Err(Box::new(UnsupportedScopeError(
            "Rulesets / merge-queue governance isn't supported by `shipyard wait pr --state green` yet — see governance/profiles.py.".to_owned(),
        )));
    }
    if snapshot.get("_required_checks_known") == Some(&serde_json::Value::Bool(false)) {
        return Err(Box::new(UnsupportedScopeError(
            "GitHub required-check classification is unavailable; refusing to report the PR green"
                .to_owned(),
        )));
    }

    let rollup = snapshot
        .get("statusCheckRollup")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut checks = evaluate_pr_check_rollup(rollup);

    if checks.required_entries.is_empty() {
        checks.all_required_pass = snapshot
            .get("mergeable")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .eq_ignore_ascii_case("MERGEABLE")
            && merge_state == "CLEAN";
    }

    Ok(TruthResult {
        matched: checks.all_required_pass && !checks.any_still_waiting,
        observed: pr_green_observed(snapshot, merge_state, checks),
    })
}

/// Whether the PR is governed by a merge queue, derived from the
/// `mergeStateStatus` field.
///
/// This is the ONLY merge-queue accessor that works against a `gh pr view
/// --json` snapshot. `mergeStateStatus == "MERGE_QUEUED"` is a scalar the
/// `gh` JSON surface can fetch. The GraphQL `mergeQueueEntry` connection —
/// which would name the queue and the PR's position — is NOT a field `gh pr
/// view --json` can request, so any accessor reading `snapshot["mergeQueueEntry"]`
/// returns null/false permanently. Such accessors are worse than absent: an
/// eviction watcher built on them would read "not in queue" forever and strand
/// a queued PR. Do not add them; ride on `mergeStateStatus` alone.
pub fn merge_queue_required(snapshot: &serde_json::Value) -> bool {
    snapshot
        .get("mergeStateStatus")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .eq_ignore_ascii_case("MERGE_QUEUED")
}

struct PrGreenChecks {
    required_entries: Vec<serde_json::Value>,
    advisory_entries: Vec<serde_json::Value>,
    all_required_pass: bool,
    any_still_waiting: bool,
}

fn evaluate_pr_check_rollup(rollup: Vec<serde_json::Value>) -> PrGreenChecks {
    let mut checks = PrGreenChecks {
        required_entries: Vec::new(),
        advisory_entries: Vec::new(),
        all_required_pass: true,
        any_still_waiting: false,
    };

    for entry in rollup {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let observed = observed_pr_check(entry);
        if !observed.required {
            checks.advisory_entries.push(observed.value);
            continue;
        }
        checks.required_entries.push(observed.value);
        if observed.waiting {
            checks.any_still_waiting = true;
            checks.all_required_pass = false;
        } else if !observed.passing {
            checks.all_required_pass = false;
        }
    }
    checks
}

struct ObservedPrCheck {
    value: serde_json::Value,
    required: bool,
    waiting: bool,
    passing: bool,
}

fn observed_pr_check(entry: &serde_json::Map<String, serde_json::Value>) -> ObservedPrCheck {
    let name = entry
        .get("name")
        .or_else(|| entry.get("context"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    let state = upper_entry_value(entry, "state");
    let conclusion = upper_entry_value(entry, "conclusion");
    let required = entry
        .get("isRequired")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let waiting = STILL_WAITING_STATES.contains(&state.as_str())
        && !PASSING_CONCLUSIONS.contains(&conclusion.as_str());
    let passing = PASSING_CONCLUSIONS.contains(&conclusion.as_str())
        || PASSING_CONCLUSIONS.contains(&state.as_str());

    ObservedPrCheck {
        value: serde_json::json!({
            "name": name,
            "state": nullable_uppercase(state),
            "conclusion": nullable_uppercase(conclusion),
            "required": required,
        }),
        required,
        waiting,
        passing,
    }
}

fn upper_entry_value(entry: &serde_json::Map<String, serde_json::Value>, key: &str) -> String {
    entry
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase()
}

fn nullable_uppercase(value: String) -> serde_json::Value {
    if value.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(value)
    }
}

fn pr_green_observed(
    snapshot: &serde_json::Value,
    merge_state: String,
    checks: PrGreenChecks,
) -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        (
            "pr".to_owned(),
            snapshot
                .get("number")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ),
        (
            "head_sha".to_owned(),
            snapshot
                .get("headRefOid")
                .cloned()
                .unwrap_or(serde_json::Value::String(String::new())),
        ),
        (
            "merge_state_status".to_owned(),
            nullable_uppercase(merge_state),
        ),
        (
            "checks".to_owned(),
            serde_json::Value::Array(checks.required_entries),
        ),
        (
            "advisory".to_owned(),
            serde_json::Value::Array(checks.advisory_entries),
        ),
    ])
}

/// Evaluate whether a PR reached `merged` or `closed`.
pub fn evaluate_pr_state(
    snapshot: Option<&serde_json::Value>,
    target_state: &str,
) -> Result<TruthResult, InvalidInputError> {
    let snapshot = snapshot.ok_or_else(|| InvalidInputError("PR not found".to_owned()))?;
    let state = snapshot
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    let merged = snapshot
        .get("merged")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(state == "MERGED");
    let matched = match target_state {
        "merged" => merged,
        "closed" => state == "CLOSED" || state == "MERGED",
        other => return Err(InvalidInputError(format!("unknown target state {other:?}"))),
    };
    Ok(TruthResult {
        matched,
        observed: BTreeMap::from([
            (
                "pr".to_owned(),
                snapshot
                    .get("number")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            ),
            ("state".to_owned(), serde_json::Value::String(state)),
            ("merged".to_owned(), serde_json::Value::Bool(merged)),
        ]),
    })
}

/// Evaluate whether a workflow run reached a terminal state.
pub fn evaluate_run(
    snapshot: Option<&serde_json::Value>,
    require_success: bool,
) -> Result<TruthResult, Box<dyn std::error::Error>> {
    let snapshot = snapshot.ok_or_else(|| InvalidInputError("run not found".to_owned()))?;
    let status = snapshot
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let conclusion = snapshot
        .get("conclusion")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let observed = BTreeMap::from([
        (
            "run_id".to_owned(),
            snapshot
                .get("databaseId")
                .or_else(|| snapshot.get("id"))
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ),
        (
            "status".to_owned(),
            serde_json::Value::String(status.clone()),
        ),
        (
            "conclusion".to_owned(),
            if conclusion.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(conclusion.clone())
            },
        ),
    ]);

    if !RUN_TERMINAL_STATUSES.contains(&status.as_str()) {
        return Ok(TruthResult {
            matched: false,
            observed,
        });
    }
    if !require_success {
        return Ok(TruthResult {
            matched: true,
            observed,
        });
    }
    if conclusion == "success" {
        return Ok(TruthResult {
            matched: true,
            observed,
        });
    }

    Err(Box::new(RunFailedFastError { observed }))
}

#[cfg(test)]
mod tests {
    use super::{
        InvalidInputError, RunFailedFastError, UnsupportedScopeError, evaluate_pr_green,
        evaluate_pr_state, evaluate_release, evaluate_run, merge_queue_required,
    };

    fn rollup_entry(
        name: &str,
        conclusion: &str,
        state: &str,
        required: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "conclusion": conclusion,
            "state": state,
            "isRequired": required,
        })
    }

    #[test]
    fn release_missing_snapshot_is_not_matched() {
        let result = evaluate_release(None, None).expect("release");
        assert!(!result.matched);
        assert_eq!(result.observed["exists"], serde_json::Value::Bool(false));
    }

    #[test]
    fn release_manifest_requires_all_assets_uploaded() {
        let snapshot = serde_json::json!({
            "tag_name": "v1",
            "draft": false,
            "assets": [
                {"name": "linux", "state": "uploaded", "size": 10},
                {"name": "darwin", "state": "starter", "size": 0}
            ]
        });
        let pending = evaluate_release(
            Some(&snapshot),
            Some(&["linux".to_owned(), "darwin".to_owned()]),
        )
        .expect("release");
        assert!(!pending.matched);
        assert_eq!(
            pending.observed["not_uploaded"],
            serde_json::json!(["darwin"])
        );
    }

    #[test]
    fn pr_green_matches_when_all_required_pass() {
        let snapshot = serde_json::json!({
            "number": 151,
            "headRefOid": "abc123",
            "state": "OPEN",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": [
                rollup_entry("Linux", "SUCCESS", "COMPLETED", true),
                rollup_entry("Windows", "SUCCESS", "COMPLETED", true),
                rollup_entry("macOS", "SUCCESS", "COMPLETED", true)
            ]
        });
        let result = evaluate_pr_green(Some(&snapshot)).expect("green");
        assert!(result.matched);
        assert_eq!(result.observed["head_sha"], "abc123");
        assert_eq!(
            result.observed["checks"].as_array().expect("checks").len(),
            3
        );
    }

    #[test]
    fn pr_green_ignores_advisory_failures() {
        let snapshot = serde_json::json!({
            "number": 1,
            "headRefOid": "x",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": [
                rollup_entry("Required", "SUCCESS", "COMPLETED", true),
                rollup_entry("Coverage", "FAILURE", "COMPLETED", false)
            ]
        });
        let result = evaluate_pr_green(Some(&snapshot)).expect("green");
        assert!(result.matched);
        assert_eq!(
            result.observed["checks"].as_array().expect("checks").len(),
            1
        );
        assert_eq!(
            result.observed["advisory"]
                .as_array()
                .expect("advisory")
                .len(),
            1
        );
    }

    #[test]
    fn pr_green_fails_closed_when_required_check_classification_is_unknown() {
        let snapshot = serde_json::json!({
            "number": 1,
            "headRefOid": "x",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": [],
            "_required_checks_known": false
        });
        let error = evaluate_pr_green(Some(&snapshot)).expect_err("unknown required checks");
        assert!(error.downcast_ref::<UnsupportedScopeError>().is_some());
    }

    #[test]
    fn pr_green_treats_unclassified_rollup_entries_as_required() {
        let snapshot = serde_json::json!({
            "number": 1,
            "headRefOid": "x",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": [{
                "name": "required but unannotated",
                "status": "COMPLETED",
                "conclusion": "FAILURE"
            }]
        });
        let result = evaluate_pr_green(Some(&snapshot)).expect("green evaluation");
        assert!(!result.matched);
        assert_eq!(result.observed["checks"][0]["required"], true);
    }

    #[test]
    fn pr_green_accepts_passing_required_commit_status_state() {
        let snapshot = serde_json::json!({
            "number": 1,
            "headRefOid": "x",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": [{
                "context": "required-status",
                "state": "SUCCESS",
                "isRequired": true
            }],
            "_required_checks_known": true
        });
        assert!(evaluate_pr_green(Some(&snapshot)).expect("green").matched);
    }

    #[test]
    fn pr_green_does_not_match_when_authoritative_required_context_is_missing() {
        let snapshot = serde_json::json!({
            "number": 1,
            "headRefOid": "x",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": [
                rollup_entry("present", "SUCCESS", "SUCCESS", true),
                rollup_entry("missing", "", "PENDING", true)
            ],
            "_required_checks_known": true
        });
        let result = evaluate_pr_green(Some(&snapshot)).expect("green evaluation");
        assert!(!result.matched);
        assert_eq!(result.observed["checks"].as_array().unwrap().len(), 2);
        assert_eq!(result.observed["checks"][1]["name"], "missing");
        assert_eq!(result.observed["checks"][1]["state"], "PENDING");
    }

    #[test]
    fn pr_green_with_no_required_checks_still_requires_clean_merge_state() {
        let snapshot = serde_json::json!({
            "number": 1,
            "headRefOid": "x",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "BLOCKED",
            "statusCheckRollup": [],
            "_required_checks_known": true
        });
        assert!(!evaluate_pr_green(Some(&snapshot)).expect("green").matched);
    }

    #[test]
    fn pr_green_rulesets_raise_unsupported_scope() {
        let snapshot = serde_json::json!({
            "number": 1,
            "headRefOid": "x",
            "mergeable": "BLOCKED",
            "mergeStateStatus": "BLOCKED_BY_RULESET",
            "statusCheckRollup": []
        });
        let error = evaluate_pr_green(Some(&snapshot)).expect_err("unsupported");
        assert!(error.downcast_ref::<UnsupportedScopeError>().is_some());
    }

    #[test]
    fn merge_queue_required_rides_on_merge_state_status() {
        // MERGE_QUEUED is reported through the fetchable `mergeStateStatus`
        // scalar — note there is NO `mergeQueueEntry` field here, mirroring a
        // real `gh pr view --json` snapshot.
        let queued = serde_json::json!({
            "number": 7,
            "mergeStateStatus": "MERGE_QUEUED"
        });
        assert!(merge_queue_required(&queued));

        // Case-insensitive, matching evaluate_pr_green's uppercasing.
        let queued_lower = serde_json::json!({ "mergeStateStatus": "merge_queued" });
        assert!(merge_queue_required(&queued_lower));

        let clean = serde_json::json!({ "mergeStateStatus": "CLEAN" });
        assert!(!merge_queue_required(&clean));

        // Missing field must be treated as not-queued, never panic.
        let empty = serde_json::json!({ "number": 7 });
        assert!(!merge_queue_required(&empty));
    }

    #[test]
    fn merge_queue_required_ignores_unfetchable_merge_queue_entry() {
        // Negative control for the deleted dead accessors: `mergeQueueEntry`
        // is a GraphQL-only connection `gh pr view --json` cannot fetch, so it
        // must NOT drive the verdict. A snapshot that (impossibly) carries a
        // populated `mergeQueueEntry` while `mergeStateStatus` is CLEAN must
        // still report not-queued. An accessor reading `mergeQueueEntry`
        // (round 1's in_merge_queue / merge_queue_state) would return true here.
        let bogus = serde_json::json!({
            "mergeStateStatus": "CLEAN",
            "mergeQueueEntry": { "position": 3, "mergeQueue": { "id": "MQ_x" } }
        });
        assert!(!merge_queue_required(&bogus));
    }

    #[test]
    fn pr_green_merge_queued_is_not_terminal_success() {
        // A green-but-queued PR must stay a loud, non-terminal signal — never
        // matched:true (which the CLI maps to exit 0 and a `wait pr --state
        // green && treat-as-landed` script reads as "landed"). Round 1's
        // regression turned this into terminal success; this pins it back to
        // the UnsupportedScope error (exit 7).
        let snapshot = serde_json::json!({
            "number": 42,
            "headRefOid": "deadbeef",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "MERGE_QUEUED",
            "statusCheckRollup": [
                rollup_entry("Linux", "SUCCESS", "COMPLETED", true),
                rollup_entry("macOS", "SUCCESS", "COMPLETED", true)
            ]
        });
        let result = evaluate_pr_green(Some(&snapshot));
        let error = result.expect_err("green-but-queued must not report terminal success");
        assert!(
            error.downcast_ref::<UnsupportedScopeError>().is_some(),
            "queued PR must surface UnsupportedScope, not a silent matched:true"
        );
    }

    #[test]
    fn pr_green_non_merge_queue_evaluates_unchanged() {
        // Regression guard: a repo not under merge-queue governance must
        // evaluate byte-identically to before — all required green -> matched,
        // and merge_queue_required is false so no code path diverges.
        let snapshot = serde_json::json!({
            "number": 99,
            "headRefOid": "cafe",
            "state": "OPEN",
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": [
                rollup_entry("Linux", "SUCCESS", "COMPLETED", true),
                rollup_entry("macOS", "SUCCESS", "COMPLETED", true)
            ]
        });
        assert!(!merge_queue_required(&snapshot));
        let result = evaluate_pr_green(Some(&snapshot)).expect("green");
        assert!(result.matched);
        assert_eq!(
            result.observed["checks"].as_array().expect("checks").len(),
            2
        );
    }

    #[test]
    fn pr_state_handles_merged_and_closed() {
        let merged = serde_json::json!({"number": 1, "state": "CLOSED", "merged": true});
        assert!(
            evaluate_pr_state(Some(&merged), "merged")
                .expect("merged")
                .matched
        );
        assert!(
            evaluate_pr_state(Some(&merged), "closed")
                .expect("closed")
                .matched
        );

        // `gh pr view --json` has no `merged` field; its GraphQL-shaped
        // snapshot reports the terminal state directly.
        let graphql_merged = serde_json::json!({"number": 2, "state": "MERGED"});
        let result = evaluate_pr_state(Some(&graphql_merged), "merged").expect("merged state");
        assert!(result.matched);
        assert_eq!(result.observed["merged"], true);
    }

    #[test]
    fn run_success_fails_fast_on_terminal_wrong_conclusion() {
        let snapshot = serde_json::json!({
            "databaseId": 1,
            "status": "completed",
            "conclusion": "failure"
        });
        let error = evaluate_run(Some(&snapshot), true).expect_err("run failure");
        let error = error
            .downcast::<RunFailedFastError>()
            .expect("run failed fast error");
        assert_eq!(
            error
                .observed
                .get("conclusion")
                .and_then(serde_json::Value::as_str),
            Some("failure")
        );
    }

    #[test]
    fn run_non_terminal_is_not_matched() {
        let snapshot = serde_json::json!({
            "databaseId": 1,
            "status": "in_progress",
            "conclusion": null
        });
        let result = evaluate_run(Some(&snapshot), true).expect("run");
        assert!(!result.matched);
    }

    #[test]
    fn missing_pr_snapshot_raises_invalid_input() {
        let error = evaluate_pr_state(None, "merged").expect_err("missing pr");
        assert_eq!(error, InvalidInputError("PR not found".to_owned()));
    }
}
