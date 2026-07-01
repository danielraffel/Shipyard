# Host-Health Pre-Dispatch Gate

Date: 2026-07-01

Status: Part 1 of proposal shipped (this PR). Parts 2–3 tracked as follow-ups.

## Motivation

When a self-hosted runner serving the required gate is *also* carrying an
interactive session + a heavy dev/MCP stack, RAM can exhaust → macOS jetsam →
WindowServer crash → **unclean reboot**. That kills the in-flight required-gate
job so the leg goes red for an **infra reason, not the code** — the author burns
a manual re-run to discover it wasn't them — and can strand a foreground ship.

A downstream incident (Mac Studio, 2026-07-01) hit exactly this: RepoPrompt +
Figma + a heavy MCP stack + the CI runner on one host, ~20 min of escalating
memory-pressure signals, then a reboot that failed the required leg. The spiral
was visible ~20 min out from cheap local metrics.

## The three-part proposal (each independent, all opt-in, off by default)

1. **Pre-dispatch host-health gate** *(this PR)* — before validating, read a
   host-health signal; if the host is `critical`, surface it (or, opt-in,
   hard-stop) instead of running into a saturation failure.
2. **Infra-vs-code failure classification** *(follow-up)* — when a required leg
   fails, correlate its window against host reboot / jetsam / WindowServer-crash
   timestamps; if it overlapped one, auto-classify as infra and retry once.
3. **Restart-safe ship-state** *(follow-up)* — persist ship-state so a
   daemon/host restart resumes or reports an orphaned ship rather than silently
   dropping it. (Interim workaround today: arm GitHub-native auto-merge instead
   of a foreground watch.)

## Part 1 design (shipped)

`src/host_health.rs` — a self-contained reader for the shared `host_vitals`
signal, consulted by ship/run preflight (`src/preflight.rs`).

- **Config `[host_health]`**, all default OFF: `gate` (master opt-in),
  `block_on_critical` (escalate `critical` from a warning to a hard preflight
  failure, exit `4`), `file` (path override; default
  `~/.local/state/pulp/host_vitals.json`, the launchd sensor's location).
- **Signal contract**: JSON with numeric `code` (0/10/20) and/or string `level`
  (green/warn/critical) + optional `reason`. `code` wins. Shipyard ships no
  producer; Pulp's `tools/scripts/host_vitals.sh` + sensor is one.
- **Placement**: after the checkout-root check in
  `collect_ship_preflight_with_options`, before target probes and any durable
  ship-state mutation — a true pre-dispatch gate.

## FAILS OPEN — the deliberate inverse of backend-reachability preflight

Backend reachability gates *correctness* and fails closed. Host-health gates
only *crash-avoidance*, so it **fails open**: absent config, absent signal, or an
unreadable/garbled file all yield "no opinion" (proceed). A broken probe must
never wedge a ship; the worst case is we forgo avoidance we cannot measure.

## Tests

`src/host_health.rs` has 13 unit tests over the pure decision core (gate-off,
absent/unclassifiable fail-open, green/warn/critical, block opt-in, code-wins,
level-string fallback, placeholder reason) + the file reader (sensor contract,
missing, garbage). `src/preflight.rs` adds 3 integration tests driving the gate
through real preflight (block on critical, warn-by-default, gate-off ignores the
signal). `cargo test`/`clippy -D warnings`/`fmt --check` all green.

## Why not the dispatch-level "route to overflow" now

The proposal's "park / route to overflow" variant is a dispatch-layer change
(`ExecutorDispatcher::validate_host_pool` member selection). Part 1 keeps the
first slice at the preflight layer — surface + optional hard-stop — which is
zero-risk by default and already delivers the core value (the author sees a
saturated host before the ship fails). Rerouting is a natural later slice.
