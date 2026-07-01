# Manual CLI Workflows

Most of the time your agent handles CI automatically. These scenarios are
for when you want to run things manually, debug a failure, or manage the
queue.

## You finished a feature and want to merge

You've been working on a feature branch. Everything looks good. Time to
validate across platforms and merge.

```
$ shipyard run
  mac     = pass  (local, 3m12s)
  ubuntu  = pass  (ssh, 5m30s)
  windows = pass  (ssh, 4m18s)
  All green.

$ shipyard ship
  PR #42 created → Validated → Merged to main
```

Or in one step: `shipyard ship` does the validation and merge together.

## CI fails on one platform

You ran validation and Windows failed. You don't want to re-validate
macOS and Linux (they already passed) — just fix and re-run Windows.

```
$ shipyard run
  mac = pass, ubuntu = pass, windows = FAIL

$ shipyard logs sy-001 --target windows
  MSVC error C2065: 'M_PI' undeclared in reverb.cpp:42

# Fix the issue, commit
$ shipyard run --targets windows
  windows = pass

$ shipyard ship
  PR #42 → Merged
```

Shipyard remembers the evidence from the previous run. When you re-run
just Windows and it passes, all three platforms now have green evidence
for this SHA.

## Re-run only the test stage on a remote

If only the test stage failed, skip the configure and build stages on the
remote — Shipyard probes for build artifacts on the remote and skips
ahead when they exist:

```
$ shipyard run --targets windows --resume-from test
  windows: skipping setup, configure, build (markers found on remote)
  windows = pass  (ssh, 1m02s)
```

If the markers aren't found (e.g. a fresh remote, or a different SHA),
Shipyard runs all stages from the beginning and logs a note.

## Multiple agents working in parallel

You have two agents working in separate worktrees — one on reverb,
one on delay. Both need CI, and your machine has one Windows VM.

Shipyard's queue handles this automatically. The first agent's run starts
immediately. The second agent's run queues behind it. When the first
finishes, the second starts.

```
Agent 1 (worktree: ~/Code/my-plugin-reverb):
  shipyard ship → queued → running → PR #42 merged

Agent 2 (worktree: ~/Code/my-plugin-delay):
  shipyard ship → queued → waiting → running → PR #43 merged
```

No collisions. No manual coordination. The queue is machine-global.

## Prefer a network Mac for macOS jobs

If you have a Mac Studio on the network, configure the macOS target so the Mac
Studio is primary and the controller Mac is fallback:

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

Then validate only that lane while bringing the machine online:

```
$ shipyard targets test mac
$ shipyard run --targets mac
```

This fallback form does not add pool scheduling; the Mac Studio simply becomes
the first place Shipyard tries macOS work.

When you want named members and lease visibility, use a host-pool target:

```toml
[host_pools.local_macs]
strategy = "ordered"

[[host_pools.local_macs.members]]
id = "mac-studio"
type = "ssh"
host = "mac-studio"
repo_path = "/Users/shipyard/work/shipyard"
capabilities = ["macos", "arm64"]

[[host_pools.local_macs.members]]
id = "local"
type = "local"
cwd = "/Users/danielraffel/Code/shipyard"
capabilities = ["macos", "arm64"]

[targets.mac]
backend = "host-pool"
pool = "local_macs"
platform = "macos-arm64"
requires = ["macos", "arm64"]
```

Use `shipyard targets pool status` to inspect active and stale leases, and
`shipyard targets pool cleanup --dry-run` before pruning stale lease records.
With a host-pool target, the queue can drain multiple non-conflicting jobs
across available members under one local drain owner. Jobs still serialize when
they need the same checkout, PR ship-state, evidence lane, or exhausted pool
capacity.

## Prioritizing one job over another

Two jobs are queued. The delay feature is urgent. Bump it up.

```
$ shipyard queue
  Running: sy-001 feature/reverb  [normal]
  Pending: sy-002 feature/delay   [low]

$ shipyard bump sy-002 high
  Bumped sy-002 to high
```

When the current job finishes, the high-priority job runs next.

## Merging to develop, not main

Your team uses a develop branch as a staging area. Ship to develop first,
promote to main later when stable.

```
$ shipyard ship --base develop
  PR #44 → Validated → Merged to develop

# Later, when develop is stable:
$ git checkout develop
$ shipyard ship --base main
  PR #45 → Validated → Merged to main
```
