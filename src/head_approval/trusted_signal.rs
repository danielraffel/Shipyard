//! A required check whose poster the ruleset cannot pin, verified by its run.
//!
//! ## Why
//!
//! A repository ruleset can require a status context and bind it to an App,
//! but for GitHub Actions the App is the one every workflow shares
//! (`github-actions`, App 15368). Any workflow with `statuses: write` satisfies
//! such a rule, including one a same-repository pull request adds under
//! `pull_request`. Vellum's `framework-trusted-verify` is such a signal: it is
//! only worth something when it comes from
//! `.github/workflows/framework-trusted-verify.yml` running on
//! `pull_request_target` (the base's definition of the workflow).
//!
//! So before the steward admits a head (every caller of the approval gate:
//! enqueue, exact-head merge, native arm, the carrier and the
//! unapproved-admission sweep), each required trusted signal must resolve to
//! that workflow's own latest run on that exact head.
//!
//! ## What counts
//!
//! The canonical signal is a commit status. Vellum's workflow posts the
//! context `name` to the head it ran for (pending at start, then success or
//! failure) from a job of a different name, and lints that no job carries
//! `name`. So a check run named `name`, from any App, is a spoof and refuses
//! the head.
//!
//! Every status of that context on the head must name a workflow run of the
//! same repository by its exact URL,
//! `https://github.com/<owner>/<repo>/actions/runs/<id>` and nothing else, and
//! that run must have:
//!
//! * `path` equal to the signal's workflow file;
//! * `event` `pull_request_target`;
//! * `head_sha` equal to the head being admitted;
//! * `head_branch` equal to the pull request's head branch;
//! * `repository.full_name` equal to the repository.
//!
//! A same-named status from any other workflow or event refuses the head even
//! beside a genuine one. Then the workflow's own runs decide, not the statuses:
//! of the workflow's `pull_request_target` runs on the head, the newest (by id)
//! must pass the same checks and have concluded `success`, so a newer run that
//! failed, or is queued or in progress, refuses. The newest status must be
//! `success` (pending never admits) and must name exactly that newest run, so
//! an older success cannot be replayed over a newer run. No status refuses.
//!
//! Which sha a `pull_request_target` run reports was measured on
//! Generous-Corp/vellum's own `pull_request_target` workflow
//! (`review-verdict.yml`): the run's `head_sha` is the pull request's head
//! commit it was started for, and `head_branch` the pull request's branch, not
//! the base. Run 42581013787 reports `11120fb7d8b2…`, the head of #272 when it
//! ran; the earlier runs 42580805263 and 42578269821 keep the earlier heads
//! `09b407e14b07…` and `0b5a7afaac13…`, while their `pull_requests[]` shows
//! only the pull request's current head. So the run's own `head_sha` is the
//! binding, and a run for an older push never admits a newer head.
//!
//! Because `pull_request_target` runs whatever workflow the pull request's
//! base branch defines, a run is only as trusted as that base. So when a
//! signal is enforced, every pull request (open or closed) whose head is this
//! commit, found by this head branch or by the commit, must target `main`. An
//! older pull request from the same branch, headed by another commit, does not
//! count.
//!
//! ## When it is enforced
//!
//! Until the base branch has the workflow file no run can post the signal, and
//! enforcing it would hold every pull request, including the one that adds the
//! file. A signal is enforced exactly when the workflow file is a blob at the
//! pull request's base branch as it is now: `refs/heads/<base.ref>` resolved at
//! admission, then walked through the git trees API (so absence is an answer,
//! not a 404). The pull request's `base.sha` is deliberately not used: it is a
//! snapshot, and a pull request opened before the workflow landed would escape
//! forever. A pull request that deletes or edits the file is still enforced,
//! because its base has it. A failed, truncated or malformed read is an error.
//!
//! Anything missing, unparsable, foreign or mismatched refuses
//! ([`HeadGate::UntrustedSignal`], named in the verdict). A read that fails is
//! an error, never an admission: the steward does not admit on an error, and
//! the dequeue sweep only defers on one.
//!
//! ## Threat model
//!
//! This guards against accidents: an agent's workflow, a misnamed job, a
//! stale or replayed result. It is not a defence against someone with push
//! rights. Known residuals: retargeting or deleting a pull request after its
//! run; a forged status on a `merge_group` commit (merge-group heads are
//! outside this gate, which judges pull request heads before admission); and
//! a token with `statuses: write` posting a status directly; and anything
//! else a pusher can do through a workflow sharing App 15368. Only a dedicated
//! judge App closes those (planned in Generous-Corp/vellum#275).
//!
//! ## Scope
//!
//! [`TRUSTED_SIGNAL_REPOS`] requires Vellum's signal whatever its base config
//! says. Any repository can add signals with
//! `[auto_merge] trusted_signals = [{ name = "...", workflow = "..." }]` in its
//! base config; a config can add a requirement, never remove one. A workflow
//! must be a file directly under `.github/workflows/`. Every other repository
//! is unaffected.

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

/// The event whose runs may carry a trusted signal.
const TRUSTED_EVENT: &str = "pull_request_target";

/// The branch every pull request with a trusted head must target.
const TRUSTED_BASE: &str = "main";

/// Where a workflow file must live for its runs to be listed.
const WORKFLOWS_DIR: &str = ".github/workflows/";

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
/// When the pull request, its base branch, a tree, a status, a check run, a
/// workflow run or a related pull request cannot be read. Callers must not
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
    let pull = read_pull(run_gh, repo, pr)?;
    let tip = branch_tip(run_gh, repo, &pull.base_ref)?;
    let untrusted = |detail| HeadGate::UntrustedSignal {
        head: head.to_owned(),
        detail,
    };
    let mut enforced = false;
    for signal in signals {
        if !signal.workflow.is_empty() && !blob_at(run_gh, repo, &tip, &signal.workflow)? {
            continue;
        }
        enforced = true;
        if let Some(detail) = judge(run_gh, repo, head, &pull.head_ref, &signal)? {
            return Ok(untrusted(detail));
        }
    }
    if !enforced {
        return Ok(gate);
    }
    Ok(foreign_base(run_gh, repo, head, &pull)?.map_or(gate, untrusted))
}

/// The pull request fields enforcement reads.
#[derive(Debug)]
struct Pull {
    base_ref: String,
    head_ref: String,
    head_owner: String,
}

fn read_pull(run_gh: RunGh<'_>, repo: &str, pr: u64) -> Result<Pull, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/pulls/{pr}"),
        "--jq".to_owned(),
        "{base_ref: .base.ref, head_ref: .head.ref, head_owner: .head.repo.owner.login}".to_owned(),
    ])?;
    let value: Value = serde_json::from_str(raw.trim())
        .map_err(|error| format!("malformed pull request {pr}: {error}"))?;
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("pull request {pr} has no {key}"))
    };
    Ok(Pull {
        base_ref: field("base_ref")?,
        head_ref: field("head_ref")?,
        head_owner: field("head_owner")?,
    })
}

/// The commit `branch` points at now.
fn branch_tip(run_gh: RunGh<'_>, repo: &str, branch: &str) -> Result<String, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/git/ref/heads/{branch}"),
        "--jq".to_owned(),
        ".object.sha".to_owned(),
    ])?;
    let sha = raw.trim();
    if is_sha(sha) {
        Ok(sha.to_owned())
    } else {
        Err(format!("branch `{branch}` resolved to {sha:?}"))
    }
}

/// Why a pull request headed by this commit (found by this head branch or by
/// the commit) makes its runs untrustworthy (it targets a base other than [`TRUSTED_BASE`], whose
/// workflow definition `pull_request_target` would have run), or `None`.
fn foreign_base(
    run_gh: RunGh<'_>,
    repo: &str,
    head: &str,
    pull: &Pull,
) -> Result<Option<String>, String> {
    let fields = ".[] | {number, base: .base.ref, head_sha: .head.sha}";
    let by_branch = read_lines(
        run_gh,
        &format!(
            "repos/{repo}/pulls?state=all&head={}:{}&per_page=100",
            pull.head_owner,
            crate::required_check_policy::encode_path_segment(&pull.head_ref)
        ),
        fields,
    )?;
    let by_commit = read_lines(
        run_gh,
        &format!("repos/{repo}/commits/{head}/pulls?per_page=100"),
        fields,
    )?;
    // Only pull requests headed by this very commit: an older pull request
    // from the same branch says nothing about where this head's runs came from.
    let headed_here = |other: &&Value| {
        other
            .get("head_sha")
            .and_then(Value::as_str)
            .is_some_and(|sha| sha.eq_ignore_ascii_case(head))
    };
    Ok(by_branch
        .iter()
        .chain(&by_commit)
        .filter(headed_here)
        .find_map(|other| {
            let base = other.get("base").and_then(Value::as_str).unwrap_or("");
            (base != TRUSTED_BASE).then(|| {
                format!(
                    "pull request #{} with this head targets `{base}`, not `{TRUSTED_BASE}`, so \
                 its `{TRUSTED_EVENT}` runs used that branch's workflow",
                    other.get("number").and_then(Value::as_u64).unwrap_or(0)
                )
            })
        }))
}

/// Why `signal` does not admit `head` (pushed on branch `head_ref`), or
/// `None` when it does.
///
/// # Errors
///
/// When a read fails.
pub fn judge(
    run_gh: RunGh<'_>,
    repo: &str,
    head: &str,
    head_ref: &str,
    signal: &TrustedSignal,
) -> Result<Option<String>, String> {
    let name = &signal.name;
    let Some(file) = signal
        .workflow
        .strip_prefix(WORKFLOWS_DIR)
        .filter(|file| !file.is_empty() && !file.contains('/'))
    else {
        return Ok(Some(format!("`{name}` cannot be verified")));
    };
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
    let workflow_runs = read_lines(
        run_gh,
        &format!(
            "repos/{repo}/actions/workflows/{}/runs?head_sha={head}&event={TRUSTED_EVENT}&per_page=100",
            crate::required_check_policy::encode_path_segment(file)
        ),
        ".workflow_runs[] | {id, path, event, head_sha, head_branch, status, conclusion, \
         repository: .repository.full_name}",
    )?;
    let mut runs: BTreeMap<u64, Value> = workflow_runs
        .iter()
        .filter_map(|run| Some((run.get("id")?.as_u64()?, run.clone())))
        .collect();
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
        if let Some(why) = untrusted_run(&runs[&run_id], repo, head, head_ref, signal) {
            return Ok(Some(format!(
                "`{name}` status {id} names run {run_id}, which {why}"
            )));
        }
    }
    Ok(latest_refusal(
        repo,
        head,
        head_ref,
        signal,
        &statuses,
        &workflow_runs,
    ))
}

/// Why the workflow's newest run on `head`, or the newest status, does not
/// admit it: the newest run must be trusted and have succeeded, and the newest
/// status must be `success` and name exactly that run.
fn latest_refusal(
    repo: &str,
    head: &str,
    head_ref: &str,
    signal: &TrustedSignal,
    statuses: &[Value],
    workflow_runs: &[Value],
) -> Option<String> {
    let name = &signal.name;
    let workflow = &signal.workflow;
    let Some(latest) = newest(workflow_runs) else {
        return Some(format!(
            "no `{TRUSTED_EVENT}` run of `{workflow}` on head {}",
            short(head)
        ));
    };
    let latest_id = latest.get("id").and_then(Value::as_u64).unwrap_or(0);
    if let Some(why) =
        untrusted_run(latest, repo, head, head_ref, signal).or_else(|| unsuccessful(latest))
    {
        return Some(format!(
            "the newest run {latest_id} of `{workflow}` on head {} {why}",
            short(head)
        ));
    }
    let status = newest(statuses)?;
    let state = status
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("none");
    if state != "success" {
        return Some(format!("the newest `{name}` status is `{state}`"));
    }
    let named = run_id_from_url(
        status
            .get("target_url")
            .and_then(Value::as_str)
            .unwrap_or(""),
        repo,
    )
    .ok();
    (named != Some(latest_id)).then(|| {
        format!(
            "the newest `{name}` status names run {}, not the newest run {latest_id}",
            named.map_or_else(|| "none".to_owned(), |id| id.to_string())
        )
    })
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
fn untrusted_run(
    run: &Value,
    repo: &str,
    head: &str,
    head_ref: &str,
    signal: &TrustedSignal,
) -> Option<String> {
    let field = |key: &str| run.get(key).and_then(Value::as_str).unwrap_or("");
    if !field("repository").eq_ignore_ascii_case(repo) {
        return Some(format!("belongs to `{}`", field("repository")));
    }
    if field("path") != signal.workflow {
        return Some(format!("is `{}`, not `{}`", field("path"), signal.workflow));
    }
    if field("event") != TRUSTED_EVENT {
        return Some(format!("ran on `{}`, not {TRUSTED_EVENT}", field("event")));
    }
    if !field("head_sha").eq_ignore_ascii_case(head) {
        return Some(format!(
            "ran for {}, not {}",
            short(field("head_sha")),
            short(head)
        ));
    }
    if field("head_branch") != head_ref {
        return Some(format!(
            "ran on branch `{}`, not `{head_ref}`",
            field("head_branch")
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
        "{id, path, event, head_sha, head_branch, status, conclusion, repository: .repository.full_name}"
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
