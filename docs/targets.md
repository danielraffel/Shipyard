# Targets & Fallback Chains

A target is a real machine where your code gets validated. You name them
whatever you want and can have as many as you need.

## Target types

| Target name | Platform | Backend | What it is |
|------------|----------|---------|------------|
| `mac` | macos-arm64 | local | Your Apple Silicon Mac |
| `mac-intel` | macos-x64 | local | Your Intel Mac (if you have one) |
| `ubuntu` | linux-x64 | ssh | Ubuntu VM running on your Mac |
| `ubuntu-arm` | linux-arm64 | ssh | ARM64 Linux server |
| `windows` | windows-x64 | ssh | Windows VM running on your Mac |
| `cloud-linux` | linux-x64 | cloud | A Namespace runner |

You don't need all of these. Use what matches your project — one target
is fine, six is fine. Add more any time with `shipyard targets add`.

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
    { type = "cloud", provider = "namespace" },
]
```

This keeps things predictable. You always know exactly what Shipyard will
do because you configured it.

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
    ✗ nsc — not installed
      → Install with: brew install namespace-cli

  SSH targets:
    ✓ ubuntu — reachable (847ms)
    ✗ windows — unreachable
      → Check: ssh win

  Overall: ready (1 optional item missing)
```

If something is missing, Shipyard tells you exactly what to install and how.
