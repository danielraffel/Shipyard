# Targets & Fallback Chains

A target is a real machine where your code gets validated. You name them
whatever you want and can have as many as you need.

## Target types

| Target name | Platform | Backend | What it is |
|------------|----------|---------|------------|
| `mac` | macos-arm64 | local | Your Apple Silicon Mac |
| `mac-pool` | macos-arm64 | host-pool | Ordered local Mac pool with leases |
| `mac-intel` | macos-x64 | local | Your Intel Mac (if you have one) |
| `ubuntu` | linux-x64 | ssh | Ubuntu VM running on your Mac |
| `ubuntu-arm` | linux-arm64 | ssh | ARM64 Linux server |
| `windows` | windows-x64 | ssh | Windows VM running on your Mac |
| `cloud-linux` | linux-x64 | cloud | A Namespace runner |

You don't need all of these. Use what matches your project — one target
is fine, six is fine. Add more any time with `shipyard targets add`.

## Watch a local VM or SSH build

`shipyard watch` also has a target-backed mode for long local/SSH VM jobs that
are not GitHub Actions runs. Use it when you would otherwise hand-roll
`ssh ... | tee ... | grep ...` loops to follow a build inside a Tart Linux VM.

```sh
shipyard watch local \
  --target linux-vm \
  --command './build-v8.py --target linux-x64 --seal --audit' \
  --milestone-regex '\[[0-9]+/[0-9]+\]' \
  --milestone-regex 'AUDIT (PASS|FAIL)' \
  --terminal-regex 'AUDIT FAIL|ld\.lld: error' \
  --log-path .shipyard.local/logs/v8-linux-x64.log
```

The target must resolve to `backend = "local"` or `backend = "ssh"`. Local
targets run in their configured `cwd`; SSH targets run in `repo_path`. Override
that with `--cwd` when the workload lives somewhere else. Human output streams
the target output, prints `shipyard milestone [...]` for each milestone regex
match, and prints exactly one `shipyard terminal [...]` line when the process
exits or a terminal regex matches. A terminal regex stops the command early and
exits nonzero; otherwise Shipyard exits with the process status.

## Run a target command and keep typed evidence

Use `shipyard run command` when a local or POSIX SSH target should run one
workload-specific command and return a durable evidence bundle, without making
that command count as merge-ready validation evidence.

```sh
shipyard run command \
  --target linux-vm \
  --name v8-linux-x64-seal \
  --expect-code 0 \
  --artifact 'build/linux-x64/lib/libv8.so' \
  --artifact 'logs/v8-audit.log' \
  --env-fingerprint PATH \
  -- bash -lc './build-v8.py --target linux-x64 --seal --audit'
```

Local targets run in their configured `cwd`; SSH targets run in `repo_path`.
Override that with `--target-cwd` for worktrees or mounted VM paths. Artifact
globs are relative to that target working directory and are copied into
`command-evidence/<id>/artifacts/` under Shipyard state. The record stores the
command argv, expected and observed exit code, duration, target/backend/host,
bounded log excerpt, optional environment fingerprints, artifact metadata, and
the bundle path. Query the newest bundle with:

```sh
shipyard evidence command --json
shipyard evidence command --list
```

## Fallback when a machine is down

Each target can have a fallback chain. When the primary is unreachable,
Shipyard tries the next option automatically:

```
1. Try SSH to your VM → unreachable (VM is off)
2. Boot the VM via UTM → wait for SSH to come up → success
3. If that also fails → dispatch to Namespace cloud runners
4. If cloud fails too → dispatch to GitHub-hosted runners (last resort)
```

The chain is configurable per target. You can skip VMs, skip cloud,
or make cloud the primary. An indie developer just having a play with
a project might use: local first, VM fallback, cloud last resort.

## Fallback is opt-in

By default, if a target is unreachable, it just reports unreachable. No
automatic VM booting, no cloud dispatch. You add fallback chains only if
you want them:

```toml
# No fallback — unreachable means unreachable
[targets.ubuntu]
backend = "ssh"
host = "ubuntu"

# With fallback — tries VM, then cloud
[targets.ubuntu]
backend = "ssh"
host = "ubuntu"
fallback = [
    { type = "vm", vm_name = "Ubuntu 24.04" },
    { type = "cloud", provider = "github-hosted" },
]
```

This keeps things predictable. You always know exactly what Shipyard will
do because you configured it.

## Opt-in targets (`default = false`)

A target can be declared but left out of ordinary validation. Set
`default = false` on it, and `shipyard pr`, `shipyard ship` and `shipyard run`
skip it unless something names it:

```toml
[targets.mac]
backend = "local"
platform = "macos-arm64"
default = false   # validated only on request
```

Name it to validate it:

```sh
shipyard pr --target mac          # default set plus mac (repeatable)
shipyard ship --pr 42 --target mac
shipyard run --targets mac        # run takes an explicit list
```

An active profile whose `targets` list names the target also selects it; a
profile selection is explicit, so `default` does not apply while one is in
force. The recipe, timeouts and overrides of an opt-in target are unchanged,
and `default` must be a boolean when present.

Use this when a repository's required GitHub checks already decide whether a
pull request lands and a local lane only duplicates them, but you still want
that lane one flag away.

**When every target is opt-in**, `shipyard pr` and `shipyard ship` still push
the branch, open or find the pull request, and arm native auto-merge (unless
`--no-arm`). They queue no validation job and write no ship-state, because an
evidence-free ship-state reads as in flight to `watch` and as an orphan to the
resume sweep. Instead they report that the pull request's required checks own
the verdict:

```json
{"command": "ship", "pr": 88, "validation": "delegated",
 "verdict_owner": "required-checks", "opt_in_targets": ["mac"]}
```

Wait on the outcome with `shipyard wait pr <n>` or GitHub itself.
`shipyard run` has nothing to delegate to, so with only opt-in targets and no
`--targets` it exits 2 and asks for one.

**An opt-in target is reported, never silently absent.** Every surface that
lists targets or explains a verdict names it with the same line:

```
mac: opt-in, not run (GitHub required checks decide)
```

`shipyard status` prints it in place of `reachable=` (JSON: `status`,
`verdict_owner`, no `reachable`), `shipyard ship-state list` prints it above the
records (JSON: `opt_in_targets`), `shipyard ship-state show <pr>` names it when
a PR has no ship-state, `shipyard landing` has a `LOCAL SHIPYARD TARGETS`
section (JSON: `opt_in_targets`), and `shipyard doctor` shows it as a Runners
row. Unless it is requested, an opt-in target is never probed, never counted
as a Shipyard-derived required context in landability, and never reported as a
deliberate skip or validation gap, including when `--skip-target` names it.

`--skip-target` keeps its meaning: skipping every **default** target is still
an error (exit 2, "No targets remain after --skip-target filtering."). A
`--target` that names no configured target, or a name passed to both
`--target` and `--skip-target`, also exits 2.

## Prefer a Mac Studio with local fallback

For a two-Mac setup, make the network Mac the primary target and this Mac the
fallback. This gives you an explicit Mac Studio first path without adding a
scheduler or hidden self-hosted runner behavior:

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

Fallback is for infrastructure failures. If the Mac Studio is reachable and
the validation command fails, Shipyard reports that failure instead of trying
to make it pass elsewhere. See [`docs/local-mac-pool.md`](./local-mac-pool.md)
for the Phase 1 setup checklist and current queue limits.

## Host-pool Mac targets

For a named local Mac pool, configure `[host_pools]` and point a target at it:

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

`shipyard targets pool status` shows configured members, active leases, stale
leases, available slots, and queue job ownership when a pool member is leased.
`shipyard targets pool cleanup --fix` removes stale lease records from Shipyard
state; it does not delete remote workdirs. The queue can run multiple
non-conflicting jobs under one local drain owner, so separate jobs may use
different available host-pool members concurrently. Jobs still serialize when
they claim the same checkout, PR state, evidence lane, or exhausted pool
capacity.

## Locality routing (`requires`)

Targets can declare capability constraints with `requires = [...]`.
Shipyard then filters the fallback chain down to providers whose
profile advertises every required capability. If nothing in the chain
matches, the target fails with a clear error — better than silently
dispatching a CUDA build to a CPU-only runner.

```toml
[targets.cuda-build]
platform = "linux-x64"
requires = ["gpu", "x86_64"]
fallback = [
    { type = "cloud", provider = "namespace", profile = "gpu" },
    { type = "ssh", host = "gpu-box", capabilities = ["gpu", "x86_64", "linux"] },
]
```

The standard capability vocabulary is `gpu`, `arm64`, `x86_64`,
`macos`, `linux`, `windows`, `nested_virt`, `privileged`. You can add
your own strings — the matcher is pure set containment, so unknown
capabilities work as long as the target and the provider agree.

## Emulated x86_64 smoke (local, via tartci)

On an Apple-Silicon Mac the local VM lanes are **ARM64** — there is no x86 guest
(Apple Virtualization and QEMU-on-hvf are both ARM64). You can still get a *local
x86_64 signal* by cross-compiling in the guest and running the tests under
emulation (qemu-user on Linux, Prism on Windows-ARM). Wire it as a plain
`backend = "local"` target whose validation command shells out to
[tartci](https://github.com/danielraffel/tartci)'s cross lane — no new Shipyard
config field is needed, because a target is just a machine + a command:

```toml
# Emulated x86_64 Linux smoke. Cross-builds x64 and runs the test subset under
# qemu-user-static inside an ephemeral Tart Linux clone, then discards it.
[targets.linux-x64-smoke]
backend  = "local"
platform = "linux-x64"

[targets.linux-x64-smoke.validation]
command = "tartci up linux --target-arch x86_64"
```

```toml
# Prove just the toolchain + emulator chain (golden-agnostic, no checkout):
[targets.x64-selftest]
backend  = "local"
platform = "linux-x64"

[targets.x64-selftest.validation]
command = "tartci up linux --target-arch x86_64 --self-test"
```

This is a **smoke / debug** signal, not a gate: sanitizers, SIMD/Highway
dispatch, and RT timing are unreliable under emulation. Keep a real x86_64 runner
(GitHub-hosted, an SSH x64 box, or a Namespace cloud profile) as the
authoritative x64 gate, and model it as a **separate target** — do **not** chain
the smoke target to the gate with `fallback`. Fallback fires only when a machine
is *unreachable* (an infrastructure failure), not when validation *fails* (see
"Fallback is for infrastructure failures" above), so a failing emulated smoke
must surface as a failure, never silently fall through to cloud:

```toml
# Fast local pre-check (manual / pre-push): emulated x64 via tartci, above.
[targets.linux-x64-smoke]
backend  = "local"
platform = "linux-x64"

[targets.linux-x64-smoke.validation]
command = "tartci up linux --target-arch x86_64"

# The authoritative x64 gate — a real x86_64 runner, an independent target.
[targets.linux]
backend  = "cloud"
platform = "linux-x64"
```

Run the smoke target when you want a fast local signal; the gate target stays
the one your PR must pass. The GPU-on cross build needs a separate x86_64 Skia
tree (both Linux arches collide on one `libskia.a` path); the tartci lane
defaults GPU-off for the smoke and documents the `--skia-dir` opt-in. See
tartci's runbook §3.8.

Capabilities are resolved in this order for each backend:

1. An inline `capabilities = [...]` list on the backend entry.
2. For `type = "cloud"` backends, the provider's profile registry
   (`[providers.<p>.profiles.<name>]`) — see
   [`docs/profiles.md`](./profiles.md).
3. Nothing — the backend is filtered out of the chain.

Omitting `requires` keeps today's behavior exactly — every backend in
the chain is still tried in order.

### Clear error when nothing matches

```
$ shipyard run --targets cuda-build
…
  cuda-build  error
    no provider satisfies requires=['gpu']: tried [namespace.default, github-hosted.ubuntu-latest]
```

Fix by either adding a GPU-capable backend to the target's `fallback`
or adding the needed capability to the profile you're already using.

## What Shipyard checks on setup

`shipyard doctor` checks what you have and tells you what's missing:

```
$ shipyard doctor

  Core:
    ✓ git 2.44.0
    ✓ ssh (OpenSSH 9.7)

  Cloud providers:
    ✓ gh 2.62.0 (authenticated as danielraffel)
    ✓ nsc — not configured (optional)
      Only needed for Namespace runners; install with: brew install namespace-cli

  SSH targets:
    ✓ ubuntu — reachable (847ms)
    ✗ windows — unreachable
      → Check: ssh win

  Overall: ready (1 optional item missing)
```

If something is missing, Shipyard tells you exactly what to install and how.
