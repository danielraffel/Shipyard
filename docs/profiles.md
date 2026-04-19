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
# Mac local + cloud for Windows and Linux
targets = ["mac", "ubuntu-cloud", "windows-cloud"]

[profiles.full]
# Mac local + VMs with cloud fallback for everything
targets = ["mac", "ubuntu", "windows"]
```

## Switch instantly

```bash
$ shipyard config use local          # just my Mac
$ shipyard config use normal         # Mac + Namespace cloud
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

## Provider profiles & capabilities

Shipyard's runner providers (GitHub-hosted, Namespace) expose *profiles*
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
the built-in. Add new profiles for custom fleets:

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
