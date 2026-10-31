# Release smoke failed on transient GitHub GraphQL 502s

**Status:** IN PROGRESS
**Filed:** 2026-10-28 by Codex@m3
**Owner:** Shipyard release maintainers
**Class:** SYSTEMIC
**Recurrence:** observed during the v0.372.0 release smoke; related transport failures are covered by the existing release-smoke retry tests

## What happened

The v0.372.0 release build produced valid Linux, Windows, and macOS artifacts, but the hosted `live-smoke` probes failed after their retry budget because GitHub returned HTTP 502 from the GraphQL-backed Shipyard observation path. Authenticated local GitHub reads succeeded at the same time. The release remained fail-closed and was not published.

This consumed a release cycle and left the m3 daemon on a manually installed, checksum-verified generation while the public release was unavailable.

## Evidence

- Shipyard `0.372.0` built successfully from `origin/main`.
- Installed m3 binary: `~/.local/share/shipyard/manual-generations/v0.372.0/shipyard`.
- Installed binary SHA-256: `b1978d6893b309b5db67f6a7f9b99747ac9705d95fae4bc810037e53744c9bbb`.
- m3 daemon reported `shipyard_version: 0.372.0` after refresh.
- Release smoke: all artifact builds passed; both live probes returned GraphQL HTTP 502 after five attempts.
- Local authenticated `ghapp` reads worked, so this was not evidence of a local credential outage.
- Durable external tracker: https://github.com/danielraffel/Shipyard/issues/1014

## Root cause

The release smoke uses a GraphQL-backed observation path whose transport can return a gateway 502 even while the local authenticated GitHub path is healthy. The existing smoke retry policy correctly recognizes 502/503/504 and fails closed after a bounded budget, but it has no alternate observation transport for a persistent GraphQL gateway failure and no durable receipt beyond workflow logs.

## Why it will recur

Any release or hosted smoke running during a GitHub GraphQL gateway incident can exhaust the same retry budget. A later agent could mistake the failure for bad artifacts or credentials unless the transport surface is recorded separately.

## The fix now (unblock)

1. Keep the release fail-closed; do not publish artifacts when both live probes are unobserved.
2. Use the checksum-verified m3 generation only as a controlled one-off while the public release is unavailable.
3. Re-run the release smoke after the GitHub transport recovers; do not rotate credentials based on a GraphQL 502 alone.
4. Use issue #1014 for the bounded fallback implementation and its acceptance proof.

## The fix forever (prevent)

1. Preserve the current bounded retry and regression tests in `scripts/release_live_smoke.py` (`test_transient_failure_is_retried_then_passes` and `test_persistent_transient_failure_is_bounded`).
2. Add a small, read-only fallback for the smoke observation that uses an existing REST-backed query when GraphQL returns only transient 5xx responses; keep GraphQL as the primary path and preserve fail-closed semantics.
3. Emit a machine-readable smoke receipt containing probe, transport, attempts, final status, and whether the result was observed. A receipt must never convert an unobserved result into success.
4. Close this report only after a real release smoke passes with the fallback disabled (normal path) and after a planted GraphQL-502 test proves the fallback is selected and remains read-only.

## Routing

- [x] Hand off — issue #1014 owns the implementation; this report is the durable incident and acceptance contract.

## Resolution

**Outcome:** Existing bounded 5xx retry behavior was verified in source and tests. A durable report and acceptance contract were added; the REST fallback and machine-readable receipt remain open under issue #1014.
**Landed in:** this commit (planning record); implementation target: Shipyard issue #1014.
**Did it work?:** The current release remains correctly fail-closed; no false publish occurred. The unresolved hosted 502 was reproduced in the v0.372.0 smoke.
**Still open:** persistent GraphQL gateway failures still require a later bounded fallback change and a fresh release-smoke proof.
