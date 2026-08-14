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

## Local Build Command Contract (project-supplied wrappers)

The `local` backend runs each project-configured stage string
(`setup` / `configure` / `build` / `test` from the project's Shipyard config)
**verbatim** — the whole string is handed to `sh -c "<string>"`, so shell
quoting, `&&`, and multi-word commands work as written. Shipyard does not parse,
rewrite, or split the string beyond that. Three properties of that execution are
a stable contract that projects rely on, and that should not regress:

1. **Working directory is the target's `cwd`.** Local targets run in their
   configured `cwd` (or, when a target leaves `cwd` unset, Shipyard's own current
   directory). Set `cwd` to the repo/worktree root so a stage string may use
   **relative** paths (`tools/ci/build.sh`, `cmake --build build`).
2. **The full parent environment is inherited.** The local executor does not
   clear or whitelist the environment, so `PATH` and any tool-specific variables
   present when Shipyard runs reach the stage command unchanged. A wrapper can
   therefore resolve helper binaries on `PATH` and read its own env inputs.
3. **The child's exit code is propagated.** A nonzero exit from the stage command
   fails that stage (and the target); zero passes.

Because of these three properties, a project can point a stage at a **wrapper
command** with no special Shipyard support. For example, Pulp's `local` macOS
build stage is `tools/ci/governed-build.sh cmake --build build`: a thin wrapper
that acquires a tartci host **build-lease** (sizing parallelism to a shared host
budget so a validation build does not oversubscribe a Mac already running agent
builds), exports `CMAKE_BUILD_PARALLEL_LEVEL`, runs the real `cmake --build`
child, and releases the lease on exit. To Shipyard it is just another build
string. It depends on exactly the guarantees above — repo-root `cwd` (for the
relative script path and the relative `cmake --build build`), inherited `PATH`
(to find `tartci` / `getconf`), and a propagated exit code (so a failed compile
still fails the leg). Shipyard itself is unchanged by this; the note exists so a
future change to how the local backend sets `cwd`, scrubs the environment, or
maps exit codes is understood to break such wrappers.

## Mixed OS VM Lanes

A host can serve more than one VM lane. For Pulp/tartci, the same controller and
secondary Apple Silicon hosts may participate in the macOS pool and also serve
Linux Tart or Windows QEMU jobs when those supervisors are enabled. Capacity is
accounted per lane: macOS jobs consume `macos` VM slots, while Linux and Windows
jobs use their own labels, supervisors, and caps. A running Linux or Windows VM
must not reduce Shipyard's macOS free-slot count, although operators should
still set host route weights or reservations if shared CPU/RAM becomes the real
bottleneck.

When `[host_class.*]` entries are configured, the cooperative queue scheduler
uses the same live Tart capacity probe as `runner capacity` / `runner
fleet-status` before admitting jobs that claim a macOS VM slot. If every local
macOS slot is occupied, the macOS job stays queued; Linux/Windows/cloud jobs do
not consume that `macos` slot and can still run.

`runner fleet-status --target macos` scopes supervisor freshness and problems
to macOS-labeled providers. A wedged Linux provider remains visible to a Linux
fleet query but does not falsely remove healthy macOS capacity. When tartci must
use an App-authenticated GitHub wrapper, set `github_cli = "ghapp"` on each
`[host_class.<name>]`; Shipyard passes it to local and SSH doctor probes as
`TARTCI_GH_CLI`. When omitted, local probes preserve any inherited
`TARTCI_GH_CLI` and remote probes retain tartci's default.

`shipyard runner fleet-status` is the read-only GitHub monitoring surface for
this pool (it writes only a local observation snapshot). In addition to TartCI
capacity and supervisor freshness, it correlates
the configured `governance.required_status_checks` with the front merge-group
commit. It exits nonzero when an aged queue front has no required-check
progress while a routable slot is free, and reports active non-front
merge-group and optional jobs assigned to configured host classes. Exact
merge-group SHAs distinguish the current front from a superseded run for the
same PR. A run whose PR is no
longer present in the queue is labeled `superseded`; the command only reports
it and never cancels it. This makes `runner fleet-status --json` safe for a
periodic queue-tick or launchd monitor. `runner watch` invokes this fleet tick
by default whenever `[host_class.*]` is configured, so an existing durable
watch service does not depend on an interactive agent.

Use `--base` for a non-`main` merge queue and
`--merge-queue-stall-threshold-secs` to tune the default 15-minute front-stall
window. Each tick inspects one page of each active-run status and at most 50
workflow-job pages in total (`--queue-run-limit` can lower that cap), including
queued macOS jobs inside an `in_progress` workflow. A reached bound emits
`OBSERVATION_TRUNCATED` instead of consuming an unbounded API budget;
durable enrollment reconciliation is separately capped at 25 PR lookups per
tick and uses the same explicit truncation signal.
Authentication and rate-limit failures use stable `GITHUB_*` codes.
The optional open release-incident count does not require `Issues: read`;
when unavailable it emits non-fatal `AUXILIARY_OBSERVATION_UNAVAILABLE`.
If `governance.required_status_checks` is empty, every check observed
on the front merge-group commit is treated as a liveness signal. The same
report compares the latest GitHub release tag with the monitored base and
flags when the oldest non-doc/non-changelog commit has remained unreleased past
`--release-stale-threshold-secs` (24 hours by default), with age measured no
earlier than the latest release publication time. Future commit timestamps fail
the release observation closed. A root `VERSION` file
also exposes whether the version
itself is unchanged.

A PR behind a healthy, progressing queue front is a normal serialized wait,
even when its own exact-head required checks are green. It is not a blocked
goal and does not need re-arming. The fleet report alerts only when the timed
front signal is stale or missing, when queue-eligible capacity is idle, or
when non-front/superseded work owns that capacity; each occupier row names the
run, job, runner, and PR so the current bottleneck owner is explicit.

All agents and automation must consume the same command:

```sh
shipyard runner fleet-status --repo OWNER/REPO --json
```

The JSON `reason_codes` are the stable policy API (including
`NORMAL_SERIAL_WAIT`); skills and chat agents must not reimplement the
classifier. Run this command from launchd or the existing queue tick on a fixed
cadence so detection continues independently of any agent session or model
rate limit. The durable snapshot additionally emits
`AUTO_MERGE_ENROLLMENT_CLEARED` when a previously queued PR remains open after
both its queue entry and auto-merge request disappear.

### Offline and busy are separate facts

`shipyard runner status` reports `offline_busy` when GitHub says a runner is
busy but the runner API says it is offline. This is a reconciliation signal,
not a cancellation instruction. For Vellum's repository-scoped disposable
lanes, correlate it with `tartci doctor --reap --json`: require two bounded
observations and record the VM, lease, supervisor, runner, and associated job
IDs. If local ownership is live or uncertain, keep the protected job running.
Current TartCI reports `offline_busy_runner:<name>` with action
`offline_busy_wait_for_github`; it does not infer orphanhood, so that result
must be preserved and escalated rather than canceled or reaped. A future
machine-checked orphan verdict requires a TartCI implementation and pin update
before it can authorize narrow recovery. Shipyard must never bulk-cancel busy
runners, reset shared names, or bypass a merge queue. A fresh assignment and
teardown proof is required before local capacity is considered healthy again.

## Explicit Cloud Fallback

GitHub-hosted macOS fallback must be explicit and should be reserved for a local
fleet outage/unhealthy fleet or a workflow that deliberately wants hosted
coverage. Do not send macOS jobs to hosted runners just because all local VM
slots are temporarily full; leave them queued for the next local slot.

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

## Host-Health Pre-Dispatch Gate (optional)

When a self-hosted runner is *co-located with heavy interactive work* (an agent
session, a large MCP/editor stack), RAM can exhaust → macOS jetsam →
WindowServer crash → **unclean reboot**, which kills the in-flight required-gate
job and fails the leg for an *infra* reason, not the code. Shipyard can read a
shared host-health signal during preflight and surface — or, opt-in, hard-stop
on — a saturated host **before** a ship runs into that failure.

**Off by default. Fails open.** With no `[host_health]` block, or no signal file
present, nothing changes and nothing is read. A missing or unreadable signal is
treated as "no opinion" (the ship proceeds) — a broken probe must never wedge a
ship. This is the deliberate inverse of backend-reachability preflight, which
fails closed: reachability gates correctness, host-health gates only
crash-avoidance.

```toml
[host_health]
gate = true                    # master opt-in; when false the signal is never read
block_on_critical = false      # true = a `critical` reading hard-stops preflight (exit 4)
                               #        (default: surface a warning and proceed)
classify_local_failures = false # true = relabel a LOCAL leg's TEST failure as INFRA when a
                               #        host jetsam/WindowServer crash overlapped its window
# file = "/custom/path/host_vitals.json"   # default: ~/.local/state/pulp/host_vitals.json
```

**The signal** is any JSON file matching the `host_vitals` contract — a numeric
`code` (`0` green / `10` warn / `20` critical) and/or a string `level`
(`green` / `warn` / `critical`), plus an optional human `reason`. `code` wins
when both are present. Shipyard does **not** ship a producer; bring your own.
Pulp publishes one (`tools/scripts/host_vitals.sh` plus a 60 s launchd sensor
that writes `~/.local/state/pulp/host_vitals.json` — the default path here).

**Behavior** when `gate = true`:

| Signal level | `block_on_critical = false` (default) | `block_on_critical = true` |
|---|---|---|
| green / absent / unreadable | proceed silently | proceed silently |
| warn | proceed, print a warning | proceed, print a warning |
| critical | proceed, print a warning | **fail preflight (exit 4)** |

The `SHIPYARD_HOST_VITALS_FILE` env var overrides the path (primarily for tests).

#### Infra-vs-code failure labelling (`classify_local_failures`)

A separate, independent opt-in. When on, a **local** leg that fails with a plain
`TEST` class (a non-zero exit with no infra marker) is relabelled `INFRA` — with
an honest note on the result — if the `host_vitals` signal shows a jetsam or
`WindowServer` crash whose reconstructed time (`file mtime − age_s`) overlaps the
leg's `[started, completed]` window. This distinguishes "your code failed" from
"the host shed load under you", so the author isn't sent to debug a green tree.

Deliberately conservative: only a `TEST` class is eligible (a real `CONTRACT` /
`TIMEOUT` / `TREE_DRIFT` is authoritative and kept), only local legs are
considered (SSH/cloud DiagnosticReports live on another host), and it is a
**pure label** — it never changes `TargetStatus`, so a failed leg still blocks
merge exactly as before (merge readiness keys on pass/fail, not the class). Fails
open: an absent/stale/unreadable signal leaves the original class untouched.

#### Same-backend transient retry (`[ship] transient_local_retries`)

An independent, off-by-default opt-in that re-runs a **local** leg once (or up to
a small bound) on the **same** backend when it fails with a transient `INFRA`
blip — a momentary network/runner hiccup, not an authoritative test failure.

```toml
[ship]
transient_local_retries = 0   # 0 = off (default); clamped to 0..=2
```

Deliberately narrower than the global retryable taxonomy: **only `INFRA`** is
re-run. A local `TIMEOUT` already burned its full wall-clock budget on a host
that is likely still slow, so re-running it in place would just double the wait;
every other class (`CONTRACT`, `TEST`, `TREE_DRIFT`) is authoritative and never
retried. Only local legs qualify — remote backends already have next-backend
failover. Each retry writes to a distinct `<log>.retry<N>` sibling so the failing
attempt's log is never truncated; the outcome is honest about the re-run (a
recovered leg notes it in `phase`, an exhausted one in its error message). With
the default `0`, execution is byte-identical to no retry.

If `classify_local_failures` is also on, a `TEST` failure that gets relabelled
`INFRA` (because a host incident overlapped) becomes retry-eligible — the two
opt-ins compose deliberately.
