//! A required check whose poster the ruleset cannot pin, verified by its run.
//!
//! ## Why
//!
//! A repository ruleset can require a status context or a check run name, and
//! bind it to an App, but for GitHub Actions the App is the one every workflow
//! shares (`github-actions`, App 15368). Any workflow with
//! `statuses: write`, or any job of the same name, satisfies such a rule,
//! including one a same-repository pull request adds under `pull_request`.
//! Vellum's `framework-trusted-verify` is such a signal: it is only worth
//! something when it comes from `.github/workflows/framework-trusted-verify.yml`
//! running on `pull_request_target` (the base's definition) or `merge_group`.
//!
//! So before the steward admits a head (every caller of the approval gate:
//! enqueue, exact-head merge, native arm, the carrier and the
//! unapproved-admission sweep), each required trusted signal must resolve to
//! that workflow's own run on that exact head.
//!
//! ## What counts
//!
//! The signal is a commit status: Vellum's workflow posts the context `name`
//! to the head it ran for (pending at start, then success or failure) from a
//! job of a different name, and lints that no job carries `name`. So a check
//! run named `name`, from any App, is a spoof and refuses the head.
//!
//! Every commit status whose context is `name` must name a workflow run of
//! the same repository by its exact URL,
//! `https://github.com/<owner>/<repo>/actions/runs/<id>` and nothing else,
//! and that run must have:
//!
//! * `path` equal to the signal's workflow file;
//! * `event` `pull_request_target` or `merge_group`;
//! * `head_sha` equal to the head being admitted;
//! * `repository.full_name` equal to the repository.
//!
//! Every status of that name must pass this, not only the newest: a
//! same-named status from any other workflow or event is a spoof attempt, and
//! the head is refused even beside a genuine one. Then the newest status (by
//! id) must be `success`, so a pending one never admits, and its run must
//! have concluded `success`. No status of that name refuses.
//!
//! ## When it is enforced
//!
//! `pull_request_target` runs the workflow as the base defines it, so until
//! the base has the workflow file no run can post the signal, and enforcing
//! it would hold every pull request, including the one that adds the file.
//! A signal is therefore enforced for a pull request exactly when the
//! workflow file is a blob at the pull request's `base.sha`, read through the
//! git trees API. A pull request that deletes or edits the file is still
//! enforced, because its base has it. A failed read is an error.
//!
//! Which sha a `pull_request_target` run reports was measured on
//! Generous-Corp/vellum's own `pull_request_target` workflow
//! (`review-verdict.yml`): the run's `head_sha` is the pull request's head
//! commit it was started for, and `head_branch` the pull request's branch, not
//! the base. Run 42581013787 reports `11120fb7d8b2…`, the head of #272 when it
//! ran; the earlier runs 42580805263 and 42578269821 keep the earlier heads
//! `09b407e14b07…` and `0b5a7afaac13…`, while their `pull_requests[]` shows
//! only the pull request's current head. So the run's own `head_sha` is the
//! binding, and a run for an older push never admits a newer head. A
//! `merge_group` run's `head_sha` is the merge-group commit, which is never a
//! pull request's head, so on a pull request it fails the binding.
//!
//! Anything missing, unparsable, foreign or mismatched refuses
//! ([`HeadGate::UntrustedSignal`], named in the verdict). A read that fails is
//! an error, never an admission: the steward does not admit on an error, and
//! the dequeue sweep only defers on one.
//!
//! ## Scope
//!
//! [`TRUSTED_SIGNAL_REPOS`] requires Vellum's signal whatever its base config
//! says. Any repository can add signals with
//! `[auto_merge] trusted_signals = [{ name = "...", workflow = "..." }]` in its
//! base config; a config can add a requirement, never remove one. Every other
//! repository is unaffected.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{HeadGate, RunGh};

/// `[auto_merge]` key listing extra trusted signals.
pub const TRUSTED_SIGNALS_KEY: &str = "trusted_signals";

/// Signals required for a repository whatever its base config says:
/// `(repository, signal name, workflow path)`.
pub const TRUSTED_SIGNAL_REPOS: &[(&str, &str, &str)] = &[(
    "Generous-Corp/vellum",
    "framework-trusted-verify",
    ".github/workflows/framework-trusted-verify.yml",
)];

/// Events whose runs may carry a trusted signal.
const TRUSTED_EVENTS: &[&str] = &["pull_request_target", "merge_group"];

/// A status context that only one workflow may satisfy.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TrustedSignal {
    /// The status context.
    pub name: String,
    /// The only workflow file whose run may post it.
    pub workflow: String,
}

/// The signals a base's `[auto_merge]` section lists. An entry that is not a
/// table of two non-empty strings becomes a signal no run can satisfy, so a
/// typo refuses every head rather than dropping the requirement.
#[must_use]
pub fn signals_from_section(section: Option<&toml::Table>) -> Vec<TrustedSignal> {
    let Some(value) = section.and_then(|section| section.get(TRUSTED_SIGNALS_KEY)) else {
        return Vec::new();
    };
    let Some(entries) = value.as_array() else {
        return vec![unusable(&format!(
            "`{TRUSTED_SIGNALS_KEY}` is not an array"
        ))];
    };
    entries
        .iter()
        .map(|entry| {
            let field = |key: &str| {
                entry
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            };
            match (field("name"), field("workflow")) {
                (Some(name), Some(workflow)) => TrustedSignal {
                    name: name.to_owned(),
                    workflow: workflow.to_owned(),
                },
                _ => unusable(&format!(
                    "a `{TRUSTED_SIGNALS_KEY}` entry needs non-empty `name` and `workflow`"
                )),
            }
        })
        .collect()
}

/// A requirement nothing satisfies, named after what made it unusable.
fn unusable(detail: &str) -> TrustedSignal {
    TrustedSignal {
        name: format!("unusable trusted_signals config ({detail})"),
        workflow: String::new(),
    }
}

/// Every signal `repo` requires: [`TRUSTED_SIGNAL_REPOS`] plus `configured`.
#[must_use]
pub fn required(repo: &str, configured: &[TrustedSignal]) -> Vec<TrustedSignal> {
    let mut signals: Vec<TrustedSignal> = TRUSTED_SIGNAL_REPOS
        .iter()
        .filter(|(required_repo, _, _)| required_repo.eq_ignore_ascii_case(repo))
        .map(|(_, name, workflow)| TrustedSignal {
            name: (*name).to_owned(),
            workflow: (*workflow).to_owned(),
        })
        .chain(configured.iter().cloned())
        .collect();
    signals.sort();
    signals.dedup();
    signals
}

/// `gate` unless it admits `head` and a required signal does not verify, in
/// which case [`HeadGate::UntrustedSignal`] naming why.
///
/// # Errors
///
/// When the pull request, its base tree, a status, a check run or a workflow
/// run cannot be read. Callers must not
/// admit on an error.
pub(super) fn apply(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    head: &str,
    configured: &[TrustedSignal],
    gate: HeadGate,
) -> Result<HeadGate, String> {
    if !gate.allows() {
        return Ok(gate);
    }
    let signals = required(repo, configured);
    if signals.is_empty() {
        return Ok(gate);
    }
    let base = base_sha(run_gh, repo, pr)?;
    for signal in signals {
        if !signal.workflow.is_empty() && !blob_at(run_gh, repo, &base, &signal.workflow)? {
            continue;
        }
        if let Some(detail) = judge(run_gh, repo, head, &signal)? {
            return Ok(HeadGate::UntrustedSignal {
                head: head.to_owned(),
                detail,
            });
        }
    }
    Ok(gate)
}

/// Why `signal` does not admit `head`, or `None` when it does.
///
/// # Errors
///
/// When a read fails.
pub fn judge(
    run_gh: RunGh<'_>,
    repo: &str,
    head: &str,
    signal: &TrustedSignal,
) -> Result<Option<String>, String> {
    let name = &signal.name;
    if signal.workflow.is_empty() {
        return Ok(Some(format!("`{name}` cannot be verified")));
    }
    let statuses: Vec<Value> = read_lines(
        run_gh,
        &format!("repos/{repo}/commits/{head}/statuses?per_page=100"),
        ".[] | {id, state, context, target_url}",
    )?
    .into_iter()
    .filter(|status| status.get("context").and_then(Value::as_str) == Some(name.as_str()))
    .collect();
    let spoofs: Vec<Value> = read_lines(
        run_gh,
        &format!(
            "repos/{repo}/commits/{head}/check-runs?check_name={}&filter=all&per_page=100",
            crate::required_check_policy::encode_path_segment(name)
        ),
        ".check_runs[] | {id, name, app_id: .app.id}",
    )?
    .into_iter()
    .filter(|run| run.get("name").and_then(Value::as_str) == Some(name.as_str()))
    .collect();
    if let Some(spoof) = spoofs.first() {
        return Ok(Some(format!(
            "check run {} named `{name}` (App {}) is on head {}; the signal is a \
             commit status, so a same-named check run is a spoof",
            spoof.get("id").and_then(Value::as_u64).unwrap_or(0),
            spoof.get("app_id").and_then(Value::as_u64).unwrap_or(0),
            short(head)
        )));
    }
    if statuses.is_empty() {
        return Ok(Some(format!("no `{name}` status on head {}", short(head))));
    }
    let mut runs: BTreeMap<u64, Value> = BTreeMap::new();
    for status in &statuses {
        let id = status.get("id").and_then(Value::as_u64).unwrap_or(0);
        let url = status
            .get("target_url")
            .and_then(Value::as_str)
            .unwrap_or("");
        let run_id = match run_id_from_url(url, repo) {
            Ok(run_id) => run_id,
            Err(why) => return Ok(Some(format!("`{name}` status {id}: {why}"))),
        };
        if let std::collections::btree_map::Entry::Vacant(entry) = runs.entry(run_id) {
            entry.insert(read_run(run_gh, repo, run_id)?);
        }
        if let Some(why) = untrusted_run(&runs[&run_id], repo, head, signal) {
            return Ok(Some(format!(
                "`{name}` status {id} names run {run_id}, which {why}"
            )));
        }
    }
    Ok(newest_unsuccessful(name, repo, &statuses, &runs))
}

/// Why the newest `name` status has not succeeded, or `None` when it and its
/// run have (every status already judged trusted, its run fetched into `runs`).
fn newest_unsuccessful(
    name: &str,
    repo: &str,
    statuses: &[Value],
    runs: &BTreeMap<u64, Value>,
) -> Option<String> {
    let status = newest(statuses)?;
    let state = status
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("none");
    if state != "success" {
        return Some(format!("the newest `{name}` status is `{state}`"));
    }
    let url = status
        .get("target_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    match run_id_from_url(url, repo).map(|run_id| (run_id, runs.get(&run_id))) {
        Ok((run_id, Some(run))) => unsuccessful(run)
            .map(|why| format!("the newest `{name}` status names run {run_id}, which {why}")),
        _ => Some(format!("the newest `{name}` status names no fetched run")),
    }
}

/// The pull request's `base.sha`.
fn base_sha(run_gh: RunGh<'_>, repo: &str, pr: u64) -> Result<String, String> {
    let sha = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/pulls/{pr}"),
        "--jq".to_owned(),
        ".base.sha".to_owned(),
    ])?;
    let sha = sha.trim();
    if is_sha(sha) {
        Ok(sha.to_owned())
    } else {
        Err(format!("pull request {pr} base sha read back as {sha:?}"))
    }
}

/// Whether `path` is a blob in commit `commit`'s tree, walked one directory
/// at a time through the git trees API so absence is an answer, not a 404.
fn blob_at(run_gh: RunGh<'_>, repo: &str, commit: &str, path: &str) -> Result<bool, String> {
    let parts: Vec<&str> = path.split('/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err(format!(
            "workflow path `{path}` is not a plain relative path"
        ));
    }
    let mut tree = commit.to_owned();
    for (index, part) in parts.iter().enumerate() {
        let raw = run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/git/trees/{tree}"),
            "--jq".to_owned(),
            "{truncated, tree: [.tree[] | {path, type, sha}]}".to_owned(),
        ])?;
        let listing: Value = serde_json::from_str(raw.trim())
            .map_err(|error| format!("malformed tree {tree}: {error}"))?;
        if listing.get("truncated") != Some(&Value::Bool(false)) {
            return Err(format!("tree {tree} is truncated or unreadable"));
        }
        let entries = listing
            .get("tree")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("tree {tree} has no entries"))?;
        let Some(entry) = entries
            .iter()
            .find(|entry| entry.get("path").and_then(Value::as_str) == Some(part))
        else {
            return Ok(false);
        };
        let want = if index + 1 == parts.len() {
            "blob"
        } else {
            "tree"
        };
        if entry.get("type").and_then(Value::as_str) != Some(want) {
            return Ok(false);
        }
        let sha = entry.get("sha").and_then(Value::as_str).unwrap_or("");
        if !is_sha(sha) {
            return Err(format!("tree {tree} lists `{part}` without a sha"));
        }
        tree = sha.to_owned();
    }
    Ok(true)
}

fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The record with the highest id.
fn newest(records: &[Value]) -> Option<&Value> {
    records
        .iter()
        .max_by_key(|record| record.get("id").and_then(Value::as_u64).unwrap_or(0))
}

/// Why `run` may not carry `signal` for `head`, or `None` when it may.
fn untrusted_run(run: &Value, repo: &str, head: &str, signal: &TrustedSignal) -> Option<String> {
    let field = |key: &str| run.get(key).and_then(Value::as_str).unwrap_or("");
    if !field("repository").eq_ignore_ascii_case(repo) {
        return Some(format!("belongs to `{}`", field("repository")));
    }
    if field("path") != signal.workflow {
        return Some(format!("is `{}`, not `{}`", field("path"), signal.workflow));
    }
    if !TRUSTED_EVENTS.contains(&field("event")) {
        return Some(format!(
            "ran on `{}`, not pull_request_target or merge_group",
            field("event")
        ));
    }
    if !field("head_sha").eq_ignore_ascii_case(head) {
        return Some(format!(
            "ran for {}, not {}",
            short(field("head_sha")),
            short(head)
        ));
    }
    None
}

/// Why `run` has not succeeded, or `None` when it has.
fn unsuccessful(run: &Value) -> Option<String> {
    let status = run.get("status").and_then(Value::as_str).unwrap_or("none");
    let conclusion = run
        .get("conclusion")
        .and_then(Value::as_str)
        .unwrap_or("none");
    (status != "completed" || conclusion != "success").then(|| format!("is {status}/{conclusion}"))
}

/// The workflow run id an exact Actions run URL of `repo` names.
///
/// # Errors
///
/// For any other host, repository or shape.
pub fn run_id_from_url(url: &str, repo: &str) -> Result<u64, String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .ok_or_else(|| format!("`{url}` is not a github.com Actions run URL"))?;
    if rest.contains(['?', '#', '\\', ' ']) || rest.contains("//") {
        return Err(format!("`{url}` is not an exact Actions run URL"));
    }
    let parts: Vec<&str> = rest.split('/').collect();
    let number = |text: &str| -> Option<u64> {
        (!text.is_empty()
            && text.bytes().all(|byte| byte.is_ascii_digit())
            && !text.starts_with('0'))
        .then(|| text.parse::<u64>().ok())
        .flatten()
    };
    let (owner_repo, tail) = match parts.as_slice() {
        [owner, name, "actions", "runs", id, tail @ ..] => (format!("{owner}/{name}"), (id, tail)),
        _ => return Err(format!("`{url}` is not an Actions run URL")),
    };
    if !owner_repo.eq_ignore_ascii_case(repo) {
        return Err(format!("`{url}` names `{owner_repo}`, not `{repo}`"));
    }
    let (id, tail) = tail;
    let run_id = number(id).ok_or_else(|| format!("`{url}` has no run id"))?;
    if !tail.is_empty() {
        return Err(format!("`{url}` is not an exact Actions run URL"));
    }
    Ok(run_id)
}

/// The fields of workflow run `run_id` the judgement reads.
fn read_run(run_gh: RunGh<'_>, repo: &str, run_id: u64) -> Result<Value, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/actions/runs/{run_id}"),
        "--jq".to_owned(),
        "{id, path, event, head_sha, status, conclusion, repository: .repository.full_name}"
            .to_owned(),
    ])?;
    serde_json::from_str(raw.trim()).map_err(|error| format!("malformed run {run_id}: {error}"))
}

/// Every JSON line `jq` yields over every page of `path`.
fn read_lines(run_gh: RunGh<'_>, path: &str, jq: &str) -> Result<Vec<Value>, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        path.to_owned(),
        "--jq".to_owned(),
        jq.to_owned(),
    ])?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|error| format!("malformed {path} record: {error}"))
        })
        .collect()
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

#[cfg(test)]
mod tests;
