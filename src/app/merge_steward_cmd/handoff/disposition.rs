//! Durable post-handoff disposition derived from an explicit task graph.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::CliFailure;

const MAX_TASK_GRAPH_BYTES: u64 = 256 * 1024;
const MAX_TASK_NODES: usize = 1_024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentDisposition {
    #[default]
    Continue,
    Pause,
}

impl AgentDisposition {
    pub(crate) fn parse(value: &str) -> Result<Self, CliFailure> {
        match value {
            "continue" => Ok(Self::Continue),
            "pause" => Ok(Self::Pause),
            _ => Err(CliFailure::new(
                1,
                "--after-handoff must be continue or pause",
            )),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Pause => "pause",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredDispositionProofV1 {
    pub(crate) schema_version: u32,
    pub(crate) workstream_id: String,
    pub(crate) graph_revision: u64,
    pub(crate) handoff_task_id: String,
    pub(crate) graph_digest: String,
    pub(crate) blocked_nodes: u64,
    pub(crate) integrity_hash: String,
}

impl StoredDispositionProofV1 {
    pub(crate) fn valid_for(&self, workstream_id: &str) -> bool {
        self.schema_version == 1
            && self.workstream_id == workstream_id
            && self.graph_revision > 0
            && valid_id(&self.handoff_task_id)
            && valid_digest(&self.graph_digest)
            && self.integrity_hash == proof_integrity(self)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TaskGraphV1 {
    schema_version: u32,
    workstream_id: String,
    revision: u64,
    handoff_task_id: String,
    nodes: Vec<TaskNodeV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TaskNodeV1 {
    id: String,
    state: TaskStateV1,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TaskStateV1 {
    Pending,
    Running,
    Blocked,
    HandedOff,
    Complete,
    Canceled,
}

pub(crate) fn load_pause_proof(
    path: &Path,
    workstream_id: &str,
) -> Result<StoredDispositionProofV1, CliFailure> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| CliFailure::new(1, format!("read disposition task graph: {error}")))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_TASK_GRAPH_BYTES
    {
        return Err(CliFailure::new(
            1,
            "disposition task graph must be a bounded regular file, not a symlink",
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| CliFailure::new(1, format!("read disposition task graph: {error}")))?;
    let graph: TaskGraphV1 = serde_json::from_slice(&bytes).map_err(|error| {
        CliFailure::new(1, format!("invalid disposition task graph JSON: {error}"))
    })?;
    evaluate_pause_graph(graph, workstream_id)
}

#[allow(clippy::too_many_lines)]
fn evaluate_pause_graph(
    graph: TaskGraphV1,
    workstream_id: &str,
) -> Result<StoredDispositionProofV1, CliFailure> {
    if graph.schema_version != 1
        || graph.workstream_id != workstream_id
        || graph.revision == 0
        || !valid_id(&graph.handoff_task_id)
        || graph.nodes.is_empty()
        || graph.nodes.len() > MAX_TASK_NODES
    {
        return Err(CliFailure::new(
            1,
            "disposition task graph identity, revision, or bounds are invalid",
        ));
    }
    let mut nodes = BTreeMap::new();
    for node in &graph.nodes {
        if !valid_id(&node.id)
            || node
                .depends_on
                .iter()
                .any(|dependency| !valid_id(dependency))
            || !node.depends_on.windows(2).all(|pair| pair[0] < pair[1])
            || nodes.insert(node.id.clone(), node).is_some()
        {
            return Err(CliFailure::new(
                1,
                "disposition task graph node IDs and dependencies must be unique canonical tokens",
            ));
        }
    }
    let handoff = nodes
        .get(&graph.handoff_task_id)
        .ok_or_else(|| CliFailure::new(1, "disposition task graph omits its handoff task"))?;
    if handoff.state != TaskStateV1::HandedOff {
        return Err(CliFailure::new(
            1,
            "disposition handoff task must have state handed_off",
        ));
    }
    for node in nodes.values() {
        if node.id == graph.handoff_task_id && !node.depends_on.is_empty() {
            return Err(CliFailure::new(
                1,
                "the handed-off task cannot retain unresolved local dependencies",
            ));
        }
        for dependency in &node.depends_on {
            if dependency == &node.id || !nodes.contains_key(dependency) {
                return Err(CliFailure::new(
                    1,
                    "disposition task graph contains a missing or self dependency",
                ));
            }
        }
    }
    reject_cycles(&nodes)?;

    let mut runnable = Vec::new();
    let mut blocked_nodes = 0_u64;
    for node in nodes
        .values()
        .filter(|node| node.id != graph.handoff_task_id)
    {
        let dependencies_complete = node.depends_on.iter().all(|dependency| {
            matches!(
                nodes.get(dependency).map(|node| node.state),
                Some(TaskStateV1::Complete | TaskStateV1::Canceled)
            )
        });
        match node.state {
            TaskStateV1::Running => runnable.push(node.id.clone()),
            TaskStateV1::Pending if dependencies_complete => runnable.push(node.id.clone()),
            TaskStateV1::Pending | TaskStateV1::HandedOff => blocked_nodes += 1,
            TaskStateV1::Blocked if !node.depends_on.is_empty() && !dependencies_complete => {
                blocked_nodes += 1;
            }
            TaskStateV1::Blocked => {
                return Err(CliFailure::new(
                    1,
                    format!("blocked task {} lacks an incomplete dependency", node.id),
                ));
            }
            TaskStateV1::Complete | TaskStateV1::Canceled => {}
        }
    }
    if !runnable.is_empty() {
        return Err(CliFailure::new(
            1,
            format!(
                "pause refused: independent runnable task nodes remain: {}",
                runnable.into_iter().take(8).collect::<Vec<_>>().join(",")
            ),
        ));
    }

    let canonical = serde_json::to_vec(&graph)
        .map_err(|error| CliFailure::new(1, format!("serialize task graph: {error}")))?;
    let mut proof = StoredDispositionProofV1 {
        schema_version: 1,
        workstream_id: graph.workstream_id,
        graph_revision: graph.revision,
        handoff_task_id: graph.handoff_task_id,
        graph_digest: hex::encode(Sha256::digest(canonical)),
        blocked_nodes,
        integrity_hash: String::new(),
    };
    proof.integrity_hash = proof_integrity(&proof);
    Ok(proof)
}

fn reject_cycles(nodes: &BTreeMap<String, &TaskNodeV1>) -> Result<(), CliFailure> {
    fn visit(
        id: &str,
        nodes: &BTreeMap<String, &TaskNodeV1>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> bool {
        if visited.contains(id) {
            return false;
        }
        if !visiting.insert(id.to_owned()) {
            return true;
        }
        let cyclic = nodes[id]
            .depends_on
            .iter()
            .any(|dependency| visit(dependency, nodes, visiting, visited));
        visiting.remove(id);
        visited.insert(id.to_owned());
        cyclic
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    if nodes
        .keys()
        .any(|id| visit(id, nodes, &mut visiting, &mut visited))
    {
        return Err(CliFailure::new(1, "disposition task graph must be acyclic"));
    }
    Ok(())
}

fn proof_integrity(proof: &StoredDispositionProofV1) -> String {
    hex::encode(Sha256::digest(
        format!(
            "shipyard-post-handoff-disposition-v1\n{}\n{}\n{}\n{}\n{}",
            proof.workstream_id,
            proof.graph_revision,
            proof.handoff_task_id,
            proof.graph_digest,
            proof.blocked_nodes
        )
        .as_bytes(),
    ))
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 124
        && value.trim() == value
        && !value.chars().any(char::is_whitespace)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(nodes: Vec<TaskNodeV1>) -> TaskGraphV1 {
        TaskGraphV1 {
            schema_version: 1,
            workstream_id: "GEN-14".to_owned(),
            revision: 7,
            handoff_task_id: "pr-148".to_owned(),
            nodes,
        }
    }

    fn node(id: &str, state: TaskStateV1, dependencies: &[&str]) -> TaskNodeV1 {
        TaskNodeV1 {
            id: id.to_owned(),
            state,
            depends_on: dependencies
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    #[test]
    fn one_blocked_child_does_not_hide_an_independent_runnable_node() {
        let error = evaluate_pause_graph(
            graph(vec![
                node("pr-148", TaskStateV1::HandedOff, &[]),
                node("blocked-child", TaskStateV1::Blocked, &["pr-148"]),
                node("independent-docs", TaskStateV1::Pending, &[]),
            ]),
            "GEN-14",
        )
        .expect_err("independent work must force continue");
        assert!(error.message().contains("independent-docs"));
    }

    #[test]
    fn exact_dependency_boundary_produces_a_digest_bound_pause_proof() {
        let proof = evaluate_pause_graph(
            graph(vec![
                node("pr-148", TaskStateV1::HandedOff, &[]),
                node("release", TaskStateV1::Pending, &["pr-148"]),
                node("docs", TaskStateV1::Complete, &[]),
            ]),
            "GEN-14",
        )
        .expect("true dependency boundary");
        assert_eq!(proof.blocked_nodes, 1);
        assert!(proof.valid_for("GEN-14"));
    }

    #[test]
    fn cycles_and_unjustified_blocked_states_fail_closed() {
        let cyclic = graph(vec![
            node("pr-148", TaskStateV1::HandedOff, &[]),
            node("one", TaskStateV1::Pending, &["two"]),
            node("two", TaskStateV1::Pending, &["one"]),
        ]);
        assert!(evaluate_pause_graph(cyclic, "GEN-14").is_err());
        let unjustified = graph(vec![
            node("pr-148", TaskStateV1::HandedOff, &[]),
            node("blocked", TaskStateV1::Blocked, &[]),
        ]);
        assert!(evaluate_pause_graph(unjustified, "GEN-14").is_err());
    }
}
