//! Private `ci.outage/v1` records.
//!
//! Outage membership is deliberately keyed by the immutable GitHub job
//! identity `(repo, run, attempt, job)`.  Display names, branches, and times
//! are useful evidence, but are not joins.  Rows written before this contract
//! existed remain readable as `Unavailable::Legacy`; they must not be joined
//! by guessing from their old aggregate fields.

use std::collections::BTreeSet;

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::record_identity::{canonical_repository_slug, is_valid_repository_slug};

/// Private outage-window schema identifier.
pub(crate) const SCHEMA: &str = "ci.outage/v1";

/// A stable member identity.  The separators are unambiguous because a
/// canonical repository slug cannot contain `|`.
pub(crate) fn member_key(repo: &str, run: u64, attempt: u64, job: u64) -> Option<String> {
    let repo = canonical_repository_slug(repo);
    is_valid_repository_slug(&repo).then(|| format!("{repo}|{run}|{attempt}|{job}"))
}

/// Provenance of the private collector output.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Provenance {
    pub(crate) collector: String,
    pub(crate) collector_version: String,
    pub(crate) collected_at: String,
    pub(crate) source: String,
}

/// One validated outage window.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct OutageRecord {
    pub(crate) schema: String,
    pub(crate) id: String,
    pub(crate) member_keys: Vec<String>,
    #[serde(default)]
    pub(crate) owner_keys: Vec<String>,
    pub(crate) started_at: String,
    pub(crate) ended_at: String,
    #[serde(default)]
    pub(crate) signatures: Vec<String>,
    #[serde(default)]
    pub(crate) classes: Vec<String>,
    pub(crate) provenance: Provenance,
}

/// Why an archived row cannot participate in a stable outage join.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UnavailableReason {
    /// A pre-membership row has no immutable member keys.
    LegacyMissingMemberKeys,
    /// The row claims a different schema.
    UnsupportedSchema,
    /// The row is malformed or fails a v1 invariant.
    Invalid(String),
}

/// A row read from the private archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Availability {
    Available(OutageRecord),
    Unavailable(UnavailableReason),
}

/// Parse and validate one JSON row.  Legacy rows are intentionally retained as
/// unavailable rather than upgraded from mutable aggregate fields.
pub(crate) fn parse(value: Value) -> Availability {
    let Some(object) = value.as_object() else {
        return Availability::Unavailable(UnavailableReason::Invalid(
            "row is not an object".to_owned(),
        ));
    };
    if !object.contains_key("member_keys") {
        return Availability::Unavailable(UnavailableReason::LegacyMissingMemberKeys);
    }
    if object.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
        return Availability::Unavailable(UnavailableReason::UnsupportedSchema);
    }
    let record = match serde_json::from_value::<OutageRecord>(value) {
        Ok(record) => record,
        Err(error) => {
            return Availability::Unavailable(UnavailableReason::Invalid(error.to_string()));
        }
    };
    if let Err(error) = validate(&record) {
        return Availability::Unavailable(UnavailableReason::Invalid(error));
    }
    Availability::Available(record)
}

/// Validate v1 invariants without changing the record.
pub(crate) fn validate(record: &OutageRecord) -> Result<(), String> {
    if record.schema != SCHEMA {
        return Err("schema must be ci.outage/v1".to_owned());
    }
    if record.id.trim().is_empty() {
        return Err("id is empty".to_owned());
    }
    if record.member_keys.is_empty() {
        return Err("member_keys must not be empty".to_owned());
    }
    let mut keys = BTreeSet::new();
    let mut previous = None;
    for key in &record.member_keys {
        parse_member_key(key).ok_or_else(|| format!("invalid member_key: {key}"))?;
        if !keys.insert(key) {
            return Err("member_keys must be unique".to_owned());
        }
        if previous.is_some_and(|previous: &str| previous >= key.as_str()) {
            return Err("member_keys must be sorted".to_owned());
        }
        previous = Some(key.as_str());
    }
    if DateTime::parse_from_rfc3339(&record.started_at).is_err()
        || DateTime::parse_from_rfc3339(&record.ended_at).is_err()
    {
        return Err("started_at and ended_at must be RFC3339".to_owned());
    }
    if record.provenance.collector.trim().is_empty()
        || record.provenance.collector_version.trim().is_empty()
        || record.provenance.source.trim().is_empty()
        || DateTime::parse_from_rfc3339(&record.provenance.collected_at).is_err()
    {
        return Err("provenance is incomplete".to_owned());
    }
    Ok(())
}

fn parse_member_key(value: &str) -> Option<(&str, u64, u64, u64)> {
    let mut parts = value.split('|');
    let repo = parts.next()?;
    let run = parts.next()?.parse().ok()?;
    let attempt = parts.next()?.parse().ok()?;
    let job = parts.next()?.parse().ok()?;
    if parts.next().is_some() || run == 0 || attempt == 0 || job == 0 {
        return None;
    }
    (repo == canonical_repository_slug(repo) && is_valid_repository_slug(repo))
        .then_some((repo, run, attempt, job))
}

/// Whether two available windows have a stable member in common.
pub(crate) fn joins(left: &OutageRecord, right: &OutageRecord) -> bool {
    left.member_keys
        .iter()
        .any(|key| right.member_keys.contains(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(keys: Vec<String>) -> Value {
        json!({
            "schema": SCHEMA,
            "id": "outage:abc",
            "member_keys": keys,
            "started_at": "2026-10-15T00:00:00Z",
            "ended_at": "2026-10-15T00:30:00Z",
            "provenance": {
                "collector": "shipyard outage recorder",
                "collector_version": "0.335.0",
                "collected_at": "2026-10-15T01:00:00Z",
                "source": "github-actions"
            }
        })
    }

    #[test]
    fn member_key_is_canonical_and_joins_only_on_exact_job_identity() {
        let key = member_key(" Owner/Repo ", 42, 2, 7).expect("key");
        assert_eq!(key, "owner/repo|42|2|7");
        let a = match parse(row(vec![key])) {
            Availability::Available(row) => row,
            other => panic!("{other:?}"),
        };
        let b = match parse(row(vec!["owner/repo|42|2|8".to_owned()])) {
            Availability::Available(row) => row,
            other => panic!("{other:?}"),
        };
        assert!(!joins(&a, &b));
    }

    #[test]
    fn legacy_rows_remain_unavailable() {
        let result =
            parse(json!({"schema": SCHEMA, "id": "old", "started_at": "2026-10-15T00:00:00Z"}));
        assert_eq!(
            result,
            Availability::Unavailable(UnavailableReason::LegacyMissingMemberKeys)
        );
    }

    #[test]
    fn malformed_membership_and_provenance_fail_closed() {
        let mut value = row(vec!["owner/repo|1|1|1".to_owned()]);
        value["member_keys"] = json!(["owner/repo|1|1|1", "owner/repo|1|1|1"]);
        assert!(matches!(
            parse(value),
            Availability::Unavailable(UnavailableReason::Invalid(_))
        ));
        let value = row(vec![
            "owner/repo|2|1|1".to_owned(),
            "owner/repo|1|1|1".to_owned(),
        ]);
        assert!(matches!(
            parse(value),
            Availability::Unavailable(UnavailableReason::Invalid(_))
        ));
        assert!(member_key("Owner/Repo", 1, 1, 1).is_some());
        assert!(matches!(
            parse(row(vec!["Owner/Repo|1|1|1".to_owned()])),
            Availability::Unavailable(UnavailableReason::Invalid(_))
        ));
        let mut value = row(vec!["owner/repo|1|1|1".to_owned()]);
        value["provenance"]["collected_at"] = json!("unknown");
        assert!(matches!(
            parse(value),
            Availability::Unavailable(UnavailableReason::Invalid(_))
        ));
    }
}
