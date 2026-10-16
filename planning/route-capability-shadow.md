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

Every receipt carries `dispatch_mutated = false` and `dispatch_performed =
false`. A caller must provide the observation; the shadow pass never probes a
host, acquires a lease, changes queue state, or starts a worker.

## Acceptance boundary

This slice proves only deterministic snapshot evaluation and typed
counterfactual evidence. It does not prove host reachability, lease
availability, artifact production, queue throughput, or a production reroute.
Those require separate exact-head and physical gates before any live dispatch
integration is considered.
