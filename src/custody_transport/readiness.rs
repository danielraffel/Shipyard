//! Typed owner-attested prerequisite receipt schema for custody setup.

use serde::{Deserialize, Serialize};

pub(super) const SCHEMA_VERSION: u32 = 1;
pub(super) const MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub(super) schema_version: u32,
    pub(super) kind: String,
    pub(super) machine_ref: String,
    pub(super) incarnation_ref: String,
    pub(super) route_ref: String,
    pub(super) authority_digest: String,
    pub(super) destination_bootstrap_digest: String,
    pub(super) profile_digest: String,
    pub(super) native_publication_digest: String,
    pub(super) payload_digest: String,
}
