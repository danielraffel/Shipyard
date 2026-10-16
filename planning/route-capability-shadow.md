# Route/capability shadow slice

Status: implemented as a pure, receipt-only observation on the current
`origin/main` line. It does not change queue admission, host-pool leasing, or
dispatch.

## Why this slice exists

Shipyard already preserves configured ordered routing and applies `requires`
capability filters before trying a host-pool member or fallback backend. The
missing audit surface was an immutable record that lets us compare the
authoritative first eligible route with later eligible routes. Without that
record, a benchmark could accidentally be mistaken for a routing decision or
could silently reorder candidates.

`src/route_shadow.rs` accepts a scheduler-owned snapshot. Each candidate keeps
two separate labels:

- `execution_environment`: where work would run (`macos-arm64`, Linux, or a
  hosted runner);
- `artifact_capability`: what artifact the environment can produce.

The evaluator walks the candidates exactly as supplied. It selects the first
available candidate satisfying every required capability and records later
capable candidates as `would_admit_if_earlier_unavailable`. Busy, unreachable,
unknown, and capability-mismatched candidates remain typed refusals.

## Benchmark and control

The benchmark/control pair is deterministic and local to the receipt:

1. **Benchmark:** an ordered snapshot with a capable local primary followed by
   a capable hosted overflow route. The receipt must select the primary and
   mark overflow as a counterfactual.
2. **Control:** the same evaluator with only the overflow route, or with the
   primary marked unavailable. The receipt may then select overflow, proving
   that the counterfactual is conditional and that no hidden route mutation
   occurred.

The focused tests also use a Linux execution environment that advertises a
macOS artifact capability. This keeps the route schema honest for future
cross-build hosts without claiming that such a path is currently dispatched.

`RouteShadowReceiptStore` is the smallest durable path for this evidence. It
evaluates a supplied snapshot and writes one crash-safe JSON receipt below the
Shipyard state directory, keyed by a digest of the request identity. Loading
the receipt does not recompute or mutate routing. The receipt's
`control.scope = receipt_only` describes the evaluator's scope; it is not a
claim about actions taken by the caller that supplied the snapshot.

A caller must provide the observation. The shadow evaluator never probes a
host, acquires a lease, changes queue state, or starts a worker. The durable
store therefore records evidence without making the evaluator a dispatch
authority.

## Acceptance boundary

This slice proves only deterministic snapshot evaluation, typed
counterfactual evidence, and crash-safe receipt round-tripping. It does not
prove host reachability, lease availability, artifact production, queue
throughput, or a production reroute. A production caller still needs an
explicit integration point to supply an authoritative observation and decide
when to persist it.
Those require separate exact-head and physical gates before any live dispatch
integration is considered.
