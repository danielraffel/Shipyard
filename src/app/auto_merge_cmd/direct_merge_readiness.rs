//! Fail-closed GitHub readiness fence for classic client-side merges.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use super::{base_branch_from_value, head_sha_from_value, shas_match};
use crate::cloud::GitHubActions;
use crate::identity::RuntimeMode;
use crate::merge_steward::{
    RequiredCheck, StewardCheck, StewardCheckSource, selected_required_check,
};
use crate::ship_state::ShipState;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CheckIdentity {
    source: String,
    name: String,
    app_id: Option<u64>,
    check_run_id: Option<u64>,
    run_id: Option<u64>,
    observed_at: Option<String>,
    state: String,
    conclusion: String,
    required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Readiness {
    head_sha: String,
    base_branch: String,
    required_policy: Vec<(String, Option<u64>)>,
    checks: Vec<CheckIdentity>,
}

/// Read every current-head check/status page and authoritative policy surface.
/// Callers compare two observations and keep the second immediately adjacent
/// to the merge mutation.
pub(super) fn fetch(
    cwd: &Path,
    state: &ShipState,
    snapshot_file: Option<&Path>,
) -> Result<Readiness, String> {
    let snapshot = if let Some(path) = snapshot_file {
        std::fs::read_to_string(path)
            .map_err(|error| format!("failed to read direct-merge readiness snapshot: {error}"))
            .and_then(|contents| {
                serde_json::from_str::<Value>(&contents).map_err(|error| {
                    format!("direct-merge readiness snapshot is malformed: {error}")
                })
            })?
    } else {
        let actions =
            GitHubActions::from_cwd(RuntimeMode::Shipyard, cwd).with_repo_override(&state.repo);
        let policy = super::super::merge_steward_cmd::required_checks(
            &actions,
            &state.repo,
            &state.base_branch,
        )
        .map_err(|error| {
            format!(
                "GitHub required-check policy is unavailable or malformed for PR #{}: {error}",
                state.pr
            )
        })?;
        let checks = super::super::merge_steward_cmd::complete_checks_for_head(
            &actions,
            &state.repo,
            &state.head_sha,
        )
        .map_err(|error| {
            format!(
                "complete GitHub exact-head check/status discovery failed for PR #{}: {error}",
                state.pr
            )
        })?;
        // This identity read is deliberately last. The returned head/base are
        // part of the readiness digest compared immediately before mutation.
        let (live_head, live_base) = super::super::merge_steward_cmd::exact_pr_merge_identity(
            &actions,
            &state.repo,
            state.pr,
        )?;
        snapshot_from_observation(state, &policy, checks, &live_head, &live_base)?
    };
    classify(&snapshot, state)
}

/// Final readiness read for a built-in merge. Snapshot-backed check fixtures
/// still receive a live PR identity read, performed last, so they cannot
/// remove the head/base mutation fence.
pub(super) fn fetch_at_mutation_boundary(
    cwd: &Path,
    state: &ShipState,
    snapshot_file: Option<&Path>,
) -> Result<Readiness, String> {
    let Some(path) = snapshot_file else {
        return fetch(cwd, state, None);
    };
    let mut snapshot = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read direct-merge readiness snapshot: {error}"))
        .and_then(|contents| {
            serde_json::from_str::<Value>(&contents)
                .map_err(|error| format!("direct-merge readiness snapshot is malformed: {error}"))
        })?;
    let actions =
        GitHubActions::from_cwd(RuntimeMode::Shipyard, cwd).with_repo_override(&state.repo);
    let (live_head, live_base) =
        super::super::merge_steward_cmd::exact_pr_merge_identity(&actions, &state.repo, state.pr)?;
    snapshot["headRefOid"] = Value::String(live_head);
    snapshot["baseRefName"] = Value::String(live_base);
    classify(&snapshot, state)
}

pub(super) fn snapshot_declares_readiness(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
        .is_some_and(|snapshot| snapshot.get("_required_checks_known").is_some())
}

/// Make the complete readiness read, whose last sub-read is live PR identity,
/// the final remote authority observation before the caller mutates GitHub.
pub(super) fn confirm_at_mutation_boundary(
    expected: &Readiness,
    fetch_immediate: impl FnOnce() -> Result<Readiness, String>,
    pr: u64,
) -> Result<(), String> {
    let immediate = fetch_immediate()?;
    if expected != &immediate {
        return Err(format!(
            "GitHub exact-head check readiness changed before classic merge mutation for PR #{pr}; refusing merge"
        ));
    }
    Ok(())
}

fn snapshot_from_observation(
    state: &ShipState,
    policy: &[RequiredCheck],
    checks: Vec<StewardCheck>,
    live_head: &str,
    live_base: &str,
) -> Result<Value, String> {
    let checks = latest_checks(checks);
    // An authoritative empty policy cannot distinguish an intentionally
    // checkless repository from CI that has not materialized yet. Unattended
    // built-in direct merge requires positive readiness evidence; custom or
    // manual merge remains available for deliberately checkless repositories.
    if policy.is_empty() && checks.is_empty() {
        return Err(format!(
            "GitHub exposed neither required-check policy nor materialized exact-head checks for PR #{}; refusing direct merge before CI appears",
            state.pr
        ));
    }
    let mut selected_required = BTreeSet::new();
    for required in policy {
        let selected = selected_required_check(&checks, required).ok_or_else(|| {
            format!(
                "required GitHub check {:?} has not materialized for PR #{}; refusing direct merge",
                required.context, state.pr
            )
        })?;
        selected_required.insert(check_key(selected));
    }
    let rollup = checks
        .into_iter()
        .map(|check| {
            let required = selected_required.contains(&check_key(&check));
            let source = match check.source {
                StewardCheckSource::CheckRun => "check_run",
                StewardCheckSource::StatusContext => "status_context",
            };
            serde_json::json!({
                "source": source,
                "name": check.name,
                "app_id": check.app_id,
                "check_run_id": check.check_run_id,
                "run_id": check.run_id,
                "observed_at": check.observed_at,
                "state": check.status,
                "conclusion": check.conclusion,
                "isRequired": required,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "headRefOid": live_head,
        "baseRefName": live_base,
        "_required_checks_known": true,
        "_required_check_policy": policy.iter().map(|required| serde_json::json!({
            "context": required.context,
            "app_id": required.app_id,
        })).collect::<Vec<_>>(),
        "statusCheckRollup": rollup,
    }))
}

fn latest_checks(checks: Vec<StewardCheck>) -> Vec<StewardCheck> {
    let mut latest =
        BTreeMap::<(String, String, Option<u64>, Option<u64>, Option<u64>), StewardCheck>::new();
    for check in checks {
        let key = check_key(&check);
        let replace = latest.get(&key).is_none_or(|current| {
            if matches!(check.source, StewardCheckSource::CheckRun) {
                (check_is_active(&check), check_recency(&check))
                    > (check_is_active(current), check_recency(current))
            } else {
                check_recency(&check) > check_recency(current)
            }
        });
        if replace {
            latest.insert(key, check);
        }
    }
    latest.into_values().collect()
}

fn check_is_active(check: &StewardCheck) -> bool {
    matches!(
        check.status.to_ascii_uppercase().as_str(),
        "QUEUED" | "PENDING" | "IN_PROGRESS" | "WAITING" | "REQUESTED" | "EXPECTED"
    )
}

fn check_key(check: &StewardCheck) -> (String, String, Option<u64>, Option<u64>, Option<u64>) {
    let source = match check.source {
        StewardCheckSource::CheckRun => "check_run",
        StewardCheckSource::StatusContext => "status_context",
    };
    let attempt_run_id = matches!(check.source, StewardCheckSource::CheckRun)
        .then_some(check.run_id)
        .flatten();
    let check_run_id = matches!(check.source, StewardCheckSource::CheckRun)
        .then_some(check.check_run_id)
        .flatten();
    (
        source.to_owned(),
        check.name.to_ascii_lowercase(),
        check.app_id,
        check_run_id,
        attempt_run_id,
    )
}

fn check_recency(check: &StewardCheck) -> (bool, &str, bool) {
    (
        check.observed_at.is_none() && !check.status.eq_ignore_ascii_case("COMPLETED"),
        check.observed_at.as_deref().unwrap_or_default(),
        check.status.eq_ignore_ascii_case("COMPLETED"),
    )
}

fn classify(snapshot: &Value, state: &ShipState) -> Result<Readiness, String> {
    let head_sha = head_sha_from_value(snapshot).ok_or_else(|| {
        format!(
            "GitHub check readiness omitted the exact head for PR #{}; refusing direct merge",
            state.pr
        )
    })?;
    if !shas_match(&head_sha, &state.head_sha) {
        return Err(format!(
            "GitHub check readiness head {head_sha} does not match validated head {}; refusing direct merge",
            state.head_sha
        ));
    }
    let base_branch = base_branch_from_value(snapshot).ok_or_else(|| {
        format!(
            "GitHub check readiness omitted the base branch for PR #{}; refusing direct merge",
            state.pr
        )
    })?;
    if base_branch != state.base_branch {
        return Err(format!(
            "GitHub check readiness base {base_branch} does not match validated base {}; refusing direct merge",
            state.base_branch
        ));
    }
    if snapshot.get("_required_checks_known") != Some(&Value::Bool(true)) {
        return Err(format!(
            "GitHub required-check policy is unavailable or malformed for PR #{}; refusing direct merge",
            state.pr
        ));
    }
    let required_policy = parse_policy(snapshot, state.pr)?;
    let rollup = snapshot
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!(
                "GitHub check rollup is unavailable or malformed for PR #{}; refusing direct merge",
                state.pr
            )
        })?;
    let mut checks = rollup
        .iter()
        .map(|entry| parse_check(entry, state.pr))
        .collect::<Result<Vec<_>, String>>()?;
    if required_policy.is_empty() && checks.is_empty() {
        return Err(format!(
            "GitHub exposed neither required-check policy nor materialized exact-head checks for PR #{}; refusing direct merge before CI appears",
            state.pr
        ));
    }
    if required_policy.is_empty() {
        let effective_checks = effective_terminal_checks(&checks, state.pr)?;
        if effective_checks
            .iter()
            .any(|check| check_identity_is_terminal_failure(check))
        {
            return Err(format!(
                "GitHub has no required-check policy and at least one exact-head check failed for PR #{}; refusing direct merge without explicit signed policy",
                state.pr
            ));
        }
        if !effective_checks
            .iter()
            .any(|check| check_identity_is_success(check))
        {
            return Err(format!(
                "GitHub has no required-check policy and no passing exact-head check for PR #{}; refusing direct merge without positive evidence",
                state.pr
            ));
        }
    }
    checks.sort();
    Ok(Readiness {
        head_sha,
        base_branch,
        required_policy,
        checks,
    })
}

fn effective_terminal_checks(
    checks: &[CheckIdentity],
    pr: u64,
) -> Result<Vec<&CheckIdentity>, String> {
    let mut latest = BTreeMap::<(String, String, Option<u64>), &CheckIdentity>::new();
    for check in checks {
        let key = (
            check.source.clone(),
            check.name.to_ascii_lowercase(),
            check.app_id,
        );
        let replace = match latest.get(&key) {
            None => true,
            Some(current) if check.source == "check_run" => {
                let Some(check_run_id) = check.check_run_id else {
                    return Err(format!(
                        "duplicate GitHub check {:?} lacks immutable check-run ID for PR #{pr}; refusing ambiguous no-policy readiness",
                        check.name
                    ));
                };
                let Some(current_check_run_id) = current.check_run_id else {
                    return Err(format!(
                        "duplicate GitHub check {:?} lacks immutable check-run ID for PR #{pr}; refusing ambiguous no-policy readiness",
                        current.name
                    ));
                };
                check_run_id > current_check_run_id
            }
            Some(current) => {
                let Some(observed_at) = check.observed_at.as_deref() else {
                    return Err(format!(
                        "duplicate GitHub status {:?} lacks observation time for PR #{pr}; refusing ambiguous no-policy readiness",
                        check.name
                    ));
                };
                let Some(current_observed_at) = current.observed_at.as_deref() else {
                    return Err(format!(
                        "duplicate GitHub status {:?} lacks observation time for PR #{pr}; refusing ambiguous no-policy readiness",
                        current.name
                    ));
                };
                observed_at > current_observed_at
            }
        };
        if replace {
            latest.insert(key, check);
        }
    }
    Ok(latest.into_values().collect())
}

fn check_identity_is_success(check: &CheckIdentity) -> bool {
    check.conclusion == "SUCCESS" || check.state == "SUCCESS"
}

fn check_identity_is_terminal_failure(check: &CheckIdentity) -> bool {
    matches!(
        check.conclusion.as_str(),
        "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STALE" | "STARTUP_FAILURE"
    ) || matches!(
        check.state.as_str(),
        "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT"
    )
}

fn parse_policy(snapshot: &Value, pr: u64) -> Result<Vec<(String, Option<u64>)>, String> {
    let mut policy = snapshot
        .get("_required_check_policy")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!(
                "GitHub required-check policy digest is unavailable or malformed for PR #{pr}; refusing direct merge"
            )
        })?
        .iter()
        .map(|entry| {
            let context = entry
                .get("context")
                .and_then(Value::as_str)
                .filter(|context| !context.is_empty())
                .ok_or_else(|| "required-check policy contains an invalid context".to_owned())?;
            let app_id = match entry.get("app_id") {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.as_u64().ok_or_else(|| {
                    "required-check policy contains an invalid app_id".to_owned()
                })?),
            };
            Ok((context.to_owned(), app_id))
        })
        .collect::<Result<Vec<_>, String>>()?;
    policy.sort();
    Ok(policy)
}

fn parse_check(entry: &Value, pr: u64) -> Result<CheckIdentity, String> {
    let entry = entry.as_object().ok_or_else(|| {
        format!(
            "GitHub check rollup contains a malformed entry for PR #{pr}; refusing direct merge"
        )
    })?;
    let name = entry
        .get("name")
        .or_else(|| entry.get("context"))
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "GitHub check rollup contains an unnamed entry for PR #{pr}; refusing direct merge"
            )
        })?
        .to_owned();
    let state = entry
        .get("state")
        .or_else(|| entry.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    let conclusion = entry
        .get("conclusion")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    let required = entry
        .get("isRequired")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            format!(
                "GitHub check {name:?} lacks required-policy classification for PR #{pr}; refusing direct merge"
            )
        })?;
    verify_terminal_check(&name, &state, &conclusion, required, pr)?;
    let source = match entry.get("source") {
        None => "snapshot".to_owned(),
        Some(value) => value
            .as_str()
            .filter(|source| matches!(*source, "check_run" | "status_context" | "snapshot"))
            .ok_or_else(|| {
                format!(
                    "GitHub check {name:?} has a malformed source for PR #{pr}; refusing direct merge"
                )
            })?
            .to_owned(),
    };
    let app_id = match entry.get("app_id") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            format!(
                "GitHub check {name:?} has a malformed app_id for PR #{pr}; refusing direct merge"
            )
        })?),
    };
    let run_id = match entry.get("run_id") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            format!(
                "GitHub check {name:?} has a malformed run_id for PR #{pr}; refusing direct merge"
            )
        })?),
    };
    let check_run_id = match entry.get("check_run_id") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            format!(
                "GitHub check {name:?} has a malformed check_run_id for PR #{pr}; refusing direct merge"
            )
        })?),
    };
    let observed_at = match entry.get("observed_at") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|observed_at| !observed_at.trim().is_empty())
                .ok_or_else(|| {
                    format!(
                        "GitHub check {name:?} has a malformed observed_at for PR #{pr}; refusing direct merge"
                    )
                })?
                .to_owned(),
        ),
    };
    Ok(CheckIdentity {
        source,
        name,
        app_id,
        check_run_id,
        run_id,
        observed_at,
        state,
        conclusion,
        required,
    })
}

fn verify_terminal_check(
    name: &str,
    state: &str,
    conclusion: &str,
    required: bool,
    pr: u64,
) -> Result<(), String> {
    let active = matches!(
        state,
        "QUEUED" | "PENDING" | "IN_PROGRESS" | "WAITING" | "REQUESTED" | "EXPECTED"
    );
    let passing = matches!(conclusion, "SUCCESS" | "NEUTRAL" | "SKIPPED")
        || matches!(state, "SUCCESS" | "NEUTRAL" | "SKIPPED");
    let terminal_failure = matches!(
        conclusion,
        "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STALE" | "STARTUP_FAILURE"
    ) || matches!(state, "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT");
    if active {
        return Err(format!(
            "GitHub check {name:?} is still {state}; refusing direct merge for PR #{pr}"
        ));
    }
    if required && !passing {
        return Err(format!(
            "required GitHub check {name:?} is not passing (state={state:?}, conclusion={conclusion:?}); refusing direct merge for PR #{pr}"
        ));
    }
    if !required && !passing && !terminal_failure {
        return Err(format!(
            "advisory GitHub check {name:?} has ambiguous terminal state (state={state:?}, conclusion={conclusion:?}); refusing direct merge for PR #{pr}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ShipState {
        ShipState::new(
            533,
            "danielraffel/Shipyard",
            "fix/guardian",
            "main",
            "b78200f7d26171d46450f5eec3850e803f458b94",
            "policy",
        )
    }

    fn snapshot(checks: &Value) -> Value {
        serde_json::json!({
            "headRefOid": "b78200f7d26171d46450f5eec3850e803f458b94",
            "baseRefName": "main",
            "_required_checks_known": true,
            "_required_check_policy": [{"context":"Linux","app_id":null}],
            "statusCheckRollup": checks.clone(),
        })
    }

    #[test]
    fn pending_advisory_blocks_after_required_checks_pass() {
        let value = snapshot(&serde_json::json!([
            {"name":"Linux","state":"COMPLETED","conclusion":"SUCCESS","isRequired":true},
            {"name":"Windows","state":"IN_PROGRESS","conclusion":null,"isRequired":false}
        ]));
        let error = classify(&value, &state()).expect_err("advisory is live");
        assert!(error.contains("Windows") && error.contains("IN_PROGRESS"));
    }

    #[test]
    fn terminal_advisory_failure_is_nonblocking_under_explicit_required_policy() {
        let value = snapshot(&serde_json::json!([
            {"name":"Linux","state":"COMPLETED","conclusion":"SUCCESS","isRequired":true},
            {"name":"Coverage","state":"COMPLETED","conclusion":"FAILURE","isRequired":false}
        ]));
        assert_eq!(classify(&value, &state()).expect("ready").checks.len(), 2);
    }

    #[test]
    fn unavailable_required_policy_fails_closed() {
        let mut value = snapshot(&serde_json::json!([
            {"name":"Coverage","state":"COMPLETED","conclusion":"SUCCESS","isRequired":false}
        ]));
        value["_required_checks_known"] = Value::Bool(false);
        assert!(
            classify(&value, &state())
                .expect_err("unavailable policy")
                .contains("required-check policy is unavailable")
        );
    }

    #[test]
    fn empty_policy_and_zero_materialized_checks_refuse_before_ci_appears() {
        let mut value = snapshot(&serde_json::json!([]));
        value["_required_check_policy"] = serde_json::json!([]);
        assert!(
            classify(&value, &state())
                .expect_err("zero-evidence readiness")
                .contains("before CI appears")
        );
    }

    #[test]
    fn empty_policy_with_only_red_advisory_evidence_refuses() {
        let mut value = snapshot(&serde_json::json!([
            {"name":"Windows","state":"COMPLETED","conclusion":"FAILURE","isRequired":false}
        ]));
        value["_required_check_policy"] = serde_json::json!([]);
        assert!(
            classify(&value, &state())
                .expect_err("unsigned all-red evidence")
                .contains("without explicit signed policy")
        );
    }

    #[test]
    fn empty_policy_with_clean_passing_evidence_is_ready() {
        let mut value = snapshot(&serde_json::json!([
            {"name":"Linux","state":"COMPLETED","conclusion":"SUCCESS","isRequired":false}
        ]));
        value["_required_check_policy"] = serde_json::json!([]);
        assert_eq!(
            classify(&value, &state())
                .expect("positive evidence")
                .checks
                .len(),
            1
        );
    }

    #[test]
    fn empty_policy_requires_success_not_skipped_or_neutral() {
        for conclusion in ["SKIPPED", "NEUTRAL"] {
            let mut value = snapshot(&serde_json::json!([
                {"name":"Linux","state":"COMPLETED","conclusion":conclusion,"isRequired":false}
            ]));
            value["_required_check_policy"] = serde_json::json!([]);
            assert!(
                classify(&value, &state())
                    .expect_err("non-success is not positive evidence")
                    .contains("without positive evidence")
            );
        }
    }

    #[test]
    fn empty_policy_with_mixed_pass_and_failure_refuses() {
        let mut value = snapshot(&serde_json::json!([
            {"name":"Linux","state":"COMPLETED","conclusion":"SUCCESS","isRequired":false},
            {"name":"Windows","state":"COMPLETED","conclusion":"FAILURE","isRequired":false}
        ]));
        value["_required_check_policy"] = serde_json::json!([]);
        assert!(
            classify(&value, &state())
                .expect_err("mixed unsigned evidence")
                .contains("without explicit signed policy")
        );
    }

    #[test]
    fn empty_policy_uses_successful_terminal_rerun_over_older_failure() {
        let mut value = snapshot(&serde_json::json!([
            {
                "source":"check_run","name":"Windows","app_id":7,"check_run_id":101,
                "state":"COMPLETED","conclusion":"FAILURE","isRequired":false,
                "observed_at":"2026-09-01T10:11:44Z"
            },
            {
                "source":"check_run","name":"Windows","app_id":7,"check_run_id":102,
                "state":"COMPLETED","conclusion":"SUCCESS","isRequired":false,
                "observed_at":"2026-09-01T10:13:14Z"
            }
        ]));
        value["_required_check_policy"] = serde_json::json!([]);
        assert_eq!(
            classify(&value, &state())
                .expect("green rerun")
                .checks
                .len(),
            2
        );
    }

    #[test]
    fn empty_policy_uses_newer_attempt_id_when_completion_order_is_reversed() {
        let mut value = snapshot(&serde_json::json!([
            {
                "source":"check_run","name":"Windows","app_id":7,"check_run_id":101,
                "state":"COMPLETED","conclusion":"SUCCESS","isRequired":false,
                "observed_at":"2026-09-01T10:15:00Z"
            },
            {
                "source":"check_run","name":"Windows","app_id":7,"check_run_id":102,
                "state":"COMPLETED","conclusion":"FAILURE","isRequired":false,
                "observed_at":"2026-09-01T10:13:14Z"
            }
        ]));
        value["_required_check_policy"] = serde_json::json!([]);
        assert!(
            classify(&value, &state())
                .expect_err("newer failed attempt")
                .contains("without explicit signed policy")
        );
    }

    #[test]
    fn older_active_attempt_is_not_hidden_by_newer_terminal_attempt() {
        let checks = vec![
            StewardCheck {
                name: "Windows".to_owned(),
                source: StewardCheckSource::CheckRun,
                app_id: Some(7),
                check_run_id: Some(101),
                status: "IN_PROGRESS".to_owned(),
                conclusion: None,
                run_id: None,
                observed_at: Some("2026-09-01T10:11:44Z".to_owned()),
            },
            StewardCheck {
                name: "Windows".to_owned(),
                source: StewardCheckSource::CheckRun,
                app_id: Some(7),
                check_run_id: Some(102),
                status: "COMPLETED".to_owned(),
                conclusion: Some("SUCCESS".to_owned()),
                run_id: None,
                observed_at: Some("2026-09-01T10:13:14Z".to_owned()),
            },
        ];
        let latest = latest_checks(checks);
        assert_eq!(latest.len(), 2);
        assert!(latest.iter().any(|check| check.status == "IN_PROGRESS"));
    }

    #[test]
    fn historical_pending_status_does_not_override_newer_terminal_status() {
        let checks = vec![
            StewardCheck {
                name: "legacy".to_owned(),
                source: StewardCheckSource::StatusContext,
                app_id: None,
                check_run_id: None,
                status: "IN_PROGRESS".to_owned(),
                conclusion: None,
                run_id: Some(1),
                observed_at: Some("2026-09-01T10:11:44Z".to_owned()),
            },
            StewardCheck {
                name: "legacy".to_owned(),
                source: StewardCheckSource::StatusContext,
                app_id: None,
                check_run_id: None,
                status: "COMPLETED".to_owned(),
                conclusion: Some("SUCCESS".to_owned()),
                run_id: Some(2),
                observed_at: Some("2026-09-01T10:13:14Z".to_owned()),
            },
        ];
        let latest = latest_checks(checks);
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].status, "COMPLETED");
    }

    #[test]
    fn final_readiness_detects_live_base_mutation() {
        let expected = classify(
            &snapshot(&serde_json::json!([
                {"name":"Linux","state":"COMPLETED","conclusion":"SUCCESS","isRequired":true}
            ])),
            &state(),
        )
        .expect("initial readiness");
        let mut observed = expected.clone();
        observed.base_branch = "release".to_owned();
        let order = std::cell::RefCell::new(Vec::new());
        let error = confirm_at_mutation_boundary(
            &expected,
            || {
                order.borrow_mut().push("final-readiness");
                Ok(observed)
            },
            533,
        )
        .expect_err("mutated readiness");
        assert_eq!(*order.borrow(), ["final-readiness"]);
        assert!(error.contains("readiness changed"));
    }

    #[test]
    fn malformed_check_identity_fails_closed() {
        let value = snapshot(&serde_json::json!([
            {
                "name":"Linux",
                "source":"check_run",
                "app_id":"not-a-number",
                "state":"COMPLETED",
                "conclusion":"SUCCESS",
                "isRequired":true
            }
        ]));
        assert!(
            classify(&value, &state())
                .expect_err("malformed identity")
                .contains("malformed app_id")
        );
    }

    #[test]
    fn required_failure_blocks_direct_merge() {
        let value = snapshot(&serde_json::json!([
            {"name":"Linux","state":"COMPLETED","conclusion":"FAILURE","isRequired":true}
        ]));
        assert!(
            classify(&value, &state())
                .expect_err("required failure")
                .contains("required GitHub check")
        );
    }

    #[test]
    fn readiness_is_order_independent_but_identity_sensitive() {
        let left = snapshot(&serde_json::json!([
            {"name":"Linux","state":"COMPLETED","conclusion":"SUCCESS","isRequired":true},
            {"name":"Coverage","state":"COMPLETED","conclusion":"FAILURE","isRequired":false}
        ]));
        let right = snapshot(&serde_json::json!([
            {"name":"Coverage","state":"COMPLETED","conclusion":"FAILURE","isRequired":false},
            {"name":"Linux","state":"COMPLETED","conclusion":"SUCCESS","isRequired":true}
        ]));
        let state = state();
        let left = classify(&left, &state).expect("left");
        let right = classify(&right, &state).expect("right");
        assert_eq!(left, right);
        let changed = snapshot(&serde_json::json!([
            {"name":"Linux","state":"COMPLETED","conclusion":"SUCCESS","isRequired":true}
        ]));
        assert_ne!(left, classify(&changed, &state).expect("changed"));
    }
}
