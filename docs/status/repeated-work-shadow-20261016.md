# Repeated-work elimination shadow slice

Status: implementation complete; PR 863 is open and intentionally unarmed on
`feat/repeated-work-shadow-20261016` (2026-10-16).

Base: `origin/main` at `ecec8359223d981751e51a943371471b441a6fe0`.

Dependency: `FLEET-BASELINE` (fleet readiness is an input to later rollout
decisions; this slice does not modify fleet configuration or require a fleet
receipt to record observations).

## Scope and ownership

Owned paths are the durable queue enqueue boundary, the existing SQLite metrics
store, focused unit tests, and this status receipt. The slice records shadow
observations when the current queue supersedence rule encounters a pending job:

- `superseded_exact_head`: same branch, target list, and validation mode, but a
  different exact head SHA;
- `duplicate_exact_head_config`: same branch, target list, validation mode, and
  exact head SHA.

The existing queue mutation remains authoritative. Pending jobs are cancelled
and the new job is enqueued exactly as before; required-check policy and target
dispatch are untouched. The observation is best effort so an unavailable
metrics store cannot reject an otherwise valid enqueue.

## Alternative considered

A separate JSONL shadow log would have avoided coupling queue code to SQLite,
but would create a second accounting surface and duplicate retention/query
logic. The existing metrics store is already the durable timing/evidence store,
so an additive table there is the smallest auditable surface.

## Validation

Focused queue and metrics tests cover both classifications, persistence, and
the no-authority-change behavior. The shadow write takes a zero-timeout
writer-domain probe; a contended metrics store drops the advisory observation
instead of delaying enqueue. The contention probe and local clippy pass, with
clippy reporting only the repository's pre-existing unknown-lint warning. A
live FLEET-BASELINE receipt is not required for this shadow-only accounting
change.

PR: https://github.com/danielraffel/Shipyard/pull/863
