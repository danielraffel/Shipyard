//! Crash-safe publication boundary for reviewer claim receipts.
//!
//! This module deliberately contains no `gh` command construction or raw HTTP.
//! The authenticated GitHub App adapter implements [`CheckRunTransport`].
//! Keeping that seam explicit makes the create/read/reconcile contract testable
//! without accidentally introducing an ambient-token or direct-merge path.

use std::fmt;
use std::io::Write;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use crate::evidence::canonical_repository;
use crate::gh::{GhClient, GhPrepareError, GhSupervision};
use crate::ship_state::ReviewerClaim;
use chrono::{DateTime, Utc};
use wait_timeout::ChildExt;

const GH_RECEIPT_TIMEOUT: Duration = Duration::from_mins(1);

/// Check-run name every reviewer claim receipt carries. The protected base's
/// `[auto_merge] reviewer_claim_receipt` must name exactly this check.
pub const REVIEWER_CLAIM_CHECK_NAME: &str = "reviewer-claim";
/// `[auto_merge]` key naming the receipt check run.
pub const CLAIM_RECEIPT_KEY: &str = "reviewer_claim_receipt";
/// `[auto_merge]` key naming the GitHub App that alone publishes receipts.
pub const CLAIM_APP_KEY: &str = "reviewer_claim_app";
/// GitHub Actions' App id. Every workflow token carries it, so it can never be
/// a reviewer authority; `scripts/ghapp_queue_arm_guard.py` refuses it too.
pub const SHARED_ACTIONS_APP_ID: u64 = 15368;
/// App slugs shared by every agent or workflow, refused as receipt authority.
/// The Python guard refuses the same names.
const SHARED_CLAIM_APP_SLUGS: &[&str] = &["github-actions", "shipyard-local"];

/// The GitHub App the protected base names as the only receipt publisher.
///
/// The base's `reviewer_claim_app` is either an App slug or a positive App
/// id, and a receipt matches on exactly that field, as the Python arm guard
/// does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimApp {
    /// GitHub App slug.
    Slug(String),
    /// GitHub App id.
    Id(u64),
}

impl ClaimApp {
    /// Whether a check run's `app` object is this App. A run published under
    /// the shared GitHub Actions id never matches, whatever the policy says.
    fn matches(&self, run: &serde_json::Value) -> bool {
        let Some(app) = run.get("app").and_then(serde_json::Value::as_object) else {
            return false;
        };
        let id = app.get("id").and_then(serde_json::Value::as_u64);
        if id == Some(SHARED_ACTIONS_APP_ID) {
            return false;
        }
        match self {
            Self::Id(expected) => id == Some(*expected),
            Self::Slug(expected) => {
                app.get("slug").and_then(serde_json::Value::as_str) == Some(expected.as_str())
            }
        }
    }
}

impl fmt::Display for ClaimApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Slug(slug) => write!(f, "App `{slug}`"),
            Self::Id(id) => write!(f, "App id {id}"),
        }
    }
}

/// Read the receipt App from the protected base's `.shipyard/config.toml`.
///
/// `None` is a base without that file. Claim acquisition requires the policy
/// to be enabled and complete: both `[auto_merge]` fields present, the check
/// name equal to [`REVIEWER_CLAIM_CHECK_NAME`], and an App that is neither a
/// shared slug nor [`SHARED_ACTIONS_APP_ID`]. Anything else is an error, so a
/// receipt is never published under an authority the arm guard would refuse.
pub fn claim_app_from_base_config(config_text: Option<&str>) -> Result<ClaimApp, String> {
    let Some(text) = config_text else {
        return Err(format!(
            "the base has no .shipyard/config.toml, so it does not enable reviewer claim \
             receipts ([auto_merge] {CLAIM_RECEIPT_KEY} and {CLAIM_APP_KEY})"
        ));
    };
    let table = text
        .parse::<toml::Table>()
        .map_err(|error| format!("the base's .shipyard/config.toml does not parse: {error}"))?;
    let section = match table.get("auto_merge") {
        None => None,
        Some(toml::Value::Table(section)) => Some(section),
        Some(_) => {
            return Err("the base's .shipyard/config.toml [auto_merge] is not a table".to_owned());
        }
    };
    let receipt = section.and_then(|section| section.get(CLAIM_RECEIPT_KEY));
    let app = section.and_then(|section| section.get(CLAIM_APP_KEY));
    if receipt.is_none() && app.is_none() {
        return Err(format!(
            "the base does not enable reviewer claim receipts: [auto_merge] \
             {CLAIM_RECEIPT_KEY} and {CLAIM_APP_KEY} are unset"
        ));
    }
    match receipt {
        Some(toml::Value::String(name)) if name.trim() == REVIEWER_CLAIM_CHECK_NAME => {}
        Some(toml::Value::String(name)) if !name.trim().is_empty() => {
            return Err(format!(
                "the base's [auto_merge] {CLAIM_RECEIPT_KEY} names `{}`, but receipts are \
                 published as `{REVIEWER_CLAIM_CHECK_NAME}`",
                name.trim()
            ));
        }
        _ => {
            return Err(format!(
                "the base's [auto_merge] {CLAIM_RECEIPT_KEY} must be a non-empty check-run \
                 name when claim receipts are enabled"
            ));
        }
    }
    let app = match app {
        Some(toml::Value::String(slug)) if !slug.trim().is_empty() => {
            let slug = slug.trim();
            if SHARED_CLAIM_APP_SLUGS
                .iter()
                .any(|shared| shared.eq_ignore_ascii_case(slug))
            {
                return Err(format!(
                    "the base's [auto_merge] {CLAIM_APP_KEY} may not name the shared `{slug}` App"
                ));
            }
            ClaimApp::Slug(slug.to_owned())
        }
        Some(toml::Value::Integer(id)) if *id > 0 => {
            let id = u64::try_from(*id).map_err(|error| error.to_string())?;
            if id == SHARED_ACTIONS_APP_ID {
                return Err(format!(
                    "the base's [auto_merge] {CLAIM_APP_KEY} may not name shared GitHub \
                     Actions App id {SHARED_ACTIONS_APP_ID}"
                ));
            }
            ClaimApp::Id(id)
        }
        _ => {
            return Err(format!(
                "the base's [auto_merge] {CLAIM_APP_KEY} must be a non-empty app slug or \
                 positive app id when claim receipts are enabled"
            ));
        }
    };
    Ok(app)
}

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

/// One `gh` invocation returning JSON: the arguments and optional stdin.
type GhJsonRunner = Box<dyn FnMut(&[String], Option<&[u8]>) -> Result<serde_json::Value, String>>;

/// Authenticated GitHub App check-run transport.
///
/// This uses Shipyard's privileged, token-helper-backed `GhClient` command
/// boundary. It never falls back to ambient credentials or a direct merge API.
pub struct GhAppCheckRunTransport {
    runner: GhJsonRunner,
    repository: String,
    app: ClaimApp,
}

impl GhAppCheckRunTransport {
    /// Construct a transport bound to one canonical repository and the App
    /// the protected base names ([`claim_app_from_base_config`]).
    pub fn new(
        client: GhClient,
        cwd: impl AsRef<Path>,
        repository: &str,
        app: ClaimApp,
    ) -> Result<Self, GhPrepareError> {
        let client = client.with_repo_override(repository)?;
        let cwd = cwd.as_ref().to_path_buf();
        Ok(Self {
            runner: Box::new(move |args, input| run_gh_json(&client, &cwd, args, input)),
            repository: repository.to_owned(),
            app,
        })
    }

    /// A transport whose `gh` calls are answered by `runner`.
    #[cfg(test)]
    pub(crate) fn with_runner(
        repository: &str,
        app: ClaimApp,
        runner: impl FnMut(&[String], Option<&[u8]>) -> Result<serde_json::Value, String> + 'static,
    ) -> Self {
        Self {
            runner: Box::new(runner),
            repository: repository.to_owned(),
            app,
        }
    }

    fn run_json(
        &mut self,
        args: &[String],
        input: Option<&[u8]>,
    ) -> Result<serde_json::Value, String> {
        (self.runner)(args, input)
    }

    /// Mark a check run this transport just created as failed, so a receipt
    /// that did not validate certifies nothing. Returns the refusal to report.
    fn invalidate_created(&mut self, created: &serde_json::Value, refusal: &str) -> String {
        let Some(id) = created.get("id").and_then(serde_json::Value::as_u64) else {
            return format!(
                "{refusal}; the created check run carries no id, so it could not be marked failed"
            );
        };
        let body = serde_json::json!({
            "status": "completed",
            "conclusion": "failure",
            "output": {
                "title": "Reviewer claim receipt refused",
                "summary": "This receipt did not match the reviewer claim policy and certifies nothing.",
            }
        });
        let outcome = serde_json::to_vec(&body)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                let path = format!("repos/{}/check-runs/{id}", self.repository);
                self.run_json(&patch_check_run_args(path), Some(&bytes))
            });
        match outcome {
            Ok(_) => format!("{refusal}; created check run {id} was marked failed"),
            Err(error) => format!(
                "{refusal}; created check run {id} could not be marked failed ({error}) and \
                 must be marked failed by hand"
            ),
        }
    }
}

fn run_gh_json(
    client: &GhClient,
    cwd: &Path,
    args: &[String],
    input: Option<&[u8]>,
) -> Result<serde_json::Value, String> {
    let mut command = client
        .prepare_privileged_command(cwd, GhSupervision::Supervised)
        .map_err(|error| error.to_string())?;
    command.current_dir(cwd).args(args);
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
                return validate_run(run, request, &self.app).map(Some);
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
                "summary": serde_json::to_string(&serde_json::json!({
                    "schema": "shipyard-reviewer-claim/v1",
                    "repo": request.receipt.repository,
                    "pr": request.receipt.pr,
                    "head_sha": request.receipt.head_sha,
                    "reviewer": request.receipt.reviewer,
                    "epoch": request.receipt.epoch,
                    "claimed_at": request.receipt.claimed_at,
                    "deadline_at": request.receipt.deadline_at,
                })).map_err(|error| error.to_string())?
            }
        });
        let bytes = serde_json::to_vec(&body).map_err(|error| error.to_string())?;
        let path = format!("repos/{}/check-runs", self.repository);
        let value = self.run_json(&create_check_run_args(path), Some(&bytes))?;
        // A run that does not validate (wrong App, head or summary) is still a
        // successful check run on GitHub. Mark it failed before reporting, so
        // a refused acquire leaves no success receipt behind.
        validate_run(&value, request, &self.app)
            .map_err(|refusal| self.invalidate_created(&value, &refusal))
    }
}

fn patch_check_run_args(path: String) -> Vec<String> {
    vec![
        "api".to_owned(),
        path,
        "--method".to_owned(),
        "PATCH".to_owned(),
        "--input".to_owned(),
        "-".to_owned(),
    ]
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
    app: &ClaimApp,
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
    if !app.matches(run) {
        return Err(format!(
            "review receipt was published by an unexpected App (expected {app})"
        ));
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
    let mut value: serde_json::Value = serde_json::from_str(summary)
        .map_err(|error| format!("malformed reviewer receipt: {error}"))?;
    if value.get("schema").and_then(serde_json::Value::as_str) != Some("shipyard-reviewer-claim/v1")
    {
        return Err("reviewer receipt has an unexpected schema".to_owned());
    }
    let repo = value
        .get("repo")
        .cloned()
        .ok_or_else(|| "reviewer receipt has no repo".to_owned())?;
    let external_id = run
        .get("external_id")
        .cloned()
        .ok_or_else(|| "reviewer receipt has no external_id".to_owned())?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "reviewer receipt summary is not an object".to_owned())?;
    object.remove("schema");
    object.remove("repo");
    object.insert("repository".to_owned(), repo);
    object.insert("external_id".to_owned(), external_id);
    serde_json::from_value(value).map_err(|error| format!("malformed reviewer receipt: {error}"))
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
        name: REVIEWER_CLAIM_CHECK_NAME.to_owned(),
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

    fn summary(receipt: &ReviewerReceipt) -> String {
        serde_json::to_string(&serde_json::json!({
            "schema": "shipyard-reviewer-claim/v1",
            "repo": receipt.repository,
            "pr": receipt.pr,
            "head_sha": receipt.head_sha,
            "reviewer": receipt.reviewer,
            "epoch": receipt.epoch,
            "claimed_at": receipt.claimed_at,
            "deadline_at": receipt.deadline_at,
        }))
        .expect("receipt summary serializes")
    }
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
        assert!(matches!(
            publish_or_reconcile(&mut Unreadable, &request),
            Err(ReceiptError::Transport(_))
        ));
    }

    #[test]
    fn malformed_timestamp_and_receipt_id_are_rejected() {
        let request = request();
        let mut receipt: serde_json::Value =
            serde_json::from_str(&summary(&request.receipt)).expect("receipt serializes");
        receipt["claimed_at"] = serde_json::Value::String("not-a-timezone".to_owned());
        let run = serde_json::json!({
            "external_id": request.receipt.external_id,
            "output": {"summary": receipt.to_string()}
        });
        assert!(decode_receipt(&run).is_err());

        let mut receipt: serde_json::Value =
            serde_json::from_str(&summary(&request.receipt)).expect("receipt serializes");
        receipt["external_id"] = serde_json::Value::String("stale".to_owned());
        let run = serde_json::json!({
            "external_id": "stale",
            "output": {"summary": receipt.to_string()}
        });
        let decoded = decode_receipt(&run).expect("well-formed receipt");
        assert_ne!(decoded.external_id, request.receipt.external_id);
    }

    #[test]
    fn run_identity_rejects_wrong_app_head_or_external_id() {
        let request = request();
        let summary = summary(&request.receipt);
        let slug = ClaimApp::Slug("shipyard-review".to_owned());
        let base = || {
            serde_json::json!({
                "name": request.name,
                "external_id": request.receipt.external_id,
                "head_sha": request.receipt.head_sha,
                "app": {"slug": "shipyard-review", "id": 42},
                "status": "completed",
                "conclusion": "success",
                "output": {"summary": summary}
            })
        };
        assert_eq!(
            validate_run(&base(), &request, &slug),
            Ok(request.receipt.clone())
        );
        let mut wrong = base();
        wrong["app"]["slug"] = serde_json::Value::String("ambient-gh".to_owned());
        assert!(validate_run(&wrong, &request, &slug).is_err());
        let mut wrong = base();
        wrong["head_sha"] = serde_json::Value::String("b".repeat(40));
        assert!(validate_run(&wrong, &request, &slug).is_err());
        let mut wrong = base();
        wrong["external_id"] = serde_json::Value::String("stale".to_owned());
        assert!(validate_run(&wrong, &request, &slug).is_err());
        let mut wrong = base();
        wrong["status"] = serde_json::Value::String("in_progress".to_owned());
        assert!(validate_run(&wrong, &request, &slug).is_err());
        let mut wrong = base();
        wrong["conclusion"] = serde_json::Value::String("failure".to_owned());
        assert!(validate_run(&wrong, &request, &slug).is_err());
    }

    #[test]
    fn create_payload_is_stdin_only() {
        let args = create_check_run_args("repos/o/r/check-runs".to_owned());
        assert!(args.windows(2).any(|pair| pair == ["--input", "-"]));
        assert!(!args.iter().any(|arg| arg == "--body-file"));
    }

    #[test]
    fn id_policy_matches_the_app_id_and_never_the_shared_actions_id() {
        let request = request();
        let run = |slug: &str, id: u64| {
            serde_json::json!({
                "name": request.name,
                "external_id": request.receipt.external_id,
                "head_sha": request.receipt.head_sha,
                "app": {"slug": slug, "id": id},
                "status": "completed",
                "conclusion": "success",
                "output": {"summary": summary(&request.receipt)}
            })
        };
        let fleet = ClaimApp::Id(4242);
        assert_eq!(
            validate_run(&run("renamed", 4242), &request, &fleet),
            Ok(request.receipt.clone())
        );
        assert!(validate_run(&run("shipyard-review", 4243), &request, &fleet).is_err());
        // Even a policy that names the shared id directly matches nothing.
        let shared = ClaimApp::Id(SHARED_ACTIONS_APP_ID);
        assert!(
            validate_run(
                &run("github-actions", SHARED_ACTIONS_APP_ID),
                &request,
                &shared
            )
            .is_err()
        );
    }

    const ENABLED: &str = "[auto_merge]\nreviewer_claim_receipt = \"reviewer-claim\"\n";

    #[test]
    fn base_policy_names_the_receipt_app() {
        assert_eq!(
            claim_app_from_base_config(Some(&format!(
                "{ENABLED}reviewer_claim_app = \" shipyard-review \"\n"
            ))),
            Ok(ClaimApp::Slug("shipyard-review".to_owned()))
        );
        assert_eq!(
            claim_app_from_base_config(Some(&format!("{ENABLED}reviewer_claim_app = 4242\n"))),
            Ok(ClaimApp::Id(4242))
        );
    }

    #[test]
    fn base_policy_refuses_an_absent_disabled_or_partial_policy() {
        for config in [
            None,
            Some(""),
            Some("[auto_merge]\narm_requires_head_approval = true\n"),
            Some(ENABLED),
            Some("[auto_merge]\nreviewer_claim_app = 4242\n"),
            Some("[auto_merge]\nreviewer_claim_receipt = \"\"\nreviewer_claim_app = 4242\n"),
            Some("auto_merge = 3\n"),
            Some("not toml ["),
        ] {
            assert!(
                claim_app_from_base_config(config).is_err(),
                "{config:?} must refuse"
            );
        }
    }

    #[test]
    fn base_policy_refuses_a_check_name_receipts_are_not_published_under() {
        let config = "[auto_merge]\nreviewer_claim_receipt = \"shipyard-reviewer-claim\"\n\
                      reviewer_claim_app = 4242\n";
        let error = claim_app_from_base_config(Some(config)).expect_err("wrong name");
        assert!(error.contains("reviewer-claim"), "{error}");
    }

    #[test]
    fn base_policy_refuses_a_shared_app() {
        for app in [
            "15368",
            "\"github-actions\"",
            "\"GitHub-Actions\"",
            "\"shipyard-local\"",
            "0",
            "-5",
            "\"  \"",
            "true",
        ] {
            let config = format!("{ENABLED}reviewer_claim_app = {app}\n");
            assert!(
                claim_app_from_base_config(Some(&config)).is_err(),
                "{app} must refuse"
            );
        }
    }
}
