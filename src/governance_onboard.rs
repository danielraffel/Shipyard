//! Design A governance onboarding planner.
//!
//! The command is deliberately split into pure guards and a thin CLI adapter.
//! The guards are used by both the offline fixture tests and the live planner;
//! this keeps a plan from becoming an implicit authorization to write.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Files an onboarding PR may change.
pub const ALLOWED_PATHS: &[&str] = &[
    ".github/workflows/review-verdict.yml",
    ".github/workflows/review-verdict-trigger.yml",
    ".shipyard/config.toml",
];
/// GitHub's Actions App integration id.
pub const ACTIONS_INTEGRATION_ID: u64 = 15_368;

/// The bounded waits used by onboarding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct BoundedWaits {
    /// The approval wait in minutes.
    pub approval_minutes: u64,
    /// The per-PR evaluation wait in minutes.
    pub evaluation_minutes: u64,
}

impl Default for BoundedWaits {
    fn default() -> Self {
        Self {
            approval_minutes: 30,
            evaluation_minutes: 10,
        }
    }
}

/// Rendered onboarding files and their hashes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RenderedFiles {
    /// Review-verdict workflow.
    pub review_verdict: String,
    /// Permission-less review trigger workflow.
    pub review_verdict_trigger: String,
    /// Minimal config patch.
    pub config: String,
    /// SHA-256 hashes, keyed by rendered path.
    pub hashes: BTreeMap<String, String>,
}

/// A single open pull request in the first-evaluation sweep.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SweepRow {
    /// PR number.
    pub number: u64,
    /// Exact head.
    pub head: String,
    /// Trigger selected by the planner.
    pub trigger: String,
    /// Existing or expected check-run id.
    pub check_run_id: Option<u64>,
    /// Terminal conclusion when already observed.
    pub conclusion: Option<String>,
}

/// A durable receipt written after an applied run.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct OnboardingReceipt {
    /// Repository slug.
    pub repo: String,
    /// Rendered hashes.
    pub rendered_hashes: BTreeMap<String, String>,
    /// PR number, when opened or reused.
    pub onboarding_pr: Option<u64>,
    /// Exact approval comment or review id.
    pub approval_id: Option<u64>,
    /// Approval stamp/session.
    pub approval_stamp: Option<String>,
    /// Merge commit.
    pub merge_sha: Option<String>,
    /// Ruleset backup path.
    pub ruleset_backup: Option<String>,
    /// Post-write ruleset JSON.
    pub ruleset_after: Option<Value>,
    /// First-evaluation rows.
    pub sweep: Vec<SweepRow>,
    /// Unstable PR hand-offs.
    pub unstable: Vec<String>,
    /// Enrollment result.
    pub enrolled: bool,
}

/// Validate the onboarding PR path allowlist.
pub fn validate_path_allowlist(paths: &[String]) -> Result<(), String> {
    for path in paths {
        if !ALLOWED_PATHS.contains(&path.as_str()) {
            return Err(format!("onboarding PR touches disallowed path: {path}"));
        }
    }
    if paths.is_empty() {
        return Err("onboarding PR has no files".to_owned());
    }
    Ok(())
}

/// Whether a repository belongs to the fleet-owned accounts.
#[must_use]
pub fn is_owned_repo(repo: &str) -> bool {
    repo.split_once('/').is_some_and(|(owner, name)| {
        !name.is_empty()
            && (owner.eq_ignore_ascii_case("danielraffel")
                || owner.eq_ignore_ascii_case("generous-corp"))
    })
}

/// Compare rendered files byte-for-byte with the PR content.
pub fn require_byte_identical(expected: &[u8], actual: &[u8], path: &str) -> Result<(), String> {
    if expected == actual {
        Ok(())
    } else {
        Err(format!("rendered {path} differs byte-for-byte from the PR"))
    }
}

/// A compact approval record consumed by the exact-head guard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalRecord {
    /// Reviewer login or configured signer.
    pub signer: String,
    /// State (`APPROVED` or `HOLD`).
    pub state: String,
    /// Commit named by the review/comment.
    pub head: String,
    /// Stamped reviewer session.
    pub session: Option<String>,
    /// Record id.
    pub id: u64,
}

/// Count one approval at the exact head, from another reviewer session.
pub fn counted_approve<'a>(
    records: &'a [ApprovalRecord],
    head: &str,
    signers: &[String],
    session_globs: &[String],
    current_session: Option<&str>,
) -> Result<&'a ApprovalRecord, String> {
    if records.iter().any(|record| {
        record.head.eq_ignore_ascii_case(head) && record.state.eq_ignore_ascii_case("HOLD")
    }) {
        return Err("counting HOLD exists at the exact head".to_owned());
    }
    records
        .iter()
        .find(|record| {
            record.head.eq_ignore_ascii_case(head)
                && record.state.eq_ignore_ascii_case("APPROVED")
                && signers
                    .iter()
                    .any(|signer| signer.eq_ignore_ascii_case(&record.signer))
                && record.session.as_deref().is_some_and(|session| {
                    session_globs.is_empty()
                        || session_globs.iter().any(|glob| glob_matches(glob, session))
                })
                && current_session.is_none_or(|current| record.session.as_deref() != Some(current))
        })
        .ok_or_else(|| "no counted APPROVE at the exact head from a different session".to_owned())
}

/// Prove that the required verdict cannot currently be produced on main.
#[must_use]
pub fn verdict_unproducible(
    workflow_present: bool,
    last_three_own_errors: bool,
    trigger_blocked_by_policy: bool,
) -> bool {
    !workflow_present || last_three_own_errors || trigger_blocked_by_policy
}

/// Require the merge request to remain bound to the evaluated head.
pub fn require_expected_head(evaluated: &str, expected_head_oid: &str) -> Result<(), String> {
    if evaluated.eq_ignore_ascii_case(expected_head_oid) {
        Ok(())
    } else {
        Err(format!(
            "expectedHeadOid {expected_head_oid} differs from evaluated head {evaluated}"
        ))
    }
}

/// Validate a ruleset update as add-only.
pub fn validate_add_only_ruleset(before: &Value, after: &Value) -> Result<(), String> {
    if before.get("bypass_actors") != after.get("bypass_actors") {
        return Err("ruleset update changes bypass_actors".to_owned());
    }
    if enforcement_rank(after.get("enforcement")) < enforcement_rank(before.get("enforcement")) {
        return Err("ruleset update lowers enforcement".to_owned());
    }
    let before_rules = before
        .get("rules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let after_rules = after
        .get("rules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for rule in &before_rules {
        if !after_rules.iter().any(|candidate| candidate == rule) {
            return Err("ruleset update removes or edits an existing rule".to_owned());
        }
    }
    let required_before = required_contexts(&before_rules);
    for rule in &after_rules {
        let Some(parameters) = rule.get("parameters") else {
            continue;
        };
        let Some(contexts) = parameters
            .get("required_status_checks")
            .and_then(Value::as_array)
        else {
            continue;
        };
        for context in contexts {
            let name = context
                .get("context")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !required_before.contains(name)
                && context.get("integration_id").and_then(Value::as_u64)
                    != Some(ACTIONS_INTEGRATION_ID)
            {
                return Err(format!("new required check {name} is not bound to Actions"));
            }
        }
    }
    Ok(())
}

fn required_contexts(rules: &[Value]) -> BTreeSet<String> {
    rules
        .iter()
        .filter_map(|rule| rule.get("parameters"))
        .filter_map(|parameters| parameters.get("required_status_checks"))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|entry| entry.get("context").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn enforcement_rank(value: Option<&Value>) -> u8 {
    match value.and_then(Value::as_str).unwrap_or("disabled") {
        "active" => 2,
        "evaluate" => 1,
        _ => 0,
    }
}

fn glob_matches(glob: &str, value: &str) -> bool {
    let mut pattern = String::from("^");
    for character in glob.chars() {
        match character {
            '*' => pattern.push_str(".*"),
            '?' => pattern.push('.'),
            other => pattern.push_str(&regex::escape(&other.to_string())),
        }
    }
    pattern.push('$');
    regex::Regex::new(&pattern).is_ok_and(|regex| regex.is_match(value))
}

/// Render the review-verdict template with a workflow-dispatch input.
#[must_use]
pub fn render_review_verdict_template(template: &str, pinned_ref: &str) -> String {
    let mut rendered = template.replace(
        "on:\n  pull_request_target:",
        "on:\n  workflow_dispatch:\n    inputs:\n      pull_request:\n        description: Pull request number to evaluate\n        required: true\n        type: string\n  pull_request_target:",
    );
    rendered = rendered.replace("${{ github.event.repository.default_branch }}", pinned_ref);
    rendered
}

/// Render the minimal onboarding config, preserving no unrelated keys.
#[must_use]
pub fn render_config(signers: &[String], stamp_sessions: &[String]) -> String {
    let signers = signers
        .iter()
        .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let sessions = stamp_sessions
        .iter()
        .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "[auto_merge]\nreviewer_signers = [{signers}]\nreviewer_stamp_sessions = [{sessions}]\nfleet_review_required = true\n\n[validation]\npr_gates = false\n"
    )
}

/// Write a receipt atomically, creating its parent directory.
pub fn write_receipt(path: &Path, receipt: &OnboardingReceipt) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "receipt path has no parent".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create receipt directory: {error}"))?;
    let bytes =
        serde_json::to_vec_pretty(receipt).map_err(|error| format!("encode receipt: {error}"))?;
    std::fs::write(path, bytes).map_err(|error| format!("write receipt: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn allowlist_rejects_extra_file() {
        assert!(validate_path_allowlist(&["src/lib.rs".to_owned()]).is_err());
    }

    #[test]
    fn ownership_guard_rejects_third_party() {
        assert!(!is_owned_repo("upstream/project"));
        assert!(is_owned_repo("danielraffel/project"));
    }

    #[test]
    fn byte_guard_is_mutation_sensitive() {
        assert!(require_byte_identical(b"a", b"b", "x").is_err());
    }

    #[test]
    fn approval_requires_different_stamped_session_and_no_hold() {
        let records = vec![ApprovalRecord {
            signer: "pulp-review".to_owned(),
            state: "APPROVED".to_owned(),
            head: "abc".to_owned(),
            session: Some("fleet-pass-reviewer-1".to_owned()),
            id: 7,
        }];
        assert!(
            counted_approve(
                &records,
                "abc",
                &["pulp-review".to_owned()],
                &["fleet-pass-reviewer-*".to_owned()],
                Some("fleet-pass-reviewer-1")
            )
            .is_err()
        );
        assert!(
            counted_approve(
                &records,
                "abc",
                &["pulp-review".to_owned()],
                &["fleet-pass-reviewer-*".to_owned()],
                Some("steward-1")
            )
            .is_ok()
        );
    }

    #[test]
    fn add_only_rejects_removal_bypass_and_lowering() {
        let before =
            json!({"enforcement":"active","bypass_actors":[],"rules":[{"type":"deletion"}]});
        assert!(
            validate_add_only_ruleset(
                &before,
                &json!({"enforcement":"active","bypass_actors":[],"rules":[]})
            )
            .is_err()
        );
        assert!(
            validate_add_only_ruleset(
                &before,
                &json!({"enforcement":"active","bypass_actors":[{}],"rules":[{"type":"deletion"}]})
            )
            .is_err()
        );
        assert!(
            validate_add_only_ruleset(
                &before,
                &json!({"enforcement":"evaluate","bypass_actors":[],"rules":[{"type":"deletion"}]})
            )
            .is_err()
        );
    }

    #[test]
    fn healthy_workflow_never_qualifies_for_bootstrap() {
        assert!(!verdict_unproducible(true, false, false));
        assert!(verdict_unproducible(false, false, false));
    }

    #[test]
    fn merge_guard_binds_expected_head() {
        assert!(require_expected_head("abc", "def").is_err());
        assert!(require_expected_head("abc", "abc").is_ok());
    }

    #[test]
    fn dispatch_input_is_rendered() {
        let output = render_review_verdict_template("on:\n  pull_request_target:\n", "abc");
        assert!(output.contains("workflow_dispatch:"));
        assert!(output.contains("pull_request:"));
    }
}
