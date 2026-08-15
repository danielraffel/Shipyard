//! Shared parsing for GitHub's authoritative required-check policy surfaces.
//!
//! Classic branch protection and repository rulesets expose required checks
//! through different REST payloads. Consumers must combine both surfaces;
//! materialized PR checks alone cannot reveal a required context that has not
//! posted yet.

use serde_json::Value;

use crate::merge_steward::RequiredCheck;

/// Percent-encode one GitHub REST path segment.
#[must_use]
pub fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

/// Parse classic branch-protection required checks.
pub fn classic_required_checks(value: &Value) -> Result<Vec<RequiredCheck>, String> {
    let mut checks = Vec::new();
    if let Some(contexts) = value.get("contexts") {
        let contexts = contexts
            .as_array()
            .ok_or_else(|| "classic required-check contexts was not an array".to_owned())?;
        for context in contexts {
            let context = context
                .as_str()
                .filter(|context| !context.is_empty())
                .ok_or_else(|| {
                    "classic required-check context was not a non-empty string".to_owned()
                })?;
            checks.push(RequiredCheck {
                context: context.to_owned(),
                app_id: None,
            });
        }
    }
    if let Some(required) = value.get("checks") {
        let required = required
            .as_array()
            .ok_or_else(|| "classic required-check checks was not an array".to_owned())?;
        for check in required {
            let context = check
                .get("context")
                .and_then(Value::as_str)
                .filter(|context| !context.is_empty())
                .ok_or_else(|| "classic required check missing context".to_owned())?;
            checks.push(RequiredCheck {
                context: context.to_owned(),
                app_id: optional_app_id(check, "app_id")?,
            });
        }
    }
    Ok(normalize_required_checks(checks))
}

/// Parse evaluated repository-ruleset required checks.
pub fn evaluated_required_checks(value: &Value) -> Result<Vec<RequiredCheck>, String> {
    let rules = evaluated_rules(value)?;
    let mut checks = Vec::new();
    for rule in rules {
        if rule.get("type").and_then(Value::as_str) != Some("required_status_checks") {
            continue;
        }
        let required = rule
            .pointer("/parameters/required_status_checks")
            .and_then(Value::as_array)
            .ok_or_else(|| "required-status-check rule was missing its checks array".to_owned())?;
        for check in required {
            let context = check
                .get("context")
                .and_then(Value::as_str)
                .filter(|context| !context.is_empty())
                .ok_or_else(|| "required status check missing context".to_owned())?;
            checks.push(RequiredCheck {
                context: context.to_owned(),
                app_id: optional_app_id(check, "integration_id")?,
            });
        }
    }
    Ok(normalize_required_checks(checks))
}

/// Canonicalize a union of classic and ruleset policies.
#[must_use]
pub fn normalize_required_checks(mut checks: Vec<RequiredCheck>) -> Vec<RequiredCheck> {
    checks.sort();
    checks.dedup();
    let pinned_contexts = checks
        .iter()
        .filter(|check| check.app_id.is_some())
        .map(|check| check.context.clone())
        .collect::<Vec<_>>();
    checks.retain(|check| {
        check.app_id.is_some()
            || !pinned_contexts
                .iter()
                .any(|context| context.eq_ignore_ascii_case(&check.context))
    });
    checks
}

fn evaluated_rules(value: &Value) -> Result<Vec<&Value>, String> {
    let pages = value
        .as_array()
        .ok_or_else(|| "evaluated branch rules was not an array".to_owned())?;
    let arrays = pages.iter().filter(|page| page.is_array()).count();
    if arrays != 0 && arrays != pages.len() {
        return Err("evaluated branch rules mixed paginated and flat shapes".to_owned());
    }
    Ok(if arrays == pages.len() {
        pages
            .iter()
            .flat_map(|page| page.as_array().into_iter().flatten())
            .collect()
    } else {
        pages.iter().collect()
    })
}

fn optional_app_id(value: &Value, field: &str) -> Result<Option<u64>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_i64() {
            Some(-1) => Ok(None),
            Some(app_id) if app_id >= 0 => u64::try_from(app_id)
                .map(Some)
                .map_err(|_| format!("required status check has invalid {field}")),
            _ => Err(format!("required status check has invalid {field}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classic_required_checks, encode_path_segment, evaluated_required_checks,
        normalize_required_checks,
    };
    use crate::merge_steward::RequiredCheck;

    #[test]
    fn combines_classic_contexts_and_app_pins_without_duplicates() {
        let checks = classic_required_checks(&serde_json::json!({
            "contexts": ["macos", "legacy"],
            "checks": [
                {"context": "macos", "app_id": 42},
                {"context": "signed", "app_id": 7}
            ]
        }))
        .expect("classic policy");
        assert_eq!(
            checks,
            vec![
                RequiredCheck {
                    context: "legacy".to_owned(),
                    app_id: None
                },
                RequiredCheck {
                    context: "macos".to_owned(),
                    app_id: Some(42)
                },
                RequiredCheck {
                    context: "signed".to_owned(),
                    app_id: Some(7)
                },
            ]
        );
    }

    #[test]
    fn parses_paginated_evaluated_rules() {
        let checks = evaluated_required_checks(&serde_json::json!([[
            {"type":"merge_queue","parameters":{}},
            {"type":"required_status_checks","parameters":{"required_status_checks":[
                {"context":"rules-a","integration_id":42}
            ]}}
        ], [
            {"type":"required_status_checks","parameters":{"required_status_checks":[
                {"context":"rules-b"}
            ]}}
        ]]))
        .expect("evaluated policy");
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].context, "rules-a");
        assert_eq!(checks[0].app_id, Some(42));
        assert_eq!(checks[1].context, "rules-b");
    }

    #[test]
    fn rejects_malformed_evaluated_policy() {
        assert!(evaluated_required_checks(&serde_json::json!({})).is_err());
        assert!(
            evaluated_required_checks(&serde_json::json!([{
                "type":"required_status_checks",
                "parameters":{"required_status_checks":[{}]}
            }]))
            .is_err()
        );
        assert!(
            evaluated_required_checks(&serde_json::json!([{
                "type":"required_status_checks",
                "parameters":{}
            }]))
            .is_err()
        );
        assert!(
            evaluated_required_checks(&serde_json::json!([
                [{"type":"merge_queue","parameters":{}}],
                {"type":"merge_queue","parameters":{}}
            ]))
            .is_err()
        );
    }

    #[test]
    fn rejects_malformed_classic_policy_instead_of_treating_it_as_empty() {
        assert!(classic_required_checks(&serde_json::json!({"contexts": {}})).is_err());
        assert!(classic_required_checks(&serde_json::json!({"contexts": [null]})).is_err());
        assert!(classic_required_checks(&serde_json::json!({"checks": {}})).is_err());
        assert!(classic_required_checks(&serde_json::json!({"checks": [{}]})).is_err());
    }

    #[test]
    fn normalization_preserves_distinct_pinned_producers() {
        let checks = normalize_required_checks(vec![
            RequiredCheck {
                context: "ci".to_owned(),
                app_id: Some(1),
            },
            RequiredCheck {
                context: "ci".to_owned(),
                app_id: Some(2),
            },
            RequiredCheck {
                context: "ci".to_owned(),
                app_id: None,
            },
        ]);
        assert_eq!(checks.len(), 2);
    }

    #[test]
    fn path_segments_are_percent_encoded() {
        assert_eq!(encode_path_segment("main"), "main");
        assert_eq!(encode_path_segment("release/1.2"), "release%2F1.2");
        assert_eq!(encode_path_segment("topic name"), "topic%20name");
    }
}
