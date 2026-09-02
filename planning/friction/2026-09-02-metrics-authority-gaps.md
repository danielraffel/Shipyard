# Metrics scorecards omit authoritative queue and lifecycle evidence

**Status:** IN PROGRESS
**Filed:** 2026-09-02 by codex@local
**Owner:** unassigned
**Class:** SYSTEMIC
**Recurrence:** seen in the Phase-1 stewardship scorecard and current v0.155.0 audit

## What happened

The stewardship scorecard reports queue, cache, submit-to-receipt, and model-token
coverage. GitHub job imports currently discard the provider's `created_at` timestamp,
so queue latency is unavailable even when GitHub supplies both creation and start
times. Submit-to-receipt and model-token data are not present in the durable metrics
event contract; cache fields exist in SQLite but are not populated by all importers.

## Evidence

- `src/metrics.rs`: `jobs.queued_at` and `jobs.queue_ms` exist in the schema.
- `GitHubRunJob` previously carried `started_at`/`completed_at` but not `created_at`.
- `github_job_to_record` previously emitted no queue timestamp.
- `StewardshipScorecard` explicitly reports submit-to-receipt and model-token coverage
  as unavailable because durable source fields do not exist.
- M5 verification of the queue-only fix: `cargo test metrics::tests` — 16 passed.

## Root cause

The persistence schema was designed ahead of the importer/input APIs. No single
authoritative event contract binds submission, provider receipt, cache reuse, and
token counters, so inferring those values would create misleading history.

## Why it will recur

Every agent or provider importer that omits lifecycle fields silently produces a
partial scorecard. The gap affects all repositories using GitHub or TartCI metrics,
not a single PR.

## The fix now (unblock)

Commit `f70c4e1d` carries GitHub `created_at` as `queued_at`, derives `queue_ms` only
from valid authoritative queue/start timestamps, persists it, and covers terminal
refresh plus SQLite round-trip tests.

## The fix forever (prevent)

1. Land the queue-timing patch through the normal Shipyard PR path.
2. Define authenticated submit/receipt and cache event fields before importing them;
   never infer them from wall-clock duration or log ordering.
3. Add importer contract tests requiring explicit coverage or an honest unavailable
   result for every scorecard dimension.
4. Add model-token counters only from provider receipts that bind model and token
   counts to the exact work item.

## Routing

- [ ] Fix now, by me
- [x] Hand off — needs owner assignment for the metrics API/PR

## Resolution

**Outcome:** Queue timing implementation is prepared and locally committed; other
metrics remain intentionally unavailable pending an authoritative event contract.
**Landed in:** local commit on `feature/metrics-queue-latency-20260902` (not pushed).
**Did it work?:** M5 disposable current-main worktree compiled successfully; all 16
metrics tests passed.
**Still open:** PR publication/ownership, cache importer coverage, submit-to-receipt,
and model-token telemetry.
