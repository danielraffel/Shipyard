//! Restart-reconcilable custody for protected parallel-proof canary jobs.
//!
//! This module deliberately does not accept a command line or shell text. An
//! authenticated controller submits one typed operation, persists intent before
//! launch, and delegates process mechanics to an adapter. After a controller or
//! agent restart, reconciliation observes the original launch nonce and process
//! identity; it never redispatches the operation. A missing process is a terminal
//! loss, never inferred success.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::immutable_store::{ImmutableByteStore, ImmutableStoreError};
use crate::parallel_proof::{ParallelProofError, Sha256Digest, StoreWriteOutcome};
#[cfg(test)]
use crate::parallel_proof_canary_driver::ArtifactDeliveryObservation;
use crate::parallel_proof_canary_driver::DistributedExecutionObservation;
use crate::parallel_proof_canary_receipt::ArtifactDeliveryMode;

const CURRENT_JOB_SCHEMA_VERSION: u32 = 2;
const LEGACY_JOB_SCHEMA_VERSION: u32 = 1;
const RECEIPT_SCHEMA_VERSION: u32 = 1;
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ID_BYTES: usize = 160;
const MAX_HEARTBEATS: u32 = 128;
const MAX_LOG_SEGMENTS: u32 = 32;
const MAX_LOG_SEGMENT_BYTES: u32 = 256 * 1024;
include!("parallel_proof_canary_job/model.rs");
include!("parallel_proof_canary_job/store.rs");
include!("parallel_proof_canary_job/lifecycle.rs");
include!("parallel_proof_canary_job/tests.rs");
