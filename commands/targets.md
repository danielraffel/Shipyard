---
name: targets
description: Inspect configured targets, live target status, and the warm-pool of reusable runners
---

Shipyard exposes a handful of `shipyard targets` subcommands; the older
`shipyard status --json` remains the canonical live-state view.

| Task | Command |
|------|---------|
| List configured targets and reachability | `shipyard targets list --json` |
| Probe one target | `shipyard targets test <name> --json` |
| Add a target | `shipyard targets add <name> --backend ssh --host <host> --platform <plat>` |
| Remove a target | `shipyard targets remove <name>` |
| Show live target phase / liveness | `shipyard status --json` |
| Show warm-pool entries (target, host, TTL, SHA) | `shipyard targets warm status --json` |
| Drain the warm-pool (force cold-start everywhere) | `shipyard targets warm drain --yes` |

## Warm-pool reuse

A target can opt in to warm-pool reuse by setting
`warm_keepalive_seconds = <N>` on its config section. On PASS Shipyard
records `(target, host, workdir, sha, expires_at)`; the next
`shipyard ship` / `shipyard run` within the TTL and on the same SHA
re-enters the same workdir and skips the pre-stage (clone / sync /
deps). Validate always re-runs.

**Three disable levels** (use when reporting / recommending):

1. Per-target: `warm_keepalive_seconds = 0` (default — feature is off).
2. Global kill switch: `SHIPYARD_NO_WARM_POOL=1` in the environment.
3. Per-ship flag: `shipyard ship --no-warm` / `shipyard run --no-warm`.

Any failure during a warm reuse evicts the entry, so the pool never
serves a dirty workdir twice. GitHub-hosted cloud backends are silently
ineligible (workflow runs are ephemeral); Shipyard warns once per
invocation when a target opted in on an ineligible backend.

## Configured vs active backend

When reporting target state, distinguish between:

- The configured backend in the target definition.
- The selected backend after preflight or failover, which may differ if a
  fallback backend is used.
