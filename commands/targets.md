---
name: targets
description: Inspect configured targets and live target status
---

Shipyard does not have a dedicated `shipyard targets` subcommand yet.

Use these instead:

- `shipyard status --json` for queue state, live target phase/liveness, and
  fallback information during active jobs.
- `.shipyard/config.toml` and `.shipyard.local/config.toml` for the configured
  targets, backends, hosts, and fallback chains.
- `shipyard doctor --json` to confirm the local toolchain before running jobs.

When reporting target state, distinguish between:

- The configured backend in the target definition.
- The selected backend after preflight or failover, which may differ if a
  fallback backend is used.
