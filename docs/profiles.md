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

### Per-machine switching (`--local`)

`config use` writes the **tracked** `.shipyard/config.toml` — a committed change
that switches the profile for everyone. To switch only for **your machine**, add
`--local`:

```bash
$ shipyard config use cloud --local  # this machine ships via cloud; repo default unchanged
```

`--local` writes the active profile to the gitignored `.shipyard.local/config.toml`
overlay, which merges over the tracked config. Collaborators are unaffected, and
nothing shows up in `git status`. `config profiles --json` reports
`active_source` (`local` | `tracked` | `global`) so tooling (e.g. the menu-bar
app) can show whether you're on a per-machine override or the repo default. In a
git worktree, `--local` writes the current checkout's own overlay rather than a
borrowed main-checkout overlay.

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
