# Repeated-work elimination shadow slice

Status: implementation complete; PR 863 is open and intentionally unarmed on
`feat/repeated-work-shadow-20261016` (2026-10-16).

Base: `origin/main` at `c0be67d5acf4e8d42fa40c5c22b0e52936102ca7`.

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

## Public survivor-runtime readout follow-up (2026-10-30)

Status: implemented on the current-main follow-up branch, rebased onto
`origin/main` at `8c24b0079d0dd26245ef92172d77c0806f208827`.

`metrics repeated-work-cost` now joins distinct canonical queue survivors to
the authenticated `queue_timing` table and reports avoided survivor runtime
with clean, uncertain, cancelled, and unmatched coverage. The readout remains
observational: it does not alter queue authority, skip validation, or make a
merge decision. The implementation preserves the final-attempt-only boundary
and keeps numeric-looking or malformed IDs unjoined.

Validation on the rebased head includes `cargo fmt --all -- --check` and
`cargo test --locked repeated_work` (4 passed, 0 failed).
