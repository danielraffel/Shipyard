//! Versioned, producer-self-asserted, format-validated joins between a validated PR head and a
//! protected-branch merge. GitHub PR counts are deliberately insufficient.

use std::io::BufRead;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The durable import contract. Producers must set `safe_merge` only after
/// Shipyard has matched the exact validated head and its gate receipt to the
/// protected-branch merge event.
pub const SCHEMA: &str = "shipyard-merge-receipt/v1";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MergeReceipt {
    pub schema: String,
    pub receipt_id: String,
    pub repo: String,
    pub project: String,
    pub base_branch: String,
    pub pr: i64,
    pub head_sha: String,
    pub validated_head_sha: String,
    pub merge_commit_sha: String,
    pub merged_at: DateTime<Utc>,
    pub safe_merge: bool,
    pub source: String,
    /// Exact required checks that were green for `validated_head_sha`.
    pub required_checks: Vec<String>,
    /// SHA-256 of the immutable gate receipt that supplied those checks.
    pub gate_receipt_sha256: String,
    /// Immutable hash or URL of the producer's gate/merge evidence.
    pub evidence: String,
}

impl MergeReceipt {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!(
                "unsupported merge receipt schema {:?}",
                self.schema
            ));
        }
        for (name, value) in [
            ("receipt_id", &self.receipt_id),
            ("repo", &self.repo),
            ("project", &self.project),
            ("base_branch", &self.base_branch),
            ("source", &self.source),
            ("evidence", &self.evidence),
        ] {
            if value.trim().is_empty() {
                return Err(format!("merge receipt {name} is empty"));
            }
        }
        if self.pr <= 0 {
            return Err("merge receipt pr must be positive".to_owned());
        }
        for (name, sha) in [
            ("head_sha", &self.head_sha),
            ("validated_head_sha", &self.validated_head_sha),
            ("merge_commit_sha", &self.merge_commit_sha),
        ] {
            if sha.len() != 40
                || !sha.bytes().all(|b| b.is_ascii_hexdigit())
                || sha.chars().any(|c| c.is_ascii_uppercase())
                || sha.chars().all(|c| c == '0')
            {
                return Err(format!("merge receipt {name} must be a 40-character SHA"));
            }
        }
        if self.head_sha != self.validated_head_sha {
            return Err("merge receipt validated_head_sha does not equal head_sha".to_owned());
        }
        if !self.safe_merge {
            return Err("merge receipt is not a safe merge".to_owned());
        }
        if self.source != "shipyard" {
            return Err(format!(
                "merge receipt source {:?} is not the expected producer label",
                self.source
            ));
        }
        if self.required_checks.is_empty()
            || self
                .required_checks
                .iter()
                .any(|check| check.trim().is_empty())
        {
            return Err("merge receipt required_checks must be non-empty".to_owned());
        }
        if self.gate_receipt_sha256.len() != 64
            || !self
                .gate_receipt_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            || self
                .gate_receipt_sha256
                .chars()
                .any(|c| c.is_ascii_uppercase())
            || self.gate_receipt_sha256.chars().all(|c| c == '0')
        {
            return Err(
                "merge receipt gate_receipt_sha256 must be a nonzero lowercase SHA-256".to_owned(),
            );
        }
        Ok(())
    }
}

/// Parse a JSON array, one JSON object, or JSONL and reject the whole import
/// when any record is malformed. Partial imports are never durable.
pub fn parse_reader(reader: impl BufRead) -> Result<Vec<MergeReceipt>, String> {
    let mut text = String::new();
    for line in reader.lines() {
        text.push_str(&line.map_err(|e| e.to_string())?);
        text.push('\n');
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("merge receipt import is empty".to_owned());
    }
    let value: Value = serde_json::from_str(trimmed)
        .or_else(|_| {
            let values = trimmed
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Array(values))
        })
        .map_err(|e: String| format!("merge receipt JSON parse failed: {e}"))?;
    let values = value.as_array().cloned().unwrap_or_else(|| vec![value]);
    let mut receipts = Vec::with_capacity(values.len());
    for value in values {
        let receipt: MergeReceipt = serde_json::from_value(value)
            .map_err(|e| format!("merge receipt decode failed: {e}"))?;
        receipt.validate()?;
        receipts.push(receipt);
    }
    if receipts.is_empty() {
        return Err("merge receipt import is empty".to_owned());
    }
    Ok(receipts)
}

pub fn parse_path(path: Option<&Path>) -> Result<Vec<MergeReceipt>, String> {
    let stdin = std::io::stdin();
    if path.is_none_or(|path| path == Path::new("-")) {
        return parse_reader(stdin.lock());
    }
    let file = std::fs::File::open(path.expect("path present")).map_err(|e| e.to_string())?;
    parse_reader(std::io::BufReader::new(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn valid() -> MergeReceipt {
        MergeReceipt {
            schema: SCHEMA.to_owned(),
            receipt_id: "r-1".to_owned(),
            repo: "Generous-Corp/pulp".to_owned(),
            project: "pulp".to_owned(),
            base_branch: "main".to_owned(),
            pr: 42,
            head_sha: "a".repeat(40),
            validated_head_sha: "a".repeat(40),
            merge_commit_sha: "b".repeat(40),
            merged_at: "2026-10-16T00:00:00Z".parse().unwrap(),
            safe_merge: true,
            source: "shipyard".to_owned(),
            required_checks: vec!["macos".to_owned()],
            gate_receipt_sha256: "c".repeat(64),
            evidence: "sha256:abc".to_owned(),
        }
    }

    #[test]
    fn parses_jsonl_and_rejects_unsafe_records() {
        let value = serde_json::to_string(&valid()).unwrap();
        assert_eq!(
            parse_reader(Cursor::new(format!("{value}\n{value}\n").into_bytes()))
                .unwrap()
                .len(),
            2
        );
        let mut unsafe_receipt = valid();
        unsafe_receipt.safe_merge = false;
        assert!(parse_reader(Cursor::new(serde_json::to_vec(&unsafe_receipt).unwrap())).is_err());
    }

    #[test]
    fn rejects_head_mismatch_and_non_shipyard_source() {
        let mut receipt = valid();
        receipt.validated_head_sha = "c".repeat(40);
        assert!(receipt.validate().is_err());
        let mut receipt = valid();
        receipt.source = "github".to_owned();
        assert!(receipt.validate().is_err());
    }

    #[test]
    fn rejects_unknown_fields_bad_hashes_and_empty_checks() {
        let mut receipt = valid();
        receipt.head_sha = "A".repeat(40);
        assert!(receipt.validate().is_err());
        let mut receipt = valid();
        receipt.gate_receipt_sha256 = "0".repeat(64);
        assert!(receipt.validate().is_err());
        let mut receipt = valid();
        receipt.required_checks.clear();
        assert!(receipt.validate().is_err());
        let mut object = serde_json::to_value(valid()).unwrap();
        object["unexpected"] = serde_json::json!(true);
        assert!(parse_reader(Cursor::new(serde_json::to_vec(&object).unwrap())).is_err());
    }

    #[test]
    fn accepts_single_and_array_and_rejects_empty_or_bad_lines() {
        let value = serde_json::to_string(&valid()).unwrap();
        assert_eq!(parse_reader(Cursor::new(value.clone())).unwrap().len(), 1);
        assert_eq!(
            parse_reader(Cursor::new(format!("[{value}]")))
                .unwrap()
                .len(),
            1
        );
        assert!(parse_reader(Cursor::new(" ")).is_err());
        assert!(parse_reader(Cursor::new(format!("{value}\nnot-json"))).is_err());
    }
}
