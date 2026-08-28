//! Conservative projection from validated legacy records into shadow candidates.

use super::{
    BTreeMap, Digest, ImportCandidate, ImportReport, Sha256, Value, WorkLedgerError,
    WorkLedgerResult, opaque_ref,
};
use crate::record_identity::canonical_repository_slug;

pub(in crate::work_ledger) fn candidate(
    kind: &str,
    source_ref: String,
    content_digest: String,
    value: &Value,
) -> ImportCandidate {
    let request = value.get("request").unwrap_or(value);
    let ship_state = value.get("ship_state");
    let repo = text(request, &["repo", "repository"])
        .or_else(|| text(value, &["repo", "repository"]))
        .or_else(|| ship_state.and_then(|state| text(state, &["repo", "repository"])))
        .map(|repo| canonical_repository_slug(&repo));
    let pr = number(request, &["pr", "pr_number"]).or_else(|| number(value, &["pr", "pr_number"]));
    let head_sha = text(request, &["head_sha", "head", "sha"])
        .or_else(|| text(value, &["head_sha", "head", "sha"]))
        .or_else(|| ship_state.and_then(|state| text(state, &["head_sha", "head", "sha"])));
    let base_ref = text(request, &["base_ref", "base_branch", "base"])
        .or_else(|| text(value, &["base_ref", "base_branch", "base"]))
        .or_else(|| ship_state.and_then(|state| text(state, &["base_ref", "base_branch", "base"])));
    let owner_id = text(value, &["owner_id"]).map(|owner| opaque_ref("owner", &owner));
    let owner_generation =
        number(value, &["ownership_generation", "owner_generation"]).unwrap_or(1);
    let terminal_adapter = value
        .get("terminal_adapter")
        .and_then(|adapter| text(adapter, &["kind"]))
        .or_else(|| text(value, &["owner_terminal_provenance"]));
    // Legacy adapter fragments are only searchable hints. They are not a
    // dispatchable provider route until a complete integrity-bound route
    // registry record is joined, so missing provenance remains explicit.
    let provider_adapter = None;
    let agent_adapter = value
        .get("agent_adapter")
        .and_then(|adapter| text(adapter, &["kind"]))
        .or_else(|| text(value, &["owner_provider"]));
    let coordinator_route_ref = text(value, &["coordinator_route_id", "agent_parent_session_id"])
        .map(|route| opaque_ref("route", &route));
    let repair_route_ref = repair_route_ref(value);
    let pr_truth = "unknown";
    let continuation_truth = "unknown";
    let durable_id = durable_id(value);
    let storage_identity = if kind == "ship_state" || (kind == "recovery" && durable_id.is_some()) {
        ""
    } else {
        &source_ref
    };
    let identity = format!(
        "{kind}|{}|{}|{}|{}|{storage_identity}",
        repo.as_deref().unwrap_or(""),
        pr.map_or_else(String::new, |value| value.to_string()),
        head_sha.as_deref().unwrap_or(""),
        durable_id.unwrap_or_default(),
    );
    ImportCandidate {
        work_id: opaque_ref("wi", &identity),
        kind: kind.to_owned(),
        repo,
        pr,
        head_sha,
        base_ref,
        goal_id: text(value, &["workstream_id", "goal_id"]).map(|goal| opaque_ref("goal", &goal)),
        goal_generation: number(value, &["goal_generation"]).unwrap_or(1),
        lane: text(value, &["lane", "target"]),
        role: text(value, &["role"]).unwrap_or_else(|| "root".to_owned()),
        owner_id,
        owner_generation,
        terminal_adapter,
        agent_adapter,
        provider_adapter,
        coordinator_route_ref,
        repair_route_ref,
        pr_truth: pr_truth.to_owned(),
        acceptance_truth: "unknown".to_owned(),
        continuation_truth: continuation_truth.to_owned(),
        phase: "shadow_imported".to_owned(),
        source_ref,
        content_digest,
        source_updated_at: text(value, &["updated_at"]).or_else(|| {
            value
                .pointer("/ship_state/updated_at")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }),
    }
}

pub(super) fn legacy_is_newer(
    legacy: &ImportCandidate,
    scoped: &ImportCandidate,
) -> WorkLedgerResult<bool> {
    let parse = |candidate: &ImportCandidate| {
        candidate
            .source_updated_at
            .as_deref()
            .ok_or_else(|| WorkLedgerError::Refused("ship state is missing updated_at".to_owned()))
            .and_then(|value| {
                chrono::DateTime::parse_from_rfc3339(value).map_err(|_| {
                    WorkLedgerError::Refused("ship state has invalid updated_at".to_owned())
                })
            })
    };
    Ok(parse(legacy)? > parse(scoped)?)
}

fn durable_id(value: &Value) -> Option<String> {
    text(value, &["resume_id", "dedupe_key", "job_id", "id"]).or_else(|| {
        value
            .pointer("/request/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

fn repair_route_ref(value: &Value) -> Option<String> {
    text(value, &["owner_route_id"])
        .or_else(|| pointer_text(value, "/agent_adapter/route_id"))
        .or_else(|| pointer_text(value, "/terminal_adapter/route_id"))
        .map(|route| opaque_ref("route", &route))
}

fn pointer_text(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn text(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn number(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

pub(in crate::work_ledger) fn import_report(
    candidates: &[ImportCandidate],
    applied: bool,
    inserted: usize,
    updated: usize,
) -> ImportReport {
    let mut by_kind = BTreeMap::new();
    let mut plan = Sha256::new();
    for candidate in candidates {
        *by_kind.entry(candidate.kind.clone()).or_insert(0) += 1;
        plan.update(candidate.work_id.as_bytes());
        plan.update([0]);
        plan.update(candidate.content_digest.as_bytes());
        plan.update([0]);
    }
    ImportReport {
        applied,
        mode: "shadow".to_owned(),
        candidates: candidates.len(),
        inserted,
        updated,
        unchanged: candidates.len().saturating_sub(inserted + updated),
        by_kind,
        plan_digest: hex::encode(plan.finalize()),
        activation_enabled: false,
        dispatch_enabled: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_canonicalizes_historical_repository_whitespace() {
        let value = serde_json::json!({
            "repo": " Owner/Repository ",
            "pr": 42,
            "head_sha": "a".repeat(40),
        });
        let projected = candidate(
            "queue_request",
            "legacy_ref".to_owned(),
            "b".repeat(64),
            &value,
        );

        assert_eq!(projected.repo.as_deref(), Some("owner/repository"));
    }
}
