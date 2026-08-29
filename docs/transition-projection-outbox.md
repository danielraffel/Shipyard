# External transition projection outbox

Shipyard records authenticated native status transitions for an optional
external projector without placing Linear, another network service, or a model
in the stewardship hot path. The daemon starts only the default-off bounded
drainer; it does not change work execution authority.

Each record binds a stable workstream handle, strictly increasing sequence,
transition type, exact source/head/receipt evidence, and optional supersession.
The supported transition types are `handoff`, `waiting`, `actionable`,
`new_head`, `merge`, and `configured_closure`. Shipyard derives deterministic
transition and evidence digests. Notes and adapter failures are bounded and
known credential shapes are redacted before persistence.

The local NDJSON outbox is append-only, owner-private, fsynced, and serialized
with a file lock. On restart it discards only a trailing record that never
received its newline commit marker. Complete malformed or contradictory records
fail closed. Concurrent writers cannot lose committed transitions.

An external worker explicitly calls `reconcile_one`. The adapter submits with
the transition ID as its idempotency key, then reads the external object back.
Shipyard acknowledges only an exact transition/evidence match. Temporary
failures append a bounded exponential-backoff attempt; permanent refusals stop
automatic retries. Reconciliation first appends a short durable claim, then
releases the file lock before calling the adapter. An active claim prevents a
producer from superseding an in-flight transition; an abandoned claim becomes
reclaimable after its lease on restart. Adapter implementations must bound one
submit/readback attempt to less than the claim lease. Newer transitions may
supersede older unclaimed state.

The existing GEN-14 Linear integration is the only physical adapter/configuration
surface. This change adds no second Linear schema or project. Linear remains a
status projection, never execution authority. Disabled mode retains schema-v11
SQLite intents for explicit enable-later draining while performing no file
outbox or adapter I/O. No live Linear object is changed by these tests.
