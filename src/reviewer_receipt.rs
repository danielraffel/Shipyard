//! Crash-safe publication boundary for reviewer claim receipts.
//!
//! This module deliberately contains no `gh` command construction or raw HTTP.
//! The authenticated GitHub App adapter implements [`CheckRunTransport`].
//! Keeping that seam explicit makes the create/read/reconcile contract testable
//! without accidentally introducing an ambient-token or direct-merge path.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use crate::evidence::canonical_repository;
use crate::gh::{GhClient, GhPrepareError, GhSupervision};
use crate::ship_state::ReviewerClaim;
use chrono::{DateTime, Utc};
use wait_timeout::ChildExt;

const GH_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

/// Immutable identity and fence carried by one reviewer check run.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ReviewerReceipt {
    /// Stable external check-run identity.
    pub external_id: String,
    /// Canonical repository slug.
    pub repository: String,
    /// Pull request number.
    pub pr: u64,
    /// Exact reviewed head.
    pub head_sha: String,
    /// Claimed reviewer identity.
    pub reviewer: String,
    /// Monotonic claim epoch.
    pub epoch: u64,
    /// Claim acquisition timestamp, serialized with an explicit UTC offset.
    pub claimed_at: DateTime<Utc>,
    /// Exclusive claim deadline, serialized with an explicit UTC offset.
    pub deadline_at: DateTime<Utc>,
}

/// Request passed to an authenticated check-run transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptRequest {
    /// Check-run name used by the App adapter.
    pub name: String,
    /// Exact receipt identity and fence.
    pub receipt: ReviewerReceipt,
}

/// Authenticated GitHub App transport for check-run reconciliation.
pub trait CheckRunTransport {
    /// Transport error type.
    type Error: fmt::Display;

    /// Find by the stable external id. The App adapter must authenticate this
    /// read and return only records from the requested repository/head.
    fn find(&mut self, request: &ReceiptRequest) -> Result<Option<ReviewerReceipt>, Self::Error>;

    /// Create one completed receipt. A conflict means another dispatcher won;
    /// the publisher must reconcile by calling `find` again.
    fn create(&mut self, request: &ReceiptRequest) -> Result<ReviewerReceipt, Self::Error>;
}

/// Authenticated GitHub App check-run transport.
///
/// This uses Shipyard's privileged, token-helper-backed `GhClient` command
/// boundary. It never falls back to ambient credentials or a direct merge API.
pub struct GhAppCheckRunTransport {
    client: GhClient,
    cwd: PathBuf,
    repository: String,
    app_slug: String,
    app_id: u64,
}

impl GhAppCheckRunTransport {
    /// Construct a transport bound to one canonical repository and App slug.
    pub fn new(
        client: GhClient,
        cwd: impl AsRef<Path>,
        repository: &str,
        app_slug: &str,
        app_id: u64,
    ) -> Result<Self, GhPrepareError> {
        Ok(Self {
            client: client.with_repo_override(repository)?,
            cwd: cwd.as_ref().to_path_buf(),
            repository: repository.to_owned(),
            app_slug: app_slug.to_owned(),
            app_id,
        })
    }

    fn run_json(&self, args: &[String], input: Option<&[u8]>) -> Result<serde_json::Value, String> {
        let mut command = self
            .client
            .prepare_privileged_command(&self.cwd, GhSupervision::Supervised)
            .map_err(|error| error.to_string())?;
        command.current_dir(&self.cwd).args(args);
        if input.is_some() {
            command.stdin(std::process::Stdio::piped());
        }
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        if let Some(input) = input {
            child
                .stdin
                .take()
                .ok_or_else(|| "gh stdin unavailable".to_owned())?
                .write_all(input)
                .map_err(|error| error.to_string())?;
        }
        // Never leave a per-PR claim lock held behind a wedged gh/token helper.
        // `wait_with_output` alone has no deadline and made a provider outage
        // indistinguishable from an active reviewer.  Kill and reap the child
        // before returning so a retry can reconcile the deterministic receipt.
        let completed = child
            .wait_timeout(GH_RECEIPT_TIMEOUT)
            .map_err(|error| error.to_string())?;
        if completed.is_none() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "gh reviewer receipt command timed out after {}s",
                GH_RECEIPT_TIMEOUT.as_secs()
            ));
        }
        let output: Output = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
    }
}

impl CheckRunTransport for GhAppCheckRunTransport {
    type Error = String;

    fn find(&mut self, request: &ReceiptRequest) -> Result<Option<ReviewerReceipt>, Self::Error> {
        let path = format!(
            "repos/{}/commits/{}/check-runs?per_page=100",
            self.repository, request.receipt.head_sha
        );
        let pages = self.run_json(
            &[
                "api".to_owned(),
                path,
                "--paginate".to_owned(),
                "--slurp".to_owned(),
            ],
            None,
        )?;
        let pages = pages
            .as_array()
            .ok_or_else(|| "check-runs pagination was not an array".to_owned())?;
        for page in pages {
            let runs = page
                .get("check_runs")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| "check-runs page missing check_runs".to_owned())?;
            for run in runs {
                if run.get("external_id").and_then(serde_json::Value::as_str)
                    != Some(request.receipt.external_id.as_str())
                {
                    continue;
                }
                return validate_run(run, request, &self.app_slug, self.app_id).map(Some);
            }
        }
        Ok(None)
    }

    fn create(&mut self, request: &ReceiptRequest) -> Result<ReviewerReceipt, Self::Error> {
        let body = serde_json::json!({
            "name": request.name,
            "head_sha": request.receipt.head_sha,
            "external_id": request.receipt.external_id,
            "status": "completed",
            "conclusion": "success",
            "output": {
                "title": "Reviewer claim receipt",
                "summary": serde_json::to_string(&request.receipt).map_err(|error| error.to_string())?
            }
        });
        let bytes = serde_json::to_vec(&body).map_err(|error| error.to_string())?;
        let path = format!("repos/{}/check-runs", self.repository);
        let value = self.run_json(&create_check_run_args(path), Some(&bytes))?;
        validate_run(&value, request, &self.app_slug, self.app_id)
    }
}

fn create_check_run_args(path: String) -> Vec<String> {
    vec![
        "api".to_owned(),
        path,
        "--method".to_owned(),
        "POST".to_owned(),
        // Keep the payload on stdin. A user-controlled body-file path would
        // bypass the authenticated command boundary and read arbitrary files.
        "--input".to_owned(),
        "-".to_owned(),
    ]
}

fn validate_run(
    run: &serde_json::Value,
    request: &ReceiptRequest,
    app_slug: &str,
    app_id: u64,
) -> Result<ReviewerReceipt, String> {
    if run.get("name").and_then(serde_json::Value::as_str) != Some(request.name.as_str()) {
        return Err("review receipt has an unexpected check name".to_owned());
    }
    if run.get("external_id").and_then(serde_json::Value::as_str)
        != Some(request.receipt.external_id.as_str())
    {
        return Err("review receipt has an unexpected external_id".to_owned());
    }
    if run.get("head_sha").and_then(serde_json::Value::as_str)
        != Some(request.receipt.head_sha.as_str())
    {
        return Err("review receipt has an unexpected head".to_owned());
    }
    if run
        .get("app")
        .and_then(|app| app.get("slug"))
        .and_then(serde_json::Value::as_str)
        != Some(app_slug)
    {
        return Err("review receipt was published by an unexpected App".to_owned());
    }
    if run
        .get("app")
        .and_then(|app| app.get("id"))
        .and_then(serde_json::Value::as_u64)
        != Some(app_id)
    {
        return Err("review receipt was published by an unexpected App id".to_owned());
    }
    if run.get("status").and_then(serde_json::Value::as_str) != Some("completed")
        || run.get("conclusion").and_then(serde_json::Value::as_str) != Some("success")
    {
        return Err("review receipt is not a completed successful check".to_owned());
    }
    let decoded = decode_receipt(run)?;
    if decoded != request.receipt {
        return Err("review receipt summary does not match its external identity".to_owned());
    }
    Ok(decoded)
}

fn decode_receipt(run: &serde_json::Value) -> Result<ReviewerReceipt, String> {
    let summary = run
        .get("output")
        .and_then(|output| output.get("summary"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "review receipt has no output summary".to_owned())?;
    serde_json::from_str(summary).map_err(|error| format!("malformed reviewer receipt: {error}"))
}

/// Fail-closed publication and reconciliation errors.
#[derive(Debug, Eq, PartialEq)]
pub enum ReceiptError {
    /// Authenticated transport failed before a receipt was observed.
    Transport(String),
    /// GitHub returned a receipt for a different claim.
    IdentityMismatch {
        /// Receipt key that was being reconciled.
        external_id: String,
    },
    /// Create may have succeeded but the authoritative readback failed.
    CreateLostAndUnreadable {
        /// Receipt key that may have been created.
        external_id: String,
        /// Original create error.
        error: String,
    },
}

impl fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "review receipt transport failed: {error}"),
            Self::IdentityMismatch { external_id } => {
                write!(f, "review receipt identity mismatch for {external_id}")
            }
            Self::CreateLostAndUnreadable { external_id, error } => write!(
                f,
                "review receipt create for {external_id} may have succeeded but reconciliation failed: {error}"
            ),
        }
    }
}

impl std::error::Error for ReceiptError {}

/// Convert a persisted claim into an externally publishable request.
#[must_use]
pub fn request_from_claim(repository: &str, pr: u64, claim: &ReviewerClaim) -> ReceiptRequest {
    let repository = canonical_repository(repository);
    let reviewer = claim.reviewer.trim().to_ascii_lowercase();
    ReceiptRequest {
        name: "reviewer-claim".to_owned(),
        receipt: ReviewerReceipt {
            external_id: claim.receipt_id(&repository, pr),
            repository,
            pr,
            head_sha: claim.head_sha.clone(),
            reviewer,
            epoch: claim.epoch,
            claimed_at: claim.claimed_at,
            deadline_at: claim.deadline_at,
        },
    }
}

fn same_identity(expected: &ReviewerReceipt, actual: &ReviewerReceipt) -> bool {
    expected == actual
}

/// Publish or reconcile one receipt exactly once.
///
/// A successful create is never repeated after a process restart: the stable
/// external id is read first. If create returns an ambiguous error, a second
/// authenticated read determines whether GitHub accepted it; an unreadable
/// result remains an explicit error rather than creating a duplicate.
pub fn publish_or_reconcile<T: CheckRunTransport>(
    transport: &mut T,
    request: &ReceiptRequest,
) -> Result<ReviewerReceipt, ReceiptError> {
    if let Some(existing) = transport
        .find(request)
        .map_err(|error| ReceiptError::Transport(error.to_string()))?
    {
        if same_identity(&request.receipt, &existing) {
            return Ok(existing);
        }
        return Err(ReceiptError::IdentityMismatch {
            external_id: request.receipt.external_id.clone(),
        });
    }

    match transport.create(request) {
        Ok(created) if same_identity(&request.receipt, &created) => Ok(created),
        Ok(_) => Err(ReceiptError::IdentityMismatch {
            external_id: request.receipt.external_id.clone(),
        }),
        Err(create_error) => match transport.find(request) {
            Ok(Some(existing)) if same_identity(&request.receipt, &existing) => Ok(existing),
            Ok(Some(_)) => Err(ReceiptError::IdentityMismatch {
                external_id: request.receipt.external_id.clone(),
            }),
            Ok(None) | Err(_) => Err(ReceiptError::CreateLostAndUnreadable {
                external_id: request.receipt.external_id.clone(),
                error: create_error.to_string(),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Fake {
        records: BTreeMap<String, ReviewerReceipt>,
        create_calls: usize,
        ambiguous_create: bool,
    }

    impl CheckRunTransport for Fake {
        type Error = String;

        fn find(
            &mut self,
            request: &ReceiptRequest,
        ) -> Result<Option<ReviewerReceipt>, Self::Error> {
            Ok(self.records.get(&request.receipt.external_id).cloned())
        }

        fn create(&mut self, request: &ReceiptRequest) -> Result<ReviewerReceipt, Self::Error> {
            self.create_calls += 1;
            self.records
                .insert(request.receipt.external_id.clone(), request.receipt.clone());
            if self.ambiguous_create {
                Err("connection reset after publish".to_owned())
            } else {
                Ok(request.receipt.clone())
            }
        }
    }

    fn request() -> ReceiptRequest {
        let claim = ReviewerClaim {
            head_sha: "a".repeat(40),
            epoch: 3,
            reviewer: "pulp-review".to_owned(),
            claimed_at: Utc::now(),
            deadline_at: Utc::now() + Duration::hours(1),
        };
        request_from_claim("danielraffel/Shipyard", 974, &claim)
    }

    #[test]
    fn restart_reconciles_without_duplicate_create() {
        let request = request();
        let mut transport = Fake::default();
        assert_eq!(
            publish_or_reconcile(&mut transport, &request),
            Ok(request.receipt.clone())
        );
        assert_eq!(
            publish_or_reconcile(&mut transport, &request),
            Ok(request.receipt.clone())
        );
        assert_eq!(transport.create_calls, 1);
    }

    #[test]
    fn ambiguous_create_is_reconciled() {
        let request = request();
        let mut transport = Fake {
            ambiguous_create: true,
            ..Fake::default()
        };
        assert_eq!(
            publish_or_reconcile(&mut transport, &request),
            Ok(request.receipt.clone())
        );
        assert_eq!(transport.create_calls, 1);
    }

    #[test]
    fn ambiguous_create_without_readback_stays_explicit() {
        let request = request();
        struct Unreadable;
        impl CheckRunTransport for Unreadable {
            type Error = &'static str;
            fn find(&mut self, _: &ReceiptRequest) -> Result<Option<ReviewerReceipt>, Self::Error> {
                Err("offline")
            }
            fn create(&mut self, _: &ReceiptRequest) -> Result<ReviewerReceipt, Self::Error> {
                Err("reset")
            }
        }
        assert!(matches!(
            publish_or_reconcile(&mut Unreadable, &request),
            Err(ReceiptError::Transport(_))
        ));
    }

    #[test]
    fn malformed_timestamp_and_receipt_id_are_rejected() {
        let request = request();
        let mut receipt = serde_json::to_value(&request.receipt).expect("receipt serializes");
        receipt["claimed_at"] = serde_json::Value::String("not-a-timezone".to_owned());
        let run = serde_json::json!({
            "external_id": request.receipt.external_id,
            "output": {"summary": receipt.to_string()}
        });
        assert!(decode_receipt(&run).is_err());

        let mut receipt = serde_json::to_value(&request.receipt).expect("receipt serializes");
        receipt["external_id"] = serde_json::Value::String("stale".to_owned());
        let run = serde_json::json!({"output": {"summary": receipt.to_string()}});
        let decoded = decode_receipt(&run).expect("well-formed receipt");
        assert_ne!(decoded.external_id, request.receipt.external_id);
    }

    #[test]
    fn run_identity_rejects_wrong_app_head_or_external_id() {
        let request = request();
        let summary = serde_json::to_string(&request.receipt).expect("receipt serializes");
        let base = || {
            serde_json::json!({
                "name": request.name,
                "external_id": request.receipt.external_id,
                "head_sha": request.receipt.head_sha,
            "app": {"slug": "shipyard-review", "id": 42},
                "output": {"summary": summary}
            })
        };
        let mut wrong = base();
        wrong["app"]["slug"] = serde_json::Value::String("ambient-gh".to_owned());
        assert!(validate_run(&wrong, &request, "shipyard-review", 42).is_err());
        let mut wrong = base();
        wrong["head_sha"] = serde_json::Value::String("b".repeat(40));
        assert!(validate_run(&wrong, &request, "shipyard-review", 42).is_err());
        let mut wrong = base();
        wrong["external_id"] = serde_json::Value::String("stale".to_owned());
        assert!(validate_run(&wrong, &request, "shipyard-review", 42).is_err());
    }

    #[test]
    fn create_payload_is_stdin_only() {
        let args = create_check_run_args("repos/o/r/check-runs".to_owned());
        assert!(args.windows(2).any(|pair| pair == ["--input", "-"]));
        assert!(!args.iter().any(|arg| arg == "--body-file"));
    }
}
