//! Branch governance profiles and GitHub branch-protection application.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;
use serde_json::Value as JsonValue;
use toml::{Table, Value};

use crate::config::LoadedConfig;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;

/// Branch-protection rules resolved from Shipyard governance config.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BranchProtectionRules {
    /// Require pull requests before merging.
    pub require_pr: bool,
    /// Required status-check contexts.
    pub require_status_checks: Vec<String>,
    /// Require branches to be up to date before merging.
    pub require_strict_status: bool,
    /// Number of required approving reviews.
    pub require_review_count: u64,
    /// Enforce rules for administrators.
    pub enforce_admins: bool,
    /// Dismiss stale reviews.
    pub dismiss_stale_reviews: bool,
    /// Require code-owner review.
    pub require_code_owner_reviews: bool,
    /// Allow force pushes.
    pub allow_force_push: bool,
    /// Allow branch deletion.
    pub allow_deletions: bool,
    /// Require linear history.
    pub require_linear_history: bool,
    /// Require conversation resolution before merging.
    pub required_conversation_resolution: bool,
}

/// Config-aware GitHub CLI boundary for governance API calls.
#[derive(Clone, Debug)]
pub struct GovernanceGh {
    cwd: PathBuf,
    client: GhClient,
    binary_override: Option<PathBuf>,
}

impl GovernanceGh {
    /// Build from already loaded Shipyard config.
    pub fn from_loaded_config(
        cwd: &Path,
        config: &LoadedConfig,
        binary_override: Option<&Path>,
    ) -> Result<Self, String> {
        let client = GhClient::from_loaded_config(config).map_err(|error| error.to_string())?;
        Ok(Self {
            cwd: cwd.to_path_buf(),
            client,
            binary_override: binary_override.map(Path::to_path_buf),
        })
    }

    /// Build by loading Shipyard config from `cwd`.
    pub fn from_cwd(
        mode: RuntimeMode,
        cwd: &Path,
        binary_override: Option<&Path>,
    ) -> Result<Self, String> {
        let client = GhClient::from_cwd(mode, cwd).map_err(|error| error.to_string())?;
        Ok(Self {
            cwd: cwd.to_path_buf(),
            client,
            binary_override: binary_override.map(Path::to_path_buf),
        })
    }

    /// Build an ambient-auth client for tests or already classified callers.
    #[must_use]
    pub fn ambient(cwd: &Path, binary_override: Option<&Path>) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            client: GhClient::ambient(),
            binary_override: binary_override.map(Path::to_path_buf),
        }
    }

    fn command(&self) -> Result<Command, String> {
        self.client
            .prepare_command(
                &self.cwd,
                self.binary_override.as_deref(),
                GhSupervision::Unsupervised,
                GhAuthPolicy::Default,
            )
            .map_err(|error| error.to_string())
    }
}

/// A field-level drift status for one branch-protection knob.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriftStatus {
    /// Profile, declared config, and live GitHub state agree.
    Aligned,
    /// Declared config intentionally differs from the active profile.
    Deviated,
    /// Live GitHub state differs from declared config.
    Drifted,
    /// Declared config differs from profile and live differs from declared.
    Both,
    /// GitHub reports no branch protection for this branch.
    Unprotected,
}

impl DriftStatus {
    /// Python-compatible status string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aligned => "aligned",
            Self::Deviated => "deviated",
            Self::Drifted => "drifted",
            Self::Both => "both",
            Self::Unprotected => "unprotected",
        }
    }

    /// Whether this status requires an apply operation.
    #[must_use]
    pub const fn needs_apply(self) -> bool {
        matches!(self, Self::Drifted | Self::Both | Self::Unprotected)
    }
}

/// One row in the profile-vs-declared-vs-live governance matrix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriftEntry {
    /// Branch-protection field name.
    pub field_name: &'static str,
    /// Value implied by the active profile.
    pub profile_value: JsonValue,
    /// Value declared by project config after overrides.
    pub declared_value: JsonValue,
    /// Value currently reported by GitHub.
    pub live_value: JsonValue,
    /// Field drift status.
    pub status: DriftStatus,
}

/// Drift report for one branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriftReport {
    /// Branch name.
    pub branch: String,
    /// Field-level drift rows.
    pub entries: Vec<DriftEntry>,
    /// Whether GitHub reports no protection for this branch.
    pub live_unprotected: bool,
}

impl DriftReport {
    /// Whether applying declared rules would change live state.
    #[must_use]
    pub fn has_drift(&self) -> bool {
        self.live_unprotected || self.entries.iter().any(|entry| entry.status.needs_apply())
    }

    /// Fields that would change on apply.
    #[must_use]
    pub fn drifted_entries(&self) -> Vec<&DriftEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.status.needs_apply())
            .collect()
    }

    /// Fields intentionally deviated from the active profile.
    #[must_use]
    pub fn deviated_entries(&self) -> Vec<&DriftEntry> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.status, DriftStatus::Deviated | DriftStatus::Both))
            .collect()
    }
}

/// Aggregate governance status across branches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceStatus {
    /// Owner/repo slug.
    pub repo: String,
    /// Active governance profile name.
    pub profile_name: String,
    /// Per-branch reports.
    pub reports: Vec<DriftReport>,
    /// Non-fatal branch read errors.
    pub errors: Vec<String>,
}

impl GovernanceStatus {
    /// Whether any branch has drift.
    #[must_use]
    pub fn has_drift(&self) -> bool {
        self.reports.iter().any(DriftReport::has_drift)
    }

    /// Whether any branch could not be read.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// Apply action for one branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyAction {
    /// No live update is needed.
    Noop,
    /// Existing protection should be updated.
    Update,
    /// Protection should be created.
    Create,
}

impl ApplyAction {
    /// Python-compatible action string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Noop => "noop",
            Self::Update => "update",
            Self::Create => "create",
        }
    }
}

/// A read-only governance apply plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyPlan {
    /// Owner/repo slug.
    pub repo: String,
    /// Branch to apply.
    pub branch: String,
    /// Planned action.
    pub action: ApplyAction,
    /// Rules that should be present after apply.
    pub declared_rules: BranchProtectionRules,
    /// Drift report that produced the plan.
    pub drift_report: DriftReport,
    /// Manual follow-up items Shipyard cannot enforce through the API.
    pub manual_followups: Vec<String>,
}

impl ApplyPlan {
    /// Whether this plan performs no update.
    #[must_use]
    pub const fn is_noop(&self) -> bool {
        matches!(self.action, ApplyAction::Noop)
    }
}

/// Result of executing a governance apply plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyResult {
    /// Plan that was executed or skipped.
    pub plan: ApplyPlan,
    /// Whether a GitHub API write was issued.
    pub executed: bool,
    /// Error detail when the write failed.
    pub error_message: Option<String>,
}

/// Resolve effective branch-protection rules for `branch`.
pub fn resolve_branch_rules(data: &Table, branch: &str) -> Result<BranchProtectionRules, String> {
    let required_status_checks = required_status_checks(data);
    let profile = profile_for_name(profile_name(data), &required_status_checks)?;
    let overrides = resolve_overrides_for(data, branch)?;
    Ok(apply_overrides(profile, &overrides))
}

/// Resolve the active profile's branch-protection defaults before branch overrides.
pub fn resolve_profile_rules(data: &Table) -> Result<BranchProtectionRules, String> {
    let required_status_checks = required_status_checks(data);
    profile_for_name(profile_name(data), &required_status_checks)
}

/// Resolve the active profile name.
#[must_use]
pub fn resolved_profile_name(data: &Table) -> String {
    profile_name(data).to_owned()
}

/// Apply branch protection through `gh api`.
pub fn put_branch_protection(
    repo: &str,
    branch: &str,
    rules: &BranchProtectionRules,
    gh: &GovernanceGh,
) -> Result<(), String> {
    let payload = serde_json::to_vec(&api_payload_from_rules(rules))
        .map_err(|error| format!("failed to encode branch protection payload: {error}"))?;
    let mut command = gh.command()?;
    command.args([
        "api",
        "-X",
        "PUT",
        &format!("repos/{repo}/branches/{branch}/protection"),
        "--input",
        "-",
    ]);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to run gh api: {error}"))?;
    {
        use std::io::Write as _;
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "failed to open gh stdin".to_owned())?;
        stdin
            .write_all(&payload)
            .map_err(|error| format!("failed to write gh api payload: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed to wait for gh api: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        });
        Err(format!(
            "gh api branch protection failed: {}",
            detail.trim()
        ))
    }
}

/// Read branch protection through `gh api`.
///
/// Returns `Ok(None)` when GitHub reports that the branch is unprotected.
pub fn get_branch_protection(
    repo: &str,
    branch: &str,
    gh: &GovernanceGh,
) -> Result<Option<BranchProtectionRules>, String> {
    let output = gh
        .command()?
        .args(["api", &format!("repos/{repo}/branches/{branch}/protection")])
        .output()
        .map_err(|error| format!("failed to run gh api: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        });
        let detail = detail.trim();
        if detail.contains("Branch not protected") || detail.contains("404") {
            return Ok(None);
        }
        return Err(format!(
            "gh api failed for {repo}/{branch}: {}",
            if detail.is_empty() {
                "no detail"
            } else {
                detail
            }
        ));
    }
    let payload = serde_json::from_slice::<JsonValue>(&output.stdout)
        .map_err(|error| format!("gh api returned non-JSON: {error}"))?;
    Ok(Some(rules_from_api_payload(&payload)))
}

/// Build a status report for the requested branches.
#[must_use]
pub fn build_status(
    repo: &str,
    data: &Table,
    branches: &[String],
    gh: &GovernanceGh,
) -> GovernanceStatus {
    let profile_name = resolved_profile_name(data);
    let profile_rules = resolve_profile_rules(data).unwrap_or_default();
    let mut reports = Vec::new();
    let mut errors = Vec::new();

    for branch in branches {
        let declared_rules = match resolve_branch_rules(data, branch) {
            Ok(rules) => rules,
            Err(error) => {
                errors.push(format!("{branch}: {error}"));
                continue;
            }
        };
        let live_rules = match get_branch_protection(repo, branch, gh) {
            Ok(rules) => rules,
            Err(error) => {
                errors.push(format!("{branch}: {error}"));
                continue;
            }
        };
        reports.push(compute_drift(
            branch,
            &profile_rules,
            &declared_rules,
            live_rules.as_ref(),
        ));
    }

    GovernanceStatus {
        repo: repo.to_owned(),
        profile_name,
        reports,
        errors,
    }
}

/// Compute drift for one branch.
#[must_use]
pub fn compute_drift(
    branch: &str,
    profile_rules: &BranchProtectionRules,
    declared_rules: &BranchProtectionRules,
    live_rules: Option<&BranchProtectionRules>,
) -> DriftReport {
    let mut entries = Vec::new();
    for &field_name in GOVERNANCE_FIELDS {
        let profile_value = normalized_rule_value(profile_rules, field_name);
        let declared_value = normalized_rule_value(declared_rules, field_name);
        let (live_value, status) = if let Some(live_rules) = live_rules {
            let live_value = normalized_rule_value(live_rules, field_name);
            let deviated = declared_value != profile_value;
            let drifted = live_value != declared_value;
            let status = match (deviated, drifted) {
                (true, true) => DriftStatus::Both,
                (true, false) => DriftStatus::Deviated,
                (false, true) => DriftStatus::Drifted,
                (false, false) => DriftStatus::Aligned,
            };
            (live_value, status)
        } else {
            (JsonValue::Null, DriftStatus::Unprotected)
        };
        entries.push(DriftEntry {
            field_name,
            profile_value,
            declared_value,
            live_value,
            status,
        });
    }

    DriftReport {
        branch: branch.to_owned(),
        entries,
        live_unprotected: live_rules.is_none(),
    }
}

/// Build an apply plan from a drift report and declared rules.
#[must_use]
pub fn build_apply_plan(
    repo: &str,
    branch: &str,
    declared_rules: BranchProtectionRules,
    drift_report: DriftReport,
) -> ApplyPlan {
    let action = if drift_report.live_unprotected {
        ApplyAction::Create
    } else if drift_report.has_drift() {
        ApplyAction::Update
    } else {
        ApplyAction::Noop
    };
    ApplyPlan {
        repo: repo.to_owned(),
        branch: branch.to_owned(),
        action,
        declared_rules,
        drift_report,
        manual_followups: vec![format!(
            "Immutable releases: verify the 'Immutable releases' checkbox is enabled at https://github.com/{repo}/settings. Shipyard cannot read this setting via API on personal repos."
        )],
    }
}

/// Execute an apply plan.
#[must_use]
pub fn execute_apply_plan(plan: ApplyPlan, dry_run: bool, gh: &GovernanceGh) -> ApplyResult {
    if plan.is_noop() || dry_run {
        return ApplyResult {
            plan,
            executed: false,
            error_message: None,
        };
    }
    match put_branch_protection(&plan.repo, &plan.branch, &plan.declared_rules, gh) {
        Ok(()) => ApplyResult {
            plan,
            executed: true,
            error_message: None,
        },
        Err(error) => ApplyResult {
            plan,
            executed: false,
            error_message: Some(error),
        },
    }
}

/// Translate GitHub's branch-protection JSON into rules.
#[must_use]
pub fn rules_from_api_payload(payload: &JsonValue) -> BranchProtectionRules {
    let status_checks = payload
        .get("required_status_checks")
        .unwrap_or(&JsonValue::Null);
    let require_status_checks = status_checks
        .get("contexts")
        .and_then(JsonValue::as_array)
        .map_or_else(Vec::new, |items| {
            items
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect()
        });
    let require_strict_status = status_checks
        .get("strict")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let reviews = payload
        .get("required_pull_request_reviews")
        .unwrap_or(&JsonValue::Null);
    let require_review_count = reviews
        .get("required_approving_review_count")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);

    BranchProtectionRules {
        require_pr: payload
            .get("required_pull_request_reviews")
            .is_some_and(|value| !value.is_null()),
        require_status_checks,
        require_strict_status,
        require_review_count,
        enforce_admins: enabled(payload.get("enforce_admins")),
        dismiss_stale_reviews: reviews
            .get("dismiss_stale_reviews")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false),
        require_code_owner_reviews: reviews
            .get("require_code_owner_reviews")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false),
        allow_force_push: enabled(payload.get("allow_force_pushes")),
        allow_deletions: enabled(payload.get("allow_deletions")),
        require_linear_history: enabled(payload.get("required_linear_history")),
        required_conversation_resolution: enabled(payload.get("required_conversation_resolution")),
    }
}

/// Convert rules to a deterministic TOML table.
#[must_use]
pub fn rules_to_toml_table(rules: &BranchProtectionRules) -> Table {
    let mut table = Table::new();
    table.insert("require_pr".to_owned(), Value::Boolean(rules.require_pr));
    table.insert(
        "require_status_checks".to_owned(),
        Value::Array(
            rules
                .require_status_checks
                .iter()
                .map(|check| Value::String(check.clone()))
                .collect(),
        ),
    );
    table.insert(
        "require_strict_status".to_owned(),
        Value::Boolean(rules.require_strict_status),
    );
    table.insert(
        "require_review_count".to_owned(),
        Value::Integer(i64::try_from(rules.require_review_count).unwrap_or(i64::MAX)),
    );
    table.insert(
        "enforce_admins".to_owned(),
        Value::Boolean(rules.enforce_admins),
    );
    table.insert(
        "dismiss_stale_reviews".to_owned(),
        Value::Boolean(rules.dismiss_stale_reviews),
    );
    table.insert(
        "require_code_owner_reviews".to_owned(),
        Value::Boolean(rules.require_code_owner_reviews),
    );
    table.insert(
        "allow_force_push".to_owned(),
        Value::Boolean(rules.allow_force_push),
    );
    table.insert(
        "allow_deletions".to_owned(),
        Value::Boolean(rules.allow_deletions),
    );
    table.insert(
        "require_linear_history".to_owned(),
        Value::Boolean(rules.require_linear_history),
    );
    table.insert(
        "required_conversation_resolution".to_owned(),
        Value::Boolean(rules.required_conversation_resolution),
    );
    table
}

/// Parse rules from a TOML table.
#[must_use]
pub fn rules_from_toml_table(table: &Table) -> BranchProtectionRules {
    BranchProtectionRules {
        require_pr: table
            .get("require_pr")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        require_status_checks: table
            .get("require_status_checks")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |items| {
                items
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect()
            }),
        require_strict_status: table
            .get("require_strict_status")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        require_review_count: table
            .get("require_review_count")
            .and_then(Value::as_integer)
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(0),
        enforce_admins: table
            .get("enforce_admins")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        dismiss_stale_reviews: table
            .get("dismiss_stale_reviews")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        require_code_owner_reviews: table
            .get("require_code_owner_reviews")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        allow_force_push: table
            .get("allow_force_push")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        allow_deletions: table
            .get("allow_deletions")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        require_linear_history: table
            .get("require_linear_history")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        required_conversation_resolution: table
            .get("required_conversation_resolution")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn profile_name(data: &Table) -> &str {
    dotted_str(data, "project.profile").unwrap_or("solo")
}

fn required_status_checks(data: &Table) -> Vec<String> {
    dotted_array(data, "governance.required_status_checks")
        .or_else(|| dotted_array(data, "merge.require_platforms"))
        .map_or(&[] as &[Value], Vec::as_slice)
        .iter()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn profile_for_name(
    name: &str,
    required_status_checks: &[String],
) -> Result<BranchProtectionRules, String> {
    let mut rules = BranchProtectionRules {
        require_status_checks: required_status_checks.to_owned(),
        ..BranchProtectionRules::default()
    };
    match name {
        "solo" => {
            rules.require_pr = true;
            rules.require_strict_status = false;
            Ok(rules)
        }
        "multi" => {
            rules.require_pr = true;
            rules.require_strict_status = true;
            rules.require_review_count = 1;
            rules.enforce_admins = true;
            rules.dismiss_stale_reviews = true;
            rules.required_conversation_resolution = true;
            Ok(rules)
        }
        "custom" => Ok(rules),
        other => Err(format!(
            "Unknown governance profile '{other}'. Expected one of: solo, multi, custom"
        )),
    }
}

fn resolve_overrides_for(data: &Table, branch: &str) -> Result<BTreeMap<String, Value>, String> {
    let mut merged = BTreeMap::new();
    let Some(overrides) = data.get("branch_protection").and_then(Value::as_table) else {
        return Ok(merged);
    };
    for (glob, value) in overrides {
        if !glob_matches(glob, branch) {
            continue;
        }
        let Some(table) = value.as_table() else {
            continue;
        };
        let flattened = flatten_extends_chain(glob, overrides, Vec::new())?;
        for (key, value) in flattened {
            if key != "extends" {
                merged.insert(key, value);
            }
        }
        for (key, value) in table {
            if key != "extends" {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(merged)
}

fn flatten_extends_chain(
    glob: &str,
    overrides: &Table,
    mut visited: Vec<String>,
) -> Result<BTreeMap<String, Value>, String> {
    if visited.iter().any(|item| item == glob) {
        return Ok(BTreeMap::new());
    }
    visited.push(glob.to_owned());
    let Some(table) = overrides.get(glob).and_then(Value::as_table) else {
        return Ok(BTreeMap::new());
    };
    let mut merged = BTreeMap::new();
    if let Some(parent) = table.get("extends").and_then(Value::as_str)
        && overrides.contains_key(parent)
    {
        merged.extend(flatten_extends_chain(parent, overrides, visited)?);
    }
    for (key, value) in table {
        if key != "extends" {
            merged.insert(key.clone(), value.clone());
        }
    }
    Ok(merged)
}

fn apply_overrides(
    mut rules: BranchProtectionRules,
    overrides: &BTreeMap<String, Value>,
) -> BranchProtectionRules {
    for (key, value) in overrides {
        match key.as_str() {
            "require_pr" => set_bool(value, &mut rules.require_pr),
            "require_status_checks" => {
                if let Some(values) = value.as_array() {
                    rules.require_status_checks = values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect();
                }
            }
            "require_strict_status" => set_bool(value, &mut rules.require_strict_status),
            "require_review_count" => set_u64(value, &mut rules.require_review_count),
            "enforce_admins" => set_bool(value, &mut rules.enforce_admins),
            "dismiss_stale_reviews" => set_bool(value, &mut rules.dismiss_stale_reviews),
            "require_code_owner_reviews" => set_bool(value, &mut rules.require_code_owner_reviews),
            "allow_force_push" => set_bool(value, &mut rules.allow_force_push),
            "allow_deletions" => set_bool(value, &mut rules.allow_deletions),
            "require_linear_history" => set_bool(value, &mut rules.require_linear_history),
            "required_conversation_resolution" => {
                set_bool(value, &mut rules.required_conversation_resolution);
            }
            _ => {}
        }
    }
    rules
}

const GOVERNANCE_FIELDS: &[&str] = &[
    "require_pr",
    "require_status_checks",
    "require_strict_status",
    "require_review_count",
    "enforce_admins",
    "dismiss_stale_reviews",
    "require_code_owner_reviews",
    "allow_force_push",
    "allow_deletions",
    "require_linear_history",
    "required_conversation_resolution",
];

fn normalized_rule_value(rules: &BranchProtectionRules, field_name: &str) -> JsonValue {
    match field_name {
        "require_pr" => JsonValue::Bool(rules.require_pr),
        "require_status_checks" => {
            let mut checks = rules.require_status_checks.clone();
            checks.sort();
            JsonValue::Array(checks.into_iter().map(JsonValue::String).collect())
        }
        "require_strict_status" => JsonValue::Bool(rules.require_strict_status),
        "require_review_count" => JsonValue::from(rules.require_review_count),
        "enforce_admins" => JsonValue::Bool(rules.enforce_admins),
        "dismiss_stale_reviews" => JsonValue::Bool(rules.dismiss_stale_reviews),
        "require_code_owner_reviews" => JsonValue::Bool(rules.require_code_owner_reviews),
        "allow_force_push" => JsonValue::Bool(rules.allow_force_push),
        "allow_deletions" => JsonValue::Bool(rules.allow_deletions),
        "require_linear_history" => JsonValue::Bool(rules.require_linear_history),
        "required_conversation_resolution" => {
            JsonValue::Bool(rules.required_conversation_resolution)
        }
        _ => JsonValue::Null,
    }
}

fn enabled(value: Option<&JsonValue>) -> bool {
    value
        .and_then(|value| {
            value
                .get("enabled")
                .and_then(JsonValue::as_bool)
                .or_else(|| value.as_bool())
        })
        .unwrap_or(false)
}

fn set_bool(value: &Value, target: &mut bool) {
    if let Some(value) = value.as_bool() {
        *target = value;
    }
}

fn set_u64(value: &Value, target: &mut u64) {
    if let Some(value) = value
        .as_integer()
        .and_then(|value| u64::try_from(value).ok())
    {
        *target = value;
    }
}

#[derive(Serialize)]
struct RequiredStatusChecksPayload<'a> {
    strict: bool,
    contexts: &'a [String],
}

#[derive(Serialize)]
struct PullRequestReviewsPayload {
    required_approving_review_count: u64,
    dismiss_stale_reviews: bool,
    require_code_owner_reviews: bool,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Serialize)]
struct BranchProtectionPayload<'a> {
    required_status_checks: Option<RequiredStatusChecksPayload<'a>>,
    enforce_admins: bool,
    required_pull_request_reviews: Option<PullRequestReviewsPayload>,
    restrictions: Option<serde_json::Value>,
    required_linear_history: bool,
    allow_force_pushes: bool,
    allow_deletions: bool,
    required_conversation_resolution: bool,
}

fn api_payload_from_rules(rules: &BranchProtectionRules) -> BranchProtectionPayload<'_> {
    BranchProtectionPayload {
        required_status_checks: (!rules.require_status_checks.is_empty()).then_some(
            RequiredStatusChecksPayload {
                strict: rules.require_strict_status,
                contexts: &rules.require_status_checks,
            },
        ),
        enforce_admins: rules.enforce_admins,
        required_pull_request_reviews: (rules.require_pr || rules.require_review_count > 0)
            .then_some(PullRequestReviewsPayload {
                required_approving_review_count: rules.require_review_count,
                dismiss_stale_reviews: rules.dismiss_stale_reviews,
                require_code_owner_reviews: rules.require_code_owner_reviews,
            }),
        restrictions: None,
        required_linear_history: rules.require_linear_history,
        allow_force_pushes: rules.allow_force_push,
        allow_deletions: rules.allow_deletions,
        required_conversation_resolution: rules.required_conversation_resolution,
    }
}

fn dotted<'a>(data: &'a Table, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut value = data.get(parts.next()?)?;
    for part in parts {
        value = value.get(part)?;
    }
    Some(value)
}

fn dotted_str<'a>(data: &'a Table, path: &str) -> Option<&'a str> {
    dotted(data, path)?.as_str()
}

fn dotted_array<'a>(data: &'a Table, path: &str) -> Option<&'a Vec<Value>> {
    dotted(data, path)?.as_array()
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    glob_matches_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_matches_bytes(pattern: &[u8], text: &[u8]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            glob_matches_bytes(&pattern[1..], text)
                || (!text.is_empty() && glob_matches_bytes(pattern, &text[1..]))
        }
        (Some(pattern_byte), Some(text_byte)) if pattern_byte == text_byte => {
            glob_matches_bytes(&pattern[1..], &text[1..])
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use toml::Table;

    use super::{api_payload_from_rules, glob_matches, resolve_branch_rules};

    fn table(contents: &str) -> Table {
        contents.parse::<Table>().expect("toml")
    }

    #[test]
    fn solo_profile_uses_required_checks_from_governance() {
        let config = table(
            r#"
            [project]
            profile = "solo"

            [governance]
            required_status_checks = ["macOS", "Linux"]
            "#,
        );

        let rules = resolve_branch_rules(&config, "main").expect("rules");

        assert!(rules.require_pr);
        assert!(!rules.require_strict_status);
        assert_eq!(rules.require_status_checks, ["macOS", "Linux"]);
    }

    #[test]
    fn branch_overrides_extend_and_override_profile() {
        let config = table(
            r#"
            [project]
            profile = "multi"

            [branch_protection."main"]
            require_status_checks = ["ci"]

            [branch_protection."release/*"]
            extends = "main"
            require_review_count = 2
            allow_deletions = true
            "#,
        );

        let rules = resolve_branch_rules(&config, "release/1.0").expect("rules");

        assert_eq!(rules.require_status_checks, ["ci"]);
        assert_eq!(rules.require_review_count, 2);
        assert!(rules.enforce_admins);
        assert!(rules.allow_deletions);
    }

    #[test]
    fn branch_protection_payload_matches_python_shape() {
        let config = table(
            r#"
            [project]
            profile = "multi"

            [governance]
            required_status_checks = ["CI"]
            "#,
        );
        let rules = resolve_branch_rules(&config, "main").expect("rules");
        let payload = serde_json::to_value(api_payload_from_rules(&rules)).expect("payload");

        assert_eq!(
            payload,
            json!({
                "required_status_checks": {"strict": true, "contexts": ["CI"]},
                "enforce_admins": true,
                "required_pull_request_reviews": {
                    "required_approving_review_count": 1,
                    "dismiss_stale_reviews": true,
                    "require_code_owner_reviews": false
                },
                "restrictions": null,
                "required_linear_history": false,
                "allow_force_pushes": false,
                "allow_deletions": false,
                "required_conversation_resolution": true
            })
        );
    }

    #[test]
    fn glob_matching_covers_branch_patterns() {
        assert!(glob_matches("develop/*", "develop/next"));
        assert!(glob_matches("release/**", "release/1.0/hotfix"));
        assert!(!glob_matches("develop/*", "feature/next"));
    }
}
