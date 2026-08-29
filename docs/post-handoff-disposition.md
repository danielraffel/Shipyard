# Post-handoff disposition

Shipyard separates durable monitoring transfer from the originating agent's
disposition. `continue` is always the default. `pause` is authorized only when
an explicit durable task graph proves that no independent local task remains
runnable.

The machine-readable result is the tuple:

```json
{
  "monitoring_transferred": true,
  "agent_disposition": "pause",
  "pause_required": true
}
```

An agent may park only when all three values have exactly those values. A
GitHub status or label, a provider result, or the requested disposition alone
is never pause authority. When transfer is false, or disposition is
`continue`, the agent retains the monitor or continues independent work.

## Durable task graph

Pass `--after-handoff pause --task-graph <private-json>`. The bounded,
non-symlink JSON file uses schema version 1:

```json
{
  "schema_version": 1,
  "workstream_id": "GEN-14",
  "revision": 12,
  "handoff_task_id": "land-pr-488",
  "nodes": [
    {"id": "land-pr-488", "state": "handed_off"},
    {
      "id": "release-after-merge",
      "state": "blocked",
      "depends_on": ["land-pr-488"]
    }
  ]
}
```

IDs are canonical non-whitespace tokens. Node IDs are unique, dependency IDs
are lexicographically sorted and unique, all dependencies exist, and the graph
must be acyclic. States are `pending`, `running`, `blocked`, `handed_off`,
`complete`, and `canceled`. A pending node whose dependencies are complete, or
any running node other than the handed-off task, proves independent runnable
work and refuses pause. A blocked node must have an incomplete dependency.
Shipyard stores a digest-bound proof, not the graph contents, in the private
receipt.

## Crash and replay boundaries

1. Shipyard persists private handoff intent before the public status.
2. It writes status before the managed label and rechecks the exact PR head.
   Neither public signal authorizes pausing.
3. It persists the managed receipt and a pending native-publication record.
4. It transactionally applies the zero-wake canonical ledger record. This
   creates a durable daemon monitoring obligation, not a provider process.
5. It marks publication accepted and emits the disposition tuple. A restart
   from pending reuses the stored profile and task-graph proof; accepted replay
   is idempotent.

Provider delivery, provider success/failure, terminal attachment, and session
creation cannot change `agent_disposition`. An ordinary managed handoff creates
zero wake and zero provider delivery.

## Repository workflow

After `monitoring_transferred=true`, stop monitor-only child processes. Then:

| Repository | Continue while Shipyard monitors | Pause boundary |
| --- | --- | --- |
| Pulp | macOS build/test, review, docs, or another runnable PR node | every remaining node depends on the handed-off exact head |
| Forge Modular | module generation, CLI-first acceptance, unrelated macOS lanes | all remaining generation or landing work depends on the handoff |
| Forge Sequencer | independent MIDI/instrument/audio-FX acceptance or release preparation | the remaining acceptance chain is blocked on the handoff |
| Vellum | independent implementation, review, or macOS validation | all remaining nodes depend on the handoff |

Linux and Windows compatibility nodes remain independent unless the durable
graph explicitly declares them as dependencies. They must not silently block a
macOS-focused parent. Update the graph revision whenever task state or
dependencies change; do not reuse a stale pause proof for a different receipt.

