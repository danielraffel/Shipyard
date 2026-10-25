//! Durable witness for ships whose verdict is delegated to GitHub required checks.
//!
//! The delegated path intentionally does not create ordinary `ShipState`: it has
//! no Shipyard runs or evidence.  It still needs a durable identity before native
//! auto-merge is armed, otherwise a later merge observer cannot distinguish a
//! delegated, exact-head admission from an unrelated native merge.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::metrics::merge_receipt::{MergeReceipt, SCHEMA as MERGE_RECEIPT_SCHEMA};

/// Serialized delegated witness schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Authoritative post-merge observation supplied by the GitHub reader.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergedObservation {
    /// Merged commit's pull-request head SHA.
    pub head_sha: String,
    /// Base branch observed at merge.
    pub base_branch: String,
    /// Whether GitHub confirms the pull request merged.
    pub merged: bool,
    /// GitHub merge commit SHA, when available.
    pub merge_commit_sha: Option<String>,
    /// GitHub merge timestamp, when available.
    pub merged_at: Option<DateTime<Utc>>,
    /// Required check context -> terminal conclusion on the exact head.
    pub required_checks: std::collections::BTreeMap<String, String>,
    /// Digest of the authoritative policy marker and canonical required-check
    /// name/status/conclusion observations on the exact merged head.
    pub policy_evidence_digest: String,
    /// Protected-branch policy identities observed at merge.
    pub policy_checks: Vec<String>,
    /// Canonical full identity/status/conclusion evidence for every required check.
    pub policy_evidence: Vec<Value>,
}

/// Durable identity witness for a delegated validation admission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DelegatedValidationWitness {
    /// Serialized witness schema version.
    pub schema_version: u32,
    /// Canonical repository slug.
    pub repo: String,
    /// Pull-request number.
    pub pr: u64,
    /// Exact head admitted for delegated validation.
    pub head_sha: String,
    /// Base branch admitted for delegated validation.
    pub base_branch: String,
    /// Source branch admitted for delegated validation.
    pub branch: String,
    /// Time the witness was created.
    pub created_at: DateTime<Utc>,
    /// Time native auto-merge was confirmed armed.
    pub armed_at: Option<DateTime<Utc>>,
    /// The arm/review identity is recorded when the caller can provide one.
    /// `None` is explicit: it never authorizes a receipt by itself.
    #[serde(default)]
    pub approval_identity: Option<String>,
    /// Exact protected-branch required context/app identities captured before arm.
    #[serde(default)]
    pub policy_checks: Vec<String>,
}

impl DelegatedValidationWitness {
    /// Construct an unarmed witness for one exact pull-request head.
    #[must_use]
    pub fn new(
        repo: impl Into<String>,
        pr: u64,
        head_sha: impl Into<String>,
        base_branch: impl Into<String>,
        branch: impl Into<String>,
        approval_identity: Option<String>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            repo: repo.into(),
            pr,
            head_sha: head_sha.into(),
            base_branch: base_branch.into(),
            branch: branch.into(),
            created_at: Utc::now(),
            armed_at: None,
            approval_identity,
            policy_checks: Vec::new(),
        }
    }

    #[must_use]
    /// Return the durable, collision-resistant witness path.
    pub fn path(state_dir: &Path, repo: &str, pr: u64) -> PathBuf {
        // A sanitized slug is not a unique filename (`a/b_c` and
        // `a_b/c` collide). Keep the path opaque and collision-resistant;
        // the serialized witness remains the source of the human identity.
        let key = sha256(repo.as_bytes());
        state_dir
            .join("delegated-validation")
            .join(format!("{key}-{pr}.json"))
    }

    /// Atomically persist the witness before issuing any GitHub arm mutation.
    pub fn save(&self, state_dir: &Path) -> io::Result<()> {
        let path = Self::path(state_dir, &self.repo, self.pr);
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("delegated witness has no parent"))?;
        fs::create_dir_all(parent)?;
        let temp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| io::Error::other(format!("serialize delegated witness: {error}")))?;
        let mut file = fs::File::create(&temp)?;
        std::io::Write::write_all(&mut file, &bytes)?;
        file.sync_all()?;
        fs::rename(temp, &path)?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    /// Mark a previously persisted witness as having received native arm.
    pub fn mark_armed(&mut self, state_dir: &Path) -> io::Result<()> {
        self.armed_at = Some(Utc::now());
        self.save(state_dir)
    }

    /// Convert a verified merged observation into the normal safe-merge receipt.
    /// Every identity and provenance field is required; this function never
    /// infers a receipt from merged status alone.
    pub fn receipt(&self, observation: &MergedObservation) -> Result<MergeReceipt, String> {
        if self.armed_at.is_none() {
            return Err("delegated receipt refused: witness was never armed".to_owned());
        }
        let approval = self.approval_identity.as_deref().ok_or_else(|| {
            "delegated receipt refused: approval identity is unavailable".to_owned()
        })?;
        validate_approval_identity(approval)?;
        let approval_value: Value = serde_json::from_str(approval)
            .map_err(|_| "delegated receipt refused: malformed approval identity".to_owned())?;
        let approval_head = approval_value
            .get("head_sha")
            .and_then(Value::as_str)
            .filter(|head| is_sha(head))
            .ok_or_else(|| {
                "delegated receipt refused: approval head identity is unavailable".to_owned()
            })?;
        if !same_sha(approval_head, &self.head_sha) {
            return Err(
                "delegated receipt refused: approval head differs from witnessed head".to_owned(),
            );
        }
        if self.policy_checks.is_empty() {
            return Err(
                "delegated receipt refused: protected-branch policy identity is unavailable"
                    .to_owned(),
            );
        }
        if self.policy_checks != observation.policy_checks {
            return Err("delegated receipt refused: protected-branch policy drifted".to_owned());
        }
        if !observation.merged {
            return Err("delegated receipt refused: merge is not confirmed".to_owned());
        }
        if !same_sha(&self.head_sha, &observation.head_sha) {
            return Err(
                "delegated receipt refused: merged head differs from witnessed head".to_owned(),
            );
        }
        if self.base_branch != observation.base_branch {
            return Err(
                "delegated receipt refused: merged base differs from witnessed base".to_owned(),
            );
        }
        let merge_commit_sha = observation
            .merge_commit_sha
            .as_deref()
            .filter(|sha| is_sha(sha))
            .ok_or_else(|| "delegated receipt refused: merge commit is unavailable".to_owned())?;
        let merged_at = observation.merged_at.ok_or_else(|| {
            "delegated receipt refused: merged timestamp is unavailable".to_owned()
        })?;
        if observation.required_checks.is_empty()
            || observation
                .required_checks
                .values()
                .any(|conclusion| !conclusion.eq_ignore_ascii_case("success"))
        {
            return Err(
                "delegated receipt refused: required checks are not terminal green".to_owned(),
            );
        }
        if observation.policy_evidence_digest.len() != 64
            || !observation
                .policy_evidence_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(
                "delegated receipt refused: required-check policy evidence digest is unavailable"
                    .to_owned(),
            );
        }
        let canonical_policy_evidence = serde_json::json!({
            "required_checks_known": true,
            "checks": observation.policy_evidence,
        });
        let computed_policy_digest = sha256(
            serde_json::to_vec(&canonical_policy_evidence).map_err(|error| error.to_string())?,
        );
        if !computed_policy_digest.eq_ignore_ascii_case(&observation.policy_evidence_digest) {
            return Err("delegated receipt refused: required-check policy evidence digest does not match evidence".to_owned());
        }
        let required_checks = observation
            .required_checks
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let gate_payload = serde_json::json!({
            "repo": self.repo,
            "pr": self.pr,
            "base_branch": self.base_branch,
            "head_sha": self.head_sha,
            "required_checks": required_checks,
            "policy_evidence_digest": observation.policy_evidence_digest,
            "policy_checks": self.policy_checks,
            "policy_evidence": observation.policy_evidence,
            "approval_identity": self.approval_identity,
            "armed_at": self.armed_at,
        });
        let gate_receipt_sha256 =
            sha256(serde_json::to_vec(&gate_payload).map_err(|e| e.to_string())?);
        let receipt_id = sha256(format!(
            "{}|{}|{}|{}|{}",
            self.repo,
            self.pr,
            self.head_sha,
            merge_commit_sha,
            merged_at.to_rfc3339()
        ));
        Ok(MergeReceipt {
            schema: MERGE_RECEIPT_SCHEMA.to_owned(),
            receipt_id,
            repo: self.repo.clone(),
            project: self
                .repo
                .rsplit('/')
                .next()
                .unwrap_or(&self.repo)
                .to_owned(),
            base_branch: self.base_branch.clone(),
            pr: i64::try_from(self.pr)
                .map_err(|_| "delegated receipt refused: PR number out of range".to_owned())?,
            head_sha: self.head_sha.clone(),
            validated_head_sha: self.head_sha.clone(),
            merge_commit_sha: merge_commit_sha.to_owned(),
            merged_at,
            safe_merge: true,
            source: "shipyard".to_owned(),
            required_checks,
            gate_receipt_sha256: gate_receipt_sha256.clone(),
            evidence: format!("sha256:{gate_receipt_sha256};merge:{merge_commit_sha}"),
        })
    }

    /// Retire the witness only after its receipt has been persisted.
    pub fn retire(&self, state_dir: &Path) -> io::Result<()> {
        let path = Self::path(state_dir, &self.repo, self.pr);
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// Extract the exact protected-branch policy identity attached by the live
/// GitHub reader. A materialized rollup alone cannot prove this set.
pub fn policy_checks_from_snapshot(snapshot: &Value) -> Result<Vec<String>, String> {
    let entries = snapshot
        .get("_required_check_policy")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "delegated receipt refused: protected-branch policy identity is unavailable".to_owned()
        })?;
    let mut checks = Vec::with_capacity(entries.len());
    for entry in entries {
        let object = entry.as_object().ok_or_else(|| {
            "delegated receipt refused: malformed protected-branch policy identity".to_owned()
        })?;
        let context = object
            .get("context")
            .and_then(Value::as_str)
            .filter(|context| !context.is_empty())
            .ok_or_else(|| {
                "delegated receipt refused: protected-branch policy context is missing".to_owned()
            })?;
        let app = match object.get("app_id") {
            None | Some(Value::Null) => "any".to_owned(),
            Some(value) => value.as_u64().map(|id| id.to_string()).ok_or_else(|| {
                "delegated receipt refused: protected-branch app identity is malformed".to_owned()
            })?,
        };
        checks.push(format!("{context}@app:{app}"));
    }
    let mut sorted = checks.clone();
    sorted.sort();
    if checks.is_empty() || sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(
            "delegated receipt refused: protected-branch policy identity is empty or duplicated"
                .to_owned(),
        );
    }
    Ok(sorted)
}

fn validate_approval_identity(raw: &str) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|_| "delegated receipt refused: malformed approval identity".to_owned())?;
    let object = value.as_object().ok_or_else(|| {
        "delegated receipt refused: approval identity is not an object".to_owned()
    })?;
    let via = object
        .get("via")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if via == "head_bound" {
        let head = object
            .get("head_sha")
            .and_then(Value::as_str)
            .filter(|head| is_sha(head))
            .ok_or_else(|| {
                "delegated receipt refused: approval head identity is malformed".to_owned()
            })?;
        let _ = head;
        let nested = object.get("approval").ok_or_else(|| {
            "delegated receipt refused: head-bound approval is incomplete".to_owned()
        })?;
        return validate_approval_value(nested);
    }
    if !matches!(via, "review" | "marker" | "carried") {
        return Err("delegated receipt refused: approval identity has unknown schema".to_owned());
    }
    if via == "review" || via == "marker" {
        if object
            .get("login")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err("delegated receipt refused: approval reviewer login is missing".to_owned());
        }
    }
    if let Some(app_id) = object.get("performed_via_github_app_id")
        && !app_id.is_null()
        && app_id.as_u64().is_none()
    {
        return Err("delegated receipt refused: approval app identity is malformed".to_owned());
    }
    if via == "marker"
        && object
            .get("comment_id")
            .and_then(serde_json::Value::as_u64)
            .is_none()
    {
        return Err("delegated receipt refused: approval comment identity is missing".to_owned());
    }
    if via == "carried" {
        if object
            .get("from_head")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|head| !is_sha(head))
            || object.get("approval").is_none()
        {
            return Err(
                "delegated receipt refused: carried approval provenance is incomplete".to_owned(),
            );
        }
        validate_approval_value(object.get("approval").expect("checked above"))?;
    }
    Ok(())
}

fn validate_approval_value(value: &Value) -> Result<(), String> {
    let raw = serde_json::to_string(value)
        .map_err(|_| "delegated receipt refused: malformed carried approval".to_owned())?;
    validate_approval_identity(&raw)
}

fn same_sha(left: &str, right: &str) -> bool {
    left.len() == 40 && right.len() == 40 && left.eq_ignore_ascii_case(right)
}

fn is_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256(value: impl AsRef<[u8]>) -> String {
    let mut digest = Sha256::new();
    digest.update(value);
    format!("{:x}", digest.finalize())
}

pub(crate) fn evidence_digest(value: impl AsRef<[u8]>) -> String {
    sha256(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_repo_scoped_and_cannot_escape_state_dir() {
        let path = DelegatedValidationWitness::path(Path::new("/state"), "owner/repo", 42);
        assert_eq!(
            path,
            PathBuf::from(format!(
                "/state/delegated-validation/{}-42.json",
                sha256("owner/repo")
            ))
        );
        assert_ne!(
            DelegatedValidationWitness::path(Path::new("/state"), "a/b_c", 42),
            DelegatedValidationWitness::path(Path::new("/state"), "a_b/c", 42)
        );
    }

    #[test]
    fn save_is_readable_before_arm_and_after_arm() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut witness = DelegatedValidationWitness::new(
            "owner/repo",
            42,
            "a".repeat(40),
            "main",
            "feature",
            Some(format!(
                r#"{{"via":"head_bound","head_sha":"{}","approval":{{"via":"marker","login":"reviewer","comment_id":7}}}}"#,
                "a".repeat(40)
            )),
        );
        witness.save(dir.path()).expect("save");
        let path = DelegatedValidationWitness::path(dir.path(), "owner/repo", 42);
        let before: DelegatedValidationWitness =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("parse");
        assert!(before.armed_at.is_none());
        witness.mark_armed(dir.path()).expect("mark armed");
        let after: DelegatedValidationWitness =
            serde_json::from_slice(&fs::read(path).expect("read")).expect("parse");
        assert!(after.armed_at.is_some());
    }

    fn witness() -> DelegatedValidationWitness {
        let mut witness = DelegatedValidationWitness::new(
            "owner/repo",
            42,
            "a".repeat(40),
            "main",
            "feature",
            Some(format!(
                r#"{{"via":"head_bound","head_sha":"{}","approval":{{"via":"review","login":"reviewer","performed_via_github_app_id":1}}}}"#,
                "a".repeat(40)
            )),
        );
        witness.armed_at = Some(Utc::now());
        witness.policy_checks = vec!["macos@app:1".to_owned()];
        witness
    }

    fn observation() -> MergedObservation {
        let policy_evidence = vec![serde_json::json!({
            "name": "macos",
            "app_id": 1,
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
        })];
        MergedObservation {
            head_sha: "a".repeat(40),
            base_branch: "main".to_owned(),
            merged: true,
            merge_commit_sha: Some("b".repeat(40)),
            merged_at: Some(Utc::now()),
            required_checks: std::collections::BTreeMap::from([(
                "macos".to_owned(),
                "SUCCESS".to_owned(),
            )]),
            policy_evidence_digest: evidence_digest(
                serde_json::to_vec(&serde_json::json!({
                    "required_checks_known": true,
                    "checks": policy_evidence,
                }))
                .expect("policy evidence"),
            ),
            policy_checks: vec!["macos@app:1".to_owned()],
            policy_evidence,
        }
    }

    #[test]
    fn receipt_requires_exact_identity_and_green_required_checks() {
        let receipt = witness().receipt(&observation()).expect("receipt");
        assert_eq!(receipt.schema, MERGE_RECEIPT_SCHEMA);
        assert_eq!(receipt.required_checks, vec!["macos"]);
        for (mutated, expected) in [
            (
                {
                    let mut value = observation();
                    value.head_sha = "c".repeat(40);
                    value
                },
                "merged head differs",
            ),
            (
                {
                    let mut value = observation();
                    value.merge_commit_sha = None;
                    value
                },
                "merge commit is unavailable",
            ),
            (
                {
                    let mut value = observation();
                    value.required_checks.clear();
                    value
                },
                "required checks are not terminal green",
            ),
        ] {
            let error = witness().receipt(&mutated).expect_err("negative control");
            assert!(error.contains(expected), "{error}");
        }
        let mut no_approval = witness();
        no_approval.approval_identity = None;
        assert!(no_approval.receipt(&observation()).is_err());
    }

    #[test]
    fn approval_identity_schema_is_recursive_and_strict() {
        let mut malformed = witness();
        malformed.approval_identity = Some(r#"{"via":"carried","from_head":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","approval":{"via":"marker","login":"reviewer"}}"#.to_owned());
        assert!(malformed.receipt(&observation()).is_err());
        malformed.approval_identity = Some(
            r#"{"via":"review","login":"reviewer","performed_via_github_app_id":"1"}"#.to_owned(),
        );
        assert!(malformed.receipt(&observation()).is_err());
    }

    #[test]
    fn policy_identity_requires_unique_entries() {
        let duplicate = serde_json::json!({
            "_required_check_policy": [
                {"context":"macos","app_id":1},
                {"context":"macos","app_id":1}
            ]
        });
        assert!(policy_checks_from_snapshot(&duplicate).is_err());
    }
}
