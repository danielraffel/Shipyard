use super::*;

const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn required_check(context: &str) -> RecoveryFailureFact {
    RecoveryFailureFact::RequiredCheck {
        context: context.to_owned(),
        app_id: None,
        conclusion: "FAILURE".to_owned(),
        run_id: None,
    }
}

fn required_policy(context: &str) -> Vec<RecoveryRequiredCheck> {
    vec![RecoveryRequiredCheck {
        context: context.to_owned(),
        app_id: None,
    }]
}

fn request(head: &str, config: &str) -> RecoveryRequest {
    RecoveryRequest::new(
        "Generous-Corp/pulp",
        42,
        "main",
        head,
        "failure-v1",
        "Required Windows check failed at the exact head.",
        required_policy("windows"),
        vec![required_check("windows")],
        "policy-v1",
        config,
    )
    .expect("request")
}

fn no_change_output() -> RecoveryOutput {
    RecoveryOutput {
        schema_version: RECOVERY_SCHEMA_VERSION,
        verdict: RecoveryVerdict::NoChange,
        category: RecoveryCategory::Compile,
        confidence: RecoveryConfidence::High,
        evidence: vec![],
        candidate_paths: vec![],
        focused_tests: vec![],
    }
}

fn escalation_output() -> RecoveryOutput {
    RecoveryOutput {
        verdict: RecoveryVerdict::Escalate,
        ..no_change_output()
    }
}

#[path = "tests/archive.rs"]
mod archive;
#[path = "tests/claim.rs"]
mod claim;
#[path = "tests/enqueue.rs"]
mod enqueue;
#[path = "tests/identity.rs"]
mod identity;
#[path = "tests/output.rs"]
mod output;
#[path = "tests/pending.rs"]
mod pending;
