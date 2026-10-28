//! Authenticated transport envelope for merge receipts.
//!
//! This is deliberately separate from [`super::merge_receipt`].  The v1
//! receipt is a useful, producer-self-asserted compatibility format; it does
//! not authenticate where its bytes came from.  This envelope binds the
//! canonical v1 payload to a trusted producer fence supplied by the
//! authenticated transport.  The fields carried by the envelope are claims
//! until [`AuthenticatedHandoffEnvelope::verify`] compares them with that
//! trusted context.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{BufRead, Read};
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::parallel_proof::{MAX_IDENTIFIER_BYTES, Sha256Digest};
use crate::parallel_proof_canary::CanaryRoute;

/// Current authenticated merge-receipt handoff schema.
pub const SCHEMA: &str = "shipyard-merge-receipt-handoff-auth/v1";

/// Maximum time an envelope may remain valid.
pub const DEFAULT_MAX_AGE: Duration = Duration::minutes(15);

/// Maximum amount of producer clock skew accepted by a controller.
pub const DEFAULT_MAX_FUTURE_SKEW: Duration = Duration::minutes(2);

/// Maximum encoded envelope size accepted by the bounded parser.
pub const MAX_ENVELOPE_BYTES: usize = 16 * 1024 * 1024;

/// Maximum receipt count accepted in one envelope.
pub const MAX_RECEIPTS: usize = 16_384;

/// Maximum policy age accepted from an authenticated importer.
pub const MAX_ALLOWED_AGE: Duration = Duration::hours(1);

/// Maximum future-skew policy accepted from an authenticated importer.
pub const MAX_ALLOWED_FUTURE_SKEW: Duration = Duration::minutes(5);

/// Domain separator for receipt-payload digests.
const PAYLOAD_DIGEST_DOMAIN: &str = "shipyard.merge-receipt-handoff.payload.v1";

/// A canonical v1 receipt batch plus the producer claims needed to bind it to
/// an authenticated transport session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedHandoffEnvelope {
    /// Envelope schema identifier.
    pub schema: String,
    /// Unique immutable identity used for replay detection.
    pub envelope_id: String,
    /// Repository all payload receipts must belong to.
    pub repository: String,
    /// Project key all payload receipts must belong to.
    pub project: String,
    /// Protected base branch all payload receipts must belong to.
    pub base_branch: String,
    /// Canonically sorted v1 receipts.
    pub receipts: Vec<super::merge_receipt::MergeReceipt>,
    /// Domain-separated SHA-256 over the canonical JSON bytes of `receipts`.
    pub payload_sha256: Sha256Digest,
    /// Stable producer host identity from the authenticated transport.
    pub producer_host_id: String,
    /// Reconnect-fenced producer session generation.
    pub producer_session_generation: u64,
    /// Exact route selected by the authenticated transport.
    pub route: CanaryRoute,
    /// Exact destination selected for `route`.
    pub destination: String,
    /// Digest of the locked known-host authority used by the transport.
    pub known_hosts_sha256: Sha256Digest,
    /// Digest of the pinned producer executable.
    pub executable_sha256: Sha256Digest,
    /// Controller-observed issue time.
    pub issued_at: DateTime<Utc>,
    /// Hard expiry of this envelope.
    pub expires_at: DateTime<Utc>,
}

/// Trusted producer fence established outside the envelope by the
/// authenticated transport.  Envelope fields alone are never proof.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Wired by the authenticated importer slice.
pub(crate) struct AuthenticatedProducerContext {
    pub repository: String,
    pub project: String,
    pub base_branch: String,
    pub producer_host_id: String,
    pub producer_session_generation: u64,
    pub route: CanaryRoute,
    pub destination: String,
    pub known_hosts_sha256: Sha256Digest,
    pub executable_sha256: Sha256Digest,
    pub now: DateTime<Utc>,
    pub max_age: Duration,
    pub max_future_skew: Duration,
}

/// A validated envelope that is eligible for a later authenticated importer.
/// No database writes or replay-set mutations happen during verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedAuthenticatedHandoff<'a> {
    pub envelope_id: &'a str,
    pub repository: &'a str,
    pub receipts: &'a [super::merge_receipt::MergeReceipt],
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Fail-closed envelope validation or trusted-fence mismatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HandoffError {
    Invalid(&'static str),
    NonCanonical(&'static str),
    Duplicate(&'static str),
    RepositoryMismatch,
    PayloadDigestMismatch,
    AuthorityMismatch(&'static str),
    Stale,
    Future,
    Replayed(&'static str),
    Serialization,
}

impl fmt::Display for HandoffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(field) => write!(formatter, "invalid handoff {field}"),
            Self::NonCanonical(field) => write!(formatter, "non-canonical handoff {field}"),
            Self::Duplicate(field) => write!(formatter, "duplicate handoff {field}"),
            Self::RepositoryMismatch => formatter.write_str("handoff repository mismatch"),
            Self::PayloadDigestMismatch => formatter.write_str("handoff payload digest mismatch"),
            Self::AuthorityMismatch(field) => {
                write!(formatter, "handoff authority mismatch: {field}")
            }
            Self::Stale => formatter.write_str("handoff is stale or expired"),
            Self::Future => formatter.write_str("handoff is issued too far in the future"),
            Self::Replayed(field) => write!(formatter, "handoff replayed {field}"),
            Self::Serialization => formatter.write_str("handoff payload serialization failed"),
        }
    }
}

impl std::error::Error for HandoffError {}

impl AuthenticatedHandoffEnvelope {
    /// Validate intrinsic shape, canonical ordering, and payload integrity.
    pub fn validate(&self) -> Result<(), HandoffError> {
        if self.schema != SCHEMA {
            return Err(HandoffError::Invalid("schema"));
        }
        for (field, value) in [
            ("envelope_id", &self.envelope_id),
            ("repository", &self.repository),
            ("project", &self.project),
            ("base_branch", &self.base_branch),
            ("producer_host_id", &self.producer_host_id),
            ("destination", &self.destination),
        ] {
            if !valid_identifier(value) {
                return Err(HandoffError::Invalid(field));
            }
        }
        if !valid_repository(&self.repository) {
            return Err(HandoffError::Invalid("repository"));
        }
        if self.producer_session_generation == 0
            || matches!(self.route, CanaryRoute::Unavailable)
            || self.expires_at <= self.issued_at
            || self.receipts.is_empty()
            || self.receipts.len() > MAX_RECEIPTS
            || is_zero_digest(&self.payload_sha256)
            || is_zero_digest(&self.known_hosts_sha256)
            || is_zero_digest(&self.executable_sha256)
        {
            return Err(HandoffError::Invalid("envelope fields"));
        }
        let mut previous: Option<&str> = None;
        let mut receipt_ids = BTreeSet::new();
        for receipt in &self.receipts {
            receipt
                .validate()
                .map_err(|_| HandoffError::Invalid("receipt"))?;
            if receipt.repo != self.repository {
                return Err(HandoffError::RepositoryMismatch);
            }
            if receipt.project != self.project {
                return Err(HandoffError::AuthorityMismatch("project"));
            }
            if receipt.base_branch != self.base_branch {
                return Err(HandoffError::AuthorityMismatch("base_branch"));
            }
            if !receipt_ids.insert(receipt.receipt_id.clone()) {
                return Err(HandoffError::Duplicate("receipt_id"));
            }
            if previous.is_some_and(|id| id >= receipt.receipt_id.as_str()) {
                return Err(HandoffError::NonCanonical("receipts"));
            }
            previous = Some(&receipt.receipt_id);
        }
        if digest_receipts(&self.receipts)? != self.payload_sha256 {
            return Err(HandoffError::PayloadDigestMismatch);
        }
        Ok(())
    }

    /// Compare claims against an authenticated producer fence and replay
    /// state. Call this before opening the database write transaction.
    ///
    /// `seen_receipt_digests` is keyed by durable receipt ID. An identical
    /// receipt is idempotent so a periodic full export can be reprocessed and
    /// can attach authenticated provenance to an existing legacy row. A
    /// changed payload under an existing ID is rejected as a conflict.
    #[allow(dead_code)] // Wired by the authenticated importer slice.
    pub(crate) fn verify<'a>(
        &'a self,
        context: &AuthenticatedProducerContext,
        seen_envelope_ids: &BTreeSet<String>,
        seen_receipt_digests: &BTreeMap<String, Sha256Digest>,
    ) -> Result<VerifiedAuthenticatedHandoff<'a>, HandoffError> {
        self.validate()?;
        if seen_envelope_ids.contains(&self.envelope_id) {
            return Err(HandoffError::Replayed("envelope_id"));
        }
        for receipt in &self.receipts {
            if let Some(existing) = seen_receipt_digests.get(&receipt.receipt_id)
                && *existing != digest_receipt(receipt)?
            {
                return Err(HandoffError::Replayed("receipt_id_conflict"));
            }
        }
        if self.repository != context.repository {
            return Err(HandoffError::RepositoryMismatch);
        }
        if self.project != context.project {
            return Err(HandoffError::AuthorityMismatch("project"));
        }
        if self.base_branch != context.base_branch {
            return Err(HandoffError::AuthorityMismatch("base_branch"));
        }
        for (field, equal) in [
            (
                "producer_host_id",
                self.producer_host_id == context.producer_host_id,
            ),
            (
                "producer_session_generation",
                self.producer_session_generation == context.producer_session_generation,
            ),
            ("route", self.route == context.route),
            ("destination", self.destination == context.destination),
            (
                "known_hosts_sha256",
                self.known_hosts_sha256 == context.known_hosts_sha256,
            ),
            (
                "executable_sha256",
                self.executable_sha256 == context.executable_sha256,
            ),
        ] {
            if !equal {
                return Err(HandoffError::AuthorityMismatch(field));
            }
        }
        if context.max_age <= Duration::zero()
            || context.max_age > MAX_ALLOWED_AGE
            || context.max_future_skew < Duration::zero()
            || context.max_future_skew > MAX_ALLOWED_FUTURE_SKEW
        {
            return Err(HandoffError::Invalid("freshness policy"));
        }
        let future_limit = context
            .now
            .checked_add_signed(context.max_future_skew)
            .ok_or(HandoffError::Invalid("freshness policy"))?;
        if self.issued_at > future_limit {
            return Err(HandoffError::Future);
        }
        if self.expires_at <= context.now
            || context.now.signed_duration_since(self.issued_at) > context.max_age
            || self.expires_at.signed_duration_since(self.issued_at) > context.max_age
        {
            return Err(HandoffError::Stale);
        }
        Ok(VerifiedAuthenticatedHandoff {
            envelope_id: &self.envelope_id,
            repository: &self.repository,
            receipts: &self.receipts,
            issued_at: self.issued_at,
            expires_at: self.expires_at,
        })
    }
}

/// Compute the domain-separated digest that an envelope must carry for its
/// canonical receipt vector. Receipt ordering is checked by `validate`; this
/// function preserves the exact bytes that will later be imported.
pub fn digest_receipts(
    receipts: &[super::merge_receipt::MergeReceipt],
) -> Result<Sha256Digest, HandoffError> {
    let bytes = serde_json::to_vec(receipts).map_err(|_| HandoffError::Serialization)?;
    Ok(domain_digest(PAYLOAD_DIGEST_DOMAIN, &bytes))
}

pub(crate) fn digest_receipt(
    receipt: &super::merge_receipt::MergeReceipt,
) -> Result<Sha256Digest, HandoffError> {
    let bytes = serde_json::to_vec(receipt).map_err(|_| HandoffError::Serialization)?;
    Ok(domain_digest(PAYLOAD_DIGEST_DOMAIN, &bytes))
}

/// Parse one bounded JSON envelope and validate it before returning it.
pub fn parse_reader(
    mut reader: impl BufRead,
) -> Result<AuthenticatedHandoffEnvelope, HandoffError> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take((MAX_ENVELOPE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| HandoffError::Serialization)?;
    if bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(HandoffError::Invalid("encoded size"));
    }
    let envelope: AuthenticatedHandoffEnvelope =
        serde_json::from_slice(&bytes).map_err(|_| HandoffError::Serialization)?;
    envelope.validate()?;
    Ok(envelope)
}

/// Parse and validate a bounded JSON envelope from a file or stdin (`-`).
pub fn parse_path(path: Option<&Path>) -> Result<AuthenticatedHandoffEnvelope, HandoffError> {
    let stdin = std::io::stdin();
    if path.is_none_or(|path| path == Path::new("-")) {
        return parse_reader(stdin.lock());
    }
    let path = path.ok_or(HandoffError::Invalid("path"))?;
    let file = std::fs::File::open(path).map_err(|_| HandoffError::Serialization)?;
    parse_reader(std::io::BufReader::new(file))
}

fn domain_digest(domain: &str, payload: &[u8]) -> Sha256Digest {
    let mut bytes = Vec::with_capacity(16 + domain.len() + payload.len());
    bytes.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    bytes.extend_from_slice(domain.as_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    bytes.extend_from_slice(payload);
    Sha256Digest::of_bytes(&bytes)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'/' | b':' | b'@')
        })
}

fn valid_repository(value: &str) -> bool {
    // Repository identity is canonicalized before authentication. Legacy v1
    // imports still normalize case at their storage boundary; authenticated
    // envelopes reject mixed-case spellings so the digest and fence agree.
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return false;
    }
    let mut parts = value.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(repo) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !owner.is_empty()
        && !repo.is_empty()
        && [owner, repo].into_iter().all(|part| {
            part.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        })
}

fn is_zero_digest(value: &Sha256Digest) -> bool {
    value.as_str().bytes().all(|byte| byte == b'0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parallel_proof::Sha256Digest;
    use crate::parallel_proof_canary::CanaryRoute;

    fn receipt(id: &str, repo: &str) -> super::super::merge_receipt::MergeReceipt {
        super::super::merge_receipt::MergeReceipt {
            schema: super::super::merge_receipt::SCHEMA.to_owned(),
            receipt_id: id.to_owned(),
            repo: repo.to_owned(),
            project: "repo".to_owned(),
            base_branch: "main".to_owned(),
            pr: 1,
            head_sha: "a".repeat(40),
            validated_head_sha: "a".repeat(40),
            merge_commit_sha: "b".repeat(40),
            merged_at: "2026-10-28T00:00:00Z".parse().unwrap(),
            safe_merge: true,
            source: "shipyard".to_owned(),
            required_checks: vec!["test".to_owned()],
            gate_receipt_sha256: "c".repeat(64),
            evidence: "sha256:test".to_owned(),
        }
    }

    fn envelope() -> AuthenticatedHandoffEnvelope {
        let repository = "owner/repo";
        let receipts = vec![receipt("r-1", repository), receipt("r-2", repository)];
        AuthenticatedHandoffEnvelope {
            schema: SCHEMA.to_owned(),
            envelope_id: "env-1".to_owned(),
            repository: repository.to_owned(),
            project: "repo".to_owned(),
            base_branch: "main".to_owned(),
            payload_sha256: digest_receipts(&receipts).unwrap(),
            receipts,
            producer_host_id: "m1".to_owned(),
            producer_session_generation: 7,
            route: CanaryRoute::Lan,
            destination: "shipyard@m1.local".to_owned(),
            known_hosts_sha256: Sha256Digest::of_bytes(b"known-hosts"),
            executable_sha256: Sha256Digest::of_bytes(b"shipyard"),
            issued_at: "2026-10-28T00:00:00Z".parse().unwrap(),
            expires_at: "2026-10-28T00:10:00Z".parse().unwrap(),
        }
    }

    fn context() -> AuthenticatedProducerContext {
        AuthenticatedProducerContext {
            repository: "owner/repo".to_owned(),
            project: "repo".to_owned(),
            base_branch: "main".to_owned(),
            producer_host_id: "m1".to_owned(),
            producer_session_generation: 7,
            route: CanaryRoute::Lan,
            destination: "shipyard@m1.local".to_owned(),
            known_hosts_sha256: Sha256Digest::of_bytes(b"known-hosts"),
            executable_sha256: Sha256Digest::of_bytes(b"shipyard"),
            now: "2026-10-28T00:05:00Z".parse().unwrap(),
            max_age: Duration::minutes(15),
            max_future_skew: Duration::minutes(2),
        }
    }

    #[test]
    fn validates_and_verifies_against_trusted_fence() {
        let envelope = envelope();
        let seen_envelopes = BTreeSet::new();
        let seen_receipts = BTreeMap::new();
        let verified = envelope
            .verify(&context(), &seen_envelopes, &seen_receipts)
            .unwrap();
        assert_eq!(verified.envelope_id, "env-1");
        assert_eq!(verified.receipts.len(), 2);
    }

    #[test]
    fn rejects_tamper_mixed_repo_duplicates_and_noncanonical_order() {
        let mut tampered = envelope();
        tampered.payload_sha256 = Sha256Digest::of_bytes(b"tampered");
        assert_eq!(
            tampered.validate(),
            Err(HandoffError::PayloadDigestMismatch)
        );

        let mut mixed = envelope();
        mixed.receipts[1].repo = "other/repo".to_owned();
        assert_eq!(mixed.validate(), Err(HandoffError::RepositoryMismatch));

        let mut duplicate = envelope();
        duplicate.receipts[1].receipt_id = "r-1".to_owned();
        assert_eq!(
            duplicate.validate(),
            Err(HandoffError::Duplicate("receipt_id"))
        );

        let mut reversed = envelope();
        reversed.receipts.reverse();
        assert_eq!(
            reversed.validate(),
            Err(HandoffError::NonCanonical("receipts"))
        );
    }

    #[test]
    fn rejects_replay_authority_drift_and_freshness_failures() {
        let mut seen_envelopes = BTreeSet::new();
        seen_envelopes.insert("env-1".to_owned());
        assert_eq!(
            envelope().verify(&context(), &seen_envelopes, &BTreeMap::new()),
            Err(HandoffError::Replayed("envelope_id"))
        );

        let mut seen_receipts = BTreeMap::new();
        seen_receipts.insert(
            "r-1".to_owned(),
            digest_receipt(&envelope().receipts[0]).unwrap(),
        );
        assert!(
            envelope()
                .verify(&context(), &BTreeSet::new(), &seen_receipts)
                .is_ok()
        );

        let mut conflict = BTreeMap::new();
        conflict.insert("r-1".to_owned(), Sha256Digest::of_bytes(b"different"));
        assert_eq!(
            envelope().verify(&context(), &BTreeSet::new(), &conflict),
            Err(HandoffError::Replayed("receipt_id_conflict"))
        );

        let mut zero_age = context();
        zero_age.max_age = Duration::zero();
        assert_eq!(
            envelope().verify(&zero_age, &BTreeSet::new(), &BTreeMap::new()),
            Err(HandoffError::Invalid("freshness policy"))
        );

        let mut drifted = context();
        drifted.producer_session_generation = 8;
        assert_eq!(
            envelope().verify(&drifted, &BTreeSet::new(), &BTreeMap::new()),
            Err(HandoffError::AuthorityMismatch(
                "producer_session_generation"
            ))
        );

        let mut future = context();
        future.now = "2026-10-27T23:00:00Z".parse().unwrap();
        assert_eq!(
            envelope().verify(&future, &BTreeSet::new(), &BTreeMap::new()),
            Err(HandoffError::Future)
        );

        let mut stale = context();
        stale.now = "2026-10-28T01:00:00Z".parse().unwrap();
        assert_eq!(
            envelope().verify(&stale, &BTreeSet::new(), &BTreeMap::new()),
            Err(HandoffError::Stale)
        );
    }

    #[test]
    fn bounded_parser_rejects_unknown_fields_and_large_input() {
        let encoded = serde_json::to_vec(&envelope()).unwrap();
        let parsed = parse_reader(std::io::Cursor::new(encoded.clone())).unwrap();
        assert_eq!(parsed, envelope());

        let mut unknown = serde_json::to_value(envelope()).unwrap();
        unknown["unexpected"] = serde_json::json!(true);
        assert_eq!(
            parse_reader(std::io::Cursor::new(serde_json::to_vec(&unknown).unwrap())),
            Err(HandoffError::Serialization)
        );

        let oversized = vec![b'x'; MAX_ENVELOPE_BYTES + 1];
        assert_eq!(
            parse_reader(std::io::Cursor::new(oversized)),
            Err(HandoffError::Invalid("encoded size"))
        );
    }

    #[test]
    fn binds_project_and_base_branch() {
        let mut changed_project = envelope();
        changed_project.receipts[0].project = "other".to_owned();
        assert_eq!(
            changed_project.validate(),
            Err(HandoffError::AuthorityMismatch("project"))
        );

        let mut changed_branch = envelope();
        changed_branch.receipts[0].base_branch = "release".to_owned();
        assert_eq!(
            changed_branch.validate(),
            Err(HandoffError::AuthorityMismatch("base_branch"))
        );

        let mut mixed_case = envelope();
        mixed_case.repository = "OWNER/REPO".to_owned();
        for receipt in &mut mixed_case.receipts {
            receipt.repo = mixed_case.repository.clone();
        }
        mixed_case.payload_sha256 = digest_receipts(&mixed_case.receipts).unwrap();
        assert_eq!(
            mixed_case.validate(),
            Err(HandoffError::Invalid("repository"))
        );
    }
}
