//! Read-only supply-chain compliance audit for a governance profile.
//!
//! `shipyard governance status` compares branch protection against the
//! declared profile. This module covers the supply-chain rows of the profile
//! table in `docs/governance.md`: exact action refs, the workflow lockfile and
//! its `--verify` job, the updates bot, zizmor (report and enforce), workflow
//! `permissions:`, untrusted triggers, tag protection, immutable releases and
//! the default workflow token. Every input is read with `GET`; the audit never
//! writes.
//!
//! Row keys ([`Setting::key`]) are the same names the profile table uses, so a
//! report can be pasted next to the table. Evaluation is a pure function over a
//! [`RepoSnapshot`], so fixtures drive the tests and the live fetch is a thin
//! layer over [`GithubReader`].

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use toml::Table;

use crate::governance::GovernanceGh;

/// Path of the gh-actions-lock lockfile inside a repository.
pub const LOCKFILE_PATH: &str = ".github/workflows/actions.lock";

/// Dependency-update bot a profile expects for `github-actions`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdatesBot {
    /// GitHub Dependabot (`.github/dependabot.yml`). The default.
    Dependabot,
    /// Mend Renovate (`renovate.json` and friends). Same cadence and age.
    Renovate,
}

impl UpdatesBot {
    /// Parse a config or flag value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "dependabot" => Ok(Self::Dependabot),
            "renovate" => Ok(Self::Renovate),
            other => Err(format!(
                "unknown updates bot '{other}'; expected dependabot or renovate"
            )),
        }
    }

    /// Lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dependabot => "dependabot",
            Self::Renovate => "renovate",
        }
    }
}

/// Whether the lockfile `--verify` job must also be a required status check.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LockfileMode {
    /// The verify job runs and reports, but is not a required context.
    Advisory,
    /// The verify job's context is required on the default branch.
    Required,
}

impl LockfileMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "advisory" => Ok(Self::Advisory),
            "required" => Ok(Self::Required),
            other => Err(format!(
                "unknown lockfile mode '{other}'; expected advisory or required"
            )),
        }
    }

    /// Lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Advisory => "advisory",
            Self::Required => "required",
        }
    }
}

/// The supply-chain part of a governance profile.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AuditPolicy {
    /// Profile name (`solo`, `multi`, `custom`).
    pub profile: String,
    /// Expected updates bot.
    pub updates_bot: UpdatesBot,
    /// Minimum release age in days before the bot proposes an update.
    pub cooldown_days: u32,
    /// Maximum open update PRs for `github-actions`.
    pub open_pull_requests_limit: u32,
    /// Lockfile verify mode.
    pub lockfile_mode: LockfileMode,
    /// Status-check context the verify job produces.
    pub lockfile_context: String,
}

impl Default for AuditPolicy {
    fn default() -> Self {
        Self::for_profile("solo")
    }
}

impl AuditPolicy {
    /// Profile defaults: Dependabot, weekly, one group, limit 1, 7-day
    /// cooldown, lockfile verify advisory.
    #[must_use]
    pub fn for_profile(profile: &str) -> Self {
        Self {
            profile: profile.to_owned(),
            updates_bot: UpdatesBot::Dependabot,
            cooldown_days: 7,
            open_pull_requests_limit: 1,
            lockfile_mode: LockfileMode::Advisory,
            lockfile_context: "lockfile".to_owned(),
        }
    }

    /// Resolve from `[project].profile`, `[governance.updates]` and
    /// `[governance.lockfile]`.
    ///
    /// Switching bots is one value (`[governance.updates].bot`); the cadence,
    /// grouping and cooldown semantics stay the same.
    pub fn from_config(data: &Table) -> Result<Self, String> {
        let profile = data
            .get("project")
            .and_then(|project| project.get("profile"))
            .and_then(toml::Value::as_str)
            .unwrap_or("solo");
        let mut policy = Self::for_profile(profile);
        let section = |name: &str| {
            data.get("governance")
                .and_then(|governance| governance.get(name))
                .and_then(toml::Value::as_table)
        };
        if let Some(updates) = section("updates") {
            if let Some(bot) = updates.get("bot").and_then(toml::Value::as_str) {
                policy.updates_bot = UpdatesBot::parse(bot)?;
            }
            if let Some(days) = non_negative(updates, "updates", "cooldown_days")? {
                policy.cooldown_days = days;
            }
            if let Some(limit) = non_negative(updates, "updates", "open_pull_requests_limit")? {
                policy.open_pull_requests_limit = limit;
            }
        }
        if let Some(lockfile) = section("lockfile") {
            if let Some(mode) = lockfile.get("mode").and_then(toml::Value::as_str) {
                policy.lockfile_mode = LockfileMode::parse(mode)?;
            }
            if let Some(context) = lockfile.get("context").and_then(toml::Value::as_str) {
                context.clone_into(&mut policy.lockfile_context);
            }
        }
        Ok(policy)
    }
}

fn non_negative(table: &Table, section: &str, key: &str) -> Result<Option<u32>, String> {
    table
        .get(key)
        .map(|value| {
            value
                .as_integer()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or_else(|| format!("governance.{section}.{key} must be a non-negative integer"))
        })
        .transpose()
}

/// One repository file as read from the default branch.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowFile {
    /// Repository-relative path.
    pub path: String,
    /// File contents.
    pub content: String,
}

/// A tag-targeted ruleset, its ref patterns and its rule types.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TagRuleset {
    /// Ruleset name.
    pub name: String,
    /// `active`, `evaluate` or `disabled`.
    pub enforcement: String,
    /// `conditions.ref_name.include` patterns (`refs/tags/v*`, `~ALL`, ...).
    pub include: Vec<String>,
    /// Rule types (`deletion`, `update`, `non_fast_forward`, `creation`, ...).
    pub rules: Vec<String>,
}

/// Everything the audit reads from one repository.
///
/// A `None` live field with no matching `read_errors` entry is reported as
/// UNKNOWN rather than guessed.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct RepoSnapshot {
    /// `owner/repo`.
    pub repo: String,
    /// Branch the files were read from.
    pub default_branch: String,
    /// `.github/workflows/*.yml|yaml`.
    pub workflows: Vec<WorkflowFile>,
    /// `.github/workflows/actions.lock`, when present.
    pub lockfile: Option<String>,
    /// `.github/dependabot.yml|yaml`, when present.
    pub dependabot: Option<String>,
    /// First Renovate config found, when present.
    pub renovate: Option<WorkflowFile>,
    /// Required status-check contexts on the default branch.
    pub required_contexts: Option<Vec<String>>,
    /// Tag-targeted rulesets.
    pub tag_rulesets: Option<Vec<TagRuleset>>,
    /// Immutable-releases setting.
    pub immutable_releases: Option<bool>,
    /// `default_workflow_permissions` (`read` or `write`).
    pub default_workflow_permissions: Option<String>,
    /// Read failures, keyed by [`Setting::key`] (or `workflows`).
    pub read_errors: BTreeMap<String, String>,
}

/// One audited profile row. Keys equal the profile table's audit-row names.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Setting {
    /// Every `uses:` is an exact tag, or a full SHA with a version comment.
    ExactRefs,
    /// `actions.lock` committed and a pinned `gh actions-lock --verify` job.
    Lockfile,
    /// The updates bot covers `github-actions` with the profile's cadence.
    UpdatesBot,
    /// zizmor runs in CI (report mode is enough).
    ZizmorReport,
    /// zizmor runs with exit codes and a severity floor.
    ZizmorEnforce,
    /// Every workflow declares top-level `permissions:`.
    WorkflowPermissions,
    /// Every workflow starts from `permissions: {}`.
    PermissionsEmpty,
    /// No workflow triggers on `pull_request_target` or `workflow_run`.
    UntrustedTriggers,
    /// An active ruleset on `refs/tags/v*` forbids update, deletion, force.
    TagProtection,
    /// Immutable releases enabled.
    ImmutableReleases,
    /// Default workflow token is read-only.
    DefaultToken,
}

impl Setting {
    /// All rows, in report order.
    pub const ALL: [Self; 11] = [
        Self::ExactRefs,
        Self::Lockfile,
        Self::UpdatesBot,
        Self::ZizmorReport,
        Self::ZizmorEnforce,
        Self::WorkflowPermissions,
        Self::PermissionsEmpty,
        Self::UntrustedTriggers,
        Self::TagProtection,
        Self::ImmutableReleases,
        Self::DefaultToken,
    ];

    /// Stable row name, as used in the profile table.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::ExactRefs => "exact-refs",
            Self::Lockfile => "lockfile",
            Self::UpdatesBot => "updates-bot",
            Self::ZizmorReport => "zizmor-report",
            Self::ZizmorEnforce => "zizmor-enforce",
            Self::WorkflowPermissions => "workflow-permissions",
            Self::PermissionsEmpty => "permissions-empty",
            Self::UntrustedTriggers => "untrusted-triggers",
            Self::TagProtection => "tag-protection",
            Self::ImmutableReleases => "immutable-releases",
            Self::DefaultToken => "default-token",
        }
    }

    const fn reads_workflows(self) -> bool {
        matches!(
            self,
            Self::ExactRefs
                | Self::Lockfile
                | Self::ZizmorReport
                | Self::ZizmorEnforce
                | Self::WorkflowPermissions
                | Self::PermissionsEmpty
                | Self::UntrustedTriggers
        )
    }
}

/// Audit verdict for one row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditStatus {
    /// The repository meets the row.
    Pass,
    /// The repository does not meet it.
    Drift,
    /// The input could not be read, so no verdict is claimed.
    Unknown,
}

impl AuditStatus {
    /// Uppercase label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Drift => "DRIFT",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// One audit row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditRow {
    /// Row audited.
    pub setting: Setting,
    /// Verdict.
    pub status: AuditStatus,
    /// Human-readable evidence.
    pub detail: String,
}

/// The audit of one repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditReport {
    /// `owner/repo`.
    pub repo: String,
    /// Rows in [`Setting::ALL`] order.
    pub rows: Vec<AuditRow>,
}

impl AuditReport {
    /// Row for a setting.
    #[must_use]
    pub fn row(&self, setting: Setting) -> Option<&AuditRow> {
        self.rows.iter().find(|row| row.setting == setting)
    }

    /// Whether any row is DRIFT.
    #[must_use]
    pub fn has_drift(&self) -> bool {
        self.rows.iter().any(|row| row.status == AuditStatus::Drift)
    }

    /// Whether any row is UNKNOWN.
    #[must_use]
    pub fn has_unknown(&self) -> bool {
        self.rows
            .iter()
            .any(|row| row.status == AuditStatus::Unknown)
    }
}

/// How a `uses:` reference is pinned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefKind {
    /// Full 40-hex commit SHA with a `# vX.Y.Z` comment.
    Sha,
    /// Full SHA with no version comment.
    BareSha,
    /// Exact `vX.Y.Z` tag.
    ExactTag,
    /// A moving ref: `v4`, `main`, `v1.2`, or no ref at all.
    Floating,
    /// `./local` action or `docker://` image (out of the lockfile's scope).
    Local,
}

/// One action reference found in a workflow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionRef {
    /// Workflow path.
    pub path: String,
    /// 1-based line number.
    pub line: usize,
    /// The `uses:` value.
    pub uses: String,
    /// Classification.
    pub kind: RefKind,
}

static USES_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^\s*(?:-\s+)?uses:\s*["']?([^\s"'#]+)["']?(?:\s*(#.*))?"#)
        .expect("valid uses regex")
});
static SHA_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{40}$").expect("valid sha regex"));
static EXACT_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v?\d+\.\d+\.\d+$").expect("valid tag regex"));
static VERSION_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#\s*v?\d+(\.\d+)*").expect("valid comment regex"));
static TOP_LEVEL_PERMISSIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^permissions\s*:").expect("valid permissions regex"));
static EMPTY_PERMISSIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^permissions\s*:\s*\{\s*\}\s*(#.*)?$").expect("valid empty regex")
});
static VERIFY_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^[^#\n]*\bactions-lock\b[^#\n]*--verify(?:\s|$)").expect("valid verify regex")
});
static EXTENSION_PIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"gh\s+extension\s+install\s+github/gh-actions-lock(?:\s+--pin[\s=]+([^\s#]+))?")
        .expect("valid extension regex")
});
static ZIZMOR_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^[^#\n]*\bzizmor").expect("valid zizmor regex"));
static UNTRUSTED_TRIGGER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(pull_request_target|workflow_run)\b").expect("valid trigger regex")
});
static RENOVATE_AGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""?minimumReleaseAge"?\s*:\s*["']?(\d+)\s*days?"#).expect("valid renovate regex")
});

/// Classify one `uses:` value; `comment` is the rest of the line after `#`.
#[must_use]
pub fn classify_uses(value: &str, comment: Option<&str>) -> RefKind {
    if value.starts_with("./") || value.starts_with("docker://") {
        return RefKind::Local;
    }
    let Some((_, reference)) = value.rsplit_once('@') else {
        return RefKind::Floating;
    };
    if SHA_REF.is_match(reference) {
        if comment.is_some_and(|comment| VERSION_COMMENT.is_match(comment)) {
            RefKind::Sha
        } else {
            RefKind::BareSha
        }
    } else if EXACT_TAG.is_match(reference) {
        RefKind::ExactTag
    } else {
        RefKind::Floating
    }
}

/// Every `uses:` reference in the workflows.
#[must_use]
pub fn action_refs(workflows: &[WorkflowFile]) -> Vec<ActionRef> {
    let mut refs = Vec::new();
    for workflow in workflows {
        for (index, line) in workflow.content.lines().enumerate() {
            if let Some(captures) = USES_LINE.captures(line) {
                let uses = captures[1].to_owned();
                let comment = captures.get(2).map(|comment| comment.as_str());
                refs.push(ActionRef {
                    path: workflow.path.clone(),
                    line: index + 1,
                    kind: classify_uses(&uses, comment),
                    uses,
                });
            }
        }
    }
    refs
}

/// Audit one snapshot against a policy. Pure: no I/O.
#[must_use]
pub fn audit_snapshot(snapshot: &RepoSnapshot, policy: &AuditPolicy) -> AuditReport {
    let rows = Setting::ALL
        .iter()
        .map(|&setting| {
            let error = snapshot.read_errors.get(setting.key()).or_else(|| {
                setting
                    .reads_workflows()
                    .then(|| snapshot.read_errors.get("workflows"))
                    .flatten()
            });
            if let Some(error) = error {
                return row(
                    setting,
                    AuditStatus::Unknown,
                    format!("read failed: {error}"),
                );
            }
            match setting {
                Setting::ExactRefs => check_exact_refs(snapshot),
                Setting::Lockfile => check_lockfile(snapshot, policy),
                Setting::UpdatesBot => check_updates_bot(snapshot, policy),
                Setting::ZizmorReport => check_zizmor_report(snapshot),
                Setting::ZizmorEnforce => check_zizmor_enforce(snapshot),
                Setting::WorkflowPermissions => check_permissions(snapshot),
                Setting::PermissionsEmpty => check_permissions_empty(snapshot),
                Setting::UntrustedTriggers => check_untrusted_triggers(snapshot, policy),
                Setting::TagProtection => check_tags(snapshot, policy),
                Setting::ImmutableReleases => check_immutable(snapshot),
                Setting::DefaultToken => check_token(snapshot),
            }
        })
        .collect();
    AuditReport {
        repo: snapshot.repo.clone(),
        rows,
    }
}

fn row(setting: Setting, status: AuditStatus, detail: impl Into<String>) -> AuditRow {
    AuditRow {
        setting,
        status,
        detail: detail.into(),
    }
}

fn verdict(setting: Setting, pass: bool, detail: impl Into<String>) -> AuditRow {
    row(
        setting,
        if pass {
            AuditStatus::Pass
        } else {
            AuditStatus::Drift
        },
        detail,
    )
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn check_exact_refs(snapshot: &RepoSnapshot) -> AuditRow {
    let refs = action_refs(&snapshot.workflows);
    let count = |kind| refs.iter().filter(|item| item.kind == kind).count();
    let (sha, bare, exact, floating) = (
        count(RefKind::Sha),
        count(RefKind::BareSha),
        count(RefKind::ExactTag),
        count(RefKind::Floating),
    );
    let mut detail =
        format!("{exact} exact tag, {sha} SHA+comment, {bare} bare SHA, {floating} floating");
    if let Some(first) = refs
        .iter()
        .find(|item| matches!(item.kind, RefKind::Floating | RefKind::BareSha))
    {
        let _ = write!(
            detail,
            "; e.g. {}:{} {}",
            file_name(&first.path),
            first.line,
            first.uses
        );
    }
    verdict(Setting::ExactRefs, floating == 0 && bare == 0, detail)
}

fn check_lockfile(snapshot: &RepoSnapshot, policy: &AuditPolicy) -> AuditRow {
    let drift = |detail: String| row(Setting::Lockfile, AuditStatus::Drift, detail);
    if snapshot.lockfile.is_none() {
        return drift(format!("no {LOCKFILE_PATH}"));
    }
    let Some(workflow) = snapshot
        .workflows
        .iter()
        .find(|workflow| VERIFY_COMMAND.is_match(&workflow.content))
    else {
        return drift("lockfile present, but no workflow runs `gh actions-lock --verify`".into());
    };
    let name = file_name(&workflow.path);
    match EXTENSION_PIN
        .captures(&workflow.content)
        .map(|captures| captures.get(1).map(|pin| pin.as_str().to_owned()))
    {
        Some(Some(pin)) if SHA_REF.is_match(&pin) => {}
        Some(Some(pin)) => {
            return drift(format!(
                "{name}: extension pinned to `{pin}`, not a full commit SHA"
            ));
        }
        Some(None) | None => {
            return drift(format!(
                "{name}: gh-actions-lock extension is not installed with --pin <sha>"
            ));
        }
    }
    let required = snapshot
        .required_contexts
        .as_ref()
        .is_some_and(|contexts| contexts.iter().any(|c| c == &policy.lockfile_context));
    match policy.lockfile_mode {
        LockfileMode::Advisory => row(
            Setting::Lockfile,
            AuditStatus::Pass,
            format!(
                "{name} verifies (advisory{})",
                if required { "; already required" } else { "" }
            ),
        ),
        LockfileMode::Required if snapshot.required_contexts.is_none() => row(
            Setting::Lockfile,
            AuditStatus::Unknown,
            "required contexts could not be read",
        ),
        LockfileMode::Required => verdict(
            Setting::Lockfile,
            required,
            format!(
                "{name} verifies; context `{}` {}",
                policy.lockfile_context,
                if required {
                    "is required"
                } else {
                    "is NOT required"
                }
            ),
        ),
    }
}

/// The `updates:` entry for the github-actions ecosystem, as text.
fn dependabot_actions_block(config: &str) -> Option<String> {
    let mut block: Option<Vec<&str>> = None;
    let mut indent = 0;
    for line in config.lines() {
        let trimmed = line.trim_start();
        let this_indent = line.len() - trimmed.len();
        if let Some(rest) = trimmed.strip_prefix("- ") {
            if block.is_some() && this_indent <= indent {
                break;
            }
            if rest.trim_start().starts_with("package-ecosystem:")
                && rest.contains("github-actions")
            {
                indent = this_indent;
                block = Some(vec![rest]);
                continue;
            }
        }
        if let Some(lines) = block.as_mut() {
            if !trimmed.is_empty() && this_indent <= indent && !trimmed.starts_with('#') {
                break;
            }
            lines.push(trimmed);
        }
    }
    block.map(|lines| lines.join("\n"))
}

fn yaml_scalar<'a>(block: &'a str, key: &str) -> Option<&'a str> {
    block.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix(key)?.strip_prefix(':')?;
        Some(rest.split('#').next()?.trim().trim_matches(['"', '\'']))
    })
}

fn yaml_number(block: &str, key: &str) -> Option<u32> {
    yaml_scalar(block, key)?.parse().ok()
}

fn check_updates_bot(snapshot: &RepoSnapshot, policy: &AuditPolicy) -> AuditRow {
    let want = format!(
        "want {}: github-actions, weekly, one group, limit <= {}, cooldown >= {}d",
        policy.updates_bot.as_str(),
        policy.open_pull_requests_limit,
        policy.cooldown_days
    );
    let other = match policy.updates_bot {
        UpdatesBot::Dependabot => snapshot
            .renovate
            .as_ref()
            .map(|config| format!("; {} present", config.path)),
        UpdatesBot::Renovate => snapshot
            .dependabot
            .as_ref()
            .map(|_| "; .github/dependabot.yml present".to_owned()),
    }
    .unwrap_or_default();
    let drift = |problem: &str| {
        row(
            Setting::UpdatesBot,
            AuditStatus::Drift,
            format!("{problem}{other}; {want}"),
        )
    };
    match policy.updates_bot {
        UpdatesBot::Dependabot => {
            let Some(config) = &snapshot.dependabot else {
                return drift("no .github/dependabot.yml");
            };
            let Some(block) = dependabot_actions_block(config) else {
                return drift("dependabot.yml has no github-actions entry");
            };
            let interval = yaml_scalar(&block, "interval");
            let cooldown = yaml_number(&block, "default-days");
            // Dependabot's documented default when the key is absent.
            let limit = yaml_number(&block, "open-pull-requests-limit").unwrap_or(5);
            let grouped = block.lines().any(|line| line.trim_start() == "groups:");
            let mut problems = Vec::new();
            if interval != Some("weekly") {
                problems.push(format!("interval {}", interval.unwrap_or("-")));
            }
            if !grouped {
                problems.push("no group".to_owned());
            }
            if limit > policy.open_pull_requests_limit {
                problems.push(format!("limit {limit}"));
            }
            match cooldown {
                None => problems.push("no cooldown".to_owned()),
                Some(days) if days < policy.cooldown_days => {
                    problems.push(format!("cooldown {days}d"));
                }
                Some(_) => {}
            }
            if problems.is_empty() {
                row(
                    Setting::UpdatesBot,
                    AuditStatus::Pass,
                    format!(
                        "dependabot weekly, grouped, limit {limit}, cooldown {}d{other}",
                        cooldown.unwrap_or_default()
                    ),
                )
            } else {
                drift(&format!("dependabot: {}", problems.join(", ")))
            }
        }
        UpdatesBot::Renovate => {
            let Some(config) = &snapshot.renovate else {
                return drift("no Renovate config");
            };
            let age = RENOVATE_AGE
                .captures(&config.content)
                .and_then(|captures| captures[1].parse::<u32>().ok());
            let grouped = config.content.contains("groupName");
            match age {
                Some(days) if days >= policy.cooldown_days && grouped => row(
                    Setting::UpdatesBot,
                    AuditStatus::Pass,
                    format!(
                        "renovate {}: minimumReleaseAge {days} days, grouped (schedule not text-checked){other}",
                        config.path
                    ),
                ),
                Some(days) if days >= policy.cooldown_days => {
                    drift(&format!("renovate {}: no groupName", config.path))
                }
                Some(days) => drift(&format!(
                    "renovate {}: minimumReleaseAge {days} days",
                    config.path
                )),
                None => drift(&format!("renovate {}: no minimumReleaseAge", config.path)),
            }
        }
    }
}

fn zizmor_workflows(snapshot: &RepoSnapshot) -> Vec<&WorkflowFile> {
    snapshot
        .workflows
        .iter()
        .filter(|workflow| ZIZMOR_LINE.is_match(&workflow.content))
        .collect()
}

fn check_zizmor_report(snapshot: &RepoSnapshot) -> AuditRow {
    match zizmor_workflows(snapshot).first() {
        Some(workflow) => row(
            Setting::ZizmorReport,
            AuditStatus::Pass,
            file_name(&workflow.path),
        ),
        None => row(
            Setting::ZizmorReport,
            AuditStatus::Drift,
            "no workflow runs zizmor",
        ),
    }
}

fn check_zizmor_enforce(snapshot: &RepoSnapshot) -> AuditRow {
    let workflows = zizmor_workflows(snapshot);
    if workflows.is_empty() {
        return row(
            Setting::ZizmorEnforce,
            AuditStatus::Drift,
            "no workflow runs zizmor",
        );
    }
    match workflows.iter().find(|workflow| {
        workflow.content.contains("min-severity") && !workflow.content.contains("--no-exit-codes")
    }) {
        Some(workflow) => row(
            Setting::ZizmorEnforce,
            AuditStatus::Pass,
            file_name(&workflow.path),
        ),
        None => row(
            Setting::ZizmorEnforce,
            AuditStatus::Drift,
            "zizmor runs in report mode only (no severity floor with exit codes)",
        ),
    }
}

fn check_permissions(snapshot: &RepoSnapshot) -> AuditRow {
    let missing: Vec<&str> = snapshot
        .workflows
        .iter()
        .filter(|workflow| !TOP_LEVEL_PERMISSIONS.is_match(&workflow.content))
        .map(|workflow| file_name(&workflow.path))
        .collect();
    verdict(
        Setting::WorkflowPermissions,
        missing.is_empty(),
        if missing.is_empty() {
            format!("{} workflow(s) declare it", snapshot.workflows.len())
        } else {
            format!(
                "{} of {} missing: {}",
                missing.len(),
                snapshot.workflows.len(),
                missing.join(", ")
            )
        },
    )
}

fn check_permissions_empty(snapshot: &RepoSnapshot) -> AuditRow {
    let empty = snapshot
        .workflows
        .iter()
        .filter(|workflow| EMPTY_PERMISSIONS.is_match(&workflow.content))
        .count();
    let total = snapshot.workflows.len();
    verdict(
        Setting::PermissionsEmpty,
        empty == total,
        format!("{empty} of {total} start from `permissions: {{}}`"),
    )
}

/// The `on:` part of a workflow: everything before the top-level `jobs:`.
fn trigger_section(content: &str) -> String {
    content
        .lines()
        .take_while(|line| !line.starts_with("jobs:"))
        .map(|line| line.split('#').next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_untrusted_triggers(snapshot: &RepoSnapshot, policy: &AuditPolicy) -> AuditRow {
    let mut hits = Vec::new();
    for workflow in &snapshot.workflows {
        let section = trigger_section(&workflow.content);
        let mut triggers: Vec<&str> = UNTRUSTED_TRIGGER
            .find_iter(&section)
            .map(|found| found.as_str())
            .collect();
        triggers.dedup();
        if !triggers.is_empty() {
            hits.push(format!(
                "{} ({})",
                file_name(&workflow.path),
                triggers.join(", ")
            ));
        }
    }
    if hits.is_empty() {
        return row(
            Setting::UntrustedTriggers,
            AuditStatus::Pass,
            "no pull_request_target or workflow_run",
        );
    }
    let note = if policy.profile == "multi" {
        ""
    } else {
        "; solo allows these only by a per-workflow event policy, which the audit cannot read"
    };
    row(
        Setting::UntrustedTriggers,
        AuditStatus::Drift,
        format!("{}{note}", hits.join(", ")),
    )
}

fn covers_release_tags(ruleset: &TagRuleset) -> bool {
    ruleset.include.iter().any(|pattern| {
        matches!(
            pattern.as_str(),
            "~ALL" | "refs/tags/*" | "refs/tags/**/*" | "refs/tags/v*"
        )
    })
}

fn check_tags(snapshot: &RepoSnapshot, policy: &AuditPolicy) -> AuditRow {
    let Some(rulesets) = &snapshot.tag_rulesets else {
        return row(
            Setting::TagProtection,
            AuditStatus::Unknown,
            "tag rulesets not read",
        );
    };
    let active: Vec<&TagRuleset> = rulesets
        .iter()
        .filter(|ruleset| ruleset.enforcement == "active" && covers_release_tags(ruleset))
        .collect();
    if active.is_empty() {
        return row(
            Setting::TagProtection,
            AuditStatus::Drift,
            "no active ruleset on refs/tags/v*",
        );
    }
    let mut wanted = vec!["update", "deletion", "non_fast_forward"];
    if policy.profile == "multi" {
        wanted.push("creation");
    }
    let missing: Vec<&str> = wanted
        .into_iter()
        .filter(|rule| {
            !active
                .iter()
                .any(|ruleset| ruleset.rules.iter().any(|have| have == rule))
        })
        .collect();
    let names = active
        .iter()
        .map(|ruleset| ruleset.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    verdict(
        Setting::TagProtection,
        missing.is_empty(),
        if missing.is_empty() {
            names
        } else {
            format!("{names}: missing {}", missing.join(", "))
        },
    )
}

fn check_immutable(snapshot: &RepoSnapshot) -> AuditRow {
    match snapshot.immutable_releases {
        Some(true) => row(Setting::ImmutableReleases, AuditStatus::Pass, "enabled"),
        Some(false) => row(Setting::ImmutableReleases, AuditStatus::Drift, "disabled"),
        None => row(
            Setting::ImmutableReleases,
            AuditStatus::Unknown,
            "setting not readable",
        ),
    }
}

fn check_token(snapshot: &RepoSnapshot) -> AuditRow {
    match snapshot.default_workflow_permissions.as_deref() {
        Some("read") => row(Setting::DefaultToken, AuditStatus::Pass, "read"),
        Some(other) => row(Setting::DefaultToken, AuditStatus::Drift, other.to_owned()),
        None => row(
            Setting::DefaultToken,
            AuditStatus::Unknown,
            "setting not readable",
        ),
    }
}

/// GET-only GitHub access the audit needs.
pub trait GithubReader {
    /// GET a JSON resource; `Ok(None)` on 404.
    fn get_json(&self, path: &str) -> Result<Option<JsonValue>, String>;
    /// GET a file's raw contents; `Ok(None)` on 404.
    fn get_raw(&self, path: &str) -> Result<Option<String>, String>;
}

impl GithubReader for GovernanceGh {
    fn get_json(&self, path: &str) -> Result<Option<JsonValue>, String> {
        let Some(body) = self.api_get(path, None)? else {
            return Ok(None);
        };
        serde_json::from_str(&body)
            .map(Some)
            .map_err(|error| format!("{path}: non-JSON response: {error}"))
    }

    fn get_raw(&self, path: &str) -> Result<Option<String>, String> {
        self.api_get(path, Some("Accept: application/vnd.github.raw+json"))
    }
}

const RENOVATE_PATHS: [&str; 6] = [
    "renovate.json",
    "renovate.json5",
    ".github/renovate.json",
    ".github/renovate.json5",
    ".renovaterc",
    ".renovaterc.json",
];

/// Read everything the audit needs from one repository, GET only.
#[must_use]
pub fn fetch_snapshot(repo: &str, reader: &dyn GithubReader) -> RepoSnapshot {
    let mut snapshot = RepoSnapshot {
        repo: repo.to_owned(),
        ..RepoSnapshot::default()
    };
    let branch = match reader.get_json(&format!("repos/{repo}")) {
        Ok(Some(value)) => value["default_branch"]
            .as_str()
            .unwrap_or("main")
            .to_owned(),
        failure => {
            let error = match failure {
                Err(error) => error,
                _ => "repository not found or not readable".to_owned(),
            };
            for setting in Setting::ALL {
                snapshot
                    .read_errors
                    .insert(setting.key().to_owned(), error.clone());
            }
            return snapshot;
        }
    };
    snapshot.default_branch.clone_from(&branch);
    let contents = |path: &str| format!("repos/{repo}/contents/{path}?ref={branch}");

    fetch_workflow_files(&mut snapshot, reader, &contents);
    fetch_bot_configs(&mut snapshot, reader, &contents);

    snapshot.required_contexts = fetch_required_contexts(repo, &branch, reader);
    match fetch_tag_rulesets(repo, reader) {
        Ok(rulesets) => snapshot.tag_rulesets = Some(rulesets),
        Err(error) => {
            snapshot
                .read_errors
                .insert("tag-protection".to_owned(), error);
        }
    }
    match reader.get_json(&format!("repos/{repo}/immutable-releases")) {
        Ok(Some(value)) => snapshot.immutable_releases = value["enabled"].as_bool(),
        Ok(None) => {
            snapshot
                .read_errors
                .insert("immutable-releases".to_owned(), "HTTP 404".to_owned());
        }
        Err(error) => {
            snapshot
                .read_errors
                .insert("immutable-releases".to_owned(), error);
        }
    }
    match reader.get_json(&format!("repos/{repo}/actions/permissions/workflow")) {
        Ok(Some(value)) => {
            snapshot.default_workflow_permissions = value["default_workflow_permissions"]
                .as_str()
                .map(str::to_owned);
        }
        Ok(None) => {
            snapshot
                .read_errors
                .insert("default-token".to_owned(), "HTTP 404".to_owned());
        }
        Err(error) => {
            snapshot
                .read_errors
                .insert("default-token".to_owned(), error);
        }
    }
    snapshot
}

/// Workflow files and the lockfile from `.github/workflows`.
fn fetch_workflow_files(
    snapshot: &mut RepoSnapshot,
    reader: &dyn GithubReader,
    contents: &dyn Fn(&str) -> String,
) {
    match reader.get_json(&contents(".github/workflows")) {
        Ok(Some(JsonValue::Array(entries))) => {
            for entry in entries {
                let (Some(name), Some(path)) = (entry["name"].as_str(), entry["path"].as_str())
                else {
                    continue;
                };
                let is_workflow = std::path::Path::new(name).extension().is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml")
                });
                if name != "actions.lock" && !is_workflow {
                    continue;
                }
                match reader.get_raw(&contents(path)) {
                    Ok(Some(text)) if is_workflow => snapshot.workflows.push(WorkflowFile {
                        path: path.to_owned(),
                        content: text,
                    }),
                    Ok(text) => {
                        if !is_workflow {
                            snapshot.lockfile = text;
                        }
                    }
                    Err(error) => {
                        snapshot.read_errors.insert("workflows".to_owned(), error);
                    }
                }
            }
        }
        Ok(Some(_) | None) => {}
        Err(error) => {
            snapshot.read_errors.insert("workflows".to_owned(), error);
        }
    }
}

/// The first Dependabot and the first Renovate config found.
fn fetch_bot_configs(
    snapshot: &mut RepoSnapshot,
    reader: &dyn GithubReader,
    contents: &dyn Fn(&str) -> String,
) {
    let bot_paths = [".github/dependabot.yml", ".github/dependabot.yaml"]
        .into_iter()
        .map(|path| (true, path))
        .chain(RENOVATE_PATHS.into_iter().map(|path| (false, path)));
    for (dependabot, path) in bot_paths {
        if (dependabot && snapshot.dependabot.is_some())
            || (!dependabot && snapshot.renovate.is_some())
        {
            continue;
        }
        match reader.get_raw(&contents(path)) {
            Ok(Some(content)) if dependabot => snapshot.dependabot = Some(content),
            Ok(Some(content)) => {
                snapshot.renovate = Some(WorkflowFile {
                    path: path.to_owned(),
                    content,
                });
            }
            Ok(None) => {}
            Err(error) => {
                snapshot.read_errors.insert("updates-bot".to_owned(), error);
            }
        }
    }
}

fn fetch_required_contexts(
    repo: &str,
    branch: &str,
    reader: &dyn GithubReader,
) -> Option<Vec<String>> {
    let mut contexts = Vec::new();
    let mut read_any = false;
    if let Ok(Some(JsonValue::Array(rules))) =
        reader.get_json(&format!("repos/{repo}/rules/branches/{branch}"))
    {
        read_any = true;
        for rule in rules {
            if rule["type"] == "required_status_checks" {
                for check in rule["parameters"]["required_status_checks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if let Some(context) = check["context"].as_str() {
                        contexts.push(context.to_owned());
                    }
                }
            }
        }
    }
    match reader.get_json(&format!(
        "repos/{repo}/branches/{branch}/protection/required_status_checks"
    )) {
        Ok(Some(value)) => {
            read_any = true;
            for context in value["contexts"].as_array().into_iter().flatten() {
                if let Some(context) = context.as_str() {
                    contexts.push(context.to_owned());
                }
            }
        }
        Ok(None) => read_any = true,
        Err(_) => {}
    }
    read_any.then_some(contexts)
}

fn fetch_tag_rulesets(repo: &str, reader: &dyn GithubReader) -> Result<Vec<TagRuleset>, String> {
    let list = reader
        .get_json(&format!(
            "repos/{repo}/rulesets?targets=tag&includes_parents=true&per_page=100"
        ))?
        .unwrap_or(JsonValue::Array(Vec::new()));
    let mut rulesets = Vec::new();
    for summary in list.as_array().into_iter().flatten() {
        if summary["target"] != "tag" {
            continue;
        }
        let Some(id) = summary["id"].as_u64() else {
            continue;
        };
        let detail = reader
            .get_json(&format!("repos/{repo}/rulesets/{id}"))?
            .unwrap_or(JsonValue::Null);
        let strings = |value: &JsonValue| -> Vec<String> {
            value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        };
        rulesets.push(TagRuleset {
            name: summary["name"].as_str().unwrap_or_default().to_owned(),
            enforcement: summary["enforcement"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            include: strings(&detail["conditions"]["ref_name"]["include"]),
            rules: detail["rules"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|rule| rule["type"].as_str().map(str::to_owned))
                .collect(),
        });
    }
    Ok(rulesets)
}

#[cfg(test)]
mod tests;
