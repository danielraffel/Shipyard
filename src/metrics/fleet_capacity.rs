//! Strict importer and reducer for `TartCI`'s observation-only capacity receipts.

use std::collections::BTreeMap;
use std::io::BufRead;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const SCHEMA: &str = "tartci.fleet-capacity/v1";
const MAX_VALIDITY_SECONDS: i64 = 3600;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FleetCapacitySample {
    pub schema: String,
    pub sample_id: String,
    pub observed_at: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    pub host_id: String,
    pub host_boot_id: String,
    pub generation: String,
    pub repo: String,
    pub target: String,
    pub profile_sha256: String,
    pub configured_slots: i64,
    pub eligible_slots: Option<i64>,
    pub free_slots: Option<i64>,
    pub occupied_slots: Option<i64>,
    pub in_flight_slots: Option<i64>,
    pub unknown_slots: i64,
    pub pool_state: String,
    pub source: String,
    pub auth_method: String,
    pub verification: String,
    pub evidence: Value,
    pub evidence_sha256: String,
    pub census_sha256: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl FleetCapacitySample {
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!(
                "unsupported fleet capacity schema {:?}",
                self.schema
            ));
        }
        for (name, value) in [
            ("sample_id", &self.sample_id),
            ("host_id", &self.host_id),
            ("host_boot_id", &self.host_boot_id),
            ("generation", &self.generation),
            ("repo", &self.repo),
            ("target", &self.target),
            ("profile_sha256", &self.profile_sha256),
            ("evidence_sha256", &self.evidence_sha256),
            ("census_sha256", &self.census_sha256),
        ] {
            if value.trim().is_empty() {
                return Err(format!("fleet capacity {name} is empty"));
            }
        }
        for (name, digest) in [
            ("sample_id", &self.sample_id),
            ("profile_sha256", &self.profile_sha256),
            ("evidence_sha256", &self.evidence_sha256),
            ("census_sha256", &self.census_sha256),
        ] {
            if !is_lower_sha256(digest) {
                return Err(format!("fleet capacity {name} must be a lowercase SHA-256"));
            }
        }
        if self.repo.split('/').count() != 2
            || self.repo.split('/').any(|part| part.trim().is_empty())
        {
            return Err("fleet capacity repo must be OWNER/REPO".to_owned());
        }
        if self.configured_slots < 0 || self.unknown_slots < 0 {
            return Err("fleet capacity slot counts must be nonnegative".to_owned());
        }
        for (name, value) in [
            ("eligible_slots", self.eligible_slots),
            ("free_slots", self.free_slots),
            ("occupied_slots", self.occupied_slots),
            ("in_flight_slots", self.in_flight_slots),
        ] {
            if value.is_some_and(|value| value < 0) {
                return Err(format!("fleet capacity {name} must be nonnegative"));
            }
        }
        if self.verification == "verified" {
            let (Some(eligible), Some(free), Some(occupied), Some(in_flight)) = (
                self.eligible_slots,
                self.free_slots,
                self.occupied_slots,
                self.in_flight_slots,
            ) else {
                return Err("verified fleet capacity must include all slot counts".to_owned());
            };
            let eligible_u = u128::try_from(eligible)
                .map_err(|_| "fleet capacity eligible_slots is negative".to_owned())?;
            let free_u = u128::try_from(free)
                .map_err(|_| "fleet capacity free_slots is negative".to_owned())?;
            let occupied_u = u128::try_from(occupied)
                .map_err(|_| "fleet capacity occupied_slots is negative".to_owned())?;
            let in_flight_u = u128::try_from(in_flight)
                .map_err(|_| "fleet capacity in_flight_slots is negative".to_owned())?;
            let unknown_u = u128::try_from(self.unknown_slots)
                .map_err(|_| "fleet capacity unknown_slots is negative".to_owned())?;
            let configured_u = u128::try_from(self.configured_slots)
                .map_err(|_| "fleet capacity configured_slots is negative".to_owned())?;
            let slot_sum = free_u
                .checked_add(occupied_u)
                .and_then(|sum| sum.checked_add(in_flight_u));
            let partition_sum = eligible_u.checked_add(unknown_u);
            if eligible > self.configured_slots
                || slot_sum != Some(eligible_u)
                || partition_sum != Some(configured_u)
            {
                return Err("fleet capacity slot counts are inconsistent".to_owned());
            }
        } else if self.verification != "unknown" {
            return Err("fleet capacity verification must be verified or unknown".to_owned());
        } else if !self.evidence.is_object()
            || self.eligible_slots.is_some()
            || self.free_slots.is_some()
            || self.occupied_slots.is_some()
            || self.in_flight_slots.is_some()
            || self.unknown_slots != self.configured_slots
        {
            return Err("unknown fleet capacity must not advertise slot counts".to_owned());
        }
        if !matches!(
            self.pool_state.as_str(),
            "on" | "draining" | "off" | "unknown"
        ) {
            return Err("invalid fleet capacity pool_state".to_owned());
        }
        if self.source != "tartci" {
            return Err("fleet capacity source must be tartci".to_owned());
        }
        if self.auth_method != "untrusted_local" {
            return Err("fleet capacity auth_method must be untrusted_local".to_owned());
        }
        if self.valid_until <= self.observed_at {
            return Err("fleet capacity valid_until must be after observed_at".to_owned());
        }
        if self.valid_until - self.observed_at > Duration::seconds(MAX_VALIDITY_SECONDS) {
            return Err("fleet capacity validity interval exceeds 3600 seconds".to_owned());
        }
        if self.observed_at > Utc::now() + Duration::seconds(60) {
            return Err("fleet capacity observed_at is materially in the future".to_owned());
        }
        let expected = canonical_digest(self, true)?;
        if self.sample_id != expected {
            return Err("fleet capacity sample_id does not match canonical sample".to_owned());
        }
        let evidence_sha = digest_value(&self.evidence)?;
        if self.evidence_sha256 != evidence_sha {
            return Err("fleet capacity evidence_sha256 does not match evidence".to_owned());
        }
        let evidence_object = self
            .evidence
            .as_object()
            .ok_or_else(|| "fleet capacity evidence must be an object".to_owned())?;
        if evidence_object.get("schema") != Some(&Value::String("tartci.gate-supply/v1".to_owned()))
            || evidence_object.get("host") != Some(&Value::String(self.host_id.clone()))
            || evidence_object.get("repo") != Some(&Value::String(self.repo.clone()))
            || evidence_object.get("class") != Some(&Value::String(self.target.clone()))
            || evidence_object.get("profile_sha256")
                != Some(&Value::String(self.profile_sha256.clone()))
        {
            return Err(
                "fleet capacity evidence identity or profile does not match sample".to_owned(),
            );
        }
        let evidence_verdict = evidence_object
            .get("verdict")
            .and_then(Value::as_str)
            .ok_or_else(|| "fleet capacity evidence verdict is missing".to_owned())?;
        let evidence_pool = evidence_object
            .get("pool")
            .and_then(Value::as_str)
            .ok_or_else(|| "fleet capacity evidence pool is missing".to_owned())?;
        if self.pool_state != evidence_pool {
            return Err("fleet capacity pool_state does not match evidence pool".to_owned());
        }
        let lanes = evidence_object
            .get("lanes")
            .and_then(Value::as_array)
            .ok_or_else(|| "fleet capacity evidence lanes must be an array".to_owned())?;
        if self.configured_slots != i64::try_from(lanes.len()).unwrap_or(i64::MAX) {
            return Err("fleet capacity configured_slots does not match evidence lanes".to_owned());
        }
        if self.verification == "verified" {
            if evidence_verdict != "ok" || evidence_pool != "on" {
                return Err("verified fleet capacity requires ok/on evidence".to_owned());
            }
            if evidence_object
                .get("inventory")
                .is_some_and(|inventory| inventory.as_str() != Some("tart"))
            {
                return Err(
                    "verified fleet capacity requires a recognized capacity inventory".to_owned(),
                );
            }
            for (name, expected) in [
                ("free", self.free_slots),
                ("occupied", self.occupied_slots),
                ("in_flight", self.in_flight_slots),
            ] {
                let actual = evidence_object
                    .get(name)
                    .and_then(Value::as_i64)
                    .ok_or_else(|| format!("verified fleet capacity evidence {name} is missing"))?;
                if Some(actual) != expected {
                    return Err(format!(
                        "fleet capacity evidence {name} does not match sample"
                    ));
                }
            }
        }
        let census = self
            .evidence
            .as_object()
            .map(|object| {
                [
                    "schema", "host", "repo", "class", "verdict", "pool", "lanes",
                ]
                .into_iter()
                .map(|key| {
                    (
                        key.to_owned(),
                        object.get(key).cloned().unwrap_or(Value::Null),
                    )
                })
                .collect::<Map<_, _>>()
            })
            .ok_or_else(|| "fleet capacity evidence must be an object".to_owned())?;
        if self.census_sha256 != digest_value(&Value::Object(census))? {
            return Err("fleet capacity census_sha256 does not match evidence".to_owned());
        }
        Ok(())
    }
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value.bytes().all(|byte| !byte.is_ascii_uppercase())
        && value.bytes().any(|byte| byte != b'0')
}

pub(crate) fn canonical_digest(
    sample: &FleetCapacitySample,
    omit_sample_id: bool,
) -> Result<String, String> {
    let mut value = serde_json::to_value(sample).map_err(|error| error.to_string())?;
    if omit_sample_id {
        value
            .as_object_mut()
            .ok_or_else(|| "fleet capacity sample is not an object".to_owned())?
            .remove("sample_id");
    }
    let value = canonical_value(&value);
    let bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    let mut digest = Sha256::new();
    digest.update(bytes);
    digest.update(b"\n");
    Ok(hex::encode(digest.finalize()))
}

pub(crate) fn digest_value(value: &Value) -> Result<String, String> {
    let bytes = serde_json::to_vec(&canonical_value(value)).map_err(|error| error.to_string())?;
    let mut digest = Sha256::new();
    digest.update(bytes);
    digest.update(b"\n");
    Ok(hex::encode(digest.finalize()))
}

fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.clone(), canonical_value(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect::<Map<_, _>>())
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_value).collect()),
        other => other.clone(),
    }
}

/// Parse one JSON object, an array, or JSONL and reject the whole import if any record is bad.
pub fn parse_reader(reader: impl BufRead) -> Result<Vec<FleetCapacitySample>, String> {
    let mut text = String::new();
    for line in reader.lines() {
        text.push_str(&line.map_err(|error| error.to_string())?);
        text.push('\n');
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("fleet capacity import is empty".to_owned());
    }
    let values = match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Array(values)) => values,
        Ok(Value::Object(value)) => vec![Value::Object(value)],
        Ok(_) => return Err("fleet capacity import must contain an object or array".to_owned()),
        Err(_) => trimmed
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?,
    };
    values
        .into_iter()
        .map(|value| {
            let sample: FleetCapacitySample = serde_json::from_value(value)
                .map_err(|error| format!("invalid fleet capacity record: {error}"))?;
            sample.validate()?;
            Ok(sample)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> FleetCapacitySample {
        let mut value = FleetCapacitySample {
            schema: SCHEMA.to_owned(),
            sample_id: String::new(),
            observed_at: "2026-01-01T00:00:00Z".parse().unwrap(),
            valid_until: "2026-01-01T00:02:00Z".parse().unwrap(),
            host_id: "m3".to_owned(),
            host_boot_id: "boot-1".to_owned(),
            generation: "gen-1".to_owned(),
            repo: "Generous-Corp/pulp".to_owned(),
            target: "macos".to_owned(),
            profile_sha256: "a".repeat(64),
            configured_slots: 3,
            eligible_slots: Some(3),
            free_slots: Some(1),
            occupied_slots: Some(1),
            in_flight_slots: Some(1),
            unknown_slots: 0,
            pool_state: "on".to_owned(),
            source: "tartci".to_owned(),
            auth_method: "untrusted_local".to_owned(),
            verification: "verified".to_owned(),
            evidence: serde_json::json!({
                "schema": "tartci.gate-supply/v1",
                "host": "m3",
                "repo": "Generous-Corp/pulp",
                "class": "macos",
                "profile_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "verdict": "ok",
                "pool": "on",
                "free": 1,
                "occupied": 1,
                "in_flight": 1,
                "lanes": [{"lane": "a"}, {"lane": "b"}, {"lane": "c"}]
            }),
            evidence_sha256: "b".repeat(64),
            census_sha256: "c".repeat(64),
            reason: None,
        };
        value.evidence_sha256 = digest_value(&value.evidence).unwrap();
        let evidence = value.evidence.as_object().unwrap();
        let census = [
            "schema", "host", "repo", "class", "verdict", "pool", "lanes",
        ]
        .into_iter()
        .map(|key| {
            (
                key.to_owned(),
                evidence.get(key).cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
        value.census_sha256 = digest_value(&Value::Object(census)).unwrap();
        value.sample_id = canonical_digest(&value, true).unwrap();
        value
    }

    #[test]
    fn validates_canonical_identity_and_jsonl() {
        let value = sample();
        value.validate().unwrap();
        let text = serde_json::to_string(&value).unwrap();
        let parsed = parse_reader(text.as_bytes()).unwrap();
        assert_eq!(parsed, vec![value]);

        let pretty = serde_json::to_string_pretty(&parsed[0]).unwrap();
        assert_eq!(parse_reader(pretty.as_bytes()).unwrap(), parsed);
    }

    #[test]
    fn accepts_a_real_tartci_fixture_as_object_array_and_jsonl() {
        let text = include_str!("../../tests/fixtures/fleet-capacity/tartci-v1.json");
        let value: Value = serde_json::from_str(text).unwrap();
        let object = parse_reader(text.as_bytes()).unwrap();
        assert_eq!(object.len(), 1);
        assert_eq!(
            parse_reader(
                serde_json::to_string(&vec![value.clone()])
                    .unwrap()
                    .as_bytes()
            )
            .unwrap()
            .len(),
            1
        );
        let line = serde_json::to_string(&value).unwrap();
        assert_eq!(
            parse_reader(format!("{line}\n{line}").as_bytes())
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn accepts_a_producer_edge_case_fixture_with_utf8_float_and_large_integer() {
        let text = include_str!("../../tests/fixtures/fleet-capacity/tartci-v1-edge.json");
        let parsed = parse_reader(text.as_bytes()).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].evidence["label"], "café");
        assert_eq!(parsed[0].evidence["large"], 9_007_199_254_740_993_i64);
    }

    #[test]
    fn rejects_tampering_unknown_fields_and_long_intervals() {
        let mut value = sample();
        value.free_slots = Some(2);
        assert!(value.validate().is_err());
        let mut value = sample();
        value.configured_slots = 0;
        value.eligible_slots = Some(0);
        value.free_slots = Some(i64::MAX);
        value.occupied_slots = Some(i64::MAX);
        value.in_flight_slots = Some(2);
        value.unknown_slots = 0;
        let evidence = value.evidence.as_object_mut().unwrap();
        evidence.insert("free".to_owned(), Value::from(i64::MAX));
        evidence.insert("occupied".to_owned(), Value::from(i64::MAX));
        evidence.insert("in_flight".to_owned(), Value::from(2));
        evidence.insert("lanes".to_owned(), Value::Array(Vec::new()));
        value.evidence_sha256 = digest_value(&value.evidence).unwrap();
        value.census_sha256 = digest_value(&Value::Object(
            [
                "schema", "host", "repo", "class", "verdict", "pool", "lanes",
            ]
            .into_iter()
            .map(|key| (key.to_owned(), value.evidence[key].clone()))
            .collect(),
        ))
        .unwrap();
        value.sample_id = canonical_digest(&value, true).unwrap();
        assert!(value.validate().is_err());
        let mut value = sample();
        value.evidence["inventory"] = Value::String("unavailable".to_owned());
        value.evidence_sha256 = digest_value(&value.evidence).unwrap();
        value.sample_id = canonical_digest(&value, true).unwrap();
        assert!(value.validate().is_err());
        let mut value = sample();
        value.evidence["inventory"] = Value::String("unknown".to_owned());
        value.evidence_sha256 = digest_value(&value.evidence).unwrap();
        value.sample_id = canonical_digest(&value, true).unwrap();
        assert!(value.validate().is_err());
        let mut value = sample();
        value.profile_sha256 = "d".repeat(64);
        value.sample_id = canonical_digest(&value, true).unwrap();
        assert!(value.validate().is_err());
        let text = format!("{}\n", serde_json::json!({"schema": SCHEMA}));
        assert!(parse_reader(text.as_bytes()).is_err());
        let mut value = sample();
        value.valid_until = value.observed_at + Duration::seconds(MAX_VALIDITY_SECONDS + 1);
        value.sample_id = canonical_digest(&value, true).unwrap();
        assert!(value.validate().is_err());
    }
}
