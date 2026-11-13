//! Design A governance onboarding: the read-only plan.
//!
//! `shipyard governance onboard` prints a plan and writes nothing. Every
//! input is read from GitHub with GET requests, and any read that fails stops
//! the plan (exit 9) instead of standing in an empty value. The write path
//! (`--apply`) is not implemented; the CLI refuses it before any read.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::governance_audit::GithubReader;

/// Files an onboarding PR may change.
pub const ALLOWED_PATHS: &[&str] = &[
    ".github/workflows/review-verdict.yml",
    ".github/workflows/review-verdict-trigger.yml",
    ".shipyard/config.toml",
];
/// GitHub's Actions App id; `review-verdict` check runs must come from it.
pub const ACTIONS_INTEGRATION_ID: u64 = 15_368;
/// The repository whose pinned commit the rendered workflow runs.
pub const SHIPYARD_REPO: &str = "danielraffel/Shipyard";
/// The commit-status and check-run context the rendered workflow produces.
pub const REVIEW_VERDICT_CONTEXT: &str = "review-verdict";
/// Onboarding steps in order; `--through` takes any of these names.
pub const STEPS: &[&str] = &[
    "preflight",
    "pr",
    "approved",
    "green",
    "merged",
    "tightened",
    "evaluated",
    "verified",
    "handed_over",
    "audited",
    "enrolled",
];
/// `[auto_merge]` keys the onboarding PR may set; every other key is kept.
pub const AUTO_MERGE_KEYS: &[&str] = &[
    "reviewer_signers",
    "reviewer_stamp_sessions",
    "fleet_review_required",
];
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 50;

/// The rendered `review-verdict.yml`. `@VERSION@` and `@COMMIT@` name the
/// pinned Shipyard release.
const REVIEW_VERDICT_TEMPLATE: &str = r#"# Posts the `review-verdict` commit status on each pull request's head: the
# repository-side twin of Shipyard's review-before-arm guard. The job is also
# the `review-verdict` check run and fails unless the head is allowed, so a
# required check cannot be green on a refused head. The rule runs from a pinned
# Shipyard commit, never from the pull request's code, on triggers that all run
# the default branch's definition. Rendered by
# `shipyard governance onboard`; see Shipyard's docs/review-verdict-status.md.
name: Review verdict

on:
  pull_request_target:
    types: [opened, reopened, synchronize, ready_for_review]
  issue_comment:
    types: [created, edited, deleted]
  workflow_run:
    workflows: [Review verdict trigger]
    types: [completed]
  check_run:
    types: [completed]

permissions:
  contents: read
  pull-requests: read
  checks: read
  statuses: write
  actions: write

concurrency:
  group: review-verdict-${{ github.event.pull_request.number || github.event.issue.number || github.event.workflow_run.pull_requests[0].number || github.event.check_run.pull_requests[0].number || github.event.workflow_run.head_sha || github.event.check_run.head_sha }}
  cancel-in-progress: false

jobs:
  review-verdict:
    if: >-
      (github.event_name != 'issue_comment' || github.event.issue.pull_request)
      && (github.event_name != 'check_run' || github.event.check_run.name == 'fleet-guard/admission')
    runs-on: ubuntu-24.04
    timeout-minutes: 5
    steps:
      - name: Check out the pinned rule
        uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          repository: danielraffel/Shipyard
          # Shipyard v@VERSION@
          ref: @COMMIT@
          persist-credentials: false
      - name: Set up Python
        id: python
        uses: actions/setup-python@ece7cb06caefa5fff74198d8649806c4678c61a1 # v6.3.0
        with:
          python-version: '3.12.12'
      - name: Post review-verdict
        env:
          GH_TOKEN: ${{ github.token }}
        run: '"${{ steps.python.outputs.python-path }}" scripts/review_verdict_status.py'
"#;

/// The rendered permission-less trigger workflow.
pub const REVIEW_VERDICT_TRIGGER: &str =
    include_str!("../.github/workflows/review-verdict-trigger.yml");

/// Why a plan stopped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OnboardError {
    /// A guard refused the request (exit 1).
    Refused(String),
    /// A GitHub read failed or answered something unusable (exit 9).
    Unreadable(String),
}

impl OnboardError {
    /// The command's exit code for this error.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Refused(_) => 1,
            Self::Unreadable(_) => 9,
        }
    }

    /// The human-readable reason.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Refused(message) | Self::Unreadable(message) => message,
        }
    }
}

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

/// Inputs to [`build_plan`].
#[derive(Clone, Debug, Default)]
pub struct OnboardRequest {
    /// `OWNER/NAME`.
    pub repo: String,
    /// `fleet` or `builder`.
    pub profile: String,
    /// Existing CI contexts to require.
    pub require_checks: Vec<String>,
    /// Last step to include; `None` means the default.
    pub through: Option<String>,
    /// Stop after `audited`.
    pub no_enroll: bool,
    /// `[auto_merge] reviewer_signers`; empty keeps the base's own list.
    pub reviewer_signers: Vec<String>,
    /// `[auto_merge] reviewer_stamp_sessions`; empty keeps the base's own list.
    pub stamp_sessions: Vec<String>,
    /// The running Shipyard release, whose `v<version>` tag is pinned.
    pub shipyard_version: String,
}

/// The Shipyard commit the rendered workflow runs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ShipyardPin {
    /// Release version, without the `v`.
    pub version: String,
    /// The tag's 40-hex commit.
    pub commit: String,
}

/// A rendered `.shipyard/config.toml` and the named keys it changed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RenderedConfig {
    /// Full file text.
    pub text: String,
    /// Dotted keys whose value differs from the base's file.
    pub changed_keys: Vec<String>,
}

/// One open pull request in the first-evaluation sweep.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SweepRow {
    /// PR number.
    pub number: u64,
    /// Exact head.
    pub head: String,
    /// `skip-existing` when the head already carries a verdict, otherwise
    /// the trigger the sweep would fire.
    pub trigger: String,
    /// State of the head's `review-verdict` commit status, if any.
    pub status_state: Option<String>,
    /// The head's latest Actions `review-verdict` check run, if any.
    pub check_run_id: Option<u64>,
    /// That check run's conclusion, if it has one.
    pub conclusion: Option<String>,
}

/// Validate an `OWNER/NAME` slug.
pub fn validate_repo(repo: &str) -> Result<(), String> {
    let part_ok = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    match repo.split_once('/') {
        Some((owner, name)) if part_ok(owner) && part_ok(name) => Ok(()),
        _ => Err(format!("--repo must be OWNER/NAME, not {repo:?}")),
    }
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

/// Validate the onboarding PR path allowlist.
pub fn validate_path_allowlist(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Err("onboarding PR has no files".to_owned());
    }
    for path in paths {
        if !ALLOWED_PATHS.contains(&path.as_str()) {
            return Err(format!("onboarding PR touches disallowed path: {path}"));
        }
    }
    Ok(())
}

/// The last included step: `--through`, `audited` for `--no-enroll`, else
/// `enrolled`.
pub fn resolve_through(through: Option<&str>, no_enroll: bool) -> Result<&'static str, String> {
    match (through, no_enroll) {
        (Some(_), true) => Err("--no-enroll cannot be combined with --through".to_owned()),
        (None, true) => Ok("audited"),
        (None, false) => Ok("enrolled"),
        (Some(name), false) => STEPS
            .iter()
            .copied()
            .find(|step| *step == name)
            .ok_or_else(|| format!("unknown --through step {name:?}")),
    }
}

/// Resolve the running release's `v<version>` tag to its commit.
pub fn resolve_pin(reader: &dyn GithubReader, version: &str) -> Result<ShipyardPin, OnboardError> {
    let path = format!("repos/{SHIPYARD_REPO}/commits/v{version}");
    let value = read_json(reader, &path)?;
    let commit = value
        .get("sha")
        .and_then(Value::as_str)
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| OnboardError::Unreadable(format!("{path}: no 40-hex commit sha")))?;
    Ok(ShipyardPin {
        version: version.to_owned(),
        commit: commit.to_ascii_lowercase(),
    })
}

/// Render `review-verdict.yml` pinned to `pin`.
#[must_use]
pub fn render_review_verdict(pin: &ShipyardPin) -> String {
    REVIEW_VERDICT_TEMPLATE
        .replace("@VERSION@", &pin.version)
        .replace("@COMMIT@", &pin.commit)
}

/// Render `.shipyard/config.toml`: `existing` with only [`AUTO_MERGE_KEYS`]
/// set, and `[validation] pr_gates = false` added only when the base sets no
/// `pr_gates` and has no gate scripts. An existing `pr_gates` is never changed
/// (a `true` is never lowered), and `shipyard pr` refuses `false` beside gate
/// scripts. Every other key, table and comment is kept.
///
/// An empty `signers` or `sessions` keeps the base's own list; when the base
/// has none either, the render refuses.
pub fn render_config(
    existing: Option<&str>,
    signers: &[String],
    sessions: &[String],
    gate_scripts_present: bool,
) -> Result<RenderedConfig, OnboardError> {
    let source = existing.unwrap_or("");
    let mut document: toml_edit::DocumentMut = source.parse().map_err(|error| {
        OnboardError::Refused(format!(
            "the base's .shipyard/config.toml does not parse: {error}"
        ))
    })?;
    let before: toml::Table = toml::from_str(source).map_err(|error| {
        OnboardError::Refused(format!(
            "the base's .shipyard/config.toml does not parse: {error}"
        ))
    })?;
    let existing_list = |key: &str| -> Option<Vec<String>> {
        before
            .get("auto_merge")?
            .get(key)?
            .as_array()?
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect()
    };
    let signers = if signers.is_empty() {
        existing_list("reviewer_signers").filter(|list| !list.is_empty())
    } else {
        Some(signers.to_vec())
    }
    .ok_or_else(|| {
        OnboardError::Refused(
            "no reviewer signers: pass --reviewer-signer (the base sets no reviewer_signers)"
                .to_owned(),
        )
    })?;
    let sessions = if sessions.is_empty() {
        existing_list("reviewer_stamp_sessions").filter(|list| !list.is_empty())
    } else {
        Some(sessions.to_vec())
    }
    .ok_or_else(|| {
        OnboardError::Refused(
            "no stamp sessions: pass --stamp-session (the base sets no reviewer_stamp_sessions)"
                .to_owned(),
        )
    })?;

    let auto_merge = table_mut(&mut document, "auto_merge")?;
    set_key(
        auto_merge,
        "reviewer_signers",
        toml_edit::value(string_array(&signers)),
    );
    set_key(
        auto_merge,
        "reviewer_stamp_sessions",
        toml_edit::value(string_array(&sessions)),
    );
    set_key(auto_merge, "fleet_review_required", toml_edit::value(true));
    let pr_gates_set = before
        .get("validation")
        .and_then(|validation| validation.get("pr_gates"))
        .is_some();
    if !pr_gates_set && !gate_scripts_present {
        let validation = table_mut(&mut document, "validation")?;
        set_key(validation, "pr_gates", toml_edit::value(false));
    }
    let text = document.to_string();

    let after: toml::Table = toml::from_str(&text).map_err(|error| {
        OnboardError::Refused(format!("rendered config does not parse: {error}"))
    })?;
    let changed_keys = changed_named_keys(&before, &after);
    require_only_named_keys_changed(&before, &after)?;
    Ok(RenderedConfig { text, changed_keys })
}

fn table_mut<'a>(
    document: &'a mut toml_edit::DocumentMut,
    name: &str,
) -> Result<&'a mut toml_edit::Table, OnboardError> {
    document
        .entry(name)
        .or_insert_with(toml_edit::table)
        .as_table_mut()
        .ok_or_else(|| {
            OnboardError::Refused(format!(
                "the base's .shipyard/config.toml [{name}] is not a table"
            ))
        })
}

/// Set `key`, writing `key = value` even when the key existed as a table
/// header (whose decor has no space before `=`).
fn set_key(table: &mut toml_edit::Table, key: &str, item: toml_edit::Item) {
    table[key] = item;
    if let Some(mut key) = table.key_mut(key) {
        key.leaf_decor_mut().set_suffix(" ");
    }
}

/// Where `shipyard pr` looks for the skill-sync and version-bump gate
/// scripts: the base config's `validation.*_script` paths when set, otherwise
/// the default script directories.
#[must_use]
pub fn gate_script_paths(existing: Option<&str>) -> Vec<String> {
    let table: toml::Table = existing
        .and_then(|text| toml::from_str(text).ok())
        .unwrap_or_default();
    let configured = |key: &str| {
        table
            .get("validation")
            .and_then(|validation| validation.get(key))
            .and_then(toml::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let mut paths = Vec::new();
    for (key, filename) in [
        ("skill_sync_script", "skill_sync_check.py"),
        ("version_bump_script", "version_bump_check.py"),
    ] {
        match configured(key) {
            Some(path) => paths.push(path.trim_start_matches("./").to_owned()),
            None => paths.extend(
                ["tools/scripts", "scripts"]
                    .iter()
                    .map(|dir| format!("{dir}/{filename}")),
            ),
        }
    }
    paths
}

fn string_array(values: &[String]) -> toml_edit::Array {
    values.iter().map(String::as_str).collect()
}

fn named_keys() -> Vec<(&'static str, &'static str)> {
    AUTO_MERGE_KEYS
        .iter()
        .map(|key| ("auto_merge", *key))
        .chain(std::iter::once(("validation", "pr_gates")))
        .collect()
}

fn changed_named_keys(before: &toml::Table, after: &toml::Table) -> Vec<String> {
    let lookup = |table: &toml::Table, section: &str, key: &str| {
        table.get(section).and_then(|value| value.get(key)).cloned()
    };
    named_keys()
        .into_iter()
        .filter(|(section, key)| lookup(before, section, key) != lookup(after, section, key))
        .map(|(section, key)| format!("{section}.{key}"))
        .collect()
}

/// Refuse a config edit that changed anything but the named keys.
pub fn require_only_named_keys_changed(
    before: &toml::Table,
    after: &toml::Table,
) -> Result<(), OnboardError> {
    let strip = |table: &toml::Table| {
        let mut table = table.clone();
        for (section, key) in named_keys() {
            if let Some(toml::Value::Table(inner)) = table.get_mut(section) {
                inner.remove(key);
                if inner.is_empty() {
                    table.remove(section);
                }
            }
        }
        table
    };
    if strip(before) == strip(after) {
        Ok(())
    } else {
        Err(OnboardError::Refused(
            "rendered .shipyard/config.toml changes keys outside the onboarding allowlist"
                .to_owned(),
        ))
    }
}

/// SHA-256 of each rendered file, keyed by path.
#[must_use]
pub fn rendered_hashes(review: &str, trigger: &str, config: &str) -> BTreeMap<String, String> {
    use sha2::{Digest, Sha256};
    let digest = |bytes: &[u8]| -> String { format!("{:x}", Sha256::digest(bytes)) };
    BTreeMap::from([
        (ALLOWED_PATHS[0].to_owned(), digest(review.as_bytes())),
        (ALLOWED_PATHS[1].to_owned(), digest(trigger.as_bytes())),
        (ALLOWED_PATHS[2].to_owned(), digest(config.as_bytes())),
    ])
}

fn read_json(reader: &dyn GithubReader, path: &str) -> Result<Value, OnboardError> {
    reader
        .get_json(path)
        .map_err(OnboardError::Unreadable)?
        .ok_or_else(|| OnboardError::Unreadable(format!("GitHub returned 404 for {path}")))
}

/// Every element of a paginated list endpoint. `path` must already carry a
/// query string. A page that is not an array, a 404 or more than
/// `MAX_PAGES` pages stops the read.
pub fn read_all_pages(reader: &dyn GithubReader, path: &str) -> Result<Vec<Value>, OnboardError> {
    let mut items = Vec::new();
    for page in 1..=MAX_PAGES {
        let page_path = format!("{path}&per_page={PAGE_SIZE}&page={page}");
        let Value::Array(rows) = read_json(reader, &page_path)? else {
            return Err(OnboardError::Unreadable(format!(
                "{page_path}: not a JSON array"
            )));
        };
        let count = rows.len();
        items.extend(rows);
        if count < PAGE_SIZE {
            return Ok(items);
        }
    }
    Err(OnboardError::Unreadable(format!(
        "{path}: more than {MAX_PAGES} pages"
    )))
}

/// The first-evaluation sweep: every open, non-draft PR, and whether its head
/// already carries a `review-verdict` status or Actions check run.
///
/// The status is read because a comment-triggered or review-triggered run posts
/// the status on the head while its own check run lands on the default branch.
pub fn sweep_rows(
    reader: &dyn GithubReader,
    repo: &str,
    pulls: &[Value],
) -> Result<Vec<SweepRow>, OnboardError> {
    let mut rows = Vec::new();
    for pull in pulls {
        if pull.get("draft").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let number = pull
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| OnboardError::Unreadable("an open pull has no number".to_owned()))?;
        let head = pull
            .get("head")
            .and_then(|head| head.get("sha"))
            .and_then(Value::as_str)
            .ok_or_else(|| OnboardError::Unreadable(format!("PR #{number} has no head sha")))?
            .to_owned();

        let status_path = format!("repos/{repo}/commits/{head}/status?per_page={PAGE_SIZE}");
        let combined = read_json(reader, &status_path)?;
        let statuses = combined
            .get("statuses")
            .and_then(Value::as_array)
            .ok_or_else(|| OnboardError::Unreadable(format!("{status_path}: no statuses list")))?;
        let total = combined
            .get("total_count")
            .and_then(Value::as_u64)
            .unwrap_or(statuses.len() as u64);
        if total > statuses.len() as u64 {
            return Err(OnboardError::Unreadable(format!(
                "{status_path}: {total} contexts exceed one page"
            )));
        }
        let status_state = statuses
            .iter()
            .find(|status| {
                status.get("context").and_then(Value::as_str) == Some(REVIEW_VERDICT_CONTEXT)
            })
            .and_then(|status| status.get("state").and_then(Value::as_str))
            .map(str::to_owned);

        let runs_path = format!(
            "repos/{repo}/commits/{head}/check-runs?check_name={REVIEW_VERDICT_CONTEXT}&app_id={ACTIONS_INTEGRATION_ID}&filter=latest"
        );
        let listing = read_json(reader, &runs_path)?;
        let runs = listing
            .get("check_runs")
            .and_then(Value::as_array)
            .ok_or_else(|| OnboardError::Unreadable(format!("{runs_path}: no check_runs list")))?;
        let run = runs.iter().find(|run| {
            run.get("app")
                .and_then(|app| app.get("id"))
                .and_then(Value::as_u64)
                == Some(ACTIONS_INTEGRATION_ID)
        });
        let existing = status_state.is_some() || run.is_some();
        rows.push(SweepRow {
            number,
            head,
            trigger: if existing {
                "skip-existing"
            } else {
                "pull_request_target"
            }
            .to_owned(),
            status_state,
            check_run_id: run.and_then(|run| run.get("id")).and_then(Value::as_u64),
            conclusion: run
                .and_then(|run| run.get("conclusion"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    Ok(rows)
}

/// The `required_status_checks` entries the tightening step adds:
/// `review-verdict` and each `--require-check`, in order, without duplicates,
/// each bound to the GitHub Actions app. A blank name is refused.
pub fn planned_required_checks(require_checks: &[String]) -> Result<Vec<Value>, OnboardError> {
    let mut names: Vec<&str> = vec![REVIEW_VERDICT_CONTEXT];
    for name in require_checks {
        let name = name.trim();
        if name.is_empty() {
            return Err(OnboardError::Refused(
                "--require-check names an empty context".to_owned(),
            ));
        }
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names
        .into_iter()
        .map(|context| json!({"context": context, "integration_id": ACTIONS_INTEGRATION_ID}))
        .collect())
}

/// Build the read-only onboarding plan. Only GET requests are made, and the
/// ownership and argument guards run before the first one.
pub fn build_plan(
    reader: &dyn GithubReader,
    request: &OnboardRequest,
) -> Result<Value, OnboardError> {
    let repo = request.repo.as_str();
    validate_repo(repo).map_err(OnboardError::Refused)?;
    if !matches!(request.profile.as_str(), "fleet" | "builder") {
        return Err(OnboardError::Refused(
            "profile must be fleet or builder".to_owned(),
        ));
    }
    let through = resolve_through(request.through.as_deref(), request.no_enroll)
        .map_err(OnboardError::Refused)?;
    let required_checks = planned_required_checks(&request.require_checks)?;
    if !is_owned_repo(repo) {
        return Err(OnboardError::Refused(format!(
            "refusing non-owned repository {repo}"
        )));
    }

    let repo_json = read_json(reader, &format!("repos/{repo}"))?;
    let default_branch = repo_json
        .get("default_branch")
        .and_then(Value::as_str)
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| OnboardError::Unreadable(format!("repos/{repo}: no default_branch")))?
        .to_owned();
    let pin = resolve_pin(reader, &request.shipyard_version)?;
    let config_path = format!("repos/{repo}/contents/.shipyard/config.toml?ref={default_branch}");
    let existing_config = reader
        .get_raw(&config_path)
        .map_err(OnboardError::Unreadable)?;

    let mut gate_scripts_present = false;
    for path in gate_script_paths(existing_config.as_deref()) {
        let path = format!("repos/{repo}/contents/{path}?ref={default_branch}");
        if reader
            .get_raw(&path)
            .map_err(OnboardError::Unreadable)?
            .is_some()
        {
            gate_scripts_present = true;
            break;
        }
    }

    let review = render_review_verdict(&pin);
    let config = render_config(
        existing_config.as_deref(),
        &request.reviewer_signers,
        &request.stamp_sessions,
        gate_scripts_present,
    )?;
    let hashes = rendered_hashes(&review, REVIEW_VERDICT_TRIGGER, &config.text);
    validate_path_allowlist(&hashes.keys().cloned().collect::<Vec<_>>())
        .map_err(OnboardError::Refused)?;

    let rulesets = read_all_pages(
        reader,
        &format!("repos/{repo}/rulesets?includes_parents=true"),
    )?;
    let pulls = read_all_pages(reader, &format!("repos/{repo}/pulls?state=open"))?;
    let sweep = sweep_rows(reader, repo, &pulls)?;

    let steps = plan_steps(
        through,
        &hashes,
        &config.changed_keys,
        &rulesets,
        &required_checks,
        &sweep,
    );

    Ok(json!({
        "command": "governance onboard",
        "mode": "plan-only",
        "apply": "not implemented; this command performs no GitHub writes",
        "repo": repo,
        "profile": request.profile,
        "require_checks": request.require_checks,
        "required_status_checks_to_add": required_checks,
        "default_branch": default_branch,
        "through": through,
        "shipyard_pin": pin,
        "rendered": {
            ALLOWED_PATHS[0]: review,
            ALLOWED_PATHS[1]: REVIEW_VERDICT_TRIGGER,
            ALLOWED_PATHS[2]: config.text,
        },
        "steps": steps,
    }))
}

/// The plan's step table, each marked `included` up to `through`.
fn plan_steps(
    through: &str,
    hashes: &BTreeMap<String, String>,
    changed_keys: &[String],
    rulesets: &[Value],
    required_checks: &[Value],
    sweep: &[SweepRow],
) -> Vec<Value> {
    let last = STEPS
        .iter()
        .position(|step| *step == through)
        .unwrap_or(STEPS.len() - 1);
    let waits = BoundedWaits::default();
    let details = |name: &str| -> Value {
        match name {
            "preflight" => json!({"action": "read-only audit"}),
            "pr" => json!({
                "action": "render and open an unarmed onboarding PR",
                "paths": ALLOWED_PATHS,
                "hashes": hashes,
                "config_changed_keys": changed_keys,
            }),
            "approved" => json!({
                "action": "bounded exact-head approval wait",
                "timeout_minutes": waits.approval_minutes,
            }),
            "green" => json!({"action": "verify current required checks"}),
            "merged" => json!({"action": "mergePullRequest(expectedHeadOid)"}),
            "tightened" => json!({
                "action": "back up and add to the ruleset",
                "required_status_checks_to_add": required_checks,
                "rulesets_observed": rulesets,
            }),
            "evaluated" => json!({
                "action": "first review-verdict evaluation sweep",
                "wait_minutes": waits.evaluation_minutes,
                "prs": sweep,
            }),
            "verified" => json!({"action": "bounded wait for a review-verdict on each swept head"}),
            "handed_over" => json!({"action": "report UNSTABLE heads"}),
            "audited" => json!({"action": "re-audit"}),
            _ => json!({"action": "enroll in fleet-repos.txt"}),
        }
    };
    STEPS
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let mut step = details(name);
            step["step"] = json!(index);
            step["name"] = json!(name);
            step["included"] = json!(index <= last);
            step
        })
        .collect()
}

#[cfg(test)]
mod tests;
