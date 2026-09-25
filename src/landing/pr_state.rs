//! `shipyard landing --pr <n>`: one pull request's merge-queue state.
//!
//! The landing model says how a repository merges work; this says where one
//! pull request stands in that mechanism right now, and what not to do about
//! it. The classification itself lives in [`crate::pr_queue_state`]; this
//! module only fetches the facts and renders them.

use std::io::Write;

use serde::Serialize;
use serde_json::Value;

use crate::cloud::GitHubActions;
use crate::pr_queue_state::{
    PR_QUEUE_STATE_QUERY, PrQueueReport, PrQueueState, REST_AUTO_MERGE_PREFACE,
    explain_pr_queue_state, same_head_requeue_allowed, same_head_requeue_cascades,
};
use crate::validation_signals::{self, PrValidationSignals};

/// Machine-readable envelope for one PR's queue state.
#[derive(Clone, Debug, Serialize)]
pub struct PrStateReport {
    /// Report generation, shared with the landing model.
    pub schema_version: u32,
    /// Fixed warning about the REST `auto_merge` trap.
    pub preface: String,
    /// `OWNER/REPO`.
    pub repo: String,
    /// Pull request number requested.
    pub pr: u64,
    /// The classification with every source fact.
    pub classification: PrQueueReport,
    /// What an agent should do next, derived from the classification.
    pub next_action: String,
    /// How much the head's required checks tested, and the receipt decisions
    /// of the pull request's merge group(s), read from the
    /// `shipyard-test-tier` / `shipyard-receipt-decision` annotation contract
    /// (see [`crate::validation_signals`]).
    pub validation: PrValidationSignals,
}

impl PrStateReport {
    /// Whether the classification could not be determined.
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self.classification.state, PrQueueState::Unknown { .. })
    }
}

/// Read one pull request's queue facts and classify them.
#[must_use]
pub fn gather(actions: &GitHubActions, repo: &str, pr: u64) -> PrStateReport {
    let classification = match read(actions, repo, pr) {
        Ok(value) => explain_pr_queue_state(&value),
        Err(detail) => explain_pr_queue_state(&serde_json::json!({
            "errors": [{"message": detail}]
        })),
    };
    let next_action = next_action(&classification.state, pr);
    let validation = validation_signals::gather_pr(
        &|args: &[String]| actions.run_gh(args).map_err(|error| error.to_string()),
        repo,
        pr,
    );
    PrStateReport {
        schema_version: super::SCHEMA_VERSION,
        preface: REST_AUTO_MERGE_PREFACE.to_owned(),
        repo: repo.to_owned(),
        pr,
        classification,
        next_action,
        validation,
    }
}

fn read(actions: &GitHubActions, repo: &str, pr: u64) -> Result<Value, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("repo `{repo}` is not OWNER/REPO"))?;
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={PR_QUEUE_STATE_QUERY}"),
            "-F".to_owned(),
            format!("owner={owner}"),
            "-F".to_owned(),
            format!("name={name}"),
            "-F".to_owned(),
            format!("number={pr}"),
        ])
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&raw).map_err(|error| format!("malformed GraphQL JSON: {error}"))
}

/// The one next step each class calls for.
///
/// Kept in agreement with `scripts/ghapp_queue_arm_guard.py`: every class the
/// guard refuses to arm is answered here with something other than arming.
#[must_use]
pub fn next_action(state: &PrQueueState, pr: u64) -> String {
    match state {
        PrQueueState::Merged => "Merged. Nothing to do.".to_owned(),
        PrQueueState::Closed => "Closed. Reopen it deliberately before landing it.".to_owned(),
        PrQueueState::Queued { position, .. } => format!(
            "Already in the merge queue{}. Nothing to do; do not re-arm auto-merge.",
            position.map_or_else(String::new, |position| format!(" at position {position}"))
        ),
        PrQueueState::ArmedNotQueued { enabled_at, .. } => format!(
            "Auto-merge is armed{}; the queue admits it once required checks pass. Nothing to do.",
            enabled_at
                .as_deref()
                .map_or_else(String::new, |at| format!(" since {at}"))
        ),
        PrQueueState::NeverArmed => {
            format!("Not armed. Land it with `shipyard ship --pr {pr}`.")
        }
        PrQueueState::Ejected {
            reason,
            new_head_since_removal: true,
            ..
        } => format!(
            "Removed from the queue ({reason}) and a new head has been pushed since. Re-land \
             it with \
             `shipyard ship --pr {pr}`."
        ),
        PrQueueState::Ejected { reason, at, .. } => {
            let at = at.as_deref().unwrap_or("an unknown time");
            if same_head_requeue_cascades(reason) {
                format!(
                    "Ejected for {reason} at {at} and the head has not changed. Re-enqueuing the \
                     same head under ALLGREEN fails its batch-mates: push a fix first, then \
                     `shipyard ship --pr {pr}`."
                )
            } else if same_head_requeue_allowed(reason) {
                format!(
                    "Removed from the queue ({reason}) at {at}: GitHub could not build the merge \
                     commit, which says nothing against this head. Re-land it with \
                     `shipyard ship --pr {pr}`."
                )
            } else {
                format!(
                    "Removed from the queue ({reason}) at {at} and the head has not changed. \
                     Confirm with whoever dequeued it before re-enqueuing with \
                     `shipyard ship --pr {pr}`."
                )
            }
        }
        PrQueueState::Unknown { detail } => {
            format!("UNKNOWN ({detail}). Do not act on this PR's queue state until it can be read.")
        }
    }
}

/// Write the machine-readable form.
pub fn write_json<W: Write>(stdout: &mut W, report: &PrStateReport) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(report)
        .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"));
    writeln!(stdout, "{text}")
}

/// Write the human-readable form.
pub fn write_human<W: Write>(stdout: &mut W, report: &PrStateReport) -> std::io::Result<()> {
    let classification = &report.classification;
    writeln!(stdout, "{}", report.preface)?;
    writeln!(stdout)?;
    writeln!(
        stdout,
        "PR #{} in {}: {}",
        report.pr,
        report.repo,
        classification.state.class().to_uppercase()
    )?;
    writeln!(stdout, "  {}", report.next_action)?;
    writeln!(stdout)?;
    write_validation(stdout, &report.validation)?;
    writeln!(stdout)?;
    writeln!(stdout, "HISTORY")?;
    match &classification.last_ejection {
        Some(ejection) => writeln!(
            stdout,
            "  last ejection            {} at {} (new head since: {})",
            ejection.reason,
            ejection.at.as_deref().unwrap_or("UNKNOWN"),
            if ejection.new_head_since { "yes" } else { "no" }
        )?,
        None => writeln!(stdout, "  last ejection            none visible")?,
    }
    writeln!(
        stdout,
        "  same-head re-enqueues    {}",
        classification.requeues_without_new_head
    )?;
    writeln!(
        stdout,
        "  timeline window          {}",
        match classification.timeline_complete {
            Some(true) => "complete",
            Some(false) => "TRUNCATED (older items not read; counts are lower bounds)",
            None => "UNKNOWN (pageInfo not reported)",
        }
    )?;
    writeln!(stdout)?;
    writeln!(stdout, "FACTS")?;
    for fact in &classification.facts {
        writeln!(stdout, "  {:<26} {}", fact.name, fact.value)?;
        writeln!(stdout, "  {:<26} <- {}", "", fact.source)?;
    }
    Ok(())
}

/// Write the VALIDATION block: what the head's green means, and whether the
/// merge group(s) ran tests.
pub fn write_validation<W: Write>(
    stdout: &mut W,
    validation: &PrValidationSignals,
) -> std::io::Result<()> {
    writeln!(stdout, "VALIDATION")?;
    writeln!(stdout, "  {}", validation.headline())?;
    for check in &validation.test_tier {
        writeln!(
            stdout,
            "    {}: {}{}",
            check.check,
            check.reading.render(),
            check
                .check_run_id
                .map_or_else(String::new, |id| format!("  [check run {id}]"))
        )?;
    }
    for group in &validation.merge_groups {
        writeln!(
            stdout,
            "  merge group {} ({}): {}",
            group.sha.get(..12).unwrap_or(&group.sha),
            match group.source {
                validation_signals::MergeGroupSource::MergeQueueEntry => "queue entry",
                validation_signals::MergeGroupSource::MergeCommit => "merge commit",
            },
            group.headline()
        )?;
        for decision in &group.receipt_decisions {
            writeln!(stdout, "    {}", decision.render())?;
        }
        for check in &group.test_tier {
            writeln!(stdout, "    {}: {}", check.check, check.reading.render())?;
        }
        if group.truncated {
            writeln!(
                stdout,
                "    (read bound reached or a read failed; the lists above may be incomplete)"
            )?;
        }
    }
    for warning in &validation.warnings {
        writeln!(stdout, "  warning: {warning}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/github");

    #[cfg(unix)]
    fn fake_gh(temp: &tempfile::TempDir, fixture: &str) -> GitHubActions {
        let path = temp.path().join("gh");
        crate::test_support::write_executable_script(
            &path,
            &format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> '{}'\ncat '{FIXTURES}/{fixture}'\n",
                temp.path().join("calls").display()
            ),
        );
        actions_for(temp, path)
    }

    #[cfg(unix)]
    fn actions_for(temp: &tempfile::TempDir, path: std::path::PathBuf) -> GitHubActions {
        let config = crate::config::LoadedConfig {
            data: toml::Table::new(),
            global_dir: temp.path().join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: crate::config::LocalOverlaySource::None,
        };
        GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(path)
    }

    #[cfg(unix)]
    #[test]
    fn queued_pr_report_leads_with_the_rest_trap_and_cites_sources() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(&temp, "pr_queued.json");
        let report = gather(&actions, "Generous-Corp/pulp", 8669);
        let mut out = Vec::new();
        write_human(&mut out, &report).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.starts_with(
            "REST pulls/<n>.auto_merge is null for every queued PR (GitHub consumes auto-merge \
             on enqueue) — never read it as 'unarmed'."
        ));
        assert!(
            text.contains("PR #8669 in Generous-Corp/pulp: QUEUED"),
            "{text}"
        );
        assert!(text.contains("at position 1"));
        assert!(text.contains("<- data.repository.pullRequest.isInMergeQueue"));
        assert!(!report.is_unknown());
        let calls = std::fs::read_to_string(temp.path().join("calls")).expect("calls");
        assert!(calls.contains("number=8669"), "{calls}");
        assert!(calls.contains("owner=Generous-Corp"), "{calls}");
    }

    #[cfg(unix)]
    #[test]
    fn json_form_carries_preface_and_class() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = fake_gh(&temp, "pr_ejected_requeued.json");
        let report = gather(&actions, "Generous-Corp/pulp", 8702);
        let mut out = Vec::new();
        write_json(&mut out, &report).expect("render");
        let value: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(value["classification"]["state"]["class"], "queued");
        assert_eq!(value["classification"]["requeues_without_new_head"], 1);
        assert_eq!(value["preface"], REST_AUTO_MERGE_PREFACE);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_pr_is_unknown() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("gh");
        crate::test_support::write_executable_script(
            &path,
            "#!/bin/sh\necho 'HTTP 502' >&2\nexit 1\n",
        );
        let actions = actions_for(&temp, path);
        let report = gather(&actions, "Generous-Corp/pulp", 1);
        assert!(report.is_unknown());
        assert!(report.next_action.starts_with("UNKNOWN"));
    }

    /// A `gh` that answers the queue-state query from a fixture, the
    /// validation query and REST reads from inline bodies, and 404s the rest.
    #[cfg(unix)]
    fn routing_gh(temp: &tempfile::TempDir) -> GitHubActions {
        let write = |name: &str, body: &serde_json::Value| {
            let path = temp.path().join(name);
            std::fs::write(&path, body.to_string()).expect("fixture");
            path
        };
        let tier = r#"{"schema":"shipyard-test-tier/v1","tier":"fast","selector":"pr-fast","full_suite_runs_in":"merge_group"}"#;
        let decision = r#"{"schema":"shipyard-receipt-decision/v1","target":"macos","verdict":"refuse","reason":"the receipt ran a narrowed tier","source_run_id":null,"selected":null,"passed":null,"skipped":null,"inventory_count":null}"#;
        let validation = write(
            "validation.json",
            &serde_json::json!({"data": {"repository": {"pullRequest": {
                "headRefOid": "abc123abc123abc1",
                "mergeCommit": {"oid": "def456def456def4"},
                "mergeQueueEntry": null,
                "commits": {"nodes": [{"commit": {"oid": "abc123abc123abc1", "statusCheckRollup": {"contexts": {
                    "pageInfo": {"hasNextPage": false},
                    "nodes": [{"__typename": "CheckRun", "databaseId": 77, "name": "macos",
                               "status": "COMPLETED", "conclusion": "SUCCESS", "isRequired": true}]}}}}]}
            }}}}),
        );
        let tier_annotations = write(
            "tier.json",
            &serde_json::json!([{"title": "shipyard-test-tier", "message": tier}]),
        );
        let runs = write(
            "runs.json",
            &serde_json::json!({"workflow_runs": [{"id": 5, "check_suite_id": 50}]}),
        );
        let check_runs = write(
            "check_runs.json",
            &serde_json::json!({"total_count": 1, "check_runs": [
                {"id": 88, "name": "receipt-gate", "check_suite": {"id": 50}, "output": {"annotations_count": 1}}]}),
        );
        let decision_annotations = write(
            "decision.json",
            &serde_json::json!([{"title": "shipyard-receipt-decision", "message": decision}]),
        );
        let path = temp.path().join("gh");
        crate::test_support::write_executable_script(
            &path,
            &format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> '{calls}'\ncase \"$*\" in\n\
                 *isRequired*) cat '{validation}' ;;\n\
                 *graphql*) cat '{FIXTURES}/pr_merged.json' ;;\n\
                 *check-runs/77/annotations*) cat '{tier}' ;;\n\
                 *actions/runs*) cat '{runs}' ;;\n\
                 *commits/def456def456def4/check-runs*) cat '{check_runs}' ;;\n\
                 *check-runs/88/annotations*) cat '{decision}' ;;\n\
                 *) echo 'HTTP 404' >&2; exit 1 ;;\nesac\n",
                calls = temp.path().join("calls").display(),
                validation = validation.display(),
                tier = tier_annotations.display(),
                runs = runs.display(),
                check_runs = check_runs.display(),
                decision = decision_annotations.display(),
            ),
        );
        actions_for(temp, path)
    }

    #[cfg(unix)]
    #[test]
    fn pr_report_separates_fast_tier_green_from_full_validation() {
        let temp = tempfile::tempdir().expect("temp");
        let actions = routing_gh(&temp);
        let report = gather(&actions, "Generous-Corp/pulp", 42);
        let mut out = Vec::new();
        write_human(&mut out, &report).expect("render");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(
                "PR head abc123abc123 GREEN on the fast tier, NOT full validation: validated on \
                 the fast tier (pr-fast) by macos; the full suite runs in merge_group, not on \
                 this head"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "merge group def456def456 (merge commit): full suite ran in this merge group"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "    macos: validated in full: receipt refused because the receipt ran a \
                 narrowed tier"
            ),
            "{text}"
        );
        let mut json_out = Vec::new();
        write_json(&mut json_out, &report).expect("json");
        let value: Value = serde_json::from_slice(&json_out).expect("json");
        assert_eq!(value["validation"]["test_tier_verdict"]["tier"], "fast");
        assert_eq!(
            value["validation"]["test_tier"][0]["reading"]["selector"],
            "pr-fast"
        );
        assert_eq!(
            value["validation"]["merge_groups"][0]["receipt_decisions"][0]["verdict"],
            "refuse"
        );
        assert_eq!(
            value["validation"]["merge_groups"][0]["source"],
            "merge_commit"
        );
        assert_eq!(value["validation"]["api_calls"], 5);
    }

    #[test]
    fn next_action_never_recommends_arming_a_refused_class() {
        for state in [
            PrQueueState::Queued {
                entry_state: None,
                position: Some(2),
                requeues_without_new_head: 0,
            },
            PrQueueState::ArmedNotQueued {
                enabled_at: None,
                requeues_without_new_head: 0,
            },
            PrQueueState::Merged,
            PrQueueState::Closed,
        ] {
            let action = next_action(&state, 7);
            assert!(!action.contains("shipyard ship"), "{action}");
            assert!(!action.contains("--auto"), "{action}");
        }
        let unchanged = next_action(
            &PrQueueState::Ejected {
                reason: "failed_checks".to_owned(),
                at: None,
                new_head_since_removal: false,
                requeues_without_new_head: 1,
            },
            7,
        );
        assert!(unchanged.contains("push a fix first"), "{unchanged}");
        let manual = next_action(
            &PrQueueState::Ejected {
                reason: "manual".to_owned(),
                at: Some("2026-09-22T21:30:26Z".to_owned()),
                new_head_since_removal: false,
                requeues_without_new_head: 0,
            },
            7,
        );
        assert!(
            manual.contains("Removed from the queue (manual) at 2026-09-22T21:30:26Z"),
            "{manual}"
        );
        assert!(
            manual.contains("Confirm with whoever dequeued it"),
            "{manual}"
        );
        assert!(!manual.contains("ALLGREEN"), "{manual}");
        for state in [
            PrQueueState::NeverArmed,
            PrQueueState::Unknown {
                detail: "x".to_owned(),
            },
        ] {
            let action = next_action(&state, 7);
            for name in ["GHAPP_ALLOW", "SHIPYARD_INTERNAL"] {
                assert!(!action.contains(name), "{action}");
            }
        }
    }
}
