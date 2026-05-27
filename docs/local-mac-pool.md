# Local Mac Capacity

The simplest setup uses Shipyard's existing SSH plus local fallback behavior to
prefer a network Mac, such as a Mac Studio, and fall back to the controller Mac
when the network Mac is unreachable. A first-class `host-pool` backend is
available when you want named members, leases, `targets pool status`, and
multi-job queue draining across non-conflicting local capacity.

## What Phase 1 Supports

- Prefer Mac Studio by making it the primary `mac` target.
- Transfer the current commit to Mac Studio over SSH with the existing git
  bundle path.
- Fall back to this Mac only for infrastructure reachability failures.
- Reuse warm same-SHA remote workdirs when `warm_keepalive_seconds` is set and
  the current warm-pool rules allow it.
- Keep GitHub-hosted macOS as an explicit fallback only when configured.

The simple fallback form does not support busy/idle scheduling, host leases,
automatic retargeting, GitHub-hosted job cancellation, or multiple local Macs
draining multiple queued jobs at the same time. Use `backend = "host-pool"` for
lease-aware local capacity.

## Example Config

```toml
[targets.mac]
backend = "ssh"
host = "mac-studio"
platform = "macos-arm64"
repo_path = "/Users/shipyard/work/shipyard"
warm_keepalive_seconds = 1800

fallback = [
  { type = "local", cwd = "/Users/danielraffel/Code/shipyard" },
]
```

This order means:

1. Shipyard probes `mac-studio`.
2. If SSH or remote setup is unreachable, it tries the local checkout.
3. If Mac Studio runs the validation and the tests fail, that failure is
   authoritative. Shipyard does not hide real test failures behind fallback.

## Bootstrap Checklist

1. Enable SSH from this Mac to the Mac Studio.
2. Clone the repo on the Mac Studio at the configured `repo_path`.
3. Install matching toolchains, SDKs, signing dependencies, and package
   managers on the Mac Studio.
4. Add or update the explicit `targets.mac` config.
5. Run `shipyard targets test mac`.
6. Run `shipyard run --targets mac`.
7. Inspect warm entries with `shipyard targets warm status --json` if warm
   reuse is enabled.
8. Drain stale warm entries with `shipyard targets warm drain --yes` when
   changing repo paths or machine roles.

## Host-Pool Option

Use `backend = "host-pool"` when you want explicit pool members and lease
visibility instead of a plain fallback chain:

```toml
[host_pools.local_macs]
strategy = "ordered"
lease_stale_seconds = 180
heartbeat_interval_seconds = 15

[[host_pools.local_macs.members]]
id = "mac-studio"
type = "ssh"
host = "mac-studio"
repo_path = "/Users/shipyard/work/shipyard"
max_concurrency = 1
capabilities = ["macos", "arm64"]

[[host_pools.local_macs.members]]
id = "local"
type = "local"
cwd = "/Users/danielraffel/Code/shipyard"
max_concurrency = 1
capabilities = ["macos", "arm64"]

[targets.mac]
backend = "host-pool"
pool = "local_macs"
platform = "macos-arm64"
requires = ["macos", "arm64"]
```

Then inspect and run it:

```bash
shipyard targets pool status
shipyard targets pool cleanup --dry-run
shipyard targets pool cleanup --fix
shipyard run --targets mac
```

The pool filters eligible members by `requires`, accounts for active leases,
acquires a lease before running, and releases the lease when validation
finishes. Stale leases stop blocking after `lease_stale_seconds`; `targets pool
cleanup --fix` prunes those stale lease records from Shipyard state. Remote
workdir deletion and adaptive cloud overflow are still future phases.

## Queue Expectations

Shipyard can run multiple non-conflicting queued jobs under one local drain
owner. Jobs that claim the same local checkout, SSH repo, Windows repo, PR
ship-state, evidence lane, or exhausted host-pool capacity still serialize.
The first concurrency release uses a conservative worker cap, so host-pool
throughput is bounded by both configured pool capacity and the queue worker
limit.

## Explicit Cloud Overflow

GitHub-hosted macOS overflow must be explicit:

```toml
[targets.mac]
backend = "ssh"
host = "mac-studio"
platform = "macos-arm64"
repo_path = "/Users/shipyard/work/shipyard"

fallback = [
  { type = "local", cwd = "/Users/danielraffel/Code/shipyard" },
  { type = "cloud", provider = "github-hosted", workflow = "macos" },
]
```

Do not rely on hidden repository variables or stale self-hosted runner labels
to steal default GitHub-hosted jobs. Local capacity should be visible in
Shipyard config.
