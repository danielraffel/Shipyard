# Baseline telemetry first wave

Status: implementation on `feat/baseline-telemetry-20261016` (2026-10-16).

Base: `origin/main` at `c2acdb5f77f288906e2a10c08038fcb385459552`.

The existing metrics store already provides the scorecard foundation: SQLite
`runs`, `jobs`, and `steps` rows, TartCI `phases_ms` import, GitHub timing
imports, and proxy-based scorecard queries. This slice makes that foundation
correlatable without changing target selection or routing:

- `jobs.correlation_id` is a durable optional key. Callers may provide one;
  provider plus external ID is the deterministic fallback. Existing stores
  backfill that fallback during migration.
- `steps` are the phase rows. A refresh updates only the same named phase;
  observing a new phase appends a row, so queue/boot/setup/run evidence stays
  available under one correlation ID.
- `MetricsStore::phase_rows` exposes bounded phase rows for a correlation ID.

Metric definitions in this wave are deliberately limited to identity and
phase evidence: `correlation_id` joins observations, `external_id` deduplicates
provider jobs, and `steps.step` names a measured phase with duration, status,
and optional timestamps. Scorecard verdicts and routing behavior are unchanged.
