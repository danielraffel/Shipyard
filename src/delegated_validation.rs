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
use sha2::{Digest, Sha256};

use crate::metrics::merge_receipt::{MergeReceipt, SCHEMA as MERGE_RECEIPT_SCHEMA};

pub const SCHEMA_VERSION: u32 = 1;

/// Authoritative post-merge observation supplied by the GitHub reader.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergedObservation {
    pub head_sha: String,
    pub base_branch: String,
    pub merged: bool,
    pub merge_commit_sha: Option<String>,
    pub merged_at: Option<DateTime<Utc>>,
    /// Required check context -> terminal conclusion on the exact head.
    pub required_checks: std::collections::BTreeMap<String, String>,
    /// Digest of the authoritative policy marker and canonical required-check
    /// name/status/conclusion observations on the exact merged head.
    pub policy_evidence_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DelegatedValidationWitness {
    pub schema_version: u32,
    pub repo: String,
    pub pr: u64,
    pub head_sha: String,
    pub base_branch: String,
    pub branch: String,
    pub created_at: DateTime<Utc>,
    pub armed_at: Option<DateTime<Utc>>,
    /// The arm/review identity is recorded when the caller can provide one.
    /// `None` is explicit: it never authorizes a receipt by itself.
    #[serde(default)]
    pub approval_identity: Option<String>,
}

impl DelegatedValidationWitness {
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
        }
    }

    #[must_use]
    pub fn path(state_dir: &Path, repo: &str, pr: u64) -> PathBuf {
        let key = repo.replace(['/', '\\'], "_");
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
        fs::write(&temp, bytes)?;
        fs::rename(temp, path)
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
        if self.approval_identity.is_none() {
            return Err("delegated receipt refused: approval identity is unavailable".to_owned());
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
            PathBuf::from("/state/delegated-validation/owner_repo-42.json")
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
            Some("reviewer".to_owned()),
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
            Some("reviewer".to_owned()),
        );
        witness.armed_at = Some(Utc::now());
        witness
    }

    fn observation() -> MergedObservation {
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
            policy_evidence_digest: "c".repeat(64),
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
}
