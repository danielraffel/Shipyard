# Profiles & Configuration

Once you're comfortable with Shipyard, profiles let you switch between
different setups with one command.

## The problem they solve

Some days you want local-only validation (fast, free). Other days you need
the full cross-platform proof (Mac + Windows + Linux via cloud). Editing
your config every time is annoying.

## Define profiles once

```toml
# .shipyard/config.toml

[profiles.local]
# Just your Mac. Fast. Free. No network.
targets = ["mac"]

[profiles.normal]
# Mac local + GitHub-hosted Windows and Linux
targets = ["mac", "ubuntu-cloud", "windows-cloud"]

[profiles.full]
# Mac local + VMs with cloud fallback for everything
targets = ["mac", "ubuntu", "windows"]
```

## Switch instantly

```bash
$ shipyard config use local          # just my Mac
$ shipyard config use normal         # Mac + GitHub-hosted cloud
$ shipyard config use full           # Mac + VMs + cloud fallback
```

## See what's active

```bash
$ shipyard config profiles

  local     mac                                          ← active
  normal    mac, ubuntu-cloud, windows-cloud
  full      mac, ubuntu, windows (+fallback)

$ shipyard targets

  Profile: local

  mac              local        macos-arm64      reachable

  (ubuntu and windows are disabled in this profile)
```

## Per-stage target selection

A profile can also give individual stages their own chain. Gates are cheap
and want the shortest path; a full test pass can afford a longer fallback.

```toml
[profiles.local-infra]
targets = ["m3", "m5", "m1", "github"]

[profiles.local-infra.stages.gates]
# Gates are seconds of work. Don't wait on a cold fallback for them.
targets = ["m3"]

[profiles.local-infra.stages.ship]
targets = ["m3", "m1"]
```

A stage without its own `targets` falls back to the profile's list, and a
profile without either resolves every declared target.

### What absence means

Target selection is deliberately hard to trip over:

| Situation | Result |
|---|---|
| No active profile | Every declared target resolves |
| Active profile is not defined | Every declared target resolves |
| Profile declares only `focus_platforms` | Every declared target resolves |
| Profile declares `targets` | Only those, in the order given |
| Profile names a target `[targets]` does not define | **Hard error** |

The order matters and is preserved: the list is a fallback chain, not a set.

That last row is the one deliberate failure. Silently skipping an undeclared
name would drop a lane from validation with nothing anywhere to say so, which
is exactly the kind of quiet gap this whole system exists to prevent.

## Platform-focus profiles

Profiles can also describe which platforms are merge-blocking during a
focused development phase. Shipyard still runs every configured target, but
targets outside the focus set become advisory and are listed in the PR body.

```toml
[project]
profile = "macos-only"

[profiles.macos-only]
description = "Active focus is macOS. Linux and Windows still build for visibility."
focus_platforms = ["macos"]
advisory_platforms = ["linux", "windows"]
```

`Lane-Policy:` commit trailers still win for a single PR:

```text
Lane-Policy: windows=required linux=advisory
```

Automatic issue filing for advisory failures is intentionally not enabled in
this first slice; issue #274 tracks that follow-up so Shipyard does not spam a
repo while the local runner migration is still settling.

## Provider profiles & capabilities

Shipyard's runner providers (GitHub-hosted, Namespace where available) expose *profiles*
— named bundles of capabilities a given runner class advertises. This
is the other side of the [`requires`](./targets.md#locality-routing-requires)
feature: when a target says it needs `gpu`, Shipyard filters the
fallback chain down to profiles that actually offer `gpu`.

### Built-in profiles

These ship with Shipyard — no config needed for the common cases.

| Provider | Profile | Capabilities |
|---|---|---|
| `github-hosted` | `ubuntu-latest` | `linux`, `x86_64` |
| `github-hosted` | `windows-latest` | `windows`, `x86_64` |
| `github-hosted` | `macos-15` | `macos`, `arm64` |
| `github-hosted` | `macos-13` | `macos`, `x86_64` |
| `namespace` | `default` | `x86_64`, `arm64`, `linux`, `macos`, `windows`, `nested_virt` |
| `namespace` | `gpu` | `gpu`, `x86_64`, `linux` |

### Overriding or extending

Any same-named profile you define in `.shipyard/config.toml` overrides
the built-in. Add new profiles for custom fleets. Namespace examples remain
for users with Namespace access; Shipyard's own CI defaults to GitHub-hosted
runners unless a workflow input or repo variable opts into Namespace.

```toml
[providers.namespace.profiles.default]
capabilities = ["x86_64", "arm64", "linux", "macos", "windows", "nested_virt"]

[providers.namespace.profiles.gpu]
capabilities = ["gpu", "x86_64", "linux"]

[providers.namespace.profiles.privileged]
capabilities = ["x86_64", "linux", "privileged", "nested_virt"]
```

### Capability vocabulary

Standard: `gpu`, `arm64`, `x86_64`, `macos`, `linux`, `windows`,
`nested_virt`, `privileged`. Unknown strings are treated as opaque
tags — the matcher is pure set containment, so any agreed-on label
between the target and the profile works (e.g. `tee`, `fpga`,
`pci-passthrough`).

## Global vs project profiles

Profiles work at both levels:

- **Global** (`~/.config/shipyard/config.toml`) — your default setups, shared
  across all projects. Define `local`, `normal`, `full` here once.
- **Project** (`.shipyard/config.toml`) — project-specific profiles that
  override or extend global ones. A project that needs ARM Linux testing
  can add a `release` profile with extra targets.

Switch profiles globally or per-project. `shipyard status` always shows
which profile is active and exactly where each target will run.

## Trusted per-project machine environment

A tracked repository may name non-secret machine inputs that every fresh
worktree needs, while each machine supplies its own absolute values. This keeps
host paths out of git and avoids copying `.shipyard.local` into every worktree.

The repository opts in by name:

```toml
[project]
name = "forge"
repository = "Generous-Corp/forge"

[validation.default]
machine_environment = ["PULP_SDK_DIR", "FORGE_MODULAR_TOOLCHAIN_ROOT"]
configure = "cmake -S . -B build -DCMAKE_PREFIX_PATH=\"$PULP_SDK_DIR\""
```

Each host then supplies the values in its trusted machine-global
`config.toml` (the path printed by `shipyard paths`):

```toml
[repository_environment."Generous-Corp/forge"]
PULP_SDK_DIR = "/path/on/this/host/pulp-sdk"
FORGE_MODULAR_TOOLCHAIN_ROOT = "/path/on/this/host/pulp-source"
```

Shipyard reads this table only from the machine-global layer and requires the
declared repository slug to match the canonical GitHub `origin` byte-for-byte.
A tracked config or checkout-local overlay cannot supply or override the
values. Missing values, non-string values, invalid environment names, and
malformed machine config fail before validation starts. Resolved values are
snapshotted into daemon-owned queue requests under Shipyard's protected machine
state directory so a submitting shell or agent session can exit without
dropping them.

This surface accepts non-secret machine paths only. Requested names must end in
`_DIR`, `_FILE`, `_HOME`, `_PATH`, or `_ROOT`; common key, auth, cookie,
credential, signing, certificate, session, password, secret, and token names
are rejected even when they have a path-like suffix. Use Shipyard's dedicated
credential mechanisms for secrets. Changing a machine mapping affects new
requests; already-queued requests retain their exact submitted snapshot.

## tartci routing profiles

For local VM fleets, Shipyard can integrate with
[tartci](https://github.com/danielraffel/tartci) without making tartci a
requirement for ordinary Shipyard use. tartci owns the local VM facts: which
goldens exist, which hosts can serve them, what labels a per-job runner uses,
and how much capacity each host currently has. Shipyard stays the orchestrator:
it reads those facts, combines them with the active project profile, and
dispatches one concrete GitHub runner selector.

tartci exposes read-only profile and host status commands:

```bash
tartci profile list
tartci profile explain normal-local-fast --repo Generous-Corp/pulp --json
tartci profile plan normal-local-fast --repo Generous-Corp/pulp --json
tartci status --json
```

Shipyard can also read the same profile vocabulary directly from a repo checkout
without requiring tartci:

```bash
shipyard ci profile show normal-local-fast
shipyard ci profile plan normal-local-fast --repo Generous-Corp/pulp --json
```

The Shipyard command searches `.tartci/<name>.toml`,
`.shipyard/ci-profiles/<name>.toml`, then `ci-profiles/<name>.toml`. It is
read-only: it explains the selected target and exact GitHub variable values, but
does not apply variables or dispatch work.

The profile file describes PR, release, coverage, scheduled, and
issue-on-failure policies in commented TOML. Its target IDs are stable routing
vocabulary, not GitHub labels. Each target maps to a concrete `runs-on` selector
such as `["self-hosted","Windows","ARM64","pulp-build-windows"]` or
`"windows-latest"`.

Self-managed x64 targets must set `proven = true` only after a real job has
claimed and completed on that exact selector. Until then, `plan` emits a
warning so a profile cannot silently promote an unverified architecture or
label set. GitHub-hosted and Namespace cloud targets are exempt because their
provider owns the architecture claim.

Shipyard's job is to consume those facts across hosts:

1. Read `tartci status --json` from each configured host.
2. Read `tartci profile explain ... --json` for the repo policy.
3. Resolve ordered fallback before dispatch, using live capacity.
4. Apply or pass one concrete GitHub `runs-on` selector per workflow run.
5. Keep GitHub-hosted x64 scheduled validation authoritative until local x64
   emulation is explicitly proven.

GitHub Actions cannot change `runs-on` once a job is queued. Do not pass an
ordered fallback chain into a Pulp workflow and expect GitHub to handle it.
Fallback must be resolved before repo variables or `workflow_dispatch` inputs are
set.

Routing migrations use per-PR admission holds. Add the configured Shipyard
opt-out label (for Pulp, `shipyard:no-auto-merge`) only to the routing PRs while
their external runner-group or reporter proofs are incomplete. The label stops
future steward admission; it does not dequeue a PR or disable auto-merge that
is already armed. Before adding it to an admitted PR, explicitly dequeue that
PR or disable native auto-merge and confirm the PR is no longer queued. Never use
`shipyard merge-queue hold` for this purpose: a repository-wide hold suppresses
eligible unrelated PRs and turns a local routing issue into queue-wide
serialization. Once the proof is complete, remove the label and let the normal
steward arm protected auto-merge for that PR.

For Pulp, the normal fast profile routes PR macOS and Windows to local ARM64
VMs first, and ordinary Linux PR work to the disposable Mac Pro x64 selector
`["self-hosted","Linux","X64","pulp-build-linux-x64","pulp-host-macpro"]`,
then falls back to GitHub only when live capacity is absent. Scheduled nightly
Intel Linux/Windows validation stays on GitHub-hosted x64 runners. Coverage targets must use dedicated ephemeral
labels; do not route coverage to a warm bare-metal build pool. Pulp's current
repo-specific variables and labels live in Pulp's own docs and
`.shipyard/ci-profiles/` files so Shipyard docs do not stale on Pulp operations.

### Fleet identity and drift guard

The shared profile contract uses stable target IDs plus stable `host/lane/slot`
identity for operations. It does not use static GitHub registration names:
disposable workers register with a unique per-boot name, and the supervisor may
reclaim only an offline registration from the same slot. This preserves audit
continuity while preventing zombie-name collisions after a reboot.

Each repository profile must explicitly cover `pr`, `debug`, `release` build,
`coverage`, and `scheduled` classes. Signing, deployment, privileged, and
secret-bearing jobs remain hosted-only unless separately security-reviewed.
Missing repository policy, missing target, label mismatch, stale lease, or
unhealthy local capacity resolves to the hosted target before dispatch. It must
never create an empty selector or leave GitHub with a local selector that no
runner can satisfy.

For a new Pulp/Forge repository, copy the profile vocabulary, add a repository
stanza, run both `tartci profile validate` and
`shipyard ci profile plan <profile-name> --repo OWNER/REPO`, then
prove one real dispatch and its fallback before enabling merge-queue routing.
The profile and exact selectors are version-controlled in TartCI plus the
consumer repository; secrets, registration tokens, and host state are never
committed. This makes the repositories the durable, reviewable backup for
policy while keeping credentials private to the host/provider.
