//! The rebase carry: the Rust twin of `rebase_carry` in
//! `scripts/ghapp_queue_arm_guard.py`.
//!
//! An approval of an earlier head carries across a rebase to the new head
//! only when all four hold, each checked on its own and every failing one
//! named:
//!
//! 1. the pull request's diff has the same `git patch-id --stable` at both
//!    heads, each against its own merge base;
//! 2. it changes the same files, with the same status and rename sources;
//! 3. re-applying the approved diff onto the new merge base gives exactly the
//!    new head's tree: the new merge base descends from the old one, the base
//!    touched (changed, renamed or deleted) none of the pull request's files
//!    between them, and every file keeps its approved blob and mode, with no
//!    binary change;
//! 4. no required check on the new head concluded `skipped` or `neutral` or
//!    reused a receipt: the carry covers the review, never the checks.
//!
//! A host turns it on with `SHIPYARD_REBASE_CARRY=1` or a `rebase-carry` file
//! in the guards directory (the same switch the Python guard reads); an
//! [`super::ApprovalPolicy`] with `rebase_carry` set turns it on for one
//! evaluation (the server-side `shipyard review-verdict`). Anything
//! unreadable refuses.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use super::{RunGh, blob_is_binary, compare_files, gh_json, short, tree_modes};

/// Environment switch for the rebase carry: `1` on, `0` off.
pub const REBASE_CARRY_ENV: &str = "SHIPYARD_REBASE_CARRY";
/// File in the guards directory that turns the rebase carry on.
pub const REBASE_CARRY_FILE: &str = "rebase-carry";
/// Carry log in the guards directory, one JSON line per carry.
pub const REBASE_CARRY_LOG: &str = "carries.jsonl";
/// A required check that concluded one of these did not run in full.
const NOT_RUN_IN_FULL: [&str; 2] = ["skipped", "neutral"];
/// The annotation a job prints when it decides whether a receipt is reused.
const RECEIPT_DECISION_TITLE: &str = "shipyard-receipt-decision";

/// Whether this host carries approvals across a rebase: off unless
/// [`REBASE_CARRY_ENV`] is `1` or the guards directory holds
/// [`REBASE_CARRY_FILE`].
#[must_use]
pub fn rebase_carry_enabled() -> bool {
    match std::env::var(REBASE_CARRY_ENV).as_deref() {
        Ok("1") => return true,
        Ok("0") => return false,
        _ => {}
    }
    super::guards_dir().is_some_and(|dir| dir.join(REBASE_CARRY_FILE).is_file())
}

/// What a carry across a rebase checked, for the carry log.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CarryRecord {
    /// The approved head's merge base.
    pub old_base: String,
    /// The new head's merge base.
    pub new_base: String,
    /// The diff's `git patch-id --stable`.
    pub patch_id: String,
    /// Files the pull request changes.
    pub files: usize,
}

type Files = BTreeMap<String, (String, String, bool)>;

/// Whether the approval of `from` carries across a rebase to `to`.
///
/// # Errors
///
/// Every condition that fails, joined by `; `, or why the carry cannot be
/// checked.
pub fn rebase_carry(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    from: &str,
    to: &str,
) -> Result<CarryRecord, String> {
    let unreadable = |error: String| format!("the rebase carry cannot be checked ({error})");
    let base = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/pulls/{pr}"),
        "--jq".to_owned(),
        ".base.ref".to_owned(),
    ])
    .map_err(unreadable)?
    .trim()
    .to_owned();
    if base.is_empty() {
        return Err("the pull request has no base branch".to_owned());
    }
    let (before, old_base, before_sources) =
        compare_files(run_gh, repo, &format!("{base}...{from}")).map_err(unreadable)?;
    let (after, new_base, after_sources) =
        compare_files(run_gh, repo, &format!("{base}...{to}")).map_err(unreadable)?;
    let status = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/compare/{old_base}...{new_base}"),
        "--jq".to_owned(),
        ".status".to_owned(),
    ])
    .map_err(unreadable)?;
    let status = status.trim();
    if status != "ahead" && status != "identical" {
        return Err(format!(
            "the new merge base {} does not descend from {}",
            short(&new_base),
            short(&old_base)
        ));
    }
    let base_touched = if status == "ahead" {
        let (moved, _, moved_sources) =
            compare_files(run_gh, repo, &format!("{old_base}...{new_base}")).map_err(unreadable)?;
        moved
            .into_keys()
            .chain(moved_sources.into_values())
            .collect()
    } else {
        BTreeSet::new()
    };
    let diff = |range: String| {
        run_gh(&[
            "api".to_owned(),
            "-H".to_owned(),
            "Accept: application/vnd.github.diff".to_owned(),
            format!("repos/{repo}/compare/{range}"),
        ])
    };
    let old_diff = diff(format!("{old_base}...{from}")).map_err(unreadable)?;
    let new_diff = diff(format!("{new_base}...{to}")).map_err(unreadable)?;
    let old_id = patch_id(&old_diff).map_err(unreadable)?;
    let new_id = patch_id(&new_diff).map_err(unreadable)?;
    let mut reasons = Vec::new();
    if old_id.is_none() || old_id != new_id {
        reasons.push(format!(
            "the diff's patch-id changed ({} -> {})",
            short(old_id.as_deref().unwrap_or("none")),
            short(new_id.as_deref().unwrap_or("none"))
        ));
    }
    if let Some(reason) = same_files(&before, &after, to, &before_sources, &after_sources) {
        reasons.push(reason);
    }
    let rename_sources: BTreeSet<String> = before_sources
        .values()
        .chain(after_sources.values())
        .cloned()
        .collect();
    let reapplies = reapplies_exactly(
        run_gh,
        repo,
        from,
        to,
        (&before, &after),
        &base_touched,
        &rename_sources,
    )
    .map_err(unreadable)?;
    reasons.extend(reapplies);
    if let Some(reason) = required_not_run_in_full(run_gh, repo, &base, to).map_err(unreadable)? {
        reasons.push(reason);
    }
    if !reasons.is_empty() {
        return Err(reasons.join("; "));
    }
    Ok(CarryRecord {
        old_base,
        new_base,
        patch_id: old_id.unwrap_or_default(),
        files: before.len(),
    })
}

/// Condition 2: the same files, with the same status and rename sources.
fn same_files(
    before: &Files,
    after: &Files,
    to: &str,
    before_sources: &BTreeMap<String, String>,
    after_sources: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(name) = before
        .keys()
        .find(|name| !after.contains_key(*name))
        .or_else(|| after.keys().find(|name| !before.contains_key(*name)))
    {
        return Some(format!(
            "the set of files the pull request changes moved ({name})"
        ));
    }
    if let Some((name, (status, _, _))) = before
        .iter()
        .find(|(name, (status, _, _))| after.get(*name).is_some_and(|other| &other.0 != status))
    {
        return Some(format!(
            "{name} is {} at {}, {status} when approved",
            after[name].0,
            short(to)
        ));
    }
    super::rename_source_moved(before_sources, after_sources).map(|name| {
        format!(
            "{name} is renamed from {} at {}, from {} when approved",
            after_sources.get(&name).map_or("nothing", String::as_str),
            short(to),
            before_sources.get(&name).map_or("nothing", String::as_str),
        )
    })
}

/// Condition 3, proved rather than computed: no binary change, the base
/// touched none of the pull request's files (a rename's source included),
/// and each file keeps its approved blob and mode.
fn reapplies_exactly(
    run_gh: RunGh<'_>,
    repo: &str,
    from: &str,
    to: &str,
    (before, after): (&Files, &Files),
    base_touched: &BTreeSet<String>,
    rename_sources: &BTreeSet<String>,
) -> Result<Option<String>, String> {
    for (name, (_, sha, patch)) in before {
        if !patch && blob_is_binary(run_gh, repo, sha)? {
            return Ok(Some(format!(
                "the pull request changes a binary file ({name})"
            )));
        }
    }
    if let Some(name) = before
        .keys()
        .chain(after.keys())
        .chain(rename_sources.iter())
        .filter(|name| base_touched.contains(*name))
        .min()
    {
        return Ok(Some(format!(
            "{name} was changed, renamed or deleted by the base between the merge bases"
        )));
    }
    if let Some(name) = before.iter().find_map(|(name, (status, sha, _))| {
        (status != "removed" && after.get(name).is_some_and(|other| &other.1 != sha))
            .then_some(name)
    }) {
        return Ok(Some(format!(
            "{name} at {} is not the blob approved at {}",
            short(to),
            short(from)
        )));
    }
    let present: Vec<&str> = before
        .iter()
        .filter(|(name, (status, _, _))| status != "removed" && after.contains_key(*name))
        .map(|(name, _)| name.as_str())
        .collect();
    if !present.is_empty() {
        let approved = tree_modes(run_gh, repo, from, &present)?;
        let head = tree_modes(run_gh, repo, to, &present)?;
        if let Some(name) = present
            .iter()
            .find(|name| approved.get(**name) != head.get(**name))
        {
            return Ok(Some(format!(
                "{name} at {} does not keep the mode approved at {}",
                short(to),
                short(from)
            )));
        }
    }
    Ok(None)
}

/// Condition 4: why a required check on `head` did not run in full, or
/// `None`.
fn required_not_run_in_full(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    head: &str,
) -> Result<Option<String>, String> {
    let required = required_contexts(run_gh, repo, base)?;
    if required.is_empty() {
        return Ok(None);
    }
    let runs = gh_json(
        run_gh,
        &[
            "api".to_owned(),
            format!("repos/{repo}/commits/{head}/check-runs?per_page=100"),
        ],
    )?;
    let listed = runs
        .get("check_runs")
        .and_then(Value::as_array)
        .filter(|listed| {
            runs.get("total_count").and_then(Value::as_u64) == Some(listed.len() as u64)
        })
        .ok_or_else(|| {
            format!(
                "the check runs of {} are unreadable or partial",
                short(head)
            )
        })?;
    for context in &required {
        let Some(latest) = listed
            .iter()
            .filter(|run| run.get("name").and_then(Value::as_str) == Some(context.as_str()))
            .max_by_key(|run| run.get("id").and_then(Value::as_u64).unwrap_or(0))
        else {
            continue;
        };
        let conclusion = latest.get("conclusion").and_then(Value::as_str);
        if latest.get("status").and_then(Value::as_str) == Some("completed")
            && conclusion.is_some_and(|conclusion| NOT_RUN_IN_FULL.contains(&conclusion))
        {
            return Ok(Some(format!(
                "required check `{context}` on {} concluded `{}`",
                short(head),
                conclusion.unwrap_or_default()
            )));
        }
        if let Some(reason) = reused_receipt(run_gh, repo, latest)? {
            return Ok(Some(format!(
                "required check `{context}` on {} reused a receipt ({reason})",
                short(head)
            )));
        }
    }
    Ok(None)
}

/// The base branch's required contexts, from its rulesets and its classic
/// protection (an unprotected branch has none).
fn required_contexts(run_gh: RunGh<'_>, repo: &str, base: &str) -> Result<Vec<String>, String> {
    let mut contexts = BTreeSet::new();
    let rules = gh_json(
        run_gh,
        &[
            "api".to_owned(),
            format!("repos/{repo}/rules/branches/{base}"),
        ],
    )?;
    for rule in rules.as_array().into_iter().flatten() {
        if rule.get("type").and_then(Value::as_str) != Some("required_status_checks") {
            continue;
        }
        for check in rule
            .pointer("/parameters/required_status_checks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(context) = check.get("context").and_then(Value::as_str) {
                contexts.insert(context.to_owned());
            }
        }
    }
    match gh_json(
        run_gh,
        &[
            "api".to_owned(),
            format!("repos/{repo}/branches/{base}/protection/required_status_checks"),
        ],
    ) {
        Ok(classic) => {
            for context in classic
                .get("contexts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                contexts.insert(context.to_owned());
            }
        }
        Err(error) if error.contains("404") => {}
        Err(error) => return Err(error),
    }
    Ok(contexts.into_iter().collect())
}

/// The reason of a `shipyard-receipt-decision` annotation with verdict
/// `reuse` on `run`, else `None`.
fn reused_receipt(run_gh: RunGh<'_>, repo: &str, run: &Value) -> Result<Option<String>, String> {
    let (Some(id), Some(count)) = (
        run.get("id").and_then(Value::as_u64),
        run.pointer("/output/annotations_count")
            .and_then(Value::as_u64),
    ) else {
        return Ok(None);
    };
    if count == 0 {
        return Ok(None);
    }
    let annotations = gh_json(
        run_gh,
        &[
            "api".to_owned(),
            format!("repos/{repo}/check-runs/{id}/annotations?per_page=100"),
        ],
    )?;
    let annotations = annotations
        .as_array()
        .ok_or_else(|| format!("the annotations of check run {id} are unreadable"))?;
    for annotation in annotations {
        if annotation.get("title").and_then(Value::as_str) != Some(RECEIPT_DECISION_TITLE) {
            continue;
        }
        let message = annotation
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let decision: Value = serde_json::from_str(message)
            .map_err(|_| format!("check run {id} has an unparseable receipt decision"))?;
        if decision.get("verdict").and_then(Value::as_str) == Some("reuse") {
            return Ok(Some(
                decision
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given")
                    .to_owned(),
            ));
        }
    }
    Ok(None)
}

/// `git patch-id --stable` of a diff (no repository needed); `None` when the
/// diff is empty.
///
/// # Errors
///
/// When git cannot run or reads the diff as more than one patch.
pub fn patch_id(diff: &str) -> Result<Option<String>, String> {
    let output = run_patch_id(diff.as_bytes())?;
    if !output.status.success() {
        return Err(format!(
            "git patch-id failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let ids: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .collect();
    if ids.len() > 1 {
        return Err("the diff read as more than one patch".to_owned());
    }
    Ok(ids.into_iter().next())
}

#[cfg(unix)]
fn run_patch_id(input: &[u8]) -> Result<std::process::Output, String> {
    let mut command = std::process::Command::new("git");
    command.args(["patch-id", "--stable"]);
    crate::process::run_output_with_input_until(
        &mut command,
        input,
        std::time::Instant::now() + std::time::Duration::from_mins(1),
        "git patch-id",
    )
    .map_err(|error| format!("cannot run git patch-id: {error}"))
}

#[cfg(not(unix))]
fn run_patch_id(input: &[u8]) -> Result<std::process::Output, String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run git patch-id: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "cannot run git patch-id: no stdin".to_owned())?
        .write_all(input)
        .map_err(|error| format!("cannot run git patch-id: {error}"))?;
    child
        .wait_with_output()
        .map_err(|error| format!("cannot run git patch-id: {error}"))
}

/// Append one carry to the guards directory's carry log. Never fails: the
/// log records carries, it does not gate them.
pub fn log_carry(repo: &str, pr: u64, from: &str, to: &str, record: &CarryRecord) {
    let Some(dir) = super::guards_dir() else {
        return;
    };
    let entry = serde_json::json!({
        "at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "kind": "rebase",
        "by": "shipyard",
        "repo": repo,
        "pr": pr,
        "approved": from,
        "head": to,
        "old_base": record.old_base,
        "new_base": record.new_base,
        "patch_id": record.patch_id,
        "files": record.files,
    });
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(REBASE_CARRY_LOG))
    {
        use std::io::Write as _;
        let _ = writeln!(file, "{entry}");
    }
}
