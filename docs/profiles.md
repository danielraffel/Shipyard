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

For Pulp, the normal fast profile routes PR macOS and Windows to local ARM64
VMs first, and ordinary Linux PR work to the disposable Mac Pro x64 selector
`["self-hosted","Linux","X64","pulp-build-linux-x64","pulp-host-macpro"]`,
then falls back to GitHub only when live capacity is absent. Scheduled nightly
Intel Linux/Windows validation stays on GitHub-hosted x64 runners. Coverage targets must use dedicated ephemeral
labels; do not route coverage to a warm bare-metal build pool. Pulp's current
repo-specific variables and labels live in Pulp's own docs and
`.shipyard/ci-profiles/` files so Shipyard docs do not stale on Pulp operations.
