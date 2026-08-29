# Terminal delivery-target verification

Shipyard treats cmux and HerdR addresses as locators, not instance identity.
Names, pane IDs, workspace IDs, and surface IDs can move or be reused. A route
is therefore `leader_live_unbound` until fresh adapter evidence binds the
stored exact local boot/PID/start tuple to one live terminal instance.

The verifier is intentionally inert. It returns typed evidence to a future
transactional route-change gate; it does not publish a `TerminalAdapter`,
change an owner generation, wake an agent, or enable dispatch. Existing resume
records continue to preserve native-agent and launch-profile provenance, but
unverified terminal labels do not appear as terminal adapters.

## cmux

Capture the exact cmux socket, surface UUID, exact local process tuple, and any
available lifecycle identifier. In cmux 0.64.22 there is no public bounded
target-surface lifecycle lookup, so lifecycle is correlation metadata only and
never authority. Verification independently re-observes the OS tuple before
calling:

```text
cmux --socket SOCKET rpc agent.resolve_delivery_target {"pid":PID,"pid_resolution":"controlling_tty"}
```

The runner clears `CMUX_SOCKET_PATH` and legacy `CMUX_SOCKET`. Success requires
`source=pid`, `pid_resolution=controlling_tty`, an echoed exact PID, valid
UUIDs, and the exact expected surface identity. Installed cmux 0.64.22 does not
echo PID, so this path intentionally remains activation-blocked until that
response capability lands. A workspace move may update the workspace
locator only when the later route-change transaction commits it. Missing
methods, relay/remote-only evidence, ambient labels, malformed output, and any
surface mismatch fail closed. The OS tuple is re-observed again immediately
before typed evidence returns so process exit or PID reuse during the query
cannot cross the verification boundary.
Non-local socket locators fail closed.
Shipyard must not recover lifecycle from `ps eww` or broad environment capture;
that would both be non-authoritative and risk collecting secrets.

## HerdR

Every supported query carries the explicit selector `herdr --session <name>`.
A command runner applies declared environment overrides exactly: `Some(value)`
sets a variable and `None` removes its ambient value. `api snapshot` must contain exactly one pane
for the stored stable `terminal_id`; a moved pane is allowed because the
terminal identity remains unchanged. `pane process-info` must bind the exact
PID as either the shell or one foreground process, after the independent OS
tuple check. `agent get` must agree with the terminal and must expose the same
required native session value; absent native-session provenance fails closed.
HerdR 0.8.2 exposes socket selection only through `HERDR_SOCKET_PATH` and does
not echo server identity, so socket verification fails closed as unsupported.
Remote or forwarded routes are never accepted as local process authority.

Only a live handoff where the old terminal disappeared may scan all snapshot
panes. The scan is capped at 256 panes, probes each pane once, and succeeds only
for one exact-process match. The newly matched terminal and pane are returned
as candidate evidence for transactional rebind. Zero or multiple matches fail
closed.

The typed success value is opaque outside the verifier: callers cannot construct
adapter-bound evidence from labels or deserialize it from stored input. The
only binding transition consumes this non-cloneable, one-shot verified value.

## Authority boundary

Demotion retains the prior verified instance as a tombstone. It cannot erase
history and later regain authority from a reusable label. Subrouter remains
separate provider provenance: terminal verification neither validates nor
rewrites wrapper, account, model, headers, or native resume identity, and it
never falls back to direct Codex.

HerdR terminal IDs are upstream identity claims rather than cryptographic
incarnations. Shipyard mitigates terminal-ID ABA with exact local process and
native-session checks before and after adapter queries, but activation remains
blocked until the transactional generation CAS and physical canaries prove the
complete route-change gate.
