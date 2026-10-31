# Authenticated handoff adversarial review — follow-up

- **Observed:** 2026-10-28T15:31:00Z
- **Reviewer:** Claude Opus through Subrouter, session `da8cc0bd-02f5-434d-a70c-2763189fbe9c`
- **Review head:** `d3488c119f2877e24882e4a40ae31f6685536c6b`
- **Contract:** https://github.com/danielraffel/pulp-planning/blob/codex/live-drain-refresh-20261028/2026-10-28-agent-delivery-contract.md
- **PR:** https://github.com/danielraffel/Shipyard/pull/998

## Review result

Claude found the implementation safe to begin the importer/transport slice only
after the following preconditions are addressed. This is advisory review input,
not an allowed GitHub review or merge authorization. No approval, arm, enqueue,
or merge occurred.

1. Build `AuthenticatedProducerContext` only inside the supervised transport;
   envelope fields and plain hashes are not authenticity by themselves.
2. Remove the misleading in-memory replay-set parameters from `verify`; durable
   database rows must be the sole replay authority.
3. Define the behavior for authenticated receipts conflicting with legacy rows;
   quarantine or authenticated precedence must preserve the rest of the batch.
4. Specify canonical JSON bytes and add cross-repository golden vectors,
   including fractional timestamps.
5. Normalize project identity at handoff persistence and use exact equality in
   authenticated scorecard joins.
6. Bind provenance links to the current receipt digest, not only `receipt_id`.
7. Clarify expiry replay across reconnects and executable changes.
8. Move the existing-envelope lookup inside the write transaction.
9. Add boundary tests and a negative control proving no steward, landing,
   merge, arm, or risk-tier path reads authenticated telemetry.

## Disposition

`ACTIVE — persistence hardening is the next bounded slice; importer/transport
wiring remains separate and cannot be promoted until the transport-derived
context, conflict policy, canonical encoding, provenance-integrity, and
authority-boundary tests are present. The legacy scorecard remains the default.`
