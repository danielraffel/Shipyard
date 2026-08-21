use serde::Serialize;
use serde_json::{Value, json};

use crate::recovery_worker::{RECOVERY_SCHEMA_VERSION, RecoveryRequest};

#[derive(Serialize)]
struct RecoveryPrompt<'a> {
    schema_version: u32,
    task: &'static str,
    provider: &'a str,
    request: &'a RecoveryRequest,
    output_schema: Value,
    constraints: [&'static str; 8],
}

fn recovery_output_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "required": [
            "schema_version",
            "verdict",
            "category",
            "confidence",
            "evidence",
            "candidate_paths",
            "focused_tests"
        ],
        "properties": {
            "schema_version": { "const": RECOVERY_SCHEMA_VERSION },
            "verdict": { "const": "escalate" },
            "category": {
                "enum": [
                    "compile",
                    "test",
                    "conflict",
                    "security",
                    "workflow",
                    "infra",
                    "unknown"
                ]
            },
            "confidence": { "enum": ["low", "medium", "high"] },
            "evidence": { "type": "array", "maxItems": 0 },
            "candidate_paths": { "type": "array", "maxItems": 0 },
            "focused_tests": { "type": "array", "maxItems": 0 }
        }
    })
}

pub(super) fn recovery_prompt(
    provider: &str,
    request: &RecoveryRequest,
) -> serde_json::Result<Value> {
    serde_json::to_value(RecoveryPrompt {
        schema_version: RECOVERY_SCHEMA_VERSION,
        task: "Route this exact-head failure from Shipyard-normalized check names and merge state only. Return exactly one JSON object satisfying output_schema; phase 1 cannot diagnose a cause or authorize repair.",
        provider,
        request,
        output_schema: recovery_output_schema(),
        constraints: [
            "classification and escalation routing only",
            "entire response must be fewer than 800 words",
            "tools are disabled and no repository is available",
            "bounded_repair is forbidden",
            "no_change is forbidden; verdict must be escalate",
            "evidence, candidate_paths, and focused_tests must be empty arrays",
            "return no free-form summary, evidence, explanation, or causal claim",
            "no GitHub, queue, merge, release, publish, or repository action",
        ],
    })
}
