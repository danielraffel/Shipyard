//! Read-only reconciliation of Pulp merge-group reuse artifacts with GitHub.
//!
//! The provider adapter owns pagination and captures a complete snapshot before
//! calling [`reconcile`]. Check-run snapshots therefore carry an explicit
//! `complete: true` marker; this module never treats a partial page as a green
//! gate. The `metrics import github-merge-receipt` command is the production
//! entry point and remains observational: it imports a receipt but cannot merge.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use super::merge_receipt::{MergeReceipt, SCHEMA};

const REUSE_SCHEMA: &str = "pulp-reuse-record/v1";
const POLICY_SCHEMA: &str = "shipyard.required-check-policy/v1";

/// Normalized branch-protection/ruleset evidence captured by the caller.
///
/// A bare list of required names is not authoritative: a caller could omit a
/// required gate and still manufacture a safe-merge metric. The policy object
/// is bound to the repository, base branch, and policy digest in the receipt.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredCheckPolicy {
    pub schema: String,
    pub repo: String,
    pub base_branch: String,
    pub required_checks: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReuseJob {
    pub schema: String,
    pub run_id: i64,
    pub run_attempt: i64,
    pub run_kind: String,
    pub pr: Option<i64>,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub merge_sha: Option<String>,
    pub merge_group_head_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunObservation {
    pub id: i64,
    pub run_attempt: i64,
    pub event: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub head_sha: String,
    pub head_branch: Option<String>,
    /// Parents of the exact merge-group commit, in GitHub's order. A
    /// single-PR merge group has the protected base first and that PR head
    /// second; a batched group has a synthetic second parent and is refused.
    pub head_commit_parents: Vec<String>,
    pub html_url: Option<String>,
    /// Required in the normalized projection. GitHub's `merge_group` payload
    /// legitimately supplies an empty array, so omission is not equivalent.
    pub pull_requests: Vec<RunPullRequest>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunPullRequest {
    pub number: i64,
    pub head_sha: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestObservation {
    pub number: i64,
    pub state: String,
    pub merged_at: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub head: PullRequestHead,
    pub base: PullRequestBase,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestHead {
    pub sha: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestBase {
    pub ref_name: Option<String>,
    #[serde(rename = "ref")]
    pub branch: Option<String>,
    pub repo: Option<PullRequestRepository>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestRepository {
    pub full_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRunsResponse {
    /// Provider-reported total after pagination; must equal `check_runs.len()`.
    pub total_count: usize,
    pub check_runs: Vec<CheckRunObservation>,
    /// True only when the provider transport has exhausted every page.
    pub complete: bool,
    /// Provider-scoped run identity for this complete snapshot.
    pub snapshot_run_id: i64,
    pub snapshot_run_attempt: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRunObservation {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub head_sha: Option<String>,
    pub html_url: Option<String>,
    pub details_url: Option<String>,
    /// Nullable in raw GitHub commit-wide responses; reconciliation rejects
    /// null values unless an upstream collector enriches them from run jobs.
    pub run_id: Option<i64>,
    pub run_attempt: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
struct GateEvidence<'a> {
    artifact: &'a ReuseJob,
    run: &'a RunObservation,
    pull_request: &'a PullRequestObservation,
    checks_snapshot: &'a CheckRunsResponse,
    policy: &'a RequiredCheckPolicy,
    artifact_url: &'a str,
    run_url: &'a str,
    required_checks: &'a [String],
    checks: &'a [CheckRunObservation],
    artifact_sha256: &'a str,
    policy_sha256: &'a str,
    collector_sha256: &'a str,
}

/// Reconcile one extracted `job.json` with exact GitHub observations.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn reconcile(
    repo: &str,
    project: &str,
    base_branch: &str,
    artifact_url: &str,
    run_url: &str,
    artifact_sha256: &str,
    policy_sha256: &str,
    collector_sha256: &str,
    artifact: &ReuseJob,
    run: &RunObservation,
    pull_request: &PullRequestObservation,
    checks: &CheckRunsResponse,
    policy: &RequiredCheckPolicy,
    required_checks: &[String],
) -> Result<MergeReceipt, String> {
    let repo = nonempty(repo, "repo")?;
    let project = nonempty(project, "project")?;
    let base_branch = nonempty(base_branch, "base branch")?;
    let artifact_url = nonempty(artifact_url, "artifact URL")?;
    let run_url = nonempty(run_url, "run URL")?;
    let artifact_prefix = format!("https://github.com/{repo}/actions/artifacts/");
    let artifact_id = artifact_url
        .strip_prefix(&artifact_prefix)
        .unwrap_or_default();
    if artifact_id.is_empty()
        || !artifact_id.bytes().all(|byte| byte.is_ascii_digit())
        || artifact_id.parse::<u64>().ok().is_none_or(|id| id == 0)
    {
        return Err("artifact URL is not bound to the requested repository artifact".to_owned());
    }
    let observed_run_url = run
        .html_url
        .as_deref()
        .ok_or("GitHub run URL is unavailable")?;
    if run_url != observed_run_url {
        return Err("run URL does not match the observed GitHub run".to_owned());
    }
    let run_prefix = format!("https://github.com/{repo}/actions/runs/");
    if !run_url.starts_with(&run_prefix) || run_url[run_prefix.len()..] != run.id.to_string() {
        return Err("run URL is not bound to the observed run identity".to_owned());
    }
    valid_sha256(artifact_sha256, "artifact SHA-256")?;
    valid_sha256(policy_sha256, "policy SHA-256")?;
    valid_sha256(collector_sha256, "collector SHA-256")?;
    if policy.schema != POLICY_SCHEMA {
        return Err("required-check policy schema is unsupported".to_owned());
    }
    if policy.repo != repo || policy.base_branch != base_branch {
        return Err(
            "required-check policy is bound to another repository or base branch".to_owned(),
        );
    }
    if policy.required_checks != required_checks {
        return Err("required checks do not match the authoritative policy snapshot".to_owned());
    }
    let policy_bytes = serde_json::to_vec(policy)
        .map_err(|error| format!("required-check policy encode failed: {error}"))?;
    let expected_policy_sha256 = format!("{:x}", Sha256::digest(policy_bytes));
    if expected_policy_sha256 != policy_sha256 {
        return Err("policy SHA-256 does not match the normalized policy snapshot".to_owned());
    }
    if artifact.schema != REUSE_SCHEMA {
        return Err(format!(
            "unsupported reuse artifact schema {:?}",
            artifact.schema
        ));
    }
    if artifact.run_kind != "merge_group" {
        return Err("reuse artifact is not a merge_group record".to_owned());
    }
    if artifact.run_id <= 0 || artifact.run_attempt <= 0 {
        return Err("reuse artifact run identity is invalid".to_owned());
    }
    if artifact.run_id != run.id || artifact.run_attempt != run.run_attempt {
        return Err("reuse artifact and GitHub run identity do not match".to_owned());
    }
    if run.event != "merge_group"
        || run.status != "completed"
        || run.conclusion.as_deref() != Some("success")
    {
        return Err("GitHub merge-group run is not terminal success".to_owned());
    }
    let merge_sha = artifact
        .merge_sha
        .as_deref()
        .ok_or("reuse artifact has no merge SHA")?;
    valid_sha(merge_sha, "merge SHA")?;
    if run.head_sha != merge_sha {
        return Err("merge-group run head SHA does not match artifact merge SHA".to_owned());
    }
    let head_sha = artifact
        .head_sha
        .as_deref()
        .ok_or("reuse artifact has no PR head SHA")?;
    valid_sha(head_sha, "PR head SHA")?;
    if pull_request.head.sha != head_sha {
        return Err("pull request head SHA does not match reuse artifact".to_owned());
    }
    let pr = artifact
        .pr
        .ok_or("reuse artifact PR identity is unavailable")?;
    if pr <= 0 || pull_request.number != pr {
        return Err("pull request number does not match reuse artifact".to_owned());
    }
    let queue_ref = artifact
        .merge_group_head_ref
        .as_deref()
        .or(run.head_branch.as_deref())
        .ok_or("merge-group queue ref is unavailable")?;
    let queue_identity = parse_queue_ref(queue_ref, base_branch)?;
    if queue_identity.pr != pr {
        return Err("merge-group queue ref does not identify the artifact PR".to_owned());
    }
    if artifact.base_sha.as_deref() != Some(queue_identity.base_sha.as_str()) {
        return Err("reuse artifact base SHA does not match the merge-group queue ref".to_owned());
    }
    if run.head_commit_parents.len() != 2
        || run.head_commit_parents[0] != queue_identity.base_sha
        || run.head_commit_parents[1] != head_sha
    {
        return Err(
            "merge-group commit parents do not prove a single-PR group for the artifact".to_owned(),
        );
    }
    if let (Some(artifact_ref), Some(run_ref)) = (
        artifact.merge_group_head_ref.as_deref(),
        run.head_branch.as_deref(),
    ) && canonical_queue_ref(artifact_ref) != canonical_queue_ref(run_ref)
    {
        return Err("artifact and GitHub run queue refs do not match".to_owned());
    }
    if run.pull_requests.is_empty() {
        // GitHub's merge_group run payload normally has no pull_requests array;
        // the exact queue ref is the authoritative PR binding in that shape.
    } else if run.pull_requests.len() != 1
        || run.pull_requests[0].number != pr
        || run.pull_requests[0].head_sha.as_deref() != Some(head_sha)
    {
        return Err("merge-group member identity does not match artifact and PR".to_owned());
    }
    if pull_request.state != "closed" || pull_request.merged_at.is_none() {
        return Err("pull request is not confirmed merged".to_owned());
    }
    let merged_at = pull_request
        .merged_at
        .as_deref()
        .ok_or("merged pull request has no merged_at")?
        .parse::<DateTime<Utc>>()
        .map_err(|error| format!("merged_at is invalid: {error}"))?;
    let merge_commit_sha = pull_request
        .merge_commit_sha
        .as_deref()
        .ok_or("merged pull request has no merge commit SHA")?;
    valid_sha(merge_commit_sha, "merge commit SHA")?;
    let branch = pull_request
        .base
        .branch
        .as_deref()
        .or(pull_request.base.ref_name.as_deref())
        .ok_or("pull request base branch is unavailable")?;
    if branch != base_branch {
        return Err("pull request base branch does not match requested branch".to_owned());
    }
    let observed_repo = pull_request
        .base
        .repo
        .as_ref()
        .and_then(|repo| repo.full_name.as_deref())
        .ok_or("pull request repository is unavailable")?;
    if observed_repo != repo {
        return Err("pull request repository does not match requested repository".to_owned());
    }
    if required_checks.is_empty() {
        return Err("required checks must be explicit and non-empty".to_owned());
    }
    if !checks.complete {
        return Err("required check snapshot is incomplete".to_owned());
    }
    if checks.total_count != checks.check_runs.len() {
        return Err("required check snapshot count is incomplete".to_owned());
    }
    if checks.snapshot_run_id <= 0 || checks.snapshot_run_attempt <= 0 {
        return Err("required check snapshot run identity is invalid".to_owned());
    }
    if checks.snapshot_run_id != run.id || checks.snapshot_run_attempt != run.run_attempt {
        return Err("required check snapshot is bound to another run attempt".to_owned());
    }
    let mut required_names = HashSet::with_capacity(required_checks.len());
    for required in required_checks {
        let required = nonempty(required, "required check")?;
        if !required_names.insert(required) {
            return Err(format!("required check {required:?} is duplicated"));
        }
    }
    let mut selected = Vec::with_capacity(required_checks.len());
    for required in required_checks {
        let required = nonempty(required, "required check")?;
        let matches: Vec<&CheckRunObservation> = checks
            .check_runs
            .iter()
            .filter(|check| check.name == required)
            .collect();
        if matches.len() != 1 {
            return Err(format!(
                "required check {required:?} is missing or ambiguous"
            ));
        }
        let check = matches[0];
        if check.status != "completed" || check.conclusion.as_deref() != Some("success") {
            return Err(format!(
                "required check {required:?} is not terminal success"
            ));
        }
        if check.head_sha.as_deref() != Some(merge_sha) {
            return Err(format!(
                "required check {required:?} is bound to another SHA"
            ));
        }
        if check.run_id != Some(run.id) || check.run_attempt != Some(run.run_attempt) {
            return Err(format!(
                "required check {required:?} is not bound to this run attempt"
            ));
        }
        for url in [check.html_url.as_deref(), check.details_url.as_deref()]
            .into_iter()
            .flatten()
        {
            let run_url_prefix = format!("https://github.com/{repo}/actions/runs/{}", run.id);
            if !(url == run_url_prefix || url.starts_with(&format!("{run_url_prefix}/"))) {
                return Err(format!(
                    "required check {required:?} URL is bound to another run"
                ));
            }
        }
        selected.push(check.clone());
    }
    let evidence = GateEvidence {
        artifact,
        run,
        pull_request,
        checks_snapshot: checks,
        policy,
        artifact_url,
        run_url,
        required_checks,
        checks: &selected,
        artifact_sha256,
        policy_sha256,
        collector_sha256,
    };
    let evidence_bytes = serde_json::to_vec(&evidence)
        .map_err(|error| format!("evidence encode failed: {error}"))?;
    let gate_receipt_sha256 = format!("{:x}", Sha256::digest(&evidence_bytes));
    let receipt = MergeReceipt {
        schema: SCHEMA.to_owned(),
        receipt_id: format!(
            "github:{repo}:merge-group:{}:{}:{pr}:{merge_commit_sha}",
            artifact.run_id, artifact.run_attempt
        ),
        repo: repo.to_owned(),
        project: project.to_owned(),
        base_branch: base_branch.to_owned(),
        pr,
        head_sha: head_sha.to_owned(),
        validated_head_sha: head_sha.to_owned(),
        merge_commit_sha: merge_commit_sha.to_owned(),
        merged_at,
        safe_merge: true,
        source: "shipyard".to_owned(),
        required_checks: required_checks.to_vec(),
        gate_receipt_sha256,
        evidence: format!(
            "artifact={artifact_url};artifact_sha256={artifact_sha256};run={run_url};merge_sha={merge_commit_sha};policy_sha256={policy_sha256};collector_sha256={collector_sha256}"
        ),
    };
    receipt
        .validate()
        .map_err(|error| format!("reconciled receipt invalid: {error}"))?;
    Ok(receipt)
}

struct QueueIdentity {
    pr: i64,
    base_sha: String,
}

fn parse_queue_ref(reference: &str, base_branch: &str) -> Result<QueueIdentity, String> {
    let reference = canonical_queue_ref(reference);
    let prefix = format!("gh-readonly-queue/{base_branch}/pr-");
    let rest = reference
        .strip_prefix(&prefix)
        .ok_or("merge-group queue ref does not match the expected base branch")?;
    let (pr_text, suffix) = rest
        .split_once('-')
        .ok_or("merge-group queue ref is missing its commit suffix")?;
    let pr = pr_text
        .parse::<i64>()
        .map_err(|_| "merge-group queue ref has an invalid PR number".to_owned())?;
    if pr <= 0
        || suffix.len() != 40
        || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        || suffix.chars().any(|ch| ch.is_ascii_uppercase())
        || suffix.chars().all(|ch| ch == '0')
        || suffix.contains('/')
    {
        return Err("merge-group queue ref has an invalid commit suffix".to_owned());
    }
    Ok(QueueIdentity {
        pr,
        base_sha: suffix.to_owned(),
    })
}

fn canonical_queue_ref(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

fn nonempty<'a>(value: &'a str, label: &str) -> Result<&'a str, String> {
    if value.trim().is_empty() {
        Err(format!("{label} is empty"))
    } else {
        Ok(value)
    }
}

fn valid_sha(value: &str, label: &str) -> Result<(), String> {
    if value.len() != 40
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.chars().any(|ch| ch.is_ascii_uppercase())
        || value.chars().all(|ch| ch == '0')
    {
        return Err(format!(
            "{label} must be a nonzero lowercase 40-character SHA"
        ));
    }
    Ok(())
}

fn valid_sha256(value: &str, label: &str) -> Result<(), String> {
    if value.len() != 64
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.chars().any(|ch| ch.is_ascii_uppercase())
        || value.chars().all(|ch| ch == '0')
    {
        return Err(format!("{label} must be a nonzero lowercase SHA-256"));
    }
    Ok(())
}

/// Decode one JSON snapshot while keeping the provider transport outside the core.
pub fn read_json<T: for<'de> serde::Deserialize<'de>>(
    path: &std::path::Path,
    label: &str,
) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{label} read failed: {error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{label} JSON decode failed: {error}"))
}

/// Decode an extracted `job.json`, rejecting top-level arrays.
pub fn read_job(path: &std::path::Path) -> Result<ReuseJob, String> {
    let value: Value = read_json(path, "reuse artifact")?;
    let object = value
        .as_object()
        .ok_or("reuse artifact job.json must be an object")?;
    let artifact = ReuseJob {
        schema: required_string(object, "schema")?,
        run_id: required_i64(object, "run_id")?,
        run_attempt: required_i64(object, "run_attempt")?,
        run_kind: required_string(object, "run_kind")?,
        pr: optional_i64(object, "pr")?,
        head_sha: optional_string(object, "head_sha")?,
        base_sha: optional_string(object, "base_sha")?,
        merge_sha: optional_string(object, "merge_sha")?,
        merge_group_head_ref: optional_string(object, "merge_group_head_ref")?,
    };
    Ok(artifact)
}

fn required_string(object: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("reuse artifact field {key:?} must be a string"))
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| format!("reuse artifact field {key:?} must be a string or null")),
    }
}

fn required_i64(object: &serde_json::Map<String, Value>, key: &str) -> Result<i64, String> {
    match object.get(key) {
        Some(Value::Number(value)) => value
            .as_i64()
            .ok_or_else(|| format!("reuse artifact field {key:?} must be an integer")),
        Some(Value::String(value)) => value
            .parse::<i64>()
            .map_err(|_| format!("reuse artifact field {key:?} must be an integer")),
        _ => Err(format!("reuse artifact field {key:?} must be an integer")),
    }
}

fn optional_i64(object: &serde_json::Map<String, Value>, key: &str) -> Result<Option<i64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("reuse artifact field {key:?} must be an integer or null")),
        Some(Value::String(value)) => value
            .parse::<i64>()
            .map(Some)
            .map_err(|_| format!("reuse artifact field {key:?} must be an integer or null")),
        Some(_) => Err(format!(
            "reuse artifact field {key:?} must be an integer or null"
        )),
    }
}

pub fn sha256_file(path: &std::path::Path, label: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{label} read failed: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_sha256(policy: &RequiredCheckPolicy) -> String {
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(policy).expect("policy JSON"))
        )
    }

    fn fixture() -> (
        ReuseJob,
        RunObservation,
        PullRequestObservation,
        CheckRunsResponse,
        RequiredCheckPolicy,
        Vec<String>,
    ) {
        let head = "a".repeat(40);
        let merge = "b".repeat(40);
        let job = ReuseJob {
            schema: REUSE_SCHEMA.to_owned(),
            run_id: 7,
            run_attempt: 1,
            run_kind: "merge_group".to_owned(),
            pr: Some(42),
            head_sha: Some(head.clone()),
            base_sha: Some("c".repeat(40)),
            merge_sha: Some(merge.clone()),
            merge_group_head_ref: Some(format!(
                "refs/heads/gh-readonly-queue/main/pr-42-{}",
                "c".repeat(40)
            )),
        };
        let run = RunObservation {
            id: 7,
            run_attempt: 1,
            event: "merge_group".to_owned(),
            status: "completed".to_owned(),
            conclusion: Some("success".to_owned()),
            head_sha: merge.clone(),
            head_branch: Some(format!("gh-readonly-queue/main/pr-42-{}", "c".repeat(40))),
            head_commit_parents: vec!["c".repeat(40), "a".repeat(40)],
            html_url: Some("https://github.com/Generous-Corp/pulp/actions/runs/7".to_owned()),
            pull_requests: vec![RunPullRequest {
                number: 42,
                head_sha: Some(head.clone()),
            }],
        };
        let pr = PullRequestObservation {
            number: 42,
            state: "closed".to_owned(),
            merged_at: Some("2026-10-25T00:00:00Z".to_owned()),
            merge_commit_sha: Some("d".repeat(40)),
            head: PullRequestHead { sha: head },
            base: PullRequestBase {
                ref_name: None,
                branch: Some("main".to_owned()),
                repo: Some(PullRequestRepository {
                    full_name: Some("Generous-Corp/pulp".to_owned()),
                }),
            },
        };
        let checks = CheckRunsResponse {
            total_count: 1,
            check_runs: vec![CheckRunObservation {
                id: 8,
                name: "macos".to_owned(),
                status: "completed".to_owned(),
                conclusion: Some("success".to_owned()),
                head_sha: Some(merge),
                html_url: None,
                details_url: None,
                run_id: Some(7),
                run_attempt: Some(1),
            }],
            complete: true,
            snapshot_run_id: 7,
            snapshot_run_attempt: 1,
        };
        let required = vec!["macos".to_owned()];
        let policy = RequiredCheckPolicy {
            schema: POLICY_SCHEMA.to_owned(),
            repo: "Generous-Corp/pulp".to_owned(),
            base_branch: "main".to_owned(),
            required_checks: required.clone(),
        };
        (job, run, pr, checks, policy, required)
    }

    #[test]
    fn read_job_projects_producer_shape_and_numeric_string_run_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("job.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "schema": REUSE_SCHEMA,
                "run_id": "7",
                "run_attempt": 1,
                "run_kind": "merge_group",
                "pr": 42,
                "head_sha": "a".repeat(40),
                "base_sha": "c".repeat(40),
                "merge_sha": "b".repeat(40),
                "merge_group_head_ref": "gh-readonly-queue/main/pr-42-cccccccccccccccccccccccccccccccccccccccc",
                "identity": {"usable": true},
                "runner_image": {"digest": "sha256:example"},
                "problems": [],
            })
            .to_string(),
        )
        .expect("job artifact");
        let job = read_job(&path).expect("producer-shaped job artifact");
        assert_eq!(job.run_id, 7);
        assert_eq!(job.pr, Some(42));
    }

    #[test]
    fn reconciles_exact_successful_merge_group() {
        let (job, run, pr, checks, policy, required) = fixture();
        let receipt = reconcile(
            "Generous-Corp/pulp",
            "pulp",
            "main",
            "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
            "https://github.com/Generous-Corp/pulp/actions/runs/7",
            &"a".repeat(64),
            &policy_sha256(&policy),
            &"c".repeat(64),
            &job,
            &run,
            &pr,
            &checks,
            &policy,
            &required,
        )
        .unwrap();
        assert_eq!(receipt.pr, 42);
        assert_eq!(receipt.head_sha, "a".repeat(40));
        assert_eq!(receipt.merge_commit_sha, "d".repeat(40));
    }

    #[test]
    fn rejects_required_check_subset_not_in_policy_snapshot() {
        let (job, run, pr, checks, mut policy, required) = fixture();
        policy.required_checks = vec!["macos".to_owned(), "linux".to_owned()];
        let error = reconcile(
            "Generous-Corp/pulp",
            "pulp",
            "main",
            "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
            "https://github.com/Generous-Corp/pulp/actions/runs/7",
            &"a".repeat(64),
            &policy_sha256(&policy),
            &"c".repeat(64),
            &job,
            &run,
            &pr,
            &checks,
            &policy,
            &required,
        )
        .expect_err("a caller-selected subset must not become a safe merge");
        assert!(error.contains("do not match"), "{error}");
    }

    #[test]
    fn reconciles_real_merge_group_shape_without_pull_request_array() {
        // GitHub's merge_group workflow-run payload normally omits membership
        // from pull_requests[]. The queue ref is the authoritative binding.
        let (job, mut run, pr, checks, policy, required) = fixture();
        run.pull_requests.clear();
        let receipt = reconcile(
            "Generous-Corp/pulp",
            "pulp",
            "main",
            "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
            "https://github.com/Generous-Corp/pulp/actions/runs/7",
            &"a".repeat(64),
            &policy_sha256(&policy),
            &"c".repeat(64),
            &job,
            &run,
            &pr,
            &checks,
            &policy,
            &required,
        )
        .expect("real-shaped merge-group payload reconciles");
        assert_eq!(receipt.pr, 42);
    }

    #[test]
    fn rejects_empty_membership_when_queue_ref_names_another_pr() {
        let (mut job, mut run, pr, checks, policy, required) = fixture();
        let queue_ref = "gh-readonly-queue/main/pr-43-cccccccccccccccccccccccccccccccccccccccc";
        job.merge_group_head_ref = Some(queue_ref.to_owned());
        run.head_branch = Some(queue_ref.to_owned());
        run.pull_requests.clear();
        let error = reconcile(
            "Generous-Corp/pulp",
            "pulp",
            "main",
            "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
            "https://github.com/Generous-Corp/pulp/actions/runs/7",
            &"a".repeat(64),
            &policy_sha256(&policy),
            &"c".repeat(64),
            &job,
            &run,
            &pr,
            &checks,
            &policy,
            &required,
        )
        .expect_err("queue ref must bind the artifact PR");
        assert!(error.contains("queue ref does not identify"), "{error}");
    }

    #[test]
    fn rejects_batched_or_reordered_merge_group_parents() {
        for parents in [
            vec!["a".repeat(40), "c".repeat(40)],
            vec!["c".repeat(40), "b".repeat(40)],
        ] {
            let (job, mut run, pr, checks, policy, required) = fixture();
            run.pull_requests.clear();
            run.head_commit_parents = parents;
            let error = reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &required,
            )
            .expect_err("only the exact single-PR parent order is accepted");
            assert!(error.contains("commit parents"), "{error}");
        }
    }

    #[test]
    fn rejects_run_identity_and_check_ambiguity() {
        let (mut job, run, pr, checks, policy, required) = fixture();
        job.run_attempt = 2;
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &required
            )
            .is_err()
        );
        let (job, run, pr, mut checks, policy, required) = fixture();
        checks.check_runs.push(checks.check_runs[0].clone());
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &required
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_commit_wide_check_shape_with_null_check_identity() {
        let (job, mut run, pr, mut checks, policy, required) = fixture();
        run.pull_requests.clear();
        checks.check_runs[0].run_id = None;
        checks.check_runs[0].run_attempt = None;
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &required,
            )
            .is_err()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn rejects_queue_ref_and_required_check_contract_violations() {
        let (mut job, run, pr, checks, policy, required) = fixture();
        job.merge_group_head_ref = Some(
            "gh-readonly-queue/release/pr-42-cccccccccccccccccccccccccccccccccccccccc".to_owned(),
        );
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &required,
            )
            .is_err()
        );

        let (job, run, pr, mut checks, policy, _) = fixture();
        checks.complete = false;
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &["macos".to_owned()],
            )
            .is_err()
        );

        let (job, run, pr, mut checks, policy, _) = fixture();
        checks.total_count = 2;
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &["macos".to_owned()],
            )
            .is_err()
        );

        let missing_count = serde_json::json!({
            "complete": true,
            "snapshot_run_id": 7,
            "snapshot_run_attempt": 1,
            "check_runs": []
        });
        assert!(serde_json::from_value::<CheckRunsResponse>(missing_count).is_err());

        let (_, run, _, _, _policy, _) = fixture();
        let mut missing_membership = serde_json::to_value(run).expect("run JSON");
        missing_membership
            .as_object_mut()
            .expect("run object")
            .remove("pull_requests");
        assert!(serde_json::from_value::<RunObservation>(missing_membership).is_err());

        let (job, run, pr, checks, policy, _) = fixture();
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &["macos".to_owned(), "macos".to_owned()],
            )
            .is_err()
        );

        let (job, run, pr, mut checks, policy, _) = fixture();
        checks.check_runs[0].details_url =
            Some("https://github.com/Generous-Corp/pulp/actions/runs/99/job/8".to_owned());
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &["macos".to_owned()],
            )
            .is_err()
        );

        let (job, run, pr, mut checks, policy, _) = fixture();
        checks.snapshot_run_attempt = 2;
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &["macos".to_owned()],
            )
            .is_err()
        );

        let (job, run, pr, checks, policy, _) = fixture();
        let mut run = run;
        run.pull_requests.push(RunPullRequest {
            number: 43,
            head_sha: Some("e".repeat(40)),
        });
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &["macos".to_owned()],
            )
            .is_err()
        );
    }
    #[test]
    fn rejects_non_merge_group_and_unmerged_pr() {
        let (mut job, run, pr, checks, policy, required) = fixture();
        job.run_kind = "pr_head".to_owned();
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &required
            )
            .is_err()
        );
        let (job, run, mut pr, checks, policy, required) = fixture();
        pr.merged_at = None;
        assert!(
            reconcile(
                "Generous-Corp/pulp",
                "pulp",
                "main",
                "https://github.com/Generous-Corp/pulp/actions/artifacts/1",
                "https://github.com/Generous-Corp/pulp/actions/runs/7",
                &"a".repeat(64),
                &policy_sha256(&policy),
                &"c".repeat(64),
                &job,
                &run,
                &pr,
                &checks,
                &policy,
                &required
            )
            .is_err()
        );
    }
}
