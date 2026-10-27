//! Crash-safe publication boundary for reviewer claim receipts.
//!
//! This module deliberately contains no `gh` command construction or raw HTTP.
//! The authenticated GitHub App adapter implements [`CheckRunTransport`].
//! Keeping that seam explicit makes the create/read/reconcile contract testable
//! without accidentally introducing an ambient-token or direct-merge path.

use std::fmt;

use crate::ship_state::ReviewerClaim;

/// Immutable identity and fence carried by one reviewer check run.
#[derive(Clone, Debug, Eq, PartialEq)]
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
pub fn request_from_claim(repository: &str, pr: u64, claim: &ReviewerClaim) -> ReceiptRequest {
    ReceiptRequest {
        name: "reviewer-claim".to_owned(),
        receipt: ReviewerReceipt {
            external_id: claim.receipt_id(repository, pr),
            repository: repository.to_owned(),
            pr,
            head_sha: claim.head_sha.clone(),
            reviewer: claim.reviewer.clone(),
            epoch: claim.epoch,
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
}
