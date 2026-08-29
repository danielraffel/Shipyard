//! Stable transition identities, validation, and diagnostic redaction.

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{
    MAX_NOTE_BYTES, MAX_REASON_BYTES, ProjectedTransition, ProjectionError, ProjectionEvidence,
    SCHEMA_VERSION, TransitionDraft, TransitionKind,
};

impl ProjectionEvidence {
    fn validate(&self) -> Result<(), ProjectionError> {
        validate_hex_identity(&self.source_revision, "source_revision", &[40, 64])?;
        if let Some(head) = &self.exact_head {
            validate_hex_identity(head, "exact_head", &[40, 64])?;
        }
        validate_hex_identity(&self.receipt_sha256, "receipt_sha256", &[64])
    }

    /// Stable digest of the exact evidence tuple.
    #[must_use]
    pub fn identity(&self) -> String {
        canonical_digest(self)
    }
}

impl TransitionDraft {
    /// Validate, redact, and derive stable transition/evidence identities.
    pub fn seal(self) -> Result<ProjectedTransition, ProjectionError> {
        validate_workstream_id(&self.workstream_id)?;
        if self.sequence == 0 {
            return Err(ProjectionError::Invalid(
                "transition sequence must be positive".to_owned(),
            ));
        }
        self.evidence.validate()?;
        if let Some(id) = &self.supersedes_transition_id {
            validate_hex_identity(id, "supersedes_transition_id", &[64])?;
        }
        let note = self.note.map(|value| redact_note(&value));
        let evidence_identity = self.evidence.identity();
        let transition_id = canonical_digest(&TransitionIdentity {
            schema_version: SCHEMA_VERSION,
            workstream_id: &self.workstream_id,
            sequence: self.sequence,
            kind: self.kind,
            evidence_identity: &evidence_identity,
            supersedes_transition_id: &self.supersedes_transition_id,
            note: &note,
        });
        Ok(ProjectedTransition {
            schema_version: SCHEMA_VERSION,
            transition_id,
            workstream_id: self.workstream_id,
            sequence: self.sequence,
            kind: self.kind,
            evidence: self.evidence,
            evidence_identity,
            supersedes_transition_id: self.supersedes_transition_id,
            note,
        })
    }
}

#[derive(Serialize)]
struct TransitionIdentity<'a> {
    schema_version: u32,
    workstream_id: &'a str,
    sequence: u64,
    kind: TransitionKind,
    evidence_identity: &'a str,
    supersedes_transition_id: &'a Option<String>,
    note: &'a Option<String>,
}

impl ProjectedTransition {
    pub(super) fn validate_identity(&self) -> Result<(), ProjectionError> {
        validate_workstream_id(&self.workstream_id)?;
        if self.schema_version != SCHEMA_VERSION || self.sequence == 0 {
            return Err(ProjectionError::Corrupt(
                "queued transition schema or sequence is invalid".to_owned(),
            ));
        }
        self.evidence.validate()?;
        if self
            .note
            .as_ref()
            .is_some_and(|note| note.len() > MAX_NOTE_BYTES)
        {
            return Err(ProjectionError::Corrupt(
                "queued transition note exceeds its bound".to_owned(),
            ));
        }
        if let Some(id) = &self.supersedes_transition_id {
            validate_hex_identity(id, "supersedes_transition_id", &[64])?;
        }
        if self.evidence.identity() != self.evidence_identity {
            return Err(ProjectionError::Corrupt(
                "queued transition evidence identity is invalid".to_owned(),
            ));
        }
        let expected = canonical_digest(&TransitionIdentity {
            schema_version: self.schema_version,
            workstream_id: &self.workstream_id,
            sequence: self.sequence,
            kind: self.kind,
            evidence_identity: &self.evidence_identity,
            supersedes_transition_id: &self.supersedes_transition_id,
            note: &self.note,
        });
        if expected != self.transition_id {
            return Err(ProjectionError::Corrupt(
                "queued transition identity is invalid".to_owned(),
            ));
        }
        Ok(())
    }
}

fn validate_workstream_id(value: &str) -> Result<(), ProjectionError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(ProjectionError::Invalid(
            "workstream_id must be a bounded non-secret stable handle".to_owned(),
        ));
    }
    Ok(())
}

fn validate_hex_identity(
    value: &str,
    field: &str,
    lengths: &[usize],
) -> Result<(), ProjectionError> {
    if !lengths.contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ProjectionError::Invalid(format!(
            "{field} must be a lowercase hexadecimal identity of an allowed length"
        )));
    }
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(ProjectionError::Invalid(format!(
            "{field} must use canonical lowercase hexadecimal"
        )));
    }
    Ok(())
}

fn redact_note(value: &str) -> String {
    let bounded = truncate_utf8(value, MAX_NOTE_BYTES);
    let mut redact_next = false;
    let mut redacted = Vec::new();
    for word in bounded.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        let secret_shape = word.starts_with("ghp_")
            || word.starts_with("github_pat_")
            || lower.starts_with("token=")
            || lower.starts_with("password=")
            || lower.starts_with("authorization:")
            || lower.starts_with("private_key=")
            || lower.starts_with("secret=")
            || lower == "bearer";
        if redact_next || secret_shape {
            redacted.push("[REDACTED]");
        } else {
            redacted.push(word);
        }
        redact_next = matches!(lower.as_str(), "bearer" | "authorization:");
    }
    redacted.join(" ")
}

pub(super) fn redact_reason(value: &str) -> String {
    truncate_utf8(&redact_note(value), MAX_REASON_BYTES)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn canonical_digest(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("canonical identity structures are serializable");
    digest_bytes(&bytes)
}

pub(super) fn digest_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
